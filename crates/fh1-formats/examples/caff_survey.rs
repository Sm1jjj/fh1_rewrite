//! caff_survey <bin.zip> [n] : header words of CAFF `_0x*.bin` textures.
use std::collections::BTreeMap;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let n: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let mut shown = 0;
    let mut seen = std::collections::HashSet::new();
    let mut stats: BTreeMap<String, usize> = BTreeMap::new();
    for e in ar.entries.clone() {
        let name = e.name.to_ascii_lowercase();
        if !(name.starts_with("_0x") && name.ends_with(".bin")) || !seen.insert(name.clone()) { continue; }
        let d = ar.read(&e).unwrap();
        let be = |o: usize| u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
        let tex = d.windows(8).position(|w| w == b"texture\0").unwrap_or(0);
        let o = tex + 0x18;
        let key = format!("fmt {:#x} w1 {:#x} mips {} tail {:#x} gpu_at_end {}", be(o) & 0x3f, be(o + 4), be(o + 24) >> 24, be(o + 28), d.len() - be(0x88) as usize);
        *stats.entry(key).or_default() += 1;
        if shown < n {
            shown += 1;
            let o = tex + 0x18;
            let w: Vec<String> = (0..10).map(|i| format!("{:08x}", be(o + i * 4))).collect();
            println!("{} len={} data={:#x} gpu={:#x} {}", e.name, d.len(), be(0x60), be(0x88), w.join(" "));
        }
    }
    for (k, v) in stats { println!("{v:5} {k}"); }
}
