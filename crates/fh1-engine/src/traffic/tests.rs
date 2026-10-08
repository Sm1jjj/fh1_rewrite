//! Headless traffic checks (need the converted install: ui/map/colorado.nav, world/colorado, cars; the disc for
//! AIOpenWorld.xml when the `traffic` group isn't installed; skip otherwise).
//! `cargo test --release -p fh1-engine --lib traffic::tests -- --nocapture`. Env: TRAFFIC_CAR (default TOY_Camry_07),
//! TRAFFIC_SECS (default 180), TRAFFIC_CARS (default 6).

use std::path::PathBuf;

use bevy::math::Vec3;

use super::config::OpenWorldConfig;
use super::driver::{mph, TrafficDriver};
use super::network::Network;
use crate::data::{private_assets, CarData};
use crate::vehicle::{contact, Ground, Vehicle};
use crate::world::{WorldGround, MIRROR_Z};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn network() -> Option<(PathBuf, Network)> {
    let assets = private_assets(&root().join("data")).ok()?;
    let bytes = std::fs::read(assets.join("traffic/colorado.nav")).or_else(|_| std::fs::read(assets.join("ui/map/colorado.nav"))).ok()?;
    let nav = fh1_ui::nav::Nav::parse(&bytes).ok()?;
    Some((assets, Network::build(&nav, MIRROR_Z)))
}

fn fitted_network(assets: &std::path::Path, world: &WorldGround) -> Option<Network> {
    let bytes = std::fs::read(assets.join("traffic/colorado.nav")).or_else(|_| std::fs::read(assets.join("ui/map/colorado.nav"))).ok()?;
    let nav = fh1_ui::nav::Nav::parse(&bytes).ok()?;
    Some(Network::build_on(&nav, MIRROR_Z, Some(world)))
}

fn config(assets: &std::path::Path) -> Option<OpenWorldConfig> {
    if let Ok(x) = std::fs::read_to_string(assets.join("traffic/AIOpenWorld.xml")) {
        return Some(OpenWorldConfig::parse(&x));
    }
    let disc = std::env::var("FH1_DISC").map(PathBuf::from).unwrap_or_else(|_| root().join("disc"));
    let mut ar = fh1_formats::zip::Archive::open(disc.join("media/gametunablesettings.zip")).ok()?;
    let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case("AIOpenWorld.xml")).cloned()?;
    Some(OpenWorldConfig::parse(&String::from_utf8_lossy(&ar.read(&e).ok()?)))
}

fn var<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[test]
fn network_and_config() {
    let Some((assets, net)) = network() else {
        eprintln!("skipped: needs data/ (fh1setup)");
        return;
    };
    let total: f32 = net.lanes.iter().map(|l| l.length).sum();
    let dead = net.lanes.iter().filter(|l| l.next.is_empty()).count();
    eprintln!("{} lanes, {:.0} km of lane, {} without a successor", net.lanes.len(), total / 1000.0, dead);
    assert!(net.lanes.len() > 500, "too few lanes");
    assert!(total > 300_000.0, "lane length {total}");
    assert!(dead * 50 < net.lanes.len(), "{dead} dead-end lanes");
    // Every density id on the network has a freeroam entry.
    if let Some(c) = config(&assets) {
        let fr = c.set("freeroam").expect("freeroam");
        assert_eq!(fr.cars.len(), 17);
        assert_eq!(fr.max_loaded_models, 4);
        let mut ids: Vec<u32> = net.lanes.iter().map(|l| l.density).collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            assert!(fr.traffic.contains_key(&id), "density {id} missing");
        }
        assert!(!fr.allowed(&fr.traffic[&2], 1529), "bus on a B-road");
        assert!(fr.allowed(&fr.traffic[&12], 1541) && !fr.allowed(&fr.traffic[&12], 282), "dirt roads: 4x4 only");
    }
}

