// Cross-platform authoritative time source for the daily-pause scheduler.
//
// The whole premise of the daily pause is that a child changing the
// Windows clock or the Windows time-zone panel must NOT be able to
// shift the fire moment. So :
//   - `now_authoritative()` returns UTC based on the last successful
//     network sync + a monotonic tick (QueryPerformanceCounter on
//     Windows, std::time::Instant on Linux — tamper-resistant), falling
//     back silently to `SystemTime::now()` when never synced. Silent
//     fallback is by contract : the app must remain usable offline.
//   - `in_configured_tz()` applies the IANA time-zone name persisted
//     in `Config::daily_pause.timezone` (default `Europe/Paris`),
//     ignoring the OS-level TZ setting entirely.
//   - `use_system_clock=true` short-circuits both : that's the public /
//     permissive escape hatch documented in the ROLLOUT.
//
// Session 1 provides the shape + fallback logic + TZ resolution. The
// actual NTP + HTTPS sync loop (spawn + retry cadence) lands in
// session 2 when the caller in `service.rs` is ready to drive it.
// `sync_ntp()` / `sync_https()` are wired but return `Err` for now —
// the fallback path is what keeps the scheduler correct while the
// sync is stubbed.

#![cfg_attr(not(windows), allow(dead_code))]

use std::str::FromStr;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, TimeZone, Timelike};
use chrono_tz::Tz;

use crate::config::Config;
use crate::config::defaults::DEFAULT_DAILY_PAUSE_TIMEZONE;

/// Snapshot of the last successful network sync : the UTC instant the
/// server reported paired with the monotonic tick at reception. `now
/// = wall_at_sync + (mono_now - mono_at_sync)` is drift-bounded and
/// immune to wall-clock jumps.
#[derive(Debug, Clone, Copy)]
struct SyncedState {
    wall_at_sync: SystemTime,
    mono_at_sync: MonoInstant,
}

/// Cross-platform monotonic instant. On Windows, backed by
/// `QueryPerformanceCounter` — a raw tick counter that keeps advancing
/// regardless of wall-clock changes, hibernation, or NTP steps. On
/// Linux (test build), `std::time::Instant` gives the same guarantees.
#[derive(Debug, Clone, Copy)]
struct MonoInstant {
    #[cfg(windows)]
    ticks: i64,
    #[cfg(not(windows))]
    instant: std::time::Instant,
}

impl MonoInstant {
    fn now() -> Self {
        #[cfg(windows)]
        {
            use windows::Win32::System::Performance::QueryPerformanceCounter;
            let mut counter = 0i64;
            // SAFETY : QueryPerformanceCounter always succeeds on
            // supported OS versions (Windows XP+, per MSDN). The
            // return value is a `BOOL` — we ignore it because failure
            // is not observed on any Windows version we ship on.
            unsafe {
                let _ = QueryPerformanceCounter(&mut counter);
            }
            Self { ticks: counter }
        }
        #[cfg(not(windows))]
        {
            Self {
                instant: std::time::Instant::now(),
            }
        }
    }

    fn elapsed_since(self, earlier: Self) -> Duration {
        #[cfg(windows)]
        {
            use windows::Win32::System::Performance::QueryPerformanceFrequency;
            let mut freq = 0i64;
            unsafe {
                let _ = QueryPerformanceFrequency(&mut freq);
            }
            if freq <= 0 {
                return Duration::ZERO;
            }
            let delta = self.ticks.saturating_sub(earlier.ticks).max(0) as u128;
            let freq = freq as u128;
            let secs = (delta / freq) as u64;
            let sub_ns = ((delta % freq) * 1_000_000_000 / freq) as u32;
            Duration::new(secs, sub_ns)
        }
        #[cfg(not(windows))]
        {
            self.instant
                .checked_duration_since(earlier.instant)
                .unwrap_or_default()
        }
    }
}

