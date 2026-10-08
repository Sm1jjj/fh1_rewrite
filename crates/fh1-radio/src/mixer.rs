//! Plays [`RadioSystem`] (the game's radio logic): drives it at the game's 0.03 s update rate,
//! decodes what its four channels play (music, DJ, festival lead, ident) and mixes them into
//! interleaved stereo f32 at any output rate. Device-independent (`output` plays it,
//! `examples/radio_render.rs` writes a WAV).
//!
//! Also here, because the radio needs them every update:
//! - the voice-activity duck input: mean square of the last 256 samples of the DJ (else lead)
//!   channel > 0.001 (Update 0x82BDAD10 reads `getWaveData`). Measured on the decoded clip before
//!   its volume (INFERRED: FMOD's channel wave data);
//! - the mixer snapshots' GameMusic / DJModifier levels ([`crate::snapshots`]), including the
//!   radio's own Radio-group snapshot.
//!
//! Volumes ramp linearly across each update to avoid zipper noise; stopped clips are cut with a
//! 2.7 ms ramp (the game hard-stops its channels).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config;
use crate::decode::Mp3Stream;
use crate::install::RadioData;
use crate::snapshots::Mix;
use crate::system::{Bank, HudPost, Play, RadioSystem, TICK};

/// What the HUD shows (kept for the engine's simple card; the game's own HUD is driven by
/// [`HudPost`]s, see [`Mixer::take_hud_posts`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NowPlaying {
    /// `Radio1` .. or `Radio4_Silent`.
    pub station: String,
    /// "Horizon Bass Arena", or "Radio Off".
    pub display: String,
    pub dj: Option<String>,
    pub off: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// Changes whenever a new song starts (or the station changes).
    pub song_key: u64,
    /// Changes whenever the station changes.
    pub station_key: u64,
    /// Seconds since this song started being heard.
    pub heard_for: f64,
}

const DECLICK: usize = 128;
const VAD_WINDOW: usize = 256;

struct Voice {
    id: u64,
    stream: Option<Mp3Stream>,
    buf: Vec<f32>,
    pos: f64,
    step: f64,
    finished: bool,
}

impl Voice {
    fn open(dir: &std::path::Path, data: &RadioData, lang: &str, p: &Play, rate: u32) -> Option<Voice> {
        let clip = match p.bank {
            Bank::Music => data.music.get(&p.clip),
            Bank::Vo => data.vo.get(lang).and_then(|b| b.get(&p.clip)),
        }?;
        let start = (p.pos_ms.max(0.0) * clip.rate as f64 / 1000.0) as u64;
        Some(Voice {
            id: p.id,
            stream: Mp3Stream::open(&dir.join(&clip.file), start).ok(),
            buf: Vec::new(),
            pos: 0.0,
            step: clip.rate as f64 / rate as f64,
            finished: false,
        })
    }

    /// Next frame at the output rate (linear resampling).
    fn next(&mut self) -> [f32; 2] {
        let i = self.pos as usize;
        while (i + 2) * 2 > self.buf.len() {
            let Some(s) = self.stream.as_mut() else { break };
            if !s.decode_into(&mut self.buf) {
                self.stream = None;
            }
        }
        if i * 2 + 1 >= self.buf.len() {
            self.finished = true;
            return [0.0; 2];
        }
        let t = (self.pos - i as f64) as f32;
        let a = [self.buf[i * 2], self.buf[i * 2 + 1]];
        let b = if (i + 1) * 2 + 1 < self.buf.len() { [self.buf[i * 2 + 2], self.buf[i * 2 + 3]] } else { a };
        self.pos += self.step;
        let used = self.pos as usize;
        if used > 8192 {
            self.buf.drain(..used * 2);
            self.pos -= used as f64;
        }
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
    }
}

/// A channel slot: the live voice, its gain ramp, and a cut-off previous voice.
#[derive(Default)]
struct Slot {
    voice: Option<Voice>,
    gain: f32,
    target: f32,
    old: Option<(Voice, f32)>,
}

