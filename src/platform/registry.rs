// Windows registry helpers.
//
// Two responsibilities live here :
//
// 1. **PendingFileRenameOperations defuse** — when a previous `--uninstall`
//    scheduled the exe for delete-on-reboot via
//    `MoveFileExW(MOVEFILE_DELAY_UNTIL_REBOOT)`, Windows records the
//    pending rename in
//    `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\PendingFileRenameOperations`
//    (REG_MULTI_SZ). If the user reinstalls before rebooting, the OS
//    silently zaps the fresh install. `defuse_pending_rename_for` reads
//    the key, filters out matching sources, and rewrites.
//
// 2. **Uninstalled marker** — a DWORD flag under a camouflaged HKLM
//    subkey, set by `uninstall::teardown` and cleared by
//    `install::fresh_install`. Distinguishes "user intentionally
//    uninstalled" (watchdog bails) from "state.dat vanished
//    accidentally" (watchdog resurrects). Without this marker, deleting
//    state.dat alone would silently kill the app.
//
// Pure codecs / filters / predicates are Linux-testable ; only the
// actual registry syscalls are `#[cfg(windows)]`.

use std::path::Path;

use anyhow::Result;

/// Parse a `REG_MULTI_SZ` payload : a sequence of null-terminated wide
/// strings, terminated by an extra null (double-null at end). Returns
/// the strings in order. Empty strings are legal (they mean "delete on
/// reboot" when they appear as the destination half of a rename pair).
pub fn parse_multi_sz(bytes: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0 {
            if i == start {
                // Terminator (double-null).
                break;
            }
            out.push(String::from_utf16_lossy(&bytes[start..i]));
            start = i + 1;
        }
        i += 1;
    }
    out
}

