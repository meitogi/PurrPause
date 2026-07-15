// Entry point for SystemHealthAgent (project codename: PurrPause).
//
// Session 1 delivers only the argv dispatcher — every mode is a stub that
// logs its label and exits. Later sessions replace each stub body with the
// real implementation per /workspace/plans/purrpause/ROLLOUT.md.
//
// Argv contract mirrors the "Modes du binaire (dispatch argv)" table in the
// design constitution. Keep this file in sync with STATUS.md as modes get
// fleshed out.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod embedded;
mod ipc;
mod lottie_sanitize;
mod modes;
mod password;
mod platform;
mod runtime_dat;
mod scheduler;
#[cfg(test)]
mod tracing_lint;
#[cfg(test)]
#[path = "../build_support/versioninfo.rs"]
mod versioninfo_test;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = classify_mode(&args);
    init_tracing(&mode);

    tracing::info!(?mode, argv = ?args, "SystemHealthAgent starting");

    // Pre-flight : WebView2 runtime must be available before any UI mode.
    // The service / countdown / popup modes all end up needing wry, and
    // installing without WebView2 would leave a broken install. Bail
    // early with a native MessageBoxW dialog if it's missing.
    #[cfg(windows)]
    if mode_needs_webview2(&mode) {
        if let Err(err) = platform::win32::webview2::ensure_available() {
            tracing::error!(error = %err, "WebView2 pre-flight failed");
            return ExitCode::FAILURE;
        }
    }

    match dispatch(mode.clone()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "mode handler failed");
            // Under windows_subsystem="windows" nothing is visible on
            // failure — pop a MessageBox so the user sees WHAT went
            // wrong, especially for the install-flow modes that route
            // to install.log which not everyone thinks to check.
            #[cfg(windows)]
            fatal_error_dialog(&mode, &err);
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn fatal_error_dialog(mode: &Mode, err: &anyhow::Error) {
    use std::iter::once;
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let title = format!("SystemHealthAgent — {mode:?}");
    let body = format!(
        "Une erreur fatale est survenue :\n\n{err:#}\n\nLog complet : install.log ou widget.log à côté de l'exe."
    );
    let title_w: Vec<u16> = title.encode_utf16().chain(once(0)).collect();
    let body_w: Vec<u16> = body.encode_utf16().chain(once(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(windows)]
fn mode_needs_webview2(mode: &Mode) -> bool {
    // Headless / cleanup paths never need WebView2 — the service runs
    // in Session 0, the watchdog exits after checking SCM state, and
    // rollback + uninstall are teardown. Everything else ends up in
    // the WebView2 dep graph one way or another (popup renders Lottie ;
    // the wizard opens an egui window that co-exists with wry).
    !matches!(
        mode,
        Mode::Service
            | Mode::Watchdog
            | Mode::RollbackFromFailedInstall
            | Mode::Uninstall
            | Mode::Countdown { .. }
            | Mode::Sandbox { .. }
            // Pure-egui config UI — no WebView2 needed.
            | Mode::Config { first_run: false }
    )
}

fn init_tracing(mode: &Mode) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Service and watchdog both run in SYSTEM context, write to
    // C:\ProgramData\DiagnosticsCache\, but land in DIFFERENT files so a
    // grep for "watchdog tick" doesn't have to wade through service loop
    // noise (and vice versa). Both are rolled daily and never rotated
    // otherwise — the DiagnosticsCache folder is wiped by --uninstall.
    #[cfg(windows)]
    if matches!(mode, Mode::Service | Mode::Watchdog) {
        let log_dir = std::path::Path::new(modes::install::DIAGNOSTICS_CACHE_DIR);
        let _ = std::fs::create_dir_all(log_dir);
        let filename = if matches!(mode, Mode::Watchdog) {
            "watchdog.log"
        } else {
            "service.log"
        };
        let appender = tracing_appender::rolling::daily(log_dir, filename);
        let _ = fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(appender)
            .try_init();
        return;
    }

    // User-session UI modes (Countdown, Popup, Config) have no console
    // under windows_subsystem = "windows" — route to a file next to the
    // exe so tracing survives long enough to diagnose crashes. Falls
    // back to stderr on any error (dead pipe, but harmless).
    //
    // Landing dir is `<exe_dir>/Data/Logs/` so the exe folder stays
    // clean ; create_dir_all runs BEFORE opening the appender because
    // on first install Data/Logs/ doesn't exist yet.
    #[cfg(windows)]
    if matches!(
        mode,
        Mode::Countdown { .. }
            | Mode::Popup { .. }
            | Mode::Config { .. }
            | Mode::Sandbox { .. }
    ) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let log_dir = modes::install::logs_dir(dir);
                let _ = std::fs::create_dir_all(&log_dir);
                let appender = tracing_appender::rolling::daily(&log_dir, "widget.log");
                let _ = fmt()
                    .with_env_filter(filter)
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(appender)
                    .try_init();
                return;
            }
        }
    }

    // Install-flow modes (InstallOrConfig, Rollback, Uninstall) also
    // run without a console under `windows_subsystem = "windows"`.
    // Route them to `<exe_dir>/Data/Logs/install.log.YYYY-MM-DD` so
    // any failure during the 10-step install is diagnosable after the
    // fact. First-run creates Data/Logs/ ahead of the appender open
    // (chicken-and-egg : install mode logs its OWN creation of Data/).
    #[cfg(windows)]
    if matches!(
        mode,
        Mode::InstallOrConfig | Mode::RollbackFromFailedInstall | Mode::Uninstall
    ) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let log_dir = modes::install::logs_dir(dir);
                let _ = std::fs::create_dir_all(&log_dir);
                let appender = tracing_appender::rolling::daily(&log_dir, "install.log");
                let _ = fmt()
                    .with_env_filter(filter)
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(appender)
                    .try_init();
                return;
            }
        }
    }

    let _ = mode; // silence unused-var on non-Windows
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Default: installer / update-path / launch config UI, decided at runtime.
    InstallOrConfig,
    /// Windows service entry point, invoked by SCM.
    Service,
    /// Fullscreen Lottie break popup, spawned by the service.
    /// `debug` skips the keyboard hook (Alt+F4 / Alt+Tab work) so a
    /// developer can exit the popup during smoke without killing the
    /// process from Task Manager.
    Popup {
        preview: bool,
        debug: bool,
        anim_override: Option<String>,
        config_override: Option<String>,
        test_countdown_secs: Option<u32>,
    },
    /// Small corner countdown widget for T-15 / T-10 / T-5 / T-1.
    /// `debug` skips force-minimize so the current window isn't lost.
    Countdown {
        seconds: u64,
        palier: u32,
        debug: bool,
        config_override: Option<String>,
    },
    /// Isolated diagnostic window with tunable options. Zero
    /// dependency on the rest of the app (no state.dat load, no
    /// wry, no keyboard hook). See `modes::sandbox::run` for the
    /// preset menu.
    Sandbox { preset: String },
    /// Passcode-gated config UI (egui window).
    Config { first_run: bool },
    /// Health-check task run by Scheduled Task every minute.
    Watchdog,
    /// Passcode-gated uninstall flow.
    Uninstall,
    /// Rollback partial install if wizard was cancelled.
    RollbackFromFailedInstall,
    /// Argv did not match any known mode.
    Unknown(Vec<String>),
}

