//! Track texture preview bundles (`bin/_0x1000####.bundle`, big-endian).
//!
//! Each bundle holds the low-resolution packed mip tail (at most 16x16) of many track textures, keyed by
//! the texture's **index in the PVS texture table** (`pvs::Pvs::textures`), not its file id. The same
//! texture appears in several bundles (one per streaming area) with identical records. For the
//! textures the PVS flags as runtime-provided (flags bit 0, no `.bix` / CAFF file on the disc), the
//! bundle record is the only data the disc has: 4x4 previews that are constant black on lightmap slots
//! and white on AO slots. See docs/LIGHTMAPS.md.
//!
//! Layout (verified on all 146 distinct Colorado bundles: every record chain ends exactly at the end of
//! the file): `u32 count`, then `count` records of
//! `{u32 texture index, u32 width, u32 height, u32 mip count, u32 format word, u32 0xFFFFFFFF,
//! u32 data size}` followed by `data size` bytes. The format word is the fetch-constant dword 1 (low 6
//! bits GPU format, bits 6-7 endian), as in `.bix`. The data is one Xenos-tiled 32-block-wide tile
//! holding every level packed together (the "packed mip tail"); [`packed_mip_offset`] gives where each
//! level sits.

use crate::xds::{self, Format, Image};
use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    /// Index into the track's PVS texture table.
    pub texture_index: u32,
    pub width: u32,
    pub height: u32,
    pub mip_count: u32,
    pub format: Format,
    pub endian: u32,
    pub raw_format: u32,
    /// Byte range of the tiled data in the bundle.
    pub offset: usize,
    pub len: usize,
}

fn be(d: &[u8], o: usize) -> Result<u32, Error> {
    Ok(u32::from_be_bytes(d.get(o..o + 4).ok_or(Error::Truncated("bundle"))?.try_into().unwrap()))
}

pub fn parse(d: &[u8]) -> Result<Vec<Entry>, Error> {
    let n = be(d, 0)? as usize;
    if n.saturating_mul(28) > d.len() {
        return Err(Error::Truncated("bundle count"));
    }
    let mut p = 4;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let u = |i: usize| be(d, p + 4 * i);
        let word = u(4)?;
        let len = u(6)? as usize;
        let offset = p + 28;
        if offset + len > d.len() {
            return Err(Error::Truncated("bundle record"));
        }
        out.push(Entry {
            texture_index: u(0)?,
            width: u(1)?,
            height: u(2)?,
            mip_count: u(3)?,
            format: Format::from_raw(word & 0x3F),
            endian: (word >> 6) & 3,
            raw_format: word,
            offset,
            len,
        });
        p = offset + len;
    }
    if p != d.len() {
        return Err(Error::SizeMismatch { expected: d.len(), got: p });
    }
    Ok(out)
}

/// Texel offset of mip `level` inside a Xenos packed mip tail whose first level is `width` x `height`
/// (smaller side at most 16). Levels 0-2 sit 16, 8, 4 texels along x (along y when the tail is wider than
/// tall); levels 3+ sit `long side >> (level - 2)` texels along the long axis (y for square tails), so a
/// 16x16 tail has level 3 at y = 8 and level 4 at y = 4, and a 128x16 tail has levels 3..7 at x = 64..4.
/// Verified on the disc: the 8:1 `.bix` tails pin the level 3+ order; level 0-2 decodes match the
/// box-downscaled full-resolution textures (docs/LIGHTMAPS.md).
pub fn packed_mip_offset(width: u32, height: u32, level: u32) -> (u32, u32) {
    let log2 = |v: u32| 32 - v.max(1).saturating_sub(1).leading_zeros();
    let wide = log2(width) > log2(height);
    if level < 3 {
        let o = 16 >> level;
        if wide { (0, o) } else { (o, 0) }
    } else if wide {
        ((1 << log2(width)) >> (level - 2), 0)
    } else {
        (0, (1 << log2(height)) >> (level - 2))
    }
}

