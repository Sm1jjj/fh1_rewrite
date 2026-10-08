//! Placed templates that have no LOD0 `Class::Normal` submodel (what the scenery setup draws), with
//! their submodel names / LODs / classes and placement counts:
//! `props_empty <disc/media/tracks/colorado>`
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use fh1_formats::props::{collobj_placements, collobj_templates, parse_obj_xml, track_placements};
use fh1_formats::{rmb, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let track = Path::new(&a[1]);
    let mut ar = Archive::open(track.join("bin.zip")).expect("bin.zip");
    let pvs = std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).expect("pvs");
    let (mut placed, _) = track_placements(&mut ar, &pvs).expect("pgeo");
    let by_name: HashMap<String, fh1_formats::zip::Entry> = ar.entries.iter().rev().map(|e| (e.name.to_ascii_lowercase(), e.clone())).collect();
    let ar = RefCell::new(ar);
    let load = |n: u16| -> Option<rmb::TrackModel> {
        let e = by_name.get(&format!("coloradoout.{n:05}.rmb.bin"))?;
        rmb::parse(&ar.borrow_mut().read(e).ok()?).ok()
    };
    let xml = std::fs::read_to_string(track.join("Ribbon_00/CollObjs.xml")).expect("CollObjs.xml");
    let objs = parse_obj_xml(&xml);
    let map = collobj_templates(&objs, &pvs, |n| load(n).map(|t| t.submodels.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(";"))).expect("collobj");
    placed.extend(collobj_placements(&objs, &map));

    let mut uses: BTreeMap<u16, (usize, Option<u16>)> = BTreeMap::new();
    for p in &placed {
        let e = uses.entry(p.model_number).or_insert((0, p.lod1));
        e.0 += 1;
    }
    let mut empty: Vec<(usize, u16, Option<u16>, String, bool)> = Vec::new();
    for (&n, &(count, lod1)) in &uses {
        let Some(t) = load(n) else {
            empty.push((count, n, lod1, "(missing or unparsable)".into(), false));
            continue;
        };
        if t.submodels.iter().any(|s| s.class() == rmb::Class::Normal && s.lod() == 0) {
            continue;
        }
        let desc: Vec<String> = t.submodels.iter().map(|s| format!("{} [lod{} {:?}]", s.name, s.lod(), s.class())).collect();
        let lod1_ok = lod1.and_then(load).is_some_and(|t| t.submodels.iter().any(|s| s.class() == rmb::Class::Normal));
        empty.push((count, n, lod1, desc.join("; "), lod1_ok));
    }
    empty.sort_by_key(|a| std::cmp::Reverse(a.0));
    let total: usize = empty.iter().map(|e| e.0).sum();
    println!("{} placed templates, {} without a LOD0 Normal submodel, covering {total} placements", uses.len(), empty.len());
    let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
    for (count, n, lod1, desc, lod1_ok) in &empty {
        let reason = if desc.contains("[lod0") {
            "lod0 but not Normal"
        } else if desc.contains("Normal]") {
            "Normal but only lod1+ names"
        } else {
            "no Normal submodel"
        };
        *by_reason.entry(reason).or_default() += count;
        println!("{count:6}  {n:5}  lod1 {lod1:?} (has Normal: {lod1_ok})  {}", desc.chars().take(150).collect::<String>());
    }
    println!("by reason: {by_reason:?}");
}