fn classify_mode(args: &[String]) -> Mode {
    match args.first().map(String::as_str) {
        None => Mode::InstallOrConfig,
        Some("--service") => Mode::Service,
        Some("--popup") => Mode::Popup {
            preview: args.iter().any(|a| a == "--preview"),
            // Prod default : debug=false (keyboard hook + force-minimize
            // enforced). Developers opt-in via --debug for manual smoke.
            // `--no-debug` is still parsed harmlessly by service.rs and
            // config UI preview spawns as belt-and-suspenders redundancy.
            debug: args.iter().any(|a| a == "--debug"),
            anim_override: parse_anim_override(args),
            config_override: parse_config_override(args),
            test_countdown_secs: parse_u32_after(args, "--test-countdown-secs"),
        },
        Some("--countdown") => parse_countdown(args),
        Some("--config") => Mode::Config {
            first_run: args.iter().any(|a| a == "--first-run"),
        },
        Some("--watchdog") => Mode::Watchdog,
        Some("--uninstall") => Mode::Uninstall,
        Some("--rollback-from-failed-install") => Mode::RollbackFromFailedInstall,
        Some("--sandbox") => Mode::Sandbox {
            preset: args
                .iter()
                .position(|a| a == "--try")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_else(|| "plain".to_string()),
        },
        Some(_) => Mode::Unknown(args.to_vec()),
    }
}

