//! The game's per-wheel tyre force (default.xex 82D2F348 and the rolling-resistance part of its caller 82D36DF8).
//! Spec: docs/HANDLING_PARITY.md section 8 (verified on the Xenia Corrado capture to 3-4 significant figures).
//!
//! A normalised friction circle: slip ratio and slip angle are divided by their load-dependent curve peaks, the
//! combined slip samples both curves, and each axis gets its share. Forces are scaled by the compound's TorqueFree
//! scales (lateral; driving or braking), the tyre-width scale (baked into the curves), the chassis per-axle scales and
//! the surface grip, with the slip axes stretched by the sidewall slip-axis scale (8.15). Not ported (open in 8.8):
//! the tyre-condition terms incl. the 1.39x standstill grip and the racing-line bonus (1.01). Per-surface terms
//! (surfaceTypes.xml `<Friction>`, [`TyreSurface`]): off-road peak blend, RearGripMultiplier, MinimumArcadeGripValue (the
//! arcade curve layer), HandbrakeGripMultiplier and VelDepFriction (8.3 steps 4, 8, 11, 12; 8.6).

use super::Vehicle;
use crate::data::TorqueFree;

/// Below this |v| (m/s) the slip angle is replaced by |v_lat| / 0.7; blended up to 1 m/s.
const LOW_SPEED: f32 = 0.7;

/// The tyre terms of one surface (surfaceTypes.xml `<Friction>`, the game's 284-byte surface record; docs/HANDLING_PARITY.md
/// 2.3). Default = Asphalt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TyreSurface {
    /// FrictionScale (+0x04): every tyre force.
    pub friction: f32,
    /// OffRoadness (+0x0C) and OffRoadDryPeakSA (+0x14, degrees): blend both curve peaks toward this slip angle (8.3 step 4).
    pub offroadness: f32,
    pub offroad_peak_sa: f32,
    /// RearGripMultiplier (+0x1C): rear wheels, both axes.
    pub rear_grip: f32,
    /// MinimumArcadeGripValue (+0x20): weight of the no-drop-off curve layer (grip holds its peak past the peak slip).
    pub arcade: f32,
    /// HandbrakeGripMultiplier (+0x24): rear grip under full handbrake above 20 m/s.
    pub handbrake_grip: f32,
    /// VelDepFriction VelPeak0 / VelPeak (m/s), LateralCoeff0 / LateralCoeff, LongCoeff0 / LongCoeff (+0x3C..+0x50): rolling
    /// resistance and lateral scrub (8.6).
    pub veldep_speed: [f32; 2],
    pub veldep_lat: [f32; 2],
    pub veldep_long: [f32; 2],
}

impl Default for TyreSurface {
    fn default() -> Self {
        Self {
            friction: 1.0,
            offroadness: 0.0,
            offroad_peak_sa: 12.0,
            rear_grip: 1.0,
            arcade: 0.0,
            handbrake_grip: 0.5,
            veldep_speed: [0.0, 8.0],
            veldep_lat: [0.0, 0.03],
            veldep_long: [0.0, 0.01],
        }
    }
}

impl TyreSurface {
    /// From a surface's `<Friction>` values by path (`param("Friction/FrictionScale")`); missing values = Asphalt's.
    /// `FH1_SURFACE_TYRE=0`: Asphalt's terms with only the FrictionScale (as before 2026-10-07).
    pub fn from_params(param: impl Fn(&str) -> Option<f32>) -> Self {
        let a = Self::default();
        let g = |k: &str, d: f32| param(&format!("Friction/{k}")).unwrap_or(d);
        let friction = g("FrictionScale", a.friction);
        if !surface_tyre_enabled() {
            return Self { friction, ..a };
        }
        let v = |k: &str, d: f32| g(&format!("VelDepFriction/{k}"), d);
        Self {
            friction,
            offroadness: g("OffRoadness", a.offroadness),
            offroad_peak_sa: g("OffRoadDryPeakSA", a.offroad_peak_sa),
            rear_grip: g("RearGripMultiplier", a.rear_grip),
            arcade: g("MinimumArcadeGripValue", a.arcade),
            handbrake_grip: g("HandbrakeGripMultiplier", a.handbrake_grip),
            veldep_speed: [v("VelPeak0", a.veldep_speed[0]), v("VelPeak", a.veldep_speed[1])],
            veldep_lat: [v("LateralCoeff0", a.veldep_lat[0]), v("LateralCoeff", a.veldep_lat[1])],
            veldep_long: [v("LongCoeff0", a.veldep_long[0]), v("LongCoeff", a.veldep_long[1])],
        }
    }

