// Sandbox mode : minimal isolated diagnostic window. Zero dependency
// on state.dat / IPC / wry / keyboard hook / force-minimize. Each
// preset tunes one dimension of the window creation stack so a smoke
// tester can figure out which combination Parallels' virtualized
// graphics stack accepts.
//
// Usage :
//   SystemHealthAgent.exe --sandbox --try <preset>
//
// Presets (case-insensitive) :
//   plain                       Opaque 500x300, decorated, top-left.
//   transparent                 with_transparent(true).
//   borderless                  with_decorations(false).
//   borderless-transparent      Both.
//   topmost                     with_always_on_top() + borderless + transparent.
//   fullscreen                  Fullscreen borderless opaque.
//   fullscreen-transparent      Fullscreen borderless with_transparent.
//   red-only                    Plain but fill bright red instead of dark.
//   center                      Plain but centered on primary monitor (uses
//                               eframe's default center-of-primary).
//   strip                       Borderless opaque + Win32 strip_window_frame post-create.
//   strip-transparent           Borderless transparent + strip_window_frame.
//   lwa-alpha                   Opaque window + Win32 SetLayeredWindowAttributes(alpha=217).
//                               Workaround for glutin+ANGLE transparency fail : the window is
//                               technically opaque so no alpha pixel format is requested, and
//                               DWM composites the whole buffer at 85% opacity against the desktop.
//   lwa-alpha-strong            Same but alpha=140 (~55% opacity, obvious see-through).
//   lwa-borderless              lwa-alpha + borderless + Win32 strip_window_frame (closest to
//                               the widget's target production look).
//   circle                      Borderless transparent window with a bouncing red circle.
//                               Best visual test of per-pixel alpha : if the window bg shows
//                               the desktop and ONLY the circle is red, D3D12 alpha works.
//   circle-fullscreen           Same but fullscreen (no window edges by definition — matches
//                               the popup's target production shape).
//   rounded                     Big (500x300) semi-opaque dark rounded rectangle with a
//                               bouncing circle inside. Windowed borderless transparent :
//                               tests true Aero-style per-pixel rounded corners without
//                               any DWM shadow around the round shape.
//   widget-preview              320x90 top-right dark rounded semi-opaque with label
//                               (mimicks the production countdown widget look).
//   mixed-alpha                 Windowed transparent showcase : several shapes with
//                               different alpha values (100%, 75%, 50%, 25%, 0%) to
//                               demonstrate arbitrary per-pixel transparency.
//
// All presets paint a big red square with a black cross so the window
// is unmistakable. Any preset that shows red = rendering works with
// that combination. Preset that renders as white/blank = we've found
// a combo Parallels doesn't like.

#[cfg(windows)]
pub fn run(preset: &str) -> anyhow::Result<()> {
    windows_impl::run(preset)
}

#[cfg(not(windows))]
pub fn run(_preset: &str) -> anyhow::Result<()> {
    anyhow::bail!("sandbox mode is Windows-only")
}

#[cfg(windows)]
mod windows_impl {
    use anyhow::{anyhow, Result};
    use eframe::egui;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use windows::Win32::Foundation::HWND;

    use crate::platform::win32::window_style;

    #[derive(Clone, Copy)]
    enum Style {
        /// Solid fill + diagonal cross + centered "SANDBOX OK" label.
        Cross,
        /// Fully transparent bg + bouncing circle + tiny label.
        BouncingCircle,
        /// Dark rounded semi-opaque rectangle + bouncing circle inside.
        RoundedWithCircle,
        /// Dark rounded semi-opaque rectangle + static "T-15" label,
        /// mimicking the production countdown widget.
        WidgetPreview,
        /// Multiple shapes at varying alpha values (100/75/50/25/0 %).
        MixedAlpha,
    }

    struct Cfg {
        title: &'static str,
        size: [f32; 2],
        transparent: bool,
        decorations: bool,
        always_on_top: bool,
        fullscreen: bool,
        center: bool,
        top_right: bool,
        fill: egui::Color32,
        clear: [f32; 4],
        strip_frame_win32: bool,
        /// If Some(alpha), apply WS_EX_LAYERED + SetLayeredWindowAttributes
        /// with the given whole-window alpha value (0–255).
        lwa_alpha: Option<u8>,
        /// What to render inside the window.
        style: Style,
    }

