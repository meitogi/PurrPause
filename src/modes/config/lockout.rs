// Passcode gate throttle — pure state machine, no I/O, no egui refs.
//
// After N failed verifies inside a window, arms a fixed-duration
// lockout. `check(now)` tells the caller whether an incoming submit
// should be honoured or rejected *without hashing the passcode*
// (protects against brute-force + timing side-channels). Expiry
// auto-resets the counter on the next `check`.

use std::time::{Duration, Instant};

pub const MAX_ATTEMPTS: u8 = 3;
pub const LOCKOUT_DURATION: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Default)]
pub struct Lockout {
    pub attempts: u8,
    pub locked_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockoutState {
    Open,
    Locked { remaining: Duration },
}

impl Lockout {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(&mut self, now: Instant) -> LockoutState {
        if let Some(until) = self.locked_until {
            if now >= until {
                self.locked_until = None;
                self.attempts = 0;
                LockoutState::Open
            } else {
                LockoutState::Locked { remaining: until - now }
            }
        } else {
            LockoutState::Open
        }
    }

    pub fn record_failure(&mut self, now: Instant) -> LockoutState {
        self.attempts = self.attempts.saturating_add(1);
        if self.attempts >= MAX_ATTEMPTS {
            self.locked_until = Some(now + LOCKOUT_DURATION);
        }
        self.check(now)
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
        self.locked_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_when_never_hit() {
        let mut l = Lockout::new();
        assert_eq!(l.check(Instant::now()), LockoutState::Open);
    }

    #[test]
    fn locks_after_three_failures() {
        let now = Instant::now();
        let mut l = Lockout::new();
        assert_eq!(l.record_failure(now), LockoutState::Open);
        assert_eq!(l.record_failure(now), LockoutState::Open);
        assert!(matches!(l.record_failure(now), LockoutState::Locked { .. }));
    }

    #[test]
    fn rejects_attempts_during_window() {
        let start = Instant::now();
        let mut l = Lockout::new();
        for _ in 0..3 {
            l.record_failure(start);
        }
        assert!(matches!(l.check(start + Duration::from_secs(1)), LockoutState::Locked { .. }));
        assert!(matches!(l.check(start + Duration::from_secs(29)), LockoutState::Locked { .. }));
    }

    #[test]
    fn expires_after_lockout_duration() {
        let start = Instant::now();
        let mut l = Lockout::new();
        for _ in 0..3 {
            l.record_failure(start);
        }
        assert_eq!(l.check(start + LOCKOUT_DURATION), LockoutState::Open);
        assert_eq!(l.attempts, 0);
        assert!(l.locked_until.is_none());
    }

    #[test]
    fn reset_clears_counter() {
        let start = Instant::now();
        let mut l = Lockout::new();
        l.record_failure(start);
        l.record_failure(start);
        l.reset();
        assert_eq!(l.attempts, 0);
        assert_eq!(l.check(start), LockoutState::Open);
    }
}
