// Post-create window styling. tao/winit's builder options handle
// most of the fullscreen-transparent-borderless-topmost combo, but the
// following bits still need SetWindowLongPtrW to be reliable — the
// winit path has been observed to leave the title bar visible on
// certain virtualized graphics stacks (Parallels ARM64) and to drop
// the topmost bit after a Fullscreen::Borderless.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

/// Defense-in-depth flag read by `lock_chromeless_proc` on WM_SYSCOMMAND
/// to decide whether to swallow SC_CLOSE (Alt+F4). The LL keyboard hook
/// is the primary defense but has been observed to miss keystrokes on
/// certain virtualized graphics stacks ; the subclass acts as a fallback
/// so Alt+F4 during the countdown lockdown is never a single point of
/// failure. Popup process = one boolean's worth of state, so a static
/// AtomicBool is sufficient. Popup subprocess starts with false ; the
/// Track A CountdownExpired handler flips it to true when the dismiss
/// button appears.
pub static POPUP_UNLOCKED: AtomicBool = AtomicBool::new(false);

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE,
    GWL_STYLE, HWND_TOPMOST, LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, WS_BORDER, WS_CAPTION, WS_DLGFRAME, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME,
    WS_EX_LAYERED, WS_EX_STATICEDGE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_WINDOWEDGE,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

/// Swap the window class's background brush to a solid black brush so
/// the very first `WM_ERASEBKGND` paints the client area black instead
/// of the Windows default (white). Kills the white flash between window
/// creation and wgpu's first frame — visible on the countdown widget
/// which takes 100-200 ms to initialise wgpu before rendering.
///
/// Uses `SetClassLongPtrW(GCLP_HBRBACKGROUND)` so the change lives on
/// the window class, not the HWND itself — cheap and one-shot.
pub fn paint_class_black(hwnd: HWND) -> Result<()> {
    use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
    use windows::Win32::UI::WindowsAndMessaging::{SetClassLongPtrW, GCLP_HBRBACKGROUND};
    unsafe {
        let brush = GetStockObject(BLACK_BRUSH);
        if brush.is_invalid() {
            anyhow::bail!("GetStockObject(BLACK_BRUSH) returned null");
        }
        let brush_hbrush = HBRUSH(brush.0);
        SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush_hbrush.0 as isize);
    }
    Ok(())
}

pub fn apply_topmost_toolwindow(hwnd: HWND) -> Result<()> {
    unsafe {
        let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new = cur | (WS_EX_TOOLWINDOW.0 as isize) | (WS_EX_TOPMOST.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .context("SetWindowPos HWND_TOPMOST")?;
    }
    Ok(())
}

/// Explicitly remove the window from the Windows taskbar via
/// `ITaskbarList::DeleteTab`. `WS_EX_TOOLWINDOW` alone doesn't
/// reliably hide the popup on Win11 ARM64 (taskbar still shows a
/// preview thumbnail with a close button on hover), so drive the
/// shell directly.
///
/// COM must be initialised on the calling thread — safe to call
/// multiple times, `CoInitializeEx` returns `S_FALSE` on subsequent
/// calls which we swallow.
pub fn remove_from_taskbar(hwnd: HWND) -> Result<()> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};

    unsafe {
        // S_FALSE = already initialised on this thread ; both are fine.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let taskbar: ITaskbarList =
            CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(TaskbarList)")?;
        taskbar.HrInit().context("ITaskbarList::HrInit")?;
        taskbar
            .DeleteTab(hwnd)
            .context("ITaskbarList::DeleteTab")?;
        let _ = taskbar; // explicit drop keeps intent clear
    }
    Ok(())
}

