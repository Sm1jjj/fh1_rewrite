//! Remaster scenery/prop material (W1, docs/REMASTER.md "Material rules"): ONE bindless uber-material for every
//! Colorado game material (`StandardMaterial` + [`SceneryExt`]), replacing the ~170 translated FxMaterial programs.
//!
//! - The record (class, layering, texture per role, UV set/scale per role, constants) comes from the `remaster`
//!   setup group (`remaster/scenery/<track>/materials.bin`, written by fh1setup `remaster.rs`).
//! - Own vertex + fragment shader (main pass): position, normal, uv0-2 and the game's vertex colour (Fx_Color) as the
//!   faithful tiles carry them; blending (splat / road noise + vertex colour / vertex blend), derivative normal mapping,
//!   AO, night lightmap and emissive, then Bevy's PBR lighting. The prepass and shadow passes use Bevy's defaults
//!   (cutouts: the StandardMaterial base texture = layer A with the A UV scale, alpha mask).
//! - Pipelines: one per (alpha mode, cull, vertex layout) — a handful, not one per game shader.
//! - Night: [`RemasterNight`] (written by the lighting owner, W3) is copied into every material when it changes by
//!   more than a step; between changes nothing is touched.

use bevy::mesh::{MeshVertexBufferLayoutRef, VertexAttributeDescriptor};
use bevy::pbr::{ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError};
use bevy::shader::{ShaderDefVal, ShaderRef};

use crate::batch;

pub type RemasterMaterial = ExtendedMaterial<StandardMaterial, SceneryExt>;

/// Texture roles, in record order (fh1setup remaster.rs `ROLE_NAMES`).
pub const ROLES: [&str; 12] = ["a", "b", "c", "weight", "normal", "normal_b", "modulate", "ao", "lightmap", "emissive", "specular", "mask"];
pub const ROLE_A: usize = 0;
pub const ROLE_B: usize = 1;
pub const ROLE_C: usize = 2;
pub const ROLE_W: usize = 3;
pub const ROLE_N: usize = 4;
pub const ROLE_NB: usize = 5;
pub const ROLE_MOD: usize = 6;
pub const ROLE_AO: usize = 7;
pub const ROLE_LM: usize = 8;
pub const ROLE_EM: usize = 9;
pub const ROLE_SP: usize = 10;
pub const ROLE_MASK: usize = 11;

/// Material class (record byte 0).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Class {
    Opaque,
    Cutout,
    Decal,
    Water,
    Additive,
    Skip,
    Unlit,
}

impl Class {
    fn from_u8(v: u8) -> Class {
        match v {
            0 => Class::Opaque,
            1 => Class::Cutout,
            2 => Class::Decal,
            4 => Class::Water,
            5 => Class::Additive,
            7 => Class::Unlit,
            _ => Class::Skip,
        }
    }
}

pub const FLAG_TWO_SIDED: u16 = 1;
pub const FLAG_DEPTH_BIAS: u16 = 2;
pub const FLAG_NIGHT_EMISSIVE: u16 = 4;
pub const FLAG_ANIMATED: u16 = 8;
pub const FLAG_OBJECT_TINT: u16 = 16;
pub const FLAG_REFLECTIVE: u16 = 32;
pub const FLAG_INSTANCE_LM: u16 = 64;
pub const FLAG_CLOTH: u16 = 128;
pub const FLAG_LM_BAKED: u16 = 256;
pub const FLAG_ROAD: u16 = 512;
pub const FLAG_TREE: u16 = 1024;
/// Uniform-only flag (not in the setup table): a decal drawn as an alpha-tested Mask ([`decal_mask_on`]).
pub const FLAG_DECAL_MASK: u16 = 0x8000;

/// P8 (2026-10-08, OPT-IN `FH1_RM_DECAL_MASK=1`): decals draw as alpha-tested Mask (cutoff `FH1_RM_DECAL_MASK_CUTOFF`,
/// default 0.5) in the opaque pass instead of sorted Blend draws in the transparent pass (1.0 ms/frame in log 105206).
/// The decal alpha (texture x vertex alpha / blue) is computed as before and tested; soft edges (road wear, dirt
/// patches fading into the ground) turn hard, so this waits for the user's look check. Water / Additive unchanged.
pub fn decal_mask_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RM_DECAL_MASK").is_ok_and(|v| v == "1"))
}

fn decal_mask_cutoff() -> f32 {
    std::env::var("FH1_RM_DECAL_MASK_CUTOFF").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5)
}

/// One `materials.bin` record.
#[derive(Clone, Debug)]
pub struct Record {
    pub class: Class,
    pub layering: u8,
    pub flags: u16,
    pub tex: [Option<u32>; 12],
    pub uv_set: [u8; 12],
    pub uv_scale: [[f32; 2]; 12],
    pub params: [[f32; 4]; 4],
    pub srgb: u32,
}

const RECORD_SIZE: usize = 4 + 4 * 12 + 12 + 3 + 8 * 12 + 64 + 4;

