//! `.fxobj` effect files: a header, a pointer-fixed-up FXL effect body, then the vertex
//! declaration. See `docs/SHADERS.md` for the layout.
//!
//! Decoded so far: the embedded shader containers, the techniques and their passes (which
//! vertex/pixel shader each pass binds) and each pass's render states.

use crate::container::{ShaderBlob, Stage, MAGIC_PS, MAGIC_VS};
use crate::Error;

/// Marker word 12 bytes before the effect body.
const BODY_MARKER: u32 = 0xA3D7_0141;

#[derive(Debug, Clone)]
pub struct Pass {
    pub name: String,
    /// Index into [`Effect::shaders`].
    pub vs: Option<usize>,
    pub ps: Option<usize>,
    /// Xbox 360 `D3DRS_*` byte offset → value.
    pub render_states: Vec<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct Technique {
    pub name: String,
    pub passes: Vec<Pass>,
}

#[derive(Debug, Clone)]
pub struct Effect {
    pub hash: u32,
    pub shaders: Vec<ShaderBlob>,
    pub techniques: Vec<Technique>,
}

/// Xbox 360 render state offsets (`D3DRS_*`) identified so far.
pub mod rs {
    pub const ZENABLE: u32 = 0x28;
    pub const ZFUNC: u32 = 0x2C;
    pub const ZWRITEENABLE: u32 = 0x30;
    pub const CULLMODE: u32 = 0x38;
    pub const ALPHABLENDENABLE: u32 = 0x3C;
    /// Xenos blend factors: 0 zero, 1 one, 4 src colour, 5 inv src colour, 6 src alpha,
    /// 7 inv src alpha, 8 dest colour, 9 inv dest colour, 10 dest alpha, 11 inv dest alpha.
    pub const SRCBLEND: u32 = 0x48;
    pub const DESTBLEND: u32 = 0x4C;
    pub const ALPHATESTENABLE: u32 = 0x60;
    /// Float bit patterns, set together by the *_DEPTHBIAS shaders (1.0 and 1e-5 / 6.1e-5);
    /// most likely SLOPESCALEDEPTHBIAS and DEPTHBIAS. UNVERIFIED.
    pub const SLOPESCALEDEPTHBIAS: u32 = 0xCC;
    pub const DEPTHBIAS: u32 = 0xD0;
    /// Most likely ALPHATOMASKENABLE / ALPHATOMASKOFFSETS (alpha-to-coverage); UNVERIFIED.
    pub const ALPHATOMASKENABLE: u32 = 0x150;
    pub const ALPHATOMASKOFFSETS: u32 = 0x154;
}

impl Pass {
    pub fn state(&self, id: u32) -> Option<u32> {
        self.render_states.iter().find(|(k, _)| *k == id).map(|(_, v)| *v)
    }
}

impl Effect {
    pub fn parse(d: &[u8]) -> Result<Self, Error> {
        let be = |o: usize| -> Result<u32, Error> {
            d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("fxobj"))
        };
        let _cstr = |o: usize| -> Result<String, Error> {
            let s = d.get(o..).ok_or(Error::Truncated("fxobj string"))?;
            let end = s.iter().position(|&b| b == 0).ok_or(Error::Truncated("fxobj string"))?;
            Ok(String::from_utf8_lossy(&s[..end]).into_owned())
        };
        if be(0)? != 0x101 {
            return Err(Error::BadMagic("fxobj version"));
        }
        let hash = be(4)?;
        let technique_count = be(12)? as usize;

        let mut shaders = Vec::new();
        let mut o = 0x1C;
        while o + 4 <= d.len() {
            let w = u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
            if w == MAGIC_VS || w == MAGIC_PS {
                if let Ok(blob) = ShaderBlob::parse(&d[o..], o) {
                    let len = ShaderBlob::container_len(&d[o..])?;
                    shaders.push(blob);
                    o += len.max(4) & !3;
                    continue;
                }
            }
            o += 4;
        }

        // Effect body.
        let marker = (0x1C..0x10000.min(d.len().saturating_sub(4))).step_by(4).find(|&o| be(o).ok() == Some(BODY_MARKER)).ok_or(Error::BadMagic("fxobj body marker"))?;
        let (techniques, _) = parse_body(d, marker + 12, &mut shaders, Some(technique_count))?;
        Ok(Self { hash, shaders, techniques })
    }

