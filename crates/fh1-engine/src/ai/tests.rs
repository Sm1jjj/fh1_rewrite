//! Headless AI checks on Colorado (need the converted install: world/colorado + ailines; skip otherwise).
//! `cargo test --release -p fh1-engine ai::tests -- --nocapture`. Env: AI_ROUTE=<n> (default 5), AI_CAR=<MediaName>
//! (default VW_Corrado_95), AI_SKILL=<AISkills id> (default 1), AI_CARS=<n> (field size for the pack test, default 4).

use std::path::PathBuf;
use std::sync::Arc;


use super::driver::{Driver, Obstacle, Situation};
use super::line::RacingLine;
use super::tables::AiTables;
use crate::data::{private_assets, CarData};
use crate::vehicle::{contact, Vehicle};
use crate::world::{WorldGround, MIRROR_Z};

struct Env {
    assets: PathBuf,
    world: WorldGround,
    tables: AiTables,
}

fn env() -> Option<Env> {
    let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let assets = private_assets(&data).ok()?;
    let world = WorldGround::load(&assets.join("world/colorado")).ok()?;
    let tables = AiTables::load(&assets.join("ailines/ai_tables.json")).ok()?;
    Some(Env { assets, world, tables })
}

fn var<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn line(e: &Env, route: u32) -> Arc<RacingLine> {
    Arc::new(RacingLine::load(&super::line_path(&e.assets, "colorado", route), MIRROR_Z).expect("line"))
}

/// Stats of one run.
#[derive(Debug, Default)]
struct Run {
    time: f32,
    finished: bool,
    resets: u32,
    off_track: u32,
    max_lateral_err: f32,
}

/// Lap the route with one AI car: route time vs the profile's prediction, off-track excursions, resets.
#[test]
fn ai_laps_route() {
    let Some(e) = env() else {
        eprintln!("skipped: needs data/ with world + ailines (fh1setup)");
        return;
    };
    let routes: Vec<u32> = std::env::var("AI_ROUTE").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_else(|| vec![5, 3, 49]);
    let car: String = var("AI_CAR", "VW_Corrado_95".to_string());
    let skill: u32 = var("AI_SKILL", 1);
    let d = CarData::load(&e.assets.join("cars").join(&car)).expect("car");
    let mut total_resets = 0;
    for route in routes {
        let l = line(&e, route);
        let mut v = Vehicle::new(d.clone(), l.point_at(0.0));
        v.place(l.point_at(0.0), l.yaw_at(0.0));
        let params = e.tables.driver(skill, 0, 0, 0);
        let mut drv = Driver::new(&l, &v, params, route);
        let run = lap(&mut v, &mut drv, &e.world, 600.0, &[]);
        eprintln!(
            "route {route:03} ({:.0} m, {}) {car} skill {skill}: {} in {:.1} s (profile predicts {:.1} s), resets {}, off-track {}, max |lat err| {:.1} m",
            l.length,
            if l.closed { "circuit" } else { "sprint" },
            if run.finished { "finished" } else { "DNF" },
            run.time,
            drv.profile_max.time,
            run.resets,
            run.off_track,
            run.max_lateral_err
        );
        total_resets += run.resets;
        assert!(run.finished, "route {route}: AI didn't finish");
    }
    assert!(total_resets <= 3, "too many resets: {total_resets}");
}

