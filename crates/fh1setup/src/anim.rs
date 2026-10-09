//! `anim` group: Colorado's animated objects (`.pgeo` type 4, fh1_formats::granny) and the scene
//! instances that place them (type 5, fh1_formats::props::track_anim), for fh1-engine `anim.rs`.
//!
//! - `anim/colorado/objects/<i>.pgeo`: the raw type-4 objects (scene object id = i, bin.zip name order);
//!   the engine parses them with fh1_formats::granny (meshes, skeletons, animation curves).
//! - `anim/colorado/index.json`: `objects: [{name, file, textures: {slot: file}}]` and
//!   `instances: [{object, scene, free_roam, matrix (collision space, rows = axes, row 3 = position),
//!   distances}]`.
//! - `anim/colorado/textures/<pvs index>.dds`: the textures the objects reference (`.bix` and CAFF).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use fh1_formats::{granny, props, pvs, zip::Archive};
use serde_json::json;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let track = disc.join("media/tracks/colorado");
    let mut ar = Archive::open(track.join("bin.zip"))?;
    let pvs = pvs::parse(&std::fs::read(track.join("Ribbon_00/Colorado_00.pvs"))?)?;
    let dir = out.join("colorado");
    std::fs::create_dir_all(dir.join("objects"))?;
    std::fs::create_dir_all(dir.join("textures"))?;

    // Type-4 objects in bin.zip name order (= the scenes' object ids, as props::track_anim).
    let mut seen = HashSet::new();
    let mut pgeos: Vec<_> = ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pgeo") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    pgeos.sort_by_key(|e| e.name.to_ascii_lowercase());
    let mut by_name = HashMap::new();
    for e in &ar.entries {
        by_name.entry(e.name.to_ascii_lowercase()).or_insert_with(|| e.clone());
    }
    let mut objects = Vec::new();
    let mut wanted: BTreeMap<u32, ()> = BTreeMap::new();
    for e in pgeos {
        let d = ar.read(&e)?;
        if d.get(0x30..0x34) != Some(&[0, 0, 0, 4]) {
            continue;
        }
        let o = granny::parse_anim_object(&d).with_context(|| e.name.clone())?;
        let i = objects.len();
        std::fs::write(dir.join(format!("objects/{i}.pgeo")), &d)?;
        for &t in &o.textures {
            wanted.insert(t, ());
        }
        objects.push((o.name, o.textures, format!("objects/{i}.pgeo")));
    }
    // Textures.
    let mut files: HashMap<u32, String> = HashMap::new();
    for &t in wanted.keys() {
        let Some(tex) = pvs.textures.get(t as usize) else { continue };
        let Some(name) = tex.file_name() else { continue };
        let ln = name.to_ascii_lowercase();
        let converted = if ln.ends_with(".bix") {
            match (by_name.get(&ln), by_name.get(&ln.replace(".bix", "_b.bix"))) {
                (Some(h), Some(b)) => crate::textures::bix_to_dds(&ar.read(h)?, &ar.read(b)?).ok(),
                _ => None,
            }
        } else {
            match by_name.get(&ln) {
                Some(e) => crate::textures::caff_to_dds(&ar.read(e)?).ok(),
                None => None,
            }
        };
        let Some(c) = converted else {
            println!("[anim] texture {t} ({name}): not converted");
            continue;
        };
        let file = format!("textures/{t}.dds");
        std::fs::write(dir.join(&file), &c.dds)?;
        files.insert(t, file);
    }
    let mut objects_json: Vec<_> = objects
        .iter()
        .map(|(name, tex, file)| {
            let slots: serde_json::Map<String, serde_json::Value> = tex.iter().enumerate().filter_map(|(i, t)| Some((i.to_string(), json!(files.get(t)?)))).collect();
            json!({ "name": name, "file": file, "textures": slots })
        })
        .collect();
    crate::airborne_objects::append(disc, &dir, &mut objects_json)?;
    // Instances from the scenes.
    let anim = props::track_anim(&mut ar)?;
    let mut instances = Vec::new();
    for s in &anim.scenes {
        for i in &s.instances {
            if i.object >= objects.len() {
                continue;
            }
            let p = &i.placement;
            instances.push(json!({
                "object": i.object,
                "scene": s.name,
                "free_roam": s.in_free_roam(),
                "matrix": [p.x_axis, p.y_axis, p.z_axis, p.position],
                "distances": i.distances,
            }));
        }
    }
    let free = instances.iter().filter(|i| i["free_roam"].as_bool() == Some(true)).count();
    std::fs::write(dir.join("index.json"), serde_json::to_vec_pretty(&json!({ "objects": objects_json, "instances": instances }))?)?;
    println!("[anim] {} objects, {} textures, {} instances ({free} in free roam)", objects.len(), files.len(), instances.len());
    Ok(())
}
