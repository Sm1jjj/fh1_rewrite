//! P6 (render-thread frame cost): the engine-side lever and the in-run A/B over every P6 lever. docs/PERF.md "P6".
//!
//! - **Light clustering off** (default on; `FH1_CLUSTERS=1` = Bevy's GPU clustering as before). FH1 spawns no point or
//!   spot lights (lamps and glows are the game's own shaders), yet Bevy 0.19 ran its 5-pass GPU clustering and a
//!   buffer readback for every camera, every frame. While the world has at most 4 `PointLight`/`SpotLight`s, Bevy's CPU path
//!   is used and every camera gets `ClusterConfig::Single` (one cluster; a few lights assign fine); more than 4
//!   point/spot lights restore both. Not `ClusterConfig::None`: Bevy 0.19's `Clusters::clear` (its path for None) never converts the clusters from
//!   GPU to CPU storage, and extraction then logs "Clusterable objects must have been in CPU mode" every frame.
//! - **`FH1_P6_AB=1`** (or a comma list of mode names from [`MODES`]): after `FH1_P6_START` s (20) cycles the modes,
//!   `FH1_P6_SECS` s each (6, the first 1.5 s of each discarded), and after every full cycle logs per mode the frame
//!   time (mean, p50, p99) and, with `FH1_PERF_LOG=1`, the render thread's Prepare / Render / total. All modes see the
//!   same place and machine load; park the car (default festival spawn) for a steady scene.

use std::sync::atomic::{AtomicU8, Ordering};

use bevy::light::cluster::{ClusterConfig, GlobalClusterGpuSettings, GlobalClusterSettings};
use bevy::prelude::*;

use super::hitch::RenderStamps;
use crate::ui::minimap::P6_MAP_OFF;
use fh1_render::reflect::{P6_CUBE, P6_CUBE_EVERY};
use fh1_render::shadow::{P6_LAZY_CASTERS, P6_MAIN_ONLY};

/// A/B override for the clustering lever: 0 = env/default, 1 = clustering off, 2 = Bevy's GPU clustering.
pub static P6_CLUSTERS: AtomicU8 = AtomicU8::new(0);

pub fn plugin(app: &mut App) {
    let env_off = std::env::var("FH1_CLUSTERS").map_or(true, |v| v != "1");
    app.insert_resource(ClusterState { env_off, saved: None }).add_systems(Update, clustering);
    let ab = std::env::var("FH1_P6_AB").ok().filter(|v| !v.is_empty() && v != "0");
    if let Some(list) = ab {
        let modes: Vec<usize> = if list == "1" {
            (0..MODES.len()).collect()
        } else {
            list.split(',').filter_map(|n| MODES.iter().position(|m| m.name == n.trim())).collect()
        };
        if modes.is_empty() {
            warn!("FH1_P6_AB: no known mode in {list:?}");
            return;
        }
        let num = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        app.insert_resource(Ab {
            stats: vec![Stats::default(); modes.len()],
            modes,
            start: num("FH1_P6_START", 20.0),
            secs: num("FH1_P6_SECS", 6.0).max(2.0),
            ..default()
        })
        .add_systems(Last, ab_step);
    }
}

#[derive(Resource)]
struct ClusterState {
    env_off: bool,
    /// Bevy's GPU clustering settings while we hold them off.
    saved: Option<GlobalClusterGpuSettings>,
}

/// Marks a camera whose `ClusterConfig::Single` this module inserted.
#[derive(Component)]
struct NoClusters;

#[allow(clippy::type_complexity)]
fn clustering(
    mut commands: Commands,
    mut st: ResMut<ClusterState>,
    settings: Option<ResMut<GlobalClusterSettings>>,
    lights: Query<(), Or<(With<PointLight>, With<SpotLight>)>>,
    cams: Query<(Entity, Has<NoClusters>), With<Camera3d>>,
) {
    let Some(mut settings) = settings else { return };
    let off = match P6_CLUSTERS.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => st.env_off,
    } && lights.iter().count() <= 4;
    if off {
        if settings.gpu_clustering.is_some() {
            st.saved = settings.gpu_clustering.take();
        }
    } else if settings.gpu_clustering.is_none() && st.saved.is_some() {
        settings.gpu_clustering = st.saved.take();
    }
    for (e, ours) in &cams {
        if off && !ours {
            commands.entity(e).insert((ClusterConfig::Single, NoClusters));
        } else if !off && ours {
            commands.entity(e).remove::<(ClusterConfig, NoClusters)>();
        }
    }
}

/// One A/B mode: lever overrides (0 = the default, see each static) plus measurement-only camera switches.
struct Mode {
    name: &'static str,
    clusters: u8,
    main_only: u8,
    lazy: u8,
    cube: u8,
    every: u32,
    ui_off: bool,
    map_off: bool,
}