/// Drive until the route is done (one lap on circuits).
fn lap(v: &mut Vehicle, drv: &mut Driver, ground: &WorldGround, max_t: f32, obstacles: &[Obstacle]) -> Run {
    const HZ: f32 = 120.0;
    const SUB: usize = 4;
    let dt = 1.0 / HZ;
    let mut r = Run::default();
    let goal = drv.progress + drv.line.length as f64 - if drv.line.closed { 0.0 } else { 5.0 };
    while r.time < max_t {
        v.begin_tick();
        let dec = drv.update(v, Situation { obstacles, ..Default::default() }, dt);
        v.torque_mult = dec.torque_mult;
        for _ in 0..SUB {
            v.step(dec.controls, dt / SUB as f32, ground);
        }
        r.time += dt;
        r.max_lateral_err = r.max_lateral_err.max(drv.proj.distance);
        if std::env::var_os("AI_TRACE").is_some() && (r.time * 2.0).fract() < dt * 2.0 {
            let c = dec.controls;
            eprintln!(
                "t {:5.1} s {:6.0} v {:5.1} pred {:5.1} rpm {:5.0} tgt {:5.1} corner {:5.1} lat {:+5.1}/{:4.1} err {:4.1} steer {:+.2} thr {:.2} brk {:.2} gear {} slipF {:+.2} slipR {:+.2}",
                r.time, drv.proj.s, v.forward_speed(), drv.profile_max.v_pred_at(&drv.line, drv.proj.s), v.rpm, drv.target_speed, drv.profile_max.v_corner[drv.proj.index], drv.proj.lateral, drv.proj.half_width,
                drv.proj.distance, c.steer, c.throttle, c.brake, v.gear, v.wheels[0].norm_slip_angle, v.wheels[2].norm_slip_angle
            );
        }
        if drv.progress >= goal {
            r.finished = true;
            break;
        }
    }
    r.resets = drv.resets;
    r.off_track = drv.off_track;
    r
}

/// A pack of AI cars from a two-wide grid on one route with car-vs-car contact: everyone finishes, contacts counted.
#[test]
fn ai_pack_race() {
    let Some(e) = env() else {
        eprintln!("skipped: needs data/ with world + ailines (fh1setup)");
        return;
    };
    let route: u32 = var("AI_ROUTE", 5);
    let n: usize = var("AI_CARS", 4);
    let cars = ["VW_Corrado_95", "ALF_8C_08", "BMW_M3E92_08", "CHE_CamaroSS_69", "DOD_ViperSRT10ACRX_12", "FOR_MustangBOSS429_70", "VW_Corrado_95", "ALF_8C_08"];
    let l = line(&e, route);
    let mut field: Vec<(Vehicle, Driver)> = Vec::new();
    for k in 0..n {
        let d = CarData::load(&e.assets.join("cars").join(cars[k % cars.len()])).expect("car");
        // Grid: 2 wide, 8 m rows, starting 10 m into the line.
        let s = 10.0 + 8.0 * (n / 2 - k / 2) as f32;
        let (c, lv, _) = l.road_at(s);
        let side = if k % 2 == 0 { 0.35 } else { -0.35 };
        let p = c + lv * side;
        let mut v = Vehicle::new(d, p);
        v.place(p, l.yaw_at(s));
        let drv = Driver::new(&l, &v, e.tables.driver(10 + 5 * k as u32, 0, 0, 0), k as u32 + 1);
        field.push((v, drv));
    }
    const HZ: f32 = 120.0;
    let dt = 1.0 / HZ;
    let (mut t, mut hits) = (0.0f32, 0u32);
    let goal = l.length as f64 + 20.0;
    let mut done = vec![None; n];
    while t < 600.0 && done.iter().any(Option::is_none) {
        let obs: Vec<Obstacle> = field.iter().map(|(v, _)| Obstacle::of(v)).collect();
        for (k, (v, drv)) in field.iter_mut().enumerate() {
            v.begin_tick();
            let others: Vec<Obstacle> = obs.iter().enumerate().filter(|(j, _)| *j != k).map(|(_, o)| *o).collect();
            let dec = drv.update(v, Situation { obstacles: &others, finished: done[k].is_some(), ..Default::default() }, dt);
            v.torque_mult = dec.torque_mult;
            for _ in 0..4 {
                v.step(dec.controls, dt / 4.0, &e.world);
            }
        }
        for a in 0..n {
            for b in a + 1..n {
                let (lo, hi) = field.split_at_mut(b);
                if contact::collide(&mut lo[a].0, &mut hi[0].0) > 0.5 {
                    hits += 1;
                }
            }
        }
        t += dt;
        for k in 0..n {
            if done[k].is_none() && field[k].1.progress >= goal {
                done[k] = Some(t);
            }
        }
    }
    for (k, (v, drv)) in field.iter().enumerate() {
        eprintln!(
            "car {k} {}: {} resets {} off-track {} skill {}",
            v.data.media_name,
            done[k].map_or("DNF".into(), |t| format!("{t:.1} s")),
            drv.resets,
            drv.off_track,
            drv.params.skill.id
        );
    }
    eprintln!("route {route:03}: {n} cars, {hits} car-car hits over 0.5 m/s");
    assert!(done.iter().all(Option::is_some), "every car finishes");
}

