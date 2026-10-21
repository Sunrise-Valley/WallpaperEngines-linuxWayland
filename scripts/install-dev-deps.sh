#!/bin/bash
# Install the system dev libraries needed to build a libcosmic (Rust/iced) app.
# Needs root (sudo). This installs distribution packages, nothing else.
set -e
export DEBIAN_FRONTEND=noninteractive

echo "=== 1/2 必需的 dev 库 ==="
apt-get update -qq
apt-get install -y --no-install-recommends \
  libxkbcommon-dev \
  libwayland-dev \
  libvulkan-dev \
  libudev-dev \
  libssl-dev
echo "  ✅ 必需库安装完成"

echo "=== 2/2 可选工具（失败不影响） ==="
apt-get install -y --no-install-recommends just || echo "  （just 装不上，可稍后用 cargo install just）"
apt-get install -y --no-install-recommends libfontconfig1-dev || true

echo "=== 校验 ==="
for p in libxkbcommon-dev libwayland-dev libvulkan-dev libudev-dev libssl-dev; do
  if dpkg -s "$p" >/dev/null 2>&1; then printf "  ✅ %-20s %s\n" "$p" "$(dpkg -s $p | awk '/^Version/{print $2}')"
  else printf "  ❌ %-20s 未安装\n" "$p"; fi
done
pkg-config --modversion xkbcommon wayland-client vulkan 2>/dev/null | sed 's/^/  pkg-config 版本: /'
echo "APT_DEPS_DONE"
