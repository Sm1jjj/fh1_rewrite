//! Half-resolution effects pass (docs/PERF_P15_B.md; the "half-res effects with min/max depth, composited before
//! transparents" technique). Large, soft, low-frequency effects (tyre / volume smoke, the larger effects.zip particles,
//! backfire flames) are drawn into a half-res HDR target against a half-res depth and composited into the main HDR
//! target in ONE fullscreen pass before Bevy's transparent pass: a quarter of the fill (overdraw of big alpha-blended
//! quads near the camera is what makes smoke expensive) and no per-effect Bevy material meshes, mesh-allocator traffic or
//! transparent-phase items.
//!
//! Producers (smoke.rs, backfire.rs, particles.rs) push [`FxBatch`]es (world-space quads + their own WGSL, textures,
//! uniform bytes) into [`FxHalfRes`] in PostUpdate; the plugin clears it in First and the render world takes it at
//! extraction (no copy of the vertex arrays). Main camera only (the [`crate::post::FxPostCamera`] view).
//!
//! Render side (Core3d, [`FxHalfResSet`]: after the opaque and transmissive passes, before the transparent pass):
//! 1. `fx_half_depth`: the main depth (sample 0 when MSAA) -> a half-res Depth32Float, checkerboard min/max per 2x2
//!    (fx_half_res.wgsl `downsample`; reverse-Z: max = nearest on odd (x + y) texels, min = farthest on even ones).
//! 2. `fx_half_res`: every batch back to front (larger [`FxBatch::dist2`] first), one indexed draw each, into the half-res
//!    Rgba16Float (cleared to (0, 0, 0, 1): rgb = accumulated premultiplied light, a = transmittance) with depth test
//!    GreaterEqual against the half-res depth, no depth write. Colour blend per [`FxBlend`]; alpha tracks transmittance
//!    (Alpha / Premul: `a *= 1 - src.a`; Add / AddPremul: unchanged).
//! 3. `fx_half_composite`: `main = light + main * transmittance`, depth-aware upsample (bilinear where the 4 half-res
//!    depths agree with the full-res depth within `FH1_FX_HALF_DEPTH_TOL`, else the nearest-depth texel).
//!
//! All three are skipped when no batch is drawable this frame (zero GPU cost without effects on screen). The passes are
//! named for the perf recorder's "GPU time per render pass" table.
//!
//! Not done: the talk's "responsive mask in alpha" is for TAA (marks pixels the history must not keep); there is no TAA
//! here, and our alpha channel already carries the transmittance the composite needs, so it would cost a second target.
//!
//! `FH1_FX_HALF_RES=0` = the old behaviour (every effect on its own Bevy Material mesh at full res).
//! `FH1_FX_HALF_DEPTH_TOL=<x>`: relative view-distance tolerance of the bilinear upsample (default 0.1 = 10 %).

use bevy::core_pipeline::core_3d::{main_opaque_pass_3d, main_transparent_pass_3d};
use bevy::core_pipeline::schedule::{Core3d, Core3dSystems};
use bevy::mesh::VertexBufferLayout;
use bevy::prelude::*;
use bevy::render::camera::ExtractedCamera;
use bevy::render::diagnostic::RecordDiagnostics;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::globals::{GlobalsBuffer, GlobalsUniform};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{sampler, texture_2d, texture_depth_2d, texture_depth_2d_multisampled, uniform_buffer, uniform_buffer_sized};
use bevy::render::render_resource::encase::{internal::WriteInto, UniformBuffer};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, BindingResource, BlendComponent, BlendFactor, BlendOperation, BlendState,
    Buffer, BufferBinding, BufferDescriptor, BufferInitDescriptor, BufferSize, BufferUsages, CachedRenderPipelineId, ColorTargetState, ColorWrites,
    CompareFunction, DepthBiasState, DepthStencilState, Extent3d, FragmentState, IndexFormat, LoadOp, MultisampleState, Operations, PipelineCache,
    PrimitiveState, RenderPassColorAttachment, RenderPassDepthStencilAttachment, RenderPassDescriptor, RenderPipelineDescriptor, SamplerBindingType,
    ShaderStages, ShaderType, SpecializedRenderPipeline, SpecializedRenderPipelines, StencilState, StoreOp, TextureAspect, TextureDescriptor,
    TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureViewDescriptor, VertexFormat, VertexState, VertexStepMode,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::texture::{CachedTexture, FallbackImage, GpuImage, TextureCache};