/// Untile one level to linear little-endian blocks.
pub fn decode_level(bundle: &[u8], e: &Entry, level: u32) -> Result<Image, Error> {
    if e.width.min(e.height) > 16 {
        return Err(Error::Unsupported("bundle texture larger than a packed mip tail".into()));
    }
    let data = &bundle[e.offset..e.offset + e.len];
    let (w, h) = ((e.width >> level).max(1), (e.height >> level).max(1));
    let (ox, oy) = packed_mip_offset(e.width, e.height, level);
    untile_region(data, e.format, e.endian, ox, oy, w, h, 32)
}

/// Every level, largest first.
pub fn decode_mips(bundle: &[u8], e: &Entry) -> Result<Vec<Image>, Error> {
    decode_chain(&bundle[e.offset..e.offset + e.len], e.format, e.endian, e.width, e.height, 0, e.mip_count.max(1))
}

/// Xenos mip chain: levels `first_level..mip_count` of a `width` x `height` texture, stored back to back.
/// Each level whose smaller side is above 16 texels is its own tiled surface padded to 32x32 blocks; the
/// first level at or below 16 starts the packed tail (one 32-block-wide tile holding the rest, see
/// [`packed_mip_offset`]). Level 0 is stored at its real size; levels 1.. at the power-of-two-rounded `width >> level` (non-power-of-two
/// `.xds`); the returned images have the real sizes. Bundle records are a tail on its own (`first_level`
/// 0). `.bix` files hold levels 1.. after their 28-byte header; `.xds` hold them at the fetch constant's mip
/// address (verified: decoded levels match the box-downscaled base and the bundle previews, all formats and
/// aspects; docs/LIGHTMAPS.md "Mip chains").
pub fn decode_chain(
    data: &[u8],
    format: Format,
    endian: u32,
    width: u32,
    height: u32,
    first_level: u32,
    mip_count: u32,
) -> Result<Vec<Image>, Error> {
    let levels = decode_layers(data, format, endian, width, height, first_level, mip_count, 1)?;
    Ok(levels.into_iter().map(|mut l| l.remove(0)).collect())
}

