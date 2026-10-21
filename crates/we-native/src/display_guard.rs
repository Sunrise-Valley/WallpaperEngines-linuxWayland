//! Clean shutdown when the display connection (compositor / EGL) dies .
//! Why: khronos-egl 6.0.0 maps every EGL failure to
//! `Err(self.get_error.unwrap)` (crate src/lib.rs:1065 / 1173). When the
//! compositor dies, Mesa's eglSwapBuffers returns FALSE while eglGetError still
//! reports success, so get_error is None and unwrap panics. The release
//! profile is panic = "abort", so that panic becomes SIGABRT(6) + a core dump
//! and the journal only shows a bare unwrap instead of "the compositor died".
//! Two things happen here, neither touches rendered pixels:
//!
//! 1. install_panic_hook: a panic located in khronos-egl is classified as a
//! lost display, printed as one line and turned into exit(75). Any other
//! panic keeps the default hook (message + abort) so real bugs stay visible.
//!
//! 2. lost: the active exit for Wayland flush/dispatch errors. The main loop
//! flushes every frame, so a dead compositor is noticed on the first frame.
//! Before this, flush errors were dropped with .ok and wayland-backend
//! printed one "Io error: Broken pipe" per flush (25+ lines at 21:00)
//! before the swap panic.
//! Exit code 75 is distinct from the watchdog (70) and bad arguments (2);
//! systemd Restart=on-failure restarts the service.

pub const EXIT_CODE: i32 = 75;

/// Is this panic location inside khronos-egl? Path based: the panic message is
/// always the same Option::unwrap text, so it cannot tell us which EGL call it was.
pub fn is_egl_panic(file: &str) -> bool {
    file.contains("khronos-egl")
}

/// Does this error text mean "the display connection is gone" (our raw swap /
/// EGL / event dispatch)? Used to classify render errors: retrying is pointless
/// once the compositor is dead, so the kernel should exit cleanly right away.
pub fn is_display_lost(msg: &str) -> bool {
    if is_would_block(msg) {
        return false;
    }
    msg.contains("EGL 提交失败") || msg.contains("显示连接") || msg.contains("Io error")
}

/// Would-block is NOT a lost display .
/// `wl_display_flush` returns -1 with EAGAIN when the compositor is not reading fast enough
/// (its socket buffer is full) - which is exactly what happens when the compositor is busy,
/// stopped for a moment or reconfiguring. The queue can be flushed again on the next lap.
/// Before this, the first EAGAIN was printed as "显示连接断开" and exited 75.
/// Measured : 12 s compositor freeze =>
/// old binary gave up at 9.67 s with `os error 11` while the pending submit had only been
/// outstanding for 7.9 s, i.e. a *busy* compositor was reported as a dead one and the whole
/// scene was rebuilt for nothing.
pub fn is_would_block(msg: &str) -> bool {
    msg.contains("os error 11")
        || msg.contains("Resource temporarily unavailable")
        || msg.contains("os error 4") // EINTR: interrupted syscall, retry
}

/// Print one reason line and exit (never returns).
pub fn lost(reason: impl std::fmt::Display) -> ! {
    eprintln!(
        "[显示连接断开] {reason} => 退出 {EXIT_CODE}（合成器/EGL 不可用；systemd Restart=on-failure 会重拉）"
    );
    std::process::exit(EXIT_CODE)
}

/// Install the panic hook: khronos-egl panics become one line + exit(75);
/// everything else goes through the default hook (which aborts afterwards).
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info.location();
        let file = loc.map(|l| l.file()).unwrap_or("");
        let line = loc.map(|l| l.line()).unwrap_or(0);
        if is_egl_panic(file) {
            let msg = payload_msg(info.payload());
            eprintln!(
                "[显示连接断开] EGL 调用失败（{}:{}）：{} => 退出 {}",
                short_file(file),
                line,
                msg,
                EXIT_CODE
            );
            std::process::exit(EXIT_CODE);
        }
        default(info);
    }));
}

/// Panic payload -> one-line string (covers panic!("x") and panic!("{}", x)).
fn payload_msg(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic)".to_string()
    }
}

/// Keep only "khronos-egl-6.0.0/src/lib.rs", not the whole registry path.
fn short_file(file: &str) -> String {
    match file.find("khronos-egl") {
        Some(i) => file[i..].to_string(),
        None => file.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egl_panic_is_recognised_by_path() {
        let p = "/home/user/.cargo/registry/src/x/khronos-egl-6.0.0/src/lib.rs";
        assert!(is_egl_panic(p));
        assert!(!is_egl_panic("src/main.rs"));
        assert!(!is_egl_panic(
            "/home/user/src/we-scene-render/src/lib.rs"
        ));
    }

    #[test]
    fn display_lost_markers() {
        assert!(is_display_lost(
            "EGL 提交失败（eglSwapBuffers=FALSE，EGL error=EGL_SUCCESS）"
        ));
        assert!(is_display_lost(
            "Wayland flush 失败：Io error: Broken pipe (os error 32)"
        ));
        assert!(!is_display_lost("着色器编译失败：0:12: error"));
        assert!(!is_display_lost("没有满足 WINDOW_BIT+OPENGL 的 EGLConfig"));
    }

    /// a full socket buffer is a busy compositor, not a dead one.
    #[test]
    fn would_block_is_not_a_lost_display() {
        let eagain = "Wayland flush 失败：Io error: Resource temporarily unavailable (os error 11)";
        assert!(is_would_block(eagain));
        assert!(!is_display_lost(eagain));
        // a real dead connection still counts
        let epipe = "Wayland flush 失败：Io error: Broken pipe (os error 32)";
        assert!(!is_would_block(epipe));
        assert!(is_display_lost(epipe));
    }

    #[test]
    fn short_file_keeps_crate_relative_tail() {
        assert_eq!(
            short_file("/a/b/khronos-egl-6.0.0/src/lib.rs"),
            "khronos-egl-6.0.0/src/lib.rs"
        );
        assert_eq!(short_file("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn payload_message_covers_str_and_string() {
        let s: &str = "boom";
        assert_eq!(payload_msg(&s), "boom");
        let owned = String::from("boom2");
        assert_eq!(payload_msg(&owned), "boom2");
    }
}