/// Subclass the WndProc to :
///  1. Block WS_CAPTION / WS_SYSMENU / WS_MINIMIZEBOX / WS_MAXIMIZEBOX
///     from being re-added by Windows on state transitions
///     (restore-from-minimize, activation change etc.). Fires from
///     `WM_STYLECHANGING` which is sent BEFORE the change lands.
///  2. Block `SC_MINIMIZE` via `WM_SYSCOMMAND` so Win+D (which sends
///     that command to every top-level window) can't minimize the
///     popup — chrome never gets a chance to be re-added since
///     minimize never happens.
///  3. Log every relevant message so `widget.log` shows exactly which
///     Win32 events fire during the popup's life. Set `RUST_LOG=info`
///     for the noisy traces.
///
/// Idempotent : re-subclassing the same hwnd with the same ID is a
/// no-op (RemoveWindowSubclass + SetWindowSubclass).
pub fn subclass_lock_chromeless(hwnd: HWND) -> Result<()> {
    use windows::Win32::UI::Shell::{RemoveWindowSubclass, SetWindowSubclass};
    unsafe {
        const SUBCLASS_ID: usize = 0xC0DE_1234;
        let _ = RemoveWindowSubclass(hwnd, Some(lock_chromeless_proc), SUBCLASS_ID);
        SetWindowSubclass(hwnd, Some(lock_chromeless_proc), SUBCLASS_ID, 0)
            .ok()
            .context("SetWindowSubclass(lock_chromeless)")?;
        tracing::info!(
            hwnd = format!("0x{:x}", hwnd.0 as usize),
            "subclass_lock_chromeless: installed"
        );
    }
    Ok(())
}

/// Style-bit mask that never belongs on the popup (caption + sysmenu
/// + min/max box). Kept as a module constant so both the initial
/// strip and the subclass block-on-add use exactly the same bits.
const CHROME_MASK: u32 =
    WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0;

