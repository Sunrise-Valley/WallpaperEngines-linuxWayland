//! 慢圈归因的宿主侧：一圈结束后把它"卡在哪一段"打一行。
//! 为什么需要它：看门狗能告诉你"落后了"、阶段名能告诉你"停在 poll 还是 scene"，但分不清
//! 「缺页把这一帧拖慢」与「在 eglSwapBuffers 上等合成器」。渲染器侧已按帧在
//! `draw_frame` 与 `eglSwapBuffers` 前后各打一个单调时钟读数（`we_scene_render::trace`），
//! 这里只负责：读开关、在循环的其余位置补点、算差值、打印。
//! 开关 `WE_LOOP_SLOW_MS`（毫秒），默认未设就等于关：`trace::enabled` 为假，一个点都不打，
//! 像素与上屏行为逐字节不变。`WE_LOOP_SLOW_DUMP=1` 时再附一份全线程现场，默认关。
//!
//! 口径说明（避免又一次"拿错的钟比"）：
//!
//! * 所有差值都是同一进程内 `Instant` 的差，不受墙钟跳变影响；
//!
//! * 「其它」等于整圈减去已量到的各段之和，负数按 0 记，故意留出来给没打点的地方；
//!
//! * 一圈里可能一个帧都没画（frames 为 0），那时 scene 段为空，poll 是合法等待。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use we_scene_render::trace::{self, Mark};

/// `dump_cap_ms` handed over by the kernel for the lap in progress (f64 bits, 0 = not set).
/// The kernel already times that memcpy for its `[转储]` line; without this it landed in
/// `scene ... rest` and looked like builder/draw time.
static DUMP_MS_BITS: AtomicU64 = AtomicU64::new(0);

/// Called by the kernel right after a dump capture, with the same number it logs.
pub fn note_dump_ms(ms: f64) {
    if !trace::enabled() || !(ms > 0.0) {
        return;
    }
    DUMP_MS_BITS.store(ms.to_bits(), Ordering::Relaxed);
}

/// 一段耗时。没打过点的段打印成横线
#[derive(Default)]
pub struct Lap {
    /// Seconds since the process started (to line the lap up with outside samples).
    pub t_s: f64,
    pub total_ms: f64,
    pub prepare_ms: Option<f64>,
    pub poll_ms: Option<f64>,
    pub dispatch_ms: Option<f64>,
    pub scene_ms: Option<f64>,
    pub build_ms: Option<f64>,
    pub draw_ms: Option<f64>,
    pub swap_ms: Option<f64>,
    pub cmd_ms: Option<f64>,
    /// Frames this lap **handed to** the compositor (eglSwapBuffers started).
    pub submits: u64,
    /// Submits this lap **collected** (got a result for). A bounded submit that timed out
    /// leaves `submits=1 swaps=0` on the drawing lap and `submits=0 swaps=1` on the collecting one.
    pub swaps: u64,
    /// Readback memcpy for frame dumping (0-copy stays on the GPU; only the copy is timed).
    pub dump_ms: Option<f64>,
    pub frames: u64,
    pub thread: String,
    pub mem: String,
}

