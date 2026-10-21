// SPDX-License-Identifier: MIT
//
//! **真实音频频谱 → 粒子音频响应**（16 段能量）。
//!
//! ## 为什么是 16 段
//! WE 的粒子音频响应吃的是**场景音频平均谱**，而它的分辨率就是 16：
//! `open-wallpaper-engine/src/Scene/Pkg/Parse/Particle/ParticleRuntime.cppm:239`
//! `array<float, 16> audio_average`；`Scene.cppm:2468` 的 `SceneAudioAverage` 也是
//! `array<Atomic<f32>, 16>`。所以 `ParticleAudioResponse.frequency`（`audiofrequencystart/end`）
//! **是这 16 段数组的下标**（默认 `[0,15]` = 全段，见 `ParticleRuntime.cppm:44-46`）。
//! 本机数据也对得上：`2481886649` 的三条预设只写 `audioprocessingfrequencyend: 15`。
//!
//! ## 谱是怎么算出来的（照 OWE 的 `AudioResponse` + `SceneAudioResponse` 移植）
//!
//! 1. 48kHz / 立体声 PCM → 每声道 FFT（OWE 用 2089 点 DFT + 1392 点分析窗；我们用 2048 点
//! radix-2 + Hann 窗 —— 段能量口径一致，只是快得多、泄漏更少，见本文件 `kNoteDsp`）。
//!
//! 2. FFT bin（1..1024）→ **64 段**：`coordinate = (bin-1)/(bins-1)`、
//! `band = min(coordinate^0.25 × 64, band+1)`、权重 `sqrt(0.501 - 0.499·cos(π·coordinate))`、
//! 段内取 **max**（OWE `AudioResponse.cpp:52-70`）。
//!
//! 3. 64 段 → 逐声道 16 段（8 段取 max，OWE `SceneAudioResponse.cpp:40-51`），左右取平均。
//!
//! 4. **包络归一化 + 平滑**（OWE `SceneAudioResponse.cpp:117-164`）：16 组各自跟峰、
//! 峰值下限 = 0.333 × 全局峰、归一化后再按 20/s 平滑 + 40/s 限速 ——
//! 这一步是**必须**的：本机三条音频预设用的 `bounds` 是 `"0.5 1"` / `"0.8 1"`，
//! 只有归一化到 ~0..1 的量纲才可能越过它们（原始幅度在 1e-3 量级 ⇒ 永远不动）。
//!
//! ## 开关（**默认看入口**；关着时逐字节恒等）
//!
//! * `WE_AUDIO_SPECTRUM` — 总开关：**显式设定才开**，库默认关（离线 CLI / 调试 bin
//! 也走 autostart，默认开会让每次离线渲染都 spawn parec 抓系统输出，实测 17 条全中。
//! 屏上内核由 unit drop-in 显式设 1（win 语义；17 条里 12 条场景真消费频谱）。
//!
//! * `WE_AUDIO_SOURCE=monitor|own|off` — 采集源，默认 `monitor`（**WE 语义**：抓默认输出的
//! monitor，= linux-WE 的 `PulseAudioPlaybackRecorder`）；`own` = 只吃内核自己喂进来的 PCM
//! （`feed_pcm`，隐私模式，只对**自带音轨**的壁纸有反应）；
//!
//! * `WE_AUDIO_DEVICE=<pulse source>` — 覆盖采集设备（默认 `pactl` 查默认 sink 的 `.monitor`）
//!
//! * `WE_AUDIO_BANDS="b0 .. b15"`（1 个值 ⇒ 16 段全等）— **静态频谱**，离线复现用；设置即开
//!
//! * `WE_PARTICLE_AUDIO=<0..1>` — 上次的标量探针（等价于静态频谱全 16 段同值）
//! 开关关着 ⇒ `Spectrum::any` 全 0 ⇒ `AudioCfg::scale` 恒 1.0、`vortex` 音频算子照旧跳过
//! ⇒ 场景逐字节不变（离线默认就是这一档）。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// 频段数（= WE 的 `audio_average` 长度）
pub const BANDS: usize = 16;
/// 采样率（OWE `kSampleRate`）
pub const SAMPLE_RATE: u32 = 48_000;
/// 声道数（OWE `kChannels`）
pub const CHANNELS: usize = 2;
/// FFT 窗长（样本/声道）
pub const FFT_N: usize = 2048;
/// 每窗前进的样本数（50% 重叠 ⇒ 48k 下 46.9 次/秒）
const HOP: usize = FFT_N / 2;
/// OWE `kResponseBins`
pub const RESPONSE_BINS: usize = 64;
/// 可用的幅度 bin 数（FFT 正频率半边）
const SPECTRUM_BINS: usize = FFT_N / 2;

/// 一次频谱快照。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spectrum {
    /// 16 段能量（已按 OWE 的包络归一化 ⇒ 量纲 ~0..1+）
    pub bands: [f32; BANDS],
    /// 总能量指示（16 段均值），给"节奏/整体强度"用
    pub level: f32,
    /// 第几次分析（调试用；静态频谱固定 0）
    pub generation: u64,
}

impl Default for Spectrum {
    fn default() -> Self {
        Spectrum {
            bands: [0.0; BANDS],
            level: 0.0,
            generation: 0,
        }
    }
}

