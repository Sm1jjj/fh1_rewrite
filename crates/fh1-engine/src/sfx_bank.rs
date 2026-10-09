//! Shared world-SFX helper (W9): FEV cache, lazily loaded sample cache, the 3D listener, spatial gain / pan law and
//! `play_event` on fh1-audio's software mixer (`fh1_audio::pcm::shared().ambient()`). Used by ambience.rs, the impact /
//! race / horn / VO players.
//!
//! - [`SfxBank`] (resource, inserted at Startup): `fev(project)` loads `<assets>/audio/fev/<project>.fev` once (cached,
//!   failures too); `sample(bank, index)` returns the decoded `<assets>/audio/banks/<bank>/<index:03>.wav` when a worker
//!   thread has loaded it, else `None` and queues the load (`prefetch` = queue only). `play_event` picks a wave by
//!   weight and applies the event volume / pitch, the random ranges and the 3D gain / pan.
//! - [`Listener`] (resource): position and axes of the `FxPostCamera`, refreshed in PreUpdate.
//! - [`world_sfx_allowed`]: mirrors audio.rs gating (agent launch silence, M mute, pause, loading covers).
//! - Bus gains (Update): Ambience = Settings.ambient_volume, Sfx = Settings.engine_volume, both x0 when gated.
//!
//! Flag: `FH1_SFX_BANK=0` = no mixer (every play returns None; the FEV / sample caches still work).
//! Tags: custom-rolloff events use their FEV `(distance)` volume curve (docs/FEV.md); for Inverse / Linear events the
//! law (inverse from min_dist, fade to zero over the last 10 % before max_dist) is a guess. `volume_rand_db` is applied as 0..-x dB and `pitch_rand` as +-x octaves
//! (both UNKNOWN units).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, Weak};

use bevy::prelude::*;
use fh1_audio::ambient::{AmbientMixer, Bus, Pcm, VoiceId, VoiceParams};
use fh1_audio::fev::{self, Fev};

use crate::ui::Settings;

pub struct SfxBankPlugin;

impl Plugin for SfxBankPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Listener>()
            .add_systems(Startup, start_bank)
            .add_systems(PreUpdate, update_listener)
            .add_systems(Update, bus_gains);
    }
}

/// The ears: the main camera (FxPostCamera), engine space.
#[derive(Resource, Default, Clone, Copy)]
pub struct Listener {
    pub pos: Vec3,
    pub right: Vec3,
    pub forward: Vec3,
}

/// How a 3D event's gain falls off. `Curve` = FMOD custom rolloff: the event's volume envelope on its `(distance)`
/// parameter, (metres, gain) points (docs/FEV.md; every Colorado ambience event uses it, VERIFIED).
#[derive(Clone, PartialEq, Debug)]
pub enum Roll {
    Inverse,
    Linear,
    Curve(Arc<[(f32, f32)]>),
}

/// Rolloff from the mode bits alone (Custom without its curve falls back to Inverse).
pub fn roll_of(r: &fev::Rolloff) -> Roll {
    match r {
        fev::Rolloff::Linear => Roll::Linear,
        _ => Roll::Inverse,
    }
}

/// Rolloff of an event, with its custom distance curve when it has one.
pub fn roll_for(ev: &fev::Event) -> Roll {
    match ev.rolloff {
        fev::Rolloff::Custom => ev.distance_curve().filter(|c| !c.is_empty()).map_or(Roll::Inverse, |c| Roll::Curve(c.into())),
        ref r => roll_of(r),
    }
}

/// Piecewise-linear lookup in (x, y) points sorted by x; held flat outside.
fn curve_at(c: &[(f32, f32)], x: f32) -> f32 {
    let Some(&(x0, y0)) = c.first() else { return 1.0 };
    if x <= x0 {
        return y0;
    }
    for w in c.windows(2) {
        let ((a, ya), (b, yb)) = (w[0], w[1]);
        if x <= b {
            return if b > a { ya + (yb - ya) * (x - a) / (b - a) } else { yb };
        }
    }
    c[c.len() - 1].1
}

/// Distance gain: 1 up to `min`, then per `roll`; fades to exactly 0 over the last 10 % before `max` and is 0 from `max`.
/// Non-increasing in `d`.
pub fn rolloff_gain(roll: &Roll, d: f32, min: f32, max: f32) -> f32 {
    if let Roll::Curve(c) = roll {
        // The curve is the whole law (FMOD custom rolloff ignores min / max distance).
        return curve_at(c, d).clamp(0.0, 1.0);
    }
    let min = min.max(0.01);
    let max = max.max(min + 0.01);
    if d >= max {
        return 0.0;
    }
    let mut g = match roll {
        Roll::Inverse => (min / d.max(1e-3)).min(1.0),
        Roll::Linear => 1.0 - (d - min).max(0.0) / (max - min),
        Roll::Curve(_) => 1.0,
    }
    .clamp(0.0, 1.0);
    let fade_start = min + (max - min) * 0.9;
    if d > fade_start {
        g *= ((max - d) / (max - fade_start)).clamp(0.0, 1.0);
    }
    g
}

