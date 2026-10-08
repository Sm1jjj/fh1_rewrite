//! Named `.pvsz` zone instances (blocks with a name: barn-find doors, event nodes, GameObj ids), deduplicated
//! across zones: name, record -> model number, position, distances, activities, object type.
//!
//! `cargo run --release -p fh1-formats --example named_instances -- <disc/media/tracks/colorado> [name filter]`

use std::collections::BTreeMap;
use std::path::Path;

use fh1_formats::{props, pvsz, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let track = Path::new(&a[1]);
    let filter = a.get(2).map(|s| s.to_ascii_lowercase());
    let mut ar = Archive::open(track.join("bin.zip")).expect("bin.zip");
    let pvs = std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).expect("pvs");
    let records = props::pvs_records(&pvs).expect("pvs records");
    let mut seen = std::collections::HashSet::new();
    let zones: Vec<_> = ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pvsz") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    let mut out: BTreeMap<(String, u16, [i32; 3]), (pvsz::Instance, Vec<String>)> = BTreeMap::new();
    for z in zones {
        let zone = pvsz::parse(&ar.read(&z).expect("read")).expect("pvsz");
        for i in zone.instances {
            let rm = u16::from_be_bytes([records[i.record as usize][0], records[i.record as usize][1]]);
            if std::env::var("MODEL").ok().and_then(|m| m.parse::<u16>().ok()) == Some(rm) {
                println!("model {rm} rec {} pos {:?} axes {:?} d {:?} block {:?}", i.record, i.position, i.axes, i.distances, i.block);
            }
            let Some(b) = &i.block else { continue };
            if b.name.is_empty() || filter.as_ref().is_some_and(|f| !b.name.to_ascii_lowercase().contains(f.as_str())) {
                continue;
            }
            let r = &records[i.record as usize];
            let model = u16::from_be_bytes([r[0], r[1]]);
            let key = (b.name.clone(), model, i.position.map(|v| (v * 10.0).round() as i32));
            out.entry(key).or_insert_with(|| (i.clone(), Vec::new())).1.push(z.name.clone());
        }
    }
    for ((name, model, _), (i, zs)) in &out {
        let b = i.block.as_ref().unwrap();
        let r = &records[i.record as usize];
        println!(
            "{name:34} rec {:5} model {model:5} flags {:02x} pos ({:8.1} {:6.1} {:8.1}) d {:?} type {} act {:?} zones {}",
            i.record,
            r[4],
            i.position[0],
            i.position[1],
            i.position[2],
            i.distances,
            b.object_type,
            b.activities,
            zs.len()
        );
    }
    if std::env::var_os("AXES").is_some() {
        for ((name, _, _), (i, _)) in &out {
            println!("{name}: axes {:?} radius {}", i.axes, i.radius);
        }
    }
    println!("{} named instances", out.len());
}
