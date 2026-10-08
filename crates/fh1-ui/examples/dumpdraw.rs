//! dumpdraw <SCENE> [disc_root] [EVENT...] — evaluates a UI scene in its default state (then fires
//! the given events) and prints the draw list, one line per item:
//! `M|T name layer x y opacity` (x, y = world origin). Used to diff against the reference renderer.

use std::path::PathBuf;

use fh1_formats::zip::Archive;
use fh1_ui::anark::Scene;
use fh1_ui::player::{apply, DrawKind, Player};

fn main() {
    let mut args = std::env::args().skip(1);
    let scene = args.next().expect("scene name");
    let root = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("disc"));
    let events: Vec<String> = args.collect();
    let mut ar = Archive::open(root.join("media/UI.zip")).expect("open UI.zip");
    let mut read = |ext: &str| {
        let want = format!("scenes/ui4/{}.{ext}", scene.to_lowercase());
        let e = ar.entries.iter().find(|e| e.name.to_lowercase().replace('\\', "/") == want).cloned()?;
        ar.read(&e).ok()
    };
    let (bgf, fbf, bsg) = (read("bgf").expect("bgf"), read("fbf"), read("bsg"));
    let mut p = Player::new(Scene::load(&bgf, fbf.as_deref(), bsg.as_deref()).expect("parse"));
    for e in &events {
        eprintln!("{e}: {} actions", p.fire(e));
    }
    for d in p.evaluate().draws {
        let o = apply(&d.world, [0.0; 3]);
        let k = match d.kind {
            DrawKind::Model { .. } => 'M',
            DrawKind::Text(_) => 'T',
        };
        println!("{k} {} {} {:.1} {:.1} {:.3}", d.name, d.layer, o[0], o[1], d.opacity);
    }
}
