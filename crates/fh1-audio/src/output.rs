//! Plays a [`CarSound`] on the default output device (cpal, its own thread so the handle is
//! `Send + Sync` and can live in a Bevy resource).
//!
//! Device: Windows' default output (followed live: if the default changes or the device
//! disappears, the stream re-opens on the new one within ~1 s), or the first device whose name contains
//! `FH1_AUDIO_DEVICE` (case-insensitive), e.g. `FH1_AUDIO_DEVICE=headphones`. [`device`] is
//! shared so every stream in the game (car, radio) lands on the same device.
//!
//! Debug: `FH1_AUDIO_CAPTURE=<file>` also writes everything played as raw s16le stereo at the
//! device rate (printed at start), for checking the in-game mix offline.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::synth::{Backfire, CarInput, CarSound};

/// Render block: parameters are re-evaluated this often (~5 ms at 48 kHz).
const BLOCK: usize = 256;

/// Other cars heard at once (AI / traffic), mixed on top of the player's car.
pub const MAX_VOICES: usize = 8;

/// One other car: its sound, input and where it sits in the mix.
#[derive(Default)]
struct OtherCar {
    sound: Option<CarSound>,
    input: CarInput,
    /// Distance gain (0..1) and pan (-1 = left .. 1 = right).
    gain: f32,
    pan: f32,
    /// Gain/pan of the last block (ramped per block so voices don't click).
    last: (f32, f32),
}

struct Shared {
    input: Mutex<CarInput>,
    sound: Mutex<Option<CarSound>>,
    others: Mutex<Vec<OtherCar>>,
    /// Set by the stream's error callback (device unplugged etc.).
    broken: AtomicBool,
    /// Pops / bangs played: (None = the player's car, Some(slot) = another car's voice), capped.
    backfires: Mutex<Vec<(Option<usize>, Backfire)>>,
}

pub struct Player {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub sample_rate: u32,
    /// Device at start (the stream may later follow a new Windows default).
    pub device: String,
}

/// Human-readable device name.
pub fn device_name(d: &cpal::Device) -> String {
    d.description().map(|x| x.to_string()).unwrap_or_else(|_| "?".into())
}

/// All output device names, the default first.
pub fn device_names() -> Vec<String> {
    let host = cpal::default_host();
    let default = host.default_output_device().map(|d| device_name(&d));
    let mut v: Vec<String> = host.output_devices().map(|ds| ds.map(|d| device_name(&d)).collect()).unwrap_or_default();
    if let Some(d) = default {
        v.retain(|n| *n != d);
        v.insert(0, d);
    }
    v
}

/// The output device to use: `FH1_AUDIO_DEVICE` (name substring) if set and found, else the
/// system default.
pub fn device() -> Result<cpal::Device> {
    let host = cpal::default_host();
    if let Some(want) = std::env::var("FH1_AUDIO_DEVICE").ok().filter(|s| !s.is_empty()) {
        let want = want.to_lowercase();
        if let Some(d) = host.output_devices()?.find(|d| device_name(d).to_lowercase().contains(&want)) {
            return Ok(d);
        }
        eprintln!("fh1-audio: no output device matching {want:?}; have {:?}", device_names());
    }
    host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))
}

impl Player {
    pub fn start() -> Result<Player> {
        let others = Mutex::new((0..MAX_VOICES).map(|_| OtherCar::default()).collect());
        let shared = Arc::new(Shared { input: Mutex::default(), sound: Mutex::default(), others, broken: AtomicBool::new(false), backfires: Mutex::default() });
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(u32, String)>>();
        let thread = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::Builder::new().name("fh1-audio".into()).spawn(move || {
                let (mut stream, mut name) = match open_stream(shared.clone()) {
                    Ok((s, rate, name)) => {
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
                while !stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(std::time::Duration::from_millis(100));
                    if last_check.elapsed().as_secs_f32() < 1.0 {
                        continue;
                    }
                    last_check = std::time::Instant::now();
                    let moved = !pinned && device().ok().map(|d| device_name(&d)).is_some_and(|d| d != name);
                    if moved || shared.broken.swap(false, Ordering::Relaxed) {
                        drop(stream.take());
                        match open_stream(shared.clone()) {
                            Ok((s, rate, n)) => {
                                eprintln!("fh1-audio: output moved to \"{n}\" ({rate} Hz)");
                                stream = Some(s);
                                name = n;
                            }
                            Err(e) => eprintln!("fh1-audio: reopening output failed: {e:#}"),
                        }
                    }
                }
                drop(stream);
            })?
        };
        let (sample_rate, device) = rx.recv().context("audio thread died")??;
        Ok(Player { shared, stop, thread: Some(thread), sample_rate, device })
    }

    /// Swaps the car being played (`None` = silence).
    pub fn set_sound(&self, sound: Option<CarSound>) {
        *self.shared.sound.lock().unwrap() = sound;
    }

    /// Pops / bangs played since the last call: (None = the player's car, Some(slot) = other car voice `slot`).
    pub fn take_backfires(&self) -> Vec<(Option<usize>, Backfire)> {
        std::mem::take(&mut *self.shared.backfires.lock().unwrap())
    }

