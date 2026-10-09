//! Vehicle simulation: rigid body + 4 raycast suspension corners + slip-curve tyres +
//! engine/gearbox, stepped at a fixed rate. Independent of Bevy's ECS so it runs headless in tests.
//!
//! Frames: world +Y up. Body space = model space (origin = bottom-centre of the body) shifted so
//! the centre of mass is the origin;
//! the car's front is -Z and its right is +X.

//! Split by subsystem so parallel sessions can each own one file (orchestrator rule, 2026-10-03):
//! `brakes.rs` (brakes, handbrake, ABS), `drivetrain.rs` (engine, clutch, gearbox, torque split),
//! `steering.rs` (steering, TCS and other assists), `tyre.rs` (the game's tyre force, 82D2F348). This file keeps
//! the body, suspension and integration.

mod brakes;
pub mod contact;
mod drivetrain;
mod steering;
mod tyre;

pub use steering::TcsParams;
pub use tyre::{tyre_scales_enabled, TyreSurface};

use bevy::math::{Quat, Vec3};

use crate::data::CarData;

pub const GRAVITY: f32 = 9.81;

/// The game's suspension with a wheel vertical DOF, tyre spring, damper clamps, bump stop and bar damping
/// (default on; FH1_SUSP_GAME=0 = the old massless raycast suspension).
pub fn suspension_game() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SUSP_GAME").map_or(true, |v| v != "0"))
}

/// What a ray query against the world returns.
#[derive(Debug, Clone, Copy)]
pub struct GroundHit {
    pub distance: f32,
    pub point: Vec3,
    /// Unit surface normal, facing the query origin.
    pub normal: Vec3,
    /// The surface's tyre terms (FrictionScale, off-road blend, VelDep, ...; Asphalt = default).
    pub tyre: tyre::TyreSurface,
    pub surface: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct SphereContact {
    pub point: Vec3,
    /// Direction to push the sphere out along.
    pub normal: Vec3,
    pub depth: f32,
    pub surface: u8,
}

/// The world the car drives on.
pub trait Ground {
    /// First hit along origin + dir * t, t in 0..=max (dir normalised).
    fn ray(&self, origin: Vec3, dir: Vec3, max: f32) -> Option<GroundHit>;
    /// Every overlap of the sphere with world geometry (written to out, cleared first).
    fn sphere(&self, center: Vec3, radius: f32, out: &mut Vec<SphereContact>);
    /// [`Ground::sphere`] for a sphere that moved from `from` to `to` during the step: contacts it passed through
    /// (thin walls it stepped over) count too, with normals on the side it came from. Default: the overlap at `to`.
    fn sphere_sweep(&self, _from: Vec3, to: Vec3, radius: f32, out: &mut Vec<SphereContact>) {
        self.sphere(to, radius, out);
    }
}

/// Infinite plane at y = 0 (test track, parity runs).
pub struct FlatGround;

impl Ground for FlatGround {
    fn ray(&self, origin: Vec3, dir: Vec3, max: f32) -> Option<GroundHit> {
        if dir.y >= -1e-6 || origin.y < 0.0 {
            return None;
        }
        let t = origin.y / -dir.y;
        (t <= max).then(|| GroundHit { distance: t, point: origin + dir * t, normal: Vec3::Y, tyre: tyre::TyreSurface::default(), surface: 0 })
    }

    fn sphere(&self, center: Vec3, radius: f32, out: &mut Vec<SphereContact>) {
        out.clear();
        if center.y < radius {
            out.push(SphereContact { point: Vec3::new(center.x, 0.0, center.z), normal: Vec3::Y, depth: radius - center.y, surface: 0 });
        }
    }
}

/// FH1's Steering assist (ForzaProfile `Steering`, car+0x17F4: 0 Assisted, 1 Normal, 2 Simulation; docs/ASSISTS.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SteeringAssist {
    /// The game also steers along the race line (not available in free roam here); STM uses the tighter
    /// AutoSteerSTMSlip thresholds.
    Assisted,
    #[default]
    Normal,
    /// FrictionTorqueMod off (no damping of the tyres' yaw torque).
    Simulation,
}

/// FH1's Shifting assist (ForzaProfile `Shifting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Shifting {
    #[default]
    Automatic,
    /// Gear up / down buttons; the clutch works automatically.
    Manual,
    /// Gear up / down need the clutch pedal pressed; the pedal also slips the clutch.
    ManualClutch,
}

/// Driver assists that aren't per-tick inputs (set from the player's settings; `Controls` carries TCS and ABS).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Assists {
    /// Stability management (STM, car+0x1830): differential braking against a sliding rear.
    pub stm: bool,
    pub steering: SteeringAssist,
    pub shifting: Shifting,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Controls {
    /// -1 (left) .. 1 (right)
    pub steer: f32,
    pub throttle: f32,
    pub brake: f32,
    pub handbrake: f32,
    /// Traction control assist (FH1 offers TCS as an assist).
    pub tcs: bool,
    /// Anti-lock brakes assist.
    pub abs: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Wheel {
    /// Spin rate (rad/s), positive = rolling forward.
    pub omega: f32,
    /// Visual spin angle (rad).
    pub angle: f32,
    /// Suspension length from the top of travel to the wheel centre (m).
    pub length: f32,
    /// d(length)/dt (m/s, + = extending): the wheel's vertical degree of freedom (game suspension only).
    pub length_vel: f32,
    /// Tyre vertical deflection (m, wheel+0x18) and the suspension force before the >= 0 clamp (N).
    pub tyre_deflection: f32,
    pub steer: f32,
    pub load: f32,
    pub grounded: bool,
    pub slip_ratio: f32,
    pub slip_angle_deg: f32,
    /// Normalised slip ratio / slip angle (1 = the curve's peak at the wheel's load), signed like the raw values.
    pub norm_slip: f32,
    pub norm_slip_angle: f32,
    /// Surface id under the tyre (index into the world's surface list).
    pub surface: u8,
    /// Last tyre force (world space, N) and contact normal, for debugging.
    pub force: Vec3,
    pub normal: Vec3,
    /// SlidingInstability state (tyre.rs).
    instability: tyre::Instability,
}

/// Aerodynamic scales on drag and per-axle downforce (drafting, ai/race_physics.rs). `ONE` = the car's own numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AeroScale {
    pub drag: f32,
    /// Front, rear.
    pub down: [f32; 2],
}

impl AeroScale {
    pub const ONE: Self = Self { drag: 1.0, down: [1.0; 2] };
}

#[derive(Debug, Clone)]
pub struct Vehicle {
    pub data: CarData,
    pub position: Vec3,
    /// Pose at the start of the current fixed physics tick, for render interpolation.
    pub prev_position: Vec3,
    pub prev_rotation: Quat,
    pub rotation: Quat,
    pub velocity: Vec3,
    pub angular_velocity: Vec3,
    /// Body-collision contacts resolved so far (debugging).
    pub body_contacts: u64,
    /// Last body contact: sphere index, model-space sphere centre, contact normal, depth.
    pub last_contact: Option<(usize, Vec3, Vec3, f32, u8)>,
    /// World-space linear acceleration of the last step (m/s²).
    pub acceleration: Vec3,
    pub wheels: [Wheel; 4],
    /// Body-space top-of-travel anchor per corner.
    anchors: [Vec3; 4],
    /// Static suspension length (wheel centre at the modelled hub height).
    static_length: [f32; 4],
    rest_length: [f32; 4],
    max_length: [f32; 4],
    inertia: Vec3,
    /// Model-space position of the centre of mass.
    pub cg_model: Vec3,

