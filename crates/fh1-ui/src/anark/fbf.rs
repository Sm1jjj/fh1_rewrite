//! `.fbf` (flat object file): `u32 version` (1006..=1008), `u32 count`, object records up to and
//! including the group record with id 1 ("Scene"), then the mesh blob. VERIFIED: all 205 files parse
//! exactly to EOF; every vertex/index buffer is accounted for (gaps < 4 bytes, zero tail).

use crate::reader::Reader;
use crate::{need, Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Fbf {
    pub version: u32,
    /// = number of object records (VERIFIED 205/205).
    pub count: u32,
    pub records: Vec<Record>,
    pub meshes: Vec<Mesh>,
    /// File offset where the mesh blob starts.
    pub records_end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Raw type code (0 group, 1 text, 2 camera, 3 layer, 4 model, 5 material, 6 light, 7 image, 11 custom).
    pub type_code: u32,
    /// Scene id (= bgf S1 / bsg id).
    pub id: u32,
    pub name: String,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    /// Group / component: no payload.
    Group,
    Text(Text),
    /// Row-major 4×4; translation row = (x, y, −z) of the camera.
    Camera { matrix: [f32; 16] },
    /// Unknown: words 0, floats (1, 1).
    Layer { words: [u32; 4], floats: [f32; 2] },
    /// `mesh` indexes [`Fbf::meshes`]; each mesh is used by exactly one model (VERIFIED). `f1` = 1.0.
    Model { mesh: u32, f1: f32 },
    Material(Material),
    /// `light_type` 3 = directional? + 25 floats (colours, direction…), not decoded.
    Light { light_type: u32, raw: [f32; 25] },
    Image(Image),
    /// Only `Map_Renderer` in 1000_HorizonTest.
    Custom { value: u32 },
}

/// Text record payload. The displayed default string is the bgf `textstring`, not stored here.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub u0: [u32; 2],
    /// String-table key (`ingame:IDS_Race_Starts_In`), often empty.
    pub loc_key: String,
    /// = bgf boxheight/boxwidth (0.25 typically; meaning unknown).
    pub boxheight: f32,
    pub boxwidth: f32,
    /// e.g. `horizon_e`, `e_helvetica67-condensedmedium medium`.
    pub font: String,
    pub size: u32,
    /// 0 left, 1 centre, 2 right (GUESS).
    pub horzalign: u32,
    /// 1 middle, 0 top, 2 bottom (GUESS).
    pub vertalign: u32,
    pub renderstyle: u32,
    pub leading: i32,
    pub tracking: f32,
    pub u1: [u32; 2],
    pub wordwrap: u8,
    /// v1008 only: authored width in px of the default string (GUESS).
    pub text_width: Option<f32>,
    /// v≥1007 only: `Some(flag)` is the has-extra byte.
    pub has_extra: Option<u8>,
    /// The 59-byte block when `has_extra != 0` (shadow/outline params? GUESS).
    pub extra: Option<Vec<u8>>,
}

/// Material payload (104 bytes). Colours are 0..1.
#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
    pub ambient: [f32; 4],
    pub emissive: [f32; 3],
    /// Always 1.0.
    pub f15: f32,
    pub specular_power: f32,
    /// Id of the type-4 model record (VERIFIED).
    pub model: u32,
    /// Index group of that model's mesh this material draws (VERIFIED < groups).
    pub submesh: u32,
    /// 2 (shade mode?).
    pub shade: u32,
    /// (0, 4, 6, 7|1, 0, 2): `blend[3]` 7 = normal alpha, 1 = additive (GUESS).
    pub blend: [u32; 6],
}

impl Material {
    /// GUESS: `blend[3] == 1` → additive.
    pub fn is_additive(&self) -> bool {
        self.blend[3] == 1
    }
}

