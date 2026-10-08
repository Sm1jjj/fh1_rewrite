//! FH1's Performance Index, class, display PI and the five car ratings, recomputed from a car's (upgraded) physics.json
//! (docs/PI.md). Port of the verified scratch model (re/out/pi/lap.py + final.py): on the 176 disc cars it gives PI
//! median error -0.0005, mean |error| 0.0018 (without the 3 ShowCase cars), class 170/176.
//!
//! - PI (default.xex 82BF2C40): a simulated lap of the virtual track in `PI.xml` with an analytic model of the car, best
//!   of three runs (aero tune sliders default / 0 / 1), lap time mapped linearly between Min/MaxPITimeSeconds.
//! - Class (82BE0310) / display PI (82BE0378): CarClasses thresholds.
//! - Ratings (82BF3680): averages of the capacity functions over fixed speed bands, mapped to 3..10.
//! - The game does NOT re-run the timed stats harness on upgrade: the Sim* times stay stock (82546650, flag 0).
//!
//! Inputs: the car's physics.json AFTER the Customize patch (ui/customize_upgrades.rs `patch`); the game's fitting
//! rules (one aspiration, tyre sizes at the stock diameter, part sums) are applied here as `CarData::load_with` does.
//! Setup data: `<assets>/upgrades/pi.json` (PI.xml) and `<assets>/upgrades/car_classes.json` (fh1setup upgrades-3+).
//! Internals are f64 (the game mixes f32 state with a double lap time; the fit was made in f64).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::data::{fit_tyre_sizes, keep_one_aspiration, part_sums, upgrade_rules_on};

const G: f64 = 9.80665;
/// kgf at 150 mph -> k in F = k v² (82D33188).
const KGF_150: f64 = G / (67.056 * 67.056);
const RPM: f64 = std::f64::consts::PI / 30.0;
const MPH: f64 = 0.44704;

/// Speed table: 26 rows from 0 to 250 mph (832A1F78 x 0.04 step, 831BC8F0).
const TABLE_TOP: f64 = 111.815186;
const TABLE_STEP: f64 = TABLE_TOP * 0.04;
/// Lap integration step (8213C85C).
const LAP_DT: f64 = 0.2;
/// Corner entry speed = sqrt(0.95) x the radius-limited speed (8213C960).
const CORNER_FRACTION: f64 = 0.95;

/// One CarClasses row.
#[derive(Debug, Clone)]
pub struct CarClass {
    pub max_pi: f64,
    pub max_display: u32,
    /// "F", "E", ... "R1", "U" (from BadgeTexturePathPrefix `CLASS_<letter>`).
    pub letter: String,
}

/// PI.xml (physics.zip) plus the CarClasses table.
#[derive(Debug, Clone)]
pub struct PiConfig {
    pub min_time: f64,
    pub max_time: f64,
    pub track_width: f64,
    /// AccelFrictionScaleWhileCornering / WhileStraight / LapTimeScale, indexed FWD, RWD, AWD.
    pub corner_scale: [f64; 3],
    pub straight_scale: [f64; 3],
    pub lap_scale: [f64; 3],
    /// PI gearbox (RedlineSpeedMPH per gear): used by the game only for a flagged build (INFERRED: adjustable gearbox);
    /// stock and shop cars use their own gears, so it is kept for reference.
    pub gears_mph: Vec<f64>,
    /// Track segments: (straight m, corner radius m, corner angle degrees).
    pub track: Vec<(f64, f64, f64)>,
    pub classes: Vec<CarClass>,
}

impl PiConfig {
    /// Reads `<assets>/upgrades/pi.json` and `<assets>/upgrades/car_classes.json` (fh1setup `upgrades` group).
    pub fn load(assets: &Path) -> Option<Self> {
        let read = |name: &str| -> Option<Value> { serde_json::from_slice(&std::fs::read(assets.join("upgrades").join(name)).ok()?).ok() };
        let pi = read("pi.json")?;
        let classes = read("car_classes.json")?;
        Self::from_json(&pi, &classes)
    }

    pub fn from_json(pi: &Value, classes: &Value) -> Option<Self> {
        let num = |v: &Value, k: &str| v[k].as_f64();
        let three = |k: &str| -> Option<[f64; 3]> {
            let a = pi[k].as_array()?;
            Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?, a.get(2)?.as_f64()?])
        };
        let track: Vec<(f64, f64, f64)> = pi["track"]
            .as_array()?
            .iter()
            .filter_map(|s| Some((num(s, "straight")?, num(s, "radius")?, num(s, "angle")?)))
            .collect();
        let mut classes: Vec<CarClass> = classes
            .as_array()?
            .iter()
            .filter_map(|c| {
                Some(CarClass {
                    max_pi: num(c, "max_pi")?,
                    max_display: num(c, "max_display")? as u32,
                    letter: c["letter"].as_str().unwrap_or("?").to_owned(),
                })
            })
            .collect();
        classes.sort_by(|a, b| a.max_pi.total_cmp(&b.max_pi));
        (!track.is_empty() && !classes.is_empty()).then_some(Self {
            min_time: num(pi, "min_time")?,
            max_time: num(pi, "max_time")?,
            track_width: num(pi, "track_width")?,
            corner_scale: three("corner_scale")?,
            straight_scale: three("straight_scale")?,
            lap_scale: three("lap_scale")?,
            gears_mph: pi["gears_mph"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default(),
            track,
            classes,
        })
    }
}

/// The calculator's output.
#[derive(Debug, Clone)]
pub struct PiResult {
    /// Raw PI, 0..1 (Data_Car.PerformanceIndex).
    pub pi: f32,
    /// CarClasses row index (Data_Car.ClassID).
    pub class_index: usize,
    pub class_letter: String,
    /// The 100..999 number the game shows.
    pub display_pi: u32,
    /// Speed, handling, acceleration, launch, braking (3..10).
    pub ratings: [f32; 5],
    /// Best virtual lap time (s).
    pub lap_time: f32,
}

/// Class index from raw PI (82BE0310): the first row with PI <= MaxPerformanceIndex, PI clamped to [0, 1]. VERIFIED
/// 176/176 on Data_Car.
pub fn class_index(pi: f64, cfg: &PiConfig) -> usize {
    let p = pi.clamp(0.0, 1.0);
    cfg.classes.iter().position(|c| p <= c.max_pi).unwrap_or(cfg.classes.len().saturating_sub(1))
}

