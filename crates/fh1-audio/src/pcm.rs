//! One shared cpal output stream mixing software voices (decoded [`Pcm`] clips: UI, ambience, sfx)
//! and pull streams ([`PcmSource`], FMV audio).
//!
//! [`shared`] opens the stream on first use (same device rules as [`crate::output`]: Windows'
//! default followed live, or `FH1_AUDIO_DEVICE`), `FH1_PCM=0` turns it off. The mixing core
//! ([`Mixer`]) is device-independent: voices are resampled linearly to the device rate, gain / pan /
//! bus gain are ramped across each 256-frame block, the sum is hard-clamped to -1..1.
//!
//! API calls push commands onto a queue that the audio callback drains at each block start
//! (`try_lock`: if contended, the next block); the callback never blocks. When the device changes
//! the stream re-opens on the new one: voices survive, pull streams are dropped.
//!
//! Debug: `FH1_PCM_CAPTURE=<file>` writes the stereo mix as raw s16le at the device rate.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};

use crate::ambient::AmbientMixer;
use crate::output::{device, device_name};
use crate::ui_sfx::UiSfxPlayer;

/// Render block: parameters are re-evaluated this often (~5 ms at 48 kHz).
const BLOCK: usize = 256;

/// Interleaved f32 samples in -1..1, mono or stereo.
pub struct Pcm {
    pub rate: u32,
    pub channels: u16,
    pub data: Arc<[f32]>,
}

impl Pcm {
    pub fn load_wav(path: &Path) -> Result<Arc<Pcm>> {
        let (rate, channels, data) = crate::wav::read(path)?;
        if channels == 0 || channels > 2 {
            bail!("{}: {channels} channels (only mono / stereo)", path.display());
        }
        Ok(Arc::new(Pcm { rate, channels, data: data.into() }))
    }

