// Runtime state file holding the epoch seconds of the last popup fire.
// Plaintext 8-byte big-endian u64, written atomically via .tmp+rename.
// Read by the service tick loop at cold start and by the Config UI for
// the "Prochain contrôle" live preview. Missing / corrupt = None.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

#[cfg(windows)]
pub const PATH: &str = r"C:\ProgramData\DiagnosticsCache\runtime.dat";

#[cfg(windows)]
pub fn read() -> Option<SystemTime> {
    read_at(Path::new(PATH))
}

#[cfg(windows)]
pub fn write(t: SystemTime) -> Result<()> {
    write_at(Path::new(PATH), t)
}

pub fn read_at(path: &Path) -> Option<SystemTime> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != 8 {
        return None;
    }
    let secs = u64::from_be_bytes(bytes.try_into().ok()?);
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

pub fn write_at(path: &Path, t: SystemTime) -> Result<()> {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let bytes = secs.to_be_bytes();
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

    #[test]
    fn roundtrip_happy_path() {
        let (_dir, path) = tmp_dat();
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        write_at(&path, t).unwrap();
        assert_eq!(read_at(&path), Some(t));
    }

    #[test]
    fn read_missing_file_is_none() {
        let (_dir, path) = tmp_dat();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_truncated_4_bytes_is_none() {
        let (_dir, path) = tmp_dat();
        std::fs::write(&path, [0u8; 4]).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn read_oversized_12_bytes_is_none() {
        let (_dir, path) = tmp_dat();
        std::fs::write(&path, [0u8; 12]).unwrap();
        assert_eq!(read_at(&path), None);
    }

    #[test]
    fn roundtrip_unix_epoch_boundary() {
        let (_dir, path) = tmp_dat();
        write_at(&path, UNIX_EPOCH).unwrap();
        assert_eq!(read_at(&path), Some(UNIX_EPOCH));
    }

    #[test]
    fn roundtrip_far_future_timestamp() {
        let (_dir, path) = tmp_dat();
        // u64::MAX / 2 seconds — far beyond year 2038, tests full 64-bit width.
        let t = UNIX_EPOCH + Duration::from_secs(u64::MAX / 2);
        write_at(&path, t).unwrap();
        assert_eq!(read_at(&path), Some(t));
    }
}
