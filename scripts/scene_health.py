#!/usr/bin/env python3
"""亮度/标准差体检：标准差 >8 才算"真有画面"（坑 39：只写出来 ≠ 画得有东西）。"""
import sys, os
sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
from desktop_diff import read_png


def stat(p):
    w, h, ch, px = read_png(p)
    vals = [(px[i] + px[i + 1] + px[i + 2]) // 3 for i in range(0, len(px), 4 * 11)]
    m = sum(vals) / len(vals)
    var = sum((v - m) ** 2 for v in vals) / len(vals)
    return int(m), int(var ** 0.5)


if __name__ == "__main__":
    for p in sys.argv[1:]:
        if not os.path.exists(p):
            print(f"{p}: 不存在")
            continue
        a, s = stat(p)
        tag = "有画面 ✓" if s > 8 else "平色/全黑 ✗"
        print(f"{os.path.basename(p):<34} 平均 {a:>4}  标准差 {s:>4}  {tag}")
