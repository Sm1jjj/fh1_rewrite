//! Engine character: the car-wide DSP the ET puts after the three emitters, plus the master limiter.
//!
//! Chain (per car, on the summed intake + ambient + exhaust): FocusPEQ -> Distortion -> TrashDSP (3-band overdrive)
//! -> bus compressor -> shift / limiter gain envelope. Then the car's whole mix goes through makeup gain and a
//! look-ahead peak limiter instead of the old per-sample tanh.
//!
//! What is from the disc: every parameter value (ET `FocusPEQ`, `Distortion Level/VolumeCompensate`, `TrashDSP
//! Cutoff1/2, BandN effecttype/inputgain/overdrive/mix/outputgain`, `ShiftVolumeScalar`, `BurbleVolume`,
//! `BackfireVolume`, `blowoffL1Choice`). The xex registers TrashDSP as an FMOD DSP plugin (param table 831D8440).
//! INFERRED (not decoded from the xex): the shaper curves (Distortion = FMOD's (1+k)x/(1+k|x|), k = 2L/(1-L);
//! Trash effect 1 = tanh, 2 = hard knee), overdrive read as dB of drive, VolumeCompensate as a 1/(1+vc) output
//! cut, and the drive being taken at a fixed reference level (the bus is normalised by its own envelope so the
//! amount of grit does not depend on how loud the car happens to be).
//!
//! Deliberately EXAGGERATED (user, 2026-10-07: "we want it exaggerated even"): grit, makeup, burbles, shift kicks and
//! the limiter bounce are pushed past the data. Knobs: `FH1_AUDIO_CHAR=0` = the old clean chain, `FH1_AUDIO_GRIT`
//! (default 1.0 = our push, 0 = clean), `FH1_AUDIO_POPS` (burbles/backfires, default 0.6), `FH1_AUDIO_TURBO` (turbo / blow-off over the game's, default 1.0), `FH1_AUDIO_ENGINE` (engine layers, default 1.3), `FH1_AUDIO_LOUD` (extra
//! makeup in dB, default 0).

use crate::synth::Biquad;
use crate::tuning::{Drive, EngineExtras};

#[derive(Debug, Clone, Copy)]
pub struct Knobs {
    pub on: bool,
    pub grit: f32,
    pub pops: f32,
    /// Extra makeup, dB.
    pub loud: f32,
    /// Turbo spool / blow-off gain over the game's (user 2026-10-07: "turbo and other effects are loud, I want to hear
    /// the raw engine more"; was a fixed 1.6 with the character chain on).
    pub turbo: f32,
    /// Engine layers' gain (user 2026-10-07: "more engine sound, by about 30 %").
    pub engine: f32,
}

pub fn knobs() -> &'static Knobs {
    static K: std::sync::OnceLock<Knobs> = std::sync::OnceLock::new();
    K.get_or_init(|| {
        let f = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Knobs {
            on: std::env::var("FH1_AUDIO_CHAR").map_or(true, |v| v != "0"),
            grit: f("FH1_AUDIO_GRIT", 1.0).max(0.0),
            // 0.6 since 2026-10-07 (was 1.0): pops / burbles under the raw engine.
            pops: f("FH1_AUDIO_POPS", 0.6).max(0.0),
            loud: f("FH1_AUDIO_LOUD", 0.0),
            turbo: f("FH1_AUDIO_TURBO", 1.0).max(0.0),
            engine: f("FH1_AUDIO_ENGINE", 1.3).max(0.0),
        }
    })
}

#[inline]
pub fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

/// Level the bus is normalised to before the shapers.
const REF: f32 = 0.3;

/// Engine bus processor (one per car).
#[derive(Default)]
pub struct EngineBus {
    focus: Option<Biquad>,
    lo: [Biquad; 2],
    hi: [Biquad; 2],
    /// Input envelope (RMS), compressor envelope.
    env: f32,
    comp_env: f32,
    /// Gains at the end of the last block (pre-normalisation, compressor x envelope).
    last_pre: f32,
    last_out: f32,
}

