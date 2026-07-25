# NOTICE — Third-party attributions

PurrPause bundles third-party assets and libraries listed below. This
document satisfies the redistribution + attribution obligations of each
component's license.

## PurrPause (this project)

- **License** : MIT (see [LICENSE](LICENSE))
- **Copyright © 2026 meitogi**

## Bundled Lottie animations

Both animations are distributed under the **Lottie Simple License** — a
copyleft-lite license from Design Barn Inc. (parent of LottieFiles).
Redistribution of the animation files requires including the license
text (reproduced verbatim at the end of this document). Derivative
works (modifications to the animation JSON) are subject to the same
terms. Attribution of the original animator is OPTIONAL under this
license but strongly encouraged ; animator names were not captured at
download time and are not enumerated here.

### `resources/animations/dance-cat.lottie`
- **Source** : https://lottiefiles.com/free-animation/dance-cat-5wfbtgfSi0
- **Published** : 2024-01-16
- **Specs** : 30 FPS, 512×512, 3 s duration, 26 layers.
- **Modifications** : none.

### `resources/animations/cat-sleeping-no-bg.lottie`
- **Source** : https://lottiefiles.com/free-animation/cat-is-sleeping-and-rolling-QnXhMBjCbD
- **Published** : 2021-04-09
- **Specs** : 29.97 FPS, 500×500, 16 s duration, 15 layers.
- **Modifications** : full-canvas white solid layer stripped for
  transparency using `tools/sanitize-lottie.py`. Derivative work
  under the Lottie Simple License terms.

## Bundled JavaScript / WASM runtime

### `@lottiefiles/dotlottie-wc` — Web Component wrapper
- **Version** : 0.9.21
- **License** : MIT (Copyright LottieFiles / Design Barn Inc.)
- **Location in tree** : `resources/vendor/dotlottie-wc/*.js`
- **Purpose** : Web Component `<dotlottie-wc>` that mounts the WASM
  player.

### `@lottiefiles/dotlottie-web` — WASM Lottie player
- **Version** : 0.77.1
- **License** : MIT (Copyright LottieFiles / Design Barn Inc.)
- **Location in tree** : `resources/vendor/dotlottie-wc/dotlottie-player.wasm`
- **Purpose** : Rust-compiled Lottie player that renders frames to a
  Canvas context inside the WebView2 popup.

### Lit HTML (transitive) — templating library
- **Publisher** : Google LLC
- **License** : BSD-3-Clause (Copyright 2017, 2019 Google LLC ;
  SPDX-License-Identifier headers visible in
  `resources/vendor/dotlottie-wc/base-dotlottie-wc-BqyUGr__.js`)
- **Purpose** : templating layer bundled inside dotlottie-wc.

### `@rive-app/webgl2` — Rive WebGL2 runtime
- **Version** : 2.39.1
- **License** : MIT (Copyright Rive Inc.)
- **Location in tree** : `resources/vendor/rive/rive.js` + `rive.wasm`
- **Purpose** : GPU (WebGL2) renderer for `.riv` vector animations,
  mounted inside the WebView2 popup.

### `canvaskit-wasm` (Skia / Skottie) — GPU Lottie runtime
- **Version** : 0.41.1 (the **full** dist build — `bin/full/`, the smallest
  variant whose runtime actually exposes Skottie's `MakeManagedAnimation` ;
  the default `bin/` build ships the type but not the compiled binding)
- **License** : BSD-3-Clause (Copyright Google LLC ; see
  `resources/vendor/skottie/` and the Skia LICENSE)
- **Location in tree** : `resources/vendor/skottie/canvaskit.js` +
  `canvaskit.wasm`
- **Purpose** : Skia's CanvasKit (WebGL) build, including Skottie
  (`MakeManagedAnimation`), the GPU renderer for `.lottie` animations
  whose entry selects the Skottie engine.

## Icon and watermark PNGs

- `resources/cat-icon.png` and `resources/cat-watermark.png` are
  user-supplied for this project. Contact the project owner for
  reuse permissions.

## Rust dependencies

The full Rust dependency graph and their SPDX licenses are extractable
via `cargo license` (from the `cargo-license` crate). Not enumerated
here to avoid drift ; the graph is dominated by MIT / Apache-2.0 /
BSD-3-Clause crates from the `rust-lang`, `windows-rs`, and `tauri`
ecosystems.

To reproduce the full attribution list at any time :

```bash
cd apps/purrpause
cargo license --json > third-party-licenses.json
```

---

# Lottie Simple License (reproduced verbatim)

Copyright © 2021 Design Barn Inc.

Permission is hereby granted, free of charge, to any person obtaining a
copy of the public animation files available for download at the
LottieFiles site (“Files”) to download, reproduce, modify, publish,
distribute, publicly display, and publicly digitally perform such
Files, including for commercial purposes, provided that any display,
publication, performance, or distribution of Files must contain (and
be subject to) the same terms and conditions of this license.
Modifications to Files are deemed derivative works and must also be
expressly distributed under the same terms and conditions of this
license. You may not purport to impose any additional or different
terms or conditions on, or apply any technical measures that restrict
exercise of, the rights granted under this license. This license does
not include the right to collect or compile Files from LottieFiles to
replicate or develop a similar or competing service.

Use of Files without attributing the creator(s) of the Files is
permitted under this license, though attribution is strongly
encouraged. If attributions are included, such attributions should be
visible to the end user.

FILES ARE PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
EXCEPT TO THE EXTENT REQUIRED BY APPLICABLE LAW, IN NO EVENT WILL THE
CREATOR(S) OF FILES OR DESIGN BARN, INC. BE LIABLE ON ANY LEGAL
THEORY FOR ANY SPECIAL, INCIDENTAL, CONSEQUENTIAL, PUNITIVE, OR
EXEMPLARY DAMAGES ARISING OUT OF THIS LICENSE OR THE USE OF SUCH
FILES.
