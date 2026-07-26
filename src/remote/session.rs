// In-memory session-token store for the remote log console. This is the
// first session concept in the crate — everything else auths one-shot per
// action.
//
// Auth model (per the approved design): argon2id is verified ONCE in
// `/api/auth`; on success a 32-byte OsRng token is issued and every later
// `/api/*` validates the cookie against this store. Sliding expiry keeps a
// left-open tab alive while it's in use. Brute-force throttling reuses the
// existing `Lockout` state machine, consulted *before* hashing so a locked
// gate never pays the argon2 cost (anti-timing).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand_core_06::{OsRng, RngCore};

/// Sliding lifetime of a session token. Refreshed on every successful
/// validation — a parent who leaves the console open stays logged in; an
/// abandoned tab expires 30 min after its last request.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Token store: `token -> expiry instant`. Guarded by a `Mutex` because the
/// service serves connections thread-per-socket.
#[derive(Default)]
pub struct Sessions {
    store: Mutex<HashMap<String, Instant>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh 32-byte token (64 hex chars) with expiry `now + TTL`.
    pub fn issue(&self, now: Instant) -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let token = hex(&bytes);
        self.store
            .lock()
            .unwrap()
            .insert(token.clone(), now + SESSION_TTL);
        token
    }

    /// True iff `token` is known and unexpired. On a hit the expiry slides
    /// forward to `now + TTL`. Expired entries are pruned lazily on access.
    pub fn validate(&self, token: &str, now: Instant) -> bool {
        if token.is_empty() {
            return false;
        }
        let mut store = self.store.lock().unwrap();
        match store.get(token).copied() {
            Some(expiry) if now < expiry => {
                store.insert(token.to_string(), now + SESSION_TTL);
                true
            }
            Some(_) => {
                store.remove(token);
                false
            }
            None => false,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::config::lockout::{Lockout, LockoutState};

    #[test]
    fn issued_token_is_64_hex_chars() {
        let s = Sessions::new();
        let tok = s.issue(Instant::now());
        assert_eq!(tok.len(), 64);
        assert!(tok.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn issued_tokens_are_distinct() {
        let s = Sessions::new();
        let now = Instant::now();
        assert_ne!(s.issue(now), s.issue(now));
    }

    #[test]
    fn fresh_token_validates() {
        let s = Sessions::new();
        let now = Instant::now();
        let tok = s.issue(now);
        assert!(s.validate(&tok, now));
    }

    #[test]
    fn unknown_and_empty_tokens_reject() {
        let s = Sessions::new();
        let now = Instant::now();
        assert!(!s.validate("deadbeef", now));
        assert!(!s.validate("", now));
    }

    #[test]
    fn token_expires_after_ttl() {
        let s = Sessions::new();
        let now = Instant::now();
        let tok = s.issue(now);
        assert!(!s.validate(&tok, now + SESSION_TTL + Duration::from_secs(1)));
        // Second call still rejects (entry was pruned on the first miss).
        assert!(!s.validate(&tok, now));
    }

    #[test]
    fn validation_slides_expiry_forward() {
        let s = Sessions::new();
        let start = Instant::now();
        let tok = s.issue(start);
        // Access at +20 min refreshes expiry; +45 min from start (25 min
        // after the refresh) is therefore still valid.
        let mid = start + Duration::from_secs(20 * 60);
        assert!(s.validate(&tok, mid));
        let later = start + Duration::from_secs(45 * 60);
        assert!(s.validate(&tok, later));
    }

    #[test]
    fn lockout_reuse_locks_after_three_and_reopens() {
        // The auth endpoint reuses the existing Lockout as-is; assert the
        // 3/30s contract holds when driven from here.
        let start = Instant::now();
        let mut l = Lockout::new();
        assert_eq!(l.record_failure(start), LockoutState::Open);
        assert_eq!(l.record_failure(start), LockoutState::Open);
        assert!(matches!(
            l.record_failure(start),
            LockoutState::Locked { .. }
        ));
        assert!(matches!(
            l.check(start + Duration::from_secs(29)),
            LockoutState::Locked { .. }
        ));
        assert_eq!(l.check(start + Duration::from_secs(30)), LockoutState::Open);
    }
}