/// Display PI (82BE0378): class 0 shows its MaxDisplay (99); class i >= 1 maps linearly onto
/// [MaxDisplay(i-1) + 1, MaxDisplay(i)] with truncation: trunc(t x (MaxDisplay(i) + 1 - lo) + lo). VERIFIED (code).
pub fn display_pi(pi: f64, cfg: &PiConfig) -> u32 {
    let p = pi.clamp(0.0, 1.0);
    let c = &cfg.classes;
    if c.is_empty() {
        return 0;
    }
    let i = class_index(p, cfg);
    if i == 0 {
        // trunc(p / max0 x (d0 + 1 - 100) + 100), clamped to [100, d0] -> d0 when d0 < 100.
        let d0 = c[0].max_display as f64;
        let v = (p / c[0].max_pi.max(1e-9) * (d0 + 1.0 - 100.0) + 100.0) as i64;
        return if v < 100 { 100 } else { v.min(d0 as i64) as u32 };
    }
    let lo = c[i - 1].max_display as i64 + 1;
    let hi = c[i].max_display as i64;
    let t = (p - c[i - 1].max_pi) / (c[i].max_pi - c[i - 1].max_pi).max(1e-12);
    let v = (t * ((hi + 1 - lo) as f64) + lo as f64) as i64;
    if v < lo { lo as u32 } else { v.min(hi) as u32 }
}

// ---------------------------------------------------------------------------------------------------------------------
// The car model (car-physics vtable 0x8223759C, docs/PI.md section 2).

/// A friction table's per-row peaks (82D0F810 / 82D0F860: linear in load between the two rows, clamped).
#[derive(Debug, Clone, Copy)]
struct Peaks {
    loads: [f64; 2],
    mu: [f64; 2],
    /// Peak slip per row in the table's units (degrees for the lateral table).
    slip: [f64; 2],
}

impl Peaks {
    fn from_json(v: &Value) -> Result<Self> {
        let n = |k: &str| v[k].as_f64().with_context(|| format!("friction table: {k}"));
        let max_slip = n("MaxSlip")?;
        let mut mu = [0.0; 2];
        let mut slip = [0.0; 2];
        for r in 0..2 {
            let c = &v["curves"][r];
            let scale = c["friction_scale"].as_f64().context("friction_scale")?;
            let s: Vec<f64> = c["samples"].as_array().context("samples")?.iter().filter_map(Value::as_f64).collect();
            let mut best = 0usize;
            for (i, &x) in s.iter().enumerate() {
                if x > s[best] {
                    best = i;
                }
            }
            mu[r] = scale * s.get(best).copied().unwrap_or(0.0);
            slip[r] = max_slip * best as f64 / (s.len().max(2) - 1) as f64;
        }
        Ok(Self { loads: [n("MinLoadCurve")? * G, n("MaxLoadCurve")? * G], mu, slip })
    }

    fn t(&self, load: f64) -> f64 {
        ((load - self.loads[0]) / (self.loads[1] - self.loads[0]).max(1e-3)).clamp(0.0, 1.0)
    }

    fn mu_at(&self, load: f64) -> f64 {
        self.mu[0] + (self.mu[1] - self.mu[0]) * self.t(load)
    }

    fn slip_at(&self, load: f64) -> f64 {
        self.slip[0] + (self.slip[1] - self.slip[0]) * self.t(load)
    }
}

/// The analytic car the calculator drives.
#[derive(Debug, Clone)]
struct Model {
    mass: f64,
    fw: f64,
    cg_h: f64,
    track: [f64; 2],
    wheelbase: f64,
    /// TireFricScale(width) per axle (baked into the game's tables, 82D127A0).
    width_scale: [f64; 2],
    chassis_lat: [f64; 2],
    chassis_long: [f64; 2],
    /// Sidewall slip-axis scale per axle (82D12968; data.rs `slip_axis_scale`).
    slip_scale: [f64; 2],
    tf_lat: f64,
    tf_brake: f64,
    /// Driving TorqueFree scale at car+0x1720 when the calculator runs: Accel1 fits (INFERRED).
    tf_accel: f64,
    brake_scale: f64,
    lat: Peaks,
    lon: Peaks,
    drag_k: f64,
    downforce_k: [f64; 2],
    radius: [f64; 2],
    /// 1 FWD, 2 RWD, 3 AWD.
    drive: i64,
    torque_scale: f64,
    gears: Vec<f64>,
    final_drive: f64,
    redline_w: f64,
    rev_limit_w: f64,
    shift_time: f64,
    /// Full-throttle torque table (N·m) over [0, table_wmax], incl. S and boost (82D31CB8).
    torque: Vec<f64>,
    table_wmax: f64,
    i_engine: f64,
    i_trans: f64,
    i_driveline: f64,
    /// Wheel inertia used by +0xA8: the compound MomentInertia (1.6) fits; the live wheel+0x36C is 1.82 (INFERRED).
    i_wheel: f64,
}

fn num(v: &Value, k: &str) -> Result<f64> {
    v[k].as_f64().with_context(|| format!("missing number {k}"))
}

fn lerp_clamped(x: f64, x0: f64, x1: f64, y0: f64, y1: f64) -> f64 {
    let (lo, hi) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
    if x <= lo {
        if x0 <= x1 { y0 } else { y1 }
    } else if x >= hi {
        if x0 <= x1 { y1 } else { y0 }
    } else {
        y0 + (x - x0) / (x1 - x0) * (y1 - y0)
    }
}

/// Turbo / supercharger torque drop-off (82D31CB8): b = 1 + (b - 1) x lerp(Scale0, Scale1) between RPM0..1.
fn drop_off(b: f64, row: &Value, w: f64) -> f64 {
    if b <= 1.0 {
        return b;
    }
    let g = |k: &str, d: f64| row[k].as_f64().unwrap_or(d);
    let (r0, r1) = (g("TorqueDropOffRPM0", 1e6) * RPM, g("TorqueDropOffRPM1", 2e6) * RPM);
    let (s0, s1) = (g("TorqueDropOffScale0", 1.0) * (b - 1.0) + 1.0, g("TorqueDropOffScale1", 1.0) * (b - 1.0) + 1.0);
    lerp_clamped(w, r0, r1, s0, s1)
}

