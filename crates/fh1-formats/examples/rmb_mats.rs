//! rmb_mats <bin.zip> <name filter> : per-material shader and texture slots of matching .rmb.bin models.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let filt = a.get(2).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".rmb.bin") || !n.contains(&filt) { continue; }
        let m = fh1_formats::rmb::parse(&ar.read(&e).unwrap()).unwrap();
        println!("{} materials={} shaders={:?}", e.name, m.materials.len(), m.shaders);
        for (i, mt) in m.materials.iter().enumerate() {
            println!("  m{i:3} shader={} tech={} slots={:?}", mt.shader, mt.technique, mt.texture_slots);
        }
    }
}
