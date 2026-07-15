// Tab shell + 6 tab bodies + change-password modal + uninstall screen.
// Kept in a single module for now — the tabs share the ChangePwdDialog
// state, the string_row helper, and the top-level TabsState / TabAction
// signalling, so splitting further would just create tight cross-file
// coupling with no code-size win.

use std::path::Path;
use std::time::{Duration, SystemTime};

use eframe::egui;

use super::decoration::{ingest_lottie, paint_watermark, spawn_local};
use super::Action;
use crate::config::defaults::{
    DEFAULT_COUNTDOWN_TEMPLATE, DEFAULT_DISMISS_LABEL, DEFAULT_POPUP_SUBTITLE,
    DEFAULT_POPUP_TITLE, DEFAULT_POPUP_WINDOW_TITLE, DEFAULT_WIDGET_COUNTDOWN_TEMPLATE,
    DEFAULT_WIZARD_PASSWORD_HINT, DEFAULT_WIZARD_WELCOME,
};
use crate::config::{defaults, AnimationEntry, Config, RotationMode};
use crate::modes::install::STATE_DAT;
use crate::runtime_dat;
use crate::scheduler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    General,
    Animations,
    Messages,
    Notifications,
    Security,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::General => "Général",
            Tab::Animations => "Animations",
            Tab::Messages => "Messages",
            Tab::Notifications => "Notifications",
            Tab::Security => "Sécurité",
        }
    }
}

pub struct TabsState {
    pub active: Tab,
    pub dirty: bool,
    pub change_pwd: Option<ChangePwdDialog>,
    pub confirm_close: bool,
    pub save_error: Option<String>,
    pub new_palier_buffer: u32,
}

impl TabsState {
    pub fn new() -> Self {
        Self {
            active: Tab::General,
            dirty: false,
            change_pwd: None,
            confirm_close: false,
            save_error: None,
            new_palier_buffer: 20,
        }
    }
}

pub struct UninstallState {
    pub input: String,
    pub error: Option<String>,
}

impl UninstallState {
    pub fn new() -> Self {
        Self { input: String::new(), error: None }
    }
}

pub struct ChangePwdDialog {
    pub current: String,
    pub new_pwd: String,
    pub confirm: String,
    pub error: Option<String>,
    pub focus_grabbed: bool,
}

impl ChangePwdDialog {
    pub fn new() -> Self {
        Self {
            current: String::new(),
            new_pwd: String::new(),
            confirm: String::new(),
            error: None,
            focus_grabbed: false,
        }
    }

    fn is_valid(&self) -> bool {
        let n = self.new_pwd.chars().count();
        (4..=12).contains(&n)
            && self.new_pwd.chars().all(|c| c.is_ascii_digit())
            && self.new_pwd == self.confirm
            && !self.current.is_empty()
    }
}

/// Signals from the tabs shell back to the parent screen state machine.
pub enum TabsAction {
    None,
    Close,
    OpenUninstall,
}

