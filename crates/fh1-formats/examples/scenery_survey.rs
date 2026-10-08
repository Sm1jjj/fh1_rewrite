//! scenery_survey <pvs> <bin.zip> [max models] : stats for binding diffuse textures (material slot 0)
//! to Colorado models: uv offset/scale ranges, slot-0 texture kinds and sizes.
use std::collections::{BTreeMap, HashMap, HashSet};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pvs = fh1_formats::pvs::parse(&std::fs::read(&a[1]).unwrap()).unwrap();
    let max: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let mut ar = fh1_formats::zip::Archive::open(&a[2]).unwrap();
    let by: HashMap<String, _> = ar.entries.iter().map(|e| (e.name.to_ascii_lowercase(), e.clone())).collect();
    let (mut kinds, mut shaders): (BTreeMap<&str, usize>, BTreeMap<String, (usize, usize)>) = Default::default();
    let mut used = HashSet::new();
    let mut seen = HashSet::new();
    let mut models = 0;
    let mut samples = 0;
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        let Some(num) = n.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<usize>().ok()) else { continue };
        if !seen.insert(n.clone()) { continue; }
        models += 1;
        if models > max { break; }
        let m = fh1_formats::rmb::parse(&ar.read(&e).unwrap()).unwrap();
        for s in m.submodels.iter().filter(|s| s.lod() == 0 && !s.is_helper()) {
            for mesh in &s.meshes {
                let Some(mat) = m.materials.get(mesh.material as usize) else { continue };
                let sh = m.shaders[mat.shader as usize].rsplit(char::from(92)).next().unwrap().to_string();
                let t = mat.texture_slots.first().and_then(|&s0| pvs.texture(num, s0));
                let kind = match t.and_then(|t| t.file_name()) { None if t.is_some() => "runtime", None => "none", Some(f) if f.ends_with(".bix") => "bix", Some(_) => "caff" };
                *kinds.entry(kind).or_default() += mesh.indices.len() / 3;
                let ent = shaders.entry(sh.clone()).or_default();
                ent.0 += mesh.indices.len() / 3;
                if kind == "bix" { ent.1 += mesh.indices.len() / 3; used.insert(t.unwrap().file_id); }
                // TRACE=<hex id,...>: who binds these textures at slot 0.
                if let (Ok(ids), Some(t)) = (std::env::var("TRACE"), t) {
                    if ids.split(',').any(|h| u32::from_str_radix(h, 16).ok() == Some(t.file_id)) {
                        println!("trace {:x}: {n} {} shader {sh} slots {:?} -> {:?}", t.file_id, s.name, mat.texture_slots,
                            mat.texture_slots.iter().map(|&s| pvs.texture(num, s).map(|t| format!("{:x}", t.file_id))).collect::<Vec<_>>());
                    }
                }
                if samples < 25 && models % 500 == 1 {
                    samples += 1;
                    println!("{n} {} {} uvOS={:?} slots={:?} vs={:?}", s.name, mesh.name, mesh.uv_offset_scale, mat.texture_slots, mat.vs_constants.first());
                }
            }
        }
    }
    println!("slot-0 kinds by triangles: {kinds:?}");
    let mut v: Vec<_> = shaders.into_iter().collect();
    v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for (k, (t, b)) in v.iter().take(40) { println!("{k:50} {t:9} bix {b:9}"); }
    let (mut px, mut bytes) = (0u64, 0u64);
    for id in &used {
        if let Some(e) = by.get(&format!("_0x{id:08x}.bix")) {
            let h = fh1_formats::bix::parse_header(&ar.read(e).unwrap()).unwrap();
            px += (h.width * h.height) as u64;
            bytes += h.base_bytes as u64;
        }
    }
    println!("distinct slot-0 bix {} base pixels {:.1} M base bytes {:.1} MB", used.len(), px as f64 / 1e6, bytes as f64 / 1e6);
}
