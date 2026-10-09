//! barn_reach <installed world dir> <installed scenery/colorado dir> <missions.json> [radius m] : can a car drive from the paved
//! road network to each of the 9 barn finds in free roam? (user 2026-10-09: "barn finds are inaccessible, in the original parts of
//! fences / gates are breakable").
//!
//! Per barn a 0.5 m grid flood fill (engine space) grows from every drivable cell within the find zone (radius + 4 m reach, 8 m height
//! band, barn.rs) over free-roam ground (route bit 0x8000, step <= 0.45 m per 0.5 m) until it touches a paved surface or the search
//! radius (default 300 m) runs out. A step is blocked by a free-roam `.fiz` wall (steep triangle within 0.5 m of a 0.9 m high point)
//! and, by mode, by prop colliders as smash.rs builds them (wall props `solid[].wall`, tree trunks, rocks; smashables with a break speed
//! are passable, 9999 = never breaks = solid). Modes: `all` (what the game builds today), `nowallprops` (our name-based fence / wall
//! boxes removed: only `.fiz` walls + trunks + rocks), `fiz` (only `.fiz` walls). Blockers met at the frontier are counted per template.
use std::collections::{HashMap, HashSet, VecDeque};

const FREE: u16 = 0x8000;
const WALL_MIN_HALF: f32 = 0.2;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    WallProp,
    Trunk,
    Rock,
    Smash,
    /// The generated invisible wall along a paved road edge (smash.rs edge_walls).
    Edge,
}

