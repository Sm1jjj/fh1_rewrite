//! P12 static world drawing (docs/PERF.md "P12 static world").
//!
//! Chunk 2: every static record is drawn through the remaster scenery shader (STATIC_WORLD variant) with Bevy's own
//! bindless RemasterMaterial bind groups, binned by (material bind group, cull mode, alpha test).
//! Chunk 3: GPU culling. The CPU keeps one candidate list (record slots in bin order), rebuilt only when the arena
//! changes (streaming). Per view and frame, a compute pass tests every candidate (live / hidden flags, the record's LOD
//! band from the LOD eye, frustum vs world AABB, per-kind rules) and writes its `DrawIndexedIndirect` args into the view's
//! own region with instance_count 1 or 0; each bin is then one `multi_draw_indexed_indirect`. Per-frame CPU work is
//! O(views x bins), independent of the record count.
//! Compaction (chunk 5, default when the GPU has MULTI_DRAW_INDIRECT_COUNT; `FH1_STATIC_WORLD_COMPACT=0` = off): visible
//! candidates are appended per (view, bin) with an atomic counter and each bin is drawn with
//! `multi_draw_indexed_indirect_count`, so culled entries cost nothing (no zero-instance draws for the command processor).
//! Chunk 4: the same cull feeds (a) the car probe faces (fh1-remaster car_probe.rs `CarProbeFace`, 47's rules: radius >=
//! `probe_min_radius()`, within the face's far plane) and (b) a depth-only pass into each directional shadow cascade of the
//! main camera after Bevy's shadow pass (records with the casts flag; small casters (half-diagonal < 1.5 m) only within
//! FH1_SHADOW_SMALL_DIST of the main camera; cascades in `DirectionalShadowSkipThisFrame` are left alone; cutouts
//! alpha-tested on layer A). `FH1_STATIC_WORLD_CULL=0` = no culling (chunk 2), `FH1_STATIC_WORLD_SHADOWS=0` = no static
//! shadows, `FH1_STATIC_WORLD_PROBE=0` = static scenery not in the probe.

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
use bevy::render::render_resource::binding_types::{storage_buffer_read_only_sized, storage_buffer_sized, uniform_buffer};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, Buffer, BufferDescriptor, BufferUsages, CachedComputePipelineId,
    CachedRenderPipelineId, CompareFunction, ComputePassDescriptor, ComputePipelineDescriptor, DepthBiasState, DepthStencilState, DynamicUniformBuffer,
    Face, FragmentState, IndexFormat, PipelineCache, PrimitiveState, RenderPassDescriptor, RenderPipelineDescriptor, ShaderStages, ShaderType,
    SpecializedMeshPipeline, SpecializedRenderPipeline, SpecializedRenderPipelines, StencilState, StoreOp, VertexState,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::settings::WgpuFeatures;
use bevy::render::view::{ExtractedView, ViewDepthTexture, ViewTarget};
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

/// Main world: the two shaders.
pub(super) fn register_shaders(app: &mut App) {
    let mut shaders = app.world_mut().resource_mut::<Assets<Shader>>();
    let _ = shaders.insert(&CULL_SHADER, Shader::from_wgsl(CULL_WGSL, "fh1_remaster/static_world_cull.wgsl"));
    let _ = shaders.insert(&SHADOW_SHADER, Shader::from_wgsl(shadow_wgsl(), "fh1_remaster/static_world_shadow.wgsl"));
}

