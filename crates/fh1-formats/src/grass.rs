//! Colorado grass: `Grass_*` `.pgeo` objects (header type 2, `CProceduralVegetation`), decoded from
//! default.xex (loader 0x82E0B280 + mesh loader 0x82DFD6A0, scatter 0x82E126E8, per-layer blade count
//! 0x82DFB4F0, triangle sampler 0x82DF5210, vertex decode 0x82DF5308). Notes: docs/PROPS.md.
//!
//! The game stores no blades. Each object holds a small ground mesh (the grass area) whose vertices
//! carry a density weight, a grass-mix id and an RGB565 tint; every triangle carries its area, slope
//! and a 16-bit seed. Blades are scattered at runtime per triangle with the game's LCG, separately for
//! three **sub-layers** (each drawn up to its own distance, see [`Grass::distances`]). [`Grass::scatter`]
//! reproduces that walk: the per-batch blade counts it produces equal the counts the game's tool
//! precomputed into every file (VERIFIED on all 3,199 Colorado objects x 3 sub-layers), and the tool's
//! removal lists (blades it culled, e.g. on roads) are applied by blade index exactly like the game.
//!
//! Layout (big-endian; every list is aligned as noted, the walk ends exactly at the end of all 3,199
//! files):
//! - common `.pgeo` header (0x60 bytes), name (`header@0x3C` bytes), 4-align, `header@0x40` texture
//!   references `(u32 PVS texture index, u32 0)` (the PVS 28-byte texture table, `_0x%08X.bix`).
//! - 16-align: mesh header (0x40): bbox min/max (vec4), u32 vertex count, u32 index count.
//!   Vertices (12 bytes): u16 x, y, z (fractions of the bbox), u16 density (16.16, 0..1), i16 mix id
//!   (< 0 = none), u16 RGB565 tint. u16 indices (2-align). Per triangle (2-align): u16 seed.
//!   Per triangle (4-align): i32 area (16.16 m^2), i32 slope (16.16 degrees).
//! - 4-align: vegetation header (0x3C): `[ptr, n]` blade types (0x2C each), `[ptr, n]` layers (0x40),
//!   `[ptr, n]` mixes (8: `[ptr, n]` -> u32 layer indices), u32 batch count, ptr batches (8), three
//!   ptrs (per sub-layer batch counts), f32 x4 distances. Pointers are file garbage, the loader fixes
//!   them up in this order: mixes + their lists, types, layers + their type lists, batches, then per
//!   sub-layer: batch entries `(u32 count, u32 removed, ptr)` + their removed-index lists.

use crate::Error;

/// One vertex of the grass-area mesh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrassVertex {
    /// Collision space (left-handed, +Z north, like the `.pgeo` Models placements).
    pub position: [f32; 3],
    /// Density weight, 16.16 (0x10000 = 1).
    pub density: u16,
    /// Grass mix ([`Grass::mixes`]) or negative for none.
    pub mix: i16,
    /// RGB565 tint (0xFFFF = white).
    pub colour: u16,
}

/// A blade type (`0x2C` record): size ranges, atlas rectangle and its draw batch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BladeType {
    pub width: [f32; 2],
    pub height: [f32; 2],
    /// Atlas rectangle `[u_left, u_right, v_bottom, v_top]` (u may be mirrored).
    pub uv: [f32; 4],
    /// Words at +0x20 / +0x24 (unknown; 0/0 or 1/0 in Colorado).
    pub unknown: [u32; 2],
    /// Index into [`Grass::batches`].
    pub batch: u32,
}

/// A layer (`0x40` record): density, slope fade and the three sub-layer weights.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    /// Blades per m^2 (16.16) at full vertex density on flat ground.
    pub density: i32,
    /// Slope fade (16.16 degrees): full density below `slope[0]`, none above `slope[1]`.
    pub slope: [i32; 2],
    /// Keep probability out of 0x7FFF (0x7FFF = keep all).
    pub keep: u32,
    /// Share of the blades for each sub-layer (16.16, sums to 1).
    pub weights: [i32; 3],
    /// Blade types to pick from (uniformly).
    pub types: Vec<u32>,
}

/// A draw batch: one texture, one instance kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrassBatch {
    /// 0 = 16-byte instances (camera-facing cards; all of Colorado), 1 = 0xC0-byte rotated cards.
    pub kind: u32,
    /// PVS texture index (via [`Grass::textures`]).
    pub texture: u32,
}