    pub fn set_input(&self, input: CarInput) {
        *self.shared.input.lock().unwrap() = input;
    }

    /// Put another car's sound in voice `slot` (< [`MAX_VOICES`]; `None` = free the slot).
    pub fn set_other_sound(&self, slot: usize, sound: Option<CarSound>) {
        if let Some(o) = self.shared.others.lock().unwrap().get_mut(slot) {
            o.sound = sound;
            o.last = (0.0, o.pan);
        }
    }

    /// Another car's state: its input (`external` is set here), distance gain 0..1 and pan -1..1.
    pub fn set_other_input(&self, slot: usize, input: CarInput, gain: f32, pan: f32) {
        if let Some(o) = self.shared.others.lock().unwrap().get_mut(slot) {
            o.input = CarInput { external: true, ..input };
            o.gain = gain;
            o.pan = pan.clamp(-1.0, 1.0);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn open_stream(shared: Arc<Shared>) -> Result<(cpal::Stream, u32, String)> {
    let device = device()?;
    let supported = device.default_output_config()?;
    anyhow::ensure!(
        supported.sample_format() == cpal::SampleFormat::F32,
        "output device format {:?} (only f32 supported)",
        supported.sample_format()
    );
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate as u32;
    let channels = config.channels as usize;
    let shared_err = shared.clone();
    let mut stereo = vec![0f32; BLOCK * 2];
    let mut voice = vec![0f32; BLOCK * 2];
    // Next frame of `stereo` to hand to the device; BLOCK = render a new block.
    let mut cursor = BLOCK;
    let mut input = CarInput::default();
    let mut limiter = crate::character::Limiter::default();
    let (mut fired, mut scratch) = (Vec::new(), Vec::new());
    let mut capture = std::env::var_os("FH1_AUDIO_CAPTURE").and_then(|p| {
        eprintln!("fh1-audio: capturing to {} (s16le stereo {rate} Hz)", p.to_string_lossy());
        std::fs::File::create(p).ok().map(std::io::BufWriter::new)
    });
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _| {
            for out in data.chunks_mut(channels) {
                if cursor == BLOCK {
                    if let Ok(i) = shared.input.try_lock() {
                        input = *i;
                    }
                    // `fired` is cleared only once handed over (below): a contended queue lock no longer drops flames for
                    // pops that played.
                    if fired.len() > 64 {
                        fired.clear();
                    }
                    match shared.sound.try_lock().as_deref_mut() {
                        Ok(Some(s)) => {
                            s.render(&input, &mut stereo, rate as f32);
                            s.take_backfires(&mut scratch);
                            fired.extend(scratch.drain(..).map(|b| (None, b)));
                        }
                        _ => stereo.fill(0.0),
                    }
                    // Other cars: each rendered on its own, then panned (equal power) and attenuated, gain / pan ramped
                    // over the block so they don't click.
                    if let Ok(mut others) = shared.others.try_lock() {
                        let mut any = false;
                        for (k, o) in others.iter_mut().enumerate() {
                            let Some(s) = o.sound.as_mut() else { continue };
                            if o.gain < 1e-4 && o.last.0 < 1e-4 {
                                continue;
                            }
                            s.render(&o.input, &mut voice, rate as f32);
                            s.take_backfires(&mut scratch);
                            // Another car's pop is as loud as its voice gain lets it be: inaudible ones fire no flame.
                            let g = o.gain.max(o.last.0) * std::f32::consts::SQRT_2;
                            let sync = crate::synth::backfire_sync();
                            fired.extend(scratch.drain(..).filter_map(|mut b| {
                                b.level *= g;
                                (!sync || b.level >= crate::synth::BACKFIRE_MIN_LEVEL).then_some((Some(k), b))
                            }));
                            let (g0, p0) = o.last;
                            for f in 0..BLOCK {
                                let t = f as f32 / BLOCK as f32;
                                let g = (g0 + (o.gain - g0) * t) * std::f32::consts::SQRT_2;
                                let a = 0.5 * (p0 + (o.pan - p0) * t + 1.0) * std::f32::consts::FRAC_PI_2;
                                stereo[2 * f] += voice[2 * f] * g * a.cos();
                                stereo[2 * f + 1] += voice[2 * f + 1] * g * a.sin();
                            }
                            o.last = (o.gain, o.pan);
                            any = true;
                        }
                        if any {
                            if crate::character::knobs().on {
                                // The player's car is limited on its own; this catches the sum with the other cars.
                                limiter.process(&mut stereo, rate as f32);
                            } else {
                                for x in stereo.iter_mut() {
                                    *x = x.clamp(-1.0, 1.0);
                                }
                            }
                        }
                    }
                    if !fired.is_empty() {
                        if let Ok(mut q) = shared.backfires.try_lock() {
                            // Nobody draining (no game): keep the newest only.
                            if q.len() > 256 {
                                q.clear();
                            }
                            q.append(&mut fired);
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
        {
            let shared = shared_err;
            move |e| {
                eprintln!("fh1-audio: stream error: {e}");
                shared.broken.store(true, Ordering::Relaxed);
            }
        },
        None,
    )?;
    stream.play()?;
    Ok((stream, rate, device_name(&device)))
}
