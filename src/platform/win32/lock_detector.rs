// Workstation-lock detector via the active input desktop.
//
// A locked workstation is proof the human is absent, so it must force
// the interval "active-time" counter to pause — beating every other
// signal (media, gamepad, recent input). Media flags keep reporting
// true while locked (WASAPI/SMTC are session-scoped and the probe
// survives the lock), so without this the interval would keep running
// with nobody there.
//
// # Session 0 pitfall
//
// The input desktop is session/desktop-scoped, exactly like
// `GetLastInputInfo` (see `idle.rs`) and WASAPI (see `busy_detector.rs`).
// A SYSTEM service in Session 0 cannot read the interactive desktop
// reliably, so the check runs in the user-session `--input-probe`
// child, which ships the flag to the service inside `Message::InputStamp`.
//
// # Detection
//
// `OpenInputDesktop` returns a handle to the desktop currently
// receiving input. When unlocked that is `"Default"` ; when locked the
// OS switches the input desktop to the secure desktop (`"Winlogon"`),
// which an unprivileged user-session process cannot open (access
// denied / invalid handle). So: a readable name of `"Default"` ⇒
// unlocked ; anything else, or a failed open/read ⇒ locked. This also
// flags UAC / secure-desktop prompts as "locked", which is acceptable —
// the human is on a system dialog, not using the app.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, UOI_NAME,
};

/// True when the workstation is locked (or on the secure desktop). A
/// failure to open or read the input desktop is treated as locked: an
/// unprivileged process is denied the `"Winlogon"` secure desktop
/// precisely when the screen is locked. (The "assume active on a dead
/// probe" default is a service-side concern — see `idle::screen_locked`
/// — not this raw probe.)
pub fn screen_locked() -> bool {
    let hdesk = match unsafe {
        OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
    } {
        Ok(h) if !h.is_invalid() => h,
        _ => return true,
    };

    let mut buf = [0u16; 256];
    let mut needed: u32 = 0;
    let read_ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .is_ok();
    unsafe {
        let _ = CloseDesktop(hdesk);
    }

    if !read_ok {
        return true;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    !String::from_utf16_lossy(&buf[..end]).eq_ignore_ascii_case("Default")
}
