# Animation renderers — benchmark & native alternatives

Comparison of every animation format the popup can render, measured on the
real target, plus a memory-cost analysis and a survey of what it would take
to render natively (without the embedded browser). The popup shows a looping
fullscreen transparent animation behind a pincode / dismiss overlay; the open
question this answers is *which renderer is cheapest, and is the WebView2
engine pulling its weight?*

**Measured on** : Parallels Desktop · Windows 11 ARM64 (aarch64) · Apple
Silicon host. WebGL compositing backend on this machine (captured by the
popup's own diagnostic) :

```
ANGLE (0x05404C42, Parallels Display Adapter (WDDM) Direct3D11 vs_5_0 ps_5_0, D3D11)
```

→ **hardware GPU compositing (Direct3D 11)**, *not* SwiftShader software
rendering. The GPU numbers below are therefore real. (Parallels' virtual GPU
is backed by the host Apple GPU; re-confirm on native target hardware before
final decisions.)

## 1. Benchmark

Each renderer ran a **60 s** window; the harness samples the whole WebView2
**process tree** (host + `msedgewebview2.exe` children) every 500 ms and sums
per tick (`role=total`). Averages below exclude the first tick group (a
cold baseline). `cpu_pct` / `gpu_pct` are percent of one core / one engine
summed across the tree, so they can exceed 100.

| Type | Renderer | CPU % | GPU % | Working set | **Private** | vs Lottie CPU |
|---|---|---|---|---|---|---|
| **WebM alpha** | `<video>` (native, HW decode) | **59.5** | 35.2 | 487 MB | **237 MB** | **−63 %** |
| **Rive** | `@rive-app/webgl2` (WASM, GPU) | **111.7** | 33.9 | 661 MB | 427 MB | **−30 %** |
| GIF | `<img>` (native) | 117.4 | 36.7 | 989 MB | 311 MB | −26 % |
| WebP | `<img>` (native) | 119.2 | 38.7 | 1011 MB | 332 MB | −25 % |
| APNG | `<img>` (native) | 123.4 | 35.4 | 1017 MB | 333 MB | −22 % |
| SVG (SMIL) | `<img>` (native) | 131.9 | 46.8 | 689 MB | 398 MB | −17 % |
| **Lottie** | dotLottie / ThorVG (WASM, **software**) | **158.7** | 40.4 | 633 MB | 332 MB | baseline |

Conditional loading verified: a Lottie popup fetches only
`vendor/dotlottie-wc/*`, a Rive popup only `vendor/rive/*`, and the
img/video formats pull **zero** engine bytes (pure native elements).

### What it says

- **Lottie / ThorVG-software is the CPU hog** (159 %). Per-frame vector
  rasterisation on the CPU is exactly what saturates a core on a looping
  fullscreen animation. This is the problem the comparison set out to explain.
- **Rive cuts CPU by ~30 %** at near-equal RAM, and is the **lowest GPU %**
  of all — its WebGL2 pipeline is efficient, not GPU-hungry. Best **vector**
  option.
- **WebM alpha video wins outright**: −63 % CPU *and* the lowest RAM, because
  it rides the **hardware video decoder** — no per-frame CPU raster, no WASM.
  The clear choice *when the asset can be an alpha WebM*.
- **Raster (GIF / WebP / APNG)** is cheaper on CPU than Lottie but costs
  **~1 GB working set** (every decoded frame held in memory) and carries
  quality limits (GIF = 1-bit alpha). Poor trade.
- A **GPU-Lottie** renderer (Skottie / CanvasKit, or ThorVG's GL/WebGPU
  backend) is the one contender not yet measured here — it may close the
  Lottie→Rive gap. To be benchmarked.

**Recommendation (inside the current WebView2 stack)** : prefer **alpha WebM**
when the content allows it, else **Rive** for vector; avoid ThorVG-software
Lottie for anything looping fullscreen; treat raster as a last resort on RAM.

## 2. Why the popup uses ~600 MB — and is it shared with Edge?

The `workingset` on the `role=total` line (≈ 490–1010 MB) is the **sum of the
whole WebView2 process tree** — ~7 processes (browser + GPU + renderer +
network + utilities + host). **WebView2 *is* Edge/Chromium embedded**: a full
browser engine.

Two things matter:

- **Working set over-counts shared pages.** The same Chromium DLL mapped into
  7 processes is counted 7×. The truer "cost added by this popup" is
  `private_bytes` (the **Private** column): ≈ **240–430 MB**, not 600. The
  `ws − private` gap (huge on raster: 1010 → 330) is shared code + mapped
  frame buffers, counted multiple times.
- **It is transient.** The popup is a separate process (`--popup`) that exits
  when the break ends; the memory is reclaimed. The always-resident background
  service is a lean native Rust process with **no webview**. So this is not a
  standing 600 MB footprint — it is the cost of a one-minute break window.

**Is WebView2 shared if Edge is already running?**

- **Processes: no.** WebView2 runs its own **WebView2 Runtime**
  (`msedgewebview2.exe`) — a separate install from the Edge browser
  (`msedge.exe`), same Chromium base but distinct binaries and its own
  user-data folder. It never attaches to a running Edge; the process/CPU cost
  is identical whether Edge is open or not.
- **Memory: partial, at the OS level only.** Windows maps identical read-only
  DLL code pages once (copy-on-write) across all processes that load them. So
  Chromium code is shared — **but already between WebView2's own child
  processes**, without needing Edge. If Edge runs the exact same DLL version,
  those pages are already resident, but that is neither guaranteed nor
  something we control.

Net: counting on "Edge is open" to lighten the popup does not work.

## 3. Could a lighter engine than `tao` + `wry` do this?

**`tao` is already light** — it is just the window (a winit fork). The weight
is **`wry` = WebView2 = Chromium** (the 240–430 MB private). "Lighter" means
replacing the webview, i.e. rendering the animation *and* the pincode/button
overlay natively.

### UI host (pincode + button + floating transparent window)

This part is easy and much lighter than Chromium:

| Toolkit | GPU | Transparent overlay | Composite external texture | Plays anim itself | Maintained | Notes |
|---|---|---|---|---|---|---|
| **egui / eframe** | wgpu (default) | Yes (`with_transparent` + borderless + fullscreen + always-on-top) | **Yes** — `egui_wgpu::Renderer::register_native_texture()` → `TextureId` | No | Very active (0.35.x, 2026) | **Already a dependency** (the config UI). ~10–30 MB. |
| **slint** | Skia / FemtoVG / wgpu | Yes | Yes (`Image::try_from(wgpu::Texture)`) | No | Active | Heavier for one overlay (adds a DSL + build step). |

**egui is the natural target**: it is already in the app, GPU-rendered, does
transparent borderless windows, and the idiomatic pattern is exactly what we
need — upload each animation frame to a `wgpu::Texture`, register it as a
`TextureId`, paint it full-window, then draw the pincode `TextEdit` + dismiss
`Button` on top in the same pass. Footprint would drop from ~300 MB to
~50–100 MB. Caveat: Windows transparency has known egui quirks — smoke-test on
the Parallels ARM64 target early (same ANGLE trap documented in
`rust-windows-window-transparency.md`).

### Native renderer per format (the hard part)

Neither egui nor slint decode animations — you must **feed frames yourself**.
Landscape as of 2026 (verify crate versions + maintenance at adoption time —
several are git-pinned or drifting):

| Format | Native option(s) | Rust binding | GPU/CPU | Alpha | Maintained | Verdict |
|---|---|---|---|---|---|---|
| **Lottie** | **velato** (vello/wgpu) · **ThorVG** / **dotlottie-rs** · rlottie | native crates | velato/ThorVG **GPU**; rlottie CPU | Yes | velato & dotlottie-rs active; rlottie drifting (archived) | Best-supported path. velato = pure-Rust GPU but limited fidelity (no text/images/motion-blur); dotlottie-rs = batteries-included full `.lottie`. |
| **Rive** | **rive-rs** (renders via Vello/wgpu) · C++ `rive-runtime` | rive-rs (git only, unpublished) | GPU | Yes | rive-rs ~1 yr stale; C++ runtime active but no Rust FFI | Works on wgpu (same device as egui) but **brittle** — git-pinned, no crates.io release. The fast PLS renderer is C++-only. |
| **WebM alpha** | **FFmpeg** (`ffmpeg-next`) · GStreamer `codecalpha` · libvpx DIY · Media Foundation | ffmpeg-next 8.x | **CPU (software)** | Yes (hard) | ffmpeg active | ⚠️ **The key risk.** VP8/VP9 carry alpha as a *second* grayscale stream in the container → needs demux + a **second decoder** + plane merge. **All hardware/MF decoders drop alpha** (single-stream) → alpha is inherently CPU. `ffmpeg-next` is the only turnkey path; real cost = building a libvpx-enabled FFmpeg for `aarch64-pc-windows-msvc`. |
| **GIF (animated)** | crate **`image`** (`GifDecoder`) | native | CPU | 1-bit | Canonical, active | Easy — decode frames + delays, blit each. |
| **WebP (animated)** | crate **`webp`** (libwebp `AnimDecoder`) · `image-webp` | native | CPU | Full alpha | webp active; `image-webp` animated support to verify | Easy via libwebp bindings. |
| **SVG animated (SMIL)** | **`resvg`/`usvg`**, `vello_svg` | native | CPU (resvg) / GPU (vello_svg) | Yes | active | ⚠️ **Gap** — these render SVG **static only**; **no native SMIL animator** exists. The browser is one of the few engines that animates `<animateTransform>`. This format specifically favours keeping the webview (or converting it to another format). |

### Estimated memory saving if rendered natively (per format)

Baseline = measured `private_bytes` in the webview → estimated native
footprint as a **single** egui/wgpu process. ⚠️ **The native figures are
estimates, not measured** (a POC is needed to confirm), but the orders of
magnitude are sound. Animated SVG is excluded — it has no native path.

| Format | Webview (measured) | Native (estimate) | Saving |
|---|---|---|---|
| **Rive** | 427 MB | ~100–160 MB | **~270–330 MB (~70 %)** |
| APNG | 333 MB | ~90–150 MB | ~180–240 MB (~60 %) |
| WebP | 332 MB | ~90–150 MB | ~180–240 MB (~60 %) |
| Lottie | 332 MB | ~100–160 MB | ~170–230 MB (~60 %) |
| GIF | 311 MB | ~90–150 MB | ~160–220 MB (~60 %) |
| **WebM** | 237 MB | ~110–200 MB | ~40–130 MB (~20–45 %) |
| ~~SVG animated~~ | ~~398 MB~~ | *no native path* | — |

Key point: the egui/wgpu **host cost (~80–120 MB** — wgpu device + swapchain
+ a fullscreen RGBA texture) is **fixed and shared, not per-format**. So the
popup becomes a **single ~100–160 MB process regardless of format**, versus
237–427 MB today. Biggest absolute win is **Rive (~300 MB)** and the raster
formats (~200 MB); **WebM saves the least** — it was already cheapest via
hardware decode, and native it reverts to CPU decode. And the memory is
**transient** (per-popup, freed on close), so the practical value is low
unless the popup becomes persistent or targets very low-RAM machines.

### Cross-platform (Windows / macOS / Linux)

The rendering + UI bricks are **fully cross-platform** — in fact *more*
uniform than the current webview:

| Brick | Windows | macOS | Linux |
|---|---|---|---|
| egui / eframe (wgpu) | D3D12 | Metal | Vulkan/GL |
| Rive (rive-rs → vello/wgpu) | ✅ | ✅ | ✅ |
| Lottie (velato / ThorVG / dotlottie-rs) | ✅ | ✅ | ✅ |
| WebM alpha (ffmpeg-next, CPU) | ✅ | ✅ | ✅ |
| GIF (`image`) / WebP (`webp`) | ✅ | ✅ | ✅ |
| Media Foundation VP9 | Windows-only | — | — |
| SVG animated (SMIL) | ❌ | ❌ | ❌ |

Two caveats worth stating:

- **The current `wry`/webview stack is itself NOT uniform across OS** — wry
  uses WebView2 on Windows, **WKWebView** on macOS, **WebKitGTK** on Linux.
  Three different engines that render Lottie/Rive/WebGL/alpha *differently*
  (WebKitGTK is notoriously buggy for transparency + WebGL on Linux; WKWebView
  handles VP9-alpha differently). A native **egui + wgpu** stack would be
  **more consistent** across platforms, not less.
- **What is not portable — webview or native — is the OS-integration layer**:
  the always-on-top kiosk window, the global keyboard hook + lock, per-pixel
  transparent compositing, and foreground-steal. Today that is pure Win32
  (`WS_EX_TOOLWINDOW`, `SetWindowsHookEx`, DirectComposition). macOS needs
  `NSWindow` level/collectionBehavior + `CGEventTap`; Linux needs X11/Wayland
  equivalents. **This must be rewritten per OS regardless** of the renderer
  choice, and the Parallels JS keyboard fallback is a Windows-VM-specific
  workaround with no native equivalent.

### Bottom line

- The UI is not the blocker — **egui (already present) would host the pincode
  + overlay 5–10× lighter than WebView2.**
- The blockers are (a) picking **one** renderer to port natively, and (b) two
  format-specific traps: **alpha-WebM native decode is CPU-only and
  ARM64-build-heavy** (`ffmpeg-next` the sole path), and **animated SVG has no
  native renderer at all**. The benchmark says the renderers worth keeping are
  **Rive** and **WebM** — commit to one (ideally a *non-alpha-WebM* vector like
  Rive/Lottie-GPU) and a single wgpu vector renderer can cover both the
  animation and the UI.
- ⚠️ **The webview also earns its weight as an input surface.** On Parallels /
  Apple Silicon the low-level keyboard hook **and** the window-message path
  both fail (the hypervisor routes keystrokes straight to the Chromium
  renderer); the pincode capture falls back to **JS in the WebView**
  (`window.ipc.postMessage`). Dropping the webview loses that fallback and
  risks an uncapturable pincode on exactly this class of VM — a native
  re-platform must solve keyboard input on virtualised guests first.

A native re-platform is therefore a real "v2 architecture" project, not an
incremental change — consistent with the current decision to keep
`tao` + `wry`/WebView2 and A/B renderers inside it.