    pub fn frames(&self) -> usize {
        self.data.len() / self.channels.max(1) as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VoiceId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StreamId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Bus {
    Ui,
    Fmv,
    Ambience,
    Sfx,
}

impl Bus {
    fn idx(self) -> usize {
        match self {
            Bus::Ui => 0,
            Bus::Fmv => 1,
            Bus::Ambience => 2,
            Bus::Sfx => 3,
        }
    }

    /// Most voices at once on the bus.
    fn cap(self) -> usize {
        match self {
            Bus::Ui => 12,
            Bus::Fmv => 4,
            Bus::Ambience => 48,
            Bus::Sfx => 16,
        }
    }
}

/// Most pull streams at once.
const MAX_STREAMS: usize = 2;

#[derive(Clone, Copy, Debug)]
pub struct VoiceParams {
    pub looped: bool,
    pub gain: f32,
    /// -1 (left) .. 1 (right).
    pub pan: f32,
    /// Playback-rate multiplier.
    pub pitch: f32,
    pub start_frame: usize,
    pub bus: Bus,
    pub loop_start: usize,
    /// Exclusive frame; `None` = end of data.
    pub loop_end: Option<usize>,
}

impl Default for VoiceParams {
    fn default() -> Self {
        VoiceParams { looped: false, gain: 1.0, pan: 0.0, pitch: 1.0, start_frame: 0, bus: Bus::Sfx, loop_start: 0, loop_end: None }
    }
}

/// Pull source for [`PcmOutput::play_stream`].
pub trait PcmSource: Send + Sync {
    /// Fill `out` with interleaved STEREO f32 at the device rate; returns the number of f32 values
    /// written (the rest is zero-filled). Called from the audio callback: must never block.
    fn read(&self, out: &mut [f32]) -> usize;
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// (left, right) gain for a source with `channels` at `pan`. Mono: equal power with a centred
/// voice at unity per channel; stereo: balance.
fn pan_gains(channels: usize, pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    if channels == 1 {
        let a = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        (a.cos() * std::f32::consts::SQRT_2, a.sin() * std::f32::consts::SQRT_2)
    } else {
        (if pan > 0.0 { 1.0 - pan } else { 1.0 }, if pan < 0.0 { 1.0 + pan } else { 1.0 })
    }
}

// ---------------------------------------------------------------------------------------------
// Mixer (device-independent)
// ---------------------------------------------------------------------------------------------

/// A command for the mixer, queued by the API and applied at a block start.
pub(crate) enum Cmd {
    Play { id: u32, pcm: Arc<Pcm>, p: VoiceParams },
    Set { id: u32, gain: f32, pan: f32, pitch: f32 },
    Stop { id: u32, fade_s: f32 },
    BusGain(Bus, f32),
    PlayStream { id: u32, src: Arc<dyn PcmSource>, gain: f32 },
    SetStream { id: u32, gain: f32 },
    StopStream { id: u32 },
    DropStreams,
}

struct Voice {
    id: u32,
    pcm: Arc<Pcm>,
    looped: bool,
    bus: Bus,
    loop_start: usize,
    loop_end: Option<usize>,
    /// Position in source frames.
    pos: f64,
    gain: f32,
    pan: f32,
    pitch: f32,
    /// Gain / pan of the last block (ramped to the targets across the next one).
    gain_prev: f32,
    pan_prev: f32,
    /// Fade-out: total and remaining seconds (`fade_total` 0 = not fading).
    fade_total: f32,
    fade_left: f32,
}

struct Stream {
    id: u32,
    src: Arc<dyn PcmSource>,
    gain: f32,
    gain_prev: f32,
    stopping: bool,
}

pub(crate) struct Mixer {
    voices: Vec<Voice>,
    streams: Vec<Stream>,
    bus_gain: [f32; 4],
    bus_prev: [f32; 4],
    /// Ids that ended (or were dropped) since the last [`Mixer::take_finished`].
    finished: Vec<u32>,
    buf: Vec<f32>,
}

impl Mixer {
    pub(crate) fn new() -> Mixer {
        Mixer { voices: Vec::new(), streams: Vec::new(), bus_gain: [1.0; 4], bus_prev: [1.0; 4], finished: Vec::new(), buf: vec![0.0; BLOCK * 2] }
    }

    pub(crate) fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play { id, pcm, p } => {
                let pan = p.pan.clamp(-1.0, 1.0);
                if pcm.frames() == 0 {
                    self.finished.push(id);
                    return;
                }
                self.voices.push(Voice {
                    id,
                    pcm,
                    looped: p.looped,
                    bus: p.bus,
                    loop_start: p.loop_start,
                    loop_end: p.loop_end,
                    pos: p.start_frame as f64,
                    gain: p.gain,
                    pan,
                    pitch: p.pitch,
                    gain_prev: p.gain,
                    pan_prev: pan,
                    fade_total: 0.0,
                    fade_left: 0.0,
                });
            }
            Cmd::Set { id, gain, pan, pitch } => {
                if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
                    v.gain = gain;
                    v.pan = pan.clamp(-1.0, 1.0);
                    v.pitch = pitch;
                }
            }
            Cmd::Stop { id, fade_s } => {
                if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
                    let f = fade_s.max(0.005);
                    // A second stop can only shorten the fade.
                    if v.fade_total == 0.0 || f < v.fade_left {
                        v.fade_total = f;
                        v.fade_left = f;
                    }
                }
            }
            Cmd::BusGain(b, g) => self.bus_gain[b.idx()] = g.max(0.0),
            Cmd::PlayStream { id, src, gain } => self.streams.push(Stream { id, src, gain, gain_prev: gain, stopping: false }),
            Cmd::SetStream { id, gain } => {
                if let Some(s) = self.streams.iter_mut().find(|s| s.id == id) {
                    s.gain = gain;
                }
            }
            Cmd::StopStream { id } => {
                if let Some(s) = self.streams.iter_mut().find(|s| s.id == id) {
                    s.stopping = true;
                }
            }
            Cmd::DropStreams => {
                for s in self.streams.drain(..) {
                    self.finished.push(s.id);
                }
            }
        }
    }

    /// Moves the ids that ended since the last call into `out`.
    pub(crate) fn take_finished(&mut self, out: &mut Vec<u32>) {
        out.append(&mut self.finished);
    }

    pub(crate) fn voice_count(&self) -> usize {
        self.voices.len()
    }

    /// Renders `out_stereo` (interleaved L R) at `device_rate`, in blocks of up to [`BLOCK`] frames.
    pub fn render(&mut self, out_stereo: &mut [f32], device_rate: u32) {
        for chunk in out_stereo.chunks_mut(BLOCK * 2) {
            self.render_block(chunk, device_rate);
        }
    }

    fn render_block(&mut self, out: &mut [f32], device_rate: u32) {
        let n = out.len() / 2;
        if n == 0 {
            return;
        }
        out.fill(0.0);
        let dev = device_rate.max(1) as f64;
        let secs = n as f32 / dev as f32;
        let (bp, bt) = (self.bus_prev, self.bus_gain);

        let mut i = 0;
        while i < self.voices.len() {
            let b = self.voices[i].bus.idx();
            if self.voices[i].render(out, dev, secs, bp[b], bt[b]) {
                let v = self.voices.swap_remove(i);
                self.finished.push(v.id);
            } else {
                i += 1;
            }
        }

        let mut i = 0;
        let (f0, f1) = (bp[Bus::Fmv.idx()], bt[Bus::Fmv.idx()]);
        while i < self.streams.len() {
            let s = &mut self.streams[i];
            self.buf.resize(n * 2, 0.0);
            self.buf.fill(0.0);
            let got = s.src.read(&mut self.buf).min(n * 2);
            self.buf[got..].fill(0.0);
            let g1 = if s.stopping { 0.0 } else { s.gain };
            let g0 = s.gain_prev;
            for k in 0..n {
                let t = k as f32 / n as f32;
                let g = lerp(g0, g1, t) * lerp(f0, f1, t);
                out[2 * k] += self.buf[2 * k] * g;
                out[2 * k + 1] += self.buf[2 * k + 1] * g;
            }
            s.gain_prev = g1;
            if s.stopping {
                let s = self.streams.swap_remove(i);
                self.finished.push(s.id);
            } else {
                i += 1;
            }
        }

        self.bus_prev = bt;
        for x in out.iter_mut() {
            *x = x.clamp(-1.0, 1.0);
        }
    }
}