/// Parse `materials.bin` (index = game material id).
pub fn parse_table(b: &[u8]) -> Option<Vec<Record>> {
    if b.get(..8)? != b"FH1RMAT1" {
        return None;
    }
    let u32_at = |o: usize| b.get(o..o + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()));
    let f32_at = |o: usize| b.get(o..o + 4).map(|x| f32::from_le_bytes(x.try_into().unwrap()));
    let n = u32_at(8)? as usize;
    let size = u32_at(12)? as usize;
    if size != RECORD_SIZE {
        warn!("remaster: materials.bin record size {size}, expected {RECORD_SIZE} (re-run fh1setup --only remaster)");
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let o = 16 + i * size;
        let r = b.get(o..o + size)?;
        let mut tex = [None; 12];
        for (k, t) in tex.iter_mut().enumerate() {
            let v = u32_at(o + 4 + k * 4)?;
            *t = (v != u32::MAX).then_some(v);
        }
        let mut uv_set = [0u8; 12];
        uv_set.copy_from_slice(&r[52..64]);
        let mut uv_scale = [[1.0f32; 2]; 12];
        for (k, s) in uv_scale.iter_mut().enumerate() {
            *s = [f32_at(o + 67 + k * 8)?, f32_at(o + 71 + k * 8)?];
        }
        let mut params = [[0.0f32; 4]; 4];
        for (k, p) in params.iter_mut().enumerate() {
            for (j, v) in p.iter_mut().enumerate() {
                *v = f32_at(o + 163 + (k * 4 + j) * 4)?;
            }
        }
        out.push(Record {
            class: Class::from_u8(r[0]),
            layering: r[1],
            flags: u16::from_le_bytes([r[2], r[3]]),
            tex,
            uv_set,
            uv_scale,
            params,
            srgb: u32_at(o + 227)?,
        });
    }
    Some(out)
}

/// Night inputs for the scenery (W3 writes it from the TOD clock; defaults = day).
/// - `lightmap`: the game's `max(c213.y, c6.z)` weight of the baked night lightmaps (0 by day, 1 at night).
/// - `switch_on`: the game's SwitchOnLights L; emissive maps light when L >= 0.5 (ModelData.x default).
/// - `emissive_scale`: brightness of lightmap / emissive light in the remaster's units (W3 tunes it to its exposure).
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct RemasterNight {
    pub lightmap: f32,
    pub switch_on: f32,
    pub emissive_scale: f32,
    /// Brightness of a lamp emissive texel of 1 (nits).
    pub lamp_scale: f32,
}

impl Default for RemasterNight {
    fn default() -> Self {
        Self { lightmap: 0.0, switch_on: 0.0, emissive_scale: 1.0, lamp_scale: 1.0 }
    }
}

/// The scenery extension: layer textures + the record's constants.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone, Default)]
#[data(50, SceneryUniform, binding_array(101))]
#[bindless(index_table(range(50..75), binding(100)))]
pub struct SceneryExt {
    pub params: SceneryUniform,
    #[texture(51)]
    #[sampler(52)]
    pub a: Option<Handle<Image>>,
    #[texture(53)]
    #[sampler(54)]
    pub b: Option<Handle<Image>>,
    #[texture(55)]
    #[sampler(56)]
    pub c: Option<Handle<Image>>,
    #[texture(57)]
    #[sampler(58)]
    pub weight: Option<Handle<Image>>,
    #[texture(59)]
    #[sampler(60)]
    pub normal: Option<Handle<Image>>,
    #[texture(61)]
    #[sampler(62)]
    pub normal_b: Option<Handle<Image>>,
    #[texture(63)]
    #[sampler(64)]
    pub modulate: Option<Handle<Image>>,
    #[texture(65)]
    #[sampler(66)]
    pub ao: Option<Handle<Image>>,
    #[texture(67)]
    #[sampler(68)]
    pub lightmap: Option<Handle<Image>>,
    #[texture(69)]
    #[sampler(70)]
    pub emissive: Option<Handle<Image>>,
    #[texture(71)]
    #[sampler(72)]
    pub specular: Option<Handle<Image>>,
    #[texture(73)]
    #[sampler(74)]
    pub mask: Option<Handle<Image>>,
}

impl SceneryExt {
    pub fn slot_mut(&mut self, role: usize) -> &mut Option<Handle<Image>> {
        match role {
            ROLE_A => &mut self.a,
            ROLE_B => &mut self.b,
            ROLE_C => &mut self.c,
            ROLE_W => &mut self.weight,
            ROLE_N => &mut self.normal,
            ROLE_NB => &mut self.normal_b,
            ROLE_MOD => &mut self.modulate,
            ROLE_AO => &mut self.ao,
            ROLE_LM => &mut self.lightmap,
            ROLE_EM => &mut self.emissive,
            ROLE_SP => &mut self.specular,
            _ => &mut self.mask,
        }
    }
}

/// GPU constants (all vec4 / uvec4: no padding surprises).
#[derive(Clone, Copy, Default, Debug, ShaderType)]
pub struct SceneryUniform {
    /// UV scale per role, two roles per vec4 (a|b, c|weight, normal|normal_b, modulate|ao, lightmap|emissive, specular|mask).
    pub uv: [Vec4; 6],
    /// p0 = (albedo scale, spec level, spec power, normal strength); p1 = blend weights (game c4);
    /// p2 = (luminance scales c5.xyz, alpha cutoff); p3 = (lightmap floor, water, -, -).
    pub p: [Vec4; 4],
    /// x = UV set per role (2 bits each), y = roles present (bit per role), z = flags | class << 16, w = layering.
    pub info: UVec4,
    /// x = lightmap weight, y = emissive switch (SwitchOnLights), z = lightmap scale, w = lamp emissive scale (display-linear, not exposed).
    pub night: Vec4,
}

impl From<&SceneryExt> for SceneryUniform {
    fn from(e: &SceneryExt) -> Self {
        e.params
    }
}

pub(crate) const SHADER: Handle<Shader> = bevy::asset::uuid_handle!("2b9e4f61-7c3a-4d58-9e1f-a6c0b3d7e215");

