//! Real-time car sound: renders stereo f32 from a [`CarInput`] snapshot, device-independent
//! (the `output` module plays it; tests render it offline).
//!
//! Engine: per emitter (intake, engine ambient, exhaust L/R) the HT's RPM loops play at
//! pitch = rpm / rpm_sample and crossfade (equal power) over the zones where neighbouring loops'
//! [rpm_min, rpm_max] overlap, then go through the ET's curve-driven lowpass and peaking EQs.
//! UNVERIFIED: the crossfade shape and EQ units (gain = linear factor, bandwidth = octaves)
//! are our reading of the data; the real rules live in the FMOD `.fev` projects / default.xex.
//!
//! Road/other: tyre roll and skid layers chosen by surface group, wind, transmission whine,
//! shift clunks, turbo spool + blow-off. STOPGAP: driven by hand-picked thresholds on sample
//! names until `Tires.fev` etc. are parsed.

use std::collections::HashMap;
use std::f32::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::install::BankInfo;
use crate::tuning::{CarAudio, Curve, Drive, Emitter, Loop, Peq, ViewMix};

// ---------------------------------------------------------------- inputs

/// Tyre sound group, from `tires.json` (physics surface name → group).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TyreGroup {
    #[default]
    OnRoad,
    OffRoad,
    Grass,
    Brick,
    Trackway,
}

impl TyreGroup {
    pub fn from_name(group: &str) -> TyreGroup {
        match group.to_ascii_lowercase().as_str() {
            "offroad" => TyreGroup::OffRoad,
            "grass" => TyreGroup::Grass,
            "brick" => TyreGroup::Brick,
            "trackway" => TyreGroup::Trackway,
            _ => TyreGroup::OnRoad,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WheelInput {
    /// (wheel speed - ground speed) / ground speed, signed.
    pub slip_ratio: f32,
    pub slip_angle_deg: f32,
    /// Vertical load in newtons; 0 = in the air.
    pub load: f32,
    pub surface: TyreGroup,
    /// Combined normalised slip from the tyre model (1 = this tyre's grip peak at its load; the game's wheel+0x25C ρ).
    /// When given, it drives the skid layers instead of the fixed degree / ratio thresholds.
    pub norm_slip: Option<f32>,
}

/// Camera, selects the CMT mix (`Front` = bumper/hood, `Follow` = chase, `Cockpit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    Front,
    #[default]
    Follow,
    Cockpit,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CarInput {
    pub rpm: f32,
    /// 0..1.
    pub throttle: f32,
    /// Engine torque / peak torque: positive when driving, negative on overrun. `None` →
    /// estimated from throttle and rpm.
    pub torque: Option<f32>,
    /// 0 = neutral, -1 = reverse.
    pub gear: i32,
    /// Increments on every gear change; a change plays a shift clunk.
    pub shifts: u32,
    /// m/s.
    pub speed: f32,
    /// Turbo/supercharger boost, 0..1 of maximum.
    pub boost: f32,
    pub has_turbo: bool,
    pub wheels: [WheelInput; 4],
    pub view: View,
    /// Master volume, 0..1.
    pub volume: f32,
    /// Pitch multiplier on the engine loops (doppler of another car), 0 = 1.
    pub pitch: f32,
    /// Another car heard from outside (AI / traffic): engine, transmission, turbo and shifts only; no tyres, chassis
    /// rattle or wind (those are the listener's own car).
    pub external: bool,
    /// Rev limiter rpm (0 = unknown): at the limiter with the throttle down the engine "bounces" (ignition cuts).
    pub rpm_limit: f32,
}

/// A pop or backfire bang the synth just played, for the exhaust flame visuals (engine backfire.rs).
///
/// With the flame sync on (default; `FH1_BACKFIRE_SYNC=0` = old) only pops that are actually AUDIBLE are reported: the
/// pop's peak at the car's output (clip peak x gain x view mix) must stand `FH1_BACKFIRE_MIN_PROM` (default 1.5) times above
/// the engine bus RMS of the same block, and `strength` is that loudness mapped to 0..1 (threshold .. 6x threshold). The
/// old rule reported every queued pop, incl. quiet crackle buried under the engine: flames with no bang (user 2026-10-08).
#[derive(Debug, Clone, Copy)]
pub struct Backfire {
    /// Sync on: audible loudness 0..1 (see above). Sync off: the pop gain incl. the knobs (0..~1.5).
    pub strength: f32,
    /// A crack (`*Bang*` sample) rather than a burble pop.
    pub bang: bool,
    /// Peak amplitude this pop reaches at the car's output (after the character makeup, before the limiter); the
    /// output thread scales it by another car's distance gain and drops pops that end up inaudible.
    pub level: f32,
}

/// `FH1_BACKFIRE_SYNC` (default on): report only audible pops, `strength` = loudness (see [`Backfire`]).
pub fn backfire_sync() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_BACKFIRE_SYNC").map_or(true, |v| v != "0"))
}

/// `FH1_BACKFIRE_BANGS_ONLY` (default on; needs the sync): only BANG events fire flames: the bang samples of the upshift
/// crack and the limiter bounce. Overrun burble / crackle never does (user 2026-10-08: "still a lot of flames even from
/// just burble ... we want it only on bangs"), except a burble bang at the very top of the loudness range
/// (strength >= [`BURBLE_BANG_MIN`]). Off = every audible pop (the sync rule alone).
pub fn backfire_bangs_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_BACKFIRE_BANGS_ONLY").map_or(true, |v| v != "0"))
}

/// Loudness (0..1, see [`Backfire::strength`]) a burble-window bang needs to fire a flame with bangs-only on.
pub const BURBLE_BANG_MIN: f32 = 0.85;

/// Pop peak over the engine bus RMS needed for a flame (`FH1_BACKFIRE_MIN_PROM`, default 1.5).
fn backfire_min_prom() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_BACKFIRE_MIN_PROM").ok().and_then(|v| v.parse().ok()).unwrap_or(1.5f32).max(0.0))
}

/// Absolute floor for [`Backfire::level`] (output peak) below which a pop counts as silent (-36 dBFS).
pub const BACKFIRE_MIN_LEVEL: f32 = 0.016;

