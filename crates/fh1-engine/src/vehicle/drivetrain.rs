//! Engine, clutch, gearbox and torque delivery to the driven wheels (docs/DRIVETRAIN.md).

use super::{Shifting, Vehicle};

const TO_RPM: f32 = 60.0 / std::f32::consts::TAU;

/// Drivetrain state that nothing outside the vehicle reads.
#[derive(Debug, Clone, Default)]
pub struct DrivetrainState {
    pub(super) shift_timer: f32,
    /// Engine (flywheel side of the clutch) speed, rad/s.
    pub(super) engine_omega: f32,
    /// Time since the clutch started closing for a pull-away (capacity ramps over ClutchOutTime).
    pub(super) clutch_timer: f32,
    /// Clutch stuck (engine turns with the gearbox) vs slipping.
    pub(super) clutch_locked: bool,
    /// Rev-limiter cut time left (s): throttle forced to 0 (82D2CC38, car+0xC6C / +0xC5C).
    pub(super) rev_cut: f32,
    pub(super) reverse_timer: f32,
    /// The launch (clutch slipping at the launch rpm: stats harness, or normal play `play_launch_mode`) is over.
    pub(super) launch_done: bool,
    /// Normal-play launch: seconds spent launching since it was armed (the game's car+0x16F4 blend, see `launch_target`).
    pub(super) launch_t: f32,
    /// Boost state (docs/DRIVETRAIN.md "Turbo spool", 82D2B920 / 82D20538): lagged engine power (hp, car+0xC34), boost
    /// pressure (psi, car+0xC3C / SC +0xC44), the blow-off latch (car+0xC40) and the last tick's engine power (hp, car+0xC60).
    pub(super) turbo_power: f32,
    pub(super) boost_psi: f32,
    pub(super) blowoff: bool,
    pub(super) engine_power: f32,
}

impl DrivetrainState {
    pub(super) fn new(idle_rpm: f32) -> Self {
        Self { engine_omega: idle_rpm / TO_RPM, ..Default::default() }
    }
}

impl Vehicle {
    pub(super) fn driven(&self, i: usize) -> bool {
        match self.data.drive_type {
            1 => i < 2,
            3 => true,
            _ => i >= 2,
        }
    }

    pub(super) fn driven_wheels(&self) -> Vec<usize> {
        (0..4).filter(|&i| self.driven(i)).collect()
    }

    /// For tests that start the car at speed: pick the lowest gear below 90% of redline at the current
    /// road speed and lock the clutch with the engine at the matching rpm.
    pub fn sync_drivetrain(&mut self) {
        let d = &self.data;
        let wheel_omega = self.forward_speed() / d.tyre_radius[1];
        let omega = |g: f32| wheel_omega * g * d.final_drive;
        let limit = 0.9 * d.redline_rpm / TO_RPM;
        self.gear = (1..=d.gears.len()).find(|&g| omega(d.gears[g - 1]) < limit).unwrap_or(d.gears.len());
        self.drivetrain.engine_omega = omega(d.gears[self.gear - 1]).max(d.idle_rpm / TO_RPM);
        self.drivetrain.clutch_timer = d.clutch_out_time;
        self.drivetrain.clutch_locked = true;
        self.drivetrain.launch_done = true;
    }

    fn gear_ratio(&self) -> f32 {
        if self.gear == 0 { self.data.reverse } else { self.data.gears[self.gear - 1] }
    }

