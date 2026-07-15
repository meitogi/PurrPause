#!/usr/bin/env python3
"""
Strip full-canvas white solid layers from a .lottie file.

A .lottie is a ZIP archive containing manifest.json and animations/*.json.
LottieFiles exports sometimes ship a solid-white background layer that
defeats transparent-window rendering (WebView2 transparent, wry, etc.).

This tool creates a new .lottie next to the input with a " (no-bg)" suffix,
with any full-canvas solid layer whose fill color is white removed.

Usage:
    python3 sanitize-lottie.py <input.lottie> [--in-place]

The algorithm to port to Rust in session 6 (config UI ingestion):
    1. Open .lottie as ZIP; enumerate animations/*.json entries.
    2. For each JSON, drop layers where ty == 1 AND
       sc.lower() in {'#ffffff', '#fff'} AND
       sw >= animation.w AND sh >= animation.h.
    3. Also set the top-level `bg` field to null.
    4. Re-zip with the same entry names.
"""

from __future__ import annotations
import argparse
import io
import json
import sys
import zipfile
from pathlib import Path


def sanitize_animation(anim: dict) -> int:
    """Mutate in place; return the number of layers removed."""
    w = anim.get("w", 0)
    h = anim.get("h", 0)
    before = len(anim.get("layers", []))

    def is_full_white_solid(layer: dict) -> bool:
        return (
            layer.get("ty") == 1
            and (layer.get("sc", "") or "").lower() in ("#ffffff", "#fff")
            and layer.get("sw", 0) >= w
            and layer.get("sh", 0) >= h
        )

    anim["layers"] = [l for l in anim.get("layers", []) if not is_full_white_solid(l)]
    anim["bg"] = None
    return before - len(anim["layers"])


def sanitize_lottie(src: Path, dst: Path) -> int:
    """Read src .lottie, strip, write dst. Return total layers removed."""
    total_removed = 0
    with zipfile.ZipFile(src, "r") as zin:
        entries: dict[str, bytes] = {}
        for name in zin.namelist():
            data = zin.read(name)
            if name.startswith("animations/") and name.endswith(".json"):
                anim = json.loads(data)
                total_removed += sanitize_animation(anim)
                data = json.dumps(anim, separators=(",", ":")).encode("utf-8")
            entries[name] = data

    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zout:
        for name, data in entries.items():
            zout.writestr(name, data)

    dst.write_bytes(buf.getvalue())
    return total_removed


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    ap.add_argument("input", type=Path, help="Path to input .lottie")
    ap.add_argument(
        "--in-place",
        action="store_true",
        help="Overwrite the input instead of writing a sibling '(no-bg)' file.",
    )
    args = ap.parse_args()

    src = args.input.resolve()
    if not src.is_file():
        print(f"error: {src} not found", file=sys.stderr)
        return 2

    if args.in_place:
        dst = src
    else:
        stem = src.stem
        dst = src.with_name(f"{stem} (no-bg){src.suffix}")

    removed = sanitize_lottie(src, dst)
    print(f"Removed {removed} full-canvas white-solid layer(s).")
    print(f"Output: {dst} ({dst.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
