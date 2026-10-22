## wallpaper-hub 0.1.0

Linux 原生的 Wallpaper Engine **场景**壁纸引擎：真 `zwlr_layer_shell_v1` 壁纸层，
无 Wine、无 Electron、无 CEF、运行时完全不调用 Valve 的代码。

### 安装（Debian/Ubuntu，amd64）
```sh
sudo -S -p '' apt install ./wallpaper-hub_0.1.0_amd64.deb
systemctl --user daemon-reload
systemctl --user enable --now wallpaper-hub.service
```

⚠️ **本包在 Debian forky 上构建，依赖 `libc6 >= 2.43`**。Debian 13 (trixie)、
Ubuntu 24.04 及更老的系统**装不上**，请在本机跑 `packaging/build-deb.sh` 自行构建。

### 本版包含

**新增**
- `scene.pkg` / `.tex` / `scene.json` 解析
- GL/EGL 渲染器：pass 管线与 RT、puppet 骨骼蒙皮、遮罩烘焙、链接层、文字对象、粒子
- layer-shell 内核：控制 socket、零拷贝视频、遮挡休眠、frame 回调配速
- libcosmic 桌面应用：壁纸网格、工坊浏览与搜索、属性面板、圆形色盘、逐场景帧率
- 单帧离线渲染器（可复现同一 `(t, mouse)` 的画面）

**变更**
- 壁纸库根目录成为正式概念：默认 `~/Templates`，Steam 工坊目录仅作**可选**扫描源（不需要安装 Steam）
- 缩略图三档 + 响应式网格；卡片间距 24px
- 移除全部 Steam 登录/订阅 UI
- 新增「播放速度」（0.25–3.0×）与「场景音频」滑杆
- systemd 用户单元：`Restart=on-failure` + `SuccessExitStatus=0`

**修复**
- `{"user": …}` 属性绑定失效（改文字/颜色不生效）
- 属性值 `"2.0"` 与条件 `"2"` 比较失败（主题条件永不成立）
- 遮罩与特效采样空间错位（滑板尖端自激、虹膜/纱衣效果丢失）
- 负 scale 被当成镜像（与 Windows 侧行为不符）
- 每帧两次 `wl_surface.commit`（合成器每帧被要求上屏两遍）
- 关闭窗口后应用被立刻重新拉起
- **调整壁纸音量会改变系统音量**（改为 per-stream，sink 不再被动）
- 拖动颜色滑杆把内核打满（20 Hz 节流 + 合并队列 + 松手必发终值）

### 已知限制
- 仅在有限场景样本上验证过，无法保证所有壁纸都能完全还原；异常的请带工坊 ID 开 issue
- 渲染成本随场景复杂度上升，重场景依赖自适应帧率
- 仅支持 Wayland（COSMIC 是测过的合成器）；未实现 X11

### 说明
本仓库**不包含任何 Steam 内容或壁纸资源**，也不包含 Wallpaper Engine 的代码。
