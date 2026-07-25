# tools/ — dev-only helpers (not shipped)

## gen-anim-fixtures.py

Regenerates the popup's raster/video animation **test fixtures** from a bundled
Lottie, so every container format renders the *same* animation and the popup
benchmark (`src/bench.rs`) compares decoder/format overhead rather than content.

Outputs → `resources/fixtures/`:

| file        | format         | alpha              |
|-------------|----------------|--------------------|
| `cat.gif`   | animated GIF   | 1-bit transparency |
| `cat.webp`  | animated WebP  | full alpha         |
| `cat.apng`  | animated PNG   | full alpha         |
| `cat.webm`  | VP9 WebM       | `yuva420p`         |

The animated **SVG** fixture (`resources/w7vTYuF0iN.svg`) is hand-authored and
is *not* produced here.

### Requirements

The tools live in the devcontainer image (`.devcontainer/Dockerfile`), not the
running shell by default — **rebuild the devcontainer** once so they exist:

- `/opt/anim-tools` venv → `rlottie-python` (faithful Lottie raster) + `Pillow`
- `ffmpeg` on `PATH` → carries `libvpx-vp9` for the alpha WebM

### Run

```
/opt/anim-tools/bin/python tools/gen-anim-fixtures.py
```

Flags: `--src <file.lottie>` (default `resources/animations/dance-cat.lottie`),
`--out <dir>`, `--size <px>` (default 256), `--fps <n>` (default: the Lottie's
native fps).

Rare, one-off task: the generated fixtures are committed binaries — re-run only
when the source Lottie or the framing changes.
