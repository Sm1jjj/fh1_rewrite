//! The AI driver: racing line + speed profile + other cars -> [`Controls`] each physics tick (docs/AI.md "Driver").
//!
//! - Line: the route's .owt line with the offsets limited by the curb margins (AIRacing.xml RaceTableReader) and a small
//!   per-driver shift (ChiVariance 0.1).
//! - Steering: pure pursuit on the target path (the line, or a lateral offset while passing) with a speed-dependent
//!   look-ahead that grows up to x4 while sliding (PredictiveControllerRLH LookAheadSlideLerp), plus yaw-rate damping; the
//!   wanted road-wheel angle becomes an input through the car's own speed -> lock table (as the game's steering
//!   controller 0x82B8B698 divides by a lock-table value), so the AI drives through the player's steering model.
//! - Speed: the profile's v_max a little ahead; the game's throttle states (0x82B89008): error > 1 m/s full throttle,
//!   |error| <= 1 proportional, error < -1 brake; lift while the fronts are past their peak.
//! - Skill (AISkills, 0x82B90EE8 VERIFIED): braking / cornering drawn uniformly in [Min, Max], re-drawn at the start and
//!   whenever the gap to the player crosses StartRubberbanding, StartTorqueCut, -DistBehindForTorqueBoost or 0; further
//!   behind than DistBehindForMaxPerformance -> the Max values. Times the built-in 0.94 margins.
//! - Rubber band: torque cut while ahead (0x82B93A18), torque boost while far behind, catch-up with hysteresis + gap per
//!   position (0x82B84610).
//! - Traffic (AITemperaments): pass on the side with room when the time to impact drops under StartPassAtImpactTime,
//!   keeping CarClearance (+ ExtraCarWidthAI 0.2) / TrackEdgeClearance; otherwise follow at TrailingDistance (our rule:
//!   the game's DLS line search is not ported).
//! - Recovery: the game's reset rules (MetaAIDriver ResetParameters, 0x82B84808) plus our quicker reverse-out when stuck
//!   against something and a reset after 6 s far off the route.

use std::collections::VecDeque;
use std::sync::Arc;

use bevy::math::Vec3;

use super::line::{Projection, RacingLine};
use super::profile::{CarLimits, SpeedProfile};
use super::tables::DriverParams;
use crate::vehicle::{Controls, Vehicle};

/// The game's built-in braking / cornering margins on top of the skill factors (VERIFIED).
const MARGIN: f32 = 0.94;
/// AIRacing.xml (default set).
const CURB_MARGIN_INNER: f32 = 2.0;
const CURB_MARGIN_OUTER: f32 = -1.0;
const CHI_VARIANCE: f32 = 0.1;
const EXTRA_CAR_WIDTH_AI: f32 = 0.2;
const LOOK_SLIDE_MAX: f32 = 4.0;
const LOOK_SLIDE_MIN_DOT: f32 = 0.98;
const RESET_FELL_BELOW: f32 = -50.0;
const RESET_UPSIDE_DOWN_S: f32 = 3.0;
const RESET_SLOW_S: f32 = 25.0;
const RESET_CANCEL_MOVED_M: f32 = 20.0;

/// Our grip correction: the fraction of `Vehicle::lateral_grip` a skill factor of 1.0 stands for. The game's top skills
/// draw cornering up to 1.25 (x 0.94), i.e. more than the car's steady-state grip in our sim; with 0.8 the top skill uses
/// ~94% of it. INFERRED (the game's CCarDynamics accel functions 0x82D244B8.. aren't decoded). FH1_AI_GRIP_BASE overrides.
fn grip_base() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_AI_GRIP_BASE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0))
}

/// FH1_AI_TCS=0: AI cars drive without traction control (AISkills OfflineTractionAssistFactor is 0 in every row).
fn ai_tcs() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_AI_TCS").map_or(true, |v| v != "0"))
}

