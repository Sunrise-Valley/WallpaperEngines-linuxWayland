//! 壁纸内核：把壁纸画进 Wayland 的 layer-shell 背景层。
//!
//! 一块渲染表面挂在 `zwlr_layer_shell_v1` 的 **Background 层**上，每台显示器一个；
//! 内核自身不解析 `scene.pkg` —— 它把壁纸目录交给 [`we_scene_render`]（同进程库），
//! 由渲染器画出帧，内核负责上屏、输入、音频、睡眠与失败恢复。
//!
//! 用法：
//!   we-native --serve <socket>           常驻并接受控制面命令（UI 用这条）
//!   we-native --scene <目录>             直接播放一条 scene 壁纸
//!   we-native --image <文件> [--seconds N]
//!   we-native --video <文件> [--video-max WxH]
//!   we-native --globals                  列出合成器支持的全部协议
//!   we-native --list-outputs             只列出显示器
//!
//! 常驻期间每 5 秒（`--stats`）自报 CPU 累计时间与常驻内存。

use std::{num::NonZeroU32, path::PathBuf, process, time::Duration};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    delegate_simple,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState, SimpleGlobal},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_callback, wl_output, wl_shm, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewport::WpViewport, wp_viewporter};
// 全局指针：`ext_image_copy_capture` 的**光标会话** —— 不抓帧就给全局绝对坐标，
// 指针压在任何窗口上都有（`wl_pointer` 只在指针压在我们表面上时有效 ⇒ 两者互补）。
// `wayland-protocols` 的 `staging` 特性已自带这两个协议，无需自己生成代码。
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1 as cap_source,
    ext_output_image_capture_source_manager_v1 as cap_src_mgr,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1 as cap_cursor,
    ext_image_copy_capture_manager_v1 as cap_mgr,
    ext_image_copy_capture_session_v1 as cap_sess,
};
// 生成代码里那些 pub use 是给宏展开当作用域路径用的（"不被用到"与"再导出可见性含糊"都是它的
// 固有噪声）。把这两类 lint 限定在这个模块内允许；main.rs 自己的代码照样受全部 lint 约束。
#[allow(unused_imports, ambiguous_import_visibilities)]
mod audio;
/// **app 存档恢复**：内核重启后把运行时下发的属性覆盖表与逐场景帧率找回来
/// （读 `~/.local/share/wallpaper-hub/{prop_overrides,scene_fps}.json`，只读不写）。
/// 开关 `WE_RESTORE_APP_STATE=0` ⇒ 关（默认开，关时逐字节等于旧行为）。
mod app_state;
/// PipeWire/PulseAudio sink 枚举与选择（音频频谱"跟哪一路输出"）
mod audio_sinks;
mod cosmic_toplevel;
mod watchdog;
mod dump;
mod slowloop;
mod display_guard;
/// **合成后取帧**：`ext_image_copy_capture` 的 **output 会话** —— 抓的是**合成器合成之后**
/// 的帧，与"壁纸自己再渲一遍"完全不同：屏幕上到底是什么，只有这一路能给出硬证据。
mod ovdump;
/// a submit that never comes back (pure logic + exit code).
mod commit_guard;
/// 生成代码位于 cosmic_toplevel.rs 内层的同名模块，这里起个别名少一层
use cosmic_toplevel::cosmic_toplevel as ct;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use smithay_client_toolkit::globals::GlobalData;

// ── 视频解码：ffmpeg 子进程 → 原始 BGRA 帧（b3）──────────────────────
// 资源政策：
// · 能用的现成工具直接用（ffmpeg），不自己写解码器
// · 分辨率按屏封顶：只缩小、绝不放大（force_original_aspect_ratio=decrease）
// · 帧率跟源：-re 按源时间戳推，不自由狂奔
// · 被遮挡/全屏时 **SIGSTOP 子进程**（真·停止解码，而不是只停提交）
// · 帧直接读进要 attach 的共享内存缓冲，不额外多拷一道
mod video {
    use std::io::Read;
    use std::process::{Child, ChildStdout, Command, Stdio};

