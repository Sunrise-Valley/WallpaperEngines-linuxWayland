//! 探针：本机 EGL/驱动**能不能把 GL 渲染结果导出成 dmabuf**（0 拷贝方案的地基）。
//!
//! 跑法（需要 LD_LIBRARY_PATH 指向无 root 提出来的加载器）：
//! ```text
//! export LD_LIBRARY_PATH="$HOME/.local/opt/gl/root/usr/lib/x86_64-linux-gnu"
//! cargo run --offline --bin egl-dmabuf-probe
//! ```
//!
//! 它做的事：EGL 初始化（surfaceless）→ 建一张 GL 贴图 + FBO → 渲个纯色 →
//! `eglCreateImage(EGL_GL_TEXTURE_2D)` → `eglExportDMABUFImageQueryMESA/MESA`
//! → 打印 fourcc / modifier / stride / plane 数 / fd 是否有效。
//!
//! 为什么先做这个而不是直接改渲染器：**导出路径能不能通完全取决于驱动**，
//! 通不了（只有 import 没有 export）就得换方案（GBM 自己分配 buffer），
//! 那种返工要在动渲染器之前就知道。

use std::ffi::{c_void, CString};

type EglDisplay = *mut c_void;
type EglImage = *mut c_void;

fn fourcc_str(f: u32) -> String {
    let b = f.to_le_bytes();
    let s: String = b
        .iter()
        .map(|c| {
            if c.is_ascii_graphic() {
                *c as char
            } else {
                '.'
            }
        })
        .collect();
    format!("0x{f:08x} '{s}'")
}

