//! Remaster camera motion blur (P15-C, 2026-10-08; docs/notes/yt_threat_interactive_optimize_rendering_SnNm7rSSvlg.txt
//! 1:42-1:58, Need for Speed 2015 as the reference). Options > Graphics > Motion blur Off / Low / Medium (default) / High
//! via [`MotionBlurSettings`]; `FH1_RM_MOTION_BLUR=0..3` forces a level (0 = off, nothing registered = the old frame).
//!
//! - Camera motion only: each pixel's past screen position comes from the main depth buffer and a reprojection matrix
//!   (this frame's clip -> last frame's clip, built in f64 from the two views). Our world writes no motion vectors and
//!   there is no prepass: the main view's depth texture gets TEXTURE_BINDING ([`depth_usage`]); with MSAA it is
//!   multisampled and the shader loads sample 0 (pipeline variant on the depth texture's sample count).
//! - Only beyond a few metres: blur = smoothstep(FH1_RM_MB_START 4 m, FH1_RM_MB_FULL 16 m) of the view depth, 1 for the
//!   sky. The player car is masked: its root-local box (union of its meshes' bounds, [`car_box`]) projected to a feathered
//!   screen rect, applied only to pixels nearer than the box's far corner (+ 1 m feather). When the camera is inside the
//!   box (hood / cockpit) the rect is the whole screen and the depth test alone masks the car. Masked and near pixels get
//!   weight 0 as taps too, so they never smear into the background's trail.
//! - Trails run in ONE direction (pixel -> its past position), uniform taps, no noise / dither / max-velocity tiles. A
//!   second short pass along the same per-pixel trail spreads FH1_RM_MB_SMOOTH (3) taps over one pass-1 tap spacing,
//!   which turns the stepping into a ramp (taps x smooth effective samples). Trail length = shutter x 1/60 s of motion,
//!   frame-rate independent (screen velocity x exposure), clamped to a fraction of the screen width.
//! - Order: in Core3dSystems::PostProcess before Bevy's bloom (and Bevy's own motion blur), so bloom sees the blurred HDR
//!   frame; tonemapping and our grade come after. Camera cuts (big jumps in one frame) and a missing previous frame skip it.
//! - Off (level 0) returns before any work: no pass, no bind group. Not under RTX (DLSS owns the frame there).

use std::collections::HashMap;
use std::sync::OnceLock;

use bevy::camera::primitives::Aabb;
use bevy::core_pipeline::schedule::Core3d;
use bevy::core_pipeline::Core3dSystems;
use bevy::math::{Affine3A, DMat4, DVec4};
use bevy::prelude::*;
use bevy::render::diagnostic::RecordDiagnostics;
use bevy::render::render_resource::binding_types::{sampler, texture_2d, texture_2d_multisampled, uniform_buffer_sized};
use bevy::render::render_resource::{
    BindGroupEntry, BindGroupLayoutDescriptor, BindingResource, Buffer, BufferDescriptor, BufferUsages, CachedRenderPipelineId, ColorTargetState,
    ColorWrites, FilterMode, FragmentState, LoadOp, Operations, PipelineCache, RenderPassColorAttachment, RenderPassDescriptor,
    RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor, ShaderStages, StoreOp, TextureFormat, TextureSampleType,
    TextureUsages, VertexState,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::view::{ExtractedView, ViewDepthTexture, ViewTarget};
use bevy::render::{Extract, ExtractSchedule, RenderApp};
use fh1_render::reflect::EnvCubeAnchor;

use crate::light::{env_f32, RemasterView};

const MB_WGSL: &str = include_str!("motion_blur.wgsl");

/// Options > Graphics > Motion blur (fh1-engine ui/graphics.rs writes `level` from settings.json).
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotionBlurSettings {
    /// 0 = off, 1 = low, 2 = medium (default), 3 = high.
    pub level: u8,
}

impl Default for MotionBlurSettings {
    fn default() -> Self {
        Self { level: 2 }
    }
}

impl MotionBlurSettings {
    /// The level that runs: `FH1_RM_MOTION_BLUR` wins over the setting; 0 under RTX.
    pub fn effective_level(&self) -> u8 {
        if rtx_on() {
            return 0;
        }
        forced_level().unwrap_or(self.level).min(3)
    }
}

/// `FH1_RM_MOTION_BLUR=0..3`, when set.
pub fn forced_level() -> Option<u8> {
    static L: OnceLock<Option<u8>> = OnceLock::new();
    *L.get_or_init(|| std::env::var("FH1_RM_MOTION_BLUR").ok().and_then(|v| v.trim().parse::<u8>().ok()).map(|v| v.min(3)))
}

fn rtx_on() -> bool {
    #[cfg(feature = "rtx")]
    {
        crate::rtx::on()
    }
    #[cfg(not(feature = "rtx"))]
    {
        false
    }
}

/// Per-level look: (shutter as a fraction of a 60 fps frame, max trail as a fraction of the screen width, pass-1 taps).
fn preset(level: u8) -> (f32, f32, u32) {
    match level {
        1 => (0.3, 0.02, 4),
        3 => (0.9, 0.04, 8),
        _ => (0.55, 0.03, 6),
    }
}

/// Env tuning, read once (unset = the level's preset).
struct Tuning {
    shutter: Option<f32>,
    max: Option<f32>,
    taps: Option<f32>,
    smooth: f32,
    start: f32,
    full: f32,
}

fn tuning() -> &'static Tuning {
    static T: OnceLock<Tuning> = OnceLock::new();
    T.get_or_init(|| {
        let opt = |n: &str| std::env::var(n).ok().and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite());
        let start = env_f32("FH1_RM_MB_START", 4.0).max(0.0);
        Tuning {
            shutter: opt("FH1_RM_MB_SHUTTER"),
            max: opt("FH1_RM_MB_MAX"),
            taps: opt("FH1_RM_MB_TAPS"),
            smooth: env_f32("FH1_RM_MB_SMOOTH", 3.0).clamp(1.0, 8.0).round(),
            start,
            full: env_f32("FH1_RM_MB_FULL", 16.0).max(start + 0.5),
        }
    })
}

