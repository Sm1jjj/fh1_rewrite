//! Chase (mode 0 = the game's `FollowLowCam`, "Follow Cam") and far chase (mode 1 = `FollowHighCam`, "Follow Cam 2"),
//! following FH1's follow camera (docs/CAMERA.md "Follow cameras"); photo mode keeps a free orbit around the car.
//!
//! Per frame: a yaw spring pulls the camera's heading towards the car's (damped against the car's own yaw rate, capped,
//! weakened as wheels leave the ground); a height spring holds the camera at its rest height over the car; the radius is
//! the distance from the car to the rear of its bounding box plus a base distance. At activation the height is clamped
//! so the camera stands `HeightMin..HeightMax` above the ground (both equal on the disc: 1.8 m / 2.08 m), and the look
//! target moves by the same amount. A ground probe keeps the camera above the terrain.

use bevy::prelude::*;
use fh1_formats::camera::Settings;

use super::effects::{self, Effects};
use super::{CameraRig, CarPose};
use crate::vehicle::Ground;

/// Bevy's default vertical FOV, for photo mode.
pub(super) const FOV: f32 = std::f32::consts::FRAC_PI_4;

/// One follow camera's tuning: CameraSettings.ini `FollowLowCam\*` / `FollowHighCam\*` (the game's 56-byte block at
/// 0x8324BD5C / 0x8324BD94, field order = the ini's) plus the per-camera base height and radius the camera's setup
/// virtual writes (0x8284E350 Low: 0.35 m / 2.95 m; 0x8284E430 High: 0.6 m / 3.1 m).
#[derive(Clone, Copy, Debug)]
pub(super) struct Follow {
    pub height_offset: f32,
    pub radius_offset: f32,
    pub target_y_offset: f32,
    pub height_min: f32,
    pub height_max: f32,
    pub near_clip: f32,
    /// Degrees.
    pub fov: f32,
    pub yaw_k: f32,
    pub yaw_d: f32,
    pub yaw_car_damp: f32,
    pub yaw_max_speed: f32,
    pub height_k: f32,
    pub height_d: f32,
    pub base_height: f32,
    pub base_radius: f32,
}

impl Follow {
    /// Disc values (CameraSettings.ini), used when the camera group isn't installed.
    pub const LOW: Follow = Follow {
        height_offset: 0.9,
        radius_offset: -0.15,
        target_y_offset: 0.8,
        height_min: 1.8,
        height_max: 1.8,
        near_clip: 0.3,
        fov: 48.5,
        yaw_k: 4.5,
        yaw_d: 4.4,
        yaw_car_damp: 0.67,
        yaw_max_speed: 2.0,
        height_k: 30.0,
        height_d: 11.85,
        base_height: 0.35,
        base_radius: 2.95,
    };
    pub const HIGH: Follow = Follow {
        height_offset: -1.74,
        radius_offset: -0.2,
        target_y_offset: -1.79,
        height_min: 2.08,
        height_max: 2.08,
        fov: 58.0,
        base_height: 0.6,
        base_radius: 3.1,
        ..Follow::LOW
    };

    pub fn from_settings(s: &Settings, high: bool) -> Follow {
        let (sec, d) = if high { ("FollowHighCam", Follow::HIGH) } else { ("FollowLowCam", Follow::LOW) };
        let g = |k: &str, v: f32| s.get_or(&format!("{sec}\\{k}"), v);
        Follow {
            height_offset: g("HeightOffsetGlobal", d.height_offset),
            radius_offset: g("RadiusOffsetGlobal", d.radius_offset),
            target_y_offset: g("TargetYOffsetGlobal", d.target_y_offset),
            height_min: g("HeightMinAboveGround", d.height_min),
            height_max: g("HeightMaxAboveGround", d.height_max),
            near_clip: g("CamNearClip", d.near_clip),
            fov: g("FOV", d.fov),
            yaw_k: g("Spring\\YRotSpringK", d.yaw_k),
            yaw_d: g("Spring\\YRotSpringD", d.yaw_d),
            yaw_car_damp: g("Spring\\YRotCarDampScale", d.yaw_car_damp),
            yaw_max_speed: g("Spring\\YRotMaxSpeed", d.yaw_max_speed),
            height_k: g("Spring\\HeightSpringK", d.height_k),
            height_d: g("Spring\\HeightSpringD", d.height_d),
            ..d
        }
    }