fn main() {
    let lib = unsafe {
        khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required_from_filename(
            "libEGL.so.1",
        )
    }
    .expect("加载 libEGL.so.1 失败（LD_LIBRARY_PATH 设了吗）");

    // ── display：必须**显式绑定 DRM 设备** ──────────────────────────────────
    // `eglGetDisplay(EGL_DEFAULT_DISPLAY)` 在 Mesa 上给的是 surfaceless 客户端 display，
    // 它**没有 DRM 设备**：GL 能画（走软件/设备猜测），但一旦走 EGLImage
    // （GL 贴图导出 / dmabuf 导入）就会空指针崩溃 —— 实测段错误 ✗。
    // 正解：`eglGetPlatformDisplayEXT(EGL_PLATFORM_DEVICE_EXT, render_node_fd, NULL)`。
    let p_get_platform_display: Option<unsafe extern "C" fn(u32, *mut c_void, *const i32) -> *mut c_void> =
        lib.get_proc_address("eglGetPlatformDisplayEXT").map(|f| unsafe { std::mem::transmute(f) });

    const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
    let render_fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
        .map(|f| {
            use std::os::fd::IntoRawFd;
            f.into_raw_fd()
        });
    let display = match (p_get_platform_display, &render_fd) {
        (Some(get_pd), Ok(fd)) => {
            let dpy = unsafe { get_pd(EGL_PLATFORM_DEVICE_EXT, *fd as *mut c_void, std::ptr::null()) };
            if dpy.is_null() {
                println!("/dev/dri/renderD128 上的 eglGetPlatformDisplayEXT 失败，回退 DEFAULT_DISPLAY");
                unsafe { lib.get_display(khronos_egl::DEFAULT_DISPLAY) }.expect("eglGetDisplay")
            } else {
                println!("display = EGL_PLATFORM_DEVICE_EXT(/dev/dri/renderD128) ✓");
                unsafe { khronos_egl::Display::from_ptr(dpy) }
            }
        }
        _ => {
            println!("拿不到 render node 或没有 eglGetPlatformDisplayEXT，回退 DEFAULT_DISPLAY");
            unsafe { lib.get_display(khronos_egl::DEFAULT_DISPLAY) }.expect("eglGetDisplay")
        }
    };
    let (maj, min) = lib.initialize(display).expect("eglInitialize");
    let dpy = display.as_ptr();

    println!("EGL {maj}.{min}");
    let qs = |name: khronos_egl::Int| -> String {
        lib.query_string(Some(display), name)
            .ok()
            .and_then(|c| c.to_str().ok())
            .unwrap_or("?")
            .to_string()
    };
    // **客户端**扩展要用 EGL_NO_DISPLAY 查（platform_* 系列只在这里出现）
    let client_exts = lib
        .query_string(None, khronos_egl::EXTENSIONS)
        .ok()
        .and_then(|c| c.to_str().ok())
        .unwrap_or("")
        .to_string();
    println!("CLIENT_EXTENSIONS = {client_exts}");
    println!("VENDOR   = {}", qs(khronos_egl::VENDOR));
    println!("CLIENT_APIS = {}", qs(khronos_egl::CLIENT_APIS));
    let exts = qs(khronos_egl::EXTENSIONS);
    println!("\n── 关键扩展 ──");
    for k in [
        "EGL_MESA_image_dma_buf_export",
        "EGL_EXT_image_dma_buf_export",
        "EGL_EXT_image_dma_buf_import",
        "EGL_EXT_image_dma_buf_import_modifiers",
        "EGL_KHR_gl_texture_2D_image",
        "EGL_KHR_fence_sync",
        "EGL_ANDROID_native_fence_sync",
        "EGL_MESA_drm_image",
        "EGL_KHR_surfaceless_context",
        "EGL_EXT_platform_device",
        "EGL_MESA_platform_surfaceless",
        "EGL_EXT_platform_base",
    ] {
        println!("  {:<42} {}", k, if exts.contains(k) { "✓" } else { "✗" });
    }

    // ── 取函数指针 ────────────────────────────────────────────────────────
    // 只保留导出用的两个（create_image 走 khronos-egl 的类型安全包装）
    let sym = |name: &str| -> *const c_void {
        lib.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const c_void)
    };
    let p_export_query = sym("eglExportDMABUFImageQueryMESA");
    let p_export = sym("eglExportDMABUFImageMESA");
    println!("\n── 函数指针 ──");
    println!("  eglExportDMABUFImageQueryMESA     {}", if p_export_query.is_null() { "✗" } else { "✓" });
    println!("  eglExportDMABUFImageMESA          {}", if p_export.is_null() { "✗" } else { "✓" });
    if p_export.is_null() || p_export_query.is_null() {
        println!("\n结论：这台机器**没有导出 dmabuf 的路** ⇒ 得改方案（用 GBM 自己分配 buffer 再 import 成 EGLImage）");
        std::process::exit(2);
    }

    type ExportQueryFn =
        unsafe extern "C" fn(EglDisplay, EglImage, *mut i32, *mut i32, *mut i64) -> u32;
    type ExportFn = unsafe extern "C" fn(EglDisplay, EglImage, *mut i32, *mut i32, *mut i32) -> u32;
    let export_query: ExportQueryFn = unsafe { std::mem::transmute(p_export_query) };
    let export: ExportFn = unsafe { std::mem::transmute(p_export) };

    // ── 建 GL 上下文并渲一帧 ─────────────────────────────────────────────
    lib.bind_api(khronos_egl::OPENGL_API).expect("bind_api");
    let config = lib
        .choose_first_config(display, &[khronos_egl::NONE])
        .expect("choose_config")
        .expect("无 config");
    let ctx = lib
        .create_context(display, config, None, &[khronos_egl::NONE])
        .expect("create_context");
    lib.make_current(display, None, None, Some(ctx)).expect("make_current");
    let gl = unsafe {
        glow::Context::from_loader_function(|s| {
            let c = CString::new(s).unwrap();
            lib.get_proc_address(c.to_str().unwrap())
                .map_or(std::ptr::null(), |f| f as *const c_void)
        })
    };

    let (w, h) = (640u32, 360u32);
    unsafe {
        use glow::HasContext as _;
        // 注意：FBO 的颜色附件必须是**贴图**（不是 renderbuffer）—— 导出要贴图
        let tex = gl.create_texture().expect("texture");
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
        // 贴图必须"完整"，否则 EGLImage 创建可能出问题（mipmap 级别不全）
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_BASE_LEVEL, 0);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAX_LEVEL, 0);

        let fbo = gl.create_framebuffer().expect("fbo");
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        let st = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        println!(
            "\nFBO {w}x{h} 状态 = 0x{st:x}（0x8cd5 = 完整）"
        );
        // 渲一个好认的图案（左半红、右半蓝）
        gl.viewport(0, 0, w as i32, h as i32);
        gl.clear_color(1.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.enable(glow::SCISSOR_TEST);
        gl.scissor((w / 2) as i32, 0, (w / 2) as i32, h as i32);
        gl.clear_color(0.0, 0.0, 1.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.disable(glow::SCISSOR_TEST);
        gl.finish();

        // ── 导出 ────────────────────────────────────────────────────────
        // 用 khronos-egl 的类型安全包装（自己 transmute 函数指针的版本实测**段错误** ✗，
        // 而且段错误看不到 EGL 错误码；走包装至少能拿到 BadParameter 之类的真实原因）
        let buffer = khronos_egl::ClientBuffer::from_ptr(tex.0.get() as *mut c_void);
        // 试最小属性列表（空表 = 只用 NONE 收尾）。EGL_IMAGE_PRESERVED 是"导入"侧的属性，
        // 传给 GL_TEXTURE_2D 导出有些驱动会崩（Mesa 实测段错误 ✗）。
        let use_attribs = std::env::var("PROBE_ATTRIBS").is_ok();
        let attribs: Vec<usize> = if use_attribs {
            vec![khronos_egl::IMAGE_PRESERVED as usize, 1, khronos_egl::NONE as usize]
        } else {
            vec![khronos_egl::NONE as usize]
        };
        eprintln!(
            "[探针] 准备 eglCreateImage: tex={} target=0x{:x} attribs={:?}",
            tex.0.get(),
            khronos_egl::GL_TEXTURE_2D,
            attribs
        );
        let image = match lib.create_image(
            display,
            khronos_egl::Context::from_ptr(khronos_egl::NO_CONTEXT),
            khronos_egl::GL_TEXTURE_2D as u32,
            buffer,
            &attribs,
        ) {
            Ok(i) => {
                eprintln!("[探针] eglCreateImage 成功");
                i
            }
            Err(e) => {
                println!("\neglCreateImage 失败：{e:?}（EGL error = {:?}）", lib.get_error());
                std::process::exit(3);
            }
        };
        let image_ptr = image.as_ptr();
        let (mut fourcc, mut nplanes, mut modifier) = (0i32, 0i32, 0i64);
        let ok = export_query(dpy, image_ptr, &mut fourcc, &mut nplanes, &mut modifier);
        println!(
            "\n── 导出查询（返回 {ok}）──\n  fourcc   = {}\n  planes   = {nplanes}\n  modifier = 0x{modifier:x}",
            fourcc_str(fourcc as u32)
        );
        let (mut fd, mut stride, mut offset) = (0i32, 0i32, 0i32);
        let ok2 = export(dpy, image_ptr, &mut fd, &mut stride, &mut offset);
        println!(
            "── 导出（返回 {ok2}）──\n  fd     = {fd}\n  stride = {stride}\n  offset = {offset}"
        );
        let fd_ok = fd >= 0 && std::fs::metadata(format!("/proc/self/fd/{fd}")).is_ok();
        println!(
            "  fd 有效 = {}",
            if fd_ok { "✓（/proc/self/fd 里在）" } else { "✗" }
        );
        if fd_ok {
            println!(
                "\n结论：**导出 dmabuf 的路通的** ⇒ 可以做 0 拷贝（GL 渲染 → dmabuf → zwp_linux_dmabuf_v1 给合成器）"
            );
        } else {
            println!("\n结论：导出的 fd 无效 ✗ —— 需要换 GBM 路线");
            std::process::exit(4);
        }
    }
}
