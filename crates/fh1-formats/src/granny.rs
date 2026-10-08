//! Animated track objects: `.pgeo` type 4 (`CProceduralAnimatedObject`, `Anim_ANIM_*`) = the game's
//! draw data (models, LOD meshes, draw ranges, vertex/index buffers) around an embedded Granny3D
//! `.gr2` (skeletons, models, track groups, animations; its own meshes are stripped). Decoded from
//! default.xex (loader 0x82DFEDC0, vertex declarations 0x82E031E8, shaders PROC_ANIM_OBJ_*) and checked
//! on all 33 Colorado objects (docs/PROPS.md "Animated objects"):
//!
//! - wrapper: common `.pgeo` header + name, texture references `(u32 PVS texture index, 0)`, 16-aligned
//!   0x60-byte header (bbox, Granny offset/size @+0x20/+0x24, model count @+0x3C, a size list @+0x44),
//!   models (0x3C: bone count @+8, three LOD lists `(count, ptr, ., .)` @+0x14), meshes (0x44 each: bone
//!   @+0 (-1 = per-vertex blend indices), vertex count @+4, index count @+8, draw lists @+0xC / +0x10
//!   (0x2C records), a size list @+0x18, light attachments @+0x1C (0x14 records with 0x38 sub-records),
//!   vertex format @+0x40), then the Granny file, then (4-aligned) every mesh's u16 indices, then every
//!   mesh's vertices. The walk lands exactly on the Granny magic and ends exactly at the file end on
//!   all 33 objects.
//! - vertex formats (stream 0 declarations built at 0x82E031E8): 5 = 12 B `F16_4 pos (w = U), BYTE4N
//!   normal (w = V)`; 4 = 16 B `+ tangent`; 3 = 16 B `pos, UBYTE4 indices, normal`; 1 = 20 B `pos, UBYTE4N
//!   weights, UBYTE4 indices, normal`; 0 = 24 B `+ tangent`. PROC_ANIM_OBJ_RIGID_VS: `uv = (pos.w + UOffset,
//!   normal.w * 0.5 + 0.5)`. 4-byte elements are read lowest byte = x (Xenos order): the normals then agree
//!   with the face normals (mean cos 0.89-0.96 per format).
//! - draw record (0x2C): four texture slots (index into the references, -1 none), start triangle,
//!   triangle count, three zeros, two floats (e.g. 10 / 0.1, 75 / 0.01: specular power / level, UNVERIFIED).
//! - spaces: mesh and Granny data are left-handed like the collision space (the windmill LOD1 mesh equals
//!   template 6626 with Z negated).

use crate::Error;

// ---- Generic GR2 reader -------------------------------------------------------------------------

/// Granny's big-endian 32-bit magic as stored here (each u32 of the documented BE magic byte-swapped).
pub const GR2_MAGIC_FH1: [u8; 16] = [0xB5, 0x95, 0x11, 0x0E, 0x4B, 0xB5, 0xA5, 0x6A, 0x50, 0x28, 0x28, 0xEB, 0x04, 0xB3, 0x78, 0x25];

/// A Granny transform: translation, rotation (x, y, z, w), 3x3 scale/shear (row-major).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub flags: u32,
    pub position: [f32; 3],
    pub orientation: [f32; 4],
    pub scale_shear: [[f32; 3]; 3],
}

impl Transform {
    pub const IDENTITY: Transform = Transform { flags: 0, position: [0.0; 3], orientation: [0.0, 0.0, 0.0, 1.0], scale_shear: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] };

    /// Column-major 4x4 (`m[col][row]`), `T * R * S` (Granny's composition).
    pub fn matrix(&self) -> Mat4 {
        let r = quat_matrix(self.orientation);
        let s = self.scale_shear;
        let mut m = [[0.0f32; 4]; 4];
        for c in 0..3 {
            for row in 0..3 {
                m[c][row] = (0..3).map(|k| r[row][k] * s[k][c]).sum();
            }
        }
        m[3] = [self.position[0], self.position[1], self.position[2], 1.0];
        m
    }
}

/// Column-major 4x4.
pub type Mat4 = [[f32; 4]; 4];

pub fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut m = [[0.0f32; 4]; 4];
    for (c, col) in m.iter_mut().enumerate() {
        for (r, v) in col.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[k][r] * b[c][k]).sum();
        }
    }
    m
}

pub const MAT_IDENTITY: Mat4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];

/// Row-major 3x3 rotation of a unit quaternion (x, y, z, w).
fn quat_matrix(q: [f32; 4]) -> [[f32; 3]; 3] {
    let l = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt().max(1e-12);
    let [x, y, z, w] = q.map(|v| v / l);
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

/// A value of the Granny data tree (built from the file's own type definitions).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Struct(Vec<(String, Value)>),
    Array(Vec<Value>),
    Null,
    Str(String),
    Transform(Transform),
    Reals(Vec<f32>),
    Ints(Vec<i64>),
}

