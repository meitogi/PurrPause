// On non-Windows targets the production callers of read()/write() are
// behind `#[cfg(windows)]`, so the module surface looks dead to the
// compiler outside of `mod tests`. Silence the noise on Linux dev.
#![cfg_attr(not(windows), allow(dead_code))]

// Runtime state file — persists everything the cold-start rules A/B/C
// and the mid-cycle active-time counter need across reboots.
//
// Binary layout v3 (39 bytes, big-endian) :
//   [0..4]   magic                    b"PRRP"
//   [4..6]   version                  u16 = 3
//   [6..14]  last_popup               u64 epoch secs
//   [14..22] paused_secs              u64
//   [22..23] popup_pending            u8 (0/1)
//   [23..31] last_heartbeat           u64 epoch secs
//   [31..39] last_clean_shutdown_at   u64 epoch secs (0 = never / unclean)
//
// Written atomically via .tmp + rename. Missing / short / oversized /
// bad-magic / wrong-version file → read() returns None, which routes
// the caller to the existing fresh-install / cold-start-shift path.
//
// v2 backward compatibility (session 7) : a v2 file (31 bytes) reads
// successfully, populating `last_clean_shutdown_at` with UNIX_EPOCH
// (the "never / unclean" sentinel). This forces the B5 clean-shutdown
// gate to treat any first boot after a b.9 → b.10 upgrade as unclean
// exactly once — safe by construction, popup_pending gets cleared
// pre-Rule-B and the next natural cycle fires cleanly.
//
// v1 files (raw u64, 8 bytes) still route to None — pre-v1 shipping,
// no real users, no migration path needed.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub const MAGIC: &[u8; 4] = b"PRRP";
pub const VERSION: u16 = 3;
pub const FILE_SIZE: usize = 39;
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
    // Only exact-size matches for v2 (31) or v3 (39) — any other size
    // routes to fresh-install to avoid partial-write footguns.
    if bytes.len() != FILE_SIZE && bytes.len() != FILE_SIZE_V2 {
        return None;
    }
    if &bytes[0..4] != MAGIC {
        return None;
    }
    let version = u16::from_be_bytes(bytes[4..6].try_into().ok()?);
    match version {
        3 if bytes.len() == FILE_SIZE => parse_v3(&bytes),
        2 if bytes.len() == FILE_SIZE_V2 => parse_v2_as_v3(&bytes),
        _ => None,
    }
}

fn parse_v3(bytes: &[u8]) -> Option<RuntimeState> {
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
    })
}

fn parse_v2_as_v3(bytes: &[u8]) -> Option<RuntimeState> {
    let last_popup_secs = u64::from_be_bytes(bytes[6..14].try_into().ok()?);
    let paused_secs = u64::from_be_bytes(bytes[14..22].try_into().ok()?);
    let popup_pending = bytes[22] != 0;
    let last_heartbeat_secs = u64::from_be_bytes(bytes[23..31].try_into().ok()?);
    Some(RuntimeState {
        last_popup: UNIX_EPOCH + Duration::from_secs(last_popup_secs),
        paused_secs,
        popup_pending,
        last_heartbeat: UNIX_EPOCH + Duration::from_secs(last_heartbeat_secs),
        // No v2 clean-shutdown info — treat as unclean so the B5
        // gate fires once on b.9 → b.10 upgrade boot. Safe.
        last_clean_shutdown_at: UNIX_EPOCH,
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
        };
        write_at(&path, &state).unwrap();
        assert_eq!(read_at(&path), Some(state));
    }

    #[test]
    fn roundtrip_zero_state() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: UNIX_EPOCH,
            paused_secs: 0,
            popup_pending: false,
            last_heartbeat: UNIX_EPOCH,
            last_clean_shutdown_at: UNIX_EPOCH,
        };
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
    fn read_wrong_version_4_is_none() {
        let (_dir, path) = tmp_dat();
        let mut bytes = [0u8; FILE_SIZE];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..6].copy_from_slice(&4u16.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_truncated_38_bytes_is_none() {
        let (_dir, path) = tmp_dat();
        std::fs::write(&path, [0u8; FILE_SIZE - 1]).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_oversized_40_bytes_is_none() {
        let (_dir, path) = tmp_dat();
        std::fs::write(&path, [0u8; FILE_SIZE + 1]).unwrap();
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
    fn read_v2_migrates_to_v3_with_epoch_clean_shutdown() {
        // A v2 file (31 bytes, version=2) surviving a b.9 → b.10 upgrade
        // must parse cleanly and populate last_clean_shutdown_at with
        // UNIX_EPOCH — the unclean sentinel. That's the whole point of
        // the B5 gate : any unclean boot clears popup_pending pre-Rule-B.
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
    }

    #[test]
    fn v3_write_produces_exactly_39_bytes_starting_with_prrp03() {
        let (_dir, path) = tmp_dat();
        let state = RuntimeState {
            last_popup: epoch_plus(1),
            paused_secs: 2,
            popup_pending: false,
            last_heartbeat: epoch_plus(3),
            last_clean_shutdown_at: epoch_plus(4),
        };
        write_at(&path, &state).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 39);
        assert_eq!(&bytes[0..4], MAGIC);
        assert_eq!(u16::from_be_bytes(bytes[4..6].try_into().unwrap()), 3);
    }
}
