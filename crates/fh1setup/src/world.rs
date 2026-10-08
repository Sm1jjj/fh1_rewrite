//! `world` group: Colorado collision + surface types (via `fh1-world`) and spawn points.
//!
//! Output (`world/colorado/`): `collision.bin` + `surfaces.json` (see `fh1_world::World::save`),
//! and `spawns.json`: the `start_location_NN` transforms from `Ribbon_00/TrackRoute000.xml`, each
//! checked against the collision and measured for clear road ahead (disc space as stored:
//! left-handed, +X east, +Z north, +Y up).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use fh1_world::World;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    build_track(disc, "colorado", &out.join("colorado"))
}

/// `media/tracks/<track>` -> `dir` (another Horizon-engine track, e.g. FH2's `Anthem`; docs/FH2_RECON.md).
pub fn build_track(disc: &Path, track: &str, dir: &Path) -> Result<()> {
    let (world, stats) = World::from_disc(disc, track)?;
    println!(
        "[world] {track}: {} squares, {} triangles ({} border duplicates and {} degenerate dropped), {} surfaces",
        stats.squares,
        world.tris.len(),
        stats.duplicate_tris,
        stats.degenerate_tris,
        world.surfaces.len()
    );
    world.save(dir)?;
    spawns(disc, track, &world, dir)
}

fn spawns(disc: &Path, track: &str, world: &World, dir: &Path) -> Result<()> {
    let path = disc.join("media/tracks").join(track).join("Ribbon_00/TrackRoute000.xml");
    let xml = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let doc = crate::xml::to_json(&xml)?;
    let named = match &doc["TrackRoute"]["NamedTransforms"]["NamedTransform"] {
        Value::Array(a) => a.clone(),
        v => vec![v.clone()],
    };
    let mut out = Vec::new();
    let mut on_road = 0;
    for n in &named {
        let name = n["name"].as_str().unwrap_or("");
        if !name.starts_with("start_location") {
            continue;
        }
        let t = &n["Transform"];
        let g = |k: &str| t[k].as_f64().unwrap_or(0.0) as f32;
        let (pos, facing) = ([g("pos.x"), g("pos.y"), g("pos.z")], [g("facing.x"), g("facing.y"), g("facing.z")]);
        // Ground directly below (within a few metres) — confirms these share the collision's space.
        let ground = world.raycast([pos[0], pos[1] + 3.0, pos[2]], [0.0, -1.0, 0.0], 8.0);
        let clear = clear_ahead(world, [pos[0], pos[1] + 1.0, pos[2]], [facing[0], 0.0, facing[2]], 300.0);
        if ground.is_some() {
            on_road += 1;
        }
        out.push(json!({
            "name": name,
            "position": pos,
            "facing": facing,
            "ground_y": ground.map(|h| h.point[1]),
            "surface": ground.and_then(|h| world.surface(h.surface)).map(|s| s.name.clone()),
            "clear_ahead": clear,
        }));
    }
    println!("[world] {} start locations, {on_road} on the collision mesh", out.len());
    std::fs::write(dir.join("spawns.json"), serde_json::to_vec_pretty(&json!({ "spawns": out }))?)?;
    Ok(())
}

/// Free distance along `dir` before the first wall that exists in free roam (routes bit 15),
/// capped at `max` metres.
pub(crate) fn clear_ahead(world: &World, origin: [f32; 3], dir: [f32; 3], max: f32) -> f32 {
    let len = (dir[0] * dir[0] + dir[2] * dir[2]).sqrt().max(1e-6);
    let d = [dir[0] / len, 0.0, dir[2] / len];
    let mut t0 = 0.0;
    while t0 < max {
        let o = [origin[0] + d[0] * t0, origin[1], origin[2] + d[2] * t0];
        match world.raycast(o, d, max - t0) {
            Some(h) if world.tris[h.tri as usize].routes & 0x8000 == 0 => t0 += h.t + 0.01,
            Some(h) => return t0 + h.t,
            None => return max,
        }
    }
    max
}