unsafe extern "system" fn lock_chromeless_proc(
    hwnd: HWND,
    umsg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uidsubclass: usize,
    _dwrefdata: usize,
) -> LRESULT {
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{
        SC_CLOSE, SC_MINIMIZE, STYLESTRUCT, WM_ACTIVATE, WM_ERASEBKGND, WM_NCACTIVATE, WM_NCPAINT,
        WM_STYLECHANGED, WM_STYLECHANGING, WM_SYSCOMMAND, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING,
    };
    match umsg {
        WM_STYLECHANGING => {
            let which = wparam.0 as i32;
            if which == GWL_STYLE.0 && !(lparam.0 as *const u8).is_null() {
                let style: *mut STYLESTRUCT = lparam.0 as _;
                let old = unsafe { (*style).styleOld };
                let new = unsafe { (*style).styleNew };
                if (new & CHROME_MASK) != 0 {
                    let cleaned = new & !CHROME_MASK;
                    unsafe { (*style).styleNew = cleaned };
                    tracing::info!(
                        old = format!("0x{:x}", old),
                        proposed_new = format!("0x{:x}", new),
                        forced_new = format!("0x{:x}", cleaned),
                        removed = format!("0x{:x}", new & CHROME_MASK),
                        "subclass: blocked chrome bits on WM_STYLECHANGING"
                    );
                } else {
                    tracing::info!(
                        old = format!("0x{:x}", old),
                        new = format!("0x{:x}", new),
                        "subclass: WM_STYLECHANGING (no chrome bits)"
                    );
                }
            }
        }
        WM_STYLECHANGED => {
            let which = wparam.0 as i32;
            if which == GWL_STYLE.0 && !(lparam.0 as *const u8).is_null() {
                let style: *const STYLESTRUCT = lparam.0 as _;
                let new = unsafe { (*style).styleNew };
                if (new & CHROME_MASK) != 0 {
                    tracing::warn!(
                        new = format!("0x{:x}", new),
                        chrome_remnant = format!("0x{:x}", new & CHROME_MASK),
                        "subclass: WM_STYLECHANGED landed WITH chrome bits (block failed?)"
                    );
                } else {
                    tracing::info!(
                        new = format!("0x{:x}", new),
                        "subclass: WM_STYLECHANGED clean (no chrome)"
                    );
                }
            }
        }
        WM_SYSCOMMAND => {
            let cmd = wparam.0 & 0xFFF0;
            if cmd == SC_MINIMIZE as usize {
                tracing::info!("subclass: blocked SC_MINIMIZE (Win+D or similar)");
                return LRESULT(0);
            }
            // SC_CLOSE = 0xF060 fires on Alt+F4 (and on other close
            // paths). Block during lockdown ; allow post-countdown
            // once the popup process flipped POPUP_UNLOCKED to true.
            // Defense-in-depth : the LL keyboard hook is the primary
            // Alt+F4 barrier but has been observed to miss keystrokes
            // on some virtualized graphics stacks, so we double up.
            if cmd == SC_CLOSE as usize && !POPUP_UNLOCKED.load(Ordering::Relaxed) {
                tracing::info!("subclass: blocked SC_CLOSE (lockdown active)");
                return LRESULT(0);
            }
            tracing::info!(cmd = format!("0x{:x}", cmd), "subclass: WM_SYSCOMMAND (allowed)");
        }
        WM_NCPAINT => {
            // Block non-client area painting entirely. If a phantom
            // title bar is being drawn via NC paint (rather than via
            // WS_CAPTION style), this returns "already painted" and
            // Windows skips the drawing.
            tracing::info!("subclass: intercepted WM_NCPAINT (returning 0 = suppress NC draw)");
            return LRESULT(0);
        }
        WM_NCACTIVATE => {
            // wParam TRUE = active, FALSE = inactive. Returning TRUE
            // + not calling DefWindowProc prevents the NC area from
            // redrawing on activation change.
            let active = wparam.0 != 0;
            tracing::info!(active, "subclass: WM_NCACTIVATE (suppressing NC redraw)");
            return LRESULT(1);
        }
        WM_ACTIVATE => {
            let low = (wparam.0 & 0xFFFF) as u32;
            tracing::info!(state = low, "subclass: WM_ACTIVATE");
        }
        WM_ERASEBKGND => {
            // Return 1 = "background already erased, don't paint".
            // Prevents Windows from filling any unpainted margin with
            // the class HBRBACKGROUND (default silver-grey) which shows
            // as a grey band at the top of the popup if the WebView2
            // child doesn't quite cover the whole client area.
            tracing::info!("subclass: intercepted WM_ERASEBKGND (returning 1 = don't erase)");
            return LRESULT(1);
        }
        WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED => {
            let msg_name = if umsg == WM_WINDOWPOSCHANGING { "CHANGING" } else { "CHANGED" };
            tracing::info!(msg = msg_name, "subclass: WM_WINDOWPOS");
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) }
}

/// Strip only the title-bar bits (WS_CAPTION + WS_SYSMENU + WS_MINIMIZEBOX
/// + WS_MAXIMIZEBOX) — enough to hide the grey Windows chrome without
/// touching WS_THICKFRAME / WS_BORDER / WS_DLGFRAME or any WS_EX_* edge
/// (which session 5.2's `strip_window_frame` observed to clobber
/// WebView2's DirectComposition alpha). Safe to call on windows that
/// already had `with_decorations(false)` applied — no-op in that case.
///
/// SWP_FRAMECHANGED forces DWM to recompute the non-client rect right
/// away so the title bar disappears without a user resize.
pub fn strip_titlebar_minimal(hwnd: HWND) -> Result<()> {
    unsafe {
        let cur = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let mask = (WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0) as isize;
        let new = cur & !mask;
        if new != cur {
            SetWindowLongPtrW(hwnd, GWL_STYLE, new);
            tracing::info!(
                before = format!("0x{:x}", cur),
                after = format!("0x{:x}", new),
                "strip_titlebar_minimal: WS_CAPTION+WS_SYSMENU removed"
            );
        }
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .context("SetWindowPos SWP_FRAMECHANGED (titlebar strip)")?;
    }
    Ok(())
}