    fn defaults() -> Cfg {
        Cfg {
            title: "sandbox",
            size: [500.0, 300.0],
            transparent: false,
            decorations: true,
            always_on_top: false,
            fullscreen: false,
            center: false,
            top_right: false,
            fill: egui::Color32::from_rgb(220, 40, 40),
            clear: [0.85, 0.15, 0.15, 1.0],
            strip_frame_win32: false,
            lwa_alpha: None,
            style: Style::Cross,
        }
    }

    pub fn run(preset: &str) -> Result<()> {
        let normalized = preset.to_ascii_lowercase();
        let mut cfg = defaults();
        match normalized.as_str() {
            "plain" => { /* baseline */ }
            "transparent" => {
                cfg.transparent = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
            }
            "borderless" => {
                cfg.decorations = false;
            }
            "borderless-transparent" => {
                cfg.decorations = false;
                cfg.transparent = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
            }
            "topmost" => {
                cfg.decorations = false;
                cfg.transparent = true;
                cfg.always_on_top = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
            }
            "fullscreen" => {
                cfg.fullscreen = true;
            }
            "fullscreen-transparent" => {
                cfg.fullscreen = true;
                cfg.transparent = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
            }
            "red-only" => {
                cfg.fill = egui::Color32::from_rgb(255, 0, 0);
                cfg.clear = [1.0, 0.0, 0.0, 1.0];
            }
            "center" => {
                cfg.center = true;
            }
            "strip" => {
                cfg.decorations = false;
                cfg.strip_frame_win32 = true;
            }
            "strip-transparent" => {
                cfg.decorations = false;
                cfg.transparent = true;
                cfg.strip_frame_win32 = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
            }
            "lwa-alpha" => {
                cfg.lwa_alpha = Some(217);
            }
            "lwa-alpha-strong" => {
                cfg.lwa_alpha = Some(140);
            }
            "lwa-borderless" => {
                cfg.decorations = false;
                cfg.strip_frame_win32 = true;
                cfg.lwa_alpha = Some(217);
            }
            "circle" => {
                cfg.decorations = false;
                cfg.strip_frame_win32 = true;
                cfg.transparent = true;
                cfg.always_on_top = true;
                cfg.size = [600.0, 600.0];
                cfg.center = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
                cfg.style = Style::BouncingCircle;
            }
            "circle-fullscreen" => {
                cfg.decorations = false;
                cfg.transparent = true;
                cfg.always_on_top = true;
                cfg.fullscreen = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
                cfg.style = Style::BouncingCircle;
            }
            "rounded" => {
                cfg.decorations = false;
                cfg.strip_frame_win32 = true;
                cfg.transparent = true;
                cfg.always_on_top = true;
                cfg.size = [500.0, 300.0];
                cfg.center = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
                cfg.style = Style::RoundedWithCircle;
            }
            "widget-preview" => {
                cfg.decorations = false;
                cfg.strip_frame_win32 = true;
                cfg.transparent = true;
                cfg.always_on_top = true;
                cfg.size = [320.0, 90.0];
                cfg.top_right = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
                cfg.style = Style::WidgetPreview;
            }
            "mixed-alpha" => {
                cfg.decorations = false;
                cfg.strip_frame_win32 = true;
                cfg.transparent = true;
                cfg.always_on_top = true;
                cfg.size = [700.0, 260.0];
                cfg.center = true;
                cfg.clear = [0.0, 0.0, 0.0, 0.0];
                cfg.style = Style::MixedAlpha;
            }
            _ => {
                tracing::warn!(
                    preset,
                    "sandbox: unknown preset, falling back to 'plain'"
                );
            }
        }

        tracing::info!(
            preset = %normalized,
            transparent = cfg.transparent,
            decorations = cfg.decorations,
            always_on_top = cfg.always_on_top,
            fullscreen = cfg.fullscreen,
            strip_frame_win32 = cfg.strip_frame_win32,
            lwa_alpha = ?cfg.lwa_alpha,
            fill = ?cfg.fill,
            "sandbox: starting"
        );

        let mut viewport = egui::ViewportBuilder::default()
            .with_title(cfg.title)
            .with_inner_size(cfg.size)
            .with_resizable(false)
            .with_decorations(cfg.decorations)
            .with_transparent(cfg.transparent);
        if cfg.always_on_top {
            viewport = viewport.with_always_on_top();
        }
        if cfg.fullscreen {
            viewport = viewport.with_fullscreen(true);
        }
        if cfg.top_right {
            // Top-right of a 1440-wide logical screen (approx). Real
            // widget uses GetMonitorInfo but this is enough for a demo.
            viewport = viewport.with_position([1440.0 - cfg.size[0] - 20.0, 20.0]);
        } else if !cfg.center && !cfg.fullscreen {
            // Fixed position top-left corner-ish so we know where to look.
            viewport = viewport.with_position([120.0, 120.0]);
        }

        let options = eframe::NativeOptions {
            viewport,
            ..Default::default()
        };

        let sandbox_app_cfg = SandboxAppCfg {
            fill: cfg.fill,
            clear: cfg.clear,
            strip_frame_win32: cfg.strip_frame_win32,
            lwa_alpha: cfg.lwa_alpha,
            style: cfg.style,
        };

        eframe::run_native(
            cfg.title,
            options,
            Box::new(move |_cc| Ok(Box::new(SandboxApp::new(sandbox_app_cfg)))),
        )
        .map_err(|e| anyhow!("eframe: {e}"))
    }

