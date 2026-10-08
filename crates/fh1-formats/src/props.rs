//! Colorado prop placement: `.pgeo` procedural-geometry instance groups (`bin.zip`
//! `__R00G#####.pgeo`, magic `OEGP`) and the loose `CollObjs.xml` / `GameObjs.xml` placements.
//!
//! The `.pgeo` layout was read from default.xex (`proceduralGeometry::CProceduralModels`,
//! instance packer 0x82E13808, culling 0x82E1FD00) and verified on the user's disc: every one of
//! the 1,562 `Models_*` files parses exactly, and decoded positions land on the collision surface
//! (see docs/PROPS.md). Big-endian throughout.
//!
//! - Header: `OEGP`, u32 version (42..44), 2 f32, vec4 bbox min @0x10, vec4 bbox max @0x20
//!   (collision space), ..., u32 file size @0x48, u32 instance-block size @0x50, u32 model count
//!   @0x54, 32-byte group name @0x60 (`Models_Ungrouped_904`; stale bytes after the NUL).
//! - Models @0x80, 0x40 bytes each: vec4 local bounds min, vec4 max, then (count, pointer) of the
//!   LOD0 / LOD1 / LOD2 reference lists @0x24 / @0x2C / @0x34.
//! - Model references: u32 per LOD entry (each model's LOD0, LOD1, LOD2 lists, in model order).
//!   Each is an index into the **18-byte record table of `Ribbon_00/<Track>_00.pvs`** (62,173 in
//!   Colorado), whose first u16 is the **model number** (`<track>out.%05d.rmb.bin`), see
//!   [`pvs_model_numbers`]. Verified: 7,986 of the 11,217 referenced models have bounds equal to
//!   their template's; the rest are the union of the LOD0 and LOD1 meshes (same X, Z extents).
//! - Then other references (`.fiz` collision etc.) and a triple `(u32, draw count, block size)`,
//!   followed by draw entries, 0x5C bytes each: model index @0x2C, full-record count @0x30,
//!   compact-instance count @0x34, LOD-table entry count @0x38, scaled-size factors @0x18 / @0x1C.
//! - Per draw entry: 16-byte alignment, `full` records of 0x60 bytes (4x4 f32 matrix, rows =
//!   axes incl. scale, row 3 = translation with w = 1; extras after), then `lod` x 0x20 bytes of
//!   LOD distances.
//! - The file tail = the compact instances, 40 bytes each, in draw-entry order:
//!   - word 0: position, packed **x:11 | z:11 << 11 | y:10 << 22**, unsigned fractions of the
//!     header bbox (scales 2047 / 2047 / 1023);
//!   - word 1: normalised Y axis (row 1), signed fractions packed x:11 | y:11 | z:10
//!     (scales 1023 / 1023 / 511); word 2: normalised X axis (row 0), same packing;
//!   - word 3: a signed 10:10:10 vector (record +0x50; not needed to place the mesh);
//!   - words 4-5: per-draw fade distances; words 6-7: four f16 (bounding size x scale for
//!     culling: h0 = size@0x18 x |row2| / 2, h1 = size@0x1C x |row1|, then -1000 and a constant);
//!   - word 8: tint, word 9: colour.
//!
//! Transforms are returned in **collision space** (left-handed, +Z north) as the game stores
//! them; [`Placement::engine_matrix`] converts to the engine's right-handed space (S·M·S with
//! S = diag(1, 1, -1)), which is what the right-handed `.rmb.bin` templates need.

use crate::Error;

/// `PhysicsDefinitions.bin` (prop rigid bodies and collision shapes). Declared here so lib.rs needn't change.
#[path = "physdef.rs"]
pub mod physdef;

/// One placed model instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    /// Model index within its `.pgeo` file.
    pub model: usize,
    /// Rows = the model's X, Y, Z axes in collision space, scale included (row-vector convention:
    /// world = local.x * x_axis + local.y * y_axis + local.z * z_axis + position).
    pub x_axis: [f32; 3],
    pub y_axis: [f32; 3],
    pub z_axis: [f32; 3],
    pub position: [f32; 3],
    /// Came from a full 0x60 record (exact float matrix) rather than a compact 40-byte one.
    pub full: bool,
    /// Ground normal under the instance (collision space): compact word 3, a signed 10:10:10 unit vector
    /// (VERIFIED unit length on 122k instances; meaning INFERRED), [`DEFAULT_NORMAL`] where absent.
    /// Feeds the tree shaders' `SurfaceNormalAndShadowPower` (docs/PROPS.md "Per-instance shader values").
    pub normal: [f32; 3],
    /// Instance tint, D3DCOLOR (compact word 8; [`DEFAULT_TINT`] on half of all instances): `ModelData`.
    pub tint: u32,
}

/// Ground normal for placements that don't store one.
pub const DEFAULT_NORMAL: [f32; 3] = [0.0, 1.0, 0.0];
/// The most common instance tint (neutral: the shaders multiply by 2 x RGB).
pub const DEFAULT_TINT: u32 = 0xFF80_8080;

/// A signed 10:10:10 vector (x bits 0-9, y 10-19, z 20-29, /511) if it has unit length.
fn unit_10_10_10(w: u32) -> Option<[f32; 3]> {
    let v = [sfield(w, 0, 10) as f32 / 511.0, sfield(w, 10, 10) as f32 / 511.0, sfield(w, 20, 10) as f32 / 511.0];
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    ((l - 1.0).abs() < 0.02).then(|| v.map(|x| x / l))
}

impl Placement {
    /// Column-major 4x4 matrix in engine space (right-handed, Z negated), mapping the engine-space
    /// template mesh to the world.
    pub fn engine_matrix(&self) -> [f32; 16] {
        let f = |v: [f32; 3]| [v[0], v[1], -v[2]];
        let (x, y, z, p) = (f(self.x_axis), f(self.y_axis), f(self.z_axis), f(self.position));
        // S·M·S: the Z axis row also flips sign as a whole.
        let z = [-z[0], -z[1], -z[2]];
        [x[0], x[1], x[2], 0.0, y[0], y[1], y[2], 0.0, z[0], z[1], z[2], 0.0, p[0], p[1], p[2], 1.0]
    }
}

/// A model of a `.pgeo` group.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoModel {
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    /// References per LOD (indices into the `.pvs` 18-byte records, see [`pvs_model_numbers`]).
    /// Lists at model +0x24 / +0x2C / +0x34 (count, pointer). A `NOLOD` model repeats the same
    /// template in each slot.
    pub lod0: Vec<u32>,
    pub lod1: Vec<u32>,
    pub lod2: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcGeo {
    pub version: u32,
    pub name: String,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub models: Vec<GeoModel>,
    pub placements: Vec<Placement>,
    pub draws: Vec<Draw>,
    /// Activity-conditional group (`0xFFFFFFFF` @0x60, extended header): the activity ids it
    /// belongs to (count u8 @0x84, ids u8 from 0x85). Id 0 = free roam: those groups' names end
    /// in `[freeroam]` (Colorado: 55 tree / fencing groups); the others are event barriers, event
    /// crowds and animated festival dressing. `None` = an ordinary, always-present group.
    pub activities: Option<Vec<u8>>,
}

/// A draw entry: one model's instances and their LOD / fade distances.
#[derive(Debug, Clone, PartialEq)]
pub struct Draw {
    pub model: usize,
    pub full: usize,
    pub compact: usize,
    /// LOD tables (an instance picks one): per the culling code (0x82E1FD00), in the main views
    /// LOD0 is drawn below `t[0]` m, LOD1 below `t[1]`, LOD2 beyond, culled beyond `t[2]` (the
    /// global LOD scale at 0x832B9C3C is 1.0); `t[3]` (x 0.5) and `t[4]` are the cull distances of
    /// the shadow-cascade and reflection views; `t[5..8]` unknown. All-zero `t[0..3]` occurs only on
    /// compact-instance draws, whose distances come from elsewhere (UNVERIFIED).
    pub lod_tables: Vec<[f32; 8]>,
    /// Floats at +0x3C / +0x40 / +0x44 (e.g. 220, 150, 300).
    pub distances: [f32; 3],
}

impl ProcGeo {
    /// Group kind from the name: `Models`, `Grass`, `crowd`, `LightMap`, `Glow`, ... (conditional
    /// groups use lowercase `models` / `anim`).
    pub fn kind(&self) -> &str {
        self.name.split('_').next().unwrap_or("")
    }

    /// Present in free roam: an ordinary group, or a conditional one listing activity 0.
    pub fn in_free_roam(&self) -> bool {
        self.activities.as_ref().is_none_or(|a| a.contains(&0))
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn u32(&self, o: usize) -> Result<u32, Error> {
        self.0.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("pgeo"))
    }
    fn f32(&self, o: usize) -> Result<f32, Error> {
        Ok(f32::from_bits(self.u32(o)?))
    }
    fn vec3(&self, o: usize) -> Result<[f32; 3], Error> {
        Ok([self.f32(o)?, self.f32(o + 4)?, self.f32(o + 8)?])
    }
    fn f16(&self, o: usize) -> Result<f32, Error> {
        let b = self.0.get(o..o + 2).ok_or(Error::Truncated("pgeo"))?;
        Ok(half_to_f32(u16::from_be_bytes([b[0], b[1]])))
    }
}

fn half_to_f32(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1F) as i32;
    let m = (h & 0x3FF) as f32;
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Signed `bits`-wide field of `v` starting at bit `shift`.
fn sfield(v: u32, shift: u32, bits: u32) -> i32 {
    let x = ((v >> shift) & ((1 << bits) - 1)) as i32;
    if x >> (bits - 1) != 0 {
        x - (1 << bits)
    } else {
        x
    }
}

