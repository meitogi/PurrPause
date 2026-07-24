// On non-Windows targets the production callers of read()/write() are
// behind `#[cfg(windows)]`, so the module surface looks dead to the
// compiler outside of `mod tests`. Silence the noise on Linux dev.
#![cfg_attr(not(windows), allow(dead_code))]

// Runtime state file — persists everything the cold-start rules A/B/C,
// the mid-cycle active-time counter, AND the daily-pause scheduler
// need across reboots.
//
// Binary layout v4 (59 bytes, big-endian) :
//   [0..4]   magic                          b"PRRP"
//   [4..6]   version                        u16 = 4
//   [6..14]  last_popup                     u64 epoch secs
//   [14..22] paused_secs                    u64
//   [22..23] popup_pending                  u8 (0/1)
//   [23..31] last_heartbeat                 u64 epoch secs
//   [31..39] last_clean_shutdown_at         u64 epoch secs (0 = never / unclean)
//   [39..40] daily_pause_fired_at.is_some() u8 (0/1)
//   [40..48] daily_pause_fired_at           u64 epoch secs (0 if None)
//   [48..49] daily_pause_fired_paliers_bits u8 (bitmask : b0=15, b1=10, b2=5)
//   [49..50] daily_pause_skipped_today.is_some() u8 (0/1)
//   [50..54] daily_pause_skipped_today      i32 num_days_from_ce (0 if None)
//   [54..55] popup_pid.is_some()            u8 (0/1)
//   [55..59] popup_pid                      u32 (0 if None)
//
// Written atomically via .tmp + rename. Missing / short / oversized /
// bad-magic / unknown-version file → read() returns None, which routes
// the caller to the existing fresh-install / cold-start-shift path.
//
// v3 backward compatibility : a v3 file (39 bytes) reads successfully ;
// the four new daily-pause fields default to None/0 (no daily fire
// recorded yet, no skipped date, no live popup PID). First save after
// upgrade rewrites the file at v4 layout.
//
// v2 backward compatibility : preserved from v3-era code. A v2 file
// (31 bytes) reads successfully, populating `last_clean_shutdown_at`
// with UNIX_EPOCH (the "never / unclean" sentinel) and the four new
// daily-pause fields with their defaults.
//
// v1 files (raw u64, 8 bytes) still route to None (no migration path
// — the format long predates any real user install).

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate};

pub const MAGIC: &[u8; 4] = b"PRRP";
pub const VERSION: u16 = 4;
pub const FILE_SIZE: usize = 59;
pub const FILE_SIZE_V3: usize = 39;
pub const FILE_SIZE_V2: usize = 31;

#[cfg(windows)]
pub const PATH: &str = r"C:\ProgramData\DiagnosticsCache\runtime.dat";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeState {
    pub last_popup: SystemTime,
    pub paused_secs: u64,
    pub popup_pending: bool,
    pub last_heartbeat: SystemTime,
    /// Set to `SystemTime::now()` during the graceful shutdown path of
    /// `run_service` ; zeroed to `UNIX_EPOCH` at service startup (so
    /// any crash before the next graceful shutdown leaves this field
    /// as UNIX_EPOCH). Cold-start compares against `last_heartbeat` to
    /// derive `was_clean_shutdown`, which gates Rule B relaunch — a
    /// force-killed service therefore does NOT replay the popup that
    /// was up when it died.
    pub last_clean_shutdown_at: SystemTime,
    /// UTC instant of the last daily-pause fire, in the service's
    /// authoritative time (never wall-local — the caller converts to
    /// the configured TZ when comparing against the schedule). `None`
    /// = never fired. Day-boundary reset happens in the tick loop, not
    /// here.
    pub daily_pause_fired_at: Option<SystemTime>,
    /// Paliers already fired for TODAY's daily pause, as a `u8`
    /// bitmask. Bit 0 = palier 15 fired, bit 1 = palier 10, bit 2 = 5.
    /// Fixed palier set per the daily-pause spec. Converted to a
    /// `HashSet<u32>` at the scheduler boundary via
    /// [`bits_to_paliers`].
    pub daily_pause_fired_paliers_bits: u8,
    /// Anti-forkbomb (Cas C) : if the PC boots past the daily-pause
    /// grace window, mark the day as skipped so the tick loop doesn't
    /// oscillate between "fire attempt" and "grace expired" every 5s.
    /// Reset at day boundary.
    pub daily_pause_skipped_today: Option<NaiveDate>,
    /// PID of the currently live daily-pause popup process, if any.
    /// Used by Cas D.4 : on service restart with `popup_pending=true`,
    /// check whether the PID is still alive before respawning — a
    /// stale service crash must not double the popup.
    pub popup_pid: Option<u32>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            last_popup: UNIX_EPOCH,
            paused_secs: 0,
            popup_pending: false,
            last_heartbeat: UNIX_EPOCH,
            last_clean_shutdown_at: UNIX_EPOCH,
            daily_pause_fired_at: None,
            daily_pause_fired_paliers_bits: 0,
            daily_pause_skipped_today: None,
            popup_pid: None,
        }
    }
}

