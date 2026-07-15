// Low-level keyboard hook that absorbs the shortcuts a child would use to
// escape the popup :
//
//   Alt+F4         — close app
//   Alt+Tab        — switch window
//   Win+D          — show desktop
//   Ctrl+Esc       — open Start menu
//
// Win+L and Ctrl+Alt+Del are protected by the OS and never reach a hook
// callback ; the popup reappears when the user returns to the session
// (documented as a known limit in the design).
//
// WH_KEYBOARD_LL requires a message pump on the installing thread. tao's
// event loop provides one on the main thread, so install the hook there
// and let the guard's Drop tear it down when the event loop exits.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_D, VK_ESCAPE, VK_F4, VK_LWIN, VK_RWIN, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

pub struct KeyboardHookGuard {
    handle: HHOOK,
}

pub fn install_keyboard_hook() -> Result<KeyboardHookGuard> {
    // hmod = None + dwThreadId = 0 : LL hooks are global and don't need
    // to be hosted in a DLL. The kernel injects the callback into the
    // installing thread's message pump.
    let handle = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) }
        .context("SetWindowsHookExW(WH_KEYBOARD_LL)")?;
    Ok(KeyboardHookGuard { handle })
}

impl Drop for KeyboardHookGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.handle);
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Below-zero codes must be forwarded unchanged per MSDN — the OS is
    // asking to pass the event to the next hook without inspection.
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let msg = wparam.0 as u32;
    if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let vk = info.vkCode as u16;
        let alt = (info.flags.0 & LLKHF_ALTDOWN.0) != 0;
        let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
        let win = unsafe {
            GetAsyncKeyState(VK_LWIN.0 as i32) < 0 || GetAsyncKeyState(VK_RWIN.0 as i32) < 0
        };
        let swallow = (alt && vk == VK_F4.0)
            || (alt && vk == VK_TAB.0)
            || (win && vk == VK_D.0)
            || (ctrl && vk == VK_ESCAPE.0);
        if swallow {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