/// Lanes sit on the collision surface: a ray down from 4 m above each sample hits within 4 m of the lane height.
#[test]
fn lanes_on_ground() {
    let Some((assets, net)) = network() else { return };
    let Ok(world) = WorldGround::load(&assets.join("world/colorado")) else { return };
    let (mut n, mut hit, mut near) = (0, 0, 0);
    for l in net.lanes.iter().step_by(3) {
        for &p in l.pts.iter().step_by(4) {
            n += 1;
            if let Some(h) = world.ray(p + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0) {
                hit += 1;
                if (h.point.y - p.y).abs() < 4.0 {
                    near += 1;
                }
            }
        }
    }
    eprintln!("{n} lane points: {hit} over ground, {near} within 4 m of it");
    assert!(near as f32 > 0.9 * n as f32);
}

/// Far mode: cars slide along their lanes for 10 minutes without stalling at a dead end.
#[test]
fn kinematic_wander() {
    let Some((_, net)) = network() else { return };
    let mut stuck = 0;
    let mut travelled = 0.0;
    for k in 0..40u32 {
        let lane = (k * 7919) % net.lanes.len() as u32;
        let mut d = TrafficDriver::new(&net, lane, 0.0, k + 1);
        let dt = 1.0 / 30.0;
        let mut last = d.ahead(&net, 0.0).0;
        for _ in 0..(600.0 / dt) as usize {
            let v0 = d.target_speed(&net, mph(var("TRAFFIC_MPH", 30.0)));
            let a = TrafficDriver::idm(d.speed, v0, None);
            let (p, _) = d.kinematic(&net, a, dt);
            travelled += (p - last).length();
            last = p;
        }
        if d.speed < 0.5 {
            stuck += 1;
        }
    }
    eprintln!("40 cars, {:.0} km in 10 min, {stuck} stopped at the end", travelled / 1000.0);
    assert!(stuck <= 2);
}

