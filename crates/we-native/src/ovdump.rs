//! **合成后取帧**：`ext_image_copy_capture` 的 **output 会话**。
//!
//! ## 为什么必须有这一路
//! 内核已有的 `we-ctl dump` 拿的是**壁纸自己再渲一遍**的帧（`render_window_with_capture`）。
//! 那只能证明"渲染器算出了什么"，证明不了"屏上到底是什么" —— 用户报的撕裂/重影恰恰在
//! 呈现一侧（帧内容已经被 逐字节证明是纯函数）。所以这里再加一个 **output 会话**
//! 让合成器把**合成之后**的输出缓冲拷给我们。
//! 这就是"屏上对不对"的唯一硬手段：
//! · 不再需要用户拍照；
//! · 每一帧都是合成器的真实输出（含它自己的缩放、层叠、扫描时序）；
//! · 连拍 N 帧 ⇒ 逐行位移场（`scripts/row_displacement.py`）可以量出**单行阶跃**有没有超过 1px。
//!
//! ## 实现要点（踩过的坑都写在这）
//!
//! 1. **必须有自己的 user data 类型**（`OvSession` / `OvFrame`）：光标会话用的是
//! `Dispatch<…SessionV1, >`，同一个接口不能再挂一份 `` —— 事件会串到光标那条路上
//! （`buffer_size` 被写成指针归一化基准，指针坐标当场错掉）。
//!
//! 2. **一次只允许一个 frame 对象**（协议：`:duplicate_frame`）⇒ `inflight` 是 `Option`。
//!
//! 3. **缓冲从 `SlotPool` 拿槽位、用 `raw_data_mut` 读**：抓帧缓冲**永远不会**收到
//! `wl_buffer.release`（协议明说 "The wl_buffer.release request is unused"），
//! SCTK 的 `canvas` 会因为 `active_buffers != 0` 永远返回 `None`。
//!
//! 4. **原始 RGBA 直接落盘**（`.raw` + 旁注 `.txt`），不在内核里压 PNG：
//! 2560x1440 压一张 ~180 ms，30 帧就是 5 秒的主循环停顿（见 `dump.rs` 的历史教训）。
//! 写盘在独立 worker 线程，主循环只做一次 memcpy。
//!
//! 5. **超时兜底**：协议允许合成器"等内容变化"才拷 ⇒ 画面静止时回调可能一直不来。
//! 3 秒没回来就放弃这一帧并停止（日志说清楚），绝不把主循环钉死。

use std::fs::File;
use std::io::Write;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use smithay_client_toolkit::shm::slot::{Buffer, Slot};
use wayland_client::protocol::wl_shm;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1 as cap_source;
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as cap_frame, ext_image_copy_capture_manager_v1 as cap_mgr,
    ext_image_copy_capture_session_v1 as cap_sess,
};

use crate::Wall;

/// 会话对象的 user data（见模块头 ①）
pub struct OvSession;
/// 帧对象的 user data
pub struct OvFrame;

/// 环里放几块抓帧缓冲。4 块 = 4 × 14.7 MB @2560x1440，够 worker 追一轮写盘。
const RING: usize = 4;
/// 一帧最多等多久（ms）。超时 = 放弃这一帧（见模块头 ⑤）。
const FRAME_TIMEOUT_MS: u128 = 3000;

struct Inflight {
    frame: cap_frame::ExtImageCopyCaptureFrameV1,
    slot: usize,
    /// 必须持有：Drop 时才会把 slot 的引用计数还回去
    _buf: Buffer,
    since: Instant,
}

/// 原始帧写盘（worker 线程；主循环只投递）
struct RawSink {
    tx: Sender<(String, String, Arc<Mutex<File>>, Vec<u8>)>,
    idle: Receiver<Vec<u8>>,
}

impl RawSink {
    fn new(slot_len: usize) -> Self {
        let (tx, job_rx) = channel::<(String, String, Arc<Mutex<File>>, Vec<u8>)>();
        let (idle_tx, idle) = channel::<Vec<u8>>();
        for _ in 0..RING {
            let _ = idle_tx.send(vec![0u8; slot_len]);
        }
        let idle_tx_w = idle_tx.clone();
        std::thread::Builder::new()
            .name("ovcap-write".into())
            .spawn(move || {
                while let Ok((path, line, log, data)) = job_rx.recv() {
                    match std::fs::File::create(&path) {
                        Ok(mut f) => {
                            if let Err(e) = f.write_all(&data) {
                                eprintln!("[合成抓帧] 写 {path} 失败：{e}");
                            } else if let Ok(mut l) = log.lock() {
                                let _ = l.write_all(line.as_bytes());
                                let _ = l.flush();
                            }
                        }
                        Err(e) => eprintln!("[合成抓帧] 建 {path} 失败：{e}"),
                    }
                    let _ = idle_tx_w.send(data);
                }
            })
            .expect("起不了合成抓帧的写盘线程");
        Self { tx, idle }
    }