use bevy::render::view::{ExtractedView, ViewDepthTexture, ViewTarget, ViewUniform, ViewUniformOffset, ViewUniforms};
use bevy::render::{ExtractSchedule, MainWorld, Render, RenderApp, RenderSystems};
use bevy::shader::ShaderDefVal;

/// Half-res effects on (`FH1_FX_HALF_RES`, default on; `=0` = the old full-res Material meshes).
pub fn on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_FX_HALF_RES").map_or(true, |v| v != "0"))
}

/// `FH1_FX_HALF_DEPTH_TOL`: relative view-distance difference under which the composite blends the 4 half-res texels
/// bilinearly (default 0.1); passed to the shader in per mille.
fn depth_tol_per_mille() -> u32 {
    static V: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let t: f32 = std::env::var("FH1_FX_HALF_DEPTH_TOL").ok().and_then(|v| v.parse().ok()).unwrap_or(0.1);
        (t.clamp(0.0, 10.0) * 1000.0).round() as u32
    })
}

/// The shader defs the half-res pipeline adds to an effect's own shader (vertex and fragment): `FX_HALF_RES` (swap
/// the Bevy mesh-view imports for `fh1_render::fx_half`, see fx_half.wgsl) and `MATERIAL_BIND_GROUP` = 1.
pub fn shader_defs() -> Vec<ShaderDefVal> {
    vec!["FX_HALF_RES".into(), ShaderDefVal::UInt("MATERIAL_BIND_GROUP".into(), 1)]
}

pub struct FxHalfResPlugin;

/// The Core3d system of the pass (all three passes run in one system, in order).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct FxHalfResSet;

impl Plugin for FxHalfResPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FxHalfRes>().add_systems(First, |mut fx: ResMut<FxHalfRes>| fx.batches.clear());
        // Always loaded: Bevy collects a shader's imports without evaluating #ifdef, so the effect shaders' Material
        // pipelines (FX_HALF_RES undefined) still wait for `fh1_render::fx_half` to exist.
        bevy::shader::load_shader_library!(app, "fx_half.wgsl");
        if !on() {
            return;
        }
        bevy::asset::embedded_asset!(app, "fx_half_res.wgsl");
        let shader: Handle<Shader> = bevy::asset::load_embedded_asset!(app, "fx_half_res.wgsl");
        app.add_plugins(ExtractComponentPlugin::<FxHalfResView>::default()).add_systems(Update, mark_views);
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app
            .insert_resource(FxPipelines::new(shader))
            .init_resource::<SpecializedRenderPipelines<FxPipelines>>()
            .init_resource::<FxFrame>()
            .add_systems(ExtractSchedule, extract_batches)
            .add_systems(Render, prepare.in_set(RenderSystems::PrepareBindGroups))
            .add_systems(
                Core3d,
                // After the transmissive pass too: effects used to be transparent-phase items, drawn after it.
                fx_half_res_passes
                    .after(main_opaque_pass_3d)
                    .after(bevy::pbr::main_transmissive_pass_3d)
                    // After the atmosphere sky pass (vendor/bevy_pbr patch 6), which would otherwise fog the smoke.
                    .after(bevy::pbr::render_sky)
                    .before(main_transparent_pass_3d)
                    .in_set(Core3dSystems::MainPass)
                    .in_set(FxHalfResSet),
            );
    }
}

/// Blend of one batch into the half-res target (colour; alpha tracks transmittance).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FxBlend {
    /// SrcAlpha, OneMinusSrcAlpha.
    Alpha,
    /// One, OneMinusSrcAlpha.
    Premul,
    /// SrcAlpha, One.
    Add,
    /// One, One.
    AddPremul,
}