const fn mode(name: &'static str) -> Mode {
    Mode { name, clusters: 0, main_only: 0, lazy: 0, cube: 0, every: 0, ui_off: false, map_off: false }
}

/// `new` = every default; the others change one lever from it; `old` = all P6 levers off (pre-P6 behaviour).
const MODES: &[Mode] = &[
    Mode { clusters: 2, main_only: 2, lazy: 2, cube: 1, ..mode("old") },
    mode("new"),
    Mode { clusters: 2, ..mode("gpuclu") },
    Mode { main_only: 2, ..mode("allcasc") },
    Mode { lazy: 1, ..mode("lazy") },
    Mode { cube: 1, ..mode("cubekeep") },
    Mode { cube: 2, ..mode("cubesleep") },
    Mode { cube: 3, ..mode("cubeoff") },
    Mode { every: 4, ..mode("cube4") },
    Mode { ui_off: true, ..mode("uioff") },
    Mode { map_off: true, ..mode("mapoff") },
];

#[derive(Default, Clone)]
struct Stats {
    frames: Vec<f32>,
    /// Render-thread sums (ms): Prepare, Render, all sets.
    prepare: f64,
    render: f64,
    total: f64,
    rt_frames: u32,
    /// Render-thread total per frame (ms), for its median.
    totals: Vec<f32>,
}

#[derive(Resource, Default)]
struct Ab {
    modes: Vec<usize>,
    stats: Vec<Stats>,
    start: f32,
    secs: f32,
    current: usize,
    mode_t: f32,
    cycles: u32,
    applied: bool,
}

fn apply(m: &Mode, ui: &mut Query<&mut Camera, With<bevy::ui::IsDefaultUiCamera>>) {
    P6_CLUSTERS.store(m.clusters, Ordering::Relaxed);
    P6_MAIN_ONLY.store(m.main_only, Ordering::Relaxed);
    P6_LAZY_CASTERS.store(m.lazy, Ordering::Relaxed);
    P6_CUBE.store(m.cube, Ordering::Relaxed);
    P6_CUBE_EVERY.store(m.every, Ordering::Relaxed);
    P6_MAP_OFF.store(m.map_off, Ordering::Relaxed);
    for mut cam in ui.iter_mut() {
        if cam.is_active == m.ui_off {
            cam.is_active = !m.ui_off;
        }
    }
}

fn ab_step(mut ab: ResMut<Ab>, time: Res<Time<Real>>, rs: Option<Res<RenderStamps>>, mut ui: Query<&mut Camera, With<bevy::ui::IsDefaultUiCamera>>) {
    if time.elapsed_secs() < ab.start {
        return;
    }
    let ab = &mut *ab;
    if !ab.applied {
        apply(&MODES[ab.modes[ab.current]], &mut ui);
        ab.applied = true;
        ab.mode_t = 0.0;
        return;
    }
    let dt = time.delta_secs();
    ab.mode_t += dt;
    if ab.mode_t > 1.5 {
        let s = &mut ab.stats[ab.current];
        s.frames.push(dt * 1000.0);
        if let Some(f) = rs.as_ref().and_then(|r| r.0.lock().ok().map(|f| f.last)) {
            s.prepare += f[5] as f64;
            s.render += f[6] as f64;
            let t = f[..8].iter().sum::<f32>();
            s.total += t as f64;
            s.totals.push(t);
            s.rt_frames += 1;
        }
    }
    if ab.mode_t < ab.secs {
        return;
    }
    ab.current = (ab.current + 1) % ab.modes.len();
    apply(&MODES[ab.modes[ab.current]], &mut ui);
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
            let mut v = s.frames.clone();
            v.sort_by(f32::total_cmp);
            let n = v.len().max(1);
            let mean = v.iter().sum::<f32>() / n as f32;
            let pct = |p: usize| v.get(v.len() * p / 100).copied().unwrap_or(0.0);
            let rt = s.rt_frames.max(1) as f64;
            let mut t = s.totals.clone();
            t.sort_by(f32::total_cmp);
            let rt50 = t.get(t.len() / 2).copied().unwrap_or(0.0);
            format!(
                "{:>9}: mean {mean:5.2} p50 {:5.2} p99 {:5.2} | rt prepare {:4.2} render {:5.2} total {:5.2} p50 {rt50:5.2} ({} frames)",
                MODES[m].name,
                pct(50),
                pct(99),
                s.prepare / rt,
                s.render / rt,
                s.total / rt,
                v.len()
            )
        })
        .collect();
    info!("P6 AB after {} cycles:\n  {}", ab.cycles, lines.join("\n  "));
}
