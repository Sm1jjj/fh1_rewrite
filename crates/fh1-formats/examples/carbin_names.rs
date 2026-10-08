//! carbin_names <disc>/media/cars : section-name frequency across main carbins.
use fh1_formats::{carbin, zip::Archive};
use std::collections::BTreeMap;
fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    for f in std::fs::read_dir(&dir).unwrap() {
        let p = f.unwrap().path();
        let car = p.file_stem().unwrap().to_string_lossy().to_string();
        let Ok(mut ar) = Archive::open(&p) else { continue };
        let Some(e) = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(&format!("{car}.carbin"))).cloned() else { continue };
        if let Ok(c) = carbin::parse(&ar.read(&e).unwrap()) {
            for s in &c.sections { *names.entry(s.name.to_lowercase()).or_default() += 1; }
        }
    }
    let mut v: Vec<_> = names.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (n, c) in v { print!("{n}:{c} "); }
    println!();
}
