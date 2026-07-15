// First screen of the config UI — passcode entry with brute-force
// throttle. Delegates the timing decisions to `Lockout` (pure state
// machine, testable on Linux) so this file stays focused on the egui
// layout.

use std::time::Instant;

use eframe::egui;

use super::lockout::{Lockout, LockoutState};
use super::Action;
use crate::config::Config;

pub struct PasscodeState {
    pub input: String,
    pub error: Option<String>,
    pub lockout: Lockout,
    pub focus_grabbed: bool,
}

impl PasscodeState {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            error: None,
            lockout: Lockout::new(),
            focus_grabbed: false,
        }
    }
}

pub fn ui(ui: &mut egui::Ui, state: &mut PasscodeState, cfg: &Config) -> Option<Action> {
    ui.vertical_centered(|ui| {
        ui.add_space(60.0);
        ui.heading("Paramètres");
        ui.add_space(6.0);
        ui.label("Entre le code parental pour continuer.");
        ui.add_space(24.0);
    });

    let now = Instant::now();
    let lock_state = state.lockout.check(now);
    // Repaint every 500 ms while locked so the countdown label ticks.
    if matches!(lock_state, LockoutState::Locked { .. }) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }

    let mut submit = false;
    ui.vertical_centered(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.input)
                .password(true)
                .desired_width(220.0)
                .hint_text("Code"),
        );
        if !state.focus_grabbed {
            resp.request_focus();
            state.focus_grabbed = true;
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        ui.add_space(10.0);
        if ui
            .add_enabled(
                matches!(lock_state, LockoutState::Open) && !state.input.is_empty(),
                egui::Button::new("Déverrouiller"),
            )
            .clicked()
        {
            submit = true;
        }
        ui.add_space(10.0);
        match lock_state {
            LockoutState::Locked { remaining } => {
                let secs = remaining.as_secs().max(1);
                ui.colored_label(
                    egui::Color32::from_rgb(220, 60, 60),
                    format!("Trop d'essais — réessaie dans {secs} s"),
                );
                // Wipe queued input so a keystroke can't slip through
                // the moment the lockout expires.
                state.input.clear();
                state.error = None;
            }
            LockoutState::Open => {
                if let Some(err) = &state.error {
                    ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                }
            }
        }
    });

    if submit && matches!(state.lockout.check(Instant::now()), LockoutState::Open) {
        match crate::password::verify(&state.input, &cfg.passcode_hash) {
            Ok(true) => {
                state.lockout.reset();
                return Some(Action::UnlockToTabs);
            }
            Ok(false) => {
                state.error = Some("Code incorrect".to_string());
                state.input.clear();
                state.lockout.record_failure(Instant::now());
            }
            Err(e) => {
                state.error = Some(format!("Erreur : {e}"));
                state.input.clear();
            }
        }
    }
    None
}
