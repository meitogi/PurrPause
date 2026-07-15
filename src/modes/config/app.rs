// Passcode-gated multi-tab config UI (egui / eframe wgpu backend).
//
// Flow : first frame shows a passcode prompt (`passcode` module) ; on
// verify success the user lands on `tabs::ui` — 6 tab bar with a
// persistent bottom bar exposing [Enregistrer] + [Fermer]. Every edit
// mutates an in-memory `Config` copy ; save writes state.dat + sends
// `Message::Reload` to the service pipe.
//
// The window is user-facing and shows in the taskbar (no toolwindow
// bit, no topmost). WebView2 pre-flight is skipped in main.rs — egui
// doesn't need it.

use std::path::Path;

use anyhow::{anyhow, Result};
use eframe::egui;

use crate::config::Config;
use crate::modes::install::STATE_DAT;

use super::decoration::{load_cat_icon, load_watermark_rgba, paint_watermark, spawn_local};
use super::passcode::{self, PasscodeState};
use super::tabs::{self, TabsAction, TabsState, UninstallState};

const WINDOW_W: f32 = 640.0;
const WINDOW_H: f32 = 540.0;
const MIN_W: f32 = 560.0;
const MIN_H: f32 = 460.0;

/// The config UI is parent-facing and should NOT reuse the popup's
/// camouflaged title (`Config::popup_window_title`, defaults to
/// "Windows Session Health"). Uses a distinct, explicit title so the
/// window is unambiguously identifiable in the taskbar / Alt-Tab.
const CONFIG_WINDOW_TITLE: &str = "PurrPause — Paramètres";

pub fn run() -> Result<()> {
    tracing::info!("config UI: run() entered");
    let cfg = crate::config::load_or_default(Path::new(STATE_DAT));
    tracing::info!(
        passcode_hash_len = cfg.passcode_hash.len(),
        anims = cfg.animations.len(),
        "config UI: state.dat loaded"
    );
    if cfg.passcode_hash.is_empty() {
        // Wizard never ran → nothing to verify against. Under
        // windows_subsystem="windows" a plain anyhow::bail! is
        // invisible ; surface a MessageBox so the user sees what
        // happened.
        show_error_dialog(
            "Configuration verrouillée",
            "Aucun code parental n'est configuré (state.dat vide).\n\nLance à nouveau le premier démarrage : supprime C:\\ProgramData\\DiagnosticsCache\\state.dat puis relance SystemHealthAgent.exe.",
        );
        anyhow::bail!("no passcode configured");
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(CONFIG_WINDOW_TITLE)
        .with_inner_size([WINDOW_W, WINDOW_H])
        .with_min_inner_size([MIN_W, MIN_H])
        .with_resizable(true)
        // Ensure the window opens focused. Combined with the install flow's
        // AllowSetForegroundWindow(ASFW_ANY) call before spawn, this works
        // around Windows' anti-focus-stealing policy on parent→child spawn.
        .with_active(true);

    if let Some(icon) = load_cat_icon() {
        viewport = viewport.with_icon(icon);
        tracing::info!("config UI: cat icon loaded");
    } else {
        tracing::info!("config UI: no cat icon (resources/cat-icon.png missing or invalid)");
    }
    #[cfg(windows)]
    if let Some((x, y)) = center_on_primary_monitor_logical() {
        // Sanity check : reject positions that would place the window
        // entirely off-screen (any monitor query weirdness) — let
        // eframe use its default centering instead.
        if x.is_finite() && y.is_finite() && x >= -50.0 && y >= -50.0 && x < 20000.0 && y < 20000.0 {
            viewport = viewport.with_position([x, y]);
            tracing::info!(x, y, "config UI: computed position");
        } else {
            tracing::warn!(x, y, "config UI: position out of range, using eframe default");
        }
    } else {
        tracing::info!("config UI: monitor query failed, using eframe default position");
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let watermark_bytes = load_watermark_rgba();

    tracing::info!("config UI: calling eframe::run_native");
    let run_result = eframe::run_native(
        CONFIG_WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            tracing::info!("config UI: eframe CreationContext fired");

            // The config UI runs elevated (inherited UAC token from the
            // double-clicked exe). Explorer typically runs as the normal
            // user — Windows UIPI blocks WM_DROPFILES from lower-integrity
            // sources by default. Whitelist the 3 messages that carry
            // drag-drop / clipboard cross-integrity so the Animations
            // tab's drag-drop works.
            #[cfg(windows)]
            unblock_drag_drop_uipi(cc);

            let watermark_tex = watermark_bytes.as_ref().map(|(rgba, w, h)| {
                let image = egui::ColorImage::from_rgba_unmultiplied([*w, *h], rgba);
                cc.egui_ctx
                    .load_texture("cat-watermark", image, egui::TextureOptions::LINEAR)
            });
            Ok(Box::new(ConfigApp::new(cfg, watermark_tex)))
        }),
    );
    match run_result {
        Ok(()) => {
            tracing::info!("config UI: eframe returned Ok - window closed");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "config UI: eframe::run_native failed");
            show_error_dialog(
                "Erreur config UI",
                &format!("Impossible d'ouvrir la fenêtre de configuration :\n\n{e}"),
            );
            Err(anyhow!("eframe: {e}"))
        }
    }
}