    fn take(&mut self, need: usize) -> Option<Vec<u8>> {
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

    fn submit(&mut self, path: String, line: String, log: Arc<Mutex<File>>, data: Vec<u8>) {
        let _ = self.tx.send((path, line, log, data));
    }
}

pub struct OvCap {
    /// 会话对象**必须持有**：代理一 Drop，合成器就销毁会话（`stopped`）。
    session: Option<cap_sess::ExtImageCopyCaptureSessionV1>,
    /// 捕获源同样必须持有（会话的父对象）。
    source: Option<cap_source::ExtImageCaptureSourceV1>,
    /// 收到 `done`（约束齐了）之后才允许 create_frame
    ready: bool,
    /// 会话报的缓冲尺寸（= 输出缓冲像素，2560x1440）
    w: i32,
    h: i32,
    fmt: Option<wl_shm::Format>,
    slots: Vec<Slot>,
    free: Vec<usize>,
    inflight: Option<Inflight>,
    prefix: String,
    left: u32,
    n: u32,
    fails: u32,
    log: Option<Arc<Mutex<File>>>,
    sink: Option<RawSink>,
    /// 合成器报的 **presentation_time**（system monotonic，ns）—— 这就是"第几帧"的硬时间戳
    last_ts_ns: u64,
    /// 所有抓到的帧的时间戳（验收用：帧间隔分布）
    pub tss: Vec<u64>,
}

impl OvCap {
    fn stride(&self) -> i32 {
        self.w * 4
    }

    fn len(&self) -> usize {
        (self.h as usize) * (self.stride() as usize)
    }

    /// 建会话（每次 `we-ctl dump --output` 都新建一个：新会话的第一帧**必定全量**，
    /// 不受上次"等内容变化"状态影响）。
    fn new(prefix: String, left: u32) -> Self {
        Self {
            session: None,
            source: None,
            ready: false,
            w: 0,
            h: 0,
            fmt: None,
            slots: Vec::new(),
            free: Vec::new(),
            inflight: None,
            prefix,
            left,
            n: 0,
            fails: 0,
            log: None,
            sink: None,
            last_ts_ns: 0,
            tss: Vec::new(),
        }
    }

    /// 约束齐了：开槽位 + 写盘 worker + 旁注文件。
    fn arm(&mut self, wall: &mut Wall) -> Result<(), String> {
        if self.w <= 0 || self.h <= 0 {
            return Err("会话没报 buffer_size".into());
        }
        let fmt = self.fmt.unwrap_or(wl_shm::Format::Xrgb8888);
        let len = self.len();
        for _ in 0..RING {
            let slot = wall
                .pool
                .new_slot(len)
                .map_err(|e| format!("分配抓帧槽位失败：{e}"))?;
            self.free.push(self.slots.len());
            self.slots.push(slot);
        }
        let log_path = format!("{}.txt", self.prefix);
        let mut f = File::create(&log_path).map_err(|e| format!("建 {log_path} 失败：{e}"))?;
        let _ = f.write_all(
            "# idx\tpresentation_ns\tw\th\tformat\tstride\t(合成器合成之后的输出帧；像素按小端 XRGB8888 = 内存序 B,G,R,X)\n"
                .as_bytes(),
        );
        self.log = Some(Arc::new(Mutex::new(f)));
        self.sink = Some(RawSink::new(len));
        self.ready = true;
        println!(
            "[合成抓帧] 会话就绪：{}x{} 格式 {:?}（环 {} 块，共 {:.1} MB）⇒ 目标 {} 帧，前缀 {}",
            self.w,
            self.h,
            fmt,
            RING,
            (RING * len) as f64 / 1e6,
            self.left,
            self.prefix
        );
        Ok(())
    }

