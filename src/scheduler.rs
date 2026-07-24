// On non-Windows targets the only callers of this kernel are behind
// `#[cfg(windows)]` (service.rs, tabs.rs), so the whole public surface
// looks dead to the compiler outside of `mod tests`. Silence on Linux dev.
#![cfg_attr(not(windows), allow(dead_code))]

// Pure scheduling logic — decides whether the service should fire a
// popup, a countdown palier, or do nothing, given the current wall-clock
// time and the last popup timestamp.
//
// Fully unit-testable on Linux : all time is passed in as `SystemTime`,
// no calls to `SystemTime::now()` or any Win32 API. The service loop
// glues this together with the actual tick / spawn plumbing.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Duration as ChronoDuration, TimeZone};
use chrono_tz::Tz;

use crate::config::Config;

// 1 minute minimum : aligned with the config UI's minutes-based slider
// (range 1..=720 min). Was 0.1 h (= 6 min) which silently forced any
// smaller user input back to the 2 h default — invisible to the user
// and made 1-min service-cycle testing impossible.
const MIN_INTERVAL_HOURS: f32 = 1.0 / 60.0;
const MAX_INTERVAL_HOURS: f32 = 168.0;

const DEFAULT_INTERVAL_HOURS: f32 = 2.0;

/// Anti-cheat: on a cold start where `runtime.dat` is missing but the
/// install is not brand-new (migrated 0.6.x install, or the user
/// manually deleted runtime.dat), pretend the previous popup fired
/// `interval - COLD_START_GRACE` ago. The next popup then fires in
/// exactly `COLD_START_GRACE`, so a child can't reset the timer by
/// killing the service.
pub const COLD_START_GRACE: Duration = Duration::from_secs(60);

/// Fresh-install grace window: `runtime.dat` missing AND
/// `first_install_at` within this many seconds of now = "user just
/// installed, respect the full interval, don't shorten to 60 s."
pub const FRESH_INSTALL_GRACE: Duration = Duration::from_secs(300);

/// Reload clamp: when the user reduces `interval_hours` mid-cycle, the
/// scheduler bumps `last_popup` forward so the next popup fires at
/// least this many seconds from now. Keeps a reduced interval from
/// firing instantly on Save.
pub const RELOAD_MIN_GRACE: Duration = Duration::from_secs(120);

/// Inputs shared by [`resolve_last_popup`] (reload + fresh-install) and
/// [`resolve_cold_start`] (cold-boot threshold / anti-bypass / relaunch
/// / credit).
///
/// - `now` : wall clock at the decision moment.
/// - `last_popup` : `None` when `runtime.dat` is missing, `Some(t)`
///   otherwise.
/// - `first_install_at` : from `Config::first_install_at()`. Sentinel
///   `UNIX_EPOCH` means "unknown / migrated 0.6.x install."
/// - `is_reload` : `true` only on IPC `Reload`. Switches
///   [`resolve_last_popup`] to the reduce-interval clamp branch.
/// - `paused_secs` : cumulative "not-active" seconds already banked
///   into the current cycle (persisted).
/// - `popup_pending` : a popup was mid-fire when the service last
///   heartbeat'd. Combined with `was_clean_shutdown`, drives the
///   anti-bypass vs relaunch branch selection.
/// - `last_heartbeat` : the last tick's `SystemTime::now()`, persisted
///   every ~30 s. `now - last_heartbeat` approximates cross-reboot
///   downtime credited into `paused_secs` on the credit-downtime
///   branch.
pub struct ResolveInputs {
    pub now: SystemTime,
    pub last_popup: Option<SystemTime>,
    pub first_install_at: SystemTime,
    pub is_reload: bool,
    pub paused_secs: u64,
    pub popup_pending: bool,
    pub last_heartbeat: SystemTime,
    /// `true` when the previous run stamped `last_clean_shutdown_at`
    /// at or after its final heartbeat, i.e. the SCM Stop path
    /// completed gracefully. Used by [`resolve_cold_start`] to
    /// discriminate a user-initiated PC restart mid-popup (legitimate,
    /// relaunch the popup) from a kid-killed service (unclean, punish
    /// with anti-bypass full reset). Ignored by [`resolve_last_popup`]
    /// — reload / fresh-install branches don't need it.
    pub was_clean_shutdown: bool,
}

/// Decide what `last_popup` should hold given cold-start / reload /
/// fresh-install context. Pure : no I/O, no globals. See
/// [`ResolveInputs`] for parameter semantics.
///
/// The three decision branches :
/// 1. **Reload with reduced interval** : if the user shrank the
///    interval and the naive `last_popup + new_interval` lands within
///    the reload grace window, bump `last_popup` forward so the next
///    popup fires at exactly `now + RELOAD_MIN_GRACE`.
/// 2. **Cold start, no runtime.dat** : fresh install grace (returns
///    `now` so the first popup fires in the full interval) or, when
///    the sentinel indicates migrated install, the legacy anti-cheat
///    shift.
/// 3. **Cold start with runtime.dat** : if elapsed downtime exceeds
///    `2 * interval` reset to `now` (long-downtime reset) ; else return
///    `last_popup` unchanged.
pub fn resolve_last_popup(inputs: ResolveInputs, config: &Config) -> SystemTime {
    let ResolveInputs {
        now,
        last_popup,
        first_install_at,
        is_reload,
        // Cold-start-only fields — the reload / fresh-install / legacy
        // branches don't touch active-time counters.
        paused_secs: _,
        popup_pending: _,
        last_heartbeat: _,
        was_clean_shutdown: _,
    } = inputs;
    let interval = interval(config);

    // Branch 1 : Reload with (potentially) reduced interval.
    if is_reload {
        let lp = match last_popup {
            Some(lp) => lp,
            // Defensive : Reload before the service ever started
            // shouldn't happen in practice ; treat as fresh install.
            None => return now,
        };
        // Ensure `lp + interval >= now + RELOAD_MIN_GRACE`. If already
        // satisfied, leave `lp` alone. Else clamp `lp` forward.
        let next = lp + interval;
        let floor = now + RELOAD_MIN_GRACE;
        if next >= floor {
            return lp;
        }
        // We want `lp' + interval == floor`, so `lp' = floor - interval`.
        // saturating_sub via checked_sub for pre-epoch safety.
        return floor.checked_sub(interval).unwrap_or(now);
    }

    // Branch 2 : Cold start with no runtime.dat.
    let lp = match last_popup {
        Some(lp) => lp,
        None => {
            // Fresh install detection : first_install_at is a real
            // stamp AND recent enough.
            if first_install_at != SystemTime::UNIX_EPOCH {
                if let Ok(age) = now.duration_since(first_install_at) {
                    if age < FRESH_INSTALL_GRACE {
                        return now;
                    }
                }
            }
            // Migrated 0.6.x install OR runtime.dat manually deleted.
            // Apply legacy anti-cheat shift.
            let shift = interval.saturating_sub(COLD_START_GRACE);
            return now.checked_sub(shift).unwrap_or(now);
        }
    };

    // Branch 3 : Cold start with runtime.dat. Long-downtime reset.
    let elapsed = now.duration_since(lp).unwrap_or(Duration::ZERO);
    if elapsed >= interval.saturating_mul(2) {
        return now;
    }
    lp
}

/// Outcome of [`resolve_cold_start`] — everything the service tick loop
/// needs to seed its in-memory state after reading `runtime.dat`.
/// `relaunch_now = true` asks the caller to fire a popup immediately
/// after cold-start (resurrected Rule B, now gated on clean shutdown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdStartDecision {
    pub last_popup: SystemTime,
    pub paused_secs: u64,
    pub popup_pending: bool,
    pub relaunch_now: bool,
}

