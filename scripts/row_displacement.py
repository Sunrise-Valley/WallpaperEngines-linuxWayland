#!/usr/bin/env python3
"""逐行位移场量具：判"同一帧里有没有撕裂"。

## 判据是什么（以及为什么这样判）

撕裂的物理形态只有一种：**同一帧的不同高度来自不同时刻**。发梢类动画在 30fps 下
只走 ~0.17 px/帧（Q12 实测量），一帧的时间差在画面上等于 0 px；**能看见的 ≥5 px
位移，意味着有一段内容整整差了 ~30 帧（≈1 秒）**。

于是判据可以写成纯几何的：
  · 对每一**行**，用横向模板相关求出它相对上一帧的整数位移 d(y)（像素）；
  · 干净的一帧：所有行都来自同一时刻 ⇒ d(y) 近似常数 ⇒ **相邻两行的阶跃 ≈ 0**；
  · 撕裂的一帧：撕裂线上下各是一段"差了几十帧"的内容 ⇒ 线上那一处 d(y) **跳变 5 px 起**。

所以量的是 **max |d(y) − d(y−1)|（单行阶跃）**，而不是帧间差：帧间差只能证明"在动"，
证明不了"同一帧内上下是一致的"。

## 用法

  scripts/row_displacement.py <目录或 raw> [--roi 名字=x1,y1,x2,y2 ...] [--max-shift 12]
                                     [--row-step 2] [--col-step 2] [--min-peak 1.4]
                                     [--json out.json] [--pairs 0,1,2]

`--min-peak`（默认 3.0）是这一行"值不值得信"的闸门：最优 SAD 必须比中位 SAD 好 3 倍以上。
为什么必须卡：半透明终端压在壁纸上时，窗口投影/渐变的行几乎是平的，相关系数曲线没有峰，
估计出来的 -7px / +5px 全是噪声（实测这些行的 peak 只有 1.2~1.5，而真正有结构的发丝行是 5~80）。

ROI 用**归一化**坐标（0..1，y 向下，屏幕左上为原点），与 `docs/QUEUE.md` 里的写法一致。
不给 ROI 就用整帧。

输出：每个 ROI 的逐对「最大单行阶跃 / 行数 / 参与行数 / 帧间平均差」，以及整段的
最大值。`--json` 写出机器可读结果（验收报告直接引用）。
"""
from __future__ import annotations

import glob
import json
import operator
import os
import struct
import sys

CH = 4  # Abgr8888 / Xrgb8888 都是 32 位


def load_meta(raw_path: str) -> tuple[int, int, str, int]:
    stem = raw_path[: -len(".raw")]
    prefix = stem.rsplit("_", 1)[0]
    idx = int(stem.rsplit("_", 1)[1])
    meta = None
    with open(prefix + ".txt", encoding="utf-8") as f:
        for line in f:
            if line.startswith("#") or not line.strip():
                continue
            p = line.rstrip("\n").split("\t")
            if len(p) >= 6 and int(p[0]) == idx:
                meta = (int(p[2]), int(p[3]), p[4], int(p[5]))
    if meta is None:
        raise SystemExit(f"{raw_path}: 旁注里没有第 {idx} 帧")
    return meta


def gray_plane(raw_path: str) -> tuple[bytes, int, int]:
    """取一帧的灰度（用 G 通道；BGRX/RGBA 两种内存序里 G 都在第 1 字节）。"""
    w, h, fmt, stride = load_meta(raw_path)
    d = open(raw_path, "rb").read()
    if len(d) < stride * h:
        raise SystemExit(f"{raw_path}: 数据不全")
    if fmt == "Xrgb8888":
        # 内存序 B,G,R,X ⇒ 灰度取 G = 1
        return d[1::4][: w * h], w, h
    # Abgr8888 ⇒ 内存序 R,G,B,A ⇒ 灰度取 G = 1
    return d[1::4][: w * h], w, h


def row_slice(plane: bytes, w: int, y: int, x1: int, x2: int) -> bytes:
    o = y * w
    return plane[o + x1 : o + x2]


