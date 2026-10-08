//! shadow_survey <track.pvs> <bin.zip> : which Colorado models are shadow-only proxies (rmb `Class::Shadow`),
//! their shaders, sizes and `.pvs` 18-byte records, plus the record flag/band/class byte histograms by
//! submodel class. Used to pick the shadow casters (docs/SHADOWS.md).
use std::collections::{BTreeMap, HashMap, HashSet};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pvs = std::fs::read(&a[1]).unwrap();
    let recs = fh1_formats::props::pvs_records(&pvs).unwrap();
    let mut by_model: HashMap<u16, Vec<[u8; 18]>> = HashMap::new();
    for r in &recs {
        by_model.entry(u16::from_be_bytes([r[0], r[1]])).or_default().push(*r);
    }
    let mut ar = fh1_formats::zip::Archive::open(&a[2]).unwrap();
    let mut seen = HashSet::new();
    // (class of the model's first submodel, byte index, value) -> count
    let mut hist: BTreeMap<(String, usize, u8), usize> = BTreeMap::new();
    let mut shaders: BTreeMap<String, usize> = BTreeMap::new();
    let mut mixed = 0;
    let mut prefixes: BTreeMap<(String, u8, u8, String), usize> = BTreeMap::new();
    for e in ar.entries.clone() {
        let name = e.name.to_ascii_lowercase();
        let Some(num) = name.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<usize>().ok()) else { continue };
        if !seen.insert(name) {
            continue;
        }
        let Ok(m) = fh1_formats::rmb::parse(&ar.read(&e).unwrap()) else { continue };
        let classes: HashSet<_> = m.submodels.iter().map(|s| s.class()).collect();
        let cls = m.submodels.first().map(|s| format!("{:?}", s.class())).unwrap_or_default();
        for r in by_model.get(&(num as u16)).map(|v| v.as_slice()).unwrap_or(&[]) {
            for k in [4usize, 5, 6, 7] {
                *hist.entry((cls.clone(), k, r[k])).or_default() += 1;
            }
        }
        let first = m.submodels.first().map(|s| s.name.to_ascii_uppercase()).unwrap_or_default();
        let toks: Vec<&str> = first.split('_').collect();
        let pre = toks.iter().take(2).cloned().collect::<Vec<_>>().join("_");
        for r in by_model.get(&(num as u16)).map(|v| v.as_slice()).unwrap_or(&[]) {
            if r[6] & 0x38 != 0 {
                *prefixes.entry((cls.clone(), r[4], r[5], pre.clone())).or_default() += 1;
            }
        }
        if !classes.contains(&fh1_formats::rmb::Class::Shadow) {
            continue;
        }
        if classes.len() > 1 {
            mixed += 1;
        }
        for s in &m.submodels {
            let tris: usize = s.meshes.iter().map(|x| x.indices.len() / 3).sum();
            let sh: Vec<String> = s.meshes.iter().map(|x| m.materials.get(x.material as usize).and_then(|mt| m.shaders.get(mt.shader as usize)).cloned().unwrap_or_default()).collect();
            for x in &sh {
                *shaders.entry(x.clone()).or_default() += 1;
            }
            let r: Vec<String> = by_model.get(&(num as u16)).map(|v| v.iter().map(|r| r[2..8].iter().map(|b| format!("{b:02x}")).collect()).collect()).unwrap_or_default();
            println!(
                "{num}\t{}\t{:?}\t{tris}\t{:.0},{:.0},{:.0}\t{:.0},{:.0},{:.0}\t{}\t{}",
                s.name, s.class(), m.bounds_min[0], m.bounds_min[1], m.bounds_min[2], m.bounds_max[0], m.bounds_max[1], m.bounds_max[2],
                sh.join(","),
                r.join(" ")
            );
        }
    }
    for ((c, f, g, p), n) in &prefixes {
        eprintln!("pre {c} {f:02x} {g:02x} {p} {n}");
    }
    eprintln!("models with shadow + other classes: {mixed}");
    for (s, n) in shaders {
        eprintln!("shader {s}: {n}");
    }
    for ((c, k, v), n) in hist {
        eprintln!("hist {c} byte{k} {v:02x}: {n}");
    }
}
