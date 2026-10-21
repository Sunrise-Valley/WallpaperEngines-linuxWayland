// SPDX-License-Identifier: MIT
//! 慢圈归因的时间戳——默认关，零行为变化。
//! 为什么要有它：主循环偶尔单圈阻塞数秒（的 8.4 s 现场），看门狗只能告诉你
//! "落后了"，告诉不了"落后在哪一段"。光靠阶段名（poll/scene）也分不清
//! 「缺页把这一帧拖慢」与「在 eglSwapBuffers 上等合成器」。这里在几个关键点各存一个
//! 单调时钟读数（自进程基准时刻起的纳秒），主循环一圈结束后一次性取走、相减、打一行。
//! 开关：WE_LOOP_SLOW_MS=<毫秒>（默认未设 => enabled 恒假，所有 mark 立刻返回，
//! 只多一次 Relaxed 原子读）。基准时刻用 Instant（不是墙钟）=> 不受 NTP 跳变影响。
//! 打点分工：LoopBegin/PollBegin/PollEnd/DispatchEnd/SceneBegin/SceneEnd/CmdBegin/CmdEnd
//! 由内核主循环打；DrawBegin/DrawEnd/SwapBegin/SwapEnd 由渲染器在真正画帧/交帧处打
//! （同一进程里的同一份静态变量，跨 crate 直接读）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// 未打点 / 已清除的哨兵值（纳秒；Instant::elapsed 永远到不了 584 年）。
const NS_UNSET: u64 = u64::MAX;

macro_rules! mark_slots {
    ($($name:ident),* $(,)?) => {
        /// 打点位置。判别值就是 MARKS 的下标（顺序即语义分组）。
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Mark { $($name),* }
        const SLOT_COUNT: usize = Mark::LoopEnd as usize + 1;
        static MARKS: [AtomicU64; SLOT_COUNT] = [const { AtomicU64::new(NS_UNSET) }; SLOT_COUNT];
    };
}

mark_slots!(
    LoopBegin,
    PollBegin,
    PollEnd,
    DispatchEnd,
    SceneBegin,
    DrawBegin,
    DrawEnd,
    SwapBegin,
    SwapEnd,
    SceneEnd,
    CmdBegin,
    CmdEnd,
    LoopEnd,
);

static ON: AtomicBool = AtomicBool::new(false);
static THRESHOLD_MS: AtomicU64 = AtomicU64::new(0);
static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static SWAPS: AtomicU64 = AtomicU64::new(0);
/// frames this lap **handed to** eglSwapBuffers (submitted), as opposed to
/// SWAPS, which counts submits **collected** in this lap. They used to be one number bumped
/// at collection time, so the lap that actually submitted a frame (and timed out waiting)
/// printed `swaps=0` while the next, idle lap printed `swaps=1`. Two numbers, two facts.
static SUBMITS: AtomicU64 = AtomicU64::new(0);

fn base() -> &'static Instant {
    BASE.get_or_init(Instant::now)
}

/// 打开开关。threshold_ms == 0 => 保持关闭（默认）。
pub fn init(threshold_ms: u64) {
    THRESHOLD_MS.store(threshold_ms, Ordering::Relaxed);
    if threshold_ms > 0 {
        let _ = base(); // 先把基准钉在"打开时"，第一圈也算得出绝对值
    }
    // `init(0)` 必须真的关掉：文档说"0 => 保持关闭"，而旧实现只会打开不会关闭
    // （ 第十轮补单测时踩到；生产路径只在启动时调一次，所以从没暴露）。
    ON.store(threshold_ms > 0, Ordering::Relaxed);
}

#[inline]
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

pub fn threshold_ms() -> u64 {
    THRESHOLD_MS.load(Ordering::Relaxed)
}

