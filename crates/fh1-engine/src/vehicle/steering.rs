//! Steering and driver assists other than ABS (TCS, stability helpers) (docs/HANDLING_PARITY.md §2, §6).
//!
//! The steering is FH1's "mode 0" model (default.xex 82D37598; the mode byte car+0x1A27 reads 0 while a player drives,
//! Xenia capture `corrado`): a speed -> lock table built at setup from the car's lateral grip (82D25E00), rate limits
//! with an angular acceleration (82D22A30), a "find the peak" extension at full input, and fallbacks to the plain
//! max-angle target while countersteering or when the rear slides. Verified against the live capture: target angle,
//! table values and rate limit (docs/HANDLING_PARITY.md §2).
//!
//! Sign convention inside this file is the game's: a positive steer input gives a positive angle, and in a normal
//! corner the front tyres' slip angle has the same sign as the angle. The engine's wheel `steer` and
//! `slip_angle_deg` have the opposite sign, so both are negated at the boundary.

use super::{Controls, SteeringAssist, Vehicle, GRAVITY};
use bevy::math::Vec3;

/// Speed -> lock table: 32 samples over 0..150 mph (82D25E00: car+0xB88 = 67.056 m/s, +0xB8C = 0.4623 samples per m/s).
const TABLE_LEN: usize = 32;
const TABLE_MAX_SPEED: f32 = 67.056;
const TABLE_SAMPLES_PER_MPS: f32 = 0.462_300_18;
/// The steering tuning slider scales the four angular velocities by 2^(2s - 1) (82D25C38, setup+0x2C30). MEASURED:
/// 2 (s = 1) in the Xenia capture (210 deg/s from data 105). Where that default comes from isn't traced yet.
const RATE_SLIDER_SCALE: f32 = 2.0;
/// Same rule for SteerSpeedSensitiveMaxGees with setup+0x2C34; 1 (s = 0.5) in the capture.
const MAX_GEES_SLIDER_SCALE: f32 = 1.0;
/// |input| at or above this counts as full lock (82237B50).
const FULL_INPUT: f32 = 0.976;
/// The find-peak extension stops growing once the front tyres reach this normalised slip angle (82236914).
const FIND_PEAK_FRONT_SLIP: f32 = 1.05;
/// PhysicsSettings +0xDC = NormRearSlipAngleToCountersteer (settings constructor 82D45F08 registers the key at +0xDC; FH1's
/// PhysicsSettings.ini sets **50**, the constructor default is 3.0) and +0xE8 = SteerFrictionClamp (not in the INI: the
/// constructor's 1.2). With 50 the rear-slide fallback never fires in practice (live |x_r| p99 0.69). Rear slide ->
/// countersteer fallback, front saturation clamp. `FH1_REAR_SLIDE_SLIP=<x>` overrides (3 = before 2026-10-08: past 3x the
/// rear's peak slip the fronts jumped to input x SteerMaxAngle, 42 deg into a spin, instead of the speed lock).
fn rear_slide_slip() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_REAR_SLIDE_SLIP").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0))
}
const FRONT_SATURATION_SLIP: f32 = 1.2;
/// Below this forward speed (m/s) the plain max-angle target is used (82000CE0).
const MIN_TABLE_SPEED: f32 = 2.0;
/// car+0x170C: per-part steering limit; 999 deg on the captured car, so SteerMaxAngle is what limits.
const PART_MAX_ANGLE: f32 = 999.0 * std::f32::consts::PI / 180.0;

/// Game TCS (82D365B0, ~82D36800-82D36BE8; docs/PHYSICS_PARITY.md "TCS rule DECODED"). Data_Car
/// AssistsTCSSlipDefTakeoff / Moving are 33 / 0.5 for all 176 cars. They compare with wheel+0x1F0, which the Corrado
/// capture shows is NORMALISED slip (slip ratio / peak, Worker A's read of the live field, re/xenia/brakes.py).
const TCS_TAKEOFF_SLIP: f32 = 33.0;
const TCS_MOVING_SLIP: f32 = 0.5;
/// TractionControlSpeed 30 mph: Moving target above it, Takeoff below.
const TCS_SPEED: f32 = 30.0 * 0.447_04;
/// TCSFullEffectFricDiff: the cut ramps from 0 to 1 over this much slip above the target.
const TCS_FULL_EFFECT: f32 = 0.25;
/// TCSFullSteerSlipScale FWD / RWD / AWD (PhysicsSettings.ini; drive-type mapping verified at 82D36AF8).
const TCS_FULL_STEER_SCALE: [f32; 3] = [1.2, 1.1, 2.0];

