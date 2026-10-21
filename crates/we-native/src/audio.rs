//! scene 的 `sound` 对象 → `pw-play` 子进程。
//!
//! WE 语义（照两个参考实现，`open-wallpaper-engine/src/Scene/Pkg/Parse/Sound/SoundParser.cpp`）：
//! `sound` 是**播放列表**，一个文件播完切下一个；`playbackmode` = `loop`（顺序循环）/ `random`（随机挑）
//! / `single`（放一个就停）；`volume` 0..1。
//!
//! **`mintime`/`maxtime` 不参与播放** —— OWE 把这两个值读进 `Config` 却从没用过
//! （waywallen 那边同样没有），本机 6 条带音频的壁纸全是 `mintime=1 / maxtime=5`，
//! 真按它切成 1~5 秒碎片会把整首曲子剁烂。所以跟参考实现保持一致：只解析、不执行。
//!
//! 为什么是子进程：本机已有 PipeWire（`/usr/bin/pw-play`，支持 `--volume`），
//! 复用"起子进程 + 生命周期跟着壁纸走"这套（视频那条路已经这么干了），不引新依赖。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// 音频落盘目录：**不要用 tmpfs**（最大的那条壁纸有 12 个 mp3、约 100MB）
pub fn default_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".cache")
        });
    base.join("wallpaper-hub").join("sounds")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Loop,
    Random,
    Single,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.to_ascii_lowercase().as_str() {
            "random" => Mode::Random,
            "single" => Mode::Single,
            _ => Mode::Loop,
        }
    }

    /// 下一个该播的下标；`None` = 到此为止（single）
    pub fn next_index(&self, cur: usize, n: usize, rnd: u32) -> Option<usize> {
        if n == 0 {
            return None;
        }
        match self {
            Mode::Loop => Some((cur + 1) % n),
            Mode::Random => Some((rnd as usize) % n),
            Mode::Single => None,
        }
    }
}

#[derive(Debug)]
struct Track {
    name: String,
    files: Vec<PathBuf>,
    volume: f32,
    mode: Mode,
    idx: usize,
    child: Option<Child>,
    /// **自己输出的 PCM 管线**（`WE_AUDIO_SOURCE=own` 时用它替掉 `pw-play`）：
    /// `ffmpeg 解码 → 我们在进程内拿同一份 PCM 做 FFT（喂给渲染器）→ pw-cat 播出`。
    /// 这样频谱**只来自壁纸自己的音频**，不碰系统回环（隐私模式）。
    pipe: Option<OwnPipe>,
}

/// 是否走"只采自己音频"的自播管线。
///
/// 判据 = **音频频谱功能开着** 且 **来源选了 own**：
/// 功能没开时一律走原来的 `pw-play`（一行行为都不变，`pw-play` 是久经考验的那条路）。
fn own_mode() -> bool {
    we_scene_render::audio::enabled()
        && we_scene_render::audio::selected_source() == we_scene_render::audio::Source::Own
}

/// `ffmpeg → 进程内 FFT → pw-cat` 的管线本体（跑在自己的线程上）。
#[derive(Debug)]
struct OwnPipe {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl OwnPipe {
    fn spawn(file: &Path, volume: f32) -> Result<OwnPipe, String> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let (s2, a2) = (stop.clone(), alive.clone());
        let f = file.to_path_buf();
        let join = std::thread::Builder::new()
            // Wayland/Rust 线程名上限 15 字节：`we-audio-self`（含前缀）刚好塞得下
            .name("we-audio-self".into())
            .spawn(move || {
                if let Err(e) = pipe_loop(&f, volume, &s2) {
                    eprintln!("[音频] 自播管线出错（{e}）—— 静音继续");
                }
                a2.store(false, Ordering::Relaxed);
            })
            .map_err(|e| format!("起不了自播线程：{e}"))?;
        Ok(OwnPipe {
            stop,
            alive,
            join: Some(join),
        })
    }

