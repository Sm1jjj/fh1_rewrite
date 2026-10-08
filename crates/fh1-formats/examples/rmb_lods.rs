//! rmb_lods <bin.zip> : submodel triangle counts by LOD level and helper class, plus the extent of each
//! class (to see what distant geometry exists).
use std::collections::{BTreeMap, HashSet};
fn class(name: &str) -> String {
    let n = name.to_ascii_uppercase();
    for k in ["TERR_UBERLOD", "TERR_CUBE_", "SHADOWCASTER", "SHADOWBOX", "SHADOW_TERRAIN", "FARTERRAIN", "MIDDIST", "_MID", "_CAGE", "_NOLOD"] {
        if n.contains(k) { return k.to_string(); }
    }
    "normal".into()
}
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let mut seen = HashSet::new();
    let mut stats: BTreeMap<(String, u32), (usize, usize, [f32; 4], Vec<String>)> = BTreeMap::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".rmb.bin") || !seen.insert(n) { continue; }
        let Ok(m) = fh1_formats::rmb::parse(&ar.read(&e).unwrap()) else { continue };
        for s in &m.submodels {
            let t: usize = s.meshes.iter().map(|x| x.indices.len() / 3).sum();
            let ent = stats.entry((class(&s.name), s.lod())).or_insert((0, 0, [f32::MAX, f32::MAX, f32::MIN, f32::MIN], vec![]));
            ent.0 += 1;
            ent.1 += t;
            for p in &s.positions {
                ent.2 = [ent.2[0].min(p[0]), ent.2[1].min(p[2]), ent.2[2].max(p[0]), ent.2[3].max(p[2])];
            }
            if ent.3.len() < 3 { ent.3.push(s.name.clone()); }
        }
    }
    for ((c, lod), (n, t, b, ex)) in stats {
        println!("{c:14} lod {lod:2}: {n:6} submodels {t:9} tris  x {:.0}..{:.0} z {:.0}..{:.0}  {ex:?}", b[0], b[2], b[1], b[3]);
    }
}