fn norm(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l > 0.0 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        v
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn scale(v: [f32; 3], s: f32) -> [f32; 3] {
    [v[0] * s, v[1] * s, v[2] * s]
}

/// Parses a `.pgeo`. Only `Models_*` groups carry placements of `.rmb.bin` templates; other kinds
/// (grass, crowds, light maps, glows, animated objects) return their header with no placements.
pub fn parse_pgeo(d: &[u8]) -> Result<ProcGeo, Error> {
    let r = R(d);
    if d.get(..4) != Some(b"OEGP") {
        return Err(Error::BadMagic("pgeo: OEGP"));
    }
    let version = r.u32(4)?;
    let bbox_min = r.vec3(0x10)?;
    let bbox_max = r.vec3(0x20)?;
    // Activity-conditional groups (0xFFFFFFFF @0x60) have a longer header: activity list @0x84 and
    // a NUL-terminated name @0x98, models from the next 16-byte boundary after the NUL (verified:
    // all 469 Colorado type-7 files walk exactly with this base). Event crowds / glows (types 3,
    // 6) keep a short name @0x64.
    let conditional = d.get(0x60..0x64) == Some(&[0xFF; 4][..]);
    let kind_id = r.u32(0x30)?;
    let extended = conditional && matches!(kind_id, 4 | 5 | 7);
    let (name_at, name_max) = match (conditional, extended) {
        (true, true) => (0x98, 0x80),
        (true, false) => (0x64, 0x1C),
        _ => (0x60, 0x20),
    };
    let name_raw = d.get(name_at..(name_at + name_max).min(d.len())).ok_or(Error::Truncated("pgeo name"))?;
    let name_len = name_raw.iter().position(|&b| b == 0).unwrap_or(name_raw.len());
    let name: String = name_raw[..name_len].iter().map(|&b| b as char).collect();
    let base = if extended { (name_at + name_len + 1 + 15) & !15 } else { 0x80 };
    let activities = if conditional {
        let n = *d.get(0x84).ok_or(Error::Truncated("pgeo activities"))? as usize;
        Some(d.get(0x85..0x85 + n.min(0x13)).ok_or(Error::Truncated("pgeo activities"))?.to_vec())
    } else {
        None
    };
    let mut geo = ProcGeo { version, name, bbox_min, bbox_max, models: Vec::new(), placements: Vec::new(), draws: Vec::new(), activities };
    if kind_id != 7 || !geo.kind().eq_ignore_ascii_case("Models") {
        return Ok(geo);
    }
    let inst_size = r.u32(0x50)? as usize;
    let n_models = r.u32(0x54)? as usize;
    if n_models > 10_000 || !inst_size.is_multiple_of(40) || inst_size > d.len() {
        return Err(Error::Unsupported(format!("pgeo: {n_models} models, instance block {inst_size}")));
    }
    // Models and their file references.
    let mut counts = Vec::with_capacity(n_models);
    for i in 0..n_models {
        let m = base + i * 0x40;
        counts.push([r.u32(m + 0x24)? as usize, r.u32(m + 0x2C)? as usize, r.u32(m + 0x34)? as usize]);
        geo.models.push(GeoModel { bounds_min: r.vec3(m)?, bounds_max: r.vec3(m + 0x10)?, lod0: Vec::new(), lod1: Vec::new(), lod2: Vec::new() });
    }
    let mut o = base + n_models * 0x40;
    for (i, c) in counts.iter().enumerate() {
        for (lod, &n) in c.iter().enumerate() {
            for _ in 0..n {
                let v = r.u32(o)?;
                o += 4;
                let m = &mut geo.models[i];
                [&mut m.lod0, &mut m.lod1, &mut m.lod2][lod].push(v);
            }
        }
    }
    // The (u32, draw count, block size) triple that precedes the draw entries.
    let mut p = o;
    let draws_at = loop {
        if p + 12 > d.len().saturating_sub(inst_size) || p > o + 8192 {
            return Err(Error::Unsupported("pgeo: draw table not found".into()));
        }
        let nd = r.u32(p + 4)?;
        if r.u32(p + 8)? as usize == inst_size && nd > 0 && nd < 10_000 {
            break p + 12;
        }
        p += 4;
    };
    let n_draws = r.u32(draws_at - 8)? as usize;
    let blk = d.len() - inst_size;
    let (min, max) = (bbox_min, bbox_max);
    let mut q = draws_at + n_draws * 0x5C;
    let mut ci = 0usize;
    for k in 0..n_draws {
        let e = draws_at + k * 0x5C;
        let model = r.u32(e + 0x2C)? as usize;
        let full = r.u32(e + 0x30)? as usize;
        let compact = r.u32(e + 0x34)? as usize;
        let lods = r.u32(e + 0x38)? as usize;
        let (size_z, size_y) = (r.f32(e + 0x18)?, r.f32(e + 0x1C)?);
        if model >= n_models {
            return Err(Error::Unsupported(format!("pgeo: draw {k} uses model {model} of {n_models}")));
        }
        // Full records are 16-byte aligned; LOD tables follow without alignment (verified: with
        // this rule every file's walk ends exactly at the instance block).
        if full > 0 {
            q = (q + 15) & !15;
        }
        for i in 0..full {
            let a = q + i * 0x60;
            geo.placements.push(Placement {
                model,
                x_axis: r.vec3(a)?,
                y_axis: r.vec3(a + 0x10)?,
                z_axis: r.vec3(a + 0x20)?,
                position: r.vec3(a + 0x30)?,
                full: true,
                // +0x50 holds the same packed vector as compact word 3 (UNVERIFIED for full records:
                // used only when it decodes to unit length); no tint known.
                normal: unit_10_10_10(r.u32(a + 0x50)?).unwrap_or(DEFAULT_NORMAL),
                tint: DEFAULT_TINT,
            });
        }
        q += full * 0x60;
        let mut lod_tables = Vec::with_capacity(lods);
        for t in 0..lods {
            let mut v = [0f32; 8];
            for (j, x) in v.iter_mut().enumerate() {
                *x = r.f32(q + t * 0x20 + j * 4)?;
            }
            lod_tables.push(v);
        }
        q += lods * 0x20;
        geo.draws.push(Draw { model, full, compact, lod_tables, distances: [r.f32(e + 0x3C)?, r.f32(e + 0x40)?, r.f32(e + 0x44)?] });
        for _ in 0..compact {
            let a = blk + ci * 40;
            ci += 1;
            let w0 = r.u32(a)?;
            let (qx, qz, qy) = (w0 & 0x7FF, (w0 >> 11) & 0x7FF, w0 >> 22);
            let position = [
                min[0] + qx as f32 / 2047.0 * (max[0] - min[0]),
                min[1] + qy as f32 / 1023.0 * (max[1] - min[1]),
                min[2] + qz as f32 / 2047.0 * (max[2] - min[2]),
            ];
            let axis = |w: u32| norm([sfield(w, 0, 11) as f32 / 1023.0, sfield(w, 11, 11) as f32 / 1023.0, sfield(w, 22, 10) as f32 / 511.0]);
            let y = axis(r.u32(a + 4)?);
            let x = axis(r.u32(a + 8)?);
            let z = norm(cross(x, y));
            // Uniform scale from the culling sizes (h1 = size_y * |row1|, h0 = size_z * |row2| / 2).
            let (h0, h1) = (r.f16(a + 0x18)?, r.f16(a + 0x1A)?);
            let sy = if size_y > 0.0 { h1 / size_y } else { 1.0 };
            let sz = if size_z > 0.0 { 2.0 * h0 / size_z } else { sy };
            let normal = unit_10_10_10(r.u32(a + 12)?).unwrap_or(DEFAULT_NORMAL);
            let tint = r.u32(a + 32)?;
            geo.placements.push(Placement { model, x_axis: scale(x, sz), y_axis: scale(y, sy), z_axis: scale(z, sz), position, full: false, normal, tint });
        }
    }
    if ci * 40 != inst_size {
        return Err(Error::Unsupported(format!("pgeo: {ci} compact instances for a {inst_size}-byte block")));
    }
    Ok(geo)
}

/// The model number (`<track>out.%05d.rmb.bin`) of every record in a track `.pvs`'s 18-byte table,
/// which is what `.pgeo` model references index.
pub fn pvs_model_numbers(pvs: &[u8]) -> Result<Vec<u16>, Error> {
    Ok(pvs_records(pvs)?.iter().map(|r| u16::from_be_bytes([r[0], r[1]])).collect())
}

/// How a `CollObjs.xml` type was matched to its whole-object template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollObjMatch {
    /// Aligned in order with an equal object count (the strong check).
    Count,
    /// Out of order, matched by name with an equal object count.
    NameAndCount,
    /// Out of order, the only unused template with an equal object count near the type's place in
    /// the order (no name evidence; UNVERIFIED).
    CountNearOrder,
    /// Matched by name only (the template has no 0x08 records; UNVERIFIED).
    NameOnly,
    /// Corrected from the zone instances at the objects' positions ([`refine_collobj_templates`]).
    Zones,
}

fn name_tokens(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_ascii_alphabetic())
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() > 2 && !matches!(t.as_str(), "clrd" | "obj" | "lod" | "nolod" | "co"))
        .collect()
}