impl Spectrum {
    /// 有任何一段 > 0 ⇒ "有音频"。**没有音频时所有音频响应必须恒等**，
    /// 全靠这个判断（`AudioCfg::scale` / `vortex` 的音频模式）。
    pub fn any(&self) -> bool {
        self.bands.iter().any(|b| *b > 0.0)
    }

    /// OWE `AudioResponseScale`（`ParticleEmitter.cpp:60-82`）取的那个"段均值"：
    /// `frequency` 是**段下标**（四舍五入后夹到 0..15），闭区间平均。
    pub fn range_mean(&self, freq: [f32; 2]) -> f32 {
        let idx = |v: f32| -> usize {
            let i = v.round() as i64;
            i.clamp(0, BANDS as i64 - 1) as usize
        };
        let (mut first, mut last) = (idx(freq[0]), idx(freq[1]));
        if last < first {
            std::mem::swap(&mut first, &mut last);
        }
        let mut sum = 0.0;
        for i in first..=last {
            sum += self.bands[i].max(0.0);
        }
        sum / (last - first + 1) as f32
    }
}

// ── 全局状态（进程内共享：采集线程写，渲染线程每帧读）────────────────────────────
static GATE: AtomicBool = AtomicBool::new(false);
static STATIC: AtomicBool = AtomicBool::new(false);
static BAND_BITS: [AtomicU32; BANDS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static LEVEL_BITS: AtomicU32 = AtomicU32::new(0);
/// 左右声道各自的 16 段（win 的 `g_AudioSpectrum16Left/Right`）。
const ZERO_BAND: AtomicU32 = AtomicU32::new(0);
static BAND_L_BITS: [AtomicU32; BANDS] = [ZERO_BAND; BANDS];
static BAND_R_BITS: [AtomicU32; BANDS] = [ZERO_BAND; BANDS];
static GENERATION: AtomicU64 = AtomicU64::new(0);
static ANALYZED: AtomicU64 = AtomicU64::new(0);
static CAPTURE_ON: AtomicBool = AtomicBool::new(false);
/// 采集线程是否该退出（`stop_capture` 置位；线程在下次 read 返回/子进程被杀后退出）
static CAPTURE_STOP: AtomicBool = AtomicBool::new(false);
/// 当前 `parec` 的 pid（0 = 没有）——用来在关开关时**立刻**杀掉它，
/// 否则空闲的 monitor 源是 SUSPENDED、`read` 会一直阻塞，线程根本看不到退出标志。
static CAPTURE_PID: AtomicU32 = AtomicU32::new(0);
static CAPTURE_INFO: OnceLock<Mutex<String>> = OnceLock::new();

fn info_cell() -> &'static Mutex<String> {
    CAPTURE_INFO.get_or_init(|| Mutex::new(String::new()))
}

/// 把设备名/状态存起来（内核状态行会打出来）
fn set_info(s: &str) {
    if let Ok(mut g) = info_cell().lock() {
        *g = s.to_string();
    }
}

/// 采集状态一行（`源:设备:分析次数`；静态频谱会标 `static`）
pub fn capture_status() -> String {
    let base = info_cell().lock().map(|g| g.clone()).unwrap_or_default();
    let kind = if STATIC.load(Ordering::Relaxed) { "static" } else { "live" };
    format!(
        "{}({}) analyzed={}",
        base,
        kind,
        ANALYZED.load(Ordering::Relaxed)
    )
}

/// 功能是否开着（`WE_AUDIO_SPECTRUM=1` / 静态频谱 / 运行时显式开）
pub fn enabled() -> bool {
    init_once();
    GATE.load(Ordering::Relaxed)
}

/// 环境变量只读一次（幂等）；任何拿频谱的入口都先过这里，
/// 这样离线探针（`particle_probe`）不需要额外调用就能吃到 `WE_AUDIO_BANDS`。
fn init_once() {
    static INITED: AtomicBool = AtomicBool::new(false);
    if !INITED.swap(true, Ordering::Relaxed) {
        init_from_env();
    }
}

/// 静态频谱（离线复现）是不是当前来源
pub fn is_static() -> bool {
    STATIC.load(Ordering::Relaxed)
}

/// 运行时显式开（内核控制面 `{"cmd":"audio","on":true}`）
pub fn set_enabled(on: bool) {
    GATE.store(on, Ordering::Relaxed);
}
fn publish(s: Spectrum) {
    for (i, b) in s.bands.iter().enumerate() {
        let bits = b.to_bits();
        BAND_BITS[i].store(bits, Ordering::Relaxed);
        // 单声道/静态来源没有左右之分 ⇒ 两个声道数组同值。
        BAND_L_BITS[i].store(bits, Ordering::Relaxed);
        BAND_R_BITS[i].store(bits, Ordering::Relaxed);
    }
    LEVEL_BITS.store(s.level.to_bits(), Ordering::Relaxed);
    GENERATION.store(s.generation, Ordering::Relaxed);
}

/// 写入左右声道各自的 16 段（`g_AudioSpectrum16Left/Right` 的数据源）。
fn publish_channels(l: [f32; BANDS], r: [f32; BANDS]) {
    for i in 0..BANDS {
        BAND_L_BITS[i].store(l[i].to_bits(), Ordering::Relaxed);
        BAND_R_BITS[i].store(r[i].to_bits(), Ordering::Relaxed);
    }
}

