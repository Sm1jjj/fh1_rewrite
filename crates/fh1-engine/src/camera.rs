//! Chase / far / bonnet / cockpit cameras with free look, plus a photo mode.
//!
//! Free look: right stick or mouse drag orbits; it eases back behind the car shortly after you
//! let go (not in photo mode). Zoom: mouse wheel or D-pad up/down.
//! Photo mode (F, or the pause menu): the simulation pauses and the HUD hides (both via `ui`); the
//! camera stays where you put it. Esc / Start / B leaves it.
//! F12 (or A while in photo mode) saves a screenshot into `screenshots/`.
//!
//! Split: this file = rig state, input, mode dispatch; `chase` = chase/far follow cameras (and the photo orbit);
//! `views` = car-mounted views (bonnet/bumper/cockpit).

mod chase;
mod effects;
mod views;

pub use views::next_mode;
pub use views::drive_mirror;

use std::path::PathBuf;

use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseButton};
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};

use crate::Car;

#[derive(Resource)]
pub struct CameraRig {
    /// 0 chase, 1 far chase, 2 bonnet, 3 cockpit (driver's eye, the game's `Driver` camera).
    pub mode: u8,
    /// Orbit offsets from straight behind (radians).
    pub(super) yaw: f32,
    pub(super) pitch: f32,
    pub(super) zoom: f32,
    pub(super) idle: f32,
    pub photo: bool,
    pub(super) chase: chase::State,
}

impl Default for CameraRig {
    fn default() -> Self {
        Self { mode: 0, yaw: 0.0, pitch: 0.0, zoom: 1.0, idle: 0.0, photo: false, chase: chase::State::default() }
    }
}

/// The game's camera tuning from the `camera` setup group (camera.zip, parsed by `fh1_formats::camera`). Missing
/// files leave empty data: every camera then falls back to the disc values compiled in.
#[derive(Resource, Default)]
pub struct CameraData {
    /// CameraSettings.ini.
    pub settings: fh1_formats::camera::Settings,
    /// CameraPhysics.xml (effect layer stacks per view) and CameraPhysicsSansEffects.xml.
    pub physics: fh1_formats::camera::Physics,
    pub sans_effects: fh1_formats::camera::Physics,
}

