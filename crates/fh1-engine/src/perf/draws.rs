//! Draw counts per render phase for the gameplay recorder (2026-10-08: the render thread is the frame, rt_render 8-11 ms of
//! command encoding, and agents don't run perf tests, so the user's logs must say where the draws are). Each frame, after
//! Queue, counts views and draw commands of the 3D phases: a multidraw batch set or an instanced bin is one command, an
//! unbatchable mesh is one per entity. The recorder writes the last frame's numbers per second (record.rs).

use std::sync::atomic::{AtomicU32, Ordering};

use bevy::core_pipeline::core_3d::{AlphaMask3d, Opaque3d, Transparent3d};
use bevy::prelude::*;
use bevy::render::render_phase::{BinnedPhaseItem, ViewBinnedRenderPhases, ViewSortedRenderPhases};
use bevy::render::{Render, RenderApp, RenderSystems};

/// CSV column names, in [`COUNTS`] order.
pub(super) const NAMES: [&str; 8] = [
    "views_3d",
    "draws_opaque",
    "draws_mask",
    "draws_transparent",
    "views_shadow",
    "draws_shadow",
    "draws_unbatched",
    "draws_total",
];
pub(super) static COUNTS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

pub(super) fn plugin(app: &mut App) {
    if let Some(ra) = app.get_sub_app_mut(RenderApp) {
        ra.add_systems(Render, count.in_set(RenderSystems::Prepare));
    }
}

/// (views, draw commands, unbatchable draws).
fn binned<B: BinnedPhaseItem>(phases: &ViewBinnedRenderPhases<B>) -> [u32; 3] {
    let (mut draws, mut unbatched) = (0usize, 0usize);
    for p in phases.0.values() {
        let u: usize = p.unbatchable_meshes.values().map(|b| b.entities.len()).sum();
        let n: usize = p.non_mesh_items.values().map(|b| b.entities.len()).sum();
        draws += p.multidrawable_meshes.len() + p.batchable_meshes.len() + u + n;
        unbatched += u;
    }
    [phases.0.len() as u32, draws as u32, unbatched as u32]
}

fn count(
    opaque: Option<Res<ViewBinnedRenderPhases<Opaque3d>>>,
    mask: Option<Res<ViewBinnedRenderPhases<AlphaMask3d>>>,
    transparent: Option<Res<ViewSortedRenderPhases<Transparent3d>>>,
    shadow: Option<Res<ViewBinnedRenderPhases<bevy::pbr::Shadow>>>,
) {
    let o = opaque.as_deref().map_or([0; 3], binned);
    let m = mask.as_deref().map_or([0; 3], binned);
    let t = transparent.as_deref().map_or(0, |p| p.0.values().map(|v| v.items.len()).sum::<usize>() as u32);
    let s = shadow.as_deref().map_or([0; 3], binned);
    let values = [o[0], o[1], m[1], t, s[0], s[1], o[2] + m[2] + s[2], o[1] + m[1] + t + s[1]];
    for (c, v) in COUNTS.iter().zip(values) {
        c.store(v, Ordering::Relaxed);
    }
}
