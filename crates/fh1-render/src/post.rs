//! Post-processing with the game's own post shaders (the containers embedded in default.xex).
//!
//! An [`FxPostChain`] is a list of fullscreen passes. Each pass runs a translated FH1 vertex +
//! pixel shader pair over a fullscreen triangle (the vertex shader's position0/texcoord0 inputs
//! are fed clip position and D3D-style uv), reads its inputs through the sampler registers it names
//! (pixel shader tf0-15, vertex shader tf16+), and writes a target: the view output, a per-frame
//! target sized relative to the view or fixed, or a persistent target that survives to the next
//! frame (luminance adaptation, bloom persistence). Constants live per pass in a uniform
//! (`vs`/`ps` register files + bools), set by the main world every frame.
//!
//! The FH1 pass list and constants come from the host-code RE (docs/SHADERS.md, "Post chain");
//! see `postfx.rs`.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use bevy::core_pipeline::schedule::Core3d;
use bevy::core_pipeline::tonemapping::tonemapping;
use bevy::core_pipeline::Core3dSystems;
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{sampler, texture_2d, texture_3d, texture_depth_2d, texture_depth_2d_multisampled, uniform_buffer_sized};
use bevy::render::render_resource::{
    BindGroupEntry, BindGroupLayoutDescriptor, Buffer, BufferDescriptor, Sampler, BindingResource, BufferInitDescriptor, BufferUsages, CachedRenderPipelineId,
    ColorTargetState, ColorWrites, Extent3d, FragmentState, Operations, PipelineCache, RenderPassColorAttachment,
    RenderPassDescriptor, RenderPipelineDescriptor, SamplerBindingType, SamplerDescriptor, ShaderStages, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureView,
    TextureViewDescriptor, VertexState,
};
use bevy::render::diagnostic::RecordDiagnostics;
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::texture::{GpuImage, TextureCache};
use bevy::render::view::{ExtractedView, ViewDepthTexture, ViewTarget};
use bevy::render::{Render, RenderApp, RenderSystems};
use fh1_shaders::container::{ShaderBlob, Stage};
use fh1_shaders::wgsl::{self, Resolver};

/// Where a pass input comes from.
#[derive(Clone, Debug)]
pub enum PostSource {
    /// The rendered scene (the view's main texture before post).
    Scene,
    /// Output of an earlier pass in the chain (this frame).
    Pass(usize),
    /// Last frame's content of a persistent target.
    Previous(u32),
    /// A texture asset (2D or 3D).
    Image(Handle<Image>),
    /// The view's depth (Bevy reversed-Z, sample 0), resolved to an R16Float target at depth size.
    Depth,
}

impl FxPostChain {
    /// Some pass reads depth (the depth texture must be sampleable, the resolve pipeline built).
    fn declares_depth(&self) -> bool {
        self.passes.iter().any(|p| p.inputs.iter().any(|(_, s)| matches!(s, PostSource::Depth)))
    }

    /// A pass that runs this frame reads depth (the resolve runs only then).
    fn uses_depth(&self) -> bool {
        self.passes.iter().any(|p| !p.skip && p.inputs.iter().any(|(_, s)| matches!(s, PostSource::Depth)))
    }
}

/// Where a pass writes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PostTarget {
    /// The view output (last pass).
    View,
    /// View size × (sx, sy), optionally rounded up to a multiple.
    Scaled { sx: f32, sy: f32, round: u32 },
    Fixed { w: u32, h: u32 },
    /// A persistent target (double-buffered across frames) of a fixed size.
    Persistent { slot: u32, w: u32, h: u32 },
    /// A persistent target sized like the view × (sx, sy).
    PersistentScaled { slot: u32, sx: f32, sy: f32 },
}