/// (gain, pan) of a source at `pos` for the listener, inverse rolloff between `min_dist` and `max_dist`;
/// pan = dot(direction to the source, listener right), -1 left .. 1 right.
pub fn spatial(listener: &Listener, pos: Vec3, min_dist: f32, max_dist: f32) -> (f32, f32) {
    spatial_roll(listener, pos, min_dist, max_dist, &Roll::Inverse)
}

/// [`spatial`] with the event's own law (custom curve, min / max distance).
pub fn spatial_event(listener: &Listener, pos: Vec3, ev: &fev::Event) -> (f32, f32) {
    spatial_roll(listener, pos, ev.min_dist, ev.max_dist, &roll_for(ev))
}

pub fn spatial_roll(listener: &Listener, pos: Vec3, min_dist: f32, max_dist: f32, roll: &Roll) -> (f32, f32) {
    let to = pos - listener.pos;
    let d = to.length();
    let pan = if d > 1e-3 { (to / d).dot(listener.right).clamp(-1.0, 1.0) } else { 0.0 };
    (rolloff_gain(roll, d, min_dist, max_dist), pan)
}

static MUTED: AtomicBool = AtomicBool::new(false);

/// Whether world SFX may be heard: not an agent launch (`audio::audio_disabled`), not muted (M, toggled in this module
/// like audio.rs does), not paused (`paused` = `Time<Virtual>::is_paused()`) and no loading / menu cover.
pub fn world_sfx_allowed(paused: bool) -> bool {
    !crate::audio::audio_disabled() && !MUTED.load(Ordering::Relaxed) && !paused && crate::ui::loading::world_audio_allowed()
}

/// Result of [`SfxBank::play_event_ex`].
#[derive(Clone, Copy, Debug)]
pub struct Played {
    pub id: VoiceId,
    pub pitch: f32,
    /// Gain before the distance law (event volume x random x caller gain); per-frame updates use `base * spatial`.
    pub base: f32,
    pub duration_s: f32,
}

enum Slot {
    Loading,
    Ready(Arc<Pcm>),
    Failed,
}

struct Inner {
    dir: PathBuf,
    fevs: Mutex<HashMap<String, Option<Arc<Fev>>>>,
    samples: Mutex<HashMap<(String, u32), Slot>>,
    jobs: Mutex<Option<Sender<(String, u32)>>>,
    rng: Mutex<u64>,
}

#[derive(Resource)]
pub struct SfxBank {
    inner: Arc<Inner>,
    mixer: Option<AmbientMixer>,
}

