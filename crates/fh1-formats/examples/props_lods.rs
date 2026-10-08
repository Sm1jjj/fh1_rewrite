//! How a `.pgeo` model's two references relate (is the second one really its LOD1?):
//! `props_lods <disc/media/tracks/colorado>`. Compares the templates' first submodel names with the
//! LOD suffix stripped, and prints the draw entries' per-draw floats for a few models.
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use fh1_formats::props::{parse_pgeo, pvs_model_numbers};
use fh1_formats::{rmb, zip::Archive};

fn base(name: &str) -> String {
    let u = name.to_ascii_uppercase();
    let cut = u.rfind("_LOD").unwrap_or(u.len());
    u[..cut].trim_end_matches('_').to_owned()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let track = Path::new(&a[1]);
    let ar = Archive::open(track.join("bin.zip")).expect("bin.zip");
    let numbers = pvs_model_numbers(&std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")).unwrap()).unwrap();
    let entries = ar.entries.clone();
    let by_name: HashMap<String, usize> = entries.iter().enumerate().rev().map(|(i, e)| (e.name.to_ascii_lowercase(), i)).collect();
    let ar = RefCell::new(ar);
    let cache: RefCell<HashMap<u16, Option<String>>> = RefCell::default();
    let first_name = |n: u16| -> Option<String> {
        cache
            .borrow_mut()
            .entry(n)
            .or_insert_with(|| {
                let i = *by_name.get(&format!("coloradoout.{n:05}.rmb.bin"))?;
                let t = rmb::parse(&ar.borrow_mut().read(&entries[i]).ok()?).ok()?;
                t.submodels.iter().find(|s| s.class() == rmb::Class::Normal).map(|s| s.name.clone())
            })
            .clone()
    };
    let mut seen = HashSet::new();
    let pgeos: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| e.name.to_ascii_lowercase().ends_with(".pgeo") && seen.insert(e.name.to_ascii_lowercase())).map(|(i, _)| i).collect();
    let mut st: BTreeMap<String, usize> = BTreeMap::new();
    let mut shown = 0;
    for i in pgeos {
        let d = ar.borrow_mut().read(&entries[i]).unwrap();
        let Ok(g) = parse_pgeo(&d) else { continue };
        for m in &g.models {
            let (Some(&r0), Some(&r1)) = (m.lod0.first(), m.lod1.first()) else { continue };
            let (n0, n1) = (numbers[r0 as usize], numbers[r1 as usize]);
            let (Some(a0), Some(a1)) = (first_name(n0), first_name(n1)) else { continue };
            let same = base(&a0) == base(&a1);
            *st.entry(format!("second ref same object: {same}")).or_default() += 1;
            if !same && shown < 12 {
                shown += 1;
                println!("{}: {n0} {a0}  |  {n1} {a1}   (raw refs {r0} {r1}, pvs rec link?)", entries[i].name);
            }
        }
    }
    println!("{st:?}");
}
