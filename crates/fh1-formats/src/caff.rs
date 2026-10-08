//! CAFF compiled track textures (`bin/_0x########.bin`, the textures the PVS flags as non-`.bix`).
//!
//! Layout (verified on all 2,965 unique Colorado CAFF textures): `"CAFF21.11.05.0034"` container with a
//! `.data` and a `.gpu` section. The `.gpu` section is the last `gpu_size` bytes of the file (u32 at 0x88)
//! and holds the Xenos-tiled texels, base level first (same tiling as `.xds` / `.bix`). In `.data`, the
//! asset starts with `"texture\0"` + version string; 0x18 bytes after `"texture"`:
//! `u32 format word` (fetch-constant dword 1: low 6 bits GPU format, bits 6-7 endian),
//! `u32 kind` (0 = 2D, 2 = cube), `u32 0`, `u16 width, u16 height`, `u32 0`, `u32 -1`, `u8 mip count`.
//! Colorado: 2,953 DXT1 + 9 DXT5 2D textures (2,877 are 64x64 with 7 mips) and 3 DXT1 cube maps.

use crate::xds::{self, Format, Image};
use crate::Error;

#[derive(Debug, Clone, Copy)]
pub struct CaffTexture {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub endian: u32,
    pub cube: bool,
    /// The fetch-constant format word as stored (gamma flag in bits 8-13, see fh1setup textures.rs).
    pub raw_format: u32,
    pub mip_count: u32,
    /// Offset and length of the `.gpu` section (tiled texels).
    pub gpu_offset: usize,
    pub gpu_len: usize,
}

pub fn parse(d: &[u8]) -> Result<CaffTexture, Error> {
    if !d.starts_with(b"CAFF") || d.len() < 0x90 {
        return Err(Error::BadMagic("CAFF"));
    }
    let be = |o: usize| -> Result<u32, Error> {
        Ok(u32::from_be_bytes(d.get(o..o + 4).ok_or(Error::Truncated("caff"))?.try_into().unwrap()))
    };
    let gpu_len = be(0x88)? as usize;
    let gpu_offset = d.len().checked_sub(gpu_len).ok_or(Error::Truncated("caff .gpu"))?;
    let tex = d.windows(8).take(0x1000).position(|w| w == b"texture\0").ok_or(Error::BadMagic("caff: not a texture"))?;
    let o = tex + 0x18;
    let word = be(o)?;
    let dims = be(o + 12)?;
    Ok(CaffTexture {
        width: dims >> 16,
        height: dims & 0xFFFF,
        format: Format::from_raw(word & 0x3F),
        endian: (word >> 6) & 3,
        cube: be(o + 4)? == 2,
        raw_format: word,
        mip_count: be(o + 24)? >> 24,
        gpu_offset,
        gpu_len,
    })
}

/// Base level of each face: one for 2D textures, six for cube maps. Colorado's cube maps (256x256 DXT1,
/// one level) store the six faces back to back, `gpu_len / 6` bytes each.
pub fn decode_faces(d: &[u8]) -> Result<(CaffTexture, Vec<Image>), Error> {
    let t = parse(d)?;
    if !t.cube {
        return decode_base(d).map(|(t, img)| (t, vec![img]));
    }
    if t.mip_count != 1 {
        return Err(Error::Unsupported("CAFF cube map with mips".into()));
    }
    let face = t.gpu_len / 6;
    let faces = (0..6)
        .map(|i| xds::untile(&d[t.gpu_offset + i * face..], t.format, t.width, t.height, t.width, true, t.endian))
        .collect::<Result<_, _>>()?;
    Ok((t, faces))
}

/// Untile the base level of a 2D CAFF texture.
pub fn decode_base(d: &[u8]) -> Result<(CaffTexture, Image), Error> {
    let t = parse(d)?;
    if t.cube {
        return Err(Error::Unsupported("CAFF cube map".into()));
    }
    let img = xds::untile(&d[t.gpu_offset..], t.format, t.width, t.height, t.width, true, t.endian)?;
    Ok((t, img))
}
