//! Car data from the setup tool's output (`physics.json` + `model.json`), converted to SI units.
//!
//! Unit conversions marked UNVERIFIED are inferred from plausibility (e.g. natural frequencies,
//! damping ratios, top speed) and still need checking against the original game (Xenia) or
//! `default.xex`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

/// Locates the active installation's `assets/private` directory.
pub fn private_assets(data_dir: &Path) -> Result<PathBuf> {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(data_dir.join("installation.json"))
            .with_context(|| format!("no installation in {} — run fh1setup first", data_dir.display()))?,
    )?;
    let id = manifest["id"].as_str().context("installation.json: id")?;
    Ok(data_dir.join("installations").join(id).join("assets").join("private"))
}

/// gamedb aero columns are kilogram-force at 150 mph (default.xex 82D33188). Converts to k in F = k v² (SI).
pub const KGF_AT_150MPH: f32 = 9.806_65 / (67.056 * 67.056);

/// Rev limiter / auto-upshift point relative to the camshaft's RedlineRPM. MEASURED in the real game (Xenia,
/// VIP_Viper_13, docs/PHYSICS_PARITY.md section 7): the engine bounces off 7,104 rpm with RedlineRPM 6,200. One car so far,
/// so it's provisional: the game's rule (scale vs offset) isn't decoded.
pub const REV_LIMIT_SCALE: f32 = 1.146;

#[derive(Debug, Clone)]
pub struct FrictionCurve {
    /// Slip at the last sample (degrees for lateral, slip ratio for longitudinal — UNVERIFIED).
    pub max_slip: f32,
    /// Loads (N) at which `curves[0]` and `curves[1]` apply.
    pub loads: [f32; 2],
    /// (friction scale, normalised samples over 0..=max_slip)
    pub curves: [(f32, Vec<f32>); 2],
    /// Upper load bound (N) of the force evaluation: min(load where load x mu peaks, LoadClamp), tyre table +0xC
    /// (82D127A0; docs/HANDLING_PARITY.md section 8.2).
    pub load_limit: f32,
    /// Per row, the "arcade" layer B (82D0F8B0): the samples with a limited post-peak drop, B[0] = A[0],
    /// B[i] = max(A[i], B[i-1] - 1e-5 / FrictionScale), i.e. the curve holding its peak. Blended in by the surface's
    /// MinimumArcadeGripValue (docs/HANDLING_PARITY.md 2.2, 8.3 step 8).
    pub arcade: [Vec<f32>; 2],
}

impl FrictionCurve {
    fn from_json(v: &Value) -> Result<Self> {
        let kgf = 9.80665;
        let curve = |i: usize| -> Result<(f32, Vec<f32>)> {
            let c = &v["curves"][i];
            Ok((
                f(c, "friction_scale")?,
                c["samples"].as_array().context("samples")?.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect(),
            ))
        };
        let loads = [f(v, "MinLoadCurve")? * kgf, f(v, "MaxLoadCurve")? * kgf];
        let curves = [curve(0)?, curve(1)?];
        let load_clamp = f(v, "LoadClamp").unwrap_or(5000.0) * kgf;
        // Load where load x mu(load) peaks with mu linear in load (only when mu falls with load).
        let (s0, s1) = (curves[0].0, curves[1].0);
        let load_limit = if s1 < s0 {
            let l_star = (s0 * (loads[1] - loads[0]) + loads[0] * (s0 - s1)) / (2.0 * (s0 - s1));
            l_star.min(load_clamp)
        } else {
            load_clamp
        };
        let arcade = [0, 1].map(|r| {
            let (scale, a) = (&curves[r].0, &curves[r].1);
            let drop = 1e-5 / scale.max(1e-6);
            let mut b: Vec<f32> = Vec::with_capacity(a.len());
            for (i, &x) in a.iter().enumerate() {
                b.push(if i == 0 { x } else { x.max(b[i - 1] - drop) });
            }
            b
        });
        Ok(Self { max_slip: f(v, "MaxSlip")?, loads, curves, load_limit, arcade })
    }

    /// The game's load clamp for a force evaluation: [MinLoadCurve, load_limit].
    pub fn clamp_load(&self, load: f32) -> f32 {
        let (a, b) = (self.loads[0], self.load_limit);
        load.clamp(a.min(b), a.max(b))
    }

    /// Position of a (clamped) load between the two curves; above MaxLoadCurve it extrapolates, as the game does.
    pub fn load_frac(&self, load: f32) -> f32 {
        (load - self.loads[0]) / (self.loads[1] - self.loads[0]).max(1e-3)
    }

    /// Peak slip of each row (MaxSlip x argmax / (n - 1)), and its friction (= the row's FrictionScale for curves
    /// normalised to peak 1).
    fn row_peak(&self, row: usize) -> (f32, f32) {
        let s = &self.curves[row].1;
        let (i, m) = s.iter().enumerate().fold((0, f32::MIN), |b, (i, &x)| if x > b.1 { (i, x) } else { b });
        (self.max_slip * i as f32 / (s.len().max(2) - 1) as f32, m * self.curves[row].0)
    }

    /// Peak slip at a load fraction (tyre function: row +0x30 interpolated by load, extrapolating like the forces).
    pub fn peak_slip_at(&self, frac: f32) -> f32 {
        let (a, b) = (self.row_peak(0).0, self.row_peak(1).0);
        (a + (b - a) * frac).max(1e-4)
    }

    /// Peak friction at `load` N, load clamped to the two curves (82D0F810, used by the brake and lock tables).
    pub fn peak_mu_at(&self, load: f32) -> f32 {
        let t = self.load_frac(load).clamp(0.0, 1.0);
        let (a, b) = (self.row_peak(0).1, self.row_peak(1).1);
        a + (b - a) * t
    }

    /// Friction at `slip` with the load given as a fraction between the curves (not clamped: the game extrapolates).
    pub fn mu_frac(&self, slip: f32, frac: f32) -> f32 {
        let a = self.curves[0].0 * sample(&self.curves[0].1, slip / self.max_slip);
        let b = self.curves[1].0 * sample(&self.curves[1].1, slip / self.max_slip);
        a + (b - a) * frac
    }