/// 当前左右声道频谱。**没开功能时恒为全 0**。
pub fn channels() -> ([f32; BANDS], [f32; BANDS]) {
    let mut l = [0.0f32; BANDS];
    let mut r = [0.0f32; BANDS];
    if !enabled() {
        return (l, r);
    }
    for i in 0..BANDS {
        l[i] = f32::from_bits(BAND_L_BITS[i].load(Ordering::Relaxed));
        r[i] = f32::from_bits(BAND_R_BITS[i].load(Ordering::Relaxed));
    }
    (l, r)
}

/// 当前频谱快照。**没开功能时恒为全 0**（⇒ 所有音频响应恒等）。
pub fn spectrum() -> Spectrum {
    if !enabled() {
        return Spectrum::default();
    }
    let mut s = Spectrum {
        level: f32::from_bits(LEVEL_BITS.load(Ordering::Relaxed)),
        generation: GENERATION.load(Ordering::Relaxed),
        ..Default::default()
    };
    for i in 0..BANDS {
        s.bands[i] = f32::from_bits(BAND_BITS[i].load(Ordering::Relaxed));
    }
    s
}

/// 外部（内核播放管线 / 离线探针 / 控制面）直接写频谱。
pub fn feed(spectrum: Spectrum) {
    STATIC.store(false, Ordering::Relaxed);
    publish(spectrum);
}

/// 静态频谱一次写 16 段（离线复现；设置即开）
pub fn set_static_bands(bands: [f32; BANDS]) {
    GATE.store(true, Ordering::Relaxed);
    STATIC.store(true, Ordering::Relaxed);
    publish(Spectrum {
        bands,
        level: bands.iter().sum::<f32>() / BANDS as f32,
        generation: 0,
    });
}

/// 环境变量解析：总开关 / 静态频谱。
fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// 采集源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 默认输出的 monitor（WE / linux-WE 语义）
    Monitor,
    /// 只吃 `feed_pcm` 喂进来的 PCM（隐私模式：壁纸自己的音频）
    Own,
    /// 不采集
    Off,
}

impl Source {
    pub fn parse(s: &str) -> Source {
        match s.trim().to_ascii_lowercase().as_str() {
            "own" | "self" | "internal" => Source::Own,
            "off" | "none" | "0" => Source::Off,
            _ => Source::Monitor,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Source::Monitor => "monitor",
            Source::Own => "own",
            Source::Off => "off",
        }
    }
}

/// 读环境变量初始化（幂等；在渲染器构造 / 探针启动时各调一次）
pub fn init_from_env() {
    if let Some(v) = env_nonempty("WE_AUDIO_BANDS") {
        let vals: Vec<f32> = v
            .split_whitespace()
            .filter_map(|x| x.parse::<f32>().ok())
            .collect();
        let mut bands = [0.0f32; BANDS];
        match vals.len() {
            0 => {}
            1 => bands = [vals[0].clamp(0.0, 1.0); BANDS],
            n if n < BANDS => {
                // 少给几个 = 后面补最后一个（宽容解析，方便手写）
                let mut last = 0.0;
                for (i, b) in bands.iter_mut().enumerate() {
                    if i < n {
                        last = vals[i].clamp(0.0, 1.0);
                    }
                    *b = last;
                }
            }
            _ => {
                for (i, b) in bands.iter_mut().enumerate() {
                    *b = vals[i].clamp(0.0, 1.0);
                }
            }
        }
        set_static_bands(bands);
        return;
    }
    // 上次的标量探针（等价于 16 段全等）
    if let Some(v) = env_nonempty("WE_PARTICLE_AUDIO") {
        if let Ok(x) = v.parse::<f32>() {
            if x > 0.0 {
                set_static_bands([x.clamp(0.0, 1.0); BANDS]);
                return;
            }
        }
    }
    // 库默认关 —— 只认**显式设定**（0/false/off/no 仍是关）；屏上内核由 drop-in 打开。
    let explicit = std::env::var_os("WE_AUDIO_SPECTRUM").is_some();
    GATE.store(crate::flag_default_on("WE_AUDIO_SPECTRUM") & explicit, Ordering::Relaxed);
}

/// 采集源（环境变量）
pub fn source_from_env() -> Source {
    Source::parse(&std::env::var("WE_AUDIO_SOURCE").unwrap_or_else(|_| "monitor".into()))
}

/// 当前选定的采集源（控制面可以改；`WE_AUDIO_SOURCE` 只是初值）
static SOURCE: AtomicU32 = AtomicU32::new(0); // 0=monitor 1=own 2=off

pub fn selected_source() -> Source {
    match SOURCE.load(Ordering::Relaxed) {
        1 => Source::Own,
        2 => Source::Off,
        _ => Source::Monitor,
    }
}

fn store_source(s: Source) {
    SOURCE.store(
        match s {
            Source::Monitor => 0,
            Source::Own => 1,
            Source::Off => 2,
        },
        Ordering::Relaxed,
    );
}

/// **控制面入口**（内核 `{"cmd":"audio", …}`）：一次把开关/来源/静态频谱都处理掉。
/// 返回一行状态（内核把它原样回给客户端 + 打进日志）。
pub fn set_from_control(
    on: Option<bool>,
    source: Option<&str>,
    bands: Option<[f32; BANDS]>,
) -> String {
    init_once();
    if let Some(b) = bands {
        set_static_bands(b);
    }
    if let Some(s) = source {
        let s = Source::parse(s);
        store_source(s);
        if enabled() && !is_static() {
            let _ = start_capture(s);
        }
    }
    if let Some(o) = on {
        set_enabled(o);
        if o && !is_static() {
            let _ = start_capture(selected_source());
        } else if !o {
            // 关就把采集也收掉：不然会留一个抓着 monitor 不放的 `parec`（实测踩到过）
            stop_capture();
        }
    }
    capture_status()
}

