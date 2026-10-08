//! Remaster grade (W3): FH1's colour grading on top of Bevy's tonemapping. One full-screen pass after
//! `tonemapping`: display-referred colour -> sRGB -> the game's 16³ grading LUT -> linear, plus a soft vignette.
//!
//! The LUT is fh1-render's [`FxPost::lut_image`]: postfx.rs keeps blending it on the CPU exactly as the game does
//! (zone ColorGradingMap/Night by DayColorGradeAmount/NightColorGradeAmount, track defaults outside zones), so the
//! remaster gets every zone's and the night's grade for free. The faithful chain's other stages (FH1 bloom,
//! adaptation, Hable filmic) are replaced by Bevy bloom, light.rs's exposure and AgX.
//!
//! Default tone map = the game's (FH1_RM_TONEMAP unset/game): Bevy's tonemapper is off and this pass runs FH1's Hable
//! curve with the track's `<filmicTone>` (day/night blended), the LUT on its output and the game's sqrt encoding, so the
//! contrast and black level match the faithful renderer; FH1_RM_TONEMAP=tony|agx|aces = Bevy's curve, LUT after it.
//! Env: FH1_RM_LUT=strength (default 1; 0 = pass off, Bevy tonemapper only), FH1_RM_VIGNETTE=amount (default 0.22),
//! FH1_RM_FILMIC_EV=ev (the curve's exposure offset, default -2.2), FH1_RM_FRAME_EV=ev (whole-frame offset incl. game-shader
//! output, default +0.4), FH1_RM_DUSK_EV / FH1_RM_NIGHT_EV (extra after sunset -1.4 / at night -1.0).

use bevy::core_pipeline::schedule::Core3d;
use bevy::core_pipeline::tonemapping::tonemapping;
use bevy::core_pipeline::Core3dSystems;
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{sampler, texture_2d, texture_3d, uniform_buffer_sized};
use bevy::render::render_resource::{
    BindGroupEntry, BindGroupLayoutDescriptor, BindingResource, Buffer, BufferDescriptor, BufferUsages, CachedRenderPipelineId, ColorTargetState,
    ColorWrites, FragmentState, Operations, PipelineCache, RenderPassColorAttachment, RenderPassDescriptor, RenderPipelineDescriptor, Sampler,
    SamplerBindingType, SamplerDescriptor, ShaderStages, TextureFormat, TextureSampleType, VertexState,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::texture::GpuImage;
use bevy::render::view::{ExtractedView, ViewTarget};
use bevy::render::{Render, RenderApp, RenderSystems};

use crate::light::{env_f32, RemasterView};

const GRADE_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var src_s: sampler;
@group(0) @binding(2) var lut: texture_3d<f32>;
@group(0) @binding(3) var lut_s: sampler;
struct Params { a: vec4<f32>, v: vec4<f32>, f1: vec4<f32>, f2: vec4<f32> };
@group(0) @binding(4) var<uniform> p: Params;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vertex(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055, c * 12.92, c <= vec3<f32>(0.0031308));
}

