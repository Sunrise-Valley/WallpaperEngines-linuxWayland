// SPDX-License-Identifier: MIT
//! EGL（surfaceless）+ GLES 上下文 + FBO + 画四边形 + 回读。
//! 实测结论（见 `../README.md`）：`eglGetDisplay(EGL_DEFAULT_DISPLAY)` + `EGL_KHR_surfaceless_context`
//! 能拿到 **radeonsi 真 GPU** 的 ES 3.2 上下文；`eglChooseConfig` 用"无约束属性"最稳
//! （自作聪明加 RGB 尺寸反而会 `EGL_BAD_ATTRIBUTE`）。

use glow::HasContext;
use khronos_egl as egl;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

thread_local! {
    /// `update_layer_mesh` 的复用缓冲：每帧每网格层的顶点装配结果（`[x,y,z,u,v]` × N）。
    /// 以前每次都 `Vec::with_capacity` 新开一块（每帧每层至少一次堆分配 + 释放）。
    static MESH_SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// 一块 dmabuf 平面（导入成 GL 贴图用）。数据由 `c/av_decode.c` 填。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBufPlane {
    pub fd: i32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

/// 4×4 矩阵转置（列主序数组，`m[c*4+r]`）。**上传矩阵的每一处都要过它**。
/// 为什么：`shader::PRELUDE` 里 `mul` 已经改成 `(b) * (a)`（对齐 HLSL 的操作数顺序，
/// 见那里的长注释）。WE 的 shader 用 `mul(v, M)` 做**行向量 × 矩阵**，而 GLSL 的 `M * v`
/// 用的是"数学矩阵"，和我们 `Mat4` 里存的列主序数组差一次转置 ⇒ 上传时补上这一次转置，
/// 两处互相抵消：**"引擎上传的矩阵"那条路逐位不变**，只有 shader 内部自己算出来的矩阵被修对。
/// 忘了转置的后果：MVP/模型矩阵整体转置，全库画面错乱（人物/背景全错位）。
pub fn t4(m: &[f32; 16]) -> [f32; 16] {
    let mut o = [0.0f32; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[c * 4 + r] = m[r * 4 + c];
        }
    }
    o
}

/// 3×3 版本（列主序 `m[c*3+r]`），语义与 [`t4`] 完全相同（`g_NormalModelMatrix` 用）。
pub fn t3(m: &[f32; 9]) -> [f32; 9] {
    let mut o = [0.0f32; 9];
    for c in 0..3 {
        for r in 0..3 {
            o[c * 3 + r] = m[r * 3 + c];
        }
    }
    o
}

/// 场景灯 + 光照环境色 —— **每帧下发一次**的引擎 uniform 组。
/// 打包口径（**逐字段出处**见 `lib.rs::lights_from_scene` 与 `we_scene::scene::anim`）：
///
/// * `position[4*3]` ← `g_LightsPosition[4]`（`vec3`）—— 灯的**世界位置**（场景单位）
///
/// * `color_radius[4*4]` ← `g_LightsColorRadius[4]`（`vec4`）—— `.rgb = color*intensity`、`.w = radius`
///
/// * `direction_type[4*4]` ← `g_LightsDirectionType[4]`（`vec4`）—— `.xyz` 朝向、`.w` 类型（**未用槽 = -1**）
///
/// * `cone_exponent[4*4]` ← `g_LightsConeExponent[4]`（`vec4`）—— `(innercone_cos, outercone_cos, exponent, 0)`
///
/// * `cast_shadow[4]` ← `g_LightsCastShadow[4]`（`float`）—— 1/0
///
/// * `color_premultiplied[3*4]` ← `g_LightsColorPremultiplied[3]`（`vec4`）—— **rgb = color*intensity*radius²**（前 3 盏）
///
/// * `ambient` ← `g_LightAmbientColor`（`general.ambientcolor`）
///
/// * `skylight` ← `g_LightSkylightColor`（`general.skylightcolor`）
#[derive(Debug, Clone, Copy)]
pub struct LightUniforms {
    pub position: [f32; 12],
    pub color_radius: [f32; 16],
    pub direction_type: [f32; 16],
    pub cone_exponent: [f32; 16],
    pub cast_shadow: [f32; 4],
    pub color_premultiplied: [f32; 12],
    pub ambient: [f32; 3],
    pub skylight: [f32; 3],
}

impl Default for LightUniforms {
    /// "没有灯"的**中性**状态：所有槽 `direction_type[i].w = -1`（= OWE 的未用槽标记，
    /// `UniformSource.cpp:856-857`），其余全 0 ⇒ `PerformLighting_V1` 直接 `continue`。
    fn default() -> Self {
        let mut d = [0.0f32; 16];
        for i in 0..4 {
            d[i * 4 + 3] = -1.0;
        }
        LightUniforms {
            position: [0.0; 12],
            color_radius: [0.0; 16],
            direction_type: d,
            cone_exponent: [0.0; 16],
            cast_shadow: [0.0; 4],
            color_premultiplied: [0.0; 12],
            ambient: [0.0; 3],
            skylight: [0.0; 3],
        }
    }
}

impl LightUniforms {
    /// 4 盏灯的槽位全部按 OWE 的默认值收尾（未用槽 `w = -1`）。
    pub fn finish(&mut self, used: usize) {
        for i in used..4 {
            self.direction_type[i * 4 + 3] = -1.0;
        }
    }
}

/// 这一趟的顶点几何是不是「裸 NDC」（⇒ 要用 V 反向的四边形）。
/// 原判据 = 链接后 `g_ModelViewProjectionMatrix` 变 inactive（说明 shader 走的是
/// `gl_Position = vec4(a_Position, 1.0)` 那一支，a_Position 本身就是屏幕 NDC）。
/// 补一条**例外**：WE 自己的 shader 有**第二条**把四边形摆进屏幕的路 ——
/// `#if LIGHTING` 下 `gl_Position = mul(worldPos, M_VP)`，而 `M_VP` 被
/// `#define M_VP g_ViewProjectionMatrix` 绑定（官方 `assets/shaders/genericimage4.vert:282/415/451`、
/// `genericimage2.vert:57/143-148`、`generic.vert:7`）。这种趟里 MVP **同样是 inactive**
/// （那一支整个被编译掉），但 a_Position 已经不是 NDC 了 ⇒ 不能再套 V 反向四边形，
/// 否则整层**上下镜像**（实测 `3571777169` 的人物层：材质 `LIGHTING=1` 生效后 → 人物倒过来）。
/// 判据是**顶点源码自己有没有引用 view-projection**（数据驱动，不看场景 id / 对象名 / shader 名）。
/// Does this shader read any per-frame uniform (time / pointer / audio / lights / morph)?
/// `vs_src` / `fs_src` are the final sources handed to the driver (macros expanded), so a
/// substring scan sees exactly what gets compiled. Deliberately conservative: a false
/// positive only loses a few cached passes, a false negative freezes a moving layer.
pub fn dynamic_uniform(vs_src: &str, fs_src: &str) -> &'static str {
    const TOKENS: [&str; 4] = [
        "g_Time",
        "g_PointerPosition",
        "g_AudioSpectrum",
        "g_ParallaxPosition",
    ];
    TOKENS
        .iter()
        .copied()
        .find(|t| vs_src.contains(t) || fs_src.contains(t))
        .unwrap_or("")
}


/// Does this shader read the light uniforms (g_Lights*)?
/// Kept apart from the per-frame token list because light uniforms are constant unless a
/// light source itself carries an animation curve (see `light_uniforms`).
pub fn layer_reads_lights(vs_src: &str, fs_src: &str) -> bool {
    vs_src.contains("g_Lights") || fs_src.contains("g_Lights")
}

fn wants_raw_position(loc_mvp: Option<glow::UniformLocation>, vs_src: &str) -> bool {
    loc_mvp.is_none()
        && !vs_src.contains("g_ViewProjectionMatrix")
        && !vs_src.contains("g_AltViewProjectionMatrix")
}

/// 导入出来的 dmabuf 图像（**自己持有裸 `EGLImage`**，不用 khronos-egl 的 `Image`：
/// 那个包装把属性表当 8 字节的 `Attrib` 传，和 EGL 的 `EGLint*` 对不上，见 `import_dma_buf`）。
#[derive(Debug, Clone, Copy)]
pub struct DmaBufImage(pub *mut std::ffi::c_void);


// bounded frame commit ------------------------------------
// Field measurement : SIGSTOP cosmic-comp for 2.5 s and the
// main-loop lap prints `poll 11.4 | scene 2554.9 [build 0.0 draw 3.4 swap 2551.5]`
// => the recorded 8 s stalls are locked inside `eglSwapBuffers`, not page faults, not decode,
// not poll.
// Why it waits forever: in window mode eglSwapBuffers waits for the compositor's frame
// callback (swap interval 1). When the compositor reconfigures its output (23/23 recorded
// stalls align with that second) our layer surface is invalidated, the callback never comes,
// and the call has no timeout => the main loop stops and at 8 s the watchdog can only restart
// the process (cost: scene rebuild + animation back at frame 1).
// Fix: hand the swap to a dedicated `we-commit` thread and wait at most `deadline_ms` for it.
// EGL current is per-thread, so the main thread releases it (`eglMakeCurrent(None)`) before
// handing off and takes it back in `ensure_current`.
// * normal path: two extra eglMakeCurrent per frame, pixels untouched;
// * compositor not presenting: the main loop keeps polling / dispatching / answering the
// control socket instead of terminating itself;
// * when the late swap finally returns, the next frame continues (scene time is wall clock).

/// Result of one bounded commit attempt.
pub enum CommitOutcome {
    /// eglSwapBuffers returned within the deadline (inner Err = the call itself failed).
    Done(Result<(), String>),
    /// Deadline hit: the main loop stops waiting. The job stays on the books and the next
    /// call collects its result.
    Pending,
    /// The previous job is still in flight, so nothing was requested this time.
    StillPending,
}

enum CommitState {
    Idle,
    Requested,
    Busy,
    Finished(Result<(), String>, f64),
    Stop,
}

struct CommitSlot {
    st: std::sync::Mutex<CommitState>,
    cv: std::sync::Condvar,
}

/// Handle to the per-EGL-window commit thread.
pub struct Committer {
    slot: Arc<CommitSlot>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Committer {
    fn spawn(dpy: usize, surf: usize, ctx: usize) -> Committer {
        let slot = Arc::new(CommitSlot {
            st: std::sync::Mutex::new(CommitState::Idle),
            cv: std::sync::Condvar::new(),
        });
        let s2 = slot.clone();
        let thread = std::thread::Builder::new()
            .name("we-commit".into())
            .spawn(move || commit_loop(s2, dpy, surf, ctx))
            .ok();
        Committer { slot, thread }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CommitState> {
        self.slot.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask the thread to finish. Returns false when it is still blocked inside `eglSwapBuffers`:
    /// then the caller must NOT tear down the EGL surface / wl_egl_window, because the thread
    /// would touch freed memory the moment it returns.
    fn stop_thread(&mut self) -> bool {
        {
            let mut st = self.lock();
            match *st {
                CommitState::Busy | CommitState::Requested => return false,
                _ => *st = CommitState::Stop,
            }
        }
        self.slot.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        true
    }
}

fn wait_request(slot: &CommitSlot) -> bool {
    let mut st = slot.st.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match *st {
            CommitState::Requested => {
                *st = CommitState::Busy;
                return true;
            }
            CommitState::Stop => return false,
            _ => st = slot.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
        }
    }
}

fn commit_loop(slot: Arc<CommitSlot>, dpy: usize, surf: usize, ctx: usize) {
    // A second DynamicInstance: dlopen of libEGL is refcounted and eglBindAPI is per-thread.
    let lib = match unsafe {
        egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
    } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[commit] thread could not load libEGL: {e}");
            return;
        }
    };
    let _ = lib.bind_api(egl::OPENGL_API);
    let display = unsafe { egl::Display::from_ptr(dpy as *mut std::ffi::c_void) };
    let surface = unsafe { egl::Surface::from_ptr(surf as *mut std::ffi::c_void) };
    let context = unsafe { egl::Context::from_ptr(ctx as *mut std::ffi::c_void) };
    while wait_request(&slot) {
        let t0 = std::time::Instant::now();
        let r = match lib.make_current(display, Some(surface), Some(surface), Some(context)) {
            Ok(()) => egl_swap_checked(&lib, display, surface),
            Err(e) => Err(format!("commit thread eglMakeCurrent failed: {e}")),
        };
        let _ = lib.make_current(display, None, None, None);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut st = slot.st.lock().unwrap_or_else(|e| e.into_inner());
        *st = CommitState::Finished(r, ms);
        slot.cv.notify_all();
    }
}

pub struct Gl {
    pub gl: glow::Context,
    pub fbo: glow::Framebuffer,
    pub color: glow::Texture,
    pub w: u32,
    pub h: u32,
    pub vendor: String,
    pub renderer: String,
    pub version: String,
    // EGL 的对象要一直活着
    _lib: egl::DynamicInstance<egl::EGL1_5>,
    _display: egl::Display,
    _context: egl::Context,
    /// **0 拷贝模式**下的窗口表面（EGL wayland 平台 + `wl_egl_window`）。
    /// None = 旧的 surfaceless + FBO + `glReadPixels` 回读那条路。
    egl_surface: Option<egl::Surface>,
    /// 0 拷贝模式下的 `wl_egl_window*` + 它的销毁函数（libwayland-egl 是 dlopen 来的）
    wl_window: Option<(*mut std::ffi::c_void, unsafe extern "C" fn(*mut std::ffi::c_void))>,
    /// 场景灯 + 环境色（每帧由 `SceneRenderer::draw_frame` 按本帧场景时间算好后下发）。
    /// **为什么放在 `Gl` 上而不是 `DrawCtx` 里**：灯是**场景级**的（与对象无关），
    /// 按对象算等于每帧把同一组值重算上百次；而 `DrawCtx` 构造点有 5 处，
    /// 每处都要多带一个字段纯属噪音。`None` ⇒ 本帧没有灯（`draw_pass` 什么都不做）。
    lights: Option<LightUniforms>,
    /// the swap runs on this thread instead of the caller's.
    committer: Option<Committer>,
    /// Is the EGL context current on the calling thread right now? The commit thread takes it
    /// away for the duration of eglSwapBuffers, so the caller re-binds lazily.
    current_here: AtomicBool,
    /// Wall time of the last collected swap (ms); 0 = nothing collected yet.
    last_swap_ms: AtomicU64,
    /// 逐 pass GPU 计时查询池（`WE_PROFILE_PASSES`）。默认一个查询都不建、一次都不 begin
    /// ⇒ 对默认路径零影响（见 [`PassQ`]）。
    pt: std::sync::Mutex<PassQ>,
}

/// 逐 pass 的 `GL_TIME_ELAPSED` 查询池。
///
/// # 为什么不用 `glFinish` 夹每趟（`WE_PASS_GPU` 那条老路）
/// `glFinish` 把流水线**逐趟打断** ⇒ 量到的是"串行化之后的单趟时间"，与真实帧时间不是一回事
/// （老路自己也打了"串行代价偏高"）。timer query 是**异步**的：begin/end 只是往命令流里插两笔，
/// 结果几帧之后才回读（`QUERY_RESULT_AVAILABLE`），所以既不夹断流水、也不改像素。
///
/// # 环形槽
/// `PT_SLOTS` 个槽轮换：本帧写槽 `cur`，回读**最早**那个"结果已可用"的槽。槽在被回读之前
/// 不会被再次 begin ⇒ 绝不覆盖未回读的结果。所有槽都在飞行中（GPU 落后 ≥6 帧）⇒ 本帧不采样。
pub struct PassQ {
    slots: Vec<Vec<glow::Query>>,
    /// 每张查询对应的**趟下标**（`pt_begin(pi)` 记下来）。
    /// 为什么必须记：趟会被跳过（`fuse_skip` / `bake_skip` / static cache），
    /// 只按"这一帧的第几个查询"当趟号，报表就会把时间贴到被跳过的趟上
    /// （实测：融合趟 4.56 ms 被贴到 `#56`，而 #56 是 skip=true 的旧趟）。
    idx: Vec<Vec<usize>>,
    counts: Vec<usize>,
    cur: usize,
    cursor: usize,
    /// 这个槽本帧是否在采样（false = 这一帧整个跳过，`pt_begin`/`pt_end` 什么都不做）
    active: bool,
    /// 查询创建失败（驱动不支持）⇒ 永久停用，不再刷错误
    dead: bool,
}

const PT_SLOTS: usize = 6;
const PT_MAX_PASSES: usize = 1024;

impl Default for PassQ {
    fn default() -> Self {
        PassQ {
            slots: vec![Vec::new(); PT_SLOTS],
            idx: vec![Vec::new(); PT_SLOTS],
            counts: vec![0; PT_SLOTS],
            cur: PT_SLOTS - 1,
            cursor: 0,
            active: false,
            dead: false,
        }
    }
}

impl Gl {
    pub fn new(w: u32, h: u32) -> Result<Gl, String> {
        // 无 root 提取出来的 libEGL 通过 LD_LIBRARY_PATH 找得到（裸名字即可，别写路径）
        let lib = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
        }
        .map_err(|e| format!("加载 libEGL.so.1 失败（LD_LIBRARY_PATH 设了吗）：{e}"))?;
        let display = unsafe { lib.get_display(egl::DEFAULT_DISPLAY) }
            .ok_or_else(|| "eglGetDisplay 失败".to_string())?;
        let (maj, min) = lib
            .initialize(display)
            .map_err(|e| format!("eglInitialize 失败: {e}"))?;
        // 用**桌面 OpenGL**（不是 ES）：WE 的 shader 是 GLSL 1.20 方言，
        // 里面有 `const float x = someInt - 1;` 这种隐式 int→float ——
        // GLSL **ES 1.00 禁止**，桌面 1.20 允许。EGL 报的 CLIENT_APIS 里两者都有。
        lib.bind_api(egl::OPENGL_API)
            .map_err(|e| format!("eglBindAPI(OpenGL) 失败: {e}"))?;
        // 无约束属性最稳
        let config = lib
            .choose_first_config(display, &[egl::NONE])
            .map_err(|e| format!("eglChooseConfig 失败: {e}"))?
            .ok_or_else(|| "没有可用的 EGLConfig".to_string())?;
        // 不指定 CLIENT_VERSION ⇒ 拿到兼容档的桌面上下文（能编 #version 120）
        let context = lib
            .create_context(display, config, None, &[egl::NONE])
            .map_err(|e| format!("eglCreateContext 失败: {e}"))?;
        lib.make_current(display, None, None, Some(context))
            .map_err(|e| format!("eglMakeCurrent(surfaceless) 失败: {e}"))?;

        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                lib.get_proc_address(s)
                    .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
            })
        };

        unsafe {
            let get = |e: u32| gl.get_parameter_string(e);
            let vendor = get(glow::VENDOR);
            let renderer = get(glow::RENDERER);
            let version = get(glow::VERSION);

            let color = gl
                .create_texture()
                .map_err(|e| format!("create_texture: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(color));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            let fbo = gl
                .create_framebuffer()
                .map_err(|e| format!("create_framebuffer: {e}"))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(color),
                0,
            );
            let st = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if st != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!("FBO 不完整: 0x{st:x}"));
            }
            gl.viewport(0, 0, w as i32, h as i32);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);

            println!("  EGL {maj}.{min} ✓  FBO {w}x{h} 完整 ✓");
            Ok(Gl {
                gl,
                fbo,
                color,
                w,
                h,
                vendor,
                renderer,
                version,
                _lib: lib,
                _display: display,
                _context: context,
                egl_surface: None,
                wl_window: None,
                lights: None,
                committer: None,
                current_here: AtomicBool::new(true),
                last_swap_ms: AtomicU64::new(0),
                pt: std::sync::Mutex::new(PassQ::default()),
            })
        }
    }

    /// 清屏：清**当前绑定的那个**帧缓冲（`bind_target` 刚设好的那个）。
    /// 这里曾经硬绑 `self.fbo` —— 0 拷贝窗口模式下那是个**没有附件的 1x1 占位 FBO**
    /// （`open_window` 里只为"免得把 fbo 弄成 Option"建的），于是：
///
    /// * 每帧每一次 `clear` 都产生 GL_INVALID_FRAMEBUFFER_OPERATION (0x506)，
    /// 标志粘住，由 `draw_frame` 开头那次读错误打出来，看着像"某一趟失败"；
///
    /// * 真正该清的 **FBO 0 和每个中间 RT 一个都没清**（画面看着正常，是因为每趟都是
    /// 全屏四边形互相覆盖，把这个洞盖住了）。
    /// 两种模式的绑定差异只允许在 `bind_target` 里判断一次 —— clear 里再来一份就必然走偏。
    pub fn clear(&self, r: f32, g: f32, b: f32, a: f32) {
        unsafe {
            self.gl.clear_color(r, g, b, a);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    /// 读**窗口当前画面**（默认帧缓冲 FBO 0 的后缓冲）—— **0 拷贝窗口模式专用**。
    /// ⚠ 不能用 `read_pixels`：它绑的是那个**没有附件的 1x1 占位 FBO**（窗口模式下
    /// 它只是为了让调用点少写个 Option），读出来是垃圾/全 0。
    /// ⚠ **前提：必须紧跟在一次"画满整屏的 draw"之后、`swap` 之前调用**（`draw_frame` 开头
    /// 会用场景 clearcolor 清整屏，所以画完即"每个像素都已定义"）。swap 之后后缓冲内容按
    /// GL/EGL 规定变成**未定义** —— 那时读回来是空的，不是"读错了 FBO"
    /// （`we-ctl dump` 曾因此在 0 拷贝下出全 0 帧：它读的是 1x1 占位 FBO，见 `render_to` 的注释）。
    pub fn read_pixels_window(&self) -> Vec<u8> {
        let mut buf = vec![0u8; (self.w * self.h * 4) as usize];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            // 默认帧缓冲的读缓冲显式钉到后缓冲（默认就是 BACK；显式设一次免得被别处的
            // read_buffer 状态带偏 —— 读错缓冲同样只会得到全 0）。
            self.gl.read_buffer(glow::BACK);
            self.gl.read_pixels(
                0,
                0,
                self.w as i32,
                self.h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
        }
        buf
    }

    /// 与 `read_pixels_window` 同一条路（FBO 0 的后缓冲），但写进调用方的缓冲。
    /// 为什么要有它：转储每帧要读 2560x1440x4 = 14.7 MB，`read_pixels_window` 每次都
    /// `vec![0; n]` 新分配一块，放在主循环里就是一次 mmap/缺页抖动。这里复用调用方的池缓冲。
    /// GL 约定：第 0 行在底部（调用方负责翻转）。
    pub fn read_pixels_window_into(&self, buf: &mut [u8]) {
        let n = (self.w * self.h * 4) as usize;
        assert!(buf.len() >= n, "缓冲太小：{} < {n}", buf.len());
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.read_buffer(glow::BACK);
            self.gl.read_pixels(
                0,
                0,
                self.w as i32,
                self.h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf[..n])),
            );
        }
    }

    pub fn read_pixels(&self) -> Vec<u8> {
        let mut buf = vec![0u8; (self.w * self.h * 4) as usize];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            self.gl.read_pixels(
                0,
                0,
                self.w as i32,
                self.h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
        }
        buf
    }

    /// 读**任意一张中间 RT** 的内容（GL 约定：第 0 行在**底**，与 `read_pixels` 同）。
    /// 用途（`WE_DUMP_RT=<前缀>` 调用）：链里"这一趟根本没画 / 画了但输出 0 / 缓冲尺寸
    /// 退化成 1 像素"这三种情况在**最终图上一模一样**，只有把中间缓冲读出来才能分辨。
    /// `2861113973` 的整屏后处理层（fullscreenlayer + effects/blur）就是靠它定位的。
    pub fn read_rt_pixels(&self, rt: &Rt) -> Vec<u8> {
        let mut buf = vec![0u8; (rt.w * rt.h * 4) as usize];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(rt.fbo));
            self.gl.read_pixels(
                0,
                0,
                rt.w as i32,
                rt.h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        buf
    }
}

/// 动态顶点批（粒子用）：每帧重传顶点，不重建程序/VAO
pub struct GlBatch {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    tex: glow::Texture,
    pub tex_w: u32,
    pub tex_h: u32,
    capacity: usize,
    loc_mvp: Option<glow::UniformLocation>,
    /// 精灵表布局：(帧宽占比, 帧高占比, 帧数, 贴图高宽比) —— WE 的 `g_RenderVar1`
    pub render_var1: [f32; 4],
    /// WE 的 `g_RenderVar0` = `(trail length, maxlength, minlength, 0)`（`spritetrail` 渲染器用）。
    /// 普通 `sprite` 传全 0 —— 此时 WE 的顶点 shader 走 `ComputeParticleTangents`（不吃它）。
    render_var0: [f32; 4],
    /// 这一批是不是 `spritetrail`（`g_RenderVar0` 非零）⇒ 顶点 shader 走
    /// `ComputeParticleTrailTangents`，它**读 `g_EyePosition`**（sprite 那条不读）。
    pub is_trail: bool,
}