    /// VelDep(|v|): lerp from Coeff0 at VelPeak0 to Coeff at VelPeak, clamped.
    fn veldep(&self, v: f32, c: [f32; 2]) -> f32 {
        let [v0, v1] = self.veldep_speed;
        let t = if v1 > v0 { ((v.abs() - v0) / (v1 - v0)).clamp(0.0, 1.0) } else { (v.abs() >= v1) as u8 as f32 };
        c[0] + (c[1] - c[0]) * t
    }
}

/// `FH1_SURFACE_TYRE=0`: every surface uses Asphalt's tyre terms with only its FrictionScale (as before 2026-10-07).
pub fn surface_tyre_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SURFACE_TYRE").map_or(true, |v| v != "0"))
}
/// Old normalised slip clamp (x peak), under `FH1_SLIP_CLAMP=1` only: the game has none (8.15).
const SLIP_CLAMP: f32 = 2.5;
/// PhysicsSettings ABSOffBrakingFrictionScale (settings+0x128).
const ABS_OFF_BRAKING_FRICTION_SCALE: f32 = 1.05;

/// PhysicsSettings.ini `SlidingInstability\*` (settings+0x2C0..+0x2D8, read by 82D2BBB0).
const INSTABILITY_FRICTION: [f32; 2] = [1.05, 2.0];
const INSTABILITY_DURATION: [f32; 2] = [0.45, 0.15];
const INSTABILITY_DURATION_RAND: f32 = 0.05;
const INSTABILITY_SCALE: f32 = 0.12;
/// The two load offsets the instability alternates between (0x82005BFC / 0x82141E70; live wheel+0x74 = 1.1 / -0.9).
const INSTABILITY_SIGN: [f32; 2] = [1.1, -0.9];
/// Surfaces with FrictionScale below this get no instability (const 0x82000C98, not read: live it acts on asphalt 1.0 and
/// dirt 0.95, never on grass 0.85; 0.9 is inside that bracket).
const INSTABILITY_MIN_FRICTION: f32 = 0.9;
/// Low-pass of ρ the instability reads (wheel+0x260). FITTED on the live capture: ≈0.3 of the gap per 2.78 ms tick.
const RHO_FILTER_TAU: f32 = 0.0078;

/// `FH1_SLIDING_INSTABILITY=0`: off (as before 2026-10-06).
fn sliding_instability_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SLIDING_INSTABILITY").map_or(true, |v| v != "0"))
}

/// Per-wheel SlidingInstability state (wheel+0x74 / +0x78 / +0x7C / +0x80 / +0x260). All zero = off.
#[derive(Debug, Clone, Copy, Default)]
pub struct Instability {
    /// Current load offset sign (+0x74), half-period timer (+0x7C), period (+0x80).
    sign: f32,
    timer: f32,
    period: f32,
    /// Load multiplier - 1 this tick (+0x78 - 1).
    pub offset: f32,
    /// ρ low-passed (+0x260).
    pub rho: f32,
}

