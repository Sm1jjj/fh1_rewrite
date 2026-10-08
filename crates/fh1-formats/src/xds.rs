//! `.xds` textures: an Xbox 360 `D3DBaseTexture` header (52 bytes, big-endian) followed by the
//! GPU texture data, usually tiled and byte-swapped.
//!
//! Header: Common, ReferenceCount, Fence, ReadFence, Identifier, BaseFlush, MipFlush, then
//! the 6-dword GPU texture fetch constant (bitfields as in Xenia's `xe_gpu_texture_fetch_t`).

use crate::Error;

pub const HEADER_LEN: usize = 52;

/// Xenos texture formats we know how to handle (values from Xenia's `TextureFormat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// 8:8:8:8 (stored ARGB in big-endian dwords).
    Argb8,
    Dxt1,
    Dxt3,
    Dxt5,
    /// Two-channel BC5 (normal maps).
    Dxn,
    /// Single-channel BC4.
    Dxt5A,
    /// Single-channel 4-bit explicit alpha (Xenos DXT3A, 0x3A).
    Dxt3A,
    Other(u32),
}

impl Format {
    pub fn from_raw(v: u32) -> Self {
        match v {
            6 | 50 => Format::Argb8,
            18 | 51 => Format::Dxt1,
            19 | 52 => Format::Dxt3,
            20 | 53 => Format::Dxt5,
            49 => Format::Dxn,
            58 => Format::Dxt3A,
            59 => Format::Dxt5A,
            o => Format::Other(o),
        }
    }

