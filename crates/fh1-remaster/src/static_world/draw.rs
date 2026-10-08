//! P12 chunk 2: draws every static-world record through the remaster scenery shader (STATIC_WORLD variant) in the main
//! camera's opaque pass. No culling yet (chunk 3): every live, unhidden record is one indirect draw, binned by (bindless
//! material bind group, cull mode, alpha test); one `multi_draw_indexed_indirect` per bin. The draw lists are rebuilt on
//! the CPU only when the arena changed (streaming), not per frame.
//!
//! Pipeline: Bevy's `MeshPipeline::specialize` for the view's key (so every view / light / fog / atmosphere shader def
//! matches the ECS path) with a scenery vertex layout, then: no vertex buffers, group 2 = the arena (vertices + records,
//! bindings 100 / 101), group 3 = RemasterMaterial's bindless layout, the remaster shader with STATIC_WORLD + RM_* +
//! VISIBILITY_RANGE_DITHER (+ BINDLESS), the bin's cull mode.

use std::any::TypeId;
use std::collections::HashMap;

use bevy::core_pipeline::core_3d::main_opaque_pass_3d;
use bevy::core_pipeline::schedule::{Core3d, Core3dSystems};
use bevy::pbr::{MaterialBindGroupAllocators, MaterialBindGroupIndex, MeshPipeline, MeshPipelineKey, MeshViewBindGroup, RenderMaterialBindings, ViewKeyCache};
use bevy::prelude::*;
use bevy::render::camera::ExtractedCamera;
use bevy::render::mesh::{MeshVertexBufferLayoutRef, MeshVertexBufferLayouts};
use bevy::render::render_resource::binding_types::storage_buffer_read_only_sized;
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, Buffer, BufferDescriptor, BufferUsages, CachedRenderPipelineId, Face,
    IndexFormat, PipelineCache, RenderPassDescriptor, RenderPipelineDescriptor, ShaderStages, SpecializedMeshPipeline, SpecializedRenderPipeline,
    SpecializedRenderPipelines, StoreOp,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::view::{ExtractedView, ViewDepthTexture, ViewTarget};
use bevy::render::{Render, RenderSystems};
use bevy::shader::ShaderDefVal;

use super::{Arena, FLAG_CASTS, FLAG_HIDDEN, FLAG_LIVE, FLAG_MASK, FLAG_MIRRORED, FLAG_TWO_SIDED};
use crate::material::RemasterMaterial;

pub(super) fn plugin(ra: &mut SubApp) {
    ra.init_resource::<SpecializedRenderPipelines<SwPipeline>>()
        .init_resource::<DrawLists>()
        .add_systems(Render, (resolve_materials, build_lists).chain().in_set(RenderSystems::PrepareResources).after(super::apply_ops))
        .add_systems(Render, (init_pipeline, specialize_views).chain().in_set(RenderSystems::Queue))
        .add_systems(Render, prepare_bind_group.in_set(RenderSystems::PrepareBindGroups))
        .add_systems(Core3d, draw_static_world.after(main_opaque_pass_3d).in_set(Core3dSystems::MainPass));
}

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
}

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
        d.primitive.cull_mode = match key.variant.cull {
            0 => Some(Face::Back),
            1 => Some(Face::Front),
            _ => None,
        };
        d
    }
}

/// Creates the pipeline resource once the mesh pipeline exists.
fn init_pipeline(
    mut commands: Commands,
    existing: Option<Res<SwPipeline>>,
    mesh_pipeline: Option<Res<MeshPipeline>>,
    device: Res<RenderDevice>,
    mut layouts: ResMut<MeshVertexBufferLayouts>,
) {
    if existing.is_some() {
        return;
    }
    let Some(mp) = mesh_pipeline else { return };
    // The scenery vertex layout (prepare_mesh), only for the mesh pipeline's attribute defines.
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
    info!("static world: pipeline ready (bindless materials: {bindless})");
    commands.insert_resource(SwPipeline { mesh_pipeline: mp.clone(), layout_ref, arena_layout, material_layout, bindless });
}

/// The main camera's pipelines per variant (main camera only in chunk 2).
#[derive(Component, Default)]
pub(super) struct SwViewPipelines(HashMap<Variant, CachedRenderPipelineId>);

/// The views the static world draws into: the main 3D camera (order 0).
fn is_main(camera: &ExtractedCamera) -> bool {
    camera.order == 0
}

fn specialize_views(
    mut commands: Commands,
    pipeline: Option<Res<SwPipeline>>,
    mut pipelines: ResMut<SpecializedRenderPipelines<SwPipeline>>,
    cache: Res<PipelineCache>,
    keys: Res<ViewKeyCache>,
    lists: Res<DrawLists>,
    views: Query<(Entity, &ExtractedView, &ExtractedCamera), With<ViewTarget>>,
) {
    let Some(pipeline) = pipeline else { return };
    for (e, view, camera) in &views {
        if !is_main(camera) {
            continue;
        }
        let Some(view_key) = keys.get(&view.retained_view_entity) else { continue };
        let mesh_key = *view_key | MeshPipelineKey::from_primitive_topology_and_strip_index(bevy::mesh::PrimitiveTopology::TriangleList, None);
        let mut map = HashMap::new();
        for v in lists.variants() {
            map.insert(v, pipelines.specialize(&cache, &pipeline, SwKey { mesh_key, variant: v }));
        }
        commands.entity(e).insert(SwViewPipelines(map));
    }
}

