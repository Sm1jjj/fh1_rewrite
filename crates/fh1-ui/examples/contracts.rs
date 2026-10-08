//! contracts <scene_dir> <SCENE> — lists a scene's data-binding contracts: type, owner, and each
//! field path with the object it resolves to. Also lists the components and their slides.

use fh1_ui::anark::Scene;
use fh1_ui::player::Player;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let base = std::path::Path::new(&a[1]).join(a[2].to_lowercase());
    let read = |e: &str| std::fs::read(base.with_extension(e)).ok();
    let p = Player::new(Scene::load(&read("bgf").expect("bgf"), read("fbf").as_deref(), read("bsg").as_deref()).expect("parse"));
    for c in p.contracts() {
        println!("{} on {} ({})", c.kind, c.owner, p.name(c.owner).unwrap_or("?"));
        for (i, (h, path)) in c.fields.iter().enumerate() {
            let to = c.target(&p, i);
            println!("    {h:07x} {path} -> {:?}", to.map(|o| (o, p.name(o).unwrap_or("?"))));
        }
    }
    let b = &p.scene.bgf;
    let mut comps: Vec<usize> = p.components().collect();
    comps.sort();
    for c in comps {
        let slides: Vec<String> = b.slides.iter().filter(|s| s.component as usize == c).map(|s| format!("{}[f{} {}-{}]", s.name, s.flags, s.start, s.end)).collect();
        println!("component {c} {}: {}", p.name(c).unwrap_or("?"), slides.join(" "));
    }
}
