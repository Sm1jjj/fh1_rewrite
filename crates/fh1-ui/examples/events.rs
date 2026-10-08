//! events <scene_dir> <SCENE> — lists the scene's event handlers with event names (from
//! `EventNames.txt` next to the scenes) and what each action does.

use std::collections::HashMap;

use fh1_ui::anark::bgf::Command;
use fh1_ui::anark::Scene;
use fh1_ui::hash::ahash31;
use fh1_ui::player::Player;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::Path::new(&a[1]);
    let names: HashMap<u32, String> = std::fs::read_to_string(dir.join("EventNames.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| (ahash31(l.trim().as_bytes()), l.trim().to_owned()))
        .collect();
    let base = dir.join(a[2].to_lowercase());
    let read = |e: &str| std::fs::read(base.with_extension(e)).ok();
    let p = Player::new(Scene::load(&read("bgf").unwrap(), read("fbf").as_deref(), read("bsg").as_deref()).unwrap());
    let slide_name = |c: usize, h: u32| p.scene.bgf.slides.iter().filter(|s| s.component as usize == c).find(|s| ahash31(s.name.as_bytes()) == h & 0x7FFF_FFFF).map(|s| s.name.clone());
    for h in &p.scene.bgf.handlers {
        for e in &h.events {
            let ev = names.get(&(e.hash31 & 0x7FFF_FFFF)).cloned().unwrap_or(format!("#{:08x}", e.hash31));
            let acts: Vec<String> = e
                .actions
                .iter()
                .map(|x| {
                    let t = x.target.max(0) as usize;
                    let tn = p.name(t).unwrap_or("?");
                    match x.command {
                        Command::GotoSlide => format!("{tn}->{}", slide_name(t, x.arg1).unwrap_or("?".into())),
                        Command::SetProperty => format!("{tn}.{}={}", fh1_ui::names::prop_name(x.arg1).unwrap_or("?"), x.arg2),
                        c => format!("{tn}:{c:?}({})", x.arg1),
                    }
                })
                .collect();
            println!("{:4} {ev}: {}", h.object, acts.join(", "));
        }
    }
}
