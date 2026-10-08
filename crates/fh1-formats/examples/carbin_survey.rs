//! carbin_survey <disc>/media/cars : parse every non-stripped .carbin in every car zip.
use fh1_formats::{carbin, zip::Archive};
use std::collections::BTreeMap;
fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let mut stats: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut fails = Vec::new();
    for f in std::fs::read_dir(&dir).unwrap() {
        let p = f.unwrap().path();
        if p.extension().is_none_or(|e| e != "zip") { continue; }
        let Ok(mut ar) = Archive::open(&p) else { continue };
        let car = p.file_stem().unwrap().to_string_lossy().to_lowercase();
        for e in ar.entries.clone() {
            let n = e.name.to_lowercase();
            if !n.ends_with(".carbin") || n.starts_with("stripped_") || n.contains('/') { continue; }
            let kind = n.trim_end_matches(".carbin").strip_prefix(&car).unwrap_or("?").trim_start_matches('_').to_string();
            let kind = if kind.is_empty() { "main".into() } else if kind.starts_with("caliper") { "caliper".into() } else if kind.starts_with("rotor") { "rotor".into() } else { kind };
            let data = ar.read(&e).unwrap();
            let ok = match carbin::parse(&data) {
                Ok(c) => {
                    let tris: usize = c.sections.iter().flat_map(|s| &s.subsections).map(|s| s.indices.len() / 3).sum();
                    tris > 0
                }
                Err(err) => { fails.push(format!("{}: {err}", e.name)); false }
            };
            let s = stats.entry(kind).or_default();
            if ok { s.0 += 1 } else { s.1 += 1 }
        }
    }
    for (k, (ok, bad)) in &stats { println!("{k:10} ok {ok:4} failed {bad}"); }
    for f in fails.iter().take(15) { println!("  {f}"); }
}