// ══ DSP ══════════════════════════════════════════════════════════════════════════════

/// 迭代 radix-2 复数 FFT（原地，预计算旋转因子 + 位反转表）。
struct Fft {
    n: usize,
    rev: Vec<u32>,
    /// 前半圈的 (cos, sin)（sin 带负号 = 正变换）
    tw: Vec<(f32, f32)>,
    hann: Vec<f32>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        let bits = n.trailing_zeros();
        let mut rev = vec![0u32; n];
        for i in 0..n {
            rev[i] = (i as u32).reverse_bits() >> (32 - bits);
        }
        let mut tw = Vec::with_capacity(n / 2);
        for k in 0..n / 2 {
            let a = -2.0 * std::f32::consts::PI * k as f32 / n as f32;
            tw.push((a.cos(), a.sin()));
        }
        // Hann 窗（OWE 不用时域窗；我们加窗是为了让"低音鼓"别糊到全 16 段）
        let hann = (0..n)
            .map(|i| {
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()
            })
            .collect();
        Fft { n, rev, tw, hann }
    }

    fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        for i in 0..n {
            let j = self.rev[i] as usize;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let step = n / len;
            let half = len / 2;
            let mut base = 0;
            while base < n {
                for k in 0..half {
                    let (c, s) = self.tw[k * step];
                    let (ar, ai) = (re[base + k], im[base + k]);
                    let (br, bi) = (re[base + k + half], im[base + k + half]);
                    let tr = br * c - bi * s;
                    let ti = br * s + bi * c;
                    re[base + k] = ar + tr;
                    im[base + k] = ai + ti;
                    re[base + k + half] = ar - tr;
                    im[base + k + half] = ai - ti;
                }
                base += len;
            }
            len <<= 1;
        }
    }
}

/// PCM → 16 段频谱（OWE `ResponseEngine` + `ResponseProcessor` 的移植）。
pub struct Analyzer {
    fft: Fft,
    /// 最近 `FFT_N` 帧（每声道一份）
    hist: [Vec<f32>; CHANNELS],
    /// 还没凑够一个 hop 的样本
    pend: [Vec<f32>; CHANNELS],
    /// 单声道混合（保持每声道独立是为了对齐 OWE 的 left/right 分组）
    envelopes: [f32; BANDS],
    smooth_history: [f32; RESPONSE_BINS * 2],
    output_history: [f32; RESPONSE_BINS * 2],
    generation: u64,
}

const ACTIVITY_THRESHOLD: f32 = 0.0001;
const ENVELOPE_FLOOR: f32 = 0.001;
const PEAK_FLOOR_RATIO: f32 = 0.333;
const SMOOTH_RATE: f32 = 20.0;
const RESPONSE_RATE: f32 = 40.0;
/// 包络分组的组大小：128 个值（左 64 + 右 64）÷ 16 组（OWE `SceneAudioResponse.cpp:119-126` 的 `group * 8`）
const GROUP: usize = RESPONSE_BINS * 2 / BANDS; // 8
/// 64 → 16 的分组大小（OWE `MaxPool<16,64>`）
const POOL: usize = RESPONSE_BINS / BANDS; // 4

impl Default for Analyzer {
    fn default() -> Self {
        Analyzer::new()
    }
}

impl Analyzer {
    pub fn new() -> Analyzer {
        Analyzer {
            fft: Fft::new(FFT_N),
            hist: [vec![0.0; FFT_N], vec![0.0; FFT_N]],
            pend: [Vec::with_capacity(HOP), Vec::with_capacity(HOP)],
            envelopes: [0.0; BANDS],
            smooth_history: [0.0; RESPONSE_BINS * 2],
            output_history: [0.0; RESPONSE_BINS * 2],
            generation: 0,
        }
    }

    /// 喂交错立体声 PCM（f32，48kHz）。内部凑够一个 hop 就分析一次。
///
    /// `frame_dt` = 渲染帧间隔（秒），用于包络平滑；`None` 用分析间隔（`HOP/SR`）。
    pub fn push_interleaved(&mut self, pcm: &[f32], frame_dt: Option<f32>) -> Option<Spectrum> {
        let mut last = None;
        for frame in pcm.chunks_exact(CHANNELS) {
            for (ch, s) in frame.iter().enumerate() {
                let v = if s.is_finite() { *s } else { 0.0 };
                self.pend[ch].push(v);
            }
            if self.pend[0].len() >= HOP {
                last = Some(self.analyze(frame_dt));
            }
        }
        last
    }

