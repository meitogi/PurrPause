// Config schema — serialized to TOML then DPAPI-machine encrypted into
// state.dat. All user-facing strings are configurable ; every field has
// a default so a missing/corrupt file never blocks startup.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::defaults::d;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    // --- Timings ---
    #[serde(default = "d::interval_hours")]
    pub interval_hours: f32,

    #[serde(default = "d::duration_minutes")]
    pub duration_minutes: u32,

    #[serde(default = "d::pre_notification_minutes")]
    pub pre_notification_minutes: Vec<u32>,

    /// Paliers where the widget spawn should force-minimize any
    /// foreground exclusive-fullscreen window before rendering.
    /// Values : `15`, `10`, `5`, `1` for the countdown widgets ;
    /// `0` for the popup itself. Default = `[1, 0]` — escalate at
    /// T-1 and guarantee the popup is visible.
    #[serde(default = "d::force_minimize_paliers")]
    pub force_minimize_paliers: Vec<u32>,

    // --- Animations ---
    /// Populated from `d::animations()` with the two shipped assets
    /// when state.dat is fresh — user adds/removes via the config UI.
    #[serde(default = "d::animations")]
    pub animations: Vec<AnimationEntry>,

    #[serde(default = "d::rotation_mode")]
    pub rotation_mode: RotationMode,

    // --- User-facing strings ---
    /// OS-level window title. Leaks via Task Manager / accessibility
    /// APIs even though the popup is borderless — keep bland.
    #[serde(default = "d::popup_window_title")]
    pub popup_window_title: String,

    #[serde(default = "d::popup_title")]
    pub popup_title: String,

    #[serde(default = "d::popup_subtitle")]
    pub popup_subtitle: String,

    #[serde(default = "d::dismiss_button_label")]
    pub dismiss_button_label: String,

    /// Compte à rebours affiché DANS le popup (temps avant que le bouton
    /// dismiss ne soit activable). Placeholders : `{mm}`, `{ss}`, `{total_min}`.
    #[serde(default = "d::countdown_template")]
    pub countdown_template: String,

    /// Compte à rebours affiché DANS le widget T-N top-right (temps
    /// restant avant que la pause fire). Placeholders identiques.
    #[serde(default = "d::widget_countdown_template")]
    pub widget_countdown_template: String,

    /// Key = palier in minutes (15/10/5), value = user-visible string.
    #[serde(default = "d::pre_notif_messages")]
    pub pre_notif_messages: HashMap<u32, String>,

    #[serde(default = "d::wizard_welcome")]
    pub wizard_welcome: String,

    #[serde(default = "d::wizard_password_hint")]
    pub wizard_password_hint: String,

    // --- Security + state ---
    #[serde(default = "d::passcode_length")]
    pub passcode_length: u32,

    /// Argon2 hash of the numeric passcode. Empty string until the
    /// first-run wizard writes one — never plaintext on disk.
    #[serde(default)]
    pub passcode_hash: String,

    #[serde(default)]
    pub disabled: bool,

    /// Unix-epoch seconds stamp of the moment `fresh_install` wrote the
    /// initial state.dat. Sentinel `0` = unknown (migrated 0.6.x install
    /// OR state.dat manually deleted). Used by the scheduler to detect
    /// a *genuine* fresh install and grant the "full first interval"
    /// grace instead of the anti-cheat cold-start shift. Kept as `u64`
    /// rather than `SystemTime` because the `toml` crate has no native
    /// serde bridge for it - epoch seconds round-trip trivially.
    #[serde(default)]
    pub first_install_at_epoch_secs: u64,
}

impl Config {
    /// Interpret [`Self::first_install_at_epoch_secs`] as a `SystemTime`.
    /// Returns [`std::time::UNIX_EPOCH`] on the sentinel value `0`.
    pub fn first_install_at(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(self.first_install_at_epoch_secs)
    }
}

impl Default for Config {
    fn default() -> Self {
        // Deserialize from an empty TOML doc so every #[serde(default)]
        // fires — single source of truth for defaults.
        toml::from_str("").expect("empty toml deserializes to Config default")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimationEntry {
    /// Relative path under `<exe_dir>/animations/`.
    pub file: String,
    pub enabled: bool,
    pub display_name: String,
    /// CSS `transform: scale(N)` applied to the Lottie stage.
    /// `1.0` = native size. Range clamped in the config UI to `0.5..=3.0`.
    #[serde(default = "d::animation_scale")]
    pub scale: f32,
    /// CSS `transform: translateY(N vh)` applied to the Lottie stage.
    /// Negative = raise (cat moves toward the ceiling), positive = lower.
    /// Range clamped in the config UI to `-50..=50`.
    #[serde(default = "d::animation_offset_y_vh")]
    pub offset_y_vh: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RotationMode {
    Random,
    Sequential,
}
