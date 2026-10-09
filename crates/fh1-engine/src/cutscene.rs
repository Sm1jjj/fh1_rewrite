//! Cutscene player: FH1's decoded cutscene camera paths and timed events, played through the main camera.
//!
//! Data: `<assets>/story/cutscenes/<name>.json` (written by the `story` setup group; serde mirror in `cutscene/path.rs`).
//! API: [`Cutscenes`] (`play` / `stop` / `playing` / `time` / `take_ended`), [`active`], [`swallowing`], [`hide_hud`],
//! the [`CutsceneCue`] message, [`CutscenePlugin`]. Style precedent: `ui/fmv.rs` and `ui/intro.rs`.
//!
//! # How it takes the camera
//! `tick` runs in `Update` AFTER `camera::follow_camera` and overwrites the main camera's `Transform` and
//! `PerspectiveProjection::fov` for as long as a cutscene plays. `follow_camera` keeps running underneath (its chase
//! state follows the car), so the normal camera resumes the next frame with no jump. The UI overlay camera is skipped by
//! querying `FxPostCamera` (only the main camera carries it).
//!
//! # Conventions, FITTED over `data/extracted/story/camera_tracks.json` (5,660 CarSpace keys more than 3 m from the origin)
//! The file is left-handed; the JSON `pos` is already engine space (Z negated). Angles are RAW degrees.
//! - **Yaw**: engine forward (x, z) = (sin yaw, -cos yaw), i.e. `Quat::from_rotation_y(-yaw)` applied to -Z. Of the 8
//!   sin/cos sign variants this is the clear winner against "the camera looks at the frame origin": 57.2 % of keys within
//!   10 degrees (82 % within 20), median error 9.4 degrees; the next best is 17.7 % within 10 (`-sin, -cos`), the rest
//!   under 3 %. (Not all cams look at the origin, so the ceiling is below 100 %; the Opening's node-targeted cams 0..2 do
//!   not look at their node at all.)
//! - **Pitch**: engine elevation = `-pitch` (file positive pitch looks DOWN). Over the 3,238 keys whose yaw fits, `-pitch`
//!   matches the elevation to the origin (height 0) with median error 2.3 degrees, 85 % within 5 degrees; `+pitch`: 4.5
//!   degrees / 55 %.
//! - **Roll**: `Quat::from_rotation_z(roll)` (positive raises the camera's right side). INFERRED from the D3DX-style left
//!   handed convention (yaw/pitch fit it); not testable on the data. |roll| < 30 degrees everywhere.
//! - **Car-space frame** (pos relative to the car): axes are the engine car frame (X right, Y up, front = -Z): 77.6 % of
//!   the CarSpace keys sit at engine z < 0 (in front of the car), 3,238 keys hit the look-at test with no X mirroring. The
//!   origin used is the car MODEL origin (`position - rotation * cg_model`), with the heading only (flat) rotation.
//! - **fov**: vertical degrees, INFERRED (range 4.3..85, median 35, Opening 49.2; the game's follow-cam FOV key is 48.5
//!   and `camera.rs` writes it straight into the vertical `PerspectiveProjection::fov`).
//! - **Time**: key `t` is LOCAL to its cam (starts at 0); a cam plays from `start_cut` for `duration`. Cams are sequential:
//!   Opening_Cutscene has 11 cams, start_cut 0, 2.504, 5.998, 8, 11, 16.5, 18.75, 21, 22.08, 31, 36 with durations ending
//!   at exactly 39.0 = `duration_s`. Events use cutscene time (Opening: audio at 6.5, release car at 31.0, snapshot 38.0).
//! - **What "CarSpace" is relative to**: the cam's `target_name` when set, else the car. In Opening_Cutscene cams 0..2 target
//!   `CameraTargetNode_0/1` (world nodes from `CCutsceneCameraNodeTrigger`, at engine (-111, 88, -1938) and
//!   (-145, 85.5, -1942), the bird flock and deer sit at those nodes) and cams 3..10 (no target) are relative to the car the
//!   cutscene places with `PlacePlayerCar_0` at (-146.7, 85.2, -1962.5), heading 114.2. These are NOT at the story start
//!   placement (-1361.97, 47.32, -1881.99) / 94.07, so the opening is an aerial intro that places the car itself. Node and
//!   placement rotations are headings in the same convention as yaw (`rotation_y(-deg)`, INFERRED).
//! - PartSpace is treated as CarSpace (the car-part offset is unknown; INFERRED). Follow / Wheel cams are played as
//!   Animateable. `mirror_rhd`, `ease_in/out`, `time_anim`, post keys, `disable_car_rendering` are ignored (TODO).
//!
//! # Anchors
//! [`Anchor::PlayerCar`]: every CarSpace / PartSpace key is relative to the player car's render pose (the dev hook).
//! [`Anchor::World`]: relative to a fixed world frame: a named node when the cam targets one, else the cutscene's own
//! `PlacePlayerCar` transform when it has one, else the transform captured from the player car when playback starts (or
//! set by [`Cutscenes::place`]). WorldSpace cams never use a frame. The car is never moved: the caller owns placement.
//!
//! # Fades
//! A full-screen UI node at `GlobalZIndex(990)` (below the loading cover 1001 and the FMV layer 1002) driven by the cams'
//! fade-in (cover for `hold` s, then fades out over `duration`) and fade-out (rises over the last `duration` s), INFERRED.
//!
//! # Our own car shots (`FH1_CUTSCENE_OWN_CAR_CAMS=0` = the decoded car keys)
//! The decoded CarSpace / PartSpace keys clipped through the car and framed it wrong on our cars (user test), so every car
//! cam (not WorldSpace, and not targeting a resolvable `CameraTargetNode_*` world node) plays our own shots instead
//! (`cutscene/shots.rs`: slow orbit, low front three-quarter dolly, side tracking, rising crane, rear chase pull-back; picked
//! per cutscene, cut every 4.5..8 s on long cams), for the cam's own duration, framed from the car's `CarData::bbox`, looking at
//! the car's centre slightly raised. Guarantees, applied last every frame: the eye stays outside the car box + 0.6 m in the
//! car's true pose, and 0.35 m above the ground (`Track::ground` ray); world collision between the car and the eye pulls the
//! eye in (as the chase camera does), easing back out over ~0.3 s. Fades, skip, cues and the world / node cams are unchanged.
//!
//! # Flags
//! `FH1_CUTSCENE=<name>` plays that cutscene once the world is loaded (dev). `FH1_CUTSCENES=0` makes `play` refuse.

