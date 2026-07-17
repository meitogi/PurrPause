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
//
// On top of the raw shortcut suppression, the hook also captures the
// parent-side Ctrl+Shift+pincode emergency unlock (see
// `install_keyboard_hook_with_pincode`) : while Ctrl is held, digit
// key-downs feed a `PincodeBuffer` and are swallowed so they never
// reach the frontmost window. Non-digit non-modifier keys clear the
// buffer ; Shift / Alt / CapsLock keydowns pass through (needed on
// AZERTY where Shift is required to reach the top-row digits). On
// Ctrl-release the buffer's verdict (Submit / Cancel) is dispatched
// to the popup via the caller-supplied closure. The PincodeBuffer
// semantics are covered by unit tests in `crate::signals`.
//
// The capture prefix is Ctrl, not Alt. AltGr = Right-Alt participates
// in layout resolution on many EU layouts (on FR AZERTY, AltGr+digit
// produces ~ # { [ | ` \ ^ @), and some Mac→VM keyboard drivers
// classify the Option key as AltGr — in which case ToUnicodeEx returns
// the AltGr variant instead of the digit and the capture buffer never
// fills. Ctrl is a pure command modifier that no Windows layout binds
// to a printable char, so it doesn't collide with layout-shift output.

use std::sync::Mutex;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_0, VK_9, VK_CAPITAL, VK_CONTROL, VK_D, VK_ESCAPE, VK_F4, VK_LCONTROL,
    VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NUMPAD0, VK_NUMPAD9, VK_RCONTROL, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::signals::{PincodeBuffer, Verdict};

/// Event dispatched by the hook to the caller's closure. Deliberately
/// carries no `tao` / `wry` types so this module stays independent of
/// the popup's event loop implementation — the popup adapts these into
/// its own `UserEvent` variants.
pub enum PincodeEvent {
    /// A digit was accepted into the buffer at the given 0-based index.
    Digit(u32),
    /// Alt was released with a full-length buffer ; the collected
    /// digits are ready for argon2id verification.
    Submit(String),
    /// Buffer was cleared (Alt-release too early, non-digit while Alt
    /// held, etc.). The popup should hide the asterisk display.
    Cancel,
}

pub struct KeyboardHookGuard {
    handle: HHOOK,
}

/// Shared state consumed by `hook_proc`. Guarded by a Mutex because
/// `hook_proc` is an `extern "system" fn` that the OS may re-enter from
/// its own message pump ; the Mutex lock cost is negligible next to a
/// SetWindowsHookEx round-trip.
struct HookState {
    send: Box<dyn Fn(PincodeEvent) + Send + Sync>,
    buffer: PincodeBuffer,
    ctrl_held: bool,
}

static HOOK_STATE: Mutex<Option<HookState>> = Mutex::new(None);

/// Install the LL keyboard hook WITHOUT pincode capture. Used in
/// `--debug` popup spawns (developer needs Alt+F4 to exit) and when
/// the config has no passcode set yet (`passcode_hash` empty at fresh
/// install). Only the Alt+F4 / Alt+Tab / Win+D / Ctrl+Esc suppression
/// is active.
pub fn install_keyboard_hook() -> Result<KeyboardHookGuard> {
    // Leaves HOOK_STATE at its `None` default so hook_proc's pincode
    // branch stays inert.
    install_raw()
}

/// Install the LL keyboard hook WITH Alt+pincode capture enabled.
///
/// - `send` is called from the hook thread on every accepted digit /
///   submit / cancel. Must be `Send + Sync + 'static` so it can live
///   in the static `HOOK_STATE`.
/// - `cap` is the exact number of digits required to trigger a Submit
///   verdict (short buffers on Alt-release → Cancel). `cap == 0`
///   collapses to Cancel-only, which the caller should treat as
///   "capture disabled".
pub fn install_keyboard_hook_with_pincode(
    send: impl Fn(PincodeEvent) + Send + Sync + 'static,
    cap: usize,
) -> Result<KeyboardHookGuard> {
    *HOOK_STATE.lock().unwrap() = Some(HookState {
        send: Box::new(send),
        buffer: PincodeBuffer::new(cap),
        ctrl_held: false,
    });
    install_raw()
}

fn install_raw() -> Result<KeyboardHookGuard> {
    // hmod = None + dwThreadId = 0 : LL hooks are global and don't need
    // to be hosted in a DLL. The kernel injects the callback into the
    // installing thread's message pump.
    let handle = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) }
        .context("SetWindowsHookExW(WH_KEYBOARD_LL)")?;
    Ok(KeyboardHookGuard { handle })
}

