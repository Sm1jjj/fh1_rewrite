//! Builds a complete Bevy WGSL module from one effect pass (translated VS + PS).
//!
//! Constant registers are routed three ways:
//! - registers below [`MATERIAL_REGS`] → the per-material uniform (`fx_mat`), initialised from
//!   the constant tables' defaults and then from the model's material constants;
//! - object/camera matrices (by constant-table name) → computed from Bevy's mesh and view
//!   uniforms, so every object gets its own transform;
//! - track per-object registers the game sets for every draw (`ModelData` c148,
//!   `SurfaceNormalAndShadowPower` c162, [`object_slot`]) → `fx_mat.object[k]`, so each placement can
//!   carry its own (tint and ground normal from the `.pgeo` instance; docs/PROPS.md);
//! - everything else → the shared global register file (`fx_glob`, a storage buffer), which the
//!   lighting code fills by parameter name.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use fh1_shaders::container::{usage, RegisterSet, ShaderBlob, Stage};
use fh1_shaders::effect::{rs, Effect, Pass};
use fh1_shaders::wgsl::{self, Resolver, Translation};

/// Float registers per stage that live in the material uniform.
pub const MATERIAL_REGS: usize = 16;

/// Material texture bindings: (binding of the texture; its sampler is binding + 1).
pub const TEX_2D_BASE: u32 = 2; // tf0..tf15 → 2 + 2*tf
pub const TEX_CUBE_BASE: u32 = 34; // cube slots: 34 + 2*k
pub const CUBE_SLOTS: usize = 3;

/// Which kind of geometry/material a program serves: decides vertex formats and how constant
/// registers are split between the material and the shared globals.
#[derive(Clone, Debug)]
pub enum Family {
    /// Track scenery: c0..c15 are material registers, decoded float vertex attributes.
    Track,
    /// Cars: registers whose constant-table name is in `material_names` (ShaderSettings and per-part
    /// parameters) or that have no name are material registers (256 per stage); raw packed carbin
    /// vertices.
    Car { material_names: Arc<HashSet<String>> },
}

impl Family {
    pub fn material_regs(&self) -> usize {
        match self {
            Family::Track => MATERIAL_REGS,
            Family::Car { .. } => 256,
        }
    }
    fn is_material(&self, name: Option<&str>, reg: u32) -> bool {
        match self {
            Family::Track => (reg as usize) < MATERIAL_REGS,
            Family::Car { material_names } => name.is_none_or(|n| material_names.contains(n)),
        }
    }
}

/// Vertex attribute shader locations for a family, by (usage, index).
pub fn family_attribute_location(f: &Family, u: u8, idx: u8) -> Option<u32> {
    match f {
        Family::Track => attribute_location(u, idx),
        // Car pool (stride 28): scaledPosition SHORT4N, uv / uv2 USHORT2N, TanFrameQuat SHORT4N,
        // SH0 = 4 signed bytes (see fx_sh0).
        Family::Car { .. } => Some(match (u, idx) {
            (usage::POSITION, 0) => 20,
            (usage::TEXCOORD, 0) => 21,
            (usage::TEXCOORD, 1) => 22,
            (usage::TEXCOORD, 2) => 23,
            (usage::TEXCOORD, 3) => 24,
            _ => return None,
        }),
    }
}

/// Vertex attribute shader locations (track family), by (usage, index).
pub fn attribute_location(u: u8, idx: u8) -> Option<u32> {
    Some(match (u, idx) {
        (usage::POSITION, 0) => 0,
        (usage::NORMAL, 0) => 1,
        (usage::TEXCOORD, 0) => 2,
        (usage::TEXCOORD, 1) => 3,
        (usage::TEXCOORD, 2) => 4,
        (usage::TANGENT, 0) => 5,
        (usage::COLOR, 0) => 6,
        (usage::TEXCOORD, 3) => 7,
        (usage::BINORMAL, 0) => 8,
        (usage::COLOR, 1) => 9,
        _ => return None,
    })
}