/// What the tyre pass gives the tyre function for one grounded wheel.
pub(super) struct TyreIn {
    /// Normal load (N), unclamped.
    pub load: f32,
    /// Contact-point velocity along the wheel's forward / right axes (m/s).
    pub v_long: f32,
    pub v_lat: f32,
    /// Wheel spin (rad/s) and tyre radius (m).
    pub omega: f32,
    pub radius: f32,
    /// The contact surface's tyre terms.
    pub surf: TyreSurface,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TyreOut {
    /// Tyre forces (N) along forward / right, without rolling resistance: these turn the wheel.
    pub fx: f32,
    pub fy: f32,
    /// Rolling resistance and lateral scrub (N), applied to the body only (82D36DF8 adds them after the call).
    pub rolling_x: f32,
    pub rolling_y: f32,
    /// Slip ratio with the game's 6 m/s floor (signed, + when the wheel turns faster than the road) and slip angle (deg,
    /// signed like v_lat).
    pub slip_ratio: f32,
    pub slip_angle_deg: f32,
    /// Normalised slips (1 = the curve's peak at this load), signed like slip_ratio / slip_angle_deg.
    pub sigma_x: f32,
    pub sigma_y: f32,
    /// The TorqueFree long scale this wheel used (driving or braking; wheel+0x68): the chassis torque divides it out.
    pub long_scale: f32,
    /// Combined normalised slip ρ (wheel+0x25C).
    pub rho: f32,
}

/// `FH1_SLIP_CLAMP=1`: clamp the normalised slips at 2.5 as before 2026-10-06. The game doesn't: the "2.5" read on the
/// live Viper's locked wheels was 1 / (peak 0.4) on dirt and grass (OffRoadness 1, OffRoadDryPeakSA 18 deg); on asphalt
/// the live σx reaches 29 and σy 6.8 (docs/HANDLING_PARITY.md 8.15).
fn slip_clamp_old() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SLIP_CLAMP").is_ok_and(|v| v == "1"))
}

/// `FH1_TYRE_SCALES=0`: drive with the dev-screen grip (all TorqueFree scales 1.0), i.e. as before the scales were
/// ported. Default on (the game's normal play).
pub fn tyre_scales_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_TYRE_SCALES").map_or(true, |v| v != "0"))
}

impl Vehicle {
    /// TorqueFree scales in use: the compound's in normal play, 1.0 under `FH1_TYRE_SCALES=0` or the stats harness.
    pub(super) fn default_torque_free(data: &crate::data::CarData) -> TorqueFree {
        let t = data.tyre.torque_free;
        if tyre_scales_enabled() { t } else { t.overridden(1.0) }
    }

    /// TorqueFree scales the setup tables (brake capacity 82D262B0, steering lock table 82D25E00) are built with: always the
    /// compound's own. The stats harness's override (82D3B9E8 -> 82D18DB0) is four plain stores into the tyre objects and
    /// nothing rebuilds the tables after it (docs/HANDLING_PARITY.md 8.17), so in the dev screen only the per-tick tyre
    /// scales are 1.0. Same as before in normal play (no override). `FH1_SETUP_SCALES=override` = the old harness behaviour.
    pub(super) fn setup_torque_free(&self) -> TorqueFree {
        static OLD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let old = *OLD.get_or_init(|| std::env::var("FH1_SETUP_SCALES").is_ok_and(|v| v == "override"));
        if old { self.torque_free } else { Self::default_torque_free(&self.data) }
    }

    /// The stats harness as the dev car-stats screen runs it (82D3B9E8 with config+24 = 1.0 -> 82D18DB0): every
    /// TorqueFree scale on every tyre set to `value`. gamedb's Sim* numbers were produced with 1.0.
    pub fn override_torque_free(&mut self, value: f32) {
        self.torque_free = self.data.tyre.torque_free.overridden(value);
    }

