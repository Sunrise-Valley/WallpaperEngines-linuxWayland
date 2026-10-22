# Wallpaper Hub

**A native Linux engine for Wallpaper Engine *scene* wallpapers — real layer-shell
wallpaper on Wayland, no Wine, no Electron, no WebView.**

Wallpaper Hub parses the `scene.pkg` files that Wallpaper Engine (WE) ships, plays them
back with its own GL/EGL renderer on a `wlr-layer-shell` background surface, and exposes
a small COSMIC desktop app for browsing, applying and tuning them.

It is not a wrapper. There is no Proton prefix, no bundled Chromium, no CEF, and no
call into Valve's code at runtime. The scene format was reconstructed from the files
themselves and cross-checked against public reimplementations; the renderer is written
from scratch in Rust on top of `glow` + EGL/GLES.

---

## What it does

* **Plays workshop `scene` wallpapers** — reads `scene.pkg` (PKGV container), the `.tex`
  texture containers inside it, and `scene.json`, then renders the scene graph.
* **Real wallpaper layer on Wayland** — one `zwlr_layer_shell_v1` surface per output on
  the *background* layer, sized to the output's physical pixels, so windows, panels and
  the compositor's own effects composite on top of it as usual.
* **Mouse interaction** — pointer position is fed into the renderer, so X-ray style
  effects, parallax layers and mouse-driven particle emitters react to the cursor even
  while the pointer is over other windows (via `ext_image_copy_capture`'s cursor session
  where the compositor supports it, `wl_pointer` otherwise).
* **User properties** — the per-wallpaper properties the author exposed in WE
  (`text`, `color`, `bool`, `combo`, `slider`) are editable in the UI and applied live:
  colours go through a circular colour wheel, text properties use the scene's own
  layout boxes, and scripted properties run the author's embedded JavaScript.
* **Per-scene FPS, time scale and volume** — frame rate is pinned per wallpaper
  (60/30/auto with adaptive downshift), playback speed is a 0.25–3.0 multiplier, and
  audio volume/mute is per wallpaper.
* **Occlusion-aware sleeping** — when a fullscreen window covers the output (or the
  session locks), the kernel stops rendering and `SIGSTOP`s the ffmpeg child; CPU use
  drops to roughly nothing until the desktop is visible again.
* **Audio-reactive scenes** — 16-band spectrum from PipeWire/PulseAudio, either from a
  chosen sink or pushed in over the control plane.
* **Video and image wallpapers too** — `video` wallpapers decode through ffmpeg (with an
  optional in-process VAAPI zero-copy path), images are drawn as-is with correct
  aspect handling.

## Why it is different

| | Typical Linux WE players | Wallpaper Hub |
|---|---|---|
| Runtime | Wine + D3D translation, or an Electron/CEF shell | native Rust, EGL/GLES, no runtime to ship |
| Surface | a normal window parked behind icons, or an X11 root pixmap | `zwlr_layer_shell_v1` background layer per output |
| Scene support | re-implements a subset, or leans on the Windows binary | own `scene.pkg` / `scene.json` / `MDLV` / `TEX` parsers |
| Reproducibility | eyeball it | a single-frame offline renderer (`we-scene-render`) that is deterministic by construction |

Concretely, what the engine implements today:

* **`scene.pkg` parsing** — PKGV container (with its 4-byte length prefix before the
  magic and entry offsets relative to the end of the header), TEX texture containers
  (TEXV/TEXI/TEXB, DXT/BC and raw formats, repeat-wrap flags), and the `scene.json`
  object graph (camera, general, image/particle/text/puppet objects, effects).
* **Pass pipeline with render targets** — the scene is compiled into a list of draw
  passes, each with a target (screen or an off-screen RT), a shader, uniforms, and a
  blend mode; RTs are reference-counted and aliased where lifetimes do not overlap.
* **Puppet / skeletal animation** — MDLV mesh parsing plus per-frame bone matrices
  (MDLSv2) so mesh layers deform instead of being drawn as flat quads.
* **Effect chains** — WE's effect list is composed into a chain of full-screen passes,
  including the fused cases that WE itself merges (same shader, different uniforms),
  with per-effect combos resolved from the scene's `combos` map.
* **Masks and linked layers** — mask geometry is baked into a mask buffer in frame
  space, and "linked layer" RTs are resolved with an explicit dependency graph so a
  layer can sample another layer's output.
* **Text objects** — author text is laid out using the scene's own layout box, rasterised
  with `ab_glyph`, and tinted the way the effect chain expects.
* **GPU frame-rate adaptation** — the kernel measures frame cost and drops the render
  scale (physical → logical pixels) when a scene cannot hold its target rate, telling
  the user exactly which source ("per-scene table / user choice / default / adaptive")
  produced the current value; it never silently overrides a user pin.