def best_shift(a: bytes, b: bytes, max_shift: int, col_step: int, min_peak: float):
    """b 相对 a 的整数横向位移（>0 = b 的内容向右移了）。返回 (shift, peak) 或 None。

    只在**重叠区**上比（否则边缘会伪造出一个位移）。peak = 次优/最优 SAD 比，
    用来判断这一行到底有没有可测的结构：平坦行（天空、纯色）没有峰 ⇒ 跳过。
    """
    n = len(a)
    if n < 2 * max_shift + 8:
        return None
    sad_by = {}
    for s in range(-max_shift, max_shift + 1):
        if s >= 0:
            aa = a[s:]
            bb = b[: n - s]
        else:
            aa = a[: n + s]
            bb = b[-s:]
        # 列抽样（col_step）省时间：结构是横向连续的，抽样不改变峰的位置
        aa = aa[::col_step]
        bb = bb[::col_step]
        sad_by[s] = sum(map(abs, map(operator.sub, aa, bb))) / max(len(aa), 1)
    best = min(sad_by, key=sad_by.get)
    vals = sorted(sad_by.values())
    med = vals[len(vals) // 2]
    peak = (med + 1e-9) / (sad_by[best] + 1e-9)
    if peak < min_peak:
        return None
    return best, peak


def parse_roi(spec: str):
    name, _, coords = spec.partition("=")
    if not coords:
        name, coords = "frame", spec
    x1, y1, x2, y2 = (float(v) for v in coords.split(","))
    return name, (x1, y1, x2, y2)


def run_pair(pa: str, pb: str, rois, max_shift: int, row_step: int, col_step: int,
             min_peak: float):
    ga, w, h = gray_plane(pa)
    gb, w2, h2 = gray_plane(pb)
    if (w, h) != (w2, h2):
        raise SystemExit(f"{pa} 与 {pb} 尺寸不同")
    out = {}
    for name, (rx1, ry1, rx2, ry2) in rois:
        x1, x2 = int(rx1 * w), int(rx2 * w)
        y1, y2 = int(ry1 * h), int(ry2 * h)
        shifts = []  # (y, shift)
        diff_sum = 0
        npx = 0
        for y in range(y1, y2, row_step):
            a = row_slice(ga, w, y, x1, x2)
            b = row_slice(gb, w, y, x1, x2)
            diff_sum += sum(map(abs, map(operator.sub, a, b)))
            npx += len(a)
            r = best_shift(a, b, max_shift, col_step, min_peak)
            if r is not None:
                shifts.append((y, r[0]))
        # 单行阶跃：只在"相邻两个采样行都测到了"的地方算，且必须是相邻行（row_step 步长）
        max_step = 0
        at = None
        for (y0, s0), (y1_, s1) in zip(shifts, shifts[1:]):
            if y1_ - y0 != row_step:
                continue
            st = abs(s1 - s0)
            if st > max_step:
                max_step = st
                at = (y0, s0, y1_, s1)
        mean_shift = sum(s for _, s in shifts) / max(len(shifts), 1)
        out[name] = {
            "rows_total": len(range(y1, y2, row_step)),
            "rows_measured": len(shifts),
            "max_row_step_px": max_step,
            "max_row_step_at": at,
            "mean_shift_px": round(mean_shift, 2),
            "min_shift_px": min((s for _, s in shifts), default=0),
            "max_shift_abs_px": max((abs(s) for _, s in shifts), default=0),
            "mean_abs_diff": round(diff_sum / max(npx, 1), 3),
        }
    return out


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__)
        return 2
    rois = []
    max_shift, row_step, col_step, min_peak = 12, 2, 2, 3.0
    json_out = None
    pairs_arg = None
    args = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--roi":
            rois.append(parse_roi(argv[i + 1]))
            i += 2
        elif a == "--max-shift":
            max_shift = int(argv[i + 1])
            i += 2
        elif a == "--row-step":
            row_step = int(argv[i + 1])
            i += 2
        elif a == "--col-step":
            col_step = int(argv[i + 1])
            i += 2
        elif a == "--min-peak":
            min_peak = float(argv[i + 1])
            i += 2
        elif a == "--json":
            json_out = argv[i + 1]
            i += 2
        elif a == "--pairs":
            pairs_arg = {int(x) for x in argv[i + 1].split(",")}
            i += 2
        else:
            args.append(a)
            i += 1
    if not args:
        print("缺 raw 目录", file=sys.stderr)
        return 2
    paths = []
    for a in args:
        paths += sorted(glob.glob(os.path.join(a, "*.raw"))) if os.path.isdir(a) else [a]
    if len(paths) < 2:
        print("至少要两帧", file=sys.stderr)
        return 2
    if not rois:
        rois = [("frame", (0.0, 0.0, 1.0, 1.0))]
    worst = {}
    report = {"pairs": [], "rois": [n for n, _ in rois], "max_shift": max_shift}
    for k in range(len(paths) - 1):
        if pairs_arg is not None and k not in pairs_arg:
            continue
        res = run_pair(paths[k], paths[k + 1], rois, max_shift, row_step, col_step, min_peak)
        report["pairs"].append({"a": os.path.basename(paths[k]),
                                "b": os.path.basename(paths[k + 1]), "rois": res})
        line = [f"{os.path.basename(paths[k])}→{os.path.basename(paths[k + 1])}"]
        for name in report["rois"]:
            r = res[name]
            line.append(
                f"{name}: step {r['max_row_step_px']}px"
                f" (n={r['rows_measured']}/{r['rows_total']}, 平均位移 {r['mean_shift_px']:+.2f}px,"
                f" 帧间差 {r['mean_abs_diff']:.2f})"
            )
            if r["max_row_step_px"] > worst.get(name, -1):
                worst[name] = r["max_row_step_px"]
                report.setdefault("worst_at", {})[name] = {
                    "pair": k, "at": r["max_row_step_at"], "step": r["max_row_step_px"],
                }
        print("  ".join(line))
    report["worst_step"] = worst
    print("\n== 结论（单行阶跃，判据 ≤1px）==")
    for name, v in worst.items():
        print(f"  {name:>10}: {v} px  {'✓ 通过' if v <= 1 else '✗ 不过'}")
    if json_out:
        with open(json_out, "w", encoding="utf-8") as f:
            json.dump(report, f, ensure_ascii=False, indent=1)
        print(f"（JSON: {json_out}）")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