    /// 0 = reverse, 1.. = forward gears.
    pub gear: usize,
    pub rpm: f32,
    /// Current boost multiplier (1 for naturally aspirated engines).
    pub boost: f32,
    /// Engine torque this step as a signed fraction of the curve's peak (negative = engine braking).
    pub torque_fraction: f32,
    /// Increments on every gear change (including into/out of reverse), for audio and UI.
    pub shift_count: u32,
    /// Test switch: skip the speed-sensitive steering-lock reduction.
    pub full_lock: bool,
    /// Direct steering (traffic drivers): road-wheel angle = input x the speed lock, turning-rate limited, without the
    /// mode-0 fallbacks (steering.rs; like the game's automated-driver mode 4, 82D2FFB8).
    pub direct_steer: bool,
    /// Turn 10's stats-harness mode (CAutomatedCarStatsImp "test mode"): set by the parity tool only.
    pub test_mode: bool,
    /// TorqueFree tyre scales in use (tyre.rs): the compound's in game, 1.0 in the stats harness.
    pub torque_free: crate::data::TorqueFree,
    drivetrain: drivetrain::DrivetrainState,
    steering: steering::SteeringState,
    brakes: brakes::BrakeState,
    /// Traction-control throttle cut this tick (0 = not intervening); read by the HUD.
    pub tcs_cut: f32,
    /// Assists other than TCS / ABS (the game copies them into the car at setup, 82D32F08).
    pub assists: Assists,
    /// STM differential braking this tick: per-wheel brake input added to the pedal (82D20FF8, wheel+0x24).
    stm_brake: [f32; 4],
    /// STM intervened this tick (car+0x1698); read by the HUD.
    pub stm_active: bool,
    /// Manual gearbox: pending gear change (+1 up, -1 down), taken at the next tick.
    pub shift_request: i8,
    /// Clutch pedal (0 = engaged .. 1 = pressed), Manual with clutch only.
    pub clutch_pedal: f32,
    /// Engine torque multiplier (1 = stock): the AI's rubber band torque cut / catch-up boost (ai/, docs/AI.md).
    pub torque_mult: f32,
    /// Drafting scales on drag / downforce, set every tick by the race code (ai/race_physics.rs); `AeroScale::ONE` otherwise.
    pub aero_scale: AeroScale,
    /// Race-AI traction-control numbers; `None` = the player's (steering.rs).
    pub tcs_params: Option<steering::TcsParams>,
    /// Stats-harness lateral test: clutch pedal held in (gearbox state 9, car+0x137C = 1; docs/HANDLING_PARITY.md §1).
    pub clutch_in: bool,
    /// Grounded wheels on an off-road surface (OffRoadness > 0) last tick (car+0x5031, 82D35B90).
    pub offroad_wheels: u8,
    /// Random state for SlidingInstability's periods (tyre.rs).
    rng: u32,
}

impl Vehicle {
    pub fn new(data: CarData, spawn: Vec3) -> Self {
        let wb_front = data.hubs[0][2];
        let wb_rear = data.hubs[2][2];
        // Model space: origin at the bottom of the body; at static ride height each wheel centre is
        // at its hub, so the ground is at hub y - tyre radius.
        let ground_y = 0.5 * ((data.hubs[0][1] - data.tyre_radius[0]) + (data.hubs[2][1] - data.tyre_radius[1]));
        let cg_z = wb_front + (1.0 - data.front_weight) * (wb_rear - wb_front);
        let cg_model = Vec3::new(0.0, ground_y + data.cg_height, cg_z);

        let mut anchors = [Vec3::ZERO; 4];
        let mut static_length = [0.0; 4];
        let mut rest_length = [0.0; 4];
        let mut max_length = [0.0; 4];
        for i in 0..4 {
            let axle = i / 2;
            let s = &data.suspension[axle];
            let travel = s.max_compress.max(0.03);
            let hub = Vec3::from(data.hubs[i]);
            anchors[i] = hub - cg_model + Vec3::Y * travel;
            static_length[i] = travel;
            let axle_load = if axle == 0 { data.front_weight } else { 1.0 - data.front_weight };
            let corner_weight = data.mass * GRAVITY * axle_load * 0.5;
            rest_length[i] = travel + corner_weight / s.spring;
            max_length[i] = travel + 0.12;
        }

        let [x, y, z] = data.block_dims;
        let m = data.mass / 12.0;
        let inertia = Vec3::new(m * (y * y + z * z), m * (x * x + z * z), m * (x * x + y * y));

        let mut v = Self {
            position: spawn + Vec3::Y * (cg_model.y - ground_y),
            prev_position: spawn + Vec3::Y * (cg_model.y - ground_y),
            prev_rotation: Quat::IDENTITY,
            rotation: Quat::IDENTITY,
            velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
            acceleration: Vec3::ZERO,
            body_contacts: 0,
            last_contact: None,
            wheels: [Wheel::default(); 4],
            anchors,
            static_length,
            rest_length,
            max_length,
            inertia,
            cg_model,
            gear: 1,
            rpm: data.idle_rpm,
            boost: data.boost.map(|b| b.min_scale).unwrap_or(1.0),
            torque_fraction: 0.0,
            shift_count: 0,
            full_lock: false,
            direct_steer: false,
            test_mode: false,
            torque_free: Self::default_torque_free(&data),
            drivetrain: drivetrain::DrivetrainState::new(data.idle_rpm),
            steering: steering::SteeringState::default(),
            brakes: brakes::BrakeState::default(),
            tcs_cut: 0.0,
            assists: Assists::default(),
            stm_brake: [0.0; 4],
            stm_active: false,
            shift_request: 0,
            clutch_pedal: 0.0,
            torque_mult: 1.0,
            aero_scale: AeroScale::ONE,
            tcs_params: None,
            offroad_wheels: 0,
            clutch_in: false,
            rng: 0x9E37_79B9,
            data,
        };
        for (i, w) in v.wheels.iter_mut().enumerate() {
            w.length = v.static_length[i];
        }
        v
    }

    pub fn speed(&self) -> f32 {
        self.velocity.length()
    }

    /// Signed speed along the car's forward axis.
    pub fn forward_speed(&self) -> f32 {
        self.velocity.dot(self.rotation * Vec3::NEG_Z)
    }

    fn tyre_radius(&self, i: usize) -> f32 {
        self.data.tyre_radius[i / 2]
    }

    /// ABS limited a wheel this tick (car+0x169C); read by the HUD.
    pub fn abs_active(&self) -> bool {
        self.brakes.abs_active
    }

    /// Visual suspension offset of a wheel from its modelled hub (m, +Y up).
    pub fn wheel_drop(&self, i: usize) -> f32 {
        self.static_length[i] - self.wheels[i].length
    }