    /// Automatic: reverse when stopped and holding brake; in reverse the pedals swap. Manual: reverse is a gear below 1st
    /// (shift_request) and the pedals keep their roles. Returns (throttle, brake).
    ///
    /// Dead-throttle fix (2026-10-06, user: "the throttle stops working until I press the brake"): reverse used to be left
    /// only with the throttle past HALF, and in reverse the throttle is the brake, so a gentle squeeze after a stop just
    /// braked the stopped car (the "dead engine") until the brake pedal (reverse's throttle) moved it. Now any throttle
    /// that outweighs the brake selects 1st, and reverse needs the whole car stopped (not only the forward component:
    /// a sideways slide with the brake held engaged it at speed). `FH1_REVERSE_FIX=0` = the old rules.
    pub(super) fn select_direction(&mut self, throttle: f32, brake: f32, fwd_speed: f32, dt: f32) -> (f32, f32) {
        if self.assists.shifting != Shifting::Automatic {
            return (throttle, brake);
        }
        let fix = reverse_fix();
        let stopped = if fix { self.speed() < 0.5 } else { fwd_speed.abs() < 0.5 };
        let go = if fix { throttle > 0.05 && throttle >= brake } else { throttle > 0.5 };
        let dt_state = &mut self.drivetrain;
        if self.gear != 0 && stopped && brake > 0.5 && throttle < 0.05 {
            dt_state.reverse_timer += dt;
            if dt_state.reverse_timer > 0.3 {
                self.gear = 0;
                self.shift_count += 1;
            }
        } else if self.gear == 0 && fwd_speed > -0.5 && go {
            self.gear = 1;
            self.shift_count += 1;
            dt_state.reverse_timer = 0.0;
        } else if self.gear != 0 {
            dt_state.reverse_timer = 0.0;
        }
        if self.gear == 0 { (brake, throttle) } else { (throttle, brake) }
    }

    /// Stats harness only (default.xex tyre function 82D2F348, branch at 82D2F6B4, gated by test mode car+0x1828):
    /// a driven wheel turning forwards past the friction curve's peak slip has its angular speed cut so it sits
    /// exactly at the peak (the same delta goes into the integrator's copy of that wheel speed, car+0x1800). Call it in
    /// the tyre pass before the slip is computed; `denom` is that pass's slip denominator. Returns the new ω.
    /// `kp` = the longitudinal curve's peak slip at the wheel's load.
    pub(super) fn harness_slip_clamp(&self, i: usize, omega: f32, v_long: f32, denom: f32, kp: f32) -> f32 {
        if !self.test_mode || slip_clamp_mode() == 0 || !self.driven(i) || omega <= 0.0 {
            return omega;
        }
        let r = self.tyre_radius(i);
        if (omega * r - v_long) / denom > kp { (v_long + kp * denom) / r } else { omega }
    }