impl Value {
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Value::Struct(f) => f.iter().find(|(n, _)| n == name).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn array(&self) -> &[Value] {
        match self {
            Value::Array(a) => a,
            _ => &[],
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn reals(&self) -> &[f32] {
        match self {
            Value::Reals(r) => r,
            _ => &[],
        }
    }
    pub fn int(&self) -> Option<i64> {
        match self {
            Value::Ints(i) => i.first().copied(),
            _ => None,
        }
    }
    /// The single member of a one-member struct (Granny wraps scalars in arrays of `{UInt16}` etc.).
    fn scalar_list(&self) -> Vec<f64> {
        self.array()
            .iter()
            .filter_map(|v| match v {
                Value::Struct(f) if f.len() == 1 => match &f[0].1 {
                    Value::Ints(i) => i.first().map(|&x| x as f64),
                    Value::Reals(r) => r.first().map(|&x| x as f64),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

struct Gr2<'a> {
    sections: Vec<&'a [u8]>,
    fixups: std::collections::HashMap<(u32, u32), (u32, u32)>,
}

type Ref = (u32, u32);

impl<'a> Gr2<'a> {
    fn u32(&self, r: Ref, o: u32) -> Result<u32, Error> {
        let s = self.sections.get(r.0 as usize).ok_or(Error::Truncated("gr2 section"))?;
        let a = (r.1 + o) as usize;
        s.get(a..a + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("gr2 data"))
    }
    fn bytes(&self, r: Ref, o: u32, n: usize) -> Result<&'a [u8], Error> {
        let s = self.sections.get(r.0 as usize).ok_or(Error::Truncated("gr2 section"))?;
        let a = (r.1 + o) as usize;
        s.get(a..a + n).ok_or(Error::Truncated("gr2 data"))
    }
    fn ptr(&self, r: Ref, o: u32) -> Option<Ref> {
        self.fixups.get(&(r.0, r.1 + o)).copied()
    }
    fn cstr(&self, r: Option<Ref>) -> String {
        let Some(r) = r else { return String::new() };
        let Some(s) = self.sections.get(r.0 as usize) else { return String::new() };
        let t = &s[(r.1 as usize).min(s.len())..];
        t[..t.iter().position(|&b| b == 0).unwrap_or(t.len())].iter().map(|&b| b as char).collect()
    }
    /// (type, name, sub-type, array width) of each member.
    fn members(&self, t: Ref) -> Result<Vec<(u32, String, Option<Ref>, u32)>, Error> {
        let mut out = Vec::new();
        for i in 0..256u32 {
            let ty = self.u32(t, 32 * i)?;
            if ty == 0 {
                return Ok(out);
            }
            out.push((ty, self.cstr(self.ptr(t, 32 * i + 4)), self.ptr(t, 32 * i + 8), self.u32(t, 32 * i + 12)?));
        }
        Err(Error::Unsupported("gr2: type definition without end".into()))
    }
    fn size(&self, t: Ref) -> Result<u32, Error> {
        let mut n = 0;
        for (ty, _, sub, arr) in self.members(t)? {
            let one = match ty {
                1 => self.size(sub.ok_or(Error::Unsupported("gr2 inline".into()))?)?,
                2 | 8 | 10 | 19 | 20 | 22 => 4,
                3..=5 => 8,
                7 => 12,
                9 => 68,
                11..=14 => 1,
                15..=18 | 21 => 2,
                _ => return Err(Error::Unsupported(format!("gr2: member type {ty}"))),
            };
            n += one * arr.max(1);
        }
        Ok(n)
    }
    fn read(&self, t: Ref, at: Ref, depth: u32) -> Result<Value, Error> {
        if depth > 24 {
            return Ok(Value::Null);
        }
        let mut fields = Vec::new();
        let mut off = 0u32;
        for (ty, name, sub, arr) in self.members(t)? {
            let n = arr.max(1);
            let here = (at.0, at.1 + off);
            let (v, size) = match ty {
                1 => {
                    let st = sub.ok_or(Error::Unsupported("gr2 inline".into()))?;
                    let s = self.size(st)?;
                    let v = if n == 1 { self.read(st, here, depth + 1)? } else { Value::Array((0..n).map(|k| self.read(st, (here.0, here.1 + k * s), depth + 1)).collect::<Result<_, _>>()?) };
                    (v, s * n)
                }
                2 => (match (self.ptr(here, 0), sub) { (Some(p), Some(st)) => self.read(st, p, depth + 1)?, _ => Value::Null }, 4),
                3 => {
                    let c = self.u32(here, 0)?;
                    let v = match (self.ptr(here, 4), sub) {
                        (Some(p), Some(st)) if c > 0 => {
                            let s = self.size(st)?;
                            Value::Array((0..c).map(|k| self.read(st, (p.0, p.1 + k * s), depth + 1)).collect::<Result<_, _>>()?)
                        }
                        _ => Value::Array(Vec::new()),
                    };
                    (v, 8)
                }
                4 => {
                    let c = self.u32(here, 0)?;
                    let v = match (self.ptr(here, 4), sub) {
                        (Some(p), Some(st)) => Value::Array((0..c).map(|k| self.ptr(p, 4 * k).map_or(Ok(Value::Null), |q| self.read(st, q, depth + 1))).collect::<Result<_, _>>()?),
                        _ => Value::Array(Vec::new()),
                    };
                    (v, 8)
                }
                5 => (match (self.ptr(here, 0), self.ptr(here, 4)) { (Some(st), Some(p)) => self.read(st, p, depth + 1)?, _ => Value::Null }, 8),
                7 => (Value::Null, 12),
                8 => (Value::Str(self.cstr(self.ptr(here, 0))), 4),
                9 => {
                    let b = self.bytes(here, 0, 68)?;
                    let f = |i: usize| f32::from_be_bytes(b[4 + 4 * i..8 + 4 * i].try_into().unwrap());
                    let t = Transform {
                        flags: u32::from_be_bytes(b[..4].try_into().unwrap()),
                        position: [f(0), f(1), f(2)],
                        orientation: [f(3), f(4), f(5), f(6)],
                        scale_shear: [[f(7), f(8), f(9)], [f(10), f(11), f(12)], [f(13), f(14), f(15)]],
                    };
                    (Value::Transform(t), 68)
                }
                10 => {
                    let b = self.bytes(here, 0, 4 * n as usize)?;
                    (Value::Reals(b.chunks_exact(4).map(|c| f32::from_be_bytes(c.try_into().unwrap())).collect()), 4 * n)
                }
                11..=14 => {
                    let b = self.bytes(here, 0, n as usize)?;
                    (Value::Ints(b.iter().map(|&x| if ty == 11 || ty == 13 { x as i8 as i64 } else { x as i64 }).collect()), n)
                }
                15..=18 | 21 => {
                    let b = self.bytes(here, 0, 2 * n as usize)?;
                    (Value::Ints(b.chunks_exact(2).map(|c| { let x = u16::from_be_bytes(c.try_into().unwrap()); if ty == 15 || ty == 17 { x as i16 as i64 } else { x as i64 } }).collect()), 2 * n)
                }
                19 | 20 | 22 => {
                    let b = self.bytes(here, 0, 4 * n as usize)?;
                    (Value::Ints(b.chunks_exact(4).map(|c| { let x = u32::from_be_bytes(c.try_into().unwrap()); if ty == 19 { x as i32 as i64 } else { x as i64 } }).collect()), 4 * n)
                }
                _ => return Err(Error::Unsupported(format!("gr2: member type {ty}"))),
            };
            fields.push((name, v));
            off += size;
        }
        Ok(Value::Struct(fields))
    }
}

/// Parses an uncompressed big-endian GR2 (version 7) into its data tree (the root object).
pub fn parse_gr2(d: &[u8]) -> Result<Value, Error> {
    if d.get(..16) != Some(&GR2_MAGIC_FH1[..]) {
        return Err(Error::BadMagic("gr2"));
    }
    let u = |o: usize| d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("gr2 header"));
    if u(0x20)? != 7 {
        return Err(Error::Unsupported(format!("gr2 version {}", u(0x20)?)));
    }
    let n = u(0x30)? as usize;
    let root_type = (u(0x34)?, u(0x38)?);
    let root = (u(0x3C)?, u(0x40)?);
    let sa = 0x20 + u(0x2C)? as usize;
    let mut g = Gr2 { sections: Vec::with_capacity(n), fixups: std::collections::HashMap::new() };
    for i in 0..n {
        let h = sa + 44 * i;
        let (comp, off, size) = (u(h)?, u(h + 4)? as usize, u(h + 8)? as usize);
        if comp != 0 {
            return Err(Error::Unsupported("gr2: compressed section".into()));
        }
        g.sections.push(d.get(off..off + size).ok_or(Error::Truncated("gr2 section"))?);
        let (fo, fc) = (u(h + 28)? as usize, u(h + 32)? as usize);
        for k in 0..fc {
            let r = fo + 12 * k;
            g.fixups.insert((i as u32, u(r)?), (u(r + 4)?, u(r + 8)?));
        }
    }
    g.read(root_type, root, 0)
}

// ---- Curves ---------------------------------------------------------------------------------------

/// A Granny curve: knots (seconds; frames for `DaKeyframes32f`) and controls (`dim` floats each),
/// evaluated as Granny's B-spline of `degree` ([`Curve::evaluate`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Curve {
    pub format: String,
    pub degree: u32,
    pub dim: usize,
    pub knots: Vec<f32>,
    pub controls: Vec<f32>,
    /// Identity curve (no data): position 0, identity rotation / scale.
    pub identity: bool,
}

const QSCALE: [f32; 16] = [1.414_213_5, 0.707_106_77, 0.353_553_38, 0.353_553_38, 0.353_553_38, 0.176_776_69, 0.176_776_69, 0.176_776_69, -1.414_213_5, -0.707_106_77, -0.353_553_38, -0.353_553_38, -0.353_553_38, -0.176_776_69, -0.176_776_69, -0.176_776_69];
const QOFFSET: [f32; 16] = [-0.707_106_77, -0.353_553_38, -0.530_330_06, -0.176_776_69, 0.176_776_69, -0.176_776_69, -0.088_388_346, 0.0, 0.707_106_77, 0.353_553_38, 0.530_330_06, 0.176_776_69, -0.176_776_69, 0.176_776_69, 0.088_388_346, -0.0];

impl Curve {
    /// Decodes a `CurveData` variant (the struct whose first member is `CurveDataHeader_<format>`).
    pub fn from_value(v: &Value) -> Result<Curve, Error> {
        let Value::Struct(f) = v else { return Ok(Curve { identity: true, ..Default::default() }) };
        let (hname, header) = f.first().ok_or(Error::Unsupported("curve: empty".into()))?;
        let format = hname.strip_prefix("CurveDataHeader_").ok_or_else(|| Error::Unsupported(format!("curve header {hname}")))?.to_owned();
        let degree = header.get("Degree").and_then(Value::int).unwrap_or(0) as u32;
        let g = |n: &str| v.get(n);
        let reals = |n: &str| g(n).map(|x| x.reals().to_vec()).unwrap_or_default();
        let list = |n: &str| g(n).map(|x| x.scalar_list()).unwrap_or_default();
        let mut c = Curve { format: format.clone(), degree, ..Default::default() };
        match format.as_str() {
            "DaIdentity" => {
                c.identity = true;
                c.dim = g("Dimension").and_then(Value::int).unwrap_or(0) as usize;
            }
            "DaConstant32f" | "D3Constant32f" | "D4Constant32f" => {
                c.controls = if format == "DaConstant32f" { list("Controls").iter().map(|&x| x as f32).collect() } else { reals("Controls") };
                c.dim = c.controls.len();
                c.knots = vec![0.0];
            }
            "DaKeyframes32f" => {
                c.dim = g("Dimension").and_then(Value::int).unwrap_or(1).max(1) as usize;
                c.controls = list("Controls").iter().map(|&x| x as f32).collect();
                // Keyframes: one per frame; knots in frames (scaled to seconds by the caller's TimeStep).
                c.knots = (0..c.controls.len() / c.dim).map(|i| i as f32).collect();
            }
            "DaK32fC32f" => {
                c.knots = list("Knots").iter().map(|&x| x as f32).collect();
                c.controls = list("Controls").iter().map(|&x| x as f32).collect();
                c.dim = if c.knots.is_empty() { 0 } else { c.controls.len() / c.knots.len() };
            }
            "DaK16uC16u" | "DaK8uC8u" => {
                let inv = g("OneOverKnotScaleTrunc").and_then(Value::int).unwrap_or(0) as u32;
                let inv = f32::from_bits(inv << 16);
                let so = list("ControlScaleOffsets").iter().map(|&x| x as f32).collect::<Vec<_>>();
                let kc = list("KnotsControls");
                let dim = so.len() / 2;
                let n = if dim == 0 { 0 } else { kc.len() / (dim + 1) };
                c.dim = dim;
                c.knots = kc[..n].iter().map(|&k| k as f32 / inv).collect();
                for i in 0..n {
                    for d in 0..dim {
                        c.controls.push(kc[n + i * dim + d] as f32 * so[d] + so[dim + d]);
                    }
                }
            }
            "D3K16uC16u" | "D3K8uC8u" => {
                let inv = f32::from_bits((g("OneOverKnotScaleTrunc").and_then(Value::int).unwrap_or(0) as u32) << 16);
                let (sc, of) = (reals("ControlScales"), reals("ControlOffsets"));
                let kc = list("KnotsControls");
                let n = kc.len() / 4;
                c.dim = 3;
                c.knots = kc[..n].iter().map(|&k| k as f32 / inv).collect();
                for i in 0..n {
                    for d in 0..3 {
                        c.controls.push(kc[n + 3 * i + d] as f32 * sc.get(d).copied().unwrap_or(1.0) + of.get(d).copied().unwrap_or(0.0));
                    }
                }
            }
            "D3I1K16uC16u" | "D3I1K8uC8u" | "D3I1K32fC32f" => {
                let inv = if format == "D3I1K32fC32f" { 1.0 } else { f32::from_bits((g("OneOverKnotScaleTrunc").and_then(Value::int).unwrap_or(0) as u32) << 16) };
                let (sc, of) = (reals("ControlScales"), reals("ControlOffsets"));
                let kc = list("KnotsControls");
                let n = kc.len() / 2;
                c.dim = 3;
                c.knots = kc[..n].iter().map(|&k| k as f32 / inv).collect();
                for i in 0..n {
                    for d in 0..3 {
                        c.controls.push(kc[n + i] as f32 * sc.get(d).copied().unwrap_or(1.0) + of.get(d).copied().unwrap_or(0.0));
                    }
                }
            }
            "D9I1K16uC16u" | "D9I1K8uC8u" | "D9I3K16uC16u" | "D9I3K8uC8u" => {
                let inv = f32::from_bits((g("OneOverKnotScaleTrunc").and_then(Value::int).unwrap_or(0) as u32) << 16);
                let three = format.starts_with("D9I3");
                let (sc, of) = if three { (reals("ControlScales"), reals("ControlOffsets")) } else { (reals("ControlScale"), reals("ControlOffset")) };
                let kc = list("KnotsControls");
                let per = if three { 3 } else { 1 };
                let n = kc.len() / (per + 1);
                c.dim = 9;
                c.knots = kc[..n].iter().map(|&k| k as f32 / inv).collect();
                for i in 0..n {
                    let v: Vec<f32> = (0..per).map(|d| kc[n + per * i + d] as f32 * sc.get(d).copied().unwrap_or(1.0) + of.get(d).copied().unwrap_or(0.0)).collect();
                    let diag = if three { [v[0], v[1], v[2]] } else { [v[0]; 3] };
                    c.controls.extend_from_slice(&[diag[0], 0.0, 0.0, 0.0, diag[1], 0.0, 0.0, 0.0, diag[2]]);
                }
            }
            "D4nK16uC15u" | "D4nK8uC7u" => {
                let wide = format == "D4nK16uC15u";
                let sel = g("ScaleOffsetTableEntries").and_then(Value::int).unwrap_or(0) as u32;
                let inv = g("OneOverKnotScale").map(|x| x.reals().first().copied().unwrap_or(1.0)).unwrap_or(1.0);
                let kc = list("KnotsControls");
                let n = kc.len() / 4;
                let unit = if wide { 1.0 / 32767.0 } else { 1.0 / 127.0 };
                let scales: [f32; 4] = std::array::from_fn(|i| QSCALE[((sel >> (4 * i)) & 15) as usize] * unit);
                let offsets: [f32; 4] = std::array::from_fn(|i| QOFFSET[((sel >> (4 * i)) & 15) as usize]);
                c.dim = 4;
                c.knots = kc[..n].iter().map(|&k| k as f32 / inv).collect();
                let (hi, mask) = if wide { (0x8000u32, 0x7FFFu32) } else { (0x80, 0x7F) };
                for i in 0..n {
                    let [a, b, cc] = [0, 1, 2].map(|k| kc[n + 3 * i + k] as u32);
                    let s1 = (((b & hi) != 0) as usize) << 1 | ((cc & hi) != 0) as usize;
                    let (s2, s3, s4) = ((s1 + 1) & 3, (s1 + 2) & 3, (s1 + 3) & 3);
                    let da = (a & mask) as f32 * scales[s2] + offsets[s2];
                    let db = (b & mask) as f32 * scales[s3] + offsets[s3];
                    let dc = (cc & mask) as f32 * scales[s4] + offsets[s4];
                    let mut dd = (1.0 - (da * da + db * db + dc * dc)).max(0.0).sqrt();
                    if a & hi != 0 {
                        dd = -dd;
                    }
                    let mut q = [0.0f32; 4];
                    q[s2] = da;
                    q[s3] = db;
                    q[s4] = dc;
                    q[s1] = dd;
                    c.controls.extend_from_slice(&q);
                }
            }
            _ => return Err(Error::Unsupported(format!("curve format {format}"))),
        }
        Ok(c)
    }

    /// Value at time `t` (seconds), not looping; orientations (dim 4) normalised. See [`Curve::evaluate`].
    pub fn sample(&self, t: f32) -> Option<Vec<f32>> {
        self.evaluate(t, None, self.dim == 4)
    }

    /// Granny's curve evaluator (default.xex 0x829D7A00, VERIFIED by disassembly), or None for an
    /// identity / empty curve. `looping` = the animation duration when the clip wraps (Granny's
    /// sample context Underflow/OverflowLoop, both set for a looping control: INFERRED); `normalize` is
    /// what the track sampler passes (0x829D8xxx: orientation 1, position / scale-shear 0, VERIFIED).
    ///
    /// - Constant formats and single-knot curves return control 0; keyframe curves (`DaKeyframes32f`,
    ///   knots in frames here) return the frame `trunc(t)` (Granny passes a precomputed frame index:
    ///   truncation INFERRED).
    /// - Otherwise FindKnot (0x829CF378) = first knot > t, n-1 if none. ConstructBSplineBuffers
    ///   (0x829C8D80) takes the 2*Degree knots and controls from K-Degree; outside the curve it repeats
    ///   the first / last knot and control, or with `looping` wraps round (the last knot duplicates the
    ///   first and is skipped) offsetting knots by the duration. If it padded, dim-4 normalised curves
    ///   get the hemisphere fix (0x829CFCB0) over the Degree+1 controls used.
    /// - SampleBSpline (0x829C87D8, per-degree/dim table 0x8328BFA8): de Boor on knots ti[-D..D-1],
    ///   controls pi[-D..0], interval [ti[-1], ti[0]] (t outside it extrapolates); degree 0 = pi[0].
    ///   The normalised variants divide by the length (rsqrt; a zero vector stays as is).
    pub fn evaluate(&self, t: f32, looping: Option<f32>, normalize: bool) -> Option<Vec<f32>> {
        if self.identity || self.dim == 0 || self.knots.is_empty() || self.controls.len() < self.dim {
            return None;
        }
        let dim = self.dim;
        let n = self.knots.len().min(self.controls.len() / dim);
        let at = |i: usize| &self.controls[i * dim..(i + 1) * dim];
        if n == 1 || self.format.contains("Constant") {
            return Some(at(0).to_vec());
        }
        if self.format == "DaKeyframes32f" {
            let f = if t.is_finite() { t.max(0.0) as usize } else { 0 };
            return Some(at(f.min(n - 1)).to_vec());
        }
        let d = self.degree as usize;
        let k = self.knots[..n].partition_point(|&x| x <= t).min(n - 1);
        // Window of 2*d knots / controls starting at k - d.
        let m = if looping.is_some() { n - 1 } else { n };
        let mut ti = Vec::with_capacity(2 * d);
        let mut pi: Vec<f32> = Vec::with_capacity(2 * d * dim);
        let mut padded = false;
        for j in 0..2 * d {
            let i = k as isize - d as isize + j as isize;
            let (src, off) = if i >= 0 && (i as usize) < m {
                (i as usize, 0.0)
            } else {
                padded = true;
                match looping {
                    // Granny walks whole periods: index i maps to i mod m, knot shifted by the periods crossed.
                    Some(dur) if m > 0 => {
                        let p = i.div_euclid(m as isize);
                        (i.rem_euclid(m as isize) as usize, p as f32 * dur)
                    }
                    _ => (i.clamp(0, m as isize - 1) as usize, 0.0),
                }
            };
            ti.push(self.knots[src] + off);
            pi.extend_from_slice(at(src));
        }
        if d == 0 {
            return Some(at(k).to_vec());
        }
        if padded && normalize && dim == 4 {
            let mut prev = [0.0f32; 4];
            for c in pi.chunks_mut(4).take(d + 1) {
                if c.iter().zip(&prev).map(|(a, b)| a * b).sum::<f32>() < 0.0 {
                    c.iter_mut().for_each(|x| *x = -*x);
                }
                prev.copy_from_slice(c);
            }
        }
        // de Boor: window knot w[r] = ti[r - d] ([w[d-1], w[d]] = [ti[-1], ti[0]]), controls q[j] = pi[j - d].
        // A zero-length span (repeated knots in the data) would be 0/0 in the game; weight 0 here.
        let mut q: Vec<Vec<f32>> = (0..=d).map(|j| pi[j * dim..(j + 1) * dim].to_vec()).collect();
        for r in 1..=d {
            for j in (r..=d).rev() {
                let (lo, hi) = (ti[j - 1], ti[j + d - r]);
                let den = hi - lo;
                let a = if den != 0.0 { (t - lo) / den } else { 0.0 };
                for c in 0..dim {
                    q[j][c] = (1.0 - a) * q[j - 1][c] + a * q[j][c];
                }
            }
        }
        let mut out = q.pop().unwrap_or_default();
        if normalize && (dim == 3 || dim == 4) {
            let l2: f32 = out.iter().map(|x| x * x).sum();
            if l2 > 0.0 {
                let s = 1.0 / l2.sqrt();
                out.iter_mut().for_each(|x| *x *= s);
            }
        }
        Some(out)
    }
}

// ---- Typed view: skeletons, models, animations ----------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Bone {
    pub name: String,
    pub parent: i32,
    pub transform: Transform,
    /// Column-major (Granny stores it row-vector style: rows = axes, row 3 = translation).
    pub inverse_world: Mat4,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GrannyModel {
    pub name: String,
    pub bones: Vec<Bone>,
    pub initial_placement: Transform,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransformTrack {
    pub name: String,
    pub orientation: Curve,
    pub position: Curve,
    pub scale_shear: Curve,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackGroup {
    pub name: String,
    pub tracks: Vec<TransformTrack>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Animation {
    pub name: String,
    pub duration: f32,
    pub time_step: f32,
    pub track_groups: Vec<TrackGroup>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Granny {
    pub source: String,
    pub models: Vec<GrannyModel>,
    pub animations: Vec<Animation>,
}

impl Granny {
    pub fn from_tree(root: &Value) -> Result<Granny, Error> {
        let mut g = Granny { source: root.get("FromFileName").and_then(Value::str).unwrap_or_default().to_owned(), ..Default::default() };
        for m in root.get("Models").map(Value::array).unwrap_or_default() {
            let name = m.get("Name").and_then(Value::str).unwrap_or_default().to_owned();
            let ip = match m.get("InitialPlacement") { Some(Value::Transform(t)) => *t, _ => Transform::IDENTITY };
            let mut bones = Vec::new();
            for b in m.get("Skeleton").and_then(|s| s.get("Bones")).map(Value::array).unwrap_or_default() {
                let iw = b.get("InverseWorldTransform").map(Value::reals).unwrap_or_default();
                let mut inverse_world = MAT_IDENTITY;
                if iw.len() == 16 {
                    // Rows of the stored matrix are basis vectors (row-vector convention) = our columns.
                    for c in 0..4 {
                        for r in 0..4 {
                            inverse_world[c][r] = iw[c * 4 + r];
                        }
                    }
                }
                bones.push(Bone {
                    name: b.get("Name").and_then(Value::str).unwrap_or_default().to_owned(),
                    parent: b.get("ParentIndex").and_then(Value::int).unwrap_or(-1) as i32,
                    transform: match b.get("Transform") { Some(Value::Transform(t)) => *t, _ => Transform::IDENTITY },
                    inverse_world,
                });
            }
            g.models.push(GrannyModel { name, bones, initial_placement: ip });
        }
        for a in root.get("Animations").map(Value::array).unwrap_or_default() {
            let mut groups = Vec::new();
            for tg in a.get("TrackGroups").map(Value::array).unwrap_or_default() {
                let mut tracks = Vec::new();
                for t in tg.get("TransformTracks").map(Value::array).unwrap_or_default() {
                    let curve = |n: &str| -> Result<Curve, Error> { t.get(n).and_then(|c| c.get("CurveData")).map_or(Ok(Curve { identity: true, ..Default::default() }), Curve::from_value) };
                    tracks.push(TransformTrack {
                        name: t.get("Name").and_then(Value::str).unwrap_or_default().to_owned(),
                        orientation: curve("OrientationCurve")?,
                        position: curve("PositionCurve")?,
                        scale_shear: curve("ScaleShearCurve")?,
                    });
                }
                groups.push(TrackGroup { name: tg.get("Name").and_then(Value::str).unwrap_or_default().to_owned(), tracks });
            }
            let real = |n: &str| a.get(n).and_then(|v| v.reals().first().copied()).unwrap_or(0.0);
            g.animations.push(Animation { name: a.get("Name").and_then(Value::str).unwrap_or_default().to_owned(), duration: real("Duration"), time_step: real("TimeStep"), track_groups: groups });
        }
        Ok(g)
    }

    /// Per bone of model `mi`: `InitialPlacement * boneWorld(t) * InverseWorld` (collision space),
    /// with every animation's track group of the model's name applied (looping on its duration).
    pub fn pose(&self, mi: usize, t: f32) -> Vec<Mat4> {
        let Some(m) = self.models.get(mi) else { return Vec::new() };
        let mut local: Vec<Transform> = m.bones.iter().map(|b| b.transform).collect();
        for a in &self.animations {
            let Some(tg) = a.track_groups.iter().find(|g| g.name == m.name) else { continue };
            let tt = if a.duration > 0.0 { t.rem_euclid(a.duration) } else { 0.0 };
            for tr in &tg.tracks {
                let Some(bi) = m.bones.iter().position(|b| b.name == tr.name) else { continue };
                let mut x = Transform::IDENTITY;
                let step = |c: &Curve| if c.format == "DaKeyframes32f" && a.time_step > 0.0 { tt / a.time_step } else { tt };
                let lp = (a.duration > 0.0).then_some(a.duration);
                if let Some(p) = tr.position.evaluate(step(&tr.position), lp, false) {
                    x.position = [p[0], p.get(1).copied().unwrap_or(0.0), p.get(2).copied().unwrap_or(0.0)];
                }
                if let Some(q) = tr.orientation.evaluate(step(&tr.orientation), lp, true) {
                    if q.len() == 4 {
                        x.orientation = [q[0], q[1], q[2], q[3]];
                    }
                }
                if let Some(s) = tr.scale_shear.evaluate(step(&tr.scale_shear), lp, false) {
                    if s.len() == 9 {
                        x.scale_shear = [[s[0], s[1], s[2]], [s[3], s[4], s[5]], [s[6], s[7], s[8]]];
                    }
                }
                local[bi] = x;
            }
        }
        let mut world: Vec<Mat4> = Vec::with_capacity(local.len());
        for (i, b) in m.bones.iter().enumerate() {
            let l = local[i].matrix();
            let w = if b.parent >= 0 && (b.parent as usize) < i { mat_mul(&world[b.parent as usize], &l) } else { l };
            world.push(w);
        }
        let ip = m.initial_placement.matrix();
        world.iter().zip(&m.bones).map(|(w, b)| mat_mul(&ip, &mat_mul(w, &b.inverse_world))).collect()
    }

    /// Longest animation (seconds).
    pub fn duration(&self) -> f32 {
        self.animations.iter().map(|a| a.duration).fold(0.0, f32::max)
    }
}

// ---- The `.pgeo` type-4 wrapper -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    /// BYTE4N tangent (formats 0 and 4; zero otherwise), as stored (left-handed space).
    pub tangent: [f32; 4],
    /// Blend indices / weights (formats 0, 1, 3); rigid formats use the mesh's bone.
    pub joints: [u8; 4],
    pub weights: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimDraw {
    /// Indices into [`AnimObject::textures`] (-1 = none). Slot 0 = diffuse (UNVERIFIED for 1..3).
    pub textures: [i32; 4],
    pub first_triangle: u32,
    pub triangles: u32,
    /// 0 = first list (+0xC), 1 = second list (+0x10; e.g. a separate alpha / glow pass, UNVERIFIED).
    pub pass: u8,
    /// Words +0x18 / +0x1C / +0x20 (draw 0x82414A00, VERIFIED): two-sided (cull none; alpha-tested depth),
    /// additive (SRCALPHA / ONE, no Z write, DIFF_ONLY technique), SSS mode (non-zero -> *_SSS pixel
    /// shaders; 2 -> PS c1.w = 0.7).
    pub flags: [u32; 3],
    /// +0x24 -> PS c1.x (x 3 / a view value, INFERRED specular power), +0x28 -> PS c0.w (VERIFIED).
    pub params: [f32; 2],
}

/// A light attachment (0x14 bytes) and its glow sprites (VERIFIED from the loader, the VB fill 0x82E0FAD8 and
/// the draw 0x82E16C80; docs/PROPS.md "Animated objects"). Drawn like the static Cone glows (type 6) with the
/// mesh's world matrix; on when SwitchOnLights >= 2 x the mesh's `model_data` threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimLightGroup {
    /// +0x0: texture reference (index into [`AnimObject::textures`], -1 = none) -> sampler 0.
    pub texture: i32,
    /// +0x4: animation strip texture (-1 = none; else the animated sprite shaders).
    pub anim_texture: i32,
    /// +0xC: UVScale index (VS c160, the static glows' table 0x834AD630).
    pub uv_scale: u32,
    pub lights: Vec<AnimLight>,
}

/// A 0x38-byte light: the static Cone glow record without its threshold (mesh / bone local, left-handed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimLight {
    pub position: [f32; 3],
    pub direction: [f32; 3],
    /// Inner / outer view angles, radians (VS TEX2 = (cos outer, cos inner)).
    pub angles: [f32; 2],
    /// +0x20: depth pull towards the camera.
    pub pull: f32,
    /// +0x24: only copied into the light-glow manager descriptor.
    pub unused: f32,
    /// +0x28: sprite half-size.
    pub half_size: f32,
    /// +0x2C / +0x30: animation phase / rate (cycles per second).
    pub phase: f32,
    pub rate: f32,
    /// +0x34..0x36: R, G, B.
    pub colour: [u8; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnimMesh {
    /// Bone of the model's skeleton; None = per-vertex blend indices.
    pub bone: Option<u32>,
    pub format: u32,
    /// +0x20 non-zero: rigid draws use the LIGHT_SHAFT technique (draw 0x82414A00, VERIFIED selection;
    /// meaning INFERRED from the shader names).
    pub light_shaft: bool,
    /// +0x3C raw word -> VS/PS c148 ModelData.x (VERIFIED as passed; read as f32 bits, INFERRED).
    pub model_data: u32,
    /// Light attachments (+0x1C).
    pub lights: Vec<AnimLightGroup>,
    pub vertices: Vec<AnimVertex>,
    pub indices: Vec<u16>,
    pub draws: Vec<AnimDraw>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnimModel {
    /// LOD0..2 meshes.
    pub lods: [Vec<AnimMesh>; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnimObject {
    pub name: String,
    /// PVS texture indices (`Colorado_00.pvs` 28-byte table).
    pub textures: Vec<u32>,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    /// Parallel to [`Granny::models`].
    pub models: Vec<AnimModel>,
    pub granny: Granny,
}

fn half(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1F) as i32;
    let m = (h & 0x3FF) as f32;
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => 0.0,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Parses a type-4 `.pgeo` (`Anim_ANIM_*`).
pub fn parse_anim_object(d: &[u8]) -> Result<AnimObject, Error> {
    let u = |o: usize| d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("anim object"));
    if d.get(..4) != Some(b"OEGP") || u(0x30)? != 4 {
        return Err(Error::Unsupported("not a type-4 .pgeo".into()));
    }
    let al = |o: usize, a: usize| o.next_multiple_of(a);
    let name_len = u(0x3C)? as usize;
    let name_raw = d.get(0x60..0x60 + name_len).ok_or(Error::Truncated("anim name"))?;
    let name: String = name_raw[..name_raw.iter().position(|&b| b == 0).unwrap_or(name_len)].iter().map(|&b| b as char).collect();
    let mut o = al(0x60 + name_len, 4);
    let textures = (0..u(0x40)? as usize).map(|i| u(o + 8 * i)).collect::<Result<Vec<_>, _>>()?;
    o += 8 * textures.len();
    o = al(o, 16);
    let h = o;
    let f = |o: usize| u(o).map(f32::from_bits);
    let (bbox_min, bbox_max) = ([f(h)?, f(h + 4)?, f(h + 8)?], [f(h + 0x10)?, f(h + 0x14)?, f(h + 0x18)?]);
    o += 0x60;
    let n44 = u(h + 0x44)? as usize;
    if n44 > 0 {
        o = al(o, 4);
        let sizes = (0..n44).map(|i| u(o + 4 * i)).collect::<Result<Vec<_>, _>>()?;
        o += 4 * n44 + sizes.iter().map(|&s| s as usize).sum::<usize>();
    }
    let nm = u(h + 0x3C)? as usize;
    if nm > 4096 {
        return Err(Error::Unsupported(format!("anim object: {nm} models")));
    }
    let mut mesh_at: Vec<(usize, usize, usize)> = Vec::new(); // (model, lod, record)
    let mut light_groups: std::collections::HashMap<usize, Vec<AnimLightGroup>> = std::collections::HashMap::new();
    if nm > 0 {
        o = al(o, 4);
        let models_at = o;
        o += 0x3C * nm;
        for mi in 0..nm {
            for lod in 0..3 {
                let c = u(models_at + 0x3C * mi + 0x14 + 16 * lod)? as usize;
                if c == 0 {
                    continue;
                }
                if c > 4096 {
                    return Err(Error::Unsupported("anim object: mesh count".into()));
                }
                o = al(o, 4);
                let a = o;
                o += 0x44 * c;
                let recs: Vec<usize> = (0..c).map(|k| a + 0x44 * k).collect();
                for &m in &recs {
                    mesh_at.push((mi, lod, m));
                    let n = u(m + 0x18)? as usize;
                    if n > 0 {
                        o = al(o, 4);
                        let sizes = (0..n).map(|i| u(o + 4 * i)).collect::<Result<Vec<_>, _>>()?;
                        o += 4 * n + sizes.iter().map(|&s| s as usize).sum::<usize>();
                    }
                }
                for &m in &recs {
                    let n = u(m + 0x1C)? as usize;
                    let mut groups = Vec::new();
                    if n > 0 {
                        o = al(o, 4);
                        let r = o;
                        o += 0x14 * n;
                        for k in 0..n {
                            let a = r + 0x14 * k;
                            let count = u(a + 8)? as usize;
                            o = al(o, 4);
                            let mut lights = Vec::new();
                            for j in 0..count {
                                let l = o + 0x38 * j;
                                let f = |x: usize| u(l + x).map(f32::from_bits);
                                let rgb = d.get(l + 0x34..l + 0x37).ok_or(Error::Truncated("anim light"))?;
                                lights.push(AnimLight {
                                    position: [f(0)?, f(4)?, f(8)?],
                                    direction: [f(0xC)?, f(0x10)?, f(0x14)?],
                                    angles: [f(0x18)?, f(0x1C)?],
                                    pull: f(0x20)?,
                                    unused: f(0x24)?,
                                    half_size: f(0x28)?,
                                    phase: f(0x2C)?,
                                    rate: f(0x30)?,
                                    colour: [rgb[0], rgb[1], rgb[2]],
                                });
                            }
                            o += 0x38 * count;
                            groups.push(AnimLightGroup { texture: u(a)? as i32, anim_texture: u(a + 4)? as i32, uv_scale: u(a + 0xC)?, lights });
                        }
                    }
                    light_groups.insert(m, groups);
                }
            }
        }
    }
    let mut draws: Vec<Vec<AnimDraw>> = vec![Vec::new(); mesh_at.len()];
    for (i, &(_, _, m)) in mesh_at.iter().enumerate() {
        for (pass, k) in [(0u8, 0xC), (1, 0x10)] {
            let n = u(m + k)? as usize;
            if n == 0 {
                continue;
            }
            o = al(o, 4);
            for j in 0..n {
                let r = o + 0x2C * j;
                draws[i].push(AnimDraw {
                    textures: [u(r)? as i32, u(r + 4)? as i32, u(r + 8)? as i32, u(r + 12)? as i32],
                    first_triangle: u(r + 16)?,
                    triangles: u(r + 20)?,
                    pass,
                    flags: [u(r + 24)?, u(r + 28)?, u(r + 32)?],
                    params: [f(r + 36)?, f(r + 40)?],
                });
            }
            o += 0x2C * n;
        }
    }
    o = al(o, 4);
    if d.get(o..o + 16) != Some(&GR2_MAGIC_FH1[..]) {
        return Err(Error::Unsupported(format!("anim object: Granny not at {o:#x}")));
    }
    let gsize = u(h + 0x24)? as usize;
    let granny = Granny::from_tree(&parse_gr2(d.get(o..o + gsize).ok_or(Error::Truncated("granny"))?)?)?;
    o += gsize;
    o = al(o, 4);
    let mut indices: Vec<Vec<u16>> = vec![Vec::new(); mesh_at.len()];
    for (i, &(_, _, m)) in mesh_at.iter().enumerate() {
        let n = u(m + 8)? as usize;
        if n > 0 && u(m + 0x34)? != 0 {
            o = al(o, 4);
            let b = d.get(o..o + 2 * n).ok_or(Error::Truncated("anim indices"))?;
            indices[i] = b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            o += 2 * n;
        }
    }
    let mut models: Vec<AnimModel> = (0..nm).map(|_| AnimModel { lods: Default::default() }).collect();
    for (i, &(mi, lod, m)) in mesh_at.iter().enumerate() {
        let (nv, format) = (u(m + 4)? as usize, u(m + 0x40)?);
        let mut vertices = Vec::new();
        if nv > 0 && u(m + 0x2C)? != 0 {
            let (stride, w_at, j_at, n_at, t_at) = match format {
                5 => (12, None, None, 8, None),
                4 => (16, None, None, 8, Some(12)),
                3 => (16, None, Some(8), 12, None),
                1 => (20, Some(8), Some(12), 16, None),
                0 => (24, Some(8), Some(12), 16, Some(20)),
                _ => return Err(Error::Unsupported(format!("anim vertex format {format}"))),
            };
            o = al(o, 4);
            let b = d.get(o..o + stride * nv).ok_or(Error::Truncated("anim vertices"))?;
            for v in b.chunks_exact(stride) {
                let h = |k: usize| half(u16::from_be_bytes([v[2 * k], v[2 * k + 1]]));
                // 4-byte elements: x = lowest byte of the big-endian word (byte 3).
                let s8 = |a: usize, k: usize| (v[a + 3 - k] as i8 as f32 / 127.0).max(-1.0);
                let normal = [s8(n_at, 0), s8(n_at, 1), s8(n_at, 2)];
                let joints = j_at.map_or([0; 4], |a| [v[a + 3], v[a + 2], v[a + 1], v[a]]);
                let weights = w_at.map_or([1.0, 0.0, 0.0, 0.0], |a| [v[a + 3], v[a + 2], v[a + 1], v[a]].map(|x| x as f32 / 255.0));
                let tangent = t_at.map_or([0.0; 4], |a| [s8(a, 0), s8(a, 1), s8(a, 2), s8(a, 3)]);
                vertices.push(AnimVertex { position: [h(0), h(1), h(2)], normal, uv: [h(3), s8(n_at, 3) * 0.5 + 0.5], tangent, joints, weights });
            }
            o += stride * nv;
        }
        let bone = u(m)?;
        if mi >= models.len() || lod >= 3 {
            return Err(Error::Unsupported("anim object: mesh model".into()));
        }
        models[mi].lods[lod].push(AnimMesh { bone: (bone != u32::MAX).then_some(bone), format, light_shaft: u(m + 0x20)? != 0, model_data: u(m + 0x3C)?, lights: light_groups.remove(&m).unwrap_or_default(), vertices, indices: std::mem::take(&mut indices[i]), draws: std::mem::take(&mut draws[i]) });
    }
    if o != d.len() {
        return Err(Error::SizeMismatch { expected: d.len(), got: o });
    }
    if models.len() != granny.models.len() {
        return Err(Error::Unsupported(format!("anim object: {} models vs {} Granny models", models.len(), granny.models.len())));
    }
    Ok(AnimObject { name, textures, bbox_min, bbox_max, models, granny })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(degree: u32, dim: usize, knots: &[f32], controls: &[f32]) -> Curve {
        Curve { format: "DaK32fC32f".into(), degree, dim, knots: knots.to_vec(), controls: controls.to_vec(), identity: false }
    }

    /// Granny's SampleBSpline1x1 (0x829C7298): lerp between controls K-1 and K over [knot K-1, knot K].
    #[test]
    fn bspline_degree1_is_lerp() {
        let c = curve(1, 1, &[0.0, 1.0, 3.0], &[0.0, 10.0, 30.0]);
        assert_eq!(c.evaluate(0.5, None, false), Some(vec![5.0]));
        assert_eq!(c.evaluate(2.0, None, false), Some(vec![20.0]));
        assert_eq!(c.evaluate(3.0, None, false), Some(vec![30.0]));
    }

    /// Degree 3 against the closed-form weights of SampleBSpline3x1 (0x829C7D80), mid-curve.
    #[test]
    fn bspline_degree3_matches_game_weights() {
        let k = [0.0, 0.5, 1.5, 2.0, 3.5, 4.0, 5.0, 6.5];
        let p = [1.0, -2.0, 4.0, 0.5, 3.0, -1.0, 2.0, 7.0];
        let c = curve(3, 1, &k, &p);
        let t = 2.7; // knot index K = 4 (first knot > t)
        let ti = |i: isize| k[(4 + i) as usize];
        let pi = |i: isize| p[(4 + i) as usize];
        let (f9, f6, f4) = ((t - ti(-1)) / (ti(0) - ti(-1)), (t - ti(-2)) / (ti(0) - ti(-2)), (t - ti(-1)) / (ti(1) - ti(-1)));
        let (f3, f1, f11) = ((t - ti(-3)) / (ti(0) - ti(-3)), (t - ti(-2)) / (ti(1) - ti(-2)), (t - ti(-1)) / (ti(2) - ti(-1)));
        let (a10, a7, a5) = (1.0 - f9, 1.0 - f6, 1.0 - f4);
        let f4b = f4 * f9;
        let (a31, a30, a0) = (1.0 - f3, 1.0 - f1, 1.0 - f11);
        let w6 = a10 * f6;
        let w10 = a7 * a10;
        let w9b = a5 * f9;
        let w7 = f4b * f11;
        let w5 = w10 * f3;
        let s3 = w9b + w6;
        let w11 = a31 * w10;
        let w10b = s3 * f1;
        let w9 = a30 * s3 + w5;
        let w6b = a0 * f4b + w10b;
        let game = w11 * pi(-3) + w9 * pi(-2) + w6b * pi(-1) + w7 * pi(0);
        let ours = c.evaluate(t, None, false).unwrap()[0];
        assert!((ours - game).abs() < 1e-5, "{ours} vs {game}");
    }

    /// Looping wraps the window round (last knot = first + duration) instead of clamping.
    #[test]
    fn bspline_loop_wraps() {
        let c = curve(2, 1, &[0.0, 1.0, 2.0, 3.0, 4.0], &[0.0, 1.0, 2.0, 1.0, 0.0]);
        let a = c.evaluate(0.0, Some(4.0), false).unwrap()[0];
        let b = c.evaluate(4.0 - 1e-4, Some(4.0), false).unwrap()[0];
        assert!((a - b).abs() < 1e-3, "{a} vs {b}");
    }

    /// All 33 Colorado animated objects parse to their end, every mesh's indices are in range, and
    /// every draw range lies inside its mesh.
    #[test]
    fn colorado_anim_objects() {
        let disc = std::env::var("FH1_DISC").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../disc").into());
        let Ok(mut ar) = crate::zip::Archive::open(std::path::Path::new(&disc).join("media/tracks/colorado/bin.zip")) else {
            eprintln!("no disc, skipped");
            return;
        };
        let mut seen = std::collections::HashSet::new();
        let (mut objects, mut meshes, mut anims) = (0, 0, 0);
        for e in ar.entries.clone() {
            let n = e.name.to_ascii_lowercase();
            if !n.ends_with(".pgeo") || !seen.insert(n.clone()) {
                continue;
            }
            let d = ar.read(&e).unwrap();
            if d.get(0x30..0x34) != Some(&[0, 0, 0, 4]) {
                continue;
            }
            let o = parse_anim_object(&d).unwrap_or_else(|err| panic!("{n}: {err}"));
            objects += 1;
            anims += o.granny.animations.len();
            for (mi, m) in o.models.iter().enumerate() {
                for mesh in m.lods.iter().flatten() {
                    meshes += 1;
                    assert!(mesh.indices.iter().all(|&i| (i as usize) < mesh.vertices.len()), "{n}: index range");
                    for dr in &mesh.draws {
                        assert!(3 * (dr.first_triangle + dr.triangles) as usize <= mesh.indices.len(), "{n}: draw range");
                    }
                    if let Some(b) = mesh.bone {
                        assert!((b as usize) < o.granny.models[mi].bones.len(), "{n}: bone");
                    }
                }
                let _ = o.granny.pose(mi, 1.0);
            }
        }
        assert_eq!(objects, 33);
        eprintln!("{objects} objects, {meshes} meshes, {anims} animations");
    }
}