/// 读开关并交给渲染器侧的 trace，返回阈值毫秒，0 表示关闭
pub fn init_from_env() -> u64 {
    let ms = std::env::var("WE_LOOP_SLOW_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    trace::init(ms);
    if ms > 0 {
        println!(
            "[慢圈] 已开: 单圈 >= {ms} ms 时打印分段耗时 (poll / scene[builder|draw|swap] / 命令)"
        );
    }
    ms
}

pub fn enabled() -> bool {
    trace::enabled()
}

/// Start of one main-loop lap: clear the previous marks, then mark LoopBegin.
/// Returns None when the probe is off (so the caller skips the clock read too).
pub fn start() -> Option<Instant> {
    if !trace::enabled() {
        return None;
    }
    trace::reset();
    DUMP_MS_BITS.store(0, Ordering::Relaxed);
    trace::mark(Mark::LoopBegin);
    Some(Instant::now())
}

pub fn mark(m: Mark) {
    trace::mark(m);
}

/// End of one lap: subtract the marks, print one line if this lap was slow.
/// `frames` is how many frames this lap actually drew.
pub fn finish(t0: Instant, frames: u64, t_since_start: f64) {
    let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if !trace::enabled() || total_ms < trace::threshold_ms() as f64 {
        return;
    }
    let mut lap = Lap {
        t_s: t_since_start,
        total_ms,
        prepare_ms: trace::span_ms(Mark::LoopBegin, Mark::PollBegin),
        poll_ms: trace::span_ms(Mark::PollBegin, Mark::PollEnd),
        dispatch_ms: trace::span_ms(Mark::PollEnd, Mark::DispatchEnd),
        scene_ms: trace::span_ms(Mark::SceneBegin, Mark::SceneEnd),
        build_ms: trace::span_ms(Mark::SceneBegin, Mark::DrawBegin),
        draw_ms: trace::span_ms(Mark::DrawBegin, Mark::DrawEnd),
        swap_ms: trace::span_ms(Mark::SwapBegin, Mark::SwapEnd),
        cmd_ms: trace::span_ms(Mark::CmdBegin, Mark::CmdEnd),
        submits: trace::submits(),
        swaps: trace::swaps(),
        dump_ms: match DUMP_MS_BITS.load(Ordering::Relaxed) {
            0 => None,
            bits => Some(f64::from_bits(bits)),
        },
        frames,
        thread: thread_state(),
        mem: mem_line(),
    };
    if std::env::var("WE_LOOP_SLOW_DUMP").map(|v| v != "0").unwrap_or(false) {
        let dump = crate::watchdog::dump_all_threads("slow-lap");
        lap.thread = format!("{}\n{}", lap.thread, dump);
    }
    println!("{}", format_lap(&lap));
}

/// 开火瞬间的**部分**分段：这一圈还没结束，但已经打过的点都在。
/// 为什么必须有它（本次实测的洞）：`finish` 只在圈尾打印，而看门狗在**圈中间** `exit(70)`。
/// 10-18/10-19 两天里 23 次停摆，`[slow-lap]` 一行都没有 —— 不是探针没生效，是**圈没走完**，
/// 证据被自己的退路抹掉了。这里在开火前把"最后打点是哪个、之后过去多久"打出来：
/// `last=SwapBegin +8200ms` 就等于"卡在 eglSwapBuffers"，不用等它自己回来。
///
/// 关着（默认）时返回 `None`，一次原子读，零成本。
pub fn partial() -> Option<String> {
    if !trace::enabled() {
        return None;
    }
    const NAMES: [(&str, Mark); 13] = [
        ("LoopBegin", Mark::LoopBegin),
        ("PollBegin", Mark::PollBegin),
        ("PollEnd", Mark::PollEnd),
        ("DispatchEnd", Mark::DispatchEnd),
        ("SceneBegin", Mark::SceneBegin),
        ("DrawBegin", Mark::DrawBegin),
        ("DrawEnd", Mark::DrawEnd),
        ("SwapBegin", Mark::SwapBegin),
        ("SwapEnd", Mark::SwapEnd),
        ("SceneEnd", Mark::SceneEnd),
        ("CmdBegin", Mark::CmdBegin),
        ("CmdEnd", Mark::CmdEnd),
        ("LoopEnd", Mark::LoopEnd),
    ];
    let now = trace::now_ms();
    let loop_at = trace::at_ms(Mark::LoopBegin);
    let mut last: Option<(&'static str, f64)> = None;
    let mut seen: Vec<String> = Vec::new();
    for (name, m) in NAMES {
        if let Some(at) = trace::at_ms(m) {
            let rel = loop_at.map(|l| at - l).unwrap_or(at);
            seen.push(format!("{name}=+{rel:.0}"));
            if last.map_or(true, |(_, l)| at > l) {
                last = Some((name, at));
            }
        }
    }
    let (name, at) = last?;
    let since = now - at;
    Some(format!(
        "[slow-lap·开火现场] 这一圈没走完：最后打点={}（圈内 +{:.0}ms），之后已过去 {:.0}ms；\n  \
         已打点: {}",
        name,
        loop_at.map(|l| at - l).unwrap_or(at),
        since,
        seen.join(" ")
    ))
}

fn fmt_ms(v: Option<f64>) -> String {
    match v {
        Some(ms) => format!("{ms:.1}"),
        None => "-".to_string(),
    }
}

/// One line per slow lap. Pure function so the arithmetic is unit-testable.
pub fn format_lap(l: &Lap) -> String {
    let mut known: f64 = [
        l.prepare_ms,
        l.poll_ms,
        l.dispatch_ms,
        l.scene_ms,
        l.cmd_ms,
    ]
    .iter()
    .flatten()
    .sum();
    // A video / image source has no SceneBegin/SceneEnd bracket, so its draw+swap marks are the
    // only inner segments we have. Count them, or `other` reports the same milliseconds twice.
    if l.scene_ms.is_none() {
        known += [l.build_ms, l.draw_ms, l.swap_ms, l.dump_ms]
            .iter()
            .flatten()
            .sum::<f64>();
    }
    let other = (l.total_ms - known).max(0.0);
    let scene_other = match (l.scene_ms, l.build_ms, l.draw_ms, l.swap_ms) {
        (Some(s), b, d, w) => {
            // The dump memcpy happens inside the SceneBegin/SceneEnd bracket, so it must be
            // taken out of `rest` or it reads as builder time (~8 ms per 2560x1440 frame).
            let inner: f64 = [b, d, w, l.dump_ms].iter().flatten().sum();
            format!("{:.1}", (s - inner).max(0.0))
        }
        _ => "-".to_string(),
    };
    // Only grow the line when there is something to say: a slow lap that is not dumping keeps
    // the old shape byte for byte.
    let dump_seg = match l.dump_ms {
        Some(ms) => format!(" dump {ms:.1}"),
        None => String::new(),
    };
    format!(
        "[slow-lap] t={:.3}s {:.1}ms frames={} submits={} swaps={} | prepare {} | poll {} | dispatch {} | \
         scene {} [build {} draw {} swap {} rest {}{}] | cmd {} | other {:.1} | main {} | {}",
        l.t_s,
        l.total_ms,
        l.frames,
        l.submits,
        l.swaps,
        fmt_ms(l.prepare_ms),
        fmt_ms(l.poll_ms),
        fmt_ms(l.dispatch_ms),
        fmt_ms(l.scene_ms),
        fmt_ms(l.build_ms),
        fmt_ms(l.draw_ms),
        fmt_ms(l.swap_ms),
        scene_other,
        dump_seg,
        fmt_ms(l.cmd_ms),
        other,
        l.thread,
        l.mem,
    )
}

/// Main thread state/wchan right now (after the stall ended - the segment split, not
/// this string, is what says where the time went; this is a cheap sanity line).
fn thread_state() -> String {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let state = crate::watchdog::parse_task_stat(&stat)
        .map(|(_, c)| c)
        .unwrap_or('?');
    let wchan = std::fs::read_to_string("/proc/self/wchan")
        .unwrap_or_default()
        .trim()
        .to_string();
    format!("state={state} wchan={wchan}")
}

fn kb_field(status: &str, key: &str) -> Option<u64> {
    status.lines().find(|l| l.starts_with(key))?.split_whitespace().nth(1)?.parse().ok()
}

/// Read-only GPU memory accounting from sysfs (world readable, no debugfs needed).
fn gpu_mem() -> Option<(u64, u64)> {
    let rd = std::fs::read_dir("/sys/class/drm").ok()?;
    let (mut gtt, mut vram) = (0u64, 0u64);
    let mut any = false;
    for e in rd.flatten() {
        let base = e.path().join("device");
        if let Ok(s) = std::fs::read_to_string(base.join("mem_info_gtt_used")) {
            if let Ok(v) = s.trim().parse::<u64>() {
                gtt = gtt.max(v);
                any = true;
            }
        }
        if let Ok(s) = std::fs::read_to_string(base.join("mem_info_vram_used")) {
            if let Ok(v) = s.trim().parse::<u64>() {
                vram = vram.max(v);
            }
        }
    }
    any.then_some((gtt, vram))
}

fn mem_line() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let rss = kb_field(&status, "VmRSS:").map(|kb| kb as f64 / 1024.0);
    let swp = kb_field(&status, "VmSwap:").map(|kb| kb as f64 / 1024.0);
    let gpu = gpu_mem()
        .map(|(g, v)| {
            format!(
                "gtt={:.0}MB vram={:.0}MB",
                g as f64 / 1048576.0,
                v as f64 / 1048576.0
            )
        })
        .unwrap_or_else(|| "gtt=- vram=-".to_string());
    format!(
        "rss={:.1}MB swap={:.1}MB {gpu}",
        rss.unwrap_or(f64::NAN),
        swp.unwrap_or(f64::NAN)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(total: f64) -> Lap {
        Lap {
            t_s: 1.0,
            total_ms: total,
            prepare_ms: Some(1.0),
            poll_ms: Some(16.0),
            dispatch_ms: Some(0.5),
            scene_ms: Some(2590.0),
            build_ms: Some(10.0),
            draw_ms: Some(20.0),
            swap_ms: Some(2560.0),
            cmd_ms: None,
            submits: 1,
            swaps: 1,
            dump_ms: None,
            frames: 1,
            thread: "state=R wchan=-".to_string(),
            mem: "rss=1.0MB".to_string(),
        }
    }

    #[test]
    fn other_is_the_remainder_of_the_lap() {
        let line = format_lap(&sample(2612.4));
        // known = 1.0 + 16.0 + 0.5 + 2590.0 = 2607.5 => other 4.9
        assert!(line.contains("other 4.9"), "{line}");
        // scene rest = 2590 - (10 + 20 + 2560) = 0
        assert!(line.contains("rest 0.0"), "{line}");
        assert!(line.contains("2612.4ms frames=1 submits=1 swaps=1"), "{line}");
        // nothing was dumped on this lap: the line keeps the old shape (no ` dump` segment)
        assert!(!line.contains("dump"), "{line}");
    }

    #[test]
    fn a_pending_submit_is_one_submit_and_zero_collected_swaps() {
        // The lap that drew and handed the frame over but timed out waiting for the compositor.
        let mut l = sample(3000.0);
        l.submits = 1;
        l.swaps = 0;
        let line = format_lap(&l);
        assert!(line.contains("frames=1 submits=1 swaps=0"), "{line}");
        // ... and the lap that only collected it: nothing drawn, nothing submitted, one swap.
        let mut c = sample(2600.0);
        c.frames = 0;
        c.submits = 0;
        c.swaps = 1;
        let line = format_lap(&c);
        assert!(line.contains("frames=0 submits=0 swaps=1"), "{line}");
    }

    #[test]
    fn the_dump_copy_gets_its_own_segment_and_leaves_scene_rest() {
        // 8 ms of dump inside a scene bracket that grew by the same 8 ms
        let mut l = sample(2620.4);
        l.scene_ms = Some(2598.0);
        l.dump_ms = Some(8.0);
        let line = format_lap(&l);
        // rest = 2598 - (10 + 20 + 2560 + 8) = 0
        assert!(line.contains("rest 0.0 dump 8.0"), "{line}");
        // and the dump time is not double-counted into `other`
        assert!(line.contains("other 4.9"), "{line}");
    }

    #[test]
    fn missing_marks_print_a_dash_and_other_never_goes_negative() {
        let mut l = sample(100.0); // 100 < known 2607.5 => clamp at 0
        l.cmd_ms = None;
        l.dispatch_ms = None;
        let line = format_lap(&l);
        assert!(line.contains("other 0.0"), "{line}");
        assert!(line.contains("dispatch -"), "{line}");
        assert!(line.contains("cmd -"), "{line}");
    }

    #[test]
    fn partial_names_the_last_mark_and_its_age() {
        trace::init(500);
        trace::reset();
        trace::mark(Mark::LoopBegin);
        std::thread::sleep(std::time::Duration::from_millis(3));
        trace::mark(Mark::SwapBegin);
        // then sit in "the blocking call" for 3 ms, like a real stall inside swap
        std::thread::sleep(std::time::Duration::from_millis(3));
        let line = partial().expect("probe is on, so a partial lap must be printable");
        assert!(line.contains("最后打点=SwapBegin"), "{line}");
        // 3 ms between the two marks, so the relative stamp must not be 0
        assert!(line.contains("SwapBegin=+3") || line.contains("SwapBegin=+2"), "{line}");
        assert!(line.contains("已打点: LoopBegin=+0"), "{line}");
        // the age of the last mark is the number that says "how long we have been stuck here"
        let tail = line.split("之后已过去 ").nth(1).expect("age is part of the line");
        let since: f64 = tail
            .split("ms")
            .next()
            .unwrap()
            .trim()
            .parse()
            .expect("age must be a plain number");
        assert!(since >= 2.5, "3 ms elapsed after the last mark, got {since}");
        trace::init(0);
        assert!(partial().is_none(), "off by default => nothing printed");
    }

    #[test]
    fn video_lap_counts_draw_and_swap_without_a_scene_bracket() {
        // A video wallpaper marks Draw/Swap but never SceneBegin/SceneEnd.
        let mut l = sample(420.0);
        l.scene_ms = None;
        l.build_ms = None;
        l.draw_ms = Some(12.5);
        l.swap_ms = Some(400.0);
        l.poll_ms = Some(5.0);
        l.dispatch_ms = Some(0.5);
        l.prepare_ms = Some(1.0);
        let line = format_lap(&l);
        // known = 1.0 + 5.0 + 0.5 + 12.5 + 400.0 = 419.0 => other 1.0 (406.5 without the fix)
        assert!(line.contains("other 1.0"), "{line}");
        assert!(line.contains("draw 12.5"), "{line}");
        assert!(line.contains("swap 400.0"), "{line}");
        assert!(line.contains("rest -"), "{line}");
    }

    #[test]
    fn no_scene_means_dash_inside_brackets() {
        let mut l = sample(20.0);
        l.scene_ms = None;
        l.build_ms = None;
        l.draw_ms = None;
        l.swap_ms = None;
        let line = format_lap(&l);
        assert!(line.contains("scene - [build - draw - swap - rest -]"), "{line}");
    }
}