/// Emit a `REG_MULTI_SZ` payload : each string null-terminated,
/// followed by an extra null. Empty inputs still emit the terminator
/// so the registry value is a valid (empty) multi-sz.
pub fn encode_multi_sz(strings: &[String]) -> Vec<u16> {
    let mut out = Vec::new();
    for s in strings {
        out.extend(s.encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

/// Filter a list of pending rename pairs, dropping any entry whose
/// source path (after `\??\` prefix strip and case-insensitive compare)
/// matches any of `exclude`. Windows registers rename sources with the
/// NT-style `\??\` prefix ; strip it before comparing. Case-insensitive
/// because Windows filesystems are.
pub fn filter_pending_rename_operations(
    entries: &[(String, String)],
    exclude: &[&Path],
) -> Vec<(String, String)> {
    let exclude_lc: Vec<String> = exclude
        .iter()
        .map(|p| p.to_string_lossy().to_lowercase())
        .collect();
    entries
        .iter()
        .cloned()
        .filter(|(src, _dst)| {
            let stripped = src.strip_prefix(r"\??\").unwrap_or(src).to_lowercase();
            !exclude_lc.iter().any(|ex| stripped == *ex)
        })
        .collect()
}

/// Convert a flat multi-sz list into (source, destination) pairs. An
/// empty destination string means "delete on reboot".
pub fn pairs_from_multi_sz(entries: &[String]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut iter = entries.iter();
    while let Some(src) = iter.next() {
        let dst = iter.next().cloned().unwrap_or_default();
        pairs.push((src.clone(), dst));
    }
    pairs
}

/// Flatten (source, destination) pairs back to a multi-sz list.
pub fn multi_sz_from_pairs(pairs: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::with_capacity(pairs.len() * 2);
    for (src, dst) in pairs {
        out.push(src.clone());
        out.push(dst.clone());
    }
    out
}

/// Remove any `PendingFileRenameOperations` entry whose source matches
/// one of `paths`. Best-effort : any registry error is logged and the
/// caller continues. Returns `Ok(())` on success OR when there was
/// nothing to defuse ; only surfaces an error when the registry read
/// half-succeeded but the write failed (rare, requires admin).
#[cfg(windows)]
pub fn defuse_pending_rename_for(paths: &[&Path]) -> Result<()> {
    use std::iter::once;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_MULTI_SZ,
    };

    let subkey: Vec<u16> =
        r"SYSTEM\CurrentControlSet\Control\Session Manager"
            .encode_utf16()
            .chain(once(0))
            .collect();
    let value_name: Vec<u16> = "PendingFileRenameOperations"
        .encode_utf16()
        .chain(once(0))
        .collect();

    unsafe {
        let mut hkey = HKEY::default();
        let open = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_READ | KEY_SET_VALUE,
            &mut hkey,
        );
        if open != ERROR_SUCCESS {
            tracing::debug!(err = ?open, "defuse_pending_rename: RegOpenKeyExW failed (probably no key), skipping");
            return Ok(());
        }
        // Two-call pattern : first query returns required size in bytes.
        let mut size: u32 = 0;
        let query1 = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        );
        if query1 != ERROR_SUCCESS || size == 0 {
            let _ = RegCloseKey(hkey);
            tracing::debug!("defuse_pending_rename: value absent, nothing to defuse");
            return Ok(());
        }
        let cap_u16 = (size as usize).div_ceil(2);
        let mut buf: Vec<u16> = vec![0u16; cap_u16];
        let mut size2 = size;
        let query2 = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut size2),
        );
        if query2 != ERROR_SUCCESS {
            let _ = RegCloseKey(hkey);
            tracing::warn!(err = ?query2, "defuse_pending_rename: RegQueryValueExW failed");
            return Ok(());
        }
        // The actual number of u16s written = size2 / 2.
        buf.truncate((size2 as usize).div_ceil(2));
        let strings = parse_multi_sz(&buf);
        let pairs = pairs_from_multi_sz(&strings);
        let filtered = filter_pending_rename_operations(&pairs, paths);

        if filtered.len() == pairs.len() {
            let _ = RegCloseKey(hkey);
            tracing::debug!(
                pairs = pairs.len(),
                "defuse_pending_rename: no matching entries, unchanged",
            );
            return Ok(());
        }

        let flat = multi_sz_from_pairs(&filtered);
        let encoded = encode_multi_sz(&flat);
        let byte_len = (encoded.len() * 2) as u32;
        let byte_slice = std::slice::from_raw_parts(encoded.as_ptr() as *const u8, byte_len as usize);
        let write = RegSetValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            REG_MULTI_SZ,
            Some(byte_slice),
        );
        let _ = RegCloseKey(hkey);
        if write != ERROR_SUCCESS {
            anyhow::bail!("RegSetValueExW failed: {:?}", write);
        }
        tracing::info!(
            removed = pairs.len() - filtered.len(),
            remaining = filtered.len(),
            "defuse_pending_rename: rewrote key",
        );
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn defuse_pending_rename_for(_paths: &[&Path]) -> Result<()> {
    anyhow::bail!("defuse_pending_rename_for is Windows-only")
}

// ---------------------------------------------------------------------------
// Uninstalled marker (HKLM DWORD)
// ---------------------------------------------------------------------------

/// Camouflaged subkey path under HKLM. The parent chain
/// (`Microsoft\Windows\CurrentVersion\Diagnostics`) is a common
/// Microsoft namespace ; `SessionHealth` mirrors the service naming
/// (`WindowsSystemHealth`). Values written here are indistinguishable
/// from legitimate OS telemetry state at casual inspection.
pub const UNINSTALLED_MARKER_SUBKEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Diagnostics\SessionHealth";

/// DWORD value name. `1` = user has intentionally uninstalled ;
/// absent or `0` = no uninstall intent recorded.
pub const UNINSTALLED_MARKER_VALUE: &str = "Uninstalled";

/// Pure predicate : should the watchdog respect an uninstall intent and
/// bail (skip resurrection) ? Extracted for Linux-side unit tests.
/// Truth table :
///   state.dat present + any marker  → false (normal SCM classify path)
///   state.dat absent + marker set   → true  (respect intent)
///   state.dat absent + marker unset → false (treat as tampering,
///                                            resurrect via SCM classify)
pub fn should_watchdog_bail(state_dat_present: bool, marker_present: bool) -> bool {
    !state_dat_present && marker_present
}

