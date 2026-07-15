// Hardcoded defaults for every user-configurable field of the Config
// schema. If state.dat is missing / corrupted / older-schema, the loader
// falls back to these values via `#[serde(default = "d::…")]` per field.
//
// All French — v1 is French-only per the design constitution.

use std::collections::HashMap;

use super::schema::{AnimationEntry, RotationMode};

/// Empty by default — un-titled fullscreen borderless popup is
/// cleaner in Alt-Tab / Task Manager (no visible label). The parent
/// can re-enable a title via the Messages tab if they want the
/// camouflage / distinctive label. Preview popups append "(preview)"
/// only when this is non-empty.
pub const DEFAULT_POPUP_WINDOW_TITLE: &str = "";
pub const DEFAULT_POPUP_TITLE: &str = "C'est l'heure de la pause !";
pub const DEFAULT_POPUP_SUBTITLE: &str =
    "Prends 5 minutes pour t'étirer, boire, regarder au loin.";
pub const DEFAULT_DISMISS_LABEL: &str = "J'ai fait ma pause";
pub const DEFAULT_COUNTDOWN_TEMPLATE: &str = "Dismiss dans {mm}:{ss}";
pub const DEFAULT_WIDGET_COUNTDOWN_TEMPLATE: &str = "Pause dans {mm}:{ss}";
pub const DEFAULT_WIZARD_WELCOME: &str =
    "Bienvenue. Configure les rappels de pause avant de continuer.";
pub const DEFAULT_WIZARD_PASSWORD_HINT: &str =
    "Choisis un code numérique entre 4 et 12 chiffres. Il sera demandé pour modifier ou désinstaller.";

pub mod d {
    use super::*;

    pub fn interval_hours() -> f32 {
        2.0
    }
    pub fn duration_minutes() -> u32 {
        5
    }
    pub fn pre_notification_minutes() -> Vec<u32> {
        vec![15, 10, 5, 1]
    }
    pub fn force_minimize_paliers() -> Vec<u32> {
        // T-1 minute + popup (T=0). Escalation aligned with the
        // « friction, not fort-knox » philosophy — soft widgets at
        // T-15/T-10/T-5, hard interruption at T-1 and the popup itself.
        vec![1, 0]
    }
    pub fn rotation_mode() -> RotationMode {
        RotationMode::Random
    }
    pub fn popup_window_title() -> String {
        DEFAULT_POPUP_WINDOW_TITLE.to_string()
    }
    pub fn popup_title() -> String {
        DEFAULT_POPUP_TITLE.to_string()
    }
    pub fn popup_subtitle() -> String {
        DEFAULT_POPUP_SUBTITLE.to_string()
    }
    pub fn dismiss_button_label() -> String {
        DEFAULT_DISMISS_LABEL.to_string()
    }
    pub fn countdown_template() -> String {
        DEFAULT_COUNTDOWN_TEMPLATE.to_string()
    }
    pub fn widget_countdown_template() -> String {
        DEFAULT_WIDGET_COUNTDOWN_TEMPLATE.to_string()
    }
    pub fn pre_notif_messages() -> HashMap<u32, String> {
        let mut m = HashMap::new();
        m.insert(15, "Prochaine pause dans 15 min".to_string());
        m.insert(10, "Plus que 10 min avant la pause".to_string());
        m.insert(5, "Pause dans 5 min !".to_string());
        m.insert(1, "Pause dans 1 min !".to_string());
        m
    }
    pub fn wizard_welcome() -> String {
        DEFAULT_WIZARD_WELCOME.to_string()
    }
    pub fn wizard_password_hint() -> String {
        DEFAULT_WIZARD_PASSWORD_HINT.to_string()
    }
    pub fn passcode_length() -> u32 {
        6
    }
    pub fn animation_scale() -> f32 {
        1.0
    }
    pub fn animation_offset_y_vh() -> i8 {
        0
    }
    /// Default AnimationEntry list — the two `.lottie` files shipped
    /// under `<exe_dir>/resources/animations/`, with `scale` +
    /// `offset_y_vh` tuned by the parent (values mirror the
    /// `PREVIEW_ANIMS` const in `resources/popup.html`, tuned during
    /// session 1.5's browser-preview iteration).
    pub fn animations() -> Vec<AnimationEntry> {
        // Opaque `embed:*` identifiers rather than filesystem paths —
        // signal that these are baked into the binary. The popup's
        // `resolve_animation_url` maps them to the embedded asset
        // path. User-added animations use plain relative paths
        // (`animations/foo.lottie`).
        vec![
            AnimationEntry {
                file: "embed:dance-cat".to_string(),
                enabled: true,
                display_name: "Chat qui danse".to_string(),
                scale: 1.5,
                offset_y_vh: 0,
            },
            AnimationEntry {
                file: "embed:sleep-cat".to_string(),
                enabled: true,
                display_name: "Chat qui dort".to_string(),
                scale: 2.6,
                offset_y_vh: -11,
            },
        ]
    }
}
