//! 渲染循环看门狗：让内核"卡死了也能自恢复"。
//!
//! ## 为什么必须有它（不是锦上添花）
//! 内核的渲染是**同步**的：`tick_scene` 里 0 拷贝那条路会调 `eglSwapBuffers`，
//! 而 Wayland EGL 平台在 swap 里会走 `wl_display_dispatch` ⇒ 在 `poll(display_fd, -1)`
//! 上**无限期**等合成器的 frame callback / buffer release。
//! 实测（23:25:39，现场已取栈）：
//! ```text
//! libc +0xde45e poll / ppoll（超时 NULL = 无限）
//! libwayland-client wl_display_dispatch_queue
//! libEGL_mesa wayland EGL swap 路径
//! we-native +0x5e2ff4 ← 我们的 tick_scene / render_window（eglSwapBuffers）
//! ```
//! 触发点是**合成器做了一次输出重配**（journal 同刻：`cosmic-comp: Failed to set xwayland
//! primary output …`、`cosmic-session: Overlap notify … No matching layer surface found`）
//! ⇒ 我们的 layer 表面被作废，frame callback 再也不来 ⇒ swap 永不返回。
//! 后果：主线程永久停在 poll、CPU 0、**无任何日志/报错**、出帧停止，控制面也一起死
//! （我们自己的 wake fd 不在 Mesa 那个 poll 集合里，唤醒字节递不到）。
//! **`Restart=on-failure` 对这种情况完全无效**：进程没退出、systemd 眼里 `active`，
//! 只能等人手动 `systemctl --user restart`（两次事故就是这么恢复的）。
//!
//! ## 瞬时停摆 vs 死锁
//! 现场两次重启（18:40:07 / 18:54:00）都是**单圈阻塞 8.4 s / 9.0 s、随后自己回来了**，
//! 背景是整机内存压力（swap 已用 6.9 GB、可用 1.5 GB）。对"慢"就重启动画时钟是净亏：
//! 停摆本来就快结束，重启只多重置一次相位。所以判定拆成两级：
//! 落后 > `GRACE`(8 s) 且 ≤ `HARD_CAP`(30 s) ⇒ **容错**：每秒一行日志，不退出；
//! 落后 > `HARD_CAP`，或 120 s 内已容错 `MAX_TRANSIENT`(2) 次 ⇒ 才退出（真死锁 / 反复抖动）。
//! `WE_WATCHDOG_TOLERATE=0` 精确回旧行为（第一次超过宽限就退出）；`WE_WATCHDOG_HARD_MS` 调硬上限。
//!
//! ## 机制
//! 主循环每一圈**在进 poll 之前**把"我下一次该回来的时刻"发布出来（`due`）；本模块的巡检
//! 线程每 `POLL_EVERY` 比一次，**超过 `due + GRACE` 还不见新的一圈** ⇒ 判定停摆，
//! 打印一行原因后 `std::process::exit(WATCHDOG_EXIT_CODE)` ⇒ systemd `Restart=on-failure`
//! 立刻重拉。重启会把动画时钟归零（从 t=0 重播）—— 这是已知代价，换来的是"不用等人"。
//!
//! ## 为什么不用 systemd 的 `WatchdogSec=` + `sd_notify`
//!
//! 1. `WatchdogSec` 要写进 drop-in（`WatchdogSec=` + 进程必须**周期性**发 `WATCHDOG=1`）；
//! 而"合法长睡"（被遮挡睡眠 / 用户暂停 / 没有源）时主循环本来就该**一动不动**
//! （0 唤醒是设计目标），发不出心跳 ⇒ 会在长睡时被 systemd 误杀。
//!
//! 2. 本模块知道"现在本来就该停"（`park`），所以只会对**真正违反自己排定的唤醒时刻**开火。
//!
//! 3. 不动 drop-in 也能生效（本次硬性要求）。
//! ⇒ 选进程内看门狗；它与 systemd 的 `Restart=on-failure` 是**配合**关系（负责"咬一口退出"）。
//!
//! ## 覆盖边界（**为什么不是万能**，如实记 —— 本模块本身不改这些）
//! 本模块只在主循环**自己排定的**唤醒时刻上判违约（`should_fire`）。因此：
//! **被遮挡睡眠 / 用户暂停 / 没有源** 时主循环 `park`（`due = i64::MAX`）⇒ 永不介入。
//! 这是设计（长睡本就是目标），但也意味着"**假睡眠**"（窗口事实被错报成全屏/铺满，而状态
//! 再也没被刷新回 false）会表现为"0 出帧却不触发看门狗"。可区分特征：**控制面仍然秒回**
//! （wake fd 一直在 poll 集合里）—— 与 `eglSwapBuffers` 卡死（控制面一起死）不同。
//! `fds.is_empty` 那条 `blocking_dispatch` 分支同样 `park` ⇒ 若卡在那里面不会被咬。
//! 命令处理期间临时臂 `CMD_ARM = 60s` ⇒ 那一段里最长要等约 68s 才发现（apply 重建是合法慢操作）。
//! 卡死期间**控制面不可用**（Mesa 那个 `poll` 集合里没有我们的 wake fd）⇒ 这 ~9 秒 `status`
//! 也会超时；要"卡着也能 status"得把 status 也搬到独立线程（**没做**）。
//!
//! ## 现场取证（**默认全关**；`WE_WATCHDOG_DUMP=1` / `WE_WATCHDOG_WARN_MS=<ms>` 才启用）
//! 开火时（以及可选地在**开火之前提前**）把本进程所有线程的 `state/wchan/syscall` 打到 stderr
//! ⇒ journal 里直接留下"主线程卡在 `ppoll`（wayland fd）"这类判据，**不用 gdb、不用 root**。
//! 两个旋钮都默认关 ⇒ 上屏路径与加它们之前逐字节一致（只多两次 `getenv`）。

