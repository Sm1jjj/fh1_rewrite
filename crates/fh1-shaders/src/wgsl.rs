//! Xenos microcode -> WGSL translation.
//!
//! The translated shader is a single WGSL function that runs the original program instruction
//! by instruction, with the Xenos semantics spelled out (legacy `0 * x = 0` multiplies, the
//! scalar `ps` register, predicates, relative addressing, export write masks). How constants,
//! textures and vertex inputs are bound is left to a [`Resolver`] supplied by the caller, so
//! the same translation can run inside Bevy's material system or a test harness.
//!
//! Output functions:
//! - vertex: `fn <name>(vertex_index: u32) -> FxVsOut` (`pos`, `o0..o15`, `pts`)
//! - pixel: `fn <name>(i: FxPsIn) -> FxPsOut` (`i.r0..r15` = interpolator registers,
//!   `i.vpos`, `i.front_facing`; returns `c0..c3`, `depth`)
//! [`PRELUDE`] (helpers + those structs) must be included once per module.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::container::{ShaderBlob, Stage};
use crate::ucode::{self, alu_swizzle, Alu, Cf, CfOp, Fetch, Instr, TextureFetch, VertexFetch};

/// How the translated code reaches the outside world.
pub trait Resolver {
    /// WGSL `vec4<f32>` expression for float constant `reg` (stage-local register number).
    fn float_const(&mut self, stage: Stage, reg: u32) -> String;
    /// WGSL `vec4<f32>` expression for float constant `base + index` (`index` is an `i32`
    /// WGSL expression).
    fn float_const_rel(&mut self, stage: Stage, base: u32, index: &str) -> String;
    /// WGSL `bool` expression for the CF bool constant address (raw `b#` address as encoded;
    /// pixel shaders use 128-255).
    fn bool_const(&mut self, stage: Stage, addr: u32) -> String;
    /// WGSL `vec4<i32>` expression for loop constant `i#` (x = count, y = start, z = step).
    fn int_const(&mut self, stage: Stage, id: u32) -> String;
    /// WGSL texture and sampler expressions for fetch constant `tf` with the fetch dimension
    /// (0 1D, 1 2D, 2 3D, 3 cube). `None` = unbound (reads return 0).
    fn texture(&mut self, stage: Stage, tf: u32, dimension: u8) -> Option<(String, String)>;
    /// WGSL `vec4<f32>` expression for a vertex element (D3D defaults for missing components:
    /// 0, 0, 0, 1).
    fn vertex_input(&mut self, usage: u8, usage_index: u8) -> String;
    /// Post-process a texture fetch result (e.g. the Xenos gamma decode for gamma-signed
    /// textures). Default: unchanged.
    fn texture_result(&mut self, _stage: Stage, _tf: u32, _dimension: u8, value: String) -> String {
        value
    }
}

pub const PRELUDE: &str = r#"
const FX_FLT_MAX: f32 = 3.40282347e+38;

struct FxVsOut {
    pos: vec4<f32>,
    o0: vec4<f32>, o1: vec4<f32>, o2: vec4<f32>, o3: vec4<f32>,
    o4: vec4<f32>, o5: vec4<f32>, o6: vec4<f32>, o7: vec4<f32>,
    o8: vec4<f32>, o9: vec4<f32>, o10: vec4<f32>, o11: vec4<f32>,
    o12: vec4<f32>, o13: vec4<f32>, o14: vec4<f32>, o15: vec4<f32>,
    pts: vec4<f32>,
}

struct FxPsIn {
    r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>, r3: vec4<f32>,
    r4: vec4<f32>, r5: vec4<f32>, r6: vec4<f32>, r7: vec4<f32>,
    r8: vec4<f32>, r9: vec4<f32>, r10: vec4<f32>, r11: vec4<f32>,
    r12: vec4<f32>, r13: vec4<f32>, r14: vec4<f32>, r15: vec4<f32>,
    vpos: vec4<f32>,
    front_facing: bool,
}

struct FxPsOut {
    c0: vec4<f32>, c1: vec4<f32>, c2: vec4<f32>, c3: vec4<f32>,
    depth: f32,
}

