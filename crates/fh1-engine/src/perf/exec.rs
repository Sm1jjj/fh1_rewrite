//! Render-schedule executor experiment (2026-10-08 perf). The span profile shows a ~0.15-0.22 ms floor on near-empty
//! systems in both worlds (`prepare_erased_assets<GrassMaterial>` 0.16 ms with no grass material changing,
//! `check_entities_needing_specialization<MarkerMaterial>` 0.18 ms), even with the lock-free profiler. That is the
//! multi-threaded executor's per-system hand-off (task spawn, thread park / unpark on Windows), and the Render schedule
//! has dozens of systems on its critical path, so the render thread (= the frame, ~20 ms) may be mostly hand-offs.
//! Bevy's single-threaded executor runs the same systems back to back on the render thread (systems that `par_iter`
//! still use the pool).
//!
//! `FH1_RENDER_EXEC=multi` (Bevy's default, used when unset) | `single` | `ab` (switches every [`AB_SECS`] s between
//! frames, for a same-run comparison; the recorder's `render_exec` column says which).

use std::sync::atomic::{AtomicU8, Ordering};

use bevy::ecs::schedule::{MultiThreadedExecutor, Schedules, SingleThreadedExecutor};
use bevy::prelude::*;
use bevy::render::{ExtractSchedule, Render, RenderApp};

const AB_SECS: f32 = 20.0;

/// 1 = multi, 2 = single (0 = untouched).
static CURRENT: AtomicU8 = AtomicU8::new(0);

pub(crate) fn current_name() -> &'static str {
    match CURRENT.load(Ordering::Relaxed) {
        1 => "multi",
        2 => "single",
        _ => "",
    }
}

#[derive(Resource)]
struct ExecAb {
    single: bool,
    ab: bool,
    started: std::time::Instant,
    applied: Option<bool>,
}

pub(super) fn plugin(app: &mut App) {
    let spec = std::env::var("FH1_RENDER_EXEC").unwrap_or_default().to_ascii_lowercase();
    let (single, ab) = match spec.as_str() {
        "single" => (true, false),
        "ab" => (false, true),
        _ => (false, false),
    };
    CURRENT.store(if single { 2 } else { 1 }, Ordering::Relaxed);
    if !single && !ab {
        return;
    }
    if let Some(ra) = app.get_sub_app_mut(RenderApp) {
        ra.insert_resource(ExecAb { single, ab, started: std::time::Instant::now(), applied: None }).add_systems(ExtractSchedule, switch);
    }
}

/// Runs in the render world before Render (which is idle in `Schedules` then): swaps its executor when due.
fn switch(world: &mut World) {
    let want = {
        let mut s = world.resource_mut::<ExecAb>();
        if s.ab {
            let phase = (s.started.elapsed().as_secs_f32() / AB_SECS) as u64;
            s.single = phase % 2 == 1;
        }
        if s.applied == Some(s.single) {
            return;
        }
        s.applied = Some(s.single);
        s.single
    };
    let mut schedules = world.resource_mut::<Schedules>();
    let Some(render) = schedules.get_mut(Render) else { return };
    if want {
        render.set_executor(SingleThreadedExecutor::new());
    } else {
        render.set_executor(MultiThreadedExecutor::new());
    }
    CURRENT.store(if want { 2 } else { 1 }, Ordering::Relaxed);
    info!("render schedule executor: {}", if want { "single-threaded" } else { "multi-threaded" });
}