#[derive(Clone)]
pub struct PostPass {
    pub label: String,
    pub vs: Arc<ShaderBlob>,
    pub ps: Arc<ShaderBlob>,
    /// Sampler register → source (pixel shader tf0-15, vertex shader tf16+).
    pub inputs: Vec<(u32, PostSource)>,
    pub target: PostTarget,
    pub format: TextureFormat,
    /// Register files (256 each) and raw bool addresses (VS 0-127, PS 128-255).
    pub vs_consts: Vec<[f32; 4]>,
    pub ps_consts: Vec<[f32; 4]>,
    pub bools: Vec<u32>,
    shader: Handle<Shader>,
    /// Feed texcoord0 as (1 - u, 1 - v) (FinalCombine's quad; see docs/SHADERS.md).
    pub flip_uv: bool,
    /// (tf, dimension) of every texture the pair samples.
    textures: Vec<(u32, u8)>,
    /// Skip this pass this frame (set per frame by the main world); readers of its output get `bypass`.
    pub skip: bool,
    /// What a skipped pass's readers sample instead (default: the scene).
    pub bypass: PostSource,
}

impl PostPass {
    /// Translate the pair and register its shader. `present`: the pass writes the view output;
    /// FH1's final pass writes gamma-2 values meant for the TV as is, and Bevy sRGB-encodes the
    /// view output, so the pass emits sRGB-decoded values and the screen gets the game's bytes.
    pub fn new(label: &str, vs: Arc<ShaderBlob>, ps: Arc<ShaderBlob>, inputs: Vec<(u32, PostSource)>, target: PostTarget, shaders: &mut Assets<Shader>) -> Self {
        Self::with_fetch_swizzle(label, vs, ps, inputs, target, &[], shaders)
    }

    /// Like [`PostPass::new`], with the fetches from the listed `tf`s returning `.xxxx`: the game's
    /// single-channel targets, whose fetch constant replicates red (light rays read the mask from .w).
    pub fn with_fetch_swizzle(
        label: &str,
        vs: Arc<ShaderBlob>,
        ps: Arc<ShaderBlob>,
        inputs: Vec<(u32, PostSource)>,
        target: PostTarget,
        replicate_x: &[u32],
        shaders: &mut Assets<Shader>,
    ) -> Self {
        let present = target == PostTarget::View;
        let (code, textures) = build_wgsl(&vs, &ps, present, replicate_x);
        // FH1_POST_WGSL=<dir>: write each pass's translated shader there (debugging).
        if let Some(dir) = std::env::var_os("FH1_POST_WGSL") {
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{label}.wgsl")), &code);
        }
        let shader = shaders.add(Shader::from_wgsl(code, format!("fh1/post/{label}.wgsl")));
        let mut vs_consts = vec![[0.0; 4]; 256];
        let mut ps_consts = vec![[0.0; 4]; 256];
        for (blob, file) in [(&*vs, &mut vs_consts), (&*ps, &mut ps_consts)] {
            for c in blob.constants.iter().filter(|c| c.set == fh1_shaders::container::RegisterSet::Float4) {
                if let Some(d) = &c.default {
                    for k in 0..c.count as usize {
                        if d.len() >= k * 4 + 4 {
                            if let Some(r) = file.get_mut(c.register as usize + k) {
                                *r = [0, 1, 2, 3].map(|j| f32::from_bits(d[k * 4 + j]));
                            }
                        }
                    }
                }
            }
        }
        Self { label: label.into(), vs, ps, inputs, target, format: TextureFormat::Rgba16Float, vs_consts, ps_consts, bools: vec![0; 256], shader, flip_uv: false, textures, skip: false, bypass: PostSource::Scene }
    }

    pub fn with_format(mut self, f: TextureFormat) -> Self {
        self.format = f;
        self
    }

    /// Set a float parameter by constant-table name in whichever stage declares it.
    pub fn set(&mut self, name: &str, values: &[[f32; 4]]) -> bool {
        let mut hit = false;
        for (blob, file) in [(&*self.vs, &mut self.vs_consts), (&*self.ps, &mut self.ps_consts)] {
            for c in blob.constants.iter().filter(|c| c.name == name) {
                for (k, v) in values.iter().take(c.count.max(1) as usize).enumerate() {
                    if let Some(r) = file.get_mut(c.register as usize + k) {
                        *r = *v;
                        hit = true;
                    }
                }
            }
        }
        hit
    }

