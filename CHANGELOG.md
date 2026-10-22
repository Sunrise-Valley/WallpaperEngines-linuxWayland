# 更新日志 / Changelog

本项目的重要变更都记录在这里。格式遵循
[Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循
[语义化版本](https://semver.org/lang/zh-CN/)。

## [0.1.0] - 2026-10-22

首个公开发布版本。First public release.

### Added

**场景解析（`we-scene`）**

* `scene.pkg` 解析：PKGV 容器、条目表、按需读取，容错旧/新写法的头。
* `.tex` 贴图解码：DXT/BC 压缩格式、mip 链、GIF/精灵图帧序列。
* `scene.json` 场景图解析：图层树、特效链、材质与着色器参数、粒子系统、
  骨骼动画（`mdl`）、时间轴与属性脚本。

**渲染器（`we-scene-render`）**

* 自研 GL/EGL 渲染器（GLES 3，走 `glow` + `khronos-egl`），无 CEF、无 WebView、
  无 Proton。
* 逐趟（pass）管线：组合 pass、特效 pass、渲染目标（RT）分配与依赖排序、
  中间缓冲复用。
* puppet 骨骼网格蒙皮（顶点权重 + 逐帧骨骼矩阵）。
* 遮罩烘焙（mask bake）：把遮罩/alpha 预烘到贴图，运行时只做采样。
* 链接层（linked layers）：同一场景内多个图层共享同一对象与变换。
* 文字对象：内嵌字体、系统字体回退、描边与阴影。
* 单帧离线渲染器（`we-scene-render` CLI）：同参数同像素，用于回归对比与差异定位。

**内核（`we-native`）**

* layer-shell 内核：每个输出一张 `wlr-layer-shell` `background` surface，真正的
  壁纸层，不是「藏起来的窗口」。
* 控制面：`$XDG_RUNTIME_DIR` 下的 Unix socket，一行一个 JSON 的请求/响应。
* 视频壁纸零拷贝路径（dmabuf → EGLImage，不经 CPU 拷贝）。
* 遮挡休眠：被全屏/最大化窗口遮挡时停止渲染并 `SIGSTOP` 解码进程，露出时恢复。
* frame callback 驱动的帧率控制，支持逐场景 fps 表。
* 看门狗：主循环卡死或合成器无响应时以非零码退出，交给 systemd 重启。

**界面（`wallpaper-hub`，libcosmic）**

* 壁纸库网格视图，列数随窗口宽度自适应。
* 工坊浏览视图（搜索、筛选、详情页）。
* 属性面板：作者暴露的用户属性（颜色、滑杆、开关、下拉、文字）实时下发到内核。
* 圆形色盘（HSV），拖动即时预览。
* 逐场景 fps 与播放速度（0.25× ~ 3.0×，步进 0.05）。
* 场景音频音量滑杆。

**打包与工程**

* `packaging/build-deb.sh`：一条命令产出可 `apt install ./x.deb` 的 amd64 包；
  `Depends` 由 `dpkg-shlibdeps` 从两个 ELF 实测计算，不靠手写猜测。
* systemd 用户单元（内核 + UI）、desktop entry、hicolor 图标。
* CI（`cargo check`）与 issue 模板。

### Changed

* **壁纸库根目录成为正式概念**：默认 `~/Templates`，优先级为
  `WPHUB_LIBRARY_DIRS` > UI 设置 > 状态文件 > 默认值；Steam 工坊目录降级为
  「存在才扫」的可选来源，引擎不再需要 Steam 客户端。
* 缩略图三档尺寸 + 响应式网格，面板变宽不再挤爆格子。
* UI 去掉登录/订阅入口 —— 引擎从不与 Steam 通信。
* 播放速度与场景音频滑杆归入「引擎内置项」，与作者属性分开。
* `.deb` 去掉 `Suggests: steam`，长描述改写为「壁纸来自本地目录」。
* `build-deb.sh` 在构建前 `unset LD_LIBRARY_PATH`/`LD_PRELOAD`，避免私有 GL/ffmpeg
  sysroot 泄漏进链接步骤导致 `Depends` 算错。
* `wallpaper-hub.service` 改为 `Restart=on-failure` + `SuccessExitStatus=0`：关窗口
  是正常退出，不拉起；崩溃才重启。

### UI

一轮以「密度与可读性」为目标的界面整理（全部有布局单测钉住，不靠截图猜）：

* **筛选面板默认展开**，并记住手动折叠状态（`Group::expanded` 持久化到 `Config::workshop_fold`）。
  面板高度上限 28% 窗口高，超出部分在面板内部滚动，不再把详情区挤没。
* **发现页改为「热度推荐」**：不再复用筛选面板，直接走 `browsesort=trend` 的热门排序
  （`BrowseSurface::{Hot, Workshop}`），只带搜索词、不带筛选组；工坊页仍走完整筛选。
* **工坊页去掉「来源」分组**：组数 5 → 4 —— 引擎已不与 Steam 通信，来源不再是有效筛选维度。
* **三页各自的底部翻页行**（`view_page_bar`）：首页 / 上一页 / 7 个页码 / 下一页 / 尾页，
  页码跟着当前页居中滚动，翻页不再需要回到顶部。
* **预览图与控件间距 16 px**：右侧面板的缩略预览和下面的属性控件分开，视觉上不再黏成一块。
* **网格滚动条留 12 px 沟槽**（`grid_scrollable` + `GRID_SCROLL_GUTTER`/`GRID_SCROLL_RESERVE`）：
  卡片右边缘不再压着滚动条；列数计算同时扣掉「滚动条 8 + 沟槽 12」，
  最右一张卡片不会被裁（滚动区是纵向的，横向没有滚动条可救）。
* **默认窗口 1488×900**（`WIN_DEFAULT`，开窗设置与 `AppModel` 初值共用一处）：
  默认档（280×158）一排 **3 列**、右侧固定 16 px 余量。旧默认 1400 只能排 2 列、
  右边空 ≈232 px（即用户说的「两列 + 一小列空白」）。

### Fixed

* 属性绑定：`{"user": …}` 形式的属性引用现在正确解析（此前整条属性静默失效）。
* 数值型条件比较：`>`/`<`/`==` 在 number 与 string 混用时按数值比较。
* 遮罩空间：遮罩在正确坐标系里采样，不再随父层缩放漂移。
* 负 scale 镜像：与 Windows 原引擎行为对齐 —— **不镜像**。
* 单帧离线渲染器由「两遍上屏」改为「一遍」，输出与实时路径逐像素一致。
* 关闭窗口不再复活（见上：unit 的 `Restart`/`SuccessExitStatus` 组合）。
* **调壁纸音量不再改系统音量**：改为 per-stream —— 音量只作用于我们自己 spawn 的
  播放流（`pactl set-sink-input-volume`，用 `-P we.stream.id=<tag>` 唯一定位），
  不再碰 `@DEFAULT_SINK@`；UI 也不再从 sink 读滑杆初值。
* 颜色/滑杆拖动 20 Hz 节流，拖动时不再把内核消息队列打满。

### Notes

* 预编译 `.deb` 在 **Debian forky** 上构建，`Depends` 含 **`libc6 (>= 2.43)`**。
  Debian 13（trixie）、Ubuntu 24.04/22.04 及更旧的系统装不上；请在这些系统上跑
  `./packaging/build-deb.sh` 自建，脚本会在本机重算 `Depends`。
* 仅在有限的场景样本上验证过（开发期约 17 个场景）。复杂特效与遮罩语义仍可能与
  原引擎有差异，欢迎带 workshop id 报 issue。
* 仅支持 Wayland（主要测 COSMIC）；X11 未实现。
* 本仓库不含任何 Steam 内容或壁纸素材，不需要账号 / SteamID / token，不做登录、
  订阅或下载。

### English summary

First public release: a native Wayland engine for Wallpaper Engine *scene* wallpapers.
Added `scene.pkg`/`.tex`/`scene.json` parsing, a from-scratch GL/EGL renderer (pass
pipeline, render targets, puppet skinning, baked masks, linked layers, text objects),
a layer-shell kernel (JSON control socket, zero-copy video, occlusion sleep, frame
callbacks), a libcosmic UI (library grid, workshop browser, properties panel, circular
colour wheel, per-scene fps), a single-frame offline renderer, and a `build-deb.sh`
that computes `Depends` from the binaries. Library roots are now a first-class concept
(default `~/Templates`; Steam is an optional scan source). Fixed user-property binding
(`{"user": …}`), numeric conditionals, mask space, negative-scale mirroring,
double-present in the offline renderer, window-close respawn, and — most visibly —
the wallpaper volume slider no longer moves the system volume (per-stream gain).
Note: the prebuilt `.deb` requires `libc6 >= 2.43` (built on Debian forky).

[0.1.0]: https://github.com/Sunrise-Valley/WallpaperEngines-linuxWayland/releases/tag/v0.1.0
