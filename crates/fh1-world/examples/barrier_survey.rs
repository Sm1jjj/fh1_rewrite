//! barrier_survey <installed world dir> [top N] : where can a car leave the free-roam collision? (user 2026-10-08: "some
//! barriers have no clipping, you go through and fall out of the map"). Free roam = triangles whose routes have bit 15
//! (0x8000); there is no terrain collision off the road network, so every edge of the free-roam ground must be fenced by
//! a free-roam wall. For each boundary edge of the active ground (an edge no other active ground triangle shares) it looks
//! 1.5 m outwards:
//!   - active ground there (within 3 m up/down): not an edge, skip;
//!   - an active wall within 2 m of the edge (0.3-2.5 m up): fenced, ok;
//!   - otherwise a GAP, classified as: `inactive_ground` (an event-route road continues: the car drives onto it and falls
//!     through), `inactive_wall` (only an event-route barrier is there: closed for races, open in free roam), or `void`.
//! Gaps are summed per 25 m cell and printed longest first with engine coordinates for FH1_TELEPORT (x, y, z engine = x,
//! y, -z collision). Also: the route bits of the inactive walls / ground at gaps (which event bits fence free roam).
use std::collections::{BTreeMap, HashMap};

const FREE: u16 = 0x8000;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let world = fh1_world::World::load(std::path::Path::new(&a[1])).expect("world dir");
    let top: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    // Optional: the installed scenery/colorado dir, to name the props standing at paved gaps (template ids).
    let props = a.get(3).map(|d| load_props(std::path::Path::new(d))).unwrap_or_default();
    let paved_name = |n: &str| ["Asphalt", "Asphault", "Concrete", "Brick", "Rumble", "Trackway"].iter().any(|k| n.contains(k)) && !n.contains("Barrier");
    let mut paved_cells: HashMap<(i32, i32), (f32, [f32; 3], BTreeMap<u32, f32>)> = HashMap::new();
    let mut paved_templates: BTreeMap<u32, f32> = BTreeMap::new();
    let t0 = std::time::Instant::now();
    let normal = |ti: u32| {
        let p = world.tri_points(ti);
        let u = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let v = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12);
        [n[0] / l, n[1] / l, n[2] / l]
    };
    let is_ground = |ti: u32| normal(ti)[1].abs() >= 0.5;
    // Boundary edges of the active ground.
    let mut edges: HashMap<u64, (u32, u32)> = HashMap::new(); // key -> (count, a triangle)
    let mut active_ground = 0usize;
    for (ti, t) in world.tris.iter().enumerate() {
        if t.routes & FREE == 0 || !is_ground(ti as u32) {
            continue;
        }
        active_ground += 1;
        for k in 0..3 {
            let (x, y) = (t.v[k], t.v[(k + 1) % 3]);
            let key = ((x.min(y) as u64) << 32) | x.max(y) as u64;
            let e = edges.entry(key).or_insert((0, ti as u32));
            e.0 += 1;
        }
    }
    let boundary: Vec<(u64, u32)> = edges.iter().filter(|(_, v)| v.0 == 1).map(|(k, v)| (*k, v.1)).collect();
    eprintln!("{} triangles, {active_ground} active ground, {} boundary edges ({:.1} s)", world.tris.len(), boundary.len(), t0.elapsed().as_secs_f32());

    let mut contacts = Vec::new();
    // cell -> (gap metres by class [inactive_ground, inactive_wall, void], a sample point, route bits seen)
    let mut cells: HashMap<(i32, i32), ([f32; 3], [f32; 3], u16)> = HashMap::new();
    let mut bits_wall: BTreeMap<u16, f32> = BTreeMap::new();
    let mut surf: BTreeMap<String, f32> = BTreeMap::new();
    let mut bits_ground: BTreeMap<u16, f32> = BTreeMap::new();
    let (mut fenced, mut skipped, mut total) = (0.0f32, 0.0f32, [0.0f32; 3]);
    for (key, ti) in boundary {
        let (ia, ib) = ((key >> 32) as u32, (key & 0xFFFF_FFFF) as u32);
        let (pa, pb) = (world.verts[ia as usize], world.verts[ib as usize]);
        let len = ((pb[0] - pa[0]).powi(2) + (pb[2] - pa[2]).powi(2)).sqrt();
        if len < 0.2 {
            continue;
        }
        let m = [(pa[0] + pb[0]) * 0.5, (pa[1] + pb[1]) * 0.5, (pa[2] + pb[2]) * 0.5];
        // Outward = horizontal perpendicular to the edge, away from the triangle's centroid.
        let tp = world.tri_points(ti);
        let c = [(tp[0][0] + tp[1][0] + tp[2][0]) / 3.0, (tp[0][2] + tp[1][2] + tp[2][2]) / 3.0];
        let (dx, dz) = ((pb[0] - pa[0]) / len, (pb[2] - pa[2]) / len);
        let mut out = [dz, -dx];
        if (m[0] - c[0]) * out[0] + (m[2] - c[1]) * out[1] < 0.0 {
            out = [-out[0], -out[1]];
        }
        let q = [m[0] + out[0] * 1.5, m[1], m[2] + out[1] * 1.5];
        // Ground beyond (any route) within 3 m up / down?
        let mut beyond: Option<u16> = None;
        let mut origin = [q[0], q[1] + 3.0, q[2]];
        // Any ground below (a drop onto a lower road / terrain is not a way out of the map).
        let mut left = 200.0f32;
        while let Some(h) = world.raycast(origin, [0.0, -1.0, 0.0], left) {
            if is_ground(h.tri) {
                beyond = Some(world.tris[h.tri as usize].routes);
                break;
            }
            let step = h.t + 0.01;
            origin[1] -= step;
            left -= step;
            if left <= 0.0 {
                break;
            }
        }
        let below_far = beyond.is_some_and(|r| r & FREE != 0);
        if below_far {
            skipped += len;
            continue;
        }
        // Walls within 6 m outwards of the edge, 0.3-2.5 m up (the boundary walls can stand a few metres out).
        let (mut active_wall, mut inactive_bits) = (false, 0u16);
        for (dy, o) in [0.4f32, 1.2, 2.2].into_iter().flat_map(|dy| [0.5f32, 2.5, 4.5].map(move |o| (dy, o))) {
            world.sphere_contacts([m[0] + out[0] * o, m[1] + dy, m[2] + out[1] * o], 2.0, &mut contacts);
            for ct in &contacts {
                if normal(ct.tri)[1].abs() >= 0.5 {
                    continue;
                }
                let r = world.tris[ct.tri as usize].routes;
                if r & FREE != 0 {
                    active_wall = true;
                } else {
                    inactive_bits |= r;
                }
            }
            if active_wall {
                break;
            }
        }
        if active_wall {
            fenced += len;
            continue;
        }
        let class = match beyond {
            Some(r) => {
                *bits_ground.entry(r).or_default() += len;
                0
            }
            None if inactive_bits != 0 => {
                *bits_wall.entry(inactive_bits).or_default() += len;
                1
            }
            None => 2,
        };
        total[class] += len;
        let sname = world.surface(world.tris[ti as usize].surface).map(|s| s.name.clone()).unwrap_or_default();
        *surf.entry(sname.clone()).or_default() += len;
        if paved_name(&sname) {
            let cell = ((m[0] / 25.0).floor() as i32, (m[2] / 25.0).floor() as i32);
            let e = paved_cells.entry(cell).or_insert((0.0, m, BTreeMap::new()));
            e.0 += len;
            // Props within 3 m of the gap (and its outside), any height within 3 m.
            for q in [m, [m[0] + out[0] * 1.5, m[1], m[2] + out[1] * 1.5]] {
                let key = ((q[0] / 8.0).floor() as i32, (q[2] / 8.0).floor() as i32);
                for dx in -1..=1 {
                    for dz in -1..=1 {
                        for &(t, p) in props.get(&(key.0 + dx, key.1 + dz)).into_iter().flatten() {
                            if (p[0] - q[0]).hypot(p[2] - q[2]) < 3.0 && (p[1] - q[1]).abs() < 3.0 {
                                *e.2.entry(t).or_default() += len * 0.5;
                                *paved_templates.entry(t).or_default() += len * 0.5;
                            }
                        }
                    }
                }
            }
        }
        let cell = ((m[0] / 25.0).floor() as i32, (m[2] / 25.0).floor() as i32);
        let e = cells.entry(cell).or_insert(([0.0; 3], m, 0));
        e.0[class] += len;
        e.2 |= inactive_bits | beyond.unwrap_or(0);
    }
    eprintln!(
        "edge metres: fenced {fenced:.0}, ground continues {skipped:.0}; GAPS: inactive ground {:.0}, inactive wall only {:.0}, void {:.0} ({:.1} s)",
        total[0], total[1], total[2], t0.elapsed().as_secs_f32()
    );
    let fmt = |m: &BTreeMap<u16, f32>| m.iter().filter(|(_, v)| **v > 20.0).map(|(k, v)| format!("{k:#06x}:{v:.0}")).collect::<Vec<_>>().join(" ");
    eprintln!("route bits at inactive-wall gaps (m): {}", fmt(&bits_wall));
    eprintln!("ground surface at gap edges (m): {}", surf.iter().filter(|(_, v)| **v > 50.0).map(|(k, v)| format!("{k}:{v:.0}")).collect::<Vec<_>>().join(" "));
    eprintln!("route bits of inactive ground at gaps (m): {}", fmt(&bits_ground));
    let mut list: Vec<_> = cells.into_iter().collect();
    list.sort_by(|a, b| (b.1 .0.iter().sum::<f32>()).total_cmp(&a.1 .0.iter().sum::<f32>()));
    println!("rank\tgap_m\tinactive_ground_m\tinactive_wall_m\tvoid_m\troutes\tteleport(engine x,y,z)");
    for (r, (_, (g, p, bits))) in list.iter().take(top).enumerate() {
        println!("{r}\t{:.0}\t{:.0}\t{:.0}\t{:.0}\t{bits:#06x}\t{:.1},{:.1},{:.1}", g.iter().sum::<f32>(), g[0], g[1], g[2], p[0], p[1] + 1.0, -p[2]);
    }
    let paved: f32 = paved_cells.values().map(|c| c.0).sum();
    println!("
PAVED gaps (road edges with no free-roam wall): {paved:.0} m in {} cells", paved_cells.len());
    let mut pt: Vec<_> = paved_templates.into_iter().collect();
    pt.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!("templates standing at paved gaps (template: gap m): {}", pt.iter().take(40).map(|(t, m)| format!("{t}:{m:.0}")).collect::<Vec<_>>().join(" "));
    let mut pl: Vec<_> = paved_cells.into_iter().collect();
    pl.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));
    println!("rank	paved_gap_m	teleport(engine x,y,z)	props(template:m)");
    for (r, (_, (g, p, t))) in pl.iter().take(top).enumerate() {
        let tl = t.iter().map(|(k, v)| format!("{k}:{v:.0}")).collect::<Vec<_>>().join(",");
        println!("{r}	{g:.0}	{:.1},{:.1},{:.1}	{tl}", p[0], p[1] + 1.0, -p[2]);
    }
}

/// Prop placements (template id, collision-space position) on an 8 m grid, from `props/tiles/*.bin` (FH1PROP1/2/3).
fn load_props(dir: &std::path::Path) -> HashMap<(i32, i32), Vec<(u32, [f32; 3])>> {
    let mut grid: HashMap<(i32, i32), Vec<(u32, [f32; 3])>> = HashMap::new();
    let Ok(rd) = std::fs::read_dir(dir.join("props/tiles")) else { return grid };
    for e in rd.flatten() {
        let Ok(b) = std::fs::read(e.path()) else { continue };
        let rec = match b.get(..8) {
            Some(b"FH1PROP3") => 92,
            Some(b"FH1PROP2") => 84,
            Some(b"FH1PROP1") => 68,
            _ => continue,
        };
        let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        for i in 0..n {
            let o = 12 + i * rec;
            if o + rec > b.len() {
                break;
            }
            let t = u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            let f = |k: usize| f32::from_le_bytes(b[o + 4 + k * 4..o + 8 + k * 4].try_into().unwrap());
            let p = [f(12), f(13), -f(14)];
            grid.entry(((p[0] / 8.0).floor() as i32, (p[2] / 8.0).floor() as i32)).or_default().push((t, p));
        }
    }
    grid
}
