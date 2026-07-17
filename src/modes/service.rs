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
    use crate::platform::win32::{busy_detector, gamepad, idle, power_notify};
    use crate::platform::{spawn_replacing, spawn_replacing_keyed};
    use crate::runtime_dat;
    use crate::scheduler::{self, SchedulerDecision};
    use crate::signals;

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
        // Cold-start dispatch. No runtime.dat → fresh-install / migrated
        // 0.6.x path via resolve_last_popup (anti-cheat shift). Present
        // runtime.dat → active-time A/B/C via resolve_cold_start :
        //   A (elapsed >= 2 × interval)     → full reset
        //   B (popup_pending && downtime < 30 min) → relaunch immediately
        //   C (else)                        → absorb downtime into
        //                                    paused_secs, keep cycle
        let (mut last_popup, mut paused_secs, mut popup_pending, relaunch_now) =
            match runtime_dat::read() {
                None => {
                    let lp = scheduler::resolve_last_popup(
                        scheduler::ResolveInputs {
                            now,
                            last_popup: None,
                            first_install_at: config.first_install_at(),
                            is_reload: false,
                            paused_secs: 0,
                            popup_pending: false,
                            last_heartbeat: SystemTime::UNIX_EPOCH,
                        },
                        &config,
                    );
                    let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                        last_popup: lp,
                        paused_secs: 0,
                        popup_pending: false,
                        last_heartbeat: now,
                    });
                    tracing::info!(
                        path = "no-runtime-dat",
                        seeded_last_popup = ?lp,
                        "cold-start: fresh install or migrated 0.6.x"
                    );
                    (lp, 0u64, false, false)
                }
                Some(state) => {
                    let downtime_secs = now
                        .duration_since(state.last_heartbeat)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let d = scheduler::resolve_cold_start(
                        scheduler::ResolveInputs {
                            now,
                            last_popup: Some(state.last_popup),
                            first_install_at: config.first_install_at(),
                            is_reload: false,
                            paused_secs: state.paused_secs,
                            popup_pending: state.popup_pending,
                            last_heartbeat: state.last_heartbeat,
                        },
                        &config,
                    );
                    // Rule identification for bug-report clarity. Derived
                    // from the decision : Rule B is uniquely marked by
                    // relaunch_now = true ; Rule A resets last_popup to
                    // now AND clears both paused_secs and popup_pending ;
                    // everything else is Rule C (kept cycle, absorbed
                    // downtime).
                    let rule = if d.relaunch_now {
                        "B (relaunch pending popup, downtime < 30 min)"
                    } else if d.last_popup >= now
                        && d.paused_secs == 0
                        && !d.popup_pending
                    {
                        "A (elapsed >= 2 x interval, full reset)"
                    } else {
                        "C (keep cycle, absorb downtime into paused_secs)"
                    };
                    tracing::info!(
                        path = "with-runtime-dat",
                        rule,
                        downtime_secs,
                        prior_last_popup = ?state.last_popup,
                        prior_paused_secs = state.paused_secs,
                        prior_popup_pending = state.popup_pending,
                        new_last_popup = ?d.last_popup,
                        new_paused_secs = d.paused_secs,
                        new_popup_pending = d.popup_pending,
                        relaunch_now = d.relaunch_now,
                        "cold-start: A/B/C dispatch"
                    );
                    (d.last_popup, d.paused_secs, d.popup_pending, d.relaunch_now)
                }
            };
        let mut last_heartbeat = now;
        let mut fired = scheduler::derive_already_fired(now, &config, last_popup, paused_secs);
        tracing::info!(
            initial_paliers_fired = ?fired,
            paused_secs,
            popup_pending,
            relaunch_now,
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

        // Suspend / resume broadcast listener. Guard is held for the
        // lifetime of run_service ; Drop tears down the pump thread and
        // unregisters the notification.
        let (power_tx, power_rx) = mpsc::channel::<power_notify::PowerEvent>();
        let _power_guard =
            power_notify::install(power_tx).context("power_notify::install")?;

        // Track spawned children so we can TerminateProcess a stale
        // predecessor before spawning a replacement — the post-countdown
        // unlock (0.7.1 Track A) lets a user Alt+Tab away from a still-
        // running popup, so two popups would otherwise stack up on the
        // next schedule tick. Handle-based (not PID-based) so a recycled
        // PID cannot cause us to terminate an unrelated process.
        let mut last_popup_child: Option<SpawnedChild> = None;
        let mut last_widget_children: HashMap<u32, SpawnedChild> = HashMap::new();

        // Rule B : runtime.dat said popup_pending && downtime < 30 min
        // when we cold-booted. Fire the popup before entering the tick
        // loop so the user sees it as soon as the service is ready.
        if relaunch_now {
            tracing::info!("cold-start Rule B: relaunching pending popup");
            if let Err(e) = fire_popup(
                &exe,
                &mut last_popup,
                &mut paused_secs,
                &mut popup_pending,
                &mut last_heartbeat,
                &mut fired,
                &mut last_popup_child,
                &config.passcode_hash,
            ) {
                tracing::warn!(error = ?e, "Rule B relaunch failed - will retry on next tick");
            }
        }

        // Main tick loop.
        let loop_result = tick_loop(
            &mut config,
            &mut last_popup,
            &mut paused_secs,
            &mut popup_pending,
            &mut last_heartbeat,
            &mut fired,
            &mut last_popup_child,
            &mut last_widget_children,
            &exe,
            &rx,
            power_rx,
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

    #[allow(clippy::too_many_arguments)]
    fn tick_loop(
        config: &mut Config,
        last_popup: &mut SystemTime,
        paused_secs: &mut u64,
        popup_pending: &mut bool,
        last_heartbeat: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        exe: &Path,
        rx: &mpsc::Receiver<TickCommand>,
        power_rx: mpsc::Receiver<power_notify::PowerEvent>,
    ) -> Result<()> {
        // 60 s idle threshold. Toggled at runtime via
        // `config.count_active_time` — when false, the check below is
        // short-circuited to `is_idle_now = false` so the counter runs
        // on wall-clock time.
        const IDLE_THRESHOLD_SECS: u64 = 60;
        // Cap for the video / fullscreen / audio deferral. Beyond this
        // the popup fires regardless of busy state — the user has been
        // "busy" for 15 min already, we're going to interrupt anyway.
        const MAX_DEFERRAL_SECS: u64 = 15 * 60;
        // Heartbeat cadence : every 6th timeout tick (~30 s at 5 s
        // TICK_INTERVAL). Fire-path already persists eagerly ; this
        // covers pure-idle stretches so downtime detection at next
        // cold-boot has ≤30 s resolution (RESUME_WINDOW is 30 min, so
        // Rule B stays correct with ~60× headroom).
        const HEARTBEAT_EVERY_N_TICKS: u32 = 6;
        // Periodic idle-state log cadence : every 12th timeout tick
        // (~60 s at 5 s TICK_INTERVAL). Emits a `state snapshot` log
        // line with all signals + counters even when no active↔inactive
        // transition fired, so log-scraping can plot the paused_secs
        // curve minute-by-minute. Bounded — one line per minute is ~1440
        // lines/day, cheap compared to the 5-tick heartbeat.
        const STATE_LOG_EVERY_N_TICKS: u32 = 12;

        let tick_delta_secs = TICK_INTERVAL.as_secs();
        let mut deferred_secs: u64 = 0;
        let mut suspended_at: Option<SystemTime> = None;
        let mut ticks_since_write: u32 = 0;
        let mut ticks_since_state_log: u32 = 0;
        // `was_idle = None` means "never observed yet" — the first tick's
        // idle transition log will read as a seed. After that, every
        // active↔inactive flip emits a single log line with the three
        // component signals so bug reports show why the counter did or
        // didn't advance.
        let mut was_idle: Option<bool> = None;

        loop {
            match rx.recv_timeout(TICK_INTERVAL) {
                Ok(TickCommand::Shutdown) => {
                    tracing::info!("tick loop received Shutdown");
                    return Ok(());
                }
                Ok(TickCommand::Trigger) => {
                    tracing::info!("TriggerPopupNow received");
                    if let Err(e) = fire_popup(
                        exe,
                        last_popup,
                        paused_secs,
                        popup_pending,
                        last_heartbeat,
                        fired,
                        last_popup_child,
                        &config.passcode_hash,
                    ) {
                        tracing::warn!(error = ?e, "trigger popup failed");
                    }
                }
                Ok(TickCommand::Reload) => {
                    tracing::info!("config reload triggered");
                    let state_path = PathBuf::from(STATE_DAT);
                    *config = config::load_or_default(&state_path);
                    tracing::info!(
                        count_active_time = config.count_active_time,
                        "reload: active-time accounting effective mode"
                    );
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
                            paused_secs: *paused_secs,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
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
                        *last_heartbeat = now;
                        let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                            last_popup: *last_popup,
                            paused_secs: *paused_secs,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
                        });
                    }
                    *fired = scheduler::derive_already_fired(
                        now,
                        config,
                        *last_popup,
                        *paused_secs,
                    );
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // 1. Drain power events. Suspend timestamps are
                    //    captured wall-clock inside the pump thread's
                    //    wndproc, so the delta is accurate regardless
                    //    of receive latency.
                    while let Ok(evt) = power_rx.try_recv() {
                        match evt {
                            power_notify::PowerEvent::Suspend(ts) => {
                                suspended_at = Some(ts);
                            }
                            power_notify::PowerEvent::Resume(ts) => {
                                if let Some(sus) = suspended_at.take() {
                                    if let Ok(delta) = ts.duration_since(sus) {
                                        let delta_secs = delta.as_secs();
                                        *paused_secs += delta_secs;
                                        tracing::info!(
                                            sleep_secs = delta_secs,
                                            paused_secs_after = *paused_secs,
                                            "power resume: sleep duration absorbed into paused_secs"
                                        );
                                    }
                                } else {
                                    tracing::info!(
                                        "power resume without matching suspend (service was down at suspend, cold-start absorbed downtime)"
                                    );
                                }
                            }
                        }
                    }

                    // 2. Detect popup exit (Dismiss button OR the
                    //    countdown-then-close path both terminate the
                    //    popup process).
                    if *popup_pending {
                        if let Some(child) = last_popup_child.as_ref() {
                            if child.try_wait_exited() {
                                *popup_pending = false;
                                tracing::info!(
                                    paused_secs = *paused_secs,
                                    "popup child exited - clearing popup_pending"
                                );
                            }
                        }
                    }

                    // 3. Idle accumulation. Signals are read once and
                    //    reused by the fire-decision below.
                    let now = SystemTime::now();
                    let is_busy_now = busy_detector::is_busy();
                    let gamepad_seen = gamepad::gamepad_active_since_last_call();
                    let idle_secs = idle::idle_seconds();
                    let is_idle_now = if config.count_active_time {
                        signals::idle_tick_should_pause(
                            idle_secs,
                            IDLE_THRESHOLD_SECS,
                            gamepad_seen,
                            is_busy_now,
                        )
                    } else {
                        false
                    };
                    if is_idle_now {
                        *paused_secs += tick_delta_secs;
                    }
                    if was_idle != Some(is_idle_now) {
                        tracing::info!(
                            transition = if is_idle_now { "active -> inactive" } else { "inactive -> active" },
                            idle_secs,
                            gamepad_seen,
                            is_busy = is_busy_now,
                            paused_secs = *paused_secs,
                            "idle state changed"
                        );
                        was_idle = Some(is_idle_now);
                    }
                    ticks_since_state_log += 1;
                    if ticks_since_state_log >= STATE_LOG_EVERY_N_TICKS {
                        tracing::info!(
                            is_idle = is_idle_now,
                            idle_secs,
                            gamepad_seen,
                            is_busy = is_busy_now,
                            paused_secs = *paused_secs,
                            count_active_time = config.count_active_time,
                            "state snapshot"
                        );
                        ticks_since_state_log = 0;
                    }

                    // 4. Scheduler decision, with video / audio deferral
                    //    when the fire lands during a busy stretch.
                    match scheduler::next_event(
                        now,
                        config,
                        *last_popup,
                        *paused_secs,
                        fired,
                    ) {
                        SchedulerDecision::Nothing => {}
                        SchedulerDecision::FirePopup => {
                            // Fire-decision context. Elapsed_active is
                            // the metric the scheduler used to decide
                            // fire ; logging it alongside the interval
                            // makes "why did the popup fire now ?" a
                            // one-line answer in the service log.
                            let elapsed_wall_secs = now
                                .duration_since(*last_popup)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            let elapsed_active_secs =
                                elapsed_wall_secs.saturating_sub(*paused_secs);
                            let interval_secs =
                                scheduler::interval(config).as_secs();
                            let decision = signals::busy_defer_decision(
                                is_busy_now,
                                deferred_secs,
                                tick_delta_secs,
                                MAX_DEFERRAL_SECS,
                            );
                            match decision {
                                signals::DeferOutcome::Fire
                                | signals::DeferOutcome::ForceFire => {
                                    // Actual fire — log once with full
                                    // context so bug reports show why.
                                    tracing::info!(
                                        elapsed_wall_secs,
                                        paused_secs = *paused_secs,
                                        elapsed_active_secs,
                                        interval_secs,
                                        is_busy = is_busy_now,
                                        deferred_secs,
                                        verdict = if matches!(decision, signals::DeferOutcome::ForceFire) { "ForceFire (defer cap hit)" } else { "Fire" },
                                        "scheduler said FirePopup - firing"
                                    );
                                    if let Err(e) = fire_popup(
                                        exe,
                                        last_popup,
                                        paused_secs,
                                        popup_pending,
                                        last_heartbeat,
                                        fired,
                                        last_popup_child,
                                        &config.passcode_hash,
                                    ) {
                                        tracing::warn!(error = ?e, "tick fire_popup failed");
                                    } else {
                                        deferred_secs = 0;
                                    }
                                }
                                signals::DeferOutcome::Defer(next) => {
                                    // Log only the FIRST defer tick of
                                    // the stretch so long busy sessions
                                    // don't spam ~180 lines over 15 min.
                                    if deferred_secs == 0 {
                                        tracing::info!(
                                            elapsed_wall_secs,
                                            paused_secs = *paused_secs,
                                            elapsed_active_secs,
                                            interval_secs,
                                            "scheduler said FirePopup - deferring (busy)"
                                        );
                                    }
                                    deferred_secs = next;
                                }
                            }
                        }
                        SchedulerDecision::FireCountdown {
                            palier_minutes,
                            seconds_until_popup,
                        } => {
                            if let Err(e) = fire_countdown(
                                exe,
                                palier_minutes,
                                seconds_until_popup,
                                fired,
                                last_widget_children,
                            ) {
                                tracing::warn!(error = ?e, "tick fire_countdown failed");
                            }
                        }
                    }

                    // 5. Throttled heartbeat (~30 s cadence). The fire
                    //    path persists eagerly so we don't double-write
                    //    on the same tick.
                    *last_heartbeat = now;
                    ticks_since_write += 1;
                    if ticks_since_write >= HEARTBEAT_EVERY_N_TICKS {
                        ticks_since_write = 0;
                        if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
                            last_popup: *last_popup,
                            paused_secs: *paused_secs,
                            popup_pending: *popup_pending,
                            last_heartbeat: *last_heartbeat,
                        }) {
                            tracing::warn!(error = ?e, "heartbeat runtime.dat write failed");
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("command channel disconnected, stopping");
                    return Ok(());
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fire_popup(
        exe: &Path,
        last_popup: &mut SystemTime,
        paused_secs: &mut u64,
        popup_pending: &mut bool,
        last_heartbeat: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        passcode_hash: &str,
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
        //
        // --passcode-hash <phc> forwards the argon2id hash so the popup
        // can verify Alt+pincode unlock attempts. Empty hash (fresh
        // install pre-passcode-set) → arg omitted, popup falls back to
        // countdown-only. The PHC string is NOT the passcode itself
        // and is not recoverable to one, so argv exposure is acceptable.
        let args: Vec<&OsStr> = if passcode_hash.is_empty() {
            vec![OsStr::new("--popup"), OsStr::new("--no-debug")]
        } else {
            vec![
                OsStr::new("--popup"),
                OsStr::new("--no-debug"),
                OsStr::new("--passcode-hash"),
                OsStr::new(passcode_hash),
            ]
        };
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
                *paused_secs = 0;
                *popup_pending = true;
                *last_heartbeat = now;
                fired.clear();
                if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
                    last_popup: now,
                    paused_secs: 0,
                    popup_pending: true,
                    last_heartbeat: now,
                }) {
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
