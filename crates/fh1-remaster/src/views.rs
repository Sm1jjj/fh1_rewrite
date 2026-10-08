//! W4 views: what each extra camera costs the render thread, and the in-run A/B to measure it (docs/REMASTER.md
//! "Performance"). The festival frame is the render thread (docs/PERF.md P5b/P6): every view pays Bevy's per-view
//! prepare, binning and pass encoding, so the cheapest view is the one that doesn't exist.
//!
//! `FH1_VIEWS_AB=<modes>` (comma list of [`MODES`] names, or `1` = all): after `FH1_VIEWS_START` s (20) cycles the
//! modes `FH1_VIEWS_SECS` s each (4; the first second of each discarded) and after every full cycle logs per mode the
//! frame time and the render thread's extract..post-cleanup time (p50 / mean). Interleaved short windows because the
//! festival scene drifts by +-2 ms over tens of seconds.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::render::{ExtractSchedule, Render, RenderApp, RenderSystems};
use bevy::ui::IsDefaultUiCamera;

/// An otherwise unused render layer: a camera switched to it sees nothing (an empty view).
const EMPTY_LAYER: usize = 31;

struct Mode {
    name: &'static str,
    /// The window UI camera (HUD + Bevy UI) on / off.
    ui_active: bool,
    /// The window UI camera kept but looking at an empty layer: its fixed per-view cost alone.
    ui_empty: bool,
}

const MODES: &[Mode] = &[
    Mode { name: "base", ui_active: true, ui_empty: false },
    Mode { name: "uioff", ui_active: false, ui_empty: false },
    Mode { name: "uiempty", ui_active: true, ui_empty: true },
];

/// Render-thread frame time (ms), written by the render app.
#[derive(Resource, Clone, Default)]
struct RtClock(Arc<Mutex<(Option<Instant>, Option<f32>)>>);

#[derive(Default, Clone)]
struct Stats {
    frames: Vec<f32>,
    rt: Vec<f32>,
}

#[derive(Resource)]
struct Ab {
    modes: Vec<usize>,
    stats: Vec<Stats>,
    start: f32,
    secs: f32,
    current: usize,
    mode_t: f32,
    cycles: u32,
    applied: bool,
    /// UI camera layers saved while `ui_empty` swaps them out.
    saved_layers: Vec<(Entity, RenderLayers)>,
}

pub fn plugin(app: &mut App) {
    let Some(list) = std::env::var("FH1_VIEWS_AB").ok().filter(|v| !v.is_empty() && v != "0") else { return };
    let modes: Vec<usize> = if list == "1" {
        (0..MODES.len()).collect()
    } else {
        list.split(',').filter_map(|n| MODES.iter().position(|m| m.name == n.trim())).collect()
    };
    if modes.is_empty() {
        warn!("FH1_VIEWS_AB: no known mode in {list:?}");
        return;
    }
    let num = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let clock = RtClock::default();
    app.insert_resource(clock.clone()).insert_resource(Ab {
        stats: vec![Stats::default(); modes.len()],
        modes,
        start: num("FH1_VIEWS_START", 20.0),
        secs: num("FH1_VIEWS_SECS", 4.0).max(2.0),
        current: 0,
        mode_t: 0.0,
        cycles: 0,
        applied: false,
        saved_layers: Vec::new(),
    });
    app.add_systems(Last, ab_step);
    if let Some(render) = app.get_sub_app_mut(RenderApp) {
        render
            .insert_resource(clock)
            .add_systems(ExtractSchedule, rt_begin)
            .add_systems(Render, rt_end.in_set(RenderSystems::PostCleanup));
    }
}

fn rt_begin(c: Res<RtClock>) {
    if let Ok(mut g) = c.0.lock() {
        g.0 = Some(Instant::now());
    }
}

fn rt_end(c: Res<RtClock>) {
    if let Ok(mut g) = c.0.lock() {
        if let Some(t) = g.0.take() {
            g.1 = Some(t.elapsed().as_secs_f32() * 1000.0);
        }
    }
}

fn apply(ab: &mut Ab, m: &Mode, commands: &mut Commands, ui: &mut Query<(Entity, &mut Camera, Option<&RenderLayers>), With<IsDefaultUiCamera>>) {
    for (e, mut cam, layers) in ui.iter_mut() {
        if cam.is_active != m.ui_active {
            cam.is_active = m.ui_active;
        }
        let saved = ab.saved_layers.iter().position(|s| s.0 == e);
        match (m.ui_empty, saved) {
            (true, None) => {
                ab.saved_layers.push((e, layers.cloned().unwrap_or_default()));
                commands.entity(e).insert(RenderLayers::layer(EMPTY_LAYER));
            }
            (false, Some(i)) => {
                let (_, l) = ab.saved_layers.swap_remove(i);
                commands.entity(e).insert(l);
            }
            _ => {}
        }
    }
}

fn ab_step(
    mut commands: Commands,
    mut ab: ResMut<Ab>,
    clock: Res<RtClock>,
    time: Res<Time<Real>>,
    mut ui: Query<(Entity, &mut Camera, Option<&RenderLayers>), With<IsDefaultUiCamera>>,
) {
    if time.elapsed_secs() < ab.start {
        return;
    }
    let ab = &mut *ab;
    if !ab.applied {
        apply(ab, &MODES[ab.modes[ab.current]], &mut commands, &mut ui);
        ab.applied = true;
        ab.mode_t = 0.0;
        return;
    }
    let dt = time.delta_secs();
    ab.mode_t += dt;
    let rt = clock.0.lock().ok().and_then(|mut g| g.1.take());
    if ab.mode_t > 1.0 {
        let s = &mut ab.stats[ab.current];
        s.frames.push(dt * 1000.0);
        s.rt.extend(rt);
    }
    if ab.mode_t < ab.secs {
        return;
    }
    ab.current = (ab.current + 1) % ab.modes.len();
    apply(ab, &MODES[ab.modes[ab.current]], &mut commands, &mut ui);
    ab.mode_t = 0.0;
    if ab.current != 0 {
        return;
    }
    ab.cycles += 1;
    let lines: Vec<String> = ab
        .modes
        .iter()
        .zip(&ab.stats)
        .map(|(&m, s)| {
            let (f50, f99, fmean) = pcts(&s.frames);
            let (r50, r99, rmean) = pcts(&s.rt);
            format!(
                "{:>8}: frame p50 {f50:5.2} p99 {f99:5.2} mean {fmean:5.2} | rt p50 {r50:5.2} p99 {r99:5.2} mean {rmean:5.2} ({} frames)",
                MODES[m].name,
                s.frames.len()
            )
        })
        .collect();
    info!("views AB after {} cycles:\n  {}", ab.cycles, lines.join("\n  "));
}

/// (p50, p99, mean).
fn pcts(v: &[f32]) -> (f32, f32, f32) {
    let mut v = v.to_vec();
    v.sort_by(f32::total_cmp);
    let at = |p: usize| v.get(v.len() * p / 100).copied().unwrap_or(0.0);
    (at(50), at(99), v.iter().sum::<f32>() / v.len().max(1) as f32)
}
