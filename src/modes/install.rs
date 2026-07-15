//! First-run install flow and path-update contract.
//!
//! `run()` is invoked from `main()` when the binary is launched with no
//! argv. It queries the SCM for the `WindowsSystemHealth` service and
//! dispatches on one of three [`InstallAction`]s returned by [`classify`] :
//!
//! - [`InstallAction::FreshInstall`] — no prior service. Runs the full
//!   10-step install (animations dir, config dir with restrictive DACL,
//!   scheduled task, DPAPI-encrypted state.dat, service registration,
//!   service start, config UI launch).
//! - [`InstallAction::SamePathRelaunch`] — service already registered
//!   pointing at THIS exe. Just opens the config UI.
//! - [`InstallAction::PathUpdate`] — service points at a different path
//!   (user moved the binary between launches). Rewires the SCM ImagePath
//!   and the scheduled task action to the new location, then restarts.
//!
//! # Path-update contract
//!
//! [`path_update`] rewires the SCM entry via a direct
//! [`ChangeServiceConfigW`] call (see
//! [`crate::platform::win32::service_config`]). One atomic mutation,
//! no `SERVICE_MARKED_FOR_DELETE` race, no retry backoff. Scheduled
//! task action still updated separately via
//! [`crate::platform::win32::itaskservice::update_watchdog_action`]
//! (`RegisterTask` with `TASK_CREATE_OR_UPDATE` is idempotent).
//!
//! # Called from
//!
//! [`register_service`] and [`path_update`] are `pub(crate)` so the
//! watchdog mode can resurrect a fully-deleted SCM entry
//! ([`crate::modes::watchdog`] triggers `register_service` when state.dat
//! still exists but the SCM entry is gone) and heal a moved exe while the
//! service is stopped (triggers `path_update` when `env::current_exe()`
//! disagrees with the SCM ImagePath).
//!
//! [`ChangeServiceConfigW`]: https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-changeserviceconfigw

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const SERVICE_NAME: &str = "WindowsSystemHealth";
pub const SERVICE_DISPLAY_NAME: &str = "Windows Session Health Service";
pub const SERVICE_DESCRIPTION: &str =
    "Monitors user session health metrics for ergonomic notifications.";
pub const DIAGNOSTICS_CACHE_DIR: &str = r"C:\ProgramData\DiagnosticsCache";
pub const STATE_DAT: &str = r"C:\ProgramData\DiagnosticsCache\state.dat";

// Exe-adjacent Data/ layout : all runtime artefacts live under one
// folder so the install directory stays visually clean. Kept as
// PathBuf-returning helpers (not string constants) because callers
// need to join with the current exe's parent, which varies.
pub const DATA_DIR: &str = "Data";
pub const DATA_ANIMATIONS: &str = "Animations";
pub const DATA_WEBVIEW2: &str = "WebView2";
pub const DATA_LOGS: &str = "Logs";

pub fn data_dir(exe_parent: &Path) -> PathBuf {
    exe_parent.join(DATA_DIR)
}
pub fn animations_dir(exe_parent: &Path) -> PathBuf {
    data_dir(exe_parent).join(DATA_ANIMATIONS)
}
pub fn webview2_dir(exe_parent: &Path) -> PathBuf {
    data_dir(exe_parent).join(DATA_WEBVIEW2)
}
pub fn logs_dir(exe_parent: &Path) -> PathBuf {
    data_dir(exe_parent).join(DATA_LOGS)
}

/// Legacy rolling-log file predicate. Extracted so both the migration
/// step (wipes legacy `<exe_dir>/install.log.*` + `widget.log.*` after
/// the tracing layer is retargeted to `Data/Logs/`) and any future
/// hygiene sweep can share the classification. The trailing dot + date
/// suffix is required to avoid nuking a bare `install.log` if a user
/// created one manually.
pub fn is_legacy_rolling_log(name: &str) -> bool {
    (name.starts_with("install.log.") || name.starts_with("widget.log."))
        && name.len() > "install.log.".len()
}

#[derive(Debug, PartialEq, Eq)]
pub enum InstallAction {
    FreshInstall,
    SamePathRelaunch,
    PathUpdate { old_path: PathBuf },
}

/// Pure classifier — decides which action to take given the current
/// binary path and the ImagePath registered in SCM (or `None` if the
/// service doesn't exist). Case-insensitive comparison, matching
/// Windows filesystem semantics.
pub fn classify(current: &Path, existing_image_path: Option<&Path>) -> InstallAction {
    match existing_image_path {
        None => InstallAction::FreshInstall,
        Some(existing) => {
            if paths_equal_ci(current, existing) {
                InstallAction::SamePathRelaunch
            } else {
                InstallAction::PathUpdate {
                    old_path: existing.to_path_buf(),
                }
            }
        }
    }
}