/// Near mode: the full vehicle sim follows lanes on Colorado (lateral error, no crashes), a few cars in a row.
#[test]
fn physics_follow() {
    let Some((assets, _)) = network() else { return };
    let Ok(world) = WorldGround::load(&assets.join("world/colorado")) else { return };
    // Lanes fitted to the road as in game (plugin.rs loads with the world ground).
    let Some(net) = fitted_network(&assets, &world) else { return };
    let car: String = var("TRAFFIC_CAR", "TOY_Camry_07".to_string());
    let Ok(data) = CarData::load(&assets.join("cars").join(&car)) else { return };
    let secs: f32 = var("TRAFFIC_SECS", 180.0);
    let n: usize = var("TRAFFIC_CARS", 6);
    // Start on a long A-road lane.
    // TRAFFIC_ROAD=freeway: start on a freeway lane instead.
    let road = if var("TRAFFIC_ROAD", "a".to_string()) == "freeway" { super::network::RoadType::Freeway } else { super::network::RoadType::A };
    let lane = net.lanes.iter().enumerate().filter(|(_, l)| l.length > 400.0 && l.road == road).map(|(i, _)| i as u32).nth(3).expect("start lane");
    let mut cars: Vec<(Vehicle, TrafficDriver)> = (0..n)
        .map(|k| {
            let s = 20.0 + 14.0 * (n - 1 - k) as f32;
            // Same seed: the convoy takes the same turns.
            let d = TrafficDriver::new(&net, lane, s, 77);
            let (p, t) = d.ahead(&net, 0.0);
            let ground = world.ray(p + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0).map_or(p, |h| h.point);
            let mut v = Vehicle::new(data.clone(), ground);
            v.direct_steer = super::driver::direct_steer();
            v.place(ground, (-t.x).atan2(-t.z));
            // TRAFFIC_ENTRY_MPH=<v>: enter at speed like a far car switching to the sim (plugin.rs step_traffic);
            // TRAFFIC_ENTRY_SYNC=0 skips the drivetrain sync (the old entry).
            let entry = mph(var("TRAFFIC_ENTRY_MPH", 0.0));
            if entry > 0.0 {
                v.velocity = t * entry;
                for i in 0..4 {
                    v.wheels[i].omega = entry / v.data.tyre_radius[i / 2].max(0.2);
                }
                if var("TRAFFIC_ENTRY_SYNC", 1) != 0 {
                    v.sync_drivetrain();
                }
            }
            (v, d)
        })
        .collect();
    let dt = 1.0 / 120.0;
    let (mut max_err, mut contacts, mut travelled) = (0.0f32, 0, 0.0f32);
    let mut errs: Vec<f32> = Vec::new();
    let start = cars[0].0.position;
    // Entering at speed: the transient is what's being checked, so measure from the first tick.
    let warmup = if var("TRAFFIC_ENTRY_MPH", 0.0f32) > 0.0 { 0 } else { 600 };
    for tick in 0..(secs / dt) as usize {
        let poses: Vec<(Vec3, f32, f32)> = cars.iter().map(|(v, d)| (v.position, d.s, v.forward_speed())).collect();
        for (i, (v, d)) in cars.iter_mut().enumerate() {
            let err = d.sync(&net, v.position);
            if tick > warmup {
                max_err = max_err.max(err);
                errs.push(err);
            }
            // Leader = the car ahead in the convoy (index i - 1), gap along the path.
            let leader = (i > 0).then(|| {
                let (p, _, vs) = poses[i - 1];
                ((p - v.position).length() - 4.6, vs)
            });
            let v0 = d.target_speed(&net, mph(var("TRAFFIC_MPH", 30.0)));
            let a = TrafficDriver::idm(v.forward_speed().max(0.0), v0, leader);
            let c = d.controls(&net, v, a);
            v.begin_tick();
            for _ in 0..4 {
                v.step(c, dt / 4.0, &world);
            }
        }
        for i in 0..cars.len() {
            for j in i + 1..cars.len() {
                let (a, b) = cars.split_at_mut(j);
                let before = a[i].0.velocity;
                contact::collide(&mut a[i].0, &mut b[0].0);
                if (a[i].0.velocity - before).length() > 0.5 {
                    contacts += 1;
                }
            }
        }
    }
    travelled += (cars[0].0.position - start).length();
    let speeds: Vec<String> = cars.iter().map(|(v, _)| format!("{:.1}", v.forward_speed())).collect();
    errs.sort_by(f32::total_cmp);
    let mean = errs.iter().sum::<f32>() / errs.len().max(1) as f32;
    let p95 = errs.get(errs.len() * 95 / 100).copied().unwrap_or(0.0);
    eprintln!("{car} x{n}: {secs} s, lead car {travelled:.0} m from start, lane error mean {mean:.2} p95 {p95:.2} max {max_err:.1} m, {contacts} contacts, speeds {speeds:?}");
    assert!(max_err < 6.0, "left the lane: {max_err}");
    assert!(contacts == 0, "convoy touched {contacts} times");
}

/// Car-vs-car contact shapes: every traffic car (MAXData Flags 3-only cars included) has spheres, and two of them parked
/// 3 m apart nose to tail touch while 6 m apart they don't.
#[test]
fn traffic_cars_collide() {
    let Some(assets) = private_assets(&root().join("data")).ok() else { return };
    let names = ["TOY_Camry_07", "BUS_TOUR_12", "KEN_T440_12", "VW_Beetle_04", "AUD_A4Avant_04", "VW_Corrado_95"];
    for n in names {
        let Ok(d) = CarData::load(&assets.join("cars").join(n)) else { return };
        assert!(!d.car_spheres.is_empty(), "{n}: no car spheres");
        let len = (d.bbox[1].z - d.bbox[0].z).abs();
        let mut a = Vehicle::new(d.clone(), Vec3::ZERO);
        let mut b = Vehicle::new(d.clone(), Vec3::new(0.0, 0.0, -(len - 0.3)));
        assert!(contact::overlap(&a, &b).is_some(), "{n}: overlapping cars don't touch");
        b.position.z = a.position.z - len - 1.0;
        assert!(contact::overlap(&a, &b).is_none(), "{n}: cars 1 m apart touch");
        a.velocity = Vec3::new(0.0, 0.0, -5.0);
        b.position.z = a.position.z - (len - 0.3);
        let hit = contact::collide(&mut a, &mut b);
        eprintln!("{n}: {} spheres, closing speed {hit:.1} m/s, b now {:.2} m/s", d.car_spheres.len(), b.velocity.z);
        assert!(hit > 0.0 && b.velocity.z < -0.5, "{n}: no impulse");
    }
}

