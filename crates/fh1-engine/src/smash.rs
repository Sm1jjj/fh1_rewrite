//! Prop collision and smashables (docs/SMASH.md).
//!
//! Walls, fences and barriers are already in the track's `.fiz` collision. The rest of the props are not:
//! - **smashables** (the `CollObjs.xml` objects: signs, benches, bins, cones, fences, outhouses...; the
//!   `GameObjs.xml` flyers and speed cameras): an oriented box from the whole-object template. Hit faster than
//!   [`BREAK_SPEED`] they break: the whole mesh goes, its shard templates fly off as simple rigid bodies, and the car
//!   loses the momentum the object takes. Slower, they push back like a solid.
//! - **solid** trees (vertical trunk cylinders) and rocks (boxes). Which props the game makes solid, the trunk
//!   radius and the object masses are UNVERIFIED (the game's own shapes are in the undecoded
//!   `PhysicsDefinitions.bin`).
//!
//! The car sees props through [`PropGround`], a [`Ground`] wrapping the track's: main.rs builds one per physics tick.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use crate::scenery::Scenery;
use crate::track::Track;
use fh1_engine::vehicle::{Ground, GroundHit, SphereContact, Vehicle};

/// Impact speed (m/s, along the contact normal) above which a smashable breaks.
const BREAK_SPEED: f32 = 2.5;
/// Collider grid cell (m).
const CELL: f32 = 16.0;
/// Smashable mass per m^3 of its bounding box, and the clamp (kg). UNVERIFIED.
const DENSITY: f32 = 120.0;
const MASS_RANGE: (f32, f32) = (4.0, 300.0);
/// Debris lifetime (s).
const DEBRIS_LIFE: f32 = 20.0;
/// Surface id reported for prop contacts.
const PROP_SURFACE: u8 = 0;

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// Oriented box: centre, unit axes, half extents.
    Box { centre: Vec3, axes: Mat3, half: Vec3 },
    /// Vertical cylinder: base centre, height, radius.
    Trunk { base: Vec3, height: f32, radius: f32 },
    Sphere { centre: Vec3, radius: f32 },
}

/// A smashable's game shapes (PhysicsDefinitions.bin via setup `phys`, template space) and break speed.
struct GameBody {
    shapes: Vec<Shape>,
    /// m/s; `None` = never breaks (9999 mph).
    break_speed: Option<f32>,
}

#[derive(Clone)]
struct Collider {
    /// Usually one; the game's shape list for smashables with physics data.
    shapes: Vec<Shape>,
    /// Impact speed (m/s) above which a smashable breaks; `f32::INFINITY` = never.
    break_speed: f32,
    /// Prop placement key: (tile x, tile z, index in the tile file).
    key: (i32, i32, u32),
    template: u16,
    /// World transform of the placement (for the shards).
    matrix: Mat4,
    /// Some(mass) for smashables.
    mass: Option<f32>,
}

/// Every prop collider, in a grid.
#[derive(Default)]
struct Colliders {
    list: Vec<Collider>,
    grid: HashMap<(i32, i32), Vec<u32>>,
    /// Smashable whole template -> shard templates.
    shards: HashMap<u16, Vec<u16>>,
    /// Template -> local bounds.
    bounds: HashMap<u16, (Vec3, Vec3)>,
}

#[derive(Resource, Default)]
pub struct PropCollision {
    colliders: Option<Colliders>,
    loading: Option<Task<Option<Colliders>>>,
    broken: HashSet<u32>,
    /// Smashed this tick (collider, car velocity at impact), consumed by [`update`].
    pending: Vec<(u32, Vec3)>,
    /// Time spent in prop sphere queries (ns) and their count, for `FH1_SMASH_STATS=1`.
    query_ns: std::sync::atomic::AtomicU64,
    queries: std::sync::atomic::AtomicU64,
}

impl PropCollision {
    /// Starts loading the colliders from the installed scenery (`index.json` `props.collision` + the placement tiles).
    pub fn start(&mut self, dir: &Path, world: Option<std::sync::Arc<fh1_engine::world::WorldGround>>) {
        let dir = dir.to_path_buf();
        self.loading = Some(AsyncComputeTaskPool::get().spawn(async move {
            let mut cols = load(&dir);
            // Edge walls work without a prop table too.
            if let Some(w) = world.filter(|_| edge_walls_enabled()) {
                let t0 = std::time::Instant::now();
                let walls = edge_walls(&w);
                let c = cols.get_or_insert_with(Colliders::default);
                let n = walls.len();
                for (k, shape) in walls.into_iter().enumerate() {
                    c.push_solid(shape, (i32::MIN, 0, k as u32));
                }
                info!("smash: {n} edge walls along unfenced paved road edges ({:.0} ms)", t0.elapsed().as_secs_f32() * 1000.0);
            }
            cols
        }));
    }