/// WGSL type and conversion to vec4 for each attribute location (D3D defaults 0, 0, 0, 1).
fn attribute_wgsl(loc: u32) -> (&'static str, &'static str) {
    match loc {
        0 | 1 | 5 | 8 => ("vec3<f32>", "vec4<f32>({}, 1.0)"),
        2 | 3 | 4 | 7 | 21 | 22 => ("vec2<f32>", "vec4<f32>({}, 0.0, 1.0)"),
        20 | 23 => ("vec4<f32>", "{}"),
        24 => ("u32", "fx_sh0({})"),
        // D3DCOLOR: bytes are A, R, G, B in file order.
        _ => ("vec4<f32>", "({}).yzwx"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cull {
    None,
    /// D3DCULL_CW: cull triangles that are clockwise on screen.
    Cw,
    Ccw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Blend {
    /// Xenos blend factors (see fh1_shaders::effect::rs::SRCBLEND).
    pub src: u32,
    pub dst: u32,
}

/// Pipeline state for one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PassState {
    pub z_enable: bool,
    pub z_write: bool,
    pub cull: Cull,
    pub blend: Option<Blend>,
    pub alpha_to_coverage: bool,
    /// Raw f32 bits (slope-scaled, constant).
    pub depth_bias: (u32, u32),
}

impl PassState {
    pub fn from_pass(p: &Pass) -> Self {
        let get = |k: u32, d: u32| p.state(k).unwrap_or(d);
        let blend = (get(rs::ALPHABLENDENABLE, 0) != 0).then(|| Blend { src: get(rs::SRCBLEND, 1), dst: get(rs::DESTBLEND, 0) });
        Self {
            z_enable: get(rs::ZENABLE, 1) != 0,
            z_write: get(rs::ZWRITEENABLE, 1) != 0,
            cull: match get(rs::CULLMODE, 2) {
                0 => Cull::None,
                6 => Cull::Ccw,
                _ => Cull::Cw,
            },
            blend,
            alpha_to_coverage: get(rs::ALPHATOMASKENABLE, 0) != 0,
            depth_bias: (get(rs::SLOPESCALEDEPTHBIAS, 0), get(rs::DEPTHBIAS, 0)),
        }
    }
}

/// A translated pass, ready to become a Bevy shader + pipeline.
#[derive(Debug, Clone)]
pub struct Program {
    pub wgsl: String,
    /// Vertex attribute locations the vertex shader reads.
    pub attributes: Vec<u32>,
    /// (tf index, dimension) of every texture fetched (both stages).
    pub textures: Vec<(u32, u8)>,
    pub state: PassState,
    /// Default material registers from the constant tables: (stage, reg) → value.
    pub material_defaults: BTreeMap<(Stage, u32), [f32; 4]>,
    /// Global parameters this program reads: name → (stage, first register, count).
    pub globals: Vec<(String, Stage, u32, u32)>,
    /// Every float constant of both stages: name → (stage, first register, count).
    pub named: Vec<(String, Stage, u32, u32)>,
    /// Sampler names from the constant tables: (tf index, name).
    pub samplers: Vec<(u32, String)>,
    pub vs: Translation,
    pub ps: Option<Translation>,
    /// The program reads a per-object register (`fx_mat.object`): materials need a variant per
    /// distinct object value.
    pub uses_object: bool,
}

/// Per-object track registers routed to the material's `object` slots: `ModelData` (c148, the instance
/// tint as RGBA/255) → 0, `SurfaceNormalAndShadowPower` (c162, ground normal + power) → 1.
pub fn object_slot(name: &str) -> Option<usize> {
    match name {
        "ModelData" => Some(0),
        "SurfaceNormalAndShadowPower" => Some(1),
        _ => None,
    }
}

/// Which cube slot a cube fetch uses.
pub fn cube_slot(tf: u32) -> usize {
    match tf {
        0 | 4 => 0,
        2 | 5 => 1,
        _ => 2,
    }
}

struct BevyResolver<'a> {
    blob: &'a ShaderBlob,
    family: &'a Family,
    attributes: BTreeMap<u32, ()>,
    textures: BTreeMap<(u32, u8), ()>,
    uses_object: bool,
}

/// Object/camera matrices that are computed per object in the shader instead of uploaded.
fn builtin_matrix(name: &str) -> Option<&'static str> {
    Some(match name {
        "WorldViewProjMatrix" | "wvp" | "matWVP" | "MatWVP" | "worldViewProj" | "modelviewproj" => "fx_wvp",
        "WorldMatrix" | "world" | "matW" | "matObj" => "fx_world",
        "WorldInvTransMatrix" | "worldIT" | "matWInvT" => "fx_world_it",
        "worldI" | "WorldInvMatrix" => "fx_world_inv",
        "ViewInvMatrix" | "viewInv" | "matVInv" => "fx_view_inv",
        "ViewProj" | "matVP" | "c_viewproj" => "fx_view_proj",
        "WorldViewMatrix" | "matWV" => "fx_world_view",
        _ => return None,
    })
}

fn builtin_vector(name: &str) -> Option<&'static str> {
    Some(match name {
        "CamPosWorld" | "camPosWorld" | "CameraPos" | "c_eyePosWS" | "c_vsEyePosWS" => "vec4<f32>(view.world_position, 1.0)",
        // A constant with no default that the shader multiplies by (the lake VS scales its position by
        // c5 Default_identity); left at 0 it collapsed every lake vertex. Identity by name (INFERRED).
        "Default_identity" => "vec4<f32>(1.0)",
        _ => return None,
    })
}

