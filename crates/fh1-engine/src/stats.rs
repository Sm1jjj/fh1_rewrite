//! FH1's stats harness (CAutomatedCarStatsImp, docs/HANDLING_PARITY.md §1 and §5) on our vehicle simulation, as a
//! library: the `parity` bin compares it with gamedb's `Data_Car.Sim*`, and the upgrade shop runs it on an upgraded car
//! (docs/PI.md) on a background task.
//!
//! - Launch + braking (states 1-3, default.xex 82D3B9E8 / 82D18EB8 / 82D295F0 / 82D18F10): test mode (no TCS),
//!   auto-clutch rates (ACSGlobalClutchIn/OutTime, ACSGlobalShiftTime 0.05 s); settle 1 s, then full throttle from
//!   idle with the clock starting at rollout (> 1 m/s) and top speed = max over the 90 s run; then, from that top speed,
//!   one instant full-brake stop with 100-0 / 60-0 measured from the crossings of 100 / 60 mph.
//! - Lateral (state 4, 82D19280): speed held by rescaling the velocity every tick (no throttle), steering ramped to keep
//!   the front tyres' normalised slip angle in [0.98, 1.2], result = time-average |lateral g| while it is in
//!   (0.95, 1.2), over 1..20 s.
//! - The dev car-stats screen that produced gamedb's numbers sets the compound's TorqueFree tyre scales to 1.0
//!   (825CDE58 -> 82D3B9E8 -> 82D18DB0); `Options::in_game` keeps them.

use bevy::math::Vec3;

use crate::data::CarData;
use crate::vehicle::{Controls, FlatGround, Vehicle};

pub const DT: f32 = 1.0 / 480.0;
pub const MPH: f32 = 0.44704;

/// Harness options (the `parity` bin's flags).
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Traction control in the settle phase and the lateral test (`--no-tcs` clears it).
    pub tcs: bool,
    /// Lateral test without the speed-sensitive steering-lock reduction (`--full-lock`).
    pub full_lock: bool,
    /// Keep the compound's TorqueFree tyre scales (normal play) instead of the stats screen's 1.0 (`--in-game`).
    pub in_game: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { tcs: true, full_lock: false, in_game: false }
    }
}

/// The harness results, gamedb `Data_Car.Sim*` units (s, m/s, m, g).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SimStats {
    pub time_to_60: f32,
    pub time_to_100: f32,
    pub quarter_mile_time: f32,
    pub quarter_mile_speed: f32,
    pub top_speed: f32,
    pub brake_60: f32,
    pub brake_100: f32,
    pub lat_g_60: f32,
    pub lat_g_120: f32,
}

/// A car as the stats screen builds it (TorqueFree scales 1.0), unless `o.in_game`.
pub fn new_vehicle(d: CarData, o: &Options) -> Vehicle {
    let mut v = Vehicle::new(d, Vec3::ZERO);
    if !o.in_game {
        v.override_torque_free(1.0);
    }
    v
}

pub fn assists(o: &Options) -> Controls {
    Controls { tcs: o.tcs, abs: true, ..Default::default() }
}

pub fn settle(v: &mut Vehicle, o: &Options) {
    for _ in 0..480 {
        v.step(assists(o), DT, &FlatGround);
    }
}

/// Every test of the harness.
pub fn sim_stats(d: &CarData, o: &Options) -> SimStats {
    let [time_to_60, time_to_100, quarter_mile_time, quarter_mile_speed, top_speed, brake_60, brake_100] = harness(d, o);
    SimStats {
        time_to_60,
        time_to_100,
        quarter_mile_time,
        quarter_mile_speed,
        top_speed,
        brake_60,
        brake_100,
        lat_g_60: lateral_game(d, 60.0 * MPH, o),
        lat_g_120: lateral_game(d, 120.0 * MPH, o),
    }
}

/// The game's lateral test at `speed` (m/s); NaN when the tyres never reached the measuring window.
pub fn lateral_game(d: &CarData, speed: f32, o: &Options) -> f32 {
    let mut v = new_vehicle(d.clone(), o);
    settle(&mut v, o);
    v.velocity = Vec3::NEG_Z * speed;
    for (i, w) in v.wheels.iter_mut().enumerate() {
        w.omega = speed / d.tyre_radius[i / 2];
    }
    v.sync_drivetrain();
    v.full_lock = o.full_lock;
    // The game's test forces gearbox state 9 with the clutch pedal in: no engine (drag) on the driven wheels.
    v.clutch_in = true;
    let (mut t, mut s, mut sum, mut time) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    while t < 20.0 {
        t += DT;
        // Front wheels' normalised slip angle (wheel+0x208; 1 = the lateral curve's peak at the wheel's load).
        let x = (v.wheels[0].norm_slip_angle.abs() + v.wheels[1].norm_slip_angle.abs()) * 0.5;
        if t > 1.0 {
            if x < 0.98 {
                s += if x <= 0.7 { 0.3 } else { 0.3 - 0.2 * (x - 0.7) / 0.28 } * DT;
            } else if x > 1.2 {
                s -= (0.1 + 0.2 * ((x - 1.2) / 1.8).min(1.0)) * DT;
            }
        }
        s = s.clamp(0.0, 1.0);
        if x > 0.95 && x < 1.2 {
            let right = (v.rotation * Vec3::X).reject_from(Vec3::Y).normalize_or_zero();
            sum += v.acceleration.dot(right).abs() * DT;
            time += DT;
        }
        v.velocity *= speed / v.velocity.length().max(0.1);
        v.step(Controls { steer: s, ..assists(o) }, DT, &FlatGround);
    }
    if time > 0.1 { sum / time / 9.80665 } else { f32::NAN }
}