impl Voice {
    /// Adds this voice's next `out.len() / 2` frames to `out`; true = the voice is done.
    fn render(&mut self, out: &mut [f32], dev: f64, secs: f32, bus0: f32, bus1: f32) -> bool {
        let n = out.len() / 2;
        let frames = self.pcm.frames();
        let ch = self.pcm.channels as usize;
        let data = &self.pcm.data;
        let (g0, g1) = (self.gain_prev, self.gain);
        let (l0, r0) = pan_gains(ch, self.pan_prev);
        let (l1, r1) = pan_gains(ch, self.pan);
        let (fa, fb) = if self.fade_total > 0.0 {
            ((self.fade_left / self.fade_total).max(0.0), ((self.fade_left - secs) / self.fade_total).max(0.0))
        } else {
            (1.0, 1.0)
        };
        let step = self.pcm.rate as f64 / dev * self.pitch.max(0.01) as f64;
        // Loop region [ls, le).
        let (mut ls, mut le) = (self.loop_start, self.loop_end.unwrap_or(frames).min(frames));
        if ls >= le {
            ls = 0;
            le = frames;
        }
        if self.looped {
            if self.pos >= le as f64 {
                let len = (le - ls) as f64;
                self.pos = ls as f64 + (self.pos - ls as f64).rem_euclid(len);
            }
        } else if self.pos >= frames as f64 {
            return true;
        }
        let mut done = false;
        for k in 0..n {
            let t = k as f32 / n as f32;
            let gain = lerp(g0, g1, t) * lerp(bus0, bus1, t) * lerp(fa, fb, t);
            let i0 = self.pos as usize;
            let fr = (self.pos - i0 as f64) as f32;
            let mut i1 = i0 + 1;
            if self.looped {
                if i1 >= le {
                    i1 = ls;
                }
            } else if i1 >= frames {
                i1 = i0;
            }
            let (sl, sr) = if ch == 1 {
                let s = lerp(data[i0], data[i1], fr);
                (s, s)
            } else {
                (lerp(data[2 * i0], data[2 * i1], fr), lerp(data[2 * i0 + 1], data[2 * i1 + 1], fr))
            };
            out[2 * k] += sl * gain * lerp(l0, l1, t);
            out[2 * k + 1] += sr * gain * lerp(r0, r1, t);
            self.pos += step;
            if self.looped {
                let len = (le - ls) as f64;
                while self.pos >= le as f64 {
                    self.pos -= len;
                }
            } else if self.pos >= frames as f64 {
                done = true;
                break;
            }
        }
        self.gain_prev = g1;
        self.pan_prev = self.pan;
        if self.fade_total > 0.0 {
            self.fade_left -= secs;
            if self.fade_left <= 0.0 {
                done = true;
            }
        }
        done
    }
}

// ---------------------------------------------------------------------------------------------
// Shared output
// ---------------------------------------------------------------------------------------------

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Shared {
    cmds: Mutex<Vec<Cmd>>,
    /// Live ids -> (bus, is a pull stream).
    live: Mutex<HashMap<u32, (Bus, bool)>>,
    next_id: AtomicU32,
    /// Bus gains as f32 bits (the mixer has its own copy, set through the command queue).
    bus_gain: [AtomicU32; 4],
    rate: AtomicU32,
    device: Mutex<String>,
    /// Set by the stream's error callback (device unplugged etc.).
    broken: AtomicBool,
}

