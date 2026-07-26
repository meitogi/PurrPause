// Read-only reachability diagnostics for the LAN log console, surfaced in the
// Sécurité tab. A correctly-configured console can still be unreachable: the
// service wasn't restarted after enabling (the accept loop reads the flag once
// at start), the active network is Public (the firewall rule is Private+Domain
// only), or the inbound rule is missing entirely. This module holds the
// platform-agnostic core — the profile-bitmask → verdict mapping and the shared
// state the UI worker fills. The live probes (loopback HTTP, INetFwPolicy2 COM)
// are Windows-only and live in the config tab / the win32 firewall module.
//
// Compiled on every platform so the pure mapping is unit-tested under Linux
// `cargo test`; its only runtime consumer is the Windows-only config UI, hence
// the dead-code allowance off-Windows.
#![cfg_attr(not(windows), allow(dead_code))]

/// Windows firewall network-profile bitmask values (`NET_FW_PROFILE2_*`).
/// Re-declared here as plain constants so the mapping stays testable without
/// the `windows` crate; the win32 layer casts the COM `LONG` to this `u32`.
pub const PROFILE_DOMAIN: u32 = 1;
pub const PROFILE_PRIVATE: u32 = 2;
pub const PROFILE_PUBLIC: u32 = 4;

/// Whether the current network profile lets the LAN reach the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileVerdict {
    /// Private and/or Domain is active — the port is open on this profile.
    Reachable,
    /// Only Public is active — the port stays closed (rule is Private+Domain).
    PublicOnly,
    /// No known profile bit set (query returned 0 / unrecognised).
    Unknown,
}

/// Map an OR-combined active-profile bitmask to a reachability verdict.
/// Reachable if *any* of Private/Domain is active (a multi-NIC host can set
/// several bits at once); Public-only is the warning case; everything else is
/// Unknown.
pub fn summarize_profile(bitmask: u32) -> ProfileVerdict {
    if bitmask & (PROFILE_PRIVATE | PROFILE_DOMAIN) != 0 {
        ProfileVerdict::Reachable
    } else if bitmask & PROFILE_PUBLIC != 0 {
        ProfileVerdict::PublicOnly
    } else {
        ProfileVerdict::Unknown
    }
}

/// Outcome of one diagnostic probe. `Failed` means the probe ran but the answer
/// is negative/errored (service down, COM query failed) — distinct from the
/// not-yet-run `Pending` state the UI shows before the first check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe<T> {
    Pending,
    Ok(T),
    Failed,
}

/// Shared diagnostics state: the UI thread reads a `Copy` snapshot each frame,
/// a worker thread writes the three probe results then requests a repaint.
#[derive(Debug, Clone, Copy)]
pub struct AccessHealth {
    /// A check is in flight — the UI shows a spinner and suppresses re-runs.
    pub running: bool,
    /// Loopback `GET /api/session` — `Ok(())` = server answered (service up).
    pub liveness: Probe<()>,
    /// Active network profile → reachability verdict.
    pub profile: Probe<ProfileVerdict>,
    /// Inbound firewall rule present — `Ok(false)` = absent (a valid read).
    pub rule: Probe<bool>,
}

impl Default for AccessHealth {
    fn default() -> Self {
        Self {
            running: false,
            liveness: Probe::Pending,
            profile: Probe::Pending,
            rule: Probe::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_only_warns() {
        assert_eq!(summarize_profile(PROFILE_PUBLIC), ProfileVerdict::PublicOnly);
    }

    #[test]
    fn private_is_reachable() {
        assert_eq!(summarize_profile(PROFILE_PRIVATE), ProfileVerdict::Reachable);
    }

    #[test]
    fn domain_is_reachable() {
        assert_eq!(summarize_profile(PROFILE_DOMAIN), ProfileVerdict::Reachable);
    }

    #[test]
    fn domain_and_private_reachable() {
        assert_eq!(
            summarize_profile(PROFILE_DOMAIN | PROFILE_PRIVATE),
            ProfileVerdict::Reachable
        );
    }

    #[test]
    fn mixed_public_and_private_is_reachable() {
        // Multi-NIC: Public + Private both active → reachable via Private.
        assert_eq!(
            summarize_profile(PROFILE_PUBLIC | PROFILE_PRIVATE),
            ProfileVerdict::Reachable
        );
    }

    #[test]
    fn zero_is_unknown() {
        assert_eq!(summarize_profile(0), ProfileVerdict::Unknown);
    }
}