    pub fn step(&mut self, input: Controls, dt: f32, ground: &dyn Ground) {
        let start_pose = (self.position, self.rotation);
        let up = self.rotation * Vec3::Y;
        let down = -up;
        let mut force = Vec3::NEG_Y * self.data.mass * GRAVITY;
        let mut torque = Vec3::ZERO;

        let fwd_speed = self.forward_speed();
        let (throttle, brake) = self.select_direction(input.throttle, input.brake, fwd_speed, dt);
        // The game's standstill brake hold (82D30BE8) and our line lock (brakes.rs).
        let brake = self.standstill_brake(throttle, brake, input.handbrake, dt);
        self.update_line_lock(throttle, brake);
        self.update_steering(input, dt);
        let d = &self.data;

        // --- suspension ---
        let mut contact = [None::<(Vec3, Vec3, TyreSurface)>; 4];
        let mut compression = [0.0f32; 4];
        if suspension_game() {
            self.suspension_game(ground, down, dt, &mut contact);
        } else {
        for i in 0..4 {
            let r = self.tyre_radius(i);
            let anchor = self.position + self.rotation * self.anchors[i];
            // Ray from the top of travel: the wheel centre sits r above the hit.
            let hit = ground.ray(anchor, down, self.max_length[i] + r);
            let length = hit.map_or(f32::MAX, |h| h.distance - r);
            let w = &mut self.wheels[i];
            if let (Some(h), true) = (hit, length < self.max_length[i]) {
                let length = length.max(0.0);
                let vel = (length - w.length) / dt;
                w.length = length;
                compression[i] = self.rest_length[i] - length;
                let s = d.suspension[i / 2];
                let damper = if vel < 0.0 { -s.bump * vel } else { -s.rebound * vel };
                // Bump stop at the top of travel.
                let bump_stop = if length < 0.01 { (0.01 - length) * s.spring * 20.0 } else { 0.0 };
                let f = (s.spring * compression[i] + damper + bump_stop).max(0.0);
                w.load = f;
                w.grounded = true;
                w.surface = h.surface;
                contact[i] = Some((h.point, h.normal, h.tyre));
            } else {
                w.length = self.max_length[i];
                w.load = 0.0;
                w.grounded = false;
            }
        }
        // Anti-roll bars move load across each axle.
        for axle in 0..2 {
            let (l, r) = (axle * 2, axle * 2 + 1);
            if self.wheels[l].grounded && self.wheels[r].grounded {
                let f = (compression[l] - compression[r]) * d.suspension[axle].anti_roll;
                self.wheels[l].load = (self.wheels[l].load + f).max(0.0);
                self.wheels[r].load = (self.wheels[r].load - f).max(0.0);
            }
        }
        }

        // SlidingInstability (82D2BBB0): per-wheel load multiplier while the tyre slides.
        self.sliding_instability(dt, contact.map(|c| c.map(|(_, _, surf)| surf.friction)));

        self.offroad_wheels = contact.iter().filter(|c| c.is_some_and(|(_, _, s)| s.offroadness > 0.0)).count() as u8;
        let throttle = self.traction_control(input, throttle, fwd_speed);
        self.stability_management();
        let drive = self.update_drivetrain(throttle, fwd_speed, dt);
        let brake_torques = self.brake_torques(input, brake, dt);

        // --- tyres --- pass 1: forces from the current wheel speeds (tyre.rs, the game's 82D2F348).
        let mut reaction = [0.0f32; 4];
        // The tyres' contribution to the chassis torque (for FrictionTorqueMod, applied after the loop).
        let mut tyre_torque = Vec3::ZERO;
        for i in 0..4 {
            let r = self.tyre_radius(i);
            let mut w = self.wheels[i];
            let brake_torque = brake_torques[i];

            let mut fx = 0.0;
            if let Some((cp, normal, surf)) = contact[i] {
                let steer = Quat::from_rotation_y(w.steer);
                // Tyre axes lie in the contact surface's plane.
                let fwd = (self.rotation * steer * Vec3::NEG_Z).reject_from(normal).normalize_or_zero();
                let right = fwd.cross(normal);
                let v = self.velocity + self.angular_velocity.cross(cp - self.position);
                let v_long = v.dot(fwd);
                let v_lat = v.dot(right);

                // Stats harness: driven wheels never slip past the curve's peak (drivetrain.rs, 82D2F6B4).
                let denom = v_long.abs().max(drivetrain::slip_floor());
                w.omega = self.harness_slip_clamp(i, w.omega, v_long, denom, self.peak_slip_ratio_at(i, w.load));
                self.wheels[i].omega = w.omega;
                let t = tyre::TyreIn { load: w.load, v_long, v_lat, omega: w.omega, radius: r, surf };
                let out = self.tyre_force(i, &t, input.handbrake, input.abs, w.steer);
                let (mut f_long, mut f_lat) = (out.fx, out.fy);
                // Don't overshoot zero slip within one step (stability at low speed).
                let corner_mass = self.data.mass * 0.25;
                let lat_cap = v_lat.abs() * corner_mass / dt;
                f_lat = f_lat.clamp(-lat_cap, lat_cap);
                let long_cap =
                    (w.omega * r - v_long).abs() * self.data.wheel_inertia / (r * r * dt) + (drive[i].abs() + brake_torque) / r;
                f_long = f_long.clamp(-long_cap, long_cap);

                // Rolling resistance and scrub act on the body only (the wheel sees the tyre force).
                let f = fwd * (f_long + out.rolling_x) + right * (f_lat + out.rolling_y) + up * w.load;
                self.wheels[i].force = f;
                self.wheels[i].normal = normal;
                force += f;
                // The chassis torque (82D36DF8's second vector) uses the tyre force WITHOUT the TorqueFree scales: the
                // extra normal-play grip pushes the car but adds no roll / pitch / yaw moment (docs/HANDLING_PARITY.md
                // 8.11; verified on the live Viper's summed torque). Force and arm still at the contact patch.
                let f_torque = fwd * ((f_long + out.rolling_x) / out.long_scale.max(1e-3))
                    + right * ((f_lat + out.rolling_y) / self.torque_free.lat.max(1e-3))
                    + up * w.load;
                tyre_torque += (cp - self.position).cross(f_torque);
                fx = f_long;
                let wm = &mut self.wheels[i];
                wm.slip_ratio = out.slip_ratio;
                wm.slip_angle_deg = out.slip_angle_deg;
                wm.norm_slip = out.sigma_x;
                wm.norm_slip_angle = out.sigma_y;
                self.filter_rho(i, out.rho, dt);
            } else {
                self.filter_rho(i, 0.0, dt);
            }
            reaction[i] = -fx * r;
            if brakes::brake_in_solve() {
                // 82D37C70: the brake torque is part of the wheel's torque in the coupled solve, with a smooth sign of ω
                // (full above π rad/s, linear below). The linear part is a stiff damper: it is integrated implicitly
                // in pass 2 (the game's RK step at 359 Hz damps it; an explicit step oscillates tick to tick).
                let w = self.wheels[i].omega;
                if w.abs() >= std::f32::consts::PI {
                    reaction[i] -= brake_torque * w.signum();
                }
            }
        }

        // FrictionTorqueMod (82D2DE58, steering.rs): the yaw part of the tyre torque is scaled and clamped by the body slip
        // angle, the speed and the car's mass, unless the Steering assist is Simulation.
        torque += self.friction_torque_mod(tyre_torque);
        torque += self.engine_torque_body_roll();
        // Static friction at a standstill (82D2E4A8): holds the car against the slope / the rest of the forces.
        let grip = contact.map(|c| c.map(|(_, _, s)| s.friction));
        let (f_hold, t_hold) = self.static_hold(force, throttle, brake, input, grip, dt);
        force += f_hold;
        torque += t_hold;

        // Limited-slip diffs move torque from the faster wheel to the slower (drivetrain.rs).
        let locks = self.diff_locks(drive.torque);
        for i in 0..4 {
            reaction[i] += locks[i];
        }
        // Pass 2: wheel spin from the coupled driveline (drivetrain.rs, 82D396E0 / 82D39AE0), then brakes (can lock).
        let alpha = self.driveline_accels(&drive, reaction);
        for i in 0..4 {
            let w = &mut self.wheels[i];
            let mut omega = w.omega + alpha[i] * dt;
            if brakes::brake_in_solve() {
                let pi = std::f32::consts::PI;
                if w.omega.abs() < pi {
                    // Implicit: dω = (α - T·ω/(π·I))·dt  ->  ω' = (ω + α·dt) / (1 + dt·T/(π·I)).
                    omega /= 1.0 + dt * brake_torques[i] / (pi * self.data.wheel_inertia);
                } else if omega * w.omega < 0.0 && brake_torques[i] > 0.0 {
                    // The constant part can't carry the wheel through zero within one tick.
                    omega = 0.0;
                }
            } else {
                let db = brake_torques[i] / self.data.wheel_inertia * dt;
                omega = if omega.abs() <= db { 0.0 } else { omega - db * omega.signum() };
            }
            w.omega = omega;
            w.angle = (w.angle + omega * dt) % std::f32::consts::TAU;
        }

        // Aerodynamics: drag at the centre of mass, downforce at each axle.
        let d = &self.data;
        force -= self.velocity * self.velocity.length() * (d.drag_k * self.aero_scale.drag);
        let v_fwd = self.forward_speed();
        for axle in 0..2 {
            let df = down * (d.downforce_k[axle] * self.aero_scale.down[axle]) * v_fwd * v_fwd;
            let at = self.position + self.rotation * (0.5 * (self.anchors[axle * 2] + self.anchors[axle * 2 + 1]));
            force += df;
            torque += (at - self.position).cross(df);
        }

        // --- integrate ---
        self.acceleration = force / d.mass + Vec3::Y * GRAVITY;
        self.velocity += force / d.mass * dt;
        let inv = self.rotation.inverse();
        let w_body = inv * self.angular_velocity;
        let t_body = inv * torque;
        let iw = self.inertia * w_body;
        let dw = (t_body - w_body.cross(iw)) / self.inertia;
        self.angular_velocity = self.rotation * (w_body + dw * dt);
        // 82D2B540: total angular speed hard-clamped to 6 rad/s (consts 0x82144740 = 36, 0x82000C90 = 6) after each
        // integration. `FH1_ANGVEL_CLAMP=0` = no clamp (before 2026-10-08).
        static CLAMP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *CLAMP.get_or_init(|| std::env::var("FH1_ANGVEL_CLAMP").map_or(true, |v| v != "0")) {
            let w2 = self.angular_velocity.length_squared();
            if w2 > 36.0 {
                self.angular_velocity *= 6.0 / w2.sqrt();
            }
        }
        self.position += self.velocity * dt;
        let spin = Quat::from_xyzw(self.angular_velocity.x, self.angular_velocity.y, self.angular_velocity.z, 0.0) * self.rotation;
        self.rotation = (self.rotation + spin * (0.5 * dt)).normalize();

        self.collide_body(ground, start_pose);
    }

