// Strip full-canvas white solid layers from a .lottie file in place.
//
// A .lottie is a ZIP archive containing `manifest.json` and one or more
// `animations/*.json` entries. Some LottieFiles exports ship a solid
// white layer that covers the whole canvas — invisible against a white
// browser background but very much visible against our transparent
// WebView2 popup. This helper reads each animation JSON, drops any layer
// where `ty == 1` (solid), the fill colour is white, and the solid
// dimensions cover the canvas.
//
// Pure Rust ; runs on Linux `cargo test` against fixtures under
// `tests/fixtures/`. Called by the config UI's Animations tab on file
// picker / drag-drop before the animation is exposed in the list.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SanitizeReport {
    pub inspected_files: usize,
    pub stripped_layers: usize,
}

/// Sanitize the `.lottie` at `path` in place. Any full-canvas white
/// solid layer inside every `animations/*.json` entry is removed and
/// the top-level `bg` field is nulled ; other entries pass through
/// byte-for-byte. Write goes to `<path>.tmp` then atomic rename so a
/// crash mid-write can't leave a truncated archive.
pub fn sanitize_lottie_in_place(path: &Path) -> Result<SanitizeReport> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (cleaned, report) = sanitize_lottie_bytes(&bytes).context("sanitize archive")?;

    let tmp = path.with_extension("lottie.tmp");
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&cleaned)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename to {}", path.display()))?;
    Ok(report)
}

/// In-memory variant. Feed raw `.lottie` bytes, get sanitized bytes plus
/// a report. Extracted so the tests don't need a filesystem roundtrip.
pub fn sanitize_lottie_bytes(bytes: &[u8]) -> Result<(Vec<u8>, SanitizeReport)> {
    let cursor = Cursor::new(bytes);
    let mut zin = zip::ZipArchive::new(cursor).context("open .lottie as ZIP")?;

    let mut out = Cursor::new(Vec::<u8>::with_capacity(bytes.len()));
    let mut report = SanitizeReport::default();
    // Compression left at Deflate — matches the browser-friendly output
    // of both LottieFiles' exporter and our Python tool.
    let options: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    {
        let mut zout = zip::ZipWriter::new(&mut out);
        for i in 0..zin.len() {
            let mut entry = zin.by_index(i).context("read zip entry")?;
            let name = entry.name().to_string();

            let mut data = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut data).context("read entry bytes")?;
            drop(entry);

            let payload = if is_animation_json(&name) {
                report.inspected_files += 1;
                match sanitize_animation_json(&data) {
                    Ok((cleaned, stripped)) => {
                        report.stripped_layers += stripped;
                        cleaned
                    }
                    Err(e) => {
                        tracing::warn!(entry = %name, error = %e, "keeping animation entry as-is (parse failed)");
                        data
                    }
                }
            } else {
                data
            };

            zout.start_file(name, options)
                .context("start_file")?;
            zout.write_all(&payload).context("write entry")?;
        }
        zout.finish().context("finalize zip")?;
    }

    Ok((out.into_inner(), report))
}

fn is_animation_json(name: &str) -> bool {
    name.starts_with("animations/") && name.ends_with(".json")
}

/// Parses one animation JSON, drops full-canvas white solid layers, and
/// nulls the top-level `bg`. Returns `(cleaned_bytes, stripped_count)`.
fn sanitize_animation_json(bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut anim: Value = serde_json::from_slice(bytes).context("parse animation json")?;

    let w = anim.get("w").and_then(Value::as_f64).unwrap_or(0.0);
    let h = anim.get("h").and_then(Value::as_f64).unwrap_or(0.0);

    let mut stripped = 0usize;
    if let Some(layers_val) = anim.get_mut("layers") {
        if let Some(layers) = layers_val.as_array_mut() {
            let before = layers.len();
            layers.retain(|layer| !is_full_white_solid(layer, w, h));
            stripped = before - layers.len();
        }
    }
    if let Some(obj) = anim.as_object_mut() {
        obj.insert("bg".to_string(), Value::Null);
    }

    // Compact separators mirror the Python tool's `json.dumps(separators=(",",":"))`
    // — keeps the archive small and matches the original file's shape.
    let bytes = serde_json::to_vec(&anim).context("serialize animation json")?;
    Ok((bytes, stripped))
}

