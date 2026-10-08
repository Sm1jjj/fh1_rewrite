//! drive_probe [car] : full throttle from Colorado start location 0 for 6 s, printing state.
//! CHECKS=1: handling checks with the in-game tyre scales and with the dev-screen ones (1.0): a TCS launch to 60 mph,
//! an instant full-brake ABS stop from 60 mph, and a full-lock corner at 40 mph (FLAT=1 for a plane).
use bevy::math::Vec3;
use fh1_engine::data::{private_assets, CarData};
use fh1_engine::vehicle::{Controls, FlatGround, Ground, SphereContact, Vehicle};
use fh1_engine::world::{WorldGround, MIRROR_Z};

fn main() -> anyhow::Result<()> {
    let car = std::env::args().nth(1).unwrap_or("ALF_8C_08".into());
    let assets = private_assets(std::path::Path::new("data"))?;
    let world = WorldGround::load(&assets.join("world/colorado"))?;
    let spawns: serde_json::Value = serde_json::from_slice(&std::fs::read(assets.join("world/colorado/spawns.json"))?)?;
    let idx: usize = std::env::var("SPAWN").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let s = &spawns["spawns"][idx];
    let mz = if MIRROR_Z { -1.0 } else { 1.0 };
    let n = |v: &serde_json::Value, i: usize| v[i].as_f64().unwrap() as f32;
    let point = Vec3::new(n(&s["position"], 0), s["ground_y"].as_f64().unwrap() as f32, n(&s["position"], 2) * mz);
    let yaw = (-n(&s["facing"], 0)).atan2(-n(&s["facing"], 2) * mz);
    let flat = std::env::var_os("FLAT").is_some();
    let point = if flat { Vec3::new(point.x, 0.0, point.z) } else { point };
    let ground: &dyn Ground = if flat { &FlatGround } else { &world };
    let d = CarData::load(&assets.join("cars").join(&car))?;
    if std::env::var_os("CHECKS").is_some() {
        for scales in [true, false] {
            checks(&d, point, yaw, ground, scales);
        }
        return Ok(());
    }
    let mut v = Vehicle::new(d.clone(), point);
    v.place(point, yaw);
    let fwd = v.rotation * Vec3::NEG_Z;
    for k in [-10.0f32, -5.0, 0.0, 5.0, 10.0] {
        let p = point + fwd * k + Vec3::Y * 20.0;
        let h = world.ray(p, Vec3::NEG_Y, 60.0);
        println!("ground {k:+} m along heading: {:?}", h.map(|h| (h.point.y, world.surface_name(h.surface).to_string(), h.normal)));
    }
    println!("heading {fwd:?}");
    for hgt in [0.2f32, 0.6, 1.0, 1.5, 3.0] {
        let o = Vec3::new(point.x, point.y + hgt, point.z);
        let o_w = [o.x, o.y, -o.z];
        let d_w = [fwd.x, fwd.y, -fwd.z];
        let h = world.world.raycast(o_w, d_w, 30.0);
        println!("forward ray at {hgt} m: {:?}", h.map(|h| (h.t, world.surface_name(h.surface).to_string(), h.normal)));
    }
    let dt = 1.0 / 480.0;
    let mut contacts: Vec<SphereContact> = Vec::new();
    for step in 0..(480 * 6) {
        let input = Controls { throttle: if step > 480 { 1.0 } else { 0.0 }, tcs: true, abs: true, ..Default::default() };
        v.step(input, dt, ground);
        if step % 240 == 0 {
            let mut touching = Vec::new();
            for (i, (c, r)) in d.collision_spheres.iter().enumerate() {
                ground.sphere(v.position + v.rotation * (*c - v.cg_model), *r, &mut contacts);
                if !contacts.is_empty() {
                    touching.push((i, contacts[0].depth, contacts[0].normal));
                }
            }
            let w = &v.wheels;
            println!(
                "t={:4.1} speed {:5.1} pos ({:.1},{:.1},{:.1}) grounded {:?} loads {:.0} {:.0} {:.0} {:.0} slip {:+.2} {:+.2} surface {} sphere contacts {:?}",
                step as f32 * dt, v.speed(), v.position.x, v.position.y, v.position.z,
                w.iter().map(|w| w.grounded as u8).collect::<Vec<_>>(), w[0].load, w[1].load, w[2].load, w[3].load,
                w[2].slip_ratio, w[3].slip_ratio, world.surface_name(w[2].surface), touching
            );
            if step > 480 {
                for (i, w) in w.iter().enumerate() {
                    println!("   wheel {i}: force {:?} normal {:?} omega {:.1} len {:.3}", w.force, w.normal, w.omega, w.length);
                }
                println!("   vel {:?} fwd {:?} body contacts so far {} last {:?}", v.velocity, v.rotation * Vec3::NEG_Z, v.body_contacts, v.last_contact);
            }
        }
    }
    Ok(())
}

