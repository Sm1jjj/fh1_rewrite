//! rmb_class_names <bin.zip> <substring> : names (by prefix) and triangle counts of submodels whose name
//! contains the substring (case-insensitive), with each one's helper flag.
use std::collections::{BTreeMap, HashSet};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let key = a[2].to_ascii_uppercase();
    let mut seen = HashSet::new();
    let mut by: BTreeMap<String, (usize, usize, bool)> = BTreeMap::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".rmb.bin") || !seen.insert(n) { continue; }
        let Ok(m) = fh1_formats::rmb::parse(&ar.read(&e).unwrap()) else { continue };
        for s in m.submodels.iter().filter(|s| s.name.to_ascii_uppercase().contains(&key)) {
            let p: String = s.name.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect();
            let ent = by.entry(p).or_insert((0, 0, s.is_helper()));
            ent.0 += 1;
            ent.1 += s.meshes.iter().map(|x| x.indices.len() / 3).sum::<usize>();
        }
    }
    let mut v: Vec<_> = by.into_iter().collect();
    v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    for (k, (n, t, h)) in v.iter().take(40) { println!("{t:8} tris {n:4}x helper={h} {k}"); }
}
