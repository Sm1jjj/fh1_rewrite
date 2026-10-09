//! FH1 FMV decoding through the `ffmpeg` CLI (data/extracted/plans/architecture.md §3a, §4; fmv.md §3.1/§3.3).
//! Bevy-free and cpal-free: the UI side polls [`FfmpegVideo::frame`] each render frame and hands
//! [`FfmpegVideo::audio_source`] to the audio side.
//!
//! # Caller checklist
//! 1. [`probe`] once; on `Err` skip the movie with one log line (needs `wmv3`, `wmapro`, `wmav2` decoders).
//! 2. Pick audio: [`StreamDef::load`] the `.def` next to the movie, then [`AudioSel::choose`] (EN/FR/... stream)
//!    or [`AudioSel::FirstAudio`] when there is no `.def` / no audio entry (splash intros, PressStart).
//! 3. [`FfmpegVideo::open`] with the output device's sample rate in [`OpenOpts::sample_rate`].
//! 4. Give `audio_source()` to the audio thread; its `read` is the cpal-callback pull (never blocks).
//! 5. Each Bevy frame call `frame()` (new RGBA frame or `None`), stop when `finished()` or `error()`.
//!
//! # Decisions
//! * Two ffmpeg children per movie: video (`rawvideo`, RGBA, `-s WxH`) and audio (`f32le`, stereo, device
//!   rate; f32 interleaved because the cpal side mixes f32). Both read the same file, `-nostdin`, console hidden.
//! * Frame rate and duration come from one `ffmpeg -hide_banner -i <file>` run in `open()` (parses the
//!   `NN.NN fps` and `Duration:` stderr lines; no ffprobe, the release only ships ffmpeg.exe). The video child
//!   is forced to that constant rate with `-r` (29.97 / 23.98 snap to N*1000/1001), so `pts = index / fps`.
//! * Master clock: frames pulled from the audio ring / rate. It only advances when the device pulls real data,
//!   so A/V stay locked through underruns. With no audio (or once audio has fully drained, or the audio child
//!   produced nothing) it falls back to a wall clock that starts at the first `clock()`/`frame()` call
//!   and continues from the audio position.
//! * Back-pressure: the video thread holds each decoded frame until `pts <= clock + 20 ms` before publishing
//!   it, so ffmpeg is throttled by the pipe. The slot keeps only the newest frame (drop, never queue).
//! * The audio ring holds ~1 s; the audio thread blocks (Condvar with timeout) when it is full.
//! * Looping (`-stream_loop -1` on both children): pts keep increasing across loops; audio and video loops
//!   can drift by the difference of their stream lengths (video then just catches up/waits).
//! * Risk: `finished()` waits for the audio ring to drain, so a caller that never pulls audio never finishes.
//! * Drop sets `stop`, kills and waits both children, then joins the threads (every wait has a timeout).

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How far (seconds) ahead of the clock a frame may be published / returned.
const LEAD: f64 = 0.02;
const TAIL_LINES: usize = 8;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `FH1_FFMPEG`, else `ffmpeg` on PATH.
pub fn ffmpeg_path() -> PathBuf {
    std::env::var_os("FH1_FFMPEG").map(PathBuf::from).unwrap_or_else(|| "ffmpeg".into())
}

/// No console window for child processes (Windows); nothing to do elsewhere.
fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Checks (once, cached) that ffmpeg runs and has the `wmv3`, `wmapro` and `wmav2` decoders.
pub fn probe() -> Result<(), String> {
    static CACHE: OnceLock<Result<(), String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let mut cmd = Command::new(ffmpeg_path());
            cmd.args(["-hide_banner", "-decoders"]).stdin(Stdio::null()).stderr(Stdio::null());
            let out = hide_console(&mut cmd).output().map_err(|e| format!("cannot run ffmpeg: {e}"))?;
            check_decoders(&String::from_utf8_lossy(&out.stdout))
        })
        .clone()
}