impl Model {
    /// From a physics.json with the fitting rules already applied; `slider` = aero tune slider override (None = the
    /// parts' DefaultTuneSlider).
    fn new(p: &Value, slider: Option<f64>) -> Result<Self> {
        let car = &p["car"];
        let parts = &p["stock_parts"];
        let rules = upgrade_rules_on();
        let (mass_diff, dist_diff, drag_scale, s_engine, ics) = if rules { part_sums(parts) } else { (0.0, 0.0, 1.0, 1.0, 1.0) };
        let (mass_diff, dist_diff, drag_scale, s_engine, ics) = (mass_diff as f64, dist_diff as f64, drag_scale as f64, s_engine as f64, ics as f64);
        let weight = &parts["List_UpgradeCarBodyWeight"];
        let fw = num(weight, "CMBackFront")? + dist_diff;
        let fw = if rules { fw.clamp(0.01, 0.99) } else { fw };
        let body = &p["body"];
        let compound = &p["tires"]["compound"];
        let c = |k: &str, d: f64| compound[k].as_f64().unwrap_or(d);
        let width_scale = |mm: f64| {
            let (w0, w1, s0, s1) = (c("TireFricWidth0", 150.0), c("TireFricWidth1", 650.0), c("TireFricScale0", 1.0), c("TireFricScale1", 1.0));
            s0 + (s1 - s0) * ((mm - w0) / (w1 - w0).max(1.0)).clamp(0.0, 1.0)
        };
        let width = [num(car, "FrontTireWidthMM")?, num(car, "RearTireWidthMM")?];
        let aspect = [num(car, "FrontTireAspect")?, num(car, "RearTireAspect")?];
        let rim = [num(car, "FrontWheelDiameterIN")?, num(car, "RearWheelDiameterIN")?];
        let radius = [0, 1].map(|a| width[a] * 0.001 * aspect[a] * 0.01 + rim[a] * 0.0127);
        let chassis = &parts["List_UpgradeCarBodyChassisStiffness"];
        let ch = |k: &str| chassis[k].as_f64().unwrap_or(1.0);
        let lat = Peaks::from_json(&p["tires"]["friction_lateral"])?;
        let lon = Peaks::from_json(&p["tires"]["friction_longitudinal"])?;
        // Sidewall slip-axis scale (82D12968), with the heavy-load peak held inside 4..18 deg.
        let slip_scale = [0, 1].map(|a| {
            let sidewall = width[a] * aspect[a] * 1e-5;
            let t = ((sidewall - 0.0781) / (0.1143 - 0.0781)).clamp(0.0, 1.0);
            let s = 1.0 + 0.5 * t;
            let heavy = lat.slip[1] * s;
            if heavy > 18.0 {
                s * 18.0 / heavy
            } else if heavy > 0.0 && heavy < 4.0 {
                s * 4.0 / heavy
            } else {
                s
            }
        });

        // Aero (82D33188 + the bumper / wing elements at the tune slider).
        let element = |v: &Value| -> (f64, f64) {
            let g = |k: &str| v[k].as_f64().unwrap_or(0.0);
            let s = slider.unwrap_or_else(|| v["DefaultTuneSlider"].as_f64().unwrap_or(0.5));
            (g("Drag0") + (g("Drag1") - g("Drag0")) * s, (g("Downforce0") + (g("Downforce1") - g("Downforce0")) * s).max(0.0))
        };
        let (drag_f, df_f) = element(&p["aero"]["front_bumper"]);
        let (drag_r, df_r) = element(&p["aero"]["rear_wing"]);
        let gds = car["GameDragScale"].as_f64().unwrap_or(1.0).clamp(0.5, 1.5);
        let drag_k = (num(car, "BodyAeroLongitudinalDrag")? * gds * drag_scale + drag_f + drag_r) * KGF_150;
        let downforce_k = [
            (num(car, "BodyAeroForwardDownforceFront")?.max(0.0) + df_f) * KGF_150,
            (num(car, "BodyAeroForwardDownforceRear")?.max(0.0) + df_r) * KGF_150,
        ];

        let trans = &parts["List_UpgradeDrivetrainTransmission"];
        let gears: Vec<f64> = (1..10).filter_map(|i| trans[format!("GearRatio{i}")].as_f64()).filter(|&g| g > 0.0).collect();
        anyhow::ensure!(!gears.is_empty(), "no forward gears");
        let cam = &parts["List_UpgradeEngineCamshaft"];
        let tc = &p["torque_curve"];
        let redline_rpm = num(cam, "RedlineRPM")?;
        let tc_max_rpm = cam["TorqueCurveMaxRPM"].as_f64().or_else(|| tc["max_rpm"].as_f64()).context("TorqueCurveMaxRPM")?;
        let clutch = &parts["List_UpgradeDrivetrainClutch"];
        let shift_time = 0.5 * (clutch["ClutchInTime"].as_f64().unwrap_or(0.0) + clutch["ClutchOutTime"].as_f64().unwrap_or(0.0))
            + trans["GearShiftTime"].as_f64().unwrap_or(0.0);

        // Torque table (82D31CB8): base x S, plus (b - 1) per boost system (turbo b over the previous sample's power in hp,
        // supercharger b over rpm / redline), each with its drop-off. VERIFIED: peaks = SimPeakTorque on 174/176 cars.
        let samples: Vec<f64> = tc["samples"].as_array().context("torque samples")?.iter().filter_map(Value::as_f64).collect();
        anyhow::ensure!(samples.len() >= 2, "torque curve too short");
        let base = num(tc, "torque_scale_nm")? * s_engine;
        let table_wmax = num(tc, "max_rpm")? * RPM;
        let n = samples.len();
        let turbo = ["List_UpgradeEngineTurboSingle", "List_UpgradeEngineTurboTwin", "List_UpgradeEngineTurboQuad"]
            .iter()
            .find_map(|t| parts.get(*t).filter(|r| r.get("MaxScale").is_some()));
        let sc = ["List_UpgradeEngineCSC", "List_UpgradeEngineDSC"].iter().find_map(|t| parts.get(*t).filter(|r| r.get("RedlineRPMScale").is_some()));
        let red_w = redline_rpm * RPM;
        let mut torque = Vec::with_capacity(n);
        let mut prev_power = 0.0;
        for (i, &x) in samples.iter().enumerate() {
            let w = i as f64 / (n - 1) as f64 * table_wmax;
            let mut f = 1.0;
            if let Some(t) = turbo {
                let g = |k: &str, d: f64| t[k].as_f64().unwrap_or(d);
                let mn = (g("MinScale", 1.0) - 1.0 + s_engine) / s_engine;
                let mx = (g("MaxScale", 1.0) - 1.0 + ics - 1.0 + s_engine) / s_engine;
                let b = lerp_clamped(prev_power / 745.7, g("PowerMinScale", 0.0), g("PowerMaxScale", 1.0), mn, mx);
                f += drop_off(b, t, w) - 1.0;
            }
            if let Some(t) = sc {
                let g = |k: &str, d: f64| t[k].as_f64().unwrap_or(d);
                let z = (g("ZeroRPMScale", 1.0) - 1.0 + s_engine) / s_engine;
                let r = (g("RedlineRPMScale", 1.0) - 1.0 + ics - 1.0 + s_engine) / s_engine;
                let b = z + (w.min(red_w) / red_w.max(1e-6)) * (r - z);
                f += drop_off(b, t, w) - 1.0;
            }
            let tq = base * x * f;
            torque.push(tq);
            prev_power = tq * w;
        }

        let mi = |v: &Value| v["MomentInertia"].as_f64().unwrap_or(0.0);
        Ok(Self {
            mass: num(weight, "Mass")? + mass_diff,
            fw,
            cg_h: num(weight, "CMHeight")?,
            track: [
                num(body, "ModelFrontTrackOuter")? - width[0] * 0.001,
                num(body, "ModelRearTrackOuter")? - width[1] * 0.001,
            ],
            wheelbase: num(body, "ModelWheelbase")?,
            width_scale: [width_scale(width[0]), width_scale(width[1])],
            chassis_lat: [ch("FrontLatFrictionScale"), ch("RearLatFrictionScale")],
            chassis_long: [ch("FrontLongFrictionScale"), ch("RearLongFrictionScale")],
            slip_scale,
            tf_lat: c("TorqueFreeLatFrictionScale", 1.0),
            tf_brake: c("TorqueFreeLongFrictionScaleBrake", 1.0),
            tf_accel: c("TorqueFreeLongFrictionScaleAccel1", 1.0),
            brake_scale: parts["List_UpgradeBrakes"]["GameFrictionScaleBraking"].as_f64().unwrap_or(1.0),
            lat,
            lon,
            drag_k,
            downforce_k,
            radius,
            drive: car["DriveTypeID"].as_i64().unwrap_or(2),
            torque_scale: car["GameTorqueScale"].as_f64().unwrap_or(1.0).clamp(0.5, 1.5),
            gears,
            final_drive: num(trans, "FinalDriveRatio")?,
            redline_w: red_w,
            rev_limit_w: (redline_rpm + tc_max_rpm) * 0.5 * RPM,
            shift_time,
            torque,
            table_wmax,
            i_engine: mi(&p["engine"]) + mi(&parts["List_UpgradeEngineFlywheel"]),
            i_trans: mi(trans),
            i_driveline: mi(&parts["List_UpgradeDrivetrainDriveline"]),
            i_wheel: compound["MomentInertia"].as_f64().unwrap_or(1.82),
        })
    }