/// Build the uniform for a record (night values from `night`).
pub fn uniform(r: &Record, night: RemasterNight) -> SceneryUniform {
    let mut u = SceneryUniform::default();
    for k in 0..6 {
        let (s0, s1) = (r.uv_scale[2 * k], r.uv_scale[2 * k + 1]);
        u.uv[k] = Vec4::new(s0[0], s0[1], s1[0], s1[1]);
    }
    for k in 0..4 {
        u.p[k] = Vec4::from_array(r.params[k]);
    }
    let class = match r.class {
        Class::Opaque => 0,
        Class::Cutout => 1,
        Class::Decal => 2,
        Class::Water => 4,
        Class::Additive => 5,
        Class::Skip => 6,
        Class::Unlit => 7,
    };
    let sets = r.uv_set.iter().enumerate().fold(0u32, |a, (k, &s)| a | ((s as u32 & 3) << (2 * k)));
    let present = r.tex.iter().enumerate().fold(0u32, |a, (k, t)| a | ((t.is_some() as u32) << k));
    let decal_mask = if r.class == Class::Decal && decal_mask_on() { FLAG_DECAL_MASK as u32 } else { 0 };
    u.info = UVec4::new(sets, present, r.flags as u32 | decal_mask | (class << 16), r.layering as u32);
    u.night = Vec4::new(night.lightmap, night.switch_on, night.emissive_scale, night.lamp_scale);
    u
}

/// The StandardMaterial base for a record: alpha mode, culling, depth bias, and (cutouts) the base texture used by
/// Bevy's prepass / shadow alpha test.
pub fn base(r: &Record, mirrored: bool) -> StandardMaterial {
    // Water two-sided (the river / lake sheets are seen from above whatever their winding; `FH1_RM_WATER_CULL=1` = the
    // record's own culling as before 2026-10-08).
    let two_sided = r.flags & FLAG_TWO_SIDED != 0 || (r.class == Class::Water && !std::env::var("FH1_RM_WATER_CULL").is_ok_and(|v| v == "1"));
    let alpha_mode = match r.class {
        Class::Cutout => AlphaMode::Mask(r.params[2][3].clamp(0.05, 0.95)),
        Class::Decal if decal_mask_on() => AlphaMode::Mask(decal_mask_cutoff()),
        Class::Decal | Class::Water => AlphaMode::Blend,
        Class::Additive => AlphaMode::Add,
        _ => AlphaMode::Opaque,
    };
    StandardMaterial {
        alpha_mode,
        double_sided: two_sided,
        // The game culls CW = Bevy's back faces (fh1-render material.rs); mirrored placements flip it.
        cull_mode: if two_sided {
            None
        } else if mirrored {
            Some(bevy::render::render_resource::Face::Front)
        } else {
            Some(bevy::render::render_resource::Face::Back)
        },
        depth_bias: if r.flags & FLAG_DEPTH_BIAS != 0 { 4.0 } else { 0.0 },
        unlit: matches!(r.class, Class::Unlit | Class::Additive),
        // Bevy 0.19 queues Blend / Add materials into the shadow pass and its prepass only discards base alpha < 0.05:
        // alpha 0 here keeps decals, glows and water out of the shadow maps (the main shader sets its own colour).
        base_color: if matches!(r.class, Class::Decal | Class::Additive | Class::Water) { Color::NONE } else { Color::WHITE },
        uv_transform: bevy::math::Affine2::from_scale(Vec2::from_array(r.uv_scale[ROLE_A])),
        perceptual_roughness: 1.0,
        ..default()
    }
}

impl MaterialExtension for SceneryExt {
    /// Depth prepass off by default (as FxMaterial; the main pass is the only depth writer): halves scenery draws.
    /// `FH1_RM_PREPASS=1` = on (needed only if something reads the prepass depth / normal textures).
    fn enable_prepass() -> bool {
        std::env::var("FH1_RM_PREPASS").is_ok_and(|v| v == "1")
    }

    fn vertex_shader() -> ShaderRef {
        ShaderRef::Handle(SHADER)
    }

    fn fragment_shader() -> ShaderRef {
        ShaderRef::Handle(SHADER)
    }

    fn specialize(
        _pipeline: &MaterialExtensionPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        key: MaterialExtensionKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // Only the main pass runs our vertex shader; prepass / shadows keep Bevy's (their layouts are Bevy's).
        if descriptor.vertex.shader != SHADER {
            return Ok(());
        }
        // Decals / overlays (alpha blend, no depth write): the game's blended effects set SLOPESCALEDEPTHBIAS 1.0
        // (render state 0xCC on every blended track family). Bevy's StandardMaterial depth_bias is a constant only,
        // which is ~nothing on Depth32Float at distance: add the slope term (reverse-Z: positive = towards the camera).
        let blend = key.mesh_key.intersection(bevy::pbr::MeshPipelineKey::BLEND_RESERVED_BITS);
        use bevy::render::render_resource::PrimitiveTopology as T;
        let triangles = matches!(descriptor.primitive.topology, T::TriangleList | T::TriangleStrip);
        // wgpu rejects depth bias on point / line topologies.
        if blend == bevy::pbr::MeshPipelineKey::BLEND_ALPHA && triangles {
            if let Some(ds) = descriptor.depth_stencil.as_mut() {
                ds.bias.slope_scale = decal_slope_bias();
                ds.bias.constant = ds.bias.constant.max(decal_constant_bias());
            }
        }
        let l = &layout.0;
        let mut attrs: Vec<VertexAttributeDescriptor> = vec![Mesh::ATTRIBUTE_POSITION.at_shader_location(0), Mesh::ATTRIBUTE_NORMAL.at_shader_location(1)];
        let mut defs: Vec<ShaderDefVal> = Vec::new();
        let mut opt = |present: bool, a: VertexAttributeDescriptor, def: &str| {
            if present {
                attrs.push(a);
                defs.push(def.into());
            }
        };
        opt(l.contains(Mesh::ATTRIBUTE_UV_0), Mesh::ATTRIBUTE_UV_0.at_shader_location(2), "RM_UV0");
        opt(l.contains(Mesh::ATTRIBUTE_UV_1), Mesh::ATTRIBUTE_UV_1.at_shader_location(3), "RM_UV1");
        opt(l.contains(fh1_render::material::ATTRIBUTE_UV2), fh1_render::material::ATTRIBUTE_UV2.at_shader_location(4), "RM_UV2");
        opt(l.contains(fh1_render::material::ATTRIBUTE_COLOR), fh1_render::material::ATTRIBUTE_COLOR.at_shader_location(5), "RM_COLOR");
        let lod = l.contains(batch::ATTRIBUTE_INSTANCE_LOD);
        opt(lod, batch::ATTRIBUTE_INSTANCE_LOD.at_shader_location(6), batch::SHADER_DEF);
        opt(lod && l.contains(batch::ATTRIBUTE_INSTANCE_TINT), batch::ATTRIBUTE_INSTANCE_TINT.at_shader_location(7), "INSTANCE_TINT");
        descriptor.vertex.buffers = vec![l.get_layout(&attrs)?];
        descriptor.vertex.shader_defs.extend(defs.iter().cloned());
        if let Some(f) = descriptor.fragment.as_mut() {
            f.shader_defs.extend(defs);
        }
        Ok(())
    }
}