/// Maps each `CollObjs.xml` type (`CO_Bench_001`) to its whole-object template model number.
///
/// The `.pvs` record table holds one flag-0x08 record per placed smashable object, pointing at the
/// type's whole-object template (verified: ArmcoArrow 139 = template 3, Bench 45 = 6, BinE 24 =
/// 23, ...). Types sorted case-insensitively (lowercase) own increasing templates, so types are
/// aligned to templates in order, requiring equal object counts; the few out-of-order types are
/// then matched by name (`template_name` = the template's submodel names). Colorado: 176 of 180 by
/// count, 3 by name and count, 1 by name only.
pub fn collobj_templates(
    objs: &[XmlPlacement],
    pvs: &[u8],
    template_name: impl Fn(u16) -> Option<String>,
) -> Result<std::collections::BTreeMap<String, (u16, CollObjMatch)>, Error> {
    use std::collections::{BTreeMap, HashMap, HashSet};
    let recs = pvs_records(pvs)?;
    // Templates come before the scenery models; record 0 is the first scenery model.
    let first_scenery = recs.first().map_or(u16::MAX, |r| u16::from_be_bytes([r[0], r[1]]));
    let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
    for r in &recs {
        let m = u16::from_be_bytes([r[0], r[1]]);
        if r[4] == 0x08 && m < first_scenery {
            *counts.entry(m).or_default() += 1;
        }
    }
    // Objects per type = placements of its most-placed part (each part is listed per object).
    let mut parts: HashMap<&str, usize> = HashMap::new();
    for o in objs {
        *parts.entry(o.kind.as_str()).or_default() += 1;
    }
    let mut objects: HashMap<String, usize> = HashMap::new();
    for (k, v) in parts {
        let e = objects.entry(k.split('.').next().unwrap_or(k).to_owned()).or_default();
        *e = (*e).max(v);
    }
    let mut types: Vec<String> = objects.keys().cloned().collect();
    types.sort_by_key(|t| t.to_ascii_lowercase());
    let models: Vec<u16> = counts.keys().copied().collect();
    let names: HashMap<u16, HashSet<String>> = models.iter().map(|&m| (m, template_name(m).map(|n| name_tokens(&n)).unwrap_or_default())).collect();
    let sim = |t: &str, m: u16| name_tokens(t).intersection(&names[&m]).count() as i64;
    // DP alignment: skip templates freely, skip types at a cost, match only on equal counts.
    let (nt, nm) = (types.len(), models.len());
    const NEG: i64 = i64::MIN / 4;
    let mut dp = vec![vec![NEG; nm + 1]; nt + 1];
    let mut back = vec![vec![0u8; nm + 1]; nt + 1];
    dp[0].fill(0);
    for i in 1..=nt {
        dp[i][0] = dp[i - 1][0] - 5;
        back[i][0] = 2;
        for j in 1..=nm {
            let (mut best, mut arg) = (dp[i][j - 1], 0u8);
            if dp[i - 1][j] - 5 > best {
                (best, arg) = (dp[i - 1][j] - 5, 2);
            }
            if objects[&types[i - 1]] == counts[&models[j - 1]] {
                let v = dp[i - 1][j - 1] + 10 + sim(&types[i - 1], models[j - 1]);
                if v > best {
                    (best, arg) = (v, 1);
                }
            }
            dp[i][j] = best;
            back[i][j] = arg;
        }
    }
    let mut out = BTreeMap::new();
    let (mut i, mut j) = (nt, nm);
    while i > 0 {
        match back[i][j] {
            1 => {
                out.insert(types[i - 1].clone(), (models[j - 1], CollObjMatch::Count));
                i -= 1;
                j -= 1;
            }
            2 => i -= 1,
            _ => j -= 1,
        }
    }
    // Leftovers (out-of-order types), weakest evidence last.
    let mut used: HashSet<u16> = out.values().map(|v| v.0).collect();
    // Name score: type tokens found inside the template's names (letters only, lowercase).
    let score = |t: &str, m: u16| -> usize {
        let Some(n) = template_name(m) else { return 0 };
        let flat: String = n.chars().filter(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
        name_tokens(t).iter().filter(|k| flat.contains(k.as_str())).count()
    };
    for (ti, t) in types.iter().enumerate() {
        if out.contains_key(t) {
            continue;
        }
        let same_count: Vec<u16> = models.iter().copied().filter(|m| !used.contains(m) && counts[m] == objects[t]).collect();
        let pick = if let Some(m) = same_count.iter().copied().max_by_key(|&m| score(t, m)).filter(|&m| score(t, m) > 0) {
            Some((m, CollObjMatch::NameAndCount))
        } else if !same_count.is_empty() {
            // Where the type would sit: between its aligned neighbours' templates.
            let prev = types[..ti].iter().rev().find_map(|x| out.get(x)).map_or(0, |v| v.0 as i64);
            let next = types[ti + 1..].iter().find_map(|x| out.get(x)).map_or(first_scenery as i64, |v| v.0 as i64);
            let mid = (prev + next) / 2;
            same_count.iter().copied().min_by_key(|&m| (m as i64 - mid).abs()).map(|m| (m, CollObjMatch::CountNearOrder))
        } else {
            let need = name_tokens(t).len().max(1);
            (0..first_scenery).filter(|m| !used.contains(m)).max_by_key(|&m| score(t, m)).filter(|&m| score(t, m) >= need).map(|m| (m, CollObjMatch::NameOnly))
        };
        if let Some(p) = pick {
            used.insert(p.0);
            out.insert(t.clone(), p);
        }
    }
    Ok(out)
}

/// One placement per `CollObjs.xml` object (its parts share a transform), with the type's template.
pub fn collobj_placements(objs: &[XmlPlacement], map: &std::collections::BTreeMap<String, (u16, CollObjMatch)>) -> Vec<TrackPlacement> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for o in objs {
        let t = o.kind.split('.').next().unwrap_or(&o.kind);
        let Some(&(model_number, _)) = map.get(t) else { continue };
        if !seen.insert((t.to_owned(), o.position.map(|v| (v * 100.0).round() as i64))) {
            continue;
        }
        let p = Placement { model: 0, x_axis: o.x_axis, y_axis: o.y_axis, z_axis: o.z_axis, position: o.position, full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT };
        out.push(TrackPlacement { model_number, lod1: None, matrix: p.engine_matrix(), full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT, lightmaps: [NO_LIGHTMAP; 2] });
    }
    out
}

/// The 18-byte record table of a track `.pvs`: first u16 = model number, byte 4 = flags (0x08 on
/// records of placed smashable objects, one per object, see [`collobj_templates`]).
pub fn pvs_records(pvs: &[u8]) -> Result<Vec<[u8; 18]>, Error> {
    let u = |o: usize| -> Result<u32, Error> { pvs.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("pvs")) };
    if pvs.get(..4) != Some(b"FPVS") {
        return Err(Error::BadMagic("FPVS"));
    }
    // Same header walk as `pvs::parse`: offsets list, 7 bytes, textures, shaders.
    let mut p = 32;
    p += 4 + 4 * u(p)? as usize + 7;
    p += 4 + 28 * u(p)? as usize;
    let shaders = u(p)? as usize;
    p += 4;
    for _ in 0..shaders {
        p += 4 + u(p)? as usize;
    }
    let n = u(p)? as usize;
    p += 4;
    let table = pvs.get(p..p + n * 18).ok_or(Error::Truncated("pvs 18-byte table"))?;
    Ok(table.as_chunks::<18>().0.to_vec())
}

/// A placement resolved to its template, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackPlacement {
    /// LOD0 template: `<track>out.%05d.rmb.bin`.
    pub model_number: u16,
    /// LOD1 template, if the model has one.
    pub lod1: Option<u16>,
    /// Column-major engine-space matrix ([`Placement::engine_matrix`]).
    pub matrix: [f32; 16],
    /// From an exact float matrix (full record) rather than a quantized compact record.
    pub full: bool,
    /// Ground normal in engine space (Z negated from [`Placement::normal`]).
    pub normal: [f32; 3],
    /// Instance tint (D3DCOLOR, [`Placement::tint`]).
    pub tint: u32,
    /// Night lightmaps of the LOD0 / LOD1 draw records (`.pvs` record u32 @8, a PVS texture index, when u32 @12
    /// names a list position; docs/LIGHTMAPS.md "Per-instance lightmaps"); [`NO_LIGHTMAP`] = none. Only
    /// `.pvsz` zone instances carry them.
    pub lightmaps: [u32; 2],
}

/// [`TrackPlacement::lightmaps`] entry for "no per-instance lightmap".
pub const NO_LIGHTMAP: u32 = u32::MAX;

/// Every free-roam `Models` placement of a track: parses each `.pgeo` in `bin.zip` once (the
/// archive duplicates them per streaming block) and resolves its models through the track `.pvs`.
/// Includes the activity-conditional groups that list activity 0 (free roam); event-only groups
/// come from [`track_event_groups`]. Placements whose model has no reference are skipped and
/// counted in the second value.
pub fn track_placements<R: std::io::Read + std::io::Seek>(ar: &mut crate::zip::Archive<R>, pvs: &[u8]) -> Result<(Vec<TrackPlacement>, usize), Error> {
    let (mut out, mut skipped) = (Vec::new(), 0);
    for (geo, placed, s) in resolved_groups(ar, pvs)? {
        if geo.in_free_roam() {
            out.extend(placed);
            skipped += s;
        }
    }
    Ok((out, skipped))
}

/// An event-only `Models` group (activity-conditional, without activity 0): barriers, festival
/// dressing and event tree sets, shown while one of `activities` runs.
#[derive(Debug, Clone)]
pub struct EventGroup {
    pub name: String,
    pub activities: Vec<u8>,
    pub placements: Vec<TrackPlacement>,
}

/// The track's event-only `Models` groups (see [`EventGroup`]).
pub fn track_event_groups<R: std::io::Read + std::io::Seek>(ar: &mut crate::zip::Archive<R>, pvs: &[u8]) -> Result<Vec<EventGroup>, Error> {
    Ok(resolved_groups(ar, pvs)?
        .into_iter()
        .filter(|(g, _, _)| !g.in_free_roam())
        .map(|(g, placements, _)| EventGroup { name: g.name.clone(), activities: g.activities.clone().unwrap_or_default(), placements })
        .collect())
}

/// Every `Models` group with its resolved placements and the count of unresolved ones.
fn resolved_groups<R: std::io::Read + std::io::Seek>(ar: &mut crate::zip::Archive<R>, pvs: &[u8]) -> Result<Vec<(ProcGeo, Vec<TrackPlacement>, usize)>, Error> {
    let numbers = pvs_model_numbers(pvs)?;
    let mut seen = std::collections::HashSet::new();
    let pgeos: Vec<crate::zip::Entry> =
        ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pgeo") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    let mut groups = Vec::new();
    for e in pgeos {
        let geo = parse_pgeo(&ar.read(&e)?)?;
        let (mut out, mut skipped) = (Vec::new(), 0);
        let num = |r: Option<&u32>| r.and_then(|&r| numbers.get(r as usize).copied());
        let models: Vec<(Option<u16>, Option<u16>)> = geo.models.iter().map(|m| (num(m.lod0.first()), num(m.lod1.first()))).collect();
        for p in &geo.placements {
            match models[p.model] {
                (Some(model_number), lod1) => out.push(TrackPlacement {
                    model_number,
                    lod1,
                    matrix: p.engine_matrix(),
                    full: p.full,
                    normal: [p.normal[0], p.normal[1], -p.normal[2]],
                    tint: p.tint,
                    lightmaps: [NO_LIGHTMAP; 2],
                }),
                (None, _) => skipped += 1,
            }
        }
        if !geo.placements.is_empty() || skipped > 0 {
            groups.push((geo, out, skipped));
        }
    }
    Ok(groups)
}

/// The game's draw distances for one LOD0 template, from its `.pgeo` draw entries.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateDistances {
    /// LOD1 / LOD2 templates (`None` when the model has no such LOD or repeats the LOD0 one).
    pub lod1_model: Option<u16>,
    pub lod2_model: Option<u16>,
    /// Switch to LOD1 / LOD2 beyond these distances (metres), when the table has them.
    pub lod1_m: Option<f32>,
    pub lod2_m: Option<f32>,
    /// Not drawn beyond this distance.
    pub cull_m: f32,
    /// From a LOD table read in the culling code (`true`), or the draw's +0x44 value used as a
    /// fallback for compact-only draws with all-zero tables (`false`, INFERRED).
    pub from_lod_table: bool,
}

/// Per LOD0 template: the most common main-view LOD table among its draws (LOD switch and cull
/// distances, see [`Draw::lod_tables`]); templates whose draws only have all-zero tables fall back
/// to the largest draw +0x44 value as the cull distance.
pub fn track_template_distances<R: std::io::Read + std::io::Seek>(
    ar: &mut crate::zip::Archive<R>,
    pvs: &[u8],
) -> Result<std::collections::BTreeMap<u16, TemplateDistances>, Error> {
    use std::collections::{BTreeMap, HashMap, HashSet};
    let numbers = pvs_model_numbers(pvs)?;
    let mut seen = HashSet::new();
    let pgeos: Vec<crate::zip::Entry> =
        ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pgeo") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    // template -> (table (t0, t1, t2) bits -> uses, lod1, lod2, fallback cull)
    type Acc = (HashMap<[u32; 3], usize>, Option<u16>, Option<u16>, f32);
    let mut acc: BTreeMap<u16, Acc> = BTreeMap::new();
    for e in pgeos {
        let geo = parse_pgeo(&ar.read(&e)?)?;
        let num = |r: Option<&u32>| r.and_then(|&r| numbers.get(r as usize).copied());
        for d in &geo.draws {
            let m = &geo.models[d.model];
            let Some(n0) = num(m.lod0.first()) else { continue };
            let (n1, n2) = (num(m.lod1.first()).filter(|&n| n != n0), num(m.lod2.first()).filter(|&n| n != n0));
            let a = acc.entry(n0).or_insert_with(|| (HashMap::new(), n1, n2, 0.0));
            a.1 = a.1.or(n1);
            a.2 = a.2.or(n2);
            a.3 = a.3.max(d.distances[2]);
            for t in d.lod_tables.iter().filter(|t| t[2] > 0.0) {
                *a.0.entry([t[0].to_bits(), t[1].to_bits(), t[2].to_bits()]).or_default() += d.full + d.compact;
            }
        }
    }
    Ok(acc
        .into_iter()
        .map(|(n, (tables, lod1_model, lod2_model, fallback))| {
            let best = tables.into_iter().max_by_key(|&(k, v)| (v, k)).map(|(k, _)| k.map(f32::from_bits));
            let pos = |x: f32| (x > 0.0).then_some(x);
            let d = match best {
                Some(t) => TemplateDistances { lod1_model, lod2_model, lod1_m: pos(t[0]), lod2_m: pos(t[1]), cull_m: t[2], from_lod_table: true },
                None => TemplateDistances { lod1_model, lod2_model, lod1_m: None, lod2_m: None, cull_m: fallback, from_lod_table: false },
            };
            (n, d)
        })
        .collect())
}

