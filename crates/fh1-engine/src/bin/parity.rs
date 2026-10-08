//! parity [--data <dir>] [--csv out.csv] [--no-tcs] [--full-lock] [--old-lat] [--old-harness] [--in-game] [car ...]
//!
//! Runs scripted tests through the vehicle simulation and compares the results with the
//! numbers gamedb stores for each car (Turn 10's own simulation: `Data_Car.Sim*`).
//! With no cars given, runs every car that has a model and prints aggregate error.

use std::path::PathBuf;

use bevy::math::Vec3;
use fh1_engine::data::{private_assets, CarData};
use fh1_engine::stats;
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

/// The library harness (fh1_engine::stats) with this run's flags.
fn opts() -> stats::Options {
    use std::sync::atomic::Ordering::Relaxed;
    stats::Options { tcs: TCS.load(Relaxed), full_lock: FULL_LOCK.load(Relaxed), in_game: IN_GAME.load(Relaxed) }
}

/// FH1's lateral test (docs/HANDLING_PARITY.md §1; fh1_engine::stats::lateral_game).
fn lateral_game(d: &CarData, speed: f32) -> f32 {
    stats::lateral_game(d, speed, &opts())
}

/// FH1's stats harness, launch + braking (docs/HANDLING_PARITY.md §5; fh1_engine::stats::harness).
fn harness(d: &CarData) -> [f32; 7] {
    stats::harness(d, &opts())
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
