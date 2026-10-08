//! parity [--data <dir>] [--csv out.csv] [--no-tcs] [--full-lock] [--old-lat] [--old-harness] [--in-game] [car ...]
//!
//! Runs scripted tests through the vehicle simulation and compares the results with the
//! numbers gamedb stores for each car (Turn 10's own simulation: `Data_Car.Sim*`).
//! With no cars given, runs every car that has a model and prints aggregate error.

use std::path::PathBuf;

use bevy::math::Vec3;
use fh1_engine::data::{private_assets, CarData};
use fh1_engine::vehicle::{Controls, FlatGround, Vehicle};
use serde_json::Value;

const DT: f32 = 1.0 / 480.0;
const MPH: f32 = 0.44704;

/// gamedb reference values and our measurements, same order as `METRICS`.
const METRICS: [&str; 10] = ["0-60 s", "0-100 s", "1/4 mi s", "1/4 mi m/s", "top m/s", "60-0 m", "100-0 m", "lat g 60", "lat g 120", "brake ok"];

/// Cleared by `--no-tcs` (launch tests without traction control; ABS stays on).
static TCS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// Set by `--full-lock`: the lateral test skips the speed-sensitive steering-lock reduction.
static FULL_LOCK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Lateral test: FH1's own (CAutomatedCarStatsImp state 4, default.xex 82D19280) by default; cleared by `--old-lat`
/// (our earlier throttle-held, peak-g test).
static GAME_LAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// Launch + braking: FH1's own harness (CAutomatedCarStatsImp states 1-3) by default; cleared by `--old-harness`
/// (our earlier separate launch with TCS and 0.6 s brake-ramp stops from 60 / 100 mph).
static GAME_HARNESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Set by `--in-game`: keep the compound's TorqueFree tyre scales (normal play) instead of the dev car-stats screen's
/// override to 1.0 (825CDE58 -> 82D3B9E8 -> 82D18DB0), which is how gamedb's Sim* numbers were produced.
static IN_GAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A car as the dev car-stats screen builds it (TorqueFree scales 1.0), unless `--in-game`.
fn new_vehicle(d: CarData) -> Vehicle {
    let mut v = Vehicle::new(d, Vec3::ZERO);
    if !IN_GAME.load(std::sync::atomic::Ordering::Relaxed) {
        v.override_torque_free(1.0);
    }
    v
}

fn assists() -> Controls {
    Controls { tcs: TCS.load(std::sync::atomic::Ordering::Relaxed), abs: true, ..Default::default() }
}

fn settle(v: &mut Vehicle) {
    for _ in 0..480 {
        v.step(assists(), DT, &FlatGround);
    }
}

/// Standing start at full throttle: 0-60, 0-100, quarter mile, top speed.
fn straight(d: &CarData) -> [f32; 5] {
    let mut v = new_vehicle(d.clone());
    settle(&mut v);
    let start = v.position;
    let input = Controls { throttle: 1.0, ..assists() };
    let (mut t, mut t60, mut t100, mut tq, mut vq, mut top) = (0.0, f32::NAN, f32::NAN, f32::NAN, f32::NAN, 0.0f32);
    while t < 150.0 {
        v.step(input, DT, &FlatGround);
        t += DT;
        let s = v.forward_speed();
        top = top.max(s);
        if std::env::var_os("TRACE").is_some() && t < 10.0 && (t * 2.0).fract() < DT * 2.0 {
            let w = &v.wheels;
            eprintln!(
                "t={t:4.1} v={s:5.1} gear={} rpm={:5.0} slip {:+.2} {:+.2} {:+.2} {:+.2} load {:.0} {:.0} {:.0} {:.0}",
                v.gear, v.rpm, w[0].slip_ratio, w[1].slip_ratio, w[2].slip_ratio, w[3].slip_ratio, w[0].load, w[1].load, w[2].load, w[3].load
            );
        }
        if t60.is_nan() && s >= 60.0 * MPH {
            t60 = t;
        }
        if t100.is_nan() && s >= 100.0 * MPH {
            t100 = t;
        }
        if tq.is_nan() && (v.position - start).length() >= 402.336 {
            tq = t;
            vq = s;
        }
    }
    [t60, t100, tq, vq, top]
}