fn fresh(d: &CarData, point: Vec3, yaw: f32, ground: &dyn Ground, scales: bool) -> Vehicle {
    let mut v = Vehicle::new(d.clone(), point);
    if !scales {
        v.override_torque_free(1.0);
    }
    v.place(point, yaw);
    for _ in 0..480 {
        v.step(Controls { tcs: true, abs: true, ..Default::default() }, 1.0 / 480.0, ground);
    }
    v
}

fn set_speed(v: &mut Vehicle, d: &CarData, speed: f32) {
    v.velocity = v.rotation * Vec3::NEG_Z * speed;
    for (i, w) in v.wheels.iter_mut().enumerate() {
        w.omega = speed / d.tyre_radius[i / 2];
    }
    v.sync_drivetrain();
}

/// Launch, 60-0 stop and full-lock corner; prints one line each.
fn checks(d: &CarData, point: Vec3, yaw: f32, ground: &dyn Ground, scales: bool) {
    let dt = 1.0 / 480.0;
    let mph = 0.44704;
    let tag = if scales { "in-game scales" } else { "scales 1.0    " };
    let input = Controls { tcs: true, abs: true, ..Default::default() };
    // Launch.
    let mut v = fresh(d, point, yaw, ground, scales);
    let (mut t, mut cut, mut n, mut slip_max) = (0.0f32, 0.0f32, 0u32, 0.0f32);
    while v.forward_speed() < 60.0 * mph && t < 20.0 {
        v.step(Controls { throttle: 1.0, ..input }, dt, ground);
        t += dt;
        cut += v.tcs_cut;
        n += (v.tcs_cut > 0.0) as u32;
        slip_max = slip_max.max(v.wheels.iter().map(|w| w.norm_slip).fold(0.0, f32::max));
    }
    println!("{tag} launch: 0-60 {t:.2} s, TCS active {:.0}% of ticks (mean cut {:.2}), max driven norm slip {slip_max:.2}",
        n as f32 / (t / dt) * 100.0, cut / (t / dt));
    // 60-0, instant full brake.
    let mut v = fresh(d, point, yaw, ground, scales);
    set_speed(&mut v, d, 60.0 * mph);
    let start = v.position;
    let (mut t, mut abs_ticks, mut releases, mut peak_g, mut was) = (0.0f32, 0u32, 0u32, 0.0f32, false);
    while v.forward_speed() > 0.3 && t < 15.0 {
        let before = v.velocity;
        v.step(Controls { brake: 1.0, ..input }, dt, ground);
        if std::env::var_os("CHECKS_TRACE").is_some() && scales && v.forward_speed() < 0.6 && ((t / dt).round() as u32) % 16 == 0 {
            let sum: Vec3 = v.wheels.iter().map(|w| w.force).sum();
            println!("  DV t={t:.4} dv/dt {:.2?} accel {:.2?} sum wheel force/m {:.2?} gear {}", (v.velocity - before) / dt, v.acceleration,
                sum / d.mass, v.gear);
        }
        t += dt;
        let a = v.abs_active();
        abs_ticks += a as u32;
        releases += (a && !was) as u32;
        was = a;
        peak_g = peak_g.max(-v.acceleration.dot(v.rotation * Vec3::NEG_Z) / 9.81);
        if std::env::var_os("CHECKS_TRACE").is_some() && scales && v.forward_speed() < 0.6 && ((t / dt).round() as u32) % 4 == 0 {
            let w = &v.wheels;
            println!("  END t={t:.4} v={:.3} omega {:.2?} force {:.0?} contacts {}", v.forward_speed(),
                w.iter().map(|w| w.omega).collect::<Vec<_>>(), w.iter().map(|w| w.force).collect::<Vec<_>>(), v.body_contacts);
        }
        if std::env::var_os("CHECKS_TRACE").is_some() && scales && ((t / dt).round() as u32) % 24 == 0 {
            let w = &v.wheels;
            println!(
                "  t={t:.2} v={:5.2} decel {:+.2} g  norm slip {:+.2} {:+.2} {:+.2} {:+.2}  omega {:.1} {:.1} {:.1} {:.1}  abs {}  gear {} rpm {:.0} torque {:+.2} pedal {:.2?} vel {:.2?} angvel {:.2?}",
                v.forward_speed(), -v.acceleration.dot(v.rotation * Vec3::NEG_Z) / 9.81,
                w[0].norm_slip, w[1].norm_slip, w[2].norm_slip, w[3].norm_slip, w[0].omega, w[1].omega, w[2].omega, w[3].omega, a as u8, v.gear, v.rpm, v.torque_fraction, v.brake_pedals(), v.velocity, v.angular_velocity
            );
        }
    }
    let dist = (v.position - start).reject_from(Vec3::Y).length();
    println!("{tag} 60-0: {dist:.1} m in {t:.2} s (mean {:.2} g, peak {peak_g:.2} g), ABS active {:.0}% of ticks, {releases} releases",
        (60.0 * mph) / t / 9.81, abs_ticks as f32 / (t / dt) * 100.0);
    // Full lock at 40 mph, throttle holding speed.
    let mut v = fresh(d, point, yaw, ground, scales);
    let target = 40.0 * mph;
    set_speed(&mut v, d, target);
    let (mut t, mut sum, mut cnt, mut lock, mut xf, mut xr) = (0.0f32, 0.0f32, 0u32, 0.0f32, 0.0f32, 0.0f32);
    while t < 6.0 {
        let err = target - v.speed();
        let thr = (err * 0.5).clamp(0.0, 1.0);
        v.step(Controls { steer: 1.0, throttle: thr, ..input }, dt, ground);
        t += dt;
        if t > 3.0 {
            let right = (v.rotation * Vec3::X).reject_from(Vec3::Y).normalize_or_zero();
            sum += v.acceleration.dot(right).abs() / 9.81;
            cnt += 1;
            lock += v.wheels[0].steer.abs().to_degrees();
            xf += 0.5 * (v.wheels[0].norm_slip_angle.abs() + v.wheels[1].norm_slip_angle.abs());
            xr += 0.5 * (v.wheels[2].norm_slip_angle.abs() + v.wheels[3].norm_slip_angle.abs());
        }
    }
    let c = cnt.max(1) as f32;
    let roll_deg = |v: &Vehicle| (v.rotation * Vec3::X).y.asin().to_degrees();
    println!("{tag}   roll at the end {:+.2} deg, wheel loads {:.0?} N", roll_deg(&v), v.wheels.iter().map(|w| w.load).collect::<Vec<_>>());
    // Lane change at 80 mph: steer 0.5 sine at 1 Hz for 2 s (throttle holds speed).
    let mut v2 = fresh(d, point, yaw, ground, scales);
    set_speed(&mut v2, d, 80.0 * mph);
    let (mut t2, mut max_roll, mut max_g, mut min_load, mut lifted, mut max_rate) = (0.0f32, 0.0f32, 0.0f32, f32::MAX, 0u32, 0.0f32);
    while t2 < 3.0 {
        let steer = if t2 < 2.0 { 0.5 * (std::f32::consts::TAU * t2).sin() } else { 0.0 };
        let thr = ((80.0 * mph - v2.speed()) * 0.5).clamp(0.0, 1.0);
        v2.step(Controls { steer, throttle: thr, ..input }, dt, ground);
        t2 += dt;
        let right = (v2.rotation * Vec3::X).reject_from(Vec3::Y).normalize_or_zero();
        max_g = max_g.max(v2.acceleration.dot(right).abs() / 9.81);
        max_roll = max_roll.max(roll_deg(&v2).abs());
        max_rate = max_rate.max((v2.rotation.inverse() * v2.angular_velocity).z.abs().to_degrees());
        let ml = v2.wheels.iter().map(|w| w.load).fold(f32::MAX, f32::min);
        min_load = min_load.min(ml);
        lifted += (ml <= 1.0) as u32;
    }
    println!("{tag} lane change 80 mph: max lat {max_g:.2} g, max roll {max_roll:.2} deg, max roll rate {max_rate:.0} deg/s, min wheel load {min_load:.0} N, wheel-lift ticks {lifted}, end roll {:+.2} deg, upright {:.3}",
        roll_deg(&v2), (v2.rotation * Vec3::Y).y);
    println!("{tag} full lock 40 mph: {:.2} g at {:.1} mph, road-wheel angle {:.1} deg, norm slip angle front {:.2} rear {:.2}, yaw rate {:.0} deg/s",
        sum / c, v.speed() / mph, lock / c, xf / c, xr / c, v.angular_velocity.y.abs().to_degrees());
}