/// FH1_AI_GEARS=0: AI cars use the car's automatic instead of the AI gearbox.
fn ai_gears() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_AI_GEARS").map_or(true, |v| v != "0"))
}

/// The game's rev limit, (RedlineRPM + TorqueCurveMaxRPM) / 2 (vehicle/drivetrain.rs).
pub fn rev_limit(d: &crate::data::CarData) -> f32 {
    0.5 * (d.redline_rpm + d.torque_curve_max_rpm)
}

/// Engine rpm in forward gear `gear` (1..) at road speed `speed`.
pub fn road_rpm(v: &Vehicle, gear: usize, speed: f32) -> f32 {
    let d = &v.data;
    let r = 0.5 * (d.tyre_radius[0] + d.tyre_radius[1]);
    speed.max(0.0) / r * d.gears[gear - 1] * d.final_drive * 30.0 / std::f32::consts::PI
}

/// Full-throttle wheel force (N, before losses) in forward gear `gear` at `speed`; 0 past the rev limit.
pub fn gear_force(v: &Vehicle, gear: usize, speed: f32) -> f32 {
    let d = &v.data;
    let rpm = road_rpm(v, gear, speed);
    if rpm > 0.985 * rev_limit(d) {
        return 0.0;
    }
    let r = 0.5 * (d.tyre_radius[0] + d.tyre_radius[1]);
    d.boosted_torque_at(rpm.max(d.idle_rpm.max(1000.0))) * d.gears[gear - 1] * d.final_drive / r
}

/// The forward gear with the most wheel force at `speed` (the top gear when every gear is past the limit).
pub fn best_gear(v: &Vehicle, speed: f32) -> usize {
    let n = v.data.gears.len().max(1);
    let mut best = (n, 0.0f32);
    for g in 1..=n {
        let f = gear_force(v, g, speed);
        if f > best.1 * 1.0001 {
            best = (g, f);
        }
    }
    best.0
}

/// Another car, as the driver sees it.
#[derive(Debug, Clone, Copy)]
pub struct Obstacle {
    pub position: Vec3,
    pub velocity: Vec3,
    pub forward: Vec3,
    pub half_length: f32,
    pub half_width: f32,
}

impl Obstacle {
    pub fn of(v: &Vehicle) -> Self {
        let [a, b] = v.data.bbox;
        Self {
            position: v.position,
            velocity: v.velocity,
            forward: v.rotation * Vec3::NEG_Z,
            half_length: 0.5 * (b.z - a.z).abs().max(3.0),
            half_width: 0.5 * (b.x - a.x).abs().max(1.5),
        }
    }
}

/// What the race tells the driver this tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Situation<'a> {
    /// Hold the brakes (grid / countdown).
    pub hold: bool,
    /// Past the finish: cruise.
    pub finished: bool,
    /// The player's progress along this line (m, laps included), for skill and rubber band; None = solo.
    pub player_progress: Option<f64>,
    /// Race positions ahead of the player (+) or behind (-) this car is, for the catch-up gap per position.
    pub positions_from_player: i32,
    pub obstacles: &'a [Obstacle],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Drive,
    Reverse,
}

/// Small deterministic RNG (xorshift32).
#[derive(Debug, Clone, Copy)]
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
}

