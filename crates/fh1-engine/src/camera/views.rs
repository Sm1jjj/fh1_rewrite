//! Car-mounted views: bonnet (mode 2, the game's `Hood`), cockpit (mode 3, `DriverCam`) and bumper (mode 4,
//! `BumperHigh`), with the game's in-car look controller. Rules and addresses: docs/CAMERA.md "Car-mounted views".
//!
//! Frames: the game's car space is the gamedb one (origin = wheelbase bottom-centre, +Z front); our model space is
//! the same point with +Z back, so game z is negated on the way in. The body model is a child of the car at -cg
//! (main.rs spawn_car): world = position + rotation * (model - cg_model).

use std::f32::consts::{FRAC_PI_2, FRAC_PI_3, FRAC_PI_4, PI};
use std::path::Path;

use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::prelude::*;
use serde_json::Value;

use fh1_render::car::{FxCockpitEye, FxCockpitView};

use super::effects::{Effects, Inputs, Stack};
use super::{wrap, CameraData, CameraRig, CarPose};

pub(super) const HOOD: u8 = 2;
pub(super) const COCKPIT: u8 = 3;
pub(super) const BUMPER: u8 = 4;

/// The change-camera cycle in our mode numbers. The game's type ids are 0 FollowCam, 1 FollowCam2, 2 DriverCam,
/// 3 Hood, 4 BumperHigh (camera type table 0x8324BB08). PROVISIONAL: id order; the real cycle waits on the Pinyon probe.
const CYCLE: [u8; 5] = [0, 1, COCKPIT, HOOD, BUMPER];

/// Next view for the change-camera button.
pub fn next_mode(mode: u8) -> u8 {
    let i = CYCLE.iter().position(|&m| m == mode).unwrap_or(0);
    CYCLE[(i + 1) % CYCLE.len()]
}

/// CameraSettings.ini values, read from the `camera` setup group; these disc values are the fallback. FOVs are
/// taken as vertical, like the follow cameras (INFERRED).
#[derive(Clone, Copy, Debug)]
struct Ini {
    hood_fov: f32,
    bumper_fov: f32,
    cockpit_fov: f32,
    /// `BumperHighCam\RestHeightAboveGround`.
    bumper_rest_height: f32,
    /// `BumperHighCam\CamZOffset`: used when the car's CamBumperHighZOffset is 0 (every car).
    bumper_z: f32,
    /// `HoodCam\CamYOffset/CamZOffset/CamPitchOffset`: used when the car's three CamHood* columns are all 0 (no car).
    hood_default: (f32, f32, f32),
}

impl Ini {
    fn new(data: Option<&CameraData>) -> Ini {
        let get = |k: &str, d: f32| data.and_then(|c| c.settings.get(k)).unwrap_or(d);
        Ini {
            hood_fov: get(r"HoodCam\FOV", 66.0),
            bumper_fov: get(r"BumperHighCam\FOV", 53.0),
            cockpit_fov: get(r"Driver\FOV", 62.0),
            bumper_rest_height: get(r"BumperHighCam\RestHeightAboveGround", 1.15),
            bumper_z: get(r"BumperHighCam\CamZOffset", -0.05),
            hood_default: (get(r"HoodCam\CamYOffset", 0.0), get(r"HoodCam\CamZOffset", 0.0), get(r"HoodCam\CamPitchOffset", 0.0)),
        }
    }
}
/// Hood mount base added to the car point (0x828517C0: 0.45 at 0x82031F60, 0.6 at 0x82022858).
const HOOD_BASE: (f32, f32) = (0.45, 0.6);
/// Inset from the bounding-box face when the camera moves there (0x820740F8 = 2 ft).
const BBOX_INSET: f32 = 0.6096;

/// Look controller constants (0x82852590): spring rate (0x82000F2C, set by the base ctor 0x828616B0), step gain,
/// the front cone and the snap step.
const LOOK_K: f32 = 1.01;
const LOOK_GAIN: f32 = 10.0;
/// Right-stick deflection that counts as a look input (INFERRED: the game's look actions are digital-ish).
const LOOK_DEADZONE: f32 = 0.3;