/// STM (82D2CE50, docs/ASSISTS.md): acts above StabilityManageSpeed 30 mph (x the surface's
/// TCandStabilitySpeedMultiplier, 1 on asphalt) while moving forwards. Gamedb AutoSteerOverrides: STMSlipMin / Max 1.5 / 3.0
/// (identical for every row; car+0x17C0 / +0x17C4), or AutoSteerSTMSlipMin / Max 0.4 / 0.82 with the Assisted steering.
const STM_SPEED: f32 = 30.0 * 0.447_04;
const STM_SLIP: [f32; 2] = [1.5, 3.0];
const STM_SLIP_AUTOSTEER: [f32; 2] = [0.4, 0.82];

/// FrictionTorqueMod (PhysicsSettings.ini; settings+1352.., read by 82D2DE58). Each block: CosYawAngle0, CosYawAngle1,
/// TurnScale0, StraightenScale0, TurnClamp0, StraightenClamp0, TurnScale1, StraightenScale1, TurnClamp1, StraightenClamp1
/// (clamps in deg/s² of yaw acceleration, x the yaw inertia). The two blocks = the Mass0KG / Mass1KG ends.
type TorqueModBlock = [f32; 10];
const FTM_MASS: [f32; 2] = [909.1, 1590.9];
/// WithAssists (TCS or STM on) and NoAssists hold the same values in the shipped INI.
const FTM_ASSISTS: [TorqueModBlock; 2] = [
    [0.996194, 0.961261, 1.0, 1.0, 180.0, 145.0, 0.7, 0.8, 120.0, 140.0],
    [0.996194, 0.961261, 1.0, 1.0, 150.0, 115.0, 0.7, 0.8, 100.0, 120.0],
];
const FTM_NO_ASSISTS: [TorqueModBlock; 2] = FTM_ASSISTS;
const FTM_SIMULATION: [TorqueModBlock; 2] = [[1.0, 1.0, 1.0, 1.0, 10000.0, 10000.0, 1.0, 1.0, 10000.0, 10000.0]; 2];
const FTM_ONES: TorqueModBlock = [1.0; 10];
/// Per drive type (82D21B60; a neutral block for Simulation steering): RWD and AWD are all ones.
const FTM_FWD: [TorqueModBlock; 2] = [
    [1.0, 1.0, 0.9, 1.0, 0.7778, 0.9655, 1.0, 1.0, 1.0, 1.0],
    [1.0, 1.0, 0.9, 1.0, 0.7333, 0.9565, 1.0, 1.0, 1.0, 1.0],
];
/// Speed ramp over v² (constants 820C73B0 = 64, 82240F1C = 225): the modification fades in from 8 to 15 m/s; below, the
/// clamp is 10x wider (82000DA4).
const FTM_V2: [f32; 2] = [64.0, 225.0];
const FTM_LOW_SPEED_CLAMP: f32 = 10.0;

/// `FH1_STM=0`: no stability management whatever the setting (A/B switch).
fn stm_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_STM").map_or(true, |v| v != "0"))
}

/// `FH1_FRICTION_TORQUE_MOD=0`: no FrictionTorqueMod (the tyre yaw torque reaches the body unmodified, as before).
fn friction_torque_mod_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_FRICTION_TORQUE_MOD").map_or(true, |v| v != "0"))
}

/// The car's steering constants in SI units (82D25C38 copies the Data_Car columns with these conversions).
#[derive(Debug, Clone, Copy, Default)]
struct SteerParams {
    /// SteerMaxAngle (rad), car+0xB24.
    max_angle: f32,
    /// SteerMaxAngVelTurning / Straighten / Countersteer, SteerAngVelDynFindPeak (rad/s), car+0xB2C..+0xB38.
    turning: f32,
    straighten: f32,
    countersteer: f32,
    find_peak: f32,
    /// SteerAccelTimeToMaxRate (s): the angular acceleration is rate / this, car+0xB3C.
    accel_time: f32,
    /// SteerSpeedSensitiveMaxGees, car+0xB40.
    max_gees: f32,
    /// SteerSpeedSensitiveMinMaxAngle (rad), car+0xB44.
    min_max_angle: f32,
    /// SteerSpeedSensitiveSlow / FastSpeed (m/s), FastRateScale, car+0xB48..+0xB50.
    slow: f32,
    fast: f32,
    fast_rate_scale: f32,
}

