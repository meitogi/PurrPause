// Two build-time responsibilities :
//
// 1. Sidecar UAC manifest deployment. `embed-manifest` needs Microsoft's
//    `mt.exe` which the devcontainer doesn't have, so we ship
//    `SystemHealthAgent.exe.manifest` next to the exe instead - Windows
//    auto-loads sidecar manifests at process creation.
//
// 2. VS_VERSIONINFO metadata for the Explorer "Details" tab (CompanyName,
//    ProductName, FileDescription, FileVersion, LegalCopyright). No RC
//    toolchain in the devcontainer either - so we hand-roll a COFF `.o`
//    object with an RT_VERSION resource (same technique embed-manifest
//    uses for RT_MANIFEST) and feed it to lld-link via a linker arg.

#[path = "build_support/versioninfo.rs"]
mod versioninfo;

use std::path::PathBuf;

fn main() {
    let manifest_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("SystemHealthAgent.exe.manifest");

    println!("cargo:rerun-if-changed=resources/SystemHealthAgent.exe.manifest");
    println!("cargo:rerun-if-changed=build_support/versioninfo.rs");

    // OUT_DIR is `target/<triple>/<profile>/build/<crate>-<hash>/out`.
    // Walk up four levels to reach `target/<triple>/<profile>/` where the
    // exe lands after linking.
    let out_dir_raw = std::env::var("OUT_DIR").expect("OUT_DIR unset");
    let out_dir = PathBuf::from(&out_dir_raw);
    let target_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR shallower than expected")
        .to_path_buf();

    let manifest_dst = target_dir.join("SystemHealthAgent.exe.manifest");

    if let Err(e) = std::fs::copy(&manifest_src, &manifest_dst) {
        // Non-fatal : sidecar is a Windows-only UX feature, and other
        // targets (linux cargo test) don't need it. Warn only.
        println!(
            "cargo:warning=failed to deploy sidecar manifest to {}: {e}",
            manifest_dst.display()
        );
    }

    emit_versioninfo(&out_dir);
}

fn emit_versioninfo(out_dir: &std::path::Path) {
    let target = std::env::var("TARGET").unwrap_or_default();
    // VS_VERSIONINFO only applies to Windows PE binaries.
    if !target.contains("windows-msvc") {
        return;
    }
    let machine = if target.starts_with("aarch64") {
        versioninfo::MachineType::Aarch64
    } else if target.starts_with("x86_64") {
        versioninfo::MachineType::X86_64
    } else {
        println!(
            "cargo:warning=unsupported Windows arch for versioninfo: {target}",
        );
        return;
    };

    let major: u16 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0);
    let minor: u16 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0);
    let patch: u16 = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0);
    let version_str = format!("{major}.{minor}.{patch}.0");

    // Bland Microsoft-style camouflage. FileDescription matches the
    // SCM Description string so `sc.exe qc` and Explorer agree.
    let entries: &[(&str, &str)] = &[
        ("CompanyName", "Microsoft Corporation"),
        ("ProductName", "Windows Session Health Service"),
        (
            "FileDescription",
            "Monitors user session health metrics for ergonomic notifications.",
        ),
        ("FileVersion", &version_str),
        ("ProductVersion", &version_str),
        ("InternalName", "SystemHealthAgent"),
        ("OriginalFilename", "SystemHealthAgent.exe"),
        (
            "LegalCopyright",
            "(c) Microsoft Corporation. All rights reserved.",
        ),
    ];

    let vi_blob = versioninfo::build_versioninfo(major, minor, patch, entries);
    let obj = match versioninfo::build_coff_object(machine, &vi_blob) {
        Ok(o) => o,
        Err(e) => {
            println!("cargo:warning=versioninfo COFF build failed: {e}");
            return;
        }
    };

    let obj_path = out_dir.join("versioninfo.o");
    if let Err(e) = std::fs::write(&obj_path, &obj) {
        println!(
            "cargo:warning=failed to write {}: {e}",
            obj_path.display(),
        );
        return;
    }
    println!("cargo:rustc-link-arg-bins={}", obj_path.display());
}
