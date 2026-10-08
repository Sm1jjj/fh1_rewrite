//! tracks <scene_dir> <SCENE> <first> <last> — every keyframe track and master prop of the
//! objects with bgf index in first..=last (name, property, keys).

use fh1_ui::anark::{PropValue, Scene};
use fh1_ui::player::Player;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let base = std::path::Path::new(&a[1]).join(a[2].to_lowercase());
    let (lo, hi): (usize, usize) = (a[3].parse().unwrap(), a[4].parse().unwrap());
    let read = |e: &str| std::fs::read(base.with_extension(e)).ok();
    let p = Player::new(Scene::load(&read("bgf").unwrap(), read("fbf").as_deref(), read("bsg").as_deref()).unwrap());
    let b = &p.scene.bgf;
    for i in lo..=hi {
        let Some(o) = b.objects.get(i) else { continue };
        let props: Vec<String> = o
            .props
            .iter()
            .filter_map(|pr| {
                let n = pr.name()?;
                Some(match pr.value() {
                    PropValue::Float(f) => format!("{n}={f:.3}"),
                    PropValue::Int(v) => format!("{n}={v}"),
                    v => format!("{n}={v:?}"),
                })
            })
            .collect();
        println!("{i} {:?} {} parent {} | {}", o.kind, p.name(i).unwrap_or("?"), o.parent, props.join(" "));
        for t in b.tracks.iter().filter(|t| t.object as usize == i) {
            let keys: Vec<String> = t.keys.iter().map(|k| format!("{}:{:.3}", k.time, k.value)).collect();
            println!("    track {} {}", fh1_ui::names::prop_name(t.key).unwrap_or("?"), keys.join(" "));
        }
    }
}