/// Looks for the required decoders in `ffmpeg -decoders` output (`" A....D wmapro  Windows Media ..."`).
fn check_decoders(text: &str) -> Result<(), String> {
    let missing: Vec<&str> = ["wmv3", "wmapro", "wmav2"]
        .into_iter()
        .filter(|want| !text.lines().any(|l| l.split_whitespace().nth(1) == Some(*want)))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("ffmpeg lacks decoders: {}", missing.join(", ")))
    }
}

// ---------------------------------------------------------------------------------------------------------
// .def parsing and language selection
// ---------------------------------------------------------------------------------------------------------

/// Parsed `<movie>.def`: `<Audio id="6" lang="EN"/>` ... `<Video id="7" />`. Ids are 1-based ffmpeg stream
/// indices (`-map 0:<id-1>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamDef {
    pub audio: Vec<(u32, String)>,
    pub video: Option<u32>,
}

impl StreamDef {
    pub fn parse(text: &str) -> Self {
        let mut def = StreamDef::default();
        for piece in text.split('<').skip(1) {
            let body = piece.split('>').next().unwrap_or("");
            let name = body.split_whitespace().next().unwrap_or("");
            let attr = |key: &str| {
                body.split_whitespace().skip(1).find_map(|p| {
                    let (k, v) = p.split_once('=')?;
                    k.eq_ignore_ascii_case(key).then(|| v.trim_matches(|c| c == '"' || c == '\'' || c == '/'))
                })
            };
            let id = attr("id").and_then(|v| v.parse::<u32>().ok());
            if name.eq_ignore_ascii_case("Audio") {
                if let Some(id) = id {
                    def.audio.push((id, attr("lang").unwrap_or("").to_string()));
                }
            } else if name.eq_ignore_ascii_case("Video") {
                def.video = def.video.or(id);
            }
        }
        def
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        Ok(Self::parse(&String::from_utf8_lossy(&std::fs::read(path)?)))
    }

    /// 0-based stream index of the audio track for `game_lang`; falls back to EN, then the first audio entry.
    pub fn audio_stream(&self, game_lang: &str) -> Option<u32> {
        let find = |l: &str| self.audio.iter().find(|(_, x)| x.eq_ignore_ascii_case(l));
        find(audio_lang_for(game_lang))
            .or_else(|| find("EN"))
            .or_else(|| self.audio.first())
            .map(|(id, _)| id.saturating_sub(1))
    }
}

/// Dub language for a game language (fmv.md §1b): EN/GB, FR, DE, IT, ES/MX, NL; everything else is EN.
pub fn audio_lang_for(game_lang: &str) -> &'static str {
    match game_lang.trim().to_ascii_uppercase().as_str() {
        "FR" => "FR",
        "DE" => "DE",
        "IT" => "IT",
        "ES" | "MX" => "ES",
        "NL" => "NL",
        _ => "EN",
    }
}

/// Which audio stream of the file to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSel {
    /// Silent movie; wall clock.
    None,
    /// Absolute 0-based stream index (`-map 0:<n>`).
    Stream(u32),
    /// First audio stream of the file, if any (`-map 0:a:0?`).
    FirstAudio,
}

impl AudioSel {
    /// `.def` with an audio entry: the language's stream; otherwise (no `.def`, or a `.def` that lists no audio
    /// such as PressStart) the file's first audio stream. Pass `AudioSel::None` yourself to mute a movie.
    pub fn choose(def: Option<&StreamDef>, game_lang: &str) -> Self {
        def.and_then(|d| d.audio_stream(game_lang)).map_or(AudioSel::FirstAudio, AudioSel::Stream)
    }
}

// ---------------------------------------------------------------------------------------------------------
// ffmpeg -i stderr parsing
// ---------------------------------------------------------------------------------------------------------

/// What `ffmpeg -i` reports about a file.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MediaInfo {
    pub fps: Option<f64>,
    pub duration: Option<f64>,
}