/// Per sub-layer, per batch: the game's precomputed instance count and its removal list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BatchCounts {
    /// Blades kept (= scattered minus removed).
    pub count: u32,
    /// Indices (in scatter order within the batch) of the blades the tool removed, ascending.
    pub removed: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grass {
    pub name: String,
    /// PVS texture indices (`Colorado_00.pvs` 28-byte table).
    pub textures: Vec<u32>,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub vertices: Vec<GrassVertex>,
    pub indices: Vec<u16>,
    pub tri_seeds: Vec<u16>,
    /// 16.16 m^2.
    pub tri_area: Vec<i32>,
    /// 16.16 degrees.
    pub tri_slope: Vec<i32>,
    pub types: Vec<BladeType>,
    pub layers: Vec<Layer>,
    /// Mix id -> layer indices.
    pub mixes: Vec<Vec<u32>>,
    pub batches: Vec<GrassBatch>,
    /// Per sub-layer, per batch.
    pub counts: [Vec<BatchCounts>; 3],
    /// Header floats at +0x2C: `(26, 75, 150, 20)` on 2,967 objects, `(100, 300, 500, 50)` and
    /// `(200, 400, 800, 50)` on the rest. Sub-layer k is wanted within `d[k]` m (VERIFIED: the `.pvsz`
    /// zones list sub k exactly out to `d[k]` + ~273 m from their hex centre, docs/PROPS.md); `d[3]`
    /// is probably the fade length (UNVERIFIED).
    pub distances: [f32; 4],
}

/// One scattered blade.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Blade {
    /// Ground point, collision space.
    pub position: [f32; 3],
    /// Interpolated vertex tint, linear 0..1 per channel of the RGB565 value (`[bits 11-15, 5-10,
    /// 0-4]`). The game only uses `colour[2]` (bits 0-4), see [`Blade::shade`].
    pub colour: [f32; 3],
    /// Unit normal of the ground triangle (collision space, pointing up). VERIFIED: the instance
    /// writer stores the face normal (0x82DF5160, `cross(V2 - V0, V1 - V0)` normalised; flipped up
    /// here), and PROC_VEGETATION_VS stands each card along it.
    pub normal: [f32; 3],
    pub width: f32,
    pub height: f32,
    /// Index into [`Grass::types`].
    pub blade_type: u16,
    /// Index into [`Grass::batches`].
    pub batch: u16,
}

struct Rd<'a> {
    d: &'a [u8],
    o: usize,
}

impl Rd<'_> {
    fn align(&mut self, a: usize) {
        self.o = self.o.next_multiple_of(a);
    }
    fn bytes(&mut self, n: usize) -> Result<&[u8], Error> {
        let b = self.d.get(self.o..self.o + n).ok_or(Error::Truncated("grass pgeo"))?;
        self.o += n;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, Error> {
        Ok(self.u32()? as i32)
    }
    fn f32(&mut self) -> Result<f32, Error> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn at_u32(&self, o: usize) -> Result<u32, Error> {
        self.d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("grass pgeo"))
    }
    fn count(&mut self, max: u32) -> Result<usize, Error> {
        let n = self.u32()?;
        if n > max {
            return Err(Error::Unsupported(format!("grass pgeo: count {n} at {:#x}", self.o - 4)));
        }
        Ok(n as usize)
    }
}

/// Header type at 0x30 of a `.pgeo` (2 = grass, 3 = crowd, 7 = models...).
pub fn pgeo_type(d: &[u8]) -> Option<u32> {
    d.get(0x30..0x34).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
}

