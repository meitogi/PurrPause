// Per-process-tree CPU / GPU / memory sampler for the popup benchmark.
//
// The popup host (SystemHealthAgent --popup) does almost none of the
// rendering work : ThorVG WASM rasterization and GPU compositing live
// in the msedgewebview2.exe children. The sampler therefore walks the
// whole process tree each tick (Toolhelp snapshot, same idiom as
// proc_kill.rs) and emits one BenchRow per live process.
//
// GPU numbers come from PDH — the same counters Task Manager's GPU
// columns read : `\GPU Engine(pid_*)\Utilization Percentage` (summed
// across a pid's engine instances) and
// `\GPU Process Memory(pid_*)\Dedicated Usage`. Counters are added via
// PdhAddEnglishCounterW so the paths resolve on localized Windows.
// A virtual GPU driver (Parallels, RDP) may expose no counterset at
// all : the sampler then degrades to CPU+memory-only with a single
// warn, GPU columns stay 0 — never aborts the bench.
//
// Role labels are exe-name based (`host` / `webview` / exe stem) —
// reading msedgewebview2's `--type=` cmdline would need a PEB walk,
// rejected by project policy (see proc_kill.rs header). The GPU-heavy
// webview row identifies the renderer process empirically.

use std::collections::HashMap;
use std::mem::size_of;
use std::time::Duration;

use anyhow::{ensure, Context, Result};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::bench::{cpu_delta_ms_pct, descendant_pids, filetime_to_u64, parse_gpu_instance_pid, BenchRow};

/// Closes the PDH query on drop (closes its counters too). Best-effort :
/// the popup exits through tao's diverging `run` (process::exit), so
/// Drop may never run — the OS reclaims the query then.
struct PdhQueryGuard(PDH_HQUERY);

impl Drop for PdhQueryGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.0);
        }
    }
}

/// RAII process HANDLE (model : SpawnedChild in spawn_user.rs).
struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct ProcSampler {
    root_pid: u32,
    query: PdhQueryGuard,
    /// `None` when the GPU counterset is unavailable (virtual GPU).
    gpu_util: Option<PDH_HCOUNTER>,
    gpu_mem: Option<PDH_HCOUNTER>,
    /// pid -> last kernel+user total in 100 ns ticks.
    cpu_prev: HashMap<u32, u64>,
}

impl ProcSampler {
    /// Opens the PDH query, adds both wildcard GPU counters and runs the
    /// priming collect (utilization is a rate counter — it needs two
    /// collections before the first formatted read is valid).
    pub fn new(root_pid: u32) -> Result<Self> {
        let mut raw_query = PDH_HQUERY::default();
        let st = unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut raw_query) };
        ensure!(st == 0, "PdhOpenQueryW: 0x{st:08x}");
        let query = PdhQueryGuard(raw_query);

        let add = |path: windows::core::PCWSTR, label: &str| -> Option<PDH_HCOUNTER> {
            let mut counter = PDH_HCOUNTER::default();
            let st = unsafe { PdhAddEnglishCounterW(query.0, path, 0, &mut counter) };
            if st == 0 {
                Some(counter)
            } else {
                tracing::warn!(status = format!("0x{st:08x}"), counter = label, "PDH GPU counter unavailable - gpu columns will be 0");
                None
            }
        };
        let gpu_util = add(w!(r"\GPU Engine(pid_*)\Utilization Percentage"), "gpu-engine");
        let gpu_mem = add(w!(r"\GPU Process Memory(pid_*)\Dedicated Usage"), "gpu-process-memory");

        // Priming collect ; status intentionally ignored (a query with
        // zero counters, or NO_DATA before instances exist, is fine).
        let _ = unsafe { PdhCollectQueryData(query.0) };

        Ok(Self {
            root_pid,
            query,
            gpu_util,
            gpu_mem,
            cpu_prev: HashMap::new(),
        })
    }

    /// One tick : PDH collect + tree walk + per-pid CPU / memory.
    /// `interval` is the measured wall time since the previous call
    /// (drives cpu_pct). First sighting of a pid yields cpu_ms = 0 /
    /// cpu_pct = 0 (baseline tick).
    pub fn sample(&mut self, interval: Duration) -> Result<Vec<BenchRow>> {
        // GPU first : one collect refreshes both counters.
        let mut gpu_pct_by_pid: HashMap<u32, f64> = HashMap::new();
        let mut gpu_mem_by_pid: HashMap<u32, f64> = HashMap::new();
        if self.gpu_util.is_some() || self.gpu_mem.is_some() {
            let st = unsafe { PdhCollectQueryData(self.query.0) };
            if st == 0 {
                if let Some(c) = self.gpu_util {
                    for (name, value) in formatted_counter_array(c)? {
                        if let Some(pid) = parse_gpu_instance_pid(&name) {
                            *gpu_pct_by_pid.entry(pid).or_default() += value;
                        }
                    }
                }
                if let Some(c) = self.gpu_mem {
                    for (name, value) in formatted_counter_array(c)? {
                        if let Some(pid) = parse_gpu_instance_pid(&name) {
                            *gpu_mem_by_pid.entry(pid).or_default() += value;
                        }
                    }
                }
            } else {
                tracing::debug!(status = format!("0x{st:08x}"), "PdhCollectQueryData failed - gpu columns 0 this tick");
            }
        }

        let table = snapshot_process_table().context("process snapshot")?;
        let parent_map: Vec<(u32, u32)> = table.iter().map(|&(pid, ppid, _)| (pid, ppid)).collect();
        let names: HashMap<u32, &str> = table.iter().map(|&(pid, _, ref name)| (pid, name.as_str())).collect();
        let tree = descendant_pids(self.root_pid, &parent_map);

        let mut rows = Vec::with_capacity(tree.len());
        for &pid in &tree {
            let Some((cpu_total, ws, private)) = query_cpu_mem(pid) else {
                // Process died between snapshot and OpenProcess, or
                // access denied — skip silently (proc_kill precedent).
                continue;
            };
            let (cpu_ms, cpu_pct) = match self.cpu_prev.get(&pid) {
                Some(&prev) => cpu_delta_ms_pct(prev, cpu_total, interval),
                None => (0, 0.0),
            };
            self.cpu_prev.insert(pid, cpu_total);
            rows.push(BenchRow {
                pid,
                role: role_for(pid, self.root_pid, names.get(&pid).copied()),
                cpu_ms,
                cpu_pct,
                gpu_pct: gpu_pct_by_pid.get(&pid).copied().unwrap_or(0.0),
                gpu_mem_bytes: gpu_mem_by_pid.get(&pid).copied().unwrap_or(0.0) as u64,
                workingset_bytes: ws,
                private_bytes: private,
            });
        }

        // Prune baselines of pids that left the tree (dead children).
        self.cpu_prev.retain(|pid, _| tree.contains(pid));

        Ok(rows)
    }
}

