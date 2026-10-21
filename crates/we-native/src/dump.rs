//! 帧转储（`we-ctl dump`）的**主循环外编码**。
//!
//! ## 为什么要有这个模块
//! 转储要做两件事：① 再画一帧并从 GPU 回读（必须留在主线程：GL 上下文绑在这条线程上）；
//! ② 把 RGBA 缓冲压成 PNG 写盘（`image::save_buffer`，2560x1440 实测 ~180ms 纯 CPU）。
//! 原来 ①② 都在主循环里 ⇒ 每转储一帧主循环就多转 ~200ms，
//! 40 帧 = 主循环连续 ~9s 每圈 220ms（vs 正常 16.7ms）。
//! 看门狗的判据是"主循环有没有在排定的时刻回到 poll"，一次循环里塞进几百毫秒的编码
//! 就是**假停摆**，取证脚本因此能把内核拖进 `exit(70)`。
//! 现在 ② 移到本模块的 worker 线程，主循环只付 ① 的钱。为了避免"主循环产得比编码快"
//! 把内存堆爆，缓冲**不新建**：池里固定 `slots` 块，worker 编完才把缓冲还回来；
//! 池空了就**跳过这一帧**（`skipped += 1`），既不阻塞也不排队。
//!
//! ## 语义变化（如实记）
//! 单条 `dump` 命令**最多 10 帧**（`MAX_FRAMES`）：取证从不需要更多，
//! 而 40 帧会把"再画一帧 + 回读"的 GPU 开销也累积成秒级。
//! 帧与帧之间不再是一定相邻的 1 帧（编码器忙时中间那些帧被跳过）⇒
//! 旁注里的 `scene_t` 仍然是每帧真实时刻，对时照旧按 `scene_t` 匹配。
//! 旁注行由 **worker** 在 PNG 写成功之后写（所以"有行 = 有图"），主循环不再碰文件。

use std::io::Write;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 单条 `dump` 命令允许的最大帧数。
pub const MAX_FRAMES: u32 = 10;
/// 缓冲池块数：1 块给主循环正在画，1 块给 worker 正在编码。
pub const SLOTS: usize = 2;

/// 请求帧数 → 实际帧数（至少 1，至多 `MAX_FRAMES`）。纯函数（好测）。
pub fn cap_count(requested: u32) -> u32 {
    requested.clamp(1, MAX_FRAMES)
}

/// BGRA to RGBA, in place, one linear pass (the wl_shm path hands us BGRA).
pub fn swap_rb(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
}

/// 一帧的编码任务：缓冲的所有权交给 worker，编完再从 `idle` 还回来。
struct Job {
    path: String,
    buf: Vec<u8>,
    w: u32,
    h: u32,
    idx: u32,
    /// 旁注那一行（含 `scene_t` / 指针），PNG 写成功后才落盘
    line: String,
    /// 主循环给的是 BGRA（wl_shm 那条路）⇒ 让 worker 自己换 R/B（不占主循环）
    swap_rb: bool,
    log: Arc<Mutex<std::fs::File>>,
}

/// 主循环这一侧的句柄：投任务、取空缓冲。
pub struct DumpEncoder {
    tx: Sender<Job>,
    idle: Receiver<Vec<u8>>,
    /// 还缓冲用（worker 的缓冲池入口）
    idle_tx: Sender<Vec<u8>>,
    pub sent: u32,
    pub skipped: u32,
}

impl DumpEncoder {
    /// 起 worker 并预填 `slots` 块缓冲（`slot_len` = 一帧 RGBA 字节数）。
///
    /// 分配发生在**控制面命令处理**期间（那一段看门狗临时臂 60s），不在帧循环里。
    pub fn new(slot_len: usize) -> Self {
        let (tx, job_rx) = channel::<Job>();
        let (idle_tx, idle) = channel::<Vec<u8>>();
        for _ in 0..SLOTS {
            let _ = idle_tx.send(vec![0u8; slot_len]);
        }
        let idle_tx_worker = idle_tx.clone();
        std::thread::Builder::new()
            .name("dump-encode".into())
            .spawn(move || {
                while let Ok(mut job) = job_rx.recv() {
                    let t0 = Instant::now();
                    if job.swap_rb {
                        swap_rb(&mut job.buf);
                    }
                    let res = image::save_buffer(
                        &job.path,
                        &job.buf,
                        job.w,
                        job.h,
                        image::ColorType::Rgba8,
                    );
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    match res {
                        Ok(()) => {
                            // 先落盘 PNG，再写旁注行 ⇒ 有行必有图
                            if let Ok(mut f) = job.log.lock() {
                                let _ = f.write_all(job.line.as_bytes());
                                let _ = f.flush();
                            }
                            eprintln!(
                                "[转储] 第 {} 帧写盘完成（编码 {:.0} ms，worker）⇒ {}",
                                job.idx, ms, job.path
                            );
                        }
                        Err(e) => {
                            eprintln!("[转储] 写 {} 失败：{e}（这一帧没有旁注行）", job.path);
                        }
                    }
                    let _ = idle_tx_worker.send(job.buf);
                }
            })
            .expect("起不了转储编码线程");
        Self { tx, idle, idle_tx, sent: 0, skipped: 0 }
    }

