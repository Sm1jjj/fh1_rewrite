//! evalbench <scene_dir> <SCENE> [frames] — times `Player::evaluate` (the per-frame UI cost the
//! engine pays in `draw_scenes`), advancing 1/60 s per frame.

use fh1_ui::anark::Scene;
use fh1_ui::player::Player;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let base = std::path::Path::new(&a[1]).join(a[2].to_lowercase());
    let frames: usize = a.get(3).and_then(|f| f.parse().ok()).unwrap_or(600);
    let read = |e: &str| std::fs::read(base.with_extension(e)).ok();
    let mut p = Player::new(Scene::load(&read("bgf").unwrap(), read("fbf").as_deref(), read("bsg").as_deref()).unwrap());
    let (mut draws, mut tu, mut te) = (0, 0.0, 0.0);
    for _ in 0..frames {
        let t = std::time::Instant::now();
        p.update(1000.0 / 60.0);
        tu += t.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        draws += p.evaluate().draws.len();
        te += t.elapsed().as_secs_f64();
    }
    let t = std::time::Instant::now();
    let mut over = 0;
    for _ in 0..frames {
        over += p.bench_resolve();
    }
    let tr = t.elapsed().as_secs_f64();
    let per = |s: f64| s * 1000.0 / frames as f64;
    println!("  of which resolve {:.3} ms ({} overlay entries)", per(tr), over / frames);
    println!("{}: update {:.3} ms + evaluate {:.3} ms per frame, {} draws/frame, {} objects", a[2], per(tu), per(te), draws / frames, p.scene.bgf.objects.len());
}
