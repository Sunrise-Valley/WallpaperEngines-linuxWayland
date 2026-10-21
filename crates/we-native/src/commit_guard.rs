//! what to do when a frame submit is never collected.
//! Round 11-13 made the submit bounded: `eglSwapBuffers` runs on the `we-commit` thread and the
//! main loop waits at most `WE_COMMIT_DEADLINE_MS` (400 ms). That fixed the production stalls
//! (23 recorded, all 8.0-8.5 s, all "compositor reconfigured the output and the frame callback
//! never came"): the loop keeps polling, answering the control socket and drawing the next frame
//! as soon as the late submit resolves.
//!
//! What it did NOT fix is the case where the submit never resolves at all. Then the wallpaper
//! survives but stops presenting: the last frame stays on screen forever, `frames` stops
//! climbing, and nothing in the process can recover, because the EGL context is still held by
//! the thread blocked inside `eglSwapBuffers`. Rebuilding the surface in-process would mean
//! creating a second EGLContext + wl_egl_window on the same wl_display while the abandoned
//! thread is still dispatching Mesa's per-display event queue - two threads on one wl_event_queue
//! is not allowed by the Wayland client API, so that path is a race, not a fix.
//!
//! So the recovery is: say it once, then hand the process to systemd (`Restart=on-failure`) with
//! a *distinct* exit code. The desktop gets its animation back in RestartSec (5 s) + one scene
//! build instead of staying frozen until a human restarts the unit. The exit uses `_exit`
//! (same reason as round 13): any destructor would race the thread still standing inside
//! `eglSwapBuffers`.
//!
//! This module is pure logic so it can be unit-tested without a compositor.

use std::io::Write as _;

/// Exit code: the display surface is unusable, come back with a fresh one.
/// 70 = watchdog stall, 75 = display connection lost (display_guard), 76 = submit never collected.
pub const EXIT_CODE_SUBMIT_LOST: i32 = 76;

/// Default threshold in ms. Every stall on record recovered by itself within 8.0-8.5 s
/// , so 15 s is ~1.8x the worst observed recovery: a real
/// recovery is never punished, a submit that is truly gone is caught in 15 s.
pub const DEFAULT_MAX_PENDING_MS: u64 = 15_000;

/// Parse the limit. Unset / empty / garbage => default. `0` => disabled (freeze forever, the
/// pre-round-14 behaviour).
pub fn limit_from(raw: Option<&str>) -> u64 {
    match raw.map(|s| s.trim()) {
        None | Some("") => DEFAULT_MAX_PENDING_MS,
        Some(s) => match s.parse::<u64>() {
            Ok(0) => 0,
            Ok(v) => v,
            Err(_) => DEFAULT_MAX_PENDING_MS,
        },
    }
}

/// Is the outstanding submit past the limit? `limit == 0` disables the check.
pub fn exceeded(pending_ms: u64, limit_ms: u64) -> bool {
    limit_ms > 0 && pending_ms >= limit_ms
}

/// What to do when the compositor closes our layer surface.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SurfaceGone {
    /// A submit is still out: dropping the renderer would tear the EGL graph down under the
    /// blocked `we-commit` thread (round 13: 1/6 runs died with SIGSEGV when that branch fired).
    ExitForRestart,
    /// Nothing is in flight: the renderer can be dropped and the surface re-created in-process.
    Recreate,
}

/// Decide from the one fact that matters (is a submit still outstanding).
pub fn surface_gone_action(commit_pending: bool) -> SurfaceGone {
    if commit_pending {
        SurfaceGone::ExitForRestart
    } else {
        SurfaceGone::Recreate
    }
}

/// Exit through `_exit` with `code`, after flushing what we printed.
///
/// `_exit` and not `exit`: `exit` runs atexit handlers, and mesa/LLVM use them to tear down the
/// state a still-running `eglSwapBuffers` is standing on (round 13 measured 1/12 SIGSEGV with
/// `process::exit(0)` vs 0/16 with `_exit`).
pub fn exit_now(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe { libc::_exit(code) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limit_is_above_every_recorded_recovery() {
        // worst observed self-recovery was 8.5 s; the default must not fire on those
        assert!(!exceeded(8_500, limit_from(None)));
        assert_eq!(limit_from(None), DEFAULT_MAX_PENDING_MS);
    }

    #[test]
    fn zero_disables_the_check() {
        assert_eq!(limit_from(Some("0")), 0);
        assert!(!exceeded(10 * 60 * 1000, 0));
    }

    #[test]
    fn garbage_falls_back_to_the_default() {
        assert_eq!(limit_from(Some("abc")), DEFAULT_MAX_PENDING_MS);
        assert_eq!(limit_from(Some("")), DEFAULT_MAX_PENDING_MS);
        assert_eq!(limit_from(Some("3000")), 3000);
    }

    #[test]
    fn boundary_is_inclusive() {
        assert!(!exceeded(2_999, 3000));
        assert!(exceeded(3_000, 3000));
        assert!(exceeded(3_001, 3000));
    }

    #[test]
    fn surface_gone_exits_only_when_a_submit_is_out() {
        assert_eq!(surface_gone_action(true), SurfaceGone::ExitForRestart);
        assert_eq!(surface_gone_action(false), SurfaceGone::Recreate);
    }
}
