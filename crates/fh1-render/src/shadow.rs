//! FH1 sun shadows: cascaded shadow maps resolved into the screen shadow mask that the game's
//! materials read as `ShadowMaskSamp` (tf13). Spec and status: docs/SHADOWS.md.
//!
//! - **Casters** are drawn by Bevy's directional-light shadow pass (one 512² layer per cascade, i.e.
//!   the game's 2×2 atlas of a 1024² map). [`FxMaterial`](crate::FxMaterial) joins it, and Bevy's
//!   depth prepass, through [`specialize_prepass`]. The game's DepthOnly / ShadowDepthOnly cull and
//!   alpha-kill rules apply. Which entities cast is decided where they are spawned
//!   (`NotShadowCaster`; scenery uses the `.pvs` record flag).
//! - **Cascades**: Bevy's fit is replaced by the game's splits (far 25/50/200/600 m, light distance
//!   1000/1000/1000/1300 m) in [`fit_cascades`]; the light direction is TrackSettings
//!   `ShadowLightDirection` when `OverrideShadowLightDirection` is set.
//! - **Mask**: after the prepass and the shadow pass, one fullscreen pass per view evaluates every split
//!   that covers the pixel with the game's filters, ported from the ApplyShadowsPCF3x3WithFading
//!   (split 0) and ApplyShadowsPCF2x2WithFading shaders, and keeps the minimum. Output .x =
//!   `CameraOriginAndShadowIntensity.w + pcf + fade`. Materials pick it up in [`bind_shadow_mask`].

use bevy::asset::RenderAssetUsages;
use bevy::core_pipeline::prepass::{DepthPrepass, ViewPrepassTextures};
use bevy::core_pipeline::schedule::Core3d;
use bevy::core_pipeline::Core3dSystems;
use bevy::light::cascade::{Cascade, CascadeShadowConfig, Cascades};
use bevy::light::{DirectionalLightShadowMap, SimulationLightSystems, SunDisk};
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::pbr::{MeshPipelineKey, ViewShadowBindings};
use bevy::prelude::*;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{texture_2d_array, texture_depth_2d, texture_depth_2d_multisampled, uniform_buffer_sized};
use bevy::render::render_resource::{
    AsBindGroup, BindGroupEntry, BindGroupLayoutDescriptor, BindingResource, BufferInitDescriptor, BufferUsages, CachedRenderPipelineId,
    ColorTargetState, ColorWrites, Extent3d, Face, FragmentState, LoadOp, Operations, PipelineCache, RenderPassColorAttachment,
    RenderPassDescriptor, RenderPipelineDescriptor, ShaderStages, StoreOp, TextureDimension, TextureFormat, TextureSampleType,
    TextureUsages, TextureViewDescriptor, TextureViewDimension, VertexState,
};
use bevy::render::renderer::{RenderContext, RenderDevice, ViewQuery};
use bevy::render::texture::GpuImage;
use bevy::render::view::ExtractedView;
use bevy::render::{Render, RenderApp, RenderSystems};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use crate::car_material::FxCarMaterial;

/// In-run A/B overrides (engine perf/p6.rs): 0 = the env/default choice, 1 = force on, 2 = force off.
pub static P6_MAIN_ONLY: AtomicU8 = AtomicU8::new(0);
/// See [`P6_MAIN_ONLY`]; for [`lazy_casters`].
pub static P6_LAZY_CASTERS: AtomicU8 = AtomicU8::new(0);
use crate::material::{FxKey, FxMaterial, PROGRAMS};
use crate::program::Cull;

/// Extra receiver bias (m, along the light) for the car shaders. The car moves against the world-aligned cascades
/// (~5 cm texels in split 0), so its fine self-shadow (lamp recesses, bumper lips) re-rasterised differently every
/// frame and crawled over the rear and the lamp plastics, even parked (user report 2026-10-05; gone with
/// FH1_CAR_CSM=0). Shadows of other casters and of the car's big shapes stay. GUESSED value; FH1_CAR_SHADOW_BIAS=m.
pub fn car_shadow_bias() -> f32 {
    static B: OnceLock<f32> = OnceLock::new();
    *B.get_or_init(|| std::env::var("FH1_CAR_SHADOW_BIAS").ok().and_then(|v| v.parse().ok()).unwrap_or(CAR_SHADOW_BIAS))
}

/// Lamp-region pixels changing per frame, parked, sun on the rear (same scene): 0 m ~700, 0.15 ~108, 0.3 ~57 (caster off ~21).
const CAR_SHADOW_BIAS: f32 = 0.3;

/// The game's CSM setup (default.xex 0x82DE4500 → 0x82DDDB18 per split, VERIFIED constants).
#[derive(Resource, Clone, Debug, ExtractResource)]
pub struct FxShadowSettings {
    /// Split far distances (m).
    pub far: [f32; 4],
    /// Splits drawn in game: TrackSettings `ShadowEnds InGameShadowEnd` (202 m on Colorado) ends the shadows
    /// inside split 2, so split 3 (600 m) is not drawn in game (INFERRED; it may serve the track-cam).
    pub splits: usize,
    /// Distance from the split to the light camera (m).
    pub light_distance: [f32; 4],
    /// The game's per-split depth bias (normalised depth).
    pub depth_bias: [f32; 4],
    /// Extra bias in metres along the light (GUESSED; the game relies on back-face casters).
    pub bias_m: [f32; 4],
    /// Fraction of each split over which it fades out (GUESSED: LinDepthAndFadingParams.zw untraced).
    pub fade: f32,
    /// Shadow light direction (the way the light travels, engine space); `None` = keep the light's own.
    pub direction: Option<Vec3>,
    /// `CameraOriginAndShadowIntensity.w`: the mask value in full shadow (set from the TOD).
    pub floor: f32,
    /// Casters whose bounding radius is under this many texels of a split are left out of it (GUESSED).
    pub min_texels: f32,
    pub enabled: bool,
}

/// The game's cascade texture size: 512² (its 2×2 atlas of 1024²).
pub const GAME_CASCADE_SIZE: u32 = 512;

/// Our cascade texture size. Default 1024² (P4, 2026-10-05): a caster moving against the world-snapped cascades
/// changes the map only when its silhouette crosses a texel centre, so at 512² (~10 cm texels in split 0 at the
/// chase FOV) the car's shadow advanced in visible steps a few dozen times a second; unnoticeable at the game's
/// 30 fps, "shadows update at a low rate" at 300 Hz. 1024² halves the step. `FH1_SHADOW_RES=512` = the game's size.
/// Per-texel tunings (receiver half-texel bias, min-texel culling) stay in game texels, so only the step changes.
pub fn cascade_size() -> u32 {
    static S: OnceLock<u32> = OnceLock::new();
    *S.get_or_init(|| std::env::var("FH1_SHADOW_RES").ok().and_then(|v| v.parse().ok()).unwrap_or(1024u32).clamp(256, 4096))
}

/// Our texel size over the game's, for tunings expressed in game texels.
fn texel_to_game() -> f32 {
    cascade_size() as f32 / GAME_CASCADE_SIZE as f32
}

impl Default for FxShadowSettings {
    fn default() -> Self {
        Self {
            far: [25.0, 50.0, 200.0, 600.0],
            splits: 4,
            light_distance: [1000.0, 1000.0, 1000.0, 1300.0],
            depth_bias: [1e-7, 1.1e-7, 2.1e-7, 2.3e-7],
            bias_m: [0.03, 0.05, 0.15, 0.4],
            fade: 0.1,
            direction: None,
            floor: 0.0,
            // 3 texels: ~1.5 ms cheaper, no visible change at the bush test spot (FH1_TELEPORT=-4700,30,-900,90).
            min_texels: 3.0,
            // On by default (user, 2026-10-04; ~+9 ms); FH1_SHADOWS=0 turns them off.
            enabled: shadows_on(),
        }
    }
}

impl FxShadowSettings {
    /// Read `<OverrideShadowLightDirection>` / `<ShadowLightDirection>` from TrackSettings.xml. The XML is
    /// left-handed (+Z north): Z is negated into engine space.
    pub fn apply_track_settings(&mut self, xml: &str) {
        let attr = |tag: &str, name: &str| -> Option<f32> {
            let at = xml.find(&format!("<{tag} "))?;
            let rest = &xml[at..];
            let rest = &rest[..rest.find('>')?];
            let k = rest.find(&format!("{name}=\""))? + name.len() + 2;
            rest[k..].split('"').next()?.trim().parse().ok()
        };
        if let Some(end) = attr("ShadowEnds", "InGameShadowEnd") {
            if let Some(k) = self.far.iter().position(|&f| f >= end * 0.95) {
                self.far[k] = end;
                self.splits = k + 1;
            }
        }
        if attr("OverrideShadowLightDirection", "value").unwrap_or(0.0) != 0.0 {
            if let (Some(x), Some(y), Some(z)) = (attr("ShadowLightDirection", "x"), attr("ShadowLightDirection", "y"), attr("ShadowLightDirection", "z")) {
                self.direction = Some(Vec3::new(x, y, -z).normalize_or(Vec3::NEG_Y));
            }
        }
    }
}

/// Marker for the camera that gets a shadow mask. Added automatically to every `FxPostCamera`
/// (with Bevy's `DepthPrepass`).
#[derive(Component, Clone, Copy, Default, ExtractComponent)]
pub struct FxShadowCamera;