/// Whether a template's bounds match a model's (either Z orientation: `.rmb.bin` is engine
/// space, the `.pgeo` collision space), within `tol` metres.
pub fn bounds_match(model: &GeoModel, rmb_min: [f32; 3], rmb_max: [f32; 3], tol: f32) -> bool {
    let close = |a: f32, b: f32| (a - b).abs() <= tol;
    let xy = close(model.bounds_min[0], rmb_min[0]) && close(model.bounds_max[0], rmb_max[0]) && close(model.bounds_min[1], rmb_min[1]) && close(model.bounds_max[1], rmb_max[1]);
    let z_same = close(model.bounds_min[2], rmb_min[2]) && close(model.bounds_max[2], rmb_max[2]);
    let z_flip = close(model.bounds_min[2], -rmb_max[2]) && close(model.bounds_max[2], -rmb_min[2]);
    xy && (z_same || z_flip)
}

/// A `CollObjs.xml` / `GameObjs.xml` placement (collision space).
#[derive(Debug, Clone, PartialEq)]
pub struct XmlPlacement {
    /// `PhysicsType` (`CO_Bench_001.3.rmb`) or `GameplayID` (`BF_CUDA_426BF_CLOSEDC`).
    pub kind: String,
    pub position: [f32; 3],
    pub x_axis: [f32; 3],
    pub y_axis: [f32; 3],
    pub z_axis: [f32; 3],
}

/// Parses the `<ObjN ...><Pos x y z/><Orientation><XAxis/><YAxis/><ZAxis/></Orientation></ObjN>`
/// placements of `CollObjs.xml` (`PhysicsType`) or `GameObjs.xml` (`GameplayID`).
pub fn parse_obj_xml(xml: &str) -> Vec<XmlPlacement> {
    let attr = |tag: &str, key: &str| -> Option<String> {
        let i = tag.find(&format!("{key}=\""))? + key.len() + 2;
        Some(tag[i..i + tag[i..].find('"')?].to_owned())
    };
    let vec = |block: &str, tag: &str| -> [f32; 3] {
        let Some(i) = block.find(&format!("<{tag} ")) else { return [0.0; 3] };
        let t = &block[i..i + block[i..].find('>').unwrap_or(0)];
        let g = |k: &str| attr(t, k).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        [g("x"), g("y"), g("z")]
    };
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<Obj") {
        let after = &rest[i..];
        let tag_end = after.find('>').unwrap_or(after.len());
        let tag = &after[..tag_end];
        let close = after.find("</Obj").unwrap_or(after.len());
        let block = &after[..close];
        if let Some(kind) = attr(tag, "PhysicsType").or_else(|| attr(tag, "GameplayID")) {
            out.push(XmlPlacement {
                kind,
                position: vec(block, "Pos"),
                x_axis: vec(block, "XAxis"),
                y_axis: vec(block, "YAxis"),
                z_axis: vec(block, "ZAxis"),
            });
        }
        rest = &after[close.max(1)..];
    }
    out
}

// ---- Animated objects (`.pgeo` types 4 and 5) ---------------------------------------------------

/// A `CProceduralAnimatedObject` (`.pgeo` type 4, `Anim_ANIM_*`, 33 in Colorado): an embedded,
/// uncompressed Granny3D file (skeleton, meshes, animation curves; `ArtToolInfo`, `Skeletons`,
/// `TrackGroups`...). Not decoded beyond its string table: for some objects (windmills, tractor,
/// blimp) the mesh names are those of the track's `.rmb.bin` templates; the others (fair rides,
/// fireworks, birds, stage lights) appear to carry their geometry inside the Granny data.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimObject {
    pub name: String,
    /// Strings of the Granny data that look like LOD mesh names (`..._LOD00_`, `..._LOD01`).
    pub meshes: Vec<String>,
}

/// One instance in an animated scene.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimInstance {
    /// Index into [`TrackAnim::objects`].
    pub object: usize,
    /// Collision-space transform (the record's 4x4 matrix, rows = axes, row 3 = position, w = 1).
    pub placement: Placement,
    /// Record floats @+0x44..0x50, e.g. (300, 800, 1200) or (100, 200, 300); read as LOD1 / LOD2 /
    /// cull distances by analogy with the `Models` LOD tables (UNVERIFIED).
    pub distances: [f32; 3],
}

/// A `CProceduralAnimatedScene` (`.pgeo` type 5, `anim_proc_clrd_*`): instances of animated
/// objects. Layout (VERIFIED on all 202 Colorado files: every walk ends exactly at the file end and
/// every object index is in range): after the NUL-terminated name, 4-byte aligned, `n` pairs
/// `(object id u32, 0)` then `(instance count u32, 0)`, two runtime words, two pointers and a
/// float; then, 16-byte aligned, 0x80-byte instance records: 4x4 matrix, u32 index into the
/// scene's object list @+0x40, three distances, flags. The object id is the index of the type-4
/// file in `bin.zip` name order (`festival_combineharvester` -> 5 = `Anim_ANIM_CombineHarvester`).
#[derive(Debug, Clone, PartialEq)]
pub struct AnimScene {
    pub name: String,
    /// As [`ProcGeo::activities`]: `None` = always present.
    pub activities: Option<Vec<u8>>,
    pub instances: Vec<AnimInstance>,
}

impl AnimScene {
    pub fn in_free_roam(&self) -> bool {
        self.activities.as_ref().is_none_or(|a| a.contains(&0))
    }
}

/// A track's animated objects and scenes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackAnim {
    pub objects: Vec<AnimObject>,
    pub scenes: Vec<AnimScene>,
}

/// A type-4 `.pgeo`: its name and the mesh-like names of its Granny string table.
pub fn anim_object(d: &[u8]) -> Result<AnimObject, Error> {
    let geo = parse_pgeo(d)?;
    let mut meshes = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let j = d[i..].iter().position(|b| !(0x20..0x7F).contains(b)).map_or(d.len(), |k| i + k);
        if j - i >= 6 && d.get(j) == Some(&0) {
            let s: String = d[i..j].iter().map(|&b| b as char).collect();
            if s.contains("_LOD0") && !meshes.contains(&s) {
                meshes.push(s);
            }
        }
        i = j + 1;
    }
    Ok(AnimObject { name: geo.name, meshes })
}

/// Parses a type-5 animated scene; `Ok(None)` for other kinds.
pub fn parse_anim_scene(d: &[u8]) -> Result<Option<AnimScene>, Error> {
    let r = R(d);
    let geo = parse_pgeo(d)?;
    if r.u32(0x30)? != 5 {
        return Ok(None);
    }
    // Scene names run past the 32-byte field of ordinary groups (`..._combineharvester_1`).
    let name_at = if geo.activities.is_some() { 0x98 } else { 0x60 };
    let name_end = d.get(name_at..).and_then(|t| t.iter().position(|&b| b == 0)).ok_or(Error::Truncated("anim scene name"))? + name_at;
    let name: String = d[name_at..name_end].iter().map(|&b| b as char).collect();
    let o = (name_end + 1 + 3) & !3;
    for n in 1..64usize {
        let count = r.u32(o + 8 * n)? as usize;
        let rec = (o + 8 * n + 28 + 15) & !15;
        if rec.checked_add(count.saturating_mul(0x80)) != Some(d.len()) {
            continue;
        }
        let objects: Vec<u32> = (0..n).map(|i| r.u32(o + 8 * i)).collect::<Result<_, _>>()?;
        let mut instances = Vec::with_capacity(count);
        for i in 0..count {
            let a = rec + i * 0x80;
            let k = r.u32(a + 0x40)? as usize;
            let object = *objects.get(k).ok_or_else(|| Error::Unsupported(format!("anim scene: object {k} of {n}")))? as usize;
            let placement = Placement { model: k, x_axis: r.vec3(a)?, y_axis: r.vec3(a + 0x10)?, z_axis: r.vec3(a + 0x20)?, position: r.vec3(a + 0x30)?, full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT };
            instances.push(AnimInstance { object, placement, distances: [r.f32(a + 0x44)?, r.f32(a + 0x48)?, r.f32(a + 0x4C)?] });
        }
        return Ok(Some(AnimScene { name, activities: geo.activities, instances }));
    }
    Err(Error::Unsupported("anim scene: no record layout fits the file size".into()))
}

/// Every animated object (type 4, in `bin.zip` name order = the scenes' object ids) and scene
/// (type 5) of a track.
pub fn track_anim<R: std::io::Read + std::io::Seek>(ar: &mut crate::zip::Archive<R>) -> Result<TrackAnim, Error> {
    let mut seen = std::collections::HashSet::new();
    let mut pgeos: Vec<crate::zip::Entry> =
        ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pgeo") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    pgeos.sort_by_key(|e| e.name.to_ascii_lowercase());
    let mut out = TrackAnim::default();
    for e in pgeos {
        let d = ar.read(&e)?;
        match d.get(0x30..0x34).map(|b| u32::from_be_bytes(b.try_into().unwrap())) {
            Some(4) => out.objects.push(anim_object(&d)?),
            Some(5) => out.scenes.extend(parse_anim_scene(&d)?),
            _ => {}
        }
    }
    if let Some(bad) = out.scenes.iter().flat_map(|s| &s.instances).find(|i| i.object >= out.objects.len()) {
        return Err(Error::Unsupported(format!("anim scene uses object {} of {}", bad.object, out.objects.len())));
    }
    Ok(out)
}

