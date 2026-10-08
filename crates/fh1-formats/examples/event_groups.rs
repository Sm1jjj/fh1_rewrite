//! Event-only `.pgeo` Models groups (activity-conditional, without activity 0): per activity id,
//! groups / placements / templates; optionally which groups place given template numbers.
//!
//! `cargo run --release -p fh1-formats --example event_groups -- <disc/media/tracks/colorado> [template...]`

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use fh1_formats::{props, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let track = Path::new(&a[1]);
    let mut ar = Archive::open(track.join("bin.zip")).expect("bin.zip");
    let pvs = std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).expect("pvs");
    let groups = props::track_event_groups(&mut ar, &pvs).expect("groups");
    let want: BTreeSet<u16> = a[2..].iter().filter_map(|s| s.parse().ok()).collect();
    let mut per: BTreeMap<u8, (usize, usize, BTreeSet<u16>)> = BTreeMap::new();
    let mut all = BTreeSet::new();
    for g in &groups {
        for &id in &g.activities {
            let e = per.entry(id).or_default();
            e.0 += 1;
            e.1 += g.placements.len();
            e.2.extend(g.placements.iter().map(|p| p.model_number));
        }
        all.extend(g.placements.iter().map(|p| p.model_number));
        let hits: BTreeSet<u16> = g.placements.iter().map(|p| p.model_number).filter(|n| want.contains(n)).collect();
        if !hits.is_empty() {
            println!("{} {:?}: {hits:?}", g.name, g.activities);
        }
    }
    for (id, (n, p, t)) in &per {
        println!("activity {id:3}: {n:3} groups, {p:6} placements, {:4} templates", t.len());
    }
    println!("{} groups, {} distinct templates", groups.len(), all.len());
}