/// The screen shadow mask image bound as tf13 on every FX material.
#[derive(Resource, Clone, ExtractResource)]
pub struct FxShadowMask {
    pub image: Handle<Image>,
    size: UVec2,
}

/// Per-frame mask constants (main world → render world).
#[derive(Resource, Clone, Default, ExtractResource)]
struct ShadowFrame {
    active: bool,
    /// Light clip from world per split (Bevy's reverse-Z orthographic cascades).
    clip_from_world: [Mat4; 4],
    world_from_view: Mat4,
    /// (P00, P11, near, 0) of the camera's infinite reverse-Z projection.
    proj: Vec4,
    /// Per split: (start, far, fadeScale, fadeBias) in view depth.
    ranges: [Vec4; 4],
    /// Per split depth bias in light clip units.
    bias: Vec4,
    floor: f32,
}

pub struct FxShadowPlugin;

impl Plugin for FxShadowPlugin {
    fn build(&self, app: &mut App) {
        let image = {
            let mut images = app.world_mut().resource_mut::<Assets<Image>>();
            images.add(mask_image(UVec2::ONE))
        };
        PREPASS_SHADER.get_or_init(|| app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(PREPASS_WGSL, "fh1/shadow_prepass.wgsl")));
        CASTER_SHADER.get_or_init(|| app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(CASTER_WGSL, "fh1/shadow_caster.wgsl")));
        let mask_shader = app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(MASK_WGSL, "fh1/shadow_mask.wgsl"));
        app.add_plugins(bevy::pbr::MaterialPlugin::<FxCasterMaterial>::default())
            .init_resource::<CasterMaterials>()
            .add_systems(Update, sync_caster_proxies)
            .add_systems(PostUpdate, lazy_casters.before(bevy::camera::visibility::VisibilitySystems::VisibilityPropagate))
            .insert_resource(FxShadowMask { image, size: UVec2::ONE })
            .init_resource::<FxShadowSettings>()
            .init_resource::<ShadowFrame>()
            .insert_resource(DirectionalLightShadowMap { size: cascade_size() as usize })
            .add_plugins((
                ExtractResourcePlugin::<FxShadowMask>::default(),
                ExtractResourcePlugin::<ShadowFrame>::default(),
                ExtractComponentPlugin::<FxShadowCamera>::default(),
            ))
            .add_systems(Update, (tag_cameras, bind_shadow_mask, resize_mask, skip_non_casters))
            // Debug material picker (FH1_FX_PICK), registered here for want of a scenery plugin.
            .add_systems(Update, (crate::scenery::fx_pick, crate::scenery::fx_ray, crate::scenery::fx_stats))
            .add_systems(PostUpdate, (track_shadow_inputs, configure_light).chain().before(bevy::transform::TransformSystems::Propagate))
            .add_systems(
                PostUpdate,
                fit_cascades.after(SimulationLightSystems::UpdateDirectionalLightCascades).before(SimulationLightSystems::UpdateLightFrusta),
            )
            .insert_resource(ab_from_env())
            .add_systems(PostUpdate, shadow_ab.after(track_shadow_inputs).before(configure_light))
            .add_systems(PostUpdate, cull_small_casters.after(SimulationLightSystems::CheckLightVisibility));
        // The screen-space mask pass needs a depth prepass that matches the colour pass exactly; it didn't
        // (holes where the colour pass discards), so it is off and the materials evaluate the mask per
        // fragment instead. FH1_SHADOW_MASKPASS=1 brings the old path back for comparison.
        if std::env::var("FH1_SHADOW_MASKPASS").map_or(true, |v| v != "1") {
            return;
        }
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app
            .insert_resource(MaskPipelines { shader: mask_shader, ids: Default::default() })
            .add_systems(Render, prepare_mask_pipelines.in_set(RenderSystems::Prepare))
            .add_systems(
                Core3d,
                shadow_mask_system
                    .after(Core3dSystems::Prepass)
                    .after(bevy::pbr::per_view_shadow_pass::<{ bevy::pbr::LATE_SHADOW_PASS }>)
                    .after(bevy::pbr::shared_shadow_pass::<{ bevy::pbr::LATE_SHADOW_PASS }>)
                    .before(Core3dSystems::MainPass),
            );
    }
}

fn mask_image(size: UVec2) -> Image {
    let mut image = Image::new_fill(
        Extent3d { width: size.x.max(1), height: size.y.max(1), depth_or_array_layers: 1 },
        TextureDimension::D2,
        &[255, 0, 0, 0],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST;
    image
}

/// Every FH1 post camera gets the shadow mask (and the depth prepass it is resolved from).
fn tag_cameras(mut commands: Commands, cams: Query<Entity, (With<crate::post::FxPostCamera>, Without<FxShadowCamera>)>) {
    for e in &cams {
        commands.entity(e).insert(FxShadowCamera);
        if std::env::var("FH1_SHADOW_MASKPASS").is_ok_and(|v| v == "1") {
            commands.entity(e).insert(DepthPrepass);
        }
    }
}

/// FX meshes that never cast: the sky dome (it would cover the whole map) and blended programs (water,
/// glass, decals: the game's ShadowDepthOnly passes are opaque/alpha-tested only, INFERRED).
/// Opaque casters cast through a [`CasterProxy`] instead of their own material (see [`FxCasterMaterial`]).
#[allow(clippy::type_complexity)]
fn skip_non_casters(
    mut commands: Commands,
    new: Query<
        (Entity, &Mesh3d, &MeshMaterial3d<FxMaterial>, Has<crate::sky::SkyPart>, Option<&bevy::camera::visibility::VisibilityRange>, Has<bevy::mesh::skinning::SkinnedMesh>),
        (Added<MeshMaterial3d<FxMaterial>>, Without<bevy::light::NotShadowCaster>),
    >,
    materials: Res<Assets<FxMaterial>>,
    mut casters: ResMut<CasterMaterials>,
    mut caster_assets: ResMut<Assets<FxCasterMaterial>>,
) {
    for (e, mesh, m, sky, range, skinned) in &new {
        let Some(m) = materials.get(&m.0) else { continue };
        if sky || m.alpha_blend {
            commands.entity(e).try_insert(bevy::light::NotShadowCaster);
            continue;
        }
        // Skinned meshes cast through their own material: a proxy child has no skin, so its shadow wouldn't move.
        if casters.off || skinned {
            continue;
        }
        let key = FxKey::from(m);
        let Some(cull) = caster_cull(key) else { continue };
        let material = match caster_rule(key) {
            // Masked programs need t0 for the kill; without one they cast like opaque ones.
            Some(rule) if casters.masked => match m.t0.as_ref() {
                Some(t0) => casters.get_masked(cull, rule, t0, m.consts.ps[0], &mut caster_assets),
                None => casters.get(cull, &mut caster_assets),
            },
            Some(_) => continue,
            None => casters.get(cull, &mut caster_assets),
        };
        // Queued so it only acts if `e` still exists when commands apply: zone streaming can despawn it in the
        // same frame, which made the insert panic and left the proxy an orphan (0f, 2026-10-04).
        let (mesh, range) = (mesh.0.clone(), range.cloned());
        commands.queue(move |world: &mut World| {
            if world.get_entity(e).is_err() {
                return;
            }
            let mut proxy = world.spawn((
                Mesh3d(mesh),
                MeshMaterial3d(material),
                Transform::IDENTITY,
                bevy::camera::visibility::RenderLayers::layer(CASTER_LAYER),
                ChildOf(e),
            ));
            if let Some(range) = range {
                proxy.insert(range);
            }
            let proxy = proxy.id();
            world.entity_mut(e).insert((bevy::light::NotShadowCaster, CasterProxy(proxy)));
        });
    }
}

/// Keep a proxy's mesh in step with its owner's.
fn sync_caster_proxies(
    owners: Query<(&Mesh3d, &CasterProxy), Changed<Mesh3d>>,
    mut proxies: Query<&mut Mesh3d, Without<CasterProxy>>,
    all: Query<(), With<MeshMaterial3d<FxCasterMaterial>>>,
    owned: Query<(), With<CasterProxy>>,
    fx: Query<(), With<MeshMaterial3d<FxMaterial>>>,
    time: Res<Time<Real>>,
    mut last: Local<f32>,
) {
    if std::env::var("FH1_SHADOW_STATS").is_ok_and(|v| v == "1") && time.elapsed_secs() - *last >= 2.0 {
        *last = time.elapsed_secs();
        info!("caster proxies: {} proxies, {} owners, {} fx meshes", all.iter().count(), owned.iter().count(), fx.iter().count());
    }
    for (mesh, proxy) in &owners {
        if let Ok(mut m) = proxies.get_mut(proxy.0) {
            if m.0 != mesh.0 {
                m.0 = mesh.0.clone();
            }
        }
    }
}

/// Lazy caster proxies (P6, OPT-IN `FH1_LAZY_CASTERS=1`; `FH1_LAZY_CASTER_MARGIN=m`, default 100). The festival
/// in-run A/B measured it 0.7 ms WORSE on the render thread (the Visibility changes re-extract the proxies; Bevy's
/// own frustum tests were cheaper), so it stays off:
/// every 8 frames, a proxy that could only land in a split where [`cull_small_casters`] would drop it anyway, or
/// beyond the last split, is hidden. Bevy then skips it in the per-cascade frustum tests and the shadow bins (~22k
/// proxies at the festival, most of them small props or far zone batches). The test uses the horizontal distance
/// from the camera minus the caster radius and `margin` (a caster toward the sun throws its shadow back towards the
/// camera; 100 m covers a 25 m building at 15° sun), so a kept caster is still tested per split as before.
#[allow(clippy::type_complexity)]
fn lazy_casters(
    settings: Res<FxShadowSettings>,
    ab: Res<ShadowAb>,
    cams: Query<(Entity, &GlobalTransform, &Camera), With<FxShadowCamera>>,
    lights: Query<(&DirectionalLight, &Cascades)>,
    mut proxies: Query<(&bevy::camera::primitives::Aabb, &GlobalTransform, &mut Visibility), With<MeshMaterial3d<FxCasterMaterial>>>,
    mut frame: Local<u32>,
    mut was_on: Local<bool>,
    time: Res<Time<Real>>,
    mut last_log: Local<f32>,
) {
    static ENV: OnceLock<(bool, f32)> = OnceLock::new();
    let (env_on, margin) = *ENV.get_or_init(|| {
        let on = std::env::var("FH1_LAZY_CASTERS").is_ok_and(|v| v == "1");
        let margin = std::env::var("FH1_LAZY_CASTER_MARGIN").ok().and_then(|v| v.parse().ok()).unwrap_or(100.0f32);
        (on, margin)
    });
    let on = match P6_LAZY_CASTERS.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => env_on,
    } && settings.enabled;
    if !on {
        if std::mem::take(&mut *was_on) {
            for (_, _, mut vis) in &mut proxies {
                vis.set_if_neq(Visibility::Inherited);
            }
        }
        return;
    }
    *frame = frame.wrapping_add(1);
    // Right after switching on, and then every 8th frame.
    if *was_on && *frame % 8 != 0 {
        return;
    }
    *was_on = true;
    let Some((cam, cam_t, _)) = cams.iter().find(|c| c.2.is_active) else { return };
    let Some(splits) = lights.iter().find(|l| l.0.shadow_maps_enabled).and_then(|l| l.1.cascades.get(&cam)) else { return };
    static MIN_TEXELS: OnceLock<f32> = OnceLock::new();
    let min_texels = *MIN_TEXELS.get_or_init(|| std::env::var("FH1_SHADOW_MIN_TEXELS").ok().and_then(|v| v.parse().ok()).unwrap_or(settings.min_texels));
    let min_texels = ab.min_texels.unwrap_or(min_texels);
    let n = settings.splits.min(splits.len());
    let min_r: Vec<f32> = splits[..n].iter().map(|c| min_texels * c.texel_size * texel_to_game()).collect();
    let here = cam_t.translation().xz();
    let (mut shown, mut hidden) = (0usize, 0usize);
    for (aabb, t, mut vis) in &mut proxies {
        let (s, _, _) = t.to_scale_rotation_translation();
        let r = (Vec3::from(aabb.half_extents) * s.abs()).length();
        let d = t.transform_point(aabb.center.into()).xz().distance(here) - r - margin;
        let keep = (0..n).find(|&i| settings.far[i] >= d).is_some_and(|i| r >= min_r[i]);
        vis.set_if_neq(if keep { Visibility::Inherited } else { Visibility::Hidden });
        if keep {
            shown += 1;
        } else {
            hidden += 1;
        }
    }
    if std::env::var("FH1_SHADOW_STATS").is_ok_and(|v| v == "1") && time.elapsed_secs() - *last_log >= 2.0 {
        *last_log = time.elapsed_secs();
        info!("lazy casters: {shown} kept, {hidden} hidden (margin {margin} m)");
    }
}

