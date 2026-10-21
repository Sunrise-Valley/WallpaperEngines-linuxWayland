#!/usr/bin/env python3
"""量具自检：注入一个**已知**的撕裂，看 `row_displacement.py` 能不能量出来。

为什么要这个：一个"永远报 0 px"的量具看起来完美通过，实际上什么都没证明。
所以判据必须带一条**反向实验**：把某帧 `y≥split` 的内容整体右移 N 像素（= 典型的撕裂形态：
线上下来自两个不同时刻），量具必须报出 **N px** 的单行阶跃；再把两帧做成逐字节相同，
量具必须报 **0 px**。两个方向都对，量具才算可信。

用法:
  scripts/row_displacement_selftest.py <源 raw> [--shift 5] [--split 0.43] [--outdir DIR]
                                      [--roi 名字=x1,y1,x2,y2]

不给 `--outdir` 就写 scratch 目录（默认 `$HOME/.hermes/cache/scratch/r1/selftest`），
产物是三帧 raw + 旁注；随后打印两条量具命令与期望值（不自动调用量具，便于肉眼核对）。
"""
from __future__ import annotations

import os
import sys


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__)
        return 2
    src = argv[0]
    rest = argv[1:]
    shift, split = 5, 0.43
    outdir = os.path.join(
        os.environ.get("HOME", "."), ".hermes/cache/scratch/r1/selftest"
    )
    roi = "马尾=0.62,0.31,0.84,0.60"
    i = 0
    while i < len(rest):
        if rest[i] == "--shift":
            shift = int(rest[i + 1]); i += 2
        elif rest[i] == "--split":
            split = float(rest[i + 1]); i += 2
        elif rest[i] == "--outdir":
            outdir = rest[i + 1]; i += 2
        elif rest[i] == "--roi":
            roi = rest[i + 1]; i += 2
        else:
            i += 1

    stem = src[: -len(".raw")]
    prefix = stem.rsplit("_", 1)[0]
    idx = int(stem.rsplit("_", 1)[1])
    meta = None
    for line in open(prefix + ".txt", encoding="utf-8"):
        if line.startswith("#") or not line.strip():
            continue
        p = line.rstrip("\n").split("\t")
        if len(p) >= 6 and int(p[0]) == idx:
            meta = (int(p[2]), int(p[3]), p[4], int(p[5]))
    if meta is None:
        raise SystemExit(f"{src}: 旁注里没有第 {idx} 帧")
    w, h, fmt, stride = meta
    if stride != w * 4:
        raise SystemExit("只支持未填充的 32 位缓冲")

    data = bytearray(open(src, "rb").read())
    split_y = int(split * h)
    torn = bytearray(data)
    for y in range(split_y, h):
        row = bytes(torn[y * stride : (y + 1) * stride])
        torn[y * stride : (y + 1) * stride] = row[-shift * 4 :] + row[: stride - shift * 4]

    os.makedirs(outdir, exist_ok=True)
    open(os.path.join(outdir, "t_0000.raw"), "wb").write(data)          # 干净
    open(os.path.join(outdir, "t_0001.raw"), "wb").write(torn)          # 注入撕裂
    open(os.path.join(outdir, "t_0002.raw"), "wb").write(data)          # 与 0 逐字节相同
    open(os.path.join(outdir, "t_0003.raw"), "wb").write(data)          # 再一份：用来量"相同帧 = 0px"
    with open(os.path.join(outdir, "t.txt"), "w", encoding="utf-8") as f:
        f.write("# idx\tpresentation_ns\tw\th\tformat\tstride\t(量具自检：0=干净 1=注入 %dpx 撕裂 2=与 0 相同)\n" % shift)
        for k in range(4):
            f.write(f"{k}\t{k*1000000}\t{w}\t{h}\t{fmt}\t{stride}\n")
    print(f"已在 {outdir} 生成自检三帧（split y={split_y}, shift={shift}px, {w}x{h} {fmt}）")
    print("期望：")
    print(f"  python3 scripts/row_displacement.py {outdir} --roi {roi} --pairs 0")
    print(f"    ⇒ 单行阶跃 = {shift} px（必须量出来，否则量具是瞎的）")
    print(f"  python3 scripts/row_displacement.py {outdir} --roi {roi} --pairs 1")
    print(f"    ⇒ 单行阶跃 = {shift} px（反方向，torn → clean 同样量得出）")
    print(f"  python3 scripts/row_displacement.py {outdir} --roi {roi} --pairs 2")
    print("    ⇒ 单行阶跃 = 0 px（t_0002 与 t_0003 逐字节相同 ⇒ 量具不会无中生有）")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
