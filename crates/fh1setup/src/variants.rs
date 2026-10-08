//! `variants` group: the car parts the Customize menu can swap in the remaster renderer, which draws the cars group's
//! baked glTF (docs/CUSTOMIZE.md "Remaster export"). The faithful renderer reads the game's .fxcar parts instead.
//!
//! - Rims: every `media/wheels/<rim>.zip` -> `wheels/<rim>/model.gltf` (+ model.bin, wheel.png) and `wheels/<rim>/rim.json`.
//!   The glTF holds the rim only (no tyre: the car keeps its own), node `rim`, in the rim's model space (left-side wheel,
//!   axis = X, centred on the hub) with the LOD0 pool correction applied (model.rs WheelGeo). `rim.json` has the game's
//!   model measures (`model_rim_d`, `model_tyre_d`, `model_width`, metres) so a car scales it like the game's c59
//!   wheelScale: X by axle tyre_width / model_width, Y and Z by 2 rim_radius / model_rim_d (model.json `axles`).
//! - Body kits: `cars/<CAR>/model.gltf` (+ model.bin) with the car's NON-stock kit sections only (stems bumperf,
//!   bumperr, hood, skirtl, skirtr, wing + a letter other than the stock one), one node per section named as in the
//!   carbin (`bumperFb`, `wingc`, ...; match case-insensitively on stem + letter), under a root node `<CAR>_kit` with
//!   the same `mesh_offset` translation as the cars group's model.gltf root. Materials are the cars group's (same
//!   names), textures point at the cars group's atlases (`../../../cars/<CAR>/exterior.png`). Cars without
//!   non-stock kit sections get no folder.
//! - Race parts (variants-2, 2026-10-08): the `<stem>race` sections (bumperfrace on 153 cars, wingrace 144, bumperrrace
//!   36; carbin_names survey) and the roll cage `cagerace` (140) are exported too, named as in the carbin. The gamedb
//!   race rows (Level 3: one per car and slot; FrontBumper on 162 bodies, RearWing on 155 cars) are those sections
//!   (INFERRED from the counts; the Level 1-2 rows are the letters a + Sequence). `kit.json` lists the exported section
//!   names (lower case) so the engine swaps in only what the car has.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;

use fh1_formats::carbin::{self, Section, Subsection};
use fh1_formats::zip::Archive;

use crate::model::{self, Gltf, Kit, WheelGeo};

/// Kit stems (fh1setup model.rs is_stock), plus the roll cage (race weight reduction; only its `race` section).
const KIT_STEMS: [&str; 7] = ["bumperf", "bumperr", "hood", "skirtl", "skirtr", "wing", "cage"];

/// The letter standing for a `<stem>race` section (fh1-render StockKit / fh1-remaster kit_node use the same).
pub const RACE: char = '#';

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let rims = rims(disc, &out.join("wheels"))?;
    let kits = kits(disc, &out.join("cars"))?;
    println!("[variants] {rims} rims, {kits} cars with kit parts");
    Ok(())
}

fn rims(disc: &Path, out: &Path) -> Result<usize> {
    let mut n = 0;
    let dir = disc.join("media/wheels");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(0) };
    for e in entries.flatten() {
        let path = e.path();
        if !path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
            continue;
        }
        let Some(media) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else { continue };
        match rim(&path, &media, &out.join(&media)) {
            Ok(true) => n += 1,
            Ok(false) => println!("[variants] rim {media}: no usable wheel section"),
            Err(err) => println!("[variants] rim {media}: {err:#}"),
        }
    }
    Ok(n)
}

fn rim(zip: &Path, media: &str, out: &Path) -> Result<bool> {
    let Some((sec, tex)) = model::read_rim(zip, media)? else { return Ok(false) };
    let Some(geo) = WheelGeo::new(&sec, Some(&sec)) else { return Ok(false) };
    std::fs::create_dir_all(out)?;
    let mut atlases = HashMap::new();
    if let Some((w, h, rgba)) = &tex {
        // As model.rs: RGB is the whole rim, alpha only marks the decals (TintColor white drops it out).
        let baked: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        model::write_png(&out.join("wheel.png"), *w, *h, &baked)?;
        atlases.insert("wheel", "wheel.png".to_owned());
    }
    let paint = model::Paint { rgb: 0x808080, metallic: false, sequence: 0 };
    let mut g = Gltf::default();
    let fit = |p: [f32; 3]| geo.rim_fit.apply(p);
    let Some(mesh) = g.mesh_xf("rim", geo.rim_sec, &geo.rim_subs, &atlases, &paint, &fit) else { return Ok(false) };
    let root = g.node(json!({"name": "rim", "mesh": mesh}));
    g.write(out, root)?;
    let info = json!({
        "media_name": media,
        "model_rim_d": geo.model_rim_d,
        "model_tyre_d": geo.model_tyre_d,
        "model_width": geo.model_width,
    });
    std::fs::write(out.join("rim.json"), serde_json::to_vec_pretty(&info)?)?;
    Ok(true)
}

