//! meshdump <SCENE> <disc_root> <object> [component=progress ...] — seeks the given components
//! (`set_progress`), then prints every draw whose name matches `object`: world matrix, materials
//! (diffuse, opacity, texture slots with their UV transforms) and the mesh vertices, or the text.

use std::path::PathBuf;

use fh1_formats::zip::Archive;
use fh1_ui::anark::Scene;
use fh1_ui::player::{DrawKind, Player};

fn main() {
    let mut args = std::env::args().skip(1);
    let scene = args.next().expect("scene name");
    let root = PathBuf::from(args.next().expect("disc root"));
    let object = args.next().expect("object name");
    let mut ar = Archive::open(root.join("media/UI.zip")).expect("open UI.zip");
    let mut read = |ext: &str| {
        let want = format!("scenes/ui4/{}.{ext}", scene.to_lowercase());
        let e = ar.entries.iter().find(|e| e.name.to_lowercase().replace('\\', "/") == want).cloned()?;
        ar.read(&e).ok()
    };
    let (bgf, fbf, bsg) = (read("bgf").expect("bgf"), read("fbf"), read("bsg"));
    let mut p = Player::new(Scene::load(&bgf, fbf.as_deref(), bsg.as_deref()).expect("parse"));
    for a in args {
        let (c, f) = a.split_once('=').expect("component=progress");
        let c = p.find(c).expect("component");
        p.set_progress(c, f.parse().unwrap());
        p.set_playing(c, false);
    }
    for d in p.evaluate().draws {
        if d.name != object {
            continue;
        }
        println!("{} layer {} opacity {:.3}", d.name, d.layer, d.opacity);
        for r in 0..4 {
            println!("  world row {r}: {:?}", d.world[r]);
        }
        if let DrawKind::Model { mesh, materials } = &d.kind {
            for m in materials {
                println!("  material submesh {} diffuse {:?} opacity {} additive {}", m.submesh, m.diffuse, m.opacity, m.additive);
                for t in &m.textures {
                    println!("    texture {t:?}");
                }
            }
            let mesh = &p.scene.fbf.as_ref().expect("fbf").meshes[*mesh];
            for v in &mesh.vertices {
                println!("  v pos {:?} uv {:?}", v.pos, v.uv);
            }
            println!("  groups {:?}", mesh.groups);
        }
        if let DrawKind::Text(t) = &d.kind {
            println!("  text {t:?}");
        }
    }
}