    /// Set a raw register (for constants the game sets by number).
    pub fn set_reg(&mut self, stage: Stage, reg: usize, v: [f32; 4]) {
        let file = if stage == Stage::Vertex { &mut self.vs_consts } else { &mut self.ps_consts };
        if let Some(r) = file.get_mut(reg) {
            *r = v;
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 * 32 + 1024);
        for v in self.vs_consts.iter().chain(&self.ps_consts) {
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        for b in &self.bools {
            out.extend_from_slice(&b.to_le_bytes());
        }
        for f in [self.flip_uv as u32, 0, 0, 0] {
            out.extend_from_slice(&f.to_le_bytes());
        }
        out
    }
}

/// Load an installed default.xex shader container (`shaders/xex/<addr:08x>.bin`).
pub fn load_xex_shader(dir: &std::path::Path, addr: u32) -> Option<Arc<ShaderBlob>> {
    let d = std::fs::read(dir.join(format!("{addr:08x}.bin"))).ok()?;
    ShaderBlob::parse(&d, addr as usize).ok().map(Arc::new)
}

/// The post chain applied to every 3D camera that has [`FxPostCamera`].
#[derive(Resource, Clone, Default, ExtractResource)]
pub struct FxPostChain {
    pub passes: Vec<PostPass>,
    /// A/B baseline (`FH1_POST_AB`): allocate the sampler and uniform buffers every frame, as before 2026-10-04 L4b.
    pub alloc_per_frame: bool,
}

/// Marker: run the FH1 post chain on this camera (use with `Hdr` and `Tonemapping::None`).
#[derive(Component, Clone, Copy, Default, bevy::render::extract_component::ExtractComponent)]
pub struct FxPostCamera;

struct PostResolver<'a> {
    textures: &'a mut Vec<(u32, u8)>,
    replicate_x: &'a [u32],
}

impl Resolver for PostResolver<'_> {
    fn float_const(&mut self, st: Stage, reg: u32) -> String {
        format!("post.{}[{reg}]", if st == Stage::Vertex { "vs" } else { "ps" })
    }
    fn float_const_rel(&mut self, st: Stage, base: u32, index: &str) -> String {
        format!("post.{}[clamp({base} + {index}, 0, 255)]", if st == Stage::Vertex { "vs" } else { "ps" })
    }
    fn bool_const(&mut self, _: Stage, addr: u32) -> String {
        format!("(post.b[{}][{}] != 0u)", addr / 4, addr % 4)
    }
    fn int_const(&mut self, _: Stage, _id: u32) -> String {
        "vec4<i32>(0, 0, 1, 0)".into()
    }
    fn texture(&mut self, _: Stage, tf: u32, dim: u8) -> Option<(String, String)> {
        if dim == 3 {
            return None;
        }
        if !self.textures.iter().any(|(t, _)| *t == tf) {
            self.textures.push((tf, dim));
        }
        Some((format!("post_t{tf}"), format!("post_s{tf}")))
    }
    fn texture_result(&mut self, _: Stage, tf: u32, _: u8, value: String) -> String {
        if self.replicate_x.contains(&tf) {
            format!("({value}).xxxx")
        } else {
            value
        }
    }
    fn vertex_input(&mut self, u: u8, idx: u8) -> String {
        match (u, idx) {
            (fh1_shaders::container::usage::POSITION, 0) => "post_vin_pos".into(),
            (fh1_shaders::container::usage::TEXCOORD, 0) => "post_vin_uv".into(),
            _ => "vec4<f32>(0.0, 0.0, 0.0, 1.0)".into(),
        }
    }
}

