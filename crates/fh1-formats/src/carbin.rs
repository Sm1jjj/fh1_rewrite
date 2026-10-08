//! `.carbin` car models (FH1 = TypeId 5 bodies, TypeId 3 rims, TypeId 1 calipers/rotors; FM4 bodies = TypeId 2),
//! big-endian.
//!
//! Layout follows the community reverse-engineering in carbin-garage's `FH1_CARBIN_MASTER.md`
//! (GPL docs used as a format reference only; this is an independent implementation).
//!
//! A file is a header, `partCount`, then `partCount` sections. Each section has an optional
//! LOD vertex pool (main carbin), subsections (index buffers + UV transforms, each tagged with a
//! LOD level) and a LOD0 vertex pool. Vertices are 28 bytes:
//! `i16 x,y,z,w | u16 uv0 | u16 uv1 | i16 quat[4] | 4 opaque`.
//! Position: `(x, y, z)` is a unit vector (|xyz| = 32767 on every vertex) scaled by `w`. Each
//! pool's decoded points are then remapped from their own bounding box onto the section's
//! target box, and the section offset is added (as Forza Studio's `CalculateBoundTargetValue`).

use crate::Error;

#[derive(Debug, Clone)]
pub struct Carbin {
    pub type_id: u32,
    pub sections: Vec<Section>,
    /// shCompressionFactors (offset, scale) for the vertex SH0 stream (see [`sh_compression`]).
    pub sh_compression: Option<[f32; 2]>,
    /// False when only part of the section table parsed (the best candidate is returned then).
    pub complete: bool,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    /// Section origin; vertex positions (already mapped into the bounds) are relative to it.
    pub offset: [f32; 3],
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub lod_vertices: Vec<Vertex>,
    pub lod0_vertices: Vec<Vertex>,
    pub subsections: Vec<Subsection>,
    /// The pools exactly as stored (big-endian), for renderers that run the game's car vertex shader.
    pub lod_raw: RawPool,
    pub lod0_raw: RawPool,
}