---

## Architecture

Three blocks, one control plane, one renderer:

```
                       ┌──────────────────────────────────────────────┐
                       │  UI  —  crates/wallpaper-hub (libcosmic)     │
                       │  browse / apply / properties / settings      │
                       └───────────────┬──────────────────────────────┘
                                       │  $XDG_RUNTIME_DIR/we-native.sock
                                       │  one JSON object per line, one reply per line
                       ┌───────────────▼──────────────────────────────┐
                       │  KERNEL  —  crates/we-native                 │
                       │  layer-shell surfaces per output             │
                       │  apply/pause/quit, props, fps, mouse, audio  │
                       │  watchdog + display/commit guards            │
                       └───────────────┬──────────────────────────────┘
                                       │  in-process calls (SceneRenderer)
                       ┌───────────────▼──────────────────────────────┐
                       │  RENDERER  —  crates/we-scene-render         │
                       │  scene.pkg → pass list → GL/EGL → dmabuf     │
                       │  puppet, effects, masks, linked layers, text │
                       └───────────────┬──────────────────────────────┘
                                       │
                       ┌───────────────▼──────────────────────────────┐
                       │  wl_surface (layer-shell background)         │
                       │  → compositor → display                      │
                       └──────────────────────────────────────────────┘
```

Data flow for one wallpaper:

```
UI: user clicks "apply"
  → {"cmd":"apply","kind":"scene","path":"<workshop dir>"}      (unix socket)
KERNEL: resolves the scene, drops any previous renderer state
  → we_scene::pkg::open()        PKGV → entries
  → we_scene::scene::parse()     scene.json → objects/effects
  → SceneRenderer::open(dir)     textures uploaded, passes compiled
  → per frame: uniform update → draw pass N → … → screen
  → status reply: {"ok":true,"kind":"scene","playing":true,"fps":60,…}
UI: polls status on a timer, pushes user property edits the same way
```

### Repository layout

| Path | What lives there |
|---|---|
| `crates/we-scene/` | Format parsing: PKGV container, TEX textures, `scene.json` model, tolerant JSON reader. No GL, no Wayland — unit-testable on its own. |
| `crates/we-scene-render/` | The renderer: pass compilation, GL context (`gl_ctx`), shader plumbing (`shader`, `fuse`), particles, puppet meshes (`mdl`), text (`text`), property scripts (`propscript`), audio spectrum, parallax, plus small diagnostic binaries and the offline single-frame CLI (`we-scene-render`). |
| `crates/we-native/` | The kernel: Wayland connection, one layer surface per output, control-plane socket, video decode host, audio sinks, sleep/occlusion logic, watchdog, display and commit guards, app-state restore. |
| `crates/wallpaper-hub/` | The COSMIC app: library scanning, workshop metadata browser, detail panel with user properties, settings, per-scene performance table. |
| `scripts/` | Standalone tools: `we-ctl` (control-plane client), `raw2png.py`, `png_crop.py`, `img_region_diff.py`, `sharpness.py`, `checker_png.py`, `row_displacement.py`, `scene_health.py`, sysroot helpers. |
| `packaging/` | systemd user units and `build-deb.sh` (the `.deb` builder). |
| `docs/` | `BUILDING.md` (how to build and run). `ARCHITECTURE.md` lives at the repo root. |

`ARCHITECTURE.md` goes into the renderer's pass model, the mask/RT graph, the control
protocol and the failure-handling guards in detail.

---

## Building

```sh
# each crate is standalone (own Cargo.lock): use --manifest-path from the repo root
cargo build --release --manifest-path crates/we-scene/Cargo.toml
cargo build --release --manifest-path crates/we-scene-render/Cargo.toml
cargo build --release --manifest-path crates/we-native/Cargo.toml

# the COSMIC app pulls libcosmic from git and needs the usual libcosmic dev deps
cargo build --release --manifest-path crates/wallpaper-hub/Cargo.toml
```

All four crates pass `cargo check --offline` with a warm dependency cache. Full
prerequisites, EGL/driver notes, the ffmpeg header requirement and the offline-build recipe
are in [`docs/BUILDING.md`](docs/BUILDING.md). Short version: a Rust toolchain, EGL + GLES 3
drivers, `libwayland`, and — for the UI — the libcosmic system dependencies.

Run it:

```sh
# the kernel's control socket lives in $XDG_RUNTIME_DIR
crates/we-native/target/release/we-native --serve "$XDG_RUNTIME_DIR/we-native.sock" &
scripts/we-ctl status
scripts/we-ctl apply-scene ~/.local/share/Steam/steamapps/workshop/content/431960/<id>

# or use the GUI, which speaks the same socket
crates/wallpaper-hub/target/release/wallpaper-hub
```

## Install (Debian/Ubuntu)