fn build_wgsl(vs: &ShaderBlob, ps: &ShaderBlob, present: bool, replicate_x: &[u32]) -> (String, Vec<(u32, u8)>) {
    let mut textures = Vec::new();
    let vt = wgsl::translate(vs, "post_vs_main", &mut PostResolver { textures: &mut textures, replicate_x });
    let pt = wgsl::translate(ps, "post_ps_main", &mut PostResolver { textures: &mut textures, replicate_x });
    textures.sort();
    let mut m = String::from("diagnostic(off, derivative_uniformity);\n");
    m += "struct PostConsts { vs: array<vec4<f32>, 256>, ps: array<vec4<f32>, 256>, b: array<vec4<u32>, 64>, flags: vec4<u32> }\n";
    m += "@group(0) @binding(0) var<uniform> post: PostConsts;\n";
    for (n, (tf, dim)) in textures.iter().enumerate() {
        let ty = if *dim == 2 { "texture_3d<f32>" } else { "texture_2d<f32>" };
        let _ = writeln!(m, "@group(0) @binding({}) var post_t{tf}: {ty};", 1 + 2 * n);
        let _ = writeln!(m, "@group(0) @binding({}) var post_s{tf}: sampler;", 2 + 2 * n);
    }
    m += wgsl::PRELUDE;
    m += "var<private> post_vin_pos: vec4<f32>;\nvar<private> post_vin_uv: vec4<f32>;\n";
    m += "struct PostVaryings {\n    @builtin(position) position: vec4<f32>,\n";
    for (n, _) in ps.interpolators.iter().enumerate() {
        let _ = writeln!(m, "    @location({n}) v{n}: vec4<f32>,");
    }
    m += "}\n";
    m += &vt.code;
    m += &pt.code;
    m += "@vertex\nfn vertex(@builtin(vertex_index) vi: u32) -> PostVaryings {\n";
    m += "    let uv = vec2<f32>(f32(vi >> 1u), f32(vi & 1u)) * 2.0;\n";
    m += "    post_vin_pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);\n";
    // The game's quad: a rect list of float4 (x, y, u, v), uv = source rect / size with (0, 0) at
    // the top-left; FinalCombine is fed (1 - u, 1 - v) (docs/SHADERS.md).
    m += "    post_vin_uv = vec4<f32>(select(uv, vec2<f32>(1.0) - uv, post.flags.x != 0u), 0.0, 1.0);\n";
    // Post passes run with the viewport transform off: only oPos.xy matters (FinalCombine writes
    // oPos = r0.zwww).
    m += "    let o = post_vs_main(vi);\n    var out: PostVaryings;\n    out.position = vec4<f32>(o.pos.xy, 0.0, 1.0);\n";
    for (n, i) in ps.interpolators.iter().enumerate() {
        match vs.interpolators.iter().position(|v| v.usage == i.usage && v.usage_index == i.usage_index) {
            Some(k) => {
                let _ = writeln!(m, "    out.v{n} = o.o{k};");
            }
            None => {
                let _ = writeln!(m, "    out.v{n} = vec4<f32>(0.0);");
            }
        }
    }
    m += "    return out;\n}\n";
    m += "@fragment\nfn fragment(input: PostVaryings, @builtin(front_facing) ff: bool) -> @location(0) vec4<f32> {\n";
    m += "    var i: FxPsIn;\n    i.vpos = input.position;\n    i.front_facing = ff;\n";
    for (n, it) in ps.interpolators.iter().enumerate() {
        let _ = writeln!(m, "    i.r{} = input.v{n};", it.reg);
    }
    m += "    let c = post_ps_main(i).c0;\n";
    if present {
        m += "    var v = clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0));\n";
        // The display gamma ramp FH1 loads (PWL, same for R/G/B; read from a Pinyon capture, docs/GPU_CAPTURE.md):
        // a fit within ~0.003 of the 128 segments. Mids +3-5 %, black floor 0.0137. FH1_POST_GAMMA_RAMP=0 = off.
        if std::env::var("FH1_POST_GAMMA_RAMP").as_deref() != Ok("0") {
            m += "    v = select(min(1.107 * pow(v, vec3<f32>(0.896)) - 0.062, vec3<f32>(0.999)), 0.0137 + 6.3 * v * v, v < vec3<f32>(0.125));\n";
        }
        m += "    let lin = select(pow((v + 0.055) / 1.055, vec3<f32>(2.4)), v / 12.92, v <= vec3<f32>(0.04045));\n";
        m += "    return vec4<f32>(lin, 1.0);\n}\n";
    } else {
        m += "    return c;\n}\n";
    }
    (m, textures)
}

pub struct FxPostPlugin;