/// A vertex pool's bytes as stored, plus what decoding them needs.
#[derive(Debug, Clone, Default)]
pub struct RawPool {
    /// 28 or 32 (0 when the pool is empty).
    pub stride: usize,
    pub data: Vec<u8>,
    /// Bounding box of the decoded points (unit xyz * w, before the remap onto the section bounds):
    /// position = bounds_min + (p - pool_min) / (pool_max - pool_min) * (bounds_max - bounds_min).
    pub pool_min: [f32; 3],
    pub pool_max: [f32; 3],
    /// This pool's extra per-vertex stream from the section tail, empty when absent; meaning not decoded
    /// (LOD pool: 16 bytes per vertex on many rims; LOD0 pool: 4 bytes per vertex).
    pub extra: Vec<u8>,
    pub extra_stride: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Vertex {
    pub position: [f32; 3],
    pub uv0: [f32; 2],
    pub uv1: [f32; 2],
    pub normal: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct Subsection {
    pub name: String,
    /// 0 = highest detail (indexes `lod0_vertices`), 1.. = indexes `lod_vertices`.
    pub lod: i32,
    /// UV transform: (x_offset, x_scale, y_offset, y_scale) for UV0 then UV1.
    pub uv_transform: [f32; 8],
    /// Triangle list (strips are expanded on load).
    pub indices: Vec<u32>,
}

impl Section {
    pub fn vertices_for(&self, sub: &Subsection) -> &[Vertex] {
        if sub.lod == 0 { &self.lod0_vertices } else { &self.lod_vertices }
    }
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn need(&self, n: usize) -> Result<(), Error> {
        if self.p + n > self.d.len() { Err(Error::Truncated("carbin")) } else { Ok(()) }
    }
    fn skip(&mut self, n: usize) -> Result<(), Error> {
        self.need(n)?;
        self.p += n;
        Ok(())
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        self.need(n)?;
        let b = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, Error> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn f32s<const N: usize>(&mut self) -> Result<[f32; N], Error> {
        let mut out = [0.0; N];
        for v in &mut out {
            *v = self.f32()?;
        }
        Ok(out)
    }
}

fn be_u32(d: &[u8], p: usize) -> Option<u32> {
    d.get(p..p + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
}

/// An empty section's bounds: min = +FLT_MAX, max = -FLT_MAX on every axis (FIA_595_68's first
/// section `roof1`, which has no geometry).
fn empty_bounds(d: &[u8], p: usize) -> bool {
    (0..3).all(|k| be_u32(d, p + 16 + k * 4) == Some(0x7F7F_FFFF) && be_u32(d, p + 28 + k * 4) == Some(0xFF7F_FFFF))
}

/// `[marker][9 finite, modest floats]` — how a section starts (or an empty section's sentinel bounds).
fn looks_like_section(d: &[u8], p: usize) -> bool {
    match be_u32(d, p) {
        Some(2 | 5) => {}
        _ => return false,
    }
    let modest = |i: usize| be_u32(d, p + 4 + i * 4).map(f32::from_bits).is_some_and(|f| f.is_finite() && f.abs() < 100.0);
    (0..9).all(modest) || ((0..3).all(modest) && empty_bounds(d, p))
}

/// A real section's bounding box (floats 3-5 min, 6-8 max) is non-empty: max >= min on every axis and
/// > on one. Rejects all-zero false tables, e.g. at 0x3A0 in the CHE_CorvetteZR1_09 and MAZ_Miata_94 rims,
/// which otherwise parse "completely" before the real table (0x444) is reached. The empty-section sentinel
/// counts as bounds too.
fn has_bounds(d: &[u8], p: usize) -> bool {
    let f = |k: usize| be_u32(d, p + 4 + k * 4).map(f32::from_bits).unwrap_or(0.0);
    empty_bounds(d, p) || ((0..3).all(|k| f(6 + k) >= f(3 + k)) && (0..3).any(|k| f(6 + k) > f(3 + k)))
}

pub fn parse(data: &[u8]) -> Result<Carbin, Error> {
    let type_id = be_u32(data, 0).ok_or(Error::Truncated("carbin"))?;
    // Brake header word: 0x11 in FH1, 0x10 in FM4.
    let is_brake = type_id == 1 && matches!(be_u32(data, 4), Some(0x10 | 0x11));
    // 3 = rims (`media/wheels/<rim>.zip`): same section layout, table right after the header.
    // 2 = FM4 bodies (main, _lod0, _cockpit): the same sections; the header carries non-zero words where
    // FH1's is blank, and the section-table scan below finds the table (docs/FM4_RECON.md).
    if type_id != 5 && type_id != 2 && type_id != 3 && !is_brake {
        return Err(Error::Unsupported(format!("carbin type {type_id}")));
    }
    // partCount sits right before the first section. Its offset depends on the variant, so
    // try every candidate and keep the one that parses the most sections.
    let start = if type_id == 5 && data.get(4..24).is_some_and(|w| w.iter().any(|&b| b != 0)) {
        0x4DC
    } else {
        0x8
    };
    let mut best: Option<Carbin> = None;
    let mut i = start;
    while i + 48 < data.len() {
        let pc = be_u32(data, i).unwrap();
        if (1..=255).contains(&pc) && looks_like_section(data, i + 4) && has_bounds(data, i + 4) {
            let mut r = Reader { d: data, p: i + 4 };
            let mut sections = Vec::new();
            for _ in 0..pc {
                let at = r.p;
                match parse_section(&mut r) {
                    Ok(s) => sections.push(s),
                    Err(e) => {
                        if std::env::var_os("CARBIN_DEBUG").is_some() {
                            eprintln!("partCount {pc} @ {i:#x}: section {} @ {at:#x} failed at {:#x}: {e}", sections.len(), r.p);
                        }
                        break;
                    }
                }
            }
            let complete = sections.len() as u32 == pc;
            if complete {
                return Ok(Carbin { type_id, sections, sh_compression: sh_compression(data), complete: true });
            }
            if best.as_ref().is_none_or(|b| sections.len() > b.sections.len()) {
                best = Some(Carbin { type_id, sections, sh_compression: None, complete: false });
            }
        }
        i += 4;
        // The table is near the header; scanning further only finds false positives.
        if i > start + 0x2000 {
            break;
        }
    }
    best.filter(|b| !b.sections.is_empty())
        .map(|b| Carbin { sh_compression: sh_compression(data), ..b })
        .ok_or(Error::BadMagic("carbin: no section table found"))
}

/// The header's shCompressionFactors (offset, scale): the car VS decodes each SH0 byte as
/// `(b * 0.5 + 0.5) * scale + offset` (default.xex 0x82daa7fc reads them from the model header).
/// Located as the two floats after a `1 0 0 1` float row, offset < 0 < scale, followed by a u32 0/1
/// (INFERRED locator; exactly one hit in each of 328 sampled body, _lod0, cockpit, brake and rim
/// carbins, at 0x490 in _lod0/cockpit, 0x33C brakes, 0x380 rims, varying in main carbins).
pub fn sh_compression(data: &[u8]) -> Option<[f32; 2]> {
    const ROW: [u8; 16] = [0x3f, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x3f, 0x80, 0, 0];
    let f = |p: usize| be_u32(data, p).map(f32::from_bits);
    (0..data.len().min(0x3000).saturating_sub(28)).step_by(4).find_map(|i| {
        if data[i..i + 16] != ROW {
            return None;
        }
        let (a, b) = (f(i + 16)?, f(i + 20)?);
        (a < 0.0 && b > 0.0 && b < 2.0 && matches!(be_u32(data, i + 24), Some(0 | 1))).then_some([a, b])
    })
}

fn parse_section(r: &mut Reader) -> Result<Section, Error> {
    let marker = r.u32()?;
    if marker != 5 && marker != 2 {
        return Err(Error::BadMagic("carbin section marker"));
    }
    let [ox, oy, oz, minx, miny, minz, maxx, maxy, maxz] = r.f32s::<9>()?;
    r.skip(28)?;
    let perm_count = r.u32()? as usize;
    if perm_count > 1_000_000 {
        return Err(Error::BadMagic("carbin permCount"));
    }
    r.skip(perm_count * 16)?;
    r.skip(4)?;
    let cnt2 = r.u32()? as usize;
    // TOY_Prius_11 body: 162,447.
    if cnt2 > 1_000_000 {
        return Err(Error::BadMagic("carbin cnt2"));
    }
    r.skip(cnt2 * 2)?;
    r.skip(12)?;
    let name_len = r.u8()? as usize;
    let name = String::from_utf8_lossy(r.bytes(name_len)?).into_owned();
    let version = r.u32()?;

    let lod_count = r.u32()? as usize;
    let lod_size = r.u32()? as usize;
    if lod_count > 0 && lod_size != 28 && lod_size != 32 {
        return Err(Error::BadMagic("carbin LOD vertex size"));
    }
    if version >= 3 {
        bone_block(r)?;
    }
    let lod_start = r.p;
    let lod_vertices = read_pool(r, lod_count, lod_size)?;
    let mut lod_raw = RawPool { stride: lod_size, data: r.d[lod_start..r.p].to_vec(), ..Default::default() };

    r.skip(4)?;
    let sub_count = r.u32()? as usize;
    if sub_count > 1000 {
        return Err(Error::BadMagic("carbin subpart count"));
    }
    let mut subsections = Vec::with_capacity(sub_count);
    for _ in 0..sub_count {
        subsections.push(parse_subsection(r, version)?);
    }

    r.skip(4)?;
    let lod0_count = r.u32()? as i32;
    let lod0_size = r.u32()? as usize;
    let lod0_count = lod0_count.max(0) as usize;
    if lod0_count > 0 && lod0_size != 28 && lod0_size != 32 {
        return Err(Error::BadMagic("carbin LOD0 vertex size"));
    }
    // Present even when the pool is empty (main carbins), like the LOD pool's.
    if version >= 3 {
        bone_block(r)?;
    }
    let lod0_start = r.p;
    let lod0_vertices = read_pool(r, lod0_count, lod0_size)?;
    let mut lod0_raw = RawPool { stride: lod0_size, data: r.d[lod0_start..r.p].to_vec(), ..Default::default() };
    (lod_raw.pool_min, lod_raw.pool_max) = bbox(&lod_vertices);
    (lod0_raw.pool_min, lod0_raw.pool_max) = bbox(&lod0_vertices);
    let target = ([minx, miny, minz], [maxx, maxy, maxz]);
    let (lod_vertices, lod0_vertices) = (remap(lod_vertices, target), remap(lod0_vertices, target));

    let [lod_extra, lod0_extra] = skip_tail(r)?;
    if let Some((stream, stride)) = lod_extra {
        (lod_raw.extra, lod_raw.extra_stride) = (stream, stride);
    }
    if let Some((stream, stride)) = lod0_extra {
        (lod0_raw.extra, lod0_raw.extra_stride) = (stream, stride);
    }

    for s in &subsections {
        let n = if s.lod == 0 { lod0_vertices.len() } else { lod_vertices.len() };
        if let Some(&max) = s.indices.iter().max().filter(|&&m| m as usize >= n) {
            return Err(Error::Unsupported(format!(
                "carbin index out of range: section {name} sub {} lod {} max index {max}, pools lod={} lod0={}",
                s.name, s.lod, lod_vertices.len(), lod0_vertices.len()
            )));
        }
    }

    Ok(Section {
        name,
        offset: [ox, oy, oz],
        bounds_min: [minx, miny, minz],
        bounds_max: [maxx, maxy, maxz],
        lod_vertices,
        lod0_vertices,
        subsections,
        lod_raw,
        lod0_raw,
    })
}

fn bbox(v: &[Vertex]) -> ([f32; 3], [f32; 3]) {
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for x in v {
        for k in 0..3 {
            lo[k] = lo[k].min(x.position[k]);
            hi[k] = hi[k].max(x.position[k]);
        }
    }
    if v.is_empty() { ([0.0; 3], [0.0; 3]) } else { (lo, hi) }
}

/// Linearly remap a pool's decoded positions from their own bounding box onto the section's
/// target box (Forza Studio's `CalculateBoundTargetValue`).
fn remap(mut v: Vec<Vertex>, (tmin, tmax): ([f32; 3], [f32; 3])) -> Vec<Vertex> {
    if (0..3).any(|k| tmin[k] > tmax[k]) {
        return v; // empty-section sentinel bounds: nothing to map onto
    }
    let (lo, hi) = bbox(&v);
    for x in &mut v {
        for k in 0..3 {
            let span = hi[k] - lo[k];
            let t = if span > 0.0 { (x.position[k] - lo[k]) / span } else { 0.5 };
            x.position[k] = tmin[k] + t * (tmax[k] - tmin[k]);
        }
    }
    v
}

/// `m_NumBoneWeights` then, when non-zero, a per-section id.
fn bone_block(r: &mut Reader) -> Result<(), Error> {
    if r.u32()? != 0 {
        r.skip(4)?;
    }
    Ok(())
}

fn read_pool(r: &mut Reader, count: usize, size: usize) -> Result<Vec<Vertex>, Error> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let raw = r.bytes(count * size)?;
    Ok(raw.chunks_exact(size).map(decode_vertex).collect())
}

fn decode_vertex(v: &[u8]) -> Vertex {
    let i16_at = |o: usize| i16::from_be_bytes([v[o], v[o + 1]]) as f32 / 32767.0;
    let u16_at = |o: usize| u16::from_be_bytes([v[o], v[o + 1]]) as f32 / 65535.0;
    let w = i16_at(6);
    let q = [i16_at(16), i16_at(18), i16_at(20), i16_at(22)];
    Vertex {
        // Unit direction times length; remapped onto the section bounds in `parse_section`.
        position: [i16_at(0) * w, i16_at(2) * w, i16_at(4) * w],
        uv0: [u16_at(8), u16_at(10)],
        uv1: [u16_at(12), u16_at(14)],
        normal: quat_normal(q),
    }
}

/// First row of the rotation matrix of the (x, y, z, w) quaternion.
fn quat_normal([x, y, z, w]: [f32; 4]) -> [f32; 3] {
    let len = (x * x + y * y + z * z + w * w).sqrt();
    if len < 1e-6 {
        return [0.0, 1.0, 0.0];
    }
    let (x, y, z, w) = (x / len, y / len, z / len, w / len);
    [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y + w * z), 2.0 * (x * z - w * y)]
}

fn parse_subsection(r: &mut Reader, version: u32) -> Result<Subsection, Error> {
    r.skip(5)?;
    let uv_transform = r.f32s::<8>()?;
    r.skip(36)?;
    let name_len = r.u32()? as usize;
    if name_len > 256 {
        return Err(Error::BadMagic("carbin subsection name"));
    }
    let name = String::from_utf8_lossy(r.bytes(name_len)?).into_owned();
    let lod = r.u32()? as i32;
    let index_type = r.u32()?;
    if index_type != 4 && index_type != 6 {
        return Err(Error::BadMagic("carbin index type"));
    }
    r.skip(24 + 32)?;
    // A subsection version word (3 in FM4, 4 in FH1), plus a zero word in FH1.
    r.skip(if version >= 3 { 8 } else { 4 })?;
    let count = r.u32()? as usize;
    let size = r.u32()? as usize;
    if size != 2 && size != 4 {
        return Err(Error::BadMagic("carbin index size"));
    }
    let raw = r.bytes(count * size)?;
    let idx: Vec<u32> = raw
        .chunks_exact(size)
        .map(|c| if size == 2 { u16::from_be_bytes([c[0], c[1]]) as u32 } else { u32::from_be_bytes(c.try_into().unwrap()) })
        .collect();
    r.skip(4)?;
    let indices = if index_type == 4 { idx } else { strip_to_list(&idx, size) };
    Ok(Subsection { name, lod, uv_transform, indices })
}

/// Strip restart: 0xFFFF for 16-bit indices; 32-bit strips use 0x00FFFFFF (the GPU's 24-bit
/// index limit) and sometimes 0xFFFFFFFF.
fn strip_to_list(strip: &[u32], index_size: usize) -> Vec<u32> {
    let is_restart = |i: u32| if index_size == 2 { i == 0xFFFF } else { i >= 0x00FF_FFFF };
    let mut out = Vec::new();
    for run in strip.split(|&i| is_restart(i)) {
        for (k, w) in run.windows(3).enumerate() {
            let (a, b, c) = if k % 2 == 0 { (w[0], w[1], w[2]) } else { (w[1], w[0], w[2]) };
            if a != b && b != c && a != c {
                out.extend([a, b, c]);
            }
        }
    }
    out
}

/// Section tail: `[u32 1][u8 flag]` then two per-vertex streams, each `[u32 version][u32 count]
/// [u32 stride][bone block][count * stride]`: the first belongs to the LOD pool (4 bytes per vertex on
/// bodies, 16 on many rims), the second to the LOD0 pool (4 bytes per vertex in `_lod0` / cockpit). A few trailing bytes follow; snap to wherever the next section (or end of file) starts.
/// Returns the two streams (data, stride), `None` when empty.
fn skip_tail(r: &mut Reader) -> Result<[Option<(Vec<u8>, usize)>; 2], Error> {
    r.skip(5)?;
    let mut streams = [None, None];
    for s in &mut streams {
        let version = r.u32()?;
        let count = r.u32()? as usize;
        let stride = r.u32()? as usize;
        if version >= 3 {
            bone_block(r)?;
        }
        let data = r.bytes(count.checked_mul(stride).ok_or(Error::BadMagic("carbin tail"))?)?;
        if count > 0 && stride > 0 {
            *s = Some((data.to_vec(), stride));
        }
    }
    let base = r.p;
    for t in [0, 4, 8, 12] {
        if looks_like_section(r.d, base + t) {
            r.p = base + t;
            return Ok(streams);
        }
    }
    // Last section: whatever remains is footer.
    r.p = (base + 4).min(r.d.len());
    Ok(streams)
}
