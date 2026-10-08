//! `FH1_PERF_LOG=1`: progressive-slowdown diagnostics. Every 10 s (real time) logs the frame time
//! (mean / p99 / max), the entity count, the asset counts that can leak, and entities per component
//! type with the biggest growers since the previous log. Component counts come straight from the
//! archetypes, so every system (scenery, props, crowd, grass, glows, anim, smash, colliders...) shows
//! up without hooks in its own file.
//! `FH1_PERF_TOUR=1` (with the log) moves the car along a long loop across Colorado (a new point every
//! 3 s, held on the ground like FH1_TELEPORT) so streaming loads and unloads all the time.
//! `FH1_PERF_TOUR=drive` (with FH1_AUTODRIVE=1) lets the car drive and only moves it to the next stop
//! after 5 s below 2 m/s (stuck), so physics, smashing and real distance are exercised too.
//! Frame pacing (default on, FH1_FRAMEPACE=0 = off) and its 30 s pacing log: [`pacing`]; wheel interpolation: [`interp`].

mod draws;
mod meshes;
mod exec;
mod hitch;
mod p6;
pub mod interp;
mod pacing;
mod present;
pub(crate) mod record;
mod stack;
mod watchdog;
pub mod writer;
pub(crate) use watchdog::{system_span_layer, watch};

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::render::storage::ShaderBuffer;
use fh1_render::car_material::{FxCarMaterial, FxRawStandard};
use fh1_render::glow::GlowMaterial;
use fh1_render::FxMaterial;

/// Seconds between reports.
const EVERY: f32 = 10.0;

pub struct PerfPlugin;

impl Plugin for PerfPlugin {
    fn build(&self, app: &mut App) {
        writer::plugin(app);
        pacing::plugin(app);
        present::plugin(app);
        exec::plugin(app);
        interp::plugin(app);
        hitch::plugin(app);
        p6::plugin(app);
        record::plugin(app);
        if !std::env::var("FH1_PERF_LOG").is_ok_and(|v| v == "1") {
            return;
        }
        app.init_resource::<PerfState>().add_systems(Last, (record_frame, report.run_if(report_due)).chain());
        if std::env::var("FH1_PERF_TOUR").is_ok_and(|v| v == "1" || v == "drive") {
            app.add_systems(Update, tour);
        }
    }
}

#[derive(Resource, Default)]
struct PerfState {
    /// Frame times (ms) since the last report.
    frames: Vec<f32>,
    since: f32,
    elapsed: f32,
    /// Component short name -> entity count at the last report.
    prev: HashMap<String, usize>,
    prev_assets: Vec<usize>,
}

fn record_frame(time: Res<Time<Real>>, mut s: ResMut<PerfState>) {
    let dt = time.delta_secs();
    s.frames.push(dt * 1000.0);
    s.since += dt;
    s.elapsed += dt;
}

fn report_due(s: Res<PerfState>) -> bool {
    s.since >= EVERY
}

fn count<A: Asset>(world: &World) -> usize {
    world.get_resource::<Assets<A>>().map_or(0, |a| a.len())
}