/// Parses the `Duration: HH:MM:SS.ss` line and the first video stream's `NN.NN fps` (else `tbr`).
pub fn parse_media_info(stderr: &str) -> MediaInfo {
    let duration = stderr.split("Duration:").nth(1).and_then(|r| {
        let t = r.trim_start().split(',').next()?;
        let mut it = t.split(':');
        let h = it.next()?.trim().parse::<f64>().ok()?;
        let m = it.next()?.trim().parse::<f64>().ok()?;
        let s = it.next()?.trim().parse::<f64>().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    });
    let fps = stderr.lines().find(|l| l.contains("Stream #") && l.contains("Video:")).and_then(|l| {
        let toks: Vec<&str> = l.split_whitespace().collect();
        ["fps", "tbr"].iter().find_map(|unit| {
            let i = toks.iter().position(|t| t.trim_end_matches(',') == *unit)?;
            toks.get(i.checked_sub(1)?)?.parse::<f64>().ok()
        })
    });
    MediaInfo { fps, duration }
}

fn media_info(path: &Path) -> io::Result<MediaInfo> {
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-hide_banner", "-i"]).arg(path).stdin(Stdio::null());
    // ffmpeg exits non-zero ("At least one output file must be specified"); the stream list is on stderr.
    let out = hide_console(&mut cmd).output()?;
    Ok(parse_media_info(&String::from_utf8_lossy(&out.stderr)))
}

/// Nearest exact rational: NTSC rates (29.97, 23.98, 59.94) become N*1000/1001.
fn snap_fps(f: f64) -> (u32, u32) {
    if !(1.0..=240.0).contains(&f) {
        return (30000, 1001);
    }
    let n = (f * 1.001).round();
    if (n * 1000.0 / 1001.0 - f).abs() < 0.01 {
        ((n * 1000.0) as u32, 1001)
    } else if (f - f.round()).abs() < 0.01 {
        (f.round() as u32, 1)
    } else {
        ((f * 1000.0).round() as u32, 1000)
    }
}

// ---------------------------------------------------------------------------------------------------------
// playback
// ---------------------------------------------------------------------------------------------------------

/// A decoded video frame (`width*height*4` bytes RGBA).
#[derive(Clone)]
pub struct Frame {
    /// 1-based decode index; strictly increasing.
    pub seq: u64,
    /// Presentation time in seconds (`index / fps`), monotonic across loops.
    pub pts: f64,
    pub data: Arc<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct OpenOpts {
    pub audio: AudioSel,
    /// Output device rate; the audio child resamples to it.
    pub sample_rate: u32,
    pub width: u32,
    pub height: u32,
    pub looping: bool,
}

impl Default for OpenOpts {
    fn default() -> Self {
        Self { audio: AudioSel::None, sample_rate: 48_000, width: 1280, height: 720, looping: false }
    }
}

struct Shared {
    rate: u32,
    has_audio: bool,
    stop: AtomicBool,
    slot: Mutex<Option<Frame>>,
    published: AtomicU64,
    presented: AtomicU64,
    video_eof: AtomicBool,
    tick: Mutex<()>,
    tick_cv: Condvar,
    ring: Mutex<VecDeque<f32>>,
    ring_cv: Condvar,
    buffered: AtomicUsize,
    consumed: AtomicU64,
    audio_done: AtomicBool,
    wall: Mutex<Option<(Instant, f64)>>,
    errors: Mutex<Vec<String>>,
}

impl Shared {
    fn fail(&self, msg: String) {
        lock(&self.errors).push(msg);
    }

    /// No (more) audio to follow: absent, or the audio child ended and the ring is empty.
    fn exhausted(&self) -> bool {
        !self.has_audio || (self.audio_done.load(SeqCst) && self.buffered.load(SeqCst) == 0)
    }