pub struct Driver {
    pub line: Arc<RacingLine>,
    /// Profiles at the skill's minimum and maximum braking/cornering (the draw interpolates between them).
    pub profile_min: SpeedProfile,
    pub profile_max: SpeedProfile,
    pub params: DriverParams,
    /// Projection of the car on the line this tick.
    pub proj: Projection,
    /// Distance driven along the route (m), laps included.
    pub progress: f64,
    pub lap: u32,
    /// Current skill draw between the minimum (0) and maximum (1).
    pub performance: f32,
    draw: f32,
    gap_zone: i8,
    /// Target lateral position (m left of the road centre) while passing; None = the racing line.
    pub pass_lateral: Option<f32>,
    lateral_now: Option<f32>,
    mode: Mode,
    mode_timer: f32,
    stuck_timer: f32,
    stuck_attempts: u32,
    off_timer: f32,
    flip_timer: f32,
    slow_timer: f32,
    /// (time, progress, position) samples every 0.5 s over the last 36 s, for the game's stuck rules.
    history: VecDeque<(f32, f64, Vec3)>,
    clock: f32,
    was_off: bool,
    catching_up: bool,
    rng: Rng,
    /// Count of resets and of excursions off the road (|lateral| > half width + 1 m), for tests and debugging.
    pub resets: u32,
    pub off_track: u32,
    /// Target speed and the controls of the last tick (debug / HUD).
    pub target_speed: f32,
    pub last: Controls,
    /// Assist mode (the player's car, ai/assist.rs): never resets or reverses the car.
    pub assist_only: bool,
    /// Seconds since the last gear request (AI gearbox).
    shift_wait: f32,
    /// Seconds after the hold ends during which the stuck / reverse-out rule stays off.
    launch_grace: f32,
}

/// The driver's decision for one tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Decision {
    pub controls: Controls,
    /// Engine torque multiplier (rubber band); copy into `Vehicle::torque_mult`.
    pub torque_mult: f32,
    /// The car was put back on the line this tick.
    pub reset: bool,
}

impl Driver {
    /// A driver for `v` on the route `line` (the raw .owt line), starting where the car is. `seed` varies the draws.
    pub fn new(line: &RacingLine, v: &Vehicle, params: DriverParams, seed: u32) -> Self {
        let mut rng = Rng(seed.wrapping_mul(2_654_435_761).max(1) ^ 0x9E37_79B9);
        let shift = (rng.next() * 2.0 - 1.0) * CHI_VARIANCE;
        let line = Arc::new(line.limited(CURB_MARGIN_INNER, CURB_MARGIN_OUTER, shift));
        let limits = CarLimits::new(v);
        let k = grip_base() * MARGIN;
        let sk = params.skill;
        let profile_min = SpeedProfile::compute(&line, &limits, sk.cornering[0] * k, sk.braking[0] * k);
        let profile_max = SpeedProfile::compute(&line, &limits, sk.cornering[1] * k, sk.braking[1] * k);
        let proj = line.project(v.position, None);
        let draw = rng.next();
        Self {
            progress: proj.s as f64,
            proj,
            line,
            profile_min,
            profile_max,
            params,
            lap: 0,
            performance: draw,
            draw,
            gap_zone: i8::MIN,
            pass_lateral: None,
            lateral_now: None,
            mode: Mode::Drive,
            mode_timer: 0.0,
            stuck_timer: 0.0,
            stuck_attempts: 0,
            off_timer: 0.0,
            flip_timer: 0.0,
            slow_timer: 0.0,
            history: VecDeque::new(),
            clock: 0.0,
            was_off: false,
            catching_up: false,
            rng,
            resets: 0,
            off_track: 0,
            target_speed: 0.0,
            last: Controls::default(),
            assist_only: false,
            shift_wait: 0.0,
            launch_grace: 0.0,
        }
    }

    /// Profile speed at `s` for the current performance.
    pub fn v_max_at(&self, s: f32) -> f32 {
        let a = self.profile_min.v_max_at(&self.line, s);
        let b = self.profile_max.v_max_at(&self.line, s);
        a + (b - a) * self.performance
    }

    fn track_progress(&mut self, v: &Vehicle) {
        let old_s = self.proj.s;
        let p = self.line.project(v.position, Some(self.proj.index));
        // Lost the line (teleport, long excursion): search the whole route.
        let p = if p.distance > 40.0 { self.line.project(v.position, None) } else { p };
        let len = self.line.length;
        let mut ds = p.s - old_s;
        if self.line.closed {
            if ds < -0.5 * len {
                ds += len;
                self.lap += 1;
            } else if ds > 0.5 * len {
                ds -= len;
                self.lap = self.lap.saturating_sub(1);
            }
        }
        self.progress += ds as f64;
        self.proj = p;
    }

