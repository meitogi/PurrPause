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
    use windows::Win32::System::SystemInformation::GetTickCount64;
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
    /// Divergence between wall-clock delta and unbiased-interrupt-time
    /// delta across a tick that triggers a `last_popup` shift. Filters
    /// normal 5 s tick jitter and mpsc receiver latency (typically < 15
    /// s) from being interpreted as a jump.
    const CLOCK_JUMP_THRESHOLD: Duration = Duration::from_secs(60);
    const DEBOUNCE: Duration = Duration::from_millis(300);
    const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
    // Popup crash retry policy. 30/60/90 s backoff × 3 attempts caps
    // the retry storm at 3 min post-crash and gives the popup-side
    // WebView2 wipe path time to settle if the UserData folder was in
    // a stale state.
    const MAX_POPUP_RETRIES: u32 = 3;
    const RETRY_BACKOFF_STEP_SECS: u64 = 30;

    /// In-memory retry state armed when a popup exits with a non-zero
    /// code. Not persisted to `runtime.dat` : if the service dies
    /// mid-retry, the cold-start clean-shutdown gate clears
    /// `popup_pending` post-unclean-shutdown and the next natural cycle
    /// fires the popup cleanly. Cleared on exit-code=0 or on
    /// `attempts >= MAX_POPUP_RETRIES`.
    #[derive(Debug, Clone)]
    struct PopupRetryState {
        attempts: u32,
        next_retry_at: SystemTime,
    }

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

        // b.9 : the input-probe transport moved from a file
        // (<exe_dir>/Data/lastinput.dat) to the IPC pipe. Any pre-
        // existing file left by a b.8 install is now orphan bookkeeping
        // — remove it silently so upgrades leave no stale artefact on
        // disk. Idempotent, ENOENT is not an error.
        if let Some(exe_parent) = exe.parent() {
            let stale = exe_parent.join("Data").join("lastinput.dat");
            let _ = std::fs::remove_file(&stale);
        }

        let state_path = PathBuf::from(STATE_DAT);
        let mut config = config::load_or_default(&state_path);
        let now = SystemTime::now();
        // Cold-start dispatch. No runtime.dat → fresh-install / migrated
        // 0.6.x path via resolve_last_popup (anti-cheat shift). Present
        // runtime.dat → threshold / anti-bypass / relaunch / credit via
        // resolve_cold_start :
        //   threshold  (elapsed >= reset_threshold_hours) → full reset
        //   anti-bypass(pending && !clean, kid killed service) → full reset
        //   relaunch   (pending && clean, user cleanly rebooted PC mid-popup) → refire popup
        //   credit     (else) → keep cycle, credit downtime into paused_secs
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
                            was_clean_shutdown: false,
                        },
                        &config,
                    );
                    // Fresh install → last_clean_shutdown_at = UNIX_EPOCH
                    // ("we're running, next boot is unclean unless graceful
                    // shutdown says otherwise"). Same treatment as every
                    // other runtime write : only the shutdown path bumps
                    // this field to `now`.
                    let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                        last_popup: lp,
                        paused_secs: 0,
                        popup_pending: false,
                        last_heartbeat: now,
                        last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                        ..runtime_dat::RuntimeState::default()
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
                    // `was_clean_shutdown` : true when the previous
                    // graceful shutdown stamped last_clean_shutdown_at
                    // at or after the final heartbeat. Migrated formats
                    // populate the field with UNIX_EPOCH → always <
                    // heartbeat → treated as unclean (converges after
                    // one clean boot cycle). Fed into resolve_cold_start
                    // to discriminate the anti-bypass path (unclean +
                    // pending, kid killed service) from the relaunch
                    // path (clean + pending, user cleanly rebooted PC
                    // while a popup was displayed).
                    let was_clean_shutdown =
                        state.last_clean_shutdown_at >= state.last_heartbeat;
                    let d = scheduler::resolve_cold_start(
                        scheduler::ResolveInputs {
                            now,
                            last_popup: Some(state.last_popup),
                            first_install_at: config.first_install_at(),
                            is_reload: false,
                            paused_secs: state.paused_secs,
                            popup_pending: state.popup_pending,
                            last_heartbeat: state.last_heartbeat,
                            was_clean_shutdown,
                        },
                        &config,
                    );
                    // Rule identification for bug-report clarity.
                    // Discriminants :
                    //   - relaunch_now = true → relaunch branch
                    //     (clean + pending).
                    //   - full_reset shape + elapsed >= threshold →
                    //     threshold branch (short-circuits inside
                    //     resolve_cold_start).
                    //   - full_reset shape otherwise → anti-bypass
                    //     (unclean + pending).
                    //   - else → credit-downtime.
                    let full_reset =
                        d.last_popup >= now && d.paused_secs == 0 && !d.popup_pending;
                    let threshold_secs =
                        (f64::from(config.reset_threshold_hours) * 3600.0) as u64;
                    let elapsed_secs = now
                        .duration_since(state.last_popup)
                        .unwrap_or(Duration::ZERO)
                        .as_secs();
                    let rule = if d.relaunch_now {
                        "relaunch (popup_pending on clean shutdown, user rebooted mid-popup, refiring)"
                    } else if full_reset && elapsed_secs >= threshold_secs {
                        "threshold (elapsed >= reset_threshold_hours, full reset)"
                    } else if full_reset {
                        "anti-bypass (popup_pending on unclean shutdown, full reset)"
                    } else {
                        "credit (keep cycle, absorb downtime into paused_secs)"
                    };
                    tracing::info!(
                        path = "with-runtime-dat",
                        rule,
                        downtime_secs,
                        was_clean_shutdown,
                        prior_last_popup = ?state.last_popup,
                        prior_paused_secs = state.paused_secs,
                        prior_popup_pending = state.popup_pending,
                        new_last_popup = ?d.last_popup,
                        new_paused_secs = d.paused_secs,
                        new_popup_pending = d.popup_pending,
                        relaunch_now = d.relaunch_now,
                        "cold-start: threshold / anti-bypass / relaunch / credit dispatch"
                    );
                    // Immediately persist last_clean_shutdown_at = UNIX_EPOCH
                    // so any crash from here until the next graceful shutdown
                    // leaves the field zeroed. Uses the post-cold-start
                    // resolved values (not the raw file contents) so the
                    // next boot reads a coherent state.
                    let _ = runtime_dat::write(&runtime_dat::RuntimeState {
                        last_popup: d.last_popup,
                        paused_secs: d.paused_secs,
                        popup_pending: d.popup_pending,
                        last_heartbeat: now,
                        last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                        ..runtime_dat::RuntimeState::default()
                    });
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
        // Secondary channel dedicated to input-stamp pushes from the
        // pipe thread. Kept out of the main TickCommand mpsc so the
        // ~5 s stamp cadence (matching TICK_INTERVAL) cannot starve
        // the timeout branch that drives the actual tick logic — the
        // tick loop drains this at the start of each timeout tick.
        let (stamp_tx, stamp_rx) = mpsc::channel::<idle::InputStampState>();
        let pipe_thread = thread::spawn(move || run_pipe(sendable, ipc_tx_map, stamp_tx));

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

        // User-session input probe. Spawned once here so the pipe
        // client is ready before the first idle::idle_seconds() call
        // in the tick loop. `idle_seconds` returns 0 (assume active)
        // if no stamp has arrived yet, so the timing window is safe.
        // Respawned by the tick loop if it dies. See modes::input_probe
        // for the Session 0 rationale.
        let mut input_probe_child: Option<SpawnedChild> = None;
        match spawn_input_probe(&exe) {
            Ok(child) => {
                tracing::info!(pid = child.pid, "input-probe spawned");
                input_probe_child = Some(child);
            }
            Err(e) => {
                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                    tracing::info!(
                        error = %e,
                        "input-probe initial spawn deferred - user session not ready yet, will retry on next tick"
                    );
                } else {
                    tracing::warn!(
                        error = ?e,
                        "input-probe initial spawn failed - will retry on next tick"
                    );
                }
            }
        }

        // Relaunch path : runtime.dat said popup_pending && the
        // previous shutdown was clean. User rebooted the PC mid-popup ;
        // refire it now so they pick up where they left off. Anti-bypass
        // (unclean shutdown) does NOT reach this block — that path
        // returns relaunch_now = false and full-reset shape.
        //
        // On cold boot the user session usually isn't ready yet (~5 s
        // after service start), so this initial attempt often fails
        // with a session-not-ready error. `popup_pending` stays true
        // in that case and the tick loop's cold-start-retry branch
        // (step 2c) drives the actual refire once the OS finishes the
        // logon sequence.
        if relaunch_now {
            tracing::info!("cold-start relaunch: refiring popup for user rebooted mid-popup");
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
                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                    tracing::info!(
                        error = %e,
                        "cold-start relaunch deferred - user session not ready yet, tick loop will retry"
                    );
                } else {
                    tracing::warn!(error = ?e, "cold-start relaunch failed - tick loop will retry");
                }
            }
        }

        // Popup crash retry state (in-memory only). See PopupRetryState
        // docstring for why it isn't persisted.
        let mut popup_retry: Option<PopupRetryState> = None;

        // Main tick loop.
        let loop_result = tick_loop(
            &mut config,
            &mut last_popup,
            &mut paused_secs,
            &mut popup_pending,
            &mut popup_retry,
            &mut last_heartbeat,
            &mut fired,
            &mut last_popup_child,
            &mut last_widget_children,
            &mut input_probe_child,
            &exe,
            &rx,
            stamp_rx,
            power_rx,
        );

        // Signal shutdown.
        notify_running_flag.store(false, Ordering::Relaxed);
        unsafe { let _ = SetEvent(cancel_event); }

        // Terminate the input probe explicitly. Without this the old
        // probe would linger until user log-off and hold onto its pipe
        // client + process handle across the next service restart.
        // HANDLE-based terminate is immune to PID recycle.
        if let Some(probe) = input_probe_child.take() {
            probe.terminate();
        }

        // Graceful-shutdown marker : stamp `last_clean_shutdown_at`
        // with the current time so the next boot can distinguish this
        // clean SCM Stop from a force-kill. Only path in the codebase
        // that writes a non-UNIX_EPOCH value into this field. If write
        // fails, next boot treats it as unclean (safe default) — logged
        // but not fatal.
        let shutdown_now = SystemTime::now();
        if let Err(e) = runtime_dat::write(&runtime_dat::RuntimeState {
            last_popup,
            paused_secs,
            popup_pending,
            last_heartbeat: shutdown_now,
            last_clean_shutdown_at: shutdown_now,
            ..runtime_dat::RuntimeState::default()
        }) {
            tracing::warn!(
                error = ?e,
                "final clean-shutdown runtime.dat write failed - next boot will treat as unclean"
            );
        } else {
            tracing::info!(
                last_clean_shutdown_at = ?shutdown_now,
                "graceful shutdown : runtime.dat stamped clean"
            );
        }

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
        popup_retry: &mut Option<PopupRetryState>,
        last_heartbeat: &mut SystemTime,
        fired: &mut std::collections::HashSet<u32>,
        last_popup_child: &mut Option<SpawnedChild>,
        last_widget_children: &mut HashMap<u32, SpawnedChild>,
        input_probe_child: &mut Option<SpawnedChild>,
        exe: &Path,
        rx: &mpsc::Receiver<TickCommand>,
        stamp_rx: mpsc::Receiver<idle::InputStampState>,
        power_rx: mpsc::Receiver<power_notify::PowerEvent>,
    ) -> Result<()> {
        // 60 s idle threshold. Toggled at runtime via
        // `config.count_active_time` — when false, the check below is
        // short-circuited to `is_idle_now = false` so the counter runs
        // on wall-clock time.
        const IDLE_THRESHOLD_SECS: u64 = 60;
        // Heartbeat cadence : every 6th timeout tick (~30 s at 5 s
        // TICK_INTERVAL). Fire-path already persists eagerly ; this
        // covers pure-idle stretches so cold-boot downtime detection
        // resolves at ≤30 s granularity.
        const HEARTBEAT_EVERY_N_TICKS: u32 = 6;
        // Periodic idle-state log cadence : every 12th timeout tick
        // (~60 s at 5 s TICK_INTERVAL). Emits a `state snapshot` log
        // line with all signals + counters even when no active↔inactive
        // transition fired, so log-scraping can plot the paused_secs
        // curve minute-by-minute. Bounded — one line per minute is ~1440
        // lines/day, cheap compared to the 5-tick heartbeat.
        const STATE_LOG_EVERY_N_TICKS: u32 = 12;

        let tick_delta_secs = TICK_INTERVAL.as_secs();
        let mut suspended_at: Option<SystemTime> = None;
        let mut ticks_since_write: u32 = 0;
        let mut ticks_since_state_log: u32 = 0;
        // Anchors for the clock-shift detector : compare wall delta
        // against unbiased-interrupt-time delta each tick. When they
        // diverge ≥ CLOCK_JUMP_THRESHOLD we shift `last_popup` by the
        // divergence — neutralises VM freeze, S4 hibernate without
        // power broadcast, and admin/NTP clock tampering. `None` on
        // the first tick (no baseline to compare yet).
        let mut prev_wall: Option<SystemTime> = None;
        let mut prev_unbiased_secs: Option<u64> = None;
        // `was_idle = None` means "never observed yet" — the first tick's
        // idle transition log will read as a seed. After that, every
        // active↔inactive flip emits a single log line with the three
        // component signals so bug reports show why the counter did or
        // didn't advance.
        let mut was_idle: Option<bool> = None;
        // Latest input stamp materialized from the pipe transport. Held
        // between ticks so a probe that stops pushing decays into
        // staleness (`idle::idle_seconds` returns 0 after 30 s of no
        // new stamps) rather than snapping back to "assume active"
        // instantly.
        let mut input_stamp: Option<idle::InputStampState> = None;

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
                            was_clean_shutdown: false, // unused on reload path
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
                            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                            ..runtime_dat::RuntimeState::default()
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

                    // 1b. Clock-shift detector. Sample wall + unbiased
                    //     BEFORE the fire decision so a jump this tick
                    //     is compensated before `next_event` reads
                    //     `last_popup`. Skipped on the first tick (no
                    //     baseline yet).
                    let now_wall = SystemTime::now();
                    let now_unbiased_secs = crate::platform::win32::unbiased_time::ticks_to_secs(
                        crate::platform::win32::unbiased_time::query_100ns(),
                    );
                    if let (Some(pw), Some(pu)) = (prev_wall, prev_unbiased_secs) {
                        let unbiased_delta_secs = now_unbiased_secs.saturating_sub(pu);
                        if let Some(divergence) = scheduler::compute_clock_shift(
                            pw,
                            now_wall,
                            unbiased_delta_secs,
                            CLOCK_JUMP_THRESHOLD.as_secs(),
                        ) {
                            let wall_delta_secs = now_wall
                                .duration_since(pw)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or_else(|e| -(e.duration().as_secs() as i64));
                            let cause_hint = if divergence < 0 {
                                "clock_rewind"
                            } else if divergence >= 5 * 60 {
                                "suspend_or_hibernate"
                            } else {
                                "clock_advanced"
                            };
                            let last_popup_before = *last_popup;
                            *last_popup =
                                scheduler::shift_systemtime(*last_popup, divergence);
                            tracing::warn!(
                                divergence_secs = divergence,
                                wall_delta_secs,
                                unbiased_delta_secs,
                                cause_hint,
                                last_popup_before = ?last_popup_before,
                                last_popup_after = ?*last_popup,
                                "clock jump detected, last_popup shifted to preserve active-time"
                            );
                        }
                    }
                    prev_wall = Some(now_wall);
                    prev_unbiased_secs = Some(now_unbiased_secs);

                    // 2. Detect popup exit (Dismiss button OR the
                    //    countdown-then-close path both terminate the
                    //    popup process). The exit code discriminates a
                    //    normal dismiss (0) from a crash (≠ 0). Crash
                    //    arms a retry backoff instead of clearing the
                    //    cycle ; a following FirePopup attempt uses the
                    //    popup-side wipe-then-retry path in popup.rs to
                    //    recover from a stale WebView2 UserData folder.
                    if *popup_pending {
                        if let Some(child) = last_popup_child.as_ref() {
                            if let Some(exit_code) = child.try_wait_exited() {
                                if exit_code == 0 {
                                    *popup_pending = false;
                                    *popup_retry = None;
                                    tracing::info!(
                                        exit_code,
                                        paused_secs = *paused_secs,
                                        "popup child exited cleanly - clearing popup_pending"
                                    );
                                } else {
                                    let attempts_prior =
                                        popup_retry.as_ref().map(|r| r.attempts).unwrap_or(0);
                                    if attempts_prior >= MAX_POPUP_RETRIES {
                                        tracing::warn!(
                                            exit_code,
                                            attempts = attempts_prior,
                                            max_retries = MAX_POPUP_RETRIES,
                                            "popup gave up after N retries - clearing pending, next natural cycle"
                                        );
                                        *popup_pending = false;
                                        *popup_retry = None;
                                        fired.clear();
                                    } else {
                                        let attempts_next = attempts_prior + 1;
                                        let backoff_secs =
                                            RETRY_BACKOFF_STEP_SECS * (attempts_next as u64);
                                        let next_retry_at = SystemTime::now()
                                            + Duration::from_secs(backoff_secs);
                                        tracing::warn!(
                                            exit_code,
                                            attempts = attempts_next,
                                            max_retries = MAX_POPUP_RETRIES,
                                            backoff_secs,
                                            "popup exited abnormally - retry armed"
                                        );
                                        *popup_retry = Some(PopupRetryState {
                                            attempts: attempts_next,
                                            next_retry_at,
                                        });
                                        // Fire-set carries per-palier "already announced"
                                        // state ; clear it so the next attempted popup
                                        // re-considers the schedule cleanly.
                                        fired.clear();
                                    }
                                }
                            }
                        }
                    }

                    // 2a. If a retry is armed and its backoff has
                    //     elapsed, re-attempt fire_popup. Successful
                    //     spawn resets popup_pending/last_popup via the
                    //     fire path ; a subsequent crash re-arms with
                    //     the incremented attempt count until MAX.
                    if let Some(retry) = popup_retry.as_ref() {
                        if SystemTime::now() >= retry.next_retry_at {
                            tracing::info!(
                                attempts = retry.attempts,
                                "popup retry backoff elapsed - re-firing"
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
                                tracing::warn!(
                                    error = ?e,
                                    "popup retry fire_popup failed - keeping retry armed"
                                );
                            }
                        }
                    }

                    // 2b. Ensure the user-session input probe is alive.
                    //     Cheap poll (HANDLE-based try_wait_exited). If
                    //     dead, respawn — the probe writes the file
                    //     idle::idle_seconds() reads, so a stale/absent
                    //     probe would degrade kbd/mouse detection.
                    let probe_alive = input_probe_child
                        .as_ref()
                        .map(|c| c.try_wait_exited().is_none())
                        .unwrap_or(false);
                    if !probe_alive {
                        if input_probe_child.is_some() {
                            tracing::warn!("input-probe child exited - respawning");
                        }
                        match spawn_input_probe(exe) {
                            Ok(child) => {
                                tracing::info!(pid = child.pid, "input-probe respawned");
                                *input_probe_child = Some(child);
                            }
                            Err(e) => {
                                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                                    tracing::info!(
                                        error = %e,
                                        "input-probe respawn deferred - user session not ready, will retry on next tick"
                                    );
                                } else {
                                    tracing::warn!(
                                        error = ?e,
                                        "input-probe respawn failed - will retry on next tick"
                                    );
                                }
                            }
                        }
                    }

                    // 2c. Cold-start relaunch retry. `popup_pending`
                    //     came out of runtime.dat set to `true` and the
                    //     resolve_cold_start "relaunch" branch fired
                    //     (see the pre-loop attempt), but the initial
                    //     spawn either wasn't attempted (relaunch_now
                    //     was false because the pre-loop already
                    //     handled it) or failed because the user
                    //     session wasn't ready. `last_popup_child.is_none()`
                    //     uniquely identifies "never spawned" here
                    //     (crash-retry path always leaves the exited
                    //     child in `last_popup_child`). Retry each tick
                    //     until spawn succeeds.
                    if *popup_pending && last_popup_child.is_none() {
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
                            if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                                tracing::info!(
                                    error = %e,
                                    "cold-start relaunch retry deferred - user session not ready"
                                );
                            } else {
                                tracing::warn!(
                                    error = ?e,
                                    "cold-start relaunch retry failed - will retry on next tick"
                                );
                            }
                        }
                    }

                    // 3. Idle accumulation. Signals are read once and
                    //    reused by the fire-decision below.
                    let now = SystemTime::now();
                    let playing_video_or_audio_now = busy_detector::playing_video_or_audio();
                    let gamepad_seen = gamepad::gamepad_active_since_last_call();
                    // Drain any pending input-stamp pushes from the
                    // pipe thread ; keep the newest (stamps are
                    // monotonic). Held state carries across ticks
                    // when the drain sees nothing.
                    while let Ok(stamp) = stamp_rx.try_recv() {
                        input_stamp = Some(stamp);
                    }
                    let idle_secs = idle::idle_seconds(input_stamp.as_ref());
                    let is_idle_now = if config.count_active_time {
                        signals::idle_tick_should_pause(
                            idle_secs,
                            IDLE_THRESHOLD_SECS,
                            gamepad_seen,
                            playing_video_or_audio_now,
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
                            playing_video_or_audio = playing_video_or_audio_now,
                            paused_secs = *paused_secs,
                            "idle state changed"
                        );
                        was_idle = Some(is_idle_now);
                    }
                    ticks_since_state_log += 1;
                    if ticks_since_state_log >= STATE_LOG_EVERY_N_TICKS {
                        let next = scheduler::next_popup_at(*last_popup, config, *paused_secs);
                        let remaining_secs =
                            next.duration_since(now).unwrap_or(Duration::ZERO).as_secs();
                        // Categorical reason so the log reader doesn't
                        // have to correlate the raw booleans by eye.
                        // Priority order : the most user-driven signal
                        // wins (kbd/mouse > gamepad > media).
                        let active_reason = if !config.count_active_time {
                            "wall_clock_mode"
                        } else if is_idle_now {
                            "idle"
                        } else if idle_secs < IDLE_THRESHOLD_SECS {
                            "kbd_or_mouse"
                        } else if gamepad_seen {
                            "gamepad"
                        } else if playing_video_or_audio_now {
                            "playing_video_or_audio"
                        } else {
                            "unknown"
                        };
                        tracing::info!(
                            is_idle = is_idle_now,
                            active_reason,
                            idle_secs,
                            gamepad_seen,
                            playing_video_or_audio = playing_video_or_audio_now,
                            paused_secs = *paused_secs,
                            remaining_secs,
                            count_active_time = config.count_active_time,
                            "state snapshot"
                        );
                        ticks_since_state_log = 0;
                    }

                    // 4. Scheduler decision. Simplified in b.7 — popups
                    //    fire the moment the active-time echelon is hit,
                    //    with no busy-context deferral. Alt+pincode on
                    //    the popup is the intended escape hatch for the
                    //    rare "I'm mid-presentation, don't interrupt"
                    //    case.
                    // Session-1 placeholder pass : the three new
                    // args (popup_pending, daily_pause_state,
                    // now_local) preserve the pre-daily-pause
                    // behavior. Session 2 wires the real values via
                    // `time_authority::in_configured_tz` + runtime.dat
                    // state, at which point the daily-pause branch
                    // starts firing.
                    let now_local_placeholder = {
                        let utc: chrono::DateTime<chrono::Utc> = now.into();
                        utc.with_timezone(&chrono_tz::Europe::Paris)
                    };
                    match scheduler::next_event(
                        now,
                        config,
                        *last_popup,
                        *paused_secs,
                        fired,
                        false,
                        &scheduler::DailyPauseState::default(),
                        now_local_placeholder,
                    ) {
                        SchedulerDecision::Nothing => {}
                        SchedulerDecision::FirePopup => {
                            let elapsed_wall_secs = now
                                .duration_since(*last_popup)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            let elapsed_active_secs =
                                elapsed_wall_secs.saturating_sub(*paused_secs);
                            let interval_secs =
                                scheduler::interval(config).as_secs();
                            tracing::info!(
                                elapsed_wall_secs,
                                paused_secs = *paused_secs,
                                elapsed_active_secs,
                                interval_secs,
                                playing_video_or_audio = playing_video_or_audio_now,
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
                        // Session-1 placeholder : the daily-pause
                        // scheduler branch is dormant because the
                        // DailyPauseState placeholder has enabled=false.
                        // Session 2 replaces these arms with
                        // fire_popup_daily / fire_countdown_daily.
                        SchedulerDecision::FireDailyPause => {}
                        SchedulerDecision::FireDailyCountdown { .. } => {}
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
                            last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                            ..runtime_dat::RuntimeState::default()
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
                    last_clean_shutdown_at: SystemTime::UNIX_EPOCH,
                    ..runtime_dat::RuntimeState::default()
                }) {
                    tracing::warn!(error = ?e, "write runtime.dat failed");
                }
                *last_popup_child = Some(child);
                Ok(())
            }
            Err(e) => {
                if crate::platform::win32::spawn_user::is_user_session_not_ready(&e) {
                    tracing::info!(
                        error = %e,
                        "popup spawn deferred - user session not ready, caller will retry"
                    );
                } else {
                    tracing::warn!(error = ?e, "popup spawn failed - will retry on next tick");
                }
                Err(e)
            }
        }
    }

    /// Spawn the user-session input probe (`--input-probe`). The
    /// probe lives for the service's lifetime ; if it dies (crash,
    /// user log-off, session change), the tick loop's respawn poll
    /// catches it within one tick (~5 s). No spawn-replacing pattern
    /// needed — the caller holds at most one handle at a time.
    fn spawn_input_probe(exe: &Path) -> Result<SpawnedChild> {
        use crate::platform::Spawner;
        RealSpawner.spawn(exe, &[OsStr::new("--input-probe")])
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

    fn run_pipe(
        cancel: ipc::SendableHandle,
        tx: mpsc::Sender<TickCommand>,
        stamp_tx: mpsc::Sender<idle::InputStampState>,
    ) {
        let (msg_tx, msg_rx) = mpsc::channel::<Message>();
        let pipe_tx = tx.clone();
        // Bridge Message → (TickCommand OR stamp) on the same thread.
        // Spawn the accept loop, then relay messages until the pipe
        // closes. `InputStamp` bypasses the TickCommand mpsc and goes
        // straight to a dedicated stamp channel — see the run_service
        // comment on the stamp channel for the starvation rationale.
        let accept_result = thread::spawn(move || ipc::run_server(cancel, msg_tx));
        while let Ok(msg) = msg_rx.recv() {
            match msg {
                Message::TriggerPopupNow => {
                    let _ = pipe_tx.send(TickCommand::Trigger);
                }
                Message::Reload => {
                    let _ = pipe_tx.send(TickCommand::Reload);
                }
                Message::Shutdown => {
                    let _ = pipe_tx.send(TickCommand::Shutdown);
                    break;
                }
                Message::InputStamp { tick_ms } => {
                    let received_at = unsafe { GetTickCount64() };
                    let _ = stamp_tx.send(idle::InputStampState {
                        last_input_tick_ms: tick_ms,
                        received_at_tick_ms: received_at,
                    });
                }
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
