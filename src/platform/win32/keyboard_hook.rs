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
// **Ctrl over Alt (session 7 follow-up)** : the original design used
// Alt as the capture prefix, but on AZERTY Windows AltGr = Right-Alt
// participates in layout resolution (AltGr+digit produces `~#{[|`\^@`
// on FR AZERTY). The Mac→VM keyboard driver can silently classify
// Option as AltGr in some passthrough modes, and ToUnicodeEx then
// returns the AltGr variant instead of the digit. Ctrl is a pure
// command modifier — no Windows layout uses Ctrl+digit for a
// printable char — so it doesn't collide with layout-shift chars.

use std::sync::Mutex;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardLayout, ToUnicodeEx, VK_CAPITAL, VK_CONTROL, VK_D,
    VK_ESCAPE, VK_F4, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, SetWindowsHookExW,
    UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
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

/// Resolve the character the current keydown would produce *if Alt
/// were not held*, using the active keyboard layout of the foreground
/// window. Returns `Some('0'..'9')` when the resulting character is an
/// ASCII digit, `None` otherwise (letter, symbol, dead key, or no
/// mapping at all).
///
/// Layout-aware — works for QWERTY top-row `1`, AZERTY `Shift+1`,
/// numpad `1` with NumLock on, and any exotic layout that produces a
/// digit through some other combination. Deliberately avoids Alt in
/// the synthesized key-state array : with Alt down, `ToUnicodeEx`
/// suppresses most character output, so we ask "what would this key
/// combo type in a normal editor, ignoring Alt?" and check that.
///
/// The `WM_KEYDOWN` scan code is taken directly from
/// `KBDLLHOOKSTRUCT.scanCode`. `wFlags = 2` prevents dead-key state
/// mutation so a partial pincode entry doesn't leave the layout in a
/// half-typed diacritic state for the frontmost window.
/// Diagnostic variant of the layout-aware digit resolver. Returns
/// `(parsed_digit, raw ToUnicodeEx return count, first char if n == 1)`
/// so the pincode capture branch can log distinguishable evidence of
/// "layout returned no char" (n=0) vs. "layout returned a non-digit
/// char" (n=1 with tu_ch=Some) vs. "dead key" (n=-1) vs. "multi-char
/// sequence" (n>1) without needing separate instrumentation of
/// ToUnicodeEx. Doc comment above `typed_digit` in the pre-diag
/// version explains the layout-aware rationale.
fn typed_digit_diag(vk: u16, scan: u32) -> (Option<char>, i32, Option<char>) {
    unsafe {
        let mut key_state = [0u8; 256];
        if GetAsyncKeyState(VK_SHIFT.0 as i32) < 0 {
            key_state[VK_SHIFT.0 as usize] = 0x80;
            key_state[VK_LSHIFT.0 as usize] = 0x80;
        }
        // Deliberately do NOT set VK_CONTROL — Ctrl is the pincode
        // capture prefix (session 7 b.11 switch), NOT part of the
        // typed character. ToUnicodeEx with Ctrl held returns 0 or a
        // control code (Ctrl+A → 0x01) for most keys, which would
        // hide the digit intent. Same rationale that used to apply
        // to VK_MENU under the old Alt-prefix design.
        if (GetKeyState(VK_CAPITAL.0 as i32) & 1) != 0 {
            key_state[VK_CAPITAL.0 as usize] = 0x01;
        }

        let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let hkl = GetKeyboardLayout(fg_thread);

        let mut buf = [0u16; 8];
        let n = ToUnicodeEx(vk as u32, scan, &key_state, &mut buf, 2, Some(hkl));
        let ch = if n == 1 { char::from_u32(buf[0] as u32) } else { None };
        let digit = ch.filter(|c| c.is_ascii_digit());
        (digit, n, ch)
    }
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
            return LRESULT(1);
        }

        // Pincode capture — only fires when a HookState was installed
        // (i.e. install_keyboard_hook_with_pincode was used).
        if let Some(state) = HOOK_STATE.lock().unwrap().as_mut() {
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
                // B6 : ignore pure modifier keydowns (Shift, Ctrl,
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
                // B6 : layout-aware digit resolution. Ask the active
                // keyboard layout what character this vk+scan+modifier
                // combo actually types. Handles top-row `1`, Shift+`&`
                // on AZERTY, numpad `1`, and any other layout that
                // routes to '0'..'9'.
                let (typed, tu_n, tu_ch) = typed_digit_diag(vk, info.scanCode);
                tracing::info!(
                    vk = format!("0x{:02X}", vk),
                    scan = info.scanCode,
                    tu_n,
                    tu_ch = tu_ch.map(|c| format!("U+{:04X}", c as u32)),
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