/// Slope-scaled depth bias of decal pipelines (`FH1_RM_DECAL_SLOPE`, default 1.0 = the game's).
fn decal_slope_bias() -> f32 {
    std::env::var("FH1_RM_DECAL_SLOPE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0)
}

/// Constant depth bias of decal pipelines in depth ulps (`FH1_RM_DECAL_BIAS`, default 256 ~ a few mm at 100 m on
/// Depth32Float reverse-Z). Most of the game's blended families have constant 0 and rely on 24-bit integer depth.
fn decal_constant_bias() -> i32 {
    std::env::var("FH1_RM_DECAL_BIAS").ok().and_then(|v| v.parse().ok()).unwrap_or(256)
}

/// WGSL source: per-role sampling helpers are generated (bindless index table or plain bindings).
fn wgsl() -> String {
    let mut decl_bindless = String::from("struct SceneryIndices {\n    material: u32,\n");
    let mut decl_plain = String::new();
    let mut samplers = String::new();
    for (k, name) in ROLES.iter().enumerate() {
        let (t, s) = (51 + 2 * k, 52 + 2 * k);
        decl_bindless += &format!("    {name}_texture: u32,\n    {name}_sampler: u32,\n");
        decl_plain += &format!(
            "@group(#{{MATERIAL_BIND_GROUP}}) @binding({t}) var {name}_texture: texture_2d<f32>;\n@group(#{{MATERIAL_BIND_GROUP}}) @binding({s}) var {name}_sampler: sampler;\n"
        );
        samplers += &format!(
            "fn sample_{name}(slot: u32, uv: vec2<f32>, ddx: vec2<f32>, ddy: vec2<f32>) -> vec4<f32> {{
#ifdef BINDLESS
    let ix = scenery_indices[slot];
    return textureSampleGrad(bindless_textures_2d[ix.{name}_texture], bindless_samplers_filtering[ix.{name}_sampler], uv, ddx, ddy);
#else
    return textureSampleGrad({name}_texture, {name}_sampler, uv, ddx, ddy);
#endif
}}
"
        );
    }
    decl_bindless += "}\n";
    // Road / ground minimum roughness (`FH1_RM_ROAD_ROUGHNESS`, default 0.75 since 2026-10-07, was 0.6), baked into the shader source.
    let road = std::env::var("FH1_RM_ROAD_ROUGHNESS").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.75).clamp(0.0, 1.0);
    // Water (`FH1_RM_WATER=flat` debug colour, `FH1_RM_WATER_BUMP`, `FH1_RM_WATER_OPACITY` scale, `FH1_RM_WATER_MIN_ALPHA`).
    let wenv = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    let flat = std::env::var("FH1_RM_WATER").is_ok_and(|v| v == "flat");
    WGSL.replace("RM_ROAD_ROUGHNESS", &format!("{road:.3}"))
        .replace("RM_WATER_FLAT", if flat { "1.0" } else { "0.0" })
        .replace("RM_WATER_BUMP", &format!("{:.3}", wenv("FH1_RM_WATER_BUMP", 0.35)))
        .replace("RM_WATER_OPACITY", &format!("{:.3}", wenv("FH1_RM_WATER_OPACITY", 1.0).clamp(0.0, 1.0)))
        .replace("RM_WATER_MIN_ALPHA", &format!("{:.3}", wenv("FH1_RM_WATER_MIN_ALPHA", 0.55).clamp(0.0, 1.0)))
        .replace("//BINDLESS_DECL", &decl_bindless).replace("//PLAIN_DECL", &decl_plain).replace("//SAMPLERS", &samplers)
}

const WGSL: &str = r#"
#import bevy_pbr::{
    mesh_functions,
    mesh_bindings::mesh,
    mesh_view_bindings::{view, globals},
    view_transformations::position_world_to_clip,
    pbr_types,
    pbr_bindings,
    pbr_functions,
    forward_io::FragmentOutput,
}
#import bevy_render::bindless::{bindless_samplers_filtering, bindless_textures_2d}
#ifdef INSTANCE_LOD
#import fh1_remaster::batch_lod::{batch_lod_fades, batch_lod_hidden, batch_lod_collapsed, batch_lod_discard}
#endif

struct SceneryParams {
    uv: array<vec4<f32>, 6>,
    p: array<vec4<f32>, 4>,
    info: vec4<u32>,
    night: vec4<f32>,
}