// ---------------------------------------------------------------- shared caster material

/// Render layer only the shadow light sees: the proxies live there, so cameras never draw them.
pub const CASTER_LAYER: usize = 30;

/// On an opaque FX mesh: the shadow-only child that casts for it.
#[derive(Component, Clone, Copy)]
pub struct CasterProxy(pub Entity);

/// Shadow-only material shared by FX casters. The game's opaque ShadowDepthOnly passes have no pixel
/// shader (VERIFIED, see [`specialize_prepass`]), so an opaque caster needs nothing from its own material
/// but the cull side. Every FxMaterial is its own bind group, so Bevy could not batch the ~3,000 shadow
/// draws a frame (≈8 ms); with one material per cull side they multi-draw (Bevy drops the material bind
/// group from opaque depth-only draws). Alpha-tested casters keep the game's kill and share one material
/// per (t0, kill rule, uv scale, cull side) instead of one per instance. `FH1_SHADOW_PROXY=0` = old path,
/// `FH1_SHADOW_PROXY=opaque` = proxies for opaque casters only.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
#[bind_group_data(CasterKey)]
pub struct FxCasterMaterial {
    /// xy: uv scale of the mask rule (PS c0.xy of the owner).
    #[uniform(0)]
    pub scale: Vec4,
    #[texture(1)]
    #[sampler(2)]
    pub t0: Option<Handle<Image>>,
    /// 0 = two-sided, 1 = cull back faces, 2 = cull front faces (already the shadow-pass side).
    pub cull: u8,
    /// 0 = opaque, 1 = mask rule (kill a < 0.5 at uv × scale), 2 = tree rule (kill a < 0.1).
    pub rule: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CasterKey {
    cull: u8,
    rule: u8,
}

impl From<&FxCasterMaterial> for CasterKey {
    fn from(m: &FxCasterMaterial) -> Self {
        Self { cull: m.cull, rule: m.rule }
    }
}

impl FxCasterMaterial {
    /// An opaque caster with the given cull side (0 two-sided, 1 back culled, 2 front culled).
    pub fn opaque(cull: u8) -> Self {
        Self { scale: Vec4::ONE, t0: None, cull, rule: 0 }
    }
}

static CASTER_SHADER: OnceLock<Handle<Shader>> = OnceLock::new();

impl Material for FxCasterMaterial {
    fn alpha_mode(&self) -> AlphaMode {
        if self.rule == 0 { AlphaMode::Opaque } else { AlphaMode::Mask(0.5) }
    }

    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        true
    }

    fn prepass_fragment_shader() -> bevy::shader::ShaderRef {
        bevy::shader::ShaderRef::Handle(CASTER_SHADER.get().unwrap().clone())
    }

    fn specialize(
        _pipeline: &bevy::pbr::MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        key: bevy::pbr::MaterialPipelineKey<Self>,
    ) -> Result<(), bevy::render::render_resource::SpecializedMeshPipelineError> {
        descriptor.primitive.cull_mode = match key.bind_group_data.cull {
            1 => Some(Face::Back),
            2 => Some(Face::Front),
            _ => None,
        };
        if let Some(f) = descriptor.fragment.as_mut() {
            if key.bind_group_data.rule != 0 && layout.0.contains(Mesh::ATTRIBUTE_UV_0) {
                f.shader_defs.push("FX_ALPHA_KILL".into());
                if key.bind_group_data.rule == 2 {
                    f.shader_defs.push("FX_TREE".into());
                }
            }
        }
        Ok(())
    }
}

const CASTER_WGSL: &str = r#"
#import bevy_pbr::prepass_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> fx_scale: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var fx_t0: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var fx_s0: sampler;

@fragment
fn fragment(in: VertexOutput) {
#ifdef FX_ALPHA_KILL
#ifdef VERTEX_UVS_A
#ifdef FX_TREE
    if textureSample(fx_t0, fx_s0, in.uv).a < 0.1 {
        discard;
    }
#else
    if textureSample(fx_t0, fx_s0, in.uv * fx_scale.xy).a < 0.5 {
        discard;
    }
#endif
#endif
#endif
}
"#;

/// The shared caster materials: three opaque ones (per cull side) and the masked ones by
/// (t0, rule, uv scale, cull).
#[derive(Resource)]
struct CasterMaterials {
    handles: [Option<Handle<FxCasterMaterial>>; 3],
    masked_ids: std::collections::HashMap<(AssetId<Image>, u8, u8, [u32; 2]), AssetId<FxCasterMaterial>>,
    off: bool,
    /// Alpha-tested casters get proxies too (else they cast through their own FxMaterial).
    masked: bool,
}

/// Shadows are on unless FH1_SHADOWS=0.
fn shadows_on() -> bool {
    std::env::var("FH1_SHADOWS").map_or(true, |v| v != "0")
}

impl Default for CasterMaterials {
    fn default() -> Self {
        // Only spawned when shadows can be on this run: otherwise proxies just add entities.
        let shadows = shadows_on() || std::env::var("FH1_SHADOW_AB").is_ok();
        let env = std::env::var("FH1_SHADOW_PROXY").unwrap_or_default();
        Self { handles: Default::default(), masked_ids: default(), off: !shadows || env == "0", masked: env != "opaque" }
    }
}

impl CasterMaterials {
    fn get(&mut self, cull: u8, assets: &mut Assets<FxCasterMaterial>) -> Handle<FxCasterMaterial> {
        self.handles[cull as usize]
            .get_or_insert_with(|| assets.add(FxCasterMaterial::opaque(cull)))
            .clone()
    }