impl SfxBank {
    fn new(dir: PathBuf, mixer: Option<AmbientMixer>) -> Self {
        let (tx, rx) = channel::<(String, u32)>();
        let inner = Arc::new(Inner {
            dir,
            fevs: Mutex::new(HashMap::new()),
            samples: Mutex::new(HashMap::new()),
            jobs: Mutex::new(Some(tx)),
            rng: Mutex::new(0x9E37_79B9_7F4A_7C15 ^ std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64)),
        });
        let weak: Weak<Inner> = Arc::downgrade(&inner);
        let _ = std::thread::Builder::new().name("fh1-sfx-load".into()).spawn(move || {
            // Ends when the bank (and with it the Sender) is dropped.
            while let Ok((bank, index)) = rx.recv() {
                let Some(inner) = weak.upgrade() else { break };
                let slot = match load_sample(&inner.dir, &bank, index) {
                    Some(p) => Slot::Ready(p),
                    None => Slot::Failed,
                };
                // Statement, not the block's tail: the guard must drop before `inner` (edition-2021 tail temporaries).
                if let Ok(mut m) = inner.samples.lock() {
                    m.insert((bank, index), slot);
                };
            }
        });
        Self { inner, mixer }
    }

    /// `<assets>/audio/fev/<project>.fev`, parsed once (a missing / bad file is remembered as None and warned once).
    pub fn fev(&self, project: &str) -> Option<Arc<Fev>> {
        let mut m = self.inner.fevs.lock().ok()?;
        if let Some(f) = m.get(project) {
            return f.clone();
        }
        let path = self.inner.dir.join("fev").join(format!("{project}.fev"));
        let f = match Fev::load(&path) {
            Ok(f) => Some(Arc::new(f)),
            Err(e) => {
                warn!("sfx: {}: {e:#}", path.display());
                None
            }
        };
        m.insert(project.to_owned(), f.clone());
        f
    }

    /// The decoded sample if loaded; otherwise queues the load on the worker thread and returns None.
    pub fn sample(&self, bank: &str, index: u32) -> Option<Arc<Pcm>> {
        let mut m = self.inner.samples.lock().ok()?;
        match m.get(&(bank.to_owned(), index)) {
            Some(Slot::Ready(p)) => return Some(p.clone()),
            Some(_) => return None,
            None => {}
        }
        m.insert((bank.to_owned(), index), Slot::Loading);
        drop(m);
        self.queue(bank, index);
        None
    }

    /// Queues a load without needing the result now.
    pub fn prefetch(&self, bank: &str, index: u32) {
        let _ = self.sample(bank, index);
    }

    fn queue(&self, bank: &str, index: u32) {
        if let Ok(j) = self.inner.jobs.lock() {
            if let Some(tx) = j.as_ref() {
                let _ = tx.send((bank.to_owned(), index));
            }
        }
    }

    pub fn mixer(&self) -> Option<&AmbientMixer> {
        self.mixer.as_ref()
    }

    /// Uniform 0..1 (xorshift64*; shared so callers need no RNG of their own).
    pub fn rand(&self) -> f32 {
        let Ok(mut s) = self.inner.rng.lock() else { return 0.5 };
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        ((s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / (1u64 << 24) as f32
    }

    pub fn rand_range(&self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.rand()
    }

    /// A wave of the event chosen by weight (uniform when no weight is positive).
    pub fn pick_wave<'a>(&self, ev: &'a fev::Event) -> Option<&'a fev::WaveRef> {
        let total: f32 = ev.waves.iter().map(|w| w.weight.max(0.0)).sum();
        if ev.waves.is_empty() {
            return None;
        }
        if total <= 0.0 {
            return ev.waves.get(((self.rand() * ev.waves.len() as f32) as usize).min(ev.waves.len() - 1));
        }
        let mut r = self.rand() * total;
        for w in &ev.waves {
            r -= w.weight.max(0.0);
            if r <= 0.0 {
                return Some(w);
            }
        }
        ev.waves.last()
    }

    /// True when this trigger lands on one of the event's silence entries (FEV wave kind 2: thins out random one-shots,
    /// e.g. Default_Whistle 14 silent vs 6 waves). Spawners call it before [`Self::play_event_ex`] and treat true as a
    /// played (inaudible) trigger.
    pub fn rolls_silence(&self, ev: &fev::Event) -> bool {
        if ev.silence_weight <= 0.0 {
            return false;
        }
        let total: f32 = ev.waves.iter().map(|w| w.weight.max(0.0)).sum::<f32>() + ev.silence_weight;
        self.rand() * total < ev.silence_weight
    }

    /// Plays the event: picks a wave, gain = event volume x random x distance law x `gain`, pitch = event pitch x random.
    /// `at` = world position of a 3D source (None = 2D / head-relative). None when the sample is not loaded yet (retry
    /// later), the source is inaudible, or there is no mixer.
    pub fn play_event(&self, ev: &fev::Event, at: Option<Vec3>, listener: &Listener, gain: f32, bus: Bus) -> Option<VoiceId> {
        self.play_event_ex(ev, at, listener, gain, bus).map(|p| p.id)
    }

    /// [`Self::play_event`] plus what a caller needs to keep updating the voice.
    pub fn play_event_ex(&self, ev: &fev::Event, at: Option<Vec3>, listener: &Listener, gain: f32, bus: Bus) -> Option<Played> {
        let mixer = self.mixer.as_ref()?;
        let wave = self.pick_wave(ev)?;
        let pcm = self.sample(&wave.bank, wave.index)?;
        let rand_db = if ev.volume_rand_db > 0.0 { -self.rand() * ev.volume_rand_db } else { 0.0 };
        let base = ev.volume * wave.gain * 10f32.powf(rand_db / 20.0) * gain;
        let pitch = ev.pitch.max(0.01) * wave.pitch.max(0.01) * if ev.pitch_rand > 0.0 { 2f32.powf(self.rand_range(-ev.pitch_rand, ev.pitch_rand)) } else { 1.0 };
        let (spatial_gain, pan) = match at {
            Some(p) if ev.is_3d => spatial_event(listener, p, ev),
            _ => (1.0, 0.0),
        };
        if spatial_gain * base < 1e-4 {
            return None;
        }
        let params = VoiceParams { looped: ev.looped, gain: base * spatial_gain, pan, pitch, bus, ..Default::default() };
        let id = mixer.play(pcm.clone(), params)?;
        let duration_s = pcm.frames() as f32 / pcm.rate.max(1) as f32 / pitch.max(0.01);
        Some(Played { id, pitch, base, duration_s })
    }
}

