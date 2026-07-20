// Monotonic-oracle clock reader used by the scheduler's clock-shift
// detector. `QueryUnbiasedInterruptTime` returns 100-nanosecond units
// since boot, EXCLUDING time the system spent suspended / hibernated /
// (on VMs) frozen. Wall clock counts all of that ; comparing the two
// deltas across a tick isolates VM freeze, S4 hibernate without power
// broadcast, and admin/NTP clock tampering.
//
// Windows 7+. Never fails at runtime — the underlying kernel counter is
// always present, unlike GetSystemTimeAsFileTime which can jump on NTP
// resync but has no failure mode we could exploit.

use windows::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;

/// Read the current unbiased interrupt time in 100 ns units.
///
/// The count starts at 0 on boot and is monotonic — even if the wall
/// clock is set backward. Sleep / hibernate / VM freeze do NOT advance
/// this counter. Wraparound at ~58 500 years, ignorable.
pub fn query_100ns() -> u64 {
    let mut ticks: u64 = 0;
    // SAFETY: `QueryUnbiasedInterruptTime` writes a `u64` to the pointer
    // we pass in. It cannot fail — the return `BOOL` is always non-zero
    // on documented platforms (Win7+), and we compile only on Windows.
    unsafe {
        let _ = QueryUnbiasedInterruptTime(&mut ticks);
    }
    ticks
}

/// Convert a raw 100 ns tick count to whole seconds.
pub fn ticks_to_secs(ticks_100ns: u64) -> u64 {
    ticks_100ns / 10_000_000
}
