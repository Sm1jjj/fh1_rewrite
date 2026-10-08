//! scene_counts [disc_root] — parses every UI scene in `media/UI.zip` and prints one line per scene:
//! `name bytes objects slides tracks keys actions strings meshes texrefs nodes fbf_records resources`,
//! then the totals. Used to diff against the reference reader's counts.

use std::collections::BTreeMap;
use std::path::PathBuf;

use fh1_formats::zip::Archive;
use fh1_ui::anark::Scene;

fn main() {
    let root = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("disc"));
    let mut ar = Archive::open(root.join("media/UI.zip")).expect("open UI.zip");
    let mut files: BTreeMap<String, [Option<Vec<u8>>; 3]> = BTreeMap::new();
    for e in ar.entries.clone() {
        let lower = e.name.to_lowercase().replace('\\', "/");
        let Some(file) = lower.strip_prefix("scenes/ui4/") else { continue };
        let Some((stem, ext)) = file.rsplit_once('.') else { continue };
        let slot = match ext {
            "bgf" => 0,
            "fbf" => 1,
            "bsg" => 2,
            _ => continue,
        };
        let stem = e.name.rsplit('/').next().unwrap_or(stem).rsplit_once('.').map_or(stem, |s| s.0);
        files.entry(stem.to_string()).or_default()[slot] = Some(ar.read(&e).expect("read"));
    }
    let mut tot = [0usize; 12];
    for (name, [bgf, fbf, bsg]) in &files {
        let Some(bgf) = bgf else { continue };
        let s = Scene::load(bgf, fbf.as_deref(), bsg.as_deref()).unwrap_or_else(|e| panic!("{name}: {e}"));
        let bytes = [Some(bgf), fbf.as_ref(), bsg.as_ref()].iter().flatten().map(|b| b.len()).sum();
        let b = &s.bgf;
        let c = [
            bytes,
            b.objects.len(),
            b.slides.len(),
            b.tracks.len(),
            b.tracks.iter().map(|t| t.keys.len()).sum(),
            b.actions().count(),
            b.strings.len(),
            s.fbf.as_ref().map_or(0, |f| f.meshes.len()),
            s.fbf.as_ref().map_or(0, |f| f.images().count()),
            s.bsg.as_ref().map_or(0, |g| g.nodes.len()),
            s.fbf.as_ref().map_or(0, |f| f.records.len()),
            s.bsg.as_ref().map_or(0, |g| g.resources.len()),
        ];
        for (t, v) in tot.iter_mut().zip(c) {
            *t += v;
        }
        println!("{name} {}", c.map(|v| v.to_string()).join(" "));
    }
    println!("TOTAL {}", tot.map(|v| v.to_string()).join(" "));
}