    /// Driving longitudinal scale (car+0x1720, 82D35B90): lerp(Accel0 -> Accel1) over speed² between the two speeds.
    fn accel_scale(&self) -> f32 {
        let t = self.torque_free;
        let (a, b) = (t.accel_speed[0] * t.accel_speed[0], t.accel_speed[1] * t.accel_speed[1]);
        let v2 = self.velocity.length_squared();
        let k = if b > a { ((v2 - a) / (b - a)).clamp(0.0, 1.0) } else { (v2 >= b) as u8 as f32 };
        t.accel[0] + (t.accel[1] - t.accel[0]) * k
    }

    /// Longitudinal TorqueFree scale for this wheel (wheel+0x68, set by the caller): driving when the wheel turns
    /// faster than the road in its direction (ω·s < 0), else braking: GameFrictionScaleBraking x brake scale x
    /// (ABS off ? 1.05 : 1) (82D24150).
    pub(super) fn long_scale(&self, omega: f32, slip_vel: f32, abs: bool) -> f32 {
        if omega * slip_vel < 0.0 {
            self.accel_scale()
        } else {
            self.data.brakes.game_friction_scale * self.torque_free.brake * if abs { 1.0 } else { ABS_OFF_BRAKING_FRICTION_SCALE }
        }
    }