    /// Driven tyre radius (rear for RWD, front for FWD / AWD: the first driven wheel, car+0x14AC).
    fn driven_radius(&self) -> f64 {
        if self.drive == 2 { self.radius[1] } else { self.radius[0] }
    }

    /// Highest table sample the calculator reads: round(redline / spacing) (82D31CB8 count); torque above is held.
    fn table_cap_index(&self) -> usize {
        let n = self.torque.len();
        let sp = self.table_wmax / (n - 1) as f64;
        ((self.redline_w / sp + 0.5) as usize).min(n - 1)
    }

    /// Full-throttle torque (N·m) at ω (rad/s), linear in the table, ω held at the redline sample.
    fn torque_at(&self, w: f64) -> f64 {
        let t = &self.torque;
        let n = t.len();
        let sp = self.table_wmax / (n - 1) as f64;
        let cap = (self.table_cap_index() as f64 * sp).min(self.table_wmax);
        let x = w.clamp(0.0, cap) / self.table_wmax * (n - 1) as f64;
        let i = (x as usize).min(n - 2);
        t[i] + (t[i + 1] - t[i]) * (x - i as f64)
    }

    /// Peak power (W) of the table up to the redline sample (= SimPeakPower x 100 on 173/176 cars).
    fn peak_power(&self) -> f64 {
        let n = self.torque.len();
        let sp = self.table_wmax / (n - 1) as f64;
        (0..=self.table_cap_index()).map(|i| self.torque[i] * i as f64 * sp).fold(0.0, f64::max)
    }

    fn axle_loads(&self, v: f64) -> [f64; 2] {
        let w = self.mass * G;
        [(w * self.fw + self.downforce_k[0] * v * v).max(0.0), (w * (1.0 - self.fw) + self.downforce_k[1] * v * v).max(0.0)]
    }

    /// Top speed (vtable +0x90, 82D32268): min(rev limit in top gear, (P_peak x GameTorqueScale / k)^(1/3)). VERIFIED.
    fn top_speed(&self) -> f64 {
        let top = *self.gears.last().unwrap_or(&1.0);
        let v_gear = self.rev_limit_w * self.driven_radius() / (top * self.final_drive);
        let v_power = (self.peak_power() * self.torque_scale / self.drag_k.max(1e-6)).cbrt();
        v_gear.min(v_power)
    }

    /// Lateral capacity (vtable +0xA0, 82D242B8): per axle 6 passes of left/right transfer, each wheel at its peak μ.
    fn lateral_capacity(&self, v: f64) -> f64 {
        let ax = self.axle_loads(v);
        let mut total = 0.0;
        for a in 0..2 {
            let w = ax[a];
            let (mut inner, mut outer) = (0.5 * w, 0.5 * w);
            let (mut mi, mut mo) = (0.0, 0.0);
            for _ in 0..6 {
                mi = self.lat.mu_at(inner) * self.width_scale[a] * self.chassis_lat[a];
                mo = self.lat.mu_at(outer) * self.width_scale[a] * self.chassis_lat[a];
                let shift = (inner * mi + outer * mo) * self.cg_h / self.track[a].max(0.5);
                inner = (0.5 * w - shift).max(0.0);
                outer = (0.5 * w + shift).min(w);
            }
            total += inner * mi + outer * mo;
        }
        total * self.tf_lat / self.mass
    }

