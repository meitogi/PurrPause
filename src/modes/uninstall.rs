//! Password-gated uninstall flow — the real teardown behind `--uninstall`.
//!
//! Two entry paths funnel through [`run`] :
//!
//! - Config UI Sécurité tab → `Action::ShellUninstall` spawns the installed
//!   exe with `--uninstall`. The passcode was already verified in the tab,
//!   but we re-verify here as defense in depth — a direct
//!   `SystemHealthAgent.exe --uninstall` from a shell must not bypass the
//!   passcode.
//! - `Nettoyer.bat` double-clic → self-elevates, locates the installed exe,
//!   shells `--uninstall`. Same passcode gate applies.
//!
//! The teardown is 6 ordered steps :
//!
//! 1. Send `Message::Shutdown` over the named pipe so the service can drain
//!    (best-effort — service may already be dead).
//! 2. Poll SCM until the service reaches `Stopped` (short 5 s window).
//! 3. Delete the SCM entry with exponential backoff — mirrors
//!    [`crate::modes::install::retry_register_service`] because
//!    `SERVICE_MARKED_FOR_DELETE` contention applies to `svc.delete()`
//!    exactly like it does to `create_service()`.
//! 4. Delete the `\Microsoft\Windows\SystemHealth\HealthCheck` scheduled
//!    task via [`crate::platform::win32::itaskservice::delete_watchdog`].
//! 5. Wipe `C:\ProgramData\DiagnosticsCache\` (state.dat + logs + passcode).
//! 6. Schedule the running exe (and its `.manifest` sidecar) for
//!    delete-on-reboot via `MoveFileExW(MOVEFILE_DELAY_UNTIL_REBOOT)` —
//!    Windows can't unlink a running exe live.
//!
//! Any step past #3 is best-effort ; the SCM entry is the piece we MUST
//! delete for the service to stop coming back at boot.

use anyhow::Result;

/// Cross-platform entry called from `main::dispatch`. On non-Windows this
/// bails so the crate keeps compiling for host-side unit tests.
pub fn run() -> Result<()> {
    #[cfg(windows)]
    {
        win::run()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("uninstall is Windows-only")
    }
}

/// Pure test-friendly wrapper around [`crate::password::verify`]. Split
/// off from the config-loading gate so unit tests can exercise the
/// verification without needing a real state.dat on disk.
fn verify_passcode_against_hash(input: &str, hash: &str) -> Result<bool> {
    crate::password::verify(input, hash).map_err(Into::into)
}

/// Post-order walk of a directory tree : yields every child before its
/// containing directory, deepest first. The root itself is the last
/// entry. Extracted as a pure fn so the ordering can be unit-tested on
/// Linux ; the Windows `schedule_delete_tree` consumer relies on this
/// order because `MoveFileExW(MOVEFILE_DELAY_UNTIL_REBOOT)` refuses
/// non-empty directories at boot cleanup time.
///
/// Missing / unreadable paths yield an empty vector rather than an
/// error - callers are best-effort.
fn collect_tree_post_order(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    return out;

    fn walk(path: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.is_dir() {
                if let Ok(entries) = std::fs::read_dir(path) {
                    for entry in entries.flatten() {
                        walk(&entry.path(), out);
                    }
                }
            }
        } else {
            // Missing — nothing to schedule (root case).
            return;
        }
        out.push(path.to_path_buf());
    }
}

