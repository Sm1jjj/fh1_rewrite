//! Car audio: feeds the player car's state to `fh1-audio`'s synth, which plays on its own
//! output stream. Needs the setup tool's `audio` group; without it the game runs silent.
//! M toggles mute; the pause menu's engine volume scales it, and it falls silent while paused and while the main menu
//! or a loading card covers the world (`ui::loading::world_audio_allowed`, `FH1_MENU_AUDIO_GATE=0` = old behaviour).
//!
//! Other cars (race AI, traffic, and multiplayer puppets): the nearest `FH1_AI_AUDIO_VOICES` (default 4, at most
//! fh1_audio MAX_VOICES; 0 = off) within 200 m play their own engine banks from outside (engine / transmission / turbo /
//! shifts, no tyres or wind) at the listener = the main camera: gain = 6 m / distance (FMOD's inverse rolloff; the
//! game's own min distance is not decoded), faded out from 150 to 200 m, equal-power pan, and doppler on the pitch
//! (FMOD Ex default, scale 1; `FH1_AI_AUDIO_DOPPLER=0` off). Their sounds load on a worker thread (no hitch).

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use fh1_audio::output::{Player, MAX_VOICES};
use fh1_engine::ai::AiCar;
use fh1_audio::synth::{CarInput, CarSound, Library, TyreGroup, View, WheelInput};

use crate::camera::CameraRig;
use crate::track::Track;
use crate::ui::Settings;
use crate::{Car, Garage, Input};

pub struct CarAudioPlugin;

impl Plugin for CarAudioPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, start_audio).add_systems(Update, (feed_audio, feed_other_cars, forward_backfires).after(crate::net::puppets_posed));
    }
}

#[derive(Resource)]
struct CarAudioState {
    player: Player,
    lib: Arc<Library>,
    /// Voice slots for other cars.
    others: [OtherSlot; MAX_VOICES],
    loaded_tx: Sender<(usize, Entity, Option<CarSound>)>,
    loaded_rx: Mutex<Receiver<(usize, Entity, Option<CarSound>)>>,
    /// The player car's sound, loaded on a worker thread: (media name, sound).
    player_tx: Sender<(String, Option<CarSound>)>,
    player_rx: Mutex<Receiver<(String, Option<CarSound>)>>,
    /// Media name of the car being played.
    car: String,
    /// Tyre group by world surface id (filled lazily).
    groups: Vec<TyreGroup>,
    muted: bool,
}

/// TEMPORARY (user request 2026-10-04): instances launched by Claude agents (their shells set
/// `CLAUDECODE`) run silent, car audio and radio both. `FH1_AUDIO=on` overrides, `FH1_AUDIO=off`
/// silences any launch.
pub fn audio_disabled() -> bool {
    match std::env::var("FH1_AUDIO").map(|v| v.to_ascii_lowercase()) {
        Ok(v) if v == "on" => false,
        Ok(v) if v == "off" => true,
        _ => std::env::var_os("CLAUDECODE").is_some(),
    }
}

fn start_audio(mut commands: Commands, garage: Res<Garage>) {
    if audio_disabled() {
        return info!("audio: disabled (agent launch; FH1_AUDIO=on to enable)");
    }
    let dir = garage.assets.join("audio");
    if !dir.join("cars").is_dir() {
        info!("no audio installed ({}); run fh1setup to convert it", dir.display());
        return;
    }
    let lib = match Library::open(&dir) {
        Ok(l) => l,
        Err(e) => return warn!("audio: {e:#}"),
    };
    match Player::start() {
        Ok(player) => {
            info!(
                "audio: {} Hz on \"{}\" (set FH1_AUDIO_DEVICE=<part of name> to pick another of {:?})",
                player.sample_rate,
                player.device,
                fh1_audio::output::device_names()
            );
            let (loaded_tx, loaded_rx) = std::sync::mpsc::channel();
            let (player_tx, player_rx) = std::sync::mpsc::channel();
            commands.insert_resource(CarAudioState {
                player_tx,
                player_rx: Mutex::new(player_rx),
                player,
                lib: Arc::new(lib),
                others: Default::default(),
                loaded_tx,
                loaded_rx: Mutex::new(loaded_rx),
                car: String::new(),
                groups: Vec::new(),
                muted: false,
            });
        }
        Err(e) => warn!("audio: no output ({e:#})"),
    }
}