fn is_full_white_solid(layer: &Value, canvas_w: f64, canvas_h: f64) -> bool {
    let ty = layer.get("ty").and_then(Value::as_i64).unwrap_or(-1);
    if ty != 1 {
        return false;
    }
    let sc = layer
        .get("sc")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if sc != "#ffffff" && sc != "#fff" {
        return false;
    }
    let sw = layer.get("sw").and_then(Value::as_f64).unwrap_or(0.0);
    let sh = layer.get("sh").and_then(Value::as_f64).unwrap_or(0.0);
    sw >= canvas_w && sh >= canvas_h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal in-memory `.lottie` (ZIP with a `manifest.json`
    /// and one `animations/anim.json`) so tests don't ship binary
    /// fixtures. Keeps the sanitizer's contract testable in a single
    /// file that anyone can read top-to-bottom.
    fn build_lottie(anim_json: &str) -> Vec<u8> {
        let mut out = Cursor::new(Vec::<u8>::new());
        let options: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
        {
            let mut zout = zip::ZipWriter::new(&mut out);
            zout.start_file("manifest.json", options).unwrap();
            zout.write_all(br#"{"animations":[{"id":"anim"}]}"#).unwrap();
            zout.start_file("animations/anim.json", options).unwrap();
            zout.write_all(anim_json.as_bytes()).unwrap();
            zout.finish().unwrap();
        }
        out.into_inner()
    }

    fn read_anim_entry(bytes: &[u8]) -> Value {
        let cursor = Cursor::new(bytes);
        let mut zin = zip::ZipArchive::new(cursor).unwrap();
        let mut entry = zin.by_name("animations/anim.json").unwrap();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    #[test]
    fn strips_full_canvas_white_solid_layer() {
        let anim = r##"{
            "w": 1200, "h": 1200,
            "bg": "#ffffff",
            "layers": [
                {"ty": 1, "sc": "#FFFFFF", "sw": 1200, "sh": 1200, "nm": "White BG"},
                {"ty": 4, "nm": "Cat body"}
            ]
        }"##;
        let (out, report) = sanitize_lottie_bytes(&build_lottie(anim)).unwrap();
        assert_eq!(report.inspected_files, 1);
        assert_eq!(report.stripped_layers, 1);

        let cleaned = read_anim_entry(&out);
        let layers = cleaned["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0]["nm"], "Cat body");
        assert!(cleaned["bg"].is_null());
    }

    #[test]
    fn preserves_non_white_solid_and_undersized_layers() {
        let anim = r##"{
            "w": 1000, "h": 1000,
            "layers": [
                {"ty": 1, "sc": "#000000", "sw": 1000, "sh": 1000, "nm": "Black BG"},
                {"ty": 1, "sc": "#ffffff", "sw": 500, "sh": 500, "nm": "White stripe"},
                {"ty": 4, "nm": "Cat body"}
            ]
        }"##;
        let (out, report) = sanitize_lottie_bytes(&build_lottie(anim)).unwrap();
        assert_eq!(report.stripped_layers, 0);
        let cleaned = read_anim_entry(&out);
        assert_eq!(cleaned["layers"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn accepts_shorthand_fff_hex() {
        // Some exporters emit the 3-char form.
        let anim = r##"{
            "w": 800, "h": 800,
            "layers": [
                {"ty": 1, "sc": "#fff", "sw": 800, "sh": 800},
                {"ty": 4, "nm": "Cat"}
            ]
        }"##;
        let (_, report) = sanitize_lottie_bytes(&build_lottie(anim)).unwrap();
        assert_eq!(report.stripped_layers, 1);
    }

    #[test]
    fn clean_animation_is_byte_identical_layer_count() {
        // No white solid → nothing dropped, entry re-emitted through
        // serde_json so we assert on the semantic shape, not byte
        // identity (JSON re-serialisation may re-order keys).
        let anim = r#"{"w":600,"h":600,"layers":[{"ty":4,"nm":"Cat"}]}"#;
        let (out, report) = sanitize_lottie_bytes(&build_lottie(anim)).unwrap();
        assert_eq!(report.stripped_layers, 0);
        assert_eq!(report.inspected_files, 1);
        let cleaned = read_anim_entry(&out);
        assert_eq!(cleaned["layers"].as_array().unwrap().len(), 1);
        assert_eq!(cleaned["layers"][0]["nm"], "Cat");
    }

    #[test]
    fn non_animation_entries_pass_through() {
        // manifest.json must survive untouched.
        let anim = r#"{"w":100,"h":100,"layers":[]}"#;
        let bytes = build_lottie(anim);
        let (out, _) = sanitize_lottie_bytes(&bytes).unwrap();
        let cursor = Cursor::new(&out);
        let mut zin = zip::ZipArchive::new(cursor).unwrap();
        let mut mf = zin.by_name("manifest.json").unwrap();
        let mut buf = Vec::new();
        mf.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, br#"{"animations":[{"id":"anim"}]}"#);
    }
}
