// On non-Windows targets the production callers of these helpers all
// live under `platform::win32::*` and are cfg-gated away, so every fn
// looks dead outside of `mod tests`. Silence the noise on Linux dev.
#![cfg_attr(not(windows), allow(dead_code))]

// Cross-platform pure helpers backing the Win32 signal wrappers under
// `platform::win32::{idle, gamepad, power_notify, busy_detector}`. Each
// helper is Linux-unit-testable ; the Windows-only wrapper delegates to
// it after unpacking the Win32 types (LASTINPUTINFO, XINPUT_STATE,
// WPARAM, QUERY_USER_NOTIFICATION_STATE).

// -- gamepad -----------------------------------------------------------

/// Per-slot XInput packet number, or None if the slot is disconnected.
/// `dwPacketNumber` in XInput increments if and only if button / stick /
/// trigger state changes — comparing packet numbers is safer AND simpler
/// than deep-comparing `XINPUT_GAMEPAD` fields.
pub type GamepadSnapshot = [Option<u32>; 4];

/// True if any slot's packet number changed OR a slot transitioned
/// between connected / disconnected. Reduces to a tuple `!=` but wrapped
/// in a named function so the connect / disconnect intent is testable
/// in isolation.
pub fn snapshot_differs(prev: &GamepadSnapshot, current: &GamepadSnapshot) -> bool {
    prev != current
}

// -- power_notify ------------------------------------------------------

/// Semantic kind of a `WM_POWERBROADCAST` message. Only the two the
/// service cares about ; every other `PBT_*` value classifies as `None`
/// and the wrapper ignores it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PowerEventKind {
    /// `PBT_APMSUSPEND` = 0x0004 — system is about to enter sleep.
    Suspend,
    /// `PBT_APMRESUMEAUTOMATIC` = 0x0012 — system has resumed from sleep.
    Resume,
}

/// Map the `WPARAM` of a `WM_POWERBROADCAST` message to a semantic
/// event kind. Returns `None` for `PBT_APMSUSPENDED` (post-suspend
/// no-op), `PBT_POWERSETTINGCHANGE`, and any other value.
pub fn classify_wm_powerbroadcast(wparam: usize) -> Option<PowerEventKind> {
    match wparam as u32 {
        0x0004 => Some(PowerEventKind::Suspend),
        0x0012 => Some(PowerEventKind::Resume),
        _ => None,
    }
}

// -- tick decisions ----------------------------------------------------

/// True when the current tick should accrue `paused_secs` : the user
/// has been idle at least `threshold_secs`, no gamepad packet has
/// bumped since the previous call, and no WASAPI audio/video session
/// is currently active.
///
/// Ordering matters : short-circuit on gamepad and playing before
/// touching `idle_secs` in the caller is not required (the args are
/// all evaluated), but the boolean AND short-circuits inside.
pub fn idle_tick_should_pause(
    idle_secs: u64,
    threshold_secs: u64,
    gamepad_active: bool,
    playing_video_or_audio: bool,
) -> bool {
    idle_secs >= threshold_secs && !gamepad_active && !playing_video_or_audio
}

// -- pincode buffer ---------------------------------------------------

/// Outcome returned by `PincodeBuffer::on_alt_release`.
///
/// `Submit(chars)` carries a buffer that reached the configured `cap` —
/// caller verifies against the argon2id hash. `Cancel` means the buffer
/// was short (or empty) at Alt-release, so nothing to verify.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Submit(String),
    Cancel,
}

/// Pure state machine backing the popup's Alt+pincode emergency unlock.
///
/// The Win32 low-level keyboard hook feeds digit / non-digit / Alt-release
/// events into it ; the buffer decides accept / cap / verdict / lock.
/// Isolating the invariants here keeps them Linux-testable (Win32 hook
/// glue is `#[cfg(windows)]` and unreachable from CI).
///
/// Popup-scoped semantics per the design : the buffer lives only as long
/// as the popup process. Lockout is triggered externally by the popup
/// (via `force_lock` after 3 failed verifies) and lasts until the popup
/// exits. This differs from `modes::config::lockout::Lockout` which has
/// a 30 s wall-clock timer suited to the Config UI ; we do not share the
/// struct because the 3-strike counter naturally lives in the popup's
/// event loop where the verify happens.
pub struct PincodeBuffer {
    chars: Vec<char>,
    cap: usize,
    locked: bool,
}

