// Countdown widget — small top-right always-on-top notification rendered
// with eframe/egui. Spawned by the service at each palier boundary
// before the popup (T-15 / T-10 / T-5 / T-1). Two lines, both per-palier
// configurable via Config::pre_notif_messages[palier] : a title on top +
// a body that IS the live countdown line (the body is a template holding
// {mm}/{ss}/{total_min}, substituted every 500ms).
//
// Two pure helpers (format_countdown, resolve_palier_message) live
// outside the #[cfg(windows)] gate so they run under Linux `cargo test`.
// The event loop and window styling are Windows-only.
//
// Dismiss (× click) or auto-close at counter=0 both call
// ViewportCommand::Close. The service tracks the palier as notified in
// its `fired_paliers` set — the next palier boundary spawns a fresh
// widget on its own schedule.

// Most of this module's surface is reached only through #[cfg(windows)]
// callers, so it looks dead on the Linux dev build. Enforcement stays on
// for the shipping Windows target.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::config::{Config, PreNotif};

/// Substitutes `{mm}`, `{ss}`, `{total_min}` in the template. Unknown
/// placeholders pass through unchanged. `{ss}` is zero-padded to two
/// digits ; `{mm}` and `{total_min}` are not padded. `{total_min}` is
/// the ceiling of `remaining_seconds / 60` so a display of "1 min
/// restantes" makes sense at 61 s but not at 60 s (which is "1 min"
/// exactly).
pub fn format_countdown(template: &str, remaining_seconds: u64) -> String {
    let mm = remaining_seconds / 60;
    let ss = remaining_seconds % 60;
    let total_min = remaining_seconds.div_ceil(60);
    template
        .replace("{mm}", &mm.to_string())
        .replace("{ss}", &format!("{ss:02}"))
        .replace("{total_min}", &total_min.to_string())
}

/// Reads the palier message from either `Config::pre_notif_messages`
/// (interval mode, default) or `Config::daily_pause.pre_notif_messages`
/// (daily-pause mode, when `mode == Some("daily-pause")`). Falls back
/// to a templated default (`"Pause dans <N> min"`) if the palier is
/// missing from the map. Callers pass the raw palier value (15, 10, 5,
/// 1, or a custom user value) — the config UI will guarantee an entry
/// exists for every palier in `pre_notification_minutes`, but state.dat
/// edited by hand can drift.
pub fn resolve_palier_message(cfg: &Config, palier: u32, mode: Option<&str>) -> PreNotif {
    let source = if mode == Some("daily-pause") {
        &cfg.daily_pause.pre_notif_messages
    } else {
        &cfg.pre_notif_messages
    };
    source.get(&palier).cloned().unwrap_or_else(|| PreNotif {
        title: String::new(),
        body: format!("Pause dans {palier} min"),
    })
}

#[cfg(windows)]
pub fn run(
    seconds: u64,
    palier: u32,
    debug: bool,
    config_override: Option<String>,
    mode: Option<String>,
) -> anyhow::Result<()> {
    windows_impl::run(seconds, palier, debug, config_override, mode)
}

#[cfg(not(windows))]
pub fn run(
    _seconds: u64,
    _palier: u32,
    _debug: bool,
    _config_override: Option<String>,
    _mode: Option<String>,
) -> anyhow::Result<()> {
    anyhow::bail!("countdown mode is Windows-only")
}