/// Reads `--anim <relative_path>` from a `--popup` argv. Rejects paths
/// containing `..` (path traversal guard) — a valid override is a plain
/// filename or `subdir/name.lottie` under `<exe_dir>/resources/animations/`.
/// Absent or invalid → `None`, and `modes::popup::run` falls back to the
/// config's `animation_pick`.
fn parse_anim_override(args: &[String]) -> Option<String> {
    let idx = args.iter().position(|a| a == "--anim")?;
    let raw = args.get(idx + 1)?;
    if raw.contains("..") || raw.starts_with('/') || raw.starts_with('\\') {
        tracing::warn!(path = %raw, "rejecting --anim path (traversal or absolute)");
        return None;
    }
    Some(raw.clone())
}

/// Reads `--config-override <absolute_path>` for the popup / countdown
/// preview flow. The config UI writes the in-memory (un-saved) config
/// to a per-PID temp file and passes the path here so the spawned
/// child sees the current values without state.dat autosave. Absent →
/// `None` and the child uses the real state.dat.
fn parse_config_override(args: &[String]) -> Option<String> {
    let idx = args.iter().position(|a| a == "--config-override")?;
    args.get(idx + 1).cloned()
}

/// Generic `--flag <u32>` extractor. Returns `None` when the flag is
/// absent or the value isn't parseable. Used for `--test-countdown-secs`
/// so a parent can spawn a full-prod popup (keyboard hook + force-
/// minimize) with a short countdown for Alt-Tab / dismiss testing.
fn parse_u32_after(args: &[String], flag: &str) -> Option<u32> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1)?.parse::<u32>().ok()
}