/// FH1_AUDIO_ASYNC_PLAYER=0: the player car's sound loads inside the frame (old).
fn player_sound_async() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| !std::env::var("FH1_AUDIO_ASYNC_PLAYER").is_ok_and(|v| v == "0"))
}

fn feed_audio(
    audio: Option<ResMut<CarAudioState>>,
    cars: Query<&Car>,
    input: Res<Input>,
    track: Res<Track>,
    rig: Res<CameraRig>,
    keys: Res<ButtonInput<KeyCode>>,
    settings: Res<Settings>,
    virt: Res<Time<Virtual>>,

) {
    let _watch = crate::perf::watch("feed_audio");
    let Some(mut audio) = audio else { return };
    if keys.just_pressed(KeyCode::KeyM) {
        audio.muted = !audio.muted;
    }
    let Ok(Car(v)) = cars.single() else {
        audio.player.set_sound(None);
        audio.car.clear();
        return;
    };
    if audio.car != v.data.sound {
        audio.car = v.data.sound.clone();
        if player_sound_async() {
            // 2026-10-08: the synchronous bank load (hundreds of WAV reads) held the main thread for 21 s on a map load
            // (stall report 20261008_132621, main in feed_audio). The old car keeps playing until the new one is ready.
            let (lib, tx, name) = (audio.lib.clone(), audio.player_tx.clone(), audio.car.clone());
            let _ = std::thread::Builder::new().name("fh1-audio-load".into()).spawn(move || {
                let sound = CarSound::new(&lib, &name).map_err(|e| warn!("audio: {name}: {e:#}")).ok();
                let _ = tx.send((name, sound));
            });
        } else {
            let sound = CarSound::new(&audio.lib, &audio.car);
            if let Err(e) = &sound {
                warn!("audio: {}: {e:#}", audio.car);
            }
            audio.player.set_sound(sound.ok());
        }
    }
    let ready: Vec<_> = audio.player_rx.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default();
    for (name, sound) in ready {
        // A load for a car switched away from in the meantime is dropped.
        if name == audio.car {
            audio.player.set_sound(sound);
        }
    }
    if audio.groups.is_empty() {
        if let Some(world) = &track.world {
            let groups = (0..=255u8).map(|id| audio.lib.tyre_group(world.surface_name(id))).collect();
            audio.groups = groups;
        }
    }

    let boost = v.data.boost.map_or(0.0, |b| ((v.boost - 1.0) / (b.max_scale - 1.0).max(1e-3)).clamp(0.0, 1.0));
    let mut wheels = [WheelInput::default(); 4];
    for (w, src) in wheels.iter_mut().zip(&v.wheels) {
        *w = WheelInput {
            slip_ratio: src.slip_ratio,
            slip_angle_deg: src.slip_angle_deg,
            load: if src.grounded { src.load } else { 0.0 },
            surface: audio.groups.get(src.surface as usize).copied().unwrap_or_default(),
            // Braking / wheelspin slip at half weight: ABS swings it every cycle, so it made braking screech harder than
            // cornering (user 2026-10-07).
            norm_slip: Some((0.5 * src.norm_slip).hypot(src.norm_slip_angle)),
        };
    }
    audio.player.set_input(CarInput {
        rpm: v.rpm,
        throttle: input.0.throttle,
        torque: Some(v.torque_fraction),
        // Vehicle: 0 = reverse, 1.. = forward.
        gear: if v.gear == 0 { -1 } else { v.gear as i32 },
        shifts: v.shift_count,
        speed: v.speed(),
        boost,
        has_turbo: v.data.boost.is_some(),
        wheels,
        rpm_limit: rpm_limit(&v.data),
        view: match rig.mode {
            2 | 3 => View::Cockpit,
            _ => View::Follow,
        },
        volume: if audio.muted || virt.is_paused() || !crate::ui::loading::world_audio_allowed() { 0.0 } else { settings.engine_volume },
        ..Default::default()
    });
}