mod path;
mod shots;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use bevy::input::gamepad::Gamepad;
use bevy::prelude::*;

use crate::Car;
use fh1_engine::vehicle::Ground;
use path::{eval, Cam, Cutscene, Sample, Trigger};

/// A skip press is swallowed this long (s), and a fresh cutscene ignores presses this long (the press that started it).
const SWALLOW_S: f32 = 0.3;
/// Fade overlay layer: below the loading overlay (1001) and the FMV layer (1002).
const FADE_Z: i32 = 990;
/// Longest frame step the clock takes (a long hitch must not skip a whole cam).
const MAX_DT: f32 = 0.1;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static HIDE_HUD: AtomicBool = AtomicBool::new(false);
static SWALLOW: AtomicBool = AtomicBool::new(false);

/// A cutscene owns the camera (driving input / HUD hooks read it).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// A skip press was just consumed: menus / driving ignore input for a moment.
pub fn swallowing() -> bool {
    SWALLOW.load(Ordering::Relaxed)
}

/// The playing cutscene asked for the HUD to be hidden (`CutsceneOpts::hide_hud`).
pub fn hide_hud() -> bool {
    ACTIVE.load(Ordering::Relaxed) && HIDE_HUD.load(Ordering::Relaxed)
}

/// `FH1_CUTSCENE_OWN_CAR_CAMS=0` = car cams play the decoded keys (old).
fn own_car_cams() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_CUTSCENE_OWN_CAR_CAMS").as_deref(), Ok("0") | Ok("off")))
}

