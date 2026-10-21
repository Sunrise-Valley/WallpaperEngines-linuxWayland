// SPDX-License-Identifier: MIT
//! 视频壁纸的 **0 拷贝** 路：进程内 VAAPI 解码（`c/av_decode.c`）→ dmabuf →
//! `EGL_EXT_image_dma_buf_import` 当贴图 → 画进 `wl_egl_window`（`eglSwapBuffers`）。
//! 对比老路（ffmpeg 子进程 + `hwdownload` → 管道 → CPU 写 shm）：
//! 老路每帧要把整屏 2560x1440x4 ≈ 14.7MB 从 GPU 读到 CPU、再经管道搬一遍；
//! 新路 CPU 只发命令，像素全程在显存里（和 scene 的 0 拷贝是一个道理）。
//! 解码这半边在 C 里（见 `build.rs` 的说明）；这里只做两件事：
//!
//! 1. 把 C 的 `wv_frame` 包成 Rust 结构（`DmaVideo`）
//!
//! 2. NV12 → RGB 的着色器 + 全屏四边形 + cover 裁切（`VideoGl`）

use std::ffi::{c_char, c_void, CString};
use std::path::Path;

use glow::HasContext;

use crate::gl_ctx::{DmaBufImage, DmaBufPlane, Gl};

// ── C shim（c/av_decode.h）───────────────────────────────────────────────────

/// 一帧 dmabuf 的描述（字段顺序/类型必须和 `c/av_decode.h` 的 `wv_frame` 一致）
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WvFrame {
    pub width: i32,
    pub height: i32,
    pub nb_planes: i32,
    pub fd: [i32; 2],
    pub fourcc: [u32; 2],
    pub offset: [u32; 2],
    pub pitch: [u32; 2],
    pub modifier: [u64; 2],
    pub colorspace: i32,
    pub color_range: i32,
}

extern "C" {
    fn wv_open(path: *const c_char, err: *mut c_char, errlen: i32) -> *mut c_void;
    fn wv_close(d: *mut c_void);
    fn wv_next(d: *mut c_void, out: *mut WvFrame, err: *mut c_char, errlen: i32) -> i32;
    fn wv_release(d: *mut c_void);
    fn wv_fps(d: *mut c_void) -> f64;
    fn wv_hw(d: *mut c_void) -> i32;
    fn wv_hw_err(d: *mut c_void) -> *const c_char;
}

/// AVCOL_SPC_BT709（其余按 BT.601 处理）
const AVCOL_SPC_BT709: i32 = 1;
/// AVCOL_RANGE_JPEG = 全范围
const AVCOL_RANGE_JPEG: i32 = 2;

const DRM_FORMAT_R8: u32 = 0x2020_3852; // 'R','8',0,0 → 平面0 的 NV12 Y
const DRM_FORMAT_GR88: u32 = 0x3838_5247; // 'G','R','8','8'

pub struct DmaVideo {
    dec: *mut c_void,
    pub fps: f64,
    pub frames: u64,
    last: WvFrame,
}

// 解码器只在单线程里用（内核的渲染线程）；指针不跨线程共享
unsafe impl Send for DmaVideo {}