    /// The tyre function for wheel `i`. `handbrake` is the input (0..1), `abs` the assist, `steer` the road-wheel
    /// angle (rad). Before calling, the stats-harness clamp may adjust ω (it needs `peak_slip_ratio_at`).
    pub(super) fn tyre_force(&self, i: usize, t: &TyreIn, handbrake: f32, abs: bool, steer: f32) -> TyreOut {
        let d = &self.data;
        let axle = i / 2;
        let lat_c = &d.lateral;
        let long_c = &d.longitudinal;
        let width = d.tyre.width_scale[axle];
        // s = v_long - ωr (negative when driving), the game's slip velocity.
        let s = t.v_long - t.omega * t.radius;
        let sx_scale = self.long_scale(t.omega, s, abs);
        let surf = &t.surf;
        let surface = surf.friction;
        // Tyre-condition speed term (82D2BD90, wheel+0x60 / +0x64): extra grip below 1.5 m/s contact speed.
        let boost = self.speed_grip((t.v_long * t.v_long + t.v_lat * t.v_lat).sqrt());
        let scale_x = sx_scale * d.tyre.chassis_long[axle] * surface * boost;
        let scale_y = self.torque_free.lat * d.tyre.chassis_lat[axle] * surface * boost;

        // Load clamp and position between the curves, per table.
        let (lx, ly) = (long_c.clamp_load(t.load), lat_c.clamp_load(t.load));
        let (fx_frac, fy_frac) = (long_c.load_frac(lx), lat_c.load_frac(ly));
        // Peaks on the stretched slip axes (slip-axis scale, 8.15). The curves are sampled at ρ x the unstretched
        // peak, which is the game's ρ x stretched peak on the stretched axis.
        let ss = d.tyre.slip_scale[axle];
        let pk = long_c.peak_slip_at(fx_frac) * ss;
        let pa = lat_c.peak_slip_at(fy_frac) * ss;
        // Off-road blend (8.3 step 4): b = 1 + OffRoadness x (OffRoadDryPeakSA / Pα - 1) pulls both peaks toward the
        // surface's peak slip angle (1 on asphalt). The curves are still sampled at ρ x the unblended peak.
        let b = 1.0 + surf.offroadness * (surf.offroad_peak_sa / pa - 1.0);

        let kappa = s.abs() / t.v_long.abs().max(super::drivetrain::slip_floor());
        // No clamp on the normalised slips: past the peak the curve keeps falling to its last sample at MaxSlip.
        let clamp = if slip_clamp_old() { SLIP_CLAMP } else { f32::MAX };
        let sigma_x = (kappa / (b * pk)).min(clamp);
        let angle_deg = t.v_lat.abs().atan2(t.v_long.abs()).to_degrees();
        let v = (t.v_long * t.v_long + t.v_lat * t.v_lat).sqrt();
        let hi = angle_deg / (b * pa);
        let lo = t.v_lat.abs() / LOW_SPEED;
        let sigma_y = if v > 1.0 {
            hi
        } else if v < LOW_SPEED {
            lo
        } else {
            lo + (hi - lo) * (v - LOW_SPEED) / (1.0 - LOW_SPEED)
        }
        .min(clamp);
        let rho = (sigma_x * sigma_x + sigma_y * sigma_y).sqrt().max(1e-4);
        // Layer weight = max(wheel+0x58 / +0x5C, MinimumArcadeGripValue); the wheel weights are 0 in every capture.
        let mu_x = long_c.mu_layered(rho * pk / ss, fx_frac, surf.arcade) * width;
        let mu_y = lat_c.mu_layered(rho * pa / ss, fy_frac, surf.arcade) * width;
        // Forces oppose the slip velocity / lateral velocity (game sgn⁻).
        let opp = |x: f32| if x >= 0.0 { -1.0 } else { 1.0 };
        let mut fx = mu_x * (sigma_x / rho) * lx * opp(s) * scale_x;
        let mut fy = mu_y * (sigma_y / rho) * ly * opp(t.v_lat) * scale_y;

        if axle == 1 {
            // FixListing (car+0x17E0..0x17F0): rear lateral grip x RearFricScale at low slip angle, fading to x1 at
            // NormSlip1 or once the road wheels steer past SteerAngle0..1 (degrees). A straight-line stability aid.
            let [rear, n0, n1, a0, a1] = d.steer.fix_listing;
            let (lo1, hi1) = (n0.min(n1), n0.max(n1));
            let ts = ((sigma_y - lo1) / (hi1 - lo1).max(1e-4)).clamp(0.0, 1.0);
            let a = rear + (1.0 - rear) * ts;
            let (lo2, hi2) = (a0.min(a1).to_radians(), a0.max(a1).to_radians());
            let u = ((steer.abs() - lo2) / (hi2 - lo2).max(1e-6)).clamp(0.0, 1.0);
            fy *= a + u * (1.0 - a);
            // Surface RearGripMultiplier, both axes (8.3 step 11).
            fx *= surf.rear_grip;
            fy *= surf.rear_grip;
            // Handbrake grip (handbrake wheels = the rears): fades in from 10 m/s to 20 m/s.
            let g = ((self.speed() - 10.0) * 0.1).clamp(0.0, 1.0);
            let k = 1.0 - g * (1.0 - surf.handbrake_grip) * handbrake.clamp(0.0, 1.0);
            fx *= k;
            fy *= k;
        }

        // Rolling resistance and scrub (82D36DF8): on the unclamped load, without the chassis / surface scales.
        let ramp = |x: f32| (x.abs() * 0.25).min(1.0);
        let sign = |x: f32| if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { 0.0 };
        let rolling_x = -sign(t.v_long) * ramp(t.v_long) * surf.veldep(t.v_long, surf.veldep_long) * sx_scale * t.load;
        let rolling_y = -sign(t.v_lat) * ramp(t.v_lat) * surf.veldep(t.v_lat, surf.veldep_lat) * self.torque_free.lat * t.load;

        // Our sign convention: slip_ratio + when the wheel outruns the road, slip angle signed like v_lat.
        let ds = -sign(s);
        let dl = if t.v_lat < 0.0 { -1.0 } else { 1.0 };
        TyreOut {
            fx,
            fy,
            rolling_x,
            rolling_y,
            slip_ratio: kappa * ds,
            slip_angle_deg: angle_deg * dl,
            sigma_x: sigma_x * ds,
            sigma_y: sigma_y * dl,
            long_scale: sx_scale,
            rho,
        }
    }