    fn near(&self, c: Vec3, r: f32) -> impl Iterator<Item = u32> + '_ {
        let cols = self.colliders.as_ref();
        let (x0, x1) = (((c.x - r) / CELL).floor() as i32, ((c.x + r) / CELL).floor() as i32);
        let (z0, z1) = (((c.z - r) / CELL).floor() as i32, ((c.z + r) / CELL).floor() as i32);
        (x0..=x1)
            .flat_map(move |x| (z0..=z1).map(move |z| (x, z)))
            .filter_map(move |k| cols.and_then(|c| c.grid.get(&k)))
            .flatten()
            .copied()
            .filter(|i| !self.broken.contains(i))
    }

    /// After the car's physics substeps: the smashed objects take their share of the car's momentum.
    pub fn apply_hits(&mut self, car: &mut Vehicle, hits: Vec<u32>) {
        let Some(cols) = self.colliders.as_ref() else { return };
        for i in hits {
            if !self.broken.insert(i) {
                continue;
            }
            let m = cols.list[i as usize].mass.unwrap_or(0.0);
            let v = car.velocity;
            // Perfectly inelastic share of the momentum, horizontal only (UNVERIFIED stand-in for the game's response).
            let share = m / (car.data.mass + m);
            car.velocity -= Vec3::new(v.x, 0.0, v.z) * share;
            info!(
                "smash: broke template {} ({m:.0} kg) at {:.1} m/s, car {:.1} -> {:.1} m/s",
                cols.list[i as usize].template,
                v.length(),
                v.length(),
                car.velocity.length()
            );
            self.pending.push((i, v));
        }
    }
}

/// Race event objects (race.rs, R1): solid boxes for the barriers / signs a race places, removed when it ends.
impl PropCollision {
    /// Whether the free-roam colliders are loaded (race objects are added on top of them).
    pub fn ready(&self) -> bool {
        self.colliders.is_some()
    }

    /// Adds a solid box per object: (template, placement matrix, template-space bounds). Never breaks (the race
    /// objects are drawn outside the props path, so a smash couldn't remove them). Returns the ids for [`Self::remove`].
    pub fn add_solid_boxes(&mut self, objects: &[(u16, Mat4, Vec3, Vec3)]) -> Vec<u32> {
        let Some(cols) = self.colliders.as_mut() else { return Vec::new() };
        let mut ids = Vec::with_capacity(objects.len());
        for (k, &(template, m, lo, hi)) in objects.iter().enumerate() {
            let (scale, rot, _) = m.to_scale_rotation_translation();
            let half = (hi - lo) * scale.abs() * 0.5;
            let centre = m.transform_point3((lo + hi) * 0.5);
            let id = cols.list.len() as u32;
            let r = half.length();
            let (x0, x1) = (((centre.x - r) / CELL).floor() as i32, ((centre.x + r) / CELL).floor() as i32);
            let (z0, z1) = (((centre.z - r) / CELL).floor() as i32, ((centre.z + r) / CELL).floor() as i32);
            for x in x0..=x1 {
                for z in z0..=z1 {
                    cols.grid.entry((x, z)).or_default().push(id);
                }
            }
            cols.list.push(Collider {
                shapes: vec![Shape::Box { centre, axes: Mat3::from_quat(rot), half }],
                break_speed: f32::INFINITY,
                key: (i32::MIN, i32::MIN, k as u32),
                template,
                matrix: m,
                mass: None,
            });
            ids.push(id);
        }
        ids
    }

    /// Removes colliders added by [`Self::add_solid_boxes`].
    pub fn remove(&mut self, ids: &[u32]) {
        let Some(cols) = self.colliders.as_mut() else { return };
        let gone: HashSet<u32> = ids.iter().copied().collect();
        for list in cols.grid.values_mut() {
            list.retain(|i| !gone.contains(i));
        }
    }
}

impl Colliders {
    /// A never-breaking collider (edge walls) registered in the grid.
    fn push_solid(&mut self, shape: Shape, key: (i32, i32, u32)) {
        let id = self.list.len() as u32;
        let (c, r) = match shape {
            Shape::Box { centre, half, .. } => (centre, half.length()),
            Shape::Trunk { base, radius, .. } => (base, radius),
            Shape::Sphere { centre, radius } => (centre, radius),
        };
        let (x0, x1) = (((c.x - r) / CELL).floor() as i32, ((c.x + r) / CELL).floor() as i32);
        let (z0, z1) = (((c.z - r) / CELL).floor() as i32, ((c.z + r) / CELL).floor() as i32);
        for x in x0..=x1 {
            for z in z0..=z1 {
                self.grid.entry((x, z)).or_default().push(id);
            }
        }
        self.list.push(Collider { shapes: vec![shape], break_speed: f32::INFINITY, key, template: u16::MAX, matrix: Mat4::IDENTITY, mass: None });
    }
}

/// `FH1_EDGE_WALLS=0`: no generated edge walls.
fn edge_walls_enabled() -> bool {
    std::env::var("FH1_EDGE_WALLS").map_or(true, |v| v != "0")
}