    /// gamedb `CameraOverrides` `CamFollow{Low,High}{Height,Radius,TargetY}Offset`: a non-zero value REPLACES the
    /// global one (0x8284E350 / 0x8284E430).
    fn with_car(mut self, overrides: &std::collections::BTreeMap<String, f32>, high: bool) -> Follow {
        let p = if high { "CamFollowHigh" } else { "CamFollowLow" };
        for (k, f) in [("HeightOffset", &mut self.height_offset), ("RadiusOffset", &mut self.radius_offset), ("TargetYOffset", &mut self.target_y_offset)] {
            if let Some(&v) = overrides.get(&format!("{p}{k}")) {
                if v != 0.0 {
                    *f = v;
                }
            }
        }
        self
    }
}

/// Follow camera state (the game's follow camera object +0xF0..+0x11C).
#[derive(Default)]
pub struct State {
    /// The game's look controller in smooth mode (follow cam ctor sets +0x128 = 0; views.rs `Look`). Its yaw is the
    /// orbit offset (obj+0x11C; + = look right).
    pub(super) look: Option<super::views::Look>,
    /// The view's CameraPhysics.xml effect stack (`FollowCam` / `FollowCam2`), rebuilt on a camera switch.
    fx: Option<effects::Stack>,
    active: Option<u8>,
    /// Camera heading (radians, same convention as `Vehicle::yaw`) and its rate.
    yaw: f32,
    yaw_vel: f32,
    /// Sprung height over the car's position and its rate.
    height: f32,
    height_vel: f32,
    /// Seconds with fewer than 3 wheels on the ground.
    airborne: f32,
    /// Offsets after the activation height clamp.
    height_offset: f32,
    target_y_offset: f32,
    last_car: Vec3,
    /// Activation happened with the car in the air (our spawn drops it): redo the height clamp once it lands. The
    /// game resets its camera with the car already on the ground.
    clamp_pending: bool,
}

/// What the chase camera needs from the car beyond its pose.
pub(super) struct CarInputs<'a> {
    pub data: &'a crate::data::CarData,
    pub fx: effects::Inputs,
    pub physics: Option<&'a fh1_formats::camera::Physics>,
    pub yaw_rate: f32,
    /// The car's up axis (world).
    pub up: Vec3,
    pub wheels_grounded: usize,
    pub ground: &'a dyn Ground,
}

fn wrap(a: f32) -> f32 {
    super::wrap(a)
}

/// Height probe below `p` (the game's camera ground clamp 0x8284E1E0).
fn ground_below(ground: &dyn Ground, p: Vec3) -> Option<f32> {
    ground.ray(p + Vec3::Y * 2.0, Vec3::NEG_Y, 50.0).map(|h| h.point.y)
}

