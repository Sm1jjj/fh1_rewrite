//! The whole track's collision as one welded triangle mesh, with a uniform XZ grid for queries.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use fh1_formats::zip::Archive;

use crate::col::{self, FizMesh};
use crate::surface::{self, Surface};

/// Grid cell edge for the query grid, metres.
const CELL: f32 = 10.0;
const FILE_MAGIC: &[u8; 8] = b"FH1WCOL1";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tri {
    pub v: [u32; 3],
    /// Index into [`World::surfaces`].
    pub surface: u8,
    pub flags: u8,
    pub routes: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct Hit {
    /// Distance along the ray, in units of the ray direction's length.
    pub t: f32,
    pub point: [f32; 3],
    /// Unit face normal, flipped to face the ray origin.
    pub normal: [f32; 3],
    pub surface: u8,
    pub tri: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Contact {
    /// Closest point on the triangle.
    pub point: [f32; 3],
    /// Unit direction to push the sphere out along.
    pub normal: [f32; 3],
    /// How far the sphere overlaps the triangle (> 0).
    pub depth: f32,
    pub surface: u8,
    pub tri: u32,
}

/// Uniform XZ grid in CSR form: triangles overlapping cell `c` are `items[start[c]..start[c + 1]]`.
#[derive(Debug, Clone, PartialEq)]
struct CellGrid {
    min: [f32; 2],
    dims: [u32; 2],
    start: Vec<u32>,
    items: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct World {
    pub verts: Vec<[f32; 3]>,
    pub tris: Vec<Tri>,
    pub surfaces: Vec<Surface>,
    grid: CellGrid,
}

/// What [`World::from_disc`] read, for logging.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadStats {
    pub squares: usize,
    pub raw_verts: usize,
    pub raw_tris: usize,
    /// Triangles dropped because a neighbouring square already had them (shared cushion).
    pub duplicate_tris: usize,
    /// Zero-area triangles dropped.
    pub degenerate_tris: usize,
}

impl World {
    /// Reads a track's collision and the surface table straight from an extracted disc.
    /// `track` is the folder under `media/tracks` (e.g. `colorado`).
    pub fn from_disc(disc: &Path, track: &str) -> Result<(World, LoadStats)> {
        let track_dir = disc.join("media/tracks").join(track);
        let col_path = col::find_col(&track_dir)?;
        Self::from_col(disc, track, &col_path)
    }

    /// [`World::from_disc`] with an explicit `.col` (FM4: one per layout, `Ribbon_NN/<track>_track_NN.col`).
    pub fn from_col(disc: &Path, track: &str, col_path: &Path) -> Result<(World, LoadStats)> {
        let track_dir = disc.join("media/tracks").join(track);
        let col_bytes = std::fs::read(col_path).with_context(|| col_path.display().to_string())?;
        let grid = col::parse_col(&col_bytes).with_context(|| col_path.display().to_string())?;
        // FM4: the squares' meshes are inside the .col.
        if grid.squares.iter().all(|sq| sq.embedded.is_some()) {
            let mut meshes = Vec::with_capacity(grid.squares.len());
            for sq in &grid.squares {
                let (off, size) = sq.embedded.unwrap();
                let b = col_bytes.get(off as usize..(off + size) as usize).context("col (FM4): square past the end")?;
                meshes.push(col::parse_fm4_square(b).with_context(|| format!("{} square {}", col_path.display(), sq.array_id))?);
            }
            let surfaces = surface::parse_surface_types(&surface_types_xml(disc)?)?;
            let mut stats = LoadStats { squares: meshes.len(), ..Default::default() };
            let world = Self::from_meshes(&meshes, surfaces, &mut stats);
            return Ok((world, stats));
        }

        let mut bin = Archive::open(track_dir.join("bin.zip")).context("opening the track's bin.zip")?;
        // bin.zip repeats shared files for streaming; every copy is identical, so keep the first.
        let mut by_name = HashMap::new();
        for e in &bin.entries {
            by_name.entry(e.name.to_ascii_lowercase()).or_insert_with(|| e.clone());
        }
        let mut meshes = Vec::with_capacity(grid.squares.len());
        for sq in &grid.squares {
            let name = format!("{}.fiz", sq.array_id);
            let entry = by_name.get(&name).with_context(|| format!("{name} missing from bin.zip"))?;
            let bytes = bin.read(entry).with_context(|| name.clone())?;
            meshes.push(col::parse_fiz(&bytes).with_context(|| name.clone())?);
        }

        let surfaces = surface::parse_surface_types(&surface_types_xml(disc)?)?;

        let mut stats = LoadStats { squares: meshes.len(), ..Default::default() };
        let world = Self::from_meshes(&meshes, surfaces, &mut stats);
        Ok((world, stats))
    }

    /// Welds the squares into one mesh (vertices by exact position, triangles by vertex set)
    /// and builds the query grid.
    pub fn from_meshes(meshes: &[FizMesh], surfaces: Vec<Surface>, stats: &mut LoadStats) -> World {
        let mut verts = Vec::new();
        let mut vmap: HashMap<[u32; 3], u32> = HashMap::new();
        let mut tris = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for m in meshes {
            stats.raw_verts += m.verts.len();
            stats.raw_tris += m.polys.len();
            let remap: Vec<u32> = m
                .verts
                .iter()
                .map(|p| {
                    *vmap.entry(p.map(f32::to_bits)).or_insert_with(|| {
                        verts.push(*p);
                        verts.len() as u32 - 1
                    })
                })
                .collect();
            for p in &m.polys {
                let v = p.v.map(|i| remap[i as usize]);
                if v[0] == v[1] || v[1] == v[2] || v[0] == v[2] {
                    stats.degenerate_tris += 1;
                    continue;
                }
                let mut key = v;
                key.sort_unstable();
                if !seen.insert(key) {
                    stats.duplicate_tris += 1;
                    continue;
                }
                tris.push(Tri { v, surface: p.surface, flags: p.flags, routes: p.routes });
            }
        }
        let grid = build_grid(&verts, &tris);
        World { verts, tris, surfaces, grid }
    }

    pub fn surface(&self, id: u8) -> Option<&Surface> {
        self.surfaces.get(id as usize)
    }

    /// World-space bounds (min, max).
    pub fn bounds(&self) -> ([f32; 3], [f32; 3]) {
        let mut lo = [f32::MAX; 3];
        let mut hi = [f32::MIN; 3];
        for p in &self.verts {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        (lo, hi)
    }

    /// First triangle hit by the segment `origin + dir * t`, `t` in `[0, max_t]` (both faces
    /// count). Cost grows with the segment's XZ footprint, so keep rays short (suspension,
    /// camera) or vertical (`height_at`).
    pub fn raycast(&self, origin: [f32; 3], dir: [f32; 3], max_t: f32) -> Option<Hit> {
        let end = add(origin, scale(dir, max_t));
        let mut best: Option<Hit> = None;
        self.for_tris_in(
            [origin[0].min(end[0]), origin[2].min(end[2])],
            [origin[0].max(end[0]), origin[2].max(end[2])],
            |ti| {
                let limit = best.map_or(max_t, |h| h.t);
                if let Some(t) = self.ray_tri(ti, origin, dir, limit) {
                    let [a, b, c] = self.tri_points(ti);
                    let mut n = normalize(cross(sub(b, a), sub(c, a)));
                    if dot(n, dir) > 0.0 {
                        n = scale(n, -1.0);
                    }
                    best = Some(Hit { t, point: add(origin, scale(dir, t)), normal: n, surface: self.tris[ti as usize].surface, tri: ti });
                }
            },
        );
        best
    }

    /// Highest collision surface at (x, z), if any.
    pub fn height_at(&self, x: f32, z: f32) -> Option<Hit> {
        self.raycast([x, 10_000.0, z], [0.0, -1.0, 0.0], 20_000.0)
    }

    /// Every triangle overlapping the sphere, appended to `out` (cleared first).
    pub fn sphere_contacts(&self, center: [f32; 3], radius: f32, out: &mut Vec<Contact>) {
        out.clear();
        self.for_tris_in([center[0] - radius, center[2] - radius], [center[0] + radius, center[2] + radius], |ti| {
            if out.iter().any(|c| c.tri == ti) {
                return; // a triangle spanning several cells is visited once per cell
            }
            let [a, b, c] = self.tri_points(ti);
            let q = closest_on_tri(center, a, b, c);
            let d = sub(center, q);
            let dist2 = dot(d, d);
            if dist2 >= radius * radius {
                return;
            }
            let dist = dist2.sqrt();
            let normal = if dist > 1e-6 {
                scale(d, 1.0 / dist)
            } else {
                normalize(cross(sub(b, a), sub(c, a)))
            };
            out.push(Contact { point: q, normal, depth: radius - dist, surface: self.tris[ti as usize].surface, tri: ti });
        });
    }

    pub fn tri_points(&self, ti: u32) -> [[f32; 3]; 3] {
        self.tris[ti as usize].v.map(|i| self.verts[i as usize])
    }

    fn for_tris_in(&self, lo: [f32; 2], hi: [f32; 2], mut f: impl FnMut(u32)) {
        let g = &self.grid;
        let cell = |v: f32, k: usize| (((v - g.min[k]) / CELL).floor() as i64).clamp(0, g.dims[k] as i64 - 1) as usize;
        let (x0, x1, z0, z1) = (cell(lo[0], 0), cell(hi[0], 0), cell(lo[1], 1), cell(hi[1], 1));
        for z in z0..=z1 {
            for x in x0..=x1 {
                let c = z * g.dims[0] as usize + x;
                for &ti in &g.items[g.start[c] as usize..g.start[c + 1] as usize] {
                    f(ti);
                }
            }
        }
    }

    /// Möller–Trumbore, double-sided.
    fn ray_tri(&self, ti: u32, o: [f32; 3], d: [f32; 3], max_t: f32) -> Option<f32> {
        let [a, b, c] = self.tri_points(ti);
        let e1 = sub(b, a);
        let e2 = sub(c, a);
        let p = cross(d, e2);
        let det = dot(e1, p);
        if det.abs() < 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        let s = sub(o, a);
        let u = dot(s, p) * inv;
        if !(0.0..=1.0).contains(&u) {
            return None;
        }
        let q = cross(s, e1);
        let v = dot(d, q) * inv;
        if v < 0.0 || u + v > 1.0 {
            return None;
        }
        let t = dot(e2, q) * inv;
        (t >= 0.0 && t <= max_t).then_some(t)
    }

    /// Writes `collision.bin` (mesh + grid, little-endian) and `surfaces.json` into `dir`.
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let g = &self.grid;
        let mut b = Vec::with_capacity(32 + self.verts.len() * 12 + self.tris.len() * 16 + (g.start.len() + g.items.len()) * 4);
        b.extend_from_slice(FILE_MAGIC);
        for n in [self.verts.len(), self.tris.len(), g.start.len(), g.items.len()] {
            b.extend_from_slice(&(n as u32).to_le_bytes());
        }
        for f in [g.min[0], g.min[1], CELL] {
            b.extend_from_slice(&f.to_le_bytes());
        }
        for n in g.dims {
            b.extend_from_slice(&n.to_le_bytes());
        }
        for p in &self.verts {
            for f in p {
                b.extend_from_slice(&f.to_le_bytes());
            }
        }
        for t in &self.tris {
            for i in t.v {
                b.extend_from_slice(&i.to_le_bytes());
            }
            b.extend_from_slice(&[t.surface, t.flags]);
            b.extend_from_slice(&t.routes.to_le_bytes());
        }
        for n in g.start.iter().chain(&g.items) {
            b.extend_from_slice(&n.to_le_bytes());
        }
        std::fs::write(dir.join("collision.bin"), b)?;
        std::fs::write(dir.join("surfaces.json"), serde_json::to_vec_pretty(&self.surfaces)?)?;
        Ok(())
    }

    /// Reads what [`World::save`] wrote.
    pub fn load(dir: &Path) -> Result<World> {
        let b = std::fs::read(dir.join("collision.bin")).with_context(|| format!("reading {}/collision.bin", dir.display()))?;
        let surfaces: Vec<Surface> = serde_json::from_slice(&std::fs::read(dir.join("surfaces.json"))?)?;
        ensure!(b.len() >= 44 && &b[..8] == FILE_MAGIC, "collision.bin: bad magic");
        let mut r = LeReader { b: &b, at: 8 };
        let (nv, nt, ns, ni) = (r.u32() as usize, r.u32() as usize, r.u32() as usize, r.u32() as usize);
        let min = [r.f32(), r.f32()];
        let cell = r.f32();
        ensure!(cell == CELL, "collision.bin: cell size {cell}, expected {CELL}");
        let dims = [r.u32(), r.u32()];
        ensure!(b.len() == 44 + nv * 12 + nt * 16 + (ns + ni) * 4, "collision.bin: wrong length");
        ensure!(ns == (dims[0] * dims[1]) as usize + 1, "collision.bin: grid size mismatch");
        let verts = (0..nv).map(|_| [r.f32(), r.f32(), r.f32()]).collect();
        let tris = (0..nt)
            .map(|_| {
                let v = [r.u32(), r.u32(), r.u32()];
                let (surface, flags) = (r.u8(), r.u8());
                Tri { v, surface, flags, routes: r.u16() }
            })
            .collect();
        let start = (0..ns).map(|_| r.u32()).collect();
        let items = (0..ni).map(|_| r.u32()).collect();
        Ok(World { verts, tris, surfaces, grid: CellGrid { min, dims, start, items } })
    }

    #[doc(hidden)]
    pub fn same_as(&self, other: &World) -> bool {
        self.verts == other.verts && self.tris == other.tris && self.grid == other.grid && self.surfaces.len() == other.surfaces.len()
    }
}

fn build_grid(verts: &[[f32; 3]], tris: &[Tri]) -> CellGrid {
    let mut min = [f32::MAX; 2];
    let mut max = [f32::MIN; 2];
    for p in verts {
        min = [min[0].min(p[0]), min[1].min(p[2])];
        max = [max[0].max(p[0]), max[1].max(p[2])];
    }
    if verts.is_empty() {
        min = [0.0; 2];
        max = [0.0; 2];
    }
    let dims = [((max[0] - min[0]) / CELL).floor() as u32 + 1, ((max[1] - min[1]) / CELL).floor() as u32 + 1];
    let cell_range = |t: &Tri| {
        let ps = t.v.map(|i| verts[i as usize]);
        let c = |v: f32, k: usize| (((v - min[k]) / CELL).floor() as u32).min(dims[k] - 1);
        let (lx, hx) = (ps.iter().map(|p| p[0]).fold(f32::MAX, f32::min), ps.iter().map(|p| p[0]).fold(f32::MIN, f32::max));
        let (lz, hz) = (ps.iter().map(|p| p[2]).fold(f32::MAX, f32::min), ps.iter().map(|p| p[2]).fold(f32::MIN, f32::max));
        (c(lx, 0), c(hx, 0), c(lz, 1), c(hz, 1))
    };
    let ncells = (dims[0] * dims[1]) as usize;
    let mut start = vec![0u32; ncells + 1];
    for t in tris {
        let (x0, x1, z0, z1) = cell_range(t);
        for z in z0..=z1 {
            for x in x0..=x1 {
                start[(z * dims[0] + x) as usize + 1] += 1;
            }
        }
    }
    for i in 0..ncells {
        start[i + 1] += start[i];
    }
    let mut fill = start.clone();
    let mut items = vec![0u32; start[ncells] as usize];
    for (ti, t) in tris.iter().enumerate() {
        let (x0, x1, z0, z1) = cell_range(t);
        for z in z0..=z1 {
            for x in x0..=x1 {
                let c = (z * dims[0] + x) as usize;
                items[fill[c] as usize] = ti as u32;
                fill[c] += 1;
            }
        }
    }
    CellGrid { min, dims, start, items }
}

/// Closest point on triangle abc to p (Ericson, Real-Time Collision Detection 5.1.5).
fn closest_on_tri(p: [f32; 3], a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let ab = sub(b, a);
    let ac = sub(c, a);
    let ap = sub(p, a);
    let (d1, d2) = (dot(ab, ap), dot(ac, ap));
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = sub(p, b);
    let (d3, d4) = (dot(ab, bp), dot(ac, bp));
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        return add(a, scale(ab, d1 / (d1 - d3)));
    }
    let cp = sub(p, c);
    let (d5, d6) = (dot(ab, cp), dot(ac, cp));
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        return add(a, scale(ac, d2 / (d2 - d6)));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        return add(b, scale(sub(c, b), (d4 - d3) / ((d4 - d3) + (d5 - d6))));
    }
    let denom = 1.0 / (va + vb + vc);
    add(a, add(scale(ab, vb * denom), scale(ac, vc * denom)))
}

struct LeReader<'a> {
    b: &'a [u8],
    at: usize,
}

impl LeReader<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let v = self.b[self.at..self.at + N].try_into().unwrap();
        self.at += N;
        v
    }
    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take())
    }
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = dot(a, a).sqrt();
    if l > 0.0 { scale(a, 1.0 / l) } else { [0.0, 1.0, 0.0] }
}

/// `surfaceTypes.xml` from `media/physics.zip` (FH1), or loose in `media/physics/` (FM4).
fn surface_types_xml(disc: &Path) -> Result<String> {
    let loose = disc.join("media/physics/surfaceTypes.xml");
    if !disc.join("media/physics.zip").exists() && loose.exists() {
        return String::from_utf8(std::fs::read(&loose)?).context("surfaceTypes.xml is not UTF-8");
    }
    let mut physics = Archive::open(disc.join("media/physics.zip")).context("opening media/physics.zip")?;
    let entry = physics
        .entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case("surfaceTypes.xml"))
        .cloned()
        .context("surfaceTypes.xml missing from physics.zip")?;
    String::from_utf8(physics.read(&entry)?).context("surfaceTypes.xml is not UTF-8")
}