/// Parses a `Grass_*` `.pgeo` (header type 2).
pub fn parse(d: &[u8]) -> Result<Grass, Error> {
    if d.get(..4) != Some(b"OEGP") {
        return Err(Error::BadMagic("pgeo: OEGP"));
    }
    if pgeo_type(d) != Some(2) {
        return Err(Error::Unsupported("pgeo: not a grass object (type 2)".into()));
    }
    let mut r = Rd { d, o: 0 };
    // Common header: the loader (0x82E099D0) walks blob @0x58 (unused here), name (@0x3C bytes),
    // the lists @0x44 / @0x54 (empty in every grass file), then the references @0x40.
    if r.at_u32(0x58)? != 0 || r.at_u32(0x44)? != 0 || r.at_u32(0x54)? != 0 {
        return Err(Error::Unsupported("grass pgeo: unexpected header lists".into()));
    }
    let name_len = r.at_u32(0x3C)? as usize;
    let name_raw = d.get(0x60..0x60 + name_len).ok_or(Error::Truncated("grass name"))?;
    let name = name_raw[..name_raw.iter().position(|&b| b == 0).unwrap_or(name_len)].iter().map(|&b| b as char).collect();
    let n_refs = r.at_u32(0x40)? as usize;
    r.o = 0x60 + name_len;
    r.align(4);
    let mut textures = Vec::with_capacity(n_refs);
    for _ in 0..n_refs {
        textures.push(r.u32()?);
        if r.u32()? != 0 {
            return Err(Error::Unsupported("grass pgeo: reference with a payload".into()));
        }
    }
    // Mesh (0x82DFD6A0).
    r.align(16);
    let m = r.o;
    let f = |o: usize| r.at_u32(o).map(f32::from_bits);
    let (bbox_min, bbox_max) = ([f(m)?, f(m + 4)?, f(m + 8)?], [f(m + 0x10)?, f(m + 0x14)?, f(m + 0x18)?]);
    r.o = m + 0x20;
    let nv = r.count(1 << 20)?;
    let ni = r.count(3 << 20)?;
    let nt = ni / 3;
    r.o = m + 0x40;
    let size = [0, 1, 2].map(|k| bbox_max[k] - bbox_min[k]);
    let mut vertices = Vec::with_capacity(nv);
    for _ in 0..nv {
        let q = [r.u16()?, r.u16()?, r.u16()?];
        let (density, mix, colour) = (r.u16()?, r.u16()? as i16, r.u16()?);
        // 0x82DF5308: q * (1 / 65535) * size + min.
        let position = [0, 1, 2].map(|k| (q[k] as f32 * (1.0 / 65535.0)).mul_add(size[k], bbox_min[k]));
        vertices.push(GrassVertex { position, density, mix, colour });
    }
    r.align(2);
    let mut indices = Vec::with_capacity(ni);
    for _ in 0..ni {
        let i = r.u16()?;
        if i as usize >= nv {
            return Err(Error::Unsupported(format!("grass pgeo: index {i} of {nv} vertices")));
        }
        indices.push(i);
    }
    r.align(2);
    let tri_seeds = (0..nt).map(|_| r.u16()).collect::<Result<Vec<_>, _>>()?;
    r.align(4);
    let (mut tri_area, mut tri_slope) = (Vec::with_capacity(nt), Vec::with_capacity(nt));
    for _ in 0..nt {
        tri_area.push(r.i32()?);
        tri_slope.push(r.i32()?);
    }
    // Vegetation data (0x82E0B280).
    r.align(4);
    let h = r.o;
    let n_types = r.at_u32(h + 4)? as usize;
    let n_layers = r.at_u32(h + 0xC)? as usize;
    let n_mixes = r.at_u32(h + 0x14)? as usize;
    let n_batches = r.at_u32(h + 0x18)? as usize;
    if n_types > 4096 || n_layers > 4096 || n_mixes > 4096 || n_batches > 4096 {
        return Err(Error::Unsupported("grass pgeo: implausible table sizes".into()));
    }
    let mut distances = [0f32; 4];
    r.o = h + 0x2C;
    for x in &mut distances {
        *x = r.f32()?;
    }
    r.o = h + 0x3C;
    r.align(4);
    let mix_counts: Vec<usize> = (0..n_mixes).map(|_| r.u32().and_then(|_| r.count(1 << 16))).collect::<Result<_, _>>()?;
    let mut mixes = Vec::with_capacity(n_mixes);
    for n in mix_counts {
        if n > 0 {
            r.align(4);
        }
        mixes.push((0..n).map(|_| r.u32()).collect::<Result<Vec<_>, _>>()?);
    }
    r.align(4);
    let mut types = Vec::with_capacity(n_types);
    for _ in 0..n_types {
        let f: Vec<f32> = (0..8).map(|_| r.f32()).collect::<Result<_, _>>()?;
        let unknown = [r.u32()?, r.u32()?];
        let batch = r.u32()?;
        types.push(BladeType { width: [f[0], f[1]], height: [f[2], f[3]], uv: [f[4], f[5], f[6], f[7]], unknown, batch });
    }
    r.align(4);
    let mut layer_counts = Vec::with_capacity(n_layers);
    let mut layers = Vec::with_capacity(n_layers);
    for _ in 0..n_layers {
        let l = r.o;
        // +4..+0x1C hold float copies of the fixed-point fields used by the scatter.
        r.o = l + 0x20;
        layer_counts.push(r.count(1 << 16)?);
        let keep = r.u32()?;
        let weights = [r.i32()?, r.i32()?, r.i32()?];
        let density = r.i32()?;
        let slope = [r.i32()?, r.i32()?];
        layers.push(Layer { density, slope, keep, weights, types: Vec::new() });
    }
    for (layer, n) in layers.iter_mut().zip(layer_counts) {
        if n > 0 {
            r.align(4);
        }
        layer.types = (0..n).map(|_| r.u32()).collect::<Result<_, _>>()?;
    }
    r.align(4);
    let mut batches = Vec::with_capacity(n_batches);
    for _ in 0..n_batches {
        let (kind, texture) = (r.u32()?, r.u32()?);
        batches.push(GrassBatch { kind, texture: *textures.get(texture as usize).ok_or(Error::Unsupported("grass pgeo: batch texture".into()))? });
    }
    let mut counts: [Vec<BatchCounts>; 3] = Default::default();
    for sub in &mut counts {
        r.align(4);
        let heads: Vec<(u32, usize)> = (0..n_batches).map(|_| -> Result<_, Error> { let c = r.u32()?; let n = r.count(1 << 24)?; r.u32()?; Ok((c, n)) }).collect::<Result<_, _>>()?;
        for (count, n) in heads {
            if n > 0 {
                r.align(4);
            }
            sub.push(BatchCounts { count, removed: (0..n).map(|_| r.u32()).collect::<Result<_, _>>()? });
        }
    }
    if r.o != d.len() {
        return Err(Error::SizeMismatch { expected: d.len(), got: r.o });
    }
    // Cross-checks the scatter relies on.
    for t in &types {
        if t.batch as usize >= batches.len() {
            return Err(Error::Unsupported("grass pgeo: blade type batch".into()));
        }
    }
    for l in &layers {
        if l.types.is_empty() || l.types.iter().any(|&t| t as usize >= types.len()) {
            return Err(Error::Unsupported("grass pgeo: layer types".into()));
        }
    }
    if mixes.iter().flatten().any(|&l| l as usize >= layers.len()) || vertices.iter().any(|v| v.mix as i32 >= mixes.len() as i32) {
        return Err(Error::Unsupported("grass pgeo: mix ids".into()));
    }
    Ok(Grass { name, textures, bbox_min, bbox_max, vertices, indices, tri_seeds, tri_area, tri_slope, types, layers, mixes, batches, counts, distances })
}

