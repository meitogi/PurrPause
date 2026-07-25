// Config schema — serialized to TOML then DPAPI-machine encrypted into
// state.dat. All user-facing strings are configurable ; every field has
// a default so a missing/corrupt file never blocks startup.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::defaults::d;

/// One pre-notification message shown in the T-N countdown widget : a
/// title line above a body line (both above the live counter).
/// Configured per palier, independently for the interval and the
/// daily-pause pre-notifs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreNotif {
    pub title: String,
    pub body: String,
}

impl<'de> Deserialize<'de> for PreNotif {
    /// Accepts BOTH the `{ title, body }` table AND a legacy bare string
    /// (pre-1.4.3 configs stored a single string per palier). A legacy
    /// string becomes the `body` with an empty `title`, so an existing
    /// state.dat upgrades in place instead of failing to deserialize
    /// (which would reset the whole config via `load_or_default`).
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Legacy(String),
            Full {
                #[serde(default)]
                title: String,
                #[serde(default)]
                body: String,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Legacy(body) => PreNotif {
                title: String::new(),
                body,
            },
            Raw::Full { title, body } => PreNotif { title, body },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    // --- Timings ---
    #[serde(default = "d::interval_hours")]
    pub interval_hours: f32,

    /// Cold-boot dispatch : if `now - last_popup >= reset_threshold_hours`
    /// the scheduler treats the child as having "missed a full cycle"
    /// and reboots the timer to `now` on service startup. Default
    /// tracks `interval_hours` — see `d::reset_threshold_hours()`.
    /// UI wiring lands in session 2 (Volet 3) ; for now the slider is
    /// absent and the value is edited by hand in state.dat.
    #[serde(default = "d::reset_threshold_hours")]
    pub reset_threshold_hours: f32,

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

    /// Quand `true` : le compteur pause-cycle décompte uniquement
    /// le temps actif (clavier + souris + gamepad, ou vidéo/audio
    /// en cours). L'inactivité ≥ 60 s incrémente `paused_secs` et
    /// décale la prochaine popup d'autant. Quand `false` : le
    /// compteur suit le temps calendaire pur, sans déduction.
    /// Défaut : `true`.
    #[serde(default = "d::count_active_time")]
    pub count_active_time: bool,

    /// Signaux comptés comme « activité » quand `count_active_time`
    /// est actif. Tous à `true` par défaut. L'UI empêche de tout
    /// décocher (le dernier signal coché est verrouillé) ; si un
    /// state.dat édité à la main les met tous à `false`, le service
    /// retombe sur le temps calendaire pur.
    #[serde(default = "d::count_signal")]
    pub count_keyboard_mouse: bool,
    /// Activité XInput (boutons / sticks / gâchettes).
    #[serde(default = "d::count_signal")]
    pub count_gamepad: bool,
    /// Session audio WASAPI active — une appli émet du son.
    #[serde(default = "d::count_signal")]
    pub count_audio: bool,
    /// Session SMTC en état « lecture » — couvre la vidéo mutée dans
    /// le lecteur/l'onglet, dont le flux audio est arrêté.
    #[serde(default = "d::count_signal")]
    pub count_media: bool,

    // --- Time authority ---
    /// When `true`, the scheduler uses `SystemTime::now()` + Windows
    /// local TZ (public / permissive mode). When `false` (default),
    /// `time_authority` queries NTP/HTTPS for real UTC and applies
    /// `daily_pause.timezone` — the anti-tamper story. Persisted in
    /// state.dat so a child toggling this off later is a config edit,
    /// not a runtime flag.
    #[serde(default = "d::use_system_clock")]
    pub use_system_clock: bool,

    // --- Daily pause (Pause midi) ---
    /// Core config for the daily lunch pause. Independent kill-switch
    /// from [`Self::disabled`] — the General-tab kill-switch scopes to
    /// intervals only ; disabling the daily pause happens via
    /// [`DailyPauseConfig::enabled`] in its own tab.
    #[serde(default)]
    pub daily_pause: DailyPauseConfig,

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

    /// Key = palier in minutes (15/10/5/1), value = title + body. The
    /// body is a countdown template ({mm}/{ss}/{total_min}) shown live
    /// under the title in the T-N widget.
    #[serde(default = "d::pre_notif_messages")]
    pub pre_notif_messages: HashMap<u32, PreNotif>,

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

/// Daily lunch pause — mandatory popup at a wall-clock H:M in a
/// persisted IANA time-zone. Independent of the interval-based
/// popups (different kill-switch, different texts, different paliers).
///
/// Paliers `[15, 10, 5]` are fixed per spec ; the `HashMap` key set
/// is those three values. The pre-notif texts are user-editable via
/// the Messages tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyPauseConfig {
    /// Kill-switch scoped to the daily pause only. Default `true` —
    /// the pause fires unless the child (or admin) explicitly turns
    /// it off. Independent of [`Config::disabled`], which scopes to
    /// interval popups.
    #[serde(default = "d::daily_pause_enabled")]
    pub enabled: bool,

    /// IANA time-zone name — default `"Europe/Paris"`. Persisted so a
    /// child changing Windows TZ cannot shift the pause hour. Invalid
    /// values fall back to Europe/Paris at read time (see
    /// [`crate::time_authority::in_configured_tz`]).
    #[serde(default = "d::daily_pause_timezone")]
    pub timezone: String,

    /// Wall-clock hour (0..=23) in [`Self::timezone`]. Default 12.
    #[serde(default = "d::daily_pause_hour")]
    pub hour: u8,
    /// Wall-clock minute (0..=59) in [`Self::timezone`]. Default 0.
    #[serde(default = "d::daily_pause_minute")]
    pub minute: u8,

    /// Duration of the daily-pause popup before the dismiss button
    /// becomes clickable. Default 30. UI slider clamps to 5..=120
    /// (session 3).
    #[serde(default = "d::daily_pause_duration_minutes")]
    pub duration_minutes: u32,

    /// Popup title — independent of [`Config::popup_title`] so the
    /// daily pause reads distinctly (« Pause midi » vs the interval's
    /// « C'est l'heure de la pause »).
    #[serde(default = "d::daily_pause_popup_title")]
    pub popup_title: String,
    /// Popup subtitle — pairs with [`Self::popup_title`].
    #[serde(default = "d::daily_pause_popup_subtitle")]
    pub popup_subtitle: String,

    /// Dismiss-button label — independent of
    /// [`Config::dismiss_button_label`] so the daily pause reads
    /// distinctly (« J'ai fini de manger » vs the interval's
    /// « J'ai fait ma pause »).
    #[serde(default = "d::daily_pause_dismiss_button_label")]
    pub dismiss_button_label: String,

    /// Pre-notification messages keyed by palier minutes. Keys are
    /// always `{15, 10, 5}` — the palier set is fixed for the daily
    /// pause (unlike the interval paliers). Missing keys mean the
    /// countdown widget shows an empty message ; use the defaults.
    #[serde(default = "d::daily_pause_pre_notif_messages")]
    pub pre_notif_messages: HashMap<u32, PreNotif>,
}

impl Default for DailyPauseConfig {
    fn default() -> Self {
        // Mirror the `Config::default` idiom : deserialize from an
        // empty TOML doc so every `#[serde(default = "d::…")]` fires.
        toml::from_str("").expect("empty toml deserializes to DailyPauseConfig default")
    }
}