/// Steering / assist state that nothing outside the vehicle reads.
#[derive(Debug, Clone, Default)]
pub struct SteeringState {
    /// Built on the first steering update (the game builds it at car setup).
    params: Option<SteerParams>,
    table: [f32; TABLE_LEN],
    /// Road-wheel angle (rad, game sign), car+0xAD4, and its rate (rad/s), car+0xADC.
    angle: f32,
    rate: f32,
    /// Find-peak timer and extra angle, car+0xAE0 / +0xAE4.
    find_peak_timer: f32,
    find_peak_extra: f32,
}

impl Vehicle {
    fn steer_params(&self) -> SteerParams {
        let c = &self.data.steer;
        let deg = std::f32::consts::PI / 180.0;
        let mph = 0.447_04;
        SteerParams {
            max_angle: c.max_angle_deg * deg,
            turning: c.ang_vel_turning * deg * RATE_SLIDER_SCALE,
            straighten: c.ang_vel_straighten * deg * RATE_SLIDER_SCALE,
            countersteer: c.ang_vel_countersteer * deg * RATE_SLIDER_SCALE,
            find_peak: c.ang_vel_dyn_find_peak * deg * RATE_SLIDER_SCALE,
            accel_time: c.accel_time_to_max_rate.max(1e-4),
            max_gees: c.ss_max_gees * MAX_GEES_SLIDER_SCALE,
            min_max_angle: c.ss_min_max_angle_deg * deg,
            slow: c.ss_slow_mph * mph,
            fast: c.ss_fast_mph * mph,
            fast_rate_scale: c.ss_fast_rate_scale,
        }
    }

    /// Peak lateral friction coefficient at `load` N on an axle's tyres (82D0F810 on the lateral table, which has the
    /// tyre-width scale baked in).
    fn peak_lateral_mu(&self, load: f32, axle: usize) -> f32 {
        self.data.lateral.peak_mu_at(load) * self.data.tyre.width_scale[axle]
    }

    /// The car's maximum lateral acceleration (m/s²) at speed `v` (car-physics vtable +0xA0, 82D242B8): static axle
    /// weights plus downforce, left/right load transfer from the CG height over each axle's track solved in 6 passes,
    /// each wheel at the peak of its lateral curve for its load (82D0F810: linear in load between the two curves'
    /// peaks) x its wheel scale (wheel+0x3A4 = ChassisStiffness Front/RearLatFrictionScale, read live), all x the
    /// car-wide tyre scale (tyre+0xC via vtable +0x4A8) = TorqueFreeLatFrictionScale (82D12968; 1.0 in the stats harness).
    /// The width scale comes from the curve tables (HANDLING_PARITY section 8); together they reproduce the live tables
    /// of the two captured cars (the old fit used the front width on both axles).
    pub(super) fn max_lateral_accel(&self, v: f32) -> f32 {
        let d = &self.data;
        let c = &d.tyre;
        let tyre_scale = self.setup_torque_free().lat;
        let weight = d.mass * GRAVITY;
        let axle_load = [weight * d.front_weight + d.downforce_k[0] * v * v, weight * (1.0 - d.front_weight) + d.downforce_k[1] * v * v];
        let mut total = 0.0;
        for axle in 0..2 {
            let w = axle_load[axle].max(0.0);
            let track = (d.hubs[axle * 2 + 1][0] - d.hubs[axle * 2][0]).abs().max(0.5);
            let (mut inner, mut outer) = (0.5 * w, 0.5 * w);
            let (mut mu_in, mut mu_out) = (0.0, 0.0);
            for _ in 0..6 {
                mu_in = self.peak_lateral_mu(inner, axle) * c.chassis_lat[axle];
                mu_out = self.peak_lateral_mu(outer, axle) * c.chassis_lat[axle];
                let shift = (inner * mu_in + outer * mu_out) * d.cg_height / track;
                inner = (0.5 * w - shift).max(0.0);
                outer = (0.5 * w + shift).min(w);
            }
            // As the game: the last pass's coefficients times the updated loads.
            total += inner * mu_in + outer * mu_out;
        }
        total * tyre_scale / d.mass
    }

