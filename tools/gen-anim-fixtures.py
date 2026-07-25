#!/opt/anim-tools/bin/python
"""Regenerate purrpause's raster/video animation test fixtures from a Lottie.

Every output is the SAME animation rendered into each container format, so the
popup benchmark (see src/bench.rs) measures *format/decoder* overhead rather
than differing content. Formats produced:

    <out>/cat.gif    animated GIF   (1-bit transparency)
    <out>/cat.webp   animated WebP  (lossless, full alpha)
    <out>/cat.apng   animated PNG   (full alpha)
    <out>/cat.webm   VP9 alpha WebM (yuva420p — the only alpha video path
                                     WebView2/Chromium composites)

The animated SVG fixture (resources/w7vTYuF0iN.svg) is authored by hand and is
NOT regenerated here.

REQUIREMENTS (provided by .devcontainer/Dockerfile — rebuild the devcontainer
if the imports below fail):
    * the /opt/anim-tools venv  → rlottie-python + Pillow
    * ffmpeg on PATH            → carries libvpx-vp9 for the alpha WebM

Run it with the venv interpreter so rlottie/PIL resolve:

    /opt/anim-tools/bin/python tools/gen-anim-fixtures.py

Optional flags: --src <file.lottie> --out <dir> --size <px> --fps <n>.
This is a rare, dev-only convenience — the fixtures are committed binaries, so
you only re-run it when the source Lottie or the desired framing changes.
"""

from __future__ import annotations

import argparse
import io
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_SRC = REPO / "resources" / "animations" / "dance-cat.lottie"
DEFAULT_OUT = REPO / "resources" / "fixtures"


def die(msg: str) -> "NoReturn":  # type: ignore[valid-type]
    sys.exit(f"gen-anim-fixtures: {msg}")


def load_lottie_json(src: Path) -> str:
    """Return the raw Lottie JSON text from a `.lottie` (dotLottie ZIP) or a
    plain `.json` file. dotLottie stores one or more animations under
    `animations/*.json`; we take the largest (the primary animation)."""
    data = src.read_bytes()
    if not zipfile.is_zipfile(io.BytesIO(data)):
        # Plain Lottie JSON.
        return data.decode("utf-8")
    with zipfile.ZipFile(io.BytesIO(data)) as zf:
        anims = [n for n in zf.namelist()
                 if n.startswith("animations/") and n.endswith(".json")]
        if not anims:
            die(f"{src}: no animations/*.json inside the .lottie archive")
        primary = max(anims, key=lambda n: zf.getinfo(n).file_size)
        return zf.read(primary).decode("utf-8")


def render_frames(lottie_json: str, size: int):
    """Render the Lottie to a list of RGBA PIL frames plus its native fps."""
    try:
        from PIL import Image  # noqa: F401  (used indirectly by rlottie)
        from rlottie_python import LottieAnimation
    except ImportError as e:
        die(f"missing tool ({e}). Rebuild the devcontainer so the anim-tools "
            f"venv exists, then run this with /opt/anim-tools/bin/python.")

    anim = LottieAnimation.from_data(lottie_json)
    total = anim.lottie_animation_get_totalframe()
    fps = anim.lottie_animation_get_framerate() or 30.0
    frames = []
    for i in range(total):
        # render_pillow_frame returns an RGBA image at the requested size.
        frames.append(anim.render_pillow_frame(frame_num=i, width=size, height=size))
    if not frames:
        die("Lottie produced zero frames")
    return frames, fps


def save_gif(frames, fps: float, dest: Path) -> None:
    dur_ms = int(round(1000.0 / fps))
    # GIF is 1-bit alpha: quantize keeping a reserved transparent index.
    first, *rest = [f.convert("RGBA") for f in frames]
    first.save(dest, format="GIF", save_all=True, append_images=rest,
               duration=dur_ms, loop=0, disposal=2, transparency=0)


def save_webp(frames, fps: float, dest: Path) -> None:
    dur_ms = int(round(1000.0 / fps))
    first, *rest = frames
    first.save(dest, format="WEBP", save_all=True, append_images=rest,
               duration=dur_ms, loop=0, lossless=True, allow_mixed=True)


def save_apng(frames, fps: float, dest: Path) -> None:
    dur_ms = int(round(1000.0 / fps))
    first, *rest = frames
    first.save(dest, format="PNG", save_all=True, append_images=rest,
               duration=dur_ms, loop=0, disposal=1)


def save_webm(frames, fps: float, dest: Path) -> None:
    if not shutil.which("ffmpeg"):
        die("ffmpeg not found — rebuild the devcontainer (Dockerfile installs it).")
    with tempfile.TemporaryDirectory() as td:
        tdp = Path(td)
        for i, f in enumerate(frames):
            f.save(tdp / f"f{i:04d}.png")
        cmd = [
            "ffmpeg", "-y", "-framerate", f"{fps:g}",
            "-i", str(tdp / "f%04d.png"),
            "-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p",
            "-b:v", "0", "-crf", "32", str(dest),
        ]
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            die(f"ffmpeg failed:\n{proc.stderr[-2000:]}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--src", type=Path, default=DEFAULT_SRC)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--size", type=int, default=256)
    ap.add_argument("--fps", type=float, default=0.0,
                    help="override output fps (default: the Lottie's native fps)")
    args = ap.parse_args()

    if not args.src.exists():
        die(f"source not found: {args.src}")
    args.out.mkdir(parents=True, exist_ok=True)

    print(f"→ source   : {args.src}")
    frames, native_fps = render_frames(load_lottie_json(args.src), args.size)
    fps = args.fps or native_fps
    print(f"→ frames   : {len(frames)} @ {fps:g} fps, {args.size}px")

    for ext, fn in (("gif", save_gif), ("webp", save_webp),
                    ("apng", save_apng), ("webm", save_webm)):
        dest = args.out / f"cat.{ext}"
        fn(frames, fps, dest)
        print(f"  wrote {dest.relative_to(REPO)}  ({dest.stat().st_size} B)")

    print("done. Import each via the Animations tab to smoke-test.")


if __name__ == "__main__":
    main()