    /// (block edge in texels, bytes per block)
    pub fn block(self) -> Option<(u32, u32)> {
        Some(match self {
            Format::Argb8 => (1, 4),
            Format::Dxt1 | Format::Dxt5A | Format::Dxt3A => (4, 8),
            Format::Dxt3 | Format::Dxt5 | Format::Dxn => (4, 16),
            Format::Other(_) => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub common: u32,
    pub fetch: [u32; 6],
    pub format: Format,
    pub raw_format: u32,
    /// 0 none, 1 8-in-16, 2 8-in-32, 3 16-in-32.
    pub endian: u32,
    pub tiled: bool,
    /// Row pitch in texels (the fetch constant stores it in units of 32).
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    /// 0 = 1D, 1 = 2D, 2 = 3D, 3 = cube.
    pub dimension: u32,
    pub max_mip: u32,
    pub packed_mips: bool,
}

impl Header {
    pub fn parse(b: &[u8]) -> Result<Self, Error> {
        if b.len() < HEADER_LEN {
            return Err(Error::Truncated("xds header"));
        }
        let be = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let common = be(0);
        if common & 0xF != 3 {
            return Err(Error::BadMagic("xds: D3DCOMMON type is not texture"));
        }
        let f: [u32; 6] = std::array::from_fn(|i| be(28 + i * 4));
        let raw_format = f[1] & 0x3F;
        let dimension = (f[5] >> 9) & 3;
        let (width, height) = match dimension {
            0 => ((f[2] & 0xFF_FFFF) + 1, 1),
            _ => ((f[2] & 0x1FFF) + 1, ((f[2] >> 13) & 0x1FFF) + 1),
        };
        Ok(Self {
            common,
            fetch: f,
            format: Format::from_raw(raw_format),
            raw_format,
            endian: (f[1] >> 6) & 3,
            tiled: f[0] >> 31 != 0,
            pitch: ((f[0] >> 22) & 0x1FF) * 32,
            width,
            height,
            dimension,
            max_mip: (f[4] >> 6) & 0xF,
            packed_mips: (f[5] >> 11) & 1 != 0,
        })
    }
}

/// A texture's top mip level as linear, little-endian blocks (BCn) or RGBA8 texels.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// BCn: blocks in row-major order. Argb8: RGBA8 texels.
    pub data: Vec<u8>,
}

/// Untile and byte-swap the base (largest) mip level of an `.xds` file.
pub fn decode_base(file: &[u8]) -> Result<(Header, Image), Error> {
    let h = Header::parse(file)?;
    if h.dimension != 1 {
        return Err(Error::Unsupported(format!("xds dimension {}", h.dimension)));
    }
    // Small packed-mip textures keep their base level in the mip tail: at texel (16,0), or
    // (0,16) when wider than tall.
    let (ox, oy) = if h.packed_mips && h.width.min(h.height) <= 16 {
        if h.width.next_power_of_two() <= h.height.next_power_of_two() { (16, 0) } else { (0, 16) }
    } else {
        (0, 0)
    };
    let (w, ht) = (h.width + ox, h.height + oy);
    let full = untile(&file[HEADER_LEN..], h.format, w, ht, h.pitch.max(w), h.tiled, h.endian)?;
    if (ox, oy) == (0, 0) {
        return Ok((h, full));
    }
    let (bw, bpb) = h.format.block().expect("untile checked the format");
    let (bx0, by0) = (ox / bw, oy / bw);
    let (src_w, dst_w, dst_h) = (w.div_ceil(bw), h.width.div_ceil(bw), h.height.div_ceil(bw));
    let mut data = Vec::with_capacity((dst_w * dst_h * bpb) as usize);
    for y in 0..dst_h {
        let s = (((y + by0) * src_w + bx0) * bpb) as usize;
        data.extend_from_slice(&full.data[s..s + (dst_w * bpb) as usize]);
    }
    let (width, height, format) = (h.width, h.height, h.format);
    Ok((h, Image { width, height, format, data }))
}

/// Turn one Xenos texture level into linear little-endian blocks / RGBA texels.
/// `pitch` is the row pitch in texels; `endian` is the fetch constant's endian field.
pub fn untile(data: &[u8], format: Format, width: u32, height: u32, pitch: u32, tiled: bool, endian: u32) -> Result<Image, Error> {
    let (bw, bpb) = format.block().ok_or(Error::Unsupported(format!("texture format {format:?}")))?;
    let blocks_w = width.div_ceil(bw);
    let blocks_h = height.div_ceil(bw);
    // Storage is laid out in 32x32-block tiles over the pitch-aligned width.
    let pitch_blocks = pitch.max(width).div_ceil(bw);
    let log_bpb = bpb.trailing_zeros();

    let mut out = vec![0u8; (blocks_w * blocks_h * bpb) as usize];
    for y in 0..blocks_h {
        for x in 0..blocks_w {
            let src_block = if tiled { tiled_offset_2d(x, y, pitch_blocks, log_bpb) } else { y * pitch_blocks + x };
            let src = (src_block * bpb) as usize;
            let dst = ((y * blocks_w + x) * bpb) as usize;
            let block = data.get(src..src + bpb as usize).ok_or(Error::Truncated("texel data"))?;
            out[dst..dst + bpb as usize].copy_from_slice(block);
        }
    }
    swap_endian(&mut out, endian);

    if format == Format::Argb8 {
        // After the dword swap texels are B,G,R,A in memory; reorder to RGBA.
        for px in out.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    Ok(Image { width, height, format, data: out })
}

fn swap_endian(d: &mut [u8], endian: u32) {
    match endian {
        1 => d.chunks_exact_mut(2).for_each(|c| c.swap(0, 1)),
        2 => d.chunks_exact_mut(4).for_each(|c| c.reverse()),
        3 => d.chunks_exact_mut(4).for_each(|c| {
            c.swap(0, 2);
            c.swap(1, 3);
        }),
        _ => {}
    }
}

/// Xenos 2D tiling: element index of block (x, y) in a surface `width` blocks wide.
/// Same arithmetic as `XGAddress2DTiledOffset`.
pub fn tiled_offset_2d(x: u32, y: u32, width: u32, log_bpb: u32) -> u32 {
    let aligned_width = (width + 31) & !31;
    let macro_ = ((x >> 5) + (y >> 5) * (aligned_width >> 5)) << (log_bpb + 7);
    let micro = ((x & 7) + ((y & 6) << 2)) << log_bpb;
    let offset = macro_ + ((micro & !15) << 1) + (micro & 15) + ((y & 8) << (3 + log_bpb)) + ((y & 1) << 4);
    (((offset & !511) << 3)
        + ((offset & 448) << 2)
        + (offset & 63)
        + ((y & 16) << 7)
        + (((((y & 8) >> 2) + (x >> 3)) & 3) << 6))
        >> log_bpb
}

/// Decode an [`Image`] to RGBA8 (for previews and formats the engine can't sample directly).
pub fn to_rgba8(img: &Image) -> Result<Vec<u8>, Error> {
    let (w, h) = (img.width as usize, img.height as usize);
    if img.format == Format::Argb8 {
        return Ok(img.data.clone());
    }
    if img.format == Format::Dxt3A {
        // 16 nibbles per block, texel i = byte i/2, low nibble first (as BC2's alpha half).
        let mut out = vec![0u8; w * h * 4];
        let bw = w.div_ceil(4);
        for (bi, blk) in img.data.chunks_exact(8).enumerate().take(bw * h.div_ceil(4)) {
            let (bx, by) = (bi % bw * 4, bi / bw * 4);
            for i in 0..16 {
                let (x, y) = (bx + i % 4, by + i / 4);
                if x < w && y < h {
                    let v = (blk[i / 2] >> (4 * (i % 2))) & 0xF;
                    out[(y * w + x) * 4..][..4].fill(v * 17);
                }
            }
        }
        return Ok(out);
    }
    let mut px = vec![0u32; w * h];
    let r = match img.format {
        Format::Dxt1 => texture2ddecoder::decode_bc1(&img.data, w, h, &mut px),
        Format::Dxt3 => texture2ddecoder::decode_bc2(&img.data, w, h, &mut px),
        Format::Dxt5 => texture2ddecoder::decode_bc3(&img.data, w, h, &mut px),
        Format::Dxn => texture2ddecoder::decode_bc5(&img.data, w, h, &mut px),
        Format::Dxt5A => texture2ddecoder::decode_bc4(&img.data, w, h, &mut px),
        f => return Err(Error::Unsupported(format!("{f:?}"))),
    };
    r.map_err(|e| Error::Unsupported(e.to_string()))?;
    // texture2ddecoder packs BGRA into u32 (little-endian: B, G, R, A).
    Ok(px
        .iter()
        .flat_map(|p| {
            let [b, g, r, a] = p.to_le_bytes();
            [r, g, b, a]
        })
        .collect())
}
