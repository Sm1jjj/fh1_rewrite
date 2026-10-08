//! Frame pacing (job P5, docs/PERF.md "Frame pacing").
//!
//! The festival frame is GPU-bound at ~13-17 ms. With the default vsync (Fifo) on a high-refresh monitor each frame is
//! shown on the next vblank, so on a 300 Hz screen consecutive frames last 4, 5 or 6 refreshes (13.3 / 16.7 / 20 ms) in a
//! changing pattern, and the env cube (one face every 2nd frame, reflect.rs) makes alternate frames heavier. Bevy's
//! animation dt is the render thread's timestamp after the previous present (bevy_render `send_time`), so it carries the
//! render thread's own jitter too. The result looks uneven even when the mean frame time is fine.
//!
//! [`FramePacer`] (OPT-IN `FH1_FRAMEPACE=1`; it measured worse, see docs/PERF.md) holds the main loop to a steady period: a whole number of
//! refreshes just above the recent 90th-percentile frame cost (`N x 1/Hz`, re-chosen with hysteresis), sleeps to that
//! deadline at the start of the frame and feeds the deadline to Bevy's clock (`TimeUpdateStrategy::ManualInstant`), so
//! every frame's dt is exactly the period the display shows. `FH1_FPS_CAP=<fps>` = fixed period instead of the adaptive
//! one. `FH1_FRAMEPACE=ab` toggles it every 10 s (one-run A/B with `FH1_PERF_LOG=1`).
//!
//! [`PacingLog`] (`FH1_PERF_LOG=1`): every 30 s logs frame dt percentiles, frame-to-frame jitter, spikes, the frame cost
//! without the pacer's sleep, fixed physics ticks per frame and how evenly the car's drawn pose moves.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::time::{TimeSystems, TimeUpdateStrategy};
use bevy::render::{Render, RenderApp, RenderSystems};
use bevy::window::{Monitor, PrimaryMonitor};
use std::sync::{Arc, Mutex};

/// Frames of history for the adaptive period.
const HISTORY: usize = 120;
/// Percentile of recent frame costs the period must cover, and the headroom over it. Frames above it slip one refresh;
/// covering p95 x 1.08 instead cost a whole refresh (57 -> 43 fps) on the open-road drive for little gain.
const PERCENTILE: usize = 90;
const MARGIN: f64 = 1.03;
/// Seconds the cost must stay low before the period steps down a refresh.
const STEP_DOWN_AFTER: f64 = 2.0;
/// Below this rate (Hz) the pacer stops sleeping: the frame is too slow for pacing to matter.
const MIN_HZ: f64 = 25.0;
/// A/B half period (s) for `FH1_FRAMEPACE=ab`.
const AB_SECS: f64 = 10.0;

pub fn plugin(app: &mut App) {
    // OPT-IN: the 2026-10-05 same-run A/B made present-to-present jitter worse (3.0 vs 1.6 ms), see docs/PERF.md.
    let spec = std::env::var("FH1_FRAMEPACE").unwrap_or_default();
    if spec == "1" || spec == "ab" || std::env::var("FH1_FPS_CAP").is_ok() {
        let cap = std::env::var("FH1_FPS_CAP").ok().and_then(|v| v.parse::<f64>().ok()).filter(|f| *f > 1.0);
        app.insert_resource(FramePacer { enabled: true, ab: spec == "ab", cap, ..default() })
            .add_systems(First, pace.before(TimeSystems));
    }
    if std::env::var("FH1_PERF_LOG").is_ok_and(|v| v == "1") {
        let clock = PresentClock::default();
        app.init_resource::<PacingLog>()
            .insert_resource(clock.clone())
            .add_systems(FixedFirst, count_tick)
            .add_systems(Last, record);
        if let Some(ra) = app.get_sub_app_mut(RenderApp) {
            ra.insert_resource(clock).add_systems(Render, stamp_present.in_set(RenderSystems::PostCleanup));
        }
    }
}