/// The .owt format: every installed line parses, closed loops close, and the offset sits on the inside of corners.
#[test]
fn owt_lines_parse() {
    let Some(e) = env() else {
        eprintln!("skipped: needs data/ (fh1setup)");
        return;
    };
    let dir = e.assets.join("ailines/colorado");
    let mut n = 0;
    for f in std::fs::read_dir(&dir).expect("ailines/colorado") {
        let p = f.unwrap().path();
        let l = RacingLine::load(&p, MIRROR_Z).expect("parse");
        if l.closed {
            assert!(l.points[0].distance(l.points[l.len() - 1]) < 5.0, "{}: circuit doesn't close", p.display());
        }
        // Inside of corners: the offset's sign follows the road centre's curvature (+ = left in both).
        let centre = RacingLine::new(l.closed, l.centre.clone(), l.lateral.clone(), vec![0.0; l.len()]);
        let corr: f32 = (5..l.len() - 5).map(|i| centre.curvature(i, 4) * l.offset[i]).sum();
        assert!(corr > 0.0, "{}: racing line not on the inside ({corr})", p.display());
        n += 1;
    }
    assert!(n >= 40, "only {n} lines");
}

/// Grid hold then GO (user bug R3: AI reversed on the grid): 6 cars held 5 s, then released for 4 s. No car moves
/// backwards along the line by more than 0.2 m at any time, and every car is moving forwards at the end.
#[test]
fn ai_grid_hold_then_go() {
    let Some(e) = env() else {
        eprintln!("skipped: needs data/ with world + ailines (fh1setup)");
        return;
    };
    let route: u32 = var("AI_ROUTE", 5);
    let cars = ["VW_Corrado_95", "ALF_8C_08", "BMW_M3E92_08", "CHE_CamaroSS_69", "DOD_ViperSRT10ACRX_12", "FOR_MustangBOSS429_70"];
    let l = line(&e, route);
    let mut field: Vec<(Vehicle, Driver, f32)> = Vec::new();
    for (k, car) in cars.iter().enumerate() {
        let d = CarData::load(&e.assets.join("cars").join(car)).expect("car");
        let s = 10.0 + 8.0 * (cars.len() / 2 - k / 2) as f32;
        let (c, lv, _) = l.road_at(s);
        let p = c + lv * if k % 2 == 0 { 0.35 } else { -0.35 };
        let mut v = Vehicle::new(d, p);
        v.place(p, l.yaw_at(s));
        let drv = Driver::new(&l, &v, e.tables.driver(50, 0, 0, 0), k as u32 + 1);
        let s0 = drv.proj.s;
        field.push((v, drv, s0));
    }
    let dt = 1.0 / 120.0;
    let mut t = 0.0f32;
    let mut worst = vec![0.0f32; cars.len()];
    while t < 9.0 {
        let hold = t < 5.0;
        for (k, (v, drv, s0)) in field.iter_mut().enumerate() {
            v.begin_tick();
            let dec = drv.update(v, Situation { hold, ..Default::default() }, dt);
            v.torque_mult = dec.torque_mult;
            for _ in 0..4 {
                v.step(dec.controls, dt / 4.0, &e.world);
            }
            worst[k] = worst[k].min(drv.proj.s - *s0);
        }
        t += dt;
    }
    for (k, (v, drv, s0)) in field.iter().enumerate() {
        eprintln!("car {k} {}: worst backwards {:.2} m, after GO {:.1} m, {:.1} m/s, gear {}", cars[k], -worst[k], drv.proj.s - s0, v.forward_speed(), v.gear);
        assert!(worst[k] > -0.2, "{}: rolled back {:.2} m", cars[k], -worst[k]);
        assert!(v.forward_speed() > 3.0 && v.gear >= 1, "{}: not leaving forwards", cars[k]);
    }
}