impl Plugin for FxPostPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FxPostChain>()
            .add_plugins(ExtractResourcePlugin::<FxPostChain>::default())
            .add_plugins(bevy::render::extract_component::ExtractComponentPlugin::<FxPostCamera>::default());
        // StandardMaterial objects must write the game's sqrt-encoded colour too (car_material.rs).
        crate::car_material::add_raw_standard(app);
        app.add_systems(Update, depth_binding).add_systems(Last, crate::postfx::post_ab_report);
        let depth_shader = app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(DEPTH_RESOLVE_WGSL, "fh1/post/depth_resolve.wgsl"));
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app
            .init_resource::<PostPipelines>()
            .init_resource::<PostPersistent>()
            .insert_resource(DepthResolve { shader: depth_shader, ids: HashMap::new() })
            .add_systems(Render, (prepare_post_pipelines, prepare_depth_resolve).in_set(RenderSystems::Prepare))
            .add_systems(Core3d, post_chain_system.in_set(Core3dSystems::PostProcess).before(tonemapping));
    }
}

/// A chain that reads depth needs the main depth texture to be sampleable.
fn depth_binding(chain: Res<FxPostChain>, mut cams: Query<&mut Camera3d, With<FxPostCamera>>) {
    if !chain.declares_depth() {
        return;
    }
    for mut c in &mut cams {
        let u = TextureUsages::from(c.depth_texture_usages);
        if !u.contains(TextureUsages::TEXTURE_BINDING) {
            c.depth_texture_usages = (u | TextureUsages::TEXTURE_BINDING).into();
        }
    }
}

/// Depth resolve: sample 0 of the (possibly multisampled) view depth -> R16Float.
const DEPTH_RESOLVE_WGSL: &str = r#"
#ifdef MULTISAMPLED
@group(0) @binding(0) var depth: texture_depth_multisampled_2d;
#else
@group(0) @binding(0) var depth: texture_depth_2d;
#endif
@vertex
fn vertex(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32(vi >> 1u), f32(vi & 1u)) * 2.0;
    return vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}
@fragment
fn fragment(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    let n = vec2<i32>(textureDimensions(depth)) - 1;
    let z = textureLoad(depth, clamp(vec2<i32>(p.xy), vec2<i32>(0), n), 0);
    return vec4<f32>(z, 0.0, 0.0, 1.0);
}
"#;

const DEPTH_FORMAT: TextureFormat = TextureFormat::R16Float;

#[derive(Resource)]
struct DepthResolve {
    shader: Handle<Shader>,
    /// By depth sample count: (layout, pipeline).
    ids: HashMap<u32, (BindGroupLayoutDescriptor, CachedRenderPipelineId)>,
}

fn prepare_depth_resolve(chain: Res<FxPostChain>, mut resolve: ResMut<DepthResolve>, cache: Res<PipelineCache>, views: Query<&ViewDepthTexture, With<FxPostCamera>>) {
    if !chain.declares_depth() {
        return;
    }
    for depth in &views {
        let samples = depth.texture.sample_count();
        if resolve.ids.contains_key(&samples) {
            continue;
        }
        let entry = if samples > 1 { texture_depth_2d_multisampled() } else { texture_depth_2d() };
        let layout = BindGroupLayoutDescriptor::new("fh1_post_depth_layout", &[entry.build(0, ShaderStages::FRAGMENT)]);
        let defs: Vec<_> = if samples > 1 { vec!["MULTISAMPLED".into()] } else { vec![] };
        let id = cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("fh1_post_depth_resolve".into()),
            layout: vec![layout.clone()],
            vertex: VertexState { shader: resolve.shader.clone(), shader_defs: defs.clone(), entry_point: Some("vertex".into()), buffers: vec![] },
            fragment: Some(FragmentState {
                shader: resolve.shader.clone(),
                shader_defs: defs,
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState { format: DEPTH_FORMAT, blend: None, write_mask: ColorWrites::ALL })],
            }),
            ..default()
        });
        resolve.ids.insert(samples, (layout, id));
    }
}

#[derive(Resource, Default)]
struct PostPipelines {
    /// Per pass: (shader, output format) it was built for, layout, pipeline.
    entries: Vec<(Handle<Shader>, TextureFormat, BindGroupLayoutDescriptor, CachedRenderPipelineId)>,
}