#[cfg(windows)]
mod win {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, Context, Result};
    use eframe::egui;
    use tracing::{info, warn};

    use crate::modes::install::{self, DIAGNOSTICS_CACHE_DIR, SERVICE_NAME, STATE_DAT};

    pub(super) fn run() -> Result<()> {
        // If state.dat is already gone, someone (Nettoyer.bat, a previous
        // --uninstall run) already cleaned up. Show a friendly note and
        // exit instead of prompting for a passcode that doesn't exist.
        if !Path::new(STATE_DAT).exists() {
            info!("state.dat missing - nothing to uninstall");
            show_info("Désinstallation", "Rien à désinstaller : la configuration a déjà été supprimée.");
            return Ok(());
        }

        let passcode = match prompt_passcode()? {
            Some(pw) => pw,
            None => {
                info!("uninstall: user cancelled at passcode prompt");
                return Ok(());
            }
        };

        if !passcode_gate(&passcode)? {
            warn!("uninstall: incorrect passcode");
            show_error("Désinstallation", "Passcode incorrect. Désinstallation annulée.");
            return Ok(());
        }

        info!("uninstall: passcode verified, starting teardown");
        teardown()?;
        show_info(
            "Désinstallation",
            "Désinstallation terminée. Redémarre le PC pour finaliser (l'exécutable sera supprimé au prochain démarrage).",
        );
        Ok(())
    }

    fn passcode_gate(input: &str) -> Result<bool> {
        let cfg = crate::config::load(Path::new(STATE_DAT))
            .context("load state.dat for passcode gate")?;
        super::verify_passcode_against_hash(input, &cfg.passcode_hash)
    }

    // ----- Passcode prompt ---------------------------------------------

    #[derive(Clone, Debug)]
    enum Outcome {
        Cancelled,
        Submitted(String),
    }

    fn prompt_passcode() -> Result<Option<String>> {
        let outcome: Arc<Mutex<Outcome>> = Arc::new(Mutex::new(Outcome::Cancelled));
        let outcome_clone = outcome.clone();

        let mut viewport = egui::ViewportBuilder::default()
            .with_inner_size([480.0, 220.0])
            .with_resizable(false);
        if let Some(icon) = crate::modes::config::decoration::load_cat_icon() {
            viewport = viewport.with_icon(icon);
        }
        let options = eframe::NativeOptions {
            viewport,
            ..Default::default()
        };

        eframe::run_native(
            "Désinstallation",
            options,
            Box::new(move |_cc| Ok(Box::new(UninstallPromptApp::new(outcome_clone)))),
        )
        .map_err(|e| anyhow!("eframe: {e}"))?;

        let outcome = outcome.lock().unwrap().clone();
        Ok(match outcome {
            Outcome::Cancelled => None,
            Outcome::Submitted(pw) => Some(pw),
        })
    }

    struct UninstallPromptApp {
        passcode: String,
        outcome: Arc<Mutex<Outcome>>,
        focus_grabbed: bool,
    }

    impl UninstallPromptApp {
        fn new(outcome: Arc<Mutex<Outcome>>) -> Self {
            Self {
                passcode: String::new(),
                outcome,
                focus_grabbed: false,
            }
        }
    }

    impl eframe::App for UninstallPromptApp {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            // CentralPanel wrap so panel_fill default paints the viewport
            // — same treatment as the Config UI tabs, keeps all 4 windows
            // visually consistent.
            egui::CentralPanel::default().show(ui, |ui| {
            egui::Frame::new().inner_margin(24).show(ui, |ui| {
                ui.heading("Désinstallation");
                ui.add_space(6.0);
                ui.label(
                    "Entre le passcode pour confirmer la suppression complète \
                     (service, tâche planifiée, configuration).",
                );
                ui.add_space(18.0);

                ui.horizontal(|ui| {
                    ui.label("Passcode :");
                    let resp = bordered_password_input(ui, &mut self.passcode);
                    if !self.focus_grabbed {
                        resp.request_focus();
                        self.focus_grabbed = true;
                    }
                });

                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    let can_confirm = !self.passcode.is_empty();
                    if ui
                        .add_enabled(can_confirm, egui::Button::new("Confirmer"))
                        .clicked()
                    {
                        *self.outcome.lock().unwrap() =
                            Outcome::Submitted(self.passcode.clone());
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.add_space(8.0);
                    if ui.button("Annuler").clicked() {
                        *self.outcome.lock().unwrap() = Outcome::Cancelled;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
            }); // CentralPanel
        }
    }

    /// Password `TextEdit` with a visible 1-px border. The border is
    /// applied to the widget itself via a scoped visuals override, so it
    /// hugs the field's desired_width instead of the full row width like
    /// the old outer-Frame version did.
    fn bordered_password_input(ui: &mut egui::Ui, input: &mut String) -> egui::Response {
        ui.scope(|ui| {
            let stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(140));
            ui.visuals_mut().widgets.inactive.bg_stroke = stroke;
            ui.visuals_mut().widgets.hovered.bg_stroke = stroke;
            ui.visuals_mut().widgets.active.bg_stroke = stroke;
            ui.add(
                egui::TextEdit::singleline(input)
                    .password(true)
                    .desired_width(200.0),
            )
        })
        .inner
    }

    // ----- Teardown ----------------------------------------------------

    fn teardown() -> Result<()> {
        // Step 0 — set the "Uninstalled" HKLM marker BEFORE any
        // destructive step. If teardown crashes mid-way (power off,
        // panic), the marker remains and the watchdog respects the
        // uninstall intent instead of resurrecting.
        if let Err(e) = crate::platform::registry::mark_uninstalled() {
            warn!(error = %e, "teardown[0]: mark_uninstalled failed - watchdog may resurrect");
        } else {
            info!("teardown[0]: Uninstalled marker set");
        }

        // Step 1 — send Shutdown so the service drains cleanly.
        match crate::ipc::send(crate::ipc::Message::Shutdown) {
            Ok(()) => info!("teardown[1]: sent IPC Shutdown"),
            Err(e) => info!(error = %e, "teardown[1]: IPC Shutdown skipped (service likely stopped)"),
        }

        // Step 2 — poll SCM until Stopped (5 s window).
        wait_for_service_stopped(Duration::from_secs(5));

        // Step 3 — delete the SCM entry with backoff.
        match delete_service_with_retry() {
            Ok(()) => info!("teardown[3]: SCM entry deleted"),
            Err(e) => warn!(error = %e, "teardown[3]: SCM delete failed - service may reappear at boot"),
        }

        // Step 4 — delete the scheduled task (idempotent helper).
        if let Err(e) = crate::platform::win32::itaskservice::delete_watchdog() {
            warn!(error = %e, "teardown[4]: scheduled task delete failed");
        } else {
            info!("teardown[4]: scheduled task deleted");
        }

        // Step 5 — wipe DiagnosticsCache (state.dat + logs + passcode).
        match std::fs::remove_dir_all(DIAGNOSTICS_CACHE_DIR) {
            Ok(()) => info!(path = DIAGNOSTICS_CACHE_DIR, "teardown[5]: DiagnosticsCache wiped"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("teardown[5]: DiagnosticsCache already absent");
            }
            Err(e) => warn!(error = %e, "teardown[5]: DiagnosticsCache removal failed"),
        }

        // Steps 5.5 + 6 both need current_exe — grab it once.
        let exe = match std::env::current_exe() {
            Ok(e) => Some(e),
            Err(e) => {
                warn!(error = %e, "teardown[5.5+6]: current_exe() failed, skipping side + self-delete");
                None
            }
        };

        // Step 5.5 — best-effort wipe of exe-dir runtime state we created.
        // animations/ is wiped NOW (no live handles). WebView2 cache too
        // (uninstall UI is pure egui/wgpu — no wry, so no WebView2
        // handles from THIS process ; workers from prior popup runs died
        // when the service stopped). Log files are scheduled for
        // delete-on-reboot instead of unlinked immediately because
        // tracing_appender holds a handle to today's file.
        // The .bat helpers (Activer-Desactiver.bat, Nettoyer.bat) are
        // shipped in the zip alongside the exe — leaving them for the
        // user to delete since they aren't strictly "ours" post-extract.
        if let Some(exe) = exe.as_ref() {
            wipe_exe_dir_side_files(exe);
        }

        // Step 6 — schedule the running exe + manifest for delete-on-reboot.
        if let Some(exe) = exe.as_ref() {
            if let Err(e) = schedule_self_delete(exe) {
                warn!(error = %e, exe = %exe.display(), "teardown[6]: self-delete schedule failed");
            }
            let manifest = with_extension_dot(exe, "manifest");
            if manifest.exists() {
                if let Err(e) = schedule_self_delete(&manifest) {
                    warn!(error = %e, path = %manifest.display(), "teardown[6]: manifest delete schedule failed");
                }
            }
        }

        Ok(())
    }

    fn wipe_exe_dir_side_files(exe: &Path) {
        let Some(dir) = exe.parent() else { return };

        // Everything runtime-generated lives under Data/ (Animations,
        // WebView2 cache, Logs). One recursive wipe replaces the old
        // three-branch enumeration. If WebView2 workers still hold
        // handles, remove_dir_all returns error 32 - schedule the tree
        // for delete-on-reboot as fallback.
        let data = install::data_dir(dir);
        if data.exists() {
            match std::fs::remove_dir_all(&data) {
                Ok(()) => info!(path = %data.display(), "teardown[5.5]: Data/ wiped"),
                Err(e) => {
                    warn!(
                        error = %e,
                        path = %data.display(),
                        "teardown[5.5]: Data/ live-wipe failed, scheduling reboot delete",
                    );
                    if let Err(e2) = schedule_delete_tree(&data) {
                        warn!(error = %e2, "teardown[5.5]: Data/ tree schedule failed");
                    }
                }
            }
        }
    }

    fn wait_for_service_stopped(timeout: Duration) {
        use windows_service::service::{ServiceAccess, ServiceState};
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let scm = match ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        ) {
            Ok(s) => s,
            Err(e) => {
                info!(error = %e, "teardown[2]: SCM unreachable, skipping stop-poll");
                return;
            }
        };
        let svc = match scm.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
        ) {
            Ok(s) => s,
            Err(_) => {
                info!("teardown[2]: service already absent, skipping stop-poll");
                return;
            }
        };

        // Best-effort stop kick — the service might have missed the IPC.
        let _ = svc.stop();

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match svc.query_status() {
                Ok(status) if status.current_state == ServiceState::Stopped => {
                    info!("teardown[2]: service reached Stopped");
                    return;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(200)),
                Err(_) => return, // service gone mid-poll — good enough
            }
        }
        warn!("teardown[2]: service did not reach Stopped within {:?}", timeout);
    }

    fn delete_service_with_retry() -> Result<()> {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        const MAX_ATTEMPTS: u32 = 6;
        const BACKOFF_MS: u64 = 500;

        let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("open SCM for delete")?;

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let svc = match scm.open_service(SERVICE_NAME, ServiceAccess::DELETE) {
                Ok(s) => s,
                Err(e) => {
                    // Already gone — win.
                    info!(error = %e, attempt, "delete: service absent, treating as success");
                    return Ok(());
                }
            };
            match svc.delete() {
                Ok(()) => {
                    if attempt > 1 {
                        info!(attempt, "delete: succeeded after retry");
                    }
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        attempt,
                        error = %e,
                        "delete: failed (likely marked-for-deletion contention) — retrying"
                    );
                    last_err = Some(anyhow!("{e}"));
                    drop(svc);
                    std::thread::sleep(Duration::from_millis(BACKOFF_MS * attempt as u64));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("delete_service_with_retry: exhausted attempts")))
    }

    fn schedule_self_delete(path: &Path) -> Result<()> {
        use std::iter::once;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT,
        };

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(once(0))
            .collect();
        // Second arg NULL = delete (rename to nothing) at next boot.
        unsafe {
            MoveFileExW(PCWSTR(wide.as_ptr()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT)
                .with_context(|| format!("MoveFileExW({})", path.display()))?;
        }
        info!(path = %path.display(), "scheduled delete-on-reboot");
        Ok(())
    }

    /// Recursively schedule every entry of `root` for delete-on-reboot,
    /// then schedule `root` itself. Children before their parent because
    /// Windows refuses to reboot-delete a non-empty directory. Used as
    /// fallback when a live `remove_dir_all` fails (e.g. WebView2
    /// workers still holding handles). Best-effort throughout - a
    /// failed schedule on one entry doesn't abort the sweep.
    pub(crate) fn schedule_delete_tree(root: &Path) -> Result<()> {
        let entries = super::collect_tree_post_order(root);
        let mut scheduled = 0usize;
        for path in &entries {
            if schedule_self_delete(path).is_ok() {
                scheduled += 1;
            }
        }
        info!(
            root = %root.display(),
            scheduled,
            total = entries.len(),
            "scheduled tree for delete-on-reboot",
        );
        Ok(())
    }

    // Helper because Path::with_extension doesn't handle "add sidecar
    // extension" for exe paths like we want ; e.g. `foo.exe.manifest`
    // sits alongside `foo.exe` — not `foo.manifest`.
    fn with_extension_dot(exe: &Path, ext: &str) -> std::path::PathBuf {
        let mut s = exe.as_os_str().to_owned();
        s.push(".");
        s.push(ext);
        std::path::PathBuf::from(s)
    }

    // encode_wide requires OsStrExt.
    use std::os::windows::ffi::OsStrExt;

    // ----- MessageBox wrappers -----------------------------------------

    fn show_info(title: &str, body: &str) {
        show_message(title, body, MessageIcon::Info);
    }

    fn show_error(title: &str, body: &str) {
        show_message(title, body, MessageIcon::Error);
    }

    enum MessageIcon {
        Info,
        Error,
    }

    fn show_message(title: &str, body: &str, icon: MessageIcon) {
        use std::iter::once;
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
        };
        let title_w: Vec<u16> = title.encode_utf16().chain(once(0)).collect();
        let body_w: Vec<u16> = body.encode_utf16().chain(once(0)).collect();
        let flags = MB_OK
            | match icon {
                MessageIcon::Info => MB_ICONINFORMATION,
                MessageIcon::Error => MB_ICONERROR,
            };
        unsafe {
            let _ = MessageBoxW(
                None,
                PCWSTR(body_w.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                flags,
            );
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passcode_gate_correct_returns_true() {
        let hash = crate::password::hash("123456").unwrap();
        assert!(verify_passcode_against_hash("123456", &hash).unwrap());
    }

    #[test]
    fn passcode_gate_incorrect_returns_false() {
        let hash = crate::password::hash("123456").unwrap();
        assert!(!verify_passcode_against_hash("999999", &hash).unwrap());
    }

    #[test]
    fn walk_order_files_first_then_dirs_bottom_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        std::fs::write(root.join("a").join("b").join("leaf.txt"), b"x").unwrap();
        std::fs::write(root.join("a").join("mid.txt"), b"y").unwrap();

        let order = collect_tree_post_order(root);
        let pos = |suffix: &str| -> usize {
            order
                .iter()
                .position(|p| p.strip_prefix(root).unwrap() == std::path::Path::new(suffix))
                .expect("entry not in walk")
        };
        // leaf before its dir, mid file before its dir, deep dir before shallow.
        assert!(pos("a/b/leaf.txt") < pos("a/b"));
        assert!(pos("a/mid.txt") < pos("a"));
        assert!(pos("a/b") < pos("a"));
        // Root itself is the very last entry.
        assert_eq!(order.last().unwrap(), root);
    }

    #[test]
    fn walk_empty_tree_yields_only_root() {
        let tmp = tempfile::tempdir().unwrap();
        let order = collect_tree_post_order(tmp.path());
        assert_eq!(order.len(), 1);
        assert_eq!(order[0], tmp.path());
    }

    #[test]
    fn walk_missing_root_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let ghost = tmp.path().join("does-not-exist");
        let order = collect_tree_post_order(&ghost);
        assert!(order.is_empty());
    }

    // -------- verify_passcode_against_hash — 3 additional coverage bumps --------

    #[test]
    fn passcode_gate_empty_input_rejected() {
        // Empty passcode never matches a non-empty-passcode hash. argon2
        // returns Ok(false), not an error — the gate handles the false path
        // by showing "Mot de passe incorrect", not by crashing.
        let hash = crate::password::hash("123456").unwrap();
        assert!(!verify_passcode_against_hash("", &hash).unwrap());
    }

    #[test]
    fn passcode_gate_unicode_passcode_roundtrips() {
        // Users may set non-ASCII passcodes ("é$", emoji, katakana). argon2
        // hashes the raw UTF-8 bytes ; verify must handle the same bytes.
        let passcode = "café🐈🍩キツネ";
        let hash = crate::password::hash(passcode).unwrap();
        assert!(verify_passcode_against_hash(passcode, &hash).unwrap());
        assert!(!verify_passcode_against_hash("café", &hash).unwrap());
    }

    #[test]
    fn passcode_gate_cross_install_hash_still_verifies() {
        // Cross-version compat : a hash string produced by argon2 0.5.x on
        // one machine must verify on another. Salt is embedded in the PHC
        // string so verify only needs the passcode + hash. This concrete
        // fixture (generated once via crate::password::hash("test-cross-install"))
        // pins the wire format — a future argon2 bump that changes it trips
        // the test.
        let stored_hash = "$argon2id$v=19$m=19456,t=2,p=1$SDig+t2BBcD7iSKZdSlMOg$P8ytb4Ick+FrXgze86oS69dB4ygUVkecy7fKoLko51Y";
        assert!(verify_passcode_against_hash("test-cross-install", stored_hash).unwrap());
        assert!(!verify_passcode_against_hash("wrong", stored_hash).unwrap());
    }
}
