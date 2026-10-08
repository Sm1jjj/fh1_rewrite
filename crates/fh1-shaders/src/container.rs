//! Xbox 360 compiled shader container (`0x102A1100` pixel / `0x102A1101` vertex), as embedded
//! in `.fxobj` effects. All fields are big-endian.
//!
//! ```text
//! +0x00 u32 magic            0x102A1100 PS, 0x102A1101 VS
//! +0x04 u32 virtual_size     microcode section starts here (relative to the container)
//! +0x08 u32 physical_size    size of the microcode section
//! +0x10 u32 ctab_offset      u32 size, then a D3DXSHADER_CONSTANTTABLE (offsets relative to it)
//! +0x14 u32 defs_offset      literal constant definitions (0 = none)
//! +0x18 u32 shader_offset    program header (below)
//! ```
//! Program header: `u32 physical_offset, size, ?, fieldC (PS: vPos register = (fieldC >> 8) & 0xFF),
//! ?, interpolator_info (count = (info >> 5) & 0x1F)`; then
//! VS: `u32 n_pre, n_elements, n_post` and `n_pre + n_elements + interpolator_count` u32s;
//! PS: `u32 ?, outputs (bit 0-3 colour targets, bit 4 depth)` and `interpolator_count` u32s.
//! Vertex element = `address:12 (fetch instruction address), usage:4, usage_index:4`.
//! Interpolator = `usage_index:4, usage:4, reg:4`.
//! (Layout cross-checked against XenosRecomp's `shader.h`, MIT; field names are ours.)

use crate::Error;

pub const MAGIC_PS: u32 = 0x102A_1100;
pub const MAGIC_VS: u32 = 0x102A_1101;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    Vertex,
    Pixel,
}

/// D3DDECLUSAGE.
pub mod usage {
    pub const POSITION: u8 = 0;
    pub const BLEND_WEIGHT: u8 = 1;
    pub const BLEND_INDICES: u8 = 2;
    pub const NORMAL: u8 = 3;
    pub const PSIZE: u8 = 4;
    pub const TEXCOORD: u8 = 5;
    pub const TANGENT: u8 = 6;
    pub const BINORMAL: u8 = 7;
    pub const TESSFACTOR: u8 = 8;
    pub const POSITIONT: u8 = 9;
    pub const COLOR: u8 = 10;
    pub const FOG: u8 = 11;
    pub const DEPTH: u8 = 12;
    pub const SAMPLE: u8 = 13;