impl FxBlend {
    /// Colour as named; alpha = transmittance: occluding blends multiply it by (1 - src.a), additive ones keep it.
    fn state(self) -> BlendState {
        use BlendFactor::*;
        let (src, dst, alpha_dst) = match self {
            FxBlend::Alpha => (SrcAlpha, OneMinusSrcAlpha, OneMinusSrcAlpha),
            FxBlend::Premul => (One, OneMinusSrcAlpha, OneMinusSrcAlpha),
            FxBlend::Add => (SrcAlpha, One, One),
            FxBlend::AddPremul => (One, One, One),
        };
        BlendState {
            color: BlendComponent { src_factor: src, dst_factor: dst, operation: BlendOperation::Add },
            alpha: BlendComponent { src_factor: Zero, dst_factor: alpha_dst, operation: BlendOperation::Add },
        }
    }
}

/// One quad corner; locations 0..4 = the effect shaders' POSITION / UV_0 / UV_1 / COLOR / TANGENT inputs
/// (Float32x3, Float32x2, Float32x2, Float32x4, Float32x4; 60 bytes, tightly packed).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct FxVertex {
    pub pos: [f32; 3],
    pub corner: [f32; 2],
    pub size_rot: [f32; 2],
    pub colour: [f32; 4],
    pub extra: [f32; 4],
}

const VERTEX_BYTES: usize = std::mem::size_of::<FxVertex>();
const _: () = assert!(VERTEX_BYTES == 60);

impl FxVertex {
    /// The vertices as bytes (fh1-render has no bytemuck).
    fn bytes(v: &[FxVertex]) -> &[u8] {
        // SAFETY: FxVertex is repr(C) of 15 f32s (no padding, every bit pattern valid).
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
    }
}

/// One effect draw: world-space quads (4 vertices each, indices 0,1,2 0,2,3 per quad, as particles.rs
/// `quad_indices`) through the effect's own shader (vertex entry `vertex`, fragment entry `fragment`).
#[derive(Clone)]
pub struct FxBatch {
    pub shader: Handle<Shader>,
    pub blend: FxBlend,
    /// Material bindings 0 + 1 (texture + its own sampler); None = 1x1 white.
    pub tex0: Option<Handle<Image>>,
    /// Material bindings 3 + 4; None = 1x1 white.
    pub tex1: Option<Handle<Image>>,
    /// Uniform bytes for material binding 2 (std140, see [`FxBatch::uniform`]).
    pub params: Vec<u8>,
    pub verts: Vec<FxVertex>,
    /// Squared camera distance; larger draws first (back to front over all batches).
    pub dist2: f32,
}

impl FxBatch {
    /// `v` as uniform-buffer bytes (encase layout).
    pub fn uniform<T: ShaderType + WriteInto>(v: &T) -> Vec<u8> {
        let mut b = UniformBuffer::new(Vec::<u8>::new());
        b.write(v).expect("fx_half_res uniform");
        b.into_inner()
    }
}

/// This frame's batches (producers push in PostUpdate; cleared in First, taken by the render world at extraction).
#[derive(Resource, Default)]
pub struct FxHalfRes {
    batches: Vec<FxBatch>,
}

impl FxHalfRes {
    pub fn push(&mut self, b: FxBatch) {
        self.batches.push(b);
    }

    pub fn batches(&self) -> &[FxBatch] {
        &self.batches
    }
}

// ---------------------------------------------------------------- main world

/// Marks the main game camera (the FxPostCamera view) in both worlds.
#[derive(Component, Clone, Copy, Default, ExtractComponent)]
pub struct FxHalfResView;

/// Tags the FxPostCamera and makes its depth sampleable (the downsample and composite read it; Bevy's default is
/// RENDER_ATTACHMENT only). Idempotent: writes only when something is missing.
fn mark_views(mut commands: Commands, mut cams: Query<(Entity, &mut Camera3d, Has<FxHalfResView>), With<crate::post::FxPostCamera>>) {
    for (e, mut c, marked) in &mut cams {
        if !marked {
            commands.entity(e).insert(FxHalfResView);
        }
        let u = TextureUsages::from(c.depth_texture_usages);
        if !u.contains(TextureUsages::TEXTURE_BINDING) {
            c.depth_texture_usages = (u | TextureUsages::TEXTURE_BINDING).into();
        }
    }
}

