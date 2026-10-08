//! Track textures (`.bix` + `_B.bix`, CAFF `.bin`, `.bundle` previews) -> DDS (DX10 header), BC-compressed.
//!
//! Every level keeps the game's own blocks where the file stores them in a standard BCn (DXT1/3/5, DXN,
//! DXT5A): level 0, and levels 1.. from the `.bix` / CAFF chain (`fh1_formats::bundle::decode_chain`; see
//! `game_mips` for the all-zero chains). Missing levels are box-filtered (in linear light for colour
//! formats) and re-encoded. Formats with no BCn twin (ARGB8, DXT3A) are encoded to BC3 throughout. DXGI
//! formats are written as UNORM: the same texture can be a diffuse (sRGB) in one material and a mask in
//! another, so the renderer chooses (`Converted::word` carries the game's gamma flag).

use anyhow::Result;
use fh1_formats::{bix, bundle, caff, xds};
use texpresso::{Algorithm, Params};

pub struct Converted {
    pub dds: Vec<u8>,
    /// Some texel has alpha < 128 (cut-out foliage, fences, decals).
    pub alpha: bool,
    /// The game's format word (fetch-constant dword 1 as stored in .bix / CAFF / .bundle): low 6 bits GPU format,
    /// bits 6-7 endian; bits 8-13 = 0x3F on colour textures (diffuse, lightmaps), 0x01 on masks/normals
    /// (gamma flag, UNVERIFIED; docs/LIGHTMAPS.md).
    pub word: u32,
}

/// `.bix`: level 0 from `_B.bix`, the game's own levels 1.. from the `.bix` itself.
pub fn bix_to_dds(header: &[u8], base: &[u8]) -> Result<Converted> {
    let (h, img) = bix::decode_base(header, base)?;
    let stored = bundle::decode_chain(&header[bix::HEADER_LEN..], h.format, h.endian, h.width, h.height, 1, h.mip_count)?;
    let word = u32::from_be_bytes(header[16..20].try_into()?);
    faces_to_dds(&[(img, game_mips(stored))], word)
}

/// CAFF textures: 2D (level 0 and the game's mips decoded as one Xenos chain, which also places level 0 of the
/// narrow 8x256 ones, stored inside a packed tail), or cube maps (written as a DDS cube, faces +X -X +Y -Y +Z -Z as
/// stored).
pub fn caff_to_dds(file: &[u8]) -> Result<Converted> {
    let t = caff::parse(file)?;
    if t.cube {
        let faces: Vec<_> = caff::decode_faces(file)?.1.into_iter().map(|f| (f, Vec::new())).collect();
        return faces_to_dds(&faces, t.raw_format);
    }
    let mut chain = bundle::decode_chain(&file[t.gpu_offset..], t.format, t.endian, t.width, t.height, 0, t.mip_count.max(1))?;
    let base = chain.remove(0);
    faces_to_dds(&[(base, game_mips(chain))], t.raw_format)
}

/// Bundle previews (fh1_formats::bundle): the only data the disc has for PVS runtime textures. `chain` largest
/// first.
pub fn preview_to_dds(mut chain: Vec<xds::Image>, word: u32) -> Result<Converted> {
    let base = chain.remove(0);
    faces_to_dds(&[(base, game_mips(chain))], word)
}

/// RGBA8 texels -> BC3 DDS with generated mips (for images decoded by hand, e.g. punch-through BC1).
pub fn rgba_to_dds(rgba: Vec<u8>, width: u32, height: u32) -> Result<Converted> {
    faces_to_dds(&[(xds::Image { width, height, format: xds::Format::Argb8, data: rgba }, Vec::new())], 0)
}

/// The game's stored levels below level 0, unless all zero (837 .bix, mostly 128² lightmaps, and many 64² CAFF AO
/// maps ship zeroed chains; those keep generated mips: whether the game samples its black mips is UNVERIFIED).
fn game_mips(chain: Vec<xds::Image>) -> Vec<xds::Image> {
    if chain.iter().all(|l| l.data.iter().all(|&b| b == 0)) {
        Vec::new()
    } else {
        chain
    }
}