/// The player car's box in its root's space (union of its meshes' bounds), refreshed every 30 frames.
#[derive(Resource, Default, Clone, Copy)]
struct CarBox {
    root: Option<Entity>,
    lo: Vec3,
    hi: Vec3,
    found: bool,
    age: u32,
}

/// Render world: this frame's inputs from the main world.
#[derive(Resource, Clone, Copy)]
struct MbFrame {
    level: u8,
    frame: u64,
    dt: f32,
    /// Car root (world from car), box min / max in car space.
    car: Option<(Affine3A, Vec3, Vec3)>,
}

#[derive(Resource)]
struct MbPipeline {
    shader: Handle<Shader>,
    layout: BindGroupLayoutDescriptor,
    layout_ms: BindGroupLayoutDescriptor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MbKey {
    format: TextureFormat,
    multisampled: bool,
    smooth: bool,
}

struct PrevView {
    frame: u64,
    world_from_view: DMat4,
    clip_from_view: DMat4,
}

#[derive(Default)]
struct MbState {
    prev: HashMap<Entity, PrevView>,
    ids: Vec<(MbKey, CachedRenderPipelineId)>,
    buffers: HashMap<Entity, Buffer>,
    sampler: Option<Sampler>,
}

pub(crate) fn plugin(app: &mut App) {
    app.init_resource::<MotionBlurSettings>().init_resource::<CarBox>();
    // Forced off / RTX: nothing registered (the old frame). The settings resource stays for the Options page.
    if forced_level() == Some(0) || rtx_on() {
        return;
    }
    app.add_systems(Update, depth_usage)
        .add_systems(PostUpdate, car_box.after(bevy::transform::TransformSystems::Propagate));
    let shader = app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(MB_WGSL, "fh1_remaster/motion_blur.wgsl"));
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
    let tail = |depth| {
        [
            depth,
            texture_2d(TextureSampleType::Float { filterable: true }).build(1, ShaderStages::FRAGMENT),
            sampler(SamplerBindingType::Filtering).build(2, ShaderStages::FRAGMENT),
            uniform_buffer_sized(false, None).build(3, ShaderStages::FRAGMENT),
        ]
    };
    // The depth buffer (Depth32Float) read as an unfilterable float texture, like Bevy's own motion blur.
    let single = tail(texture_2d(TextureSampleType::Float { filterable: false }).build(0, ShaderStages::FRAGMENT));
    let multi = tail(texture_2d_multisampled(TextureSampleType::Float { filterable: false }).build(0, ShaderStages::FRAGMENT));
    render_app
        .insert_resource(MbPipeline {
            shader,
            layout: BindGroupLayoutDescriptor::new("fh1_remaster_motion_blur_layout", &single),
            layout_ms: BindGroupLayoutDescriptor::new("fh1_remaster_motion_blur_layout_ms", &multi),
        })
        .add_systems(ExtractSchedule, extract_frame)
        .add_systems(
            Core3d,
            motion_blur_system
                .in_set(Core3dSystems::PostProcess)
                .before(bevy::post_process::bloom::bloom)
                .before(bevy::post_process::motion_blur::motion_blur),
        );
}