// ---------------------------------------------------------------- render world

/// Takes the main world's batches (no copy of the vertex arrays).
fn extract_batches(mut main: ResMut<MainWorld>, mut frame: ResMut<FxFrame>) {
    frame.batches.clear();
    if let Some(mut fx) = main.get_resource_mut::<FxHalfRes>() {
        if !fx.batches.is_empty() {
            frame.batches = std::mem::take(&mut fx.batches);
        }
    }
}

/// Layouts and the downsample / composite shader; specialised per [`FxKey`].
#[derive(Resource)]
struct FxPipelines {
    shader: Handle<Shader>,
    /// Effect group 0: Bevy's view uniform (dynamic offset) + globals.
    effect_view: BindGroupLayoutDescriptor,
    /// Effect group 1: tex0, sampler0, params, tex1, sampler1.
    material: BindGroupLayoutDescriptor,
    /// [single-sample, multisampled] main depth.
    downsample: [BindGroupLayoutDescriptor; 2],
    composite: [BindGroupLayoutDescriptor; 2],
}

impl FxPipelines {
    fn new(shader: Handle<Shader>) -> Self {
        let vf = ShaderStages::VERTEX_FRAGMENT;
        let effect_view = BindGroupLayoutDescriptor::new("fx_half_res view", &BindGroupLayoutEntries::sequential(vf, (uniform_buffer::<ViewUniform>(true), uniform_buffer::<GlobalsUniform>(false))));
        let material = BindGroupLayoutDescriptor::new(
            "fx_half_res material",
            &BindGroupLayoutEntries::sequential(
                vf,
                (
                    texture_2d(TextureSampleType::Float { filterable: true }),
                    sampler(SamplerBindingType::Filtering),
                    uniform_buffer_sized(false, None),
                    texture_2d(TextureSampleType::Float { filterable: true }),
                    sampler(SamplerBindingType::Filtering),
                ),
            ),
        );
        let downsample = [
            BindGroupLayoutDescriptor::new("fx_half_depth", &BindGroupLayoutEntries::sequential(vf, (uniform_buffer::<ViewUniform>(true), texture_depth_2d()))),
            BindGroupLayoutDescriptor::new("fx_half_depth_ms", &BindGroupLayoutEntries::sequential(vf, (uniform_buffer::<ViewUniform>(true), texture_depth_2d_multisampled()))),
        ];
        let half_colour = || texture_2d(TextureSampleType::Float { filterable: false });
        let composite = [
            BindGroupLayoutDescriptor::new(
                "fx_half_composite",
                &BindGroupLayoutEntries::sequential(vf, (uniform_buffer::<ViewUniform>(true), texture_depth_2d(), half_colour(), texture_depth_2d())),
            ),
            BindGroupLayoutDescriptor::new(
                "fx_half_composite_ms",
                &BindGroupLayoutEntries::sequential(vf, (uniform_buffer::<ViewUniform>(true), texture_depth_2d_multisampled(), half_colour(), texture_depth_2d())),
            ),
        ];
        Self { shader, effect_view, material, downsample, composite }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum FxKey {
    /// An effect's own shader with a blend.
    Effect(Handle<Shader>, FxBlend),
    /// Depth downsample (main depth multisampled or not).
    Downsample { ms: bool },
    /// Composite into the main target (its sample count and format).
    Composite { samples: u32, format: TextureFormat },
}

const HALF_COLOUR: TextureFormat = TextureFormat::Rgba16Float;
const HALF_DEPTH: TextureFormat = TextureFormat::Depth32Float;

impl SpecializedRenderPipeline for FxPipelines {
    type Key = FxKey;

    fn specialize(&self, key: FxKey) -> RenderPipelineDescriptor {
        match key {
            FxKey::Effect(shader, blend) => {
                let defs = shader_defs();
                RenderPipelineDescriptor {
                    label: Some("fx_half_res effect".into()),
                    layout: vec![self.effect_view.clone(), self.material.clone()],
                    vertex: VertexState {
                        shader: shader.clone(),
                        shader_defs: defs.clone(),
                        entry_point: Some("vertex".into()),
                        buffers: vec![VertexBufferLayout::from_vertex_formats(
                            VertexStepMode::Vertex,
                            [VertexFormat::Float32x3, VertexFormat::Float32x2, VertexFormat::Float32x2, VertexFormat::Float32x4, VertexFormat::Float32x4],
                        )],
                    },
                    fragment: Some(FragmentState {
                        shader,
                        shader_defs: defs,
                        entry_point: Some("fragment".into()),
                        targets: vec![Some(ColorTargetState { format: HALF_COLOUR, blend: Some(blend.state()), write_mask: ColorWrites::ALL })],
                    }),
                    primitive: PrimitiveState { cull_mode: None, ..default() },
                    depth_stencil: Some(DepthStencilState {
                        format: HALF_DEPTH,
                        depth_write_enabled: Some(false),
                        depth_compare: Some(CompareFunction::GreaterEqual),
                        stencil: StencilState::default(),
                        bias: DepthBiasState::default(),
                    }),
                    ..default()
                }
            }
            FxKey::Downsample { ms } => {
                let defs: Vec<ShaderDefVal> = if ms { vec!["MULTISAMPLED".into()] } else { Vec::new() };
                RenderPipelineDescriptor {
                    label: Some("fx_half_depth".into()),
                    layout: vec![self.downsample[ms as usize].clone()],
                    vertex: VertexState { shader: self.shader.clone(), shader_defs: defs.clone(), entry_point: Some("vertex".into()), buffers: Vec::new() },
                    fragment: Some(FragmentState { shader: self.shader.clone(), shader_defs: defs, entry_point: Some("downsample".into()), targets: Vec::new() }),
                    depth_stencil: Some(DepthStencilState {
                        format: HALF_DEPTH,
                        depth_write_enabled: Some(true),
                        depth_compare: Some(CompareFunction::Always),
                        stencil: StencilState::default(),
                        bias: DepthBiasState::default(),
                    }),
                    ..default()
                }
            }
            FxKey::Composite { samples, format } => {
                let ms = samples > 1;
                let mut defs: Vec<ShaderDefVal> = vec!["COMPOSITE".into(), ShaderDefVal::UInt("FX_HALF_DEPTH_TOL".into(), depth_tol_per_mille())];
                if ms {
                    defs.push("MULTISAMPLED".into());
                }
                RenderPipelineDescriptor {
                    label: Some("fx_half_composite".into()),
                    layout: vec![self.composite[ms as usize].clone()],
                    vertex: VertexState { shader: self.shader.clone(), shader_defs: defs.clone(), entry_point: Some("vertex".into()), buffers: Vec::new() },
                    fragment: Some(FragmentState {
                        shader: self.shader.clone(),
                        shader_defs: defs,
                        entry_point: Some("composite".into()),
                        targets: vec![Some(ColorTargetState {
                            format,
                            // main = light + main x transmittance; the main alpha is kept.
                            blend: Some(BlendState {
                                color: BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::SrcAlpha, operation: BlendOperation::Add },
                                alpha: BlendComponent { src_factor: BlendFactor::Zero, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                            }),
                            write_mask: ColorWrites::ALL,
                        })],
                    }),
                    multisample: MultisampleState { count: samples, ..default() },
                    ..default()
                }
            }
        }
    }
}

/// Per main view and frame: the half-res targets and the fullscreen pipelines for its depth / target.
#[derive(Component)]
struct FxViewTargets {
    colour: CachedTexture,
    depth: CachedTexture,
    downsample: CachedRenderPipelineId,
    composite: CachedRenderPipelineId,
}

/// One batch's draw.
struct FxDraw {
    pipeline: CachedRenderPipelineId,
    bind_group: BindGroup,
    base_vertex: i32,
    quads: u32,
}

/// Render-world frame state: this frame's batches, the shared grow-only buffers, the draws.
#[derive(Resource, Default)]
struct FxFrame {
    batches: Vec<FxBatch>,
    vertex: Option<Buffer>,
    vertex_cap: u64,
    /// Implicit quads 0..index_quads (0,1,2 0,2,3); every batch draws from 0 with its own base vertex.
    index: Option<Buffer>,
    index_quads: u32,
    uniform: Option<Buffer>,
    uniform_cap: u64,
    /// Effect group 0 (view uniform at the view's dynamic offset + globals).
    view_bind_group: Option<BindGroup>,
    draws: Vec<FxDraw>,
    /// Uniform staging, reused.
    staging: Vec<u8>,
}

/// Half-res targets + pipelines per main view; batches sorted back to front, uploaded (one vertex buffer, one uniform
/// buffer at aligned offsets), one material bind group each. A batch whose image isn't on the GPU yet is skipped.
#[allow(clippy::too_many_arguments)]
fn prepare(
    mut commands: Commands,
    mut frame: ResMut<FxFrame>,
    pipes: Res<FxPipelines>,
    mut specialized: ResMut<SpecializedRenderPipelines<FxPipelines>>,
    cache: Res<PipelineCache>,
    (device, queue): (Res<RenderDevice>, Res<RenderQueue>),
    mut textures: ResMut<TextureCache>,
    (images, fallback): (Res<RenderAssets<GpuImage>>, Res<FallbackImage>),
    (view_uniforms, globals): (Res<ViewUniforms>, Res<GlobalsBuffer>),
    views: Query<(Entity, &ExtractedView, &ViewTarget, &ViewDepthTexture), With<FxHalfResView>>,
) {
    let frame = &mut *frame;
    frame.draws.clear();
    frame.view_bind_group = None;
    // Targets are requested every frame (TextureCache drops what isn't asked for), so the first smoke puff of a session
    // doesn't allocate.
    for (e, view, target, depth) in &views {
        let size = Extent3d { width: view.viewport.z.div_ceil(2).max(1), height: view.viewport.w.div_ceil(2).max(1), depth_or_array_layers: 1 };
        let mut get = |label: &'static str, format: TextureFormat| {
            textures.get(
                &device,
                TextureDescriptor {
                    label: Some(label),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format,
                    usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };
        let colour = get("fx_half_res colour", HALF_COLOUR);
        let half_depth = get("fx_half_res depth", HALF_DEPTH);
        let samples = depth.texture.sample_count();
        let downsample = specialized.specialize(&cache, &pipes, FxKey::Downsample { ms: samples > 1 });
        let composite = specialized.specialize(&cache, &pipes, FxKey::Composite { samples, format: target.main_texture_format() });
        commands.entity(e).insert(FxViewTargets { colour, depth: half_depth, downsample, composite });
    }
    let mut batches = std::mem::take(&mut frame.batches);
    batches.retain(|b| b.verts.len() >= 4);
    if batches.is_empty() || views.is_empty() {
        return;
    }
    let (Some(vu), Some(gu)) = (view_uniforms.uniforms.binding(), globals.buffer.binding()) else { return };
    frame.view_bind_group = Some(device.create_bind_group("fx_half_res view", &cache.get_bind_group_layout(&pipes.effect_view), &BindGroupEntries::sequential((vu, gu))));
    // Back to front over all batches.
    batches.sort_by(|a, b| b.dist2.total_cmp(&a.dist2));

    // Vertices: one grow-only buffer, each batch at its own base vertex.
    let total: usize = batches.iter().map(|b| b.verts.len() / 4 * 4).sum();
    let bytes = (total * VERTEX_BYTES) as u64;
    if frame.vertex.is_none() || frame.vertex_cap < bytes {
        let cap = bytes.next_power_of_two().max(1 << 16);
        frame.vertex = Some(device.create_buffer(&BufferDescriptor { label: Some("fx_half_res vertices"), size: cap, usage: BufferUsages::VERTEX | BufferUsages::COPY_DST, mapped_at_creation: false }));
        frame.vertex_cap = cap;
    }
    // Shared quad indices for the largest batch.
    let max_quads = batches.iter().map(|b| b.verts.len() / 4).max().unwrap_or(0) as u32;
    if frame.index.is_none() || frame.index_quads < max_quads {
        let quads = max_quads.next_power_of_two().max(1024);
        let mut idx: Vec<u32> = Vec::with_capacity(quads as usize * 6);
        for q in 0..quads {
            let b = q * 4;
            idx.extend_from_slice(&[b, b + 1, b + 2, b, b + 2, b + 3]);
        }
        // SAFETY: u32 slice as bytes.
        let raw = unsafe { std::slice::from_raw_parts(idx.as_ptr() as *const u8, std::mem::size_of_val(idx.as_slice())) };
        frame.index = Some(device.create_buffer_with_data(&BufferInitDescriptor { label: Some("fx_half_res quad indices"), contents: raw, usage: BufferUsages::INDEX }));
        frame.index_quads = quads;
    }
    // Uniforms: every batch's bytes at a min_uniform_buffer_offset_alignment offset (16-byte padded, at least 16).
    let align = (device.limits().min_uniform_buffer_offset_alignment as usize).max(16);
    let slot = |b: &FxBatch| b.params.len().max(16).next_multiple_of(16);
    let mut offsets = Vec::with_capacity(batches.len());
    frame.staging.clear();
    for b in &batches {
        let at = frame.staging.len().next_multiple_of(align);
        frame.staging.resize(at, 0);
        offsets.push(at as u64);
        frame.staging.extend_from_slice(&b.params);
        frame.staging.resize(at + slot(b), 0);
    }
    let ubytes = frame.staging.len() as u64;
    if frame.uniform.is_none() || frame.uniform_cap < ubytes {
        let cap = ubytes.next_power_of_two().max(4096);
        frame.uniform = Some(device.create_buffer(&BufferDescriptor { label: Some("fx_half_res params"), size: cap, usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST, mapped_at_creation: false }));
        frame.uniform_cap = cap;
    }
    let (Some(vb), Some(ub)) = (frame.vertex.clone(), frame.uniform.clone()) else { return };
    queue.write_buffer(&ub, 0, &frame.staging);
    let material_layout = cache.get_bind_group_layout(&pipes.material);
    let white = (&fallback.d2.texture_view, &fallback.d2.sampler);
    let mut base = 0usize;
    for (b, &offset) in batches.iter().zip(&offsets) {
        let n = b.verts.len() / 4 * 4;
        let verts = &b.verts[..n];
        queue.write_buffer(&vb, (base * VERTEX_BYTES) as u64, FxVertex::bytes(verts));
        let first = base;
        base += n;
        let tex = |h: &Option<Handle<Image>>| match h {
            Some(h) => images.get(h).map(|g| (&g.texture_view, &g.sampler)),
            None => Some(white),
        };
        let (Some(t0), Some(t1)) = (tex(&b.tex0), tex(&b.tex1)) else { continue };
        let params = BindingResource::Buffer(BufferBinding { buffer: &ub, offset, size: BufferSize::new(slot(b) as u64) });
        let bind_group = device.create_bind_group("fx_half_res material", &material_layout, &BindGroupEntries::sequential((t0.0, t0.1, params, t1.0, t1.1)));
        let pipeline = specialized.specialize(&cache, &pipes, FxKey::Effect(b.shader.clone(), b.blend));
        frame.draws.push(FxDraw { pipeline, bind_group, base_vertex: first as i32, quads: (n / 4) as u32 });
    }
}

/// The three passes on the main view (skipped without drawable batches or before the pipelines / sampleable depth exist).
#[allow(clippy::too_many_arguments)]
fn fx_half_res_passes(
    view: ViewQuery<(&ExtractedCamera, &ViewTarget, &ViewDepthTexture, &ViewUniformOffset, Option<&FxViewTargets>), With<FxHalfResView>>,
    frame: Res<FxFrame>,
    pipes: Res<FxPipelines>,
    cache: Res<PipelineCache>,
    view_uniforms: Res<ViewUniforms>,
    mut ctx: RenderContext,
) {
    let (camera, target, depth, view_offset, half) = view.into_inner();
    let Some(half) = half else { return };
    if frame.draws.is_empty() || !frame.draws.iter().any(|d| cache.get_render_pipeline(d.pipeline).is_some()) {
        return;
    }
    let (Some(vb), Some(ib), Some(view_bg), Some(vu)) = (frame.vertex.as_ref(), frame.index.as_ref(), frame.view_bind_group.as_ref(), view_uniforms.uniforms.binding()) else { return };
    if !depth.texture.usage().contains(TextureUsages::TEXTURE_BINDING) {
        return;
    }
    let (Some(down), Some(comp)) = (cache.get_render_pipeline(half.downsample), cache.get_render_pipeline(half.composite)) else { return };
    let ms = (depth.texture.sample_count() > 1) as usize;
    let device = ctx.render_device().clone();
    let full_depth = depth.texture.create_view(&TextureViewDescriptor { aspect: TextureAspect::DepthOnly, ..default() });
    let down_bg = device.create_bind_group("fx_half_depth", &cache.get_bind_group_layout(&pipes.downsample[ms]), &BindGroupEntries::sequential((vu.clone(), &full_depth)));
    let comp_bg = device.create_bind_group(
        "fx_half_composite",
        &cache.get_bind_group_layout(&pipes.composite[ms]),
        &BindGroupEntries::sequential((vu, &full_depth, &half.colour.default_view, &half.depth.default_view)),
    );
    let recorder = ctx.diagnostic_recorder();
    let diagnostics = recorder.as_deref();
    // 1. Half-res min/max depth.
    {
        let mut pass = ctx.begin_tracked_render_pass(RenderPassDescriptor {
            label: Some("fx_half_depth"),
            color_attachments: &[],
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: &half.depth.default_view,
                depth_ops: Some(Operations { load: LoadOp::Clear(0.0), store: StoreOp::Store }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let span = diagnostics.pass_span(&mut pass, "fx_half_depth");
        pass.set_render_pipeline(down);
        pass.set_bind_group(0, &down_bg, &[view_offset.offset]);
        pass.draw(0..3, 0..1);
        span.end(&mut pass);
    }
    // 2. The effects, back to front.
    {
        let colour = [Some(RenderPassColorAttachment {
            view: &half.colour.default_view,
            depth_slice: None,
            resolve_target: None,
            ops: Operations { load: LoadOp::Clear(LinearRgba::new(0.0, 0.0, 0.0, 1.0).into()), store: StoreOp::Store },
        })];
        let mut pass = ctx.begin_tracked_render_pass(RenderPassDescriptor {
            label: Some("fx_half_res"),
            color_attachments: &colour,
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: &half.depth.default_view,
                depth_ops: Some(Operations { load: LoadOp::Load, store: StoreOp::Store }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let span = diagnostics.pass_span(&mut pass, "fx_half_res");
        pass.set_vertex_buffer(0, vb.slice(..));
        pass.set_index_buffer(ib.slice(..), IndexFormat::Uint32);
        pass.set_bind_group(0, view_bg, &[view_offset.offset]);
        for d in &frame.draws {
            let Some(p) = cache.get_render_pipeline(d.pipeline) else { continue };
            pass.set_render_pipeline(p);
            pass.set_bind_group(1, &d.bind_group, &[]);
            pass.draw_indexed(0..d.quads * 6, d.base_vertex, 0..1);
        }
        span.end(&mut pass);
    }
    // 3. Depth-aware upsample into the main HDR target.
    {
        let colour = [Some(target.get_color_attachment())];
        let mut pass = ctx.begin_tracked_render_pass(RenderPassDescriptor {
            label: Some("fx_half_composite"),
            color_attachments: &colour,
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some(viewport) = camera.viewport.as_ref() {
            pass.set_camera_viewport(viewport);
        }
        let span = diagnostics.pass_span(&mut pass, "fx_half_composite");
        pass.set_render_pipeline(comp);
        pass.set_bind_group(0, &comp_bg, &[view_offset.offset]);
        pass.draw(0..3, 0..1);
        span.end(&mut pass);
    }
}