/// 粒子顶点：17 个 f32 = 68 字节
/// `[px,py,pz, uvx,uvy,rotz,size, rotx,roty, vx,vy,vz,lifetime01, r,g,b,a]`
/// 对应 WE `genericparticle.vert` 的布局：
///
/// * `a_Position` = 粒子**中心**（不是四个角！`ComputeParticlePosition` 自己按 uv 撑开）
///
/// * `a_TexCoordVec4` = `.xy` 四边形角(0/1) **兼** 纹理 uv；`.z` 旋转；`.w` **尺寸**
///
/// * `a_TexCoordC2` = 旋转 xy
///
/// * `a_TexCoordVec4C1` = `.xyz` 速度、`.w` 寿命进度（精灵表按它选帧）
///
/// * `a_Color` = 颜色
pub const PARTICLE_FLOATS: usize = 17;
pub const PARTICLE_STRIDE: i32 = 68;

impl Gl {
    /// 为粒子的 shader 建一个批。`sheet` = (帧宽占比, 帧高占比, 帧数, 贴图高宽比)；
    /// `render_var0` = WE 的 `g_RenderVar0`（`spritetrail` 用 `(length, maxlength, minlength, 0)`，
    /// 普通 sprite 传全 0）。
    pub fn make_batch(
        &self,
        vs_src: &str,
        fs_src: &str,
        tex: (glow::Texture, u32, u32),
        capacity: usize,
        sheet: [f32; 4],
        render_var0: [f32; 4],
    ) -> Result<GlBatch, String> {
        unsafe {
            let gl = &self.gl;
            let vs = gl.create_shader(glow::VERTEX_SHADER).map_err(|e| e.to_string())?;
            gl.shader_source(vs, vs_src);
            gl.compile_shader(vs);
            if !gl.get_shader_compile_status(vs) {
                return Err(format!("粒子顶点 shader 编译失败:\n{}", gl.get_shader_info_log(vs)));
            }
            let fs = gl.create_shader(glow::FRAGMENT_SHADER).map_err(|e| e.to_string())?;
            gl.shader_source(fs, fs_src);
            gl.compile_shader(fs);
            if !gl.get_shader_compile_status(fs) {
                return Err(format!("粒子片元 shader 编译失败:\n{}", gl.get_shader_info_log(fs)));
            }
            let program = gl.create_program().map_err(|e| e.to_string())?;
            gl.attach_shader(program, vs);
            gl.attach_shader(program, fs);
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("粒子程序链接失败:\n{}", gl.get_program_info_log(program)));
            }
            gl.detach_shader(program, vs);
            gl.detach_shader(program, fs);
            gl.delete_shader(vs);
            gl.delete_shader(fs);

            let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer().map_err(|e| e.to_string())?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let bytes = vec![0u8; capacity * 6 * PARTICLE_STRIDE as usize];
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::DYNAMIC_DRAW);

            let stride = PARTICLE_STRIDE;
            let bind = |name: &str, size: i32, offset: i32| {
                if let Some(loc) = gl.get_attrib_location(program, name) {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, size, glow::FLOAT, false, stride, offset);
                }
            };
            bind("a_Position", 3, 0);
            bind("a_TexCoordVec4", 4, 12);
            bind("a_TexCoordC2", 2, 28);
            bind("a_TexCoordVec4C1", 4, 36);
            bind("a_Color", 4, 52);
            gl.bind_vertex_array(None);
            if std::env::var("WE_DEBUG_PARTICLES").is_ok() {
                eprintln!(
                    "[粒子批] 属性 loc: Position={:?} TexCoordVec4={:?} C2={:?} C1={:?} Color={:?}  MVP={:?}",
                    gl.get_attrib_location(program, "a_Position"),
                    gl.get_attrib_location(program, "a_TexCoordVec4"),
                    gl.get_attrib_location(program, "a_TexCoordC2"),
                    gl.get_attrib_location(program, "a_TexCoordVec4C1"),
                    gl.get_attrib_location(program, "a_Color"),
                    gl.get_uniform_location(program, "g_ModelViewProjectionMatrix"),
                );
            }

            Ok(GlBatch {
                loc_mvp: gl.get_uniform_location(program, "g_ModelViewProjectionMatrix"),
                program,
                vao,
                vbo,
                tex: tex.0,
                tex_w: tex.1,
                tex_h: tex.2,
                capacity,
                render_var1: sheet,
                is_trail: render_var0 != [0.0; 4],
                render_var0,
            })
        }
    }

    /// 画一批粒子四边形（`verts` 每个顶点 17 个 f32）
    pub fn draw_batch(
        &self,
        b: &GlBatch,
        target: Option<&Rt>,
        mvp: [f32; 16],
        verts: &[[f32; PARTICLE_FLOATS]],
        additive: bool,
    ) -> Result<(), String> {
        if verts.is_empty() {
            return Ok(());
        }
        if verts.len() > b.capacity * 6 {
            // **截断而不是报错**：粒子多出来最多是少画几颗，绝不能因为这种事把整张
            // 壁纸判死（以前返回 Err ⇒ 内核丢掉渲染器重建 ⇒ 每 22 秒一次抖动）。
            // 容量本身已按模拟上限对齐（ParticleSystem::capacity），走到这里说明
            // 上游还有偏差，留条命比报错重要。
            eprintln!(
                "[粒子] 顶点数 {} 超过批容量 {}，只画前 {} 个",
                verts.len(),
                b.capacity * 6,
                b.capacity * 6
            );
            let keep = b.capacity * 6;
            let clamped: Vec<[f32; PARTICLE_FLOATS]> = verts[..keep].to_vec();
            return self.draw_batch(b, target, mvp, &clamped, additive);
        }
        let flat: Vec<f32> = verts.iter().flat_map(|v| v.iter().copied()).collect();
        unsafe {
            let gl = &self.gl;
            self.bind_target(target);
            gl.use_program(Some(b.program));
            gl.bind_vertex_array(Some(b.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(b.vbo));
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytemuck_cast(&flat));
            if let Some(loc) = &b.loc_mvp {
                gl.uniform_matrix_4_f32_slice(Some(loc), false, &t4(&mvp));
            }
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(b.tex));
            let set4 = |name: &str, v: [f32; 4]| {
                if let Some(loc) = gl.get_uniform_location(b.program, name) {
                    gl.uniform_4_f32(Some(&loc), v[0], v[1], v[2], v[3]);
                }
            };
            let set3 = |name: &str, v: [f32; 3]| {
                if let Some(loc) = gl.get_uniform_location(b.program, name) {
                    gl.uniform_3_f32(Some(&loc), v[0], v[1], v[2]);
                }
            };
            let set1 = |name: &str, v: f32| {
                if let Some(loc) = gl.get_uniform_location(b.program, name) {
                    gl.uniform_1_f32(Some(&loc), v);
                }
            };
            if let Some(loc) = gl.get_uniform_location(b.program, "g_Texture0") {
                gl.uniform_1_i32(Some(&loc), 0);
            }
            set4(
                "g_Texture0Resolution",
                [
                    b.tex_w as f32,
                    b.tex_h as f32,
                    1.0 / b.tex_w as f32,
                    1.0 / b.tex_h as f32,
                ],
            );
            // 精灵表布局（选帧用）
            set4("g_RenderVar1", b.render_var1);
            // `g_RenderVar0` = 拖尾参数（`spritetrail`）；普通 sprite 全 0，shader 也不会读它。
            set4("g_RenderVar0", b.render_var0);
            // ── 拖尾渲染器需要的"眼睛" ───────────────────────────────────────
            // WE 的 `ComputeParticleTrailTangents`：
            // eyeDirection = localPosition - mul(vec4(g_EyePosition,1), g_ModelMatrixInverse)
            // right = normalize(cross(eyeDirection, localVelocity))
            // 2D 场景里眼睛在屏幕正前方 ⇒ 取 z 正向**很远**的一点（1e6），这样
            // `cross(d, v)` 落在屏幕平面内（否则叉积会被 d 的 z 分量主导 ⇒ 四边形退化成一条线）。
            // 依据：WE 的 eye 距离 ≥ 场景尺度时，`cross` 才由速度方向决定（照抄公式、给一个
            // 与场景尺度相容的相机）；我们的 a_Position 直接是场景坐标（x 可达 3841），
            // 所以 eye 也必须远大于场景尺度。
            set3("g_EyePosition", [0.0, 0.0, 1.0e6]);
            if let Some(loc) = gl.get_uniform_location(b.program, "g_ModelMatrixInverse") {
                // 单位阵：粒子顶点已经是场景坐标，没有额外的模型矩阵
                let id = [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
                    1.0,
                ];
                gl.uniform_matrix_4_f32_slice(Some(&loc), false, &id);
            }
            // ── 粒子的**屏幕朝向基** ─────────────────────────────────────────
            // `ComputeParticleTangents` 里的 right/up 是从这个基向量算出来的：
            // mRotation = mul(mRotation, mat3(g_OrientationRight, g_OrientationUp,
            // g_OrientationForward));
            // right = mul(vec3(1,0,0), mRotation);
            // up = mul(vec3(0,1,0), mRotation);
            // 不设 ⇒ GL 里默认全 0 ⇒ mRotation 是零矩阵 ⇒ right=up=(0,0,0)
            // ⇒ `ComputeParticlePosition` 对六个顶点都返回同一个中心点 ⇒ **退化三角形**
            // ⇒ 什么都画不出来，而且不报任何 GL 错。（这就是粒子"在跑、顶点也发了、
            // 回读 0 像素"的真凶。）
            set3("g_OrientationRight", [1.0, 0.0, 0.0]);
            set3("g_OrientationUp", [0.0, 1.0, 0.0]);
            set3("g_OrientationForward", [0.0, 0.0, 1.0]);
            set3("g_ViewRight", [1.0, 0.0, 0.0]);
            set3("g_ViewUp", [0.0, 1.0, 0.0]);
            // ⚠ 顺序很关键：`ComputeParticleTangents`（sprite）**不读** `g_EyePosition`，
            // 只有 `ComputeParticleTrailTangents`（spritetrail）读 —— 所以拖尾的\"眼睛\"
            // 必须**在这一行之后**再设一次（见下面 `WE_TRAIL_EYE`），否则会被这行清成 0。
            set3("g_EyePosition", [0.0, 0.0, 0.0]);
            if b.is_trail {
                let eye = std::env::var("WE_TRAIL_EYE")
                    .ok()
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(1.0e6);
                set3("g_EyePosition", [0.0, 0.0, eye]);
            }
            // 亮度乘子：不设 = 0 ⇒ `color.rgb *= g_Overbright` 把颜色乘成纯黑，
            // 这是"画面纹丝不动"的**第二个独立原因**。
            set1("g_Overbright", 1.0);
            set1("g_RefractAmount", 0.0);
            set1("g_CutoutStart", 0.99);
            set1("g_CutoutEnd", 1.0);
            set1("g_CutoutOpacity", 1.0);
            // 关掉深度测试：粒子是最后画的屏幕空间元素，而 GL 状态是有粘性的 ✗ ——
            // 底图材质如果开了 depthtest，它会把整屏写成 z=0，粒子的 z 也是 0，
            // LESS 判定失败 ⇒ 整批粒子被丢弃（排查了很久：模拟在跑、顶点在发、无报错，
            // 就是画面纹丝不动）。
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.enable(glow::BLEND);
            if additive {
                gl.blend_func(glow::SRC_ALPHA, glow::ONE);
            } else {
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, verts.len() as i32);
            // 诊断：染色调试模式下，把整张 FBO 回读一遍数"红像素"——用来判定
            // draw_arrays 到底有没有落到帧缓冲上（只看日志里"存活/顶点"会误导）。
            if std::env::var("WE_DEBUG_PARTICLES_BIG").is_ok() {
                let mut buf = vec![0u8; (self.w * self.h * 4) as usize];
                gl.read_pixels(
                    0,
                    0,
                    self.w as i32,
                    self.h as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut buf)),
                );
                let n = buf
                    .chunks_exact(4)
                    .filter(|p| p[0] > 80 && p[1] < p[0] / 2 && p[2] < p[0] / 2)
                    .count();
                eprintln!("[粒子] 绘制后回读：红像素 {n} 个（共 {}）", self.w * self.h);
            }
            gl.disable(glow::BLEND);
            let err = gl.get_error();
            gl.bind_vertex_array(None);
            if err != glow::NO_ERROR {
                return Err(format!("粒子 GL 报错 0x{err:x}"));
            }
            Ok(())
        }
    }
}

/// 一个离屏渲染目标（effect.json 的 fbos 声明：scale=2 ⇒ 半分辨率）
#[derive(Clone, Copy)]
pub struct Rt {
    pub fbo: glow::Framebuffer,
    pub tex: glow::Texture,
    pub w: u32,
    pub h: u32,
}

/// 传进 uniform 的值
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    F(f32),
    I(i32),
    V2(f32, f32),
    V3(f32, f32, f32),
    V4(f32, f32, f32, f32),
    /// 4x4 矩阵（列主序，和 GL 的 `uniformMatrix4fv` 一样）
    M4([f32; 16]),
    /// 3x3 矩阵（列主序 `m[c*3+r]`）。`g_NormalModelMatrix` 用（`genericimage2.vert:34`、
    /// `base/model_vertex_v1.h:5` 声明的都是 `mat3`）。
    /// ⚠ 与 [`Value::M4`] **同一口径**：这里放的必须是"已转置"的那份
    /// （`M.transposed.column_major`），`set_uniform` 会再 `t3` 一次，
    /// 两次抵消后 GL 里的矩阵就是 `M`（见 [`t4`] 的长注释）。
    M3([f32; 9]),
}

/// 一趟绘制的混合方式。
/// WE 的**材质**自己有 `blending` 字段，参考实现（OWE `PassCommon.cppm`）的映射是：
/// `translucent → SRC_ALPHA/ONE_MINUS_SRC_ALPHA`、`additive → SRC_ALPHA/ONE`、
/// `normal`/`disabled`/认不出来的 ⇒ **关掉混合（覆盖）**。
/// 另外对象的 `colorBlendMode=31` 会在引擎侧把它改成 `additive`（OWE `ApplyLayerColorBlend`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    /// 关混合：直接覆盖目标（WE 的 `normal` / 没写 blending 的那些）
    Opaque,
    /// 常规半透明
    Translucent,
    /// 加法（发光、光晕、粒子）
    Additive,
}

impl Blend {
    /// `translucent` / `additive` / 其它 ⇒ 对应混合；`None` 或未知 ⇒ 覆盖
    pub fn of_material(s: Option<&str>) -> Blend {
        match s.map(str::trim) {
            Some("translucent") => Blend::Translucent,
            Some("additive") => Blend::Additive,
            _ => Blend::Opaque,
        }
    }

    /// 对象级的覆盖：`colorBlendMode=31` ⇒ 加法
    /// ⚠ 1..30（以及 32 以上）这里是**旧行为**：当成加法兜着。OWE 的语义不是这样
    /// 。
    /// 本机 17 条壁纸的 `colorBlendMode` 只有 `0`（26 处）与 `31`（4 处）
    /// ⇒ 两条实现**逐字节等价**，
    /// 保留这条只是为了不翻默认。
    pub fn of_object(material: Option<&str>, color_blend_mode: i32) -> Blend {
        Self::of_object_ex(material, color_blend_mode, false)
    }

    /// `WE_CBM_MATERIAL=1` 启用的**忠实**版本（默认关）。
    /// 依据 OWE `SceneMaterialBuilder.cpp::ApplyLayerColorBlend`：
    /// ```text
    /// if (color_blend_mode == 0) return None; // 什么都不做
    /// if (color_blend_mode == 31) { combos.remove(BLENDMODE); blending = "additive"; return Additive; }
    /// material.combos.insert("BLENDMODE", color_blend_mode); // 1..30：只塞编译期 combo
    /// return None; // **不动 GL 混合状态**
    /// ```
    /// ⇒ 1..30 的正确语义是「材质自己的 `blending` 照旧，混合由 **shader** 里的
    /// `ApplyBlending(BLENDMODE, A=画面已画部分, B=本层颜色, opacity)` 完成」，
    /// 而不是「把这层改成加法」。旧实现（加法）会把「本该和画面做 multiply/overlay 的层」
    /// 变成加光 —— 本机 0 场景触发，所以改动是 inert 的，但语义是对的。
    pub fn of_object_faithful(material: Option<&str>, color_blend_mode: i32) -> Blend {
        Self::of_object_ex(material, color_blend_mode, true)
    }

    /// K1：`colorBlendMode` 的**完整计划** = (GL 混合状态, 要塞给 shader 的 `BLENDMODE`)。
    /// 这是把「GL 混合枚举」和「shader 里的 `ApplyBlending`」两条链路一次算清的接口；
    /// `of_object` / `of_object_faithful` 保持原样（不翻任何现有开关）。
    /// 官方口径（一手，来自 WE 自己的资产）：
///
    /// * `assets/shaders/common_blending.h:172-271`：`ApplyBlending(N, A, B, opacity)` 全表，
    /// **A = 目标/屏幕已有色、B = 本层片元色**，opacity 由调用方给（对象趟 = 片元 alpha）；
///
    /// * `assets/shaders/genericimage2.frag:162-167`、`genericimage4.frag:203-207`、
    /// `passthroughblend.frag`：对象趟在 `#if BLENDMODE` 里调它，并 `gl_FragColor.a = screen.a`；
///
    /// * `assets/shaders/common_composite.h:33`：复合趟同式（`effect.a * g_CompositeAlpha`）。
    /// 「哪些 `colorBlendMode` 值走这条」的一手判据只有 shader 侧（`#if BLENDMODE == n` 分支
    /// 覆盖 1..32，其余值落到 270 行的 `BlendNormal` 兜底）；把 `colorBlendMode` 映射成
    /// `BLENDMODE` **combo** 这一步在官方资产里没有可执行样例（官方 42 处 `colorBlendMode`
    /// 全是 0），依据是**第三方参照实现**：
///
    /// * OWE `SceneMaterialBuilder.cpp:48-61`（`n != 0 && n != 31` ⇒ 塞 combo、不动 GL 混合）；
///
    /// * LWE `CImage.cpp:750-765`（`n > 0` ⇒ 塞 combo，**连 31 也交回 shader**）。
    /// ⇒ 本函数取两者的交集：`n > 0 && n != 31` 塞 combo（31 仍按 OWE 的"改成 additive"，
    /// 与 0 一样不塞）——两者在这两点上一致，交集不需要裁判。
    /// `shader_blend=false`（`WE_CBM_SHADER_BLEND=0`）⇒ 完全退回旧行为，逐字节不变；默认 **true**。
    pub fn plan_of_object(
        material: Option<&str>,
        color_blend_mode: i32,
        shader_blend: bool,
    ) -> (Blend, Option<i32>) {
        match color_blend_mode {
            0 => (Blend::of_material(material), None),
            31 => (Blend::Additive, None),
            n if shader_blend && n > 0 => (Blend::of_material(material), Some(n)),
            _ => (Blend::Additive, None),
        }
    }

    fn of_object_ex(material: Option<&str>, color_blend_mode: i32, faithful: bool) -> Blend {
        match color_blend_mode {
            31 => Blend::Additive,
            0 => Blend::of_material(material),
            // 1..30 是 shader 里的 `BLENDMODE` 混合分支（需要把"画面已画部分"当纹理喂进去）
            _ => {
                if faithful {
                    Blend::of_material(material)
                } else {
                    Blend::Additive
                }
            }
        }
    }
}

/// 一个纹理单元的上报信息（`g_Texture{N}` / `g_Texture{N}Resolution` / `g_Texture{N}Texel`）。
/// `g_Texture{N}Resolution` 是**四分量 = (源宽, 源高, 采样宽, 采样高)**：
/// 一手依据 OWE `UniformSource.cpp:1044-1056` 显式写
/// `{source_extent.x, source_extent.y, sample_extent.x, sample_extent.y}`，
/// 而 `g_Texture{N}Texel` 才是 `(1/w, 1/h, w, h)`（同文件 1050-1056）；
/// `SceneMaterialBuilder.cpp:400-414` 给出两个 extent 的来源：
///
/// * 贴图：普通图 `{mapW, mapH, mapW, mapH}`、`mipmap_larger` 时 `{w, h, mapW, mapH}`；
///
/// * 渲染目标：`{rt.width, rt.height, rt.width, rt.height}`（383 行）。
/// `.xy` = **源尺寸**（source extent）、`.zw` = **采样尺寸**（sample extent）。
/// 两者在本渲染器里通常相同（贴图按原尺寸上传、RT 就是 RT 自己的尺寸）；
/// 唯一有意义的例外是**图层画布 RT**：我们的物理缓冲是输出画布（1080p ⇒ 1920×1080），
/// 而 WE 给这一层的缓冲是**该图层自己的画布**（scene.json 的 `size`；691 = 1804×1344）
/// ⇒ 这里报后者，否则 `blend.vert` 里按像素计的 `g_BlendOffset` / `scaleA` 会随输出尺寸漂移
/// （见 `lib.rs::rt_logical` 与 `per_tex_res_on` 的注释）。
#[derive(Clone, Copy)]
pub struct UnitTex {
    pub unit: u32,
    pub tex: glow::Texture,
    /// `g_Texture{N}Resolution.xy`
    pub src_w: u32,
    pub src_h: u32,
    /// `g_Texture{N}Resolution.zw`（`g_Texture{N}Texel` 也从这一对算）
    pub w: u32,
    pub h: u32,
}

impl UnitTex {
    /// 源尺寸 == 采样尺寸（贴图 / 普通 RT 的默认情形；也是今天**唯一**的情形 ——
    /// 我们按原图解码，没有 `mipmap_larger` 那种"图集比采样区大"的数据）。
    pub fn new(unit: u32, tex: glow::Texture, w: u32, h: u32) -> Self {
        Self {
            unit,
            tex,
            src_w: w,
            src_h: h,
            w,
            h,
        }
    }
}

/// 一趟绘制需要的全部输入
pub struct DrawCtx<'a> {
    /// 画到哪（None = 屏幕 FBO）
    pub target: Option<&'a Rt>,
    pub mvp: [f32; 16],
    /// 纹理单元：unit → (贴图 + 上报尺寸)。会顺带设 `g_Texture{N}` / `g_Texture{N}Resolution` /
    /// `g_Texture{N}Texel`（尺寸语义见 [`UnitTex`]）
    pub units: &'a [UnitTex],
    /// 逐趟常量：uniform 名 → 值（名字由 shader 注释里的元数据翻译过来）
    pub scalars: &'a [(String, Value)],
    /// 本帧的场景时间（秒）⇒ 下发 `g_Time`。单独一项，**不要**再让调用点克隆整条
    /// `scalars` 来 push 它（每趟每帧一次 Vec 分配 + N 个 String 复制）。
    /// 若 `scalars` 里也有 `g_Time`，后者（这里）覆盖 —— 与旧行为一致。
    pub time: Option<f32>,
    /// 这一趟怎么和已有画面混合
    pub blend: Blend,
}

/// 把 RGBA 像素按行上下翻转（`h` 行 × `w` 列，每像素 4 字节）。
/// 用途见 [`Gl::make_texture`]：我们的 UV 约定与 GL 的行序相反，上传前要翻一次。
/// 长度不足时原样返回（宁可画面不对，也不要在渲染线程里 panic）。
fn flip_rows(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row = (w as usize) * 4;
    let need = row * (h as usize);
    if row == 0 || rgba.len() < need || h <= 1 {
        return rgba.to_vec();
    }
    let mut out = vec![0u8; need];
    for y in 0..h as usize {
        let src = (h as usize - 1 - y) * row;
        out[y * row..(y + 1) * row].copy_from_slice(&rgba[src..src + row]);
    }
    out
}

impl Gl {
    /// 上传一张 RGBA 贴图。
    /// ⚠ **上传前把行上下翻一遍**：我们的四边形 UV 约定是"屏幕上边 = v=1"
    /// （WE 场景坐标 y 向下 + `Mat4::ortho` 反着传，见 `SceneRenderer::new`），
    /// 而 GL 的 `tex_image_2d` 把**数据第一行当成 v=0**。两者正好相反 ⇒ 不翻的话
    /// 整条场景贴图上下颠倒（实测："Gaze" 的天空跑到屏幕下方、人物倒立）。
    /// 只影响**上传的**贴图（材质/掩膜/粒子）；渲染目标 RT 是屏幕取向的，不需要翻。
    pub fn make_texture(&self, rgba: &[u8], w: u32, h: u32) -> Result<(glow::Texture, u32, u32), String> {
        self.make_texture_ex(rgba, w, h, true, false)
    }