/// The main view's depth must be readable (Bevy's default usage is RENDER_ATTACHMENT only). Set once blur is on; left
/// on when it is switched off again (harmless, avoids reallocating the depth texture).
fn depth_usage(settings: Res<MotionBlurSettings>, mut cams: Query<&mut Camera3d, With<RemasterView>>) {
    if settings.effective_level() == 0 {
        return;
    }
    let want = (TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING).bits();
    for mut c in &mut cams {
        if c.depth_texture_usages.0 & want != want {
            c.depth_texture_usages.0 |= want;
        }
    }
}

/// The player car's (EnvCubeAnchor root) box in root space: union of every descendant mesh's bounds (body, wheels,
/// interior), meshes over 4 m half-extent skipped (effects). Recomputed every 30 frames and when the car changes.
#[allow(clippy::type_complexity)]
fn car_box(
    settings: Res<MotionBlurSettings>,
    mut cb: ResMut<CarBox>,
    anchors: Query<(Entity, &GlobalTransform), With<EnvCubeAnchor>>,
    children: Query<&Children>,
    meshes: Query<(&GlobalTransform, &Aabb)>,
) {
    if settings.effective_level() == 0 {
        return;
    }
    let Some((root, rt)) = anchors.iter().next() else {
        cb.root = None;
        return;
    };
    if cb.root == Some(root) && cb.found && cb.age < 30 {
        cb.age += 1;
        return;
    }
    let inv = rt.affine().inverse();
    let (mut lo, mut hi, mut n) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN), 0);
    for d in children.iter_descendants(root) {
        let Ok((gt, aabb)) = meshes.get(d) else { continue };
        if aabb.half_extents.max_element() > 4.0 {
            continue;
        }
        let m = inv * gt.affine();
        let c = Vec3::from(aabb.center);
        let h = Vec3::from(aabb.half_extents);
        for i in 0..8u32 {
            let s = Vec3::new(if i & 1 == 0 { -1.0 } else { 1.0 }, if i & 2 == 0 { -1.0 } else { 1.0 }, if i & 4 == 0 { -1.0 } else { 1.0 });
            let q = m.transform_point3(c + h * s);
            lo = lo.min(q);
            hi = hi.max(q);
        }
        n += 1;
    }
    let found = n > 0;
    let (lo, hi) = if found {
        (lo.max(Vec3::new(-1.8, -2.0, -3.8)), hi.min(Vec3::new(1.8, 3.0, 3.8)))
    } else {
        // Not loaded yet: a generic car around the centre of mass (root), ~0.5 m above the ground.
        (Vec3::new(-1.0, -0.6, -2.4), Vec3::new(1.0, 1.0, 2.4))
    };
    *cb = CarBox { root: Some(root), lo, hi, found, age: 0 };
}

fn extract_frame(
    mut commands: Commands,
    settings: Extract<Option<Res<MotionBlurSettings>>>,
    time: Extract<Res<Time<Real>>>,
    cb: Extract<Option<Res<CarBox>>>,
    anchors: Extract<Query<&GlobalTransform, With<EnvCubeAnchor>>>,
    mut frame: Local<u64>,
) {
    *frame += 1;
    let level = (*settings).as_deref().map_or(0, |s| s.effective_level());
    let car = (*cb)
        .as_deref()
        .filter(|b| b.root.is_some())
        .and_then(|b| anchors.iter().next().map(|g| (g.affine(), b.lo, b.hi)));
    commands.insert_resource(MbFrame { level, frame: *frame, dt: time.delta_secs(), car });
}

