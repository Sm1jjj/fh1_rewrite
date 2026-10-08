//! pvs <track.pvs> <bin.zip> : checks the PVS model bindings against every `<name>.NNNNN.rmb.bin`:
//! shader lists must name the model's own shaders, and every material texture slot must be in range.
use std::collections::HashSet;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pvs = fh1_formats::pvs::parse(&std::fs::read(&a[1]).unwrap()).unwrap();
    println!("textures {} shaders {} models {}", pvs.textures.len(), pvs.shaders.len(), pvs.models.len());
    let mut ar = fh1_formats::zip::Archive::open(&a[2]).unwrap();
    let names: HashSet<String> = ar.entries.iter().map(|e| e.name.to_ascii_lowercase()).collect();
    let (mut ok, mut bad_sh, mut bad_slot, mut parse_err, mut slots, mut missing, mut runtime) = (0, 0, 0, 0, 0, 0, 0);
    let mut seen = HashSet::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        let Some(num) = n.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<usize>().ok()) else { continue };
        if !seen.insert(n.clone()) { continue; }
        let Ok(m) = fh1_formats::rmb::parse(&ar.read(&e).unwrap()) else { parse_err += 1; continue };
        let b = &pvs.models[num];
        let sh_ok = b.shaders.len() == m.shaders.len()
            && b.shaders.iter().zip(&m.shaders).all(|(&g, own)| own.eq_ignore_ascii_case(&format!("{}.fx", pvs.shaders[g as usize])));
        let max_slot = m.materials.iter().flat_map(|mt| mt.texture_slots.iter().copied()).max().unwrap_or(-1);
        let slot_ok = max_slot + 1 == b.textures.len() as i32;
        if !sh_ok { bad_sh += 1; if bad_sh <= 3 { println!("shader mismatch {n}: {:?} vs {:?}", m.shaders, b.shaders.iter().map(|&g| &pvs.shaders[g as usize]).collect::<Vec<_>>()); } }
        if !slot_ok { bad_slot += 1; if bad_slot <= 3 { println!("slot count {n}: max slot {max_slot}, list {}", b.textures.len()); } }
        if sh_ok && slot_ok { ok += 1; }
        for mt in &m.materials {
            for &s in &mt.texture_slots {
                let Some(t) = pvs.texture(num, s) else { continue };
                slots += 1;
                match t.file_name() {
                    None => runtime += 1,
                    Some(f) => if !names.contains(&f.to_ascii_lowercase()) { missing += 1; },
                }
            }
        }
    }
    println!("models ok {ok}, shader mismatch {bad_sh}, slot-count mismatch {bad_slot}, rmb parse errors {parse_err}");
    println!("bound slots {slots}: runtime (no file) {runtime}, file missing from zip {missing}");
}