/// Eye clearance above the ground for our car shots (m).
const SHOT_GROUND_CLEARANCE: f32 = 0.35;
/// Gap kept in front of world collision between the car and the eye (m).
const SHOT_WALL_GAP: f32 = 0.3;

/// `FH1_CUTSCENES=0` = the player refuses every play.
fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_CUTSCENES").as_deref(), Ok("0") | Ok("off")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// CarSpace keys are relative to the player car (render pose).
    PlayerCar,
    /// CarSpace keys are relative to a fixed world transform (see the module docs).
    World,
}

#[derive(Clone, Debug)]
pub struct CutsceneOpts {
    pub skippable: bool,
    pub anchor: Anchor,
    pub hide_hud: bool,
}

impl Default for CutsceneOpts {
    fn default() -> Self {
        Self { skippable: true, anchor: Anchor::PlayerCar, hide_hud: true }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CutsceneEnd {
    Finished,
    Skipped,
    Missing,
}

/// A timed audio / music / voice-over trigger fired during a cutscene. Nobody consumes it yet.
#[derive(Message, Clone, Debug)]
pub struct CutsceneCue {
    /// The trigger class (`CCutsceneAudioTrigger`, `CCutsceneMusicTrigger`, ...).
    pub class: String,
    /// The cue name (`CS_OpeningSequence`).
    pub event: String,
    /// The audio group (`UIInGame/UI/Cutscenes`).
    pub group: String,
}

/// A world frame: CarSpace keys are `origin + rot * pos`.
#[derive(Clone, Copy, Debug)]
struct Frame {
    origin: Vec3,
    rot: Quat,
}

impl Frame {
    const IDENTITY: Frame = Frame { origin: Vec3::ZERO, rot: Quat::IDENTITY };
}

struct Playing {
    cs: Cutscene,
    opts: CutsceneOpts,
    t: f32,
    next_event: usize,
    /// Real time before which skip presses are ignored.
    skip_after: f32,
    /// `Anchor::World` fallback frame (captured at the first tick).
    world_frame: Option<Frame>,
    /// `CameraTargetNode_*` frames by name.
    nodes: Vec<(String, Frame)>,
    /// The cutscene's own `PlacePlayerCar` transform.
    placement: Option<Frame>,
    logged_cams: u32,
    /// Per cam: Some(first global shot segment) when it is a car cam that plays our own shots (`cutscene/shots.rs`).
    car_shot: Vec<Option<u32>>,
    /// Eye correction (occlusion / ground) carried between frames so it eases out instead of popping.
    shot_adjust: Vec3,
    /// Last wanted eye (a jump = a cut: the correction restarts).
    shot_last: Option<Vec3>,
}

#[derive(Resource, Default)]
pub struct Cutscenes {
    assets: PathBuf,
    cur: Option<Playing>,
    ended: Option<(String, CutsceneEnd)>,
    swallow_until: f32,
    now: f32,
    place: Option<Frame>,
}

impl Cutscenes {
    /// Load `<assets>/story/cutscenes/<name>.json` and start it. False (and a log line, and `take_ended` = Missing) when
    /// the file is absent or unreadable, or when `FH1_CUTSCENES=0`; callers then skip the cutscene.
    pub fn play(&mut self, name: &str, opts: CutsceneOpts) -> bool {
        if !enabled() {
            info!("cutscene: {name} refused (FH1_CUTSCENES=0)");
            self.ended = Some((name.to_string(), CutsceneEnd::Missing));
            return false;
        }
        if self.assets.as_os_str().is_empty() || name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
            warn!("cutscene: cannot play '{name}' (assets dir {:?})", self.assets);
            self.ended = Some((name.to_string(), CutsceneEnd::Missing));
            return false;
        }
        let file = self.assets.join("story").join("cutscenes").join(format!("{name}.json"));
        let parsed = std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|t| Cutscene::parse(&t).map_err(|e| e.to_string()));
        let mut cs = match parsed {
            Ok(c) => c,
            Err(e) => {
                warn!("cutscene: {name}: {} ({e}); skipped", file.display());
                self.ended = Some((name.to_string(), CutsceneEnd::Missing));
                return false;
            }
        };
        if cs.name.is_empty() {
            cs.name = name.to_string();
        }
        if self.cur.is_some() {
            self.stop();
        }
        let (mut nodes, mut placement) = (Vec::new(), None);
        for e in &cs.events {
            for tr in &e.triggers {
                if !tr.parent.is_empty() {
                    continue;
                }
                let Some(frame) = trigger_frame(tr) else { continue };
                if tr.class == "CCutsceneCameraNodeTrigger" && !nodes.iter().any(|(n, _): &(String, Frame)| *n == tr.name) {
                    nodes.push((tr.name.clone(), frame));
                } else if tr.class == "CCutscenePlaceCarTrigger" && tr.name.starts_with("PlacePlayerCar") && placement.is_none() {
                    placement = Some(frame);
                }
            }
        }
        info!(
            "cutscene: play {name} ({:.2} s, {} cams, {} events, {} nodes, {:?}{})",
            cs.length(),
            cs.cams.len(),
            cs.events.len(),
            nodes.len(),
            opts.anchor,
            if cs.looping() { ", looping" } else { "" }
        );
        // Car cams (not WorldSpace, not on a resolvable world node) get our own shots; segments are numbered over them in order.
        let mut k = 0u32;
        let car_shot: Vec<Option<u32>> = cs
            .cams
            .iter()
            .map(|c| {
                let on_node = !c.target_name.is_empty() && nodes.iter().any(|(n, _)| *n == c.target_name);
                if !own_car_cams() || c.world_space() || on_node {
                    return None;
                }
                let first = k;
                k += shots::segments((c.end() - c.start_cut).max(0.0)).0;
                Some(first)
            })
            .collect();
        if k > 0 {
            info!("cutscene: {name}: {} of {} cams use our own car shots ({k} shots)", car_shot.iter().flatten().count(), cs.cams.len());
        }
        HIDE_HUD.store(opts.hide_hud, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
        self.ended = None;
        self.cur = Some(Playing {
            cs,
            opts,
            t: 0.0,
            next_event: 0,
            skip_after: self.now + SWALLOW_S,
            world_frame: self.place,
            nodes,
            placement,
            logged_cams: 0,
            car_shot,
            shot_adjust: Vec3::ZERO,
            shot_last: None,
        });
        true
    }

    /// End the current cutscene now (reported by `take_ended` as `Finished`). The normal camera is back next frame.
    pub fn stop(&mut self) {
        if let Some(p) = self.cur.take() {
            self.ended = Some((p.cs.name, CutsceneEnd::Finished));
        }
        ACTIVE.store(false, Ordering::Relaxed);
        HIDE_HUD.store(false, Ordering::Relaxed);
    }

    pub fn playing(&self) -> Option<&str> {
        self.cur.as_ref().map(|p| p.cs.name.as_str())
    }

    /// Seconds into the current cutscene (0 when none plays).
    pub fn time(&self) -> f32 {
        self.cur.as_ref().map_or(0.0, |p| p.t)
    }

    pub fn take_ended(&mut self) -> Option<(String, CutsceneEnd)> {
        self.ended.take()
    }

    /// Optional extra: the world frame that `Anchor::World` falls back to (instead of the car's pose when playback starts).
    /// Engine position and heading in degrees (same convention as a cam yaw). Applies to cutscenes started afterwards.
    pub fn place(&mut self, pos: Vec3, heading_deg: f32) {
        self.place = Some(Frame { origin: pos, rot: Quat::from_rotation_y(-heading_deg.to_radians()) });
    }
}

/// World frame of a node / placement trigger: engine position + heading (`rotation` attribute, degrees).
fn trigger_frame(tr: &Trigger) -> Option<Frame> {
    let e = tr.engine?;
    let heading = tr.attr_f32("rotation").unwrap_or(0.0);
    Some(Frame { origin: Vec3::from(e), rot: Quat::from_rotation_y(-heading.to_radians()) })
}

/// The car's frame: model origin (the body model is a child at -cg), heading only.
fn car_frame(car: &Car, alpha: f32) -> Frame {
    let v = &car.0;
    let (position, rotation) = v.render_pose(alpha);
    let fwd = rotation * Vec3::NEG_Z;
    let yaw = (-fwd.x).atan2(-fwd.z);
    Frame { origin: position - rotation * v.cg_model, rot: Quat::from_rotation_y(yaw) }
}

/// Camera orientation from the RAW file angles (see the module docs for the fit).
fn look(s: &Sample) -> Quat {
    Quat::from_rotation_y(-s.yaw.to_radians()) * Quat::from_rotation_x(-s.pitch.to_radians()) * Quat::from_rotation_z(s.roll.to_radians())
}

/// Which frame a cam's keys are relative to; None = no frame known yet (hold the last picture).
fn frame_for(p: &Playing, cam: &Cam, car: Option<Frame>) -> Option<Frame> {
    if cam.world_space() {
        return Some(Frame::IDENTITY);
    }
    // A cam that targets a resolvable CameraTargetNode_* always uses that world node (both anchors).
    let node = (!cam.target_name.is_empty()).then(|| p.nodes.iter().find(|(n, _)| *n == cam.target_name).map(|(_, f)| *f)).flatten();
    match p.opts.anchor {
        Anchor::PlayerCar => node.or(car),
        Anchor::World => node.or(p.placement).or(p.world_frame),
    }
}

/// The cutscene's own `PlacePlayerCar*` (parentless) pose from `<assets>/story/cutscenes/<name>.json`:
/// (engine position, heading in degrees). Heading -> engine rotation is `Quat::from_rotation_y(-heading.to_radians())`
/// (same convention as a cam yaw; the car's front is -Z, so it faces (sin h, 0, -cos h)). INFERRED.
/// None when the file or the trigger is absent.
pub fn placement(name: &str, assets: &std::path::Path) -> Option<(Vec3, f32)> {
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return None;
    }
    let text = std::fs::read_to_string(assets.join("story").join("cutscenes").join(format!("{name}.json"))).ok()?;
    let cs = Cutscene::parse(&text).ok()?;
    cs.events
        .iter()
        .flat_map(|e| e.triggers.iter())
        .find(|t| t.class == "CCutscenePlaceCarTrigger" && t.name.starts_with("PlacePlayerCar") && t.parent.is_empty() && t.engine.is_some())
        .map(|t| (Vec3::from(t.engine.unwrap_or_default()), t.attr_f32("rotation").unwrap_or(0.0)))
}