pub(super) fn plugin(ra: &mut SubApp) {
    ra.init_resource::<SpecializedRenderPipelines<SwPipeline>>()
        .init_resource::<SpecializedRenderPipelines<ShadowPipeline>>()
        .init_resource::<DrawLists>()
        .init_resource::<ViewUniforms>()
        .add_systems(Render, (resolve_materials, build_lists).chain().in_set(RenderSystems::PrepareResources).after(super::apply_ops))
        .add_systems(Render, (init_pipelines, specialize_views).chain().in_set(RenderSystems::Queue))
        .add_systems(Render, (prepare_views, prepare_bind_groups).chain().in_set(RenderSystems::PrepareBindGroups))
        .add_systems(Core3d, draw_static_shadows.after(bevy::pbr::per_view_shadow_pass::<true>).before(Core3dSystems::MainPass))
        .add_systems(Core3d, draw_static_world.after(main_opaque_pass_3d).in_set(Core3dSystems::MainPass));
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

/// Depth-only cascade pipeline: [cull view uniform, arena, material].
#[derive(Resource)]
pub(super) struct ShadowPipeline {
    view_layout: BindGroupLayoutDescriptor,
    arena_layout: BindGroupLayoutDescriptor,
    material_layout: BindGroupLayoutDescriptor,
    bindless: bool,
    unclipped: bool,
}

impl SpecializedRenderPipeline for ShadowPipeline {
    type Key = Variant;

    fn specialize(&self, v: Variant) -> RenderPipelineDescriptor {
        let mut defs: Vec<ShaderDefVal> = vec![ShaderDefVal::UInt("MATERIAL_BIND_GROUP".into(), 2)];
        if self.bindless {
            defs.push("BINDLESS".into());
        }
        if !self.unclipped {
            defs.push("CLAMP_DEPTH".into());
        }
        // Cutouts are alpha-tested only with bindless materials (the plain layout has no index table).
        let mask = v.mask && self.bindless;
        if mask {
            defs.push("MASK".into());
        }
        RenderPipelineDescriptor {
            label: Some("static world shadow".into()),
            layout: vec![self.view_layout.clone(), self.arena_layout.clone(), self.material_layout.clone()],
            vertex: VertexState { shader: SHADOW_SHADER, shader_defs: defs.clone(), entry_point: Some("vertex".into()), buffers: Vec::new() },
            fragment: mask.then(|| FragmentState { shader: SHADOW_SHADER, shader_defs: defs, entry_point: Some("fragment".into()), targets: Vec::new() }),
            primitive: PrimitiveState { cull_mode: v.face(), unclipped_depth: self.unclipped, ..default() },
            depth_stencil: Some(DepthStencilState {
                format: CORE_3D_DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(CompareFunction::GreaterEqual),
                stencil: StencilState::default(),
                bias: DepthBiasState::default(),
            }),
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
            ((100, storage_buffer_read_only_sized(false, None)), (101, storage_buffer_read_only_sized(false, None))),
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
            ),
        ),
    );
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

/// Per drawn view: the lit pipelines per variant (main / probe).
#[derive(Component, Default)]
pub(super) struct SwViewPipelines(HashMap<Variant, CachedRenderPipelineId>);

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
        let map = variants.iter().map(|&v| (v, pipelines.specialize(&cache, &pipeline, SwKey { mesh_key, variant: v }))).collect();
        commands.entity(e).insert(SwViewPipelines(map));
    }
    lists.shadow_pipelines = variants.iter().map(|&v| (v, shadow_pipelines.specialize(&cache, &shadow, v))).collect();
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
    for slot in pending {
        let s = slot as usize;
        let Some(Some((_, material))) = arena.slot_mesh.get(s).cloned() else { continue };
        match bindings.get(&material) {
            Some(b) => {
                if arena.records[s].material != b.slot.0 || arena.slot_group[s] != Some(b.group.0) {
                    arena.records[s].material = b.slot.0;
                    arena.slot_group[s] = Some(b.group.0);
                    arena.write_record(&device, &queue, slot);
                    arena.dirty = true;
                }
            }
            None => still.push(slot),
        }
    }
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
    first: u32,
    count: u32,
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
    arena_bind_group: Option<(u32, BindGroup)>,
    shadow_pipelines: HashMap<Variant, CachedRenderPipelineId>,
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
    let mut by_bin: HashMap<(u32, Variant), Vec<u32>> = HashMap::new();
    for (s, r) in arena.records.iter().enumerate() {
        // With culling, hidden records stay candidates (the GPU checks the flag): a zone switch doesn't rebuild the list.
        if r.flags & FLAG_LIVE == 0 || (!cull && r.flags & FLAG_HIDDEN != 0) || r.index_count == 0 {
            continue;
        }
        let Some(Some(group)) = arena.slot_group.get(s) else { continue };
        by_bin.entry((*group, Variant::of(r.flags))).or_default().push(s as u32);
    }
    let mut keys: Vec<(u32, Variant)> = by_bin.keys().copied().collect();
    keys.sort_by_key(|(g, v)| (*g, v.cull, v.mask));
    let mut slots: Vec<u32> = Vec::new();
    let mut bins = Vec::new();
    for k in keys {
        let v = &by_bin[&k];
        bins.push(Bin { group: k.0, variant: k.1, first: slots.len() as u32, count: v.len() as u32 });
        slots.extend_from_slice(v);
    }
    lists.bins = bins;
    lists.count = slots.len() as u32;
    if slots.is_empty() {
        return;
    }
    // Args: one region per view (culling) or one region (no culling). Re-created when the candidates outgrow it.
    let region = slots.len() as u32;
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
    // Candidates as (slot, bin) pairs; the bins' first arg entries; the per-(view, bin) counts.
    let mut pairs: Vec<u32> = Vec::with_capacity(slots.len() * 2);
    for (b, bin) in lists.bins.iter().enumerate() {
        for &slot in &slots[bin.first as usize..(bin.first + bin.count) as usize] {
            pairs.push(slot);
            pairs.push(b as u32);
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
}

const KIND_MAIN: u32 = 0;
const KIND_SHADOW: u32 = 1;
const KIND_PROBE: u32 = 2;

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
    cameras: Query<(Entity, &ExtractedView, &ExtractedCamera, Option<&CarProbeFace>, Option<&ViewLightEntities>), With<ViewTarget>>,
    lights: Query<(&ExtractedView, Option<&LightEntity>), With<ShadowView>>,
    stale: Query<Entity, With<SwCullView>>,
) {
    // Views not drawn this frame (a cached cascade, an idle probe face) must not keep last frame's slot.
    for e in &stale {
        commands.entity(e).remove::<SwCullView>();
    }
    let u = &mut *uniforms;
    u.buffer.clear();
    CANDIDATES.store(lists.count, std::sync::atomic::Ordering::Relaxed);
    VIEWS.store(0, std::sync::atomic::Ordering::Relaxed);
    if !cull_on() || lists.count == 0 {
        return;
    }
    let (count, region, bins_cap) = (lists.count, lists.region, lists.bins_cap);
    let mut slot = 0u32;
    let mut push = |commands: &mut Commands, e: Entity, vu: ViewUniform, buffer: &mut DynamicUniformBuffer<ViewUniform>| {
        if slot >= MAX_VIEWS {
            return;
        }
        let base = slot * region;
        let counts = slot * bins_cap;
        let offset = buffer.push(&ViewUniform { info: UVec4::new(vu.info.x, base, count, counts), ..vu });
        commands.entity(e).insert(SwCullView { offset, base, counts });
        slot += 1;
    };
    let skip_mask = skip.map_or(0, |s| s.0);
    for (e, view, camera, face, view_lights) in &cameras {
        let face = probe_face(face);
        let main = is_main(camera);
        if !main && !(probe_on() && face.is_some()) {
            continue;
        }
        let cfw = clip_from_world(view);
        let eye = view.world_from_view.translation();
        let (p, n) = planes(&cfw, true);
        let vu = if main {
            ViewUniform { clip_from_world: cfw, planes: p, eye: eye.extend(1.0), params: Vec4::new(0.0, 0.0, 0.0, n as f32), info: UVec4::new(KIND_MAIN, 0, 0, 0) }
        } else {
            // 47's probe rules: radius >= probe_min_radius, within the face's far plane (80 m, down face 8 m).
            let far = if face == Some(3) { 8.0 } else { 80.0 };
            ViewUniform {
                clip_from_world: cfw,
                planes: p,
                eye: eye.extend(1.0),
                params: Vec4::new(crate::car_probe::probe_min_radius(), far, 0.0, n as f32),
                info: UVec4::new(KIND_PROBE, 0, 0, 0),
            }
        };
        push(&mut commands, e, vu, &mut u.buffer);
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
                    eye: eye.extend(1.0),
                    params: Vec4::new(0.0, 0.0, small_dist(), n as f32),
                    info: UVec4::new(KIND_SHADOW, 0, 0, 0),
                };
                push(&mut commands, le, vu, &mut u.buffer);
            }
        }
    }
    VIEWS.store(slot, std::sync::atomic::Ordering::Relaxed);
    u.buffer.write_buffer(&device, &queue);
}

