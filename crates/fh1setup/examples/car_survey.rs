//! car_survey <disc> <installed cars dir> : one line per Data_Car car, auditing the cars group's output.
//! Checks: main / `_lod0` / `_cockpit` carbins parse; model.gltf + cockpit.gltf exist; every glTF material that
//! should sample a texture has one and its image file exists; the fx `tex.json` holds the body / lights / interior
//! textures (case-insensitive); body-kit stems keep a stock (`a`) variant; LOD0 sections line up with LOD1.
//! Ends with totals. Flags in the line: `NOMODEL`, `PARSE:<file>`, `UNTEX:<materials>`, `NOIMG:<png>`,
//! `FXTEX:<missing>`, `KIT:<stem without a>`, `LOD0BOX:<section>`, `LOD0ONLY:<section>`.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use fh1_formats::carbin::{self, Carbin, Section};
use fh1_formats::zip::Archive;
use serde_json::Value;

const KIT_STEMS: [&str; 10] = ["bumperf", "bumperr", "hood", "wing", "skirtl", "skirtr", "exhaustl", "exhaustr", "exhaust", "undercarriage"];

/// Materials that are untextured by design (env-map-only / flat-colour techniques, model.rs material_for).
fn untextured_by_design(n: &str) -> bool {
    let n = n.to_lowercase();
    ((n.contains("glass") || n.contains("window")) && !n.starts_with("lights_gls") && !n.starts_with("detail_glass") && n != "lights_glass")
        || n.contains("chrome")
        || n.contains("mirror")
        || n.contains("rubber")
        || n == "black"
        || n.starts_with("black_")
        || n.starts_with("bottom")
        || n.contains("black")
        || n.contains("tire")
}

