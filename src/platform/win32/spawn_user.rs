// Spawn a child process in the active console user's session from a
// service running as LocalSystem in Session 0.
//
// The recipe (well-worn but easy to get wrong) :
//   1. Discover the active console session via WTSGetActiveConsoleSessionId.
//   2. Query the user's impersonation token for that session.
//   3. DuplicateTokenEx to a primary token — CreateProcessAsUserW rejects
//      impersonation tokens. SecurityIdentification + TokenPrimary are
//      the combination that works ; other pairings return
//      ERROR_BAD_TOKEN_TYPE.
//   4. Build a Unicode environment block from the user's profile so the
//      child sees USERPROFILE / TEMP / PATH as the user would.
//   5. STARTUPINFOW.lpDesktop = "winsta0\\default" so the child renders
//      on the interactive desktop (not Session 0's invisible one).
//   6. CreateProcessAsUserW with CREATE_UNICODE_ENVIRONMENT +
//      CREATE_NO_WINDOW + NORMAL_PRIORITY_CLASS. bInheritHandles = FALSE
//      because cross-session handle inheritance is undefined.
//   7. Close the thread handle immediately ; the process HANDLE is
//      kept in the returned SpawnedChild so the caller can
//      TerminateProcess on it later (HANDLE is stable across PID reuse,
//      whereas OpenProcess(pid) at kill time isn't).

use std::ffi::c_void;
use std::ffi::OsStr;
use std::mem::size_of;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_NO_TOKEN, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityIdentification, TokenLinkedToken, TokenPrimary,
    SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS, TOKEN_LINKED_TOKEN,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, TerminateProcess, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    NORMAL_PRIORITY_CLASS, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::platform::argv::build_command_line;

const NO_USER_SESSION: u32 = 0xFFFFFFFF;

/// A spawned user-session process. Owns the process HANDLE so
/// TerminateProcess targets the specific process even after PID reuse.
/// Drop closes the HANDLE ; callers never need to CloseHandle manually.
pub struct SpawnedChild {
    pub pid: u32,
    handle: HANDLE,
}

// HANDLE is a raw Windows opaque pointer ; all our uses cross a thread
// boundary via the service tick loop's owned state, so it needs Send.
unsafe impl Send for SpawnedChild {}
unsafe impl Sync for SpawnedChild {}