/// Looks up the bindless slot of each record waiting for its material.
fn resolve_materials(mut arena: ResMut<Arena>, bindings: Res<RenderMaterialBindings>, device: Res<RenderDevice>, queue: Res<RenderQueue>) {
    if arena.pending.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut arena.pending);
    let mut still = Vec::new();
    for slot in pending {
        let s = slot as usize;
        let Some(Some((_, material))) = arena.slot_mesh.get(s).cloned() else { continue };
        match bindings.get(&material) {
            Some(b) => {
                arena.records[s].material = b.slot.0;
                arena.slot_group[s] = Some(b.group.0);
                arena.write_record(&device, &queue, slot);
                arena.dirty = true;
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

/// A run of draws sharing a material bind group and a variant.
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
    args: Option<Buffer>,
    args_cap: u64,
    bind_group: Option<(u32, BindGroup)>,
    pub(super) draws: u32,
}

impl DrawLists {
    fn variants(&self) -> Vec<Variant> {
        let mut v: Vec<Variant> = self.bins.iter().map(|b| b.variant).collect();
        v.sort_by_key(|x| (x.cull, x.mask));
        v.dedup();
        v
    }
}

/// Rebuilds the indirect args when the arena changed (streaming), binned by (group, variant).
fn build_lists(mut arena: ResMut<Arena>, mut lists: ResMut<DrawLists>, device: Res<RenderDevice>, queue: Res<RenderQueue>) {
    if !arena.dirty {
        return;
    }
    arena.dirty = false;
    let mut by_bin: HashMap<(u32, Variant), Vec<Args>> = HashMap::new();
    for (s, r) in arena.records.iter().enumerate() {
        if r.flags & FLAG_LIVE == 0 || r.flags & FLAG_HIDDEN != 0 || r.index_count == 0 {
            continue;
        }
        let Some(Some(group)) = arena.slot_group.get(s) else { continue };
        let a = Args { index_count: r.index_count, instance_count: 1, first_index: r.first_index, base_vertex: r.base_vertex as i32, first_instance: s as u32 };
        by_bin.entry((*group, Variant::of(r.flags))).or_default().push(a);
    }
    let _ = FLAG_CASTS;
    let mut all: Vec<Args> = Vec::new();
    let mut bins = Vec::new();
    let mut keys: Vec<(u32, Variant)> = by_bin.keys().copied().collect();
    keys.sort_by_key(|(g, v)| (*g, v.cull, v.mask));
    for k in keys {
        let v = &by_bin[&k];
        bins.push(Bin { group: k.0, variant: k.1, first: all.len() as u32, count: v.len() as u32 });
        all.extend_from_slice(v);
    }
    lists.draws = all.len() as u32;
    lists.bins = bins;
    let bytes = std::mem::size_of_val(all.as_slice()) as u64;
    if bytes == 0 {
        return;
    }
    if lists.args.is_none() || lists.args_cap < bytes {
        let cap = bytes.max(lists.args_cap * 3 / 2).max(1 << 20);
        lists.args = Some(device.create_buffer(&BufferDescriptor { label: Some("static world indirect"), size: cap, usage: BufferUsages::INDIRECT | BufferUsages::COPY_DST, mapped_at_creation: false }));
        lists.args_cap = cap;
    }
    if let Some(b) = &lists.args {
        // SAFETY: Args is repr(C) of 4-byte fields.
        let raw = unsafe { std::slice::from_raw_parts(all.as_ptr() as *const u8, bytes as usize) };
        queue.write_buffer(b, 0, raw);
    }
}

/// The arena bind group (group 2), rebuilt when a buffer is re-created.
fn prepare_bind_group(arena: Res<Arena>, mut lists: ResMut<DrawLists>, pipeline: Option<Res<SwPipeline>>, cache: Res<PipelineCache>, device: Res<RenderDevice>) {
    let Some(pipeline) = pipeline else { return };
    if lists.bind_group.as_ref().is_some_and(|(g, _)| *g == arena.buffers_generation) {
        return;
    }
    let (Some(v), Some(r)) = (arena.vertex_buffer(), arena.record_buffer.as_ref()) else { return };
    let layout = cache.get_bind_group_layout(&pipeline.arena_layout);
    let bg = device.create_bind_group("static world arena", &layout, &BindGroupEntries::with_indices(((100, v.as_entire_binding()), (101, r.as_entire_binding()))));
    lists.bind_group = Some((arena.buffers_generation, bg));
}

#[allow(clippy::too_many_arguments)]
fn draw_static_world(
    view: ViewQuery<(&ExtractedCamera, &ViewTarget, &ViewDepthTexture, &MeshViewBindGroup, Option<&SwViewPipelines>)>,
    arena: Res<Arena>,
    lists: Res<DrawLists>,
    cache: Res<PipelineCache>,
    allocators: Res<MaterialBindGroupAllocators>,
    mut ctx: RenderContext,
) {
    let (camera, target, depth, view_bg, pipelines) = view.into_inner();
    if !is_main(camera) || lists.bins.is_empty() {
        return;
    }
    let (Some(pipelines), Some((_, arena_bg)), Some(args), Some(ib)) = (pipelines, lists.bind_group.as_ref(), lists.args.as_ref(), arena.index_buffer()) else { return };
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
        pass.multi_draw_indexed_indirect(args, bin.first as u64 * std::mem::size_of::<Args>() as u64, bin.count);
    }
}
