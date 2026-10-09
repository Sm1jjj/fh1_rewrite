//! P12 chunk 5: Hi-Z occlusion for the static world's main view (docs/PERF.md "P12 static world", 47).
//!
//! Bevy's own depth pyramid (OcclusionCulling) is built from its depth prepass, which the static scenery (the occluders)
//! isn't in, so the static world keeps its own. Two phases inside draw.rs `draw_static_world` for the main camera:
//! 1. cull with the visibility bit of last frame (`bits`, one u32 per candidate): draw what was visible;
//! 2. build a min-depth pyramid from the main depth (reverse-Z: the min of a 2x2 block = its farthest surface), then
//!    test every frustum / LOD-passing candidate's projected AABB against it (nearest corner depth < the footprint's
//!    farthest occluder = hidden), store the bit, and draw the newly visible ones.
//! A rebuilt candidate list resets every bit to visible (no holes); without a usable depth (multisampled, no
//! TEXTURE_BINDING) the main view culls once, as before. `FH1_STATIC_WORLD_HIZ=0` = off.

use bevy::prelude::*;
use bevy::render::render_resource::binding_types::{texture_2d, texture_depth_2d, texture_storage_2d};
use bevy::render::diagnostic::RecordDiagnostics;
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, Buffer, BufferDescriptor, BufferUsages, CachedComputePipelineId,
    ComputePassDescriptor, ComputePipelineDescriptor, Extent3d, PipelineCache, ShaderStages, StorageTextureAccess, Texture, TextureDescriptor,
    ComputePass, TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureView, TextureViewDescriptor, TextureViewId,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue};

pub(super) const FIRST_SHADER: Handle<Shader> = bevy::asset::uuid_handle!("5b2d8e91-4c7a-4f03-9e6b-1a8c3d7f2e50");
pub(super) const DOWN_SHADER: Handle<Shader> = bevy::asset::uuid_handle!("8a4f1c63-2e9d-4b78-a5c0-6d3e9f1b7a24");

/// `FH1_STATIC_WORLD_HIZ=0`: no occlusion test (one cull per main view, as in chunk 4).
pub fn hiz_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_STATIC_WORLD_HIZ").map_or(true, |v| v != "0"))
}

pub(super) fn register_shaders(shaders: &mut Assets<Shader>) {
    let _ = shaders.insert(&FIRST_SHADER, Shader::from_wgsl(FIRST_WGSL, "fh1_remaster/static_world_hiz_first.wgsl"));
    let _ = shaders.insert(&DOWN_SHADER, Shader::from_wgsl(DOWN_WGSL, "fh1_remaster/static_world_hiz_down.wgsl"));
}

/// The two downsample pipelines.
#[derive(Resource)]
pub(super) struct HizPipelines {
    first_layout: BindGroupLayoutDescriptor,
    down_layout: BindGroupLayoutDescriptor,
    first: CachedComputePipelineId,
    down: CachedComputePipelineId,
}

impl HizPipelines {
    pub(super) fn new(cache: &PipelineCache) -> Self {
        let first_layout = BindGroupLayoutDescriptor::new(
            "static world hiz first",
            &BindGroupLayoutEntries::sequential(ShaderStages::COMPUTE, (texture_depth_2d(), texture_storage_2d(TextureFormat::R32Float, StorageTextureAccess::WriteOnly))),
        );
        let down_layout = BindGroupLayoutDescriptor::new(
            "static world hiz down",
            &BindGroupLayoutEntries::sequential(
                ShaderStages::COMPUTE,
                (texture_2d(TextureSampleType::Float { filterable: false }), texture_storage_2d(TextureFormat::R32Float, StorageTextureAccess::WriteOnly)),
            ),
        );
        let first = cache.queue_compute_pipeline(ComputePipelineDescriptor {
            label: Some("static world hiz first".into()),
            layout: vec![first_layout.clone()],
            shader: FIRST_SHADER,
            entry_point: Some("first".into()),
            ..default()
        });
        let down = cache.queue_compute_pipeline(ComputePipelineDescriptor {
            label: Some("static world hiz down".into()),
            layout: vec![down_layout.clone()],
            shader: DOWN_SHADER,
            entry_point: Some("down".into()),
            ..default()
        });
        Self { first_layout, down_layout, first, down }
    }

    /// Both pipelines compiled.
    pub(super) fn ready(&self, cache: &PipelineCache) -> bool {
        cache.get_compute_pipeline(self.first).is_some() && cache.get_compute_pipeline(self.down).is_some()
    }
}