#[cfg(windows)]
fn show_error_dialog(title: &str, body: &str) {
    use std::iter::once;
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
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

#[cfg(not(windows))]
fn show_error_dialog(_title: &str, _body: &str) {}

/// Signals from any per-screen ui() fn back to the top-level state
/// machine below. Kept flat (no nested enums per screen) so pattern
/// matching in `ConfigApp::ui` stays short.
pub enum Action {
    UnlockToTabs,
    Close,
    ShellUninstall,
    BackToTabs,
}

enum Screen {
    Passcode(PasscodeState),
    Tabs(TabsState),
    Uninstall(UninstallState),
}

struct ConfigApp {
    cfg: Config,
    screen: Screen,
    watermark: Option<egui::TextureHandle>,
    focus_requested: bool,
}

impl ConfigApp {
    fn new(cfg: Config, watermark: Option<egui::TextureHandle>) -> Self {
        Self {
            cfg,
            screen: Screen::Passcode(PasscodeState::new()),
            watermark,
            focus_requested: false,
        }
    }
}

impl eframe::App for ConfigApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // First-frame focus request : belt-and-braces alongside
        // ViewportBuilder::with_active(true). Windows' anti-focus-stealing
        // policy sometimes rejects with_active — the explicit Focus command
        // succeeds when the install-flow's ASFW_ANY grant is still fresh.
        if !self.focus_requested {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            self.focus_requested = true;
        }
        // The Tabs screen has its own CentralPanel inside tabs::ui.
        // The Passcode + in-config Uninstall screens need one here so
        // panel_fill paints their viewport uniformly with the Tabs bg.
        let action = match &mut self.screen {
            Screen::Passcode(state) => {
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(tex) = self.watermark.as_ref() {
                        paint_watermark(ui, tex);
                    }
                    let mut inner_action: Option<Action> = None;
                    egui::Frame::new().inner_margin(20).show(ui, |ui| {
                        inner_action = passcode::ui(ui, state, &self.cfg);
                    });
                    inner_action
                }).inner
            }
            Screen::Tabs(state) => {
                match tabs::ui(ui, state, &mut self.cfg, self.watermark.as_ref()) {
                    TabsAction::Close => Some(Action::Close),
                    TabsAction::OpenUninstall => {
                        self.screen = Screen::Uninstall(UninstallState::new());
                        None
                    }
                    TabsAction::None => None,
                }
            }
            Screen::Uninstall(state) => {
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(tex) = self.watermark.as_ref() {
                        paint_watermark(ui, tex);
                    }
                    let mut inner_action: Option<Action> = None;
                    egui::Frame::new().inner_margin(20).show(ui, |ui| {
                        inner_action = tabs::ui_uninstall(ui, state, &self.cfg);
                    });
                    inner_action
                }).inner
            }
        };

        match action {
            Some(Action::UnlockToTabs) | Some(Action::BackToTabs) => {
                self.screen = Screen::Tabs(TabsState::new());
            }
            Some(Action::Close) => {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(Action::ShellUninstall) => {
                if let Err(e) = spawn_local(&["--uninstall"]) {
                    tracing::warn!(error = %e, "failed to spawn --uninstall");
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            None => {}
        }
    }
}

/// Whitelist drag-drop / clipboard messages so an unelevated Explorer
/// can drop files onto this elevated window. Without this call the
/// WM_DROPFILES message is silently discarded by Windows' UIPI
/// (User Interface Privilege Isolation) filter.
#[cfg(windows)]
fn unblock_drag_drop_uipi(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        ChangeWindowMessageFilterEx, MSGFLT_ALLOW, WM_COPYDATA, WM_DROPFILES,
    };
    // WM_COPYGLOBALDATA is not re-exported by windows-rs 0.62 (undocumented
    // in official headers but well-known — needed for clipboard formats
    // carrying dropped-file payload). Value 0x0049 per Raymond Chen.
    const WM_COPYGLOBALDATA: u32 = 0x0049;

    let hwnd = match cc.window_handle() {
        Ok(h) => match h.as_raw() {
            RawWindowHandle::Win32(w) => HWND(w.hwnd.get() as *mut _),
            _ => return,
        },
        Err(_) => return,
    };
    unsafe {
        let a = ChangeWindowMessageFilterEx(hwnd, WM_DROPFILES, MSGFLT_ALLOW, None);
        let b = ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, None);
        let c = ChangeWindowMessageFilterEx(hwnd, WM_COPYGLOBALDATA, MSGFLT_ALLOW, None);
        tracing::info!(
            ok_dropfiles = a.is_ok(),
            ok_copydata = b.is_ok(),
            ok_copyglobal = c.is_ok(),
            "config UI: UIPI drag-drop whitelisted"
        );
    }
}

#[cfg(windows)]
fn center_on_primary_monitor_logical() -> Option<(f32, f32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe {
        let point = POINT { x: 0, y: 0 };
        let hmon = MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut info).as_bool() {
            return None;
        }
        let mut dpix: u32 = 96;
        let mut dpiy: u32 = 96;
        if GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpix, &mut dpiy).is_err() {
            return None;
        }
        let scale = (dpix as f32 / 96.0).max(0.5);
        let rc = info.rcWork;
        let mw = (rc.right - rc.left) as f32 / scale;
        let mh = (rc.bottom - rc.top) as f32 / scale;
        let mx = rc.left as f32 / scale;
        let my = rc.top as f32 / scale;
        let x = mx + (mw - WINDOW_W) * 0.5;
        let y = my + (mh - WINDOW_H) * 0.5;
        Some((x.max(0.0), y.max(0.0)))
    }
}