// Direct3D 9 / Xenos multiply: +-0 * anything (including inf and NaN) = +0.
fn fx_mul(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    return select(a * b, vec4<f32>(0.0), (a == vec4<f32>(0.0)) | (b == vec4<f32>(0.0)));
}
fn fx_muls(a: f32, b: f32) -> f32 {
    return select(a * b, 0.0, a == 0.0 || b == 0.0);
}
fn fx_dp4(a: vec4<f32>, b: vec4<f32>) -> f32 {
    let m = fx_mul(a, b);
    return (m.x + m.y) + (m.z + m.w);
}
fn fx_dp3(a: vec4<f32>, b: vec4<f32>) -> f32 {
    let m = fx_mul(a, b);
    return (m.x + m.y) + m.z;
}
fn fx_max(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> { return select(b, a, a >= b); }
fn fx_min(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> { return select(b, a, a < b); }
fn fx_trunc(a: vec4<f32>) -> vec4<f32> { return select(-floor(-a), floor(a), a >= vec4<f32>(0.0)); }
fn fx_b(c: vec4<bool>) -> vec4<f32> { return select(vec4<f32>(0.0), vec4<f32>(1.0), c); }
fn fx_cube(s0: vec4<f32>, s1: vec4<f32>) -> vec4<f32> {
    // Operands arrive as src0.zzxy, src1.yxzz.
    let x = s1.y; let y = s1.x; let z = s0.x;
    var tc: f32; var sc: f32; var ma: f32; var id: f32;
    if (abs(z) >= abs(x) && abs(z) >= abs(y)) {
        tc = -y; sc = select(x, -x, z < 0.0); ma = 2.0 * z; id = select(4.0, 5.0, z < 0.0);
    } else if (abs(y) >= abs(x)) {
        tc = select(z, -z, y < 0.0); sc = x; ma = 2.0 * y; id = select(2.0, 3.0, y < 0.0);
    } else {
        tc = -y; sc = select(-z, z, x < 0.0); ma = 2.0 * x; id = select(0.0, 1.0, x < 0.0);
    }
    return vec4<f32>(tc, sc, ma, id);
}
fn fx_max4(a: vec4<f32>) -> f32 {
    if (a.x >= a.y && a.x >= a.z && a.x >= a.w) { return a.x; }
    if (a.y >= a.z && a.y >= a.w) { return a.y; }
    if (a.z >= a.w) { return a.z; }
    return a.w;
}
fn fx_clamp_inf(t: f32, pos: f32, neg: f32) -> f32 {
    // isinf without relying on inf constants.
    if (t > FX_FLT_MAX) { return pos; }
    if (t < -FX_FLT_MAX) { return neg; }
    return t;
}
fn fx_rcp(a: f32) -> f32 { return select(1.0 / a, 1.0, a == 1.0); }
fn fx_rsq(a: f32) -> f32 { return select(inverseSqrt(a), 1.0, a == 1.0); }
fn fx_log(a: f32) -> f32 { return select(log2(a), 0.0, a == 1.0); }
fn fx_exp(a: f32) -> f32 { return select(exp2(a), 1.0, a == 0.0); }
// Xenos piecewise-linear gamma -> linear (TextureSign::kGamma; segments as in Xenia's xenos.cc).
fn fx_pwl_degamma(g: f32) -> f32 {
    let x = clamp(g, 0.0, 1.0);
    var scale: f32; var offset: f32;
    if (x >= 96.0 / 255.0) {
        if (x >= 192.0 / 255.0) { scale = 8.0 / 1024.0; offset = -1024.0; } else { scale = 4.0 / 1024.0; offset = -256.0; }
    } else {
        if (x >= 64.0 / 255.0) { scale = 2.0 / 1024.0; offset = -64.0; } else { scale = 1.0 / 1024.0; offset = 0.0; }
    }
    var l = x * (255.0 * 1024.0 * scale) + offset;
    l = l + trunc(l * scale);
    return l / 1023.0;
}
// Cube fetch: the shaders pass (sc/|ma| + 1.5, tc/|ma| + 1.5, face) from the CUBE result (tc, sc, ma = 2·major, face)
// (lake_anim_norm_opac_refl_3 PS); rebuild the direction (s, t in -1..1 on the face).
fn fx_cube_dir(c: vec3<f32>) -> vec3<f32> {
    let s = (c.x - 1.5) * 2.0;
    let t = (c.y - 1.5) * 2.0;
    let f = i32(c.z + 0.5);
    switch f {
        case 0: { return vec3<f32>(1.0, -t, -s); }
        case 1: { return vec3<f32>(-1.0, -t, s); }
        case 2: { return vec3<f32>(s, 1.0, t); }
        case 3: { return vec3<f32>(s, -1.0, -t); }
        case 4: { return vec3<f32>(s, -t, 1.0); }
        default: { return vec3<f32>(-s, -t, -1.0); }
    }
}
"#;

#[derive(Debug, Clone, Default)]
pub struct Translation {
    pub code: String,
    /// Float constant registers read with absolute addressing (excluding literal defs).
    pub float_consts: BTreeSet<u32>,
    /// Float constant bases read with relative addressing.
    pub float_consts_rel: BTreeSet<u32>,
    pub bool_consts: BTreeSet<u32>,
    pub int_consts: BTreeSet<u32>,
    /// (tf index, dimension).
    pub textures: BTreeSet<(u32, u8)>,
    /// VS: interpolator exports written (o#).
    pub exports: BTreeSet<u32>,
    /// PS: colour targets written.
    pub color_outputs: BTreeSet<u32>,
    pub writes_depth: bool,
    pub uses_kill: bool,
    /// Used the generic pc state machine (loops/calls/unstructured jumps).
    pub state_machine: bool,
}

struct Ctx<'a, R: Resolver> {
    blob: &'a ShaderBlob,
    r: &'a mut R,
    out: Translation,
    temps_array: bool,
    indent: usize,
}

const C: [char; 4] = ['x', 'y', 'z', 'w'];

fn flit(v: f32) -> String {
    if v.is_nan() {
        return "bitcast<f32>(0x7fc00000u)".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "bitcast<f32>(0x7f800000u)".into() } else { "bitcast<f32>(0xff800000u)".into() };
    }
    let s = format!("{v:?}");
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

impl<R: Resolver> Ctx<'_, R> {
    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.code.push_str("    ");
        }
        self.out.code.push_str(s);
        self.out.code.push('\n');
    }

    fn temp(&self, n: u32, rel: bool) -> String {
        if self.temps_array {
            if rel {
                format!("r[clamp({n} + aL, 0, 63)]")
            } else {
                format!("r[{n}]")
            }
        } else {
            format!("r{n}")
        }
    }

    /// Loop constant `i#`: a literal `defi` if the shader has one, else the resolver.
    fn iconst(&mut self, id: u32) -> String {
        if let Some((_, v)) = self.blob.int_defs.iter().find(|(r, _)| *r as u32 == id) {
            return format!("vec4<i32>({}, {}, {}, {})", v[0], v[1], v[2], v[3]);
        }
        self.out.int_consts.insert(id);
        self.r.int_const(self.blob.stage, id)
    }

    fn fconst_abs(&mut self, reg: u32) -> String {
        if let Some((_, v)) = self.blob.float_defs.iter().find(|(r, _)| *r as u32 == reg) {
            return format!("vec4<f32>({}, {}, {}, {})", flit(v[0]), flit(v[1]), flit(v[2]), flit(v[3]));
        }
        self.out.float_consts.insert(reg);
        self.r.float_const(self.blob.stage, reg)
    }

    /// Raw (un-swizzled) vec4 expression of ALU source `i` (0-based), with abs applied.
    fn alu_src_base(&mut self, a: &Alu, i: usize) -> String {
        let reg = a.src_reg[i] as u32;
        if a.src_is_temp[i] {
            let t = self.temp(reg & 0x3F, reg & 0x40 != 0);
            if reg & 0x80 != 0 {
                format!("abs({t})")
            } else {
                t
            }
        } else {
            let base = if a.const_is_addressed(i) {
                let idx = if a.const_a0_relative { "a0" } else { "aL" };
                self.out.float_consts_rel.insert(reg);
                self.r.float_const_rel(self.blob.stage, reg, idx)
            } else {
                self.fconst_abs(reg)
            };
            if a.abs_constants {
                format!("abs({base})")
            } else {
                base
            }
        }
    }

    fn alu_src_vec(&mut self, a: &Alu, i: usize) -> String {
        let base = self.alu_src_base(a, i);
        let sw: String = (0..4).map(|k| C[alu_swizzle(a.src_swiz[i], k) as usize]).collect();
        let e = if sw == "xyzw" { base } else { format!("({base}).{sw}") };
        if a.src_neg[i] {
            format!("(-{e})")
        } else {
            e
        }
    }

    /// Scalar operand component `k` (3 = `a` (W), 0 = `b` (X)) of source 3.
    fn alu_src_scalar(&mut self, a: &Alu, k: u32) -> String {
        let base = self.alu_src_base(a, 2);
        let e = format!("({base}).{}", C[alu_swizzle(a.src_swiz[2], k) as usize]);
        if a.src_neg[2] {
            format!("(-{e})")
        } else {
            e
        }
    }

    fn write_dest(&mut self, dst: &str, mask: u8, value: &str, clamp: bool) {
        let v = if clamp { format!("clamp({value}, vec4<f32>(0.0), vec4<f32>(1.0))") } else { value.to_string() };
        if mask == 0 {
            return;
        }
        if mask == 0xF {
            self.line(&format!("{dst} = {v};"));
            return;
        }
        self.line(&format!("{{ let w = {v};"));
        for (k, c) in C.iter().enumerate() {
            if mask & (1 << k) != 0 {
                self.line(&format!("    {dst}.{c} = w.{c};"));
            }
        }
        self.line("}");
    }

    fn export_target(&mut self, reg: u8) -> Option<String> {
        match self.blob.stage {
            Stage::Vertex => match reg {
                0..=15 => {
                    self.out.exports.insert(reg as u32);
                    Some(format!("out.o{reg}"))
                }
                62 => Some("out.pos".into()),
                63 => Some("out.pts".into()),
                _ => None,
            },
            Stage::Pixel => match reg {
                0..=3 => {
                    self.out.color_outputs.insert(reg as u32);
                    Some(format!("out.c{reg}"))
                }
                61 => {
                    self.out.writes_depth = true;
                    Some("depth_tmp".into())
                }
                _ => None,
            },
        }
    }

    fn alu(&mut self, a: &Alu) {
        let vop = a.vector_op;
        let sop = a.scalar_op;
        if a.predicated {
            self.line(&format!("if (p0 == {}) {{", a.pred_condition));
            self.indent += 1;
        }
        self.line("{");
        self.indent += 1;

        // Read every operand before any write (co-issued vector + scalar ops).
        let nargs = ucode::VECTOR_OP_ARGS[vop as usize] as usize;
        let mut vargs = Vec::new();
        for i in 0..nargs {
            let e = self.alu_src_vec(a, i);
            self.line(&format!("let s{i} = {e};"));
            vargs.push(format!("s{i}"));
        }
        let shape = ucode::scalar_op_shape(sop);
        match shape {
            1 => {
                let e = self.alu_src_scalar(a, 3);
                self.line(&format!("let sa = {e};"));
            }
            2 => {
                let ea = self.alu_src_scalar(a, 3);
                let eb = self.alu_src_scalar(a, 0);
                self.line(&format!("let sa = {ea};"));
                self.line(&format!("let sb = {eb};"));
            }
            3 => {
                let reg = a.src_reg[2] as u32;
                let cbase = if a.const_is_addressed(2) {
                    let idx = if a.const_a0_relative { "a0" } else { "aL" };
                    self.out.float_consts_rel.insert(reg);
                    self.r.float_const_rel(self.blob.stage, reg, idx)
                } else {
                    self.fconst_abs(reg)
                };
                let t = self.temp(a.scalar_const_temp_reg() as u32, false);
                let (ca, tb) = (C[alu_swizzle(a.src_swiz[2], 3) as usize], C[alu_swizzle(a.src_swiz[2], 0) as usize]);
                let wrap = |e: String| {
                    let e = if a.abs_constants { format!("abs({e})") } else { e };
                    if a.src_neg[2] {
                        format!("(-{e})")
                    } else {
                        e
                    }
                };
                let ea = wrap(format!("({cbase}).{ca}"));
                let eb = wrap(format!("{t}.{tb}"));
                self.line(&format!("let sa = {ea};"));
                self.line(&format!("let sb = {eb};"));
            }
            _ => {}
        }

        // Vector operation.
        let v = |i: usize| vargs[i].clone();
        let vexpr: Option<String> = match vop {
            0 => Some(format!("{} + {}", v(0), v(1))),
            1 => Some(format!("fx_mul({}, {})", v(0), v(1))),
            2 => Some(format!("fx_max({}, {})", v(0), v(1))),
            3 => Some(format!("fx_min({}, {})", v(0), v(1))),
            4 => Some(format!("fx_b({} == {})", v(0), v(1))),
            5 => Some(format!("fx_b({} > {})", v(0), v(1))),
            6 => Some(format!("fx_b({} >= {})", v(0), v(1))),
            7 => Some(format!("fx_b({} != {})", v(0), v(1))),
            8 => Some(format!("{0} - floor({0})", v(0))),
            9 => Some(format!("fx_trunc({})", v(0))),
            10 => Some(format!("floor({})", v(0))),
            11 => Some(format!("fx_mul({}, {}) + {}", v(0), v(1), v(2))),
            12 => Some(format!("select({2}, {1}, {0} == vec4<f32>(0.0))", v(0), v(1), v(2))),
            13 => Some(format!("select({2}, {1}, {0} >= vec4<f32>(0.0))", v(0), v(1), v(2))),
            14 => Some(format!("select({2}, {1}, {0} > vec4<f32>(0.0))", v(0), v(1), v(2))),
            15 => Some(format!("vec4<f32>(fx_dp4({}, {}))", v(0), v(1))),
            16 => Some(format!("vec4<f32>(fx_dp3({}, {}))", v(0), v(1))),
            17 => Some(format!("vec4<f32>(fx_muls({0}.x, {1}.x) + fx_muls({0}.y, {1}.y) + {2}.x)", v(0), v(1), v(2))),
            18 => Some(format!("fx_cube({}, {})", v(0), v(1))),
            19 => Some(format!("vec4<f32>(fx_max4({}))", v(0))),
            20..=23 => {
                let cmp = ["==", "!=", ">", ">="][(vop - 20) as usize];
                self.line(&format!("let pv = {0}.w == 0.0 && {1}.w {cmp} 0.0;", v(0), v(1)));
                Some(format!("select(vec4<f32>({0}.x + 1.0), vec4<f32>(0.0), {0}.x == 0.0 && {1}.x {cmp} 0.0)", v(0), v(1)))
            }
            24..=27 => {
                let cmp = ["==", ">", ">=", "!="][(vop - 24) as usize];
                self.line(&format!("let kv = any({} {cmp} {});", v(0), v(1)));
                Some("select(vec4<f32>(0.0), vec4<f32>(1.0), kv)".to_string())
            }
            28 => Some(format!("vec4<f32>(1.0, fx_muls({0}.y, {1}.y), {0}.z, {1}.w)", v(0), v(1))),
            29 => Some(format!("fx_max({}, {})", v(0), v(1))),
            _ => None,
        };
        if let Some(e) = &vexpr {
            self.line(&format!("let vr = {e};"));
        }

        // Scalar operation.
        let sexpr: Option<String> = match sop {
            0 => Some("sa + sb".into()),
            1 => Some("sa + ps".into()),
            2 => Some("fx_muls(sa, sb)".into()),
            3 => Some("fx_muls(sa, ps)".into()),
            4 => Some("select(fx_muls(sa, ps), -FX_FLT_MAX, ps == -FX_FLT_MAX || !(abs(ps) <= FX_FLT_MAX) || !(abs(sb) <= FX_FLT_MAX) || sb <= 0.0)".into()),
            5 | 23 | 24 => Some("select(sb, sa, sa >= sb)".into()),
            6 => Some("select(sb, sa, sa < sb)".into()),
            7 => Some("select(0.0, 1.0, sa == 0.0)".into()),
            8 => Some("select(0.0, 1.0, sa > 0.0)".into()),
            9 => Some("select(0.0, 1.0, sa >= 0.0)".into()),
            10 => Some("select(0.0, 1.0, sa != 0.0)".into()),
            11 => Some("sa - floor(sa)".into()),
            12 => Some("select(-floor(-sa), floor(sa), sa >= 0.0)".into()),
            13 => Some("floor(sa)".into()),
            14 => Some("fx_exp(sa)".into()),
            15 => Some("fx_clamp_inf(fx_log(sa), FX_FLT_MAX, -FX_FLT_MAX)".into()),
            16 => Some("fx_log(sa)".into()),
            17 => Some("fx_clamp_inf(fx_rcp(sa), FX_FLT_MAX, -FX_FLT_MAX)".into()),
            18 => Some("fx_clamp_inf(fx_rcp(sa), 0.0, -0.0)".into()),
            19 => Some("fx_rcp(sa)".into()),
            20 => Some("fx_clamp_inf(fx_rsq(sa), FX_FLT_MAX, -FX_FLT_MAX)".into()),
            21 => Some("fx_clamp_inf(fx_rsq(sa), 0.0, -0.0)".into()),
            22 => Some("fx_rsq(sa)".into()),
            25 => Some("sa - sb".into()),
            26 => Some("sa - ps".into()),
            27 => Some("select(1.0, 0.0, sa == 0.0)".into()),
            28 => Some("select(1.0, 0.0, sa != 0.0)".into()),
            29 => Some("select(1.0, 0.0, sa > 0.0)".into()),
            30 => Some("select(1.0, 0.0, sa >= 0.0)".into()),
            31 => Some("select(select(sa, 1.0, sa == 0.0), 0.0, sa == 1.0)".into()),
            32 => Some("select(sa - 1.0, 0.0, sa - 1.0 <= 0.0)".into()),
            33 => Some("FX_FLT_MAX".into()),
            34 => Some("select(sa, 0.0, sa == 0.0)".into()),
            35 => Some("select(0.0, 1.0, sa == 0.0)".into()),
            36 => Some("select(0.0, 1.0, sa > 0.0)".into()),
            37 => Some("select(0.0, 1.0, sa >= 0.0)".into()),
            38 => Some("select(0.0, 1.0, sa != 0.0)".into()),
            39 => Some("select(0.0, 1.0, sa == 1.0)".into()),
            40 => Some("sqrt(sa)".into()),
            42 | 43 => Some("fx_muls(sa, sb)".into()),
            44 | 45 => Some("sa + sb".into()),
            46 | 47 => Some("sa - sb".into()),
            48 => Some("sin(sa)".into()),
            49 => Some("cos(sa)".into()),
            50 => Some("ps".into()),
            _ => None,
        };
        let sr = sexpr.clone().unwrap_or_else(|| "ps".into());
        self.line(&format!("let sr = {sr};"));

        // Side effects that don't depend on write masks.
        match vop {
            20..=23 => self.line("p0 = pv;"),
            24..=27 => {
                if self.blob.stage == Stage::Pixel {
                    self.out.uses_kill = true;
                    self.line("if (kv) { discard; }");
                }
            }
            29 => {
                let s0 = v(0);
                self.line(&format!("a0 = i32(clamp(floor({s0}.w + 0.5), -256.0, 255.0));"));
            }
            _ => {}
        }
        match sop {
            23 => self.line("a0 = i32(clamp(floor(sa + 0.5), -256.0, 255.0));"),
            24 => self.line("a0 = i32(clamp(floor(sa), -256.0, 255.0));"),
            27 => self.line("p0 = sa == 0.0;"),
            28 => self.line("p0 = sa != 0.0;"),
            29 => self.line("p0 = sa > 0.0;"),
            30 => self.line("p0 = sa >= 0.0;"),
            31 => self.line("p0 = sa == 1.0;"),
            32 => self.line("p0 = sa - 1.0 <= 0.0;"),
            33 => self.line("p0 = false;"),
            34 => self.line("p0 = sa == 0.0;"),
            35..=39 => {
                if self.blob.stage == Stage::Pixel {
                    self.out.uses_kill = true;
                    let cond = ["sa == 0.0", "sa > 0.0", "sa >= 0.0", "sa != 0.0", "sa == 1.0"][(sop - 35) as usize];
                    self.line(&format!("if ({cond}) {{ discard; }}"));
                }
            }
            _ => {}
        }

        // Writes.
        let vmask = a.vector_result_mask();
        let smask = a.scalar_result_mask();
        if a.export {
            if let Some(dst) = self.export_target(a.vector_dest) {
                let c0 = a.const0_mask();
                let c1 = a.const1_mask();
                if let Some(e) = &vexpr {
                    let _ = e;
                    self.write_dest(&dst, vmask, "vr", a.vector_clamp);
                }
                self.write_dest(&dst, smask, "vec4<f32>(sr)", a.scalar_clamp);
                self.write_dest(&dst, c0, "vec4<f32>(0.0)", false);
                self.write_dest(&dst, c1, "vec4<f32>(1.0)", false);
            }
        } else {
            if vexpr.is_some() && vmask != 0 {
                let dst = self.temp(a.vector_dest as u32, a.vector_dest_rel);
                self.write_dest(&dst, vmask, "vr", a.vector_clamp);
            }
            if smask != 0 {
                let dst = self.temp(a.scalar_dest as u32, a.scalar_dest_rel);
                self.write_dest(&dst, smask, "vec4<f32>(sr)", a.scalar_clamp);
            }
        }
        // Every scalar op except retain_prev updates ps (clamped value, as written).
        if sop != 50 && sexpr.is_some() {
            if a.scalar_clamp {
                self.line("ps = clamp(sr, 0.0, 1.0);");
            } else {
                self.line("ps = sr;");
            }
        }

        self.indent -= 1;
        self.line("}");
        if a.predicated {
            self.indent -= 1;
            self.line("}");
        }
    }

    fn fetch_dst(&mut self, dst: u8, rel: bool, swiz: u16, value: &str) {
        let d = self.temp(dst as u32, rel);
        self.line("{");
        self.line(&format!("    let f = {value};"));
        for (k, c) in C.iter().enumerate() {
            let s = (swiz >> (3 * k)) & 7;
            let e = match s {
                0..=3 => format!("f.{}", C[s as usize]),
                4 => "0.0".into(),
                5 => "1.0".into(),
                7 => continue,
                _ => "0.0".into(),
            };
            self.line(&format!("    {d}.{c} = {e};"));
        }
        self.line("}");
    }

    fn vfetch(&mut self, addr: u32, v: &VertexFetch) {
        let el = self.blob.vertex_elements.iter().find(|e| e.address == addr).copied();
        let value = match el {
            Some(e) => self.r.vertex_input(e.usage, e.usage_index),
            None => "vec4<f32>(0.0, 0.0, 0.0, 1.0)".into(),
        };
        if v.predicated {
            self.line(&format!("if (p0 == {}) {{", v.pred_condition));
            self.indent += 1;
        }
        self.fetch_dst(v.dst_reg, v.dst_rel, v.dst_swiz, &value);
        if v.predicated {
            self.indent -= 1;
            self.line("}");
        }
    }

    fn tfetch(&mut self, t: &TextureFetch) {
        if t.predicated {
            self.line(&format!("if (p0 == {}) {{", t.pred_condition));
            self.indent += 1;
        }
        let src = self.temp(t.src_reg as u32, t.src_rel);
        let sw: String = (0..3).map(|i| C[((t.src_swiz >> (2 * i)) & 3) as usize]).collect();
        match t.opcode {
            1 => {
                self.out.textures.insert((t.const_index as u32, t.dimension));
                let bound = self.r.texture(self.blob.stage, t.const_index as u32, t.dimension);
                let value = match bound {
                    None => "vec4<f32>(0.0)".to_string(),
                    Some((tex, samp)) => {
                        let off = t.offset;
                        let has_off = off != [0.0; 3];
                        let lod = if t.use_register_lod { Some("tex_lod".to_string()) } else { None };
                        let bias = t.lod_bias;
                        let sample = |coord: String| -> String {
                            match (&lod, self.blob.stage) {
                                (Some(l), _) => format!("textureSampleLevel({tex}, {samp}, {coord}, {l} + {})", flit(bias)),
                                (None, Stage::Vertex) => format!("textureSampleLevel({tex}, {samp}, {coord}, {})", flit(bias.max(0.0))),
                                (None, Stage::Pixel) => {
                                    if bias != 0.0 {
                                        format!("textureSampleBias({tex}, {samp}, {coord}, {})", flit(bias))
                                    } else {
                                        format!("textureSample({tex}, {samp}, {coord})")
                                    }
                                }
                            }
                        };
                        match t.dimension {
                            0 => {
                                let c = format!("{src}.{}", &sw[..1]);
                                let c = if has_off { format!("({c} + {} / f32(textureDimensions({tex}).x))", flit(off[0])) } else { c };
                                sample(format!("vec2<f32>({c}, 0.5)"))
                            }
                            1 => {
                                let c = format!("{src}.{}", &sw[..2]);
                                let c = if t.unnormalized {
                                    format!("({c} / vec2<f32>(textureDimensions({tex})))")
                                } else {
                                    c
                                };
                                let c = if has_off {
                                    format!("({c} + vec2<f32>({}, {}) / vec2<f32>(textureDimensions({tex})))", flit(off[0]), flit(off[1]))
                                } else {
                                    c
                                };
                                sample(c)
                            }
                            2 => sample(format!("{src}.{sw}")),
                            _ => sample(format!("fx_cube_dir({src}.{sw})")),
                        }
                    }
                };
                let value = self.r.texture_result(self.blob.stage, t.const_index as u32, t.dimension, value);
                self.fetch_dst(t.dst_reg, t.dst_rel, t.dst_swiz, &value);
            }
            24 => {
                self.line(&format!("tex_lod = {src}.{};", &sw[..1]));
            }
            17 => {
                // getCompTexLOD: approximate with the derivative-based LOD of a 2D texture.
                self.out.textures.insert((t.const_index as u32, t.dimension));
                let bound = self.r.texture(self.blob.stage, t.const_index as u32, t.dimension);
                let value = match bound {
                    Some((tex, _)) if self.blob.stage == Stage::Pixel => {
                        let c = format!("{src}.{}", &sw[..2]);
                        format!(
                            "vec4<f32>(0.5 * log2(max(dot(dpdx({c}) * vec2<f32>(textureDimensions({tex})), dpdx({c}) * vec2<f32>(textureDimensions({tex}))), dot(dpdy({c}) * vec2<f32>(textureDimensions({tex})), dpdy({c}) * vec2<f32>(textureDimensions({tex}))))), 0.0, 0.0, 0.0)"
                        )
                    }
                    _ => "vec4<f32>(0.0)".into(),
                };
                self.fetch_dst(t.dst_reg, t.dst_rel, t.dst_swiz, &value);
            }
            18 => {
                let value = if self.blob.stage == Stage::Pixel {
                    format!("vec4<f32>(dpdx({src}.x), dpdy({src}.x), dpdx({src}.y), dpdy({src}.y))")
                } else {
                    "vec4<f32>(0.0)".into()
                };
                self.fetch_dst(t.dst_reg, t.dst_rel, t.dst_swiz, &value);
            }
            _ => {
                // getBCF / getWeights / gradients setters: not used by FH1 shaders so far.
                self.line(&format!("// unsupported fetch opcode {}", t.opcode));
                if matches!(t.opcode, 16 | 19) {
                    self.fetch_dst(t.dst_reg, t.dst_rel, t.dst_swiz, "vec4<f32>(0.0)");
                }
            }
        }
        if t.predicated {
            self.indent -= 1;
            self.line("}");
        }
    }

    fn exec_body(&mut self, cf: &Cf) {
        for (addr, ins) in ucode::exec_instructions(&self.blob.microcode, cf) {
            match ins {
                Instr::Alu(a) => self.alu(&a),
                Instr::Fetch(Fetch::Vertex(v)) => self.vfetch(addr, &v),
                Instr::Fetch(Fetch::Texture(t)) => self.tfetch(&t),
            }
        }
    }

    fn ret(&mut self) {
        match self.blob.stage {
            Stage::Vertex => self.line("return out;"),
            Stage::Pixel => {
                self.line("out.depth = depth_tmp.x;");
                self.line("return out;");
            }
        }
    }

    fn cf_condition(&mut self, cf: &Cf) -> Option<String> {
        match cf.op {
            CfOp::CondExec { bool_addr, condition, .. } => {
                self.out.bool_consts.insert(bool_addr as u32);
                let b = self.r.bool_const(self.blob.stage, bool_addr as u32);
                Some(format!("({b}) == {condition}"))
            }
            CfOp::CondExecPred { condition, .. } => Some(format!("p0 == {condition}")),
            _ => None,
        }
    }

    fn jump_condition(&mut self, unconditional: bool, predicated: bool, bool_addr: u8, condition: bool) -> String {
        if unconditional {
            "true".into()
        } else if predicated {
            format!("p0 == {condition}")
        } else {
            self.out.bool_consts.insert(bool_addr as u32);
            let b = self.r.bool_const(self.blob.stage, bool_addr as u32);
            format!("({b}) == {condition}")
        }
    }

    /// Structured translation: execs, conditional execs and forward conditional jumps that nest.
    fn structured(&mut self, cfs: &[Cf]) -> bool {
        // Check the program is structurable.
        let mut stack: Vec<usize> = Vec::new();
        for (k, cf) in cfs.iter().enumerate() {
            while stack.last() == Some(&k) {
                stack.pop();
            }
            match cf.op {
                CfOp::LoopStart { .. } | CfOp::LoopEnd { .. } | CfOp::CondCall { .. } | CfOp::Return => return false,
                CfOp::CondJmp { target, unconditional, .. } => {
                    let t = target as usize;
                    if unconditional || t <= k || t > cfs.len() || stack.last().is_some_and(|&top| t > top) {
                        return false;
                    }
                    stack.push(t);
                }
                _ => {}
            }
        }
        let mut open: Vec<usize> = Vec::new();
        for (k, cf) in cfs.iter().enumerate() {
            while open.last() == Some(&k) {
                open.pop();
                self.indent -= 1;
                self.line("}");
            }
            match cf.op {
                CfOp::Exec { .. } | CfOp::CondExec { .. } | CfOp::CondExecPred { .. } => {
                    let cond = self.cf_condition(cf);
                    if let Some(c) = &cond {
                        self.line(&format!("if ({c}) {{"));
                        self.indent += 1;
                    }
                    self.exec_body(cf);
                    if cf.ends() {
                        self.ret();
                    }
                    if cond.is_some() {
                        self.indent -= 1;
                        self.line("}");
                    }
                    if cf.ends() && cond.is_none() {
                        break;
                    }
                }
                CfOp::CondJmp { target, unconditional, predicated, bool_addr, condition } => {
                    let c = self.jump_condition(unconditional, predicated, bool_addr, condition);
                    self.line(&format!("if (!({c})) {{"));
                    self.indent += 1;
                    open.push(target as usize);
                }
                _ => {}
            }
        }
        while open.pop().is_some() {
            self.indent -= 1;
            self.line("}");
        }
        true
    }

    /// Generic translation: a pc-driven state machine over CF instructions.
    fn state_machine(&mut self, cfs: &[Cf]) {
        self.out.state_machine = true;
        self.line("var pc: u32 = 0u;");
        self.line("var loop_sp: i32 = 0;");
        self.line("var loop_al: array<i32, 4>;");
        self.line("var loop_it: array<i32, 4>;");
        self.line("var call_sp: i32 = 0;");
        self.line("var call_ret: array<u32, 4>;");
        self.line("for (var guard: u32 = 0u; guard < 65536u; guard = guard + 1u) {");
        self.indent += 1;
        self.line("switch pc {");
        self.indent += 1;
        for (k, cf) in cfs.iter().enumerate() {
            let next = k + 1;
            self.line(&format!("case {k}u: {{"));
            self.indent += 1;
            match cf.op {
                CfOp::Exec { .. } | CfOp::CondExec { .. } | CfOp::CondExecPred { .. } => {
                    let cond = self.cf_condition(cf);
                    if let Some(c) = &cond {
                        self.line(&format!("if ({c}) {{"));
                        self.indent += 1;
                    }
                    self.exec_body(cf);
                    if cf.ends() {
                        self.ret();
                    }
                    if cond.is_some() {
                        self.indent -= 1;
                        self.line("}");
                    }
                    self.line(&format!("pc = {next}u;"));
                }
                CfOp::LoopStart { loop_id, skip_to, repeat } => {
                    let i = self.iconst(loop_id as u32);
                    self.line(&format!("let lc = {i};"));
                    self.line("if (lc.x <= 0) {");
                    self.line(&format!("    pc = {skip_to}u;"));
                    self.line("} else {");
                    self.line("    loop_al[clamp(loop_sp, 0, 3)] = aL;");
                    self.line("    loop_it[clamp(loop_sp, 0, 3)] = 0;");
                    self.line("    loop_sp = loop_sp + 1;");
                    if !repeat {
                        self.line("    aL = lc.y;");
                    }
                    self.line(&format!("    pc = {next}u;"));
                    self.line("}");
                }
                CfOp::LoopEnd { loop_id, body, predicated_break, condition } => {
                    let i = self.iconst(loop_id as u32);
                    self.line(&format!("let lc = {i};"));
                    self.line("let li = clamp(loop_sp - 1, 0, 3);");
                    self.line("loop_it[li] = loop_it[li] + 1;");
                    self.line("aL = aL + lc.z;");
                    let brk = if predicated_break { format!(" && !(p0 == {condition})") } else { String::new() };
                    self.line(&format!("if (loop_it[li] < lc.x{brk}) {{"));
                    self.line(&format!("    pc = {body}u;"));
                    self.line("} else {");
                    self.line("    aL = loop_al[li];");
                    self.line("    loop_sp = loop_sp - 1;");
                    self.line(&format!("    pc = {next}u;"));
                    self.line("}");
                }
                CfOp::CondCall { target, unconditional, predicated, bool_addr, condition } => {
                    let c = self.jump_condition(unconditional, predicated, bool_addr, condition);
                    self.line(&format!("if ({c}) {{"));
                    self.line(&format!("    call_ret[clamp(call_sp, 0, 3)] = {next}u;"));
                    self.line("    call_sp = call_sp + 1;");
                    self.line(&format!("    pc = {target}u;"));
                    self.line("} else {");
                    self.line(&format!("    pc = {next}u;"));
                    self.line("}");
                }
                CfOp::Return => {
                    self.line("if (call_sp > 0) {");
                    self.line("    call_sp = call_sp - 1;");
                    self.line("    pc = call_ret[clamp(call_sp, 0, 3)];");
                    self.line("} else {");
                    self.line(&format!("    pc = {next}u;"));
                    self.line("}");
                }
                CfOp::CondJmp { target, unconditional, predicated, bool_addr, condition } => {
                    let c = self.jump_condition(unconditional, predicated, bool_addr, condition);
                    self.line(&format!("if ({c}) {{ pc = {target}u; }} else {{ pc = {next}u; }}"));
                }
                _ => self.line(&format!("pc = {next}u;")),
            }
            self.indent -= 1;
            self.line("}");
        }
        self.line("default: {");
        self.indent += 1;
        self.ret();
        self.indent -= 1;
        self.line("}");
        self.indent -= 1;
        self.line("}");
        self.indent -= 1;
        self.line("}");
    }
}

