// Assets embedded at compile time via `include_bytes!` so the shipped
// binary is a standalone drop-in — no `resources/` folder to lose or
// leak. Order of lookup at runtime :
//
//   1. Disk override :
//      - For `animations/*` names → `<exe_dir>/Data/Animations/<basename>`
//        (user drag-drop store, populated by config UI's ingest_lottie).
//      - For every other name → `<exe_dir>/resources/<name>` (dev
//        override so a parent can hot-swap popup.html / icons without
//        a rebuild).
//   2. This static table (the shipped defaults).
//
// The custom protocol handler in `modes::popup` and the icon/watermark
// loaders in `modes::config::decoration` both go through `get_asset`.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// (relative-path, bytes) — compile-time only, no allocations. The
/// paths mirror what `popup.html` fetches via the `purrpause://`
/// custom protocol (`vendor/...`, `animations/...`) plus the top-level
/// `popup.html` itself and the config UI's 2 PNGs.
pub const EMBEDDED_ASSETS: &[(&str, &[u8])] = &[
    ("popup.html", include_bytes!("../resources/popup.html")),
    (
        "animations/dance-cat.lottie",
        include_bytes!("../resources/animations/dance-cat.lottie"),
    ),
    (
        "animations/cat-sleeping-no-bg.lottie",
        include_bytes!("../resources/animations/cat-sleeping-no-bg.lottie"),
    ),
    (
        "vendor/dotlottie-wc/index.js",
        include_bytes!("../resources/vendor/dotlottie-wc/index.js"),
    ),
    (
        "vendor/dotlottie-wc/dotlottie-wc.js",
        include_bytes!("../resources/vendor/dotlottie-wc/dotlottie-wc.js"),
    ),
    (
        "vendor/dotlottie-wc/dotlottie-worker-wc.js",
        include_bytes!("../resources/vendor/dotlottie-wc/dotlottie-worker-wc.js"),
    ),
    (
        "vendor/dotlottie-wc/base-dotlottie-wc.js",
        include_bytes!("../resources/vendor/dotlottie-wc/base-dotlottie-wc.js"),
    ),
    (
        "vendor/dotlottie-wc/base-dotlottie-wc-BqyUGr__.js",
        include_bytes!("../resources/vendor/dotlottie-wc/base-dotlottie-wc-BqyUGr__.js"),
    ),
    (
        "vendor/dotlottie-wc/dist-orftbxW_.js",
        include_bytes!("../resources/vendor/dotlottie-wc/dist-orftbxW_.js"),
    ),
    (
        "vendor/dotlottie-wc/dotlottie-player.wasm",
        include_bytes!("../resources/vendor/dotlottie-wc/dotlottie-player.wasm"),
    ),
    ("cat-icon.png", include_bytes!("../resources/cat-icon.png")),
    (
        "cat-watermark.png",
        include_bytes!("../resources/cat-watermark.png"),
    ),
];

fn disk_path(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let parent = exe.parent()?;
    disk_path_under(parent, name)
}

/// Testable variant. `animations/foo.lottie` → `<parent>/Data/Animations/foo.lottie`
/// (drops the `animations/` prefix so the user drag-drop store is flat).
/// Every other name resolves under `<parent>/resources/<name>` for dev
/// hot-swap.
fn disk_path_under(parent: &Path, name: &str) -> Option<PathBuf> {
    if let Some(rest) = name.strip_prefix("animations/") {
        return Some(
            crate::modes::install::animations_dir(parent).join(rest),
        );
    }
    Some(parent.join("resources").join(name))
}

/// Runtime lookup with disk override. Returns bytes owned or borrowed,
/// or `None` if the asset isn't known at compile time AND not on disk.
pub fn get_asset(name: &str) -> Option<Cow<'static, [u8]>> {
    if let Some(path) = disk_path(name) {
        if let Ok(bytes) = std::fs::read(&path) {
            tracing::debug!(asset = %name, path = %path.display(), "asset: disk override");
            return Some(Cow::Owned(bytes));
        }
    }
    EMBEDDED_ASSETS
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| Cow::Borrowed(*v as &'static [u8]))
}

/// Convenience for callers that want owned Vec (image decoders, etc.).
pub fn get_asset_vec(name: &str) -> Option<Vec<u8>> {
    get_asset(name).map(|c| c.into_owned())
}

/// Also look for user-added assets under `<exe_dir>/resources/<sub>/`
/// that aren't in the embedded table — e.g. animations the user
/// imported via drag-drop or the file picker. Used by the popup's
/// custom protocol handler to serve arbitrary paths.
pub fn get_disk_only(name: &str) -> Option<Vec<u8>> {
    let path = disk_path(name)?;
    std::fs::read(&path).ok()
}

/// Convenience for chaining : embedded → disk-only fallback (in that
/// order). Callers usually want `get_asset` (disk-override → embedded)
/// but the popup protocol handler needs the reverse : known asset →
/// user-added file. Kept as a distinct fn to make intent explicit.
pub fn get_any(name: &str) -> Option<Vec<u8>> {
    if let Some(c) = get_asset(name) {
        return Some(c.into_owned());
    }
    get_disk_only(name)
}

#[allow(dead_code)]
pub fn embedded_paths() -> impl Iterator<Item = &'static str> {
    EMBEDDED_ASSETS.iter().map(|(k, _)| *k)
}

#[allow(dead_code)]
pub fn is_embedded(name: &str) -> bool {
    EMBEDDED_ASSETS.iter().any(|(k, _)| *k == name)
}

#[allow(dead_code)]
fn _touch_path(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn disk_path_routes_animations_to_data_animations() {
        let parent = PathBuf::from("Purr");
        let got = disk_path_under(&parent, "animations/foo.lottie").unwrap();
        assert_eq!(got, parent.join("Data").join("Animations").join("foo.lottie"));
    }

    #[test]
    fn disk_path_routes_non_animations_to_resources() {
        let parent = PathBuf::from("Purr");
        assert_eq!(
            disk_path_under(&parent, "popup.html").unwrap(),
            parent.join("resources").join("popup.html"),
        );
        assert_eq!(
            disk_path_under(&parent, "vendor/dotlottie-wc/index.js").unwrap(),
            parent.join("resources").join("vendor/dotlottie-wc/index.js"),
        );
    }

    #[test]
    fn disk_only_animations_looks_in_data_animations() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Simulate <exe_dir>/Data/Animations/foo.lottie
        let anim_dir = crate::modes::install::animations_dir(dir);
        std::fs::create_dir_all(&anim_dir).unwrap();
        std::fs::write(anim_dir.join("foo.lottie"), b"payload").unwrap();
        // Bypass current_exe by calling the testable helper directly.
        let path = disk_path_under(dir, "animations/foo.lottie").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"payload");
    }
}
