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

mod bench;
mod config;
mod embedded;
mod ipc;
mod ipc_popup;
mod lottie_sanitize;
mod modes;
mod password;
mod platform;
mod runtime_dat;
mod scheduler;
mod signals;
mod time_authority;
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
            //
            // EXCEPT background daemon helpers (service, watchdog, and the
            // input-probe the service respawns) : they run unattended, are
            // resurrected automatically, and must never throw a dialog onto
            // the user's desktop. They've already logged via tracing above.
            #[cfg(windows)]
            if !matches!(mode, Mode::Service | Mode::Watchdog | Mode::InputProbe) {
                fatal_error_dialog(&mode, &err);
            }
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
            | Mode::InputProbe
            | Mode::RollbackFromFailedInstall
            | Mode::Uninstall
            | Mode::Countdown { .. }
            | Mode::Sandbox { .. }
            // Headless pipe client — no UI at all.
            | Mode::PopupIpc { .. }
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
            | Mode::InputProbe
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
        /// Argon2id PHC-format hash of the parent pincode. `None` when
        /// the flag is absent (fresh install pre-passcode-set) — the
        /// popup then skips the Alt+pincode capture path entirely and
        /// only the natural countdown-then-dismiss path is available.
        passcode_hash: Option<String>,
        /// `"daily-pause"` when spawned by the daily-pause fire path ;
        /// `None` = interval mode (default). Switches the source of
        /// popup title / subtitle / duration from `cfg.*` to
        /// `cfg.daily_pause.*`.
        mode: Option<String>,
        /// Cas D respawn : when the service was killed mid-pause and
        /// restarts, the popup is respawned with the elapsed wall time
        /// so the dismiss countdown resumes at the right offset (or
        /// activates immediately if elapsed >= duration).
        elapsed_seconds: Option<u32>,
        /// `Some(secs)` when spawned with `--bench [--bench-secs N]`
        /// (config UI Animations tab, Benchmark checkbox). The popup
        /// samples its process tree's CPU/GPU/memory every 500 ms into
        /// `Data\Logs\bench.log` and self-exits after N seconds.
        bench_secs: Option<u32>,
    },
    /// Small corner countdown widget for T-15 / T-10 / T-5 / T-1.
    /// `debug` skips force-minimize so the current window isn't lost.
    Countdown {
        seconds: u64,
        palier: u32,
        debug: bool,
        config_override: Option<String>,
        /// `"daily-pause"` when spawned for the noon pause palier —
        /// countdown reads `cfg.daily_pause.pre_notif_messages[palier]`
        /// instead of the interval `cfg.pre_notif_messages[palier]`.
        mode: Option<String>,
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
    /// User-session keyboard/mouse activity probe. Spawned by the
    /// service via CreateProcessAsUserW, runs in the console user's
    /// session, polls GetLastInputInfo, pushes the last-input tick
    /// to the SYSTEM service over the named-pipe IPC every ~5 s. See
    /// modes::input_probe for the Session 0 isolation rationale.
    InputProbe,
    /// Developer smoke vehicle for the popup content pipe : sends one
    /// framed `PopupMessage` to the live popup running as `pid`.
    /// Undocumented in user-facing help ; `Dismiss` / `RestartCountdown`
    /// have no production sender yet, this flag is how the host smoke
    /// exercises them against a real WebView2 popup.
    PopupIpc { pid: u32, spec: Vec<String> },
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
        Some("--input-probe") => Mode::InputProbe,
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
            passcode_hash: parse_string_after(args, "--passcode-hash"),
            mode: parse_string_after(args, "--mode"),
            elapsed_seconds: parse_u32_after(args, "--elapsed-seconds"),
            bench_secs: args.iter().any(|a| a == "--bench").then(|| {
                parse_u32_after(args, "--bench-secs").unwrap_or(bench::DEFAULT_BENCH_SECS)
            }),
        },
        Some("--countdown") => parse_countdown(args),
        Some("--config") => Mode::Config {
            first_run: args.iter().any(|a| a == "--first-run"),
        },
        Some("--popup-ipc") => match args.get(1).and_then(|s| s.parse::<u32>().ok()) {
            Some(pid) => Mode::PopupIpc {
                pid,
                spec: args[2..].to_vec(),
            },
            None => Mode::Unknown(args.to_vec()),
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

/// Generic `--flag <string>` extractor. Returns the following argv
/// value verbatim, or `None` when the flag is absent. Used for
/// `--passcode-hash <phc>` — the value is an argon2id PHC string, which
/// only uses base64 chars + `$` `=` `,` and round-trips through
/// Windows argv without escaping.
fn parse_string_after(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
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
    let mode = parse_string_after(args, "--mode");
    Mode::Countdown { seconds, palier, debug, config_override, mode }
}

/// Parse the `--popup-ipc` message spec into a wire message.
/// Grammar :
///   dismiss
///   restart <palier_minutes> <seconds_until>
///   update <title> <subtitle> <dismiss_label> <duration_secs> <interval|daily-pause>
fn parse_popup_ipc_spec(spec: &[String]) -> Option<ipc_popup::PopupMessage> {
    match spec.first().map(String::as_str)? {
        "dismiss" if spec.len() == 1 => Some(ipc_popup::PopupMessage::Dismiss),
        "restart" if spec.len() == 3 => Some(ipc_popup::PopupMessage::RestartCountdown {
            palier_minutes: spec[1].parse().ok()?,
            seconds_until: spec[2].parse().ok()?,
        }),
        "update" if spec.len() == 6 => {
            let mode = match spec[5].as_str() {
                "interval" => ipc_popup::PopupContentMode::Interval,
                "daily-pause" => ipc_popup::PopupContentMode::DailyPause,
                _ => return None,
            };
            Some(ipc_popup::PopupMessage::UpdateContent {
                title: spec[1].clone(),
                subtitle: spec[2].clone(),
                dismiss_label: spec[3].clone(),
                duration_secs: spec[4].parse().ok()?,
                mode,
            })
        }
        _ => None,
    }
}

fn dispatch(mode: Mode) -> anyhow::Result<()> {
    match mode {
        Mode::InstallOrConfig => modes::install::run()?,
        Mode::RollbackFromFailedInstall => modes::rollback::run()?,
        #[cfg(windows)]
        Mode::Config { first_run: true } => modes::config_first_run::run()?,
        Mode::Service => modes::service::run()?,
        Mode::Watchdog => modes::watchdog::run()?,
        Mode::InputProbe => modes::input_probe::run()?,
        Mode::Popup { preview, debug, anim_override, config_override, test_countdown_secs, passcode_hash, mode: popup_mode, elapsed_seconds, bench_secs } => {
            modes::popup::run(preview, debug, anim_override, config_override, test_countdown_secs, passcode_hash, popup_mode, elapsed_seconds, bench_secs)?
        }
        Mode::Countdown { seconds, palier, debug, config_override, mode: cd_mode } => {
            modes::countdown::run(seconds, palier, debug, config_override, cd_mode)?
        }
        Mode::Config { first_run: false } => modes::config::run()?,
        #[cfg(not(windows))]
        Mode::Config { first_run: true } => {
            anyhow::bail!("config wizard is Windows-only")
        }
        Mode::Sandbox { preset } => modes::sandbox::run(&preset)?,
        Mode::PopupIpc { pid, spec } => {
            let msg = parse_popup_ipc_spec(&spec).ok_or_else(|| {
                anyhow::anyhow!(
                    "bad --popup-ipc spec (dismiss | restart <palier> <secs> | update <title> <subtitle> <label> <secs> <interval|daily-pause>)"
                )
            })?;
            ipc_popup::send_to_popup(pid, &msg)?;
        }
        Mode::Uninstall => modes::uninstall::run()?,
        Mode::Unknown(argv) => {
            eprintln!("Unknown argv: {argv:?}");
            eprintln!("Valid modes: --service | --popup [--preview] [--anim <file>] [--bench [--bench-secs N]] | --countdown <secs> --palier <15|10|5> | --config [--first-run] | --watchdog | --uninstall | --rollback-from-failed-install | --sandbox --try <preset>");
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
            Mode::Popup { preview: true, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
        );
    }

    #[test]
    fn popup_without_preview() {
        assert_eq!(
            classify_mode(&as_args(["--popup"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
        );
    }

    #[test]
    fn popup_default_has_no_debug() {
        assert_eq!(
            classify_mode(&as_args(["--popup"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
        );
    }

    #[test]
    fn popup_debug_opts_in() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--debug"])),
            Mode::Popup { preview: false, debug: true, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
        );
    }

    #[test]
    fn popup_legacy_no_debug_is_ignored() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--no-debug"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
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
                passcode_hash: None,
                mode: None,
                elapsed_seconds: None,
                bench_secs: None,
            }
        );
    }

    #[test]
    fn popup_anim_traversal_rejected() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--anim", "../etc/passwd"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
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
                passcode_hash: None,
                mode: None,
                elapsed_seconds: None,
                bench_secs: None,
            }
        );
    }

    #[test]
    fn popup_anim_absolute_path_rejected() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--anim", "/etc/hosts"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
        );
    }

    #[test]
    fn popup_passcode_hash_captured() {
        // The PHC value contains the argon2id sigil `$argon2id$…` — argv
        // round-trips it verbatim on both Windows and *nix.
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$abcabcabcabcabcabc$xyzxyzxyzxyzxyzxyzxyzxyzxyzxyzxyzxyzxyzxyzxyz";
        assert_eq!(
            classify_mode(&as_args(["--popup", "--passcode-hash", hash])),
            Mode::Popup {
                preview: false,
                debug: false,
                anim_override: None,
                config_override: None,
                test_countdown_secs: None,
                passcode_hash: Some(hash.to_string()),
                mode: None,
                elapsed_seconds: None,
                bench_secs: None,
            }
        );
    }

    #[test]
    fn popup_passcode_hash_absent_is_none() {
        assert_eq!(
            classify_mode(&as_args(["--popup"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
        );
    }

    #[test]
    fn countdown_parsed() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "900", "--palier", "15"])),
            Mode::Countdown { seconds: 900, palier: 15, debug: false, config_override: None, mode: None }
        );
    }

    #[test]
    fn countdown_default_has_no_debug() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "60", "--palier", "1"])),
            Mode::Countdown { seconds: 60, palier: 1, debug: false, config_override: None, mode: None }
        );
    }

    #[test]
    fn countdown_debug_opts_in() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "60", "--palier", "1", "--debug"])),
            Mode::Countdown { seconds: 60, palier: 1, debug: true, config_override: None, mode: None }
        );
    }

    #[test]
    fn countdown_legacy_no_debug_is_ignored() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "60", "--palier", "1", "--no-debug"])),
            Mode::Countdown { seconds: 60, palier: 1, debug: false, config_override: None, mode: None }
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

    #[test]
    fn popup_mode_daily_pause_captured() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--mode", "daily-pause"])),
            Mode::Popup {
                preview: false,
                debug: false,
                anim_override: None,
                config_override: None,
                test_countdown_secs: None,
                passcode_hash: None,
                mode: Some("daily-pause".to_string()),
                elapsed_seconds: None,
                bench_secs: None,
            }
        );
    }

    #[test]
    fn popup_elapsed_seconds_captured() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--mode", "daily-pause", "--elapsed-seconds", "900"])),
            Mode::Popup {
                preview: false,
                debug: false,
                anim_override: None,
                config_override: None,
                test_countdown_secs: None,
                passcode_hash: None,
                mode: Some("daily-pause".to_string()),
                elapsed_seconds: Some(900),
                bench_secs: None,
            }
        );
    }

    #[test]
    fn popup_bench_flag_with_secs() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--preview", "--bench", "--bench-secs", "30"])),
            Mode::Popup { preview: true, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: Some(30) }
        );
    }

    #[test]
    fn popup_bench_flag_defaults_secs() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--bench"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: Some(bench::DEFAULT_BENCH_SECS) }
        );
    }

    #[test]
    fn popup_bench_secs_without_bench_is_none() {
        assert_eq!(
            classify_mode(&as_args(["--popup", "--bench-secs", "30"])),
            Mode::Popup { preview: false, debug: false, anim_override: None, config_override: None, test_countdown_secs: None, passcode_hash: None, mode: None, elapsed_seconds: None, bench_secs: None }
        );
    }

    #[test]
    fn countdown_mode_daily_pause_captured() {
        assert_eq!(
            classify_mode(&as_args(["--countdown", "600", "--palier", "10", "--mode", "daily-pause"])),
            Mode::Countdown {
                seconds: 600,
                palier: 10,
                debug: false,
                config_override: None,
                mode: Some("daily-pause".to_string()),
            }
        );
    }

    #[test]
    fn popup_ipc_mode_classified() {
        assert_eq!(
            classify_mode(&as_args(["--popup-ipc", "1234", "dismiss"])),
            Mode::PopupIpc {
                pid: 1234,
                spec: vec!["dismiss".to_string()],
            }
        );
    }

    #[test]
    fn popup_ipc_missing_or_bad_pid_is_unknown() {
        assert!(matches!(
            classify_mode(&as_args(["--popup-ipc"])),
            Mode::Unknown(_)
        ));
        assert!(matches!(
            classify_mode(&as_args(["--popup-ipc", "not-a-pid", "dismiss"])),
            Mode::Unknown(_)
        ));
    }

    #[test]
    fn parse_popup_ipc_spec_dismiss() {
        assert_eq!(
            parse_popup_ipc_spec(&as_args(["dismiss"])),
            Some(ipc_popup::PopupMessage::Dismiss)
        );
    }

    #[test]
    fn parse_popup_ipc_spec_restart() {
        assert_eq!(
            parse_popup_ipc_spec(&as_args(["restart", "15", "900"])),
            Some(ipc_popup::PopupMessage::RestartCountdown {
                palier_minutes: 15,
                seconds_until: 900,
            })
        );
    }

    #[test]
    fn parse_popup_ipc_spec_update_daily() {
        assert_eq!(
            parse_popup_ipc_spec(&as_args([
                "update", "Pause", "On mange", "Fini", "1800", "daily-pause"
            ])),
            Some(ipc_popup::PopupMessage::UpdateContent {
                title: "Pause".to_string(),
                subtitle: "On mange".to_string(),
                dismiss_label: "Fini".to_string(),
                duration_secs: 1800,
                mode: ipc_popup::PopupContentMode::DailyPause,
            })
        );
    }

    #[test]
    fn parse_popup_ipc_spec_update_interval() {
        assert_eq!(
            parse_popup_ipc_spec(&as_args(["update", "T", "S", "L", "60", "interval"])),
            Some(ipc_popup::PopupMessage::UpdateContent {
                title: "T".to_string(),
                subtitle: "S".to_string(),
                dismiss_label: "L".to_string(),
                duration_secs: 60,
                mode: ipc_popup::PopupContentMode::Interval,
            })
        );
    }

    #[test]
    fn parse_popup_ipc_spec_garbage_is_none() {
        assert_eq!(parse_popup_ipc_spec(&[]), None);
        assert_eq!(parse_popup_ipc_spec(&as_args(["frob"])), None);
        // Wrong arity for each verb.
        assert_eq!(parse_popup_ipc_spec(&as_args(["dismiss", "extra"])), None);
        assert_eq!(parse_popup_ipc_spec(&as_args(["restart", "15"])), None);
        assert_eq!(
            parse_popup_ipc_spec(&as_args(["update", "T", "S", "L", "60"])),
            None
        );
        // Bad numeric / mode values.
        assert_eq!(parse_popup_ipc_spec(&as_args(["restart", "x", "900"])), None);
        assert_eq!(
            parse_popup_ipc_spec(&as_args(["update", "T", "S", "L", "60", "weekly"])),
            None
        );
    }
}