pub(crate) fn paths_equal_ci(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Extract the executable path from a Windows service `BINARY_PATH_NAME`
/// (a.k.a. command line). SCM stores the exe path and launch arguments
/// as one string ; `query_config().executable_path` returns the whole
/// thing including args. Comparing that to `env::current_exe()` (which
/// is bare) always shows a mismatch and falsely triggers `PathUpdate`.
///
/// Handles both conventions per Windows service registration :
/// - Unquoted : split on the first ASCII whitespace.
/// - Quoted : take everything between the first and second `"`.
///
/// If input is malformed (unmatched quote, empty), returns the input
/// verbatim — `classify()` will then decide what to do with it.
pub(crate) fn parse_exe_from_command_line(cmdline: &str) -> PathBuf {
    let s = cmdline.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        return PathBuf::from(
            rest.split_once('"').map(|(exe, _)| exe).unwrap_or(rest),
        );
    }
    PathBuf::from(
        s.split_once(|c: char| c.is_ascii_whitespace())
            .map(|(exe, _)| exe)
            .unwrap_or(s),
    )
}

#[cfg(windows)]
pub fn run() -> Result<()> {
    use std::env;

    let current_exe = env::current_exe().context("env::current_exe()")?;
    tracing::info!(path = %current_exe.display(), "install flow starting");

    let existing = scm_image_path();
    let action = classify(&current_exe, existing.as_deref());
    tracing::info!(?action, "install action decided");

    match action {
        InstallAction::FreshInstall => fresh_install(&current_exe),
        InstallAction::SamePathRelaunch => launch_config_ui(&current_exe),
        InstallAction::PathUpdate { old_path } => path_update(&current_exe, &old_path),
    }
}

#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!("install flow is Windows-only");
}

/// The **single** entry point for reading the SCM's stored image path.
///
/// # Second trap-walk : BINARY_PATH_NAME parsing
///
/// Any code that reads `Service::query_config().executable_path` MUST
/// funnel through [`parse_exe_from_command_line`] before comparing to
/// `env::current_exe()`. SCM stores the full command line (exe + args)
/// there - a naive comparison misfires every relaunch as a spurious
/// [`InstallAction::PathUpdate`]. The watchdog was bitten by this in
/// 0.6.2 (bug #7 in the 0.6.3 smoke) after the install-flow was fixed
/// but the watchdog's independent query wasn't. Using this helper -
/// rather than a raw `svc.query_config()` - makes the mistake
/// unrepresentable at future call sites.
#[cfg(windows)]
pub(crate) fn scm_image_path() -> Option<PathBuf> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let svc = scm.open_service(SERVICE_NAME, ServiceAccess::QUERY_CONFIG).ok()?;
    let cfg = svc.query_config().ok()?;
    // executable_path is the full BINARY_PATH_NAME (exe + args as one
    // string). Strip the args so classify() sees just the exe path.
    Some(parse_exe_from_command_line(
        &cfg.executable_path.to_string_lossy(),
    ))
}

