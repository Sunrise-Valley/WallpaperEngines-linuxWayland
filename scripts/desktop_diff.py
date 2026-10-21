#!/usr/bin/env python3
"""Prove a live wallpaper is actually being drawn, by diffing two screenshots.

Why this exists: on COSMIC the wallpaper is a layer-shell surface *behind* every
window, so a screenshot only shows pixels where no window covers the desktop —
and it is impossible to tell "this strip is the animated wallpaper" from "this
strip is cosmic-bg's static image" by eye.

The discriminator: **cosmic-bg is a static image, a live wallpaper animates.**
Take two screenshots a few seconds apart and compare the same region:

  * region changes  -> something is animating there (the live wallpaper)
  * region identical -> only the static background is showing

It also reports per-region colour mean/stddev, so a solid-black region with zero
variance is caught as "rendered nothing / missing texture" rather than passing
just because it happens to be stable.

Zero dependencies on purpose (Pillow/ImageMagick are not installed here): PNG is
decoded with zlib + the five standard unfilter modes.

Usage:
  scripts/desktop_diff.py A.png B.png [--region x1,y1,x2,y2 ...]
  # default regions are edge strips a maximised window tends to leave exposed
"""
from __future__ import annotations

import struct
import sys
import zlib

DEFAULT_REGIONS = [
    ("left strip",   (0, 200, 220, 1360)),
    ("bottom strip", (230, 1355, 2500, 1400)),
]


def read_png(path: str):
    """Minimal 8-bit PNG reader -> (w, h, channels, bytearray of pixels)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG")
    pos, idat = 8, bytearray()
    w = h = bd = ct = None
    while pos < len(data):
        ln = struct.unpack(">I", data[pos:pos + 4])[0]
        typ = data[pos + 4:pos + 8]
        chunk = data[pos + 8:pos + 8 + ln]
        if typ == b"IHDR":
            w, h, bd, ct, _comp, _filt, inter = struct.unpack(">IIBBBBB", chunk)
            if inter:
                raise SystemExit("interlaced PNG not supported")
        elif typ == b"IDAT":
            idat += chunk
        elif typ == b"IEND":
            break
        pos += 12 + ln
    if bd != 8:
        raise SystemExit(f"bit depth {bd} not supported")
    ch = {0: 1, 2: 3, 4: 2, 6: 4}[ct]
    raw = zlib.decompress(bytes(idat))
    stride = w * ch
    out = bytearray(h * stride)
    prev = bytearray(stride)
    p = 0
    for y in range(h):
        f = raw[p]
        p += 1
        line = bytearray(raw[p:p + stride])
        p += stride
        if f == 1:
            for i in range(ch, stride):
                line[i] = (line[i] + line[i - ch]) & 255
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 255
        elif f == 3:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 255
        elif f == 4:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                b = prev[i]
                c = prev[i - ch] if i >= ch else 0
                pp = a + b - c
                pa, pb, pc = abs(pp - a), abs(pp - b), abs(pp - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        out[y * stride:(y + 1) * stride] = line
        prev = line
    return w, h, ch, out


def region_stats(w, ch, px, box):
    """-> (mean_abs_diff_placeholder, mean_rgb, stddev_rgb) for one region."""
    x1, y1, x2, y2 = box
    n = 0
    s = [0, 0, 0]
    s2 = [0, 0, 0]
    stride = w * ch
    for y in range(y1, y2):
        base = y * stride + x1 * ch
        for x in range(x2 - x1):
            o = base + x * ch
            for c in range(3):
                v = px[o + (c if ch >= 3 else 0)]
                s[c] += v
                s2[c] += v * v
            n += 1
    mean = [s[c] / n for c in range(3)]
    var = [max(0.0, s2[c] / n - mean[c] ** 2) for c in range(3)]
    return mean, [v ** 0.5 for v in var]


def region_diff(w, ch, pa, pb, box):
    x1, y1, x2, y2 = box
    stride = w * ch
    tot, mx, n = 0, 0, 0
    for y in range(y1, y2):
        base = y * stride + x1 * ch
        for x in range(x2 - x1):
            o = base + x * ch
            for c in range(3):
                d = abs(pa[o + (c if ch >= 3 else 0)] - pb[o + (c if ch >= 3 else 0)])
                tot += d
                if d > mx:
                    mx = d
            n += 1
    return tot / (n * 3), mx


def main() -> int:
    argv = sys.argv[1:]
    files: list[str] = []
    regions: list = []
    i = 0
    while i < len(argv):
        if argv[i] == "--region":
            i += 1
            while i < len(argv) and not argv[i].startswith("--"):
                x1, y1, x2, y2 = (int(v) for v in argv[i].split(","))
                regions.append((argv[i], (x1, y1, x2, y2)))
                i += 1
        else:
            files.append(argv[i])
            i += 1
    if not regions:
        regions = list(DEFAULT_REGIONS)
    args = files
    if len(args) < 2:
        print(__doc__)
        return 2

    wa, ha, ca, pa = read_png(args[0])
    wb, hb, cb, pb = read_png(args[1])
    print(f"A: {args[0]}  {wa}x{ha} ch={ca}")
    print(f"B: {args[1]}  {wb}x{hb} ch={cb}")
    if (wa, ha) != (wb, hb):
        print("  !! sizes differ; comparing the common area only")
    w, h = min(wa, wb), min(ha, hb)
    w = min(w, wb)

    print("\n  region                                      animating?  mean-abs-diff  max  mean RGB          stddev")
    for name, (x1, y1, x2, y2) in regions:
        box = (x1, y1, min(x2, w), min(y2, h))
        mean, mx = region_diff(w, ca, pa, pb, box)
        cm, cs = region_stats(w, ca, pa, box)
        verdict = "YES" if mean > 0.5 else "no "
        blackish = " (⚠ 近乎纯黑/无变化)" if max(cm) < 12 and max(cs) < 3 else ""
        print(f"  {name:42} {verdict}        {mean:8.2f}  {mx:4d}  "
              f"{' '.join(f'{v:5.0f}' for v in cm)}   "
              f"{' '.join(f'{v:5.1f}' for v in cs)}{blackish}")
    print("\n  animating?=YES 表示该区域在变化 -> 有活动壁纸在画（cosmic-bg 是静态图，不会变）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