    /// 起下一张抓帧（有在飞的就不动）
    fn kick(&mut self, wall: &mut Wall) {
        if !self.ready || self.left == 0 || self.inflight.is_some() {
            return;
        }
        let Some(qh) = wall.qh.clone() else { return };
        let Some(si) = self.free.first().copied() else {
            return; // 环里没有空槽（写盘 worker 还没还）⇒ 下一轮再试
        };
        self.free.retain(|&i| i != si);
        let fmt = self.fmt.unwrap_or(wl_shm::Format::Xrgb8888);
        let (w, h, stride) = (self.w, self.h, self.stride());
        let buf = match wall
            .pool
            .create_buffer_in(&self.slots[si], w, h, stride, fmt)
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[合成抓帧] 建缓冲失败：{e} ⇒ 停止");
                self.left = 0;
                return;
            }
        };
        let Some(sess) = self.session.as_ref() else { return };
        let frame = sess.create_frame(&qh, OvFrame);
        frame.attach_buffer(buf.wl_buffer());
        // 不跟踪 damage ⇒ 每次全量（协议：首次与"不跟踪"都必须全量）
        frame.damage_buffer(0, 0, w, h);
        frame.capture();
        self.inflight = Some(Inflight {
            frame,
            slot: si,
            _buf: buf,
            since: Instant::now(),
        });
    }

    /// `ready` 事件：把这一帧抄出来 → 交给写盘 worker → 起下一张。
    fn take_ready(&mut self, wall: &mut Wall) {
        let Some(inf) = self.inflight.take() else { return };
        inf.frame.destroy();
        let need = self.len();
        let mut data = match self.sink.as_mut().and_then(|s| s.take(need)) {
            Some(b) => b,
            // worker 落后：宁可多分配一块也不丢帧（"连续 30 帧"必须真的是连续帧）
            None => vec![0u8; need],
        };
        {
            let src = wall.pool.raw_data_mut(&self.slots[inf.slot]);
            if src.len() >= need {
                data[..need].copy_from_slice(&src[..need]);
            }
        }
        let idx = self.n;
        let path = format!("{}_{:04}.raw", self.prefix, idx);
        let fmt = self.fmt.unwrap_or(wl_shm::Format::Xrgb8888);
        let line = format!(
            "{idx}\t{}\t{}\t{}\t{:?}\t{}\n",
            self.last_ts_ns, self.w, self.h, fmt, self.stride()
        );
        match (self.log.clone(), self.sink.as_mut()) {
            (Some(log), Some(sink)) => sink.submit(path.clone(), line, log, data),
            _ => {}
        }
        self.tss.push(self.last_ts_ns);
        self.n += 1;
        self.left -= 1;
        self.free.push(inf.slot);
        if idx == 0 || (idx + 1) % 10 == 0 {
            println!(
                "[合成抓帧] 已抓 {} 帧（presentation_ns {}，剩 {}）⇒ {}",
                idx + 1,
                self.last_ts_ns,
                self.left,
                path
            );
        }
        if self.left == 0 {
            let dt = self.tss.last().copied().unwrap_or(0) as f64 / 1e6
                - self.tss.first().copied().unwrap_or(0) as f64 / 1e6;
            println!(
                "[合成抓帧] 完成：{} 帧 / {:.1} ms（合成器时间戳）⇒ {}_{{:04}}.raw + {}.txt",
                self.n,
                dt,
                self.prefix,
                self.prefix
            );
        }
        self.kick(wall);
    }

    fn note_ts(&mut self, tv_sec_hi: u32, tv_sec_lo: u32, tv_nsec: u32) {
        let sec = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
        self.last_ts_ns = sec.saturating_mul(1_000_000_000) + tv_nsec as u64;
    }

    /// 一句话状态（status 里回报）
    pub fn status(&self) -> String {
        format!(
            "{}x{} {:?} 已抓 {} 帧（剩 {}，环 {} 空闲 {}）{}",
            self.w,
            self.h,
            self.fmt,
            self.n,
            self.left,
            RING,
            self.free.len(),
            if self.ready { "" } else { "未就绪" }
        )
    }
}

/// 开始一次"合成后取帧"。返回实际帧数（夹到 1..=600：一次 600 帧 = 8.8 GB，
/// 那是取证上限而不是常用值）。
pub fn start(wall: &mut Wall, prefix: String, count: u32) -> Result<u32, String> {
    let n = count.clamp(1, 600);
    let (Some(src_mgr), Some(mgr)) = (wall.cap_src_manager.clone(), wall.cap_manager.clone())
    else {
        return Err("合成器没给 ext_image_copy_capture_manager_v1（这条协议不可用）".into());
    };
    let Some(qh) = wall.qh.clone() else {
        return Err("还没有队列句柄".into());
    };
    let Some(output) = wall.output_state.outputs().next() else {
        return Err("还没有输出（合成器没报 wl_output）".into());
    };
    // 关掉上次（同一个会话不能并发两轮）
    if let Some(mut old) = wall.ov.take() {
        if let Some(inf) = old.inflight.take() {
            inf.frame.destroy();
        }
        if let Some(sess) = old.session.take() {
            sess.destroy();
        }
        if let Some(src) = old.source.take() {
            src.destroy();
        }
    }
    let source = src_mgr.create_source(&output, &qh, ());
    let session = mgr.create_session(&source, cap_mgr::Options::empty(), &qh, OvSession);
    let mut cap = OvCap::new(prefix.clone(), n);
    cap.session = Some(session);
    cap.source = Some(source);
    wall.ov = Some(cap);
    println!("[合成抓帧] 已请求 output 会话（源 = 第一块输出）：{prefix} × {n} 帧");
    Ok(n)
}

