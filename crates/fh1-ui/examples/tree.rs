//! tree <scene_dir> <SCENE> [depth] — prints the object tree (bgf S1) to a depth, skipping
//! materials, images and behaviours: index, kind, name, child count.

use fh1_ui::anark::bgf::ObjectKind;
use fh1_ui::anark::Scene;
use fh1_ui::player::Player;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let base = std::path::Path::new(&a[1]).join(a[2].to_lowercase());
    let depth: usize = a.get(3).and_then(|d| d.parse().ok()).unwrap_or(3);
    let read = |e: &str| std::fs::read(base.with_extension(e)).ok();
    let p = Player::new(Scene::load(&read("bgf").unwrap(), read("fbf").as_deref(), read("bsg").as_deref()).unwrap());
    let objs = &p.scene.bgf.objects;
    let kids = |i: i32| objs.iter().enumerate().filter(move |(_, o)| o.parent == i && !matches!(o.kind, ObjectKind::Material | ObjectKind::Image | ObjectKind::Behavior)).map(|(j, _)| j);
    fn walk(p: &Player, i: usize, d: usize, max: usize, kids: &dyn Fn(i32) -> Vec<usize>) {
        let o = &p.scene.bgf.objects[i];
        let k = kids(i as i32);
        println!("{}{i} {:?} {} ({})", "  ".repeat(d), o.kind, p.name(i).unwrap_or("?"), k.len());
        if d < max {
            for c in k {
                walk(p, c, d + 1, max, kids);
            }
        }
    }
    let kv = |i: i32| kids(i).collect::<Vec<_>>();
    walk(&p, 0, 0, depth, &kv);
}