> ### ⚠️ Read this first: the prebuilt package needs `libc6 >= 2.43`
>
> The `.deb` in [`dist/`](dist/) was built on **Debian forky** and therefore declares
> **`libc6 (>= 2.43)`**. It will **not install** on Debian 13 (trixie), Ubuntu 24.04/22.04,
> or any older distribution — `apt` will stop with *unmet dependencies*.
>
> This is a build-host artifact, not a hard requirement of the code. To get a package that
> matches your system, build it **on that system**:
>
> ```sh
> git clone https://github.com/Sunrise-Valley/WallpaperEngines-linuxWayland
> cd WallpaperEngines-linuxWayland
> packaging/build-deb.sh          # needs a Rust toolchain + the dev headers in docs/BUILDING.md
> sudo apt install ./dist/wallpaper-hub_0.1.0_amd64.deb
> ```
>
> Read `libc6` of your own system with `ldd --version | head -1`; if it is below 2.43 the
> prebuilt file is not for you.

A prebuilt `amd64` package is built from this tree; you do not need a Rust toolchain to
install it.

```sh
# 1. get the package (release asset, or build it yourself: see below)
sudo apt install ./dist/wallpaper-hub_0.1.0_amd64.deb   # file lives in dist/ in this repo

# 2. start it for the current user; the units are user units, no root involved
systemctl --user daemon-reload
systemctl --user enable --now wallpaper-hub.service
```

`apt install ./file.deb` (rather than `dpkg -i`) is deliberate: apt resolves the
`Depends` computed from the binaries with `dpkg-shlibdeps` and pulls the Wayland, EGL and
font libraries in one step. `dpkg -i` works too, but leaves you to `apt-get -f install`.

The binaries are dynamically linked against the glibc of the machine that built the
package, so `Depends` pins it (`libc6 (>= 2.43)` for a Debian 14/forky build). On an
older release — Debian 13, Ubuntu 24.04 — apt refuses the package; run
`./packaging/build-deb.sh` there (or in a container of that release) to get a build whose
`Depends` matches. Nothing else about the package changes.

What lands where:

| Path | Contents |
|---|---|
| `/usr/bin/we-native` | the layer-shell kernel |
| `/usr/bin/wallpaper-hub` | the COSMIC front-end |
| `/usr/lib/systemd/user/wallpaper-native-kernel.service` | kernel unit (`--serve %t/we-native.sock`) |
| `/usr/lib/systemd/user/wallpaper-hub.service` | UI unit, pulls in the kernel unit |
| `/usr/share/applications/io.github.wallpaperhub.WallpaperHub.desktop` | launcher entry |
| `/usr/share/icons/hicolor/scalable/apps/io.github.wallpaperhub.WallpaperHub.svg` | icon |
| `/usr/share/doc/wallpaper-hub/` | `README.md`, `ARCHITECTURE.md`, `copyright` |

Both units are `WantedBy=graphical-session.target`, so enabling them once is enough —
they reconnect on every login. `wallpaper-hub.service` starts the kernel too; you can
enable just the kernel (`systemctl --user enable --now wallpaper-native-kernel.service`)
if you prefer to open the UI by hand.

Rebuild the package from source (no sudo, no `/tmp`, output in `dist/`):

```sh
./packaging/build-deb.sh             # build both binaries, then dpkg-deb
./packaging/build-deb.sh --no-build  # reuse crates/*/target/release
```

Uninstall:

```sh
systemctl --user disable --now wallpaper-hub.service
sudo apt remove wallpaper-hub
```

## Status and limitations

* **Tested on a limited sample of scenes.** The renderer has not been validated against
  the whole workshop, so we cannot guarantee that every wallpaper reproduces exactly.
  If a scene looks wrong, please open an issue with the workshop ID.
* **Rendering cost grows with the scene.** A full-screen scene with many effect passes is
  GPU-heavy; the adaptive frame-rate logic exists for that reason.
* **Wayland only** (COSMIC is the tested compositor); X11 is not implemented.
* **No Steam integration.** The UI reads the local workshop directory; it never logs in,
  subscribes, downloads, or modifies your Steam library.

## Environment variables

These are features, not debug leftovers — grouped by what they change.

**Performance / quality**

| Variable | Effect |
|---|---|
| `WE_FPS` / `WE_SCENE_FPS` | target frame rate / per-scene table override |
| `WE_RENDER_SCALE` | `physical` (sharp) or `logical` (cheaper) render size |
| `WE_EFFECTS` | enable/disable the effect chain |
| `WE_SLEEP_MODE` | `never` / `fullscreen` / `maximized` sleep policy |
| `WE_SCENE_ZEROCOPY`, `WE_VIDEO_ZEROCOPY` | dmabuf zero-copy paths |
| `WE_PROFILE_PASSES`, `WE_PERF_SCISSOR`, `WE_PERF_RES_SCALE` | renderer profiling and bandwidth savers |
| `WE_SETPROP_FAST`, `WE_SETPROP_RECT` | partial refresh when only a property changed |