    /// Cornering lateral accel (vtable +0xA4, 82D253E0): +0xA0 per wheel with cos(peak slip angle) and the rear force
    /// limited to front x (1 - fw) / fw; also returns rear used / rear capacity.
    fn corner_lateral(&self, v: f64) -> (f64, f64) {
        let ax = self.axle_loads(v);
        let mut n = [0.5 * ax[0], 0.5 * ax[0], 0.5 * ax[1], 0.5 * ax[1]];
        let mu = |load: f64, a: usize| {
            self.lat.mu_at(load) * self.width_scale[a] * self.chassis_lat[a] * (self.lat.slip_at(load) * self.slip_scale[a]).to_radians().cos()
        };
        let (mut ff, mut fr, mut fr_raw) = (0.0, 0.0, 0.0);
        for _ in 0..6 {
            ff = n[0] * mu(n[0], 0) + n[1] * mu(n[1], 0);
            fr_raw = n[2] * mu(n[2], 1) + n[3] * mu(n[3], 1);
            fr = fr_raw.min(ff * (1.0 - self.fw) / self.fw.max(0.001));
            let df = self.cg_h * ff / self.track[0].max(0.5);
            let dr = self.cg_h * fr / self.track[1].max(0.5);
            n = [(0.5 * ax[0] - df).max(0.0), (0.5 * ax[0] + df).min(ax[0]), (0.5 * ax[1] - dr).max(0.0), (0.5 * ax[1] + dr).min(ax[1])];
        }
        ((ff + fr) * self.tf_lat / self.mass, fr / fr_raw.max(1e-4))
    }

    /// Braking capacity incl. aero drag (vtable +0x9C with the drag flag, 82D244B8).
    fn braking(&self, v: f64) -> f64 {
        let b = self.axle_loads(v);
        let tot = b[0] + b[1];
        let mut front = b[0];
        let axle = |n: f64, a: usize| n * self.lon.mu_at(0.5 * n) * self.width_scale[a] * self.chassis_long[a];
        let (mut ff, mut fr) = (0.0, 0.0);
        for _ in 0..6 {
            ff = axle(front, 0);
            fr = axle(tot - front, 1);
            front = (b[0] + (ff + fr) * self.cg_h / self.wheelbase).clamp(0.0, tot);
        }
        (ff + fr) * self.brake_scale * self.tf_brake / self.mass + self.drag_k * v * v / self.mass
    }

    /// Traction-limited drive force split shared by +0xA8 / +0xAC: 6 passes of front/rear transfer.
    fn traction(&self, v: f64, drive: f64, frac: [f64; 2]) -> f64 {
        let b = self.axle_loads(v);
        let tot = b[0] + b[1];
        let (mut nf, mut nr) = (b[0], b[1]);
        let s = self.tf_accel;
        let (mut f_f, mut f_r) = (0.0, 0.0);
        for _ in 0..6 {
            let cf = s * self.lon.mu_at(0.5 * nf) * self.width_scale[0] * self.chassis_long[0] * nf * frac[0];
            let cr = s * self.lon.mu_at(0.5 * nr) * self.width_scale[1] * self.chassis_long[1] * nr * frac[1];
            match self.drive {
                1 => {
                    f_f = cf.min(drive / self.radius[0]);
                    f_r = 0.0;
                }
                2 => {
                    f_f = 0.0;
                    f_r = cr.min(drive / self.radius[1]);
                }
                _ => {
                    let cap = self.radius[0] * cf + self.radius[1] * cr;
                    let k = drive.min(cap) / cap.max(1e-5);
                    f_f = cf * k;
                    f_r = cr * k;
                }
            }
            nr = (b[1] + self.cg_h * (f_f + f_r) / s.max(1e-6) / self.wheelbase).clamp(0.0, tot);
            nf = tot - nr;
        }
        (f_f + f_r - self.drag_k * v * v) / self.mass
    }

    /// Ratings drive accel (vtable +0xAC, 82D24A00): gear by redline, no inertia, full friction.
    fn rating_accel(&self, v: f64) -> f64 {
        let rd = self.driven_radius();
        let Some(g) = self.gears.iter().copied().find(|&g| v < self.redline_w * rd / (self.final_drive * g)) else { return 0.0 };
        let w = g * self.final_drive * v / rd;
        let drive = self.torque_scale * g * self.final_drive * self.torque_at(w);
        self.traction(v, drive, [1.0, 1.0])
    }