    /// Static-friction hold (82D2E4A8, VERIFIED from code; added after the tyre forces and FrictionTorqueMod): with all
    /// four wheels down, |v|^2 <= 4 (<= 1 while steering with neither brake nor handbrake), |w|^2 <= 0.09, upright (up.y
    /// >= 0.7) and no throttle, the remaining force along the body's lateral axis, and along its forward axis when the
    /// brake is past 0.787 or the handbrake past 0.1, is cancelled up to the tyres' grip: cap = min(N / mg, 1) x mu (load-
    /// weighted surface friction; the tyre factor tyre+12 is taken as 1). Within the cap the axis velocity is stopped this
    /// tick, beyond it the force is reduced by the cap. The force acts at the contact patches (by load), so it also
    /// cancels the matching torque. Returns (force, torque).
    fn static_hold(&self, f_in: Vec3, throttle: f32, brake: f32, input: Controls, grip: [Option<f32>; 4], dt: f32) -> (Vec3, Vec3) {
        const G_INV: f32 = 0.10197;
        if !brakes::standstill_hold_enabled() || self.test_mode || throttle > 1e-3 || grip.iter().any(Option::is_none) {
            return (Vec3::ZERO, Vec3::ZERO);
        }
        let (v2, w2) = (self.velocity.length_squared(), self.angular_velocity.length_squared());
        let up = self.rotation * Vec3::Y;
        let steering = input.steer.abs() > 0.157 && brake < throttle + 0.2 && input.handbrake < 0.1;
        if v2 > if steering { 1.0 } else { 4.0 } || w2 > 0.09 || up.y < 0.7 {
            return (Vec3::ZERO, Vec3::ZERO);
        }
        let n: f32 = self.wheels.iter().map(|w| w.load.max(0.0)).sum();
        if n <= 0.0 {
            return (Vec3::ZERO, Vec3::ZERO);
        }
        // Load-weighted surface FrictionScale (the game's surface record +4).
        let mu = self.wheels.iter().zip(grip).map(|(w, g)| w.load.max(0.0) * g.unwrap_or(1.0)).sum::<f32>() / n;
        let m = self.data.mass;
        let cap = (n / (m * GRAVITY)).min(1.0) * mu;
        let mut axes = vec![self.rotation * Vec3::X];
        if brake > 0.787 || input.handbrake > 0.1 {
            axes.push(self.rotation * Vec3::NEG_Z);
        }
        let (mut force, mut torque) = (Vec3::ZERO, Vec3::ZERO);
        for (k, a) in axes.iter().enumerate() {
            let a_ext = f_in.dot(*a) / m;
            let a_g = a_ext * G_INV;
            if a_g.abs() >= mu {
                if k == 0 {
                    continue;
                }
                break;
            }
            let v = self.velocity.dot(*a);
            if v.abs() >= 0.05 && v.signum() != (v + a_ext * dt).signum() {
                continue;
            }
            let accel = if cap * dt * GRAVITY > v.abs() { -v / dt - a_ext } else { -v.signum() * (cap - a_g.abs()) * GRAVITY };
            let f = *a * accel * m;
            force += f;
            for (i, w) in self.wheels.iter().enumerate() {
                let r = self.position + self.rotation * self.anchors[i] - self.position;
                torque += r.cross(f * (w.load.max(0.0) / n));
            }
        }
        (force, torque)
    }

    /// The game's suspension (82D2EC70 per wheel + the anti-roll bar 82D2F070; docs/HANDLING_PARITY.md 8.11/8.13): each
    /// wheel is a vertical degree of freedom with the unsprung mass, between the suspension (spring + clamped damper +
    /// bump stop + anti-roll bar + bar damping) and the tyre's vertical spring k_t x deflection (no tyre damping, live
    /// wheel+0x3B4/0x3B8 = 0). The body and the tyre friction see the suspension force clamped at >= 0 (wheel+0x2A4).
    fn suspension_game(&mut self, ground: &dyn Ground, down: Vec3, dt: f32, contact: &mut [Option<(Vec3, Vec3, TyreSurface)>; 4]) {
        let up = -down;
        let d = &self.data;
        let mut hits = [None::<GroundHit>; 4];
        let mut free = [f32::MAX; 4];
        let mut point_vel = [0.0f32; 4];
        for i in 0..4 {
            let r = self.tyre_radius(i);
            let anchor = self.position + self.rotation * self.anchors[i];
            point_vel[i] = (self.velocity + self.angular_velocity.cross(anchor - self.position)).dot(up);
            // Ray from a little above the top of travel (the bump stop lets the wheel pass it).
            let lift = 0.1;
            if let Some(h) = ground.ray(anchor - down * lift, down, self.max_length[i] + r + lift) {
                free[i] = h.distance - lift - r;
                hits[i] = Some(h);
            }
        }
        // Body acceleration along the suspension axis (last tick's, without gravity: self.acceleration is the specific
        // force), so the wheel's length is integrated relative to the moving body.
        let body_acc_down = self.acceleration.dot(down);
        let mut f_susp = [0.0f32; 4];
        for i in 0..4 {
            let s = d.suspension[i / 2];
            let w = &self.wheels[i];
            let comp = self.rest_length[i] - w.length;
            let v_comp = -w.length_vel;
            let mut damper = if v_comp >= 0.0 { s.bump * v_comp } else { s.rebound * v_comp };
            let mut spring = s.spring * comp;
            // Bump stop past the top of travel (wheel+0x2D4 = penetration): stiffness plus compression-only damping.
            if w.length < 0.0 {
                spring += s.bumpstop_k * -w.length;
                damper += s.bumpstop_c * v_comp.max(0.0);
            }
            let damper = damper.clamp(-s.rebound_clamp, s.bump_clamp);
            // Anti-roll bar (82D2F070): K (x_this - x_other) plus damping on the chassis points' vertical velocities.
            let o = i ^ 1;
            let bar = s.anti_roll * ((self.rest_length[i] - w.length) - (self.rest_length[o] - self.wheels[o].length))
                + s.bar_damping * (point_vel[o] - point_vel[i]);
            f_susp[i] = spring + damper + bar;
        }
        for i in 0..4 {
            let s = d.suspension[i / 2];
            let w = &mut self.wheels[i];
            let deflection = if free[i] < f32::MAX { (w.length - free[i]).max(0.0) } else { 0.0 };
            let f_tyre = s.tyre_k * deflection;
            // Wheel: m_u (a_wheel - a_body) along the suspension axis.
            let acc = (f_susp[i] - f_tyre) / s.unsprung - body_acc_down;
            w.length_vel += acc * dt;
            w.length += w.length_vel * dt;
            let (lo, hi) = (-0.05, self.max_length[i]);
            if w.length < lo || w.length > hi {
                w.length = w.length.clamp(lo, hi);
                w.length_vel = 0.0;
            }
            let deflection = if free[i] < f32::MAX { (w.length - free[i]).max(0.0) } else { 0.0 };
            w.tyre_deflection = deflection;
            match hits[i] {
                Some(h) if deflection > 0.0 => {
                    w.load = f_susp[i].max(0.0);
                    w.grounded = true;
                    w.surface = h.surface;
                    contact[i] = Some((h.point, h.normal, h.tyre));
                }
                _ => {
                    w.load = 0.0;
                    w.grounded = false;
                }
            }
        }
    }

