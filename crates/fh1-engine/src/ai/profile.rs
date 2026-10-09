//! Target speed along a racing line for one car (docs/AI.md "Speed profile"). The game stores no speeds in its lines;
//! this is the standard quasi-steady-state profile from the car's own numbers:
//! - corner limit: v²·|κ| = cornering x a_lat(v), with a_lat(v) = the car's maximum lateral acceleration including
//!   downforce (the same function the game builds its steering lock table from, 82D242B8; `Vehicle::lateral_grip`);
//! - braking (backward pass): the deceleration the tyres have left inside the friction circle x the skill's braking
//!   factor, plus drag, minus the downhill component of gravity;
//! - acceleration (forward pass, the predicted speed): best-gear engine force, limited by grip, minus drag and the uphill
//!   component. The AI targets `v_max`; `v_pred` is what the car will actually do (lap times, driving line colours).
//! Skill factors (AISkills Cornering*/Braking*) scale the grip the profile assumes; 1.0 = the car's full grip.
//!
//! The game's speed computer has the same structure (driver ctor 0x82B8F368 builds 40-bin lateral / forward / braking
//! accel tables at 2.5 m/s from the car's physics; corner speed per waypoint 0x82B7DF90 iterated to a fixed point; backward
//! braking pass 0x82475810 with the friction-circle factor sqrt(1 - v²/v_corner²); forward accel pass 0x82B7E708; target =
//! min). VERIFIED there: the lateral table carries a speed factor 1 up to 15 m/s, 1 - 0.22·((v-15)/60.1)^3.8 above, 0.78
//! from 75.1 m/s; built-in margins 0.94 on braking and cornering (applied by the driver). Not ported yet: bank / vertical
//! curvature terms of the corner solve (SpeedComputerSlopeCurvatureModifier 0.75).

use bevy::math::Vec3;

use super::line::RacingLine;
use crate::vehicle::{Vehicle, GRAVITY};

/// Speeds are capped here (m/s, ~400 km/h).
const V_CAP: f32 = 110.0;

#[derive(Debug, Clone)]
pub struct SpeedProfile {
    /// Highest speed at each point that still makes every corner ahead (corner + braking limit), m/s.
    pub v_max: Vec<f32>,
    /// Predicted speed with full acceleration out of corners (forward pass), m/s.
    pub v_pred: Vec<f32>,
    /// Corner-only limit per point (m/s), before braking: the apexes.
    pub v_corner: Vec<f32>,
    /// Predicted lap / route time at v_pred (s).
    pub time: f32,
    pub cornering: f32,
    pub braking: f32,
}

/// The car numbers the profile needs, sampled once (a_lat and drive force against speed).
pub struct CarLimits {
    mass: f32,
    drag_k: f32,
    /// (a_lat, a_drive) per 1 m/s.
    table: Vec<(f32, f32)>,
}

impl CarLimits {
    pub fn new(v: &Vehicle) -> Self {
        let d = &v.data;
        let r = 0.5 * (d.tyre_radius[0] + d.tyre_radius[1]);
        let table = (0..=V_CAP as usize)
            .map(|i| {
                let speed = i as f32;
                let a_lat = v.lateral_grip(speed.max(1.0)) * speed_factor(speed);
                // Best gear: wheel force = engine torque x ratio x final / r (10% driveline loss, our guess).
                let _ = r;
                // The AI gearbox's choice (driver.rs best_gear), 10% driveline loss (our guess).
                let f = super::driver::gear_force(v, super::driver::best_gear(v, speed), speed) * 0.9;
                (a_lat, f / d.mass)
            })
            .collect();
        Self { mass: d.mass, drag_k: d.drag_k, table }
    }

