//! 整帧 GPU 计时 + 两个仅供测量的清屏开关（`WE_PROFILE_TOTAL`）。
//!
//! 为什么要有它：`WE_PROFILE_PASSES` 只覆盖趟链，而“屏上 GPU 86% 里还有约 9.7 ms 不在趟链里”
//! 这件事此前只有总量、没有拆分。GL 不允许 timer query 嵌套 ⇒ 整帧与逐趟必须分两次跑。
//! 本模块只做三件事：整帧一条 `GL_TIME_ELAPSED`、把结果交给 `passtime` 的同一张报表、
//! 两个把清屏单独减出来的测量开关。默认全关（一个查询都不建，行为逐字节不变）。

use std::time::Instant;

pub fn total_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WE_PROFILE_TOTAL").is_some())
}

/// `WE_SKIP_RT_CLEAR`：跳过帧首“每张 RT 清透明”的循环。输出是错的（上一帧残影），仅供测量。
pub fn skip_rt_clear() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::env_flag("WE_SKIP_RT_CLEAR", false))
}

/// `WE_SKIP_MASK_CLEAR`：跳过遮罩烘焙趟之前的清屏。同上，仅供测量。
pub fn skip_mask_clear() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::env_flag("WE_SKIP_MASK_CLEAR", false))
}

/// 帧首：整帧计时开始（`WE_PROFILE_TOTAL` 关着时返回 `None`，一条 GL 调用都不发）。
pub fn frame_begin(gl: &crate::gl_ctx::Gl) -> Option<Instant> {
    if !total_on() {
        return None;
    }
    static META: std::sync::Once = std::sync::Once::new();
    META.call_once(|| {
        crate::passtime::prof_meta_init(vec!["整帧：清屏+趟链+粒子尾+present".to_string()])
    });
    gl.pt_frame_begin();
    gl.pt_begin(0);
    Some(Instant::now())
}

/// 帧尾：结束整帧计时并交给 `passtime` 报表（每 30 个采样帧打一行）。
pub fn frame_end(gl: &crate::gl_ctx::Gl, t0: Instant) {
    gl.pt_end();
    let samples = gl.pt_collect();
    if samples.is_empty() {
        return;
    }
    crate::passtime::prof_frame(&samples, 0, t0.elapsed().as_nanos() as u64);
    crate::passtime::prof_tick();
}