    /// `latch` starts the wall clock if it is in use and not started yet.
    fn clock_at(&self, latch: bool) -> f64 {
        let audio = self.consumed.load(SeqCst) as f64 / self.rate.max(1) as f64;
        if !self.exhausted() {
            return audio;
        }
        let mut w = lock(&self.wall);
        match *w {
            Some((t0, off)) => off + t0.elapsed().as_secs_f64(),
            None => {
                if latch {
                    *w = Some((Instant::now(), audio));
                }
                audio
            }
        }
    }

    fn read(&self, out: &mut [f32]) -> usize {
        let mut ring = match self.ring.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return 0,
        };
        let n = out.len().min(ring.len()) & !1;
        for (o, s) in out.iter_mut().zip(ring.drain(..n)) {
            *o = s;
        }
        if n > 0 {
            self.buffered.fetch_sub(n, SeqCst);
            self.consumed.fetch_add((n / 2) as u64, SeqCst);
            self.ring_cv.notify_one();
        }
        n
    }
}

/// Cloneable, `Send + Sync` pull handle for the audio thread.
#[derive(Clone)]
pub struct AudioSource(Arc<Shared>);

impl AudioSource {
    /// Copies up to `out.len()` interleaved stereo f32 values (always an even count) and returns how many VALUES
    /// were written (not frames); the caller zero-fills the rest. Never blocks: returns 0 on lock contention.
    /// Each value pair advances the movie clock by one frame.
    pub fn read(&self, out: &mut [f32]) -> usize {
        self.0.read(out)
    }
}

type Tail = Arc<Mutex<VecDeque<String>>>;

struct Proc {
    name: &'static str,
    child: Child,
    tail: Tail,
}

pub struct FfmpegVideo {
    sh: Arc<Shared>,
    procs: Mutex<Vec<Proc>>,
    threads: Vec<JoinHandle<()>>,
    duration: Option<f64>,
    fps: f64,
}

fn base_cmd(path: &Path, looping: bool) -> Command {
    let mut c = Command::new(ffmpeg_path());
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if looping {
        c.args(["-stream_loop", "-1"]);
    }
    c.arg("-i").arg(path);
    c
}

fn spawn_proc(mut cmd: Command, name: &'static str) -> io::Result<(Proc, ChildStdout, ChildStderr)> {
    hide_console(&mut cmd);
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    match (child.stdout.take(), child.stderr.take()) {
        (Some(o), Some(e)) => Ok((Proc { name, child, tail: Arc::default() }, o, e)),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            Err(io::Error::new(io::ErrorKind::Other, "ffmpeg pipe missing"))
        }
    }
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(name.into()).spawn(f)
}

fn drain_stderr(err: ChildStderr, tail: Tail) {
    for line in BufReader::new(err).split(b'\n').map_while(Result::ok) {
        let mut t = lock(&tail);
        if t.len() >= TAIL_LINES {
            t.pop_front();
        }
        t.push_back(String::from_utf8_lossy(&line).trim().to_string());
    }
}

fn video_loop(sh: Arc<Shared>, mut out: ChildStdout, fsize: usize, fps: f64) {
    let mut spare: Option<Arc<Vec<u8>>> = None;
    let mut idx = 0u64;
    while !sh.stop.load(SeqCst) {
        let mut data = spare.take().filter(|a| Arc::strong_count(a) == 1).unwrap_or_else(|| Arc::new(vec![0u8; fsize]));
        let Some(buf) = Arc::get_mut(&mut data) else { break };
        if let Err(e) = out.read_exact(buf) {
            if e.kind() != io::ErrorKind::UnexpectedEof && !sh.stop.load(SeqCst) {
                sh.fail(format!("video pipe: {e}"));
            }
            break;
        }
        let pts = idx as f64 / fps;
        idx += 1;
        while pts > sh.clock_at(false) + LEAD && !sh.stop.load(SeqCst) {
            let g = lock(&sh.tick);
            let _ = sh.tick_cv.wait_timeout(g, Duration::from_millis(8));
        }
        if sh.stop.load(SeqCst) {
            break;
        }
        let old = lock(&sh.slot).replace(Frame { seq: idx, pts, data });
        sh.published.store(idx, SeqCst);
        spare = old.map(|f| f.data);
    }
    sh.video_eof.store(true, SeqCst);
}