/// Persistent targets: slot → (size, format, [texture; 2]); `frame` picks the current half.
#[derive(Resource, Default)]
struct PostPersistent {
    targets: HashMap<u32, ((u32, u32), TextureFormat, [(Texture, TextureView); 2])>,
    frame: usize,
}

fn layout_for(pass: &PostPass) -> BindGroupLayoutDescriptor {
    use bevy::render::render_resource::BindGroupLayoutEntryBuilder;
    let mut entries: Vec<BindGroupLayoutEntryBuilder> = vec![uniform_buffer_sized(false, None)];
    for (_, dim) in &pass.textures {
        if *dim == 2 {
            entries.push(texture_3d(TextureSampleType::Float { filterable: true }));
        } else {
            entries.push(texture_2d(TextureSampleType::Float { filterable: true }));
        }
        entries.push(sampler(SamplerBindingType::Filtering));
    }
    let built: Vec<_> = entries.iter().enumerate().map(|(i, e)| e.build(i as u32, ShaderStages::VERTEX_FRAGMENT)).collect();
    BindGroupLayoutDescriptor::new("fh1_post_layout", &built)
}

fn prepare_post_pipelines(
    chain: Res<FxPostChain>,
    mut pipelines: ResMut<PostPipelines>,
    pipeline_cache: Res<PipelineCache>,
    views: Query<&ExtractedView, With<FxPostCamera>>,
) {
    let Some(view) = views.iter().next() else { return };
    let final_format = view.target_format;
    pipelines.entries.truncate(chain.passes.len());
    for (i, pass) in chain.passes.iter().enumerate() {
        let format = if pass.target == PostTarget::View { final_format } else { pass.format };
        if let Some(e) = pipelines.entries.get(i) {
            if e.0 == pass.shader && e.1 == format {
                continue;
            }
        }
        let layout = layout_for(pass);
        let id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some(format!("fh1_post_{}", pass.label).into()),
            layout: vec![layout.clone()],
            vertex: VertexState { shader: pass.shader.clone(), shader_defs: vec![], entry_point: Some("vertex".into()), buffers: vec![] },
            fragment: Some(FragmentState {
                shader: pass.shader.clone(),
                shader_defs: vec![],
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState { format, blend: None, write_mask: ColorWrites::ALL })],
            }),
            ..default()
        });
        let entry = (pass.shader.clone(), format, layout, id);
        if i < pipelines.entries.len() {
            pipelines.entries[i] = entry;
        } else {
            pipelines.entries.push(entry);
        }
    }
}

fn target_size(t: PostTarget, view: UVec2) -> (u32, u32) {
    let scaled = |sx: f32, sy: f32, round: u32| {
        let r = round.max(1);
        let w = ((view.x as f32 * sx).ceil() as u32).max(1).div_ceil(r) * r;
        let h = ((view.y as f32 * sy).ceil() as u32).max(1).div_ceil(r) * r;
        (w, h)
    };
    match t {
        PostTarget::View => (view.x, view.y),
        PostTarget::Scaled { sx, sy, round } => scaled(sx, sy, round),
        PostTarget::Fixed { w, h } | PostTarget::Persistent { w, h, .. } => (w, h),
        PostTarget::PersistentScaled { sx, sy, .. } => scaled(sx, sy, 1),
    }
}

/// Render-world caches: the shared sampler and one uniform buffer per pass (rewritten every frame).
#[derive(Default)]
struct PostCache {
    sampler: Option<Sampler>,
    uniforms: Vec<Buffer>,
}

