//! world_probe <world dir> [x z]  (disc coordinates): per-surface area/verticality, and what's
//! above/below a point.
use fh1_world::World;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let w = World::load(std::path::Path::new(&a[1])).unwrap();
    let mut stats = vec![(0usize, 0f64, 0f64, f32::MAX, f32::MIN); w.surfaces.len()];
    for t in &w.tris {
        let [p0, p1, p2] = t.v.map(|i| w.verts[i as usize]);
        let u = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
        let v = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let area = 0.5 * ((n[0] * n[0] + n[1] * n[1] + n[2] * n[2]) as f64).sqrt();
        let s = &mut stats[t.surface as usize];
        s.0 += 1;
        s.1 += area;
        if (n[1] as f64).abs() / (2.0 * area).max(1e-9) < 0.3 { s.2 += area; }
        s.3 = s.3.min(p0[1]); s.4 = s.4.max(p0[1]);
    }
    for (i, s) in stats.iter().enumerate().filter(|(_, s)| s.0 > 0) {
        let sf = &w.surfaces[i];
        println!("{i:3} {:24} {:8} tris {:9.0} m2 walls {:4.0}% y {:6.0}..{:6.0} colour {:?} cat {:?}", sf.name, s.0, s.1, 100.0 * s.2 / s.1, s.3, s.4, sf.debug_color(), sf.category);
    }
    // routes mask histogram, walls vs floors
    let mut hist: std::collections::BTreeMap<(u16, bool), usize> = Default::default();
    for t in &w.tris {
        let [p0, p1, p2] = t.v.map(|i| w.verts[i as usize]);
        let u = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
        let v = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let wall = n[1].abs() / (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-9) < 0.3;
        *hist.entry((t.routes, wall)).or_default() += 1;
    }
    let mut h: Vec<_> = hist.into_iter().collect();
    h.sort_by(|a, b| b.1.cmp(&a.1));
    for ((r, wall), n) in h.iter().take(20) {
        println!("routes {r:#06x} {} {n}", if *wall { "wall " } else { "floor" });
    }
    if a.len() >= 6 {
        let c = [a[4].parse().unwrap(), a[5].parse().unwrap(), a[6].parse().unwrap()];
        let mut out = Vec::new();
        w.sphere_contacts(c, 3.0, &mut out);
        for ct in out.iter().take(12) {
            let t = w.tris[ct.tri as usize];
            println!("near ({c:?}) tri {} surface {} routes {:#06x} flags {:#04x} normal {:?}", ct.tri, w.surfaces[t.surface as usize].name, t.routes, t.flags, ct.normal);
        }
    }
    if a.len() >= 4 {
        let (x, z): (f32, f32) = (a[2].parse().unwrap(), a[3].parse().unwrap());
        let mut y = 2000.0;
        while let Some(h) = w.raycast([x, y, z], [0.0, -1.0, 0.0], y + 500.0) {
            println!("hit y {:8.2} surface {} ({})", h.point[1], h.surface, w.surfaces[h.surface as usize].name);
            y = h.point[1] - 0.01;
        }
    }
}

#[allow(dead_code)]
fn unused() {}
