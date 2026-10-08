//! `.rmb.bin` track models (CTrackModel version 6, big-endian), the visual meshes of the open world.
//!
//! Layout from the Colorado recon (`docs/COLORADO_RECON.md`, ".rmb.bin visual meshes"):
//! header (version, bounds, identity world matrix, 5 words ending in the submodel count), then
//! submodels, each with a name, one vertex buffer (stride 16..36, position first as f32 x, y, z in
//! world space) and meshes (triangle strips or lists into that buffer). Material / command-buffer
//! data between meshes and submodels isn't decoded; the parser skips to the next recognisable
//! record the same way the recon probe does.
//!
//! Positions are in the engine's (right-handed) space already — unlike the collision.

use crate::Error;

#[derive(Debug, Clone)]
pub struct TrackModel {
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub submodels: Vec<SubModel>,
    pub materials: Vec<Material>,
    /// Shader paths, e.g. `shaders\track\h_road_diff1_norm1_modulate_ao_lm.fx`.
    pub shaders: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Material {
    /// Index into [`TrackModel::shaders`].
    pub shader: u32,
    pub technique: u32,
    pub vs_constants: Vec<[f32; 4]>,
    pub ps_constants: Vec<[f32; 4]>,
    /// Texture sampler slots (-2 = unused). How a slot maps to a global texture id is still open.
    pub texture_slots: Vec<i32>,
}

impl TrackModel {
    /// The shader a submodel's vertex layout comes from (its first mesh's material's shader).
    pub fn submodel_shader(&self, sub: &SubModel) -> Option<&str> {
        let mat = sub.meshes.iter().map(|m| m.material).find(|&m| m != u32::MAX)?;
        let shader = self.materials.get(mat as usize)?.shader;
        self.shaders.get(shader as usize).map(String::as_str)
    }
}

#[derive(Debug, Clone)]
pub struct SubModel {
    pub name: String,
    pub offset: [f32; 3],
    pub stride: usize,
    /// Raw big-endian vertex data (`count × stride`); attributes after the position aren't decoded yet.
    pub vertex_data: Vec<u8>,
    pub positions: Vec<[f32; 3]>,
    pub meshes: Vec<Mesh>,
}

#[derive(Debug, Clone)]
pub struct Mesh {
    pub name: String,
    pub material: u32,
    /// (offset u, offset v, scale u, scale v) — meaning still to be confirmed.
    pub uv_offset_scale: [f32; 4],
    /// Triangle list (strips expanded, restarts and degenerates dropped).
    pub indices: Vec<u32>,
}

/// Decoded per-vertex attributes of a submodel (attributes its layout lacks are left empty).
#[derive(Debug, Clone, Default)]
pub struct Attributes {
    pub normals: Vec<[f32; 3]>,
    /// DEC3N like the normal (no handedness sign; the shaders build the binormal).
    pub tangents: Vec<[f32; 3]>,
    /// Raw texcoords in 0..1. The track vertex shaders apply each mesh's `uv_offset_scale` to all three
    /// sets (offset + raw * scale).
    pub uv0: Vec<[f32; 2]>,
    pub uv1: Vec<[f32; 2]>,
    pub uv2: Vec<[f32; 2]>,
    /// Fourth texcoord set (FM4 track shaders; raw 0..1 like the others).
    pub uv3: Vec<[f32; 2]>,
    /// DEC3N binormal (FM4 track shaders declare it next to the tangent).
    pub binormals: Vec<[f32; 3]>,
    /// RGBA, as stored in D3DCOLOR (ARGB in memory).
    pub colors: Vec<[u8; 4]>,
}

impl SubModel {
    /// Decode attributes with the vertex declaration of the submodel's shader.
    pub fn attributes(&self, decl: &crate::fxobj::VertexDecl) -> Attributes {
        use crate::fxobj::*;
        let mut a = Attributes::default();
        let be16 = |v: &[u8], o: usize| u16::from_be_bytes([v[o], v[o + 1]]);
        let be32 = |v: &[u8], o: usize| u32::from_be_bytes(v[o..o + 4].try_into().unwrap());
        let get = |usage: u8, index: u8| decl.find(usage, index).filter(|e| e.offset as usize + e.size() <= self.stride);
        let verts = || self.vertex_data.chunks_exact(self.stride);
        if let Some(e) = get(USAGE_NORMAL, 0).filter(|e| e.decl_type == TYPE_DEC3N) {
            a.normals = verts().map(|v| dec3n(be32(v, e.offset as usize))).collect();
        }
        if let Some(e) = get(USAGE_TANGENT, 0).filter(|e| e.decl_type == TYPE_DEC3N) {
            a.tangents = verts().map(|v| dec3n(be32(v, e.offset as usize))).collect();
        }
        let uv = |index: u8| -> Vec<[f32; 2]> {
            match get(USAGE_TEXCOORD, index).filter(|e| e.decl_type == TYPE_USHORT2N) {
                Some(e) => verts()
                    .map(|v| {
                        let o = e.offset as usize;
                        [be16(v, o) as f32 / 65535.0, be16(v, o + 2) as f32 / 65535.0]
                    })
                    .collect(),
                None => Vec::new(),
            }
        };
        a.uv0 = uv(0);
        a.uv1 = uv(1);
        a.uv2 = uv(2);
        a.uv3 = uv(3);
        if let Some(e) = get(USAGE_BINORMAL, 0).filter(|e| e.decl_type == TYPE_DEC3N) {
            a.binormals = verts().map(|v| dec3n(be32(v, e.offset as usize))).collect();
        }
        if let Some(e) = get(USAGE_COLOR, 0).filter(|e| e.decl_type == TYPE_D3DCOLOR) {
            a.colors = verts()
                .map(|v| {
                    let o = e.offset as usize;
                    [v[o + 1], v[o + 2], v[o + 3], v[o]]
                })
                .collect();
        }
        a
    }