/// Cold-boot dispatch for the active-time cycle. Pure ; no I/O.
///
/// Preconditions : `inputs.last_popup.is_some()` — the caller must have
/// read a valid `runtime.dat`. On a missing / corrupt file the caller
/// keeps using [`resolve_last_popup`] with `last_popup: None` for the
/// fresh-install / legacy-shift branch.
///
/// Five ordered rules :
/// 1. **Threshold reset** — `elapsed = now - last_popup >= reset_threshold_hours * 3600`.
///    Full reset : `last_popup = now`, `paused_secs = 0`,
///    `popup_pending = false`, no relaunch. Child was away long enough
///    that resuming mid-cycle buys nothing.
/// 2. **Anti-bypass** — else if `popup_pending && !was_clean_shutdown`.
///    Signature of "kid killed the service mid-popup to skip it".
///    Full reset (same shape as threshold) : the trick costs one full
///    cycle before the next popup fires.
/// 3. **Absence covered the break** — else if
///    `popup_pending && was_clean_shutdown && downtime >= duration_minutes * 60`.
///    User cleanly rebooted mid-popup AND stayed away at least as long
///    as the popup's own countdown asked them to. The absence *is* the
///    break — full reset, no relaunch. Kids-can't-bypass-by-rebooting
///    is preserved because either downtime is below duration (goes to
///    rule 4, relaunch) or elapsed is above the interval threshold
///    (goes to rule 1, threshold reset).
/// 4. **Relaunch pending popup** — else if `popup_pending && was_clean_shutdown`.
///    Short clean reboot mid-popup — the popup deserves to be re-shown.
///    Keep `last_popup` unchanged, keep `popup_pending`, set
///    `relaunch_now = true`. Caller fires the popup right after
///    cold-start.
/// 5. **Credit downtime** — else (popup was NOT pending). Keep
///    `last_popup`, credit `downtime = now - last_heartbeat` into
///    `paused_secs`, no relaunch. Clock skew (`now < last_heartbeat`)
///    treated as zero downtime, never negative.
pub fn resolve_cold_start(inputs: ResolveInputs, config: &Config) -> ColdStartDecision {
    let ResolveInputs {
        now,
        last_popup,
        paused_secs,
        popup_pending,
        last_heartbeat,
        was_clean_shutdown,
        // Reload / fresh-install fields — unused on the cold-start path.
        first_install_at: _,
        is_reload: _,
    } = inputs;

    let last_popup = last_popup.expect(
        "resolve_cold_start requires runtime.dat present; caller must \
         route None to resolve_last_popup fresh-install branch",
    );

    let elapsed = now.duration_since(last_popup).unwrap_or(Duration::ZERO);
    let reset_threshold_secs =
        (f64::from(config.reset_threshold_hours) * 3600.0) as u64;

    if elapsed.as_secs() >= reset_threshold_secs {
        return ColdStartDecision {
            last_popup: now,
            paused_secs: 0,
            popup_pending: false,
            relaunch_now: false,
        };
    }

    if popup_pending && !was_clean_shutdown {
        return ColdStartDecision {
            last_popup: now,
            paused_secs: 0,
            popup_pending: false,
            relaunch_now: false,
        };
    }

    if popup_pending && was_clean_shutdown {
        let downtime = now
            .duration_since(last_heartbeat)
            .unwrap_or(Duration::ZERO);
        let popup_duration_secs = u64::from(config.duration_minutes)
            .saturating_mul(60);
        if downtime.as_secs() >= popup_duration_secs {
            // The absence itself was longer than the break the popup
            // was going to force. Treat as taken — fresh cycle from now,
            // no relaunch.
            return ColdStartDecision {
                last_popup: now,
                paused_secs: 0,
                popup_pending: false,
                relaunch_now: false,
            };
        }
        return ColdStartDecision {
            last_popup,
            paused_secs,
            popup_pending: true,
            relaunch_now: true,
        };
    }

    let downtime = now
        .duration_since(last_heartbeat)
        .unwrap_or(Duration::ZERO);
    ColdStartDecision {
        last_popup,
        paused_secs: paused_secs.saturating_add(downtime.as_secs()),
        popup_pending: false,
        relaunch_now: false,
    }
}

/// Compare wall-clock and unbiased-interrupt-time deltas across a tick
/// and return the divergence in seconds if it exceeds `jump_threshold_secs`.
///
/// The unbiased interrupt time (see `QueryUnbiasedInterruptTime`) is
/// monotonic and does NOT count time the system spent suspended /
/// hibernated / (on VMs) frozen. Wall clock counts all of that, plus
/// tolerates administrator or NTP jumps in either direction. Divergence
/// isolates the anomaly :
///
/// - `wall_delta ≈ unbiased_delta` : normal tick — return `None`.
/// - `wall_delta >> unbiased_delta` : suspend, hibernate, VM freeze, or
///   admin advanced the clock — positive divergence, caller should
///   shift `last_popup` forward.
/// - `wall_delta << unbiased_delta` : admin rewound the clock — negative
///   divergence, caller should shift `last_popup` backward by the same
///   amount so `now - last_popup` preserves the real elapsed time.
///
/// The threshold gate (default 60 s in service.rs) filters normal 5 s
/// tick jitter and ~15 s pauses in the mpsc receiver from being
/// interpreted as jumps.
pub fn compute_clock_shift(
    prev_wall: SystemTime,
    now_wall: SystemTime,
    unbiased_delta_secs: u64,
    jump_threshold_secs: u64,
) -> Option<i64> {
    let wall_delta_secs: i64 = match now_wall.duration_since(prev_wall) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let divergence = wall_delta_secs.saturating_sub(unbiased_delta_secs as i64);
    if divergence.unsigned_abs() >= jump_threshold_secs {
        Some(divergence)
    } else {
        None
    }
}

/// Add `delta_secs` (signed) to a `SystemTime`. Negative deltas subtract
/// ; if the subtraction would go before `UNIX_EPOCH` the result clamps
/// to the epoch. Explicit UNIX_EPOCH gate — `SystemTime::checked_sub`
/// permits pre-epoch values on Linux (`tv_sec: -N`), which would break
/// downstream `duration_since(UNIX_EPOCH)` unwraps.
pub fn shift_systemtime(base: SystemTime, delta_secs: i64) -> SystemTime {
    if delta_secs >= 0 {
        return base + Duration::from_secs(delta_secs as u64);
    }
    let sub = Duration::from_secs(delta_secs.unsigned_abs());
    match base.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(from_epoch) => from_epoch
            .checked_sub(sub)
            .map(|d| SystemTime::UNIX_EPOCH + d)
            .unwrap_or(SystemTime::UNIX_EPOCH),
        Err(_) => SystemTime::UNIX_EPOCH,
    }
}

/// Preview helper : given a candidate `proposed_interval` the user is
/// dragging in the Config UI, return the wall-clock time at which the
/// next popup would fire IF they clicked Save right now. Shifts the
/// naive deadline by `paused_secs` (active-time accounting), then
/// applies the same `RELOAD_MIN_GRACE` clamp as [`resolve_last_popup`]'s
/// reload branch so the preview stays truthful even when the user
/// reduces the interval into the past.
pub fn next_popup_after_reload(
    now: SystemTime,
    last_popup: SystemTime,
    proposed_interval: Duration,
    paused_secs: u64,
) -> SystemTime {
    let naive = last_popup + proposed_interval + Duration::from_secs(paused_secs);
    let floor = now + RELOAD_MIN_GRACE;
    if naive >= floor {
        naive
    } else {
        floor
    }
}

/// Config UI live-preview kernel : ceil-minutes from `now` to the next
/// popup fire, respecting the reload clamp when `last_popup` is known
/// AND the accumulated `paused_secs` (idle / sleep / boot-off time
/// deducted by the service). Pure — no I/O — so it's Linux-testable.
/// The tabs.rs wrapper injects `SystemTime::now()` + `runtime_dat::read()`
/// at call time.
pub fn preview_minutes(
    now: SystemTime,
    last_popup: Option<SystemTime>,
    interval: Duration,
    paused_secs: u64,
) -> u64 {
    let next = match last_popup {
        Some(lp) => next_popup_after_reload(now, lp, interval, paused_secs),
        None => now + interval,
    };
    let delta = next.duration_since(now).unwrap_or(Duration::ZERO);
    // Ceil so "dans 0 min" never appears when the clamp actually holds.
    (delta.as_secs() + 59) / 60
}

#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerDecision {
    Nothing,
    FireCountdown {
        palier_minutes: u32,
        seconds_until_popup: u64,
    },
    FirePopup,
    /// Fire the daily lunch pause popup. Takes priority over
    /// `FirePopup` when both would trigger on the same tick — the
    /// daily pause is time-anchored while the interval popup will
    /// simply reschedule.
    FireDailyPause,
    /// Fire a pre-notification widget for the daily pause at palier
    /// `palier_minutes` (one of 15, 10, 5). `seconds_until` is the
    /// countdown displayed inside the widget.
    FireDailyCountdown {
        palier_minutes: u32,
        seconds_until: u64,
    },
}