use std::io::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 看门狗专用退出码（非 0 ⇒ systemd `Restart=on-failure` 会重拉）。
pub const EXIT_CODE: i32 = 70;
/// 宽限：`due` 之后再过这么久还没有新的一圈，才认定停摆。
/// 取 8 秒的依据：主循环里最长的**合法**单段阻塞是"控制面 apply 重建渲染器/场景"
/// （实测构建 0.5~2s，再加上 GL 上下文与音频准备）；8 秒对它是安全的，同时把"真卡死"
/// 的发现时间压到约 10 秒级（配合 30fps 的 due ≈ 33ms）。
pub const GRACE: Duration = Duration::from_millis(8000);
/// 巡检间隔。
pub const POLL_EVERY: Duration = Duration::from_millis(500);
/// 处理控制面命令期间临时臂的更长上限：apply 这类操作天然慢，不能按帧节奏要求它。
pub const CMD_ARM: Duration = Duration::from_secs(60);
/// **单次**停摆的硬上限。现场两次事故是 8.4 s / 9.0 s 的单圈阻塞，随后自己回来了
/// 那是"慢"不是"死"。宽限（8 s）只用来判定"已经不正常"，真死锁的判据用这个上限。
pub const HARD_CAP: Duration = Duration::from_secs(30);
/// 同一窗口内最多容错几次：第 `MAX_TRANSIENT + 1` 次直接开火 ⇒ 反复抖动的连接仍会被重启。
pub const MAX_TRANSIENT: u32 = 2;
/// 容错计数的老化窗口：这么久没有再犯，就把计数清零（偶发一次不该攒着算账）。
pub const STALL_WINDOW: Duration = Duration::from_secs(120);

static EPOCH: OnceLock<Instant> = OnceLock::new();
/// 主循环下一次"应该回来"的时刻（相对 EPOCH 的毫秒）。`i64::MAX` = 合法长睡，别管我。
static DUE_MS: AtomicI64 = AtomicI64::new(i64::MAX);
/// 0 = 还没开过火。
static FIRED: AtomicU64 = AtomicU64::new(0);

/// 主循环此刻停在哪一段（默认只有几个原子写，不改变任何行为）。
/// 线程现场能说清「卡在哪个 syscall / 是不是在用户态跑」，但说不清「用户态那几秒是在画帧
/// 还是在 apply 重建」。 的两次停摆连现场都没留下，所以主循环每进一段就记一次（连同
/// 进入时刻）：开火/告警时打出「阶段=scene，已在该阶段 8273ms」⇒ 8 秒花在哪一步一眼可见。
pub const PHASE_POLL: u8 = 1;
pub const PHASE_SLEEP: u8 = 2;
pub const PHASE_SCENE: u8 = 3;
pub const PHASE_COMMAND: u8 = 4;
pub const PHASE_DISPATCH: u8 = 5;