    fn at(&self, v: f32) -> (f32, f32) {
        let x = v.clamp(0.0, V_CAP);
        let i = (x as usize).min(self.table.len() - 2);
        let t = x - i as f32;
        let (a, b) = (self.table[i], self.table[i + 1]);
        (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
    }

    fn drag(&self, v: f32) -> f32 {
        self.drag_k * v * v / self.mass
    }
}

/// The game's high-speed factor on the AI's lateral accel table (see the module doc).
pub fn speed_factor(v: f32) -> f32 {
    if v <= 15.0 {
        1.0
    } else if v >= 75.1 {
        0.78
    } else {
        1.0 - 0.22 * ((v - 15.0) / 60.1).powf(3.8)
    }
}

impl SpeedProfile {
    pub fn compute(line: &RacingLine, car: &CarLimits, cornering: f32, braking: f32) -> Self {
        let n = line.len();
        // Curvature over ~16 m chords, then the tightest within ±3 points (conservative on short kinks).
        let ck: usize = std::env::var("FH1_P_CK").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
        let cw: isize = std::env::var("FH1_P_CW").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
        let raw: Vec<f32> = (0..n).map(|i| line.curvature(i, ck).abs()).collect();
        let kappa: Vec<f32> = (0..n).map(|i| (-cw..=cw).map(|k| raw[line.wrap(i as isize + k)]).fold(0.0, f32::max)).collect();
        let v_corner: Vec<f32> = kappa
            .iter()
            .map(|&k| {
                if k < 1e-4 {
                    return V_CAP;
                }
                let mut v = 30.0f32;
                for _ in 0..8 {
                    v = (cornering * car.at(v).0 / k).sqrt().min(V_CAP);
                }
                v
            })
            .collect();
        let seg = |i: usize| -> (f32, f32) {
            // Length and grade (rise / run) from i to i+1.
            let j = line.wrap(i as isize + 1);
            let d: Vec3 = line.points[j] - line.points[i];
            let len = d.length().max(0.01);
            (len, d.y / len)
        };
        // Lateral use of the friction circle at speed v on point i.
        let lat_use = |i: usize, v: f32| -> f32 {
            let a_lat = car.at(v).0 * cornering;
            ((v * v * kappa[i]) / a_lat.max(0.1)).clamp(0.0, 1.0)
        };
        let laps = if line.closed { 2 } else { 1 };
        // Backward pass (braking): v_i² <= v_{i+1}² + 2·a·ds.
        let mut v_max = v_corner.clone();
        if !line.closed {
            // Point-to-point: no corner after the finish.
            v_max[n - 1] = v_max[n - 1].min(V_CAP);
        }
        for pass in 0..laps {
            let _ = pass;
            for k in (0..n).rev() {
                let i = k;
                let j = line.wrap(i as isize + 1);
                if !line.closed && i == n - 1 {
                    continue;
                }
                let (len, grade) = seg(i);
                let vn = v_max[j];
                let (a_lat, _) = car.at(vn);
                let u = lat_use(j, vn);
                let a = braking * a_lat * (1.0 - u * u).max(0.0).sqrt() + car.drag(vn) + GRAVITY * grade;
                let v = (vn * vn + 2.0 * a.max(0.5) * len).sqrt();
                v_max[i] = v_max[i].min(v);
            }
        }
        // Forward pass (acceleration), from a standing start on open routes.
        let mut v_pred = v_max.clone();
        if !line.closed {
            v_pred[0] = 0.0;
        }
        for _ in 0..laps {
            for i in 0..n {
                let j = line.wrap(i as isize + 1);
                if !line.closed && i == n - 1 {
                    continue;
                }
                let (len, grade) = seg(i);
                let v = v_pred[i];
                let (a_lat, a_drive) = car.at(v);
                let u = lat_use(i, v);
                let a = a_drive.min(a_lat * (1.0 - u * u).max(0.0).sqrt()) - car.drag(v) - GRAVITY * grade;
                let vn = (v * v + 2.0 * a * len).max(0.0).sqrt();
                v_pred[j] = v_max[j].min(vn);
            }
        }
        let mut time = 0.0;
        for i in 0..n {
            if !line.closed && i == n - 1 {
                break;
            }
            let j = line.wrap(i as isize + 1);
            let (len, _) = seg(i);
            time += len / (0.5 * (v_pred[i] + v_pred[j])).max(1.0);
        }
        Self { v_max, v_pred, v_corner, time, cornering, braking }
    }

    /// Interpolated v_max at distance `s` along the line.
    pub fn v_max_at(&self, line: &RacingLine, s: f32) -> f32 {
        sample(line, &self.v_max, s)
    }

    pub fn v_pred_at(&self, line: &RacingLine, s: f32) -> f32 {
        sample(line, &self.v_pred, s)
    }
}

fn sample(line: &RacingLine, v: &[f32], s: f32) -> f32 {
    let s = line.wrap_s(s);
    let i = line.index_at(s);
    let j = line.wrap(i as isize + 1);
    let len = if j > i { line.s[j] - line.s[i] } else { line.length - line.s[i] };
    let t = if len > 1e-3 { ((s - line.s[i]) / len).clamp(0.0, 1.0) } else { 0.0 };
    v[i] + (v[j] - v[i]) * t
}