/// Decode the fixed-palier bitmask into the scheduler's `HashSet<u32>`
/// shape. Bit 0 = 15, bit 1 = 10, bit 2 = 5. Any other bits are
/// silently ignored (forward-compatibility if a future format widens
/// the palier set).
pub fn bits_to_paliers(bits: u8) -> HashSet<u32> {
    let mut out = HashSet::new();
    if bits & 0b001 != 0 {
        out.insert(15);
    }
    if bits & 0b010 != 0 {
        out.insert(10);
    }
    if bits & 0b100 != 0 {
        out.insert(5);
    }
    out
}

/// Inverse of [`bits_to_paliers`]. Values outside `{15, 10, 5}` are
/// silently dropped — the daily-pause scheduler only recognises those
/// three paliers.
pub fn paliers_to_bits(paliers: &HashSet<u32>) -> u8 {
    let mut bits = 0u8;
    if paliers.contains(&15) {
        bits |= 0b001;
    }
    if paliers.contains(&10) {
        bits |= 0b010;
    }
    if paliers.contains(&5) {
        bits |= 0b100;
    }
    bits
}

#[cfg(windows)]
pub fn read() -> Option<RuntimeState> {
    read_at(Path::new(PATH))
}

#[cfg(windows)]
pub fn write(state: &RuntimeState) -> Result<()> {
    write_at(Path::new(PATH), state)
}

pub fn read_at(path: &Path) -> Option<RuntimeState> {
    let bytes = std::fs::read(path).ok()?;
    // Only exact-size matches for v2 (31), v3 (39), or v4 (59) — any
    // other size routes to fresh-install to avoid partial-write
    // footguns.
    if bytes.len() != FILE_SIZE && bytes.len() != FILE_SIZE_V3 && bytes.len() != FILE_SIZE_V2 {
        return None;
    }
    if &bytes[0..4] != MAGIC {
        return None;
    }
    let version = u16::from_be_bytes(bytes[4..6].try_into().ok()?);
    match version {
        4 if bytes.len() == FILE_SIZE => parse_v4(&bytes),
        3 if bytes.len() == FILE_SIZE_V3 => parse_v3_as_v4(&bytes),
        2 if bytes.len() == FILE_SIZE_V2 => parse_v2_as_v4(&bytes),
        _ => None,
    }
}

fn parse_v4(bytes: &[u8]) -> Option<RuntimeState> {
    let base = parse_v3_common(bytes)?;
    let daily_pause_fired_at = if bytes[39] != 0 {
        let secs = u64::from_be_bytes(bytes[40..48].try_into().ok()?);
        Some(UNIX_EPOCH + Duration::from_secs(secs))
    } else {
        None
    };
    let daily_pause_fired_paliers_bits = bytes[48];
    let daily_pause_skipped_today = if bytes[49] != 0 {
        let days = i32::from_be_bytes(bytes[50..54].try_into().ok()?);
        // Malformed date → treat as None (defensive : preserves the
        // rest of the state instead of nuking the whole file).
        NaiveDate::from_num_days_from_ce_opt(days)
    } else {
        None
    };
    let popup_pid = if bytes[54] != 0 {
        Some(u32::from_be_bytes(bytes[55..59].try_into().ok()?))
    } else {
        None
    };
    Some(RuntimeState {
        daily_pause_fired_at,
        daily_pause_fired_paliers_bits,
        daily_pause_skipped_today,
        popup_pid,
        ..base
    })
}