    /// Skill draw + rubber band for this tick; returns the torque multiplier.
    fn skill_and_rubberband(&mut self, sit: &Situation) -> f32 {
        let sk = self.params.skill;
        let rb = self.params.rubberband;
        let Some(pp) = sit.player_progress else {
            self.performance = self.draw;
            return sk.nominal_torque_scale.max(0.1);
        };
        // + = the AI is behind the player.
        let gap = (pp - self.progress) as f32;
        let ahead = -gap;
        // Re-draw when the gap crosses a threshold (0x82B90EE8).
        let zone = [rb.start_rubberbanding, rb.start_torque_cut, 0.0, -sk.dist_behind_for_torque_boost]
            .iter()
            .filter(|&&t| ahead > t)
            .count() as i8;
        if zone != self.gap_zone {
            self.gap_zone = zone;
            self.draw = self.rng.next();
        }
        self.performance = if gap > sk.dist_behind_for_max { 1.0 } else { self.draw };
        let mut torque = sk.nominal_torque_scale.max(0.1);
        // Torque cut while ahead (0x82B93A18).
        if rb.max_torque_cut > rb.start_torque_cut {
            let cut = ((ahead - rb.start_torque_cut) / (rb.max_torque_cut - rb.start_torque_cut)).clamp(0.0, 1.0) * rb.torque_cut_factor;
            torque *= 1.0 - cut;
        }
        // Torque boost while behind (FarBehindTorqueBoost beyond DistBehindForTorqueBoost, lerping back to 1 at 0).
        if gap > 0.0 && sk.far_behind_torque_boost != 1.0 {
            let f = if sk.dist_behind_for_torque_boost > 0.0 { (gap / sk.dist_behind_for_torque_boost).min(1.0) } else { 1.0 };
            torque *= 1.0 + (sk.far_behind_torque_boost - 1.0) * f;
        }
        // Catch-up with hysteresis and the per-position allowance (0x82B84610). What the game does with CatchUpFactor
        // isn't traced: our rule = up to +30% torque x the factor.
        let allowance = rb.catch_up_gap_per_position * sit.positions_from_player.max(0) as f32;
        if gap > rb.start_catch_up + allowance {
            self.catching_up = true;
        } else if gap < rb.stop_catch_up + allowance {
            self.catching_up = false;
        }
        if self.catching_up {
            torque *= 1.0 + 0.3 * rb.catch_up_factor;
        }
        torque
    }

    /// The game's reset rules (MetaAIDriver ResetParameters) + ours (far off the route 6 s, fallen 15 m below it, stuck
    /// after 3 reverse attempts).
    fn needs_reset(&mut self, v: &Vehicle, dt: f32) -> bool {
        self.clock += dt;
        let p = self.proj;
        let speed = v.speed();
        let up = v.rotation * Vec3::Y;
        let grounded = v.wheels.iter().filter(|w| w.grounded).count();
        self.flip_timer = if up.y < 0.3 && speed < 3.0 && grounded <= 2 { self.flip_timer + dt } else { 0.0 };
        self.slow_timer = if speed < 3.0 { self.slow_timer + dt } else { 0.0 };
        self.off_timer = if p.distance > p.half_width + 12.0 { self.off_timer + dt } else { 0.0 };
        if self.history.back().is_none_or(|h| self.clock - h.0 >= 0.5) {
            self.history.push_back((self.clock, self.progress, v.position));
            while self.history.front().is_some_and(|h| self.clock - h.0 > 36.0) {
                self.history.pop_front();
            }
        }
        // Waypoints are ~2 m: fewer than 1 / 4 / 8 waypoints in 25 / 30 / 35 s; within 2 m of one spot for 25 s.
        let stuck = [(25.0, 2.0), (30.0, 8.0), (35.0, 16.0)].iter().any(|&(t, d)| {
            let Some(h) = self.history.iter().find(|h| self.clock - h.0 <= t + 0.25) else { return false };
            self.clock - h.0 >= t - 0.25 && (self.progress - h.1).abs() < d
        });
        let parked = self.history.iter().find(|h| self.clock - h.0 <= 25.25).is_some_and(|h| self.clock - h.0 >= 24.75 && h.2.distance(v.position) < 2.0);
        // Moving 20 m cancels the waypoint rules (the history window is the reference).
        let moved = self.history.front().is_some_and(|h| h.2.distance(v.position) > RESET_CANCEL_MOVED_M) && self.clock < 25.0;
        let fallen = v.position.y < RESET_FELL_BELOW || v.position.y < self.line.point_at(p.s).y - 15.0;
        (!moved && (stuck || parked))
            || self.slow_timer > RESET_SLOW_S
            || self.flip_timer > RESET_UPSIDE_DOWN_S
            || fallen
            || self.off_timer > 6.0
            || self.stuck_attempts >= 3
    }

