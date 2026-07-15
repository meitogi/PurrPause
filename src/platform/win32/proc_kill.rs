// Orphan WebView2-worker sweep.
//
// Background : session 9 Track B-primary (event_loop.run_return + clean
// WebView2 drop) was reverted in 0.7.2 because run_return on tao Windows
// breaks the LL keyboard hook. Reverting means WebView2 workers
// (msedgewebview2.exe children of our popup) leak when a popup exits.
// Over hours of use they accumulate — memory + open handles.
//
// Sweep strategy — deliberately simpler than the NtQueryInformationProcess
// PEB-walk originally scoped :
//
//   1. CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS) → iterate all processes.
//   2. Filter by szExeFile == "msedgewebview2.exe".
//   3. For each candidate, OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION |
//      PROCESS_TERMINATE) + QueryFullProcessImageNameW → full exe path.
//   4. If the exe path starts with our own `<install>\Data\WebView2\`,
//      TerminateProcess. Other WebView2 hosts on the machine (Teams, VS Code,
//      Notion, …) run from their own Data\WebView2 subtree — path match
//      confines the sweep to our workers.
//
// We don't need to read the command line — WebView2 loader deploys the
// runtime binaries under our data folder, and workers exec from there.
// The exe path IS the discriminator ; no PEB walk, no unsafe memory read.

use std::mem::size_of;
use std::path::Path;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

/// Kill every `msedgewebview2.exe` whose full image path lives under
/// `webview2_root` (typically `<install>\Data\WebView2`). Best-effort :
/// snapshot / OpenProcess / terminate failures are logged at debug and
/// skipped ; there's no useful recovery.
pub fn kill_orphan_webview2_workers(webview2_root: &Path) {
    let root_lower = match webview2_root.canonicalize().ok() {
        Some(p) => p.to_string_lossy().to_lowercase(),
        // Missing root → nothing to match against ; no workers can be ours.
        None => return,
    };

    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) if !h.is_invalid() => h,
        _ => {
            tracing::debug!("CreateToolhelp32Snapshot failed - skipping orphan sweep");
            return;
        }
    };

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    let mut killed = 0u32;

    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            if exe_name_matches(&entry.szExeFile, "msedgewebview2.exe") {
                if let Some(path) = full_process_image_path(entry.th32ProcessID) {
                    if path.to_lowercase().starts_with(&root_lower) {
                        if terminate_by_pid(entry.th32ProcessID) {
                            killed += 1;
                            tracing::info!(
                                pid = entry.th32ProcessID,
                                "orphan msedgewebview2.exe worker terminated"
                            );
                        }
                    }
                }
            }
            entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }

    unsafe {
        let _ = CloseHandle(snapshot);
    }

    if killed > 0 {
        tracing::info!(count = killed, "orphan WebView2 workers swept");
    }
}

fn exe_name_matches(sz_exe_file: &[u16; 260], expected_lower: &str) -> bool {
    let end = sz_exe_file.iter().position(|&c| c == 0).unwrap_or(260);
    let name = String::from_utf16_lossy(&sz_exe_file[..end]).to_lowercase();
    name == expected_lower
}

fn full_process_image_path(pid: u32) -> Option<String> {
    let handle = open_process_for_query(pid)?;
    let mut buf = [0u16; 32768]; // MAX_PATH_UNICODE = 32767 chars
    let mut size = buf.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0), // 0 = Win32 path format
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if result.is_err() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..size as usize]))
}

fn open_process_for_query(pid: u32) -> Option<HANDLE> {
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, false, pid) }
    {
        Ok(h) if !h.is_invalid() => Some(h),
        // Access denied is expected for system / other-user processes ; skip silently.
        _ => None,
    }
}

fn terminate_by_pid(pid: u32) -> bool {
    let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        Ok(h) if !h.is_invalid() => h,
        _ => return false,
    };
    let ok = unsafe { TerminateProcess(handle, 0) }.is_ok();
    unsafe {
        let _ = CloseHandle(handle);
    }
    ok
}