    /// 用环形历史做一次 16 段分析（并把结果 publish 出去）。
    pub fn analyze(&mut self, frame_dt: Option<f32>) -> Spectrum {
        // 历史前移 hop，尾巴补上新样本
        for ch in 0..CHANNELS {
            let n = self.pend[ch].len().min(HOP);
            self.hist[ch].copy_within(n..FFT_N, 0);
            let tail = FFT_N - n;
            self.hist[ch][tail..].copy_from_slice(&self.pend[ch][..n]);
            self.pend[ch].clear();
        }

        // ① 每声道 FFT → 64 段
        let mut bands64 = [[0.0f32; RESPONSE_BINS]; CHANNELS];
        let mut has_signal = false;
        for ch in 0..CHANNELS {
            let mut re: Vec<f32> = self.hist[ch]
                .iter()
                .zip(self.fft.hann.iter())
                .map(|(s, w)| s * w)
                .collect();
            let mut im = vec![0.0f32; FFT_N];
            if re.iter().any(|v| *v != 0.0) {
                has_signal = true;
                self.fft.forward(&mut re, &mut im);
                // 归一化：正变换幅度 = 2/N（幅度谱近似瞬时幅度）
                let norm = 2.0 / FFT_N as f32;
                let mut band = 0usize;
                for bin in 1..SPECTRUM_BINS {
                    let coordinate = (bin - 1) as f32 / (SPECTRUM_BINS - 1) as f32;
                    // OWE：band 单调不减（低段不被后面的 bin 抢回去）
                    let candidate = (coordinate.powf(0.25) * RESPONSE_BINS as f32) as usize;
                    band = candidate.min(band + 1).min(RESPONSE_BINS - 1);
                    let angle = std::f32::consts::PI * coordinate;
                    let weight = (0.501 - 0.499 * angle.cos()).max(0.0).sqrt();
                    let mag = (re[bin] * re[bin] + im[bin] * im[bin]).sqrt() * norm;
                    let v = mag * weight;
                    if v > bands64[ch][band] {
                        bands64[ch][band] = v;
                    }
                }
            }
        }
        self.generation += 1;

        // ② 无信号：输出全 0（OWE 也是直接 `BuildBuffers({})` 返回，不动历史）
        if !has_signal {
            let zero = Spectrum {
                generation: self.generation,
                ..Default::default()
            };
            publish(zero);
            publish_channels([0.0; BANDS], [0.0; BANDS]);
            return zero;
        }

        // ③ 128 值 = 左 64 + 右 64；包络归一化 + 平滑（OWE `SceneAudioResponse.cpp:117-164`）
        let mut input = [0.0f32; RESPONSE_BINS * 2];
        input[..RESPONSE_BINS].copy_from_slice(&bands64[0]);
        input[RESPONSE_BINS..].copy_from_slice(&bands64[1]);

        let mut group_peaks = [0.0f32; BANDS];
        let mut global_peak = 0.0f32;
        for group in 0..BANDS {
            let mut peak = 0.0f32;
            // OWE 把 128 个值（左 64 + 右 64）按 **8 个一组** 分成 16 组算包络
            // （`SceneAudioResponse.cpp:117-126`：`group = index / 8`，与下面的 `GROUP` 一致），
            // 注意这和"64 → 16 取 max"（`MaxPool`，比例 4）不是同一个分组。
            for offset in 0..GROUP {
                peak = peak.max(input[group * GROUP + offset]);
            }
            group_peaks[group] = peak;
            global_peak = global_peak.max(peak);
        }
        let peak_floor = global_peak * PEAK_FLOOR_RATIO;
        for p in group_peaks.iter_mut() {
            *p = p.max(peak_floor);
        }
        if global_peak < ACTIVITY_THRESHOLD {
            let zero = Spectrum {
                generation: self.generation,
                ..Default::default()
            };
            publish(zero);
            publish_channels([0.0; BANDS], [0.0; BANDS]);
            return zero;
        }
        // 重新出现音频：包络直接顶到 1（OWE:131-133 的"冷启动"分支）
        if self.envelopes.iter().all(|e| *e <= ACTIVITY_THRESHOLD) {
            self.envelopes = [1.0; BANDS];
        }

        let dt = frame_dt.unwrap_or(HOP as f32 / SAMPLE_RATE as f32);
        let frame_scale = dt.clamp(ACTIVITY_THRESHOLD, 0.25);
        for group in 0..BANDS {
            let delta = group_peaks[group] - self.envelopes[group];
            if delta.abs() <= ACTIVITY_THRESHOLD {
                self.envelopes[group] = group_peaks[group];
                continue;
            }
            let amount = frame_scale.min(delta.abs());
            self.envelopes[group] += amount * if delta > 0.0 { 1.0 } else { -0.5 };
        }

        let smooth_amount = (frame_scale * SMOOTH_RATE).min(1.0);
        let rise_limit = (frame_scale * RESPONSE_RATE).min(1.0);
        let fall_limit = (-frame_scale * RESPONSE_RATE).max(-1.0);
        for index in 0..RESPONSE_BINS * 2 {
            let group = index / GROUP;
            let envelope = self.envelopes[group].max(ENVELOPE_FLOOR);
            let normalized = input[index] / envelope;
            self.smooth_history[index] +=
                (normalized - self.smooth_history[index]) * smooth_amount;
            let delta = self.smooth_history[index] - self.output_history[index];
            self.output_history[index] += delta.clamp(fall_limit, rise_limit);
        }
        // ④ 64 → 16（每 4 段取 max，OWE `MaxPool<16,64>`），左右取平均
        let out16 = |base: usize| -> [f32; BANDS] {
            let mut o = [0.0f32; BANDS];
            for i in 0..BANDS {
                let mut m = 0.0f32;
                for k in 0..POOL {
                    m = m.max(self.output_history[base + i * POOL + k]);
                }
                o[i] = m;
            }
            o
        };
        let l = out16(0);
        let r = out16(RESPONSE_BINS);
        let mut bands = [0.0f32; BANDS];
        for i in 0..BANDS {
            bands[i] = (l[i] + r[i]) * 0.5;
        }
        let snap = Spectrum {
            bands,
            level: bands.iter().sum::<f32>() / BANDS as f32,
            generation: self.generation,
        };
        ANALYZED.fetch_add(1, Ordering::Relaxed);
        publish(snap);
        publish_channels(l, r);
        snap
    }
}

