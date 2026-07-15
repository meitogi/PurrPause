// Cosmetic bits for the config UI :
//   * cat icon in the window's title bar / taskbar / Alt-Tab preview
//   * bottom-right cat watermark (~20 % opacity, decorative)
//   * pink-background auto-removal for the icon (keeps the character,
//     drops the pastel-pink circle in cat-icon.png)
//   * shared spawn_local helper for the "Prévisualiser popup",
//     "Tester animation" and "Désinstaller" buttons
//   * ingest_lottie : file-picker / drag-drop handler that copies +
//     sanitizes a `.lottie` into `<exe_dir>/resources/animations/`
//
// PNG decoding + auto-crop of transparent borders is Linux-runnable
// (used by unit tests below) so the crop logic is verified on every
// `cargo test`.

use std::path::Path;

use anyhow::{anyhow, Result};
use eframe::egui;

/// Best-effort loader for the config UI's cat icon. Bytes come from
/// `crate::embedded::get_asset` (disk override wins over embedded),
/// auto-crops the transparent margins so the icon fills the frame,
/// then knocks the pastel-pink background out. Missing / unreadable
/// PNG → `None` + warn log, no crash.
pub fn load_cat_icon() -> Option<egui::IconData> {
    let bytes = crate::embedded::get_asset_vec("cat-icon.png")?;
    let (mut rgba, w, h) = decode_and_autocrop_png(&bytes)
        .inspect_err(|e| tracing::warn!(error = ?e, "cat-icon: decode failed"))
        .ok()?;
    remove_pink_background(&mut rgba);
    Some(egui::IconData {
        rgba,
        width: w as u32,
        height: h as u32,
    })
}

/// Loader for the bottom-right watermark. Returns raw RGBA + dims so
/// egui can build a texture. Falls back to `cat-icon.png` when the
/// dedicated `cat-watermark.png` is absent — a single dropped PNG
/// still enables the decoration.
///
/// No pink removal here — the watermark benefits visually from the
/// pastel circle behind the cat.
pub fn load_watermark_rgba() -> Option<(Vec<u8>, usize, usize)> {
    for name in ["cat-watermark.png", "cat-icon.png"] {
        if let Some(bytes) = crate::embedded::get_asset_vec(name) {
            match decode_and_autocrop_png(&bytes) {
                Ok(v) => return Some(v),
                Err(e) => tracing::warn!(error = ?e, asset = %name, "watermark: decode failed"),
            }
        }
    }
    None
}

/// Decode PNG bytes and trim transparent margins on all four sides.
/// Preserves original pixel density inside the bounding box.
pub fn decode_and_autocrop_png(bytes: &[u8]) -> Result<(Vec<u8>, usize, usize)> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|e| anyhow!("png decode: {e}"))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let (w, h) = (w as usize, h as usize);
    let raw: Vec<u8> = img.into_raw();

    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            let alpha = raw[(y * w + x) * 4 + 3];
            if alpha > 0 {
                any = true;
                if x < min_x { min_x = x; }
                if y < min_y { min_y = y; }
                if x > max_x { max_x = x; }
                if y > max_y { max_y = y; }
            }
        }
    }
    if !any {
        return Err(anyhow!("image is fully transparent"));
    }
    let cw = max_x - min_x + 1;
    let ch = max_y - min_y + 1;
    let mut out = Vec::with_capacity(cw * ch * 4);
    for y in min_y..=max_y {
        let start = (y * w + min_x) * 4;
        let end = start + cw * 4;
        out.extend_from_slice(&raw[start..end]);
    }
    Ok((out, cw, ch))
}

/// Knock the pastel-pink circle out of `cat-icon.png`. Heuristic :
/// pink is R clearly > G and > B with R ≥ 220 and a reasonable
/// saturation (skip whites / grays / oranges). Chosen against the
/// specific asset shipped in `resources/cat-icon.png` — a soft
/// pastel around RGB(240, 200, 210). White, black, orange eyes,
/// and the muted grey brush strokes all pass through untouched.
pub fn remove_pink_background(rgba: &mut [u8]) {
    for chunk in rgba.chunks_exact_mut(4) {
        let r = chunk[0] as i32;
        let g = chunk[1] as i32;
        let b = chunk[2] as i32;
        // Only touch already-opaque pixels — an autocropped border
        // may still contain a few edge alphas.
        if chunk[3] == 0 {
            continue;
        }
        let pink_ish = r >= 220
            && g >= 170
            && g <= 225
            && b >= 180
            && b <= 235
            && r > g
            && r > b
            && (r - g) >= 15
            && (r - b) <= 60
            && (b as i32 - g as i32).abs() <= 40;
        if pink_ish {
            chunk[3] = 0;
        }
    }
}