impl PincodeBuffer {
    pub fn new(cap: usize) -> Self {
        Self { chars: Vec::with_capacity(cap), cap, locked: false }
    }

    /// Push a digit character. Returns the newly-added asterisk index
    /// (0-based) when accepted, or `None` when : the buffer is locked,
    /// `c` is not an ASCII digit, or the cap is already reached.
    /// Silent absorption of over-cap input protects against a stuck Alt
    /// + auto-repeat filling memory.
    pub fn on_digit(&mut self, c: char) -> Option<u32> {
        if self.locked || !c.is_ascii_digit() || self.chars.len() >= self.cap {
            return None;
        }
        self.chars.push(c);
        Some((self.chars.len() - 1) as u32)
    }

    /// Non-digit key seen while Alt is still held — clears the buffer
    /// so accidental Alt+letter combinations don't leak into a later
    /// verify. Does NOT return a verdict (Alt is still down).
    pub fn on_non_digit(&mut self) {
        self.chars.clear();
    }

    /// Alt released. Full-length buffer → `Submit(chars)` for verify ;
    /// anything shorter → `Cancel`. Always clears the buffer so the
    /// next Alt-press starts fresh regardless of verdict.
    pub fn on_alt_release(&mut self) -> Verdict {
        let verdict = if self.chars.len() == self.cap && self.cap > 0 {
            Verdict::Submit(self.chars.iter().collect())
        } else {
            Verdict::Cancel
        };
        self.chars.clear();
        verdict
    }