fn parse_v3_as_v4(bytes: &[u8]) -> Option<RuntimeState> {
    // v3 → v4 upgrade : new daily-pause fields default to None/0. First
    // write after upgrade rewrites at v4 layout.
    parse_v3_common(bytes)
}

fn parse_v2_as_v4(bytes: &[u8]) -> Option<RuntimeState> {
    let last_popup_secs = u64::from_be_bytes(bytes[6..14].try_into().ok()?);
    let paused_secs = u64::from_be_bytes(bytes[14..22].try_into().ok()?);
    let popup_pending = bytes[22] != 0;
    let last_heartbeat_secs = u64::from_be_bytes(bytes[23..31].try_into().ok()?);
    Some(RuntimeState {
        last_popup: UNIX_EPOCH + Duration::from_secs(last_popup_secs),
        paused_secs,
        popup_pending,
        last_heartbeat: UNIX_EPOCH + Duration::from_secs(last_heartbeat_secs),
        // No v2 clean-shutdown info — treat as unclean so the
        // pre-Rule-B gate fires once on the upgrade boot. Safe.
        last_clean_shutdown_at: UNIX_EPOCH,
        ..RuntimeState::default()
    })
}

fn parse_v3_common(bytes: &[u8]) -> Option<RuntimeState> {
    let last_popup_secs = u64::from_be_bytes(bytes[6..14].try_into().ok()?);
    let paused_secs = u64::from_be_bytes(bytes[14..22].try_into().ok()?);
    let popup_pending = bytes[22] != 0;
    let last_heartbeat_secs = u64::from_be_bytes(bytes[23..31].try_into().ok()?);
    let last_clean_shutdown_secs = u64::from_be_bytes(bytes[31..39].try_into().ok()?);
    Some(RuntimeState {
        last_popup: UNIX_EPOCH + Duration::from_secs(last_popup_secs),
        paused_secs,
        popup_pending,
        last_heartbeat: UNIX_EPOCH + Duration::from_secs(last_heartbeat_secs),
        last_clean_shutdown_at: UNIX_EPOCH + Duration::from_secs(last_clean_shutdown_secs),
        ..RuntimeState::default()
    })
}

