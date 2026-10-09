//! P12 static world drawing (docs/PERF.md "P12 static world").
//!
//! Chunk 2: every static record is drawn through the remaster scenery shader (STATIC_WORLD variant) with Bevy's own
//! bindless RemasterMaterial bind groups, binned by (material bind group, cull mode, alpha test).
//! Chunk 3: GPU culling. The CPU keeps one candidate list (record slots in bin order), rebuilt only when the arena
//! changes (streaming). Per view and frame, a compute pass tests every candidate (live / hidden flags, the record's LOD
//! band from the LOD eye, frustum vs world AABB, per-kind rules) and writes its `DrawIndexedIndirect` args into the view's
//! own region with instance_count 1 or 0; each bin is then one `multi_draw_indexed_indirect`. Per-frame CPU work is
//! O(views x bins), independent of the record count.
//! Hi-Z (chunk 5, 47; static_world/hiz.rs, `FH1_STATIC_WORLD_HIZ=0` = off): the main camera culls twice. Phase 1 draws
//! what was visible last frame, the main depth is reduced into a min-depth pyramid, phase 2 tests every frustum / LOD-
//! passing candidate's projected box against it, stores the visibility bit and draws the newly visible ones (own args
//! region / draw counts). Only with a single-sample main depth that has TEXTURE_BINDING (light.rs `hiz_depth_usage`).
//! Compaction (chunk 5, default when the GPU has MULTI_DRAW_INDIRECT_COUNT; `FH1_STATIC_WORLD_COMPACT=0` = off): visible
//! candidates are appended per (view, bin) with an atomic counter and each bin is drawn with
//! `multi_draw_indexed_indirect_count`, so culled entries cost nothing (no zero-instance draws for the command processor).
//! Chunk 4: the same cull feeds (a) the car probe faces (fh1-remaster car_probe.rs `CarProbeFace`, 47's rules: radius >=
//! `probe_min_radius()`, within the face's far plane) and (b) a depth-only pass into each directional shadow cascade of the
//! main camera after Bevy's shadow pass (records with the casts flag; small casters (half-diagonal < 1.5 m) only within
//! FH1_SHADOW_SMALL_DIST of the main camera; cascades in `DirectionalShadowSkipThisFrame` are left alone; cutouts
//! alpha-tested on layer A). `FH1_STATIC_WORLD_CULL=0` = no culling (chunk 2), `FH1_STATIC_WORLD_SHADOWS=0` = no static
//! shadows, `FH1_STATIC_WORLD_PROBE=0` = static scenery not in the probe.
//! P15-A (docs/PERF_P15_A.md; needs culling + compaction): every (group, cull, mask) bin is split into draw classes that
//! the cull picks per candidate and frame, and the bins are drawn class by class: near cutouts (write their own depth
//! first; never pre-passed), pre-passed near occluders, other near opaque, far opaque, far cutouts
//! (`FH1_SW_DRAW_ORDER=0` = old group-major order). A depth-only, position-only pre-pass of the big near opaque occluders
//! (ground / road tiles, buildings: fully visible, no cloth, within FH1_SW_PREPASS_DIST) runs before Bevy's main opaque
//! pass into the main depth, so the lit passes depth-reject what they hide and the Hi-Z pyramid (built from the same
//! depth) sees them (`FH1_SW_PREPASS=0` = off). Cascades dither the LOD / zone fades like the main pass: in-band opaque
//! records go to a dithering class, cutouts dither in their alpha-test stage (`FH1_SW_SHADOW_DITHER=0` = both levels
//! cast fully, as before).

use std::any::TypeId;
use std::collections::HashMap;

use bevy::core_pipeline::core_3d::{main_opaque_pass_3d, CORE_3D_DEPTH_FORMAT};
use bevy::core_pipeline::schedule::{Core3d, Core3dSystems};
use bevy::pbr::{
    DirectionalShadowSkipThisFrame, LightEntity, MaterialBindGroupAllocators, MaterialBindGroupIndex, MeshPipeline, MeshPipelineKey, MeshViewBindGroup,
    RenderMaterialBindings, ShadowView, ViewKeyCache, ViewLightEntities,
};
use bevy::prelude::*;
use bevy::render::camera::ExtractedCamera;
use bevy::render::mesh::{MeshVertexBufferLayoutRef, MeshVertexBufferLayouts};
use bevy::render::render_resource::binding_types::{storage_buffer_read_only_sized, storage_buffer_sized, uniform_buffer, uniform_buffer_sized};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, Buffer, BufferDescriptor, BufferUsages, CachedComputePipelineId,
    CachedRenderPipelineId, CompareFunction, ComputePass, ComputePassDescriptor, ComputePipeline, ComputePipelineDescriptor, DepthBiasState, DepthStencilState, DynamicUniformBuffer,
    Face, FragmentState, IndexFormat, PipelineCache, PrimitiveState, RenderPassDescriptor, RenderPipelineDescriptor, ShaderStages, ShaderType,
    SpecializedMeshPipeline, SpecializedRenderPipeline, SpecializedRenderPipelines, StencilState, StoreOp, VertexState,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::settings::WgpuFeatures;
use bevy::render::view::{ExtractedView, ViewDepthTexture, ViewTarget};
use bevy::render::diagnostic::RecordDiagnostics;
use bevy::render::{Render, RenderSystems};
use bevy::shader::ShaderDefVal;

use super::{Arena, FLAG_HIDDEN, FLAG_LIVE, FLAG_MASK, FLAG_MIRRORED, FLAG_TWO_SIDED};
use crate::car_probe::CarProbeFace;
use crate::material::RemasterMaterial;

/// Cull compute shader and shadow depth shader (registered by `register_shaders`).
pub(super) const CULL_SHADER: Handle<Shader> = bevy::asset::uuid_handle!("7c1f3a52-9b0e-4d61-a8c2-5e4f90d1b3a7");
pub(super) const SHADOW_SHADER: Handle<Shader> = bevy::asset::uuid_handle!("3e8b6d14-2a7f-4c95-b0e1-9d7c5a2f6e48");

/// Render-world counters for the perf CSV (static_world::render_stats): candidates, views culled this frame.
pub(super) static CANDIDATES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub(super) static VIEWS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Views drawn per frame at most (main, probe face, cascades); each owns an args region.
const MAX_VIEWS: u32 = 12;

fn flag_on(k: &str) -> bool {
    std::env::var(k).map_or(true, |v| v != "0")
}

fn cull_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_STATIC_WORLD_CULL"))
}

fn shadows_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_STATIC_WORLD_SHADOWS"))
}

fn compact_wanted() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_STATIC_WORLD_COMPACT"))
}

fn probe_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_STATIC_WORLD_PROBE"))
}

/// P15-A draw classes / class-ordered bins (needs culling + compaction).
fn order_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_SW_DRAW_ORDER"))
}

/// P15-A partial depth pre-pass of the near occluders (needs the draw classes and bindless materials).
fn prepass_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_SW_PREPASS"))
}

/// P15-A LOD / zone fade dither in the cascades.
fn shadow_dither_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_SW_SHADOW_DITHER"))
}

/// P16: cascade pipelines that don't sample textures (opaque bins) leave the material bind group out, and the cascade
/// pass draws them first, then the cutout bins slab by slab: a bindless slab bind costs the render thread a walk over
/// its hundreds of texture views (wgpu usage tracking + memory-init checks), once per bind per pass.
fn shadow_nomat_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_SW_SHADOW_NOMAT"))
}

/// P16-A: fewer compute passes. The cascades' culls run as one compute pass (one dispatch per cascade), and the Hi-Z
/// pyramid + the phase-2 cull run as one compute pass with cached bind groups. wgpu-core resets a usage scope sized to
/// every buffer / texture alive on the device for each pass, so a pass costs the render thread more than its dispatches.
/// Dispatches inside a compute pass get their own barriers, so the results are the same.
fn pass_merge_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_SW_PASS_MERGE"))
}

/// P16-A: the main view's pre-pass (and its cull, in the cascades' cull pass) is recorded by `draw_static_shadows`, so
/// both share one command buffer (each buffer costs a wgpu submit-time walk of every bind group it used plus an internal
/// transition buffer). The pre-pass then runs just before the main pass set instead of at its start.
fn enc_merge_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| flag_on("FH1_SW_ENC_MERGE"))
}

fn env_f32(k: &str, default: f32) -> f32 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Main view class distances (m, to the record's box): x pre-pass occluders, y occluder min half-diagonal, z near opaque,
/// w near cutouts.
fn ring() -> Vec4 {
    static V: std::sync::OnceLock<Vec4> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        Vec4::new(
            env_f32("FH1_SW_PREPASS_DIST", 120.0),
            env_f32("FH1_SW_PREPASS_RADIUS", 4.0),
            env_f32("FH1_SW_NEAR_DIST", 150.0).max(1.0),
            env_f32("FH1_SW_MASK_NEAR_DIST", 80.0).max(1.0),
        )
    })
}

/// Draw classes of a bin (draw order: near cutouts, occluders, near opaque, far opaque, far cutouts). Without the split
/// every bin is CLASS_FAR. Shadow views use CLASS_NEAR of opaque bins for the in-band (dithered) records.
const CLASS_FAR: u8 = 0;
const CLASS_NEAR: u8 = 1;
const CLASS_OCC: u8 = 2;
/// Draw bins addressable by the packed candidate word (3 x 10 bits).
const MAX_DRAW_BINS: usize = 1023;

/// GPU culling on (static_world.rs `apply_ops`: hidden records stay list candidates only with it).
pub(super) fn culling() -> bool {
    cull_on()
}

/// The static world draws its scenery into the car probe faces (car_probe.rs static-only faces).
pub(super) fn probe_faces() -> bool {
    probe_on()
}

/// Main world: the two shaders.
pub(super) fn register_shaders(app: &mut App) {
    let mut shaders = app.world_mut().resource_mut::<Assets<Shader>>();
    let _ = shaders.insert(&CULL_SHADER, Shader::from_wgsl(CULL_WGSL, "fh1_remaster/static_world_cull.wgsl"));
    let _ = shaders.insert(&SHADOW_SHADER, Shader::from_wgsl(shadow_wgsl(), "fh1_remaster/static_world_shadow.wgsl"));
    super::hiz::register_shaders(&mut shaders);
}