/// `<dir>/banks/<bank>/<index:03>.wav`; the quads FEV names `AMB_Quads` but the bank installs as `AMB_Quads_Stream`.
fn load_sample(dir: &std::path::Path, bank: &str, index: u32) -> Option<Arc<Pcm>> {
    for stem in [bank.to_owned(), format!("{bank}_Stream")] {
        let path = dir.join("banks").join(&stem).join(format!("{index:03}.wav"));
        if path.is_file() {
            return match Pcm::load_wav(&path) {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!("sfx: {}: {e:#}", path.display());
                    None
                }
            };
        }
    }
    None
}

fn start_bank(mut commands: Commands, garage: Option<Res<crate::Garage>>) {
    let dir = garage.map_or_else(|| PathBuf::from("assets/audio"), |g| g.assets.join("audio"));
    let off = std::env::var("FH1_SFX_BANK").is_ok_and(|v| v == "0");
    // pcm::shared() opens the output stream: once, here, off the render thread.
    let mixer = if off || crate::audio::audio_disabled() { None } else { fh1_audio::pcm::shared().map(|p| p.ambient()) };
    if mixer.is_none() {
        info!("sfx: no mixer (FH1_SFX_BANK=0, agent launch or no device); world SFX silent");
    }
    commands.insert_resource(SfxBank::new(dir, mixer));
}

fn update_listener(cam: Query<&GlobalTransform, With<fh1_render::post::FxPostCamera>>, mut listener: ResMut<Listener>) {
    if let Ok(t) = cam.single() {
        *listener = Listener { pos: t.translation(), right: t.right().as_vec3(), forward: t.forward().as_vec3() };
    }
}

/// M toggles the shared mute (same key as audio.rs); applies the volume settings and the gate to the two buses.
fn bus_gains(bank: Option<Res<SfxBank>>, settings: Res<Settings>, virt: Res<Time<Virtual>>, keys: Res<ButtonInput<KeyCode>>, mut last: Local<[f32; 2]>) {
    if keys.just_pressed(KeyCode::KeyM) {
        MUTED.fetch_xor(true, Ordering::Relaxed);
    }
    let Some(bank) = bank else { return };
    let Some(mixer) = bank.mixer() else { return };
    let open = world_sfx_allowed(virt.is_paused());
    let want = if open { [settings.ambient_volume, settings.engine_volume] } else { [0.0, 0.0] };
    if *last != want {
        mixer.set_bus_gain(Bus::Ambience, want[0]);
        mixer.set_bus_gain(Bus::Sfx, want[1]);
        *last = want;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolloff_is_monotone_and_zero_past_max() {
        for roll in [Roll::Inverse, Roll::Linear] {
            let mut prev = f32::MAX;
            for i in 0..=2200 {
                let d = i as f32 * 0.1;
                let g = rolloff_gain(&roll, d, 6.0, 200.0);
                assert!(g <= prev + 1e-6, "{roll:?} rose at {d}");
                assert!((0.0..=1.0).contains(&g));
                prev = g;
            }
            assert_eq!(rolloff_gain(&roll, 200.0, 6.0, 200.0), 0.0);
            assert_eq!(rolloff_gain(&roll, 500.0, 6.0, 200.0), 0.0);
            assert_eq!(rolloff_gain(&roll, 3.0, 6.0, 200.0), 1.0);
        }
    }

    #[test]
    fn curve_rolloff_interpolates() {
        let c = Roll::Curve(vec![(0.0, 1.0), (10.0, 0.5), (100.0, 0.0)].into());
        assert_eq!(rolloff_gain(&c, 0.0, 6.0, 200.0), 1.0);
        assert!((rolloff_gain(&c, 5.0, 6.0, 200.0) - 0.75).abs() < 1e-5);
        assert!((rolloff_gain(&c, 55.0, 6.0, 200.0) - 0.25).abs() < 1e-5);
        assert_eq!(rolloff_gain(&c, 500.0, 6.0, 200.0), 0.0);
    }

    #[test]
    fn pan_follows_right_axis() {
        let l = Listener { pos: Vec3::ZERO, right: Vec3::X, forward: Vec3::NEG_Z };
        assert!(spatial(&l, Vec3::new(10.0, 0.0, 0.0), 6.0, 100.0).1 > 0.99);
        assert!(spatial(&l, Vec3::new(-10.0, 0.0, 0.0), 6.0, 100.0).1 < -0.99);
        assert!(spatial(&l, Vec3::new(0.0, 0.0, -10.0), 6.0, 100.0).1.abs() < 1e-5);
    }
}