    /// External "you're locked now" signal — invoked when the popup's
    /// verify counter hits 3 fails. Absorbs subsequent input silently
    /// for the rest of this popup's lifetime.
    pub fn force_lock(&mut self) {
        self.locked = true;
        self.chars.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- snapshot_differs ---------------------------------------------

    #[test]
    fn snapshot_identical_snapshots_return_false() {
        let snap: GamepadSnapshot = [Some(42), None, Some(7), None];
        assert!(!snapshot_differs(&snap, &snap));
    }

    #[test]
    fn snapshot_single_slot_packet_bump_returns_true() {
        let prev: GamepadSnapshot = [Some(42), None, None, None];
        let current: GamepadSnapshot = [Some(43), None, None, None];
        assert!(snapshot_differs(&prev, &current));
    }

    #[test]
    fn snapshot_connect_transition_returns_true() {
        // Slot 2 goes from disconnected to just-connected (packet 0).
        let prev: GamepadSnapshot = [None; 4];
        let current: GamepadSnapshot = [None, None, Some(0), None];
        assert!(snapshot_differs(&prev, &current));
    }

    #[test]
    fn snapshot_disconnect_transition_returns_true() {
        let prev: GamepadSnapshot = [Some(100), None, None, None];
        let current: GamepadSnapshot = [None; 4];
        assert!(snapshot_differs(&prev, &current));
    }

    #[test]
    fn snapshot_all_disconnected_stays_false() {
        let prev: GamepadSnapshot = [None; 4];
        let current: GamepadSnapshot = [None; 4];
        assert!(!snapshot_differs(&prev, &current));
    }

    // -- classify_wm_powerbroadcast -----------------------------------

    #[test]
    fn classify_suspend_maps_to_suspend() {
        assert_eq!(
            classify_wm_powerbroadcast(0x0004),
            Some(PowerEventKind::Suspend)
        );
    }

    #[test]
    fn classify_resume_automatic_maps_to_resume() {
        assert_eq!(
            classify_wm_powerbroadcast(0x0012),
            Some(PowerEventKind::Resume)
        );
    }

    #[test]
    fn classify_apm_suspended_no_op_returns_none() {
        // PBT_APMSUSPENDED = 0x0007 : post-suspend broadcast, no-op for us.
        assert_eq!(classify_wm_powerbroadcast(0x0007), None);
    }

    #[test]
    fn classify_arbitrary_wparam_returns_none() {
        assert_eq!(classify_wm_powerbroadcast(0), None);
        assert_eq!(classify_wm_powerbroadcast(0xDEAD_BEEF), None);
        // PBT_POWERSETTINGCHANGE = 0x8013
        assert_eq!(classify_wm_powerbroadcast(0x8013), None);
    }

    // -- idle_tick_should_pause ---------------------------------------

    #[test]
    fn idle_pause_at_threshold_boundary_is_true() {
        assert!(idle_tick_should_pause(60, 60, false, false));
    }

    #[test]
    fn idle_pause_below_threshold_is_false() {
        assert!(!idle_tick_should_pause(59, 60, false, false));
    }

    #[test]
    fn idle_pause_gamepad_active_short_circuits() {
        assert!(!idle_tick_should_pause(600, 60, true, false));
    }

    #[test]
    fn idle_pause_busy_short_circuits() {
        assert!(!idle_tick_should_pause(600, 60, false, true));
    }

    #[test]
    fn idle_pause_all_conditions_ok_is_true() {
        assert!(idle_tick_should_pause(3600, 60, false, false));
    }

    // -- PincodeBuffer -----------------------------------------------

    #[test]
    fn pincode_digit_accepted_returns_incrementing_indices() {
        let mut b = PincodeBuffer::new(4);
        assert_eq!(b.on_digit('1'), Some(0));
        assert_eq!(b.on_digit('2'), Some(1));
        assert_eq!(b.on_digit('3'), Some(2));
        assert_eq!(b.on_digit('4'), Some(3));
    }

    #[test]
    fn pincode_cap_enforced_returns_none_after_limit() {
        let mut b = PincodeBuffer::new(4);
        for c in ['1', '2', '3', '4'] {
            assert!(b.on_digit(c).is_some());
        }
        // Over-cap is silently absorbed — protects against stuck Alt
        // + auto-repeat.
        assert_eq!(b.on_digit('5'), None);
        // Alt-release still resolves to Submit with the first 4 chars
        // (not the over-cap 5th).
        assert_eq!(b.on_alt_release(), Verdict::Submit("1234".to_string()));
    }

    #[test]
    fn pincode_alt_release_full_buffer_returns_submit() {
        let mut b = PincodeBuffer::new(6);
        for c in ['4', '2', '4', '2', '4', '2'] {
            b.on_digit(c);
        }
        assert_eq!(b.on_alt_release(), Verdict::Submit("424242".to_string()));
        // Cleared afterwards — next Alt-press starts at idx 0.
        assert_eq!(b.on_digit('9'), Some(0));
    }

    #[test]
    fn pincode_alt_release_short_buffer_returns_cancel_and_clears() {
        let mut b = PincodeBuffer::new(6);
        b.on_digit('1');
        b.on_digit('2');
        b.on_digit('3');
        assert_eq!(b.on_alt_release(), Verdict::Cancel);
        // Cleared — next digit starts at idx 0, not 3.
        assert_eq!(b.on_digit('4'), Some(0));
    }

    #[test]
    fn pincode_force_lock_absorbs_all_input() {
        let mut b = PincodeBuffer::new(4);
        b.on_digit('1');
        b.on_digit('2');
        b.force_lock();
        // Once locked, all input paths become no-ops : digits ignored,
        // non-digit still safe to call, Alt-release yields Cancel with
        // no leak of the pre-lock partial buffer.
        assert_eq!(b.on_digit('3'), None);
        b.on_non_digit();
        assert_eq!(b.on_alt_release(), Verdict::Cancel);
    }
}