/// Invisible walls along the free-roam road edges you could otherwise drive off into the void (2026-10-08, user: "barriers
/// you can go through and fall out of the map"; survey crates/fh1-world/examples/barrier_survey.rs: 10.3 km of paved edges
/// with no wall). The free-roam collision has no terrain off the road network, so an edge of the active ground (an edge no
/// other active ground triangle shares) on a PAVED surface gets a wall when, 1.5 m and 4 m outwards, there's no active
/// ground within 10 m below (car parks and verges that continue onto other ground stay open) and no active wall within
/// 6 m (the game's own boundary walls). OUR rule (the game's fall-out prevention there isn't traced). Engine space.
fn edge_walls(w: &fh1_engine::world::WorldGround) -> Vec<Shape> {
    const FREE: u16 = fh1_engine::world::FREE_ROAM;
    let world = &w.world;
    let normal = |ti: u32| {
        let p = world.tri_points(ti);
        let (a, b, c) = (Vec3::from(p[0]), Vec3::from(p[1]), Vec3::from(p[2]));
        (b - a).cross(c - a).normalize_or_zero()
    };
    let is_ground = |ti: u32| normal(ti).y.abs() >= 0.5;
    let paved: Vec<bool> = world
        .surfaces
        .iter()
        .map(|s| ["Asphalt", "Asphault", "Concrete", "Brick", "Rumble", "Trackway"].iter().any(|k| s.name.contains(k)) && !s.name.contains("Barrier"))
        .collect();
    let mut edges: HashMap<u64, (u32, u32)> = HashMap::new();
    for (ti, t) in world.tris.iter().enumerate() {
        if t.routes & FREE == 0 || !is_ground(ti as u32) {
            continue;
        }
        for k in 0..3 {
            let (x, y) = (t.v[k], t.v[(k + 1) % 3]);
            edges.entry(((x.min(y) as u64) << 32) | x.max(y) as u64).or_insert((0, ti as u32)).0 += 1;
        }
    }
    // Active ground straight below `q` within `depth` (collision space), stepping past walls / inactive triangles.
    let ground_below = |q: [f32; 3], depth: f32| {
        let mut o = [q[0], q[1] + 3.0, q[2]];
        let mut left = depth + 3.0;
        while left > 0.0 {
            let Some(h) = world.raycast(o, [0.0, -1.0, 0.0], left) else { return false };
            if is_ground(h.tri) && world.tris[h.tri as usize].routes & FREE != 0 {
                return true;
            }
            let step = h.t + 0.01;
            o[1] -= step;
            left -= step;
        }
        false
    };
    let mut contacts = Vec::new();
    let mut out_shapes = Vec::new();
    for (key, (count, ti)) in edges {
        if count != 1 || !paved.get(world.tris[ti as usize].surface as usize).copied().unwrap_or(false) {
            continue;
        }
        let (pa, pb) = (Vec3::from(world.verts[(key >> 32) as usize]), Vec3::from(world.verts[(key & 0xFFFF_FFFF) as usize]));
        let flat = Vec3::new(pb.x - pa.x, 0.0, pb.z - pa.z);
        let len = flat.length();
        if len < 0.2 {
            continue;
        }
        let along = flat / len;
        let m = (pa + pb) * 0.5;
        let tp = world.tri_points(ti);
        let centroid = (Vec3::from(tp[0]) + Vec3::from(tp[1]) + Vec3::from(tp[2])) / 3.0;
        let mut out = Vec3::new(along.z, 0.0, -along.x);
        if (m - centroid).dot(out) < 0.0 {
            out = -out;
        }
        if [1.5f32, 4.0].iter().any(|&d| ground_below((m + out * d).to_array(), 10.0)) {
            continue;
        }
        let mut fenced = false;
        'probe: for dy in [0.4f32, 1.2, 2.2] {
            for o in [0.5f32, 2.5, 4.5] {
                world.sphere_contacts((m + out * o + Vec3::Y * dy).to_array(), 2.0, &mut contacts);
                if contacts.iter().any(|c| normal(c.tri).y.abs() < 0.5 && world.tris[c.tri as usize].routes & FREE != 0) {
                    fenced = true;
                    break 'probe;
                }
            }
        }
        if fenced {
            continue;
        }
        // A 2.4 m high, 0.4 m thick wall just outside the edge, slightly longer than it (neighbours overlap).
        let rise = (pb.y - pa.y).abs();
        let centre = m + out * 0.25 + Vec3::Y * (1.0 + 0.5 * rise);
        let half = Vec3::new(0.5 * len + 0.15, 1.2 + 0.5 * rise, 0.2);
        // Collision space -> engine space (z mirrored); the box is symmetric, so the basis handedness doesn't matter.
        let e = |v: Vec3| if fh1_engine::world::MIRROR_Z { Vec3::new(v.x, v.y, -v.z) } else { v };
        out_shapes.push(Shape::Box { centre: e(centre), axes: Mat3::from_cols(e(along), Vec3::Y, e(out)), half });
    }
    out_shapes
}