    /// 同 `make_texture`，但**按 `.tex` 头部声明**决定采样状态：
    /// `repeat=true` ⇒ `TEXTURE_WRAP_* = REPEAT`（WE 的 `clampUVs` 位没置时就是这样）。
    /// `repeat=false` ⇒ 与 `make_texture` 逐字节同路。
    pub fn make_texture_wrapped(
        &self,
        rgba: &[u8],
        w: u32,
        h: u32,
        repeat: bool,
    ) -> Result<(glow::Texture, u32, u32), String> {
        self.make_texture_ex(rgba, w, h, true, repeat)
    }

    /// 同 `make_texture`，但**不翻行**（贴图按"第 0 行 = v=0"原样上传）。
    /// 给**屏幕取向**的数据用：切换壁纸的过渡覆盖层（`SceneRenderer::make_fade_layer` 那套四边形
    /// 的 v=0 落在屏幕顶）与窗口预热（`draw_fullscreen_rgba_oneshot`，同一套几何）
    /// 这两处必须用同一种朝向，否则预热帧与淡出帧会互相上下颠倒。
    pub fn make_texture_screen(
        &self,
        rgba: &[u8],
        w: u32,
        h: u32,
    ) -> Result<(glow::Texture, u32, u32), String> {
        self.make_texture_ex(rgba, w, h, false, false)
    }

    fn make_texture_ex(
        &self,
        rgba: &[u8],
        w: u32,
        h: u32,
        flip: bool,
        repeat: bool,
    ) -> Result<(glow::Texture, u32, u32), String> {
        let flipped = if flip { flip_rows(rgba, w, h) } else { rgba.to_vec() };
        // 调试旋钮：把**真正上传的那份**（已翻行）落盘（`WE_DUMP_TEX=<目录>`）。
        // 与 `lib.rs::tex_to_rgba` 里"解码结果"的落盘区分开 —— 同一张 .tex 可能被**解码两次**
        // 走不同分支，两次的行序未必相同（`1931934283` 的倒置就在查这个）。
        if let Some(dir) = std::env::var_os("WE_DUMP_TEX") {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 256 {
                if let Some(img) = image::RgbaImage::from_raw(w, h, flipped.clone()) {
                    let _ = std::fs::create_dir_all(&dir);
                    let p = std::path::Path::new(&dir).join(format!("upload{n:03}_{w}x{h}.png"));
                    let _ = img.save(&p);
                    eprintln!("[贴图上传 dump] → {}", p.display());
                }
            }
        }
        unsafe {
            let gl = &self.gl;
            let tex = gl.create_texture().map_err(|e| e.to_string())?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&flipped)),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            let wrap = if repeat {
                glow::REPEAT as i32
            } else {
                glow::CLAMP_TO_EDGE as i32
            };
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, wrap);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, wrap);
            Ok((tex, w, h))
        }
    }

    /// 1x1 白：贴图槽没有对应资源时的兜底（不绑会采样出黑，透明度掩膜就全废了）
    pub fn make_white_texture(&self) -> Result<(glow::Texture, u32, u32), String> {
        self.make_texture(&[255, 255, 255, 255], 1, 1)
    }

    /// **原地替换一张贴图的内容**（尺寸必须一致）。
    /// 只给"文字层换色"用（`WE_SETPROP_FAST=1`）：文字层的字色是**烘进光栅化贴图**的，
    /// 换色不需要重新建场景（那要 ~600 ms），只要把同一块画布的 RGB 换成新色再传一遍。
    /// 采样状态（滤波/包裹）沿用建贴图时设的，不重设 ⇒ 与 `make_texture` 那一路逐字节同参。
    pub fn replace_texture(
        &self,
        tex: glow::Texture,
        rgba: &[u8],
        w: u32,
        h: u32,
        flip: bool,
    ) -> Result<(), String> {
        let flipped = if flip { flip_rows(rgba, w, h) } else { rgba.to_vec() };
        unsafe {
            let gl = &self.gl;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&flipped)),
            );
        }
        Ok(())
    }

    /// 只重传贴图的一个子矩形（`glTexSubImage2D`）。
    /// 用途：`set-prop` 换字色时只有"有字形的那块"会变（本机 691 的覆盖率掩膜
    /// bbox 只占画布 **43.7%**）⇒ 9.7 MB 的全量 `glTexImage2D` 变成 ~4.2 MB 的子块，
    /// 实测上传段 8.8 ms → ~3 ms。**朝向必须与 [`replace_texture`] 同口径**：
    /// 数据行 r 落到纹理行 `h-1-r`，所以子块 `(x0,y0,sw,sh)` 的落点是
    /// `(x0, h-y0-sh)`，块内行也要再翻一次。
    #[allow(clippy::too_many_arguments)]
    pub fn replace_texture_rect(
        &self,
        tex: glow::Texture,
        tex_w: u32,
        tex_h: u32,
        x0: u32,
        y0: u32,
        sw: u32,
        sh: u32,
        rgba: &[u8],
        flip: bool,
    ) -> Result<(), String> {
        if sw == 0 || sh == 0 || sw * sh * 4 != rgba.len() as u32 {
            return Err(format!(
                "replace_texture_rect: 尺寸不符 {sw}x{sh}（缓冲 {} 字节）",
                rgba.len()
            ));
        }
        let flipped;
        let data: &[u8] = if flip {
            flipped = flip_rows(rgba, sw, sh);
            &flipped
        } else {
            rgba
        };
        let dy = tex_h.saturating_sub(y0).saturating_sub(sh);
        unsafe {
            let gl = &self.gl;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            // UNPACK 对齐：子块宽度不一定是 4 的倍数 ⇒ 明确按 4 字节对齐（RGBA8 天然满足）
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x0 as i32,
                dy as i32,
                sw as i32,
                sh as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(data)),
            );
        }
        let _ = tex_w;
        Ok(())
    }

    pub fn create_rt(&self, w: u32, h: u32) -> Result<Rt, String> {
        unsafe {
            let gl = &self.gl;
            let tex = gl.create_texture().map_err(|e| e.to_string())?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            let fbo = gl.create_framebuffer().map_err(|e| e.to_string())?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            let st = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if st != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!("RT {w}x{h} FBO 不完整: 0x{st:x}"));
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            Ok(Rt { fbo, tex, w, h })
        }
    }

    /// 颜色写入掩码（参照实现用 glColorMask(1,1,1,0) 让「链尾那趟不写 alpha」）。
    pub fn color_mask(&self, r: bool, g: bool, b: bool, a: bool) {
        unsafe {
            self.gl.color_mask(r, g, b, a);
        }
    }

    /// 剪裁矩形（`glScissor`；`None` = 关掉）。
    /// 用途：`WE_PERF_SCISSOR` 把每一趟剪到该 RT 的**内容包围盒**。注意 scissor 是
    /// **粘的**，而且它同时也剪 `glClear` ⇒ 调用点必须在"清屏"之后才设（见 draw_frame 里
    /// 遮罩烘焙趟那一段的顺序）。
    pub fn scissor(&self, r: Option<[i32; 4]>) {
        unsafe {
            match r {
                Some([x, y, w, h]) if w > 0 && h > 0 => {
                    self.gl.enable(glow::SCISSOR_TEST);
                    self.gl.scissor(x, y, w, h);
                }
                _ => self.gl.disable(glow::SCISSOR_TEST),
            }
        }
    }

    /// 诊断用：等 GPU 把已排队的活干完（只给 WE_PASS_GPU 探针用；默认路径一次都不调）。
    pub fn gpu_sync(&self) {
        unsafe {
            self.gl.finish();
        }
    }

    // ───────────────────────── 逐 pass GPU 计时（WE_PROFILE_PASSES） ─────────────────────────
    // 用法（`lib.rs` 的趟循环）：`pt_frame_begin` → 每趟 `pt_begin` / `pt_end` →
    // 帧尾 `pt_collect` 拿回可用的 `(趟下标, 纳秒)`。全部是**异步**的 timer query，
    // 不夹断流水（对比 `gpu_sync` 那条老路）。

    /// 一帧开始：挑一个"手上没有未回读结果"的槽。全都在飞行中 ⇒ 本帧不采样。
    pub fn pt_frame_begin(&self) {
        let mut g = self.pt.lock().unwrap();
        if g.dead {
            g.active = false;
            return;
        }
        // 上一帧的槽已经填完 ⇒ 记下它的 query 数
        if g.active {
            let c = g.cur;
            g.counts[c] = g.cursor;
        }
        let mut pick = None;
        for k in 1..=PT_SLOTS {
            let s = (g.cur + k) % PT_SLOTS;
            if g.counts[s] == 0 {
                pick = Some(s);
                break;
            }
        }
        match pick {
            Some(s) => {
                g.cur = s;
                g.cursor = 0;
                g.active = true;
            }
            None => {
                g.active = false;
            }
        }
    }

    /// 本趟开始计时（没在采样的帧什么都不做）。
    pub fn pt_begin(&self, pi: usize) {
        let mut g = self.pt.lock().unwrap();
        if !g.active || g.cursor >= PT_MAX_PASSES {
            return;
        }
        let i = g.cursor;
        // 懒建这张 query（池子的第 cur 槽第 i 个）
        if g.slots[g.cur].len() <= i {
            let s = g.cur;
            match unsafe { self.gl.create_query() } {
                Ok(q) => g.slots[s].push(q),
                Err(_) => {
                    g.dead = true;
                    g.active = false;
                    return;
                }
            }
        }
        let q = g.slots[g.cur][i];
        let cur = g.cur;
        // 记下这张 query 对应的**趟下标**（被跳过的趟不会走到这里，所以序号与趟号会错位）
        if g.idx[cur].len() <= i {
            g.idx[cur].push(pi);
        } else {
            g.idx[cur][i] = pi;
        }
        unsafe { self.gl.begin_query(glow::TIME_ELAPSED, q) };
    }

    /// 本趟结束（`pt_begin` 之后必须成对调用）。
    pub fn pt_end(&self) {
        let mut g = self.pt.lock().unwrap();
        if !g.active {
            return;
        }
        unsafe { self.gl.end_query(glow::TIME_ELAPSED) };
        g.cursor += 1;
    }

    /// 帧尾：把**最早**那个"结果全部可用"的槽读出来（`(趟下标, 纳秒)`，一趟没漏）；
    /// 没有可用槽 ⇒ 空。读 `QUERY_RESULT` 前先查 `QUERY_RESULT_AVAILABLE` ⇒ 绝不阻塞。
    pub fn pt_collect(&self) -> Vec<(usize, u64)> {
        let mut g = self.pt.lock().unwrap();
        if g.dead {
            return Vec::new();
        }
        let mut out = Vec::new();
        for s in 0..PT_SLOTS {
            let n = g.counts[s];
            if n == 0 || s == g.cur && g.active {
                continue;
            }
            let ready = (0..n.min(g.slots[s].len())).all(|i| unsafe {
                self.gl
                    .get_query_parameter_u32(g.slots[s][i], glow::QUERY_RESULT_AVAILABLE)
                    != 0
            });
            if !ready {
                continue;
            }
            for i in 0..n.min(g.slots[s].len()) {
                let ns = unsafe {
                    self.gl
                        .get_query_parameter_u32(g.slots[s][i], glow::QUERY_RESULT)
                } as u64;
                out.push((g.idx[s].get(i).copied().unwrap_or(i), ns));
            }
            g.counts[s] = 0;
        }
        out
    }

    /// 把这张 RT 清成指定颜色（默认全透明黑）。**场景级 RT 每帧必须先清**：
    /// 否则半透明图层（如 Ice Princess 的第一层 `фон мин`，blending=translucent）会把
    /// "上一帧/未初始化"的内容混出来 ⇒ 整幅画发灰发软。实测就是这条把人物区锐度
    /// 从 20.84 打到 11（而首层不透明的另外 5 条壁纸逐字节不受影响）。
    pub fn clear_rt(&self, rt: &Rt, rgba: [f32; 4]) {
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(rt.fbo));
            gl.clear_color(rgba[0], rgba[1], rgba[2], rgba[3]);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// 把**当前屏幕（默认帧缓冲）**的内容在 **GPU 侧**拷进这张 RT 的纹理。
    /// 用途：WE 的"全屏后处理层"（`models/util/fullscreenlayer.json`，`fullscreen:true` +
    /// `passthrough:true`）要拿**到目前为止的场景合成图**当输入（材质里管它叫
    /// `_rt_FullFrameBuffer`）—— `effects/blur` 正是靠它把整幅画糊掉。我们没有场景级 RT
    /// （逐层直接画屏幕），所以遇到这种层时现建一张，并在画它之前把屏幕内容灌进去。
    /// `copy_tex_sub_image_2d` 从当前 **read framebuffer** 读 ⇒ 无需 CPU 回读。
    pub fn copy_screen_into_rt(&self, rt: &Rt) {
        unsafe {
            let gl = &self.gl;
            gl.bind_texture(glow::TEXTURE_2D, Some(rt.tex));
            gl.copy_tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                0,
                0,
                rt.w as i32,
                rt.h as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// K1：把 **RT `src` 的内容**在 GPU 侧拷进 **RT `dst`** 的纹理
    /// （`glCopyTexSubImage2D` 从当前 **read framebuffer** 读 ⇒ 先把 `src` 绑成 read）。
    /// 用途：对象级 `colorBlendMode` 走 shader 内 `ApplyBlending` 时，A 必须是
    /// **"画这一层之前的画面"**。而这一层的绘制目标往往**就是**那张画面
    /// （无特效层直接画进场景 RT）⇒ 直接采样它是 GL 里的 feedback（结果未定义）。
    /// 先拷一份再采样，语义与官方一致（`_rt_FullFrameBuffer` = 到目前为止的场景合成图），
    /// 且没有 feedback。`dst` 与 `src` 尺寸必须相同（只拷满幅）。
    pub fn copy_rt_into_rt(&self, src: &Rt, dst: &Rt) {
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(src.fbo));
            gl.bind_texture(glow::TEXTURE_2D, Some(dst.tex));
            gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, 0, 0, dst.w as i32, dst.h as i32);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        }
    }

    /// 切换绘制目标（None = 屏幕）+ 设 viewport。注意投影不变 ——
    /// 同一块"场景矩形"画到半分辨率 RT 上，正好就是 WE 的 scale=2 语义。
    /// 当前绑定的帧缓冲是否完整（诊断用）
    pub fn check_fb_status(&self) -> u32 {
        unsafe { self.gl.check_framebuffer_status(glow::FRAMEBUFFER) }
    }

    /// 当前绑定的 draw framebuffer 编号（0 = 默认/窗口帧缓冲，诊断用）
    pub fn draw_fb_binding(&self) -> i32 {
        unsafe { self.gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) }
    }

    /// 读一次 GL 错误（清掉粘住的标志）；诊断用
    pub fn get_error_inner(&self) -> u32 {
        unsafe { self.gl.get_error() }
    }

    /// 强制绑到窗口的默认帧缓冲（FBO 0）—— 排查"窗口分支到底走没走"用
    pub fn force_window_fb(&self) {
        unsafe { self.gl.bind_framebuffer(glow::FRAMEBUFFER, None) }
    }

    /// 装好本帧的**场景灯**（`None` ⇒ 本帧不下发任何灯 uniform）。
    /// 由 `SceneRenderer::draw_frame` 在趟循环之前按本帧场景时间调用一次。
    pub fn set_lights(&mut self, lights: Option<LightUniforms>) {
        self.lights = lights;
    }

    pub fn bind_target(&self, rt: Option<&Rt>) {
        unsafe {
            match rt {
                Some(r) => {
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(r.fbo));
                    self.gl.viewport(0, 0, r.w as i32, r.h as i32);
                }
                None => {
                    // 窗口模式：画到**窗口的默认帧缓冲（FBO 0）**，再由 swap 交给合成器
                    // （surfaceless 模式没有默认帧缓冲，所以还是绑自己的 FBO ✗ 两种情况必须分开）
                    match self.egl_surface {
                        Some(_) => self.gl.bind_framebuffer(glow::FRAMEBUFFER, None),
                        None => self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo)),
                    }
                    self.gl.viewport(0, 0, self.w as i32, self.h as i32);
                }
            }
        }
    }

    /// 按一趟的输入画（RT / 逐趟常量 / 多贴图单元）
    pub fn draw_pass(&self, l: &GlLayer, ctx: &DrawCtx) -> Result<(), String> {
        unsafe {
            let gl = &self.gl;
            // 逐调用查错：哪一步出错就报哪一步（不然只知道"这一趟炸了"）。
            // ⚠ **默认关**：这是 2026-10 测出来的头号每帧浪费 —— `glGetError` 在 Mesa/radeonsi 上
            // 实测约 **1.5µs/次**（不是"读一个标志位"那么便宜），一趟 4 次 × 113 趟 = 每帧 452 次
            // ≈ 0.68ms，占整帧 CPU 的 14%。而它**只产出诊断信息，不改变任何一个像素**。
            // 趟尾那一次仍然保留（`get_error` 在 draw 之后）⇒ 错误照样能被发现，"连续 N 次失败
            // 就重建"的护栏不受影响，丢的只是"哪一步"的粒度。
            // `WE_GL_STEP_CHECK=1` 可以把逐调用查错开回来（排障用）。
            let chk = |step: &str| -> Result<(), String> {
                if !gl_step_check() {
                    return Ok(());
                }
                let e = gl.get_error();
                if e != glow::NO_ERROR {
                    return Err(format!("{step} 之后报 0x{e:x}"));
                }
                Ok(())
            };
            let sp_bind = crate::ftime::Span::pass(&crate::ftime::P_BIND);
            self.bind_target(ctx.target);
            chk("bind_target")?;
            // 混合状态：**glow 的 GL 状态是粘的**，每一趟都必须显式设一次
            // （以前对象图层从没设过 ⇒ 一直是"关混合"，后画的图层把前面的整个覆盖掉）
            match ctx.blend {
                Blend::Opaque => gl.disable(glow::BLEND),
                Blend::Translucent => {
                    gl.enable(glow::BLEND);
                    // WE 的贴图是**预乘 alpha**（颜色已乘过 alpha）⇒ 直乘混合会**再乘一次 alpha**，
                    // 结果整体压暗（人物画成剪影）。`WE_PREMULT=1` 切到预乘混合做 A/B。
                    // 默认仍是老行为（直乘），避免在没验证前改动全局观感。
                    if std::env::var_os("WE_PREMULT").is_some() {
                        gl.blend_func_separate(
                            glow::ONE,
                            glow::ONE_MINUS_SRC_ALPHA,
                            glow::ONE,
                            glow::ONE_MINUS_SRC_ALPHA,
                        );
                    } else {
                        gl.blend_func_separate(
                            glow::SRC_ALPHA,
                            glow::ONE_MINUS_SRC_ALPHA,
                            glow::SRC_ALPHA,
                            glow::ONE_MINUS_SRC_ALPHA,
                        );
                    }
                }
                Blend::Additive => {
                    gl.enable(glow::BLEND);
                    gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE, glow::SRC_ALPHA, glow::ONE);
                }
            }
            chk("blend")?;
            drop(sp_bind);
            let sp_prog = crate::ftime::Span::pass(&crate::ftime::P_PROG);
            gl.use_program(Some(l.program));
            chk("use_program")?;
            gl.bind_vertex_array(Some(match l.vao_custom {
                Some(v) => v,
                None => {
                    if l.raw_position {
                        l.vao_flip_v
                    } else {
                        l.vao
                    }
                }
            }));
            chk("bind_vertex_array")?;
            if let Some(loc) = &l.loc_mvp {
                gl.uniform_matrix_4_f32_slice(Some(loc), false, &t4(&ctx.mvp));
            }
            drop(sp_prog);
            let sp_unit = crate::ftime::Span::pass(&crate::ftime::P_UNIT);
            let res_fix_val = res_fix();
            // 每趟取一次（`OnceLock` 缓存过，但别放进 unit 循环里白刷 8 次）
            let per_tex_res = crate::per_tex_res_on();
            for ut in ctx.units {
                let unit = ut.unit;
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, Some(ut.tex));
                // `g_Texture{N}Resolution` 的布局**是四个尺寸，不是尺寸+倒数**：
                // `.xy` = 源尺寸（source extent）、`.zw` = 采样尺寸（sample extent）
                // 依据（两处独立证据）：① 参考实现 OWE `UniformSource.cpp` 显式写
                // `{source_w, source_h, sample_w, sample_h}`，另有 `g_Texture{N}Texel` 才是 `(1/w,1/h,w,h)`；
                // ② 本机 17 条壁纸的 shader 里 68 种用法全是尺寸语义，例如
                // `vec2 offsets = 0.5 / g_Texture0Resolution.xy`（半纹素）、
                // `blurredCoords += 0.75 / g_Texture0Resolution.zw`、
                // `float offsetX = g_Scale.x / g_Texture0Resolution.z`（纹素步长）、
                // `float aspect = Resolution.z / Resolution.w * g_Ratio`（宽高比）。
                // 以前这里填的是 `(w, h, 1/w, 1/h)`（把 Texel 的倒数塞进了 Resolution）⇒
                // `0.75 / (1/3840) = 2880 个纹素`，blur 类特效全错（实测：画面出现竖条带）。
                // 三个名字的位置**一次查到、终身复用**（见 `GlLayer::ulocs_unit`）。
                let [u_tex, u_res, u_texel] = l.uloc_unit(gl, unit);
                if let Some(loc) = u_tex {
                    gl.uniform_1_i32(Some(&loc), unit as i32);
                }
                if let Some(loc) = u_res {
                    // ⚠ 实验开关 `WE_RES_FIX=WxH`：把 `g_Texture{N}Resolution` **强制**成给定尺寸。
                    // 动机：官方 `blend.vert` 的 UV 数学里 `g_BlendOffset`（像素）要除以
                    // `g_Texture0Resolution` ⇒ 我们拿**窗口尺寸**当图层缓冲尺寸 ⇒ 映射随输出尺寸漂移
                    // ⇒ 同一条壁纸 2560x1440 出插画、1920x1080 不出（实测字区颜色数 3810 vs win 11037）。
                    // 用它验证"喂分辨率无关的尺寸"能否让 1080p 也出画；验证完按结论做正式修法。
                    let fixed = res_fix_val;
                    match fixed {
                        Some((uw, uh)) => gl.uniform_4_f32(Some(&loc), uw, uh, uw, uh),
                        None if per_tex_res => gl.uniform_4_f32(
                            Some(&loc),
                            ut.src_w as f32,
                            ut.src_h as f32,
                            ut.w as f32,
                            ut.h as f32,
                        ),
                        // 旧口径（`WE_TEXRES=0`）：`.xy` 与 `.zw` 同值，且就是这张贴图的物理尺寸。
                        None => {
                            let (uw, uh) = (ut.w as f32, ut.h as f32);
                            gl.uniform_4_f32(Some(&loc), uw, uh, uw, uh)
                        }
                    }
                }
                // 顺带把 Texel 也给上（WE 的另一个约定名：`(1/w, 1/h, w, h)`），本机没有 shader 用它，
                // 但给了不影响任何东西。
                if let Some(loc) = u_texel {
                    gl.uniform_4_f32(
                        Some(&loc),
                        1.0 / ut.w as f32,
                        1.0 / ut.h as f32,
                        ut.w as f32,
                        ut.h as f32,
                    );
                }
            }
            drop(sp_unit);
            let sp_scal = crate::ftime::Span::pass(&crate::ftime::P_SCAL);
            for (name, v) in ctx.scalars {
                l.set_uniform(gl, name, *v);
            }
            // 每帧都要下发的场景时间。以前是调用点把整条 `scalars` **克隆一份**再 push
            // （每趟每帧一次 Vec 分配 + N 个 String 复制）—— 现在只多一项，同名后者覆盖。
            if let Some(t) = ctx.time {
                l.set_uniform(gl, "g_Time", Value::F(t));
            }
            // 音频频谱数组：win 的 shader 直接读 `g_AudioSpectrum16Left/Right`（float[16]）。
            // 只喂标量 `g_Audio*` 时这两个数组恒为 0 ⇒ Simple_Audio_Bars 之类永远平。
            // 关掉音频时**不下发**（uniform 保持未赋值）⇒ 与旧行为逐字节一致。
            if crate::audio::enabled() {
                let (al, ar) = crate::audio::channels();
                for (name, arr) in [
                    ("g_AudioSpectrum16Left", &al),
                    ("g_AudioSpectrum16Right", &ar),
                ] {
                    if let Some(loc) = l.uloc(gl, name) {
                        // 本函数体已在 `unsafe` 块内（见函数开头），无需再包一层。
                        gl.uniform_1_f32_slice(Some(&loc), arr);
                    }
                }
            }
            drop(sp_scal);
            let sp_fixed = crate::ftime::Span::pass(&crate::ftime::P_FIXED);
            // 没赋值的 uniform 在 GLSL 里是 0 ⇒ 这几个"语义上应该是 1"的要显式给。
            // 但这四个名字里的三个（`g_Brightness`/`g_UserAlpha`/`g_Alpha`）在官方语义里是
            // **对象常量**（底图趟拿对象自己的值、其余趟才中性 1）—— 与 `g_Color4` 是**同一个
            // 缺陷**、**同一个 P_FIXED 段**，只是上（一）轮只修了颜色那一支。判据、双出处与
            // "为什么不能只看名字"见 `obj_alpha_on` / `carries_obj_consts` 的注释。
            // `WE_OBJ_ALPHA=1`（默认关）时它们从"无条件"改成"缺省值"；关时仍是旧的无条件下发
            // ⇒ 逐字节不变（`WE_OBJ_COLOR` 的修复一个字没动，两个开关各自独立 A/B）。
            // `g_Opacity`：官方 shader **0 处**声明它、OWE 也没有这个名字（`SpecNames.cppm:118-122`
            // 只有 COLOR4/COLOR/ALPHA/USERALPHA/BRIGHTNESS）⇒ **语义无法判定**；且全仓 grep
            // 只有本行写过它 ⇒ 无论哪种口径都恒为 1.0（实测逐字节不变）。
            let obj_family_gate = obj_alpha_on() && carries_obj_consts(ctx);
            // `WE_FX_ALPHA=1`：**特效趟**（= 非对象底图趟）也按官方 `NeutralColorUniforms`
            // 语义 —— 本趟**显式**给了就采信、没给才兜 1。与上面 `WE_OBJ_ALPHA` 是**同一段
            // 的另一个缺口**（那一支管"对象底图趟"，这一支管"特效趟"）。
            // 名单**只**含官方 `NeutralColorUniforms` 的那三支（`g_Alpha`/`g_UserAlpha`/
            // `g_Brightness`）；`g_Opacity` 无任何出处 ⇒ 不作语义主张、保持强制 1.0。
            // `g_BlendAlpha`/`g_KeyAlpha`/`g_AlphaMultiply` **从不在这条强制名单里**
            // （它们由 `ctx.scalars` 正常下发）⇒ 本开关**不碰**它们：碰了反而会把
            // tint/colorkey 的显式值搞乱（同一个 label `alpha` 在别家特效里映射到这四个
            // 不同名字 —— 出处见 `fx_alpha_on` 的注释）。判据「显式给了」= 开关打开时
            // `lib.rs` 不再把这几支的 range 兜底塞进特效趟 scalars ⇒ 还在 scalars 里就是显式值。
            let fx_family_gate = fx_alpha_on() && !carries_obj_consts(ctx);
            for (n, v, fx_gov) in [
                ("g_Brightness", 1.0f32, true),
                ("g_UserAlpha", 1.0, true),
                ("g_Alpha", 1.0, true),
                ("g_Opacity", 1.0, false),
            ] {
                let given = ctx.scalars.iter().any(|(k, _)| k == n);
                // 底图趟：`WE_OBJ_ALPHA`。
                // 特效趟：只在 `WE_FX_ALPHA` 打开**且**这一支在官方名单里时才采信显式值。
                if (obj_family_gate && given) || (fx_family_gate && fx_gov && given) {
                    // 本趟已经显式给了自己的值 ⇒ 不许盖掉它。
                    continue;
                }
                l.set_uniform(gl, n, Value::F(v));
            }
            // `g_TextureReductionScale`：**官方引擎全局**，不是材质参数 —— 官方 `blend.vert:5` /
            // `skew.vert:8` 声明它、**没有注解也没有 `default`** ⇒ 注解兜底机制天然捞不到它 ✗，
            // 而 `blend.vert:75` 直接拿它**做除数**：
            // `v_TexCoord.zw -= ((g_BlendOffset / g_TextureReductionScale) - …) / g_Texture0Resolution.zw;`
            // 我们从不喂它 ⇒ GLSL 里是 **0** ⇒ `-671 / 0 = ±Inf` ⇒ `v_TexCoord.zw` 变 Inf/NaN
            // ⇒ 混合贴图（那张插画）**永远采同一颗像素** ⇒ 一片纯色。实测 `3704273480` 主题2：
            // 大字笔画里出不来人物、默认画面≈遮罩强制关（只差 5.38%）—— 都是这一条。
            // 语义 = 纹理降采样倍率，正常 **1.0**（参照实现 OWE 的引擎常量）。
            // 本趟显式给了就采信（不覆盖作者/场景的值）。
            // `WE_TRS=0` 关回旧行为（逐字回到"从不喂 ⇒ 0 ⇒ Inf"），用于 A/B 与横切基准。
            let trs_on = std::env::var("WE_TRS")
                .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
                .unwrap_or(true);
            if trs_on
                && !ctx
                    .scalars
                    .iter()
                    .any(|(k, _)| k == "g_TextureReductionScale")
            {
                l.set_uniform(gl, "g_TextureReductionScale", Value::F(1.0));
            }
            // 对象级 `g_Color4`（对象的 `color`/`alpha`，由 `lib.rs::obj_scalars` 喂）**已经**
            // 在 `ctx.scalars` 里 ⇒ 这里不能无条件盖成 `(1,1,1,1)`，否则任何读 `g_Color4` 的
            // 趟乘的都是 1（现代分支 `color *= g_Color4` 形同虚设）。
            // 官方语义 = "对象常量优先、缺省才 1"：底图趟拿到对象值、特效趟拿到中性 1
            // （依据见 `obj_color_on` 的注释；OWE `NeutralColorUniforms`）。
            // `WE_OBJ_COLOR=1`（默认关）时才改成"缺省值"；关时仍是旧的无条件下发，逐字节不变。
            let obj_color4_given =
                obj_color_on() && ctx.scalars.iter().any(|(k, _)| k == "g_Color4");
            if !obj_color4_given {
                l.set_uniform(gl, "g_Color4", Value::V4(1.0, 1.0, 1.0, 1.0));
            }
            drop(sp_fixed);
            // ── 场景灯（`g_LightsPosition[4]` 等）──────────────────────────────
            // 本帧有灯就下发。**数组型 uniform 不能走 `set_uniform`**（`Value` 只有标量/向量），
            // 这里按 shader 自己声明的名字逐个查位置（`uloc` 有缓存）后整段上传。
            // 未声明的名字 `uloc` 返回 `None` ⇒ 直接跳过（= 与"没喂"逐字节同效）。
            if let Some(li) = self.lights.as_ref() {
                let arr3 = |name: &str, v: &[f32]| {
                    if let Some(loc) = l.uloc(gl, name) {
                        gl.uniform_3_f32_slice(Some(&loc), v)
                    }
                };
                arr3("g_LightsPosition", &li.position);
                let arr4 = |name: &str, v: &[f32]| {
                    if let Some(loc) = l.uloc(gl, name) {
                        gl.uniform_4_f32_slice(Some(&loc), v)
                    }
                };
                arr4("g_LightsColorRadius", &li.color_radius);
                arr4("g_LightsDirectionType", &li.direction_type);
                arr4("g_LightsConeExponent", &li.cone_exponent);
                arr4("g_LightsColorPremultiplied", &li.color_premultiplied);
                if let Some(loc) = l.uloc(gl, "g_LightsCastShadow") {
                    gl.uniform_1_f32_slice(Some(&loc), &li.cast_shadow)
                }
                // 环境色 / 天空色：**无条件**下发（哪怕是 0 也下发）
                l.set_uniform(gl, "g_LightAmbientColor", Value::V3(li.ambient[0], li.ambient[1], li.ambient[2]));
                l.set_uniform(gl, "g_LightSkylightColor", Value::V3(li.skylight[0], li.skylight[1], li.skylight[2]));
            }
            let sp_draw = crate::ftime::Span::pass(&crate::ftime::P_DRAW);
            // puppet/MDLV 网格是**索引**绘制；普通图层仍是 6 顶点的四边形。
            // 诊断旋钮 `WE_NO_DRAW=1`：跳过绘制本身（**只为把"绘制提交"的代价单独量出来**，
            // 画面必然错，别在别处用）。
            if !no_draw() {
                if l.index_count > 0 {
                    gl.draw_elements(glow::TRIANGLES, l.index_count, glow::UNSIGNED_SHORT, 0);
                } else {
                    gl.draw_arrays(glow::TRIANGLES, 0, 6);
                }
            }
            drop(sp_draw);
            let sp_err = crate::ftime::Span::pass(&crate::ftime::P_ERR);
            let err = gl.get_error();
            if err != glow::NO_ERROR {
                return Err(format!("GL 报错 0x{err:x}"));
            }
            drop(sp_err);
            crate::ftime::NPASS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // 调试旋钮：把**这一趟的目标缓冲** dump 成 PNG（`WE_DUMP_RT=<目录>`，前 60 趟）。
            // 中间缓冲的像素比最终图更能说明问题 —— "哪一趟把图像翻了/缩了/染黑了"一眼可见。
            if let Some(dir) = std::env::var_os("WE_DUMP_RT") {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                if n < 60 {
                    // 目标可能是 RT，也可能是**屏幕**（`ctx.target == None`）⇒ 两种都 dump：
                    // 屏幕那趟读的是"当前绑定的帧缓冲"，正好能回答"是没画，还是画到别处去了"。
                    let (w, h) = match ctx.target {
                        Some(rt) => (rt.w as usize, rt.h as usize),
                        None => (self.w as usize, self.h as usize),
                    };
                    {
                        let mut px = vec![0u8; w * h * 4];
                        gl.read_pixels(
                            0,
                            0,
                            w as i32,
                            h as i32,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelPackData::Slice(Some(&mut px)),
                        );
                        // GL 的第 0 行在**下**，PNG 的第 0 行在**上** ⇒ 翻行再存
                        let mut img = image::RgbaImage::new(w as u32, h as u32);
                        for y in 0..h {
                            for x in 0..w {
                                let s = ((h - 1 - y) * w + x) * 4;
                                img.put_pixel(
                                    x as u32,
                                    y as u32,
                                    image::Rgba([px[s], px[s + 1], px[s + 2], px[s + 3]]),
                                );
                            }
                        }
                        let _ = std::fs::create_dir_all(&dir);
                        let tag = if ctx.target.is_some() { "rt" } else { "screen" };
                        let path =
                            std::path::Path::new(&dir).join(format!("{tag}{n:03}_{w}x{h}.png"));
                        let _ = img.save(&path);
                        eprintln!("[RT dump] 第 {n} 趟 → {}", path.display());
                    }
                }
            }
            gl.bind_vertex_array(None);
            Ok(())
        }
    }
}

