//! Xenos (Xbox 360 GPU) shader microcode decoding.
//!
//! Bit layouts follow Xenia's `src/xenia/gpu/ucode.h` (BSD-3), used as the ISA reference.
//! Control-flow instructions are 48 bits, packed two per three dwords at the start of the
//! program; ALU and fetch instructions are three dwords each, addressed in 3-dword units
//! from the start of the microcode.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfOp {
    Nop,
    Exec { end: bool },
    /// Execute if bool constant `b[bool_addr] == condition`.
    CondExec { end: bool, bool_addr: u8, condition: bool },
    /// Execute if `p0 == condition`.
    CondExecPred { end: bool, condition: bool },
    LoopStart { loop_id: u8, skip_to: u16, repeat: bool },
    LoopEnd { loop_id: u8, body: u16, predicated_break: bool, condition: bool },
    CondCall { target: u16, unconditional: bool, predicated: bool, bool_addr: u8, condition: bool },
    Return,
    CondJmp { target: u16, unconditional: bool, predicated: bool, bool_addr: u8, condition: bool },
    Alloc { kind: u8, size: u8 },
    MarkVsFetchDone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cf {
    pub op: CfOp,
    /// For exec variants: first instruction address, count, sequence bits (2 per instruction,
    /// bit 0 = fetch).
    pub address: u16,
    pub count: u8,
    pub sequence: u16,
    /// a0-relative addressing (1) vs aL-relative (0) for relative operands inside the exec.
    pub absolute_addressing: bool,
}

impl Cf {
    pub fn is_exec(&self) -> bool {
        matches!(self.op, CfOp::Exec { .. } | CfOp::CondExec { .. } | CfOp::CondExecPred { .. })
    }
    pub fn ends(&self) -> bool {
        matches!(
            self.op,
            CfOp::Exec { end: true } | CfOp::CondExec { end: true, .. } | CfOp::CondExecPred { end: true, .. }
        )
    }
}

fn decode_cf(lo: u32, hi: u32) -> Cf {
    // lo = bits 0-31, hi = bits 32-47.
    let opcode = (hi >> 12) & 0xF;
    let absolute_addressing = (hi >> 11) & 1 == 1;
    let exec_fields = || ((lo & 0xFFF) as u16, ((lo >> 12) & 7) as u8, ((lo >> 16) & 0xFFF) as u16);
    let (mut address, mut count, mut sequence) = (0, 0, 0);
    let op = match opcode {
        0 => CfOp::Nop,
        1 | 2 => {
            (address, count, sequence) = exec_fields();
            CfOp::Exec { end: opcode == 2 }
        }
        3 | 4 | 13 | 14 => {
            (address, count, sequence) = exec_fields();
            CfOp::CondExec {
                end: opcode == 4 || opcode == 14,
                bool_addr: ((hi >> 2) & 0xFF) as u8,
                condition: (hi >> 10) & 1 == 1,
            }
        }
        5 | 6 => {
            (address, count, sequence) = exec_fields();
            CfOp::CondExecPred { end: opcode == 6, condition: (hi >> 10) & 1 == 1 }
        }
        7 => CfOp::LoopStart {
            skip_to: (lo & 0x1FFF) as u16,
            repeat: (lo >> 13) & 1 == 1,
            loop_id: ((lo >> 16) & 0x1F) as u8,
        },
        8 => CfOp::LoopEnd {
            body: (lo & 0x1FFF) as u16,
            loop_id: ((lo >> 16) & 0x1F) as u8,
            predicated_break: (lo >> 21) & 1 == 1,
            condition: (hi >> 10) & 1 == 1,
        },
        9 => CfOp::CondCall {
            target: (lo & 0x1FFF) as u16,
            unconditional: (lo >> 13) & 1 == 1,
            predicated: (lo >> 14) & 1 == 1,
            bool_addr: ((hi >> 2) & 0xFF) as u8,
            condition: (hi >> 10) & 1 == 1,
        },
        10 => CfOp::Return,
        11 => CfOp::CondJmp {
            target: (lo & 0x1FFF) as u16,
            unconditional: (lo >> 13) & 1 == 1,
            predicated: (lo >> 14) & 1 == 1,
            bool_addr: ((hi >> 2) & 0xFF) as u8,
            condition: (hi >> 10) & 1 == 1,
        },
        12 => CfOp::Alloc { size: (lo & 7) as u8, kind: ((hi >> 9) & 3) as u8 },
        _ => CfOp::MarkVsFetchDone,
    };
    Cf { op, address, count, sequence, absolute_addressing }
}