pub(super) fn plugin(ra: &mut SubApp) {
    ra.init_resource::<SpecializedRenderPipelines<SwPipeline>>()
        .init_resource::<super::hiz::Hiz>()
        .init_resource::<SpecializedRenderPipelines<ShadowPipeline>>()
        .init_resource::<DrawLists>()
        .init_resource::<ViewUniforms>()
        .add_systems(Render, (resolve_materials, build_lists).chain().in_set(RenderSystems::PrepareResources).after(super::apply_ops))
        .add_systems(Render, (init_pipelines, specialize_views).chain().in_set(RenderSystems::Queue))
        .add_systems(Render, (prepare_views, prepare_bind_groups).chain().in_set(RenderSystems::PrepareBindGroups))
        // After Bevy's prepass too: with FH1_SW_ENC_MERGE it also records the main view's pre-pass.
        .add_systems(Core3d, draw_static_shadows.after(bevy::pbr::per_view_shadow_pass::<true>).after(Core3dSystems::Prepass).before(Core3dSystems::MainPass))
        // P15-A pre-pass: after Bevy's own prepass (contact shadows' DepthPrepass copies the depth at its end, so their
        // input is unchanged), before the main opaque pass.
        .add_systems(Core3d, draw_static_prepass.after(draw_static_shadows).before(main_opaque_pass_3d).in_set(Core3dSystems::MainPass))
        .add_systems(
            Core3d,
            // Before Bevy's transparent pass too (decals / water / glass over the static ground; dc).
            draw_static_world.after(main_opaque_pass_3d).before(bevy::core_pipeline::core_3d::main_transparent_pass_3d).before(fh1_render::fx_half_res::FxHalfResSet).in_set(Core3dSystems::MainPass),
        );
}

// ---------------------------------------------------------------- pipelines

/// Draw variant: cull mode (0 back, 1 front, 2 none) and alpha test.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Variant {
    cull: u8,
    mask: bool,
}

impl Variant {
    fn of(flags: u32) -> Self {
        let cull = if flags & FLAG_TWO_SIDED != 0 {
            2
        } else if flags & FLAG_MIRRORED != 0 {
            1
        } else {
            0
        };
        Self { cull, mask: flags & FLAG_MASK != 0 }
    }

    fn face(self) -> Option<Face> {
        match self.cull {
            0 => Some(Face::Back),
            1 => Some(Face::Front),
            _ => None,
        }
    }
}

/// Lit main / probe pipeline (Bevy's mesh pipeline for the view key + the remaster shader's STATIC_WORLD variant).
#[derive(Resource)]
pub(super) struct SwPipeline {
    mesh_pipeline: MeshPipeline,
    layout_ref: MeshVertexBufferLayoutRef,
    arena_layout: BindGroupLayoutDescriptor,
    material_layout: BindGroupLayoutDescriptor,
    bindless: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct SwKey {
    mesh_key: MeshPipelineKey,
    variant: Variant,
}

impl SpecializedRenderPipeline for SwPipeline {
    type Key = SwKey;

    fn specialize(&self, key: SwKey) -> RenderPipelineDescriptor {
        let mut d = self.mesh_pipeline.specialize(key.mesh_key, &self.layout_ref).expect("mesh pipeline specialize");
        d.label = Some("static world".into());
        d.vertex.buffers.clear();
        // [view main, view binding array, mesh] -> [view main, view binding array, arena, material].
        d.layout.truncate(2);
        d.layout.push(self.arena_layout.clone());
        d.layout.push(self.material_layout.clone());
        d.vertex.shader = crate::material::SHADER;
        let mut defs: Vec<ShaderDefVal> = vec![
            ShaderDefVal::UInt("MATERIAL_BIND_GROUP".into(), 3),
            "STATIC_WORLD".into(),
            "RM_UV0".into(),
            "RM_UV1".into(),
            "RM_UV2".into(),
            "RM_COLOR".into(),
            "VISIBILITY_RANGE_DITHER".into(),
        ];
        if self.bindless {
            defs.push("BINDLESS".into());
        }
        d.vertex.shader_defs.extend(defs.iter().cloned());
        if let Some(f) = d.fragment.as_mut() {
            f.shader = crate::material::SHADER;
            f.shader_defs.extend(defs);
        }
        d.primitive.cull_mode = key.variant.face();
        d
    }
}

/// Depth-only cascade pipeline: [cull view uniform, arena, material]. Also the main view's pre-pass.
#[derive(Resource)]
pub(super) struct ShadowPipeline {
    view_layout: BindGroupLayoutDescriptor,
    arena_layout: BindGroupLayoutDescriptor,
    material_layout: BindGroupLayoutDescriptor,
    bindless: bool,
    unclipped: bool,
}

/// Cascade (prepass 0) or main-view pre-pass (prepass = MSAA sample count) pipeline key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct ShadowKey {
    variant: Variant,
    /// Cascades: LOD / zone fade dither in a fragment stage.
    dither: bool,
    prepass: u8,
}

impl SpecializedRenderPipeline for ShadowPipeline {
    type Key = ShadowKey;

    fn specialize(&self, k: ShadowKey) -> RenderPipelineDescriptor {
        let v = k.variant;
        let mut defs: Vec<ShaderDefVal> = vec![ShaderDefVal::UInt("MATERIAL_BIND_GROUP".into(), 2)];
        if self.bindless {
            defs.push("BINDLESS".into());
        }
        let pre = k.prepass > 0;
        if !self.unclipped && !pre {
            defs.push("CLAMP_DEPTH".into());
        }
        // Cutouts are alpha-tested only with bindless materials (the plain layout has no index table).
        let mask = v.mask && self.bindless && !pre;
        if mask {
            defs.push("MASK".into());
        }
        let dither = k.dither && !pre;
        if dither {
            defs.push("DITHER".into());
        }
        if pre {
            // Pre-pass: the material table only to drop cloth / non-opaque materials in the vertex stage.
            defs.push("PREPASS".into());
        }
        // P18: the pre-pass tests the record's FLAG_PRE_OK instead of the material table (FH1_SW_PREPASS_NOMAT=0 = old).
        let pre_flag = pre && super::prepass_nomat_on();
        if pre_flag {
            defs.push("PREPASS_FLAG".into());
        }
        let table = mask || (pre && self.bindless && !pre_flag);
        if table {
            defs.push("TABLE".into());
        }
        let frag = mask || dither;
        if frag {
            defs.push("FRAG".into());
        }
        // P16: without the table the shader reads nothing from the material group (FH1_SW_SHADOW_NOMAT=0 = old).
        let mut layout = vec![self.view_layout.clone(), self.arena_layout.clone()];
        if table || (!shadow_nomat_on() && !pre_flag) {
            layout.push(self.material_layout.clone());
        }
        RenderPipelineDescriptor {
            label: Some(if pre { "static world prepass" } else { "static world shadow" }.into()),
            layout,
            vertex: VertexState { shader: SHADOW_SHADER, shader_defs: defs.clone(), entry_point: Some("vertex".into()), buffers: Vec::new() },
            fragment: frag.then(|| FragmentState { shader: SHADOW_SHADER, shader_defs: defs, entry_point: Some("fragment".into()), targets: Vec::new() }),
            primitive: PrimitiveState { cull_mode: v.face(), unclipped_depth: self.unclipped && !pre, ..default() },
            depth_stencil: Some(DepthStencilState {
                format: CORE_3D_DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(CompareFunction::GreaterEqual),
                stencil: StencilState::default(),
                // Pre-pass: written a hair farther (reverse-Z: smaller) than the lit pass computes the same surface, so the
                // lit pass's GreaterEqual always passes on it (different shaders, no position invariance) while anything
                // behind is still rejected.
                bias: if pre { DepthBiasState { constant: -16, slope_scale: -2.0, clamp: 0.0 } } else { DepthBiasState::default() },
            }),
            multisample: bevy::render::render_resource::MultisampleState { count: k.prepass.max(1) as u32, ..default() },
            ..default()
        }
    }
}

/// The cull compute pipeline and its layout.
#[derive(Resource)]
pub(super) struct CullPipeline {
    layout: BindGroupLayoutDescriptor,
    id: CachedComputePipelineId,
    /// Compacted args + indirect count draws.
    compact: bool,
}

fn init_pipelines(
    mut commands: Commands,
    existing: Option<Res<SwPipeline>>,
    mesh_pipeline: Option<Res<MeshPipeline>>,
    device: Res<RenderDevice>,
    cache: Res<PipelineCache>,
    mut layouts: ResMut<MeshVertexBufferLayouts>,
) {
    if existing.is_some() {
        return;
    }
    let Some(mp) = mesh_pipeline else { return };
    // The scenery vertex layout, only for the mesh pipeline's attribute defines (vertices come from the arena).
    let mut mesh = Mesh::new(bevy::mesh::PrimitiveTopology::TriangleList, bevy::asset::RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; 3]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; 3]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0f32; 2]; 3]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, vec![[0.0f32; 2]; 3]);
    let layout_ref = mesh.get_mesh_vertex_buffer_layout(&mut layouts);
    let arena_layout = BindGroupLayoutDescriptor::new(
        "static world arena",
        &BindGroupLayoutEntries::with_indices(
            ShaderStages::VERTEX_FRAGMENT,
            (
                (100, storage_buffer_read_only_sized(false, None)),
                (101, storage_buffer_read_only_sized(false, None)),
                // P17-A: x = LOD distance scale (material.rs rm_dither; the cull and cascades carry it in eye.w).
                (102, uniform_buffer_sized(false, std::num::NonZeroU64::new(16))),
            ),
        ),
    );
    let material_layout = <RemasterMaterial as bevy::render::render_resource::AsBindGroup>::bind_group_layout_descriptor(&device);
    let bindless = bevy::pbr::material_uses_bindless_resources::<RemasterMaterial>(&device);
    let view_layout = BindGroupLayoutDescriptor::new(
        "static world view",
        &BindGroupLayoutEntries::single(ShaderStages::VERTEX_FRAGMENT | ShaderStages::COMPUTE, uniform_buffer::<ViewUniform>(true)),
    );
    let cull_layout = BindGroupLayoutDescriptor::new(
        "static world cull",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                uniform_buffer::<ViewUniform>(true),
                storage_buffer_read_only_sized(false, None),
                storage_buffer_read_only_sized(false, None),
                storage_buffer_sized(false, None),
                storage_buffer_read_only_sized(false, None),
                storage_buffer_sized(false, None),
                // Hi-Z: visibility bits, pyramid.
                storage_buffer_sized(false, None),
                bevy::render::render_resource::binding_types::texture_2d(bevy::render::render_resource::TextureSampleType::Float { filterable: false }),
            ),
        ),
    );
    commands.insert_resource(super::hiz::HizPipelines::new(&cache));
    let compact = compact_wanted() && device.features().contains(WgpuFeatures::MULTI_DRAW_INDIRECT_COUNT);
    let id = cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("static world cull".into()),
        layout: vec![cull_layout.clone()],
        shader: CULL_SHADER,
        shader_defs: if compact { vec!["COMPACT".into()] } else { Vec::new() },
        entry_point: Some("cull".into()),
        ..default()
    });
    let unclipped = device.features().contains(WgpuFeatures::DEPTH_CLIP_CONTROL);
    info!("static world: pipelines ready (bindless materials: {bindless}, unclipped shadow depth: {unclipped}, compacted draws: {compact})");
    commands.insert_resource(ShadowPipeline { view_layout, arena_layout: arena_layout.clone(), material_layout: material_layout.clone(), bindless, unclipped });
    commands.insert_resource(CullPipeline { layout: cull_layout, id, compact });
    commands.insert_resource(SwPipeline { mesh_pipeline: mp.clone(), layout_ref, arena_layout, material_layout, bindless });
}