fn audio_loop(sh: Arc<Shared>, mut out: ChildStdout) {
    let cap = sh.rate as usize * 2;
    let mut buf = vec![0u8; 16384];
    let mut filled = 0;
    let mut tmp: Vec<f32> = Vec::with_capacity(4096);
    while !sh.stop.load(SeqCst) {
        let n = match out.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if !sh.stop.load(SeqCst) {
                    sh.fail(format!("audio pipe: {e}"));
                }
                break;
            }
        };
        filled += n;
        let whole = filled / 8 * 8; // whole stereo frames
        tmp.clear();
        tmp.extend(buf[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        buf.copy_within(whole..filled, 0);
        filled -= whole;
        let mut off = 0;
        let mut ring = lock(&sh.ring);
        while off < tmp.len() && !sh.stop.load(SeqCst) {
            let free = cap.saturating_sub(ring.len()) & !1;
            if free == 0 {
                ring = sh.ring_cv.wait_timeout(ring, Duration::from_millis(20)).unwrap_or_else(|e| e.into_inner()).0;
                continue;
            }
            let k = free.min(tmp.len() - off);
            ring.extend(&tmp[off..off + k]);
            sh.buffered.fetch_add(k, SeqCst);
            off += k;
        }
    }
    sh.audio_done.store(true, SeqCst);
}

impl FfmpegVideo {
    /// Probes the file (`ffmpeg -i`, ~50-100 ms, blocking) and starts the decoders. If the audio child cannot
    /// be spawned the movie plays silent on the wall clock and `error()` says why.
    pub fn open(path: &Path, o: OpenOpts) -> io::Result<Self> {
        let info = media_info(path)?;
        let (num, den) = snap_fps(info.fps.unwrap_or(30000.0 / 1001.0));
        let fps = num as f64 / den as f64;
        let (w, h) = (o.width.max(2) as usize, o.height.max(2) as usize);
        let rate = o.sample_rate.max(8000);
        let mut errors = Vec::new();

        let audio = match o.audio {
            AudioSel::None => None,
            sel => {
                let map = match sel {
                    AudioSel::Stream(i) => format!("0:{i}"),
                    _ => "0:a:0?".to_string(),
                };
                let mut c = base_cmd(path, o.looping);
                c.args(["-map", map.as_str(), "-vn", "-ac", "2", "-ar", rate.to_string().as_str(), "-f", "f32le", "pipe:1"]);
                match spawn_proc(c, "audio") {
                    Ok(a) => Some(a),
                    Err(e) => {
                        errors.push(format!("audio ffmpeg spawn: {e}"));
                        None
                    }
                }
            }
        };

        let mut v = base_cmd(path, o.looping);
        v.args(["-map", "0:v:0", "-an", "-r", format!("{num}/{den}").as_str(), "-f", "rawvideo", "-pix_fmt", "rgba", "-s"])
            .arg(format!("{w}x{h}"))
            .arg("pipe:1");
        let video = match spawn_proc(v, "video") {
            Ok(v) => v,
            Err(e) => {
                if let Some((mut a, ..)) = audio {
                    let _ = a.child.kill();
                    let _ = a.child.wait();
                }
                return Err(e);
            }
        };

        let sh = Arc::new(Shared {
            rate,
            has_audio: audio.is_some(),
            stop: AtomicBool::new(false),
            slot: Mutex::new(None),
            published: AtomicU64::new(0),
            presented: AtomicU64::new(0),
            video_eof: AtomicBool::new(false),
            tick: Mutex::new(()),
            tick_cv: Condvar::new(),
            ring: Mutex::new(VecDeque::new()),
            ring_cv: Condvar::new(),
            buffered: AtomicUsize::new(0),
            consumed: AtomicU64::new(0),
            audio_done: AtomicBool::new(false),
            wall: Mutex::new(None),
            errors: Mutex::new(errors),
        });
        let mut me = FfmpegVideo { sh: sh.clone(), procs: Mutex::new(Vec::new()), threads: Vec::new(), duration: info.duration, fps };

        let (vp, vout, verr) = video;
        let vtail = vp.tail.clone();
        lock(&me.procs).push(vp);
        me.threads.push(spawn_thread("fmv-verr", move || drain_stderr(verr, vtail))?);
        let s = sh.clone();
        me.threads.push(spawn_thread("fmv-video", move || video_loop(s, vout, w * h * 4, fps))?);

        if let Some((ap, aout, aerr)) = audio {
            let atail = ap.tail.clone();
            lock(&me.procs).push(ap);
            me.threads.push(spawn_thread("fmv-aerr", move || drain_stderr(aerr, atail))?);
            let s = sh.clone();
            me.threads.push(spawn_thread("fmv-audio", move || audio_loop(s, aout))?);
        }
        Ok(me)
    }