pub fn write_at(path: &Path, state: &RuntimeState) -> Result<()> {
    let mut bytes = [0u8; FILE_SIZE];
    bytes[0..4].copy_from_slice(MAGIC);
    bytes[4..6].copy_from_slice(&VERSION.to_be_bytes());
    let last_popup_secs = state
        .last_popup
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    bytes[6..14].copy_from_slice(&last_popup_secs.to_be_bytes());
    bytes[14..22].copy_from_slice(&state.paused_secs.to_be_bytes());
    bytes[22] = u8::from(state.popup_pending);
    let last_heartbeat_secs = state
        .last_heartbeat
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    bytes[23..31].copy_from_slice(&last_heartbeat_secs.to_be_bytes());
    let last_clean_shutdown_secs = state
        .last_clean_shutdown_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    bytes[31..39].copy_from_slice(&last_clean_shutdown_secs.to_be_bytes());

    // v4 additions.
    if let Some(t) = state.daily_pause_fired_at {
        bytes[39] = 1;
        let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        bytes[40..48].copy_from_slice(&secs.to_be_bytes());
    }
    bytes[48] = state.daily_pause_fired_paliers_bits;
    if let Some(d) = state.daily_pause_skipped_today {
        bytes[49] = 1;
        bytes[50..54].copy_from_slice(&d.num_days_from_ce().to_be_bytes());
    }
    if let Some(pid) = state.popup_pid {
        bytes[54] = 1;
        bytes[55..59].copy_from_slice(&pid.to_be_bytes());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("dat.tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_dat() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("runtime.dat");
        (dir, path)
    }

    fn epoch_plus(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn roundtrip_full_runtime_state() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(1_700_000_000),
            paused_secs: 3600,
            popup_pending: true,
            last_heartbeat: epoch_plus(1_700_003_599),
            last_clean_shutdown_at: epoch_plus(1_700_003_599),
            ..RuntimeState::default()
        };
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn roundtrip_zero_state() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState::default();
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn roundtrip_max_u64_fields() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(u64::MAX / 2),
            paused_secs: u64::MAX,
            popup_pending: true,
            last_heartbeat: epoch_plus(u64::MAX / 2),
            last_clean_shutdown_at: epoch_plus(u64::MAX / 2),
            ..RuntimeState::default()
        };
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn roundtrip_popup_pending_false() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(42),
            paused_secs: 7,
            popup_pending: false,
            last_heartbeat: epoch_plus(43),
            last_clean_shutdown_at: epoch_plus(43),
            ..RuntimeState::default()
        };
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn roundtrip_unclean_sentinel() {
        // last_clean_shutdown_at = UNIX_EPOCH is the sentinel used at
        // service boot ("we're running, next boot is unclean unless
        // graceful shutdown says otherwise"). Round-trip must preserve
        // it exactly so cold-start's `was_clean_shutdown` derivation
        // stays deterministic.
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(100),
            paused_secs: 42,
            popup_pending: true,
            last_heartbeat: epoch_plus(200),
            last_clean_shutdown_at: UNIX_EPOCH,
            ..RuntimeState::default()
        };
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn read_missing_file_is_none() {
        let (_dir, path) = tmp_dat();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_bad_magic_is_none() {
        let (_dir, path) = tmp_dat();
        let mut bytes = [0u8; FILE_SIZE];
        bytes[0..4].copy_from_slice(b"XXXX");
        bytes[4..6].copy_from_slice(&VERSION.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_wrong_version_1_is_none() {
        let (_dir, path) = tmp_dat();
        let mut bytes = [0u8; FILE_SIZE];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..6].copy_from_slice(&1u16.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_wrong_version_5_is_none() {
        // Guard-rail for the *next* format bump : if someone ships v5
        // without updating this reader, the file routes cleanly to the
        // fresh-install path instead of silently mis-parsing as v4.
        let (_dir, path) = tmp_dat();
        let mut bytes = [0u8; FILE_SIZE];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..6].copy_from_slice(&5u16.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_truncated_by_one_is_none() {
        let (_dir, path) = tmp_dat();
        std::fs::write(&path, vec![0u8; FILE_SIZE - 1]).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_oversized_by_one_is_none() {
        let (_dir, path) = tmp_dat();
        std::fs::write(&path, vec![0u8; FILE_SIZE + 1]).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_v1_8_bytes_is_none() {
        let (_dir, path) = tmp_dat();
        // Legacy v1 file (raw u64 epoch, 8 bytes) — must route to
        // fresh-install path via None. No implicit migration.
        std::fs::write(&path, 1_700_000_000u64.to_be_bytes()).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_v2_migrates_to_v4_with_epoch_clean_shutdown() {
        // A v2 file (31 bytes, version=2) surviving a format upgrade
        // must parse cleanly and populate last_clean_shutdown_at with
        // UNIX_EPOCH — the unclean sentinel. That's the whole point of
        // the clean-shutdown gate : any unclean boot clears
        // popup_pending pre-Rule-B. The v4 daily-pause fields default
        // to None/0 (no daily fire recorded pre-upgrade).
        let (_dir, path) = tmp_dat();
        let mut bytes = [0u8; FILE_SIZE_V2];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
        bytes[6..14].copy_from_slice(&1_700_000_000u64.to_be_bytes());
        bytes[14..22].copy_from_slice(&500u64.to_be_bytes());
        bytes[22] = 1; // popup_pending true
        bytes[23..31].copy_from_slice(&1_700_001_000u64.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();

        let parsed = read_at(&path).unwrap();
        assert_eq!(parsed.last_popup, epoch_plus(1_700_000_000));
        assert_eq!(parsed.paused_secs, 500);
        assert!(parsed.popup_pending);
        assert_eq!(parsed.last_heartbeat, epoch_plus(1_700_001_000));
        assert_eq!(parsed.last_clean_shutdown_at, UNIX_EPOCH);
        assert_eq!(parsed.daily_pause_fired_at, None);
        assert_eq!(parsed.daily_pause_fired_paliers_bits, 0);
        assert_eq!(parsed.daily_pause_skipped_today, None);
        assert_eq!(parsed.popup_pid, None);
    }

    #[test]
    fn read_v3_migrates_to_v4_with_default_daily_fields() {
        // A v3 file (39 bytes, version=3) is the pre-daily-pause
        // format. All existing fields must round-trip verbatim ; the
        // four new daily-pause fields default to None/0.
        let (_dir, path) = tmp_dat();
        let mut bytes = [0u8; FILE_SIZE_V3];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..6].copy_from_slice(&3u16.to_be_bytes());
        bytes[6..14].copy_from_slice(&1_700_000_000u64.to_be_bytes());
        bytes[14..22].copy_from_slice(&500u64.to_be_bytes());
        bytes[22] = 1;
        bytes[23..31].copy_from_slice(&1_700_001_000u64.to_be_bytes());
        bytes[31..39].copy_from_slice(&1_700_000_500u64.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();

        let parsed = read_at(&path).unwrap();
        assert_eq!(parsed.last_popup, epoch_plus(1_700_000_000));
        assert_eq!(parsed.paused_secs, 500);
        assert!(parsed.popup_pending);
        assert_eq!(parsed.last_heartbeat, epoch_plus(1_700_001_000));
        assert_eq!(parsed.last_clean_shutdown_at, epoch_plus(1_700_000_500));
        assert_eq!(parsed.daily_pause_fired_at, None);
        assert_eq!(parsed.daily_pause_fired_paliers_bits, 0);
        assert_eq!(parsed.daily_pause_skipped_today, None);
        assert_eq!(parsed.popup_pid, None);
    }

    #[test]
    fn v4_write_produces_exactly_59_bytes_starting_with_prrp04() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(1),
            paused_secs: 2,
            popup_pending: false,
            last_heartbeat: epoch_plus(3),
            last_clean_shutdown_at: epoch_plus(4),
            ..RuntimeState::default()
        };
        write_at(&path, &state).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 59);
        assert_eq!(&bytes[0..4], MAGIC);
        assert_eq!(u16::from_be_bytes(bytes[4..6].try_into().unwrap()), 4);
    }

    #[test]
    fn roundtrip_daily_pause_fields_populated() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(1_700_000_000),
            paused_secs: 100,
            popup_pending: true,
            last_heartbeat: epoch_plus(1_700_000_500),
            last_clean_shutdown_at: epoch_plus(1_700_000_200),
            daily_pause_fired_at: Some(epoch_plus(1_700_040_000)),
            daily_pause_fired_paliers_bits: 0b011, // paliers 15 + 10 fired
            daily_pause_skipped_today: NaiveDate::from_ymd_opt(2026, 7, 24),
            popup_pid: Some(12345),
        };
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn roundtrip_daily_pause_fields_all_none() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(1_700_000_000),
            paused_secs: 100,
            popup_pending: false,
            last_heartbeat: epoch_plus(1_700_000_500),
            last_clean_shutdown_at: epoch_plus(1_700_000_200),
            daily_pause_fired_at: None,
            daily_pause_fired_paliers_bits: 0,
            daily_pause_skipped_today: None,
            popup_pid: None,
        };
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn bits_to_paliers_all_three() {
        let paliers = bits_to_paliers(0b111);
        assert!(paliers.contains(&15));
        assert!(paliers.contains(&10));
        assert!(paliers.contains(&5));
        assert_eq!(paliers.len(), 3);
    }

    #[test]
    fn bits_to_paliers_empty() {
        assert!(bits_to_paliers(0).is_empty());
    }

    #[test]
    fn bits_to_paliers_single() {
        assert_eq!(bits_to_paliers(0b001), HashSet::from([15]));
        assert_eq!(bits_to_paliers(0b010), HashSet::from([10]));
        assert_eq!(bits_to_paliers(0b100), HashSet::from([5]));
    }

    #[test]
    fn bits_to_paliers_ignores_unknown_bits() {
        // Bits 3..7 are unused today ; a future format extension might
        // widen the set. Current reader must ignore them silently.
        assert_eq!(bits_to_paliers(0b1111_1111), HashSet::from([15, 10, 5]));
    }

    #[test]
    fn paliers_to_bits_roundtrip() {
        for bits in 0..=0b111u8 {
            let paliers = bits_to_paliers(bits);
            assert_eq!(paliers_to_bits(&paliers), bits, "bits={bits:03b}");
        }
    }

    #[test]
    fn paliers_to_bits_drops_unknown_values() {
        let paliers: HashSet<u32> = HashSet::from([15, 10, 5, 1, 99]);
        // Only the three known paliers should be encoded.
        assert_eq!(paliers_to_bits(&paliers), 0b111);
    }
}
