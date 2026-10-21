#!/usr/bin/env python3
"""Crop a PNG and write the crop back out — zero dependencies.

Why: on COSMIC the live wallpaper is a layer-shell surface *behind* every
window, so a full-screen screenshot only shows wallpaper where no window covers
the desktop. That visible part is often a narrow strip, and a 2560x1440 shot
mostly full of somebody's browser is useless as evidence. This cuts the strip
out at full resolution so it can actually be looked at.

Reuses the PNG decoder in desktop_diff.py (no Pillow on this box by default).

Usage:
  scripts/png_crop.py IN.png OUT.png x1,y1,x2,y2
"""
from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from desktop_diff import read_png  # noqa: E402


def write_png(path: str, w: int, h: int, ch: int, px: bytearray, box) -> tuple[int, int]:
    x1, y1, x2, y2 = box
    cw, chh = max(0, x2 - x1), max(0, y2 - y1)
    stride = w * ch
    raw = bytearray()
    for y in range(y1, y2):
        raw.append(0)                      # filter type 0 (None) per scanline
        base = y * stride + x1 * ch
        raw += px[base:base + cw * ch]
    color_type = {0: 0, 1: 0, 2: 4, 3: 2, 4: 6}[ch]

    def chunk(t: bytes, d: bytes) -> bytes:
        return (struct.pack(">I", len(d)) + t + d
                + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF))

    blob = (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", cw, chh, 8, color_type, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(raw), 6))
            + chunk(b"IEND", b""))
    Path(path).write_bytes(blob)
    return cw, chh


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    src, out, spec = sys.argv[1], sys.argv[2], sys.argv[3]
    x1, y1, x2, y2 = (int(v) for v in spec.split(","))
    w, h, ch, px = read_png(src)
    cw, chh = write_png(out, w, h, ch, px, (x1, y1, min(x2, w), min(y2, h)))
    print(f"{out}: {cw}x{chh}  (cropped from {src} {w}x{h})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