    /// ffmpeg 可执行文件：`WE_FFMPEG` 覆盖，否则走 `PATH` 里的 `ffmpeg`。
    pub fn ffmpeg_bin() -> String {
        std::env::var("WE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
    }

    /// 额外库搜索路径（从自提 prefix 里跑 ffmpeg 时才需要）：只在 `WE_FFMPEG_LIB` 设了时注入。
    pub fn with_ffmpeg_env(cmd: &mut Command) {
        if let Some(p) = std::env::var_os("WE_FFMPEG_LIB") {
            cmd.env("LD_LIBRARY_PATH", p);
        }
    }

    pub struct Decoder {
        pub child: Child,
        pub out: ChildStdout,
        pub width: u32,
        pub height: u32,
        pub bytes_per_frame: usize,
        pub paused: bool,
        frames: u64,
    }

    /// ffprobe 可执行文件：`WE_FFPROBE` 覆盖，否则走 `PATH`。
    pub fn ffprobe_bin() -> String {
        std::env::var("WE_FFPROBE").unwrap_or_else(|_| "ffprobe".into())
    }

    /// VAAPI 渲染节点：`WE_VAAPI_DEVICE` 覆盖。
    pub fn vaapi_device() -> String {
        std::env::var("WE_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into())
    }

    /// 一次性探源：真实分辨率 + 帧率（用来精确算解码尺寸，避免在滤镜里写表达式）
    pub fn probe_source(path: &str) -> Option<(u32, u32, f64)> {
        let mut cmd = Command::new(ffprobe_bin());
        with_ffmpeg_env(&mut cmd);
        let out = cmd
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height,avg_frame_rate",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let mut it = s.split(',');
        let w: u32 = it.next()?.trim().parse().ok()?;
        let h: u32 = it.next()?.trim().parse().ok()?;
        let fps = it.next().unwrap_or("30/1").trim().to_string();
        let mut fr = fps.split('/');
        let num: f64 = fr.next()?.parse().unwrap_or(30.0);
        let den: f64 = fr.next().and_then(|x| x.parse().ok()).unwrap_or(1.0);
        let fps = if den > 0.0 { num / den } else { 30.0 };
        Some((w, h, fps))
    }

    /// 把源尺寸内接到 (max_w,max_h) 里：只缩小、绝不放大；取偶数（4:2:0 友好）
    fn fit(sw: u32, sh: u32, max_w: u32, max_h: u32) -> (u32, u32) {
        let s = (max_w as f64 / sw as f64)
            .min(max_h as f64 / sh as f64)
            .min(1.0);
        let ev = |v: f64| ((v.round() as u32).max(2)) & !1;
        (ev(sw as f64 * s), ev(sh as f64 * s))
    }

    impl Decoder {
        /// 起解码器：**默认走 vaapi 硬解 + GPU 缩放**（实测 0.13~0.27 核，软件路要 0.67~0.93 核），
        /// 硬解起不来或第一帧出不来会自动回落到软件解码。
        /// (max_w,max_h) 是**上限**（一般传屏幕物理尺寸）：源比它小就原样解，不放大。
        pub fn spawn(path: &str, max_w: u32, max_h: u32) -> std::io::Result<Self> {
            let (sw, sh, fps) = probe_source(path).unwrap_or((0, 0, 30.0));
            let (fw, fh) = if sw > 0 && sh > 0 {
                fit(sw, sh, max_w, max_h)
            } else {
                (max_w & !1, max_h & !1)
            };
            println!(
                "[视频] 源 {sw}x{sh}@{fps:.1}fps → 解码尺寸 {fw}x{fh}（屏幕上限 {max_w}x{max_h}，只缩不放）"
            );
            let want_hw = std::path::Path::new(&vaapi_device()).exists()
                && std::env::var("WE_VIDEO_SOFTWARE").is_err();
            if want_hw {
                match Self::spawn_with(path, fw, fh, true) {
                    Ok(mut d) => {
                        // 必须先确认硬解真的能出帧，否则静默回落到软件
                        let mut probe = vec![0u8; d.bytes_per_frame];
                        if d.next_frame(&mut probe) {
                            d.frames = 0;
                            return Ok(d);
                        }
                        eprintln!("[视频] 硬解没出帧 → 回落软件解码");
                        let _ = d.child.kill();
                        let _ = d.child.wait();
                    }
                    Err(e) => eprintln!("[视频] 起硬解失败（{e}）→ 回落软件解码"),
                }
            }
            Self::spawn_with(path, fw, fh, false)
        }

        fn spawn_with(path: &str, w: u32, h: u32, hw: bool) -> std::io::Result<Self> {
            let mut c = Command::new(ffmpeg_bin());
            with_ffmpeg_env(&mut c);
            c.args(["-hide_banner", "-loglevel", "error"])
                // 内存底噪几乎全在解码线程池上（实测 1 线程 89MB / 2 线程 99MB / 16 线程 189MB）
                .args(["-threads", "2"]);
            if hw {
                c.args([
                    "-hwaccel",
                    "vaapi",
                    "-vaapi_device",
                    &vaapi_device(),
                    "-hwaccel_output_format",
                    "vaapi",
                ]);
            }
            // 壁纸必须循环：-stream_loop -1 让 ffmpeg 无限重放输入（seekable 文件才支持）
            // re：按源帧率读入（壁纸不需要跑得比源更快）
            c.args(["-stream_loop", "-1", "-re"]).arg("-i").arg(path);
            let vf = if hw {
                // 全程在 GPU 上缩放，最后才回读成 BGRA（核显回读=内存拷贝，很便宜）
                format!("scale_vaapi=w={w}:h={h},hwdownload,format=bgra")
            } else {
                format!("scale={w}:{h}:flags=bilinear")
            };
            c.args(["-vf", &vf])
                .args(["-f", "rawvideo", "-pix_fmt", "bgra", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = c.spawn()?;
            let out = child.stdout.take().expect("stdout 被 piped 了");
            println!(
                "[视频] 解码器已起：{} {}x{}（LD_LIBRARY_PATH 已注入私有 prefix）",
                if hw { "vaapi 硬解" } else { "软件解码" },
                w,
                h
            );
            Ok(Decoder {
                child,
                out,
                width: w,
                height: h,
                bytes_per_frame: (w * h * 4) as usize,
                paused: false,
                frames: 0,
            })
        }

        /// 读满一帧就算成功；返回 false 表示流结束
        pub fn next_frame(&mut self, buf: &mut [u8]) -> bool {
            debug_assert_eq!(buf.len(), self.bytes_per_frame);
            let mut filled = 0usize;
            while filled < buf.len() {
                match self.out.read(&mut buf[filled..]) {
                    Ok(0) => return false,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return false,
                }
            }
            self.frames += 1;
            true
        }

        /// 被遮挡/全屏时：SIGSTOP 真的让 ffmpeg 停止解码（CPU 归零）
        pub fn pause(&mut self) {
            if !self.paused {
                unsafe { libc::kill(self.child.id() as i32, libc::SIGSTOP) };
                self.paused = true;
            }
        }

        /// 恢复
        pub fn resume(&mut self) {
            if self.paused {
                unsafe { libc::kill(self.child.id() as i32, libc::SIGCONT) };
                self.paused = false;
            }
        }

        pub fn frames(&self) -> u64 {
            self.frames
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            // 别留孤儿进程
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// 探针：不用 Wayland，只验证"ffmpeg 解码 → 拿到规定尺寸的 BGRA 帧"这条路
    pub fn probe(path: &str, w: u32, h: u32, want_frames: u64) {
        let t0 = std::time::Instant::now();
        let mut dec = match Decoder::spawn(path, w, h) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  起 ffmpeg 失败：{e}");
                return;
            }
        };
        let mut buf = vec![0u8; dec.bytes_per_frame];
        let mut first: Option<std::time::Duration> = None;
        let mut ok = 0u64;
        while ok < want_frames {
            let t = std::time::Instant::now();
            if !dec.next_frame(&mut buf) {
                break;
            }
            first.get_or_insert(t.elapsed());
            ok += 1;
        }
        let total = t0.elapsed();
        println!(
            "  目标尺寸 {}x{}（{} 字节/帧）· 取到 {} 帧 · 首帧 {:?} · 平均 {:?}/帧 · 总 {:?}",
            dec.width,
            dec.height,
            dec.bytes_per_frame,
            ok,
            first.unwrap_or_default(),
            total / (ok.max(1) as u32),
            total
        );
        // 看一眼前几个像素，确认不是全黑/全零
        println!(
            "  首帧前 4 个像素（BGRA）: {:?}",
            &buf[..16.min(buf.len())]
        );
    }
}

// ── 控制面（b4）：Unix socket + JSON 行协议 ────────────────────────
// 自研内核不再"一个壁纸一个进程"，而是常驻；GUI/CLI 通过这个控制面换壁纸、暂停、查状态。
// 协议：一行一个 JSON 请求，服务端回一行 JSON。
// 请求 {"cmd":"apply","kind":"image|video","path":"<绝对路径>"}
// {"cmd":"pause"} / {"cmd":"resume"} / {"cmd":"status"} / {"cmd":"quit"}
// 响应 {"ok":true,...} 或 {"ok":false,"error":"..."}
// 唤醒机制：accept 线程把命令塞进 channel 后往 socketpair 写 1 字节；主循环把
// 它的读端和 Wayland fd、视频帧 fd 一起 poll(-1)，于是**空闲时仍然零唤醒**，
// 而命令/帧一到就立刻响应，不需要任何定时器。
// 早前用的是 eventfd，实测 poll 会**永远报告可读**（计数没被清干净 ⇒ 事件循环
// 每秒空转一万六千次、白烧 0.3 个核）。换成 socketpair 这种 self-pipe 写法：
// 写一字节 = 唤醒，读一次 = 排空，语义不含糊。（诊断计数保留在 status 里备查。）
mod serve {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::RawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc::{channel, Receiver, Sender};

    #[derive(Debug)]
    pub enum Cmd {
        Status,
        /// 帧转储（`we-ctl dump <前缀> <张数>`）：把接下来 N 帧的**上屏画面**额外渲一份到 PNG，
        /// 并附一份逐帧旁注（场景时刻 / 当时的指针值 / 来源 / 全局事件数）。
        /// 为什么需要：壁纸被窗口盖住时抓屏看不到它，而转储拿的是**壁纸自己的帧** ——
        /// 这是"上屏那一帧里透镜到底在哪"的唯一诚实证据。
        Dump {
            prefix: String,
            count: u32,
            /// 抓谁的帧：`scene`（默认，内核自己再渲一遍 = 渲染器的帧）/ `output`
            /// （**合成器合成之后**的输出帧，需要 `ext_image_copy_capture` 的 output 会话）。
            source: String,
        },
        Apply { kind: String, path: String },
        Pause,
        Resume,
        Quit,
        /// 运行时改设置（低配/高配的可选档位就走这条）
        Set {
            fps: Option<u64>,
            sleep: Option<String>,
            scale: Option<String>,
            effects: Option<String>,
            /// 配乐开关：`on` / `off`（别的值当没写）
            sound: Option<String>,
            /// 取景模式：`cover` / `stretch` / `center` / `contain`（中英文都认）
            fit: Option<String>,
            /// 色彩调整：`亮度,对比度,饱和度,色相,色温,伽马`（六个浮点，单位值 `1,1,1,0,0,1`）
            color: Option<String>,
            /// **播放速度**（场景时间倍率）：0.25 ~ 3.0（渲染器再夹一次）
            /// `we_scene_render::set_time_scale`）。只乘场景时间 —— 音频/鼠标不受影响。
            time_scale: Option<f32>,
            /// **音乐音量**：0 ~ 100（百分比），调默认 sink 的 volume（见 `audio_sinks`）。
            volume: Option<f32>,
        },
        /// runtime 改用户属性覆盖（壁纸参数页改 bool/slider/color/combo/text 走这条）。
        /// **通用**：内核只当"搬运工"，`props` 里的（名字, 值）原样写进渲染器的用户属性覆盖表 ——
        /// 属性有哪些、叫什么、什么类型，全由壁纸自己的 `project.json general.properties` 决定，
        /// 内核**不认也不该认**任何具体属性名（否则每来一张新壁纸就得改内核）。
        SetProp {
            /// 覆盖条目（可批量）。值统一是字符串，由渲染器按 `userprop::parse_value` 解释。
            props: Vec<(String, String)>,
            /// `true` = **先清空整张表再写**（切壁纸时用，防上一条的属性名撞上新壁纸）。
            clear: bool,
        },
        /// 鼠标位置（屏幕归一化 `[0,1]`，**y 从屏幕顶往下**）⇒ 场景级 uniform `g_PointerPosition`。
        /// 为什么由控制面喂（而不是内核自己拿）：内核的层壳表面在 **Background 层**、
        /// 输入区为空 ⇒ 合成器不会把指针事件发给它（这是"壁纸不抢输入"的前提）。
        /// 于是鼠标位置有两个来源：① 内核自己的 `wl_pointer`（`WE_MOUSE_POINTER=1` 时给表面
        /// 设一个**输入区**，指针在**空桌面**上时能收到 enter/motion —— 见 `mouse_input`）；
        /// ② 控制面这条命令（任何有指针位置的进程都能推，也可用来**离线/脚本化验证**）。
        /// 两条路最终都落进同一个 `Wall::mouse`，同一口径。
        Mouse {
            x: f32,
            y: f32,
        },
        /// **音频频谱**（粒子音频响应的真频谱输入；`WE_AUDIO_SPECTRUM=1` 是等效的环境变量开关）。
///
        /// * `on: true/false` — 总开关（false ⇒ 渲染器立刻回到"没有音频"⇒ 所有音频响应恒等）
///
        /// * `source: "monitor"|"own"|"off"` — 频谱来源；`monitor` = 抓默认输出的 monitor
        /// （WE/linux-WE 的语义），`own` = 只吃内核自己播的 PCM（隐私模式）
///
        /// * `bands: [..] 或 "a b c .."` — **静态频谱**（1 个值 ⇒ 16 段全等），用于离线/脚本化验证；
        /// 设置即生效且不再采集。
///
        /// * `sink: "<id|名字|default|all>"` — **跟哪一路输出做分析**（采集源切到它的 monitor）。
        /// `default` = 跟随系统默认（插拔后跟着变，跟原来的行为一致）；`all` = **如实报不支持**
        /// （渲染器的采集线程一次只钉一个 monitor，见 `audio_sinks` 头注释）。可与 `on` 同用。
///
        /// * `sinks: true` — **只列可用 sink**（不改任何状态），回 `{"ok":true,"sinks":[…]}`；
        /// 枚举不到就回 `{"ok":false,"error":"…"}`（如实报"无法枚举"）。
        Audio {
            on: Option<bool>,
            source: Option<String>,
            bands: Option<[f32; 16]>,
            sink: Option<String>,
            sinks: bool,
        },
        Unknown(String),
    }

    pub struct Server {
        rx: Receiver<(Cmd, UnixStream)>,
        wake_rx: UnixStream,
        _wake_tx: UnixStream,
        _listener: UnixListener,
    }

    /// 每条连接最多占住一个工人这么久（读了这么久还没有一整行 ⇒ 丢掉这条连接）。
    pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// 应答写超时：客户端连上却不收，写不进去也不能把工人钉死。
    pub const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// 读连接的工人数。
    /// **为什么是"多个工人各自 accept"而不是"accept 线程 + 每条连接一个线程"**：
    /// · 界面会把控制面当轮询用（实测 `cmd_wakes` 量级 每秒数千次）⇒ 一条连接一个线程
    /// 等于每秒几百上千次 `clone3`，代价与风险都不划算；
    /// · 而"单个 accept 线程顺序读"的老写法有个致命后果：**一个客户端连上却不发完整行
    /// （半开连接 / 被 timeout 强杀 / 只连不写）就把这个唯一的线程钉在 `read_line` 上**，
    /// 读超时没到之前，后面所有连接（含界面自己的 status）全部排队没人受理
    /// 实测「控制面无应答」就是这么来的（N 条坏连接 ≈ N×5s）。
    /// 多个工人同时 `accept` 在同一个监听 fd 上（内核按到达次序唤醒其中一个）⇒
    /// 一条坏连接最多占住 1 个工人 5 秒，其余工人照常服务；accept 路径本身**永不阻塞在客户端上**。
    pub const WORKERS: usize = 4;

    impl Server {
        pub fn start(path: &str) -> std::io::Result<Server> {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)?;
            let (tx, rx): (Sender<(Cmd, UnixStream)>, Receiver<(Cmd, UnixStream)>) = channel();
            let (wake_rx, wake_tx) = UnixStream::pair()?;
            let writer = wake_tx.try_clone()?;

            for i in 0..WORKERS {
                let l = listener.try_clone()?;
                let tx = tx.clone();
                let mut w = writer.try_clone()?;
                std::thread::Builder::new()
                    .name(format!("ctl-worker-{i}"))
                    .spawn(move || loop {
                        let Ok((conn, _addr)) = l.accept() else { continue };
                        // 读超时：连上却不发完整行 ⇒ 5 秒后丢弃这条连接，回去接下一个。
                        let _ = conn.set_read_timeout(Some(READ_TIMEOUT));
                        let _ = conn.set_write_timeout(Some(WRITE_TIMEOUT));
                        let mut line = String::new();
                        {
                            let mut r = BufReader::new(&conn);
                            if r.read_line(&mut line).is_err() {
                                continue; // 超时/断开/半开：丢掉这条，继续服务别人
                            }
                        }
                        let cmd = parse(&line);
                        if tx.send((cmd, conn)).is_err() {
                            return; // 主循环没了
                        }
                        // 写一字节唤醒主循环（写失败说明主循环没了，直接退出）
                        if w.write_all(&[1u8]).is_err() {
                            return;
                        }
                    })
                    .expect("起不了控制面工人线程");
            }

            Ok(Server {
                rx,
                wake_rx,
                _wake_tx: wake_tx,
                _listener: listener,
            })
        }

        /// 取走所有已到的命令（顺便把唤醒字节读干净）
        pub fn drain(&mut self) -> Vec<(Cmd, UnixStream)> {
            let mut buf = [0u8; 64];
            // 读到没有为止（非阻塞：EAGAIN 就停）
            let _ = self.wake_rx.set_nonblocking(true);
            while let Ok(n) = self.wake_rx.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            let mut out = Vec::new();
            while let Ok(v) = self.rx.try_recv() {
                out.push(v);
            }
            out
        }

        pub fn fd(&self) -> RawFd {
            use std::os::fd::AsRawFd;
            self.wake_rx.as_raw_fd()
        }
    }

    /// 极简 JSON 取值（够用就好：字符串/裸值都吃，支持 \" 与 \\ 转义）
    fn field(s: &str, key: &str) -> Option<String> {
        let k = format!("\"{key}\"");
        let i = s.find(&k)?;
        let rest = &s[i + k.len()..];
        let rest = &rest[rest.find(':')? + 1..];
        let rest = rest.trim_start();
        let stripped = rest.strip_prefix('"')?;
        let mut out = String::new();
        let mut esc = false;
        for ch in stripped.chars() {
            if esc {
                out.push(ch);
                esc = false;
                continue;
            }
            match ch {
                '\\' => esc = true,
                '"' => return Some(out),
                _ => out.push(ch),
            }
        }
        // 没引号的裸值（true/false/数字）
        Some(out)
    }

    pub fn parse(line: &str) -> Cmd {
        match field(line, "cmd").as_deref() {
            Some("status") => Cmd::Status,
            Some("apply") => Cmd::Apply {
                kind: field(line, "kind").unwrap_or_else(|| "image".to_string()),
                path: field(line, "path").unwrap_or_default(),
            },
            Some("pause") => Cmd::Pause,
            Some("resume") => Cmd::Resume,
            Some("quit") => Cmd::Quit,
            Some("set") => Cmd::Set {
                fps: num_field(line, "fps"),
                sleep: field(line, "sleep"),
                scale: field(line, "scale"),
                effects: field(line, "effects"),
                sound: field(line, "sound"),
                fit: field(line, "fit"),
                color: field(line, "color"),
                time_scale: f32_field(line, "time_scale"),
                volume: f32_field(line, "volume"),
            },
            Some("set-prop") => Cmd::SetProp {
                props: parse_props(line),
                clear: bool_field(line, "clear"),
            },
            Some("dump") => Cmd::Dump {
                prefix: field(line, "prefix").unwrap_or_default(),
                count: num_field(line, "count").unwrap_or(0) as u32,
                source: field(line, "source").unwrap_or_else(|| "scene".into()),
            },
            Some("mouse") => Cmd::Mouse {
                x: f32_field(line, "x").unwrap_or(0.5),
                y: f32_field(line, "y").unwrap_or(0.5),
            },
            Some("audio") => Cmd::Audio {
                // `"on":true` 才认；`"on":false` 与"没写"区分开（没写 = 不改开关）
                on: on_field(line),
                source: field(line, "source"),
                bands: parse_bands(line),
                // `"sink":"default"` / `"sink":"593"` / `"sink":"名字"` / `"sink":"all"`
                sink: field(line, "sink"),
                // `"sinks":true`（裸布尔，走 bool_field）
                sinks: bool_field(line, "sinks"),
            },
            other => Cmd::Unknown(other.unwrap_or("").to_string()),
        }
    }

    /// 三态布尔字段：`true` / `false` / 没写（`None`）。
    /// 与 `bool_field` 的区别：那个"没写 = false"，用在"clear"这种默认关闭的地方正合适；
    /// 音频开关要区分"没写"（别动）和"显式关"，所以单开一个。
    pub fn on_field(line: &str) -> Option<bool> {
        let pat = "\"on\":";
        let i = line.find(pat)? + pat.len();
        let rest = line[i..].trim_start();
        if rest.starts_with("true") {
            Some(true)
        } else if rest.starts_with("false") {
            Some(false)
        } else if rest.starts_with('1') {
            Some(true)
        } else if rest.starts_with('0') {
            Some(false)
        } else {
            None
        }
    }

    /// 解析 `"bands": [0.1, 0.2, ...]` 或 `"bands": "0.1 0.2 ..."` ⇒ 16 段。
    /// 宽容口径（与渲染器的 `WE_AUDIO_BANDS` 一致）：1 个值 ⇒ 16 段全等；少于 16 个 ⇒ 后面补最后一个；
    /// 多于 16 个 ⇒ 只取前 16；一个数都取不到 ⇒ `None`（= 不改频谱）。
    pub fn parse_bands(line: &str) -> Option<[f32; 16]> {
        let pat = "\"bands\":";
        let i = line.find(pat)? + pat.len();
        let rest = line[i..].trim_start();
        // 数组：[..] 字符串：".."
        let body: &str = if let Some(stripped) = rest.strip_prefix('[') {
            match stripped.find(']') {
                Some(e) => &stripped[..e],
                None => stripped,
            }
        } else if let Some(stripped) = rest.strip_prefix('"') {
            match stripped.find('"') {
                Some(e) => &stripped[..e],
                None => stripped,
            }
        } else {
            // 裸着写（`"bands":[..]` 之外的脏数据）：取到第一个 `}` 或 `,` 为止
            let end = rest.find(['}', ',']).unwrap_or(rest.len());
            &rest[..end]
        };
        let vals: Vec<f32> = body
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter_map(|t| t.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .collect();
        if vals.is_empty() {
            return None;
        }
        let mut out = [0.0f32; 16];
        if vals.len() == 1 {
            out = [vals[0].clamp(0.0, 1.0); 16];
        } else {
            let mut last = 0.0f32;
            for (i, slot) in out.iter_mut().enumerate() {
                if i < vals.len() {
                    last = vals[i].clamp(0.0, 1.0);
                }
                *slot = last;
            }
        }
        Some(out)
    }

    /// JSON 字符串转义
    pub fn jesc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// 取数字字段：**引号可选**（`"fps":15` 和 `"fps":"15"` 都收 ✓）。
    /// 踩过的坑：界面按 JSON 规范发数字，而原来的 field 只认带引号的字符串，
    /// 于是 `set` 一路"成功"但 fps 根本没变 ✗ —— 协议测试必须用真实 payload 打一遍。
    pub fn num_field(line: &str, key: &str) -> Option<u64> {
        let pat = format!("\"{key}\":");
        let i = line.find(&pat)? + pat.len();
        let rest = line[i..].trim_start().trim_start_matches('"');
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    }

    /// 取布尔字段（裸值形式，如 `"clear":true`）。字段不存在 / 不是 true ⇒ false。
    pub fn bool_field(line: &str, key: &str) -> bool {
        let pat = format!("\"{key}\":");
        match line.find(&pat) {
            Some(i) => line[i + pat.len()..].trim_start().starts_with("true"),
            None => false,
        }
    }

    /// 取浮点字段：**引号可选**（`"x":0.3` / `"x":"0.3"` 都收）。取不到 / 不成数字 ⇒ None。
    /// 为什么单开一个而不是改 `num_field`：鼠标位置是 `[0,1]` 的**小数**，而 `num_field`
    /// 只吃整数（`0.3` 会被它截成 0 ⇒ 鼠标永远在左上角）。两个函数各自的用例都测。
    pub fn f32_field(line: &str, key: &str) -> Option<f32> {
        let pat = format!("\"{key}\":");
        let i = line.find(&pat)? + pat.len();
        let rest = line[i..].trim_start().trim_start_matches('"');
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    }

    /// 从一行请求里取 `"props": { … }` 的内容（外层花括号之间的原文）。
    /// 为什么不用 `field`：那是"找第一个 `"key":` 然后取一个标量"，对付不了**嵌套对象**，
    /// 而属性覆盖天然是一张**任意键**的表（键名由壁纸作者起，内核事先不可能知道）。
    /// 这里做一个最小但正确的花括号配对扫描：**字符串里的花括号不算数**（转义也认）。
    /// 找不到 / 不是对象 ⇒ 空（调用方当作"没给属性"）。
    fn props_body(line: &str) -> Option<String> {
        let pat = "\"props\"";
        let ki = line.find(pat)?;
        let rest = &line[ki + pat.len()..];
        let rest = rest[rest.find(':')? + 1..].trim_start();
        if !rest.starts_with('{') {
            return None;
        }
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        let mut end = None;
        for (bi, ch) in rest.char_indices() {
            if in_str {
                if esc {
                    esc = false;
                } else if ch == '\\' {
                    esc = true;
                } else if ch == '"' {
                    in_str = false;
                }
                continue;
            }
            match ch {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(bi);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        Some(rest[1..end].to_string())
    }

    /// 从 `"props": {...}` 里取出 (名字, 值) 对。名字与值都按 JSON 解转义；
    /// 值可以是字符串（`"true"`）或裸标量（`true` / `1` / `0.5`）。
    pub fn parse_props(line: &str) -> Vec<(String, String)> {
        match props_body(line) {
            Some(body) => split_pairs(&body),
            None => Vec::new(),
        }
    }

    /// 读一个 JSON 字符串（`s[i]` 必须是 `"`）：返回 (解转义后的内容, 结束后的下标)。
    fn read_json_string(s: &str, mut i: usize) -> Option<(String, usize)> {
        let b = s.as_bytes();
        if b.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        let mut out = String::new();
        while i < b.len() {
            match b[i] {
                b'\\' => {
                    i += 1;
                    let e = *b.get(i)?;
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        // `\uXXXX`：属性名/值实测都是 ASCII（`true`/`0.5`/英文名），
                        // 这里按单个码位解；代理对（emoji 等）不做合并 —— 够用且有测试盯着。
                        b'u' => {
                            let hx = s.get(i + 1..i + 5)?;
                            let cp = u32::from_str_radix(hx, 16).ok()?;
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            i += 4;
                        }
                        other => out.push(other as char),
                    }
                    i += 1;
                }
                b'"' => return Some((out, i + 1)),
                _ => {
                    let ch = s[i..].chars().next()?;
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        None
    }

    /// 把 `"a":"1","b":true,"c":0.5` 这类**对象内层**切成键值对（跳过空白与逗号）。
    fn split_pairs(body: &str) -> Vec<(String, String)> {
        let b = body.as_bytes();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < b.len() {
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
                i += 1;
            }
            if i >= b.len() {
                break;
            }
            // 键必须是 JSON 字符串
            let Some((key, ni)) = read_json_string(body, i) else {
                break;
            };
            i = ni;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b.get(i) != Some(&b':') {
                break;
            }
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= b.len() {
                break;
            }
            let value = if b[i] == b'"' {
                let Some((v, ni)) = read_json_string(body, i) else {
                    break;
                };
                i = ni;
                v
            } else {
                // 裸标量：读到 `,` 或 `}` 为止
                let start = i;
                while i < b.len() && b[i] != b',' && b[i] != b'}' {
                    i += 1;
                }
                body[start..i].trim().to_string()
            };
            out.push((key, value));
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 全局指针归一化：除以捕获会话报的 `buffer_size`（**输出缓冲像素**）。
        /// 这条锁住"两条路同口径"的算术 —— `mouse_from_surface` 用表面逻辑尺寸，
        /// 而 `buffer = logical × scale` ⇒ `x/buffer == (x/scale)/logical`，两者必须等价可对账。
        #[test]
        fn global_pointer_normalizes_by_buffer_size() {
            // 实测那一行：全局 528,799 ⇒ 0.2750,0.7398（1920x1080 输出）—— 逐位对齐内核日志
            let (x, y) = crate::Wall::normalize_global(528.0, 799.0, (1920.0, 1080.0));
            assert!((x - 0.2750).abs() < 1e-4, "x={x}");
            assert!((y - 0.7398).abs() < 1e-4, "y={y}");
            // `position` 允许为负 / 超出缓冲（协议明说）⇒ 必须夹到 [0,1]
            assert_eq!(
                crate::Wall::normalize_global(-40.0, 5000.0, (1920.0, 1080.0)),
                (0.0, 1.0)
            );
            // `buffer_size` 还没来 ⇒ 屏幕中心（WE/OWE 的"指针总有位置"语义）
            assert_eq!(crate::Wall::normalize_global(10.0, 10.0, (0.0, 0.0)), (0.5, 0.5));
        }

        /// 半开连接隔离的前提：**必须多于一个工人**，否则一条坏连接仍然把所有人排队。
        /// （老实现是"单个 accept 线程顺序读" ⇒ 这里注册的是这个不变量。）
        #[test]
        fn has_more_than_one_ctl_worker() {
            assert!(
                WORKERS >= 2,
                "控制面工人必须 ≥2：一个客户端只连不写不能把整个控制面堵死"
            );
        }

        /// 每条连接都必须有读超时，否则工人会被半开连接永久占住。
        #[test]
        fn per_connection_timeouts_are_set() {
            assert!(READ_TIMEOUT.as_secs() >= 1 && READ_TIMEOUT.as_secs() <= 30);
            assert!(WRITE_TIMEOUT.as_secs() >= 1 && WRITE_TIMEOUT.as_secs() <= 30);
        }

        /// 协议不能被这次改动碰坏：真实 payload 逐条走一遍。
        #[test]
        fn parses_real_control_payloads() {
            assert!(matches!(parse("{\"cmd\":\"status\"}\n"), Cmd::Status));
            assert!(matches!(parse("{\"cmd\":\"pause\"}"), Cmd::Pause));
            match parse("{\"cmd\":\"apply\",\"kind\":\"scene\",\"path\":\"/x\"}") {
                Cmd::Apply { kind, path } => {
                    assert_eq!(kind, "scene");
                    assert_eq!(path, "/x");
                }
                other => panic!("apply 解析错了：{other:?}"),
            }
            match parse("{\"cmd\":\"set\",\"fps\":30,\"sleep\":\"never\"}") {
                Cmd::Set { fps, sleep, .. } => {
                    assert_eq!(fps, Some(30));
                    assert_eq!(sleep.as_deref(), Some("never"));
                }
                other => panic!("set 解析错了：{other:?}"),
            }
            // 两个新旋钮（播放速度 / 音乐音量）——**数字**字段，不是字符串
            match parse("{\"cmd\":\"set\",\"time_scale\":1.25,\"volume\":47.5}") {
                Cmd::Set { time_scale, volume, .. } => {
                    assert_eq!(time_scale, Some(1.25));
                    assert_eq!(volume, Some(47.5));
                }
                other => panic!("set(time_scale/volume) 解析错了：{other:?}"),
            }
            // 不写这两个字段 ⇒ None（不是 0：0 是合法音量，混了就分不清"没给"和"静音"）
            match parse("{\"cmd\":\"set\",\"fps\":60}") {
                Cmd::Set { time_scale, volume, .. } => {
                    assert_eq!(time_scale, None);
                    assert_eq!(volume, None);
                }
                other => panic!("set 解析错了：{other:?}"),
            }
        }

        /// `set-prop`：**通用**解析任意属性名（内核不认识它们）。
        /// 用一张真实壁纸(A 2887537617)的属性名，以及一个"内核绝对没听说过"的名字。
        #[test]
        fn parses_set_prop_payload_generically() {
            match parse(
                "{\"cmd\":\"set-prop\",\"props\":{\"newproperty\":\"true\",\"newproperty1\":true,\"whatever_new\":\"0.5\"}}",
            ) {
                Cmd::SetProp { props, clear } => {
                    assert!(!clear);
                    assert_eq!(
                        props,
                        vec![
                            ("newproperty".to_string(), "true".to_string()),
                            ("newproperty1".to_string(), "true".to_string()),
                            ("whatever_new".to_string(), "0.5".to_string()),
                        ],
                        "名字/裸值/字符串值都要原样取出，且不针对任何属性名特判"
                    );
                }
                other => panic!("set-prop 解析错了：{other:?}"),
            }
            // clear 标志（清表再写：切壁纸用）
            match parse("{\"cmd\":\"set-prop\",\"clear\":true,\"props\":{\"a\":\"1\"}}") {
                Cmd::SetProp { props, clear } => {
                    assert!(clear);
                    assert_eq!(props, vec![("a".to_string(), "1".to_string())]);
                }
                other => panic!("set-prop(clear) 解析错了：{other:?}"),
            }
            // 空对象 = 合法请求（什么都不覆盖）；缺 props = 空表（不是崩溃）
            match parse("{\"cmd\":\"set-prop\",\"props\":{}}") {
                Cmd::SetProp { props, .. } => assert!(props.is_empty()),
                other => panic!("空 props 解析错了：{other:?}"),
            }
            match parse("{\"cmd\":\"set-prop\"}") {
                Cmd::SetProp { props, clear } => {
                    assert!(props.is_empty() && !clear);
                }
                other => panic!("缺 props 解析错了：{other:?}"),
            }
            // props 是数组/标量 ⇒ 当作没有（不 panic）
            assert!(parse_props("{\"cmd\":\"set-prop\",\"props\":[\"a=1\"]}").is_empty());
            assert!(parse_props("{\"cmd\":\"set-prop\",\"props\":null}").is_empty());
        }

        /// 鼠标命令：**小数**必须原样收下（只认整数会让鼠标永远贴在左上角）。
        #[test]
        fn mouse_cmd_parses_floats() {
            match parse(r#"{"cmd":"mouse","x":0.3,"y":0.75}"#) {
                Cmd::Mouse { x, y } => {
                    assert!((x - 0.3).abs() < 1e-6, "x 丢了小数：{x}");
                    assert!((y - 0.75).abs() < 1e-6, "y 丢了小数：{y}");
                }
                other => panic!("mouse 解析错了：{other:?}"),
            }
            // 引号形式（界面按 JSON 规范可能发字符串）、整数形式、负号/指数
            match parse(r#"{"cmd":"mouse","x":"0.25","y":1}"#) {
                Cmd::Mouse { x, y } => {
                    assert!((x - 0.25).abs() < 1e-6, "{x}");
                    assert!((y - 1.0).abs() < 1e-6, "{y}");
                }
                other => panic!("mouse(引号/整数) 解析错了：{other:?}"),
            }
            // 缺字段 ⇒ 取中心（不是 0：0,0 会把透视区甩到左上角）
            match parse(r#"{"cmd":"mouse"}"#) {
                Cmd::Mouse { x, y } => assert!((x - 0.5).abs() < 1e-6 && (y - 0.5).abs() < 1e-6),
                other => panic!("mouse(缺字段) 解析错了：{other:?}"),
            }
            // 这条是**协议验证**的本体：`num_field` 只吃整数 ⇒ 老写法会把 0.3 读成 0
            assert_eq!(num_field(r#"{"cmd":"mouse","x":0.3}"#, "x"), Some(0));
            assert_eq!(f32_field(r#"{"cmd":"mouse","x":0.3}"#, "x"), Some(0.3));
            assert_eq!(f32_field(r#"{"cmd":"mouse","x":0.3,"y":0.4}"#, "y"), Some(0.4));
            assert_eq!(f32_field(r#"{"cmd":"mouse","x":-0.5}"#, "x"), Some(-0.5));
            assert_eq!(f32_field(r#"{"cmd":"mouse"}"#, "x"), None);
            assert_eq!(f32_field(r#"{"cmd":"mouse","x":"abc"}"#, "x"), None);
        }

        /// 音频频谱命令（`{"cmd":"audio",…}`）：三态 `on`、两种 `bands` 写法、`source`。
        /// **协议要点**：`on` 不写 ≠ `on:false`（不写 = 别动开关）；
        /// `bands` 一个数都取不到 ⇒ `None`（别把"解析失败"当成"用户要全 0 静音"）。
        #[test]
        fn audio_cmd_parses_on_source_and_bands() {
            match parse(r#"{"cmd":"audio","on":true,"source":"monitor"}"#) {
                Cmd::Audio {
                    on,
                    source,
                    bands,
                    sink,
                    sinks,
                } => {
                    assert_eq!(on, Some(true));
                    assert_eq!(source.as_deref(), Some("monitor"));
                    assert!(bands.is_none(), "没给 bands 就别动频谱");
                    assert_eq!(sink, None, "没给 sink ⇒ 不改输出源");
                    assert!(!sinks, "没给 sinks ⇒ 不是列列表");
                }
                other => panic!("audio 解析错了：{other:?}"),
            }
            match parse(r#"{"cmd":"audio","source":"off"}"#) {
                Cmd::Audio { on, .. } => assert_eq!(on, None, "没写 on ⇒ 不改开关"),
                other => panic!("audio 解析错了：{other:?}"),
            }
            match parse(r#"{"cmd":"audio","on":false}"#) {
                Cmd::Audio { on, .. } => assert_eq!(on, Some(false)),
                other => panic!("audio 解析错了：{other:?}"),
            }
            // 数组（界面按 JSON 规范发数字）
            let a = match parse(r#"{"cmd":"audio","bands":[0.1,0.2,0.3]}"#) {
                Cmd::Audio { bands, .. } => bands.expect("数组要能解析"),
                other => panic!("audio 解析错了：{other:?}"),
            };
            assert_eq!((a[0], a[2]), (0.1, 0.3));
            assert_eq!(a[15], 0.3, "不足 16 个 ⇒ 后面补最后一个（与 WE_AUDIO_BANDS 同口径）");
            // 字符串 + 单值 ⇒ 16 段全等（离线探针口径）
            match parse(r#"{"cmd":"audio","bands":"1"}"#) {
                Cmd::Audio { bands, .. } => {
                    let b = bands.unwrap();
                    assert!(b.iter().all(|v| *v == 1.0));
                }
                other => panic!("audio 解析错了：{other:?}"),
            }
            // 越界夹到 0..1；解析不到的 ⇒ None
            assert_eq!(parse_bands(r#"{"cmd":"audio","bands":[2.0,-1.0]}"#).unwrap()[0], 1.0);
            assert_eq!(parse_bands(r#"{"cmd":"audio","bands":[2.0,-1.0]}"#).unwrap()[1], 0.0);
            assert!(parse_bands(r#"{"cmd":"audio","bands":"abc"}"#).is_none());
            assert!(parse_bands(r#"{"cmd":"status"}"#).is_none());
            assert_eq!(parse_bands(r#"{"cmd":"audio","bands":"0.5 0.6"}"#).unwrap()[1], 0.6);
        }

        /// 音频 **sink 选择**（`{"cmd":"audio","sink":…}`）与 **列 sink**（`"sinks":true`）：
        /// 两种写法都要被解析出来（`sinks` 是**裸布尔**，只有 `bool_field` 收得住 ——
        /// `field` 见非引号就返回 None，用它会把"列列表"静默丢成"什么也不做"）。
        #[test]
        fn audio_cmd_parses_sink_and_sinks() {
            for spec in ["default", "593", "alsa_output.pci-0000_09_00.1.hdmi-stereo", "all"] {
                let line = format!(r#"{{"cmd":"audio","sink":"{spec}"}}"#);
                match parse(&line) {
                    Cmd::Audio { sink, sinks, .. } => {
                        assert_eq!(sink.as_deref(), Some(spec), "sink 丢了：{line}");
                        assert!(!sinks);
                    }
                    other => panic!("audio sink 解析错了：{other:?}"),
                }
            }
            // 与 on 同用
            match parse(r#"{"cmd":"audio","sink":"whtest","on":true}"#) {
                Cmd::Audio { sink, on, .. } => {
                    assert_eq!(sink.as_deref(), Some("whtest"));
                    assert_eq!(on, Some(true));
                }
                other => panic!("audio sink+on 解析错了：{other:?}"),
            }
            // 列列表：裸布尔
            match parse(r#"{"cmd":"audio","sinks":true}"#) {
                Cmd::Audio { sinks, sink, on, bands, .. } => {
                    assert!(sinks, "sinks:true 没被认出来");
                    assert!(sink.is_none() && on.is_none() && bands.is_none());
                }
                other => panic!("audio sinks 解析错了：{other:?}"),
            }
            // `"sinks":false` / 没写 ⇒ 不是列列表
            assert!(!matches!(
                parse(r#"{"cmd":"audio","sinks":false}"#),
                Cmd::Audio { sinks: true, .. }
            ));
            assert!(!matches!(
                parse(r#"{"cmd":"audio","sink":"default"}"#),
                Cmd::Audio { sinks: true, .. }
            ));
            // 协议验证本体：`field` 收不住裸布尔（这就是上面必须用 `bool_field` 的理由）
            assert_eq!(field(r#"{"cmd":"audio","sinks":true}"#, "sinks"), None);
            assert!(bool_field(r#"{"cmd":"audio","sinks":true}"#, "sinks"));
        }

        /// 值里的**花括号/逗号/冒号**不能把解析带偏（属性值可能是任意文本/颜色）。
        #[test]
        fn set_prop_values_with_delimiters_survive() {
            let line = r#"{"cmd":"set-prop","props":{"color":"0.1 0.2 0.3","note":"a,b:c","braces":"{x}"}}"#;
            let props = parse_props(line);
            assert_eq!(props.len(), 3, "三个键都要在：{props:?}");
            assert_eq!(props[0], ("color".to_string(), "0.1 0.2 0.3".to_string()));
            assert_eq!(props[1], ("note".to_string(), "a,b:c".to_string()));
            assert_eq!(props[2], ("braces".to_string(), "{x}".to_string()));
        }

        /// 转义：名字/值里的 `\"` 与 `\\` 按 JSON 解回来。
        #[test]
        fn set_prop_unescapes_strings() {
            let line = r#"{"cmd":"set-prop","props":{"a\"b":"c\\d"}}"#;
            let props = parse_props(line);
            assert_eq!(props, vec![(r#"a"b"#.to_string(), r#"c\d"#.to_string())]);
        }

        /// `bool_field` 认裸 true，缺字段/别的值都是 false。
        #[test]
        fn bool_field_reads_bare_true() {
            assert!(bool_field("{\"clear\":true}", "clear"));
            assert!(!bool_field("{\"clear\":false}", "clear"));
            assert!(!bool_field("{\"clear\":\"true\"}", "clear"));
            assert!(!bool_field("{\"other\":true}", "clear"));
        }
    }
}

const NAMESPACE: &str = "we-native";

/// "上次应用的壁纸"状态文件。
/// 内核本身**无状态**是设计原则（界面发什么就执行什么、不记档）。但"重启后桌面空着"并不是
/// 我们想要的"无状态"—— 持久单元把它拉起来之后应当自己回到上次那条。所以这里只记**一条
/// 用户显式下发过的命令**，不猜、不推导任何偏好。
fn last_applied_file() -> std::path::PathBuf {
    let cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".cache")
        });
    cache.join("wallpaper-hub").join("last-applied")
}

fn save_last_applied(kind: &str, path: &str) {
    let f = last_applied_file();
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&f, format!("{kind}\n{path}\n"));
}

/// 读回状态；**路径不存在就当作没有**（壁纸目录被删/移动了就不该去恢复它）。
fn read_last_applied() -> Option<(String, String)> {
    let s = std::fs::read_to_string(last_applied_file()).ok()?;
    let mut it = s.lines();
    let kind = it.next()?.trim().to_string();
    let path = it.next()?.trim().to_string();
    if kind.is_empty() || path.is_empty() || !std::path::Path::new(&path).exists() {
        return None;
    }
    Some((kind, path))
}

fn main() {
    // --version / --help answer without touching the Wayland display, so a package
    // smoke test (`we-native --version`) works in a headless shell.
    if let Some(first) = std::env::args().nth(1) {
        match first.as_str() {
            "--version" | "-V" => {
                println!("we-native {}", env!("CARGO_PKG_VERSION"));
                process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "we-native {} — Wayland layer-shell wallpaper kernel\n\n\
                     Usage:\n  \
                     we-native --serve <socket>          run as a service, accept control-plane commands\n  \
                     we-native --scene <dir>             play one scene wallpaper\n  \
                     we-native --image <file> [--seconds N]\n  \
                     we-native --video <file> [--video-max WxH]\n  \
                     we-native --list-outputs            list outputs and exit\n  \
                     we-native --globals                 list compositor globals and exit\n",
                    env!("CARGO_PKG_VERSION")
                );
                process::exit(0);
            }
            _ => {}
        }
    }

    display_guard::install_panic_hook();
    let args: Vec<String> = std::env::args().collect();
    let list_only = args.iter().any(|a| a == "--list-outputs");
    let image_arg = args
        .iter()
        .position(|a| a == "--image")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let seconds: Option<u64> = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // 只跟踪窗口状态、不创建任何层壳表面（测试用：不动用户桌面）
    let track_only = args.iter().any(|a| a == "--track-only");
    // 调试用：把"最大化"也当成睡眠条件（正式规则只有 全屏/铺满）
    let sleep_on_maximized = args.iter().any(|a| a == "--sleep-on-maximized");
    // 调试/验收用：人为把主循环停摆这么多毫秒（模拟"卡在 eglSwapBuffers 里"）。
    // 默认**没有**（上屏路径上等于不存在）；用来实测看门狗。
    let mut debug_stall_ms: Option<u64> = args
        .iter()
        .position(|a| a == "--debug-stall-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            std::env::var("WE_DEBUG_STALL_MS")
                .ok()
                .and_then(|s| s.parse().ok())
        });
    // 停摆只做一次（默认）；`--debug-stall-count N` 重复 N 次，用来验证"反复抖动才开火"。
    let mut debug_stall_count: u32 = args
        .iter()
        .position(|a| a == "--debug-stall-count")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    // probe only: shut the compositor socket down after N ms to simulate a dead
    // compositor. No flag => None => zero code on the production path.
    let mut debug_kill_wl_ms: Option<u64> = args
        .iter()
        .position(|a| a == "--debug-kill-wayland-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // probe only: run the "compositor closed our surface" body after N ms. No flag
    // => None => zero code on the production path.
    let mut debug_drop_surface_ms: Option<u64> = args
        .iter()
        .position(|a| a == "--debug-drop-surface-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // b3：视频源（用 ffmpeg 解码）
    let video_arg = args
        .iter()
        .position(|a| a == "--video")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // b4：控制面开关 —— `--serve <socket>` 常驻并接受命令；`--stats` 每 5 秒自报开销
    let serve_arg = args
        .iter()
        .position(|a| a == "--serve")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // s2：scene 壁纸目录（自研渲染器直接渲它的 scene.pkg）
    let scene_arg = args
        .iter()
        .position(|a| a == "--scene")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let show_stats = args.iter().any(|a| a == "--stats");
    // b3：解码尺寸上限（默认 = 屏幕物理尺寸；给个小值可换更省资源，缩放交给 GPU）
    let video_max = args
        .iter()
        .position(|a| a == "--video-max")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| {
            let mut it = s.split('x');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        });
    // b3 探针：只解码、不上屏（不碰桌面）
    let video_probe_arg = args
        .iter()
        .position(|a| a == "--video-probe")
        .and_then(|i| args.get(i + 1))
        .cloned();

    if let Some(path) = &video_probe_arg {
        let (w, h) = args
            .iter()
            .position(|a| a == "--size")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| {
                let mut it = s.split('x');
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .unwrap_or((1920, 1080));
        let n = args
            .iter()
            .position(|a| a == "--frames")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        println!("########## ffmpeg 解码探针：{path} → {w}x{h} BGRA，取 {n} 帧 ##########");
        video::probe(path, w, h, n);
        return;
    }

    let conn = Connection::connect_to_env().expect("连不上 Wayland（WAYLAND_DISPLAY 设了吗）");
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("取全局对象失败");
    let qh = event_queue.handle();

    // globals：直接问合成器支持哪些协议（决定"全屏检测"用哪个协议）
    if args.iter().any(|a| a == "--globals") {
        let mut list: Vec<(String, u32)> = globals
            .contents()
            .clone_list()
            .into_iter()
            .map(|g| (g.interface, g.version))
            .collect();
        list.sort();
        println!("合成器暴露 {} 个全局对象：", list.len());
        for (iface, ver) in &list {
            let mark = if iface.contains("toplevel")
                || iface.contains("viewporter")
                || iface.contains("layer_shell")
                || iface.contains("fractional")
                || iface.contains("dmabuf")
                || iface.contains("session_lock")
                || iface.contains("idle")
            {
                "  ← 相关"
            } else {
                ""
            };
            println!("  {iface:<52} v{ver}{mark}");
        }
        return;
    }

    let compositor = CompositorState::bind(&globals, &qh).expect("没有 wl_compositor");
    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(ls) => {
            eprintln!("[ok] zwlr_layer_shell_v1 可用");
            ls
        }
        Err(e) => {
            eprintln!("[失败] 这个合成器没有 zwlr_layer_shell_v1：{e}");
            process::exit(2);
        }
    };
    let shm = Shm::bind(&globals, &qh).expect("没有 wl_shm");
    // wp_viewporter 没有事件，用 SimpleGlobal 即可。它让我们把「物理尺寸的缓冲」映射到「逻辑尺寸的表面」，
    // 从而在 1.25 倍分数缩放的合成器上保持锐利（否则合成器会把逻辑尺寸的缓冲放大 1.25 倍）。
    let viewporter = SimpleGlobal::<wp_viewporter::WpViewporter, 1>::bind(&globals, &qh).ok();
    if viewporter.is_some() {
        eprintln!("[ok] wp_viewporter 可用（按物理尺寸渲染 + 缩放到逻辑尺寸）");
    } else {
        eprintln!("[警告] 没有 wp_viewporter：只能按逻辑尺寸渲染，会被合成器放大 1.25 倍");
    }
    let pool = SlotPool::new(512 * 512 * 4, &shm).expect("建 wl_shm 池失败");

    // 窗口身份来源：自己绑 ext_foreign_toplevel_list_v1（版本 1..=1，同 COSMIC client-toolkit 的做法）
    let foreign_list = match globals.bind::<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, Wall, GlobalData>(
        &qh,
        1..=1,
        GlobalData,
    ) {
        Ok(l) => {
            eprintln!("[ok] ext_foreign_toplevel_list_v1 可用（窗口身份来源）");
            Some(l)
        }
        Err(e) => {
            eprintln!("[警告] 拿不到 ext_foreign_toplevel_list_v1（{e}）");
            None
        }
    };
    // 窗口状态来源（zcosmic_toplevel_info_v1 v2；COSMIC 不提供 wlr-foreign-toplevel）
    let ct_info = match globals.bind::<ct::zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, Wall, GlobalData>(
        &qh,
        2..=3,
        GlobalData,
    ) {
        Ok(info) => {
            eprintln!("[ok] zcosmic_toplevel_info_v1 可用（全屏/最大化状态来源）");
            Some(info)
        }
        Err(e) => {
            eprintln!("[警告] 拿不到 zcosmic_toplevel_info_v1（{e}）：全屏检测不可用");
            None
        }
    };

    // ── 鼠标输入源：wl_seat → wl_pointer ────────────────────────
    // 我们的层壳表面在 **Background 层**、输入区为空 ⇒ 合成器不会把指针事件发给我们
    // （这是"壁纸不抢输入"的前提，必须保持）。想拿鼠标位置有两条路：
    // ① `mouse_input`（`WE_MOUSE_POINTER`，默认开）：给表面设一个**铺满输出的输入区**，
    // 于是**指针在空桌面上时**（没有窗口盖着）我们能收到 enter/motion ⇒ 归一化后
    // 喂给场景。代价：空桌面上的点击会落到这张表面上（我们忽略它们；WE 在 Windows 上
    // 的 WorkerW 也是这个形态）。`WE_MOUSE_POINTER=0` 完全恢复旧行为。
    // ② 控制面 `{"cmd":"mouse","x":..,"y":..}`：任何知道指针位置的进程都能推（脚本/离线验证）。
    let mouse_input = std::env::var("WE_MOUSE_POINTER").map(|v| v != "0").unwrap_or(true);
    let seat = if mouse_input {
        match globals.bind::<wayland_client::protocol::wl_seat::WlSeat, Wall, GlobalData>(&qh, 1..=9, GlobalData)
        {
            Ok(s) => {
                eprintln!("[ok] wl_seat 可用（鼠标输入源：指针在空桌面上时进场景）");
                Some(s)
            }
            Err(e) => {
                eprintln!("[警告] 拿不到 wl_seat（{e}）：鼠标只能走控制面 `{{\"cmd\":\"mouse\"}}`");
                None
            }
        }
    } else {
        eprintln!("[鼠标] WE_MOUSE_POINTER=0 ⇒ 不取指针（空桌面点击行为与旧版一致）");
        None
    };

    // 全局指针：绑 `ext_image_copy_capture` 两个管理器（光标会话用得到）。
    // 实测：在 COSMIC 上**不抓帧**就能拿到指针绝对坐标：
    // 0.5 秒 150 条、间隔 2~4ms、指针压在任何窗口上都有、无授权框、零带宽。
    // 拿不到 ⇒ 两个都 None ⇒ `ensure_global_pointer` 直接放弃 ⇒ 行为与加这一路之前完全一致。
    // `WE_POINTER_GLOBAL=0` 可整体关（A/B 用）。
    let pointer_global_on = std::env::var("WE_POINTER_GLOBAL").map(|v| v != "0").unwrap_or(true);
    let (cap_src_manager, cap_manager) = if pointer_global_on {
        let a = globals.bind::<cap_src_mgr::ExtOutputImageCaptureSourceManagerV1, Wall, GlobalData>(
            &qh,
            1..=1,
            GlobalData,
        );
        let b = globals
            .bind::<cap_mgr::ExtImageCopyCaptureManagerV1, Wall, GlobalData>(&qh, 1..=1, GlobalData);
        match (a, b) {
            (Ok(a), Ok(b)) => {
                eprintln!("[ok] 全局指针：ext_image_copy_capture 管理器就位（光标会话）");
                (Some(a), Some(b))
            }
            (a, b) => {
                eprintln!(
                    "[警告] 全局指针不可用（source_mgr={} capture_mgr={}）⇒ 只用 wl_pointer",
                    a.is_ok(),
                    b.is_ok()
                );
                (None, None)
            }
        }
    } else {
        eprintln!("[指针] WE_POINTER_GLOBAL=0 ⇒ 不建全局指针会话（只保留 wl_pointer）");
        (None, None)
    };

    // 图片：--list-outputs 时不需要；给了 --video 时用 1x1 占位（像素由视频提供）
    let image = match &image_arg {
        Some(p) if video_arg.is_none() => {
            let t0 = std::time::Instant::now();
            let img = image::open(p)
                .unwrap_or_else(|e| panic!("读不了 {p:?}：{e}"))
                .to_rgba8();
            println!(
                "图片 {p:?}  {}x{}  解码 {} ms",
                img.width(),
                img.height(),
                t0.elapsed().as_millis()
            );
            img
        }
        _ => image::RgbaImage::new(1, 1),
    };

    let mut wall = Wall {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        viewporter,
        pool,
        surfaces: Vec::new(),
        image,
        drawn: 0,
        foreign_list,
        ct_info,
        windows: Vec::new(),
        suppressed: false,
        video: None,
        video_path: video_arg.clone(),
        video_max,
        vdec: None,
        video_gpu: None,
        // 视频 0 拷贝默认开（`WE_VIDEO_ZEROCOPY=0` 退回 ffmpeg 子进程那条路做 A/B）
        video_zerocopy: std::env::var("WE_VIDEO_ZEROCOPY")
            .map(|v| v != "0")
            .unwrap_or(true),
        video_gpu_errs: 0,
        video_commit_hold: false,
        video_next: None,
        scene_path: scene_arg.clone(),
        audio: None,
        audio_for: None,
        // 音频分析跟哪一路输出：初值从 `WE_AUDIO_DEVICE` 推（内核切换时也写这个变量）——
        // 这样 status.audio 对"外部就用环境变量指定了设备"的情形也是诚实的，不乱报 default。
        audio_sink: {
            let dev = std::env::var("WE_AUDIO_DEVICE").unwrap_or_default();
            let dev = dev.trim().to_string();
            if dev.is_empty() {
                audio_sinks::SinkSel::Default
            } else {
                audio_sinks::SinkSel::Named(
                    dev.strip_suffix(".monitor").unwrap_or(&dev).to_string(),
                )
            }
        },
        audio_sink_note: None,
        scene: None,
        scene_t0: None,
        last_phys: None,
        scene_errs: 0,
        dpy_ptr: conn.backend().display_ptr() as usize,
        fps: scene_fps(),
        sleep_mode: std::env::var("WE_SLEEP_MODE").unwrap_or_else(|_| "fullscreen".to_string()),
        render_scale: std::env::var("WE_RENDER_SCALE").unwrap_or_else(|_| "physical".to_string()),
        sound_on: std::env::var("WE_SOUND").map(|v| v != "0").unwrap_or(true),
        time_scale: we_scene_render::time_scale(),
        // 启动读一次默认 sink 的实际音量（一次 pactl，几毫秒）⇒ status 一开始就是诚实的
        sink_volume: audio_sinks::default_volume().ok().map(|(_, v)| v),
        sink_name: audio_sinks::default_sink(),
        sink_volume_note: None,
        effects_on: std::env::var("WE_EFFECTS").map(|v| v != "none").unwrap_or(true),
        // 鼠标输入：seat 已在上面的全局绑定处拿到（`WE_MOUSE_POINTER=0` 时为 None）
        seat,
        pointer: None,
        cursor_surf: None,
        mouse: None,
        mouse_events: 0,
        mouse_input,
        mouse_log_at: None,
        // 诊断：`WE_MOUSE_DEBUG=1` 才打逐事件日志（默认关 ⇒ 与旧版行为完全一致）
        mouse_debug: std::env::var("WE_MOUSE_DEBUG").map(|v| v != "0").unwrap_or(false),
        pointer_events: 0,
        mouse_enter: 0,
        mouse_leave: 0,
        mouse_motion: 0,
        mouse_src: "none",
        // 全局指针：管理器在上面的全局绑定处拿到（拿不到就是 None ⇒ 整条路关，行为与旧版一致）
        cap_src_manager,
        cap_manager,
        cap_source: None,
        cap_session: None,
        cap_cursor: None,
        pointer_global_ok: false,
        pointer_global_ready: false,
        pointer_global_base: (0.0, 0.0),
        pointer_global_events: 0,
        pointer_global_enter: 0,
        pointer_global_leave: 0,
        mouse_wl: None,
        mouse_cs: None,
        mouse_override_until: None,
        mouse_override_ms: std::env::var("WE_MOUSE_OVERRIDE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000),
        dump_prefix: String::new(),
        dump_left: 0,
        dump_n: 0,
        dump_log: None,
        dump_enc: None,
        cm_frames: 0,
        cm_seen: 0,
        cm_commits: 0,
        cm_max: 0,
        cm_this: 0,
        cm_total: 0,
        frame_cb: None,
        frame_wait_since: None,
        cb_done: 0,
        cb_gated: 0,
        cb_timeouts: 0,
        cb_last_ts: 0,
        cb_gaps: [0.0; 128],
        cb_gap_i: 0,
        frame_cb_on: std::env::var("WE_FRAME_CB").map(|v| v.trim() != "0").unwrap_or(true),
        ov: None,
        fit_mode: we_scene_render::current_fit(),
        color: we_scene_render::current_color(),
        scene_next: None,
        fade_ms: fade_ms_env(),
        fade_fallback: fade_fallback_env(),
        fade_src: None,
        fade_t_apply: None,
        commit_deadline_ms: std::env::var("WE_COMMIT_DEADLINE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(400),
        commit_pending_since: None,
        commit_pending_laps: 0,
        commit_lost_ms: commit_guard::limit_from(
            std::env::var("WE_COMMIT_MAX_PENDING_MS").ok().as_deref(),
        ),
        dump_skips: 0,
        image_path: image_arg.as_ref().map(|p| p.to_string_lossy().into_owned()),
        user_paused: false,
        server: None,
        qh: None,
        sleep_on_maximized,
        last_suppressed: false,
        polls: 0,
        ticks: 0,
        wl_wakes: 0,
        cmd_wakes: 0,
        vid_wakes: 0,
        last_timeout_ms: -2,
        last_ready: 0,
        poll_errno: 0,
        p_npos: 0,
        p_nzero: 0,
        p_nerr: 0,
        p_to0: 0,
        to_hist: [0; 8],
        to_hist_i: 0,
        due_asked: 0,
        due_true: 0,
    };

    // 队列句柄要在**这个 roundtrip 之前**存进 `wall`：`wl_seat` 的 `capabilities`
    // 事件就是在这条 roundtrip 里被分发的（`registry_queue_init` 之后我们才 bind 的 seat），
    // 而那时要拿 `qh` 去 `get_pointer`。若等到下面才赋值 ⇒ 事件被白白丢掉 ⇒
    // **永远拿不到指针**（合成器不会再发第二次 capabilities）。
    // 实测：赋值在 roundtrip 之后时，日志只有 `[ok] wl_seat 可用`、
    // 没有 `合成器给了 wl_pointer`。
    wall.qh = Some(qh.clone());

    // 收齐已有显示器 / seat 等全局对象
    event_queue.roundtrip(&mut wall).expect("roundtrip 失败");

    // 注意：不要在这里（收到 info 对象的 done 之前）就调 get_cosmic_toplevel ——
    // 时序不对会被合成器判为 malformed message。跟踪由 info 对象的 done 事件触发。

    if list_only {
        println!("显示器：");
        for output in wall.output_state.outputs() {
            match wall.output_state.info(&output) {
                Some(i) => {
                    let (w, h) = i.logical_size.unwrap_or((0, 0));
                    println!(
                        "  {}  {}x{}",
                        i.name.clone().unwrap_or_else(|| "?".into()),
                        w,
                        h
                    );
                }
                None => println!("  （信息还没到）"),
            }
        }
        return;
    }

    if image_arg.is_none() && video_arg.is_none() && scene_arg.is_none() && !track_only && serve_arg.is_none() {
        eprintln!("用法: we-native --image <图> | --video <视频> | --scene <壁纸目录> [--seconds N] [--video-max WxH] [--stats]");
        eprintln!("      we-native --serve <socket 路径> [--stats]        # 常驻控制面");
        eprintln!("      we-native --list-outputs | --globals | --track-only | --video-probe <视频>");
        process::exit(2);
    }

    // 一次性模式（--image / --video）立刻建表面；**常驻模式（--serve）懒创建**：
    // 没被 apply 之前完全不占背景层，避免和 waywallen 抢同一层。
    if !track_only && serve_arg.is_none() {
        wall.create_surfaces(&qh);
    }
    wall.qh = Some(qh.clone());

    // b4：起控制面（常驻模式）
    if let Some(p) = &serve_arg {
        match serve::Server::start(p) {
            Ok(s) => {
                println!("[控制面] 监听 {p}");
                println!("[控制面] 命令：apply / status / pause / resume / set / set-prop / quit（一行一个 JSON）");
                wall.server = Some(s);
            }
            Err(e) => eprintln!("[控制面] 起不来：{e}"),
        }
    }

    // 开机恢复：持久单元（systemd --user）把我们拉起来之后，把"上次应用的壁纸"重新下发一次，
    // 做到"重启后桌面自己回来"。刻意走**和真实命令完全相同的 `handle_cmd` 路径** ——
    // 恢复逻辑一旦和命令路径分家，迟早两边行为不一致。
    // ⚠ 注意：属性覆盖表与帧率**已经**在这次 apply 里恢复了（`apply_source` 里调的
    // `restore_app_state` —— 见下面 ② 的注释），所以这里不能再补一次（会多打一行日志）。
    let mut restored_by_apply = false;
    if serve_arg.is_some() {
        if let Some((kind, path)) = read_last_applied() {
            match std::os::unix::net::UnixStream::pair() {
                // `_peer` 必须**留在作用域里**：提前丢掉会让 handle_cmd 的应答写入失败（EPIPE）
                Ok((me, _peer)) => {
                    println!("[控制面] 恢复上次的壁纸：{kind} {path}");
                    let is_scene = kind == "scene";
                    let quit = wall.handle_cmd(
                        serve::Cmd::Apply {
                            kind: kind.clone(),
                            path: path.clone(),
                        },
                        me,
                    );
                    // apply 成功时 `apply_source` 会把 `scene_path` 设成这条 ⇒ 用它判断
                    // "属性/帧率的恢复动作已经在 apply 里做过了"（失败时 scene_path 不动）。
                    restored_by_apply = is_scene && wall.scene_path.as_deref() == Some(path.as_str());
                    println!("[控制面] 恢复完成（quit={quit}）");
                }
                Err(e) => eprintln!("[控制面] 恢复跳过（socketpair 失败：{e}）"),
            }
        }
    }

    // ② **启动恢复（app 存档）**：属性覆盖表与逐场景帧率都是内核运行时状态，重启就没了
    // ⇒ 启动时按 app 的存档（`~/.local/share/wallpaper-hub/*.json`）把**当前这一个场景**
    // 的属性与 fps 写回运行时（`app_state` 模块头有完整来龙去脉）。只在启动做一次，
    // **不轮询**：app 随时会重发，它优先。
    // serve 模式下"当前场景"是上面那条 apply 拉回来的 ⇒ 已经在 `apply_source` 里恢复过
    // （`restored_by_apply`），这里只负责"当前已有场景但这次启动没走 apply"的那条路
    // （一次性 CLI `--scene`）。没有当前场景 ⇒ 什么都不做，也不打日志。
    if !restored_by_apply && wall.scene_path.is_some() {
        wall.restore_app_state("内核启动");
    }

    // 每 5 秒自报开销（用 --stats 开；常驻服务模式默认安静，免得刷屏）
    if show_stats {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(5));
            let (cpu_s, rss_kb) = self_stats();
            println!("[stats] CPU 累计 {cpu_s:.2}s   常驻 {:.1} MB", rss_kb as f64 / 1024.0);
        });
    }

    let deadline = seconds.map(|s| std::time::Instant::now() + Duration::from_secs(s));

    // s3：看门狗。**在 serve（常驻服务）模式下才起**：这是上屏那个进程，也正是会卡死的那个。
    // 一次性 CLI（--image/--video）有 --seconds 兜底，不必也不该被它咬。
    // `WE_WATCHDOG=0` 可以关掉（排障时用；上屏路径不设它）。
    if serve_arg.is_some() && std::env::var("WE_WATCHDOG").map(|v| v != "0").unwrap_or(true) {
        watchdog::spawn();
        println!(
            "[看门狗] 已起：宽限 {}s，退出码 {}（systemd Restart=on-failure 会重拉）",
            watchdog::GRACE.as_secs(),
            watchdog::EXIT_CODE
        );
    }

    // slow-lap attribution. Off unless WE_LOOP_SLOW_MS is set; when on it
    // prints one line per lap that took longer than the threshold.
    let _slow_ms = slowloop::init_from_env();

    // say which side of the switch we are on (the deployed unit does not set it,
    // so the default is what the desktop gets).
    if wall.commit_lost_ms == 0 {
        println!("[交帧] 永不回收检测：关（WE_COMMIT_MAX_PENDING_MS=0）⇒ 合成器彻底不回时画面会停在最后一帧");
    } else {
        println!(
            "[交帧] 永不回收检测：交帧挂满 {} ms ⇒ 退出 {} 交给 systemd 重建表面",
            wall.commit_lost_ms,
            commit_guard::EXIT_CODE_SUBMIT_LOST
        );
    }

    let loop_start = std::time::Instant::now();
    // how many laps saw a would-block flush (0 on a healthy session).
    let mut flush_blocked: u64 = 0;
    let mut wl_fd_known: Option<libc::c_int> = None;
    loop {
        let lap_t0 = slowloop::start();
        // Frames come from whichever source is live (scene renderer / 0-copy video / ffmpeg
        // video / image layers). `scene.frames` alone reads 0 for a video wallpaper, so every
        // slow-lap line on a video wallpaper used to print `frames=0` while frames were climbing.
        let frames_before = wall.frames_presented();
        // probe: exercise the surface-closed recovery (same body the compositor's
        // `closed` event calls) without unplugging a display.
        if let Some(ms) = debug_drop_surface_ms {
            if loop_start.elapsed().as_millis() as u64 >= ms && !wall.surfaces.is_empty() {
                debug_drop_surface_ms = None;
                eprintln!("[N22r14 probe] surface_closed(0) after {ms} ms");
                wall.surface_closed(0, &qh);
            }
        }
        // probe: simulate a dead compositor by shutting our socket down.
        if let (Some(ms), Some(fd)) = (debug_kill_wl_ms, wl_fd_known) {
            if loop_start.elapsed().as_millis() as u64 >= ms {
                debug_kill_wl_ms = None;
                eprintln!("[N23 probe] shutdown(wayland fd={fd}) after {ms} ms");
                unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
            }
        }
        // ── 三路 poll：Wayland + 控制面 + 视频帧，一起等 ──
        // 空闲时集合里没有任何"有数据的 fd"，poll(-1) 就是真的永久睡着（零唤醒）；
        // 而命令或帧一到就立刻返回 —— 不需要定时器，也没有轮询。
        if let Err(e) = event_queue.flush() {
            let msg = format!("Wayland flush 失败：{e}");
            // EAGAIN means "the compositor is not reading right now", not "the
            // compositor is gone". Retry on the next lap instead of rebuilding the whole kernel
            // (measured: a 12 s freeze used to end in exit 75 at 9.67 s).
            if display_guard::is_would_block(&msg) {
                flush_blocked += 1;
                if flush_blocked == 1 || flush_blocked % 600 == 0 {
                    println!(
                        "[交帧] 合成器读得慢（flush EAGAIN，第 {flush_blocked} 次）⇒ 不当断开，继续跑"
                    );
                }
            } else {
                display_guard::lost(msg);
            }
        }
        let mut guard = event_queue.prepare_read();
        let mut fds: Vec<libc::pollfd> = Vec::new();
        let (mut i_wl, mut i_cmd, mut i_vid) = (usize::MAX, usize::MAX, usize::MAX);
        if let Some(g) = guard.as_ref() {
            i_wl = fds.len();
            wl_fd_known = Some(g.connection_fd().as_raw_fd());
            fds.push(libc::pollfd {
                fd: g.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(s) = wall.server.as_ref() {
            i_cmd = fds.len();
            fds.push(libc::pollfd {
                fd: s.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if wall.is_active() {
            if let Some(d) = wall.video.as_ref() {
                i_vid = fds.len();
                fds.push(libc::pollfd {
                    fd: d.out.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }

        if fds.is_empty() {
            // 极罕见（连接里已有待处理事件，prepare_read 拿不到 guard）。
            // 有截止时间时不能永久阻塞，否则定时退出会被吃掉；但也**不能只睡**——见下面
            // 的注释：`prepare_read` 返回 None 恰恰是「队列里已有事件」。
            match no_fd_action(deadline.is_some()) {
                NoFdAction::DispatchPending => {
                    // `prepare_read` returning None means "the queue already
                    // holds events, dispatch them first" - NOT "nothing happened". The old
                    // code only slept here, so those pending events could never be
                    // dispatched: the next lap got None again, `fds` stayed empty, and the
                    // loop degenerated into a 10 ms heartbeat that never polls, never
                    // dispatches and never reaches `tick_scene`. Measured (`--scene ...
                    // seconds 6`, scene 3704273480): 462 consecutive laps at
                    // `other 10.1ms`, frames stuck at 1. Serve mode never hits this branch
                    // because the command fd keeps `fds` non-empty - that is why only the
                    // one-shot path was broken.
                    watchdog::park(); // a legitimate short wait; do not let the watchdog bite
                    watchdog::set_phase(watchdog::PHASE_DISPATCH);
                    if let Err(e) = event_queue.dispatch_pending(&mut wall) {
                        display_guard::lost(format!("Wayland 事件分发失败：{e}"));
                    }
                    slowloop::mark(we_scene_render::trace::Mark::DispatchEnd);
                    std::thread::sleep(Duration::from_millis(1));
                }
                NoFdAction::BlockingDispatch => {
                    watchdog::park(); // 这段是合法的阻塞等待，别让看门狗误判
                    watchdog::set_phase(watchdog::PHASE_DISPATCH);
                    if let Err(e) = event_queue.blocking_dispatch(&mut wall) {
                        display_guard::lost(format!("Wayland 事件分发失败：{e}"));
                    }
                }
            }
        } else {
            // poll 超时：有 --seconds 截止时间就睡到那一刻（空闲也能按时退出）；
            // 常驻模式（无截止）才用 -1 永久阻塞 —— 那才是"零唤醒"的形态。
            let timeout_ms: libc::c_int = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    left.as_millis().min(i32::MAX as u128) as libc::c_int
                }
                None => -1,
            };
            // s2：有 scene 在放时按帧率唤醒（睡眠/暂停时不参与 ⇒ 仍是 -1，真 0 唤醒）
            // s3：视频 0 拷贝同理（老路视频是 ffmpeg 管道可读驱动，不用这里的超时）
            // ⚠ `due_left` 是**精确**剩余时间，补睡用它（见 `scene_due_left` 的注释）；
            // `scene_due_in_ms` 已向上取整，睡满会把每帧多等最多 1ms。
            let mut due_left: Option<Duration> = None;
            let timeout_ms = if wall.is_active()
                && (wall.scene_path.is_some() || wall.zerocopy_video_active())
            {
                let (left, due) = if wall.zerocopy_video_active() {
                    (wall.video_due_left(), wall.video_due_in_ms())
                } else {
                    (wall.scene_due_left(), wall.scene_due_in_ms())
                };
                due_left = left;
                wall.due_asked += 1;
                if timeout_ms < 0 {
                    due
                } else {
                    timeout_ms.min(due)
                }
            } else {
                timeout_ms
            };
            // probe: wake up at the scheduled kill time even when idle.
            let timeout_ms = if let Some(ms) = debug_kill_wl_ms {
                let left = ms.saturating_sub(loop_start.elapsed().as_millis() as u64);
                let left = left.max(1) as libc::c_int;
                if timeout_ms < 0 { left } else { timeout_ms.min(left) }
            } else {
                timeout_ms
            };
            wall.to_hist[wall.to_hist_i % 8] = timeout_ms;
            wall.to_hist_i += 1;

            // ── 看门狗：进 poll 之前把"我下一次该在什么时刻回来"发布出去 ──
            // timeout < 0 = 我现在合法地一动不动（被遮挡睡眠 / 用户暂停 / 没有源）⇒ park，别管我。
            // timeout >= 0 = 我承诺下次在这个毫秒数内回来；看门狗再叠自己的宽限。
            // 这一行是整套自恢复的关键：卡在 `eglSwapBuffers` 里时，主循环再也不会执行到下面，
            // 于是 due 不再更新 ⇒ 看门狗按"你违约了"处理。
            if timeout_ms < 0 {
                watchdog::park();
            } else {
                watchdog::due_in(Duration::from_millis(timeout_ms as u64));
            }
            watchdog::set_phase(watchdog::PHASE_POLL);

            // 调试/验收用：人为停摆，模拟"进了 poll 之后卡在 eglSwapBuffers 里再也出不来"。
            // 只在 `WE_DEBUG_STALL_MS`（或 --debug-stall-ms）显式设了时生效 ⇒ 上屏路径上等于不存在；
            // 用它可以在**完全不碰屏幕**的前提下实测看门狗的发现时间与自恢复。
            if let Some(ms) = debug_stall_ms {
                watchdog::due_in(Duration::from_millis(1000)); // 假装"下一帧 1 秒内该到"
                eprintln!(
                    "[调试] 人为停摆 {ms} ms（模拟 eglSwapBuffers 卡死）；还剩 {debug_stall_count} 次"
                );
                std::thread::sleep(Duration::from_millis(ms));
                // 只停这一次：旧版每圈都停，用来测"发现时间"够用，但测不了"停完是否恢复"。
                debug_stall_count = debug_stall_count.saturating_sub(1);
                if debug_stall_count == 0 {
                    debug_stall_ms = None;
                }
            }

            slowloop::mark(we_scene_render::trace::Mark::PollBegin);
            let poll_t0 = std::time::Instant::now();
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
            // 实测：poll 会带着 32ms 超时"提前返回 0"（每秒六千次 ⇒ 白白烧掉 0.3 个核）。
            // 不管它为什么提前醒，这里把差额补睡掉 —— 超时是我们自己定的，就该由我们兑现。
            // ⚠ 睡到**精确的截止时刻**（`due_left`）而不是睡满 `timeout_ms`：后者已向上取整，
            // 睡满会每帧多等最多 1ms。
            if n == 0 && timeout_ms > 0 {
                let want = due_left.unwrap_or_else(|| Duration::from_millis(timeout_ms as u64));
                let elapsed = poll_t0.elapsed();
                if elapsed < want {
                    watchdog::set_phase(watchdog::PHASE_SLEEP);
                    std::thread::sleep(want - elapsed);
                }
                watchdog::set_phase(watchdog::PHASE_POLL);
            }
            // poll end = after the compensating sleep, i.e. "how long we waited"
            slowloop::mark(we_scene_render::trace::Mark::PollEnd);
            wall.polls += 1;
            wall.last_timeout_ms = timeout_ms;
            if timeout_ms <= 0 {
                wall.p_to0 += 1;
            }
            if n < 0 {
                wall.p_nerr += 1;
                wall.poll_errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            } else if n == 0 {
                wall.p_nzero += 1;
            } else {
                wall.p_npos += 1;
                for f in fds.iter() {
                    wall.last_ready |= f.revents as u32;
                }
            }

            // ① Wayland 事件
            if i_wl != usize::MAX && fds[i_wl].revents & libc::POLLIN != 0 {
                wall.wl_wakes += 1;
                if let Some(g) = guard.take() {
                    let _ = g.read();
                }
            }
            drop(guard); // 先释放：dispatch_pending 需要 &mut event_queue
            if let Err(e) = event_queue.dispatch_pending(&mut wall) {
                display_guard::lost(format!("Wayland 事件分发失败：{e}"));
            }
            slowloop::mark(we_scene_render::trace::Mark::DispatchEnd);

            // ② 视频帧（可读 = 有一帧了；-re 让它按源帧率到达）
            if i_vid != usize::MAX && fds[i_vid].revents & libc::POLLIN != 0 && wall.is_active() {
                wall.vid_wakes += 1;
                wall.tick_video();
            }

            // ②.5 scene 帧（定时驱动；睡眠/暂停时不进来，poll 也就不会设超时）
            if wall.is_active() && wall.scene_path.is_some() && {
                let d = wall.scene_due();
                wall.due_true += u64::from(d);
                d
            } {
                wall.tick_scene();
                wall.ticks += 1;
            }

            // ②.55 配乐：子进程播完一首就按模式切下一首（每轮都问一下，很便宜）
            if let Some(a) = wall.audio.as_mut() {
                a.tick();
            }

            // ②.56 合成后取帧：只看超时 / 该不该起下一张（很便宜；没有抓帧任务时等于不存在）
            if wall.ov.is_some() {
                ovdump::tick(&mut wall);
            }

            // ②.6 视频 0 拷贝帧（定时驱动；老路视频仍是管道可读驱动，走上面的 ②）
            if wall.is_active() && wall.zerocopy_video_active() && {
                let d = wall.video_due();
                wall.due_true += u64::from(d);
                d
            } {
                wall.tick_video();
                wall.ticks += 1;
            }

            // ③ 控制面命令
            if i_cmd != usize::MAX && fds[i_cmd].revents & libc::POLLIN != 0 {
                wall.cmd_wakes += 1;
                let cmds = match wall.server.as_mut() {
                    Some(s) => s.drain(),
                    None => Vec::new(),
                };
                if !cmds.is_empty() {
                    // `apply` 要重建 GL 上下文/解析场景（实测 0.5~2s）：这是**合法慢操作**，
                    // 不能按帧节奏（几十毫秒）去要求它 ⇒ 处理命令期间把上限临时放到 CMD_ARM。
                    // 若它真的永久卡住，仍然会在 CMD_ARM 之后被看门狗咬出来。
                    watchdog::due_in(watchdog::CMD_ARM);
                    watchdog::set_phase(watchdog::PHASE_COMMAND);
                }
                slowloop::mark(we_scene_render::trace::Mark::CmdBegin);
                for (c, conn) in cmds {
                    if wall.handle_cmd(c, conn) {
                        println!("[控制面] 收到 quit，退出");
                        watchdog::park();
                        return;
                    }
                }
            }
        }

        slowloop::mark(we_scene_render::trace::Mark::CmdEnd);
        if let Some(t0) = lap_t0 {
            let frames_now = wall.frames_presented();
            slowloop::finish(
                t0,
                frames_now.saturating_sub(frames_before),
                loop_start.elapsed().as_secs_f64(),
            );
        }

        if let Some(d) = deadline {
            if std::time::Instant::now() >= d {
                // `drawn` only counts image layers and `scene frames` is 0 for a video run:
                // report the live source first, keep the other two for the old readers.
                println!(
                    "到时间了，退出（frames={}，drawn={}，scene frames={}）",
                    wall.frames_presented(),
                    wall.drawn,
                    wall.scene.as_ref().map(|s| s.frames()).unwrap_or(0)
                );
                break;
            }
        }
    }
}

/// 读自己的 CPU 时间与常驻内存（/proc/self）
fn self_stats() -> (f64, u64) {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let cpu_s = stat
        .rsplit(')')
        .next()
        .and_then(|rest| {
            let f: Vec<&str> = rest.split_whitespace().collect();
            let utime: f64 = f.get(11)?.parse().ok()?;
            let stime: f64 = f.get(12)?.parse().ok()?;
            Some((utime + stime) / 100.0)
        })
        .unwrap_or(0.0);
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (cpu_s, pages * 4)
}

struct OutputSurface {
    name: String,
    output: wl_output::WlOutput,
    layer: LayerSurface,
    /// viewporter 的 per-surface 对象：把「物理尺寸缓冲」映射到「逻辑尺寸表面」
    viewport: Option<WpViewport>,
    /// **已经下发**的 viewport 目标尺寸。viewporter 的状态是持久的（随下一次 commit
    /// 生效），所以只在尺寸变化时下发一次 —— 这就是每帧那次多余 commit 的来源。
    vp_dest: Option<(i32, i32)>,
    width: u32,
    height: u32,
}

struct Wall {
    registry_state: RegistryState,
    output_state: OutputState,
    /// 必须持有：否则 compositor/层壳代理被销毁，已建的表面会失效
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    viewporter: Option<SimpleGlobal<wp_viewporter::WpViewporter, 1>>,
    pool: SlotPool,
    surfaces: Vec<OutputSurface>,
    image: image::RgbaImage,
    drawn: u32,

    // ── 睡眠/活跃判定（b2）─────────────────────────────────────────────
    /// 身份来源：ext_foreign_toplevel_list_v1（自己绑，版本 1..=1，同 COSMIC client-toolkit）
    /// 扩展置顶窗口列表的代理。**必须持有**：这是我们要收事件的唯一凭据，
    /// 丢了它 Wayland 对象就被销毁，`[状态]`/`[done]` 再也不会来（b2 的睡眠判定会静默失效）。
    /// 所以它"不被读"是正常的，不是死代码。
    #[allow(dead_code)]
    foreign_list: Option<wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>,
    /// 状态来源：zcosmic_toplevel_info_v1（COSMIC 自有；提供 state/geometry）
    ct_info: Option<ct::zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1>,
    /// 每个窗口一组事实，多个 Arc 共享给 wayland 的对象 user-data
    windows: Vec<(ExtForeignToplevelHandleV1, Arc<Mutex<WindowFacts>>)>,
    /// 是否应该"睡觉"（正式：有全屏或铺满整屏的窗口）
    suppressed: bool,
    /// b3：视频解码器（ffmpeg 子进程）；None = 静态图片模式
    video: Option<video::Decoder>,
    /// b3：待启动的视频路径（首次 configure 拿到屏幕尺寸后再起 ffmpeg）
    video_path: Option<String>,
    /// b3：解码尺寸上限覆盖（None = 用屏幕物理尺寸）
    video_max: Option<(u32, u32)>,
    /// s3 视频 0 拷贝：进程内 VAAPI 解码器（Rust 侧句柄；老的 `video` 是 ffmpeg 子进程那条路）
    vdec: Option<we_scene_render::video::DmaVideo>,
    /// s3 视频 0 拷贝：NV12 → wl_egl_window 的渲染器（首次拿到屏幕尺寸后建）
    video_gpu: Option<we_scene_render::video::VideoGl>,
    /// s3：视频 0 拷贝这条路开着吗（`WE_VIDEO_ZEROCOPY=0` 关掉；起不来会自己退回老路）
    video_zerocopy: bool,
    /// s3：0 拷贝连续失败次数（够多就退回 ffmpeg 子进程那条老路）
    video_gpu_errs: u32,
    /// a decoded frame whose submit is still out there. The decoder surface is
    /// being sampled until that swap returns, so its `release` is deferred to the lap that
    /// collects the submit (0-copy video used to block the main loop inside eglSwapBuffers).
    video_commit_hold: bool,
    /// s3：0 拷贝视频下一帧的时刻（老路是 ffmpeg 管道可读驱动，新路要自己按 fps 排）
    video_next: Option<std::time::Instant>,
    /// s2：要渲染的 scene 壁纸目录（自研渲染器）；None = 不走 scene
    scene_path: Option<String>,
    /// scene 的 sound 对象（配乐）；跟着 scene_path 走，Drop 里收掉子进程
    audio: Option<audio::Player>,
    /// 当前挂了音频的场景目录（同一场景换 scale 重建渲染器时不重启音乐）
    audio_for: Option<String>,
    /// **音频分析跟哪一路输出**（控制面 `{"cmd":"audio","sink":…}` 改；`default` = 跟随系统默认）。
    /// 切换 = 写 `WE_AUDIO_DEVICE` 环境变量 + 重启渲染器那条采集（见 `handle_cmd` 的 `Cmd::Audio`）。
    audio_sink: audio_sinks::SinkSel,
    /// 上一次 sink 枚举/切换的备注（枚举失败、按名字降级…）；status.audio 里原样带出来，好排查
    audio_sink_note: Option<String>,
    /// s2：渲染器（首次拿到屏幕尺寸后建）
    scene: Option<we_scene_render::SceneRenderer>,
    /// 取景模式（cover/stretch/center/contain）。它烘进 `build` 的投影矩阵 ⇒ 改完必须重建渲染器
    fit_mode: we_scene_render::FitMode,
    /// 色彩调整（亮度/对比度/饱和度/色相/色温/伽马）。同样是烘进 shader 的 ⇒ 改完要重建
    color: [f32; 6],
    /// s2：渲染器建好的时刻（粒子/动画要真实时间）
    scene_t0: Option<std::time::Instant>,
    /// s2c3：上一次成功读到的**物理**分辨率。合成器把输出摘掉再挂回后
    /// `physical_size` 会短暂为空，这时若退回逻辑尺寸重建，画面会变糊
    /// （实测 2560x1440 → 2048x1152）且白花一次构建。
    last_phys: Option<(u32, u32)>,
    /// s2c3：连续渲染失败计数（够多才丢弃渲染器重建，避免单帧抖动引发重启风暴）
    scene_errs: u32,
    /// s2c7：裸 `wl_display*`（0 拷贝模式建 EGL wayland 平台 display 要用）。
    /// 存成 usize 免得给整个 struct 加生命周期。
    dpy_ptr: usize,
    /// s2c9：运行时设置（由控制面 `set` 改；界面按档位组合这些值）
    fps: u64,
    /// 睡眠策略：never / fullscreen（默认）/ maximized
    sleep_mode: String,
    /// 渲染尺寸：physical（默认，锐）/ logical（省 GPU）
    render_scale: String,
    /// 配乐总开关（`set sound=off` / `WE_SOUND=0`）：机制就一个，策略留给界面
    sound_on: bool,
    /// **播放速度**（场景时间倍率，默认 1.0）。真正生效的是渲染器那个全局
    /// （`we_scene_render::time_scale`）；这里存一份**实际生效值**给 status/回执。
    time_scale: f32,
    /// **音乐音量**（0..100，默认 sink 的 volume）。`None` = 启动时没读出来；
    /// 由 `set volume=` 回读填上，status 里 -1 表示未知（不假装 0）。
    sink_volume: Option<f32>,
    /// 最近一次音量操作生效的 sink 节点名（status/日志用，明说打的是哪一路）
    sink_name: Option<String>,
    /// 最近一次音量操作失败的原因（人话；成功后保留上一次的值不动）
    sink_volume_note: Option<String>,
    /// 特效链开不开（省电档的最大杠杆）
    effects_on: bool,

    // ── 鼠标输入───────────────────────────────────────────────
    /// `wl_seat`（必须持有：丢了代理就没 capabilities 事件）
    seat: Option<wayland_client::protocol::wl_seat::WlSeat>,
    /// `wl_pointer`（必须持有：丢了就收不到 motion）；`Some` = 这条路已经起来
    pointer: Option<wayland_client::protocol::wl_pointer::WlPointer>,
    /// 指针当前在哪个表面上（`Enter` 里记下来，`Motion` 用它换算归一化坐标）
    cursor_surf: Option<wl_surface::WlSurface>,
    /// **当前鼠标位置**（屏幕归一化 `[0,1]`，y 从屏幕顶往下）。
    /// 两个来源写同一个字段：① 内核自己的 `wl_pointer`（空桌面上的 enter/motion）；
    /// ② 控制面 `{"cmd":"mouse","x":..,"y":..}`（也可用于离线/脚本化验证）。
    /// `None` = 还没有任何鼠标输入（渲染器那侧就不会下发 `g_PointerPosition`，
    /// 与加这一路之前逐字节一致）。
    mouse: Option<(f32, f32)>,
    /// 鼠标事件计数（status 里回报，用来一眼看出"到底有没有拿到指针"）
    mouse_events: u64,
    /// 是否给层壳表面设输入区、绑 `wl_pointer`（`WE_MOUSE_POINTER=0` 关掉 ⇒ 与旧行为一致）
    mouse_input: bool,
    /// 上一次打印鼠标位置的时刻（日志限流，别刷屏）
    mouse_log_at: Option<std::time::Instant>,
    // ── 鼠标诊断：`WE_MOUSE_DEBUG=1` 打开，**默认关**（行为零变化）────
    /// 诊断开关：把 `wl_pointer` 的每一类事件都打一条日志（enter/leave/motion/其它），
    /// 外加输入区/configure/绑定的过程日志。默认关 ⇒ 只多几个计数器，行为与旧版逐字节一致。
    mouse_debug: bool,
    /// 诊断：`wl_pointer` 收到的**原始事件总数**（enter/leave/motion/button/axis/… 全算）。
    /// 和 `mouse_events` 的区别（这是排查"到底有没有收到指针"的关键）：
    /// · `mouse_events` 只数**成功换算成场景坐标**的 enter/motion（尺寸还没 configure
    /// 出来时会丢，见 `mouse_from_surface` 的早退）；
    /// · `pointer_events` 数**合成器到底发没发**——它是"会不会来"的唯一诚实指标。
    pointer_events: u64,
    /// 诊断：分开计数，一眼看出"是只收到 enter 没有 motion"还是"一个都没有"
    mouse_enter: u64,
    mouse_leave: u64,
    mouse_motion: u64,
    // ── 全局指针：`ext_image_copy_capture` 光标会话 ──────────────────
    // 为什么需要它：`wl_pointer` 只在"指针正压在我们表面上"时才有事件；窗口盖住桌面时
    // 我们完全收不到。光标会话读的是**合成器光标**，与"压在哪块表面"无关 ⇒ 全覆盖。
    /// 捕获源管理器 / 捕获管理器（`WE_POINTER_GLOBAL=0` 或拿不到 ⇒ None ⇒ 整条路关）
    cap_src_manager: Option<cap_src_mgr::ExtOutputImageCaptureSourceManagerV1>,
    cap_manager: Option<cap_mgr::ExtImageCopyCaptureManagerV1>,
    /// 必须持有：丢了代理，合成器会销毁会话
    cap_source: Option<cap_source::ExtImageCaptureSourceV1>,
    cap_session: Option<cap_sess::ExtImageCopyCaptureSessionV1>,
    cap_cursor: Option<cap_cursor::ExtImageCopyCaptureCursorSessionV1>,
    /// 会话已建（尝试过就不再重试）
    pointer_global_ok: bool,
    /// 收到过 `done`（约束齐了，会话真在跑）
    pointer_global_ready: bool,
    /// 归一化基准 = 捕获会话报的 `buffer_size`（**输出缓冲像素**）。除以它就等于除以逻辑尺寸
    /// （buffer = logical × scale ⇒ x/buffer == (x/scale)/logical）⇒ 与 `mouse_from_surface` 同口径，可对账。
    pointer_global_base: (f32, f32),
    /// 收到过多少条全局 `position` —— status 里回报，一眼看出"全局指针到底通没通"
    pointer_global_events: u64,
    pointer_global_enter: u64,
    pointer_global_leave: u64,
    /// **对账用**：两条路各自最后的值（`wl_pointer` / 光标会话）并排回报 ——
    /// 指针经过"空桌面条带"时两条路都有效，数值必须一致（不一致就是某一路的换算错了）。
    mouse_wl: Option<(f32, f32)>,
    mouse_cs: Option<(f32, f32)>,
    /// **控制面覆盖窗口**：`{"cmd":"mouse"}` 写值后这段时间内全局指针**不写入**。
    /// 为什么需要：光标会话每秒能写 ~300 次 ⇒ 不设窗口的话，那一条命令下一帧就被真实指针盖掉，
    /// app 的"指针→画面中心"按钮与脚本化验收（`we-ctl mouse …`）会**直接失灵**。
    mouse_override_until: Option<std::time::Instant>,
    /// 覆盖窗口长度（毫秒；`WE_MOUSE_OVERRIDE_MS`，默认 2000）
    mouse_override_ms: u64,
    // ── 帧转储（`we-ctl dump`）：调试用、按需 ─────────────────────────────
    /// 转储文件名前缀：`<前缀>_0000.png`… + 旁注 `<前缀>.txt`
    dump_prefix: String,
    /// 还要转储多少帧（0 = 不转储 ⇒ 这条路径一个字节都不动）
    dump_left: u32,
    /// 已经**排队**多少帧（送到编码 worker 的计数；真正写盘在 worker 里）
    dump_n: u32,
    /// 旁注文件（worker 在 PNG 写成功后追加；主循环不再碰它）
    dump_log: Option<Arc<Mutex<std::fs::File>>>,
    /// 编码 worker + 缓冲池（见 `dump.rs`：编码移出主循环的原因）
    dump_enc: Option<dump::DumpEncoder>,

    // ── （呈现路径根治）：每帧**一次** commit + `wl_surface.frame` 回调配速 ──────
    // 为什么要有这两组字段：实测（ 的 upstream 结论）帧内容本身干净，撕裂只在**呈现**
    // 一侧 ⇒ 只能靠"数 commit 次数"和"合成器什么时候给我们下一帧"来定位/验收。
    /// 窗口内已上屏的帧数（每 100 帧打一行 commit 统计后清零）
    cm_frames: u64,
    /// 累计上屏帧数（只用于日志编号）
    cm_seen: u64,
    /// 窗口内的 commit 次数
    cm_commits: u64,
    /// 窗口内单帧最多 commit 几次（必须 = 1）
    cm_max: u64,
    /// 本帧已经 commit 了几次（每帧开头清零）
    cm_this: u64,
    /// 累计（status 里回报）
    cm_total: u64,
    /// 当前挂着的 `wl_surface.frame` 回调（有值 = 正在等合成器；等到了才画下一帧）
    frame_cb: Option<wl_callback::WlCallback>,
    /// 从什么时候开始等（超时兜底，免得合成器不回收时永远不画）
    frame_wait_since: Option<std::time::Instant>,
    /// 收到过多少次回调 / 因等回调而推迟的帧 / 超时兜底次数
    cb_done: u64,
    cb_gated: u64,
    cb_timeouts: u64,
    /// 上一帧的**合成器时间戳**（回调里的 msec 时钟）：帧间隔分布就是用它算的
    cb_last_ts: u32,
    /// 最近 128 个帧间隔（ms，合成器时钟）
    cb_gaps: [f32; 128],
    cb_gap_i: usize,
    /// `WE_FRAME_CB=0` ⇒ 关掉回调配速（回到旧的定时器驱动，用于 A/B）
    frame_cb_on: bool,
    /// **合成后取帧**（`ext_image_copy_capture` 的 output 会话，见 `ovdump.rs`）
    ov: Option<ovdump::OvCap>,
    /// 当前鼠标位置**是谁写的**：`wl_pointer`（合成器给的）/ `control`（控制面命令）。
    /// status 里回报 —— 一眼看出"真鼠标到底有没有在驱动画面"。
    mouse_src: &'static str,
    /// s2：下一帧的时刻（按 SCENE_FPS 节流；睡眠时不参与 poll 超时 ⇒ 真正 0 唤醒）
    scene_next: Option<std::time::Instant>,
    // ── 切换壁纸的过渡（淡入淡出）─────────────────────────────────────────
    /// 过渡时长（ms）。默认 400；`WE_FADE=0` / `WE_FADE_MS=0` ⇒ 关闭（老行为：预热清黑 + 硬切）。
    fade_ms: f32,
    /// 拿不到旧帧时的兜底纯色（`WE_FADE_FALLBACK=R,G,B`，默认深灰 18,18,20）。
    /// 语义是"**绝不留纯黑空档**"——宁可闪一下深灰也不要黑屏。
    fade_fallback: [u8; 3],
    /// 切换时抓下的**旧壁纸最后一帧**（图像取向 RGBA），在丢旧渲染器之前抓好、
    /// 新渲染器建好之后用它当预热帧 + 淡出源。
    fade_src: Option<(u32, u32, Vec<u8>)>,
    /// 本次过渡的 apply 起点时刻（只用于打日志：首帧/构建各花多少 ms）。
    fade_t_apply: Option<std::time::Instant>,
    /// deadline for one frame commit (ms). 0 = off (old synchronous swap).
    /// Measured : SIGSTOP the compositor for 2.5 s and one
    /// main-loop lap reports `swap 2551.5 ms` with every other segment normal, i.e. the stall
    /// is locked inside eglSwapBuffers waiting for a frame callback. With a deadline the main
    /// loop keeps polling / dispatching / answering commands instead of being restarted.
    commit_deadline_ms: u64,
    /// When the outstanding commit started (log + "too long" bookkeeping).
    commit_pending_since: Option<std::time::Instant>,
    /// How many laps the commit has been outstanding (log rate limit).
    commit_pending_laps: u64,
    /// give up after this many ms of an uncollected submit (`WE_COMMIT_MAX_PENDING_MS`,
    /// default 15 s, 0 = off). Every stall on record resolved by itself within 8.5 s, so a real
    /// recovery is never punished; a submit that is gone forever stops the picture at 15 s instead
    /// of freezing the desktop until someone restarts the unit.
    commit_lost_ms: u64,
    /// dump frames dropped because a commit was still outstanding. One
    /// rate-limited line (first + every 60th): a present that never comes back during
    /// `we-ctl dump` used to look like "the dump silently stopped". Nothing is consumed and
    /// nothing is an error (`dump_left` stays as it was).
    dump_skips: u64,
    /// b4：当前播放的图片路径（状态回报用）
    image_path: Option<String>,
    /// b4：用户显式暂停（控制面 pause/resume；与"被遮挡自动睡眠"是两件事）
    user_paused: bool,
    /// b4：控制面服务端
    server: Option<serve::Server>,
    /// b4：队列句柄（用于**懒创建**层壳表面：常驻内核在没被 apply 前不占背景层）
    qh: Option<QueueHandle<Wall>>,
    /// 调试开关：把"最大化"也算睡眠条件
    sleep_on_maximized: bool,
    /// 上一次的判定，用于只在变化时打印
    last_suppressed: bool,
    /// 诊断：事件循环转了多少圈 / 实际出过多少帧（用来抓"空转"）
    polls: u64,
    ticks: u64,
    /// 诊断：poll 里到底是哪一路在唤醒
    wl_wakes: u64,
    cmd_wakes: u64,
    vid_wakes: u64,
    /// 诊断：最近一次 poll 用的超时值 / 是否有事件（0 = 根本没等到东西）
    last_timeout_ms: i32,
    #[allow(dead_code)]
    last_ready: u32,
    poll_errno: i32,
    /// 诊断：poll 返回值的分布
    p_npos: u64,
    p_nzero: u64,
    p_nerr: u64,
    p_to0: u64,
    /// 诊断：最近 8 次 poll 的超时值（看"到底是 0 还是 33"）
    to_hist: [i32; 8],
    to_hist_i: usize,
    /// 诊断：scene_due 被问到过几次、其中几次为真
    due_asked: u64,
    due_true: u64,
}

/// 我们关心的窗口事实（由 zcosmic_toplevel_handle_v1 的事件填充）
#[derive(Default, Debug, Clone)]
struct WindowFacts {
    title: String,
    app_id: String,
    fullscreen: bool,
    maximized: bool,
    activated: bool,
    minimized: bool,
    /// 有没有哪个输出上铺满了整块屏幕（geometry 覆盖）
    covers_output: bool,
    closed: bool,
}

impl Wall {
    /// 这台显示器当前模式下的物理分辨率（如 2560x1440）。取不到就返回 None。
    fn physical_size(&self, output: &wl_output::WlOutput) -> Option<(i32, i32)> {
        let info = self.output_state.info(output)?;
        info.modes
            .iter()
            .find(|m| m.current)
            .or_else(|| info.modes.iter().find(|m| m.preferred))
            .map(|m| m.dimensions)
    }

    /// 建**全局指针会话**（`ext_image_copy_capture` 光标会话）：不抓帧就拿到指针绝对坐标。
    /// 懒建：等第一块输出出现（挑第一块做捕获源；多显示器 v1 只跟第一块，日志会说明）。
    fn ensure_global_pointer(&mut self, qh: &QueueHandle<Self>) {
        let (Some(src_mgr), Some(mgr)) = (self.cap_src_manager.clone(), self.cap_manager.clone())
        else {
            self.pointer_global_ok = true; // 没有管理器 ⇒ 认了，别再试
            return;
        };
        // ⚠ SCTK 的 `outputs` 返回**迭代器**（不是 Vec）⇒ 收一次再取第一块。
        let outs: Vec<_> = self.output_state.outputs().collect();
        let Some(output) = outs.first().cloned() else {
            return; // 输出还没来 ⇒ 下一帧再试
        };
        let n_out = outs.len();
        let name = self
            .output_state
            .info(&output)
            .and_then(|i| i.name)
            .unwrap_or_else(|| "?".into());
        if n_out > 1 {
            eprintln!("[指针] 有 {n_out} 块输出，v1 只跟第一块（{name}）：其余屏上的指针暂不覆盖");
        }
        // 光标会话需要一个 `wl_pointer` 对象 —— 它只是标识"哪个指针"，**不需要**指针压在我们表面上。
        let pointer = match self.pointer.clone() {
            Some(p) => p,
            None => match self.seat.clone() {
                Some(seat) => {
                    let p = seat.get_pointer(qh, ());
                    self.pointer = Some(p.clone());
                    p
                }
                None => {
                    self.pointer_global_ok = true;
                    return;
                }
            },
        };
        let source = src_mgr.create_source(&output, qh, ());
        let session = mgr.create_session(&source, cap_mgr::Options::empty(), qh, ());
        let cursor = mgr.create_pointer_cursor_session(&source, &pointer, qh, ());
        self.cap_source = Some(source);
        self.cap_session = Some(session);
        self.cap_cursor = Some(cursor);
        self.pointer_global_ok = true;
        eprintln!("[指针] 全局指针会话已建（输出 {name}）：不 attach 缓冲、不抓帧，只读光标 position");
    }

    /// 把 `wl_pointer` 的**表面局部坐标**换算成场景用的鼠标位（`[0,1]`，y 从屏幕顶往下）。
    /// 为什么直接除以 configure 出来的表面尺寸：指针事件的坐标单位就是表面局部坐标，
    /// 而我们的层壳表面锚满四边（尺寸 = 那台输出的逻辑尺寸）⇒ 除法就是归一化。
    /// 尺寸还未知（首次 configure 之前）时**直接丢弃**：否则会拿 0 做除数、或把位置记成 0,0
    /// （那会让透视区跳到左上角）。
    fn mouse_from_surface(&mut self, surf: &wl_surface::WlSurface, x: f64, y: f64) {
        let (w, h) = self
            .surfaces
            .iter()
            .find(|s| s.layer.wl_surface() == surf)
            .map(|s| (s.width, s.height))
            .unwrap_or((0, 0));
        if w == 0 || h == 0 {
            // 诊断：尺寸未知时**故意丢弃**（否则会拿 0 做除数、把位置记成 0,0 ⇒ 透视区跳左上角）。
            // 这条日志是排查"收到了事件但鼠标没动"的最后一环，默认关。
            if self.mouse_debug {
                eprintln!(
                    "[鼠标诊断] 丢弃：表面还没 configure 出尺寸（{w}x{h}，raw #{}）",
                    self.pointer_events
                );
            }
            return;
        }
        let nx = (x as f32 / w as f32).clamp(0.0, 1.0);
        let ny = (y as f32 / h as f32).clamp(0.0, 1.0);
        self.mouse_events += 1;
        // 这两个来源写同一个字段，谁最后写谁生效 —— status.mouse_src 如实回报
        self.mouse_src = "wl_pointer";
        self.mouse_wl = Some((nx, ny)); // 对账用（另一条路写 mouse_cs）
        // 日志限流：指针每秒能来几百个 motion，无条件打印会把 journal 刷爆
        // （踩过：每帧一条的日志把有用信息全埋了）。第一条 + 每秒最多一条。
        let now = std::time::Instant::now();
        let loud = self
            .mouse_log_at
            .map(|t| now.duration_since(t).as_secs_f32() > 1.0)
            .unwrap_or(true);
        if loud {
            self.mouse_log_at = Some(now);
            eprintln!(
                "[鼠标] 指针 {nx:.4},{ny:.4}（表面 {w}x{h}，累计事件 {}）",
                self.mouse_events
            );
        }
        self.mouse = Some((nx, ny));
    }

    /// 全局指针坐标归一化：`position` 是**输出缓冲像素**（含 transform），除以捕获会话报的
/// `buffer_size` 得 `[0,1]`。因为 `buffer = logical × scale` ⇒ `x/buffer == (x/scale)/logical`，
/// 与 `mouse_from_surface`（用逻辑尺寸归一化）**同口径** ⇒ 两条路的数值可以直接对账。
/// 基准还没拿到（0x0）时返回屏幕中心（= WE/OWE "指针总有位置"的语义）。
fn normalize_global(x: f32, y: f32, base: (f32, f32)) -> (f32, f32) {
    let (bw, bh) = base;
    if !(bw > 0.0) || !(bh > 0.0) {
        return (0.5, 0.5);
    }
    ((x / bw).clamp(0.0, 1.0), (y / bh).clamp(0.0, 1.0))
}

/// 每台显示器开一个 Background 层壳表面，锚满四边（=整块输出）
    fn create_surfaces(&mut self, qh: &QueueHandle<Self>) {
        let outputs = self.output_state.outputs();
        for output in outputs {
            // 已经有表面的输出不重复开（热插拔/输出重建时这个方法会被再调一次）
            if self.surfaces.iter().any(|s| s.output.id() == output.id()) {
                continue;
            }
            let name = self
                .output_state
                .info(&output)
                .and_then(|i| i.name)
                .unwrap_or_else(|| "?".into());
            let surface = self.compositor.create_surface(qh);
            let layer = self.layer_shell.create_layer_surface(
                qh,
                surface,
                Layer::Background,
                Some(NAMESPACE),
                Some(&output),
            );
            layer.set_anchor(Anchor::all());
            layer.set_exclusive_zone(-1); // 背景层不占位
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.set_size(0, 0); // 0 + 锚满四边 ⇒ 铺满整块输出
            layer.commit();
            // 1.0 倍整数缩放的表面才有 buffer_scale>1；分数缩放（1.25）靠 viewport 表达
            layer.wl_surface().set_buffer_scale(1);
            // ── 鼠标输入区────────────────────────────────────────
            // 层壳表面**默认没有输入区** ⇒ 指针事件全归窗口/桌面（这正是"壁纸不抢输入"的前提）。
            // 设一个铺满输出的输入区后：**指针在空桌面上时**（没有窗口盖着）合成器会把
            // enter/motion 发给我们 ⇒ 归一化后喂 `g_PointerPosition`（鼠标驱动的特效/视差）。
            // ⚠ 边界（必须说清，这是这一路唯一的代价）：命中的**点击**也会落到这张表面上（我们只读
            // motion，其余一概忽略）；指针在任何窗口之上时仍归那个窗口 ⇒ 不抢焦点、不挡操作。
            // WE 在 Windows 上用的是桌面 WorkerW 窗口，形态与此一致。
            // `WE_MOUSE_POINTER=0` ⇒ 不设输入区、行为与旧版逐字节一致。
            if self.mouse_input {
                match Region::new(&self.compositor) {
                    Ok(r) => {
                        r.add(0, 0, i32::MAX, i32::MAX);
                        layer.wl_surface().set_input_region(Some(r.wl_region()));
                        // 输入区随下一次 commit 生效；这里立刻补一次（draw 里还会再 commit）
                        layer.commit();
                        if self.mouse_debug {
                            eprintln!(
                                "[鼠标诊断] 输入区已设：表面={:?} 全输出 0,0..i32::MAX（层=Background，锚满四边）",
                                layer.wl_surface().id()
                            );
                        }
                    }
                    Err(e) => eprintln!("[鼠标] 输入区创建失败（{e}）：拿不到指针事件"),
                }
            } else if self.mouse_debug {
                eprintln!("[鼠标诊断] 跳过输入区（mouse_input=false ⇒ WE_MOUSE_POINTER=0）");
            }
            let viewport = self
                .viewporter
                .as_ref()
                .map(|v| v.get().unwrap().get_viewport(layer.wl_surface(), qh, ()));
            println!("  已请求层壳表面：{name}（Background，锚满四边）");
            self.surfaces.push(OutputSurface {
                name,
                output,
                layer,
                viewport,
                vp_dest: None,
                width: 0,
                height: 0,
            });
        }
    }

    /// 把图片按 cover 画到某个表面的缓冲，attach + commit。
    /// 如果拿得到物理分辨率，就按物理尺寸渲染、再用 viewport 指定逻辑尺寸（分数缩放下更锐利）。
    fn draw(&mut self, index: usize) {
        let Some(surf) = self.surfaces.get(index) else {
            return;
        };
        if surf.width == 0 || surf.height == 0 {
            eprintln!("[跳过] {} 尺寸还是 0", surf.name);
            return;
        }
        let (lw, lh) = (surf.width, surf.height);
        // b3/s3：视频模式 —— 首次 configure 拿到屏幕尺寸后起解码器（老路 = ffmpeg 子进程；
        // 0 拷贝那条路的解码器和 EGL 窗口在 tick_video_gpu 里懒建，同样等这个尺寸）。
        if self.video_path.is_some() {
            if !self.zerocopy_video() {
                self.spawn_shm_decoder(lw, lh);
            }
            return;
        }
        // s2：scene 模式 —— 同样等拿到屏幕尺寸后再建渲染器，之后每帧由 tick_scene 提交
        if self.scene_path.is_some() {
            return;
        }
        let physical = self.physical_size(&surf.output);
        let (bw, bh) = match physical {
            Some((pw, ph)) if pw > 0 && ph > 0 => (pw as u32, ph as u32),
            _ => (lw, lh),
        };
        let name = surf.name.clone();
        let wl_surface = surf.layer.wl_surface().clone();
        let iw = bw as i32;
        let ih = bh as i32;
        let stride = iw * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(iw, ih, stride, wl_shm::Format::Argb8888)
            .expect("create buffer");
        let t0 = std::time::Instant::now();
        blit_cover(&self.image, canvas, bw, bh);
        let ms = t0.elapsed().as_millis();

        wl_surface.damage_buffer(0, 0, iw, ih);
        buffer.attach_to(&wl_surface).expect("attach 失败");
        // 物理缓冲 → 逻辑尺寸：这一步让 1.25 倍缩放变成"下采样"而不是"放大"
        let mut routed = false;
        if let Some(surf) = self.surfaces.get(index) {
            if let Some(vp) = &surf.viewport {
                vp.set_destination(lw as i32, lh as i32);
                routed = true;
            }
        }
        if let Some(surf) = self.surfaces.get(index) {
            surf.layer.commit();
        }
        self.drawn += 1;
        println!(
            "  已上屏：{name}  逻辑 {lw}x{lh}  渲染缓冲 {bw}x{bh}  分数缩放 {}  绘制 {ms} ms",
            if routed {
                format!("{:.3}", bw as f64 / lw as f64)
            } else {
                "1.000（无 viewport）".into()
            }
        );
    }
}

impl Wall {
    /// 现在该不该动（既没被遮挡睡，也没被用户暂停）
    fn is_active(&self) -> bool {
        !self.suppressed && !self.user_paused
    }

    /// 把解码器的 SIGSTOP/SIGCONT 状态同步到"睡眠 ∪ 用户暂停"
    fn sync_decoder(&mut self) {
        let want_pause = !self.is_active();
        if let Some(d) = self.video.as_mut() {
            if want_pause {
                d.pause();
            } else {
                d.resume();
            }
        }
        // 音频跟随同一个判据：睡/暂停 ⇒ 静音（收掉播放器进程，不是调音量）
        if let Some(a) = self.audio.as_mut() {
            a.set_playing(!want_pause && self.sound_on);
        }
    }

    /// 重画所有表面（换壁纸后）
    fn redraw_all(&mut self) {
        for i in 0..self.surfaces.len() {
            self.draw(i);
        }
    }

    /// Shared body of `LayerShellHandler::closed` (and of the `--debug-drop-surface-ms` probe, so
    /// the recovery path can be exercised without unplugging a display).
    /// Order matters: the renderers go first, because `wl_egl_window` is bound to the wl_surface we
    /// are about to drop; and if a submit is still outstanding neither can be dropped at all
    /// (round 13: the `we-commit` thread is inside `eglSwapBuffers` on exactly that graph), so the
    /// only safe recovery is a fresh process -> exit 76 and let systemd restart us.
    fn surface_closed(&mut self, index: usize, qh: &QueueHandle<Self>) {
        let name = self
            .surfaces
            .get(index)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "?".to_string());
        let in_flight = self.scene.as_ref().map(|s| s.commit_pending()).unwrap_or(false)
            || self
                .video_gpu
                .as_ref()
                .map(|g| g.commit_pending())
                .unwrap_or(false);
        match commit_guard::surface_gone_action(in_flight) {
            commit_guard::SurfaceGone::ExitForRestart => {
                println!(
                    "[层壳] {name} 的表面被合成器关闭，但交帧还挂着（已挂 {} ms）⇒ 不能就地丢渲染器（会踩阻塞线程）：退出 {} 让 systemd 重建表面",
                    self.commit_pending_since
                        .map(|t| t.elapsed().as_millis())
                        .unwrap_or(0),
                    commit_guard::EXIT_CODE_SUBMIT_LOST
                );
                commit_guard::exit_now(commit_guard::EXIT_CODE_SUBMIT_LOST);
            }
            commit_guard::SurfaceGone::Recreate => {
                self.scene = None;
                self.scene_t0 = None;
                self.video_gpu = None;
                self.fade_t_apply = Some(std::time::Instant::now());
                let s = self.surfaces.remove(index);
                println!(
                    "[层壳] {name} 的表面被合成器关闭：已摘除并重建（剩 {} 个表面）{}",
                    self.surfaces.len(),
                    if s.width > 0 { "" } else { "（原表面还没 configure）" }
                );
                self.create_surfaces(qh);
            }
        }
    }

    /// 懒创建层壳表面：常驻内核在第一次 apply 之前不该占背景层（否则和 waywallen 抢同一层）
    fn ensure_surfaces(&mut self) {
        if !self.surfaces.is_empty() {
            return;
        }
        if let Some(qh) = self.qh.clone() {
            self.create_surfaces(&qh);
        } else {
            eprintln!("[警告] 还没有队列句柄，建不了表面");
        }
    }

    /// 重建渲染器时的统一入口（改 scale / fit / 色彩 / 属性都会走到）。
    /// **先抓旧帧再丢渲染器**，这样"重建"期间屏幕上是旧画面（干净地过渡过去），
    /// 而不是闪一下预热用的兜底纯色。
    fn rebuild_scene(&mut self) {
        if self.fade_ms > 0.0 {
            if let Some(s) = self.scene.as_mut() {
                if let Ok((w, h, px)) = s.capture_frame() {
                    self.fade_src = Some((w, h, px));
                }
            }
        }
        self.fade_t_apply = Some(std::time::Instant::now());
        self.scene = None;
        self.scene_t0 = None;
    }

    /// **app 存档恢复**：把「当前这一个场景」的属性覆盖表与帧率写回运行时。
    /// 为什么必须做（这条坑反复踩过）：属性覆盖表（`set-prop`）与 fps（`{"cmd":"set","fps":N}`）
    /// 都只是**内核内存里的状态**，进程一重启就全没了 ⇒ 桌面退回**作者默认属性** + drop-in 的
    /// 默认帧率。为了上屏渲染器修复，内核被反复重装/重启，于是用户看到的就是错的
    /// （实测：X-Ray 场景 `xrayversion` 回到 `false` ⇒ "X-Ray 消失了"）。
    /// 界面（shell）自己有一份事实存档（`~/.local/share/wallpaper-hub/*.json`），但它不一定在跑、
    /// 也不一定在重启那一刻补发 ⇒ **内核自己**按同一份存档恢复一次（**只读存档**，一个字都不写）。
    /// 调用点只有两个，都是"一次性"的：① 内核启动时；② 每次**成功 apply 一个 scene** 之后。
    /// **没有定时器、没有轮询** —— app 随时可以重发（它优先）：它发晚了就覆盖我们恢复的值，
    /// 而值来自同一份存档 ⇒ 不会来回抖。
    /// ⚠ 必须在**渲染器建起来之前**调用：覆盖表是 `build` 时逐对象/逐特效解析显隐的
    /// （`Visibility::resolve`）。apply 路径上渲染器在 `tick_scene` 里才建 ⇒ 放在 `apply_source`
    /// 里天然早于它。
    /// ⚠ 开关 `WE_RESTORE_APP_STATE=0` ⇒ 这里**立刻返回**：连存档都不读、日志也不打、
    /// 渲染器与 fps 一个字都不动 ⇒ 与旧内核逐字节一致。
    fn restore_app_state(&mut self, why: &str) {
        // 开关关 ⇒ 连存档都不读（`decide_in` 的早退）、日志不打、一个字都不动
        let enabled = app_state::restore_enabled();
        if !enabled {
            return;
        }
        let Some(path) = self.scene_path.clone() else {
            return; // 当前没有场景（图片/视频/还没 apply）⇒ 没什么可恢复的，也不打日志
        };
        let Some(id) = app_state::scene_id_from_path(&path) else {
            eprintln!("[启动恢复] 场景 id 推不出来（path={path}）⇒ 跳过 · 触发={why}");
            return;
        };
        let dec = app_state::decide_here(enabled, &id);
        if let Some(line) = dec.log_line(why) {
            // 一行证据：证明存档真的被读了、恢复了几条、fps 是多少、值是哪来的（存档/缺省）
            println!("{line}");
        }
        // 决策 = 一串**纯数据**动作（`app_state::Action`）⇒ 这里只负责落地，不做判断。
        // 没有动作（开关关/存档不可用/没有当前场景）⇒ 什么都不动、什么都不打。
        let actions = dec.actions();
        if actions.is_empty() {
            return;
        }
        // 渲染器那一半：覆盖表（清表 + 逐条写）。改过就重建 —— 规矩同 `set-prop`
        // （覆盖表是建场景时解析显隐的）。apply 路径上渲染器还没建 ⇒ 这次重建是白送的安全网。
        if apply_actions_to_renderer(&actions) {
            self.rebuild_scene();
        }
        // 内核自己那一半：fps 是 `Wall` 的字段（不是渲染器的），改完立刻按新节奏排下一帧
        for a in &actions {
            if let app_state::Action::SetFps(f) = a {
                self.fps = *f;
                self.scene_next = None;
            }
        }
    }

    /// 换源（控制面 apply）：图片=读进来画一次；视频=换 video_path，由 draw 去起解码器
    fn apply_source(&mut self, kind: &str, path: &str) -> Result<(), String> {
        if !std::path::Path::new(path).exists() {
            return Err(format!("路径不存在：{path}"));
        }
        self.ensure_surfaces();
        // 换任何源：视频的 0 拷贝状态（进程内解码器 + EGL 窗口）都要丢掉重建
        self.vdec = None;
        self.video_gpu = None;
        self.video_next = None;
        self.video_gpu_errs = 0;
        self.video_commit_hold = false;
        if kind == "scene" {
            self.video = None; // Drop 里 kill 掉旧 ffmpeg，不留孤儿
            self.video_path = None;
            // 切换过渡（第一半）：在**丢掉旧渲染器之前**把它最后一帧抓下来。
            // 旧渲染器一丢，合成器手上那段画面就成了"最后一份"；老内核在新渲染器
            // 建 EGL 窗口时会清一次黑并 swap ⇒ 用户看到的黑屏就发生在这里。
            self.fade_src = None;
            self.fade_t_apply = Some(std::time::Instant::now());
            let fms = self.fade_ms;
            let fb = self.fade_fallback;
            if fms > 0.0 {
                if let Some(s) = self.scene.as_mut() {
                    let c0 = std::time::Instant::now();
                    match s.capture_frame() {
                        Ok((w, h, px)) => {
                            println!(
                                "[过渡] 旧帧已抓下 {w}x{h}（{} ms）⇒ 过渡 {fms:.0} ms",
                                c0.elapsed().as_millis()
                            );
                            self.fade_src = Some((w, h, px));
                        }
                        Err(e) => eprintln!("[过渡] 旧帧抓取失败（改用兜底纯色 {fb:?}）：{e}"),
                    }
                } else {
                    println!("[过渡] 没有旧场景 ⇒ 预热/淡出用兜底纯色 {fb:?}（绝不留纯黑）");
                }
            }
            self.scene = None; // 旧渲染器随 GL 上下文一起丢
            self.scene_next = None; // None ⇒ 下一轮事件循环立刻出首帧
            self.scene_path = Some(path.to_string());
            self.image_path = None;
            self.image = image::RgbaImage::new(1, 1);
            self.drawn = 0;
            self.redraw_all();
            // app 存档恢复（**每次成功 apply 一个 scene 就做一次**，不轮询）：把这条场景的
            // 属性覆盖表与帧率写回运行时 —— 内核重启/重装之后桌面上照样是用户要的那套属性。
            // 放在这里（而不是 tick_scene）是因为覆盖表必须在渲染器 `build` **之前**进表。
            self.restore_app_state(&format!("apply scene {path}"));
            return Ok(());
        }
        // 换到非 scene 源时把 scene 状态清干净
        self.scene = None;
        self.scene_next = None;
        self.scene_path = None;
        // 配乐跟着场景走：换到非场景源（图片/视频）立刻收掉，不然音乐会在别的壁纸上继续放
        self.audio = None;
        self.audio_for = None;
        if kind == "video" {
            self.video = None; // Drop 里 kill 掉旧 ffmpeg，不留孤儿
            self.video_path = Some(path.to_string());
            self.image_path = None;
            self.image = image::RgbaImage::new(1, 1);
        } else {
            let img = image::open(path).map_err(|e| format!("读不了图片：{e}"))?;
            self.video = None;
            self.video_path = None;
            self.image = img.to_rgba8();
            self.image_path = Some(path.to_string());
        }
        self.drawn = 0;
        self.redraw_all();
        self.sync_decoder();
        Ok(())
    }

    /// 状态（控制面 status；也给 GUI 用）
    fn status_json(&self) -> String {
        let (cpu_s, rss_kb) = self_stats();
        let kind = if self.video_path.is_some() {
            "video"
        } else if self.scene_path.is_some() {
            "scene"
        } else if self.image_path.is_some() {
            "image"
        } else {
            "none"
        };
        let path = self
            .video_path
            .clone()
            .or_else(|| self.scene_path.clone())
            .or_else(|| self.image_path.clone())
            .unwrap_or_default();
        let frames = self
            .video
            .as_ref()
            .map(|d| d.frames())
            .or_else(|| self.video_gpu.as_ref().map(|g| g.frames()))
            .or_else(|| self.vdec.as_ref().map(|d| d.frames))
            .or_else(|| self.scene.as_ref().map(|s| s.frames()))
            .unwrap_or(0);
        let (dw, dh) = match (&self.video, &self.vdec, &self.scene) {
            (Some(d), _, _) => (d.width, d.height),
            // 0 拷贝视频：报**源**尺寸（不是渲染缓冲尺寸 —— 那是对外投递的尺寸，报出去会误导）
            (None, Some(d), _) => d.source_size(),
            (None, None, Some(s)) => s.size(),
            _ => (0, 0),
        };
        let mouse_str = match self.mouse {
            Some((x, y)) => format!("{x:.4},{y:.4}"),
            None => String::new(),
        };
        format!(
            "{{\"ok\":true,\"kind\":\"{kind}\",\"path\":\"{}\",\"playing\":{},\"suppressed\":{},\"user_paused\":{},\"frames\":{},\"decode_w\":{dw},\"decode_h\":{dh},\"outputs\":{},\"cpu_s\":{cpu_s:.2},\"rss_mb\":{:.1},\"fps\":{},\"scene_time\":{:.3},\"sleep\":\"{}\",\"scale\":\"{}\",\"effects\":\"{}\",\"time_scale\":{},\"volume\":{},\"volume_sink\":\"{}\",\"polls\":{},\"ticks\":{},\"wl_wakes\":{},\"cmd_wakes\":{},\"last_to\":{},\"poll_err\":{},\"p_npos\":{},\"p_nzero\":{},\"p_nerr\":{},\"p_to0\":{},\"to_hist\":{:?},\"due_asked\":{},\"due_true\":{},\"audio_tracks\":{},\"audio_playing\":{},\"mouse\":\"{}\",\"mouse_events\":{},\"mouse_src\":\"{}\",\"pointer_events\":{},\"mouse_enter\":{},\"mouse_leave\":{},\"mouse_motion\":{},\"mouse_input\":{},\"pointer_bound\":{},\"pointer_global_ok\":{},\"pointer_global_ready\":{},\"pointer_global_events\":{},\"pointer_global_enter\":{},\"mouse_wl\":\"{}\",\"mouse_cs\":\"{}\",\"audio\":\"{}\",\"audio_sink\":\"{}\",\"cm_total\":{},\"cm_seen\":{},\"cb_done\":{},\"cb_gated\":{},\"cb_timeouts\":{},\"cb_gap_p50\":{:.1},\"cb_gap_p95\":{:.1},\"frame_cb\":{},\"ovcap\":\"{}\",\"commit_pending_ms\":{}}}",
            serve::jesc(&path),
            self.video_path.is_some() || self.scene_path.is_some(),
            self.suppressed,
            self.user_paused,
            frames,
            self.surfaces.len(),
            rss_kb as f64 / 1024.0,
            // s3：把当前四项设置也回给控制面 —— 界面据此显示"内核当前值"，
            // 也让 `we-ctl status` 不用再发一条空 `set` 就能看到档位是否生效。
            self.fps,
            // 场景时刻（只给"播放速度是否真的生效"取证；time_scale=1 时就是墙钟秒）
            self.scene.as_ref().map(|s| s.scene_time()).unwrap_or(0.0),
            serve::jesc(&self.sleep_mode),
            serve::jesc(&self.render_scale),
            if self.effects_on { "all" } else { "none" },
            // 播放速度 / 音乐音量（新增的两个调节项）：status 里**直接可见**
            self.time_scale,
            self.sink_volume.unwrap_or(-1.0),
            serve::jesc(self.sink_name.as_deref().unwrap_or("")),
            self.polls,
            self.ticks,
            self.wl_wakes,
            self.cmd_wakes,
            self.last_timeout_ms,
            self.poll_errno,
            self.p_npos,
            self.p_nzero,
            self.p_nerr,
            self.p_to0,
            self.to_hist,
            self.due_asked,
            self.due_true,
            self.audio.as_ref().map(|a| a.track_count()).unwrap_or(0),
            self.audio.as_ref().map(|a| a.is_playing()).unwrap_or(false),
            mouse_str,
            self.mouse_events,
            // 本次新增的鼠标诊断字段（来源 / 原始事件数 / 分项计数 / 输入区与绑定状态）
            self.mouse_src,
            self.pointer_events,
            self.mouse_enter,
            self.mouse_leave,
            self.mouse_motion,
            self.mouse_input,
            self.pointer.is_some(),
            // 全局指针（光标会话）：是否建起 / 是否就绪 / 收到多少条 / enter 计数
            self.pointer_global_ok,
            self.pointer_global_ready,
            self.pointer_global_events,
            self.pointer_global_enter,
            // 对账：两条路各自最后的值（空桌面条带上必须一致）
            self.mouse_wl.map(|(x, y)| format!("{x:.4},{y:.4}")).unwrap_or_default(),
            self.mouse_cs.map(|(x, y)| format!("{x:.4},{y:.4}")).unwrap_or_default(),
            serve::jesc(&self.audio_status_string()),
            serve::jesc(&self.audio_sink.label()),
            // 呈现侧取证（commit 次数 / frame 回调间隔 / 合成后取帧状态）
            self.cm_total,
            self.cm_seen,
            self.cb_done,
            self.cb_gated,
            self.cb_timeouts,
            self.cb_stats().0,
            self.cb_stats().1,
            self.frame_cb_on,
            serve::jesc(&self.ov.as_ref().map(|o| o.status()).unwrap_or_default()),
            // how long a submit has been outstanding (0 = nothing in flight). This is
            // the number the "never comes back" detector fires on, so `we-ctl status` shows it.
            self.commit_pending_since
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0)
        )
    }

    /// `status.audio` 的那一行：渲染器报的采集状态 + **当前跟哪个 sink**（本次新增）。
    /// 设备名是渲染器那条采集线程的**实际** `--device`（不会因为选了个错名字就装成对的）。
    fn audio_status_string(&self) -> String {
        format!(
            "{} {}",
            we_scene_render::audio::capture_status(),
            audio_sinks::status_line(&self.audio_sink, self.audio_sink_note.as_deref())
        )
    }

    /// **切换音频分析的输出源**（`{"cmd":"audio","sink":…}`）。返回切换到的那一路标签。
    /// 机制（**只动内核这一侧**，渲染器的采集线程一行不改）：
    /// 渲染器起采集时读 `WE_AUDIO_DEVICE` 环境变量当 `parec --device`（空 ⇒ 现查 `pactl` 默认）。
    /// 所以这里 ① 写环境变量；② 若采集正在跑（功能开 / 非静态 / 源=monitor）就**重启**那条采集：
    /// `stop_capture`（SIGTERM 掉 parec）→ 等旧线程真正退出 → `start_capture(Monitor)`。
    /// ⚠ `stop_capture` 之后必须**等一拍**再 `start_capture`：渲染器的采集线程是先 `read` 到 0
    /// 再检查退出标志的，若立刻把标志清掉，旧线程会"复活"并拿**旧设备**重连 ⇒ 两路 parec
    /// 同时喂同一份频谱（`analyzed` 翻倍、还串了另一路的声音）。400ms 是保险值（实测旧线程 <50ms 退出）。
    fn switch_audio_sink(&mut self, spec: &str) -> Result<String, String> {
        // 枚举失败 ⇒ 诚实降级（plan_sink 决定：default 照用 / id 拒绝 / 名字直接用）
        let (list, enum_err) = match audio_sinks::enumerate() {
            Ok(l) => (l, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let sel = audio_sinks::plan_sink(spec, &list, enum_err.as_deref())?;
        let changed = sel != self.audio_sink;
        self.audio_sink = sel.clone();
        self.audio_sink_note = enum_err;
        // ① 写采集设备（空串 = 让渲染器每次现查系统默认 ⇒ "跟随默认"是真跟随）
        std::env::set_var("WE_AUDIO_DEVICE", sel.device_env());
        // ② 采集在跑且真的换了源 ⇒ 重启采集（没换就别白重启一遍，省一次音频断流）
        let live = we_scene_render::audio::enabled()
            && !we_scene_render::audio::is_static()
            && we_scene_render::audio::selected_source() == we_scene_render::audio::Source::Monitor;
        if changed && live {
            we_scene_render::audio::stop_capture();
            std::thread::sleep(std::time::Duration::from_millis(400));
            we_scene_render::audio::start_capture(we_scene_render::audio::Source::Monitor)?;
            println!(
                "[控制面] 采集源已切到 {}（{}）",
                sel.label(),
                if sel.device_env().is_empty() {
                    "系统默认".to_string()
                } else {
                    sel.device_env()
                }
            );
        }
        Ok(sel.label())
    }

    /// `{"cmd":"audio","sinks":true}` ⇒ 只列可用 sink。枚举失败**如实报错**（不假装空列表）。
    fn reply_audio_sinks(&mut self, mut conn: std::os::unix::net::UnixStream) -> bool {
        let reply = match audio_sinks::enumerate() {
            Ok(list) => {
                let default = audio_sinks::default_sink().unwrap_or_default();
                let items: Vec<String> = list
                    .iter()
                    .map(|s| {
                        format!(
                            "{{\"id\":{},\"name\":\"{}\",\"desc\":\"{}\",\"monitor\":\"{}\",\"default\":{}}}",
                            s.id,
                            serve::jesc(&s.name),
                            serve::jesc(&s.desc),
                            serve::jesc(&s.monitor),
                            s.is_default
                        )
                    })
                    .collect();
                println!(
                    "[控制面] audio sinks ⇒ {} 个可用（默认 {}；当前跟 {}）",
                    list.len(),
                    if default.is_empty() { "(未知)" } else { &default },
                    self.audio_sink.label()
                );
                format!(
                    "{{\"ok\":true,\"sinks\":[{}],\"default\":\"{}\",\"selected\":\"{}\",\"note\":\"{}\"}}",
                    items.join(","),
                    serve::jesc(&default),
                    serve::jesc(&self.audio_sink.label()),
                    serve::jesc(self.audio_sink_note.as_deref().unwrap_or(""))
                )
            }
            Err(e) => {
                println!("[控制面] audio sinks ⇒ 无法枚举：{e}");
                format!("{{\"ok\":false,\"error\":\"{}\"}}", serve::jesc(&e))
            }
        };
        let _ = writeln!(conn, "{reply}");
        let _ = conn.flush();
        false
    }

    /// 处理一条控制面命令；返回 true 表示请求退出
    fn handle_cmd(&mut self, cmd: serve::Cmd, mut conn: std::os::unix::net::UnixStream) -> bool {
        let (reply, quit) = match cmd {
            serve::Cmd::Status => (self.status_json(), false),
            serve::Cmd::Apply { kind, path } => match self.apply_source(&kind, &path) {
                Ok(()) => (
                    {
                        // 记下来，供持久单元下次开机自动恢复（见 last_applied_file）
                        save_last_applied(&kind, &path);
                        format!(
                            "{{\"ok\":true,\"applied\":\"{}\",\"kind\":\"{}\"}}",
                            serve::jesc(&path),
                            serve::jesc(&kind)
                        )
                    },
                    false,
                ),
                Err(e) => (
                    format!("{{\"ok\":false,\"error\":\"{}\"}}", serve::jesc(&e)),
                    false,
                ),
            },
            serve::Cmd::Pause => {
                self.user_paused = true;
                self.sync_decoder();
                (r#"{"ok":true,"user_paused":true}"#.to_string(), false)
            }
            serve::Cmd::Resume => {
                self.user_paused = false;
                self.sync_decoder();
                (r#"{"ok":true,"user_paused":false}"#.to_string(), false)
            }
            serve::Cmd::Quit => (r#"{"ok":true,"bye":true}"#.to_string(), true),
            serve::Cmd::Set {
                fps,
                sleep,
                scale,
                effects,
                sound,
                fit,
                color,
                time_scale,
                volume,
            } => {
                // 运行时改设置：只改字段（机制），档位怎么组合由界面决定（策略）
                if let Some(v) = fps {
                    self.fps = v.clamp(1, 240);
                    self.scene_next = None; // 立刻按新节奏排下一帧
                }
                if let Some(v) = sleep {
                    if v == "never" || v == "fullscreen" || v == "maximized" {
                        self.sleep_mode = v;
                    }
                }
                if let Some(v) = scale {
                    if (v == "physical" || v == "logical") && v != self.render_scale {
                        // 换渲染尺寸必须重建渲染器（wl_egl_window 的尺寸是建的时候定的）
                        self.render_scale = v;
                        self.rebuild_scene();
                    }
                }
                if let Some(m) = fit.as_deref().and_then(we_scene_render::FitMode::parse) {
                    if m != self.fit_mode {
                        // 取景模式是 build 时烘进每一趟的投影矩阵的 ⇒ 必须重建渲染器
                        self.fit_mode = m;
                        we_scene_render::set_global_fit(m);
                        self.rebuild_scene();
                    }
                }
                if let Some(v) = color {
                    // 六个逗号分隔的浮点。解析不成/不是 6 个 ⇒ 当没写。
                    let v: Vec<f32> = v.split(',').filter_map(|t| t.trim().parse().ok()).collect();
                    if v.len() == 6 {
                        let c = [v[0], v[1], v[2], v[3], v[4], v[5]];
                        if c != self.color {
                            // 调色是烘进"最后上屏"那条 shader 的 ⇒ 也得重建（只重做一个全屏层）
                            self.color = c;
                            we_scene_render::set_global_color(c);
                            self.rebuild_scene();
                        }
                    }
                }
                if let Some(v) = time_scale {
                    // **播放速度**：只改渲染器的场景时间倍率 —— 不重建任何东西
                    // （`g_Time`/粒子 dt/网格动画都在每帧现算，见 renderer 的 `scale_clock`）。
                    // 渲染器自己再夹一次范围，这里存的是**实际生效值**。
                    let eff = we_scene_render::set_time_scale(v);
                    self.time_scale = eff;
                    println!("[控制面] time_scale = {eff:.3}×（只乘场景时间：g_Time/粒子/网格动画；音频与鼠标不受影响）");
                }
                if let Some(v) = volume {
                    // **音乐音量**：调默认 sink 的 volume（壁纸自己的 sound 就播在它上面）。
                    // 同步调用 pactl（几毫秒），回读实际值 —— 回执与 status 都报**实际**百分比。
                    match audio_sinks::set_default_volume(v) {
                        Ok((sink, actual)) => {
                            self.sink_volume = Some(actual);
                            self.sink_name = Some(sink.clone());
                            println!(
                                "[控制面] volume 请求 {v:.1}% ⇒ sink {sink} 实际 {actual:.0}%（pactl 量化后回读）"
                            );
                        }
                        Err(e) => {
                            println!("[控制面] volume {v:.1}% 失败：{e}");
                            self.sink_volume_note = Some(e);
                        }
                    }
                }
                if let Some(v) = sound {
                    let on = v != "off";
                    if on != self.sound_on {
                        self.sound_on = on;
                    }
                }
                if let Some(v) = effects {
                    let on = v != "none";
                    if on != self.effects_on {
                        self.effects_on = on;
                        match self.scene.as_mut() {
                            Some(s) => s.set_effects(on),
                            None => {}
                        }
                    }
                }
                // 策略改了要**立刻**重算一次该不该睡：不然要等下一个窗口事件才生效
                // （实测踩过：桌面明明有最大化窗口，`set sleep=maximized` 之后帧数照涨 ——
                // 因为窗口事实没变，recompute_suppression 根本没被调到）
                self.recompute_suppression();
                // 再显式把状态推给子进程（视频 SIGSTOP/SIGCONT、音频起停）——
                // recompute 只在**状态变化**时才同步，`set sound=off` 这种直接改开关的它不动
                // （实测踩过：回执说 sound:"off"，两个 pw-play 还在放）
                self.sync_decoder();
                (
                    format!(
                        "{{\"ok\":true,\"fps\":{},\"sleep\":\"{}\",\"scale\":\"{}\",\"effects\":\"{}\",\"sound\":\"{}\",\"time_scale\":{},\"volume\":{}}}",
                        self.fps,
                        serve::jesc(&self.sleep_mode),
                        serve::jesc(&self.render_scale),
                        if self.effects_on { "all" } else { "none" },
                        if self.sound_on { "on" } else { "off" },
                        self.time_scale,
                        // 1 = 还没读过（界面据此显示"未知"而不是假的 0）
                        self.sink_volume.unwrap_or(-1.0)
                    ),
                    false,
                )
            }
            serve::Cmd::SetProp { props, clear } => {
                // 通用搬运：把（名字, 值）写进渲染器的用户属性覆盖表（`WE_PROP_*` / `--prop` 同一张表）。
                // 内核不认识任何一个属性名 —— 属性定义在壁纸自己的 `project.json general.properties`。
                if clear {
                    we_scene_render::clear_user_props();
                }
                for (k, v) in &props {
                    we_scene_render::set_user_prop(k, v);
                }
                // 覆盖表是在 **build 时**逐对象/逐特效解析显隐的（`Visibility::resolve`）⇒
                // 改完必须重建渲染器，否则只是改了表、画面纹丝不动（"改参数没反应"的老毛病）。
                // 没带属性的空请求不算改动（不重建 ⇒ 不浪费那 ~0.3 秒）。
                // （`WE_SETPROP_FAST=1`，默认**关**）：**纯颜色类**属性
                // （只绑到某个文字层的 `color`，如本机 `text5`）不需要重建 ——
                // 字色是烘在光栅化贴图里的，换色 = 同一张覆盖率掩膜换个 RGB 再传一遍，
                // 与重新光栅化**逐字节相同**（渲染器单测 `recolor_matches_rasterize` 钉住）。
                // 实测重建一个 2560x1440 场景要 **620 ms**/条 ⇒ 拖动色盘时每条都重建
                // 会把内核打满一分钟。
                // 只要有一条属性走不了这条路（`visible`/材质色/带底板的文字），整条请求照旧重建
                // 宁可慢，不许错。
                let mut fast_done: Vec<String> = Vec::new();
                let mut need_rebuild = clear || !props.is_empty();
                if !clear && !props.is_empty() && we_scene_render::setprop_fast_on() {
                    let mut all_fast = true;
                    for (k, v) in &props {
                        match self.scene.as_mut().map(|sc| sc.refresh_user_prop(k, v)) {
                            Some(Ok(true)) => fast_done.push(k.clone()),
                            _ => {
                                all_fast = false;
                                break;
                            }
                        }
                    }
                    if all_fast {
                        need_rebuild = false;
                    } else {
                        // 半路失败：已经换过的那几张贴图**不影响**接下来的重建（重建会重光栅化）
                        fast_done.clear();
                    }
                }
                if need_rebuild {
                    self.rebuild_scene();
                }
                let applied = we_scene_render::user_props();
                let pairs: Vec<String> = applied
                    .iter()
                    .map(|(k, v)| format!("\"{}\":\"{}\"", serve::jesc(k), serve::jesc(v)))
                    .collect();
                // 内核日志留一行**证据**：证明命令真的到了内核、写进了表（不是"假通"）。
                println!(
                    "[控制面] set-prop clear={clear} 收到 {:?} ⇒ 覆盖表 {:?}（已重建场景={}{}）",
                    props,
                    applied,
                    need_rebuild,
                    if fast_done.is_empty() {
                        String::new()
                    } else {
                        format!("·局部刷新 {fast_done:?}")
                    }
                );
                (
                    format!("{{\"ok\":true,\"props\":{{{}}}}}", pairs.join(",")),
                    false,
                )
            }
            serve::Cmd::Mouse { x, y } => {
                // 鼠标位置（屏幕归一化 [0,1]，y 从顶往下）。内核**只存值**，
                // 真正下发在每帧渲染前（`tick_scene` 里 `set_mouse`）—— 这样"改设置不重建"
                // 也天然成立（鼠标不像 fit/color 那样要重烘 shader）。
                let nx = x.clamp(0.0, 1.0);
                let ny = y.clamp(0.0, 1.0);
                self.mouse = Some((nx, ny));
                self.mouse_src = "control";
                // 开覆盖窗口：全局指针（光标会话）300 条/秒，不设窗口的话这条命令会被立刻盖掉。
                self.mouse_override_until = Some(
                    std::time::Instant::now()
                        + std::time::Duration::from_millis(self.mouse_override_ms),
                );
                if let Some(s) = self.scene.as_mut() {
                    s.set_mouse(nx, ny);
                }
                println!("[控制面] mouse {nx:.4},{ny:.4}（屏幕归一化，y 从顶往下）");
                (
                    // 回执里带上来源与两项计数：脚本化验收（`we-ctl mouse …`）不用再补一条 status
                    format!(
                        "{{\"ok\":true,\"mouse\":\"{nx:.4},{ny:.4}\",\"mouse_src\":\"control\",\"mouse_events\":{},\"pointer_events\":{}}}",
                        self.mouse_events, self.pointer_events
                    ),
                    false,
                )
            }
            serve::Cmd::Audio {
                on,
                source,
                bands,
                sink,
                sinks,
            } => {
                // ① `"sinks":true` ⇒ **只列**可用 sink（不改任何状态、不碰采集）。
                if sinks {
                    return self.reply_audio_sinks(conn);
                }
                // ② `"sink":"…"` ⇒ 切换"跟哪一路输出做分析"。
                if let Some(spec) = sink.as_deref() {
                    match self.switch_audio_sink(spec) {
                        Ok(label) => {
                            println!("[控制面] audio sink={spec} ⇒ 切到 {label}");
                        }
                        Err(e) => {
                            println!("[控制面] audio sink={spec} ⇒ 拒绝：{e}");
                            let reply = format!(
                                "{{\"ok\":false,\"error\":\"{}\",\"audio\":\"{}\"}}",
                                serve::jesc(&e),
                                serve::jesc(&self.audio_status_string())
                            );
                            let _ = writeln!(conn, "{reply}");
                            let _ = conn.flush();
                            return false;
                        }
                    }
                }
                // ③ 音频频谱（粒子音频响应）：开关 / 来源 / 静态频谱 一次处理。
                // 真正下发是**每帧从进程内全局读**（渲染器 `audio::spectrum`），所以这里
                // 只写全局，不需要重建渲染器 —— 和 mouse 同一条"改设置不重建"的路子。
                let status = we_scene_render::audio::set_from_control(on, source.as_deref(), bands);
                println!(
                    "[控制面] audio on={:?} source={:?} sink={:?} bands={} ⇒ {}",
                    on,
                    source,
                    sink,
                    if bands.is_some() { "静态" } else { "不改" },
                    status
                );
                (
                    format!(
                        "{{\"ok\":true,\"audio\":\"{}\",\"enabled\":{},\"static\":{},\"sink\":\"{}\"}}",
                        serve::jesc(&self.audio_status_string()),
                        we_scene_render::audio::enabled(),
                        we_scene_render::audio::is_static(),
                        serve::jesc(&self.audio_sink.label())
                    ),
                    false,
                )
            }
            serve::Cmd::Dump { prefix, count, source } => {
                if source == "output" {
                    // **合成后取帧**。抓的是合成器的输出（屏幕上真正是什么）
                    // 与"壁纸自己再渲一遍"是两件事（见 ovdump.rs 头注释）。
                    match ovdump::start(self, prefix.clone(), count) {
                        Ok(n) => (
                            format!(
                                "{{\"ok\":true,\"dump\":\"{}\",\"count\":{n},\"source\":\"output\"}}",
                                serve::jesc(&prefix)
                            ),
                            false,
                        ),
                        Err(e) => (
                            format!("{{\"ok\":false,\"error\":\"{}\"}}", serve::jesc(&e)),
                            false,
                        ),
                    }
                } else {
                // 单条命令限流到 10 帧：每帧要多画一遍 + GPU 回读，40 帧会把
                // "主循环每圈多久回来"推到秒级（看门狗假停摆的来源，见 dump.rs）。
                let capped = dump::cap_count(count);
                self.dump_prefix = prefix.clone();
                self.dump_n = 0;
                self.dump_skips = 0;
                self.dump_left = capped;
                self.dump_log = None;
                self.dump_enc = None;
                let log_path = format!("{prefix}.txt");
                let slot = self
                    .scene
                    .as_ref()
                    .map(|s| {
                        let (w, h) = s.size();
                        (w as usize) * (h as usize) * 4
                    })
                    .unwrap_or(0);
                match std::fs::File::create(&log_path) {
                    Ok(mut f) => {
                        let _ = f.write_all(b"# idx\tscene_t\tmouse\tmouse_src\tglobal_events\n");
                        let log = Arc::new(Mutex::new(f));
                        self.dump_log = Some(log);
                        self.dump_enc = Some(dump::DumpEncoder::new(slot));
                        let note = if capped != count {
                            format!("（请求 {count} 帧 ⇒ 限流到 {capped}）")
                        } else {
                            String::new()
                        };
                        println!("[转储] 开始{note}：编码在 worker 线程，主循环只做回读");
                        (
                            format!(
                                "{{\"ok\":true,\"dump\":\"{}\",\"count\":{}}}",
                                serve::jesc(&prefix),
                                self.dump_left
                            ),
                            false,
                        )
                    }
                    Err(e) => (
                        format!(
                            "{{\"ok\":false,\"error\":\"{}\"}}",
                            serve::jesc(&e.to_string())
                        ),
                        false,
                    ),
                }
                }
            }
            serve::Cmd::Unknown(c) => (
                format!("{{\"ok\":false,\"error\":\"未知命令 {}\"}}", serve::jesc(&c)),
                false,
            ),
        };
        let _ = writeln!(conn, "{reply}");
        let _ = conn.flush();
        quit
    }

    /// b3/s3：视频每帧入口。0 拷贝（默认，进程内 VAAPI + dmabuf）或老路（ffmpeg 子进程 + shm）。
    fn tick_video(&mut self) {
        if self.zerocopy_video() {
            self.tick_video_gpu();
        } else {
            self.tick_video_shm();
        }
    }

    /// 视频 0 拷贝现在该用吗（用户关掉了 / 连错几次已退回老路 ⇒ false）
    fn zerocopy_video(&self) -> bool {
        self.video_zerocopy && self.video_gpu_errs < 3
    }

    /// How many frames the **live** source has presented.
    /// the slow-lap line and the exit line both read `scene.frames`, which is 0
    /// for a video or image wallpaper - a video stall then printed `frames=0` while frames were
    /// going up, which reads as "nothing is being rendered". Same precedence as `status_json`
    /// (the source being drawn right now), with image-layer draws as the last resort.
    fn frames_presented(&self) -> u64 {
        if let Some(d) = self.video.as_ref() {
            return d.frames();
        }
        if let Some(g) = self.video_gpu.as_ref() {
            return g.frames();
        }
        if let Some(s) = self.scene.as_ref() {
            return s.frames();
        }
        self.drawn as u64
    }

    /// 0 拷贝视频这条路正在跑吗（决定要不要按帧率排 poll 超时）
    fn zerocopy_video_active(&self) -> bool {
        self.video_path.is_some() && self.zerocopy_video()
    }

    /// 视频 0 拷贝：距离下一帧还有多久（精确，见 `scene_due_left`）
    fn video_due_left(&self) -> Option<std::time::Duration> {
        self.video_next
            .map(|t| t.saturating_duration_since(std::time::Instant::now()))
    }

    /// 视频 0 拷贝：距离下一帧还有多少毫秒（给 poll 当超时用；剩 >0 但不足 1ms ⇒ 报 1）
    fn video_due_in_ms(&self) -> i32 {
        match self.video_due_left() {
            Some(d) => {
                let ms = d.as_millis().min(1000) as i32;
                if ms == 0 && d.subsec_nanos() > 0 {
                    1
                } else {
                    ms
                }
            }
            None => 0,
        }
    }

    fn video_due(&self) -> bool {
        match self.video_next {
            Some(t) => std::time::Instant::now() >= t,
            None => true,
        }
    }

    /// This lap drew no video frame: push the next one out by a period (same rule as the
    /// post-draw pacing, so the two never fight). Prevents a poll(0) spin while a submit is late.
    fn defer_video(&mut self) {
        let period = Duration::from_millis(1000 / self.fps.max(1));
        let now = std::time::Instant::now();
        self.video_next = Some(match self.video_next {
            Some(prev) if prev + period > now => prev + period,
            _ => now + period,
        });
    }

    /// Count one 0-copy video failure; after 3 in a row fall back to the ffmpeg/shm path.
    fn video_zero_copy_err(&mut self, e: String, lw: u32, lh: u32) {
        if display_guard::is_display_lost(&e) {
            display_guard::lost(&e);
        }
        self.video_gpu_errs += 1;
        eprintln!(
            "[视频·0拷贝] 渲染失败（连续第 {}　次）：{e}",
            self.video_gpu_errs
        );
        if self.video_gpu_errs >= 3 {
            eprintln!(
                "[视频·0拷贝] 连续失败 ⇒ 关掉 0 拷贝，退回 ffmpeg 老路"
            );
            self.video_gpu = None;
            self.vdec = None;
            self.video_commit_hold = false;
            self.spawn_shm_decoder(lw, lh);
        }
    }

    /// 起 ffmpeg 子进程解码器（老路；0 拷贝起不来时也用它兜底）
    fn spawn_shm_decoder(&mut self, lw: u32, lh: u32) {
        if self.video.is_some() {
            return;
        }
        let Some(path) = self.video_path.clone() else { return };
        let (pw, ph) = match self.video_max {
            Some((w, h)) => (w, h),
            None => match self.last_phys {
                Some(p) => p,
                _ => (lw, lh),
            },
        };
        match video::Decoder::spawn(&path, pw, ph) {
            Ok(d) => {
                println!("[视频] ffmpeg 已启动：{path} → 解码上限 {pw}x{ph}（按屏封顶，只缩不放）");
                self.video = Some(d);
            }
            Err(e) => {
                eprintln!("[视频] 起 ffmpeg 失败：{e}");
                self.video_path = None;
            }
        }
    }

    /// s3：视频 0 拷贝 —— 进程内 VAAPI 解码 → dmabuf → EGL 贴图 → `wl_egl_window` swap。
    /// CPU 每帧只发 GL 命令；老路那三笔（`hwdownload` GPU→CPU、管道、写 shm）全没了。
    fn tick_video_gpu(&mut self) {
        let Some(surf) = self.surfaces.first() else { return };
        let (lw, lh) = (surf.width, surf.height);
        if lw == 0 || lh == 0 {
            return;
        }
        let out = surf.output.clone();
        // 渲染缓冲 = 物理尺寸（分数缩放下比按逻辑尺寸渲染锐；视频采样本来就是 GPU 干活）
        let (bw, bh) = match self.physical_size(&out) {
            Some((w, h)) if w > 0 && h > 0 => {
                self.last_phys = Some((w as u32, h as u32));
                (w as u32, h as u32)
            }
            _ => self.last_phys.unwrap_or((lw, lh)),
        };

        // ① 解码器（懒起）
        if self.vdec.is_none() {
            let path = match &self.video_path {
                Some(p) => p.clone(),
                None => return,
            };
            match we_scene_render::video::DmaVideo::open(std::path::Path::new(&path)) {
                Ok(d) => {
                    println!(
                        "[视频·0拷贝] 解码器就绪：{path}  源 {:.1} fps（硬解状态第一帧后确认）",
                        d.fps
                    );
                    self.vdec = Some(d);
                }
                Err(e) => {
                    eprintln!("[视频·0拷贝] 起解码器失败：{e} ⇒ 退回 ffmpeg 老路");
                    self.video_gpu_errs = 99;
                    self.spawn_shm_decoder(lw, lh);
                    return;
                }
            }
        }
        // ② 渲染器（懒建；要等拿到屏幕物理尺寸）
        if self.video_gpu.is_none() {
            let surface_raw = surf.layer.wl_surface().id().as_ptr();
            match unsafe {
                we_scene_render::video::VideoGl::new_window(
                    bw,
                    bh,
                    self.dpy_ptr as *mut std::ffi::c_void,
                    surface_raw as *mut std::ffi::c_void,
                )
            } {
                Ok(g) => {
                    println!(
                        "[视频·0拷贝] EGL 窗口就绪：渲染 {bw}x{bh} → 投递 逻辑 {lw}x{lh}（dmabuf，0 拷贝）"
                    );
                    self.video_gpu = Some(g);
                }
                Err(e) => {
                    eprintln!("[视频·0拷贝] 建 EGL 窗口失败：{e} ⇒ 退回 ffmpeg 老路");
                    self.video_gpu_errs = 99;
                    self.spawn_shm_decoder(lw, lh);
                    return;
                }
            }
        }

        // 3a a submit from the previous frame may still be out there. While it
        // is, the EGL context belongs to the `we-commit` thread and the decoder surface is still
        // being sampled by the GPU: do not decode, do not draw, keep the main loop answering.
        // The decoder frame is released by the lap that collects the submit.
        if self.video_commit_hold {
            let got = self.video_gpu.as_ref().and_then(|g| g.poll_commit());
            match video_commit_step(true, got) {
                VideoCommitStep::Release => {
                    self.video_commit_hold = false;
                    if let Some(d) = self.vdec.as_mut() {
                        d.release();
                    }
                    self.commit_note_done();
                }
                VideoCommitStep::Wait => {
                    self.commit_note_pending();
                    self.defer_video();
                    return;
                }
                VideoCommitStep::ReleaseErr(e) => {
                    self.video_commit_hold = false;
                    if let Some(d) = self.vdec.as_mut() {
                        d.release();
                    }
                    self.video_zero_copy_err(e, lw, lh);
                    self.defer_video();
                    return;
                }
                VideoCommitStep::Submit => {}
            }
        }

        // 3b decode one frame -> draw -> submit -> only then release it (releasing earlier lets
        // the decoder reuse a surface the GPU is still sampling).
        let frame = match self.vdec.as_mut().and_then(|d| d.next()) {
            Some(f) => *f,
            None => return, // nothing decoded this lap (needs more packets / EOF wrap)
        };
        // Bounded submit: eglSwapBuffers runs on the `we-commit` thread and this thread waits at
        // most `WE_COMMIT_DEADLINE_MS`. A late submit is not a render failure: the frame was drawn.
        // bracket the video draw with DrawBegin/DrawEnd. Without them a video
        // wallpaper's slow-lap line showed `draw -`, so the dmabuf import + blit landed in
        // `other` and the only segment we could attribute was the swap. gl_ctx marks the swap.
        // viewport 目标（物理缓冲 → 逻辑尺寸）**先**下发 —— 它随 swap 那次 commit 生效
        // 不在这里补第二次 commit（每帧只 commit 一次；第二次是"没有新 buffer 的空 commit"）。
        self.set_vp_dest(0, lw as i32, lh as i32);
        let drawn = match self.video_gpu.as_mut() {
            Some(g) => {
                slowloop::mark(we_scene_render::trace::Mark::DrawBegin);
                let r = g.draw(&frame);
                slowloop::mark(we_scene_render::trace::Mark::DrawEnd);
                r.and_then(|_| match g.swap_bounded(self.commit_deadline_ms) {
                    we_scene_render::gl_ctx::CommitOutcome::Done(Ok(())) => Ok(()),
                    we_scene_render::gl_ctx::CommitOutcome::Done(Err(e)) => Err(e),
                    _ => Ok(()),
                })
            }
            None => Ok(()),
        };
        self.video_commit_hold = self
            .video_gpu
            .as_ref()
            .is_some_and(|g| g.commit_pending());
        if !self.video_commit_hold {
            if let Some(d) = self.vdec.as_mut() {
                d.release();
            }
        }
        if let Err(e) = drawn {
            self.video_zero_copy_err(e, lw, lh);
            return;
        }
        self.video_gpu_errs = 0;
        // attach+commit 由 swap 做了（**一次**）；viewport 目标已在画之前下发，这里不再补 commit。
        let n = self.video_gpu.as_ref().map(|g| g.frames()).unwrap_or(0);
        if n == 1 {
            // 硬解状态只有解出帧之后才准（起解码器时还没有帧，那时读 wv_hw 一定是 0）
            let (hw, why) = match self.vdec.as_ref() {
                Some(d) => (d.hw(), d.hw_err()),
                None => (false, String::new()),
            };
            println!(
                "[视频·0拷贝] 第一帧已上屏（{}）",
                if hw { "VAAPI 硬解 ✓".to_string() } else { format!("软件解码：{why}") }
            );
        }
        if n == 1 || n % 60 == 0 {
            println!(
                "[视频·0拷贝] 已上屏 {n} 帧（渲染 {bw}x{bh} → 投递 逻辑 {lw}x{lh}，dmabuf，CPU 不搬像素）"
            );
        }
        // 严格按周期推进（和 scene 同一套：把干活时间吸收进周期，别叠成双倍周期）
        let period = Duration::from_millis(1000 / self.fps.max(1));
        let now = std::time::Instant::now();
        self.video_next = Some(match self.video_next {
            Some(prev) if prev + period > now => prev + period,
            _ => now + period,
        });
    }

    /// b3：从 ffmpeg 读一帧并上屏（老路，`WE_VIDEO_ZEROCOPY=0` 时走这条做 A/B）。
    /// 前提：调用方已确认**没在睡觉**（睡觉时阻塞读会卡住恢复事件）。
    /// 帧直接读进要 attach 的池缓冲（BGRA == Argb8888 的内存序，零转换）；
    /// 尺寸/比例差异交给 wp_viewport（set_source 裁 + set_destination 缩），CPU 不参与缩放。
    fn tick_video_shm(&mut self) {
        let Some(surf) = self.surfaces.first() else { return };
        let (lw, lh) = (surf.width, surf.height);
        if lw == 0 || lh == 0 {
            return;
        }
        let (fw, fh) = match &self.video {
            Some(d) => (d.width, d.height),
            None => return,
        };
        let stride = (fw * 4) as i32;
        let (buffer, canvas) = match self
            .pool
            .create_buffer(fw as i32, fh as i32, stride, wl_shm::Format::Argb8888)
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[视频] 取缓冲失败：{e}");
                return;
            }
        };
        // 直接读进池缓冲（self.pool 与 self.video 是不同的字段，可同时可变借用）
        let ok = match &mut self.video {
            Some(d) => d.next_frame(canvas),
            None => false,
        };
        if !ok {
            // 循环播放时不该走到这里；真到了说明解码器退了
            println!("[视频] 解码器退出（循环模式下不应发生）");
            self.video = None;
            return;
        }
        let wl_surface = self.surfaces[0].layer.wl_surface().clone();
        wl_surface.damage_buffer(0, 0, fw as i32, fh as i32);
        let _ = buffer.attach_to(&wl_surface);
        if let Some(vp) = &self.surfaces[0].viewport {
            let (sx, sy, sw, sh) = cover_crop(fw, fh, lw, lh);
            vp.set_source(sx, sy, sw, sh);
            vp.set_destination(lw as i32, lh as i32);
        }
        self.surfaces[0].layer.commit();
        let n = self.video.as_ref().map(|d| d.frames()).unwrap_or(0);
        if n % 60 == 0 {
            println!("[视频] 已上屏 {n} 帧（{fw}x{fh} → 逻辑 {lw}x{lh}，GPU 缩放）");
        }
    }

    /// s2：scene 该不该现在出下一帧。
    /// **先看 frame 回调闸门** —— 上一帧的回调还没回来 ⇒ 一律不画（这就是 vsync 对齐
    /// 以前是"按固定的 33ms 定时器往外推"，合成器慢了也照推，于是堆帧、抢扫）。
    /// 兜底：`FRAME_CB_MAX_WAIT_MS` 到点就放行（合成器不回调是合法的，见那个常量）。
    fn scene_due(&mut self) -> bool {
        if let Some(left) = self.frame_gate_left() {
            if !left.is_zero() {
                self.cb_gated += 1;
                return false;
            }
            // 兜底到点：丢掉这个回调再往下走（记一次 cb_timeouts）
            self.frame_gate_expire();
        }
        match self.scene_next {
            Some(t) => std::time::Instant::now() >= t,
            None => true,
        }
    }

    /// s2：距离下一帧还有多久（**精确**，保留亚毫秒）。
    /// 为什么需要它：`*_due_in_ms` 给 `poll` 用的是**毫秒**，而 `Duration::as_millis` 是
    /// **向下截断** ⇒ 每帧最后一毫秒里它一直返回 0 ⇒ `poll(0)` 立刻返回 ⇒ 主循环空转。
    /// 实测（IP 28 层 @30fps）：**每帧 1778 轮 `poll`，其中 1718 轮 timeout=0**，
    /// 白烧 ≈0.7ms/帧（= 整帧 CPU 的 ~16%）。补睡要用这个精确值，不能睡满取整后的毫秒。
    /// 等回调时**只等回调**（或兜底）—— 绝不能把 `min(due, gate)` 里的 due=0 拿去 poll(0)
    /// 过了 fps 截止时刻但回调还没回来时，`due` 是 0 而闸门还没开 ⇒ 那会变成**空转**
    /// （实测：3 秒 938 万轮 poll、CPU 100%）。等回调期间，能让我们往前走的唯一事件就是回调。
    fn scene_due_left(&self) -> Option<std::time::Duration> {
        let now = std::time::Instant::now();
        poll_wait_while_gated(
            self.frame_gate_left(),
            self.scene_next.map(|t| t.saturating_duration_since(now)),
        )
    }

    /// s2：距离下一帧还有多少毫秒（给 poll 当超时用）。
    /// ⚠ **必须向上取整**：向下截断会让"还剩 0.9ms"变成 `poll(0)`。剩 >0 但不足 1ms ⇒ 报 1。
    fn scene_due_in_ms(&self) -> i32 {
        match self.scene_due_left() {
            Some(d) => {
                let ms = d.as_millis().min(1000) as i32;
                if ms == 0 && d.subsec_nanos() > 0 {
                    1
                } else {
                    ms
                }
            }
            None => 0,
        }
    }

    /// s2：渲染一帧 scene 并上屏。
    /// 每帧的路径是「GL 渲到 FBO → 直接回读进要 attach 的那个 shm 缓冲（顺手翻 Y + 转 BGRA）」，
    /// 中间没有额外拷贝；尺寸/比例差异交给 wp_viewport（GPU 缩放）。
    fn tick_scene(&mut self) {
        watchdog::set_phase(watchdog::PHASE_SCENE);
        slowloop::mark(we_scene_render::trace::Mark::SceneBegin);
        // 「本次什么也没画」必须**退避一个周期**：否则 `scene_next` 不前移 ⇒
        // `scene_due` 恒真 ⇒ 事件循环 `poll(0)` 立刻返回 ⇒ 主循环空转
        // 。
        // 退避用的是与出帧后**同一个**排程规则（`next_scene_deadline`），不引入第二条时间口径。
        let drew = self.tick_scene_frame();
        if !drew {
            self.defer_scene();
        } else {
            // a presented frame means no commit is outstanding any more.
            self.commit_note_done();
        }
        slowloop::mark(we_scene_render::trace::Mark::SceneEnd);
    }

    /// A commit is outstanding: log it (first lap + every 60th), never count it as a
    /// render failure (the frame was drawn; only the submit is late).
    fn commit_note_pending(&mut self) {
        let now = std::time::Instant::now();
        let since = *self.commit_pending_since.get_or_insert(now);
        self.commit_pending_laps += 1;
        let waited = now.duration_since(since).as_millis();
        if self.commit_pending_laps == 1 || self.commit_pending_laps % 60 == 0 {
            let swap_ms = self
                .scene
                .as_ref()
                .map(|s| s.last_swap_ms())
                .or_else(|| self.video_gpu.as_ref().map(|g| g.last_swap_ms()))
                .unwrap_or(0);
            println!("【交帧】 eglSwapBuffers 已挂 {} ms（第 {} 轮未返回，上次用时 {} ms）⇒ 主循环继续跑，下一帧等它回来", waited, self.commit_pending_laps, swap_ms);
        }
        // bounded commit keeps the loop alive, but if the submit never comes back
        // the picture stays on its last frame forever and the EGL context is gone with the
        // blocked thread. Say it once and hand the process to systemd (exit 76, Restart=on-failure).
        if commit_guard::exceeded(waited as u64, self.commit_lost_ms) {
            println!(
                "【交帧】 eglSwapBuffers 已挂 {} ms（{} 轮），合成器始终没回收 ⇒ 表面已失效：退出 {} 让 systemd 重建（阈值 {} ms = WE_COMMIT_MAX_PENDING_MS，0=关）",
                waited,
                self.commit_pending_laps,
                commit_guard::EXIT_CODE_SUBMIT_LOST,
                self.commit_lost_ms
            );
            commit_guard::exit_now(commit_guard::EXIT_CODE_SUBMIT_LOST);
        }
    }

    /// The commit came back (or never hung): clear the pending timer.
    fn commit_note_done(&mut self) {
        if self.commit_pending_since.take().is_some() {
            let laps = std::mem::take(&mut self.commit_pending_laps);
            println!("【交帧】 已恢复（挂起期间主循环跑了 {laps} 轮）", );
        }
    }

    // ── ：每帧**一次** commit + `wl_surface.frame` 回调配速 ────────────────────
    // 实测：帧内容本身没有撕裂，撕裂只在呈现侧；呈现实测 p50 swap 51.7 ms
    // （≈19 fps，目标 60）、iGPU 103%、**每帧两次 wl_surface.commit**。
    // 两次 commit 的来源：一次是自己的 buffer attach + commit，第二次是"补 viewport
    // set_destination 后再 commit 一遍"。第二次是个**没有新 buffer 的 commit** ——
    // 合成器照样要重排一次合成/上屏，等于每帧让合成器干两遍活 ⇒ 错过 vblank。
    // 现在：viewport 目标只在**尺寸变化**时下发（状态本来就是持久的），每帧只 commit 一次。

    /// 本帧唯一那次 commit。**所有**上屏 commit 都必须从这里走（好让计数是诚实的）。
    fn commit_surface(&mut self, index: usize) {
        if let Some(s) = self.surfaces.get(index) {
            s.layer.commit();
            self.cm_this += 1;
            self.cm_total += 1;
        }
    }

    /// 0 拷贝：`eglSwapBuffers` 自己做了 attach + damage + commit ⇒ 这里只**计数**，
    /// 绝不再补一次 `layer.commit`（那正是第二次 commit）。
    fn note_swap_commit(&mut self) {
        self.cm_this += 1;
        self.cm_total += 1;
    }

    /// viewport 目标尺寸：只在变化时下发（返回 true = 本次真的下发了）。
    /// ⚠ 下发后不需要单独 commit —— 它随**下一帧**那次唯一 commit 一起生效。
    fn set_vp_dest(&mut self, index: usize, w: i32, h: i32) -> bool {
        let Some(s) = self.surfaces.get_mut(index) else {
            return false;
        };
        if s.vp_dest == Some((w, h)) {
            return false;
        }
        if let Some(vp) = &s.viewport {
            vp.set_destination(w, h);
            s.vp_dest = Some((w, h));
            return true;
        }
        false
    }

    /// 一帧真正上屏（或交帧）了：记账 + 每 100 帧打一行（验收就认这一行）。
    fn frame_presented(&mut self) {
        let c = std::mem::take(&mut self.cm_this);
        self.cm_frames += 1;
        self.cm_seen += 1;
        self.cm_commits += c;
        self.cm_max = self.cm_max.max(c);
        if self.cm_seen % 100 != 0 {
            return;
        }
        let (p50, p95, pmax, n) = self.cb_stats();
        println!(
            "[提交] 累计 {} 帧 / {} 次 commit｜最近 100 帧 {} 次 ⇒ {:.2} 次/帧（单帧最多 {}）\
             ｜frame 回调间隔 p50 {:.1} ms / p95 {:.1} ms / max {:.1} ms（n={}，超时兜底 {} 次）",
            self.cm_seen,
            self.cm_total,
            self.cm_commits,
            self.cm_commits as f64 / self.cm_frames.max(1) as f64,
            self.cm_max,
            p50,
            p95,
            pmax,
            n,
            self.cb_timeouts
        );
        self.cm_frames = 0;
        self.cm_commits = 0;
        self.cm_max = 0;
    }

    /// 挂一个 `wl_surface.frame` 回调：**下一帧必须等它回来**再画。
    /// 这就是 vsync 对齐：合成器说完"这一帧已经上了"之后我们才动下一次绘制，
    /// 而不是自己 sleep 一个固定周期去猜。
    fn request_frame_cb(&mut self, index: usize) {
        if !self.frame_cb_on || self.frame_cb.is_some() {
            return;
        }
        let Some(qh) = self.qh.clone() else { return };
        let Some(s) = self.surfaces.get(index) else { return };
        self.frame_cb = Some(s.layer.wl_surface().frame(&qh, ()));
        self.frame_wait_since = Some(std::time::Instant::now());
    }

    /// 还在等回调吗？`Some(left)` = 正在等（`left` 为 0 表示**超时兜底**已经到点）。
    fn frame_gate_left(&self) -> Option<Duration> {
        if !self.frame_cb_on {
            return None;
        }
        let since = self.frame_wait_since?;
        Some(Duration::from_millis(FRAME_CB_MAX_WAIT_MS).saturating_sub(since.elapsed()))
    }

    /// 撤掉一个还没被锁存的回调（这一圈根本没 commit，它永远不会触发）。
    fn cancel_frame_cb(&mut self) {
        self.frame_cb = None;
        self.frame_wait_since = None;
    }

    /// 兜底到点：丢掉这个回调（合成器不给我们也不能永远不画）。
    fn frame_gate_expire(&mut self) {
        if self.frame_cb.take().is_some() {
            self.cb_timeouts += 1;
        }
        self.frame_wait_since = None;
    }

    /// 最近 128 次回调的间隔分布（ms）：(p50, p95, max, n)。
    fn cb_stats(&self) -> (f32, f32, f32, usize) {
        let n = self.cb_gap_i.min(self.cb_gaps.len());
        if n == 0 {
            return (0.0, 0.0, 0.0, 0);
        }
        let mut v: Vec<f32> = self.cb_gaps[..n].to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pick = |q: f32| -> f32 {
            let i = ((n as f32 - 1.0) * q).round() as usize;
            v[i.min(n - 1)]
        };
        (pick(0.50), pick(0.95), v[n - 1], n)
    }

    /// 排下一帧到 `now + period`（本次没出帧）。
    fn defer_scene(&mut self) {
        let now = std::time::Instant::now();
        self.scene_next = Some(next_scene_deadline(self.scene_next, now, scene_period(self.fps)));
    }

    /// 渲染一帧 scene 并上屏。返回 **false** = 本次什么都没画（调用方退避一个周期）。
    fn tick_scene_frame(&mut self) -> bool {
        // 全局指针：懒建（等第一块输出出现；建好后 pointer_global_ok=true 不再重试）。
        if !self.pointer_global_ok && self.cap_manager.is_some() {
            if let Some(qh) = self.qh.clone() {
                self.ensure_global_pointer(&qh);
            }
        }
        // ⚠ 这里不能把 `surf` 的不可变借用带过 `set_vp_dest`（要 &mut self）：先取走三个值。
        let (lw, lh, out) = match self.surfaces.first() {
            Some(s) => (s.width, s.height, s.output.clone()),
            None => return false,
        };
        if lw == 0 || lh == 0 {
            return false;
        }
        // 本帧的 commit 计数从 0 开始（每帧只允许 1 次，见 `frame_presented`）。
        self.cm_this = 0;
        // viewport 目标尺寸**先**下发（状态是持久的）⇒ 它随本帧那次唯一 commit 生效
        // 不需要事后补一次 commit。渲染器建起来之前就设好，连预热那一帧都是对的尺寸。
        self.set_vp_dest(0, lw as i32, lh as i32);
        // frame 回调**必须在 commit 之前**挂上 —— 回调是"随这一次 commit 被锁存、在这一帧
        // 上屏时触发"的。挂在 commit 之后，它就只能被**下一帧**的 commit 锁存，而下一帧要等
        // 这个回调才画 ⇒ 自己把自己锁成两倍周期（实测：fps=60 只出 30 帧、间隔正好 33ms）。
        // 时间顺序：挂回调 → 画 → attach → commit(唯一一次) → 合成器上屏 → 回调 → 画下一帧。
        self.request_frame_cb(0);
        if self.scene.is_none() {
            let dir = match &self.scene_path {
                Some(d) => d.clone(),
                None => return false,
            };
            let (bw, bh) = match self.physical_size(&out) {
                Some((w, h)) if w > 0 && h > 0 => {
                    self.last_phys = Some((w as u32, h as u32));
                    (w as u32, h as u32)
                }
                // 输出被合成器摘掉再挂回之后，`physical_size` 会短暂为空 ⇒ 沿用
                // 上次已知的物理尺寸。否则会按**逻辑尺寸**重建（更糊），还白花一次构建。
                _ => self.last_phys.unwrap_or((lw, lh)),
            };
            let t0 = std::time::Instant::now();
            // 切换过渡（第二半）：把旧帧当**预热帧**交给新渲染器 —— 建 EGL 窗口那次
            // "清屏 + swap" 改成画旧帧 ⇒ 场景构建（几百毫秒）期间屏幕上是旧画面，**不是黑**。
            // 没拿到旧帧（第一次 apply / 从图片视频切过来）⇒ 用兜底纯色（绝不留纯黑）。
            let preload: Option<(u32, u32, Vec<u8>)> = if self.fade_ms > 0.0 {
                self.fade_src.take().or_else(|| {
                    let [r, g, b] = self.fade_fallback;
                    Some((1u32, 1u32, vec![r, g, b, 255]))
                })
            } else {
                None
            };
            let preload_ref: Option<(&[u8], u32, u32, bool)> = preload
                .as_ref()
                .map(|(w, h, p)| (p.as_slice(), *w, *h, false));
            // s2c7：0 拷贝（EGL wayland 平台 + wl_egl_window + swap）**默认开启** ✓
            // （踩坑记录：窗口表面的默认帧缓冲 FBO 0 在"第一次成功 swap 之前"是不完整的，
            // 清屏阶段会留下一个粘住的 0x506；必须先把它读掉，否则每一趟都会被误判成失败
            // ⇒ 再叠上"连续 5 次失败就重建"就成了重建风暴烧 1 个核 ✗）
            // `WE_SCENE_ZEROCOPY=0` 可以临时退回老的「回读 + shm」路做 A/B。
            let zerocopy = std::env::var("WE_SCENE_ZEROCOPY")
                .map(|v| v != "0")
                .unwrap_or(true);
            let opened = if zerocopy {
                // 省 GPU 档：按**逻辑**尺寸渲染特效链（viewport 照样铺满整屏，只是少画像素）
                let (bw, bh) = if self.render_scale == "logical" {
                    (lw, lh)
                } else {
                    (bw, bh)
                };
                let surface_raw = self.surfaces[0].layer.wl_surface().id().as_ptr();
                unsafe {
                    we_scene_render::SceneRenderer::open_window_fade(
                        std::path::Path::new(&dir),
                        bw,
                        bh,
                        self.dpy_ptr as *mut std::ffi::c_void,
                        surface_raw as *mut std::ffi::c_void,
                        preload_ref,
                    )
                }
            } else {
                we_scene_render::SceneRenderer::open(std::path::Path::new(&dir), bw, bh)
            };
            match opened {
                Ok(s) => {
                    let (rend, ver) = s.tile_gl_info();
                    println!(
                        "[场景] 渲染器就绪：{}  {bw}x{bh}  图层 {}  显示裁切 {:?}  构建 {} ms",
                        s.title,
                        s.layer_count,
                        s.cover,
                        t0.elapsed().as_millis()
                    );
                    println!("[场景] GL: {rend} / {ver}");
                    self.scene = Some(s);
                    self.scene_t0 = Some(std::time::Instant::now());
                    // 把当前的设置同步给新渲染器（特效档）
                    if let Some(sc) = self.scene.as_mut() {
                        sc.set_effects(self.effects_on);
                    }
                    // 切换过渡（第三半）：新场景就绪 ⇒ 从旧帧 crossfade 过来（α 1→0）。
                    // 第一帧的 α≈1 ⇒ 屏幕上仍是刚才那张旧画面 ⇒ 与预热帧**无缝衔接**
                    // （不会"旧帧→黑→新帧"），随后 400ms 线性淡到新场景。
                    if let Some((w, h, px)) = preload {
                        let dm = self.fade_ms;
                        let started = self.scene.as_mut().map(|sc| sc.start_fade(w, h, px, dm));
                        match started {
                            Some(Ok(())) => {
                                let el = self
                                    .fade_t_apply
                                    .map(|t| t.elapsed().as_millis())
                                    .unwrap_or(0);
                                println!(
                                    "[过渡] crossfade 开始：{dm:.0} ms（apply 起 {el} ms，源 {w}x{h}）"
                                );
                            }
                            Some(Err(e)) => eprintln!("[过渡] 起 crossfade 失败：{e}"),
                            None => {}
                        }
                    }
                    // 配乐（scene 的 sound 对象）：换场景时重建；同一场景（比如换 scale 重建渲染器）
                    // 就不重来，免得切歌/重播。音轨在 Drop 里收掉，不留孤儿播放器。
                    let same_scene = self.audio_for.as_deref() == Some(dir.as_str());
                    if !same_scene {
                        self.audio = None;
                        self.audio_for = None;
                        match audio::Player::prepare(
                            std::path::Path::new(&dir),
                            &audio::default_cache_dir(),
                        ) {
                            Ok(Some(p)) => {
                                let n = p.track_count();
                                self.audio = Some(p);
                                self.audio_for = Some(dir.clone());
                                println!("[音频] 已挂上 {n} 个 sound 对象");
                            }
                            Ok(None) => {}
                            Err(e) => eprintln!("[音频] 这个场景没读成：{e}"),
                        }
                        // 立刻按当前状态（睡/醒）决定播不播
                        let want = self.is_active();
                        if let Some(a) = self.audio.as_mut() {
                            a.set_playing(want);
                        }
                    }
                }
                Err(e) => {
                    if display_guard::is_display_lost(&e) {
                        display_guard::lost(&e);
                    }
                    eprintln!("[场景] 打不开：{e}");
                    self.scene_path = None;
                    // 渲染器都没起来就别放音乐了（本机实测有壁纸会在解析/着色器阶段失败）
                    self.audio = None;
                    self.audio_for = None;
                    return false;
                }
            }
        }
        let (bw, bh) = self.scene.as_ref().map(|s| s.size()).unwrap_or((lw, lh));
        // 0 拷贝模式不用池缓冲（缓冲归 EGL/合成器管），只有回读那条路才要
        let window_mode = self.scene.as_ref().map(|s| s.is_window()).unwrap_or(false);
        let mut slot = None;
        if !window_mode {
            let stride = (bw * 4) as i32;
            match self
                .pool
                .create_buffer(bw as i32, bh as i32, stride, wl_shm::Format::Argb8888)
            {
                Ok(b) => slot = Some(b),
                Err(e) => {
                    eprintln!("[场景] 取缓冲失败：{e}");
                    return false;
                }
            }
        }
        // 回读直接落进池缓冲（self.pool 与 self.scene 是不同字段，可同时可变借用）
        let elapsed = self
            .scene_t0
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        // dump: take one buffer from the pool for this frame; if the pool is empty
        // (worker still encoding) we skip the frame instead of blocking or allocating.
        let mut dump_buf = if self.dump_left > 0 && self.dump_enc.is_some() {
            let need = (bw as usize) * (bh as usize) * 4;
            match self.dump_enc.as_mut().map(|e| e.take_buf(need)) {
                Some(Some(b)) => Some(b),
                Some(None) => {
                    if let Some(e) = self.dump_enc.as_mut() {
                        e.skipped += 1;
                    }
                    None
                }
                None => None,
            }
        } else {
            None
        };
        let mut dump_swizzle = false;
        // A submit was still outstanding: this lap drew nothing and the dump buffer goes back.
        let mut dump_skip = false;
        // This lap drew and captured, but its submit is late: note it after the pool borrow ends.
        let mut dump_commit_note = false;
        // 转储这一帧在主循环上多花的时间（回读/拷贝；编码在 worker 里）
        let mut dump_cap_ms = 0.0f64;
        // 0 拷贝这条路本帧是否真的调过 `eglSwapBuffers`（= commit 已经发生一次）
        let mut swap_done = false;
        let res = match (self.scene.as_mut(), slot.as_mut()) {
            (Some(s), Some((_, canvas))) => {
                s.set_time(elapsed); // 粒子/动画按真实时间推进（别用帧数猜）
                // 鼠标（屏幕归一化，y 从顶往下）⇒ 渲染器的 `g_PointerPosition`。
                // 每帧推一次：两个来源（内核自己的 wl_pointer / 控制面命令）都写 self.mouse，
                // 这里统一出口。`None` = 还没拿到鼠标 ⇒ 不下发（= 与加这一路之前逐字节一致）。
                if let Some((mx, my)) = self.mouse {
                    s.set_mouse(mx, my);
                }
                let r = s.render_to(&mut *canvas, true); // true = 输出 BGRA
                // the on-screen frame is already in the pool buffer (BGRA): copy it for
                // the dump instead of drawing the whole frame a second time. R/B swap is
                // done by the worker, so the main loop only pays a memcpy.
                if r.is_ok() {
                    if let Some(buf) = dump_buf.as_mut() {
                        let need = (bw as usize) * (bh as usize) * 4;
                        if buf.len() >= need && canvas.len() >= need {
                            let t_cap = std::time::Instant::now();
                            buf[..need].copy_from_slice(&canvas[..need]);
                            dump_cap_ms = t_cap.elapsed().as_secs_f64() * 1000.0;
                            dump_swizzle = true;
                        }
                    }
                }
                r
            }
            (Some(s), None) => {
                s.set_time(elapsed);
                if let Some((mx, my)) = self.mouse {
                    s.set_mouse(mx, my);
                }
                // a submit from an earlier frame may still be in flight. While it
                // is, the EGL context is with the `we-commit` thread, so drawing here would be a
                // use-after-handoff: poll only, and skip this dump frame (buffer goes back to the
                // pool). Late successes need no action; late failures surface as the frame error.
                if self.commit_deadline_ms > 0 && s.commit_pending() {
                    match s.poll_commit() {
                        Some(Err(e)) => Err(e),
                        _ => {
                            dump_skip = true;
                            Ok(())
                        }
                    }
                } else {
                    match dump_buf.as_mut() {
                        // 0-copy + dump: draw once, read back, submit (bounded like the normal path)
                        Some(buf) => {
                            if self.commit_deadline_ms == 0 {
                                match s.render_window_with_capture(buf) {
                                    Ok(ms) => {
                                        dump_cap_ms = ms;
                                        swap_done = true;
                                        Ok(())
                                    }
                                    Err(e) => Err(e),
                                }
                            } else {
                                match s.render_window_with_capture_bounded(
                                    buf,
                                    self.commit_deadline_ms,
                                ) {
                                    Ok((ms, we_scene_render::FrameCommit::Presented)) => {
                                        dump_cap_ms = ms;
                                        swap_done = true;
                                        Ok(())
                                    }
                                    // Drawn + read back, only the present is late: the pixels in
                                    // `buf` are this frame, so keep it and let the loop carry on.
                                    Ok((ms, we_scene_render::FrameCommit::Pending)) => {
                                        dump_cap_ms = ms;
                                        swap_done = true;
                                        dump_commit_note = true;
                                        Ok(())
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                        }
                        // 0-copy: draw + submit, CPU never touches a pixel.
                        // with a deadline the swap runs on the `we-commit` thread, so
                        // this returns `Pending` instead of parking the whole main loop when the
                        // compositor stops answering (23/23 stalls = output reconfiguration).
                        None => {
                            if self.commit_deadline_ms == 0 {
                                match s.render_window() {
                                    Ok(()) => {
                                        swap_done = true;
                                        Ok(())
                                    }
                                    Err(e) => Err(e),
                                }
                            } else {
                                match s.render_window_bounded(self.commit_deadline_ms) {
                                    Ok(we_scene_render::FrameCommit::Presented) => {
                                        swap_done = true;
                                        Ok(())
                                    }
                                    Ok(we_scene_render::FrameCommit::Pending) => {
                                        // 交帧还没回来：这一帧不算上屏（计数别漏到下一帧）
                                        self.cm_this = 0;
                                        self.commit_note_pending();
                                        return false;
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                        }
                    }
                }
            }
            (None, _) => return false,
        };
        if let Err(e) = res {
            if display_guard::is_display_lost(&e) {
                display_guard::lost(&e);
            }
            // 单帧渲染抖动（例如某帧粒子数/贴图参数越界）**不该把整张壁纸判死**：
            // （实测那每 ~22 秒一次的"重建抖动"就是这么来的）。
            // 连续多次失败才认为是真坏了、重建一次。
            self.scene_errs += 1;
            eprintln!("[场景] 渲染失败（连续第 {} 次）：{e}", self.scene_errs);
            if self.scene_errs >= 5 {
                eprintln!("[场景] 连续 5 次渲染失败 ⇒ 丢弃渲染器重建");
                self.scene = None;
                self.scene_t0 = None;
                self.scene_next = Some(std::time::Instant::now());
                self.scene_errs = 0;
            }
            return false;
        }
        self.scene_errs = 0;
        // the readback memcpy is already timed (`dump_cap_ms`) for the `[转储]`
        // line; hand the same number to the slow-lap probe so it lands in its own `dump`
        // segment instead of `scene ... rest`. No-op unless WE_LOOP_SLOW_MS is set.
        if dump_cap_ms > 0.0 {
            slowloop::note_dump_ms(dump_cap_ms);
        }
        // 帧转储：**上屏之后**额外渲一份到 CPU 缓冲（不改上屏路径，0 拷贝照旧）。
        // 为什么能拿到"真相"：这是壁纸自己渲染的帧，**不受窗口遮挡** —— 最大化窗口把桌面盖住时，
        // `computer_use` 抓屏根本看不到透镜，而这里能。`set_time` 重设一次 ⇒ 相位不因多渲而漂。
        // queue the dump frame for the encoder worker (non-blocking); the worker
        // returns the buffer to the pool when done. On render failure give the buffer
        // back so the pool never starves.
        if self.dump_left > 0 {
            let captured = if res.is_ok() && !dump_skip {
                dump_buf.take()
            } else {
                None
            };
            match captured {
                None => {
                    if let (Some(b), Some(e)) = (dump_buf.take(), self.dump_enc.as_mut()) {
                        e.give_back(b);
                    }
                }
                Some(buf) => {
                    let idx = self.dump_n;
                    let path = format!("{}_{:04}.png", self.dump_prefix, idx);
                    let m = self
                        .mouse
                        .map(|(x, y)| format!("{x:.4},{y:.4}"))
                        .unwrap_or_else(|| "-".into());
                    let line = format!(
                        "{idx}\t{elapsed:.3}\t{m}\t{}\t{}\n",
                        self.mouse_src, self.pointer_global_events
                    );
                    let log = self.dump_log.clone();
                    let sent = match (log, self.dump_enc.as_mut()) {
                        (Some(log), Some(e)) => {
                            e.submit(path.clone(), buf, bw, bh, idx, line, dump_swizzle, log)
                        }
                        _ => false,
                    };
                    if sent {
                        self.dump_n += 1;
                        self.dump_left -= 1;
                        println!(
                            "[转储] 第 {idx} 帧 @ {elapsed:.3}s 指针 {m} 抓帧 {dump_cap_ms:.0} ms ⇒ 已排队 {path}"
                        );
                        if self.dump_left == 0 {
                            let (sent_n, skipped) = self
                                .dump_enc
                                .as_ref()
                                .map(|e| (e.sent, e.skipped))
                                .unwrap_or((self.dump_n, 0));
                            println!(
                                "[转储] 已排队 {sent_n} 张，跳过 {skipped} 帧（编码器忙）/ {} 帧（交帧未回收）",
                                self.dump_skips
                            );
                        }
                    } else {
                        eprintln!("[转储] 编码 worker 已退出 ⇒ 停止转储");
                        self.dump_left = 0;
                    }
                }
            }
        }
        match slot {
            Some((buffer, _)) => {
                // **一次 commit**（这条路本来就只有一次，保留 `attach_to` 的 activate 语义
                // 它把 slot 标记为"合成器正在读"，否则 `Buffer` 一 Drop 内存就被池回收，
                // 下一帧 create_buffer 会拿到同一块内存 ⇒ 花屏）。
                // `attach_to` = activate + wl_surface.attach（**不 commit**，SCTK 0.20 实测）。
                let wl_surface = self.surfaces[0].layer.wl_surface().clone();
                wl_surface.damage_buffer(0, 0, bw as i32, bh as i32);
                let _ = buffer.attach_to(&wl_surface);
                self.commit_surface(0);
            }
            None => {
                // 0 拷贝：attach + damage + commit **由 eglSwapBuffers 做掉了**（见上面的
                // `note_swap_commit`）。这里绝不能再补一次 `layer.commit` ——
                // viewport 目标尺寸已经在帧开头下发过，它随那次 swap 的 commit 一起生效。
                // ⇒ 修掉的正是"每帧两次 commit"：第二次是个**没有新 buffer 的 commit**，
                // 合成器照样再排一次合成/上屏（实测 iGPU 103%、swap p50 51.7ms）。
            }
        }
        // 0 拷贝这一帧的 commit 由 `eglSwapBuffers` 做（attach+damage+commit 一次）
        // 这里只记账，**绝不再补一次** `layer.commit`。
        if swap_done {
            self.note_swap_commit();
        }
        if dump_skip {
            // A submit was still outstanding: no frame drawn this lap, so the dump buffer goes
            // back to the pool and the caller backs off one period (this is not an error).
            // say so once, then every 60th - this path was mute, so a dump issued
            // while the compositor was not collecting looked like "the dump silently stopped".
            // Only a dump frame can be skipped, so only count/report while a dump is running:
            // with no dump the same guard still fires every pending lap (use-after-handoff
            // protection), and a line saying "skipped the dump frame, 0 left" is a lie.
            self.cm_this = 0; // 本帧没有上屏 ⇒ 不进统计（别让计数漏到下一帧）
            // 本圈**什么都没 commit** ⇒ 挂出去的回调永远不会被锁存，必须撤掉，否则要等
            // 100ms 兜底才继续（白等）。
            self.cancel_frame_cb();
            if self.dump_left > 0 {
                self.dump_skips += 1;
                if self.dump_skips == 1 || self.dump_skips % 60 == 0 {
                    println!(
                        "[转储] 交帧还没回收（已挂 {} ms）⇒ 本圈跳过抓帧（第 {} 次，还剩 {} 帧待抓）",
                        self.commit_pending_since
                            .map(|t| t.elapsed().as_millis())
                            .unwrap_or(0),
                        self.dump_skips,
                        self.dump_left
                    );
                }
            }
            if let Some(b) = dump_buf.take() {
                if let Some(e) = self.dump_enc.as_mut() {
                    e.give_back(b);
                }
            }
            self.commit_note_pending();
            return false;
        }
        if dump_commit_note {
            // The frame is captured and queued; only the present is late. Tell the caller so it
            // backs off instead of drawing into a context the commit thread still owns.
            // 这一帧确实画了/交了（commit 已发生），仍要进统计；但**不挂 frame 回调** ——
            // 它在等 commit 线程，回调闸门由 `commit_pending` 负责。
            self.frame_presented();
            self.commit_note_pending();
            return false;
        }
        // 本帧收尾 —— 记账（commit 次数 / 帧间隔分布）。回调已经在**本帧 commit 之前**
        // 挂上了，所以下一帧会等到"这一帧真的上屏"之后才开始（vsync 对齐）。
        self.frame_presented();
        let n = self.scene.as_ref().map(|s| s.frames()).unwrap_or(0);
        if n == 1 {
            // 过渡的**终点判据**：首帧上屏的那一刻 = 屏幕上第一次出现新场景
            // （之前一直是预热帧/crossfade）。配合 `[过渡] crossfade 开始` 与构建时间，
            // 就能算出"黑窗"到底还在不在（修好后应当没有黑窗，只有一段 crossfade）。
            if let Some(t) = self.fade_t_apply {
                let fa = self.scene.as_ref().map(|s| s.fade_active()).unwrap_or(false);
                println!(
                    "[过渡] 首帧已上屏（apply 起 {} ms）{}",
                    t.elapsed().as_millis(),
                    if fa { "，crossfade 进行中" } else { "" }
                );
            }
        }
        if n == 1 || n % 60 == 0 {
            // 注意 `bw x bh` 是**渲染缓冲**尺寸，`逻辑 lw x lh` 是**投递**尺寸（viewport 的目标）：
            // 物理档也是"2560x1440 → 逻辑 2048x1152"（分数缩放 1.25 就是这么表达的），
            // 所以**别把这行当成"在按逻辑尺寸渲染"** —— 渲染档看下面这个 [物理]/[逻辑] 标注。
            println!(
                "[场景] 已上屏 {n} 帧（渲染 {bw}x{bh} [{}] → 投递 逻辑 {lw}x{lh}{}）",
                if self.render_scale == "logical" { "逻辑" } else { "物理" },
                if window_mode { "，0 拷贝" } else { "" }
            );
        }        // 严格按周期推进：把这一帧的工作时间**吸收进周期**，而不是"干完活再等一整个周期"。
        // 后者会把 50ms 周期叠成 50+18=68ms ⇒ 实测只有 14.6fps，白写了 20。
        // 落后太多（刚从睡眠/遮挡中恢复）就不追补，直接对齐到现在，避免补帧风暴。
        let period = scene_period(self.fps);
        let now = std::time::Instant::now();
        self.scene_next = Some(next_scene_deadline(self.scene_next, now, period));
        true
    }
}

/// 等 `wl_surface.frame` 回调时，`poll` 该睡多久。
/// **为什么单拎出来当纯函数**：这里错过一次就会**空转**。规矩只有一条：
/// 闸门还开着（`gate_left > 0`）⇒ 只等这个剩余时间 —— 哪怕 fps 截止时刻早就过了（`due = 0`），
/// 因为那一刻能让我们往前走的唯一事件就是回调。实测反例：按 `min(due, gate)` 走 ⇒
/// 3 秒 938 万轮 `poll(0)`、CPU 100%。
fn poll_wait_while_gated(
    gate_left: Option<std::time::Duration>,
    due: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match gate_left {
        Some(left) if !left.is_zero() => Some(left),
        _ => due,
    }
}

/// 场景出帧周期。`fps` 由 `WE_FPS`/控制面夹到 >=1 ⇒ 永不返回 0（0 会让 poll 退化成空转）。
fn scene_period(fps: u64) -> Duration {
    Duration::from_millis(1000 / fps.max(1))
}

/// 下一帧的排程（**唯一**一条规则：出帧后与退避共用）。
///
/// * `prev + period > now`：把这一帧的工作时间吸收进周期（否则 50ms 周期叠成 68ms，实测只有 14.6fps）。
///
/// * 落后太多（刚从睡眠/遮挡恢复）：不追补，直接对齐到 `now + period`，避免补帧风暴。
fn next_scene_deadline(
    prev: Option<std::time::Instant>,
    now: std::time::Instant,
    period: Duration,
) -> std::time::Instant {
    match prev {
        Some(p) if p + period > now => p + period,
        _ => now + period,
    }
}

/// 切换壁纸的过渡时长（毫秒）。默认 **400**。
///
/// * `WE_FADE=0` 或 `WE_FADE_MS=0` ⇒ **关闭**过渡（回到老行为：窗口预热清黑 + 硬切到新场景）。
/// 这是 A/B 与回退用的开关。
///
/// * `WE_FADE_MS=<n>` ⇒ 自定义时长（0..；内核会夹到 0..5000）。
fn fade_ms_env() -> f32 {
    if std::env::var("WE_FADE").map(|v| v.trim() == "0").unwrap_or(false) {
        return 0.0;
    }
    match std::env::var("WE_FADE_MS").ok().and_then(|v| v.trim().parse::<f32>().ok()) {
        Some(v) => v.clamp(0.0, 5000.0),
        None => 400.0,
    }
}

/// 拿不到旧帧（第一次 apply / 从图片/视频切到 scene）时的兜底纯色。
/// `WE_FADE_FALLBACK=R,G,B`（默认 `18,18,20` 深灰）。**绝不留纯黑空档**是本次硬要求。
fn fade_fallback_env() -> [u8; 3] {
    let s = std::env::var("WE_FADE_FALLBACK").unwrap_or_default();
    let v: Vec<u8> = s
        .split(',')
        .filter_map(|t| t.trim().parse::<u8>().ok())
        .collect();
    if v.len() == 3 {
        [v[0], v[1], v[2]]
    } else {
        [18, 18, 20]
    }
}

/// s2：scene 的刷新上限（**源码默认 20**；上屏那份由 drop-in `WE_SCENE_FPS` 覆盖）。
/// **可以调、也可以关掉"被遮挡就睡"**，方便对比测量：
/// `WE_SCENE_FPS=<1..240>` —— 覆盖默认上限（源码默认见下 `SCENE_FPS_DEFAULT`）。
/// `WE_NO_SLEEP=1` —— 忽略"窗口遮挡就停渲"，强制一直画（测量时用 ✗：
/// 不然壁纸被窗口盖住会静默降到 ~1fps，量出来的是"没在画"而不是"画得便宜"）
///
/// ## `20` 是怎么来的（本次调研的结论，如实记）
///
/// 1. **历史原因**：老渲染路径是「GL→FBO→回读 14.7MB→CPU 翻 Y+RGBA→BGRA→shm」，
/// 实测 **9.4ms/帧**（≈106fps 的理论值被各种开销压到 20），于是**主动限到 20fps 省资源**。
///
/// 2. **理由已消失但常数没改**：b3.5 换成 DMA-BUF 零拷贝之后每帧只要 **1.37ms**（CPU 只发命令），
/// "20fps 才跑得起"这条前提不再成立 —— 但 `SCENE_FPS_DEFAULT` **一直留在 20**（本行的上一条
/// 注释曾写"所以现在默认 30fps"，那与代码不符，是**过期的注释**，本次据实改回）。
///
/// 3. **上屏实际跑 30**：部署的 drop-in `20-mesh-sway.conf` 设 `WE_SCENE_FPS=30`
/// （用户报"动画不流畅"⇒ 从 20 提到 30）。所以"桌面上是 30fps、源码默认是 20"两者不矛盾。
///
/// 4. **上限为什么停在 240 而不该往上抬**：本机 iGPU 实测每帧 GPU 17.1ms（线性拟合，空闲基线 0.6%）：
/// @20fps = 34.8% GPU / 0.103 核；@30fps = 51% GPU / ≈0.155 核；**@60fps = 103% ⇒ 做不到**。
/// 再叠上 `eglSwapInterval=1` + 60Hz 合成器这一硬顶 ⇒ 实质上限就是 **60**，240 只是协议层的夹取。
/// 功耗取向：留 20 更凉更省（少 ~16% GPU 占用），30 只是顺滑一点；默认值不该在"只做诊断"里改。
const SCENE_FPS_DEFAULT: u64 = 20;

/// 等 `wl_surface.frame` 回调的**兜底上限**（ms）。
/// 为什么必须有：合成器**不调** `frame` 回调是完全合法的（表面被遮挡、输出被摘掉、
/// 合成器正在改配置）。没有兜底的话，"等回调才画下一帧"会变成永不出帧。
/// 100 ms ≈ 6 个 60Hz vblank：真在正常合成时回调远早于此（实测 p95 十几 ms），
/// 到点就是异常 ⇒ 记一次 `cb_timeouts` 并放行，绝不把自己钉死。
const FRAME_CB_MAX_WAIT_MS: u64 = 100;

fn scene_fps() -> u64 {
    std::env::var("WE_SCENE_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v >= 1 && *v <= 240)
        .unwrap_or(SCENE_FPS_DEFAULT)
}

/// 把一次「app 存档恢复」的动作序列里**属于渲染器的那一半真的写下去**（清表 + 逐条写属性），
/// 返回"覆盖表被动过没有"（动过 ⇒ 调用方必须重建渲染器）。
/// 抽成自由函数有两个原因：
/// ① `SetFps` 是**内核自己的字段**（`Wall::fps`），不属于渲染器 ⇒ 由调用方处理；
/// ② 覆盖表是进程内静态表（不需要 GL / Wayland）⇒ 单测能直接验这一跳。
/// 语义与 `serve::Cmd::SetProp` 完全一致：`ClearProps` ⇒ 先清空整张表，再逐条写
/// 界面侧补发也是 `clear=true` 的整套属性，两边口径一样才不会互相踩。
fn apply_actions_to_renderer(actions: &[app_state::Action]) -> bool {
    let mut touched = false;
    for a in actions {
        match a {
            app_state::Action::ClearProps => {
                we_scene_render::clear_user_props();
                touched = true;
            }
            app_state::Action::SetProp(k, v) => {
                // 内核不认识任何一个属性名，原样搬运（属性定义在壁纸自己的 project.json 里）
                we_scene_render::set_user_prop(k, v);
                touched = true;
            }
            app_state::Action::SetFps(_) => {}
        }
    }
    touched
}

fn no_sleep() -> bool {
    std::env::var("WE_NO_SLEEP").is_ok()
}

/// cover 裁切：从 (fw,fh) 里裁出与 (pw,ph) 同比例的矩形（供 wp_viewport.set_source）
fn cover_crop(fw: u32, fh: u32, pw: u32, ph: u32) -> (f64, f64, f64, f64) {
    let target = pw as f64 / ph as f64;
    let src = fw as f64 / fh as f64;
    if src > target {
        let cw = fh as f64 * target;
        ((fw as f64 - cw) / 2.0, 0.0, cw, fh as f64)
    } else {
        let ch = fw as f64 / target;
        (0.0, (fh as f64 - ch) / 2.0, fw as f64, ch)
    }
}

/// 等比铺满（cover）+ 双线性采样，居中裁剪。纯 CPU，只算一次。
fn blit_cover(src: &image::RgbaImage, canvas: &mut [u8], w: u32, h: u32) {
    let (sw, sh) = src.dimensions();
    if sw == 0 || sh == 0 {
        return;
    }
    let scale = f64::max(w as f64 / sw as f64, h as f64 / sh as f64);
    let ox = (sw as f64 * scale - w as f64) / 2.0;
    let oy = (sh as f64 * scale - h as f64) / 2.0;
    let px = src.as_raw();
    for y in 0..h {
        let sy = (y as f64 + oy) / scale;
        let sy0 = sy.floor().clamp(0.0, (sh - 1) as f64) as u32;
        let sy1 = (sy0 + 1).min(sh - 1);
        let fy = (sy - sy0 as f64).clamp(0.0, 1.0);
        for x in 0..w {
            let sx = (x as f64 + ox) / scale;
            let sx0 = sx.floor().clamp(0.0, (sw - 1) as f64) as u32;
            let sx1 = (sx0 + 1).min(sw - 1);
            let fx = (sx - sx0 as f64).clamp(0.0, 1.0);
            let mut acc = [0f64; 3];
            for (dy, wy) in [(sy0, 1.0 - fy), (sy1, fy)] {
                for (dx, wx) in [(sx0, 1.0 - fx), (sx1, fx)] {
                    let weight = wy * wx;
                    if weight == 0.0 {
                        continue;
                    }
                    let i = ((dy * sw + dx) * 4) as usize;
                    acc[0] += px[i] as f64 * weight;
                    acc[1] += px[i + 1] as f64 * weight;
                    acc[2] += px[i + 2] as f64 * weight;
                }
            }
            // wl_shm 的 Argb8888 在小端上是 [B, G, R, A]
            let o = ((y * w + x) * 4) as usize;
            canvas[o] = acc[2] as u8;
            canvas[o + 1] = acc[1] as u8;
            canvas[o + 2] = acc[0] as u8;
            canvas[o + 3] = 0xFF;
        }
    }
}

impl CompositorHandler for Wall {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // 静态壁纸不请求新帧 —— 空闲 0 开销的来源
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Wall {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        println!("[输出] 新显示器接入 ⇒ 给还没有表面的输出补开层壳表面");
        self.create_surfaces(qh);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        println!("[输出] 显示器移除（表面清理由 LayerShell::closed 接手）");
    }
}

impl LayerShellHandler for Wall {
    fn closed(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor dismissed our layer surface (output unplugged / re-added, mode change,
        // compositor restart). A submit may still be out on that retired surface: then nothing can
        // be torn down safely (round 13) and only a fresh process can present again - so exit 76.
        // Otherwise the shared body drops the renderers (wl_egl_window before wl_surface),
        // removes the dead surface and creates a new one: without that the desktop keeps the last
        // frame forever (round 11 item 2).
        match self
            .surfaces
            .iter()
            .position(|s| s.layer.wl_surface().id() == layer.wl_surface().id())
        {
            Some(i) => self.surface_closed(i, qh),
            None => println!("[层壳] 合成器关掉了一个我们不认识的表面（已忽略）"),
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(index) = self
            .surfaces
            .iter()
            .position(|s| s.layer.wl_surface() == layer.wl_surface())
        else {
            return;
        };
        let w = NonZeroU32::new(configure.new_size.0).map_or(0, NonZeroU32::get);
        let h = NonZeroU32::new(configure.new_size.1).map_or(0, NonZeroU32::get);
        let first = self.surfaces[index].width == 0;
        self.surfaces[index].width = w;
        self.surfaces[index].height = h;
        println!(
            "[configure] {} {}x{}{}",
            self.surfaces[index].name,
            w,
            h,
            if first { "（首次）" } else { "（尺寸变化）" }
        );
        if w > 0 && h > 0 {
            self.draw(index);
        }
    }
}

impl ShmHandler for Wall {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

// ── 鼠标输入：wl_seat → wl_pointer────────────────────────────
// 手写这两个 Dispatch（**不用** sctk 的 seat 模块）：我们只要指针位置，
// 不需要光标形状/键盘，而 sctk 的 seat 模块会拖上 calloop 那套 handler 注册。
// ── ：`wl_surface.frame` 回调（配速 + 帧间隔取证）──────────────────────────
// 语义：**挂上回调之后不要立刻画下一帧**。合成器在"这一帧真的进了合成/上屏"时把
// `done` 发回来，带着它自己的 msec 时间戳 ⇒
// ① 配速（vsync 对齐，替代固定 sleep）；
// ② 帧间隔分布直接用它算（p50/p95 用**合成器时钟**，不是我们的墙钟）。
impl Dispatch<wl_callback::WlCallback, ()> for Wall {
    fn event(
        state: &mut Self,
        _cb: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { callback_data } = event {
            state.cb_done += 1;
            state.frame_cb = None;
            state.frame_wait_since = None;
            if state.cb_last_ts != 0 {
                let dt = callback_data.wrapping_sub(state.cb_last_ts) as f32;
                if dt > 0.0 && dt < 1000.0 {
                    state.cb_gaps[state.cb_gap_i % state.cb_gaps.len()] = dt;
                    state.cb_gap_i += 1;
                }
            }
            state.cb_last_ts = callback_data;
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_seat::WlSeat, GlobalData> for Wall {
    fn event(
        state: &mut Self,
        seat: &wayland_client::protocol::wl_seat::WlSeat,
        event: wayland_client::protocol::wl_seat::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_seat::{Capability, Event as SeatEvent};
        if let SeatEvent::Capabilities { capabilities } = event {
            let caps = Capability::from_bits_truncate(capabilities.into());
            if caps.contains(Capability::Pointer) && state.pointer.is_none() {
                if let Some(qh) = state.qh.clone() {
                    state.pointer = Some(seat.get_pointer(&qh, ()));
                    eprintln!("[鼠标] 合成器给了 wl_pointer ⇒ 指针位置进场景（g_PointerPosition）");
                }
            }
            if state.mouse_debug {
                eprintln!(
                    "[鼠标诊断] wl_seat.capabilities bits={:#x} pointer={} keyboard={} touch={} ⇒ pointer 已绑={}",
                    u32::from(capabilities),
                    caps.contains(Capability::Pointer),
                    caps.contains(Capability::Keyboard),
                    caps.contains(Capability::Touch),
                    state.pointer.is_some()
                );
            }
        }
    }
}

// ── 全局指针：光标会话（`ext_image_copy_capture`）的事件 ─────────────────────────
// 只消费 `enter/leave/position/hotspot`；**不 attach 缓冲、不 capture**（零帧、零带宽）。
impl Dispatch<cap_src_mgr::ExtOutputImageCaptureSourceManagerV1, GlobalData> for Wall {
    fn event(
        _s: &mut Self,
        _p: &cap_src_mgr::ExtOutputImageCaptureSourceManagerV1,
        _e: cap_src_mgr::Event,
        _d: &GlobalData,
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<cap_mgr::ExtImageCopyCaptureManagerV1, GlobalData> for Wall {
    fn event(
        _s: &mut Self,
        _p: &cap_mgr::ExtImageCopyCaptureManagerV1,
        _e: cap_mgr::Event,
        _d: &GlobalData,
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<cap_source::ExtImageCaptureSourceV1, ()> for Wall {
    fn event(
        _s: &mut Self,
        _p: &cap_source::ExtImageCaptureSourceV1,
        _e: cap_source::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<cap_sess::ExtImageCopyCaptureSessionV1, ()> for Wall {
    fn event(
        state: &mut Self,
        _p: &cap_sess::ExtImageCopyCaptureSessionV1,
        e: cap_sess::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
        use cap_sess::Event as E;
        match e {
            E::BufferSize { width, height } => {
                state.pointer_global_base = (width as f32, height as f32);
            }
            E::Done => {
                state.pointer_global_ready = true;
                eprintln!(
                    "[指针] 全局指针会话就绪：输出缓冲 {}x{}（**零抓帧**，只读光标 position）",
                    state.pointer_global_base.0, state.pointer_global_base.1
                );
            }
            E::Stopped => {
                eprintln!("[指针] 合成器停了全局指针会话 ⇒ 退回只用 wl_pointer");
                state.pointer_global_ok = false;
                state.cap_cursor = None;
                state.cap_session = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<cap_cursor::ExtImageCopyCaptureCursorSessionV1, ()> for Wall {
    fn event(
        state: &mut Self,
        _p: &cap_cursor::ExtImageCopyCaptureCursorSessionV1,
        e: cap_cursor::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
        use cap_cursor::Event as E;
        match e {
            E::Position { x, y } => {
                state.pointer_global_events += 1;
                // `normalize_global` 是 `Wall` 的关联函数（它就写在 `impl Wall` 里）
                let (nx, ny) = Self::normalize_global(x as f32, y as f32, state.pointer_global_base);
                let now = std::time::Instant::now();
                // 控制面覆盖窗口内**不写入**（但照样计数/计数对账值 ⇒ 一眼能看出"被覆盖"）。
                let overridden = state
                    .mouse_override_until
                    .map(|t| now < t)
                    .unwrap_or(false);
                if !overridden {
                    // 写**同一个** `self.mouse`（单一出口）：`tick_scene` 每帧统一推给渲染器。
                    state.mouse = Some((nx, ny));
                    state.mouse_src = "cursor_session";
                }
                state.mouse_cs = Some((nx, ny)); // 对账用（另一条路写 mouse_wl）
                // 限流日志：指针动起来是 ~300 条/秒，无条件打印会把 journal 刷爆
                let loud = state
                    .mouse_log_at
                    .map(|t| now.duration_since(t).as_secs_f32() > 2.0)
                    .unwrap_or(true);
                if loud {
                    state.mouse_log_at = Some(now);
                    eprintln!(
                        "[指针] 全局 {x},{y} ⇒ 归一到 {nx:.4},{ny:.4}（累计 {} 条{}）",
                        state.pointer_global_events,
                        if overridden { "，控制面覆盖中" } else { "" }
                    );
                }
            }
            E::Enter => state.pointer_global_enter += 1,
            E::Leave => state.pointer_global_leave += 1,
            E::Hotspot { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_pointer::WlPointer, ()> for Wall {
    fn event(
        state: &mut Self,
        _pointer: &wayland_client::protocol::wl_pointer::WlPointer,
        event: wayland_client::protocol::wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_pointer::Event as PEvent;
        // 诊断：**先记账再处理**。`pointer_events` 是"合成器到底发没发"的诚实指标——
        // 不管我们后面能不能换算成坐标，它都 +1。
        state.pointer_events += 1;
        match event {
            PEvent::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.mouse_enter += 1;
                if state.mouse_debug {
                    let ours = state
                        .surfaces
                        .iter()
                        .any(|s| s.layer.wl_surface() == &surface);
                    eprintln!(
                        "[鼠标诊断] enter 表面={:?} 局部 {surface_x:.1},{surface_y:.1} 是我们的表面={ours}（enter #{}/raw #{}）",
                        surface.id(),
                        state.mouse_enter,
                        state.pointer_events
                    );
                }
                state.cursor_surf = Some(surface.clone());
                state.mouse_from_surface(&surface, surface_x, surface_y);
            }
            // ⚠ 这版 `wayland.xml` 的 `motion` **不带 surface**（只有 enter/leave 带）
            // ⇒ 得记住"指针现在在哪个表面上"（`cursor_surf`），否则没法归一化。
            PEvent::Motion {
                surface_x, surface_y, ..
            } => {
                state.mouse_motion += 1;
                if state.mouse_debug && state.mouse_motion <= 3 {
                    // 头三条逐条打（证明 motion 真的在来），之后交给 `mouse_from_surface` 的限流日志
                    eprintln!(
                        "[鼠标诊断] motion #{}/raw #{} 局部 {surface_x:.1},{surface_y:.1}（cursor_surf={}）",
                        state.mouse_motion,
                        state.pointer_events,
                        if state.cursor_surf.is_some() { "有" } else { "无" }
                    );
                }
                if let Some(s) = state.cursor_surf.clone() {
                    state.mouse_from_surface(&s, surface_x, surface_y);
                }
            }
            PEvent::Leave { .. } => {
                state.mouse_leave += 1;
                if state.mouse_debug {
                    eprintln!(
                        "[鼠标诊断] leave #{}/raw #{}（保留最后位置，不跳回原点）",
                        state.mouse_leave, state.pointer_events
                    );
                }
                // 指针离开 ⇒ **保留最后位置**（WE 也是保留：效果停在最后处，
                // 指针跑到窗口上时透视区不应该跳回原点/角落）。
                state.cursor_surf = None;
            }
            other => {
                // 按钮/滚轮/axis/… 我们不用，但诊断时如实打出来（证明路的全貌）
                if state.mouse_debug {
                    eprintln!("[鼠标诊断] 其它指针事件 raw #{}：{other:?}", state.pointer_events);
                }
            }
        }
    }
}

delegate_compositor!(Wall);
delegate_output!(Wall);
delegate_shm!(Wall);
delegate_layer!(Wall);
delegate_registry!(Wall);

// ── 窗口跟踪与"该不该睡觉"的判定（b2）────────────────────────────────
/// 睡眠判定（**纯函数**，好测）：窗口事实 × 睡眠策略 → 该睡的话给出原因
/// 策略（界面下发，三档表见 `shell/src/config.rs`）：
/// `never` = 永不睡
/// `fullscreen` = 默认：真有全屏窗口、或几何上铺满整屏才睡
/// `maximized` = 更积极（省电档）：**最大化也算**
/// 这两档确实不等价：COSMIC 有顶部面板时，最大化窗口的高度是"输出高 - 面板高"，
/// 几何上够不到 `covers_output`（那 ±4px 容差挡不住 32px 面板）⇒ 全屏档不睡、最大化档睡。
/// `legacy_max_flag` = 启动参数 `--sleep-on-maximized`（老开关，保留兼容：不管什么档都算最大化）。
fn sleep_reason(f: &WindowFacts, mode: &str, legacy_max_flag: bool) -> Option<&'static str> {
    if mode == "never" || f.closed || f.minimized {
        return None;
    }
    if f.fullscreen {
        return Some("全屏");
    }
    if f.covers_output {
        return Some("铺满整屏");
    }
    if f.maximized && (mode == "maximized" || legacy_max_flag) {
        return Some("最大化");
    }
    None
}

impl Wall {
    /// 汇总所有窗口事实，判断现在该渲染还是该睡觉
    fn recompute_suppression(&mut self) {
        let mut reason: Option<&'static str> = None;
        let mut who = String::new();
        for (_, facts) in &self.windows {
            let f = facts.lock().unwrap();
            if let Some(r) = sleep_reason(&f, &self.sleep_mode, self.sleep_on_maximized) {
                if reason.is_none() {
                    reason = Some(r);
                    // v2+ 起 COSMIC 不再发 title/app_id（已弃用），拿不到就照实说
                    who = if f.title.is_empty() {
                        if f.app_id.is_empty() { "(标题未提供)".into() } else { f.app_id.clone() }
                    } else {
                        f.title.clone()
                    };
                }
            }
        }
        self.suppressed = reason.is_some() && !no_sleep();
        if self.suppressed != self.last_suppressed {
            self.last_suppressed = self.suppressed;
            // 日志带上"为什么睡"，测试睡眠策略时不用猜（是最大化还是真全屏）
            if self.suppressed {
                println!("[睡眠] 「{who}」{} → 停渲（释放占用）", reason.unwrap_or("?"));
            } else {
                println!("[活跃] 挡屏窗口消失 → 恢复渲染");
            }
            // 关键一刀：真正停止/恢复 ffmpeg 解码（SIGSTOP/SIGCONT），而不是只停提交
            self.sync_decoder();
        }
    }
}

#[cfg(test)]
mod sleep_tests {
    use super::*;

    fn facts(fullscreen: bool, maximized: bool, minimized: bool, covers: bool) -> WindowFacts {
        WindowFacts {
            fullscreen,
            maximized,
            minimized,
            covers_output: covers,
            ..Default::default()
        }
    }

    #[test]
    fn never_mode_never_sleeps() {
        let f = facts(true, true, false, true);
        assert_eq!(sleep_reason(&f, "never", false), None);
    }

    #[test]
    fn minimized_window_does_not_count() {
        let f = facts(true, true, true, true);
        assert_eq!(sleep_reason(&f, "maximized", false), None);
    }

    #[test]
    fn fullscreen_mode_sleeps_on_real_fullscreen_or_covers() {
        assert_eq!(
            sleep_reason(&facts(true, false, false, false), "fullscreen", false),
            Some("全屏")
        );
        assert_eq!(
            sleep_reason(&facts(false, false, false, true), "fullscreen", false),
            Some("铺满整屏")
        );
    }

    #[test]
    fn fullscreen_mode_ignores_plain_maximized() {
        // 光最大化、面板挡着没铺满 ⇒ 默认档不睡（这正是它和 maximized 档的区别）
        assert_eq!(
            sleep_reason(&facts(false, true, false, false), "fullscreen", false),
            None
        );
    }

    #[test]
    fn maximized_mode_sleeps_on_maximized() {
        assert_eq!(
            sleep_reason(&facts(false, true, false, false), "maximized", false),
            Some("最大化")
        );
    }

    #[test]
    fn legacy_flag_still_works_in_any_mode() {
        assert_eq!(
            sleep_reason(&facts(false, true, false, false), "fullscreen", true),
            Some("最大化")
        );
    }
}

impl Dispatch<ct::zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, GlobalData> for Wall {
    fn event(
        state: &mut Wall,
        _: &ct::zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
        event: ct::zcosmic_toplevel_info_v1::Event,
        _: &GlobalData,
        _: &Connection,
        _qh: &QueueHandle<Wall>,
    ) {
        use ct::zcosmic_toplevel_info_v1::Event;
        match event {
            // 协议要求：info 对象的 done 只用来标记"一批状态发完了"
            // 睡眠判定就在这一刻重算 —— 这是"所有窗口状态都一致了"的唯一时刻。
            // 别在每个 State/Geometry 事件里各算一次：一轮广播里，新窗口的首个 State（全 false）
            // 会**先于**老窗口的 maximized 到达，中途重算会得到"没有窗口挡屏"的假象，
            // 于是壁纸醒一下又立刻睡回去（实测踩过：新窗口出现时 睡→醒→睡 连跳）。
            Event::Done => {
                println!("[done] 状态批次（已跟踪 {} 个窗口）", state.windows.len());
                state.recompute_suppression();
            }
            _ => {}
        }
    }
}

impl Dispatch<ct::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, Arc<Mutex<WindowFacts>>>
    for Wall
{
    fn event(
        state: &mut Wall,
        _: &ct::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
        event: ct::zcosmic_toplevel_handle_v1::Event,
        facts: &Arc<Mutex<WindowFacts>>,
        _: &Connection,
        _: &QueueHandle<Wall>,
    ) {
        use ct::zcosmic_toplevel_handle_v1::{Event, State};
        // 只在"窗口关了"这一刻立刻重算（关掉的可能正是挡屏那个）；
        // 其余事件只更新事实，重算统一等 info 的 done（一批发完，见上面的注释）
        let mut changed = false;
        {
            let mut f = facts.lock().unwrap();
            match event {
                Event::Title { title } => f.title = title,
                Event::AppId { app_id } => f.app_id = app_id,
                Event::State { state } => {
                    // 这个事件给的是**完整状态集**（不是增量），所以先清空再设置
                    f.fullscreen = false;
                    f.maximized = false;
                    f.activated = false;
                    f.minimized = false;
                    for raw in &state {
                        match State::try_from(*raw as u32) {
                            Ok(State::Fullscreen) => f.fullscreen = true,
                            Ok(State::Maximized) => f.maximized = true,
                            Ok(State::Activated) => f.activated = true,
                            Ok(State::Minimized) => f.minimized = true,
                            _ => {}
                        }
                    }
                    println!(
                        "[状态] {:<28} fullscreen={:<5} maximized={:<5} activated={:<5} minimized={}",
                        if f.title.is_empty() { f.app_id.clone() } else { f.title.clone() },
                        f.fullscreen,
                        f.maximized,
                        f.activated,
                        f.minimized
                    );
                }
                Event::Geometry {
                    output,
                    x,
                    y,
                    width,
                    height,
                } => {
                    // 铺满整块输出 = 也算"全屏"（有些窗口不报 fullscreen 状态）
                    if let Some((ow, oh)) = state
                        .output_state
                        .info(&output)
                        .and_then(|i| i.logical_size)
                    {
                        f.covers_output =
                            x <= 2 && y <= 2 && width >= ow - 4 && height >= oh - 4;
                    }
                }
                Event::Closed => {
                    f.closed = true;
                    changed = true;
                }
                // Done（这一批说完了）/ 其它：事实已记下，重算交给 info 的 done
                _ => changed = false,
            }
        }
        if changed {
            state.recompute_suppression();
        }
    }
}

// ── 窗口身份：自己处理 ext_foreign_toplevel_list_v1（照 COSMIC client-toolkit）────
impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, GlobalData> for Wall {
    fn event(
        state: &mut Wall,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &GlobalData,
        _: &Connection,
        qh: &QueueHandle<Wall>,
    ) {
        match event {
            // 关键：就在这个事件里向 COSMIC 的 info 对象索取扩展对象（client-toolkit 同样做法）
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                if state.windows.iter().any(|(h, _)| h == &toplevel) {
                    return;
                }
                if let Some(info) = &state.ct_info {
                    let facts = Arc::new(Mutex::new(WindowFacts::default()));
                    // 重新启用：改用完整（未裁剪）XML + 绑 2..=3 后，验证是否还 malformed
                    let _ext = info.get_cosmic_toplevel(&toplevel, qh, facts.clone());
                    state.windows.push((toplevel, facts));
                    println!("[窗口] 开始跟踪（共 {} 个）", state.windows.len());
                }
            }
            ext_foreign_toplevel_list_v1::Event::Finished => println!("[窗口] 列表协议结束"),
            _ => {}
        }
    }

    // 注意：这个宏必须写在 impl 块**内部** —— 它定义的是 Dispatch 的关联函数 event_created_child
    wayland_client::event_created_child!(Wall, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Wall {
    fn event(
        state: &mut Wall,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Wall>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        if let Event::Closed = event {
            state.windows.retain(|(h, _)| h != handle);
            println!("[窗口] 关闭（剩 {} 个）", state.windows.len());
            state.recompute_suppression();
        }
    }
}

// 列表事件里产生的句柄对象，user-data 用 （与上面的 Dispatch 一致）

impl ProvidesRegistryState for Wall {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

impl AsMut<SimpleGlobal<wp_viewporter::WpViewporter, 1>> for Wall {
    fn as_mut(&mut self) -> &mut SimpleGlobal<wp_viewporter::WpViewporter, 1> {
        self.viewporter.as_mut().expect("viewporter 未绑定")
    }
}

impl Dispatch<WpViewport, ()> for Wall {
    fn event(
        _: &mut Wall,
        _: &WpViewport,
        _: wp_viewport::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Wall>,
    ) {
        unreachable!("wp_viewport 在 version 1 没有事件")
    }
}

delegate_simple!(Wall, wp_viewporter::WpViewporter, 1);

#[cfg(test)]
mod app_state_glue_tests {
    use super::*;

    /// 属性覆盖表是**进程内全局**（渲染器的静态表）⇒ 本模块里碰它的用例必须串行，
    /// 否则并行跑的单测会互相踩（表被对方清掉 ⇒ 假红）。
    static PROPS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn props_guard() -> std::sync::MutexGuard<'static, ()> {
        PROPS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 造一份**真实形状**的 app 存档（内容取自本机 `~/.local/share/wallpaper-hub/*.json`）。
    fn fixture_dir(tag: &str) -> std::path::PathBuf {
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/app-state-glue")
            .join(tag);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("建临时目录");
        std::fs::write(
            d.join("prop_overrides.json"),
            r#"{
  "2564205376": {
    "nsfwversion": false,
    "sfwversion": false,
    "xraysize": 0.5,
    "xrayversion": true
  }
}"#,
        )
        .unwrap();
        std::fs::write(d.join("scene_fps.json"), "{\n  \"2564205376\": 60\n}").unwrap();
        d
    }

    /// 「恢复真的写下去了」那一跳：动作序列 → 渲染器的属性覆盖表。
    /// 覆盖表是**纯进程内静态表**（`we_scene::scene::userprop`），不需要 GL / Wayland
    /// ⇒ 单测能直接验。语义必须与 `serve::Cmd::SetProp` 一致（先清空、再逐条写）。
    #[test]
    fn restore_actions_land_in_the_renderer_prop_table() {
        let _g = props_guard();
        use app_state::Action;
        let actions = vec![
            Action::ClearProps,
            Action::SetProp("xrayversion".to_string(), "true".to_string()),
            Action::SetProp("xraysize".to_string(), "0.5".to_string()),
            // 内核**不认识**的属性名照样原样搬（本用例造一个从没见过的名字）
            Action::SetProp("nsfw_wat_".to_string(), "1 0 1".to_string()),
            Action::SetFps(60), // 这一条不属于渲染器 ⇒ 自由函数必须忽略它
        ];
        let touched = apply_actions_to_renderer(&actions);
        assert!(touched, "清表/写属性 ⇒ 必须报'覆盖表被动过'（调用方据此重建渲染器）");
        let got: std::collections::BTreeMap<String, String> =
            we_scene_render::user_props().into_iter().collect();
        assert_eq!(got.get("xrayversion").map(String::as_str), Some("true"));
        assert_eq!(got.get("xraysize").map(String::as_str), Some("0.5"));
        assert_eq!(got.get("nsfw_wat_").map(String::as_str), Some("1 0 1"));
        assert_eq!(got.len(), 3, "清表后只剩这次恢复写进去的（顺序无关，比较集合）");

        // `ClearProps` 的语义：换场景时上一条的属性必须被清掉（界面侧补发也是 clear=true）
        apply_actions_to_renderer(&[Action::ClearProps, Action::SetFps(60)]);
        assert!(
            we_scene_render::user_props().is_empty(),
            "只清空不写属性 ⇒ 表里一条不剩"
        );

        // 只有 fps 动作（存档里这条场景没记过属性？不会——但**语义上**必须一个属性都不碰）
        we_scene_render::set_user_prop("keep_me", "true");
        let touched = apply_actions_to_renderer(&[Action::SetFps(30)]);
        assert!(!touched, "只有 fps 动作 ⇒ 覆盖表没被动过");
        let got: std::collections::BTreeMap<String, String> =
            we_scene_render::user_props().into_iter().collect();
        assert_eq!(got.get("keep_me").map(String::as_str), Some("true"));
        assert_eq!(got.len(), 1, "不许凭空多出/少掉条目");
        we_scene_render::clear_user_props(); // 别把表留给同进程里别的用例
    }

    /// 开关关 ⇒ 内核连存档都不读：决策层必须是 `Off`（零动作），执行那一跳也就不会被调到。
    /// （"逐字节旧行为"的完整比对在 `app_state::tests` 里，这里只钉住 main.rs 这一侧的接线。）
    #[test]
    fn main_side_glue_asks_app_state_and_does_nothing_when_off() {
        assert_eq!(
            app_state::decide_in(std::path::Path::new("/nonexistent"), false, "2564205376"),
            app_state::Decision::Off
        );
        // 关的时候 `log_line` 是 None ⇒ main.rs 那条 `if let Some(line)` 不会打印任何东西
        assert_eq!(
            app_state::decide_in(std::path::Path::new("/nonexistent"), false, "2564205376")
                .log_line("内核启动"),
            None
        );
    }

    /// 开关关 ⇒ **运行时状态逐字节不变**（实测，不是推理）。
    /// 拿内核真正用的那串函数跑一遍（`decide_in` → `log_line` → `actions` 落地，`fps` 用局部变量
    /// 代表 `Wall::fps`），存档是**真的**写在磁盘上、内容就是这条场景的属性与 fps。开关关时必须：
    /// 覆盖表哨兵值还在（一条不多一条不少）、fps 还是 drop-in 的那个数、日志一个字不打、
    /// 动作序列逐字节等于旧内核（空）。对照组（同一条存档、开关开）必须真的改变两样东西。
    #[test]
    fn off_switch_leaves_runtime_state_byte_for_byte_unchanged() {
        let _g = props_guard();
        let dir = fixture_dir("off_switch_runtime");
        let scene = "2564205376";

        // 旧内核留下的运行时状态：一张有哨兵的覆盖表 + drop-in 的 WE_SCENE_FPS=60
        we_scene_render::clear_user_props();
        we_scene_render::set_user_prop("sentinel_should_survive", "yes");
        let table_before = we_scene_render::user_props();
        let fps_before: u64 = 60;

        // 内核那一侧的执行序（与 `Wall::restore_app_state` 逐行同构）——
        let mut fps = fps_before;
        let mut printed: Vec<String> = Vec::new();
        let dec = app_state::decide_in(&dir, false, scene);
        if let Some(line) = dec.log_line("apply scene /x/2564205376") {
            printed.push(line);
        }
        let actions = dec.actions();
        if apply_actions_to_renderer(&actions) {
            panic!("开关关：覆盖表不该被动过");
        }
        for a in &actions {
            if let app_state::Action::SetFps(f) = a {
                fps = *f;
            }
        }

        assert_eq!(
            we_scene_render::user_props(),
            table_before,
            "开关关：覆盖表必须逐条不变（哨兵还在、没被清、没被写）"
        );
        assert_eq!(fps, fps_before, "开关关：fps 必须还是 drop-in 的 60");
        assert!(printed.is_empty(), "开关关：一个字都不许打（旧内核 stdout 就是这样）");
        assert_eq!(
            app_state::actions_bytes(&actions),
            "",
            "开关关：动作序列必须逐字节等于旧内核（空）"
        );

        // 对照组：同一条存档、开关开 ⇒ 两样都必须真的变 ——
        we_scene_render::clear_user_props();
        we_scene_render::set_user_prop("sentinel_should_survive", "yes");
        let dec_on = app_state::decide_in(&dir, true, scene);
        assert!(
            matches!(dec_on, app_state::Decision::Apply(_)),
            "开的时候必须真的恢复：{dec_on:?}"
        );
        let printed_on = dec_on.log_line("apply scene /x/2564205376");
        let actions_on = dec_on.actions();
        if apply_actions_to_renderer(&actions_on) {
            // 与内核一致：覆盖表动过 ⇒ 重建渲染器（这里没法建，只记下事实）
        }
        for a in &actions_on {
            if let app_state::Action::SetFps(f) = a {
                fps = *f;
            }
        }
        let table_on = we_scene_render::user_props();
        assert_ne!(table_on, table_before, "开的时候覆盖表必须真的变（否则比对是空对空）");
        assert!(
            !table_on.iter().any(|(k, _)| k == "sentinel_should_survive"),
            "清表语义：上一条场景/哨兵不许残留"
        );
        assert_eq!(table_on.len(), 4, "只该有这条场景的 4 条属性");
        assert_eq!(fps, 60, "fps 按存档恢复成 60");
        let line = printed_on.expect("开的时候必须留一行证据");
        assert!(
            line.contains("场景=2564205376 属性=4 条 fps=60（来源=app 存档）"),
            "证据行的格式与需求给的一致：{line}"
        );
        we_scene_render::clear_user_props();
    }
}

/// 没有 fd 可等（`prepare_read` 拿不到 guard）时，这一圈该做什么。
/// 的回归点：`prepare_read` 返回 None 的含义是「队列里**已有**事件，先分发」
/// 所以带截止时间（`--seconds`，一次性 CLI 用的就是这条路）的运行**必须**先把它们
/// `dispatch_pending` 掉，否则事件永远发不出去、下一圈依旧拿不到 guard，主循环退化成
/// 10 ms 心跳（实测 462 圈 `other 10.1ms`、frames 卡在 1）。常驻模式那边控制面 fd
/// 让 `fds` 非空，走不到这条分支，所以只有一次性模式中招。
/// 抽成小函数只为让这个"选哪条"变成可单测的判据（见 `scene_sched_tests`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoFdAction {
    /// 先分发队列里已有的事件，再让出 CPU（带截止时间的运行）。
    DispatchPending,
    /// 阻塞等到下一个事件（常驻模式，"零唤醒"形态）。
    BlockingDispatch,
}

/// what one lap of the 0-copy video path does about a submit that may still
/// be in flight. Pure on purpose: this table is the whole memory-safety argument.
///
/// * `Submit` - nothing outstanding, decode/draw/submit as usual;
///
/// * `Wait` - still out there, keep the main loop running (never count it as a failure);
///
/// * `Release` - it came back, the held decoder frame may now be reused;
///
/// * `ReleaseErr` - it came back with an error: release, then count one failure
/// (3 in a row falls back to the ffmpeg/shm path).
#[derive(Debug, PartialEq, Eq)]
enum VideoCommitStep {
    Submit,
    Wait,
    Release,
    ReleaseErr(String),
}

fn video_commit_step(held: bool, collected: Option<Result<(), String>>) -> VideoCommitStep {
    match (held, collected) {
        (false, _) => VideoCommitStep::Submit,
        (true, None) => VideoCommitStep::Wait,
        (true, Some(Ok(()))) => VideoCommitStep::Release,
        (true, Some(Err(e))) => VideoCommitStep::ReleaseErr(e),
    }
}

fn no_fd_action(has_deadline: bool) -> NoFdAction {
    if has_deadline {
        NoFdAction::DispatchPending
    } else {
        NoFdAction::BlockingDispatch
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    /// 等回调期间**绝不**能拿 `poll(0)` 去空转（实测过 938 万轮/3 秒）。
    #[test]
    fn waiting_for_the_frame_callback_never_polls_zero() {
        let ms = Duration::from_millis;
        // fps 截止已过（due=0）但回调还没回来 ⇒ 等回调
        assert_eq!(poll_wait_while_gated(Some(ms(60)), Some(ms(0))), Some(ms(60)));
        // 没有闸门 ⇒ 照旧按 fps 截止
        assert_eq!(poll_wait_while_gated(None, Some(ms(16))), Some(ms(16)));
        // 闸门到点 ⇒ 放行，按 fps 截止
        assert_eq!(poll_wait_while_gated(Some(ms(0)), Some(ms(16))), Some(ms(16)));
        // 两边都没有 ⇒ 永久阻塞（静止时零唤醒）
        assert_eq!(poll_wait_while_gated(None, None), None);
    }
}

mod scene_sched_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// fps 被夹到 >=1 ⇒ 周期永不为 0（0 会让 poll 超时退化成立刻返回 = 空转）。
    #[test]
    fn period_is_never_zero() {
        assert_eq!(scene_period(0), Duration::from_millis(1000));
        assert_eq!(scene_period(1), Duration::from_millis(1000));
        assert_eq!(scene_period(60), Duration::from_millis(16));
    }

    /// 出一条帧之后：没到期就在 prev 上累加（吸收工作时间）；落后太多就对齐到现在。
    #[test]
    fn deadline_absorbs_work_and_does_not_catch_up() {
        let now = Instant::now();
        let period = scene_period(60);
        let prev = now - Duration::from_millis(10);
        assert_eq!(next_scene_deadline(Some(prev), now, period), prev + period);
        let stale = now - Duration::from_secs(5);
        assert_eq!(next_scene_deadline(Some(stale), now, period), now + period);
        assert_eq!(next_scene_deadline(None, now, period), now + period);
    }

    /// 的根因回归：没有表面时 scene_next 停在过去 ⇒ 退避后必须**还没到期**。
    #[test]
    fn defer_moves_an_overdue_deadline_into_the_future() {
        let now = Instant::now();
        let overdue = Some(now - Duration::from_secs(3));
        let next = next_scene_deadline(overdue, now, scene_period(60));
        assert!(next > now, "退避后必须还没到期，否则主循环继续 poll(0) 空转");
        assert_eq!(next - now, Duration::from_millis(16));
    }

    /// 的回归：没有 fd 可等时，带 `--seconds` 的运行**必须**先分发已排队事件
    /// 不许"只睡 10ms"。睡掉那一版实测 462 圈 `other 10.1ms`、frames 停在 1（场景一直不动）。
    #[test]
    fn no_fd_with_deadline_dispatches_pending_events() {
        assert_eq!(
            no_fd_action(true),
            NoFdAction::DispatchPending,
            "带截止时间只睡不分发 = 事件永远发不出去，主循环空转"
        );
        assert_eq!(no_fd_action(false), NoFdAction::BlockingDispatch);
    }

    /// the 0-copy video path holds the decoder frame (and refuses to draw) while a
    /// submit is in flight. The table below is what makes "late submit" stop being an error:
    /// only a submit that actually failed increments `video_gpu_errs`, and 3 of those still
    /// mean "fall back to the ffmpeg/shm path".
    #[test]
    fn video_commit_step_table() {
        assert_eq!(video_commit_step(false, None), VideoCommitStep::Submit);
        assert_eq!(
            video_commit_step(false, Some(Ok(()))),
            VideoCommitStep::Submit,
            "nothing held: a collected result is not a reason to skip this frame"
        );
        assert_eq!(
            video_commit_step(true, None),
            VideoCommitStep::Wait,
            "held and not back yet: keep the main loop running, do not touch the decoder"
        );
        assert_eq!(video_commit_step(true, Some(Ok(()))), VideoCommitStep::Release);
        assert_eq!(
            video_commit_step(true, Some(Err("io".into()))),
            VideoCommitStep::ReleaseErr("io".into()),
            "a failed submit releases the frame, then counts one failure"
        );
    }
}