    pub fn name(u: u8) -> &'static str {
        const N: [&str; 14] = [
            "position", "blendweight", "blendindices", "normal", "psize", "texcoord", "tangent", "binormal", "tessfactor",
            "positiont", "color", "fog", "depth", "sample",
        ];
        N.get(u as usize).copied().unwrap_or("unknown")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegisterSet {
    Bool,
    Int4,
    Float4,
    Sampler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamClass {
    Scalar,
    Vector,
    MatrixRows,
    MatrixColumns,
    Object,
    Struct,
}

/// One D3DXSHADER_CONSTANTINFO entry.
#[derive(Debug, Clone)]
pub struct Constant {
    pub name: String,
    pub set: RegisterSet,
    pub register: u16,
    pub count: u16,
    pub class: ParamClass,
    /// D3DXPARAMETER_TYPE (1 bool, 2 int, 3 float, 10-14 samplers).
    pub ty: u16,
    pub rows: u16,
    pub columns: u16,
    pub elements: u16,
    /// Default value (raw big-endian words), if the table carries one.
    pub default: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VertexElement {
    /// Address of the vfetch instruction that reads this element.
    pub address: u32,
    pub usage: u8,
    pub usage_index: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interpolator {
    pub usage: u8,
    pub usage_index: u8,
    pub reg: u8,
}

#[derive(Debug, Clone)]
pub struct ShaderBlob {
    pub stage: Stage,
    /// Offset of the container inside its file (for diagnostics).
    pub file_offset: usize,
    pub constants: Vec<Constant>,
    /// Literal float constants (`def c#`): register (stage-local), value.
    pub float_defs: Vec<(u16, [f32; 4])>,
    /// Literal integer constants: register, (count, start, step, ?) as signed bytes x,y,z,w.
    pub int_defs: Vec<(u16, [i8; 4])>,
    pub vertex_elements: Vec<VertexElement>,
    pub interpolators: Vec<Interpolator>,
    /// PS: register that receives the pixel position (vPos), if any.
    pub vpos_register: Option<u8>,
    /// PS: output mask (bit 0-3 = oC0-3, bit 4 = oDepth).
    pub ps_outputs: u32,
    /// Microcode dwords (big-endian decoded).
    pub microcode: Vec<u32>,
    pub target: String,
}

fn be32(d: &[u8], o: usize) -> Result<u32, Error> {
    d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("shader container"))
}
fn be16(d: &[u8], o: usize) -> Result<u16, Error> {
    d.get(o..o + 2).map(|b| u16::from_be_bytes(b.try_into().unwrap())).ok_or(Error::Truncated("shader container"))
}
fn cstr(d: &[u8], o: usize) -> String {
    let s = d.get(o..).unwrap_or(&[]);
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).into_owned()
}

impl ShaderBlob {
    /// Parse a container starting at `d[0]` (`d` may run past its end).
    pub fn parse(d: &[u8], file_offset: usize) -> Result<Self, Error> {
        let magic = be32(d, 0)?;
        let stage = match magic {
            MAGIC_VS => Stage::Vertex,
            MAGIC_PS => Stage::Pixel,
            _ => return Err(Error::BadMagic("shader container magic")),
        };
        let virtual_size = be32(d, 4)? as usize;
        let physical_size = be32(d, 8)? as usize;
        let ctab_off = be32(d, 0x10)? as usize;
        let defs_off = be32(d, 0x14)? as usize;
        let shader_off = be32(d, 0x18)? as usize;

        // Constant table: u32 size, then D3DXSHADER_CONSTANTTABLE; offsets relative to the table.
        let t = ctab_off + 4;
        let n_const = be32(d, t + 12)? as usize;
        let info_off = be32(d, t + 16)? as usize;
        let target = cstr(d, t + be32(d, t + 24)? as usize);
        let mut constants = Vec::with_capacity(n_const);
        for i in 0..n_const {
            let c = t + info_off + i * 20;
            let name = cstr(d, t + be32(d, c)? as usize);
            let set = match be16(d, c + 4)? {
                0 => RegisterSet::Bool,
                1 => RegisterSet::Int4,
                2 => RegisterSet::Float4,
                _ => RegisterSet::Sampler,
            };
            let register = be16(d, c + 6)?;
            let count = be16(d, c + 8)?;
            let ti = t + be32(d, c + 12)? as usize;
            let default_off = be32(d, c + 16)? as usize;
            let class = match be16(d, ti)? {
                0 => ParamClass::Scalar,
                1 => ParamClass::Vector,
                2 => ParamClass::MatrixRows,
                3 => ParamClass::MatrixColumns,
                4 => ParamClass::Object,
                _ => ParamClass::Struct,
            };
            let ty = be16(d, ti + 2)?;
            let rows = be16(d, ti + 4)?;
            let columns = be16(d, ti + 6)?;
            let elements = be16(d, ti + 8)?;
            let default = if default_off != 0 && set != RegisterSet::Sampler {
                let words = count as usize * 4;
                Some((0..words).map(|k| be32(d, t + default_off + k * 4)).collect::<Result<Vec<_>, _>>()?)
            } else {
                None
            };
            constants.push(Constant { name, set, register, count, class, ty, rows, columns, elements, default });
        }

        // Program header.
        let s = shader_off;
        let physical_offset = be32(d, s)? as usize;
        let code_size = be32(d, s + 4)? as usize;
        let field_c = be32(d, s + 0xC)?;
        let interp_count = ((be32(d, s + 0x14)? >> 5) & 0x1F) as usize;
        let mut vertex_elements = Vec::new();
        let mut interpolators = Vec::new();
        let mut vpos_register = None;
        let mut ps_outputs = 0;
        let interp = |v: u32| Interpolator {
            usage_index: (v & 0xF) as u8,
            usage: ((v >> 4) & 0xF) as u8,
            reg: ((v >> 8) & 0xF) as u8,
        };
        match stage {
            Stage::Vertex => {
                let n_pre = be32(d, s + 0x18)? as usize;
                let n_el = be32(d, s + 0x1C)? as usize;
                let list = s + 0x24;
                for i in 0..n_el {
                    let v = be32(d, list + (n_pre + i) * 4)?;
                    vertex_elements.push(VertexElement {
                        address: v & 0xFFF,
                        usage: ((v >> 12) & 0xF) as u8,
                        usage_index: ((v >> 16) & 0xF) as u8,
                    });
                }
                for i in 0..interp_count {
                    interpolators.push(interp(be32(d, list + (n_pre + n_el + i) * 4)?));
                }
            }
            Stage::Pixel => {
                ps_outputs = be32(d, s + 0x1C)?;
                for i in 0..interp_count {
                    interpolators.push(interp(be32(d, s + 0x20 + i * 4)?));
                }
                let r = ((field_c >> 8) & 0xFF) as u8;
                // 0xFF (or out of range) means the shader doesn't read vPos.
                if r < 64 {
                    vpos_register = Some(r);
                }
            }
        }

        let code_start = virtual_size + physical_offset;
        if physical_offset + code_size > physical_size || code_size % 4 != 0 {
            return Err(Error::Truncated("shader microcode"));
        }
        let microcode = (0..code_size / 4).map(|i| be32(d, code_start + i * 4)).collect::<Result<Vec<_>, _>>()?;

        // Literal definitions: float4 records {u16 reg, u16 count(components), u32 physical offset}
        // until 0, then int4 records {u16 reg, u16 count, u32 values[count]} until 0.
        let mut float_defs = Vec::new();
        let mut int_defs = Vec::new();
        if defs_off != 0 {
            let mut p = defs_off + 0x14;
            loop {
                let w = be32(d, p)?;
                if w == 0 {
                    p += 4;
                    break;
                }
                let reg = (w >> 16) as u16;
                let count = (w & 0xFFFF) as usize;
                let off = be32(d, p + 4)? as usize;
                for k in 0..count.div_ceil(4) {
                    let mut v = [0f32; 4];
                    for (j, x) in v.iter_mut().enumerate() {
                        *x = f32::from_bits(be32(d, virtual_size + off + (k * 4 + j) * 4)?);
                    }
                    // Pixel shader registers are stored with the PS bank base (256).
                    let local = if stage == Stage::Pixel { reg.wrapping_sub(256) } else { reg } + k as u16;
                    float_defs.push((local, v));
                }
                p += 8;
            }
            loop {
                let w = be32(d, p)?;
                if w == 0 {
                    break;
                }
                let reg = (w >> 16) as u16;
                let count = (w & 0xFFFF) as usize;
                for k in 0..count {
                    let v = be32(d, p + 4 + k * 4)?;
                    // Integer constants live at GPU register 8992 + 4 * index.
                    let idx = (reg.wrapping_sub(8992)) / 4 + k as u16;
                    int_defs.push((idx, [v as i8, (v >> 8) as i8, (v >> 16) as i8, (v >> 24) as i8]));
                }
                p += 4 + count * 4;
            }
        }

        Ok(Self {
            stage,
            file_offset,
            constants,
            float_defs,
            int_defs,
            vertex_elements,
            interpolators,
            vpos_register,
            ps_outputs,
            microcode,
            target,
        })
    }

    /// Total container length in bytes.
    pub fn container_len(d: &[u8]) -> Result<usize, Error> {
        Ok(be32(d, 4)? as usize + be32(d, 8)? as usize)
    }

    pub fn constant(&self, name: &str) -> Option<&Constant> {
        self.constants.iter().find(|c| c.name == name)
    }
}