/// Bind groups: the arena (draws), the cull inputs / outputs, the shadow view uniform.
#[allow(clippy::too_many_arguments)]
fn prepare_bind_groups(
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
    if !lists.arena_bind_group.as_ref().is_some_and(|(g, _)| *g == arena.buffers_generation) {
        if let (Some(v), Some(r)) = (arena.vertex_buffer(), arena.record_buffer.as_ref()) {
            let layout = cache.get_bind_group_layout(&pipeline.arena_layout);
            let bg = device.create_bind_group("static world arena", &layout, &BindGroupEntries::with_indices(((100, v.as_entire_binding()), (101, r.as_entire_binding()))));
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
        u.cull_bind_group = Some(device.create_bind_group(
            "static world cull",
            &layout,
            &BindGroupEntries::sequential((view_binding.clone(), r.as_entire_binding(), c.as_entire_binding(), a.as_entire_binding(), bf.as_entire_binding(), n.as_entire_binding())),
        ));
    }
    let layout = cache.get_bind_group_layout(&shadow.view_layout);
    u.shadow_view_bind_group = Some(device.create_bind_group("static world shadow view", &layout, &BindGroupEntries::single(view_binding)));
}

// ---------------------------------------------------------------- passes

/// Culls the view's candidates into its args region.
fn dispatch_cull(ctx: &mut RenderContext, cache: &PipelineCache, cull: &CullPipeline, uniforms: &ViewUniforms, lists: &DrawLists, view: &SwCullView, count: u32) -> bool {
    let (Some(p), Some(bg)) = (cache.get_compute_pipeline(cull.id), uniforms.cull_bind_group.as_ref()) else { return false };
    if cull.compact {
        let Some(counts) = lists.counts.as_ref() else { return false };
        ctx.command_encoder().clear_buffer(counts, view.counts as u64 * 4, Some(lists.bins_cap as u64 * 4));
    }
    let mut pass = ctx.command_encoder().begin_compute_pass(&ComputePassDescriptor { label: Some("static world cull"), timestamp_writes: None });
    pass.set_pipeline(p);
    pass.set_bind_group(0, bg, &[view.offset]);
    pass.dispatch_workgroups(count.div_ceil(64), 1, 1);
    true
}

#[allow(clippy::too_many_arguments)]
fn draw_static_world(
    view: ViewQuery<(&ExtractedCamera, &ViewTarget, &ViewDepthTexture, &MeshViewBindGroup, Option<&SwViewPipelines>, Option<&SwCullView>)>,
    arena: Res<Arena>,
    lists: Res<DrawLists>,
    uniforms: Res<ViewUniforms>,
    cull: Option<Res<CullPipeline>>,
    cache: Res<PipelineCache>,
    allocators: Res<MaterialBindGroupAllocators>,
    mut ctx: RenderContext,
) {
    let (camera, target, depth, view_bg, pipelines, cull_view) = view.into_inner();
    let (Some(pipelines), Some((_, arena_bg)), Some(args), Some(ib)) = (pipelines, lists.arena_bind_group.as_ref(), lists.args.as_ref(), arena.index_buffer()) else { return };
    if lists.bins.is_empty() {
        return;
    }
    // Culled: this view's region (+ its draw counts when compacted); unculled (FH1_STATIC_WORLD_CULL=0): region 0, main
    // camera only.
    let counts = cull.as_ref().filter(|c| c.compact && cull_on()).and(lists.counts.as_ref()).zip(cull_view.map(|v| v.counts));
    let base = if cull_on() {
        let (Some(cv), Some(cull)) = (cull_view, cull.as_ref()) else { return };
        if !dispatch_cull(&mut ctx, &cache, cull, &uniforms, &lists, cv, lists.count) {
            return;
        }
        cv.base
    } else {
        if !is_main(camera) {
            return;
        }
        0
    };
    let Some(allocator) = allocators.get(&TypeId::of::<RemasterMaterial>()) else { return };
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
    for bin in &lists.bins {
        let Some(id) = pipelines.0.get(&bin.variant) else { continue };
        let Some(p) = cache.get_render_pipeline(*id) else { continue };
        let Some(slab) = allocator.get(MaterialBindGroupIndex(bin.group)) else { continue };
        let Some(material_bg) = slab.bind_group() else { continue };
        pass.set_render_pipeline(p);
        pass.set_bind_group(3, material_bg, &[]);
        draw_bin(&mut pass, args, counts, base, bin, &lists);
    }
}

/// One bin's draws: compacted (indirect count) or every candidate (zero-instance when culled).
fn draw_bin<'a>(pass: &mut bevy::render::render_phase::TrackedRenderPass<'a>, args: &'a Buffer, counts: Option<(&'a Buffer, u32)>, base: u32, bin: &Bin, lists: &DrawLists) {
    let offset = (base + bin.first) as u64 * ARGS_BYTES;
    match counts {
        Some((cb, cbase)) => {
            let b = lists.bins.iter().position(|x| x.first == bin.first).unwrap_or(0) as u32;
            pass.multi_draw_indexed_indirect_count(args, offset, cb, (cbase + b) as u64 * 4, bin.count);
        }
        None => pass.multi_draw_indexed_indirect(args, offset, bin.count),
    }
}