    fn get_masked(&mut self, cull: u8, rule: u8, t0: &Handle<Image>, c0: Vec4, assets: &mut Assets<FxCasterMaterial>) -> Handle<FxCasterMaterial> {
        // Same scale rule as the prepass shader: c0.xy unless either is zero.
        let scale = if rule == 1 && c0.x != 0.0 && c0.y != 0.0 { Vec2::new(c0.x, c0.y) } else { Vec2::ONE };
        let key = (t0.id(), rule, cull, [scale.x.to_bits(), scale.y.to_bits()]);
        // The map holds ids only, so a material (and its texture) goes once its last proxy does.
        if let Some(h) = self.masked_ids.get(&key).and_then(|id| assets.get_strong_handle(*id)) {
            return h;
        }
        let h = assets.add(FxCasterMaterial { scale: scale.extend(0.0).extend(0.0), t0: Some(t0.clone()), cull, rule });
        self.masked_ids.insert(key, h.id());
        h
    }
}

/// The kill rule of an FX program's shadow pass, as [`specialize_prepass`] picks it: `None` = opaque,
/// 1 = mask rule, 2 = tree rule (two-sided).
fn caster_rule(key: FxKey) -> Option<u8> {
    let programs = PROGRAMS.read().unwrap();
    let st = programs.get(key.program as usize)?.state;
    if !(st.alpha_to_coverage && st.blend.is_none()) {
        return None;
    }
    Some(if key.no_cull || st.cull == Cull::None { 2 } else { 1 })
}

/// The shadow-pass cull side of an FX program, as [`specialize_prepass`] picks it (0 none, 1 back,
/// 2 front); `None` when the program is unknown.
fn caster_cull(key: FxKey) -> Option<u8> {
    let programs = PROGRAMS.read().unwrap();
    let st = programs.get(key.program as usize)?.state;
    if key.no_cull || st.cull == Cull::None {
        return Some(0);
    }
    let back = (st.cull == Cull::Cw) ^ true ^ key.flip_cull;
    Some(if back { 1 } else { 2 })
}

/// Keep the mask at the shadow camera's render size.
fn resize_mask(mut mask: ResMut<FxShadowMask>, mut images: ResMut<Assets<Image>>, cams: Query<&Camera, With<FxShadowCamera>>) {
    let Some(size) = cams.iter().find_map(|c| c.physical_viewport_size()) else { return };
    if size == mask.size {
        return;
    }
    mask.size = size;
    let _ = images.insert(&mask.image, mask_image(size));
}

/// Bind the mask as tf13 (ShadowMaskSamp) on every FX material as it is created.
fn bind_shadow_mask(
    mask: Res<FxShadowMask>,
    mut fx_events: MessageReader<AssetEvent<FxMaterial>>,
    mut car_events: MessageReader<AssetEvent<FxCarMaterial>>,
    mut fx: ResMut<Assets<FxMaterial>>,
    mut car: ResMut<Assets<FxCarMaterial>>,
) {
    for ev in fx_events.read() {
        if let AssetEvent::Added { id } = ev {
            if fx.get(*id).is_some_and(|m| m.t13.is_none()) {
                if let Some(mut m) = fx.get_mut(*id) {
                    m.t13 = Some(mask.image.clone());
                }
            }
        }
    }
    for ev in car_events.read() {
        if let AssetEvent::Added { id } = ev {
            if car.get(*id).is_some_and(|m| m.t13.is_none()) {
                if let Some(mut m) = car.get_mut(*id) {
                    m.t13 = Some(mask.image.clone());
                }
            }
        }
    }
}

/// Track inputs: the TrackSettings light direction (once, from the post config's track folder) and the
/// mask floor from TimeOfDayA `ShadowIntensity`: c24.w = 1 − |intensity| (INFERRED: 1 by day → floor 0,
/// crosses 0 at 19:10 → no shadows at dusk).
fn track_shadow_inputs(
    mut settings: ResMut<FxShadowSettings>,
    config: Option<Res<crate::postfx::FxPostConfig>>,
    tod: Option<Res<crate::lighting::FxTimeOfDay>>,
    mut loaded: Local<bool>,
) {
    if !*loaded {
        if let Some(config) = config {
            *loaded = true;
            let xml = config.0.timeofday.parent().and_then(|d| std::fs::read_to_string(d.join("TrackSettings.xml")).ok());
            if let Some(xml) = xml {
                settings.apply_track_settings(&xml);
            }
        }
    }
    if let Some(tod) = tod {
        let floor = (1.0 - tod.tod.scalar("ShadowIntensity", tod.minutes()).abs()).clamp(0.0, 1.0);
        if settings.floor != floor {
            settings.floor = floor;
        }
    }
}

/// Shadow-casting directional lights use the game's splits; the light direction follows TrackSettings.
fn configure_light(
    mut commands: Commands,
    settings: Res<FxShadowSettings>,
    mut lights: Query<(Entity, &mut DirectionalLight, &mut CascadeShadowConfig, &mut Transform, Option<&mut SunDisk>, Option<&bevy::camera::visibility::RenderLayers>)>,
) {
    for (e, mut light, mut config, mut t, disk, layers) in &mut lights {
        // The light sees the main layer and the caster proxies' layer.
        let want = bevy::camera::visibility::RenderLayers::from_layers(&[0, CASTER_LAYER]);
        if layers.is_none_or(|l| !l.intersects(&bevy::camera::visibility::RenderLayers::layer(CASTER_LAYER))) {
            commands.entity(e).insert(layers.map_or(want.clone(), |l| l.union(&want)));
        }
        // The mask floor (CameraOriginAndShadowIntensity.w) reaches the FX shaders through the light's
        // sun-disk intensity: FH1 draws its own sky, so Bevy's atmosphere never reads it.
        match disk {
            Some(mut d) if d.intensity != settings.floor => d.intensity = settings.floor,
            Some(_) => {}
            None => {
                commands.entity(e).insert(SunDisk { intensity: settings.floor, ..SunDisk::EARTH });
            }
        }
        if light.shadow_maps_enabled != settings.enabled {
            light.shadow_maps_enabled = settings.enabled;
        }
        // The mask applies its own bias; StandardMaterial receivers sample Bevy's map with these.
        if light.shadow_depth_bias != 0.02 {
            light.shadow_depth_bias = 0.02;
            light.shadow_normal_bias = 0.6;
        }
        if config.bounds.as_slice() != &settings.far[..settings.splits] {
            config.bounds = settings.far[..settings.splits].to_vec();
            config.minimum_distance = 0.1;
            config.overlap_proportion = settings.fade;
        }
        if let Some(dir) = settings.direction {
            let want = Transform::default().looking_to(dir, if dir.abs().y > 0.99 { Vec3::Z } else { Vec3::Y }).rotation;
            if t.rotation.angle_between(want) > 1e-4 {
                t.rotation = want;
            }
        }
    }
}

/// Split i covers view depths [start_i, far_i]: it starts where split i-1 starts fading, so the min over
/// splits hands over smoothly (the game min-blends one pass per split; INFERRED).
fn split_ranges(s: &FxShadowSettings) -> [Vec4; 4] {
    std::array::from_fn(|i| {
        let far = s.far[i];
        let start = if i == 0 { 0.0 } else { s.far[i - 1] * (1.0 - s.fade) };
        let len = (far * s.fade).max(1e-3);
        // fade = saturate(lin * scale + bias): 0 until far - len, 1 at far.
        Vec4::new(start, far, 1.0 / len, -(far - len) / len)
    })
}