fn kits(disc: &Path, out: &Path) -> Result<usize> {
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut n = 0;
    for car in crate::cars::rows(&db, "SELECT Id, MediaName FROM Data_Car ORDER BY Id", [])? {
        let (Some(id), Some(media)) = (car["Id"].as_i64(), car["MediaName"].as_str()) else { continue };
        let zip = disc.join("media/cars").join(format!("{media}.zip"));
        if !zip.exists() {
            continue;
        }
        let body_id = crate::cars::rows(&db, "SELECT CarBodyID FROM List_UpgradeCarBody WHERE Ordinal=?1 AND IsStock=1", [id])?.first().and_then(|r| r["CarBodyID"].as_i64());
        let stock = crate::cars::stock_kit(&db, id, body_id)?;
        let centre = crate::cars::rows(&db, "SELECT * FROM Data_CarBody WHERE Id=?1", [body_id.unwrap_or(-1)])?.into_iter().next();
        match kit(&zip, media, &stock, centre.as_ref(), &out.join(media)) {
            Ok(true) => n += 1,
            Ok(false) => {}
            Err(err) => println!("[variants] kit {media}: {err:#}"),
        }
    }
    Ok(n)
}

/// `bumperFb` -> Some(("bumperf", 'b')) when the name is a kit stem + one letter (+ `_tail`); `wingrace` ->
/// Some(("wing", RACE)).
fn kit_part(name: &str) -> Option<(&'static str, char)> {
    let n = name.to_lowercase();
    for stem in KIT_STEMS {
        if let Some(rest) = n.strip_prefix(stem) {
            if let Some(tail) = rest.strip_prefix("race") {
                if tail.is_empty() || tail.starts_with('_') {
                    return Some((stem, RACE));
                }
            }
            if stem == "cage" {
                continue;
            }
            let mut ch = rest.chars();
            let letter = ch.next().filter(|c| c.is_ascii_lowercase())?;
            let tail = ch.as_str();
            if tail.is_empty() || tail.starts_with('_') {
                return Some((stem, letter));
            }
        }
    }
    None
}

fn kit(zip: &Path, media: &str, stock: &Kit, body: Option<&serde_json::Map<String, serde_json::Value>>, out: &Path) -> Result<bool> {
    let mut ar = Archive::open(zip)?;
    let mut read = |name: &str| -> Result<Option<Vec<u8>>> {
        match ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned() {
            Some(e) => Ok(Some(ar.read(&e)?)),
            None => Ok(None),
        }
    };
    let main = carbin::parse(&read(&format!("{media}.carbin"))?.context("main carbin missing")?).context("main carbin")?;
    let lod0 = read(&format!("{media}_lod0.carbin"))?.and_then(|d| carbin::parse(&d).ok());
    // Non-stock kit sections: the main carbin's, plus the ones only `_lod0.carbin` has.
    let lod0_secs: &[Section] = lod0.as_ref().map(|c| c.sections.as_slice()).unwrap_or_default();
    let mut parts: Vec<(&Section, Vec<&Subsection>)> = Vec::new();
    for s in main.sections.iter().chain(lod0_secs.iter().filter(|h| !main.sections.iter().any(|s| s.name.eq_ignore_ascii_case(&h.name)))) {
        let Some((stem, letter)) = kit_part(&s.name) else { continue };
        if letter == stock.letter(stem) || model::is_placeholder(s) {
            continue;
        }
        // The LOD0 sibling when it has real LOD0 geometry (model.rs: at least half the LOD1 triangles).
        let tris = |v: &[&Subsection]| v.iter().map(|x| x.indices.len() / 3).sum::<usize>();
        let hi = lod0_secs.iter().find(|h| h.name.eq_ignore_ascii_case(&s.name)).filter(|h| !model::is_placeholder(h) && h.subsections.iter().any(|x| x.lod == 0));
        let (sec, subs) = match hi {
            Some(h) if tris(&model::best_lod(h)) * 2 >= tris(&model::best_lod(s)) => (h, model::best_lod(h)),
            _ => (s, model::best_lod(s)),
        };
        if tris(&subs) > 0 {
            parts.push((sec, subs));
        }
    }
    if parts.is_empty() {
        return Ok(false);
    }
    std::fs::create_dir_all(out)?;
    // The cars group's atlases and shared textures, next to its model.gltf (cars/<CAR>/ vs variants/cars/<CAR>/).
    let up = format!("../../../cars/{media}/");
    let atlases: HashMap<&str, String> = ["exterior", "interior", "lights", "tire", "grille1", "grille2", "bumper_frame", "undercarriage", "carbon"]
        .into_iter()
        .map(|k| (k, format!("{up}{k}.png")))
        .collect();
    let paint = model::Paint { rgb: 0x808080, metallic: false, sequence: 0 };
    let mut g = Gltf::default();
    let mut children = Vec::new();
    for (sec, subs) in &parts {
        if let Some(mesh) = g.mesh(&sec.name, sec, subs, &atlases, &paint) {
            children.push(g.node(json!({"name": sec.name, "mesh": mesh})));
        }
    }
    // Same frame as the cars group's model.gltf root (model.rs: translation = -BottomCenterWheelbasePos).
    let centre = ["BottomCenterWheelbasePosx", "BottomCenterWheelbasePosy", "BottomCenterWheelbasePosZ"].map(|k| body.and_then(|b| b.get(k)).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32);
    let root = g.node(json!({"name": format!("{media}_kit"), "children": children, "translation": centre.map(|v| -v)}));
    g.write(out, root)?;
    let sections: Vec<String> = parts.iter().map(|(s, _)| s.name.to_lowercase()).collect();
    std::fs::write(out.join("kit.json"), serde_json::to_vec_pretty(&json!({ "sections": sections }))?)?;
    Ok(true)
}