impl Shared {
    fn push(&self, c: Cmd) {
        lock(&self.cmds).push(c);
    }

    /// All pull streams are dropped (device changed): never resumed.
    fn drop_streams(&self) {
        self.push(Cmd::DropStreams);
        lock(&self.live).retain(|_, v| !v.1);
    }
}

pub struct PcmOutput {
    shared: Arc<Shared>,
}

static SHARED: OnceLock<Option<PcmOutput>> = OnceLock::new();

/// The process-wide output; the first call opens the stream. `None` if `FH1_PCM=0`, there is no
/// device or opening failed (logged once).
pub fn shared() -> Option<&'static PcmOutput> {
    SHARED
        .get_or_init(|| {
            if std::env::var("FH1_PCM").is_ok_and(|s| s == "0") {
                return None;
            }
            match PcmOutput::start() {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!("fh1-audio pcm: {e:#}");
                    None
                }
            }
        })
        .as_ref()
}

pub fn device_rate() -> Option<u32> {
    shared().map(|p| p.device_rate())
}

impl PcmOutput {
    fn start() -> Result<PcmOutput> {
        let shared = Arc::new(Shared {
            cmds: Mutex::new(Vec::new()),
            live: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            bus_gain: [(); 4].map(|_| AtomicU32::new(1.0f32.to_bits())),
            rate: AtomicU32::new(0),
            device: Mutex::new(String::new()),
            broken: AtomicBool::new(false),
        });
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(u32, String)>>();
        {
            let shared = shared.clone();
            std::thread::Builder::new().name("fh1-pcm".into()).spawn(move || {
                let (mut stream, mut name) = match open_stream(&shared, &mixer) {
                    Ok((s, rate, name)) => {
                        shared.rate.store(rate, Ordering::Relaxed);
                        let _ = tx.send(Ok((rate, name.clone())));
                        (Some(s), name)
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let pinned = std::env::var("FH1_AUDIO_DEVICE").is_ok_and(|s| !s.is_empty());
                let mut last_check = std::time::Instant::now();
                // Lives for the whole process (the output is 'static), so no stop flag.
                loop {
                    std::thread::park_timeout(std::time::Duration::from_millis(100));
                    if last_check.elapsed().as_secs_f32() < 1.0 {
                        continue;
                    }
                    last_check = std::time::Instant::now();
                    let moved = !pinned && device().ok().map(|d| device_name(&d)).is_some_and(|d| d != name);
                    if moved || shared.broken.swap(false, Ordering::Relaxed) {
                        drop(stream.take());
                        shared.drop_streams();
                        match open_stream(&shared, &mixer) {
                            Ok((s, rate, n)) => {
                                eprintln!("fh1-audio pcm: output moved to \"{n}\" ({rate} Hz)");
                                shared.rate.store(rate, Ordering::Relaxed);
                                *lock(&shared.device) = n.clone();
                                stream = Some(s);
                                name = n;
                            }
                            Err(e) => eprintln!("fh1-audio pcm: reopening output failed: {e:#}"),
                        }
                    }
                }
            })?;
        }
        let (_rate, name) = rx.recv().context("pcm thread died")??;
        *lock(&shared.device) = name;
        Ok(PcmOutput { shared })
    }

    /// Current stream's sample rate (changes if the output moves to another device).
    pub fn device_rate(&self) -> u32 {
        self.shared.rate.load(Ordering::Relaxed)
    }

    pub fn device(&self) -> String {
        lock(&self.shared.device).clone()
    }

    fn new_id(&self) -> u32 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Starts a voice; `None` if the bus is at its cap (or the clip is empty).
    pub fn play(&self, pcm: Arc<Pcm>, p: VoiceParams) -> Option<VoiceId> {
        if pcm.frames() == 0 {
            return None;
        }
        let id = self.new_id();
        {
            let mut live = lock(&self.shared.live);
            if live.values().filter(|v| v.0 == p.bus && !v.1).count() >= p.bus.cap() {
                return None;
            }
            live.insert(id, (p.bus, false));
        }
        self.shared.push(Cmd::Play { id, pcm, p });
        Some(VoiceId(id))
    }

    pub fn set(&self, id: VoiceId, gain: f32, pan: f32, pitch: f32) {
        self.shared.push(Cmd::Set { id: id.0, gain, pan, pitch });
    }

    /// Fades out over `fade_s` (min ~5 ms, so it never clicks) and removes the voice.
    pub fn stop(&self, id: VoiceId, fade_s: f32) {
        self.shared.push(Cmd::Stop { id: id.0, fade_s });
    }

    /// One-shots end on their own.
    pub fn is_playing(&self, id: VoiceId) -> bool {
        lock(&self.shared.live).contains_key(&id.0)
    }

    pub fn set_bus_gain(&self, bus: Bus, gain: f32) {
        let gain = gain.max(0.0);
        self.shared.bus_gain[bus.idx()].store(gain.to_bits(), Ordering::Relaxed);
        self.shared.push(Cmd::BusGain(bus, gain));
    }

    pub fn bus_gain(&self, bus: Bus) -> f32 {
        f32::from_bits(self.shared.bus_gain[bus.idx()].load(Ordering::Relaxed))
    }

    /// Pull stream on [`Bus::Fmv`] (max 2; beyond that the returned id is never alive).
    pub fn play_stream(&self, src: Arc<dyn PcmSource>, gain: f32) -> StreamId {
        let id = self.new_id();
        {
            let mut live = lock(&self.shared.live);
            if live.values().filter(|v| v.1).count() >= MAX_STREAMS {
                return StreamId(id);
            }
            live.insert(id, (Bus::Fmv, true));
        }
        self.shared.push(Cmd::PlayStream { id, src, gain });
        StreamId(id)
    }

    pub fn set_stream_gain(&self, id: StreamId, gain: f32) {
        self.shared.push(Cmd::SetStream { id: id.0, gain });
    }

    pub fn stop_stream(&self, id: StreamId) {
        lock(&self.shared.live).remove(&id.0);
        self.shared.push(Cmd::StopStream { id: id.0 });
    }

    /// False after [`Self::stop_stream`] or after the device changed (streams are dropped then, never resumed).
    pub fn stream_alive(&self, id: StreamId) -> bool {
        lock(&self.shared.live).get(&id.0).is_some_and(|v| v.1)
    }

    pub fn ambient(&'static self) -> AmbientMixer {
        AmbientMixer::new(self)
    }

    pub fn ui(&'static self) -> UiSfxPlayer {
        UiSfxPlayer::new(self)
    }
}

fn open_stream(shared: &Arc<Shared>, mixer: &Arc<Mutex<Mixer>>) -> Result<(cpal::Stream, u32, String)> {
    let device = device()?;
    let supported = device.default_output_config()?;
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return Err(anyhow!("output device format {:?} (only f32 supported)", supported.sample_format()));
    }
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate as u32;
    let channels = config.channels as usize;
    let (shared_cb, shared_err) = (shared.clone(), shared.clone());
    let mixer = mixer.clone();
    let mut stereo = vec![0f32; BLOCK * 2];
    // Next frame of `stereo` to hand to the device; BLOCK = render a new block.
    let mut cursor = BLOCK;
    let mut cmds: Vec<Cmd> = Vec::new();
    let mut pending: Vec<u32> = Vec::new();
    let mut capture = std::env::var_os("FH1_PCM_CAPTURE").and_then(|p| {
        eprintln!("fh1-audio pcm: capturing to {} (s16le stereo {rate} Hz)", p.to_string_lossy());
        std::fs::File::create(p).ok().map(std::io::BufWriter::new)
    });
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _| {
            for out in data.chunks_mut(channels) {
                if cursor == BLOCK {
                    match mixer.try_lock() {
                        Ok(mut m) => {
                            if let Ok(mut q) = shared_cb.cmds.try_lock() {
                                cmds.append(&mut q);
                            }
                            for c in cmds.drain(..) {
                                m.apply(c);
                            }
                            m.render(&mut stereo, rate);
                            m.take_finished(&mut pending);
                        }
                        Err(_) => stereo.fill(0.0),
                    }
                    if !pending.is_empty() {
                        if let Ok(mut live) = shared_cb.live.try_lock() {
                            for id in pending.drain(..) {
                                live.remove(&id);
                            }
                        }
                    }
                    cursor = 0;
                    if let Some(f) = &mut capture {
                        use std::io::Write;
                        let bytes: Vec<u8> = stereo.iter().flat_map(|x| ((x * 32767.0) as i16).to_le_bytes()).collect();
                        let _ = f.write_all(&bytes).and_then(|_| f.flush());
                    }
                }
                out.fill(0.0);
                out[0] = stereo[2 * cursor];
                if channels > 1 {
                    out[1] = stereo[2 * cursor + 1];
                }
                cursor += 1;
            }
        },
        move |e| {
            eprintln!("fh1-audio pcm: stream error: {e}");
            shared_err.broken.store(true, Ordering::Relaxed);
        },
        None,
    )?;
    stream.play()?;
    Ok((stream, rate, device_name(&device)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn mono(rate: u32, data: Vec<f32>) -> Arc<Pcm> {
        Arc::new(Pcm { rate, channels: 1, data: data.into() })
    }

    fn play(m: &mut Mixer, id: u32, pcm: Arc<Pcm>, p: VoiceParams) {
        m.apply(Cmd::Play { id, pcm, p });
    }

    #[test]
    fn one_shot_plays_every_frame_then_silence() {
        let n = 4800;
        let sine: Vec<f32> = (0..n).map(|k| (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / 48000.0).sin() * 0.5).collect();
        let mut m = Mixer::new();
        play(&mut m, 1, mono(48000, sine.clone()), VoiceParams::default());
        let mut out = vec![0f32; 2 * (n + 1000)];
        m.render(&mut out, 48000);
        for k in 0..n {
            assert!((out[2 * k] - sine[k]).abs() < 1e-4, "L frame {k}");
            assert!((out[2 * k + 1] - sine[k]).abs() < 1e-4, "R frame {k}");
        }
        assert!(out[2 * n..].iter().all(|&x| x == 0.0));
        assert_eq!(m.voice_count(), 0);
        let mut fin = Vec::new();
        m.take_finished(&mut fin);
        assert_eq!(fin, vec![1]);
    }

    #[test]
    fn resampling_doubles_length() {
        let n = 2400;
        let mut m = Mixer::new();
        play(&mut m, 1, mono(24000, vec![0.5; n]), VoiceParams::default());
        let mut out = vec![0f32; 2 * 6000];
        m.render(&mut out, 48000);
        let played = out.chunks(2).filter(|f| f[0] > 0.1).count();
        assert!((played as i64 - 2 * n as i64).abs() <= 2, "played {played}");
    }

    #[test]
    fn pull_stream_passes_through_and_counts() {
        struct Ramp(AtomicUsize);
        impl PcmSource for Ramp {
            fn read(&self, out: &mut [f32]) -> usize {
                let base = self.0.fetch_add(out.len(), Ordering::Relaxed);
                for (i, x) in out.iter_mut().enumerate() {
                    *x = (base + i) as f32 * 1e-5;
                }
                out.len()
            }
        }
        let src = Arc::new(Ramp(AtomicUsize::new(0)));
        let mut m = Mixer::new();
        m.apply(Cmd::PlayStream { id: 1, src: src.clone(), gain: 1.0 });
        let frames = 4 * BLOCK;
        let mut out = vec![0f32; 2 * frames];
        m.render(&mut out, 48000);
        assert_eq!(src.0.load(Ordering::Relaxed), 2 * frames);
        for (j, x) in out.iter().enumerate() {
            assert_eq!(*x, j as f32 * 1e-5, "sample {j}");
        }
    }

    #[test]
    fn loop_wraps() {
        let data: Vec<f32> = (0..10).map(|k| k as f32 / 10.0).collect();
        let mut m = Mixer::new();
        let p = VoiceParams { looped: true, loop_start: 2, loop_end: Some(6), ..Default::default() };
        play(&mut m, 1, mono(48000, data), p);
        let mut out = vec![0f32; 2 * 20];
        m.render(&mut out, 48000);
        let idx = [0, 1, 2, 3, 4, 5, 2, 3, 4, 5, 2, 3, 4, 5, 2, 3, 4, 5, 2, 3];
        for (k, i) in idx.iter().enumerate() {
            assert!((out[2 * k] - *i as f32 / 10.0).abs() < 1e-4, "frame {k}: {}", out[2 * k]);
        }
        assert_eq!(m.voice_count(), 1);
    }

    #[test]
    fn bus_gain_zero_silences() {
        let mut m = Mixer::new();
        m.apply(Cmd::BusGain(Bus::Sfx, 0.0));
        let p = VoiceParams { looped: true, ..Default::default() };
        play(&mut m, 1, mono(48000, vec![0.8; 1000]), p);
        let mut out = vec![0f32; 2 * BLOCK];
        m.render(&mut out, 48000); // ramps down
        m.render(&mut out, 48000);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(m.voice_count(), 1);
    }
}