fn report(world: &mut World) {
    let assets = [
        ("Mesh", count::<Mesh>(world)),
        ("Image", count::<Image>(world)),
        ("StdMat", count::<StandardMaterial>(world)),
        ("RawStdMat", count::<FxRawStandard>(world)),
        ("FxMat", count::<FxMaterial>(world)),
        ("FxCarMat", count::<FxCarMaterial>(world)),
        ("GlowMat", count::<GlowMaterial>(world)),
        ("ShaderBuf", count::<ShaderBuffer>(world)),
    ];
    // Entities per component type. Release builds have no component names, so a fixed list of the
    // types worth watching is resolved to ids; other components show up as "#<id>" when they change.
    let watched: Vec<(&str, Option<bevy::ecs::component::ComponentId>)> = vec![
        ("Mesh3d", world.component_id::<Mesh3d>()),
        ("ChildOf", world.component_id::<ChildOf>()),
        ("FxMat", world.component_id::<MeshMaterial3d<FxMaterial>>()),
        ("StdMat", world.component_id::<MeshMaterial3d<StandardMaterial>>()),
        ("RawStdMat", world.component_id::<MeshMaterial3d<FxRawStandard>>()),
        ("CasterMat", world.component_id::<MeshMaterial3d<fh1_render::shadow::FxCasterMaterial>>()),
        ("CasterProxy", world.component_id::<fh1_render::shadow::CasterProxy>()),
        ("CubeCandidate", world.component_id::<fh1_render::reflect::CubeCandidate>()),
        ("GlowMatC", world.component_id::<MeshMaterial3d<GlowMaterial>>()),
        ("CarMat", world.component_id::<MeshMaterial3d<FxCarMaterial>>()),
        ("SkinnedMesh", world.component_id::<bevy::mesh::skinning::SkinnedMesh>()),
        ("Debris", world.component_id::<crate::smash::Debris>()),
        ("VisRange", world.component_id::<bevy::camera::visibility::VisibilityRange>()),
        ("PointLight", world.component_id::<PointLight>()),
        ("Text", world.component_id::<Text>()),
        ("Node", world.component_id::<Node>()),
    ];
    let names: HashMap<bevy::ecs::component::ComponentId, &str> = watched.iter().filter_map(|(n, id)| Some(((*id)?, *n))).collect();
    let mut by: HashMap<String, usize> = HashMap::new();
    for arch in world.archetypes().iter() {
        if arch.is_empty() {
            continue;
        }
        for id in arch.components() {
            let key = names.get(id).map_or_else(|| format!("#{}", id.index()), |n| n.to_string());
            *by.entry(key).or_default() += arch.len() as usize;
        }
    }
    // Orphans: ChildOf entities whose parent is gone (leaked children).
    let mut orphans = 0;
    let mut q = world.query::<&ChildOf>();
    for c in q.iter(world) {
        if world.get_entity(c.parent()).is_err() {
            orphans += 1;
        }
    }
    by.insert("orphans".into(), orphans);
    // GPU texture memory estimate (all mips, all layers) of every Image asset.
    let image_mb = world.get_resource::<Assets<Image>>().map_or(0.0, |imgs| {
        imgs.iter()
            .map(|(_, i)| {
                let d = &i.texture_descriptor;
                let f = d.format;
                let (bw, bh) = f.block_dimensions();
                let bytes = f.block_copy_size(None).unwrap_or(4) as f64;
                let mut sum = 0.0;
                for m in 0..d.mip_level_count.max(1) {
                    let w = (d.size.width >> m).max(1).div_ceil(bw) as f64;
                    let h = (d.size.height >> m).max(1).div_ceil(bh) as f64;
                    sum += w * h * bytes;
                }
                sum * d.size.depth_or_array_layers.max(1) as f64
            })
            .sum::<f64>()
            / (1024.0 * 1024.0)
    });
    let entities: usize = world.archetypes().iter().map(|a| a.len() as usize).sum();
    let car = world.query_filtered::<&GlobalTransform, With<crate::Car>>().iter(world).next().map(|t| t.translation());

    let mut s = world.resource_mut::<PerfState>();
    let mut f = std::mem::take(&mut s.frames);
    f.sort_by(f32::total_cmp);
    let n = f.len().max(1);
    let mean = f.iter().sum::<f32>() / n as f32;
    let p99 = f.get((n * 99 / 100).min(n - 1)).copied().unwrap_or(0.0);
    let max = f.last().copied().unwrap_or(0.0);
    let t = s.elapsed;
    s.since = 0.0;

    let asset_line: Vec<String> = assets
        .iter()
        .enumerate()
        .map(|(i, (k, v))| match s.prev_assets.get(i) {
            Some(p) if *p != *v => format!("{k} {v} ({:+})", *v as i64 - *p as i64),
            _ => format!("{k} {v}"),
        })
        .collect();
    info!(
        "perf t={t:.0}s: frame mean {mean:.2} ms, p99 {p99:.2}, max {max:.2} ({n} frames); entities {entities}; car {:?}",
        car.map(|c| c.round())
    );
    info!("perf assets: {} | textures ~{image_mb:.0} MB", asset_line.join(", "));
    let mut top: Vec<(&String, &usize)> = by.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    let top_line: Vec<String> = top.iter().filter(|(k, _)| !k.starts_with('#')).map(|(k, v)| format!("{k} {v}")).collect();
    info!("perf components: {}", top_line.join(", "));
    if !s.prev.is_empty() {
        let mut grow: Vec<(String, i64)> = by.iter().map(|(k, v)| (k.clone(), *v as i64 - *s.prev.get(k).unwrap_or(&0) as i64)).filter(|(_, d)| *d != 0).collect();
        grow.sort_by_key(|(_, d)| -d.abs());
        let g: Vec<String> = grow.iter().take(10).map(|(k, d)| format!("{k} {d:+}")).collect();
        info!("perf components (changed): {}", if g.is_empty() { "none".into() } else { g.join(", ") });
    }
    s.prev = by;
    s.prev_assets = assets.iter().map(|(_, v)| *v).collect();
}