/// Decode the control-flow program. It runs until the first exec target (instructions start
/// there), and is cut at the end of the microcode.
pub fn control_flow(code: &[u32]) -> Vec<Cf> {
    let mut out = Vec::new();
    let mut limit_dwords = code.len();
    let mut i = 0;
    while i + 3 <= code.len() && i < limit_dwords {
        let (d0, d1, d2) = (code[i], code[i + 1], code[i + 2]);
        for (lo, hi) in [(d0, d1 & 0xFFFF), ((d1 >> 16) | (d2 << 16), d2 >> 16)] {
            let cf = decode_cf(lo, hi);
            if cf.is_exec() && cf.count > 0 {
                limit_dwords = limit_dwords.min(cf.address as usize * 3);
            }
            out.push(cf);
        }
        i += 3;
    }
    out
}

// ---------------------------------------------------------------- ALU

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alu {
    pub vector_dest: u8,
    pub vector_dest_rel: bool,
    pub abs_constants: bool,
    pub scalar_dest: u8,
    pub scalar_dest_rel: bool,
    pub export: bool,
    pub vector_mask: u8,
    pub scalar_mask: u8,
    pub vector_clamp: bool,
    pub scalar_clamp: bool,
    pub scalar_op: u8,
    pub src_swiz: [u8; 3],
    pub src_neg: [bool; 3],
    pub pred_condition: bool,
    pub predicated: bool,
    pub const_a0_relative: bool,
    pub const_1_rel: bool,
    pub const_0_rel: bool,
    pub src_reg: [u8; 3],
    pub vector_op: u8,
    /// true = temporary register, false = float constant.
    pub src_is_temp: [bool; 3],
}