    /// Engine, clutch and automatic gearbox for one step. `throttle` is after traction control.
    /// Returns what the gearbox delivers; `driveline_accels` turns it into wheel accelerations.
    pub(super) fn update_drivetrain(&mut self, throttle: f32, fwd_speed: f32, dt: f32) -> DrivelineInput {
        let ratio = self.gear_ratio() * self.data.final_drive;
        let driven = self.driven_wheels();
        // Carrier speed (the weights sum to 1; for AWD it is the centre diff's input, weighted by the split).
        let k = self.carrier_weights();
        let wheel_omega = if driveline_mode() == 0 {
            driven.iter().map(|&i| self.wheels[i].omega).sum::<f32>() / driven.len() as f32
        } else {
            (0..4).map(|i| k[i] * self.wheels[i].omega).sum::<f32>()
        };
        let road_rpm = (fwd_speed / self.tyre_radius(driven[0]) * ratio).abs() * TO_RPM;
        let test_mode = self.test_mode;
        let shifting = if test_mode { Shifting::Automatic } else { self.assists.shifting };
        let request = std::mem::take(&mut self.shift_request);
        // The pedal opens the clutch (82D30668: capacity x (1 - pedal)). Manual with clutch: always. Manual: the shifts
        // are automatic, but the player can still press it (clutch kicks; FH1_MANUAL_CLUTCH=0 = ignored as before).
        let pedal_on = shifting == Shifting::ManualClutch || (shifting == Shifting::Manual && manual_clutch());
        let pedal = if self.clutch_in { 1.0 } else if pedal_on { self.clutch_pedal.clamp(0.0, 1.0) } else { 0.0 };
        // Mean |normalised slip| of the driven wheels (the launch keeps slipping while they spin, 82D30800).
        let driven_slip = driven.iter().map(|&i| self.wheels[i].norm_slip.abs()).sum::<f32>() / driven.len().max(1) as f32;
        let d = &self.data;
        let rev_limit = rev_limit_rpm(d);
        let s = &mut self.drivetrain;
        // Gearbox-side speed of the clutch (positive when rolling in the selected gear's direction).
        let clutch_omega = wheel_omega * ratio;
        let idle_omega = d.idle_rpm / TO_RPM;
        // Clutch (gamedb List_UpgradeDrivetrainClutch, docs/PHYSICS_PARITY.md): pulling away, its capacity
        // ramps 0 -> ClutchMaxTorque over ClutchOutTime; it reopens once stopped off-throttle or below idle.
        if self.gear <= 1 && throttle > 0.05 {
            s.clutch_timer += dt;
        } else if clutch_omega < idle_omega && throttle <= 0.05 {
            s.clutch_timer = 0.0;
        }
        let capacity = if self.gear >= 2 { 1.0 } else { (s.clutch_timer / d.clutch_out_time).min(1.0) } * d.clutch_max_torque * (1.0 - pedal);
        self.rpm = s.engine_omega * TO_RPM;

        s.shift_timer = (s.shift_timer - dt).max(0.0);
        let base = d.torque_at(self.rpm);
        let game_boost = turbo_game();
        if let Some(b) = d.boost {
            if game_boost {
                // The game's spool (82D2B920; docs/DRIVETRAIN.md "Turbo spool", VERIFIED from the xex): the last tick's
                // ACTUAL (boosted) engine power, lagged by tau = MomentInertia x 0.01 s, sets a pressure target between
                // 3.675 psi and (2 MaxScale - 1) x 14.7 psi over PowerMin..PowerMax hp; the pressure follows at 15/s and
                // the factor is lerp(Min', Max') by pressure. Throttle < 0.01 latches the blow-off (target power 0,
                // pressure 3.675) until > 0.05. Superchargers (82D20538): pressure from rpm / redline x throttle, same 15/s.
                let lo = BOOST_PSI_LO;
                if b.supercharger {
                    let (plo, phi) = ((2.0 * b.raw_lo - 1.0) * 14.7, (2.0 * b.raw_hi - 1.0) * 14.7);
                    let p_rpm = plo + (phi - plo) * (self.rpm / b.redline_rpm.max(1.0)).clamp(0.0, 1.0);
                    let target = lo + (p_rpm - lo) * throttle.clamp(0.0, 1.0);
                    s.boost_psi += (target - s.boost_psi) * (BOOST_PSI_RATE * dt).min(1.0);
                    let y = ((s.boost_psi - lo) / (phi - lo).max(1e-3)).clamp(0.0, 1.0);
                    let min = b.min_scale.min(1.0);
                    self.boost = min + (b.max_scale - min) * y;
                } else {
                    if throttle < 0.01 {
                        s.blowoff = true;
                    } else if throttle > 0.05 {
                        s.blowoff = false;
                    }
                    let p_star = if s.blowoff { 0.0 } else { s.engine_power.clamp(10.0, 1000.0) };
                    let tau = (b.inertia * 0.01).max(1e-3);
                    s.turbo_power = (s.turbo_power + (p_star - s.turbo_power) * (dt / tau).min(1.0)).clamp(0.0, b.power_max_hp.max(0.0));
                    let hi = (2.0 * b.raw_hi - 1.0) * 14.7;
                    let x = ((s.turbo_power - b.power_min_hp) / (b.power_max_hp - b.power_min_hp).max(1e-3)).clamp(0.0, 1.0);
                    let target = if s.blowoff { lo } else { lo + (hi - lo) * x };
                    s.boost_psi += (target - s.boost_psi) * (BOOST_PSI_RATE * dt).min(1.0);
                    let y = ((s.boost_psi - lo) / (hi - lo).max(1e-3)).clamp(0.0, 1.0);
                    self.boost = b.min_scale + (b.max_scale - b.min_scale) * y;
                }
            } else {
                let target = b.min_scale + (b.target(base, self.rpm) - b.min_scale) * throttle;
                self.boost += (target - self.boost) * (dt / b.spool_time.max(0.05)).min(1.0);
            }
        }
        // Drop-off: the game shrinks only the boost part, b = 1 + (b - 1) x lerp(scale) (82D230B8); the old rule scaled the
        // whole torque.
        let dropoff = match d.boost {
            Some(b) if game_boost => {
                if self.boost > 1.0 {
                    (1.0 + (self.boost - 1.0) * b.dropoff(self.rpm)) / self.boost
                } else {
                    1.0
                }
            }
            Some(b) => b.dropoff(self.rpm),
            None => 1.0,
        };
        let mut engine_torque = if engine_drag_mode() {
            // The game's engine torque (82D23380; docs/DRIVETRAIN.md "Engine torque", VERIFIED live to 0.005 hN·m):
            // T = a(ω) + t·(b(ω)·k − a(ω)), a = zero-throttle drag (engine braking), b = the full-throttle curve (x boost),
            // k = OffRoadEnginePowerScale with all four tyres off road. Rev limiter (82D2CC38): above the limit the throttle
            // is forced to 0 for 0.04 s, so the cut brings the full drag, not zero torque.
            if self.rpm > rev_limit {
                s.rev_cut = REV_CUT_TIME;
            }
            let t = if s.rev_cut > 0.0 { 0.0 } else { throttle.clamp(0.0, 1.0) };
            s.rev_cut = (s.rev_cut - dt).max(0.0);
            let k = if self.offroad_wheels >= 4 { d.offroad_power_scale } else { 1.0 };
            let full = base * self.boost * dropoff * self.torque_mult * k;
            let drag = d.engine_drag(self.rpm);
            if s.shift_timer > 0.0 { 0.0 } else { drag + t * (full - drag) }
        } else if self.rpm >= rev_limit || s.shift_timer > 0.0 {
            0.0
        } else {
            throttle * base * self.boost * dropoff * self.torque_mult
        };
        if !engine_drag_mode() && throttle < 0.05 && self.rpm > d.idle_rpm * 1.05 {
            // Old engine braking (FH1_ENGINE_DRAG=0): 15% of peak torque scaled by rpm.
            engine_torque = -0.15 * d.torque_scale * (self.rpm / d.redline_rpm);
        }
        // The turbo's next power target (car+0xC60 = T_e x omega of this tick).
        s.engine_power = engine_torque.max(0.0) * s.engine_omega / 745.7;
        // Manual gearbox (Shifting assist Manual / Manual with clutch): the driver's requests, taken when no shift is in
        // progress; with the clutch variant only while the pedal is down. Reverse sits below 1st and is refused above
        // MaxForwardVelShiftReverseMPH (15 mph, PhysicsSettings.ini); a downshift that would put the engine past the rev
        // limit is refused (our rule: FullThrottleShiftDownSafetyScale 0.85 is not decoded).
        if shifting != Shifting::Automatic {
            let n = d.gears.len();
            let clutch_ok = shifting == Shifting::Manual || pedal > 0.5;
            if request != 0 && s.shift_timer == 0.0 && clutch_ok {
                let old = self.gear;
                if request > 0 && self.gear < n {
                    self.gear += 1;
                } else if request < 0 && self.gear > 1 {
                    let lower = d.gears[self.gear - 2] / d.gears[self.gear - 1];
                    if road_rpm * lower < rev_limit {
                        self.gear -= 1;
                    }
                } else if request < 0 && self.gear == 1 && fwd_speed < MAX_FORWARD_SPEED_FOR_REVERSE {
                    self.gear = 0;
                }
                if self.gear != old {
                    s.shift_timer = d.shift_time;
                    self.shift_count += 1;
                }
            }
        }
        // Automatic gearbox. Shift decisions use road speed so wheelspin doesn't trigger upshifts.
        else if self.gear >= 1 && s.shift_timer == 0.0 {
            let n = d.gears.len();
            if road_rpm > 0.97 * d.redline_rpm && self.gear < n {
                self.gear += 1;
                s.shift_timer = d.shift_time;
                self.shift_count += 1;
            } else if self.gear > 1 {
                let lower = d.gears[self.gear - 2] / d.gears[self.gear - 1];
                if road_rpm * lower < 0.7 * d.redline_rpm && road_rpm < 0.45 * d.redline_rpm {
                    self.gear -= 1;
                    s.shift_timer = d.shift_time;
                    self.shift_count += 1;
                }
            }
        }
        // Clutch, stick/slip: locked, the engine turns with the gearbox and passes its torque (until that
        // exceeds the capacity); slipping, it transmits the full capacity towards the slower side and locks
        // once the two speeds meet.
        // Stats harness launch (auto-clutch 82D30800, state car+0x1A2C = 1, test mode): the clutch slips to hold the
        // engine at a launch speed until the clutch output reaches it, then releases (and locks below).
        // Normal play (auto-clutch launch state, same function; not with Manual with clutch, whose clutch is the player's):
        // armed whenever the car is stopped off-throttle in 1st / reverse, it slips at `launch_target` until the clutch
        // output reaches it with the driven wheels gripping (mean normalised slip < 0.7). Without it the clutch closed
        // on an idling engine and the car bogged until moving. FH1_PLAY_LAUNCH=0 = off (old).
        let launch_omega = if test_mode { 0.95 * d.redline_rpm / TO_RPM } else { launch_target(d, s.launch_t, throttle) };
        if test_mode {
            if launch_mode() == 0 || self.gear != 1 || clutch_omega >= launch_omega {
                s.launch_done = true;
            }
        } else {
            let play = play_launch_mode() != 0 && shifting != Shifting::ManualClutch;
            if play && self.gear <= 1 && clutch_omega.abs() < idle_omega && throttle <= 0.05 {
                s.launch_done = false;
                s.launch_t = 0.0;
            }
            // Live (Pinyon Viper launch, ticks 12676-12804): with the rears at normalised slip 7.7 the pedal still fully
            // releases once the clutch output (spinning wheels x gearing) reaches the engine speed, and the revs then climb
            // with the wheelspin. So the launch also ends when the output catches the engine, whatever the slip; before,
            // spinning wheels kept the launch going and pinned the engine at the target ("revs don't rise while the wheels
            // spin", user 2026-10-07). FH1_LAUNCH_SPIN_EXIT=0 = the old rule.
            let caught = launch_spin_exit() && clutch_omega >= s.engine_omega - 1.0;
            if !play || self.gear > 1 || caught || (clutch_omega >= launch_omega && driven_slip < LAUNCH_EXIT_SLIP) {
                s.launch_done = true;
            }
        }
        let launching = !s.launch_done && throttle > 0.05;
        if launching && !test_mode {
            s.launch_t += dt;
        }
        let open = !launching && (capacity <= 0.0 || (clutch_omega < idle_omega && throttle <= 0.05));
        let delta = s.engine_omega - clutch_omega;
        if open || engine_torque.abs() > capacity {
            s.clutch_locked = false;
        }
        let clutch_torque = if launching {
            // The game P-controls the pedal (rate = 53 × (target − (ω + 0.04·ω̇)) / redline ω); here the slipping
            // clutch passes the engine torque plus whatever pulls the engine back onto the target within LAUNCH_TAU.
            s.clutch_locked = false;
            let tc = (engine_torque + d.engine_inertia * (s.engine_omega - launch_omega) / LAUNCH_TAU).clamp(0.0, d.clutch_max_torque * (1.0 - pedal));
            s.engine_omega += (engine_torque - tc) / d.engine_inertia * dt;
            tc
        } else if open {
            0.0
        } else if s.clutch_locked {
            s.engine_omega = clutch_omega;
            engine_torque
        } else if delta.abs() < 1.0 && engine_torque.abs() <= capacity {
            s.clutch_locked = true;
            s.engine_omega = clutch_omega;
            engine_torque
        } else {
            // No more than closes the gap in one step: engine against the driven wheels' inertia seen
            // through the gearing (a truck's ~60:1 first gear would otherwise overshoot every step).
            let wheels = driven.len() as f32 * d.wheel_inertia / (ratio * ratio).max(1e-6);
            let reduced = d.engine_inertia * wheels / (d.engine_inertia + wheels);
            let tc = (reduced * delta.abs() / dt).min(capacity) * if delta.abs() > 1e-3 { delta.signum() } else { engine_torque.signum() };
            s.engine_omega += (engine_torque - tc) / d.engine_inertia * dt;
            tc
        };
        if open {
            s.engine_omega += engine_torque / d.engine_inertia * dt;
        }
        // Idle governor and rev limiter bound the engine.
        s.engine_omega = s.engine_omega.clamp(idle_omega, rev_limit * 1.02 / TO_RPM);
        self.torque_fraction = engine_torque / d.torque_scale.max(1.0);
        // Clutch engaged: the engine's inertia rides on the driven wheels through the gearing (82D396E0 /
        // 82D39AE0: G²·I_engine at the gearbox output, × final drive² at the axle). Slipping or open, the engine
        // integrates on its own (above) and only the clutch torque reaches the wheels.
        let engaged = !open && s.clutch_locked;
        let torque = clutch_torque * ratio;
        let weights = self.carrier_weights();
        DrivelineInput {
            torque,
            inertia: if engaged && driveline_mode() != 0 { ratio * ratio * d.engine_inertia } else { 0.0 },
            weights,
            drive: weights.map(|k| k * torque),
        }
    }