/// Per drawn view: the lit pipelines per variant (main / probe) and the main view's pre-pass pipelines (opaque variants).
#[derive(Component, Default)]
pub(super) struct SwViewPipelines {
    lit: HashMap<Variant, CachedRenderPipelineId>,
    pre: HashMap<Variant, CachedRenderPipelineId>,
}

/// The main view runs the P15-A pre-pass this frame (prepare_views): its first cull is dispatched by draw_static_prepass
/// (by draw_static_shadows with FH1_SW_ENC_MERGE).
#[derive(Component, Clone, Copy)]
pub(super) struct SwPrepass;

/// The main 3D camera (order 0).
fn is_main(camera: &ExtractedCamera) -> bool {
    camera.order == 0
}

/// A car probe face drawn this frame (`CarProbeFace` 0..5).
fn probe_face(face: Option<&CarProbeFace>) -> Option<u8> {
    face.map(|f| f.0).filter(|&f| f < 6)
}

#[allow(clippy::too_many_arguments)]
fn specialize_views(
    mut commands: Commands,
    pipeline: Option<Res<SwPipeline>>,
    shadow: Option<Res<ShadowPipeline>>,
    mut pipelines: ResMut<SpecializedRenderPipelines<SwPipeline>>,
    mut shadow_pipelines: ResMut<SpecializedRenderPipelines<ShadowPipeline>>,
    cache: Res<PipelineCache>,
    keys: Res<ViewKeyCache>,
    mut lists: ResMut<DrawLists>,
    views: Query<(Entity, &ExtractedView, &ExtractedCamera, Option<&CarProbeFace>), With<ViewTarget>>,
) {
    let (Some(pipeline), Some(shadow)) = (pipeline, shadow) else { return };
    let variants = lists.variants();
    for (e, view, camera, face) in &views {
        if !is_main(camera) && !(probe_on() && probe_face(face).is_some()) {
            continue;
        }
        let Some(view_key) = keys.get(&view.retained_view_entity) else { continue };
        let mesh_key = *view_key | MeshPipelineKey::from_primitive_topology_and_strip_index(bevy::mesh::PrimitiveTopology::TriangleList, None);
        let lit = variants.iter().map(|&v| (v, pipelines.specialize(&cache, &pipeline, SwKey { mesh_key, variant: v }))).collect();
        let pre = if is_main(camera) && lists.split && prepass_on() && shadow.bindless {
            let samples = view_key.msaa_samples().clamp(1, 255) as u8;
            variants
                .iter()
                .filter(|v| !v.mask)
                .map(|&v| (v, shadow_pipelines.specialize(&cache, &shadow, ShadowKey { variant: v, dither: false, prepass: samples })))
                .collect()
        } else {
            HashMap::new()
        };
        commands.entity(e).insert(SwViewPipelines { lit, pre });
    }
    // Cascades: per variant the plain pipeline (cutouts dither in it when on) and the dithering one (in-band opaque class).
    let dither = shadow_dither_on();
    let mut map = HashMap::new();
    for &v in &variants {
        map.insert((v, false), shadow_pipelines.specialize(&cache, &shadow, ShadowKey { variant: v, dither: dither && v.mask && shadow.bindless, prepass: 0 }));
        // Always (not only with the split): specialize_views runs before build_lists flips `split`.
        if dither {
            map.insert((v, true), shadow_pipelines.specialize(&cache, &shadow, ShadowKey { variant: v, dither: true, prepass: 0 }));
        }
    }
    lists.shadow_pipelines = map;
}

// ---------------------------------------------------------------- draw lists (CPU, on change only)

/// Looks up the bindless slot of each record waiting for its material.
/// Re-bound slots (material changes) are checked once more the frame after, in case Bevy's re-preparation lands a frame
/// later than the change; only a changed slot / slab writes the record and rebuilds the lists.
fn resolve_materials(mut arena: ResMut<Arena>, bindings: Res<RenderMaterialBindings>, device: Res<RenderDevice>, queue: Res<RenderQueue>, mut later: Local<Vec<u32>>) {
    let mut pending = std::mem::take(&mut arena.pending);
    pending.append(&mut later);
    *later = std::mem::take(&mut arena.recheck);
    if pending.is_empty() {
        return;
    }
    pending.sort_unstable();
    pending.dedup();
    let mut still = Vec::new();
    // Batched uploads (static_world.rs `batch_ops_on`): one write per run of changed slots.
    let batch = super::batch_ops_on();
    let mut touched = Vec::new();
    for slot in pending {
        let s = slot as usize;
        let Some(Some((_, material))) = arena.slot_mesh.get(s).cloned() else { continue };
        match bindings.get(&material) {
            Some(b) => {
                // P18: the pre-pass reads this bit instead of the material table (FH1_SW_PREPASS_NOMAT).
                let flags = if super::prepass_nomat_on() && arena.pre_ok.contains_key(&material) {
                    arena.records[s].flags | super::FLAG_PRE_OK
                } else {
                    arena.records[s].flags & !super::FLAG_PRE_OK
                };
                if arena.records[s].material != b.slot.0 || arena.slot_group[s] != Some(b.group.0) || arena.records[s].flags != flags {
                    arena.records[s].flags = flags;
                    arena.records[s].material = b.slot.0;
                    arena.slot_group[s] = Some(b.group.0);
                    if batch {
                        touched.push(slot);
                    } else {
                        arena.write_record(&device, &queue, slot);
                    }
                    arena.dirty = true;
                }
            }
            None => still.push(slot),
        }
    }
    arena.write_records(&device, &queue, &mut touched);
    arena.pending = still;
}

/// One indirect draw (wgpu DrawIndexedIndirectArgs).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Args {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
}

const ARGS_BYTES: u64 = std::mem::size_of::<Args>() as u64;

/// A run of candidates sharing a material bind group and a variant.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bin {
    group: u32,
    variant: Variant,
    /// First arg entry of the bin in a view's region (= first candidate without the class split).
    first: u32,
    /// Arg capacity (the source bin's candidate count).
    count: u32,
    /// CLASS_* (P15-A).
    class: u8,
}

#[derive(Resource, Default)]
pub(super) struct DrawLists {
    bins: Vec<Bin>,
    /// Candidate record slots, in bin order (cull input).
    candidates: Option<Buffer>,
    candidates_cap: u64,
    count: u32,
    /// Indirect args: MAX_VIEWS regions of `region` entries (one per view drawn this frame); without culling, region 0
    /// holds every candidate's args (written by the CPU).
    args: Option<Buffer>,
    region: u32,
    /// Compaction: each candidate's bin is in the candidate buffer (slot, bin pairs); first arg entry per bin; one draw
    /// count per (view, bin) (MAX_VIEWS x `bins_cap`).
    bin_first: Option<Buffer>,
    counts: Option<Buffer>,
    bins_cap: u32,
    /// Bumped on every candidate list rebuild (Hi-Z: the visibility bits reset).
    generation: u64,
    arena_bind_group: Option<(u32, BindGroup)>,
    /// P17-A: the arena bind group's LOD uniform (vec4, x = LOD distance scale), rewritten every frame.
    lod_uniform: Option<Buffer>,
    /// Cascade pipelines per (variant, dithering class).
    shadow_pipelines: HashMap<(Variant, bool), CachedRenderPipelineId>,
    /// P16: cascade draw order (bin indices): bins that need no material first, then the rest grouped by slab.
    shadow_order: Vec<u32>,
    /// P15-A: the bins are split into draw classes (culling + compaction + FH1_SW_DRAW_ORDER).
    split: bool,
}

impl DrawLists {
    fn variants(&self) -> Vec<Variant> {
        let mut v: Vec<Variant> = self.bins.iter().map(|b| b.variant).collect();
        v.sort_by_key(|x| (x.cull, x.mask));
        v.dedup();
        v
    }
}