/// One face = 2D texture; six = cube map (each face with its own mip chain, as DDS stores them). Each face comes
/// with the game's stored levels below level 0 (may be empty).
fn faces_to_dds(faces: &[(xds::Image, Vec<xds::Image>)], word: u32) -> Result<Converted> {
    let mut levels: Vec<Vec<u8>> = Vec::new();
    let (mut dxgi, mut alpha, mut mips) = (0, false, 0);
    for (img, stored) in faces {
        let (d, a, l) = mip_chain(img, stored)?;
        (dxgi, alpha, mips) = (d, alpha || a, l.len());
        levels.extend(l);
    }
    let (w, h) = (faces[0].0.width as usize, faces[0].0.height as usize);
    let cube = faces.len() == 6;
    let mut dds = Vec::with_capacity(148 + levels.iter().map(Vec::len).sum::<usize>());
    let mut put = |v: u32| dds.extend_from_slice(&v.to_le_bytes());
    put(0x2053_4444); // "DDS "
    put(124);
    put(0x1 | 0x2 | 0x4 | 0x1000 | 0x20000 | 0x80000); // caps, height, width, pixelformat, mipcount, linearsize
    put(h as u32);
    put(w as u32);
    put(levels[0].len() as u32);
    put(0);
    put(mips as u32);
    for _ in 0..11 {
        put(0);
    }
    put(32); // pixel format: size, DDPF_FOURCC, "DX10"
    put(0x4);
    put(0x3031_5844);
    for _ in 0..5 {
        put(0);
    }
    put(0x1000 | 0x400000 | 0x8); // texture, mipmap, complex
    put(if cube { 0xFE00 } else { 0 }); // caps2: cube map, all six faces
    for _ in 0..3 {
        put(0);
    }
    put(dxgi);
    put(3); // TEXTURE2D
    put(if cube { 0x4 } else { 0 }); // TEXTURECUBE
    put(1);
    put(0);
    for l in &levels {
        dds.extend_from_slice(l);
    }
    Ok(Converted { dds, alpha, word })
}

/// (DXGI format, has cut-out alpha, levels largest first).
fn mip_chain(img: &xds::Image, stored: &[xds::Image]) -> Result<(u32, bool, Vec<Vec<u8>>)> {
    let (w, h) = (img.width as usize, img.height as usize);
    // BCn needs block-aligned base levels; the few odd sizes (1x256 gradients) stay uncompressed RGBA8.
    let raw = w % 4 != 0 || h % 4 != 0;
    let rgba = xds::to_rgba8(img)?;
    let (fmt, dxgi, keep_base) = match img.format {
        _ if raw => (texpresso::Format::Bc3, DXGI_RGBA8, false),
        xds::Format::Dxt1 => (texpresso::Format::Bc1, DXGI_BC1, true),
        xds::Format::Dxt3 => (texpresso::Format::Bc2, DXGI_BC2, true),
        xds::Format::Dxt5 => (texpresso::Format::Bc3, DXGI_BC3, true),
        xds::Format::Dxn => (texpresso::Format::Bc5, DXGI_BC5, true),
        xds::Format::Dxt5A => (texpresso::Format::Bc4, DXGI_BC4, true),
        _ => (texpresso::Format::Bc3, DXGI_BC3, false),
    };
    // Colour formats are filtered as sRGB colour (most uses are diffuse); BC4/BC5 linearly.
    let srgb = matches!(dxgi, DXGI_BC1 | DXGI_BC2 | DXGI_BC3 | DXGI_RGBA8);
    let alpha = rgba.chunks_exact(4).any(|p| p[3] < 128);
    let params = Params { algorithm: Algorithm::RangeFit, ..Default::default() };

    let mut levels: Vec<Vec<u8>> = Vec::new();
    let (mut cw, mut ch, mut cur) = (w, h, rgba);
    loop {
        // The game's own blocks where it ships them: level 0, then stored mips of the same BCn format and size.
        let game = match levels.len() {
            0 => Some(img),
            n => stored.get(n - 1),
        }
        .filter(|l| keep_base && l.format == img.format && (l.width as usize, l.height as usize) == (cw, ch));
        if let Some(l) = game {
            if !levels.is_empty() {
                cur = xds::to_rgba8(l)?; // later generated levels continue from the game's level
            }
            levels.push(l.data.clone());
        } else if raw {
            levels.push(cur.clone());
        } else {
            let mut out = vec![0u8; fmt.compressed_size(cw, ch)];
            fmt.compress(&cur, cw, ch, params, &mut out);
            levels.push(out);
        }
        if cw == 1 && ch == 1 {
            break;
        }
        (cur, cw, ch) = downsample(&cur, cw, ch, srgb);
    }

    Ok((dxgi, alpha, levels))
}