/// Like [`decode_chain`] for layered textures (cube maps: 6 layers, +X -X +Y -Y +Z -Z): level-major, each
/// layer of a level padded to whole 32x32-block tiles, the packed tail included (one full tile per layer).
/// Returns `[level][layer]`. Verified on the disc's XPR cube maps (docs/LIGHTMAPS.md, "Car textures").
#[allow(clippy::too_many_arguments)]
pub fn decode_layers(
    data: &[u8],
    format: Format,
    endian: u32,
    width: u32,
    height: u32,
    first_level: u32,
    mip_count: u32,
    layers: u32,
) -> Result<Vec<Vec<Image>>, Error> {
    let (bw, bpb) = format.block().ok_or(Error::Unsupported(format!("texture format {format:?}")))?;
    let dims = |l: u32| ((width >> l).max(1), (height >> l).max(1));
    let (sw0, sh0) = (width.next_power_of_two(), height.next_power_of_two());
    let mut out = Vec::new();
    let mut off = 0usize;
    for l in first_level..mip_count {
        let (sw, sh) = ((sw0 >> l).max(1), (sh0 >> l).max(1));
        if sw.min(sh) <= 16 {
            let tile = (32 * 32 * bpb) as usize;
            for k in l..mip_count {
                let (kw, kh) = dims(k);
                let (ox, oy) = packed_mip_offset(sw, sh, k - l);
                let level = (0..layers as usize)
                    .map(|i| {
                        let rest = data.get(off + i * tile..).ok_or(Error::Truncated("mip tail"))?;
                        untile_region(rest, format, endian, ox, oy, kw, kh, 32)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                out.push(level);
            }
            break;
        }
        let (lw, lh) = dims(l);
        // Level 0 is stored at its real size (padded to 32x32 blocks), levels 1.. at the power-of-two
        // rounded size: verified on the 768x256 CAFF textures (base 98,304 + 512x128 / 256x64 / 128x32 +
        // the used half of the tail = the 159,744-byte .gpu section exactly).
        let (sw, sh) = if l == 0 { (width, height) } else { (sw, sh) };
        let (blocks_w, blocks_h) = (sw.div_ceil(bw), sh.div_ceil(bw));
        let size = (blocks_w.next_multiple_of(32) * blocks_h.next_multiple_of(32) * bpb) as usize;
        let level = (0..layers as usize)
            .map(|i| {
                let rest = data.get(off + i * size..).ok_or(Error::Truncated("mip chain"))?;
                untile_region(rest, format, endian, 0, 0, lw, lh, blocks_w)
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.push(level);
        off += size * layers as usize;
    }
    Ok(out)
}

/// Untile a `w` x `h` texel region at texel offset (`ox`, `oy`) of a tiled surface `pitch_blocks` wide.
fn untile_region(data: &[u8], format: Format, endian: u32, ox: u32, oy: u32, w: u32, h: u32, pitch_blocks: u32) -> Result<Image, Error> {
    let (bw, bpb) = format.block().ok_or(Error::Unsupported(format!("texture format {format:?}")))?;
    let (ox, oy) = (ox / bw, oy / bw);
    let (blocks_w, blocks_h) = (w.div_ceil(bw), h.div_ceil(bw));
    let log_bpb = bpb.trailing_zeros();
    let mut out = Vec::with_capacity((blocks_w * blocks_h * bpb) as usize);
    for y in 0..blocks_h {
        for x in 0..blocks_w {
            let src = (xds::tiled_offset_2d(ox + x, oy + y, pitch_blocks, log_bpb) * bpb) as usize;
            out.extend_from_slice(data.get(src..src + bpb as usize).ok_or(Error::Truncated("tiled texels"))?);
        }
    }
    match endian {
        1 => out.chunks_exact_mut(2).for_each(|c| c.swap(0, 1)),
        2 => out.chunks_exact_mut(4).for_each(|c| c.reverse()),
        3 => out.chunks_exact_mut(4).for_each(|c| {
            c.swap(0, 2);
            c.swap(1, 3);
        }),
        _ => {}
    }
    if format == Format::Argb8 {
        out.chunks_exact_mut(4).for_each(|px| px.swap(0, 2));
    }
    Ok(Image { width: w, height: h, format, data: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_offsets() {
        // 16x16: levels 0-2 along x, 3-4 along y.
        assert_eq!(packed_mip_offset(16, 16, 0), (16, 0));
        assert_eq!(packed_mip_offset(16, 16, 2), (4, 0));
        assert_eq!(packed_mip_offset(16, 16, 3), (0, 8));
        assert_eq!(packed_mip_offset(16, 16, 4), (0, 4));
        // Wide tails: levels 0-2 along y, 3+ along x from the long side (Colorado's 512x64 .bix tails).
        assert_eq!(packed_mip_offset(16, 8, 0), (0, 16));
        assert_eq!(packed_mip_offset(128, 16, 3), (64, 0));
        assert_eq!(packed_mip_offset(128, 16, 7), (4, 0));
    }

    #[test]
    fn parse_chain() {
        // One 4x4 DXT1 record whose level 0 (block (4, 0) of the tile = byte 256) is non-zero.
        let mut d = vec![0, 0, 0, 1];
        for v in [7u32, 4, 4, 3, 0x1A20_7F52, 0xFFFF_FFFF, 1024] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        let base = d.len();
        d.resize(base + 1024, 0);
        d[base + 256..base + 264].copy_from_slice(&[0xF8, 0x1F, 0xF8, 0x1F, 0, 0, 0, 0]);
        let e = parse(&d).unwrap();
        assert_eq!((e[0].texture_index, e[0].format, e[0].endian), (7, Format::Dxt1, 1));
        let img = decode_level(&d, &e[0], 0).unwrap();
        assert_eq!(img.data, [0x1F, 0xF8, 0x1F, 0xF8, 0, 0, 0, 0]);
    }
}