/// 全局分析器（采集线程与 `feed_pcm` 共用；用锁是因为内核可能同时喂）
fn analyzer() -> &'static Mutex<Analyzer> {
    static A: OnceLock<Mutex<Analyzer>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(Analyzer::new()))
}

/// 外部喂 PCM（交错立体声 f32 @48k）。内核"只采自己音频"那条路走这里。
pub fn feed_pcm(pcm: &[f32], frame_dt: Option<f32>) {
    if !enabled() {
        return;
    }
    if let Ok(mut a) = analyzer().lock() {
        a.push_interleaved(pcm, frame_dt);
    }
}

/// 从裸 f32le 交错立体声文件算一遍谱（离线：`audio_probe` 用）。
/// 返回每窗的 16 段；`dt` 用来对齐包络平滑的节奏。
pub fn analyze_pcm_file(path: &std::path::Path, dt: Option<f32>) -> Result<Vec<Spectrum>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读不了 {}：{e}", path.display()))?;
    analyze_pcm_bytes(&bytes, dt)
}

/// 同 [`analyze_pcm_file`]，但直接吃内存（WAV/PCM 已解出）。
pub fn analyze_pcm_bytes(bytes: &[u8], dt: Option<f32>) -> Result<Vec<Spectrum>, String> {
    if bytes.len() % 4 != 0 {
        return Err("PCM 字节数不是 4 的倍数（要求 f32le）".into());
    }
    let mut pcm = Vec::with_capacity(bytes.len() / 4);
    for c in bytes.chunks_exact(4) {
        pcm.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    let mut a = Analyzer::new();
    let mut out = Vec::new();
    let mut i = 0;
    while i + HOP * CHANNELS <= pcm.len() {
        let end = (i + HOP * CHANNELS).min(pcm.len());
        if let Some(s) = a.push_interleaved(&pcm[i..end], dt) {
            out.push(s);
        }
        i = end;
    }
    Ok(out)
}

// ══ 采集（PipeWire/PulseAudio 的 monitor）════════════════════════════════════════════

/// 默认采集设备：`pactl` 查默认 sink 的 `.monitor`；查不到就退回 `@DEFAULT_MONITOR@`。
pub fn default_monitor_device() -> String {
    if let Ok(out) = std::process::Command::new("pactl").arg("info").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(sink) = line.strip_prefix("Default Sink:") {
                let sink = sink.trim();
                if !sink.is_empty() {
                    return format!("{sink}.monitor");
                }
            }
        }
    }
    "@DEFAULT_MONITOR@".to_string()
}

