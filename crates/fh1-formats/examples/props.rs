//! Colorado prop placement survey: parses every `.pgeo` in bin.zip, resolves each model's LOD0
//! reference through the track `.pvs` record table to a `.rmb.bin` template, checks the template
//! exists in bin.zip (and how many have bounds equal to the model's), and reports the totals.
//! Optionally checks `CollObjs.xml` placements against the resolved set.
//!
//! `cargo run --release -p fh1-formats --example props -- <disc/media/tracks/colorado> [out.csv]`
//!
//! The CSV (one row per resolved placement) has the template name, engine-space position and the
//! engine-space basis rows: `template,x,y,z,xx,xy,xz,yx,yy,yz,zx,zy,zz,full`.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::Path;

use fh1_formats::props::{bounds_match, collobj_placements, collobj_templates, parse_obj_xml, parse_pgeo, pvs_model_numbers, CollObjMatch, GeoModel};
use fh1_formats::{rmb, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let track = Path::new(&a[1]);
    let mut ar = Archive::open(track.join("bin.zip")).expect("bin.zip");
    let entries = ar.entries.clone();

    // One copy of each .pgeo (bin.zip duplicates them per streaming block).
    let mut pgeos: BTreeMap<String, usize> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.name.to_ascii_lowercase().ends_with(".pgeo") {
            pgeos.entry(e.name.to_ascii_lowercase()).or_insert(i);
        }
    }

    let numbers = pvs_model_numbers(&std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).expect("pvs")).expect("pvs table");
    let by_name: HashMap<String, usize> = entries.iter().enumerate().rev().map(|(i, e)| (e.name.to_ascii_lowercase(), i)).collect();
    type Bounds = Option<([f32; 3], [f32; 3])>;
    let mut bounds_cache: HashMap<usize, Bounds> = HashMap::new();
    // LOD0 template of a model: PVS record -> model number -> bin.zip entry.
    let mut resolve = |ar: &mut Archive<std::fs::File>, m: &GeoModel, stats: &mut BTreeMap<&'static str, usize>| -> Option<usize> {
        let r = *m.lod0.first()? as usize;
        let Some(&num) = numbers.get(r) else {
            *stats.entry("model: reference outside the pvs table").or_default() += 1;
            return None;
        };
        let Some(&i) = by_name.get(&format!("coloradoout.{num:05}.rmb.bin")) else {
            *stats.entry("model: template missing from bin.zip").or_default() += 1;
            return None;
        };
        *stats.entry("model: resolved").or_default() += 1;
        let b = *bounds_cache.entry(i).or_insert_with(|| ar.read(&entries[i]).ok().and_then(|d| rmb::parse(&d).ok()).map(|t| (t.bounds_min, t.bounds_max)));
        match b {
            Some((mn, mx)) if bounds_match(m, mn, mx, 0.05) => *stats.entry("model: bounds equal").or_default() += 1,
            Some(_) => *stats.entry("model: bounds differ (LOD union)").or_default() += 1,
            None => *stats.entry("model: template does not parse").or_default() += 1,
        }
        Some(i)
    };

    let mut stats: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut templates: BTreeMap<String, usize> = BTreeMap::new();
    let mut resolved = Vec::new();
    let mut csv = a.get(2).map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    for &i in pgeos.values() {
        let d = ar.read(&entries[i]).expect("read pgeo");
        let geo = match parse_pgeo(&d) {
            Ok(g) => g,
            Err(e) => {
                *stats.entry("pgeo: parse error").or_default() += 1;
                eprintln!("{}: {e}", entries[i].name);
                continue;
            }
        };
        *kinds.entry(geo.kind().to_owned()).or_default() += 1;
        if geo.placements.is_empty() {
            continue;
        }
        let tmpl: Vec<Option<usize>> = geo.models.iter().map(|m| resolve(&mut ar, m, &mut stats)).collect();
        for p in &geo.placements {
            *stats.entry(if p.full { "placements: full" } else { "placements: compact" }).or_default() += 1;
            let Some(t) = tmpl[p.model] else {
                *stats.entry("placements: unresolved").or_default() += 1;
                continue;
            };
            *stats.entry("placements: resolved").or_default() += 1;
            let name = entries[t].name.clone();
            *templates.entry(name.clone()).or_default() += 1;
            let m = p.engine_matrix();
            resolved.push((m[12], m[14]));
            if let Some(w) = &mut csv {
                writeln!(w, "{name},{:.3},{:.3},{:.3},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{}", m[12], m[13], m[14], m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10], p.full as u8).unwrap();
            }
        }
    }
    println!("pgeo files (one copy each): {}", pgeos.len());
    println!("kinds: {kinds:?}");
    for (k, v) in &stats {
        println!("{k}: {v}");
    }
    println!("distinct templates placed: {}", templates.len());
    let mut top: Vec<_> = templates.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    for (t, c) in top.iter().take(15) {
        println!("  {c:7}  {t}");
    }

    // What the templates are: first full-detail submodel name, grouped by its first two words.
    let mut by_kind: BTreeMap<String, (usize, usize, String)> = BTreeMap::new();
    for (t, c) in &templates {
        let Some(&i) = by_name.get(&t.to_ascii_lowercase()) else { continue };
        let Ok(m) = ar.read(&entries[i]).map_err(|_| ()).and_then(|d| rmb::parse(&d).map_err(|_| ())) else { continue };
        let Some(s) = m.submodels.iter().find(|s| !s.is_helper()) else { continue };
        let key: String = s.name.split('_').take(2).collect::<Vec<_>>().join("_");
        let e = by_kind.entry(key).or_insert((0, 0, s.name.clone()));
        e.0 += 1;
        e.1 += c;
    }
    let mut kinds_sorted: Vec<_> = by_kind.into_iter().collect();
    kinds_sorted.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    println!("template kinds (templates, placements, sample submodel):");
    for (k, (n_t, n_p, sample)) in kinds_sorted.iter().take(40) {
        println!("  {n_p:7} {n_t:4}  {k:28} {sample}");
    }

    // CollObjs: how many have a resolved placement within 1 m (engine space: z negated)?
    if let Ok(xml) = std::fs::read_to_string(track.join("Ribbon_00/CollObjs.xml")) {
        let objs = parse_obj_xml(&xml);
        let mut grid: HashMap<(i32, i32), Vec<(f32, f32)>> = HashMap::new();
        for &(x, z) in &resolved {
            grid.entry(((x / 4.0).floor() as i32, (z / 4.0).floor() as i32)).or_default().push((x, z));
        }
        let near = objs
            .iter()
            .filter(|o| {
                let (x, z) = (o.position[0], -o.position[2]);
                let (cx, cz) = ((x / 4.0).floor() as i32, (z / 4.0).floor() as i32);
                (-1..=1).any(|dx| (-1..=1).any(|dz| grid.get(&(cx + dx, cz + dz)).is_some_and(|v| v.iter().any(|&(px, pz)| (px - x).hypot(pz - z) < 1.0))))
            })
            .count();
        println!("CollObjs.xml: {} placements, {near} within 1 m of a resolved .pgeo placement", objs.len());

        // Smashables: type -> whole-object template through the .pvs 0x08 records.
        let pvs = std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).expect("pvs");
        let ar = std::cell::RefCell::new(ar);
        let template_name = |m: u16| -> Option<String> {
            let i = *by_name.get(&format!("coloradoout.{m:05}.rmb.bin"))?;
            let t = rmb::parse(&ar.borrow_mut().read(&entries[i]).ok()?).ok()?;
            Some(t.submodels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(";"))
        };
        let map = collobj_templates(&objs, &pvs, &template_name).expect("collobj map");
        let mut by_match: BTreeMap<String, usize> = BTreeMap::new();
        for (t, (m, k)) in &map {
            *by_match.entry(format!("{k:?}")).or_default() += 1;
            if *k != CollObjMatch::Count {
                println!("  {t} -> {m} ({k:?})");
            }
        }
        let placed = collobj_placements(&objs, &map);
        println!("CollObj types mapped: {} of {} {by_match:?}; objects placed: {}", map.len(), objs.iter().map(|o| o.kind.split('.').next().unwrap()).collect::<std::collections::HashSet<_>>().len(), placed.len());
        if let Some(w) = &mut csv {
            for p in &placed {
                let m = p.matrix;
                writeln!(w, "coloradoout.{:05}.rmb.bin,{:.3},{:.3},{:.3},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},1", p.model_number, m[12], m[13], m[14], m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10]).unwrap();
            }
        }
    }
}
