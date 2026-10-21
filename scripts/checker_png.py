#!/usr/bin/env python3
"""Write a 1-pixel checkerboard PNG — a high-frequency test pattern for measuring
how much sharpness the wallpaper path preserves.

Why: a 1px checkerboard is the most brutal thing you can ask a compositor to keep
intact. Shown 1:1 it stays black/white/black/white; run through *any* resampling it
collapses toward flat grey. So the contrast that survives tells you objectively
whether the pipeline is 1:1 or quietly scaling. (An illustration source image is
useless for this — at 1080p the blur is invisible by eye.)

Zero dependencies: PNG is written with zlib only.

Usage:
  scripts/checker_png.py OUT.png [W] [H]        # default 2560x1440
"""
from __future__ import annotations

import struct
import sys
import zlib


def write_png(path: str, w: int, h: int, rows) -> None:
    raw = bytearray()
    for r in rows:
        raw.append(0)  # filter: none
        raw += r

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)  # 8-bit RGB
    png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
           + chunk(b"IDAT", zlib.compress(bytes(raw), 6)) + chunk(b"IEND", b""))
    open(path, "wb").write(png)


def main(argv) -> int:
    if not argv:
        print(__doc__)
        return 2
    out = argv[0]
    w = int(argv[1]) if len(argv) > 1 else 2560
    h = int(argv[2]) if len(argv) > 2 else 1440
    black = bytes([0, 0, 0]) * w
    rows = []
    for y in range(h):
        # 逐像素棋盘：相邻像素黑白相反（行与行之间也错开）
        pat = bytes([255, 255, 255]) if (y % 2) else bytes([0, 0, 0])
        inv = bytes([0, 0, 0]) if (y % 2) else bytes([255, 255, 255])
        row = bytearray()
        for x in range(w):
            row += pat if (x % 2) else inv
        rows.append(row)
        _ = black
    write_png(out, w, h, rows)
    print(f"写出 {out}（{w}x{h} 的 1px 棋盘）")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
