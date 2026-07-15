# PurrPause

Break-reminder overlay for Windows, camouflaged as a system service.

Fires a fullscreen transparent popup (dancing cat) every configurable
interval, with pre-notification countdown widgets at T-15 / T-10 / T-5 /
T-1. Designed as a low-friction parental screen-time tool for
teenage-admin machines : the parent sets a passcode ; the child sees a
generic `Windows Session Health Service` in `services.msc` and needs
non-obvious steps to disable it.

**Status** : v1.0.0-b.1 (beta). Windows 10 1607+ / Windows 11 (x64 +
ARM64). Aligned with a « friction, not fort-knox » design — see
[SECURITY-NOTES.md](docs/SECURITY-NOTES.md) for the honest attack surface.

## Install

1. Download the release zip matching your CPU :
   - `SystemHealthAgent-<version>-x64.zip` for typical Windows PCs.
   - `SystemHealthAgent-<version>-aarch64.zip` for Snapdragon / ARM64.
2. Extract anywhere (Documents, Downloads, dedicated folder — the exe
   stays put ; only `C:\ProgramData\DiagnosticsCache\` is used at
   runtime).
3. Double-click `SystemHealthAgent.exe`.
4. Accept the UAC elevation prompt.
5. First-run wizard prompts for a passcode (4-12 digits, argon2id
   hashed on disk).
6. Config UI opens once the service is registered — set intervals,
   messages, animations rotation.

WebView2 Evergreen runtime must be present on the machine (bundled
with Windows 11 by default ; on Windows 10 it's usually already
installed via Microsoft Edge — the app prompts to download if
absent).

## Uninstall

Two supported paths :

- **Config UI → Sécurité tab → Désinstaller** (passcode-gated). Full
  teardown : stops service, deletes SCM entry + Scheduled Task,
  wipes `C:\ProgramData\DiagnosticsCache\`, schedules exe self-delete
  on next boot.
- **`Nettoyer.bat`** double-clic (shipped in the zip). Self-elevates,
  runs the same teardown without the UI (useful if the Config UI
  won't open).

For recovery from a broken state, `Reset-Clean.bat` also ships in the
zip and does the same teardown without a passcode gate (admin-only,
intended for parents troubleshooting).

## Configuration overview

Config UI (opens from wizard, or via Explorer → double-click on the
installed exe) with 5 tabs :

- **Général** — interval, popup duration, pre-notif paliers.
- **Animations** — drag-drop `.lottie` files, per-anim scale + offset,
  rotation mode (random / sequential).
- **Messages** — title, subtitle, dismiss label, countdown template.
- **Notifications** — per-palier messages, force-minimize checkboxes
  (widget escalation for stubborn fullscreen apps).
- **Sécurité** — change passcode, uninstall.

All values live in `C:\ProgramData\DiagnosticsCache\state.dat` (DPAPI
machine-encrypted). Passcode is argon2id-hashed ; never stored
plaintext.

## Design highlights

- **Windows service + Scheduled Task watchdog** — child killing the
  process from Task Manager triggers resurrection within 60 s.
- **HKLM « Uninstalled » marker** — `del state.dat` alone doesn't
  silently uninstall ; without the marker the watchdog resurrects the
  service with defaults (parent notices at next Config UI visit).
- **Camouflaged naming** — service `WindowsSystemHealth`, task
  `\Microsoft\Windows\SystemHealth\HealthCheck`, state folder
  `C:\ProgramData\DiagnosticsCache\`. Nothing yells `PurrPause` in
  `services.msc` / `taskschd.msc` browses.
- **Popup fullscreen transparent** — WebView2 + dotlottie-web WASM
  renderer for smooth Lottie playback. Keyboard hook blocks Alt+F4 /
  Alt+Tab / Win+D / Ctrl+Esc during the mandatory countdown ;
  unlocked automatically at t+countdown so the child can regain
  desktop control post-break.

Full attack-surface enumeration + v1 mitigation status :
[docs/SECURITY-NOTES.md](docs/SECURITY-NOTES.md).

## Build from source

The project cross-compiles from a Linux devcontainer to both Windows
targets using [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin) :

```bash
cargo xwin build --release --target x86_64-pc-windows-msvc
cargo xwin build --release --target aarch64-pc-windows-msvc
```

For release zips (both targets + LICENSE + NOTICE.md + `.bat` scripts
bundled) :

```bash
cargo run --bin pack --release
```

Test suite (Linux-runnable, no Windows required) :

```bash
cargo test
# 202 passing at 0.9.1
```

## Dependencies

Rust 1.75+. Windows target dependencies :

- `windows` crate (Win32 FFI)
- `windows-service` (SCM helpers)
- `tao` + `wry` + `webview2-com` (popup)
- `eframe` / `egui` + `wgpu` (Config UI + countdown widgets)
- `dotlottie-wc` + `dotlottie-player.wasm` (bundled, ~2 MB, animation
  renderer)

Full graph : `cargo tree` / `cargo license`.

## License & credits

- **PurrPause** : MIT (see [LICENSE](LICENSE)).
- **Bundled Lottie animations** : Lottie Simple License (Design Barn
  Inc.) — see [NOTICE.md](NOTICE.md) for the verbatim license text
  and animation sources.
- **`@lottiefiles/dotlottie-wc` + `@lottiefiles/dotlottie-web`** : MIT
  (LottieFiles).
- **lit-html** (transitive) : BSD-3-Clause (Google LLC).

## Contributing

Not currently accepting external contributions during the beta —
scope is still stabilizing. Bug reports welcome via GitHub Issues once
the repo is public.