pub fn ui(
    ui: &mut egui::Ui,
    state: &mut TabsState,
    cfg: &mut Config,
    watermark: Option<&egui::TextureHandle>,
) -> TabsAction {
    let mut trigger_uninstall = false;
    let mut close_requested = false;

    // 1. Bottom bar — anchored to the window bottom via TopBottomPanel.
    //    Persistent across tab switches, always visible, big buttons.
    //    In egui 0.35 panels take an existing Ui (not a Context) and
    //    reserve space from it — eframe's App::ui hands us the raw
    //    window Ui, so this docks correctly to the true window bottom.
    egui::Panel::bottom("config-bottom-bar")
        .exact_size(64.0)
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let save_btn = egui::Button::new(
                    egui::RichText::new("💾  Enregistrer").size(16.0),
                )
                .min_size(egui::vec2(170.0, 40.0));
                if ui.add_enabled(state.dirty, save_btn).clicked() {
                    match crate::config::save(cfg, Path::new(STATE_DAT)) {
                        Ok(()) => {
                            state.dirty = false;
                            state.save_error = None;
                            #[cfg(windows)]
                            if let Err(e) = crate::ipc::send(crate::ipc::Message::Reload) {
                                tracing::warn!(error = %e, "IPC Reload failed (service may be stopped)");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "config::save failed");
                            state.save_error = Some(format!("Save impossible : {e}"));
                        }
                    }
                }
                if let Some(err) = &state.save_error {
                    ui.add_space(12.0);
                    ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err.as_str());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    let close_btn = egui::Button::new(
                        egui::RichText::new("Fermer").size(16.0),
                    )
                    .min_size(egui::vec2(130.0, 40.0));
                    if ui.add(close_btn).clicked() {
                        if state.dirty {
                            state.confirm_close = true;
                        } else {
                            close_requested = true;
                        }
                    }
                    // Version label, muted, right-anchored. Baked at
                    // compile time via CARGO_PKG_VERSION so the config
                    // UI always shows the exe's actual version.
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .weak()
                            .small(),
                    );
                });
            });
            ui.add_space(6.0);
        });

    // 2. Central panel — tab bar + scrollable body. The watermark
    //    is painted first so text sits on top of it.
    egui::CentralPanel::default().show(ui, |ui| {
        if let Some(tex) = watermark {
            paint_watermark(ui, tex);
        }
        egui::Frame::new().inner_margin(20).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for t in [
                    Tab::General,
                    Tab::Animations,
                    Tab::Messages,
                    Tab::Notifications,
                    Tab::Security,
                ] {
                    if ui.selectable_label(state.active == t, t.label()).clicked() {
                        state.active = t;
                    }
                }
            });
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| match state.active {
                Tab::General => ui_general(ui, cfg, &mut state.dirty),
                Tab::Animations => ui_animations(ui, cfg, &mut state.dirty),
                Tab::Messages => ui_messages(ui, cfg, &mut state.dirty),
                Tab::Notifications => {
                    ui_notifications(ui, cfg, &mut state.dirty, &mut state.new_palier_buffer)
                }
                Tab::Security => {
                    ui_security(ui, cfg, &mut state.dirty, &mut state.change_pwd, &mut trigger_uninstall);
                }
            });
        });
    });

    // 3. Confirm-close modal (in-place window overlay).
    if state.confirm_close {
        egui::Window::new("Fermer ?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Des modifications ne sont pas enregistrées. Fermer quand même ?");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Fermer sans enregistrer").clicked() {
                        close_requested = true;
                        state.confirm_close = false;
                    }
                    if ui.button("Annuler").clicked() {
                        state.confirm_close = false;
                    }
                });
            });
    }

    if trigger_uninstall {
        return TabsAction::OpenUninstall;
    }
    if close_requested {
        return TabsAction::Close;
    }
    TabsAction::None
}

// ─────────────────────────────────────────────────────────────────────
// Tab bodies
// ─────────────────────────────────────────────────────────────────────

/// Serialize the in-memory `cfg` to a per-PID temp file so a spawned
/// preview / test child sees the CURRENT (unsaved) values without
/// touching state.dat. Returns the temp path to pass as
/// `--config-override <path>`. On failure returns None and the caller
/// spawns without the override (falls back to on-disk state.dat).
///
/// The child (see `modes::popup::run`) deletes the temp file after
/// loading it.
fn write_preview_override(cfg: &Config) -> Option<std::path::PathBuf> {
    let name = format!(
        "SystemHealthAgent-preview-{}.dat",
        std::process::id(),
    );
    let path = std::env::temp_dir().join(name);
    match crate::config::save(cfg, &path) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!(error = %e, "preview override: failed to write temp state");
            None
        }
    }
}

/// Thin I/O wrapper : reads current wall clock + runtime.dat and calls
/// the pure kernel [`scheduler::preview_minutes`].
fn next_popup_preview_minutes(interval_hours: f32) -> u64 {
    let now = SystemTime::now();
    let interval = Duration::from_secs((interval_hours * 3600.0).round() as u64);
    scheduler::preview_minutes(now, runtime_dat::read(), interval)
}

