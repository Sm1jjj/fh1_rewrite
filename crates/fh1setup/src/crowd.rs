//! `crowd` group: Colorado's spectator crowds (`.pgeo` type 3, fh1_formats::crowd, docs/PROPS.md "Crowds")
//! and the sprite atlas the engine draws them with.
//!
//! - `crowd/colorado/crowds.bin`: `b"FH1CRWD1"`, u32 count, then per spectator (16 bytes, little-endian)
//!   f32 x, y, z (engine space: Z negated), u8 heading (/256 turn, collision space), u8 class (crowdclass
//!   id - 1), u8 extra byte 8, u8 0. Objects are contiguous ranges (see index.json).
//! - `crowd/colorado/index.json`: `objects` `[{name, named, first, count, min, max}]` (engine-space bbox),
//!   `classes` `[{index, name, sitting, models: [atlas model index]}]`, `atlas` layout.
//! - `crowd/sprites.dds`: `Spectators.zip/sprites.xds` (2048x768 DXT1, punch-through alpha) as BC3.
//! - `crowd/models/*.skinbin`, `crowd/anims/*.anim.bin`: copied as stored (near LOD, parsed by the engine);
//!   `crowd/textures/<name>.dds`: the spectator textures (DXT1 with punch-through alpha) as BC3.
//!   index.json `models` lists `{name, texture}` in atlas order.
//! - `crowd/colorado/walkers.json`: `paths` `[{object, named, class, length, min, max, knots: [{c, i, o, d}]}]`, the
//!   walker paths (knot centre / in-handle / out-handle in engine space, `d` = the 8 arc-length samples).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use fh1_formats::{crowd, zip::Archive};
use serde_json::json;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let media = disc.join("media");
    let mut spec = Archive::open(media.join("Spectators.zip"))?;
    let read = |ar: &mut Archive<std::fs::File>, name: &str| -> Result<Vec<u8>> {
        let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned().with_context(|| format!("Spectators.zip: {name}"))?;
        Ok(ar.read(&e)?)
    };
    let xml = read(&mut spec, "spectators.xml")?;
    let sp = crowd::parse_spectators_xml(String::from_utf8_lossy(&xml).trim_start_matches('\u{feff}'));

    // Sprite atlas, with its punch-through alpha.
    let (aw, ah, rgba) = crowd::decode_dxt1_alpha(&read(&mut spec, "sprites.xds")?)?;
    let transparent = rgba.chunks_exact(4).filter(|p| p[3] < 128).count() as f64 / (aw * ah) as f64;
    std::fs::write(out.join("sprites.dds"), crate::textures::rgba_to_dds(rgba, aw, ah)?.dds)?;

    // Near LOD: the skinned models and animations as stored (the engine parses them with fh1_formats), the
    // 256² DXT1 textures as DDS.
    for sub in ["models", "anims", "textures"] {
        std::fs::create_dir_all(out.join(sub))?;
    }
    let (mut models, mut anims) = (0, 0);
    for e in spec.entries.clone() {
        let name = e.name.replace('\\', "/");
        let file = name.rsplit('/').next().unwrap_or(&name).to_string();
        if name.ends_with(".skinbin") {
            let d = spec.read(&e)?;
            crowd::parse_skinbin(&d).with_context(|| name.clone())?;
            std::fs::write(out.join("models").join(&file), d)?;
            models += 1;
        } else if name.ends_with(".anim.bin") {
            std::fs::write(out.join("anims").join(&file), spec.read(&e)?)?;
            anims += 1;
        } else if let Some(stem) = file.strip_suffix(".dds.xds").filter(|_| name.starts_with("textures/")) {
            let (w, h, rgba) = crowd::decode_dxt1_alpha(&spec.read(&e)?).with_context(|| name.clone())?;
            std::fs::write(out.join("textures").join(format!("{stem}.dds")), crate::textures::rgba_to_dds(rgba, w, h)?.dds)?;
        }
    }

    let track = media.join("tracks/colorado");
    let mut ar = Archive::open(track.join("bin.zip"))?;
    let dir = out.join("colorado");
    std::fs::create_dir_all(&dir)?;
    let mut blob = b"FH1CRWD1".to_vec();
    blob.extend_from_slice(&0u32.to_le_bytes());
    let (mut objects, mut seen, mut total, mut groups) = (Vec::new(), HashSet::new(), 0usize, 0u32);
    let (mut walkers, mut walk_len) = (Vec::new(), 0f32);
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        // bin.zip repeats objects per streaming block: one copy each.
        if !n.ends_with(".pgeo") || !seen.insert(n.clone()) {
            continue;
        }
        let d = ar.read(&e)?;
        if crowd::pgeo_type(&d) != Some(3) {
            continue;
        }
        let c = crowd::parse(&d).with_context(|| n.clone())?;
        groups += c.paths.len() as u32;
        // Walker paths, engine space (Z negated); the engine evaluates them with WalkerPath::point_at
        // after negating Z back.
        let e3 = |v: [f32; 3]| [v[0], v[1], -v[2]];
        for p in &c.paths {
            walk_len += p.length;
            walkers.push(json!({
                "object": c.name,
                "named": c.named,
                "class": p.class,
                "length": p.length,
                "min": [p.bbox_min[0], p.bbox_min[1], -p.bbox_max[2]],
                "max": [p.bbox_max[0], p.bbox_max[1], -p.bbox_min[2]],
                "knots": p.knots.iter().map(|k| json!({"c": e3(k.centre), "i": e3(k.in_handle), "o": e3(k.out_handle), "d": k.distances})).collect::<Vec<_>>(),
            }));
        }
        if c.spectators.is_empty() {
            continue;
        }
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for s in &c.spectators {
            let p = [s.position[0], s.position[1], -s.position[2]];
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
            p.iter().for_each(|v| blob.extend_from_slice(&v.to_le_bytes()));
            blob.extend_from_slice(&[s.heading, s.class, s.extra[0], 0]);
        }
        objects.push(json!({"name": c.name, "named": c.named, "first": total, "count": c.spectators.len(), "min": lo, "max": hi}));
        total += c.spectators.len();
    }
    blob[8..12].copy_from_slice(&(total as u32).to_le_bytes());
    std::fs::write(dir.join("crowds.bin"), &blob)?;
    std::fs::write(dir.join("walkers.json"), serde_json::to_vec(&json!({"paths": walkers}))?)?;

    let atlas_index = |name: &str| sp.models.iter().position(|m| m.name == name);
    let classes: Vec<_> = sp
        .classes
        .iter()
        .map(|c| {
            // Seated sprites: the Sitting skeleton, the shoulder rider and the "Sat" poses (Standing skeleton,
            // sitting animations). INFERRED from the class / animation names.
            let n = c.name.to_ascii_lowercase();
            let sitting = c.skeleton == "Sitting" || n.ends_with("_sit") || n.starts_with("threesat") || n.starts_with("twosat") || n.starts_with("onesat");
            let models: Vec<usize> = sp.modelset(&c.modelset).iter().filter_map(|m| atlas_index(m)).collect();
            json!({"index": c.id.saturating_sub(1), "name": c.name, "sitting": sitting, "skeleton": c.skeleton, "suffix": sp.skeleton(&c.skeleton).map(|s| s.suffix.clone()).unwrap_or_default(), "idle": c.idle, "cheer": c.cheer, "models": models, "bb_offset_y": c.bb_offset_y})
        })
        .collect();
    let named: usize = objects.iter().filter(|o| o["named"] == true).map(|o| o["count"].as_u64().unwrap_or(0) as usize).sum();
    let index = json!({
        "objects": objects,
        "classes": classes,
        // 64x128 cells, 32 per row; per model 3 standing views (back, front, side) then 6 seated (back,
        // front, then four turning to the side). Model order = spectators.xml <models>.
        "models": sp.models.iter().map(|m| json!({"name": m.name, "texture": m.texture})).collect::<Vec<_>>(),
        "atlas": {"file": "../sprites.dds", "width": aw, "height": ah, "cell": [64, 128], "per_model": sp.sprites_per_model, "models": sp.models.iter().map(|m| m.name.clone()).collect::<Vec<_>>()},
    });
    std::fs::write(dir.join("index.json"), serde_json::to_vec_pretty(&index)?)?;
    println!(
        "[crowd] {models} models, {anims} anims; {} objects, {total} spectators ({named} in named crowd_* objects), {groups} walker paths ({walk_len:.0} m); atlas {}x{} ({:.0}% transparent), {} classes",
        index["objects"].as_array().map_or(0, |a| a.len()),
        aw,
        ah,
        100.0 * transparent,
        sp.classes.len()
    );
    Ok(())
}
