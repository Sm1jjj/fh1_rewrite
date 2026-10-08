//! `grass` group: Colorado's `Grass_*` `.pgeo` objects (fh1_formats::grass, docs/PROPS.md) for the
//! engine to scatter at runtime, plus the grass textures they reference.
//!
//! - `grass/colorado/grass.bin`: the raw objects back to back (the engine parses and scatters them
//!   with fh1_formats::grass; 3,199 objects, ~30 MB, instead of 8.4 M pre-scattered blades).
//! - `grass/colorado/index.json`: per object `{name, offset, len, min, max, distances}` (bbox in
//!   engine space: Z negated) and `textures: {pvs index: file}`.
//! - `grass/colorado/textures/<pvs index>.dds`: the `.bix` textures the objects reference.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use fh1_formats::{grass, pvs, zip::Archive};
use serde_json::json;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let track = disc.join("media/tracks/colorado");
    let mut ar = Archive::open(track.join("bin.zip"))?;
    let pvs = pvs::parse(&std::fs::read(track.join("Ribbon_00/Colorado_00.pvs"))?)?;
    let dir = out.join("colorado");
    std::fs::create_dir_all(dir.join("textures"))?;

    let mut blob = Vec::new();
    let mut objects = Vec::new();
    let mut textures = BTreeMap::new();
    let mut seen = HashSet::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        // bin.zip repeats objects per streaming block: one copy each.
        if !n.ends_with(".pgeo") || !seen.insert(n.clone()) {
            continue;
        }
        let d = ar.read(&e)?;
        if grass::pgeo_type(&d) != Some(2) {
            continue;
        }
        let g = grass::parse(&d).with_context(|| n.clone())?;
        for b in &g.batches {
            textures.insert(b.texture, ());
        }
        objects.push(json!({
            "name": g.name,
            "offset": blob.len(),
            "len": d.len(),
            "min": [g.bbox_min[0], g.bbox_min[1], -g.bbox_max[2]],
            "max": [g.bbox_max[0], g.bbox_max[1], -g.bbox_min[2]],
            "distances": g.distances,
        }));
        blob.extend_from_slice(&d);
    }
    std::fs::write(dir.join("grass.bin"), &blob)?;

    let mut by_name = std::collections::HashMap::new();
    for e in &ar.entries {
        by_name.entry(e.name.to_ascii_lowercase()).or_insert_with(|| e.clone());
    }
    let mut files = serde_json::Map::new();
    for &i in textures.keys() {
        let Some(t) = pvs.textures.get(i as usize) else { continue };
        let Some(name) = t.file_name().filter(|n| n.ends_with(".bix")) else {
            println!("[grass] texture {i}: not a .bix on the disc");
            continue;
        };
        let (Some(h), Some(b)) = (by_name.get(&name.to_ascii_lowercase()), by_name.get(&name.to_ascii_lowercase().replace(".bix", "_b.bix"))) else {
            println!("[grass] texture {i}: {name} missing");
            continue;
        };
        let c = crate::textures::bix_to_dds(&ar.read(h)?, &ar.read(b)?)?;
        let file = format!("textures/{i}.dds");
        std::fs::write(dir.join(&file), &c.dds)?;
        files.insert(i.to_string(), json!(file));
    }
    std::fs::write(dir.join("index.json"), serde_json::to_vec_pretty(&json!({ "objects": objects, "textures": files }))?)?;
    println!("[grass] {} objects ({} MB), {} textures", objects.len(), blob.len() >> 20, files.len());
    Ok(())
}
