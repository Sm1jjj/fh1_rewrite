//! Hitch attribution (`FH1_PERF_LOG=1`, job P5b): where a long frame went.
//!
//! A timestamp system in each main schedule (First .. Last) and in each render set (ExtractSchedule, PrepareAssets ..
//! PostCleanup) gives a coarse per-frame breakdown. Whenever a frame takes over 2.5x the running median (and over
//! 40 ms) it logs the main-world split of that frame, the latest render-thread split, and the entity count change. Every
//! 30 s it logs the mean split so steady per-frame costs show too. Stamps sit somewhere inside each schedule (their
//! order within it is free), so splits are approximate to the length of the systems around them.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::app::{First, Last, PostUpdate, PreUpdate, Update};
use bevy::prelude::*;
use bevy::render::{ExtractSchedule, Render, RenderApp, RenderSystems};

const RENDER: [&str; 9] = ["Extract", "PrepareAssets", "Specialize", "Queue", "PhaseSort", "Prepare", "Render", "Cleanup", "PostCleanup"];

#[derive(Resource, Clone, Default)]
pub(crate) struct RenderStamps(pub(crate) Arc<Mutex<RenderFrame>>);

#[derive(Default, Clone)]
pub(crate) struct RenderFrame {
    cur: [Option<Instant>; 9],
    /// Last complete render frame: ms per set (from the set's stamp to the next).
    pub(crate) last: [f32; 9],
    sum: [f64; 9],
    frames: u32,
}

/// The last main frame's split (ms): wait->First, ->PreUpdate, ->Update, ->PostUpdate, ->Last (perf/record.rs).
#[derive(Resource, Default, Clone, Copy)]
pub(crate) struct LastSplit(pub(crate) [f32; 5]);

#[derive(Resource, Default)]
struct MainStamps {
    /// FH1_PERF_LOG=1: log hitches and the 30 s mean split.
    log: bool,
    cur: [Option<Instant>; 5],
    prev_last: Option<Instant>,
    median: Vec<f32>,
    sum: [f64; 5],
    frames: u32,
    since: f32,
    entities: usize,
}

pub fn plugin(app: &mut App) {
    let log = std::env::var("FH1_PERF_LOG").is_ok_and(|v| v == "1");
    // The gameplay recorder (perf/record.rs) reads the same splits without the log lines.
    // The stall watchdog (always on, record.rs `stall_watch`) needs the phase beats too.
    if !log && !super::record::on() && !super::record::stall_watch() {
        return;
    }
    let rs = RenderStamps::default();
    app.insert_resource(MainStamps { log, ..default() })
        .init_resource::<LastSplit>()
        .add_systems(First, stamp_main::<0>)
        .add_systems(PreUpdate, stamp_main::<1>)
        .add_systems(Update, stamp_main::<2>)
        .add_systems(PostUpdate, stamp_main::<3>)
        .add_systems(Last, stamp_main::<4>)
        .add_systems(Last, report.after(stamp_main::<4>));
    if let Some(ra) = app.get_sub_app_mut(RenderApp) {
        ra.insert_resource(rs.clone())
            .add_systems(ExtractSchedule, stamp_render::<0>)
            .add_systems(Render, stamp_render::<1>.in_set(RenderSystems::PrepareAssets))
            .add_systems(Render, stamp_render::<2>.in_set(RenderSystems::Specialize))
            .add_systems(Render, stamp_render::<3>.in_set(RenderSystems::Queue))
            .add_systems(Render, stamp_render::<4>.in_set(RenderSystems::PhaseSort))
            .add_systems(Render, stamp_render::<5>.in_set(RenderSystems::Prepare))
            .add_systems(Render, stamp_render::<6>.in_set(RenderSystems::Render))
            .add_systems(Render, stamp_render::<7>.in_set(RenderSystems::Cleanup))
            .add_systems(Render, stamp_render::<8>.in_set(RenderSystems::PostCleanup));
    }
    // The main world reads the render split through the same Arc.
    app.insert_resource(rs);
}