#[cfg(windows)]
fn fresh_install(current_exe: &Path) -> Result<()> {
    use crate::platform::win32::{acl, hidden_attrs, itaskservice};

    // Step 0 — clear the "Uninstalled" HKLM marker unconditionally. A
    // fresh install rewrites the user's intent from "uninstalled" (if
    // the marker was set by a prior --uninstall) to "installed and
    // active" ; leaving the marker would confuse the watchdog if
    // state.dat were later wiped.
    if let Err(e) = crate::platform::registry::clear_uninstalled_marker() {
        tracing::warn!(error = %e, "step 0: clear_uninstalled_marker failed");
    }

    // Handle the "broken install" recovery path : files from a
    // previous 0.6.x install may still be on disk even though the SCM
    // entry is gone (e.g. previous path_update failed halfway). Move
    // legacy artefacts into Data/ so step 4's scaffolding is a no-op.
    let exe_parent = current_exe
        .parent()
        .context("current_exe has no parent")?;
    let exe_file_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("SystemHealthAgent.exe");
    let _ = migrate_layout_to_data(exe_parent, exe_file_name);

    // Step 4 : Data/ scaffolding next to the exe. Three subdirs (Animations,
    // WebView2, Logs) grouped under one folder so the install directory
    // stays visually clean. All writable, no restrictive DACL — the exe
    // itself lives in a user-writable location.
    let anim = animations_dir(exe_parent);
    let webview2 = webview2_dir(exe_parent);
    let logs = logs_dir(exe_parent);
    std::fs::create_dir_all(&anim)
        .with_context(|| format!("mkdir {}", anim.display()))?;
    std::fs::create_dir_all(&webview2)
        .with_context(|| format!("mkdir {}", webview2.display()))?;
    std::fs::create_dir_all(&logs)
        .with_context(|| format!("mkdir {}", logs.display()))?;
    tracing::info!(root = %data_dir(exe_parent).display(), "step 4: Data/ scaffolding ready");

    // Step 5 : DiagnosticsCache with DACL + hidden+system.
    let cache_dir = Path::new(DIAGNOSTICS_CACHE_DIR);
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("mkdir {}", cache_dir.display()))?;
    acl::apply_diagnostics_cache_dacl(cache_dir).context("step 5: apply DACL")?;
    hidden_attrs::set_hidden_system(cache_dir).context("step 5: hidden+system attrs")?;
    tracing::info!(path = %cache_dir.display(), "step 5: DiagnosticsCache ready");

    // Step 5.5 : defuse any PendingFileRenameOperations entries left by
    // a previous --uninstall. If the user reinstalls to the same folder
    // before rebooting, the OS would silently delete the new exe at
    // next boot. Best-effort ; never abort install for this.
    let manifest_path = current_exe.with_extension("exe.manifest");
    let paths_to_defuse: [&Path; 2] = [current_exe, &manifest_path];
    if let Err(e) = crate::platform::registry::defuse_pending_rename_for(&paths_to_defuse) {
        tracing::warn!(error = %e, "step 5.5: defuse pending renames");
    } else {
        tracing::debug!("step 5.5: pending renames defused");
    }

    // Step 6 : default state.dat - but ONLY if none exists. A prior
    // failed path_update (SCM in "marked for deletion" state) triggers
    // a re-run of fresh_install ; overwriting the existing state.dat
    // here would wipe the parent's passcode + all their tuning.
    let state_path = Path::new(STATE_DAT);
    let state_existed = state_path.exists();
    if !state_existed {
        let default_cfg = default_config_stamped(std::time::SystemTime::now());
        crate::config::save(&default_cfg, state_path)
            .context("step 6: default state.dat")?;
        tracing::info!("step 6: default state.dat written (fresh)");
    } else {
        tracing::info!("step 6: state.dat already exists, preserving passcode + config");
    }

    // Step 7 : Windows service.
    register_service(current_exe).context("step 7: register service")?;
    tracing::info!("step 7: service registered");

    // Step 8 : Scheduled Task watchdog.
    itaskservice::register_watchdog(current_exe).context("step 8: register watchdog")?;
    tracing::info!("step 8: watchdog task registered");

    // Step 9 : block on first-run wizard — SKIP if state.dat already
    // has a passcode. This handles the "PathUpdate failed → FreshInstall
    // fallback" recovery path : the parent already went through the
    // wizard on a previous version, no need to ask again.
    let wizard_needed = match crate::config::load(state_path) {
        Ok(cfg) => cfg.passcode_hash.is_empty(),
        Err(_) => true,
    };
    if wizard_needed {
        let status = std::process::Command::new(current_exe)
            .arg("--config")
            .arg("--first-run")
            .status()
            .context("step 9: spawn wizard")?;
        if !status.success() {
            tracing::warn!(?status, "step 9: wizard cancelled - rolling back");
            crate::modes::rollback::run().ok();
            anyhow::bail!("first-run wizard cancelled");
        }
        tracing::info!("step 9: first-run wizard completed");
    } else {
        tracing::info!("step 9: skipping wizard, passcode already set (recovery path)");
    }

    // Step 10 : start the service.
    start_service().context("step 10: start service")?;
    tracing::info!("step 10: service started");

    // Bonus : open the config UI right after the wizard succeeded, so
    // the parent gets a visible "you're set" moment instead of a
    // silent exit — the user typically wants to customize intervals /
    // add animations right after first install.
    launch_config_ui(current_exe).context("open config UI after fresh install")?;

    Ok(())
}

#[cfg(windows)]
pub(crate) fn register_service(current_exe: &Path) -> Result<()> {
    use std::ffi::OsString;
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let scm = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("open SCM")?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: current_exe.to_path_buf(),
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let svc = scm
        .create_service(&info, ServiceAccess::CHANGE_CONFIG)
        .context("create_service")?;
    svc.set_description(SERVICE_DESCRIPTION)
        .context("set_description")?;
    Ok(())
}

#[cfg(windows)]
fn start_service() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open SCM")?;
    let svc = scm
        .open_service(SERVICE_NAME, ServiceAccess::START)
        .context("open service")?;
    svc.start(&[] as &[&str]).context("start service")?;
    Ok(())
}