/// Minimum half thickness of a wall collider (m): fences are 0.1-0.2 m thick, the car's contact spheres ~0.3 m.
const WALL_MIN_HALF: f32 = 0.2;

fn load(dir: &Path) -> Option<Colliders> {
    let idx: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).ok()?).ok()?;
    let col = &idx["props"]["collision"];
    let v3 = |v: &serde_json::Value| Some(Vec3::new(v[0].as_f64()? as f32, v[1].as_f64()? as f32, v[2].as_f64()? as f32));
    let bounds: HashMap<u16, (Vec3, Vec3)> = col["bounds"]
        .as_object()?
        .iter()
        .filter_map(|(k, v)| Some((k.parse().ok()?, (v3(&v[0])?, v3(&v[1])?))))
        .collect();
    let shards: HashMap<u16, Vec<u16>> = col["smash"]
        .as_array()?
        .iter()
        .filter_map(|s| Some((s["n"].as_u64()? as u16, s["shards"].as_array()?.iter().filter_map(|x| x.as_u64().map(|x| x as u16)).collect())))
        .collect();
    // Solid template -> drawn as a trunk cylinder (trees) or a box (rocks).
    let solid: HashMap<u16, bool> = col["solid"]
        .as_array()?
        .iter()
        .filter_map(|x| Some((x["n"].as_u64()? as u16, x["trunk"].as_bool().unwrap_or(false))))
        .collect();
    // Walls (setup `"wall": true`, 2026-10-08: fences, walls, barriers with no `.fiz` wall behind them, user: "barriers
    // you can drive through and fall out of the map"): a full-size box (segments must join without gaps), at least
    // WALL_MIN_HALF thick so the car's small contact spheres can't tunnel through a 0.1 m fence. FH1_PROP_WALLS=0 = off.
    let walls: HashSet<u16> = if std::env::var("FH1_PROP_WALLS").is_ok_and(|v| v == "0") {
        HashSet::new()
    } else {
        col["solid"].as_array()?.iter().filter(|x| x["wall"].as_bool() == Some(true)).filter_map(|x| x["n"].as_u64().map(|n| n as u16)).collect()
    };
    // The game's shapes and break speeds per smashable template (docs/SMASH.md "Physics definitions").
    let f = |v: &serde_json::Value| v.as_f64().map(|x| x as f32);
    let bodies: HashMap<u16, GameBody> = col["smash"]
        .as_array()?
        .iter()
        .filter(|s| s["phys"].is_object() && std::env::var("FH1_GAME_SHAPES").as_deref() != Ok("0"))
        .filter_map(|s| {
            let p = &s["phys"];
            let mut shapes: Vec<Shape> = p["spheres"].as_array()?.iter().filter_map(|v| Some(Shape::Sphere { centre: v3(v)?, radius: f(&v[3])? })).collect();
            shapes.extend(p["boxes"].as_array()?.iter().filter_map(|b| {
                let a = &b["axes"];
                Some(Shape::Box { centre: v3(&b["c"])?, axes: Mat3::from_cols(v3(&a[0])?, v3(&a[1])?, v3(&a[2])?), half: v3(&b["half"])? })
            }));
            let mph = f(&p["break_mph"])?;
            // 0 on many parents: breaks into its parts on contact (GUESSED); keep the stand-in threshold for those.
            let break_speed = (mph < 9000.0).then(|| if mph > 0.0 { mph * 0.44704 } else { BREAK_SPEED });
            let n = s["n"].as_u64()? as u16;
            (!shapes.is_empty()).then_some((n, GameBody { shapes, break_speed }))
        })
        .collect();
    let mut out = Colliders { shards, bounds, ..default() };
    for t in idx["props"]["tiles"].as_array()? {
        let (tx, tz) = (t["x"].as_i64()? as i32, t["z"].as_i64()? as i32);
        let b = std::fs::read(dir.join(t["file"].as_str()?)).ok()?;
        // Same record layout as scenery::read_placements: FH1PROP1 = 68 bytes, FH1PROP2 adds normal + tint (84),
        // FH1PROP3 the LOD0 / LOD1 lightmaps (92).
        let stride = match b.get(..8)? {
            b"FH1PROP3" => 92,
            b"FH1PROP2" => 84,
            b"FH1PROP1" => 68,
            _ => continue,
        };
        let n = u32::from_le_bytes(b.get(8..12)?.try_into().ok()?) as usize;
        if b.len() < 12 + n * stride {
            continue;
        }
        for i in 0..n {
            let o = 12 + i * stride;
            let template = u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u16;
            let smash = out.shards.contains_key(&template);
            if !smash && !solid.contains_key(&template) {
                continue;
            }
            let Some(&(lo, hi)) = out.bounds.get(&template) else { continue };
            let m = Mat4::from_cols_array(&std::array::from_fn(|k| f32::from_le_bytes(b[o + 4 + k * 4..o + 8 + k * 4].try_into().unwrap())));
            let (scale, rot, pos) = m.to_scale_rotation_translation();
            let scale = scale.abs();
            let size = (hi - lo) * scale;
            let shape = if !smash && walls.contains(&template) {
                let mut half = size * 0.5;
                half.x = half.x.max(WALL_MIN_HALF);
                half.z = half.z.max(WALL_MIN_HALF);
                Shape::Box { centre: m.transform_point3((lo + hi) * 0.5), axes: Mat3::from_quat(rot), half }
            } else if !smash && solid.get(&template).copied().unwrap_or(false) {
                // Trunk: thin relative to the canopy (UNVERIFIED radius).
                Shape::Trunk { base: pos + rot * (Vec3::new(0.0, lo.y, 0.0) * scale), height: size.y, radius: (0.03 * size.y).clamp(0.15, 0.45) }
            } else {
                let shrink = if smash { 1.0 } else { 0.85 };
                Shape::Box { centre: m.transform_point3((lo + hi) * 0.5), axes: Mat3::from_quat(rot), half: size * 0.5 * shrink }
            };
            let mut mass = smash.then(|| (size.x * size.y * size.z * DENSITY).clamp(MASS_RANGE.0, MASS_RANGE.1));
            let mut break_speed = BREAK_SPEED;
            let mut shapes = vec![shape];
            if let Some(body) = bodies.get(&template) {
                // Template space -> world: the placement matrix (rotation and scale).
                shapes = body.shapes.iter().map(|s| world_shape(s, m, rot, scale)).collect();
                match body.break_speed {
                    Some(v) => break_speed = v,
                    // Never breaks (barn doors, race activation): solid.
                    None => mass = None,
                }
            }
            if !smash {
                break_speed = f32::INFINITY;
            }
            let id = out.list.len() as u32;
            let (c, r) = match shape {
                Shape::Box { centre, half, .. } => (centre, half.length()),
                Shape::Trunk { base, radius, .. } => (base, radius),
                Shape::Sphere { centre, radius } => (centre, radius),
            };
            let (x0, x1) = (((c.x - r) / CELL).floor() as i32, ((c.x + r) / CELL).floor() as i32);
            let (z0, z1) = (((c.z - r) / CELL).floor() as i32, ((c.z + r) / CELL).floor() as i32);
            for x in x0..=x1 {
                for z in z0..=z1 {
                    out.grid.entry((x, z)).or_default().push(id);
                }
            }
            out.list.push(Collider { shapes, break_speed, key: (tx, tz, i as u32), template, matrix: m, mass });
        }
    }
    Some(out)
}

