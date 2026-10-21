#!/usr/bin/env bash
# Build the wallpaper-hub Debian package (amd64) straight from this source tree.
#
#   ./packaging/build-deb.sh            -> dist/wallpaper-hub_<version>_amd64.deb
#   ./packaging/build-deb.sh --no-build -> reuse existing target/release binaries
#
# Properties that matter:
#   * no sudo anywhere; nothing is installed on the build host
#   * no /tmp: every scratch file lives under <repo>/dist/ (git-ignored)
#   * repeatable: same sources -> same file list; SOURCE_DATE_EPOCH from git so
#     the archive timestamps are pinned
#   * Depends is *computed*, not guessed: dpkg-shlibdeps reads the two ELF files
#     and their NEEDED entries, then maps them to Debian package names
#
# Requires: cargo, dpkg-deb, dpkg-shlibdeps, strip, standard coreutils.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PKG=wallpaper-hub
ARCH="${ARCH:-amd64}"
VERSION="${VERSION:-$(awk -F'"' '/^version = /{print $2; exit}' crates/wallpaper-hub/Cargo.toml)}"
MAINTAINER="${MAINTAINER:-Sunrise-Valley <sunrise-valley@users.noreply.github.com>}"
HOMEPAGE="https://github.com/Sunrise-Valley/WallpaperEngines-linuxWayland"

DIST="$ROOT/dist"
STAGE="$DIST/deb-root"
SHLIB="$DIST/deb-shlibdeps"
OUT="$DIST/${PKG}_${VERSION}_${ARCH}.deb"

BUILD=1
[[ "${1:-}" == "--no-build" ]] && BUILD=0

log() { printf '[build-deb] %s\n' "$*"; }

for tool in cargo dpkg-deb dpkg-shlibdeps strip; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

# ---------------------------------------------------------------- build ----
if [[ $BUILD == 1 ]]; then
  # --offline keeps the build hermetic when the registry cache is warm; fall back
  # to the network on a fresh machine instead of failing the whole run.
  for crate in we-native wallpaper-hub; do
    log "cargo build --release ($crate)"
    ( cd "crates/$crate" &&
      cargo build --release --locked --offline 2>/dev/null ||
      cargo build --release --locked )
  done
else
  log "reusing existing target/release binaries (--no-build)"
fi

WE_NATIVE="$ROOT/crates/we-native/target/release/we-native"
HUB_BIN="$ROOT/crates/wallpaper-hub/target/release/wallpaper-hub"
for b in "$WE_NATIVE" "$HUB_BIN"; do
  [[ -x "$b" ]] || { echo "missing binary: $b" >&2; exit 1; }
done

# ---------------------------------------------------------------- stage ----
log "staging into dist/deb-root"
rm -rf "$STAGE" "$SHLIB"
mkdir -p \
  "$STAGE/DEBIAN" \
  "$STAGE/usr/bin" \
  "$STAGE/usr/lib/systemd/user" \
  "$STAGE/usr/share/applications" \
  "$STAGE/usr/share/icons/hicolor/scalable/apps" \
  "$STAGE/usr/share/doc/$PKG"

# Binaries: copy then strip so the debug/symbol payload of the UI (≈50 MB
# unstripped) does not end up in the package. Copied files also keep the build
# tree untouched.
install -m 0755 "$WE_NATIVE" "$STAGE/usr/bin/we-native"
install -m 0755 "$HUB_BIN"   "$STAGE/usr/bin/wallpaper-hub"
strip --strip-unneeded "$STAGE/usr/bin/we-native" "$STAGE/usr/bin/wallpaper-hub"

# User units live in the *system* path /usr/lib/systemd/user/ — that is where
# systemd looks for vendor units of --user; per-user files in ~/.config shadow
# them and survive upgrades.
install -m 0644 packaging/systemd/wallpaper-native-kernel.service \
  "$STAGE/usr/lib/systemd/user/wallpaper-native-kernel.service"
install -m 0644 packaging/systemd/wallpaper-hub.service \
  "$STAGE/usr/lib/systemd/user/wallpaper-hub.service"

# Desktop entry + icon (single source of truth: the crate's resources/).
install -m 0644 crates/wallpaper-hub/resources/app.desktop \
  "$STAGE/usr/share/applications/io.github.wallpaperhub.WallpaperHub.desktop"
install -m 0644 crates/wallpaper-hub/resources/icons/hicolor/scalable/apps/icon.svg \
  "$STAGE/usr/share/icons/hicolor/scalable/apps/io.github.wallpaperhub.WallpaperHub.svg"