/// The FxCarBody entity (main.rs CarModel): cockpit marker + eye point (fh1-render car.rs).
pub(super) type BodyQuery = (Entity, Has<FxCockpitView>, Option<&'static FxCockpitEye>);

/// Per-car mount data (physics.json: gamedb `CameraOverrides` + `Data_CarBody` bounds), in our model space.
#[derive(Clone, Debug)]
pub struct Mounts {
    media: String,
    /// Hood camera point and pitch offset (radians, INFERRED unit).
    hood: Vec3,
    hood_pitch: f32,
    bumper: Vec3,
    /// Rear-view mirror camera (CCamRearView) point.
    rear: Vec3,
    /// Camera box (MAXData collision BoundingBox) centre and half size (z already in our sign).
    bbox_centre: Vec3,
    bbox_half: Vec3,
}

impl Mounts {
    fn load(car_dir: &Path, media: &str, ini: &Ini) -> Option<Mounts> {
        let p: Value = serde_json::from_slice(&std::fs::read(car_dir.join("physics.json")).ok()?).ok()?;
        let cam = &p["camera"];
        let body = &p["body"];
        let c = |k: &str| cam[k].as_f64().unwrap_or(0.0) as f32;
        let b = |k: &str| body[k].as_f64().unwrap_or(0.0) as f32;
        // The box the cameras use (car vtable +220) = MAXData's collision BoundingBox (VERIFIED on Pinyon within 2-3 cm:
        // Viper front 2.058 / rear -2.264 / half-width ~0.98 vs MAXData 2.033 / -2.285 / 0.966). Pristine is the fallback.
        let mb = &p["maxdata"]["Collision"]["BoundingBox"];
        let m = |k: &str| mb[k].as_f64().map(|v| v as f32);
        let (min, max) = match (m("MinX"), m("MinY"), m("MinZ"), m("MaxX"), m("MaxY"), m("MaxZ")) {
            (Some(a), Some(b2), Some(c2), Some(d), Some(e), Some(f)) => (Vec3::new(a, b2, c2), Vec3::new(d, e, f)),
            _ => (
                Vec3::new(b("PristineBoundingBoxMinX"), b("PristineBoundingBoxMinY"), b("PristineBoundingBoxMinZ")),
                Vec3::new(b("PristineBoundingBoxMaxX"), b("PristineBoundingBoxMaxY"), b("PristineBoundingBoxMaxZ")),
            ),
        };
        // Game frame (+Z front).
        let centre = (min + max) * 0.5;
        let half = (max - min) * 0.5;
        let ground = -0.5 * (b("ModelFrontStockRideHeight") + b("ModelRearStockRideHeight"));

        // Hood (0x828517C0 + 0x8284C780): car point + (0, 0.45, 0.6) + per-car (0, CamHoodHeightOffset,
        // CamHoodZOffset), or the ini defaults when all three columns are 0. The car point (car vtable +1128) is
        // INFERRED to be the car-space origin, pending the Pinyon probe.
        let (hy, hz, hp) = (c("CamHoodHeightOffset"), c("CamHoodZOffset"), c("CamHoodPitchOffset"));
        let (hy, hz, hp) = if hy == 0.0 && hz == 0.0 && hp == 0.0 { ini.hood_default } else { (hy, hz, hp) };
        let hood = Vec3::new(0.0, HOOD_BASE.0 + hy, HOOD_BASE.1 + hz);

        // Bumper (0x82851698 + 0x8284C588): x 0, z = box front - 2 ft + CamBumperHighZOffset (ini -0.05 when 0),
        // y = ground + RestHeightAboveGround (car vtable +792 .y minus +548: INFERRED hub height minus tyre radius).
        let bz = match c("CamBumperHighZOffset") {
            0.0 => ini.bumper_z,
            z => z,
        };
        let bumper = Vec3::new(0.0, ground + ini.bumper_rest_height, centre.z + half.z - BBOX_INSET + bz);

        let flip = |v: Vec3| Vec3::new(v.x, v.y, -v.z);
        // Rear view (0x828519A8 + 0x8284C8D8): roof height, box front − 2 ft; looking back moves z to the rear face
        // + 2 ft (0x8285C8C8). Per-car (0, CamBumperLowHeightOffset, CamBumperLowZOffset): all 0 on the disc.
        let rear = Vec3::new(0.0, centre.y + half.y + c("CamBumperLowHeightOffset"), centre.z - half.z + BBOX_INSET + c("CamBumperLowZOffset"));

        Some(Mounts { media: media.to_owned(), hood: flip(hood), hood_pitch: -hp, bumper: flip(bumper), rear: flip(rear), bbox_centre: flip(centre), bbox_half: half })
    }

    /// 0x8285C8C8: while looking away (snapped to ±90°, ±135° or 180°) the camera moves to that face of the box,
    /// 2 ft inside it; the other axes keep the mount. `yaw` > 0 = right.
    fn relocate(&self, mount: Vec3, yaw: f32) -> Vec3 {
        let (c, h) = (self.bbox_centre, self.bbox_half);
        let near = |a: f32| (yaw - a).abs() <= 0.0002;
        let right = c.x + h.x - BBOX_INSET;
        let left = c.x - h.x + BBOX_INSET;
        // Our +Z is the back: the rear face is c.z + h.z.
        let rear = c.z + h.z - BBOX_INSET;
        let mut p = mount;
        if near(PI) || near(-PI) {
            p.z = rear;
        } else if near(FRAC_PI_2) {
            p.x = right;
        } else if near(-FRAC_PI_2) {
            p.x = left;
        } else if near(3.0 * FRAC_PI_4) {
            p.x = right;
            p.z = rear;
        } else if near(-3.0 * FRAC_PI_4) {
            p.x = left;
            p.z = rear;
        }
        p
    }
}

/// The game's look controller (0x82854758 input, 0x82852590 update). Car-mounted cameras use the snapping variant
/// (byte +276 = 1 from the base ctor 0x828616B0); the follow cameras the smooth one (byte +296 = 0, ctor 0x8285FE40).
/// Kinect head tracking is not modelled.
#[derive(Default, Debug, Clone)]
pub(super) struct Look {
    pub yaw: f32,
    vel: f32,
    target: f32,
    back: bool,
    smooth_only: bool,
}

impl Look {
    /// The follow cameras' variant: no 45° snapping (the damped follow for every target).
    pub(super) fn smooth() -> Look {
        Look { smooth_only: true, ..default() }
    }

    /// Reads the look inputs: right stick, R3 / K = look straight back, J / L = look left / right (keyboard extra),
    /// FH1_LOOK=<degrees> (automation).
    pub(super) fn read(&mut self, pads: &Query<&Gamepad>, keys: &ButtonInput<KeyCode>) {
        let mut stick = Vec2::ZERO;
        let mut back = keys.pressed(KeyCode::KeyK);
        if keys.pressed(KeyCode::KeyJ) {
            stick.x -= 1.0;
        }
        if keys.pressed(KeyCode::KeyL) {
            stick.x += 1.0;
        }
        for pad in pads {
            let s = Vec2::new(pad.get(GamepadAxis::RightStickX).unwrap_or(0.0), pad.get(GamepadAxis::RightStickY).unwrap_or(0.0));
            if s.length() > stick.length() {
                stick = s;
            }
            back |= pad.pressed(GamepadButton::RightThumb);
        }
        // Automation: FH1_LOOK=<degrees> holds the stick in that direction (0 ahead, 90 right, 180 back).
        if let Some(deg) = std::env::var("FH1_LOOK").ok().and_then(|v| v.parse::<f32>().ok()) {
            let a = deg.to_radians();
            stick = Vec2::new(a.sin(), a.cos());
        }
        self.input(stick, back);
    }

    /// `stick`: right stick (x right, y up). Look actions 9-12 (LookForward/Back/Left/Right) are its four halves;
    /// the target is their direction, magnitude ignored. `back`: LookStraightBack (action 13) held.
    fn input(&mut self, stick: Vec2, back: bool) {
        self.back = back;
        self.target = if stick.length() > LOOK_DEADZONE { stick.x.atan2(stick.y) } else { 0.0 };
    }

    /// Returns true while looking away from the front cone (snapping variant: the camera then moves to the box face).
    /// `yaw` > 0 = right.
    pub(super) fn step(&mut self, dt: f32) -> bool {
        if self.back {
            self.yaw = PI;
            return true;
        }
        let cone = |a: f32| (-FRAC_PI_4..=FRAC_PI_4).contains(&a);
        if self.smooth_only {
            self.follow(dt);
            return false;
        }
        if !cone(self.target) {
            // Outside the front cone: snap to the nearest 45°.
            self.yaw = wrap((self.target / FRAC_PI_4 + 0.5).floor() * FRAC_PI_4);
            self.vel = 0.0;
            return !cone(self.yaw);
        }
        if !cone(self.yaw) {
            self.yaw = self.target;
            self.vel = 0.0;
            return false;
        }
        self.follow(dt);
        false
    }

    /// Damped follow towards the target; steps of π/3 or more jump (0x82852590 at 0x8285270C).
    fn follow(&mut self, dt: f32) {
        let diff = wrap(self.target - self.yaw);
        if diff.abs() < FRAC_PI_3 && dt != 0.0 {
            self.vel += (LOOK_K * diff - (dt + 1.0) * self.vel) / (LOOK_K * dt + 1.0);
            self.yaw += self.vel * dt * LOOK_GAIN;
        } else {
            self.yaw = self.target;
            self.vel = 0.0;
        }
        self.yaw = wrap(self.yaw);
    }
}

/// View state kept between frames (follow_camera's `Local`).
#[derive(Default)]
pub struct ViewState {
    look: Look,
    mounts: Option<Mounts>,
    /// Hood/bumper hand over to the chase camera while the car is upside down (0x828555A0): car up.y < 0.1 switches,
    /// up.y > 0.5 for 1 s switches back.
    flipped: bool,
    upright: f32,
    /// The active view's effect stack (CameraPhysics.xml `DriverCam` / `Hood` / `BumperHigh`) and its last output.
    fx: Option<(u8, Stack)>,
    effects: Effects,
    ini: Option<Ini>,
    /// Looking away from the front cone last frame: the game then does not draw the player's car (VERIFIED on
    /// Pinyon: hood look-back shows no car although the camera sits inside the body). `car_hidden` = applied state.
    away: bool,
    car_hidden: bool,
}

/// Camera effect application (0x82853DD0), default on; FH1_VIEW_FX=0 = rigid views.
pub(super) fn fx_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_VIEW_FX").map_or(true, |v| v != "0"))
}