    fn alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for OwnPipe {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 一个文件的完整生命周期：起 `ffmpeg`（解码）+ `pw-cat`（播放），
/// 中间每个块都先 `feed_pcm` 给渲染器（进程内，零拷贝到磁盘）。
fn pipe_loop(file: &Path, volume: f32, stop: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let vol = volume.clamp(0.0, 1.0);
    let mut ff = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(file)
        // 48kHz/立体声 f32le = 渲染器分析器的输入口径（`we_scene_render::audio::SAMPLE_RATE`）
        .args(["-f", "f32le", "-ac", "2", "-ar", "48000", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg 起不来：{e}"))?;
    let mut pc = Command::new("pw-cat")
        .args(["--playback", "--raw", "--rate", "48000", "--channels", "2", "--format", "f32"])
        .arg("--volume")
        .arg(format!("{vol:.3}"))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            let _ = ff.kill();
            let _ = ff.wait();
            format!("pw-cat 起不来：{e}")
        })?;
    let mut out = ff.stdout.take().ok_or("拿不到 ffmpeg 的 stdout")?;
    let mut sink = pc.stdin.take().ok_or("拿不到 pw-cat 的 stdin")?;
    // 每块 = 一个分析 hop（1024 帧 × 2 声道 × 4 字节 = 8KB）⇒ 21ms 一块，延迟极小
    let mut buf = vec![0u8; 8192];
    let mut pcm: Vec<f32> = Vec::with_capacity(2048);
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let n = out.read(&mut buf).map_err(|e| format!("读 ffmpeg：{e}"))?;
        if n == 0 {
            break;
        }
        // ① 进程内算谱（`feed_pcm` 自己只在功能开着时才动；未开 = 直接返回）
        pcm.clear();
        for c in buf[..n - (n % 4)].chunks_exact(4) {
            pcm.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        }
        we_scene_render::audio::feed_pcm(&pcm, None);
        // ② 同一份字节原样播出去
        if sink.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = ff.kill();
    let _ = ff.wait();
    drop(sink);
    let _ = pc.wait();
    Ok(())
}

impl Track {
    fn spawn_now(&mut self) {
        if self.files.is_empty() {
            return;
        }
        let file = self.files[self.idx % self.files.len()].clone();
        let vol = self.volume.clamp(0.0, 1.0);
        if own_mode() {
            match OwnPipe::spawn(&file, vol) {
                Ok(p) => {
                    self.pipe = Some(p);
                    self.child = None;
                    return;
                }
                Err(e) => {
                    eprintln!("[音频] 自播管线起不来（{e}）—— 退回 pw-play");
                }
            }
        }
        match Command::new("pw-play")
            .arg("--volume")
            .arg(format!("{vol:.3}"))
            .arg(&file)
            .spawn()
        {
            Ok(c) => self.child = Some(c),
            // 播放器不在 / 起不来：报一次，别每帧刷屏
            Err(e) => {
                eprintln!("[音频] pw-play 起不来（{e}）—— 静音继续");
                self.child = None;
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // Drop 会 join 线程（线程里自己 kill 掉 ffmpeg/pw-cat）
        self.pipe = None;
    }

    fn alive(&mut self) -> bool {
        if let Some(p) = self.pipe.as_ref() {
            return p.alive();
        }
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
}

pub struct Player {
    tracks: Vec<Track>,
    playing: bool,
    /// 伪随机（不引 rand：只是用来挑播放列表里的下一个，够用）
    rnd: u32,
    spawned_once: bool,
}

impl Player {
    /// 解析场景目录里所有**可见且真带文件**的 sound 对象，并把音频从 pkg 落到缓存目录。
    /// 没有任何音轨 ⇒ `Ok(None)`（静音壁纸，不必占一个 Player）。
    pub fn prepare(dir: &Path, cache: &Path) -> Result<Option<Player>, String> {
        let project = we_scene::Project::read_dir(dir).map_err(|e| e.to_string())?;
        let pkg_path = project.scene_pkg();
        let pkg_bytes = std::fs::read(&pkg_path).map_err(|e| format!("读不了 scene.pkg：{e}"))?;
        let pkg = we_scene::pkg::Pkg::parse(&pkg_bytes).map_err(|e| e.to_string())?;
        let scene_bytes = pkg
            .get(&pkg_bytes, "scene.json")
            .ok_or_else(|| "pkg 里没有 scene.json".to_string())?;
        let doc = we_scene::scene::parse_scene(scene_bytes).map_err(|e| e.to_string())?;

        let stem = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "scene".into());
        let out_dir = cache.join(&stem);
        let mut tracks = Vec::new();
        let mut written_bytes = 0u64;

        for obj in &doc.objects {
            let files = obj.sound_files();
            if files.is_empty() || !obj.is_visible() {
                continue;
            }
            let mut paths = Vec::new();
            for name in &files {
                // pkg 里的条目名就是 scene.json 里写的那个相对路径（如 `sounds/x.mp3`）
                let entry = pkg
                    .find(name)
                    .or_else(|| pkg.find(name.trim_start_matches('/')));
                let Some(e) = entry else {
                    eprintln!("[音频] pkg 里找不到 {name}，跳过");
                    continue;
                };
                let Some(blob) = pkg.slice(&pkg_bytes, e) else {
                    continue;
                };
                // 落盘名：把路径分隔符压平（同名文件在不同子目录也不会撞）
                let flat: String = name
                    .chars()
                    .map(|c| if c == '/' || c == '\\' { '_' } else { c })
                    .collect();
                let out = out_dir.join(flat);
                let need = match std::fs::metadata(&out) {
                    Ok(md) => md.len() != blob.len() as u64,
                    Err(_) => true,
                };
                if need {
                    if std::fs::create_dir_all(&out_dir).is_err() {
                        continue;
                    }
                    match std::fs::File::create(&out).and_then(|mut f| f.write_all(blob)) {
                        Ok(()) => written_bytes += blob.len() as u64,
                        Err(e) => {
                            eprintln!("[音频] 写不出 {}（{e}）", out.display());
                            continue;
                        }
                    }
                }
                paths.push(out);
            }
            if paths.is_empty() {
                continue;
            }
            tracks.push(Track {
                name: obj.name.clone(),
                files: paths,
                volume: obj.volume_f32(),
                mode: Mode::parse(obj.playbackmode.as_deref().unwrap_or("loop")),
                idx: 0,
                child: None,
                pipe: None,
            });
        }

        if tracks.is_empty() {
            return Ok(None);
        }
        println!(
            "[音频] {} 个 sound 对象（{} 个文件，本趟落盘 {:.1}MB，缓存 {}）",
            tracks.len(),
            tracks.iter().map(|t| t.files.len()).sum::<usize>(),
            written_bytes as f64 / (1024.0 * 1024.0),
            out_dir.display()
        );
        for t in &tracks {
            println!(
                "[音频]   {}：{} 个文件  音量 {:.2}  模式 {:?}",
                if t.name.is_empty() { "(无名)" } else { &t.name },
                t.files.len(),
                t.volume,
                t.mode
            );
        }
        Ok(Some(Player {
            tracks,
            playing: false,
            rnd: 0x9e3779b9,
            spawned_once: false,
        }))
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// 自增伪随机（xorshift）；只用来挑播放列表下标
    fn roll(&mut self) -> u32 {
        let mut x = self.rnd | 1;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rnd = x;
        x
    }

    /// 跟随"该不该渲染"：睡/暂停 ⇒ 静音（进程直接收掉，不是调音量）
    pub fn set_playing(&mut self, on: bool) {
        if on == self.playing {
            return;
        }
        self.playing = on;
        if on {
            for t in self.tracks.iter_mut() {
                if !t.files.is_empty() {
                    t.spawn_now();
                }
            }
            println!("[音频] ▶ 开始播放（{} 个对象）", self.tracks.len());
        } else {
            for t in self.tracks.iter_mut() {
                t.stop();
            }
            println!("[音频] ⏸ 已静音（壁纸睡了/暂停了）");
        }
    }

    /// 每个 tick 调一次：文件播完了就按模式切下一个
    pub fn tick(&mut self) {
        if !self.playing {
            return;
        }
        let mut roll_seed = 0u32;
        for i in 0..self.tracks.len() {
            if self.tracks[i].alive() {
                continue;
            }
            let (cur, n, mode) = {
                let t = &self.tracks[i];
                (t.idx, t.files.len(), t.mode)
            };
            if !self.tracks[i].files.is_empty() && self.tracks[i].child.is_some() {
                // child 存在但已退出 = 这一首放完了
                self.tracks[i].child = None;
            }
            if roll_seed == 0 {
                roll_seed = self.roll();
            }
            match mode.next_index(cur, n, roll_seed) {
                Some(next) => {
                    self.tracks[i].idx = next;
                    self.tracks[i].spawn_now();
                }
                None => {
                    // single：放完就停（下架这个 track，别每 tick 都试）
                    self.tracks[i].files.clear();
                }
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    #[allow(dead_code)]
    pub fn spawned_once(&self) -> bool {
        self.spawned_once
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // 换壁纸/退出：别留孤儿播放器
        for t in self.tracks.iter_mut() {
            t.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing_defaults_to_loop() {
        assert_eq!(Mode::parse("loop"), Mode::Loop);
        assert_eq!(Mode::parse("random"), Mode::Random);
        assert_eq!(Mode::parse("single"), Mode::Single);
        assert_eq!(Mode::parse("RANDOM"), Mode::Random);
        assert_eq!(Mode::parse(""), Mode::Loop, "不认识的都当 loop");
        assert_eq!(Mode::parse("bogus"), Mode::Loop);
    }

    #[test]
    fn loop_walks_the_playlist_in_order() {
        assert_eq!(Mode::Loop.next_index(0, 3, 7), Some(1));
        assert_eq!(Mode::Loop.next_index(2, 3, 7), Some(0), "到底回绕");
    }

    #[test]
    fn random_stays_inside_the_playlist() {
        for rnd in 0..50u32 {
            let i = Mode::Random.next_index(0, 3, rnd).unwrap();
            assert!(i < 3, "random 越界：{i}");
        }
        assert_eq!(Mode::Random.next_index(0, 3, 5), Some(2));
    }

    #[test]
    fn single_stops() {
        assert_eq!(Mode::Single.next_index(0, 3, 1), None);
    }

    #[test]
    fn no_files_means_no_next() {
        assert_eq!(Mode::Loop.next_index(0, 0, 1), None);
        assert_eq!(Mode::Random.next_index(0, 0, 1), None);
    }
}