/// Snapshot of the daily-pause state for a single tick — the scheduler
/// is TZ-agnostic ; the caller (`service.rs`) pre-resolves timestamps
/// via `time_authority::in_configured_tz` and the ROLLOUT's H:M schedule.
#[derive(Debug, Clone)]
pub struct DailyPauseState {
    /// Independent kill-switch (`Config::daily_pause.enabled`). When
    /// false, no daily-pause decisions are emitted — the interval
    /// path still runs.
    pub enabled: bool,
    /// True once today's daily pause has fired. Derived from
    /// `runtime.daily_pause_fired_at` compared against `now_local`'s
    /// date. Reset at day boundary by the caller.
    pub fired_today: bool,
    /// Today's fire moment in the configured TZ. Precomputed by the
    /// caller so the scheduler doesn't own the TZ/date arithmetic.
    pub fire_at_today: DateTime<Tz>,
    /// End of the grace window : `fire_at_today + duration_minutes`.
    /// Past this the pause is skipped for the day (Cas C — see
    /// ROLLOUT).
    pub grace_window_end: DateTime<Tz>,
    /// Paliers already fired for today. The caller may pre-populate
    /// with paliers whose windows are already past at boot to prevent
    /// retroactive fires (Cas B in the ROLLOUT).
    pub fired_paliers: HashSet<u32>,
    /// Anti-forkbomb marker (Cas C) : true when today's fire attempt
    /// hit past-grace-window ; scheduler must not retry until tomorrow.
    pub skipped_today: bool,
}

impl Default for DailyPauseState {
    fn default() -> Self {
        // Sentinel values : disabled + fire moment at UNIX_EPOCH-ish.
        // The scheduler short-circuits before consuming the dates
        // because `enabled=false`. Used by service.rs session-1
        // placeholder pass ; session 2 threads real values.
        let epoch = chrono_tz::Europe::Paris
            .with_ymd_and_hms(1970, 1, 1, 0, 0, 0)
            .single()
            .expect("1970-01-01 00:00 Paris is unambiguous");
        Self {
            enabled: false,
            fired_today: false,
            fire_at_today: epoch,
            grace_window_end: epoch,
            fired_paliers: HashSet::new(),
            skipped_today: false,
        }
    }
}

/// Central decision : what should happen right now given the tick's
/// current time, the config, and the recorded last-popup timestamp.
///
/// `paused_secs` shifts the next popup forward by that many seconds,
/// implementing the active-time cycle : time the user spent idle /
/// asleep / booted-off is banked here and doesn't count against the
/// interval.
///
/// `fired_paliers` tracks paliers already spawned in the current cycle.
/// Reset it to empty after a popup fires ; re-derive it via
/// [`derive_already_fired`] after service restart or config reload.
///
/// `popup_pending` is a top-level guard : when true, NO decision is
/// emitted — countdown spawns are suppressed while a popup is up, and
/// even the daily-pause fire waits for the current popup to close
/// (session 2 dispatches an explicit collision-resolution path).
///
/// `daily_pause_state` provides the TZ-pre-resolved state for the
/// daily lunch pause (fire moment, grace window, already-fired
/// paliers). The scheduler stays TZ-agnostic ; the caller in
/// `service.rs` owns the `now_local` computation via
/// `time_authority::in_configured_tz`.
///
/// **Kill-switch scoping** : `Config::disabled` only gates the
/// interval branch — the daily-pause branch honors its own
/// `DailyPauseState::enabled` flag. This mirrors the "Two orthogonal
/// switches" decision in the ROLLOUT.
pub fn next_event(
    now: SystemTime,
    config: &Config,
    last_popup: SystemTime,
    paused_secs: u64,
    fired_paliers: &HashSet<u32>,
    popup_pending: bool,
    daily_pause_state: &DailyPauseState,
    now_local: DateTime<Tz>,
) -> SchedulerDecision {
    // Top-level guard : no fresh spawns while a popup is up. Kills
    // pre-notif countdowns AND defers the daily-pause fire — session 2
    // handles the collision by dismissing the current popup and
    // re-firing on the next tick.
    if popup_pending {
        return SchedulerDecision::Nothing;
    }

    // Daily-pause fire has the highest priority : it's time-anchored
    // and the interval popup can reschedule freely.
    if daily_pause_state.enabled
        && !daily_pause_state.fired_today
        && !daily_pause_state.skipped_today
        && now_local >= daily_pause_state.fire_at_today
        && now_local <= daily_pause_state.grace_window_end
    {
        return SchedulerDecision::FireDailyPause;
    }

    // Daily-pause countdowns — iterate largest → smallest so we emit
    // the tightest-still-relevant palier when the earlier ones have
    // already been fired. Windows are closed once now_local passes
    // fire_at_today (the fire branch above catches that case).
    if daily_pause_state.enabled && !daily_pause_state.fired_today {
        for palier in [15u32, 10, 5] {
            if daily_pause_state.fired_paliers.contains(&palier) {
                continue;
            }
            let palier_moment = daily_pause_state.fire_at_today
                - ChronoDuration::minutes(i64::from(palier));
            if now_local >= palier_moment && now_local < daily_pause_state.fire_at_today {
                let seconds_until = (daily_pause_state.fire_at_today - now_local)
                    .num_seconds()
                    .max(0) as u64;
                return SchedulerDecision::FireDailyCountdown {
                    palier_minutes: palier,
                    seconds_until,
                };
            }
        }
    }

    // Interval branch — scoped kill-switch (Config::disabled only
    // disables intervals per the ROLLOUT "kill-switch separation"
    // decision).
    if config.disabled {
        return SchedulerDecision::Nothing;
    }

    let next = next_popup_at(last_popup, config, paused_secs);

    if now >= next {
        return SchedulerDecision::FirePopup;
    }

    // Paliers are sorted descending (largest = earliest). We fire the
    // first one that has crossed its boundary and hasn't been marked
    // fired yet.
    for palier in sanitize_paliers(config) {
        if fired_paliers.contains(&palier) {
            continue;
        }
        let palier_dur = Duration::from_secs(u64::from(palier) * 60);
        let boundary = match next.checked_sub(palier_dur) {
            Some(b) => b,
            None => continue,
        };
        if now >= boundary {
            let remaining = next.duration_since(now).unwrap_or(Duration::ZERO);
            return SchedulerDecision::FireCountdown {
                palier_minutes: palier,
                seconds_until_popup: remaining.as_secs(),
            };
        }
    }

    SchedulerDecision::Nothing
}

/// Reconstruct the "already fired" palier set after a service restart
/// or config reload. Any palier whose window is already in the past
/// (i.e. `next_popup - now <= palier_minutes`) is marked as fired so
/// we don't retroactively spawn a countdown for it.
pub fn derive_already_fired(
    now: SystemTime,
    config: &Config,
    last_popup: SystemTime,
    paused_secs: u64,
) -> HashSet<u32> {
    let next = next_popup_at(last_popup, config, paused_secs);
    let remaining = match next.duration_since(now) {
        Ok(d) => d,
        Err(_) => return sanitize_paliers(config).into_iter().collect(),
    };

    sanitize_paliers(config)
        .into_iter()
        .filter(|palier| {
            let palier_dur = Duration::from_secs(u64::from(*palier) * 60);
            remaining <= palier_dur
        })
        .collect()
}

/// Compute the next popup time in active-time semantics : the wall-clock
/// deadline is `last_popup + interval + paused_secs`, so time already
/// banked as "not active" pushes the deadline further out.
pub fn next_popup_at(
    last_popup: SystemTime,
    config: &Config,
    paused_secs: u64,
) -> SystemTime {
    last_popup + interval(config) + Duration::from_secs(paused_secs)
}

/// Interval between popups, clamped to safe bounds. Out-of-range values
/// fall back to the default (2h) and log a warning once per call site.
pub fn interval(config: &Config) -> Duration {
    let hours = if config.interval_hours.is_finite()
        && config.interval_hours >= MIN_INTERVAL_HOURS
        && config.interval_hours <= MAX_INTERVAL_HOURS
    {
        config.interval_hours
    } else {
        tracing::warn!(
            got = config.interval_hours,
            "interval_hours out of range [0.1, 168.0], substituting default 2.0"
        );
        DEFAULT_INTERVAL_HOURS
    };
    Duration::from_secs((f64::from(hours) * 3600.0) as u64)
}

