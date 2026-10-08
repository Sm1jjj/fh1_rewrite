//! Track physics definitions: `media/tracks/<track>/PhysicsDefinitions.bin` (big-endian, byte-packed, no
//! alignment), the rigid bodies of the smashable / movable props (docs/SMASH.md "Physics definitions").
//!
//! Grammar VERIFIED from default.xex (the serialiser names every field: `CPhysicsDefinitionList` load
//! 0x82D3DEF0, `CPhysicsDefinition::Serialize` 0x82D3BEA8, `CCollObjPhysicsDefinition::Serialize` 0x82D3E3D0,
//! shapes 0x82D3E180 / 0x82D3CC68, per-type model lists 0x82D3D730) and on the disc: Colorado walks exactly to
//! EOF (84 top-level definitions, 455 in all, 827 spheres, 394 boxes, 7 convex hulls).
//!
//! ```text
//! File:  i32 n; Definition[n];
//!        if n > 0 && defs[0].version > 10: i32 m; u16 global_to_def[m]             // global type index -> top-level def
//!        if version > 12: per global g: i32 child_models[max(defs[global_to_def[g]].children, 1)]
//! Definition: i32 version; if version > 7: i32 kind (0 = CCollObjPhysicsDefinition);
//!   f32 mass; mat3 inertia; mat3 inverse inertia; vec3 half extents; f32 bounding radius; vec3 graphics offset;
//!   i32 child count; if version > 11: vec3 AABB centre offset; if version < 13: i32 render model;
//!   f32 visible distance; i32 surface type; f32 break speed (mph); i32 unique index; u32 name hash;
//!   if version > 8: u8 object type (SmashableObjectTypes.xml index, 255 = none); i32 shape count; Shape[];
//!   Definition children[child count] (pre-order).
//! Shape: i32 type; 0 sphere: u8 type, u8 flags, vec4 (centre, radius); 1 box: u8 type, u8 flags, mat4 (rows,
//!   row 3 = translation), vec4 half extents (w junk); 2 convex: i32 size, raw in-memory hull blob[size]
//!   (+0x10 mat4, +0x50 vertex offset (vec4 each), +0x60 u8 vertex count).
//! ```
//!
//! Spaces: shapes are in BODY space; the render model's origin is at `graphics_offset` from the body centre, so
//! a body-space point p is `p - graphics_offset` in the template's space, and the template's bounds centre is
//! `aabb_offset - graphics_offset` (VERIFIED: equal to the template bounds within 2 cm on 130+ types; the axes
//! agree without a Z flip at that precision, which the symmetric props can't distinguish, INFERRED).
//! `child_models` lists the `.pvs` model numbers of the direct children (the smash shards); the whole object's own
//! template is the gap in (or the model next to) that run (VERIFIED on the bench: 6, shards 7..18).
//! Mass units are UNVERIFIED (bench 0.65, doors 100). Break speed 9999 = never breaks; 0 on many parents (meaning
//! GUESSED: breaks into its children on contact). Rigid-body init (0x82D28BA0) converts mph -> m/s.