    /// Resolve the car's collision spheres (MAXData) against the world: push out of overlaps and
    /// apply a contact impulse with a little bounce and friction.
    ///
    /// Swept (2026-10-08, "we fly straight through barriers"): the contact points are 0.05 m spheres and the walls are
    /// zero-thickness triangle sheets, so above ~50 mph a point stepped over a wall between two substeps (480 Hz).
    /// A sphere that moved more than its radius during the step (`start_pose` = position / rotation before it) is
    /// tested along its path as well ([`Ground::sphere_sweep`]). `FH1_BODY_SWEEP=0` = the old discrete test.
    fn collide_body(&mut self, ground: &dyn Ground, start_pose: (Vec3, Quat)) {
        const RESTITUTION: f32 = 0.15;
        const FRICTION: f32 = 0.35;
        static SWEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let sweep = *SWEEP.get_or_init(|| std::env::var("FH1_BODY_SWEEP").map_or(true, |v| v != "0"));
        let mut contacts = Vec::new();
        let inv_mass = 1.0 / self.data.mass;
        let spheres = self.data.collision_spheres.clone();
        for (si, (centre, radius)) in spheres.into_iter().enumerate() {
            let c = self.position + self.rotation * (centre - self.cg_model);
            let from = start_pose.0 + start_pose.1 * (centre - self.cg_model);
            let moved = c.distance(from);
            // Teleports (place / rewind) happen between steps, never inside one: a long path is a bug elsewhere.
            if sweep && moved > radius && moved < 5.0 {
                ground.sphere_sweep(from, c, radius, &mut contacts);
            } else {
                ground.sphere(c, radius, &mut contacts);
            }
            // Deepest contact only, so overlapping triangles don't push several times.
            let Some(ct) = contacts.iter().copied().max_by(|a, b| a.depth.total_cmp(&b.depth)) else { continue };
            self.body_contacts += 1;
            self.last_contact = Some((si, centre, ct.normal, ct.depth, ct.surface));
            self.position += ct.normal * ct.depth;
            let r = ct.point - self.position;
            let v = self.velocity + self.angular_velocity.cross(r);
            let vn = v.dot(ct.normal);
            crate::sfx_queue::wall(ct.point, ct.normal, -vn, (v - ct.normal * vn).length(), ct.surface);
            if vn >= 0.0 {
                continue;
            }
            let rot = self.rotation;
            let inertia = self.inertia;
            let inv_inertia = |t: Vec3| rot * ((rot.inverse() * t) / inertia);
            let k = |n: Vec3| inv_mass + n.dot(inv_inertia(r.cross(n)).cross(r));
            let jn = -(1.0 + RESTITUTION) * vn / k(ct.normal);
            let mut impulse = ct.normal * jn;
            let vt = v - ct.normal * vn;
            if let Some(t) = vt.try_normalize() {
                let jt = (vt.length() / k(t)).min(FRICTION * jn);
                impulse -= t * jt;
            }
            self.velocity += impulse * inv_mass;
            self.angular_velocity += inv_inertia(r.cross(impulse));
        }
    }

    /// Put the car back on its wheels at ground_point, facing yaw (radians about +Y; 0 = -Z).
    pub fn place(&mut self, ground_point: Vec3, yaw: f32) {
        let data = self.data.clone();
        // Drop in from a little above, so sloped or uneven ground doesn't start the car inside
        // its bump stops (the springs settle it within a fraction of a second).
        let (torque_free, test_mode, full_lock, assists, torque_mult, direct_steer) =
            (self.torque_free, self.test_mode, self.full_lock, self.assists, self.torque_mult, self.direct_steer);
        *self = Self::new(data, ground_point + Vec3::Y * 0.3);
        // Keep the run's switches (stats-harness tyre scales, test mode, full lock, assists, AI torque scale, direct steer).
        (self.torque_free, self.test_mode, self.full_lock, self.assists, self.torque_mult, self.direct_steer) =
            (torque_free, test_mode, full_lock, assists, torque_mult, direct_steer);
        self.rotation = Quat::from_rotation_y(yaw);
        self.prev_rotation = self.rotation;
    }

    /// Remember the current pose as the start of a fixed tick (call once per tick, before stepping).
    pub fn begin_tick(&mut self) {
        self.prev_position = self.position;
        self.prev_rotation = self.rotation;
    }

    /// Pose for rendering, `alpha` (0..1) of the way from the last tick's start to now. Physics runs
    /// at a fixed rate; drawing the raw pose judders when the frame rate isn't a multiple of it.
    pub fn render_pose(&self, alpha: f32) -> (Vec3, Quat) {
        let a = alpha.clamp(0.0, 1.0);
        (self.prev_position.lerp(self.position, a), self.prev_rotation.slerp(self.rotation, a))
    }

    /// EngineTorqueBodyRoll (82D39360 tail; docs/HANDLING_PARITY.md 8.18): the engine's own output torque (before the
    /// clutch, negative when engine braking) reacts on the body about its forward axis, fading out by speed²:
    /// τ = T x lerp(1 -> 0 over 0..10.18 m/s, on v²) x forward. Positive torque drops the right side (VERIFIED live, Pinyon
    /// Viper free-rev at a standstill). The game's EngineRotation / mounting columns are not used (every car alike).
    /// `FH1_ENGINE_ROLL=0` = off.
    fn engine_torque_body_roll(&self) -> Vec3 {
        const SPEED1: f32 = 10.18;
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("FH1_ENGINE_ROLL").map_or(true, |v| v != "0")) {
            return Vec3::ZERO;
        }
        let sv = (1.0 - self.velocity.length_squared() / (SPEED1 * SPEED1)).clamp(0.0, 1.0);
        if sv <= 0.0 {
            return Vec3::ZERO;
        }
        let engine_torque = self.torque_fraction * self.data.torque_scale.max(1.0);
        self.rotation * Vec3::NEG_Z * (engine_torque * sv)
    }

    /// Yaw (radians about +Y, 0 = facing -Z) of the car's heading.
    pub fn yaw(&self) -> f32 {
        let fwd = (self.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(Vec3::NEG_Z);
        (-fwd.x).atan2(-fwd.z)
    }

    /// Reset upright where the car is, onto the ground below (or at its current height).
    pub fn reset(&mut self, ground: &dyn Ground) {
        let above = self.position + Vec3::Y * 5.0;
        let point = ground.ray(above, Vec3::NEG_Y, 50.0).map_or(self.position - Vec3::Y * 0.5, |h| h.point);
        let yaw = self.yaw();
        self.place(point, yaw);
    }
}

#[cfg(test)]
mod tests {
    //! Parity checks against gamedb's own simulated numbers. Needs the converted install.
    use super::*;
    use std::path::PathBuf;

    fn car(name: &str) -> Option<CarData> {
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let dir = crate::data::private_assets(&data).ok()?.join("cars").join(name);
        CarData::load(&dir).ok()
    }