/// Steering that holds the start line: heading error (rad) and sideways offset (m), both toward the line. The run
/// (state 2) keeps the car on its line like the game's harness 82D295F0: a saturating function of the body forward axis's
/// sideways component plus one of the sideways position, clamped to ±1. The gains are INFERRED (the game's constants
/// aren't read); without it a launch disturbance (EngineTorqueBodyRoll) left the car yawing for the rest of the run.
pub fn hold_line(v: &Vehicle, start: Vec3, line: Vec3) -> f32 {
    let side = line.cross(Vec3::Y); // right of the line
    let fwd = (v.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(line);
    let heading = fwd.dot(side).asin();
    let offset = (v.position - start).dot(side);
    let speed = v.forward_speed().max(5.0);
    // Heading term plus a lateral term that asks for a heading back toward the line within ~1.5 s, scaled down with speed.
    (-(4.0 * heading + 0.5 * offset / speed) * (20.0 / speed).min(1.0)).clamp(-1.0, 1.0)
}

/// Launch + braking (states 1-3). Returns [0-60, 0-100, 1/4 s, 1/4 m/s, top, 60-0, 100-0]. `TRACE=1` prints the run.
pub fn harness(d: &CarData, o: &Options) -> [f32; 7] {
    let trace = std::env::var_os("TRACE").is_some();
    let mut d = d.clone();
    d.clutch_out_time = 0.05;
    d.shift_time = 0.05;
    let assists = Controls { tcs: false, abs: true, ..Default::default() };
    if trace {
        eprintln!("hubs {:?} cg_height {} front_weight {} tyre_radius {:?}", d.hubs, d.cg_height, d.front_weight, d.tyre_radius);
    }
    let mut v = new_vehicle(d, o);
    v.test_mode = true;
    if trace {
        eprintln!("cg_model {:?}", v.cg_model);
    }
    for _ in 0..(1.0 / DT) as usize {
        v.step(assists, DT, &FlatGround);
    }
    let start = v.position;
    let line = (v.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(Vec3::NEG_Z);
    let (mut clock, mut t60, mut t100, mut tq, mut vq, mut top) = (None::<f32>, f32::NAN, f32::NAN, f32::NAN, f32::NAN, 0.0f32);
    let mut t = 0.0;
    while t < 90.0 {
        let steer = hold_line(&v, start, line);
        v.step(Controls { throttle: 1.0, steer, ..assists }, DT, &FlatGround);
        t += DT;
        let s = v.forward_speed();
        top = top.max(s);
        if clock.is_none() && s > 1.0 {
            clock = Some(t - DT);
        }
        let Some(t0) = clock else { continue };
        let c = t - t0;
        if trace && c < 10.0 && (c * 2.0).fract() < DT * 2.0 {
            let w = &v.wheels;
            eprintln!(
                "t={c:4.1} v={s:5.1} gear={} rpm={:5.0} slip {:+.2} {:+.2} {:+.2} {:+.2} load {:.0} {:.0} {:.0} {:.0} pitch {:+.2}° ax {:.2} g",
                v.gear, v.rpm, w[0].slip_ratio, w[1].slip_ratio, w[2].slip_ratio, w[3].slip_ratio, w[0].load, w[1].load, w[2].load, w[3].load,
                (v.rotation * Vec3::NEG_Z).y.asin().to_degrees(), v.acceleration.dot(v.rotation * Vec3::NEG_Z) / 9.81
            );
        }
        if t60.is_nan() && s > 60.0 * MPH {
            t60 = c;
        }
        if t100.is_nan() && s > 100.0 * MPH {
            t100 = c;
        }
        if tq.is_nan() && (v.position - start).reject_from(Vec3::Y).length() > 402.336 {
            tq = c;
            vq = s;
        }
    }
    let (mut odo, mut odo100, mut odo60) = (0.0f32, f32::NAN, f32::NAN);
    let mut prev = v.forward_speed();
    let mut t = 0.0;
    while t < 30.0 {
        let p = v.position;
        v.step(Controls { brake: 1.0, ..assists }, DT, &FlatGround);
        t += DT;
        odo += (v.position - p).reject_from(Vec3::Y).length();
        let s = v.forward_speed();
        if trace && s < 30.0 && (t * 10.0).fract() < DT * 10.0 {
            let w = &v.wheels;
            eprintln!(
                "brake t={t:5.2} v={s:5.1} gear={} rpm={:5.0} ax {:+.3} g  slip {:+.2} {:+.2} {:+.2} {:+.2}  ω·r/v {:.2} {:.2} {:.2} {:.2}",
                v.gear, v.rpm, v.acceleration.dot(v.rotation * Vec3::NEG_Z) / 9.81, w[0].slip_ratio, w[1].slip_ratio, w[2].slip_ratio, w[3].slip_ratio,
                w[0].omega * v.data.tyre_radius[0] / s.max(0.1), w[1].omega * v.data.tyre_radius[0] / s.max(0.1),
                w[2].omega * v.data.tyre_radius[1] / s.max(0.1), w[3].omega * v.data.tyre_radius[1] / s.max(0.1)
            );
        }
        if prev >= 100.0 * MPH && s < 100.0 * MPH {
            odo100 = odo;
        }
        if prev >= 60.0 * MPH && s < 60.0 * MPH {
            odo60 = odo;
        }
        prev = s;
        if s < 0.1 {
            return [t60, t100, tq, vq, top, odo - odo60, odo - odo100];
        }
    }
    [t60, t100, tq, vq, top, f32::NAN, f32::NAN]
}