/// Sanitize `pre_notification_minutes` — sort descending, dedupe, drop
/// zeros and values that meet or exceed the interval (a palier equal to
/// the interval would fire simultaneously with the previous popup).
pub fn sanitize_paliers(config: &Config) -> Vec<u32> {
    let interval_minutes = (interval(config).as_secs() / 60) as u32;
    let mut paliers: Vec<u32> = config
        .pre_notification_minutes
        .iter()
        .copied()
        .filter(|p| *p > 0 && *p < interval_minutes)
        .collect();
    paliers.sort_unstable_by(|a, b| b.cmp(a));
    paliers.dedup();
    paliers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(interval_hours: f32, paliers: Vec<u32>, disabled: bool) -> Config {
        let mut c = Config::default();
        c.interval_hours = interval_hours;
        c.pre_notification_minutes = paliers;
        c.disabled = disabled;
        c
    }

    fn epoch_plus(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// Thin test helper — supplies default (disabled) daily-pause
    /// state and a `now_local` computed by treating `now` as UTC and
    /// projecting into Europe/Paris. Keeps the interval-focused tests
    /// terse ; the daily-pause tests below build the full state
    /// explicitly.
    fn next_event_intervals_only(
        now: SystemTime,
        cfg: &Config,
        last: SystemTime,
        paused: u64,
        fired: &HashSet<u32>,
    ) -> SchedulerDecision {
        let utc: DateTime<chrono::Utc> = now.into();
        let now_local = utc.with_timezone(&chrono_tz::Europe::Paris);
        next_event(
            now,
            cfg,
            last,
            paused,
            fired,
            false,
            &DailyPauseState::default(),
            now_local,
        )
    }

    /// Build a `DailyPauseState` firing at (H:M) today in `tz` with a
    /// grace window of `duration_minutes`. `now_local` anchors the
    /// "today" date.
    fn daily_state_at(
        tz: Tz,
        now_local: DateTime<Tz>,
        h: u32,
        m: u32,
        duration_minutes: i64,
    ) -> DailyPauseState {
        use chrono::Datelike;
        let fire = tz
            .with_ymd_and_hms(now_local.year(), now_local.month(), now_local.day(), h, m, 0)
            .single()
            .expect("test time is unambiguous");
        DailyPauseState {
            enabled: true,
            fired_today: false,
            fire_at_today: fire,
            grace_window_end: fire + ChronoDuration::minutes(duration_minutes),
            fired_paliers: HashSet::new(),
            skipped_today: false,
        }
    }

    #[test]
    fn disabled_config_returns_nothing() {
        let cfg = config_with(2.0, vec![15, 10, 5], true);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        assert_eq!(
            next_event_intervals_only(now, &cfg, last, 0, &fired),
            SchedulerDecision::Nothing
        );
    }

    #[test]
    fn fresh_start_within_interval_returns_nothing() {
        // interval 2h = 7200s ; only 60s since last popup ; no paliers crossed.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(60);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        assert_eq!(
            next_event_intervals_only(now, &cfg, last, 0, &fired),
            SchedulerDecision::Nothing
        );
    }

    #[test]
    fn interval_elapsed_returns_fire_popup() {
        // interval 2h ; 2h1min elapsed.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(2 * 3600 + 60);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        assert_eq!(
            next_event_intervals_only(now, &cfg, last, 0, &fired),
            SchedulerDecision::FirePopup
        );
    }

    #[test]
    fn exactly_at_boundary_returns_fire_popup() {
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(2 * 3600);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        assert_eq!(
            next_event_intervals_only(now, &cfg, last, 0, &fired),
            SchedulerDecision::FirePopup
        );
    }

    #[test]
    fn palier_15_first_fire() {
        // interval 2h ; 15 minutes before popup ; palier 15 should fire.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let last = epoch_plus(0);
        let next_popup = 2 * 3600;
        let now = epoch_plus(next_popup - 15 * 60);
        let fired = HashSet::new();
        match next_event_intervals_only(now, &cfg, last, 0, &fired) {
            SchedulerDecision::FireCountdown {
                palier_minutes,
                seconds_until_popup,
            } => {
                assert_eq!(palier_minutes, 15);
                assert_eq!(seconds_until_popup, 15 * 60);
            }
            other => panic!("expected FireCountdown{{15}}, got {other:?}"),
        }
    }

    #[test]
    fn palier_already_fired_returns_nothing() {
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let last = epoch_plus(0);
        let next_popup = 2 * 3600;
        let now = epoch_plus(next_popup - 14 * 60); // past T-15, before T-10.
        let mut fired = HashSet::new();
        fired.insert(15);
        assert_eq!(
            next_event_intervals_only(now, &cfg, last, 0, &fired),
            SchedulerDecision::Nothing
        );
    }

    #[test]
    fn palier_15_and_10_both_past_returns_15_first() {
        // now = T-9 (past both T-15 and T-10). If neither fired yet, we
        // return the earliest one (15) so the widget spawn order matches
        // wall-clock order.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let last = epoch_plus(0);
        let next_popup = 2 * 3600;
        let now = epoch_plus(next_popup - 9 * 60);
        let fired = HashSet::new();
        match next_event_intervals_only(now, &cfg, last, 0, &fired) {
            SchedulerDecision::FireCountdown { palier_minutes, .. } => {
                assert_eq!(palier_minutes, 15);
            }
            other => panic!("expected FireCountdown{{15}}, got {other:?}"),
        }
    }

    #[test]
    fn custom_paliers_out_of_order_still_work() {
        // Config lists paliers in ascending order — sanitize sorts desc.
        let cfg = config_with(2.0, vec![5, 15, 10], false);
        let last = epoch_plus(0);
        let now = epoch_plus(2 * 3600 - 15 * 60);
        let fired = HashSet::new();
        match next_event_intervals_only(now, &cfg, last, 0, &fired) {
            SchedulerDecision::FireCountdown { palier_minutes, .. } => {
                assert_eq!(palier_minutes, 15);
            }
            other => panic!("expected FireCountdown{{15}}, got {other:?}"),
        }
    }

    #[test]
    fn palier_zero_dropped() {
        let cfg = config_with(2.0, vec![0, 15], false);
        let paliers = sanitize_paliers(&cfg);
        assert_eq!(paliers, vec![15]);
    }

    #[test]
    fn palier_equal_to_interval_dropped() {
        // interval 2h = 120 min ; palier 120 should be dropped (would
        // fire simultaneously with the previous popup, nonsensical).
        let cfg = config_with(2.0, vec![120, 15, 10], false);
        let paliers = sanitize_paliers(&cfg);
        assert_eq!(paliers, vec![15, 10]);
    }

    #[test]
    fn duplicate_paliers_deduped() {
        let cfg = config_with(2.0, vec![15, 15, 10, 10, 5], false);
        let paliers = sanitize_paliers(&cfg);
        assert_eq!(paliers, vec![15, 10, 5]);
    }

    #[test]
    fn interval_out_of_range_falls_back_to_default() {
        let cfg = config_with(1e9, vec![15, 10, 5], false);
        assert_eq!(interval(&cfg), Duration::from_secs(2 * 3600));
    }

    #[test]
    fn negative_interval_falls_back_to_default() {
        let cfg = config_with(-1.0, vec![15, 10, 5], false);
        assert_eq!(interval(&cfg), Duration::from_secs(2 * 3600));
    }

    #[test]
    fn derive_already_fired_at_start_after_t_minus_8() {
        // Service restarts at T-8 : paliers 15 and 10 already crossed
        // their windows, palier 5 has not.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let last = epoch_plus(0);
        let now = epoch_plus(2 * 3600 - 8 * 60);
        let fired = derive_already_fired(now, &cfg, last, 0);
        let expected: HashSet<u32> = vec![15, 10].into_iter().collect();
        assert_eq!(fired, expected);
    }

    #[test]
    fn derive_already_fired_at_cycle_start() {
        // Right after popup fired (or fresh install) : nothing crossed.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let last = epoch_plus(0);
        let now = epoch_plus(60);
        let fired = derive_already_fired(now, &cfg, last, 0);
        assert!(fired.is_empty());
    }

    #[test]
    fn derive_already_fired_after_added_palier_20_at_t_minus_5() {
        // Config was reloaded and a new palier 20 was added while
        // already inside T-5. Ensure 20 is marked fired (we won't
        // retroactively spawn its widget).
        let cfg = config_with(2.0, vec![20, 15, 10, 5], false);
        let last = epoch_plus(0);
        let now = epoch_plus(2 * 3600 - 4 * 60);
        let fired = derive_already_fired(now, &cfg, last, 0);
        assert!(fired.contains(&20));
        assert!(fired.contains(&15));
        assert!(fired.contains(&10));
        assert!(fired.contains(&5));
    }

    #[test]
    fn microsleep_between_ticks_does_not_double_fire() {
        // If a popup fires, caller updates last_popup + resets fired.
        // Next tick within the interval should return Nothing.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(2 * 3600 + 1);
        let last = now;
        let fired = HashSet::new();
        assert_eq!(
            next_event_intervals_only(now, &cfg, last, 0, &fired),
            SchedulerDecision::Nothing
        );
    }

    // --- resolve_last_popup : 13 tests covering the full decision tree ---

    #[test]
    fn resolve_reload_no_reduction_leaves_last_popup_unchanged() {
        // Interval 2h ; last popup 30min ago ; user reloads but didn't
        // change interval. lp + 2h = now + 90min >> now + 2min ⇒ keep lp.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 30 * 60);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: true,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, lp);
    }

    #[test]
    fn resolve_reload_reduced_interval_landing_in_past_clamps_to_now_plus_2min() {
        // Interval reduced from 2h → 45min. Last popup 2h ago ⇒
        // lp + 45min lands 75min in the past. Clamp to now + 2min.
        let cfg = config_with(0.75, vec![], false); // 45 min
        let now = epoch_plus(3 * 3600);
        let lp = epoch_plus(3 * 3600 - 2 * 3600);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: true,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        // got + 45min should equal now + 2min.
        assert_eq!(got + Duration::from_secs(45 * 60), now + RELOAD_MIN_GRACE);
    }

    #[test]
    fn resolve_reload_reduced_landing_within_2min_clamps_to_now_plus_2min() {
        // Interval reduced to 45 min, lp was 44 min ago ⇒ next = now+1min.
        // Under floor (2min) ⇒ clamp so next = now+2min.
        let cfg = config_with(0.75, vec![], false);
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 44 * 60);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: true,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got + Duration::from_secs(45 * 60), now + RELOAD_MIN_GRACE);
    }

    #[test]
    fn resolve_reload_reduced_still_beyond_2min_no_clamp() {
        // 45 min interval, lp 30 min ago ⇒ next = now+15min. Above floor ⇒ keep lp.
        let cfg = config_with(0.75, vec![], false);
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 30 * 60);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: true,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, lp);
    }

    #[test]
    fn resolve_fresh_install_within_grace_returns_now() {
        // first_install_at 30s ago, no runtime.dat ⇒ genuine fresh install.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(1_700_000_030);
        let first = epoch_plus(1_700_000_000);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: None,
                first_install_at: first,
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, now);
    }

    #[test]
    fn resolve_fresh_install_grace_expired_returns_cold_start_shift() {
        // first_install_at 10 min ago (> FRESH_INSTALL_GRACE = 5min) ⇒
        // migrated / stale ⇒ legacy shift.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(1_700_000_000 + 10 * 60);
        let first = epoch_plus(1_700_000_000);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: None,
                first_install_at: first,
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        let expected = now - (interval(&cfg) - COLD_START_GRACE);
        assert_eq!(got, expected);
    }

    #[test]
    fn resolve_zero_sentinel_no_runtime_returns_cold_start_shift() {
        // UNIX_EPOCH sentinel = migrated 0.6.x install without stamp ⇒ legacy shift.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(1_700_000_000);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: None,
                first_install_at: SystemTime::UNIX_EPOCH,
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        let expected = now - (interval(&cfg) - COLD_START_GRACE);
        assert_eq!(got, expected);
    }

    #[test]
    fn resolve_normal_boot_recent_last_popup_continues_cycle() {
        // Elapsed 30min, interval 2h ⇒ downtime is fine, keep lp.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 30 * 60);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, lp);
    }

    #[test]
    fn resolve_boot_boundary_below_2x_continues() {
        // Elapsed = 2 * interval - 1s ⇒ below reset threshold ⇒ keep lp.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let two_x = 2 * 2 * 3600;
        let now = epoch_plus(two_x);
        let lp = epoch_plus(1); // elapsed = two_x - 1
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, lp);
    }

    #[test]
    fn resolve_boot_boundary_at_2x_resets_to_now() {
        // Elapsed == 2 * interval exactly ⇒ reset.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let two_x = 2 * 2 * 3600;
        let now = epoch_plus(two_x + 100);
        let lp = epoch_plus(100);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, now);
    }

    #[test]
    fn resolve_boot_after_long_downtime_resets_to_now() {
        // Elapsed 10h, interval 2h ⇒ way past 2x ⇒ reset.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(10 * 3600 + 500);
        let lp = epoch_plus(500);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, now);
    }

    #[test]
    fn resolve_disabled_config_still_returns_last_popup() {
        // Resolver is time-only ; the tick loop is responsible for the
        // disabled bail.
        let cfg = config_with(2.0, vec![15, 10, 5], true);
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 30 * 60);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: Some(lp),
                first_install_at: epoch_plus(0),
                is_reload: false,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, lp);
    }

    #[test]
    fn resolve_reload_without_last_popup_treats_as_fresh() {
        // Defensive : Reload before service ever wrote runtime.dat.
        let cfg = config_with(2.0, vec![], false);
        let now = epoch_plus(10_000);
        let got = resolve_last_popup(
            ResolveInputs {
                now,
                last_popup: None,
                first_install_at: epoch_plus(0),
                is_reload: true,
                paused_secs: 0,
                popup_pending: false,
                last_heartbeat: SystemTime::UNIX_EPOCH,
                was_clean_shutdown: false,
            },
            &cfg,
        );
        assert_eq!(got, now);
    }

    // --- next_popup_after_reload : Config UI live preview helper ---

    #[test]
    fn next_popup_after_reload_above_grace_passes_through() {
        // lp 30 min ago, proposed interval 45 min ⇒ next = now+15min.
        // Above 2 min floor ⇒ return naive next.
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 30 * 60);
        let got = next_popup_after_reload(now, lp, Duration::from_secs(45 * 60), 0);
        assert_eq!(got, lp + Duration::from_secs(45 * 60));
    }

    #[test]
    fn next_popup_after_reload_below_grace_clamps_to_floor() {
        // lp 44 min ago, proposed interval 45 min ⇒ next = now+1min.
        // Under 2 min floor ⇒ clamp to now + 2min.
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 44 * 60);
        let got = next_popup_after_reload(now, lp, Duration::from_secs(45 * 60), 0);
        assert_eq!(got, now + RELOAD_MIN_GRACE);
    }

    #[test]
    fn next_popup_after_reload_naive_in_past_clamps_to_floor() {
        // lp 2h ago, proposed interval 45 min ⇒ next = 75 min in the past.
        // Clamp to now + RELOAD_MIN_GRACE.
        let now = epoch_plus(3 * 3600);
        let lp = epoch_plus(3600);
        let got = next_popup_after_reload(now, lp, Duration::from_secs(45 * 60), 0);
        assert_eq!(got, now + RELOAD_MIN_GRACE);
    }

    #[test]
    fn next_popup_after_reload_exactly_at_floor_no_clamp() {
        // lp + interval == now + RELOAD_MIN_GRACE exactly. Boundary case.
        let now = epoch_plus(10_000);
        let interval = Duration::from_secs(45 * 60);
        let lp = (now + RELOAD_MIN_GRACE)
            .checked_sub(interval)
            .unwrap();
        let got = next_popup_after_reload(now, lp, interval, 0);
        assert_eq!(got, lp + interval);
    }

    #[test]
    fn next_popup_after_reload_shifts_by_paused_secs() {
        // lp 30 min ago, interval 45 min, paused 10 min ⇒ next = lp + 55 min
        // (well above the grace floor).
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 30 * 60);
        let paused = 10 * 60;
        let got = next_popup_after_reload(now, lp, Duration::from_secs(45 * 60), paused);
        assert_eq!(got, lp + Duration::from_secs(45 * 60) + Duration::from_secs(paused));
    }

    // -------- preview_minutes (Track A-2) --------

    #[test]
    fn preview_minutes_above_grace_reflects_naive_delta() {
        let now = epoch_plus(10_000);
        let lp = epoch_plus(9_400); // 10 min ago
        let interval = Duration::from_secs(60 * 60); // 1h
        // next = lp + 1h = epoch+13_000 ; delta = 3_000 s = 50 min.
        assert_eq!(preview_minutes(now, Some(lp), interval, 0), 50);
    }

    #[test]
    fn preview_minutes_below_grace_clamps_to_reload_min_grace() {
        let now = epoch_plus(10_000);
        let lp = epoch_plus(9_000); // ~17 min ago
        let interval = Duration::from_secs(60); // 1 min — well below grace
        // Naive next = lp + 60 = epoch+9_060, in the past.
        // Clamped to now + RELOAD_MIN_GRACE (120 s) → 2 min preview.
        assert_eq!(preview_minutes(now, Some(lp), interval, 0), 2);
    }

    #[test]
    fn preview_minutes_none_last_popup_uses_fresh_install_fallback() {
        let now = epoch_plus(10_000);
        let interval = Duration::from_secs(45 * 60); // 45 min
        assert_eq!(preview_minutes(now, None, interval, 0), 45);
    }

    #[test]
    fn preview_minutes_one_minute_interval_boundary() {
        // Fresh install, 1-min interval → exactly 1 min preview.
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(60), 0), 1);
    }

    #[test]
    fn preview_minutes_twelve_hour_interval_boundary() {
        // Fresh install, 720-min (12 h) interval → 720 min preview.
        let now = epoch_plus(10_000);
        assert_eq!(
            preview_minutes(now, None, Duration::from_secs(720 * 60), 0),
            720
        );
    }

    #[test]
    fn preview_minutes_past_next_returns_grace_never_negative() {
        // Regression guard for the "-4 min" bug : any `last_popup + interval`
        // in the past MUST clamp to the RELOAD_MIN_GRACE floor, never produce
        // a negative or wrap-around value.
        let now = epoch_plus(1_000_000);
        let lp = epoch_plus(100_000); // 6 h ago
        let interval = Duration::from_secs(60 * 60); // 1 h — naive next is 5 h in past
        let out = preview_minutes(now, Some(lp), interval, 0);
        // Clamped to RELOAD_MIN_GRACE = 120 s → 2 min preview.
        assert_eq!(out, 2);
    }

    #[test]
    fn preview_minutes_shifts_forward_by_paused_secs() {
        // User idle 10 min accumulated → preview shows deadline 10 min later
        // than the wall-clock naive delta would suggest.
        let now = epoch_plus(10_000);
        let lp = epoch_plus(9_400); // 10 min ago
        let interval = Duration::from_secs(60 * 60); // 1h
        let paused = 10 * 60;
        // Without paused_secs : 50 min. With paused_secs = 10 min : 60 min.
        assert_eq!(preview_minutes(now, Some(lp), interval, paused), 60);
    }

    #[test]
    fn preview_minutes_paused_secs_rescues_past_deadline() {
        // Naive next in the past → would clamp to 2 min. Paused_secs pushes
        // it back into the future, no clamping needed.
        let now = epoch_plus(10_000);
        let lp = epoch_plus(9_000); // 17 min ago
        let interval = Duration::from_secs(60); // 1 min — naive next in past
        let paused = 20 * 60; // 20 min of paused pushes next well ahead
        // next = lp + 60 + 1200 = epoch+10_260 ; delta = 260 s → ceil 5 min.
        assert_eq!(preview_minutes(now, Some(lp), interval, paused), 5);
    }

    // -------- ceil-boundary edges (Track A-3) --------
    // The `(delta.as_secs() + 59) / 60` ceil MUST hold at exact-multiple
    // boundaries so the C-2 "dans 2 min" display never regresses silently.

    #[test]
    fn preview_ceil_60_seconds_reads_as_1_minute() {
        let now = epoch_plus(10_000);
        // Fresh install + 60 s interval = delta 60 s exactly → 1 min.
        assert_eq!(preview_minutes(now, None, Duration::from_secs(60), 0), 1);
    }

    #[test]
    fn preview_ceil_61_seconds_reads_as_2_minutes() {
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(61), 0), 2);
    }

    #[test]
    fn preview_ceil_119_seconds_reads_as_2_minutes() {
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(119), 0), 2);
    }

    #[test]
    fn preview_ceil_120_seconds_reads_as_2_minutes() {
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(120), 0), 2);
    }

    // -------- next_popup_at active-time shift --------

    #[test]
    fn next_popup_at_zero_paused_matches_wall_clock() {
        // Regression guard : the pre-active-time behaviour must be
        // reproducible with paused_secs = 0.
        let cfg = config_with(2.0, vec![], false);
        let lp = epoch_plus(1_000_000);
        let got = next_popup_at(lp, &cfg, 0);
        assert_eq!(got, lp + Duration::from_secs(2 * 3600));
    }

    #[test]
    fn next_popup_at_shifts_by_paused_secs() {
        // 1 h paused_secs pushes the deadline exactly 1 h out.
        let cfg = config_with(2.0, vec![], false);
        let lp = epoch_plus(1_000_000);
        let got = next_popup_at(lp, &cfg, 3600);
        assert_eq!(got, lp + Duration::from_secs(2 * 3600 + 3600));
    }

    #[test]
    fn next_event_paused_secs_defers_fire_decision() {
        // interval 2 h, elapsed 2 h + 1 s (would normally FirePopup),
        // but paused_secs = 30 min pushes the deadline to lp + 2.5 h.
        // Now we're still 29 min 59 s short → Nothing.
        let cfg = config_with(2.0, vec![], false);
        let last = epoch_plus(0);
        let now = epoch_plus(2 * 3600 + 1);
        let fired = HashSet::new();
        assert_eq!(
            next_event_intervals_only(now, &cfg, last, 30 * 60, &fired),
            SchedulerDecision::Nothing,
        );
    }

    // -------- resolve_cold_start : threshold / anti-bypass / credit --------

    fn cold_inputs(
        now_secs: u64,
        lp_secs: u64,
        hb_secs: u64,
        paused_secs: u64,
        popup_pending: bool,
        was_clean_shutdown: bool,
    ) -> ResolveInputs {
        ResolveInputs {
            now: epoch_plus(now_secs),
            last_popup: Some(epoch_plus(lp_secs)),
            first_install_at: SystemTime::UNIX_EPOCH,
            is_reload: false,
            paused_secs,
            popup_pending,
            last_heartbeat: epoch_plus(hb_secs),
            was_clean_shutdown,
        }
    }

    #[test]
    fn resolve_cold_start_elapsed_above_threshold_full_reset() {
        // elapsed = threshold + 10 min → reset. Downtime and paused_pre
        // irrelevant on this branch.
        let cfg = config_with(2.0, vec![], false);
        let threshold_secs = (cfg.reset_threshold_hours * 3600.0) as u64;
        let now_secs = 100_000;
        let lp_secs = now_secs - (threshold_secs + 10 * 60);
        let hb_secs = now_secs - 60;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 1234, false, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(now_secs));
        assert_eq!(dec.paused_secs, 0);
        assert!(!dec.popup_pending);
    }

    #[test]
    fn resolve_cold_start_pending_unclean_shutdown_triggers_anti_bypass_full_reset() {
        // elapsed well below threshold, popup_pending=true, unclean
        // shutdown (was_clean_shutdown=false) → full reset. Kid killed
        // the service mid-popup ; anti-bypass fires.
        let cfg = config_with(2.0, vec![], false);
        let now_secs = 100_000;
        let lp_secs = now_secs - 5 * 60; // 5 min elapsed, far below 2 h.
        let hb_secs = now_secs - 30;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, true, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(now_secs));
        assert_eq!(dec.paused_secs, 0);
        assert!(!dec.popup_pending);
        assert!(!dec.relaunch_now);
    }

    #[test]
    fn resolve_cold_start_pending_clean_shutdown_triggers_relaunch() {
        // elapsed below threshold, popup_pending=true, CLEAN shutdown
        // (was_clean_shutdown=true) → user rebooted PC mid-popup ;
        // resume the popup on next tick. Keep last_popup, keep pending,
        // signal relaunch_now, don't credit downtime (the popup itself
        // will terminate the cycle when dismissed).
        let cfg = config_with(2.0, vec![], false);
        let now_secs = 100_000;
        let lp_secs = now_secs - 5 * 60;
        let hb_secs = now_secs - 30;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 400, true, true);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(lp_secs));
        assert_eq!(dec.paused_secs, 400); // unchanged, no downtime credit
        assert!(dec.popup_pending);
        assert!(dec.relaunch_now);
    }

    #[test]
    fn resolve_cold_start_pending_clean_shutdown_long_downtime_full_reset() {
        // popup_pending + clean shutdown BUT downtime >= duration_minutes*60.
        // Absence covered the break already ; full reset instead of
        // relaunching a popup for a break the user already took.
        let mut cfg = config_with(2.0, vec![], false);
        cfg.duration_minutes = 1; // popup would force 60 s of break.
        let now_secs = 100_000;
        let lp_secs = now_secs - 5 * 60; // 5 min elapsed, below threshold.
        let hb_secs = now_secs - 120; // 2 min absence, above the 60 s bar.
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, true, true);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(now_secs));
        assert_eq!(dec.paused_secs, 0);
        assert!(!dec.popup_pending);
        assert!(!dec.relaunch_now);
    }

    #[test]
    fn resolve_cold_start_pending_clean_shutdown_downtime_exactly_at_duration_full_reset() {
        // Boundary : downtime == duration_minutes*60 exactly → full reset
        // (inclusive bound, matches the ">= duration" semantics).
        let mut cfg = config_with(2.0, vec![], false);
        cfg.duration_minutes = 1;
        let now_secs = 100_000;
        let lp_secs = now_secs - 5 * 60;
        let hb_secs = now_secs - 60; // exactly the popup duration.
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, true, true);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(now_secs));
        assert!(!dec.relaunch_now);
    }

    #[test]
    fn resolve_cold_start_pending_clean_shutdown_short_downtime_still_relaunches() {
        // popup_pending + clean shutdown + downtime BELOW duration →
        // relaunch (the standard rule 4 case). Guards against my
        // "duration" gate over-firing.
        let mut cfg = config_with(2.0, vec![], false);
        cfg.duration_minutes = 5; // 300 s bar.
        let now_secs = 100_000;
        let lp_secs = now_secs - 5 * 60;
        let hb_secs = now_secs - 30; // 30 s absence, well below 300 s.
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 400, true, true);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(lp_secs));
        assert_eq!(dec.paused_secs, 400);
        assert!(dec.popup_pending);
        assert!(dec.relaunch_now);
    }

    #[test]
    fn resolve_cold_start_threshold_takes_priority_over_relaunch() {
        // elapsed > threshold AND pending=true AND clean shutdown →
        // threshold rule fires FIRST (highest priority). Full reset,
        // no relaunch, even though the "clean+pending" signature would
        // otherwise trigger relaunch.
        let cfg = config_with(2.0, vec![], false);
        let threshold_secs = (cfg.reset_threshold_hours * 3600.0) as u64;
        let now_secs = 100_000;
        let lp_secs = now_secs - (threshold_secs + 60);
        let hb_secs = now_secs - 30;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, true, true);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(now_secs));
        assert!(!dec.popup_pending);
        assert!(!dec.relaunch_now);
    }

    #[test]
    fn resolve_cold_start_no_reset_credits_downtime_to_paused_secs() {
        // elapsed 30 min < 2 h threshold, pending=false, downtime=27000 s
        // (7.5 h suspend without power broadcast — the reported bug).
        // last_popup unchanged, paused_secs += 27000.
        let cfg = config_with(2.0, vec![], false);
        let now_secs = 1_000_000;
        let lp_secs = now_secs - 30 * 60;
        let hb_secs = now_secs - 27_000;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 100, false, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(lp_secs));
        assert_eq!(dec.paused_secs, 100 + 27_000);
        assert!(!dec.popup_pending);
    }

    #[test]
    fn resolve_cold_start_zero_downtime_no_op() {
        // heartbeat fresh + elapsed 0 → last_popup unchanged, paused_secs
        // unchanged. Edge case : service restarts within one tick.
        let cfg = config_with(2.0, vec![], false);
        let now_secs = 100_000;
        let lp_secs = now_secs;
        let hb_secs = now_secs;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, false, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(lp_secs));
        assert_eq!(dec.paused_secs, 0);
    }

    #[test]
    fn resolve_cold_start_threshold_exactly_at_boundary_hits_reset() {
        // elapsed == threshold exactly → reset (inclusive boundary).
        let cfg = config_with(2.0, vec![], false);
        let threshold_secs = (cfg.reset_threshold_hours * 3600.0) as u64;
        let now_secs = 100_000;
        let lp_secs = now_secs - threshold_secs;
        let hb_secs = now_secs - 60;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, false, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(now_secs));
        assert_eq!(dec.paused_secs, 0);
    }

    #[test]
    fn resolve_cold_start_custom_reset_threshold_hours_wires_through() {
        // Set threshold to 6 h explicitly ; elapsed = 5 h → below,
        // credit downtime instead of reset. Proves the field is read.
        let mut cfg = config_with(2.0, vec![], false);
        cfg.reset_threshold_hours = 6.0;
        let now_secs = 1_000_000;
        let lp_secs = now_secs - 5 * 3600;
        let hb_secs = now_secs - 1000;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, false, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(lp_secs));
        assert_eq!(dec.paused_secs, 1000);
    }

    #[test]
    fn resolve_cold_start_clock_skew_last_heartbeat_in_future_treats_downtime_as_zero() {
        // last_heartbeat > now (NTP resync during boot) → downtime clamped
        // to 0, no negative subtraction, paused_secs unchanged.
        let cfg = config_with(2.0, vec![], false);
        let now_secs = 100_000;
        let lp_secs = now_secs - 3600;
        let hb_secs = now_secs + 10_000; // future.
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 500, false, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.paused_secs, 500);
    }

    #[test]
    fn resolve_cold_start_clock_skew_last_popup_in_future_no_reset() {
        // last_popup > now → elapsed clamped to 0, below threshold,
        // pending=false → credit downtime branch. last_popup unchanged.
        let cfg = config_with(2.0, vec![], false);
        let now_secs = 100_000;
        let lp_secs = now_secs + 10_000; // future.
        let hb_secs = now_secs - 60;
        let inputs = cold_inputs(now_secs, lp_secs, hb_secs, 0, false, false);
        let dec = resolve_cold_start(inputs, &cfg);
        assert_eq!(dec.last_popup, epoch_plus(lp_secs));
        assert!(!dec.popup_pending);
    }

    // -------- compute_clock_shift + shift_systemtime --------

    const JUMP_THRESHOLD: u64 = 60;

    #[test]
    fn clock_shift_normal_tick_returns_none() {
        // wall 5 s, unbiased 5 s → divergence 0 → below threshold.
        let prev = epoch_plus(1_000);
        let now = epoch_plus(1_005);
        assert_eq!(compute_clock_shift(prev, now, 5, JUMP_THRESHOLD), None);
    }

    #[test]
    fn clock_shift_mild_noise_below_threshold_returns_none() {
        // wall 15 s, unbiased 5 s → divergence 10 s. Below 60 s guard —
        // don't false-positive on receiver latency.
        let prev = epoch_plus(1_000);
        let now = epoch_plus(1_015);
        assert_eq!(compute_clock_shift(prev, now, 5, JUMP_THRESHOLD), None);
    }

    #[test]
    fn clock_shift_suspend_returns_positive_divergence() {
        // wall jumped 27 000 s (7.5 h suspend), unbiased 5 s.
        let prev = epoch_plus(1_000);
        let now = epoch_plus(1_000 + 27_000);
        assert_eq!(
            compute_clock_shift(prev, now, 5, JUMP_THRESHOLD),
            Some(26_995),
        );
    }

    #[test]
    fn clock_shift_admin_advance_returns_positive_divergence() {
        // Admin bumps clock forward 3 h. unbiased still 5 s.
        let prev = epoch_plus(1_000);
        let now = epoch_plus(1_000 + 10_800);
        assert_eq!(
            compute_clock_shift(prev, now, 5, JUMP_THRESHOLD),
            Some(10_795),
        );
    }

    #[test]
    fn clock_shift_admin_rewind_returns_negative_divergence() {
        // Admin rolls clock back 3 h. Wall delta = -10 800 s ; unbiased
        // still advanced 5 s (monotonic).
        let prev = epoch_plus(20_000);
        let now = epoch_plus(20_000 - 10_800);
        assert_eq!(
            compute_clock_shift(prev, now, 5, JUMP_THRESHOLD),
            Some(-10_805),
        );
    }

    #[test]
    fn clock_shift_realigned_after_tampering_returns_none() {
        // Guards false positive : after a jump was compensated, the
        // next normal tick must NOT re-trigger. wall 5 s, unbiased 5 s.
        let prev = epoch_plus(20_000);
        let now = epoch_plus(20_005);
        assert_eq!(compute_clock_shift(prev, now, 5, JUMP_THRESHOLD), None);
    }

    #[test]
    fn clock_shift_degenerate_unbiased_larger_than_wall_below_threshold_none() {
        // Impossible in practice (unbiased running ahead of wall by 15 s)
        // but the guard must still hold : |divergence| = 10 s < 60 s.
        let prev = epoch_plus(1_000);
        let now = epoch_plus(1_005);
        assert_eq!(compute_clock_shift(prev, now, 15, JUMP_THRESHOLD), None);
    }

    #[test]
    fn shift_systemtime_positive_adds_duration() {
        let base = epoch_plus(1_000);
        assert_eq!(shift_systemtime(base, 30), epoch_plus(1_030));
    }

    #[test]
    fn shift_systemtime_negative_subtracts_duration() {
        let base = epoch_plus(1_000);
        assert_eq!(shift_systemtime(base, -30), epoch_plus(970));
    }

    #[test]
    fn shift_systemtime_negative_past_epoch_clamps_to_epoch() {
        let base = epoch_plus(100);
        assert_eq!(shift_systemtime(base, -10_000), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn shift_systemtime_zero_returns_base() {
        let base = epoch_plus(1_000);
        assert_eq!(shift_systemtime(base, 0), base);
    }

    // === Daily-pause scheduler tests ===

    fn paris_at(y: i32, mo: u32, d: u32, h: u32, m: u32) -> DateTime<Tz> {
        chrono_tz::Europe::Paris
            .with_ymd_and_hms(y, mo, d, h, m, 0)
            .single()
            .expect("test wall time is unambiguous in Paris")
    }

    #[test]
    fn daily_pause_fires_at_configured_hour() {
        let cfg = config_with(2.0, vec![15, 10, 5, 1], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 0);
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::FireDailyPause);
    }

    #[test]
    fn daily_pause_skipped_when_popup_pending() {
        let cfg = config_with(2.0, vec![15, 10, 5, 1], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 0);
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, true, &state, now_local);
        assert_eq!(decision, SchedulerDecision::Nothing);
    }

    #[test]
    fn daily_pause_countdown_skips_fired_palier() {
        let cfg = config_with(2.0, vec![], false); // interval paliers irrelevant
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 11, 55); // T-5min from 12:00
        let mut state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        // Paliers 15 and 10 already fired earlier — we should get 5.
        state.fired_paliers.insert(15);
        state.fired_paliers.insert(10);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        match decision {
            SchedulerDecision::FireDailyCountdown { palier_minutes, .. } => {
                assert_eq!(palier_minutes, 5);
            }
            other => panic!("expected FireDailyCountdown{{palier=5}}, got {other:?}"),
        }
    }

    #[test]
    fn daily_pause_disabled_returns_no_daily_events() {
        // With daily-pause disabled, the scheduler ignores the daily
        // branch entirely — the interval path (also disabled here via
        // config.disabled) drops through to Nothing.
        let cfg = config_with(2.0, vec![15, 10, 5], true);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 0);
        let mut state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        state.enabled = false;
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::Nothing);
    }

    #[test]
    fn daily_pause_fires_on_weekend() {
        // 2026-03-14 is a Saturday. Fire must still trigger — the
        // daily pause has no day-of-week filter (per ROLLOUT).
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 14, 12, 0);
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::FireDailyPause);
    }

    #[test]
    fn daily_pause_non_paris_tz_fires_at_local_noon() {
        // Fire at 12:00 America/New_York. Same scheduler logic — the
        // caller (session 2) picks the TZ ; the scheduler just
        // compares DateTime<Tz> values.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = chrono_tz::America::New_York
            .with_ymd_and_hms(2026, 3, 15, 12, 0, 0)
            .single()
            .unwrap();
        let state = daily_state_at(chrono_tz::America::New_York, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::FireDailyPause);
    }

    #[test]
    fn daily_pause_grace_window_at_29_min_fires() {
        let cfg = config_with(2.0, vec![], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 29);
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::FireDailyPause);
    }

    #[test]
    fn daily_pause_grace_window_at_31_min_does_not_fire() {
        // Past the grace window (12:00 + 30min = 12:30) → no fire.
        // The caller (session 2) sets skipped_today after seeing
        // Nothing here, so the next tick doesn't re-attempt.
        // Intervals disabled so the assertion isolates the daily branch.
        let cfg = config_with(2.0, vec![], true);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 31);
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::Nothing);
    }

    #[test]
    fn daily_pause_priority_over_interval_popup() {
        // Both branches would fire on the same tick — daily pause
        // wins per the ROLLOUT ("time-anchored takes priority ; the
        // interval popup will simply reschedule").
        let cfg = config_with(0.001, vec![], false); // interval = 3.6s → immediately due
        let now = epoch_plus(10_000);
        let last = epoch_plus(0); // last popup was long ago
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 0);
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::FireDailyPause);
    }

    #[test]
    fn daily_pause_skipped_today_blocks_retry() {
        // Intervals disabled so the assertion isolates the daily branch.
        let cfg = config_with(2.0, vec![], true);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 15);
        let mut state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        state.skipped_today = true;
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::Nothing);
    }

    #[test]
    fn daily_pause_boot_mid_grace_window_fires_not_stale_countdown() {
        // Cas B : PC boots at 12h05 with the caller pre-populating
        // paliers 15+10 as "already past" (the ROLLOUT anti-retro
        // rule). The scheduler must return FireDailyPause directly,
        // NOT a stale countdown for palier 5 (which is also past
        // relative to fire_at_today, so its window has closed).
        let cfg = config_with(2.0, vec![], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 5);
        let mut state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        state.fired_paliers.insert(15);
        state.fired_paliers.insert(10);
        // Note : palier 5's window is [11:55, 12:00) ; now_local=12:05
        // is past 12:00 so the countdown branch's `< fire_at_today`
        // check already excludes it. The fire branch takes precedence.
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::FireDailyPause);
    }

    #[test]
    fn daily_pause_use_system_clock_true_still_works_from_caller_side() {
        // use_system_clock is a caller-side concern (it picks the
        // wall clock feeding time_authority::in_configured_tz). The
        // scheduler receives a valid `now_local` regardless — so a
        // fire scenario constructed with any valid DateTime<Tz>
        // fires as expected.
        let cfg = config_with(2.0, vec![], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 12, 0);
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        let decision = next_event(now, &cfg, last, 0, &fired, false, &state, now_local);
        assert_eq!(decision, SchedulerDecision::FireDailyPause);
    }

    #[test]
    fn daily_pause_countdown_palier_15_fires_at_boundary() {
        let cfg = config_with(2.0, vec![], false);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        let now_local = paris_at(2026, 3, 15, 11, 45); // exactly T-15
        let state = daily_state_at(chrono_tz::Europe::Paris, now_local, 12, 0, 30);
        match next_event(now, &cfg, last, 0, &fired, false, &state, now_local) {
            SchedulerDecision::FireDailyCountdown {
                palier_minutes,
                seconds_until,
            } => {
                assert_eq!(palier_minutes, 15);
                assert_eq!(seconds_until, 15 * 60);
            }
            other => panic!("expected FireDailyCountdown{{palier=15}}, got {other:?}"),
        }
    }
}
