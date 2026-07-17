// On non-Windows targets the production callers of read()/write() are
// behind `#[cfg(windows)]`, so the module surface looks dead to the
// compiler outside of `mod tests`. Silence the noise on Linux dev.
#![cfg_attr(not(windows), allow(dead_code))]

// Runtime state file — persists everything the cold-start rules A/B/C
// and the mid-cycle active-time counter need across reboots.
//
// Binary layout v2 (31 bytes, big-endian) :
//   [0..4]   magic          b"PRRP"
//   [4..6]   version        u16 = 2
//   [6..14]  last_popup     u64 epoch secs
//   [14..22] paused_secs    u64
//   [22..23] popup_pending  u8 (0/1)
//   [23..31] last_heartbeat u64 epoch secs
//
// Written atomically via .tmp + rename. Missing / short / oversized /
// bad-magic / wrong-version file → read() returns None, which routes
// the caller to the existing fresh-install / cold-start-shift path.
// No v1 migration: pre-v1 shipping, no real users yet.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub const MAGIC: &[u8; 4] = b"PRRP";
pub const VERSION: u16 = 2;
pub const FILE_SIZE: usize = 31;

#[cfg(windows)]
pub const PATH: &str = r"C:\ProgramData\DiagnosticsCache\runtime.dat";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeState {
    pub last_popup: SystemTime,
    pub paused_secs: u64,
    pub popup_pending: bool,
    pub last_heartbeat: SystemTime,
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
    if bytes.len() != FILE_SIZE {
        return None;
    }
    if &bytes[0..4] != MAGIC {
        return None;
    }
    let version = u16::from_be_bytes(bytes[4..6].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let last_popup_secs = u64::from_be_bytes(bytes[6..14].try_into().ok()?);
    let paused_secs = u64::from_be_bytes(bytes[14..22].try_into().ok()?);
    let popup_pending = bytes[22] != 0;
    let last_heartbeat_secs = u64::from_be_bytes(bytes[23..31].try_into().ok()?);

    Some(RuntimeState {
        last_popup: UNIX_EPOCH + Duration::from_secs(last_popup_secs),
        paused_secs,
        popup_pending,
        last_heartbeat: UNIX_EPOCH + Duration::from_secs(last_heartbeat_secs),
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
    fn read_wrong_version_3_is_none() {
        let (_dir, path) = tmp_dat();
        let mut bytes = [0u8; FILE_SIZE];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..6].copy_from_slice(&3u16.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_truncated_30_bytes_is_none() {
        let (_dir, path) = tmp_dat();
        std::fs::write(&path, [0u8; FILE_SIZE - 1]).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_oversized_32_bytes_is_none() {
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
}
