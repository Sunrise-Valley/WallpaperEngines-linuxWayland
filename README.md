# Wallpaper Hub

**[中文](README.md) | [English](README.en.md)**

**Linux 原生的 Wallpaper Engine「场景」壁纸引擎 —— 在 Wayland 上是真正的 layer-shell
壁纸层，不用 Wine、不用 Electron、不用 WebView。**

Wallpaper Hub 自己解析 Wallpaper Engine（下称 WE）随壁纸附带的 `scene.pkg`，用自研的
GL/EGL 渲染器把画面画到 `wlr-layer-shell` 背景层上，并提供一个轻量的 COSMIC 桌面应用，
用来浏览、应用和调整壁纸。

它不是套壳：没有 Proton 前缀，没有内置 Chromium，没有 CEF，运行时不调用 Valve 的任何
代码。场景格式是从文件本身逆向出的结构，并与公开的重实现交叉验证；渲染器是在 Rust 里
从零写的，底层是 `glow` + EGL/GLES。

**引擎不依赖 Steam 客户端。** 它不登录、不订阅、不下载，甚至完全不需要装 Steam。它只读
磁盘上普通的目录里的场景壁纸 —— 见[壁纸从哪来](#壁纸从哪来)。

**版本：** 0.1.0 · **Changelog：** [CHANGELOG.md](CHANGELOG.md) —— 改了什么，以及预编译
包的 `libc6` 注意事项。

---

## 壁纸从哪来

一条场景壁纸就是一个目录：`project.json`（WE 工程文件）+ `scene.pkg`（打包后的场景）+
一张预览图。引擎扫描**本地壁纸库根目录**，把找到的目录列出来。

| 来源 | 默认值 | 说明 |
|---|---|---|
| **本地壁纸库根目录** | `~/Templates` | 主要来源；每个子目录就是一条壁纸 |
| Steam 创意工坊内容 | `~/.local/share/Steam/steamapps/workshop/content/431960` | **可选**；只在它存在时才扫 |

库根目录里的布局 —— 三种形态都支持：

```
~/Templates/
├── 3704273480/         # 目录名就是工坊 id（Steam 当初的命名）
│   ├── project.json
│   ├── scene.pkg
│   └── preview.gif
├── my-favourite-scene/ # 任意名字；显示名取自 project.json 里的 "title"
│   └── …
└── one-more/           # 根目录本身就是一个工程，也可以
    ├── project.json
    └── scene.pkg
```

配置根目录（优先级从高到低）：

1. 环境变量 `WPHUB_LIBRARY_DIRS` —— 冒号分隔，`~` 会被展开：
   `WPHUB_LIBRARY_DIRS=~/Templates:/mnt/walls`
2. 应用里的 `library_dirs` 列表（COSMIC 设置 → **壁纸库 / Library**）
3. `~/.local/share/wallpaper-hub/library_dirs`，一行一个路径 —— 界面写这个文件，常驻内核
   读它，所以改根目录不需要重启内核
4. 什么都没配 ⇒ `~/Templates`

内核的 `apply` 既接受绝对路径，也接受只给壁纸 id；给 id 时它会在同一批根目录里查找，
所以一条壁纸从 Steam 目录挪到 `~/Templates` 之后照样能用。

目录一律**只读**打开。不会往 Steam 库目录里写任何东西，也不会读取或保存 Steam 账号、
token 或 SteamID。

## 它能做什么

* **播放 `scene` 壁纸** —— 读 `scene.pkg`（PKGV 容器）、里面的 `.tex` 贴图容器和
  `scene.json`，然后渲染场景图。创意工坊下载来的和手工拷来的目录同等对待。
* **Wayland 上真正的壁纸层** —— 每个输出一个 `zwlr_layer_shell_v1` surface，放在
  *background* 层，按输出的物理像素定尺寸，所以窗口、面板和合成器自己的特效照常叠在它
  上面。
* **鼠标交互** —— 指针位置会喂给渲染器，所以 X-Ray 类特效、视差层和鼠标驱动的粒子发射器
  会跟着光标反应，即使指针正压在别的窗口上（合成器支持时走 `ext_image_copy_capture` 的
  cursor session，否则走 `wl_pointer`）。
* **用户属性** —— 作者在 WE 里暴露的逐壁纸属性（`text`、`color`、`bool`、`combo`、
  `slider`）可以在界面里编辑并实时生效：颜色走圆形色轮，文本属性用场景自带的布局框，
  脚本属性执行作者内嵌的 JavaScript。
* **逐场景帧率、时间倍率与音量** —— 帧率按壁纸固定（60/30/自动，自动会自适应降档），
  播放速度是 0.25–3.0 的倍率，音频音量/静音也是逐壁纸的。
* **感知遮挡的休眠** —— 全屏窗口盖住输出（或会话锁屏）时，内核停止渲染并对 ffmpeg
  子进程发 `SIGSTOP`；CPU 占用降到几乎为零，桌面重新可见后恢复。
* **音频响应场景** —— 从 PipeWire/PulseAudio 取 16 段频谱，来源可以是选定的 sink，也可
  以从控制面推入。
* **视频与图片壁纸也支持** —— `video` 壁纸经 ffmpeg 解码（可选进程内 VAAPI 零拷贝路径），
  图片按原样绘制并正确处理宽高比。

## 为什么不一样

| | 常见的 Linux WE 播放器 | Wallpaper Hub |
|---|---|---|
| 运行时 | Wine + D3D 转译，或 Electron/CEF 外壳 | 原生 Rust、EGL/GLES，没有要一起分发的运行时 |
| 承载面 | 一个藏在图标后面的普通窗口，或 X11 root pixmap | 每输出一个 `zwlr_layer_shell_v1` 背景层 |
| 场景支持 | 只重实现一个子集，或者靠 Windows 二进制 | 自研的 `scene.pkg` / `scene.json` / `MDLV` / `TEX` 解析器 |
| 可复现性 | 靠眼睛看 | 单帧离线渲染器（`we-scene-render`），构造上就是确定性的 |

具体到今天引擎实现了什么：

* **`scene.pkg` 解析** —— PKGV 容器（magic 前有 4 字节长度前缀，entry 偏移相对 header
  末尾）、TEX 贴图容器（TEXV/TEXI/TEXB、DXT/BC 与 raw 格式、repeat-wrap 标志），以及
  `scene.json` 对象图（camera、general、image/particle/text/puppet 对象、特效）。
* **带渲染目标的 pass 管线** —— 场景被编译成一串 draw pass，每个带目标（屏幕或离屏
  RT）、shader、uniform 和混合模式；RT 引用计数，生命周期不重叠的会做别名复用。
* **puppet / 骨骼动画** —— 解析 MDLV 网格，加上逐帧骨骼矩阵（MDLSv2），所以网格层是变形
  的，而不是当平面四边形画。
* **特效链** —— WE 的特效列表被组合成一串全屏 pass，包括 WE 自己也会合并的融合情形（同
  shader、不同 uniform），每个特效的 combo 从场景的 `combos` 表解析。
* **遮罩与链接层** —— 遮罩几何烘焙到帧空间的 mask buffer；「链接层」的 RT 用显式的依赖
  图解析，好让一层能采样另一层的输出。
* **文本对象** —— 作者文本按场景自带的布局框排版，用 `ab_glyph` 光栅化，再按特效链期望
  的方式上色。
* **GPU 帧率自适应** —— 内核测量帧开销，场景扛不住目标帧率时降渲染比例（物理 → 逻辑
  像素），并明确告诉用户当前值来自哪个来源（「逐场景表 / 用户选择 / 默认 / 自适应」）；
  绝不静默覆盖用户钉死的设置。

---

## 架构设计

三块，一个控制面，一个渲染器：

```
                       ┌──────────────────────────────────────────────┐
                       │  UI  —  crates/wallpaper-hub (libcosmic)     │
                       │  浏览 / 应用 / 属性 / 设置                    │
                       └───────────────┬──────────────────────────────┘
                                       │  $XDG_RUNTIME_DIR/we-native.sock
                                       │  一行一个 JSON，一行一个回复
                       ┌───────────────▼──────────────────────────────┐
                       │  内核  —  crates/we-native                   │
                       │  每输出一个 layer-shell surface              │
                       │  apply/pause/quit、属性、fps、鼠标、音频      │
                       │  watchdog + display/commit 守卫              │
                       └───────────────┬──────────────────────────────┘
                                       │  进程内调用 (SceneRenderer)
                       ┌───────────────▼──────────────────────────────┐
                       │  渲染器  —  crates/we-scene-render           │
                       │  scene.pkg → pass 列表 → GL/EGL → dmabuf     │
                       │  puppet、特效、遮罩、链接层、文本             │
                       └───────────────┬──────────────────────────────┘
                                       │
                       ┌───────────────▼──────────────────────────────┐
                       │  wl_surface（layer-shell 背景层）             │
                       │  → 合成器 → 显示器                            │
                       └──────────────────────────────────────────────┘
```

一条壁纸的数据流：

```
UI：用户点「应用」
  → {"cmd":"apply","kind":"scene","path":"<壁纸目录>"}            (unix socket)
内核：解析场景，先丢掉上一份渲染器状态
  → we_scene::pkg::open()        PKGV → entries
  → we_scene::scene::parse()     scene.json → objects/effects
  → SceneRenderer::open(dir)     上传贴图、编译 pass
  → 逐帧：更新 uniform → 画第 N 趟 pass → … → 屏幕
  → 状态回复：{"ok":true,"kind":"scene","playing":true,"fps":60,…}
UI：定时轮询状态，用户改属性也走同一条路推下去
```

### 仓库目录

| 路径 | 里面有什么 |
|---|---|
| `crates/we-scene/` | 格式解析：PKGV 容器、TEX 贴图、`scene.json` 模型、容错的 JSON 读取。不含 GL、不含 Wayland —— 可独立单元测试。 |
| `crates/we-scene-render/` | 渲染器：pass 编译、GL 上下文（`gl_ctx`）、shader 管线（`shader`、`fuse`）、粒子、puppet 网格（`mdl`）、文本（`text`）、属性脚本（`propscript`）、音频频谱、视差，以及一些小诊断程序和离线单帧 CLI（`we-scene-render`）。 |
| `crates/we-native/` | 内核：Wayland 连接、每输出一个 layer surface、控制面 socket、视频解码宿主、音频 sink、休眠/遮挡逻辑、watchdog、display 与 commit 守卫、应用状态恢复。 |
| `crates/wallpaper-hub/` | COSMIC 应用：库扫描、工坊元数据浏览、带用户属性的详情面板、设置、逐场景性能表。 |
| `scripts/` | 独立小工具：`we-ctl`（控制面客户端）、`raw2png.py`、`png_crop.py`、`img_region_diff.py`、`sharpness.py`、`checker_png.py`、`row_displacement.py`、`scene_health.py`、sysroot 辅助脚本。 |
| `packaging/` | systemd user unit 和 `build-deb.sh`（`.deb` 构建脚本）。 |
| `docs/` | `BUILDING.md`（怎么构建、怎么跑）。`ARCHITECTURE.md` 在仓库根。 |

`ARCHITECTURE.md` 详细讲了渲染器的 pass 模型、遮罩/RT 图、控制协议和故障处理守卫。

---

## 构建

```sh
# 每个 crate 都独立（各带 Cargo.lock）：从仓库根用 --manifest-path
cargo build --release --manifest-path crates/we-scene/Cargo.toml
cargo build --release --manifest-path crates/we-scene-render/Cargo.toml
cargo build --release --manifest-path crates/we-native/Cargo.toml

# COSMIC 应用从 git 拉 libcosmic，需要常规的 libcosmic 开发依赖
cargo build --release --manifest-path crates/wallpaper-hub/Cargo.toml
```

四个 crate 在依赖缓存热的情况下都能过 `cargo check --offline`。完整前置条件、EGL/驱动
说明、ffmpeg 头文件要求和离线构建配方都在 [`docs/BUILDING.md`](docs/BUILDING.md)。短版：
一套 Rust 工具链、EGL + GLES 3 驱动、`libwayland`，以及界面需要的 libcosmic 系统依赖。

运行：

```sh
# 内核的控制 socket 放在 $XDG_RUNTIME_DIR
crates/we-native/target/release/we-native --serve "$XDG_RUNTIME_DIR/we-native.sock" &
scripts/we-ctl status
scripts/we-ctl apply-scene ~/Templates/<id>       # 只给 id 也行

# 或者直接用 GUI，它说的是同一个 socket
crates/wallpaper-hub/target/release/wallpaper-hub
```

## 安装 (Debian/Ubuntu)

> ### ⚠️ 先读这一段：预编译包要求 `libc6 >= 2.43`
>
> [`dist/`](dist/) 里的 `.deb` 是在 **Debian forky** 上构建的，因此声明
> **`libc6 (>= 2.43)`**。它**装不上** Debian 13 (trixie)、Ubuntu 24.04/22.04 或任何更老
> 的发行版 —— `apt` 会以 *unmet dependencies* 中止。
>
> 这是构建宿主造成的，不是代码的硬性要求。想要一个匹配你系统的包，就**在那个系统上**
> 自己构建：
>
> ```sh
> git clone https://github.com/Sunrise-Valley/WallpaperEngines-linuxWayland
> cd WallpaperEngines-linuxWayland
> packaging/build-deb.sh          # 需要 Rust 工具链 + docs/BUILDING.md 里的开发头文件
> sudo apt install ./dist/wallpaper-hub_0.1.0_amd64.deb
> ```
>
> 用 `ldd --version | head -1` 看你自己的 `libc6`；低于 2.43 的话，预编译文件不是给你的。

`amd64` 预编译包就是从这棵树构建的；装它不需要 Rust 工具链。

```sh
# 1. 拿到包（release 附件，或自己构建：见下）
sudo apt install ./dist/wallpaper-hub_0.1.0_amd64.deb   # 文件在本仓库 dist/ 里

# 2. 给当前用户启动；unit 都是 user unit，不需要 root
systemctl --user daemon-reload
systemctl --user enable --now wallpaper-hub.service
```

用 `apt install ./file.deb`（而不是 `dpkg -i`）是有意的：apt 会解析由 `dpkg-shlibdeps`
从二进制算出来的 `Depends`，一步把 Wayland、EGL 和字体库拉齐。`dpkg -i` 也能用，但剩下
的依赖要你自己 `apt-get -f install`。

二进制是动态链接到构建那台机器的 glibc 上的，所以 `Depends` 把它钉死了（Debian
14/forky 构建出来是 `libc6 (>= 2.43)`）。在更老的发行版上 —— Debian 13、Ubuntu 24.04 ——
apt 会拒绝这个包；在那里（或那个发行版的容器里）跑 `./packaging/build-deb.sh`，得到
`Depends` 匹配的构建。包的其他内容不变。

哪些东西装到哪：

| 路径 | 内容 |
|---|---|
| `/usr/bin/we-native` | layer-shell 内核 |
| `/usr/bin/wallpaper-hub` | COSMIC 前端 |
| `/usr/lib/systemd/user/wallpaper-native-kernel.service` | 内核 unit（`--serve %t/we-native.sock`） |
| `/usr/lib/systemd/user/wallpaper-hub.service` | 界面 unit，会拉起内核 unit |
| `/usr/share/applications/io.github.wallpaperhub.WallpaperHub.desktop` | 启动器条目 |
| `/usr/share/icons/hicolor/scalable/apps/io.github.wallpaperhub.WallpaperHub.svg` | 图标 |
| `/usr/share/doc/wallpaper-hub/` | `README.md`（中文）、`README.en.md`、`ARCHITECTURE.md`、`CHANGELOG.md`、`copyright` |

两个 unit 都是 `WantedBy=graphical-session.target`，所以启用一次就够 —— 每次登录都会重连。
`wallpaper-hub.service` 会一并启动内核；如果你喜欢手动开界面，也可以只启用内核
（`systemctl --user enable --now wallpaper-native-kernel.service`）。

重启策略是故意不同的。`wallpaper-hub.service` 是 `Restart=on-failure` 加
`SuccessExitStatus=0`，所以**关窗口是正常退出，不会被重新拉起**；只有崩溃（非零退出或被
信号杀掉）才重启。`wallpaper-native-kernel.service` 同样是 `Restart=on-failure`，但内核
非零退出一定意味着真出事了 —— 渲染器 watchdog 卡死时就以非零退出 —— 所以崩掉的内核会被
拉回来。

从源码重建包（不用 sudo、不用 `/tmp`，产物在 `dist/`）。用它来处理上一个 release 留下的
旧 `.deb`，或者任何 `libc6`/Wayland/ffmpeg soname 与构建宿主不同的发行版：脚本会重编两个
二进制并在你运行它的机器上重算 `Depends`，结果就匹配你的系统。

```sh
./packaging/build-deb.sh             # 构建两个二进制，然后 dpkg-deb
./packaging/build-deb.sh --no-build  # 复用 crates/*/target/release
```

卸载：

```sh
systemctl --user disable --now wallpaper-hub.service
sudo apt remove wallpaper-hub
```

## 状态与已知限制

* **测试样本有限。** 渲染器没有在整个创意工坊上验证过，**无法保证所有壁纸都能完全还原**。
  哪条场景看起来不对，欢迎带工坊 id 提 issue。
* **渲染开销随场景增长。** 满屏、特效趟数多的场景很吃 GPU；自适应帧率逻辑就是为此存在
  的。
* **仅 Wayland**（COSMIC 是测过的合成器）；X11 没有实现。
* **没有 Steam 集成，也不需要。** 界面扫描本地壁纸库目录（默认 `~/Templates`），以及
  Steam 创意工坊内容目录 —— 仅当它恰好存在。它从不登录、订阅、下载或修改 Steam 库，
  在没有装 Steam 客户端的机器上属于完全受支持的用法。

## 环境变量

这些是功能，不是调试残留 —— 按改变什么分组。

**性能 / 画质**

| 变量 | 作用 |
|---|---|
| `WE_FPS` / `WE_SCENE_FPS` | 目标帧率 / 覆盖逐场景表 |
| `WE_RENDER_SCALE` | `physical`（清晰）或 `logical`（更省）的渲染尺寸 |
| `WE_EFFECTS` | 开/关特效链 |
| `WE_SLEEP_MODE` | `never` / `fullscreen` / `maximized` 休眠策略 |
| `WE_SCENE_ZEROCOPY`、`WE_VIDEO_ZEROCOPY` | dmabuf 零拷贝路径 |
| `WE_PROFILE_PASSES`、`WE_PERF_SCISSOR`、`WE_PERF_RES_SCALE` | 渲染器剖析与带宽节省 |
| `WE_SETPROP_FAST`、`WE_SETPROP_RECT` | 只改属性时做局部刷新 |

**行为 / 集成**

| 变量 | 作用 |
|---|---|
| `WE_FFMPEG`、`WE_FFPROBE`、`WE_FFMPEG_LIB`、`WE_VAAPI_DEVICE` | ffmpeg 在哪、用哪个 VAAPI 节点 |
| `WE_AUDIO_SOURCE`、`WE_AUDIO_DEVICE`、`WE_AUDIO_SPECTRUM` | 音频频谱输入 |
| `WE_PROP_<NAME>=<value>` | 预置一个用户属性 |
| `WE_POINTER`、`WE_POINTER_GLOBAL`、`WE_MOUSE_OVERRIDE_MS` | 指针输入来源 |
| `WE_RESTORE_APP_STATE` | 内核重启后恢复逐场景的属性/fps |
| `WPHUB_LIBRARY_DIRS` | 冒号分隔的本地壁纸根目录（默认 `~/Templates`）；覆盖配置列表和状态文件 |
| `WPHUB_NO_AUTO_APPLY` | 内核空闲时不要重新应用上一条壁纸 |
| `WE_WATCHDOG*`、`WE_FADE*` | stall watchdog 与淡入淡出行为 |

**诊断（默认关闭，热路径上零开销）** —— `WE_MOUSE_DEBUG`、`WE_LOOP_SLOW_MS`、
`WE_WATCHDOG_DUMP`、`WE_COMMIT_DEADLINE_MS`、`docs/BUILDING.md` 里的 `--debug-*` 故障
注入开关，以及特效链回归测试用的 `WE_FUSE_DUMP_DIR`。可以让内核假装主循环卡住或合成器
死掉，这样不用搞崩真实会话也能测恢复路径；这些开关只在显式传入时才生效。

## 法律

* **本仓库不含任何 Steam 内容，也不含任何壁纸资源。** 没有 `scene.pkg`、没有 `.tex`、
  没有扒出来的 shader、没有工坊文件、没有别人壁纸的截图。
* **壁纸是你自己带来的。** 引擎只播放你磁盘上已经存在的场景目录 —— 它不获取、不下载、
  不打包分发它们，也从不与 Steam 通信。具体是哪些目录由你决定（见[壁纸从哪来](#壁纸从哪来)）；
  Steam 只是若干可选来源之一，不是必需的。
* **没有使用也没有包含 Wallpaper Engine 的代码。** 这里的格式知识来自数据文件和公开的
  重实现项目；没有分发 Valve 或 Wallpaper Engine 的任何二进制、shader 或源码。
  Wallpaper Engine 是其各自所有者的商标；本项目与 Valve 或 Wallpaper Engine 作者无关，
  也未获其背书。
* 内置的 `proto/control.proto` 数据模式来自 MIT 许可的 `waywallen` 项目（见
  `crates/wallpaper-hub/proto/NOTICE`）；它只描述数据模型。
* 项目代码为 MIT（见 `LICENSE`）。依赖各自保留自己的许可（`libcosmic` 是 MPL-2.0，
  `glow`/`khronos-egl`/`smithay-client-toolkit` 是 MIT/Apache-2.0）。

## 参与贡献

带场景 id 和「哪里看起来不对」描述的 bug 报告是最有用的东西。开 PR 之前请先读
[`CONTRIBUTING.md`](CONTRIBUTING.md) —— 尤其是那条不要附带受版权保护的壁纸资源的规则。

---

英文版：[**README.en.md（English）**](README.en.md) · 版本 0.1.0 · [CHANGELOG.md](CHANGELOG.md)