/// Builds the parenthesised suffix shown after "dans X min", explaining
/// the schedule anchor. `runtime.dat` present → "(basé sur dernier popup
/// à HH:MM)" ; absent → "(depuis l'installation)".
fn preview_anchor_suffix() -> String {
    match runtime_dat::read() {
        Some(lp) => match format_local_hhmm(lp) {
            Some(s) => format!("(basé sur dernier popup à {})", s),
            None => String::from("(basé sur dernier popup)"),
        },
        None => String::from("(depuis l'installation)"),
    }
}

fn format_local_hhmm(t: SystemTime) -> Option<String> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    // Windows FILETIME = 100-ns intervals since 1601-01-01. Offset from
    // UNIX_EPOCH (1970-01-01) is 11644473600 seconds.
    let ft_hundred_ns: u64 = (secs + 11_644_473_600) * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: (ft_hundred_ns & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ft_hundred_ns >> 32) as u32,
    };
    let mut utc_st = SYSTEMTIME::default();
    unsafe { FileTimeToSystemTime(&ft, &mut utc_st).ok()? };
    let mut local_st = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc_st, &mut local_st).ok()? };
    Some(format!("{:02}:{:02}", local_st.wHour, local_st.wMinute))
}

fn ui_general(ui: &mut egui::Ui, cfg: &mut Config, dirty: &mut bool) {
    ui.heading("Général");
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        ui.label("Intervalle entre les pauses :");
        // Slider shown in minutes for finer control (test the service
        // with 1-min intervals). Storage stays f32 hours, converted
        // in/out for display so state.dat migration is untouched.
        let mut interval_min = (cfg.interval_hours * 60.0).round() as u32;
        if ui
            .add(
                egui::Slider::new(&mut interval_min, 1..=720)
                    .text("minutes")
                    .logarithmic(true),
            )
            .changed()
        {
            cfg.interval_hours = interval_min as f32 / 60.0;
            *dirty = true;
        }
    });

    // Live "prochain contrôle" preview — mirrors the service's reload
    // clamp so the user can see the effect of a slider change before
    // saving. Truthful when runtime.dat exists (usual case) ; falls
    // back to a bare `now + interval` otherwise. The anchor suffix
    // explains why the countdown can appear shorter than the interval
    // itself (session 8 D-3 anti-cheat : `next = last_popup + interval`).
    let preview_min = next_popup_preview_minutes(cfg.interval_hours);
    let anchor_suffix = preview_anchor_suffix();
    ui.label(format!(
        "Prochain contrôle : dans {} min {}",
        preview_min, anchor_suffix
    ));

    ui.horizontal(|ui| {
        ui.label("Durée d'une pause :");
        if ui
            .add(egui::Slider::new(&mut cfg.duration_minutes, 1..=30).text("minutes"))
            .changed()
        {
            *dirty = true;
        }
    });

    if ui.checkbox(&mut cfg.disabled, "Désactiver les pauses").changed() {
        *dirty = true;
    }

    ui.add_space(20.0);
    ui.separator();
    ui.add_space(10.0);
    ui.label("Test :");
    ui.horizontal_wrapped(|ui| {
        if ui.button("Déclencher une pause maintenant").clicked() {
            #[cfg(windows)]
            if let Err(e) = crate::ipc::send(crate::ipc::Message::TriggerPopupNow) {
                tracing::warn!(error = %e, "IPC TriggerPopupNow failed");
            }
        }
        if ui.button("Prévisualiser popup").clicked() {
            let mut args: Vec<String> = vec![
                "--popup".into(),
                "--preview".into(),
                "--no-debug".into(),
            ];
            if let Some(override_path) = write_preview_override(cfg) {
                args.push("--config-override".into());
                args.push(override_path.to_string_lossy().to_string());
            }
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(e) = spawn_local(&args_ref) {
                tracing::warn!(error = %e, "spawn --popup --preview failed");
            }
        }
        // Full-prod behaviour (keyboard hook + force-minimize actifs)
        // but with a 10 s countdown so the parent can verify Alt+F4 /
        // Alt+Tab / Win+D are actually blocked without waiting the
        // full duration_minutes.
        if ui.button("Test prod (10s, clavier bloqué)").clicked() {
            let mut args: Vec<String> = vec![
                "--popup".into(),
                "--no-debug".into(),
                "--test-countdown-secs".into(),
                "10".into(),
            ];
            if let Some(override_path) = write_preview_override(cfg) {
                args.push("--config-override".into());
                args.push(override_path.to_string_lossy().to_string());
            }
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(e) = spawn_local(&args_ref) {
                tracing::warn!(error = %e, "spawn --popup test-prod failed");
            }
        }
    });
}