    /// [`Self::mu_frac`] with the arcade layer blended in per row by `w` (μrow = A + (B - A) x w).
    pub fn mu_layered(&self, slip: f32, frac: f32, w: f32) -> f32 {
        if w <= 0.0 {
            return self.mu_frac(slip, frac);
        }
        let x = slip / self.max_slip;
        let row = |r: usize| {
            let a = sample(&self.curves[r].1, x);
            self.curves[r].0 * (a + (sample(&self.arcade[r], x) - a) * w)
        };
        let (a, b) = (row(0), row(1));
        a + (b - a) * frac
    }

    /// Friction coefficient at `slip` (>= 0) under `load` newtons.
    pub fn mu(&self, slip: f32, load: f32) -> f32 {
        let t = ((load - self.loads[0]) / (self.loads[1] - self.loads[0])).clamp(0.0, 1.0);
        let a = self.curves[0].0 * sample(&self.curves[0].1, slip / self.max_slip);
        let b = self.curves[1].0 * sample(&self.curves[1].1, slip / self.max_slip);
        a + (b - a) * t
    }

    /// Slip at which the (low-load) curve peaks.
    pub fn peak_slip(&self) -> f32 {
        let s = &self.curves[0].1;
        let i = s.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|x| x.0).unwrap_or(1).max(1);
        self.max_slip * i as f32 / (s.len() - 1) as f32
    }
}

/// Linear interpolation over uniformly spaced samples, `x` in 0..=1 (clamped).
pub fn sample(s: &[f32], x: f32) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let p = x.clamp(0.0, 1.0) * (s.len() - 1) as f32;
    let i = (p as usize).min(s.len() - 1);
    let j = (i + 1).min(s.len() - 1);
    s[i] + (s[j] - s[i]) * (p - i as f32)
}

#[derive(Debug, Clone, Copy)]
pub struct Suspension {
    /// N/m (gamedb DefSpringRate × car mass kg, verified live on the Viper).
    pub spring: f32,
    /// N·s/m (DefDampenBumpRate/ReboundRate × car mass kg).
    pub bump: f32,
    pub rebound: f32,
    /// m of compression travel above static ride height (MaxCompressHeight).
    pub max_compress: f32,
    /// N/m per metre of left/right compression difference (DefSwaybarStiffness × car mass kg, verified live).
    pub anti_roll: f32,
    /// Damper force clamps (N): DampenBumpClamp / DampenReboundClamp × car mass kg (live Viper wheel+0x344/+0x34C).
    pub bump_clamp: f32,
    pub rebound_clamp: f32,
    /// Bump stop past the top of travel: BumpstopStiffness × m N/m, BumpstopDamping × m N·s/m (wheel+0x354/+0x358).
    pub bumpstop_k: f32,
    pub bumpstop_c: f32,
    /// Anti-roll bar damping (N·s/m) on the left/right chassis-point vertical velocity difference (SwaybarDamping × m).
    pub bar_damping: f32,
    /// Tyre vertical stiffness (N/m, wheel+0x3B0) and the unsprung mass per wheel (kg, wheel+0x370 = 27.2 live).
    pub tyre_k: f32,
    pub unsprung: f32,
}

/// Stock List_UpgradeBrakes columns, raw (vehicle/brakes.rs converts them; docs/HANDLING_PARITY.md §7).
#[derive(Debug, Clone, Copy)]
pub struct BrakeColumns {
    pub torque_slider: f32,
    pub bias_slider: f32,
    pub bias_handbrake: f32,
    pub release_point_ai: f32,
    pub duration_abs: f32,
    /// [front, rear] x [no steer, full steer]
    pub release_point: [[f32; 2]; 2],
    /// Fitted brakes' GameFrictionScaleBraking (car+0x17DC; 82D24150).
    pub game_friction_scale: f32,
}

impl BrakeColumns {
    fn from_json(b: &Value) -> Self {
        let g = |k: &str, d: f32| b[k].as_f64().map_or(d, |x| x as f32);
        Self {
            torque_slider: g("BrakeTorqueSlider", 0.5),
            bias_slider: g("BrakeBiasSlider", 0.5),
            bias_handbrake: g("BiasHandbrake", 1.5),
            release_point_ai: g("ReleasePointABS", 0.71),
            duration_abs: g("DurationABS", 0.05),
            release_point: [
                [g("ReleasePointABSFrontNoSteer", 3.0), g("ReleasePointABSFrontFullSteer", 0.5)],
                [g("ReleasePointABSRearNoSteer", 2.5), g("ReleasePointABSRearFullSteer", 2.0)],
            ],
            game_friction_scale: g("GameFrictionScaleBraking", 1.0),
        }
    }
}

/// Data_Car steering and driver-aid columns, raw units (degrees, deg/s, mph). vehicle/steering.rs converts them
/// (default.xex 82D25C38; docs/HANDLING_PARITY.md §2). Defaults = the most common values.
#[derive(Debug, Clone, Copy)]
pub struct SteerColumns {
    pub max_angle_deg: f32,
    pub max_angle_filtered_deg: f32,
    pub ang_vel_turning: f32,
    pub ang_vel_straighten: f32,
    pub ang_vel_countersteer: f32,
    pub ang_vel_dyn_find_peak: f32,
    pub accel_time_to_max_rate: f32,
    pub ss_max_gees: f32,
    pub ss_min_max_angle_deg: f32,
    pub ss_slow_mph: f32,
    pub ss_fast_mph: f32,
    pub ss_fast_rate_scale: f32,
    /// FixListing RearFricScale, NormSlip0, NormSlip1, SteerAngle0, SteerAngle1.
    pub fix_listing: [f32; 5],
    /// Lock-table grip scale parts: TorqueFreeLatFrictionScale, TireFricWidth0/1 (mm), TireFricScale0/1,
    /// front tyre width (mm), ChassisStiffness Front/RearLatFrictionScale.
    pub torque_free_lat_scale: f32,
    pub tire_fric_width: [f32; 2],
    pub tire_fric_scale: [f32; 2],
    pub front_tire_width_mm: f32,
    pub chassis_lat_scale: [f32; 2],
}

