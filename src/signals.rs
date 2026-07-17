// On non-Windows targets the production callers of these helpers all
// live under `platform::win32::*` and are cfg-gated away, so every fn
// looks dead outside of `mod tests`. Silence the noise on Linux dev.
#![cfg_attr(not(windows), allow(dead_code))]

// Cross-platform pure helpers backing the Win32 signal wrappers under
// `platform::win32::{idle, gamepad, power_notify, busy_detector}`. Each
// helper is Linux-unit-testable ; the Windows-only wrapper delegates to
// it after unpacking the Win32 types (LASTINPUTINFO, XINPUT_STATE,
// WPARAM, QUERY_USER_NOTIFICATION_STATE).

// -- idle --------------------------------------------------------------

/// Seconds elapsed between the current `GetTickCount64` tick (ms since
/// boot, u64) and `LASTINPUTINFO::dwTime` (ms since boot, u32 wrapped
/// on the Win32 side after 49.7 days of uptime).
///
/// Uses `saturating_sub` to floor at 0 rather than underflow — defensive
/// against the impossible-in-practice case where `dwTime` reads a value
/// higher than the current tick (would require clock drift between the
/// two syscalls on the same monotonic counter).
pub fn ms_delta_to_seconds(tick_ms: u64, last_input_ms: u32) -> u64 {
    tick_ms.saturating_sub(last_input_ms as u64) / 1000
}

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

// -- busy_detector -----------------------------------------------------

/// True if the `QUERY_USER_NOTIFICATION_STATE` value indicates a busy
/// context :
///   - `QUNS_BUSY` = 2 — Direct3D exclusive full-screen mode / other busy.
///   - `QUNS_RUNNING_D3D_FULL_SCREEN` = 3 — full-screen game.
///   - `QUNS_PRESENTATION_MODE` = 4 — PowerPoint / equivalent.
///
/// Every other value (including `QUNS_NOT_PRESENT` = 1 and
/// `QUNS_ACCEPTS_NOTIFICATIONS` = 5) is treated as not-busy.
pub fn quns_state_is_busy(state: u32) -> bool {
    matches!(state, 2..=4)
}

// -- tick decisions ----------------------------------------------------

/// True when the current tick should accrue `paused_secs` : the user
/// has been idle at least `threshold_secs`, no gamepad packet has
/// bumped since the previous call, and the busy detector reports the
/// context is not a full-screen game / presentation / active audio
/// session.
///
/// Ordering matters : short-circuit on gamepad and busy before
/// touching `idle_secs` in the caller is not required (the args are
/// all evaluated), but the boolean AND short-circuits inside.
pub fn idle_tick_should_pause(
    idle_secs: u64,
    threshold_secs: u64,
    gamepad_active: bool,
    is_busy: bool,
) -> bool {
    idle_secs >= threshold_secs && !gamepad_active && !is_busy
}

/// Decision returned by `busy_defer_decision`.
#[derive(Debug, PartialEq, Eq)]
pub enum DeferOutcome {
    /// Not busy — fire the popup now. Reset `deferred_secs` to 0.
    Fire,
    /// Busy but under the cap — hold, and use the returned running
    /// total as the new `deferred_secs`.
    Defer(u64),
    /// Busy but the cap has been reached — fire anyway. Reset
    /// `deferred_secs` to 0.
    ForceFire,
}

