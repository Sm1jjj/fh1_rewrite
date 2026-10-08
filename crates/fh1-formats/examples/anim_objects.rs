//! Animated-object survey (`.pgeo` types 4 / 5, docs/PROPS.md): lists the `Anim_*` objects with
//! the templates their Granny meshes name, and the scene instances per object.
//!
//! `cargo run --release -p fh1-formats --example anim_objects -- <disc/media/tracks/colorado>`

use std::collections::BTreeMap;
use std::path::Path;

use fh1_formats::props;
use fh1_formats::zip::Archive;

fn main() {
    let track = std::env::args().nth(1).expect("track dir");
    let t0 = std::time::Instant::now();
    let mut ar = Archive::open(Path::new(&track).join("bin.zip")).expect("bin.zip");
    let anim = props::track_anim(&mut ar).expect("anim");
    println!("{} objects, {} scenes ({:.1} s)", anim.objects.len(), anim.scenes.len(), t0.elapsed().as_secs_f32());
    let index = props::template_name_index(&mut ar, |n| anim.objects.iter().any(|o| o.meshes.iter().any(|m| m == n))).expect("index");
    println!("name index: {} names ({:.1} s)", index.len(), t0.elapsed().as_secs_f32());
    for (t, d) in props::anim_template_distances(&anim, |n| index.get(n).copied()) {
        println!("  template {t}: {d:?}");
    }
    let mut per: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for s in &anim.scenes {
        for i in &s.instances {
            let e = per.entry(i.object).or_default();
            if s.in_free_roam() { e.0 += 1 } else { e.1 += 1 }
        }
    }
    for (k, o) in anim.objects.iter().enumerate() {
        let t = props::anim_object_templates(o, |n| index.get(n).copied());
        println!("{k:2} {:36} free roam {:3} event {:3} template {:?} meshes {:?}", o.name, per.get(&k).map_or(0, |p| p.0), per.get(&k).map_or(0, |p| p.1), t, o.meshes);
    }
    let placed = props::anim_placements(&anim, |n| index.get(n).copied());
    println!("static placements: {}", placed.len());
}
