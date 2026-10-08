//! Track PVS root (`Ribbon_00/<Track>_00.pvs`, magic `FPVS`, version 0x32 (FH2: 0x33, same layout), big-endian, byte-packed:
//! fields are not aligned). Only the leading sections are decoded; they hold everything needed to bind
//! textures to the `.rmb.bin` models:
//!
//! - **textures**: 28-byte records `{u32 file_id, u32 index, f32 1.0, f32 1.0, u32 0, u32 0, u32 flags}`.
//!   `file_id` names `bin/_0x%08X.bix` (flags `0x04` set) or the CAFF `_0x%08X.bin` (flags bit 0 and
//!   bit 2 clear). Flags bit 0 set = no file on the disc (supplied at runtime). Colorado: 19,613 records,
//!   of which 5,234 `.bix` and 2,965 CAFF, matching `FilenameMap_00.dat`.
//! - **shaders**: `u32 len` + chars (no NUL), e.g. `shaders\track\h_diff_emissive_2`.
//! - an 18-byte record table (purpose not yet known, 62,173 in Colorado), skipped.
//! - **models**, one per model number N (`<track>out.%05d.rmb.bin`; N = 0 is unused): `u32 n` texture
//!   indices, `u32 m` shader indices, then 60 bytes (two u32 + 13 f32, transform/bounds, not decoded).
//!   A model's material texture slot `s` (from the `.rmb.bin`) is `textures[model.textures[s]]`, and its
//!   local shader `k` is `shaders[model.shaders[k]]`.
//!
//! Verified against the user's disc: on UIAutoshow the shader lists match the models' own shader names,
//! and the max material slot + 1 equals the list length (Colorado check: `examples/pvs.rs`).

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Texture {
    pub file_id: u32,
    pub flags: u32,
}

