//! `ChangeServiceConfigW` helper for in-place SCM ImagePath updates.
//!
//! Alternative to path_update's legacy delete + recreate approach. Two
//! benefits over the prior flow :
//!
//! - **No `SERVICE_MARKED_FOR_DELETE` race.** Deleting a service marks
//!   the SCM entry pending removal until every handle closes. Recreating
//!   the same name in the meantime returns `ERROR_SERVICE_MARKED_FOR_DELETE`
//!   (1072). The legacy retry loop worked around this with exponential
//!   backoff (up to 6 attempts, ~10 s total). Direct config change
//!   sidesteps it entirely.
//! - **Instant.** No stop→delete→wait→register→start sequence. Just
//!   stop (still required — SCM needs the service quiesced to apply an
//!   ImagePath change cleanly) → mutate → start.
//!
//! `install::retry_register_service` remains — the watchdog `Reinstall`
//! branch still needs it when the SCM entry has been fully deleted (by
//! tampering or a partial uninstall), where there's no service to
//! mutate.

#![cfg(windows)]

use std::path::Path;

use anyhow::{Context, Result};

use crate::platform::argv::build_command_line;

/// Update the SCM ImagePath of `svc` to point at `new_exe`, preserving
/// every other config field via `SERVICE_NO_CHANGE` sentinels.
///
/// The caller must have opened `svc` with `ServiceAccess::CHANGE_CONFIG`
/// and stopped the service (SCM needs the service quiesced to swap the
/// binary path without a partial-launch race).
pub fn change_service_binary_path(
    svc: &windows_service::service::Service,
    new_exe: &Path,
) -> Result<()> {
    use std::ffi::{c_void, OsStr};
    use windows::core::PCWSTR;
    use windows::Win32::System::Services::{
        ChangeServiceConfigW, ENUM_SERVICE_TYPE, SC_HANDLE, SERVICE_ERROR, SERVICE_NO_CHANGE,
        SERVICE_START_TYPE,
    };

    // Reproduce the "<quoted-exe>" --service" ImagePath shape that
    // register_service emits via ServiceInfo. build_command_line
    // (session 3, used by CreateProcessAsUserW) has the same
    // CommandLineToArgvW-compatible quoting semantics — SCM parses
    // ImagePath the same way, so this stays byte-identical for paths
    // without spaces and semantically-equivalent for paths with them.
    let service_arg: &OsStr = OsStr::new("--service");
    let cmdline_utf16 = build_command_line(new_exe, &[service_arg]);

    // windows-service exposes `raw_handle()` returning the windows-sys
    // pointer type (`*mut c_void`). Wrap it in the windows crate's
    // `SC_HANDLE` newtype — both target the same underlying kernel
    // object.
    let raw = svc.raw_handle();
    let handle = SC_HANDLE(raw as *mut c_void);

    // SAFETY : `handle` is a valid open SC_HANDLE (its lifetime is tied
    // to `svc`, which the caller owns for the duration of this call).
    // NUL-terminated UTF-16 comes from build_command_line.
    // SERVICE_NO_CHANGE sentinels preserve every field we don't touch.
    unsafe {
        ChangeServiceConfigW(
            handle,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(SERVICE_NO_CHANGE),
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR(cmdline_utf16.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
        .context("ChangeServiceConfigW")?;
    }
    tracing::info!(
        exe = %new_exe.display(),
        "ChangeServiceConfigW: ImagePath updated in place",
    );
    Ok(())
}