fn mounted(rig: &CameraRig) -> bool {
    matches!(rig.mode, HOOD | COCKPIT | BUMPER) && !rig.photo
}

fn cockpit(rig: &CameraRig) -> bool {
    rig.mode == COCKPIT && !rig.photo
}

/// True while a car-mounted view owns the right stick (camera_input must not orbit with it then).
pub(super) fn owns_stick(rig: &CameraRig) -> bool {
    mounted(rig)
}

/// Keeps the FxCockpitView marker in step with the mode; returns the cockpit eye point (model space) if loaded.
/// Also hides the car body (and what hangs under it) while a car-mounted view looks away.
pub(super) fn sync_cockpit_view(mut commands: Commands, bodies: &Query<BodyQuery, With<fh1_render::car::FxCarBody>>, rig: &CameraRig, state: &mut ViewState) -> Option<Vec3> {
    let cockpit = cockpit(rig);
    let hide = mounted(rig) && state.away;
    let mut eye_local = None;
    for (e, viewing, eye) in bodies {
        if hide != state.car_hidden {
            commands.entity(e).insert(if hide { Visibility::Hidden } else { Visibility::Inherited });
        }
        if viewing != cockpit {
            if cockpit {
                commands.entity(e).insert(FxCockpitView);
            } else {
                commands.entity(e).remove::<FxCockpitView>();
            }
        }
        eye_local = eye_local.or(eye.map(|e| e.0));
    }
    state.car_hidden = hide;
    eye_local
}

