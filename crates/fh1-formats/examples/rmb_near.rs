//! rmb_near <pvs> <bin.zip> <x> <z> <radius> : full-detail submodels near a point (engine space), with
//! each mesh's shader and slot-0 texture.
use std::collections::HashSet;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pvs = fh1_formats::pvs::parse(&std::fs::read(&a[1]).unwrap()).unwrap();
    let mut ar = fh1_formats::zip::Archive::open(&a[2]).unwrap();
    let (x, z, r): (f32, f32, f32) = (a[3].parse().unwrap(), a[4].parse().unwrap(), a[5].parse().unwrap());
    let mut seen = HashSet::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        let Some(num) = n.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<usize>().ok()) else { continue };
        if !seen.insert(n.clone()) { continue; }
        let m = fh1_formats::rmb::parse(&ar.read(&e).unwrap()).unwrap();
        for s in m.submodels.iter().filter(|s| s.lod() == 0 && !s.is_helper() && !s.positions.is_empty()) {
            let c = s.positions.iter().fold([0.0f32; 3], |acc, p| [acc[0] + p[0], acc[1] + p[1], acc[2] + p[2]]).map(|v| v / s.positions.len() as f32);
            if ((c[0] - x).powi(2) + (c[2] - z).powi(2)).sqrt() > r { continue; }
            for mesh in &s.meshes {
                let Some(mat) = m.materials.get(mesh.material as usize) else { continue };
                let t = mat.texture_slots.first().and_then(|&s0| pvs.texture(num, s0));
                println!("{n} {:40} c=({:.0},{:.0},{:.0}) {:24} {} tex {:x?}", s.name, c[0], c[1], c[2], mesh.name, m.shaders[mat.shader as usize].rsplit(char::from(92)).next().unwrap(), t.map(|t| t.file_id));
            }
        }
    }
}