    /// Fill the speed -> lock table (82D25E00): the kinematic angle that gives MaxGees x the car's lateral grip,
    /// atan(MaxGees x wheelbase x a_max(v) / v²), clamped to [MinMaxAngle, SteerMaxAngle].
    fn build_steer_table(&mut self, p: &SteerParams) {
        let wheelbase = (self.data.hubs[2][2] - self.data.hubs[0][2]).abs().max(0.5);
        let (lo, hi) = (p.min_max_angle.min(p.max_angle), p.min_max_angle.max(p.max_angle));
        for i in 0..TABLE_LEN {
            let v = (i as f32 / TABLE_SAMPLES_PER_MPS).max(0.01);
            let a = self.max_lateral_accel(v);
            self.steering.table[i] = (p.max_gees * wheelbase * a / (v * v)).atan().clamp(lo, hi);
        }
    }

    /// Sample the lock table at `v` with a Catmull-Rom spline (82D30088), capped at `max_angle`.
    fn steer_table_at(&self, v: f32, max_angle: f32) -> f32 {
        let t = &self.steering.table;
        let last = TABLE_LEN - 1;
        let x = v.clamp(0.0, TABLE_MAX_SPEED) * TABLE_SAMPLES_PER_MPS;
        let i = (x as usize).min(last);
        let j = (i + 1).min(last);
        let p0 = if i == 0 { 2.0 * t[i] - t[j] } else { t[i - 1] };
        let p3 = if j < last { t[j + 1] } else { 2.0 * t[j] - t[i] };
        let (p1, p2, s) = (t[i], t[j], x - x.floor());
        let y = 0.5 * (2.0 * p1 + (p2 - p0) * s + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * s * s + (3.0 * p1 - p0 - 3.0 * p2 + p3) * s * s * s);
        // car+0x1714 (per-part lock scale) is 1.
        y.min(max_angle)
    }

    /// Front wheels' normalised slip angle (1 = the curve's peak), game sign.
    fn norm_slip_angle(&self, i: usize) -> f32 {
        -self.wheels[i].norm_slip_angle
    }

    /// Load-weighted mean normalised slip angle of an axle's two wheels.
    fn axle_norm_slip_angle(&self, axle: usize) -> f32 {
        let (a, b) = (axle * 2, axle * 2 + 1);
        let (la, lb) = (self.wheels[a].load.max(0.0), self.wheels[b].load.max(0.0));
        (self.norm_slip_angle(a) * la + self.norm_slip_angle(b) * lb) / (la + lb).max(1e-7)
    }

    /// Steering lock (rad) the input reaches at speed `v` at full input (the speed -> lock table, as `update_steering`
    /// uses it above 2 m/s). For the AI driver, which turns a wanted road-wheel angle into an input.
    pub fn steer_lock_at(&mut self, v: f32) -> f32 {
        let p = match self.steering.params {
            Some(p) => p,
            None => {
                let p = self.steer_params();
                self.build_steer_table(&p);
                self.steering.params = Some(p);
                p
            }
        };
        let max_angle = PART_MAX_ANGLE.min(p.max_angle);
        if v < MIN_TABLE_SPEED || self.full_lock {
            return max_angle;
        }
        self.steer_table_at(v, max_angle).clamp(p.min_max_angle.min(max_angle), p.min_max_angle.max(max_angle))
    }

    /// Current road-wheel angle (rad, + = right).
    pub fn steer_angle(&self) -> f32 {
        self.steering.angle
    }

    /// The car's maximum lateral acceleration (m/s²) at `v` with its tyres in use (82D242B8), for the AI's speed profile.
    pub fn lateral_grip(&self, v: f32) -> f32 {
        self.max_lateral_accel(v)
    }