/// External "you're locked now" signal — invoked by the popup after 3
/// failed verifies. Absorbs subsequent digits silently inside the hook
/// so the child never sees new asterisks after the lockout landed.
/// No-op if no pincode capture is active.
pub fn mark_locked() {
    if let Some(s) = HOOK_STATE.lock().unwrap().as_mut() {
        s.buffer.force_lock();
    }
}

impl Drop for KeyboardHookGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.handle);
        }
        // Clear state so a subsequent install starts fresh. Matters
        // for the debug rebuild loop and, in principle, for future
        // recovery paths that reinstall the hook mid-popup.
        *HOOK_STATE.lock().unwrap() = None;
    }
}

/// VK-based digit resolver — the ONLY reliable way to detect digit
/// intent across every keyboard layout and NumLock state.
///
/// Windows names virtual-key codes by physical position for the digit
/// rows, NOT by the character produced :
///   - `VK_0..VK_9` (0x30..0x39) = the top-row digit keys, whatever
///     layout is loaded. On FR AZERTY the top-row `1/&` key is still
///     `VK_1` even though Shift is required to produce the digit char.
///   - `VK_NUMPAD0..VK_NUMPAD9` (0x60..0x69) = numpad digit keys when
///     NumLock is ON. NumLock OFF makes Windows send `VK_END`/`VK_HOME`
///     etc. instead — user's own choice, not our problem.
///
/// A layout-aware detour via `ToUnicodeEx` would be strictly overkill :
/// VK codes are stable, the char produced is irrelevant to us because
/// we only care that the user intended a digit. Shift needs a separate
/// pass-through in `is_modifier_vk` (top-row digit keys on AZERTY /
/// German-style layouts require Shift to reach the digits, but Shift
/// itself must not classify as "non-digit → cancel").
fn typed_digit(vk: u16) -> Option<char> {
    if (VK_0.0..=VK_9.0).contains(&vk) {
        return Some((b'0' + (vk - VK_0.0) as u8) as char);
    }
    if (VK_NUMPAD0.0..=VK_NUMPAD9.0).contains(&vk) {
        return Some((b'0' + (vk - VK_NUMPAD0.0) as u8) as char);
    }
    None
}

fn is_ctrl_vk(vk: u16) -> bool {
    vk == VK_CONTROL.0 || vk == VK_LCONTROL.0 || vk == VK_RCONTROL.0
}

