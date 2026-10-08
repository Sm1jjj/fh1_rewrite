//! `.pgeo` models with no LOD0 reference (their placements are skipped): bounds, placement count,
//! and the templates (from an `rmb_list` dump) whose bounds equal them.
//!
//! `cargo run --release -p fh1-formats --example props_unref -- <bin.zip> <rmb_list dump>`

use std::collections::HashMap;

use fh1_formats::props::parse_pgeo;
use fh1_formats::zip::Archive;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = Archive::open(&a[1]).expect("bin.zip");
    let mut templates = Vec::new();
    for line in std::fs::read_to_string(&a[2]).expect("dump").lines() {
        let p: Vec<&str> = line.split('|').collect();
        let v = |s: &str| -> Vec<f32> { s.split(',').filter_map(|x| x.parse().ok()).collect() };
        if p.len() >= 4 {
            templates.push((p[0].to_owned(), v(p[1]), v(p[2]), p[3].chars().take(50).collect::<String>()));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut by: HashMap<String, (usize, usize)> = HashMap::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".pgeo") || !seen.insert(n) {
            continue;
        }
        let Ok(g) = parse_pgeo(&ar.read(&e).unwrap()) else { continue };
        for (i, m) in g.models.iter().enumerate() {
            if !m.lod0.is_empty() {
                continue;
            }
            let count = g.placements.iter().filter(|p| p.model == i).count();
            let hit = templates.iter().find(|t| t.1.len() == 3 && t.2.len() == 3 && (0..3).all(|k| (t.1[k] - m.bounds_min[k]).abs() < 0.02 && (t.2[k] - m.bounds_max[k]).abs() < 0.02));
            let key = format!("{:?}..{:?} -> {}", m.bounds_min.map(|x| (x * 100.0).round() / 100.0), m.bounds_max.map(|x| (x * 100.0).round() / 100.0), hit.map_or("-".to_owned(), |t| format!("{} {}", t.0, t.3)));
            let e = by.entry(key).or_default();
            e.0 += 1;
            e.1 += count;
        }
    }
    let mut v: Vec<_> = by.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1 .1));
    for (k, (models, placements)) in v {
        println!("{placements:5} placements {models:3} models  {k}");
    }
}