    /// Lap drive accel (vtable +0xA8, 82D24E78): v >= 5 mph, the top gear runs to the rev limit (flag, fits better),
    /// rotating inertia m v² / (m v² + I ω²), axle friction fractions.
    fn lap_accel(&self, v: f64, frac: [f64; 2], gears: &[f64]) -> f64 {
        let v = v.max(2.2352);
        let rd = self.driven_radius();
        let last = gears.len().saturating_sub(1);
        let Some(g) = gears.iter().enumerate().find_map(|(i, &g)| {
            let lim = if i == last { self.rev_limit_w } else { self.redline_w };
            (v < lim * rd / (self.final_drive * g)).then_some(g)
        }) else {
            return 0.0;
        };
        let w = g * self.final_drive * v / rd;
        let mut drive = self.torque_scale * g * self.final_drive * self.torque_at(w);
        let ieq = self.i_engine
            + (self.i_trans + self.i_driveline) / (g * g)
            + 2.0 * self.i_wheel * (v / (self.radius[1] * w)).powi(2)
            + 2.0 * self.i_wheel * (v / (self.radius[0] * w)).powi(2);
        let x = self.mass * v * v / (ieq * w * w).max(1e-9);
        drive *= x / (x + 1.0);
        self.traction(v, drive, frac)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Track and lap.

#[derive(Debug, Clone, Copy)]
struct Segment {
    straight: f64,
    radius: f64,
    /// Entry half: R x angle / 2 (the centreline half arc).
    half_arc: f64,
    /// Exit half: the racing-line spiral's length.
    spiral: f64,
    /// Spiral exponent (curvature k(x) = (0.001 - 1/R) x^p + 1/R).
    p: f64,
    /// How far the spiral runs past the corner's centreline end (shortens the next straight).
    ext: f64,
}

/// Racing line through one corner (82BDEC90): spiral from the apex to the exit, exponent bisected 10 times so its
/// lateral offset equals the track width. VERIFIED by the fit (the other sincos convention fails).
fn segment_geometry(straight: f64, r: f64, deg: f64, width: f64) -> Segment {
    let th = (deg * 0.017453292).abs();
    let half = th * 0.5;
    let (sa, ca) = (std::f64::consts::FRAC_PI_2 - half).sin_cos();
    let a = (ca, sa);
    let (sb, cb) = (std::f64::consts::PI - half).sin_cos();
    let b = (-sa, ca);
    let c = ((cb + 1.0) * r, sb * r);
    let k0 = 1.0 / r;
    let k_lin = 0.001 - k0;
    let (mut p, mut hi, mut lo) = (1.0f64, -1.0f64, 0.0f64);
    let (mut length, mut ext) = (0.0f64, 0.0f64);
    for _ in 0..10 {
        let sum: f64 = (0..40).map(|i| (i as f64 / 39.0).powf(p) * k_lin + k0).sum();
        let ds = half / sum;
        let (mut phi, mut px, mut py) = (0.0f64, 0.0f64, 0.0f64);
        length = 0.0;
        for i in 0..40 {
            let k = (i as f64 / 39.0).powf(p) * k_lin + k0;
            let rr = 1.0 / k;
            let dth = k * ds;
            length += dth * rr;
            let (t100, t96) = (std::f64::consts::FRAC_PI_2 - phi).sin_cos();
            let (sd, cd) = dth.sin_cos();
            phi += dth;
            let d = (t100, -t96);
            let e = (t96, t100);
            px += rr * (e.0 * sd + d.0 * (1.0 - cd));
            py += rr * (e.1 * sd + d.1 * (1.0 - cd));
        }
        ext = (a.0 * px + a.1 * py) - (a.0 * c.0 + a.1 * c.1);
        let off = (b.0 * px + b.1 * py) - (b.0 * c.0 + b.1 * c.1);
        if off > width {
            lo = p;
            p = if hi >= 0.0 { (hi + p) * 0.5 } else { p * 2.0 };
        } else if off < width {
            hi = p;
            p = (lo + p) * 0.5;
        } else {
            break;
        }
    }
    Segment { straight, radius: r, half_arc: r * th * 0.5, spiral: length, p, ext }
}

/// All corners, then each straight shortened by the previous corner's extension, min 50 m (82BF3180).
fn build_track(cfg: &PiConfig) -> Vec<Segment> {
    let mut segs: Vec<Segment> = cfg.track.iter().map(|&(l, r, d)| segment_geometry(l, r, d, cfg.track_width)).collect();
    let n = segs.len();
    let ext: Vec<f64> = segs.iter().map(|s| s.ext).collect();
    for (i, s) in segs.iter_mut().enumerate() {
        s.straight = (s.straight - ext[(i + n - 1) % n]).max(50.0);
    }
    segs
}

#[derive(Debug, Clone, Copy, Default)]
struct Row {
    v: f64,
    r_min: f64,
    rear_ratio: f64,
    dt: f64,
    dist: f64,
    accel: f64,
}

/// The lap state (82BE9658 / 82BE0E68).
struct Lap<'a> {
    m: &'a Model,
    segs: &'a [Segment],
    corner_scale: f64,
    shift_time: f64,
    shifts: Vec<f64>,
    coast: Vec<f64>,
    gears: Vec<f64>,
    rows: [Row; 26],
    corner_speed: Vec<f64>,
    v: f64,
    t: f64,
    gear: usize,
    shifting: bool,
    timer: f64,
}

impl<'a> Lap<'a> {
    fn new(m: &'a Model, segs: &'a [Segment], corner_scale: f64, straight_scale: f64, shifts: Vec<f64>, gears: Vec<f64>) -> Self {
        let mut rows = [Row::default(); 26];
        let mut cum = 0.0;
        for (i, row) in rows.iter_mut().enumerate() {
            let v = i as f64 * TABLE_STEP;
            let (a, ratio) = m.corner_lateral(v);
            let b = m.braking(v).max(0.001);
            let (dt, dd) = if i == 0 { (0.0, 0.0) } else { (TABLE_STEP / b, (v + 0.5 * TABLE_STEP) * TABLE_STEP / b) };
            cum += dd;
            *row = Row { v, r_min: v * v / a.max(0.001), rear_ratio: ratio, dt, dist: cum, accel: m.lap_accel(v, [straight_scale; 2], &gears) };
        }
        let coast = shifts.iter().map(|s| -(m.drag_k * s * s) / m.mass).collect();
        let mut lap = Self {
            m,
            segs,
            corner_scale,
            shift_time: m.shift_time,
            shifts,
            coast,
            gears,
            rows,
            corner_speed: Vec::new(),
            v: 0.0,
            t: 0.0,
            gear: 0,
            shifting: false,
            timer: 0.0,
        };
        let speeds: Vec<f64> = segs.iter().map(|s| lap.speed_for_radius(s.radius)).collect();
        lap.corner_speed = speeds;
        lap
    }

    fn index(v: f64) -> (usize, f64) {
        let x = v.clamp(0.0, TABLE_STEP * 25.0) / TABLE_STEP;
        let i = (x as usize).min(24);
        (i, x - i as f64)
    }

    fn interp(&self, v: f64, f: impl Fn(&Row) -> f64) -> f64 {
        let (i, t) = Self::index(v);
        let (a, b) = (f(&self.rows[i]), f(&self.rows[i + 1]));
        a + (b - a) * t
    }

    /// Speed whose minimum corner radius is `r` (82BDE9B0).
    fn speed_for_radius(&self, r: f64) -> f64 {
        let t = &self.rows;
        for i in (1..26).rev() {
            if (r - t[i].r_min) * (r - t[i - 1].r_min) <= 0.0 {
                let (a, b) = (t[i - 1], t[i]);
                if a.r_min == b.r_min {
                    return b.v;
                }
                return a.v + (b.v - a.v) * (r - a.r_min) / (b.r_min - a.r_min);
            }
        }
        if r < t[25].r_min { t[0].v } else { t[25].v }
    }

    fn brake_distance(&self, v: f64, target: f64) -> f64 {
        if v <= target { 0.0 } else { self.interp(v, |r| r.dist) - self.interp(target, |r| r.dist) }
    }

    fn brake_time(&self, v: f64, target: f64) -> f64 {
        if v <= target {
            return 0.0;
        }
        let cum = |x: f64| {
            let (i, f) = Self::index(x);
            (1..=i).map(|k| self.rows[k].dt).sum::<f64>() + self.rows[i + 1].dt * f
        };
        cum(v) - cum(target)
    }

    fn gear_for(&self, v: f64) -> usize {
        let mut g = 0;
        while g < self.shifts.len() && v >= self.shifts[g] {
            g += 1;
        }
        g
    }

    /// (dt, accel) while a gear change runs (coasting on drag), or None.
    fn shift_check(&mut self) -> Option<(f64, f64)> {
        if self.shifting {
            return Some((self.timer, self.coast[self.gear.min(self.coast.len().saturating_sub(1))]));
        }
        if self.gear < self.shifts.len() && self.v > self.shifts[self.gear] && self.shifts.len() >= 2 {
            self.shifting = true;
            self.timer = self.shift_time;
            return Some((self.timer, self.coast[self.gear]));
        }
        None
    }