/// A template-space shape placed by the placement matrix `m` (= translation * rotation * scale).
fn world_shape(s: &Shape, m: Mat4, rot: Quat, scale: Vec3) -> Shape {
    match *s {
        Shape::Box { centre, axes, half } => Shape::Box { centre: m.transform_point3(centre), axes: Mat3::from_quat(rot) * axes, half: half * scale },
        Shape::Sphere { centre, radius } => Shape::Sphere { centre: m.transform_point3(centre), radius: radius * scale.max_element() },
        t @ Shape::Trunk { .. } => t,
    }
}

/// Sphere vs shapes: the deepest contact.
fn sphere_vs_all(shapes: &[Shape], c: Vec3, r: f32) -> Option<(Vec3, Vec3, f32)> {
    shapes.iter().filter_map(|s| sphere_vs(s, c, r)).max_by(|a, b| a.2.total_cmp(&b.2))
}

/// Sphere vs shape: (contact point, push-out normal, depth).
fn sphere_vs(shape: &Shape, c: Vec3, r: f32) -> Option<(Vec3, Vec3, f32)> {
    match *shape {
        Shape::Sphere { centre, radius } => {
            let d = c - centre;
            let dist = d.length();
            if dist >= r + radius || dist < 1e-5 {
                return None;
            }
            let n = d / dist;
            Some((centre + n * radius, n, r + radius - dist))
        }
        Shape::Box { centre, axes, half } => {
            let local = axes.transpose() * (c - centre);
            let q = local.clamp(-half, half);
            let d = local - q;
            let dist = d.length();
            if dist >= r {
                return None;
            }
            if dist > 1e-5 {
                return Some((centre + axes * q, axes * (d / dist), r - dist));
            }
            // Centre inside: out along the least-penetrated axis.
            let pen = half - local.abs();
            let (k, depth) = [pen.x, pen.y, pen.z].into_iter().enumerate().min_by(|a, b| a.1.total_cmp(&b.1))?;
            let mut n = Vec3::ZERO;
            n[k] = local[k].signum();
            Some((c, axes * n, depth + r))
        }
        Shape::Trunk { base, height, radius } => {
            if c.y < base.y - r || c.y > base.y + height {
                return None;
            }
            let d = Vec2::new(c.x - base.x, c.z - base.z);
            let dist = d.length();
            if dist >= r + radius || dist < 1e-5 {
                return None;
            }
            let n = Vec3::new(d.x / dist, 0.0, d.y / dist);
            Some((Vec3::new(base.x, c.y, base.z) + n * radius, n, r + radius - dist))
        }
    }
}