/// Image payload: a texture slot on a material.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    /// Id of the type-5 material record (VERIFIED 14,808/14,808).
    pub material: u32,
    /// `GAME:\MEDIA\UI\TEXTURES\HORIZON\DIRTMASKS\BGNOISE01.TGA`.
    pub path: String,
    /// Row-vector 4×4: `u' = u*m0 + v*m4 + m12`, `v' = u*m1 + v*m5 + m13`.
    pub uv_matrix: [f32; 16],
    /// Degrees.
    pub rotationuv: f32,
    pub positionu: f32,
    pub positionv: f32,
    pub scaleu: f32,
    pub scalev: f32,
    pub pivotu: f32,
    pub pivotv: f32,
    /// Always 0.
    pub f23: f32,
    /// 4, or 3 for 1,595 first slots (GUESS: filter/usage).
    pub slot_mode: u32,
    pub u25: u32,
    pub u26: u32,
    /// 0 repeat, 2 repeat in practice, 1 mirror? (GUESS).
    pub tilingmodehorz: u32,
    pub tilingmodevert: u32,
}

impl Image {
    /// The UV matrix rebuilt from the parameters (VERIFIED: reproduces all 14,808 stored matrices):
    /// `(x, y) = Scale(su, sv) · Rot(rot) · (u + pu, v + pv) + (pivotu, pivotv)`, `u' = x`, `v' = 1 − y`.
    pub fn uv_matrix_from_params(&self) -> [f32; 16] {
        uv_matrix(
            self.rotationuv,
            [self.positionu, self.positionv],
            [self.scaleu, self.scalev],
            [self.pivotu, self.pivotv],
        )
    }
}

/// See [`Image::uv_matrix_from_params`]; use this when the UV parameters are animated.
pub fn uv_matrix(rot_deg: f32, pos: [f32; 2], scale: [f32; 2], pivot: [f32; 2]) -> [f32; 16] {
    let (s, c) = (rot_deg as f64).to_radians().sin_cos();
    let f = |u: f64, v: f64| {
        let (x, y) = (u + pos[0] as f64, v + pos[1] as f64);
        let (x, y) = (c * x - s * y, s * x + c * y);
        (scale[0] as f64 * x + pivot[0] as f64, 1.0 - (scale[1] as f64 * y + pivot[1] as f64))
    };
    let (o, ex, ey) = (f(0.0, 0.0), f(1.0, 0.0), f(0.0, 1.0));
    let m = [
        ex.0 - o.0, ex.1 - o.1, 0.0, 0.0,
        ey.0 - o.0, ey.1 - o.1, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        o.0, o.1, 0.0, 1.0,
    ];
    m.map(|x| x as f32)
}

/// 32-byte vertex (stride VERIFIED on all 16,646 meshes). UV v = 0 at the bottom (−y).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

/// One mesh of the blob. Index group *k* is drawn with the material whose `submesh == k`.
/// The standard Anark "Rectangle" is ±50 units with front and back faces (draw one side).
#[derive(Debug, Clone, PartialEq)]
pub struct Mesh {
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    /// The 96-byte descriptor as words (n_verts, n_groups, Xenos vertex fetch constant, …).
    pub desc: [u32; 24],
    pub vertices: Vec<Vertex>,
    /// u16 triangle-list indices per index group.
    pub groups: Vec<Vec<u16>>,
    /// Byte offset / size of the vertex buffer inside the blob data.
    pub vertex_offset: usize,
    pub vertex_bytes: usize,
}

impl Fbf {
    pub fn parse(d: &[u8]) -> Result<Self> {
        let mut r = Reader::new(d, 0);
        let version = r.u32("fbf version")?;
        let count = r.u32("fbf count")?;
        need((1006..=1008).contains(&version), || format!("fbf version {version}"))?;
        let mut records = Vec::new();
        loop {
            let rec = record(&mut r, version)?;
            let is_scene = rec.type_code == 0 && rec.id == 1;
            records.push(rec);
            if is_scene {
                break;
            }
        }
        let records_end = r.o;
        let meshes = mesh_blob(&mut r)?;
        Ok(Self { version, count, records, meshes, records_end })
    }

