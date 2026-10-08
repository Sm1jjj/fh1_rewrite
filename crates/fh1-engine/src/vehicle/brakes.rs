//! Brakes, handbrake and ABS, ported from default.xex (docs/HANDLING_PARITY.md §7).
//!
//! Brake torque is sized from the car's own longitudinal grip (82D262B0), not from the brake hardware:
//! capacity M = the car's maximum braking deceleration (load transfer + downforce, max of 80 / 200 mph), split
//! front/rear by the axle friction forces with BrakeBiasOffsetRear, x the BrakeTorqueSlider curve. Each tick
//! (82D37C70) torque = m g/4 x r x BrakeInputScale(pedal after ABS) x capacity x speedTable(v). ABS (82D365B0)
//! releases a wheel's brake for DurationABS once its slip passes the release point.

use super::{Controls, Vehicle, GRAVITY};
use std::sync::OnceLock;

/// PhysicsSettings.ini BrakeInputScale (4-point piecewise-linear curve, 82629830).
const BRAKE_INPUT_IN: [f32; 4] = [0.0, 0.001, 0.90, 1.0];
const BRAKE_INPUT_OUT: [f32; 4] = [0.0, 0.12, 0.70, 0.81];
/// PhysicsSettings.ini BrakeBiasOffsetRear (settings+0x94).
const BRAKE_BIAS_OFFSET_REAR: f32 = 0.03;
/// settings+0x90, default constructor 82D45F08 (not in the INI): brake bias clamp [1 - x, x].
const BRAKE_BIAS_CLAMP: f32 = 0.85;
/// PhysicsSettings.ini NoABS* (settings+0x98..+0xAC): the lock-up limiter with ABS off.
const NO_ABS_BRAKE_TRAVEL: f32 = 0.975;
const NO_ABS_DISABLE_TIME: f32 = 0.0;
const NO_ABS_SLIP_DEF: [[f32; 2]; 2] = [[1.0, 0.71], [1.0, 0.71]];
/// PhysicsSettings.ini ABSOffBrakingFrictionScale (settings+0x128, read by 82D24150).
const ABS_OFF_BRAKING_FRICTION_SCALE: f32 = 1.05;
/// BrakeTorqueSlider curve 0 -> 0, 0.5 -> 1.8181818, 1 -> 4.347826 (82236BB0 / 82236BB4).
const TORQUE_SLIDER: [f32; 3] = [0.0, 1.818_181_8, 4.347_826];
/// Speed table: 10 samples over 0..200 mph (82236BAC = 9 / 89.408).
const TABLE_LEN: usize = 10;
const TABLE_MAX_SPEED: f32 = 89.408;

// Per-car List_UpgradeBrakes columns: self.data.brakes (CarData::brakes). Handbrake: car+0x2C1C =
// 82D34488(slider, BiasHandbrake, TuneHandbrakePressureMin/Max 0 / 5.5), UNVERIFIED; taken as BiasHandbrake.

/// Experiment switches for the two OPEN questions (removed once settled):
/// FH1_ABS_SLIP = norm (default) | v | wr, see AbsSlip;
/// FH1_HARNESS_ABS = on | off (ABS state in Turn 10's stats harness).
#[derive(Clone, Copy, PartialEq)]
enum AbsSlip {
    /// slip ratio / longitudinal peak slip (default; Xenia: the game's wheel+0x1F0 = 4.3-4.6 x slip ratio on the Corrado)
    Norm,
    /// raw slip ratio over road speed (ABS can never fire)
    V,
    /// slip ratio over wheel surface speed when braking
    Wr,
}
fn abs_slip_mode() -> AbsSlip {
    static S: OnceLock<AbsSlip> = OnceLock::new();
    *S.get_or_init(|| match std::env::var("FH1_ABS_SLIP").as_deref() {
        Ok("v") => AbsSlip::V,
        Ok("wr") => AbsSlip::Wr,
        _ => AbsSlip::Norm,
    })
}
fn harness_abs() -> Option<bool> {
    static S: OnceLock<Option<bool>> = OnceLock::new();
    *S.get_or_init(|| std::env::var("FH1_HARNESS_ABS").ok().map(|s| s != "off"))
}