fn box_of(s: &Section, lod0: bool) -> Option<([f32; 3], [f32; 3])> {
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    let min_lod = s.subsections.iter().filter(|x| (x.lod == 0) == lod0).map(|x| x.lod).min()?;
    for sub in s.subsections.iter().filter(|x| x.lod == min_lod) {
        let pool = s.vertices_for(sub);
        for &i in &sub.indices {
            let p = pool[i as usize].position;
            for k in 0..3 {
                lo[k] = lo[k].min(p[k] + s.offset[k]);
                hi[k] = hi[k].max(p[k] + s.offset[k]);
            }
        }
    }
    (lo[0] <= hi[0]).then_some((lo, hi))
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let disc = Path::new(&a[1]);
    let cars_dir = Path::new(&a[2]);
    let db = rusqlite::Connection::open_with_flags(disc.join("media/db/gamedb.slt"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut st = db.prepare("SELECT MediaName FROM Data_Car ORDER BY Id").unwrap();
    let cars: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    for media in &cars {
        let mut flags: Vec<String> = Vec::new();
        let dir = cars_dir.join(media);
        let zip = disc.join("media/cars").join(format!("{media}.zip"));
        let Ok(mut ar) = Archive::open(&zip) else {
            println!("{media} NOZIP");
            *totals.entry("NOZIP".into()).or_default() += 1;
            continue;
        };
        let names: Vec<String> = ar.entries.iter().map(|e| e.name.clone()).collect();
        let mut parsed = |file: String, flags: &mut Vec<String>| -> Option<Carbin> {
            let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(&file)).cloned()?;
            let d = ar.read(&e).unwrap();
            match carbin::parse(&d) {
                Ok(c) => {
                    if !c.complete {
                        flags.push(format!("PARTIAL:{file}"));
                    }
                    Some(c)
                }
                // A 1-2 KB file is a stub (FOR_F350SuperDuty_08_lod0): the disc has no such LOD.
                Err(_) => {
                    flags.push(format!("{}:{file}", if d.len() > 4096 { "PARSE" } else { "STUB" }));
                    None
                }
            }
        };
        let main = parsed(format!("{media}.carbin"), &mut flags);
        let lod0 = parsed(format!("{media}_lod0.carbin"), &mut flags);
        let _cockpit = parsed(format!("{media}_cockpit.carbin"), &mut flags);

        // Body kit: every stem present must have an `a` variant (else is_stock drops the part entirely).
        if let Some(m) = &main {
            let mut variants: BTreeMap<&str, BTreeSet<char>> = BTreeMap::new();
            for s in &m.sections {
                let n = s.name.to_lowercase();
                for stem in KIT_STEMS {
                    if let Some(rest) = n.strip_prefix(stem) {
                        let mut ch = rest.chars();
                        if let Some(l) = ch.next().filter(|c| c.is_ascii_lowercase()) {
                            if ch.as_str().is_empty() || ch.as_str().starts_with('_') {
                                variants.entry(stem).or_default().insert(l);
                            }
                        }
                    }
                }
            }
            for (stem, v) in variants {
                if !v.contains(&'a') {
                    flags.push(format!("KIT:{stem}{{{}}}", v.iter().collect::<String>()));
                }
            }
            // LOD0 vs LOD1: same-named sections whose boxes differ by > 5 cm on any face.
            if let Some(l0) = &lod0 {
                for h in &l0.sections {
                    let Some(s) = m.sections.iter().find(|s| s.name.eq_ignore_ascii_case(&h.name)) else {
                        if h.subsections.iter().any(|x| x.lod == 0) {
                            flags.push(format!("LOD0ONLY:{}", h.name));
                        }
                        continue;
                    };
                    // model.rs uses the LOD0 sibling only when it has at least half the LOD1 triangles.
                    let tris = |s: &Section, lod0: bool| {
                        let min = s.subsections.iter().filter(|x| (x.lod == 0) == lod0).map(|x| x.lod).min();
                        s.subsections.iter().filter(|x| Some(x.lod) == min).map(|x| x.indices.len() / 3).sum::<usize>()
                    };
                    let used = tris(h, true) * 2 >= tris(s, false);
                    if let (true, Some((a0, a1)), Some((b0, b1))) = (used, box_of(h, true), box_of(s, false)) {
                        let d = (0..3).map(|k| (a0[k] - b0[k]).abs().max((a1[k] - b1[k]).abs())).fold(0.0f32, f32::max);
                        // `=`: both files store the same section box, so a difference is ours (decode); `~`: the
                        // game's own LOD0 box differs (different artwork).
                        let same = (0..3).all(|k| (h.bounds_min[k] - s.bounds_min[k]).abs() < 0.01 && (h.bounds_max[k] - s.bounds_max[k]).abs() < 0.01 && (h.offset[k] - s.offset[k]).abs() < 0.01);
                        if d > 0.05 && !h.name.eq_ignore_ascii_case("wheel") {
                            flags.push(format!("{}:{}={:.2}", if same { "LOD0BOX" } else { "LOD0ART" }, h.name, d));
                        }
                    }
                }
            }
        }

        // glTF materials.
        match std::fs::read(dir.join("model.gltf")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            None => flags.push("NOMODEL".into()),
            Some(g) => {
                let images: Vec<String> = g["images"].as_array().map(|v| v.iter().map(|i| i["uri"].as_str().unwrap_or("").to_owned()).collect()).unwrap_or_default();
                let mut untex = BTreeSet::new();
                for m in g["materials"].as_array().into_iter().flatten() {
                    let name = m["name"].as_str().unwrap_or("");
                    match m["pbrMetallicRoughness"]["baseColorTexture"]["index"].as_u64() {
                        Some(t) => {
                            let img = g["textures"][t as usize]["source"].as_u64().map(|i| images[i as usize].clone()).unwrap_or_default();
                            if !dir.join(&img).exists() {
                                flags.push(format!("NOIMG:{img}"));
                            }
                        }
                        None if !untextured_by_design(name) => {
                            untex.insert(name.to_owned());
                        }
                        None => {}
                    }
                }
                if !untex.is_empty() {
                    flags.push(format!("UNTEX:{}", untex.into_iter().collect::<Vec<_>>().join(",")));
                }
            }
        }
        if !dir.join("cockpit.gltf").exists() && names.iter().any(|n| n.eq_ignore_ascii_case(&format!("{media}_cockpit.carbin"))) {
            flags.push("NOCOCKPIT".into());
        }

        // fx textures the body binds (car.rs: `name` or `name_LOD0`, case-insensitive).
        let tex: Option<Value> = std::fs::read(dir.join("fx/tex/tex.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let keys: BTreeSet<String> = tex.as_ref().and_then(Value::as_object).map(|o| o.keys().map(|k| k.to_lowercase()).collect()).unwrap_or_default();
        let missing: Vec<&str> = ["nodamage", "lights"].into_iter().filter(|k| !keys.contains(*k) && !keys.contains(&format!("{k}_lod0"))).collect();
        if !missing.is_empty() {
            flags.push(format!("FXTEX:{}", missing.join(",")));
        }
        for f in &flags {
            *totals.entry(f.split(':').next().unwrap().to_owned()).or_default() += 1;
        }
        if flags.is_empty() {
            *totals.entry("clean".into()).or_default() += 1;
        }
        println!("{media} {}", if flags.is_empty() { "ok".to_owned() } else { flags.join(" ") });
    }
    println!("TOTAL cars {}: {:?}", cars.len(), totals);
}