fn from_srgb(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

// FH1's Hable curve: f1 = (W, A, B, C), f2 = (D, E, F, exposure EV) (docs/SHADERS.md "FinalCombine").
fn hable(x: vec3<f32>) -> vec3<f32> {
    let A = p.f1.y; let B = p.f1.z; let C = p.f1.w; let D = p.f2.x; let E = p.f2.y; let F = p.f2.z;
    return (x * (A * x + C * B) + D * E) / (x * (A * x + B) + D * F) - E / F;
}

@fragment
fn fragment(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(src, src_s, in.uv, 0.0);
    var s: vec3<f32>;
    var g: vec3<f32>;
    if (p.a.y > 0.5) {
        // Game tone map (camera Tonemapping::None): exposure, Hable / Hable(W), the LUT on the curve's output, then
        // the game's sqrt output encoding read as the display's sRGB code value.
        let x = max(c.rgb, vec3<f32>(0.0)) * exp2(p.f2.w);
        let h = clamp(hable(x) / hable(vec3<f32>(p.f1.x)), vec3<f32>(0.0), vec3<f32>(1.0));
        let l = textureSampleLevel(lut, lut_s, h * (15.0 / 16.0) + 0.5 / 16.0, 0.0).rgb;
        s = sqrt(h);
        g = sqrt(clamp(l, vec3<f32>(0.0), vec3<f32>(1.0)));
    } else {
        // After Bevy's tonemapper: grade the display-referred colour in sRGB.
        s = to_srgb(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
        // 16^3 LUT: texel centres at (i + 0.5) / 16.
        g = textureSampleLevel(lut, lut_s, s * (15.0 / 16.0) + 0.5 / 16.0, 0.0).rgb;
    }
    var o = mix(s, g, p.a.x);
    // Vignette: smooth darkening towards the corners (aspect-corrected radius, 1 at the corner).
    let d = (in.uv - 0.5) * vec2<f32>(p.v.z, 1.0);
    let r = length(d) / length(vec2<f32>(0.5 * p.v.z, 0.5));
    o = o * (1.0 - p.v.x * pow(clamp(r, 0.0, 1.0), p.v.y));
    return vec4<f32>(from_srgb(o), c.a);
}
"#;

/// Main-world grade inputs, extracted every frame.
#[derive(Resource, Clone, Default, ExtractResource)]
pub struct GradeSettings {
    pub lut: Option<Handle<Image>>,
    pub strength: f32,
    pub vignette: f32,
    /// The game's filmic curve replaces Bevy's tonemapper (light.rs sets the camera to Tonemapping::None).
    pub game_tonemap: bool,
    /// (W, A, B, C, D, E, F, exposure EV), day/night blended.
    pub filmic: [f32; 8],
}

#[derive(Resource)]
struct GradePipeline {
    shader: Handle<Shader>,
    layout: BindGroupLayoutDescriptor,
    ids: Vec<(TextureFormat, CachedRenderPipelineId)>,
}

#[derive(Default)]
struct GradeCache {
    sampler: Option<Sampler>,
    uniform: Option<Buffer>,
}

pub(crate) fn plugin(app: &mut App) {
    let strength = env_f32("FH1_RM_LUT", 1.0);
    if strength <= 0.0 {
        return;
    }
    app.insert_resource(GradeSettings {
        lut: None,
        strength,
        vignette: env_f32("FH1_RM_VIGNETTE", 0.22),
        game_tonemap: game_tonemap(),
        filmic: [1.0, 0.7, 0.038, 0.634, 0.22, 0.003, 0.107, 0.31],
    })
        .add_plugins(ExtractResourcePlugin::<GradeSettings>::default())
        .add_systems(PostUpdate, follow_lut);
    let shader = app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(GRADE_WGSL, "fh1_remaster/grade.wgsl"));
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
    let entries = [
        texture_2d(TextureSampleType::Float { filterable: true }).build(0, ShaderStages::FRAGMENT),
        sampler(SamplerBindingType::Filtering).build(1, ShaderStages::FRAGMENT),
        texture_3d(TextureSampleType::Float { filterable: true }).build(2, ShaderStages::FRAGMENT),
        sampler(SamplerBindingType::Filtering).build(3, ShaderStages::FRAGMENT),
        uniform_buffer_sized(false, None).build(4, ShaderStages::FRAGMENT),
    ];
    render_app
        .insert_resource(GradePipeline { shader, layout: BindGroupLayoutDescriptor::new("fh1_remaster_grade_layout", &entries), ids: Vec::new() })
        .add_systems(Render, prepare_grade.in_set(RenderSystems::Prepare))
        // Before FXAA / SMAA (Options > Graphics > Anti-aliasing, fh1-engine ui/graphics.rs): they expect the final colour.
        .add_systems(
            Core3d,
            grade_system
                .in_set(Core3dSystems::PostProcess)
                .after(tonemapping)
                .before(bevy::anti_alias::fxaa::fxaa)
                .before(bevy::anti_alias::smaa::smaa),
        );
}

/// Calibration of light.rs's physical exposure to the game curve's input scale: the festival at 16:00 matched the
/// faithful frame's ground, car and black-level luma (2.2-2.5 EV apart) at this offset (2026-10-06, c3).
const FILMIC_EV: f32 = -2.2;
/// Whole-frame exposure offset on the curve input (game-shader output included: the sky dome, clouds and glows don't go
/// through the camera exposure). 2026-10-07 same-pose A/B vs faithful at the festival 16:00 with the TOD sun: median luma
/// matched at -0.4 EV camera bias, but sky and lit banners stayed ~0.72x; +0.4 here lifts both. FH1_RM_FRAME_EV=0 = old.
const FRAME_EV: f32 = 0.4;
/// Extra whole-frame EV once the sun has sunk ([`crate::light::twilight`] = 1) and at night (SunObjectMoon = 1). Same A/B:
/// with only [`FRAME_EV`] the frame was 2.6x the faithful luma at 20:00 (sky 3-4x) and 2.0x at 00:00; 08:00-19:00 matched
/// (0.94-1.02x). FH1_RM_DUSK_EV / FH1_RM_NIGHT_EV (0 = old).
const DUSK_EV: f32 = -1.4;
const NIGHT_EV: f32 = -1.0;

/// The whole-frame EV offset at this time of day.
fn frame_ev(l: &crate::light::RemasterLighting) -> f32 {
    env_f32("FH1_RM_FRAME_EV", FRAME_EV) + env_f32("FH1_RM_DUSK_EV", DUSK_EV) * l.twilight + env_f32("FH1_RM_NIGHT_EV", NIGHT_EV) * l.night
}

/// Multiply exposure-independent emission (emissive_exposure_weight 0) given in the game's display-linear units by this,
/// so it lands where the faithful renderer puts it: the game curve's input carries [`FILMIC_EV`] (and its env override).
/// 1 with a Bevy tonemapper.
pub fn game_unit_scale() -> f32 {
    if game_tonemap() {
        (-env_f32("FH1_RM_FILMIC_EV", FILMIC_EV)).exp2()
    } else {
        1.0
    }
}

/// `FH1_RM_TONEMAP` unset or `game`: FH1's own filmic curve (TrackSettings `<filmicTone>`) in the grade pass.
pub fn game_tonemap() -> bool {
    std::env::var("FH1_RM_TONEMAP").map_or(true, |v| v.eq_ignore_ascii_case("game")) && env_f32("FH1_RM_LUT", 1.0) > 0.0
}

/// The LUT handle follows FxPost (rebuilt on an in-process map change); the filmic curve follows the track's base
/// post settings, blended to the night variant by DayNightPostProcess like the game (postfx.rs update_post).
fn follow_lut(
    post: Option<Res<fh1_render::postfx::FxPost>>,
    tod: Option<Res<fh1_render::lighting::FxTimeOfDay>>,
    lighting: Option<Res<crate::light::RemasterLighting>>,
    mut s: ResMut<GradeSettings>,
) {
    let want = post.as_ref().map(|p| p.lut_image.clone());
    if s.lut != want {
        s.lut = want;
    }
    if let Some(p) = post {
        let k = tod.map_or(0.0, |t| t.tod.scalar_or("DayNightPostProcess", t.minutes(), 0.0));
        let f = p.base.filmic.at_night_amount(k);
        let want = [f.white, f.shoulder_strength, f.linear_strength, f.linear_angle, f.toe_strength, f.toe_numerator, f.toe_denominator, f.exposure + p.base.exposure_bias + env_f32("FH1_RM_FILMIC_EV", FILMIC_EV) + lighting.as_deref().map_or(FRAME_EV, frame_ev)];
        if s.filmic != want {
            s.filmic = want;
        }
    }
}

fn prepare_grade(mut pipe: ResMut<GradePipeline>, cache: Res<PipelineCache>, views: Query<&ExtractedView, With<RemasterView>>) {
    for v in &views {
        let format = v.target_format;
        if pipe.ids.iter().any(|(f, _)| *f == format) {
            continue;
        }
        let id = cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("fh1_remaster_grade".into()),
            layout: vec![pipe.layout.clone()],
            vertex: VertexState { shader: pipe.shader.clone(), shader_defs: vec![], entry_point: Some("vertex".into()), buffers: vec![] },
            fragment: Some(FragmentState {
                shader: pipe.shader.clone(),
                shader_defs: vec![],
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState { format, blend: None, write_mask: ColorWrites::ALL })],
            }),
            ..default()
        });
        pipe.ids.push((format, id));
    }
}