    /// How the carrier (gearbox output / final drive) speed is made of the wheel speeds, ω_c = Σ k_i ω_i, which
    /// also gives each wheel's share of the input torque. 2WD: an open axle diff, ½ per driven wheel. AWD
    /// (82D39AE0): an open centre diff splitting torque (1 − s) front / s rear, then open axle diffs.
    pub(super) fn carrier_weights(&self) -> [f32; 4] {
        let d = &self.data;
        // AWD carrier ratios (82D27008, docs/DRIVETRAIN.md "AWD split"): f = 2·FD·r_r·(1 − s) / D, r = 2·FD·r_f·s / D with
        // D = r_f·s + r_r·(1 − s), so both axles' wheels turn at the same road speed for any tyre stagger. Equal radii give
        // the plain (1 − s) / s split. `FH1_AWD_RADIUS=0` = the plain split (before 2026-10-07).
        let s = d.rear_torque_split;
        let (rf, rr) = if awd_radius() { (d.tyre_radius[0], d.tyre_radius[1]) } else { (1.0, 1.0) };
        let den = (rf * s + rr * (1.0 - s)).max(1e-4);
        std::array::from_fn(|i| match (d.drive_type, i < 2) {
            (3, true) => 0.5 * rr * (1.0 - s) / den,
            (3, false) => 0.5 * rf * s / den,
            _ if self.driven(i) => 0.5,
            _ => 0.0,
        })
    }