/// The game's LCG (0x82CFC760): `s = s * 0x41C64E6D + 0x3039`, 15-bit output.
#[derive(Debug, Clone, Copy)]
struct Lcg(u32);

impl Lcg {
    fn next15(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(0x41C6_4E6D).wrapping_add(0x3039);
        (self.0 >> 16) & 0x7FFF
    }
    /// 0x82CFC780: [0, 1] in f32 (scale 0x38000100 = 1/32767).
    fn unit(&mut self) -> f32 {
        self.next15() as f32 * f32::from_bits(0x3800_0100)
    }
    /// 0x82CFC808 with (lo, hi) = (0, 0x10000): `trunc(r * k * 65536 + 0.5)`, all f32.
    fn fraction16(&mut self) -> i32 {
        ((self.unit() * 65536.0) + 0.5) as i32
    }
}

fn mul16(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 16) as i32
}

impl Grass {
    /// Blades of sub-layer `sub` (0..3) from layer `layer` on one triangle (0x82DFB4F0).
    fn layer_count(&self, rng: &mut Lcg, layer: &Layer, tri: usize, density: i32, sub: usize) -> u32 {
        let span = (layer.slope[1] - layer.slope[0]) as i64;
        let fade = if span == 0 { 0 } else { ((((self.tri_slope[tri] - layer.slope[0]) as i64) << 16) / span) as i32 }.clamp(0, 0x10000);
        let n = mul16(mul16(mul16(layer.density, self.tri_area[tri]), 0x10000 - fade), density);
        let mut count = (n >> 16) as u32;
        if rng.fraction16() < n - ((n >> 16) << 16) {
            count += 1;
        }
        if count == 0 {
            return 0;
        }
        if layer.keep < 0x7FFF {
            count = (0..count).filter(|_| rng.next15() <= layer.keep).count() as u32;
        }
        // Split between the sub-layers, leftovers to the largest remainders (first wins ties).
        let mut whole = [0u32; 3];
        let mut rem = [0i32; 3];
        let mut total = 0;
        for k in 0..3 {
            let v = mul16(layer.weights[k], (count << 16) as i32);
            whole[k] = (v >> 16) as u32;
            rem[k] = v - ((v >> 16) << 16);
            total += whole[k];
        }
        while total < count {
            let mut j = 0;
            for k in 1..3 {
                if rem[k] > rem[j] {
                    j = k;
                }
            }
            rem[j] = 0;
            whole[j] += 1;
            total += 1;
        }
        whole[sub]
    }