pub fn decode_alu(w: [u32; 3]) -> Alu {
    let [a, b, c] = w;
    Alu {
        vector_dest: (a & 0x3F) as u8,
        vector_dest_rel: (a >> 6) & 1 == 1,
        abs_constants: (a >> 7) & 1 == 1,
        scalar_dest: ((a >> 8) & 0x3F) as u8,
        scalar_dest_rel: (a >> 14) & 1 == 1,
        export: (a >> 15) & 1 == 1,
        vector_mask: ((a >> 16) & 0xF) as u8,
        scalar_mask: ((a >> 20) & 0xF) as u8,
        vector_clamp: (a >> 24) & 1 == 1,
        scalar_clamp: (a >> 25) & 1 == 1,
        scalar_op: ((a >> 26) & 0x3F) as u8,
        // src3, src2, src1 swizzles from the low bits up; store as [src1, src2, src3].
        src_swiz: [((b >> 16) & 0xFF) as u8, ((b >> 8) & 0xFF) as u8, (b & 0xFF) as u8],
        src_neg: [(b >> 26) & 1 == 1, (b >> 25) & 1 == 1, (b >> 24) & 1 == 1],
        pred_condition: (b >> 27) & 1 == 1,
        predicated: (b >> 28) & 1 == 1,
        const_a0_relative: (b >> 29) & 1 == 1,
        const_1_rel: (b >> 30) & 1 == 1,
        const_0_rel: (b >> 31) & 1 == 1,
        src_reg: [((c >> 16) & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, (c & 0xFF) as u8],
        vector_op: ((c >> 24) & 0x1F) as u8,
        src_is_temp: [(c >> 31) & 1 == 1, (c >> 30) & 1 == 1, (c >> 29) & 1 == 1],
    }
}

impl Alu {
    /// Whether constant operand `i` (0-based) uses relative addressing.
    pub fn const_is_addressed(&self, i: usize) -> bool {
        match i {
            0 => self.const_0_rel,
            1 => {
                if self.src_is_temp[0] {
                    self.const_0_rel
                } else {
                    self.const_1_rel
                }
            }
            _ => {
                if self.src_is_temp[0] && self.src_is_temp[1] {
                    self.const_0_rel
                } else {
                    self.const_1_rel
                }
            }
        }
    }
    /// Temp register for the `mulsc/addsc/subsc` right-hand operand.
    pub fn scalar_const_temp_reg(&self) -> u8 {
        (self.scalar_op & 1) | ((self.src_is_temp[2] as u8) << 1) | (self.src_swiz[2] & 0x3C)
    }
    pub fn vector_result_mask(&self) -> u8 {
        if self.export {
            self.vector_mask & !self.scalar_mask
        } else {
            self.vector_mask
        }
    }
    pub fn scalar_result_mask(&self) -> u8 {
        if self.export {
            self.scalar_mask & !self.vector_mask
        } else {
            self.scalar_mask
        }
    }
    /// Export components forced to 0.
    pub fn const0_mask(&self) -> u8 {
        if self.export && self.scalar_dest_rel {
            0xF & !(self.vector_mask | self.scalar_mask)
        } else {
            0
        }
    }
    /// Export components forced to 1.
    pub fn const1_mask(&self) -> u8 {
        if self.export {
            self.vector_mask & self.scalar_mask
        } else {
            0
        }
    }
}

/// Absolute source component for ALU swizzle `swiz` and destination component `i`
/// (ALU swizzles are relative to the component).
pub fn alu_swizzle(swiz: u8, i: u32) -> u32 {
    ((swiz as u32 >> (2 * i)) + i) & 3
}

pub const VECTOR_OP_NAMES: [&str; 32] = [
    "add", "mul", "max", "min", "seq", "sgt", "sge", "sne", "frc", "trunc", "floor", "mad", "cndeq", "cndge", "cndgt", "dp4",
    "dp3", "dp2add", "cube", "max4", "setp_eq_push", "setp_ne_push", "setp_gt_push", "setp_ge_push", "kill_eq", "kill_gt",
    "kill_ge", "kill_ne", "dst", "maxa", "opcode_30", "opcode_31",
];

/// Number of vector operands per opcode.
pub const VECTOR_OP_ARGS: [u8; 32] =
    [2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 3, 3, 3, 3, 2, 2, 3, 2, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0];

pub const SCALAR_OP_NAMES: [&str; 64] = [
    "adds", "adds_prev", "muls", "muls_prev", "muls_prev2", "maxs", "mins", "seqs", "sgts", "sges", "snes", "frcs", "truncs",
    "floors", "exp", "logc", "log", "rcpc", "rcpf", "rcp", "rsqc", "rsqf", "rsq", "maxas", "maxasf", "subs", "subs_prev",
    "setp_eq", "setp_ne", "setp_gt", "setp_ge", "setp_inv", "setp_pop", "setp_clr", "setp_rstr", "kills_eq", "kills_gt",
    "kills_ge", "kills_ne", "kills_one", "sqrt", "opcode_41", "mulsc", "mulsc", "addsc", "addsc", "subsc", "subsc", "sin",
    "cos", "retain_prev", "opcode_51", "opcode_52", "opcode_53", "opcode_54", "opcode_55", "opcode_56", "opcode_57",
    "opcode_58", "opcode_59", "opcode_60", "opcode_61", "opcode_62", "opcode_63",
];

/// Scalar operand shape: 0 = none, 1 = `a` only, 2 = `a` and `b`, 3 = constant.a + temp.b.
pub fn scalar_op_shape(op: u8) -> u8 {
    match op {
        0 | 2 | 4 | 5 | 6 | 23 | 24 | 25 => 2,
        33 | 50 => 0,
        42..=47 => 3,
        41 | 51.. => 0,
        _ => 1,
    }
}

// ---------------------------------------------------------------- fetch

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VertexFetch {
    pub src_reg: u8,
    pub src_rel: bool,
    pub dst_reg: u8,
    pub dst_rel: bool,
    /// vf# index (0-95).
    pub const_index: u8,
    pub src_swiz: u8,
    pub dst_swiz: u16,
    pub signed: bool,
    pub normalized: bool,
    pub index_rounded: bool,
    pub format: u8,
    pub exp_adjust: i8,
    pub mini: bool,
    pub predicated: bool,
    pub pred_condition: bool,
    /// Stride and offset in dwords.
    pub stride: u8,
    pub offset: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextureFetch {
    pub opcode: u8,
    pub src_reg: u8,
    pub src_rel: bool,
    pub dst_reg: u8,
    pub dst_rel: bool,
    pub const_index: u8,
    pub unnormalized: bool,
    pub src_swiz: u8,
    pub dst_swiz: u16,
    pub mag_filter: u8,
    pub min_filter: u8,
    pub mip_filter: u8,
    pub aniso_filter: u8,
    pub use_computed_lod: bool,
    pub use_register_lod: bool,
    pub predicated: bool,
    pub use_register_gradients: bool,
    pub lod_bias: f32,
    /// 0 = 1D, 1 = 2D, 2 = 3D/stacked, 3 = cube.
    pub dimension: u8,
    pub offset: [f32; 3],
    pub pred_condition: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fetch {
    Vertex(VertexFetch),
    Texture(TextureFetch),
}

fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

pub fn decode_fetch(w: [u32; 3]) -> Fetch {
    let [a, b, c] = w;
    let opcode = (a & 0x1F) as u8;
    if opcode == 0 {
        Fetch::Vertex(VertexFetch {
            src_reg: ((a >> 5) & 0x3F) as u8,
            src_rel: (a >> 11) & 1 == 1,
            dst_reg: ((a >> 12) & 0x3F) as u8,
            dst_rel: (a >> 18) & 1 == 1,
            const_index: (((a >> 20) & 0x1F) * 3 + ((a >> 25) & 3)) as u8,
            src_swiz: ((a >> 30) & 3) as u8,
            dst_swiz: (b & 0xFFF) as u16,
            signed: (b >> 12) & 1 == 1,
            normalized: (b >> 13) & 1 == 0,
            index_rounded: (b >> 15) & 1 == 1,
            format: ((b >> 16) & 0x3F) as u8,
            exp_adjust: sext((b >> 24) & 0x3F, 6) as i8,
            mini: (b >> 30) & 1 == 1,
            predicated: (b >> 31) & 1 == 1,
            stride: (c & 0xFF) as u8,
            offset: sext((c >> 8) & 0x7F_FFFF, 23),
            pred_condition: (c >> 31) & 1 == 1,
        })
    } else {
        Fetch::Texture(TextureFetch {
            opcode,
            src_reg: ((a >> 5) & 0x3F) as u8,
            src_rel: (a >> 11) & 1 == 1,
            dst_reg: ((a >> 12) & 0x3F) as u8,
            dst_rel: (a >> 18) & 1 == 1,
            const_index: ((a >> 20) & 0x1F) as u8,
            unnormalized: (a >> 25) & 1 == 1,
            src_swiz: ((a >> 26) & 0x3F) as u8,
            dst_swiz: (b & 0xFFF) as u16,
            mag_filter: ((b >> 12) & 3) as u8,
            min_filter: ((b >> 14) & 3) as u8,
            mip_filter: ((b >> 16) & 3) as u8,
            aniso_filter: ((b >> 18) & 7) as u8,
            use_computed_lod: (b >> 28) & 1 == 1,
            use_register_lod: (b >> 29) & 1 == 1,
            predicated: (b >> 31) & 1 == 1,
            use_register_gradients: c & 1 == 1,
            lod_bias: sext((c >> 2) & 0x7F, 7) as f32 / 16.0,
            dimension: ((c >> 14) & 3) as u8,
            offset: [
                sext((c >> 16) & 0x1F, 5) as f32 * 0.5,
                sext((c >> 21) & 0x1F, 5) as f32 * 0.5,
                sext((c >> 26) & 0x1F, 5) as f32 * 0.5,
            ],
            pred_condition: (c >> 31) & 1 == 1,
        })
    }
}

/// Vertex formats (xenos::VertexFormat).
pub mod vfmt {
    pub const F8_8_8_8: u8 = 6;
    pub const F2_10_10_10: u8 = 7;
    pub const F10_11_11: u8 = 16;
    pub const F11_11_10: u8 = 17;
    pub const F16_16: u8 = 25;
    pub const F16_16_16_16: u8 = 26;
    pub const F16_16_FLOAT: u8 = 31;
    pub const F16_16_16_16_FLOAT: u8 = 32;
    pub const F32: u8 = 33;
    pub const F32_32: u8 = 34;
    pub const F32_32_32_32: u8 = 35;
    pub const F32_FLOAT: u8 = 36;
    pub const F32_32_FLOAT: u8 = 37;
    pub const F32_32_32_32_FLOAT: u8 = 38;
    pub const F32_32_32_FLOAT: u8 = 57;

    pub fn components(f: u8) -> u32 {
        match f {
            F32 | F32_FLOAT => 1,
            F16_16 | F16_16_FLOAT | F32_32 | F32_32_FLOAT => 2,
            F10_11_11 | F11_11_10 | F32_32_32_FLOAT => 3,
            _ => 4,
        }
    }
    pub fn name(f: u8) -> &'static str {
        match f {
            F8_8_8_8 => "8_8_8_8",
            F2_10_10_10 => "2_10_10_10",
            F10_11_11 => "10_11_11",
            F11_11_10 => "11_11_10",
            F16_16 => "16_16",
            F16_16_16_16 => "16_16_16_16",
            F16_16_FLOAT => "16_16_FLOAT",
            F16_16_16_16_FLOAT => "16_16_16_16_FLOAT",
            F32 => "32",
            F32_32 => "32_32",
            F32_32_32_32 => "32_32_32_32",
            F32_FLOAT => "32_FLOAT",
            F32_32_FLOAT => "32_32_FLOAT",
            F32_32_32_32_FLOAT => "32_32_32_32_FLOAT",
            F32_32_32_FLOAT => "32_32_32_FLOAT",
            _ => "unknown",
        }
    }
}

/// One instruction slot inside an exec.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Instr {
    Alu(Alu),
    Fetch(Fetch),
}

/// The instructions executed by an exec CF (address, decoded instruction).
pub fn exec_instructions(code: &[u32], cf: &Cf) -> Vec<(u32, Instr)> {
    let mut seq = cf.sequence;
    let mut out = Vec::with_capacity(cf.count as usize);
    for k in 0..cf.count as u32 {
        let addr = cf.address as u32 + k;
        let o = addr as usize * 3;
        let Some(w) = code.get(o..o + 3) else { break };
        let w = [w[0], w[1], w[2]];
        out.push((addr, if seq & 1 == 1 { Instr::Fetch(decode_fetch(w)) } else { Instr::Alu(decode_alu(w)) }));
        seq >>= 2;
    }
    out
}