/// 起采集线程（幂等）。`Source::Own` = 不起线程（等 `feed_pcm`）。
pub fn start_capture(source: Source) -> Result<(), String> {
    if source == Source::Off {
        stop_capture();
        return Ok(());
    }
    if source == Source::Own {
        // 只吃内核喂进来的 PCM：不起外部进程，也**不**自动开总开关
        CAPTURE_STOP.store(false, Ordering::Relaxed);
        CAPTURE_ON.store(true, Ordering::Relaxed);
        set_info("own(pcm)");
        return Ok(());
    }
    if CAPTURE_ON.load(Ordering::Relaxed) && !CAPTURE_STOP.load(Ordering::Relaxed) {
        return Ok(()); // 已经起过
    }
    CAPTURE_STOP.store(false, Ordering::Relaxed);
    CAPTURE_ON.store(true, Ordering::Relaxed);
    let dev = std::env::var("WE_AUDIO_DEVICE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(default_monitor_device);
    let dev2 = dev.clone();
    std::thread::Builder::new()
        .name("we-audio-capture".into())
        .spawn(move || capture_loop(&dev2))
        .map_err(|e| format!("起不了采集线程：{e}"))?;
    set_info(&format!("monitor:{dev}"));
    Ok(())
}

/// **关掉采集**：杀掉 `parec` + 让线程退出。关开关后必须调它 ——
/// 否则会留一个"什么都不读、但抓着 monitor 不放"的孤儿进程（实测踩到过：
/// `{"cmd":"audio","on":false}` 之后 `parec` 还在）。
pub fn stop_capture() {
    CAPTURE_ON.store(false, Ordering::Relaxed);
    CAPTURE_STOP.store(true, Ordering::Relaxed);
    let pid = CAPTURE_PID.swap(0, Ordering::Relaxed);
    if pid != 0 {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    set_info("off");
}

/// 采集线程主体：拉 `parec` 的 stdout → 分析 → publish。进程挂了自动重连（退避）。
fn capture_loop(device: &str) {
    let mut backoff_ms = 200u64;
    while !CAPTURE_STOP.load(Ordering::Relaxed) {
        let mut cmd = std::process::Command::new("parec");
        cmd.arg("--device")
            .arg(device)
            .arg("--format=float32le")
            .arg("--rate")
            .arg(SAMPLE_RATE.to_string())
            .arg("--channels")
            .arg(CHANNELS.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // 父进程没了就跟着死（避免渲染器崩溃后留一个孤儿 parec）
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(mut child) => {
                backoff_ms = 200;
                CAPTURE_PID.store(child.id(), Ordering::Relaxed);
                set_info(&format!("monitor:{device}:running"));
                let mut buf = vec![0u8; HOP * CHANNELS * 4 * 8];
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    loop {
                        match out.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let mut pcm = Vec::with_capacity(n / 4);
                                for c in buf[..n - (n % 4)].chunks_exact(4) {
                                    pcm.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                                }
                                feed_pcm(&pcm, None);
                                if CAPTURE_STOP.load(Ordering::Relaxed) {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                CAPTURE_PID.store(0, Ordering::Relaxed);
                if CAPTURE_STOP.load(Ordering::Relaxed) {
                    break;
                }
                set_info(&format!("monitor:{device}:reconnecting"));
            }
            Err(e) => {
                set_info(&format!("monitor:{device}:spawn-err({e})"));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
        backoff_ms = (backoff_ms * 2).min(5000);
    }
    set_info("off");
}

/// 单元测试用：序列化"动全局频谱"的测试（`cargo test` 是多线程跑同一个进程，
/// 而频谱是进程级全局 ⇒ 不串行化就会互相踩）。
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

/// 单元测试用：**动全局频谱**的测试守卫 = 串行化 + 离开时把全局状态复位。
/// 为什么不能只用 [`test_lock`]：`Analyzer::push_interleaved` 会把每窗结果 `publish`
/// 到**进程级**原子（采集线程与它共用这一条发布路径），所以哪怕测试拿着自己 `new` 出来的
/// 局部分析器，**照样会覆写全局频谱**。分析器类测试若跑在别的测试（粒子音频响应 / 恒等）
/// 中间，就会把它们的频段值 / 总开关改掉：实测全量并行时
/// `particles::tests::emitter_audio_response_boosts_the_rate_when_a_spectrum_arrives`
/// 会偶发失败（`loud` 只剩 64，因为它的 0.9 频段被分析器测试的发布覆盖了）。
/// 守卫把"取锁 + 快照总开关 + 退出时复位成没有音频"绑在一起，测试只需 `let _g = …`。
#[cfg(test)]
pub(crate) struct SpectrumGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    gate: bool,
}

#[cfg(test)]
impl Drop for SpectrumGuard {
    fn drop(&mut self) {
        // 复位成"没有音频"（全 0 频段 ⇒ 所有音频响应恒等），再把总开关还原成进入时的值
        set_static_bands([0.0; BANDS]);
        set_enabled(self.gate);
    }
}

/// 取 [`SpectrumGuard`]（见其注释：串行化 + 退出复位）。
#[cfg(test)]
pub(crate) fn spectrum_guard() -> SpectrumGuard {
    let lock = test_lock();
    let gate = enabled();
    SpectrumGuard { _lock: lock, gate }
}

/// 渲染器构造时调一次：读环境、必要时起采集。
pub fn autostart() {
    init_from_env();
    if !enabled() {
        return;
    }
    // 静态频谱（`WE_AUDIO_BANDS`/`WE_PARTICLE_AUDIO`）= 离线复现口径：**不起采集、不碰系统音频**。
    if is_static() {
        println!("[音频谱] 静态频谱（离线复现）：{}", capture_status());
        return;
    }
    let src = source_from_env();
    store_source(src);
    if let Err(e) = start_capture(src) {
        eprintln!("[音频谱] 采集起不来（{e}）—— 频谱保持 0（音频响应恒等）");
    } else {
        println!("[音频谱] 源={} 设备/状态={}", src.name(), capture_status());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 左右声道数组各走各的；总开关关掉后 `channels` 一律全 0。
    #[test]
    fn channels_keep_left_and_right_separate() {
        let _g = spectrum_guard();
        set_enabled(true);
        let mut l = [0.0f32; BANDS];
        let mut r = [0.0f32; BANDS];
        for i in 0..BANDS {
            l[i] = i as f32 * 0.05;
            r[i] = 1.0 - i as f32 * 0.05;
        }
        publish_channels(l, r);
        let (gl, gr) = channels();
        assert_eq!(gl, l, "左声道数组");
        assert_eq!(gr, r, "右声道数组");
        set_enabled(false);
        assert_eq!(channels(), ([0.0; BANDS], [0.0; BANDS]), "关掉后恒 0");
    }

    #[test]
    fn spectrum_range_mean_matches_owe_index_semantics() {
        let mut s = Spectrum::default();
        for i in 0..BANDS {
            s.bands[i] = i as f32;
        }
        // 默认 [0,15] = 全段平均
        assert!((s.range_mean([0.0, 15.0]) - 7.5).abs() < 1e-6);
        // 单段
        assert!((s.range_mean([3.0, 3.0]) - 3.0).abs() < 1e-6);
        // 闭区间平均 (4+5+6)/3 = 5
        assert!((s.range_mean([4.0, 6.0]) - 5.0).abs() < 1e-6);
        // 越界夹取：-3 → 0，99 → 15
        assert!((s.range_mean([-3.0, 0.0]) - 0.0).abs() < 1e-6);
        assert!((s.range_mean([15.0, 99.0]) - 15.0).abs() < 1e-6);
    }

    #[test]
    fn silent_spectrum_is_never_active() {
        let s = Spectrum::default();
        assert!(!s.any(), "没有音频 ⇒ 恒等");
        assert_eq!(s.range_mean([0.0, 15.0]), 0.0);
    }

    #[test]
    fn fft_peaks_at_the_right_bin() {
        // 2048 点、1000Hz 正弦（bins = 1000/48000*2048 ≈ 42.7 → 43）
        let f = Fft::new(FFT_N);
        let freq = 42.0 * SAMPLE_RATE as f32 / FFT_N as f32;
        let mut re: Vec<f32> = (0..FFT_N)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin())
            .collect();
        let mut im = vec![0.0f32; FFT_N];
        f.forward(&mut re, &mut im);
        let mag = |b: usize| (re[b] * re[b] + im[b] * im[b]).sqrt();
        let peak = (1..FFT_N / 2).max_by(|a, b| mag(*a).partial_cmp(&mag(*b)).unwrap()).unwrap();
        assert_eq!(peak, 42, "正弦的峰必须落在对应 bin 上");
        assert!(mag(42) > 100.0 * mag(50), "远离峰值的 bin 应该低两个数量级");
    }

    #[test]
    fn analyzer_tracks_a_tone_and_decays_to_zero_on_silence() {
        // 本测试用局部分析器，但 `push_interleaved` 会 `publish` 到**进程级**频谱
        // ⇒ 必须与其它动频谱的测试串行，且退出时复位（见 `SpectrumGuard` 注释）
        let _g = spectrum_guard();
        let mut a = Analyzer::new();
        // 3 秒 440Hz，音量 0.5。⚠ 前 ~1.2 秒是**冷启动**（OWE `SceneAudioResponse.cpp:131-133`
        // 把包络一次顶到 1.0，再按 0.5×frame_scale 往下追峰）⇒ 要看稳态得跑够长。
        let sr = SAMPLE_RATE as f32;
        let mut pcm = Vec::new();
        for i in 0..(SAMPLE_RATE as usize * 3) {
            let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
            pcm.push(v);
            pcm.push(v);
        }
        let mut next = None;
        for chunk in pcm.chunks(1024) {
            if let Some(s) = a.push_interleaved(chunk, Some(1.0 / 30.0)) {
                next = Some(s);
            }
        }
        let s = next.expect("应该有分析结果");
        assert!(s.any(), "440Hz 应该点亮至少一段");
        // 440Hz → 16 段里的第 5 段（index 4）必须是高的（实测 0.998）
        let (top, _) = s
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(top, 4, "440Hz 的峰段：{:?}", s.bands);
        assert!(s.bands[4] > 0.5, "峰段应该归一化到接近 1：{}", s.bands[4]);
        // 远离 440Hz 的高段不该亮（Hann 泄漏只影响相邻段）
        assert!(s.bands[15] < 0.1, "高频段不该被 440Hz 点亮：{}", s.bands[15]);
        // 静音：窗里还留着前 2 个窗的余音（FFT 窗长 2048 帧 / hop 1024 ⇒ 最多 2 窗），
        // 之后的每一窗都必须是 0（音频响应随之恒等）。
        let silence = vec![0.0f32; SAMPLE_RATE as usize * CHANNELS];
        let mut got = Vec::new();
        for chunk in silence.chunks(1024) {
            if let Some(z) = a.push_interleaved(chunk, Some(1.0 / 30.0)) {
                got.push(z);
            }
        }
        assert!(got.len() > 10, "1 秒静音应该有足够多的窗：{}", got.len());
        for (n, z) in got.iter().enumerate() {
            if n < 3 {
                continue; // 窗内余音
            }
            assert!(!z.any(), "第 {n} 窗静音必须回到 0（音频响应随之恒等）：{:?}", z.bands);
        }
    }

    #[test]
    fn static_bands_round_trip_through_atomics() {
        // 注意：本测试会动全局 GATE ⇒ 用单独的静态值再复位
        let _g = test_lock();
        let old = enabled();
        let mut b = [0.0f32; BANDS];
        for i in 0..BANDS {
            b[i] = (i as f32 + 1.0) / BANDS as f32;
        }
        set_static_bands(b);
        let s = spectrum();
        assert_eq!(s.bands, b);
        assert!(s.any());
        assert!(s.level > 0.0);
        // 复位成"没有音频"：其它测试（粒子恒等）依赖这一点
        set_static_bands([0.0; BANDS]);
        set_enabled(old);
        assert!(!spectrum().any());
    }

    #[test]
    fn analyzer_pcm_bytes_is_deterministic() {
        // 同一段 PCM 两次分析 ⇒ 逐字节相同（`WE_AUDIO_BANDS` 复现口径的基础）
        // ⚠ `analyze_pcm_bytes` 内部会 `publish` 到**进程级**频谱（每窗一次）⇒ 独占 + 复位
        let _g = spectrum_guard();
        let mut pcm = Vec::new();
        for i in 0..(SAMPLE_RATE as usize / 2) {
            let v = 0.3 * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / SAMPLE_RATE as f32).sin();
            pcm.push(v);
            pcm.push(v);
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 4);
        for v in &pcm {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let a = analyze_pcm_bytes(&bytes, Some(1.0 / 30.0)).unwrap();
        let b = analyze_pcm_bytes(&bytes, Some(1.0 / 30.0)).unwrap();
        assert_eq!(a.len(), b.len());
        assert!(!a.is_empty());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.bands, y.bands, "逐字节可复现");
        }
    }
}
