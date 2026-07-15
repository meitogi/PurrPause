// Pure scheduling logic — decides whether the service should fire a
// popup, a countdown palier, or do nothing, given the current wall-clock
// time and the last popup timestamp.
//
// Fully unit-testable on Linux : all time is passed in as `SystemTime`,
// no calls to `SystemTime::now()` or any Win32 API. The service loop
// glues this together with the actual tick / spawn plumbing.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

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

/// Inputs for [`resolve_last_popup`] - the decision that answers "what
/// value should `last_popup` hold right now given cold-start context /
/// reload / fresh-install detection".
///
/// - `now` : wall clock at the decision moment.
/// - `last_popup` : `None` when `runtime.dat` is missing, `Some(t)`
///   otherwise.
/// - `first_install_at` : from `Config::first_install_at()`. Sentinel
///   `UNIX_EPOCH` means "unknown / migrated 0.6.x install."
/// - `is_reload` : `true` only on IPC `Reload`. Switches the branch
///   from cold-start decisions to the reduce-interval clamp.
pub struct ResolveInputs {
    pub now: SystemTime,
    pub last_popup: Option<SystemTime>,
    pub first_install_at: SystemTime,
    pub is_reload: bool,
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

/// Preview helper : given a candidate `proposed_interval` the user is
/// dragging in the Config UI, return the wall-clock time at which the
/// next popup would fire IF they clicked Save right now. Applies the
/// same `RELOAD_MIN_GRACE` clamp as [`resolve_last_popup`]'s reload
/// branch, so the preview stays truthful even when the user reduces
/// the interval into the past.
pub fn next_popup_after_reload(
    now: SystemTime,
    last_popup: SystemTime,
    proposed_interval: Duration,
) -> SystemTime {
    let naive = last_popup + proposed_interval;
    let floor = now + RELOAD_MIN_GRACE;
    if naive >= floor {
        naive
    } else {
        floor
    }
}

/// Config UI live-preview kernel : ceil-minutes from `now` to the next
/// popup fire, respecting the reload clamp when `last_popup` is known.
/// Pure — no I/O — so it's Linux-testable. The tabs.rs wrapper injects
/// `SystemTime::now()` + `runtime_dat::read()` at call time.
pub fn preview_minutes(
    now: SystemTime,
    last_popup: Option<SystemTime>,
    interval: Duration,
) -> u64 {
    let next = match last_popup {
        Some(lp) => next_popup_after_reload(now, lp, interval),
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
}

/// Central decision : what should happen right now given the tick's
/// current time, the config, and the recorded last-popup timestamp.
///
/// `fired_paliers` tracks paliers already spawned in the current cycle.
/// Reset it to empty after a popup fires ; re-derive it via
/// [`derive_already_fired`] after service restart or config reload.
pub fn next_event(
    now: SystemTime,
    config: &Config,
    last_popup: SystemTime,
    fired_paliers: &HashSet<u32>,
) -> SchedulerDecision {
    if config.disabled {
        return SchedulerDecision::Nothing;
    }

    let next = next_popup_at(last_popup, config);

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
) -> HashSet<u32> {
    let next = next_popup_at(last_popup, config);
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

/// Compute the next popup time from the last one plus the interval.
pub fn next_popup_at(last_popup: SystemTime, config: &Config) -> SystemTime {
    last_popup + interval(config)
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

    #[test]
    fn disabled_config_returns_nothing() {
        let cfg = config_with(2.0, vec![15, 10, 5], true);
        let now = epoch_plus(10_000);
        let last = epoch_plus(0);
        let fired = HashSet::new();
        assert_eq!(
            next_event(now, &cfg, last, &fired),
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
            next_event(now, &cfg, last, &fired),
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
            next_event(now, &cfg, last, &fired),
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
            next_event(now, &cfg, last, &fired),
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
        match next_event(now, &cfg, last, &fired) {
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
            next_event(now, &cfg, last, &fired),
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
        match next_event(now, &cfg, last, &fired) {
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
        match next_event(now, &cfg, last, &fired) {
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
        let fired = derive_already_fired(now, &cfg, last);
        let expected: HashSet<u32> = vec![15, 10].into_iter().collect();
        assert_eq!(fired, expected);
    }

    #[test]
    fn derive_already_fired_at_cycle_start() {
        // Right after popup fired (or fresh install) : nothing crossed.
        let cfg = config_with(2.0, vec![15, 10, 5], false);
        let last = epoch_plus(0);
        let now = epoch_plus(60);
        let fired = derive_already_fired(now, &cfg, last);
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
        let fired = derive_already_fired(now, &cfg, last);
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
            next_event(now, &cfg, last, &fired),
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
        let got = next_popup_after_reload(now, lp, Duration::from_secs(45 * 60));
        assert_eq!(got, lp + Duration::from_secs(45 * 60));
    }

    #[test]
    fn next_popup_after_reload_below_grace_clamps_to_floor() {
        // lp 44 min ago, proposed interval 45 min ⇒ next = now+1min.
        // Under 2 min floor ⇒ clamp to now + 2min.
        let now = epoch_plus(10_000);
        let lp = epoch_plus(10_000 - 44 * 60);
        let got = next_popup_after_reload(now, lp, Duration::from_secs(45 * 60));
        assert_eq!(got, now + RELOAD_MIN_GRACE);
    }

    #[test]
    fn next_popup_after_reload_naive_in_past_clamps_to_floor() {
        // lp 2h ago, proposed interval 45 min ⇒ next = 75 min in the past.
        // Clamp to now + RELOAD_MIN_GRACE.
        let now = epoch_plus(3 * 3600);
        let lp = epoch_plus(3600);
        let got = next_popup_after_reload(now, lp, Duration::from_secs(45 * 60));
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
        let got = next_popup_after_reload(now, lp, interval);
        assert_eq!(got, lp + interval);
    }

    // -------- preview_minutes (Track A-2) --------

    #[test]
    fn preview_minutes_above_grace_reflects_naive_delta() {
        let now = epoch_plus(10_000);
        let lp = epoch_plus(9_400); // 10 min ago
        let interval = Duration::from_secs(60 * 60); // 1h
        // next = lp + 1h = epoch+13_000 ; delta = 3_000 s = 50 min.
        assert_eq!(preview_minutes(now, Some(lp), interval), 50);
    }

    #[test]
    fn preview_minutes_below_grace_clamps_to_reload_min_grace() {
        let now = epoch_plus(10_000);
        let lp = epoch_plus(9_000); // ~17 min ago
        let interval = Duration::from_secs(60); // 1 min — well below grace
        // Naive next = lp + 60 = epoch+9_060, in the past.
        // Clamped to now + RELOAD_MIN_GRACE (120 s) → 2 min preview.
        assert_eq!(preview_minutes(now, Some(lp), interval), 2);
    }

    #[test]
    fn preview_minutes_none_last_popup_uses_fresh_install_fallback() {
        let now = epoch_plus(10_000);
        let interval = Duration::from_secs(45 * 60); // 45 min
        assert_eq!(preview_minutes(now, None, interval), 45);
    }

    #[test]
    fn preview_minutes_one_minute_interval_boundary() {
        // Fresh install, 1-min interval → exactly 1 min preview.
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(60)), 1);
    }

    #[test]
    fn preview_minutes_twelve_hour_interval_boundary() {
        // Fresh install, 720-min (12 h) interval → 720 min preview.
        let now = epoch_plus(10_000);
        assert_eq!(
            preview_minutes(now, None, Duration::from_secs(720 * 60)),
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
        let out = preview_minutes(now, Some(lp), interval);
        // Clamped to RELOAD_MIN_GRACE = 120 s → 2 min preview.
        assert_eq!(out, 2);
    }

    // -------- ceil-boundary edges (Track A-3) --------
    // The `(delta.as_secs() + 59) / 60` ceil MUST hold at exact-multiple
    // boundaries so the C-2 "dans 2 min" display never regresses silently.

    #[test]
    fn preview_ceil_60_seconds_reads_as_1_minute() {
        let now = epoch_plus(10_000);
        // Fresh install + 60 s interval = delta 60 s exactly → 1 min.
        assert_eq!(preview_minutes(now, None, Duration::from_secs(60)), 1);
    }

    #[test]
    fn preview_ceil_61_seconds_reads_as_2_minutes() {
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(61)), 2);
    }

    #[test]
    fn preview_ceil_119_seconds_reads_as_2_minutes() {
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(119)), 2);
    }

    #[test]
    fn preview_ceil_120_seconds_reads_as_2_minutes() {
        let now = epoch_plus(10_000);
        assert_eq!(preview_minutes(now, None, Duration::from_secs(120)), 2);
    }
}