/// Where our car shot's eye goes in the world: the wanted eye pulled in front of world collision between the car and it,
/// kept above the ground, the correction eased out over ~0.3 s; then the hard guarantees (outside the car box + margin in
/// the car's TRUE pose `model`, above the ground) on the final point.
fn place_shot_eye(want: Vec3, look: Vec3, model: (Vec3, Quat), b: &shots::CarBox, ground: Option<&dyn Ground>, p: &mut Playing, dt: f32) -> Vec3 {
    if p.shot_last.is_some_and(|l| l.distance(want) > 1.5) {
        p.shot_adjust = Vec3::ZERO;
    }
    p.shot_last = Some(want);
    let ground_y = |g: &dyn Ground, e: Vec3| g.ray(Vec3::new(e.x, e.y.max(look.y) + 2.0, e.z), Vec3::NEG_Y, 60.0).map(|h| h.point.y);
    let mut eye = want;
    if let Some(g) = ground {
        let d = eye - look;
        let dist = d.length();
        if dist > 1e-3 {
            if let Some(h) = g.ray(look, d / dist, dist + SHOT_WALL_GAP) {
                eye = look + d / dist * (h.distance - SHOT_WALL_GAP).max(0.0);
            }
        }
        if let Some(y) = ground_y(g, eye) {
            eye.y = eye.y.max(y + SHOT_GROUND_CLEARANCE);
        }
    }
    let need = eye - want;
    p.shot_adjust = if need.length() >= p.shot_adjust.length() { need } else { p.shot_adjust.lerp(need, 1.0 - (-dt / 0.3).exp()) };
    let mut e = want + p.shot_adjust;
    // Hard guarantees last.
    let (mo, mr) = model;
    let local = mr.inverse() * (e - mo);
    let local = b.push_out(Vec3::new(local.x, local.y.max(b.min.y + 0.3), local.z), shots::MARGIN);
    e = mo + mr * local;
    if let Some(y) = ground.and_then(|g| ground_y(g, e)) {
        e.y = e.y.max(y + SHOT_GROUND_CLEARANCE);
    }
    e
}