/// Seconds per tour stop and the loop: a Lissajous figure over most of the map (engine space, m).
const TOUR_STEP: f32 = 3.0;
const TOUR_STOPS: usize = 400;

#[allow(clippy::type_complexity)]
fn tour(
    time: Res<Time<Real>>,
    track: Res<crate::track::Track>,
    mut cars: Query<&mut crate::Car>,
    mut st: Local<(f32, usize, Option<(Vec3, f32)>)>,
    mut drive: Local<(Option<bool>, f32, Option<Vec3>, f32)>,
) {
    let driving = *drive.0.get_or_insert_with(|| std::env::var("FH1_PERF_TOUR").is_ok_and(|v| v == "drive"));
    if driving {
        // Stuck detector: below 2 m/s for 5 s -> jump to the next stop once.
        let dt = time.delta_secs().max(1e-4);
        let Some(pos) = cars.iter().next().map(|c| c.0.position) else { return };
        let speed = drive.2.map_or(0.0, |p| (pos - p).length() / dt);
        drive.2 = Some(pos);
        drive.1 = if speed < 2.0 { drive.1 + dt } else { 0.0 };
        drive.3 += (speed * dt).min(10.0);
        if drive.1 < 5.0 {
            return;
        }
        drive.1 = 0.0;
        st.2 = None;
        info!("perf tour: stuck, moving on (driven {:.1} km)", drive.3 / 1000.0);
    }
    st.0 += time.delta_secs();
    if st.2.is_none() || st.0 >= TOUR_STEP {
        st.0 = 0.0;
        // Try stops until one has ground under it.
        for _ in 0..TOUR_STOPS {
            st.1 = (st.1 + 1) % TOUR_STOPS;
            let a = st.1 as f32 / TOUR_STOPS as f32 * std::f32::consts::TAU;
            let p = Vec3::new(3000.0 * a.sin(), 2000.0, 2600.0 * (2.0 * a).sin());
            if let Some(h) = track.ground.ray(p, Vec3::NEG_Y, 4000.0) {
                let next = Vec3::new(3000.0 * (a + 0.01).sin(), 0.0, 2600.0 * (2.0 * (a + 0.01)).sin());
                let d = next - Vec3::new(p.x, 0.0, p.z);
                st.2 = Some((h.point, (-d.x).atan2(-d.z)));
                break;
            }
        }
    }
    let Some((point, yaw)) = st.2 else { return };
    for mut car in &mut cars {
        car.0.place(point, yaw);
    }
    if driving {
        // One placement per stuck event; then the car drives on its own.
        st.2 = Some((point, yaw));
        st.0 = f32::NEG_INFINITY;
    }
}