/// 调试：把一个 RT 读回并落 PNG（`WE_GLOW_DUMP` 用）。GL 行序自下而上 ⇒ 翻行。
impl Gl {
    pub fn read_rt_to_png(&self, rt: &Rt, path: &std::path::Path) {
        let (w, h) = (rt.w as usize, rt.h as usize);
        let mut px = vec![0u8; w * h * 4];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(rt.fbo));
            self.gl.read_pixels(
                0,
                0,
                w as i32,
                h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut px)),
            );
        }
        let mut img = image::RgbaImage::new(rt.w, rt.h);
        for y in 0..h {
            for x in 0..w {
                let s = ((h - 1 - y) * w + x) * 4;
                img.put_pixel(
                    x as u32,
                    y as u32,
                    image::Rgba([px[s], px[s + 1], px[s + 2], px[s + 3]]),
                );
            }
        }
        let _ = img.save(path);
    }
}

/// 一个"编译好了的图层"：程序 / VAO / VBO / 贴图 / uniform 位置都缓存下来。
/// 每帧只做「设 uniform + draw」，绝不重新编译（实时渲染必须这样）。
pub struct GlLayer {
    program: glow::Program,
    vao: glow::VertexArray,
    /// 同一个四边形、但 **V 反向**（`uv.y → 1-uv.y`）。
    /// 给"裸 `a_Position`"的趟用：这类炸趟在 GL 下读纹理时上下是反的
    /// （GL 的 NDC y=+1 落在 rendertarget 的**最后一行**，而我们的 V 约定是 `v=0 → 画面顶`），
    /// 而 WE 在 D3D 下这两件事天然一致。见 `raw_position`。
    vao_flip_v: glow::VertexArray,
    /// **自定义四边形**（shape 对象的"直接绘制四边形"：四角来自特效材质的 `point0..point3`）。
    /// 有它时优先用，忽略内置的那个 [-1,1] 四边形。
    vao_custom: Option<glow::VertexArray>,
    /// 顶点 shader **不使用** `g_ModelViewProjectionMatrix`（`gl_Position = vec4(a_Position, 1.0)` 那类）。
    /// 判据：链接后 MVP uniform 变成 inactive ⇒ 说明取到的分支根本没引用它。
    pub raw_position: bool,
    /// 顶点几何**实际覆盖的范围**（顶点坐标系）：`Some([hx,hy])` = 官方卡片网格（顶点是对象像素
    /// `±size/2`，MVP 里已乘 `2/size`）；`None` = 我们自己的 `±1` 单位四边形。
    /// 用途只有一个：融合趟必须用**同一套几何**重建（否则 MVP 的缩放口径对不上，整层画空 ——
    /// 实测 `3504393434` 亮度 122→0，见 `apply_fuse` 的几何闸）。
    pub card_half: Option<[f32; 2]>,
    /// 顶点来自**自定义四边形 / 索引网格**（shape 收尾趟、puppet 网格）⇒ 不能用全屏四边形顶替。
    pub custom_quad: bool,
    /// 本层 shader 是否读逐帧变化的 uniform（g_Time / 指针 / 音频 / 灯 / 形变权重）。
    /// 判据见 reads_dynamic_uniforms：只做源码子串扫描，宁可保守。
    /// 用途只有一处：WE_PERF_STATIC_CACHE 判断这张链 RT 的内容还能不能跨帧复用。
    pub dyn_token: &'static str,
    /// Does it read g_Lights? (static only while no light source is animated)
    pub reads_lights: bool,

    /// 持有即可（顶点数据在 buffer 里），不用再读
    #[allow(dead_code)]
    vbo: glow::Buffer,
    /// 内置贴图（只有非特效趟用；特效趟的贴图由 `DrawCtx::units` 提供）
    tex: Option<glow::Texture>,
    loc_mvp: Option<glow::UniformLocation>,
    /// >0 表示这是个**索引网格**图层（puppet/MDLV）：绘制要用 `draw_elements`，
    /// 且元素缓冲已绑在这个 VAO 上。
    index_count: i32,
    /// 网格原始顶点（归一化到 ±1；y 已翻成我们的 y 下）。摆锤形变以它为基准。
    mesh_pos: Vec<[f32; 3]>,
    mesh_uvs: Vec<[f32; 2]>,
    /// 两套 VAO 各自的顶点缓冲（V 正/反），摆锤形变时两张都要重传。
    mesh_vbos: Option<[glow::Buffer; 2]>,
    /// 摆锤参数：`[振幅(弧度), 频率(Hz), 相位滞后, y_top, y_span]`；振幅 0 ⇒ 关。
    /// 形变 = 绕"顶部"的水平剪切，越往下摆幅越大、相位越滞后（"顶部绑定、坠为主摆"）。
    mesh_sway: [f32; 5],
    /// **uniform 位置缓存**（名字 → `glGetUniformLocation` 的结果，含「不存在」）。
    /// 这是每帧 CPU 的最大浪费点：以前**每帧、每趟、每个 uniform** 都调一次
    /// `glGetUniformLocation`。glow 的实现每次都 `CString::new(name)`（一次堆分配）
    /// 再进驱动里按名字查找 + 比较字符串。一条 28 层的壁纸 ≈ 100 趟 × 约 25 个 uniform
    /// ⇒ 每帧 2500+ 次堆分配 + 2500+ 次驱动调用，纯属重复劳动：program 的 uniform 表在
    /// **链接之后就不再变**了，位置一次查到就能一直用。
    /// 用 `RefCell` 是因为 `draw_pass` 拿的是 `&self`（GL 上下文只读借用）。
    ulocs: RefCell<HashMap<Box<str>, Option<glow::UniformLocation>>>,
    /// 纹理槽那三件套（`g_Texture{N}` / `g_Texture{N}Resolution` / `g_Texture{N}Texel`）
    /// 的 uniform 位置，按 unit 索引缓存（`None` = 这个 unit 还没查过）。
    /// 顺带省掉了每帧每槽三次 `format!` 的堆分配。
    ulocs_unit: RefCell<Vec<Option<[Option<glow::UniformLocation>; 3]>>>,
}

impl GlLayer {
    /// 纹理槽三件套的位置（带缓存；`WE_NO_ULOC_CACHE=1` 时每次真查）
    fn uloc_unit(&self, gl: &glow::Context, unit: u32) -> [Option<glow::UniformLocation>; 3] {
        let i = unit as usize;
        if !no_uloc_cache() {
            if let Some(x) = self.ulocs_unit.borrow().get(i).copied().flatten() {
                return x;
            }
        }
        let x = [
            self.uloc(gl, &format!("g_Texture{unit}")),
            self.uloc(gl, &format!("g_Texture{unit}Resolution")),
            self.uloc(gl, &format!("g_Texture{unit}Texel")),
        ];
        if !no_uloc_cache() {
            let mut v = self.ulocs_unit.borrow_mut();
            if v.len() <= i {
                v.resize(i + 1, None);
            }
            v[i] = Some(x);
        }
        x
    }
}

impl GlLayer {
    /// 查 uniform 位置（带缓存）。`WE_NO_ULOC_CACHE=1` 时**绕过缓存**，用来 A/B
    /// 证明「这一项到底值多少」（同一条二进制、同一场景、只差这一次查表）。
    fn uloc(&self, gl: &glow::Context, name: &str) -> Option<glow::UniformLocation> {
        if no_uloc_cache() {
            return unsafe { gl.get_uniform_location(self.program, name) };
        }
        if let Some(v) = self.ulocs.borrow().get(name) {
            return *v;
        }
        let loc = unsafe { gl.get_uniform_location(self.program, name) };
        self.ulocs.borrow_mut().insert(name.into(), loc);
        loc
    }

    /// 设一个 uniform（先查缓存拿位置）
    fn set_uniform(&self, gl: &glow::Context, name: &str, v: Value) {
        let Some(loc) = self.uloc(gl, name) else {
            return;
        };
        unsafe {
            match v {
                Value::F(x) => gl.uniform_1_f32(Some(&loc), x),
                Value::I(x) => gl.uniform_1_i32(Some(&loc), x),
                Value::V2(a, b) => gl.uniform_2_f32(Some(&loc), a, b),
                Value::V3(a, b, c) => gl.uniform_3_f32(Some(&loc), a, b, c),
                Value::V4(a, b, c, d) => gl.uniform_4_f32(Some(&loc), a, b, c, d),
                Value::M4(m) => gl.uniform_matrix_4_f32_slice(Some(&loc), false, &t4(&m)),
                Value::M3(m) => gl.uniform_matrix_3_f32_slice(Some(&loc), false, &t3(&m)),
            }
        }
    }
}

/// `WE_RES_FIX=WxH`（实验旋钮，A/B 用）：把 `g_Texture{N}Resolution` 的四个分量**全强制**成给定值。
/// 只用来验证"分辨率是取景旋钮"这一判断，常态不要开（开了就退化成"所有贴图同一个尺寸"）。
/// 走 `OnceLock` 缓存：`std::env::var` 是**全环境扫描**（1~3µs），一趟一次会是新浪费。
fn res_fix() -> Option<(f32, f32)> {
    static F: std::sync::OnceLock<Option<(f32, f32)>> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        let s = std::env::var("WE_RES_FIX").ok()?;
        let mut it = s.split(['x', 'X', ',', ' ']).filter(|x| !x.is_empty());
        let a = it.next().and_then(|x| x.trim().parse::<f32>().ok())?;
        let b = it.next().and_then(|x| x.trim().parse::<f32>().ok())?;
        Some((a, b))
    })
}

/// A/B 旋钮：`WE_NO_ULOC_CACHE=1` 退回「每帧每次查表」的旧行为（只为量化收益，别常态开）
fn no_uloc_cache() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("WE_NO_ULOC_CACHE").map(|v| v != "0").unwrap_or(false))
}

/// `WE_NO_DRAW=1` ⇒ 跳过绘制本身（诊断用：只为把"绘制提交"的代价单独量出来）。
/// ⚠ 必须走 `OnceLock` 缓存：`std::env::var` 每次都是**全环境扫描**（约 1~3µs），
/// 一趟调一次 × 113 趟 = 每帧凭空多 0.1~0.3ms —— 那就成了新的浪费。
fn no_draw() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var_os("WE_NO_DRAW").is_some())
}

/// `WE_GL_STEP_CHECK=1` ⇒ 每趟里每一步之后都 `glGetError`。
/// 默认关：实测单次只有 0.012µs（Mesa 读一个标志位），但一趟 4 次 × 113 趟也是白烧；
/// 更重要的是它只产出诊断信息、不改变渲染结果。
fn gl_step_check() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("WE_GL_STEP_CHECK").map(|v| v != "0").unwrap_or(false))
}

/// `WE_OBJ_COLOR=1` ⇒ **按官方语义喂对象级 `g_Color4`**（默认关）。
/// 缺陷：`draw_pass` 在应用完 `ctx.scalars` 之后**无条件**下发 `g_Color4=(1,1,1,1)`
/// （"语义上应为 1 的兜底"），把 `obj_scalars` 里的对象色（对象 `color`/`alpha`）盖掉 ⇒
/// 任何读 `g_Color4` 的现代分支乘的都是 1。
/// 官方语义（双出处）：
/// ① 官方 shader —— `assets/shaders/genericimage2.frag:11/72`（`#else`（= 定义了 `VERSION`）
/// 分支 `color *= g_Color4;`）、`genericimage3.frag:11/176`、`genericimage4.frag:25/93`
/// （这两条在 `main` 里**无条件**乘）、`puppettexturechannels.frag:2/26/31`、
/// `font.frag:3/136/149`。⇒ 对象色就是 `g_Color4`。
/// ② 参考实现 OWE —— `Scene/Pkg/Parse/Image/SceneImageObjectParser.cpp:226-237` 把对象的
/// `color`/`alpha`/`brightness` 塞进**底图（材质趟）**的 `baseConstSvs`；
/// 而 `Scene/Pkg/Parse/Scene/SceneMaterialBuilder.cpp:61-69 NeutralColorUniforms`
/// 只给**特效趟**兜 `1` ⇒ "对象常量优先，缺省才是 1"。
/// 本开关的语义 = 把那条兜底从"无条件"改成"**缺省值**"：`ctx.scalars` 里已带 `g_Color4`
/// 的趟（= 材质底图趟）不再被盖；没带的趟（= 特效趟等）仍补 `(1,1,1,1)` —— 与 OWE 的
/// `NeutralColorUniforms` 逐支对应。数据驱动，不看 id/名字。
/// 关（不设 / ≠ "1"）⇒ 保留旧的无条件下发，逐字节不变。
fn obj_color_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| crate::flag_default_on("WE_OBJ_COLOR"))
}