fn role_for(pid: u32, root_pid: u32, exe_name: Option<&str>) -> String {
    if pid == root_pid {
        return "host".to_string();
    }
    match exe_name {
        Some("msedgewebview2.exe") => "webview".to_string(),
        Some(name) => name.strip_suffix(".exe").unwrap_or(name).to_string(),
        None => "unknown".to_string(),
    }
}

/// (pid, ppid, exe_name_lowercase) for every live process. Toolhelp
/// idiom mirrored from proc_kill.rs.
fn snapshot_process_table() -> Result<Vec<(u32, u32, String)>> {
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) if !h.is_invalid() => ProcessHandle(h),
        _ => anyhow::bail!("CreateToolhelp32Snapshot failed"),
    };

    let mut out = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok() {
        loop {
            let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase();
            out.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
            entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
                break;
            }
        }
    }
    Ok(out)
}

/// (kernel+user 100 ns total, WorkingSetSize, PrivateUsage) for one pid.
/// `None` on OpenProcess / query failure (dead pid, access denied).
fn query_cpu_mem(pid: u32) -> Option<(u64, u64, u64)> {
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) if !h.is_invalid() => ProcessHandle(h),
        _ => return None,
    };

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) }.ok()?;
    let cpu_total = filetime_to_u64(kernel.dwHighDateTime, kernel.dwLowDateTime)
        + filetime_to_u64(user.dwHighDateTime, user.dwLowDateTime);

    let mut mem = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        GetProcessMemoryInfo(
            handle.0,
            &mut mem as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    }
    .ok()?;

    Some((cpu_total, mem.WorkingSetSize as u64, mem.PrivateUsage as u64))
}

/// Read a wildcard counter's per-instance values (PDH_FMT_DOUBLE).
/// Size-probe then fetch, bounded retries — instances can appear
/// between the probe and the fetch and grow the required buffer.
fn formatted_counter_array(counter: PDH_HCOUNTER) -> Result<Vec<(String, f64)>> {
    let mut buf_size: u32 = 0;
    let mut item_count: u32 = 0;
    let st = unsafe {
        PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut buf_size, &mut item_count, None)
    };
    if st != PDH_MORE_DATA {
        // No instances (pid_* matched nothing) or transient error — an
        // empty read is not fatal for the bench.
        return Ok(Vec::new());
    }

    for _attempt in 0..4 {
        // u64 backing keeps the buffer 8-aligned for the item structs.
        let mut buf = vec![0u64; (buf_size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        let st = unsafe {
            PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut buf_size, &mut item_count, Some(items))
        };
        match st {
            0 => {
                let slice = unsafe { std::slice::from_raw_parts(items, item_count as usize) };
                return Ok(slice
                    .iter()
                    .filter_map(|item| {
                        // szName points inside `buf`, valid within this scope.
                        let name = unsafe { item.szName.to_string() }.ok()?;
                        let status = item.FmtValue.CStatus;
                        if status != PDH_CSTATUS_VALID_DATA && status != PDH_CSTATUS_NEW_DATA {
                            return None;
                        }
                        Some((name, unsafe { item.FmtValue.Anonymous.doubleValue }))
                    })
                    .collect());
            }
            PDH_MORE_DATA => continue, // buf_size was updated — retry bigger
            other => anyhow::bail!("PdhGetFormattedCounterArrayW: 0x{other:08x}"),
        }
    }
    anyhow::bail!("PDH buffer size kept growing after 4 attempts")
}