#[cfg(windows)]
fn launch_config_ui(current_exe: &Path) -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    tracing::info!(exe = %current_exe.display(), "launching config UI child (fire-and-forget)");
    // Permit ANY subsequently-spawned child process to take foreground.
    // Windows' anti-focus-stealing normally blocks a background parent
    // from handing focus to a fresh child ; ASFW_ANY relaxes that scope.
    // Combined with the child's ViewportBuilder::with_active(true), the
    // Config UI opens focused instead of behind whatever was on screen.
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    // Fire-and-forget : the child owns the window loop, the parent
    // (this install-flow process) exits immediately. Using .status()
    // here would block until the child dies and leave a phantom parent
    // process in Task Manager.
    std::process::Command::new(current_exe)
        .arg("--config")
        .spawn()
        .context("spawn config UI child")?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn path_update(current_exe: &Path, old_path: &Path) -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    tracing::info!(
        old = %old_path.display(),
        new = %current_exe.display(),
        "user moved the binary - updating install paths",
    );

    // Migrate the NEW exe-dir's legacy 0.6.x artefacts into Data/ before
    // rewiring SCM. If the user moved a 0.6.x install to a folder that
    // already has a Data/ layout, the idempotency guard skips ; else
    // scaffolds fresh. Move step below (`migrate_animations_folder`)
    // then picks up any .lottie files that lived at the OLD exe-dir.
    if let Some(new_parent) = current_exe.parent() {
        let exe_file_name = current_exe
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("SystemHealthAgent.exe");
        let _ = migrate_layout_to_data(new_parent, exe_file_name);
    }

    // Defuse any leftover PendingFileRenameOperations that point at the
    // new exe path (e.g. previous uninstall scheduled a delete-on-reboot
    // then user reinstalled to the same folder before rebooting). Same
    // rationale as fresh_install step 5.5 - runs first so the rest of
    // the flow can't be silently zapped at next boot.
    let manifest_path = current_exe.with_extension("exe.manifest");
    let paths_to_defuse: [&Path; 2] = [current_exe, &manifest_path];
    if let Err(e) = crate::platform::registry::defuse_pending_rename_for(&paths_to_defuse) {
        tracing::warn!(error = %e, "path_update: defuse pending renames");
    }

    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open SCM")?;
    let svc = scm
        .open_service(
            SERVICE_NAME,
            ServiceAccess::CHANGE_CONFIG
                | ServiceAccess::STOP
                | ServiceAccess::START
                | ServiceAccess::QUERY_STATUS,
        )
        .context("open service for update")?;

    // Stop before mutating ImagePath. SCM accepts the change on a
    // running service but the swap only takes effect after a
    // stop/start cycle - stopping now lets ChangeServiceConfigW +
    // start below produce the new binary in one shot without an
    // intermediate window pointing at the old path.
    let _ = svc.stop();
    for _ in 0..20 {
        match svc.query_status() {
            Ok(status)
                if status.current_state == windows_service::service::ServiceState::Stopped =>
            {
                break;
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(250)),
        }
    }

    // Direct ChangeServiceConfigW - no delete + recreate, no
    // SERVICE_MARKED_FOR_DELETE race, no backoff loop. See
    // platform::win32::service_config for the FFI shape.
    crate::platform::win32::service_config::change_service_binary_path(&svc, current_exe)
        .context("update SCM ImagePath in place")?;
    drop(svc);

    // Same treatment for the scheduled task.
    crate::platform::win32::itaskservice::update_watchdog_action(current_exe)
        .context("update watchdog action")?;

    // Copy animations/ from the old dir if the new one doesn't have any.
    migrate_animations_folder(old_path, current_exe).ok();

    // Restart the service.
    start_service().context("start service after path update")?;

    // Open the config UI so the path-swap gives visual confirmation to
    // the user instead of a silent parent exit. Same fire-and-forget
    // shape as SamePathRelaunch.
    launch_config_ui(current_exe).context("open config UI after path update")?;
    Ok(())
}

/// Return a default `Config` with `first_install_at_epoch_secs`
/// populated from `now`. Pure : no I/O, no globals - so unit tests can
/// exercise the stamp logic without needing a real state.dat.
pub fn default_config_stamped(now: std::time::SystemTime) -> crate::config::Config {
    let mut cfg = crate::config::Config::default();
    cfg.first_install_at_epoch_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    cfg
}