/// `WE_OBJ_ALPHA=1` ⇒ **按官方语义喂"对象常量组"里的不透明度三支**
/// （`g_Brightness` / `g_UserAlpha` / `g_Alpha`，默认关）。与 `WE_OBJ_COLOR` **同族但独立**
/// （分开 A/B 的理由：`g_Color4` 只在**现代分支**里被乘，而 `g_UserAlpha`/`g_Brightness`
/// 只在**旧分支**里被乘 ⇒ 两条路互补，混在一个开关里会看不出是哪一支在动）。
///
/// ## 双出处（缺陷：`draw_pass` 的 `P_FIXED` 段把这三支**无条件**盖成 1）
/// **出处①（官方 shader，一手）** —— 这三支确实是对象常量、且被真用：
///
/// * `assets/shaders/genericimage2.frag:8/9` 声明 `g_Brightness`/`g_UserAlpha`（在
/// `#ifndef VERSION` 里），`69/70` 是 `color.rgb *= g_Brightness; color.a *= g_UserAlpha;`
/// 也就是**旧分支**的对象色/不透明度入口；`#else`（`VERSION`）才换成 `g_Color4`。
///
/// * `assets/shaders/genericimage.frag:6/7/25/26` 同两个名字、**无组合开关**（无条件乘）。
///
/// * `assets/shaders/flat.frag:2/8`、`flatpoint.frag:2/8`：`gl_FragColor = vec4(g_Color, g_Alpha)`
/// 纯色层（`solidlayer`/`纯色`）的 alpha 就走 `g_Alpha`。
///
/// * `g_Opacity`：**官方 shader 里 0 处声明**（`grep -rn "g_Opacity" ~/Downloads/wallpaper_engine/assets/shaders`
/// 为空），OWE 也没有这个名字 ⇒ **无法判定**，不作语义主张。
///
/// * 特效侧只有一处：`assets/effects/opacity/shaders/effects/opacity.frag:7` 声明
/// `g_UserAlpha`（注解 `"range":[0.01, 1]`）—— 这条是"特效趟"的材质属性，见下面的 ⚠。
/// **出处②（参考实现 OWE，同源口径）** —— 值来自对象、且只作用在**底图趟**：
///
/// * `open-wallpaper-engine/…/Parse/Image/SceneImageObjectParser.cpp:226-239`：把对象 `color`/`alpha`/`brightness`
/// 塞进 **`baseConstSvs`** 的 `G_COLOR4`/`G_COLOR`/`G_ALPHA`/`G_USERALPHA`/`G_BRIGHTNESS`
/// （`235` = `G_ALPHA ← alpha`、`236` = `G_USERALPHA ← alpha`、`237` = `G_BRIGHTNESS ← brightness`），
/// 再作为**底图材质趟**的 `shaderInfo.baseConstSvs`。
///
/// * `…/Scene/SceneMaterialBuilder.cpp:61-69 NeutralColorUniforms`：特效趟用
/// `G_COLOR4=(1,1,1,1)`、`G_COLOR=(1,1,1)`、`G_ALPHA=G_USERALPHA=G_BRIGHTNESS=1.0` **兜底**
/// （调用点 `SceneImageObjectParser.cpp:1009/1099`、`SceneTextObjectParser.cpp:633`）。
///
/// * 名字定义：`…/Pkg/SpecNames.cppm:118-122`。
///
/// ## ⚠ 为什么这三支**不能**照 `g_Color4` 那样只看"本趟 scalars 里有没有这个名字"
/// 特效趟 `opacity.frag` **自己**声明 `g_UserAlpha`，而 `lib.rs` 有一条既有的兜底
/// （`range:[lo,hi]` 且 `lo>0` 的 float 在场景没给值时补 `lo`）⇒ 那些特效趟的 scalars 里
/// **会出现** `g_UserAlpha = 0.01`。只看名字就会把 29 个 opacity 特效趟（`2861113973`/
/// `3299815325`/`3571777169` 等，见 `WE_DBG_OBJ_ALPHA` 清单）从"不透明"改成"1% 不透明"，
/// 而官方对特效趟的取值是 **1.0**（出处② `NeutralColorUniforms`；官方注解写的默认值也是
/// `default:1.0`，0.01 是"我们自己的兜底"造出来的，不是作者合同里的值）。
/// ⇒ 判据改为**结构性的**「这一趟是不是携带对象常量组的那一趟」，即 OWE 的 `baseConstSvs` 趟。
fn obj_alpha_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| crate::flag_default_on("WE_OBJ_ALPHA"))
}

/// `WE_FX_ALPHA=1` ⇒ **按官方 `NeutralColorUniforms` 语义喂"特效趟"的 alpha 族**（默认关）。
///
/// ## 缺陷（与 `WE_OBJ_ALPHA` 同一个 `draw_pass` 段，但**另一类趟**）
/// `draw_pass` 的 `P_FIXED` 段对**每一趟**无条件下发 `g_Brightness=g_UserAlpha=g_Alpha=1.0`。
/// 对**对象底图趟**这是错的（对象常量优先，由 `WE_OBJ_ALPHA` 修）；对**特效趟**同样不是
/// "无条件 1.0"，而是 **"材质/场景显式给了就采信、没给才是 1.0"** —— 官方 `NeutralColorUniforms`
/// 只是**缺省值的种子**，之后材质/场景自己的 `constantshadervalues` 会覆盖它。所以特效趟里
/// 作者显式写下的 alpha 也被我们盖掉了。
///
/// ## ① 官方 shader 声明处（逐个 grep；`WE_DUMP_SHADER` 落盘的 `*.pass*.frag` 里同样出现）
///
/// * `assets/effects/opacity/shaders/effects/opacity.frag:7`
/// `uniform float g_UserAlpha; // {"material":"alpha","default":1.0,"range":[0.01,1]}`
/// `16: albedo.a *= mask * g_UserAlpha;`
///
/// * `assets/effects/tint/shaders/effects/tint.frag:11`
/// `uniform float g_BlendAlpha; // {"material":"alpha","default":1,"range":[0,1]}`
/// `16: float mask = g_BlendAlpha;`
///
/// * `assets/effects/colorkey/shaders/effects/colorkey.frag:9`
/// `uniform float g_KeyAlpha; // {"material":"alpha","label":"ui_editor_properties_write_alpha",`
/// `"default":0,"range":[0,1]}`；`24: albedo.a *= mix(g_KeyAlpha, 1.0, blend);`
///
/// * `assets/effects/blend/shaders/effects/blend.frag:30` 与
/// `assets/effects/blendgradient/shaders/effects/blendgradient.frag:14`
/// `uniform float g_AlphaMultiply; // {"material":"alpha","default":1,"range":[0.0,1.0]}`
///
/// * `assets/effects/edgedetection/shaders/effects/edgedetection.frag:11`（同一支 `g_BlendAlpha`）
///
/// * `assets/shaders/genericimage2.frag:8/9`（对象底图**旧分支**）声明 `g_Brightness`/`g_UserAlpha`
///
/// * `assets/shaders/flat.frag:2/8`（纯色层）声明 `g_Alpha`
///
/// * **边界（实测 `grep -rln ... assets/effects`）**：`g_BlendAlpha`→tint/edgedetection 等；
/// `g_KeyAlpha`→colorkey；`g_AlphaMultiply`→blend/blendgradient；`g_UserAlpha`→**只有** opacity；
/// **`g_Alpha`/`g_Brightness` 在 `assets/effects/**` 里 0 处**（只在 `shaders/**`）。
///
/// * **`g_Opacity`：官方 shader 0 处声明、OWE 也没有这个名字**（`SpecNames.cppm:118-122`
/// 只有 COLOR4/COLOR/ALPHA/USERALPHA/BRIGHTNESS）⇒ **语义无法判定**，本开关不作主张。
///
/// ## ② OWE 中这些值从哪里来
///
/// * **同一个 label `alpha` 落到哪个名字，是"材质键 → uniform 名"的映射**，不是写死的名字：
/// `SceneMaterialBuilder.cpp:556 ResolveShaderMaterialKey`（表 `info.alias` 按每个 uniform
/// 注解里的 `"material"` 建）⇒ tint 的 `alpha`→`g_BlendAlpha`、colorkey 的
/// `alpha`→`g_KeyAlpha`、blend 的 `alpha`→`g_AlphaMultiply`、opacity 的 `alpha`→`g_UserAlpha`。
/// **这就是"不能一刀切"的出处**：那是 4 个不同 uniform，只有 `g_UserAlpha`（以及底图旧分支的
/// `g_Alpha`/`g_Brightness`）在 `NeutralColorUniforms` 的名单里。
///
/// * `SceneMaterialBuilder.cpp:61-69 NeutralColorUniforms` = **种子**：
/// `G_COLOR4/G_COLOR/G_ALPHA/G_USERALPHA/G_BRIGHTNESS ← 中性值`（1 / 1 / 1 / 1 / 1）。
/// 调用点：`SceneImageObjectParser.cpp:1009/1099`（内部 passthrough / final 特效节点）、
/// `SceneTextObjectParser.cpp:633`（文字特效底）。
///
/// * 覆盖它的显式值：材质自己的 `constantshadervalues` 走
/// `SceneMaterialBuilder.cpp:909 LoadConstvalueImpl` → `Scene.cppm:705 SceneMaterial::SetShaderValue`
/// → `customShader.constValues.insert(...)`；而 `baseConstSvs` 在 `BuildMaterial` 的 `:525-527`
/// 先落下 ⇒ **材质/场景值后落、优先**。
///
/// * ⇒ 官方语义 = **"显式给了就采信、没给才是中性 1.0"**；本开关把这条补到特效趟。
///
/// ## 为什么不能只看"本趟 scalars 里有没有这个名字"
/// `lib.rs` 有一条**我们自己的**兜底：注解 `range:[lo,hi]` 且 `lo>0` 的 float，场景没给时补 `lo`。
/// `opacity.frag` 的 `g_UserAlpha` range 正是 `[0.01,1]` ⇒ **场景没给时 scalars 里会出现
/// `g_UserAlpha=0.01`**（这是我们造的，不是作者合同里的值）。只看名字就会把 29 个 opacity
/// 特效趟从"不透明"改成"1% 不透明"。⇒ 判据改为**"显式来源"**：开关打开时 `lib.rs` 不再把
/// 这几支的 range 兜底塞进特效趟 scalars（见 `lib.rs` 兜底 2 循环里的 `fx_alpha_on` 分支）
/// ⇒ 特效趟 scalars 里**还有**这个名字，就一定来自 `ef`/`ep`/`ov` 的显式值。
///
/// ## 只覆盖哪几支（**按名字分清**）
/// 只有 `NeutralColorUniforms` 名单里的 `g_Alpha`/`g_UserAlpha`/`g_Brightness` 走本开关。
/// `g_BlendAlpha`/`g_KeyAlpha`/`g_AlphaMultiply` **本来就不在 `draw_pass` 的强制名单里**
/// （由 `ctx.scalars` 正常下发）⇒ 本开关**不碰**它们。`g_Opacity` **无出处 ⇒ 保持旧的
/// 无条件 1.0**（不作语义主张）。数据驱动：不看 id/名字/特效名，只看"这一趟是不是携带
/// 对象常量组的趟"（`carries_obj_consts`）与"这个名字有没有显式值"。
pub fn fx_alpha_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| crate::flag_default_on("WE_FX_ALPHA"))
}

/// `WE_FX_DEFAULT=1`（**默认关**，与 `WE_FX_ALPHA` 独立 A/B）：官方「scalar 缺省机制」。
/// 上次 `NOTES §9` 分歧① 的修法：**特效趟**里，某个 uniform **场景没给值**时，我们这边
/// 停在 GLSL 的 0，而官方会落 **shader 注解里的 `"default"`**。
///
/// ## 双出处
///
/// * 出处①（一手）：官方 shader 把自己的缺省值**写在同一行的 JSON 注解**里，例如
/// `assets/effects/blend/shaders/effects/blend.frag:30`
/// `uniform float g_AlphaMultiply; // {"material":"alpha","label":"ui_editor_properties_alpha","default":1,"range":[0.0, 1.0]}`
/// （`tint` 的 `g_BlendAlpha` default=1、`colorkey` 的 `g_KeyAlpha` default=0、
/// `opacity` 的 `g_UserAlpha` default=1.0 —— 见 `we-scene-format` 的 `UniformDecl::default`）。
///
/// * 出处②（参考实现 OWE）：`open-wallpaper-engine/src/Scene/Pkg/Parse/Scene/SceneMaterialBuilder.cpp:531-541`
/// 在 `BuildMaterial` 里**先**用 `shader->default_uniforms` 播种（即注解里的 `default`），
/// 再由 `NeutralColorUniforms`（`:61-69`）与材质/场景的 `constantshadervalues`
/// （`LoadConstvalueImpl :909` → `Scene.cppm:705 SetShaderValue`）**后**落下覆盖
/// ⇒ 「场景显式给了就采信、没给才落注解 default」。
/// ⚠ 边界：我**只读到** `default_uniforms` 的**落地顺序**（先 default、后显式），
/// 没有逐字读到"注解 default 一定进 default_uniforms"那一行的构造代码 ⇒ 该映射标注为
/// **"按语义/调用顺序推断"**。
///
/// ## 判据（数据驱动，不按 id/名字特判）
///
/// * **范围**只含「注解 label（`"material"` 字段）属 alpha 族」的 float —— 见 `fx_default_label`。
/// 按 **label** 判而不是按 uniform 名硬编码：官方 `ResolveShaderMaterialKey`
/// （`SceneMaterialBuilder.cpp:556`）就是拿注解里的 `"material"` 当别名键 ⇒ **同一个 label
/// `alpha` 落到 `g_UserAlpha`/`g_BlendAlpha`/`g_KeyAlpha`/`g_AlphaMultiply` 四个不同名字**。
///
/// * `default` 必须能解析成 f32（空串 / 颜色 `"1 1 1"` / 纹理名 `"util/white"` ⇒ 跳过，不给）。
///
/// * 本趟 scalars 里**已有**这个名字（= 作者显式值 / range 兜底）⇒ **不覆盖**（显式优先）。
///
/// * 判据全部来自 `shader::parse_shader_meta` 读出的注解，不看对象的 id/名字、不看特效名、不看场景。
/// 关（不设 / ≠ "1"）⇒ 这一段根本不执行 ⇒ 逐字节不变。
pub fn fx_default_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| crate::flag_default_on("WE_FX_DEFAULT"))
}

/// 注解 label（`"material"` 字段）是否属 **alpha 族**。
/// 官方在 `assets/effects/**` 里给"不透明度/亮度"类 uniform 的注解 label 只有两种拼写：
/// `"alpha"`（`opacity`→`g_UserAlpha`、`tint`/`edgedetection`→`g_BlendAlpha`、
/// `colorkey`→`g_KeyAlpha`、`blend`/`blendgradient`→`g_AlphaMultiply`）与
/// `"brightness"` / `"Brightness"` / `"Bright"`（底图 `genericimage*.frag` 的 `g_Brightness`）。
/// 大小写与 `Bright` 缩写都实测存在 ⇒ 大小写不敏感、`bright` 前缀也算。
/// ⚠ 这**不是**"按 uniform 名特判"：判的是 shader **自己声明的语义键**（OWE 的别名键），
/// 名字是它的产物而不是输入 —— 所以往 shader 里加一个新的 alpha 族 uniform（只要 label 写对）
/// 无需改这里的代码。
pub fn fx_default_label(prop: &str) -> bool {
    let p = prop.trim().to_ascii_lowercase();
    p == "alpha" || p == "brightness" || p == "bright"
}

/// `WE_FX_DEFAULT_ALL=1`（**默认关**，可单独开、也可与 `WE_FX_DEFAULT` 同开）：
/// 把 `fx_default_on` 那条「注解 default 缺省机制」的**范围**从 alpha 族放宽到
/// **所有带非空 `material` label 的 float**。
///
/// ## 双出处（与 `fx_default_on` 同一机制，只是范围）
///
/// * 出处①（一手）：官方 shader 把缺省值写在同一行的 JSON 注解里，例如
/// `assets/effects/waterwaves/shaders/effects/waterwaves.frag:17`
/// `uniform float g_Scale; // {"material":"scale",...,"default":200,"range":[0.01,1000],...}`
/// 同文件 `g_Speed` `default:5`(`:16`)、`g_Strength` `default:0.1`(`:19`)；
/// `effects/shake.frag:17` `g_Amp default:0.1`；`effects/blend.frag:25-29`
/// `g_Multiply2..6 default:1`；`effects/foliagesway.vert:8` `g_Strength default:0.4`。
/// 解析进 `shader::parse_shader_meta` 的 `UniformDecl::prop` / `::default`（`shader.rs:685-695`，
/// 数值 default 走 `other.to_string` ⇒ `"200"` 可 `parse::<f32>`）。
///
/// * 出处②（参考实现 OWE —— 三步，**都不筛 label**）：
///
/// 1. `src/Scene/Pkg/Parse/Shader/ShaderAnnot.cpp:192-202`：**每个**带注解的非纹理 uniform，
/// 若 `sv_json` 有 `"default"` 且是数字 ⇒ 造标量 `ShaderValue` 并 `info->svs.insert(name, sv)`。
///
/// 2. `src/Scene/Pkg/Parse/Scene/SceneMaterialBuilder.cpp:500-501`：
/// `shader->default_uniforms = shader_info_ref.svs.clone;`
/// ⇒ **全部**注解 default 都进 `default_uniforms`（这一步没有任何 label / `u_` 过滤）。
///
/// 3. **真正上屏那一步**（关键）：`src/Scene/VulkanRender/UniformBuffer.cpp:289-294` 把
/// `shader.default_uniforms` 当 `defaults` 传进 `UniformBufferBinding`；
/// `:160-174 Update` 里**先**把 `m_defaults` 每一条推进参数表、
/// **再**推 `material.customShader.constValues` 每一条 ⇒
/// **注解 default 先落、场景/作者的显式值后落覆盖**，且**与 label 是不是 alpha 无关**。
/// （对照：`SceneMaterialBuilder.cpp:531-541` 那个 `!var.is_user || var.material.is_empty`
/// 循环其实是**用户变量索引**的顺带播种 —— `Uniform.cpp:75` 里
/// `is_user = name.starts_with("u_")` ⇒ 只覆盖 `u_*`；本开关走的是 `UniformBuffer`
/// 那条**全量**路径，不是它。）
///
/// * ⚠ 诚实边界：`vrento::UniformBinding::SetParameters` 的"后落覆盖"语义在外部包（未 vendor，
/// 与上次 §9.1 的 rstd `insert` 同源）。但**调用方先 defaults 后 constValues 的顺序**
/// 本身就是"显式值覆盖注解 default"的判据；且这与 `NeutralColorUniforms`
/// （`SceneMaterialBuilder.cpp:61-69`，`g_Color4/g_Color/g_Alpha/g_UserAlpha/g_Brightness=1`）
/// 先播种、`LoadConstvalue`（`:909`）后落下的语序同向。
///
/// ## 判据（数据驱动，不按 id/名字/特效名特判）
///
/// * 范围 = `u.prop.trim` **非空**（= 注解真的写了 `material` 键）。判的是 shader 自己声明的键，
/// **不看 uniform 名** ⇒ 往 shader 加新的带 label 的 float 无需改代码。
///
/// * `default` 必须能解析成 f32（空串 / 颜色 `"1 1 1"` / 纹理名 `"util/white"` ⇒ 跳过）。
///
/// * 本趟 scalars 里已有本名 ⇒ **不覆盖**（作者显式值优先）。
///
/// * 放在**兜底 2（range_lo）之前** ⇒ 注解 default 优先于我们自造的 `range_lo`
/// （`waterwaves g_Scale`：注解 default **200** vs 我们那条 range 兜底给的 **0.01**）。
///
/// * 只作用于**特效趟**分支（与 `fx_default_on` 同一段、同一处）—— 见本函数末尾的边界说明。
///
/// ## 为什么**不**给某些 label 设白名单/黑名单
/// 官方那一步（`UniformBuffer`）不筛 label。我们的差**只**来自"没落 default"（停在 GLSL 0
/// 或我们自造的 `range_lo`）⇒ 任何人为排除都等于"把官方的值盖成我们造的 0/lo"。
/// 唯一"天然无分歧"的是 `default` **恰好等于**我们现在给的值的那些（如 `g_Direction` default=0）。
///
/// ## 为什么范围收在**特效趟**（而不是对象底图趟）
///
/// * 底图趟的 scalars 来源是**另一条路**（`lib.rs::obj_scalars`：`g_Color4/g_Color/g_Alpha/
/// g_UserAlpha` 来自对象字段），且那几支已由 `WE_OBJ_ALPHA` / `WE_OBJ_COLOR` 与 `draw_pass`
/// 的 `P_FIXED` 段治理；这里再播一遍会与该语义**打架**。
///
/// * 底图趟里剩下的分歧（`genericimage2/4` 的 `g_EmissiveBrightness default:1`、
/// `g_Roughness 0.5/0.7`、`g_Metallic`、`g_Reflectivity 1`、`g_ReflectivityDistance 4`）
/// **全部**在 `#if LIGHTING || REFLECTION` / `#if REFLECTION && NORMALMAP` / `#if EMISSIVE_MAP`
/// 守卫内（`assets/shaders/genericimage2.frag:20/23-28/45-48/118/152`）⇒ 只有开灯/反射时才是活代码。
/// 本语料默认这些 combo 关（本单实测：见 NOTES 诊断表 J 与 `WE_MAT_COMBOS` 组合护栏）。
///
/// * 诊断表（NOTES §1 表 J）已把这一类**逐条列出**，标明"声明 file:line / default / 我们现在给什么"，
/// 留给单独一单（改动面与 `WE_MAT_COMBOS` 的 combo 语义耦合）。
pub fn fx_default_all_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| crate::flag_default_on("WE_FX_DEFAULT_ALL"))
}

/// `WE_FX_DEFAULT_VEC=1`（**默认关**，与 `WE_FX_DEFAULT` / `WE_FX_DEFAULT_ALL` **独立 A/B**）：
/// 官方「**向量/矩阵族**注解 `default`」缺省机制 —— 上面两条只兜标量 `float`，向量族一律不兜
/// （我们停在 GLSL 的 0）。`foliagesway` 的 `g_CornerWeights`（`vec4` 注解 default `"1 1 0 0"`）
/// 正是被这一条打死的：场景没给 ⇒ 我们给 0 ⇒ `weight ≡ 0` ⇒ 顶点形变恒 0 ⇒
/// **该特效趟建起来了但像素 0 变化**。
///
/// ## 双出处（与 scalar 那一条**同一机制** —— 官方并没有给向量单开一条路）
///
/// * 出处①（一手，官方 shader 把自己的缺省值写在同一行注解里）：
/// `assets/effects/foliagesway/preview/shaders/effects/foliagesway.vert:9-10`
/// `uniform vec2 g_DirectionWeights; // {"material":"Direction weights","default":"1 0.2"}`
/// `uniform vec4 g_CornerWeights; // {"material":"Corner weights","default":"1 1 0 0"}`
/// （同一对在 `assets/effects/foliagesway/shaders/effects/foliagesway.vert:11-12`、
/// `assets/presets/fern/preview/shaders/effects/foliagesway.vert:9-10` 重复出现。）
///
/// * 出处②（参考实现 OWE —— 三段，逐字；与 `fx_default_all_on` 的三段**同一处**）：
///
/// 1. `src/Scene/Pkg/Parse/Shader/ShaderAnnot.cpp:190-204`，非纹理 uniform 的
/// **同一个分支**里：`default` 是**字符串** ⇒ `GetJsonValue(**value, values)` 取成
/// `Vec<float>` 再 `sv = ShaderValue(values.as_slice)`；`default` 是**数字** ⇒ 单分量。
/// ⇒ 官方把"向量 default"与"标量 default"当**同一件事**，差别只在字面量形态；
/// 那一步**既不筛 `material` label、也不筛类型**（`info->svs.insert(名字, sv)` 对每个
/// 带注解的非纹理 uniform 都执行）。
///
/// 2. `src/Scene/Pkg/Parse/Scene/SceneMaterialBuilder.cpp:500-501`
/// `shader->default_uniforms = shader_info_ref.svs.clone;` ⇒ 全部进 `default_uniforms`。
///
/// 3. `src/Scene/VulkanRender/UniformBuffer.cpp:160-174 Update` 先推 `m_defaults`
/// **再**推 `material.customShader.constValues`（作者显式值后落 ⇒ 覆盖），
/// 注入点在 `:289-294`（`shader.default_uniforms.clone` 当 `defaults` 传进去）。
///
/// * 出处③（**字面量切分规则**，一手）：`src/Core/Json.cpp:88-114` 的 `ArrayComponents`——
/// 字符串**含 `,`** ⇒ 按 `,` 切且每段 `trim_ascii`；否则按（单）空格切。
/// 另 `:143-151` 数字分支接受裸数字、`:154-172` 数组分支接受 JSON 数组。
/// ⚠ **偏差（写明）**：官方是 `split_once(" ")`，遇**连续空格**会切出空段 ⇒
/// `ParseNumber("")` 失败 ⇒ **整条 default 作废**；我们过滤空段（更宽容）。
/// 只读扫描：官方库 342 处向量 default **0 处**含连续空白 ⇒ 该偏差在当前官方素材上不可观测。
///
/// * ⚠ 诚实边界：`vrento::UniformBinding::SetParameters` 的"后落覆盖"语义在**外部包**
/// （未 vendor，与 `fx_default_all_on` 那条同源），判据是**调用方先 defaults 后 constValues
/// 的顺序**；另外"多分量 `ShaderValue` 会被整条上传（而不是只取 x）"也落在同一个外部包，
/// 标注为**按语义推断**（`ShaderValue(values.as_slice)` + `UniformParameter{name, View}`
/// 这一对调用只能这么解释）。
///
/// ## 判据（数据驱动，不按 id / 名字 / 特效名特判）
///
/// * 范围 = 注解 label（`"material"` 字段）**非空** 且声明类型属 `vec2/vec3/vec4/mat3/mat4`
/// （见 `lib.rs::ann_vec_ty`；`ivec*`/`bvec*`/`mat2` 在官方库 **0 处**，且 `Value` 没有
/// 2×2 变体、`set_uniform` 也没有整段上传 `ivec/bvec` 的路径 ⇒ **不主张**）。
///
/// * `default` 必须能**切出恰好 N 个能解析的数字**（N = 该类型分量数）；切不出 / 分量数不符
/// ⇒ **跳过**（不猜、不补 0）。官方库里唯一的反例是 `shaders/editorpaintbrush.frag:7/9`
/// （声明 `vec4` 但 default 只有 2/3 个分量）—— 那两处官方值本身也就 ≈0，跳过 ⇒ 与现状同效。
///
/// * 本趟 scalars 里已有本名（= 作者显式值）⇒ **不覆盖**（显式优先）。
///
/// * 放在 scalar 注解 default **之后**、`range_lo` 兜底 **之前**
/// （注解 default 优先于我们自造的近似值；`range_lo` 兜底只管 `float`，两者不重叠）。
///
/// * 只作用于**特效趟**分支（与 `fx_default_on` / `fx_default_all_on` 同一段）。官方是
/// **per-shader**（不区分底图/特效），我们收在特效趟是为了不与 `WE_OBJ_ALPHA` /
/// `WE_OBJ_COLOR` / `draw_pass` 的 `P_FIXED` 段打架（同 `fx_default_all_on` 的边界说明）；
/// 底图趟那几条（`genericimage2.frag` 的 `g_EmissiveColor` 等）已由诊断表逐条列出，
/// 留给单独一单。
/// 关（不设 / ≠ `"1"`）⇒ 这一段根本不执行 ⇒ 逐字节不变。
pub fn fx_default_vec_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| crate::flag_default_on("WE_FX_DEFAULT_VEC"))
}

