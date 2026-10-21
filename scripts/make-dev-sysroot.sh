#!/bin/bash
# make-dev-sysroot.sh —— 无 root 取得编译 libcosmic 所需的 -dev 头文件与 pkg-config 文件
#
# 原理：运行时 .so 系统里已经有了（libxkbcommon0/libwayland-client0/libvulkan1/libudev1），
# 缺的只是头文件、.pc 和 unversioned 的 .so 链接。把 -dev 包下下来解到本地前缀即可。
# 注意：-dev 包里的 libfoo.so 是指向 libfoo.so.0 的相对链接，而 .so.0 不在解出来的前缀里，
# 所以要把它重指到系统上的真实文件，否则链接器会拿到坏链接。
set -e

W=${SYSROOT_WORK:-$HOME/.cache/dev-sysroot}
S=${SYSROOT:-$HOME/.sysroot}
PKGS=${PKGS:-"libxkbcommon-dev libwayland-dev libvulkan-dev libudev-dev"}

mkdir -p "$W" "$S"
cd "$W"

echo "=== 1/3 下载 -dev 包 ==="
for p in $PKGS; do
  printf "  %-22s " "$p"
  apt-get download "$p" >/dev/null 2>&1 && echo "ok" || echo "失败"
done
ls -la *.deb 2>/dev/null | awk '{print "   ", $9, $5" bytes"}'

echo "=== 2/3 解包到 $S ==="
for f in *.deb; do
  dpkg -x "$f" "$S"
  echo "   解出 $f"
done

echo "=== 3/3 修复指向缺失运行时库的 .so 链接 ==="
fixed=0; broken=0
while IFS= read -r l; do
  if [ ! -e "$l" ]; then
    base=$(basename "$l")
    real=$(ls /usr/lib/*/"${base}".* 2>/dev/null | grep -v '\.so$' | head -1)
    if [ -n "$real" ]; then ln -sf "$real" "$l"; fixed=$((fixed+1));
    else echo "   ⚠ 找不到 ${base}.* 的真实库: $l"; broken=$((broken+1)); fi
  fi
done < <(find "$S" -name "*.so" -type l)
echo "   修复 $fixed 个，未解决 $broken 个"

echo "=== .pc 清单 ==="
find "$S" -name "*.pc" | sort | sed 's/^/   /'