/// Brake from `speed` to rest (ABS on); returns distance. The pedal ramps 0→1 over 0.6 s: gamedb's
/// distances imply ~0.3 s of brake application (median over 166 cars, consistent at 60 and
/// 100 mph), i.e. Turn 10's test isn't an instant full stomp.
fn braking(d: &CarData, speed: f32) -> f32 {
    let mut v = new_vehicle(d.clone());
    settle(&mut v);
    v.velocity = Vec3::NEG_Z * speed;
    for (i, w) in v.wheels.iter_mut().enumerate() {
        w.omega = speed / d.tyre_radius[i / 2];
    }
    v.sync_drivetrain();

    let start = v.position;
    let mut t = 0.0;
    while v.forward_speed() > 0.3 && t < 30.0 {
        let input = Controls { brake: (t / 0.6f32).min(1.0), ..assists() };
        v.step(input, DT, &FlatGround);
        t += DT;
    }
    (v.position - start).reject_from(Vec3::Y).length()
}

/// Constant-speed ramp steer; returns the peak lateral acceleration (g), smoothed over ~0.3 s.
fn lateral(d: &CarData, speed: f32) -> f32 {
    let mut v = new_vehicle(d.clone());
    settle(&mut v);
    v.velocity = Vec3::NEG_Z * speed;
    for (i, w) in v.wheels.iter_mut().enumerate() {
        w.omega = speed / d.tyre_radius[i / 2];
    }
    v.sync_drivetrain();
    v.full_lock = FULL_LOCK.load(std::sync::atomic::Ordering::Relaxed);

    let (mut t, mut smooth, mut best, mut integral) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let ramp = 12.0;
    while t < ramp {
        let err = speed - v.speed();
        integral = (integral + err * DT).clamp(-5.0, 5.0);
        let cmd = 0.6 * err + 0.3 * integral;
        let input = Controls { steer: (t / ramp).min(1.0), throttle: cmd.clamp(0.0, 1.0), brake: (-cmd).clamp(0.0, 1.0), ..assists() };
        v.step(input, DT, &FlatGround);
        t += DT;
        let right = (v.rotation * Vec3::X).reject_from(Vec3::Y).normalize_or_zero();
        let lat = v.acceleration.dot(right).abs() / 9.81;
        smooth += (lat - smooth) * (DT / 0.3);
        // Only count while speed is held (a spinning car isn't cornering).
        if (v.speed() - speed).abs() < 0.1 * speed {
            best = best.max(smooth);
        }
    }
    best
}

/// FH1's lateral test (docs/HANDLING_PARITY.md §1): speed held by rescaling the velocity every tick (no throttle),
/// steering ramped to keep the front tyres' normalised slip angle in [0.98, 1.2], result = time-average |lateral g|
/// while it is in (0.95, 1.2), over 1..20 s.
fn lateral_game(d: &CarData, speed: f32) -> f32 {
    let mut v = new_vehicle(d.clone());
    settle(&mut v);
    v.velocity = Vec3::NEG_Z * speed;
    for (i, w) in v.wheels.iter_mut().enumerate() {
        w.omega = speed / d.tyre_radius[i / 2];
    }
    v.sync_drivetrain();
    v.full_lock = FULL_LOCK.load(std::sync::atomic::Ordering::Relaxed);
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
        v.step(Controls { steer: s, ..assists() }, DT, &FlatGround);
    }
    if time > 0.1 { sum / time / 9.80665 } else { f32::NAN }
}

