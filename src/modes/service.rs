// Windows service main. Registers the SCM handler, reports Running
// within the 30s deadline, then runs a tick loop that decides when to
// spawn popup / countdown children in the active user's session.
//
// Background threads :
//   - IPC named-pipe server (accept loop, message-mode, overlapped)
//   - state.dat file watcher (notify, hand-debounced ~300ms)
// Both feed TickCommand messages back to the main tick loop over an
// mpsc channel. The tick loop is the only reader/writer of the
// in-memory config + last_popup + fired_paliers state — no mutex.
//
// Last-popup timestamp is persisted to runtime.dat (plaintext big-endian
// u64 seconds since epoch, atomic write via .tmp+rename) so service
// restart doesn't reset the schedule.

use anyhow::Result;

#[cfg(windows)]
pub fn run() -> Result<()> {
    win::run()
}

#[cfg(not(windows))]
pub fn run() -> Result<()> {
    anyhow::bail!("service is Windows-only")
}

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, SystemTime};

    use anyhow::{Context, Result};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::SetEvent;
    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    use crate::config::{self, Config};
    use crate::ipc::{self, Message};
    use crate::modes::install::{webview2_dir, DIAGNOSTICS_CACHE_DIR, SERVICE_NAME, STATE_DAT};
    use crate::platform::win32::proc_kill::kill_orphan_webview2_workers;
    use crate::platform::win32::spawn_user::{RealSpawner, SpawnedChild};
    use crate::platform::{spawn_replacing, spawn_replacing_keyed};
    use crate::runtime_dat;
    use crate::scheduler::{self, SchedulerDecision};

    const TICK_INTERVAL: Duration = Duration::from_secs(5);
    const DEBOUNCE: Duration = Duration::from_millis(300);
    const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

    #[derive(Debug, Clone, Copy)]
    enum TickCommand {
        Trigger,
        Reload,
        Shutdown,
    }

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("service_dispatcher::start")
    }

    fn service_main(_argv: Vec<OsString>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_service));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = ?e, "service exited with error"),
            Err(_) => tracing::error!("service main panicked - SCM watchdog will restart"),
        }
    }

    fn run_service() -> Result<()> {
        let (tx, rx) = mpsc::channel::<TickCommand>();

        let ctrl_tx = tx.clone();
        let status_handle = service_control_handler::register(SERVICE_NAME, move |ctl| {
            on_control_event(ctl, &ctrl_tx)
        })
        .context("service_control_handler::register")?;

        // Report Running IMMEDIATELY — heavy init happens below.
        status_handle
            .set_service_status(running())
            .context("SetServiceStatus(Running)")?;

        let exe = env::current_exe().context("env::current_exe()")?;
        tracing::info!(exe = %exe.display(), "service running");

        // Startup sweep : any msedgewebview2.exe still running from our
        // install's Data\WebView2 is an orphan from a prior session
        // (session-9 Track B-primary revert left this hole). Kill them
        // before we spawn any new popups.
        if let Some(exe_parent) = exe.parent() {
            kill_orphan_webview2_workers(&webview2_dir(exe_parent));
        }

        let state_path = PathBuf::from(STATE_DAT);
        let mut config = config::load_or_default(&state_path);
        let now = SystemTime::now();
        let runtime_last = runtime_dat::read();
        let had_runtime = runtime_last.is_some();
        let mut last_popup = scheduler::resolve_last_popup(
            scheduler::ResolveInputs {
                now,
                last_popup: runtime_last,
                first_install_at: config.first_install_at(),
                is_reload: false,
            },
            &config,
        );
        // Persist the resolved timestamp so a mid-cycle restart doesn't
        // re-enter the migration branch and re-derive from scratch.
        if !had_runtime {
            let _ = runtime_dat::write(last_popup);
        }
        let mut fired = scheduler::derive_already_fired(now, &config, last_popup);
        tracing::info!(
            initial_paliers_fired = ?fired,
            "scheduler initialised"
        );

        // Background threads.
        let cancel_event = ipc::create_cancel_event().context("create cancel event")?;
        let ipc_tx = tx.clone();
        let sendable = ipc::SendableHandle(cancel_event);
        let ipc_tx_map = ipc_tx;
        let pipe_thread = thread::spawn(move || run_pipe(sendable, ipc_tx_map));

        let notify_running_flag = Arc::new(AtomicBool::new(true));
        let notify_thread = spawn_notify(tx.clone(), notify_running_flag.clone());

        // Track spawned children so we can TerminateProcess a stale
        // predecessor before spawning a replacement — the post-countdown
        // unlock (0.7.1 Track A) lets a user Alt+Tab away from a still-
        // running popup, so two popups would otherwise stack up on the
        // next schedule tick. Handle-based (not PID-based) so a recycled
        // PID cannot cause us to terminate an unrelated process.
        let mut last_popup_child: Option<SpawnedChild> = None;
        let mut last_widget_children: HashMap<u32, SpawnedChild> = HashMap::new();

        // Main tick loop.
        let loop_result = tick_loop(
            &mut config,
            &mut last_popup,
            &mut fired,
            &mut last_popup_child,
            &mut last_widget_children,
            &exe,
            &rx,
        );

        // Signal shutdown.
        notify_running_flag.store(false, Ordering::Relaxed);
        unsafe { let _ = SetEvent(cancel_event); }

        // Grace period + best-effort join. If a thread refuses to exit,
        // process shutdown will terminate it — no forced-kill from Rust.
        thread::sleep(SHUTDOWN_GRACE);
        let _ = pipe_thread.join();
        let _ = notify_thread.join();
        unsafe { let _ = CloseHandle(cancel_event); }

        let exit_code = if loop_result.is_err() { 1 } else { 0 };
        if let Err(e) = status_handle.set_service_status(stopped(exit_code)) {
            tracing::warn!(error = ?e, "SetServiceStatus(Stopped) failed");
        }
        loop_result
    }

    fn on_control_event(
        ctl: ServiceControl,
        tx: &mpsc::Sender<TickCommand>,
    ) -> ServiceControlHandlerResult {
        match ctl {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = tx.send(TickCommand::Shutdown);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    }

    fn tick_loop(
        config: &mut Config,
        last_popup: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        exe: &Path,
        rx: &mpsc::Receiver<TickCommand>,
    ) -> Result<()> {
        loop {
            match rx.recv_timeout(TICK_INTERVAL) {
                Ok(TickCommand::Shutdown) => {
                    tracing::info!("tick loop received Shutdown");
                    return Ok(());
                }
                Ok(TickCommand::Trigger) => {
                    tracing::info!("TriggerPopupNow received");
                    if let Err(e) = fire_popup(exe, last_popup, fired, last_popup_child) {
                        tracing::warn!(error = ?e, "trigger popup failed");
                    }
                }
                Ok(TickCommand::Reload) => {
                    tracing::info!("config reload triggered");
                    let state_path = PathBuf::from(STATE_DAT);
                    *config = config::load_or_default(&state_path);
                    let now = SystemTime::now();
                    // If the user reduced interval_hours enough that
                    // `last_popup + new_interval` lands in the past
                    // (or within RELOAD_MIN_GRACE), bump last_popup
                    // forward so the next popup fires at now+2min
                    // rather than instantly on Save.
                    let clamped = scheduler::resolve_last_popup(
                        scheduler::ResolveInputs {
                            now,
                            last_popup: Some(*last_popup),
                            first_install_at: config.first_install_at(),
                            is_reload: true,
                        },
                        config,
                    );
                    if clamped != *last_popup {
                        tracing::info!(
                            old_last = ?*last_popup,
                            new_last = ?clamped,
                            "reload: interval reduced, clamping last_popup forward",
                        );
                        *last_popup = clamped;
                        let _ = runtime_dat::write(*last_popup);
                    }
                    *fired = scheduler::derive_already_fired(now, config, *last_popup);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(e) = maybe_fire(
                        config,
                        last_popup,
                        fired,
                        last_popup_child,
                        last_widget_children,
                        exe,
                    ) {
                        tracing::warn!(error = ?e, "tick maybe_fire failed");
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("command channel disconnected, stopping");
                    return Ok(());
                }
            }
        }
    }

    fn maybe_fire(
        config: &Config,
        last_popup: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        exe: &Path,
    ) -> Result<()> {
        let now = SystemTime::now();
        match scheduler::next_event(now, config, *last_popup, fired) {
            SchedulerDecision::Nothing => Ok(()),
            SchedulerDecision::FirePopup => fire_popup(exe, last_popup, fired, last_popup_child),
            SchedulerDecision::FireCountdown {
                palier_minutes,
                seconds_until_popup,
            } => fire_countdown(
                exe,
                palier_minutes,
                seconds_until_popup,
                fired,
                last_widget_children,
            ),
        }
    }

    fn fire_popup(
        exe: &Path,
        last_popup: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
    ) -> Result<()> {
        // Pre-spawn sweep : belt-and-braces. Startup already fires once ;
        // this catches workers leaked between our own popup lifetimes.
        if let Some(exe_parent) = exe.parent() {
            kill_orphan_webview2_workers(&webview2_dir(exe_parent));
        }
        // --no-debug so the service-spawned popup runs in production
        // mode : keyboard hook installed + force-minimize honoured.
        // Debug-default is only for developer manual invocations
        // from PowerShell during smoke.
        let args: [&OsStr; 2] = [OsStr::new("--popup"), OsStr::new("--no-debug")];
        // Spawn-first, kill-on-success. Predecessor left behind by an
        // Alt+Tab'd user (post-countdown unlock, 0.7.1 Track A) is
        // terminated only if the new spawn succeeded — better a stale
        // popup than none at all. HANDLE-based terminate is immune to
        // PID recycle.
        match spawn_replacing(&RealSpawner, exe, &args, last_popup_child) {
            Ok(child) => {
                tracing::info!(pid = child.pid, "popup child spawned");
                let now = SystemTime::now();
                *last_popup = now;
                fired.clear();
                if let Err(e) = runtime_dat::write(now) {
                    tracing::warn!(error = ?e, "write runtime.dat failed");
                }
                *last_popup_child = Some(child);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = ?e, "popup spawn failed - will retry on next tick");
                Err(e)
            }
        }
    }

    fn fire_countdown(
        exe: &Path,
        palier: u32,
        seconds_until_popup: u64,
        fired: &mut std::collections::HashSet<u32>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
    ) -> Result<()> {
        let seconds_str = seconds_until_popup.to_string();
        let palier_str = palier.to_string();
        // Same rationale as fire_popup : --no-debug forces the
        // service-spawned widget into production mode
        // (force-minimize per Config::force_minimize_paliers).
        let args: [&OsStr; 5] = [
            OsStr::new("--countdown"),
            OsStr::new(&seconds_str),
            OsStr::new("--palier"),
            OsStr::new(&palier_str),
            OsStr::new("--no-debug"),
        ];
        // Spawn-first, kill-on-success — same invariant as fire_popup.
        // Different-palier widgets remain untouched.
        match spawn_replacing_keyed(&RealSpawner, exe, &args, palier, last_widget_children) {
            Ok(()) => {
                tracing::info!(palier, "countdown child spawned");
                fired.insert(palier);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = ?e, palier, "countdown spawn failed");
                Err(e)
            }
        }
    }

    fn run_pipe(cancel: ipc::SendableHandle, tx: mpsc::Sender<TickCommand>) {
        let (msg_tx, msg_rx) = mpsc::channel::<Message>();
        let pipe_tx = tx.clone();
        // Bridge Message → TickCommand on the same thread. Spawn the
        // accept loop, then relay messages until the pipe closes.
        let accept_result = thread::spawn(move || ipc::run_server(cancel, msg_tx));
        while let Ok(msg) = msg_rx.recv() {
            let cmd = match msg {
                Message::TriggerPopupNow => TickCommand::Trigger,
                Message::Reload => TickCommand::Reload,
                Message::Shutdown => TickCommand::Shutdown,
            };
            let _ = pipe_tx.send(cmd);
            if matches!(msg, Message::Shutdown) {
                break;
            }
        }
        if let Ok(Err(e)) = accept_result.join() {
            tracing::warn!(error = ?e, "pipe accept loop exited with error");
        }
    }

    fn spawn_notify(
        tx: mpsc::Sender<TickCommand>,
        running: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            use notify::{RecommendedWatcher, RecursiveMode, Watcher};

            let (fs_tx, fs_rx) = mpsc::channel::<notify::Result<notify::Event>>();
            let mut watcher: RecommendedWatcher = match notify::recommended_watcher(fs_tx) {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!(error = ?e, "notify::recommended_watcher failed");
                    return;
                }
            };
            let watched_dir = Path::new(DIAGNOSTICS_CACHE_DIR);
            if let Err(e) = watcher.watch(watched_dir, RecursiveMode::NonRecursive) {
                tracing::error!(error = ?e, dir = %watched_dir.display(), "watcher.watch failed");
                return;
            }

            let target_name = Path::new(STATE_DAT).file_name();
            let mut pending = false;

            while running.load(Ordering::Relaxed) {
                match fs_rx.recv_timeout(DEBOUNCE) {
                    Ok(Ok(event)) => {
                        if is_relevant_event(&event, target_name) {
                            tracing::trace!(kind = ?event.kind, "state.dat event");
                            pending = true;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = ?e, "notify event error");
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if pending {
                            let _ = tx.send(TickCommand::Reload);
                            pending = false;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            drop(watcher);
        })
    }

    fn is_relevant_event(
        event: &notify::Event,
        target_name: Option<&std::ffi::OsStr>,
    ) -> bool {
        use notify::EventKind;
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return false;
        }
        let Some(target) = target_name else {
            return true;
        };
        event
            .paths
            .iter()
            .any(|p| p.file_name().map(|n| n == target).unwrap_or(false))
    }

    fn running() -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(0),
            process_id: None,
        }
    }

    fn stopped(exit_code: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(0),
            process_id: None,
        }
    }

}