use crate::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Sphere { centre: [f32; 3], radius: f32 },
    /// Row-major 4x4 (row 3 = translation) and half extents, body space.
    Box { matrix: [f32; 16], half: [f32; 3] },
    /// The hull's transform and vertices (body space, before the transform).
    Convex { matrix: [f32; 16], vertices: Vec<[f32; 3]> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub version: i32,
    pub mass: f32,
    pub inertia: [f32; 9],
    pub half_extents: [f32; 3],
    pub bounding_radius: f32,
    pub graphics_offset: [f32; 3],
    pub aabb_offset: [f32; 3],
    pub visible_distance: f32,
    pub surface: i32,
    pub break_mph: f32,
    pub unique_index: i32,
    pub name_hash: u32,
    /// `SmashableObjectTypes.xml` index (0 Fence, 1 Flyer, 2 Mailbox, 3 RaceActivation; INFERRED), 255 = none.
    pub object_type: u8,
    pub shapes: Vec<Shape>,
    pub children: Vec<Definition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhysicsDefinitions {
    pub defs: Vec<Definition>,
    /// Global object type index (the `.pvsz` block object type; CollObjs types sorted case-insensitively plus
    /// extras, INFERRED) -> index into `defs`.
    pub global_to_def: Vec<u16>,
    /// Per global type: the `.pvs` model numbers of the definition's direct children (or its one model).
    pub child_models: Vec<Vec<i32>>,
}

impl PhysicsDefinitions {
    /// Candidate whole-object templates of global type `g`: the gap inside its child-model run, else the models
    /// either side of it (a leaf definition lists its own model).
    pub fn whole_candidates(&self, g: usize) -> Vec<u16> {
        let mut c: Vec<i32> = self.child_models[g].clone();
        c.sort_unstable();
        let (Some(&lo), Some(&hi)) = (c.first(), c.last()) else { return Vec::new() };
        if self.defs[self.global_to_def[g] as usize].children.is_empty() {
            return vec![lo as u16];
        }
        let gaps: Vec<u16> = (lo..=hi).filter(|x| !c.contains(x)).map(|x| x as u16).collect();
        if gaps.is_empty() { [lo - 1, hi + 1].into_iter().filter(|&x| x >= 0).map(|x| x as u16).collect() } else { gaps }
    }

    /// The global type whose top-level definition fits a smashable whole template: its bounds (`lo`, `hi`,
    /// template space) against the definition's AABB, preferring types whose model run names `n`
    /// (error < 0.15 m), else any type with `parts` children whose AABB is within 0.05 m (several sign types
    /// share one geometry). Returns (global type, error in metres).
    pub fn match_template(&self, n: u16, lo: [f32; 3], hi: [f32; 3], parts: usize) -> Option<(usize, f32)> {
        let err = |g: usize| {
            let d = &self.defs[self.global_to_def[g] as usize];
            (0..3)
                .map(|k| {
                    let c = d.aabb_offset[k] - d.graphics_offset[k];
                    ((c - (lo[k] + hi[k]) * 0.5).abs()).max((d.half_extents[k] - (hi[k] - lo[k]) * 0.5).abs())
                })
                .fold(0f32, f32::max)
        };
        let best = |gs: &mut dyn Iterator<Item = usize>, tol: f32| gs.map(|g| (g, err(g))).filter(|e| e.1 < tol).min_by(|a, b| a.1.total_cmp(&b.1));
        let all = 0..self.global_to_def.len();
        best(&mut all.clone().filter(|&g| self.whole_candidates(g).contains(&n)), 0.15)
            .or_else(|| best(&mut all.filter(|&g| self.defs[self.global_to_def[g] as usize].children.len() == parts), 0.05))
    }
}

struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        let b = self.d.get(self.p..self.p + n).ok_or(Error::Truncated("PhysicsDefinitions"))?;
        self.p += n;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, Error> {
        Ok(self.u32()? as i32)
    }
    fn f32(&mut self) -> Result<f32, Error> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn fs<const N: usize>(&mut self) -> Result<[f32; N], Error> {
        let mut v = [0f32; N];
        for x in &mut v {
            *x = self.f32()?;
        }
        Ok(v)
    }
}

fn shape(r: &mut R, version: i32) -> Result<Shape, Error> {
    let t = r.i32()?;
    if t == 2 {
        let n = r.i32()? as usize;
        let blob = r.take(n)?;
        let f = |o: usize| blob.get(o..o + 4).map(|b| f32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("convex hull"));
        let matrix: [f32; 16] = std::array::from_fn(|k| f(0x10 + 4 * k).unwrap_or(0.0));
        let at = u32::from_be_bytes(blob.get(0x50..0x54).ok_or(Error::Truncated("convex hull"))?.try_into().unwrap()) as usize;
        let count = *blob.get(0x60).ok_or(Error::Truncated("convex hull"))? as usize;
        let vertices = (0..count).map(|i| Ok([f(at + 16 * i)?, f(at + 16 * i + 4)?, f(at + 16 * i + 8)?])).collect::<Result<_, Error>>()?;
        return Ok(Shape::Convex { matrix, vertices });
    }
    let base = if version < 10 { t } else { i32::from(r.u8()?) };
    if version >= 10 {
        r.u8()?; // flags (3)
    }
    if base != t {
        return Err(Error::Unsupported(format!("physics shape type {t} / {base}")));
    }
    match t {
        0 => {
            let v = r.fs::<4>()?;
            Ok(Shape::Sphere { centre: [v[0], v[1], v[2]], radius: v[3] })
        }
        1 => {
            let matrix = r.fs::<16>()?;
            let h = r.fs::<4>()?;
            Ok(Shape::Box { matrix, half: [h[0], h[1], h[2]] })
        }
        _ => Err(Error::Unsupported(format!("physics shape type {t}"))),
    }
}