impl BevyResolver<'_> {
    fn constant(&self, set: RegisterSet, reg: u32) -> Option<&fh1_shaders::container::Constant> {
        self.blob
            .constants
            .iter()
            .find(|c| c.set == set && reg >= c.register as u32 && reg < c.register as u32 + c.count.max(1) as u32)
    }
    fn stage_name(&self) -> &'static str {
        if self.blob.stage == Stage::Vertex {
            "vs"
        } else {
            "ps"
        }
    }
}

impl Resolver for BevyResolver<'_> {
    fn float_const(&mut self, _: Stage, reg: u32) -> String {
        if let Some(c) = self.constant(RegisterSet::Float4, reg) {
            if let Some(m) = builtin_matrix(&c.name) {
                // MatrixColumns (track effects): register r holds column r of the HLSL (row-vector)
                // matrix = row r of our column-vector matrix. MatrixRows (car library): register r
                // holds HLSL row r = our column r.
                let r = reg - c.register as u32;
                return if c.class == fh1_shaders::container::ParamClass::MatrixRows {
                    format!("{m}[{r}]")
                } else {
                    format!("fx_row({m}, {r})")
                };
            }
            if let Some(v) = builtin_vector(&c.name) {
                return v.to_string();
            }
        }
        let st = self.stage_name();
        let name = self.constant(RegisterSet::Float4, reg).map(|c| c.name.clone());
        if let (Family::Track, Some(k)) = (self.family, name.as_deref().and_then(object_slot)) {
            self.uses_object = true;
            return format!("fx_mat.object[{k}]");
        }
        if self.family.is_material(name.as_deref(), reg) {
            format!("fx_mat.{st}[{reg}]")
        } else {
            format!("fx_glob.{st}[{reg}]")
        }
    }
    fn float_const_rel(&mut self, _: Stage, base: u32, index: &str) -> String {
        let st = self.stage_name();
        let name = self.constant(RegisterSet::Float4, base).map(|c| c.name.clone());
        if self.family.is_material(name.as_deref(), base) {
            format!("fx_mat.{st}[clamp({base} + {index}, 0, {})]", self.family.material_regs() - 1)
        } else {
            format!("fx_glob.{st}[clamp({base} + {index}, 0, 255)]")
        }
    }
    fn bool_const(&mut self, _: Stage, addr: u32) -> String {
        // Raw address space: VS 0-127, PS 128-255 (as encoded in the microcode).
        format!("(fx_glob.b[{}][{}] != 0u)", addr / 4, addr % 4)
    }
    fn int_const(&mut self, _: Stage, id: u32) -> String {
        // Loop constants are already numbered per stage in the microcode (PS loops use i16+).
        format!("fx_glob.i[{}]", id % 32)
    }
    fn texture(&mut self, _: Stage, tf: u32, dimension: u8) -> Option<(String, String)> {
        self.textures.insert((tf, dimension), ());
        if dimension == 3 {
            let k = cube_slot(tf);
            Some((format!("fx_cube{k}"), format!("fx_cube{k}_s")))
        } else if tf < 16 {
            Some((format!("fx_t{tf}"), format!("fx_t{tf}_s")))
        } else {
            None
        }
    }
    fn texture_result(&mut self, stage: Stage, tf: u32, dimension: u8, value: String) -> String {
        // tf13 ShadowMaskSamp: the screen shadow mask, evaluated here per fragment (crate::shadow).
        if stage == Stage::Pixel && tf == crate::shadow::SHADOW_MASK_TF && dimension != 3 {
            return match self.family {
                Family::Car { .. } => format!("fx_shadow_mask_b({:.4})", crate::shadow::car_shadow_bias()),
                Family::Track => "fx_shadow_mask()".into(),
            };
        }
        match self.family {
            // Per-material gamma flags (from each .xds fetch constant's sign fields).
            _ if tf < 32 => format!("fx_tex_gamma({tf}u, {value})"),
            _ => value,
        }
    }
    fn vertex_input(&mut self, u: u8, idx: u8) -> String {
        match family_attribute_location(self.family, u, idx) {
            Some(loc) => {
                self.attributes.insert(loc, ());
                format!("fx_vin.a{loc}")
            }
            None => "vec4<f32>(0.0, 0.0, 0.0, 1.0)".into(),
        }
    }
}

