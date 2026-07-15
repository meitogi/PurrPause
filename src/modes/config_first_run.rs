// First-run wizard — passcode-setting screen only.
//
// Design constraint : `windows_subsystem = "windows"` means there's no
// console when the binary is double-clicked, so stdin prompts are out.
// egui/eframe is already in the dep graph. Session 6 extends this into
// the full multi-step wizard (welcome / timings / animations / done) —
// this session ships only what the install flow needs to prove the
// spawn-and-block IPC handshake.
//
// Exit codes :
//   0 — passcode saved, install flow proceeds
//   1 — user cancelled (X button or Cancel) → install flow triggers rollback

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use eframe::egui;

use crate::modes::install::STATE_DAT;

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Cancelled,
    Saved,
}

pub fn run() -> Result<()> {
    let outcome = Arc::new(Mutex::new(Outcome::Cancelled));
    let outcome_clone = outcome.clone();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([480.0, 280.0])
        .with_resizable(false);
    if let Some(icon) = crate::modes::config::decoration::load_cat_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Windows Session Health Service — First run",
        options,
        Box::new(move |_cc| Ok(Box::new(WizardApp::new(outcome_clone)))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))?;

    let final_outcome = *outcome.lock().unwrap();
    match final_outcome {
        Outcome::Saved => {
            tracing::info!("wizard: passcode saved, exiting 0");
            Ok(())
        }
        Outcome::Cancelled => {
            tracing::warn!("wizard: cancelled by user, exiting 1");
            // Propagate as an error so main() maps to ExitCode::FAILURE,
            // which the install flow reads as "cancelled → rollback".
            Err(anyhow!("wizard cancelled"))
        }
    }
}

struct WizardApp {
    passcode: String,
    confirm: String,
    error: Option<String>,
    outcome: Arc<Mutex<Outcome>>,
    focus_grabbed: bool,
}

impl WizardApp {
    fn new(outcome: Arc<Mutex<Outcome>>) -> Self {
        Self {
            passcode: String::new(),
            confirm: String::new(),
            error: None,
            outcome,
            focus_grabbed: false,
        }
    }

    fn can_continue(&self) -> bool {
        let n = self.passcode.chars().count();
        (4..=12).contains(&n)
            && self.passcode.chars().all(|c| c.is_ascii_digit())
            && self.passcode == self.confirm
    }

    fn save(&mut self) -> Result<()> {
        let hash = crate::password::hash(&self.passcode)?;
        let path = Path::new(STATE_DAT);
        let mut cfg = crate::config::load_or_default(path);
        cfg.passcode_hash = hash;
        cfg.passcode_length = self.passcode.chars().count() as u32;
        crate::config::save(&cfg, path)?;
        Ok(())
    }
}

impl eframe::App for WizardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Wrap in CentralPanel so the default panel_fill actually paints
        // the viewport bg — bare App::ui + Frame::new() would show the
        // wgpu clear color (near-black), inconsistent with the Config UI
        // which uses CentralPanel and paints the default dark gray.
        egui::CentralPanel::default().show(ui, |ui| {
        egui::Frame::new().inner_margin(24).show(ui, |ui| {
            ui.heading("Choose your passcode");
            ui.add_space(6.0);
            ui.label("Numeric, 4 to 12 digits. Needed to open settings or uninstall.");
            ui.add_space(20.0);

            egui::Grid::new("passcode-grid")
                .num_columns(2)
                .spacing([12.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Passcode :");
                    let passcode_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.passcode)
                            .password(true)
                            .desired_width(200.0),
                    );
                    // Auto-focus the first field on the first frame — user
                    // can start typing immediately without clicking.
                    if !self.focus_grabbed {
                        passcode_resp.request_focus();
                        self.focus_grabbed = true;
                    }
                    ui.end_row();

                    ui.label("Confirm :");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.confirm)
                            .password(true)
                            .desired_width(200.0),
                    );
                    ui.end_row();
                });

            if let Some(err) = &self.error {
                ui.add_space(10.0);
                ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
            }

            ui.add_space(24.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.can_continue(), egui::Button::new("Continue"))
                    .clicked()
                {
                    match self.save() {
                        Ok(()) => {
                            // Grant ASFW_ANY BEFORE closing : the wizard is
                            // the foreground process right now, so the call
                            // succeeds and lets the install-flow parent
                            // (or its next-spawned config UI child) take
                            // foreground when the wizard exits.
                            #[cfg(windows)]
                            unsafe {
                                use windows::Win32::UI::WindowsAndMessaging::{
                                    AllowSetForegroundWindow, ASFW_ANY,
                                };
                                let _ = AllowSetForegroundWindow(ASFW_ANY);
                            }
                            *self.outcome.lock().unwrap() = Outcome::Saved;
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(e) => self.error = Some(format!("Save failed : {e}")),
                    }
                }
                ui.add_space(8.0);
                if ui.button("Cancel").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
        }); // CentralPanel
    }
}