    /// Scatters sub-layer `sub` (0..3) like the game (0x82E126E8): triangles in order, each reseeding
    /// three LCGs with `seed * (sub + 1)`; per mix on the triangle's vertices and per layer of that
    /// mix, a count from area x density x slope fade x mean vertex density, scaled by the share of
    /// vertices carrying the mix; points uniform in the triangle (`sqrt` barycentrics), blade type
    /// picked from the layer, size lerped by one random. Blades the tool removed are skipped by index.
    pub fn scatter(&self, sub: usize) -> Vec<Blade> {
        let mut out = Vec::new();
        self.scatter_into(sub, &mut out);
        out
    }

    /// [`Grass::scatter`] appending to `out`; returns the blades scattered per batch before removal.
    pub fn scatter_into(&self, sub: usize, out: &mut Vec<Blade>) -> Vec<u32> {
        assert!(sub < 3);
        let nb = self.batches.len();
        let mut scattered = vec![0u32; nb];
        let mut cursor = vec![0usize; nb];
        let removed = &self.counts[sub];
        let mut points: Vec<([f32; 3], [f32; 3])> = Vec::with_capacity(100);
        for t in 0..self.indices.len() / 3 {
            let seed = (self.tri_seeds[t] as u32 * (sub as u32 + 1)) & 0xFFFF;
            let (mut pick, mut size, mut place) = (Lcg(seed), Lcg(seed), Lcg(seed));
            let v = [0, 1, 2].map(|k| self.vertices[self.indices[3 * t + k] as usize]);
            let density = (((v[0].density as i64 + v[1].density as i64 + v[2].density as i64) << 16) / 0x30000) as i32;
            // Distinct mix ids in vertex order with how many vertices carry each (0x82DFB420).
            let mut mixes: Vec<(i16, u32)> = Vec::with_capacity(3);
            for x in &v {
                if x.mix >= 0 {
                    match mixes.iter_mut().find(|m| m.0 == x.mix) {
                        Some(m) => m.1 += 1,
                        None => mixes.push((x.mix, 1)),
                    }
                }
            }
            let colour = v.map(|x| rgb565(x.colour));
            let normal = face_normal(v.map(|x| x.position));
            for (mix, share) in mixes {
                for &li in &self.mixes[mix as usize] {
                    let layer = &self.layers[li as usize];
                    let mut left = self.layer_count(&mut pick, layer, t, density, sub) * share / 3;
                    while left > 0 {
                        let chunk = left.min(100);
                        left -= chunk;
                        // 0x82DF5210: positions and tints of the whole chunk first.
                        points.clear();
                        for _ in 0..chunk {
                            let a = place.unit();
                            let s = place.unit().sqrt();
                            // VERIFIED (0x82DF5210): v0 gets 1 - s, v1 s(1 - a), v2 s a, a = first draw.
                            let w = [1.0 - s, s * (1.0 - a), s * a];
                            let lerp = |p: [[f32; 3]; 3]| [0, 1, 2].map(|k| w[0] * p[0][k] + w[1] * p[1][k] + w[2] * p[2][k]);
                            points.push((lerp(v.map(|x| x.position)), lerp(colour)));
                        }
                        for &(position, colour) in &points {
                            let ti = layer.types[pick.next15() as usize % layer.types.len()];
                            let ty = &self.types[ti as usize];
                            let b = ty.batch as usize;
                            let (width, height) = if self.batches[b].kind == 0 {
                                let r = size.unit();
                                ((ty.width[1] - ty.width[0]).mul_add(r, ty.width[0]), (ty.height[1] - ty.height[0]).mul_add(r, ty.height[0]))
                            } else {
                                // Kind 1: width, height, then a yaw (not kept: no Colorado batch uses it).
                                let rw = size.unit();
                                let rh = size.unit();
                                size.unit();
                                ((ty.width[1] - ty.width[0]).mul_add(rw, ty.width[0]), (ty.height[1] - ty.height[0]).mul_add(rh, ty.height[0]))
                            };
                            let index = scattered[b];
                            scattered[b] += 1;
                            let list = &removed[b].removed;
                            if cursor[b] < list.len() && list[cursor[b]] == index {
                                cursor[b] += 1;
                                continue;
                            }
                            out.push(Blade { position, colour, normal, width, height, blade_type: ti as u16, batch: b as u16 });
                        }
                    }
                }
            }
        }
        scattered
    }
}