impl CameraData {
    pub fn load(assets: &std::path::Path) -> Self {
        let dir = assets.join("camera");
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).ok();
        let Some(ini) = read("CameraSettings.ini") else {
            warn!("camera: {} missing (run fh1setup), using built-in camera values", dir.display());
            return Self::default();
        };
        Self {
            settings: fh1_formats::camera::Settings::parse(&ini),
            physics: read("CameraPhysics.xml").map(|x| fh1_formats::camera::Physics::parse(&x)).unwrap_or_default(),
            sans_effects: read("CameraPhysicsSansEffects.xml").map(|x| fh1_formats::camera::Physics::parse(&x)).unwrap_or_default(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn camera_input(
    mut rig: ResMut<CameraRig>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    pads: Query<&Gamepad>,
    time: Res<Time<Real>>,
    mut commands: Commands,
    mut started: Local<bool>,
) {
    let dt = time.delta_secs();
    // Automation: FH1_CAMERA_MODE=<n> starts in that camera mode.
    if !*started {
        *started = true;
        if let Some(m) = std::env::var("FH1_CAMERA_MODE").ok().and_then(|v| v.parse::<u8>().ok()) {
            rig.mode = m;
        }
    }
    // Automation: FH1_PHOTO_YAW=<degrees> (+ FH1_PHOTO_PITCH degrees, FH1_PHOTO_ZOOM) enters
    // photo mode at that orbit after 2 s.
    if let Some(yaw) = std::env::var("FH1_PHOTO_YAW").ok().and_then(|v| v.parse::<f32>().ok()) {
        if !rig.photo && time.elapsed_secs() > 2.0 {
            rig.yaw = yaw.to_radians();
            let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
            rig.pitch = env("FH1_PHOTO_PITCH", 3.0f32).to_radians();
            rig.zoom = env("FH1_PHOTO_ZOOM", 0.8);
            rig.photo = true;
        }
    }
    let mut look = Vec2::ZERO;
    let mut zoom = 0.0;
    let photo = keys.just_pressed(KeyCode::KeyF);
    let mut shot = keys.just_pressed(KeyCode::F12);

    if mouse.pressed(MouseButton::Left) || mouse.pressed(MouseButton::Right) {
        look += motion.delta * 0.005;
    }
    zoom -= scroll.delta.y * 0.1;
    for pad in &pads {
        // Right stick: car views and the chase cameras use the game's look controller; it orbits only in photo mode.
        if rig.photo && !views::owns_stick(&rig) {
            let stick = Vec2::new(pad.get(GamepadAxis::RightStickX).unwrap_or(0.0), pad.get(GamepadAxis::RightStickY).unwrap_or(0.0));
            if stick.length() > 0.15 {
                look += Vec2::new(stick.x, -stick.y) * 2.5 * dt;
            }
        }
        if pad.pressed(GamepadButton::DPadUp) {
            zoom -= dt;
        }
        if pad.pressed(GamepadButton::DPadDown) {
            zoom += dt;
        }
        shot |= rig.photo && pad.just_pressed(GamepadButton::South);
    }

    if photo {
        rig.photo = !rig.photo;
    }

    if look != Vec2::ZERO {
        rig.yaw -= look.x;
        let max_pitch = if rig.photo { 1.4 } else { 0.9 };
        rig.pitch = (rig.pitch + look.y).clamp(-0.35, max_pitch);
        rig.idle = 0.0;
    } else {
        rig.idle += dt;
    }
    rig.zoom = (rig.zoom + zoom).clamp(0.4, if rig.photo { 6.0 } else { 2.5 });

    // Ease back behind the car after a moment without look input.
    if !rig.photo && rig.idle > 0.6 {
        let k = 1.0 - (-4.0 * dt).exp();
        rig.yaw -= wrap(rig.yaw) * k;
        rig.pitch -= rig.pitch * k;
    }

    if shot {
        let dir = PathBuf::from("screenshots");
        let _ = std::fs::create_dir_all(&dir);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let path = dir.join(format!("fh1-{stamp}.png"));
        info!("screenshot -> {}", path.display());
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(path));
    }
}

pub(super) fn wrap(a: f32) -> f32 {
    (a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// The car's interpolated pose, shared by every camera.
pub(super) struct CarPose {
    pub position: Vec3,
    pub rotation: Quat,
    /// Flat forward (heading only).
    pub fwd: Vec3,
    /// Model-space centre of gravity (the body model is a child of the car at -cg, main.rs spawn_car).
    pub cg_model: Vec3,
}

#[allow(clippy::too_many_arguments)]
pub fn follow_camera(
    commands: Commands,
    cars: Query<&Car>,
    bodies: Query<views::BodyQuery, With<fh1_render::car::FxCarBody>>,
    // The UI session's overlay camera (ui::scene::UiCamera) is a Camera3d too.
    mut cam: Query<(&mut Transform, &mut Projection), With<fh1_render::post::FxPostCamera>>,
    mut rig: ResMut<CameraRig>,
    data: Option<Res<CameraData>>,
    track: Res<crate::track::Track>,
    time: Res<Time<Real>>,
    fixed: Res<Time<Fixed>>,
    pads: Query<&Gamepad>,
    keys: Res<ButtonInput<KeyCode>>,
    garage: Res<crate::Garage>,
    mut view: Local<views::ViewState>,
) {
    let _watch = crate::perf::watch("follow_camera");
    let (Ok(car), Ok((mut ct, mut proj))) = (cars.single(), cam.single_mut()) else { return };
    let v = &car.0;
    // Interpolated pose: the camera must follow the same smoothed pose the car is drawn at.
    let (position, rotation) = v.render_pose(fixed.overstep_fraction());
    let fwd = (rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(Vec3::NEG_Z);
    let pose = CarPose { position, rotation, fwd, cg_model: v.cg_model };

    let eye_local = views::sync_cockpit_view(commands, &bodies, &rig, &mut view);
    views::read_look(&mut view, &pads, &keys);
    let car_dir = garage.assets.join("cars").join(&garage.cars[garage.current]);
    let fx_inputs = effects::Inputs::from_vehicle(v, rotation);
    let mounted = views::update(&mut ct, &pose, eye_local, &rig, &mut view, Some((&car_dir, &v.data.media_name)), data.as_deref(), &fx_inputs, time.delta_secs());
    let fov = if mounted {
        views::fov(&rig, &view).unwrap_or(chase::FOV)
    } else {
        let empty = fh1_formats::camera::Settings::default();
        let settings = data.as_ref().map_or(&empty, |d| &d.settings);
        let inputs = chase::CarInputs {
            data: &v.data,
            fx: fx_inputs,
            physics: data.as_ref().map(|d| &d.physics),
            yaw_rate: v.angular_velocity.y,
            up: rotation * Vec3::Y,
            wheels_grounded: v.wheels.iter().filter(|w| w.grounded).count(),
            ground: track.ground.as_ref(),
        };
        let rig = &mut *rig;
        let mut st = std::mem::take(&mut rig.chase);
        st.look.get_or_insert_with(views::Look::smooth).read(&pads, &keys);
        let f = chase::update(&mut ct, &pose, &inputs, rig, &mut st, settings, time.delta_secs());
        rig.chase = st;
        f
    };
    if let Projection::Perspective(p) = &mut *proj {
        if p.fov != fov {
            p.fov = fov;
        }
    }
}