/// Replace Bevy's cascades for the shadow camera with the game's splits.
fn fit_cascades(
    settings: Res<FxShadowSettings>,
    mut frame: ResMut<ShadowFrame>,
    cams: Query<(Entity, &GlobalTransform, &Projection, &Camera), With<FxShadowCamera>>,
    mut lights: Query<(&GlobalTransform, &DirectionalLight, &mut Cascades)>,
    ab: Res<ShadowAb>,
    mut fit: Local<[f32; 4]>,
) {
    frame.active = false;
    let Some((cam, cam_t, proj, _)) = cams.iter().find(|c| c.3.is_active) else { return };
    let Some((light_t, light, mut cascades)) = lights.iter_mut().find(|l| l.1.shadow_maps_enabled) else { return };
    if !settings.enabled || !light.shadow_maps_enabled {
        return;
    }
    let world_from_view = cam_t.to_matrix();
    let world_from_light = Mat4::from_quat(light_t.rotation());
    let light_from_world = world_from_light.transpose();
    let ranges = split_ranges(&settings);
    let mut out = Vec::with_capacity(4);
    let mut bias = [0.0; 4];
    for i in 0..settings.splits {
        let (near, far) = (ranges[i].x.max(0.1), ranges[i].y);
        // View-space slice corners (near plane first), as Bevy's fit uses them.
        let corners = proj.get_frustum_corners(-near, -far);
        let light_from_view = light_from_world * world_from_view;
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for c in corners {
            let p = light_from_view.transform_point3(c.into());
            min = min.min(p);
            max = max.max(p);
        }
        // A view-independent size (slice body / far-plane diagonal) and texel-snapped centre keep the
        // shadows from swimming as the camera turns and moves.
        let need = (corners[0] - corners[6]).length().max((corners[4] - corners[6]).length()).ceil();
        let diameter = if fit_hysteresis() { held_diameter(&mut fit[i], need) } else { need };
        let texel = diameter / cascade_size() as f32;
        let cx = (0.5 * (min.x + max.x) / texel).floor() * texel;
        let cy = (0.5 * (min.y + max.y) / texel).floor() * texel;
        let mid_z = 0.5 * (min.z + max.z);
        // Light camera `light_distance` up-light of the split centre (the light looks down -Z).
        let near_z = mid_z + settings.light_distance[i];
        let far_z = min.z;
        let centre = Vec3::new(cx, cy, near_z);
        let world_from_cascade = Mat4::from_cols(world_from_light.x_axis, world_from_light.y_axis, world_from_light.z_axis, world_from_light * centre.extend(1.0));
        let cascade_from_world = Mat4::from_cols(light_from_world.x_axis, light_from_world.y_axis, light_from_world.z_axis, (-centre).extend(1.0));
        let r = 1.0 / (near_z - far_z);
        // Reverse-Z orthographic (Bevy's convention): depth 1 at the light camera, 0 at the far plane.
        let clip_from_cascade = Mat4::from_cols(
            Vec4::new(2.0 / diameter, 0.0, 0.0, 0.0),
            Vec4::new(0.0, 2.0 / diameter, 0.0, 0.0),
            Vec4::new(0.0, 0.0, r, 0.0),
            Vec4::new(0.0, 0.0, 1.0, 1.0),
        );
        let clip_from_world = clip_from_cascade * cascade_from_world;
        bias[i] = settings.depth_bias[i] + settings.bias_m[i] * r;
        frame.clip_from_world[i] = clip_from_world;
        out.push(Cascade { world_from_cascade, clip_from_cascade, clip_from_world, texel_size: texel });
    }
    // Bevy builds cascades for every active camera (UI scene, minimap, cube faces); cull_small_casters used to
    // empty their caster lists only after check_dir_light_mesh_visibility had frustum-tested every caster for
    // them. Empty cascades here skip that work (prepare_lights unwraps the per-view entry, so keep it).
    // Default on since P6 (FH1_SHADOW_MAINONLY=0 = old): each extra camera's 3-4 empty cascades are still shadow views
    // (a pass, preprocess bind groups and bins each) on the render thread, which is the frame now. P2's A/B (15.6
    // vs 15.7 ms) ran GPU-bound. `P6_MAIN_ONLY` overrides it for the in-run A/B (engine perf/p6.rs).
    static MAIN_ONLY: OnceLock<bool> = OnceLock::new();
    let main_only = match P6_MAIN_ONLY.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => *MAIN_ONLY.get_or_init(|| std::env::var("FH1_SHADOW_MAINONLY").map_or(true, |v| v != "0")),
    };
    if ab.main_only || main_only {
        for (view, list) in cascades.cascades.iter_mut() {
            if *view != cam {
                list.clear();
            }
        }
    }
    cascades.cascades.insert(cam, out);
    let p = proj.get_clip_from_view();
    frame.active = true;
    frame.world_from_view = world_from_view;
    frame.proj = Vec4::new(p.x_axis.x, p.y_axis.y, p.w_axis.z, 0.0);
    frame.ranges = ranges;
    frame.bias = Vec4::from_array(bias);
    frame.floor = settings.floor;
}

/// The split size depends on the FOV, which the camera effects (CameraPhysics FOV, default-on since the f7 push)
/// change every frame with speed. A fresh size every 1 m step re-snapped every split's texel grid, so all the
/// world's shadows jumped together a few times a second while accelerating or braking (P4). The size is now held
/// while it still covers the slice and isn't > 25% too big; on a change it takes a 10% margin, so the speed-FOV
/// range doesn't re-snap. `FH1_SHADOW_FIT_HOLD=0` = old (size follows the FOV).
fn fit_hysteresis() -> bool {
    static H: OnceLock<bool> = OnceLock::new();
    *H.get_or_init(|| std::env::var("FH1_SHADOW_FIT_HOLD").map_or(true, |v| v != "0"))
}

fn held_diameter(held: &mut f32, need: f32) -> f32 {
    if need > *held || need < *held * 0.8 {
        *held = (need * 1.1).ceil();
    }
    *held
}

/// `FH1_SHADOW_AB=<mode>,<mode>,...` (perf tool): cycle the modes one second each in a single run and log the
/// mean frame time per mode every 10 s, so load from other processes hits every mode alike. A mode is `off`
/// (no shadows), `mainonly` (only the shadow camera keeps cascades), `nomask` (casters drawn, mask skipped) or a min-texel count for [`cull_small_casters`]
/// (`1e9` = no casters). The first 10 s are skipped (loading).
#[derive(Resource, Default)]
struct ShadowAb {
    modes: Vec<(String, Option<f32>)>,
    current: usize,
    min_texels: Option<f32>,
    /// "nobig": drop casters with bounding radius > 50 m (diagnostic).
    no_big: bool,
    /// "mainonly": only the shadow camera keeps cascades (opt-in FH1_SHADOW_MAINONLY, see [`fit_cascades`]).
    main_only: bool,
    /// "drop:proxy" / "drop:mproxy" / "drop:masked" / "drop:other" / "drop:opaque": leave that caster kind out (diagnostic).
    drop: Option<String>,
    acc: Vec<(f64, u32)>,
    last_log: f32,
}

fn shadow_ab(mut ab: ResMut<ShadowAb>, mut settings: ResMut<FxShadowSettings>, time: Res<Time<Real>>, mut windows: Query<&mut Window>) {
    if ab.modes.is_empty() {
        return;
    }
    // Uncapped frames, or vsync hides any cost below one refresh.
    for mut w in &mut windows {
        if w.present_mode != bevy::window::PresentMode::AutoNoVsync {
            w.present_mode = bevy::window::PresentMode::AutoNoVsync;
        }
    }
    let t = time.elapsed_secs();
    if t > 10.0 {
        let k = ab.current;
        ab.acc[k].0 += time.delta_secs_f64() * 1000.0;
        ab.acc[k].1 += 1;
    }
    ab.current = (t as usize) % ab.modes.len();
    let (name, mode) = ab.modes[ab.current].clone();
    ab.min_texels = mode;
    ab.no_big = name == "nobig";
    ab.main_only = name == "mainonly";
    ab.drop = name.strip_prefix("drop:").map(str::to_string);
    if settings.enabled != (name != "off") {
        settings.enabled = name != "off";
    }
    // "nomask": casters drawn, the mask skipped (floor 1 makes `fx_shadow_mask` return lit early).
    if name == "nomask" {
        settings.floor = 1.0;
    }
    if t - ab.last_log >= 10.0 && t > 10.0 {
        ab.last_log = t;
        let line: Vec<String> = ab
            .modes
            .iter()
            .zip(&ab.acc)
            .map(|((m, _), (sum, n))| format!("{m}: {:.2} ms", sum / (*n).max(1) as f64))
            .collect();
        info!("shadow A/B: {}", line.join(", "));
    }
}

fn ab_from_env() -> ShadowAb {
    let modes: Vec<(String, Option<f32>)> = std::env::var("FH1_SHADOW_AB")
        .map(|v| v.split(',').map(|m| (m.trim().to_string(), m.trim().parse().ok())).collect())
        .unwrap_or_default();
    ShadowAb { acc: vec![(0.0, 0); modes.len()], modes, ..default() }
}