/// The pyramid, a 1x1 stand-in, and the per-candidate visibility bits.
#[derive(Resource, Default)]
pub(super) struct Hiz {
    texture: Option<Texture>,
    size: UVec2,
    mip_views: Vec<TextureView>,
    full_view: Option<TextureView>,
    dummy: Option<(Texture, TextureView)>,
    pub(super) bits: Option<Buffer>,
    bits_cap: u64,
    bits_generation: u64,
    /// Two-phase culling this frame (decided in draw.rs `prepare_views`: Hi-Z on, a single-sample main depth with
    /// TEXTURE_BINDING, pipelines ready).
    pub(super) active: bool,
    /// P16-A (`build_with`): the per-mip bind groups for the depth view they were made for (the pyramid's views are fixed
    /// until `ensure_pyramid` recreates it, which drops them).
    groups: std::sync::Mutex<Option<(TextureViewId, Vec<BindGroup>)>>,
}

impl Hiz {
    /// (Re)creates the pyramid for a depth target of `size` (mip 0 = full size, R32Float).
    pub(super) fn ensure_pyramid(&mut self, device: &RenderDevice, size: UVec2) {
        let size = size.max(UVec2::ONE);
        if self.texture.is_some() && self.size == size {
            return;
        }
        let mips = 32 - size.x.max(size.y).leading_zeros();
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("static world hiz"),
            size: Extent3d { width: size.x, height: size.y, depth_or_array_layers: 1 },
            mip_level_count: mips,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::R32Float,
            usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        self.mip_views = (0..mips)
            .map(|i| texture.create_view(&TextureViewDescriptor { label: Some("static world hiz mip"), base_mip_level: i, mip_level_count: Some(1), ..default() }))
            .collect();
        self.full_view = Some(texture.create_view(&TextureViewDescriptor::default()));
        self.texture = Some(texture);
        self.size = size;
        if let Ok(g) = self.groups.get_mut() {
            *g = None;
        }
    }

    /// Mip levels of the pyramid (0 without one).
    pub(super) fn mips(&self) -> u32 {
        self.mip_views.len() as u32
    }

    /// The pyramid's full view (bound in the cull bind group; phase 2 only reads it when `active`), else a 1x1 stand-in.
    pub(super) fn bind_view(&mut self, device: &RenderDevice) -> TextureView {
        match &self.full_view {
            Some(v) => v.clone(),
            None => self.dummy_view(device),
        }
    }