    /// SlidingInstability (82D2BBB0, once per tick before the integration; off in the stats harness): while a tyre
    /// slides (low-passed ρ from 1.05 to 2.0) its load multiplier (wheel+0x78, applied to the suspension force, so to
    /// both the body and the tyre) alternates between 1 + 1.1·a and 1 - 0.9·a, a = 0.12 x that ramp, every half period;
    /// the period shortens from 0.45 s to 0.15 s with the slide and is randomised ±5 %. Live (Pinyon Viper): wheel+0x78
    /// 0.892..1.132, the toggle at timer ≥ period / 2, amplitude 0.12·clamp((ρ − 1.05) / 0.95) on asphalt and dirt.
    /// Multiplies `wheels[i].load` (call after the suspension pass); `grip[i]` = the surface FrictionScale, None airborne.
    pub(super) fn sliding_instability(&mut self, dt: f32, grip: [Option<f32>; 4]) {
        if !sliding_instability_enabled() || self.test_mode {
            for w in &mut self.wheels {
                w.instability.offset = 0.0;
            }
            return;
        }
        for i in 0..4 {
            let x = self.wheels[i].instability.rho;
            let t = ((x - INSTABILITY_FRICTION[0]) / (INSTABILITY_FRICTION[1] - INSTABILITY_FRICTION[0])).clamp(0.0, 1.0);
            let amp = match grip[i] {
                Some(g) if g >= INSTABILITY_MIN_FRICTION => INSTABILITY_SCALE * t,
                _ => 0.0,
            };
            let r = self.next_random();
            let s = &mut self.wheels[i].instability;
            if s.timer >= s.period * 0.5 {
                s.timer = 0.0;
                if s.sign > 0.0 {
                    s.sign = INSTABILITY_SIGN[1];
                } else {
                    s.sign = INSTABILITY_SIGN[0];
                    let d = INSTABILITY_DURATION[0] + (INSTABILITY_DURATION[1] - INSTABILITY_DURATION[0]) * t;
                    s.period = d * (1.0 + INSTABILITY_DURATION_RAND * (2.0 * r - 1.0));
                }
            }
            s.timer += dt;
            s.offset = s.sign * amp;
            self.wheels[i].load *= 1.0 + s.offset;
        }
    }

    /// Feed this tick's ρ into the instability's low-pass (0 when airborne).
    pub(super) fn filter_rho(&mut self, i: usize, rho: f32, dt: f32) {
        let k = 1.0 - (-dt / RHO_FILTER_TAU).exp();
        let s = &mut self.wheels[i].instability;
        s.rho += (rho - s.rho) * k;
    }

    /// Uniform random in [0, 1) (xorshift32; the game uses its own generator at car+0x5010).
    fn next_random(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// AffectCurveSpeedAffectFriction at contact speed `v` (m/s): the condition scales wheel+0x60 / +0x64 = chassis scale x
    /// this (VERIFIED live, Pinyon Viper: 4.3k ticks below 1.5 m/s, inverse of the curve vs |v| correlation 0.999, both axes
    /// equal; 1.0 above). Normal play only: the stats harness (test mode) is left as before (INFERRED; car+0x182C ≠ 0 skips
    /// the condition terms, writer not traced). `FH1_SPEED_GRIP=0` = off.
    fn speed_grip(&self, v: f32) -> f32 {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if self.test_mode || !*ON.get_or_init(|| std::env::var("FH1_SPEED_GRIP").map_or(true, |x| x != "0")) {
            return 1.0;
        }
        let g = &self.data.tyre.speed_grip;
        if g.n == 0 || v >= g.hi {
            return 1.0;
        }
        crate::data::sample(&g.samples[..g.n], (v - g.lo) / (g.hi - g.lo).max(1e-3))
    }

    /// Peak slip ratio of wheel `i` at its load, on the stretched slip axis (for the stats-harness clamp before the force
    /// evaluation).
    pub(super) fn peak_slip_ratio_at(&self, i: usize, load: f32) -> f32 {
        let c = &self.data.longitudinal;
        c.peak_slip_at(c.load_frac(c.clamp_load(load))) * self.data.tyre.slip_scale[i / 2]
    }
}