fn parse_countdown(args: &[String]) -> Mode {
    // Shape: --countdown <secs> --palier <15|10|5|1> [--debug] [--config-override <path>]
    let seconds = args
        .get(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let palier = args
        .iter()
        .position(|a| a == "--palier")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    // Prod default : debug=false. --debug opts in (skips force-minimize
    // for manual dev iteration). Legacy `--no-debug` still parsed
    // harmlessly by callers as belt-and-suspenders redundancy.
    let debug = args.iter().any(|a| a == "--debug");
    let config_override = parse_config_override(args);
    Mode::Countdown { seconds, palier, debug, config_override }
}

fn dispatch(mode: Mode) -> anyhow::Result<()> {
    match mode {
        Mode::InstallOrConfig => modes::install::run()?,
        Mode::RollbackFromFailedInstall => modes::rollback::run()?,
        #[cfg(windows)]
        Mode::Config { first_run: true } => modes::config_first_run::run()?,
        Mode::Service => modes::service::run()?,
        Mode::Watchdog => modes::watchdog::run()?,
        Mode::Popup { preview, debug, anim_override, config_override, test_countdown_secs } => {
            modes::popup::run(preview, debug, anim_override, config_override, test_countdown_secs)?
        }
        Mode::Countdown { seconds, palier, debug, config_override } => {
            modes::countdown::run(seconds, palier, debug, config_override)?
        }
        Mode::Config { first_run: false } => modes::config::run()?,
        #[cfg(not(windows))]
        Mode::Config { first_run: true } => {
            anyhow::bail!("config wizard is Windows-only")
        }
        Mode::Sandbox { preset } => modes::sandbox::run(&preset)?,
        Mode::Uninstall => modes::uninstall::run()?,
        Mode::Unknown(argv) => {
            eprintln!("Unknown argv: {argv:?}");
            eprintln!("Valid modes: --service | --popup [--preview] [--anim <file>] | --countdown <secs> --palier <15|10|5> | --config [--first-run] | --watchdog | --uninstall | --rollback-from-failed-install | --sandbox --try <preset>");
            anyhow::bail!("unknown mode");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_args<const N: usize>(a: [&str; N]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_is_install_or_config() {
        assert_eq!(classify_mode(&[]), Mode::InstallOrConfig);
    }

    #[test]
    fn service_flag() {
        assert_eq!(classify_mode(&as_args(["--service"])), Mode::Service);
    }

    #[test]
    fn popup_with_preview() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--preview"])),
            Mode::Popup { preview: true, debug: false, anim_override: None, config_override: None, test_countdown_secs: None }
        );
    }

    #[test]
    fn popup_without_preview() {
        assert_eq!(
            classify_mode(&as_args(["--popup"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None }
        );
    }

    #[test]
    fn popup_default_has_no_debug() {
        assert_eq!(
            classify_mode(&as_args(["--popup"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None }
        );
    }

    #[test]
    fn popup_debug_opts_in() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--debug"])),
            Mode::Popup { preview: false, debug: true, anim_override: None, config_override: None, test_countdown_secs: None }
        );
    }

    #[test]
    fn popup_legacy_no_debug_is_ignored() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--no-debug"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None }
        );
    }

    #[test]
    fn popup_anim_override_accepted() {
        assert_eq!(
            classify_mode(&as_args([
                "--popup", "--preview", "--anim", "dance-cat.lottie"
            ])),
            Mode::Popup {
                preview: true,
                debug: false,
                anim_override: Some("dance-cat.lottie".to_string()),
                config_override: None,
                test_countdown_secs: None,
            }
        );
    }

    #[test]
    fn popup_anim_traversal_rejected() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--anim", "../etc/passwd"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None }
        );
    }

    #[test]
    fn popup_test_countdown_combined_prod() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--test-countdown-secs", "10"])),
            Mode::Popup {
                preview: false,
                debug: false,
                anim_override: None,
                config_override: None,
                test_countdown_secs: Some(10),
            }
        );
    }

    #[test]
    fn popup_anim_absolute_path_rejected() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--anim", "/etc/hosts"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None }
        );
    }

    #[test]
    fn countdown_parsed() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "900", "--palier", "15"])),
            Mode::Countdown { seconds: 900, palier: 15, debug: false, config_override: None }
        );
    }

    #[test]
    fn countdown_default_has_no_debug() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "60", "--palier", "1"])),
            Mode::Countdown { seconds: 60, palier: 1, debug: false, config_override: None }
        );
    }

    #[test]
    fn countdown_debug_opts_in() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "60", "--palier", "1", "--debug"])),
            Mode::Countdown { seconds: 60, palier: 1, debug: true, config_override: None }
        );
    }

    #[test]
    fn countdown_legacy_no_debug_is_ignored() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "60", "--palier", "1", "--no-debug"])),
            Mode::Countdown { seconds: 60, palier: 1, debug: false, config_override: None }
        );
    }

    #[test]
    fn config_first_run() {
        assert_eq!(
            classify_mode(&as_args(["--config", "--first-run"])),
            Mode::Config { first_run: true }
        );
    }

    #[test]
    fn unknown_flag() {
        let m = classify_mode(&as_args(["--garbage"]));
        assert!(matches!(m, Mode::Unknown(_)));
    }
}