    /// Limited-slip lock torques per wheel (82D396E0 2WD / 82D39AE0 AWD; docs/DRIVETRAIN.md "LSD"), added to the wheels'
    /// road torques before the driveline solve. `t_in` = the diff input torque, i.e. the carrier torque (gear × final
    /// drive × clutch torque, after TCS). Each axle diff: lock = L(t_in) × clamp(Δω / RelVelClamp, −1, 1), Δω = left −
    /// right, taken from the faster wheel and given to the slower. AWD also locks the centre diff on
    /// Δω = ½(ω_f0 + ω_f1)·r_f/r_r − ½(ω_r0 + ω_r1), ∓lock on EACH front / rear wheel (82D39AE0: lock × 2 × ½).
    /// Without it every 2WD car had an open diff: the unloaded inside wheel spun and the outside one never broke loose.
    pub(super) fn diff_locks(&self, t_in: f32) -> [f32; 4] {
        let mut out = [0.0f32; 4];
        if lsd_mode() == 0 {
            return out;
        }
        let d = &self.data;
        let w = |i: usize| self.wheels[i].omega;
        let axle = |out: &mut [f32; 4], a: usize| {
            let l = d.diffs[a].lock(t_in, w(2 * a) - w(2 * a + 1));
            out[2 * a] -= l;
            out[2 * a + 1] += l;
        };
        match d.drive_type {
            1 => axle(&mut out, 0),
            3 => {
                axle(&mut out, 0);
                axle(&mut out, 1);
                let ratio = d.tyre_radius[0] / d.tyre_radius[1].max(1e-3);
                let l = d.diffs[2].lock(t_in, 0.5 * (w(0) + w(1)) * ratio - 0.5 * (w(2) + w(3)));
                out[0] -= l;
                out[1] -= l;
                out[2] += l;
                out[3] += l;
            }
            _ => axle(&mut out, 1),
        }
        out
    }