#[derive(Resource, Default)]
pub struct FramePacer {
    /// Sleeping + clock feeding on (the A/B flips this).
    pub enabled: bool,
    ab: bool,
    cap: Option<f64>,
    /// Refresh period (s) of the primary monitor.
    refresh: Option<f64>,
    /// Start of the previous frame (after its sleep).
    last: Option<Instant>,
    /// Sleep at the start of the previous frame (s).
    slept: f64,
    /// Recent frame costs (s): frame interval minus the pacer's sleep.
    cost: VecDeque<f64>,
    /// Current period in refreshes.
    n: u32,
    low_since: Option<Instant>,
    started: Option<Instant>,
    /// Last frame's cost and period (s), for the log.
    pub last_cost: f64,
    pub period: f64,
}

impl FramePacer {
    fn choose(&mut self, now: Instant) -> Option<f64> {
        if let Some(fps) = self.cap {
            return Some(1.0 / fps);
        }
        let r = self.refresh.unwrap_or(1.0 / 60.0);
        if self.cost.len() < 30 {
            return None;
        }
        let mut c: Vec<f64> = self.cost.iter().copied().collect();
        c.sort_by(f64::total_cmp);
        let p = c[(c.len() * PERCENTILE / 100).min(c.len() - 1)];
        let need = ((p * MARGIN / r).ceil() as u32).max(1);
        if self.n == 0 || need > self.n {
            self.n = need;
            self.low_since = None;
        } else if need < self.n {
            // Step down only after the cost has stayed low for a while (no flapping at a boundary).
            let since = *self.low_since.get_or_insert(now);
            if now.duration_since(since).as_secs_f64() > STEP_DOWN_AFTER {
                // Straight to the need (loading frames can leave n at hundreds of refreshes).
                self.n = need;
                self.low_since = None;
            }
        } else {
            self.low_since = None;
        }
        let period = self.n as f64 * r;
        (period <= 1.0 / MIN_HZ).then_some(period)
    }
}

/// First, before Bevy's clock: sleep to the frame deadline and hand that instant to the clock.
fn pace(mut p: ResMut<FramePacer>, mut strategy: ResMut<TimeUpdateStrategy>, monitors: Query<(&Monitor, Has<PrimaryMonitor>)>) {
    let now = Instant::now();
    if p.refresh.is_none() {
        let hz = monitors.iter().max_by_key(|(_, primary)| *primary).and_then(|(m, _)| m.refresh_rate_millihertz).map(|mhz| mhz as f64 / 1000.0);
        if let Some(hz) = hz.filter(|h| *h > 20.0) {
            p.refresh = Some(1.0 / hz);
            info!("frame pacer: refresh {hz:.1} Hz{}", p.cap.map_or(String::new(), |c| format!(", cap {c} fps")));
        }
    }
    let started = *p.started.get_or_insert(now);
    if p.ab {
        let on = (now.duration_since(started).as_secs_f64() / AB_SECS) as u64 % 2 == 0;
        if on != p.enabled {
            p.enabled = on;
            info!("frame pacer A/B: {}", if on { "on" } else { "off" });
        }
    }
    if let Some(last) = p.last {
        let cost = (now.duration_since(last).as_secs_f64() - p.slept).max(0.0);
        p.last_cost = cost;
        if p.cost.len() == HISTORY {
            p.cost.pop_front();
        }
        p.cost.push_back(cost);
    }
    let period = p.choose(now);
    p.period = period.unwrap_or(0.0);
    let (Some(period), Some(last), true) = (period, p.last, p.enabled) else {
        p.slept = 0.0;
        p.last = Some(now);
        if !matches!(*strategy, TimeUpdateStrategy::Automatic) {
            *strategy = TimeUpdateStrategy::Automatic;
        }
        return;
    };
    let deadline = last + Duration::from_secs_f64(period);
    let frame = if deadline > now {
        // Coarse sleep, then spin the last stretch (Windows sleeps overshoot by up to ~1 ms).
        let coarse = deadline - now;
        if coarse > Duration::from_micros(1500) {
            std::thread::sleep(coarse - Duration::from_micros(1000));
        }
        while Instant::now() < deadline {
            std::hint::spin_loop();
        }
        deadline
    } else {
        now
    };
    p.slept = frame.saturating_duration_since(now).as_secs_f64();
    p.last = Some(frame);
    // The clock advances by exactly the paced period (Bevy otherwise uses the render thread's post-present instant).
    *strategy = TimeUpdateStrategy::ManualInstant(frame);
}