/// Drop casters too small to show in a split: a caster whose bounding radius is under `min_texels` texels
/// of the split leaves at most a speck in its map. Bevy's frustum test keeps everything in the light box
/// (1000 m up-light), so split 2 (texel ~0.5 m) otherwise draws every bollard and sign within ~200 m.
/// GUESSED rule (the game's caster selection per split is not traced); `FH1_SHADOW_MIN_TEXELS` tunes it,
/// `FH1_SHADOW_STATS=1` logs the per-split counts every 2 s.
fn cull_small_casters(
    settings: Res<FxShadowSettings>,
    mut lights: Query<(&Cascades, &mut bevy::camera::visibility::CascadesVisibleEntities)>,
    casters: Query<(&bevy::camera::primitives::Aabb, &GlobalTransform)>,
    time: Res<Time<Real>>,
    mut last_log: Local<f32>,
    ab: Res<ShadowAb>,
    fx: Query<&MeshMaterial3d<FxMaterial>>,
    fx_assets: Res<Assets<FxMaterial>>,
    kinds: Query<&MeshMaterial3d<FxCasterMaterial>>,
    caster_assets: Res<Assets<FxCasterMaterial>>,
    cams: Query<(&Camera, Option<&Name>)>,
    shadow_cams: Query<(), With<FxShadowCamera>>,
    shaders: Res<Assets<Shader>>,
    other_kinds: Query<(Has<MeshMaterial3d<StandardMaterial>>, Has<MeshMaterial3d<FxCarMaterial>>, Option<&Name>, Option<&ChildOf>)>,
    names: Query<&Name>,
) {
    static MIN_TEXELS: OnceLock<f32> = OnceLock::new();
    let min_texels = *MIN_TEXELS.get_or_init(|| std::env::var("FH1_SHADOW_MIN_TEXELS").ok().and_then(|v| v.parse().ok()).unwrap_or(settings.min_texels));
    let min_texels = ab.min_texels.unwrap_or(min_texels);
    let stats = std::env::var("FH1_SHADOW_STATS").is_ok_and(|v| v == "1") && time.elapsed_secs() - *last_log >= 2.0;
    let mut counts = [(0usize, 0usize); 4];
    // Casters per split with bounding radius > 50 m (whole zone batches).
    let mut big = [0usize; 4];
    for (cascades, mut visible) in &mut lights {
        for (view, per_split) in visible.entities.iter_mut() {
            // Bevy gives every active camera its own cascades. Only the FH1 view samples them (the UI-scene
            // and minimap cameras drew ~500 casters per split each): the others get empty maps (= lit).
            if !shadow_cams.contains(*view) {
                for list in per_split.iter_mut() {
                    list.entities.clear();
                }
                continue;
            }
            let Some(splits) = cascades.cascades.get(view) else { continue };
            for (i, (list, cascade)) in per_split.iter_mut().zip(splits).enumerate() {
                let min_r = min_texels * cascade.texel_size * texel_to_game();
                let before = list.entities.len();
                if let Some(kind) = ab.drop.as_deref() {
                    list.entities.retain(|e| {
                        let k = if let Ok(c) = kinds.get(*e) {
                            if caster_assets.get(&c.0).is_some_and(|c| c.rule != 0) { "mproxy" } else { "proxy" }
                        } else if let Some(m) = fx.get(*e).ok().and_then(|m| fx_assets.get(&m.0)) {
                            if alpha_mode(m.program, m.alpha_blend) == AlphaMode::Opaque { "opaque" } else { "masked" }
                        } else {
                            "other"
                        };
                        k != kind
                    });
                }
                if min_r > 0.0 || ab.no_big {
                    let max_r = if ab.no_big { 50.0 } else { f32::MAX };
                    list.entities.retain(|e| {
                        casters.get(*e).map_or(true, |(aabb, t)| {
                            let (s, _, _) = t.to_scale_rotation_translation();
                            let r = (Vec3::from(aabb.half_extents) * s.abs()).length();
                            r >= min_r && r <= max_r
                        })
                    });
                }
                if i < 4 {
                    counts[i].0 += before;
                    counts[i].1 += list.entities.len();
                    if stats {
                        big[i] += list
                            .entities
                            .iter()
                            .filter_map(|e| casters.get(*e).ok())
                            .filter(|(aabb, t)| (Vec3::from(aabb.half_extents) * t.to_scale_rotation_translation().0.abs()).length() > 50.0)
                            .count();
                    }
                }
            }
        }
    }
    if stats {
        // Which programs the big split-0 casters use (FH1_SHADOW_STATS=2 only).
        if std::env::var("FH1_SHADOW_STATS").is_ok_and(|v| v == "1") && std::env::var("FH1_SHADOW_BIG").is_ok() {
            let mut hist: std::collections::BTreeMap<String, usize> = default();
            for (_, visible) in &lights {
                for per_split in visible.entities.values() {
                    for e in per_split.first().map(|l| l.entities.as_slice()).unwrap_or(&[]) {
                        let Ok((aabb, t)) = casters.get(*e) else { continue };
                        let r = (Vec3::from(aabb.half_extents) * t.to_scale_rotation_translation().0.abs()).length();
                        if r <= 50.0 {
                            continue;
                        }
                        let name = fx
                            .get(*e)
                            .ok()
                            .and_then(|m| fx_assets.get(&m.0))
                            .and_then(|m| PROGRAMS.read().unwrap().get(FxKey::from(m).program as usize).map(|p| p.shader.clone()))
                            .and_then(|h| shaders.get(&h).map(|s| s.path.clone()))
                            .unwrap_or_else(|| "<not fx>".into());
                        *hist.entry(name).or_default() += 1;
                    }
                }
            }
            info!("big split-0 casters by program: {hist:?}");
        }
        // Split-2 casters by kind.
        let (mut proxy, mut mproxy, mut masked, mut opaque, mut other) = (0, 0, 0, 0, 0);
        let mut masked_mats = std::collections::HashSet::new();
        let mut other_hist: std::collections::BTreeMap<String, usize> = default();
        for (_, visible) in &lights {
            for per_split in visible.entities.values() {
                for e in per_split.last().map(|l| l.entities.as_slice()).unwrap_or(&[]) {
                    if let Ok(c) = kinds.get(*e) {
                        if caster_assets.get(&c.0).is_some_and(|c| c.rule != 0) {
                            mproxy += 1;
                            masked_mats.insert(c.0.id());
                        } else {
                            proxy += 1;
                        }
                    } else if let Some(m) = fx.get(*e).ok().and_then(|m| fx_assets.get(&m.0)) {
                        if alpha_mode(m.program, m.alpha_blend) == AlphaMode::Opaque { opaque += 1 } else { masked += 1 }
                    } else {
                        other += 1;
                        if let Ok((std, car, name, parent)) = other_kinds.get(*e) {
                            let n = name.or_else(|| parent.and_then(|p| names.get(p.parent()).ok())).map(|n| n.as_str().split(['_', ' ', '.']).next().unwrap_or("").to_string());
                            let k = format!("{}{}", if std { "std:" } else if car { "car:" } else { "?:" }, n.unwrap_or_default());
                            *other_hist.entry(k).or_insert(0usize) += 1;
                        }
                    }
                }
            }
        }
        info!("last-split other casters: {other_hist:?}");
        info!(
            "last-split casters: {proxy} proxies, {mproxy} masked proxies ({} materials), {opaque} opaque fx, {masked} masked fx, {other} other",
            masked_mats.len()
        );
        for (_, visible) in &lights {
            for (view, per_split) in visible.entities.iter() {
                let cam = cams.get(*view).ok();
                info!(
                    "  view {view:?} order {:?} active {:?} name {:?}: {:?}",
                    cam.map(|c| c.0.order),
                    cam.map(|c| c.0.is_active),
                    cam.and_then(|c| c.1.map(|n| n.to_string())),
                    per_split.iter().map(|l| l.entities.len()).collect::<Vec<_>>()
                );
            }
        }
        *last_log = time.elapsed_secs();
        let n = settings.splits.min(4);
        info!("shadow casters per split (before -> after cull): {:?}, radius > 50 m: {:?}", &counts[..n], &big[..n]);
    }
}

// ---------------------------------------------------------------- prepass / shadow pass for FxMaterial

static PREPASS_SHADER: OnceLock<Handle<Shader>> = OnceLock::new();

/// The prepass fragment shader FX materials declare. Bevy only gives a masked material's shadow pass a
/// fragment stage when the material names one, so without it the alpha kill never ran (bushes and trees
/// cast solid card blobs until 2026-10-04).
pub(crate) fn prepass_fragment_shader() -> bevy::shader::ShaderRef {
    PREPASS_SHADER.get().cloned().map_or(bevy::shader::ShaderRef::Default, bevy::shader::ShaderRef::Handle)
}

/// True when Bevy is specialising a material for its depth prepass or a shadow view.
pub(crate) fn is_prepass(descriptor: &RenderPipelineDescriptor) -> bool {
    descriptor.label.as_deref() == Some("prepass_pipeline")
}

/// FX material in Bevy's prepass / shadow pipelines: Bevy's position-only vertex stage, the game's
/// depth-pass cull and alpha kill (VERIFIED from the track .fxobj DepthOnly / ShadowDepthOnly passes):
/// - DepthOnly culls like the colour pass; ShadowDepthOnly culls the other side (CULLMODE 6 vs 2), i.e.
///   casters draw their back faces. Two-sided (cull none) effects stay two-sided.
/// - `*_MASK_*` DepthOnly PS: kill if tex(tf0, uv × c0.xy).a < 0.5; tree ShadowDepthOnly PS: kill if
///   tex(tf0, uv).a × fade < 0.1. Programs with alpha-to-coverage get the kill (see [`alpha_mode`]):
///   two-sided ones use the tree rule, the others the mask rule (GUESSED split by cull state).
pub(crate) fn specialize_prepass(descriptor: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, mesh_key: MeshPipelineKey, key: FxKey) {
    let programs = PROGRAMS.read().unwrap();
    let Some(p) = programs.get(key.program as usize) else { return };
    let st = p.state;
    let shadow = mesh_key.contains(MeshPipelineKey::UNCLIPPED_DEPTH_ORTHO);
    let two_sided = key.no_cull || st.cull == Cull::None;
    descriptor.primitive.cull_mode = if two_sided {
        None
    } else {
        // Same mapping as the colour pass (D3D CW → Back), flipped for shadow views and mirrored meshes.
        let back = (st.cull == Cull::Cw) ^ shadow ^ key.flip_cull;
        Some(if back { Face::Back } else { Face::Front })
    };
    let masked = st.alpha_to_coverage && st.blend.is_none();
    if let Some(f) = descriptor.fragment.as_mut() {
        f.shader = PREPASS_SHADER.get().unwrap().clone();
        f.entry_point = Some("fragment".into());
        if masked && layout.0.contains(Mesh::ATTRIBUTE_UV_0) {
            f.shader_defs.push("FX_ALPHA_KILL".into());
            if two_sided {
                f.shader_defs.push("FX_TREE".into());
            }
        }
    }
}

/// Alpha mode for an FX program in Bevy's terms: alpha-to-coverage programs are `Mask`, so Bevy gives their
/// prepass a fragment stage and the material bind group (for the alpha kill).
pub(crate) fn alpha_mode(program: u32, alpha_blend: bool) -> AlphaMode {
    if alpha_blend {
        return AlphaMode::Blend;
    }
    let a2c = PROGRAMS.read().unwrap().get(program as usize).is_some_and(|p| p.state.alpha_to_coverage && p.state.blend.is_none());
    if a2c {
        AlphaMode::Mask(0.5)
    } else {
        AlphaMode::Opaque
    }
}