struct P {
    kind: Kind,
    template: u32,
    centre: [f32; 3],
    axes: [[f32; 3]; 3],
    half: [f32; 3],
    never: bool,
    /// A wall prop standing on ground with ground on both sides and no .fiz wall behind it (smash.rs ree_standing).
    free: bool,
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let world = fh1_world::World::load(std::path::Path::new(&a[1])).expect("world");
    let dir = std::path::Path::new(&a[2]);
    let missions: serde_json::Value = serde_json::from_slice(&std::fs::read(&a[3]).unwrap()).unwrap();
    let radius: f32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(300.0);
    let idx: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
    let col = &idx["props"]["collision"];
    let v3 = |v: &serde_json::Value| [v[0].as_f64().unwrap() as f32, v[1].as_f64().unwrap() as f32, v[2].as_f64().unwrap() as f32];
    let bounds: HashMap<u32, ([f32; 3], [f32; 3])> = col["bounds"].as_object().unwrap().iter().map(|(k, v)| (k.parse().unwrap(), (v3(&v[0]), v3(&v[1])))).collect();
    let solid: HashMap<u32, (bool, bool)> = col["solid"].as_array().unwrap().iter().map(|x| (x["n"].as_u64().unwrap() as u32, (x["trunk"].as_bool().unwrap_or(false), x["wall"].as_bool().unwrap_or(false)))).collect();
    let smash: HashMap<u32, (f32, String)> = col["smash"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["n"].as_u64().unwrap() as u32, (s["phys"]["break_mph"].as_f64().unwrap_or(0.0) as f32, s["type"].as_str().unwrap_or("").to_owned())))
        .collect();
    let mut names: HashMap<u32, String> = HashMap::new();
    if let Ok(t) = std::fs::read_to_string(std::env::var("FH1_TEMPLATE_NAMES").unwrap_or_default()) {
        for l in t.lines() {
            let p: Vec<&str> = l.split('\t').collect();
            if let (Some(n), Some(nm)) = (p.first().and_then(|n| n.parse::<u32>().ok()), p.get(1)) {
                names.insert(n, (*nm).to_owned());
            }
        }
    }
    let name = |t: u32| names.get(&t).cloned().or_else(|| smash.get(&t).map(|s| s.1.clone())).unwrap_or_default();

    // Every prop collider within reach of any barn.
    let barns: Vec<(String, [f32; 3])> = missions["barn_finds"].as_array().unwrap().iter().map(|b| (b["name"].as_str().unwrap().to_owned(), v3(&b["pos"]))).collect();
    let mut props: Vec<P> = Vec::new();
    for t in idx["props"]["tiles"].as_array().unwrap() {
        let (tx, tz) = (t["x"].as_i64().unwrap() as f32 * 256.0, t["z"].as_i64().unwrap() as f32 * 256.0);
        if !barns.iter().any(|(_, p)| p[0] > tx - radius - 300.0 && p[0] < tx + 256.0 + radius + 300.0 && p[2] > tz - radius - 300.0 && p[2] < tz + 256.0 + radius + 300.0) {
            continue;
        }
        let b = std::fs::read(dir.join(t["file"].as_str().unwrap())).unwrap();
        let stride = match &b[..8] {
            b"FH1PROP3" => 92,
            b"FH1PROP2" => 84,
            _ => 68,
        };
        let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        for i in 0..n {
            let o = 12 + i * stride;
            let template = u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            let is_smash = smash.contains_key(&template);
            let sol = solid.get(&template).copied();
            if !is_smash && sol.is_none() {
                continue;
            }
            let Some(&(lo, hi)) = bounds.get(&template) else { continue };
            let f: Vec<f32> = (0..16).map(|k| f32::from_le_bytes(b[o + 4 + k * 4..o + 8 + k * 4].try_into().unwrap())).collect();
            // Columns of the placement matrix: scale * rotation axes.
            let sc = [0, 1, 2].map(|c| (f[c * 4] * f[c * 4] + f[c * 4 + 1] * f[c * 4 + 1] + f[c * 4 + 2] * f[c * 4 + 2]).sqrt());
            let axes = [0, 1, 2].map(|c| [f[c * 4] / sc[c].max(1e-6), f[c * 4 + 1] / sc[c].max(1e-6), f[c * 4 + 2] / sc[c].max(1e-6)]);
            let lc = [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5, (lo[2] + hi[2]) * 0.5];
            let centre = [0, 1, 2].map(|k| f[12 + k] + f[k] * lc[0] + f[4 + k] * lc[1] + f[8 + k] * lc[2]);
            let mut half = [0, 1, 2].map(|c| (hi[c] - lo[c]) * 0.5 * sc[c]);
            let (kind, never) = if is_smash {
                let mph = smash[&template].0;
                (Kind::Smash, mph >= 9000.0)
            } else if sol.unwrap().1 {
                half[0] = half[0].max(WALL_MIN_HALF);
                half[2] = half[2].max(WALL_MIN_HALF);
                (Kind::WallProp, true)
            } else if sol.unwrap().0 {
                let r = (0.03 * half[1] * 2.0).clamp(0.15, 0.45);
                half = [r, half[1], r];
                (Kind::Trunk, true)
            } else {
                half = half.map(|h| h * 0.85);
                (Kind::Rock, true)
            };
            if kind == Kind::Trunk {
                // vertical cylinder at the placement origin
                props.push(P { kind, template, centre: [f[12], f[13] + half[1], f[14]], axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], half, never, free: false });
            } else {
                props.push(P { kind, template, centre, axes, half, never, free: false });
            }
        }
    }
    for p in props.iter_mut().filter(|p| p.kind == Kind::WallProp) {
        p.free = free_standing(&world, p.centre, p.axes, p.half);
    }
    let n_edge = add_edge_walls(&world, &barns, radius + 20.0, &mut props);
    println!("{n_edge} generated edge walls near barns");
    println!("{} candidate prop colliders near barns ({} free-standing wall props), radius {radius} m", props.len(), props.iter().filter(|p| p.free).count());
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, p) in props.iter().enumerate() {
        let r = p.half.iter().map(|h| h * h).sum::<f32>().sqrt();
        for x in ((p.centre[0] - r) / 8.0).floor() as i32..=((p.centre[0] + r) / 8.0).floor() as i32 {
            for z in ((p.centre[2] - r) / 8.0).floor() as i32..=((p.centre[2] + r) / 8.0).floor() as i32 {
                grid.entry((x, z)).or_default().push(i);
            }
        }
    }

    let clear: f32 = std::env::var("FH1_CLEAR").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let road = |s: u8| world.surface(s).is_some_and(|s| ["asphalt", "asphault"].iter().any(|k| s.name.to_ascii_lowercase().contains(k)));
    let steep = |tri: u32| {
        let p = world.tri_points(tri);
        let u = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let v = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        n[1].abs() / (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12) < 0.5
    };
    // Ground under (x, z) (engine space) at most `down` below y: free-roam ground only.
    let ground = |x: f32, y: f32, z: f32, down: f32, up: f32| -> Option<(f32, u8)> {
        let mut oy = y + up;
        for _ in 0..4 {
            let h = world.raycast([x, oy, -z], [0.0, -1.0, 0.0], up + down)?;
            if h.point[1] < y - down {
                return None;
            }
            let t = &world.tris[h.tri as usize];
            if t.routes & FREE != 0 && !steep(h.tri) {
                return Some((h.point[1], t.surface));
            }
            oy = h.point[1] - 0.05;
        }
        None
    };
    let mut contacts = Vec::new();
    let mut blocked_by = |x: f32, y: f32, z: f32, mode: u8, props: &Vec<P>, hit_log: &mut Option<&mut HashMap<String, usize>>| -> bool {
        // .fiz walls
        world.sphere_contacts([x, y + 0.9, -z], clear, &mut contacts);
        if mode == 3 {
            return false;
        }
        if let Some(c) = contacts.iter().find(|c| world.tris[c.tri as usize].routes & FREE != 0 && steep(c.tri)) {
            if let Some(l) = hit_log {
                let t = &world.tris[c.tri as usize];
                let sname = world.surface(t.surface).map_or("?", |s| s.name.as_str());
                *l.entry(format!("fiz wall surface {sname} routes {:#06x} flags {}", t.routes, t.flags)).or_default() += 1;
            }
            return true;
        }
        if mode == 2 {
            return false;
        }
        if let Some(list) = grid.get(&((x / 8.0).floor() as i32, (z / 8.0).floor() as i32)) {
            for &i in list {
                let p = &props[i];
                let solid = match p.kind {
                    Kind::WallProp => mode == 4 || (mode == 0 && !p.free),
                    Kind::Trunk | Kind::Rock => true,
                    Kind::Smash => p.never,
                    Kind::Edge => mode != 1 || true,
                };
                if !solid {
                    continue;
                }
                let c = [x - p.centre[0], y + 0.7 - p.centre[1], z - p.centre[2]];
                let l: [f32; 3] = std::array::from_fn(|k| c[0] * p.axes[k][0] + c[1] * p.axes[k][1] + c[2] * p.axes[k][2]);
                let d: [f32; 3] = std::array::from_fn(|k| (l[k].abs() - p.half[k]).max(0.0));
                if d.iter().map(|v| v * v).sum::<f32>() < (clear * 0.9) * (clear * 0.9) {
                    if let Some(lg) = hit_log {
                        *lg.entry(format!("{:?} {} {}", p.kind, p.template, name(p.template))).or_default() += 1;
                    }
                    return true;
                }
            }
        }
        false
    };

    for (bname, pos) in &barns {
        let mut line = format!("{bname} ({:.0}, {:.0}, {:.0}):", pos[0], pos[1], pos[2]);
        let mut top: Vec<(String, usize)> = Vec::new();
        let mut seed_surf: HashMap<String, usize> = HashMap::new();
        for (mi, mode) in ["now", "nowallprops", "fiz", "nowalls", "before"].iter().enumerate() {
            let step = 0.5f32;
            let key = |x: f32, z: f32| ((x / step).round() as i32, (z / step).round() as i32);
            let mut seen: HashSet<(i32, i32, i32)> = HashSet::new();
            let mut q: VecDeque<(f32, f32, f32)> = VecDeque::new();
            // seeds: cells within the find zone (10 + 4 m)
            let mut zone = 0usize;
            let mut sx = pos[0] - 14.0;
            while sx <= pos[0] + 14.0 {
                let mut sz = pos[2] - 14.0;
                while sz <= pos[2] + 14.0 {
                    if (sx - pos[0]).hypot(sz - pos[2]) <= 14.0 {
                        if let Some((gy, sf)) = ground(sx, pos[1] + 8.0, sz, 16.0, 1.0).filter(|g| (g.0 - pos[1]).abs() <= 8.0) {
                            if mi == 0 {
                                *seed_surf.entry(world.surface(sf).map_or("?".to_owned(), |s| s.name.clone())).or_default() += 1;
                            }
                            if !blocked_by(sx, gy, sz, mi as u8, &props, &mut None) && seen.insert((key(sx, sz).0, key(sx, sz).1, (gy / 0.5).round() as i32)) {
                                q.push_back((sx, gy, sz));
                                zone += 1;
                            }
                        }
                    }
                    sz += step;
                }
                sx += step;
            }
            let mut log: HashMap<String, usize> = HashMap::new();
            let mut best_road: Option<f32> = None;
            let mut far = 0.0f32;
            let mut cells = 0usize;
            while let Some((x, y, z)) = q.pop_front() {
                cells += 1;
                let dist = (x - pos[0]).hypot(z - pos[2]);
                far = far.max(dist);
                if let Some((_, s)) = ground(x, y, z, 0.3, 0.3) {
                    if road(s) && std::env::var_os("FH1_FAR_ONLY").is_none() {
                        best_road = Some(best_road.map_or(dist, |b: f32| b.min(dist)));
                        break;
                    }
                }
                for (dx, dz) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                    let (nx, nz) = (x + dx * step, z + dz * step);
                    if (nx - pos[0]).hypot(nz - pos[2]) > radius {
                        continue;
                    }
                    let Some((ny, _)) = ground(nx, y, nz, 0.6, 0.6) else { continue };
                    if (ny - y).abs() > 0.45 {
                        continue;
                    }
                    let k = key(nx, nz);
                    if seen.contains(&(k.0, k.1, (ny / 0.5).round() as i32)) {
                        continue;
                    }
                    if blocked_by(nx, ny, nz, mi as u8, &props, &mut Some(&mut log)) {
                        continue;
                    }
                    seen.insert((k.0, k.1, (ny / 0.5).round() as i32));
                    q.push_back((nx, ny, nz));
                }
            }
            line += &format!(" [{mode}: zone {zone}, {} , cells {cells}, far {far:.0} m]", if let Some(d) = best_road { format!("road at {d:.0} m") } else if far >= radius - 1.0 { "open ground (reached the search radius)".to_owned() } else { "ENCLOSED".to_owned() });
            if mi == 0 {
                let mut v: Vec<_> = log.into_iter().collect();
                v.sort_by_key(|x| std::cmp::Reverse(x.1));
                v.truncate(8);
                top = v;
            }
        }
        println!("{line}\n      zone ground: {seed_surf:?}");
        for (k, n) in top {
            println!("      blocked {n}x by {k}");
        }
    }
}