/// One-time migration of a 0.6.x install to the 0.7.0 `Data/` layout.
///
/// Idempotent : if all three `Data/` subdirs already exist, returns
/// immediately. Otherwise scaffolds them, then :
/// - moves `<exe_parent>/resources/animations/*.lottie` into
///   `<exe_parent>/Data/Animations/` (preserves user drag-drop files),
/// - moves `<exe_parent>/animations/*.lottie` (orphan folder from
///   pre-0.7.0 install step 4) into `Data/Animations/`,
/// - wipes `<exe_parent>/<exe_file_name>.WebView2/` (WebView2 cache
///   regenerates ; on lock failure logs warn and moves on — one-time
///   migration, user reboots eventually),
/// - deletes legacy rolling logs (`install.log.*`, `widget.log.*`) at
///   the exe-dir root ; unrelated files survive.
///
/// Every step is best-effort. Failures log but never abort the caller
/// — a partial migration is preferable to a bricked install.
pub fn migrate_layout_to_data(exe_parent: &Path, exe_file_name: &str) -> Result<()> {
    let anim = animations_dir(exe_parent);
    let webview2 = webview2_dir(exe_parent);
    let logs = logs_dir(exe_parent);

    // Idempotency guard : all three subdirs exist ⇒ nothing to do.
    if anim.exists() && webview2.exists() && logs.exists() {
        tracing::debug!(
            root = %data_dir(exe_parent).display(),
            "migrate_layout_to_data: Data/ already scaffolded, skipping",
        );
        return Ok(());
    }

    // Scaffold. create_dir_all is a no-op on already-present dirs.
    let _ = std::fs::create_dir_all(&anim);
    let _ = std::fs::create_dir_all(&webview2);
    let _ = std::fs::create_dir_all(&logs);

    let mut moved = 0usize;
    // Move .lottie files from <exe>/resources/animations/ into Data/.
    let legacy_resources_anim = exe_parent.join("resources").join("animations");
    if legacy_resources_anim.is_dir() {
        moved += move_lottie_files(&legacy_resources_anim, &anim);
        // Best-effort remove_dir (only succeeds if empty).
        let _ = std::fs::remove_dir(&legacy_resources_anim);
    }

    // Move .lottie files from the orphan <exe>/animations/ folder
    // that pre-0.7.0 fresh_install step 4 used to create.
    let orphan_anim = exe_parent.join("animations");
    if orphan_anim.is_dir() {
        moved += move_lottie_files(&orphan_anim, &anim);
        let _ = std::fs::remove_dir(&orphan_anim);
    }
    if moved > 0 {
        tracing::info!(count = moved, dest = %anim.display(), "migrate_layout_to_data: .lottie files moved");
    }

    // Wipe legacy WebView2 cache directory. It regenerates on next
    // popup. If msedgewebview2.exe workers hold handles, remove_dir_all
    // returns error 32 (ERROR_SHARING_VIOLATION) — user-locked decision
    // is log-and-move-on (one-time migration, next reboot cleans it).
    let legacy_webview2 = exe_parent.join(format!("{exe_file_name}.WebView2"));
    if legacy_webview2.exists() {
        match std::fs::remove_dir_all(&legacy_webview2) {
            Ok(()) => tracing::info!(path = %legacy_webview2.display(), "migrate_layout_to_data: legacy WebView2 cache wiped"),
            Err(e) => tracing::warn!(
                error = %e,
                path = %legacy_webview2.display(),
                "migrate_layout_to_data: legacy WebView2 cache locked, reboot to clean up",
            ),
        }
    }

    // Delete legacy rolling logs at exe-dir root. Unrelated files
    // (readme.txt, user notes, etc.) survive.
    let mut wiped_logs = 0usize;
    if let Ok(entries) = std::fs::read_dir(exe_parent) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if is_legacy_rolling_log(&name_str) {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => wiped_logs += 1,
                    Err(e) => tracing::warn!(
                        error = %e,
                        path = %entry.path().display(),
                        "migrate_layout_to_data: legacy log delete failed",
                    ),
                }
            }
        }
    }
    if wiped_logs > 0 {
        tracing::info!(count = wiped_logs, "migrate_layout_to_data: legacy rolling logs wiped");
    }

    tracing::info!(
        root = %data_dir(exe_parent).display(),
        "migrate_layout_to_data: done",
    );
    Ok(())
}