fn ui_animations(ui: &mut egui::Ui, cfg: &mut Config, dirty: &mut bool) {
    ui.heading("Animations");
    ui.add_space(4.0);
    ui.label("Coche pour activer. Glisser-déposer un .lottie pour l'ajouter.");
    ui.add_space(6.0);
    rotation_row(ui, cfg, dirty);
    ui.add_space(8.0);

    // Accept files dropped anywhere on the window.
    let dropped: Vec<_> = ui.ctx().input(|i| i.raw.dropped_files.clone());
    for f in dropped {
        if let Some(path) = f.path {
            if path.extension().and_then(|s| s.to_str()).unwrap_or("") == "lottie" {
                match ingest_lottie(&path) {
                    Ok((dest_rel, name)) => {
                        cfg.animations.push(AnimationEntry {
                            file: dest_rel,
                            enabled: true,
                            display_name: name,
                            scale: 1.0,
                            offset_y_vh: 0,
                        });
                        *dirty = true;
                    }
                    Err(e) => tracing::warn!(error = %e, path = %path.display(), "drop ingest failed"),
                }
            }
        }
    }

    let mut delete_idx: Option<usize> = None;
    let mut test_idx: Option<usize> = None;

    for (i, anim) in cfg.animations.iter_mut().enumerate() {
        let mut row_changed = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.checkbox(&mut anim.enabled, "").changed() {
                    row_changed = true;
                }
                if ui
                    .add(egui::TextEdit::singleline(&mut anim.display_name).desired_width(180.0))
                    .changed()
                {
                    row_changed = true;
                }
                ui.label(egui::RichText::new(&anim.file).monospace().weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Supprimer").clicked() {
                        delete_idx = Some(i);
                    }
                    if ui.button("Tester").clicked() {
                        test_idx = Some(i);
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("Zoom :");
                if ui
                    .add(egui::Slider::new(&mut anim.scale, 0.5..=3.0).step_by(0.05))
                    .changed()
                {
                    row_changed = true;
                }
                ui.label("Offset Y :");
                if ui
                    .add(egui::Slider::new(&mut anim.offset_y_vh, -50..=50).text("vh"))
                    .changed()
                {
                    row_changed = true;
                }
            });
        });
        if row_changed {
            *dirty = true;
        }
    }

    if let Some(i) = test_idx {
        let file = cfg.animations[i].file.clone();
        let override_path = write_preview_override(cfg);
        let mut args: Vec<String> = vec![
            "--popup".into(),
            "--preview".into(),
            "--no-debug".into(),
            "--anim".into(),
            file.clone(),
        ];
        if let Some(p) = override_path.as_ref() {
            args.push("--config-override".into());
            args.push(p.to_string_lossy().to_string());
        }
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = spawn_local(&args_ref) {
            tracing::warn!(error = %e, anim = %file, "spawn test animation failed");
        }
    }
    if let Some(i) = delete_idx {
        cfg.animations.remove(i);
        *dirty = true;
    }

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button("Ajouter…").clicked() {
            if let Some(picked) = rfd::FileDialog::new()
                .add_filter("Lottie", &["lottie"])
                .pick_files()
            {
                for path in picked {
                    match ingest_lottie(&path) {
                        Ok((dest_rel, name)) => {
                            cfg.animations.push(AnimationEntry {
                                file: dest_rel,
                                enabled: true,
                                display_name: name,
                                scale: 1.0,
                                offset_y_vh: 0,
                            });
                            *dirty = true;
                        }
                        Err(e) => tracing::warn!(error = %e, path = %path.display(), "add lottie failed"),
                    }
                }
            }
        }
    });
}