    /// Wheel angular accelerations from the coupled driveline (default.xex 82D396E0 2WD / 82D39AE0 AWD).
    /// `reaction[i]` is the torque the road, rolling resistance etc. put on wheel i (brakes are applied after,
    /// as before). The mass matrix is diag(I_wheel) + J·k·kᵀ (the game builds it in full and solves with
    /// 82CB6660; it is the same rank-1 form), so Sherman-Morrison solves it exactly.
    pub(super) fn driveline_accels(&self, input: &DrivelineInput, reaction: [f32; 4]) -> [f32; 4] {
        let iw = self.data.wheel_inertia;
        let k = input.weights;
        let b: [f32; 4] = std::array::from_fn(|i| reaction[i] + k[i] * input.torque);
        let kb: f32 = (0..4).map(|i| k[i] * b[i] / iw).sum();
        let kk: f32 = (0..4).map(|i| k[i] * k[i] / iw).sum();
        let carrier = input.inertia * kb / (1.0 + input.inertia * kk);
        std::array::from_fn(|i| (b[i] - k[i] * carrier) / iw)
    }
}

/// What the gearbox delivers to the axles for one step, in carrier (final-drive input × final drive) terms.
#[derive(Debug, Clone, Copy)]
pub struct DrivelineInput {
    /// Torque at the carrier (N·m): clutch torque × gear × final drive.
    pub torque: f32,
    /// Inertia reflected to the carrier (kg·m²): (gear × final drive)² × engine inertia while the clutch is engaged.
    pub inertia: f32,
    /// ω_carrier = Σ weights[i] · ω_wheel[i]; wheel i receives weights[i] × torque.
    pub weights: [f32; 4],
    /// Torque each wheel receives from the carrier (weights × torque), N·m.
    pub drive: [f32; 4],
}