pub struct Mixer {
    core: RadioSystem,
    data: Arc<RadioData>,
    dir: PathBuf,
    lang: String,
    rate: u32,
    mix: Mix,
    /// music, dj, lead, ident
    slots: [Slot; 4],
    /// Output frames left in the current update, and the fractional carry.
    tick_left: usize,
    tick_carry: f64,
    vad: VecDeque<f32>,
    /// Profile "RADIO VOLUME" (0..1).
    pub volume: f32,
    /// 3D venue attenuation (source/listener distance, min, max): FMOD inverse rolloff.
    pub distance_3d: Option<(f32, f32, f32)>,
    station_key: u64,
    song_key: u64,
    last_song: Option<(usize, usize)>,
    song_since: f64,
}

impl Mixer {
    /// `dir` = the installed `radio` folder. `lang` = VO language (`EN`, `DE`, ...; falls back to EN).
    pub fn new(data: Arc<RadioData>, dir: PathBuf, lang: &str, rate: u32, seed: u64) -> Mixer {
        let lang = if data.vo.contains_key(lang) { lang.to_owned() } else { "EN".to_owned() };
        let core = RadioSystem::new(data.clone(), &lang, seed as u32 ^ (seed >> 32) as u32);
        Mixer {
            mix: Mix::new(&data.snapshots),
            core,
            dir,
            lang,
            rate,
            slots: Default::default(),
            tick_left: 0,
            tick_carry: 0.0,
            vad: VecDeque::with_capacity(VAD_WINDOW),
            volume: 1.0,
            distance_3d: None,
            station_key: 1,
            song_key: 1,
            last_song: None,
            song_since: 0.0,
            data,
        }
    }

    pub fn data(&self) -> &RadioData {
        &self.data
    }

    /// The game's radio logic: flows, triggers, options, station selection.
    pub fn system(&mut self) -> &mut RadioSystem {
        &mut self.core
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Switches the output rate (the device changed); playing clips carry on.
    pub fn set_rate(&mut self, rate: u32) {
        for s in &mut self.slots {
            if let Some(v) = &mut s.voice {
                v.step = v.step * self.rate as f64 / rate as f64;
            }
        }
        self.rate = rate;
    }

    pub fn clock(&self) -> f64 {
        self.core.clock()
    }

    /// Makes a mixer snapshot active (e.g. `Paused`, `UIExterior`, `VOPlaying`, `SatNavPlaying`,
    /// `Festival`); one per group, see [`crate::snapshots`].
    pub fn set_snapshot(&mut self, name: &str) {
        self.mix.set(&self.data.snapshots, name);
    }

    /// The radio's own Radio-group snapshot (RadioDJSpeaking / RadioFestivalUpdate / RadioNormal),
    /// which in the game also turns the cars down and mutes the satnav.
    pub fn radio_snapshot(&self) -> &'static str {
        self.core.snapshot
    }

    pub fn take_hud_posts(&mut self) -> Vec<HudPost> {
        self.core.take_hud_posts()
    }

    // ----- compatibility with the first engine integration -----

    /// Tunes to dial position `pos` at once (0..3 = Radio1..3, Off) and starts the radio.
    pub fn tune(&mut self, pos: usize) {
        self.core.select_external(pos, false, 0);
        self.ensure_started();
    }

    /// D-pad right (+1) / left (-1), with the game's cooldown and lock.
    pub fn step(&mut self, delta: i32) {
        self.ensure_started();
        self.core.dpad(delta < 0);
    }

    fn ensure_started(&mut self) {
        if !self.core.flow_running() {
            self.core.start_free_roam(0.0, false);
        }
    }

    pub fn now_playing(&self) -> NowPlaying {
        let idx = self.core.station();
        let st = self.core.station_def();
        let (display, dj) = match config::station_display_name(&st.name) {
            Some((d, dj)) => (d.to_owned(), Some(dj.to_owned())),
            None if st.is_off => ("Radio Off".to_owned(), None),
            None => (st.name.clone(), None),
        };
        let track = self.core.track().filter(|t| t.0 == idx && !st.is_off).map(|t| &self.data.radio.stations[t.0].playlist.items[t.1]);
        NowPlaying {
            station: st.name.clone(),
            display,
            dj,
            off: st.is_off,
            title: track.map(|t| t.title.clone()),
            artist: track.map(|t| t.artist.clone()),
            song_key: self.song_key,
            station_key: self.station_key,
            heard_for: self.core.clock() - self.song_since,
        }
    }

    pub fn station(&self) -> usize {
        self.core.station()
    }

    // ----- rendering -----

