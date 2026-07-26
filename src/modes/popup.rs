// Fullscreen transparent break-reminder popup — tao window + wry WebView.
//
// Two pure helpers (animation_pick, render_html) live outside the
// #[cfg(windows)] gate so they run under Linux `cargo test`. The event
// loop, keyboard hook and window styling are Windows-only.
//
// Resources (popup.html, vendored dotlottie-wc, animations) are served
// via a wry custom protocol (`purrpause://`) rather than file:// so that
// the popup.html's ES module imports and WASM fetches resolve against a
// same-origin scheme — WebView2 restricts several APIs on file:// origins.

// Most of this module's surface is reached only through #[cfg(windows)]
// callers, so it looks dead on the Linux dev build. Enforcement stays on
// for the shipping Windows target.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::config::{AnimationEntry, Config, Renderer, RotationMode};

const FALLBACK_ANIM_PATH: &str = "animations/dance-cat.lottie";

/// Maps an `AnimationEntry.file` value to the URL path the popup's
/// `dotlottie-wc` element fetches.
///
/// - `embed:<id>` → the matching embedded asset path (served from
///   memory by the wry custom protocol handler).
/// - Any other value → passed through unchanged (user-added file
///   under `<exe_dir>/resources/animations/`).
///
/// Kept pure + Linux-testable ; the runtime lookup lives in the
/// custom protocol handler.
pub fn resolve_animation_url(file: &str) -> String {
    match file.strip_prefix("embed:") {
        Some("dance-cat") => "animations/dance-cat.lottie".to_string(),
        Some("sleep-cat") => "animations/cat-sleeping-no-bg.lottie".to_string(),
        Some(other) => {
            // Unknown embed id → try a naming heuristic before giving
            // up. `embed:foo` → `animations/foo.lottie` matches how
            // future embedded assets would be named.
            format!("animations/{other}.lottie")
        }
        None => file.to_string(),
    }
}

/// Pure animation picker.
///
/// Filters `cfg.animations` to enabled entries and returns the pick plus
/// the next rotation index (Sequential mode) or `0` (Random mode, unused
/// by the caller). Returns `None` when no anim is enabled — caller uses
/// the bundled fallback path.
pub fn animation_pick(
    cfg: &Config,
    rotation_state: Option<u32>,
) -> Option<(AnimationEntry, u32)> {
    let enabled: Vec<&AnimationEntry> =
        cfg.animations.iter().filter(|a| a.enabled).collect();
    if enabled.is_empty() {
        return None;
    }
    let len = enabled.len();
    match cfg.rotation_mode {
        RotationMode::Random => {
            use rand::Rng;
            let idx = rand::rng().random_range(0..len);
            Some((enabled[idx].clone(), 0))
        }
        RotationMode::Sequential => {
            let cursor = rotation_state.unwrap_or(0) as usize;
            let idx = cursor % len;
            let next = (cursor as u32).wrapping_add(1);
            Some((enabled[idx].clone(), next))
        }
    }
}

/// Pure HTML template renderer. Applies a deterministic sequence of
/// `str::replace` calls, one per `{{ PLACEHOLDER }}` token.
///
/// **No HTML-escaping.** Config strings originate from the passcode-gated
/// config UI on the same machine — the threat model does not include
/// HTML injection. If a future user-supplied `countdown_template`
/// contains a `"` char it may break the enclosing HTML attribute quote ;
/// session 6's config UI should reject that at input time.
#[allow(clippy::too_many_arguments)]
pub fn render_html(
    template: &str,
    cfg: &Config,
    animation_path: &str,
    countdown_seconds: u32,
    preview: bool,
    debug: bool,
    devtools: bool,
    stage_scale: f32,
    stage_offset_y_vh: i8,
    renderer: Renderer,
) -> String {
    let mode = if preview { "preview" } else { "live" };
    let debug_mode = if debug { "true" } else { "false" };
    // Lowercase token the popup mount script reads off `data-renderer` to
    // pick mountLottie (ThorVG) vs mountSkottie (GPU) for a `.lottie`.
    let renderer_token = match renderer {
        Renderer::ThorvgSw => "thorvg",
        Renderer::SkottieGpu => "skottie",
    };
    // "1" when the popup self-diagnoses (WebGL renderer + conditional-import
    // proof over window.ipc) and keeps the context menu; "0" on a real
    // scheduled popup, which suppresses right-click instead.
    let devtools_flag = if devtools { "1" } else { "0" };
    // WebView2 propagates the HTML <title> to the parent HWND's OS
    // title, so if we only bumped the tao WindowBuilder title the
    // "(preview)" suffix would get overwritten seconds later. Apply
    // the suffix here too so both stay in sync. Skip the suffix when
    // the base title is empty — " (preview)" with a leading space
    // looks broken in Alt-Tab.
    let window_title = if preview && !cfg.popup_window_title.is_empty() {
        format!("{} (preview)", cfg.popup_window_title)
    } else {
        cfg.popup_window_title.clone()
    };
    template
        .replace("{{ WINDOW_TITLE }}", &window_title)
        .replace("{{ POPUP_TITLE }}", &cfg.popup_title)
        .replace("{{ POPUP_SUBTITLE }}", &cfg.popup_subtitle)
        .replace("{{ DISMISS_LABEL }}", &cfg.dismiss_button_label)
        .replace("{{ COUNTDOWN_TEMPLATE }}", &cfg.countdown_template)
        .replace("{{ ANIMATION_PATH }}", animation_path)
        .replace("{{ COUNTDOWN_SECONDS }}", &countdown_seconds.to_string())
        .replace("{{ POPUP_MODE }}", mode)
        .replace("{{ DEBUG_MODE }}", debug_mode)
        .replace("{{ DEVTOOLS }}", devtools_flag)
        .replace("{{ STAGE_SCALE }}", &format!("{stage_scale:.2}"))
        .replace("{{ STAGE_OFFSET_Y_VH }}", &stage_offset_y_vh.to_string())
        .replace("{{ RENDERER }}", renderer_token)
}

/// Resolve `(scale, offset_y_vh)` for the animation at `anim_file`. If the
/// config has an `AnimationEntry` for that file, its per-entry values win.
/// Otherwise fall back to `(1.0, 0)` — the CSS defaults. The Lottie engine is
/// a global choice (`Config::lottie_gpu`), not a per-entry one.
pub fn stage_params(cfg: &Config, anim_file: &str) -> (f32, i8) {
    cfg.animations
        .iter()
        .find(|e| e.file == anim_file)
        .map(|e| (e.scale, e.offset_y_vh))
        .unwrap_or((1.0, 0))
}

#[cfg(windows)]
pub fn run(
    preview: bool,
    debug: bool,
    anim_override: Option<String>,
    config_override: Option<String>,
    test_countdown_secs: Option<u32>,
    passcode_hash: Option<String>,
    mode: Option<String>,
    elapsed_seconds: Option<u32>,
    bench_secs: Option<u32>,
) -> anyhow::Result<()> {
    windows_impl::run(preview, debug, anim_override, config_override, test_countdown_secs, passcode_hash, mode, elapsed_seconds, bench_secs)
}

#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
pub fn run(
    _preview: bool,
    _debug: bool,
    _anim_override: Option<String>,
    _config_override: Option<String>,
    _test_countdown_secs: Option<u32>,
    _passcode_hash: Option<String>,
    _mode: Option<String>,
    _elapsed_seconds: Option<u32>,
    _bench_secs: Option<u32>,
) -> anyhow::Result<()> {
    anyhow::bail!("popup mode is Windows-only")
}

#[cfg(windows)]
mod windows_impl {
    use std::borrow::Cow;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use anyhow::{Context, Result};

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    // WindowExtWindows / WindowBuilderExtWindows are imported inside
    // the run() body where the shadow-dance uses them.
    use tao::window::{Fullscreen, WindowBuilder};

    use windows::Win32::Foundation::HWND;

    use crate::modes::install::{DIAGNOSTICS_CACHE_DIR, STATE_DAT};
    use crate::platform::win32::{
        fullscreen_detect, keyboard_hook, proc_kill::kill_orphan_webview2_workers,
        window_style,
    };

    use super::{
        animation_pick, render_html, resolve_animation_url, stage_params, Renderer,
        FALLBACK_ANIM_PATH,
    };

    const ROTATION_DAT_FILENAME: &str = "rotation.dat";

