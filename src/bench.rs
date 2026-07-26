// Benchmark harness - pure, cross-platform logic.
//
// The popup's rendering cost lives in the WebView2 *child* processes
// (msedgewebview2.exe browser/renderer/GPU), not in the popup host PID,
// so the sampler measures the whole process tree. This module holds
// everything Linux-testable : the tree walk over a (pid, ppid) table,
// the CPU-tick delta math, the PDH instance-name parsing and the TSV
// row formatting. The win32 sampling itself (Toolhelp / PDH / PSAPI)
// is in `platform::win32::proc_sample`.

// Most of this module's surface is reached only through #[cfg(windows)]
// callers, so it looks dead on the Linux dev build. Enforcement stays on
// for the shipping Windows target.
#![cfg_attr(not(windows), allow(dead_code))]

/// Column header of `Data\Logs\bench.log`. One row per sampled process
/// per tick, plus one `role=total` aggregate row per tick.
pub const TSV_HEADER: &str = "timestamp\tfilename\textension\trenderer\tpid\trole\tcpu_ms\tcpu_pct\tgpu_pct\tgpu_mem_bytes\tworkingset_bytes\tprivate_bytes";

/// Renderer label keyed purely by file extension, for the formats where
/// the extension alone determines the engine:
///   `.lottie`                       -> `thorvg-sw`  (software WASM)
///   `.svg/.gif/.webp/.png/.apng`    -> `img-native` (WebView `<img>` decode)
///   `.webm`                         -> `video-native`
///   `.riv`                          -> `rive-gpu`   (WebGL2 GPU renderer)
/// Unknown extensions map to `thorvg-sw` to match the popup mount's
/// unknown->Lottie fallback. `.lottie` here always yields the ThorVG
/// baseline — a renderer-aware caller uses [`renderer_label`] instead.
pub fn renderer_label_for_ext(ext: &str) -> &'static str {
    match ext {
        "lottie" => "thorvg-sw",
        "svg" | "gif" | "webp" | "png" | "apng" => "img-native",
        "webm" => "video-native",
        "riv" => "rive-gpu",
        _ => "thorvg-sw",
    }
}

/// Renderer label taking BOTH the extension and the resolved engine. Only
/// `.lottie` has two engines for one extension (ThorVG-SW vs Skottie-GPU),
/// so a Skottie `.lottie` gets `skottie-gpu` while everything else defers
/// to [`renderer_label_for_ext`]. Written into the `renderer` column so the
/// same `.lottie` fixture is directly comparable across engines in one log.
pub fn renderer_label(ext: &str, renderer: crate::config::Renderer) -> &'static str {
    use crate::config::Renderer;
    match (ext, renderer) {
        ("lottie", Renderer::SkottieGpu) => "skottie-gpu",
        _ => renderer_label_for_ext(ext),
    }
}

/// Sampling cadence.
pub const BENCH_TICK_MS: u64 = 500;

/// Bench window when `--bench` is passed without `--bench-secs`.
pub const DEFAULT_BENCH_SECS: u32 = 60;

/// One sampled process at one tick. `cpu_pct` / `gpu_pct` are percent
/// of ONE core / engine-sum and may exceed 100.
pub struct BenchRow {
    pub pid: u32,
    pub role: String,
    pub cpu_ms: u64,
    pub cpu_pct: f64,
    pub gpu_pct: f64,
    pub gpu_mem_bytes: u64,
    pub workingset_bytes: u64,
    pub private_bytes: u64,
}

/// BFS from `root` over a (pid, ppid) table : root + all transitive
/// children. The visited set guards against ppid cycles, which are
/// possible when a parent dies and its PID is recycled by a descendant's
/// child.
pub fn descendant_pids(root: u32, procs: &[(u32, u32)]) -> Vec<u32> {
    let mut out = vec![root];
    let mut visited = std::collections::HashSet::from([root]);
    let mut cursor = 0;
    while cursor < out.len() {
        let parent = out[cursor];
        cursor += 1;
        for &(pid, ppid) in procs {
            if ppid == parent && visited.insert(pid) {
                out.push(pid);
            }
        }
    }
    out
}