/// Render-thread instants after each frame's present (shared with the render world). With vsync and a full present
/// queue the present call blocks until a refresh, so these intervals are the closest thing to the displayed cadence.
#[derive(Resource, Default, Clone)]
struct PresentClock(Arc<Mutex<Vec<Instant>>>);

fn stamp_present(clock: Res<PresentClock>) {
    if let Ok(mut v) = clock.0.lock() {
        v.push(Instant::now());
    }
}

#[derive(Resource, Default)]
pub struct PacingLog {
    ticks: u32,
    since: f64,
    t: f64,
    dt: Vec<f32>,
    cost: Vec<f32>,
    /// Frames with 0, 1, 2, 3+ fixed ticks.
    tick_hist: [u32; 4],
    /// |drawn car speed - physics speed| / physics speed per frame while moving.
    pose_err: Vec<f32>,
    last_pose: Option<Vec3>,
    /// Per A/B mode (off, on): dt samples over the whole run.
    by_mode: [Vec<f32>; 2],
    jitter_by_mode: [(f64, u32); 2],
    prev_dt: f32,
    /// Present intervals (ms) this window, and per A/B mode over the run.
    present: Vec<f32>,
    present_by_mode: [Vec<f32>; 2],
    last_present: Option<Instant>,
}

fn count_tick(mut log: ResMut<PacingLog>) {
    log.ticks += 1;
}

fn pct(v: &mut [f32], p: usize) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[(v.len() * p / 100).min(v.len() - 1)]
}

