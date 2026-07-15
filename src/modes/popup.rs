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

use crate::config::{AnimationEntry, Config, RotationMode};

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

/// Pure HTML template renderer. Applies eight `str::replace` calls in
/// deterministic order.
///
/// **No HTML-escaping.** Config strings originate from the passcode-gated
/// config UI on the same machine — the threat model does not include
/// HTML injection. If a future user-supplied `countdown_template`
/// contains a `"` char it may break the enclosing HTML attribute quote ;
/// session 6's config UI should reject that at input time.
pub fn render_html(
    template: &str,
    cfg: &Config,
    animation_path: &str,
    countdown_seconds: u32,
    preview: bool,
    stage_scale: f32,
    stage_offset_y_vh: i8,
) -> String {
    let mode = if preview { "preview" } else { "live" };
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
        .replace("{{ STAGE_SCALE }}", &format!("{stage_scale:.2}"))
        .replace("{{ STAGE_OFFSET_Y_VH }}", &stage_offset_y_vh.to_string())
}

/// Resolve `(scale, offset_y_vh)` for the animation at `anim_file`. If
/// the config has an `AnimationEntry` for that file, its per-entry
/// values win. Otherwise fall back to `(1.0, 0)` — the CSS defaults.
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
) -> anyhow::Result<()> {
    windows_impl::run(preview, debug, anim_override, config_override, test_countdown_secs)
}

#[cfg(not(windows))]
pub fn run(
    _preview: bool,
    _debug: bool,
    _anim_override: Option<String>,
    _config_override: Option<String>,
    _test_countdown_secs: Option<u32>,
) -> anyhow::Result<()> {
    anyhow::bail!("popup mode is Windows-only")
}

#[cfg(windows)]
mod windows_impl {
    use std::borrow::Cow;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use anyhow::{Context, Result};

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    // WindowExtWindows / WindowBuilderExtWindows are imported inside
    // the run() body where the shadow-dance uses them.
    use tao::window::{Fullscreen, WindowBuilder};

    use windows::Win32::Foundation::HWND;

    use crate::modes::install::{DIAGNOSTICS_CACHE_DIR, STATE_DAT};
    use crate::platform::win32::{fullscreen_detect, keyboard_hook, window_style};

    use super::{animation_pick, render_html, resolve_animation_url, stage_params, FALLBACK_ANIM_PATH};

    const ROTATION_DAT_FILENAME: &str = "rotation.dat";

    #[derive(Debug, Clone)]
    enum UserEvent {
        Dismiss,
        CountdownExpired,
    }

