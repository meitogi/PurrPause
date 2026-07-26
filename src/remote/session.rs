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
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand_core_06::{OsRng, RngCore};

use crate::modes::config::lockout::LockoutState;

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

/// Per-IP brute-force throttle for the console's `/api/auth`. Replaces the
/// single shared `Lockout` (one spammer could otherwise lock out the real
/// parent) and escalates repeat offenders. Keyed by client IP; `check` returns
/// the same `LockoutState` the auth endpoint already branches on, so the
/// `429 {"error":"locked"}` path is byte-for-byte unchanged.
///
/// Ladder: `MAX_TRIES` failed tries lock the IP for `LADDER[cycles]`, then the
/// next `MAX_TRIES` tries lock it for the next (longer) rung, saturating at
/// 24 h. A correct code clears the entry (`reset`), dropping escalation back to
/// the first rung (30 s).
#[derive(Default)]
pub struct IpThrottle {
    entries: Mutex<HashMap<IpAddr, Entry>>,
}

/// Per-IP state. `fails` counts tries within the current rung (0..MAX_TRIES);
/// `locked_until` is the active lock deadline; `cycles` = how many times this
/// IP has already been locked, i.e. the `LADDER` index for its NEXT lock.
#[derive(Default)]
struct Entry {
    fails: u32,
    locked_until: Option<Instant>,
    cycles: u32,
}

/// Failed attempts needed to trip one rung.
const MAX_TRIES: u32 = 3;

/// Escalation ladder. Each rung is applied after `MAX_TRIES` more failures,
/// saturating at the last (24 h). First offence = 3 tries / 30 s (preserves
/// TEST-PLAN-2 scenario 2).
const LADDER: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(2 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

impl IpThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock state for `ip`. `Locked` carries the remaining duration for the
    /// `429` path; an expired lock transitions the entry back to `Open`
    /// (clearing `fails` but KEEPING `cycles`, so a persistent attacker keeps
    /// escalating across lock windows).
    pub fn check(&self, ip: IpAddr, now: Instant) -> LockoutState {
        let mut map = self.entries.lock().unwrap();
        let state = match map.get_mut(&ip) {
            Some(entry) => match entry.locked_until {
                Some(until) if now < until => LockoutState::Locked {
                    remaining: until - now,
                },
                Some(_) => {
                    entry.locked_until = None;
                    entry.fails = 0;
                    LockoutState::Open
                }
                None => LockoutState::Open,
            },
            None => LockoutState::Open,
        };
        prune(&mut map, ip);
        state
    }

    /// Record one failed attempt for `ip`. Every `MAX_TRIES` failures trip the
    /// next ladder rung: lock until `now + LADDER[cycles]`, reset the try
    /// counter, and advance `cycles` (saturating at the last rung). Callers
    /// gate this behind `check` (a locked IP is 429'd before it ever gets here).
    pub fn record_failure(&self, ip: IpAddr, now: Instant) {
        let mut map = self.entries.lock().unwrap();
        let entry = map.entry(ip).or_default();
        entry.fails += 1;
        if entry.fails >= MAX_TRIES {
            let rung = (entry.cycles as usize).min(LADDER.len() - 1);
            entry.locked_until = Some(now + LADDER[rung]);
            entry.fails = 0;
            entry.cycles = entry.cycles.saturating_add(1);
        }
        prune(&mut map, ip);
    }

    /// Clear all throttle state for `ip` — called on a correct code. Escalation
    /// drops back to the first rung (30 s) on the next offence.
    pub fn reset(&self, ip: IpAddr) {
        self.entries.lock().unwrap().remove(&ip);
    }
}