const PREPASS_WGSL: &str = r#"
#import bevy_pbr::prepass_io::VertexOutput

struct FxShadowMaterial { vs: array<vec4<f32>, 16>, ps: array<vec4<f32>, 16>, gamma: vec4<u32>, object: array<vec4<f32>, 2> }

#ifdef FX_ALPHA_KILL
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> fx_mat: FxShadowMaterial;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var fx_t0: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var fx_s0: sampler;
#endif

@fragment
fn fragment(in: VertexOutput) {
#ifdef FX_ALPHA_KILL
#ifdef VERTEX_UVS_A
#ifdef FX_TREE
    if textureSample(fx_t0, fx_s0, in.uv).a < 0.1 {
        discard;
    }
#else
    let c0 = fx_mat.ps[0].xy;
    let scale = select(vec2<f32>(1.0), c0, c0.x != 0.0 && c0.y != 0.0);
    if textureSample(fx_t0, fx_s0, in.uv * scale).a < 0.5 {
        discard;
    }
#endif
#endif
#endif
}
"#;

// ---------------------------------------------------------------- per-fragment mask (FX materials)

/// The sampler register the game's materials read the screen shadow mask from (ShadowMaskSamp).
pub(crate) const SHADOW_MASK_TF: u32 = 13;

/// `fx_shadow_mask()`: what the game's tf13 fetch returns, computed at the fragment instead of read
/// from a screen mask (same math as `MASK_WGSL`; no depth prepass needed). Inputs from Bevy's view
/// bindings: the cascades [`fit_cascades`] wrote (`far_bound`, overlap = fade fraction), the light's
/// `shadow_depth_bias` (m along the light) and the floor in `sun_disk_intensity` (see `configure_light`).
pub(crate) const FX_SHADOW_WGSL: &str = r#"
var<private> fx_frag_pos: vec4<f32>;

fn fx_shadow_tap(t: vec2<i32>, layer: i32, z: f32, n: i32) -> f32 {
    let d = textureLoad(directional_shadow_textures, clamp(t, vec2<i32>(0), vec2<i32>(n - 1)), layer, 0);
    return select(0.0, 1.0, d <= z);
}

fn fx_shadow_pcf2x2(uv: vec2<f32>, layer: i32, z: f32, n: i32) -> f32 {
    let t = uv * f32(n) - 0.5;
    let b = vec2<i32>(floor(t));
    let f = fract(t);
    return fx_shadow_tap(b, layer, z, n) * (1.0 - f.x) * (1.0 - f.y) + fx_shadow_tap(b + vec2<i32>(1, 0), layer, z, n) * f.x * (1.0 - f.y)
        + fx_shadow_tap(b + vec2<i32>(0, 1), layer, z, n) * (1.0 - f.x) * f.y + fx_shadow_tap(b + vec2<i32>(1, 1), layer, z, n) * f.x * f.y;
}

fn fx_shadow_pcf3x3(uv: vec2<f32>, layer: i32, z: f32, n: i32) -> f32 {
    let t = uv * f32(n);
    let c = vec2<i32>(floor(t));
    let f = fract(t);
    let g = vec2<f32>(1.0) - f;
    var s = fx_shadow_tap(c, layer, z, n);
    s += g.x * fx_shadow_tap(c + vec2<i32>(-1, 0), layer, z, n) + f.x * fx_shadow_tap(c + vec2<i32>(1, 0), layer, z, n);
    s += g.y * fx_shadow_tap(c + vec2<i32>(0, -1), layer, z, n) + f.y * fx_shadow_tap(c + vec2<i32>(0, 1), layer, z, n);
    s += g.x * g.y * fx_shadow_tap(c + vec2<i32>(-1, -1), layer, z, n) + f.x * g.y * fx_shadow_tap(c + vec2<i32>(1, -1), layer, z, n);
    s += g.x * f.y * fx_shadow_tap(c + vec2<i32>(-1, 1), layer, z, n) + f.x * f.y * fx_shadow_tap(c + vec2<i32>(1, 1), layer, z, n);
    return s * 0.25;
}

fn fx_shadow_mask() -> vec4<f32> {
    return fx_shadow_mask_b(0.0);
}

// `extra`: receiver bias along the light (m) on top of the light's; car shaders pass CAR_SHADOW_BIAS.
fn fx_shadow_mask_b(extra: f32) -> vec4<f32> {
    if lights.n_directional_lights == 0u {
        return vec4<f32>(1.0, fx_headlight_yzw());
    }
    let light = &lights.directional_lights[0];
    // Floor 1 = lit whatever the maps say (dusk, and the perf A/B's "nomask").
    if ((*light).flags & 1u) == 0u || fx_frag_pos.z <= 0.0 || (*light).sun_disk_intensity >= 1.0 {
        return vec4<f32>(1.0, fx_headlight_yzw());
    }
    let ndc = vec2<f32>((fx_frag_pos.x - view.viewport.x) / view.viewport.z * 2.0 - 1.0, 1.0 - (fx_frag_pos.y - view.viewport.y) / view.viewport.w * 2.0);
    let w = view.world_from_clip * vec4<f32>(ndc, fx_frag_pos.z, 1.0);
    let world = w.xyz / w.w;
    let lin = -(view.view_from_world * vec4<f32>(world, 1.0)).z;
    let n = i32(textureDimensions(directional_shadow_textures).x);
    let fade = (*light).cascades_overlap_proportion;
    var m = 1.0;
    var prev_far = 0.0;
    for (var i = 0u; i < (*light).num_cascades; i++) {
        let far = (*light).cascades[i].far_bound;
        let start = prev_far * (1.0 - fade);
        prev_far = far;
        if lin < start || lin >= far {
            continue;
        }
        // Bias along the light: the light's own bias (m) plus half a game texel (512²) of this split (GUESSED).
        let p = world + (*light).direction_to_light * ((*light).shadow_depth_bias + extra + 0.5 * (*light).cascades[i].texel_size * f32(n) / 512.0);
        let c = (*light).cascades[i].clip_from_world * vec4<f32>(p, 1.0);
        let uv = saturate(vec2<f32>(c.x * 0.5 + 0.5, 0.5 - c.y * 0.5));
        let z = saturate(c.z);
        let layer = i32((*light).depth_texture_base_index + i);
        var lit: f32;
        if i == 0u {
            lit = fx_shadow_pcf3x3(uv, layer, z, n);
        } else {
            lit = fx_shadow_pcf2x2(uv, layer, z, n);
        }
        let len = max(far * fade, 1e-3);
        m = min(m, (*light).sun_disk_intensity + lit + saturate((lin - (far - len)) / len));
    }
    return vec4<f32>(saturate(m), fx_headlight_yzw());
}
"#;

// ---------------------------------------------------------------- mask resolve (render world)

#[derive(Resource)]
struct MaskPipelines {
    shader: Handle<Shader>,
    /// By prepass depth sample count: (layout, pipeline).
    ids: std::collections::HashMap<u32, (BindGroupLayoutDescriptor, CachedRenderPipelineId)>,
}

fn mask_layout(samples: u32) -> BindGroupLayoutDescriptor {
    let depth = if samples > 1 { texture_depth_2d_multisampled() } else { texture_depth_2d() };
    let _ = TextureSampleType::Depth;
    let entries = [
        uniform_buffer_sized(false, None).build(0, ShaderStages::FRAGMENT),
        depth.build(1, ShaderStages::FRAGMENT),
        texture_2d_array(TextureSampleType::Depth).build(2, ShaderStages::FRAGMENT),
    ];
    BindGroupLayoutDescriptor::new("fh1_shadow_mask_layout", &entries)
}

fn prepare_mask_pipelines(mut pipelines: ResMut<MaskPipelines>, cache: Res<PipelineCache>, views: Query<&ViewPrepassTextures, With<FxShadowCamera>>) {
    for prepass in &views {
        let Some(depth) = prepass.depth.as_ref() else { continue };
        let samples = depth.texture.texture.sample_count();
        if pipelines.ids.contains_key(&samples) {
            continue;
        }
        let layout = mask_layout(samples);
        let defs = if samples > 1 { vec!["MULTISAMPLED".into()] } else { vec![] };
        let id = cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("fh1_shadow_mask".into()),
            layout: vec![layout.clone()],
            vertex: VertexState { shader: pipelines.shader.clone(), shader_defs: defs.clone(), entry_point: Some("vertex".into()), buffers: vec![] },
            fragment: Some(FragmentState {
                shader: pipelines.shader.clone(),
                shader_defs: defs,
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState { format: TextureFormat::Rgba8Unorm, blend: None, write_mask: ColorWrites::ALL })],
            }),
            ..default()
        });
        pipelines.ids.insert(samples, (layout, id));
    }
}