impl EngineBus {
    /// Processes `bus` in place. `env_gain` = shift / limiter envelope target for the end of this block.
    pub fn process(&mut self, bus: &mut [f32], rate: f32, x: &EngineExtras, d: &Drive, env_gain: f32) {
        let k = knobs();
        let frames = bus.len() / 2;
        if frames == 0 {
            return;
        }
        let dt = frames as f32 / rate;
        let rms = (bus.iter().map(|v| v * v).sum::<f32>() / bus.len() as f32).sqrt();
        self.env += (rms - self.env) * (1.0 - (-dt / 0.05).exp());
        let pre = (REF / self.env.max(1e-4)).clamp(0.5, 40.0);
        if self.last_pre == 0.0 {
            self.last_pre = pre;
        }

        // Block parameters.
        if let Some(p) = &x.focus_peq {
            let f = self.focus.get_or_insert_with(Biquad::pass);
            f.set_peak(rate, p.freq.eval(d).clamp(20.0, 18000.0), p.gain.eval(d), p.bandwidth.eval(d));
        }
        // Distortion: data level, pushed by grit (0.75 typical -> k ~ 6).
        let (dist_k, dist_mix, dist_norm, dist_vc) = match &x.distortion {
            Some(ds) => {
                let l = ds.level.eval(d).clamp(0.0, 0.95);
                let kk = 2.0 * l / (1.0 - l);
                (kk, (l * 0.8 * k.grit).clamp(0.0, 1.0), (1.0 + kk * REF) / (1.0 + kk), 1.0 / (1.0 + ds.volume_compensate.max(-0.5)))
            }
            // audio-1 installs: a moderate default so every car gets some grit.
            None => (3.0, (0.5 * k.grit).clamp(0.0, 1.0), (1.0 + 3.0 * REF) / 4.0, 1.0),
        };
        struct Band {
            effect: u8,
            g: f32,
            drive: f32,
            mix: f32,
            out: f32,
        }
        let trash = x.trash.as_ref().map(|t| {
            self.lo.iter_mut().for_each(|b| b.set_lowpass(rate, t.cutoff1.clamp(40.0, 8000.0), std::f32::consts::FRAC_1_SQRT_2));
            self.hi.iter_mut().for_each(|b| b.set_highpass(rate, t.cutoff2.clamp(200.0, 16000.0), std::f32::consts::FRAC_1_SQRT_2));
            let bands = t.bands.each_ref().map(|b| Band {
                effect: b.effect,
                g: db(b.input_gain.eval(d).clamp(-24.0, 24.0)),
                drive: db((b.overdrive.eval(d) * (0.5 + 0.75 * k.grit)).clamp(0.0, 30.0)),
                mix: (b.mix.eval(d) / 100.0 * k.grit.min(1.5)).clamp(0.0, 1.0),
                out: db(b.output_gain.eval(d).clamp(-24.0, 24.0)),
            });
            (bands, 1.0 / (1.0 + t.volume_compensate.max(-0.5)))
        });

        // Compressor (the ET's is threshold 0 dB / 50 ms; ours actually works: glue + idle lift).
        let thr = db(-24.0);
        let ratio = 2.5;
        let comp_gain = if self.comp_env > thr { (self.comp_env / thr).powf(1.0 / ratio - 1.0) } else { 1.0 };
        let out_gain = comp_gain * env_gain;
        if self.last_out == 0.0 {
            self.last_out = out_gain;
        }

        let mut post_e = 0.0f32;
        for f in 0..frames {
            let t = (f + 1) as f32 / frames as f32;
            let p = self.last_pre + (pre - self.last_pre) * t;
            let og = self.last_out + (out_gain - self.last_out) * t;
            for ch in 0..2 {
                let mut v = bus[2 * f + ch] * p;
                if let Some(fq) = &mut self.focus {
                    v = fq.run(ch, v);
                }
                if dist_mix > 0.0 {
                    let wet = (1.0 + dist_k) * v / (1.0 + dist_k * v.abs()) * dist_norm;
                    v = (v + (wet - v) * dist_mix) * dist_vc;
                }
                if let Some((bands, vc)) = &trash {
                    let lo0 = self.lo[0].run(ch, v);
                    let lo = self.lo[1].run(ch, lo0);
                    let hi0 = self.hi[0].run(ch, v);
                    let hi = self.hi[1].run(ch, hi0);
                    let parts = [lo, v - lo - hi, hi];
                    let mut sum = 0.0;
                    for (b, s) in bands.iter().zip(parts) {
                        let z = s * b.g * b.drive;
                        let shaped = match b.effect {
                            1 => z.tanh(),
                            2 => z / (1.0 + z * z * z * z).sqrt().sqrt(),
                            _ => z,
                        } / b.drive;
                        sum += (s + (shaped - s) * b.mix) * b.out;
                    }
                    v = sum * vc;
                }
                let o = v / p;
                post_e += o * o;
                bus[2 * f + ch] = o * og;
            }
        }
        let post = (post_e / bus.len() as f32).sqrt();
        let (att, rel) = (0.015, 0.15);
        let tc = if post > self.comp_env { att } else { rel };
        self.comp_env += (post - self.comp_env) * (1.0 - (-dt / tc).exp());
        self.last_pre = pre;
        self.last_out = out_gain;
    }
}

/// Look-ahead peak limiter (2 ms), stereo interleaved.
pub struct Limiter {
    delay: Vec<[f32; 2]>,
    idx: usize,
    env: f32,
    gain: f32,
    pub ceiling: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter { delay: Vec::new(), idx: 0, env: 0.0, gain: 1.0, ceiling: db(-1.0) }
    }
}

impl Limiter {
    pub fn process(&mut self, buf: &mut [f32], rate: f32) {
        let n = ((0.002 * rate) as usize).max(1);
        if self.delay.len() != n {
            self.delay = vec![[0.0; 2]; n];
            self.idx = 0;
        }
        let rel = (-1.0 / (0.12 * rate)).exp();
        let att = (-1.0 / (0.0006 * rate)).exp();
        for fr in buf.chunks_exact_mut(2) {
            let x = [fr[0], fr[1]];
            let peak = x[0].abs().max(x[1].abs());
            self.env = peak.max(self.env * rel);
            let target = if self.env > self.ceiling { self.ceiling / self.env } else { 1.0 };
            let c = if target < self.gain { att } else { rel };
            self.gain = target + (self.gain - target) * c;
            let y = std::mem::replace(&mut self.delay[self.idx], x);
            self.idx = (self.idx + 1) % n;
            fr[0] = (y[0] * self.gain).clamp(-1.0, 1.0);
            fr[1] = (y[1] * self.gain).clamp(-1.0, 1.0);
        }
    }
}