    /// Set the front wheels' steer angles from the input (82D37598, mode 0).
    pub(super) fn update_steering(&mut self, input: Controls, dt: f32) {
        let p = match self.steering.params {
            Some(p) => p,
            None => {
                let p = self.steer_params();
                self.build_steer_table(&p);
                self.steering.params = Some(p);
                p
            }
        };
        let s = input.steer.clamp(-1.0, 1.0);
        let st = &self.steering;
        let cur = st.angle;
        let x_front = self.axle_norm_slip_angle(0);
        let x_rear = self.axle_norm_slip_angle(1);
        // Countersteering: the current angle and the front slip disagree (left wheel checked for +, right for -).
        let countersteer = if cur > 0.0 { self.norm_slip_angle(0) < 0.0 } else { self.norm_slip_angle(1) > 0.0 };
        let rear_sliding = x_rear.abs() > rear_slide_slip();
        let reversing_input = (cur > 0.0 && s <= 0.0) || (cur < 0.0 && s >= 0.0) || (cur == 0.0 && s == 0.0);
        // vtable +0x94 (82D2ADC8): min(per-part limit, SteerMaxAngle).
        let max_angle = PART_MAX_ANGLE.min(p.max_angle);
        // UNVERIFIED: the game compares a VMX-computed speed with 2 m/s; taken as the forward speed.
        let slow = self.forward_speed() < MIN_TABLE_SPEED;
        let (mut timer, mut extra, mut rate) = (st.find_peak_timer, st.find_peak_extra, st.rate);

        if self.direct_steer {
            // Traffic: target = input x the speed lock (what the driver divided by), no fallback to input x SteerMaxAngle
            // when a small correction crosses zero (that made it 4-8x stronger at speed: lane weaving).
            let lock = self.steer_lock_at(self.forward_speed().abs());
            let target = (s * lock).clamp(-max_angle, max_angle);
            let away = (cur >= 0.0 && cur < target) || (cur <= 0.0 && target < cur);
            let limit = if away { p.turning } else { p.straighten };
            rate = approach(rate, target - cur, limit, limit / p.accel_time, dt);
            timer = 0.0;
            extra = 0.0;
        } else if slow || countersteer || rear_sliding || reversing_input {
            let target = s * max_angle;
            let diff = target - cur;
            if rear_sliding {
                // Countersteer rate, reached in one tick.
                let limit = p.countersteer;
                rate = if diff.abs() <= limit * dt { diff / dt } else { limit.copysign(diff) };
            } else {
                // 82D22AE8: turning rate while the angle grows away from centre, straightening rate otherwise.
                let away = (cur >= 0.0 && cur < target) || (cur <= 0.0 && target < cur);
                let limit = if away { p.turning } else { p.straighten };
                rate = approach(rate, diff, limit, limit / p.accel_time, dt);
            }
            timer = 0.0;
            extra = 0.0;
        } else {
            let v = self.speed();
            let lock = if self.full_lock { max_angle } else { self.steer_table_at(v, max_angle) };
            let lock = lock.clamp(p.min_max_angle.min(max_angle), p.min_max_angle.max(max_angle));
            let base = s * lock;
            // Turning rate scaled with the lock so the time to full lock stays the same at any speed.
            let (lo, hi) = (p.slow.min(p.fast), p.slow.max(p.fast));
            let speed_scale = if v <= lo {
                1.0
            } else if v < hi {
                1.0 + (p.fast_rate_scale - 1.0) * (v - lo) / (hi - lo)
            } else {
                p.fast_rate_scale
            };
            let lock_ratio = if self.full_lock {
                1.0
            } else {
                self.steer_table_at(v.clamp(lo, hi), max_angle) / self.steer_table_at(p.slow, max_angle).max(1e-6)
            };
            let mut limit = lock_ratio * p.turning * speed_scale;
            let full = s.abs() >= FULL_INPUT;
            if full {
                if cur.abs() < lock - limit * dt || x_front.abs() >= FIND_PEAK_FRONT_SLIP {
                    timer = 0.0;
                } else {
                    // At the lock with the fronts below their peak: keep turning slowly to find it.
                    timer += dt;
                    limit = limit.min(p.find_peak);
                    extra += if base < 0.0 { -limit } else { limit } * dt;
                }
            } else {
                timer = 0.0;
                extra = 0.0;
            }
            let mut target = (extra + base).clamp(-max_angle, max_angle);
            // At full input the angle never unwinds.
            if full && ((s > 0.0 && target < cur) || (s < 0.0 && target > cur)) {
                target = cur;
            }
            // Front tyres past their peak in the steering direction: don't add lock beyond the current angle.
            if x_front.abs() >= FRONT_SATURATION_SLIP && cur * x_front > 0.0 {
                let m = cur.abs().max(p.min_max_angle);
                target = target.clamp(-m, m);
            }
            rate = approach(rate, target - cur, limit, limit / p.accel_time, dt);
        }

        let st = &mut self.steering;
        st.rate = rate;
        st.angle = cur + rate * dt;
        st.find_peak_timer = timer;
        st.find_peak_extra = extra;
        let angle = st.angle;
        for i in 0..2 {
            self.wheels[i].steer = -angle;
        }
    }