/// `NeutralColorUniforms` 名单里那三支「alpha 族」的名字（开关打开时特效趟不吃 range 兜底）。
/// 公开给 `lib.rs` 的兜底 2 循环用（`pub` 只为跨模块调用，语义仍是本模块的）。
pub fn fx_alpha_governs(name: &str) -> bool {
    matches!(name, "g_Alpha" | "g_UserAlpha" | "g_Brightness")
}

/// 诊断用的 alpha 族名字全集（含不在本开关名单里的那几个 —— 正是为了把「按名字分清」
/// 这件事打印出来：`g_BlendAlpha`/`g_KeyAlpha`/`g_AlphaMultiply` 出现但**不**受本开关管）。
pub fn fx_dbg_family(name: &str) -> bool {
    matches!(
        name,
        "g_Color4"
            | "g_Color"
            | "g_Alpha"
            | "g_UserAlpha"
            | "g_Brightness"
            | "g_Opacity"
            | "g_BlendAlpha"
            | "g_KeyAlpha"
            | "g_AlphaMultiply"
    )
}

/// 这一趟是否携带**对象常量组**（= OWE `baseConstSvs` 对应的那一趟：对象色 + 对象 alpha 都在这里）。
/// 判据只用该组**自己**写下的两个名字 `g_Color4` / `g_Color`：全仓 grep 显示它们只有
/// `lib.rs::obj_scalars`（`lib.rs:4012`/`4016`）**一处**写入 ⇒ "本趟 = 对象底图趟"这个事实
/// 是**从数据里读**出来的，不看对象 id/名字、不按场景特判、也不新增结构体字段。
/// （对照：`g_UserAlpha` 会被 `opacity` 特效趟单独写 ⇒ 不能当判据。）
fn carries_obj_consts(ctx: &DrawCtx<'_>) -> bool {
    ctx.scalars.iter().any(|(k, _)| k == "g_Color4" || k == "g_Color")
}

/// 摆锤形变（**骨骼驱动版**）：每个顶点绕它**最近的骨骼世界锚点**做 2D 旋转，
/// 摆幅随骨链深度增长（根部≈0、末端最大），相位逐骨滞后 ⇒ "顶部绑在关节上、坠为主摆"。
/// 骨骼锚点打包存放在 `mesh_uvs` 的**尾部**（`mesh_uvs[mesh_pos.len..]`），
/// 这样不必给 `GlLayer` 加字段（该结构有三处构造点，加字段要同时改三处）。
/// 没有骨骼时退回老行为（绕顶部的水平剪切）。
pub fn sway_positions(l: &GlLayer, t: f32) -> Vec<[f32; 3]> {
    let [amp, hz, lag, y_top, y_span] = l.mesh_sway;
    if amp <= 0.0 || l.mesh_pos.is_empty() {
        return Vec::new();
    }
    let bones: Vec<[f32; 2]> = if l.mesh_uvs.len() > l.mesh_pos.len() {
        l.mesh_uvs[l.mesh_pos.len()..].to_vec()
    } else {
        Vec::new()
    };
    let span = if y_span.abs() > 1e-6 { y_span } else { 1.0 };
    let omega = std::f32::consts::TAU * hz;
    let nb = bones.len().max(2) as f32;
    l.mesh_pos
        .iter()
        .map(|p| {
            if bones.is_empty() {
                // 老行为：绕顶部的水平剪切（h=0 顶部钉死）
                let h = ((p[1] - y_top) / span).clamp(0.0, 1.0);
                let theta = amp * h * h * (omega * t - lag * h).sin();
                return [p[0] - (h * span) * theta, p[1], p[2]];
            }
            // 找最近的骨骼锚点
            let (mut bx, mut by, mut bi) = (0.0f32, 0.0f32, 0.0f32);
            let mut bd = f32::MAX;
            for (i, b) in bones.iter().enumerate() {
                let d = (p[0] - b[0]) * (p[0] - b[0]) + (p[1] - b[1]) * (p[1] - b[1]);
                if d < bd {
                    bd = d;
                    bx = b[0];
                    by = b[1];
                    bi = i as f32;
                }
            }
            let w = (bi / nb).powf(1.5); // 根部几乎不动，末端最大
            let theta = amp * w * (omega * t - lag * bi / nb).sin();
            // 绕锚点的小角度 2D 旋转
            let (dxv, dyv) = (p[0] - bx, p[1] - by);
            [p[0] - dyv * theta, p[1] + dxv * theta, p[2]]
        })
        .collect()
}

impl GlLayer {
    /// 这一层是否启用了摆锤（振幅 > 0）。字段私有，`lib.rs` 只能通过它判断。
    pub fn sway_enabled(&self) -> bool {
        self.mesh_sway[0] > 0.0
    }
}

impl Gl {
    /// 编译一对 vert/frag 源成 program（错误信息带 shader 日志）
    fn compile_layer_program(&self, vs_src: &str, fs_src: &str) -> Result<glow::Program, String> {
        unsafe {
            let gl = &self.gl;
            let vs = gl.create_shader(glow::VERTEX_SHADER).map_err(|e| e.to_string())?;
            gl.shader_source(vs, vs_src);
            gl.compile_shader(vs);
            if !gl.get_shader_compile_status(vs) {
                return Err(format!("顶点 shader 编译失败:\n{}", gl.get_shader_info_log(vs)));
            }
            let fs = gl.create_shader(glow::FRAGMENT_SHADER).map_err(|e| e.to_string())?;
            gl.shader_source(fs, fs_src);
            gl.compile_shader(fs);
            if !gl.get_shader_compile_status(fs) {
                return Err(format!("片元 shader 编译失败:\n{}", gl.get_shader_info_log(fs)));
            }
            let program = gl.create_program().map_err(|e| e.to_string())?;
            gl.attach_shader(program, vs);
            gl.attach_shader(program, fs);
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("链接失败:\n{}", gl.get_program_info_log(program)));
            }
            gl.detach_shader(program, vs);
            gl.detach_shader(program, fs);
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            Ok(program)
        }
    }

    /// 从交错顶点数据 `[x,y,z,u,v]` 建一个 VAO（属性位置按 program 查）
    fn make_vao(&self, program: glow::Program, verts: &[f32]) -> Result<glow::VertexArray, String> {
        unsafe {
            let gl = &self.gl;
            let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer().map_err(|e| e.to_string())?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(verts), glow::STATIC_DRAW);
            if let Some(loc) = gl.get_attrib_location(program, "a_Position") {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, 20, 0);
            }
            if let Some(loc) = gl.get_attrib_location(program, "a_TexCoord") {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 20, 12);
            }
            Ok(vao)
        }
    }

    /// 编译一个 WE shader（源应已经过 `shader::compose_stages*`）；`tex` 给了就顶上内置贴图槽 0
    pub fn make_layer(
        &self,
        vs_src: &str,
        fs_src: &str,
        tex: Option<(&[u8], u32, u32)>,
    ) -> Result<GlLayer, String> {
        self.make_layer_uv(vs_src, fs_src, tex, [1.0, 1.0])
    }

    /// 官方铁律③：特效/后处理趟的四边形是 **2×2 常量卡片**（`FUN_1401ea310` 传
    /// `0x4000000040000000` = `(2.0, 2.0)`），配 `ortho(2,2)` 的相机 ⇒ 顶点就是 **±1**，
    /// 与本渲染器 `[-1,1]` 的单位四边形**逐位等价**（`full_q` = 单位阵）。
    /// 唯一差别在 **uv**：官方 `FUN_1401ede30` 在 `flag & 2` 时做 **0.15 texel 内缩**
    /// （`uvEnd - 0.15/texSize`，`uv0` 不动；`flag & 4` 再翻 v）。这里的 `rt` 就是
    /// **本趟目标 RT 的像素尺寸**（= `T0`），所以内缩量与官方同一个量纲。
    pub fn make_fx_card(
        &self,
        vs_src: &str,
        fs_src: &str,
        rt: (u32, u32),
        inset_texels: f32,
    ) -> Result<GlLayer, String> {
        let (w, h) = (rt.0.max(1) as f32, rt.1.max(1) as f32);
        let d = inset_texels.max(0.0);
        self.make_layer_uv(vs_src, fs_src, None, [1.0 - d / w, 1.0 - d / h])
    }

    /// `make_layer` 的本体：`uv1` = 右上角纹理坐标（缺省 `(1,1)`；官方卡片内缩时 <1）。
    fn make_layer_uv(
        &self,
        vs_src: &str,
        fs_src: &str,
        tex: Option<(&[u8], u32, u32)>,
        uv1: [f32; 2],
    ) -> Result<GlLayer, String> {
        unsafe {
            let gl = &self.gl;
            let program = self.compile_layer_program(vs_src, fs_src)?;
            let (u1x, u1y) = (uv1[0], uv1[1]);

            // pos(vec3) + uv(vec2)，交错，stride = 20
            // ⚠ 四边形的位置是 **[-1, 1]**（不是 [-0.5, 0.5]）：WE 的全屏效果趟里有
            // `gl_Position = vec4(a_Position, 1.0)`（直接把顶点当裁剪空间用，见 `blur_precise_gaussian.vert`
            // 的 `#else VERTICAL` 分支）⇒ 只有 [-1,1] 的 quad 才能铺满屏幕。
            // 对象图层那边靠 MVP 里 `scale(size/2)` 抵消（见 `SceneRenderer::build`），净效果不变。
            // ⚠ `uv1 = (1,1)` 时与旧的硬编码数组**逐位相同**（`u1x`/`u1y` 就是 1.0）。
            let verts: [f32; 30] = [
                -1.0, -1.0, 0.0, 0.0, u1y,
                1.0, -1.0, 0.0, u1x, u1y,
                1.0, 1.0, 0.0, u1x, 0.0,
                -1.0, -1.0, 0.0, 0.0, u1y,
                1.0, 1.0, 0.0, u1x, 0.0,
                -1.0, 1.0, 0.0, 0.0, 0.0,
            ];
            let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer().map_err(|e| e.to_string())?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&verts), glow::STATIC_DRAW);
            if let Some(loc) = gl.get_attrib_location(program, "a_Position") {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, 20, 0);
            }
            if let Some(loc) = gl.get_attrib_location(program, "a_TexCoord") {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 20, 12);
            }

            // ② 同一个四边形 + **V 反向**（给裸 a_Position 的趟用，见 GlLayer::raw_position）
            let verts_flip: [f32; 30] = [
                -1.0, -1.0, 0.0, 0.0, 0.0,
                1.0, -1.0, 0.0, u1x, 0.0,
                1.0, 1.0, 0.0, u1x, u1y,
                -1.0, -1.0, 0.0, 0.0, 0.0,
                1.0, 1.0, 0.0, u1x, u1y,
                -1.0, 1.0, 0.0, 0.0, u1y,
            ];
            let vao_flip_v = gl.create_vertex_array().map_err(|e| e.to_string())?;
            gl.bind_vertex_array(Some(vao_flip_v));
            let vbo_flip = gl.create_buffer().map_err(|e| e.to_string())?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo_flip));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&verts_flip), glow::STATIC_DRAW);
            if let Some(loc) = gl.get_attrib_location(program, "a_Position") {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, 20, 0);
            }
            if let Some(loc) = gl.get_attrib_location(program, "a_TexCoord") {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 20, 12);
            }

            let tex = match tex {
                Some((rgba, tw, th)) => {
                    let t = gl.create_texture().map_err(|e| e.to_string())?;
                    gl.bind_texture(glow::TEXTURE_2D, Some(t));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        tw as i32,
                        th as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(rgba)),
                    );
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                    Some(t)
                }
                None => None,
            };

            let loc_mvp = gl.get_uniform_location(program, "g_ModelViewProjectionMatrix");
            // 链接后 MVP 变 inactive ⇒ 这一趟的 vert 走的是 `gl_Position = vec4(a_Position, 1.0)` 分支
            // （`#if` 里没取到的分支在编译期就被丢了，所以 driver 会把 uniform 优化掉）⇒ 用 V 反向的四边形。
            // 例外见 `wants_raw_position` 的注释（LIGHTING 走 `g_ViewProjectionMatrix` 的趟）。
            let raw_position = wants_raw_position(loc_mvp, vs_src);
            if raw_position && std::env::var_os("WE_DEBUG_RAW_FLIP").is_some() {
                eprintln!("[raw-flip] 这一趟不用 MVP ⇒ 用 V 反向四边形");
            }
            Ok(GlLayer {
                loc_mvp,
                program,
                vao,
                vao_flip_v,
                vao_custom: None,
                raw_position,
                dyn_token: dynamic_uniform(vs_src, fs_src),
                reads_lights: layer_reads_lights(vs_src, fs_src),
                vbo,
                tex,
                index_count: 0,
                card_half: None,
                custom_quad: false,
                mesh_pos: Vec::new(),
                mesh_uvs: Vec::new(),
                mesh_vbos: None,
                mesh_sway: [0.0; 5],
                ulocs: RefCell::new(HashMap::new()),
                ulocs_unit: RefCell::new(Vec::new()),
            })
        }
    }

    /// （`WE_FX_CARDSPACE`）：把一趟的**顶点几何**换成官方"卡片网格"空间。
///
    /// # 为什么（双出处）
    /// 官方给**图像对象每一趟**的顶点几何都是同一张卡片网格：
///
    /// * ① 一手（几何构造）：`open-wallpaper-engine/src/Scene/Pkg/Parse/Scene/
    /// SceneGeometryBuilder.cpp:19-38` `GenCardMesh`：`left = -(size.x/2)+off.x` …
    /// `right = size.x/2+off.x` ⇒ 顶点是 **`±size/2` 的对象像素空间**；
///
    /// * ② 一手（谁在用）：`.../Parse/Image/SceneImageObjectParser.cpp:325/365/372`
    /// （底图趟 `mesh`、收尾趟 `effct_final_mesh` 都用它）；
    /// `.../Scene/Scene/SceneNodeLayer.cpp:88-109` 里**每个特效趟**的网格都被
    /// `Mesh->ChangeMeshDataFrom(default_mesh)` 换成同一张卡片网格，
    /// 相机 `effect_cam` 又是 `MakeOrthographic(effect_extent.w, effect_extent.h, …)`
    /// （同文件 `SceneImageObjectParser.cpp:600-608`）⇒ 卡片网格映射到整张 RT。
    /// 我们用 **`±1` 单位四边形**，把 `scale(size/2)` 折进 MVP。两者**只在没有顶点位移时**等价；
    /// 一旦顶点着色器改 `position`（官方 shader 明确是在**对象像素**里改，例如
    /// `assets/effects/foliagesway/preview/shaders/effects/foliagesway.vert:32-33`
    /// `position.x += dot(sines,CAST4(1.0)) * g_Strength * weight * g_DirectionWeights.x`），
    /// 同一个 `δ`：官方走 `δ` 个对象像素，我们走 `δ·(size/2)` 个对象单位 ⇒ **误差 `(size/2)` 倍**。
///
    /// # 边界（**不能**换的情形）
    /// 官方文档 `open-wallpaper-engine/docs/scenejson.md:5`：`fullscreen` 才是
    /// "**signed unit quad**"特例 —— 那种趟的 shader 直接 `gl_Position = vec4(a_Position,1.0)`
    /// （官方 `assets/shaders/effects/blur_precise_gaussian.vert` 的 `#else VERTICAL` 分支；
    /// 我们的对应物见 `wants_raw_position`）。判据只用**编译产物 + 顶点源码**：
///
    /// * `g_ModelViewProjectionMatrix` 链接后仍然 active（否 ⇒ 顶点就是裁剪空间，不能换）；
///
    /// * 顶点源码**没有**引用 `g_ViewProjectionMatrix`/`g_AltViewProjectionMatrix`
    /// （有 ⇒ 那时 `g_ModelMatrix` 里折了 `scale(size/2)`、期望 `a_Position ∈ ±1`）。
    /// 返回 `(图层, card_ok)`：`card_ok=false` ⇒ 顶点**仍是单位四边形**，调用方必须**照旧**
    /// 用原来的 MVP（乘 `2/size` 会把这些趟推出裁剪体 ⇒ 整层消失）。
    /// ⚠ 本函数用**顶点源码字符串**判"是否引用 view-projection"，语义**已冻结**（`WE_FX_CARDSPACE`
    /// 上屏时就用它）⇒ 不要改。需要更精确判据的调用方用 [`Gl::make_layer_card_vp`]。
    pub fn make_layer_card(
        &self,
        vs_src: &str,
        fs_src: &str,
        half: [f32; 2],
    ) -> Result<(GlLayer, bool), String> {
        self.make_layer_card_impl(vs_src, fs_src, half, true)
    }

    /// 与 [`Gl::make_layer_card`] 同一件事，但"这一趟是不是**纯 MVP 趟**"改用**编译产物**判
    /// （链接后 `g_ViewProjectionMatrix` / `g_AltViewProjectionMatrix` / `g_MorphWeights` /
    /// `g_Bones` **是否 active**），而不是"顶点源码里有没有那个字符串"。
///
    /// # 为什么必须有它（`WE_BASE_CARDSPACE` 的底图趟）
    /// 官方底图趟用的 `assets/shaders/genericimage2.vert` 把
    /// `uniform mat4 g_ViewProjectionMatrix;` 放在 `#if LIGHTING` 里（同文件 `:55-56`）、
    /// `#define M_VP g_ViewProjectionMatrix` 放在 `#if LIGHTING || REFLECTION` 里（`:40-52`）。
    /// 默认这些 combo 全是 0 ⇒ **那两支根本没编译进去**，链接后 `g_ModelViewProjectionMatrix`
    /// 才是 active 的那个（`:164 gl_Position = mul(vec4(localPos, 1.0), M_MVP);`）——
    /// 但**源码字符串里有** `g_ViewProjectionMatrix` ⇒ 源码判据会把**所有底图趟**一律误拒
    /// （实测 `3704273480` 的 12 个作用域对象全部 `card_ok=false`，开关等于没接）。
    /// 编译产物判据只认"链接后真的 active 的那一个"：
///
    /// * LIGHTING 那一支编译进来时走 `g_ModelMatrix`(+`g_ViewProjectionMatrix`)，
    /// 那时 `g_ModelViewProjectionMatrix` 反而 inactive ⇒ **仍然被拒**（保守，安全）；