    pub fn run(
        preview: bool,
        debug: bool,
        anim_override: Option<String>,
        config_override: Option<String>,
        test_countdown_secs: Option<u32>,
    ) -> Result<()> {
        // Preview / test spawns from the config UI pass a per-PID
        // temp state file so uncommitted edits show up without an
        // autosave. Regular service-spawned popups leave
        // config_override None → read state.dat.
        let cfg = match config_override.as_ref() {
            Some(path) => {
                let cfg = crate::config::load_or_default(Path::new(path));
                // Best-effort cleanup so the temp file doesn't linger.
                let _ = std::fs::remove_file(path);
                cfg
            }
            None => crate::config::load_or_default(Path::new(STATE_DAT)),
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

        // Resolve per-anim scale + offset. Present in the AnimationEntry
        // list → use those. Absent (e.g. fallback anim or a testing-only
        // override that isn't in the config) → CSS defaults (1.0, 0).
        let (scale, offset_y_vh) = stage_params(&cfg, &anim_path);

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
        let rendered = render_html(
            &template,
            &cfg,
            &anim_url,
            countdown,
            preview,
            scale,
            offset_y_vh,
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

        // NB : strip_window_frame + disable_dwm_nc_rendering are NOT
        // called on the popup. It's fullscreen borderless — no bord
        // visible by definition, no DWM shadow either. Both calls
        // were observed to interfere with WebView2's DirectComposition
        // transparency, forcing wry to render an opaque white
        // background. Fullscreen + with_transparent(true) alone
        // suffices for the popup.

        let proxy = event_loop.create_proxy();
        let protocol_resources = resources_dir.clone();
        let protocol_rendered = rendered.clone();

        // WebView2 user data folder under <exe_dir>/Data/WebView2/ so
        // Edge WebView2 workers write their cache alongside the exe
        // instead of at exe-dir root. Best-effort create ; wry falls
        // back to a default location if we pass None.
        let webview2_data = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| crate::modes::install::webview2_dir(p)));
        if let Some(ref dir) = webview2_data {
            let _ = std::fs::create_dir_all(dir);
        }
        // WebContext must outlive the builder (`'a` lifetime) — bind
        // BEFORE the webview so drop order is correct.
        let mut web_ctx = wry::WebContext::new(webview2_data);

        tracing::info!("popup: building wry webview");
        let webview = wry::WebViewBuilder::new_with_web_context(&mut web_ctx)
            .with_transparent(true)
            .with_custom_protocol(
                "purrpause".to_string(),
                move |_webview_id, request| {
                    serve_custom_protocol(
                        request,
                        &protocol_resources,
                        &protocol_rendered,
                    )
                },
            )
            .with_url("purrpause://localhost/popup.html")
            .with_ipc_handler(move |request| {
                if request.body() == "dismiss" {
                    let _ = proxy.send_event(UserEvent::Dismiss);
                }
            })
            .build(&window)
            .context("build wry webview")?;
        tracing::info!("popup: wry webview built");

        // Silence the unused-var lint : the webview must live for the
        // event loop's duration ; dropping it destroys the child hwnd.
        let _webview = webview;
        // web_ctx must outlive the event loop too (WebView2 keeps a
        // handle to it) — hold on until the popup exits.
        let _web_ctx = web_ctx;

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
        // killing the process from Task Manager.
        let mut kb_guard = if debug {
            tracing::info!("popup: skipping keyboard hook (--debug)");
            None
        } else {
            let guard = keyboard_hook::install_keyboard_hook()
                .context("install keyboard hook")?;
            tracing::info!("popup: keyboard hook installed (post-wry init)");
            Some(guard)
        };

        // Post-countdown unlock timer. After `countdown` seconds the
        // dismiss button appears — no reason to keep the lockdown
        // (LL hook, always-on-top, focus theft) active past that point.
        // The proxy is Send ; sleeping on a dedicated thread avoids
        // adding polling to tao's message pump.
        let timer_proxy = event_loop.create_proxy();
        let timer_secs = countdown as u64;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(timer_secs));
            let _ = timer_proxy.send_event(UserEvent::CountdownExpired);
        });

        let mut countdown_expired = false;
        tracing::info!("popup: entering event loop");

        event_loop.run(move |event, _target, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::UserEvent(UserEvent::Dismiss) => {
                    tracing::info!("popup dismissed by user");
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
            "svg" => "image/svg+xml",
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
    use crate::config::{AnimationEntry, Config, RotationMode};

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
            1.0,
            0,
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
            1.0,
            0,
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
            1.0,
            0,
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
            1.0,
            0,
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
            1.0,
            0,
        );
        assert!(out.contains("<title>Windows Session Health (preview)</title>"));
        assert!(out.contains(r#"data-mode="preview""#));
    }

    #[test]
    fn stage_scale_and_offset_substituted() {
        let cfg = Config::default();
        let tpl = r#"<style>:root { --stage-scale: {{ STAGE_SCALE }}; --stage-offset-y: {{ STAGE_OFFSET_Y_VH }}vh; }</style>"#;
        let out = render_html(tpl, &cfg, "animations/foo.lottie", 60, false, 1.5, -12);
        assert!(out.contains("--stage-scale: 1.50"));
        assert!(out.contains("--stage-offset-y: -12vh"));
        assert!(!out.contains("{{ STAGE_SCALE }}"));
        assert!(!out.contains("{{ STAGE_OFFSET_Y_VH }}"));
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
        assert_eq!(stage_params(&cfg, "animations/unknown.lottie"), (1.0, 0));
    }
}