/// Submodel name -> template number for `_LOD0` names that `want` accepts: scans the raw bytes of
/// every `<track>out.%05d.rmb.bin` for length-prefixed names (u32 BE length + printable bytes)
/// containing `_LOD0` and confirms hits with `rmb::parse` (much faster than parsing every file).
pub fn template_name_index<R: std::io::Read + std::io::Seek>(
    ar: &mut crate::zip::Archive<R>,
    want: impl Fn(&str) -> bool,
) -> Result<std::collections::HashMap<String, u16>, Error> {
    let mut out = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for e in ar.entries.clone() {
        let name = e.name.to_ascii_lowercase();
        let Some(n) = name.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<u16>().ok()) else { continue };
        if !seen.insert(name) {
            continue;
        }
        let d = ar.read(&e)?;
        let printable = |b: &u8| (0x21..0x7F).contains(b);
        let hit = d.windows(5).enumerate().filter(|(_, w)| *w == b"_LOD0").any(|(i, _)| {
            let start = d[..i].iter().rposition(|b| !printable(b)).map_or(0, |k| k + 1);
            let end = d[i..].iter().position(|b| !printable(b)).map_or(d.len(), |k| i + k);
            start >= 4
                && u32::from_be_bytes(d[start - 4..start].try_into().unwrap()) as usize == end - start
                && want(&String::from_utf8_lossy(&d[start..end]))
        });
        if !hit {
            continue;
        }
        if let Ok(m) = crate::rmb::parse(&d) {
            for s in m.submodels.iter().filter(|s| want(&s.name)) {
                out.entry(s.name.clone()).or_insert(n);
            }
        }
    }
    Ok(out)
}

/// The (LOD0, LOD1) templates an animated object is drawn with at rest: one per distinct template
/// its `_LOD00` mesh names resolve to, the LOD1 being the template of the same name with `_LOD01`.
/// Parts are modelled in object space (VERIFIED by bounds: the combine harvester's reel 6623 sits
/// at the front of its body 6621), so each goes at the instance transform. Meshes that resolve to
/// no template (rides, the wind turbine, the harvester's other wheels) are skipped.
pub fn anim_object_templates(o: &AnimObject, lookup: impl Fn(&str) -> Option<u16>) -> Vec<(u16, Option<u16>)> {
    let mut out: Vec<(u16, Option<u16>)> = Vec::new();
    for m in o.meshes.iter().filter(|m| m.contains("_LOD00")) {
        let Some(t) = lookup(m) else { continue };
        if out.iter().any(|&(x, _)| x == t) {
            continue;
        }
        out.push((t, lookup(&m.replacen("_LOD00", "_LOD01", 1)).filter(|&l| l != t)));
    }
    out
}

/// Free-roam animated instances as static placements of their rest-pose templates
/// ([`anim_object_templates`]; no animation yet).
pub fn anim_placements(anim: &TrackAnim, lookup: impl Fn(&str) -> Option<u16>) -> Vec<TrackPlacement> {
    let templates: Vec<Vec<(u16, Option<u16>)>> = anim.objects.iter().map(|o| anim_object_templates(o, &lookup)).collect();
    anim.scenes
        .iter()
        .filter(|s| s.in_free_roam())
        .flat_map(|s| &s.instances)
        .flat_map(|i| {
            let matrix = i.placement.engine_matrix();
            templates[i.object].iter().map(move |&(model_number, lod1)| TrackPlacement { model_number, lod1, matrix, full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT, lightmaps: [NO_LIGHTMAP; 2] })
        })
        .collect()
}

/// LOD / cull distances of the rest-pose animated templates, from their instance records (the
/// most common triple per template; UNVERIFIED reading, see [`AnimInstance::distances`]).
pub fn anim_template_distances(anim: &TrackAnim, lookup: impl Fn(&str) -> Option<u16>) -> std::collections::HashMap<u16, TemplateDistances> {
    type Votes = std::collections::HashMap<[u32; 3], usize>;
    let mut votes: std::collections::HashMap<u16, (Option<u16>, Votes)> = Default::default();
    for i in anim.scenes.iter().flat_map(|s| &s.instances) {
        for (t, lod1) in anim_object_templates(&anim.objects[i.object], &lookup) {
            let v = votes.entry(t).or_insert((lod1, Default::default()));
            *v.1.entry(i.distances.map(f32::to_bits)).or_default() += 1;
        }
    }
    votes
        .into_iter()
        .filter_map(|(t, (lod1, d))| {
            let best = d.into_iter().max_by_key(|&(k, n)| (n, k))?.0.map(f32::from_bits);
            Some((t, TemplateDistances { lod1_model: lod1, lod2_model: None, lod1_m: lod1.map(|_| best[0]), lod2_m: None, cull_m: best[2], from_lod_table: false }))
        })
        .collect()
}

// ---- GameObjs.xml objects -----------------------------------------------------------------------

/// `GameObjs.xml` GameplayID prefix -> whole-object template (LOD0, LOD1) for the objects shown in
/// free roam. VERIFIED by count: each template's `.pvs` 0x08 (smashable) records equal the number
/// of GameObjs entries with that prefix: flyer 100 -> 673 `O_FEST_EquipSign` (the discount-sign
/// boards), speed 44 -> 677 `O_CO_FEST_SpeedCamera_001`, average 36 -> 683
/// `O_CO_Fest_SpeedCamera_Average`, BARNFIND 9 -> 1141 `Barnfind_Barn_LOD00_` (its LOD1 1142 / LOD2
/// 1143). The `BARNFIND_` transform is the barn-find marker, NOT the barn (VERIFIED: it sits ~8 m from the
/// barn with another rotation; the `.pvsz` zones place 1141 with the same axes as its doors, which then land
/// on the gable facade), so [`gameobj_placements`] skips it and the barns come from
/// [`track_zone_instances`]; doors: [`is_closed_barn_door`].
pub const GAMEOBJ_TEMPLATES: [(&str, u16, Option<u16>); 4] = [("flyer_", 673, None), ("speed_", 677, None), ("average_", 683, None), ("BARNFIND_", 1141, None)];

/// Placements of the [`GAMEOBJ_TEMPLATES`] objects from `GameObjs.xml` (parsed with
/// [`parse_obj_xml`]); transforms straight from the XML (collision space).
pub fn gameobj_placements(objs: &[XmlPlacement]) -> Vec<TrackPlacement> {
    objs.iter()
        .filter_map(|o| {
            let &(_, model_number, lod1) = GAMEOBJ_TEMPLATES.iter().find(|(p, _, _)| o.kind.starts_with(p) && *p != "BARNFIND_")?;
            let p = Placement { model: 0, x_axis: o.x_axis, y_axis: o.y_axis, z_axis: o.z_axis, position: o.position, full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT };
            Some(TrackPlacement { model_number, lod1, matrix: p.engine_matrix(), full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT, lightmaps: [NO_LIGHTMAP; 2] })
        })
        .collect()
}

/// When a `CollObjs.xml` / `GameObjs.xml` object exists, from its `.pvsz` zone instance (same template, same
/// position): the XML files hold every object of every event, and the zones carry the activity block.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectCondition {
    /// No block, a block with no activity ids (always present: road signs, mailboxes, speed cameras) or one
    /// listing activity 0 (free roam; discount-sign flyers `[0, 21..31]`). A list without 0 = only during those
    /// events (cones, festival boards and barrels laid out across roads; VERIFIED: e.g. the board + cone
    /// layout in the Red Rock cul-de-sac at (1524, 197, -5338) is `[213, 249]`).
    pub free_roam: bool,
    /// The block's activity ids (empty = no block).
    pub activities: Vec<u8>,
    /// The block's name (empty for most).
    pub name: String,
}

/// Zone-listed placed instances: (model number, collision-space position in 0.1 m cells) -> condition.
pub type ObjectConditions = std::collections::HashMap<(u16, [i32; 3]), ObjectCondition>;

fn condition_cell(p: [f32; 3]) -> [i32; 3] {
    p.map(|v| (v * 10.0).round() as i32)
}

/// Every placed `.pvsz` instance with its activity condition (see [`object_condition`]). An object listed by
/// several zones counts as free roam if any of them says so.
pub fn track_object_conditions<R: std::io::Read + std::io::Seek>(ar: &mut crate::zip::Archive<R>, pvs: &[u8]) -> Result<ObjectConditions, Error> {
    let records = pvs_records(pvs)?;
    let mut seen_files = std::collections::HashSet::new();
    let zones: Vec<crate::zip::Entry> =
        ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pvsz") && seen_files.insert(e.name.to_ascii_lowercase())).cloned().collect();
    let mut out = ObjectConditions::new();
    for z in zones {
        for i in crate::pvsz::parse(&ar.read(&z)?)?.instances {
            let Some(r) = records.get(i.record as usize) else { continue };
            if !i.is_placed() {
                continue;
            }
            let c = match &i.block {
                None => ObjectCondition { free_roam: true, activities: Vec::new(), name: String::new() },
                Some(b) => ObjectCondition { free_roam: b.activities.is_empty() || b.activities.contains(&0), activities: b.activities.clone(), name: b.name.clone() },
            };
            out.entry((u16::from_be_bytes([r[0], r[1]]), condition_cell(i.position)))
                .and_modify(|o| o.free_roam |= c.free_roam)
                .or_insert(c);
        }
    }
    Ok(out)
}

fn condition_cells(position: [f32; 3]) -> impl Iterator<Item = [i32; 3]> {
    let c = condition_cell(position);
    (-1..=1).flat_map(move |dx| (-1..=1).flat_map(move |dy| (-1..=1).map(move |dz| [c[0] + dx, c[1] + dy, c[2] + dz])))
}

/// The zone condition of the object of template `model` at collision-space `position` (within 0.1 m).
pub fn object_condition(conds: &ObjectConditions, model: u16, position: [f32; 3]) -> Option<&ObjectCondition> {
    condition_cells(position).find_map(|k| conds.get(&(model, k)))
}

/// Corrects [`collobj_templates`] from the zones: a type none of whose objects has a zone instance of its
/// template, while every one of them has an instance of the same other template, takes that template
/// (`CollObjMatch::Zones`). Colorado: `CO_CLRD_Gladstone_CliftonValley` 183 -> 179 and
/// `CO_CLRD_Gladstone2_Beaumont4` 179 -> 183 (the order/count guesses had them swapped). Returns
/// `(type, old, new)` per change.
pub fn refine_collobj_templates(
    objs: &[XmlPlacement],
    map: &mut std::collections::BTreeMap<String, (u16, CollObjMatch)>,
    conds: &ObjectConditions,
) -> Vec<(String, u16, u16)> {
    use std::collections::{BTreeMap, HashMap};
    // Type -> (objects, objects matching the mapped template, other template -> objects it is at).
    let mut votes: BTreeMap<String, (usize, usize, HashMap<u16, usize>)> = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut models_at: HashMap<[i32; 3], Vec<u16>> = HashMap::new();
    for ((m, k), _) in conds {
        models_at.entry(*k).or_default().push(*m);
    }
    for o in objs {
        let t = o.kind.split('.').next().unwrap_or(&o.kind);
        let Some(&(n, _)) = map.get(t) else { continue };
        // Parts of one object share its transform: count each object once.
        if !seen.insert((t.to_owned(), condition_cell(o.position))) {
            continue;
        }
        let v = votes.entry(t.to_owned()).or_default();
        v.0 += 1;
        if object_condition(conds, n, o.position).is_some() {
            v.1 += 1;
            continue;
        }
        let mut here: Vec<u16> = condition_cells(o.position).flat_map(|k| models_at.get(&k).into_iter().flatten().copied()).collect();
        here.sort_unstable();
        here.dedup();
        for m in here {
            *v.2.entry(m).or_default() += 1;
        }
    }
    let mut changes = Vec::new();
    for (t, (objects, matched, others)) in votes {
        if matched > 0 {
            continue;
        }
        let Some((&m, _)) = others.iter().find(|&(_, &c)| c == objects) else { continue };
        if others.values().filter(|&&c| c == objects).count() == 1 {
            let e = map.get_mut(&t).unwrap();
            changes.push((t, e.0, m));
            *e = (m, CollObjMatch::Zones);
        }
    }
    changes
}