///
    /// * 纯 MVP 那一支（`M_MVP = g_ModelViewProjectionMatrix`）⇒ 放行。
    /// 另加两条**顶点位移**来源的编译期判据（官方 `genericimage3/4.vert` 的
    /// `#if MORPH / #if SKINNING` 会改 `localPos`）⇒ 那两项 active 时也拒。
    pub fn make_layer_card_vp(
        &self,
        vs_src: &str,
        fs_src: &str,
        half: [f32; 2],
    ) -> Result<(GlLayer, bool), String> {
        self.make_layer_card_impl(vs_src, fs_src, half, false)
    }

    /// [`Gl::make_layer_card`] / [`Gl::make_layer_card_vp`] 的公共实现。
    fn make_layer_card_impl(
        &self,
        vs_src: &str,
        fs_src: &str,
        half: [f32; 2],
        vp_from_source: bool,
    ) -> Result<(GlLayer, bool), String> {
        unsafe {
            let gl = &self.gl;
            let program = self.compile_layer_program(vs_src, fs_src)?;
            let loc_mvp = gl.get_uniform_location(program, "g_ModelViewProjectionMatrix");
            let loc_any = |names: &[&str]| -> bool {
                names
                    .iter()
                    .any(|n| gl.get_uniform_location(program, n).is_some())
            };
            let card_ok = if vp_from_source {
                loc_mvp.is_some()
                    && !vs_src.contains("g_ViewProjectionMatrix")
                    && !vs_src.contains("g_AltViewProjectionMatrix")
            } else {
                loc_mvp.is_some()
                    && !loc_any(&["g_ViewProjectionMatrix", "g_AltViewProjectionMatrix"])
                    // 顶点位移来源（数组 uniform 有的驱动只认带 `[0]` 的名字 ⇒ 两种都查）
                    && !loc_any(&["g_MorphWeights", "g_MorphWeights[0]"])
                    && !loc_any(&["g_Bones", "g_Bones[0]"])
            };
            let h = if card_ok { half } else { [1.0, 1.0] };
            let mk = |flip_v: bool| -> Result<(glow::VertexArray, glow::Buffer), String> {
                let verts = card_quad_verts(h, flip_v);
                let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
                gl.bind_vertex_array(Some(vao));
                let vbo = gl.create_buffer().map_err(|e| e.to_string())?;
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&verts), glow::STATIC_DRAW);
                if let Some(loc) = gl.get_attrib_location(program, "a_Position") {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, 20, 0);
                }
                if let Some(loc) = gl.get_attrib_location(program, "a_TexCoord") {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 20, 12);
                }
                Ok((vao, vbo))
            };
            let (vao, vbo) = mk(false)?;
            let (vao_flip_v, _vbo_flip) = mk(true)?;
            let raw_position = wants_raw_position(loc_mvp, vs_src);
            Ok((
                GlLayer {
                    loc_mvp,
                    program,
                    vao,
                    vao_flip_v,
                    vao_custom: None,
                    raw_position,
                    dyn_token: dynamic_uniform(vs_src, fs_src),
                    reads_lights: layer_reads_lights(vs_src, fs_src),
                    vbo,
                    tex: None,
                    index_count: 0,
                    card_half: if card_ok { Some(half) } else { None },
                    custom_quad: false,
                    mesh_pos: Vec::new(),
                    mesh_uvs: Vec::new(),
                    mesh_vbos: None,
                    mesh_sway: [0.0; 5],
                    ulocs: RefCell::new(HashMap::new()),
                    ulocs_unit: RefCell::new(Vec::new()),
                },
                card_ok,
            ))
        }
    }

    /// 用**自定义四边形**编译图层（shape 对象的"直接绘制四边形"）。
    /// `quads` = 4 个角点 `[x, y, u, v]`：x/y 是**单位四边形坐标**（[-1,1]，因为我们的
    /// MVP 按 `size/2` 缩放），u/v 是该角点的纹理坐标。内部展开成两个三角形
    /// （顶点序 `0,2,1 / 0,3,2`，与参考实现 `GenDirectDrawQuadMesh` 一致）。
    /// V 反向的那一份也一起建 —— 直接赋给 `vao`/`vao_flip_v`，这样"裸 `a_Position` 的趟要用
    /// V 反向四边形"那套逻辑对自定义网格同样生效。
    pub fn make_mesh_layer(
        &self,
        vs_src: &str,
        fs_src: &str,
        quads: &[[f32; 4]],
    ) -> Result<GlLayer, String> {
        if quads.len() < 4 {
            return Err("自定义四边形需要 4 个角点".into());
        }
        let expand = |flip_v: bool| -> Vec<f32> {
            let order = [0usize, 2, 1, 0, 3, 2];
            let mut out = Vec::with_capacity(30);
            for i in order {
                let p = quads[i];
                let v = if flip_v { 1.0 - p[3] } else { p[3] };
                out.extend_from_slice(&[p[0], p[1], 0.0, p[2], v]);
            }
            out
        };
        let program = self.compile_layer_program(vs_src, fs_src)?;
        let vao = self.make_vao(program, &expand(false))?;
        let vao_flip_v = self.make_vao(program, &expand(true))?;
        let (loc_mvp, vbo) = unsafe {
            (
                self.gl.get_uniform_location(program, "g_ModelViewProjectionMatrix"),
                self.gl.create_buffer().map_err(|e| e.to_string())?,
            )
        };
        let raw_position = wants_raw_position(loc_mvp, vs_src);
        Ok(GlLayer {
            loc_mvp,
            program,
            vao,
            vao_flip_v,
            vao_custom: None,
            raw_position,
            dyn_token: dynamic_uniform(vs_src, fs_src),
            reads_lights: layer_reads_lights(vs_src, fs_src),
            vbo,
            tex: None,
            index_count: 0,
            card_half: None,
            custom_quad: true,
            mesh_pos: Vec::new(),
            mesh_uvs: Vec::new(),
            mesh_vbos: None,
            mesh_sway: [0.0; 5],
            ulocs: RefCell::new(HashMap::new()),
            ulocs_unit: RefCell::new(Vec::new()),
        })
    }

    /// 建一个 **puppet/MDLV 索引网格**图层：`positions`/`uvs` 是模型自身坐标
    /// （调用方已归一化到 ±1，x 右、y 上已翻成我们的 y 下），`indices` 是 uint16 三元组。
    /// 与 `make_mesh_layer`（shape 的四角小四边形）不同：那条路是 6 个顶点 `draw_arrays`，
    /// 这条路是真的元素缓冲 + `draw_elements`（网格顶点数任意）。
    pub fn make_indexed_layer(
        &self,
        vs_src: &str,
        fs_src: &str,
        positions: &[[f32; 3]],
        uvs: &[[f32; 2]],
        indices: &[u16],
        sway: [f32; 5],
    ) -> Result<GlLayer, String> {
        if positions.is_empty() || uvs.is_empty() || indices.len() < 3 {
            return Err(format!(
                "网格数据不合法：顶点 {}、uv {}、索引 {}",
                positions.len(),
                uvs.len(),
                indices.len()
            ));
        }
        // MDLV 的启发式扫描会让 **uv 比顶点多**（实测同场景不同模型：多 1、多 2 …）。
        // 这不是坏数据 —— 下面建缓冲用的是 `zip`，多余 uv 自动忽略。以前这里要求严格
        // 相等 ⇒ 整张贴图的 puppet 部件全部被拒收、退回平面贴图 ⇒ **人物散架**
        // （用户看到的"当前上屏的壁纸是错的"）。这里只保留一条"差太多就报警"的宽松护栏。
        if uvs.len() < positions.len() {
            return Err(format!(
                "网格数据不合法（uv 少于顶点）：顶点 {}、uv {}、索引 {}",
                positions.len(),
                uvs.len(),
                indices.len()
            ));
        }
        if uvs.len() > positions.len() {
            eprintln!(
                "[网格] uv 比顶点多 {} 个（顶点 {} / uv {}）⇒ 按顶点数截断使用",
                uvs.len() - positions.len(),
                positions.len(),
                uvs.len()
            );
        }
        let program = self.compile_layer_program(vs_src, fs_src)?;
        // 顶点布局与四边形一致的 20 字节跨度（pos 3@0 + uv 2@12），只是任意顶点数 + 元素缓冲。
        let build = |flip_v: bool| -> Result<(glow::VertexArray, glow::Buffer), String> {
            unsafe {
                let gl = &self.gl;
                let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
                gl.bind_vertex_array(Some(vao));
                let mut data: Vec<f32> = Vec::with_capacity(positions.len() * 5);
                for (p, t) in positions.iter().zip(uvs.iter()) {
                    let v = if flip_v { 1.0 - t[1] } else { t[1] };
                    data.extend_from_slice(&[p[0], p[1], p[2], t[0], v]);
                }
                let vbo = gl.create_buffer().map_err(|e| e.to_string())?;
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&data), glow::STATIC_DRAW);
                for name in ["a_Position"] {
                    if let Some(loc) = gl.get_attrib_location(program, name) {
                        gl.enable_vertex_attrib_array(loc);
                        gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, 20, 0);
                    }
                }
                // WE 的图片类 shader 用 `a_TexCoord`，有些用 `a_TexCoordVec4`；两个都试。
                if let Some(loc) = gl.get_attrib_location(program, "a_TexCoord") {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 20, 12);
                }
                if let Some(loc) = gl.get_attrib_location(program, "a_TexCoordVec4") {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 20, 12);
                }
                let ebo = gl.create_buffer().map_err(|e| e.to_string())?;
                gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
                // 索引是 u16，`bytemuck_cast` 只吃 &[f32] ⇒ 自己转字节
                // 判决性实验：`WE_MESH_SEQ=1` 忽略文件里的索引，改用顺序索引 0,1,2,…
                // 连接数保持不变（用原索引长度截断）。
                // 用途：区分"索引上传/计数错"（⇒ 顺序索引会变成连片）与
                // "顶点属性绑定错"（⇒ 顺序索引仍然空白/乱）。
                let idx_bytes: Vec<u8> = if std::env::var("WE_MESH_SEQ").is_ok() {
                    (0..(indices.len().min(positions.len())) as u16)
                        .flat_map(|i| i.to_le_bytes())
                        .collect()
                } else {
                    indices.iter().flat_map(|i| i.to_le_bytes()).collect()
                };
                gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, &idx_bytes, glow::STATIC_DRAW);
                gl.bind_vertex_array(None);
                Ok((vao, vbo))
            }
        };
        let (vao, mesh_vbo_a) = build(false)?;
        let (vao_flip_v, mesh_vbo_b) = build(true)?;
        let (loc_mvp, vbo) = unsafe {
            (
                self.gl
                    .get_uniform_location(program, "g_ModelViewProjectionMatrix"),
                self.gl.create_buffer().map_err(|e| e.to_string())?,
            )
        };
        let raw_position = wants_raw_position(loc_mvp, vs_src);
        Ok(GlLayer {
            loc_mvp,
            program,
            vao,
            vao_flip_v,
            vao_custom: None,
            raw_position,
            dyn_token: dynamic_uniform(vs_src, fs_src),
            reads_lights: layer_reads_lights(vs_src, fs_src),
            vbo,
            tex: None,
            index_count: indices.len() as i32,
            card_half: None,
            custom_quad: true,
            mesh_pos: positions.to_vec(),
            mesh_uvs: uvs.to_vec(),
            mesh_vbos: Some([mesh_vbo_a, mesh_vbo_b]),
            mesh_sway: sway,
            ulocs: RefCell::new(HashMap::new()),
            ulocs_unit: RefCell::new(Vec::new()),
        })
    }

    /// 把形变后的顶点重传给**这个图层真正会用到的那一套**顶点缓冲（uv 沿用建层时存的）。
    /// ⚠ 以前两套都传（slot 0 = 非翻 V、slot 1 = 翻 V）。但 `draw_pass` 只按 `raw_position`
    /// 在 `vao` / `vao_flip_v` 里**二选一**绑定 ⇒ 另一套永远不会被画到，每帧白传一份顶点
    /// （B 场景 5685 顶点 ⇒ 白传 113KB + 一次 `Vec` 分配）。这里两处都改掉：只传用的那套，
    /// 并且复用线程局部缓冲而不是每次新建 `Vec`。
    pub fn update_layer_mesh(&self, l: &GlLayer, positions: &[[f32; 3]]) -> Result<(), String> {
        let Some(vbos) = l.mesh_vbos else {
            return Ok(());
        };
        if positions.len() != l.mesh_pos.len() {
            return Err(format!(
                "摆锤顶点数不匹配：{} vs {}",
                positions.len(),
                l.mesh_pos.len()
            ));
        }
        let (slot, flip_v) = if l.raw_position { (1usize, true) } else { (0usize, false) };
        let buf = vbos[slot];
        MESH_SCRATCH.with(|s| {
            let mut data = s.borrow_mut();
            data.clear();
            data.reserve(positions.len() * 5);
            for (p, t) in positions.iter().zip(l.mesh_uvs.iter()) {
                let v = if flip_v { 1.0 - t[1] } else { t[1] };
                data.extend_from_slice(&[p[0], p[1], p[2], t[0], v]);
            }
            unsafe {
                let gl = &self.gl;
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(buf));
                gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytemuck_cast(data.as_slice()));
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
            }
        });
        Ok(())
    }

    /// 画一个已编译好的图层（用它的内置贴图；没有就只画几何）
    pub fn draw_layer(&self, l: &GlLayer, mvp: [f32; 16]) -> Result<(), String> {
        let mut units: Vec<UnitTex> = Vec::new();
        if let Some(t) = l.tex {
            units.push(UnitTex::new(0, t, 1, 1));
        }
        let ctx = DrawCtx {
            target: None,
            mvp,
            units: &units,
            scalars: &[],
            time: None,
            blend: Blend::Opaque,
        };
        self.draw_pass(l, &ctx)
    }
}

impl Gl {
    /// 一次性画一个四边形（测试/CLI 用；实时路径请用 make_layer + draw_pass）
    pub fn draw_quad(
        &self,
        vert_src: &str,
        frag_src: &str,
        mvp: [f32; 16],
        tex_rgba: &[u8],
        tex_w: u32,
        tex_h: u32,
    ) -> Result<(), String> {
        let layer = self.make_layer(vert_src, frag_src, Some((tex_rgba, tex_w, tex_h)))?;
        self.draw_layer(&layer, mvp)
    }
}

/// （`WE_FX_CARDSPACE`）：交错顶点 `[x,y,z,u,v]`（两个三角形）——
/// **顶点序与 uv 与 `Gl::make_layer` 的那两个字面量数组逐位一致**，
/// 只是位置乘上调用方给的两个半尺寸（`half = [1,1]` ⇒ 就是原来那个单位四边形：
/// `×1.0` 是精确运算，`[0,0]`/`[1,1]` 的 uv 一个位都不差）。
/// `half = 官方卡片网格的半尺寸`（= `GenCardMesh` 的 `size/2`）⇒ 顶点落在
/// **对象像素空间**，与官方 shader 里 `position` 的语义一致（见 `Gl::make_layer_card`）。
pub fn card_quad_verts(half: [f32; 2], flip_v: bool) -> [f32; 30] {
    // 两个三角形：(-1,-1),(1,-1),(1,1) 与 (-1,-1),(1,1),(-1,1)
    const P: [[f32; 2]; 6] = [
        [-1.0, -1.0],
        [1.0, -1.0],
        [1.0, 1.0],
        [-1.0, -1.0],
        [1.0, 1.0],
        [-1.0, 1.0],
    ];
    const UV: [[f32; 2]; 6] = [
        [0.0, 1.0],
        [1.0, 1.0],
        [1.0, 0.0],
        [0.0, 1.0],
        [1.0, 0.0],
        [0.0, 0.0],
    ];
    let mut v = [0.0f32; 30];
    for i in 0..6 {
        v[i * 5] = P[i][0] * half[0];
        v[i * 5 + 1] = P[i][1] * half[1];
        v[i * 5 + 2] = 0.0;
        v[i * 5 + 3] = UV[i][0];
        v[i * 5 + 4] = if flip_v { 1.0 - UV[i][1] } else { UV[i][1] };
    }
    v
}

/// `[f32]` → `[u8]`（避免为这点事引 bytemuck）
fn bytemuck_cast(floats: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(floats.as_ptr() as *const u8, std::mem::size_of_val(floats)) }
}

/// GL 原点在左下，PNG 在左上
pub fn flip_vertical(buf: &mut [u8], w: u32, h: u32) {
    let row = (w * 4) as usize;
    for y in 0..(h as usize / 2) {
        let (a, b) = (y * row, (h as usize - 1 - y) * row);
        for i in 0..row {
            buf.swap(a + i, b + i);
        }
    }
}

// ── 0 拷贝模式（EGL wayland 平台 + wl_egl_window）─────────────────────────
// 为什么值得单开一条路：旧路每帧要把整屏像素从 GPU 搬回 CPU（2560x1440 就是读 14.7MB +
// 写 14.7MB），实测 9.4ms/帧 = 0.188 个核。窗口模式下缓冲由驱动/合成器直接交换，
// CPU 只发 GL 命令（实测 0.50ms/帧 = 0.029 个核、57.7fps）。

/// Submit one frame to the compositor WITHOUT panicking .
/// khronos-egl 6.0.0's swap_buffers ends in `get_error.unwrap`. When the
/// compositor is gone Mesa returns FALSE while eglGetError still reports
/// EGL_SUCCESS, so unwrap sees None and panics; with panic=abort that becomes
/// SIGABRT(6). We call eglSwapBuffers ourselves and only return Err.
fn egl_swap_checked(
    lib: &egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    surf: egl::Surface,
) -> Result<(), String> {
    type SwapFn = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> u32;
    let f = lib
        .get_proc_address("eglSwapBuffers")
        .ok_or("驱动没提供 eglSwapBuffers")?;
    let f: SwapFn = unsafe { std::mem::transmute(f) };
    if unsafe { f(display.as_ptr(), surf.as_ptr()) } != 0 {
        return Ok(());
    }
    let err = lib
        .get_error()
        .map(|e| format!("{e:?}"))
        .unwrap_or_else(|| "EGL_SUCCESS（eglGetError 没报错，通常是显示连接已断）".to_string());
    Err(format!(
        "EGL 提交失败（eglSwapBuffers=FALSE，EGL error={err}）"
    ))
}

/// 建完窗口表面后先预热一次（把默认帧缓冲的颜色附件**分配出来**）：
/// 不这么做，第一次 `draw` 时 FBO 0 状态是 0x8cd7（缺附件）⇒ 每趟 0x506。
/// **返回值一定要看** ✗：swap 失败就说明缓冲压根没分配出来，FBO 0 永远不完整。
/// 切换壁纸的过渡：`preload` 有值时，这次预热**画的是旧壁纸的最后一帧**而不是清黑。
/// 于是「旧渲染器已丢、新场景还在构建（几百毫秒）」这段时间里，合成器手上一直是旧画面
/// ⇒ **看不到黑屏**（这就是用户报的"切换时 2s 黑屏"的修法第一半）。
/// 传进来的像素是**图像取向**（第 0 行 = 画面顶）；上传时按 `flip` 决定是否翻行，
/// 使它与 `SceneRenderer::make_fade_layer` 那套四边形的朝向一致（两者用同一条几何）。
unsafe fn gl_layer_warmup(
    lib: &egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    surf: egl::Surface,
    w: u32,
    h: u32,
    preload: Option<(&[u8], u32, u32, bool)>,
) -> Result<(), String> {
    use glow::HasContext as _;
    // 这里只需要 clear/draw 这几个 GL 调用，用加载器临时取一份（和正式 Gl 里的无关）
    let gl = glow::Context::from_loader_function(|s| {
        lib.get_proc_address(s)
            .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
    });
    gl.viewport(0, 0, w as i32, h as i32);
    match preload {
        None => {
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        Some((px, pw, ph, flip)) => {
            draw_fullscreen_rgba_oneshot(&gl, px, pw, ph, flip)?;
        }
    }
    let e = gl.get_error();
    if e != glow::NO_ERROR {
        return Err(format!("预热绘制时报 GL 错 0x{e:x}"));
    }
    egl_swap_checked(lib, display, surf)
}

/// 全屏四边形 + 一张**图像取向**的 RGBA 纹理，一次性（编译/销毁都在这函数里）。
/// 几何与 `Gl::make_layer` 内置的四边形**逐字节相同**，顶点直接是裁剪空间（单位阵 MVP）：
/// 位置 `[-1,1]`、`uv=(0,0)` 落在**屏幕左上**（见 make_layer 里的顶点表）。
/// 过渡期的覆盖层（`SceneRenderer::make_fade_layer`）走的是同一套几何 ⇒
/// 这里的朝向能被离线渲染（`--size … out.png`）逐像素验证。
unsafe fn draw_fullscreen_rgba_oneshot(
    gl: &glow::Context,
    rgba: &[u8],
    w: u32,
    h: u32,
    flip: bool,
) -> Result<(), String> {
    use glow::HasContext as _;
    let cv = "#version 120\nattribute vec3 a_Position;\nattribute vec2 a_TexCoord;\n\
              varying vec2 v_TexCoord;\nvoid main(){ gl_Position = vec4(a_Position, 1.0); \
              v_TexCoord = a_TexCoord; }\n";
    let cf = "#version 120\nvarying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\n\
              void main(){ gl_FragColor = texture2D(g_Texture0, v_TexCoord); }\n";
    let mk = |kind: u32, src: &str| -> Result<glow::Shader, String> {
        let s = gl.create_shader(kind).map_err(|e| e.to_string())?;
        gl.shader_source(s, src);
        gl.compile_shader(s);
        if !gl.get_shader_compile_status(s) {
            return Err(format!("预热 shader 编译失败：{}", gl.get_shader_info_log(s)));
        }
        Ok(s)
    };
    let vs = mk(glow::VERTEX_SHADER, cv)?;
    let fs = mk(glow::FRAGMENT_SHADER, cf)?;
    let prog = gl.create_program().map_err(|e| e.to_string())?;
    gl.attach_shader(prog, vs);
    gl.attach_shader(prog, fs);
    gl.link_program(prog);
    if !gl.get_program_link_status(prog) {
        return Err(format!(
            "预热 shader 链接失败：{}",
            gl.get_program_info_log(prog)
        ));
    }
    // 与 make_layer 同一套顶点（位置 [-1,1] + uv，交错 stride 20），保证朝向一致
    #[rustfmt::skip]
    let verts: [f32; 30] = [
        -1.0, -1.0, 0.0,  0.0, 1.0,
         1.0, -1.0, 0.0,  1.0, 1.0,
         1.0,  1.0, 0.0,  1.0, 0.0,
        -1.0, -1.0, 0.0,  0.0, 1.0,
         1.0,  1.0, 0.0,  1.0, 0.0,
        -1.0,  1.0, 0.0,  0.0, 0.0,
    ];
    let vao = gl.create_vertex_array().map_err(|e| e.to_string())?;
    gl.bind_vertex_array(Some(vao));
    let vbo = gl.create_buffer().map_err(|e| e.to_string())?;
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&verts), glow::STATIC_DRAW);
    if let Some(loc) = gl.get_attrib_location(prog, "a_Position") {
        gl.enable_vertex_attrib_array(loc);
        gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, 20, 0);
    }
    if let Some(loc) = gl.get_attrib_location(prog, "a_TexCoord") {
        gl.enable_vertex_attrib_array(loc);
        gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 20, 12);
    }
    let data = if flip { flip_rows(rgba, w, h) } else { rgba.to_vec() };
    let tex = gl.create_texture().map_err(|e| e.to_string())?;
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RGBA8 as i32,
        w as i32,
        h as i32,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(Some(&data)),
    );
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    gl.use_program(Some(prog));
    gl.active_texture(glow::TEXTURE0);
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    if let Some(loc) = gl.get_uniform_location(prog, "g_Texture0") {
        gl.uniform_1_i32(Some(&loc), 0);
    }
    gl.disable(glow::BLEND);
    gl.disable(glow::DEPTH_TEST);
    gl.disable(glow::CULL_FACE);
    gl.draw_arrays(glow::TRIANGLES, 0, 6);
    gl.bind_vertex_array(None);
    // 一次性对象，用完即还（下一帧/下一次预热会重建）
    gl.delete_program(prog);
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    gl.delete_buffer(vbo);
    gl.delete_vertex_array(vao);
    gl.delete_texture(tex);
    Ok(())
}

/// `EGL_PLATFORM_WAYLAND_EXT`（khronos-egl 没定义这个常量）
const EGL_PLATFORM_WAYLAND_EXT: u32 = 0x31D8;

/// libwayland-egl 只有 `.so.1`（没有给链接器用的 `.so`）⇒ 只能 dlopen
fn load_wl_egl() -> Result<
    (
        unsafe extern "C" fn(*mut std::ffi::c_void, i32, i32) -> *mut std::ffi::c_void,
        unsafe extern "C" fn(*mut std::ffi::c_void),
    ),
    String,