    struct SandboxAppCfg {
        fill: egui::Color32,
        clear: [f32; 4],
        strip_frame_win32: bool,
        lwa_alpha: Option<u8>,
        style: Style,
    }

    struct SandboxApp {
        cfg: SandboxAppCfg,
        first_frame_logged: bool,
        strip_applied_once: bool,
        cached_hwnd: Option<isize>,
    }

    impl SandboxApp {
        fn new(cfg: SandboxAppCfg) -> Self {
            Self {
                cfg,
                first_frame_logged: false,
                strip_applied_once: false,
                cached_hwnd: None,
            }
        }
    }

    impl eframe::App for SandboxApp {
        fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
            self.cfg.clear
        }

        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            if !self.first_frame_logged {
                tracing::info!("sandbox: first ui() frame");
                self.first_frame_logged = true;
            }

            // Cache the HWND on first frame ; re-apply LWA_ALPHA
            // every subsequent frame because winit's WndProc resets
            // SetLayeredWindowAttributes on each redraw/reposition.
            if self.cached_hwnd.is_none() {
                if let Ok(handle) = frame.window_handle() {
                    if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                        let hwnd_isize = win32.hwnd.get();
                        tracing::info!(
                            hwnd = format!("0x{:x}", hwnd_isize),
                            "sandbox: HWND acquired"
                        );
                        self.cached_hwnd = Some(hwnd_isize);
                    }
                }
            }

            if let Some(hwnd_isize) = self.cached_hwnd {
                let hwnd = HWND(hwnd_isize as *mut _);
                // strip_window_frame is sticky enough : one-shot.
                if self.cfg.strip_frame_win32 && !self.strip_applied_once {
                    if let Err(e) = window_style::strip_window_frame(hwnd) {
                        tracing::warn!(error = ?e, "strip_window_frame failed");
                    }
                    if let Err(e) = window_style::disable_dwm_nc_rendering(hwnd) {
                        tracing::warn!(error = ?e, "disable_dwm_nc_rendering failed");
                    }
                    self.strip_applied_once = true;
                }
                // LWA_ALPHA gets clobbered by winit on every tick —
                // re-apply on every frame. Cheap syscall.
                if let Some(alpha) = self.cfg.lwa_alpha {
                    if let Err(e) = window_style::apply_layered_alpha(hwnd, alpha) {
                        tracing::warn!(error = ?e, alpha, "apply_layered_alpha failed");
                    }
                }
            }

            match self.cfg.style {
                Style::Cross => {
                    let panel = egui::Frame::new().fill(self.cfg.fill).inner_margin(0);
                    panel.show(ui, |ui| {
                        let rect = ui.max_rect();
                        let painter = ui.painter();
                        painter.line_segment(
                            [rect.left_top(), rect.right_bottom()],
                            egui::Stroke::new(4.0, egui::Color32::BLACK),
                        );
                        painter.line_segment(
                            [rect.right_top(), rect.left_bottom()],
                            egui::Stroke::new(4.0, egui::Color32::BLACK),
                        );
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new("SANDBOX OK")
                                    .size(32.0)
                                    .color(egui::Color32::WHITE)
                                    .strong(),
                            );
                        });
                    });
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(1000));
                }
                Style::BouncingCircle => {
                    let panel = egui::Frame::new().fill(egui::Color32::TRANSPARENT);
                    panel.show(ui, |ui| {
                        let rect = ui.max_rect();
                        let painter = ui.painter();
                        let t = ui.ctx().input(|i| i.time as f32);
                        let cx = rect.center().x + (rect.width() * 0.35) * (t * 1.7).sin();
                        let cy = rect.center().y + (rect.height() * 0.35) * (t * 2.3).cos();
                        let radius = 60.0 + 20.0 * (t * 3.0).sin();
                        painter.circle_filled(
                            egui::pos2(cx, cy),
                            radius,
                            egui::Color32::from_rgb(220, 40, 40),
                        );
                        painter.text(
                            rect.center_top() + egui::vec2(0.0, 20.0),
                            egui::Align2::CENTER_TOP,
                            "TRANSPARENT WGPU — bouncing circle",
                            egui::FontId::proportional(18.0),
                            egui::Color32::WHITE,
                        );
                    });
                    ui.ctx().request_repaint();
                }
                Style::RoundedWithCircle => {
                    // The outer "ui" area is fully transparent (window
                    // bg = alpha 0). Paint a semi-opaque dark rounded
                    // rectangle in the middle ; anything outside its
                    // corner_radius stays alpha 0 = desktop visible.
                    let rect = ui.max_rect();
                    let painter = ui.painter();
                    painter.rect_filled(
                        rect,
                        20.0, // corner radius
                        egui::Color32::from_rgba_premultiplied(15, 15, 22, 217),
                    );
                    let t = ui.ctx().input(|i| i.time as f32);
                    let cx = rect.center().x + (rect.width() * 0.28) * (t * 1.5).sin();
                    let cy = rect.center().y + (rect.height() * 0.28) * (t * 2.1).cos();
                    painter.circle_filled(
                        egui::pos2(cx, cy),
                        40.0,
                        egui::Color32::from_rgb(255, 183, 77),
                    );
                    painter.text(
                        rect.center_top() + egui::vec2(0.0, 12.0),
                        egui::Align2::CENTER_TOP,
                        "Rounded corners over transparent desktop",
                        egui::FontId::proportional(14.0),
                        egui::Color32::WHITE,
                    );
                    ui.ctx().request_repaint();
                }
                Style::MixedAlpha => {
                    // Six regions at descending alpha so you can see
                    // the desktop bleed through more and more. Any
                    // per-pixel alpha value works — wgpu D3D12
                    // gives us full control.
                    let rect = ui.max_rect();
                    let painter = ui.painter();
                    let cols = 5;
                    let w = rect.width() / cols as f32;
                    let alphas = [255u8, 192, 128, 64, 20];
                    let labels = ["100%", "75%", "50%", "25%", "8%"];
                    for i in 0..cols {
                        let x = rect.left() + w * i as f32;
                        let col_rect = egui::Rect::from_min_size(
                            egui::pos2(x, rect.top() + 40.0),
                            egui::vec2(w - 8.0, rect.height() - 60.0),
                        );
                        // Solid color at declining alpha : reds so the
                        // per-pixel alpha effect is very legible.
                        painter.rect_filled(
                            col_rect,
                            8.0,
                            egui::Color32::from_rgba_premultiplied(
                                (220 * alphas[i] as u32 / 255) as u8,
                                (40 * alphas[i] as u32 / 255) as u8,
                                (40 * alphas[i] as u32 / 255) as u8,
                                alphas[i],
                            ),
                        );
                        painter.text(
                            col_rect.center_top() + egui::vec2(0.0, -20.0),
                            egui::Align2::CENTER_BOTTOM,
                            labels[i],
                            egui::FontId::proportional(18.0),
                            egui::Color32::WHITE,
                        );
                    }
                    painter.text(
                        rect.center_top() + egui::vec2(0.0, 6.0),
                        egui::Align2::CENTER_TOP,
                        "Per-pixel alpha showcase (100% → 8%)",
                        egui::FontId::proportional(16.0),
                        egui::Color32::WHITE,
                    );
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(1000));
                }
                Style::WidgetPreview => {
                    // Prod widget preview : dark rounded rectangle
                    // 320x90 with a 2-line palier + counter mock.
                    let rect = ui.max_rect();
                    let painter = ui.painter();
                    painter.rect_filled(
                        rect,
                        12.0,
                        egui::Color32::from_rgba_premultiplied(15, 15, 22, 230),
                    );
                    painter.text(
                        rect.min + egui::vec2(14.0, 12.0),
                        egui::Align2::LEFT_TOP,
                        "Prochaine pause dans 15 min",
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_rgba_premultiplied(240, 240, 245, 255),
                    );
                    painter.text(
                        rect.min + egui::vec2(14.0, 40.0),
                        egui::Align2::LEFT_TOP,
                        "14:32",
                        egui::FontId::monospace(24.0),
                        egui::Color32::from_rgb(255, 183, 77),
                    );
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(1000));
                }
            }
        }
    }
}