/// Uniform layout shared with `MASK_WGSL`.
fn mask_uniform(frame: &ShadowFrame, viewport: UVec4) -> Vec<u8> {
    let mut f: Vec<f32> = Vec::with_capacity(16 * 5 + 4 * 8);
    for m in &frame.clip_from_world {
        f.extend_from_slice(&m.to_cols_array());
    }
    f.extend_from_slice(&frame.world_from_view.to_cols_array());
    f.extend_from_slice(&frame.proj.to_array());
    for r in &frame.ranges {
        f.extend_from_slice(&r.to_array());
    }
    f.extend_from_slice(&frame.bias.to_array());
    f.extend_from_slice(&[frame.floor, cascade_size() as f32, 0.0, 0.0]);
    f.extend_from_slice(&viewport.as_vec4().to_array());
    f.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[allow(clippy::too_many_arguments)]
fn shadow_mask_system(
    view: ViewQuery<(&ExtractedView, &ViewPrepassTextures, Option<&ViewShadowBindings>), With<FxShadowCamera>>,
    frame: Res<ShadowFrame>,
    mask: Res<FxShadowMask>,
    pipelines: Res<MaskPipelines>,
    cache: Res<PipelineCache>,
    images: Res<RenderAssets<GpuImage>>,
    device: Res<RenderDevice>,
    mut ctx: RenderContext,
) {
    let (extracted, prepass, shadows) = view.into_inner();
    let Some(target) = images.get(&mask.image) else { return };
    let ready = frame.active && shadows.is_some() && prepass.depth.is_some();
    // Without shadows this frame the mask is cleared to "lit".
    let draw = ready.then(|| {
        let depth = prepass.depth.as_ref().unwrap();
        let samples = depth.texture.texture.sample_count();
        let (layout, id) = pipelines.ids.get(&samples)?;
        let pipeline = cache.get_render_pipeline(*id)?;
        Some((depth, layout, pipeline))
    });
    let draw = draw.flatten();
    let bind_group = draw.as_ref().map(|(depth, layout, _)| {
        let shadows = shadows.unwrap();
        let array = shadows.directional_light_depth_texture.create_view(&TextureViewDescriptor {
            label: Some("fh1_shadow_cascades"),
            dimension: Some(TextureViewDimension::D2Array),
            ..default()
        });
        let uniform = device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("fh1_shadow_mask_consts"),
            contents: &mask_uniform(&frame, extracted.viewport),
            usage: BufferUsages::UNIFORM,
        });
        let depth_view = depth.texture.texture.create_view(&TextureViewDescriptor { label: Some("fh1_shadow_scene_depth"), ..default() });
        device.create_bind_group(
            "fh1_shadow_mask",
            &cache.get_bind_group_layout(layout),
            &[
                BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                BindGroupEntry { binding: 1, resource: BindingResource::TextureView(&depth_view) },
                BindGroupEntry { binding: 2, resource: BindingResource::TextureView(&array) },
            ],
        )
    });
    let mut pass = ctx.command_encoder().begin_render_pass(&RenderPassDescriptor {
        label: Some("fh1_shadow_mask"),
        color_attachments: &[Some(RenderPassColorAttachment {
            view: &target.texture_view,
            depth_slice: None,
            resolve_target: None,
            ops: Operations { load: LoadOp::Clear(LinearRgba::RED.into()), store: StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    if let (Some((_, _, pipeline)), Some(bg)) = (draw, bind_group.as_ref()) {
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bg, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The mask resolve: the game's ApplyShadowsPCF3x3WithFading (split 0) / ApplyShadowsPCF2x2WithFading
/// math (default.xex 0x821C4138 / 0x821C3D54, VERIFIED from the disassembly), adapted to Bevy's
/// conventions: reverse-Z infinite camera depth (the game's depth is reversed too: lin = c25.y /
/// (1 − c25.x − d)), reverse-Z light depth (so "lit if map ≥ frag" becomes "lit if map ≤ frag").
const MASK_WGSL: &str = r#"
struct ShadowConsts {
    clip_from_world: array<mat4x4<f32>, 4>,
    world_from_view: mat4x4<f32>,
    proj: vec4<f32>,
    ranges: array<vec4<f32>, 4>,
    bias: vec4<f32>,
    params: vec4<f32>,
    viewport: vec4<f32>,
}
@group(0) @binding(0) var<uniform> sc: ShadowConsts;
#ifdef MULTISAMPLED
@group(0) @binding(1) var scene_depth: texture_depth_multisampled_2d;
#else
@group(0) @binding(1) var scene_depth: texture_depth_2d;
#endif
@group(0) @binding(2) var cascades: texture_depth_2d_array;

@vertex
fn vertex(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32(vi >> 1u), f32(vi & 1u)) * 2.0;
    return vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}

fn tap(t: vec2<i32>, layer: i32, z: f32) -> f32 {
    let n = i32(sc.params.y) - 1;
    let d = textureLoad(cascades, clamp(t, vec2<i32>(0), vec2<i32>(n)), layer, 0);
    return select(0.0, 1.0, d <= z);
}

// ApplyShadowsPCF2x2WithFading: four point taps at ±0.5 texel, bilinear weights (getWeights).
fn pcf2x2(uv: vec2<f32>, layer: i32, z: f32) -> f32 {
    let t = uv * sc.params.y - 0.5;
    let b = vec2<i32>(floor(t));
    let f = fract(t);
    let s00 = tap(b, layer, z);
    let s10 = tap(b + vec2<i32>(1, 0), layer, z);
    let s01 = tap(b + vec2<i32>(0, 1), layer, z);
    let s11 = tap(b + vec2<i32>(1, 1), layer, z);
    return s00 * (1.0 - f.x) * (1.0 - f.y) + s10 * f.x * (1.0 - f.y) + s01 * (1.0 - f.x) * f.y + s11 * f.x * f.y;
}

// ApplyShadowsPCF3x3WithFading: nine point taps around the texel, tent weights from the position inside
// it (getWeights offset 0.5), total weight 4, scaled by 0.25.
fn pcf3x3(uv: vec2<f32>, layer: i32, z: f32) -> f32 {
    let t = uv * sc.params.y;
    let c = vec2<i32>(floor(t));
    let f = fract(t);
    let g = vec2<f32>(1.0) - f;
    var s = tap(c, layer, z);
    s += g.x * tap(c + vec2<i32>(-1, 0), layer, z) + f.x * tap(c + vec2<i32>(1, 0), layer, z);
    s += g.y * tap(c + vec2<i32>(0, -1), layer, z) + f.y * tap(c + vec2<i32>(0, 1), layer, z);
    s += g.x * g.y * tap(c + vec2<i32>(-1, -1), layer, z) + f.x * g.y * tap(c + vec2<i32>(1, -1), layer, z);
    s += g.x * f.y * tap(c + vec2<i32>(-1, 1), layer, z) + f.x * f.y * tap(c + vec2<i32>(1, 1), layer, z);
    return s * 0.25;
}

@fragment
fn fragment(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<i32>(pos.xy);
#ifdef MULTISAMPLED
    let d = textureLoad(scene_depth, px, 0);
#else
    let d = textureLoad(scene_depth, px, 0);
#endif
    // Sky / nothing drawn: lit.
    if d <= 0.0 {
        return vec4<f32>(1.0, 0.0, 0.0, 0.0);
    }
    let lin = sc.proj.z / d;
    let ndc = vec2<f32>((pos.x - sc.viewport.x) / sc.viewport.z * 2.0 - 1.0, 1.0 - (pos.y - sc.viewport.y) / sc.viewport.w * 2.0);
    let view_pos = vec4<f32>(ndc.x * lin / sc.proj.x, ndc.y * lin / sc.proj.y, -lin, 1.0);
    let world = sc.world_from_view * view_pos;
    var m = 1.0;
    for (var i = 0; i < 4; i++) {
        let r = sc.ranges[i];
        if lin < r.x || lin >= r.y {
            continue;
        }
        let c = sc.clip_from_world[i] * world;
        let uv = saturate(vec2<f32>(c.x * 0.5 + 0.5, 0.5 - c.y * 0.5));
        let z = saturate(c.z + sc.bias[i]);
        var lit: f32;
        if i == 0 {
            lit = pcf3x3(uv, i, z);
        } else {
            lit = pcf2x2(uv, i, z);
        }
        m = min(m, sc.params.x + lit + saturate(lin * r.z + r.w));
    }
    return vec4<f32>(saturate(m), 0.0, 0.0, 0.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_settings_direction() {
        let mut s = FxShadowSettings::default();
        s.apply_track_settings(r#"<OverrideShadowLightDirection value="1"/>
  <ShadowLightDirection x="0.750000" y="-0.650000" z="0.590000"/>"#);
        let d = s.direction.unwrap();
        assert!(d.y < 0.0 && d.z < 0.0 && d.x > 0.0);
    }

    #[test]
    fn fit_size_held_across_speed_fov() {
        let mut held = 0.0;
        assert_eq!(held_diameter(&mut held, 51.0), 57.0);
        // The speed-FOV swing (a few %) keeps the size, so the texel grid doesn't re-snap.
        for need in [52.0, 55.0, 49.0, 47.0] {
            assert_eq!(held_diameter(&mut held, need), 57.0);
        }
        // Outgrown or far too big (view switch): a new size.
        assert_eq!(held_diameter(&mut held, 60.0), 66.0);
        assert_eq!(held_diameter(&mut held, 40.0), 44.0);
    }

    #[test]
    fn ranges_hand_over() {
        let r = split_ranges(&FxShadowSettings::default());
        assert_eq!(r[0].x, 0.0);
        assert!((r[1].x - 22.5).abs() < 1e-4);
        // Fade reaches 1 at the split's far distance and 0 one fade length before it.
        assert!((r[0].y * r[0].z + r[0].w - 1.0).abs() < 1e-4);
        assert!(((r[0].y * 0.9) * r[0].z + r[0].w).abs() < 1e-4);
    }
}