    /// The "TCS effective" flag car+0x1834 (82D259D8, called at the end of FrictionTorqueMod 82D2DE58): with c = unit
    /// velocity . forward, TCS is forced ON past YawMaxAngleForTCS 50 deg of body slip (or reversing), OFF between
    /// YawMinAngleForTCS 20 and 50 deg, and the player's setting below 20 deg (settings+0x848/+0x84C, cosines at +0x850/+0x854).
    /// UNVERIFIED: the vtable gate before it (+0xB4 / +0x180) is taken as "moving" (|v| > 0.5 m/s). `FH1_TCS_YAW_GATE=0` = the
    /// setting alone (before 2026-10-08).
    fn tcs_effective(&self, setting: bool) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("FH1_TCS_YAW_GATE").map_or(true, |v| v != "0")) || self.speed() < 0.5 {
            return setting;
        }
        let c = self.velocity.normalize_or_zero().dot(self.rotation * Vec3::NEG_Z);
        if c < 50f32.to_radians().cos() {
            true
        } else if c < 20f32.to_radians().cos() {
            false
        } else {
            setting
        }
    }

    /// Traction control: returns the throttle after the cut (the game's rule; off in the stats harness's test mode).
    pub(super) fn traction_control(&mut self, input: Controls, throttle: f32, fwd_speed: f32) -> f32 {
        if !self.tcs_effective(input.tcs) || self.test_mode {
            self.tcs_cut = 0.0;
            return throttle;
        }
        let target = if self.speed() > TCS_SPEED { TCS_MOVING_SLIP } else { TCS_TAKEOFF_SLIP };
        let driven = self.driven_wheels();
        let dir = if fwd_speed < 0.0 || self.gear == 0 { -1.0 } else { 1.0 };
        let n = driven.len().max(1) as f32;
        // Mean driven-wheel normalised slip (in the driving direction) and |mean normalised slip angle| (clamped to 1).
        let slip = driven.iter().map(|&i| (self.wheels[i].norm_slip * dir).max(0.0)).sum::<f32>() / n;
        let steer = (driven.iter().map(|&i| self.norm_slip_angle(i)).sum::<f32>() / n).abs().min(1.0);
        let drive = match self.data.drive_type {
            1 => 0,
            3 => 2,
            _ => 1,
        };
        // Surface multiplier (OffRoadTCSFullEffectMultiplier) is 1.
        let lower = target * (1.0 + (TCS_FULL_STEER_SCALE[drive] - 1.0) * steer);
        let cut = ((slip - lower) / TCS_FULL_EFFECT).clamp(0.0, 1.0);
        self.tcs_cut = cut;
        // UNVERIFIED: the cut (stored at +0x168C) scales the throttle by 1 - cut.
        throttle * (1.0 - cut)
    }
}

impl Vehicle {
    /// Stability management (82D2CE50 + 82D20FF8): with STM on, above 30 mph and moving forwards, x = the rear wheels' mean
    /// normalised slip angle (wheel+0x208, game sign); past STMSlipMin it brakes one diagonal by
    /// a = clamp((|x| - min) / (max - min), 0, 1): x > 0 adds a to wheels 0 and 3 (FL, RR) and takes a/2 off 1 and 2,
    /// x < 0 the mirror. In a slide that is the outside front plus the inside rear. Sets `stm_active` (car+0x1698).
    pub(super) fn stability_management(&mut self) {
        self.stm_brake = [0.0; 4];
        self.stm_active = false;
        if !self.assists.stm || !stm_enabled() || self.test_mode {
            return;
        }
        let fwd = self.rotation * Vec3::NEG_Z;
        if self.velocity.length_squared() < STM_SPEED * STM_SPEED || self.velocity.dot(fwd) < 0.0 {
            return;
        }
        let [lo, hi] = if self.assists.steering == SteeringAssist::Assisted { STM_SLIP_AUTOSTEER } else { STM_SLIP };
        let x = 0.5 * (self.norm_slip_angle(2) + self.norm_slip_angle(3));
        if x.abs() < lo {
            return;
        }
        let a = ((x.abs() - lo) / (hi - lo).max(1e-6)).clamp(0.0, 1.0);
        let (on, off) = if x > 0.0 { ([0, 3], [1, 2]) } else { ([1, 2], [0, 3]) };
        for i in on {
            self.stm_brake[i] = a;
        }
        for i in off {
            self.stm_brake[i] = -0.5 * a;
        }
        self.stm_active = true;
    }