/// The rev limiter the drivetrain uses (vehicle/drivetrain.rs `rev_limit_rpm`, same FH1_REVLIMIT switch): the synth
/// bounces off it (ignition cuts + pops) while the throttle is pinned there.
fn rpm_limit(d: &fh1_engine::data::CarData) -> f32 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    let mode = *MODE.get_or_init(|| std::env::var("FH1_REVLIMIT").ok().and_then(|s| s.parse().ok()).unwrap_or(1));
    if mode == 0 { d.rev_limit_rpm } else { 0.5 * (d.redline_rpm + d.torque_curve_max_rpm) }
}

/// Pops / bangs the synth played -> exhaust flames (backfire.rs), for the player's car and the other cars' voices.
fn forward_backfires(audio: Option<Res<CarAudioState>>, player: Query<Entity, With<Car>>, mut out: MessageWriter<crate::backfire::BackfireFired>) {
    let _watch = crate::perf::watch("forward_backfires");
    let Some(audio) = audio else { return };
    for (slot, b) in audio.player.take_backfires() {
        let car = match slot {
            None => player.single().ok(),
            Some(k) => audio.others.get(k).and_then(|s| s.car),
        };
        if let Some(car) = car {
            out.write(crate::backfire::BackfireFired { car, strength: b.strength, bang: b.bang });
        }
    }
}

/// One voice slot for another car.
#[derive(Default, Clone, Copy)]
struct OtherSlot {
    car: Option<Entity>,
    /// The sound arrived from the loader.
    ready: bool,
    /// Frames left fading out before the slot is freed.
    release: u8,
}

fn other_voices() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_AI_AUDIO_VOICES").ok().and_then(|v| v.parse().ok()).unwrap_or(4).min(MAX_VOICES))
}

fn doppler_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_AI_AUDIO_DOPPLER").map_or(true, |v| v != "0"))
}