    pub fn update(&mut self, v: &mut Vehicle, sit: Situation, dt: f32) -> Decision {
        self.track_progress(v);
        let speed = v.forward_speed();
        let p = self.proj;
        let mut out = Decision { torque_mult: self.skill_and_rubberband(&sit), ..Default::default() };
        let mut c = Controls { tcs: ai_tcs(), abs: true, ..Default::default() };

        if sit.hold {
            // Grid / countdown: brake + handbrake in 1st on the manual box. On the automatic, brake at a standstill selects
            // reverse after 0.3 s and swaps the pedals, so the held brake drove the AI backwards (user bug, R3).
            c.brake = 1.0;
            c.handbrake = 1.0;
            v.assists.shifting = crate::vehicle::Shifting::Manual;
            if v.gear != 1 {
                v.shift_request = if v.gear == 0 { 1 } else { -1 };
            }
            self.mode = Mode::Drive;
            self.launch_grace = 3.0;
            self.stuck_timer = 0.0;
            self.slow_timer = 0.0;
            self.history.clear();
            self.last = c;
            out.controls = c;
            return out;
        }

        // ---- recovery ----
        let off_road = p.lateral.abs() > p.half_width + 1.0;
        if off_road && !self.was_off {
            self.off_track += 1;
        }
        self.was_off = off_road;
        if !self.assist_only && self.needs_reset(v, dt) {
            self.reset_on_line(v);
            out.reset = true;
            out.controls = Controls { tcs: true, abs: true, ..Default::default() };
            self.last = out.controls;
            return out;
        }

        // ---- traffic: pass / follow ----
        let tangent = self.line.tangent_at(p.s);
        let (_, lat_vec, line_off) = self.line.road_at(p.s);
        let hw = lat_vec.length().max(1.5);
        let lat_unit = lat_vec / hw;
        let line_lat = line_off * hw;
        let my_half_width = Obstacle::of(v).half_width + EXTRA_CAR_WIDTH_AI * 0.5;
        let tm = self.params.temperament;
        let mut follow_speed = f32::MAX;
        let mut want_lateral: Option<f32> = None;
        let path_lat = self.lateral_now.unwrap_or(line_lat);
        let edge = (hw - tm.track_edge_clearance - my_half_width).max(0.0);
        for o in sit.obstacles {
            let rel = o.position - v.position;
            let along = rel.dot(tangent);
            if !(-8.0..=80.0).contains(&along) {
                continue;
            }
            let o_lat = p.lateral + rel.dot(lat_unit);
            let o_speed = o.velocity.dot(tangent);
            let need = my_half_width + o.half_width + tm.car_clearance;
            let gap_len = along - o.half_length - 2.2;
            if along > 0.0 {
                let closing = speed - o_speed;
                let tti = if closing > 0.1 { gap_len.max(0.0) / closing } else { f32::MAX };
                let in_path = (o_lat - path_lat).abs() < need;
                if in_path && (tti < tm.start_pass_at_impact_time || gap_len < tm.trailing_distance) {
                    // Pass on the side with room, the one nearer the current path first; else follow.
                    let (left, right) = (o_lat + need, o_lat - need);
                    let pick = match (left <= edge, right >= -edge) {
                        (true, true) => Some(if (left - path_lat).abs() < (right - path_lat).abs() { left } else { right }),
                        (true, false) => Some(left),
                        (false, true) => Some(right),
                        _ => None,
                    };
                    match pick {
                        Some(l) => want_lateral = Some(want_lateral.map_or(l, |w: f32| if (w - path_lat).abs() > (l - path_lat).abs() { w } else { l })),
                        None => follow_speed = follow_speed.min(o_speed + (gap_len - tm.trailing_distance) * 0.6),
                    }
                }
            } else if (o_lat - path_lat).abs() < need {
                // Alongside: step away from it, inside the road.
                let l = if o_lat > path_lat { (o_lat - need).max(-edge) } else { (o_lat + need).min(edge) };
                want_lateral.get_or_insert(l);
            }
        }
        self.pass_lateral = want_lateral;
        // Ease the path's lateral position towards the target (passing) or back to the racing line, 2 m/s.
        let goal = want_lateral.unwrap_or(line_lat);
        let cur = self.lateral_now.unwrap_or(line_lat);
        let step = 2.0 * dt;
        let next = cur + (goal - cur).clamp(-step, step);
        self.lateral_now = if want_lateral.is_none() && (next - line_lat).abs() < 0.05 { None } else { Some(next) };

        // ---- steering: pure pursuit + yaw damping ----
        let fwd = v.rotation * Vec3::NEG_Z;
        let right = v.rotation * Vec3::X;
        let vel_dot = if v.speed() > 3.0 { v.velocity.normalize().dot(fwd) } else { 1.0 };
        let slide = if vel_dot < LOOK_SLIDE_MIN_DOT { (1.0 + (LOOK_SLIDE_MAX - 1.0) * ((LOOK_SLIDE_MIN_DOT - vel_dot) / 0.2).min(1.0)).min(LOOK_SLIDE_MAX) } else { 1.0 };
        let look = (5.0 + 0.5 * speed.abs()).clamp(6.0, 45.0) * slide.min(1.5);
        let ts = p.s + look;
        let target = match self.lateral_now {
            None => self.line.point_at(ts),
            Some(l) => {
                // Keep the offset from the racing line, so the path stays line-shaped while passing.
                let (_, lv, lo) = self.line.road_at(ts);
                self.line.point_with_lateral(ts, lo * lv.length() + (l - line_lat))
            }
        };
        let to = target - v.position;
        let (x, z) = (to.dot(right), to.dot(fwd));
        let alpha = x.atan2(z.max(0.1));
        let ld = (x * x + z * z).sqrt().max(1.0);
        let wb = (v.data.hubs[2][2] - v.data.hubs[0][2]).abs().max(1.5);
        let mut delta = (2.0 * wb * alpha.sin() / ld).atan();
        // Yaw damping: desired yaw rate of the pursuit arc (+ = left in engine space; steering + = right).
        let yaw_des = -speed * 2.0 * alpha.sin() / ld;
        delta += 0.06 * (v.angular_velocity.y - yaw_des);
        let lock = v.steer_lock_at(speed.abs()).max(0.05);
        c.steer = (delta / lock).clamp(-1.0, 1.0);

        // ---- speed (game throttle states) ----
        let mut v_target = self.v_max_at(p.s + speed.max(0.0) * 0.3).min(follow_speed.max(0.0));
        if sit.finished {
            v_target = v_target.min(22.0);
        }
        if off_road {
            v_target = v_target.min(18.0);
        }
        self.target_speed = v_target;
        let e = v_target - speed;
        if e > 1.0 {
            c.throttle = 1.0;
        } else if e >= -1.0 {
            c.throttle = (0.5 + 0.5 * e).clamp(0.0, 1.0);
        } else {
            c.brake = (-e * 0.25).clamp(0.3, 1.0);
        }
        // Fronts past their peak (understeer): lift.
        let front_slip = 0.5 * (v.wheels[0].norm_slip_angle.abs() + v.wheels[1].norm_slip_angle.abs());
        if front_slip > 0.95 && speed > 10.0 {
            c.throttle = c.throttle.min(0.2);
        }

        // ---- stuck against something: reverse out (our rule) ----
        self.launch_grace = (self.launch_grace - dt).max(0.0);
        match self.mode {
            _ if self.assist_only => {}
            Mode::Drive if self.launch_grace > 0.0 => self.stuck_timer = 0.0,
            Mode::Drive => {
                if speed.abs() < 1.5 && c.throttle > 0.3 {
                    self.stuck_timer += dt;
                } else if speed > 4.0 {
                    self.stuck_timer = 0.0;
                    self.stuck_attempts = 0;
                }
                if self.stuck_timer > 2.0 {
                    self.mode = Mode::Reverse;
                    self.mode_timer = 0.0;
                    self.stuck_timer = 0.0;
                    self.stuck_attempts += 1;
                }
            }
            Mode::Reverse => {
                self.mode_timer += dt;
                // Brake = reverse once stopped (Vehicle::select_direction); steer the other way to turn towards the line.
                c = Controls { brake: 1.0, throttle: 0.0, steer: -c.steer.signum() * 0.8, tcs: true, abs: true, handbrake: 0.0 };
                if self.mode_timer > 1.8 {
                    self.mode = Mode::Drive;
                }
            }
        }
        if !self.assist_only {
            self.gearbox(v, dt);
        }
        self.last = c;
        out.controls = c;
        out
    }