impl SteerColumns {
    fn from_json(p: &Value) -> Self {
        let car = &p["car"];
        let compound = &p["tires"]["compound"];
        let chassis = &p["stock_parts"]["List_UpgradeCarBodyChassisStiffness"];
        let g = |k: &str, d: f32| f(car, k).unwrap_or(d);
        let max = g("SteerMaxAngle", 42.0);
        Self {
            max_angle_deg: max,
            // The loader's own rule (82BE9D30): Filtered <= 0 falls back to SteerMaxAngle.
            max_angle_filtered_deg: f(car, "SteerMaxAngleFiltered").ok().filter(|&x| x > 0.01).unwrap_or(max),
            ang_vel_turning: g("SteerMaxAngVelTurning", 105.0),
            ang_vel_straighten: g("SteerMaxAngVelStraighten", 105.0),
            ang_vel_countersteer: g("SteerAngVelCountersteer", 105.0),
            ang_vel_dyn_find_peak: g("SteerAngVelDynFindPeak", 10.0),
            accel_time_to_max_rate: g("SteerAccelTimeToMaxRate", 0.01),
            ss_max_gees: g("SteerSpeedSensitiveMaxGees", 1.0),
            ss_min_max_angle_deg: g("SteerSpeedSensitiveMinMaxAngle", 5.0),
            ss_slow_mph: g("SteerSpeedSensitiveSlowSpeed", 1.0),
            ss_fast_mph: g("SteerSpeedSensitiveFastSpeed", 120.0),
            ss_fast_rate_scale: g("SteerSpeedSensitiveFastRateScale", 1.0),
            fix_listing: [
                g("FixListingRearFricScale", 1.35),
                g("FixListingNormSlip0", 0.5),
                g("FixListingNormSlip1", 1.0),
                g("FixListingSteerAngle0", 0.5),
                g("FixListingSteerAngle1", 1.0),
            ],
            torque_free_lat_scale: f(compound, "TorqueFreeLatFrictionScale").unwrap_or(1.0),
            tire_fric_width: [f(compound, "TireFricWidth0").unwrap_or(150.0), f(compound, "TireFricWidth1").unwrap_or(650.0)],
            tire_fric_scale: [f(compound, "TireFricScale0").unwrap_or(1.0), f(compound, "TireFricScale1").unwrap_or(1.0)],
            front_tire_width_mm: g("FrontTireWidthMM", 225.0),
            chassis_lat_scale: [f(chassis, "FrontLatFrictionScale").unwrap_or(1.0), f(chassis, "RearLatFrictionScale").unwrap_or(1.0)],
        }
    }
}

/// A compound's TorqueFree friction scales (tyre object +0xC..+0x20, 82D12968; docs/HANDLING_PARITY.md section 8).
/// Normal play multiplies lateral tyre force by `lat`, driving force by lerp(accel) over speed², braking force by
/// `brake` (x GameFrictionScaleBraking). The dev car-stats screen that produced gamedb's Sim* overrides lat, brake and
/// both accel values with 1.0 (82D3B9E8 -> 82D18DB0).
#[derive(Debug, Clone, Copy)]
pub struct TorqueFree {
    pub lat: f32,
    pub brake: f32,
    pub accel: [f32; 2],
    /// m/s; not touched by the harness override.
    pub accel_speed: [f32; 2],
}

impl TorqueFree {
    /// The stats-harness override (dev screen 825CDE58 passes 1.0).
    pub fn overridden(self, value: f32) -> Self {
        Self { lat: value, brake: value, accel: [value; 2], ..self }
    }
}

/// A compound affect curve over [lo, hi] (fixed storage so [`TyreColumns`] stays `Copy`).
#[derive(Debug, Clone, Copy)]
pub struct SpeedGrip {
    pub lo: f32,
    pub hi: f32,
    pub n: usize,
    pub samples: [f32; 32],
}

/// Per-car tyre scales the tyre function uses besides the curves (docs/HANDLING_PARITY.md section 8).
#[derive(Debug, Clone, Copy)]
pub struct TyreColumns {
    pub torque_free: TorqueFree,
    /// TireFricScale(width) per axle: lerp over TireFricWidth0..1 mm -> TireFricScale0..1, clamped. Baked into both
    /// friction tables by the game (82D127A0), so it scales both forces and both peaks.
    pub width_scale: [f32; 2],
    /// ChassisStiffness Front/Rear Lat/LongFrictionScale (wheel+0x3A4 / +0x3A8).
    pub chassis_lat: [f32; 2],
    pub chassis_long: [f32; 2],
    /// Tyre section width per axle (m), Data_Car Front/RearTireWidthMM (skid mark width).
    pub width_m: [f32; 2],
    /// Slip-axis scale per axle (82D12968 -> 82D127A0 f2): both friction tables' slip axes (MaxSlip, so both peaks) are
    /// stretched by it. Sidewall rule x heavy-load peak clamp, see `slip_axis_scale`. 1.0 under FH1_SLIP_AXIS=0.
    pub slip_scale: [f32; 2],
    /// AffectCurveSpeedAffectFriction (compound): grip multiplier on both axes by the contact-point speed |v| (m/s),
    /// (min, max input, samples). 1.22-1.48 at rest -> 1.0 at 1.5 m/s. VERIFIED live (HANDLING_PARITY 8.17).
    pub speed_grip: SpeedGrip,
}

/// PhysicsSettings.ini `SidewallScalePeakSA\*`: sidewall height (m) TireSidewall0..1 -> TireSidewallScalePeakSASR0..1,
/// and TireClampHeavyPeakSALow / High (degrees).
const SIDEWALL_M: [f32; 2] = [0.0781, 0.1143];
const SIDEWALL_SCALE: [f32; 2] = [1.0, 1.5];
const HEAVY_PEAK_SA_DEG: [f32; 2] = [4.0, 18.0];

/// `FH1_SLIP_AXIS=0`: no slip-axis scale (the curves' own peaks, as before 2026-10-06).
pub fn slip_axis_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SLIP_AXIS").map_or(true, |v| v != "0"))
}