#[cfg(windows)]
mod windows_impl {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime};

    use anyhow::{anyhow, Context, Result};
    use eframe::egui;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    use crate::modes::install::STATE_DAT;
    use crate::platform::win32::{fullscreen_detect, window_style};
    use crate::{runtime_dat, scheduler};

    use super::{format_countdown, resolve_palier_message};

    const WIDTH: f32 = 320.0;
    const HEIGHT: f32 = 96.0;
    const MARGIN: i32 = 20;

    // Throttle for runtime.dat + config re-reads inside ui(). Repaint stays
    // at 500ms for smooth countdown, disk sync gated to ~1s (~one read/sec
    // on a DACL'd ProgramData path) — surfaced during ARM64 dogfood.
    const SYNC_INTERVAL: Duration = Duration::from_secs(1);

    pub fn run(
        seconds: u64,
        palier: u32,
        debug: bool,
        config_override: Option<String>,
        mode: Option<String>,
    ) -> Result<()> {
        // Preview / test spawns from the config UI pass a per-PID
        // temp state file so uncommitted edits show up. Regular
        // service-spawned countdowns leave config_override None.
        let cfg = match config_override.as_ref() {
            Some(path) => {
                let cfg = crate::config::load_or_default(Path::new(path));
                let _ = std::fs::remove_file(path);
                cfg
            }
            None => crate::config::load_or_default(Path::new(STATE_DAT)),
        };

        // If this palier is opted-in for force-minimize, escalate over
        // any foreground fullscreen app BEFORE creating our own window
        // — otherwise our topmost bit competes with the game's and
        // exclusive-fullscreen games win. Debug mode skips the
        // minimize so a developer's current window isn't stolen.
        if !debug && cfg.force_minimize_paliers.contains(&palier) {
            if let Err(e) = fullscreen_detect::force_minimize_foreground_fullscreen() {
                tracing::warn!(error = ?e, palier, "force-minimize check failed");
            }
        }
        if debug {
            tracing::info!("countdown widget: --debug mode (force-minimize skipped)");
        }

        // Position top-right on the primary monitor's work area (i.e.
        // excluding the taskbar). Fallback to a plausible 1920×1080
        // origin when the monitor query fails — the widget will still
        // show, just possibly off-position on multi-monitor setups.
        //
        // eframe/winit's with_position takes LOGICAL points, but
        // GetMonitorInfoW returns PHYSICAL pixels. On Retina / high-DPI
        // displays (Parallels default is often 2x on Apple Silicon)
        // passing physical coords as logical pushes the window
        // off-screen at scale × factor. Query the monitor's DPI and
        // divide before feeding eframe.
        let (mx, my, mw, mh, scale) = match primary_monitor_bounds_and_scale() {
            Ok(v) => {
                tracing::info!(
                    mx = v.0, my = v.1, mw = v.2, mh = v.3, scale = v.4,
                    "primary monitor bounds (physical) + DPI scale"
                );
                v
            }
            Err(e) => {
                tracing::warn!(error = ?e, "monitor/DPI query failed, using 1920x1080 @ 1.0 fallback");
                (0, 0, 1920, 1080, 1.0)
            }
        };

        // Convert physical → logical for eframe.
        let mx_l = mx as f32 / scale;
        let my_l = my as f32 / scale;
        let mw_l = mw as f32 / scale;
        let x_l = mx_l + mw_l - WIDTH - MARGIN as f32;
        let y_l = my_l + MARGIN as f32;
        tracing::info!(
            x_logical = x_l, y_logical = y_l, w = WIDTH, h = HEIGHT,
            "computed widget position (logical points for eframe)"
        );
        let _unused_mh = mh;   // kept for context in the log above

        // Classic rounding : opaque window, no per-pixel alpha. The
        // egui panel paints a rounded semi-opaque rectangle inside
        // the client area — the OS window itself stays a plain
        // rectangle so the 1px top-edge shadow artifact from
        // winit's undecorated-shadow default doesn't apply.
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([WIDTH, HEIGHT])
                .with_position([x_l, y_l])
                .with_resizable(false)
                .with_decorations(false)
                .with_always_on_top(),
            ..Default::default()
        };

        let spawn_deadline = Instant::now() + Duration::from_secs(seconds);
        let pre = resolve_palier_message(&cfg, palier, mode.as_deref());
        let title = cfg.popup_window_title.clone();
        let is_daily = mode.as_deref() == Some("daily-pause");

        // Sync is enabled in prod (widget spawned by service, well-known
        // paths live). Preview mode from the Config UI passes
        // config_override which is deleted right after the initial read
        // above — subsequent re-reads would default the config and lie
        // about interval, so preview keeps the frozen-Instant behaviour.
        //
        // Daily-pause pre-notifs count down to a FLAT wall-clock moment
        // (the noon hour), NOT the interval's active-time deadline. Live
        // sync recomputes `next_popup_at` (last_popup + interval +
        // paused_secs), so leaving it on for a daily widget makes the
        // counter drift as if the pause moved with active time — the
        // reported bug. Disable sync in daily mode so the frozen
        // spawn_deadline (the correct wall-clock seconds_until the
        // service passed) is authoritative.
        let sync = if config_override.is_none() && !is_daily {
            Some(SyncState {
                runtime_dat_path: PathBuf::from(runtime_dat::PATH),
                config_path: PathBuf::from(STATE_DAT),
                last_synced_deadline: None,
                last_sync_at: None,
            })
        } else {
            None
        };

        tracing::info!(
            seconds, palier, monitor_mh = mh, sync_enabled = sync.is_some(),
            "countdown widget starting"
        );

        eframe::run_native(
            &title,
            options,
            Box::new(move |cc| {
                // One-shot Win32 style tweak : hide from Alt+Tab +
                // reinforce topmost. Done here (CreationContext) so
                // we don't need to run any per-frame styling
                // machinery in App::ui.
                if let Ok(handle) = cc.window_handle() {
                    if let RawWindowHandle::Win32(w) = handle.as_raw() {
                        let hwnd = HWND(w.hwnd.get() as *mut _);
                        if let Err(e) = window_style::apply_topmost_toolwindow(hwnd) {
                            tracing::warn!(error = ?e, "apply_topmost_toolwindow failed");
                        }
                        // Kill the white flash on first paint by giving
                        // the window class a solid black background
                        // brush. Windows fills the client area black on
                        // the first WM_ERASEBKGND, well before wgpu is
                        // ready to render the widget's own dark panel.
                        if let Err(e) = window_style::paint_class_black(hwnd) {
                            tracing::warn!(error = ?e, "paint_class_black failed");
                        }
                    }
                }
                Ok(Box::new(CountdownApp::new(
                    spawn_deadline,
                    pre.title,
                    pre.body,
                    sync,
                )))
            }),
        )
        .map_err(|e| anyhow!("eframe: {e}"))
    }

    // Per-tick disk-read state for the sync loop. Absent (`sync: None` on
    // CountdownApp) → preview mode / no live sync → widget behaves as it
    // did pre-b.17 (frozen wall-clock against spawn_deadline).
    struct SyncState {
        runtime_dat_path: PathBuf,
        config_path: PathBuf,
        // SystemTime deadline computed via scheduler::next_popup_at at the
        // last successful sync. Held across subsequent sync misses so a
        // transient runtime.dat unavailability (service restart, file
        // rename during smoke) doesn't collapse the display back to
        // spawn_deadline mid-widget.
        last_synced_deadline: Option<SystemTime>,
        // Instant of the last sync attempt (successful or not). Throttles
        // disk reads to ~SYNC_INTERVAL cadence.
        last_sync_at: Option<Instant>,
    }

    struct CountdownApp {
        // Wall-clock fallback frozen at spawn. Used before the first
        // successful sync, and forever in preview mode where sync is
        // deliberately disabled.
        spawn_deadline: Instant,
        title: String,
        // Countdown-template body : holds {mm}/{ss}/{total_min}, rendered
        // live under the title. Replaces the old separate static message +
        // global widget_countdown_template counter.
        body: String,
        sync: Option<SyncState>,
    }

    impl CountdownApp {
        fn new(
            spawn_deadline: Instant,
            title: String,
            body: String,
            sync: Option<SyncState>,
        ) -> Self {
            Self { spawn_deadline, title, body, sync }
        }

        // Re-read runtime.dat + config if the throttle allows. Any read
        // miss (runtime.dat absent, unreadable, or config unreadable)
        // leaves the last known synced deadline in place — the widget
        // keeps counting against the last-authoritative decision rather
        // than jumping back to spawn_deadline mid-display.
        fn refresh_synced_deadline(&mut self) {
            let Some(sync) = self.sync.as_mut() else {
                return;
            };
            let should_read = match sync.last_sync_at {
                None => true,
                Some(t) => t.elapsed() >= SYNC_INTERVAL,
            };
            if !should_read {
                return;
            }
            sync.last_sync_at = Some(Instant::now());
            let Some(state) = runtime_dat::read_at(&sync.runtime_dat_path) else {
                return;
            };
            let cfg = crate::config::load_or_default(&sync.config_path);
            sync.last_synced_deadline = Some(scheduler::next_popup_at(
                state.last_popup,
                &cfg,
                state.paused_secs,
            ));
        }

        fn remaining_seconds(&self) -> u64 {
            if let Some(sync) = self.sync.as_ref() {
                if let Some(deadline) = sync.last_synced_deadline {
                    return deadline
                        .duration_since(SystemTime::now())
                        .unwrap_or(Duration::ZERO)
                        .as_secs();
                }
            }
            self.spawn_deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
        }
    }

    impl eframe::App for CountdownApp {
        // Opaque dark background matching the panel fill : the whole
        // client area reads as a solid dark rectangle. Rounded content
        // painted on top gives the classic look without the winit
        // undecorated-shadow 1px top-edge artifact.
        fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
            [15.0 / 255.0, 15.0 / 255.0, 22.0 / 255.0, 1.0]
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.refresh_synced_deadline();
            let remaining = self.remaining_seconds();

            if remaining == 0 {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            // Refresh at 500ms so the counter advances visibly even
            // without any mouse/keyboard input.
            ui.ctx().request_repaint_after(Duration::from_millis(500));

            // Rounded semi-opaque dark panel — wgpu handles per-pixel
            // alpha so pixels outside the rounded rect stay alpha 0
            // and show the desktop through cleanly.
            let rect = ui.max_rect();
            ui.painter().rect_filled(
                rect,
                12.0,
                egui::Color32::from_rgba_premultiplied(15, 15, 22, 230),
            );

            // Content laid out inside a small inset from the rounded
            // background so nothing touches the corners.
            let content_rect = rect.shrink(12.0);

            // Two lines : title on top, then the body rendered as the live
            // countdown ({mm}/{ss}/{total_min} substituted). An empty title
            // collapses to just the countdown line.
            let body = format_countdown(&self.body, remaining);
            let mut y = 0.0;
            if !self.title.is_empty() {
                ui.painter().text(
                    content_rect.min,
                    egui::Align2::LEFT_TOP,
                    &self.title,
                    egui::FontId::proportional(16.0),
                    egui::Color32::from_rgba_premultiplied(245, 245, 250, 255),
                );
                y += 30.0;
            }
            ui.painter().text(
                content_rect.min + egui::vec2(0.0, y),
                egui::Align2::LEFT_TOP,
                body,
                egui::FontId::proportional(22.0),
                egui::Color32::from_rgb(255, 183, 77),
            );

            // Close button on top of everything, small × top-right.
            let close_size = 22.0;
            let close_rect = egui::Rect::from_min_size(
                egui::pos2(content_rect.right() - close_size, content_rect.top() - 2.0),
                egui::vec2(close_size, close_size),
            );
            if ui.put(close_rect, egui::Button::new("×").frame(false)).clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn primary_monitor_bounds_and_scale() -> Result<(i32, i32, u32, u32, f32)> {
        unsafe {
            let point = POINT { x: 0, y: 0 };
            let hmon = MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(hmon, &mut info).as_bool() {
                anyhow::bail!("GetMonitorInfoW failed");
            }
            let rc = info.rcWork;
            let w = (rc.right - rc.left) as u32;
            let h = (rc.bottom - rc.top) as u32;
            let scale = monitor_scale(hmon).unwrap_or(1.0);
            Ok((rc.left, rc.top, w, h, scale))
        }
    }

    fn monitor_scale(hmon: HMONITOR) -> Result<f32> {
        unsafe {
            let mut dpix: u32 = 96;
            let mut dpiy: u32 = 96;
            GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpix, &mut dpiy)
                .context("GetDpiForMonitor")?;
            Ok(dpix as f32 / 96.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn format_countdown_mm_ss_15_min() {
        assert_eq!(
            format_countdown("Pause dans {mm}:{ss}", 900),
            "Pause dans 15:00"
        );
    }

    #[test]
    fn format_countdown_zero_padded_seconds() {
        assert_eq!(
            format_countdown("Pause dans {mm}:{ss}", 65),
            "Pause dans 1:05"
        );
    }

    #[test]
    fn format_countdown_zero_remaining() {
        assert_eq!(
            format_countdown("Pause dans {mm}:{ss}", 0),
            "Pause dans 0:00"
        );
    }

    #[test]
    fn format_countdown_total_min_ceils() {
        // 61s → 2 min (ceil), 60s → 1 min exactly.
        assert_eq!(
            format_countdown("{total_min} min restantes", 61),
            "2 min restantes"
        );
        assert_eq!(
            format_countdown("{total_min} min restantes", 60),
            "1 min restantes"
        );
    }

    #[test]
    fn format_countdown_unknown_placeholder_passthrough() {
        assert_eq!(
            format_countdown("{unknown} {mm}", 60),
            "{unknown} 1"
        );
    }

    fn pn(title: &str, body: &str) -> PreNotif {
        PreNotif {
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn resolve_palier_message_known_palier() {
        let cfg = Config::default();
        // Default HashMap ships 15/10/5/1 ; the body is a countdown
        // template (holds {mm}/{ss}), the title differentiates urgency.
        let p15 = resolve_palier_message(&cfg, 15, None);
        assert_eq!(p15.title, "Pause bientôt");
        assert_eq!(p15.body, "Pause dans {mm}:{ss}");
        assert_eq!(resolve_palier_message(&cfg, 1, None).title, "Pause imminente");
    }

    #[test]
    fn resolve_palier_message_unknown_palier_templated_fallback() {
        let cfg = Config::default();
        let pre = resolve_palier_message(&cfg, 7, None);
        assert_eq!(pre.body, "Pause dans 7 min");
        assert_eq!(pre.title, "");
    }

    #[test]
    fn resolve_palier_message_empty_map_falls_back() {
        let mut cfg = Config::default();
        cfg.pre_notif_messages = HashMap::new();
        assert_eq!(resolve_palier_message(&cfg, 15, None).body, "Pause dans 15 min");
        assert_eq!(resolve_palier_message(&cfg, 1, None).body, "Pause dans 1 min");
    }

    #[test]
    fn resolve_palier_message_daily_pause_reads_daily_map() {
        let mut cfg = Config::default();
        cfg.daily_pause
            .pre_notif_messages
            .insert(15, pn("Repas", "Repas dans 15 min"));
        cfg.pre_notif_messages
            .insert(15, pn("Interval", "Interval-specific 15"));
        let daily = resolve_palier_message(&cfg, 15, Some("daily-pause"));
        assert_eq!(daily.title, "Repas");
        assert_eq!(daily.body, "Repas dans 15 min");
        // Interval mode still uses the interval map even when daily has an entry.
        assert_eq!(
            resolve_palier_message(&cfg, 15, None).body,
            "Interval-specific 15"
        );
    }

    #[test]
    fn resolve_palier_message_daily_pause_missing_falls_back() {
        let cfg = Config::default();
        // Daily map defaults are empty for palier 7.
        assert_eq!(
            resolve_palier_message(&cfg, 7, Some("daily-pause")).body,
            "Pause dans 7 min"
        );
    }
}