/// Move every `.lottie` file from `src` into `dst`. Same-volume rename
/// first ; falls back to copy + remove_file on EXDEV. Returns count of
/// files successfully moved.
fn move_lottie_files(src: &Path, dst: &Path) -> usize {
    let mut moved = 0usize;
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_lottie = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lottie"))
            .unwrap_or(false);
        if !is_lottie {
            continue;
        }
        let file_name = match path.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let target = dst.join(&file_name);
        if target.exists() {
            // Preserve user's existing file ; skip the copy from the
            // legacy location. Safe because Data/Animations/ is the
            // canonical destination going forward.
            continue;
        }
        match std::fs::rename(&path, &target) {
            Ok(()) => moved += 1,
            Err(_) => {
                // Cross-volume or shell handle : fall back to copy+delete.
                if std::fs::copy(&path, &target).is_ok() {
                    let _ = std::fs::remove_file(&path);
                    moved += 1;
                }
            }
        }
    }
    moved
}

#[cfg(windows)]
fn migrate_animations_folder(old_exe: &Path, new_exe: &Path) -> Result<()> {
    let old_dir = animations_dir(old_exe.parent().context("old_exe parent")?);
    let new_dir = animations_dir(new_exe.parent().context("new_exe parent")?);
    if !old_dir.exists() {
        return Ok(());
    }
    // Only copy if the new one has no user-added anims (skip if present
    // to avoid clobbering user work).
    if new_dir.exists() {
        let has_files = std::fs::read_dir(&new_dir)
            .map(|d| d.filter_map(|e| e.ok()).any(|_| true))
            .unwrap_or(false);
        if has_files {
            return Ok(());
        }
    }
    std::fs::create_dir_all(&new_dir).ok();
    for entry in std::fs::read_dir(&old_dir)? {
        let entry = entry?;
        let dst = new_dir.join(entry.file_name());
        std::fs::copy(entry.path(), dst).ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn no_existing_service_yields_fresh_install() {
        let current = PathBuf::from(r"C:\Users\alice\Desktop\SystemHealthAgent.exe");
        assert_eq!(classify(&current, None), InstallAction::FreshInstall);
    }

    #[test]
    fn same_path_yields_same_path_relaunch() {
        let current = PathBuf::from(r"C:\Users\alice\Desktop\SystemHealthAgent.exe");
        assert_eq!(
            classify(&current, Some(&current)),
            InstallAction::SamePathRelaunch
        );
    }

    #[test]
    fn same_path_case_insensitive() {
        let current = PathBuf::from(r"C:\Users\alice\Desktop\SystemHealthAgent.exe");
        let existing = PathBuf::from(r"c:\users\ALICE\desktop\systemhealthagent.exe");
        assert_eq!(
            classify(&current, Some(&existing)),
            InstallAction::SamePathRelaunch
        );
    }

    #[test]
    fn different_path_yields_path_update() {
        let current = PathBuf::from(r"D:\Tools\SystemHealthAgent.exe");
        let existing = PathBuf::from(r"C:\Users\alice\Desktop\SystemHealthAgent.exe");
        match classify(&current, Some(&existing)) {
            InstallAction::PathUpdate { old_path } => assert_eq!(old_path, existing),
            other => panic!("expected PathUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_command_line_unquoted_with_args() {
        assert_eq!(
            parse_exe_from_command_line(r"C:\TT\Foo\SystemHealthAgent.exe --service"),
            PathBuf::from(r"C:\TT\Foo\SystemHealthAgent.exe"),
        );
    }

    #[test]
    fn parse_command_line_unquoted_no_args() {
        assert_eq!(
            parse_exe_from_command_line(r"C:\TT\Foo\SystemHealthAgent.exe"),
            PathBuf::from(r"C:\TT\Foo\SystemHealthAgent.exe"),
        );
    }

    #[test]
    fn parse_command_line_quoted_with_spaces_and_args() {
        assert_eq!(
            parse_exe_from_command_line(r#""C:\Program Files\Foo\SystemHealthAgent.exe" --service"#),
            PathBuf::from(r"C:\Program Files\Foo\SystemHealthAgent.exe"),
        );
    }

    #[test]
    fn parse_command_line_quoted_no_args() {
        assert_eq!(
            parse_exe_from_command_line(r#""C:\Program Files\Foo\SystemHealthAgent.exe""#),
            PathBuf::from(r"C:\Program Files\Foo\SystemHealthAgent.exe"),
        );
    }

    #[test]
    fn classify_ignores_launch_arguments_via_parser() {
        // Regression : SCM's BINARY_PATH_NAME includes launch args, and a
        // pre-parse version of query_service_image_path returned "<exe>
        // --service" which classify() would flag as a spurious PathUpdate
        // on every relaunch. Verify the fix by feeding a raw command line
        // through the parser and checking the result equals the bare exe.
        let cmdline = r"C:\TT\Foo\SystemHealthAgent.exe --service";
        let parsed = parse_exe_from_command_line(cmdline);
        let current = PathBuf::from(r"C:\TT\Foo\SystemHealthAgent.exe");
        assert_eq!(classify(&current, Some(&parsed)), InstallAction::SamePathRelaunch);
    }

    // -------- classify — 4 additional coverage bumps --------

    #[test]
    fn classify_non_ascii_paths_case_insensitive() {
        // French folder names are common on Windows FR ("Bureau", "Téléchargements").
        // Case-insensitive compare is ASCII lowercase — non-ASCII lowercases per
        // Rust's to_lowercase which handles Unicode. Ensure that survives.
        let current = PathBuf::from(r"C:\Users\Élise\Bureau\SystemHealthAgent.exe");
        let existing = PathBuf::from(r"c:\users\élise\bureau\systemhealthagent.exe");
        assert_eq!(
            classify(&current, Some(&existing)),
            InstallAction::SamePathRelaunch
        );
    }

    #[test]
    fn classify_mixed_forward_and_back_slash_treated_as_different() {
        // paths_equal_ci does raw string compare — no separator normalization.
        // Lock in this behavior : mixed slashes = PathUpdate. If we ever add
        // normalization, this test breaks intentionally.
        let current = PathBuf::from(r"C:\Users\alice\SystemHealthAgent.exe");
        let existing = PathBuf::from("C:/Users/alice/SystemHealthAgent.exe");
        match classify(&current, Some(&existing)) {
            InstallAction::PathUpdate { .. } => {}
            other => panic!("expected PathUpdate, got {other:?}"),
        }
    }

    #[test]
    fn classify_trailing_whitespace_treated_as_different() {
        // No trimming ; a stray trailing space would flag PathUpdate. Behavior
        // lock-in — SCM shouldn't produce such paths, but if it does the
        // watchdog would rewire rather than silently masking the drift.
        let current = PathBuf::from(r"C:\Users\alice\SystemHealthAgent.exe");
        let existing = PathBuf::from("C:\\Users\\alice\\SystemHealthAgent.exe ");
        match classify(&current, Some(&existing)) {
            InstallAction::PathUpdate { .. } => {}
            other => panic!("expected PathUpdate, got {other:?}"),
        }
    }

    #[test]
    fn classify_unc_paths_case_insensitive_equal() {
        // UNC paths (\\server\share\...) are compared verbatim — case-insensitive
        // just like drive-letter paths. Users may have deployed to a network
        // share ; the healer should identify same-share as SamePathRelaunch.
        let current = PathBuf::from(r"\\SERVER\Share\Purr\SystemHealthAgent.exe");
        let existing = PathBuf::from(r"\\server\share\purr\systemhealthagent.exe");
        assert_eq!(
            classify(&current, Some(&existing)),
            InstallAction::SamePathRelaunch
        );
    }

    // -------- paths_equal_ci — 5 tests locking in lowercase-string semantics --------

    #[test]
    fn paths_equal_ci_pure_case_difference_equal() {
        assert!(paths_equal_ci(
            Path::new(r"C:\Foo\Bar.exe"),
            Path::new(r"c:\foo\bar.exe"),
        ));
    }

    #[test]
    fn paths_equal_ci_forward_vs_back_slash_not_equal() {
        // Raw string compare : no separator normalization.
        assert!(!paths_equal_ci(
            Path::new(r"C:\foo\bar.exe"),
            Path::new("C:/foo/bar.exe"),
        ));
    }

    #[test]
    fn paths_equal_ci_quoted_vs_unquoted_not_equal() {
        // Quotes are part of the string.
        assert!(!paths_equal_ci(
            Path::new(r#""C:\Foo\Bar.exe""#),
            Path::new(r"C:\Foo\Bar.exe"),
        ));
    }

    #[test]
    fn paths_equal_ci_whitespace_not_trimmed() {
        assert!(!paths_equal_ci(
            Path::new(r"C:\Foo\Bar.exe"),
            Path::new(r"C:\Foo\Bar.exe "),
        ));
    }

    #[test]
    fn paths_equal_ci_relative_vs_absolute_not_equal() {
        // Trailing "Bar.exe" from both, but relative form doesn't match absolute.
        assert!(!paths_equal_ci(
            Path::new(r"Bar.exe"),
            Path::new(r"C:\Foo\Bar.exe"),
        ));
    }

    #[test]
    fn data_paths_produce_expected_join() {
        let parent = PathBuf::from("Purr");
        assert_eq!(data_dir(&parent), parent.join("Data"));
        assert_eq!(animations_dir(&parent), parent.join("Data").join("Animations"));
        assert_eq!(webview2_dir(&parent), parent.join("Data").join("WebView2"));
        assert_eq!(logs_dir(&parent), parent.join("Data").join("Logs"));
    }

    #[test]
    fn is_legacy_rolling_log_matches_dated_files() {
        assert!(is_legacy_rolling_log("install.log.2026-07-15"));
        assert!(is_legacy_rolling_log("widget.log.2026-07-15"));
    }

    #[test]
    fn is_legacy_rolling_log_rejects_bare_and_unrelated() {
        assert!(!is_legacy_rolling_log("install.log"));
        assert!(!is_legacy_rolling_log("widget.log"));
        assert!(!is_legacy_rolling_log("service.log.2026-07-15"));
        assert!(!is_legacy_rolling_log("state.dat"));
    }

    #[test]
    fn migrate_from_legacy_resources_animations() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        // Simulate a 0.6.x install : .lottie files under resources/animations/
        let legacy = parent.join("resources").join("animations");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("cat.lottie"), b"payload-cat").unwrap();
        std::fs::write(legacy.join("dog.lottie"), b"payload-dog").unwrap();

        migrate_layout_to_data(parent, "SystemHealthAgent.exe").unwrap();

        let dst = animations_dir(parent);
        assert!(dst.join("cat.lottie").exists());
        assert!(dst.join("dog.lottie").exists());
        assert_eq!(std::fs::read(dst.join("cat.lottie")).unwrap(), b"payload-cat");
        // Empty legacy dir should be removed too.
        assert!(!legacy.exists());
    }

    #[test]
    fn migrate_from_orphan_exe_animations() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        let orphan = parent.join("animations");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("bar.lottie"), b"payload-bar").unwrap();

        migrate_layout_to_data(parent, "SystemHealthAgent.exe").unwrap();

        assert!(animations_dir(parent).join("bar.lottie").exists());
        assert!(!orphan.exists());
    }

    #[test]
    fn migrate_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        // Seed with one file so first run has real work.
        let legacy = parent.join("resources").join("animations");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("cat.lottie"), b"once").unwrap();

        migrate_layout_to_data(parent, "SystemHealthAgent.exe").unwrap();
        // Take a snapshot of Data/Animations after first run.
        let dst = animations_dir(parent);
        let snapshot: Vec<_> = std::fs::read_dir(&dst)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();

        // Second run must be a no-op.
        migrate_layout_to_data(parent, "SystemHealthAgent.exe").unwrap();

        let after: Vec<_> = std::fs::read_dir(&dst)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(snapshot, after);
        assert_eq!(std::fs::read(dst.join("cat.lottie")).unwrap(), b"once");
    }

    #[test]
    fn migrate_wipes_legacy_rolling_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        std::fs::write(parent.join("install.log.2026-07-10"), b"a").unwrap();
        std::fs::write(parent.join("widget.log.2026-07-11"), b"b").unwrap();
        // Unrelated files must survive.
        std::fs::write(parent.join("readme.txt"), b"keep me").unwrap();
        std::fs::write(parent.join("install.log"), b"bare, undated").unwrap();

        migrate_layout_to_data(parent, "SystemHealthAgent.exe").unwrap();

        assert!(!parent.join("install.log.2026-07-10").exists());
        assert!(!parent.join("widget.log.2026-07-11").exists());
        assert!(parent.join("readme.txt").exists());
        assert!(parent.join("install.log").exists());
    }

    #[test]
    fn default_config_stamped_writes_epoch_seconds() {
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let cfg = default_config_stamped(stamp);
        assert_eq!(cfg.first_install_at_epoch_secs, 1_700_000_000);
        assert_eq!(cfg.first_install_at(), stamp);
    }

    #[test]
    fn default_config_stamped_uses_now() {
        let now = std::time::SystemTime::now();
        let cfg = default_config_stamped(now);
        assert!(cfg.first_install_at_epoch_secs > 0);
    }

    #[test]
    fn migrate_early_returns_when_data_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        // Pre-scaffold the Data/ layout to trip the idempotency guard.
        std::fs::create_dir_all(animations_dir(parent)).unwrap();
        std::fs::create_dir_all(webview2_dir(parent)).unwrap();
        std::fs::create_dir_all(logs_dir(parent)).unwrap();

        // Seed a legacy .lottie file — the guard should skip the move.
        let legacy = parent.join("resources").join("animations");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("cat.lottie"), b"stay").unwrap();

        migrate_layout_to_data(parent, "SystemHealthAgent.exe").unwrap();

        // File stayed put because guard tripped.
        assert!(legacy.join("cat.lottie").exists());
        assert!(!animations_dir(parent).join("cat.lottie").exists());
    }
}
