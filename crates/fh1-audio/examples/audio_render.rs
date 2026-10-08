//! audio_render <audio group dir> <CAR> <out.wav> [--live]
//!
//! Renders a scripted drive (idle, full-throttle pulls through the gears, overrun, a skid,
//! gravel, the rev limiter, a lift-off) offline to a WAV, or with `--live` plays it on the default device.

use fh1_audio::synth::{CarInput, CarSound, Library, TyreGroup, WheelInput};

const RATE: f32 = 48000.0;
const BLOCK: usize = 256;

/// Very rough drive script: returns the input at time `t` seconds.
fn script(t: f32, idle: f32, redline: f32) -> CarInput {
    let mut i = CarInput { volume: 1.0, has_turbo: true, ..Default::default() };
    let wheel = |surface, slip_ratio, slip_angle_deg| WheelInput { slip_ratio, slip_angle_deg, load: 3500.0, surface, norm_slip: None };
    let pull = |u: f32| idle * 1.2 + (redline * 0.97 - idle * 1.2) * u.clamp(0.0, 1.0);
    let mut surface = TyreGroup::OnRoad;
    let (mut sr, mut sa) = (0.0, 0.0);
    match t {
        t if t < 2.0 => {
            i.rpm = idle;
            i.gear = 1;
        }
        t if t < 9.0 => {
            // Three 2.33 s pulls (gears 1-3); each shift drops rpm by ~30 %.
            let g = ((t - 2.0) / 2.333).floor();
            let u = ((t - 2.0) % 2.333) / 2.333;
            i.gear = 1 + g as i32;
            i.throttle = 1.0;
            i.rpm = pull(0.35 * g.min(1.0) + (1.0 - 0.35 * g.min(1.0)) * u);
            i.speed = 5.0 + (t - 2.0) * 7.0;
            i.boost = ((t - 2.3) * 1.5).clamp(0.0, 1.0);
            if t < 2.6 {
                sr = 0.4; // launch wheelspin
            }
        }
        t if t < 12.0 => {
            // Lift: overrun down to 2500.
            i.gear = 4;
            i.rpm = pull(0.75 - (t - 9.0) * 0.2);
            i.speed = 54.0 - (t - 9.0) * 4.0;
            i.torque = Some(-0.6);
        }
        t if t < 14.5 => {
            // Hard cornering slide.
            i.gear = 3;
            i.throttle = 0.6;
            i.rpm = pull(0.6);
            i.speed = 30.0;
            sa = 4.0 + (t - 12.0) * 5.0;
        }
        t if t < 17.0 => {
            // Off the road onto gravel.
            i.gear = 3;
            i.throttle = 0.5;
            i.rpm = pull(0.45);
            i.speed = 25.0;
            surface = TyreGroup::OffRoad;
            sa = 3.0;
        }
        t if t < 19.5 => {
            // Pinned on the rev limiter in 2nd.
            i.gear = 2;
            i.throttle = 1.0;
            i.rpm = redline * 1.02;
            i.rpm_limit = redline * 1.02;
            i.speed = 30.0;
            i.boost = 1.0;
        }
        t if t < 22.0 => {
            // Lift: overrun burbles while the revs fall.
            i.gear = 2;
            i.rpm = redline * (1.0 - (t - 19.5) * 0.25);
            i.speed = 30.0 - (t - 19.5) * 3.0;
            i.torque = Some(-0.6);
        }
        _ => {
            i.gear = 1;
            i.rpm = idle;
        }
    }
    i.wheels = [wheel(surface, sr, sa); 4];
    i.shifts = i.gear as u32;
    i
}

/// `AUDIO_RENDER_SCRIPT=held`: the throttle held (burble gate check, 2026-10-08). 0-6 s: full throttle in 4th below the
/// limiter, with the AI-style torque-derived throttle dipping to 0 for 50 ms every 0.8 s (shift / TCS cuts); 6-6.3 s: a
/// short lift; 6.3-12 s: held again; 12-14 s: a real lift (burbles expected only here).
fn held_script(t: f32, idle: f32, redline: f32) -> CarInput {
    let mut i = CarInput { volume: 1.0, gear: 4, speed: 40.0, ..Default::default() };
    i.rpm = idle + (redline - idle) * 0.8;
    i.throttle = match t {
        t if t < 6.0 => if (t % 0.8) < 0.05 { 0.0 } else { 1.0 },
        t if t < 6.3 => 0.0,
        t if t < 12.0 => 1.0,
        _ => 0.0,
    };
    i.torque = Some(if i.throttle > 0.0 { 0.9 } else { -0.5 });
    i.shifts = 4;
    i
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(args.len() >= 4, "usage: audio_render <audio dir> <CAR> <out.wav> [--live]");
    let lib = Library::open(&args[1])?;
    let mut sound = CarSound::new(&lib, &args[2])?;
    let (idle, red) = (sound.audio.rpm_idle, sound.audio.rpm_redline);
    println!(
        "{}: idle {idle} redline {red}, {} cyl; intake {:?} ambient {:?} exhaust {:?}",
        args[2],
        sound.audio.cylinders,
        sound.audio.intake.as_ref().map(|e| &e.bank),
        sound.audio.ambient.as_ref().map(|e| &e.bank),
        sound.audio.exhaust.as_ref().map(|e| &e.bank),
    );
    const SECONDS: f32 = 23.0;
    if args.iter().any(|a| a == "--live") {
        let player = fh1_audio::output::Player::start()?;
        player.set_sound(Some(sound));
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_secs_f32() < SECONDS {
            player.set_input(script(t0.elapsed().as_secs_f32(), idle, red));
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        return Ok(());
    }
    let mut pcm = Vec::new();
    let mut block = vec![0f32; BLOCK * 2];
    let mut t = 0.0;
    // Log of (time, rpm) for checking the pitch afterwards.
    let mut log = String::new();
    let held = std::env::var("AUDIO_RENDER_SCRIPT").is_ok_and(|v| v == "held");
    let mut pops = Vec::new();
    while t < SECONDS {
        let input = if held { held_script(t, idle, red) } else { script(t, idle, red) };
        sound.render(&input, &mut block, RATE);
        sound.take_backfires(&mut pops);
        for _ in pops.drain(..) {
            log.push_str(&format!("pop {t:.3}\n"));
        }
        pcm.extend(block.iter().map(|x| (x * 32767.0) as i16));
        log.push_str(&format!("{t:.4} {:.1}\n", input.rpm));
        t += BLOCK as f32 / RATE;
    }
    fh1_audio::wav::write(args[3].as_ref(), RATE as u32, 2, &pcm)?;
    std::fs::write(format!("{}.rpm.txt", args[3]), log)?;
    println!("wrote {} ({:.1} s)", args[3], pcm.len() as f32 / 2.0 / RATE);
    Ok(())
}
