//! Map points of interest (minimap / world-map icons), from the user's disc.
//!
//! The game ties a map icon to a place in two steps (VERIFIED against the files):
//! - `media/gamemodes.zip` `Colorado/*.xml` activities hold `<TriggerZone object="<id>" ...
//!   mapTag="<activity_type>">`.
//! - `media/tracks/colorado/Ribbon_00/GameObjs.xml` gives `<ObjN GameplayID="<id>"><Pos x y z>`.
//! - The mapTag is the `activity_type` that `MapProfileMinimap.xml` / `MapProfileFullscreen.xml`
//!   filter on (racecentral, workshop, autoshow, paintshop, carclub, dlccenter, streetrace, race, ...).
//!
//! Some files carry no mapTag; their tag is taken from the profile group the file obviously
//! matches (GUESS): `gas_stations.xml` → `gas_station`, `barnfinds.xml` zones without one →
//! `barnfind`. Career events (`career_event_activations.xml`) get their tag from the object-name
//! prefix (GUESS): EXHIBITION_ → `exhibition`, NR/NEM_ → `nemesisrace`, MAIN_SRT_ → `streetrace`,
//! others → `race`. Speed cameras are GameObjs pairs `speed_camera_NN_left/right` (midpoint,
//! `speed_camera`).
//!
//! Install output `map/pois.tsv`, one per line: `tag \t object \t source \t x \t y \t z`, in
//! collision space (left-handed, +Z north: negate Z for the engine).

use std::collections::HashMap;
use std::path::Path;

use fh1_formats::zip::Archive;

use crate::Result;

#[derive(Debug, Clone, PartialEq)]
pub struct Poi {
    /// The profile's `activity_type` (e.g. `racecentral`).
    pub tag: String,
    /// The GameObjs `GameplayID`.
    pub object: String,
    /// The gamemodes file it came from (`gas_stations`, `race_central`, ... or `GameObjs`).
    pub source: String,
    /// Collision space.
    pub pos: [f32; 3],
}

/// Attribute `k="v"` of an XML start tag.
fn attr<'a>(tag: &'a str, k: &str) -> Option<&'a str> {
    let i = tag.find(&format!(" {k}=\""))? + k.len() + 3;
    Some(&tag[i..i + tag[i..].find('"')?])
}

/// `GameplayID` → position from GameObjs.xml.
pub fn game_objects(xml: &str) -> HashMap<String, [f32; 3]> {
    let mut out = HashMap::new();
    let mut rest = xml;
    while let Some(i) = rest.find("GameplayID=\"") {
        rest = &rest[i + 12..];
        let Some(e) = rest.find('"') else { break };
        let id = rest[..e].to_string();
        let Some(p) = rest.find("<Pos ") else { break };
        let Some(pe) = rest[p..].find("/>") else { break };
        let tag = &rest[p..p + pe];
        let f = |k| attr(tag, k).and_then(|v| v.parse::<f32>().ok());
        if let (Some(x), Some(y), Some(z)) = (f("x"), f("y"), f("z")) {
            out.insert(id, [x, y, z]);
        }
    }
    out
}

/// POIs from the activity XMLs (`(file stem, contents)`) and the GameObjs positions.
pub fn collect(activities: &[(String, String)], objs: &HashMap<String, [f32; 3]>) -> Vec<Poi> {
    let mut out = Vec::new();
    for (stem, xml) in activities {
        let mut rest = xml.as_str();
        while let Some(i) = rest.find("<TriggerZone ") {
            rest = &rest[i..];
            let end = rest.find('>').unwrap_or(rest.len());
            let tag = &rest[..end];
            rest = &rest[end..];
            let Some(object) = attr(tag, "object") else { continue };
            let Some(&pos) = objs.get(object) else { continue };
            let t = match (attr(tag, "mapTag"), stem.as_str()) {
                (Some(t), _) => t.to_string(),
                (None, "gas_stations") => "gas_station".into(),
                (None, "barnfinds") => "barnfind".into(),
                (None, "career_event_activations") => {
                    if object.starts_with("EXHIBITION_") {
                        "exhibition".into()
                    } else if object.starts_with("NR") || object.starts_with("NEM_") {
                        "nemesisrace".into()
                    } else if object.starts_with("MAIN_SRT_") {
                        "streetrace".into()
                    } else {
                        "race".into()
                    }
                }
                _ => continue,
            };
            if !out.iter().any(|p: &Poi| p.object == object && p.tag == t) {
                out.push(Poi { tag: t, object: object.to_string(), source: stem.clone(), pos });
            }
        }
    }
    // Speed cameras: left/right gate posts, icon at the midpoint.
    let mut cams: Vec<&String> = objs.keys().filter(|k| k.starts_with("speed_camera_") && k.ends_with("_left")).collect();
    cams.sort();
    for l in cams {
        let base = &l[..l.len() - 5];
        if let (Some(a), Some(b)) = (objs.get(l), objs.get(&format!("{base}_right"))) {
            let pos = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0, (a[2] + b[2]) / 2.0];
            out.push(Poi { tag: "speed_camera".into(), object: base.to_string(), source: "GameObjs".into(), pos });
        }
    }
    out
}

/// Install step: write `map/pois.tsv` from the disc. Returns the POI count.
pub fn build(media: &Path, out: &Path) -> Result<usize> {
    let objs = game_objects(&String::from_utf8_lossy(&std::fs::read(media.join("tracks/colorado/Ribbon_00/GameObjs.xml"))?));
    let mut ar = Archive::open(media.join("gamemodes.zip"))?;
    let mut acts = Vec::new();
    for e in ar.entries.clone() {
        let name = e.name.replace('\\', "/");
        let Some(file) = name.strip_prefix("Colorado/").filter(|f| !f.contains('/') && f.ends_with(".xml")) else { continue };
        acts.push((file.trim_end_matches(".xml").to_string(), String::from_utf8_lossy(&ar.read(&e)?).into_owned()));
    }
    let pois = collect(&acts, &objs);
    let text: String = pois.iter().map(|p| format!("{}\t{}\t{}\t{}\t{}\t{}\n", p.tag, p.object, p.source, p.pos[0], p.pos[1], p.pos[2])).collect();
    std::fs::create_dir_all(out.join("map"))?;
    std::fs::write(out.join("map/pois.tsv"), text)?;
    Ok(pois.len())
}

/// Read `map/pois.tsv`.
pub fn load(tsv: &str) -> Vec<Poi> {
    tsv.lines()
        .filter_map(|l| {
            let c: Vec<&str> = l.split('\t').collect();
            let f = |i: usize| c.get(i)?.parse::<f32>().ok();
            Some(Poi { tag: c.first()?.to_string(), object: c.get(1)?.to_string(), source: c.get(2)?.to_string(), pos: [f(3)?, f(4)?, f(5)?] })
        })
        .collect()
}