/// [`collobj_placements`] restricted to the objects the game draws in free roam: those with a zone
/// instance whose condition is free roam ([`ObjectCondition::free_roam`]). Objects without any zone
/// instance are dropped too (INFERRED not drawn: the zones list 97% of the objects, 4,094 of 5,406 as free roam; the rest are the 54
/// `CO_CLRD_Sign_Speed35`, whose template has no draw records, and 78 festival cones/boards/barrels at the
/// airfield (x 5400..5850, z -3800..-3190) with no zone instance within 30 m).
pub fn collobj_free_roam_placements(
    objs: &[XmlPlacement],
    map: &std::collections::BTreeMap<String, (u16, CollObjMatch)>,
    conds: &ObjectConditions,
) -> Vec<TrackPlacement> {
    let kept: Vec<XmlPlacement> = objs
        .iter()
        .filter(|o| {
            let t = o.kind.split('.').next().unwrap_or(&o.kind);
            map.get(t).is_some_and(|&(n, _)| object_condition(conds, n, o.position).is_some_and(|c| c.free_roam))
        })
        .cloned()
        .collect();
    collobj_placements(&kept, map)
}

/// [`gameobj_placements`] restricted to the objects whose zone instance is free roam (all 180 in Colorado:
/// the flyers list `[0, 21..31]`, the speed cameras no activity).
pub fn gameobj_free_roam_placements(objs: &[XmlPlacement], conds: &ObjectConditions) -> Vec<TrackPlacement> {
    let kept: Vec<XmlPlacement> = objs
        .iter()
        .filter(|o| {
            GAMEOBJ_TEMPLATES.iter().find(|(p, _, _)| o.kind.starts_with(p)).is_some_and(|&(_, n, _)| object_condition(conds, n, o.position).is_some_and(|c| c.free_roam))
        })
        .cloned()
        .collect();
    gameobj_placements(&kept)
}

// ---- PVS zone instances (`.pvsz` instance section; exact grammar in `crate::pvsz`) ------------

/// One entry of a `.pvsz` instance section: the transform of the zone's head record with the same
/// index (`head[i]` -> `.pvs` 18-byte record -> model). World-space models have position 0 and axes
/// diag(1, 1, -1); origin-centred templates (festival marquees, grandstands, houses...) carry their
/// world transform here, which is the only placement they have on the disc.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneEntry {
    /// `.pvs` 18-byte record index (head entry, high bit cleared).
    pub record: u32,
    /// Three f16 distances (NaN = none), read as LOD1 / LOD2 / cull (UNVERIFIED).
    pub distances: [f32; 3],
    /// Collision-space transform.
    pub placement: Placement,
    /// The block's activity ids (`None` = no block: always present).
    pub activities: Option<Vec<u8>>,
    /// The block carries a name (event node / GameObj id).
    pub named: bool,
    /// The block's name (empty = none), e.g. `BF_SHE_CobraBF_65_CLOSED`.
    pub name: String,
    /// The block's object type (-1 = none; INFERRED the physics object type).
    pub object_type: i32,
}

impl ZoneEntry {
    /// Shown in free roam: no block, activity 0 without a name (INFERRED), or a barn-find barn's closed
    /// doors ([`is_closed_barn_door`]).
    pub fn in_free_roam(&self) -> bool {
        match &self.activities {
            None => true,
            Some(a) => (a.contains(&0) && !self.named) || is_closed_barn_door(&self.name),
        }
    }
}

impl From<crate::pvsz::Instance> for ZoneEntry {
    fn from(i: crate::pvsz::Instance) -> Self {
        let [x_axis, y_axis, z_axis] = i.placement_axes();
        ZoneEntry {
            record: i.record,
            distances: i.distances,
            placement: Placement { model: 0, x_axis, y_axis, z_axis, position: i.position, full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT },
            object_type: i.block.as_ref().map_or(-1, |b| b.object_type),
            named: i.block.as_ref().is_some_and(|b| !b.name.is_empty()),
            name: i.block.as_ref().map(|b| b.name.clone()).unwrap_or_default(),
            activities: i.block.map(|b| b.activities),
        }
    }
}

/// Barn-find barns (docs/PROPS.md "Barn finds"): each of the 9 has named `.pvsz` instances
/// `BF_<car>_CLOSED` (template 1144, closed doors) and `BF_<car>_OPEN` (1145, open doors) at the same
/// transform, `..._CLOSEDC` / `..._OPENC` (object type 0 / 1, no model: their collision) and
/// `BARNFIND_<car>` (the dusty car inside, own template). None has an activity id. The closed doors are the
/// state before the find, so free roam shows them (INFERRED; the open doors and the car wait for barn-find
/// progress).
pub fn is_closed_barn_door(name: &str) -> bool {
    name.starts_with("BF_") && name.ends_with("_CLOSED")
}

/// Cull distance for zone instances whose last distance is NaN (no cull).
pub const NO_CULL_M: f32 = 20_000.0;

/// The instance section of one `.pvsz` (exact, [`crate::pvsz::parse`]).
pub fn zone_entries(pvsz: &[u8]) -> Result<Vec<ZoneEntry>, Error> {
    Ok(crate::pvsz::parse(pvsz)?.instances.into_iter().map(ZoneEntry::from).collect())
}

/// Per `.pvs` model number: whether its geometry is origin-centred (a template placed by instance),
/// from the model block's centre (floats 7..9 of the 60-byte block; equals the `.rmb.bin` submodel
/// offset, e.g. 1692 -> (0, 2.54, 0.04)): |x|, |z| < 2 m.
pub fn pvs_template_flags(pvs: &[u8]) -> Result<Vec<bool>, Error> {
    let u = |o: usize| -> Result<u32, Error> { pvs.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("pvs")) };
    let n_rec = pvs_records(pvs)?.len();
    let mut p = 32;
    p += 4 + 4 * u(p)? as usize + 7;
    p += 4 + 28 * u(p)? as usize;
    let shaders = u(p)? as usize;
    p += 4;
    for _ in 0..shaders {
        p += 4 + u(p)? as usize;
    }
    p += 4 + 18 * n_rec;
    let n = u(p)? as usize;
    p += 4;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        p += 4 + 4 * u(p)? as usize;
        p += 4 + 4 * u(p)? as usize;
        let f = |k: usize| u(p + 8 + 4 * k).map(f32::from_bits);
        out.push(f(7)?.abs() < 2.0 && f(9)?.abs() < 2.0);
        p += 60;
    }
    Ok(out)
}

/// Every track zone's free-roam placed templates: `.pvsz` entries whose record draws as LOD0 (band
/// bit 3), sits away from the origin (world-space models always have position 0) and is in
/// free roam ([`ZoneEntry::in_free_roam`]), resolved to model numbers, deduplicated across zones by (model,
/// position to 0.1 m), skipping the models `skip` returns true for (those the other placement paths
/// already cover). Returns the placements and, per template, its most common distance triple as
/// `TemplateDistances` (LOD1 / LOD2 models from the record group, switch distances from the triple).
pub fn track_zone_instances<R: std::io::Read + std::io::Seek>(
    ar: &mut crate::zip::Archive<R>,
    pvs: &[u8],
    skip: impl Fn(u16) -> bool,
) -> Result<(Vec<TrackPlacement>, std::collections::BTreeMap<u16, TemplateDistances>), Error> {
    use std::collections::{BTreeMap, HashMap, HashSet};
    let records = pvs_records(pvs)?;
    let model = |i: usize| records.get(i).map(|r| u16::from_be_bytes([r[0], r[1]]));
    let be32 = |r: &[u8; 18], o: usize| u32::from_be_bytes(r[o..o + 4].try_into().unwrap());
    let mut seen_files = HashSet::new();
    let zones: Vec<crate::zip::Entry> =
        ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pvsz") && seen_files.insert(e.name.to_ascii_lowercase())).cloned().collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut votes: HashMap<u16, (Vec<(u16, usize)>, HashMap<[u32; 3], usize>)> = HashMap::new();
    for z in zones {
        let entries = zone_entries(&ar.read(&z)?)?;
        for e in entries {
            let i = e.record as usize;
            let (Some(rec), Some(m)) = (records.get(i), model(i)) else { continue };
            if rec[6] & 0x08 == 0 || skip(m) || !e.in_free_roam() {
                continue;
            }
            let p = e.placement.position;
            if p.iter().map(|v| v.abs()).sum::<f32>() < 0.01 {
                continue;
            }
            if !seen.insert((m, p.map(|v| (v * 10.0).round() as i32))) {
                continue;
            }
            // An object's LOD records are contiguous (LOD00, LOD01, LOD02); the LOD0 record's link
            // points at the last one. Band bits 3/4/5 say which level slots each record fills, and
            // the entry's distances end the slots (VERIFIED on the placed templates: 3-LOD buildings
            // 100 / 250 / 700, `0x72` "last LOD01" fills slots 1 and 2).
            let link = i16::from_be_bytes([rec[2], rec[3]]).max(0) as usize;
            let slot = |k: u32| (i..=i + link).find(|&j| records.get(j).is_some_and(|r| r[6] & (0x08 << k) != 0)).and_then(model);
            let (s1, s2) = (slot(1).unwrap_or(m), slot(2).unwrap_or(m));
            // Each distinct model after LOD0 with the slot where it starts.
            let mut chain: Vec<(u16, usize)> = Vec::new();
            for (k, sm) in [(1, s1), (2, s2)] {
                if sm != m && chain.last().is_none_or(|c| c.0 != sm) {
                    chain.push((sm, k));
                }
            }
            let lod1 = chain.first().map(|c| c.0);
            // Each LOD record names its own lightmap (the LOD00 / LOD01 records alternate textures).
            let lm = |j: usize| records.get(j).map(|r| be32(r, 8)).filter(|_| records.get(j).is_some_and(|r| be32(r, 12) != u32::MAX)).unwrap_or(NO_LIGHTMAP);
            let lod1_record = (i..=i + link).find(|&j| records.get(j).is_some_and(|r| r[6] & 0x10 != 0) && model(j) == lod1);
            let lightmaps = [lm(i), lod1_record.map_or(NO_LIGHTMAP, lm)];
            out.push(TrackPlacement { model_number: m, lod1, matrix: e.placement.engine_matrix(), full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT, lightmaps });
            let v = votes.entry(m).or_insert((chain, HashMap::new()));
            *v.1.entry(e.distances.map(f32::to_bits)).or_default() += 1;
        }
    }
    let distances = votes
        .into_iter()
        .filter_map(|(m, (chain, d))| {
            let best = d.into_iter().max_by_key(|&(k, n)| (n, k))?.0.map(f32::from_bits);
            let ok = |v: f32| (v.is_finite() && v > 0.0).then_some(v);
            // Slot k ends at best[k]; a model starting at slot k switches in at best[k - 1]. A NaN last
            // distance after finite ones = never culled (INFERRED: the festival marquees, grandstand and
            // scoreboard, 100 / 250 / NaN).
            let cull = if best[2].is_nan() && ok(best[1]).is_some() { NO_CULL_M } else { ok(best[2]).or(ok(best[1])).or(ok(best[0])).unwrap_or(1000.0) };
            let at = |c: Option<&(u16, usize)>| c.and_then(|c| ok(best[c.1 - 1]));
            Some((
                m,
                TemplateDistances {
                    lod1_model: chain.first().map(|c| c.0),
                    lod2_model: chain.get(1).map(|c| c.0),
                    lod1_m: at(chain.first()),
                    lod2_m: at(chain.get(1)),
                    cull_m: cull,
                    from_lod_table: false,
                },
            ))
        })
        .collect::<BTreeMap<_, _>>();
    Ok((out, distances))
}