/// The game's slip-axis scale for one tyre (82D12968, docs/HANDLING_PARITY.md 8.15). VERIFIED live (Pinyon, VIP_Viper_13
/// 295/30 front, 355/30 rear on asphalt): live peak slip / curve peak = 1.1436 / 1.3923 on both axes, all four wheels.
/// s = lerp(sidewall = width x aspect: 0.0781..0.1143 m -> 1..1.5, clamped) x def+20 (1.0) x StartPressureAffectPeakSASR
/// (1.0 on the capture car). Then the lateral peak at MaxLoadCurve (row 1, x s, degrees) is held inside 4..18 deg by
/// rescaling both tables.
fn slip_axis_scale(width_mm: f32, aspect: f32, lateral: &FrictionCurve) -> f32 {
    let sidewall = width_mm * aspect * 1e-5;
    let t = ((sidewall - SIDEWALL_M[0]) / (SIDEWALL_M[1] - SIDEWALL_M[0])).clamp(0.0, 1.0);
    let s = SIDEWALL_SCALE[0] + (SIDEWALL_SCALE[1] - SIDEWALL_SCALE[0]) * t;
    let heavy = lateral.row_peak(1).0 * s;
    if heavy > HEAVY_PEAK_SA_DEG[1] {
        s * HEAVY_PEAK_SA_DEG[1] / heavy
    } else if heavy < HEAVY_PEAK_SA_DEG[0] && heavy > 0.0 {
        s * HEAVY_PEAK_SA_DEG[0] / heavy
    } else {
        s
    }
}

impl TyreColumns {
    fn from_json(p: &Value, lateral: &FrictionCurve) -> Self {
        let car = &p["car"];
        let compound = &p["tires"]["compound"];
        let chassis = &p["stock_parts"]["List_UpgradeCarBodyChassisStiffness"];
        let c = |k: &str, d: f32| f(compound, k).unwrap_or(d);
        let (w0, w1) = (c("TireFricWidth0", 150.0), c("TireFricWidth1", 650.0));
        let (s0, s1) = (c("TireFricScale0", 1.0), c("TireFricScale1", 1.0));
        let width = |mm: f32| s0 + (s1 - s0) * ((mm - w0) / (w1 - w0).max(1.0)).clamp(0.0, 1.0);
        let front = f(car, "FrontTireWidthMM").unwrap_or(225.0);
        let rear = f(car, "RearTireWidthMM").unwrap_or(front);
        let ch = |k: &str| f(chassis, k).unwrap_or(1.0);
        Self {
            torque_free: TorqueFree {
                lat: c("TorqueFreeLatFrictionScale", 1.0),
                brake: c("TorqueFreeLongFrictionScaleBrake", 1.0),
                accel: [c("TorqueFreeLongFrictionScaleAccel0", 1.0), c("TorqueFreeLongFrictionScaleAccel1", 1.0)],
                accel_speed: [c("TorqueFreeLongFrictionScaleAccelSpeed0", 5.0), c("TorqueFreeLongFrictionScaleAccelSpeed1", 15.0)],
            },
            // Experiment knob: FH1_TYRE_WIDTH=0 drops the width scale (parity decomposition only).
            width_scale: if std::env::var("FH1_TYRE_WIDTH").is_ok_and(|v| v == "0") { [1.0; 2] } else { [width(front), width(rear)] },
            chassis_lat: [ch("FrontLatFrictionScale"), ch("RearLatFrictionScale")],
            chassis_long: [ch("FrontLongFrictionScale"), ch("RearLongFrictionScale")],
            width_m: [front * 0.001, rear * 0.001],
            speed_grip: {
                let c = &p["tires"]["affect_curves"]["SpeedAffectFriction"];
                let mut g = SpeedGrip { lo: f(c, "min_input").unwrap_or(0.0), hi: f(c, "max_input").unwrap_or(1.5), n: 0, samples: [1.0; 32] };
                for (i, x) in c["samples"].as_array().into_iter().flatten().take(32).enumerate() {
                    g.samples[i] = x.as_f64().unwrap_or(1.0) as f32;
                    g.n = i + 1;
                }
                g
            },
            slip_scale: if slip_axis_enabled() {
                let aspect_f = f(car, "FrontTireAspect").unwrap_or(45.0);
                let aspect_r = f(car, "RearTireAspect").unwrap_or(aspect_f);
                [slip_axis_scale(front, aspect_f, lateral), slip_axis_scale(rear, aspect_r, lateral)]
            } else {
                [1.0; 2]
            },
        }
    }
}

/// Forced induction (turbo / supercharger) from the stock `List_UpgradeEngineTurbo*` / `*SC` row.
/// Boost multiplies the torque curve by `min_scale..max_scale` as the engine's unboosted power
/// rises from `power_min_hp` to `power_max_hp`. Fitted against gamedb's SimPeakTorque/SimPeakPower
/// over 68 forced-induction cars: 90% within 7%, with an unexplained +5.7% bias (likely spool lag
/// in Turn 10's test run). `RobScale` is not used yet (UNVERIFIED meaning).
#[derive(Debug, Clone, Copy)]
pub struct Boost {
    pub min_scale: f32,
    pub max_scale: f32,
    pub power_min_hp: f32,
    pub power_max_hp: f32,
    /// Torque drop-off: scale 0 at rpm 0 to scale 1 at rpm 1.
    pub dropoff_rpm: [f32; 2],
    pub dropoff_scale: [f32; 2],
    /// Spool-up time constant (s). STOPGAP: MomentInertia / 30 (parity barely changes from /100 to /15).
    pub spool_time: f32,
}

impl Boost {
    /// Target torque multiplier for `torque` N·m at `rpm` (unboosted).
    pub fn target(&self, torque: f32, rpm: f32) -> f32 {
        let hp = torque * rpm * std::f32::consts::TAU / 60.0 / 745.7;
        let x = ((hp - self.power_min_hp) / (self.power_max_hp - self.power_min_hp)).clamp(0.0, 1.0);
        self.min_scale + (self.max_scale - self.min_scale) * x
    }

    pub fn dropoff(&self, rpm: f32) -> f32 {
        let [r0, r1] = self.dropoff_rpm;
        let t = ((rpm - r0) / (r1 - r0).max(1.0)).clamp(0.0, 1.0);
        self.dropoff_scale[0] + (self.dropoff_scale[1] - self.dropoff_scale[0]) * t
    }
}

