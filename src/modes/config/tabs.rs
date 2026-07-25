// Tab shell + 6 tab bodies + change-password modal + uninstall screen.
// Kept in a single module for now — the tabs share the ChangePwdDialog
// state, the string_row helper, and the top-level TabsState / TabAction
// signalling, so splitting further would just create tight cross-file
// coupling with no code-size win.

use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use chrono_tz::Tz;
use eframe::egui;

use super::decoration::{
    ingest_animation, is_supported_anim, paint_watermark, spawn_local, spawn_local_with_env,
};
use crate::time_authority;
use super::Action;
use crate::config::defaults::{
    DEFAULT_COUNTDOWN_TEMPLATE, DEFAULT_DISMISS_LABEL, DEFAULT_POPUP_SUBTITLE,
    DEFAULT_POPUP_TITLE, DEFAULT_POPUP_WINDOW_TITLE,
    DEFAULT_WIZARD_PASSWORD_HINT, DEFAULT_WIZARD_WELCOME,
};
use crate::config::{defaults, AnimationEntry, Config, PreNotif, Renderer, RotationMode};
use crate::modes::install::STATE_DAT;
use crate::runtime_dat;
use crate::scheduler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    General,
    DailyPause,
    Animations,
    Messages,
    Notifications,
    Security,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::General => "Général",
            Tab::DailyPause => "Pause midi",
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
    /// Ephemeral (never persisted) : when ticked, the Animations-tab
    /// "Tester" button spawns the popup in bench mode (--bench).
    pub bench_animations: bool,
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
            bench_animations: false,
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
                // Block Save on an invalid daily-pause IANA timezone — the
                // Pause midi tab surfaces the error inline; disabling Save
                // keeps a bad TZ from ever reaching state.dat.
                let daily_tz_valid = Tz::from_str(&cfg.daily_pause.timezone).is_ok();
                if ui.add_enabled(state.dirty && daily_tz_valid, save_btn).clicked() {
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
                    Tab::DailyPause,
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

            // Always show the scrollbar so long tabs (Messages) visibly
            // signal there is more content below the fold — otherwise the
            // cut-off reads as "nothing more here".
            egui::ScrollArea::vertical()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .show(ui, |ui| match state.active {
                Tab::General => ui_general(ui, cfg, &mut state.dirty),
                Tab::DailyPause => ui_daily_pause(ui, cfg, &mut state.dirty),
                Tab::Animations => {
                    ui_animations(ui, cfg, &mut state.dirty, &mut state.bench_animations)
                }
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
/// the pure kernel [`scheduler::preview_minutes`]. Reads `.paused_secs`
/// alongside `.last_popup` so the preview reflects active-time
/// accounting — a user idle for 10 min sees the deadline shifted 10 min
/// further out, not the wall-clock deadline that would fire "on
/// schedule" while the service is actually still counting down.
fn next_popup_preview_minutes(interval_hours: f32) -> u64 {
    let now = SystemTime::now();
    let interval = Duration::from_secs((interval_hours * 3600.0).round() as u64);
    let state = runtime_dat::read();
    let last_popup = state.as_ref().map(|s| s.last_popup);
    let paused_secs = state.as_ref().map(|s| s.paused_secs).unwrap_or(0);
    scheduler::preview_minutes(now, last_popup, interval, paused_secs)
}

/// Builds the parenthesised suffix shown after "dans X min", explaining
/// the schedule anchor. `runtime.dat` present → "(basé sur dernier popup
/// à HH:MM)" ; absent → "(depuis l'installation)".
fn preview_anchor_suffix() -> String {
    match runtime_dat::read().map(|s| s.last_popup) {
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

    // ── Cycle ──
    ui.label(egui::RichText::new("Cycle").strong());
    ui.add_space(4.0);

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
        ui.label("Reset complet si offline plus de :");
        let mut reset_min = (cfg.reset_threshold_hours * 60.0).round() as u32;
        if ui
            .add(
                egui::Slider::new(&mut reset_min, 1..=720)
                    .text("minutes")
                    .logarithmic(true),
            )
            .on_hover_text(
                "Passé ce délai sans activité (ordi éteint, en \
                 veille, service coupé), le compteur repart de zéro \
                 au redémarrage. La prochaine popup arrive après un \
                 cycle complet, pas immédiatement.",
            )
            .changed()
        {
            cfg.reset_threshold_hours = reset_min as f32 / 60.0;
            *dirty = true;
        }
    });

    // ── Comportement ──
    ui.add_space(12.0);
    ui.label(egui::RichText::new("Comportement").strong());
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.label("Durée d'une pause :");
        if ui
            .add(egui::Slider::new(&mut cfg.duration_minutes, 1..=30).text("minutes"))
            .on_hover_text(
                "Combien de temps la popup reste affichée. Après ce \
                 délai le bouton « J'ai fait ma pause » devient \
                 cliquable et l'utilisateur peut la fermer.",
            )
            .changed()
        {
            *dirty = true;
        }
    });

    if ui
        .checkbox(
            &mut cfg.count_active_time,
            "Décompter uniquement le temps d'activité",
        )
        .on_hover_text(
            "Coché (défaut) : l'intervalle se compte en temps \
             d'activité (signaux cochés ci-dessous). \
             L'inactivité ≥ 1 min met le compteur en pause.\n\
             Décoché : l'intervalle se compte en temps calendaire \
             pur, même quand tu ne fais rien.",
        )
        .changed()
    {
        *dirty = true;
    }

    // Per-signal toggles. The last checked signal is locked so the
    // set can never end up empty from the UI (an empty set would
    // devolve to wall-clock counting — see the schema doc).
    ui.add_enabled_ui(cfg.count_active_time, |ui| {
        ui.indent("activity_signals", |ui| {
            let checked = [
                cfg.count_keyboard_mouse,
                cfg.count_gamepad,
                cfg.count_audio,
                cfg.count_media,
            ]
            .iter()
            .filter(|b| **b)
            .count();
            let mut signal = |ui: &mut egui::Ui, value: &mut bool, label: &str, hover: &str| {
                let locked = *value && checked == 1;
                if ui
                    .add_enabled(!locked, egui::Checkbox::new(value, label))
                    .on_hover_text(hover)
                    .changed()
                {
                    *dirty = true;
                }
            };
            signal(
                ui,
                &mut cfg.count_keyboard_mouse,
                "Clavier / souris",
                "Toute frappe ou mouvement de souris compte comme activité.",
            );
            signal(
                ui,
                &mut cfg.count_gamepad,
                "Manette",
                "Toute activité manette (boutons, sticks, gâchettes) compte.",
            );
            signal(
                ui,
                &mut cfg.count_audio,
                "Audio en lecture",
                "Une application qui émet du son compte — vidéo, musique, jeu. \
                 Muter dans le mixeur Windows compte toujours.",
            );
            signal(
                ui,
                &mut cfg.count_media,
                "Média en lecture (même muté)",
                "Un lecteur multimédia en état « lecture » compte, même si le \
                 son est coupé dans le lecteur ou l'onglet — YouTube, VLC, \
                 Spotify…",
            );
        });
    });

    // ── Test ──
    ui.add_space(12.0);
    ui.label(egui::RichText::new("Test").strong());
    ui.add_space(4.0);
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
            // Forward the passcode hash exactly like the service does
            // (fire_popup), so the Ctrl+Shift+pincode escape can actually
            // verify from this locked test popup. Empty = no passcode set.
            if !cfg.passcode_hash.is_empty() {
                args.push("--passcode-hash".into());
                args.push(cfg.passcode_hash.clone());
            }
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(e) = spawn_local(&args_ref) {
                tracing::warn!(error = %e, "spawn --popup test-prod failed");
            }
        }
    });

    // Destructive kill-switch. Visually isolated at the very bottom
    // of the tab, framed, and tinted red when active so it never
    // reads like a routine option.
    ui.add_space(20.0);
    ui.separator();
    ui.add_space(20.0);

    let frame = if cfg.disabled {
        egui::Frame::group(ui.style())
            .fill(egui::Color32::from_rgba_premultiplied(60, 30, 20, 255))
            .stroke(egui::Stroke::new(
                1.5,
                egui::Color32::from_rgb(220, 120, 60),
            ))
    } else {
        egui::Frame::group(ui.style())
    };
    frame.show(ui, |ui| {
        if ui
            .checkbox(
                &mut cfg.disabled,
                egui::RichText::new("⚠  Désactiver les intervals").strong(),
            )
            .changed()
        {
            *dirty = true;
        }
        ui.label(
            egui::RichText::new(
                "Aucune pause d'intervalle ne se déclenchera tant que \
                 cette case est cochée.",
            )
            .weak()
            .small(),
        );
        ui.label(
            egui::RichText::new(
                "N'affecte pas la pause midi. Utilisez l'onglet Pause \
                 midi pour la désactiver.",
            )
            .weak()
            .small(),
        );
    });
}