fn stamp_main<const I: usize>(mut s: ResMut<MainStamps>) {
    s.cur[I] = Some(Instant::now());
    super::watchdog::main_beat(I as u32);
}

fn stamp_render<const I: usize>(s: Res<RenderStamps>) {
    let now = Instant::now();
    super::watchdog::render_beat(I as u32);
    let Ok(mut f) = s.0.lock() else { return };
    f.cur[I] = Some(now);
    if I == 8 {
        // Frame complete: each set's time runs to the next stamp.
        let cur = f.cur;
        for k in 0..8 {
            if let (Some(a), Some(b)) = (cur[k], cur[k + 1]) {
                let ms = b.saturating_duration_since(a).as_secs_f32() * 1000.0;
                f.last[k] = ms;
                f.sum[k] += ms as f64;
            }
        }
        f.frames += 1;
        f.cur = Default::default();
    }
}

fn split(stamps: &[Option<Instant>], start: Option<Instant>) -> Vec<f32> {
    let mut out = Vec::new();
    let mut prev = start;
    for s in stamps {
        out.push(match (prev, s) {
            (Some(a), Some(b)) => b.saturating_duration_since(a).as_secs_f32() * 1000.0,
            _ => 0.0,
        });
        if s.is_some() {
            prev = *s;
        }
    }
    out
}

fn report(mut s: ResMut<MainStamps>, mut last: ResMut<LastSplit>, rs: Res<RenderStamps>, time: Res<Time<Real>>, entities: Query<()>) {
    let ms = time.delta_secs() * 1000.0;
    let s = &mut *s;
    // Main split: previous frame's Last stamp -> First (wait for render/extract + present back-pressure), then
    // First -> PreUpdate -> ... -> Last.
    let cur = s.cur;
    let main = split(&cur, s.prev_last);
    s.prev_last = cur[4];
    last.0.copy_from_slice(&main[..5]);
    if !s.log {
        return;
    }
    let n = entities.iter().count();
    let delta = n as i64 - s.entities as i64;
    s.entities = n;
    s.median.push(ms);
    if s.median.len() > 301 {
        s.median.remove(0);
    }
    let mut m = s.median.clone();
    m.sort_by(f32::total_cmp);
    let med = m[m.len() / 2];
    let rf = rs.0.lock().map(|f| f.clone()).unwrap_or_default();
    let names = |v: &[f32], n: &[&str]| v.iter().zip(n).map(|(x, k)| format!("{k} {x:.1}")).collect::<Vec<_>>().join(", ");
    let main_names = ["wait->First", "->PreUpdate", "->Update", "->PostUpdate", "->Last"];
    if time.elapsed_secs() > 10.0 && ms > 40.0 && ms > 2.5 * med {
        info!(
            "hitch {ms:.0} ms (median {med:.1}): main [{}]; render (latest) [{}]; entities {n} ({delta:+})",
            names(&main, &main_names),
            names(&rf.last[..8], &RENDER[..8])
        );
    }
    for (k, v) in main.iter().enumerate() {
        s.sum[k] += *v as f64;
    }
    s.frames += 1;
    s.since += time.delta_secs();
    if s.since >= 30.0 {
        let f = s.frames.max(1) as f64;
        let mean: Vec<f32> = s.sum.iter().map(|x| (x / f) as f32).collect();
        let rmean: Vec<f32> = rf.sum.iter().map(|x| (x / rf.frames.max(1) as f64) as f32).collect();
        info!("frame split (mean over {} frames): main [{}]; render [{}]", s.frames, names(&mean, &main_names), names(&rmean[..8], &RENDER[..8]));
        s.sum = Default::default();
        s.frames = 0;
        s.since = 0.0;
        if let Ok(mut f) = rs.0.lock() {
            f.sum = Default::default();
            f.frames = 0;
        }
    }
}