fn brake_scale() -> f32 {
    static S: OnceLock<f32> = OnceLock::new();
    *S.get_or_init(|| std::env::var("FH1_BRAKE_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0))
}

/// Per-car values computed once (82D262B0), for one ABS setting.
#[derive(Debug, Clone)]
struct BrakeSetup {
    abs: bool,
    /// Per-wheel capacity (wheel+0x374), in g: torque = m g/4 x r x this at full input, table 1.
    capacity: [f32; 4],
    /// Rear handbrake capacity (wheel+0x378), same units.
    handbrake: f32,
    /// max(0.098, F(v_i)) / M at v_i = i x 200 mph / 9.
    table: [f32; TABLE_LEN],
}

/// Brake / ABS state that nothing outside the vehicle reads.
#[derive(Debug, Clone, Default)]
pub struct BrakeState {
    setup: Option<BrakeSetup>,
    /// Per-wheel ABS pulse timer (wheel+0x20).
    timer: [f32; 4],
    /// Last per-wheel pedal after ABS (wheel+0x24).
    pub pedal: [f32; 4],
    /// Any wheel limited by ABS this tick (car+0x169C).
    pub abs_active: bool,
    /// Standstill brake-hold timer (car+0x16F8, 82D30BE8).
    hold_timer: f32,
    /// Line lock this tick (front brakes only; OUR burnout aid, `line_lock`).
    pub line_lock: bool,
}

/// `FH1_STANDSTILL_HOLD=0`: no automatic brake hold / static-friction hold at a standstill (before 2026-10-08).
pub(super) fn standstill_hold_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_STANDSTILL_HOLD").map_or(true, |v| v != "0"))
}

/// `FH1_LINE_LOCK=0`: brake + throttle brakes all four wheels as the game does (no burnouts).
fn line_lock_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_LINE_LOCK").map_or(true, |v| v != "0"))
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Piecewise-linear curve through (x[i], y[i]), clamped at both ends (82629830).
fn curve4(x: f32, xs: [f32; 4], ys: [f32; 4]) -> f32 {
    if x <= xs[0] {
        return ys[0];
    }
    for i in 1..4 {
        if x < xs[i] {
            return lerp(ys[i - 1], ys[i], (x - xs[i - 1]) / (xs[i] - xs[i - 1]));
        }
    }
    ys[3]
}

/// Catmull-Rom sample of the speed table (82D32328 -> 82CB7D60); end points extrapolated linearly.
fn sample_table(t: &[f32; TABLE_LEN], v: f32) -> f32 {
    let x = v.clamp(0.0, TABLE_MAX_SPEED) * (TABLE_LEN - 1) as f32 / TABLE_MAX_SPEED;
    let i = (x as usize).min(TABLE_LEN - 1);
    let j = (i + 1).min(TABLE_LEN - 1);
    let (p1, p2) = (t[i], t[j]);
    let p0 = if i == 0 { 2.0 * p1 - p2 } else { t[i - 1] };
    let p3 = if j < TABLE_LEN - 1 { t[j + 1] } else { 2.0 * p2 - p1 };
    let f = x - i as f32;
    0.5 * (2.0 * p1 + (p2 - p0) * f + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * f * f + (3.0 * p1 - p0 - 3.0 * p2 + p3) * f * f * f)
}

impl Vehicle {
    /// The car's maximum braking deceleration (m/s²) at speed v, and the front / rear axle friction forces (N)
    /// (car-physics vtable +0x9C, 82D244B8): static split + per-axle downforce, load transfer solved in 6 steps,
    /// each axle at the longitudinal curve's peak for its per-wheel load.
    /// x 82D24150 = car+0x17DC (GameFrictionScaleBraking) x tyre+0x10 (TorqueFreeLongFrictionScaleBrake; 1.0 in the stats
    /// harness) x ABS-off scale; per wheel x the width scale (in the curve table) and wheel+0x3A8 (chassis long scale).
    /// car+0x160 / +0x164 = mass / 100 and its inverse (Corrado live), so F is a deceleration.
    fn max_brake_decel(&self, v: f32, abs: bool) -> (f32, f32, f32) {
        let d = &self.data;
        let weight = d.mass * GRAVITY;
        let base = [weight * d.front_weight + d.downforce_k[0] * v * v, weight * (1.0 - d.front_weight) + d.downforce_k[1] * v * v];
        let total = base[0] + base[1];
        let h_over_l = d.cg_height / (d.hubs[2][2] - d.hubs[0][2]).abs().max(0.5);
        let axle = |n: f32, a: usize| n * d.longitudinal.peak_mu_at(0.5 * n) * d.tyre.width_scale[a] * d.tyre.chassis_long[a];
        let mut front = base[0];
        let (mut ff, mut fr) = (0.0, 0.0);
        for _ in 0..6 {
            ff = axle(front, 0);
            fr = axle(total - front, 1);
            front = (base[0] + (ff + fr) * h_over_l).clamp(0.0, total);
        }
        // car+0x17DC = GameFrictionScaleBraking of the fitted brakes (Xenia: upgraded Corrado 1.05, stock Camaro 1.0).
        let scale = d.brakes.game_friction_scale * self.setup_torque_free().brake * if abs { 1.0 } else { ABS_OFF_BRAKING_FRICTION_SCALE };
        ((ff + fr) * scale / d.mass, ff, fr)
    }

    fn brake_setup(&self, abs: bool) -> BrakeSetup {
        const MIN: f32 = 0.098_066_5;
        let (f80, _, _) = self.max_brake_decel(35.7632, abs);
        let (f200, ff, fr) = self.max_brake_decel(TABLE_MAX_SPEED, abs);
        let m = MIN.max(f80).max(f200);
        // UNVERIFIED which F call fills the axle forces; taken from the 200 mph one.
        let bias = (ff.max(1e-4) / (ff.max(1e-4) + fr.max(1e-4))).clamp(1.0 - BRAKE_BIAS_CLAMP, BRAKE_BIAS_CLAMP);
        let c = self.data.brakes;
        let b = (c.bias_slider - 0.5 * BRAKE_BIAS_OFFSET_REAR).clamp(0.0, 1.0);
        let s = c.torque_slider.clamp(0.0, 1.0) * 2.0;
        let slider = if s < 1.0 { lerp(TORQUE_SLIDER[0], TORQUE_SLIDER[1], s) } else { lerp(TORQUE_SLIDER[1], TORQUE_SLIDER[2], s - 1.0) };
        let front = bias * (m / GRAVITY) * 2.0 * slider * 2.0 * b;
        let rear = (1.0 - bias) * (m / GRAVITY) * 2.0 * slider * (2.0 - 2.0 * b);
        let mut table = [0.0; TABLE_LEN];
        for (i, t) in table.iter_mut().enumerate() {
            let v = i as f32 * TABLE_MAX_SPEED / (TABLE_LEN - 1) as f32;
            *t = MIN.max(self.max_brake_decel(v, abs).0) / m;
        }
        let handbrake = c.bias_handbrake * self.max_brake_decel(22.352, abs).0 / GRAVITY;
        BrakeSetup { abs, capacity: [front, front, rear, rear], handbrake, table }
    }

    /// The ABS / lock-up limiter (82D365B0): each wheel's pedal after ABS.
    fn abs_pedals(&mut self, abs: bool, steer: f32, brake: f32, dt: f32) -> [f32; 4] {
        // STM's differential braking adds to (or takes from) each wheel's brake input (82D20FF8, wheel+0x24).
        let stm = self.stm_brake();
        let lock = self.brakes.line_lock;
        // Line lock: the pedal reaches the front wheels only (the rears stay free to spin up).
        let mut b: [f32; 4] = std::array::from_fn(|i| if lock && i >= 2 { 0.0 } else { (brake + stm[i]).clamp(0.0, 1.0) });
        for t in &mut self.brakes.timer {
            *t += dt;
        }
        self.brakes.abs_active = false;
        if !(abs || b.iter().any(|&x| x >= NO_ABS_BRAKE_TRAVEL)) {
            return b;
        }
        let s = steer.abs().min(1.0);
        let (rp, duration) = if abs {
            let rp = self.data.brakes.release_point;
            ([lerp(rp[0][0], rp[0][1], s), lerp(rp[1][0], rp[1][1], s)], self.data.brakes.duration_abs)
        } else {
            ([lerp(NO_ABS_SLIP_DEF[0][0], NO_ABS_SLIP_DEF[0][1], s), lerp(NO_ABS_SLIP_DEF[1][0], NO_ABS_SLIP_DEF[1][1], s)], NO_ABS_DISABLE_TIME)
        };
        for i in 0..4 {
            let w = self.wheels[i];
            if self.brakes.timer[i] < duration {
                b[i] = 0.0;
                self.brakes.abs_active = true;
                continue;
            }
            // Grounded wheels keep the pedal unless they slip past the release point; airborne wheels are capped at a
            // level L in the game (82D36788, not ported: B unchanged).
            if b[i] <= 0.0 || !w.grounded {
                continue;
            }
            let k = w.slip_ratio;
            let slip = match abs_slip_mode() {
                AbsSlip::Norm => w.norm_slip,
                AbsSlip::V => k,
                // (wr - v) / v  ->  (wr - v) / wr, so a locked wheel goes to -inf.
                AbsSlip::Wr if k < 0.0 => k / (1.0 + k).max(1e-4),
                AbsSlip::Wr => k,
            };
            let r = rp[i / 2];
            if (slip > r && w.omega <= 0.0) || (slip < -r && w.omega >= 0.0) {
                b[i] = 0.0;
                self.brakes.timer[i] = 0.0;
                self.brakes.abs_active = true;
            }
        }
        b
    }

    /// Brake torque at each wheel this step (N·m, >= 0), including the handbrake and ABS.
    /// `brake` is the pedal after the reverse swap; wheel slip is from the previous step.
    /// Per-wheel brake input after ABS this tick (W+0x18), for probes.
    pub fn brake_pedals(&self) -> [f32; 4] {
        self.brakes.pedal
    }

    pub(super) fn brake_torques(&mut self, input: Controls, brake: f32, dt: f32) -> [f32; 4] {
        let abs = match (self.test_mode, harness_abs()) {
            (true, Some(a)) => a,
            _ => input.abs,
        };
        if self.brakes.setup.as_ref().map_or(true, |s| s.abs != abs) {
            self.brakes.setup = Some(self.brake_setup(abs));
        }
        let pedals = self.abs_pedals(abs, input.steer, brake, dt);
        self.brakes.pedal = pedals;
        let setup = self.brakes.setup.as_ref().unwrap();
        let table = sample_table(&setup.table, self.speed());
        let mg4 = self.data.mass * GRAVITY * 0.25 * brake_scale();
        let mut out = [0.0f32; 4];
        for i in 0..4 {
            // 82D37C70. Magnitude only: the step applies it inside the driveline solve with the game's smooth sign
            // clamp(-omega / pi, -1, 1) (or, with FH1_BRAKE_SOLVE=0, as the old lock clamp after the solve).
            let mut t = curve4(pedals[i], BRAKE_INPUT_IN, BRAKE_INPUT_OUT) * setup.capacity[i] * table;
            if i >= 2 {
                t += setup.handbrake * input.handbrake;
            }
            out[i] = mg4 * self.tyre_radius(i) * t;
        }
        out
    }
}

impl Vehicle {
    /// The game's automatic brake hold (gearbox update 82D30BE8, VERIFIED from code): with no throttle and no handbrake,
    /// |v|^2 < 1 and |w|^2 < 0.25, a timer runs (car+0x16F8); after 1 s the brake input becomes 1.0, at once when the car
    /// is fully stopped (|v|^2 < 0.09, |w|^2 < 0.04, every wheel below 1 rad/s). Takes the brake after the direction
    /// select (so it never selects reverse) and returns it. Not in the stats harness.
    pub(super) fn standstill_brake(&mut self, throttle: f32, brake: f32, handbrake: f32, dt: f32) -> f32 {
        if !standstill_hold_enabled() || self.test_mode {
            return brake;
        }
        let (v2, w2) = (self.velocity.length_squared(), self.angular_velocity.length_squared());
        if throttle > 1e-3 || handbrake >= 0.1 || v2 >= 1.0 || w2 >= 0.25 {
            self.brakes.hold_timer = 0.0;
            return brake;
        }
        self.brakes.hold_timer += dt;
        let stopped = v2 < 0.09 && w2 < 0.04 && self.wheels.iter().all(|w| w.omega.abs() < 1.0);
        if stopped || self.brakes.hold_timer > 1.0 { 1.0 } else { brake }
    }

    /// Line lock (OUR rule, user 2026-10-08: "hold the front brakes and keep the rears spinning like a proper burnout"; the
    /// game brakes all four wheels): throttle and brake both past 0.3 below 8 m/s in a forward gear on a RWD / AWD car.
    /// FWD cars burn out with the handbrake (rears) + throttle, which needs no rule.
    pub(super) fn update_line_lock(&mut self, throttle: f32, brake: f32) {
        self.brakes.line_lock = line_lock_enabled() && !self.test_mode && self.data.drive_type != 1 && self.gear >= 1 && throttle > 0.3 && brake > 0.3 && self.speed() < 8.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_curve() {
        assert_eq!(curve4(1.0, BRAKE_INPUT_IN, BRAKE_INPUT_OUT), 0.81);
        assert_eq!(curve4(0.0, BRAKE_INPUT_IN, BRAKE_INPUT_OUT), 0.0);
        assert!((curve4(0.45, BRAKE_INPUT_IN, BRAKE_INPUT_OUT) - (0.12 + 0.58 * (0.449 / 0.899))).abs() < 1e-5);
    }

    #[test]
    fn table_passes_through_samples() {
        let mut t = [0.0; TABLE_LEN];
        for (i, x) in t.iter_mut().enumerate() {
            *x = 1.0 + 0.1 * i as f32;
        }
        for i in 0..TABLE_LEN {
            let v = i as f32 * TABLE_MAX_SPEED / 9.0;
            assert!((sample_table(&t, v) - t[i]).abs() < 1e-4, "{i}");
        }
        assert!((sample_table(&t, 200.0) - t[9]).abs() < 1e-5);
    }
}

/// Brake torque inside the coupled wheel solve with the game's smooth sign (82D37C70), default on;
/// FH1_BRAKE_SOLVE=0 = the old lock clamp applied after the solve.
pub(super) fn brake_in_solve() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_BRAKE_SOLVE").map_or(true, |v| v != "0"))
}