    pub fn record(&self, id: u32) -> Option<&Record> {
        self.records.iter().find(|r| r.id == id)
    }

    /// All image records (texture references).
    pub fn images(&self) -> impl Iterator<Item = (&Record, &Image)> {
        self.records.iter().filter_map(|r| match &r.payload {
            Payload::Image(i) => Some((r, i)),
            _ => None,
        })
    }
}

fn record(r: &mut Reader, version: u32) -> Result<Record> {
    let at = r.o;
    let type_code = r.u32("fbf record")?;
    let id = r.u32("fbf record")?;
    let name = r.lstr("fbf record name")?;
    let payload = match type_code {
        0 => Payload::Group,
        1 => Payload::Text(text(r, version)?),
        2 => Payload::Camera { matrix: r.f32s("fbf camera")? },
        3 => Payload::Layer { words: r.u32s("fbf layer")?, floats: r.f32s("fbf layer")? },
        4 => Payload::Model { mesh: r.u32("fbf model")?, f1: r.f32("fbf model")? },
        5 => Payload::Material(material(r)?),
        6 => Payload::Light { light_type: r.u32("fbf light")?, raw: r.f32s("fbf light")? },
        7 => Payload::Image(image(r)?),
        11 => Payload::Custom { value: r.u32("fbf custom")? },
        t => return Err(Error::Format(format!("fbf unknown record type {t} at 0x{at:x}"))),
    };
    Ok(Record { type_code, id, name, payload })
}

fn text(r: &mut Reader, version: u32) -> Result<Text> {
    const W: &str = "fbf text";
    let u0 = r.u32s(W)?;
    let loc_key = r.lstr(W)?;
    let [boxheight, boxwidth] = r.f32s(W)?;
    let font = r.lstr(W)?;
    let [size, horzalign, vertalign, renderstyle] = r.u32s(W)?;
    let leading = r.i32(W)?;
    let tracking = r.f32(W)?;
    let u1 = r.u32s(W)?;
    let wordwrap = r.u8(W)?;
    let text_width = if version >= 1008 { Some(r.f32(W)?) } else { None };
    let has_extra = if version >= 1007 { Some(r.u8(W)?) } else { None };
    let extra = match has_extra {
        Some(h) if h != 0 => Some(r.bytes(59, W)?.to_vec()),
        _ => None,
    };
    Ok(Text {
        u0, loc_key, boxheight, boxwidth, font, size, horzalign, vertalign, renderstyle,
        leading, tracking, u1, wordwrap, text_width, has_extra, extra,
    })
}

fn material(r: &mut Reader) -> Result<Material> {
    let f: [f32; 17] = r.f32s("fbf material")?;
    let u: [u32; 9] = r.u32s("fbf material")?;
    let mut blend = [0; 6];
    blend.copy_from_slice(&u[3..9]);
    Ok(Material {
        diffuse: [f[0], f[1], f[2], f[3]],
        specular: [f[4], f[5], f[6], f[7]],
        ambient: [f[8], f[9], f[10], f[11]],
        emissive: [f[12], f[13], f[14]],
        f15: f[15],
        specular_power: f[16],
        model: u[0],
        submesh: u[1],
        shade: u[2],
        blend,
    })
}

fn image(r: &mut Reader) -> Result<Image> {
    const W: &str = "fbf image";
    let material = r.u32(W)?;
    let path = r.lstr(W)?;
    let uv_matrix = r.f32s(W)?;
    let [rotationuv, positionu, positionv, scaleu, scalev, pivotu, pivotv, f23] = r.f32s(W)?;
    let [slot_mode, u25, u26, tilingmodehorz, tilingmodevert] = r.u32s(W)?;
    Ok(Image {
        material, path, uv_matrix, rotationuv, positionu, positionv, scaleu, scalev, pivotu, pivotv,
        f23, slot_mode, u25, u26, tilingmodehorz, tilingmodevert,
    })
}