#[cfg(windows)]
pub fn mark_uninstalled() -> Result<()> {
    use std::iter::once;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE,
        REG_CREATE_KEY_DISPOSITION, REG_DWORD, REG_OPTION_NON_VOLATILE,
    };

    let subkey: Vec<u16> = UNINSTALLED_MARKER_SUBKEY
        .encode_utf16()
        .chain(once(0))
        .collect();
    let value_name: Vec<u16> = UNINSTALLED_MARKER_VALUE
        .encode_utf16()
        .chain(once(0))
        .collect();

    unsafe {
        let mut hkey = HKEY::default();
        let mut disposition = REG_CREATE_KEY_DISPOSITION(0);
        let create = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            Some(&mut disposition),
        );
        if create != ERROR_SUCCESS {
            anyhow::bail!("mark_uninstalled: RegCreateKeyExW failed: {:?}", create);
        }
        let value: u32 = 1;
        let bytes = value.to_le_bytes();
        let write = RegSetValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            REG_DWORD,
            Some(&bytes),
        );
        let _ = RegCloseKey(hkey);
        if write != ERROR_SUCCESS {
            anyhow::bail!("mark_uninstalled: RegSetValueExW failed: {:?}", write);
        }
        tracing::info!("mark_uninstalled: Uninstalled=1 set in HKLM marker key");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn mark_uninstalled() -> Result<()> {
    anyhow::bail!("mark_uninstalled is Windows-only")
}