    /// LOD level from a `_LODnn` suffix (0 when absent).
    pub fn lod(&self) -> u32 {
        let upper = self.name.to_ascii_uppercase();
        let Some(at) = upper.rfind("_LOD") else { return 0 };
        upper[at + 4..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)
    }

    /// What a submodel is for, from its name (Colorado's naming; survey: `examples/rmb_lods.rs`).
    pub fn class(&self) -> Class {
        let n = self.name.to_ascii_uppercase();
        // Cages first: `TERR_UberLOD_Patch01_CAGE_LOD00` is a cage (its `.pvs` record has no draw-band bits).
        if n.contains("_CAGE") {
            Class::Cage
        } else if n.contains("UBERLOD") {
            Class::UberLod
        } else if n.contains("MIDDIST") || n.contains("_MID_") {
            Class::MidDistance
        } else if n.contains("SHADOWCASTER") || n.starts_with("SHADOWBOX") || n.starts_with("SHADOW_TERRAIN") {
            Class::Shadow
        } else if n.starts_with("TERR_CUBE_") {
            Class::TerrainCube
        } else {
            Class::Normal
        }
    }

    /// Not drawn with the full-detail scenery: distant (mid-distance / uber-LOD) geometry, shadow-only
    /// proxies, cages and terrain cubes. `*_NOLOD` models ("no LOD chain") are normal, always-drawn ones.
    pub fn is_helper(&self) -> bool {
        self.class() != Class::Normal
    }
}

/// See [`SubModel::class`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    Normal,
    /// `*MIDDIST*`, `*_MID_*`: backdrop terrain/objects for the middle distance (with their own LOD chains).
    MidDistance,
    /// `TERR_UberLOD_Patch*`, `UberLOD_04B*`: the farthest terrain patches (the horizon), up to ~11 km out.
    UberLod,
    /// `SHADOWCASTER`, `SHADOWBOX*`, `Shadow_Terrain*`: shadow-only proxies (flat yellow texture).
    Shadow,
    /// `*_CAGE*`: bounding cages (never drawn: their `.pvs` records have no draw-band bits).
    Cage,
    /// `TERR_CUBE_*`: purpose not confirmed yet.
    TerrainCube,
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.d.get(self.p..self.p + 4).ok_or(Error::Truncated("rmb"))?;
        self.p += 4;
        Ok(u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, Error> {
        self.u32().map(f32::from_bits)
    }
    fn vec4(&mut self) -> Result<[f32; 4], Error> {
        Ok([self.f32()?, self.f32()?, self.f32()?, self.f32()?])
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let b = self.d.get(self.p..self.p + n).ok_or(Error::Truncated("rmb"))?;
        self.p += n;
        Ok(b)
    }
    fn string(&mut self) -> Result<String, Error> {
        let n = self.u32()? as usize;
        if n > 1024 {
            return Err(Error::BadMagic("rmb string length"));
        }
        // Names are Latin-1 (one Colorado asset is `TERR_CÖRD...`).
        Ok(self.bytes(n)?.iter().map(|&b| b as char).collect())
    }
    fn peek_u32(&self, at: usize) -> Option<u32> {
        self.d.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
}

pub fn parse(d: &[u8]) -> Result<TrackModel, Error> {
    let mut r = Reader { d, p: 0 };
    if r.u32()? != 6 {
        return Err(Error::BadMagic("rmb: CTrackModel version 6 expected"));
    }
    let bmin = r.vec4()?;
    let bmax = r.vec4()?;
    r.bytes(64)?; // world matrix (identity)
    for _ in 0..4 {
        r.u32()?;
    }
    let count = r.u32()? as usize;
    if count > 100_000 {
        return Err(Error::BadMagic("rmb submodel count"));
    }
    let mut submodels = Vec::with_capacity(count);
    for si in 0..count {
        submodels.push(parse_submodel(&mut r)?);
        if si + 1 < count {
            skip_to_submodel(&mut r)?;
        }
    }
    // FH2 (Anthem, 29 of 10,545 models) has extra `u32 1` words before the `1,1,1,1` marker, so the first marker
    // found can be one word early: retry from the next word (FH1 models all parse on the first try).
    let start = r.p;
    let mut materials = parse_materials(&mut r);
    for k in 1..=8 {
        if materials.is_ok() {
            break;
        }
        r.p = start + 4 * k;
        materials = parse_materials(&mut r);
    }
    let (materials, shaders) = materials?;
    Ok(TrackModel { bounds_min: [bmin[0], bmin[1], bmin[2]], bounds_max: [bmax[0], bmax[1], bmax[2]], submodels, materials, shaders })
}

fn parse_submodel(r: &mut Reader) -> Result<SubModel, Error> {
    if r.u32()? != 1 {
        return Err(Error::BadMagic("rmb submodel version"));
    }
    let offset = r.vec4()?;
    r.vec4()?; // min (relative to offset)
    r.vec4()?; // max
    if r.u32()? != 1 {
        return Err(Error::BadMagic("rmb TSubModel version"));
    }
    let name = r.string()?;
    // Vertex buffer version 3 (FH1/FH2) has a flags word; FM4's version 2 has none.
    let vb_version = r.u32()?;
    if vb_version != 3 && vb_version != 2 {
        return Err(Error::BadMagic("rmb vertex buffer version"));
    }
    let count = r.u32()? as usize;
    let stride = r.u32()? as usize;
    if vb_version >= 3 {
        r.u32()?; // flags
    }
    if !(12..=128).contains(&stride) {
        return Err(Error::BadMagic("rmb vertex stride"));
    }
    let vertex_data = r.bytes(count * stride)?.to_vec();
    let positions = vertex_data
        .chunks_exact(stride)
        .map(|v| {
            let f = |o: usize| f32::from_be_bytes(v[o..o + 4].try_into().unwrap());
            [f(0), f(4), f(8)]
        })
        .collect();

    if r.u32()? != 1 {
        return Err(Error::BadMagic("rmb mesh container version"));
    }
    let mesh_count = r.u32()? as usize;
    let mut meshes = Vec::with_capacity(mesh_count);
    for mi in 0..mesh_count {
        let (a, ver) = (r.u32()?, r.u32()?);
        if (a, ver) != (1, 2) {
            return Err(Error::BadMagic("rmb mesh header"));
        }
        let mname = r.string()?;
        let _lod = r.u32()?;
        let primitive = r.u32()?;
        let material = r.u32()?;
        r.u32()?;
        r.vec4()?; // position offset
        r.vec4()?; // position scale
        let uv_offset_scale = r.vec4()?;
        // Index buffer version 4 (FH1/FH2) has a flags word; FM4's version 3 has none.
        let ib_version = r.u32()?;
        if ib_version >= 4 {
            r.u32()?; // flags
        }
        let (n, size) = (r.u32()? as usize, r.u32()? as usize);
        if !(3..=4).contains(&ib_version) || (size != 2 && size != 4) {
            return Err(Error::BadMagic("rmb index buffer"));
        }
        let raw = r.bytes(n * size)?;
        let idx: Vec<u32> = raw
            .chunks_exact(size)
            .map(|c| if size == 2 { u16::from_be_bytes([c[0], c[1]]) as u32 } else { u32::from_be_bytes(c.try_into().unwrap()) })
            .collect();
        let indices = match primitive {
            6 => strip_to_list(&idx),
            _ => idx.chunks_exact(3).flatten().copied().collect(),
        };
        if indices.iter().any(|&i| i as usize >= count) {
            return Err(Error::BadMagic("rmb index out of range"));
        }
        meshes.push(Mesh { name: mname, material, uv_offset_scale, indices });
        // Undecoded trailer up to the next mesh header (1, 2).
        if mi + 1 < mesh_count {
            while r.p + 8 <= r.d.len() && !(r.peek_u32(r.p) == Some(1) && r.peek_u32(r.p + 4) == Some(2)) {
                r.p += 4;
            }
        }
    }
    Ok(SubModel { name, offset: [offset[0], offset[1], offset[2]], stride, vertex_data, positions, meshes })
}

/// After the last submodel: its last mesh's trailer, `u32 1, 1, 1, 1`, the materials, then
/// `u32 1, u32 shader_count` and length-prefixed (NUL-included) shader paths.
fn parse_materials(r: &mut Reader) -> Result<(Vec<Material>, Vec<String>), Error> {
    // Find the 1,1,1,1 marker just after the trailer.
    let mut found = false;
    for _ in 0..8 {
        if (0..4).all(|k| r.peek_u32(r.p + 4 * k) == Some(1)) {
            found = true;
            break;
        }
        r.p += 4;
    }
    if !found {
        return Err(Error::BadMagic("rmb: material block not found"));
    }
    r.p += 16;
    let count = r.u32()? as usize;
    if count > 10_000 {
        return Err(Error::BadMagic("rmb material count"));
    }
    let vec4s = |r: &mut Reader| -> Result<Vec<[f32; 4]>, Error> {
        r.u32()?;
        let n = r.u32()? as usize;
        if n > 4096 {
            return Err(Error::BadMagic("rmb shader constant count"));
        }
        (0..n).map(|_| r.vec4()).collect()
    };
    let mut materials = Vec::with_capacity(count);
    for _ in 0..count {
        if r.u32()? != 3 {
            return Err(Error::BadMagic("rmb material version"));
        }
        let shader = r.u32()?;
        let technique = r.u32()?;
        let vs_constants = vec4s(r)?;
        let ps_constants = vec4s(r)?;
        r.u32()?;
        let n = r.u32()? as usize;
        if n > 256 {
            return Err(Error::BadMagic("rmb texture slot count"));
        }
        let texture_slots = (0..n).map(|_| r.u32().map(|v| v as i32)).collect::<Result<_, _>>()?;
        materials.push(Material { shader, technique, vs_constants, ps_constants, texture_slots });
    }
    r.u32()?;
    let n = r.u32()? as usize;
    if n > 10_000 {
        return Err(Error::BadMagic("rmb shader count"));
    }
    let shaders = (0..n).map(|_| r.string().map(|s| s.trim_end_matches('\0').to_owned())).collect::<Result<_, _>>()?;
    Ok((materials, shaders))
}

/// Skip undecoded material data to the next submodel record:
/// `u32 1, vec4 offset, vec4 min, vec4 max, u32 1, u32 name_len, printable name`.
fn skip_to_submodel(r: &mut Reader) -> Result<(), Error> {
    while r.p + 64 <= r.d.len() {
        if r.peek_u32(r.p) == Some(1) && r.peek_u32(r.p + 52) == Some(1) {
            if let Some(n) = r.peek_u32(r.p + 56) {
                let n = n as usize;
                if (1..200).contains(&n) && r.d.get(r.p + 60..r.p + 60 + n).is_some_and(|s| s.iter().all(|&c| (32..127).contains(&c) || c >= 0xC0)) {
                    return Ok(());
                }
            }
        }
        r.p += 1;
    }
    Err(Error::Truncated("rmb: next submodel not found"))
}

fn strip_to_list(strip: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    for run in strip.split(|&i| i == 0xFFFF || i >= 0x00FF_FFFF) {
        for (k, w) in run.windows(3).enumerate() {
            let (a, b, c) = if k % 2 == 0 { (w[0], w[1], w[2]) } else { (w[1], w[0], w[2]) };
            if a != b && b != c && a != c {
                out.extend([a, b, c]);
            }
        }
    }
    out
}
