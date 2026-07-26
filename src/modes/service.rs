// Windows service main. Registers the SCM handler, reports Running
// within the 30s deadline, then runs a tick loop that decides when to
// spawn popup / countdown children in the active user's session.
//
// Background threads :
//   - IPC named-pipe server (accept loop, message-mode, overlapped)
//   - state.dat file watcher (notify, hand-debounced ~300ms)
// Both feed TickCommand messages back to the main tick loop over an
// mpsc channel. The tick loop is the only reader/writer of the
// in-memory config + last_popup + fired_paliers state — no mutex.
//
// Last-popup timestamp is persisted to runtime.dat (plaintext big-endian
// u64 seconds since epoch, atomic write via .tmp+rename) so service
// restart doesn't reset the schedule.

use anyhow::Result;

#[cfg(windows)]
pub fn run() -> Result<()> {
    win::run()
}

#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!("service is Windows-only")
}

// Pure daily-pause tick helpers extracted at the top so they compile
// on Linux and are reachable by `cargo test`. `mod win` re-exports them
// via `use super::*;` (no runtime cost, LTO-friendly).

/// Day-boundary detector : does `fired_at_local.date_naive()` differ
/// from `today` ? Called before building `DailyPauseState` so the
/// caller can reset the palier bitmask + skipped-today marker when
/// crossing midnight in the configured TZ.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn day_boundary_crossed(
    fired_local: chrono::DateTime<chrono_tz::Tz>,
    today: chrono::NaiveDate,
) -> bool {
    fired_local.date_naive() != today
}

/// Outcome of the per-tick "did the daily pause already fire today ?"
/// decision, resolved from the persisted `daily_pause_fired_at`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) enum DailyFiredDecision {
    /// No fire timestamp on record — not fired today, nothing to reset.
    NotFired,
    /// The stored fire happened today — `fired_today = true`.
    FiredToday,
    /// The stored fire is from a prior local date : the day rolled over.
    /// The caller must clear the per-day markers — including
    /// `daily_pause_fired_at` itself — so the reset runs ONCE, not every
    /// tick (a stale timestamp left in place keeps re-clearing the palier
    /// de-dup bitmask, which respawns the pre-notif countdown in a loop).
    DayRolledOver,
}

/// Classify the persisted daily-fire timestamp against today's local
/// date. Pure so the day-rollover reset is unit-testable without the
/// tick loop. Reuses [`day_boundary_crossed`] for the date comparison.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn resolve_daily_fired(
    fired_local: Option<chrono::DateTime<chrono_tz::Tz>>,
    today: chrono::NaiveDate,
) -> DailyFiredDecision {
    match fired_local {
        None => DailyFiredDecision::NotFired,
        Some(f) if day_boundary_crossed(f, today) => DailyFiredDecision::DayRolledOver,
        Some(_) => DailyFiredDecision::FiredToday,
    }
}

/// Cas D elapsed clamp : `min(elapsed_secs, duration_secs)` as `u32`.
/// Extracted so the "dismiss immediately when elapsed >= duration"
/// invariant is unit-testable without spawning a real popup.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn clamp_cas_d_elapsed(elapsed_secs: u64, duration_secs: u64) -> u32 {
    elapsed_secs.min(duration_secs) as u32
}

/// Did a config reload move the daily-pause window itself ? Only the
/// fields defining WHEN the pause happens count — text / message edits
/// must not invalidate per-day state. A changed window clears the
/// skipped-today marker and the palier bits (they refer to the old
/// window) ; `fired_today` is cleared only when the NEW window is still
/// in the future (see `daily_rearm_on_window_change` — amended
/// 2026-07-26), else it survives and the Rearm button owns the explicit
/// re-fire.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn daily_window_changed(
    old: &crate::config::schema::DailyPauseConfig,
    new: &crate::config::schema::DailyPauseConfig,
) -> bool {
    old.hour != new.hour
        || old.minute != new.minute
        || old.duration_minutes != new.duration_minutes
        || old.timezone != new.timezone
}

/// Inputs of the session-4 daily↔interval collision decision, resolved
/// by the tick loop from scheduler state + the live child handle.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) struct DailyCollisionInputs {
    /// The daily pause should fire now : enabled, not fired / skipped
    /// today, and `now_local` inside `[fire_at, grace_window_end]`.
    pub daily_due: bool,
    pub popup_pending: bool,
    /// HANDLE-based liveness of the tracked popup child.
    pub popup_child_alive: bool,
    /// The live child IS already the daily popup (`popup_pid` match) —
    /// nothing to convert.
    pub current_popup_is_daily: bool,
    /// An `UpdateContent{DailyPause}` was already sent and its ack is
    /// still awaited — don't re-send every tick while it's in flight.
    pub ack_pending: bool,
}

/// Should the service mutate the RUNNING popup over its content pipe ?
/// The scheduler's `popup_pending` guard defers `FireDailyPause` for as
/// long as a popup is up, so without this path a noon collision would
/// wait for the interval popup to be dismissed. When true, the tick
/// loop sends `UpdateContent` over the popup's per-PID pipe ; the daily
/// fire is stamped only once the popup ACKS the update (else the
/// deadline path kills/respawns deterministically).
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn daily_collision_wants_update(i: &DailyCollisionInputs) -> bool {
    i.daily_due
        && i.popup_pending
        && i.popup_child_alive
        && !i.current_popup_is_daily
        && !i.ack_pending
}

/// After a same-day daily-window change, should today's fired latch be
/// re-armed ? Yes iff the daily already fired today AND the NEW window
/// starts strictly in the future — the parent re-timing the pause to a
/// later moment is an implicit ask for it to fire there (amended
/// decision, 2026-07-26 ; previously fired-today always survived a
/// window move). A move to a past/current time keeps the latch so the
/// pause can't instantly re-fire ; Rearm stays the explicit escape.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn daily_rearm_on_window_change(
    fired_at: Option<std::time::SystemTime>,
    new_fire_at_today: chrono::DateTime<chrono_tz::Tz>,
    now_local: chrono::DateTime<chrono_tz::Tz>,
) -> bool {
    fired_at.is_some() && now_local < new_fire_at_today
}

/// Interval anchor after a daily fire : the END of the lunch pause, so
/// the next interval popup is `daily_hour + daily_duration + interval +
/// inactive_time` — never inside or right after the noon pause. Every
/// daily-fire path stamps through this ; the clean daily-close path
/// later overwrites with the ACTUAL close time (actual end wins).
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn daily_lunch_end_anchor(
    fire_at: std::time::SystemTime,
    daily_duration_secs: u64,
) -> std::time::SystemTime {
    fire_at + std::time::Duration::from_secs(daily_duration_secs)
}

/// In-flight daily `UpdateContent` awaiting its popup-side ack.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) struct PendingDailyAck {
    /// Pid the `UpdateContent{DailyPause}` was written to — the ack must
    /// come from this exact process.
    pub pid: u32,
    /// Past this instant with no ack, the update counts as NOT applied.
    pub deadline: std::time::SystemTime,
}

/// Per-tick outcome for an in-flight daily ack.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PendingAckTick {
    /// Deadline not reached, target still alive — keep waiting.
    Wait,
    /// No ack within the deadline and the popup is still up : the update
    /// is NOT confirmed — deterministic kill/respawn of a real daily
    /// popup (stamp only on spawn success).
    RespawnDeadline,
    /// The targeted popup is gone (dismissed / Reset / crashed) — drop
    /// the pending ack ; the normal no-popup daily path takes over.
    AbandonChildGone,
}

/// Resolve an in-flight daily ack against the current tick. Pure — the
/// tick loop owns the side effects (respawn, stamp, clear).
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn resolve_pending_daily_ack(
    pending: &PendingDailyAck,
    now: std::time::SystemTime,
    child_alive_pid: Option<u32>,
) -> PendingAckTick {
    if child_alive_pid != Some(pending.pid) {
        PendingAckTick::AbandonChildGone
    } else if now >= pending.deadline {
        PendingAckTick::RespawnDeadline
    } else {
        PendingAckTick::Wait
    }
}