const PHASE_NAMES: [&'static str; 6] = ["boot", "poll", "sleep", "scene", "command", "dispatch"];

static PHASE: AtomicU8 = AtomicU8::new(0);
/// 进入当前阶段的时刻（相对 EPOCH 的毫秒）。
static PHASE_SINCE_MS: AtomicI64 = AtomicI64::new(0);

/// 记下主循环进到哪一段了（只写一个原子；同一段重复设置不刷新进入时刻）。
pub fn set_phase(p: u8) {
    EPOCH.get_or_init(Instant::now);
    if PHASE.swap(p, Ordering::Relaxed) != p {
        let now = now_ms().max(1); // 0 表示尚未记录，避免进入时刻恰好落在 0ms
        PHASE_SINCE_MS.store(now, Ordering::Relaxed);
    }
}

/// 当前阶段名与「已在该阶段多少毫秒」（线程现场的抬头用）。
pub fn phase_snapshot() -> (&'static str, i64) {
    let i = PHASE.load(Ordering::Relaxed) as usize;
    let name = PHASE_NAMES.get(i).copied().unwrap_or("boot");
    let since = PHASE_SINCE_MS.load(Ordering::Relaxed);
    let mut el = 0;
    if since != 0 {
        el = now_ms() - since;
    }
    (name, el.max(0))
}

/// 判据是**纯函数**（好测）：`due` 已过且超过宽限 ⇒ 给出落后毫秒；否则 `None`。
/// 之后主循环已改走 [`judge_stall`]（同样的边界 + 容错分级）；这个函数留作**旧口径对照**
/// 单测仍钉着它（"第一次超过宽限就开火"那条语义没变，只是不再被 spawn 调用）。
#[allow(dead_code)]
pub fn should_fire(now_ms: i64, due_ms: i64, grace_ms: i64) -> Option<i64> {
    if due_ms == i64::MAX {
        return None; // 合法长睡（park）
    }
    let late = now_ms.saturating_sub(due_ms);
    if late > grace_ms {
        Some(late)
    } else {
        None
    }
}

/// **开火之前**的预停摆告警判据（纯函数，好测）。`warn_ms <= 0` = 关（默认）⇒ 恒 `None`。
/// 与 `should_fire` 的区别只有一个：这里**不改行为**（不退出），只是提前把"要卡了"打出来，
/// 让 `journalctl` 里有更早的时间线。默认关 ⇒ 上屏路径零影响。
pub fn should_warn(now_ms: i64, due_ms: i64, warn_ms: i64) -> Option<i64> {
    if warn_ms <= 0 || due_ms == i64::MAX {
        return None;
    }
    let late = now_ms.saturating_sub(due_ms);
    if late > warn_ms {
        Some(late)
    } else {
        None
    }
}

/// 从 `/proc/<pid>/task/<tid>/stat` 的一行里取 `(comm, state)`。纯函数（好测）。
/// 格式：`<pid> (<comm>) <state> ...`。`comm` 可以含空格/括号 ⇒ 用**头一个 `(` 配尾一个 `)`**
/// （comm 之后只剩数字字段与单字符 state，行内不会再出现 `)`）。
pub fn parse_task_stat(line: &str) -> Option<(String, char)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = line.get(open + 1..close)?.to_string();
    let state = line.get(close + 1..)?.trim_start().chars().next()?;
    Some((comm, state))
}