fn uses_relative_temps(blob: &ShaderBlob, cfs: &[Cf]) -> bool {
    for cf in cfs.iter().filter(|c| c.is_exec()) {
        for (_, ins) in ucode::exec_instructions(&blob.microcode, cf) {
            let rel = match ins {
                Instr::Alu(a) => {
                    a.vector_dest_rel && !a.export
                        || a.scalar_dest_rel && !a.export
                        || (0..3).any(|i| a.src_is_temp[i] && a.src_reg[i] & 0x40 != 0)
                }
                Instr::Fetch(Fetch::Vertex(v)) => v.dst_rel || v.src_rel,
                Instr::Fetch(Fetch::Texture(t)) => t.dst_rel || t.src_rel,
            };
            if rel {
                return true;
            }
        }
    }
    false
}

/// Translate one shader into a WGSL function called `name`.
pub fn translate<R: Resolver>(blob: &ShaderBlob, name: &str, r: &mut R) -> Translation {
    let cfs = ucode::control_flow(&blob.microcode);
    let temps_array = uses_relative_temps(blob, &cfs);
    let mut ctx = Ctx { blob, r, out: Translation::default(), temps_array, indent: 0 };
    match blob.stage {
        Stage::Vertex => ctx.line(&format!("fn {name}(vertex_index: u32) -> FxVsOut {{")),
        Stage::Pixel => ctx.line(&format!("fn {name}(i: FxPsIn) -> FxPsOut {{")),
    }
    ctx.indent = 1;
    match blob.stage {
        Stage::Vertex => ctx.line("var out: FxVsOut;"),
        Stage::Pixel => {
            ctx.line("var out: FxPsOut;");
            ctx.line("var depth_tmp: vec4<f32> = vec4<f32>(i.vpos.z);");
        }
    }
    if temps_array {
        ctx.line("var r: array<vec4<f32>, 64>;");
    } else {
        for n in 0..64 {
            ctx.line(&format!("var r{n}: vec4<f32> = vec4<f32>(0.0);"));
        }
    }
    ctx.line("var p0: bool = false;");
    ctx.line("var ps: f32 = 0.0;");
    ctx.line("var a0: i32 = 0;");
    ctx.line("var aL: i32 = 0;");
    ctx.line("var tex_lod: f32 = 0.0;");
    match blob.stage {
        Stage::Vertex => {
            // The vertex index arrives in r0.x.
            let t = ctx.temp(0, false);
            ctx.line(&format!("{t}.x = f32(vertex_index);"));
        }
        Stage::Pixel => {
            let interp_regs: Vec<u8> = blob.interpolators.iter().map(|i| i.reg).collect();
            for &reg in &interp_regs {
                let t = ctx.temp(reg as u32, false);
                ctx.line(&format!("{t} = i.r{reg};"));
            }
            if let Some(v) = blob.vpos_register {
                if !interp_regs.contains(&v) {
                    let t = ctx.temp(v as u32, false);
                    // Xenos: xy = pixel position (sign of x = facing), zw = 0.
                    ctx.line(&format!("{t} = vec4<f32>((i.vpos.xy - 0.5) * vec2<f32>(select(-1.0, 1.0, i.front_facing), 1.0), 0.0, 0.0);"));
                }
            }
        }
    }
    if !ctx.structured(&cfs) {
        // Restart the body (structured() bails before emitting anything when it fails).
        ctx.state_machine(&cfs);
    }
    ctx.ret();
    ctx.indent = 0;
    ctx.line("}");
    let mut out = ctx.out;
    // Unused-variable cleanup: drop temps that never appear.
    if !temps_array {
        let mut lines: Vec<String> = Vec::new();
        for l in out.code.lines() {
            if let Some(rest) = l.trim_start().strip_prefix("var r") {
                if let Some(n) = rest.split(':').next() {
                    let name = format!("r{n}");
                    let used = out.code.lines().any(|o| !o.trim_start().starts_with(&format!("var {name}:")) && contains_ident(o, &name));
                    if !used {
                        continue;
                    }
                }
            }
            lines.push(l.to_string());
        }
        out.code = lines.join("\n");
        out.code.push('\n');
    }
    let _ = write!(out.code, "");
    out
}

fn contains_ident(line: &str, ident: &str) -> bool {
    let b = line.as_bytes();
    let mut start = 0;
    while let Some(p) = line[start..].find(ident) {
        let s = start + p;
        let e = s + ident.len();
        let before_ok = s == 0 || !(b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_');
        let after_ok = e >= b.len() || !(b[e].is_ascii_alphanumeric() || b[e] == b'_');
        if before_ok && after_ok {
            return true;
        }
        start = e;
    }
    false
}