// Some fields are only read by the parity tests or reserved for upcoming systems.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CarData {
    pub media_name: String,
    /// Engine sound bank: `sound_donor` (imported cars borrow an FH1 car's sounds) or the car's own MediaName.
    pub sound: String,
    pub display_year: i64,
    pub mass: f32,
    /// Centre of mass height above the ground (m).
    pub cg_height: f32,
    /// Fraction of weight on the front axle (CMBackFront — UNVERIFIED which end).
    pub front_weight: f32,
    /// Box used for the inertia tensor (m).
    pub block_dims: [f32; 3],

    /// Hub positions in model space (+Y up, front -Z, origin = body bottom-centre): LF, RF, LR, RR.
    pub hubs: [[f32; 3]; 4],
    pub wheel_radius_visual: f32,
    pub tyre_radius: [f32; 2],
    pub wheel_inertia: f32,
    pub suspension: [Suspension; 2],

    pub torque_curve: Vec<f32>,
    pub torque_scale: f32,
    /// List_TorqueCurve.ZeroThrottleTorqueScale x GameTorqueScale (N·m): engine drag scale at zero throttle (82D22D58).
    /// Missing (older installs / imported cars): 0.46 x torque_scale, the median ratio over gamedb's 501 curves.
    pub zero_throttle_nm: f32,
    /// Peak-power engine speed (rpm; car+0xFAC = Data_Car.SimPeakAngVel): where the drag shape reaches -1.
    pub peak_power_rpm: f32,
    /// Data_Car.OffRoadEnginePowerScale: full-throttle torque scale with all four tyres on off-road surfaces (car+0x1004).
    pub offroad_power_scale: f32,
    pub torque_curve_max_rpm: f32,
    pub redline_rpm: f32,
    /// Where the fuel cut / auto upshift happen; above the camshaft's dashboard RedlineRPM.
    pub rev_limit_rpm: f32,
    pub idle_rpm: f32,
    /// Camshaft StartRPM (0 when the table has none): the auto-clutch's launch targets (drivetrain.rs `launch_target`).
    pub start_rpm: f32,
    pub engine_inertia: f32,
    /// Stock `List_UpgradeDrivetrainClutch`: torque capacity (N·m) and engagement time (s) when pulling away.
    pub clutch_max_torque: f32,
    pub clutch_out_time: f32,
    pub boost: Option<Boost>,

    /// Forward gear ratios (1st..), reverse ratio (negative), final drive.
    pub gears: Vec<f32>,
    pub reverse: f32,
    pub final_drive: f32,
    pub shift_time: f32,
    /// 1 FWD, 2 RWD, 3 AWD.
    pub drive_type: i64,
    pub rear_torque_split: f32,
    /// Limited-slip differentials: front, rear, centre (stock List_UpgradeDrivetrainDifferential; drivetrain.rs).
    pub diffs: [Lsd; 3],

    pub steer_max_deg: f32,
    pub steer: SteerColumns,
    pub lateral: FrictionCurve,
    pub longitudinal: FrictionCurve,
    pub tyre: TyreColumns,
    /// Stock List_UpgradeBrakes (docs/HANDLING_PARITY.md §7).
    pub brakes: BrakeColumns,

    /// Aerodynamic drag k in F = k v² (N per (m/s)²).
    pub drag_k: f32,
    /// Downforce per axle (front, rear) in F = k v².
    pub downforce_k: [f32; 2],

    /// Body collision spheres (model-space centre, radius) from MAXData.xml.
    pub collision_spheres: Vec<(bevy::math::Vec3, f32)>,
    /// MAXData Flags 2 spheres (model space): the big body shapes used for car-vs-car contact (vehicle/contact.rs).
    pub car_spheres: Vec<(bevy::math::Vec3, f32)>,
    /// gamedb CameraOverrides row, column -> value (empty if the car has none); camera.rs.
    pub camera: std::collections::BTreeMap<String, f32>,
    /// Data_CarBody PristineBoundingBox in model space (+Y up, front -Z, origin = body bottom-centre; gamedb Z
    /// negated): min, max.
    pub bbox: [bevy::math::Vec3; 2],

    /// gamedb's own simulated results, for parity checks.
    pub reference_0_60_s: f32,
    pub reference_top_speed: f32,
}

/// One limited-slip differential (List_UpgradeDrivetrainDifferential `<Axle>LimitedSlip*`, loader 82BEBA10). The live
/// Camaro (Xenia `corrado` capture, car+0x145C..) holds the gamedb values x 0.01 (torques in the game's 100 N·m) and the
/// RelVelClamp x π/30, so gamedb torques are N·m and RelVelClamp is rpm (docs/DRIVETRAIN.md "LSD"). VERIFIED.
#[derive(Debug, Clone, Copy, Default)]
pub struct Lsd {
    /// Lock torque under drive / on the overrun (N·m).
    pub accel: f32,
    pub decel: f32,
    /// Speed difference across the diff at which the lock saturates (rad/s).
    pub rel_vel: f32,
    /// Diff input torque (N·m) at which the lock reaches `accel` (lerp from `decel` at 0).
    pub accel_def_input: f32,
}

impl Lsd {
    fn from_json(diff: &Value, axle: &str) -> Self {
        let g = |k: &str, d: f32| f(diff, &format!("{axle}LimitedSlip{k}")).unwrap_or(d);
        Self {
            accel: g("TorqueAccel", 0.0).max(0.0),
            decel: g("TorqueDecel", 0.0).max(0.0),
            rel_vel: (g("RelVelClamp", 50.0) * std::f32::consts::PI / 30.0).max(1e-3),
            accel_def_input: g("AccelDefInputTorque", 50.0),
        }
    }

    /// Lock torque (N·m) for input torque `t_in` and speed difference `dw` (rad/s, + = first side faster): taken from
    /// the faster side, given to the slower (82D396E0 / 82D39AE0).
    pub fn lock(&self, t_in: f32, dw: f32) -> f32 {
        let l = if t_in <= 0.0 {
            self.decel
        } else if t_in >= self.accel_def_input {
            self.accel
        } else {
            self.decel + (self.accel - self.decel) * t_in / self.accel_def_input
        };
        l * (dw / self.rel_vel).clamp(-1.0, 1.0)
    }
}

/// Unsprung mass per wheel (kg): wheel+0x370 = 0.272 (x 100 kg) on the live Viper; source not traced (same on every
/// wheel, so probably a constant).
pub const UNSPRUNG_KG: f32 = 27.2;
/// Static tyre deflection per metre of sidewall height (fit to the live Viper's wheel+0x3B0, see 8.13).
pub const TYRE_DEFLECTION_PER_SIDEWALL: f32 = 0.0973;

fn f(v: &Value, key: &str) -> Result<f32> {
    v[key].as_f64().map(|x| x as f32).with_context(|| format!("missing number {key}"))
}