#[allow(clippy::too_many_arguments)]
fn grade_system(
    view: ViewQuery<(&ViewTarget, &ExtractedView), With<RemasterView>>,
    settings: Option<Res<GradeSettings>>,
    pipe: Res<GradePipeline>,
    pipeline_cache: Res<PipelineCache>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    images: Res<RenderAssets<GpuImage>>,
    mut cache: Local<GradeCache>,
    mut ctx: RenderContext,
) {
    let (view_target, extracted) = view.into_inner();
    let Some(settings) = settings else { return };
    let Some(lut) = settings.lut.as_ref().and_then(|h| images.get(h)) else { return };
    let Some((_, id)) = pipe.ids.iter().find(|(f, _)| *f == extracted.target_format) else { return };
    let Some(pipeline) = pipeline_cache.get_render_pipeline(*id) else { return };
    let size = extracted.viewport.zw().as_vec2();
    let aspect = size.x / size.y.max(1.0);
    let f = settings.filmic;
    let params: [f32; 16] = [
        settings.strength.min(1.0),
        settings.game_tonemap as u32 as f32,
        0.0,
        0.0,
        settings.vignette,
        2.2,
        aspect,
        0.0,
        f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7],
    ];
    let bytes: Vec<u8> = params.iter().flat_map(|f| f.to_le_bytes()).collect();
    let uniform = cache
        .uniform
        .get_or_insert_with(|| {
            render_device.create_buffer(&BufferDescriptor {
                label: Some("fh1_remaster_grade_params"),
                size: bytes.len() as u64,
                usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        })
        .clone();
    render_queue.write_buffer(&uniform, 0, &bytes);
    let sampler = cache
        .sampler
        .get_or_insert_with(|| {
            render_device.create_sampler(&SamplerDescriptor {
                mag_filter: bevy::render::render_resource::FilterMode::Linear,
                min_filter: bevy::render::render_resource::FilterMode::Linear,
                ..default()
            })
        })
        .clone();
    let post = view_target.post_process_write();
    let bind_group = render_device.create_bind_group(
        "fh1_remaster_grade",
        &pipeline_cache.get_bind_group_layout(&pipe.layout),
        &[
            BindGroupEntry { binding: 0, resource: BindingResource::TextureView(post.source) },
            BindGroupEntry { binding: 1, resource: BindingResource::Sampler(&sampler) },
            BindGroupEntry { binding: 2, resource: BindingResource::TextureView(&lut.texture_view) },
            BindGroupEntry { binding: 3, resource: BindingResource::Sampler(&lut.sampler) },
            BindGroupEntry { binding: 4, resource: uniform.as_entire_binding() },
        ],
    );
    let mut rp = ctx.command_encoder().begin_render_pass(&RenderPassDescriptor {
        label: Some("fh1_remaster_grade"),
        color_attachments: &[Some(RenderPassColorAttachment { view: post.destination, depth_slice: None, resolve_target: None, ops: Operations::default() })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, &bind_group, &[]);
    rp.draw(0..3, 0..1);
}