/// The track's ground plus the props, for one physics tick.
pub struct PropGround<'a> {
    world: &'a dyn Ground,
    props: Option<&'a PropCollision>,
    /// Car velocity at the start of the tick (decides break vs push).
    velocity: Vec3,
    hits: RefCell<Vec<u32>>,
}

impl<'a> PropGround<'a> {
    pub fn new(world: &'a dyn Ground, props: Option<&'a PropCollision>, car: &Vehicle) -> Self {
        Self { world, props, velocity: car.velocity, hits: RefCell::new(Vec::new()) }
    }

    /// Smashables broken during the tick.
    pub fn take_hits(&self) -> Vec<u32> {
        std::mem::take(&mut self.hits.borrow_mut())
    }
}

impl Ground for PropGround<'_> {
    fn ray(&self, origin: Vec3, dir: Vec3, max: f32) -> Option<GroundHit> {
        self.world.ray(origin, dir, max)
    }

    fn sphere(&self, center: Vec3, radius: f32, out: &mut Vec<SphereContact>) {
        self.world.sphere(center, radius, out);
        let Some(props) = self.props else { return };
        let Some(cols) = props.colliders.as_ref() else { return };
        let started = std::time::Instant::now();
        let _timer = Timer(&props.query_ns, &props.queries, started);
        self.prop_contacts(props, cols, center, center, radius, out);
    }

    /// The track's swept test, plus the props at a few points along the path when the sphere moved far (a thin box
    /// can't be stepped over below ~240 m/s at 480 Hz, so this is only a guard for very fast cars).
    fn sphere_sweep(&self, from: Vec3, to: Vec3, radius: f32, out: &mut Vec<SphereContact>) {
        self.world.sphere_sweep(from, to, radius, out);
        let Some(props) = self.props else { return };
        let Some(cols) = props.colliders.as_ref() else { return };
        let started = std::time::Instant::now();
        let _timer = Timer(&props.query_ns, &props.queries, started);
        let moved = from.distance(to);
        let inner = if moved > 0.25 { ((moved / 0.2).ceil() as usize - 1).min(6) } else { 0 };
        for k in 1..=inner {
            let at = from.lerp(to, k as f32 / (inner + 1) as f32);
            self.prop_contacts(props, cols, at, to, radius, out);
        }
        self.prop_contacts(props, cols, to, to, radius, out);
    }
}

impl PropGround<'_> {
    /// Prop contacts of the sphere at `at`, expressed for the sphere's end position `end` (the depth that pushes the
    /// sphere at `end` out to where it touches the shape; contacts that `end` has already moved clear of are dropped).
    fn prop_contacts(&self, props: &PropCollision, cols: &Colliders, at: Vec3, end: Vec3, radius: f32, out: &mut Vec<SphereContact>) {
        for i in props.near(at, radius) {
            let c = &cols.list[i as usize];
            let Some((point, normal, depth)) = sphere_vs_all(&c.shapes, at, radius) else { continue };
            let depth = depth - (end - at).dot(normal);
            if depth <= 0.0 {
                continue;
            }
            if c.mass.is_some() {
                if self.hits.borrow().contains(&i) {
                    continue;
                }
                if (-self.velocity.dot(normal)).max(Vec2::new(self.velocity.x, self.velocity.z).length() * 0.5) > c.break_speed {
                    self.hits.borrow_mut().push(i);
                    continue;
                }
            }
            out.push(SphereContact { point, normal, depth, surface: PROP_SURFACE });
        }
    }
}

/// Adds the elapsed time to the query counters on drop.
struct Timer<'a>(&'a std::sync::atomic::AtomicU64, &'a std::sync::atomic::AtomicU64, std::time::Instant);

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.0.fetch_add(self.2.elapsed().as_nanos() as u64, Relaxed);
        self.1.fetch_add(1, Relaxed);
    }
}

/// A flying shard.
#[derive(Component)]
pub struct Debris {
    velocity: Vec3,
    spin: Vec3,
    age: f32,
    resting: bool,
}