#[cfg(test)]
mod tick_helper_tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};
    use chrono_tz::Europe::Paris;

    #[test]
    fn day_boundary_crossed_returns_true_across_midnight() {
        let fired = Paris.with_ymd_and_hms(2026, 7, 23, 23, 30, 0).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert!(day_boundary_crossed(fired, today));
    }

    #[test]
    fn day_boundary_crossed_returns_false_same_day() {
        let fired = Paris.with_ymd_and_hms(2026, 7, 24, 9, 0, 0).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert!(!day_boundary_crossed(fired, today));
    }

    #[test]
    fn resolve_daily_fired_none_is_not_fired() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert!(matches!(
            resolve_daily_fired(None, today),
            DailyFiredDecision::NotFired
        ));
    }

    #[test]
    fn resolve_daily_fired_same_day_is_fired_today() {
        let fired = Paris.with_ymd_and_hms(2026, 7, 24, 12, 0, 0).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert!(matches!(
            resolve_daily_fired(Some(fired), today),
            DailyFiredDecision::FiredToday
        ));
    }

    #[test]
    fn resolve_daily_fired_prior_day_rolls_over() {
        // The respawn-loop repro : yesterday's noon fire still on record
        // while today is inside the pre-notif window. Must report
        // DayRolledOver so the caller clears fired_at ONCE.
        let fired = Paris.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        assert!(matches!(
            resolve_daily_fired(Some(fired), today),
            DailyFiredDecision::DayRolledOver
        ));
    }

    #[test]
    fn clamp_cas_d_elapsed_returns_elapsed_when_smaller() {
        assert_eq!(clamp_cas_d_elapsed(900, 1800), 900);
    }

    #[test]
    fn clamp_cas_d_elapsed_clamps_to_duration_when_larger() {
        // Service was down for 5h ; duration is 30min. Elapsed clamps
        // to 1800s so the dismiss button fires immediately on respawn.
        assert_eq!(clamp_cas_d_elapsed(5 * 3600, 30 * 60), 1800);
    }

    #[test]
    fn clamp_cas_d_elapsed_zero_stays_zero() {
        assert_eq!(clamp_cas_d_elapsed(0, 1800), 0);
    }

    fn collision_inputs() -> DailyCollisionInputs {
        // Baseline = the one combination that DOES trigger the in-place
        // update ; each test flips one field off it.
        DailyCollisionInputs {
            daily_due: true,
            popup_pending: true,
            popup_child_alive: true,
            current_popup_is_daily: false,
            ack_pending: false,
        }
    }

    #[test]
    fn collision_updates_when_interval_popup_alive_and_daily_due() {
        assert!(daily_collision_wants_update(&collision_inputs()));
    }

    #[test]
    fn collision_none_when_not_daily_due() {
        let i = DailyCollisionInputs {
            daily_due: false,
            ..collision_inputs()
        };
        assert!(!daily_collision_wants_update(&i));
    }

    #[test]
    fn collision_none_when_no_popup_pending() {
        // Not pending = no popup up = the normal FireDailyPause path
        // owns the fire ; the collision path must stay out.
        let i = DailyCollisionInputs {
            popup_pending: false,
            ..collision_inputs()
        };
        assert!(!daily_collision_wants_update(&i));
    }

    #[test]
    fn collision_none_when_child_dead() {
        // Pending true but child gone (crashed / killed) : the retry /
        // cold-start paths own recovery, not the pipe.
        let i = DailyCollisionInputs {
            popup_child_alive: false,
            ..collision_inputs()
        };
        assert!(!daily_collision_wants_update(&i));
    }

    #[test]
    fn collision_none_when_current_popup_is_already_daily() {
        let i = DailyCollisionInputs {
            current_popup_is_daily: true,
            ..collision_inputs()
        };
        assert!(!daily_collision_wants_update(&i));
    }

    #[test]
    fn collision_none_while_ack_pending() {
        // An UpdateContent is in flight : no re-send storm while the
        // popup's ack (or the deadline) is awaited.
        let i = DailyCollisionInputs {
            ack_pending: true,
            ..collision_inputs()
        };
        assert!(!daily_collision_wants_update(&i));
    }

    // -- resolve_pending_daily_ack ------------------------------------

    fn pending(pid: u32, deadline_secs: u64) -> PendingDailyAck {
        PendingDailyAck {
            pid,
            deadline: std::time::UNIX_EPOCH + std::time::Duration::from_secs(deadline_secs),
        }
    }

    fn at(secs: u64) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    #[test]
    fn pending_ack_waits_before_deadline_while_child_alive() {
        assert_eq!(
            resolve_pending_daily_ack(&pending(8724, 100), at(99), Some(8724)),
            PendingAckTick::Wait
        );
    }

    #[test]
    fn pending_ack_respawns_at_deadline_while_child_alive() {
        // Deadline inclusive : `now == deadline` already counts as
        // not-confirmed (the tick cadence makes the real check ~5 s
        // after send anyway).
        assert_eq!(
            resolve_pending_daily_ack(&pending(8724, 100), at(100), Some(8724)),
            PendingAckTick::RespawnDeadline
        );
        assert_eq!(
            resolve_pending_daily_ack(&pending(8724, 100), at(500), Some(8724)),
            PendingAckTick::RespawnDeadline
        );
    }

    #[test]
    fn pending_ack_abandons_when_child_gone() {
        // Dismissed / Reset / crashed popup : the no-popup daily path
        // owns the fire now — even past the deadline.
        assert_eq!(
            resolve_pending_daily_ack(&pending(8724, 100), at(500), None),
            PendingAckTick::AbandonChildGone
        );
    }

    #[test]
    fn pending_ack_abandons_when_child_pid_differs() {
        // The tracked child was already replaced by another popup : the
        // pending ack refers to a dead target, drop it.
        assert_eq!(
            resolve_pending_daily_ack(&pending(8724, 100), at(99), Some(999)),
            PendingAckTick::AbandonChildGone
        );
    }

    // -- daily_rearm_on_window_change ---------------------------------

    fn paris_at(h: u32, m: u32) -> chrono::DateTime<chrono_tz::Tz> {
        Paris.with_ymd_and_hms(2026, 7, 26, h, m, 0).unwrap()
    }

    #[test]
    fn rearm_when_fired_and_new_window_is_future() {
        // The host repro : daily fired earlier, parent re-times it to a
        // later moment the same day — the new window must fire.
        assert!(daily_rearm_on_window_change(
            Some(std::time::UNIX_EPOCH),
            paris_at(8, 0),
            paris_at(7, 45),
        ));
    }

    #[test]
    fn no_rearm_when_new_window_is_past_or_current() {
        // Moving the window to a past/current moment must not clear the
        // latch — the pause would instantly re-fire.
        assert!(!daily_rearm_on_window_change(
            Some(std::time::UNIX_EPOCH),
            paris_at(7, 30),
            paris_at(7, 45),
        ));
        assert!(!daily_rearm_on_window_change(
            Some(std::time::UNIX_EPOCH),
            paris_at(7, 45),
            paris_at(7, 45),
        ));
    }

    #[test]
    fn no_rearm_when_not_fired_today() {
        // Nothing fired : nothing to re-arm, the future window fires on
        // its own (must NOT be suppressed either — pinned by daily_due).
        assert!(!daily_rearm_on_window_change(None, paris_at(8, 0), paris_at(7, 45)));
    }

    // -- daily_lunch_end_anchor ---------------------------------------

    #[test]
    fn next_interval_after_daily_is_lunch_end_plus_interval_plus_inactive() {
        // The converged formula : next interval popup =
        // daily_fire + daily_duration + interval + inactive_time.
        let cfg = crate::config::Config {
            interval_hours: 2.0,
            ..crate::config::Config::default()
        };
        let fire = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let duration_secs = 45 * 60;
        let inactive_secs = 600;
        let anchor = daily_lunch_end_anchor(fire, duration_secs);
        assert_eq!(
            crate::scheduler::next_popup_at(anchor, &cfg, inactive_secs),
            fire + std::time::Duration::from_secs(duration_secs + 2 * 3600 + inactive_secs)
        );
    }

    // -- daily_window_changed -----------------------------------------
    //
    // Reload-reconciliation behavior pinned by these tests + the
    // scheduler suite : window changed → skipped marker + palier bits
    // cleared (fresh chance at the new time, full pre-notifs) ;
    // `fired_today` cleared only for a still-future new window
    // (daily_rearm_on_window_change, amended 2026-07-26 — else Rearm
    // owns the re-fire) ; a new window already in the past is
    // re-stamped skipped by the Cas C block on the next tick.

    fn daily_cfg() -> crate::config::schema::DailyPauseConfig {
        crate::config::schema::DailyPauseConfig {
            hour: 12,
            minute: 0,
            ..Default::default()
        }
    }

    #[test]
    fn daily_window_unchanged_for_identical_configs() {
        assert!(!daily_window_changed(&daily_cfg(), &daily_cfg()));
    }

    #[test]
    fn daily_window_changed_on_hour() {
        let new = crate::config::schema::DailyPauseConfig { hour: 14, ..daily_cfg() };
        assert!(daily_window_changed(&daily_cfg(), &new));
    }

    #[test]
    fn daily_window_changed_on_minute() {
        let new = crate::config::schema::DailyPauseConfig { minute: 30, ..daily_cfg() };
        assert!(daily_window_changed(&daily_cfg(), &new));
    }

    #[test]
    fn daily_window_changed_on_duration() {
        let old = daily_cfg();
        let new = crate::config::schema::DailyPauseConfig {
            duration_minutes: old.duration_minutes + 30,
            ..daily_cfg()
        };
        assert!(daily_window_changed(&old, &new));
    }

    #[test]
    fn daily_window_changed_on_timezone() {
        let new = crate::config::schema::DailyPauseConfig {
            timezone: "America/New_York".to_string(),
            ..daily_cfg()
        };
        assert!(daily_window_changed(&daily_cfg(), &new));
    }

    #[test]
    fn daily_window_unchanged_on_text_or_enabled_edits() {
        // Message / title edits and the kill-switch must NOT clear the
        // per-day markers — only the window definition counts.
        let new = crate::config::schema::DailyPauseConfig {
            enabled: false,
            popup_title: "Autre titre".to_string(),
            ..daily_cfg()
        };
        assert!(!daily_window_changed(&daily_cfg(), &new));
    }
}

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, SystemTime};

    use anyhow::{Context, Result};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::Win32::System::Threading::SetEvent;
    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    use crate::config::{self, Config};
    use crate::ipc::{self, Message};
    use crate::modes::install::{
        logs_dir, webview2_dir, DIAGNOSTICS_CACHE_DIR, SERVICE_NAME, STATE_DAT,
    };
    use crate::remote::logs::LogDirs;
    use crate::platform::win32::proc_kill::{is_pid_alive, kill_orphan_webview2_workers};
    use crate::platform::win32::spawn_user::{RealSpawner, SpawnedChild};
    use crate::platform::win32::{gamepad, idle, power_notify};
    use crate::platform::{
        spawn_replacing, spawn_replacing_keyed, spawn_replacing_with_env,
        update_or_spawn_replacing_with_env, UpdateOrSpawn,
    };
    use crate::runtime_dat;
    use crate::scheduler::{self, SchedulerDecision};
    use crate::signals;
    use crate::time_authority;

    const TICK_INTERVAL: Duration = Duration::from_secs(5);
    /// Divergence between wall-clock delta and unbiased-interrupt-time
    /// delta across a tick that triggers a `last_popup` shift. Filters
    /// normal 5 s tick jitter and mpsc receiver latency (typically < 15
    /// s) from being interpreted as a jump.
    const CLOCK_JUMP_THRESHOLD: Duration = Duration::from_secs(60);
    const DEBOUNCE: Duration = Duration::from_millis(300);
    const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
    // Popup crash retry policy. 30/60/90 s backoff × 3 attempts caps
    // the retry storm at 3 min post-crash and gives the popup-side
    // WebView2 wipe path time to settle if the UserData folder was in
    // a stale state.
    const MAX_POPUP_RETRIES: u32 = 3;
    const RETRY_BACKOFF_STEP_SECS: u64 = 30;

    /// In-memory retry state armed when a popup exits with a non-zero
    /// code. Not persisted to `runtime.dat` : if the service dies
    /// mid-retry, the cold-start clean-shutdown gate clears
    /// `popup_pending` post-unclean-shutdown and the next natural cycle
    /// fires the popup cleanly. Cleared on exit-code=0 or on
    /// `attempts >= MAX_POPUP_RETRIES`.
    #[derive(Debug, Clone)]
    struct PopupRetryState {
        attempts: u32,
        next_retry_at: SystemTime,
    }

    #[derive(Debug, Clone, Copy)]
    enum TickCommand {
        Trigger,
        Reload,
        Shutdown,
        RearmDaily,
        ResetPause,
        /// Popup confirmed it applied `UpdateContent{DailyPause}` in its
        /// event loop. Carries the popup's own pid so a stale ack (from
        /// an already-replaced popup) can't stamp the daily fire.
        DailyAck { pid: u32 },
    }

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("service_dispatcher::start")
    }

    fn service_main(_argv: Vec<OsString>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_service));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = ?e, "service exited with error"),
            Err(_) => tracing::error!("service main panicked - SCM watchdog will restart"),
        }
    }

    fn run_service() -> Result<()> {
        let (tx, rx) = mpsc::channel::<TickCommand>();

        let ctrl_tx = tx.clone();
        let status_handle = service_control_handler::register(SERVICE_NAME, move |ctl| {
            on_control_event(ctl, &ctrl_tx)
        })
        .context("service_control_handler::register")?;

        // Report Running IMMEDIATELY — heavy init happens below.
        status_handle
            .set_service_status(running())
            .context("SetServiceStatus(Running)")?;

        let exe = env::current_exe().context("env::current_exe()")?;
        tracing::info!(exe = %exe.display(), "service running");

        // Startup sweep : any msedgewebview2.exe still running from our
        // install's Data\WebView2 is an orphan from a prior session
        // (session-9 Track B-primary revert left this hole). Kill them
        // before we spawn any new popups.
        if let Some(exe_parent) = exe.parent() {
            kill_orphan_webview2_workers(&webview2_dir(exe_parent));
        }

        // b.9 : the input-probe transport moved from a file
        // (<exe_dir>/Data/lastinput.dat) to the IPC pipe. Any pre-
        // existing file left by a b.8 install is now orphan bookkeeping
        // — remove it silently so upgrades leave no stale artefact on
        // disk. Idempotent, ENOENT is not an error.
        if let Some(exe_parent) = exe.parent() {
            let stale = exe_parent.join("Data").join("lastinput.dat");
            let _ = std::fs::remove_file(&stale);
        }

        let state_path = PathBuf::from(STATE_DAT);
        let mut config = config::load_or_default(&state_path);
        let now = SystemTime::now();
        // Cold-start dispatch. No runtime.dat → fresh-install / migrated
        // 0.6.x path via resolve_last_popup (anti-cheat shift). Present
        // runtime.dat → threshold / anti-bypass / relaunch / credit via
        // resolve_cold_start :
        //   threshold  (elapsed >= reset_threshold_hours) → full reset
        //   anti-bypass(pending && !clean, kid killed service) → full reset
        //   relaunch   (pending && clean, user cleanly rebooted PC mid-popup) → refire popup
        //   credit     (else) → keep cycle, credit downtime into paused_secs
        // Daily-pause state pre-loaded here so the four v4 fields survive
        // across `runtime_dat::write` calls in the tick loop. Threaded as
        // `&mut` refs into `tick_loop` alongside the interval state.
        let mut daily_pause_fired_at: Option<SystemTime> = None;
        let mut daily_pause_fired_paliers_bits: u8 = 0;
        let mut daily_pause_skipped_today: Option<chrono::NaiveDate> = None;
        let mut popup_pid: Option<u32> = None;

        let (mut last_popup, mut paused_secs, mut popup_pending, relaunch_now) =
            match runtime_dat::read() {
                None => {
                    let lp = scheduler::resolve_last_popup(
                        scheduler::ResolveInputs {
                            now,
                            last_popup: None,
                            first_install_at: config.first_install_at(),
                            is_reload: false,
                            paused_secs: 0,
                            popup_pending: false,
                            last_heartbeat: SystemTime::UNIX_EPOCH,
                            was_clean_shutdown: false,
                        },
                        &config,
                    );
                    // Fresh install → last_clean_shutdown_at = UNIX_EPOCH
                    // ("we're running, next boot is unclean unless graceful
                    // shutdown says otherwise"). Same treatment as every
                    // other runtime write : only the shutdown path bumps
                    // this field to `now`.
                    let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                        last_popup: lp,
                        paused_secs: 0,
                        popup_pending: false,
                        last_heartbeat: now,
                        last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                        daily_pause_fired_at: None,
                        daily_pause_fired_paliers_bits: 0,
                        daily_pause_skipped_today: None,
                        popup_pid: None,
                    });
                    tracing::info!(
                        path = "no-runtime-dat",
                        seeded_last_popup = ?lp,
                        "cold-start: fresh install or migrated 0.6.x"
                    );
                    (lp, 0u64, false, false)
                }
                Some(state) => {
                    // Carry daily-pause v4 fields through to the tick loop.
                    daily_pause_fired_at = state.daily_pause_fired_at;
                    daily_pause_fired_paliers_bits = state.daily_pause_fired_paliers_bits;
                    daily_pause_skipped_today = state.daily_pause_skipped_today;
                    popup_pid = state.popup_pid;
                    let downtime_secs = now
                        .duration_since(state.last_heartbeat)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    // `was_clean_shutdown` : true when the previous
                    // graceful shutdown stamped last_clean_shutdown_at
                    // at or after the final heartbeat. Migrated formats
                    // populate the field with UNIX_EPOCH → always <
                    // heartbeat → treated as unclean (converges after
                    // one clean boot cycle). Fed into resolve_cold_start
                    // to discriminate the anti-bypass path (unclean +
                    // pending, kid killed service) from the relaunch
                    // path (clean + pending, user cleanly rebooted PC
                    // while a popup was displayed).
                    let was_clean_shutdown =
                        state.last_clean_shutdown_at >= state.last_heartbeat;
                    // Was the interrupted popup the daily lunch pause? Its
                    // long duration must drive the reset/relaunch decision
                    // (else a short reboot mid-midi full-resets it).
                    let is_daily_popup = state.popup_pending
                        && daily_popup_was_interrupted(&config, state.daily_pause_fired_at);
                    let d = scheduler::resolve_cold_start(
                        scheduler::ResolveInputs {
                            now,
                            last_popup: Some(state.last_popup),
                            first_install_at: config.first_install_at(),
                            is_reload: false,
                            paused_secs: state.paused_secs,
                            popup_pending: state.popup_pending,
                            last_heartbeat: state.last_heartbeat,
                            was_clean_shutdown,
                        },
                        &config,
                        is_daily_popup,
                    );
                    // Rule identification for bug-report clarity.
                    // Discriminants :
                    //   - relaunch_now = true → relaunch branch
                    //     (clean + pending).
                    //   - full_reset shape + elapsed >= threshold →
                    //     threshold branch (short-circuits inside
                    //     resolve_cold_start).
                    //   - full_reset shape otherwise → anti-bypass
                    //     (unclean + pending).
                    //   - else → credit-downtime.
                    let full_reset =
                        d.last_popup >= now && d.paused_secs == 0 && !d.popup_pending;
                    let threshold_secs =
                        (f64::from(config.reset_threshold_hours) * 3600.0) as u64;
                    let elapsed_secs = now
                        .duration_since(state.last_popup)
                        .unwrap_or(Duration::ZERO)
                        .as_secs();
                    let rule = if d.relaunch_now {
                        "relaunch (popup_pending on clean shutdown, user rebooted mid-popup, refiring)"
                    } else if full_reset && elapsed_secs >= threshold_secs {
                        "threshold (elapsed >= reset_threshold_hours, full reset)"
                    } else if full_reset {
                        "anti-bypass (popup_pending on unclean shutdown, full reset)"
                    } else {
                        "credit (keep cycle, absorb downtime into paused_secs)"
                    };
                    tracing::info!(
                        path = "with-runtime-dat",
                        rule,
                        downtime_secs,
                        was_clean_shutdown,
                        prior_last_popup = ?state.last_popup,
                        prior_paused_secs = state.paused_secs,
                        prior_popup_pending = state.popup_pending,
                        new_last_popup = ?d.last_popup,
                        new_paused_secs = d.paused_secs,
                        new_popup_pending = d.popup_pending,
                        relaunch_now = d.relaunch_now,
                        "cold-start: threshold / anti-bypass / relaunch / credit dispatch"
                    );
                    // Immediately persist last_clean_shutdown_at = UNIX_EPOCH
                    // so any crash from here until the next graceful shutdown
                    // leaves the field zeroed. Uses the post-cold-start
                    // resolved values (not the raw file contents) so the
                    // next boot reads a coherent state.
                    let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                        last_popup: d.last_popup,
                        paused_secs: d.paused_secs,
                        popup_pending: d.popup_pending,
                        last_heartbeat: now,
                        last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                        daily_pause_fired_at,
                        daily_pause_fired_paliers_bits,
                        daily_pause_skipped_today,
                        popup_pid,
                    });
                    (d.last_popup, d.paused_secs, d.popup_pending, d.relaunch_now)
                }
            };
        let mut last_heartbeat = now;
        let mut fired = scheduler::derive_already_fired(now, &config, last_popup, paused_secs);
        tracing::info!(
            initial_paliers_fired = ?fired,
            paused_secs,
            popup_pending,
            relaunch_now,
            "scheduler initialised"
        );

        // Background threads.
        let cancel_event = ipc::create_cancel_event().context("create cancel event")?;
        let ipc_tx = tx.clone();
        let sendable = ipc::SendableHandle(cancel_event);
        let ipc_tx_map = ipc_tx;
        // Secondary channel dedicated to input-stamp pushes from the
        // pipe thread. Kept out of the main TickCommand mpsc so the
        // ~5 s stamp cadence (matching TICK_INTERVAL) cannot starve
        // the timeout branch that drives the actual tick logic — the
        // tick loop drains this at the start of each timeout tick.
        let (stamp_tx, stamp_rx) = mpsc::channel::<idle::InputStampState>();
        let pipe_thread = thread::spawn(move || run_pipe(sendable, ipc_tx_map, stamp_tx));

        let notify_running_flag = Arc::new(AtomicBool::new(true));
        let notify_thread = spawn_notify(tx.clone(), notify_running_flag.clone());

        // Remote LAN log console. Read-only HTTP/1.1 + SSE (src/remote), bound
        // to 0.0.0.0:8787, reachable at http://<host>.local:8787/. Gated on the
        // config flag, read once here — toggling it takes effect at the next
        // service start (the blocking accept loop is never torn down live). The
        // pause button pushes TickCommand::Trigger through the same channel the
        // pipe uses, keeping TickCommand private to this module.
        if config.remote_logs_enabled {
            let cfg_path = state_path.clone();
            let log_dirs = {
                let logs = env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(logs_dir))
                    .unwrap_or_default();
                LogDirs {
                    diagnostics: PathBuf::from(DIAGNOSTICS_CACHE_DIR),
                    logs,
                }
            };
            let host = crate::remote::lan_host_port();
            let trigger = {
                let tx = tx.clone();
                move || {
                    let _ = tx.send(TickCommand::Trigger);
                }
            };
            let reset = {
                let tx = tx.clone();
                move || {
                    let _ = tx.send(TickCommand::ResetPause);
                }
            };
            thread::spawn(move || {
                if let Err(e) = crate::remote::server(cfg_path, log_dirs, host, trigger, reset) {
                    tracing::error!(error = %e, "remote log server exited");
                }
            });
        }

        // Background wall-time sync loop. NTP UDP first, HTTPS Date-header
        // fallback. On success sleep 6h ; on failure sleep 10min (short
        // cadence until first success recovers offline networks quickly).
        // Fire-and-forget — the service lifetime IS the process lifetime,
        // no JoinHandle to reap. Retained references to `config` are not
        // safe to pass here (the tick loop mutates it on Reload), so the
        // sync fns internally read only the SYNCED static ; they don't
        // touch Config.
        thread::spawn(|| loop {
            let synced = time_authority::sync_ntp().is_ok()
                || time_authority::sync_https().is_ok();
            let sleep_dur = if synced {
                Duration::from_secs(6 * 3600)
            } else {
                Duration::from_secs(10 * 60)
            };
            thread::sleep(sleep_dur);
        });

        // Suspend / resume broadcast listener. Guard is held for the
        // lifetime of run_service ; Drop tears down the pump thread and
        // unregisters the notification.
        let (power_tx, power_rx) = mpsc::channel::<power_notify::PowerEvent>();
        let _power_guard =
            power_notify::install(power_tx).context("power_notify::install")?;

        // Track spawned children so we can TerminateProcess a stale
        // predecessor before spawning a replacement — the post-countdown
        // unlock (0.7.1 Track A) lets a user Alt+Tab away from a still-
        // running popup, so two popups would otherwise stack up on the
        // next schedule tick. Handle-based (not PID-based) so a recycled
        // PID cannot cause us to terminate an unrelated process.
        let mut last_popup_child: Option<SpawnedChild> = None;
        let mut last_widget_children: HashMap<u32, SpawnedChild> = HashMap::new();

        // User-session input probe. Spawned once here so the pipe
        // client is ready before the first idle::idle_seconds() call
        // in the tick loop. `idle_seconds` returns 0 (assume active)
        // if no stamp has arrived yet, so the timing window is safe.
        // Respawned by the tick loop if it dies. See modes::input_probe
        // for the Session 0 rationale.
        let mut input_probe_child: Option<SpawnedChild> = None;
        match spawn_input_probe(&exe) {
            Ok(child) => {
                tracing::info!(pid = child.pid, "input-probe spawned");
                input_probe_child = Some(child);
            }
            Err(e) => {
                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                    tracing::info!(
                        error = %e,
                        "input-probe initial spawn deferred - user session not ready yet, will retry on next tick"
                    );
                } else {
                    tracing::warn!(
                        error = ?e,
                        "input-probe initial spawn failed - will retry on next tick"
                    );
                }
            }
        }

        // Was the interrupted popup the daily lunch pause? If so, the Cas
        // D block below owns its respawn and the interval relaunch further
        // down must be skipped — otherwise it spawn_replacing's the
        // resumed midi popup with an interval one (reboot-mid-midi showed
        // the classic popup + no midi after, because the interval fire
        // reset last_popup and daily_pause_fired_at stayed set).
        let was_daily_popup = popup_pending
            && daily_popup_was_interrupted(&config, daily_pause_fired_at);

        // Cas D respawn : same-day daily pause was up when the service
        // died. If the popup process is still alive → skip (Cas D.4). If
        // dead → respawn with `--elapsed-seconds=<N>` so the dismiss
        // countdown resumes at the right offset (clamped at
        // duration_minutes → dismiss immediately active).
        if popup_pending {
            if let Some(fired_at) = daily_pause_fired_at {
                let now_utc = time_authority::now_authoritative(&config);
                let now_local = time_authority::in_configured_tz(&config, now_utc);
                let fired_local = time_authority::in_configured_tz(&config, fired_at);
                if fired_local.date_naive() == now_local.date_naive() {
                    let alive = popup_pid.map(is_pid_alive).unwrap_or(false);
                    if alive {
                        tracing::info!(
                            pid = popup_pid,
                            "Cas D.4: daily-pause popup still alive after service restart, skipping respawn"
                        );
                    } else {
                        let duration_secs =
                            config.daily_pause.duration_minutes as u64 * 60;
                        let raw_elapsed = now_utc
                            .duration_since(fired_at)
                            .unwrap_or(Duration::ZERO)
                            .as_secs();
                        let elapsed_secs = super::clamp_cas_d_elapsed(raw_elapsed, duration_secs);
                        tracing::info!(
                            elapsed_secs,
                            duration_secs,
                            "Cas D: daily-pause popup dead, respawning with elapsed pre-decrement"
                        );
                        match respawn_daily_popup_cold_start(
                            &exe,
                            elapsed_secs,
                            &config,
                            &mut last_popup_child,
                        ) {
                            Ok(pid) => {
                                popup_pid = Some(pid);
                            }
                            Err(e) => {
                                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                                    tracing::info!(
                                        error = %e,
                                        "Cas D respawn deferred - user session not ready, tick loop will retry naturally"
                                    );
                                } else {
                                    tracing::warn!(
                                        error = ?e,
                                        "Cas D respawn failed - popup_pending stays true, next tick may retry"
                                    );
                                }
                            }
                        }
                    }
                }
                // Else : fired_at is from a prior day. Tick-loop day-boundary
                // reset will detect and clear the flags on next iteration.
            }
        }

        // Relaunch path : runtime.dat said popup_pending && the
        // previous shutdown was clean. User rebooted the PC mid-popup ;
        // refire it now so they pick up where they left off. Anti-bypass
        // (unclean shutdown) does NOT reach this block — that path
        // returns relaunch_now = false and full-reset shape.
        //
        // On cold boot the user session usually isn't ready yet (~5 s
        // after service start), so this initial attempt often fails
        // with a session-not-ready error. `popup_pending` stays true
        // in that case and the tick loop's cold-start-retry branch
        // (step 2c) drives the actual refire once the OS finishes the
        // logon sequence.
        if relaunch_now && !was_daily_popup {
            tracing::info!("cold-start relaunch: refiring popup for user rebooted mid-popup");
            if let Err(e) = fire_popup(
                &exe,
                &mut last_popup,
                &mut paused_secs,
                &mut popup_pending,
                &mut last_heartbeat,
                &mut fired,
                &mut last_popup_child,
                &mut last_widget_children,
                &config.passcode_hash,
                daily_pause_fired_at,
                daily_pause_fired_paliers_bits,
                daily_pause_skipped_today,
                popup_pid,
            ) {
                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                    tracing::info!(
                        error = %e,
                        "cold-start relaunch deferred - user session not ready yet, tick loop will retry"
                    );
                } else {
                    tracing::warn!(error = ?e, "cold-start relaunch failed - tick loop will retry");
                }
            }
        }

        // Popup crash retry state (in-memory only). See PopupRetryState
        // docstring for why it isn't persisted.
        let mut popup_retry: Option<PopupRetryState> = None;

        // Main tick loop.
        let loop_result = tick_loop(
            &mut config,
            &mut last_popup,
            &mut paused_secs,
            &mut popup_pending,
            &mut popup_retry,
            &mut last_heartbeat,
            &mut fired,
            &mut last_popup_child,
            &mut last_widget_children,
            &mut input_probe_child,
            &mut daily_pause_fired_at,
            &mut daily_pause_fired_paliers_bits,
            &mut daily_pause_skipped_today,
            &mut popup_pid,
            &exe,
            &rx,
            stamp_rx,
            power_rx,
        );

        // Signal shutdown.
        notify_running_flag.store(false, Ordering::Relaxed);
        unsafe { let _ = SetEvent(cancel_event); }

        // Terminate the input probe explicitly. Without this the old
        // probe would linger until user log-off and hold onto its pipe
        // client + process handle across the next service restart.
        // HANDLE-based terminate is immune to PID recycle.
        if let Some(probe) = input_probe_child.take() {
            probe.terminate();
        }

        // Graceful-shutdown marker : stamp `last_clean_shutdown_at`
        // with the current time so the next boot can distinguish this
        // clean SCM Stop from a force-kill. Only path in the codebase
        // that writes a non-UNIX_EPOCH value into this field. If write
        // fails, next boot treats it as unclean (safe default) — logged
        // but not fatal.
        let shutdown_now = SystemTime::now();
        if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
            last_popup,
            paused_secs,
            popup_pending,
            last_heartbeat: shutdown_now,
            last_clean_shutdown_at: shutdown_now,
            daily_pause_fired_at,
            daily_pause_fired_paliers_bits,
            daily_pause_skipped_today,
            popup_pid,
        }) {
            tracing::warn!(
                error = ?e,
                "final clean-shutdown runtime.dat write failed - next boot will treat as unclean"
            );
        } else {
            tracing::info!(
                last_clean_shutdown_at = ?shutdown_now,
                "graceful shutdown : runtime.dat stamped clean"
            );
        }

        // Grace period + best-effort join. If a thread refuses to exit,
        // process shutdown will terminate it — no forced-kill from Rust.
        thread::sleep(SHUTDOWN_GRACE);
        let _ = pipe_thread.join();
        let _ = notify_thread.join();
        unsafe { let _ = CloseHandle(cancel_event); }

        let exit_code = if loop_result.is_err() { 1 } else { 0 };
        if let Err(e) = status_handle.set_service_status(stopped(exit_code)) {
            tracing::warn!(error = ?e, "SetServiceStatus(Stopped) failed");
        }
        loop_result
    }

    fn on_control_event(
        ctl: ServiceControl,
        tx: &mpsc::Sender<TickCommand>,
    ) -> ServiceControlHandlerResult {
        match ctl {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = tx.send(TickCommand::Shutdown);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn tick_loop(
        config: &mut Config,
        last_popup: &mut SystemTime,
        paused_secs: &mut u64,
        popup_pending: &mut bool,
        popup_retry: &mut Option<PopupRetryState>,
        last_heartbeat: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        input_probe_child: &mut Option<SpawnedChild>,
        daily_pause_fired_at: &mut Option<SystemTime>,
        daily_pause_fired_paliers_bits: &mut u8,
        daily_pause_skipped_today: &mut Option<chrono::NaiveDate>,
        popup_pid: &mut Option<u32>,
        exe: &Path,
        rx: &mpsc::Receiver<TickCommand>,
        stamp_rx: mpsc::Receiver<idle::InputStampState>,
        power_rx: mpsc::Receiver<power_notify::PowerEvent>,
    ) -> Result<()> {
        // 60 s idle threshold. Toggled at runtime via
        // `config.count_active_time` — when false, the check below is
        // short-circuited to `is_idle_now = false` so the counter runs
        // on wall-clock time.
        const IDLE_THRESHOLD_SECS: u64 = 60;
        // Heartbeat cadence : every 6th timeout tick (~30 s at 5 s
        // TICK_INTERVAL). Fire-path already persists eagerly ; this
        // covers pure-idle stretches so cold-boot downtime detection
        // resolves at ≤30 s granularity.
        const HEARTBEAT_EVERY_N_TICKS: u32 = 6;
        // Periodic idle-state log cadence : every 12th timeout tick
        // (~60 s at 5 s TICK_INTERVAL). Emits a `state snapshot` log
        // line with all signals + counters even when no active↔inactive
        // transition fired, so log-scraping can plot the paused_secs
        // curve minute-by-minute. Bounded — one line per minute is ~1440
        // lines/day, cheap compared to the 5-tick heartbeat.
        const STATE_LOG_EVERY_N_TICKS: u32 = 12;
        // Grace for the popup's DailyAck after a daily UpdateContent
        // write. The ack normally arrives within milliseconds ; with the
        // 5 s tick cadence, 3 s means "no ack by the next tick" — then
        // the unconfirmed update falls back to kill/respawn.
        const DAILY_ACK_TIMEOUT: Duration = Duration::from_secs(3);

        let tick_delta_secs = TICK_INTERVAL.as_secs();
        let mut suspended_at: Option<SystemTime> = None;
        let mut ticks_since_write: u32 = 0;
        let mut ticks_since_state_log: u32 = 0;
        // Anchors for the clock-shift detector : compare wall delta
        // against unbiased-interrupt-time delta each tick. When they
        // diverge ≥ CLOCK_JUMP_THRESHOLD we shift `last_popup` by the
        // divergence — neutralises VM freeze, S4 hibernate without
        // power broadcast, and admin/NTP clock tampering. `None` on
        // the first tick (no baseline to compare yet).
        let mut prev_wall: Option<SystemTime> = None;
        let mut prev_unbiased_secs: Option<u64> = None;
        // `was_idle = None` means "never observed yet" — the first tick's
        // idle transition log will read as a seed. After that, every
        // active↔inactive flip emits a single log line with the three
        // component signals so bug reports show why the counter did or
        // didn't advance.
        let mut was_idle: Option<bool> = None;
        // Latest input stamp materialized from the pipe transport. Held
        // between ticks so a probe that stops pushing decays into
        // staleness (`idle::idle_seconds` returns 0 after 30 s of no
        // new stamps) rather than snapping back to "assume active"
        // instantly.
        let mut input_stamp: Option<idle::InputStampState> = None;
        // Daily-pause decision trace de-dup : the tuple of decision
        // inputs/outcome is logged only when it CHANGES, so the daily
        // branch leaves a diagnosable trail (window opened, suppressed
        // by fired_today, collision wanted, …) without a 5 s-cadence
        // spam. `None` = nothing observed yet, first tick seeds it.
        let mut last_daily_trace: Option<(bool, bool, bool, bool, bool, bool)> = None;
        // In-flight daily UpdateContent : Some(...) between the pipe
        // write and the popup's DailyAck (or its deadline). The stamp
        // happens on the ack, never on the bare write.
        let mut pending_daily_ack: Option<super::PendingDailyAck> = None;

        loop {
            match rx.recv_timeout(TICK_INTERVAL) {
                Ok(TickCommand::Shutdown) => {
                    tracing::info!("tick loop received Shutdown");
                    return Ok(());
                }
                Ok(TickCommand::Trigger) => {
                    tracing::info!("TriggerPopupNow received");
                    if let Err(e) = fire_popup(
                        exe,
                        last_popup,
                        paused_secs,
                        popup_pending,
                        last_heartbeat,
                        fired,
                        last_popup_child,
                        last_widget_children,
                        &config.passcode_hash,
                        *daily_pause_fired_at,
                        *daily_pause_fired_paliers_bits,
                        *daily_pause_skipped_today,
                        *popup_pid,
                    ) {
                        tracing::warn!(error = ?e, "trigger popup failed");
                    }
                }
                Ok(TickCommand::RearmDaily) => {
                    tracing::info!(
                        "RearmDailyPause received - clearing today's daily-pause fired marker"
                    );
                    *daily_pause_fired_at = None;
                    *daily_pause_fired_paliers_bits = 0;
                    *daily_pause_skipped_today = None;
                    if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
                        last_popup: *last_popup,
                        paused_secs: *paused_secs,
                        popup_pending: *popup_pending,
                        last_heartbeat: *last_heartbeat,
                        last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                        daily_pause_fired_at: None,
                        daily_pause_fired_paliers_bits: 0,
                        daily_pause_skipped_today: None,
                        popup_pid: *popup_pid,
                    }) {
                        tracing::warn!(error = ?e, "write runtime.dat failed (rearm daily)");
                    }
                }
                Ok(TickCommand::ResetPause) => {
                    tracing::info!("ResetPause received - resetting interval, closing popup, dismissing heads-up");
                    // 1. Reset the interval cycle WITHOUT showing a popup :
                    // the fire_popup mutations minus the spawn and minus
                    // popup_pending = true. The daily-pause bookkeeping is
                    // deliberately left untouched (Reset never marks it done).
                    *last_popup = SystemTime::now();
                    *paused_secs = 0;
                    fired.clear();
                    // 2. Close any open popup : prefer a clean Dismiss (exit 0),
                    // fall back to terminate (also exit 0). Either way the
                    // exit-0 detector reconciles popup_pending on the next tick.
                    if *popup_pending {
                        if let Some(child) = last_popup_child.as_ref() {
                            if crate::ipc_popup::send_to_popup(
                                child.pid,
                                &crate::ipc_popup::PopupMessage::Dismiss,
                            )
                            .is_err()
                            {
                                child.terminate();
                            }
                        }
                    }
                    // 3. Dismiss any heads-up (countdown) widget.
                    drain_heads_up_widgets(last_widget_children);
                    // 4. Persist. Preserve the daily-pause fields : a Reset that
                    // closes an in-progress *daily* popup does NOT mark it done,
                    // so the daily branch may re-fire (matches "just reset").
                    if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
                        last_popup: *last_popup,
                        paused_secs: *paused_secs,
                        popup_pending: *popup_pending,
                        last_heartbeat: *last_heartbeat,
                        last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                        daily_pause_fired_at: *daily_pause_fired_at,
                        daily_pause_fired_paliers_bits: *daily_pause_fired_paliers_bits,
                        daily_pause_skipped_today: *daily_pause_skipped_today,
                        popup_pid: *popup_pid,
                    }) {
                        tracing::warn!(error = ?e, "write runtime.dat failed (reset pause)");
                    }
                    // 5. Drop any in-flight daily ack : Reset never marks
                    // the daily done, so a late ack from the popup being
                    // closed must not stamp it either. The daily branch
                    // re-fires on its own once the popup is gone.
                    if pending_daily_ack.take().is_some() {
                        tracing::info!("reset: dropped in-flight daily ack");
                    }
                }
                Ok(TickCommand::DailyAck { pid }) => {
                    match pending_daily_ack.take() {
                        Some(p) if p.pid == pid => {
                            let alive = last_popup_child
                                .as_ref()
                                .filter(|c| c.try_wait_exited().is_none())
                                .map(|c| c.pid);
                            if alive == Some(pid) {
                                let now = time_authority::now_authoritative(config);
                                tracing::info!(
                                    pid,
                                    "daily ack received - popup confirmed daily content, stamping fire"
                                );
                                stamp_daily_fire_success(
                                    last_popup,
                                    paused_secs,
                                    popup_pending,
                                    last_heartbeat,
                                    fired,
                                    daily_pause_fired_at,
                                    daily_pause_fired_paliers_bits,
                                    *daily_pause_skipped_today,
                                    popup_pid,
                                    pid,
                                    now,
                                    u64::from(config.daily_pause.duration_minutes) * 60,
                                );
                                // Daily content is confirmed on screen :
                                // drain any lingering heads-up widget.
                                drain_heads_up_widgets(last_widget_children);
                            } else {
                                // Applied, then the popup died before the
                                // ack was processed. Don't stamp against a
                                // dead child — the no-popup daily path
                                // re-fires a fresh daily popup instead.
                                tracing::warn!(
                                    pid,
                                    "daily ack received but popup already gone - not stamping"
                                );
                            }
                        }
                        Some(p) => {
                            tracing::warn!(
                                ack_pid = pid,
                                expected_pid = p.pid,
                                "daily ack pid mismatch - ignoring stale ack"
                            );
                            pending_daily_ack = Some(p);
                        }
                        None => {
                            tracing::warn!(pid, "unsolicited daily ack - ignoring");
                        }
                    }
                }
                Ok(TickCommand::Reload) => {
                    tracing::info!("config reload triggered");
                    let state_path = PathBuf::from(STATE_DAT);
                    let old_daily = config.daily_pause.clone();
                    let old_interval_hours = config.interval_hours;
                    *config = config::load_or_default(&state_path);
                    tracing::info!(
                        count_active_time = config.count_active_time,
                        "reload: active-time accounting effective mode"
                    );
                    let now = SystemTime::now();
                    // A changed general-pause interval restarts the cycle
                    // from now : the user just set "every N", so the next
                    // popup must fire exactly N after Save — not N offset
                    // by the old last_popup + banked paused_secs, which
                    // fires early/late and reads as a bug. Reset both so
                    // the deadline (last_popup + interval + paused_secs)
                    // collapses to now + interval.
                    if config.interval_hours != old_interval_hours {
                        tracing::info!(
                            old_hours = old_interval_hours,
                            new_hours = config.interval_hours,
                            "reload: interval changed - restarting cycle (last_popup=now, paused_secs=0)",
                        );
                        *last_popup = now;
                        *paused_secs = 0;
                        *last_heartbeat = now;
                        let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                            last_popup: *last_popup,
                            paused_secs: 0,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
                            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                            daily_pause_fired_at: *daily_pause_fired_at,
                            daily_pause_fired_paliers_bits: *daily_pause_fired_paliers_bits,
                            daily_pause_skipped_today: *daily_pause_skipped_today,
                            popup_pid: *popup_pid,
                        });
                    }
                    // If the user reduced interval_hours enough that
                    // `last_popup + new_interval` lands in the past
                    // (or within RELOAD_MIN_GRACE), bump last_popup
                    // forward so the next popup fires at now+2min
                    // rather than instantly on Save. No-op right after an
                    // interval-change reset above (last_popup is now).
                    let clamped = scheduler::resolve_last_popup(
                        scheduler::ResolveInputs {
                            now,
                            last_popup: Some(*last_popup),
                            first_install_at: config.first_install_at(),
                            is_reload: true,
                            paused_secs: *paused_secs,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
                            was_clean_shutdown: false, // unused on reload path
                        },
                        config,
                    );
                    if clamped != *last_popup {
                        tracing::info!(
                            old_last = ?*last_popup,
                            new_last = ?clamped,
                            "reload: interval reduced, clamping last_popup forward",
                        );
                        *last_popup = clamped;
                        *last_heartbeat = now;
                        let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                            last_popup: *last_popup,
                            paused_secs: *paused_secs,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
                            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                            daily_pause_fired_at: *daily_pause_fired_at,
                            daily_pause_fired_paliers_bits: *daily_pause_fired_paliers_bits,
                            daily_pause_skipped_today: *daily_pause_skipped_today,
                            popup_pid: *popup_pid,
                        });
                    }
                    // A moved daily window invalidates the per-day
                    // markers computed against the OLD window : the
                    // skipped marker would block the new time all day
                    // (and, pre-guard-fix, suppress intervals around
                    // it), fired palier bits would swallow the new
                    // pre-notifs. `fired_today` stays — see
                    // daily_window_changed's doc.
                    if super::daily_window_changed(&old_daily, &config.daily_pause) {
                        *daily_pause_skipped_today = None;
                        *daily_pause_fired_paliers_bits = 0;
                        let now_auth = time_authority::now_authoritative(config);
                        let now_local_new =
                            time_authority::in_configured_tz(config, now_auth);
                        let new_fire_at =
                            build_daily_pause_state(config, now_local_new, false, 0, None)
                                .fire_at_today;
                        if super::daily_rearm_on_window_change(
                            *daily_pause_fired_at,
                            new_fire_at,
                            now_local_new,
                        ) {
                            *daily_pause_fired_at = None;
                            tracing::info!(
                                "reload: daily-pause window moved to a future time - re-arming today's fire (fired-at cleared)"
                            );
                        } else {
                            tracing::info!(
                                "reload: daily-pause window changed - clearing skipped marker + palier bits (fired-today untouched)"
                            );
                        }
                        let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                            last_popup: *last_popup,
                            paused_secs: *paused_secs,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
                            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                            daily_pause_fired_at: *daily_pause_fired_at,
                            daily_pause_fired_paliers_bits: 0,
                            daily_pause_skipped_today: None,
                            popup_pid: *popup_pid,
                        });
                    }
                    *fired = scheduler::derive_already_fired(
                        now,
                        config,
                        *last_popup,
                        *paused_secs,
                    );
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // 1. Drain power events. Suspend timestamps are
                    //    captured wall-clock inside the pump thread's
                    //    wndproc, so the delta is accurate regardless
                    //    of receive latency.
                    while let Ok(evt) = power_rx.try_recv() {
                        match evt {
                            power_notify::PowerEvent::Suspend(ts) => {
                                suspended_at = Some(ts);
                            }
                            power_notify::PowerEvent::Resume(ts) => {
                                if let Some(sus) = suspended_at.take() {
                                    if let Ok(delta) = ts.duration_since(sus) {
                                        // Log only. Gap compensation is owned by
                                        // the clock-shift detector below (1b) —
                                        // wall vs unbiased time diverges by the
                                        // sleep too, so absorbing it here as well
                                        // double-counted the gap (popup pushed by
                                        // 2× the sleep, countdown above interval).
                                        tracing::info!(
                                            sleep_secs = delta.as_secs(),
                                            "power resume: gap left to the clock-shift detector"
                                        );
                                    }
                                } else {
                                    tracing::info!(
                                        "power resume without matching suspend (service was down at suspend, cold-start absorbed downtime)"
                                    );
                                }
                            }
                        }
                    }

                    // 1b. Clock-shift detector. Sample wall + unbiased
                    //     BEFORE the fire decision so a jump this tick
                    //     is compensated before `next_event` reads
                    //     `last_popup`. Skipped on the first tick (no
                    //     baseline yet).
                    let now_wall = SystemTime::now();
                    let now_unbiased_secs = crate::platform::win32::unbiased_time::ticks_to_secs(
                        crate::platform::win32::unbiased_time::query_100ns(),
                    );
                    if let (Some(pw), Some(pu)) = (prev_wall, prev_unbiased_secs) {
                        let unbiased_delta_secs = now_unbiased_secs.saturating_sub(pu);
                        if let Some(divergence) = scheduler::compute_clock_shift(
                            pw,
                            now_wall,
                            unbiased_delta_secs,
                            CLOCK_JUMP_THRESHOLD.as_secs(),
                        ) {
                            let wall_delta_secs = now_wall
                                .duration_since(pw)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or_else(|e| -(e.duration().as_secs() as i64));
                            let cause_hint = if divergence < 0 {
                                "clock_rewind"
                            } else if divergence >= 5 * 60 {
                                "suspend_or_hibernate"
                            } else {
                                "clock_advanced"
                            };
                            let popup_duration_secs =
                                u64::from(config.duration_minutes).saturating_mul(60);
                            if divergence > 0
                                && scheduler::suspend_gap_resets_cycle(
                                    divergence as u64,
                                    popup_duration_secs,
                                )
                            {
                                // Absence covered the break — same rule as the
                                // cold-start path : fresh cycle from wake, the
                                // gap is the pause. (A positive divergence from
                                // a mere clock-advance also lands here : the
                                // anti-cheat concern is rewinds, and a forward
                                // jump ≥ the popup duration is indistinguishable
                                // from a suspend on VMs anyway.)
                                *last_popup = now_wall;
                                *paused_secs = 0;
                                fired.clear();
                                tracing::warn!(
                                    divergence_secs = divergence,
                                    wall_delta_secs,
                                    unbiased_delta_secs,
                                    cause_hint,
                                    "clock jump >= popup duration - break taken, cycle reset at wake"
                                );
                                let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                                    last_popup: *last_popup,
                                    paused_secs: 0,
                                    popup_pending: *popup_pending,
                                    last_heartbeat: *last_heartbeat,
                                    last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                                    daily_pause_fired_at: *daily_pause_fired_at,
                                    daily_pause_fired_paliers_bits:
                                        *daily_pause_fired_paliers_bits,
                                    daily_pause_skipped_today: *daily_pause_skipped_today,
                                    popup_pid: *popup_pid,
                                });
                            } else {
                                let last_popup_before = *last_popup;
                                *last_popup =
                                    scheduler::shift_systemtime(*last_popup, divergence);
                                tracing::warn!(
                                    divergence_secs = divergence,
                                    wall_delta_secs,
                                    unbiased_delta_secs,
                                    cause_hint,
                                    last_popup_before = ?last_popup_before,
                                    last_popup_after = ?*last_popup,
                                    "clock jump detected, last_popup shifted to preserve active-time"
                                );
                            }
                        }
                    }
                    prev_wall = Some(now_wall);
                    prev_unbiased_secs = Some(now_unbiased_secs);

                    // 2. Detect popup exit (Dismiss button OR the
                    //    countdown-then-close path both terminate the
                    //    popup process). The exit code discriminates a
                    //    normal dismiss (0) from a crash (≠ 0). Crash
                    //    arms a retry backoff instead of clearing the
                    //    cycle ; a following FirePopup attempt uses the
                    //    popup-side wipe-then-retry path in popup.rs to
                    //    recover from a stale WebView2 UserData folder.
                    if *popup_pending {
                        if let Some(child) = last_popup_child.as_ref() {
                            if let Some(exit_code) = child.try_wait_exited() {
                                if exit_code == 0 {
                                    *popup_pending = false;
                                    *popup_retry = None;
                                    // Pause midi has absolute priority : when the
                                    // DAILY popup closes, reset the interval timer
                                    // to NOW so a full interval elapses AFTER lunch
                                    // (the pause itself doesn't count toward it).
                                    // popup_pid is stamped only by fire_popup_daily,
                                    // never by interval fires, so a match on the
                                    // exited child unambiguously identifies the
                                    // daily popup.
                                    if *popup_pid == Some(child.pid) {
                                        let reset_now = SystemTime::now();
                                        *last_popup = reset_now;
                                        *paused_secs = 0;
                                        fired.clear();
                                        *popup_pid = None;
                                        // Persist immediately so a crash before the
                                        // next heartbeat doesn't lose the reset.
                                        let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                                            last_popup: reset_now,
                                            paused_secs: 0,
                                            popup_pending: false,
                                            last_heartbeat: *last_heartbeat,
                                            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                                            daily_pause_fired_at: *daily_pause_fired_at,
                                            daily_pause_fired_paliers_bits:
                                                *daily_pause_fired_paliers_bits,
                                            daily_pause_skipped_today: *daily_pause_skipped_today,
                                            popup_pid: None,
                                        });
                                        tracing::info!(
                                            "daily pause popup closed - interval timer reset to now (lunch over)"
                                        );
                                    }
                                    tracing::info!(
                                        exit_code,
                                        paused_secs = *paused_secs,
                                        "popup child exited cleanly - clearing popup_pending"
                                    );
                                } else {
                                    let attempts_prior =
                                        popup_retry.as_ref().map(|r| r.attempts).unwrap_or(0);
                                    if attempts_prior >= MAX_POPUP_RETRIES {
                                        tracing::warn!(
                                            exit_code,
                                            attempts = attempts_prior,
                                            max_retries = MAX_POPUP_RETRIES,
                                            "popup gave up after N retries - clearing pending, next natural cycle"
                                        );
                                        *popup_pending = false;
                                        *popup_retry = None;
                                        fired.clear();
                                    } else {
                                        let attempts_next = attempts_prior + 1;
                                        let backoff_secs =
                                            RETRY_BACKOFF_STEP_SECS * (attempts_next as u64);
                                        let next_retry_at = SystemTime::now()
                                            + Duration::from_secs(backoff_secs);
                                        tracing::warn!(
                                            exit_code,
                                            attempts = attempts_next,
                                            max_retries = MAX_POPUP_RETRIES,
                                            backoff_secs,
                                            "popup exited abnormally - retry armed"
                                        );
                                        *popup_retry = Some(PopupRetryState {
                                            attempts: attempts_next,
                                            next_retry_at,
                                        });
                                        // Fire-set carries per-palier "already announced"
                                        // state ; clear it so the next attempted popup
                                        // re-considers the schedule cleanly.
                                        fired.clear();
                                    }
                                }
                            }
                        }
                    }

                    // 2a. If a retry is armed and its backoff has
                    //     elapsed, re-attempt fire_popup. Successful
                    //     spawn resets popup_pending/last_popup via the
                    //     fire path ; a subsequent crash re-arms with
                    //     the incremented attempt count until MAX.
                    if let Some(retry) = popup_retry.as_ref() {
                        if SystemTime::now() >= retry.next_retry_at {
                            tracing::info!(
                                attempts = retry.attempts,
                                "popup retry backoff elapsed - re-firing"
                            );
                            if let Err(e) = fire_popup(
                                exe,
                                last_popup,
                                paused_secs,
                                popup_pending,
                                last_heartbeat,
                                fired,
                                last_popup_child,
                                last_widget_children,
                                &config.passcode_hash,
                                *daily_pause_fired_at,
                                *daily_pause_fired_paliers_bits,
                                *daily_pause_skipped_today,
                                *popup_pid,
                            ) {
                                tracing::warn!(
                                    error = ?e,
                                    "popup retry fire_popup failed - keeping retry armed"
                                );
                            }
                        }
                    }

                    // 2b. Ensure the user-session input probe is alive.
                    //     Cheap poll (HANDLE-based try_wait_exited). If
                    //     dead, respawn — the probe pushes the stamps
                    //     idle::idle_seconds() / idle::media_playing()
                    //     read, so a stale/absent probe would degrade
                    //     kbd/mouse + media detection.
                    let probe_alive = input_probe_child
                        .as_ref()
                        .map(|c| c.try_wait_exited().is_none())
                        .unwrap_or(false);
                    if !probe_alive {
                        if input_probe_child.is_some() {
                            tracing::warn!("input-probe child exited - respawning");
                        }
                        match spawn_input_probe(exe) {
                            Ok(child) => {
                                tracing::info!(pid = child.pid, "input-probe respawned");
                                *input_probe_child = Some(child);
                            }
                            Err(e) => {
                                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                                    tracing::info!(
                                        error = %e,
                                        "input-probe respawn deferred - user session not ready, will retry on next tick"
                                    );
                                } else {
                                    tracing::warn!(
                                        error = ?e,
                                        "input-probe respawn failed - will retry on next tick"
                                    );
                                }
                            }
                        }
                    }

                    // 2c. Cold-start relaunch retry. `popup_pending`
                    //     came out of runtime.dat set to `true` and the
                    //     resolve_cold_start "relaunch" branch fired
                    //     (see the pre-loop attempt), but the initial
                    //     spawn either wasn't attempted (relaunch_now
                    //     was false because the pre-loop already
                    //     handled it) or failed because the user
                    //     session wasn't ready. `last_popup_child.is_none()`
                    //     uniquely identifies "never spawned" here
                    //     (crash-retry path always leaves the exited
                    //     child in `last_popup_child`). Retry each tick
                    //     until spawn succeeds.
                    if *popup_pending && last_popup_child.is_none() {
                        // A daily lunch pause interrupted by a service
                        // death resumes as the MIDI popup — with the
                        // elapsed offset so the dismiss timer picks up
                        // where it left off (15 min into a 30 min pause →
                        // 15 min left) — never as an interval popup.
                        if daily_popup_was_interrupted(config, *daily_pause_fired_at) {
                            let fired_at =
                                daily_pause_fired_at.expect("was_interrupted implies Some");
                            let now_utc = time_authority::now_authoritative(config);
                            let duration_secs =
                                config.daily_pause.duration_minutes as u64 * 60;
                            let raw_elapsed = now_utc
                                .duration_since(fired_at)
                                .unwrap_or(Duration::ZERO)
                                .as_secs();
                            let elapsed_secs =
                                super::clamp_cas_d_elapsed(raw_elapsed, duration_secs);
                            match respawn_daily_popup_cold_start(
                                exe,
                                elapsed_secs,
                                config,
                                last_popup_child,
                            ) {
                                Ok(pid) => {
                                    *popup_pid = Some(pid);
                                }
                                Err(e) => {
                                    if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                                        tracing::info!(
                                            error = %e,
                                            "daily cold-start respawn retry deferred - user session not ready"
                                        );
                                    } else {
                                        tracing::warn!(
                                            error = ?e,
                                            "daily cold-start respawn retry failed - will retry on next tick"
                                        );
                                    }
                                }
                            }
                        } else if let Err(e) = fire_popup(
                            exe,
                            last_popup,
                            paused_secs,
                            popup_pending,
                            last_heartbeat,
                            fired,
                            last_popup_child,
                            last_widget_children,
                            &config.passcode_hash,
                            *daily_pause_fired_at,
                            *daily_pause_fired_paliers_bits,
                            *daily_pause_skipped_today,
                            *popup_pid,
                        ) {
                            if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                                tracing::info!(
                                    error = %e,
                                    "cold-start relaunch retry deferred - user session not ready"
                                );
                            } else {
                                tracing::warn!(
                                    error = ?e,
                                    "cold-start relaunch retry failed - will retry on next tick"
                                );
                            }
                        }
                    }

                    // 3. Idle accumulation. Signals are read once and
                    //    reused by the fire-decision below.
                    let now = SystemTime::now();
                    // The XInput poll must run every tick (stateful
                    // packet-number compare) ; the toggle only gates
                    // whether the result counts.
                    let gamepad_seen =
                        gamepad::gamepad_active_since_last_call() && config.count_gamepad;
                    // Drain any pending input-stamp pushes from the
                    // pipe thread ; keep the newest (stamps are
                    // monotonic). Held state carries across ticks
                    // when the drain sees nothing. The media flags
                    // ride in the same stamp (WASAPI/SMTC are session-
                    // scoped, unreadable from Session 0), so they must
                    // be read after the drain.
                    while let Ok(stamp) = stamp_rx.try_recv() {
                        input_stamp = Some(stamp);
                    }
                    let playing_video_or_audio_now = idle::media_playing(
                        input_stamp.as_ref(),
                        config.count_audio,
                        config.count_media,
                    );
                    let idle_secs = idle::idle_seconds(input_stamp.as_ref());
                    // Hard override : a locked workstation forces inactive
                    // regardless of media/gamepad/idle. Stale/dead probe
                    // reads false (never force-pause on a crashed probe).
                    let locked_now = idle::screen_locked(input_stamp.as_ref());
                    // Kbd/mouse toggle off → input no longer refutes
                    // idleness ; clamp the reading to the threshold so
                    // the pause decision sees it as idle. `idle_secs`
                    // stays raw for the logs.
                    let kbd_idle_secs = if config.count_keyboard_mouse {
                        idle_secs
                    } else {
                        IDLE_THRESHOLD_SECS
                    };
                    // All signals unchecked (hand-edited state.dat —
                    // the UI locks the last one) → wall-clock fallback
                    // rather than a permanently-paused counter.
                    let any_signal = config.count_keyboard_mouse
                        || config.count_gamepad
                        || config.count_audio
                        || config.count_media;
                    let is_idle_now = if config.count_active_time && any_signal {
                        signals::idle_tick_should_pause(
                            kbd_idle_secs,
                            IDLE_THRESHOLD_SECS,
                            gamepad_seen,
                            playing_video_or_audio_now,
                            locked_now,
                        )
                    } else {
                        false
                    };
                    if is_idle_now {
                        *paused_secs += tick_delta_secs;
                    }
                    // Invariant guard : paused time accrues at most 1 s
                    // per elapsed second, so paused_secs can never
                    // exceed the elapsed since last_popup — which also
                    // bounds the popup countdown at the interval. Any
                    // excess is corrupted state (e.g. runtime.dat
                    // written by the pre-1.6.4 double suspend
                    // compensation) ; clamping self-heals it.
                    let elapsed_since_last = now
                        .duration_since(*last_popup)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let clamped_paused =
                        scheduler::clamp_paused_to_elapsed(*paused_secs, elapsed_since_last);
                    if clamped_paused != *paused_secs {
                        // A FUTURE last_popup is the lunch-end anchor set
                        // by stamp_daily_fire_success, not corruption :
                        // elapsed saturates to 0 until lunch end, so idle
                        // seconds accrued during the pause are expected to
                        // clamp away silently (they must not push the
                        // post-lunch interval ; only post-lunch idle
                        // counts).
                        if now >= *last_popup {
                            tracing::warn!(
                                paused_secs = *paused_secs,
                                elapsed_since_last,
                                "paused_secs above elapsed - clamping (corrupted state self-heal)"
                            );
                        }
                        *paused_secs = clamped_paused;
                    }
                    if was_idle != Some(is_idle_now) {
                        tracing::info!(
                            transition = if is_idle_now { "active -> inactive" } else { "inactive -> active" },
                            idle_secs,
                            gamepad_seen,
                            playing_video_or_audio = playing_video_or_audio_now,
                            locked = locked_now,
                            paused_secs = *paused_secs,
                            "idle state changed"
                        );
                        was_idle = Some(is_idle_now);
                    }
                    ticks_since_state_log += 1;
                    if ticks_since_state_log >= STATE_LOG_EVERY_N_TICKS {
                        let next = scheduler::next_popup_at(*last_popup, config, *paused_secs);
                        let remaining_secs =
                            next.duration_since(now).unwrap_or(Duration::ZERO).as_secs();
                        // Categorical reason so the log reader doesn't
                        // have to correlate the raw booleans by eye.
                        // Priority order : the most user-driven signal
                        // wins (kbd/mouse > gamepad > media).
                        let active_reason = if !config.count_active_time || !any_signal {
                            "wall_clock_mode"
                        } else if locked_now {
                            "locked"
                        } else if is_idle_now {
                            "idle"
                        } else if kbd_idle_secs < IDLE_THRESHOLD_SECS {
                            "kbd_or_mouse"
                        } else if gamepad_seen {
                            "gamepad"
                        } else if playing_video_or_audio_now {
                            "playing_video_or_audio"
                        } else {
                            "unknown"
                        };
                        tracing::info!(
                            is_idle = is_idle_now,
                            active_reason,
                            idle_secs,
                            gamepad_seen,
                            playing_video_or_audio = playing_video_or_audio_now,
                            locked = locked_now,
                            paused_secs = *paused_secs,
                            remaining_secs,
                            count_active_time = config.count_active_time,
                            "state snapshot"
                        );
                        ticks_since_state_log = 0;
                    }

                    // 4. Scheduler decision. Simplified in b.7 — popups
                    //    fire the moment the active-time echelon is hit,
                    //    with no busy-context deferral. Alt+pincode on
                    //    the popup is the intended escape hatch for the
                    //    rare "I'm mid-presentation, don't interrupt"
                    //    case.
                    //
                    // Time source : now_authoritative() reads the last
                    // successful NTP/HTTPS sync + a monotonic tick.
                    // Falls back silently to SystemTime::now() when
                    // never synced (offline first boot). now_local
                    // resolves against the config-persisted IANA TZ so
                    // Windows-panel TZ changes don't shift the fire
                    // moment. Both are used ONLY for the daily-pause
                    // decision — the interval branch still consumes
                    // SystemTime::now() (`now`) for scheduler math to
                    // preserve prior semantics.
                    let now_authoritative = time_authority::now_authoritative(config);
                    let now_local = time_authority::in_configured_tz(config, now_authoritative);

                    // Build DailyPauseState from runtime.dat fields, with
                    // day-boundary reset : if the persisted fire timestamp
                    // is from a prior local date, clear the palier bitmask
                    // + skipped-today marker so today's cycle starts
                    // clean. Kill-switch scoping : the scheduler already
                    // gates the daily branch on `enabled` alone (session 1
                    // moved Config::disabled inside the interval branch).
                    let fired_local = daily_pause_fired_at
                        .map(|ts| time_authority::in_configured_tz(config, ts));
                    let mut fired_today =
                        match super::resolve_daily_fired(fired_local, now_local.date_naive()) {
                            super::DailyFiredDecision::DayRolledOver => {
                                // Reset the per-day markers ONCE. Clearing
                                // `daily_pause_fired_at` too is what stops the
                                // reset re-firing every tick (a stale prior-day
                                // timestamp otherwise keeps wiping the palier
                                // de-dup bits, respawning the countdown widget
                                // in a loop). Aligns with RearmDaily's clear.
                                *daily_pause_fired_paliers_bits = 0;
                                *daily_pause_skipped_today = None;
                                *daily_pause_fired_at = None;
                                if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
                                    last_popup: *last_popup,
                                    paused_secs: *paused_secs,
                                    popup_pending: *popup_pending,
                                    last_heartbeat: *last_heartbeat,
                                    last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                                    daily_pause_fired_at: None,
                                    daily_pause_fired_paliers_bits: 0,
                                    daily_pause_skipped_today: None,
                                    popup_pid: *popup_pid,
                                }) {
                                    tracing::warn!(
                                        error = ?e,
                                        "write runtime.dat failed (daily day-boundary reset)"
                                    );
                                }
                                false
                            }
                            super::DailyFiredDecision::FiredToday => true,
                            super::DailyFiredDecision::NotFired => false,
                        };
                    let daily_state = build_daily_pause_state(
                        config,
                        now_local,
                        fired_today,
                        *daily_pause_fired_paliers_bits,
                        *daily_pause_skipped_today,
                    );

                    // 4b. Daily↔interval collision (session 4). While an
                    //     interval popup is up, the scheduler's
                    //     popup_pending guard defers FireDailyPause until
                    //     dismissal ; instead of waiting, mutate the
                    //     RUNNING popup's DOM over its per-PID content
                    //     pipe (PID stable, no WebView2 flash). Pipe
                    //     unreachable → fall back to kill+respawn with
                    //     the exact fire_popup_daily argv/env. Note : an
                    //     updated (formerly interval) popup lacks
                    //     PURRPAUSE_FROZEN_TZ — cosmetic-null today, the
                    //     popup body renders no TZ-dependent text.
                    let alive_pid = last_popup_child
                        .as_ref()
                        .filter(|c| c.try_wait_exited().is_none())
                        .map(|c| c.pid);
                    // Resolve any in-flight daily ack BEFORE computing
                    // daily_due / the collision predicate : while pending
                    // nothing re-sends ; past the deadline the update is
                    // unconfirmed and a real daily popup is kill/respawned
                    // (fire_popup_daily stamps only on spawn success). A
                    // successful respawn refreshes `fired_today` so the
                    // decisions below run against the post-stamp state.
                    let pending_tick = pending_daily_ack.as_ref().map(|p| {
                        (
                            p.pid,
                            super::resolve_pending_daily_ack(p, now_authoritative, alive_pid),
                        )
                    });
                    let had_pending_tick = pending_tick.is_some();
                    if let Some((ack_pid, outcome)) = pending_tick {
                        match outcome {
                            super::PendingAckTick::Wait => {}
                            super::PendingAckTick::AbandonChildGone => {
                                tracing::info!(
                                    pid = ack_pid,
                                    "daily ack pending but popup gone - abandoning in-place update"
                                );
                                pending_daily_ack = None;
                            }
                            super::PendingAckTick::RespawnDeadline => {
                                tracing::warn!(
                                    pid = ack_pid,
                                    "daily ack timeout - update unconfirmed, falling back to kill/respawn"
                                );
                                pending_daily_ack = None;
                                if fire_popup_daily(
                                    exe,
                                    last_popup,
                                    paused_secs,
                                    popup_pending,
                                    last_heartbeat,
                                    fired,
                                    last_popup_child,
                                    last_widget_children,
                                    &config.passcode_hash,
                                    daily_pause_fired_at,
                                    daily_pause_fired_paliers_bits,
                                    *daily_pause_skipped_today,
                                    popup_pid,
                                    config.daily_pause.timezone.as_str(),
                                    now_authoritative,
                                    u64::from(config.daily_pause.duration_minutes) * 60,
                                )
                                .is_ok()
                                {
                                    fired_today = true;
                                }
                                // Err is traced inside fire_popup_daily ;
                                // the collision path below re-evaluates on
                                // this same tick (safe : spawn-first /
                                // kill-on-success, and the Err arm handles
                                // a double failure).
                            }
                        }
                    }
                    // A deadline respawn replaces last_popup_child :
                    // refresh the liveness snapshot so the collision
                    // inputs + trace below never reference the killed pid.
                    let alive_pid = if had_pending_tick {
                        last_popup_child
                            .as_ref()
                            .filter(|c| c.try_wait_exited().is_none())
                            .map(|c| c.pid)
                    } else {
                        alive_pid
                    };
                    let daily_due = daily_state.enabled
                        && !fired_today
                        && !daily_state.skipped_today
                        && now_local >= daily_state.fire_at_today
                        && now_local <= daily_state.grace_window_end;
                    let collision_wants =
                        super::daily_collision_wants_update(&super::DailyCollisionInputs {
                            daily_due,
                            popup_pending: *popup_pending,
                            popup_child_alive: alive_pid.is_some(),
                            current_popup_is_daily: alive_pid.is_some()
                                && *popup_pid == alive_pid,
                            ack_pending: pending_daily_ack.is_some(),
                        });
                    // Transition-only trace of the daily decision. The
                    // tuple deliberately EXCLUDES popup_pending (interval
                    // popups toggle it all day) but INCLUDES in_window so
                    // the moment the daily target is reached prints one
                    // line even when nothing fires — with fired_at, the
                    // discriminator between "genuinely fired earlier
                    // today" and a falsely-latched stamp.
                    let in_window = daily_state.enabled
                        && now_local >= daily_state.fire_at_today
                        && now_local <= daily_state.grace_window_end;
                    let daily_trace = (
                        daily_state.enabled,
                        in_window,
                        daily_due,
                        fired_today,
                        daily_state.skipped_today,
                        collision_wants,
                    );
                    if last_daily_trace != Some(daily_trace) {
                        last_daily_trace = Some(daily_trace);
                        tracing::info!(
                            enabled = daily_state.enabled,
                            in_window,
                            daily_due,
                            fired_today,
                            fired_at = ?*daily_pause_fired_at,
                            skipped_today = daily_state.skipped_today,
                            popup_pending = *popup_pending,
                            alive_pid = ?alive_pid,
                            collision_wants_update = collision_wants,
                            "daily decision changed"
                        );
                    }
                    if collision_wants {
                        let target_pid =
                            alive_pid.expect("wants_update implies a live child");
                        let dp = &config.daily_pause;
                        let msg = crate::ipc_popup::PopupMessage::UpdateContent {
                            title: dp.popup_title.clone(),
                            subtitle: dp.popup_subtitle.clone(),
                            dismiss_label: dp.dismiss_button_label.clone(),
                            duration_secs: u64::from(dp.duration_minutes) * 60,
                            mode: crate::ipc_popup::PopupContentMode::DailyPause,
                        };
                        let passcode_hash = &config.passcode_hash;
                        let args: Vec<&OsStr> = if passcode_hash.is_empty() {
                            vec![
                                OsStr::new("--popup"),
                                OsStr::new("--no-debug"),
                                OsStr::new("--mode"),
                                OsStr::new("daily-pause"),
                            ]
                        } else {
                            vec![
                                OsStr::new("--popup"),
                                OsStr::new("--no-debug"),
                                OsStr::new("--mode"),
                                OsStr::new("daily-pause"),
                                OsStr::new("--passcode-hash"),
                                OsStr::new(passcode_hash),
                            ]
                        };
                        let env = [(
                            OsStr::new("PURRPAUSE_FROZEN_TZ"),
                            OsStr::new(dp.timezone.as_str()),
                        )];
                        // Same pre-spawn hygiene as fire_popup_daily : the
                        // sweep only targets ORPHANED msedgewebview2
                        // workers (dead parent), so running it while the
                        // live popup is up is harmless — and the fallback
                        // spawn inside update_or_spawn then starts clean.
                        if let Some(exe_parent) = exe.parent() {
                            kill_orphan_webview2_workers(&webview2_dir(exe_parent));
                        }
                        match update_or_spawn_replacing_with_env(
                            &RealSpawner,
                            exe,
                            &args,
                            &env,
                            last_popup_child,
                            target_pid,
                            &msg,
                            |pid, m| crate::ipc_popup::send_to_popup(pid, m),
                        ) {
                            Ok(UpdateOrSpawn::Updated) => {
                                // Pipe write OK — but that is NOT proof the
                                // popup applied the daily content. Stamp
                                // nothing yet : await the popup's DailyAck,
                                // or kill/respawn at the deadline.
                                tracing::info!(
                                    pid = target_pid,
                                    "daily collision: update sent in place - awaiting popup ack"
                                );
                                pending_daily_ack = Some(super::PendingDailyAck {
                                    pid: target_pid,
                                    deadline: now_authoritative + DAILY_ACK_TIMEOUT,
                                });
                            }
                            Ok(UpdateOrSpawn::Spawned(child)) => {
                                let pid = child.pid;
                                tracing::info!(
                                    pid,
                                    "daily collision: pipe unreachable, fell back to kill+respawn"
                                );
                                *last_popup_child = Some(child);
                                stamp_daily_fire_success(
                                    last_popup,
                                    paused_secs,
                                    popup_pending,
                                    last_heartbeat,
                                    fired,
                                    daily_pause_fired_at,
                                    daily_pause_fired_paliers_bits,
                                    *daily_pause_skipped_today,
                                    popup_pid,
                                    pid,
                                    now_authoritative,
                                    u64::from(dp.duration_minutes) * 60,
                                );
                                // New daily popup is up : drain any lingering
                                // heads-up widget.
                                drain_heads_up_widgets(last_widget_children);
                            }
                            Err(e) => {
                                // Both the pipe AND the fallback spawn
                                // failed. Nothing stamped — popup_pending
                                // stays true, the predecessor stays alive,
                                // and the next tick retries the whole
                                // collision resolution.
                                tracing::warn!(
                                    error = ?e,
                                    "daily collision: update and respawn both failed - retrying next tick"
                                );
                            }
                        }
                    }

                    match scheduler::next_event(
                        now,
                        config,
                        *last_popup,
                        *paused_secs,
                        fired,
                        *popup_pending,
                        &daily_state,
                        now_local,
                    ) {
                        SchedulerDecision::Nothing => {
                            // Cas C anti-forkbomb : past the grace window,
                            // no fire happened, no skip recorded → mark
                            // today as skipped so subsequent ticks don't
                            // re-enter FireDailyPause once fired_today
                            // still reads false. Only touches state when
                            // the daily branch was live (enabled).
                            if daily_state.enabled
                                && now_local > daily_state.grace_window_end
                                && !fired_today
                                && !daily_state.skipped_today
                            {
                                let today = now_local.date_naive();
                                *daily_pause_skipped_today = Some(today);
                                tracing::info!(
                                    date = %today,
                                    "Cas C: past grace window without fire, marking day skipped"
                                );
                            }
                        }
                        SchedulerDecision::FirePopup => {
                            let elapsed_wall_secs = now
                                .duration_since(*last_popup)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            let elapsed_active_secs =
                                elapsed_wall_secs.saturating_sub(*paused_secs);
                            let interval_secs =
                                scheduler::interval(config).as_secs();
                            tracing::info!(
                                elapsed_wall_secs,
                                paused_secs = *paused_secs,
                                elapsed_active_secs,
                                interval_secs,
                                playing_video_or_audio = playing_video_or_audio_now,
                                "scheduler said FirePopup - firing"
                            );
                            if let Err(e) = fire_popup(
                                exe,
                                last_popup,
                                paused_secs,
                                popup_pending,
                                last_heartbeat,
                                fired,
                                last_popup_child,
                                last_widget_children,
                                &config.passcode_hash,
                                *daily_pause_fired_at,
                                *daily_pause_fired_paliers_bits,
                                *daily_pause_skipped_today,
                                *popup_pid,
                            ) {
                                tracing::warn!(error = ?e, "tick fire_popup failed");
                            }
                        }
                        SchedulerDecision::FireCountdown {
                            palier_minutes,
                            seconds_until_popup,
                        } => {
                            if let Err(e) = fire_countdown(
                                exe,
                                palier_minutes,
                                seconds_until_popup,
                                fired,
                                last_widget_children,
                            ) {
                                tracing::warn!(error = ?e, "tick fire_countdown failed");
                            }
                        }
                        SchedulerDecision::FireDailyPause => {
                            tracing::info!(
                                fire_at = %daily_state.fire_at_today,
                                "scheduler said FireDailyPause - firing daily-pause popup"
                            );
                            match fire_popup_daily(
                                exe,
                                last_popup,
                                paused_secs,
                                popup_pending,
                                last_heartbeat,
                                fired,
                                last_popup_child,
                                last_widget_children,
                                &config.passcode_hash,
                                daily_pause_fired_at,
                                daily_pause_fired_paliers_bits,
                                *daily_pause_skipped_today,
                                popup_pid,
                                &config.daily_pause.timezone,
                                now_authoritative,
                                u64::from(config.daily_pause.duration_minutes) * 60,
                            ) {
                                Ok(()) => {}
                                Err(e) => {
                                    tracing::warn!(error = ?e, "fire_popup_daily failed");
                                }
                            }
                        }
                        SchedulerDecision::FireDailyCountdown {
                            palier_minutes,
                            seconds_until,
                        } => {
                            match fire_countdown_daily(
                                exe,
                                palier_minutes,
                                seconds_until,
                                last_widget_children,
                                daily_pause_fired_paliers_bits,
                                &config.daily_pause.timezone,
                            ) {
                                Ok(()) => {}
                                Err(e) => {
                                    tracing::warn!(error = ?e, "fire_countdown_daily failed");
                                }
                            }
                        }
                    }

                    // 5. Throttled heartbeat (~30 s cadence). The fire
                    //    path persists eagerly so we don't double-write
                    //    on the same tick.
                    *last_heartbeat = now;
                    ticks_since_write += 1;
                    if ticks_since_write >= HEARTBEAT_EVERY_N_TICKS {
                        ticks_since_write = 0;
                        if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
                            last_popup: *last_popup,
                            paused_secs: *paused_secs,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
                            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                            daily_pause_fired_at: *daily_pause_fired_at,
                            daily_pause_fired_paliers_bits: *daily_pause_fired_paliers_bits,
                            daily_pause_skipped_today: *daily_pause_skipped_today,
                            popup_pid: *popup_pid,
                        }) {
                            tracing::warn!(error = ?e, "heartbeat runtime.dat write failed");
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("command channel disconnected, stopping");
                    return Ok(());
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Terminate every lingering heads-up (countdown) widget child. Called
    /// whenever a pause popup appears (the countdown counts down *to* that
    /// popup, so once it shows the heads-up is obsolete) and by the Reset
    /// action. `spawn_replacing_keyed` only ever replaces the same-palier
    /// predecessor, so nothing else drains the whole map.
    fn drain_heads_up_widgets(children: &mut HashMap<u32, SpawnedChild>) {
        for (_, w) in children.drain() {
            w.terminate();
        }
    }

    fn fire_popup(
        exe: &Path,
        last_popup: &mut SystemTime,
        paused_secs: &mut u64,
        popup_pending: &mut bool,
        last_heartbeat: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        passcode_hash: &str,
        daily_pause_fired_at: Option<SystemTime>,
        daily_pause_fired_paliers_bits: u8,
        daily_pause_skipped_today: Option<chrono::NaiveDate>,
        popup_pid: Option<u32>,
    ) -> Result<()> {
        // Pre-spawn sweep : belt-and-braces. Startup already fires once ;
        // this catches workers leaked between our own popup lifetimes.
        if let Some(exe_parent) = exe.parent() {
            kill_orphan_webview2_workers(&webview2_dir(exe_parent));
        }
        // --no-debug so the service-spawned popup runs in production
        // mode : keyboard hook installed + force-minimize honoured.
        // Debug-default is only for developer manual invocations
        // from PowerShell during smoke.
        //
        // --passcode-hash <phc> forwards the argon2id hash so the popup
        // can verify Alt+pincode unlock attempts. Empty hash (fresh
        // install pre-passcode-set) → arg omitted, popup falls back to
        // countdown-only. The PHC string is NOT the passcode itself
        // and is not recoverable to one, so argv exposure is acceptable.
        let args: Vec<&OsStr> = if passcode_hash.is_empty() {
            vec![OsStr::new("--popup"), OsStr::new("--no-debug")]
        } else {
            vec![
                OsStr::new("--popup"),
                OsStr::new("--no-debug"),
                OsStr::new("--passcode-hash"),
                OsStr::new(passcode_hash),
            ]
        };
        // Spawn-first, kill-on-success. Predecessor left behind by an
        // Alt+Tab'd user (post-countdown unlock, 0.7.1 Track A) is
        // terminated only if the new spawn succeeded — better a stale
        // popup than none at all. HANDLE-based terminate is immune to
        // PID recycle.
        match spawn_replacing(&RealSpawner, exe, &args, last_popup_child) {
            Ok(child) => {
                tracing::info!(pid = child.pid, "popup child spawned");
                let now = SystemTime::now();
                *last_popup = now;
                *paused_secs = 0;
                *popup_pending = true;
                *last_heartbeat = now;
                fired.clear();
                // The popup is now up : any "pause dans X min" heads-up widget
                // is obsolete and must not linger behind it.
                drain_heads_up_widgets(last_widget_children);
                if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
                    last_popup: now,
                    paused_secs: 0,
                    popup_pending: true,
                    last_heartbeat: now,
                    last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                    // Interval popup fire preserves — does not touch — the
                    // daily-pause bookkeeping. If the interval and daily
                    // popups happen to collide, `spawn_replacing` kills
                    // the predecessor regardless of which side owned it.
                    daily_pause_fired_at,
                    daily_pause_fired_paliers_bits,
                    daily_pause_skipped_today,
                    popup_pid,
                }) {
                    tracing::warn!(error = ?e, "write runtime.dat failed");
                }
                *last_popup_child = Some(child);
                Ok(())
            }
            Err(e) => {
                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                    tracing::info!(
                        error = %e,
                        "popup spawn deferred - user session not ready, caller will retry"
                    );
                } else {
                    tracing::warn!(error = ?e, "popup spawn failed - will retry on next tick");
                }
                Err(e)
            }
        }
    }

    /// Spawn the user-session input probe (`--input-probe`). The
    /// probe lives for the service's lifetime ; if it dies (crash,
    /// user log-off, session change), the tick loop's respawn poll
    /// catches it within one tick (~5 s). No spawn-replacing pattern
    /// needed — the caller holds at most one handle at a time.
    fn spawn_input_probe(exe: &Path) -> Result<SpawnedChild> {
        use crate::platform::Spawner;
        RealSpawner.spawn(exe, &[OsStr::new("--input-probe")])
    }

    fn fire_countdown(
        exe: &Path,
        palier: u32,
        seconds_until_popup: u64,
        fired: &mut std::collections::HashSet<u32>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
    ) -> Result<()> {
        let seconds_str = seconds_until_popup.to_string();
        let palier_str = palier.to_string();
        // Same rationale as fire_popup : --no-debug forces the
        // service-spawned widget into production mode
        // (force-minimize per Config::force_minimize_paliers).
        let args: [&OsStr; 5] = [
            OsStr::new("--countdown"),
            OsStr::new(&seconds_str),
            OsStr::new("--palier"),
            OsStr::new(&palier_str),
            OsStr::new("--no-debug"),
        ];
        // Spawn-first, kill-on-success — same invariant as fire_popup.
        // Different-palier widgets remain untouched.
        match spawn_replacing_keyed(&RealSpawner, exe, &args, palier, last_widget_children) {
            Ok(()) => {
                tracing::info!(palier, "countdown child spawned");
                fired.insert(palier);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = ?e, palier, "countdown spawn failed");
                Err(e)
            }
        }
    }

    /// Build the tick-time `DailyPauseState` handed to `scheduler::next_event`.
    /// Kernel input, ephemeral — not persisted.
    fn build_daily_pause_state(
        config: &Config,
        now_local: chrono::DateTime<chrono_tz::Tz>,
        fired_today: bool,
        fired_paliers_bits: u8,
        skipped_today_marker: Option<chrono::NaiveDate>,
    ) -> scheduler::DailyPauseState {
        use chrono::{Datelike, Duration as ChronoDuration, TimeZone};
        let tz = now_local.timezone();
        let hour = config.daily_pause.hour as u32;
        let minute = config.daily_pause.minute as u32;
        // Today's fire moment in the resolved TZ. We construct at H:M
        // then defer DST handling to chrono — Ambiguous → earlier,
        // None → bump 1h (mirrors `time_authority::with_hm`).
        let fire_at_today = tz
            .with_ymd_and_hms(now_local.year(), now_local.month(), now_local.day(), hour, minute, 0)
            .single()
            .or_else(|| {
                tz.with_ymd_and_hms(
                    now_local.year(),
                    now_local.month(),
                    now_local.day(),
                    hour.saturating_add(1).min(23),
                    minute,
                    0,
                )
                .single()
            })
            .unwrap_or_else(|| {
                tz.with_ymd_and_hms(
                    now_local.year(),
                    now_local.month(),
                    now_local.day(),
                    12,
                    0,
                    0,
                )
                .single()
                .expect("noon is always valid")
            });
        let grace_end = fire_at_today + ChronoDuration::minutes(config.daily_pause.duration_minutes as i64);
        let skipped_today = skipped_today_marker
            .map(|d| d == now_local.date_naive())
            .unwrap_or(false);
        scheduler::DailyPauseState {
            enabled: config.daily_pause.enabled,
            fired_today,
            fire_at_today,
            grace_window_end: grace_end,
            fired_paliers: runtime_dat::bits_to_paliers(fired_paliers_bits),
            skipped_today,
        }
    }

    /// Fire the daily-pause popup. Jumelle of [`fire_popup`] with two
    /// differences : (a) argv adds `--mode daily-pause` so the child
    /// reads `cfg.daily_pause.*` for title/subtitle/duration ;
    /// (b) `PURRPAUSE_FROZEN_TZ` env var pins the child's display
    /// formatting to the config IANA TZ (defence against Windows-panel
    /// TZ tampering). Updates `daily_pause_fired_at` +
    /// `daily_pause_fired_paliers_bits = 0` + `popup_pid` on success.
    #[allow(clippy::too_many_arguments)]
    fn fire_popup_daily(
        exe: &Path,
        last_popup: &mut SystemTime,
        paused_secs: &mut u64,
        popup_pending: &mut bool,
        last_heartbeat: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        passcode_hash: &str,
        daily_pause_fired_at: &mut Option<SystemTime>,
        daily_pause_fired_paliers_bits: &mut u8,
        daily_pause_skipped_today: Option<chrono::NaiveDate>,
        popup_pid: &mut Option<u32>,
        tz_name: &str,
        now: SystemTime,
        daily_duration_secs: u64,
    ) -> Result<()> {
        if let Some(exe_parent) = exe.parent() {
            kill_orphan_webview2_workers(&webview2_dir(exe_parent));
        }
        let args: Vec<&OsStr> = if passcode_hash.is_empty() {
            vec![
                OsStr::new("--popup"),
                OsStr::new("--no-debug"),
                OsStr::new("--mode"),
                OsStr::new("daily-pause"),
            ]
        } else {
            vec![
                OsStr::new("--popup"),
                OsStr::new("--no-debug"),
                OsStr::new("--mode"),
                OsStr::new("daily-pause"),
                OsStr::new("--passcode-hash"),
                OsStr::new(passcode_hash),
            ]
        };
        let env = [(OsStr::new("PURRPAUSE_FROZEN_TZ"), OsStr::new(tz_name))];
        match spawn_replacing_with_env(&RealSpawner, exe, &args, &env, last_popup_child) {
            Ok(child) => {
                tracing::info!(pid = child.pid, "daily-pause popup child spawned");
                // The daily popup is up : drain any lingering heads-up widget.
                drain_heads_up_widgets(last_widget_children);
                stamp_daily_fire_success(
                    last_popup,
                    paused_secs,
                    popup_pending,
                    last_heartbeat,
                    fired,
                    daily_pause_fired_at,
                    daily_pause_fired_paliers_bits,
                    daily_pause_skipped_today,
                    popup_pid,
                    child.pid,
                    now,
                    daily_duration_secs,
                );
                *last_popup_child = Some(child);
                Ok(())
            }
            Err(e) => {
                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                    tracing::info!(error = %e, "daily-pause popup spawn deferred - user session not ready");
                } else {
                    tracing::warn!(error = ?e, "daily-pause popup spawn failed");
                }
                Err(e)
            }
        }
    }

    /// Bookkeeping shared by every "the daily pause is now live" outcome :
    /// classic spawn (`fire_popup_daily`), collision in-place update, and
    /// collision fallback respawn. `new_pid` is the popup now carrying
    /// the daily content — freshly spawned or converted in place. Same
    /// persisted bytes as the historical fire_popup_daily success arm.
    #[allow(clippy::too_many_arguments)]
    fn stamp_daily_fire_success(
        last_popup: &mut SystemTime,
        paused_secs: &mut u64,
        popup_pending: &mut bool,
        last_heartbeat: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        daily_pause_fired_at: &mut Option<SystemTime>,
        daily_pause_fired_paliers_bits: &mut u8,
        daily_pause_skipped_today: Option<chrono::NaiveDate>,
        popup_pid: &mut Option<u32>,
        new_pid: u32,
        now: SystemTime,
        daily_duration_secs: u64,
    ) {
        // Anchor the interval cycle at lunch END (see
        // daily_lunch_end_anchor) — downstream duration_since(last_popup)
        // all saturate to zero while the anchor is still in the future.
        let lunch_end = super::daily_lunch_end_anchor(now, daily_duration_secs);
        *last_popup = lunch_end;
        *paused_secs = 0;
        *popup_pending = true;
        *last_heartbeat = now;
        *daily_pause_fired_at = Some(now);
        *daily_pause_fired_paliers_bits = 0;
        *popup_pid = Some(new_pid);
        fired.clear();
        tracing::info!(
            pid = new_pid,
            fired_at = ?now,
            interval_anchor = ?lunch_end,
            "daily fire stamped - fired_today latched until midnight / rearm"
        );
        if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
            last_popup: lunch_end,
            paused_secs: 0,
            popup_pending: true,
            last_heartbeat: now,
            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
            daily_pause_fired_at: *daily_pause_fired_at,
            daily_pause_fired_paliers_bits: *daily_pause_fired_paliers_bits,
            daily_pause_skipped_today,
            popup_pid: *popup_pid,
        }) {
            tracing::warn!(error = ?e, "write runtime.dat failed (daily fire)");
        }
    }

    /// Fire a daily-pause pre-notif countdown for one palier (15 / 10 / 5).
    /// Jumelle of [`fire_countdown`] with `--mode daily-pause` +
    /// `PURRPAUSE_FROZEN_TZ`. Updates the palier bitmask on success ;
    /// spawn_replacing_keyed prevents same-palier duplicates.
    fn fire_countdown_daily(
        exe: &Path,
        palier: u32,
        seconds_until: u64,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        daily_pause_fired_paliers_bits: &mut u8,
        tz_name: &str,
    ) -> Result<()> {
        let seconds_str = seconds_until.to_string();
        let palier_str = palier.to_string();
        let args: [&OsStr; 7] = [
            OsStr::new("--countdown"),
            OsStr::new(&seconds_str),
            OsStr::new("--palier"),
            OsStr::new(&palier_str),
            OsStr::new("--no-debug"),
            OsStr::new("--mode"),
            OsStr::new("daily-pause"),
        ];
        // Env-aware spawn is not required for countdowns today (they
        // don't format TZ-sensitive text yet) but we pass the env var
        // anyway to keep both children symmetric ; the config UI in
        // session 3 will surface a TZ label that reads it.
        let _tz_env = tz_name; // reserved for future use
        // Reuse spawn_replacing_keyed — no env override needed for
        // countdowns until session 3.
        match spawn_replacing_keyed(&RealSpawner, exe, &args, palier, last_widget_children) {
            Ok(()) => {
                let existing: std::collections::HashSet<u32> =
                    runtime_dat::bits_to_paliers(*daily_pause_fired_paliers_bits);
                let mut merged = existing;
                merged.insert(palier);
                *daily_pause_fired_paliers_bits = runtime_dat::paliers_to_bits(&merged);
                tracing::info!(palier, "daily-pause countdown child spawned");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = ?e, palier, "daily-pause countdown spawn failed");
                Err(e)
            }
        }
    }

    /// Cas D respawn : bare-metal spawn used at service boot ONLY, when
    /// no `tick_loop` state has been constructed yet. Returns the new
    /// child's PID on success so the caller can persist it to
    /// `runtime.dat` via the cold-start write path.
    /// True when the popup interrupted by a service death was the daily
    /// lunch pause — a `daily_pause_fired_at` from today's local date.
    /// Routes the cold-start refire (pre-loop relaunch AND tick-loop 2c
    /// retry) to the daily respawn instead of an interval popup, so a
    /// reboot mid-midi resumes the MIDI pause, never a classic one.
    fn daily_popup_was_interrupted(
        config: &Config,
        daily_pause_fired_at: Option<SystemTime>,
    ) -> bool {
        let Some(fired_at) = daily_pause_fired_at else {
            return false;
        };
        let now_local =
            time_authority::in_configured_tz(config, time_authority::now_authoritative(config));
        let fired_local = time_authority::in_configured_tz(config, fired_at);
        fired_local.date_naive() == now_local.date_naive()
    }

    fn respawn_daily_popup_cold_start(
        exe: &Path,
        elapsed_secs: u32,
        config: &Config,
        last_popup_child: &mut Option<SpawnedChild>,
    ) -> Result<u32> {
        let elapsed_str = elapsed_secs.to_string();
        let args: Vec<&OsStr> = if config.passcode_hash.is_empty() {
            vec![
                OsStr::new("--popup"),
                OsStr::new("--no-debug"),
                OsStr::new("--mode"),
                OsStr::new("daily-pause"),
                OsStr::new("--elapsed-seconds"),
                OsStr::new(&elapsed_str),
            ]
        } else {
            vec![
                OsStr::new("--popup"),
                OsStr::new("--no-debug"),
                OsStr::new("--mode"),
                OsStr::new("daily-pause"),
                OsStr::new("--elapsed-seconds"),
                OsStr::new(&elapsed_str),
                OsStr::new("--passcode-hash"),
                OsStr::new(&config.passcode_hash),
            ]
        };
        let env = [(
            OsStr::new("PURRPAUSE_FROZEN_TZ"),
            OsStr::new(config.daily_pause.timezone.as_str()),
        )];
        let child = spawn_replacing_with_env(&RealSpawner, exe, &args, &env, last_popup_child)?;
        let pid = child.pid;
        *last_popup_child = Some(child);
        Ok(pid)
    }

    fn run_pipe(
        cancel: ipc::SendableHandle,
        tx: mpsc::Sender<TickCommand>,
        stamp_tx: mpsc::Sender<idle::InputStampState>,
    ) {
        let (msg_tx, msg_rx) = mpsc::channel::<Message>();
        let pipe_tx = tx.clone();
        // Bridge Message → (TickCommand OR stamp) on the same thread.
        // Spawn the accept loop, then relay messages until the pipe
        // closes. `InputStamp` bypasses the TickCommand mpsc and goes
        // straight to a dedicated stamp channel — see the run_service
        // comment on the stamp channel for the starvation rationale.
        let accept_result = thread::spawn(move || ipc::run_server(cancel, msg_tx));
        while let Ok(msg) = msg_rx.recv() {
            match msg {
                Message::TriggerPopupNow => {
                    let _ = pipe_tx.send(TickCommand::Trigger);
                }
                Message::Reload => {
                    let _ = pipe_tx.send(TickCommand::Reload);
                }
                Message::RearmDailyPause => {
                    let _ = pipe_tx.send(TickCommand::RearmDaily);
                }
                Message::ResetPause => {
                    let _ = pipe_tx.send(TickCommand::ResetPause);
                }
                Message::DailyPauseApplied { pid } => {
                    let _ = pipe_tx.send(TickCommand::DailyAck { pid });
                }
                Message::AddFirewallRule => {
                    // Handled inline (not via TickCommand): (re)creating the
                    // firewall rule is unrelated to scheduler state, and this
                    // service process runs as SYSTEM — it has the elevation the
                    // INetFwPolicy2 COM API needs, which the unelevated config
                    // UI that sent this lacks.
                    match env::current_exe() {
                        Ok(exe) => {
                            if let Err(e) = crate::platform::win32::firewall::add_rule(&exe) {
                                tracing::error!(error = %e, "AddFirewallRule: add_rule failed");
                            } else {
                                tracing::info!("AddFirewallRule: firewall rule (re)created");
                            }
                        }
                        Err(e) => tracing::error!(error = %e, "AddFirewallRule: current_exe() failed"),
                    }
                }
                Message::Shutdown => {
                    let _ = pipe_tx.send(TickCommand::Shutdown);
                    break;
                }
                Message::InputStamp { tick_ms, playing_audio, playing_smtc, locked } => {
                    let received_at = unsafe { GetTickCount64() };
                    let _ = stamp_tx.send(idle::InputStampState {
                        last_input_tick_ms: tick_ms,
                        received_at_tick_ms: received_at,
                        playing_audio,
                        playing_smtc,
                        locked,
                    });
                }
            }
        }
        if let Ok(Err(e)) = accept_result.join() {
            tracing::warn!(error = ?e, "pipe accept loop exited with error");
        }
    }

    fn spawn_notify(
        tx: mpsc::Sender<TickCommand>,
        running: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            use notify::{RecommendedWatcher, RecursiveMode, Watcher};

            let (fs_tx, fs_rx) = mpsc::channel::<notify::Result<notify::Event>>();
            let mut watcher: RecommendedWatcher = match notify::recommended_watcher(fs_tx) {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!(error = ?e, "notify::recommended_watcher failed");
                    return;
                }
            };
            let watched_dir = Path::new(DIAGNOSTICS_CACHE_DIR);
            if let Err(e) = watcher.watch(watched_dir, RecursiveMode::NonRecursive) {
                tracing::error!(error = ?e, dir = %watched_dir.display(), "watcher.watch failed");
                return;
            }

            let target_name = Path::new(STATE_DAT).file_name();
            let mut pending = false;

            while running.load(Ordering::Relaxed) {
                match fs_rx.recv_timeout(DEBOUNCE) {
                    Ok(Ok(event)) => {
                        if is_relevant_event(&event, target_name) {
                            tracing::trace!(kind = ?event.kind, "state.dat event");
                            pending = true;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = ?e, "notify event error");
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if pending {
                            let _ = tx.send(TickCommand::Reload);
                            pending = false;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            drop(watcher);
        })
    }

    fn is_relevant_event(
        event: &notify::Event,
        target_name: Option<&std::ffi::OsStr>,
    ) -> bool {
        use notify::EventKind;
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return false;
        }
        let Some(target) = target_name else {
            return true;
        };
        event
            .paths
            .iter()
            .any(|p| p.file_name().map(|n| n == target).unwrap_or(false))
    }

    fn running() -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(0),
            process_id: None,
        }
    }

    fn stopped(exit_code: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(0),
            process_id: None,
        }
    }

}