#[allow(clippy::too_many_arguments)]
fn motion_blur_system(
    view: ViewQuery<(&ViewTarget, &ExtractedView, &ViewDepthTexture), With<RemasterView>>,
    frame: Option<Res<MbFrame>>,
    pipe: Res<MbPipeline>,
    pipeline_cache: Res<PipelineCache>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut state: Local<MbState>,
    mut ctx: RenderContext,
) {
    let Some(frame) = frame.filter(|f| f.level > 0) else { return };
    let entity = view.entity();
    let (view_target, extracted, depth) = view.into_inner();
    let state = &mut *state;

    // Previous view of this camera (always recorded; a gap in frames = no blur this frame).
    let world_from_view = extracted.world_from_view.to_matrix().as_dmat4();
    let clip_from_view = extracted.clip_from_view.as_dmat4();
    let prev = state.prev.insert(entity, PrevView { frame: frame.frame, world_from_view, clip_from_view });
    let Some(prev) = prev.filter(|p| p.frame + 1 == frame.frame) else { return };
    // The depth texture becomes readable a frame after `depth_usage` asks for it.
    if !depth.texture.usage().contains(TextureUsages::TEXTURE_BINDING) {
        return;
    }

    // Camera motion this frame; skip when still (nothing to blur) and on cuts (view switches, teleports, hitches).
    let view_from_world = world_from_view.inverse();
    let prev_from_cur = prev.world_from_view.inverse() * world_from_view;
    let moved = prev_from_cur.w_axis.truncate().length();
    let trace = prev_from_cur.x_axis.x + prev_from_cur.y_axis.y + prev_from_cur.z_axis.z;
    let turned = ((trace - 1.0) * 0.5).clamp(-1.0, 1.0).acos();
    let dt = (frame.dt as f64).clamp(1.0 / 1000.0, 0.1);
    if moved < 1e-4 && turned < 1e-5 && prev.clip_from_view.abs_diff_eq(clip_from_view, 1e-6) {
        return;
    }
    if moved > 30.0 || moved / dt > 200.0 || turned > 0.7 || turned / dt > 20.0 {
        return;
    }
    let reproj = (prev.clip_from_view * prev_from_cur * clip_from_view.inverse()).as_mat4();

    let samples = depth.texture.sample_count();
    let format = extracted.target_format;
    let mut id = |smooth: bool| {
        let key = MbKey { format, multisampled: samples > 1, smooth };
        if let Some((_, id)) = state.ids.iter().find(|(k, _)| *k == key) {
            return *id;
        }
        let id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some(if smooth { "fh1_remaster_motion_blur_smooth".into() } else { "fh1_remaster_motion_blur".into() }),
            layout: vec![if key.multisampled { pipe.layout_ms.clone() } else { pipe.layout.clone() }],
            vertex: VertexState { shader: pipe.shader.clone(), shader_defs: vec![], entry_point: Some("vertex".into()), buffers: vec![] },
            fragment: Some(FragmentState {
                shader: pipe.shader.clone(),
                shader_defs: if key.multisampled { vec!["MULTISAMPLED".into()] } else { vec![] },
                entry_point: Some(if smooth { "mb_smooth".into() } else { "mb_blur".into() }),
                targets: vec![Some(ColorTargetState { format, blend: None, write_mask: ColorWrites::ALL })],
            }),
            ..default()
        });
        state.ids.push((key, id));
        id
    };
    let (blur_id, smooth_id) = (id(false), id(true));
    let Some(blur_pipeline) = pipeline_cache.get_render_pipeline(blur_id) else { return };
    let t = tuning();
    let smooth_pipeline = if t.smooth > 1.0 { pipeline_cache.get_render_pipeline(smooth_id) } else { None };

    // Uniform.
    let (shutter, max_frac, taps) = preset(frame.level);
    let shutter = t.shutter.unwrap_or(shutter).clamp(0.0, 4.0);
    let max_frac = t.max.unwrap_or(max_frac).clamp(0.0, 0.25);
    let taps = t.taps.unwrap_or(taps as f32).clamp(2.0, 32.0).round();
    let size = extracted.viewport.zw().as_vec2().max(Vec2::ONE);
    let near = clip_from_view.w_axis.z as f32;
    let (rect, car_far) = car_rect(frame.car, view_from_world, clip_from_view, near);
    let mut params = [0.0f32; 32];
    params[..16].copy_from_slice(&reproj.to_cols_array());
    params[16..20].copy_from_slice(&[(shutter as f64 / 60.0 / dt) as f32, max_frac * size.x, near, taps]);
    params[20..24].copy_from_slice(&[t.start, t.full, car_far, t.smooth]);
    params[24..28].copy_from_slice(&rect);
    params[28..32].copy_from_slice(&[0.02, 1.0, 0.0, 0.0]);
    let bytes: Vec<u8> = params.iter().flat_map(|f| f.to_le_bytes()).collect();
    let uniform = state
        .buffers
        .entry(entity)
        .or_insert_with(|| {
            render_device.create_buffer(&BufferDescriptor {
                label: Some("fh1_remaster_motion_blur_params"),
                size: bytes.len() as u64,
                usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        })
        .clone();
    render_queue.write_buffer(&uniform, 0, &bytes);
    let sampler = state
        .sampler
        .get_or_insert_with(|| {
            render_device.create_sampler(&SamplerDescriptor { mag_filter: FilterMode::Linear, min_filter: FilterMode::Linear, ..default() })
        })
        .clone();
    let layout = pipeline_cache.get_bind_group_layout(if samples > 1 { &pipe.layout_ms } else { &pipe.layout });

    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();
    let passes: [(&'static str, Option<&bevy::render::render_resource::RenderPipeline>); 2] =
        [("fh1_motion_blur", Some(blur_pipeline)), ("fh1_motion_blur_smooth", smooth_pipeline)];
    for (label, pipeline) in passes {
        let Some(pipeline) = pipeline else { continue };
        let post = view_target.post_process_write();
        let bind_group = render_device.create_bind_group(
            label,
            &layout,
            &[
                BindGroupEntry { binding: 0, resource: BindingResource::TextureView(depth.view()) },
                BindGroupEntry { binding: 1, resource: BindingResource::TextureView(post.source) },
                BindGroupEntry { binding: 2, resource: BindingResource::Sampler(&sampler) },
                BindGroupEntry { binding: 3, resource: uniform.as_entire_binding() },
            ],
        );
        let mut rp = ctx.begin_tracked_render_pass(RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: post.destination,
                depth_slice: None,
                resolve_target: None,
                // Every pixel is written (full-screen triangle, no blending): no clear, no load.
                ops: Operations { load: LoadOp::DontCare(Default::default()), store: StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let span = diagnostics.pass_span(&mut rp, label);
        rp.set_render_pipeline(pipeline);
        rp.set_bind_group(0, &bind_group, &[]);
        rp.draw(0..3, 0..1);
        span.end(&mut rp);
    }
}

/// The player car's screen rect (uv min.xy, max.xy) and the view depth of its far corner (m). Camera inside / behind the
/// box's near side (hood, cockpit): the whole screen, masked by depth only. No car: a rect off screen.
fn car_rect(car: Option<(Affine3A, Vec3, Vec3)>, view_from_world: DMat4, clip_from_view: DMat4, near: f32) -> ([f32; 4], f32) {
    const NONE: ([f32; 4], f32) = ([-10.0, -10.0, -10.0, -10.0], 0.0);
    let Some((world_from_car, lo, hi)) = car else { return NONE };
    let (mut uv_lo, mut uv_hi, mut far, mut inside) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN), 0.0f32, false);
    for i in 0..8u32 {
        let local = Vec3::new(if i & 1 == 0 { lo.x } else { hi.x }, if i & 2 == 0 { lo.y } else { hi.y }, if i & 4 == 0 { lo.z } else { hi.z });
        let v = view_from_world.transform_point3(world_from_car.transform_point3(local).as_dvec3());
        let z = (-v.z) as f32;
        far = far.max(z);
        if z < (near * 2.0).max(0.2) {
            inside = true;
            continue;
        }
        let c = clip_from_view * DVec4::new(v.x, v.y, v.z, 1.0);
        let ndc = Vec2::new((c.x / c.w) as f32, (c.y / c.w) as f32);
        let uv = Vec2::new(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
        uv_lo = uv_lo.min(uv);
        uv_hi = uv_hi.max(uv);
    }
    if far <= 0.0 {
        return NONE;
    }
    let far = far + 0.3;
    if inside {
        return ([-1.0, -1.0, 2.0, 2.0], far);
    }
    let m = Vec2::splat(0.02);
    let (a, b) = (uv_lo - m, uv_hi + m);
    ([a.x, a.y, b.x, b.y], far)
}
