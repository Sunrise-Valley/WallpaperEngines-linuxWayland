#!/usr/bin/env python3
"""Measure preserved high-frequency detail in a PNG region.

Why: "did the compositing path resample my wallpaper?" is answerable with numbers
if you feed it a 1px checkerboard (see checker_png.py). A 1:1 path keeps adjacent
pixels at opposite ends of the range; any scaling pulls them together.

Reports, over the region:
  edge energy : mean |pixel - right neighbour| (per channel, /255) -- collapse = blur
  hard edges  : % of horizontal neighbour pairs with |diff| >= 64
  stddev      : per-channel stddev (a 1px checkerboard 1:1 is ~127)

Zero dependencies: PNG decode is reused from desktop_diff.py.

Usage:
  scripts/sharpness.py IN.png x1,y1,x2,y2
"""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from desktop_diff import read_png  # noqa: E402


def main(argv) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    path = argv[0]
    x1, y1, x2, y2 = (int(v) for v in argv[1].split(","))
    w, h, ch, px = read_png(path)
    x1, y1 = max(0, x1), max(0, y1)
    x2, y2 = min(w, x2), min(h, y2)
    ch3 = min(3, ch)

    n_pairs = 0
    sum_diff = 0.0
    hard = 0
    n = 0
    acc = [0.0] * ch3
    acc2 = [0.0] * ch3
    for y in range(y1, y2):
        base = (y * w + x1) * ch
        for x in range(x1, x2):
            i = base + (x - x1) * ch
            for c in range(ch3):
                v = px[i + c]
                acc[c] += v
                acc2[c] += v * v
            n += 1
            if x + 1 < x2:
                j = i + ch
                for c in range(ch3):
                    d = abs(px[i + c] - px[j + c])
                    sum_diff += d
                    if d >= 64:
                        hard += 1
                n_pairs += 1
    if not n or not n_pairs:
        print("区域太小")
        return 2
    means = [a / n for a in acc]
    stds = [(acc2[c] / n - means[c] ** 2) ** 0.5 for c in range(ch3)]
    print(f"{path}  区域 {x1},{y1}..{x2},{y2}  ({x2-x1}x{y2-y1})")
    print("  均值 RGB: " + " ".join(f"{v:6.2f}" for v in means))
    print("  标准差 RGB: " + " ".join(f"{v:6.2f}" for v in stds) + "   （1:1 的 1px 棋盘 ≈ 127）")
    print(f"  边缘能量（相邻像素平均差）: {sum_diff / (n_pairs * ch3):6.2f} / 255")
    print(f"  硬边占比（差 ≥64）: {100.0 * hard / (n_pairs * ch3):6.1f} %")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
