// Keyboard + mouse idleness signal, feeding the mid-cycle
// `paused_secs` bookkeeping. Does NOT detect gamepad activity —
// `LASTINPUTINFO` only reflects HID keyboard + mouse input, not XInput
// pad state. See `platform::win32::gamepad` for the separate pad poll.
//
// Cheap : two syscalls per call (`GetLastInputInfo` +
// `GetTickCount64`), both O(1) and safe from any thread.

use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

use crate::signals::ms_delta_to_seconds;

/// Seconds since the last keyboard or mouse input on the current
/// session. Wraps `GetLastInputInfo` + `GetTickCount64`.
///
/// Returns 0 if `GetLastInputInfo` fails (which per MSDN only happens
/// under memory exhaustion or an invalid `cbSize`, so treat it as
/// "assume active" rather than propagating the failure up the tick).
pub fn idle_seconds() -> u64 {
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