/// Latest successful sync, shared between `sync_*` writers and
/// `now_authoritative` readers. `None` = never synced, silent fallback
/// applies.
static SYNCED: RwLock<Option<SyncedState>> = RwLock::new(None);

/// Authoritative UTC. Respects `cfg.use_system_clock` :
///
/// - `true` → `SystemTime::now()` (public / permissive path).
/// - `false` + sync available → `wall_at_sync + monotonic_elapsed`
///   (tamper-resistant path).
/// - `false` + no sync → silent fallback to `SystemTime::now()` so the
///   scheduler still fires roughly on time when offline. This matches
///   the ROLLOUT decision "public app must work offline ; anti-tamper
///   is a nice-to-have, not a blocker for feature availability".
pub fn now_authoritative(cfg: &Config) -> SystemTime {
    if cfg.use_system_clock {
        return SystemTime::now();
    }
    let guard = match SYNCED.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    match *guard {
        Some(state) => {
            let elapsed = MonoInstant::now().elapsed_since(state.mono_at_sync);
            state.wall_at_sync + elapsed
        }
        None => SystemTime::now(),
    }
}

/// Resolve `cfg.daily_pause.timezone` to a `chrono_tz::Tz`, falling
/// back to `Europe/Paris` on any parse failure. Ignores the OS-level
/// TZ entirely when `use_system_clock=false` — that's the whole point
/// of persisting the TZ name in config.
///
/// When `use_system_clock=true`, the caller could theoretically want
/// the Windows-panel TZ instead. We don't do that here : resolving the
/// OS TZ portably is a separate concern (Windows uses
/// `GetDynamicTimeZoneInformation` + a Windows→IANA mapping) and the
/// permissive path still gets a sensible result via the configured
/// TZ. Session 3 UI wires the "use Windows TZ" toggle by writing the
/// resolved IANA name into `cfg.daily_pause.timezone`.
pub fn in_configured_tz(cfg: &Config, now: SystemTime) -> DateTime<Tz> {
    let tz = resolve_tz(&cfg.daily_pause.timezone);
    let dt_utc: DateTime<chrono::Utc> = now.into();
    dt_utc.with_timezone(&tz)
}

fn resolve_tz(name: &str) -> Tz {
    Tz::from_str(name).unwrap_or_else(|_| {
        // Fall back silently — invalid TZ names shouldn't crash the
        // service. The UI (session 3) validates on save, but a
        // hand-edited state.dat is possible.
        Tz::from_str(DEFAULT_DAILY_PAUSE_TIMEZONE)
            .expect("Europe/Paris is a well-known IANA TZ")
    })
}

/// Today's fire moment (H:M in the configured TZ) if it's still in the
/// future, otherwise tomorrow's. Never returns a moment in the past
/// (relative to `now_local`).
pub fn next_daily_pause_at(cfg: &Config, now_local: DateTime<Tz>) -> DateTime<Tz> {
    let tz = now_local.timezone();
    let hour = cfg.daily_pause.hour as u32;
    let minute = cfg.daily_pause.minute as u32;
    let today_fire = with_hm(&tz, now_local.year(), now_local.month(), now_local.day(), hour, minute);
    if today_fire > now_local {
        today_fire
    } else {
        let tomorrow = now_local
            .checked_add_signed(ChronoDuration::days(1))
            .unwrap_or(now_local);
        with_hm(
            &tz,
            tomorrow.year(),
            tomorrow.month(),
            tomorrow.day(),
            hour,
            minute,
        )
    }
}