pub fn paint_watermark(ui: &mut egui::Ui, tex: &egui::TextureHandle) {
    // Root Ui's max_rect ≈ full window client area — sufficient for a
    // decorative bottom-right stamp. Avoids the egui 0.35 Context API
    // shifts around screen_rect / available_rect.
    let screen = ui.max_rect();
    let size = tex.size_vec2();
    // Target size : ~30 % of the shorter window dimension, capped so
    // small windows don't drown in decoration. Preserve aspect ratio.
    let target_max = (screen.width().min(screen.height()) * 0.35).min(220.0);
    let ratio = size.x / size.y;
    let (w, h) = if ratio >= 1.0 {
        (target_max, target_max / ratio)
    } else {
        (target_max * ratio, target_max)
    };
    let margin = 12.0;
    let rect = egui::Rect::from_min_size(
        egui::pos2(screen.right() - w - margin, screen.bottom() - h - margin),
        egui::vec2(w, h),
    );
    let painter = ui.ctx().layer_painter(egui::LayerId::background());
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    // Semi-transparent tint via vertex colour — egui multiplies the
    // texture RGBA by this.
    let tint = egui::Color32::from_white_alpha(48);
    painter.image(tex.id(), rect, uv, tint);
}

/// Spawn a fresh instance of the running exe with the given args. Used
/// for the "Prévisualiser popup", "Tester animation", "Tester palier"
/// and "Désinstaller" buttons — the config UI runs as the interactive
/// user, so plain `Command::spawn` is enough (no session-0 dance).
pub fn spawn_local(args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| anyhow!("current_exe: {e}"))?;
    std::process::Command::new(&exe)
        .args(args)
        .spawn()
        .map_err(|e| anyhow!("spawn {exe:?} {args:?}: {e}"))?;
    Ok(())
}

/// Copy `src` into `<exe_dir>/Data/Animations/`, sanitize in-place,
/// return `(relative_path_under_protocol_namespace, display_name)`. The
/// returned relative path is `animations/<file>` — the protocol
/// namespace path served by the custom protocol handler, not a disk
/// path. Disk layout is `<exe_dir>/Data/Animations/<file>`.
pub fn ingest_lottie(src: &Path) -> Result<(String, String)> {
    let exe = std::env::current_exe().map_err(|e| anyhow!("current_exe: {e}"))?;
    let exe_parent = exe.parent().ok_or_else(|| anyhow!("exe has no parent"))?;
    let dest_dir = crate::modes::install::animations_dir(exe_parent);
    std::fs::create_dir_all(&dest_dir).map_err(|e| anyhow!("mkdir {dest_dir:?}: {e}"))?;

    let file_name = src
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("source file has no filename: {src:?}"))?
        .to_string();
    let dest = dest_dir.join(&file_name);
    std::fs::copy(src, &dest).map_err(|e| anyhow!("copy {src:?} → {dest:?}: {e}"))?;

    let report = crate::lottie_sanitize::sanitize_lottie_in_place(&dest)
        .map_err(|e| anyhow!("sanitize {dest:?}: {e}"))?;
    tracing::info!(
        stripped = report.stripped_layers,
        inspected = report.inspected_files,
        "lottie ingested"
    );

    let display_name = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Animation")
        .to_string();
    Ok((format!("animations/{file_name}"), display_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes_from(img: image::RgbaImage) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn autocrop_strips_transparent_border() {
        let mut buf = image::RgbaImage::new(4, 4);
        buf.put_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        buf.put_pixel(2, 1, image::Rgba([0, 255, 0, 255]));
        buf.put_pixel(1, 2, image::Rgba([0, 0, 255, 255]));
        buf.put_pixel(2, 2, image::Rgba([255, 255, 0, 255]));
        let (rgba, cw, ch) = decode_and_autocrop_png(&png_bytes_from(buf)).unwrap();
        assert_eq!((cw, ch), (2, 2));
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
        let last = rgba.len() - 4;
        assert_eq!(&rgba[last..], &[255, 255, 0, 255]);
    }

    #[test]
    fn autocrop_rejects_fully_transparent_image() {
        let buf = image::RgbaImage::new(2, 2);
        assert!(decode_and_autocrop_png(&png_bytes_from(buf)).is_err());
    }

    #[test]
    fn remove_pink_kills_pastel_pink_pixels() {
        // Sample from the asset's pink circle : soft pastel around
        // RGB(240, 200, 210). Expect alpha zeroed.
        let mut rgba = vec![240, 200, 210, 255];
        remove_pink_background(&mut rgba);
        assert_eq!(rgba[3], 0);
    }

    #[test]
    fn remove_pink_leaves_white_black_and_orange_alone() {
        // Cat body : white, black outline, orange eyes.
        let mut rgba = vec![
            255, 255, 255, 255, // white
            10, 10, 10, 255,    // black outline
            255, 183, 60, 255,  // orange eye
            30, 30, 30, 255,    // dark grey brush stroke
        ];
        remove_pink_background(&mut rgba);
        for i in [3, 7, 11, 15] {
            assert_eq!(rgba[i], 255, "opaque pixel at byte {i} was wiped");
        }
    }

    #[test]
    fn remove_pink_ignores_already_transparent_pixels() {
        // A partially transparent pink pixel remains as-is (alpha 0
        // pixels are early-return, we don't touch them).
        let mut rgba = vec![240, 200, 210, 0];
        remove_pink_background(&mut rgba);
        assert_eq!(rgba, vec![240, 200, 210, 0]);
    }
}