#ifdef BINDLESS
//BINDLESS_DECL
@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<storage> scenery_indices: array<SceneryIndices>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var<storage> scenery_params: array<SceneryParams>;
#else
@group(#{MATERIAL_BIND_GROUP}) @binding(50) var<uniform> scenery_params: SceneryParams;
//PLAIN_DECL
#endif

//SAMPLERS

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
#ifdef RM_UV0
    @location(2) uv0: vec2<f32>,
#endif
#ifdef RM_UV1
    @location(3) uv1: vec2<f32>,
#endif
#ifdef RM_UV2
    @location(4) uv2: vec2<f32>,
#endif
#ifdef RM_COLOR
    @location(5) color: vec4<f32>,
#endif
#ifdef INSTANCE_LOD
    @location(6) lod: vec4<f32>,
#endif
#ifdef INSTANCE_TINT
    @location(7) tint: vec4<f32>,
#endif
}

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec4<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) uv01: vec4<f32>,
    @location(3) uv2: vec2<f32>,
    @location(4) color: vec4<f32>,
    @location(5) tint: vec4<f32>,
    @location(6) @interpolate(flat) instance_index: u32,
#ifdef INSTANCE_LOD
    @location(7) @interpolate(flat) lod_fades: vec2<f32>,
#endif
#ifdef VISIBILITY_RANGE_DITHER
    @location(8) @interpolate(flat) visibility_range_dither: i32,
#endif
}

@vertex
fn vertex(v: Vertex) -> Out {
    var out: Out;
    let world_from_local = mesh_functions::get_world_from_local(v.instance_index);
    var local = v.position;
    // Cloth (flags, bunting): a REMASTER wave along the normal, weighted by vertex alpha (the game's cloth weight;
    // 0 at the pinned edge), phase from the position. Material data is visible to the vertex stage.
    let vslot = mesh[v.instance_index].material_and_lightmap_bind_group_slot & 0xffffu;
#ifdef BINDLESS
    let vp = scenery_params[scenery_indices[vslot].material];
#else
    let vp = scenery_params;
#endif
    if (vp.info.z & 128u) != 0u {
        var weight = 1.0;
#ifdef RM_COLOR
        weight = v.color.x;
#endif
        let t = globals.time * 6.2831853 * vp.p[3].z;
        let ph = dot(v.position, vec3<f32>(1.7, 0.9, 1.3));
        let wave = sin(t + ph) + 0.5 * sin(1.9 * t + 2.3 * ph + 1.0);
        local = local + normalize(v.normal) * (vp.p[3].y * weight * wave);
    }
    out.world_position = mesh_functions::mesh_position_local_to_world(world_from_local, vec4<f32>(local, 1.0));
    out.position = position_world_to_clip(out.world_position.xyz);
    out.world_normal = mesh_functions::mesh_normal_local_to_world(v.normal, v.instance_index);
    if (vp.info.z & 1024u) != 0u {
        // Trees: the game lights the cards per instance without normals (tree_diff_opac_dirlight VS). REMASTER: a
        // spherical foliage normal from the tree origin (the placement's, or the merged instance centre), so a crown
        // shades sun side -> far side smoothly instead of per-card facing.
        var origin = world_from_local[3].xyz;
#ifdef INSTANCE_LOD
        origin = v.lod.xyz;
#endif
        let d = out.world_position.xyz - origin;
        out.world_normal = normalize(vec3<f32>(d.x, 0.5 * d.y + 0.5 * length(d.xz) + 0.2, d.z));
    }
    out.uv01 = vec4<f32>(0.0);
    out.uv2 = vec2<f32>(0.0);
#ifdef RM_UV0
    out.uv01 = vec4<f32>(v.uv0, v.uv0);
#endif
#ifdef RM_UV1
    out.uv01 = vec4<f32>(out.uv01.xy, v.uv1);
#endif
#ifdef RM_UV2
    out.uv2 = v.uv2;
#endif
    // Fx_Color holds D3DCOLOR bytes in file order A, R, G, B: (r, g, b, a) = .yzwx (fh1-render program.rs).
    out.color = vec4<f32>(1.0);
#ifdef RM_COLOR
    out.color = v.color.yzwx;
#endif
    // Placement tint: the game's 2 × ModelData.rgb (0.5 = neutral).
    out.tint = vec4<f32>(0.5);
#ifdef INSTANCE_TINT
    out.tint = v.tint;
#endif
    out.instance_index = v.instance_index;
#ifdef INSTANCE_LOD
    let f = batch_lod_fades(v.lod, view.world_position);
    if batch_lod_hidden(f) {
        out.position = batch_lod_collapsed();
    }
    out.lod_fades = f;
#endif
#ifdef VISIBILITY_RANGE_DITHER
    // Zone fades (engine scenery.rs, pop-in P3): a MeshTag with bit 31 carries the dither level as (tag & 63) - 16,
    // as the faithful FX shaders read it (fh1-render program.rs fx_vr_level); otherwise Bevy's distance level.
    let tag = mesh_functions::get_tag(v.instance_index);
    if (tag & 0x80000000u) != 0u {
        out.visibility_range_dither = i32(tag & 63u) - 16;
    } else {
        out.visibility_range_dither = mesh_functions::get_visibility_range_dither_level(v.instance_index, world_from_local[3]);
    }
#endif
    return out;
}

const LAYER_SPLAT: u32 = 1u;
const LAYER_ROAD: u32 = 2u;
const LAYER_VBLEND: u32 = 3u;
const CLASS_CUTOUT: u32 = 1u;
const CLASS_DECAL: u32 = 2u;
const CLASS_WATER: u32 = 4u;
const CLASS_ADDITIVE: u32 = 5u;
const CLASS_UNLIT: u32 = 7u;
const FLAG_NIGHT_EMISSIVE: u32 = 4u;
const FLAG_OBJECT_TINT: u32 = 16u;
const FLAG_REFLECTIVE: u32 = 32u;
const FLAG_LM_BAKED: u32 = 256u;
const FLAG_ROAD: u32 = 512u;
const FLAG_TREE: u32 = 1024u;
const FLAG_DECAL_MASK: u32 = 0x8000u;