/// Places the chase camera; returns the vertical FOV (radians).
pub(super) fn update(ct: &mut Transform, pose: &CarPose, car: &CarInputs, rig: &CameraRig, st: &mut State, settings: &Settings, dt: f32) -> f32 {
    if rig.photo {
        photo(ct, pose, rig);
        st.active = None;
        return FOV;
    }
    let high = rig.mode == 1;
    let cfg = Follow::from_settings(settings, high).with_car(&car.data.camera, high);
    let (position, fwd) = (pose.position, pose.fwd);
    let heading = (-fwd.x).atan2(-fwd.z);

    // Car bounding box relative to the car's position (its centre of gravity), car frame (+Y up, front -Z).
    let bb_min = car.data.bbox[0] - pose.cg_model;
    let bb_max = car.data.bbox[1] - pose.cg_model;
    let top = bb_max.y;
    let rear = bb_max.z;

    // Activation (camera switch, first frame, or the car jumped: reset / respawn / teleport).
    let teleported = st.last_car.distance_squared(position) > 25.0;
    let activate = st.active != Some(rig.mode) || teleported;
    if activate || (st.clamp_pending && car.wheels_grounded >= 3) {
        let ground_y = ground_below(car.ground, position).unwrap_or(position.y + bb_min.y);
        let above = (position.y - ground_y) + top + cfg.base_height + cfg.height_offset;
        let c = above.clamp(cfg.height_min, cfg.height_max) - above;
        st.height_offset = cfg.height_offset + c;
        st.target_y_offset = cfg.target_y_offset + c;
        st.clamp_pending = car.wheels_grounded < 3;
    }
    if activate {
        st.yaw = heading;
        st.yaw_vel = 0.0;
        st.height = top + cfg.base_height;
        st.height_vel = 0.0;
        st.airborne = 0.0;
        st.active = Some(rig.mode);
        st.fx = car.physics.map(|p| effects::Stack::new(p.cam(if high { "FollowCam2" } else { "FollowCam" })));
    }
    st.last_car = position;
    // Frame time clamped like the game's camera code (1/15 s; effects stack 0x82857E50): the explicit springs
    // (height K = 30) blow up on a long frame (loading hitches gave h = 100 m).
    let dt = dt.clamp(-1.0 / 15.0, 1.0 / 15.0);
    let adt = dt.abs();

    // Yaw spring (0x82851E30). Stiffness and damping scale with the wheels' ground contact (average of a per-wheel
    // value; INFERRED = 1 when the wheel touches the ground).
    let contact = car.wheels_grounded as f32 / 4.0;
    let (k, d) = (cfg.yaw_k * contact, cfg.yaw_d * contact);
    let err = wrap(heading - st.yaw);
    // The damping acts on the camera's rate relative to YRotCarDampScale x the car's yaw rate (sign VERIFIED on Pinyon).
    st.yaw_vel += adt * (k * err - d * (st.yaw_vel - cfg.yaw_car_damp * car.yaw_rate));
    st.yaw_vel = st.yaw_vel.clamp(-cfg.yaw_max_speed, cfg.yaw_max_speed);
    st.yaw = wrap(st.yaw + adt * st.yaw_vel);

    // Radius (0x8284E088): rear of the bbox + base + offset, scaled so the car keeps its screen size while the FOV
    // effect widens the view (tan ratio; INFERRED half-angles). Effect layers: camera/effects.rs.
    let fx = st.fx.as_mut().map_or_else(Effects::default, |s| s.step(&car.fx, dt));
    let fov = cfg.fov.to_radians();
    let dolly = if fx.fov != 0.0 { (0.5 * fov).tan() / (0.5 * (fov + fx.fov)).tan() } else { 1.0 };
    let radius = (rear + cfg.base_radius + cfg.radius_offset) * dolly;
    // Orbit angle = yaw + look yaw (+ mouse orbit, our extra). Game heading = -ours, so a look to the right subtracts.
    let look = st.look.get_or_insert_with(super::views::Look::smooth);
    look.step(dt);
    let yaw = st.yaw - look.yaw + rig.yaw;
    let back = Vec3::new(yaw.sin(), 0.0, yaw.cos());

    // Height spring (0x8285AD98). While the car is upright (up.y > 0.7) and has been off the ground for under 0.2 s
    // (always, on the ground), the rest height follows the car's tilt: - radius * (back . up), clamped to
    // +-2 radius (0x8284DA00), so the camera rides up / down slopes with the car (sign and scale VERIFIED on Pinyon:
    // fit -0.975, 2 cm rms).
    if car.wheels_grounded >= 3 {
        st.airborne = 0.0;
    } else {
        st.airborne += adt;
    }
    let mut rest = top + cfg.base_height;
    if st.airborne < 0.2 && car.up.y > 0.7 {
        rest -= (radius * back.dot(car.up)).clamp(-2.0 * radius, 2.0 * radius);
    }
    st.height_vel += adt * ((rest - st.height) * cfg.height_k - cfg.height_d * st.height_vel);
    st.height += adt * st.height_vel;

    let mut eye = position + back * radius;
    eye.y = position.y + st.height_offset + st.height;
    let target = position + Vec3::Y * (top + st.target_y_offset);
    // Collision (0x8284E1E0; FollowCam\EnableCollision, default true): sweep from the look target to the camera
    // and stop short of the first hit (the game's sweep object isn't decoded; INFERRED 0.3 m margin). The
    // EnableCollision=false path is a ground probe: camera >= ground + 0.3 m.
    let to_eye = eye - target;
    let dist = to_eye.length();
    // How far the collision sweep pulled the camera in and the ground clamp lifted it (m), for FH1_CAM_LOG.
    let (mut pulled, mut lifted) = (0.0f32, 0.0f32);
    if dist > 1e-3 {
        if let Some(hit) = car.ground.ray(target, to_eye / dist, dist + GROUND_CLEARANCE) {
            let before = eye;
            eye = target + to_eye / dist * (hit.distance - GROUND_CLEARANCE).max(0.5);
            pulled = before.distance(eye);
        }
    }
    if let Some(gy) = ground_below(car.ground, eye) {
        lifted = (gy + GROUND_CLEARANCE - eye.y).max(0.0);
        eye.y = eye.y.max(gy + GROUND_CLEARANCE);
    }
    // Effects, as the camera manager applies them to every camera (0x82853DD0, VERIFIED code; docs/CAMERA.md
    // "Effect layers"): the follow pose is built first, then eye += base right * camera-space X (only X is read),
    // camera-space yaw/pitch/roll about the view's own axes, car-space roll/pitch/yaw about the car's axes (the car's
    // full orientation; the view turns in place, it does not orbit the target), eye += car axes * car-space XYZ.
    // FH1_VIEW_FX=0 = none of them (the FOV dolly above stays).
    ct.translation = eye;
    ct.look_at(target, Vec3::Y);
    if super::views::fx_enabled() {
        let car_frame = pose.rotation;
        let base = ct.rotation;
        ct.translation += base * Vec3::X * fx.cam_xyz.x + car_frame * fx.car_xyz;
        ct.rotation = car_frame * Effects::rotation(fx.car_ypr) * car_frame.inverse() * base * Effects::rotation(fx.cam_ypr);
    }
    if let Some(log) = cam_log() {
        // Same fields as the Pinyon hook at 0x8285B094 (docs/CAMERA.md), plus radius and the effect offsets.
        let _ = log.send(format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            dt, ct.translation.x, ct.translation.y, ct.translation.z, target.x, target.y, target.z, st.yaw, st.yaw_vel, st.height, st.height_vel,
            st.airborne, cfg.base_height, st.height_offset, cfg.base_radius, cfg.radius_offset, st.target_y_offset, rig.yaw, fx.fov,
            radius, rear, top, car.wheels_grounded, fx.car_xyz.x, fx.car_xyz.y, fx.car_xyz.z, fx.cam_xyz.x, fx.cam_xyz.y, fx.cam_xyz.z, pulled, lifted
        ));
    }
    fov + fx.fov
}

