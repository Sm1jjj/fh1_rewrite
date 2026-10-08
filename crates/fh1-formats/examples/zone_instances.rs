//! Zone-placed templates (`.pvsz` placement sections, docs/PROPS.md): counts per template.
//!
//! `cargo run --release -p fh1-formats --example zone_instances -- <disc/media/tracks/colorado> [rmb_list dump]`

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use fh1_formats::{props, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let track = Path::new(&a[1]);
    let t0 = std::time::Instant::now();
    let mut ar = Archive::open(track.join("bin.zip")).expect("bin.zip");
    let pvs = std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).expect("pvs");
    let names: HashMap<u16, String> = a
        .get(2)
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|t| t.lines().filter_map(|l| { let p: Vec<&str> = l.split('|').collect(); Some((p.first()?.parse().ok()?, p.get(3)?.chars().take(40).collect())) }).collect())
        .unwrap_or_default();
    let (placed, dist) = props::track_zone_instances(&mut ar, &pvs, |_| false).expect("zones");
    let mut per: BTreeMap<u16, usize> = BTreeMap::new();
    for p in &placed {
        *per.entry(p.model_number).or_default() += 1;
    }
    println!("{} placements of {} templates ({:.1} s)", placed.len(), per.len(), t0.elapsed().as_secs_f32());
    let mut v: Vec<_> = per.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (m, n) in v.iter().take(std::env::var("TOP").ok().and_then(|s| s.parse().ok()).unwrap_or(60)) {
        println!("{n:6} {m:5} {:40} {:?}", names.get(m).map_or("", |s| s), dist.get(m).map(|d| (d.lod1_model, d.lod1_m, d.lod2_model, d.lod2_m, d.cull_m)));
    }
}