    #[derive(Debug, Clone)]
    enum UserEvent {
        Dismiss,
        CountdownExpired,
        /// Cold-start focus insurance : re-assert foreground + focus so
        /// the popup grabs keyboard input once the session goes
        /// interactive after a reboot/logon (the spawn-time
        /// force_foreground can land too early to stick).
        ReassertForeground,
        /// One accepted digit — WebView should render one more asterisk.
        /// Payload is the 0-based index of the newly-added asterisk.
        PincodeDigitAccepted(u32),
        /// Full-length buffer at Alt-release — verify against the
        /// argon2id hash held in the popup's run scope.
        PincodeSubmit(String),
        /// Buffer cleared (short at Alt-release, non-digit while Alt
        /// held) — WebView should hide the asterisk display.
        PincodeCancel,
        /// 3 wrong pincodes reached — WebView shows the locked visual,
        /// hook silently absorbs further attempts for the rest of this
        /// popup's lifetime.
        PincodeVerdictLocked,
        /// Service (or the `--popup-ipc` debug CLI) mutated this popup's
        /// content over the per-PID pipe : daily↔interval collision
        /// resolution without a respawn. Swaps the visible texts and
        /// restarts the dismiss countdown with the new duration.
        IpcUpdateContent {
            title: String,
            subtitle: String,
            dismiss_label: String,
            duration_secs: u64,
            mode: crate::ipc_popup::PopupContentMode,
        },
        /// Remote dismiss over the pipe. Exits through the same clean
        /// path as [`UserEvent::Dismiss`] (exit code 0).
        IpcDismiss,
        /// Restart only the visible countdown / unlock timer.
        /// `palier_minutes` is informational (logged).
        IpcRestartCountdown { palier_minutes: u32, seconds_until: u64 },
        /// Bench sampling window elapsed (or the sampler failed) —
        /// clean-exit the popup through the same path as Dismiss.
        BenchDone,
    }

    /// Install the LL keyboard hook (pincode-capturing variant when a
    /// hash is present). Skipped in debug mode. Extracted from the run()
    /// body so the IPC re-lock path (a daily-pause UpdateContent landing
    /// on an already-unlocked popup) can re-install the exact same hook ;
    /// both call sites run on the tao main thread, satisfying the
    /// SetWindowsHookEx same-thread rule.
    fn install_popup_keyboard_hook(
        debug: bool,
        passcode_hash: Option<&str>,
        cap: usize,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Option<keyboard_hook::KeyboardHookGuard>> {
        if debug {
            tracing::info!("popup: skipping keyboard hook (--debug)");
            return Ok(None);
        }
        let hash_present = passcode_hash.map(|h| !h.is_empty()).unwrap_or(false);
        let guard = if hash_present && cap > 0 {
            let g = keyboard_hook::install_keyboard_hook_with_pincode(
                move |ev| {
                    let ue = match ev {
                        keyboard_hook::PincodeEvent::Digit(i) => {
                            UserEvent::PincodeDigitAccepted(i)
                        }
                        keyboard_hook::PincodeEvent::Submit(s) => {
                            UserEvent::PincodeSubmit(s)
                        }
                        keyboard_hook::PincodeEvent::Cancel => UserEvent::PincodeCancel,
                    };
                    let _ = proxy.send_event(ue);
                },
                cap,
            )
            .context("install keyboard hook (with pincode)")?;
            tracing::info!(cap, "popup: keyboard hook installed with pincode capture");
            g
        } else {
            let g = keyboard_hook::install_keyboard_hook()
                .context("install keyboard hook")?;
            tracing::info!(
                "popup: keyboard hook installed (no pincode - passcode_hash empty)"
            );
            g
        };
        Ok(Some(guard))
    }

    /// Seconds since the unix epoch, wall-clock. Basis for the resettable
    /// countdown deadline shared with the timer thread.
    fn epoch_secs_now() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
    }

    /// Bench-mode sampling loop (background thread). Appends one TSV
    /// row per live tree process per 500 ms tick, plus a `role=total`
    /// aggregate row, to `<exe_dir>\Data\Logs\bench.log` — the same dir
    /// as widget.log, proven writable by the user-session popup
    /// (DiagnosticsCache is SYSTEM/admin-write only). Rows are flushed
    /// every tick : tao's `event_loop.run` diverges and terminates via
    /// process::exit, so nothing buffered would survive the exit.
    ///
    /// Tick 1 is a baseline (cpu columns 0 — no previous CPU totals) ;
    /// analysis discards the first row group.
    fn run_bench_sampler(
        secs: u32,
        anim_file: &str,
        anim_ext: &str,
        renderer: crate::config::Renderer,
    ) -> Result<()> {
        use std::io::Write;

        let exe = std::env::current_exe().context("current_exe")?;
        let exe_dir = exe.parent().context("exe has no parent dir")?;
        let log_dir = crate::modes::install::logs_dir(exe_dir);
        std::fs::create_dir_all(&log_dir).context("create Data/Logs")?;
        let log_path = log_dir.join("bench.log");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log_path)
            .with_context(|| format!("open {}", log_path.display()))?;
        if file.metadata().map(|m| m.len() == 0).unwrap_or(false) {
            writeln!(file, "{}", crate::bench::TSV_HEADER)?;
        }

        let mut sampler = crate::platform::win32::proc_sample::ProcSampler::new(std::process::id())
            .context("ProcSampler::new")?;
        tracing::info!(secs, log = %log_path.display(), "bench: sampling started");