/// Peak |sample| of a clip (the pop's own loudness; burble samples differ by ~10 dB).
fn clip_peak(c: &Clip) -> f32 {
    c.data.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

// ---------------------------------------------------------------- samples

/// A decoded sample, interleaved f32.
#[derive(Clone)]
pub struct Clip {
    pub name: String,
    pub rate: u32,
    pub channels: usize,
    pub data: Arc<[f32]>,
}

impl Clip {
    pub fn frames(&self) -> usize {
        self.data.len() / self.channels.max(1)
    }
    /// Linear-interpolated stereo frame at fractional position `pos` (wrapping).
    #[inline]
    fn frame(&self, pos: f64) -> (f32, f32) {
        let n = self.frames();
        if n == 0 {
            return (0.0, 0.0);
        }
        let i = pos.floor() as usize % n;
        let j = (i + 1) % n;
        let t = (pos - pos.floor()) as f32;
        let c = self.channels;
        let at = |k: usize, ch: usize| self.data[k * c + ch.min(c - 1)];
        let l = at(i, 0) + (at(j, 0) - at(i, 0)) * t;
        let r = at(i, 1) + (at(j, 1) - at(i, 1)) * t;
        (l, r)
    }
}

/// Converted banks on disk (`<audio group>/banks`), loaded on demand and cached.
pub struct Library {
    root: PathBuf,
    banks: Mutex<HashMap<String, Arc<Vec<Clip>>>>,
    tyre_groups: HashMap<String, TyreGroup>,
}

impl Library {
    /// `root` is the installed `audio` group folder.
    pub fn open(root: impl Into<PathBuf>) -> Result<Library> {
        let root = root.into();
        let tyre_groups = std::fs::read(root.join("tires.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<HashMap<String, String>>(&b).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), TyreGroup::from_name(&v)))
            .collect();
        Ok(Library { root, banks: Mutex::default(), tyre_groups })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Tyre group for a physics surface name (`surfaceTypes.xml` names, e.g. `Asphalt`).
    pub fn tyre_group(&self, surface: &str) -> TyreGroup {
        self.tyre_groups.get(&surface.to_ascii_lowercase()).copied().unwrap_or_default()
    }

    pub fn car(&self, car: &str) -> Result<CarAudio> {
        let p = self.root.join("cars").join(format!("{car}.json"));
        serde_json::from_slice(&std::fs::read(&p).with_context(|| p.display().to_string())?)
            .with_context(|| p.display().to_string())
    }

    pub fn bank(&self, bank: &str) -> Result<Arc<Vec<Clip>>> {
        let stem = crate::install::bank_stem(bank).to_owned();
        if let Some(b) = self.banks.lock().unwrap().get(&stem) {
            return Ok(b.clone());
        }
        let dir = self.root.join("banks");
        let info: BankInfo = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{stem}.json"))).with_context(|| format!("bank {stem}"))?,
        )?;
        let mut clips = Vec::with_capacity(info.samples.len());
        for (i, s) in info.samples.iter().enumerate() {
            let (rate, channels, data) = crate::wav::read(&dir.join(&stem).join(format!("{i:03}.wav")))?;
            clips.push(Clip { name: s.name.clone(), rate, channels: channels as usize, data: data.into() });
        }
        let clips = Arc::new(clips);
        self.banks.lock().unwrap().insert(stem, clips.clone());
        Ok(clips)
    }

    /// Clip by exact sample name from a shared bank (e.g. `Tires`, `Asph_HDF_Skid_1V4`).
    pub fn clip(&self, bank: &str, name: &str) -> Option<Clip> {
        self.bank(bank).ok()?.iter().find(|c| c.name.eq_ignore_ascii_case(name)).cloned()
    }
}

// ---------------------------------------------------------------- DSP

#[derive(Clone, Copy, Default)]
pub(crate) struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [[f32; 2]; 2],
}

impl Biquad {
    pub(crate) fn pass() -> Biquad {
        Biquad { b: [1.0, 0.0, 0.0], ..Default::default() }
    }
    /// RBJ cookbook lowpass.
    pub(crate) fn set_lowpass(&mut self, rate: f32, freq: f32, q: f32) {
        let w = 2.0 * PI * (freq / rate).clamp(1e-4, 0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q.max(0.1));
        let a0 = 1.0 + alpha;
        self.b = [(1.0 - c) / 2.0 / a0, (1.0 - c) / a0, (1.0 - c) / 2.0 / a0];
        self.a = [-2.0 * c / a0, (1.0 - alpha) / a0];
    }
    /// RBJ cookbook highpass.
    pub(crate) fn set_highpass(&mut self, rate: f32, freq: f32, q: f32) {
        let w = 2.0 * PI * (freq / rate).clamp(1e-4, 0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q.max(0.1));
        let a0 = 1.0 + alpha;
        self.b = [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0];
        self.a = [-2.0 * c / a0, (1.0 - alpha) / a0];
    }
    /// RBJ cookbook peaking EQ; `gain` linear, `bw` in octaves.
    pub(crate) fn set_peak(&mut self, rate: f32, freq: f32, gain: f32, bw: f32) {
        let w = 2.0 * PI * (freq / rate).clamp(1e-4, 0.49);
        let (s, c) = w.sin_cos();
        let a = gain.max(0.01).sqrt();
        let alpha = s * ((2f32.ln() / 2.0) * bw.clamp(0.05, 6.0) * w / s).sinh();
        let a0 = 1.0 + alpha / a;
        self.b = [(1.0 + alpha * a) / a0, -2.0 * c / a0, (1.0 - alpha * a) / a0];
        self.a = [-2.0 * c / a0, (1.0 - alpha / a) / a0];
    }
    #[inline]
    pub(crate) fn run(&mut self, ch: usize, x: f32) -> f32 {
        let z = &mut self.z[ch];
        let y = self.b[0] * x + z[0];
        z[0] = self.b[1] * x - self.a[0] * y + z[1];
        z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// A looping sample with a fractional read head and a gain ramp per block.
#[derive(Clone)]
struct Voice {
    clip: Clip,
    pos: f64,
    gain: f32,
}

impl Voice {
    fn new(clip: Clip) -> Voice {
        Voice { clip, pos: 0.0, gain: 0.0 }
    }
    /// Adds `out.len()/2` frames at `pitch`, ramping gain to `target` across the block and
    /// panning with `pan` = (left, right) gains.
    fn mix(&mut self, out: &mut [f32], out_rate: f32, pitch: f32, target: f32, pan: (f32, f32)) {
        if self.gain < 1e-5 && target < 1e-5 {
            self.gain = 0.0;
            return;
        }
        let frames = out.len() / 2;
        let step = pitch.clamp(0.05, 8.0) as f64 * self.clip.rate as f64 / out_rate as f64;
        let dg = (target - self.gain) / frames as f32;
        let mono = self.clip.channels == 1;
        for f in 0..frames {
            let (l, r) = self.clip.frame(self.pos);
            let g = self.gain + dg * f as f32;
            if mono {
                out[2 * f] += l * g * pan.0;
                out[2 * f + 1] += l * g * pan.1;
            } else {
                // Stereo samples keep their own image; pan only tilts it.
                out[2 * f] += l * g * pan.0;
                out[2 * f + 1] += r * g * pan.1;
            }
            self.pos += step;
        }
        let n = self.clip.frames() as f64;
        if n > 0.0 {
            self.pos %= n;
        }
        self.gain = target;
    }
}

/// A one-shot sample (shift clunk, blow-off, burble pop).
struct Shot {
    clip: Clip,
    pos: f64,
    gain: f32,
    pitch: f32,
    /// (left, right) gains.
    pan: (f32, f32),
}

impl Shot {
    fn new(clip: Clip, gain: f32) -> Shot {
        Shot { clip, pos: 0.0, gain, pitch: 1.0, pan: (1.0, 1.0) }
    }
}

// ---------------------------------------------------------------- engine

struct LoopSet {
    pan: (f32, f32),
    /// Which `ViewMix` field scales this set.
    mix: fn(&ViewMix) -> f32,
    loops: Vec<Loop>,
    voices: Vec<Voice>,
}

struct EmitterSynth {
    em: Emitter,
    sets: Vec<LoopSet>,
    lowpass: Biquad,
    peqs: [Biquad; 3],
}

/// Equal-power crossfade weights of `loops` (sorted by rpm_sample) at `rpm`.
fn loop_weights(loops: &[Loop], rpm: f32, w: &mut Vec<f32>) {
    w.clear();
    let n = loops.len();
    for k in 0..n {
        let l = &loops[k];
        let mut g = 1.0f32;
        if k > 0 {
            // Fade in across the overlap with the previous loop.
            let (a, b) = (l.rpm_min, loops[k - 1].rpm_max.max(l.rpm_min + 1.0));
            g *= if rpm <= a { 0.0 } else { ((rpm - a) / (b - a)).min(1.0) };
        }
        if k + 1 < n {
            let (a, b) = (loops[k + 1].rpm_min.min(l.rpm_max - 1.0), l.rpm_max);
            g *= if rpm >= b { 0.0 } else { ((b - rpm) / (b - a)).min(1.0) };
        }
        w.push((g * PI / 2.0).sin());
    }
    let norm = w.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-6 {
        w.iter_mut().for_each(|x| *x /= norm);
    } else if n > 0 {
        // Gap between loops: use the nearest one.
        let k = (0..n)
            .min_by(|&i, &j| {
                (loops[i].rpm_sample - rpm).abs().total_cmp(&(loops[j].rpm_sample - rpm).abs())
            })
            .unwrap();
        w[k] = 1.0;
    }
}

impl EmitterSynth {
    fn new(lib: &Library, em: &Emitter, kind: &str) -> Result<EmitterSynth> {
        let bank = lib.bank(&em.bank)?;
        let mut sets = Vec::new();
        for (name, loops) in &em.sets {
            let mut loops: Vec<(Loop, Voice)> = loops
                .iter()
                .filter_map(|l| Some((l.clone(), Voice::new(bank.get(l.sample as usize)?.clone()))))
                .collect();
            loops.sort_by(|a, b| a.0.rpm_sample.total_cmp(&b.0.rpm_sample));
            let n = name.to_ascii_lowercase();
            let (pan, mix): ((f32, f32), fn(&ViewMix) -> f32) = if n.ends_with('l') && kind == "exhaust" {
                ((0.9, 0.4), |m| m.exhaust_l)
            } else if n.ends_with('r') && kind == "exhaust" {
                ((0.4, 0.9), |m| m.exhaust_r)
            } else if kind == "exhaust" {
                ((0.75, 0.75), |m| (m.exhaust_l + m.exhaust_r) / 2.0)
            } else if kind == "intake" {
                ((0.75, 0.75), |m| m.intake)
            } else {
                ((0.75, 0.75), |m| m.ambient)
            };
            let (loops, voices) = loops.into_iter().unzip();
            sets.push(LoopSet { pan, mix, loops, voices });
        }
        Ok(EmitterSynth { em: em.clone(), sets, lowpass: Biquad::pass(), peqs: [Biquad::pass(); 3] })
    }

    #[allow(clippy::too_many_arguments)]
    fn render(&mut self, out: &mut [f32], rate: f32, rpm: f32, doppler: f32, d: &Drive, gain: f32, mix: &ViewMix, w: &mut Vec<f32>, tmp: &mut Vec<f32>) {
        tmp.clear();
        tmp.resize(out.len(), 0.0);
        let dsp = &self.em.dsp;
        let vol = dsp.volume.map_or(1.0, |c| c.eval(d)) * gain;
        for set in &mut self.sets {
            loop_weights(&set.loops, rpm, w);
            let set_gain = vol * (set.mix)(mix);
            for ((l, v), &wk) in set.loops.iter().zip(set.voices.iter_mut()).zip(w.iter()) {
                let pitch = rpm.max(1.0) / l.rpm_sample.max(1.0) * l.pitch * doppler;
                v.mix(tmp, rate, pitch, wk * l.volume * set_gain, set.pan);
            }
        }
        // Filters, coefficients updated once per block.
        let lp = dsp.lowpass.map_or(20000.0, |c| c.eval(d));
        let lp_on = lp < rate * 0.45;
        if lp_on {
            self.lowpass.set_lowpass(rate, lp, std::f32::consts::FRAC_1_SQRT_2);
        }
        let peqs: [&Option<Peq>; 3] = [&dsp.peq, &dsp.pos_load_peq, &dsp.neg_load_peq];
        let mut active = [false; 3];
        for (i, p) in peqs.iter().enumerate() {
            if let Some(p) = p {
                let g = p.gain.eval(d);
                if (g - 1.0).abs() > 0.01 {
                    self.peqs[i].set_peak(rate, p.freq.eval(d).clamp(20.0, 18000.0), g, p.bandwidth.eval(d));
                    active[i] = true;
                }
            }
        }
        for f in 0..out.len() / 2 {
            for ch in 0..2 {
                let mut x = tmp[2 * f + ch];
                for (i, q) in self.peqs.iter_mut().enumerate() {
                    if active[i] {
                        x = q.run(ch, x);
                    }
                }
                if lp_on {
                    x = self.lowpass.run(ch, x);
                }
                out[2 * f + ch] += x;
            }
        }
    }
}

// ---------------------------------------------------------------- the car

/// Named loop with gain/pitch set each block.
struct Layer {
    voice: Voice,
}

impl Layer {
    fn get(lib: &Library, bank: &str, name: &str) -> Option<Layer> {
        lib.clip(bank, name).map(|c| Layer { voice: Voice::new(c) })
    }
}

/// The EngineLFE samples are not clean loops (user 2026-10-08: "thudding when the accelerator is applied, louder the
/// higher you rev, like a heartbeat / train"): LFE_Short_2V2 (every car up to 6 cylinders) has sound in its first ~62 %
/// and digital silence after, so the looped rumble switched on and off 1.5-3.8 times a second; LFE_12cyl_D_NORM jumps
/// ~0.1 at its wrap. This trims the trailing decay / silence (after the last 1/32 window at >= half the clip's median
/// RMS) and crossfades the last 30 ms into the start, so the loop has neither a gap nor a seam.
/// FH1_AUDIO_LFE_LOOP=0 = the raw sample, as before.
fn seamless_loop(clip: Clip) -> Clip {
    if std::env::var("FH1_AUDIO_LFE_LOOP").is_ok_and(|v| v == "0") {
        return clip;
    }
    let ch = clip.channels.max(1);
    let n = clip.frames();
    let win = (n / 32).max(1);
    let rms: Vec<f32> = (0..n / win)
        .map(|w| {
            let s = &clip.data[w * win * ch..(w + 1) * win * ch];
            (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt()
        })
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(f32::total_cmp);
    let Some(&median) = sorted.get(sorted.len() / 2) else { return clip };
    let end = rms.iter().rposition(|&r| r >= median * 0.5).map_or(n, |w| (w + 1) * win).min(n);
    let xf = ((clip.rate as f32 * 0.03) as usize).min(end / 4);
    if xf == 0 {
        return clip;
    }
    // Loop body = [0, end - xf); its first xf frames blend in the tail [end - xf, end), so playback runs from the tail
    // straight into the (already blended) start.
    let len = end - xf;
    let mut data: Vec<f32> = clip.data[..len * ch].to_vec();
    for i in 0..xf {
        let a = i as f32 / xf as f32;
        for c in 0..ch {
            let head = clip.data[i * ch + c];
            let tail = clip.data[(len + i) * ch + c];
            data[i * ch + c] = head * a + tail * (1.0 - a);
        }
    }
    Clip { data: data.into(), ..clip }
}

/// Overrun burbles only off the throttle (user 2026-10-08: "on some vehicles the burble / gargle continues even when
/// holding the accelerator"). The window closes when the throttle comes back, pops need the throttle off, downshift
/// blips need a lift, and a lift counts after [`LIFT_DEBOUNCE`] (torque-derived AI / traffic throttle dips at shifts).
/// The rev-limiter bounce pops stay (requested 2026-10-07). FH1_AUDIO_BURBLE_GATE=0 = the old trigger.
fn burble_gate() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("FH1_AUDIO_BURBLE_GATE").is_ok_and(|v| v == "0"))
}

/// Seconds the throttle must stay off before a lift opens a burble window.
const LIFT_DEBOUNCE: f32 = 0.12;

struct TyreLayers {
    /// Skid drive after the attack / release smoothing (ABS pulses and bumps used to cut the screech on and off).
    smooth: f32,
    roll: Option<Layer>,
    skid_pre: Option<Layer>,
    skid: Option<Layer>,
    skid_high: Option<Layer>,
}

pub struct CarSound {
    pub audio: CarAudio,
    emitters: Vec<EmitterSynth>,
    tyres: Vec<(TyreGroup, TyreLayers)>,
    offroad_chassis: Option<Layer>,
    wind: Option<Layer>,
    whine: Option<Layer>,
    turbo: Option<Layer>,
    shift_clips: Vec<Clip>,
    blowoff: Option<Clip>,
    blowoff_med: Option<Clip>,
    /// Overrun burbles and backfire bangs (`Engines/Soundbanks/Burbles`).
    pops: Vec<Clip>,
    bangs: Vec<Clip>,
    /// Peak |sample| of each pop / bang clip.
    pop_peaks: Vec<f32>,
    bang_peaks: Vec<f32>,
    /// Pops queued this block (backfire, clip peak, from the burble / crackle) until the block's mix says how loud they are.
    pending: Vec<(Backfire, f32, bool)>,
    /// Sub rumble (`EngineLFE`) and exhaust air (`ExhNoise`), each through its own lowpass.
    lfe: Option<Layer>,
    lfe_lp: [Biquad; 2],
    exh_noise: Option<Layer>,
    noise_lp: Biquad,
    shots: Vec<Shot>,
    /// `None` until the first block, so a car loaded mid-drive does not clunk.
    last_shifts: Option<u32>,
    last_gear: Option<i32>,
    last_throttle: f32,
    smooth_rpm: f32,
    /// Engine gain envelope: ignition cut left (s), shift boost above 1 and its decay rate (/s).
    cut_t: f32,
    boost: f32,
    boost_rate: f32,
    /// Limiter bounce phase (0..1) and whether the last block was in a cut.
    bounce: f32,
    bounce_cut: bool,
    /// Burble window left (s) and time to the next pop (s).
    burble_t: f32,
    next_pop: f32,
    /// Burble gate ([`burble_gate`]): how long the throttle has been off (< 0.1), and since it was last on (> 0.4), s.
    lift_t: f32,
    held_ago: f32,
    engine_bus: crate::character::EngineBus,
    limiter: crate::character::Limiter,
    /// Pops / bangs since the last [`CarSound::take_backfires`] (capped).
    fired: Vec<Backfire>,
    rng: u32,
    w: Vec<f32>,
    tmp: Vec<f32>,
    bus: Vec<f32>,
}

/// Burble bank for a car: the NSX / Esprit get their own, the rest by cylinder count (OUR choice: the game picks
/// the bank in its `.fev` event data, not decoded).
fn burble_bank(car: &str, cylinders: u32) -> &'static str {
    let c = car.to_ascii_lowercase();
    if c.contains("nsx") {
        "6N0_NSX_Pops"
    } else if c.contains("esprit") {
        "4T2_Tag_Espirit"
    } else {
        match cylinders {
            0..=4 => "EvoPops",
            5..=6 => "6N3_NSX_Pops",
            7..=8 => "SequencePops_2",
            _ => "SequencePops_3",
        }
    }
}

impl CarSound {
    pub fn new(lib: &Library, car: &str) -> Result<CarSound> {
        let audio = lib.car(car)?;
        let mut emitters = Vec::new();
        for (em, kind) in [(&audio.intake, "intake"), (&audio.ambient, "ambient"), (&audio.exhaust, "exhaust")] {
            if let Some(em) = em {
                emitters.push(EmitterSynth::new(lib, em, kind).with_context(|| format!("{car} {kind}"))?);
            }
        }
        let t = |g: TyreGroup, roll: &str, pre: &str, skid: &str, high: &str| {
            (
                g,
                TyreLayers {
                    smooth: 0.0,
                    roll: Layer::get(lib, "Tires", roll),
                    skid_pre: Layer::get(lib, "Tires", pre),
                    skid: Layer::get(lib, "Tires", skid),
                    skid_high: Layer::get(lib, "Tires", high),
                },
            )
        };
        let tyres = vec![
            t(TyreGroup::OnRoad, "Asph_Roadnoise_Slow_V2_1", "Asph_HDF_PreSkid_1V4", "Asph_HDF_Skid_1V4", "Asph_HDF_SkidHigh_1V4"),
            t(TyreGroup::Trackway, "Asph_Roadnoise_Slow_V2_2", "Asph_LDF_PreSkid_1V4", "Asph_HDF_Skid_2V4", "Asph_HDF_SkidHigh_2V4"),
            t(TyreGroup::Brick, "Brick_V3_01", "Asph_HDF_PreSkid_2V4", "Asph_HDF_Skid_3V4", "Asph_HDF_SkidHigh_3V4"),
            t(TyreGroup::OffRoad, "DRT_Fast_1V4", "DRT_MedSkid_1_V2", "DRT_MedSkid_2_V2", "DRT_Fast_2V4"),
            t(TyreGroup::Grass, "GRS_Fast_1V4", "GRS_MedSkid_1_V2", "GRS_MedSkid_2_V2", "GRS_Fast_2V4"),
        ];
        let shift_clips = (1..=3)
            .flat_map(|i| ["A", "B", "C"].map(|v| format!("HorizonShift_0{i}_{v}")))
            .filter_map(|n| lib.clip("Transmissions", &n))
            .collect();
        // Burbles: pops vs bangs by sample name. Missing in audio-1 installs (then: no pops).
        let (bangs, pops): (Vec<Clip>, Vec<Clip>) = lib
            .bank(burble_bank(car, audio.cylinders))
            .map(|b| b.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .partition(|c| c.name.to_ascii_lowercase().contains("bang"));
        let lfe_name = match audio.cylinders {
            12.. => "LFE_12cyl_D_NORM",
            10..=11 => "LFE_10_NORM_Down1",
            7..=9 => "LFE_8cyl_A_NORM_Down1",
            _ => "LFE_Short_2V2",
        };
        let choice = audio.extras.blowoff_choice.clamp(1, 3);
        let blowoff = lib.clip("Turbos", &format!("BlowOff_L1_{choice}_HI_1")).or_else(|| lib.clip("Turbos", "BlowOff_L1_1_HI_1"));
        let blowoff_med = lib.clip("Turbos", &format!("BlowOff_L1_{choice}_MED_1")).or_else(|| blowoff.clone());
        Ok(CarSound {
            emitters,
            tyres,
            offroad_chassis: Layer::get(lib, "Tires", "Chassis_OffRoad_LP_1"),
            wind: Layer::get(lib, "Wind", "Wind_Violent_V4"),
            whine: Layer::get(lib, "Transmissions", "Transm_Mid_6100"),
            turbo: Layer::get(lib, "Turbos", "turbo1_loop7_V5"),
            shift_clips,
            blowoff,
            blowoff_med,
            pop_peaks: pops.iter().map(clip_peak).collect(),
            bang_peaks: bangs.iter().map(clip_peak).collect(),
            pending: Vec::new(),
            pops,
            bangs,
            lfe: Layer::get(lib, "EngineLFE", lfe_name).map(|l| Layer { voice: Voice::new(seamless_loop(l.voice.clip)) }),
            lfe_lp: [Biquad::pass(); 2],
            exh_noise: Layer::get(lib, "ExhNoise", "pinknoise_00.wav"),
            noise_lp: Biquad::pass(),
            shots: Vec::new(),
            last_shifts: None,
            last_gear: None,
            last_throttle: 0.0,
            smooth_rpm: audio.rpm_idle,
            cut_t: 0.0,
            boost: 0.0,
            boost_rate: 1.0,
            bounce: 0.0,
            bounce_cut: false,
            burble_t: 0.0,
            next_pop: 0.0,
            lift_t: 0.0,
            held_ago: f32::MAX,
            engine_bus: Default::default(),
            limiter: Default::default(),
            fired: Vec::new(),
            rng: 0x1234_5678 ^ (car.len() as u32).wrapping_mul(0x9E37_79B9),
            w: Vec::new(),
            tmp: Vec::new(),
            bus: Vec::new(),
            audio,
        })
    }

    fn rand(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.rng
    }

    /// Uniform 0..1.
    fn randf(&mut self) -> f32 {
        (self.rand() >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Queues a burble pop (`bang` = a backfire crack) at `gain` (scaled by the engine / exhaust mix when played).
    /// `burble` = from the overrun burble / crackle (not a shift crack or limiter bang): no flame (backfire_bangs_only).
    fn pop(&mut self, bang: bool, gain: f32, burble: bool) {
        let n = if bang && !self.bangs.is_empty() { self.bangs.len() } else { self.pops.len() };
        if n == 0 || gain <= 1e-4 {
            return;
        }
        let k = self.rand() as usize % n;
        let (clip, peak) = if bang && !self.bangs.is_empty() { (self.bangs[k].clone(), self.bang_peaks[k]) } else { (self.pops[k].clone(), self.pop_peaks[k]) };
        let pitch = 0.85 + 0.3 * self.randf();
        let g = gain * (0.55 + 0.45 * self.randf());
        // Two tailpipes: wander the image a little.
        let side = self.randf() * 0.4 - 0.2;
        if self.pending.len() < 32 {
            self.pending.push((Backfire { strength: g, bang: bang && !self.bangs.is_empty(), level: 0.0 }, peak, burble));
        }
        self.shots.push(Shot { clip, pos: 0.0, gain: -g, pitch, pan: (1.0 - side, 1.0 + side) });
    }

    /// Pops / bangs played since the last call (the output thread collects them after each block).
    pub fn take_backfires(&mut self, into: &mut Vec<Backfire>) {
        into.append(&mut self.fired);
    }

    /// Renders `out.len() / 2` stereo frames (adds nothing outside; overwrites `out`).
    /// Call with blocks of ~5-20 ms; parameters are evaluated once per block.
    pub fn render(&mut self, input: &CarInput, out: &mut [f32], rate: f32) {
        out.fill(0.0);
        let knobs = *crate::character::knobs();
        let frames = out.len() / 2;
        let dt = frames as f32 / rate;
        let (idle, red) = (self.audio.rpm_idle, self.audio.rpm_redline);
        let x = std::mem::take(&mut self.audio.extras);

        // --- events: shifts, limiter bounce, burbles (gain envelope on the engine bus + pops).
        let throttle = input.throttle.clamp(0.0, 1.0);
        let rpm_raw_n = ((input.rpm - idle) / (red - idle).max(1.0)).clamp(0.0, 1.0);
        let mut pitch_mul = 1.0;
        let mut env_gain = 1.0;
        let pops_amt = knobs.pops * if input.external { 0.6 } else { 1.0 };
        let gate = burble_gate();
        if knobs.on {
            // Shifts: an ignition cut, then the ET's volume boost (pushed 1.5x) decaying over its time.
            if let Some(g0) = self.last_gear {
                let g1 = input.gear;
                if g1 != g0 && g0 > 0 && g1 > 0 {
                    let up = g1 > g0;
                    let (pct, time) = if up { (x.shift.up_pct, x.shift.up_time) } else { (x.shift.down_pct, x.shift.down_time) };
                    self.boost = ((pct - 1.0).max(0.15) * 1.5).min(1.5);
                    self.boost_rate = self.boost / time.max(0.2);
                    if up && throttle > 0.3 {
                        self.cut_t = 0.1;
                        // Upshift crack under load.
                        let p = 0.45 + 0.5 * x.backfire_volume;
                        if rpm_raw_n > 0.5 && self.randf() < p * knobs.pops.min(2.0) {
                            self.pop(true, 0.9 * pops_amt, false);
                        }
                    } else if !up && (!gate || throttle < 0.3) {
                        // Downshift blip: a burble or two (off the throttle only: a kickdown under load is no blip).
                        self.burble_t = self.burble_t.max(0.35);
                    }
                }
            }
            if self.cut_t > 0.0 {
                self.cut_t -= dt;
                env_gain *= 0.12;
                pitch_mul *= 0.985;
            } else if self.boost > 0.0 {
                env_gain *= 1.0 + self.boost;
                self.boost = (self.boost - self.boost_rate * dt).max(0.0);
            }

            // Rev limiter bounce: hard ignition cuts at ~15 Hz while pinned on the limiter.
            let limit = if input.rpm_limit > 0.0 { input.rpm_limit } else { red * 1.02 };
            if input.rpm >= limit * 0.985 && throttle > 0.5 && input.gear != 0 {
                self.bounce = (self.bounce + dt * 15.0) % 1.0;
                let cut = self.bounce < 0.45;
                if cut {
                    env_gain *= 0.15;
                    pitch_mul *= 0.955;
                    if !self.bounce_cut && self.randf() < 0.35 * knobs.pops.min(2.0) {
                        let bang = self.randf() < 0.3;
                        self.pop(bang, 0.6 * pops_amt, false);
                    }
                }
                self.bounce_cut = cut;
            } else {
                self.bounce = 0.0;
                self.bounce_cut = false;
            }

            // Burbles: lifting off at revs opens a window of pops; sustained overrun keeps a slower crackle.
            let lifted = if gate {
                // A lift that lasts LIFT_DEBOUNCE after the throttle was on within the last 0.4 s. AI / traffic throttle is
                // read from engine torque (fh1-engine audio.rs), so shift cuts, TCS and the limiter dipped it to zero for
                // a few frames and opened a burble window under a held pedal.
                let was = self.lift_t;
                self.lift_t = if throttle < 0.1 { self.lift_t + dt } else { 0.0 };
                self.held_ago = if throttle > 0.4 { 0.0 } else { self.held_ago + dt };
                was < LIFT_DEBOUNCE && self.lift_t >= LIFT_DEBOUNCE && self.held_ago < LIFT_DEBOUNCE + 0.4
            } else {
                self.last_throttle > 0.4 && throttle < 0.1
            };
            if lifted && rpm_raw_n > 0.3 {
                self.burble_t = 0.6 + 1.6 * rpm_raw_n;
                self.next_pop = 0.03;
            }
            // Back on the throttle: the window closes (it used to run its full 0.6-2.2 s under a held pedal).
            if gate && throttle > 0.3 {
                self.burble_t = 0.0;
            }
            // Sustained overrun also needs the lift to have lasted (a 50 ms torque dip fired a crackle).
            let overrun = throttle < 0.05 && rpm_raw_n > 0.4 && input.gear != 0 && input.speed.abs() > 3.0 && (!gate || self.lift_t >= LIFT_DEBOUNCE);
            if (self.burble_t > 0.0 || overrun) && (!gate || throttle < 0.15) {
                self.burble_t = (self.burble_t - dt).max(0.0);
                self.next_pop -= dt;
                if self.next_pop <= 0.0 {
                    let busy = self.burble_t > 0.0;
                    self.next_pop = if busy { 0.035 + 0.13 * self.randf() } else { 0.12 + 0.35 * self.randf() };
                    // Everyone burbles (exaggerated); the ET's BurbleVolume makes it louder.
                    let g = (0.35 + 0.65 * x.burble_volume) * (0.4 + 0.6 * rpm_raw_n) * pops_amt;
                    let bang = self.randf() < 0.08 + 0.1 * x.backfire_volume;
                    self.pop(bang, if busy { g } else { g * 0.6 }, true);
                }
            }
        }
        self.last_gear = Some(input.gear);
        let shift_r = self.rand() as usize;
        let a = &self.audio;

        // Smooth rpm a little so 60 Hz physics steps don't zipper the pitch.
        self.smooth_rpm += (input.rpm - self.smooth_rpm) * (1.0 - (-dt / 0.02).exp());
        let rpm = self.smooth_rpm.max(0.0) * pitch_mul;
        let rpm_n = ((rpm - a.rpm_idle) / (a.rpm_redline - a.rpm_idle).max(1.0)).clamp(0.0, 1.0);
        let torque = input.torque.unwrap_or(input.throttle - (1.0 - input.throttle) * 0.3 * rpm_n);
        let d = Drive {
            rpm: rpm_n,
            throttle,
            pos_torque: torque.clamp(0.0, 1.0),
            neg_torque: (-torque).clamp(0.0, 1.0),
        };
        let view_name = match input.view {
            View::Front => "Front",
            View::Follow => "Follow",
            View::Cockpit => "Cockpit",
        };
        let mix = a.views.get(view_name).cloned().unwrap_or_default();
        let master = input.volume.clamp(0.0, 1.0);
        let engine_gain = master * a.engine_volume * a.global_volume.map_or(1.0, |c: Curve| c.eval(&d)) * if knobs.on { knobs.engine } else { 1.0 };

        // --- engine emitters into the engine bus, then the character chain.
        let mut tmp = std::mem::take(&mut self.tmp);
        let mut w = std::mem::take(&mut self.w);
        let mut bus = std::mem::take(&mut self.bus);
        bus.clear();
        bus.resize(out.len(), 0.0);
        let doppler = if input.pitch > 0.0 { input.pitch } else { 1.0 };
        for e in &mut self.emitters {
            e.render(&mut bus, rate, rpm, doppler, &d, engine_gain, &mix, &mut w, &mut tmp);
        }
        if knobs.on {
            // Sub rumble + exhaust air ride the bus too, so they get the grit and the shift / limiter envelope.
            if let Some(l) = &mut self.lfe {
                tmp.clear();
                tmp.resize(out.len(), 0.0);
                let g = engine_gain * (0.25 + 0.75 * throttle) * (0.35 + 0.65 * rpm_n) * 0.45;
                l.voice.mix(&mut tmp, rate, (0.55 + 0.9 * rpm_n) * doppler, g, (1.0, 1.0));
                let lp = 140.0 + 120.0 * rpm_n;
                self.lfe_lp.iter_mut().for_each(|b| b.set_lowpass(rate, lp, std::f32::consts::FRAC_1_SQRT_2));
                for f in 0..frames {
                    for ch in 0..2 {
                        let v0 = self.lfe_lp[0].run(ch, tmp[2 * f + ch]);
                        let v = self.lfe_lp[1].run(ch, v0);
                        bus[2 * f + ch] += v;
                    }
                }
            }
            if let Some(l) = &mut self.exh_noise {
                tmp.clear();
                tmp.resize(out.len(), 0.0);
                let g = engine_gain * throttle * rpm_n * 0.06 * (mix.exhaust_l + mix.exhaust_r) * 0.5;
                l.voice.mix(&mut tmp, rate, 1.0, g, (1.0, 1.0));
                let cutoff = x.reflection_lp.unwrap_or(4000.0).clamp(800.0, 9000.0) * (0.5 + 0.5 * rpm_n);
                self.noise_lp.set_lowpass(rate, cutoff, 0.9);
                for (i, (o, v)) in bus.iter_mut().zip(&tmp).enumerate() {
                    *o += self.noise_lp.run(i % 2, *v);
                }
            }
            self.engine_bus.process(&mut bus, rate, &x, &d, env_gain);
        }
        for (o, b) in out.iter_mut().zip(&bus) {
            *o += *b;
        }
        self.w = w;
        self.tmp = tmp;
        self.bus = bus;

        // --- tyres: per group, the loudest wheel on that group drives it.
        let speed = input.speed.abs();
        let own = if input.external { 0.0 } else { 1.0 };
        let tyre_gain = master * mix.tire * own * skid_level();
        for (group, layers) in &mut self.tyres {
            let (mut roll, mut slip) = (0.0f32, 0.0f32);
            for wh in input.wheels.iter().filter(|w| w.surface == *group && w.load > 1.0) {
                roll += 0.25;
                // 1.0 = the edge of grip. With the tyre model's normalised slip, the screech starts at the peak for every
                // tyre (the old 7 deg / 12 % stopgap fired below the 10-17 deg peaks of FH1's tyres, i.e. in ordinary
                // hard cornering). The .fev tyre event curves are not decoded (docs/AUDIO.md).
                let s = match wh.norm_slip {
                    // 0 below the onset, 1.0 at the peak, then +0.4 per unit past it (the high skid layer from ~2.5 x peak).
                    Some(n) => ((n - SKID_ONSET) / (1.0 - SKID_ONSET)).clamp(0.0, 1.0) * 0.6 + if n >= 1.0 { 0.4 + 0.4 * (n - 1.0) } else { 0.0 },
                    None => (wh.slip_ratio.abs() / 0.12).max(wh.slip_angle_deg.abs() / 7.0),
                };
                slip = slip.max(s * (speed / 3.0).min(1.0).max(wh.slip_ratio.abs().min(1.0)));
            }
            // Fast attack (~60 ms), slow release (~300 ms): a steady squeal through ABS cycles and bouncy corners.
            let tau = if slip > layers.smooth { SKID_ATTACK } else { SKID_RELEASE };
            layers.smooth += (slip - layers.smooth) * (1.0 - (-dt / tau).exp());
            let slip = layers.smooth;
            let sp = (speed / 30.0).min(1.5);
            let roll_g = roll * (speed / 25.0).min(1.0).powf(0.7) * 0.5;
            let ramp = |x: f32, a: f32, b: f32| ((x - a) / (b - a)).clamp(0.0, 1.0);
            let pre = ramp(slip, 0.6, 1.0) * (1.0 - ramp(slip, 1.2, 1.8));
            let mid = ramp(slip, 0.9, 1.4) * (1.0 - ramp(slip, 2.0, 3.0) * 0.5);
            let high = ramp(slip, 1.6, 2.6);
            let pitch = 0.85 + 0.25 * sp;
            for (layer, g, p) in [
                (&mut layers.roll, roll_g, 0.7 + 0.5 * sp),
                (&mut layers.skid_pre, pre * 0.5, pitch),
                (&mut layers.skid, mid * 0.6, pitch),
                (&mut layers.skid_high, high * 0.6, pitch),
            ] {
                if let Some(l) = layer {
                    l.voice.mix(out, rate, p, g * tyre_gain, (0.7, 0.7));
                }
            }
        }
        let offroad = input
            .wheels
            .iter()
            .filter(|w| w.load > 1.0 && matches!(w.surface, TyreGroup::OffRoad | TyreGroup::Grass))
            .count() as f32
            / 4.0;
        if let Some(l) = &mut self.offroad_chassis {
            l.voice.mix(out, rate, 0.8 + speed / 60.0, offroad * (speed / 20.0).min(1.0) * 0.4 * master * own, (0.7, 0.7));
        }

        // --- wind
        if let Some(l) = &mut self.wind {
            let g = (speed / 70.0).powi(2).min(1.2) * 0.35 * mix.wind * master * own;
            l.voice.mix(out, rate, 0.7 + speed / 120.0, g, (0.7, 0.7));
        }

        // --- transmission whine: follows engine rpm in gear (driveshaft ∝ rpm / ratio, but the
        // sample is pitched for the gearbox, so engine rpm is the closer proxy).
        if let Some(l) = &mut self.whine {
            let in_gear = input.gear != 0;
            let g = if in_gear { (speed / 40.0).min(1.0) * (0.3 + 0.7 * d.throttle) } else { 0.0 };
            l.voice.mix(out, rate, rpm / 6100.0 * doppler, g * 0.25 * a.transmission_volume * mix.transmission * master, (0.7, 0.7));
        }

        // --- turbo spool + blow-off (FH1_AUDIO_TURBO over the game's level with the character chain on; was 1.6x).
        let turbo_push = if knobs.on { knobs.turbo } else { 1.0 };
        if input.has_turbo {
            if let Some(l) = &mut self.turbo {
                let b = input.boost.clamp(0.0, 1.0);
                l.voice.mix(out, rate, 0.6 + 0.9 * b, b * b * 0.4 * a.turbo_volume * mix.turbo * master * turbo_push, (0.7, 0.7));
            }
            let (from, min_boost) = if knobs.on { (0.5, 0.3) } else { (0.6, 0.5) };
            if self.last_throttle > from && input.throttle < 0.2 && input.boost > min_boost {
                let clip = if input.boost > 0.7 { self.blowoff.clone() } else { self.blowoff_med.clone() };
                if let Some(c) = clip {
                    let g = 0.5 * a.turbo_volume * mix.turbo * master * input.boost * turbo_push;
                    self.shots.push(Shot::new(c, g));
                }
            }
        }

        // --- shifts
        if self.last_shifts.is_some_and(|s| s != input.shifts) && !self.shift_clips.is_empty() {
            let g = 0.6 * a.shift_volume * mix.transmission * master * if knobs.on { 1.4 } else { 1.0 };
            let k = shift_r % self.shift_clips.len();
            let clip = self.shift_clips[k].clone();
            self.shots.push(Shot::new(clip, g));
        }
        self.last_shifts = Some(input.shifts);
        self.last_throttle = input.throttle;

        // --- one-shots. Pops are queued with a negative gain = "scale by the engine volume and exhaust mix".
        let pop_scale = master * a.engine_volume * (mix.exhaust_l + mix.exhaust_r) * 0.5;
        if !self.pending.is_empty() {
            // How loud each pop queued this block plays against the rest of the car (all layers so far, no one-shots).
            let makeup = if knobs.on { crate::character::db(MAKEUP_DB + knobs.loud) } else { 1.0 };
            let rms = (out.iter().map(|v| v * v).sum::<f32>() / out.len().max(1) as f32).sqrt();
            let (sync, min_prom) = (backfire_sync(), backfire_min_prom());
            let bangs_only = sync && backfire_bangs_only();
            for (mut b, peak, burble) in self.pending.drain(..) {
                let pre = b.strength * peak * pop_scale;
                b.level = pre * makeup;
                if sync {
                    let prom = pre / rms.max(1e-3);
                    if prom < min_prom || b.level < BACKFIRE_MIN_LEVEL {
                        continue;
                    }
                    // 0 at the threshold, 1 at 6x it (+15.6 dB).
                    b.strength = ((prom / min_prom.max(1e-3)).log2() / 6f32.log2()).clamp(0.0, 1.0);
                    // Flames on bangs only (see backfire_bangs_only).
                    if bangs_only && (!b.bang || (burble && b.strength < BURBLE_BANG_MIN)) {
                        continue;
                    }
                }
                if self.fired.len() < 32 {
                    self.fired.push(b);
                }
            }
        }
        self.shots.retain_mut(|s| {
            let step = s.clip.rate as f64 / rate as f64 * s.pitch as f64;
            let g = if s.gain < 0.0 { -s.gain * pop_scale } else { s.gain };
            for f in 0..frames {
                if s.pos as usize + 1 >= s.clip.frames() {
                    return false;
                }
                let (l, r) = s.clip.frame(s.pos);
                out[2 * f] += l * g * s.pan.0;
                out[2 * f + 1] += r * g * s.pan.1;
                s.pos += step;
            }
            true
        });
        self.audio.extras = x;

        if knobs.on {
            // Makeup (exaggerated loudness) into a look-ahead limiter: the car can slam the limiter without wrapping.
            let makeup = crate::character::db(MAKEUP_DB + knobs.loud);
            out.iter_mut().for_each(|v| *v *= makeup);
            if !input.external {
                self.limiter.process(out, rate);
            }
        } else {
            // Soft clip so stacked layers never wrap.
            for x in out.iter_mut() {
                *x = x.tanh();
            }
        }
    }
}

/// Car mix makeup with the character chain on (measured: pulls at about -11 dBFS RMS, see docs/AUDIO.md).
const MAKEUP_DB: f32 = 9.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(min: f32, s: f32, max: f32) -> Loop {
        Loop { sample: 0, rpm_min: min, rpm_sample: s, rpm_max: max, volume: 1.0, pitch: 1.0 }
    }

    #[test]
    fn crossfade_is_equal_power_and_local() {
        // From 8N2_MasGranTurismo_Alfa_Exh_HT.xml.
        let loops = [lp(500.0, 921.0, 1256.0), lp(1006.0, 1341.0, 1553.0), lp(1303.0, 1514.0, 1796.0)];
        let mut w = Vec::new();
        for rpm in [600.0, 921.0, 1100.0, 1341.0, 1400.0, 1700.0, 5000.0] {
            loop_weights(&loops, rpm, &mut w);
            let p: f32 = w.iter().map(|x| x * x).sum();
            assert!((p - 1.0).abs() < 1e-4, "rpm {rpm}: power {p}");
        }
        loop_weights(&loops, 921.0, &mut w);
        assert_eq!(w[1], 0.0);
        loop_weights(&loops, 1100.0, &mut w);
        assert!(w[0] > 0.0 && w[1] > 0.0 && w[2] == 0.0);
        loop_weights(&loops, 5000.0, &mut w);
        assert_eq!(w[2], 1.0);
    }

    #[test]
    fn biquad_lowpass_attenuates_highs() {
        let mut b = Biquad::pass();
        b.set_lowpass(48000.0, 500.0, 0.707);
        let rms = |b: &mut Biquad, f: f32| {
            let mut e = 0.0;
            for i in 0..48000 {
                let y = b.run(0, (2.0 * PI * f * i as f32 / 48000.0).sin());
                if i > 4800 {
                    e += y * y;
                }
            }
            (e / 43200.0).sqrt()
        };
        assert!(rms(&mut b.clone(), 100.0) > 0.6);
        assert!(rms(&mut b, 8000.0) < 0.01);
    }
}

/// Normalised slip where the skid layers start (just under the grip peak at 1.0).
const SKID_ONSET: f32 = 0.85;
/// Skid drive smoothing time constants (s).
const SKID_ATTACK: f32 = 0.06;
const SKID_RELEASE: f32 = 0.3;

/// `FH1_AUDIO_SKID`: tyre roll / skid level (user 2026-10-07: "the tyre screeching is way too much"; default 0.45).
fn skid_level() -> f32 {
    static L: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *L.get_or_init(|| std::env::var("FH1_AUDIO_SKID").ok().and_then(|v| v.parse().ok()).unwrap_or(0.45))
}
