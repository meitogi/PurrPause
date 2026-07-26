// Windows Firewall inbound rule for the remote LAN log console.
//
// Opens TCP 8787 for the service exe on the Private + Domain profiles only
// (never Public), so the child PC is reachable at http://<host>.local:8787/
// on a home / LAN network but not on public networks. Program-scoped to the
// exe, so the rule admits only our own binary. Created at install, removed at
// uninstall. Uses INetFwPolicy2 COM, mirroring the project's Task Scheduler
// path (and reusing `with_com` for the paired CoInitialize/CoUninitialize).

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::BSTR;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule, NetFwPolicy2, NetFwRule, NET_FW_ACTION_ALLOW,
    NET_FW_IP_PROTOCOL_TCP, NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_RULE_DIR_IN,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use super::itaskservice::with_com;
use crate::remote::REMOTE_LOG_PORT;

/// Firewall rule display name — also the key we remove by at uninstall.
const RULE_NAME: &str = "Purrpause — Console de logs (LAN)";

/// Create (or replace) the inbound Allow rule for the log console.
pub fn add_rule(exe: &Path) -> Result<()> {
    let exe = exe.to_string_lossy().into_owned();
    let port = REMOTE_LOG_PORT.to_string();
    with_com(|| unsafe {
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
            .context("CoCreateInstance(NetFwPolicy2)")?;
        let rules = policy.Rules().context("INetFwPolicy2::Rules")?;

        // Idempotent: drop any stale rule with the same name first (a repair
        // install re-runs fresh_install).
        let _ = rules.Remove(&BSTR::from(RULE_NAME));

        let rule: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)
            .context("CoCreateInstance(NetFwRule)")?;
        rule.SetName(&BSTR::from(RULE_NAME)).context("SetName")?;
        rule.SetDescription(&BSTR::from(
            "Autorise la consultation des journaux purrpause sur le réseau local.",
        ))
        .context("SetDescription")?;
        rule.SetApplicationName(&BSTR::from(exe.as_str()))
            .context("SetApplicationName")?;
        rule.SetProtocol(NET_FW_IP_PROTOCOL_TCP.0)
            .context("SetProtocol")?;
        rule.SetLocalPorts(&BSTR::from(port.as_str()))
            .context("SetLocalPorts")?;
        rule.SetDirection(NET_FW_RULE_DIR_IN)
            .context("SetDirection")?;
        rule.SetAction(NET_FW_ACTION_ALLOW).context("SetAction")?;
        // Private + Domain only — never Public.
        rule.SetProfiles(NET_FW_PROFILE2_PRIVATE.0 | NET_FW_PROFILE2_DOMAIN.0)
            .context("SetProfiles")?;
        rule.SetEnabled(VARIANT_TRUE).context("SetEnabled")?;

        rules.Add(&rule).context("INetFwRules::Add")?;
        tracing::info!(port = REMOTE_LOG_PORT, "firewall rule added");
        Ok(())
    })
}

/// Whether the console's inbound rule currently exists, by name.
///
/// `Ok(true)` / `Ok(false)` answer the question; an `Err` means the query
/// itself could not run (e.g. the Windows Firewall service is stopped), which
/// is deliberately NOT collapsed into `Ok(false)` — "no rule" and "can't tell"
/// are different states for the caller (install verification, session-7 signal).
pub fn rule_exists() -> Result<bool> {
    with_com(|| unsafe {
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
            .context("CoCreateInstance(NetFwPolicy2)")?;
        let rules = policy.Rules().context("INetFwPolicy2::Rules")?;
        // Item() returns the rule by name, or an error HRESULT when absent.
        // A lookup miss is `Ok(false)`; failure to obtain policy/rules above
        // has already propagated as `Err`.
        Ok(rules.Item(&BSTR::from(RULE_NAME)).is_ok())
    })
}

/// Active network-profile bitmask (`NET_FW_PROFILE2_*`, OR-combined) for the
/// current connections. Display-only — it reports which profile is active so
/// the Sécurité tab can tell whether the console's Private+Domain rule applies;
/// it never mutates the firewall or switches the profile.
pub fn current_profile_types() -> Result<u32> {
    with_com(|| unsafe {
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
            .context("CoCreateInstance(NetFwPolicy2)")?;
        let types = policy
            .CurrentProfileTypes()
            .context("INetFwPolicy2::CurrentProfileTypes")?;
        Ok(types as u32)
    })
}

/// Remove the console's firewall rule. A missing rule is not an error.
pub fn remove_rule() -> Result<()> {
    with_com(|| unsafe {
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
            .context("CoCreateInstance(NetFwPolicy2)")?;
        let rules = policy.Rules().context("INetFwPolicy2::Rules")?;
        match rules.Remove(&BSTR::from(RULE_NAME)) {
            Ok(()) => tracing::info!("firewall rule removed"),
            Err(e) => tracing::debug!(error = %e, "firewall rule remove (already absent?)"),
        }
        Ok(())
    })
}
