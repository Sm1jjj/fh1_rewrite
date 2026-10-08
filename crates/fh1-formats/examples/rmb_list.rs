//! Lists `<track>out.%05d.rmb.bin` models in a number range with bounds and submodel names:
//! `rmb_list <bin.zip> <first> <last>` -> `number|minx,miny,minz|maxx,maxy,maxz|name;name;...`
use std::collections::HashMap;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).expect("open archive");
    let (first, last): (u32, u32) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    let by_name: HashMap<String, fh1_formats::zip::Entry> = ar.entries.iter().rev().map(|e| (e.name.to_ascii_lowercase(), e.clone())).collect();
    for n in first..=last {
        let Some(e) = by_name.iter().find(|(k, _)| k.ends_with(&format!("out.{n:05}.rmb.bin"))).map(|(_, e)| e.clone()) else { continue };
        let Ok(d) = ar.read(&e) else { continue };
        let Ok(m) = fh1_formats::rmb::parse(&d) else { continue };
        let names: Vec<&str> = m.submodels.iter().map(|s| s.name.as_str()).collect();
        let b = |v: [f32; 3]| format!("{:.2},{:.2},{:.2}", v[0], v[1], v[2]);
        println!("{n}|{}|{}|{}", b(m.bounds_min), b(m.bounds_max), names.join(";"));
    }
}
