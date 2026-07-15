// Rollback — cleanup after a cancelled first-run wizard. Called from :
//   1. install::fresh_install() if the wizard subprocess returns non-zero
//   2. main() dispatch on `--rollback-from-failed-install`
//
// Every cleanup step is best-effort — the goal is to leave the machine
// in a clean state even if individual sub-steps fail (e.g. task didn't
// register, service half-created, etc.). Errors are logged and swallowed.
//
// The binary itself is never deleted — the user placed it, they own its
// disposition.

use std::path::Path;

use anyhow::Result;

#[cfg(windows)]
pub fn run() -> Result<()> {
    use crate::modes::install::{self, DIAGNOSTICS_CACHE_DIR, SERVICE_NAME};
    use crate::platform::win32::itaskservice;
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    tracing::info!("rollback: starting cleanup");

    // 1. Scheduled Task — best-effort.
    match itaskservice::delete_watchdog() {
        Ok(()) => tracing::info!("rollback: watchdog task removed"),
        Err(e) => tracing::warn!(error = %e, "rollback: delete watchdog task"),
    }

    // 2. Service — stop then delete, best-effort.
    if let Ok(scm) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        if let Ok(svc) = scm.open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        ) {
            let _ = svc.stop();
            match svc.delete() {
                Ok(()) => tracing::info!("rollback: service deleted"),
                Err(e) => tracing::warn!(error = %e, "rollback: delete service"),
            }
        } else {
            tracing::debug!("rollback: service not present, skipping");
        }
    }

    // 3. DiagnosticsCache — recursive delete, best-effort.
    remove_dir_all_logged(Path::new(DIAGNOSTICS_CACHE_DIR), "DiagnosticsCache");

    // 4. Data/ next to the current exe — best-effort. Wipes Animations,
    //    WebView2 cache, and Logs in one shot. The user never confirmed
    //    the install (wizard cancelled), so there are no user animations
    //    worth preserving at this stage.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let data = install::data_dir(parent);
            remove_dir_all_logged(&data, "Data");
        }
    }

    // 5. The binary itself is intentionally NOT deleted — the user placed
    //    it, they own its disposition.

    tracing::info!("rollback: done");
    Ok(())
}

#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!("rollback is Windows-only");
}

#[cfg(windows)]
fn remove_dir_all_logged(path: &Path, label: &str) {
    if !path.exists() {
        tracing::debug!(path = %path.display(), "rollback: {label} absent, nothing to remove");
        return;
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => tracing::info!(path = %path.display(), "rollback: {label} removed"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "rollback: remove {label}"),
    }
}