/// Extract the PID from a PDH GPU counter instance name like
/// `pid_1234_luid_0x00000000_0x0000ABCD_phys_0_eng_0_engtype_3D`.
/// Tolerates any suffix after the digit run ; rejects an empty run.
pub fn parse_gpu_instance_pid(instance: &str) -> Option<u32> {
    let rest = instance.strip_prefix("pid_")?;
    let digits: &str = &rest[..rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len()];
    digits.parse::<u32>().ok()
}

/// Merge a FILETIME's (high, low) halves into 100 ns ticks.
pub fn filetime_to_u64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// CPU consumed over `interval`, from two kernel+user totals in 100 ns
/// ticks. Returns (cpu_ms, percent of one core - may exceed 100 when
/// several threads burn in parallel). Saturates to 0 on counter
/// regression ; a zero interval yields 0 %.
pub fn cpu_delta_ms_pct(prev_100ns: u64, cur_100ns: u64, interval: std::time::Duration) -> (u64, f64) {
    let delta = cur_100ns.saturating_sub(prev_100ns);
    let cpu_ms = delta / 10_000;
    let interval_ns = interval.as_nanos();
    if interval_ns == 0 {
        return (cpu_ms, 0.0);
    }
    let pct = (delta as f64 * 100.0) / interval_ns as f64 * 100.0;
    (cpu_ms, pct)
}

/// One formatted TSV line (no trailing newline). `timestamp` is
/// pre-formatted by the caller.
pub fn tsv_row(timestamp: &str, filename: &str, extension: &str, renderer: &str, row: &BenchRow) -> String {
    format!(
        "{timestamp}\t{filename}\t{extension}\t{renderer}\t{pid}\t{role}\t{cpu_ms}\t{cpu_pct:.1}\t{gpu_pct:.1}\t{gpu_mem}\t{ws}\t{private}",
        pid = row.pid,
        role = row.role,
        cpu_ms = row.cpu_ms,
        cpu_pct = row.cpu_pct,
        gpu_pct = row.gpu_pct,
        gpu_mem = row.gpu_mem_bytes,
        ws = row.workingset_bytes,
        private = row.private_bytes,
    )
}

