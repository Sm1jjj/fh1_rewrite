//! template_names <bin.zip> [substring...] : `n<TAB>first submodel name<TAB>header bounds` for every
//! `coloradoout.<n>.rmb.bin`, or only those whose submodel names contain one of the substrings (case-insensitive).
//! Used to find the prop templates that are fences / barriers (fh1-engine smash.rs fence colliders, 2026-10-08).
use std::collections::HashSet;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).expect("open archive");
    let keys: Vec<String> = a[2..].iter().map(|s| s.to_ascii_uppercase()).collect();
    let mut seen = HashSet::new();
    for e in ar.entries.clone() {
        let name = e.name.to_ascii_lowercase();
        if !name.ends_with(".rmb.bin") || !seen.insert(name.clone()) {
            continue;
        }
        let Some(n) = name.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(d) = ar.read(&e) else { continue };
        let Ok(m) = fh1_formats::rmb::parse(&d) else { continue };
        let Some(first) = m.submodels.first() else { continue };
        if !keys.is_empty() && !m.submodels.iter().any(|s| keys.iter().any(|k| s.name.to_ascii_uppercase().contains(k))) {
            continue;
        }
        let f = |o: usize| f32::from_be_bytes(d[o..o + 4].try_into().unwrap());
        println!("{n}\t{}\t{:.2},{:.2},{:.2}\t{:.2},{:.2},{:.2}", first.name, f(4), f(8), f(12), f(20), f(24), f(28));
    }
}