    /// Renders interleaved stereo into `out`.
    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        let frames = out.len() / 2;
        let mut f = 0;
        while f < frames {
            if self.tick_left == 0 {
                self.tick();
            }
            let n = self.tick_left.min(frames - f);
            self.render_frames(&mut out[f * 2..(f + n) * 2]);
            self.tick_left -= n;
            f += n;
        }
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
    }

    fn tick(&mut self) {
        let station_before = self.core.station();
        self.mix.set(&self.data.snapshots, self.core.snapshot);
        let (music_mix, dj_mix) = self.mix.levels();
        self.core.inputs.music_mix = music_mix;
        self.core.inputs.dj_mix = dj_mix;
        self.core.inputs.radio_volume = self.volume;
        let vad = self.vad.len() == VAD_WINDOW && self.vad.iter().map(|x| x * x).sum::<f32>() / VAD_WINDOW as f32 > 0.001;
        self.core.update(vad);
        self.mix.step(TICK as f32);
        if self.core.station() != station_before {
            self.station_key += 1;
            self.song_key += 1;
            self.song_since = self.core.clock();
            self.last_song = None;
        }
        if self.core.track().is_some() && self.core.track() != self.last_song {
            if self.last_song.is_some() {
                self.song_key += 1;
                self.song_since = self.core.clock();
            }
            self.last_song = self.core.track();
        }

        // Follow the channels.
        let plays = [self.core.music.clone(), self.core.dj.clone(), self.core.lead.clone(), self.core.ident.clone()];
        let v = self.core.volumes;
        let att = match (self.core.is_3d, self.distance_3d) {
            (true, Some((d, min, max))) => min.max(1e-3) / d.clamp(min.max(1e-3), max.max(min)),
            _ => 1.0,
        };
        let gains = [v.music * att, v.dialogue, v.dialogue, v.ident * att];
        for (i, p) in plays.iter().enumerate() {
            let slot = &mut self.slots[i];
            let live = slot.voice.as_ref().map(|v| v.id);
            if live != p.as_ref().map(|p| p.id) {
                if let Some(old) = slot.voice.take() {
                    slot.old = Some((old, slot.gain));
                }
                slot.voice = p.as_ref().and_then(|p| Voice::open(&self.dir, &self.data, &self.lang, p, self.rate));
                slot.gain = gains[i];
            }
            slot.target = gains[i];
        }
        let exact = TICK * self.rate as f64 + self.tick_carry;
        self.tick_left = exact.floor().max(1.0) as usize;
        self.tick_carry = exact - self.tick_left as f64;
    }

    fn render_frames(&mut self, out: &mut [f32]) {
        let n = out.len() / 2;
        let paused = self.core.paused();
        let voice_slot = if self.slots[1].voice.is_some() { 1 } else { 2 };
        let tick_frames = (TICK * self.rate as f64) as usize;
        let done = tick_frames.saturating_sub(self.tick_left);
        for (si, slot) in self.slots.iter_mut().enumerate() {
            // Cut-off voice: short ramp out.
            if let Some((v, g)) = &mut slot.old {
                for f in 0..n.min(DECLICK) {
                    let k = *g * (1.0 - f as f32 / DECLICK as f32);
                    let s = v.next();
                    out[f * 2] += s[0] * k;
                    out[f * 2 + 1] += s[1] * k;
                }
                slot.old = None;
            }
            if paused {
                continue;
            }
            let Some(v) = &mut slot.voice else { continue };
            if v.finished {
                continue;
            }
            // Gain ramps from the previous update's value to this one's across the update.
            let (g0, g1) = (slot.gain, slot.target);
            for f in 0..n {
                let k = ((done + f + 1) as f32 / tick_frames.max(1) as f32).min(1.0);
                let g = g0 + (g1 - g0) * k;
                let s = v.next();
                if si == voice_slot {
                    if self.vad.len() == VAD_WINDOW {
                        self.vad.pop_front();
                    }
                    self.vad.push_back(0.5 * (s[0] + s[1]));
                }
                out[f * 2] += s[0] * g;
                out[f * 2 + 1] += s[1] * g;
            }
            if done + n >= tick_frames {
                slot.gain = g1;
            }
        }
        if self.slots[1].voice.is_none() && self.slots[2].voice.is_none() {
            self.vad.clear();
        }
    }
}