#[cfg(windows)]
pub fn clear_uninstalled_marker() -> Result<()> {
    use std::iter::once;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE,
    };

    let subkey: Vec<u16> = UNINSTALLED_MARKER_SUBKEY
        .encode_utf16()
        .chain(once(0))
        .collect();
    let value_name: Vec<u16> = UNINSTALLED_MARKER_VALUE
        .encode_utf16()
        .chain(once(0))
        .collect();

    unsafe {
        let mut hkey = HKEY::default();
        let open = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if open == ERROR_FILE_NOT_FOUND {
            tracing::debug!("clear_uninstalled_marker: subkey absent, nothing to clear");
            return Ok(());
        }
        if open != ERROR_SUCCESS {
            tracing::warn!(err = ?open, "clear_uninstalled_marker: RegOpenKeyExW failed");
            return Ok(());
        }
        let del = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
        let _ = RegCloseKey(hkey);
        if del != ERROR_SUCCESS && del != ERROR_FILE_NOT_FOUND {
            tracing::warn!(err = ?del, "clear_uninstalled_marker: RegDeleteValueW failed");
        } else {
            tracing::info!("clear_uninstalled_marker: marker cleared");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn clear_uninstalled_marker() -> Result<()> {
    anyhow::bail!("clear_uninstalled_marker is Windows-only")
}

#[cfg(windows)]
pub fn is_uninstalled_marker_present() -> bool {
    use std::iter::once;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    let subkey: Vec<u16> = UNINSTALLED_MARKER_SUBKEY
        .encode_utf16()
        .chain(once(0))
        .collect();
    let value_name: Vec<u16> = UNINSTALLED_MARKER_VALUE
        .encode_utf16()
        .chain(once(0))
        .collect();

    unsafe {
        let mut hkey = HKEY::default();
        let open = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        );
        if open != ERROR_SUCCESS {
            return false;
        }
        let mut value: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let query = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            Some(&mut value as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        query == ERROR_SUCCESS && value != 0
    }
}

#[cfg(not(windows))]
pub fn is_uninstalled_marker_present() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn filter_removes_matching_source_with_nt_prefix() {
        let entries = vec![
            (r"\??\C:\Tools\SystemHealthAgent.exe".to_string(), "".to_string()),
            (r"\??\C:\Tools\SystemHealthAgent.exe.manifest".to_string(), "".to_string()),
        ];
        let exe = PathBuf::from(r"C:\Tools\SystemHealthAgent.exe");
        let manifest = PathBuf::from(r"C:\Tools\SystemHealthAgent.exe.manifest");
        let got = filter_pending_rename_operations(&entries, &[&exe, &manifest]);
        assert!(got.is_empty());
    }

    #[test]
    fn filter_preserves_unrelated_windows_update_entries() {
        // Real-world example : Windows Update queues a servicing rename.
        let entries = vec![
            (r"\??\C:\Windows\Temp\update.msu".to_string(), "".to_string()),
            (r"\??\C:\Tools\SystemHealthAgent.exe".to_string(), "".to_string()),
        ];
        let exe = PathBuf::from(r"C:\Tools\SystemHealthAgent.exe");
        let got = filter_pending_rename_operations(&entries, &[&exe]);
        assert_eq!(got.len(), 1);
        assert!(got[0].0.contains("update.msu"));
    }

    #[test]
    fn filter_case_insensitive() {
        let entries = vec![
            (r"\??\C:\Foo.exe".to_string(), "".to_string()),
        ];
        let exe = PathBuf::from(r"c:\foo.exe");
        let got = filter_pending_rename_operations(&entries, &[&exe]);
        assert!(got.is_empty());
    }

    #[test]
    fn parse_multi_sz_roundtrip() {
        let strings = vec!["hello".to_string(), "world".to_string()];
        let encoded = encode_multi_sz(&strings);
        // Trailing double-null : final entry's null + terminator null.
        assert_eq!(encoded[encoded.len() - 1], 0);
        let decoded = parse_multi_sz(&encoded);
        assert_eq!(decoded, strings);
    }

    #[test]
    fn encode_multi_sz_terminates_with_double_null() {
        let strings = vec!["x".to_string()];
        let encoded = encode_multi_sz(&strings);
        // "x\0\0" in UTF-16 = ['x' as u16, 0, 0]
        assert_eq!(encoded.len(), 3);
        assert_eq!(encoded[0] as u32, 'x' as u32);
        assert_eq!(encoded[1], 0);
        assert_eq!(encoded[2], 0);
    }

    #[test]
    fn parse_multi_sz_handles_empty_destination() {
        // "src\0\0\0" = one string then terminator
        let bytes: Vec<u16> = [b's' as u16, b'r' as u16, b'c' as u16, 0, 0].to_vec();
        let strings = parse_multi_sz(&bytes);
        assert_eq!(strings, vec!["src".to_string()]);
    }

    // ----- Uninstalled marker predicate matrix -----

    #[test]
    fn watchdog_bails_only_when_state_dat_missing_and_marker_set() {
        // Legitimate uninstall path : parent ran Nettoyer.bat, marker
        // was set, state.dat teardown ran. Watchdog respects intent.
        assert!(should_watchdog_bail(false, true));
    }

    #[test]
    fn watchdog_resurrects_when_state_dat_missing_without_marker() {
        // Tampering path : kid deleted state.dat but never triggered
        // the uninstall flow. Watchdog treats as attack and resurrects
        // (falls through to normal SCM classify → Reinstall / Start).
        assert!(!should_watchdog_bail(false, false));
    }

    #[test]
    fn watchdog_ignores_marker_when_state_dat_present() {
        // Marker leftover from an aborted uninstall then reinstall :
        // state.dat is back, marker was never cleared. State.dat wins,
        // watchdog operates normally. fresh_install clears the marker
        // on the next full install path to avoid ambiguity.
        assert!(!should_watchdog_bail(true, true));
    }

    #[test]
    fn watchdog_healthy_path_neither_bails_nor_needs_resurrection() {
        // Nominal steady state : state.dat present, no marker. Watchdog
        // falls through to SCM classify (Nop / Start / Reinstall).
        assert!(!should_watchdog_bail(true, false));
    }
}