/// Small inline rotation-mode picker rendered at the top of the
/// Animations tab (was a dedicated tab in earlier iterations — moved
/// here since it only makes sense in the context of the list).
fn rotation_row(ui: &mut egui::Ui, cfg: &mut Config, dirty: &mut bool) {
    ui.horizontal(|ui| {
        ui.label("Rotation :");
        let before = cfg.rotation_mode;
        ui.radio_value(&mut cfg.rotation_mode, RotationMode::Random, "Aléatoire");
        ui.radio_value(&mut cfg.rotation_mode, RotationMode::Sequential, "Séquentielle");
        if before != cfg.rotation_mode {
            *dirty = true;
        }
    });
}

fn ui_messages(ui: &mut egui::Ui, cfg: &mut Config, dirty: &mut bool) {
    ui.heading("Messages");
    ui.add_space(6.0);
    ui.label("Toutes les chaînes affichées à l'utilisateur.");
    ui.add_space(10.0);

    string_row(ui, "Titre fenêtre popup", &mut cfg.popup_window_title, DEFAULT_POPUP_WINDOW_TITLE, dirty);
    string_row(ui, "Titre popup", &mut cfg.popup_title, DEFAULT_POPUP_TITLE, dirty);
    string_row(ui, "Sous-titre popup", &mut cfg.popup_subtitle, DEFAULT_POPUP_SUBTITLE, dirty);
    string_row(ui, "Bouton dismiss", &mut cfg.dismiss_button_label, DEFAULT_DISMISS_LABEL, dirty);

    ui.horizontal(|ui| {
        if ui
            .add(egui::TextEdit::singleline(&mut cfg.countdown_template).desired_width(300.0))
            .changed()
        {
            *dirty = true;
        }
        if ui.button("Défaut").clicked() {
            cfg.countdown_template = DEFAULT_COUNTDOWN_TEMPLATE.to_string();
            *dirty = true;
        }
        ui.label("Compteur popup (avant dismiss)");
    });
    ui.horizontal(|ui| {
        if ui
            .add(egui::TextEdit::singleline(&mut cfg.widget_countdown_template).desired_width(300.0))
            .changed()
        {
            *dirty = true;
        }
        if ui.button("Défaut").clicked() {
            cfg.widget_countdown_template = DEFAULT_WIDGET_COUNTDOWN_TEMPLATE.to_string();
            *dirty = true;
        }
        ui.label("Compteur widget T-N (avant pause)");
    });
    ui.label(egui::RichText::new("  placeholders : {mm}, {ss}, {total_min}").weak());

    ui.add_space(6.0);
    string_row(ui, "Message d'accueil wizard", &mut cfg.wizard_welcome, DEFAULT_WIZARD_WELCOME, dirty);
    string_row(
        ui,
        "Hint mot de passe wizard",
        &mut cfg.wizard_password_hint,
        DEFAULT_WIZARD_PASSWORD_HINT,
        dirty,
    );

    ui.add_space(12.0);
    ui.label(egui::RichText::new("Messages des paliers T-N").strong());
    let mut paliers = cfg.pre_notification_minutes.clone();
    paliers.sort_unstable_by(|a, b| b.cmp(a));
    for p in paliers {
        let entry = cfg
            .pre_notif_messages
            .entry(p)
            .or_insert_with(|| format!("Pause dans {p} min"));
        ui.horizontal(|ui| {
            ui.label(format!("T-{p} min"));
            if ui
                .add(egui::TextEdit::singleline(entry).desired_width(320.0))
                .changed()
            {
                *dirty = true;
            }
            if ui.button("Défaut").clicked() {
                let d = defaults::d::pre_notif_messages()
                    .remove(&p)
                    .unwrap_or_else(|| format!("Pause dans {p} min"));
                *entry = d;
                *dirty = true;
            }
        });
    }
}