    fn shift_tick(&mut self, dt: f64) {
        if self.shifting {
            self.timer -= dt;
            if self.timer <= 1e-5 {
                self.timer = 0.0;
                self.shifting = false;
                self.gear += 1;
            }
        }
    }

    fn run(&mut self) -> f64 {
        self.v = CORNER_FRACTION.sqrt() * self.corner_speed.last().copied().unwrap_or(0.0);
        self.t = 0.0;
        self.gear = self.gear_for(self.v);
        self.shifting = false;
        self.timer = 0.0;
        for i in 0..self.segs.len() {
            self.straight(i);
            self.corner(i);
        }
        self.t
    }

    /// Straight (82BE0538): 0.2 s steps, braking point found by bisecting the step (10 iterations).
    fn straight(&mut self, i: usize) {
        let len = self.segs[i].straight;
        let target = CORNER_FRACTION.sqrt() * self.corner_speed[i];
        let mut pos = 0.0;
        loop {
            let (p0, v0, t0) = (pos, self.v, self.t);
            let sc = self.shift_check();
            let (mut dt, a) = match sc {
                Some(x) => x,
                None => (LAP_DT, self.interp(self.v, |r| r.accel)),
            };
            let dv = dt * a;
            self.t += dt;
            pos += (dv * 0.5 + self.v) * dt;
            let v_new = self.v + dv;
            self.v = v_new;
            let mut done = false;
            if pos > len {
                let ex = pos - len;
                pos = len;
                done = true;
                let back = if v_new > 0.0 { (ex / v_new).min(dt) } else { dt };
                self.v = v_new - back * a;
                self.t -= back;
                dt -= back;
            }
            self.shift_tick(dt);
            if self.brake_distance(self.v, target) > len - pos {
                let (mut lo, mut hi) = (0.0, sc.map_or(LAP_DT, |s| s.0));
                let (mut v1, mut t1) = (v0, t0);
                for _ in 0..10 {
                    let tm = (hi + lo) * 0.5;
                    v1 = v0 + a * tm;
                    let p1 = p0 + (0.5 * a * tm + v0) * tm;
                    t1 = t0 + tm;
                    if self.brake_distance(v1, target) > len - p1 {
                        hi = tm;
                    } else {
                        lo = tm;
                    }
                }
                self.t = t1 + self.brake_time(v1, target);
                self.shifting = false;
                self.timer = 0.0;
                self.v = target;
                self.gear = self.gear_for(self.v);
                break;
            }
            if done {
                break;
            }
        }
        self.v = self.v.min(target);
    }

    /// Corner (82BE07F0): entry half at sqrt(0.95) x the apex speed, exit half along the spiral with the
    /// friction-circle accel.
    fn corner(&mut self, i: usize) {
        let s = self.segs[i];
        let mut vmax = self.corner_speed[i];
        let mut target = CORNER_FRACTION.sqrt() * vmax;
        self.v = self.v.min(target);
        let mut first = true;
        let mut pos = 0.0;
        let mut end = s.half_arc;
        loop {
            if !first {
                let x = pos / s.spiral;
                let k = (0.001 - 1.0 / s.radius) * x.powf(s.p) + 1.0 / s.radius;
                vmax = self.speed_for_radius(1.0 / k);
                target = vmax;
            }
            let (mut dt, mut a) = match self.shift_check() {
                Some(x) => x,
                None if first && self.v >= target => ((end - pos) / self.v.max(1e-6) * 1.001, 0.0),
                None => (LAP_DT, self.corner_accel(self.v, self.v * self.v / (vmax * vmax))),
            };
            if self.v + a * dt > target {
                a = (target - self.v).max(0.0) / dt.max(1e-4);
            }
            let dv = a * dt;
            self.t += dt;
            pos += (0.5 * dv + self.v) * dt;
            self.v = (self.v + dv).min(target);
            let mut cont = true;
            if pos >= end {
                let ex = pos - end;
                let step = (0.5 * dv + (self.v - dv)) * dt;
                cont = first;
                first = false;
                pos = 0.0;
                let back = (ex / step.max(1e-9)).clamp(0.0, 1.0) * dt;
                self.v -= back * a;
                self.t -= back;
                dt -= back;
                end = s.spiral;
            }
            self.shift_tick(dt);
            if !cont {
                break;
            }
        }
    }

