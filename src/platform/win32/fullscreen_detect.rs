// Detect a foreground window that covers the entire monitor (borderless
// or exclusive fullscreen) and force-minimize it before rendering a
// palier widget or the popup. Chosen over DLL injection / kernel driver
// per the "friction, not fort-knox" principle — this stays entirely in
// documented user-mode APIs.
//
// Detection heuristic :
//   1. GetForegroundWindow → foreground HWND.
//   2. Skip if owned by our own process (we don't want to minimize
//      ourselves — the popup or a countdown widget already spawned).
//   3. Skip if the window has WS_CAPTION (visible title bar → not
//      fullscreen).
//   4. GetWindowRect vs. MonitorFromWindow's rcMonitor — if the window
//      covers ≥ 95 % of the monitor rect, treat as fullscreen.
//
// Applies to BOTH exclusive fullscreen games (which normally hide our
// topmost overlay) and borderless-windowed games (which don't hide it
// but still benefit from being interrupted for the pause reminder).

use anyhow::Result;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, ShowWindow,
    GWL_STYLE, SW_FORCEMINIMIZE, WS_CAPTION,
};

/// Coverage threshold to classify a window as fullscreen. Slack for
/// window managers that inset borders by a few pixels.
const FULLSCREEN_COVERAGE_MIN: f32 = 0.95;

/// Return the HWND of a foreground fullscreen window that is NOT owned
/// by our process, or `None` if the desktop is otherwise.
pub fn foreground_fullscreen() -> Option<HWND> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }

        // Skip our own windows.
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return None;
        }

        // Skip windows with a visible title bar.
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        if (style & WS_CAPTION.0) != 0 {
            return None;
        }

        // Compare window rect to monitor rect.
        let mut wnd_rect = RECT::default();
        if GetWindowRect(hwnd, &mut wnd_rect).is_err() {
            return None;
        }
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut info).as_bool() {
            return None;
        }
        let mon_rect = info.rcMonitor;

        let coverage = rect_coverage(
            (wnd_rect.left, wnd_rect.top, wnd_rect.right, wnd_rect.bottom),
            (mon_rect.left, mon_rect.top, mon_rect.right, mon_rect.bottom),
        );
        if coverage >= FULLSCREEN_COVERAGE_MIN {
            Some(hwnd)
        } else {
            None
        }
    }
}

/// Detect + minimize. Returns `Ok(true)` if a fullscreen window was
/// minimized, `Ok(false)` if nothing was in the way. Non-fatal on
/// individual Win32 failures — logged and swallowed.
pub fn force_minimize_foreground_fullscreen() -> Result<bool> {
    match foreground_fullscreen() {
        Some(hwnd) => {
            unsafe {
                // ShowWindow returns non-zero if the window was previously
                // visible — we don't care about the result, only that the
                // syscall didn't fault. SW_FORCEMINIMIZE works even for
                // hung windows (unlike SW_MINIMIZE which waits on the
                // target's message pump).
                let _ = ShowWindow(hwnd, SW_FORCEMINIMIZE);
            }
            tracing::info!(hwnd = ?hwnd.0, "minimized foreground fullscreen window");
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Fraction of the monitor rect covered by the window rect. Pure
/// integer geometry — kept out of the Win32-typed layer so its logic
/// can be lifted to a cross-platform module for Linux-side testing.
fn rect_coverage(wnd: (i32, i32, i32, i32), mon: (i32, i32, i32, i32)) -> f32 {
    let (wl, wt, wr, wb) = wnd;
    let (ml, mt, mr, mb) = mon;
    let mon_w = (mr - ml).max(1) as f32;
    let mon_h = (mb - mt).max(1) as f32;
    let mon_area = mon_w * mon_h;

    let ix1 = wl.max(ml);
    let iy1 = wt.max(mt);
    let ix2 = wr.min(mr);
    let iy2 = wb.min(mb);
    if ix2 <= ix1 || iy2 <= iy1 {
        return 0.0;
    }
    let inter = (ix2 - ix1) as f32 * (iy2 - iy1) as f32;
    inter / mon_area
}