fn definition(r: &mut R, depth: usize) -> Result<Definition, Error> {
    if depth > 8 {
        return Err(Error::Unsupported("physics definitions nest too deep".into()));
    }
    let version = r.i32()?;
    if version > 7 && r.i32()? != 0 {
        return Err(Error::Unsupported("physics definition kind".into()));
    }
    let mass = r.f32()?;
    let inertia = r.fs::<9>()?;
    r.fs::<9>()?; // inverse inertia
    let half_extents = r.fs::<3>()?;
    let bounding_radius = r.f32()?;
    let graphics_offset = r.fs::<3>()?;
    let n_children = r.i32()? as usize;
    let aabb_offset = if version > 11 { r.fs::<3>()? } else { [0.0; 3] };
    if version < 13 {
        r.i32()?; // render model index
    }
    let visible_distance = r.f32()?;
    let surface = r.i32()?;
    let break_mph = r.f32()?;
    let unique_index = r.i32()?;
    let name_hash = r.u32()?;
    let object_type = if version > 8 { r.u8()? } else { 255 };
    let n_shapes = r.i32()? as usize;
    if n_shapes > 1000 || n_children > 1000 {
        return Err(Error::Unsupported(format!("physics definition: {n_shapes} shapes, {n_children} children")));
    }
    let shapes = (0..n_shapes).map(|_| shape(r, version)).collect::<Result<_, _>>()?;
    let children = (0..n_children).map(|_| definition(r, depth + 1)).collect::<Result<_, _>>()?;
    Ok(Definition {
        version,
        mass,
        inertia,
        half_extents,
        bounding_radius,
        graphics_offset,
        aabb_offset,
        visible_distance,
        surface,
        break_mph,
        unique_index,
        name_hash,
        object_type,
        shapes,
        children,
    })
}

/// Parses `PhysicsDefinitions.bin`; errors unless the walk ends exactly at the end of the file.
pub fn parse(d: &[u8]) -> Result<PhysicsDefinitions, Error> {
    let mut r = R { d, p: 0 };
    let n = r.i32()? as usize;
    if n > 10_000 {
        return Err(Error::Unsupported(format!("{n} physics definitions")));
    }
    let defs: Vec<Definition> = (0..n).map(|_| definition(&mut r, 0)).collect::<Result<_, _>>()?;
    let version = defs.first().map_or(0, |d| d.version);
    let mut global_to_def = Vec::new();
    if version > 10 {
        let m = r.i32()? as usize;
        global_to_def = (0..m).map(|_| r.u16()).collect::<Result<_, _>>()?;
        if global_to_def.iter().any(|&g| g as usize >= defs.len()) {
            return Err(Error::Unsupported("physics global index out of range".into()));
        }
    }
    let mut child_models = Vec::new();
    if version > 12 {
        for &g in &global_to_def {
            let k = defs[g as usize].children.len().max(1);
            child_models.push((0..k).map(|_| r.i32()).collect::<Result<_, _>>()?);
        }
    }
    if r.p != d.len() {
        return Err(Error::Unsupported(format!("PhysicsDefinitions: walk ends at {:#x} of {:#x}", r.p, d.len())));
    }
    Ok(PhysicsDefinitions { defs, global_to_def, child_models })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the user's disc: the Colorado file walks exactly, with the counts from the default.xex grammar.
    #[test]
    fn colorado_physics_definitions() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let Ok(d) = std::fs::read(disc.join("media/tracks/colorado/PhysicsDefinitions.bin")) else {
            return eprintln!("no disc, skipping");
        };
        let p = parse(&d).unwrap();
        fn walk<'a>(d: &'a Definition, out: &mut Vec<&'a Definition>) {
            out.push(d);
            d.children.iter().for_each(|c| walk(c, out));
        }
        let mut all = Vec::new();
        p.defs.iter().for_each(|d| walk(d, &mut all));
        let count = |f: fn(&Shape) -> bool| all.iter().flat_map(|d| &d.shapes).filter(|s| f(s)).count();
        assert_eq!((p.defs.len(), all.len(), p.global_to_def.len()), (84, 455, 189));
        assert_eq!(
            (count(|s| matches!(s, Shape::Sphere { .. })), count(|s| matches!(s, Shape::Box { .. })), count(|s| matches!(s, Shape::Convex { .. }))),
            (827, 394, 7)
        );
        // Unique indices are the pre-order positions.
        assert!(all.iter().enumerate().all(|(i, d)| d.unique_index == i as i32));
        // The bench (global 4): 12 children whose models are the shards 7..18, whole template 6.
        assert_eq!(p.child_models[4], (7..=18).collect::<Vec<_>>());
        assert!(p.whole_candidates(4).contains(&6));
    }
}
