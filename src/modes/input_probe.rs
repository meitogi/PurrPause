// User-session input probe. Spawned by the service via
// `platform::win32::spawn_user::spawn_in_active_user_session`, runs
// as the console user, polls `GetLastInputInfo` in a loop, and writes
// the absolute tick of the last input to a shared file so the SYSTEM
// service can read it back.
//
// # Why this exists
//
// A Windows service runs in Session 0. `GetLastInputInfo` returns
// input state for the CALLING session's window station — Session 0
// never receives user input, so a naive call from the service returns
// info.dwTime = 0 forever. Impersonation via `ImpersonateLoggedOnUser`
// changes the thread's security token but NOT its window station
// (window stations are session-scoped and not affected by token
// swaps), so impersonation is ineffective for this API specifically.
//
// The workaround : spawn a lightweight helper in the interactive user
// session (this module), which naturally sees the user's real input
// events, and pipe the data back via a file the service can read.
//
// # File format
//
// `<exe_dir>/Data/lastinput.dat` — 16 bytes big-endian :
//   [0..8]  u64 last_input_tick_ms  absolute GetTickCount64 of last input
//   [8..16] u64 written_at_tick_ms  GetTickCount64 at the time of write
//
// The service checks `written_at_tick_ms` staleness to detect a dead
// probe and falls back to "assume active" (idle_secs = 0) in that case.
//
// # Lifecycle
//
// The service spawns the probe once at startup and monitors the child
// handle every tick. If the probe crashes, the service respawns it.
// Consecutive write failures inside the probe itself (~5×) cause the
// probe to exit cleanly — the service then respawns from scratch,
// which is more likely to succeed than the probe fighting a broken
// filesystem state.

use anyhow::{Context, Result};

#[cfg(windows)]
use std::{fs, io::Write as _, path::Path, thread, time::Duration};

#[cfg(windows)]
use windows::Win32::System::SystemInformation::GetTickCount64;
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

/// Poll cadence — matches the service tick so the file is always
/// fresh enough for the service's per-tick read.
#[cfg(windows)]
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Consecutive write failures before the probe gives up and exits.
/// The service's respawn loop then re-creates a fresh probe process,
/// which is more resilient than the probe fighting an ongoing IO glitch.
#[cfg(windows)]
const MAX_CONSECUTIVE_WRITE_FAILURES: u32 = 5;

/// Filename of the probe artefact under `<exe_dir>/Data/`.
pub const LASTINPUT_FILE: &str = "lastinput.dat";

#[cfg(windows)]
pub fn run() -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let parent = exe.parent().context("current_exe parent")?;
    let data_dir = parent.join("Data");
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("mkdir {}", data_dir.display()))?;
    let path = data_dir.join(LASTINPUT_FILE);

    tracing::info!(
        path = %path.display(),
        interval_secs = POLL_INTERVAL.as_secs(),
        "input-probe: starting poll loop"
    );

    let mut consecutive_failures: u32 = 0;
    loop {
        match write_snapshot(&path) {
            Ok(()) => consecutive_failures = 0,
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!(
                    error = %e,
                    consecutive = consecutive_failures,
                    "input-probe: write failed"
                );
                if consecutive_failures >= MAX_CONSECUTIVE_WRITE_FAILURES {
                    tracing::error!(
                        max = MAX_CONSECUTIVE_WRITE_FAILURES,
                        "input-probe: giving up after max consecutive failures"
                    );
                    return Err(e);
                }
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn write_snapshot(path: &Path) -> Result<()> {
    let current_tick = unsafe { GetTickCount64() };
    let last_input_tick = read_last_input_tick_abs(current_tick);

    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&last_input_tick.to_be_bytes());
    buf[8..16].copy_from_slice(&current_tick.to_be_bytes());

    // Atomic tmp + rename so the service never reads a half-written
    // file (would look like a huge idle_secs jump on the service side).
    let tmp = path.with_extension("dat.tmp");
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("create tmp {}", tmp.display()))?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Convert `GetLastInputInfo`'s u32 tick (wraps every 49.7 days) into
/// the absolute u64 tick of the last input event, using the u32 delta
/// arithmetic which is wrap-safe even across the boundary.
#[cfg(windows)]
fn read_last_input_tick_abs(current_tick: u64) -> u64 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            let current_u32 = current_tick as u32;
            let idle_ms_u32 = current_u32.wrapping_sub(info.dwTime);
            current_tick.saturating_sub(idle_ms_u32 as u64)
        } else {
            // API failure — pretend input just happened so idle_secs
            // reads as 0 and the service errs toward "assume active".
            current_tick
        }
    }
}

#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!("input-probe is Windows-only")
}