        // secs == 0 : unlimited window — sample until the popup process
        // exits (the sampler thread dies with it).
        let ticks = if secs == 0 {
            u64::MAX
        } else {
            (secs as u64).saturating_mul(1000) / crate::bench::BENCH_TICK_MS
        };
        let mut last_tick = std::time::Instant::now();
        for _ in 0..ticks {
            std::thread::sleep(Duration::from_millis(crate::bench::BENCH_TICK_MS));
            // Measured interval, not the nominal 500 ms — cpu_pct must
            // survive scheduler jitter / a busy machine.
            let interval = last_tick.elapsed();
            last_tick = std::time::Instant::now();

            let rows = sampler.sample(interval).context("sample tick")?;
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f").to_string();
            let mut total = crate::bench::BenchRow {
                pid: std::process::id(),
                role: "total".to_string(),
                cpu_ms: 0,
                cpu_pct: 0.0,
                gpu_pct: 0.0,
                gpu_mem_bytes: 0,
                workingset_bytes: 0,
                private_bytes: 0,
            };
            for row in &rows {
                total.cpu_ms += row.cpu_ms;
                total.cpu_pct += row.cpu_pct;
                total.gpu_pct += row.gpu_pct;
                total.gpu_mem_bytes += row.gpu_mem_bytes;
                total.workingset_bytes += row.workingset_bytes;
                total.private_bytes += row.private_bytes;
                writeln!(file, "{}", crate::bench::tsv_row(&ts, anim_file, anim_ext, crate::bench::renderer_label(anim_ext, renderer), row))?;
            }
            writeln!(file, "{}", crate::bench::tsv_row(&ts, anim_file, anim_ext, crate::bench::renderer_label(anim_ext, renderer), &total))?;
            file.flush()?;
        }
        tracing::info!("bench: sampling window complete");
        Ok(())
    }

    pub fn run(
        preview: bool,
        debug: bool,
        anim_override: Option<String>,
        config_override: Option<String>,
        test_countdown_secs: Option<u32>,
        passcode_hash: Option<String>,
        mode: Option<String>,
        elapsed_seconds: Option<u32>,
        bench_secs: Option<u32>,
    ) -> Result<()> {
        // Preview / test spawns from the config UI pass a per-PID
        // temp state file so uncommitted edits show up without an
        // autosave. Regular service-spawned popups leave
        // config_override None → read state.dat.
        let cfg_loaded = match config_override.as_ref() {
            Some(path) => {
                let cfg = crate::config::load_or_default(Path::new(path));
                // Best-effort cleanup so the temp file doesn't linger.
                let _ = std::fs::remove_file(path);
                cfg
            }
            None => crate::config::load_or_default(Path::new(STATE_DAT)),
        };

        // Daily-pause mode swaps three cfg surfaces at the top so
        // the rest of run() (render_html, countdown calc, window title)
        // reads them uniformly. Keeps the diff surgical — no branching
        // downstream. Interval mode = default = cfg unchanged.
        let is_daily = mode.as_deref() == Some("daily-pause");
        let cfg = if is_daily {
            tracing::info!("popup: --mode daily-pause active");
            let mut c = cfg_loaded;
            c.popup_title = c.daily_pause.popup_title.clone();
            c.popup_subtitle = c.daily_pause.popup_subtitle.clone();
            c.dismiss_button_label = c.daily_pause.dismiss_button_label.clone();
            c.duration_minutes = c.daily_pause.duration_minutes;
            c
        } else {
            cfg_loaded
        };

        // Popup uses palier 0 for the force-minimize opt-in. Skipped
        // in preview mode (parent testing from their desktop) AND in
        // debug mode (developer wants to keep their current window).
        if !preview && !debug && cfg.force_minimize_paliers.contains(&0) {
            if let Err(e) = fullscreen_detect::force_minimize_foreground_fullscreen() {
                tracing::warn!(error = ?e, "force-minimize check failed");
            }
        }
        if debug {
            tracing::info!("popup: --debug mode (no keyboard hook, no force-minimize)");
        }

        // --anim <path> overrides the picker — used by the config UI's
        // per-animation "Tester" button. Falls back to the normal picker
        // when absent.
        let anim_path = if let Some(override_path) = anim_override.as_ref() {
            tracing::info!(anim = %override_path, "popup: --anim override active");
            override_path.clone()
        } else {
            // Sequential mode reads the rotation cursor from disk ; Random ignores it.
            let rotation_in = match cfg.rotation_mode {
                crate::config::RotationMode::Sequential => read_rotation_dat().ok(),
                crate::config::RotationMode::Random => None,
            };

            let (picked_path, rotation_out) = match animation_pick(&cfg, rotation_in) {
                Some((entry, next)) => (entry.file, Some(next)),
                None => (FALLBACK_ANIM_PATH.to_string(), None),
            };

            if matches!(cfg.rotation_mode, crate::config::RotationMode::Sequential) {
                if let Some(next) = rotation_out {
                    if let Err(e) = write_rotation_dat(next) {
                        tracing::warn!(error = %e, "failed to persist rotation state");
                    }
                }
            }
            picked_path
        };

        // Robustness : a configured animation file can go missing — the
        // Data\Animations folder was wiped, or the exe was launched from a
        // fresh install dir that never received the user's imports. Rather
        // than a blank stage (fetch 404), fall back to the embedded default
        // so a cat always shows. Applied BEFORE stage_params/resolve so the
        // whole pipeline (mount dispatch, renderer, framing) stays consistent.
        let anim_path = if crate::embedded::has_asset(&resolve_animation_url(&anim_path)) {
            anim_path
        } else {
            tracing::warn!(selected = %anim_path, fallback = %FALLBACK_ANIM_PATH, "animation file not found, falling back to embedded default");
            FALLBACK_ANIM_PATH.to_string()
        };

        // Resolve per-anim scale + offset. Present in the AnimationEntry list
        // → use those. Absent (e.g. fallback anim or a testing-only override
        // that isn't in the config) → CSS defaults (1.0, 0).
        let (scale, offset_y_vh) = stage_params(&cfg, &anim_path);
        // The Lottie engine is global: one `lottie_gpu` toggle flips every
        // `.lottie` between Skottie (GPU, default) and ThorVG (software).
        // Non-Lottie formats ignore this — they dispatch by extension.
        let renderer = if cfg.lottie_gpu {
            Renderer::SkottieGpu
        } else {
            Renderer::ThorvgSw
        };

        // `anim_path` may be an opaque `embed:*` identifier (default
        // shipped assets) OR a real relative path (user-added). Map
        // to the URL the WebView2 renderer will fetch — the custom
        // protocol handler resolves both against embedded bytes then
        // disk override.
        let anim_url = resolve_animation_url(&anim_path);

        // Countdown precedence :
        //   1. --test-countdown-secs <N> — parent forces a short one
        //      for full-prod smoke (keyboard hook + force-minimize
        //      remain active). Wins over both preview and cfg.
        //   2. --preview — 10 s + JS unlocks dismiss immediately via
        //      data-mode="preview".
        //   3. Otherwise → cfg.duration_minutes × 60.
        let countdown = if let Some(secs) = test_countdown_secs {
            secs
        } else if preview {
            10
        } else {
            cfg.duration_minutes.saturating_mul(60)
        };

        // popup.html : disk override wins over embedded so a parent can
        // hot-patch the layout without a rebuild, but the shipped bytes
        // are the default and the exe is single-file by design.
        let template_bytes = crate::embedded::get_asset("popup.html")
            .ok_or_else(|| anyhow::anyhow!("popup.html not embedded (build.rs regression?)"))?;
        let template = String::from_utf8(template_bytes.into_owned())
            .context("popup.html is not valid utf-8")?;
        let resources_dir = resources_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        // Devtools reachable (right-click > Inspect) in preview / debug /
        // bench. Also drives the popup's self-diagnostic: when on, the page
        // reports the WebGL renderer + fetched vendor bytes over window.ipc;
        // when off (real scheduled popup), it blocks the context menu.
        let devtools = preview || debug || bench_secs.is_some();
        let rendered = render_html(
            &template,
            &cfg,
            &anim_url,
            countdown,
            preview,
            debug,
            devtools,
            scale,
            offset_y_vh,
            renderer,
        );

        tracing::info!(
            preview,
            countdown,
            anim = %anim_path,
            "popup mode starting",
        );

        tracing::info!("popup: creating event loop");
        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

        tracing::info!("popup: building tao window (fullscreen transparent borderless)");
        // Follows the pattern in wry's own transparent.rs example
        // exactly : with_undecorated_shadow(false) BEFORE build, then
        // set_undecorated_shadow(true) AFTER build. Without this
        // Windows-specific dance the DWM shadow attribute clobbers
        // the WebView2 DirectComposition alpha and the popup renders
        // fully white.
        use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
        // Preview popups (parent-triggered from the config UI) get a
        // "(preview)" suffix so they're distinguishable in Task Manager
        // / Alt-Tab from a real scheduled break.
        let window_title = if preview {
            format!("{} (preview)", cfg.popup_window_title)
        } else {
            cfg.popup_window_title.clone()
        };
        let window = WindowBuilder::new()
            .with_title(&window_title)
            .with_transparent(true)
            .with_decorations(false)
            .with_always_on_top(true)
            // Fullscreen deferred to after style force below. Creating
            // with fullscreen at build-time makes tao show the window
            // immediately with default chrome bits, so the titlebar
            // flashes visible before our style force lands. Building
            // hidden + styling + then set_visible(true) skips that.
            .with_visible(false)
            .with_fullscreen(Some(Fullscreen::Borderless(None)))
            .with_undecorated_shadow(false)
            .build(&event_loop)
            .context("build tao window")?;
        window.set_undecorated_shadow(true);
        tracing::info!("popup: tao window built (hidden, undecorated shadow dance applied)");

        let hwnd = HWND(window.hwnd() as *mut _);
        tracing::info!(hwnd = format!("0x{:x}", window.hwnd() as isize), "popup: HWND acquired");

        window_style::apply_topmost_toolwindow(hwnd)
            .context("apply WS_EX_TOOLWINDOW|WS_EX_TOPMOST")?;
        tracing::info!("popup: apply_topmost_toolwindow OK");

        // Strip the title-bar bits (WS_CAPTION | WS_SYSMENU |
        // WS_MINIMIZEBOX | WS_MAXIMIZEBOX) — surgical enough to hide
        // the grey Windows chrome without touching WS_THICKFRAME /
        // WS_BORDER / WS_DLGFRAME (which session 5.2 noted breaks
        // WebView2's DirectComposition transparency) or WS_OVERLAPPED
        // (which the 0.5.13 nuke to WS_POPUP alone regressed for
        // service-spawned popups).
        if let Err(e) = window_style::strip_titlebar_minimal(hwnd) {
            tracing::warn!(error = ?e, "popup: strip_titlebar_minimal failed");
        }
        // Explicit taskbar removal via ITaskbarList::DeleteTab.
        // WS_EX_TOOLWINDOW alone doesn't reliably hide the popup on
        // Win11 ARM64 — after Win+D or a focus change, the taskbar
        // shows a preview thumbnail with a close button on hover.
        if let Err(e) = window_style::remove_from_taskbar(hwnd) {
            tracing::warn!(error = ?e, "popup: remove_from_taskbar failed");
        }
        // Lock the chromeless state at WndProc level : intercept
        // WM_STYLECHANGING to block WS_CAPTION re-add on activation /
        // restore-from-minimize, and intercept WM_SYSCOMMAND SC_MINIMIZE
        // to block Win+D from minimizing the popup entirely. Extensive
        // tracing under `RUST_LOG=info` so widget.log tells us which
        // Win32 events fire during the popup life.
        if let Err(e) = window_style::subclass_lock_chromeless(hwnd) {
            tracing::warn!(error = ?e, "popup: subclass_lock_chromeless failed");
        }

        // Now that every style bit is in its final shape, show the
        // window. Windows only ever draws this HWND with WS_POPUP —
        // no chrome flash between build and style-force.
        window.set_visible(true);

        // CRITICAL : re-force fullscreen borderless AFTER set_visible.
        // The `.with_fullscreen(...)` at build time is ignored when
        // combined with `.with_visible(false)` — tao only transitions
        // to fullscreen when the window is visible+active. Without
        // this re-invocation the popup opens as a regular top-level
        // window (Windows 11 DWM chrome + title bar) instead of
        // covering the whole monitor.
        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        tracing::info!("popup: fullscreen re-forced post-set_visible");

        // Steal foreground, defeating Windows' foreground-lock policy.
        // Necessary on any spawn where no user input happened in the
        // last few seconds : cold-start relaunch after reboot, service
        // auto-fire while the user is in a game / IDE / any focused
        // app. Without this, the popup sits topmost in z-order but
        // stays behind the currently active window until the user
        // clicks the taskbar. Preview / debug paths don't strictly
        // need it (spawn happens right after a user click, foreground
        // lock is relaxed) but calling unconditionally is safe.
        if let Err(e) = window_style::force_foreground(hwnd) {
            tracing::warn!(error = ?e, "popup: force_foreground failed");
        } else {
            tracing::info!("popup: force_foreground applied");
        }

        // NB : strip_window_frame + disable_dwm_nc_rendering are NOT
        // called on the popup. It's fullscreen borderless — no bord
        // visible by definition, no DWM shadow either. Both calls
        // were observed to interfere with WebView2's DirectComposition
        // transparency, forcing wry to render an opaque white
        // background. Fullscreen + with_transparent(true) alone
        // suffices for the popup.

        // WebView2 user data folder under <exe_dir>/Data/WebView2/ so
        // Edge WebView2 workers write their cache alongside the exe
        // instead of at exe-dir root. Best-effort create ; wry falls
        // back to a default location if we pass None.
        let webview2_data = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| crate::modes::install::webview2_dir(p)));

        // Nested helper : takes ownership of the values captured into
        // the builder's closures so it can be called twice on the B3
        // retry path (see the wipe-then-retry match below).
        fn build_webview(
            window: &tao::window::Window,
            webview2_data: Option<PathBuf>,
            resources_dir: PathBuf,
            rendered: String,
            proxy: tao::event_loop::EventLoopProxy<UserEvent>,
            devtools: bool,
        ) -> Result<(wry::WebView, wry::WebContext)> {
            if let Some(ref dir) = webview2_data {
                let _ = std::fs::create_dir_all(dir);
            }
            // WebContext must outlive the builder (`'a` lifetime) — bind
            // BEFORE the webview so drop order is correct.
            let mut web_ctx = wry::WebContext::new(webview2_data);
            let webview = wry::WebViewBuilder::new_with_web_context(&mut web_ctx)
                .with_transparent(true)
                // Right-click > Inspect. Gated off in real scheduled
                // popups (the kid must not get a devtools escape hatch) ;
                // available in preview / debug / bench so the
                // GPU-compositing diagnostic can read the WebGL renderer
                // string + rendering stats. Never auto-opened : an open
                // devtools window would pollute the bench numbers.
                .with_devtools(devtools)
                .with_custom_protocol(
                    "purrpause".to_string(),
                    move |_webview_id, request| {
                        serve_custom_protocol(
                            request,
                            &resources_dir,
                            &rendered,
                        )
                    },
                )
                .with_url("purrpause://localhost/popup.html")
                .with_ipc_handler(move |request| {
                    let body = request.body().as_str();
                    if body == "dismiss" {
                        let _ = proxy.send_event(UserEvent::Dismiss);
                        return;
                    }
                    // Self-diagnostic from a preview / debug / bench popup :
                    // the WebGL renderer string (GPU-compositing verdict) +
                    // the fetched vendor bytes (conditional-import proof),
                    // posted here so no DevTools window is needed (the
                    // fullscreen topmost popup would sit over it). Audit trail
                    // to widget.log, plus a one-line append to
                    // <exe_dir>\Data\Logs\bench-diag.log (kept out of bench.log
                    // so its strict 12-col TSV stays parseable).
                    if let Some(rest) = body.strip_prefix("diag:") {
                        tracing::info!(diag = rest, "bench.diag");
                        if let Some(dir) = std::env::current_exe()
                            .ok()
                            .and_then(|e| e.parent().map(|p| crate::modes::install::logs_dir(p)))
                        {
                            let _ = std::fs::create_dir_all(&dir);
                            let ts = chrono::Local::now()
                                .format("%Y-%m-%dT%H:%M:%S%.3f")
                                .to_string();
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .append(true)
                                .create(true)
                                .open(dir.join("bench-diag.log"))
                            {
                                use std::io::Write;
                                let _ = writeln!(f, "{ts}\t{rest}");
                            }
                        }
                        return;
                    }
                    // WebView2 JS-side pincode capture, primary surface :
                    // the LL keyboard hook + tao WindowEvent path both
                    // fail on some virtualised guests (Parallels on
                    // Apple Silicon in particular — the hypervisor
                    // routes keystrokes to the Chromium renderer
                    // without going through the Windows guest input
                    // queue). JS in popup.html does the buffer + submit
                    // trigger and posts a completed pincode string
                    // here for argon2id verification. We also log every
                    // pincode:* message for the widget.log audit trail.
                    if let Some(rest) = body.strip_prefix("pincode:") {
                        tracing::info!(msg = rest, "pincode.web: ipc message");
                        // Message shapes :
                        //   submit:<digits>   → argon2 verify path
                        //   cancel            → clear asterisks visually
                        //   digit:<char>      → JS already updated its
                        //                       own display ; ignored on
                        //                       the Rust side (kept in
                        //                       the log for audit only)
                        if let Some(s) = rest.strip_prefix("submit:") {
                            let _ = proxy.send_event(UserEvent::PincodeSubmit(s.to_string()));
                        } else if rest == "cancel" {
                            let _ = proxy.send_event(UserEvent::PincodeCancel);
                        } else if let Some(_d) = rest.strip_prefix("digit:") {
                            // JS renders its own asterisks now — no
                            // UserEvent dispatch needed. Message kept
                            // for the widget.log audit trail.
                        }
                    }
                })
                .build(window)
                .context("build wry webview")?;
            Ok((webview, web_ctx))
        }

        tracing::info!("popup: building wry webview");
        let first_attempt = build_webview(
            &window,
            webview2_data.clone(),
            resources_dir.clone(),
            rendered.clone(),
            event_loop.create_proxy(),
            devtools,
        );
        // Force-shutdown mid-popup can leave <exe_dir>/Data/WebView2/
        // in a locked/partial state, surfacing on the next popup as
        // HRESULT 0x80070490 ("Élément introuvable" / ERROR_NOT_FOUND).
        // On any first-build failure : kill surviving workers, wipe the
        // UserData folder, retry once. Latch is implicit (single retry) ;
        // a second failure surfaces to the service tick loop's crash-
        // retry backoff.
        let (webview, web_ctx) = match first_attempt {
            Ok(pair) => pair,
            Err(first_err) => {
                let hresult_match = is_webview2_not_found_error(&first_err);
                tracing::warn!(
                    error = ?first_err,
                    hresult_match,
                    "webview2 build failed - wiping UserData and retrying once"
                );
                if let Some(ref dir) = webview2_data {
                    kill_orphan_webview2_workers(dir);
                    let _ = std::fs::remove_dir_all(dir);
                    let _ = std::fs::create_dir_all(dir);
                }
                build_webview(
                    &window,
                    webview2_data,
                    resources_dir.clone(),
                    rendered.clone(),
                    event_loop.create_proxy(),
                    devtools,
                )
                .context("build wry webview after WebView2 UserData wipe (retry-once)")?
            }
        };
        tracing::info!("popup: wry webview built");

        // webview + web_ctx must outlive the event loop (dropping
        // either destroys the child hwnd / WebView2 state). Both are
        // captured by the `event_loop.run` closure below via `move`,
        // which is also how the Rust→WebView `evaluate_script` bridge
        // reaches the WebView from the pincode arms.
        let web_ctx = web_ctx;

        // Install the low-level keyboard hook AFTER wry finished
        // WebView2 init. Order matters : if we install before wry
        // build, WebView2 init blocks the message pump for 1-3 s ;
        // Windows' LowLevelHooksTimeout (default 300 ms) then silently
        // removes our hook and Alt+Tab et al pass through. Installing
        // here means the hook is registered milliseconds before the
        // event loop starts pumping, so the OS never times it out.
        //
        // Debug mode leaves Alt+F4 / Alt+Tab / Win+D / Ctrl+Esc alone
        // so a developer can exit the popup during smoke without
        // killing the process from Task Manager. Alt+pincode capture
        // is also disabled in debug for the same reason.
        //
        // Prod : if `passcode_hash` is present, install the pincode-
        // capturing variant with a closure that maps hook events to
        // tao proxy events. Empty / absent hash ⇒ install the plain
        // hook (shortcut suppression only, no unlock path).
        let pincode_cap = cfg.passcode_length as usize;
        let mut kb_guard = install_popup_keyboard_hook(
            debug,
            passcode_hash.as_deref(),
            pincode_cap,
            event_loop.create_proxy(),
        )?;

        // Post-countdown unlock timer. After `countdown` seconds
        // (WALL-CLOCK, so system suspend counts) the dismiss button
        // appears, the LL keyboard hook lifts, always-on-top drops,
        // and Alt+F4 is unblocked.
        //
        // Uses a poll loop against `SystemTime::now()` rather than
        // `std::thread::sleep(timer_secs)` because on Windows `Sleep()`
        // is not guaranteed to include suspend time — a long
        // hibernation during the popup could freeze the timer, leaving
        // Alt+F4 blocked / popup topmost even though the JS wall-clock
        // check (popup.html) has already enabled the dismiss button.
        // Poll cadence 500 ms keeps CPU cost negligible ; the timer
        // thread sleeps most of the time.
        // Cas D respawn : service was killed mid-pause and restarted.
        // The parent passes the elapsed wall-clock since the original
        // popup fire, so the dismiss countdown resumes at the right
        // offset. If elapsed >= duration, `saturating_sub` clamps to 0
        // and the 500 ms polling loop fires CountdownExpired on the
        // very first tick → dismiss button immediately active.
        // Session 4 : the deadline is a shared atomic (unix epoch secs)
        // instead of a fixed target so an inbound IPC UpdateContent /
        // RestartCountdown can re-arm the unlock timer on the live
        // popup. The thread loops forever (it must survive a re-arm ;
        // it dies with the process) and de-dupes on the deadline VALUE
        // so one distinct deadline fires exactly one CountdownExpired.
        // Degenerate edge : re-arming to the exact epoch second that
        // already fired is skipped by the de-dupe — requires a
        // 0-duration re-arm within the same second, acceptable.
        let elapsed_pre = elapsed_seconds.unwrap_or(0) as u64;
        let timer_proxy = event_loop.create_proxy();
        let deadline = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            epoch_secs_now() + (countdown as u64).saturating_sub(elapsed_pre),
        ));
        let timer_deadline = std::sync::Arc::clone(&deadline);
        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            let mut fired_for: u64 = 0;
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let d = timer_deadline.load(Ordering::Relaxed);
                if epoch_secs_now() >= d && fired_for != d {
                    if timer_proxy.send_event(UserEvent::CountdownExpired).is_err() {
                        break;
                    }
                    fired_for = d;
                }
            }
        });

        // Benchmark sampler thread. Samples the whole process tree
        // (host + msedgewebview2 children — the renderer child is where
        // the rasterization cost lives) every 500 ms into
        // Data\Logs\bench.log, then BenchDone auto-exits the popup.
        // Spawned AFTER the webview build so the WebView2 children
        // exist by the first tick. Self-sampling caveat : the thread's
        // own cost lands in the host row — negligible at 500 ms cadence
        // against a saturated renderer core.
        if let Some(secs) = bench_secs {
            let bench_proxy = event_loop.create_proxy();
            let (anim_file, anim_ext) =
                crate::bench::split_anim_name(&resolve_animation_url(&anim_path));
            // Split the *resolved* URL, not the raw `anim_path`: an `embed:*`
            // id has no `.lottie` suffix, so splitting it yields an empty ext
            // and `renderer_label` collapses to the thorvg-sw default even on
            // a Skottie run. `renderer` (Copy) is the same value threaded into
            // render_html, so the label matches what actually rendered
            // (skottie-gpu vs thorvg-sw).
            std::thread::spawn(move || {
                if let Err(e) = run_bench_sampler(secs, &anim_file, &anim_ext, renderer) {
                    tracing::warn!(error = ?e, "bench sampler failed");
                }
                // Sent on the error path too — the popup must always
                // auto-exit once the bench window is over. Exception :
                // the unlimited window (secs == 0) samples alongside a
                // normally-behaving popup, which owns its own exit.
                if secs != 0 {
                    let _ = bench_proxy.send_event(UserEvent::BenchDone);
                }
            });
        }

        // Per-PID content pipe. Runs for EVERY popup (interval popups
        // are exactly the ones a daily-pause collision must mutate).
        // The reader thread owns only a Send proxy ; the parsed
        // messages' owned Strings move through send_event into the
        // main thread, which is the sole owner of the webview. Thread
        // lives until process exit — event_loop.run diverges, so there
        // is no teardown path to wire.
        {
            let ipc_proxy = event_loop.create_proxy();
            std::thread::spawn(move || {
                let map = move |msg: crate::ipc_popup::PopupMessage| {
                    use crate::ipc_popup::PopupMessage as PM;
                    let ue = match msg {
                        PM::UpdateContent {
                            title,
                            subtitle,
                            dismiss_label,
                            duration_secs,
                            mode,
                        } => UserEvent::IpcUpdateContent {
                            title,
                            subtitle,
                            dismiss_label,
                            duration_secs,
                            mode,
                        },
                        PM::Dismiss => UserEvent::IpcDismiss,
                        PM::RestartCountdown {
                            palier_minutes,
                            seconds_until,
                        } => UserEvent::IpcRestartCountdown {
                            palier_minutes,
                            seconds_until,
                        },
                    };
                    let _ = ipc_proxy.send_event(ue);
                };
                if let Err(e) = crate::ipc_popup::run_popup_server(map) {
                    tracing::warn!(error = ?e, "popup ipc server exited");
                }
            });
        }

        // Cold-start focus insurance. A daily-pause popup respawned at
        // service boot (Cas D, identified by `--elapsed-seconds`) can
        // appear right as the session becomes interactive after the
        // logon sequence — the spawn-time force_foreground then lands
        // before the desktop accepts input and the popup stays visible
        // but WITHOUT keyboard focus, so the WebView pincode capture
        // never sees keystrokes. Re-assert foreground + focus every
        // 500 ms for the first few seconds so the popup grabs input as
        // soon as the session is ready. Scoped to the respawn case so
        // normal (session-active) fires are untouched.
        if elapsed_seconds.is_some() {
            const FOCUS_REASSERT_TICKS: u32 = 12; // ~6 s at 500 ms.
            let focus_proxy = event_loop.create_proxy();
            std::thread::spawn(move || {
                for _ in 0..FOCUS_REASSERT_TICKS {
                    std::thread::sleep(Duration::from_millis(500));
                    if focus_proxy
                        .send_event(UserEvent::ReassertForeground)
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        let mut countdown_expired = false;
        // Popup-scoped 3-strike counter for Alt+pincode verifies.
        // Semantics differ from `modes::config::lockout::Lockout` (which
        // has a 30 s wall-clock timer) : here, lockout persists for the
        // popup's lifetime and resets naturally when the process exits.
        // Kept as a plain `u32` in the closure — no persistence, no timer.
        let mut fail_count: u32 = 0;
        let hash_for_verify = passcode_hash.clone().unwrap_or_default();
        // Second proxy for the locked-verdict self-dispatch inside the
        // Submit arm — the original event proxy was moved into the hook
        // closure above.
        let locked_proxy = event_loop.create_proxy();
        // Third proxy, held for the IPC re-lock path : an UpdateContent
        // landing on an already-unlocked popup re-installs the keyboard
        // hook, which needs a fresh proxy for its pincode closure.
        let hook_proxy = event_loop.create_proxy();
        tracing::info!("popup: entering event loop");

        event_loop.run(move |event, _target, control_flow| {
            *control_flow = ControlFlow::Wait;
            // Force the closure to also own `web_ctx` — it must outlive
            // the event loop, but no branch reads it. Silences the
            // move-into-closure unused warning without a top-level `_`
            // rename that would obscure intent.
            let _ = &web_ctx;
            match event {
                Event::UserEvent(UserEvent::Dismiss) => {
                    tracing::info!("popup dismissed by user");
                    *control_flow = ControlFlow::Exit;
                }
                Event::UserEvent(UserEvent::BenchDone) => {
                    tracing::info!("popup: bench window complete - exiting");
                    *control_flow = ControlFlow::Exit;
                }
                Event::UserEvent(UserEvent::ReassertForeground) => {
                    // Cold-start focus insurance tick. Skip once the
                    // break window ended — the popup then intentionally
                    // drops always-on-top and shouldn't grab focus back.
                    if !countdown_expired {
                        if let Err(e) = window_style::force_foreground(hwnd) {
                            tracing::warn!(error = ?e, "popup: cold-start force_foreground retry failed");
                        }
                        window.set_focus();
                    }
                }
                Event::UserEvent(UserEvent::PincodeDigitAccepted(idx)) => {
                    let js = format!(
                        "window.purrpauseOnEvent&&window.purrpauseOnEvent({{kind:'digit',idx:{idx}}})"
                    );
                    let _ = webview.evaluate_script(&js);
                }
                Event::UserEvent(UserEvent::PincodeSubmit(buf)) => {
                    // Argon2id verify. On any error (malformed hash,
                    // unlikely because we generated it via
                    // `password::hash`), treat as a failure — never
                    // dismiss on a doubt.
                    let ok = if hash_for_verify.is_empty() {
                        false
                    } else {
                        crate::password::verify(&buf, &hash_for_verify).unwrap_or(false)
                    };
                    if ok {
                        tracing::info!("popup: Ctrl+Shift+pincode verify OK - dismissing");
                        let _ = webview.evaluate_script(
                            "window.purrpauseOnEvent&&window.purrpauseOnEvent({kind:'ok'})",
                        );
                        // Reuse the Dismiss termination path — the
                        // service's try_wait_exited catches the exit
                        // and clears popup_pending.
                        *control_flow = ControlFlow::Exit;
                    } else {
                        fail_count = fail_count.saturating_add(1);
                        tracing::info!(
                            fail_count,
                            "popup: Alt+pincode verify FAILED"
                        );
                        let js = format!(
                            "window.purrpauseOnEvent&&window.purrpauseOnEvent({{kind:'fail',count:{fail_count}}})"
                        );
                        let _ = webview.evaluate_script(&js);
                        if fail_count >= 3 {
                            // Self-dispatch — handled below to keep the
                            // "flip WebView + tell hook" logic in one
                            // place. proxy.send_event is safe cross-arm
                            // because the event loop consumes it FIFO.
                            let _ = locked_proxy
                                .send_event(UserEvent::PincodeVerdictLocked);
                        }
                    }
                }
                Event::UserEvent(UserEvent::PincodeCancel) => {
                    let _ = webview.evaluate_script(
                        "window.purrpauseOnEvent&&window.purrpauseOnEvent({kind:'cancel'})",
                    );
                }
                Event::UserEvent(UserEvent::PincodeVerdictLocked) => {
                    tracing::info!("popup: pincode locked (3 failed attempts)");
                    let _ = webview.evaluate_script(
                        "window.purrpauseOnEvent&&window.purrpauseOnEvent({kind:'locked'})",
                    );
                    keyboard_hook::mark_locked();
                }
                Event::UserEvent(UserEvent::IpcUpdateContent {
                    title,
                    subtitle,
                    dismiss_label,
                    duration_secs,
                    mode: content_mode,
                }) => {
                    tracing::info!(
                        ?content_mode,
                        duration_secs,
                        "popup: IPC UpdateContent received - mutating content in place"
                    );
                    let payload = serde_json::json!({
                        "title": title,
                        "subtitle": subtitle,
                        "dismissLabel": dismiss_label,
                        "durationSecs": duration_secs,
                        "mode": match content_mode {
                            crate::ipc_popup::PopupContentMode::DailyPause => "daily-pause",
                            crate::ipc_popup::PopupContentMode::Interval => "interval",
                        },
                    });
                    let _ = webview.evaluate_script(&format!(
                        "window.purrpauseApplyUpdate&&window.purrpauseApplyUpdate({payload})"
                    ));
                    deadline.store(
                        epoch_secs_now() + duration_secs,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    if countdown_expired {
                        // The previous content had already unlocked
                        // (post-countdown). A fresh pause re-locks :
                        // hook back on, topmost back on, focus stolen.
                        countdown_expired = false;
                        window_style::POPUP_UNLOCKED
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        window.set_always_on_top(true);
                        if kb_guard.is_none() {
                            match install_popup_keyboard_hook(
                                debug,
                                passcode_hash.as_deref(),
                                pincode_cap,
                                hook_proxy.clone(),
                            ) {
                                Ok(g) => kb_guard = g,
                                Err(e) => tracing::warn!(
                                    error = ?e,
                                    "popup: re-lock hook re-install failed"
                                ),
                            }
                        }
                        if let Err(e) = window_style::force_foreground(hwnd) {
                            tracing::warn!(error = ?e, "popup: re-lock force_foreground failed");
                        }
                        window.set_focus();
                    }
                    if content_mode == crate::ipc_popup::PopupContentMode::DailyPause {
                        // Ack the daily update back to the service over
                        // the signal pipe : the service stamps the daily
                        // fire ONLY on this confirmation (a bare content-
                        // pipe write is not proof the popup applied it).
                        // Detached thread — a busy pipe must never stall
                        // the UI event loop.
                        let pid = std::process::id();
                        std::thread::spawn(move || {
                            match crate::ipc::send(
                                crate::ipc::Message::DailyPauseApplied { pid },
                            ) {
                                Ok(()) => tracing::info!(
                                    pid,
                                    "popup: daily-pause apply ack sent to service"
                                ),
                                Err(e) => tracing::warn!(
                                    error = ?e,
                                    pid,
                                    "popup: daily-pause apply ack send failed"
                                ),
                            }
                        });
                    }
                }
                Event::UserEvent(UserEvent::IpcRestartCountdown {
                    palier_minutes,
                    seconds_until,
                }) => {
                    tracing::info!(
                        palier_minutes,
                        seconds_until,
                        "popup: IPC RestartCountdown received"
                    );
                    let payload = serde_json::json!({ "durationSecs": seconds_until });
                    let _ = webview.evaluate_script(&format!(
                        "window.purrpauseApplyUpdate&&window.purrpauseApplyUpdate({payload})"
                    ));
                    deadline.store(
                        epoch_secs_now() + seconds_until,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    if countdown_expired {
                        // Same re-lock as the UpdateContent arm — a
                        // shared closure can't borrow the captured
                        // &muts across arms, so the block is duplicated.
                        countdown_expired = false;
                        window_style::POPUP_UNLOCKED
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        window.set_always_on_top(true);
                        if kb_guard.is_none() {
                            match install_popup_keyboard_hook(
                                debug,
                                passcode_hash.as_deref(),
                                pincode_cap,
                                hook_proxy.clone(),
                            ) {
                                Ok(g) => kb_guard = g,
                                Err(e) => tracing::warn!(
                                    error = ?e,
                                    "popup: re-lock hook re-install failed"
                                ),
                            }
                        }
                        if let Err(e) = window_style::force_foreground(hwnd) {
                            tracing::warn!(error = ?e, "popup: re-lock force_foreground failed");
                        }
                        window.set_focus();
                    }
                }
                Event::UserEvent(UserEvent::IpcDismiss) => {
                    tracing::info!("popup dismissed via IPC");
                    // Same clean-exit path as UserEvent::Dismiss : exit
                    // code 0, the service's try_wait_exited treats it
                    // as a normal dismiss.
                    *control_flow = ControlFlow::Exit;
                }
                Event::UserEvent(UserEvent::CountdownExpired) => {
                    // Break-enforcement window over. Uninstall the LL
                    // keyboard hook on the same thread that installed
                    // it (tao's main = here, per SetWindowsHookEx docs),
                    // flip the subclass unlock flag (so SC_CLOSE stops
                    // being blocked as a defense-in-depth), lift
                    // always-on-top so Task Manager / other windows can
                    // overlay, and set the closure flag that neutralises
                    // the focus handlers below.
                    let _ = kb_guard.take();
                    window_style::POPUP_UNLOCKED
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    window.set_always_on_top(false);
                    countdown_expired = true;
                    tracing::info!("popup: countdown expired, restrictions lifted");
                }
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    *control_flow = ControlFlow::Exit;
                }
                // Fallback pincode surface : tao's WindowEvent::KeyboardInput.
                // Log every keystroke the tao event loop receives so
                // future debugging on unfamiliar environments has a
                // second independent trace next to the LL hook one.
                // The tao pump reads WM_KEYDOWN/WM_KEYUP that landed on
                // the popup HWND after normal message routing. If the
                // OS input queue is intercepted upstream (e.g. some
                // hypervisor keyboard passthroughs), both this and the
                // LL hook see nothing — that's the signal to fall back
                // to the WebView2 JS capture surface.
                Event::WindowEvent {
                    event: WindowEvent::KeyboardInput { event: ref kevt, .. },
                    ..
                } => {
                    tracing::info!(
                        state = ?kevt.state,
                        physical = format!("{:?}", kevt.physical_key),
                        logical = format!("{:?}", kevt.logical_key),
                        text = ?kevt.text,
                        repeat = kevt.repeat,
                        "pincode.tao: keyboard_input"
                    );
                }
                // Also log ReceivedCharacter — WM_CHAR-derived, layout-
                // resolved character output. Sometimes fires when
                // KeyboardInput doesn't (input methods, dead keys).
                Event::WindowEvent {
                    event: WindowEvent::ReceivedImeText(ref s),
                    ..
                } => {
                    tracing::info!(text = %s, "pincode.tao: ime_text");
                }
                Event::WindowEvent {
                    event: WindowEvent::ModifiersChanged(ref mods),
                    ..
                } => {
                    tracing::info!(mods = format!("{:?}", mods), "pincode.tao: modifiers_changed");
                }
                Event::WindowEvent { event: WindowEvent::Focused(true), .. }
                    if !countdown_expired =>
                {
                    // Re-strip title-bar bits + re-remove taskbar entry
                    // on every focus regain — Win+D that slipped past
                    // the hook, restore-from-minimize etc. can re-add
                    // WS_CAPTION or the taskbar tab.
                    if let Err(e) = window_style::strip_titlebar_minimal(hwnd) {
                        tracing::warn!(error = ?e, "popup: re-strip on focus failed");
                    }
                    if let Err(e) = window_style::apply_topmost_toolwindow(hwnd) {
                        tracing::warn!(error = ?e, "popup: re-apply topmost on focus failed");
                    }
                    if let Err(e) = window_style::remove_from_taskbar(hwnd) {
                        tracing::warn!(error = ?e, "popup: re-remove from taskbar on focus failed");
                    }
                }
                Event::WindowEvent { event: WindowEvent::Focused(false), .. }
                    if !debug && !countdown_expired =>
                {
                    // Focus lost = user hit Win+D (system hotkey that
                    // bypasses our LL keyboard hook), or clicked another
                    // window somehow. Steal focus back so the popup can't
                    // be dismissed by focus-loss shortcuts.
                    tracing::info!("popup: focus lost, re-stealing");
                    window.set_minimized(false);
                    window.set_visible(true);
                    window.set_focus();
                }
                _ => {}
            }
        });
    }

    /// Detects HRESULT `0x80070490` — HRESULT_FROM_WIN32(ERROR_NOT_FOUND)
    /// — anywhere in the error chain's Debug representation. Surfaces
    /// post-force-shutdown when the WebView2 UserData folder holds stale
    /// locks. The hex literal is locale-independent (Debug printing of
    /// `windows::core::Error` always includes `0x80070490`, unlike the
    /// message which flips to "Élément introuvable" on FR systems).
    /// Downcast-through-chain would be cleaner but our dependency graph
    /// has two versions of `webview2_com` + `windows` (transitive via
    /// wry 0.55 vs. our direct deps) so exact-type matching fails.
    /// Used only for the retry log field ; the wipe path fires on any
    /// first-build failure per the plan's wipe-once-per-lifetime latch.
    fn is_webview2_not_found_error(err: &anyhow::Error) -> bool {
        format!("{err:?}").contains("0x80070490")
    }

    fn serve_custom_protocol(
        request: wry::http::Request<Vec<u8>>,
        resources_dir: &Path,
        rendered_popup_html: &str,
    ) -> wry::http::Response<Cow<'static, [u8]>> {
        let uri = request.uri();
        // Trim leading slash — uri().path() returns "/popup.html".
        let path = uri.path().trim_start_matches('/');
        if path == "popup.html" || path.is_empty() {
            return wry::http::Response::builder()
                .status(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Cow::Owned(rendered_popup_html.as_bytes().to_vec()))
                .unwrap_or_else(|_| empty_response());
        }
        // Virtual `<name>.lottie.json` — Skottie (mountSkottie) needs raw
        // bodymovin JSON, but a `.lottie` is a ZIP. Strip `.json`, load the
        // real `.lottie` bytes, unzip the largest `animations/*.json` entry
        // and serve it. Keeps the single `.lottie` source for both engines
        // (no pre-extracted duplicate on disk / in the schema).
        if let Some(lottie_path) = path.strip_suffix(".json") {
            if lottie_path.ends_with(".lottie") {
                return match crate::embedded::get_any(lottie_path) {
                    Some(bytes) => match crate::lottie_sanitize::extract_bodymovin_json(&bytes) {
                        Ok(json) => wry::http::Response::builder()
                            .status(200)
                            .header("Content-Type", "application/json")
                            .body(Cow::Owned(json))
                            .unwrap_or_else(|_| empty_response()),
                        Err(e) => {
                            tracing::warn!(path, error = %e, "skottie: unzip bodymovin JSON failed");
                            wry::http::Response::builder()
                                .status(500)
                                .body(Cow::Owned(Vec::new()))
                                .unwrap_or_else(|_| empty_response())
                        }
                    },
                    None => {
                        tracing::warn!(lottie_path, "skottie: source .lottie not found for virtual json");
                        wry::http::Response::builder()
                            .status(404)
                            .body(Cow::Owned(Vec::new()))
                            .unwrap_or_else(|_| empty_response())
                    }
                };
            }
        }
        // Lookup order : disk override (dev iterating on shipped
        // assets) → embedded (single-exe default) → disk-only
        // (user-added animations, wildcard files that aren't baked in).
        // `resources_dir` still needed as the base for the disk-only
        // branch, but that's inside `get_any`.
        let _ = resources_dir; // silences unused-param under strict lints
        match crate::embedded::get_any(path) {
            Some(bytes) => wry::http::Response::builder()
                .status(200)
                .header("Content-Type", mime_for(path))
                .body(Cow::Owned(bytes))
                .unwrap_or_else(|_| empty_response()),
            None => {
                tracing::warn!(path, "custom protocol: asset not found (embedded + disk both missed)");
                wry::http::Response::builder()
                    .status(404)
                    .body(Cow::Owned(Vec::new()))
                    .unwrap_or_else(|_| empty_response())
            }
        }
    }

    fn empty_response() -> wry::http::Response<Cow<'static, [u8]>> {
        wry::http::Response::new(Cow::Owned(Vec::new()))
    }

    fn mime_for(path: &str) -> &'static str {
        let ext = Path::new(path).extension().and_then(|s| s.to_str()).unwrap_or("");
        match ext {
            "html" => "text/html; charset=utf-8",
            "js" | "mjs" => "application/javascript; charset=utf-8",
            "wasm" => "application/wasm",
            "json" | "lottie" => "application/json",
            "css" => "text/css; charset=utf-8",
            "png" => "image/png",
            "apng" => "image/apng",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "webm" => "video/webm",
            "riv" => "application/octet-stream",
            _ => "application/octet-stream",
        }
    }

    fn resources_dir() -> Result<PathBuf> {
        let exe = std::env::current_exe().context("env::current_exe()")?;
        let dir = exe
            .parent()
            .context("exe has no parent")?
            .join("resources");
        Ok(dir)
    }

    fn rotation_dat_path() -> PathBuf {
        Path::new(DIAGNOSTICS_CACHE_DIR).join(ROTATION_DAT_FILENAME)
    }

    fn read_rotation_dat() -> Result<u32> {
        let path = rotation_dat_path();
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let arr: [u8; 4] = bytes
            .as_slice()
            .try_into()
            .context("rotation.dat is not 4 bytes")?;
        Ok(u32::from_le_bytes(arr))
    }

    fn write_rotation_dat(value: u32) -> Result<()> {
        let path = rotation_dat_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("dat.tmp");
        std::fs::write(&tmp, value.to_le_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AnimationEntry, Config, Renderer, RotationMode};

    fn anim(name: &str, enabled: bool) -> AnimationEntry {
        AnimationEntry {
            file: format!("animations/{name}.lottie"),
            enabled,
            display_name: name.to_string(),
            scale: 1.0,
            offset_y_vh: 0,
        }
    }

    fn cfg_with_anims(anims: Vec<AnimationEntry>, mode: RotationMode) -> Config {
        let mut cfg = Config::default();
        cfg.animations = anims;
        cfg.rotation_mode = mode;
        cfg
    }

    #[test]
    fn sequential_advances() {
        let cfg = cfg_with_anims(
            vec![anim("a", true), anim("b", true), anim("c", true)],
            RotationMode::Sequential,
        );
        let (picked, next) = animation_pick(&cfg, Some(0)).expect("some");
        assert_eq!(picked.file, "animations/a.lottie");
        assert_eq!(next, 1);
    }

    #[test]
    fn sequential_wraps() {
        let cfg = cfg_with_anims(
            vec![anim("a", true), anim("b", true), anim("c", true)],
            RotationMode::Sequential,
        );
        let (picked, next) = animation_pick(&cfg, Some(5)).expect("some");
        // 5 % 3 == 2 → third entry (c)
        assert_eq!(picked.file, "animations/c.lottie");
        assert_eq!(next, 6);
    }

    #[test]
    fn random_picks_enabled_only() {
        let cfg = cfg_with_anims(
            vec![anim("a", true), anim("b", false), anim("c", true)],
            RotationMode::Random,
        );
        for _ in 0..100 {
            let (picked, _) = animation_pick(&cfg, None).expect("some");
            assert!(picked.file.ends_with("a.lottie") || picked.file.ends_with("c.lottie"));
        }
    }

    #[test]
    fn empty_list_returns_none() {
        let cfg = cfg_with_anims(vec![], RotationMode::Random);
        assert!(animation_pick(&cfg, None).is_none());
    }

    #[test]
    fn all_disabled_returns_none() {
        let cfg = cfg_with_anims(
            vec![anim("a", false), anim("b", false)],
            RotationMode::Sequential,
        );
        assert!(animation_pick(&cfg, Some(0)).is_none());
    }

    fn template() -> String {
        // Miniature template mirroring the popup.html placeholder shape.
        r#"<title>{{ WINDOW_TITLE }}</title>
<body data-remaining-seconds="{{ COUNTDOWN_SECONDS }}"
      data-animation-path="{{ ANIMATION_PATH }}"
      data-countdown-template="{{ COUNTDOWN_TEMPLATE }}"
      data-mode="{{ POPUP_MODE }}">
  <h1>{{ POPUP_TITLE }}</h1>
  <p>{{ POPUP_SUBTITLE }}</p>
  <button>{{ DISMISS_LABEL }}</button>
</body>"#
            .to_string()
    }

    #[test]
    fn all_placeholders_replaced() {
        let cfg = Config::default();
        let out = render_html(
            &template(),
            &cfg,
            "animations/foo.lottie",
            300,
            false,
            false,
            false,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(!out.contains("{{"));
        assert!(!out.contains("}}"));
    }

    #[test]
    fn preserves_non_placeholder_curlies() {
        // The `{mm}` and `{ss}` markers inside countdown_template are consumed
        // by JS on the client, not by render_html — they must survive.
        let mut cfg = Config::default();
        cfg.countdown_template = "Pause dans {mm}:{ss}".to_string();
        let out = render_html(
            &template(),
            &cfg,
            "animations/foo.lottie",
            300,
            false,
            false,
            false,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(out.contains("Pause dans {mm}:{ss}"));
    }

    #[test]
    fn preview_countdown_10s() {
        let cfg = Config::default();
        let out = render_html(
            &template(),
            &cfg,
            "animations/foo.lottie",
            10,
            true,
            false,
            false,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(out.contains(r#"data-remaining-seconds="10""#));
        assert!(out.contains(r#"data-mode="preview""#));
    }

    #[test]
    fn window_title_replaced() {
        let mut cfg = Config::default();
        cfg.popup_window_title = "Windows Session Health".to_string();
        let out = render_html(
            &template(),
            &cfg,
            "animations/foo.lottie",
            60,
            false,
            false,
            false,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(out.contains("<title>Windows Session Health</title>"));
        // And live mode should mark data-mode accordingly.
        assert!(out.contains(r#"data-mode="live""#));
    }

    #[test]
    fn preview_window_title_gets_suffix() {
        // WebView2 propagates the HTML <title> to the parent HWND, so
        // preview mode has to bake the "(preview)" suffix into the
        // template render — the tao WindowBuilder title alone gets
        // clobbered a few frames later.
        let mut cfg = Config::default();
        cfg.popup_window_title = "Windows Session Health".to_string();
        let out = render_html(
            &template(),
            &cfg,
            "animations/foo.lottie",
            10,
            true,
            false,
            false,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(out.contains("<title>Windows Session Health (preview)</title>"));
        assert!(out.contains(r#"data-mode="preview""#));
    }

    #[test]
    fn stage_scale_and_offset_substituted() {
        let cfg = Config::default();
        let tpl = r#"<style>:root { --stage-scale: {{ STAGE_SCALE }}; --stage-offset-y: {{ STAGE_OFFSET_Y_VH }}vh; }</style>"#;
        let out = render_html(
            tpl,
            &cfg,
            "animations/foo.lottie",
            60,
            false,
            false,
            false,
            1.5,
            -12,
            Renderer::ThorvgSw,
        );
        assert!(out.contains("--stage-scale: 1.50"));
        assert!(out.contains("--stage-offset-y: -12vh"));
        assert!(!out.contains("{{ STAGE_SCALE }}"));
        assert!(!out.contains("{{ STAGE_OFFSET_Y_VH }}"));
    }

    #[test]
    fn devtools_flag_substituted() {
        // "1" drives the popup's self-diagnostic (WebGL renderer +
        // conditional-import proof over window.ipc) and keeps the context
        // menu; "0" (real scheduled popup) suppresses right-click instead.
        let cfg = Config::default();
        let tpl = r#"<body data-devtools="{{ DEVTOOLS }}"></body>"#;
        let on = render_html(
            tpl,
            &cfg,
            "animations/foo.lottie",
            60,
            false,
            false,
            true,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(on.contains(r#"data-devtools="1""#));
        let off = render_html(
            tpl,
            &cfg,
            "animations/foo.lottie",
            60,
            false,
            false,
            false,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(off.contains(r#"data-devtools="0""#));
    }

    #[test]
    fn stage_params_reads_animation_entry() {
        let mut cfg = Config::default();
        cfg.animations.push(AnimationEntry {
            file: "animations/dance.lottie".to_string(),
            enabled: true,
            display_name: "Dance".to_string(),
            scale: 2.6,
            offset_y_vh: -11,
        });
        assert_eq!(stage_params(&cfg, "animations/dance.lottie"), (2.6, -11));
        // Unknown file → CSS defaults.
        assert_eq!(stage_params(&cfg, "animations/unknown.lottie"), (1.0, 0));
    }

    #[test]
    fn render_html_emits_renderer_token() {
        let cfg = Config::default();
        let tpl = r#"<body data-renderer="{{ RENDERER }}"></body>"#;
        let thorvg = render_html(
            tpl,
            &cfg,
            "animations/foo.lottie",
            60,
            false,
            false,
            false,
            1.0,
            0,
            Renderer::ThorvgSw,
        );
        assert!(thorvg.contains(r#"data-renderer="thorvg""#));
        let skottie = render_html(
            tpl,
            &cfg,
            "animations/foo.lottie",
            60,
            false,
            false,
            false,
            1.0,
            0,
            Renderer::SkottieGpu,
        );
        assert!(skottie.contains(r#"data-renderer="skottie""#));
    }
}