impl CarData {
    pub fn load(car_dir: &Path) -> Result<Self> {
        Self::load_with(car_dir, |_| {})
    }

    /// [`Self::load`] with `physics.json` edited first (Customize upgrades, fh1-engine ui/customize_upgrades.rs: part rows
    /// swapped into `stock_parts`, `torque_curve` / `suspension` / `tires` replaced, scales applied).
    pub fn load_with(car_dir: &Path, patch: impl FnOnce(&mut Value)) -> Result<Self> {
        let mut p: Value = serde_json::from_slice(&std::fs::read(car_dir.join("physics.json"))?)?;
        patch(&mut p);
        let m: Value = serde_json::from_slice(&std::fs::read(car_dir.join("model.json"))?)?;
        let car = &p["car"];
        let parts = &p["stock_parts"];
        let weight = &parts["List_UpgradeCarBodyWeight"];
        let cam = &parts["List_UpgradeEngineCamshaft"];
        let trans = &parts["List_UpgradeDrivetrainTransmission"];
        let diff = &parts["List_UpgradeDrivetrainDifferential"];

        let mut hubs = [[0.0f32; 3]; 4];
        for (i, h) in m["hubs"].as_array().context("hubs")?.iter().enumerate().take(4) {
            for k in 0..3 {
                hubs[i][k] = h["position"][k].as_f64().unwrap_or(0.0) as f32;
            }
        }

        let tyre = |w: &str, a: &str, d: &str| -> Result<f32> {
            Ok(f(car, d)? * 0.0254 * 0.5 + f(car, w)? * 0.001 * f(car, a)? * 0.01)
        };

        // Suspension stiffnesses are the gamedb value x the car's mass in kg (springs N/m, dampers N·s/m, bars N/m;
        // 82D2F070 & co., docs/HANDLING_PARITY.md 8.11; live Viper roll stiffness matched to 0.3%). FH1_SUSP_X1000=1 =
        // the old x1000 guess.
        let susp_scale = if std::env::var("FH1_SUSP_X1000").is_ok_and(|v| v == "1") { 1000.0 } else { f(weight, "Mass")? };
        let susp = |end: &str, sway: &str| -> Result<Suspension> {
            let s = &p["suspension"][end];
            Ok(Suspension {
                spring: f(s, "DefSpringRate")? * susp_scale,
                bump: f(s, "DefDampenBumpRate")? * susp_scale,
                rebound: f(s, "DefDampenReboundRate")? * susp_scale,
                max_compress: f(s, "MaxCompressHeight")?,
                anti_roll: f(&p["suspension"][sway], "DefSwaybarStiffness").unwrap_or(0.0) * susp_scale,
                bump_clamp: f(s, "DampenBumpClamp").unwrap_or(10.0) * susp_scale,
                rebound_clamp: f(s, "DampenReboundClamp").unwrap_or(10.0) * susp_scale,
                bumpstop_k: f(s, "BumpstopStiffness").unwrap_or(200.0) * susp_scale,
                bumpstop_c: f(s, "BumpstopDamping").unwrap_or(2.0) * susp_scale,
                bar_damping: f(&p["suspension"][sway], "SwaybarDamping").unwrap_or(0.0) * susp_scale,
                tyre_k: 0.0,
                unsprung: UNSPRUNG_KG,
            })
        };
        // Tyre vertical stiffness k = (corner mass + unsprung) x g / delta (docs/HANDLING_PARITY.md 8.13). The static
        // deflection delta is fitted to the live Viper (front 8.47 mm, rear 10.53 mm) as 0.0973 x sidewall height
        // (+-1.6% on that car): the writer of wheel+0x3B0 and the game's own rule are not located (UNVERIFIED rule).
        let mut suspension = [susp("front", "anti_sway_front")?, susp("rear", "anti_sway_rear")?];
        {
            let share = f(weight, "CMBackFront")?;
            let mass = f(weight, "Mass")?;
            for (axle, end) in ["Front", "Rear"].iter().enumerate() {
                let sidewall = f(car, &format!("{end}TireWidthMM"))? * 0.001 * f(car, &format!("{end}TireAspect"))? * 0.01;
                let corner = mass * if axle == 0 { share } else { 1.0 - share } * 0.5 + UNSPRUNG_KG;
                let delta = (TYRE_DEFLECTION_PER_SIDEWALL * sidewall).max(0.003);
                suspension[axle].tyre_k = corner * 9.81 / delta;
            }
        }

        let boost = ["List_UpgradeEngineTurboSingle", "List_UpgradeEngineTurboTwin", "List_UpgradeEngineTurboQuad", "List_UpgradeEngineCSC", "List_UpgradeEngineDSC"]
            .iter()
            .find_map(|t| parts.get(*t).filter(|v| v.get("MaxScale").is_some()))
            .map(|t| -> Result<Boost> {
                Ok(Boost {
                    min_scale: f(t, "MinScale")?,
                    max_scale: f(t, "MaxScale")?,
                    power_min_hp: f(t, "PowerMinScale")?,
                    power_max_hp: f(t, "PowerMaxScale")?,
                    dropoff_rpm: [f(t, "TorqueDropOffRPM0").unwrap_or(1e6), f(t, "TorqueDropOffRPM1").unwrap_or(2e6)],
                    dropoff_scale: [f(t, "TorqueDropOffScale0").unwrap_or(1.0), f(t, "TorqueDropOffScale1").unwrap_or(1.0)],
                    spool_time: f(t, "MomentInertia").unwrap_or(30.0) / 30.0,
                })
            })
            .transpose()?;
        let tc = &p["torque_curve"];
        let n_gears = trans["NumGears"].as_i64().unwrap_or(6) as usize;
        let gears: Vec<f32> = (1..n_gears)
            .filter_map(|i| trans[format!("GearRatio{i}")].as_f64().map(|x| x as f32))
            .filter(|&r| r > 0.0)
            .collect();

        let top_speed = f(car, "SimTopSpeed")?;

        let bbox = {
            // Already relative to BottomCenterWheelbasePos (= our model origin): no mesh_offset (the game's camera
            // radius for VIP_Viper_13 matches this to 1 cm, docs/CAMERA.md).
            let b = |k: &str| p["body"][k].as_f64().unwrap_or(0.0) as f32;
            [
                bevy::math::Vec3::new(b("PristineBoundingBoxMinX"), b("PristineBoundingBoxMinY"), -b("PristineBoundingBoxMaxZ")),
                bevy::math::Vec3::new(b("PristineBoundingBoxMaxX"), b("PristineBoundingBoxMaxY"), -b("PristineBoundingBoxMinZ")),
            ]
        };
        let lateral = FrictionCurve::from_json(&p["tires"]["friction_lateral"])?;
        Ok(Self {
            media_name: car["MediaName"].as_str().unwrap_or("?").to_owned(),
            sound: p["sound_donor"].as_str().or(car["MediaName"].as_str()).unwrap_or("?").to_owned(),
            display_year: car["Year"].as_i64().unwrap_or(0),
            mass: f(weight, "Mass")?,
            cg_height: f(weight, "CMHeight")?,
            front_weight: f(weight, "CMBackFront")?,
            block_dims: [f(weight, "BlockDimX")?, f(weight, "BlockDimY")?, f(weight, "BlockDimZ")?],
            hubs,
            wheel_radius_visual: m["wheel_radius"].as_f64().unwrap_or(0.33) as f32,
            tyre_radius: [
                tyre("FrontTireWidthMM", "FrontTireAspect", "FrontWheelDiameterIN")?,
                tyre("RearTireWidthMM", "RearTireAspect", "RearWheelDiameterIN")?,
            ],
            // Game rule (82D33530, wheel+0x36C): max(0.5 x (List_Wheels.Mass / 4) x r² x WheelInertiaScale (1) +
            // WheelInertiaAdd (0.25), WheelInertiaMinClamp (1.82)) kg·m² (live Corrado 1.82 on all four wheels). The
            // tyre compound's MomentInertia is not used on this path. One value for all wheels: the larger axle's.
            wheel_inertia: {
                let quarter = f(&p["wheel"], "Mass").unwrap_or(40.0) * 0.25;
                let r = tyre("FrontTireWidthMM", "FrontTireAspect", "FrontWheelDiameterIN")?
                    .max(tyre("RearTireWidthMM", "RearTireAspect", "RearWheelDiameterIN")?);
                (0.5 * quarter * r * r + 0.25).max(1.82)
            },
            suspension,
            torque_curve: tc["samples"].as_array().context("torque samples")?.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect(),
            // default.xex 82BF4FB0 scales by Data_Car.GameTorqueScale clamped to [0.5, 1.5] (docs/PHYSICS_PARITY.md).
            torque_scale: f(tc, "torque_scale_nm")? * f(car, "GameTorqueScale").unwrap_or(1.0).clamp(0.5, 1.5),
            zero_throttle_nm: f(tc, "zero_throttle_nm").unwrap_or_else(|_| 0.46 * f(tc, "torque_scale_nm").unwrap_or(300.0))
                * f(car, "GameTorqueScale").unwrap_or(1.0).clamp(0.5, 1.5),
            peak_power_rpm: f(car, "SimPeakAngVel").map(|w| w * 60.0 / std::f32::consts::TAU).unwrap_or(0.0),
            offroad_power_scale: f(car, "OffRoadEnginePowerScale").unwrap_or(1.0),
            torque_curve_max_rpm: f(tc, "max_rpm")?,
            redline_rpm: f(cam, "RedlineRPM")?,
            rev_limit_rpm: f(cam, "RedlineRPM")? * REV_LIMIT_SCALE,
            idle_rpm: f(cam, "StallRPM")?.max(700.0),
            start_rpm: f(cam, "StartRPM").unwrap_or(0.0),
            engine_inertia: f(&p["engine"], "MomentInertia").unwrap_or(0.15)
                + f(&parts["List_UpgradeEngineFlywheel"], "MomentInertia").unwrap_or(0.1),
            clutch_max_torque: f(&parts["List_UpgradeDrivetrainClutch"], "ClutchMaxTorque").unwrap_or(1000.0),
            clutch_out_time: f(&parts["List_UpgradeDrivetrainClutch"], "ClutchOutTime").unwrap_or(0.3).max(0.01),
            boost,
            gears,
            reverse: trans["GearRatio0"].as_f64().unwrap_or(-3.0) as f32,
            final_drive: f(trans, "FinalDriveRatio")?,
            shift_time: f(trans, "GearShiftTime").unwrap_or(0.2),
            drive_type: car["DriveTypeID"].as_i64().unwrap_or(2),
            rear_torque_split: f(diff, "RearToqueSplit").unwrap_or(0.5),
            diffs: ["Front", "Rear", "Center"].map(|axle| Lsd::from_json(diff, axle)),
            steer_max_deg: f(car, "SteerMaxAngleFiltered").unwrap_or(30.0),
            steer: SteerColumns::from_json(&p),
            brakes: BrakeColumns::from_json(&parts["List_UpgradeBrakes"]),
            tyre: TyreColumns::from_json(&p, &lateral),
            lateral,
            longitudinal: FrictionCurve::from_json(&p["tires"]["friction_longitudinal"])?,
            drag_k: f(car, "BodyAeroLongitudinalDrag")? * KGF_AT_150MPH * f(car, "GameDragScale").unwrap_or(1.0).clamp(0.5, 1.5),
            downforce_k: [
                f(car, "BodyAeroForwardDownforceFront")?.max(0.0) * KGF_AT_150MPH,
                f(car, "BodyAeroForwardDownforceRear")?.max(0.0) * KGF_AT_150MPH,
            ],
            reference_0_60_s: f(car, "SimTimeTo60MPH").unwrap_or(0.0),
            reference_top_speed: top_speed,
            // Ground at rest: hub height minus tyre radius, averaged over the axles.
            collision_spheres: collision_spheres(&p["maxdata"]["Collision"]["CollSpheres"], {
                let tr = [tyre("FrontTireWidthMM", "FrontTireAspect", "FrontWheelDiameterIN")?, tyre("RearTireWidthMM", "RearTireAspect", "RearWheelDiameterIN")?];
                0.5 * ((hubs[0][1] - tr[0]) + (hubs[2][1] - tr[1]))
            }),
            car_spheres: car_spheres(&p["maxdata"]["Collision"]["CollSpheres"], bbox),
            camera: p["camera"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()? as f32))).collect()).unwrap_or_default(),
            bbox,
        })
    }

    /// Radius around the car's position that holds every car-contact sphere (m; a little loose: measured from the model
    /// origin, which is within a metre of the centre of mass).
    pub fn car_spheres_reach(&self) -> f32 {
        self.car_spheres.iter().map(|(c, r)| c.length() + r).fold(0.0, f32::max) + 0.5
    }

    /// Unboosted engine torque (N·m) at full throttle.
    /// The engine's zero-throttle torque a(ω) in N·m (82D22D58; docs/DRIVETRAIN.md "Engine torque", VERIFIED to 0.005 hN·m
    /// on the live Viper): below 800 rpm a small positive idle push (table at 0x82236984), from 800 rpm to peak power a linear
    /// drag 0 -> -1, -1 up to the redline, then -1 -> -2.63 at TorqueCurveMaxRPM; all x ZeroThrottleTorqueScale.
    pub fn engine_drag(&self, rpm: f32) -> f32 {
        const IDLE: [f32; 9] = [0.10, 0.09, 0.08, 0.07, 0.06, 0.05, 0.0385, 0.0192, 0.0];
        let peak = if self.peak_power_rpm > 900.0 { self.peak_power_rpm } else { self.redline_rpm };
        let shape = if rpm < 800.0 {
            sample(&IDLE, rpm.max(0.0) / 800.0)
        } else if rpm < peak {
            -(rpm - 800.0) / (peak - 800.0).max(1.0)
        } else if rpm < self.redline_rpm {
            -1.0
        } else {
            -1.0 - 1.63 * ((rpm - self.redline_rpm) / (self.torque_curve_max_rpm - self.redline_rpm).max(1.0)).min(1.0)
        };
        shape * self.zero_throttle_nm
    }

    pub fn torque_at(&self, rpm: f32) -> f32 {
        sample(&self.torque_curve, rpm / self.torque_curve_max_rpm) * self.torque_scale
    }

    /// Torque at full throttle with fully spooled boost.
    pub fn boosted_torque_at(&self, rpm: f32) -> f32 {
        let t = self.torque_at(rpm);
        match self.boost {
            Some(b) => t * b.target(t, rpm) * b.dropoff(rpm),
            None => t,
        }
    }
}