    fn dummy_view(&mut self, device: &RenderDevice) -> TextureView {
        if self.dummy.is_none() {
            let t = device.create_texture(&TextureDescriptor {
                label: Some("static world hiz stand-in"),
                size: Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::R32Float,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let v = t.create_view(&TextureViewDescriptor::default());
            self.dummy = Some((t, v));
        }
        self.dummy.as_ref().map(|d| d.1.clone()).unwrap()
    }

    /// One u32 per candidate; a new candidate list (`generation`) sets every bit to visible.
    pub(super) fn ensure_bits(&mut self, device: &RenderDevice, queue: &RenderQueue, count: u32, generation: u64) {
        let bytes = (count.max(1) as u64 * 4 + 15) & !15;
        if self.bits.is_none() || self.bits_cap < bytes {
            let cap = (bytes + bytes / 2).max(1 << 16);
            self.bits = Some(device.create_buffer(&BufferDescriptor { label: Some("static world hiz bits"), size: cap, usage: BufferUsages::STORAGE | BufferUsages::COPY_DST, mapped_at_creation: false }));
            self.bits_cap = cap;
            self.bits_generation = u64::MAX;
        }
        if self.bits_generation != generation {
            self.bits_generation = generation;
            if let Some(b) = &self.bits {
                let ones = vec![1u32; count.max(1) as usize];
                queue.write_buffer(b, 0, super::bytemuck_words(&ones));
            }
        }
    }

    /// Builds the pyramid from the main camera's depth (`depth` = a TEXTURE_BINDING view of a single-sample depth).
    pub(super) fn build(&self, ctx: &mut RenderContext, cache: &PipelineCache, pipes: &HizPipelines, device: &RenderDevice, depth: &TextureView) {
        let (Some(first), Some(down)) = (cache.get_compute_pipeline(pipes.first), cache.get_compute_pipeline(pipes.down)) else { return };
        if self.mip_views.is_empty() {
            return;
        }
        let first_layout = cache.get_bind_group_layout(&pipes.first_layout);
        let down_layout = cache.get_bind_group_layout(&pipes.down_layout);
        let groups: Vec<_> = std::iter::once(device.create_bind_group("static world hiz first", &first_layout, &BindGroupEntries::sequential((depth, &self.mip_views[0]))))
            .chain((1..self.mip_views.len()).map(|i| device.create_bind_group("static world hiz down", &down_layout, &BindGroupEntries::sequential((&self.mip_views[i - 1], &self.mip_views[i])))))
            .collect();
        let encoder = ctx.command_encoder();
        for (i, g) in groups.iter().enumerate() {
            let s = UVec2::new((self.size.x >> i).max(1), (self.size.y >> i).max(1));
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor { label: Some("static world hiz"), timestamp_writes: None });
            pass.set_pipeline(if i == 0 { first } else { down });
            pass.set_bind_group(0, g, &[]);
            pass.dispatch_workgroups(s.x.div_ceil(8), s.y.div_ceil(8), 1);
        }
    }

    /// P16-A (FH1_SW_PASS_MERGE): the pyramid in ONE compute pass (one dispatch per mip; wgpu puts a barrier between
    /// dispatches of a compute pass, as between passes), with bind groups kept while the depth view and the pyramid stay
    /// the same, then `tail` (the phase-2 cull) in the same pass. False = not ready, nothing recorded.
    pub(super) fn build_with(
        &self,
        ctx: &mut RenderContext,
        cache: &PipelineCache,
        pipes: &HizPipelines,
        device: &RenderDevice,
        depth: &TextureView,
        tail: &mut dyn FnMut(&mut ComputePass<'_>),
    ) -> bool {
        let (Some(first), Some(down)) = (cache.get_compute_pipeline(pipes.first), cache.get_compute_pipeline(pipes.down)) else { return false };
        if self.mip_views.is_empty() {
            return false;
        }
        let groups = {
            let Ok(mut cached) = self.groups.lock() else { return false };
            match cached.as_ref() {
                Some((id, g)) if *id == depth.id() && g.len() == self.mip_views.len() => g.clone(),
                _ => {
                    let first_layout = cache.get_bind_group_layout(&pipes.first_layout);
                    let down_layout = cache.get_bind_group_layout(&pipes.down_layout);
                    let g: Vec<BindGroup> = std::iter::once(device.create_bind_group("static world hiz first", &first_layout, &BindGroupEntries::sequential((depth, &self.mip_views[0]))))
                        .chain((1..self.mip_views.len()).map(|i| device.create_bind_group("static world hiz down", &down_layout, &BindGroupEntries::sequential((&self.mip_views[i - 1], &self.mip_views[i])))))
                        .collect();
                    *cached = Some((depth.id(), g.clone()));
                    g
                }
            }
        };
        let recorder = ctx.diagnostic_recorder();
        let diagnostics = recorder.as_deref();
        let mut pass = ctx.command_encoder().begin_compute_pass(&ComputePassDescriptor { label: Some("static world hiz + cull"), timestamp_writes: None });
        let span = diagnostics.pass_span(&mut pass, "static_world_hiz_cull");
        for (i, g) in groups.iter().enumerate() {
            let s = UVec2::new((self.size.x >> i).max(1), (self.size.y >> i).max(1));
            if i <= 1 {
                pass.set_pipeline(if i == 0 { first } else { down });
            }
            pass.set_bind_group(0, g, &[]);
            pass.dispatch_workgroups(s.x.div_ceil(8), s.y.div_ceil(8), 1);
        }
        tail(&mut pass);
        span.end(&mut pass);
        true
    }
}

/// Mip 0: the depth as it is.
const FIRST_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_depth_2d;
@group(0) @binding(1) var dst: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn first(@builtin(global_invocation_id) id: vec3<u32>) {
    let s = textureDimensions(dst);
    if id.x >= s.x || id.y >= s.y {
        return;
    }
    textureStore(dst, id.xy, vec4<f32>(textureLoad(src, id.xy, 0), 0.0, 0.0, 0.0));
}
"#;

/// Next mip: the min (reverse-Z: farthest) of the 2x2 block, and of the third row / column on an odd edge.
const DOWN_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn down(@builtin(global_invocation_id) id: vec3<u32>) {
    let s = textureDimensions(dst);
    if id.x >= s.x || id.y >= s.y {
        return;
    }
    let ss = textureDimensions(src, 0);
    let nx = select(2u, 3u, id.x == s.x - 1u && (ss.x & 1u) == 1u);
    let ny = select(2u, 3u, id.y == s.y - 1u && (ss.y & 1u) == 1u);
    var m = 1.0e9;
    for (var y = 0u; y < ny; y = y + 1u) {
        for (var x = 0u; x < nx; x = x + 1u) {
            let p = min(id.xy * 2u + vec2<u32>(x, y), ss - vec2<u32>(1u));
            m = min(m, textureLoad(src, p, 0).r);
        }
    }
    textureStore(dst, id.xy, vec4<f32>(m, 0.0, 0.0, 0.0));
}
"#;