    /// STM's per-wheel brake input this tick (brakes.rs adds it to the pedal, clamped to 0..1).
    pub(super) fn stm_brake(&self) -> [f32; 4] {
        self.stm_brake
    }

    /// FrictionTorqueMod (82D2DE58): takes the tyres' chassis torque and returns it with its yaw part (about the body up
    /// axis) modified. turning = yaw rate and yaw torque share a sign. w = 0 below the CosYawAngle0 body slip (5.0 deg), 1
    /// past CosYawAngle1 (16.0 deg); scale = lerp(Turn/StraightenScale0, ..1, w), clamp = lerp(..Clamp0, ..Clamp1, w) deg/s²
    /// x yaw inertia (car+0x104, live Corrado 23.29 = 2,329 kg·m²); both blended in by the speed ramp s (v² 64 -> 225):
    /// T' = T x (1 + (scale - 1)·s), clamped to ±lerp(10·C, C, s). Params lerp by mass between the two blocks, x the
    /// drive-type modifiers. Live check (Corrado, off-road frame): car+0x16B8 / +0x16B4 = 16.85 / 14.04 = the OffRoad
    /// TurnScale 1.2. Off-road modifiers are not applied here (asphalt, like the tyre code).
    pub(super) fn friction_torque_mod(&self, torque: Vec3) -> Vec3 {
        if !friction_torque_mod_enabled() {
            return torque;
        }
        let sim = self.assists.steering == SteeringAssist::Simulation;
        let base = if sim {
            FTM_SIMULATION
        } else if self.assists.stm {
            FTM_ASSISTS
        } else {
            FTM_NO_ASSISTS
        };
        let modif = if !sim && self.data.drive_type == 1 { FTM_FWD } else { [FTM_ONES; 2] };
        let t = ((self.data.mass - FTM_MASS[0]) / (FTM_MASS[1] - FTM_MASS[0])).clamp(0.0, 1.0);
        let p: [f32; 10] = std::array::from_fn(|i| {
            let (a, b) = (base[0][i] * modif[0][i], base[1][i] * modif[1][i]);
            a + (b - a) * t
        });
        let up = self.rotation * Vec3::Y;
        let fwd = self.rotation * Vec3::NEG_Z;
        let yaw_torque = torque.dot(up);
        let yaw_rate = self.angular_velocity.dot(up);
        let turning = yaw_rate * yaw_torque > 0.0;
        let v2 = self.velocity.length_squared();
        let s = ((v2 - FTM_V2[0]) / (FTM_V2[1] - FTM_V2[0])).clamp(0.0, 1.0);
        let cos = self.velocity.normalize_or_zero().dot(fwd);
        let (lo, hi) = (p[0].min(p[1]), p[0].max(p[1]));
        let w = if cos >= hi {
            0.0
        } else if cos <= lo {
            1.0
        } else {
            1.0 - (cos - lo) / (hi - lo)
        };
        let (scale, clamp) = if turning {
            (p[2] + (p[6] - p[2]) * w, p[4] + (p[8] - p[4]) * w)
        } else {
            (p[3] + (p[7] - p[3]) * w, p[5] + (p[9] - p[5]) * w)
        };
        let c = clamp.to_radians() * self.inertia.y;
        let limit = FTM_LOW_SPEED_CLAMP * c + (c - FTM_LOW_SPEED_CLAMP * c) * s;
        let modified = (yaw_torque * (1.0 + (scale - 1.0) * s)).clamp(-limit, limit);
        torque + up * (modified - yaw_torque)
    }
}

