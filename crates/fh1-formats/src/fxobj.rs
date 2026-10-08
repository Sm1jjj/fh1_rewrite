//! `.fxobj` compiled track shaders — only the vertex declaration is read here.
//!
//! From the Colorado recon (`docs/COLORADO_RECON.md`, "Vertex layouts"): one declaration per
//! file near its end, as 12-byte elements `u16 stream, u16 offset, u32 xbox_decl_type, u8 method,
//! u8 usage, u8 usage_index, u8 pad` (big-endian), followed by NUL-separated element names
//! (`position normal uv uv1 uv2 tangent color`). The elements aren't at a fixed offset, so they
//! are found by counting the names and stepping back, then validated.

use crate::Error;

pub const TYPE_FLOAT3: u32 = 0x002A_23B9;
/// Packed normal: x = bits 0-9, y = 10-19, z = 20-29, each signed 10-bit / 511.
pub const TYPE_DEC3N: u32 = 0x001A_2187;
/// Texture coordinate: 2 × u16 / 65535.
pub const TYPE_USHORT2N: u32 = 0x002C_2059;
pub const TYPE_D3DCOLOR: u32 = 0x0018_2886;

pub const USAGE_POSITION: u8 = 0;
pub const USAGE_NORMAL: u8 = 3;
pub const USAGE_TEXCOORD: u8 = 5;
pub const USAGE_TANGENT: u8 = 6;
pub const USAGE_BINORMAL: u8 = 7;
pub const USAGE_COLOR: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element {
    pub offset: u16,
    pub decl_type: u32,
    pub usage: u8,
    pub usage_index: u8,
}

impl Element {
    pub fn size(&self) -> usize {
        match self.decl_type {
            TYPE_FLOAT3 => 12,
            _ => 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VertexDecl {
    pub elements: Vec<Element>,
}

impl VertexDecl {
    pub fn stride(&self) -> usize {
        self.elements.iter().map(|e| e.offset as usize + e.size()).max().unwrap_or(0)
    }

    pub fn find(&self, usage: u8, index: u8) -> Option<Element> {
        self.elements.iter().copied().find(|e| e.usage == usage && e.usage_index == index)
    }
}

const KNOWN: [u32; 4] = [TYPE_FLOAT3, TYPE_DEC3N, TYPE_USHORT2N, TYPE_D3DCOLOR];

pub fn vertex_decl(d: &[u8]) -> Result<VertexDecl, Error> {
    let names_at = rfind(d, b"position\0").ok_or(Error::BadMagic("fxobj: no vertex declaration names"))?;
    let count = d[names_at..].split(|&b| b == 0).filter(|s| !s.is_empty()).count();
    if count == 0 || count > 16 {
        return Err(Error::BadMagic("fxobj element count"));
    }
    let be16 = |o: usize| u16::from_be_bytes([d[o], d[o + 1]]);
    let be32 = |o: usize| u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
    // The elements end within a few bytes of the names; try nearby starts and keep the valid one.
    for gap in 0..16usize {
        let Some(start) = names_at.checked_sub(count * 12 + gap) else { break };
        let elements: Vec<Element> = (0..count)
            .map(|i| {
                let o = start + i * 12;
                Element { offset: be16(o + 2), decl_type: be32(o + 4), usage: d[o + 9], usage_index: d[o + 10] }
            })
            .collect();
        let valid = elements.iter().all(|e| KNOWN.contains(&e.decl_type))
            && elements[0].usage == USAGE_POSITION
            && elements[0].offset == 0
            && elements[0].decl_type == TYPE_FLOAT3;
        if valid {
            return Ok(VertexDecl { elements });
        }
    }
    Err(Error::BadMagic("fxobj: vertex declaration not found"))
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// Decode a DEC3N packed vector (x bits 0-9, y 10-19, z 20-29, signed 10-bit).
pub fn dec3n(v: u32) -> [f32; 3] {
    let s = |bits: u32| {
        let x = (bits & 0x3FF) as i32;
        let x = if x >= 512 { x - 1024 } else { x };
        (x as f32 / 511.0).clamp(-1.0, 1.0)
    };
    [s(v), s(v >> 10), s(v >> 20)]
}