/// Loads the colliders once the scenery exists; turns smash hits into debris; moves the debris.
pub fn update(
    mut commands: Commands,
    mut props: ResMut<PropCollision>,
    scenery: Option<ResMut<Scenery>>,
    track: Res<Track>,
    time: Res<Time>,
    mut debris: Query<(Entity, &mut Debris, &mut Transform)>,
    // World generation the colliders were started for (X1c: a map change restarts them for the new scenery).
    mut started: Local<Option<u32>>,
    generation: Res<crate::ui::world_load::WorldGeneration>,
) {
    let Some(mut sc) = scenery else { return };
    if *started != Some(generation.0) {
        *started = Some(generation.0);
        // FH1_PROP_COLLISION=0: no prop colliders (A/B timing, debugging).
        if std::env::var("FH1_PROP_COLLISION").as_deref() != Ok("0") {
            props.start(sc.dir(), track.world.clone());
        } else {
            sc.set_smashable(Default::default());
        }
    }
    if props.loading.as_ref().is_some_and(|t| t.is_finished()) {
        let task = props.loading.take().unwrap();
        props.colliders = block_on(future::poll_once(task)).flatten();
        // Remaster prop merging keeps smashables as entities (fh1_remaster::batch::PropMerge).
        sc.set_smashable(props.colliders.as_ref().map(|c| c.list.iter().filter(|c| c.mass.is_some()).map(|c| c.template).collect()).unwrap_or_default());
        if let Some(c) = &props.colliders {
            let smash = c.list.iter().filter(|c| c.mass.is_some()).count();
            info!("smash: {} prop colliders ({smash} smashable, {} solid)", c.list.len(), c.list.len() - smash);
        } else {
            warn!("smash: no prop collision table installed (re-run fh1setup scenery)");
        }
    }
    if std::env::var_os("FH1_SMASH_STATS").is_some() && time.elapsed_secs_f64() % 2.0 < time.delta_secs_f64() {
        use std::sync::atomic::Ordering::Relaxed;
        let (ns, n) = (props.query_ns.swap(0, Relaxed), props.queries.swap(0, Relaxed));
        info!("smash stats: {n} prop sphere queries in the last ~2 s, {:.3} ms total ({:.2} us each)", ns as f64 / 1e6, ns as f64 / 1e3 / n.max(1) as f64);
    }
    let hits = std::mem::take(&mut props.pending);
    if let Some(cols) = props.colliders.as_ref() {
        let mut rng = fastrand_seed(time.elapsed_secs_f64());
        for (i, v) in hits {
            let c = &cols.list[i as usize];
            sc.break_prop(&mut commands, c.key);
            let (scale, rot, pos) = c.matrix.to_scale_rotation_translation();
            for &s in cols.shards.get(&c.template).into_iter().flatten() {
                let Some(&(lo, hi)) = cols.bounds.get(&s) else { continue };
                let pivot = (lo + hi) * 0.5;
                let root = commands
                    .spawn((
                        Transform { translation: pos + rot * (pivot * scale), rotation: rot, scale },
                        Visibility::default(),
                        crate::ui::world_load::WorldEntity,
                        Debris {
                            velocity: Vec3::new(v.x, 0.0, v.z) * (0.6 + 0.4 * rng()) + Vec3::new(rng() - 0.5, 0.0, rng() - 0.5) * 4.0 + Vec3::Y * (2.0 + 3.0 * rng()),
                            spin: Vec3::new(rng() - 0.5, rng() - 0.5, rng() - 0.5) * 12.0,
                            age: 0.0,
                            resting: false,
                        },
                    ))
                    .id();
                if !sc.spawn_template(&mut commands, s, Transform::from_translation(-pivot), root) {
                    commands.entity(root).despawn();
                }
            }
        }
    }
    // Debris: gravity, bounce on the track ground, settle, expire.
    let dt = time.delta_secs().min(0.05);
    for (e, mut d, mut t) in &mut debris {
        d.age += dt;
        if d.age > DEBRIS_LIFE {
            commands.entity(e).despawn();
            continue;
        }
        if d.resting {
            continue;
        }
        d.velocity.y -= 9.81 * dt;
        t.translation += d.velocity * dt;
        let spin = d.spin;
        t.rotation = (Quat::from_scaled_axis(spin * dt) * t.rotation).normalize();
        if let Some(hit) = track.ground.ray(t.translation + Vec3::Y * 2.0, Vec3::NEG_Y, 4.0) {
            let floor = hit.point.y + 0.1;
            if t.translation.y < floor {
                t.translation.y = floor;
                d.velocity.y = -d.velocity.y * 0.3;
                d.velocity.x *= 0.6;
                d.velocity.z *= 0.6;
                d.spin *= 0.6;
                if d.velocity.length() < 0.4 {
                    d.resting = true;
                }
            }
        } else if d.age > 3.0 {
            // Off the collision mesh (open terrain has none): let it go.
            commands.entity(e).despawn();
        }
    }
}