    /// Accel while cornering at lateral usage u (82BDEA80): front fraction sqrt(1 - u²) - (1 - CornerScale), rear the
    /// same with u x the rear-usage ratio.
    fn corner_accel(&self, v: f64, u: f64) -> f64 {
        let u = u.clamp(0.0, 1.0);
        let ur = (self.interp(v, |r| r.rear_ratio) * u).clamp(0.0, 1.0);
        let cs = 1.0 - self.corner_scale;
        let ff = ((1.0 - u * u).sqrt() - cs).max(0.0);
        let fr = ((1.0 - ur * ur).sqrt() - cs).max(0.0);
        self.m.lap_accel(v, [ff, fr], &self.gears).max(0.0)
    }
}

/// One lap with the given aero slider; returns the lap time (x LapTimeScale).
fn lap_time(p: &Value, cfg: &PiConfig, segs: &[Segment], slider: Option<f64>) -> Result<f64> {
    let m = Model::new(p, slider)?;
    let d = match m.drive {
        1 => 0,
        3 => 2,
        _ => 1,
    };
    let rd = m.driven_radius();
    let mut gears = m.gears.clone();
    let mut shifts: Vec<f64> = gears.iter().map(|g| rd * m.redline_w / (g * m.final_drive)).collect();
    // 82BF2C40: a car whose top gear tops out below 150 mph gets an extra gear (one ported, docs/PI.md open item 2).
    let top = rd * m.rev_limit_w / (gears.last().copied().unwrap_or(1.0) * m.final_drive);
    if top < 67.0582 {
        let half = (67.0784 - top) * 0.5;
        let v_new = if half < 6.7056 { 67.0784 } else { top + half };
        let g = rd * m.rev_limit_w / (v_new * m.final_drive);
        gears.push(g);
        shifts.push(rd * m.redline_w / (g * m.final_drive));
    }
    shifts.pop();
    let mut lap = Lap::new(&m, segs, cfg.corner_scale[d], cfg.straight_scale[d], shifts, gears);
    Ok(lap.run() * cfg.lap_scale[d])
}

fn rating_linear(x: f64, lo: f64, hi: f64) -> f64 {
    (3.0 + 7.0 * (x - lo) / (hi - lo)).clamp(3.0, 10.0)
}

fn rating_log(x: f64, a: f64, b: f64) -> f64 {
    let (lo, hi) = (a.min(b).ln(), a.max(b).ln());
    let l = x.max(0.01).ln();
    (3.0 + 7.0 * (l - lo) / (hi - lo)).clamp(3.0, 10.0)
}

/// Mean of `f` over `n` speeds from `a` to `b` mph, endpoints included (82BE1048 / 82BE1160 / 82BE0F28).
fn band(n: usize, a: f64, b: f64, f: impl Fn(f64) -> f64) -> f64 {
    (0..n).map(|i| f((a + (b - a) * i as f64 / (n - 1) as f64) * MPH)).sum::<f64>() / n as f64
}

/// PI, class, display PI, ratings and lap time for a car. `physics` = its physics.json after the Customize patch; the
/// game's fitting rules are applied here (on a copy) as `CarData::load_with` does.
pub fn compute(physics: &Value, cfg: &PiConfig) -> Result<PiResult> {
    let mut p = physics.clone();
    if upgrade_rules_on() {
        keep_one_aspiration(&mut p["stock_parts"]);
        fit_tyre_sizes(&mut p);
    }
    let segs = build_track(cfg);
    // Best of three runs: aero tune sliders default / 0 / 1 (82BF2C40; INFERRED meaning of the tuning fields).
    let mut best = f64::INFINITY;
    for slider in [None, Some(0.0), Some(1.0)] {
        best = best.min(lap_time(&p, cfg, &segs, slider)?);
    }
    let modifier = p["car"]["CarClassModifier"].as_f64().unwrap_or(0.0);
    let t = best.clamp(cfg.min_time, cfg.max_time);
    let pi = (modifier + (1.0 - modifier) * (cfg.max_time - t) / (cfg.max_time - cfg.min_time)).clamp(0.0, 1.0);

    // Ratings (82BF3680) from the default-slider model.
    let m = Model::new(&p, None)?;
    let ratings = [
        rating_linear(m.top_speed(), 58.0, 102.0),
        rating_log(band(8, 50.0, 120.0, |v| m.lateral_capacity(v)), 19.7, 8.876),
        rating_log(band(13, 40.0, 80.0, |v| m.rating_accel(v)), 11.99, 1.36),
        rating_log(band(13, 20.0, 40.0, |v| m.rating_accel(v)), 13.65, 2.924),
        rating_log(band(8, 50.0, 120.0, |v| m.braking(v)), 22.54, 10.134),
    ]
    .map(|r| r as f32);
    let class_index = class_index(pi, cfg);
    Ok(PiResult {
        pi: pi as f32,
        class_index,
        class_letter: cfg.classes.get(class_index).map(|c| c.letter.clone()).unwrap_or_default(),
        display_pi: display_pi(pi, cfg),
        ratings,
        lap_time: best as f32,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::data::private_assets;

    /// Every installed disc car's stock PI against Data_Car (physics.json `car` block). Needs the converted install
    /// (cars + upgrades groups); skips otherwise. `cargo test --release -p fh1-engine pi:: -- --nocapture`.
    #[test]
    fn pi_matches_gamedb() {
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let Ok(assets) = private_assets(&data) else {
            eprintln!("pi_matches_gamedb: no install, skipped");
            return;
        };
        let Some(cfg) = PiConfig::load(&assets) else {
            eprintln!("pi_matches_gamedb: no upgrades/pi.json / car_classes.json (re-run fh1setup upgrades), skipped");
            return;
        };
        let Ok(dir) = std::fs::read_dir(assets.join("cars")) else {
            eprintln!("pi_matches_gamedb: no cars, skipped");
            return;
        };
        let mut errors = Vec::new();
        let (mut cars, mut class_hits, mut failed) = (0usize, 0usize, 0usize);
        let mut names: Vec<PathBuf> = dir.filter_map(|e| e.ok().map(|e| e.path())).collect();
        names.sort();
        for path in names {
            let Ok(bytes) = std::fs::read(path.join("physics.json")) else { continue };
            let Ok(p) = serde_json::from_slice::<Value>(&bytes) else { continue };
            let (Some(want), Some(class)) = (p["car"]["PerformanceIndex"].as_f64(), p["car"]["ClassID"].as_i64()) else { continue };
            cars += 1;
            match compute(&p, &cfg) {
                Ok(r) => {
                    let e = r.pi as f64 - want;
                    errors.push(e);
                    if r.class_index as i64 == class {
                        class_hits += 1;
                    }
                    if e.abs() > 0.005 {
                        eprintln!("{:<28} pi {:.4} want {:.4} ({:+.4}) class {} / {}", path.file_name().unwrap_or_default().to_string_lossy(), r.pi, want, e, r.class_index, class);
                    }
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("{}: {e:#}", path.display());
                }
            }
        }
        if cars == 0 {
            eprintln!("pi_matches_gamedb: no disc cars, skipped");
            return;
        }
        errors.sort_by(f64::total_cmp);
        let median = errors.get(errors.len() / 2).copied().unwrap_or(0.0);
        let mean = errors.iter().map(|e| e.abs()).sum::<f64>() / errors.len().max(1) as f64;
        let within = errors.iter().filter(|e| e.abs() < 0.005).count();
        eprintln!("pi_matches_gamedb: {cars} cars, {failed} failed, median {median:+.5}, mean |e| {mean:.5}, {within} within 0.005, class {class_hits}/{cars}");
        assert!(class_hits >= 165, "class hits {class_hits}/{cars}");
    }

    #[test]
    fn display_pi_bounds() {
        let classes: Vec<CarClass> = [(0.005, 99), (0.459, 200), (0.539, 300), (0.999999999999, 999), (1.0, 999)]
            .iter()
            .map(|&(m, d)| CarClass { max_pi: m, max_display: d, letter: String::new() })
            .collect();
        let cfg = PiConfig {
            min_time: 138.24,
            max_time: 279.9,
            track_width: 9.0,
            corner_scale: [1.0; 3],
            straight_scale: [1.0; 3],
            lap_scale: [1.0; 3],
            gears_mph: Vec::new(),
            track: Vec::new(),
            classes,
        };
        assert_eq!(display_pi(0.0, &cfg), 99);
        assert_eq!(display_pi(0.459, &cfg), 200);
        assert_eq!(display_pi(0.4591, &cfg), 201);
        assert_eq!(display_pi(0.539, &cfg), 300);
    }
}