# Docs required by policy (§12.5): copyright + upstream docs.
install -m 0644 README.md       "$STAGE/usr/share/doc/$PKG/README.md"
install -m 0644 ARCHITECTURE.md "$STAGE/usr/share/doc/$PKG/ARCHITECTURE.md"
install -m 0644 packaging/copyright "$STAGE/usr/share/doc/$PKG/copyright"
chmod 0644 "$STAGE/usr/share/doc/$PKG/"*

# ------------------------------------------------------------- Depends ----
# dpkg-shlibdeps wants a debian/control to read the package name from, so give it
# a throwaway one under dist/ (never /tmp) and run it with -O to get the line back.
log "computing Depends with dpkg-shlibdeps"
mkdir -p "$SHLIB/debian"
cat > "$SHLIB/debian/control" <<EOF
Source: $PKG
Section: utils
Priority: optional
Maintainer: $MAINTAINER

Package: $PKG
Architecture: $ARCH
Description: temporary control file for dpkg-shlibdeps
EOF

# dpkg-shlibdeps wraps its output across lines (continuation lines start with a
# space), so unfold it before it goes into a control field — otherwise the
# version constraints get cut in half.
SHLIBS_DEPS="$(
  cd "$SHLIB" &&
  dpkg-shlibdeps -O \
    -e"$STAGE/usr/bin/we-native" \
    -e"$STAGE/usr/bin/wallpaper-hub" 2>/dev/null |
  awk '
    /shlibs:Depends=/ { sub(/^.*shlibs:Depends=/, ""); line = $0; got = 1; next }
    got && /^[[:space:]]/ { sub(/^[[:space:]]+/, ""); line = line $0; next }
    { next }
    END { if (got) print line }
  '
)"
[[ -n "$SHLIBS_DEPS" ]] || { echo "dpkg-shlibdeps produced no Depends" >&2; exit 1; }
log "shlibs:Depends = $SHLIBS_DEPS"

# Things dlopen()'d at runtime, so no NEEDED entry exists and shlibdeps cannot
# see them: the renderer loads libEGL.so.1 (khronos-egl) and the UI's wgpu
# back-end loads libvulkan.so.1. Without these the kernel starts and then fails
# to get a GL context, so they belong in Depends.
EXTRA_DEPS="libegl1, libvulkan1"

# Recommend, do not depend: video wallpapers need ffmpeg and the UI's wgpu path
# needs a Vulkan driver, but neither is required to play scene wallpapers.
# Steam is only Suggests — wallpapers come from the Workshop, yet Recommends
# would make apt pull the i386 Steam client along with a wallpaper tool.
RECOMMENDS="ffmpeg, mesa-vulkan-drivers"
SUGGESTS="steam | steam-installer"

INSTALLED_SIZE="$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: $MAINTAINER
Installed-Size: $INSTALLED_SIZE
Depends: $SHLIBS_DEPS, $EXTRA_DEPS
Recommends: $RECOMMENDS
Suggests: $SUGGESTS
Homepage: $HOMEPAGE
Description: native Wallpaper Engine scene wallpaper engine for Wayland
 Plays Wallpaper Engine "scene" wallpapers natively on Wayland: it parses
 scene.pkg, renders the scene with its own EGL/GLES renderer and puts it on the
 wlr-layer-shell background layer, one surface per output. No Wine, no Electron,
 no WebView, no CEF.
 .
 The package ships two binaries: we-native, the layer-shell kernel that draws the
 wallpaper and owns the control plane, and wallpaper-hub, the COSMIC front-end
 for browsing, applying and tuning wallpapers. Both run as systemd user units
 (wallpaper-native-kernel.service, wallpaper-hub.service).
 .
 Video wallpapers decode through ffmpeg, and wallpapers themselves come from
 Steam Workshop, hence the Recommends.
EOF

# ----------------------------------------------------------- timestamps ----
# Pin every mtime (and hence the tar member times) to the commit date so two
# runs over the same tree produce byte-identical archives.
STAMP="${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)}"
if [[ "$STAMP" -gt 0 ]]; then
  find "$STAGE" -exec touch -h -d "@$STAMP" {} +
  # dpkg-deb honours SOURCE_DATE_EPOCH for the ar member headers; without it the
  # archive mtimes drift and two builds of the same tree differ byte for byte.
  export SOURCE_DATE_EPOCH="$STAMP"
fi

# ---------------------------------------------------------------- build ----
# zstd elsewhere, xz here: xz is present on every dpkg-based distro since forever.
rm -f "$OUT"
log "dpkg-deb --build -> ${OUT#$ROOT/}"
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" >/dev/null

log "done: $OUT"
log "size: $(du -h "$OUT" | cut -f1)  sha256: $(sha256sum "$OUT" | cut -d' ' -f1)"