    /// Per-surface tyre terms against the live game (docs/HANDLING_PARITY.md 8.16): every recorded call of 82D2F348 on the
    /// Pinyon VIP_Viper_13 capture (tools/pinyon-shift/.local/fh1/probe-out/brake/tyre_*.bin, or FH1_TYRE_REC=<file>)
    /// is replayed through our slip normalisation (off-road blend b) and curve sampling (arcade layer) and compared with
    /// the game's own σx (+0x1EC), σy (+0x204), ρ (+0x25C), μy (+0x8C) and μx (+0x90), per surface id (+0x151).
    /// Skipped when the capture or the install is missing.
    #[test]
    fn live_surface_tyre_terms() {
        let path = std::env::var("FH1_TYRE_REC").map(PathBuf::from).unwrap_or_else(|_| {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/pinyon-shift/.local/fh1/probe-out/brake");
            std::fs::read_dir(&dir)
                .ok()
                .and_then(|r| r.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("tyre_"))))
                .unwrap_or_default()
        });
        let (Ok(bytes), Some(d)) = (std::fs::read(&path), car("VIP_Viper_13")) else {
            eprintln!("skipped: no live tyre records or no install");
            return;
        };
        // surfaceTypes.xml <Friction> of the three surfaces the capture drives on (ids 0 Asphalt, 1 Dirt, 2 Grass).
        let surfaces = [
            tyre::TyreSurface::default(),
            tyre::TyreSurface { friction: 0.95, offroadness: 1.0, offroad_peak_sa: 18.0, rear_grip: 0.93, arcade: 0.3, handbrake_grip: 0.3, ..Default::default() },
            tyre::TyreSurface { friction: 0.85, offroadness: 1.0, offroad_peak_sa: 18.0, rear_grip: 0.93, arcade: 0.2, handbrake_grip: 0.3, ..Default::default() },
        ];
        const REC: usize = 1280;
        const WHEEL: usize = 32 + 2 * 112;
        let le = |b: &[u8], o: usize| f32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let be = |b: &[u8], o: usize| f32::from_be_bytes(b[WHEEL + o..WHEEL + o + 4].try_into().unwrap());
        // Per surface: relative errors of σx, σy, ρ, μx, μy, and μ with the arcade layer off (to show it matters).
        let mut err: Vec<[Vec<f32>; 7]> = (0..3).map(|_| Default::default()).collect();
        let rel = |a: f32, b: f32| (a - b).abs() / b.abs().max(1e-3);
        for r in bytes.chunks_exact(REC) {
            let axle = (u32::from_le_bytes(r[8..12].try_into().unwrap()) as usize).min(3) / 2;
            let id = r[WHEEL + 0x151] as usize;
            let Some(surf) = surfaces.get(id) else { continue };
            let (load, v_long, v_lat, s) = (le(r, 16) * 100.0, le(r, 20), le(r, 24), le(r, 28));
            if load <= 0.0 || (v_long * v_long + v_lat * v_lat) < 1.0 {
                continue;
            }
            let (lat_c, long_c) = (&d.lateral, &d.longitudinal);
            let (lx, ly) = (long_c.clamp_load(load), lat_c.clamp_load(load));
            let (fx_frac, fy_frac) = (long_c.load_frac(lx), lat_c.load_frac(ly));
            let ss = d.tyre.slip_scale[axle];
            let (pk, pa) = (long_c.peak_slip_at(fx_frac) * ss, lat_c.peak_slip_at(fy_frac) * ss);
            let b = 1.0 + surf.offroadness * (surf.offroad_peak_sa / pa - 1.0);
            let kappa = s.abs() / v_long.abs().max(6.0);
            let sx = kappa / (b * pk);
            let sy = v_lat.abs().atan2(v_long.abs()).to_degrees() / (b * pa);
            let rho = (sx * sx + sy * sy).sqrt().max(1e-4);
            let (wx, wy) = (be(r, 0x58).max(surf.arcade), be(r, 0x5C).max(surf.arcade));
            let width = d.tyre.width_scale[axle];
            let mux = long_c.mu_layered(rho * pk / ss, fx_frac, wx) * width;
            let muy = lat_c.mu_layered(rho * pa / ss, fy_frac, wy) * width;
            let (mux0, muy0) = (long_c.mu_frac(rho * pk / ss, fx_frac) * width, lat_c.mu_frac(rho * pa / ss, fy_frac) * width);
            let e = &mut err[id];
            e[0].push(rel(sx, be(r, 0x1EC).abs()));
            e[1].push(rel(sy, be(r, 0x204).abs()));
            e[2].push(rel(rho, be(r, 0x25C)));
            e[3].push(rel(mux, be(r, 0x90)));
            e[4].push(rel(muy, be(r, 0x8C)));
            e[5].push(rel(mux0, be(r, 0x90)));
            e[6].push(rel(muy0, be(r, 0x8C)));
        }
        let med = |v: &mut Vec<f32>| {
            v.sort_by(f32::total_cmp);
            if v.is_empty() { f32::NAN } else { v[v.len() / 2] }
        };
        let p95 = |v: &Vec<f32>| if v.is_empty() { f32::NAN } else { v[v.len() * 95 / 100] };
        for (id, e) in err.iter_mut().enumerate() {
            let n = e[0].len();
            let m: Vec<(f32, f32)> = e.iter_mut().map(|v| (med(v), p95(v))).collect();
            eprintln!(
                "surface {id}: n {n}  rel err median/p95: σx {:.4}/{:.4} σy {:.4}/{:.4} ρ {:.4}/{:.4} μx {:.4}/{:.4} μy {:.4}/{:.4} | arcade off: μx {:.4}/{:.4} μy {:.4}/{:.4}",
                m[0].0, m[0].1, m[1].0, m[1].1, m[2].0, m[2].1, m[3].0, m[3].1, m[4].0, m[4].1, m[5].0, m[5].1, m[6].0, m[6].1
            );
            if n > 100 {
                for (k, name) in ["σx", "σy", "ρ", "μx", "μy"].iter().enumerate() {
                    assert!(m[k].0 < 0.01, "surface {id} {name}: median rel err {}", m[k].0);
                }
            }
        }
    }

    /// Lift-off recovery (user 2026-10-07: "RWD cars get stuck in drifts even when letting off"): power oversteer at
    /// 40 km/h, half lock, then throttle 0 and steering centred at `LIFT_AT` s; traces the rear wheelspin and the slide.
    /// `LIFT_CAR=<name>` picks the car (default the Camaro).
    #[test]
    fn lift_off_recovery() {
        let name = std::env::var("LIFT_CAR").unwrap_or_else(|_| "CHE_CamaroSS_69".into());
        let lift_at: f32 = std::env::var("LIFT_AT").ok().and_then(|s| s.parse().ok()).unwrap_or(1.5);
        let Some(d) = car(&name) else {
            eprintln!("skipped: run fhsetup first");
            return;
        };
        let mut v = Vehicle::new(d, Vec3::ZERO);
        v.assists.stm = false;
        let dt = 1.0 / std::env::var("LIFT_HZ").ok().and_then(|s| s.parse::<f32>().ok()).unwrap_or(480.0);
        for _ in 0..(1.0 / dt) as usize {
            v.step(Controls::default(), dt, &FlatGround);
        }
        let speed = std::env::var("LIFT_SPEED").ok().and_then(|s| s.parse().ok()).unwrap_or(11.0);
        v.velocity = v.rotation * Vec3::NEG_Z * speed;
        for w in &mut v.wheels {
            w.omega = speed / v.data.tyre_radius[0];
        }
        if speed > 0.0 {
            v.sync_drivetrain();
        }
        let mut t = 0.0f32;
        let mut next = 0.0f32;
        while t < lift_at + 4.0 {
            let lifted = t >= lift_at;
            let steer: f32 = std::env::var("LIFT_STEER").ok().and_then(|s| s.parse().ok()).unwrap_or(0.5);
            let c = if lifted { Controls::default() } else { Controls { steer, throttle: 1.0, ..Default::default() } };
            v.step(c, dt, &FlatGround);
            t += dt;
            if t >= next {
                next += 0.1;
                let fwd = v.rotation * Vec3::NEG_Z;
                let beta = if v.speed() > 2.0 { (v.velocity.normalize().dot(fwd).clamp(-1.0, 1.0)).acos().to_degrees() } else { 0.0 };
                let r = v.data.tyre_radius[1];
                let w = &v.wheels;
                eprintln!(
                    "t {t:4.1} {} v {:5.1} gear {} rpm {:5.0} clutch {} thr_frac {:+.2} | rear ωr/v {:.2} {:.2} σx {:+.2} {:+.2} σy {:+.2} {:+.2} load {:5.0} {:5.0} | β {beta:5.1}° yaw {:+5.0}°/s | front σy {:+.2} {:+.2} steer {:+5.1}°",
                    if lifted { "LIFT" } else { "gas " },
                    v.speed(), v.gear, v.rpm, v.drivetrain.clutch_locked as u8, v.torque_fraction,
                    w[2].omega * r / v.speed().max(0.1), w[3].omega * r / v.speed().max(0.1),
                    w[2].norm_slip, w[3].norm_slip, w[2].norm_slip_angle, w[3].norm_slip_angle, w[2].load, w[3].load,
                    v.angular_velocity.y.to_degrees(),
                    w[0].norm_slip_angle, w[1].norm_slip_angle, v.steer_angle().to_degrees()
                );
            }
        }
    }

    /// Slide recovery from a live state (Pinyon VIP_Viper_13 capture probe-out/brake, t 45.6: after a wall hit the car
    /// slides at 15.8 m/s forward / 7.8 m/s sideways (beta 26 deg), yaw rate ~0, stick centred, full throttle for 0.39 s
    /// then lift. Live: beta 26 -> 4.5 deg at lift + 0.4 s, restoring yaw rate builds to ~100 deg/s, straight by + 0.6 s).
    /// Env: REC_CAR, REC_VF / REC_VS (m/s forward / sideways), REC_GAS (s of full throttle).
    #[test]
    fn slide_recovery_from_state() {
        let var = |k: &str, d: f32| std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        let Some(d) = car(&std::env::var("REC_CAR").unwrap_or_else(|_| "VIP_Viper_13".into())) else {
            eprintln!("skipped: run fhsetup first");
            return;
        };
        let mut v = Vehicle::new(d, Vec3::ZERO);
        let dt = 1.0 / 480.0;
        for _ in 0..480 {
            v.step(Controls::default(), dt, &FlatGround);
        }
        let (vf, vs, gas) = (var("REC_VF", 15.8), var("REC_VS", 7.8), var("REC_GAS", 0.39));
        v.velocity = v.rotation * (Vec3::NEG_Z * vf + Vec3::X * vs);
        for w in &mut v.wheels {
            w.omega = vf / v.data.tyre_radius[0];
        }
        v.sync_drivetrain();
        let (mut t, mut next) = (0.0f32, 0.0f32);
        while t < gas + 1.5 {
            let c = if t < gas { Controls { throttle: 1.0, ..Default::default() } } else { Controls::default() };
            v.step(c, dt, &FlatGround);
            t += dt;
            if t >= next {
                next += 0.05;
                let (fwd, right) = (v.rotation * Vec3::NEG_Z, v.rotation * Vec3::X);
                let beta = v.velocity.dot(right).atan2(v.velocity.dot(fwd)).to_degrees();
                // + = turning the nose towards the velocity (straightening).
                let restoring = -v.angular_velocity.y.to_degrees() * beta.signum();
                let w = &v.wheels;
                eprintln!(
                    "t {:+5.2} v {:5.1} beta {beta:+6.1} restoring yaw {restoring:+6.1} | σy F {:+.2} {:+.2} R {:+.2} {:+.2} σx R {:+.2} {:+.2} | load F {:4.0} {:4.0} R {:4.0} {:4.0}",
                    t - gas, v.speed(), w[0].norm_slip_angle, w[1].norm_slip_angle, w[2].norm_slip_angle, w[3].norm_slip_angle,
                    w[2].norm_slip, w[3].norm_slip, w[0].load, w[1].load, w[2].load, w[3].load
                );
            }
        }
    }

    /// Standstill hold (82D30BE8 brake hold + 82D2E4A8 static friction; user 2026-10-08: "when I stop it just rolls"):
    /// rolling at 0.8 m/s with no input, the car stops within 2 s and stays put (no creep) for the next 3 s.
    #[test]
    fn standstill_hold() {
        let Some(d) = car(&std::env::var("HOLD_CAR").unwrap_or_else(|_| "VW_Corrado_95".into())) else {
            eprintln!("skipped: run fhsetup first");
            return;
        };
        let mut v = Vehicle::new(d, Vec3::ZERO);
        let dt = 1.0 / 480.0;
        for _ in 0..480 {
            v.step(Controls::default(), dt, &FlatGround);
        }
        v.velocity = v.rotation * Vec3::NEG_Z * 0.8;
        for w in &mut v.wheels {
            w.omega = 0.8 / v.data.tyre_radius[0];
        }
        let (mut t, mut moved) = (0.0f32, 0.0f32);
        while t < 5.0 {
            let p = v.position;
            v.step(Controls::default(), dt, &FlatGround);
            t += dt;
            if t > 2.0 {
                moved += Vec3::new(v.position.x - p.x, 0.0, v.position.z - p.z).length();
            }
        }
        eprintln!("standstill: speed {:.4} m/s after 5 s, moved {moved:.4} m in the last 3 s", v.speed());
        assert!(v.speed() < 0.02 && moved < 0.02, "car keeps rolling: {:.3} m/s, {moved:.3} m", v.speed());
    }

    /// Burnout (line lock, OUR rule): RWD at rest, full throttle + full brake for 3 s: the car stays (almost) put while the
    /// rear wheels spin well past road speed, and the gearbox stays in a forward gear.
    #[test]
    fn line_lock_burnout() {
        let Some(d) = car(&std::env::var("BURN_CAR").unwrap_or_else(|_| "DOD_ViperSRT10ACRX_12".into())) else {
            eprintln!("skipped: run fhsetup first");
            return;
        };
        let mut v = Vehicle::new(d, Vec3::ZERO);
        let dt = 1.0 / 480.0;
        for _ in 0..480 {
            v.step(Controls::default(), dt, &FlatGround);
        }
        let start = v.position;
        let mut t = 0.0f32;
        while t < 3.0 {
            v.step(Controls { throttle: 1.0, brake: 1.0, ..Default::default() }, dt, &FlatGround);
            t += dt;
        }
        let r = v.data.tyre_radius[1];
        let rear = 0.5 * (v.wheels[2].omega + v.wheels[3].omega) * r;
        let moved = Vec3::new(v.position.x - start.x, 0.0, v.position.z - start.z).length();
        eprintln!("burnout: moved {moved:.2} m, speed {:.2} m/s, rear surface speed {rear:.1} m/s, gear {}, rpm {:.0}, line lock {}", v.speed(), v.gear, v.rpm, v.brakes.line_lock);
        assert!(v.gear >= 1, "gearbox left the forward gears");
        assert!(rear > 8.0, "rears don't spin: {rear:.1} m/s");
        assert!(moved < 3.0, "car drove off: {moved:.1} m");
    }

    /// Camera-input noise (user 2026-10-07: "the camera skips / jitters"): steady 25 m/s cruise with a gentle corner, 4
    /// substeps per 120 Hz tick as in game; prints how much the camera-effect inputs (acceleration in g, rear suspension)
    /// jump from one rendered frame to the next when frames alternate 14 / 27 ms.
    #[test]
    fn camera_input_noise() {
        let Some(d) = car(&std::env::var("NOISE_CAR").unwrap_or_else(|_| "DOD_ViperSRT10ACRX_12".into())) else {
            eprintln!("skipped: run fhsetup first");
            return;
        };
        let mut v = Vehicle::new(d, Vec3::ZERO);
        let dt = 1.0 / 480.0;
        for _ in 0..480 {
            v.step(Controls::default(), dt, &FlatGround);
        }
        v.velocity = v.rotation * Vec3::NEG_Z * 25.0;
        for w in &mut v.wheels {
            w.omega = 25.0 / v.data.tyre_radius[0];
        }
        v.sync_drivetrain();
        let (mut t, mut frame_t, mut k) = (0.0f32, 0.0f32, 0usize);
        let frames = [0.014f32, 0.027];
        let mut prev: Option<(f32, f32)> = None;
        let (mut jumps_long, mut jumps_lat, mut sub_long) = (Vec::new(), Vec::new(), Vec::new());
        let mut last = 0.0f32;
        while t < 6.0 {
            let thr = (0.35 + (25.0 - v.speed()) * 0.2).clamp(0.0, 1.0);
            v.step(Controls { throttle: thr, steer: 0.15, ..Default::default() }, dt, &FlatGround);
            t += dt;
            let fwd = v.rotation * Vec3::NEG_Z;
            let right = v.rotation * Vec3::X;
            let (gl, gt) = (v.acceleration.dot(fwd) / 9.80665, v.acceleration.dot(right) / 9.80665);
            if t > 2.0 {
                sub_long.push((gl - last).abs());
            }
            last = gl;
            if t >= frame_t {
                frame_t += frames[k % 2];
                k += 1;
                if t > 2.0 {
                    if let Some((pl, pt)) = prev {
                        jumps_long.push((gl - pl).abs());
                        jumps_lat.push((gt - pt).abs());
                    }
                    prev = Some((gl, gt));
                }
            }
        }
        let p = |v: &mut Vec<f32>, q: f32| {
            v.sort_by(f32::total_cmp);
            v[((v.len() - 1) as f32 * q) as usize]
        };
        eprintln!(
            "frame-to-frame |Δ g_long| p50 {:.3} p95 {:.3} | |Δ g_lat| p50 {:.3} p95 {:.3} | substep |Δ g_long| p50 {:.3} p95 {:.3}",
            p(&mut jumps_long, 0.5), p(&mut jumps_long, 0.95), p(&mut jumps_lat, 0.5), p(&mut jumps_lat, 0.95),
            p(&mut sub_long, 0.5), p(&mut sub_long, 0.95)
        );
    }

    /// Power oversteer (A1, docs/ASSISTS.md): RWD cars at 40 km/h in 2nd, half lock, then full throttle with TCS / STM off.
    /// The rear should break loose (rear normalised slip angle past 1, body slip growing). Prints per car; compare
    /// FH1_LSD=0 (open diffs) and FH1_FRICTION_TORQUE_MOD=0.
    #[test]
    fn power_oversteer() {
        let speed: f32 = std::env::var("OVERSTEER_SPEED").ok().and_then(|s| s.parse().ok()).unwrap_or(11.0);
        for (name, stm) in ["CHE_CamaroSS_69", "BMW_M3E92_08", "DOD_ViperSRT10ACRX_12", "FOR_MustangBOSS429_70"].into_iter().flat_map(|n| [(n, false), (n, true)]) {
            let Some(d) = car(name) else {
                eprintln!("skipped: run fhsetup first");
                return;
            };
            let mut v = Vehicle::new(d, Vec3::ZERO);
            v.assists.stm = stm;
            let dt = 1.0 / 480.0;
            for _ in 0..480 {
                v.step(Controls::default(), dt, &FlatGround);
            }
            v.velocity = v.rotation * Vec3::NEG_Z * speed;
            for w in &mut v.wheels {
                w.omega = speed / v.data.tyre_radius[0];
            }
            v.sync_drivetrain();
            let (mut t, mut max_rear, mut max_beta, mut max_yaw) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            let mut spun = f32::NAN;
            while t < 4.0 {
                let throttle = if t < 0.6 { 0.2 } else { 1.0 };
                v.step(Controls { steer: 0.5, throttle, ..Default::default() }, dt, &FlatGround);
                t += dt;
                let fwd = v.rotation * Vec3::NEG_Z;
                let beta = if v.speed() > 2.0 { v.velocity.normalize().dot(fwd).clamp(-1.0, 1.0).acos().to_degrees() } else { 0.0 };
                let rear = 0.5 * (v.wheels[2].norm_slip_angle + v.wheels[3].norm_slip_angle).abs();
                max_rear = max_rear.max(rear);
                max_beta = max_beta.max(beta);
                max_yaw = max_yaw.max(v.angular_velocity.y.abs().to_degrees());
                if spun.is_nan() && beta > 30.0 {
                    spun = t;
                }
            }
            let slip = 0.5 * (v.wheels[2].norm_slip + v.wheels[3].norm_slip);
            eprintln!(
                "{name} stm {stm}: max rear norm slip angle {max_rear:.2}, max body slip {max_beta:.1} deg, max yaw rate {max_yaw:.0} deg/s, \
                 beta>30 at {spun:.2} s, end speed {:.1} m/s gear {} rear norm slip {slip:.2}",
                v.speed(),
                v.gear
            );
        }
    }

    /// Manual gearbox: requests change gear, the clutch variant only with the pedal down, reverse only when slow.
    #[test]
    fn manual_shifting() {
        let Some(d) = car("CHE_CamaroSS_69") else {
            eprintln!("skipped: run fh1setup first");
            return;
        };
        let mut v = Vehicle::new(d, Vec3::ZERO);
        let dt = 1.0 / 480.0;
        let run = |v: &mut Vehicle, n: usize, throttle: f32| {
            for _ in 0..n {
                v.step(Controls { throttle, ..Default::default() }, dt, &FlatGround);
            }
        };
        v.assists.shifting = Shifting::Manual;
        run(&mut v, 480, 0.0);
        v.shift_request = -1;
        run(&mut v, 240, 0.0);
        assert_eq!(v.gear, 0, "down from 1st at rest = reverse");
        run(&mut v, 480, 1.0);
        assert!(v.forward_speed() < -0.5, "throttle in reverse drives backwards: {}", v.forward_speed());
        v.shift_request = 1;
        run(&mut v, 480, 0.0);
        assert_eq!(v.gear, 1);
        run(&mut v, 2400, 1.0);
        assert_eq!(v.gear, 1, "no automatic upshift");
        v.assists.shifting = Shifting::ManualClutch;
        v.shift_request = 1;
        run(&mut v, 10, 1.0);
        assert_eq!(v.gear, 1, "no shift without the clutch");
        v.clutch_pedal = 1.0;
        v.shift_request = 1;
        run(&mut v, 10, 1.0);
        assert_eq!(v.gear, 2, "shift with the clutch down");
    }

    /// Full throttle from rest: report 0-60 mph and speed after 30 s vs gamedb's figures.
    #[test]
    fn straight_line_alfa() {
        let Some(d) = car("ALF_8C_08") else {
            eprintln!("skipped: run fh1setup first");
            return;
        };
        let (ref060, reftop) = (d.reference_0_60_s, d.reference_top_speed);
        let mut v = Vehicle::new(d, Vec3::ZERO);
        let dt = 1.0 / 480.0;
        let input = Controls { throttle: 1.0, tcs: true, abs: true, ..Default::default() };
        // settle on the springs
        for _ in 0..480 {
            v.step(Controls::default(), dt, &FlatGround);
        }
        let mut t = 0.0;
        let mut t60 = None;
        while t < 60.0 {
            v.step(input, dt, &FlatGround);
            t += dt;
            if std::env::var_os("TRACE").is_some() && (t * 4.0).fract() < dt * 4.0 && t < 8.0 {
                eprintln!(
                    "t={t:4.2} v={:5.1} gear={} rpm={:5.0} rear slip {:+.2} load F/R {:.0}/{:.0}",
                    v.forward_speed(), v.gear, v.rpm, v.wheels[2].slip_ratio, v.wheels[0].load, v.wheels[2].load
                );
            }
            if t60.is_none() && v.forward_speed() >= 26.8224 {
                t60 = Some(t);
            }
        }
        let t60 = t60.unwrap_or(f32::NAN);
        eprintln!(
            "ALF_8C_08: 0-60 {t60:.2} s (gamedb {ref060:.2}), speed after 60 s {:.1} m/s (gamedb top {reftop:.1}), gear {}",
            v.forward_speed(),
            v.gear
        );
        assert!(t60.is_finite() && t60 < 10.0, "car should reach 60 mph");
        assert!((v.position.y - 0.0).abs() < 2.0, "car should stay on the ground");
    }
}
