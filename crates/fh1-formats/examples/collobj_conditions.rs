//! Matches the `CollObjs.xml` / `GameObjs.xml` objects with their `.pvsz` zone instances (same template, same
//! position) and counts which are free roam, event-only or unmatched, per type.
//!
//! `cargo run --release -p fh1-formats --example collobj_conditions -- <disc/media/tracks/colorado> [type filter]`

use std::collections::BTreeMap;
use std::path::Path;

use fh1_formats::{props, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let track = Path::new(&a[1]);
    let filter = a.get(2).cloned().unwrap_or_default();
    let mut ar = Archive::open(track.join("bin.zip")).expect("bin.zip");
    let pvs = std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).expect("pvs");
    let conds = props::track_object_conditions(&mut ar, &pvs).expect("zones");
    println!("{} zone object instances", conds.len());
    let r = track.join("Ribbon_00");
    let coll = props::parse_obj_xml(&std::fs::read_to_string(r.join("CollObjs.xml")).unwrap());
    let game = props::parse_obj_xml(&std::fs::read_to_string(r.join("GameObjs.xml")).unwrap());
    let by_name: std::collections::HashMap<String, fh1_formats::zip::Entry> =
        ar.entries.iter().map(|e| (e.name.to_ascii_lowercase().replace(char::from(92), "/"), e.clone())).collect();
    let cell = std::cell::RefCell::new(&mut ar);
    let map = props::collobj_templates(&coll, &pvs, |n| {
        let e = by_name.iter().find(|(k, _)| k.ends_with(&format!("out.{n:05}.rmb.bin")))?.1.clone();
        let m = fh1_formats::rmb::parse(&cell.borrow_mut().read(&e).ok()?).ok()?;
        Some(m.submodels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(";"))
    })
    .expect("collobj templates");
    // (type -> [free roam, event-only, unmatched])
    let mut tally: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut objs: Vec<(String, u16, [f32; 3])> = Vec::new();
    for o in &coll {
        let t = o.kind.split('.').next().unwrap_or(&o.kind).to_owned();
        let Some(&(n, _)) = map.get(&t) else { continue };
        if seen.insert((t.clone(), o.position.map(|v| (v * 100.0).round() as i64))) {
            objs.push((t, n, o.position));
        }
    }
    for o in &game {
        if let Some(&(p, n, _)) = props::GAMEOBJ_TEMPLATES.iter().find(|(p, _, _)| o.kind.starts_with(p)) {
            objs.push((format!("GameObjs {p}"), n, o.position));
        }
    }
    for (t, n, pos) in &objs {
        let e = tally.entry(t.clone()).or_default();
        match props::object_condition(&conds, *n, *pos) {
            Some(c) if c.free_roam => {
                e[0] += 1;
                if !filter.is_empty() && t.contains(filter.as_str()) {
                    println!("free {t} {pos:?} activities {:?} name {:?}", c.activities, c.name);
                }
            }
            Some(c) => {
                e[1] += 1;
                if !filter.is_empty() && t.contains(filter.as_str()) {
                    println!("event-only {t} {pos:?} activities {:?} name {:?}", c.activities, c.name);
                }
            }
            None => {
                e[2] += 1;
                if !filter.is_empty() && t.contains(filter.as_str()) {
                    // Zone instances of any template at this position.
                    let c = pos.map(|v| (v * 10.0).round() as i32);
                    let near: Vec<_> = conds.iter().filter(|((_, k), _)| (k[0] - c[0]).abs() <= 300 && (k[2] - c[2]).abs() <= 300).map(|((m, _), c)| (*m, c.activities.len())).collect();
                    println!("unmatched {t} (template {n}) {pos:?} zone instances here: {near:?}");
                }
            }
        }
    }
    let mut total = [0; 3];
    for (t, c) in &tally {
        println!("{t:48} free {:5} event {:5} unmatched {:5}", c[0], c[1], c[2]);
        (0..3).for_each(|i| total[i] += c[i]);
    }
    println!("TOTAL free {} event {} unmatched {}", total[0], total[1], total[2]);
}