const BINDINGS_IMPORTS: &str = r#"
#import bevy_pbr::mesh_functions::get_world_from_local
#import bevy_pbr::mesh_view_bindings::{view, lights, directional_shadow_textures}
#ifdef VISIBILITY_RANGE_DITHER
#import bevy_pbr::mesh_functions::{get_visibility_range_dither_level, get_tag}
#endif
"#;

/// Distance / fade cross-fade (pop-in, P3): entities with a crossfading `VisibilityRange` get Bevy's
/// `VISIBILITY_RANGE_DITHER` def; their dither level is Bevy's (camera distance to the entity origin) unless the
/// entity's `MeshTag` has bit 31 set, then it is `(tag & 63) - 16` (set per frame by the engine's zone fades,
/// scenery.rs). Same levels and pattern rule as bevy_pbr `visibility_range_dither`: negative = appearing,
/// positive = disappearing, so two LODs crossing over cover each pixel exactly once.
const VR_DITHER_WGSL: &str = r#"
#ifdef VISIBILITY_RANGE_DITHER
fn fx_vr_level(instance_index: u32) -> i32 {
    let tag = get_tag(instance_index);
    if ((tag & 0x80000000u) != 0u) {
        return i32(tag & 63u) - 16;
    }
    return get_visibility_range_dither_level(instance_index, get_world_from_local(instance_index)[3]);
}
fn fx_vr_dither(p: vec4<f32>, d: i32) {
    if (d == 0) {
        return;
    }
    if (d <= -16 || d >= 16) {
        discard;
    }
    var bayer = array<i32, 16>(0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5);
    let c = vec2<u32>(floor(p.xy)) % 4u;
    let t = bayer[c.y * 4u + c.x];
    if ((d >= 0 && d + t >= 16) || (d < 0 && 1 + d + t <= 0)) {
        discard;
    }
}
#endif
"#;

/// Highest inter-stage location the dither level may take (it goes after the PS inputs).
const VR_DITHER_MAX_LOCATION: usize = 14;

const TRACK_MATERIAL: &str = r#"
struct FxMaterial {
    vs: array<vec4<f32>, 16>,
    ps: array<vec4<f32>, 16>,
    /// Bit tf set: texture tf is gamma-signed (RGB decoded with the Xenos PWL curve).
    gamma: vec4<u32>,
    /// Per-object registers (see `object_slot`): ModelData, SurfaceNormalAndShadowPower.
    object: array<vec4<f32>, 2>,
}
fn fx_tex_gamma(tf: u32, c_in: vec4<f32>) -> vec4<f32> {
    // gamma.y: single-channel texture, replicated like the 360's 1-channel fetch (FM4 masks; never set on FH1).
    var c = c_in;
    if (((fx_mat.gamma.y >> tf) & 1u) != 0u) { c = c_in.xxxx; }
    if (((fx_mat.gamma.x >> tf) & 1u) == 0u) { return c; }
    return vec4<f32>(fx_pwl_degamma(c.x), fx_pwl_degamma(c.y), fx_pwl_degamma(c.z), c.w);
}
"#;

const CAR_MATERIAL: &str = r#"
struct FxMaterial {
    vs: array<vec4<f32>, 256>,
    ps: array<vec4<f32>, 256>,
    /// Bit tf set: texture tf is gamma-signed (RGB decoded with the Xenos PWL curve).
    gamma: vec4<u32>,
}
fn fx_tex_gamma(tf: u32, c: vec4<f32>) -> vec4<f32> {
    if (((fx_mat.gamma.x >> tf) & 1u) == 0u) { return c; }
    return vec4<f32>(fx_pwl_degamma(c.x), fx_pwl_degamma(c.y), fx_pwl_degamma(c.z), c.w);
}
// Car SH0: 4 signed normalized bytes, read as the big-endian u32 (x = low byte = file byte 3).
// The VS swizzles .wzyx, so file bytes 0..3 are the D3DX order-2 SH transfer coefficients
// (Y00, Y1-1 ~ -y, Y10 ~ z, Y11 ~ -x): bytes 1-3 correlate 0.7-0.8 with the vertex normal
// (TanFrameQuat X axis) over ALF_8C_08 (VERIFIED by data; DEC4N decoding showed no correlation).
fn fx_sh0(v: u32) -> vec4<f32> {
    let b = vec4<i32>(i32(v << 24u) >> 24u, i32(v << 16u) >> 24u, i32(v << 8u) >> 24u, i32(v) >> 24u);
    return max(vec4<f32>(b) / 127.0, vec4<f32>(-1.0));
}
"#;