/// FH1's stats harness (docs/HANDLING_PARITY.md §5, default.xex 82D3B9E8 / 82D18EB8 / 82D295F0 / 82D18F10): test mode
/// (no TCS), auto-clutch rates (ACSGlobalClutchIn/OutTime, ACSGlobalShiftTime 0.05 s); settle 1 s, then full throttle
/// from idle with the clock starting at rollout (> 1 m/s) and top speed = max over the 90 s run; then, from that top
/// speed, one instant full-brake stop with 100-0 / 60-0 measured from the crossings of 100 / 60 mph.
/// Returns [0-60, 0-100, 1/4 s, 1/4 m/s, top, 60-0, 100-0].
///
/// The run (state 2) keeps the car on its line like the game's harness 82D295F0: steer = a saturating function of the
/// body forward axis's sideways component plus one of the sideways position (car+0x70 row 2, car+0xB0), clamped to ±1.
/// The gains are INFERRED (the game's constants aren't read); without it a launch disturbance (EngineTorqueBodyRoll) left
/// the car yawing for the rest of the run. Braking (state 3) is open loop, as in the game.
/// Steering that holds the start line: heading error (rad) and sideways offset (m), both toward the line.
fn hold_line(v: &Vehicle, start: Vec3, line: Vec3) -> f32 {
    let side = line.cross(Vec3::Y); // right of the line
    let fwd = (v.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(line);
    let heading = fwd.dot(side).asin();
    let offset = (v.position - start).dot(side);
    let speed = v.forward_speed().max(5.0);
    // Heading term plus a lateral term that asks for a heading back toward the line within ~1.5 s, scaled down with speed.
    (-(4.0 * heading + 0.5 * offset / speed) * (20.0 / speed).min(1.0)).clamp(-1.0, 1.0)
}

fn harness(d: &CarData) -> [f32; 7] {
    let mut d = d.clone();
    d.clutch_out_time = 0.05;
    d.shift_time = 0.05;
    let assists = Controls { tcs: false, abs: true, ..Default::default() };
    if std::env::var_os("TRACE").is_some() {
        eprintln!("hubs {:?} cg_height {} front_weight {} tyre_radius {:?}", d.hubs, d.cg_height, d.front_weight, d.tyre_radius);
    }
    let mut v = new_vehicle(d);
    v.test_mode = true;
    if std::env::var_os("TRACE").is_some() {
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
        if std::env::var_os("TRACE").is_some() && c < 10.0 && (c * 2.0).fract() < DT * 2.0 {
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
        if std::env::var_os("TRACE").is_some() && s < 30.0 && (t * 10.0).fract() < DT * 10.0 {
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

fn run(d: &CarData) -> [f32; 10] {
    let [t60, t100, tq, vq, top, b60, b100] = if GAME_HARNESS.load(std::sync::atomic::Ordering::Relaxed) {
        harness(d)
    } else {
        let [t60, t100, tq, vq, top] = straight(d);
        [t60, t100, tq, vq, top, braking(d, 60.0 * MPH), braking(d, 100.0 * MPH)]
    };
    let lat = if GAME_LAT.load(std::sync::atomic::Ordering::Relaxed) { lateral_game } else { lateral };
    [t60, t100, tq, vq, top, b60, b100, lat(d, 60.0 * MPH), lat(d, 120.0 * MPH), 1.0]
}

fn reference(car: &Value) -> [f32; 10] {
    let g = |k: &str| car[k].as_f64().unwrap_or(f64::NAN) as f32;
    [
        g("SimTimeTo60MPH"),
        g("SimTimeTo100MPH"),
        g("SimTimeQuarterMile"),
        g("SimSpeedQuarterMile"),
        g("SimTopSpeed"),
        g("SimBrakeDistance60MPH"),
        g("SimBrakeDistance100MPH"),
        g("SimLatGees60MPH"),
        g("SimLatGees120MPH"),
        1.0,
    ]
}

fn main() -> anyhow::Result<()> {
    let mut data = PathBuf::from("data");
    let mut csv = None;
    let mut only = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data" => data = args.next().unwrap().into(),
            "--csv" => csv = args.next(),
            "--no-tcs" => TCS.store(false, std::sync::atomic::Ordering::Relaxed),
            "--full-lock" => FULL_LOCK.store(true, std::sync::atomic::Ordering::Relaxed),
            "--old-lat" => GAME_LAT.store(false, std::sync::atomic::Ordering::Relaxed),
            "--old-harness" => GAME_HARNESS.store(false, std::sync::atomic::Ordering::Relaxed),
            "--in-game" => IN_GAME.store(true, std::sync::atomic::Ordering::Relaxed),
            _ => only.push(a),
        }
    }
    let assets = private_assets(&data)?;
    let index: Value = serde_json::from_slice(&std::fs::read(assets.join("cars/index.json"))?)?;
    let names: Vec<String> = index
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["has_model"].as_bool() == Some(true))
        .filter_map(|c| c["media_name"].as_str().map(str::to_owned))
        .filter(|n| only.is_empty() || only.iter().any(|o| o.eq_ignore_ascii_case(n)))
        .collect();

    let mut rows = Vec::new();
    let mut errors: Vec<Vec<f32>> = vec![Vec::new(); METRICS.len() - 1];
    // Same, split by DriveTypeID (1 FWD, 2 RWD, 3 AWD).
    let mut by_type: Vec<Vec<Vec<f32>>> = vec![vec![Vec::new(); METRICS.len() - 1]; 4];
    for name in &names {
        let dir = assets.join("cars").join(name);
        let Ok(d) = CarData::load(&dir) else {
            eprintln!("{name}: failed to load");
            continue;
        };
        let physics: Value = serde_json::from_slice(&std::fs::read(dir.join("physics.json"))?)?;
        // Traffic-only vehicles (Data_Car.IsSelectable = 0) aren't player cars; leave them out unless named.
        if only.is_empty() && physics["car"]["IsSelectable"].as_i64() == Some(0) {
            continue;
        }
        let r = reference(&physics["car"]);
        let ours = run(&d);
        for i in 0..METRICS.len() - 1 {
            if r[i].is_finite() && r[i] > 0.0 && ours[i].is_finite() {
                errors[i].push((ours[i] - r[i]) / r[i] * 100.0);
                by_type[(d.drive_type as usize).min(3)][i].push((ours[i] - r[i]) / r[i] * 100.0);
            }
        }
        if names.len() <= 12 {
            println!("{name}");
            for i in 0..METRICS.len() - 1 {
                println!("  {:12} ours {:8.2}  gamedb {:8.2}  {:+6.1}%", METRICS[i], ours[i], r[i], (ours[i] - r[i]) / r[i] * 100.0);
            }
        }
        rows.push(format!(
            "{name},{}",
            (0..METRICS.len() - 1).map(|i| format!("{:.3},{:.3}", ours[i], r[i])).collect::<Vec<_>>().join(",")
        ));
    }

    println!("\n{} cars — error vs gamedb (ours - gamedb) / gamedb:", rows.len());
    println!("  {:12} {:>8} {:>8} {:>8}", "metric", "median", "mean", "|med|");
    for i in 0..METRICS.len() - 1 {
        let mut e = errors[i].clone();
        if e.is_empty() {
            continue;
        }
        e.sort_by(f32::total_cmp);
        let med = e[e.len() / 2];
        let mean = e.iter().sum::<f32>() / e.len() as f32;
        let mut abs: Vec<f32> = e.iter().map(|x| x.abs()).collect();
        abs.sort_by(f32::total_cmp);
        println!("  {:12} {:+7.1}% {:+7.1}% {:7.1}%", METRICS[i], med, mean, abs[abs.len() / 2]);
    }
    let median = |e: &[f32]| {
        let mut e = e.to_vec();
        e.sort_by(f32::total_cmp);
        if e.is_empty() { f32::NAN } else { e[e.len() / 2] }
    };
    println!("
median by drive type (n):");
    println!("  {:12} {:>14} {:>14} {:>14}", "metric", "FWD", "RWD", "AWD");
    for i in 0..METRICS.len() - 1 {
        let cell = |t: usize| format!("{:+6.1}% ({:3})", median(&by_type[t][i]), by_type[t][i].len());
        println!("  {:12} {:>14} {:>14} {:>14}", METRICS[i], cell(1), cell(2), cell(3));
    }
    if let Some(path) = csv {
        let header = METRICS[..METRICS.len() - 1].iter().map(|m| format!("{m} ours,{m} gamedb")).collect::<Vec<_>>().join(",");
        std::fs::write(&path, format!("car,{header}\n{}\n", rows.join("\n")))?;
    }
    Ok(())
}