/// Reclaim memory: drop every entry EXCEPT `keep` that carries no live state
/// (no active/pending lock and no in-progress tries). The active IP is spared
/// so its escalation survives its own lock cycles. The map is therefore bounded
/// by the distinct client IPs currently mid-attack — a handful on a LAN; a
/// stopped attacker leaves at most one idle entry until the next sweep.
fn prune(map: &mut HashMap<IpAddr, Entry>, keep: IpAddr) {
    map.retain(|&ip, e| ip == keep || e.locked_until.is_some() || e.fails > 0);
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

    fn ip_a() -> IpAddr {
        IpAddr::from([10, 0, 0, 1])
    }
    fn ip_b() -> IpAddr {
        IpAddr::from([10, 0, 0, 2])
    }

    /// Drive `n` failures for `ip` at instant `now`.
    fn fail_n(t: &IpThrottle, ip: IpAddr, now: Instant, n: u32) {
        for _ in 0..n {
            t.record_failure(ip, now);
        }
    }

    #[test]
    fn throttle_locks_after_three_failures_first_rung_30s() {
        let t = IpThrottle::new();
        let start = Instant::now();
        assert_eq!(t.check(ip_a(), start), LockoutState::Open);
        t.record_failure(ip_a(), start);
        t.record_failure(ip_a(), start);
        // Two failures: still open.
        assert_eq!(t.check(ip_a(), start), LockoutState::Open);
        // Third trips the first rung (30 s).
        t.record_failure(ip_a(), start);
        assert!(matches!(
            t.check(ip_a(), start),
            LockoutState::Locked { .. }
        ));
        assert!(matches!(
            t.check(ip_a(), start + Duration::from_secs(29)),
            LockoutState::Locked { .. }
        ));
        assert_eq!(
            t.check(ip_a(), start + Duration::from_secs(30)),
            LockoutState::Open
        );
    }

    #[test]
    fn throttle_escalates_through_the_ladder_and_saturates() {
        let t = IpThrottle::new();
        let mut now = Instant::now();

        // Rungs in order: 30 s, 2 min, 1 h, 24 h, then saturates at 24 h.
        for expected in [30u64, 120, 3600, 86_400, 86_400] {
            fail_n(&t, ip_a(), now, MAX_TRIES);
            // Locked right up to (but not at) the rung boundary.
            assert!(
                matches!(
                    t.check(ip_a(), now + Duration::from_secs(expected - 1)),
                    LockoutState::Locked { .. }
                ),
                "expected locked at +{}s for rung {expected}s",
                expected - 1
            );
            assert_eq!(
                t.check(ip_a(), now + Duration::from_secs(expected)),
                LockoutState::Open,
                "expected open at +{expected}s"
            );
            // Advance past this rung to hammer the next one.
            now += Duration::from_secs(expected);
        }
    }

    #[test]
    fn throttle_is_per_ip() {
        let t = IpThrottle::new();
        let now = Instant::now();
        fail_n(&t, ip_a(), now, MAX_TRIES);
        assert!(matches!(
            t.check(ip_a(), now),
            LockoutState::Locked { .. }
        ));
        // A different IP is entirely unaffected.
        assert_eq!(t.check(ip_b(), now), LockoutState::Open);
    }

    #[test]
    fn throttle_reset_drops_back_to_first_rung() {
        let t = IpThrottle::new();
        let start = Instant::now();
        // First lock cycle → escalation advances to the 2-min rung next time.
        fail_n(&t, ip_a(), start, MAX_TRIES);
        assert!(matches!(
            t.check(ip_a(), start),
            LockoutState::Locked { .. }
        ));
        // A correct code resets the IP.
        t.reset(ip_a());
        assert_eq!(t.check(ip_a(), start), LockoutState::Open);
        // The next offence locks for the FIRST rung again (30 s), proving the
        // escalation counter was cleared — not the 2-min second rung.
        let later = start + Duration::from_secs(1);
        fail_n(&t, ip_a(), later, MAX_TRIES);
        assert!(matches!(
            t.check(ip_a(), later + Duration::from_secs(29)),
            LockoutState::Locked { .. }
        ));
        assert_eq!(
            t.check(ip_a(), later + Duration::from_secs(30)),
            LockoutState::Open
        );
    }
}