/// FH1_CAM_LOG=<file.csv>: one row per chase-camera update (debug / Pinyon comparison). Rows go through a channel to a
/// writer thread: the camera system must never wait on the disk (a per-frame write from here froze the main thread for
/// 10-26 s in the user's 2026-10-07 runs, caught by the stall watchdog).
fn cam_log() -> Option<&'static std::sync::mpsc::Sender<String>> {
    static LOG: std::sync::OnceLock<Option<std::sync::mpsc::Sender<String>>> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let file = std::fs::File::create(std::env::var("FH1_CAM_LOG").ok()?).ok()?;
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("fh1-cam-log".into())
            .spawn(move || {
                use std::io::Write;
                let mut w = std::io::BufWriter::with_capacity(1 << 16, file);
                while let Ok(line) = rx.recv() {
                    let _ = writeln!(w, "{line}");
                    for line in rx.try_iter() {
                        let _ = writeln!(w, "{line}");
                    }
                    let _ = w.flush();
                }
            })
            .ok()?;
        Some(tx)
    })
    .as_ref()
}

/// Minimum camera height over the ground probe (0x82074078; the probe itself uses 2.5 / 0.5 at 0x82015414 / 0x82000D38).
const GROUND_CLEARANCE: f32 = 0.3;

/// Photo mode: free orbit around the car (pre-existing behaviour).
fn photo(ct: &mut Transform, pose: &CarPose, rig: &CameraRig) {
    let (position, fwd) = (pose.position, pose.fwd);
    let (dist, height, look_up) = (6.0, 1.6, 0.9);
    let orbit = Quat::from_rotation_y(rig.yaw) * Quat::from_axis_angle(fwd.cross(Vec3::Y).normalize_or(Vec3::X), -rig.pitch);
    let back = orbit * -fwd;
    let target = position + back * dist * rig.zoom + Vec3::Y * height * rig.zoom.sqrt();
    ct.translation = Vec3::new(target.x, target.y.max(position.y + 0.15), target.z);
    ct.look_at(position + Vec3::Y * look_up * 0.6, Vec3::Y);
}
