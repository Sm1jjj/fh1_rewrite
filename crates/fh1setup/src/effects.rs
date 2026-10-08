//! `effects` group: the particle effects from `media/effects.zip` for fh1-render particles.rs (docs/EFFECTS.md).
//!
//! Output: every `<Effect>` / track XML as is (`Smoke.xml`, `Dirt1.xml`, `DefaultTrack.xml`, ...) and every `.xds` texture as
//! `<stem>.dds`: DX10 RGBA8 (format 28) with a box-filtered mip chain, texel values as stored (the game's particle PS
//! writes tex × colour straight into its gamma-2 buffer, so no colour-space change).

use std::path::Path;

use anyhow::{Context, Result};
use fh1_formats::xds;
use fh1_formats::zip::Archive;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let mut ar = Archive::open(disc.join("media/effects.zip")).context("media/effects.zip")?;
    std::fs::create_dir_all(out)?;
    let (mut xmls, mut textures, mut failed) = (0, 0, Vec::new());
    for e in ar.entries.clone() {
        let name = e.name.rsplit(['/', '\\']).next().unwrap_or(&e.name).to_string();
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".xml") {
            std::fs::write(out.join(&name), ar.read(&e)?)?;
            xmls += 1;
        } else if lower.ends_with(".xds") {
            let bytes = ar.read(&e)?;
            let stem = &name[..name.len() - 4];
            match decode(&bytes) {
                Ok((w, h, rgba)) => {
                    write_dds(&out.join(format!("{stem}.dds")), w, h, &rgba)?;
                    textures += 1;
                }
                Err(err) => failed.push(format!("{name}: {err}")),
            }
        }
    }
    println!("[effects] {xmls} xml, {textures} textures");
    for f in &failed {
        println!("[effects] skipped {f}");
    }
    Ok(())
}

fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let (_, img) = xds::decode_base(bytes)?;
    let rgba = xds::to_rgba8(&img)?;
    Ok((img.width, img.height, rgba))
}

/// DX10 DDS, RGBA8 unorm, full mip chain (2×2 box filter).
fn write_dds(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let mut levels = vec![(w, h, rgba.to_vec())];
    while let Some((lw, lh, px)) = levels.last().filter(|l| l.0 > 1 || l.1 > 1) {
        let (nw, nh) = ((lw / 2).max(1), (lh / 2).max(1));
        let mut next = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let mut sum = 0u32;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let (sx, sy) = ((x * 2 + dx).min(lw - 1), (y * 2 + dy).min(lh - 1));
                        sum += px[((sy * lw + sx) * 4 + c) as usize] as u32;
                    }
                    next[((y * nw + x) * 4 + c) as usize] = ((sum + 2) / 4) as u8;
                }
            }
        }
        levels.push((nw, nh, next));
    }
    let mut b = Vec::with_capacity(148 + rgba.len() * 2);
    let u = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_le_bytes());
    b.extend_from_slice(b"DDS ");
    u(&mut b, 124);
    u(&mut b, 0x1 | 0x2 | 0x4 | 0x1000 | 0x20000 | 0x8); // caps, height, width, pixelformat, mipmapcount, pitch
    u(&mut b, h);
    u(&mut b, w);
    u(&mut b, w * 4);
    u(&mut b, 0);
    u(&mut b, levels.len() as u32);
    b.extend_from_slice(&[0u8; 44]);
    // DDS_PIXELFORMAT: size 32, DDPF_FOURCC, "DX10".
    u(&mut b, 32);
    u(&mut b, 0x4);
    b.extend_from_slice(b"DX10");
    b.extend_from_slice(&[0u8; 20]);
    u(&mut b, 0x1000 | 0x8 | 0x400000); // texture, complex, mipmap
    b.extend_from_slice(&[0u8; 16]);
    // DDS_HEADER_DXT10: DXGI_FORMAT_R8G8B8A8_UNORM, 2D, no flags, 1 element.
    u(&mut b, 28);
    u(&mut b, 3);
    u(&mut b, 0);
    u(&mut b, 1);
    u(&mut b, 0);
    debug_assert_eq!(b.len(), 148);
    for (_, _, px) in &levels {
        b.extend_from_slice(px);
    }
    std::fs::write(path, b)?;
    Ok(())
}