/// Which of the two light-glow record kinds (see [`parse_glows`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlowKind {
    /// 0x3C-byte record: a camera-facing glow sprite (view-angle fade; second texture = 1D animation strip).
    Cone,
    /// 0x34-byte record: a camera-facing light-beam ribbon along `direction`.
    Halo,
}

/// One light glow (a night-time lamp sprite).
#[derive(Debug, Clone, PartialEq)]
pub struct LightGlow {
    pub kind: GlowKind,
    /// Indices into the track `.pvs` texture table (`pvs::Pvs::textures`); the second is `Cone`-only.
    pub textures: [Option<u32>; 2],
    /// The group entry's fourth word (`Cone` only, 0..3: UV scale (1,1) / (2,1) / (1,2) / (2,2)).
    pub flags: u32,
    /// Collision space (left-handed, +Z north).
    pub position: [f32; 3],
    /// Unit vector, collision space.
    pub direction: [f32; 3],
    /// The floats after the direction (docs/PROPS.md "Light glows"): `Cone` 8 (@0x18..0x38: inner / outer view
    /// angle, depth pull, unused, half-size, switch-on threshold, animation phase / gain), `Halo` 6 (@0x18..0x30:
    /// half-width at base / end, length, switch-on threshold, two unused) then zeros.
    pub params: [f32; 8],
    /// The record's last word (RGB in the top three bytes; the low byte is never read).
    pub colour: u32,
}

/// A `.pgeo` light-glow group (type 6, `CProceduralLightGlows`).
#[derive(Debug, Clone, PartialEq)]
pub struct LightGlows {
    pub name: String,
    /// Activity-conditional groups (`0xFFFFFFFF` @0x60): the event name @0x64 (`FR01_L`,
    /// `PLANE_RACE_001_02_L`, `FESTIVAL_FIRST_01_L`...). They carry no activity-id list (bytes 0x80..0x98
    /// are zero on all 82), so they are hidden in free roam. `None` = always present.
    pub event: Option<String>,
    pub glows: Vec<LightGlow>,
}

/// Parses a light-glow `.pgeo` (type 6); `Ok(None)` for other types.
///
/// Layout (VERIFIED: all 278 Colorado files walk exactly to their end, = the file size @0x48):
/// name length incl. NUL @0x3C, texture-reference count n @0x40, name @0x60 (@0x98 when conditional);
/// 4-byte aligned after the name, n pairs (u32 `.pvs` texture-table index, u32 0) (VERIFIED: the
/// referenced texture records carry their own index; they name `.bix` / CAFF files on the disc), 8 bytes of
/// runtime pointers, u32 nCone, u32 nHalo, u32 0, nCone group entries of 20 bytes (i32 reference,
/// i32 second reference or -1, u32 count, u32 flags, u32 pointer), nHalo entries of 16 bytes (i32
/// reference, i32 -1, u32 count, u32 pointer), then every `Cone` record (0x3C) and every `Halo`
/// record (0x34) in group order. Records: vec3 position, vec3 direction, floats, u32 colour last.
pub fn parse_glows(d: &[u8]) -> Result<Option<LightGlows>, Error> {
    let r = R(d);
    if d.get(..4) != Some(b"OEGP") {
        return Err(Error::BadMagic("pgeo: OEGP"));
    }
    if r.u32(0x30)? != 6 {
        return Ok(None);
    }
    let cstr = |at: usize, max: usize| -> String {
        let b = d.get(at..(at + max).min(d.len())).unwrap_or_default();
        b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())].iter().map(|&c| c as char).collect()
    };
    let conditional = d.get(0x60..0x64) == Some(&[0xFF; 4][..]);
    let name_len = r.u32(0x3C)? as usize;
    let name_at = if conditional { 0x98 } else { 0x60 };
    let name = cstr(name_at, name_len);
    let event = conditional.then(|| cstr(0x64, 0x1C));
    let n_refs = r.u32(0x40)? as usize;
    let mut o = (name_at + name_len + 3) & !3;
    let refs = (0..n_refs).map(|i| r.u32(o + 8 * i)).collect::<Result<Vec<_>, _>>()?;
    o += 8 * n_refs + 8;
    let (n_cone, n_halo) = (r.u32(o)? as usize, r.u32(o + 4)? as usize);
    if n_cone + n_halo > 1000 || r.u32(o + 8)? != 0 {
        return Err(Error::Unsupported(format!("glow group header {n_cone} / {n_halo}")));
    }
    o += 12;
    let tex = |i: u32| -> Result<Option<u32>, Error> {
        match i as i32 {
            -1 => Ok(None),
            i => refs.get(i as usize).copied().map(Some).ok_or_else(|| Error::Unsupported(format!("glow texture reference {i} of {n_refs}"))),
        }
    };
    let mut groups = Vec::new();
    for _ in 0..n_cone {
        groups.push((GlowKind::Cone, [tex(r.u32(o)?)?, tex(r.u32(o + 4)?)?], r.u32(o + 8)? as usize, r.u32(o + 12)?));
        o += 20;
    }
    for _ in 0..n_halo {
        groups.push((GlowKind::Halo, [tex(r.u32(o)?)?, tex(r.u32(o + 4)?)?], r.u32(o + 8)? as usize, 0));
        o += 16;
    }
    let mut glows = Vec::new();
    for (kind, textures, count, flags) in groups {
        let (size, floats) = match kind {
            GlowKind::Cone => (0x3C, 8),
            GlowKind::Halo => (0x34, 6),
        };
        for _ in 0..count {
            let mut params = [0f32; 8];
            for (j, p) in params.iter_mut().take(floats).enumerate() {
                *p = r.f32(o + 0x18 + 4 * j)?;
            }
            glows.push(LightGlow { kind, textures, flags, position: r.vec3(o)?, direction: r.vec3(o + 12)?, params, colour: r.u32(o + size - 4)? });
            o += size;
        }
    }
    if o != d.len() {
        return Err(Error::Unsupported(format!("glow group {name}: records end at {o:#x} of {:#x}", d.len())));
    }
    Ok(Some(LightGlows { name, event, glows }))
}