impl SpawnedChild {
    /// TerminateProcess the tracked process by its HANDLE (immune to
    /// PID recycle). Idempotent : silently returns Ok if the process
    /// already exited (user clicked Dismiss, Alt+F4 post-countdown,
    /// crashed…). Never propagates the failure ; the caller's contract
    /// is best-effort cleanup.
    pub fn terminate(&self) {
        match unsafe { TerminateProcess(self.handle, 0) } {
            Ok(()) => tracing::info!(pid = self.pid, "spawned child terminated"),
            Err(e) => tracing::debug!(
                pid = self.pid,
                error = ?e,
                "TerminateProcess (best effort — process may already be dead)"
            ),
        }
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl crate::platform::ChildHandle for SpawnedChild {
    fn terminate(&self) {
        SpawnedChild::terminate(self);
    }
    fn pid(&self) -> u32 {
        self.pid
    }
}

/// Production `Spawner` : wraps `spawn_in_active_user_session`. Zero
/// state ; passed as `&RealSpawner` at every call site so the trait dispatch
/// stays statically monomorphized (no dyn overhead) and LTO absorbs it.
pub(crate) struct RealSpawner;

impl crate::platform::Spawner for RealSpawner {
    type Child = SpawnedChild;
    fn spawn(&self, exe: &Path, args: &[&OsStr]) -> Result<SpawnedChild> {
        spawn_in_active_user_session(exe, args)
    }
}

pub fn spawn_in_active_user_session(exe: &Path, args: &[&OsStr]) -> Result<SpawnedChild> {
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == NO_USER_SESSION {
        anyhow::bail!("no active console session");
    }

    let mut user_token = HANDLE::default();
    let query = unsafe { WTSQueryUserToken(session_id, &mut user_token) };
    if let Err(e) = query {
        if e.code().0 == ERROR_NO_TOKEN.0 as i32 {
            anyhow::bail!(
                "no user token for session {session_id} (logon screen / RDP disconnected)"
            );
        }
        return Err(anyhow!("WTSQueryUserToken: {e}"));
    }

    // If the user is a split-token admin (Windows UAC's default), the
    // token WTSQueryUserToken returned is the filtered "medium
    // integrity" one — it cannot spawn an exe whose manifest declares
    // requireAdministrator (fails with ERROR_ELEVATION_REQUIRED
    // 0x800702E4). Swap to the linked elevated token when available.
    let elevation_source = match linked_token(user_token) {
        Ok(Some(linked)) => {
            tracing::info!("swapped to linked elevated token");
            unsafe { let _ = CloseHandle(user_token); }
            linked
        }
        Ok(None) => user_token,
        Err(e) => {
            tracing::warn!(error = ?e, "linked-token query failed, using filtered token");
            user_token
        }
    };

    // From here `elevation_source` must be closed on every path.
    let primary = match duplicate_to_primary(elevation_source) {
        Ok(h) => h,
        Err(e) => {
            unsafe { let _ = CloseHandle(elevation_source); }
            return Err(e);
        }
    };
    unsafe { let _ = CloseHandle(elevation_source); }

    // From here `primary` must be closed on every path.
    let env_block = match create_env_block(primary) {
        Ok(p) => p,
        Err(e) => {
            unsafe { let _ = CloseHandle(primary); }
            return Err(e);
        }
    };

    let mut cmd_line = build_command_line(exe, args);
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();

    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();

    let null_sa: Option<*const SECURITY_ATTRIBUTES> = None;
    let create = unsafe {
        CreateProcessAsUserW(
            Some(primary),
            PCWSTR::null(),
            Some(PWSTR(cmd_line.as_mut_ptr())),
            null_sa,
            null_sa,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | NORMAL_PRIORITY_CLASS,
            Some(env_block),
            PCWSTR::null(),
            &startup,
            &mut process_info,
        )
    };

    // Always release the env block + primary token, even on failure.
    unsafe {
        let _ = DestroyEnvironmentBlock(env_block);
        let _ = CloseHandle(primary);
    }

    create.context("CreateProcessAsUserW")?;

    // Close the thread handle immediately ; keep the process handle
    // alive in SpawnedChild so the caller can TerminateProcess it
    // later without racing against PID reuse.
    unsafe {
        let _ = CloseHandle(process_info.hThread);
    }

    Ok(SpawnedChild {
        pid: process_info.dwProcessId,
        handle: process_info.hProcess,
    })
}

/// Return the linked token of an admin split-token user, or `None`
/// for standard users (non-split-token accounts have no linked token
/// and `GetTokenInformation(TokenLinkedToken)` fails with
/// `ERROR_NO_SUCH_LOGON_SESSION` — treated as "not applicable" rather
/// than an error).
fn linked_token(base: HANDLE) -> Result<Option<HANDLE>> {
    let mut info = TOKEN_LINKED_TOKEN::default();
    let mut ret_len: u32 = 0;
    let result = unsafe {
        GetTokenInformation(
            base,
            TokenLinkedToken,
            Some(&mut info as *mut _ as *mut _),
            size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut ret_len,
        )
    };
    match result {
        Ok(()) if !info.LinkedToken.is_invalid() => Ok(Some(info.LinkedToken)),
        Ok(()) => Ok(None),
        // Standard-user accounts have no linked token — the API
        // reports this as an error rather than a null handle. Swallow
        // it as "no linked token" so the caller falls back cleanly.
        Err(_) => Ok(None),
    }
}

fn duplicate_to_primary(impersonation: HANDLE) -> Result<HANDLE> {
    let mut primary = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            impersonation,
            TOKEN_ALL_ACCESS,
            None,
            SecurityIdentification,
            TokenPrimary,
            &mut primary,
        )
    }
    .context("DuplicateTokenEx")?;
    Ok(primary)
}

fn create_env_block(primary: HANDLE) -> Result<*mut c_void> {
    let mut block: *mut c_void = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut block, Some(primary), false) }
        .context("CreateEnvironmentBlock")?;
    if block.is_null() {
        anyhow::bail!("CreateEnvironmentBlock returned null block");
    }
    Ok(block)
}