/// 采样本进程**所有线程**的内核态（`state` / `wchan` / `syscall`）→ 多行文本。
/// 这是"再卡死时一眼定位"的**不需要 gdb、不需要 root** 的取证手段：主线程若卡在 Wayland 的
/// `poll`，会看到 `state=S wchan=poll_schedule_timeout syscall=271(ppoll)`。`syscall` 只报号
/// （`/proc/<tid>/syscall` 第一列）；`wchan` 给出内核里等在哪。
pub fn dump_all_threads(tag: &str) -> String {
    let mut out = String::new();
    let (ph, in_ph) = phase_snapshot();
    let head = format!("[看门狗·阶段] {ph}（主循环已停在该阶段 {in_ph}ms）\n");
    out.push_str(&head);
    out.push_str(&format!("[看门狗·线程现场] {tag}\n"));
    let Ok(rd) = std::fs::read_dir("/proc/self/task") else {
        out.push_str("  （读不到 /proc/self/task）\n");
        return out;
    };
    let mut tids: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    tids.sort();
    for tid in tids {
        let stat = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).unwrap_or_default();
        let (comm, state) = parse_task_stat(&stat).unwrap_or_else(|| ("?".into(), '?'));
        let wchan = std::fs::read_to_string(format!("/proc/self/task/{tid}/wchan"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let syscall = std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall"))
            .unwrap_or_default();
        let nr = syscall.split_whitespace().next().unwrap_or("?");
        out.push_str(&format!(
            "  tid={tid:<7} state={state} comm={comm:<16} wchan={wchan} syscall_nr={nr}\n"
        ));
    }
    out
}

#[inline]
fn now_ms() -> i64 {
    EPOCH.get().map(|t| t.elapsed().as_millis() as i64).unwrap_or(0)
}

/// 一次巡检的判定（`judge_stall` 是纯函数：判定逻辑与"退出"这个副作用分开，好测）。
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 没违约（含 `due == i64::MAX` 的合法长睡）。
    Ok,
    /// 违约但**仍在可恢复范围**：只记日志，不退出（落后毫秒数）。
    Tolerate(i64),
    /// 认定死锁：落后毫秒数 + 原因，调用方退出。
    Fire(i64, &'static str),
}

/// 停摆判定（纯函数）。
/// 规则（按顺序）：
///
/// 1. `due == i64::MAX` ⇒ `Ok`（合法长睡，永不介入）；
///
/// 2. 落后 ≤ 宽限 ⇒ `Ok`（正常，含 apply 重建这类慢操作）；
///
/// 3. 落后 > 硬上限 ⇒ `Fire`（**不**看次数：单个几十秒的阻塞就是死了）；
///
/// 4. 本窗口内已容错 `max_transient` 次 ⇒ `Fire`（反复抖动 = 连接真坏了）；
///
/// 5. 否则 ⇒ `Tolerate`（慢，但还没死）。
/// `max_transient = 0` 精确复刻旧行为：落后一超过宽限就 `Fire`。
pub fn judge_stall(
    now_ms: i64,
    due_ms: i64,
    grace_ms: i64,
    hard_ms: i64,
    stalls: u32,
    max_transient: u32,
) -> Verdict {
    if due_ms == i64::MAX {
        return Verdict::Ok;
    }
    let late = now_ms.saturating_sub(due_ms);
    if late <= grace_ms {
        return Verdict::Ok;
    }
    if late > hard_ms {
        return Verdict::Fire(late, "超过硬上限");
    }
    if stalls >= max_transient {
        if max_transient == 0 {
            return Verdict::Fire(late, "容错已关（第一次超过宽限）");
        }
        return Verdict::Fire(late, "窗口内容错次数用尽");
    }
    Verdict::Tolerate(late)
}

/// 启动巡检线程（幂等：重复调用只起一个）。`serve` 模式在进主循环前调一次。
/// 诊断旋钮（**默认全关** ⇒ 与没有它们时行为逐字节一致）：
/// `WE_WATCHDOG_WARN_MS=<ms>`：落后超过该值但**还没到** `GRACE` 时打一行告警（每秒至多一行），
/// **只告警、不退出**。用于提前看到"要卡了"。
/// `WE_WATCHDOG_DUMP=1`：告警/开火时把全进程线程的内核态（`state/wchan/syscall`）打到 stderr，
/// 让 journal 里直接留下"主线程卡在哪"，将来再卡不用 gdb 就能定位。
pub fn spawn() {
    EPOCH.get_or_init(Instant::now);
    let warn_ms: i64 = std::env::var("WE_WATCHDOG_WARN_MS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let dump: bool = std::env::var("WE_WATCHDOG_DUMP")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    // `WE_WATCHDOG_TOLERATE=0` 精确回旧行为（第一次超过宽限就退出）。
    let tolerate: bool = std::env::var("WE_WATCHDOG_TOLERATE")
        .map(|v| !matches!(v.trim(), "0" | "false" | "off" | "no"))
        .unwrap_or(true);
    let max_transient: u32 = if tolerate { MAX_TRANSIENT } else { 0 };
    let grace_ms = GRACE.as_millis() as i64;
    // 硬上限：容错不等于无限等；超过它就认定"不是慢，是死"。非法值（<= 宽限）退回常量。
    let hard_ms = std::env::var("WE_WATCHDOG_HARD_MS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > grace_ms)
        .unwrap_or(HARD_CAP.as_millis() as i64);
    let window_ms = STALL_WINDOW.as_millis() as i64;
    std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            let mut last_warn_s: i64 = -1;
            let mut last_tol_s: i64 = -1;
            // 本窗口内已经容错过几次 / 窗口起点 / 当前是否正处在一个停摆段里
            let mut stalls: u32 = 0;
            let mut window_start: Option<i64> = None;
            let mut in_episode = false;
            loop {
                std::thread::sleep(POLL_EVERY);
                let due = DUE_MS.load(Ordering::Relaxed);
                let now = now_ms();
                match judge_stall(now, due, grace_ms, hard_ms, stalls, max_transient) {
                    Verdict::Fire(late, why) => {
                        if FIRED.swap(1, Ordering::Relaxed) == 0 {
                            let (ph, ph_ms) = phase_snapshot();
                            let mut e = std::io::stderr();
                            let _ = writeln!(
                                e,
                                "[看门狗] 主循环停摆 {:.1}s（排定 {}ms 后回来，宽限 {}s；{}；阶段={} 已在该阶段 {}ms）\
                                 ⇒ 退出({}) 让 systemd 拉起。⚠ 动画时钟会归零重播",
                                late as f64 / 1000.0,
                                due,
                                GRACE.as_secs(),
                                why,
                                ph,
                                ph_ms,
                                EXIT_CODE
                            );
                            // 圈没走完就要 exit ⇒ slowloop 的分段永远等不到，这里补打一次
                            if let Some(line) = crate::slowloop::partial() {
                                let _ = writeln!(e, "{line}");
                            }
                            if dump {
                                let _ = write!(e, "{}", dump_all_threads("开火现场"));
                            }
                            let _ = e.flush();
                            std::process::exit(EXIT_CODE);
                        }
                    }
                    Verdict::Tolerate(late) => {
                        let mut e = std::io::stderr();
                        if !in_episode {
                            in_episode = true;
                            if window_start.map_or(true, |ws| now - ws > window_ms) {
                                window_start = Some(now);
                                stalls = 0;
                            }
                            // ⚠ 计数在**这一段停摆结束时**才 +1（见 Ok 分支），否则同一次停摆
                            // 会在下一次巡检里把自己算成"又一次"，第二次 8 s 就被误开火（本次踩到）。
                            last_tol_s = late / 1000;
                            let (ph, ph_ms) = phase_snapshot();
                            let _ = writeln!(
                                e,
                                "[看门狗·容错] 主循环已落后 {:.1}s（本次第 {}/{} 次；阶段={} 已在该阶段 {}ms）\
                                 ⇒ **不退出**，等它自己回来；硬上限 {}s",
                                late as f64 / 1000.0,
                                stalls + 1,
                                max_transient.max(1),
                                ph,
                                ph_ms,
                                hard_ms / 1000
                            );
                            if dump {
                                let _ = write!(e, "{}", dump_all_threads("容错现场"));
                            }
                            let _ = e.flush();
                        } else {
                            // 同一段停摆里每秒一行，别把 journal 刷爆
                            let s = late / 1000;
                            if s > last_tol_s {
                                last_tol_s = s;
                                let _ = writeln!(
                                    e,
                                    "[看门狗·容错] 仍落后 {:.1}s（等它回来，硬上限 {}s）",
                                    late as f64 / 1000.0,
                                    hard_ms / 1000
                                );
                                let _ = e.flush();
                            }
                        }
                    }
                    Verdict::Ok => {
                        // 主循环回来了：收尾本段、按窗口老化计数
                        if in_episode {
                            // 这一段停摆回来了 ⇒ 记一次"已容错"，再按窗口老化
                            in_episode = false;
                            last_tol_s = -1;
                            stalls += 1;
                        }
                        if let Some(ws) = window_start {
                            if now - ws > window_ms {
                                stalls = 0;
                                window_start = None;
                            }
                        }
                        if let Some(late) = should_warn(now, due, warn_ms) {
                            let s = late / 1000;
                            if s > last_warn_s {
                                last_warn_s = s;
                                let mut e = std::io::stderr();
                                let _ = writeln!(
                                    e,
                                    "[看门狗·告警] 主循环已落后 {:.1}s（阈值 {}ms；**仅告警，不改行为**）",
                                    late as f64 / 1000.0,
                                    warn_ms
                                );
                                if dump {
                                    let _ = write!(e, "{}", dump_all_threads("告警现场"));
                                }
                                let _ = e.flush();
                            }
                        }
                    }
                }
            }
        })
        .expect("起不了看门狗线程");
}

/// 主循环下一圈（或下一帧）预计什么时候回来。`d` = 它自己选定的 poll 超时。
pub fn due_in(d: Duration) {
    EPOCH.get_or_init(Instant::now);
    DUE_MS.store(now_ms() + d.as_millis() as i64, Ordering::Relaxed);
}

/// "我这一整段合法的长睡里别管我"（被遮挡睡眠 / 用户暂停 / 没有源 / 永久阻塞等事件）。
pub fn park() {
    DUE_MS.store(i64::MAX, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_marker_tracks_entry_and_elapsed() {
        set_phase(PHASE_POLL);
        let (n1, _) = phase_snapshot();
        assert_eq!(n1, "poll");
        std::thread::sleep(Duration::from_millis(60));
        let (n2, in2) = phase_snapshot();
        assert_eq!(n2, "poll");
        assert!(in2 >= 50, "同阶段不该重置进入时刻，实测 {in2}ms");
        set_phase(PHASE_SCENE);
        let (n3, in3) = phase_snapshot();
        assert_eq!(n3, "scene");
        assert!(in3 < 50, "换阶段应重置进入时刻，实测 {in3}ms");
        // 直接把原子写成越界值（模拟将来的新常量没登记名字）⇒ 退回 "boot"，不 panic
        PHASE.store(99, Ordering::Relaxed);
        let (n, _) = phase_snapshot();
        assert_eq!(n, "boot");
        PHASE.store(0, Ordering::Relaxed);
        set_phase(0);
    }

    #[test]
    fn parked_never_fires() {
        assert_eq!(should_fire(1_000_000, i64::MAX, 8000), None);
    }

    #[test]
    fn before_due_does_not_fire() {
        assert_eq!(should_fire(1_000, 5_000, 8000), None, "还没到点");
    }

    #[test]
    fn inside_grace_does_not_fire() {
        // due 已过 7.9s，宽限 8s ⇒ 还不动手（apply 重建渲染器这类合法慢操作在这条线上）
        assert_eq!(should_fire(7_900, 0, 8000), None);
    }

    #[test]
    fn past_grace_fires_with_lateness() {
        // due 过了 12s ⇒ 开火，并报出落后 12000ms
        assert_eq!(should_fire(12_000, 0, 8000), Some(12_000));
        // 边界：正好等于宽限不算，多 1ms 才算
        assert_eq!(should_fire(8_000, 0, 8000), None);
        assert_eq!(should_fire(8_001, 0, 8000), Some(8_001));
    }

    #[test]
    fn stall_scenario_matches_the_real_incident() {
        // 复刻现场：主循环排定 1000ms 后回来，然后卡在 eglSwapBuffers 里出不来。
        let due = 1_000;
        assert_eq!(should_fire(1_500, due, 8000), None, "半秒后：正常");
        assert_eq!(should_fire(3_000, due, 8000), None, "2 秒后：还在容错");
        assert_eq!(should_fire(9_001, due, 8000), Some(8_001), "9 秒后：判定停摆");
    }

    #[test]
    fn warn_is_off_by_default() {
        // 不设 WE_WATCHDOG_WARN_MS（= 0）⇒ 恒不告警（上屏路径零行为变化）
        assert_eq!(should_warn(1_000_000, 0, 0), None);
        assert_eq!(should_warn(1_000_000, 0, -5), None);
    }

    #[test]
    fn warn_fires_before_grace_and_never_on_park() {
        // 阈值 2000ms：落后 1.5s 不报，2.5s 报（此时离 GRACE(8s) 还早 ⇒ 提前看到"要卡了"）
        assert_eq!(should_warn(1_500, 0, 2000), None);
        assert_eq!(should_warn(2_500, 0, 2000), Some(2_500));
        // 合法长睡（park，due=i64::MAX）时绝不告警，否则睡眠会被刷屏
        assert_eq!(should_warn(9_999_999, i64::MAX, 2000), None);
        // 边界：正好等于阈值不算，多 1ms 才算
        assert_eq!(should_warn(2_000, 0, 2000), None);
        assert_eq!(should_warn(2_001, 0, 2000), Some(2_001));
    }

    #[test]
    fn parse_task_stat_handles_spaces_parens_and_state() {
        // comm 里有空格和括号（内核允许）⇒ 必须用"首 ( 配尾 )"，state 紧随其后
        let line = "1234 (we-native (worker)) S 1 2 3 4 5 6";
        assert_eq!(parse_task_stat(line), Some(("we-native (worker)".into(), 'S')));
        // 真实内核行（截断版）
        let line = "42 (watchdog) R 1 42 42 0 -1 4194560";
        assert_eq!(parse_task_stat(line), Some(("watchdog".into(), 'R')));
        // 坏输入不给 panic
        assert_eq!(parse_task_stat("没有括号"), None);
    }

    #[test]
    fn dump_all_threads_reports_this_threads_state() {
        // 真读 /proc/self/task：至少应含我们自己这一行、且带 state / wchan / syscall_nr 三件套
        let d = dump_all_threads("单测");
        assert!(d.contains("线程现场"), "缺少表头：{d}");
        assert!(d.contains("state=") && d.contains("wchan=") && d.contains("syscall_nr="), "{d}");
    }
    #[test]
    fn judge_tolerates_transient_stall_inside_hard_cap() {
        // 现场 8.4 s / 9.0 s 的单圈阻塞：落在（宽限 8 s, 硬上限 30 s] ⇒ 只容错
        assert_eq!(judge_stall(9_000, 0, 8000, 30_000, 0, 2), Verdict::Tolerate(9_000));
        assert_eq!(judge_stall(12_000, 0, 8000, 30_000, 1, 2), Verdict::Tolerate(12_000));
        // 边界：正好等于宽限仍算正常
        assert_eq!(judge_stall(8_000, 0, 8000, 30_000, 0, 2), Verdict::Ok);
    }

    #[test]
    fn judge_fires_on_hard_cap_not_before() {
        assert_eq!(judge_stall(30_000, 0, 8000, 30_000, 0, 2), Verdict::Tolerate(30_000));
        assert_eq!(
            judge_stall(30_001, 0, 8000, 30_000, 0, 2),
            Verdict::Fire(30_001, "超过硬上限")
        );
    }

    #[test]
    fn judge_fires_when_budget_used_up() {
        // 已容错 2 次 ⇒ 第 3 次（stalls=2）直接开火，哪怕只落后 9 s
        assert_eq!(
            judge_stall(9_000, 0, 8000, 30_000, 2, 2),
            Verdict::Fire(9_000, "窗口内容错次数用尽")
        );
    }

    #[test]
    fn judge_zero_budget_reproduces_old_behaviour() {
        // WE_WATCHDOG_TOLERATE=0 ⇒ max_transient=0 ⇒ 超过宽限 1 ms 就开火（旧口径）
        assert_eq!(
            judge_stall(8_001, 0, 8000, 30_000, 0, 0),
            Verdict::Fire(8_001, "容错已关（第一次超过宽限）")
        );
        assert_eq!(judge_stall(8_000, 0, 8000, 30_000, 0, 0), Verdict::Ok);
        // 合法长睡时任何次数/上限都不开火
        assert_eq!(judge_stall(1_000_000, i64::MAX, 8000, 30_000, 9, 2), Verdict::Ok);
    }

}