/// Mirror of fh1-engine smash.rs `free_standing` (engine space in, collision z mirrored): a wall prop with no free-roam `.fiz` wall
/// behind it and free-roam ground 2 m beyond both faces at three points along it.
fn free_standing(world: &fh1_world::World, centre: [f32; 3], axes: [[f32; 3]; 3], half: [f32; 3]) -> bool {
    let flat = |v: [f32; 3]| {
        let l = v[0].hypot(v[2]);
        if l < 1e-6 { None } else { Some([v[0] / l, v[2] / l]) }
    };
    let (long, thin, hl, ht) = if half[0] >= half[2] { (flat(axes[0]), flat(axes[2]), half[0], half[2]) } else { (flat(axes[2]), flat(axes[0]), half[2], half[0]) };
    let (Some(long), Some(thin)) = (long, thin) else { return false };
    let base = centre[1] - half[1];
    let ny = |ti: u32| {
        let p = world.tri_points(ti);
        let u = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let v = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        n[1].abs() / (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12)
    };
    let ground_at = |x: f32, z: f32| {
        let mut o = [x, base + 2.5, -z];
        let mut left = 4.5f32;
        while left > 0.0 {
            let Some(h) = world.raycast(o, [0.0, -1.0, 0.0], left) else { return false };
            if world.tris[h.tri as usize].routes & FREE != 0 && ny(h.tri) >= 0.5 {
                return (h.point[1] - base).abs() <= 2.0;
            }
            let step = h.t + 0.01;
            o[1] -= step;
            left -= step;
        }
        false
    };
    let mut contacts = Vec::new();
    for f in [-0.7f32, 0.0, 0.7] {
        let (px, pz) = (centre[0] + long[0] * hl * f, centre[2] + long[1] * hl * f);
        world.sphere_contacts([px, base + 0.8, -pz], 1.5, &mut contacts);
        if contacts.iter().any(|c| world.tris[c.tri as usize].routes & FREE != 0 && ny(c.tri) < 0.5) {
            return false;
        }
        for s in [1.0f32, -1.0] {
            if !ground_at(px + thin[0] * (ht + 2.0) * s, pz + thin[1] * (ht + 2.0) * s) {
                return false;
            }
        }
    }
    true
}
/// Port of fh1-engine smash.rs `edge_walls` for the boxes within `reach` of a barn: pushes `Kind::Edge` colliders (engine space).
fn add_edge_walls(world: &fh1_world::World, barns: &[(String, [f32; 3])], reach: f32, props: &mut Vec<P>) -> usize {
    let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let ny = |ti: u32| {
        let p = world.tri_points(ti);
        let (u, v) = (sub(p[1], p[0]), sub(p[2], p[0]));
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        n[1].abs() / (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12)
    };
    let paved: Vec<bool> = world.surfaces.iter().map(|s| ["Asphalt", "Asphault", "Concrete", "Brick", "Rumble", "Trackway"].iter().any(|k| s.name.contains(k)) && !s.name.contains("Barrier")).collect();
    let mut edges: HashMap<u64, (u32, u32)> = HashMap::new();
    for (ti, t) in world.tris.iter().enumerate() {
        if t.routes & FREE == 0 || ny(ti as u32) < 0.5 {
            continue;
        }
        for k in 0..3 {
            let (x, y) = (t.v[k], t.v[(k + 1) % 3]);
            edges.entry(((x.min(y) as u64) << 32) | x.max(y) as u64).or_insert((0, ti as u32)).0 += 1;
        }
    }
    let ground_below = |q: [f32; 3], depth: f32| {
        let mut o = [q[0], q[1] + 3.0, q[2]];
        let mut left = depth + 3.0;
        while left > 0.0 {
            let Some(h) = world.raycast(o, [0.0, -1.0, 0.0], left) else { return false };
            if ny(h.tri) >= 0.5 && world.tris[h.tri as usize].routes != 0 {
                return true;
            }
            let step = h.t + 0.01;
            o[1] -= step;
            left -= step;
        }
        false
    };
    let mut contacts = Vec::new();
    let mut n = 0;
    for (key, (count, ti)) in edges {
        if count != 1 || !paved.get(world.tris[ti as usize].surface as usize).copied().unwrap_or(false) {
            continue;
        }
        let (pa, pb) = (world.verts[(key >> 32) as usize], world.verts[(key & 0xFFFF_FFFF) as usize]);
        let m = [(pa[0] + pb[0]) * 0.5, (pa[1] + pb[1]) * 0.5, (pa[2] + pb[2]) * 0.5];
        // engine z = -collision z
        if !barns.iter().any(|(_, b)| (m[0] - b[0]).hypot(-m[2] - b[2]) < reach) {
            continue;
        }
        let len = (pb[0] - pa[0]).hypot(pb[2] - pa[2]);
        if len < 0.2 {
            continue;
        }
        let along = [(pb[0] - pa[0]) / len, 0.0, (pb[2] - pa[2]) / len];
        let tp = world.tri_points(ti);
        let cen = [(tp[0][0] + tp[1][0] + tp[2][0]) / 3.0, 0.0, (tp[0][2] + tp[1][2] + tp[2][2]) / 3.0];
        let mut out = [along[2], 0.0, -along[0]];
        if (m[0] - cen[0]) * out[0] + (m[2] - cen[2]) * out[2] < 0.0 {
            out = [-out[0], 0.0, -out[2]];
        }
        if [1.5f32, 4.0].iter().any(|&d| ground_below([m[0] + out[0] * d, m[1], m[2] + out[2] * d], 10.0)) {
            continue;
        }
        let mut fenced = false;
        'probe: for dy in [0.4f32, 1.2, 2.2] {
            for o in [0.5f32, 2.5, 4.5] {
                world.sphere_contacts([m[0] + out[0] * o, m[1] + dy, m[2] + out[2] * o], 2.0, &mut contacts);
                if contacts.iter().any(|c| ny(c.tri) < 0.5 && world.tris[c.tri as usize].routes & FREE != 0) {
                    fenced = true;
                    break 'probe;
                }
            }
        }
        if fenced {
            continue;
        }
        let rise = (pb[1] - pa[1]).abs();
        let c = [m[0] + out[0] * 0.25, m[1] + 1.0 + 0.5 * rise, m[2] + out[2] * 0.25];
        props.push(P {
            kind: Kind::Edge,
            template: 0,
            centre: [c[0], c[1], -c[2]],
            axes: [[along[0], 0.0, -along[2]], [0.0, 1.0, 0.0], [out[0], 0.0, -out[2]]],
            half: [0.5 * len + 0.15, 1.2 + 0.5 * rise, 0.2],
            never: true,
            free: false,
        });
        n += 1;
    }
    n
}