**Behaviour / integration**

| Variable | Effect |
|---|---|
| `WE_FFMPEG`, `WE_FFPROBE`, `WE_FFMPEG_LIB`, `WE_VAAPI_DEVICE` | where ffmpeg lives, which VAAPI node to use |
| `WE_AUDIO_SOURCE`, `WE_AUDIO_DEVICE`, `WE_AUDIO_SPECTRUM` | audio spectrum input |
| `WE_PROP_<NAME>=<value>` | pre-set a user property |
| `WE_POINTER`, `WE_POINTER_GLOBAL`, `WE_MOUSE_OVERRIDE_MS` | pointer input source |
| `WE_RESTORE_APP_STATE` | restore per-scene props/fps after a kernel restart |
| `WE_WATCHDOG*`, `WE_FADE*` | stall watchdog and fade behaviour |

**Diagnostics (off by default, no cost on the hot path)** — `WE_MOUSE_DEBUG`,
`WE_LOOP_SLOW_MS`, `WE_WATCHDOG_DUMP`, `WE_COMMIT_DEADLINE_MS`, the `--debug-*` fault
injection flags in `docs/BUILDING.md`, and `WE_FUSE_DUMP_DIR` for the effect-chain
regression test. The kernel can be told to fake a stalled main loop or a dead compositor
so the recovery paths are testable without crashing a real session; those flags take
effect only when passed explicitly.

## Legal

* **This repository contains no Steam content and no wallpaper assets.** No `scene.pkg`,
  no `.tex`, no ripped shaders, no workshop files, no screenshots of other people's
  wallpapers.
* **You need your own copy of Wallpaper Engine** (and the workshop items you want to
  display) for this to be useful. The engine only reads files you already own, from the
  Steam library on your own machine.
* **No Wallpaper Engine code is used or included.** The format knowledge here was
  derived from the data files and from public reimplementation projects; no Valve or
  Wallpaper Engine binaries, shaders or source are redistributed. Wallpaper Engine is a
  trademark of its respective owner; this project is not affiliated with or endorsed by
  Valve or the Wallpaper Engine authors.
* The vendored `proto/control.proto` schema comes from the MIT-licensed `waywallen`
  project (see `crates/wallpaper-hub/proto/NOTICE`); it describes a data model only.
* Project code is MIT (see `LICENSE`). Dependencies keep their own licenses
  (`libcosmic` is MPL-2.0, `glow`/`khronos-egl`/`smithay-client-toolkit` are MIT/Apache-2.0).

## Contributing

Bug reports with the scene id and a description of what looks wrong are the most useful
thing you can send. Please read [`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a
PR — in particular the rule about not attaching copyrighted wallpaper assets.

---

## 中文摘要

**Wallpaper Hub 是一个 Linux 原生的 Wallpaper Engine「场景」壁纸引擎**，不是 Wine /
Electron / WebView 套壳：它自己解析 `scene.pkg`（PKGV 容器 / TEX 贴图 / `scene.json`
场景图），用自己的 GL/EGL 渲染管线在 `wlr-layer-shell` 背景层上真正把画面画到桌面上。

能干什么：播放创意工坊里的 scene 壁纸、多显示器各开一个真壁纸层、鼠标交互（X-Ray、
视差、粒子）、场景内作者暴露的用户属性（文字 / 颜色 / 开关 / 滑杆）实时生效、逐场景
帧率 / 播放速度 / 音量、被全屏窗口遮挡时停渲染并 `SIGSTOP` 解码进程、音频频谱驱动。

特点与优势：Wayland 原生 layer-shell（不是「藏在图标后的窗口」）；无 Electron/CEF 运行时；
自研 scene.pkg 解析 + 渲染管线（puppet 骨骼、特效链、渲染目标、遮罩、链接层、文字对象）；
GPU 帧率自适应；单帧离线渲染器让「同参数同像素」可复现、可回归。

架构三块：UI（libcosmic）→ 控制面（Unix socket，一行一个 JSON）→ 内核（layer-shell +
`wl_surface`）→ 渲染器（场景解析 / 逐趟管线 / GL）。目录结构见上表。

当前限制：仅验证过 17 个场景；部分特效与遮罩语义仍在与原引擎对齐；性能吃 GPU；只做了
Wayland（COSMIC 上测的）；没有任何 Steam 登录 / 订阅 / 下载行为。

法律：仓库**不含任何 Steam 内容或壁纸资源**，你需要自己拥有 Wallpaper Engine；未使用其
代码；`waywallen` 的 proto 数据模型（MIT）已注明来源。项目代码 MIT。