fn string_row(
    ui: &mut egui::Ui,
    label: &str,
    field: &mut String,
    default: &str,
    dirty: &mut bool,
) {
    ui.horizontal(|ui| {
        if ui
            .add(egui::TextEdit::singleline(field).desired_width(320.0))
            .changed()
        {
            *dirty = true;
        }
        if ui.button("Défaut").clicked() {
            *field = default.to_string();
            *dirty = true;
        }
        ui.label(label);
    });
}

fn ui_notifications(
    ui: &mut egui::Ui,
    cfg: &mut Config,
    dirty: &mut bool,
    new_palier_buffer: &mut u32,
) {
    ui.heading("Notifications");
    ui.add_space(6.0);
    ui.label("Paliers d'alerte avant la pause (minutes restantes).");
    ui.add_space(10.0);

    let mut delete_idx: Option<usize> = None;
    let mut test_idx: Option<u32> = None;

    for (i, palier) in cfg.pre_notification_minutes.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.label(format!("Palier #{}", i + 1));
            if ui.add(egui::DragValue::new(palier).range(1..=60).speed(0.1)).changed() {
                *dirty = true;
            }
            ui.label("min");
            if ui.button("Tester").clicked() {
                test_idx = Some(*palier);
            }
            if ui.button("Supprimer").clicked() {
                delete_idx = Some(i);
            }
        });
    }

    if let Some(p) = test_idx {
        let seconds = "60".to_string();
        let palier_s = p.to_string();
        let override_path = write_preview_override(cfg);
        let mut args: Vec<String> = vec![
            "--countdown".into(),
            seconds,
            "--palier".into(),
            palier_s,
            "--no-debug".into(),
        ];
        if let Some(op) = override_path.as_ref() {
            args.push("--config-override".into());
            args.push(op.to_string_lossy().to_string());
        }
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = spawn_local(&args_ref) {
            tracing::warn!(error = %e, palier = p, "spawn test countdown failed");
        }
    }
    if let Some(i) = delete_idx {
        cfg.pre_notification_minutes.remove(i);
        *dirty = true;
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if ui.button("Ajouter palier").clicked() {
            let v = *new_palier_buffer;
            if !cfg.pre_notification_minutes.contains(&v) {
                cfg.pre_notification_minutes.push(v);
                cfg.pre_notification_minutes.sort_unstable_by(|a, b| b.cmp(a));
                cfg.pre_notif_messages
                    .entry(v)
                    .or_insert_with(|| format!("Pause dans {v} min"));
                *dirty = true;
            }
        }
        ui.add(egui::DragValue::new(new_palier_buffer).range(1..=60));
        ui.label("min");
    });

    ui.add_space(20.0);
    ui.separator();
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Escalade sur plein écran").strong());
    ui.label(
        egui::RichText::new(
            "Paliers pour lesquels un jeu / vidéo plein écran est forcé à se minimiser. 0 = popup.",
        )
        .weak(),
    );

    let candidates: Vec<u32> = {
        let mut v = cfg.pre_notification_minutes.clone();
        v.push(0);
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.dedup();
        v
    };
    let mut minimize_set: std::collections::HashSet<u32> =
        cfg.force_minimize_paliers.iter().copied().collect();
    let mut minimize_changed = false;
    ui.horizontal_wrapped(|ui| {
        for p in candidates {
            let label = if p == 0 { "Popup".to_string() } else { format!("T-{p} min") };
            let mut on = minimize_set.contains(&p);
            if ui.checkbox(&mut on, label).changed() {
                if on {
                    minimize_set.insert(p);
                } else {
                    minimize_set.remove(&p);
                }
                minimize_changed = true;
            }
        }
    });
    if minimize_changed {
        let mut v: Vec<u32> = minimize_set.into_iter().collect();
        v.sort_unstable();
        cfg.force_minimize_paliers = v;
        *dirty = true;
    }
}