/// Group table entry (36 bytes).
struct Group {
    n_indices: u32,
    index_offset: u32,
    index_bytes: u32,
}

fn mesh_blob(r: &mut Reader) -> Result<Vec<Mesh>> {
    const W: &str = "fbf mesh blob";
    let [n_mesh, gt_bytes, data_bytes] = r.u32s(W)?;
    need(gt_bytes % 36 == 0, || "fbf group table size".into())?;
    let mut descs = Vec::new();
    for _ in 0..n_mesh {
        descs.push(r.u32s::<24>(W)?);
    }
    let mut groups = Vec::new();
    for _ in 0..gt_bytes / 36 {
        let e: [u32; 9] = r.u32s(W)?;
        groups.push(Group { n_indices: e[0], index_offset: e[7], index_bytes: e[8] });
    }
    let blob = r.bytes(data_bytes as usize, W)?;
    need(r.remaining() == 0, || "fbf blob size mismatch".into())?;

    let mut spans = Vec::new(); // (offset, size) of every buffer, for the coverage check
    let mut meshes = Vec::new();
    for desc in descs {
        let n_verts = desc[0] as usize;
        let vertex_offset = (desc[18] & !3) as usize;
        let vertex_bytes = ((desc[19] >> 2) & 0xFF_FFFF) as usize * 4;
        let stride = vertex_bytes.checked_div(n_verts).unwrap_or(0);
        need(stride == 32 && vertex_bytes == 32 * n_verts, || format!("fbf vertex stride {stride}"))?;
        let vb = slice(blob, vertex_offset, vertex_bytes)?;
        let vertices = vb.as_chunks::<32>().0.iter().map(vertex).collect();
        spans.push((vertex_offset, vertex_bytes));

        let g0 = desc[20] as usize / 36;
        let mut subs = Vec::new();
        for gi in g0..g0 + desc[1] as usize {
            let g = groups.get(gi).ok_or_else(|| Error::Format("fbf group index".into()))?;
            need(g.index_bytes == 2 * g.n_indices, || "fbf index size".into())?;
            let ib = slice(blob, g.index_offset as usize, g.index_bytes as usize)?;
            let idx: Vec<u16> = ib.as_chunks::<2>().0.iter().map(|&c| u16::from_be_bytes(c)).collect();
            need(idx.iter().all(|&i| (i as usize) < n_verts), || "fbf index out of range".into())?;
            spans.push((g.index_offset as usize, g.index_bytes as usize));
            subs.push(idx);
        }
        let f = |i: usize| f32::from_bits(desc[i]);
        meshes.push(Mesh {
            bbox_min: [f(4), f(5), f(6)],
            bbox_max: [f(8), f(9), f(10)],
            desc,
            vertices,
            groups: subs,
            vertex_offset,
            vertex_bytes,
        });
    }
    // Every buffer starts 4-byte aligned: gaps < 4 bytes, and the tail after the last is zero.
    spans.sort_unstable();
    let mut pos = 0usize;
    for (off, size) in spans {
        need(off >= pos && off - pos < 4, || format!("fbf blob gap at {pos}"))?;
        pos = off + size;
    }
    let tail = blob.get(pos..).unwrap_or_default();
    need(pos <= blob.len() && tail.len() < 4 && tail.iter().all(|&b| b == 0), || "fbf blob tail".into())?;
    Ok(meshes)
}

fn slice(d: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    off.checked_add(len)
        .and_then(|e| d.get(off..e))
        .ok_or(Error::Truncated { what: "fbf buffer", at: off })
}

fn vertex(c: &[u8; 32]) -> Vertex {
    let f = |i: usize| f32::from_be_bytes([c[4 * i], c[4 * i + 1], c[4 * i + 2], c[4 * i + 3]]);
    Vertex { pos: [f(0), f(1), f(2)], normal: [f(3), f(4), f(5)], uv: [f(6), f(7)] }
}