const DXGI_RGBA8: u32 = 28;
const DXGI_BC1: u32 = 71;
const DXGI_BC2: u32 = 74;
const DXGI_BC3: u32 = 77;
const DXGI_BC4: u32 = 80;
const DXGI_BC5: u32 = 83;

/// Half-size box filter; colour channels averaged in linear light when `srgb`.
fn downsample(src: &[u8], w: usize, h: usize, srgb: bool) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let to_lin = |v: u8| if srgb { (v as f32 / 255.0).powf(2.2) } else { v as f32 / 255.0 };
    let from_lin = |v: f32| (if srgb { v.powf(1.0 / 2.2) } else { v } * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..nh {
        for x in 0..nw {
            let mut acc = [0f32; 4];
            let mut n = 0.0;
            for sy in [y * 2, (y * 2 + 1).min(h - 1)] {
                for sx in [x * 2, (x * 2 + 1).min(w - 1)] {
                    let p = &src[(sy * w + sx) * 4..][..4];
                    for c in 0..3 {
                        acc[c] += to_lin(p[c]);
                    }
                    acc[3] += p[3] as f32 / 255.0;
                    n += 1.0;
                }
            }
            let o = &mut out[(y * nw + x) * 4..][..4];
            for c in 0..3 {
                o[c] = from_lin(acc[c] / n);
            }
            o[3] = (acc[3] / n * 255.0 + 0.5) as u8;
        }
    }
    (out, nw, nh)
}

#[cfg(test)]
mod tests {
    /// `FH1_CAFF=<file.bin>`: convert one CAFF texture (skips without it).
    #[test]
    fn caff_one() {
        let Ok(p) = std::env::var("FH1_CAFF") else { return };
        let d = std::fs::read(p).unwrap();
        println!("{:?}", fh1_formats::caff::parse(&d));
        let c = super::caff_to_dds(&d).unwrap();
        println!("dds {} bytes", c.dds.len());
        // The game's levels must match the box-downscaled level above (mean abs difference, 0-255).
        let t = fh1_formats::caff::parse(&d).unwrap();
        let chain = fh1_formats::bundle::decode_chain(&d[t.gpu_offset..], t.format, t.endian, t.width, t.height, 0, t.mip_count).unwrap();
        for l in 1..chain.len().min(6) {
            let (a, b) = (fh1_formats::xds::to_rgba8(&chain[l - 1]).unwrap(), fh1_formats::xds::to_rgba8(&chain[l]).unwrap());
            let (down, _, _) = super::downsample(&a, chain[l - 1].width as usize, chain[l - 1].height as usize, false);
            let diff = down.iter().zip(&b).map(|(x, y)| (*x as i32 - *y as i32).abs() as f64).sum::<f64>() / b.len() as f64;
            println!("level {l} {}x{}: mean diff {diff:.2}", chain[l].width, chain[l].height);
        }
    }
}