/// 每轮主循环调一次（很便宜：只看有没有超时 / 该不该起下一张）。
pub fn tick(wall: &mut Wall) {
    let Some(mut ov) = wall.ov.take() else { return };
    let mut timed_out = false;
    if let Some(inf) = ov.inflight.as_ref() {
        if inf.since.elapsed().as_millis() > FRAME_TIMEOUT_MS {
            timed_out = true;
        }
    }
    if timed_out {
        if let Some(inf) = ov.inflight.take() {
            inf.frame.destroy();
            ov.free.push(inf.slot);
        }
        eprintln!(
            "[合成抓帧] 一帧超过 {} ms 没回来（合成器在等内容变化？）⇒ 放弃并停止（已抓 {} 帧）",
            FRAME_TIMEOUT_MS, ov.n
        );
        ov.left = 0;
    }
    ov.kick(wall);
    wall.ov = Some(ov);
}

impl Dispatch<cap_sess::ExtImageCopyCaptureSessionV1, OvSession> for Wall {
    fn event(
        state: &mut Self,
        _s: &cap_sess::ExtImageCopyCaptureSessionV1,
        e: cap_sess::Event,
        _d: &OvSession,
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
        use cap_sess::Event as E;
        let mut arm = false;
        let mut stop = false;
        if let Some(ov) = state.ov.as_mut() {
            match e {
                E::BufferSize { width, height } => {
                    ov.w = width as i32;
                    ov.h = height as i32;
                }
                E::ShmFormat { format } => {
                    // 优先 XRGB8888 / ARGB8888（我们按 32 位读）；其余先记下，能用就用
                    if let wayland_client::WEnum::Value(f) = format {
                        let prefer =
                            matches!(f, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);
                        if ov.fmt.is_none() || prefer {
                            ov.fmt = Some(f);
                        }
                    }
                }
                E::Done => arm = true,
                E::Stopped => stop = true,
                _ => {}
            }
        }
        if stop {
            eprintln!("[合成抓帧] 合成器停了 output 会话 ⇒ 停止抓帧");
            if let Some(ov) = state.ov.as_mut() {
                ov.left = 0;
                ov.ready = false;
            }
            return;
        }
        if arm {
            let Some(mut ov) = state.ov.take() else { return };
            if !ov.ready {
                if let Err(e) = ov.arm(state) {
                    eprintln!("[合成抓帧] 会话就绪但可用不了：{e}");
                    ov.left = 0;
                }
            }
            ov.kick(state);
            state.ov = Some(ov);
        }
    }
}

impl Dispatch<cap_frame::ExtImageCopyCaptureFrameV1, OvFrame> for Wall {
    fn event(
        state: &mut Self,
        _f: &cap_frame::ExtImageCopyCaptureFrameV1,
        e: cap_frame::Event,
        _d: &OvFrame,
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
        use cap_frame::Event as E;
        match e {
            E::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                if let Some(ov) = state.ov.as_mut() {
                    ov.note_ts(tv_sec_hi, tv_sec_lo, tv_nsec);
                }
            }
            E::Ready => {
                if let Some(mut ov) = state.ov.take() {
                    ov.take_ready(state);
                    state.ov = Some(ov);
                }
            }
            E::Failed { reason } => {
                if let Some(mut ov) = state.ov.take() {
                    ov.fails += 1;
                    if let Some(inf) = ov.inflight.take() {
                        inf.frame.destroy();
                        ov.free.push(inf.slot);
                    }
                    eprintln!(
                        "[合成抓帧] 这一帧失败（reason={reason:?}，第 {} 次，已抓 {} 帧）",
                        ov.fails, ov.n
                    );
                    // 约束变了 / 会话没了 ⇒ 继续试也没意义；unknown 允许重试
                    if ov.fails > 3 {
                        ov.left = 0;
                    } else {
                        ov.kick(state);
                    }
                    state.ov = Some(ov);
                }
            }
            _ => {}
        }
    }
}
