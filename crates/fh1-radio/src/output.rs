//! Plays the radio [`Mixer`] on its own output stream, separate from the car audio, on the
//! device `fh1_audio::output::device()` picks (`FH1_AUDIO_DEVICE`, else the system default).
//!
//! A producer thread renders ~150 ms ahead into a ring buffer, so MP3 decoding and the file
//! opens/seeks of a station change never run inside the device callback. Like the car audio,
//! the stream re-opens on the new default within ~1 s when the system default changes, and
//! after a stream error (cpal binds a device once, at stream open). The handle is
//! `Send + Sync` and can live in a Bevy resource.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use fh1_audio::output::{device, device_name};

use crate::install::RadioData;
use crate::mixer::{Mixer, NowPlaying};

/// How far ahead the producer renders.
const AHEAD_S: f32 = 0.15;
const CHUNK: usize = 512;

struct Shared {
    mixer: Mutex<Mixer>,
    ring: Mutex<VecDeque<f32>>,
    now: Mutex<NowPlaying>,
    /// Set by the stream's error callback; the device thread re-opens.
    broken: AtomicBool,
    stop: AtomicBool,
}

pub struct RadioPlayer {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    pub sample_rate: u32,
    /// Output device name at start.
    pub device: String,
}

impl RadioPlayer {
    /// `dir` = the installed `radio` folder.
    pub fn start(dir: PathBuf, lang: &str, seed: u64) -> Result<RadioPlayer> {
        let mut data = RadioData::load(&dir)?;
        if let Some(mods) = crate::mods::find_dir(&dir) {
            match crate::mods::add_stations(&mut data, &mods) {
                Ok(n) if n > 0 => eprintln!("radio: {n} mod station(s) from {}", mods.display()),
                Ok(_) => {}
                Err(e) => eprintln!("radio: mod stations: {e:#}"),
            }
        }
        let data = Arc::new(data);
        // The real rate is set once the stream is open.
        let mut mixer = Mixer::new(data, dir, lang, 48_000, seed);
        // StartRadioFlowForFreeRoam(0, not immediate): fade up over levelFadeUpTime.
        mixer.system().start_free_roam(0.0, false);
        let shared = Arc::new(Shared {
            now: Mutex::new(mixer.now_playing()),
            mixer: Mutex::new(mixer),
            ring: Mutex::default(),
            broken: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = std::sync::mpsc::channel::<Result<(u32, String)>>();
        let device_thread = {
            let shared = shared.clone();
            std::thread::Builder::new().name("fh1-radio-out".into()).spawn(move || {
                let (mut stream, mut name) = match open_stream(&shared) {
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
                let mut last_check = Instant::now();
                while !shared.stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(Duration::from_millis(100));
                    if last_check.elapsed().as_secs_f32() < 1.0 {
                        continue;
                    }
                    last_check = Instant::now();
                    let moved = !pinned && device().ok().map(|d| device_name(&d)).is_some_and(|d| d != name);
                    if moved || shared.broken.swap(false, Ordering::Relaxed) {
                        drop(stream.take());
                        match open_stream(&shared) {
                            Ok((s, rate, n)) => {
                                eprintln!("fh1-radio: output moved to \"{n}\" ({rate} Hz)");
                                stream = Some(s);
                                name = n;
                            }
                            Err(e) => eprintln!("fh1-radio: reopening output failed: {e:#}"),
                        }
                    }
                }
                drop(stream);
            })?
        };
        let (rate, device) = rx.recv().context("radio output thread died")??;
        let producer = {
            let shared = shared.clone();
            std::thread::Builder::new().name("fh1-radio-mix".into()).spawn(move || {
                let mut buf = vec![0f32; CHUNK * 2];
                while !shared.stop.load(Ordering::Relaxed) {
                    let ahead = {
                        let rate = shared.mixer.lock().unwrap().rate();
                        (AHEAD_S * rate as f32) as usize * 2
                    };
                    if shared.ring.lock().unwrap().len() >= ahead {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    let np = {
                        let mut m = shared.mixer.lock().unwrap();
                        m.render(&mut buf);
                        m.now_playing()
                    };
                    *shared.now.lock().unwrap() = np;
                    shared.ring.lock().unwrap().extend(buf.iter().copied());
                }
            })?
        };
        Ok(RadioPlayer { shared, threads: vec![device_thread, producer], sample_rate: rate, device })
    }

    /// Runs `f` on the mixer (tune, step, volume, DJ option, gameplay triggers).
    pub fn with<R>(&self, f: impl FnOnce(&mut Mixer) -> R) -> R {
        f(&mut self.shared.mixer.lock().unwrap())
    }

    /// [`Self::with`] without waiting: None while the mix thread holds the mixer (it opens and decodes the next song file
    /// under the lock). For the game's main thread, which must never block on the radio (a slow file open froze frames).
    pub fn try_with<R>(&self, f: impl FnOnce(&mut Mixer) -> R) -> Option<R> {
        self.shared.mixer.try_lock().ok().map(|mut m| f(&mut m))
    }

    pub fn now_playing(&self) -> NowPlaying {
        self.shared.now.lock().unwrap().clone()
    }
}

impl Drop for RadioPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Opens the chosen device and points the mixer at its rate (dropping audio rendered for the
/// previous device).
fn open_stream(shared: &Arc<Shared>) -> Result<(cpal::Stream, u32, String)> {
    let device = device()?;
    let name = device_name(&device);
    let supported = device.default_output_config()?;
    anyhow::ensure!(
        supported.sample_format() == cpal::SampleFormat::F32,
        "output device format {:?} (only f32 supported)",
        supported.sample_format()
    );
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate as u32;
    let channels = config.channels as usize;
    {
        let mut m = shared.mixer.lock().unwrap();
        if m.rate() != rate {
            m.set_rate(rate);
        }
        shared.ring.lock().unwrap().clear();
    }
    let (cb, err) = (shared.clone(), shared.clone());
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _| {
            let mut ring = cb.ring.lock().unwrap();
            for out in data.chunks_mut(channels) {
                out.fill(0.0);
                // Underrun = silence (the producer catches up).
                let (Some(l), Some(r)) = (ring.pop_front(), ring.pop_front()) else { continue };
                if channels > 1 {
                    out[0] = l;
                    out[1] = r;
                } else {
                    out[0] = 0.5 * (l + r);
                }
            }
        },
        move |e| {
            eprintln!("fh1-radio: stream error: {e}");
            err.broken.store(true, Ordering::Relaxed);
        },
        None,
    )?;
    stream.play()?;
    Ok((stream, rate, name))
}