impl Blade {
    /// The grey the game multiplies the blade's texture by (VERIFIED, 0x82E13380..0x82E13684 +
    /// PROC_VEGETATION_VS): the 16-byte kind-0 instance packs the face normal as DEC4N
    /// (`x, y, z * 511` at bits 0 / 10 / 20) with `w = round(2 * colour[2] - 1)` in the 2-bit field
    /// at bit 30, and the VS feeds `0.5 + 0.5 * w` to the PS as the tint. So only the RGB565 low
    /// 5 bits count, and they land on three levels: 0, 0.5 or 1.
    pub fn shade(&self) -> f32 {
        // vrfin128 = round to nearest even.
        0.5 + 0.5 * (2.0 * self.colour[2] - 1.0).round_ties_even().clamp(-1.0, 1.0)
    }
}

fn face_normal(p: [[f32; 3]; 3]) -> [f32; 3] {
    let (a, b) = ([0, 1, 2].map(|k| p[1][k] - p[0][k]), [0, 1, 2].map(|k| p[2][k] - p[0][k]));
    let mut n = [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if l == 0.0 {
        return [0.0, 1.0, 0.0];
    }
    if n[1] < 0.0 {
        n = n.map(|x| -x);
    }
    n.map(|x| x / l)
}

fn rgb565(c: u16) -> [f32; 3] {
    [((c >> 11) & 31) as f32 / 31.0, ((c >> 5) & 63) as f32 / 63.0, (c & 31) as f32 / 31.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Colorado grass object parses to its end, and the scatter reproduces the game tool's
    /// precomputed per-batch blade counts for all three sub-layers.
    #[test]
    fn colorado_grass_counts() {
        let disc = std::env::var("FH1_DISC").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../disc").into());
        let Ok(mut ar) = crate::zip::Archive::open(std::path::Path::new(&disc).join("media/tracks/colorado/bin.zip")) else {
            eprintln!("no disc, skipped");
            return;
        };
        let mut seen = std::collections::HashSet::new();
        let (mut objects, mut blades) = (0, 0usize);
        for e in ar.entries.clone() {
            let n = e.name.to_ascii_lowercase();
            if !n.ends_with(".pgeo") || !seen.insert(n.clone()) {
                continue;
            }
            let d = ar.read(&e).unwrap();
            if pgeo_type(&d) != Some(2) {
                continue;
            }
            let g = parse(&d).unwrap_or_else(|err| panic!("{n}: {err}"));
            objects += 1;
            for sub in 0..3 {
                let mut out = Vec::new();
                g.scatter_into(sub, &mut out);
                for (b, c) in g.counts[sub].iter().enumerate() {
                    let got = out.iter().filter(|x| x.batch as usize == b).count();
                    assert_eq!(got as u32, c.count, "{n} sub {sub} batch {b}");
                }
                blades += out.len();
            }
        }
        assert_eq!(objects, 3199);
        eprintln!("{objects} grass objects, {blades} blades");
    }
}