fn record(
    mut log: ResMut<PacingLog>,
    real: Res<Time<Real>>,
    virt: Res<Time<Virtual>>,
    fixed: Res<Time<Fixed>>,
    pacer: Option<Res<FramePacer>>,
    cars: Query<&crate::Car>,
    clock: Res<PresentClock>,
) {
    let dt = real.delta_secs() * 1000.0;
    if dt <= 0.0 {
        return;
    }
    let log = &mut *log;
    let on = pacer.as_ref().is_some_and(|p| p.enabled && p.period > 0.0);
    let stamps: Vec<Instant> = clock.0.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
    for s in stamps {
        if let Some(prev) = log.last_present.replace(s) {
            let ms = s.duration_since(prev).as_secs_f32() * 1000.0;
            if log.t >= 10.0 {
                log.present.push(ms);
                log.present_by_mode[on as usize].push(ms);
            }
        }
    }
    log.t += dt as f64 / 1000.0;
    log.since += dt as f64 / 1000.0;
    let ticks = std::mem::take(&mut log.ticks);
    if log.t < 10.0 {
        // Loading.
        log.prev_dt = dt;
        return;
    }
    log.tick_hist[(ticks as usize).min(3)] += 1;
    log.dt.push(dt);
    log.by_mode[on as usize].push(dt);
    let j = &mut log.jitter_by_mode[on as usize];
    j.0 += (dt - log.prev_dt).abs() as f64;
    j.1 += 1;
    log.prev_dt = dt;
    if let Some(p) = &pacer {
        log.cost.push(p.last_cost as f32 * 1000.0);
    }
    if let Ok(car) = cars.single() {
        let (pos, _) = car.0.render_pose(fixed.overstep_fraction());
        let speed = car.0.speed();
        let vdt = virt.delta_secs();
        if let (Some(prev), true) = (log.last_pose, speed > 5.0 && vdt > 0.0) {
            log.pose_err.push(((pos - prev).length() / vdt - speed).abs() / speed);
        }
        log.last_pose = Some(pos);
    }
    if log.since < 30.0 {
        return;
    }
    log.since = 0.0;
    let mut d = std::mem::take(&mut log.dt);
    let n = d.len();
    let mean = d.iter().sum::<f32>() / n.max(1) as f32;
    let jitter = d.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>() / (n.max(2) - 1) as f32;
    let (p50, p90, p99) = (pct(&mut d, 50), pct(&mut d, 90), pct(&mut d, 99));
    let max = d.last().copied().unwrap_or(0.0);
    let over = |k: f32| d.iter().filter(|x| **x > k * p50).count();
    let big = d.iter().filter(|x| **x > 50.0).count();
    let mut c = std::mem::take(&mut log.cost);
    let (c50, c95) = (pct(&mut c, 50), pct(&mut c, 95));
    let mut e = std::mem::take(&mut log.pose_err);
    let (e50, e95) = (pct(&mut e, 50), pct(&mut e, 95));
    let h = std::mem::take(&mut log.tick_hist);
    info!(
        "pacing t={:.0}s: dt mean {mean:.2} p50 {p50:.2} p90 {p90:.2} p99 {p99:.2} max {max:.2} ms, jitter |d dt| {jitter:.2} ms; \
         >1.5x {} >2x {} >50ms {big}; cost p50 {c50:.2} p95 {c95:.2} ms, period {:.2} ms; ticks/frame 0:{} 1:{} 2:{} 3+:{}; \
         car pose speed err p50 {:.1}% p95 {:.1}% ({} frames)",
        log.t,
        over(1.5),
        over(2.0),
        pacer.as_ref().map_or(0.0, |p| p.period * 1000.0),
        h[0],
        h[1],
        h[2],
        h[3],
        e50 * 100.0,
        e95 * 100.0,
        e.len()
    );
    let refresh = pacer.as_ref().and_then(|p| p.refresh);
    let mut pr = std::mem::take(&mut log.present);
    info!("pacing t={:.0}s present: {}", log.t, present_line(&mut pr, refresh));
    if pacer.as_ref().is_some_and(|p| p.ab) {
        for (i, name) in ["off", "on"].iter().enumerate() {
            let mut v = log.by_mode[i].clone();
            let (j, jn) = log.jitter_by_mode[i];
            info!(
                "pacing A/B {name} (run so far): {} frames, p50 {:.2} p90 {:.2} p99 {:.2} ms, jitter {:.2} ms",
                v.len(),
                pct(&mut v, 50),
                pct(&mut v, 90),
                pct(&mut v, 99),
                j / jn.max(1) as f64
            );
            let mut pv = log.present_by_mode[i].clone();
            info!("pacing A/B {name} present: {}", present_line(&mut pv, refresh));
        }
    }
}

/// Present-interval summary: percentiles, frame-to-frame jitter and how many refreshes each frame stayed up.
fn present_line(v: &mut [f32], refresh: Option<f64>) -> String {
    if v.len() < 2 {
        return "no samples".into();
    }
    let jitter = v.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f32>() / (v.len() - 1) as f32;
    let mut hist = std::collections::BTreeMap::<u32, u32>::new();
    if let Some(r) = refresh {
        for x in v.iter() {
            *hist.entry((*x as f64 / (r * 1000.0)).round() as u32).or_default() += 1;
        }
    }
    let n = v.len();
    let h: Vec<String> = hist.iter().map(|(k, c)| format!("{k}:{:.0}%", *c as f32 * 100.0 / n as f32)).collect();
    format!(
        "{n} frames, p50 {:.2} p90 {:.2} p99 {:.2} max {:.2} ms, jitter {jitter:.2} ms, refreshes/frame {}",
        pct(v, 50),
        pct(v, 90),
        pct(v, 99),
        v.last().copied().unwrap_or(0.0),
        h.join(" ")
    )
}
