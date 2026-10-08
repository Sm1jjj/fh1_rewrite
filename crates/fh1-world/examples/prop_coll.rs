//! prop_coll <installed world dir> <installed scenery/colorado dir> : for every prop placement (props/tiles), does the
//! track collision mesh already hold the prop (near-vertical triangles within 1.5 m of its position, 0.5-3 m above
//! its base)? Reported per template, only for placements that stand on collision (the drivable network).
//! Written for the smash/prop-collision work (fh1-engine smash.rs, worker 47).
use std::collections::BTreeMap;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let world = fh1_world::World::load(std::path::Path::new(&a[1])).unwrap();
    let dir = std::path::Path::new(&a[2]);
    let idx: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
    let mut stats: BTreeMap<u32, (usize, usize, usize)> = BTreeMap::new(); // template -> (placements, on collision, with walls)
    let mut contacts = Vec::new();
    for t in idx["props"]["tiles"].as_array().unwrap() {
        let b = std::fs::read(dir.join(t["file"].as_str().unwrap())).unwrap();
        let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        for i in 0..n {
            let o = 12 + i * 68;
            let model = u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            let f = |k: usize| f32::from_le_bytes(b[o + 4 + k * 4..o + 8 + k * 4].try_into().unwrap());
            // Engine space -> collision space (z mirrored).
            let (x, y, z) = (f(12), f(13), -f(14));
            let e = stats.entry(model).or_default();
            e.0 += 1;
            let Some(h) = world.raycast([x, y + 3.0, z], [0.0, -1.0, 0.0], 6.0) else { continue };
            if (h.point[1] - y).abs() > 1.5 {
                continue;
            }
            e.1 += 1;
            let mut walls = false;
            for dy in [0.7f32, 1.5, 2.5] {
                world.sphere_contacts([x, y + dy, z], 1.5, &mut contacts);
                walls |= contacts.iter().any(|c| {
                    let p = world.tri_points(c.tri);
                    let u = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
                    let v = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
                    let nrm = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
                    let l = (nrm[0] * nrm[0] + nrm[1] * nrm[1] + nrm[2] * nrm[2]).sqrt().max(1e-9);
                    (nrm[1] / l).abs() < 0.5
                });
            }
            e.2 += walls as usize;
        }
    }
    let mut rows: Vec<_> = stats.into_iter().filter(|(_, s)| s.1 >= 20).collect();
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.1));
    for (m, (n, on, w)) in rows {
        println!("{m}\t{n}\t{on}\t{w}\t{:.0}%", 100.0 * w as f32 / on as f32);
    }
}
