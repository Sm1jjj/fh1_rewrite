//! Dumps one `.pgeo` (a file, e.g. from `fzip extract`): header, models with their `.pvs`-resolved
//! LOD0 template numbers, and placement count per model.
//!
//! `cargo run --release -p fh1-formats --example pgeo_dump -- <file.pgeo> <Colorado_00.pvs>`

use fh1_formats::props::{parse_pgeo, pvs_model_numbers};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let g = parse_pgeo(&std::fs::read(&a[1]).expect("pgeo")).expect("parse");
    let numbers = pvs_model_numbers(&std::fs::read(&a[2]).expect("pvs")).expect("pvs");
    println!("{} activities {:?} bbox {:?}..{:?}, {} models, {} placements", g.name, g.activities, g.bbox_min, g.bbox_max, g.models.len(), g.placements.len());
    for (i, m) in g.models.iter().enumerate() {
        let n = g.placements.iter().filter(|p| p.model == i).count();
        let t = |v: &Vec<u32>| v.iter().map(|&r| numbers.get(r as usize).copied()).collect::<Vec<_>>();
        println!("  model {i}: lod0 {:?} lod1 {:?} lod2 {:?}: {n} placements", t(&m.lod0), t(&m.lod1), t(&m.lod2));
    }
}