    /// Stereo f32 pull handle for the audio thread.
    pub fn audio_source(&self) -> AudioSource {
        AudioSource(self.sh.clone())
    }

    /// Same as [`AudioSource::read`].
    pub fn read_audio(&self, out: &mut [f32]) -> usize {
        self.sh.read(out)
    }

    /// Movie time in seconds (audio frames pulled / rate, or the wall clock; see crate docs).
    pub fn clock(&self) -> f64 {
        self.sh.clock_at(true)
    }

    /// The newest decoded frame that is due and was not returned before.
    pub fn frame(&self) -> Option<Frame> {
        let clock = self.sh.clock_at(true);
        let f = lock(&self.sh.slot).clone()?;
        if f.seq <= self.sh.presented.load(SeqCst) || f.pts > clock + LEAD {
            return None;
        }
        self.sh.presented.store(f.seq, SeqCst);
        Some(f)
    }

    /// Video ended, its last frame was returned by `frame()`, and audio is drained or absent. Never for looping.
    pub fn finished(&self) -> bool {
        let s = &self.sh;
        s.video_eof.load(SeqCst) && s.published.load(SeqCst) == s.presented.load(SeqCst) && s.exhausted()
    }

    /// Container duration in seconds, if ffmpeg reported one.
    pub fn duration(&self) -> Option<f64> {
        self.duration
    }

    /// Output frame rate used for pts.
    pub fn fps(&self) -> f64 {
        self.fps
    }

    /// First recorded I/O error, or a non-zero ffmpeg exit with the last stderr lines.
    pub fn error(&self) -> Option<String> {
        if let Some(e) = lock(&self.sh.errors).first() {
            return Some(e.clone());
        }
        if self.sh.stop.load(SeqCst) {
            return None;
        }
        for p in lock(&self.procs).iter_mut() {
            if let Ok(Some(st)) = p.child.try_wait() {
                if !st.success() {
                    let tail: Vec<String> = lock(&p.tail).iter().cloned().collect();
                    return Some(format!("ffmpeg {} exited with {st}: {}", p.name, tail.join(" | ")));
                }
            }
        }
        None
    }
}