/// Split an animation path/override into (file name, extension) for the
/// log columns : `animations/dance-cat.lottie` -> ("dance-cat.lottie",
/// "lottie"). Callers must pass the *resolved* URL (via
/// `resolve_animation_url`), not a raw `embed:*` id — an `embed:*` id has no
/// extension, which would mislabel a Lottie run as the thorvg-sw default.
pub fn split_anim_name(anim: &str) -> (String, String) {
    let name = anim
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(anim)
        .to_string();
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    (name, ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn descendant_walk_finds_multi_level_tree() {
        let procs = [(200, 100), (201, 100), (300, 200), (999, 998)];
        let mut got = descendant_pids(100, &procs);
        got.sort_unstable();
        assert_eq!(got, vec![100, 200, 201, 300]);
    }

    #[test]
    fn descendant_walk_survives_ppid_cycle() {
        let procs = [(200, 100), (100, 200)];
        let mut got = descendant_pids(100, &procs);
        got.sort_unstable();
        assert_eq!(got, vec![100, 200]);
    }

    #[test]
    fn descendant_walk_root_only() {
        assert_eq!(descendant_pids(42, &[]), vec![42]);
    }

    #[test]
    fn parse_gpu_instance_pid_full_name() {
        assert_eq!(
            parse_gpu_instance_pid("pid_1234_luid_0x00000000_0x0000ABCD_phys_0_eng_0_engtype_3D"),
            Some(1234)
        );
    }

    #[test]
    fn parse_gpu_instance_pid_rejects_garbage() {
        assert_eq!(parse_gpu_instance_pid("pid_"), None);
        assert_eq!(parse_gpu_instance_pid("1234"), None);
        assert_eq!(parse_gpu_instance_pid("pid_x9"), None);
    }

    #[test]
    fn cpu_delta_zero_interval_is_zero_pct() {
        let (ms, pct) = cpu_delta_ms_pct(0, 5_000_000, Duration::ZERO);
        assert_eq!(ms, 500);
        assert_eq!(pct, 0.0);
    }

    #[test]
    fn cpu_delta_full_core() {
        // 500 ms of 100 ns ticks over a 500 ms interval = one full core.
        let (ms, pct) = cpu_delta_ms_pct(0, 5_000_000, Duration::from_millis(500));
        assert_eq!(ms, 500);
        assert!((pct - 100.0).abs() < 0.01, "pct = {pct}");
    }

    #[test]
    fn cpu_delta_can_exceed_100_pct() {
        let (ms, pct) = cpu_delta_ms_pct(0, 10_000_000, Duration::from_millis(500));
        assert_eq!(ms, 1000);
        assert!((pct - 200.0).abs() < 0.01, "pct = {pct}");
    }

    #[test]
    fn cpu_delta_counter_regression_saturates() {
        let (ms, pct) = cpu_delta_ms_pct(10_000_000, 5_000_000, Duration::from_millis(500));
        assert_eq!(ms, 0);
        assert_eq!(pct, 0.0);
    }

    #[test]
    fn tsv_row_column_count_and_order() {
        let row = BenchRow {
            pid: 1234,
            role: "webview".into(),
            cpu_ms: 250,
            cpu_pct: 50.04,
            gpu_pct: 12.35,
            gpu_mem_bytes: 1_048_576,
            workingset_bytes: 2_097_152,
            private_bytes: 3_145_728,
        };
        let line = tsv_row("2026-07-25T12:00:00.000", "dance-cat.lottie", "lottie", renderer_label_for_ext("lottie"), &row);
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), TSV_HEADER.split('\t').count());
        assert_eq!(fields[0], "2026-07-25T12:00:00.000");
        assert_eq!(fields[1], "dance-cat.lottie");
        assert_eq!(fields[2], "lottie");
        assert_eq!(fields[3], "thorvg-sw");
        assert_eq!(fields[4], "1234");
        assert_eq!(fields[5], "webview");
        assert_eq!(fields[6], "250");
        assert_eq!(fields[7], "50.0");
        assert_eq!(fields[8], "12.3");
        assert_eq!(fields[9], "1048576");
        assert_eq!(fields[10], "2097152");
        assert_eq!(fields[11], "3145728");
    }

    #[test]
    fn renderer_label_maps_extension_to_engine() {
        assert_eq!(renderer_label_for_ext("lottie"), "thorvg-sw");
        for ext in ["svg", "gif", "webp", "png", "apng"] {
            assert_eq!(renderer_label_for_ext(ext), "img-native", "ext {ext}");
        }
        assert_eq!(renderer_label_for_ext("webm"), "video-native");
        assert_eq!(renderer_label_for_ext("riv"), "rive-gpu");
        // Unknown ext mirrors the mount's unknown->Lottie fallback.
        assert_eq!(renderer_label_for_ext("xyz"), "thorvg-sw");
        assert_eq!(renderer_label_for_ext(""), "thorvg-sw");
    }

    #[test]
    fn renderer_label_is_renderer_aware_for_lottie() {
        use crate::config::Renderer;
        // Same `.lottie` extension, two engines → two labels.
        assert_eq!(renderer_label("lottie", Renderer::ThorvgSw), "thorvg-sw");
        assert_eq!(renderer_label("lottie", Renderer::SkottieGpu), "skottie-gpu");
        // The renderer field is meaningless for non-Lottie formats — they
        // stay extension-keyed regardless of the entry's renderer value.
        assert_eq!(renderer_label("riv", Renderer::SkottieGpu), "rive-gpu");
        assert_eq!(renderer_label("webm", Renderer::ThorvgSw), "video-native");
        assert_eq!(renderer_label("svg", Renderer::SkottieGpu), "img-native");
        assert_eq!(renderer_label("xyz", Renderer::ThorvgSw), "thorvg-sw");
    }

    #[test]
    fn split_anim_name_variants() {
        assert_eq!(
            split_anim_name("animations/dance-cat.lottie"),
            ("dance-cat.lottie".to_string(), "lottie".to_string())
        );
        assert_eq!(
            split_anim_name(r"C:\Data\Animations\Fun.WEBM"),
            ("Fun.WEBM".to_string(), "webm".to_string())
        );
        assert_eq!(split_anim_name("noext"), ("noext".to_string(), String::new()));
    }
}
