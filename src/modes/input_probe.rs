// User-session input probe. Spawned by the service via
// `platform::win32::spawn_user::spawn_in_active_user_session`, runs
// as the console user, polls `GetLastInputInfo` in a loop, and pushes
// the absolute tick of the last input to the SYSTEM service over the
// existing named-pipe IPC (`Message::InputStamp`).
//
// # Why this exists
//
// A Windows service runs in Session 0. `GetLastInputInfo` returns
// input state for the CALLING session's window station — Session 0
// never receives user input, so a naive call from the service returns
// `info.dwTime = 0` forever. Impersonation via `ImpersonateLoggedOnUser`
// changes the thread's security token but NOT its window station
// (window stations are session-scoped and not affected by token
// swaps), so impersonation is ineffective for this API specifically.
//
// The workaround : spawn a lightweight helper in the interactive user
// session (this module), which naturally sees the user's real input
// events, and push the data back via the service's named pipe.
//
// # Transport
//
// Every `POLL_INTERVAL` (5 s) the probe :
//   1. reads `GetLastInputInfo` to compute the absolute u64 tick of
//      the last input,
//   2. calls `ipc::send(Message::InputStamp { tick_ms })` to push it
//      to the service (see `ipc.rs` for the wire format).
//
// The service's pipe thread bridges `InputStamp` onto a dedicated
// stamp mpsc, and the tick loop drains that at each 5 s tick.
//
// (b.8 wrote 16 big-endian bytes to `<exe_dir>/Data/lastinput.dat`
// via atomic tmp+rename ; b.9 dropped the file transport in favour
// of the pipe — zero disk writes, reuses existing infra.)
//
// # Lifecycle
//
// The service spawns the probe once at startup and monitors the child
// handle every tick. If the probe crashes, the service respawns it.
// Consecutive send failures inside the probe itself (~5×) cause the
// probe to exit cleanly — the service then respawns from scratch,
// which is more resilient than the probe fighting an ongoing pipe
// glitch (transient close during a service restart, for example).

use anyhow::Result;

#[cfg(windows)]
use std::{thread, time::Duration};

#[cfg(windows)]
use windows::Win32::System::SystemInformation::GetTickCount64;
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

#[cfg(windows)]
use crate::ipc::{self, Message};

/// Poll cadence — matches the service tick so a stamp lands on the
/// pipe at least once between tick loop drains.
#[cfg(windows)]
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Consecutive send failures before the probe gives up and exits.
/// The service's respawn loop then re-creates a fresh probe process,
/// which is more resilient than the probe fighting an ongoing pipe
/// glitch.
#[cfg(windows)]
const MAX_CONSECUTIVE_SEND_FAILURES: u32 = 5;

#[cfg(windows)]
pub fn run() -> Result<()> {
    tracing::info!(
        interval_secs = POLL_INTERVAL.as_secs(),
        pipe = crate::ipc::PIPE_NAME,
        "input-probe: starting poll loop (IPC transport)"
    );

    let mut consecutive_failures: u32 = 0;
    loop {
        match send_stamp() {
            Ok(()) => consecutive_failures = 0,
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!(
                    error = %e,
                    consecutive = consecutive_failures,
                    "input-probe: pipe send failed"
                );
                if consecutive_failures >= MAX_CONSECUTIVE_SEND_FAILURES {
                    tracing::error!(
                        max = MAX_CONSECUTIVE_SEND_FAILURES,
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
fn send_stamp() -> Result<()> {
    let current_tick = unsafe { GetTickCount64() };
    let last_input_tick = read_last_input_tick_abs(current_tick);
    ipc::send(Message::InputStamp { tick_ms: last_input_tick })
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