    /// An effect body embedded in default.xex: `d` starts at the `0xA3D70141` marker word. Shaders
    /// are parsed from the containers the passes point at. Returns the effect and the number of
    /// bytes of `d` it uses (to cut it out of the image).
    pub fn parse_embedded(d: &[u8]) -> Result<(Self, usize), Error> {
        if d.get(..4) != Some(&BODY_MARKER.to_be_bytes()[..]) {
            return Err(Error::BadMagic("effect body marker"));
        }
        let mut shaders = Vec::new();
        let (techniques, used) = parse_body(d, 12, &mut shaders, None)?;
        Ok((Self { hash: 0, shaders, techniques }, used))
    }

    pub fn technique(&self, name: &str) -> Option<&Technique> {
        self.techniques.iter().find(|t| t.name == name)
    }
}

/// Techniques of an effect body at `base`. Shaders not yet in `shaders` are parsed at the
/// offsets the passes reference. Returns the techniques and the highest byte offset used.
fn parse_body(d: &[u8], base: usize, shaders: &mut Vec<ShaderBlob>, expect: Option<usize>) -> Result<(Vec<Technique>, usize), Error> {
    let mut used = base;
    let be = |o: usize| -> Result<u32, Error> {
        d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("effect"))
    };
    let cstr = |o: usize, used: &mut usize| -> Result<String, Error> {
        let s = d.get(o..).ok_or(Error::Truncated("effect string"))?;
        let end = s.iter().position(|&b| b == 0).ok_or(Error::Truncated("effect string"))?;
        *used = (*used).max(o + end + 1);
        Ok(String::from_utf8_lossy(&s[..end]).into_owned())
    };
    let hdr = base + 0x200;
    let tech_table = be(hdr)? as usize + base;
    let technique_count = be(hdr + 12)? as usize;
    if expect.is_some_and(|e| e != technique_count) || technique_count > 4096 {
        return Err(Error::BadMagic("effect technique count"));
    }
    let mut shader_at = |ptr: u32, stage: Stage, used: &mut usize| -> Option<usize> {
        let p = ptr as usize + base + 8;
        if let Some(i) = shaders.iter().position(|s| s.file_offset == p && s.stage == stage) {
            return Some(i);
        }
        let blob = ShaderBlob::parse(d.get(p..)?, p).ok().filter(|b| b.stage == stage)?;
        *used = (*used).max(p + ShaderBlob::container_len(&d[p..]).ok()?);
        shaders.push(blob);
        Some(shaders.len() - 1)
    };
    let mut techniques = Vec::with_capacity(technique_count);
    for t in 0..technique_count {
        let rec = be(tech_table + t * 4)? as usize + base;
        let name = cstr(be(rec)? as usize + base, &mut used)?;
        let pass_count = be(rec + 4)? as usize;
        let mut passes = Vec::with_capacity(pass_count);
        for p in 0..pass_count {
            // Pass pointers follow the record header; passes can be shared between techniques.
            let q = be(rec + 16 + p * 4)? as usize + base;
            let pname = cstr(be(q)? as usize + base, &mut used)?;
            let block = be(q + 8)? as usize + base;
            let mut k = block;
            while be(k)? != 0xFFFF_FFFF {
                k += 4;
                if k > block + 0x100 {
                    return Err(Error::BadMagic("effect state block"));
                }
            }
            let vs = shader_at(be(k + 4)?, Stage::Vertex, &mut used);
            let ps = shader_at(be(k + 8)?, Stage::Pixel, &mut used);
            let rsb = be(q + 12)? as usize + base;
            let mut render_states = Vec::new();
            // Headers seen: 0xB2200000, 0xB0200000, 0xBE200000 (the masks differ); Forza Motorsport 4 also sets
            // bit 16 (0xB2210000 on its decal passes, docs/FM4_RECON.md), same layout.
            if be(rsb)? & 0xF0FE_0000 == 0xB020_0000 {
                let n = be(rsb + 24)? as usize;
                for i in 0..n.min(64) {
                    render_states.push((be(rsb + 28 + i * 8)?, be(rsb + 32 + i * 8)?));
                }
                used = used.max(rsb + 28 + n.min(64) * 8);
            }
            used = used.max(k + 12);
            passes.push(Pass { name: pname, vs, ps, render_states });
        }
        techniques.push(Technique { name, passes });
    }
    Ok((techniques, used))
}