/// Rate-limited approach (82D22A30): the steering rate accelerates towards `limit` in the direction of `diff`
/// (reset to 0 when the direction flips) and lands exactly on the target when it is within one tick.
fn approach(rate: f32, diff: f32, limit: f32, accel: f32, dt: f32) -> f32 {
    let mut rate = if (rate > 0.0) != (diff > 0.0) { 0.0 } else { rate };
    rate = (rate + accel.copysign(if diff < 0.0 { -1.0 } else { 1.0 }) * dt).clamp(-limit, limit);
    if diff.abs() < rate.abs() * dt {
        rate = diff / dt;
    }
    rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::CarData;
    use bevy::math::Vec3;
    use std::path::PathBuf;

    fn car(name: &str) -> Option<CarData> {
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let dir = crate::data::private_assets(&data).ok()?.join("cars").join(name);
        CarData::load(&dir).ok()
    }

    /// The speed -> lock table against the live ones read from the real game (Xenia capture `corrado`, car+0xB94):
    /// the AI CHE_CamaroSS_69 (stock) and the player's VW_Corrado_95 (upgraded: brakes, +31 kg, tyres unknown).
    #[test]
    fn lock_table_matches_capture() {
        const LIVE: [(&str, [f32; 12], f32); 2] = [
            ("CHE_CamaroSS_69", [42.0, 42.0, 42.0, 42.0, 27.03, 18.08, 12.77, 9.46, 7.27, 5.75, 5.0, 5.0], 0.03),
            ("VW_Corrado_95", [42.0, 42.0, 42.0, 42.0, 27.3, 18.28, 12.92, 9.57, 7.35, 5.82, 5.0, 5.0], 0.05),
        ];
        for (name, live, tol) in LIVE {
            let Some(d) = car(name) else {
                eprintln!("skipped: run fh1setup first");
                return;
            };
            let mut v = Vehicle::new(d, Vec3::ZERO);
            v.update_steering(Controls::default(), 1.0 / 120.0);
            let ours: Vec<f32> = v.steering.table[..live.len()].iter().map(|a| a.to_degrees()).collect();
            eprintln!("{name}
 ours {ours:.2?}
 live {live:.2?}");
            for (o, l) in ours.iter().zip(live) {
                assert!((o - l).abs() <= tol * l, "{name}: ours {o:.2} vs live {l:.2}");
            }
        }
    }

    /// Step steer at speed: full lock for 3 s, then centred. The angle stays within the lock, settles, and the car
    /// stays upright (no steering oscillation from the rate/find-peak logic).
    #[test]
    fn step_steer_is_stable() {
        let Some(d) = car("ALF_8C_08") else {
            eprintln!("skipped: run fh1setup first");
            return;
        };
        let max = d.steer.max_angle_deg.to_radians();
        let mut v = Vehicle::new(d, Vec3::ZERO);
        let dt = 1.0 / 120.0;
        for _ in 0..120 {
            v.step(Controls::default(), dt, &super::super::FlatGround);
        }
        v.velocity = v.rotation * Vec3::NEG_Z * 25.0;
        for w in &mut v.wheels {
            w.omega = 25.0 / v.data.tyre_radius[0];
        }
        let mut t = 0.0;
        while t < 6.0 {
            let steer = if t < 3.0 { 1.0 } else { 0.0 };
            v.step(Controls { steer, throttle: 0.3, ..Default::default() }, dt, &super::super::FlatGround);
            t += dt;
            let a = v.steering.angle;
            assert!(a.is_finite() && a.abs() <= max + 1e-4, "angle {a}");
            if (t * 4.0).fract() < dt * 4.0 {
                eprintln!(
                    "t={t:4.2} v={:5.1} angle={:+6.2} deg  lat g={:+.2}  xf={:+.2} xr={:+.2}",
                    v.speed(),
                    a.to_degrees(),
                    v.acceleration.dot(v.rotation * Vec3::X) / GRAVITY,
                    v.axle_norm_slip_angle(0),
                    v.axle_norm_slip_angle(1)
                );
            }
        }
        assert!((v.rotation * Vec3::Y).y > 0.9, "car should stay upright");
        assert!(v.steering.angle.abs() < 0.5f32.to_radians(), "angle should return to centre");
    }
}