/// Disable DWM's non-client rendering (title bar chrome, resize
/// handles, drop shadow) AND opt out of Windows 11's auto-rounded
/// corners. Both APIs are needed on Win11 to get a truly bordless
/// transparent window : DWMWA_NCRENDERING_POLICY kills the shadow,
/// DWMWA_WINDOW_CORNER_PREFERENCE prevents the subtle white liseré
/// that Win11 draws around every top-level window by default.
pub fn disable_dwm_nc_rendering(hwnd: HWND) -> Result<()> {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    };
    unsafe {
        let policy = DWMNCRP_DISABLED;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &policy as *const _ as *const _,
            std::mem::size_of_val(&policy) as u32,
        )
        .context("DwmSetWindowAttribute(NCRENDERING_POLICY, DISABLED)")?;

        // Win11+ : opt out of the auto-rounded corners that come with
        // a subtle white outline. Safe no-op on Win10 (returns
        // E_INVALIDARG which we swallow — the API is 20H1+).
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const _,
            std::mem::size_of_val(&corner) as u32,
        );
    }
    Ok(())
}

/// Enable DWM-composited whole-window alpha transparency. Works
/// around eframe/glow/glutin failing to expose an alpha pixel format
/// on Parallels ARM64's virtualized ANGLE — the window is created
/// opaque, then DWM blends the whole framebuffer at `alpha/255`
/// opacity against the desktop. Uniform alpha only (no per-pixel
/// alpha), so rounded-corner transparency around a rectangle
/// requires the heavier UpdateLayeredWindow + bitmap approach.
///
/// `alpha = 255` → fully opaque (equivalent to no LWA_ALPHA at all).
/// `alpha = 217` → ~85% opaque, matches the design's rgba(_, _, _, 0.85).
/// `alpha = 128` → 50% opaque, quite see-through.
pub fn apply_layered_alpha(hwnd: HWND, alpha: u8) -> Result<()> {
    unsafe {
        let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new = cur | (WS_EX_LAYERED.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
        SetLayeredWindowAttributes(
            hwnd,
            windows::Win32::Foundation::COLORREF(0),
            alpha,
            LWA_ALPHA,
        )
        .context("SetLayeredWindowAttributes")?;
    }
    Ok(())
}

/// Strip every window frame style (title bar, thick frame, min/max
/// boxes, sysmenu). Belt-and-braces on top of winit's with_decorations
/// (false) — that builder option is ignored on some Parallels ARM64
/// builds. `SWP_FRAMECHANGED` forces the DWM to recompute the
/// non-client area immediately so the change is visible without a
/// user resize.
pub fn strip_window_frame(hwnd: HWND) -> Result<()> {
    unsafe {
        // Strip GWL_STYLE bits : title bar, thick frame, min/max/sysmenu,
        // plus the 1-pixel WS_BORDER and WS_DLGFRAME that survive most
        // borderless requests and show up as a thin outline in the
        // client area after DWM composition.
        let style_before = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let style_mask = (WS_CAPTION.0
            | WS_THICKFRAME.0
            | WS_MINIMIZEBOX.0
            | WS_MAXIMIZEBOX.0
            | WS_SYSMENU.0
            | WS_BORDER.0
            | WS_DLGFRAME.0) as isize;
        let style_new = style_before & !style_mask;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style_new);

        // Also strip GWL_EXSTYLE edges — WS_EX_WINDOWEDGE /
        // CLIENTEDGE / STATICEDGE / DLGMODALFRAME each render a
        // subtle 1-2px 3D border that survives WS_CAPTION removal.
        let ex_before = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let ex_mask = (WS_EX_WINDOWEDGE.0
            | WS_EX_CLIENTEDGE.0
            | WS_EX_STATICEDGE.0
            | WS_EX_DLGMODALFRAME.0) as isize;
        let ex_new = ex_before & !ex_mask;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_new);

        let style_after = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let ex_after = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        tracing::info!(
            style_before = format!("0x{:x}", style_before),
            style_after = format!("0x{:x}", style_after),
            ex_before = format!("0x{:x}", ex_before),
            ex_after = format!("0x{:x}", ex_after),
            "window frame strip"
        );

        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .context("SetWindowPos SWP_FRAMECHANGED")?;
    }
    Ok(())
}