/// Road widths from the collision surface (user 2026-10-08: highway traffic drives down the middle of the carriageway).
/// For each traffic way type, every 40 m along the nav line: rays down at -16..16 m across it, and the run of the same
/// surface as at the line around it. Prints the surface names seen and, per road type / oneway, the median left / right
/// edge (m, + = right of travel), so lane offsets can be set from the real paved width.
#[test]
fn road_width_survey() {
    let Some(assets) = private_assets(&root().join("data")).ok() else { return };
    let Ok(bytes) = std::fs::read(assets.join("traffic/colorado.nav")).or_else(|_| std::fs::read(assets.join("ui/map/colorado.nav"))) else { return };
    let Ok(nav) = fh1_ui::nav::Nav::parse(&bytes) else { return };
    let Ok(world) = WorldGround::load(&assets.join("world/colorado")) else { return };
    let pos = |i: u32| {
        let p = nav.nodes[i as usize].pos;
        Vec3::new(p[0], p[1], if MIRROR_Z { -p[2] } else { p[2] })
    };
    let mut names: std::collections::BTreeMap<String, usize> = Default::default();
    // (road_type, oneway) -> (left edges, right edges, widths, centre offsets)
    let mut stats: std::collections::BTreeMap<(String, bool), (Vec<f32>, Vec<f32>)> = Default::default();
    for w in &nav.ways {
        let (Some(rt), true) = (w.tags.get("road_type"), w.tags.contains_key("traffic_density")) else { continue };
        let oneway = w.tags.get("oneway").is_some_and(|v| v == "true");
        let pts: Vec<Vec3> = w.nodes.iter().map(|&n| pos(n)).collect();
        let mut carry = 0.0f32;
        for k in 0..pts.len().saturating_sub(1) {
            let (a, b) = (pts[k], pts[k + 1]);
            let seg = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
            let len = seg.length();
            if len < 1e-3 {
                continue;
            }
            let right = (seg / len).cross(Vec3::Y);
            let mut s = 40.0 - carry;
            while s < len {
                let p = a.lerp(b, s / len);
                let mut row: Vec<Option<u8>> = Vec::new();
                for i in 0..=64 {
                    let x = -16.0 + i as f32 * 0.5;
                    let q = p + right * x;
                    let h = world.ray(q + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0);
                    if let Some(h) = h {
                        *names.entry(world.surface_name(h.surface).to_string()).or_default() += 1;
                    }
                    row.push(h.map(|h| h.surface));
                }
                if let Some(c) = row[32] {
                    let (mut l, mut r) = (32usize, 32usize);
                    while l > 0 && row[l - 1] == Some(c) {
                        l -= 1;
                    }
                    while r < 64 && row[r + 1] == Some(c) {
                        r += 1;
                    }
                    let e = stats.entry((format!("{rt}/{}", world.surface_name(c)), oneway)).or_default();
                    e.0.push(-16.0 + l as f32 * 0.5);
                    e.1.push(-16.0 + r as f32 * 0.5);
                }
                s += 40.0;
            }
            carry = (carry + len) % 40.0;
        }
    }
    eprintln!("surfaces within 16 m of traffic ways: {names:?}");
    let med = |v: &mut Vec<f32>, q: f32| {
        v.sort_by(f32::total_cmp);
        v[((v.len() - 1) as f32 * q) as usize]
    };
    for ((k, oneway), (mut l, mut r)) in stats {
        if l.len() < 5 {
            continue;
        }
        let mut w: Vec<f32> = l.iter().zip(&r).map(|(a, b)| b - a).collect();
        let mut c: Vec<f32> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a + b)).collect();
        eprintln!(
            "{k:28} oneway {oneway:5} n {:5}: left p25/50/75 {:+5.1} {:+5.1} {:+5.1} right {:+5.1} {:+5.1} {:+5.1} width p50 {:5.1} centre p50 {:+5.1}",
            l.len(), med(&mut l, 0.25), med(&mut l, 0.5), med(&mut l, 0.75), med(&mut r, 0.25), med(&mut r, 0.5), med(&mut r, 0.75),
            med(&mut w, 0.5), med(&mut c, 0.5)
        );
    }
}

