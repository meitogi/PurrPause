// Keyboard + mouse idleness signal, feeding the mid-cycle
// `paused_secs` bookkeeping. Does NOT detect gamepad activity —
// `LASTINPUTINFO` only reflects HID keyboard + mouse input, not XInput
// pad state. See `platform::win32::gamepad` for the separate pad poll.
//
// # Session 0 pitfall (bugfix b.7)
//
// `GetLastInputInfo` returns input-time for the CALLING thread's
// session. A Windows service runs as SYSTEM in Session 0 which never
// receives user input — a naive call returns `info.dwTime = 0`, so
// `idle_seconds` == system uptime. The service would think the user
// was idle since boot, breaking active-time accounting entirely.
//
// The workaround : query the active console session (`WTSGetActiveConsoleSessionId`),
// grab that user's token (`WTSQueryUserToken`), impersonate briefly
// (`ImpersonateLoggedOnUser`) for the `GetLastInputInfo` call, then
// `RevertToSelf`. Costs one token round-trip per tick (~5 s cadence) —
// negligible next to the WASAPI enum in `busy_detector`.
//
// Failure modes (no active session at login screen, permission glitch)
// return 0 to err toward "assume active" rather than propagating up
// the tick — a Shell / SCM handshake never blocks a scheduled popup.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

use crate::signals::ms_delta_to_seconds;

/// Seconds since the last keyboard or mouse input on the interactive
/// user session. Wraps `GetLastInputInfo` inside an impersonation
/// round-trip so the SYSTEM service reads the console user's input
/// state, not Session 0's (which is always empty).
///
/// Returns 0 on any failure — no active console session, token query
/// denied, impersonate fails — so the tick errs toward "assume active".
pub fn idle_seconds() -> u64 {
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == 0xFFFF_FFFF {
        // No console session (e.g. secure desktop between login and
        // user logon). Assume active.
        return 0;
    }

    let mut token = HANDLE::default();
    let query_ok = unsafe { WTSQueryUserToken(session_id, &mut token) }.is_ok();
    if !query_ok || token.is_invalid() {
        return 0;
    }

    let idle_secs = unsafe { with_impersonation(token, query_lastinput_secs) };

    unsafe {
        let _ = CloseHandle(token);
    }
    idle_secs
}

/// Impersonate `token`, run `f`, then RevertToSelf regardless of
/// panic-free outcome. Caller retains ownership of `token`.
unsafe fn with_impersonation<F: FnOnce() -> u64>(token: HANDLE, f: F) -> u64 {
    if ImpersonateLoggedOnUser(token).is_err() {
        return 0;
    }
    let out = f();
    let _ = RevertToSelf();
    out
}

/// Actual `GetLastInputInfo` + tick-delta math. Runs under the
/// impersonated user context so `info.dwTime` reflects real kbd/mouse
/// activity in the console session.
fn query_lastinput_secs() -> u64 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            let tick = GetTickCount64();
            ms_delta_to_seconds(tick, info.dwTime)
        } else {
            0
        }
    }
}
