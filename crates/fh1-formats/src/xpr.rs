//! Xbox packed resources (`.xpr`, magic `XPR2`, big-endian): the car cube maps
//! (`media/tracks/**/staticcarcubemap.xpr`, `Shared.zip` `staticCube*.xpr`, `interiorStaticCube*.xpr`,
//! `headlight.xpr`).
//!
//! Layout (verified on all 12 on the disc): `"XPR2"`, `u32 header size`, `u32 data size`, `u32 resource
//! count`, then per resource `{[u8; 4] type, u32 offset, u32 size, u32 name offset}`; offsets count from byte
//! 12. A texture resource (`TXCM` cube, `TX2D` 2D) is a 52-byte `D3DBaseTexture` header, the same as an
//! `.xds` header. Its data starts at `12 + header size`: the base level at the fetch constant's base
//! address, the mips at its mip address (both in 4 KiB pages). Cube faces and levels are laid out as
//! [`crate::bundle::decode_layers`] describes.

use crate::bundle;
use crate::xds::{Header, Image, HEADER_LEN};
use crate::Error;

#[derive(Debug, Clone)]
pub struct Resource {
    pub kind: [u8; 4],
    pub name: String,
    pub header: Header,
    /// Absolute file offsets of the base level and the mip chain.
    pub base_offset: usize,
    pub mip_offset: usize,
}

fn be(d: &[u8], o: usize) -> Result<u32, Error> {
    Ok(u32::from_be_bytes(d.get(o..o + 4).ok_or(Error::Truncated("xpr"))?.try_into().unwrap()))
}

pub fn parse(d: &[u8]) -> Result<Vec<Resource>, Error> {
    if d.get(..4) != Some(b"XPR2") {
        return Err(Error::BadMagic("XPR2"));
    }
    let data_start = 12 + be(d, 4)? as usize;
    let n = be(d, 12)? as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let r = 16 + 16 * i;
        let kind: [u8; 4] = d.get(r..r + 4).ok_or(Error::Truncated("xpr resource"))?.try_into().unwrap();
        let offset = 12 + be(d, r + 4)? as usize;
        let name_at = 12 + be(d, r + 12)? as usize;
        let name = d.get(name_at..).unwrap_or_default().split(|&b| b == 0).next().unwrap_or_default();
        let header = Header::parse(d.get(offset..offset + HEADER_LEN).ok_or(Error::Truncated("xpr texture header"))?)?;
        out.push(Resource {
            kind,
            name: String::from_utf8_lossy(name).into_owned(),
            base_offset: data_start + ((header.fetch[1] >> 12) << 12) as usize,
            mip_offset: data_start + ((header.fetch[5] >> 12) << 12) as usize,
            header,
        });
    }
    Ok(out)
}

/// Every level (largest first) of every layer (6 for cube maps) of a texture resource: `[level][layer]`.
pub fn decode(d: &[u8], r: &Resource) -> Result<Vec<Vec<Image>>, Error> {
    let h = &r.header;
    let layers = if h.dimension == 3 { 6 } else { 1 };
    let mip_count = h.max_mip + 1;
    let base = d.get(r.base_offset..).ok_or(Error::Truncated("xpr base"))?;
    let mut levels = bundle::decode_layers(base, h.format, h.endian, h.width, h.height, 0, 1, layers)?;
    if mip_count > 1 && h.width.next_power_of_two().min(h.height.next_power_of_two()) > 16 {
        let mips = d.get(r.mip_offset..).ok_or(Error::Truncated("xpr mips"))?;
        levels.extend(bundle::decode_layers(mips, h.format, h.endian, h.width, h.height, 1, mip_count, layers)?);
    } else if mip_count > 1 {
        // The whole chain is one packed tail at the base address.
        levels = bundle::decode_layers(base, h.format, h.endian, h.width, h.height, 0, mip_count, layers)?;
    }
    Ok(levels)
}