/// Modifier keys that must NOT count as "non-digit" input during
/// Ctrl+Shift+pincode capture. On AZERTY / Mac keyboards the top-row
/// digit keys require Shift, so a naive "any non-digit vkey cancels"
/// rule would abort every pincode entry the moment Shift is pressed.
/// Modifiers are pass-through — the following digit keydown (with
/// Ctrl still held) is what carries the semantic input. Alt is
/// included here (not the capture prefix any more) so a stray Alt-
/// tap mid-entry doesn't cancel the buffer either.
fn is_modifier_vk(vk: u16) -> bool {
    vk == VK_SHIFT.0
        || vk == VK_LSHIFT.0
        || vk == VK_RSHIFT.0
        || vk == VK_MENU.0
        || vk == VK_LMENU.0
        || vk == VK_RMENU.0
        || vk == VK_CAPITAL.0
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Below-zero codes must be forwarded unchanged per MSDN — the OS is
    // asking to pass the event to the next hook without inspection.
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let msg = wparam.0 as u32;
    let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let vk = info.vkCode as u16;

    if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
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
            // Log the swallow decision so debugging can confirm whether
            // the LL hook is receiving these OS-level shortcuts at all.
            // If Win+D reaches here → hook is alive on the OS input
            // queue ; if it doesn't fire while the popup is up, the
            // hook is either silently unhooked (LowLevelHooksTimeout)
            // or the keystroke never reached the guest OS input queue
            // (some hypervisor keyboard passthroughs intercept upstream).
            tracing::info!(
                vk = format!("0x{:02X}", vk),
                alt,
                ctrl,
                win,
                "pincode.raw: swallowed shortcut (Alt+F4/Alt+Tab/Win+D/Ctrl+Esc)"
            );
            return LRESULT(1);
        }

        // Pincode capture — only fires when a HookState was installed
        // (i.e. install_keyboard_hook_with_pincode was used).
        if let Some(state) = HOOK_STATE.lock().unwrap().as_mut() {
            // Unconditional per-keydown log while capture is active.
            // Reveals what VK the OS actually delivers for each
            // physical key on exotic setups (VM keyboard passthroughs
            // can remap Ctrl / Option / etc. to unexpected VKs).
            // Bounded — popup lifetime is ≤ 60s+dismiss, so a few
            // thousand log lines max.
            tracing::info!(
                vk = format!("0x{:02X}", vk),
                scan = info.scanCode,
                extended = (info.flags.0 & 0x01) != 0,
                syskey = msg == WM_SYSKEYDOWN,
                ctrl_held = state.ctrl_held,
                ctrl_async = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0,
                shift_async = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0,
                menu_async = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0,
                "pincode.raw: keydown seen"
            );
            if is_ctrl_vk(vk) {
                state.ctrl_held = true;
                tracing::info!(
                    vk = format!("0x{:02X}", vk),
                    scan = info.scanCode,
                    "pincode: ctrl down (capture armed)"
                );
                // Swallow Ctrl-down so no downstream Ctrl+letter
                // shortcut in the frontmost window fires while we're
                // capturing. The Ctrl-up path also swallows, keeping
                // the pair symmetric.
                return LRESULT(1);
            }
            if state.ctrl_held {
                // Defensive re-check : if Ctrl-up was somehow missed
                // (LowLevelHooksTimeout kicked us momentarily), the
                // OS's async key state is authoritative. Force a
                // synthetic Ctrl-release before processing this event.
                let ctrl_still_down = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
                if !ctrl_still_down {
                    tracing::info!(
                        vk = format!("0x{:02X}", vk),
                        "pincode: ctrl missed release - synthesizing release + processing key normally"
                    );
                    state.ctrl_held = false;
                    let verdict = state.buffer.on_capture_release();
                    match verdict {
                        Verdict::Submit(s) => (state.send)(PincodeEvent::Submit(s)),
                        Verdict::Cancel => (state.send)(PincodeEvent::Cancel),
                    }
                    // Fall through to normal handling of the current
                    // key (no longer inside capture).
                    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                }
                // Ignore pure modifier keydowns (Shift, Ctrl,
                // CapsLock). On AZERTY / Mac layouts Shift is required
                // to reach the top-row digit keys, so a modifier
                // keydown must NOT be treated as "non-digit → cancel"
                // (which would kill the pincode before the digit
                // arrives). Modifiers produce no character and are
                // safe to pass through.
                if is_modifier_vk(vk) {
                    tracing::info!(
                        vk = format!("0x{:02X}", vk),
                        "pincode: modifier held (pass-through, buffer preserved)"
                    );
                    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                }
                // VK-based digit resolution — VK codes are stable
                // across every layout for the top-row (VK_0..VK_9) and
                // numpad (VK_NUMPAD0..VK_NUMPAD9) digit keys. See
                // `typed_digit` docstring for why we ripped out the
                // ToUnicodeEx detour that a layout-aware approach would use.
                let typed = typed_digit(vk);
                tracing::info!(
                    vk = format!("0x{:02X}", vk),
                    scan = info.scanCode,
                    typed = typed.map(|c| c.to_string()),
                    shift_down = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0,
                    "pincode: keydown while ctrl (digit branch)"
                );
                if let Some(digit) = typed {
                    if let Some(idx) = state.buffer.on_digit(digit) {
                        (state.send)(PincodeEvent::Digit(idx));
                    }
                    // Swallow digits during capture so they never leak
                    // to the frontmost window, even when the buffer
                    // rejected them (locked / at cap).
                    return LRESULT(1);
                }
                // Non-digit while Alt is held clears the buffer so
                // accidental Alt+letter combinations don't leak into
                // a later verify. We do NOT swallow — the Alt+F4 /
                // Alt+Tab suppression above already handled the
                // dangerous combinations, other Alt+letter combos
                // should reach the frontmost window as usual.
                tracing::info!(
                    vk = format!("0x{:02X}", vk),
                    "pincode: non-digit while ctrl - cancelling buffer"
                );
                state.buffer.on_non_digit();
                (state.send)(PincodeEvent::Cancel);
            }
        }
    } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
        if let Some(state) = HOOK_STATE.lock().unwrap().as_mut() {
            if state.ctrl_held && is_ctrl_vk(vk) {
                state.ctrl_held = false;
                let verdict = state.buffer.on_capture_release();
                match verdict {
                    Verdict::Submit(s) => (state.send)(PincodeEvent::Submit(s)),
                    Verdict::Cancel => (state.send)(PincodeEvent::Cancel),
                }
                return LRESULT(1);
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