    /// 取一块 ≥ `need` 字节的空缓冲；池空 ⇒ `None`（调用方跳过本帧，**不许**自己分配）。
    pub fn take_buf(&mut self, need: usize) -> Option<Vec<u8>> {
        match self.idle.try_recv() {
            Ok(mut b) => {
                if b.len() < need {
                    b.resize(need, 0);
                }
                Some(b)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => None,
        }
    }

    /// 把没用上的缓冲还回池里（渲染失败/提交失败时用，别让池枯掉）。
    pub fn give_back(&mut self, b: Vec<u8>) {
        let _ = self.idle_tx.send(b);
    }

    /// 投一帧给 worker（不阻塞）。返回 false = worker 已死/通道断开。
    #[allow(clippy::too_many_arguments)]
    pub fn submit(&mut self, job_path: String, buf: Vec<u8>, w: u32, h: u32, idx: u32, line: String, swap_rb: bool, log: Arc<Mutex<std::fs::File>>) -> bool {
        let ok = self
            .tx
            .send(Job { path: job_path, buf, w, h, idx, line, swap_rb, log })
            .is_ok();
        if ok {
            self.sent += 1;
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_rb_is_exact_and_leaves_alpha() {
        let mut b = vec![10u8, 20, 30, 200, 0, 0, 0, 255];
        swap_rb(&mut b);
        assert_eq!(b, vec![30u8, 20, 10, 200, 0, 0, 0, 255]);
        swap_rb(&mut b);
        assert_eq!(b, vec![10u8, 20, 30, 200, 0, 0, 0, 255]);
    }

    #[test]
    fn cap_count_clamps_both_ends() {
        assert_eq!(cap_count(0), 1);
        assert_eq!(cap_count(1), 1);
        assert_eq!(cap_count(10), 10);
        assert_eq!(cap_count(11), 10);
        assert_eq!(cap_count(40), 10);
        assert_eq!(cap_count(u32::MAX), MAX_FRAMES);
    }

    /// worker 真的写出 PNG、旁注、并把缓冲还回池里（不碰 GL、不碰屏幕）。
    #[test]
    fn worker_writes_png_and_returns_buffer() {
        let dir = std::env::temp_dir().join(format!("we-dump-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let png = dir.join("t_0000.png");
        let txt = dir.join("t.txt");
        let f = std::fs::File::create(&txt).unwrap();
        let log = Arc::new(Mutex::new(f));

        let mut enc = DumpEncoder::new(4 * 4 * 4);
        let mut buf = enc.take_buf(4 * 4 * 4).expect("池里应该有一块");
        // 4x4 纯红（RGBA）
        for px in buf.chunks_mut(4) {
            px.copy_from_slice(&[255, 0, 0, 255]);
        }
        assert!(enc.submit(
            png.to_string_lossy().into_owned(),
            buf,
            4,
            4,
            0,
            "0\t1.000\t-\tnone\t0\n".into(),
            false,
            log.clone()
        ));
        // 池里还有一块 **预先填好的** 空缓冲：先把它也取走，否则下面的循环会
        // 立刻拿到它、在 worker 还没编完时就检查文件（这个测试最初就是这么假通过的）。
        let spare = enc.take_buf(4 * 4 * 4).expect("池里第二块");
        // 等 worker 把**用过的那块**还回来（最多 5s）⇒ 此时 PNG 一定写完
        let t0 = Instant::now();
        let mut back = None;
        while t0.elapsed().as_secs() < 5 {
            if let Some(b) = enc.take_buf(4 * 4 * 4) {
                back = Some(b);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(back.is_some(), "worker 没把缓冲还回来");
        drop(spare);
        let meta = std::fs::metadata(&png).expect("PNG 没写出来");
        assert!(meta.len() > 0);
        let side = std::fs::read_to_string(&txt).unwrap();
        assert!(side.contains("0\t1.000"), "旁注行没写：{side:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pool_backpressure_skips_instead_of_allocating() {
        // 池只有 SLOTS 块：全取走后必须返回 None（调用方跳过），不许悄悄分配
        let mut enc = DumpEncoder::new(16);
        let mut got = 0;
        while enc.take_buf(16).is_some() {
            got += 1;
            if got > SLOTS + 2 {
                break;
            }
        }
        assert_eq!(got, SLOTS);
        assert!(enc.take_buf(16).is_none());
    }
}
