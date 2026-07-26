// Keyboard + mouse idleness signal for the service tick loop.
// Does NOT call `GetLastInputInfo` directly (see below for why) —
// reads the timestamp pushed by the user-session input probe
// (`crate::modes::input_probe`) over the named-pipe IPC channel.
//
// # Session 0 pitfall
//
// A Windows service runs as SYSTEM in Session 0 which never receives
// user input. `GetLastInputInfo` reads via a session-scoped window
// station : Session 0's WinSta0 has no interactive input, so a direct
// call from the service returns `info.dwTime = 0` forever. Naive
// `ImpersonateLoggedOnUser` swaps the thread's security token but not
// its window station (they're not scoped by token), so impersonation
// is ineffective for this API.
//
// # Transport
//
// A lightweight `--input-probe` child process spawned in the
// interactive user session polls `GetLastInputInfo` on its own turf
// and pushes the absolute u64 tick of the last input to the service's
// named-pipe IPC (`Message::InputStamp { tick_ms }`) every ~5 s. The
// service's pipe thread drains those onto a secondary mpsc that the
// tick loop consumes ; each timeout tick materializes the latest
// stamp into `InputStampState` and passes a reference here.
//
// (b.8 used a file at `<exe_dir>/Data/lastinput.dat` for the same
// transport ; b.9 switched to the pipe — zero disk writes, reuses the
// existing IPC infra between config UI and service.)
//
// # Fallbacks
//
// State missing / older than `MAX_PROBE_STALENESS_MS` → return 0
// (assume active). Any probe glitch errs toward firing the popup
// rather than silently deferring forever.

use windows::Win32::System::SystemInformation::GetTickCount64;

/// Beyond this staleness we treat the probe as dead. 30 s = 6× the
/// probe's 5 s push cadence, so a healthy probe never hits the
/// threshold and a crashed one is detected within one service tick.
/// Also bounds the media flag in `media_playing` — one constant, one
/// decay convention.
const MAX_PROBE_STALENESS_MS: u64 = 30_000;

/// Latest input-stamp snapshot materialized by the service tick loop
/// from the pipe transport. Mirrors the two-u64 shape of the b.8 file
/// transport so `idle_seconds` staleness math is unchanged.
#[derive(Debug, Clone, Copy)]
pub struct InputStampState {
    /// Absolute `GetTickCount64` of the last user input, as reported
    /// by the user-session probe over the IPC pipe.
    pub last_input_tick_ms: u64,
    /// Absolute `GetTickCount64` when this stamp was received on the
    /// service side. Used for staleness detection so a dead probe is
    /// caught within one service tick.
    pub received_at_tick_ms: u64,
    /// WASAPI "any active render session" flag sampled by the probe in
    /// the user session (session-scoped API, unreadable from Session 0
    /// — see `busy_detector.rs`).
    pub playing_audio: bool,
    /// SMTC "any transport session reports Playing" flag — catches a
    /// video muted in the player/tab whose audio stream is stopped.
    pub playing_smtc: bool,
    /// Workstation-lock flag sampled by the probe via the active input
    /// desktop (session/desktop-scoped — see `lock_detector.rs`). A hard
    /// "inactive" override: a locked session means the human is away.
    pub locked: bool,
}

/// Seconds since the last keyboard or mouse input on the interactive
/// user session, sourced from the input-probe IPC push. Returns 0 on
/// missing / stale state so the tick errs toward "assume active".
pub fn idle_seconds(state: Option<&InputStampState>) -> u64 {
    let Some(s) = state else {
        // Probe never pushed yet — usually the first few ticks after
        // service startup while the probe is spawning + connecting.
        return 0;
    };
    let current_tick = unsafe { GetTickCount64() };
    let staleness_ms = current_tick.saturating_sub(s.received_at_tick_ms);
    if staleness_ms > MAX_PROBE_STALENESS_MS {
        tracing::warn!(
            staleness_ms,
            "input-probe stale - assuming active (probe may have crashed)"
        );
        return 0;
    }
    let idle_ms = current_tick.saturating_sub(s.last_input_tick_ms);
    idle_ms / 1000
}

/// True if the user-session probe last reported media playing on an
/// ENABLED signal (audio / SMTC toggles from the config) AND the stamp
/// is fresh. Missing / stale state → false : no worse than the
/// pre-1.6.1 behavior (a Session 0 call always returned false) and,
/// combined with `idle_seconds` reading 0 on the same stale state, the
/// tick errs toward "assume active" without double-counting a dead
/// probe as media time.
pub fn media_playing(
    state: Option<&InputStampState>,
    count_audio: bool,
    count_media: bool,
) -> bool {
    let Some(s) = state else {
        return false;
    };
    let current_tick = unsafe { GetTickCount64() };
    let staleness_ms = current_tick.saturating_sub(s.received_at_tick_ms);
    crate::signals::media_playing_decision(
        s.playing_audio && count_audio || s.playing_smtc && count_media,
        staleness_ms,
        MAX_PROBE_STALENESS_MS,
    )
}

/// True if the user-session probe last reported the workstation locked
/// AND the stamp is fresh. Missing / stale state → false: never
/// force-pause on a crashed probe (err toward "assume active", matching
/// `idle_seconds`→0 and `media_playing`→false on the same stale state).
/// Reuses `media_playing_decision` — the shared "a probe-reported bool
/// decays to false when stale" gate, one decay convention.
pub fn screen_locked(state: Option<&InputStampState>) -> bool {
    let Some(s) = state else {
        return false;
    };
    let current_tick = unsafe { GetTickCount64() };
    let staleness_ms = current_tick.saturating_sub(s.received_at_tick_ms);
    crate::signals::media_playing_decision(s.locked, staleness_ms, MAX_PROBE_STALENESS_MS)
}