/// `input[i]` is wheel i's drive torque, so code written for the old `[f32; 4]` return keeps working.
impl std::ops::Index<usize> for DrivelineInput {
    type Output = f32;
    fn index(&self, i: usize) -> &f32 {
        &self.drive[i]
    }
}

/// MaxForwardVelShiftReverseMPH (PhysicsSettings.ini): no shift into reverse above 15 mph forwards.
const MAX_FORWARD_SPEED_FOR_REVERSE: f32 = 15.0 * 0.447_04;

/// Engine-speed error time constant of the harness launch clutch (s).
const LAUNCH_TAU: f32 = 0.05;

/// The auto-clutch keeps launching while the driven wheels' mean |normalised slip| is at least this (82D30800, 0.7 @82000D9C).
const LAUNCH_EXIT_SLIP: f32 = 0.7;

/// Normal-play launch target engine speed (rad/s), auto-clutch 82D30800 with car+0x1828 = 0 (re/out dtB_misc.c; constants
/// read from the xex, re/out/dtB_c3.log): with S = StallRPM, St = StartRPM, R = RedlineRPM and k = t / 1.5 s (car+0x16F4,
/// clamped to 0..1), hi = lerp(St + 0.2 (R - St), S + 0.3 (St - S), k), lo = lerp(S + 0.2 (St - S), S + 0.01 (St - S), k),
/// target = lo + (hi - lo) x throttle. Median stock car (S 800, St 4,446, R 7,000) at full throttle: 4,957 rpm at the
/// start of a launch, easing to 1,894 rpm by 1.5 s. INFERRED: car+0x16F4 accumulates dt and is reset when a launch ends;
/// we count it from the start of each launch (FH1_PLAY_LAUNCH=2: always the late, t >= 1.5 s targets).
fn launch_target(d: &crate::data::CarData, t: f32, throttle: f32) -> f32 {
    let (stall, red) = (d.idle_rpm, d.redline_rpm);
    let start = if d.start_rpm > stall { d.start_rpm.min(red) } else { 0.5 * (stall + red) };
    let k = if play_launch_mode() == 2 { 1.0 } else { (t / 1.5).clamp(0.0, 1.0) };
    let lerp = |a: f32, b: f32, x: f32| a + (b - a) * x;
    let hi = lerp(start + 0.2 * (red - start), stall + 0.3 * (start - stall), k);
    let lo = lerp(stall + 0.2 * (start - stall), stall + 0.01 * (start - stall), k);
    (lo + (hi - lo) * throttle.clamp(0.0, 1.0)) / TO_RPM
}

/// Normal-play launch (`launch_target`): 1 = on (default), 2 = late targets only, 0 = off (the old idle bog).
fn play_launch_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| std::env::var("FH1_PLAY_LAUNCH").ok().and_then(|s| s.parse().ok()).unwrap_or(1))
}

/// Clutch pedal honoured in Manual too (`update_drivetrain`). FH1_MANUAL_CLUTCH=0 = Manual with clutch only (old).
fn manual_clutch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_MANUAL_CLUTCH").map_or(true, |v| v != "0"))
}

/// Automatic reverse selection fix (`select_direction`). FH1_REVERSE_FIX=0 = the old rules.
fn reverse_fix() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_REVERSE_FIX").map_or(true, |v| v != "0"))
}