/// Rebuilds the candidate list (and, without culling, the args) when the arena changed (streaming).
fn build_lists(mut arena: ResMut<Arena>, mut lists: ResMut<DrawLists>, device: Res<RenderDevice>, queue: Res<RenderQueue>) {
    if !arena.dirty {
        return;
    }
    arena.dirty = false;
    let cull = cull_on();
    // Counting sort by bin key (group, cull, mask): the rebuild runs on most frames while streaming (every membership
    // change), and the HashMap<(group, variant), Vec> it used cost ~0.5 ms per 32-40k records (user log 164922:
    // build_lists 0.46 ms per frame averaged over the run).
    let key_of = |group: u32, v: Variant| group as usize * 6 + v.cull as usize * 2 + v.mask as usize;
    let max_group = arena.slot_group.iter().flatten().copied().max().unwrap_or(0) as usize;
    let mut counts = vec![0u32; (max_group + 1) * 6];
    let mut rec_key: Vec<u32> = vec![u32::MAX; arena.records.len()];
    for (s, r) in arena.records.iter().enumerate() {
        // With culling, hidden records stay candidates (the GPU checks the flag): a zone switch doesn't rebuild the list.
        if r.flags & FLAG_LIVE == 0 || (!cull && r.flags & FLAG_HIDDEN != 0) || r.index_count == 0 {
            continue;
        }
        let Some(Some(group)) = arena.slot_group.get(s) else { continue };
        let k = key_of(*group, Variant::of(r.flags));
        counts[k] += 1;
        rec_key[s] = k as u32;
    }
    // Source bins: the candidates grouped by key (candidate order).
    let mut src: Vec<Bin> = Vec::new();
    let mut cursor = vec![0u32; counts.len()];
    let mut total = 0u32;
    for (k, &n) in counts.iter().enumerate() {
        if n == 0 {
            continue;
        }
        cursor[k] = total;
        src.push(Bin { group: (k / 6) as u32, variant: Variant { cull: ((k % 6) / 2) as u8, mask: k % 2 == 1 }, first: total, count: n, class: CLASS_FAR });
        total += n;
    }
    let mut slots: Vec<u32> = vec![0; total as usize];
    for (s, &k) in rec_key.iter().enumerate() {
        if k != u32::MAX {
            slots[cursor[k as usize] as usize] = s as u32;
            cursor[k as usize] += 1;
        }
    }
    // P15-A draw bins in draw order, each with its own arg range (capacity = the source bin's count): near cutouts,
    // pre-passed occluders, near opaque, far opaque, far cutouts. `class_bins[i]` = draw bin of source bin i per class.
    let n_mask = src.iter().filter(|b| b.variant.mask).count();
    let mut split = cull && order_on() && compact_wanted() && device.features().contains(WgpuFeatures::MULTI_DRAW_INDIRECT_COUNT);
    if split && n_mask * 2 + (src.len() - n_mask) * 3 > MAX_DRAW_BINS {
        warn!("static world: {} source bins, too many for the draw classes; class split off", src.len());
        split = false;
    }
    let mut bins: Vec<Bin> = Vec::with_capacity(src.len() * 3);
    let mut class_bins = vec![[0u32; 3]; src.len()];
    let mut entries = total;
    if split {
        let mut at = 0u32;
        for (mask, class) in [(true, CLASS_NEAR), (false, CLASS_OCC), (false, CLASS_NEAR), (false, CLASS_FAR), (true, CLASS_FAR)] {
            for (i, b) in src.iter().enumerate().filter(|(_, b)| b.variant.mask == mask) {
                class_bins[i][class as usize] = bins.len() as u32;
                bins.push(Bin { first: at, class, ..*b });
                at += b.count;
            }
        }
        for (i, b) in src.iter().enumerate() {
            if b.variant.mask {
                class_bins[i][CLASS_OCC as usize] = class_bins[i][CLASS_NEAR as usize];
            }
        }
        entries = at;
    } else {
        for (i, b) in src.iter().enumerate() {
            class_bins[i] = [i as u32; 3];
            bins.push(*b);
        }
    }
    let mut order: Vec<u32> = (0..bins.len() as u32).collect();
    order.sort_by_key(|&b| {
        let bin = &bins[b as usize];
        (bin.variant.mask, if bin.variant.mask { bin.group } else { 0 }, b)
    });
    lists.shadow_order = order;
    lists.bins = bins;
    lists.split = split;
    lists.count = slots.len() as u32;
    lists.generation += 1;
    if slots.is_empty() {
        return;
    }
    // Args: one region per view (culling) or one region (no culling). Re-created when the candidates outgrow it.
    let region = entries;
    if lists.args.is_none() || lists.region < region {
        let r = (region + region / 2).max(4096);
        let views = if cull { MAX_VIEWS } else { 1 };
        lists.args = Some(device.create_buffer(&BufferDescriptor {
            label: Some("static world indirect"),
            size: r as u64 * views as u64 * ARGS_BYTES,
            usage: BufferUsages::INDIRECT | BufferUsages::STORAGE | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        lists.region = r;
    }
    if !cull {
        let all: Vec<Args> = slots
            .iter()
            .map(|&s| {
                let r = &arena.records[s as usize];
                Args { index_count: r.index_count, instance_count: 1, first_index: r.first_index, base_vertex: r.base_vertex as i32, first_instance: s }
            })
            .collect();
        if let Some(b) = &lists.args {
            // SAFETY: Args is repr(C) of 4-byte fields.
            let raw = unsafe { std::slice::from_raw_parts(all.as_ptr() as *const u8, std::mem::size_of_val(all.as_slice())) };
            queue.write_buffer(b, 0, raw);
        }
        return;
    }
    // Candidates as (slot, bin) pairs; the bins' first arg entries; the per-(view, bin) counts. With the class split the
    // bin word packs the draw bins far | near << 10 | occluder << 20 (the cull picks one per view and frame).
    let mut pairs: Vec<u32> = Vec::with_capacity(slots.len() * 2);
    for (i, b) in src.iter().enumerate() {
        let c = class_bins[i];
        let word = if split { c[0] | c[1] << 10 | c[2] << 20 } else { c[0] };
        for &slot in &slots[b.first as usize..(b.first + b.count) as usize] {
            pairs.push(slot);
            pairs.push(word);
        }
    }
    let firsts: Vec<u32> = lists.bins.iter().map(|b| b.first).collect();
    let nb = firsts.len() as u32;
    if lists.bin_first.is_none() || lists.bins_cap < nb {
        let cap = (nb * 2).max(64);
        lists.bin_first = Some(device.create_buffer(&BufferDescriptor { label: Some("static world bin firsts"), size: cap as u64 * 4, usage: BufferUsages::STORAGE | BufferUsages::COPY_DST, mapped_at_creation: false }));
        lists.counts = Some(device.create_buffer(&BufferDescriptor {
            label: Some("static world draw counts"),
            size: cap as u64 * MAX_VIEWS as u64 * 4,
            usage: BufferUsages::STORAGE | BufferUsages::INDIRECT | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        lists.bins_cap = cap;
    }
    if let Some(b) = &lists.bin_first {
        queue.write_buffer(b, 0, super::bytemuck_words(&firsts));
    }
    let slots = pairs;
    let bytes = slots.len() as u64 * 4;
    if lists.candidates.is_none() || lists.candidates_cap < bytes {
        let cap = ((bytes + bytes / 2).max(1 << 16) + 15) & !15; // storage bindings must be 4-byte multiples
        lists.candidates = Some(device.create_buffer(&BufferDescriptor { label: Some("static world candidates"), size: cap, usage: BufferUsages::STORAGE | BufferUsages::COPY_DST, mapped_at_creation: false }));
        lists.candidates_cap = cap;
    }
    if let Some(b) = &lists.candidates {
        queue.write_buffer(b, 0, super::bytemuck_words(&slots));
    }
}

// ---------------------------------------------------------------- per-view cull data

/// Cull / shadow view uniform (encase layout).
#[derive(Clone, Copy, ShaderType, Default)]
pub(super) struct ViewUniform {
    clip_from_world: Mat4,
    planes: [Vec4; 6],
    /// xyz LOD eye (main camera), w LOD distance scale.
    eye: Vec4,
    /// x min radius (probe), y max distance (0 = none), z small-caster distance (shadows), w plane count.
    params: Vec4,
    /// x kind (0 main, 1 shadow, 2 probe), y args base (entries), z candidate count, w draw-count base (compaction).
    info: UVec4,
    /// Hi-Z: x phase (0 single cull, 1 last frame's visible, 2 occlusion test), y pyramid mip levels.
    extra: UVec4,
    /// P15-A main view class distances (`ring()`; x 0 = no pre-pass class, z 0 = no near classes).
    ring: Vec4,
    /// P15-A: x 1 = packed class bins, y cascade fade mode (0 off, 1 in-band opaque -> dithering class, 2 hard switch at
    /// the band centre), z 1 = cutouts dither in their own pipeline (bindless).
    opts: UVec4,
}

const KIND_MAIN: u32 = 0;
const KIND_SHADOW: u32 = 1;
const KIND_PROBE: u32 = 2;

/// The main view's phase-2 cull (Hi-Z): its own uniform, args region and draw counts.
#[derive(Component, Clone, Copy)]
pub(super) struct SwCullViewHiz(SwCullView);

/// A view's cull uniform offset and args region (this frame).
#[derive(Component, Clone, Copy)]
pub(super) struct SwCullView {
    offset: u32,
    base: u32,
    /// First draw count of this view (compaction).
    counts: u32,
}

#[derive(Resource, Default)]
pub(super) struct ViewUniforms {
    buffer: DynamicUniformBuffer<ViewUniform>,
    cull_bind_group: Option<BindGroup>,
    shadow_view_bind_group: Option<BindGroup>,
}

/// Rows of a column-major matrix.
fn rows(m: &Mat4) -> [Vec4; 4] {
    let t = m.transpose();
    [t.x_axis, t.y_axis, t.z_axis, t.w_axis]
}

/// Frustum planes (inside = dot >= 0) of a reverse-Z clip matrix: left, right, bottom, top, and (not for shadow casters,
/// which may sit behind the cascade's near plane) near.
fn planes(m: &Mat4, near: bool) -> ([Vec4; 6], u32) {
    let r = rows(m);
    let mut p = [Vec4::ZERO; 6];
    p[0] = r[3] + r[0];
    p[1] = r[3] - r[0];
    p[2] = r[3] + r[1];
    p[3] = r[3] - r[1];
    let mut n = 4;
    if near {
        p[4] = r[3] - r[2];
        n = 5;
    }
    (p, n)
}

fn clip_from_world(v: &ExtractedView) -> Mat4 {
    v.clip_from_world.unwrap_or_else(|| v.clip_from_view * v.world_from_view.to_matrix().inverse())
}

fn small_dist() -> f32 {
    std::env::var("FH1_SHADOW_SMALL_DIST").ok().and_then(|v| v.parse().ok()).unwrap_or(80.0)
}

/// The views drawn this frame: main camera, a car probe face, the main camera's cascades (minus the cached ones).
#[allow(clippy::too_many_arguments)]
fn prepare_views(
    mut commands: Commands,
    lists: Res<DrawLists>,
    mut uniforms: ResMut<ViewUniforms>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    skip: Option<Res<DirectionalShadowSkipThisFrame>>,
    cameras: Query<
        (Entity, &ExtractedView, &ExtractedCamera, Option<&CarProbeFace>, Option<&ViewLightEntities>, Has<bevy::render::camera::TemporalJitter>),
        With<ViewTarget>,
    >,
    lights: Query<(&ExtractedView, Option<&LightEntity>), With<ShadowView>>,
    stale: Query<Entity, Or<(With<SwCullView>, With<SwCullViewHiz>, With<SwPrepass>)>>,
    depths: Query<&ViewDepthTexture>,
    (mut hiz, hiz_pipes, cache, shadow): (ResMut<super::hiz::Hiz>, Option<Res<super::hiz::HizPipelines>>, Res<PipelineCache>, Option<Res<ShadowPipeline>>),
) {
    // Views not drawn this frame (a cached cascade, an idle probe face) must not keep last frame's slot.
    for e in &stale {
        commands.entity(e).remove::<(SwCullView, SwCullViewHiz, SwPrepass)>();
    }
    let bindless = shadow.as_ref().is_some_and(|s| s.bindless);
    // P15-A options: packed class bins; cascade fades dithered (class split) or switched at the band centre.
    let fade_mode = match (shadow_dither_on(), lists.split) {
        (false, _) => 0,
        (true, true) => 1,
        (true, false) => 2,
    };
    let opts = UVec4::new(lists.split as u32, fade_mode, bindless as u32, 0);
    // P17-A: the quality preset's draw distance shrinks every LOD band (the shader multiplies the eye distance by w).
    let lod_k = 1.0 / fh1_render::quality::draw_distance();
    hiz.active = false;
    let u = &mut *uniforms;
    u.buffer.clear();
    CANDIDATES.store(lists.count, std::sync::atomic::Ordering::Relaxed);
    VIEWS.store(0, std::sync::atomic::Ordering::Relaxed);
    if !cull_on() || lists.count == 0 {
        return;
    }
    let (count, region, bins_cap) = (lists.count, lists.region, lists.bins_cap);
    let mut slot = 0u32;
    let mut push = |vu: ViewUniform, buffer: &mut DynamicUniformBuffer<ViewUniform>| -> Option<SwCullView> {
        if slot >= MAX_VIEWS {
            return None;
        }
        let base = slot * region;
        let counts = slot * bins_cap;
        let offset = buffer.push(&ViewUniform { info: UVec4::new(vu.info.x, base, count, counts), ..vu });
        slot += 1;
        Some(SwCullView { offset, base, counts })
    };
    let skip_mask = skip.map_or(0, |s| s.0);
    for (e, view, camera, face, view_lights, jittered) in &cameras {
        let face = probe_face(face);
        let main = is_main(camera);
        if !main && !(probe_on() && face.is_some()) {
            continue;
        }
        let cfw = clip_from_world(view);
        let eye = view.world_from_view.translation();
        let (p, n) = planes(&cfw, true);
        // Hi-Z for the main view: a single-sample depth that can be sampled, compiled pipelines.
        let hiz_now = main
            && super::hiz::hiz_on()
            && hiz_pipes.as_ref().is_some_and(|p| p.ready(&cache))
            && depths.get(e).is_ok_and(|d| d.texture.sample_count() == 1 && d.texture.usage().contains(bevy::render::render_resource::TextureUsages::TEXTURE_BINDING));
        if hiz_now {
            if let Ok(d) = depths.get(e) {
                let sz = d.texture.size();
                hiz.ensure_pyramid(&device, UVec2::new(sz.width, sz.height));
            }
            hiz.active = true;
        }
        let mips = hiz.mips();
        let vu = if main {
            ViewUniform {
                clip_from_world: cfw,
                planes: p,
                eye: eye.extend(lod_k),
                params: Vec4::new(0.0, 0.0, 0.0, n as f32),
                info: UVec4::new(KIND_MAIN, 0, 0, 0),
                extra: UVec4::new(if hiz_now { 1 } else { 0 }, mips, 0, 0),
                // Near / occluder classes only with the split; the occluder class only when the pre-pass draws it.
                ring: if lists.split {
                    let r = ring();
                    Vec4::new(if prepass_on() && bindless { r.x } else { 0.0 }, r.y, r.z, r.w)
                } else {
                    Vec4::ZERO
                },
                opts,
            }
        } else {
            // 47's probe rules: radius >= probe_min_radius, within the face's far plane (80 m, down face 8 m).
            let far = if face == Some(3) { 8.0 } else { 80.0 };
            ViewUniform {
                clip_from_world: cfw,
                planes: p,
                eye: eye.extend(lod_k),
                params: Vec4::new(crate::car_probe::probe_min_radius(), far, 0.0, n as f32),
                info: UVec4::new(KIND_PROBE, 0, 0, 0),
                extra: UVec4::ZERO,
                ring: Vec4::ZERO,
                opts,
            }
        };
        if let Some(cv) = push(vu, &mut u.buffer) {
            commands.entity(e).insert(cv);
            // Not under TAA / DLSS jitter: the cull matrix is unjittered, the pre-pass edges would not match the lit pass.
            if main && vu.ring.x > 0.0 && !jittered {
                commands.entity(e).insert(SwPrepass);
            }
        }
        if hiz_now {
            if let Some(cv) = push(ViewUniform { extra: UVec4::new(2, mips, 0, 0), ..vu }, &mut u.buffer) {
                commands.entity(e).insert(SwCullViewHiz(cv));
            }
        }
        // The main camera's directional cascades.
        if main && shadows_on() {
            for &le in view_lights.map_or(&[][..], |l| l.lights.as_slice()) {
                let Ok((lv, kind)) = lights.get(le) else { continue };
                let Some(LightEntity::Directional { cascade_index, .. }) = kind else { continue };
                if *cascade_index < 32 && skip_mask & (1 << *cascade_index) != 0 {
                    continue;
                }
                let cfw = clip_from_world(lv);
                let (p, n) = planes(&cfw, false);
                let vu = ViewUniform {
                    clip_from_world: cfw,
                    planes: p,
                    eye: eye.extend(lod_k),
                    params: Vec4::new(0.0, 0.0, small_dist(), n as f32),
                    info: UVec4::new(KIND_SHADOW, 0, 0, 0),
                    extra: UVec4::ZERO,
                    ring: Vec4::ZERO,
                    opts,
                };
                if let Some(cv) = push(vu, &mut u.buffer) {
                    commands.entity(le).insert(cv);
                }
            }
        }
    }
    VIEWS.store(slot, std::sync::atomic::Ordering::Relaxed);
    u.buffer.write_buffer(&device, &queue);
}

/// Bind groups: the arena (draws), the cull inputs / outputs, the shadow view uniform.
#[allow(clippy::too_many_arguments)]
fn prepare_bind_groups(
    (mut hiz, queue): (ResMut<super::hiz::Hiz>, Res<RenderQueue>),
    arena: Res<Arena>,
    mut lists: ResMut<DrawLists>,
    mut uniforms: ResMut<ViewUniforms>,
    pipeline: Option<Res<SwPipeline>>,
    cull: Option<Res<CullPipeline>>,
    shadow: Option<Res<ShadowPipeline>>,
    cache: Res<PipelineCache>,
    device: Res<RenderDevice>,
) {
    let (Some(pipeline), Some(cull), Some(shadow)) = (pipeline, cull, shadow) else { return };
    let lod = lists
        .lod_uniform
        .get_or_insert_with(|| device.create_buffer(&BufferDescriptor { label: Some("static world lod"), size: 16, usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST, mapped_at_creation: false }))
        .clone();
    let lod_k = 1.0 / fh1_render::quality::draw_distance();
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&lod_k.to_le_bytes());
    queue.write_buffer(&lod, 0, &bytes);
    if !lists.arena_bind_group.as_ref().is_some_and(|(g, _)| *g == arena.buffers_generation) {
        if let (Some(v), Some(r)) = (arena.vertex_buffer(), arena.record_buffer.as_ref()) {
            let layout = cache.get_bind_group_layout(&pipeline.arena_layout);
            let bg = device.create_bind_group(
                "static world arena",
                &layout,
                &BindGroupEntries::with_indices(((100, v.as_entire_binding()), (101, r.as_entire_binding()), (102, lod.as_entire_binding()))),
            );
            lists.arena_bind_group = Some((arena.buffers_generation, bg));
        }
    }
    let u = &mut *uniforms;
    u.cull_bind_group = None;
    u.shadow_view_bind_group = None;
    let Some(view_binding) = u.buffer.binding() else { return };
    if let (Some(r), Some(c), Some(a), Some(bf), Some(n)) =
        (arena.record_buffer.as_ref(), lists.candidates.as_ref(), lists.args.as_ref(), lists.bin_first.as_ref(), lists.counts.as_ref())
    {
        let layout = cache.get_bind_group_layout(&cull.layout);
        hiz.ensure_bits(&device, &queue, lists.count, lists.generation);
        let pyramid = hiz.bind_view(&device);
        if let Some(bits) = hiz.bits.as_ref() {
            u.cull_bind_group = Some(device.create_bind_group(
                "static world cull",
                &layout,
                &BindGroupEntries::sequential((
                    view_binding.clone(),
                    r.as_entire_binding(),
                    c.as_entire_binding(),
                    a.as_entire_binding(),
                    bf.as_entire_binding(),
                    n.as_entire_binding(),
                    bits.as_entire_binding(),
                    &pyramid,
                )),
            ));
        }
    }
    let layout = cache.get_bind_group_layout(&shadow.view_layout);
    u.shadow_view_bind_group = Some(device.create_bind_group("static world shadow view", &layout, &BindGroupEntries::single(view_binding)));
}

// ---------------------------------------------------------------- passes

/// The cull pipeline and its bind group exist (dispatch_cull can run).
fn cull_ready(cache: &PipelineCache, cull: &CullPipeline, uniforms: &ViewUniforms) -> bool {
    cache.get_compute_pipeline(cull.id).is_some() && uniforms.cull_bind_group.is_some()
}

/// Culls the view's candidates into its args region.
fn dispatch_cull(ctx: &mut RenderContext, cache: &PipelineCache, cull: &CullPipeline, uniforms: &ViewUniforms, lists: &DrawLists, view: &SwCullView, count: u32) -> bool {
    dispatch_culls(ctx, cache, cull, uniforms, lists, &[*view], count)
}

/// Clears the draw counts of `views` (compaction), before the pass that culls them. False = the cull can't run.
fn clear_counts(ctx: &mut RenderContext, cull: &CullPipeline, lists: &DrawLists, views: &[SwCullView]) -> bool {
    if cull.compact {
        let Some(counts) = lists.counts.as_ref() else { return false };
        for v in views {
            ctx.command_encoder().clear_buffer(counts, v.counts as u64 * 4, Some(lists.bins_cap as u64 * 4));
        }
    }
    true
}

/// One cull dispatch per view, in the current compute pass (each view writes its own args / counts region).
fn cull_in_pass(pass: &mut ComputePass<'_>, p: &ComputePipeline, bg: &BindGroup, views: &[SwCullView], count: u32) {
    pass.set_pipeline(p);
    for v in views {
        pass.set_bind_group(0, bg, &[v.offset]);
        pass.dispatch_workgroups(count.div_ceil(64), 1, 1);
    }
}

/// Culls every view of `views` in one compute pass (P16-A; one view = the old per-view pass).
fn dispatch_culls(ctx: &mut RenderContext, cache: &PipelineCache, cull: &CullPipeline, uniforms: &ViewUniforms, lists: &DrawLists, views: &[SwCullView], count: u32) -> bool {
    let (Some(p), Some(bg)) = (cache.get_compute_pipeline(cull.id), uniforms.cull_bind_group.as_ref()) else { return false };
    if views.is_empty() {
        return true;
    }
    if !clear_counts(ctx, cull, lists, views) {
        return false;
    }
    let recorder = ctx.diagnostic_recorder();
    let diagnostics = recorder.as_deref();
    let mut pass = ctx.command_encoder().begin_compute_pass(&ComputePassDescriptor { label: Some("static world cull"), timestamp_writes: None });
    let span = diagnostics.pass_span(&mut pass, "static_world_cull");
    cull_in_pass(&mut pass, p, bg, views, count);
    span.end(&mut pass);
    true
}

#[allow(clippy::too_many_arguments)]
fn draw_static_world(
    view: ViewQuery<(
        &ExtractedCamera,
        &ViewTarget,
        &ViewDepthTexture,
        &MeshViewBindGroup,
        Option<&SwViewPipelines>,
        Option<&SwCullView>,
        Option<&SwCullViewHiz>,
        Has<SwPrepass>,
    )>,
    arena: Res<Arena>,
    lists: Res<DrawLists>,
    uniforms: Res<ViewUniforms>,
    cull: Option<Res<CullPipeline>>,
    cache: Res<PipelineCache>,
    allocators: Res<MaterialBindGroupAllocators>,
    (hiz, hiz_pipes, device): (Res<super::hiz::Hiz>, Option<Res<super::hiz::HizPipelines>>, Res<RenderDevice>),
    mut ctx: RenderContext,
) {
    let (camera, target, depth, view_bg, pipelines, cull_view, hiz_view, prepassed) = view.into_inner();
    let (Some(pipelines), Some((_, arena_bg)), Some(args), Some(ib)) = (pipelines, lists.arena_bind_group.as_ref(), lists.args.as_ref(), arena.index_buffer()) else { return };
    if lists.bins.is_empty() {
        return;
    }
    let Some(allocator) = allocators.get(&TypeId::of::<RemasterMaterial>()) else { return };
    let compact = cull.as_ref().is_some_and(|c| c.compact) && cull_on();
    // The main view uses every draw class (P15-A order); other views cull everything into CLASS_FAR.
    let classes = lists.split && is_main(camera);
    // GPU / CPU time of the passes for the perf recorder (RenderDiagnosticsPlugin; a no-op without it).
    let recorder = ctx.diagnostic_recorder();
    let diagnostics = recorder.as_deref();
    // One lit pass over every bin from `base` (+ draw counts when compacted). `span`: the recorder's pass name (P18: the
    // Hi-Z phase-2 pass is "static_world_phase2", so the GPU table splits the lit cost between the two passes).
    let draw = |ctx: &mut RenderContext, base: u32, counts: Option<u32>, span: &'static str| {
        let counts = counts.and_then(|c| lists.counts.as_ref().map(|b| (b, c)));
        let color = [Some(target.get_color_attachment())];
        let mut pass = ctx.begin_tracked_render_pass(RenderPassDescriptor {
            label: Some("static_world"),
            color_attachments: &color,
            depth_stencil_attachment: Some(depth.get_attachment(StoreOp::Store)),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some(viewport) = camera.viewport.as_ref() {
            pass.set_camera_viewport(viewport);
        }
        pass.set_bind_group(0, &view_bg.main, &view_bg.main_offsets);
        pass.set_bind_group(1, &view_bg.binding_array, &[]);
        pass.set_bind_group(2, arena_bg, &[]);
        pass.set_index_buffer(ib.slice(..), IndexFormat::Uint32);
        let span = diagnostics.pass_span(&mut pass, span);
        for (b, bin) in lists.bins.iter().enumerate() {
            if !classes && bin.class != CLASS_FAR {
                continue;
            }
            let Some(id) = pipelines.lit.get(&bin.variant) else { continue };
            let Some(p) = cache.get_render_pipeline(*id) else { continue };
            let Some(slab) = allocator.get(MaterialBindGroupIndex(bin.group)) else { continue };
            let Some(material_bg) = slab.bind_group() else { continue };
            pass.set_render_pipeline(p);
            pass.set_bind_group(3, material_bg, &[]);
            draw_bin(&mut pass, args, counts, base, b as u32, bin);
        }
        span.end(&mut pass);
    };
    // Unculled (FH1_STATIC_WORLD_CULL=0): region 0, main camera only.
    if !cull_on() {
        if is_main(camera) {
            draw(&mut ctx, 0, None, "static_world");
        }
        return;
    }
    // Culled: this view's region (+ its draw counts when compacted).
    let (Some(cv), Some(cull)) = (cull_view, cull.as_ref()) else { return };
    // With the P15-A pre-pass, draw_static_prepass (draw_static_shadows with FH1_SW_ENC_MERGE) already culled this view
    // (same readiness test).
    if prepassed {
        if !cull_ready(&cache, cull, &uniforms) {
            return;
        }
    } else if !dispatch_cull(&mut ctx, &cache, cull, &uniforms, &lists, cv, lists.count) {
        return;
    }
    draw(&mut ctx, cv.base, compact.then_some(cv.counts), "static_world");
    // Hi-Z phase 2 (main view): pyramid from the depth so far, occlusion-tested cull, the newly visible.
    if let (Some(SwCullViewHiz(cv2)), Some(pipes)) = (hiz_view, hiz_pipes.as_ref()) {
        if hiz.active {
            // P16-A (FH1_SW_PASS_MERGE): pyramid + phase-2 cull in one compute pass.
            let mut merged = false;
            if pass_merge_on() {
                if let (Some(p), Some(bg)) = (cache.get_compute_pipeline(cull.id), uniforms.cull_bind_group.as_ref()) {
                    if clear_counts(&mut ctx, cull, &lists, &[*cv2]) {
                        merged = hiz.build_with(&mut ctx, &cache, pipes, &device, depth.view(), &mut |pass: &mut ComputePass<'_>| cull_in_pass(pass, p, bg, &[*cv2], lists.count));
                    }
                }
            }
            if merged {
                draw(&mut ctx, cv2.base, compact.then_some(cv2.counts), "static_world_phase2");
            } else {
                hiz.build(&mut ctx, &cache, pipes, &device, depth.view());
                if dispatch_cull(&mut ctx, &cache, cull, &uniforms, &lists, cv2, lists.count) {
                    draw(&mut ctx, cv2.base, compact.then_some(cv2.counts), "static_world_phase2");
                }
            }
        }
    }
}

/// P15-A partial depth pre-pass (main view, before Bevy's main opaque pass): culls the main view (phase 1 with Hi-Z) and
/// draws the occluder class depth-only, position-only, into the main depth. The lit pass then shades those pixels once
/// and depth-rejects what they hide (ECS opaque included); the Hi-Z pyramid is reduced from the same depth later.
/// With FH1_SW_ENC_MERGE (default) `draw_static_shadows` records it instead and this system records nothing.
#[allow(clippy::too_many_arguments)]
fn draw_static_prepass(
    view: ViewQuery<(&ExtractedCamera, &ViewDepthTexture, Option<&SwViewPipelines>, Option<&SwCullView>), With<SwPrepass>>,
    arena: Res<Arena>,
    lists: Res<DrawLists>,
    uniforms: Res<ViewUniforms>,
    cull: Option<Res<CullPipeline>>,
    cache: Res<PipelineCache>,
    allocators: Res<MaterialBindGroupAllocators>,
    mut ctx: RenderContext,
) {
    if enc_merge_on() {
        return;
    }
    let (camera, depth, pipelines, cull_view) = view.into_inner();
    let (Some(cv), Some(cull)) = (cull_view, cull.as_ref()) else { return };
    // The cull runs here whatever else is missing: draw_static_world relies on it for this view.
    if !dispatch_cull(&mut ctx, &cache, cull, &uniforms, &lists, cv, lists.count) {
        return;
    }
    record_prepass(&mut ctx, camera, depth, pipelines, cv, &arena, &lists, &uniforms, cull, &cache, &allocators);
}

/// The pre-pass's depth-only draws (its cull already dispatched).
#[allow(clippy::too_many_arguments)]
fn record_prepass(
    ctx: &mut RenderContext,
    camera: &ExtractedCamera,
    depth: &ViewDepthTexture,
    pipelines: Option<&SwViewPipelines>,
    cv: &SwCullView,
    arena: &Arena,
    lists: &DrawLists,
    uniforms: &ViewUniforms,
    cull: &CullPipeline,
    cache: &PipelineCache,
    allocators: &MaterialBindGroupAllocators,
) {
    let (Some(pipelines), Some((_, arena_bg)), Some(args), Some(ib), Some(view_bg), Some(counts)) =
        (pipelines, lists.arena_bind_group.as_ref(), lists.args.as_ref(), arena.index_buffer(), uniforms.shadow_view_bind_group.as_ref(), lists.counts.as_ref())
    else {
        return;
    };
    if !cull.compact || !lists.split || pipelines.pre.is_empty() {
        return;
    }
    let Some(allocator) = allocators.get(&TypeId::of::<RemasterMaterial>()) else { return };
    let recorder = ctx.diagnostic_recorder();
    let diagnostics = recorder.as_deref();
    let mut pass = ctx.begin_tracked_render_pass(RenderPassDescriptor {
        label: Some("static_world_prepass"),
        color_attachments: &[],
        depth_stencil_attachment: Some(depth.get_attachment(StoreOp::Store)),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    if let Some(viewport) = camera.viewport.as_ref() {
        pass.set_camera_viewport(viewport);
    }
    pass.set_bind_group(0, view_bg, &[cv.offset]);
    pass.set_bind_group(1, arena_bg, &[]);
    pass.set_index_buffer(ib.slice(..), IndexFormat::Uint32);
    let span = diagnostics.pass_span(&mut pass, "static_world_prepass");
    for (b, bin) in lists.bins.iter().enumerate() {
        if bin.class != CLASS_OCC {
            continue;
        }
        let Some(id) = pipelines.pre.get(&bin.variant) else { continue };
        let Some(p) = cache.get_render_pipeline(*id) else { continue };
        // P18: with FH1_SW_PREPASS_NOMAT the pipeline has no material group (ShadowPipeline::specialize, pre_flag).
        if !super::prepass_nomat_on() {
            let Some(slab) = allocator.get(MaterialBindGroupIndex(bin.group)) else { continue };
            let Some(material_bg) = slab.bind_group() else { continue };
            pass.set_bind_group(2, material_bg, &[]);
        }
        pass.set_render_pipeline(p);
        draw_bin(&mut pass, args, Some((counts, cv.counts)), cv.base, b as u32, bin);
    }
    span.end(&mut pass);
}

/// One bin's draws (`b` = its index in `lists.bins`): compacted (indirect count) or every candidate (zero-instance when
/// culled).
fn draw_bin<'a>(pass: &mut bevy::render::render_phase::TrackedRenderPass<'a>, args: &'a Buffer, counts: Option<(&'a Buffer, u32)>, base: u32, b: u32, bin: &Bin) {
    let offset = (base + bin.first) as u64 * ARGS_BYTES;
    match counts {
        Some((cb, cbase)) => pass.multi_draw_indexed_indirect_count(args, offset, cb, (cbase + b) as u64 * 4, bin.count),
        None => pass.multi_draw_indexed_indirect(args, offset, bin.count),
    }
}

/// Static scenery into the main camera's directional cascades (after Bevy's shadow pass, same depth attachments).
/// P16-A: with FH1_SW_PASS_MERGE the cascades are culled in one compute pass; with FH1_SW_ENC_MERGE the main view's
/// pre-pass (cull included) is recorded here too, after the cascades, so they share one command buffer.
#[allow(clippy::too_many_arguments)]
fn draw_static_shadows(
    view: ViewQuery<(
        Option<&ViewLightEntities>,
        Option<(&ExtractedCamera, &ViewDepthTexture, Option<&SwViewPipelines>, &SwCullView)>,
        Has<SwPrepass>,
    )>,
    lights: Query<(&ShadowView, Option<&SwCullView>)>,
    arena: Res<Arena>,
    lists: Res<DrawLists>,
    uniforms: Res<ViewUniforms>,
    cull: Option<Res<CullPipeline>>,
    shadow: Option<Res<ShadowPipeline>>,
    cache: Res<PipelineCache>,
    allocators: Res<MaterialBindGroupAllocators>,
    mut ctx: RenderContext,
) {
    if !cull_on() || lists.bins.is_empty() {
        return;
    }
    let Some(cull) = cull.as_ref() else { return };
    let (view_lights, pre_view, prepassed) = view.into_inner();
    let pre = if enc_merge_on() && prepassed { pre_view } else { None };
    // Only the cascades prepare_views gave a slot (skipped / cached cascades have none).
    let cascades: Vec<(&ShadowView, SwCullView)> = match view_lights {
        Some(vl) if shadows_on() => vl
            .lights
            .iter()
            .filter_map(|&le| match lights.get(le) {
                Ok((sv, Some(cv))) => Some((sv, *cv)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if cascades.is_empty() && pre.is_none() {
        return;
    }
    let batched = pass_merge_on();
    if batched {
        let mut views: Vec<SwCullView> = cascades.iter().map(|c| c.1).collect();
        views.extend(pre.map(|p| *p.3));
        if !dispatch_culls(&mut ctx, &cache, cull, &uniforms, &lists, &views, lists.count) {
            return;
        }
    }
    'cascades: {
        if cascades.is_empty() {
            break 'cascades;
        }
        let (Some((_, arena_bg)), Some(args), Some(ib), Some(view_bg)) =
            (lists.arena_bind_group.as_ref(), lists.args.as_ref(), arena.index_buffer(), uniforms.shadow_view_bind_group.as_ref())
        else {
            break 'cascades;
        };
        let Some(allocator) = allocators.get(&TypeId::of::<RemasterMaterial>()) else { break 'cascades };
        let Some(shadow) = shadow.as_ref() else { break 'cascades };
        let counts = if cull.compact { lists.counts.as_ref() } else { None };
        let recorder = ctx.diagnostic_recorder();
        let diagnostics = recorder.as_deref();
        for (shadow_view, cv) in &cascades {
            if !batched && !dispatch_cull(&mut ctx, &cache, cull, &uniforms, &lists, cv, lists.count) {
                continue;
            }
            let mut pass = ctx.begin_tracked_render_pass(RenderPassDescriptor {
                label: Some("static_world_shadow"),
                color_attachments: &[],
                depth_stencil_attachment: Some(shadow_view.depth_attachment.get_attachment(StoreOp::Store)),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, view_bg, &[cv.offset]);
            pass.set_bind_group(1, arena_bg, &[]);
            pass.set_index_buffer(ib.slice(..), IndexFormat::Uint32);
            let span = diagnostics.pass_span(&mut pass, "static_world_shadow");
            for &b in &lists.shadow_order {
                let bin = &lists.bins[b as usize];
                // Cascades: far = all casters, near = the in-band (dithered) ones (P15-A fade mode 1); no occluder class.
                let dithered = match bin.class {
                    CLASS_FAR => false,
                    CLASS_NEAR if lists.split && shadow_dither_on() => true,
                    _ => continue,
                };
                let Some(id) = lists.shadow_pipelines.get(&(bin.variant, dithered)) else { continue };
                let Some(p) = cache.get_render_pipeline(*id) else { continue };
                // Same rule as ShadowPipeline::specialize: only alpha-tested (bindless) cutouts read the material table.
                if !(shadow_nomat_on() && !(bin.variant.mask && shadow.bindless)) {
                    let Some(slab) = allocator.get(MaterialBindGroupIndex(bin.group)) else { continue };
                    let Some(material_bg) = slab.bind_group() else { continue };
                    pass.set_bind_group(2, material_bg, &[]);
                }
                pass.set_render_pipeline(p);
                draw_bin(&mut pass, args, counts.map(|c| (c, cv.counts)), cv.base, b, bin);
            }
            span.end(&mut pass);
        }
    }
    // The main view's pre-pass (FH1_SW_ENC_MERGE); draw_static_world relies on its cull for this view.
    if let Some((camera, depth, pipelines, cv)) = pre {
        if !batched && !dispatch_cull(&mut ctx, &cache, cull, &uniforms, &lists, cv, lists.count) {
            return;
        }
        record_prepass(&mut ctx, camera, depth, pipelines, cv, &arena, &lists, &uniforms, cull, &cache, &allocators);
    }
}

// ---------------------------------------------------------------- shaders

const RECORD_WGSL: &str = r#"
struct SwRecord {
    rows: array<vec4<f32>, 3>,
    aabb_min: vec4<f32>,
    aabb_max: vec4<f32>,
    lod: vec4<f32>,
    first_index: u32,
    index_count: u32,
    base_vertex: u32,
    material: u32,
    flags: u32,
    tag: i32,
    pad0: u32,
    pad1: u32,
}
struct SwView {
    clip_from_world: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    eye: vec4<f32>,
    params: vec4<f32>,
    info: vec4<u32>,
    extra: vec4<u32>,
    ring: vec4<f32>,
    opts: vec4<u32>,
}
"#;

const CULL_WGSL: &str = r#"
struct SwRecord {
    rows: array<vec4<f32>, 3>,
    aabb_min: vec4<f32>,
    aabb_max: vec4<f32>,
    lod: vec4<f32>,
    first_index: u32,
    index_count: u32,
    base_vertex: u32,
    material: u32,
    flags: u32,
    tag: i32,
    pad0: u32,
    pad1: u32,
}
struct SwView {
    clip_from_world: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    eye: vec4<f32>,
    params: vec4<f32>,
    info: vec4<u32>,
    extra: vec4<u32>,
    ring: vec4<f32>,
    opts: vec4<u32>,
}
struct Args {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
}
@group(0) @binding(0) var<uniform> view: SwView;
@group(0) @binding(1) var<storage, read> records: array<SwRecord>;
@group(0) @binding(2) var<storage, read> candidates: array<u32>;
@group(0) @binding(3) var<storage, read_write> args: array<Args>;
@group(0) @binding(4) var<storage, read> bin_first: array<u32>;
@group(0) @binding(5) var<storage, read_write> counts: array<atomic<u32>>;
@group(0) @binding(6) var<storage, read_write> vis_bits: array<u32>;
@group(0) @binding(7) var hiz: texture_2d<f32>;

// Hi-Z (static_world/hiz.rs): is the box (centre c, half extents e) behind the pyramid's farthest depth over its screen
// footprint? Reverse-Z: nearer = larger. A corner behind the eye, or a footprint wider than 2x2 texels at the chosen
// level, counts as visible.
fn occluded(c: vec3<f32>, e: vec3<f32>) -> bool {
    var lo = vec2<f32>(1.0e9);
    var hi = vec2<f32>(-1.0e9);
    var zmax = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let s = vec3<f32>(select(-1.0, 1.0, (k & 1u) != 0u), select(-1.0, 1.0, (k & 2u) != 0u), select(-1.0, 1.0, (k & 4u) != 0u));
        let clip = view.clip_from_world * vec4<f32>(c + e * s, 1.0);
        if clip.w <= 1.0e-4 {
            return false;
        }
        let ndc = clip.xyz / clip.w;
        let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
        lo = min(lo, uv);
        hi = max(hi, uv);
        zmax = max(zmax, ndc.z);
    }
    lo = clamp(lo, vec2<f32>(0.0), vec2<f32>(1.0));
    hi = clamp(hi, vec2<f32>(0.0), vec2<f32>(1.0));
    let levels = max(view.extra.y, 1u);
    let size = vec2<f32>(textureDimensions(hiz, 0));
    let ext = (hi - lo) * size;
    let level = u32(clamp(ceil(log2(max(max(ext.x, ext.y), 1.0))), 0.0, f32(levels - 1u)));
    let ls = vec2<i32>(textureDimensions(hiz, level));
    let a = clamp(vec2<i32>(lo * vec2<f32>(ls)), vec2<i32>(0), ls - vec2<i32>(1));
    let b = clamp(vec2<i32>(hi * vec2<f32>(ls)), vec2<i32>(0), ls - vec2<i32>(1));
    if b.x - a.x > 1 || b.y - a.y > 1 {
        return false;
    }
    let d = min(
        min(textureLoad(hiz, a, level).r, textureLoad(hiz, vec2<i32>(b.x, a.y), level).r),
        min(textureLoad(hiz, vec2<i32>(a.x, b.y), level).r, textureLoad(hiz, b, level).r),
    );
    return zmax < d;
}

const FLAG_CASTS: u32 = 1u;
const FLAG_LIVE: u32 = 4u;
const FLAG_MASK: u32 = 8u;
const FLAG_HIDDEN: u32 = 32u;
const SMALL_RADIUS: f32 = 1.5;

@compute @workgroup_size(64)
fn cull(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= view.info.z {
        return;
    }
    // Candidates are (slot, bin) pairs.
    let slot = candidates[2u * i];
    // P15-A: with the class split the bin word packs far | near << 10 | occluder << 20.
    let packed = candidates[2u * i + 1u];
    var bin = select(packed, packed & 1023u, view.opts.x != 0u);
    let r = records[slot];
    var visible = (r.flags & FLAG_LIVE) != 0u && (r.flags & FLAG_HIDDEN) == 0u && r.index_count > 0u;
    let c = 0.5 * (r.aabb_min.xyz + r.aabb_max.xyz);
    let e = 0.5 * (r.aabb_max.xyz - r.aabb_min.xyz);
    let radius = length(e);
    // LOD band (the record's VisibilityRange margins), from the LOD eye to the placement origin.
    let origin = vec3<f32>(r.rows[0].w, r.rows[1].w, r.rows[2].w);
    let d = distance(view.eye.xyz, origin) * view.eye.w;
    if d < r.lod.x || d >= r.lod.w {
        visible = false;
    }
    let kind = view.info.x;
    let is_mask = (r.flags & FLAG_MASK) != 0u;
    // Inside a LOD fade band or a zone fade (the main pass dithers it: material.rs rm_dither).
    let in_band = r.tag != 0 || d < r.lod.y || d >= r.lod.z;
    if kind == 1u {
        // Shadow cascades: casters only; small casters only near the main camera.
        if (r.flags & FLAG_CASTS) == 0u {
            visible = false;
        }
        if view.params.z > 0.0 && radius < SMALL_RADIUS && d > view.params.z {
            visible = false;
        }
        // P15-A fades: bindless cutouts dither in their own alpha-test stage. Mode 1: other in-band records go to the
        // dithering class (near bin). Mode 2 (no class split): one level casts, switched at the band centres (the
        // neighbouring LOD's margins are centred on the same switch distance); zones cast while at least half drawn.
        let own = is_mask && view.opts.z != 0u;
        if view.opts.y == 1u && !own && in_band {
            bin = (packed >> 10u) & 1023u;
        }
        if view.opts.y == 2u && !own {
            if d < 0.5 * (r.lod.x + r.lod.y) || d >= 0.5 * (r.lod.z + r.lod.w) || r.tag <= -8 || r.tag > 8 {
                visible = false;
            }
        }
    }
    // P15-A main view classes (distance to the box): near cutouts / near opaque, and the pre-pass occluders: big, fully
    // drawn (no fade dither: the pre-pass would punch holes where the lit pass discards) opaque records.
    if view.opts.x != 0u && view.ring.z > 0.0 {
        let dbox = length(max(abs(view.eye.xyz - c) - e, vec3<f32>(0.0)));
        if dbox < select(view.ring.z, view.ring.w, is_mask) {
            bin = (packed >> 10u) & 1023u;
        }
        let solid = r.tag == 0 && d >= r.lod.y + 0.5 && d < r.lod.z - 0.5;
        if !is_mask && view.ring.x > 0.0 && dbox < view.ring.x && radius >= view.ring.y && solid {
            bin = (packed >> 20u) & 1023u;
        }
    }
    if kind == 2u && radius < view.params.x {
        visible = false;
    }
    if view.params.y > 0.0 && distance(view.eye.xyz, c) - radius > view.params.y {
        visible = false;
    }
    let np = u32(view.params.w);
    for (var k = 0u; k < np; k = k + 1u) {
        let p = view.planes[k];
        if dot(p.xyz, c) + dot(abs(p.xyz), e) + p.w < 0.0 {
            visible = false;
        }
    }
    // Hi-Z phases (main view): 1 = only what was visible last frame; 2 = occlusion test of everything that passes, the
    // bit for next frame, and draw only the newly visible (phase 1 drew the rest).
    let phase = view.extra.x;
    if phase == 1u && vis_bits[i] == 0u {
        visible = false;
    }
    if phase == 2u {
        let before = vis_bits[i];
        var now = visible;
        if now && occluded(c, e) {
            now = false;
        }
        vis_bits[i] = select(0u, 1u, now);
        visible = now && before == 0u;
    }
    var a: Args;
    a.index_count = r.index_count;
    a.instance_count = select(0u, 1u, visible);
    a.first_index = r.first_index;
    a.base_vertex = i32(r.base_vertex);
    a.first_instance = slot;
#ifdef COMPACT
    if visible {
        let k = atomicAdd(&counts[view.info.w + bin], 1u);
        args[view.info.y + bin_first[bin] + k] = a;
    }
#else
    args[view.info.y + i] = a;
#endif
}
"#;

/// Depth-only cascade shader: vertex pulling from the arena; cutouts alpha-test layer A (bindless material table).
fn shadow_wgsl() -> String {
    let mut indices = String::from("struct SceneryIndices {\n    material: u32,\n");
    for name in crate::material::ROLES {
        indices += &format!("    {name}_texture: u32,\n    {name}_sampler: u32,\n");
    }
    indices += "}\n";
    let body = r#"
#import bevy_render::bindless::{bindless_samplers_filtering, bindless_textures_2d}
//RECORDS
@group(0) @binding(0) var<uniform> view: SwView;
@group(1) @binding(100) var<storage> sw_vertices: array<u32>;
@group(1) @binding(101) var<storage> sw_records: array<SwRecord>;

#ifdef TABLE
//INDICES
struct SceneryParams {
    uv: array<vec4<f32>, 6>,
    p: array<vec4<f32>, 4>,
    info: vec4<u32>,
    night: vec4<f32>,
}
@group(2) @binding(100) var<storage> scenery_indices: array<SceneryIndices>;
@group(2) @binding(101) var<storage> scenery_params: array<SceneryParams>;
#endif

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) slot: u32,
    @location(2) @interpolate(flat) dither: i32,
}

// P15-A: the main pass's fade level (material.rs rm_dither; Bevy's visibility range mapping) from the main camera's LOD
// eye, which the cascade's cull uniform carries: -16 .. 0 fading in, 0 drawn, 0 .. 16 fading out; zone tags as they are.
fn sw_dither(r: SwRecord) -> i32 {
    if r.tag != 0 {
        return r.tag;
    }
    let d = distance(view.eye.xyz, vec3<f32>(r.rows[0].w, r.rows[1].w, r.rows[2].w)) * view.eye.w;
    let offset = select(-16, 0, d >= r.lod.z);
    let b = select(r.lod.xy, r.lod.zw, d >= r.lod.z);
    return offset + clamp(i32(round((d - b.x) / max(b.y - b.x, 1e-4) * 16.0)), 0, 16);
}

@vertex
fn vertex(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> Out {
    let r = sw_records[ii];
    let m = transpose(mat4x4<f32>(r.rows[0], r.rows[1], r.rows[2], vec4<f32>(0.0, 0.0, 0.0, 1.0)));
    let b = vi * 13u;
    let pos = vec3<f32>(bitcast<f32>(sw_vertices[b]), bitcast<f32>(sw_vertices[b + 1u]), bitcast<f32>(sw_vertices[b + 2u]));
    var out: Out;
    out.position = view.clip_from_world * (m * vec4<f32>(pos, 1.0));
#ifdef CLAMP_DEPTH
    // No DEPTH_CLIP_CONTROL: keep casters in front of the cascade's near plane (reverse-Z ortho: z <= w).
    out.position.z = min(out.position.z, out.position.w);
#endif
    out.slot = r.material;
    out.uv = vec2<f32>(0.0);
    out.dither = 0;
#ifdef DITHER
    out.dither = sw_dither(r);
#endif
#ifdef PREPASS_FLAG
    // P18: the same test, done on the CPU per material (static_world.rs pre_ok -> FLAG_PRE_OK = 64).
    if (r.flags & 64u) == 0u {
        out.position = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
#else
#ifdef PREPASS
    // Only what the lit pass draws solid at exactly this position: no cloth wave (vertex animated), no cutout / decal /
    // water / additive classes, no decal mask. Collapsed to a point otherwise (no fragments).
    let q = scenery_params[scenery_indices[r.material].material];
    let cls = (q.info.z >> 16u) & 0xffu;
    if (q.info.z & 0x8080u) != 0u || cls == 1u || cls == 2u || cls == 4u || cls == 5u {
        out.position = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
#endif
#endif
#ifdef MASK
    let p = scenery_params[scenery_indices[r.material].material];
    let uv_set = p.info.x & 3u;
    var uv = vec2<f32>(bitcast<f32>(sw_vertices[b + 6u]), bitcast<f32>(sw_vertices[b + 7u]));
    if uv_set == 1u {
        uv = vec2<f32>(bitcast<f32>(sw_vertices[b + 8u]), bitcast<f32>(sw_vertices[b + 9u]));
    } else if uv_set >= 2u {
        uv = vec2<f32>(bitcast<f32>(sw_vertices[b + 10u]), bitcast<f32>(sw_vertices[b + 11u]));
    }
    out.uv = uv * p.uv[0].xy;
#endif
    return out;
}

#ifdef DITHER
// Bevy's DITHER_THRESHOLD_MAP (pbr_functions.wgsl), here in shadow-map texels: complementary levels of one placement
// cover each texel once, PCF turns that into a soft fade.
const SW_DITHER_MAP: vec4<u32> = vec4<u32>(0x0a020800u, 0x060e040cu, 0x09010b03u, 0x050d070fu);
#endif

#ifdef FRAG
@fragment
fn fragment(in: Out) {
#ifdef DITHER
    if in.dither != 0 {
        if in.dither <= -16 || in.dither >= 16 {
            discard;
        }
        let c = vec2<u32>(floor(in.position.xy)) % 4u;
        let t = i32((SW_DITHER_MAP[c.y] >> (c.x * 8u)) & 0xffu);
        if (in.dither >= 0 && in.dither + t >= 16) || (in.dither < 0 && 1 + in.dither + t <= 0) {
            discard;
        }
    }
#endif
#ifdef MASK
    let ix = scenery_indices[in.slot];
    let p = scenery_params[ix.material];
    let a = textureSampleLevel(bindless_textures_2d[ix.a_texture], bindless_samplers_filtering[ix.a_sampler], in.uv, 0.0).a;
    if a < p.p[2].w {
        discard;
    }
#endif
}
#endif
"#;
    body.replace("//RECORDS", RECORD_WGSL).replace("//INDICES", &indices)
}