fn has(p: SceneryParams, role: u32) -> bool {
    return (p.info.y & (1u << role)) != 0u;
}

fn uv_of(scale: vec2<f32>, uvset: u32, uv0: vec2<f32>, uv1: vec2<f32>, uv2: vec2<f32>) -> vec2<f32> {
    var uv = uv0;
    if uvset == 1u {
        uv = uv1;
    } else if uvset >= 2u {
        uv = uv2;
    }
    return uv * scale;
}

fn set_of(p: SceneryParams, role: u32) -> u32 {
    return (p.info.x >> (2u * role)) & 3u;
}

fn road_min_roughness() -> f32 {
    return RM_ROAD_ROUGHNESS;
}

fn lum(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.33));
}

/// Tangent-space normal (DXN two-channel or RGB) -> world, via the screen-space cotangent frame.
fn decode_normal(t: vec4<f32>) -> vec3<f32> {
    let xy = t.xy * 2.0 - 1.0;
    return vec3<f32>(xy, sqrt(max(1.0 - dot(xy, xy), 0.0)));
}

@fragment
fn fragment(in: Out, @builtin(front_facing) is_front: bool) -> FragmentOutput {
#ifdef VISIBILITY_RANGE_DITHER
    pbr_functions::visibility_range_dither(in.position, in.visibility_range_dither);
#endif
#ifdef INSTANCE_LOD
    if batch_lod_discard(in.lod_fades, in.position.xy) {
        discard;
    }
#endif
    let slot = mesh[in.instance_index].material_and_lightmap_bind_group_slot & 0xffffu;
#ifdef BINDLESS
    let p = scenery_params[scenery_indices[slot].material];
    var smat = pbr_bindings::material_array[pbr_bindings::material_indices[slot].material];
#else
    let p = scenery_params;
    var smat = pbr_bindings::material;
#endif
    let cls = (p.info.z >> 16u) & 0xffu;
    let flags = p.info.z & 0xffffu;
    let layering = p.info.w;
    let u0 = in.uv01.xy;
    let u1 = in.uv01.zw;
    let u2 = in.uv2;

    // Derivatives in uniform control flow; every fetch below uses them (sampling inside branches stays valid).
    let ua = uv_of(p.uv[0].xy, set_of(p, 0u), u0, u1, u2);
    let ub = uv_of(p.uv[0].zw, set_of(p, 1u), u0, u1, u2);
    let uc = uv_of(p.uv[1].xy, set_of(p, 2u), u0, u1, u2);
    let uw = uv_of(p.uv[1].zw, set_of(p, 3u), u0, u1, u2);
    let un = uv_of(p.uv[2].xy, set_of(p, 4u), u0, u1, u2);
    let unb = uv_of(p.uv[2].zw, set_of(p, 5u), u0, u1, u2);
    let umod = uv_of(p.uv[3].xy, set_of(p, 6u), u0, u1, u2);
    let uao = uv_of(p.uv[3].zw, set_of(p, 7u), u0, u1, u2);
    let ulm = uv_of(p.uv[4].xy, set_of(p, 8u), u0, u1, u2);
    let uem = uv_of(p.uv[4].zw, set_of(p, 9u), u0, u1, u2);
    let usp = uv_of(p.uv[5].xy, set_of(p, 10u), u0, u1, u2);
    let umask = uv_of(p.uv[5].zw, set_of(p, 11u), u0, u1, u2);
    let dxa = dpdx(ua); let dya = dpdy(ua);
    let dxb = dpdx(ub); let dyb = dpdy(ub);
    let dxc = dpdx(uc); let dyc = dpdy(uc);
    let dxw = dpdx(uw); let dyw = dpdy(uw);
    let dxn = dpdx(un); let dyn = dpdy(un);
    let dxnb = dpdx(unb); let dynb = dpdy(unb);
    let dxm = dpdx(umod); let dym = dpdy(umod);
    let dxo = dpdx(uao); let dyo = dpdy(uao);
    let dxl = dpdx(ulm); let dyl = dpdy(ulm);
    let dxe = dpdx(uem); let dye = dpdy(uem);
    let dxs = dpdx(usp); let dys = dpdy(usp);
    let dxk = dpdx(umask); let dyk = dpdy(umask);
    let dpx = dpdx(in.world_position.xyz);
    let dpy = dpdy(in.world_position.xyz);

    // Base colour + specular mask (the game's luminance x c5 per layer).
    var a = vec4<f32>(0.5, 0.5, 0.5, 1.0);
    if has(p, 0u) {
        a = sample_a(slot, ua, dxa, dya);
    }
    var col = a.rgb;
    var spec = lum(a.rgb) * p.p[2].x;
    var alpha = a.a;
    let vc = in.color;
    if layering == LAYER_SPLAT {
        var w = vec4<f32>(0.0);
        if has(p, 3u) {
            w = sample_weight(slot, uw, dxw, dyw);
        }
        if has(p, 1u) {
            let b = sample_b(slot, ub, dxb, dyb);
            col = mix(col, b.rgb, saturate(w.r));
            spec = mix(spec, lum(b.rgb) * p.p[2].y, saturate(w.r));
        }
        if has(p, 2u) {
            let c = sample_c(slot, uc, dxc, dyc);
            let wc = saturate(w.g) * c.a;
            col = mix(col, c.rgb, wc);
            spec = mix(spec, lum(c.rgb) * p.p[2].z, wc);
        }
    } else if layering == LAYER_ROAD || layering == LAYER_VBLEND {
        var noise = vec4<f32>(0.5);
        if has(p, 3u) {
            noise = sample_weight(slot, uw, dxw, dyw);
        }
        let c4 = p.p[1];
        var wb = saturate((vc.r - 0.5) * c4.x + 0.5);
        var wc = 0.0;
        if layering == LAYER_ROAD {
            wb = saturate((vc.r - 0.5) * c4.x + (noise.r - 0.5) * c4.y + 0.5);
            wc = saturate((vc.g - 0.5) * c4.z + (noise.g - 0.5) * c4.w + 0.5);
        }
        if has(p, 1u) {
            let b = sample_b(slot, ub, dxb, dyb);
            col = mix(col, b.rgb, wb);
            spec = mix(spec, lum(b.rgb) * p.p[2].y, wb);
        }
        if has(p, 2u) {
            let c = sample_c(slot, uc, dxc, dyc);
            col = mix(col, c.rgb, wc);
            spec = mix(spec, lum(c.rgb) * p.p[2].z, wc);
        }
        if has(p, 6u) {
            let m = sample_modulate(slot, umod, dxm, dym).rgb;
            col = col * mix(vec3<f32>(1.0), m, saturate(vc.b));
        }
    }
    col = col * p.p[0].x;
    if (flags & FLAG_OBJECT_TINT) != 0u {
        col = col * 2.0 * in.tint.rgb;
    }
    if has(p, 10u) {
        spec = spec * sample_specular(slot, usp, dxs, dys).g;
    }
    if has(p, 11u) {
        // Opacity maps: RGBA (alpha) or single-channel BC4 (red, alpha 1): min covers both.
        let mk = sample_mask(slot, umask, dxk, dyk);
        alpha = alpha * min(mk.r, mk.a);
    }
    if cls == CLASS_DECAL {
        if layering == LAYER_ROAD {
            // Road `_blend` (VERIFIED h_road_diff3_noise_blend_ao_lm): opacity = 1 - sat(vertex colour b).
            alpha = 1.0 - saturate(vc.b);
        } else if (flags & FLAG_LM_BAKED) == 0u {
            // vblnd decals: texture alpha x vertex alpha (INFERRED, as the faithful fallback). Not FM4 (unknown there).
            alpha = alpha * vc.a;
        }
    }

    var pbr = pbr_types::pbr_input_new();
    pbr.material = smat;
    pbr.flags = mesh[in.instance_index].flags;
    pbr.is_orthographic = view.clip_from_view[3].w == 1.0;
    pbr.V = pbr_functions::calculate_view(in.world_position, pbr.is_orthographic);
    pbr.frag_coord = in.position;
    pbr.world_position = in.world_position;
    let double_sided = (smat.flags & pbr_types::STANDARD_MATERIAL_FLAGS_DOUBLE_SIDED_BIT) != 0u;
    if (flags & FLAG_TREE) != 0u {
        // No back-face flip for tree cards: the foliage normal is per crown, not per card.
        pbr.world_normal = normalize(in.world_normal);
    } else {
        pbr.world_normal = pbr_functions::prepare_world_normal(in.world_normal, double_sided, is_front);
    }
    var n = normalize(pbr.world_normal);
    if has(p, 4u) {
        // Cotangent frame from screen derivatives (no tangent attribute needed).
        let dp2perp = cross(dpy, n);
        let dp1perp = cross(n, dpx);
        let t = dp2perp * dxn.x + dp1perp * dyn.x;
        let b = dp2perp * dxn.y + dp1perp * dyn.y;
        let inv = inverseSqrt(max(max(dot(t, t), dot(b, b)), 1e-12));
        var ts = decode_normal(sample_normal(slot, un, dxn, dyn));
        if has(p, 5u) {
            ts = normalize(vec3<f32>(ts.xy + decode_normal(sample_normal_b(slot, unb, dxnb, dynb)).xy, ts.z));
        }
        ts = vec3<f32>(ts.xy * p.p[0].w, ts.z);
        n = normalize(t * inv * ts.x - b * inv * ts.y + n * ts.z);
    }
    pbr.N = n;

    // Specular: the game's Blinn-Phong power -> GGX roughness, level x mask -> reflectance (remaster rule).
    let level = p.p[0].y;
    let power = p.p[0].z;
    if level > 0.0 && power > 0.0 {
        let alpha_r = sqrt(2.0 / (power + 2.0));
        pbr.material.perceptual_roughness = clamp(sqrt(alpha_r), 0.08, 1.0);
        pbr.material.reflectance = vec3<f32>(mix(0.25, 0.6, saturate(level * spec * 2.0)));
    } else {
        pbr.material.perceptual_roughness = 0.95;
        pbr.material.reflectance = vec3<f32>(0.25);
    }
    if layering == LAYER_ROAD || layering == LAYER_VBLEND || (flags & FLAG_ROAD) != 0u {
        // Dry tarmac / ground (user, 2026-10-06: road reflections too intense): F0 <= ~0.04 (Bevy F0 = 0.16 r^2,
        // r <= 0.5) and roughness >= 0.6. The game's power-80 sheen came from its own lighting model, not a mirror.
        pbr.material.perceptual_roughness = max(pbr.material.perceptual_roughness, road_min_roughness());
        pbr.material.reflectance = min(pbr.material.reflectance, vec3<f32>(0.4));
    }
    // Reflective (glass, metal) but never road / ground: this clamp ran after the road one and made some road
    // surfaces mirror-like again (user 2026-10-07: "some road surfaces are still very shiny").
    let is_road = layering == LAYER_ROAD || layering == LAYER_VBLEND || (flags & FLAG_ROAD) != 0u;
    if (flags & FLAG_REFLECTIVE) != 0u && !is_road {
        pbr.material.perceptual_roughness = min(pbr.material.perceptual_roughness, 0.35);
    }

    // AO: the game's AO x 0.95 + 0.05 (green channel).
    if has(p, 7u) {
        let ao = sample_ao(slot, uao, dxo, dyo).g * 0.95 + 0.05;
        pbr.diffuse_occlusion = vec3<f32>(ao);
        pbr.specular_occlusion = ao;
    }

    // Night: baked lightmap (x albedo) and emissive maps.
    var emissive = vec3<f32>(0.0);
    let lm_w = max(p.night.x, p.p[3].x);
    if has(p, 8u) && (flags & FLAG_LM_BAKED) != 0u {
        // FM4: the daytime baked track lightmap darkens the diffuse light (shadows / AO), it doesn't emit.
        pbr.diffuse_occlusion = pbr.diffuse_occlusion * sample_lightmap(slot, ulm, dxl, dyl).rgb;
    } else if has(p, 8u) && lm_w > 0.0 {
        emissive += col * sample_lightmap(slot, ulm, dxl, dyl).rgb * lm_w;
    }
    if has(p, 9u) && (flags & FLAG_NIGHT_EMISSIVE) != 0u && p.night.y >= 0.5 {
        emissive += sample_emissive(slot, uem, dxe, dye).rgb * (p.night.w / max(p.night.z, 1e-4));
    }
    // Exposure weight 0 (.a): display-linear like the game, whatever EV the lighting owner picks.
    pbr.material.emissive = vec4<f32>(emissive * p.night.z, 0.0);

    if cls == CLASS_WATER {
        // Lakes and rivers (`lake_anim_norm_opac_refl_3`, docs/WATER.md): the game's two scrolling normal maps
        // (A: uv x c3.z + T x c3.yw, B: uv x c4.y + T x c4.xz), opacity = vertex colour G (0 at the shore) faded to
        // 1 by Fresnel (c0.y = 0.6), deep -> fresnel colour by Fresnel; the reflection is Bevy's environment light
        // (PBR, F0 0.02). Remaster choices: a teal depth tint instead of the game's near-black c1, a stronger bump.
        // Setup may carry the per-material constants (p3.y = 1: p1 = speeds, uv scales = tiling, p3.z = bump);
        // otherwise the values every installed lake material shares are used.
        let carried = p.p[3].y > 0.5;
        var tile = vec2<f32>(50.0, 30.0);
        var spd = vec4<f32>(0.02, 0.004, 0.0, 0.005);
        if carried {
            tile = vec2<f32>(1.0, 1.0);
            spd = p.p[1];
        }
        let tw = globals.time;
        let wn = normalize(pbr.world_normal);
        var nw = wn;
        if has(p, 4u) {
            let dp2perp = cross(dpy, wn);
            let dp1perp = cross(wn, dpx);
            let tt = dp2perp * dxn.x + dp1perp * dyn.x;
            let bb = dp2perp * dxn.y + dp1perp * dyn.y;
            let inv = inverseSqrt(max(max(dot(tt, tt), dot(bb, bb)), 1e-12));
            var xy = decode_normal(sample_normal(slot, un * tile.x + tw * spd.xy, dxn * tile.x, dyn * tile.x)).xy;
            if has(p, 5u) {
                xy = xy + decode_normal(sample_normal_b(slot, unb * tile.y + tw * spd.zw, dxnb * tile.y, dynb * tile.y)).xy;
            }
            xy = xy * RM_WATER_BUMP;
            nw = normalize(tt * inv * xy.x - bb * inv * xy.y + wn);
        } else {
            let t = tw * 0.02;
            let w1 = sin(in.world_position.x * 0.35 + t * 9.0) + sin(in.world_position.z * 0.27 - t * 7.0);
            let w2 = cos(in.world_position.x * 0.21 - t * 5.0) + cos(in.world_position.z * 0.41 + t * 8.0);
            nw = normalize(wn + vec3<f32>(w1, 0.0, w2) * 0.03);
        }
        pbr.N = nw;
        let fres = saturate(0.6 * (1.0 - abs(dot(nw, pbr.V))));
        // Vertex G = opacity (1 when the mesh has no colours: in.color defaults to white).
        alpha = mix(saturate(vc.g), 1.0, fres) * RM_WATER_OPACITY;
        alpha = max(alpha, RM_WATER_MIN_ALPHA * saturate(vc.g * 4.0));
        col = mix(vec3<f32>(0.010, 0.030, 0.032), vec3<f32>(0.035, 0.050, 0.060), fres);
        pbr.material.perceptual_roughness = 0.05;
        pbr.material.reflectance = vec3<f32>(0.35);
        pbr.diffuse_occlusion = vec3<f32>(1.0);
        if RM_WATER_FLAT > 0.5 {
            // Debug (FH1_RM_WATER=flat): bright opaque magenta, to tell "not drawn" from "drawn but invisible".
            col = vec3<f32>(1.0, 0.0, 1.0);
            alpha = 1.0;
        }
    }

    pbr.material.base_color = vec4<f32>(col, alpha);
    pbr.material.metallic = 0.0;
    if (cls == CLASS_CUTOUT || (flags & FLAG_DECAL_MASK) != 0u) && alpha < smat.alpha_cutoff {
        discard;
    }

    var out: FragmentOutput;
    if cls == CLASS_UNLIT || cls == CLASS_ADDITIVE {
        out.color = vec4<f32>(col, alpha);
    } else {
        out.color = pbr_functions::apply_pbr_lighting(pbr);
    }
    out.color = pbr_functions::main_pass_post_lighting_processing(pbr, out.color);
    return out;
}
"#;

/// Registers the material and its shader. Called from [`crate::scenery::plugin`].
pub fn plugin(app: &mut App) {
    let mut shaders = app.world_mut().resource_mut::<Assets<Shader>>();
    let _ = shaders.insert(&SHADER, Shader::from_wgsl(wgsl(), "fh1_remaster/scenery.wgsl"));
    app.add_plugins(MaterialPlugin::<RemasterMaterial>::default());
    app.init_resource::<RemasterNight>();
}