const BINDINGS_HEAD: &str = r#"
struct FxGlobals {
    vs: array<vec4<f32>, 256>,
    ps: array<vec4<f32>, 256>,
    b: array<vec4<u32>, 64>,
    i: array<vec4<i32>, 32>,
}
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> fx_mat: FxMaterial;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<storage, read> fx_glob: FxGlobals;
"#;

const MATRIX_HELPERS: &str = r#"
var<private> fx_wvp: mat4x4<f32>;
var<private> fx_world: mat4x4<f32>;
var<private> fx_world_inv: mat4x4<f32>;
var<private> fx_world_it: mat4x4<f32>;
var<private> fx_view_inv: mat4x4<f32>;
var<private> fx_view_proj: mat4x4<f32>;
var<private> fx_world_view: mat4x4<f32>;

// Row r of a column-vector matrix: what a D3D shader reads from register base + r.
fn fx_row(m: mat4x4<f32>, r: u32) -> vec4<f32> {
    return vec4<f32>(m[0][r], m[1][r], m[2][r], m[3][r]);
}
fn fx_inverse_affine(m: mat4x4<f32>) -> mat4x4<f32> {
    let a = mat3x3<f32>(m[0].xyz, m[1].xyz, m[2].xyz);
    let c0 = cross(a[1], a[2]);
    let c1 = cross(a[2], a[0]);
    let c2 = cross(a[0], a[1]);
    let det = dot(a[0], c0);
    let inv = transpose(mat3x3<f32>(c0, c1, c2)) * (1.0 / det);
    let t = -(inv * m[3].xyz);
    return mat4x4<f32>(vec4<f32>(inv[0], 0.0), vec4<f32>(inv[1], 0.0), vec4<f32>(inv[2], 0.0), vec4<f32>(t, 1.0));
}
fn fx_setup_matrices(instance_index: u32) {
    fx_world = get_world_from_local(instance_index);
    fx_world_inv = fx_inverse_affine(fx_world);
    fx_world_it = transpose(fx_world_inv);
    fx_view_proj = view.clip_from_world;
    fx_wvp = view.clip_from_world * fx_world;
    fx_view_inv = view.world_from_view;
    fx_world_view = view.view_from_world * fx_world;
}
"#;