impl DmaVideo {
    pub fn open(path: &Path) -> Result<DmaVideo, String> {
        let cpath = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| "路径里有 NUL".to_string())?;
        let mut err = [0i8; 256];
        let dec = unsafe { wv_open(cpath.as_ptr(), err.as_mut_ptr(), err.len() as i32) };
        if dec.is_null() {
            return Err(cstr(&err));
        }
        let fps = unsafe { wv_fps(dec) };
        Ok(DmaVideo {
            dec,
            fps,
            frames: 0,
            last: WvFrame::default(),
        })
    }

    /// 现在拿到的是 VAAPI 硬解帧吗（**每帧之后才知道** —— 起解码器时还没有帧）
    pub fn hw(&self) -> bool {
        unsafe { wv_hw(self.dec) != 0 }
    }

    /// 没走硬解时的原因（VAAPI 设备/帧池建不起来时 C 侧记下来的话）
    pub fn hw_err(&self) -> String {
        unsafe {
            let p = wv_hw_err(self.dec);
            if p.is_null() {
                return String::new();
            }
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    /// 解下一帧（内部已释放上一帧）。返回 None = 这轮没解出（调用方下个周期再试）
    pub fn next(&mut self) -> Option<&WvFrame> {
        let mut out = WvFrame::default();
        let mut err = [0i8; 256];
        let r = unsafe { wv_next(self.dec, &mut out, err.as_mut_ptr(), err.len() as i32) };
        if r <= 0 {
            if r < 0 {
                eprintln!("[视频·0拷贝] 解码出错：{}", cstr(&err));
            }
            return None;
        }
        self.last = out;
        self.frames += 1;
        Some(&self.last)
    }

    /// 画完 / swap 之后再调：解映射，让解码器能复用这张 surface
    pub fn release(&mut self) {
        unsafe { wv_release(self.dec) }
    }

    /// 解码尺寸（源；内核状态回报用）——还没解出帧时是 (0,0)
    pub fn source_size(&self) -> (u32, u32) {
        if self.last.width > 0 && self.last.height > 0 {
            (self.last.width as u32, self.last.height as u32)
        } else {
            (0, 0)
        }
    }
}

impl Drop for DmaVideo {
    fn drop(&mut self) {
        unsafe { wv_close(self.dec) }
    }
}

fn cstr(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ── NV12 → RGB 的 GL 渲染器 ──────────────────────────────────────────────────

const VS: &str = r#"#version 120
// 全屏四边形：位置直接是 NDC（(-1,-1) 到 (1,1)），UV 从位置推出来
// g_UV = (u0, v0, du, dv)：source 里 cover 裁切那一块
// g_FLIP = 1.0 时把 V 翻过来（dmabuf 导入的贴图 v=0 是内存第一行=画面顶行）
attribute vec2 a_Pos;
varying vec2 v_UV;
uniform vec4 g_UV;
uniform float g_FLIP;
void main() {
    vec2 t = (a_Pos + 1.0) * 0.5;
    t.y = mix(t.y, 1.0 - t.y, g_FLIP);
    v_UV = g_UV.xy + t * g_UV.zw;
    gl_Position = vec4(a_Pos, 0.0, 1.0);
}
"#;

const FS: &str = r#"#version 120
// NV12 → RGB：Y 在 unit0（ 单通道），UV 在 unit1（GR88，.r=U .g=V）
varying vec2 v_UV;
uniform sampler2D g_TexY;
uniform sampler2D g_TexUV;
uniform vec3 g_RowR;
uniform vec3 g_RowG;
uniform vec3 g_RowB;
uniform float g_YScale;
uniform float g_YBias;
uniform float g_CScale;
void main() {
    float y = texture2D(g_TexY, v_UV).r * g_YScale + g_YBias;
    vec2 c = (texture2D(g_TexUV, v_UV).rg - 0.5) * g_CScale;
    vec3 yuv = vec3(y, c.x, c.y);   // (y, u, v)
    vec3 rgb = vec3(dot(g_RowR, yuv), dot(g_RowG, yuv), dot(g_RowB, yuv));
    gl_FragColor = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

/// 已经导入的平面缓存（解码器常复用同一张 surface，能省掉每帧的 eglCreateImage）
struct PlaneSlot {
    plane: DmaBufPlane,
    fourcc: u32,
    w: u32,
    h: u32,
    tex: glow::Texture,
    image: DmaBufImage,
}

pub struct VideoGl {
    gl: Gl,
    prog: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    planes: Vec<PlaneSlot>,
    /// 目标尺寸（渲染缓冲 = 物理尺寸）
    pub w: u32,
    pub h: u32,
    frames: u64,
    /// dmabuf 贴图要不要翻 V（拿实机截图定的）
    flip_v: bool,
    /// 「驱动支持哪些 modifier」这句话只打一次
    reported_modifiers: bool,
}

impl VideoGl {
    /// 在给定的 `wl_surface` 上开 0 拷贝窗口（EGL wayland 平台）
///
    /// # Safety
    /// 两个指针必须来自同一个活着的 Wayland 连接，且 surface 还没销毁。
    pub unsafe fn new_window(
        w: u32,
        h: u32,
        display_ptr: *mut c_void,
        surface_ptr: *mut c_void,
    ) -> Result<VideoGl, String> {
        let gl = Gl::new_window(w, h, display_ptr, surface_ptr, None)?;
        Self::with_gl(gl, w, h)
    }

    fn with_gl(gl: Gl, w: u32, h: u32) -> Result<VideoGl, String> {
        unsafe {
            let glc = &gl.gl;
            let vs = glc.create_shader(glow::VERTEX_SHADER).map_err(|e| e.to_string())?;
            glc.shader_source(vs, VS);
            glc.compile_shader(vs);
            if !glc.get_shader_compile_status(vs) {
                return Err(format!("视频顶点着色器编译失败：{}", glc.get_shader_info_log(vs)));
            }
            let fs = glc.create_shader(glow::FRAGMENT_SHADER).map_err(|e| e.to_string())?;
            glc.shader_source(fs, FS);
            glc.compile_shader(fs);
            if !glc.get_shader_compile_status(fs) {
                return Err(format!("视频片元着色器编译失败：{}", glc.get_shader_info_log(fs)));
            }
            let prog = glc.create_program().map_err(|e| e.to_string())?;
            glc.attach_shader(prog, vs);
            glc.attach_shader(prog, fs);
            glc.link_program(prog);
            if !glc.get_program_link_status(prog) {
                return Err(format!("视频着色器链接失败：{}", glc.get_program_info_log(prog)));
            }
            glc.detach_shader(prog, vs);
            glc.detach_shader(prog, fs);
            glc.delete_shader(vs);
            glc.delete_shader(fs);
            glc.use_program(Some(prog));

            // 两个三角形拼一个全屏四边形（位置在 NDC，UV 在着色器里推）
            let vao = glc.create_vertex_array().map_err(|e| e.to_string())?;
            let vbo = glc.create_buffer().map_err(|e| e.to_string())?;
            let verts: [f32; 12] = [
                -1.0, -1.0, 1.0, -1.0, 1.0, 1.0,
                -1.0, -1.0, 1.0, 1.0, -1.0, 1.0,
            ];
            glc.bind_vertex_array(Some(vao));
            glc.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let bytes = std::slice::from_raw_parts(
                verts.as_ptr() as *const u8,
                std::mem::size_of_val(&verts),
            );
            glc.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
            let loc = glc
                .get_attrib_location(prog, "a_Pos")
                .ok_or("找不到 a_Pos 属性")?;
            glc.enable_vertex_attrib_array(loc);
            glc.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 8, 0);
            glc.bind_vertex_array(None);
            Ok(VideoGl {
                gl,
                prog,
                vao,
                vbo,
                planes: Vec::with_capacity(2),
                w,
                h,
                frames: 0,
                flip_v: true,
                reported_modifiers: false,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// 换哪个面翻 V（测试用；实机截图定下来后就是默认值）
    pub fn set_flip(&mut self, flip: bool) {
        self.flip_v = flip;
    }

    /// 导入（必要时）+ 画一帧到窗口默认帧缓冲。**不含** swap。
    pub fn draw(&mut self, f: &WvFrame) -> Result<(), String> {
        if f.nb_planes < 2 || f.width <= 0 || f.height <= 0 {
            return Err(format!("帧描述不完整：planes={} {}x{}", f.nb_planes, f.width, f.height));
        }
        // a late eglSwapBuffers runs on the commit thread with the EGL context
        // bound there, so take it back **before the import loop below**: the dmabuf re-import
        // (glGenTextures / eglCreateImage) and the release of the old plane are GL calls too,
        // and without a current context they fail with "Unable to create Texture object".
        self.gl.ensure_current();
        let (vw, vh) = (f.width as u32, f.height as u32);
        // NV12 的两个平面：Y 用 （w x h），UV 用 GR88（w/2 x h/2，一"像素"两字节）
        let want: [(u32, u32, u32); 2] = [
            (DRM_FORMAT_R8, vw, vh),
            (DRM_FORMAT_GR88, (vw + 1) / 2, (vh + 1) / 2),
        ];
        // 平面换了（解码器换 surface）才重新导入
        for i in 0..2 {
            let p = DmaBufPlane {
                fd: f.fd[i],
                offset: f.offset[i],
                pitch: f.pitch[i],
                modifier: f.modifier[i],
            };
            let (fourcc, pw, ph) = want[i];
            let same = self.planes.get(i).is_some_and(|s| {
                s.plane == p && s.fourcc == fourcc && s.w == pw && s.h == ph
            });
            if same {
                continue;
            }
            if let Some(old) = self.planes.get(i) {
                self.gl.release_dma_buf(old.tex, old.image);
            }
            let (tex, image) = match self.gl.import_dma_buf(fourcc, pw, ph, &p) {
                Ok(v) => v,
                Err(e) => {
                    // 导入失败时把「驱动认哪些 modifier」打出来（一次就够，别刷屏）
                    if !self.reported_modifiers {
                        self.reported_modifiers = true;
                        let mods = self.gl.query_dmabuf_modifiers(fourcc);
                        eprintln!(
                            "[视频·0拷贝] 驱动对 fourcc 0x{fourcc:x} 声明支持 {} 个 modifier：{}",
                            mods.len(),
                            mods.iter()
                                .map(|(m, ext)| format!("0x{m:x}{}", if *ext { "(external-only)" } else { "" }))
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                    }
                    return Err(e);
                }
            };
            let slot = PlaneSlot {
                plane: p,
                fourcc,
                w: pw,
                h: ph,
                tex,
                image,
            };
            if i < self.planes.len() {
                self.planes[i] = slot;
            } else {
                self.planes.push(slot);
            }
        }

        // cover 裁切：源里取一块和窗口同比例的区域（不拉伸）；UV 折算成 0..1
        let (sx, sy, sw, sh) = crate::cover_rect(vw as f32, vh as f32, self.w as f32, self.h as f32);
        let uv = [
            sx / vw as f32,
            sy / vh as f32,
            sw / vw as f32,
            sh / vh as f32,
        ];
        // 颜色矩阵：按流里的 colorspace / range 走（BT.709 / BT.601，有限 / 全范围）
        let bt709 = f.colorspace == AVCOL_SPC_BT709;
        let (kr, kb) = if bt709 { (0.2126f32, 0.0722f32) } else { (0.299f32, 0.114f32) };
        let kg = 1.0 - kr - kb;
        let (y_scale, y_bias, c_scale) = if f.color_range == AVCOL_RANGE_JPEG {
            (1.0f32, 0.0f32, 1.0f32)
        } else {
            (255.0 / 219.0, -16.0 / 255.0, 255.0 / 224.0)
        };
        let row_r = [1.0f32, 0.0, 2.0 * (1.0 - kr)];
        let row_g = [1.0f32, -2.0 * kb * (1.0 - kb) / kg, -2.0 * kr * (1.0 - kr) / kg];
        let row_b = [1.0f32, 2.0 * (1.0 - kb), 0.0];

        unsafe {
            let gl = &self.gl.gl;
            self.gl.bind_target(None); // 窗口模式 ⇒ FBO 0，viewport = (0,0,w,h)
            gl.use_program(Some(self.prog));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.planes[0].tex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.planes[1].tex));
            let set1 = |name: &str, v: f32| {
                if let Some(l) = gl.get_uniform_location(self.prog, name) {
                    gl.uniform_1_f32(Some(&l), v);
                }
            };
            let set3 = |name: &str, v: [f32; 3]| {
                if let Some(l) = gl.get_uniform_location(self.prog, name) {
                    gl.uniform_3_f32(Some(&l), v[0], v[1], v[2]);
                }
            };
            if let Some(l) = gl.get_uniform_location(self.prog, "g_UV") {
                gl.uniform_4_f32(Some(&l), uv[0], uv[1], uv[2], uv[3]);
            }
            set1("g_FLIP", if self.flip_v { 1.0 } else { 0.0 });
            if let Some(l) = gl.get_uniform_location(self.prog, "g_TexY") {
                gl.uniform_1_i32(Some(&l), 0);
            }
            if let Some(l) = gl.get_uniform_location(self.prog, "g_TexUV") {
                gl.uniform_1_i32(Some(&l), 1);
            }
            set3("g_RowR", row_r);
            set3("g_RowG", row_g);
            set3("g_RowB", row_b);
            set1("g_YScale", y_scale);
            set1("g_YBias", y_bias);
            set1("g_CScale", c_scale);
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            let e = gl.get_error();
            if e != glow::NO_ERROR {
                return Err(format!("视频绘制后 GL 报 0x{e:x}"));
            }
            gl.bind_vertex_array(None);
        }
        self.frames += 1;
        Ok(())
    }

    /// 交给合成器（每帧一次）
    pub fn swap(&self) -> Result<(), String> {
        self.gl.swap()
    }

    /// Bounded commit : same as `swap` but the `eglSwapBuffers` call runs on the
    /// `we-commit` thread and this thread waits at most `deadline_ms`. Without it a video
    /// wallpaper blocks the whole kernel main loop whenever the compositor stops answering
    /// (23/23 recorded stalls were output reconfigurations, all inside eglSwapBuffers).
    pub fn swap_bounded(&mut self, deadline_ms: u64) -> crate::gl_ctx::CommitOutcome {
        self.gl.swap_bounded(deadline_ms)
    }

    /// Is a commit from an earlier frame still in flight?
    pub fn commit_pending(&self) -> bool {
        self.gl.commit_pending()
    }

    /// Collect a finished commit (None = still running). Closes the SwapBegin/SwapEnd span.
    pub fn poll_commit(&self) -> Option<Result<(), String>> {
        self.gl.poll_commit()
    }

    /// Wall time of the last collected commit in ms (0 = none yet).
    pub fn last_swap_ms(&self) -> u64 {
        self.gl.last_swap_ms()
    }

    pub fn is_window(&self) -> bool {
        self.gl.is_window()
    }
}

impl Drop for VideoGl {
    fn drop(&mut self) {
        // Same reason: texture/image teardown needs the context on this thread.
        self.gl.ensure_current();
        unsafe {
            for s in self.planes.drain(..) {
                self.gl.release_dma_buf(s.tex, s.image);
            }
            let gl = &self.gl.gl;
            gl.delete_program(self.prog);
            gl.delete_vertex_array(self.vao);
            gl.delete_buffer(self.vbo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wv_frame_layout_matches_the_c_header() {
        // 和 c/av_decode.h 的 wv_frame 对齐：C 侧 gcc -offsetof 实测
        // size=72 width=0 nb_planes=8 fd=12 fourcc=20 offset=28 pitch=36 modifier=48 colorspace=64
        assert_eq!(std::mem::size_of::<WvFrame>(), 72);
        assert_eq!(std::mem::offset_of!(WvFrame, width), 0);
        assert_eq!(std::mem::offset_of!(WvFrame, nb_planes), 8);
        assert_eq!(std::mem::offset_of!(WvFrame, fd), 12);
        assert_eq!(std::mem::offset_of!(WvFrame, fourcc), 20);
        assert_eq!(std::mem::offset_of!(WvFrame, offset), 28);
        assert_eq!(std::mem::offset_of!(WvFrame, pitch), 36);
        assert_eq!(std::mem::offset_of!(WvFrame, modifier), 48);
        assert_eq!(std::mem::offset_of!(WvFrame, colorspace), 64);
        assert_eq!(std::mem::offset_of!(WvFrame, color_range), 68);
    }

    #[test]
    fn constants_match_drm_and_ffmpeg() {
        // drm_fourcc.h：DRM_FORMAT_R8 = fourcc_code('R','8',' ',' ')、GR88 = ('G','R','8','8')
        assert_eq!(DRM_FORMAT_R8, u32::from_le_bytes(*b"R8  "));
        assert_eq!(DRM_FORMAT_GR88, u32::from_le_bytes(*b"GR88"));
        assert_eq!(AVCOL_SPC_BT709, 1);
        assert_eq!(AVCOL_RANGE_JPEG, 2);
    }
}