/// Build a `DateTime<Tz>` at (Y, M, D, h, m, 0) in `tz`. Handles the
/// DST-ambiguity case by preferring the earlier of two matches
/// (`MappedLocalTime::Ambiguous` → the fall-back autumn transition).
/// The "gap" case (spring-forward, no valid local time at h:m) falls
/// back to h:m + 1h so we still fire the same day rather than skipping.
fn with_hm(tz: &Tz, y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
    match tz.with_ymd_and_hms(y, m, d, h, min, 0) {
        chrono::MappedLocalTime::Single(dt) => dt,
        chrono::MappedLocalTime::Ambiguous(earlier, _later) => earlier,
        chrono::MappedLocalTime::None => tz
            .with_ymd_and_hms(y, m, d, h.saturating_add(1).min(23), min, 0)
            .single()
            .unwrap_or_else(|| {
                tz.with_ymd_and_hms(y, m, d, 12, 0, 0)
                    .single()
                    .expect("noon is always valid")
            }),
    }
}

/// Blocking NTP sync attempt. Called from a `std::thread` in session 2.
///
/// Session 1 stub : returns `Err` because the socket adapter
/// (`sntpc-net-std`) lands with the sync loop wiring in session 2. The
/// `now_authoritative` fallback path already handles this cleanly, so
/// the scheduler stays correct in the meantime.
///
/// Contract when wired : must never panic and must not log at
/// error-level on failure — offline is a normal state. Successful
/// syncs write to `SYNCED` via [`record_sync`].
pub fn sync_ntp() -> anyhow::Result<()> {
    // Session 2 fills this in with sntpc + sntpc-net-std. The stub
    // returns an anyhow error rather than panicking so the caller's
    // retry loop treats it as a routine miss.
    anyhow::bail!("NTP sync not yet wired");
}

/// Blocking HTTPS sync attempt (parse the `Date:` header of a fast
/// endpoint like `time.cloudflare.com`). Fallback for firewalled NTP.
///
/// Session 1 stub — see [`sync_ntp`] for rationale. The HTTP client
/// choice (`ureq` vs `reqwest`) is deliberately deferred until the
/// sync loop actually lands, so we don't drag a heavyweight dep in
/// for a stub.
pub fn sync_https() -> anyhow::Result<()> {
    anyhow::bail!("HTTPS sync not yet wired");
}

/// Record a successful sync. Reset semantics : always overwrites the
/// prior snapshot. Only sync fns should call this ; exposed at crate
/// scope so tests can seed a SYNCED state without hitting the network.
pub(crate) fn record_sync(wall: SystemTime) {
    let state = SyncedState {
        wall_at_sync: wall,
        mono_at_sync: MonoInstant::now(),
    };
    let mut guard = match SYNCED.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(state);
}