/// Vertical FOV (radians) for the views owned here; None = not a car-mounted view (or handed to the chase camera).
pub(super) fn fov(rig: &CameraRig, state: &ViewState) -> Option<f32> {
    if !mounted(rig) || state.flipped {
        return None;
    }
    let ini = state.ini.unwrap_or_else(|| Ini::new(None));
    let base = match rig.mode {
        HOOD => ini.hood_fov,
        BUMPER => ini.bumper_fov,
        _ => ini.cockpit_fov,
    };
    Some(base.to_radians() + state.effects.fov)
}

/// Reads the look inputs: right stick, R3 / K = look straight back, J / L = look left / right (keyboard extra).
pub(super) fn read_look(state: &mut ViewState, pads: &Query<&Gamepad>, keys: &ButtonInput<KeyCode>) {
    state.look.read(pads, keys);
}

/// Places the camera for a car-mounted view; false = the chase cameras run instead (other modes, or the car is
/// upside down in hood/bumper). `car_dir` = the car's converted folder (for its mount data); `data` = camera.zip
/// (effect stacks + ini); `inputs` = the car signals for the effect layers.
#[allow(clippy::too_many_arguments)]
pub(super) fn update(ct: &mut Transform, pose: &CarPose, eye_local: Option<Vec3>, rig: &CameraRig, state: &mut ViewState, car_dir: Option<(&Path, &str)>, data: Option<&CameraData>, inputs: &Inputs, dt: f32) -> bool {
    if !mounted(rig) {
        state.look = Look::default();
        state.flipped = false;
        state.fx = None;
        state.effects = Effects::default();
        state.away = false;
        return false;
    }
    let ini = *state.ini.get_or_insert_with(|| Ini::new(data));
    if let Some((dir, media)) = car_dir {
        if state.mounts.as_ref().map(|m| m.media.as_str()) != Some(media) {
            state.mounts = Mounts::load(dir, media, &ini);
            if let Some((_, stack)) = state.fx.as_mut() {
                stack.reset();
            }
        }
    }
    // Effect stack per view; the profile's CameraEffects option (default on) picks CameraPhysics.xml over
    // CameraPhysicsSansEffects.xml (no option in our menu yet: FH1_CAMERA_EFFECTS=0 = the reduced stack).
    if state.fx.as_ref().map(|f| f.0) != Some(rig.mode) {
        let name = match rig.mode {
            HOOD => "Hood",
            BUMPER => "BumperHigh",
            _ => "DriverCam",
        };
        let full = std::env::var("FH1_CAMERA_EFFECTS").map_or(true, |v| v != "0");
        let layers = data.map_or(&[][..], |d| if full { d.physics.cam(name) } else { d.sans_effects.cam(name) });
        state.fx = Some((rig.mode, Stack::new(layers)));
    }
    // Default on (FH1_VIEW_FX=0 = rigid view): the sums are applied as the camera manager does (0x82853DD0,
    // docs/CAMERA.md "Effect layers").
    let apply = fx_enabled();
    state.effects = if apply { state.fx.as_mut().map_or(Effects::default(), |(_, s)| s.step(inputs, dt)) } else { Effects::default() };
    let (position, rotation) = (pose.position, pose.rotation);

    // Rollover hand-over (hood and bumper only).
    let up_y = (rotation * Vec3::Y).y;
    if rig.mode != COCKPIT {
        if !state.flipped && up_y < 0.1 {
            state.flipped = true;
            state.upright = 0.0;
        } else if state.flipped {
            state.upright = if up_y > 0.5 { state.upright + dt } else { 0.0 };
            if state.upright > 1.0 {
                state.flipped = false;
            }
        }
    } else {
        state.flipped = false;
    }
    if state.flipped {
        return false;
    }

    let away = state.look.step(dt);
    state.away = away;
    // Mouse drag (rig.yaw/pitch, a PC extra) turns the head on top while not snapped away.
    let yaw = if away { state.look.yaw } else { state.look.yaw - rig.yaw };
    let m = state.mounts.as_ref();
    let (mount, pitch) = match rig.mode {
        // Driver's eye (fh1-render car.rs FxCockpitEye; docs/CAR_INTERIOR.md). Before the cockpit has loaded (or with
        // FH1_CARFX=0) it sits at the hood point.
        COCKPIT => (eye_local.or(m.map(|m| m.hood)).unwrap_or(Vec3::new(0.0, 0.9, -0.6)), 0.0),
        HOOD => m.map_or((Vec3::new(0.0, 0.9, -0.6), 0.0), |m| (m.hood, m.hood_pitch)),
        _ => (m.map_or(Vec3::new(0.0, 0.9, -1.4), |m| m.bumper), 0.0),
    };
    let mount = match (away, m) {
        (true, Some(m)) => m.relocate(mount, state.look.yaw),
        _ => mount,
    };
    // Rigid with the car (pitch and roll included). 0x82851518 / 0x82851870: forward*cos(yaw) + right*sin(yaw);
    // the hood also pitches by its offset. Effects as 0x82853DD0 applies them (VERIFIED code): eye += base right *
    // camera-space X (only X is read); camera-space yaw, pitch, roll about the view's own axes; then car-space roll,
    // pitch, yaw about the car's axes (= car * E(car_ypr) * car^-1 on the left); eye += car axes * car-space XYZ.
    let fx = state.effects;
    let base = rotation * Quat::from_rotation_y(-yaw) * Quat::from_rotation_x(pitch + rig.pitch * 0.5);
    ct.rotation = rotation * Effects::rotation(fx.car_ypr) * rotation.inverse() * base * Effects::rotation(fx.cam_ypr);
    ct.translation = position + rotation * (mount - pose.cg_model + fx.car_xyz) + base * Vec3::X * fx.cam_xyz.x;
    true
}