/// Engine sound of the nearest AI / traffic cars (module doc).
#[allow(clippy::too_many_arguments)]
fn feed_other_cars(
    audio: Option<ResMut<CarAudioState>>,
    cars: Query<(Entity, &AiCar), Without<fh1_engine::traffic::TrafficParked>>,
    remotes: Query<(Entity, &crate::net::RemoteCar)>,
    cam: Query<&GlobalTransform, With<fh1_render::post::FxPostCamera>>,
    settings: Res<Settings>,
    virt: Res<Time<Virtual>>,
    time: Res<Time<Real>>,

    mut last_cam: Local<Option<Vec3>>,
) {
    let _watch = crate::perf::watch("feed_other_cars");
    const NEAR: f32 = 6.0;
    const FADE: (f32, f32) = (150.0, 200.0);
    const SOUND_SPEED: f32 = 343.0;
    let Some(mut audio) = audio else { return };
    let audio = &mut *audio;
    let n = other_voices();
    let Ok(cam) = cam.single() else { return };
    let ear = cam.translation();
    let right = cam.right().as_vec3();
    let dt = time.delta_secs().max(1e-3);
    let ear_vel = last_cam.map_or(Vec3::ZERO, |p| (ear - p) / dt);
    *last_cam = Some(ear);
    // Sounds loaded since last frame (dropped if the slot moved on to another car meanwhile).
    let loaded: Vec<_> = audio.loaded_rx.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default();
    for (slot, car, sound) in loaded {
        let s = &mut audio.others[slot];
        if s.car == Some(car) && s.release == 0 {
            s.ready = sound.is_some();
            audio.player.set_other_sound(slot, sound);
        }
    }
    // The nearest n cars within the fade-out distance.
    let mut near: Vec<(f32, Entity)> = Vec::new();
    if n != 0 {
        near.extend(cars.iter().map(|(e, c)| (c.0.position.distance(ear), e)).filter(|(d, _)| *d < FADE.1));
        near.extend(remotes.iter().map(|(e, r)| (r.vehicle.position.distance(ear), e)).filter(|(d, _)| *d < FADE.1));
    }
    near.sort_by(|a, b| a.0.total_cmp(&b.0));
    near.truncate(n);
    let wanted = |e: Entity| near.iter().any(|(_, x)| *x == e);
    // Release slots whose car dropped out (fade over a few frames, then free).
    for (k, s) in audio.others.iter_mut().enumerate() {
        let Some(car) = s.car else { continue };
        let alive = cars.get(car).is_ok() || remotes.get(car).is_ok();
        if s.release == 0 && (!alive || !wanted(car)) {
            s.release = 3;
            audio.player.set_other_input(k, CarInput::default(), 0.0, 0.0);
        } else if s.release > 0 {
            s.release -= 1;
            if s.release == 0 {
                audio.player.set_other_sound(k, None);
                *s = OtherSlot::default();
            }
        }
    }
    // Assign new cars to free slots; their sound loads on a worker thread.
    for &(_, car) in &near {
        if audio.others.iter().any(|s| s.car == Some(car)) {
            continue;
        }
        let Some(k) = audio.others.iter().position(|s| s.car.is_none()) else { break };
        let name = if let Ok((_, c)) = cars.get(car) {
            c.0.data.sound.clone()
        } else if let Ok((_, r)) = remotes.get(car) {
            r.vehicle.data.sound.clone()
        } else {
            continue;
        };
        audio.others[k] = OtherSlot { car: Some(car), ready: false, release: 0 };
        let (lib, tx) = (audio.lib.clone(), audio.loaded_tx.clone());
        let _ = std::thread::Builder::new().name("fh1-audio-load".into()).spawn(move || {
            let sound = CarSound::new(&lib, &name).map_err(|e| warn!("audio: {name}: {e:#}")).ok();
            let _ = tx.send((k, car, sound));
        });
    }
    let master = if audio.muted || virt.is_paused() || !crate::ui::loading::world_audio_allowed() { 0.0 } else { settings.engine_volume };
    for (k, s) in audio.others.iter().enumerate() {
        let (Some(car), true, 0) = (s.car, s.ready, s.release) else { continue };
        let ai = cars.get(car).ok();
        let remote = remotes.get(car).ok();
        let v = if let Some((_, c)) = ai.as_ref() {
            &c.0
        } else if let Some((_, r)) = remote.as_ref() {
            &r.vehicle
        } else {
            continue;
        };
        let to = v.position - ear;
        let d = to.length().max(0.1);
        let u = to / d;
        let fade = 1.0 - ((d - FADE.0) / (FADE.1 - FADE.0)).clamp(0.0, 1.0);
        let gain = (NEAR / d).min(1.0) * fade;
        let pan = u.dot(right);
        // f' = f (c + v_listener . u) / (c + v_source . u), u = listener -> source.
        let pitch = if doppler_on() { ((SOUND_SPEED + ear_vel.dot(u)) / (SOUND_SPEED + v.velocity.dot(u)).max(50.0)).clamp(0.5, 2.0) } else { 1.0 };
        // Throttle from the engine's torque against full torque at this rpm (traffic in far mode writes rpm / torque too).
        let full = (v.data.torque_at(v.rpm) / v.data.torque_scale.max(1.0)).max(0.05);
        let boost = v.data.boost.map_or(0.0, |b| ((v.boost - 1.0) / (b.max_scale - 1.0).max(1e-3)).clamp(0.0, 1.0));
        let input = CarInput {
            rpm: v.rpm,
            throttle: (v.torque_fraction / full).clamp(0.0, 1.0),
            torque: Some(v.torque_fraction),
            gear: if v.gear == 0 { -1 } else { v.gear as i32 },
            shifts: v.shift_count,
            speed: v.speed(),
            boost,
            has_turbo: v.data.boost.is_some(),
            view: View::Follow,
            rpm_limit: rpm_limit(&v.data),
            volume: master,
            pitch,
            external: true,
            ..Default::default()
        };
        audio.player.set_other_input(k, input, gain, pan);
    }
}
