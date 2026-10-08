//! matsurvey <bin.zip> [shader-filter] — per track shader: material constant counts, technique
//! values and sample constant values, next to the shader's constant tables.

use std::collections::{BTreeMap, HashSet};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let filter = args.get(2).map(|s| s.to_ascii_lowercase());
    let mut ar = fh1_formats::zip::Archive::open(&args[1]).unwrap();
    let mut seen = HashSet::new();
    // shader -> (count, set of (tech, nvs, nps, nslots), samples)
    let mut by: BTreeMap<String, (usize, BTreeMap<(u32, usize, usize, usize), usize>, Vec<(Vec<[f32; 4]>, Vec<[f32; 4]>, Vec<i32>)>)> = BTreeMap::new();
    let mut fx: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if n.ends_with(".fxobj") {
            let stem = n.rsplit('/').next().unwrap().trim_end_matches(".fxobj").to_string();
            fx.insert(stem, ar.read(&e).unwrap());
        }
    }
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".rmb.bin") || !seen.insert(n) {
            continue;
        }
        let Ok(m) = fh1_formats::rmb::parse(&ar.read(&e).unwrap()) else { continue };
        for mat in &m.materials {
            let Some(sh) = m.shaders.get(mat.shader as usize) else { continue };
            let name = sh.rsplit(['\\', '/']).next().unwrap().trim_end_matches(".fx").to_ascii_lowercase();
            if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
                continue;
            }
            let ent = by.entry(name).or_default();
            ent.0 += 1;
            *ent.1.entry((mat.technique, mat.vs_constants.len(), mat.ps_constants.len(), mat.texture_slots.len())).or_default() += 1;
            if ent.2.len() < 3 {
                ent.2.push((mat.vs_constants.clone(), mat.ps_constants.clone(), mat.texture_slots.clone()));
            }
        }
    }
    for (name, (count, shapes, samples)) in &by {
        println!("== {name}: {count} materials, (tech, nvs, nps, nslots): {shapes:?}");
        if let Some(d) = fx.get(name) {
            let e = fh1_shaders::effect::Effect::parse(d).unwrap();
            for (i, s) in e.shaders.iter().enumerate() {
                let regs: Vec<String> = s
                    .constants
                    .iter()
                    .map(|c| format!("{}={:?}{}x{}", c.name, c.set, c.register, c.count))
                    .collect();
                println!("   shader {i} {:?}: {}", s.stage, regs.join(" "));
            }
        }
        for (vs, ps, slots) in samples {
            println!("   vs {vs:?}\n   ps {ps:?}\n   slots {slots:?}");
        }
    }
}
