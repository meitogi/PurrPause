//! Watchdog mode — health check invoked every minute by the Scheduled
//! Task. Three things it can do on each tick :
//!
//! 1. Nothing (service present + running).
//! 2. Kick the service (present + stopped).
//! 3. Reinstall the service (absent, but `state.dat` still there — user
//!    hasn't explicitly uninstalled, someone or something tampered with
//!    the SCM entry).
//!
//! Plus a fourth diagnostic : if the SCM `ImagePath` doesn't match the
//! current `env::current_exe()`, invoke the install-flow's `path_update`
//! to rewire everything to the new binary location — this handles the
//! "user moved the exe while the service was stopped" case that the
//! interactive install-flow only catches on the next double-clic.
//!
//! ## The state.dat + HKLM marker two-signal rule (0.9.0+)
//!
//! Two signals gate resurrection :
//! - `state.dat` at `C:\ProgramData\DiagnosticsCache\state.dat`.
//! - `Uninstalled` DWORD marker in HKLM (see `platform::registry`).
//!
//! Bail (respect uninstall intent) ONLY when state.dat is missing AND
//! the marker is set. State.dat missing without the marker is treated
//! as tampering (e.g. `del state.dat`) — fall through to SCM classify
//! so the service is resurrected with default config. The parent then
//! notices the passcode reset on their next Config UI visit and can
//! re-lock. Prior to 0.9.0 state.dat alone was the signal, making
//! `del state.dat` a one-command silent uninstall.

use anyhow::Result;

/// What the watchdog decided to do this tick. Extracted for unit tests.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WatchdogAction {
    /// Service present + running or start-pending — no work to do.
    Nop,
    /// Service present but not running — try to start it.
    Start,
    /// Service entry is gone from SCM — reinstall it.
    Reinstall,
}

/// Pure classifier. `None` means "SCM has no such service" — either the
/// entry was never created or it was deleted by tampering.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn classify<S: ServiceStateLike>(scm_status: Option<S>) -> WatchdogAction {
    match scm_status {
        Some(s) if s.is_running_or_starting() => WatchdogAction::Nop,
        Some(_) => WatchdogAction::Start,
        None => WatchdogAction::Reinstall,
    }
}

/// Trait so the classifier can be tested without pulling in
/// `windows-service` on non-Windows targets. The Windows impl wraps
/// `ServiceState` ; the test impl uses a lightweight enum.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) trait ServiceStateLike {
    fn is_running_or_starting(&self) -> bool;
}

#[cfg(windows)]
impl ServiceStateLike for windows_service::service::ServiceState {
    fn is_running_or_starting(&self) -> bool {
        matches!(
            self,
            windows_service::service::ServiceState::Running
                | windows_service::service::ServiceState::StartPending
        )
    }
}

#[cfg(windows)]
pub fn run() -> Result<()> {
    use std::path::Path;

    use anyhow::Context;
    use tracing::{info, warn};
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    use crate::modes::install::{
        self, paths_equal_ci, path_update, register_service, SERVICE_NAME, STATE_DAT,
    };
    use crate::platform::registry;

    // 1. Two-signal uninstall intent check (0.9.0+). Bail ONLY when
    //    state.dat missing AND HKLM Uninstalled marker set. Missing
    //    state.dat without the marker is treated as tampering ; fall
    //    through and let the SCM classifier resurrect with defaults.
    let state_dat_present = Path::new(STATE_DAT).exists();
    let marker_present = registry::is_uninstalled_marker_present();
    if registry::should_watchdog_bail(state_dat_present, marker_present) {
        info!(
            state_dat_present, marker_present,
            "watchdog tick - state.dat missing and Uninstalled marker set, respecting intent, bailing",
        );
        return Ok(());
    }
    if !state_dat_present {
        info!(
            marker_present,
            "watchdog tick - state.dat missing but no Uninstalled marker, treating as tampering, resurrecting",
        );
    }

    let current_exe = std::env::current_exe().context("env::current_exe()")?;

    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open SCM")?;

    // Query state via the standard service open. Image path is queried
    // through install::scm_image_path() which funnels through the
    // parse_exe_from_command_line helper - required so that comparison
    // against env::current_exe() doesn't false-fire PathUpdate on every
    // tick (0.6.2 bug ; see the "Second trap-walk" note in install.rs).
    let state = match scm.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::START,
    ) {
        Ok(svc) => svc.query_status().ok().map(|s| s.current_state),
        Err(e) => {
            info!(error = %e, "watchdog tick - service absent from SCM");
            None
        }
    };
    let image_path = install::scm_image_path();

    let action = classify::<ServiceState>(state);
    info!(?state, ?action, "watchdog tick");

    match action {
        WatchdogAction::Nop => {
            // 2. Path-drift heal only makes sense when the service is
            // fully present — if the ImagePath disagrees with our current
            // location, run path_update. We only trigger while the
            // service is running/starting so we're the only actor
            // touching the SCM entry (avoids racing the interactive
            // install-flow).
            if let Some(old) = image_path {
                if !paths_equal_ci(&current_exe, &old) {
                    info!(
                        current = %current_exe.display(),
                        registered = %old.display(),
                        "watchdog: exe moved, running path_update"
                    );
                    if let Err(e) = path_update(&current_exe, &old) {
                        warn!(error = %e, "watchdog: path_update failed");
                    }
                }
            }
            Ok(())
        }

        WatchdogAction::Start => {
            // Path drift is diagnosed here too — if the exe moved AND the
            // service is stopped, path_update will register the new path
            // and start it. Otherwise just start the existing entry.
            if let Some(old) = &image_path {
                if !paths_equal_ci(&current_exe, old) {
                    info!(
                        current = %current_exe.display(),
                        registered = %old.display(),
                        "watchdog: exe moved (service stopped), running path_update"
                    );
                    if let Err(e) = path_update(&current_exe, old) {
                        warn!(error = %e, "watchdog: path_update failed");
                    }
                    return Ok(());
                }
            }
            // Plain start.
            let svc = scm
                .open_service(SERVICE_NAME, ServiceAccess::START)
                .context("open service for start")?;
            let empty: [&str; 0] = [];
            if let Err(e) = svc.start(&empty) {
                warn!(error = %e, "watchdog: failed to start service");
            } else {
                info!("watchdog: service start kicked");
            }
            Ok(())
        }

        WatchdogAction::Reinstall => {
            info!(
                exe = %current_exe.display(),
                "watchdog: state.dat present but SCM entry gone, re-registering"
            );
            if let Err(e) = register_service(&current_exe) {
                warn!(error = %e, "watchdog: reinstall register_service failed");
                return Ok(());
            }
            // After re-registering, kick it. Best-effort — a subsequent
            // tick will Start if this one fails.
            let scm2 = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
                .context("re-open SCM after reinstall")?;
            if let Ok(svc) = scm2.open_service(SERVICE_NAME, ServiceAccess::START) {
                let empty: [&str; 0] = [];
                let _ = svc.start(&empty);
            }
            info!("watchdog: reinstall complete");
            let _ = install::DIAGNOSTICS_CACHE_DIR; // silence unused import on some configs
            Ok(())
        }
    }
}