/// Static scenery into the main camera's directional cascades (after Bevy's shadow pass, same depth attachments).
#[allow(clippy::too_many_arguments)]
fn draw_static_shadows(
    view: ViewQuery<&ViewLightEntities>,
    lights: Query<(&ShadowView, Option<&SwCullView>)>,
    arena: Res<Arena>,
    lists: Res<DrawLists>,
    uniforms: Res<ViewUniforms>,
    cull: Option<Res<CullPipeline>>,
    cache: Res<PipelineCache>,
    allocators: Res<MaterialBindGroupAllocators>,
    mut ctx: RenderContext,
) {
    if !shadows_on() || !cull_on() || lists.bins.is_empty() {
        return;
    }
    let view_lights = view.into_inner();
    let (Some(cull), Some((_, arena_bg)), Some(args), Some(ib), Some(view_bg)) =
        (cull.as_ref(), lists.arena_bind_group.as_ref(), lists.args.as_ref(), arena.index_buffer(), uniforms.shadow_view_bind_group.as_ref())
    else {
        return;
    };
    let Some(allocator) = allocators.get(&TypeId::of::<RemasterMaterial>()) else { return };
    let counts = if cull.compact { lists.counts.as_ref() } else { None };
    for &le in &view_lights.lights {
        // Only the cascades prepare_views gave a slot (skipped / cached cascades have none).
        let Ok((shadow_view, Some(cv))) = lights.get(le) else { continue };
        if !dispatch_cull(&mut ctx, &cache, cull, &uniforms, &lists, cv, lists.count) {
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
        for bin in &lists.bins {
            let Some(id) = lists.shadow_pipelines.get(&bin.variant) else { continue };
            let Some(p) = cache.get_render_pipeline(*id) else { continue };
            let Some(slab) = allocator.get(MaterialBindGroupIndex(bin.group)) else { continue };
            let Some(material_bg) = slab.bind_group() else { continue };
            pass.set_render_pipeline(p);
            pass.set_bind_group(2, material_bg, &[]);
            draw_bin(&mut pass, args, counts.map(|c| (c, cv.counts)), cv.base, bin, &lists);
        }
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

const FLAG_CASTS: u32 = 1u;
const FLAG_LIVE: u32 = 4u;
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
    let bin = candidates[2u * i + 1u];
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
    if kind == 1u {
        // Shadow cascades: casters only; small casters only near the main camera.
        if (r.flags & FLAG_CASTS) == 0u {
            visible = false;
        }
        if view.params.z > 0.0 && radius < SMALL_RADIUS && d > view.params.z {
            visible = false;
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

#ifdef MASK
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

#ifdef MASK
@fragment
fn fragment(in: Out) {
    let ix = scenery_indices[in.slot];
    let p = scenery_params[ix.material];
    let a = textureSampleLevel(bindless_textures_2d[ix.a_texture], bindless_samplers_filtering[ix.a_sampler], in.uv, 0.0).a;
    if a < p.p[2].w {
        discard;
    }
}
#endif
"#;
    body.replace("//RECORDS", RECORD_WGSL).replace("//INDICES", &indices)
}