/// Every light-glow group of a track (each `.pgeo` parsed once; `bin.zip` name order).
pub fn track_glows<R: std::io::Read + std::io::Seek>(ar: &mut crate::zip::Archive<R>) -> Result<Vec<LightGlows>, Error> {
    let mut seen = std::collections::HashSet::new();
    let mut pgeos: Vec<crate::zip::Entry> =
        ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pgeo") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    pgeos.sort_by_key(|e| e.name.to_ascii_lowercase());
    let mut out = Vec::new();
    for e in pgeos {
        let d = ar.read(&e)?;
        if d.get(0x30..0x34) == Some(&[0, 0, 0, 6][..]) {
            out.extend(parse_glows(&d)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the user's disc (skips without `disc/` or `FH1_DISC`): every Colorado `.pgeo`
    /// parses, and all placements but those of reference-less models resolve to a template.
    #[test]
    fn colorado_placements() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let track = disc.join("media/tracks/colorado");
        let (Ok(mut ar), Ok(pvs)) = (crate::zip::Archive::open(track.join("bin.zip")), std::fs::read(track.join("Ribbon_00/Colorado_00.pvs"))) else {
            return eprintln!("no disc, skipping");
        };
        let (placed, skipped) = track_placements(&mut ar, &pvs).unwrap();
        let events = track_event_groups(&mut ar, &pvs).unwrap();
        let event_placed: usize = events.iter().map(|g| g.placements.len()).sum();
        eprintln!("free roam: {} placed, {skipped} skipped; event groups: {} with {event_placed} placements", placed.len(), events.len());
        // 527,351 ordinary + 552 from the 56 free-roam conditional groups.
        assert_eq!((placed.len(), skipped), (527_903, 2_602));
        assert_eq!((events.len(), event_placed), (413, 45_796));
        assert!(placed.iter().all(|p| p.model_number > 0 && p.matrix[15] == 1.0));
        let dist = track_template_distances(&mut ar, &pvs).unwrap();
        let from_table = dist.values().filter(|d| d.from_lod_table).count();
        let uses_table = placed.iter().filter(|p| dist.get(&p.model_number).is_some_and(|d| d.from_lod_table)).count();
        eprintln!("templates with distances: {} ({from_table} from LOD tables); placements covered by a table: {uses_table}", dist.len());
        assert_eq!(uses_table, placed.len());
        // Every placed template has distances; LOD switches never exceed the cull distance.
        assert!(placed.iter().all(|p| dist.contains_key(&p.model_number)));
        assert!(dist.values().all(|d| d.cull_m > 0.0 && d.lod1_m.is_none_or(|l| l <= d.cull_m)));
    }

    /// Against the user's disc: every `CollObjs.xml` type maps to a whole-object template.
    #[test]
    fn colorado_collobjs() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let track = disc.join("media/tracks/colorado");
        let (Ok(ar), Ok(pvs), Ok(xml)) = (
            crate::zip::Archive::open(track.join("bin.zip")),
            std::fs::read(track.join("Ribbon_00/Colorado_00.pvs")),
            std::fs::read_to_string(track.join("Ribbon_00/CollObjs.xml")),
        ) else {
            return eprintln!("no disc, skipping");
        };
        let by_name: std::collections::HashMap<String, crate::zip::Entry> = ar.entries.iter().rev().map(|e| (e.name.to_ascii_lowercase(), e.clone())).collect();
        let ar = std::cell::RefCell::new(ar);
        let names = std::cell::RefCell::new(std::collections::HashMap::new());
        let template_name = |m: u16| -> Option<String> {
            names
                .borrow_mut()
                .entry(m)
                .or_insert_with(|| {
                    let e = by_name.get(&format!("coloradoout.{m:05}.rmb.bin"))?;
                    let t = crate::rmb::parse(&ar.borrow_mut().read(e).ok()?).ok()?;
                    Some(t.submodels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(";"))
                })
                .clone()
        };
        let objs = parse_obj_xml(&xml);
        let map = collobj_templates(&objs, &pvs, template_name).unwrap();
        let count = |k: CollObjMatch| map.values().filter(|v| v.1 == k).count();
        let counts = (count(CollObjMatch::Count), count(CollObjMatch::NameAndCount), count(CollObjMatch::CountNearOrder), count(CollObjMatch::NameOnly));
        eprintln!("collobj matches (count, name+count, count near order, name only): {counts:?}");
        for (t, v) in map.iter().filter(|(_, v)| v.1 != CollObjMatch::Count) {
            eprintln!("  {t} -> {v:?}");
        }
        assert_eq!(map.len(), 180);
        assert_eq!(counts.0, 176);
        assert_eq!(map["CO_CLRD_ClrSprRes3_AddWestSteelworks4"], (126, CollObjMatch::NameAndCount));
        assert_eq!(map["CO_ArmcoArrow_001"].0, 3);
        assert_eq!(map["CO_Bench_001"].0, 6);
        assert_eq!(map["CO_BinE"].0, 23);
        assert_eq!(map["CO_CLRD_Sign_Speed35"], (341, CollObjMatch::NameOnly));
        let placed = collobj_placements(&objs, &map);
        assert!(placed.len() > 5000, "{} objects", placed.len());
    }

    /// Against the user's disc: the zones' activity blocks keep event-only CollObjs (festival boards, cones,
    /// barrels laid across roads) out of free roam, and fix the two swapped Gladstone signs.
    #[test]
    fn colorado_object_conditions() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let track = disc.join("media/tracks/colorado");
        let (Ok(mut ar), Ok(pvs)) = (crate::zip::Archive::open(track.join("bin.zip")), std::fs::read(track.join("Ribbon_00/Colorado_00.pvs"))) else {
            return eprintln!("no disc, skipping");
        };
        let conds = track_object_conditions(&mut ar, &pvs).unwrap();
        let r = track.join("Ribbon_00");
        let objs = parse_obj_xml(&std::fs::read_to_string(r.join("CollObjs.xml")).unwrap());
        let by_name: std::collections::HashMap<String, crate::zip::Entry> = ar.entries.iter().map(|e| (e.name.to_ascii_lowercase(), e.clone())).collect();
        let cell = std::cell::RefCell::new(&mut ar);
        let mut map = collobj_templates(&objs, &pvs, |n| {
            let e = by_name.iter().find(|(k, _)| k.ends_with(&format!("out.{n:05}.rmb.bin")))?.1.clone();
            let m = crate::rmb::parse(&cell.borrow_mut().read(&e).ok()?).ok()?;
            Some(m.submodels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(";"))
        })
        .unwrap();
        let mut changes = refine_collobj_templates(&objs, &mut map, &conds);
        changes.sort();
        assert_eq!(changes, vec![("CO_CLRD_Gladstone2_Beaumont4".to_owned(), 179, 183), ("CO_CLRD_Gladstone_CliftonValley".to_owned(), 183, 179)]);
        let all = collobj_placements(&objs, &map).len();
        let free = collobj_free_roam_placements(&objs, &map, &conds);
        assert_eq!((all, free.len()), (5406, 4094));
        // The Red Rock cul-de-sac board (activities [213, 249]) is gone; the flyer next to it stays.
        let near = |p: &[f32; 16], x: f32, z: f32| (p[12] - x).abs() < 0.2 && (p[14] + z).abs() < 0.2;
        assert!(!free.iter().any(|p| near(&p.matrix, 1523.9, -5337.9)));
        let game = gameobj_free_roam_placements(&parse_obj_xml(&std::fs::read_to_string(r.join("GameObjs.xml")).unwrap()), &conds);
        assert_eq!(game.len(), 100 + 44 + 36);
        assert!(game.iter().any(|p| near(&p.matrix, 1536.3, -5370.5)));
    }

    /// Against the user's disc: animated scenes and their rest-pose templates.
    #[test]
    fn colorado_anim() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let Ok(mut ar) = crate::zip::Archive::open(disc.join("media/tracks/colorado/bin.zip")) else {
            return eprintln!("no disc, skipping");
        };
        let anim = track_anim(&mut ar).unwrap();
        assert_eq!((anim.objects.len(), anim.scenes.len()), (33, 202));
        assert_eq!(anim.scenes.iter().map(|s| s.instances.len()).sum::<usize>(), 455);
        assert!(anim.objects[5].name.contains("CombineHarvester") && anim.objects[7].name.contains("WindmillMetal"));
        let index = template_name_index(&mut ar, |n| anim.objects.iter().any(|o| o.meshes.iter().any(|m| m == n))).unwrap();
        let lookup = |n: &str| index.get(n).copied();
        assert_eq!(anim_object_templates(&anim.objects[7], lookup), vec![(6625, Some(6626))]);
        assert_eq!(anim_object_templates(&anim.objects[5], lookup), vec![(6621, Some(6622)), (6623, None)]);
        // 19 windmills + 6 tractors + 8 harvesters x (body, reel); the blimp scene is event-only.
        assert_eq!(anim_placements(&anim, lookup).len(), 41);
    }

    /// Against the user's disc: each GameObjs prefix count equals its template's smashable records.
    #[test]
    fn colorado_gameobjs() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let r = disc.join("media/tracks/colorado/Ribbon_00");
        let (Ok(pvs), Ok(xml)) = (std::fs::read(r.join("Colorado_00.pvs")), std::fs::read_to_string(r.join("GameObjs.xml"))) else {
            return eprintln!("no disc, skipping");
        };
        let records = pvs_records(&pvs).unwrap();
        let objs = parse_obj_xml(&xml);
        for (prefix, t, _) in GAMEOBJ_TEMPLATES {
            let n = objs.iter().filter(|o| o.kind.starts_with(prefix)).count();
            let smash = records.iter().filter(|r| u16::from_be_bytes([r[0], r[1]]) == t && r[4] == 0x08).count();
            assert_eq!(n, smash, "{prefix} -> {t}");
        }
        assert_eq!(gameobj_placements(&objs).len(), 100 + 44 + 36);
    }

    /// Against the user's disc: all 278 light-glow groups walk exactly, and every texture they name is on the disc.
    #[test]
    fn colorado_glows() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let track = disc.join("media/tracks/colorado");
        let (Ok(mut ar), Ok(pvs)) = (crate::zip::Archive::open(track.join("bin.zip")), std::fs::read(track.join("Ribbon_00/Colorado_00.pvs"))) else {
            return eprintln!("no disc, skipping");
        };
        let groups = track_glows(&mut ar).unwrap();
        let count = |k: GlowKind| groups.iter().flat_map(|g| &g.glows).filter(|l| l.kind == k).count();
        assert_eq!((groups.len(), groups.iter().filter(|g| g.event.is_some()).count()), (278, 82));
        assert_eq!((count(GlowKind::Cone), count(GlowKind::Halo)), (2_704, 1_716));
        let pvs = crate::pvs::parse(&pvs).unwrap();
        let names: std::collections::HashSet<String> = ar.entries.iter().map(|e| e.name.to_ascii_lowercase()).collect();
        for l in groups.iter().flat_map(|g| &g.glows) {
            let t = pvs.textures[l.textures[0].unwrap() as usize];
            assert!(names.contains(&t.file_name().unwrap().to_ascii_lowercase()), "{t:?}");
            let d = l.direction;
            assert!(((d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() - 1.0).abs() < 1e-3);
        }
    }

    /// Against the user's disc: the festival zone's placement section maps entries onto head records.
    #[test]
    fn colorado_zone_instances() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let track = disc.join("media/tracks/colorado");
        let (Ok(mut ar), Ok(pvs)) = (crate::zip::Archive::open(track.join("bin.zip")), std::fs::read(track.join("Ribbon_00/Colorado_00.pvs"))) else {
            return eprintln!("no disc, skipping");
        };
        let records = pvs_records(&pvs).unwrap();
        let z = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case("__R00Z00655.pvsz")).cloned().unwrap();
        let e = zone_entries(&ar.read(&z).unwrap()).unwrap();
        assert_eq!(e.len(), 1372);
        // Entries 7 / 8: FEST_AUTOSHOW_RoundMarquee LOD00 / LOD01 at one festival-site position.
        let model = |i: usize| u16::from_be_bytes([records[e[i].record as usize][0], records[e[i].record as usize][1]]);
        assert_eq!((model(7), model(8)), (1674, 1675));
        assert_eq!(e[7].placement.position, e[8].placement.position);
        assert!((e[7].placement.position[0] + 882.09).abs() < 0.1 && e[7].activities.is_none());
        let (placed, _) = track_zone_instances(&mut ar, &pvs, |_| false).unwrap();
        assert!(placed.iter().any(|p| p.model_number == 1727), "GrandstandMain placed");
        assert!(placed.len() > 7000, "{}", placed.len());
        // The 9 barn-find barns show their closed doors (1144), never the open ones (1145).
        let doors = |m: u16| placed.iter().filter(|p| p.model_number == m).count();
        assert_eq!((doors(1141), doors(1144), doors(1145)), (9, 9, 0));
    }

    #[test]
    fn half_floats() {
        assert_eq!(half_to_f32(0x3C00), 1.0);
        assert_eq!(half_to_f32(0xE3D0), -1000.0);
        assert!((half_to_f32(0x3DC4) - 1.4414).abs() < 1e-3);
    }

    #[test]
    fn signed_fields() {
        assert_eq!(sfield(0x7FC0_01FF, 0, 11), 511);
        assert_eq!(sfield(0x3FF, 0, 10), -1);
        assert_eq!(sfield(0x7FF << 11, 11, 11), -1);
    }

    #[test]
    fn engine_matrix_flips_z() {
        let p = Placement { model: 0, x_axis: [1.0, 0.0, 0.0], y_axis: [0.0, 1.0, 0.0], z_axis: [0.0, 0.0, 1.0], position: [1.0, 2.0, 3.0], full: true, normal: DEFAULT_NORMAL, tint: DEFAULT_TINT };
        let m = p.engine_matrix();
        assert_eq!(&m[12..15], &[1.0, 2.0, -3.0]);
        assert_eq!(&m[8..11], &[-0.0, -0.0, 1.0]);
    }

    #[test]
    fn obj_xml() {
        let x = r##"<Obj0 PhysicsType="CO_Bench_001.3.rmb" GraphicsName="#0"><Pos x="1" y="2" z="3"/><Orientation><XAxis x="1" y="0" z="0"/><YAxis x="0" y="1" z="0"/><ZAxis x="0" y="0" z="1"/></Orientation></Obj0>"##;
        let v = parse_obj_xml(x);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, "CO_Bench_001.3.rmb");
        assert_eq!(v[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(v[0].z_axis, [0.0, 0.0, 1.0]);
    }
}