fn ui_security(
    ui: &mut egui::Ui,
    cfg: &mut Config,
    dirty: &mut bool,
    change_pwd: &mut Option<ChangePwdDialog>,
    trigger_uninstall: &mut bool,
) {
    ui.heading("Sécurité");
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        if ui.button("Changer le mot de passe…").clicked() {
            *change_pwd = Some(ChangePwdDialog::new());
        }
        if ui.button("Désinstaller…").clicked() {
            *trigger_uninstall = true;
        }
    });

    if let Some(dialog) = change_pwd.as_mut() {
        let mut close_dialog = false;
        egui::Window::new("Changer le mot de passe")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                egui::Grid::new("pwd-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Ancien :");
                    let old_resp = ui.add(
                        egui::TextEdit::singleline(&mut dialog.current)
                            .password(true)
                            .desired_width(180.0),
                    );
                    if !dialog.focus_grabbed {
                        old_resp.request_focus();
                        dialog.focus_grabbed = true;
                    }
                    ui.end_row();
                    ui.label("Nouveau :");
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.new_pwd)
                            .password(true)
                            .desired_width(180.0),
                    );
                    ui.end_row();
                    ui.label("Confirmer :");
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.confirm)
                            .password(true)
                            .desired_width(180.0),
                    );
                    ui.end_row();
                });
                if let Some(err) = &dialog.error {
                    ui.add_space(6.0);
                    ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(dialog.is_valid(), egui::Button::new("Enregistrer"))
                        .clicked()
                    {
                        match crate::password::verify(&dialog.current, &cfg.passcode_hash) {
                            Ok(true) => match crate::password::hash(&dialog.new_pwd) {
                                Ok(new_hash) => {
                                    cfg.passcode_hash = new_hash;
                                    cfg.passcode_length = dialog.new_pwd.chars().count() as u32;
                                    *dirty = true;
                                    close_dialog = true;
                                }
                                Err(e) => dialog.error = Some(format!("Hash impossible : {e}")),
                            },
                            Ok(false) => dialog.error = Some("Ancien mot de passe incorrect".to_string()),
                            Err(e) => dialog.error = Some(format!("Erreur : {e}")),
                        }
                    }
                    if ui.button("Annuler").clicked() {
                        close_dialog = true;
                    }
                });
            });
        if close_dialog {
            *change_pwd = None;
        }
    }
}

pub fn ui_uninstall(
    ui: &mut egui::Ui,
    state: &mut UninstallState,
    cfg: &Config,
) -> Option<Action> {
    ui.heading("Désinstaller");
    ui.add_space(6.0);
    ui.label(
        "Cette action va arrêter le service, supprimer la tâche planifiée et effacer le dossier de configuration.",
    );
    ui.add_space(10.0);
    ui.label("Confirme avec le code parental :");
    ui.add(
        egui::TextEdit::singleline(&mut state.input)
            .password(true)
            .desired_width(220.0)
            .hint_text("Code"),
    );
    if let Some(err) = &state.error {
        ui.add_space(6.0);
        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err.as_str());
    }
    ui.add_space(16.0);
    let mut action: Option<Action> = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!state.input.is_empty(), egui::Button::new("Confirmer la désinstallation"))
            .clicked()
        {
            match crate::password::verify(&state.input, &cfg.passcode_hash) {
                Ok(true) => {
                    action = Some(Action::ShellUninstall);
                }
                Ok(false) => state.error = Some("Code incorrect".to_string()),
                Err(e) => state.error = Some(format!("Erreur : {e}")),
            }
        }
        if ui.button("Annuler").clicked() {
            action = Some(Action::BackToTabs);
        }
    });
    action
}