/// Audio / music / voice-over triggers become [`CutsceneCue`]s.
fn is_cue(class: &str) -> bool {
    class.contains("Audio") || class.contains("Music") || class.contains("VoiceOver")
}

/// Fire every not-yet-fired event with `time <= upto`.
fn dispatch(p: &mut Playing, upto: f32, cues: &mut MessageWriter<CutsceneCue>) {
    while let Some(e) = p.cs.events.get(p.next_event) {
        if e.time > upto {
            break;
        }
        for tr in &e.triggers {
            debug!("cutscene {}: t={:.2} {} '{}' parent='{}' at {:?}", p.cs.name, e.time, tr.class, tr.name, tr.parent, tr.engine);
            if is_cue(&tr.class) {
                cues.write(CutsceneCue { class: tr.class.clone(), event: tr.attr_str("event"), group: tr.attr_str("group") });
            }
            // TODO: HideAllCars / CCutsceneCarTrigger Hide (hide the AI / festival cars), CCutsceneAnimationTrigger objects,
            // CCutsceneCountdownTrigger, CCutscenePlaceCarTrigger / ReleaseCar (the caller owns placement): log only.
        }
        p.next_event += 1;
    }
}

#[derive(Component)]
struct FadeRoot;

fn spawn_fade(mut commands: Commands) {
    commands.spawn((
        FadeRoot,
        GlobalZIndex(FADE_Z),
        Visibility::Hidden,
        Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() },
        BackgroundColor(Color::NONE),
    ));
}