fn ui_daily_pause(ui: &mut egui::Ui, cfg: &mut Config, dirty: &mut bool) {

    // ── Horaire ──
    ui.add_space(12.0);
    ui.label(egui::RichText::new("Horaire").strong());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Heure de déclenchement :");
        if ui
            .add(egui::DragValue::new(&mut cfg.daily_pause.hour).range(0..=23))
            .changed()
        {
            *dirty = true;
        }
        ui.label(":");
        if ui
            .add(egui::DragValue::new(&mut cfg.daily_pause.minute).range(0..=59))
            .changed()
        {
            *dirty = true;
        }
    });

    ui.add_space(8.0);
    ui.label("Durée de la pause");
    if ui
        .add(egui::Slider::new(&mut cfg.daily_pause.duration_minutes, 15..=120).text("min"))
        .on_hover_text(
            "Combien de temps la popup reste affichée avant que le \
             bouton de fermeture devienne cliquable.",
        )
        .changed()
    {
        *dirty = true;
    }

    // ── Fuseau horaire & horloge ──
    ui.add_space(12.0);
    ui.label(egui::RichText::new("Fuseau horaire").strong());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Fuseau (IANA) :");
        if ui
            .add(
                egui::TextEdit::singleline(&mut cfg.daily_pause.timezone)
                    .hint_text("Europe/Paris")
                    .desired_width(220.0),
            )
            .changed()
        {
            *dirty = true;
        }
    });
    let tz_valid = Tz::from_str(&cfg.daily_pause.timezone).is_ok();
    if !tz_valid {
        ui.colored_label(
            egui::Color32::from_rgb(220, 60, 60),
            "Fuseau IANA invalide (ex : Europe/Paris)",
        );
    }

    ui.add_space(4.0);
    if ui
        .checkbox(
            &mut cfg.use_system_clock,
            "Utiliser l'horloge Windows (désactive l'anti-triche)",
        )
        .changed()
    {
        *dirty = true;
    }
    ui.label(
        egui::RichText::new(
            "Recommandé désactivé. Activez seulement si votre réseau \
             bloque NTP + HTTPS.",
        )
        .weak()
        .small(),
    );

    // ── Live preview ──
    // Recomputes each frame from the authoritative clock; a 1 s repaint
    // request keeps it advancing even when the window is idle so a
    // day-boundary crossing flips the date without a manual interaction.
    ui.add_space(12.0);
    let now_local = time_authority::in_configured_tz(cfg, time_authority::now_authoritative(cfg));
    let next = time_authority::next_daily_pause_at(cfg, now_local);
    ui.label(
        egui::RichText::new(format!(
            "Prochaine pause midi : {}",
            next.format("%Y-%m-%d %H:%M")
        ))
        .weak(),
    );
    ui.ctx().request_repaint_after(Duration::from_secs(1));

    // ── Test ──
    ui.add_space(12.0);
    ui.label(egui::RichText::new("Test").strong());
    ui.add_space(4.0);
    // PURRPAUSE_FROZEN_TZ mirrors the service's production daily-pause
    // spawn (TZ-tamper-proof display).
    let tz = cfg.daily_pause.timezone.clone();
    // Main test popup + the three pré-notif paliers, all on one row.
    ui.horizontal_wrapped(|ui| {
        if ui.button("Tester la pause midi maintenant").clicked() {
            let mut args: Vec<String> = vec![
                "--popup".into(),
                "--no-debug".into(),
                "--mode".into(),
                "daily-pause".into(),
            ];
            // Preview the CURRENT (possibly unsaved) config like the
            // "Test prod" button — same snapshot means the popup sees the
            // same `passcode_length` (= the pincode hook's `cap`), so the
            // escape behaves identically here and there. Without it this
            // popup read state.dat and could run against a stale `cap`.
            if let Some(override_path) = write_preview_override(cfg) {
                args.push("--config-override".into());
                args.push(override_path.to_string_lossy().to_string());
            }
            // Forward the passcode hash like the service (fire_popup_daily)
            // so the pincode escape can verify from this test popup.
            if !cfg.passcode_hash.is_empty() {
                args.push("--passcode-hash".into());
                args.push(cfg.passcode_hash.clone());
            }
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(e) =
                spawn_local_with_env(&args_ref, &[("PURRPAUSE_FROZEN_TZ", tz.as_str())])
            {
                tracing::warn!(error = %e, "spawn daily-pause popup failed");
            }
        }
        ui.label("Pré-notifs :");
        for p in [15u32, 10, 5] {
            if ui.small_button(format!("T-{p}")).clicked() {
                let palier = p.to_string();
                let args = [
                    "--countdown",
                    "60",
                    "--palier",
                    palier.as_str(),
                    "--no-debug",
                    "--mode",
                    "daily-pause",
                ];
                if let Err(e) =
                    spawn_local_with_env(&args, &[("PURRPAUSE_FROZEN_TZ", tz.as_str())])
                {
                    tracing::warn!(error = %e, palier = p, "spawn daily countdown failed");
                }
            }
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Reset aujourd'hui").clicked() {
            #[cfg(windows)]
            if let Err(e) = crate::ipc::send(crate::ipc::Message::RearmDailyPause) {
                tracing::warn!(error = %e, "IPC RearmDailyPause failed");
            }
        }
        ui.label(
            egui::RichText::new(
                "Efface le marqueur « déjà déclenchée aujourd'hui » : la pause midi \
                 pourra se déclencher à nouveau le jour même.",
            )
            .weak()
            .small(),
        );
    });

    // ── Kill-switch (bottom) ──
    // Independent of the interval kill-switch on the Général tab. The
    // checkbox reads "disabled" (checked = off), but the stored field is
    // `enabled` (checked = on) — so we drive it through a negated temp
    // bool. Red-tinted frame when the daily pause is off, mirroring the
    // Général kill-switch styling. Isolated at the very bottom of the tab
    // like the Général one, behind the same space/separator/space lead-in.
    ui.add_space(20.0);
    ui.separator();
    ui.add_space(20.0);

    let mut daily_disabled = !cfg.daily_pause.enabled;
    let frame = if daily_disabled {
        egui::Frame::group(ui.style())
            .fill(egui::Color32::from_rgba_premultiplied(60, 30, 20, 255))
            .stroke(egui::Stroke::new(
                1.5,
                egui::Color32::from_rgb(220, 120, 60),
            ))
    } else {
        egui::Frame::group(ui.style())
    };
    frame.show(ui, |ui| {
        if ui
            .checkbox(
                &mut daily_disabled,
                egui::RichText::new("⚠  Désactiver la pause midi").strong(),
            )
            .changed()
        {
            cfg.daily_pause.enabled = !daily_disabled;
            *dirty = true;
        }
        ui.label(
            egui::RichText::new("Indépendant du kill-switch des intervals.")
                .weak()
                .small(),
        );
    });
}