/// Popup-fire arbitration when the busy detector flags the current
/// context. `deferred_secs` is the caller's running counter (reset
/// on every actual fire). `tick_delta_secs` is what would be added
/// this tick ; `cap_secs` is `MAX_DEFERRAL_SECS` (15 min in prod).
///
/// Saturating add keeps the arithmetic safe even at u64::MAX — cap
/// comparison still works because saturation lands at u64::MAX ≥
/// cap ⇒ ForceFire.
pub fn busy_defer_decision(
    is_busy: bool,
    deferred_secs: u64,
    tick_delta_secs: u64,
    cap_secs: u64,
) -> DeferOutcome {
    if !is_busy {
        return DeferOutcome::Fire;
    }
    let next = deferred_secs.saturating_add(tick_delta_secs);
    if next >= cap_secs {
        DeferOutcome::ForceFire
    } else {
        DeferOutcome::Defer(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ms_delta_to_seconds ------------------------------------------

    #[test]
    fn ms_delta_normal_case_rounds_down() {
        // 12_500 ms delta → 12 s (integer division truncates).
        assert_eq!(ms_delta_to_seconds(20_000, 7_500), 12);
    }

    #[test]
    fn ms_delta_zero_when_ticks_equal() {
        assert_eq!(ms_delta_to_seconds(1_234_567, 1_234_567), 0);
    }

    #[test]
    fn ms_delta_saturates_when_tick_behind_last_input() {
        // Impossible in practice, but defensive : must not underflow.
        assert_eq!(ms_delta_to_seconds(5_000, 10_000), 0);
    }

    #[test]
    fn ms_delta_exact_second_boundary() {
        // 60_000 ms delta = 60 s exactly.
        assert_eq!(ms_delta_to_seconds(60_000, 0), 60);
    }

    #[test]
    fn ms_delta_with_u32_near_max_last_input() {
        // last_input near u32::MAX still fits after the widening cast.
        // u32::MAX = 4_294_967_295 ms ≈ 49.7 days of uptime.
        let last_input = u32::MAX;
        let tick = u32::MAX as u64 + 30_000; // 30 s after last input
        assert_eq!(ms_delta_to_seconds(tick, last_input), 30);
    }

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

    // -- quns_state_is_busy -------------------------------------------

    #[test]
    fn quns_busy_value_2_is_busy() {
        assert!(quns_state_is_busy(2));
    }

    #[test]
    fn quns_d3d_fullscreen_value_3_is_busy() {
        assert!(quns_state_is_busy(3));
    }

    #[test]
    fn quns_presentation_mode_value_4_is_busy() {
        assert!(quns_state_is_busy(4));
    }

    #[test]
    fn quns_not_present_value_1_is_not_busy() {
        assert!(!quns_state_is_busy(1));
    }

    #[test]
    fn quns_accepts_notifications_value_5_is_not_busy() {
        assert!(!quns_state_is_busy(5));
    }

    #[test]
    fn quns_zero_and_out_of_range_are_not_busy() {
        assert!(!quns_state_is_busy(0));
        assert!(!quns_state_is_busy(99));
        assert!(!quns_state_is_busy(u32::MAX));
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

    // -- busy_defer_decision ------------------------------------------

    #[test]
    fn defer_not_busy_returns_fire() {
        assert_eq!(
            busy_defer_decision(false, 300, 5, 900),
            DeferOutcome::Fire
        );
    }

    #[test]
    fn defer_busy_under_cap_returns_defer_with_running_total() {
        assert_eq!(
            busy_defer_decision(true, 300, 5, 900),
            DeferOutcome::Defer(305)
        );
    }

    #[test]
    fn defer_busy_at_cap_returns_force_fire() {
        // 895 + 5 = 900 → force-fire (>=).
        assert_eq!(
            busy_defer_decision(true, 895, 5, 900),
            DeferOutcome::ForceFire
        );
    }

    #[test]
    fn defer_busy_over_cap_returns_force_fire() {
        assert_eq!(
            busy_defer_decision(true, 1_000, 5, 900),
            DeferOutcome::ForceFire
        );
    }

    #[test]
    fn defer_saturating_add_never_panics_at_u64_max() {
        // deferred_secs + tick_delta_secs would overflow ; saturating
        // add caps at u64::MAX and cap comparison classifies as force.
        assert_eq!(
            busy_defer_decision(true, u64::MAX, 5, 900),
            DeferOutcome::ForceFire
        );
    }
}