/// Rev limit: FH1 keeps it at car+0xFA4 = (RedlineRPM + TorqueCurveMaxRPM) / 2 (Xenia: Corrado with the
/// Level 1 cam 7,750 = (7,000 + 8,500) / 2; Viper flat at ~7,104 = (6,200 + 8,000) / 2). FH1_REVLIMIT=0 keeps the
/// old RedlineRPM × 1.146 from data.rs.
fn rev_limit_rpm(d: &crate::data::CarData) -> f32 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    let mode = *MODE.get_or_init(|| std::env::var("FH1_REVLIMIT").ok().and_then(|s| s.parse().ok()).unwrap_or(1));
    if mode == 0 { d.rev_limit_rpm } else { 0.5 * (d.redline_rpm + d.torque_curve_max_rpm) }
}

// The decoded stats-harness chain + the driveline engine inertia, DEFAULT ON since 2026-10-05 (H3): together they took the
// parity 0-60 drive-type spread from FWD -3.8 / RWD +3.7 / AWD +12.9 to -3.2 / -0.9 / +4.9 and the 1/4-mile trap speed
// from +2.0 to +0.7 % (docs/DRIVETRAIN.md "H3"). The launch and slip clamp act in test mode only (the parity harness).

/// Stats-harness launch with the clutch slipping at 0.95 × redline (auto-clutch 82D30800, test mode only).
/// FH1_LAUNCH=0 = off.
fn launch_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| std::env::var("FH1_LAUNCH").ok().and_then(|s| s.parse().ok()).unwrap_or(1))
}

/// Stats-harness slip clamp (82D2F6B4, test mode only; uses the tyre pass slip floor). FH1_SLIPCLAMP=0 = off.
fn slip_clamp_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| std::env::var("FH1_SLIPCLAMP").ok().and_then(|s| s.parse().ok()).unwrap_or(1))
}

/// Engine inertia reflected onto the driven wheels while the clutch is engaged (82D396E0 / 82D39AE0: G²·I_e at the
/// gearbox output). Normal play too. FH1_DRIVELINE=0 = the old behaviour (engine inertia ignored when locked).
fn driveline_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| std::env::var("FH1_DRIVELINE").ok().and_then(|s| s.parse().ok()).unwrap_or(1))
}

/// Limited-slip diff lock torques (diff_locks), normal play and test mode. FH1_LSD=0 = open diffs (the old behaviour).
fn lsd_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| std::env::var("FH1_LSD").ok().and_then(|s| s.parse().ok()).unwrap_or(1))
}

/// Tyre slip-ratio denominator floor in m/s: the game's 6.0 (82000C90, tyre fn 82D2F348, normal play and test mode);
/// FH1_SLIP_FLOOR overrides it. Read by the tyre pass (tyre.rs).
pub(super) fn slip_floor() -> f32 {
    static FLOOR: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *FLOOR.get_or_init(|| std::env::var("FH1_SLIP_FLOOR").ok().and_then(|s| s.parse().ok()).unwrap_or(6.0))
}

/// `FH1_AWD_RADIUS=0`: AWD carrier ratios without the tyre-radius weighting (as before 2026-10-07).
fn awd_radius() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_AWD_RADIUS").map_or(true, |v| v != "0"))
}

/// Rev-limiter cut (82D2CC38, const 0x8223691C).
const REV_CUT_TIME: f32 = 0.04;

/// `FH1_ENGINE_DRAG=0`: the old engine model (torque 0 at the limiter, stopgap engine braking), as before 2026-10-07.
/// Boost pressure floor (psi, 82236BC8 = 14.7 / 4) and the pressure's follow rate (1/s, 82236910).
const BOOST_PSI_LO: f32 = 3.675;
const BOOST_PSI_RATE: f32 = 15.0;

/// FH1_TURBO_GAME=0: the old boost (unboosted power target, MomentInertia / 30 s lag, drop-off on the whole torque).
fn turbo_game() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_TURBO_GAME").map_or(true, |v| v != "0"))
}

fn engine_drag_mode() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_ENGINE_DRAG").map_or(true, |v| v != "0"))
}

/// `FH1_LAUNCH_SPIN_EXIT=0`: the normal-play launch keeps slipping while the driven wheels spin (as before 2026-10-07).
fn launch_spin_exit() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_LAUNCH_SPIN_EXIT").map_or(true, |v| v != "0"))
}