    /// The AI's gearbox (our rule; the game's AI gear logic is not traced): the forward gear with the most wheel force at
    /// the current road speed (engine below the rev limit), through the car's manual shifting so it also drops gears under
    /// braking (the player's automatic only downshifts below 45% of redline, which left the AI bogged down out of every
    /// corner). Reversing uses the automatic's brake-to-reverse. FH1_AI_GEARS=0 = the car's automatic.
    fn gearbox(&mut self, v: &mut Vehicle, dt: f32) {
        use crate::vehicle::Shifting;
        self.shift_wait += dt;
        if !ai_gears() || self.mode == Mode::Reverse {
            v.assists.shifting = Shifting::Automatic;
            return;
        }
        v.assists.shifting = Shifting::Manual;
        if self.shift_wait < 0.35 {
            return;
        }
        let want = best_gear(v, v.forward_speed());
        let request = if v.gear == 0 {
            1
        } else if want > v.gear {
            1
        } else if want < v.gear {
            // Down only when it pays (3%) or the current gear is lugging below 40% of redline.
            let d = &v.data;
            let now = gear_force(v, v.gear, v.forward_speed());
            let lower = gear_force(v, want, v.forward_speed());
            if lower > now * 1.03 || road_rpm(v, v.gear, v.forward_speed()) < 0.4 * d.redline_rpm { -1 } else { 0 }
        } else {
            0
        };
        if request != 0 {
            v.shift_request = request;
            self.shift_wait = 0.0;
        }
    }

    /// Put the car back on the racing line where it is (facing along the route, at half the target speed, <= 15 m/s).
    pub fn reset_on_line(&mut self, v: &mut Vehicle) {
        let s = self.proj.s;
        let point = self.line.point_at(s);
        let yaw = self.line.yaw_at(s);
        v.place(point, yaw);
        let speed = (0.5 * self.v_max_at(s)).min(15.0);
        v.velocity = self.line.tangent_at(s) * speed;
        for w in &mut v.wheels {
            w.omega = speed / v.data.tyre_radius[0];
        }
        self.mode = Mode::Drive;
        self.stuck_timer = 0.0;
        self.stuck_attempts = 0;
        self.off_timer = 0.0;
        self.flip_timer = 0.0;
        self.slow_timer = 0.0;
        self.history.clear();
        self.lateral_now = None;
        self.resets += 1;
    }
}