/// `FH1_CUTSCENE=<name>`: play it once the world is up and no cover is showing.
fn dev_play(mut cs: ResMut<Cutscenes>, ld: Option<Res<crate::ui::loading::Loading>>, cars: Query<&Car>, time: Res<Time<Real>>, mut state: Local<(bool, f32)>) {
    if state.0 {
        return;
    }
    let Some(name) = std::env::var("FH1_CUTSCENE").ok().filter(|n| !n.is_empty()) else {
        state.0 = true;
        return;
    };
    let ready = cars.single().is_ok() && ld.as_deref().is_some_and(|l| l.cover.is_none()) && !cs.assets.as_os_str().is_empty();
    state.1 = if ready { state.1 + time.delta_secs() } else { 0.0 };
    if state.1 < 1.0 {
        return;
    }
    state.0 = true;
    cs.play(&name, CutsceneOpts { skippable: true, anchor: Anchor::PlayerCar, hide_hud: true });
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn tick(
    mut cs: ResMut<Cutscenes>,
    time: Res<Time<Real>>,
    fixed: Res<Time<Fixed>>,
    cars: Query<&Car>,
    mut cams: Query<(&mut Transform, &mut Projection), With<fh1_render::post::FxPostCamera>>,
    (keys, mouse, pads): (Res<ButtonInput<KeyCode>>, Res<ButtonInput<MouseButton>>, Query<&Gamepad>),
    mut cues: MessageWriter<CutsceneCue>,
    mut fade: Query<(&mut BackgroundColor, &mut Visibility), With<FadeRoot>>,
    garage: Option<Res<crate::Garage>>,
    track: Option<Res<crate::track::Track>>,
) {
    let cs = &mut *cs;
    let now = time.elapsed_secs();
    cs.now = now;
    if cs.assets.as_os_str().is_empty() {
        if let Some(g) = garage {
            cs.assets = g.assets.clone();
        }
    }
    SWALLOW.store(now < cs.swallow_until, Ordering::Relaxed);

    let Some(p) = cs.cur.as_mut() else {
        if let Ok((_, mut vis)) = fade.single_mut() {
            if *vis != Visibility::Hidden {
                *vis = Visibility::Hidden;
            }
        }
        return;
    };

    let mut end = None;
    if p.opts.skippable && now >= p.skip_after && crate::ui::launch::pressed(&keys, &mouse, &pads) {
        info!("cutscene: skipped {} at {:.2} s", p.cs.name, p.t);
        cs.swallow_until = now + SWALLOW_S;
        SWALLOW.store(true, Ordering::Relaxed);
        end = Some(CutsceneEnd::Skipped);
    } else {
        p.t += time.delta_secs().min(MAX_DT);
        let len = p.cs.length();
        if p.cs.looping() {
            if len > 0.0 && p.t >= len {
                dispatch(p, len, &mut cues);
                let from = p.cs.loop_from_s.clamp(0.0, len);
                let span = len - from;
                p.t = if span > 1e-3 { from + (p.t - len).rem_euclid(span) } else { from };
                p.next_event = p.cs.events.partition_point(|e| e.time < from);
            }
        } else if p.t >= len {
            p.t = len;
            end = Some(CutsceneEnd::Finished);
        }
        let t = p.t;
        dispatch(p, t, &mut cues);
    }

    if end.is_none() {
        let alpha = fixed.overstep_fraction();
        let car = cars.single().ok().map(|c| car_frame(c, alpha));
        if p.world_frame.is_none() {
            p.world_frame = cs.place.or(car);
        }
        if let (Some(i), Ok((mut ct, mut proj))) = (p.cs.active_cam(p.t), cams.single_mut()) {
            let cam = &p.cs.cams[i];
            if p.logged_cams & (1 << i.min(31)) == 0 {
                p.logged_cams |= 1 << i.min(31);
                debug!(
                    "cutscene {}: t={:.2} cam {i} {} {}/{} target '{}' {} keys",
                    p.cs.name, p.t, cam.kind, cam.pos_space, cam.rot_space, cam.target_name, cam.keys.len()
                );
                if cam.disable_car_rendering {
                    debug!("cutscene {}: cam {i} wants the car hidden (TODO: not implemented)", p.cs.name);
                }
            }
            let shot_k0 = p.car_shot.get(i).copied().flatten();
            let car_ent = cars.single().ok();
            if let (Some(k0), Some(c), Some(f)) = (shot_k0, car_ent, car) {
                // Our own car shot (see the module docs): car frame = model origin + heading, look at the car's centre.
                let b = shots::CarBox::new(c.0.data.bbox[0], c.0.data.bbox[1]);
                let dur = (cam.end() - cam.start_cut).max(0.0);
                let sp = shots::cam_pose(&p.cs.name, k0, cam.local(p.t).max(0.0), dur, &b);
                let look_w = f.origin + f.rot * sp.look;
                let want = f.origin + f.rot * sp.eye;
                let (pos, rot) = c.0.render_pose(alpha);
                let model = (pos - rot * c.0.cg_model, rot);
                let ground = track.as_deref().map(|t| t.ground.as_ref() as &dyn Ground);
                let eye = place_shot_eye(want, look_w, model, &b, ground, p, time.delta_secs().min(MAX_DT));
                ct.translation = eye;
                ct.rotation = Transform::from_translation(eye).looking_at(look_w, Vec3::Y).rotation;
                if let Projection::Perspective(pp) = &mut *proj {
                    pp.fov = sp.fov.to_radians();
                }
            } else if let (Some(s), Some(frame)) = (eval(&cam.keys, cam.local(p.t)), frame_for(p, cam, car)) {
                ct.translation = frame.origin + frame.rot * Vec3::from(s.pos);
                ct.rotation = (frame.rot * look(&s)).normalize();
                if let Projection::Perspective(pp) = &mut *proj {
                    pp.fov = s.fov.clamp(1.0, 120.0).to_radians();
                }
            }
        }
    }

    // Fade overlay.
    let fade_now = if end.is_none() { p.cs.fade_at(p.t) } else { None };
    if let Ok((mut bg, mut vis)) = fade.single_mut() {
        match fade_now {
            Some((c, a)) => {
                bg.0 = Color::srgba(c[0], c[1], c[2], a);
                if *vis != Visibility::Visible {
                    *vis = Visibility::Visible;
                }
            }
            None => {
                if *vis != Visibility::Hidden {
                    *vis = Visibility::Hidden;
                }
            }
        }
    }

    if let Some(how) = end {
        if let Some(p) = cs.cur.take() {
            info!("cutscene: {} ended ({how:?}) at {:.2} s", p.cs.name, p.t);
            cs.ended = Some((p.cs.name, how));
        }
        ACTIVE.store(false, Ordering::Relaxed);
        HIDE_HUD.store(false, Ordering::Relaxed);
    }
}

pub struct CutscenePlugin;

impl Plugin for CutscenePlugin {
    fn build(&self, app: &mut App) {
        let mut cs = Cutscenes::default();
        // main.rs inserts the Garage before the plugins; `tick` also fills this in lazily.
        if let Some(g) = app.world().get_resource::<crate::Garage>() {
            cs.assets = g.assets.clone();
        }
        app.insert_resource(cs)
            .add_message::<CutsceneCue>()
            .add_systems(Startup, spawn_fade)
            .add_systems(Update, (dev_play, tick).chain().after(crate::camera::follow_camera));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(yaw: f32, pitch: f32, roll: f32) -> Sample {
        Sample { pos: [0.0; 3], yaw, pitch, roll, fov: 45.0 }
    }

    fn near(a: Vec3, b: Vec3) -> bool {
        a.distance(b) < 1e-4
    }

    #[test]
    fn angle_convention() {
        let fwd = |s: Sample| look(&s) * Vec3::NEG_Z;
        // yaw 0 looks along -Z (the car's front), yaw 90 along +X, yaw 180 along +Z (file forward flips with Z).
        assert!(near(fwd(sample(0.0, 0.0, 0.0)), Vec3::NEG_Z));
        assert!(near(fwd(sample(90.0, 0.0, 0.0)), Vec3::X));
        assert!(near(fwd(sample(180.0, 0.0, 0.0)), Vec3::Z));
        // File positive pitch looks down.
        assert!(fwd(sample(0.0, 20.0, 0.0)).y < -0.3);
        // Roll leaves the forward vector alone.
        assert!(near(fwd(sample(30.0, 10.0, 15.0)), fwd(sample(30.0, 10.0, 0.0))));
    }

    #[test]
    fn frames_and_defaults() {
        let o = CutsceneOpts::default();
        assert!(o.skippable && o.hide_hud && o.anchor == Anchor::PlayerCar);
        let mut c = Cutscenes::default();
        assert!(c.playing().is_none() && c.time() == 0.0 && c.take_ended().is_none());
        // No assets dir: play refuses without touching the disk.
        assert!(!c.play("Opening_Cutscene", CutsceneOpts::default()));
        assert_eq!(c.take_ended(), Some(("Opening_Cutscene".to_string(), CutsceneEnd::Missing)));
        // A node's heading rotates its local +X: heading 90 puts local -Z (front) on +X.
        let f = Frame { origin: Vec3::ZERO, rot: Quat::from_rotation_y(-90f32.to_radians()) };
        assert!(near(f.rot * Vec3::NEG_Z, Vec3::X));
    }

    #[test]
    fn cues_are_audio_music_and_voice() {
        assert!(is_cue("CCutsceneAudioTrigger") && is_cue("CCutsceneMusicTrigger") && is_cue("CCutsceneVoiceOverTrigger"));
        assert!(is_cue("CCutsceneAudioMixSnapshotTrigger") && is_cue("CCutsceneVoiceOverFestivalAnnouncerTrigger"));
        assert!(!is_cue("CCutsceneCameraNodeTrigger") && !is_cue("CCutsceneCarTrigger"));
    }
}
