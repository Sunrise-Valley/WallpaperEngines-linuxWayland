use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

fn env_set() -> bool {
    std::env::var("WE_PASS_GPU").is_ok()
}

/// 开关（WE_PASS_GPU 存在即开；与 WE_FRAME_TIMING 互相独立）。
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(env_set)
}

static US: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static HITS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static FRAMES: AtomicU64 = AtomicU64::new(0);

fn bump(v: &Mutex<Vec<u64>>, i: usize, x: u64) {
    let mut g = v.lock().unwrap();
    if g.len() <= i {
        g.resize(i + 1, 0);
    }
    g[i] += x;
}

/// 记一趟的实测微秒（pi = 趟下标）。
pub fn note(pi: usize, us: u64) {
    bump(&US, pi, us);
    bump(&HITS, pi, 1);
}

fn cmp_desc(a: &(u64, usize), b: &(u64, usize)) -> std::cmp::Ordering {
    b.0.cmp(&a.0)
}

/// 每帧末尾调一次；每 30 帧打一张表（累计均值）。
pub fn tick_frame(npasses: usize) {
    let n = FRAMES.fetch_add(1, Relaxed) + 1;
    if n % 30 != 0 {
        return;
    }
    let v = US.lock().unwrap().clone();
    let h = HITS.lock().unwrap().clone();
    let frames = n as f64;
    let mut total: u64 = 0;
    for x in v.iter() {
        total += *x;
    }
    let mut pairs: Vec<(u64, usize)> = Vec::new();
    let m = v.len().min(npasses);
    for i in 0..m {
        pairs.push((v[i], i));
    }
    pairs.sort_unstable_by(cmp_desc);
    eprintln!(
        "[趟GPU] {} 帧均值：{} 趟合计 {:.1} ms/帧（每趟前后 glFinish，串行代价偏高）",
        n, npasses, total as f64 / frames / 1000.0
    );
    for k in 0..pairs.len().min(12) {
        let pi = pairs[k].1;
        let hh = *h.get(pi).unwrap_or(&0) as f64;
        if hh == 0.0 {
            continue;
        }
        eprintln!(
            "[趟GPU]   趟{:>3} {:>7.2} ms/帧  单次 {:>6.2} ms",
            pi,
            pairs[k].0 as f64 / frames / 1000.0,
            pairs[k].0 as f64 / hh / 1000.0
        );
    }
}

// ═══════════════════════ WE_PROFILE_PASSES：非阻塞逐 pass 计时 ═══════════════════════
//
// 与上面的 `WE_PASS_GPU` 是**两条独立**的路：
// · `WE_PASS_GPU` —— 每趟前后 `glFinish`（串行化，单趟时间偏大，但能立刻看到）
// · `WE_PROFILE_PASSES` —— 每趟 `GL_TIME_ELAPSED` 异步查询，6 帧后回读（不夹断流水）
// 用法：`WE_PROFILE_PASSES=1 WE_PROFILE_OUT=passes.tsv we-scene-render …`
// 输出：每 30 个**采样**帧打一张按耗时降序的表 + 每帧全屏 pass 数；同表写进 `WE_PROFILE_OUT`。

/// 开关（`WE_PROFILE_PASSES` 存在即开）。
pub fn prof_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WE_PROFILE_PASSES").is_some())
}

static P_NS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static P_HITS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static P_FRAMES: AtomicU64 = AtomicU64::new(0);
static P_FULLSCREEN: AtomicU64 = AtomicU64::new(0);
static P_META: Mutex<Vec<String>> = Mutex::new(Vec::new());
static P_REPORTED: AtomicU64 = AtomicU64::new(0);
static P_WALL_NS: AtomicU64 = AtomicU64::new(0);

/// 记一张「趟下标 → 人类可读标签」的表（每次报告用它；只认第一张）。
pub fn prof_meta_init(meta: Vec<String>) {
    let mut g = P_META.lock().unwrap();
    if g.is_empty() {
        *g = meta;
    }
}

/// 一帧的采样结果（`(趟下标, 纳秒)`）+ 本帧"全屏趟"个数 + 本帧墙钟（纳秒）。
pub fn prof_frame(samples: &[(usize, u64)], fullscreen: usize, wall_ns: u64) {
    for (pi, ns) in samples {
        bump(&P_NS, *pi, *ns);
        bump(&P_HITS, *pi, 1);
    }
    if !samples.is_empty() {
        P_FRAMES.fetch_add(1, Relaxed);
        P_WALL_NS.fetch_add(wall_ns, Relaxed);
    }
    P_FULLSCREEN.fetch_add(fullscreen as u64, Relaxed);
}

/// 每帧调用；每 30 个**采样**帧打一张表。返回 true = 这一帧刚打过表。
pub fn prof_tick() -> bool {
    let n = P_FRAMES.load(Relaxed);
    if n == 0 || n % 30 != 0 || n == P_REPORTED.load(Relaxed) {
        return false;
    }
    P_REPORTED.store(n, Relaxed);
    let v = P_NS.lock().unwrap().clone();
    let h = P_HITS.lock().unwrap().clone();
    let meta = P_META.lock().unwrap().clone();
    let frames = n as f64;
    let mut total: u64 = 0;
    let mut pairs: Vec<(u64, usize)> = Vec::new();
    for (i, x) in v.iter().enumerate() {
        total += *x;
        if *x > 0 {
            pairs.push((*x, i));
        }
    }
    pairs.sort_unstable_by(cmp_desc);
    let fs = P_FULLSCREEN.load(Relaxed) as f64 / frames;
    let wall = P_WALL_NS.load(Relaxed) as f64 / frames / 1e6;
    eprintln!(
        "[逐趟剖析] {n} 帧：GPU 趟合计 {:.2} ms/帧（每帧 {:.1} 个全屏趟；帧墙钟 {:.2} ms ⇒ 趟占 {:.0}%）",
        total as f64 / frames / 1e6,
        fs,
        wall,
        if wall > 0.0 {
            total as f64 / frames / 1e6 / wall * 100.0
        } else {
            f64::NAN
        }
    );
    eprintln!("[逐趟剖析] 降序（ms/帧 | 单次 µs | 每帧趟数）：");
    let mut tsv = String::new();
    for (rank, (ns, pi)) in pairs.iter().enumerate() {
        let hh = *h.get(*pi).unwrap_or(&0) as f64;
        let per_frame = *ns as f64 / frames / 1e6;
        let each = if hh > 0.0 { *ns as f64 / hh / 1e3 } else { 0.0 };
        let calls = if hh > 0.0 { hh / frames } else { 0.0 };
        let label = meta.get(*pi).cloned().unwrap_or_default();
        if rank < 24 {
            eprintln!(
                "[逐趟剖析] #{:<2} 趟{:<3} {per_frame:>6.2} ms/帧  {each:>7.2} µs  ×{calls:.2}  {label}",
                rank + 1,
                pi
            );
        }
        tsv.push_str(&format!(
            "{}\t{}\t{:.4}\t{:.3}\t{:.2}\t{}\n",
            pi,
            rank + 1,
            per_frame,
            each,
            calls,
            label
        ));
    }
    if let Ok(p) = std::env::var("WE_PROFILE_OUT") {
        if !p.is_empty() {
            let hdr = format!(
                "# frames={n} gpu_total_ms_per_frame={:.3} fullscreen_per_frame={fs:.2}\n#pass\trank\tms_per_frame\tus_each\tcalls_per_frame\tlabel\n",
                total as f64 / frames / 1e6
            );
            let _ = std::fs::write(&p, hdr + &tsv);
        }
    }
    true
}
