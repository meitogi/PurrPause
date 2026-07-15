// Post-build packaging tool. Runs on the host (Linux devcontainer),
// invokes `cargo xwin build --release --target <triple>` for each
// requested target, then bundles the resulting `SystemHealthAgent.exe`
// + sidecar `.manifest` + convenience `.bat` scripts into
// `target/dist/SystemHealthAgent-<version>-<arch>.zip`.
//
// Invoke examples :
//   cargo run --bin pack --release                 # both aarch64 + x64
//   cargo run --bin pack --release -- --aarch64    # aarch64 only
//   cargo run --bin pack --release -- --x64        # x64 only
//   cargo run --bin pack --release -- --no-build   # skip cargo xwin (both, reuse exes)

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};

const EXE_NAME: &str = "SystemHealthAgent.exe";
const MANIFEST_NAME: &str = "SystemHealthAgent.exe.manifest";

struct Target {
    triple: &'static str,
    arch_suffix: &'static str,
}

const AARCH64: Target = Target {
    triple: "aarch64-pc-windows-msvc",
    arch_suffix: "aarch64",
};

const X64: Target = Target {
    triple: "x86_64-pc-windows-msvc",
    arch_suffix: "x64",
};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let skip_build = args.iter().any(|a| a == "--no-build");
    let only_aarch64 = args.iter().any(|a| a == "--aarch64");
    let only_x64 = args.iter().any(|a| a == "--x64");

    let targets: Vec<&Target> = match (only_aarch64, only_x64) {
        (true, false) => vec![&AARCH64],
        (false, true) => vec![&X64],
        // Default (no flag) or both flags set : build both.
        _ => vec![&AARCH64, &X64],
    };

    let version = read_cargo_version()?;

    for target in targets {
        pack_target(target, &version, skip_build)?;
    }
    Ok(())
}

fn pack_target(target: &Target, version: &str, skip_build: bool) -> Result<()> {
    println!("[pack] purrpause v{version} → {}", target.triple);

    if !skip_build {
        println!("[pack] cargo xwin build --release --target {}", target.triple);
        let status = Command::new("cargo")
            .arg("xwin")
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg(target.triple)
            .status()
            .context("spawn cargo xwin build")?;
        if !status.success() {
            anyhow::bail!("cargo xwin build failed for {} (exit {status})", target.triple);
        }
    } else {
        println!("[pack] --no-build : skipping cargo xwin build for {}", target.triple);
    }

    let release_dir = PathBuf::from("target").join(target.triple).join("release");
    let exe = release_dir.join(EXE_NAME);
    let manifest = release_dir.join(MANIFEST_NAME);
    if !exe.exists() {
        anyhow::bail!("exe not found : {}", exe.display());
    }
    if !manifest.exists() {
        anyhow::bail!("manifest not found : {}", manifest.display());
    }

    let stage_name = format!("SystemHealthAgent-{version}-{}", target.arch_suffix);
    let dist_dir = PathBuf::from("target").join("dist");
    fs::create_dir_all(&dist_dir).context("mkdir target/dist")?;

    // Remove any prior zip for this version so stale bytes don't leak
    // into the release. Wildcard cleanup at 0.5.x avoids the manual
    // `rm -rf target/dist/SystemHealthAgent-0.5.*` shell dance.
    clean_previous_zips(&dist_dir, "SystemHealthAgent-0.5.")?;
    clean_previous_zips(&dist_dir, "SystemHealthAgent-0.6.")?;

    let zip_path = dist_dir.join(format!("{stage_name}.zip"));
    let file = fs::File::create(&zip_path)
        .with_context(|| format!("create {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // Core artefacts (exe + sidecar manifest).
    let mut count = 0usize;
    for src in [&exe, &manifest] {
        let arc_name = format!(
            "{stage_name}/{}",
            src.file_name().and_then(|s| s.to_str()).unwrap()
        );
        add_file(&mut zip, src, &arc_name, options)?;
        count += 1;
    }

    // Optional convenience .bat scripts (Activer/Desactiver + Nettoyer + Reset-Clean).
    // Silently skipped if missing so the pack still succeeds for a
    // scripts-less checkout.
    let scripts_dir = PathBuf::from("scripts");
    if scripts_dir.is_dir() {
        for entry in fs::read_dir(&scripts_dir).context("read scripts/")? {
            let entry = entry.context("scripts/ entry")?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("bat") {
                let arc_name = format!(
                    "{stage_name}/{}",
                    path.file_name().and_then(|s| s.to_str()).unwrap()
                );
                add_file(&mut zip, &path, &arc_name, options)?;
                count += 1;
            }
        }
    }

    // LICENSE + NOTICE.md. Required by Lottie Simple License (copyleft-lite
    // redistribution clause) and MIT (must ship license text alongside).
    // Absent files are a hard error - shipping without these breaches the
    // upstream license obligations.
    for src_name in ["LICENSE", "NOTICE.md"] {
        let src = PathBuf::from(src_name);
        let arc_name = format!("{stage_name}/{src_name}");
        add_file(&mut zip, &src, &arc_name, options)?;
        count += 1;
    }

    zip.finish().context("finalize zip")?;

    let size = fs::metadata(&zip_path)?.len();
    println!(
        "[pack] wrote {} ({:.2} MiB, {count} files)",
        zip_path.display(),
        size as f64 / 1024.0 / 1024.0
    );
    Ok(())
}

fn read_cargo_version() -> Result<String> {
    let text = fs::read_to_string("Cargo.toml").context("read Cargo.toml")?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("version") {
            if let Some(v) = rest
                .split('=')
                .nth(1)
                .and_then(|s| s.trim().strip_prefix('"'))
                .and_then(|s| s.strip_suffix('"'))
            {
                return Ok(v.to_string());
            }
        }
    }
    Err(anyhow!("no version key found in Cargo.toml"))
}

fn add_file(
    zip: &mut zip::ZipWriter<fs::File>,
    src: &Path,
    arc_name: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<()> {
    zip.start_file(arc_name, options).context("start_file")?;
    let mut f = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).context("read")?;
    zip.write_all(&buf).context("write")?;
    Ok(())
}

fn clean_previous_zips(dist_dir: &Path, prefix: &str) -> Result<()> {
    let entries = match fs::read_dir(dist_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(".zip") {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}