impl Drop for FfmpegVideo {
    fn drop(&mut self) {
        self.sh.stop.store(true, SeqCst);
        self.sh.tick_cv.notify_all();
        self.sh.ring_cv.notify_all();
        for p in lock(&self.procs).iter_mut() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FMV01: &str = "<xml>\n<StreamDefinition>\n<Audio id=\"6\" lang=\"EN\"/>\n<Audio id=\"1\" lang=\"FR\"/>\n<Audio id=\"2\" lang=\"DE\"/>\n<Audio id=\"3\" lang=\"IT\"/>\n<Audio id=\"4\" lang=\"ES\"/>\n<Audio id=\"5\" lang=\"NL\"/>\n<Video id=\"7\" />\n</StreamDefinition>\n</xml>";
    const PRESS: &str = "<xml>\n<StreamDefinition>\n<Video id=\"2\" />\n</StreamDefinition>\n</xml>";

    #[test]
    fn def_fmv01() {
        let d = StreamDef::parse(FMV01);
        assert_eq!(d.video, Some(7));
        assert_eq!(d.audio.len(), 6);
        assert_eq!(d.audio[0], (6, "EN".to_string()));
        assert_eq!(d.audio[5], (5, "NL".to_string()));
    }

    #[test]
    fn def_press_start() {
        let d = StreamDef::parse(PRESS);
        assert_eq!((d.video, d.audio.len()), (Some(2), 0));
        assert_eq!(d.audio_stream("EN"), None);
        assert_eq!(AudioSel::choose(Some(&d), "EN"), AudioSel::FirstAudio);
        assert_eq!(AudioSel::choose(None, "EN"), AudioSel::FirstAudio);
    }

    #[test]
    fn language_mapping() {
        let d = StreamDef::parse(FMV01);
        assert_eq!(d.audio_stream("FR"), Some(0));
        assert_eq!(d.audio_stream("JP"), Some(5));
        assert_eq!(d.audio_stream("MX"), Some(3));
        assert_eq!(d.audio_stream("GB"), Some(5));
        assert_eq!(d.audio_stream("nl"), Some(4));
        assert_eq!(AudioSel::choose(Some(&d), "DE"), AudioSel::Stream(1));
        assert_eq!(audio_lang_for("CHT"), "EN");
    }

    #[test]
    fn stderr_info() {
        let s = "Input #0, asf, from 'FMV_01.wmv':\n  Duration: 00:02:15.98, start: 0.000000, bitrate: 10022 kb/s\n  Stream #0:0(fre): Audio: wmapro (b[1][0][0] / 0x0162), 48000 Hz, 5.1, fltp, 384 kb/s\n  Stream #0:6(eng): Video: wmv3 (Main) (WMV3 / 0x33564D57), yuv420p, 1280x720, 8000 kb/s, 29.97 fps, 29.97 tbr, 1k tbn\nAt least one output file must be specified\n";
        let i = parse_media_info(s);
        assert!((i.duration.unwrap() - 135.98).abs() < 1e-6);
        assert_eq!(i.fps, Some(29.97));
        let d = "  Duration: 00:00:09.05, start: 0.000000, bitrate: 1644 kb/s\n  Stream #0:1(eng): Video: wmv3 (Main) (WMV3 / 0x33564D57), yuv420p, 1280x720, 8000 kb/s, SAR 1:1 DAR 16:9, 23.98 fps, 23.98 tbr, 1k tbn\n";
        let i = parse_media_info(d);
        assert_eq!(i.fps, Some(23.98));
        assert!((i.duration.unwrap() - 9.05).abs() < 1e-6);
        assert_eq!(parse_media_info("nothing"), MediaInfo::default());
    }

    #[test]
    fn fps_snap() {
        assert_eq!(snap_fps(29.97), (30000, 1001));
        assert_eq!(snap_fps(23.98), (24000, 1001));
        assert_eq!(snap_fps(30.0), (30, 1));
        assert_eq!(snap_fps(0.0), (30000, 1001));
    }

    #[test]
    fn decoder_check() {
        let t = " V....D wmv3                 Windows Media Video 9\n A....D wmapro               x\n A....D wmav2 y\n";
        assert!(check_decoders(t).is_ok());
        assert!(check_decoders(" V....D wmv3 x\n").unwrap_err().contains("wmapro"));
    }

    #[test]
    fn assert_send_sync() {
        fn is<T: Send + Sync>() {}
        is::<AudioSource>();
        is::<FfmpegVideo>();
    }
}