/// Translate one pass of an effect.
/// `raw_output`: write the shader's colour as is (sqrt-encoded, for the FH1 post chain); otherwise
/// square it back to linear for Bevy's own pipeline.
pub fn build(effect: &Effect, technique: &str, alpha_test_fallback: bool, raw_output: bool, family: &Family) -> Option<Program> {
    // `name#k` selects pass k (default 0): car lamp lenses draw pass 1 as a second material (L3).
    let (technique, pass_index) = technique.split_once('#').map_or((technique, 0), |(t, k)| (t, k.parse().unwrap_or(0)));
    let tech = effect.technique(technique)?;
    let pass = tech.passes.get(pass_index)?;
    let vs_blob = &effect.shaders[pass.vs?];
    let ps_blob = pass.ps.map(|i| &effect.shaders[i]);
    let mut state = PassState::from_pass(pass);
    // FH1_FX_DEBUG=<hash>:<mode>+nocull also draws that effect two-sided.
    if std::env::var("FH1_FX_DEBUG").is_ok_and(|v| v.ends_with("+nocull") && v.starts_with(&format!("{:08x}", effect.hash))) {
        state.cull = Cull::None;
    }

    let mut vr = BevyResolver { blob: vs_blob, family, attributes: BTreeMap::new(), textures: BTreeMap::new(), uses_object: false };
    let vs = wgsl::translate(vs_blob, "fx_vs_main", &mut vr);
    let mut ps_object = false;
    let (ps, ps_tex) = match ps_blob {
        Some(b) => {
            let mut pr = BevyResolver { blob: b, family, attributes: BTreeMap::new(), textures: BTreeMap::new(), uses_object: false };
            let t = wgsl::translate(b, "fx_ps_main", &mut pr);
            ps_object = pr.uses_object;
            (Some(t), pr.textures)
        }
        None => (None, BTreeMap::new()),
    };
    let attributes: Vec<u32> = vr.attributes.keys().copied().collect();
    let mut textures: Vec<(u32, u8)> = vr.textures.keys().copied().collect();
    textures.extend(ps_tex.keys().copied());
    textures.sort();
    textures.dedup();

    // Material defaults and globals from both constant tables.
    let mut material_defaults = BTreeMap::new();
    let mut globals = Vec::new();
    let mut named = Vec::new();
    let mut samplers = Vec::new();
    for b in std::iter::once(vs_blob).chain(ps_blob) {
        for c in b.constants.iter().filter(|c| c.set == RegisterSet::Sampler) {
            samplers.push((c.register as u32, c.name.clone()));
        }
        for c in b.constants.iter().filter(|c| c.set == RegisterSet::Float4) {
            named.push((c.name.clone(), b.stage, c.register as u32, c.count.max(1) as u32));
            for k in 0..c.count.max(1) as u32 {
                let reg = c.register as u32 + k;
                if family.is_material(Some(&c.name), reg) && (reg as usize) < family.material_regs() {
                    if let Some(d) = &c.default {
                        let o = k as usize * 4;
                        if d.len() >= o + 4 {
                            material_defaults.insert((b.stage, reg), [0, 1, 2, 3].map(|j| f32::from_bits(d[o + j])));
                        }
                    }
                }
            }
            if !family.is_material(Some(&c.name), c.register as u32) && builtin_matrix(&c.name).is_none() && builtin_vector(&c.name).is_none() {
                globals.push((c.name.clone(), b.stage, c.register as u32, c.count.max(1) as u32));
            }
        }
    }

    // Assemble the module.
    let mut m = String::new();
    m += "diagnostic(off, derivative_uniformity);\n";
    m += BINDINGS_IMPORTS;
    m += BINDINGS_HEAD;
    for &(tf, dim) in &textures {
        if dim == 3 {
            let k = cube_slot(tf);
            let b = TEX_CUBE_BASE + 2 * k as u32;
            let _ = writeln!(m, "@group(#{{MATERIAL_BIND_GROUP}}) @binding({b}) var fx_cube{k}: texture_cube<f32>;");
            let _ = writeln!(m, "@group(#{{MATERIAL_BIND_GROUP}}) @binding({}) var fx_cube{k}_s: sampler;", b + 1);
        } else if tf < 16 {
            let b = TEX_2D_BASE + 2 * tf;
            let _ = writeln!(m, "@group(#{{MATERIAL_BIND_GROUP}}) @binding({b}) var fx_t{tf}: texture_2d<f32>;");
            let _ = writeln!(m, "@group(#{{MATERIAL_BIND_GROUP}}) @binding({}) var fx_t{tf}_s: sampler;", b + 1);
        }
    }
    // A texture can be fetched as both 2D and cube on the same tf in different shaders, but
    // never within one program; dedupe the declarations just in case.
    m = dedupe_lines(&m);
    m += wgsl::PRELUDE;
    m += match family {
        Family::Track => TRACK_MATERIAL,
        Family::Car { .. } => CAR_MATERIAL,
    };
    m += MATRIX_HELPERS;
    m += crate::shadow::FX_SHADOW_WGSL;
    m += crate::headlight::FX_HEADLIGHT_WGSL;
    m += VR_DITHER_WGSL;

    // Vertex input.
    m += "struct FxVertexIn {\n    @builtin(instance_index) instance_index: u32,\n    @builtin(vertex_index) vertex_index: u32,\n";
    for &loc in &attributes {
        let _ = writeln!(m, "    @location({loc}) a{loc}: {},", attribute_wgsl(loc).0);
    }
    m += "}\nstruct FxVin {\n";
    for &loc in &attributes {
        let _ = writeln!(m, "    a{loc}: vec4<f32>,");
    }
    m += "    pad: f32,\n}\nvar<private> fx_vin: FxVin;\n";

    // Interpolator linkage: PS inputs in order get locations 0..n.
    let ps_inputs: Vec<(u8, u8, u8)> = ps_blob.map(|b| b.interpolators.iter().map(|i| (i.usage, i.usage_index, i.reg)).collect()).unwrap_or_default();
    m += "struct FxVaryings {\n    @builtin(position) position: vec4<f32>,\n";
    for (n, _) in ps_inputs.iter().enumerate() {
        let _ = writeln!(m, "    @location({n}) v{n}: vec4<f32>,");
    }
    let vr_dither = ps_inputs.len() <= VR_DITHER_MAX_LOCATION;
    if vr_dither {
        let _ = writeln!(m, "#ifdef VISIBILITY_RANGE_DITHER\n    @location({}) @interpolate(flat) vr_dither: i32,\n#endif", ps_inputs.len());
    }
    m += "}\n";

    m += &vs.code;
    if let Some(ps) = &ps {
        m += &ps.code;
    }

    // Vertex entry.
    m += "@vertex\nfn vertex(input: FxVertexIn) -> FxVaryings {\n    fx_setup_matrices(input.instance_index);\n";
    for &loc in &attributes {
        let conv = attribute_wgsl(loc).1.replace("{}", &format!("input.a{loc}"));
        let _ = writeln!(m, "    fx_vin.a{loc} = {conv};");
    }
    m += "    let o = fx_vs_main(input.vertex_index);\n    var out: FxVaryings;\n    out.position = o.pos;\n";
    for (n, &(u, idx, _)) in ps_inputs.iter().enumerate() {
        // VS export register = position in the VS interpolator list.
        if let Some(k) = vs_blob.interpolators.iter().position(|i| i.usage == u && i.usage_index == idx) {
            let _ = writeln!(m, "    out.v{n} = o.o{k};");
        } else {
            let _ = writeln!(m, "    out.v{n} = vec4<f32>(0.0);");
        }
    }
    if vr_dither {
        m += "#ifdef VISIBILITY_RANGE_DITHER\n    out.vr_dither = fx_vr_level(input.instance_index);\n#endif\n";
    }
    m += "    return out;\n}\n";

    // Fragment entry.
    m += "@fragment\nfn fragment(input: FxVaryings, @builtin(front_facing) ff: bool) -> @location(0) vec4<f32> {\n    var i: FxPsIn;\n    i.vpos = input.position;\n    fx_frag_pos = input.position;\n    i.front_facing = ff;\n";
    if vr_dither {
        m += "#ifdef VISIBILITY_RANGE_DITHER\n    fx_vr_dither(input.position, input.vr_dither);\n#endif\n";
    }
    for (n, &(_, _, reg)) in ps_inputs.iter().enumerate() {
        let _ = writeln!(m, "    i.r{reg} = input.v{n};");
    }
    if ps.is_some() {
        m += "    let o = fx_ps_main(i);\n";
        if let Some(dbg) = debug_output(effect.hash, &textures) {
            if dbg.contains("fx_dbg") {
                // `cubefetch`: the direction handed to textureSample.
                // `cubedir`: the direction the PS's first cube instruction looks up.
                m = m.replacen("let vr = fx_cube(s0, s1);", "let vr = fx_cube(s0, s1); fx_dbg = vec4<f32>(s1.y, s1.x, s0.x, 0.0);", 1);
                m = m.replacen("derivative_uniformity);
", "derivative_uniformity);
var<private> fx_dbg: vec4<f32>;
var<private> fx_dbg2: vec4<f32>;
", 1);
                if let Some(at) = m.find("fn fx_ps_main") {
                    let (head, tail) = m.split_at(at);
                    let tail = tail.replacen("textureSample(fx_cube0, fx_cube0_s, fx_cube_dir(r[3].xyz))", "textureSample(fx_cube0, fx_cube0_s, fx_cube_dir(fx_dbg_set2(r[3].xyz)))", 1);
                    m = format!("{head}fn fx_dbg_set2(d: vec3<f32>) -> vec3<f32> {{ fx_dbg2 = vec4<f32>(d, 0.0); return d; }}
{tail}");
                }
            }
            m += &dbg;
        }
        if state.alpha_to_coverage && alpha_test_fallback {
            // Without MSAA, alpha-to-coverage becomes a 50% alpha test.
            m += "    if (o.c0.w < 0.5) { discard; }\n";
        }
        // The game writes sqrt(colour) for its own post chain. Until that chain runs here, hand
        // Bevy linear colour.
        if raw_output {
            m += "    return o.c0;\n}\n";
        } else {
            m += &format!("    return vec4<f32>(o.c0.rgb * o.c0.rgb * {:?}, o.c0.a);\n}}\n", crate::output_gain());
        }
    } else {
        m += "    return vec4<f32>(0.0);\n}\n";
    }

    let uses_object = vr.uses_object || ps_object;
    Some(Program { wgsl: m, attributes, textures, state, material_defaults, globals, named, samplers, vs, ps, uses_object })
}

/// `FH1_FX_DEBUG=<effect hash hex>:<mode>`: replace that effect's output with a debug colour (opaque) to see
/// one input at a time. Modes: `out` (colour, alpha 1), `alpha`, `n` (PS input r0 as a normal), `v` (r1 as a
/// direction), `uv` (fract r3.xy), `t<k>` (texture k at r3.xy), `cube` / `cubedown` (cube slot 0 at reflect(∓r1, r0)),
/// `in<k>` / `in<k>w` (PS input register k), `cubedir` / `cubefetch` (signs of the first cube instruction's input /
/// of the direction handed to textureSample, as 0/1 per axis), `cubeface`, `cubeid`, `cubest`. Append `+nocull` to
/// draw the effect two-sided. The register meanings are each effect's own (the lake shader: r0 = N, r1 = V, r3 = uv).
fn debug_output(hash: u32, textures: &[(u32, u8)]) -> Option<String> {
    let v = std::env::var("FH1_FX_DEBUG").ok()?;
    let (h, mode) = v.split_once(':')?;
    let mode = mode.trim_end_matches("+nocull");
    if u32::from_str_radix(h.trim_start_matches("0x"), 16).ok()? != hash {
        return None;
    }
    bevy::log::info!("FH1_FX_DEBUG: effect {hash:08x} output = {mode}");
    let c = match mode {
        "out" => "vec4<f32>(o.c0.rgb, 1.0)".to_string(),
        "alpha" => "vec4<f32>(o.c0.aaa, 1.0)".to_string(),
        "n" => "vec4<f32>(normalize(i.r0.xyz) * 0.5 + 0.5, 1.0)".to_string(),
        "v" => "vec4<f32>(normalize(i.r1.xyz) * 0.5 + 0.5, 1.0)".to_string(),
        "uv" => "vec4<f32>(fract(i.r3.xy), 0.0, 1.0)".to_string(),
        "cube" if textures.iter().any(|t| t.1 == 3) => {
            "vec4<f32>(textureSample(fx_cube0, fx_cube0_s, reflect(-normalize(i.r1.xyz), normalize(i.r0.xyz))).rgb, 1.0)".to_string()
        }
        "cubedir" => "vec4<f32>(step(vec3<f32>(0.0), fx_dbg.xyz), 1.0)".to_string(),
        "cubefetch" => "vec4<f32>(step(vec3<f32>(0.0), fx_cube_dir(fx_dbg2.xyz)), 1.0)".to_string(),
        // Face id of the fetch as grey levels 0..5 → 0..1, and the instruction's own face id.
        "cubeface" => "vec4<f32>(vec3<f32>(fx_dbg2.z / 5.0), 1.0)".to_string(),
        "cubeid" => "vec4<f32>(vec3<f32>(fx_cube(vec4<f32>(fx_dbg.z, fx_dbg.z, fx_dbg.x, fx_dbg.y), vec4<f32>(fx_dbg.y, fx_dbg.x, fx_dbg.z, fx_dbg.z)).w / 5.0), 1.0)".to_string(),
        "cubest" => "vec4<f32>(fract(fx_dbg2.xy), 0.0, 1.0)".to_string(),
        "cubedown" if textures.iter().any(|t| t.1 == 3) => {
            "vec4<f32>(textureSample(fx_cube0, fx_cube0_s, reflect(normalize(i.r1.xyz), normalize(i.r0.xyz))).rgb, 1.0)".to_string()
        }
        // in<k> / in<k>w: PS input register k (rgb / w).
        t if t.starts_with("in") => {
            let (k, w) = t[2..].strip_suffix('w').map_or((&t[2..], false), |k| (k, true));
            let k: u32 = k.parse().ok()?;
            if w {
                format!("vec4<f32>(i.r{k}.www, 1.0)")
            } else {
                format!("vec4<f32>(i.r{k}.xyz, 1.0)")
            }
        }
        t if t.starts_with('t') => {
            let k: u32 = t[1..].parse().ok()?;
            textures.iter().any(|x| x.0 == k && x.1 != 3).then(|| format!("vec4<f32>(textureSample(fx_t{k}, fx_t{k}_s, i.r3.xy).rgb, 1.0)"))?
        }
        _ => return None,
    };
    Some(format!("    return {c};\n"))
}

fn dedupe_lines(s: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out = String::new();
    for l in s.lines() {
        if l.starts_with("@group") && !seen.insert(l.to_string()) {
            continue;
        }
        out += l;
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    /// `FH1_FX_DUMP=<file.fxobj> cargo test -p fh1-render dump_wgsl -- --nocapture`: print the translated
    /// Default technique (debugging aid; skipped without the variable).
    #[test]
    fn dump_wgsl() {
        let Ok(path) = std::env::var("FH1_FX_DUMP") else { return };
        let fx = fh1_shaders::effect::Effect::parse(&std::fs::read(path).unwrap()).unwrap();
        let p = super::build(&fx, "Default", true, true, &super::Family::Track).unwrap();
        println!("// effect hash {:08x}\n{}", fx.hash, p.wgsl);
    }
}