fn ui_animations(ui: &mut egui::Ui, cfg: &mut Config, dirty: &mut bool, bench: &mut bool) {
    ui.label("Coche pour activer. Glisser-déposer une animation (.lottie, .svg, .gif, .webp, .png/.apng, .webm) pour l'ajouter.");
    ui.add_space(6.0);
    rotation_row(ui, cfg, dirty);
    ui.add_space(8.0);
    // Ephemeral toggle (not saved) : reframes the per-row "Tester"
    // button as a benchmark run instead of adding a second button.
    ui.checkbox(
        bench,
        "Benchmark : Tester échantillonne CPU/GPU/RAM pendant 10 s dans Data\\Logs\\bench.log",
    );
    ui.add_space(8.0);

    // Accept files dropped anywhere on the window.
    let dropped: Vec<_> = ui.ctx().input(|i| i.raw.dropped_files.clone());
    for f in dropped {
        if let Some(path) = f.path {
            if is_supported_anim(&path) {
                match ingest_animation(&path) {
                    Ok((dest_rel, name)) => {
                        cfg.animations.push(AnimationEntry {
                            file: dest_rel,
                            enabled: true,
                            display_name: name,
                            scale: 1.0,
                            offset_y_vh: 0,
                            renderer: Renderer::ThorvgSw,
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
                // Renderer picker — only `.lottie` has two engines (ThorVG-SW
                // vs Skottie-GPU) ; every other format is dispatched by
                // extension, so the field is meaningless there. Gate on the
                // *resolved* path so the `embed:*` defaults (which lack a
                // literal `.lottie` suffix) still count as Lottie.
                if crate::modes::popup::resolve_animation_url(&anim.file).ends_with(".lottie") {
                    ui.separator();
                    ui.label("Moteur :");
                    let before = anim.renderer;
                    egui::ComboBox::from_id_salt(("renderer", i))
                        .selected_text(match anim.renderer {
                            Renderer::ThorvgSw => "ThorVG (CPU)",
                            Renderer::SkottieGpu => "Skottie (GPU)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut anim.renderer,
                                Renderer::ThorvgSw,
                                "ThorVG (CPU)",
                            );
                            ui.selectable_value(
                                &mut anim.renderer,
                                Renderer::SkottieGpu,
                                "Skottie (GPU)",
                            );
                        });
                    if anim.renderer != before {
                        row_changed = true;
                    }
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
        if *bench {
            args.push("--bench".into());
            args.push("--bench-secs".into());
            args.push(crate::bench::DEFAULT_BENCH_SECS.to_string());
        }
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
                .add_filter("Animations", super::decoration::ANIM_EXTS)
                .pick_files()
            {
                for path in picked {
                    match ingest_animation(&path) {
                        Ok((dest_rel, name)) => {
                            cfg.animations.push(AnimationEntry {
                                file: dest_rel,
                                enabled: true,
                                display_name: name,
                                scale: 1.0,
                                offset_y_vh: 0,
                                renderer: Renderer::ThorvgSw,
                            });
                            *dirty = true;
                        }
                        Err(e) => tracing::warn!(error = %e, path = %path.display(), "add animation failed"),
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
        let default = defaults::d::pre_notif_messages()
            .remove(&p)
            .unwrap_or_else(|| PreNotif {
                title: String::new(),
                body: format!("Pause dans {p} min"),
            });
        let entry = cfg
            .pre_notif_messages
            .entry(p)
            .or_insert_with(|| default.clone());
        pre_notif_row(ui, p, entry, default, dirty);
    }

    // ── Pause midi ──
    ui.add_space(20.0);
    ui.separator();
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Pause midi").strong());
    ui.add_space(6.0);

    let dp_title = defaults::d::daily_pause_popup_title();
    string_row(ui, "Titre popup midi", &mut cfg.daily_pause.popup_title, &dp_title, dirty);
    let dp_subtitle = defaults::d::daily_pause_popup_subtitle();
    string_row(
        ui,
        "Sous-titre popup midi",
        &mut cfg.daily_pause.popup_subtitle,
        &dp_subtitle,
        dirty,
    );
    let dp_dismiss = defaults::d::daily_pause_dismiss_button_label();
    string_row(
        ui,
        "Bouton dismiss midi",
        &mut cfg.daily_pause.dismiss_button_label,
        &dp_dismiss,
        dirty,
    );

    ui.add_space(6.0);
    ui.label(egui::RichText::new("Messages des paliers T-N (pause midi)").strong());
    // Fixed palier set {15, 10, 5} — no add/remove UI (unlike interval).
    for p in [15u32, 10, 5] {
        let default = defaults::d::daily_pause_pre_notif_messages()
            .remove(&p)
            .unwrap_or_else(|| PreNotif {
                title: String::new(),
                body: format!("Pause midi dans {p} min"),
            });
        let entry = cfg
            .daily_pause
            .pre_notif_messages
            .entry(p)
            .or_insert_with(|| default.clone());
        pre_notif_row(ui, p, entry, default, dirty);
    }
}

/// Editor for one palier pre-notif : a title line + a body line (both
/// shown in the countdown widget) plus a "Défaut" reset. Shared by the
/// interval and daily-pause palier lists.
fn pre_notif_row(
    ui: &mut egui::Ui,
    palier: u32,
    entry: &mut PreNotif,
    default: PreNotif,
    dirty: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.label(format!("T-{palier} min"));
        ui.vertical(|ui| {
            if ui
                .add(
                    egui::TextEdit::singleline(&mut entry.title)
                        .desired_width(300.0)
                        .hint_text("Titre"),
                )
                .changed()
            {
                *dirty = true;
            }
            if ui
                .add(
                    egui::TextEdit::singleline(&mut entry.body)
                        .desired_width(300.0)
                        .hint_text("Corps — décompte, ex : Pause dans {mm}:{ss}"),
                )
                .changed()
            {
                *dirty = true;
            }
        });
        if ui.button("Défaut").clicked() {
            *entry = default;
            *dirty = true;
        }
    });
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
                cfg.pre_notif_messages.entry(v).or_insert_with(|| PreNotif {
                    title: String::new(),
                    body: format!("Pause dans {v} min"),
                });
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