/// Car-vs-car spheres (vehicle/contact.rs): MAXData spheres with Flags bit 2. `Flags` is a bit mask (VERIFIED on the
/// install: 130 cars use 1 and 2, 44 cars only 3 = 1|2); matching `== 2` left those 44 cars (most traffic cars) with no
/// contact shape. Those 44 carry just four small spheres at the body's lower corners, so when the flagged set doesn't
/// cover the body (OUR rule) the body box is filled with spheres instead: two columns along the length.
fn car_spheres(v: &Value, bbox: [bevy::math::Vec3; 2]) -> Vec<(bevy::math::Vec3, f32)> {
    let num = |o: &Value, k: &str, d: f64| o[k].as_f64().unwrap_or(d) as f32;
    let (pos_scale, rad_scale) = (num(v, "PosScale", 1.0), num(v, "RadiusScale", 1.0));
    let offset = bevy::math::Vec3::new(num(v, "PosOffsetX", 0.0), num(v, "PosOffsetY", 0.0), -num(v, "PosOffsetZ", 0.0));
    let flagged: Vec<(bevy::math::Vec3, f32)> = v
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter(|(k, s)| k.starts_with("Sphere") && s["Flags"].as_f64().is_some_and(|f| (f as u32) & 2 != 0))
                .map(|(_, s)| (offset + bevy::math::Vec3::new(num(s, "PosX", 0.0), num(s, "PosY", 0.0), -num(s, "PosZ", 0.0)) * pos_scale, num(s, "Radius", 0.3) * rad_scale))
                .collect()
        })
        .unwrap_or_default();
    let [a, b] = bbox;
    let size = b - a;
    let covered = flagged.iter().map(|(c, r)| c.y + r).fold(f32::MIN, f32::max) >= a.y + 0.6 * size.y && flagged.len() >= 6;
    if covered || size.z < 1.0 || size.x < 0.5 {
        return flagged;
    }
    let r = (0.5 * size.y).min(0.5 * size.x).clamp(0.35, 0.9);
    let y = a.y + size.y * 0.45;
    let xs = [a.x + r.min(0.5 * size.x), b.x - r.min(0.5 * size.x)];
    let n = ((size.z - 2.0 * r) / r).ceil().max(1.0) as usize;
    let mut out = Vec::new();
    for k in 0..=n {
        let z = a.z + r + (size.z - 2.0 * r) * k as f32 / n as f32;
        for &x in &xs {
            out.push((bevy::math::Vec3::new(x, y, z), r));
        }
    }
    out
}