#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!("watchdog is Windows-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum FakeState {
        Running,
        StartPending,
        Stopped,
        Paused,
        StopPending,
        PausePending,
        ContinuePending,
    }

    impl ServiceStateLike for FakeState {
        fn is_running_or_starting(&self) -> bool {
            matches!(self, FakeState::Running | FakeState::StartPending)
        }
    }

    #[test]
    fn classify_running_is_nop() {
        assert_eq!(classify(Some(FakeState::Running)), WatchdogAction::Nop);
    }

    #[test]
    fn classify_start_pending_is_nop() {
        assert_eq!(
            classify(Some(FakeState::StartPending)),
            WatchdogAction::Nop
        );
    }

    #[test]
    fn classify_stopped_is_start() {
        assert_eq!(classify(Some(FakeState::Stopped)), WatchdogAction::Start);
    }

    #[test]
    fn classify_paused_is_start() {
        // Any non-running present state means "kick it" — the service
        // shouldn't be paused in normal operation.
        assert_eq!(classify(Some(FakeState::Paused)), WatchdogAction::Start);
    }

    #[test]
    fn classify_absent_is_reinstall() {
        assert_eq!(
            classify::<FakeState>(None),
            WatchdogAction::Reinstall
        );
    }

    #[test]
    fn classify_ignores_launch_arguments() {
        // Regression : the watchdog's path-drift heal previously read
        // Service::query_config().executable_path raw. SCM stores it
        // as the full BINARY_PATH_NAME (exe + args), which the naive
        // comparison then flags as a mismatch against env::current_exe()
        // - firing PathUpdate every tick. This test locks the funnel
        // through parse_exe_from_command_line + install::classify so
        // future callers can't reintroduce the bug.
        use crate::modes::install::{classify, parse_exe_from_command_line, InstallAction};
        use std::path::PathBuf;
        let cmdline = r"C:\TT\Foo\SystemHealthAgent.exe --service";
        let parsed = parse_exe_from_command_line(cmdline);
        let current = PathBuf::from(r"C:\TT\Foo\SystemHealthAgent.exe");
        assert_eq!(classify(&current, Some(&parsed)), InstallAction::SamePathRelaunch);
    }

    // -------- 3 additional coverage bumps for transient SCM states --------

    #[test]
    fn classify_stop_pending_treated_as_needing_start() {
        // Service is in the middle of stopping. Watchdog kicks it back —
        // by the next tick either the stop completed (StartPending path
        // takes over) or the start hits an already-running state.
        assert_eq!(
            classify(Some(FakeState::StopPending)),
            WatchdogAction::Start
        );
    }

    #[test]
    fn classify_pause_pending_treated_as_needing_start() {
        // Pause-in-progress → not currently serving. Kick it.
        assert_eq!(
            classify(Some(FakeState::PausePending)),
            WatchdogAction::Start
        );
    }

    #[test]
    fn classify_continue_pending_treated_as_needing_start() {
        // Coming back from paused but not yet Running. Current impl treats
        // this as needing a start — SCM will no-op the redundant start on
        // a service already transitioning. Locks in that we don't add
        // ContinuePending to is_running_or_starting without a decision.
        assert_eq!(
            classify(Some(FakeState::ContinuePending)),
            WatchdogAction::Start
        );
    }
}