> {
    unsafe {
        let h = libc::dlopen(
            c"libwayland-egl.so.1".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if h.is_null() {
            return Err("dlopen(libwayland-egl.so.1) 失败".into());
        }
        let c = libc::dlsym(h, c"wl_egl_window_create".as_ptr());
        let d = libc::dlsym(h, c"wl_egl_window_destroy".as_ptr());
        if c.is_null() || d.is_null() {
            return Err("找不到 wl_egl_window_create/destroy".into());
        }
        Ok((std::mem::transmute(c), std::mem::transmute(d)))
    }
}

impl Gl {
    /// 0 拷贝模式：用 **wayland 平台**的 EGL display + `wl_egl_window` 建窗口表面。
    /// `display_ptr` / `surface_ptr` 是裸的 `wl_display*` / `wl_surface*`
    /// （`wl_display*` 只有 wayland-client 的 **sys 后端**才拿得到）。
///
    /// # Safety
    /// 两个指针必须来自同一个活着的 Wayland 连接，且 surface 还没被销毁。
    pub unsafe fn new_window(
        w: u32,
        h: u32,
        display_ptr: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
        preload: Option<(&[u8], u32, u32, bool)>,
    ) -> Result<Gl, String> {
        let lib = egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
            .map_err(|e| format!("加载 libEGL.so.1 失败（LD_LIBRARY_PATH 设了吗）：{e}"))?;
        // 必须走 wayland 平台：`eglGetDisplay(EGL_DEFAULT_DISPLAY)` 给的是 surfaceless
        // display，**没有绑 DRM 设备**，一碰 EGLImage/导出就段错误
        let display = lib
            .get_platform_display(
                EGL_PLATFORM_WAYLAND_EXT,
                display_ptr,
                &[egl::NONE as egl::Attrib],
            )
            .map_err(|e| format!("eglGetPlatformDisplay(WAYLAND): {e}"))?;
        let (maj, min) = lib
            .initialize(display)
            .map_err(|e| format!("eglInitialize: {e}"))?;
        lib.bind_api(egl::OPENGL_API)
            .map_err(|e| format!("eglBindAPI(OpenGL): {e}"))?;
        let cfg_attribs: [egl::Int; 15] = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
            egl::NONE,
            egl::NONE,
        ];
        let config = lib
            .choose_first_config(display, &cfg_attribs)
            .map_err(|e| format!("eglChooseConfig: {e}"))?
            .ok_or_else(|| "没有满足 WINDOW_BIT+OPENGL 的 EGLConfig".to_string())?;
        if std::env::var("WE_DEBUG_FBO").is_ok() {
            let g = |a: egl::Int| lib.get_config_attrib(display, config, a).unwrap_or(-1);
            println!(
                "[EGLConfig] SURFACE_TYPE=0x{:x} RENDERABLE_TYPE=0x{:x} R/G/B/A={}/{}/{}/{}",
                g(egl::SURFACE_TYPE),
                g(egl::RENDERABLE_TYPE),
                g(egl::RED_SIZE),
                g(egl::GREEN_SIZE),
                g(egl::BLUE_SIZE),
                g(egl::ALPHA_SIZE),
            );
        }
        let context = lib
            .create_context(display, config, None, &[egl::NONE])
            .map_err(|e| format!("eglCreateContext: {e}"))?;
        let (create_window, destroy_window) = load_wl_egl()?;
        let window = create_window(surface_ptr, w as i32, h as i32);
        if window.is_null() {
            return Err("wl_egl_window_create 返回空".into());
        }
        let surf = lib
            .create_window_surface(display, config, window, None)
            .map_err(|e| format!("eglCreateWindowSurface: {e}"))?;
        lib.make_current(display, Some(surf), Some(surf), Some(context))
            .map_err(|e| format!("eglMakeCurrent: {e}"))?;
        // 跟 vblank 走：合成器自己节流，不用我们 sleep（帧率也自然贴屏）
        let _ = lib.swap_interval(display, 1);
        // 先清一次 + swap 一次：**窗口表面在第一次 swap 之前，默认帧缓冲（FBO 0）
        // 还没有颜色附件** —— `checkFramebufferStatus` 会报 0x8cd7
        // (INCOMPLETE_MISSING_ATTACHMENT)，随后任何一趟 draw_pass 都是 GL_INVALID_OPERATION
        // (0x506)。实测就是这个坑：0 拷贝模式一开就每帧失败、疯狂重建、烧 1 个核 ✗。
        unsafe {
            gl_layer_warmup(&lib, display, surf, w, h, preload)?;
        }
        let gl = glow::Context::from_loader_function(|s| {
            lib.get_proc_address(s)
                .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
        });
        let (vendor, renderer, version, fbo, color);
        {
            use glow::HasContext as _;
            vendor = gl.get_parameter_string(glow::VENDOR);
            renderer = gl.get_parameter_string(glow::RENDERER);
            version = gl.get_parameter_string(glow::VERSION);
            // 窗口模式下这两个用不到（回读那条路才用），给个 1x1 占位 ——
            // 免得把 fbo/color 弄成 Option 波及一大堆调用点
            color = gl.create_texture().map_err(|e| e.to_string())?;
            gl.bind_texture(glow::TEXTURE_2D, Some(color));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                1,
                1,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            fbo = gl.create_framebuffer().map_err(|e| e.to_string())?;
        }
        println!("[GL] EGL {maj}.{min} wayland 平台 ✓ 0 拷贝模式  {renderer} / {version}");
        Ok(Gl {
            gl,
            fbo,
            color,
            w,
            h,
            vendor,
            renderer,
            version,
            _lib: lib,
            _display: display,
            _context: context,
            egl_surface: Some(surf),
            wl_window: Some((window, destroy_window)),
            lights: None,
            committer: None,
            current_here: AtomicBool::new(true),
            last_swap_ms: AtomicU64::new(0),
            pt: std::sync::Mutex::new(PassQ::default()),
        })
    }

    /// 释放一张贴图（过渡结束后把旧帧的纹理还回去，不占显存）。纹理已删也安全。
    pub fn free_texture(&self, t: glow::Texture) {
        unsafe {
            self.gl.delete_texture(t);
        }
    }

    /// 是 0 拷贝窗口模式吗（不是 = 老的 surfaceless + 回读）
    pub fn is_window(&self) -> bool {
        self.egl_surface.is_some()
    }

    /// Is a commit still in flight (requested or running)?
    pub fn commit_pending(&self) -> bool {
        let Some(c) = self.committer.as_ref() else {
            return false;
        };
        matches!(*c.lock(), CommitState::Requested | CommitState::Busy)
    }

    /// Collect a finished commit, if any, and close the SwapBegin/SwapEnd span.
    pub fn poll_commit(&self) -> Option<Result<(), String>> {
        let c = self.committer.as_ref()?;
        let (r, ms) = {
            let st = c.lock();
            match &*st {
                CommitState::Finished(r, ms) => (r.clone(), *ms),
                _ => return None,
            }
        };
        {
            let mut st = c.lock();
            *st = CommitState::Idle;
        }
        crate::trace::mark(crate::trace::Mark::SwapEnd);
        crate::trace::bump_swap();
        self.last_swap_ms
            .store(ms.max(0.0).round() as u64, Ordering::Relaxed);
        Some(r)
    }

    /// Wall time of the last collected swap in ms (0 = none collected yet).
    pub fn last_swap_ms(&self) -> u64 {
        self.last_swap_ms.load(Ordering::Relaxed)
    }

    /// Take the context back after the commit thread released it.
    pub fn ensure_current(&self) {
        if self.current_here.load(Ordering::Relaxed) {
            return;
        }
        if let Some(s) = self.egl_surface {
            if self
                ._lib
                .make_current(self._display, Some(s), Some(s), Some(self._context))
                .is_ok()
            {
                self.current_here.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Bounded commit: run eglSwapBuffers on the `we-commit` thread and wait at most
    /// `deadline_ms` for it here. Never blocks longer than the deadline.
    pub fn swap_bounded(&mut self, deadline_ms: u64) -> CommitOutcome {
        let Some(surf) = self.egl_surface else {
            return CommitOutcome::Done(Ok(()));
        };
        if self.committer.is_none() {
            self.committer = Some(Committer::spawn(
                self._display.as_ptr() as usize,
                surf.as_ptr() as usize,
                self._context.as_ptr() as usize,
            ));
        }
        if let Some(r) = self.poll_commit() {
            return CommitOutcome::Done(r);
        }
        let Some(c) = self.committer.as_ref() else {
            return CommitOutcome::Done(Err("commit thread not started".into()));
        };
        if matches!(*c.lock(), CommitState::Requested | CommitState::Busy) {
            return CommitOutcome::StillPending;
        }
        // The context may be current on one thread at a time: let go before handing off.
        let _ = self._lib.make_current(self._display, None, None, None);
        self.current_here.store(false, Ordering::Relaxed);
        {
            let mut st = c.lock();
            *st = CommitState::Requested;
        }
        c.slot.cv.notify_all();
        // the frame is submitted **here**; collecting the result is a later lap's
        // job (`poll_commit` bumps `swaps`, this bumps `submits`).
        crate::trace::bump_submit();
        crate::trace::mark(crate::trace::Mark::SwapBegin);
        let deadline = std::time::Duration::from_millis(deadline_ms.max(1));
        let st = c.lock();
        let (st, _timed_out) = c
            .slot
            .cv
            .wait_timeout_while(st, deadline, |s| !matches!(s, CommitState::Finished(..)))
            .unwrap_or_else(|e| e.into_inner());
        let got = match &*st {
            CommitState::Finished(r, ms) => Some((r.clone(), *ms)),
            _ => None,
        };
        drop(st);
        match got {
            Some((r, ms)) => {
                {
                    let mut st = c.lock();
                    *st = CommitState::Idle;
                }
                crate::trace::mark(crate::trace::Mark::SwapEnd);
                crate::trace::bump_swap();
                self.last_swap_ms
                    .store(ms.max(0.0).round() as u64, Ordering::Relaxed);
                CommitOutcome::Done(r)
            }
            None => {
                // Close the span at the point we stop waiting, so the slow-lap line shows
                // `swap <deadline>` instead of folding the wait into "rest". The lap did not
                // present a frame, so the swap counter is not bumped here.
                crate::trace::mark(crate::trace::Mark::SwapEnd);
                CommitOutcome::Pending
            }
        }
    }

    /// 把一块 dmabuf 平面导入成 GL 贴图（`EGL_LINUX_DMA_BUF_EXT`）。
    /// 返回 `(贴图, DmaBufImage)`：**图像必须活到贴图用完**（销毁顺序：先删贴图再 `release_dma_buf`）。
    /// `fourcc` 直接来自导出方（NV12 = 平面0 `` + 平面1 `GR88`）
    /// **modifier 必须原样带上** —— 本机 VAAPI 出的 NV12 带 AMD 平铺 modifier
    /// （实测 `0x200000000000901` = AMD_FMT_MOD + TILE_VER_GFX9 + TILE `GFX9_64K_S`，DCC 位=0），
    /// 当成线性布局去读画面就花了。驱动不认这个 modifier 时会直接 `EGL_BAD_PARAMETER`
    /// （用 `query_dmabuf_modifiers` 能问出它支持哪些）。
    /// 这里**不用 `khronos_egl::Instance::create_image`**：它把属性表声明成 `&[Attrib]`
    /// 而 `Attrib = usize`（8 字节/项），可 EGL 的 `eglCreateImage` 要的是 `EGLint*`
    /// （4 字节/项）—— 于是 EGL 把 `EGL_WIDTH`(0x3057) 之后的高 32 位当成它的值（=0），
    /// 每个属性都错位，直接 `EGL_BAD_PARAMETER`（实测踩过：报错文本是
    /// "One or more argument values are invalid"，很容易误判成 modifier 不被支持）。
    /// 自己取函数指针、传真正的 i32 数组。
    /// 注意：只能在**带 DRM 设备的 display** 上用（`new_window` 那条 EGL wayland 平台）。
    /// `Gl::new` 的 `EGL_DEFAULT_DISPLAY` 没有绑设备，碰 EGLImage 会段错误（见 006 README）。
    pub fn import_dma_buf(
        &self,
        fourcc: u32,
        w: u32,
        h: u32,
        plane: &DmaBufPlane,
    ) -> Result<(glow::Texture, DmaBufImage), String> {
        const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
        const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
        const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
        const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
        const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
        const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
        const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
        if !self.is_window() {
            return Err("dmabuf 导入需要带 DRM 设备的 EGL display（0 拷贝窗口那条路）".into());
        }
        let attrs: [i32; 17] = [
            egl::WIDTH,
            w as i32,
            egl::HEIGHT,
            h as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.pitch as i32,
            // WE_VIDEO_IMPORT=linear：把 modifier 属性整段去掉（当线性布局试）
            // 诊断用：能导进去就说明"属性表本身没问题，是 modifier 那条路不行"
            if std::env::var("WE_VIDEO_IMPORT").as_deref() == Ok("linear") {
                egl::NONE
            } else {
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT
            },
            (plane.modifier & 0xffff_ffff) as i32,
            if std::env::var("WE_VIDEO_IMPORT").as_deref() == Ok("linear") {
                egl::NONE
            } else {
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT
            },
            (plane.modifier >> 32) as i32,
            egl::NONE,
        ];
        type CreateImageFn = unsafe extern "C" fn(
            *mut std::ffi::c_void, // dpy
            *mut std::ffi::c_void, // ctx
            u32,                   // target
            *mut std::ffi::c_void, // buffer
            *const i32,            // attrib_list（EGLint*，不是 usize*）
        ) -> *mut std::ffi::c_void;
        let create = self
            ._lib
            .get_proc_address("eglCreateImageKHR")
            .or_else(|| self._lib.get_proc_address("eglCreateImage"))
            .ok_or("驱动没提供 eglCreateImage")?;
        let create: CreateImageFn = unsafe { std::mem::transmute(create) };
        let image = unsafe {
            create(
                self._display.as_ptr(),
                // ctx 必须是 EGL_NO_CONTEXT：EGL_EXT_image_dma_buf_import 明确规定
                // （"ctx must be EGL_NO_CONTEXT for EGL_LINUX_DMA_BUF_EXT"），
                // 报错发生在 _eglCreateImageCommon，跟 modifier/格式一点关系都没有，
                // 特别容易被误导去查 modifier（实测踩过）。
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attrs.as_ptr(),
            )
        };
        if image.is_null() {
            let err = self._lib.get_error().map(|e| format!("{e:?}")).unwrap_or_default();
            return Err(format!(
                "eglCreateImage(dmabuf) 失败: {err}（fourcc 0x{fourcc:x} {w}x{h} pitch={} off={} mod=0x{:x}）",
                plane.pitch, plane.offset, plane.modifier
            ));
        }
        unsafe {
            let tex = self.gl.create_texture().map_err(|e| e.to_string())?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32),
            ] {
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, p, v);
            }
            // glow 0.16 没有包装 glEGLImageTargetTexture2DOES ⇒ 自己从 EGL 取函数指针
            let target_tex = self
                ._lib
                .get_proc_address("glEGLImageTargetTexture2DOES")
                .ok_or("驱动没提供 glEGLImageTargetTexture2DOES")?;
            let target_tex: unsafe extern "C" fn(u32, *mut std::ffi::c_void) =
                std::mem::transmute(target_tex);
            target_tex(glow::TEXTURE_2D, image);
            let err = self.gl.get_error();
            if err != glow::NO_ERROR {
                self.gl.delete_texture(tex);
                let _ = self.destroy_dma_buf_image(DmaBufImage(image));
                return Err(format!("glEGLImageTargetTexture2D 报 0x{err:x}"));
            }
            Ok((tex, DmaBufImage(image)))
        }
    }

    /// 销毁 dmabuf 导入用的贴图 + EGLImage（顺序不能反）
    pub fn release_dma_buf(&self, tex: glow::Texture, image: DmaBufImage) {
        unsafe { self.gl.delete_texture(tex) };
        let _ = self.destroy_dma_buf_image(image);
    }

    fn destroy_dma_buf_image(&self, image: DmaBufImage) -> Result<(), String> {
        type DestroyImageFn =
            unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> u32;
        let f = self
            ._lib
            .get_proc_address("eglDestroyImageKHR")
            .or_else(|| self._lib.get_proc_address("eglDestroyImage"))
            .ok_or("驱动没提供 eglDestroyImage")?;
        let f: DestroyImageFn = unsafe { std::mem::transmute(f) };
        let ok = unsafe { f(self._display.as_ptr(), image.0) };
        if ok == 0 {
            return Err("eglDestroyImage 失败".into());
        }
        Ok(())
    }

    /// 问驱动：这个 fourcc 支持哪些 dmabuf modifier + 每个是否 external-only（诊断用）
    pub fn query_dmabuf_modifiers(&self, fourcc: u32) -> Vec<(u64, bool)> {
        let Some(f) = self._lib.get_proc_address("eglQueryDmaBufModifiersEXT") else {
            return Vec::new();
        };
        type QueryFn = unsafe extern "C" fn(
            *mut std::ffi::c_void, // dpy
            i32,                   // format (fourcc)
            i32,                   // max
            *mut u64,              // modifiers
            *mut u32,              // external_only
            *mut i32,              // num
        ) -> u32;
        let f: QueryFn = unsafe { std::mem::transmute(f) };
        let mut n: i32 = 0;
        unsafe {
            // 先问数量（max=0）
            f(
                self._display.as_ptr(),
                fourcc as i32,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut n,
            );
            if n <= 0 {
                return Vec::new();
            }
            let mut mods = vec![0u64; n as usize];
            let mut ext = vec![0u32; n as usize];
            f(
                self._display.as_ptr(),
                fourcc as i32,
                n,
                mods.as_mut_ptr(),
                ext.as_mut_ptr(),
                &mut n,
            );
            mods.truncate(n.max(0) as usize);
            ext.truncate(n.max(0) as usize);
            mods.into_iter()
                .zip(ext)
                .map(|(m, e)| (m, e != 0))
                .collect()
        }
    }

    /// 把这一帧交给合成器（窗口模式下每帧渲染完调一次；其它模式空操作）
    pub fn swap(&self) -> Result<(), String> {
        self.ensure_current();
        match self.egl_surface {
            Some(s) => {
                // 慢圈归因：eglSwapBuffers 是“等合成器”的唯一悬念处。关着时零开销。
                crate::trace::mark(crate::trace::Mark::SwapBegin);
                crate::trace::bump_submit();
                let r = egl_swap_checked(&self._lib, self._display, s);
                crate::trace::mark(crate::trace::Mark::SwapEnd);
                crate::trace::bump_swap();
                r
            }
            None => Ok(()),
        }
    }
}

impl Drop for Gl {
    fn drop(&mut self) {
        let safe = match self.committer.as_mut() {
            Some(c) => c.stop_thread(),
            None => true,
        };
        if !safe {
            eprintln!(
                "[commit] commit thread still blocked in eglSwapBuffers => leaking the EGL                  surface/window on purpose (tearing them down would be a use-after-free)"
            );
            // leaking this object is not enough. The blocked commit thread is still
            // executing inside eglSwapBuffers **on the display we are about to drop**, so any
            // orderly teardown races it: measured with plain `return` (round 12) 1/6
            // `--scene --seconds 5` runs died with SIGSEGV (and once a `Too many bits for size_t`
            // abort) right after this line, in 4/4 runs where this branch fired but 0/8 where it
            // did not. `std::process::exit(0)` was not enough either (1/12 still segv): exit
            // runs atexit handlers, and mesa/LLVM use them to tear down state the running commit
            // thread is standing on. `_exit` is the only teardown-free way out, so flush what we
            // printed and hand the rest (fds, mappings, GPU objects) to the kernel.
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let _ = std::io::Write::flush(&mut std::io::stderr());
            unsafe { libc::_exit(0) };
        }
        unsafe {
            if let Some(s) = self.egl_surface {
                let _ = self._lib.make_current(self._display, None, None, None);
                let _ = self._lib.destroy_surface(self._display, s);
            }
            if let Some((w, destroy)) = self.wl_window {
                destroy(w); // wl_egl_window 是 libwayland-egl 分配的，必须还回去
            }
            let _ = self._lib.destroy_context(self._display, self._context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the hand-off between the main loop and `we-commit` must be ordered:
    /// a `Requested` slot is picked up exactly once and the waiter sees `Finished`.
    #[test]
    fn commit_slot_hands_one_request_to_the_worker() {
        let slot = Arc::new(CommitSlot {
            st: std::sync::Mutex::new(CommitState::Idle),
            cv: std::sync::Condvar::new(),
        });
        let s2 = slot.clone();
        let worker = std::thread::spawn(move || {
            assert!(wait_request(&s2), "worker should see the request");
            let mut st = s2.st.lock().unwrap();
            *st = CommitState::Finished(Ok(()), 12.5);
            s2.cv.notify_all();
        });
        {
            let mut st = slot.st.lock().unwrap();
            *st = CommitState::Requested;
        }
        slot.cv.notify_all();
        let st = slot.st.lock().unwrap();
        let (st, _to) = slot
            .cv
            .wait_timeout_while(st, std::time::Duration::from_secs(5), |s| {
                !matches!(s, CommitState::Finished(..))
            })
            .unwrap();
        match &*st {
            CommitState::Finished(Ok(()), ms) => assert!((*ms - 12.5).abs() < 1e-9),
            _ => panic!("waiter did not see the finished commit"),
        }
        drop(st);
        worker.join().unwrap();
    }

    /// The commit thread is inside eglSwapBuffers: teardown must refuse to return true,
    /// otherwise `Gl::drop` would free the EGL surface under the thread.
    #[test]
    fn stopping_a_busy_committer_is_refused() {
        let slot = Arc::new(CommitSlot {
            st: std::sync::Mutex::new(CommitState::Busy),
            cv: std::sync::Condvar::new(),
        });
        let mut c = Committer { slot, thread: None };
        assert!(!c.stop_thread(), "busy committer must not be torn down");
        *c.lock() = CommitState::Idle;
        assert!(c.stop_thread(), "idle committer can be stopped");
    }

    #[test]
    fn flip_vertical_swaps_rows() {
        // 2x2，每像素 1 字节（这里只看行的位置关系）
        let mut buf = vec![1u8, 1, 1, 1, 2, 2, 2, 2];
        flip_vertical(&mut buf, 1, 2);
        assert_eq!(buf, vec![2u8, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn f32_slice_reinterpretation_has_right_size() {
        let v = [1.0f32, 2.0, 3.0];
        assert_eq!(bytemuck_cast(&v).len(), 12);
    }

    #[test]
    fn card_quad_verts_unit_equals_the_old_literals_and_scales_by_half() {
        // `half=[1,1]` 必须与 `make_layer` 里那两个字面量数组**逐位相同**（否则改开关关时就不等价）
        #[rustfmt::skip]
        let old: [f32; 30] = [
            -1.0, -1.0, 0.0,  0.0, 1.0,
             1.0, -1.0, 0.0,  1.0, 1.0,
             1.0,  1.0, 0.0,  1.0, 0.0,
            -1.0, -1.0, 0.0,  0.0, 1.0,
             1.0,  1.0, 0.0,  1.0, 0.0,
            -1.0,  1.0, 0.0,  0.0, 0.0,
        ];
        #[rustfmt::skip]
        let old_flip: [f32; 30] = [
            -1.0, -1.0, 0.0,  0.0, 0.0,
             1.0, -1.0, 0.0,  1.0, 0.0,
             1.0,  1.0, 0.0,  1.0, 1.0,
            -1.0, -1.0, 0.0,  0.0, 0.0,
             1.0,  1.0, 0.0,  1.0, 1.0,
            -1.0,  1.0, 0.0,  0.0, 1.0,
        ];
        assert_eq!(card_quad_verts([1.0, 1.0], false), old);
        assert_eq!(card_quad_verts([1.0, 1.0], true), old_flip);
        // 卡片空间：位置 = 单位四边形 × half，uv 一个位不动
        let c = card_quad_verts([256.0, 512.0], false);
        for i in 0..6 {
            assert_eq!(c[i * 5], old[i * 5] * 256.0);
            assert_eq!(c[i * 5 + 1], old[i * 5 + 1] * 512.0);
            assert_eq!(c[i * 5 + 2], 0.0);
            assert_eq!(c[i * 5 + 3], old[i * 5 + 3]);
            assert_eq!(c[i * 5 + 4], old[i * 5 + 4]);
        }
    }

    #[test]
    fn flip_rows_swaps_top_and_bottom_and_keeps_width_intact() {
        // 2x2，行0=红，行1=蓝 ⇒ 翻完行0=蓝、行1=红
        let src = [
            255, 0, 0, 255, 255, 0, 0, 255, // 行0：两像素红
            0, 0, 255, 255, 0, 0, 255, 255, // 行1：两像素蓝
        ];
        let out = flip_rows(&src, 2, 2);
        assert_eq!(out.len(), src.len(), "字节数不能变");
        assert_eq!(&out[0..4], &[0, 0, 255, 255], "第一行应变成原来的最后一行");
        assert_eq!(&out[8..12], &[255, 0, 0, 255], "最后一行应变成原来的第一行");
    }

    #[test]
    fn flip_rows_is_identity_for_one_row_and_safe_on_short_input() {
        let one = [1u8, 2, 3, 4];
        assert_eq!(flip_rows(&one, 1, 1), one.to_vec());
        // 长度不足（不该发生）时不 panic，原样返回
        let short = [9u8, 9, 9];
        assert_eq!(flip_rows(&short, 2, 2), short.to_vec());
    }

    #[test]
    fn material_blend_mapping_matches_reference() {
        assert_eq!(Blend::of_material(Some("translucent")), Blend::Translucent);
        assert_eq!(Blend::of_material(Some("additive")), Blend::Additive);
        // WE 的 `normal` / `disabled` / 认不出来 / 没写 ⇒ 关混合（对齐 OWE ParseBlendMode）
        assert_eq!(Blend::of_material(Some("normal")), Blend::Opaque);
        assert_eq!(Blend::of_material(Some("disabled")), Blend::Opaque);
        assert_eq!(Blend::of_material(None), Blend::Opaque);
        // 对象的 colorBlendMode=31 在引擎侧就是"改成 additive"，压过材质
        assert_eq!(Blend::of_object(Some("translucent"), 31), Blend::Additive);
        assert_eq!(Blend::of_object(Some("translucent"), 0), Blend::Translucent);
    }

    /// 第 3 轮：`colorBlendMode` 1..30 的正确语义（OWE `ApplyLayerColorBlend` 只塞 combo、
    /// **不动 GL 混合状态**）。默认仍走旧行为（加法），忠实版走 `WE_CBM_MATERIAL=1`。
    #[test]
    fn object_color_blend_mode_1_to_30_keeps_material_blending_when_faithful() {
        for cbm in [1, 2, 3, 7, 9, 11, 13, 20, 30, 32, 33, -1] {
            assert_eq!(Blend::of_object(Some("translucent"), cbm), Blend::Additive,
                       "旧行为：cbm={cbm} 按加法兜着");
            assert_eq!(Blend::of_object_faithful(Some("translucent"), cbm), Blend::Translucent,
                       "忠实：cbm={cbm} 保持材质自己的 blending");
            assert_eq!(Blend::of_object_faithful(Some("additive"), cbm), Blend::Additive);
            assert_eq!(Blend::of_object_faithful(Some("normal"), cbm), Blend::Opaque);
        }
        // 31 / 0 两条路都必须一致（31 恒加法、0 看材质）
        for cbm in [0, 31] {
            assert_eq!(Blend::of_object(Some("translucent"), cbm),
                       Blend::of_object_faithful(Some("translucent"), cbm));
        }
    }

    /// K1：`plan_of_object` —— 1..30 走 **shader 内 `ApplyBlending`**（塞 combo、GL 混合 = 材质），
    /// 0 / 31 不塞 combo（31 恒加法、0 看材质），开关关着时与旧行为**逐值一致**。
    #[test]
    fn cbm_plan_matches_official_semantics_and_is_inert_when_off() {
        // 开关关 ⇒ 与旧路径逐值相同（0/31 与 1..30/>31 三档都核）
        for material in [None, Some("translucent"), Some("additive"), Some("normal")] {
            for cbm in [0i32, 1, 5, 10, 30, 31, 32, 47, -1] {
                let (b, combo) = Blend::plan_of_object(material, cbm, false);
                assert_eq!(combo, None, "开关关着不许塞 combo（cbm={cbm}）");
                assert_eq!(b, Blend::of_object(material, cbm), "开关关着必须逐值一致（cbm={cbm}）");
            }
        }
        // 开关开 ⇒ 1..（除 31 外）塞 combo 且 GL 混合 = 材质自己的 blending
        for cbm in [1i32, 2, 5, 9, 10, 11, 30, 32] {
            let (b, combo) = Blend::plan_of_object(Some("translucent"), cbm, true);
            assert_eq!(combo, Some(cbm), "cbm={cbm} 应把 BLENDMODE={cbm} 塞给 shader");
            assert_eq!(b, Blend::Translucent, "cbm={cbm} 不动 GL 混合（= 材质 blending）");
        }
        // 特例：0 ⇒ 不塞、看材质；31 ⇒ 不塞、恒加法（两种开关状态都一样）
        for on in [false, true] {
            assert_eq!(Blend::plan_of_object(Some("translucent"), 0, on), (Blend::Translucent, None));
            assert_eq!(Blend::plan_of_object(Some("translucent"), 31, on), (Blend::Additive, None));
            // 非正数（0 已判过，-1 落这里）不塞（LWE 的 `> 0` 门）
            assert_eq!(Blend::plan_of_object(Some("translucent"), -1, on), (Blend::Additive, None));
        }
    }
}