#[allow(clippy::too_many_arguments)]
fn post_chain_system(
    view: ViewQuery<(&ViewTarget, &ExtractedView, Option<&ViewDepthTexture>), With<FxPostCamera>>,
    chain: Res<FxPostChain>,
    resolve: Res<DepthResolve>,
    pipelines: Res<PostPipelines>,
    pipeline_cache: Res<PipelineCache>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut texture_cache: ResMut<TextureCache>,
    mut persistent: ResMut<PostPersistent>,
    images: Res<RenderAssets<GpuImage>>,
    mut cache: Local<PostCache>,
    mut ctx: RenderContext,
) {
    let (view_target, extracted, view_depth) = view.into_inner();
    if chain.passes.is_empty() || pipelines.entries.len() < chain.passes.len() {
        return;
    }
    if chain.passes.iter().enumerate().any(|(i, _)| pipeline_cache.get_render_pipeline(pipelines.entries[i].3).is_none()) {
        return;
    }
    // Every image input must be on the GPU before the main textures are flipped.
    let images_ready = chain.passes.iter().flat_map(|p| p.inputs.iter()).all(|(_, s)| match s {
        PostSource::Image(h) => images.get(h).is_some(),
        _ => true,
    });
    if !images_ready {
        return;
    }
    let size = extracted.viewport.zw();
    // Depth input: wait until the depth texture is sampleable and the resolve pipeline is built.
    let depth_pipeline = view_depth.and_then(|d| resolve.ids.get(&d.texture.sample_count()));
    let depth_ready = view_depth.is_some_and(|d| d.texture.usage().contains(TextureUsages::TEXTURE_BINDING))
        && depth_pipeline.is_some_and(|(_, id)| pipeline_cache.get_render_pipeline(*id).is_some());
    if chain.declares_depth() && !depth_ready {
        return;
    }
    // GPU/CPU time per pass (render/fh1_post/<label>) when Bevy's RenderDiagnosticsPlugin is on (FH1_P2_STATS=1).
    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();
    let chain_span = diagnostics.time_span(ctx.command_encoder(), "fh1_post");
    let post = view_target.post_process_write();
    let new_sampler = || {
        render_device.create_sampler(&SamplerDescriptor {
            mag_filter: bevy::render::render_resource::FilterMode::Linear,
            min_filter: bevy::render::render_resource::FilterMode::Linear,
            ..default()
        })
    };
    let sampler = if chain.alloc_per_frame { new_sampler() } else { cache.sampler.get_or_insert_with(new_sampler).clone() };
    persistent.frame ^= 1;
    let (cur, prev) = (persistent.frame, persistent.frame ^ 1);
    // Create persistent targets (zero-initialised) when their size or format changes.
    for pass in &chain.passes {
        let slot = match pass.target {
            PostTarget::Persistent { slot, .. } | PostTarget::PersistentScaled { slot, .. } => slot,
            _ => continue,
        };
        let wh = target_size(pass.target, size);
        let ok = persistent.targets.get(&slot).is_some_and(|(s, f, _)| *s == wh && *f == pass.format);
        if !ok {
            let make = || {
                let t = render_device.create_texture(&TextureDescriptor {
                    label: Some("fh1_post_persistent"),
                    size: Extent3d { width: wh.0, height: wh.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format: pass.format,
                    usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                let v = t.create_view(&Default::default());
                (t, v)
            };
            persistent.targets.insert(slot, (wh, pass.format, [make(), make()]));
        }
    }
    let mut depth_view: Option<TextureView> = None;
    if let (true, Some(d), Some((layout, id))) = (chain.uses_depth(), view_depth, depth_pipeline) {
        let span = diagnostics.time_span(ctx.command_encoder(), "depth_resolve");
        let src = d.texture.create_view(&TextureViewDescriptor { aspect: TextureAspect::DepthOnly, ..default() });
        let (w, h) = (d.texture.width(), d.texture.height());
        let out = texture_cache
            .get(
                &render_device,
                TextureDescriptor {
                    label: Some("fh1_post_depth"),
                    size: Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format: DEPTH_FORMAT,
                    usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
            .default_view;
        let bind_group = render_device.create_bind_group(
            "fh1_post_depth_bind_group",
            &pipeline_cache.get_bind_group_layout(layout),
            &[BindGroupEntry { binding: 0, resource: BindingResource::TextureView(&src) }],
        );
        let mut rp = ctx.command_encoder().begin_render_pass(&RenderPassDescriptor {
            label: Some("fh1_post_depth_resolve"),
            color_attachments: &[Some(RenderPassColorAttachment { view: &out, depth_slice: None, resolve_target: None, ops: Operations::default() })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(pipeline_cache.get_render_pipeline(*id).unwrap());
        rp.set_bind_group(0, &bind_group, &[]);
        rp.draw(0..3, 0..1);
        drop(rp);
        span.end(ctx.command_encoder());
        depth_view = Some(out);
    }
    // What each pass's readers sample: its target, or its bypass source when it is skipped.
    let mut outputs: Vec<TextureView> = Vec::new();
    let source_view = |src: &PostSource, outputs: &[TextureView]| -> TextureView {
        match src {
            PostSource::Scene => post.source.clone(),
            PostSource::Pass(k) => outputs.get(*k).cloned().unwrap_or_else(|| post.source.clone()),
            PostSource::Previous(slot) => persistent.targets.get(slot).map(|t| t.2[prev].1.clone()).unwrap_or_else(|| post.source.clone()),
            PostSource::Image(h) => images.get(h).map(|g| g.texture_view.clone()).unwrap_or_else(|| post.source.clone()),
            PostSource::Depth => depth_view.clone().unwrap_or_else(|| post.source.clone()),
        }
    };
    for (i, pass) in chain.passes.iter().enumerate() {
        if pass.skip && pass.target != PostTarget::View {
            let v = source_view(&pass.bypass, &outputs);
            outputs.push(v);
            continue;
        }
        let span = diagnostics.time_span(ctx.command_encoder(), pass.label.clone());
        let target_view = match pass.target {
            PostTarget::View => post.destination.clone(),
            PostTarget::Persistent { slot, .. } | PostTarget::PersistentScaled { slot, .. } => persistent.targets[&slot].2[cur].1.clone(),
            t => {
                let (w, h) = target_size(t, size);
                texture_cache
                    .get(
                        &render_device,
                        TextureDescriptor {
                            label: Some("fh1_post_target"),
                            size: Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: TextureDimension::D2,
                            format: pass.format,
                            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                            view_formats: &[],
                        },
                    )
                    .default_view
            }
        };
        let bytes = pass.bytes();
        let uniform = if chain.alloc_per_frame {
            render_device.create_buffer_with_data(&BufferInitDescriptor { label: Some("fh1_post_consts"), contents: &bytes, usage: BufferUsages::UNIFORM })
        } else {
            if cache.uniforms.len() <= i {
                cache.uniforms.resize_with(i + 1, || {
                    render_device.create_buffer(&BufferDescriptor {
                        label: Some("fh1_post_consts"),
                        size: bytes.len() as u64,
                        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    })
                });
            }
            render_queue.write_buffer(&cache.uniforms[i], 0, &bytes);
            cache.uniforms[i].clone()
        };
        let mut views: Vec<TextureView> = Vec::new();
        for (tf, _) in &pass.textures {
            let src = pass.inputs.iter().find(|(r, _)| r == tf).map(|(_, s)| s.clone()).unwrap_or(PostSource::Scene);
            views.push(source_view(&src, &outputs));
        }
        let mut entries = vec![BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }];
        for (n, v) in views.iter().enumerate() {
            entries.push(BindGroupEntry { binding: 1 + 2 * n as u32, resource: BindingResource::TextureView(v) });
            entries.push(BindGroupEntry { binding: 2 + 2 * n as u32, resource: BindingResource::Sampler(&sampler) });
        }
        let (_, _, layout, pid) = &pipelines.entries[i];
        let bind_group = render_device.create_bind_group("fh1_post_bind_group", &pipeline_cache.get_bind_group_layout(layout), &entries);
        let pipeline = pipeline_cache.get_render_pipeline(*pid).unwrap();
        {
            let mut rp = ctx.command_encoder().begin_render_pass(&RenderPassDescriptor {
                label: Some("fh1_post_pass"),
                color_attachments: &[Some(RenderPassColorAttachment { view: &target_view, depth_slice: None, resolve_target: None, ops: Operations::default() })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(pipeline);
            rp.set_bind_group(0, &bind_group, &[]);
            rp.draw(0..3, 0..1);
        }
        span.end(ctx.command_encoder());
        outputs.push(target_view);
    }
    chain_span.end(ctx.command_encoder());
}
