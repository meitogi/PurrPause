// Keyboard + mouse idleness signal for the service tick loop.
// Does NOT call `GetLastInputInfo` directly (see below for why) —
// reads the timestamp written by the user-session input probe
// (`crate::modes::input_probe`) at `<exe_dir>/Data/lastinput.dat`.
//
// # Session 0 pitfall
//
// A Windows service runs as SYSTEM in Session 0 which never receives
// user input. `GetLastInputInfo` reads via a session-scoped window
// station : Session 0's WinSta0 has no interactive input, so a direct
// call from the service returns `info.dwTime = 0` forever. Naive
// `ImpersonateLoggedOnUser` swaps the thread's security token but not
// its window station (they're not scoped by token), so impersonation
// is ineffective for this API.
//
// The workaround (b.8) : a lightweight `--input-probe` child process
// spawned in the interactive user session polls `GetLastInputInfo` on
// its own turf and writes the absolute u64 tick of the last input to
// a shared file. This module reads it and computes idle seconds
// against the current tick. See `crate::modes::input_probe` for the
// probe side.
//
// # Fallbacks
//
// File missing / short read / older than `MAX_PROBE_STALENESS_MS` →
// return 0 (assume active). Any probe glitch errs toward firing the
// popup rather than silently deferring forever.

use std::fs;
use std::path::PathBuf;

use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::modes::input_probe::LASTINPUT_FILE;

/// Beyond this staleness we treat the probe as dead. 30 s = 6× the
/// probe's 5 s poll cadence, so a healthy probe never hits the
/// threshold and a crashed one is detected within one service tick.
const MAX_PROBE_STALENESS_MS: u64 = 30_000;

/// Seconds since the last keyboard or mouse input on the interactive
/// user session, sourced from the user-session probe file. Returns 0
/// on any file-read / staleness failure so the tick errs toward
/// "assume active".
pub fn idle_seconds() -> u64 {
    let path = probe_file_path();
    let buf = match fs::read(&path) {
        Ok(b) if b.len() == 16 => b,
        Ok(b) => {
            tracing::warn!(
                len = b.len(),
                path = %path.display(),
                "lastinput.dat wrong size - assuming active"
            );
            return 0;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "lastinput.dat read failed - assuming active (probe may be respawning)"
            );
            return 0;
        }
    };

    let last_input_tick = u64::from_be_bytes(buf[0..8].try_into().unwrap());
    let written_at_tick = u64::from_be_bytes(buf[8..16].try_into().unwrap());
    let current_tick = unsafe { GetTickCount64() };
    let staleness_ms = current_tick.saturating_sub(written_at_tick);
    if staleness_ms > MAX_PROBE_STALENESS_MS {
        tracing::warn!(
            staleness_ms,
            "input-probe stale - assuming active (probe may have crashed)"
        );
        return 0;
    }
    let idle_ms = current_tick.saturating_sub(last_input_tick);
    idle_ms / 1000
}

fn probe_file_path() -> PathBuf {
    // Same convention as `modes::input_probe` : <exe_dir>/Data/lastinput.dat.
    // Both service (SYSTEM, current_exe = registered ImagePath) and
    // probe (console user session, current_exe = same ImagePath via
    // CreateProcessAsUserW) resolve to the same absolute path.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.join("Data").join(LASTINPUT_FILE);
        }
    }
    PathBuf::from("Data").join(LASTINPUT_FILE)
}