/// Lane fit (user 2026-10-08: highway traffic in the middle of the carriageway): every 20 m of every paved lane, the lane
/// point lies on the road at least 1.2 m inside the paved edges (a car fits), and on one-way freeway carriageways the
/// lanes of one road are a lane width apart. Prints how far lanes sit from the paved centre, per road type.
#[test]
fn lanes_inside_road() {
    let Some(assets) = private_assets(&root().join("data")).ok() else { return };
    let Ok(world) = WorldGround::load(&assets.join("world/colorado")) else { return };
    let Some(net) = fitted_network(&assets, &world) else { return };
    use super::network::RoadType;
    let paved = |q: Vec3| world.ray(q + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0).is_some_and(|h| h.tyre.offroadness <= 0.0 && h.normal.y > 0.7);
    let mut per: std::collections::BTreeMap<String, (usize, usize, Vec<f32>)> = Default::default();
    for l in &net.lanes {
        if l.road == RoadType::Dirt || !l.road_traffic {
            continue;
        }
        let mut s = 10.0;
        while s < l.length - 10.0 {
            let (p, t) = l.at(s);
            s += 20.0;
            let right = Vec3::new(t.x, 0.0, t.z).normalize_or(Vec3::NEG_Z).cross(Vec3::Y);
            if !paved(p) {
                continue; // junction mouths, bridges with other surfaces: not judged
            }
            let (mut a, mut b) = (0.0f32, 0.0f32);
            while a > -14.0 && paved(p + right * (a - 0.25)) {
                a -= 0.25;
            }
            while b < 14.0 && paved(p + right * (b + 0.25)) {
                b += 0.25;
            }
            let e = per.entry(format!("{:?}", l.road)).or_default();
            e.0 += 1;
            if a.abs().min(b) >= 1.2 {
                e.1 += 1;
            }
            e.2.push(0.5 * (a + b));
        }
    }
    let mut ok = true;
    for (k, (n, inside, mut c)) in per {
        c.sort_by(f32::total_cmp);
        let q = |f: f32| c[((c.len() - 1) as f32 * f) as usize];
        eprintln!("{k:8}: {n} points, {:.1}% >= 1.2 m inside the paved edges; paved centre rel. lane p10/50/90 {:+.1} {:+.1} {:+.1} m", 100.0 * inside as f32 / n.max(1) as f32, q(0.1), q(0.5), q(0.9));
        if k == "Freeway" && (inside as f32) < 0.9 * n as f32 {
            ok = false;
        }
    }
    // Same-road one-way freeway lanes are a lane width apart.
    let mut by_road: std::collections::HashMap<u32, Vec<usize>> = Default::default();
    for (i, l) in net.lanes.iter().enumerate() {
        if l.road == RoadType::Freeway && l.twin.is_none() {
            by_road.entry(l.road_id).or_default().push(i);
        }
    }
    let mut gaps = Vec::new();
    for v in by_road.values().filter(|v| v.len() >= 2) {
        let (a, b) = (&net.lanes[v[0]], &net.lanes[v[1]]);
        let (p, _) = a.at(a.length * 0.5);
        let (_, d) = b.project(p, b.length * 0.5);
        gaps.push(d);
    }
    gaps.sort_by(f32::total_cmp);
    if !gaps.is_empty() {
        eprintln!("freeway carriageway lane spacing: min {:.2} median {:.2} m over {} roads", gaps[0], gaps[gaps.len() / 2], gaps.len());
        assert!(gaps[gaps.len() / 2] > 3.0, "freeway lanes too close");
    }
    assert!(ok, "freeway lanes off the road");
}
