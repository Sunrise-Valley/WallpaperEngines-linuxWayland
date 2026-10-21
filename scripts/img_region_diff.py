#!/usr/bin/env python3
"""Compare the same region of two PNGs, optionally flipping one — for pixel-level
verification of the live wallpaper against the renderer's own CLI output.

Why this exists: the CLI (`we-scene-render`) can render the *same* scene at the
*same* physical size the kernel renders at, so "is the live wallpaper upright?"
stops being a matter of eyeballing a screenshot and becomes a number. Flip the
render vertically and the match must get dramatically worse -- otherwise the
metric has no discriminating power and proves nothing.

Zero dependencies on purpose (no Pillow/ImageMagick on this box): PNG decoding is
reused from `desktop_diff.py`.

Usage:
  scripts/img_region_diff.py A.png B.png x1,y1,x2,y2 [--flip v|h|vh]

  --flip v     flip B's region vertically (top<->bottom) before comparing
  --flip h     flip B's region horizontally (left<->right)
  --flip vh    both

Prints each side's channel means and the mean/max absolute difference over the
region. Exit code is always 0; read the numbers.
"""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from desktop_diff import read_png  # noqa: E402


def rows_of(px, w, h, ch, box):
    """The region as a list of per-row bytearrays (full channel depth kept)."""
    x1, y1, x2, y2 = box
    x1, y1 = max(0, min(x1, w)), max(0, min(y1, h))
    x2, y2 = max(0, min(x2, w)), max(0, min(y2, h))
    out = []
    for y in range(y1, y2):
        start = (y * w + x1) * ch
        out.append(bytearray(px[start : start + (x2 - x1) * ch]))
    return out, (x2 - x1, y2 - y1)


def flip_rows(rows, ch, vertical: bool):
    if vertical:
        return list(reversed(rows))
    out = []
    for r in rows:
        out.append(bytearray(b"".join(bytes(r[i : i + ch]) for i in range(len(r) - ch, -1, -ch))))
    return out


def channel_means(rows, ch):
    n = 0
    acc = [0.0] * ch
    for r in rows:
        for i in range(0, len(r) - ch + 1, ch):
            for c in range(ch):
                acc[c] += r[i + c]
            n += 1
    return [v / max(1, n) for v in acc], n


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 2
    pa, pb = argv[0], argv[1]
    flip = ""
    rest = []
    i = 2
    while i < len(argv):
        if argv[i] == "--flip":
            flip = argv[i + 1]
            i += 2
        else:
            rest.append(argv[i])
            i += 1
    if not rest:
        print("要给区域 x1,y1,x2,y2")
        return 2
    box = tuple(int(v) for v in rest[0].split(","))

    wa, ha, ca, px_a = read_png(pa)
    wb, hb, cb, px_b = read_png(pb)
    if (wa, ha, ca) != (wb, hb, cb):
        print(f"尺寸/通道不一致：A={wa}x{ha}/{ca}ch  B={wb}x{hb}/{cb}ch（先统一尺寸再比）")
        return 2

    ra, size = rows_of(px_a, wa, ha, ca, box)
    rb, _ = rows_of(px_b, wb, hb, cb, box)
    if "v" in flip:
        rb = flip_rows(rb, cb, vertical=True)
    if "h" in flip:
        rb = flip_rows(rb, cb, vertical=False)

    ma, n = channel_means(ra, ca)
    mb, _ = channel_means(rb, cb)
    ch3 = min(3, ca)
    tot = 0
    mx = [0] * ch3
    cnt = 0
    for x, y in zip(ra, rb):
        m = min(len(x), len(y))
        for i in range(0, m - ch3 + 1, ca):
            for c in range(ch3):
                d = abs(x[i + c] - y[i + c])
                tot += d
                if d > mx[c]:
                    mx[c] = d
            cnt += 1
    mean_d = tot / max(1, cnt * ch3)
    tag = f"  [B 翻转 {flip}]" if flip else ""
    print(f"区域 {box}  ({size[0]}x{size[1]}, {n} 像素){tag}")
    print("  A 均值 RGB: " + " ".join(f"{v:6.2f}" for v in ma[:ch3]))
    print("  B 均值 RGB: " + " ".join(f"{v:6.2f}" for v in mb[:ch3]))
    print("  最大差 RGB: " + " ".join(f"{v:6d}" for v in mx))
    print(f"  ⇒ 平均绝对差 {mean_d:.2f} / 255")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