/// World-contact spheres from MAXData (gamedb space, front = +Z; the model's front is -Z, so flip Z).
fn collision_spheres(v: &Value, ground_y: f32) -> Vec<(bevy::math::Vec3, f32)> {
    let num = |o: &Value, k: &str, d: f64| o[k].as_f64().unwrap_or(d) as f32;
    let (pos_scale, rad_scale) = (num(v, "PosScale", 1.0), num(v, "RadiusScale", 1.0));
    let offset = bevy::math::Vec3::new(num(v, "PosOffsetX", 0.0), num(v, "PosOffsetY", 0.0), -num(v, "PosOffsetZ", 0.0));
    let Some(obj) = v.as_object() else { return Vec::new() };
    obj.iter()
        // Flags 1 = small contact points on the body's extremities (corners, roof, scrape points);
        // flags 2 = large spheres that dip below the body (car-vs-car broad shapes, presumably).
        .filter(|(k, s)| k.starts_with("Sphere") && s["Flags"].as_f64() == Some(1.0))
        .map(|(_, s)| {
            let c = bevy::math::Vec3::new(num(s, "PosX", 0.0), num(s, "PosY", 0.0), -num(s, "PosZ", 0.0));
            (offset + c * pos_scale, num(s, "Radius", 0.3) * rad_scale)
        })
        // FER_F142_10 / FER_458Spider_12 carry contact points below the ground at stock ride height (PosY -0.171):
        // kept, they pin the car (top speed 0.002 m/s). The game drives these cars, so it can't be colliding them.
        .filter(|(c, r)| c.y - r >= ground_y)
        .collect()
}