/// Clear the SYNCED state — for tests that need to exercise the
/// "never synced" fallback path after another test seeded a value.
#[cfg(test)]
pub(crate) fn reset_sync_for_tests() {
    let mut guard = SYNCED.write().unwrap();
    *guard = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DailyPauseConfig;

    fn cfg_with_tz(tz: &str, use_system: bool) -> Config {
        let mut cfg = Config::default();
        cfg.use_system_clock = use_system;
        cfg.daily_pause = DailyPauseConfig {
            timezone: tz.to_string(),
            ..DailyPauseConfig::default()
        };
        cfg
    }

    #[test]
    fn now_authoritative_uses_system_clock_when_toggled() {
        let cfg = cfg_with_tz("Europe/Paris", true);
        // Should not blow up ; result is close to SystemTime::now.
        let a = now_authoritative(&cfg);
        let b = SystemTime::now();
        // A generous 5s window covers scheduling jitter under load.
        let delta = b.duration_since(a).unwrap_or_default();
        assert!(delta < Duration::from_secs(5));
    }

    #[test]
    fn now_authoritative_falls_back_silently_when_never_synced() {
        reset_sync_for_tests();
        let cfg = cfg_with_tz("Europe/Paris", false);
        let a = now_authoritative(&cfg);
        let b = SystemTime::now();
        let delta = b.duration_since(a).unwrap_or_default();
        assert!(delta < Duration::from_secs(5));
    }

    #[test]
    fn in_configured_tz_falls_back_to_paris_on_invalid_name() {
        let cfg = cfg_with_tz("Not/A/Real_Zone", false);
        let now = SystemTime::now();
        let dt = in_configured_tz(&cfg, now);
        assert_eq!(dt.timezone(), chrono_tz::Europe::Paris);
    }

    #[test]
    fn in_configured_tz_resolves_valid_iana_name() {
        let cfg = cfg_with_tz("America/New_York", false);
        let now = SystemTime::now();
        let dt = in_configured_tz(&cfg, now);
        assert_eq!(dt.timezone(), chrono_tz::America::New_York);
    }

    #[test]
    fn next_daily_pause_at_returns_today_when_still_future() {
        let cfg = {
            let mut c = cfg_with_tz("Europe/Paris", false);
            c.daily_pause.hour = 18;
            c.daily_pause.minute = 30;
            c
        };
        let now_local = chrono_tz::Europe::Paris
            .with_ymd_and_hms(2026, 3, 15, 12, 0, 0)
            .unwrap();
        let next = next_daily_pause_at(&cfg, now_local);
        assert_eq!(next.day(), 15);
        assert_eq!(next.hour(), 18);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn next_daily_pause_at_returns_tomorrow_when_past() {
        let cfg = {
            let mut c = cfg_with_tz("Europe/Paris", false);
            c.daily_pause.hour = 12;
            c.daily_pause.minute = 0;
            c
        };
        let now_local = chrono_tz::Europe::Paris
            .with_ymd_and_hms(2026, 3, 15, 14, 0, 0)
            .unwrap();
        let next = next_daily_pause_at(&cfg, now_local);
        assert_eq!(next.day(), 16);
        assert_eq!(next.hour(), 12);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn next_daily_pause_at_dst_transition_spring_forward() {
        // 2026-03-29 Europe/Paris : clocks jump 02:00 → 03:00. A fire
        // at 02:30 doesn't exist as a local time that day — the
        // with_hm fallback picks 03:30 so we still fire the same day.
        let cfg = {
            let mut c = cfg_with_tz("Europe/Paris", false);
            c.daily_pause.hour = 2;
            c.daily_pause.minute = 30;
            c
        };
        let now_local = chrono_tz::Europe::Paris
            .with_ymd_and_hms(2026, 3, 29, 1, 0, 0)
            .unwrap();
        let next = next_daily_pause_at(&cfg, now_local);
        assert_eq!(next.day(), 29);
        // Fallback rule bumps the hour by 1 into the valid range.
        assert!(next.hour() == 3);
    }

    #[test]
    fn mono_instant_elapsed_since_is_non_negative() {
        let a = MonoInstant::now();
        std::thread::sleep(Duration::from_millis(5));
        let b = MonoInstant::now();
        let d = b.elapsed_since(a);
        assert!(d >= Duration::from_millis(1), "elapsed = {d:?}");
    }

    #[test]
    fn sync_ntp_stub_returns_err_and_does_not_panic() {
        assert!(sync_ntp().is_err());
    }

    #[test]
    fn sync_https_stub_returns_err_and_does_not_panic() {
        assert!(sync_https().is_err());
    }

    #[test]
    fn record_sync_then_now_authoritative_uses_synced_wall() {
        // Freeze the "server wall" 100s in the past and confirm
        // now_authoritative returns something close to it (small
        // monotonic delta only).
        let past = SystemTime::now() - Duration::from_secs(100);
        record_sync(past);
        let cfg = cfg_with_tz("Europe/Paris", false);
        let n = now_authoritative(&cfg);
        let real_now = SystemTime::now();
        // n should be near `past + ~0`, not near `real_now`.
        let from_past = n.duration_since(past).unwrap_or_default();
        let from_now = real_now.duration_since(n).unwrap_or_default();
        assert!(
            from_past < Duration::from_secs(5),
            "n drifted from synced past : {from_past:?}"
        );
        assert!(
            from_now > Duration::from_secs(90),
            "n is too close to real_now — sync path not honored : {from_now:?}"
        );
        reset_sync_for_tests();
    }
}
