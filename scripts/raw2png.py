#!/usr/bin/env python3
"""raw 帧（`we-ctl dump --output` 的产物）→ PNG。零依赖（zlib 手写 PNG）。

为什么要这个工具：内核侧**不压 PNG**（2560x1440 一张要 ~180 ms，30 帧就是 5 秒的
主循环停顿）。抓帧只落原始像素，压 PNG 交给这台机器上的脚本按需做（取证图、
人眼复核、给界面看）。

用法:
  scripts/raw2png.py <前缀>_0000.raw [更多 raw ...] [--outdir DIR]
  scripts/raw2png.py <目录> --all                 # 目录里所有 .raw
  scripts/raw2png.py <目录> --all --pick 0,10,20  # 只转这几张

格式来自同目录的 `<前缀>.txt` 旁注（idx / presentation_ns / w / h / format / stride）。
字节序换算（Wayland 的格式名说的是 32 位字的**位布局**）：
  · Abgr8888 ⇒ 内存序 R,G,B,A  ⇒ 直接就是 RGBA
  · Xrgb8888 ⇒ 内存序 B,G,R,X  ⇒ 换 R/B，alpha 补 255
"""
from __future__ import annotations

import glob
import os
import struct
import sys
import time
import zlib


def _png_chunk(tag: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def write_png(path: str, w: int, h: int, rgba: bytes) -> None:
    stride = w * 4
    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter = None（够快；压缩率不是目标）
        raw += rgba[y * stride : (y + 1) * stride]
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    out = (
        b"\x89PNG\r\n\x1a\n"
        + _png_chunk(b"IHDR", ihdr)
        + _png_chunk(b"IDAT", zlib.compress(bytes(raw), 1))
        + _png_chunk(b"IEND", b"")
    )
    with open(path, "wb") as f:
        f.write(out)


def sidecar_for(raw_path: str) -> dict:
    """读 `<前缀>.txt`，返回 {idx: (w, h, fmt, stride)}。"""
    stem = raw_path[: -len(".raw")] if raw_path.endswith(".raw") else raw_path
    prefix = stem.rsplit("_", 1)[0]
    txt = prefix + ".txt"
    meta = {}
    if not os.path.exists(txt):
        return meta
    for line in open(txt, encoding="utf-8", errors="replace"):
        if line.startswith("#") or not line.strip():
            continue
        p = line.rstrip("\n").split("\t")
        if len(p) < 6:
            continue
        try:
            meta[int(p[0])] = (int(p[2]), int(p[3]), p[4], int(p[5]))
        except ValueError:
            continue
    return meta


def convert(raw_path: str, outdir: str | None = None) -> str:
    name = os.path.basename(raw_path)
    idx = int(name.rsplit("_", 1)[1].split(".")[0])
    meta = sidecar_for(raw_path)
    if idx not in meta:
        raise SystemExit(f"{raw_path}: 旁注里没有第 {idx} 帧（找不到 {name} 的元数据）")
    w, h, fmt, stride = meta[idx]
    data = open(raw_path, "rb").read()
    need = stride * h
    if len(data) < need:
        raise SystemExit(f"{raw_path}: 只有 {len(data)} 字节，需要 {need}")
    if stride != w * 4:
        raise SystemExit(f"{raw_path}: stride {stride} != w*4（暂不支持填充）")
    px = bytearray(data[:need])
    if fmt == "Xrgb8888":
        for i in range(0, need, 4):
            px[i], px[i + 2] = px[i + 2], px[i]
            px[i + 3] = 255
    # Abgr8888 ⇒ 内存序已是 RGBA，不动
    outdir = outdir or os.path.dirname(raw_path) or "."
    out = os.path.join(outdir, name.replace(".raw", ".png"))
    t0 = time.time()
    write_png(out, w, h, bytes(px))
    print(f"{raw_path} → {out}  {w}x{h} {fmt}  {time.time()-t0:.2f}s")
    return out


def main(argv: list[str]) -> int:
    outdir = None
    pick = None
    args = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--outdir":
            outdir = argv[i + 1]
            i += 2
        elif a == "--pick":
            pick = {int(x) for x in argv[i + 1].split(",")}
            i += 2
        else:
            args.append(a)
            i += 1
    if not args:
        print(__doc__)
        return 2
    paths = []
    for a in args:
        if os.path.isdir(a):
            paths += sorted(glob.glob(os.path.join(a, "*.raw")))
        else:
            paths.append(a)
    if pick is not None:
        paths = [p for p in paths if int(os.path.basename(p).rsplit("_", 1)[1].split(".")[0]) in pick]
    if not paths:
        print("没找到 .raw", file=sys.stderr)
        return 2
    for p in paths:
        convert(p, outdir)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
