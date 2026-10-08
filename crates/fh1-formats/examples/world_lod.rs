//! world_lod <track.pvs> <bin.zip> > out.tsv : per model number, its submodel names / classes / LODs, model
//! bounds, the `.pvs` model record's 60-byte tail (2 u32 + 13 f32) and its 18-byte records. Used to find the
//! game's distance rules for full-detail vs MIDDIST vs UberLOD geometry (docs/WORLD_LOD.md).
use std::collections::{BTreeMap, HashSet};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pvs = std::fs::read(&a[1]).unwrap();
    let u = |o: usize| u32::from_be_bytes(pvs[o..o + 4].try_into().unwrap());
    // Same walk as fh1_formats::pvs::parse, keeping the 60-byte model tails.
    let mut p = 32;
    p += 4 + 4 * u(p) as usize + 7;
    p += 4 + 28 * u(p) as usize;
    let n = u(p) as usize;
    p += 4;
    for _ in 0..n {
        p += 4 + u(p) as usize;
    }
    let n18 = u(p) as usize;
    let recs: Vec<&[u8]> = pvs[p + 4..p + 4 + n18 * 18].chunks(18).collect();
    p += 4 + n18 * 18;
    let nm = u(p) as usize;
    p += 4;
    let mut tails = Vec::with_capacity(nm);
    for _ in 0..nm {
        p += 4 + 4 * u(p) as usize;
        p += 4 + 4 * u(p) as usize;
        tails.push(pvs[p..p + 60].to_vec());
        p += 60;
    }
    eprintln!("18-byte records {n18}, models {nm}, end at {p:#x} of {:#x}", pvs.len());
    let mut by_model: BTreeMap<u16, Vec<String>> = BTreeMap::new();
    for (i, r) in recs.iter().enumerate() {
        let m = u16::from_be_bytes([r[0], r[1]]);
        by_model.entry(m).or_default().push(format!("{i}:{}", r.iter().map(|b| format!("{b:02x}")).collect::<String>()));
    }
    let mut ar = fh1_formats::zip::Archive::open(&a[2]).unwrap();
    let mut seen = HashSet::new();
    for e in ar.entries.clone() {
        let name = e.name.to_ascii_lowercase();
        let Some(num) = name.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<usize>().ok()) else { continue };
        if !seen.insert(name) {
            continue;
        }
        let Ok(m) = fh1_formats::rmb::parse(&ar.read(&e).unwrap()) else { continue };
        let subs: Vec<String> = m.submodels.iter().map(|s| format!("{}[{:?}]", s.name, s.class())).collect();
        let t = &tails[num];
        let w = |k: usize| u32::from_be_bytes(t[k * 4..k * 4 + 4].try_into().unwrap());
        let tail: Vec<String> = (0..15).map(|k| if k < 2 { format!("{:#x}", w(k)) } else { format!("{:.1}", f32::from_bits(w(k))) }).collect();
        println!(
            "{num}\t{:.0},{:.0},{:.0}\t{:.0},{:.0},{:.0}\t{}\t{}\t{}",
            m.bounds_min[0], m.bounds_min[1], m.bounds_min[2], m.bounds_max[0], m.bounds_max[1], m.bounds_max[2],
            tail.join(","),
            by_model.get(&(num as u16)).map(|v| v.join(" ")).unwrap_or_default(),
            subs.join(";")
        );
    }
}
