//! tri_flags <installed world dir> : triangle count and area by (wall / ground, surface, flags byte, free-roam bit), to see whether the
//! `.fiz` polygon flags separate always-solid walls from breakable ones (barn-find access work, 2026-10-09).
use std::collections::BTreeMap;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let w = fh1_world::World::load(std::path::Path::new(&a[1])).expect("world");
    let mut h: BTreeMap<(bool, String, u8, bool), (usize, f64)> = BTreeMap::new();
    for (ti, t) in w.tris.iter().enumerate() {
        let p = w.tri_points(ti as u32);
        let u = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let v = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let wall = n[1].abs() / l.max(1e-12) < 0.5;
        let e = h.entry((wall, w.surface(t.surface).map_or("?".into(), |s| s.name.clone()), t.flags, t.routes & 0x8000 != 0)).or_default();
        e.0 += 1;
        e.1 += l as f64 * 0.5;
    }
    for ((wall, s, f, free), (n, area)) in h {
        if n >= 200 {
            println!("{} {:24} flags {:#04x} {:>3} {n:9} tris {area:12.0} m2", if wall { "WALL  " } else { "ground" }, s, f, if free { "free" } else { "evt" });
        }
    }
}