/// Small xorshift for debris scatter (no extra dependency).
fn fastrand_seed(seed: f64) -> impl FnMut() -> f32 {
    let mut s = (seed.to_bits() ^ 0x9E37_79B9_7F4A_7C15) | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Test hook: `FH1_SMASH_TEST=smash|tree|rock|<template>` (+ `FH1_SMASH_INDEX=k` for the k-th candidate,
/// `FH1_SMASH_SPEED` m/s, default 15) picks a prop of that kind standing on the track mesh, holds the car 25 m from
/// it facing it while the scenery streams in (8 s), then launches it straight at the prop. With
/// `FH1_SMASH_FRAMES=<dir>` it saves a frame every 0.12 s for 2.4 s after the launch, then exits.
#[allow(clippy::too_many_arguments)]
pub fn test_drive(
    mut commands: Commands,
    props: Res<PropCollision>,
    track: Res<Track>,
    time: Res<Time<Real>>,
    mut cars: Query<&mut crate::Car>,
    mut state: Local<Option<(Vec3, f32, Vec3, f32)>>,
    mut frames: Local<u32>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(kind) = std::env::var("FH1_SMASH_TEST") else { return };
    let Some(cols) = props.colliders.as_ref() else { return };
    let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    if state.is_none() {
        let Ok(car) = cars.single() else { return };
        let here = car.0.position;
        let want = |c: &Collider| match kind.as_str() {
            "smash" => c.mass.is_some(),
            "tree" => matches!(c.shapes[0], Shape::Trunk { .. }),
            "rock" => c.mass.is_none() && matches!(c.shapes[0], Shape::Box { .. }),
            n => n.parse::<u16>().is_ok_and(|n| n == c.template),
        };
        let centre = |c: &Collider| match c.shapes[0] {
            Shape::Box { centre, .. } | Shape::Sphere { centre, .. } => centre,
            Shape::Trunk { base, .. } => base,
        };
        let on_mesh = |p: Vec3| track.ground.ray(p + Vec3::Y * 3.0, Vec3::NEG_Y, 8.0).filter(|h| (h.point.y - p.y).abs() < 2.5);
        let mut found: Vec<(f32, Vec3, Vec3, f32)> = Vec::new();
        for c in cols.list.iter().filter(|c| want(c)) {
            let p = centre(c);
            let Some(base) = on_mesh(Vec3::new(p.x, p.y + 1.0, p.z)) else { continue };
            // Not against a wall of the track mesh (rails, fences): the car would stop there first.
            let mut walls = Vec::new();
            track.ground.sphere(base.point + Vec3::Y * 1.0, 1.2, &mut walls);
            if walls.iter().any(|w| w.normal.y.abs() < 0.6) {
                continue;
            }
            // An approach 25 m out over drivable ground, roughly level.
            for k in 0..16 {
                let a = k as f32 * std::f32::consts::TAU / 16.0;
                let d = Vec3::new(a.sin(), 0.0, a.cos());
                let from = base.point - d * 25.0;
                let ok = (0..=5).all(|s| on_mesh(base.point - d * (5.0 * s as f32)).is_some_and(|h| (h.point.y - base.point.y).abs() < 3.0 + s as f32))
                    && (1..=9).all(|s| {
                        // No track-mesh wall (rail, fence) on the way in.
                        let mut w = Vec::new();
                        track.ground.sphere(base.point - d * (2.5 * s as f32) + Vec3::Y * 0.8, 1.0, &mut w);
                        w.iter().all(|w| w.normal.y.abs() >= 0.6)
                    });
                if ok {
                    let yaw = (-d.x).atan2(-d.z);
                    found.push((from.distance(here), on_mesh(from).unwrap().point, base.point, yaw));
                    break;
                }
            }
        }
        found.sort_by(|a, b| a.0.total_cmp(&b.0));
        let k = env("FH1_SMASH_INDEX", 0.0) as usize;
        let Some(&(_, from, target, yaw)) = found.get(k) else {
            warn!("smash test: no {kind} candidate");
            return;
        };
        info!("smash test: {kind} #{k} of {}: car at {from}, target {target}", found.len());
        *state = Some((from, yaw, target, time.elapsed_secs()));
    }
    let (from, yaw, target, t0) = state.unwrap();
    let t = time.elapsed_secs() - t0;
    let Ok(mut car) = cars.single_mut() else { return };
    if t < 8.0 {
        car.0.place(from, yaw);
        return;
    }
    if *frames == 0 {
        let dir = (target - from).with_y(0.0).normalize_or_zero();
        car.0.velocity = dir * env("FH1_SMASH_SPEED", 15.0);
        *frames = 1;
    }
    let Ok(out) = std::env::var("FH1_SMASH_FRAMES") else { return };
    let n = ((t - 8.0) / 0.12) as u32 + 1;
    if n >= *frames && *frames <= 20 {
        let path = std::path::PathBuf::from(&out).join(format!("f{:02}.png", *frames));
        commands.spawn(bevy::render::view::screenshot::Screenshot::primary_window()).observe(bevy::render::view::screenshot::save_to_disk(path));
        *frames += 1;
    } else if *frames > 20 && t > 8.0 + 20.0 * 0.12 + 1.5 {
        exit.write(AppExit::Success);
    }
}