/// Rear-view mirror FOV: the RearView camera's 53° (global 0x8324BA24), taken as HORIZONTAL here (INFERRED: as a
/// vertical FOV it would span ~140° across the 4:1 mirror).
const MIRROR_HFOV_DEG: f32 = 53.0;

/// Drives the rear-view mirror render (fh1_render::mirror): on in the hood and bumper views only, the views the game
/// draws its HUD mirror in (camera vtable slots 19/20, docs/CAMERA.md "Rear-view mirror"), and not while the car is
/// upside down (those views hand over to the chase camera then).
#[allow(clippy::too_many_arguments)]
pub fn drive_mirror(
    rig: Res<CameraRig>,
    cars: Query<&crate::Car>,
    garage: Res<crate::Garage>,
    data: Option<Res<CameraData>>,
    settings: Res<crate::ui::Settings>,
    fixed: Res<Time<Fixed>>,
    mirror: Option<ResMut<fh1_render::mirror::MirrorView>>,
    mut mounts: Local<Option<Mounts>>,
) {
    let Some(mut mirror) = mirror else { return };
    let Ok(car) = cars.single() else { return };
    let v = &car.0;
    let (position, rotation) = v.render_pose(fixed.overstep_fraction());
    let on = mirror.allowed && matches!(rig.mode, HOOD | BUMPER) && !rig.photo && settings.hud && (rotation * Vec3::Y).y >= 0.1;
    if !on {
        if mirror.enabled {
            mirror.enabled = false;
        }
        return;
    }
    let media = v.data.media_name.as_str();
    if mounts.as_ref().map(|m| m.media.as_str()) != Some(media) {
        let ini = Ini::new(data.as_deref());
        *mounts = Mounts::load(&garage.assets.join("cars").join(&garage.cars[garage.current]), media, &ini);
    }
    let Some(m) = mounts.as_ref() else { return };
    let aspect = mirror.size.x as f32 / mirror.size.y.max(1) as f32;
    let vfov = 2.0 * ((MIRROR_HFOV_DEG.to_radians() * 0.5).tan() / aspect).atan();
    mirror.enabled = true;
    mirror.fov = vfov;
    mirror.transform = Transform { translation: position + rotation * (m.rear - v.cg_model), rotation: rotation * Quat::from_rotation_y(PI), ..default() };
}