impl Texture {
    /// File name inside the track's `bin.zip`, or None when the texture isn't stored on the disc.
    pub fn file_name(&self) -> Option<String> {
        if self.flags & 1 != 0 {
            None
        } else if self.flags & 4 != 0 {
            Some(format!("_0x{:08X}.bix", self.file_id))
        } else {
            Some(format!("_0x{:08X}.bin", self.file_id))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ModelBinding {
    /// Indices into [`Pvs::textures`], addressed by the model's material texture slots.
    pub textures: Vec<u32>,
    /// Indices into [`Pvs::shaders`], addressed by the model's local shader index.
    pub shaders: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Pvs {
    pub textures: Vec<Texture>,
    pub shaders: Vec<String>,
    /// Indexed by model number.
    pub models: Vec<ModelBinding>,
}

impl Pvs {
    /// The texture bound to material texture slot `slot` of model `model` (None for unused slots).
    pub fn texture(&self, model: usize, slot: i32) -> Option<&Texture> {
        let i = *self.models.get(model)?.textures.get(usize::try_from(slot).ok()?)?;
        self.textures.get(i as usize)
    }
}

struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl R<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], Error> {
        let b = self.d.get(self.p..self.p + n).ok_or(Error::Truncated("pvs"))?;
        self.p += n;
        Ok(b)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn count(&mut self, elem: usize) -> Result<usize, Error> {
        let n = self.u32()? as usize;
        if n.saturating_mul(elem) > self.d.len() - self.p {
            return Err(Error::Truncated("pvs count"));
        }
        Ok(n)
    }
    fn u32_list(&mut self) -> Result<Vec<u32>, Error> {
        let n = self.count(4)?;
        (0..n).map(|_| self.u32()).collect()
    }
}

pub fn parse(d: &[u8]) -> Result<Pvs, Error> {
    let mut r = R { d, p: 0 };
    if r.bytes(4)? != b"FPVS" {
        return Err(Error::BadMagic("FPVS"));
    }
    // 0x32 = FH1; 0x33 = Forza Horizon 2 (360), same layout to the end of the model table (docs/FH2_RECON.md).
    // 0x1B = Forza Motorsport 4: different sections, see [`parse_fm4`].
    match r.u32()? {
        0x32 | 0x33 => {}
        FM4_VERSION => return parse_fm4(d),
        _ => return Err(Error::Unsupported("pvs version".into())),
    }
    // hash, 0, 0, u16 + 2 x u8, 2 x u32, then a u32 list (Colorado: 33 increasing offsets).
    r.bytes(4 + 4 + 4 + 4 + 8)?;
    r.u32_list()?;
    // 7 bytes, `00 00 32 00 00 00 00` on every track checked (meaning unknown).
    r.bytes(7)?;

    let n = r.count(28)?;
    let mut textures = Vec::with_capacity(n);
    for i in 0..n {
        let file_id = r.u32()?;
        if r.u32()? as usize != i {
            return Err(Error::Unsupported(format!("pvs texture {i}: index mismatch")));
        }
        r.bytes(16)?;
        textures.push(Texture { file_id, flags: r.u32()? });
    }

    let n = r.count(4)?;
    let mut shaders = Vec::with_capacity(n);
    for _ in 0..n {
        let len = r.count(1)?;
        shaders.push(String::from_utf8_lossy(r.bytes(len)?).into_owned());
    }

    let n = r.count(18)?;
    r.bytes(n * 18)?;

    let n = r.count(68)?;
    let mut models = Vec::with_capacity(n);
    for _ in 0..n {
        let textures = r.u32_list()?;
        let shaders = r.u32_list()?;
        r.bytes(60)?;
        models.push(ModelBinding { textures, shaders });
    }
    Ok(Pvs { textures, shaders, models })
}

/// Forza Motorsport 4's PVS version.
pub const FM4_VERSION: u32 = 0x1B;

/// Forza Motorsport 4 (version 0x1B, docs/FM4_RECON.md). Only the binding tables are read; the sections around them
/// (zone/visibility data, 72-byte atlas-tile records) are not decoded, so the tables are located by validation:
/// - **textures**: `u32 n` + the FH1 28-byte records, except that `+4` is the texture object index (a lightmap atlas
///   object's tiles, flags 9, repeat it with their uv scale / offset in the floats), so `+4 <= position`.
/// - **shaders**: right after, as in FH1.
/// - **models**: `u32 n` (= highest `<track>out.NNNNN` + 1), then per model `u32 n` texture indices, `u32 m` shader
///   indices and 80 bytes (a 4x4 matrix with `[15] = 1.0`, then 4 floats). Located as the first offset after the
///   shaders where the whole table validates (every index in range, every matrix ending in 1.0); a one-model table
///   is only taken when no larger one validates (false hits).
/// Verified on all 77 FM4 `.pvs` (the model count equals the highest model number + 1 on every track).
fn parse_fm4(d: &[u8]) -> Result<Pvs, Error> {
    let be = |o: usize| d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    const ONE: u32 = 0x3F80_0000;
    // Textures.
    let start = (12..d.len().saturating_sub(64))
        .find(|&q| {
            let n = be(q - 4).unwrap_or(0) as usize;
            be(q + 4) == Some(0)
                && be(q + 8) == Some(ONE)
                && be(q + 12) == Some(ONE)
                && n > 0
                && q + n * 28 <= d.len()
                && (0..n).all(|i| be(q + i * 28 + 4).is_some_and(|v| v as usize <= i))
        })
        .ok_or(Error::BadMagic("pvs (FM4): texture table not found"))?;
    let mut r = R { d, p: start - 4 };
    let n = r.count(28)?;
    let mut textures = Vec::with_capacity(n);
    for _ in 0..n {
        let file_id = r.u32()?;
        r.bytes(20)?;
        textures.push(Texture { file_id, flags: r.u32()? });
    }
    let ns = r.count(4)?;
    let mut shaders = Vec::with_capacity(ns);
    for _ in 0..ns {
        let len = r.count(1)?;
        shaders.push(String::from_utf8_lossy(r.bytes(len)?).trim_end_matches('\0').to_owned());
    }
    // Models.
    let table = |s: usize| -> Option<Vec<ModelBinding>> {
        let m = be(s)? as usize;
        if m == 0 || m > 100_000 || m * 88 > d.len() - s {
            return None;
        }
        let mut p = s + 4;
        let mut models = Vec::with_capacity(m);
        let list = |p: &mut usize, max: usize, limit: usize| -> Option<Vec<u32>> {
            let k = be(*p)? as usize;
            if k > limit {
                return None;
            }
            let l: Vec<u32> = (0..k).map(|i| be(*p + 4 + i * 4)).collect::<Option<_>>()?;
            *p += 4 + 4 * k;
            l.iter().all(|&v| (v as usize) < max).then_some(l)
        };
        for _ in 0..m {
            let textures = list(&mut p, n, 256)?;
            let shaders = list(&mut p, ns, 64)?;
            if be(p + 60)? != ONE {
                return None;
            }
            p += 80;
            models.push(ModelBinding { textures, shaders });
        }
        Some(models)
    };
    let mut single = None;
    for s in r.p..d.len().saturating_sub(4) {
        if let Some(models) = table(s) {
            if models.len() > 1 {
                return Ok(Pvs { textures, shaders, models });
            }
            single.get_or_insert(models);
        }
    }
    let models = single.ok_or(Error::BadMagic("pvs (FM4): model table not found"))?;
    Ok(Pvs { textures, shaders, models })
}