/// 打一个点（关着时是一条原子读 + 返回，不取时钟）。
#[inline]
pub fn mark(m: Mark) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    MARKS[m as usize].store(base().elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// 清掉上一圈的所有点（LoopBegin 之后马上调）。关着时什么也不做。
pub fn reset() {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    for s in MARKS.iter() {
        s.store(NS_UNSET, Ordering::Relaxed);
    }
    SWAPS.store(0, Ordering::Relaxed);
    SUBMITS.store(0, Ordering::Relaxed);
}

/// 交帧次数（一圈里可能 0 次：没到该画帧的时候）。
#[inline]
pub fn bump_swap() {
    if ON.load(Ordering::Relaxed) {
        SWAPS.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn swaps() -> u64 {
    SWAPS.load(Ordering::Relaxed)
}

/// 本圈**交给**合成器的帧数（发起 eglSwapBuffers 的次数）。
/// 与 `swaps` 的区别就是本次修的口径：`swaps` 是**收回**（拿到结果）的次数，
/// `submits` 是**提交**的次数。有界交帧超时的那一圈 `submits=1 swaps=0`，
/// 下一圈只做回收 `submits=0 swaps=1` —— 以前两个都是一样的 0/1，读起来像是
/// 「这一圈什么也没交，下一圈没画却交了一帧」。
#[inline]
pub fn bump_submit() {
    if ON.load(Ordering::Relaxed) {
        SUBMITS.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn submits() -> u64 {
    SUBMITS.load(Ordering::Relaxed)
}

/// 现在（与所有打点同一个基准时刻的毫秒数）。
/// 第十轮：看门狗开火时主循环那一圈**还没结束**，`span_ms` 只算得完两端都打过的段
/// 所以需要一个"现在"来量「进了最后一段之后已经过去多久」。
pub fn now_ms() -> f64 {
    base().elapsed().as_secs_f64() * 1000.0
}

/// 从基准时刻到这个点的毫秒数，没打过就返回空
pub fn at_ms(m: Mark) -> Option<f64> {
    match MARKS[m as usize].load(Ordering::Relaxed) {
        NS_UNSET => None,
        ns => Some(ns as f64 / 1e6),
    }
}

/// 两点之间的毫秒差。两点都打过才有值，否则为空
pub fn span_ms(a: Mark, b: Mark) -> Option<f64> {
    let x = MARKS[a as usize].load(Ordering::Relaxed);
    let y = MARKS[b as usize].load(Ordering::Relaxed);
    if x == NS_UNSET || y == NS_UNSET {
        return None;
    }
    Some((y as f64 - x as f64) / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The toggle and the mark array are process globals, so these tests must not
    /// interleave (cargo runs them on several threads by default).
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn disabled_marks_nothing() {
        let _g = serial();
        init(0);
        reset();
        mark(Mark::SwapBegin);
        assert!(at_ms(Mark::SwapBegin).is_none());
        assert!(!enabled());
    }

    #[test]
    fn enabled_marks_are_monotonic_and_spans_add_up() {
        let _g = serial();
        init(500);
        assert!(enabled());
        reset();
        mark(Mark::LoopBegin);
        std::thread::sleep(std::time::Duration::from_millis(2));
        mark(Mark::SwapBegin);
        std::thread::sleep(std::time::Duration::from_millis(3));
        mark(Mark::SwapEnd);
        let d = span_ms(Mark::SwapBegin, Mark::SwapEnd).expect("both ends marked");
        assert!(d >= 2.5, "swap span should be >= 2.5ms, got {d}");
        let all = span_ms(Mark::LoopBegin, Mark::SwapEnd).expect("whole span");
        assert!(all > d, "whole {all} should exceed swap {d}");
        reset();
        assert!(span_ms(Mark::SwapBegin, Mark::SwapEnd).is_none());
        init(0);
    }

    #[test]
    fn swap_counter_resets_with_lap() {
        let _g = serial();
        init(500);
        reset();
        bump_swap();
        bump_swap();
        assert_eq!(swaps(), 2);
        reset();
        assert_eq!(swaps(), 0);
        init(0);
    }
}
