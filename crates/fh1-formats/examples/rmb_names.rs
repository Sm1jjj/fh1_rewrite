//! rmb_names <bin.zip> : triangle counts of full-detail submodels by name prefix (text before the
//! first '_' or digit), plus sample names.
use std::collections::{BTreeMap, HashSet};
fn main() {
    let mut ar = fh1_formats::zip::Archive::open(std::env::args().nth(1).unwrap()).unwrap();
    let mut seen = HashSet::new();
    let mut by: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".rmb.bin") || !seen.insert(n) { continue; }
        let m = fh1_formats::rmb::parse(&ar.read(&e).unwrap()).unwrap();
        for s in m.submodels.iter().filter(|s| s.lod() == 0 && !s.is_helper()) {
            let key: String = s.name.chars().take_while(|c| *c != '_' && !c.is_ascii_digit()).collect::<String>().to_uppercase();
            let t: usize = s.meshes.iter().map(|x| x.indices.len() / 3).sum();
            let ent = by.entry(key).or_default();
            ent.0 += t;
            if ent.1.len() < 3 { ent.1.push(s.name.clone()); }
        }
    }
    let mut v: Vec<_> = by.into_iter().collect();
    v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for (k, (t, ex)) in v.iter().take(30) { println!("{k:16} {t:9}  {ex:?}"); }
}
