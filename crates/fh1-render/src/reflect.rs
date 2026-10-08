//! Reflections: FH1's live environment cube, the static per-track car cube, the LogLuv cube decode
//! (UI tracks). The rear-view mirror render is mirror.rs. Findings and status: docs/SHADERS.md "Reflections".
//!
//! - **Live cube** (`EnvCube`): TrackSettings `<Performance DynamicCubemap="1">` on Colorado, so the
//!   game renders a cube around the focus car every frame (`<Cubemap FacesPerFrame="6"
//!   FarDistance="200">`, render targets "dynamicCubemap" / "cubemapFace (aliased)", one mip,
//!   default.xex 0x82DC3FE0). Car shaders read it through `envSampler` when `useStaticCubeMap` is
//!   false and square the fetch, so it holds the scene's sqrt-encoded colour. Here: six 90° cameras
//!   (`RenderTarget::None`, far = FarDistance) whose scene output is blitted into the cube layers.
//!   A Bevy camera frame is right-handed and a cube face is not, so the blit mirrors u.
//! - **Static cube** (`EnvCube::static_cube`): `tracks\<trk>\staticCarCubemap[<n>].xpr` (loader
//!   0x82D5EBB0), the `envStaticSampler` source. Plain colour (DXT5 alpha is 255 everywhere).
//! - **LogLuv** ([`decode_logluv`]): only the UI tracks (`imageBasedLighting LogLuvEncode="1"`).
//!
//! Env: FH1_ENVCUBE=0 (live cube off; default on, FACES=1 EVERY=2, see `setup_reflections` for the cost), FH1_ENVCUBE_IDLE=off
//! (opt-in: the face camera is switched off on frames without a face), FH1_ENVCUBE_MINR/MAXR=m, FH1_ENVCUBE_SIZE=n, FH1_ENVCUBE_SHOT=out.ppm (face strip after FH1_ENVCUBE_SHOT_AT s, default 3),
//! FH1_FRAMETIME=1 (log frame time every 2 s), FH1_ENVCUBE_AB=1 (cube on/off every 5 s in one run, logs each
//! mode's mean frame time; both modes see the same machine load).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::OnceLock;

use bevy::asset::RenderAssetUsages;
use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::core_pipeline::schedule::Core3d;
use bevy::core_pipeline::tonemapping::{tonemapping, Tonemapping};
use bevy::core_pipeline::Core3dSystems;
use bevy::prelude::*;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::texture_2d;
use bevy::render::render_resource::{
    BindGroupEntry, BindGroupLayoutDescriptor, BindingResource, CachedRenderPipelineId, ColorTargetState, ColorWrites,
    Extent3d, FragmentState, Operations, PipelineCache, RenderPassColorAttachment, RenderPassDescriptor,
    RenderPipelineDescriptor, ShaderStages, TextureDimension, TextureFormat, TextureSampleType, TextureUsages,
    TextureViewDescriptor, TextureViewDimension, VertexState,
};
use bevy::render::renderer::{RenderContext, RenderDevice, ViewQuery};
use bevy::render::texture::GpuImage;
use bevy::render::view::{Msaa, ViewTarget};
use bevy::render::RenderApp;

/// Render layer the live cube faces see. Meshes big enough for the cube (see [`tag_cube_meshes`])
/// are on layers 0 and this one; everything else stays on layer 0 only.
/// Must not clash with other cameras' layers: 7 = ui::scene::UI_LAYER (the UI camera drew the cube-only sky as grey sheets), 8 = minimap, 30 = shadow::CASTER_LAYER.
pub const CUBE_LAYER: usize = 29;

/// Layer nothing is on: the persistent face camera looks at it on frames without a face (no draws, view kept).
const CUBE_IDLE_LAYER: usize = 27;

/// [`EnvCubeCamera`] face value meaning "no face this frame" (the blit is skipped).
const NO_FACE: u8 = u8::MAX;

/// In-run A/B overrides (engine perf/p6.rs). `P6_CUBE`: 0 = env/default, 1 = keep the idle view, 2 = camera off on
/// idle frames, 3 = live cube off. `P6_CUBE_EVERY`: 0 = env/default, n = a face every n-th frame.
pub static P6_CUBE: AtomicU8 = AtomicU8::new(0);
pub static P6_CUBE_EVERY: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdleMode {
    /// Default: the persistent face camera stays active on frames without a face, looking at an empty layer (K3).
    /// An empty view still costs its whole Core3d schedule and per-view prepare work.
    Keep,
    /// Opt-in `FH1_ENVCUBE_IDLE=off` (P6): it is switched off on those frames. Bevy then drops the view's
    /// specializations and bins, so each face frame re-specializes its (<= MeshLimit) meshes. Festival in-run A/Bs:
    /// render thread -1.15 ms in one run, +0.2 in the next, frame p99 worse in both (26.8 vs 21.5, 36.2 vs 33.6 ms).
    Sleep,
    /// No faces at all (A/B only).
    Off,
}

fn idle_mode() -> IdleMode {
    static ENV: OnceLock<IdleMode> = OnceLock::new();
    match P6_CUBE.load(Ordering::Relaxed) {
        1 => IdleMode::Keep,
        2 => IdleMode::Sleep,
        3 => IdleMode::Off,
        _ => *ENV.get_or_init(|| if env_flag("FH1_ENVCUBE_IDLE").is_some_and(|v| v == "off") { IdleMode::Sleep } else { IdleMode::Keep }),
    }
}

/// The single face camera of the persistent live cube (FH1_ENVCUBE_PERSIST, default on): it stays active and is
/// re-aimed at the next face each update. Toggling six cameras' `is_active` (the old way) made Bevy 0.19 drop each
/// inactive view's material specializations, phase bins and pooled view textures (bevy_pbr material.rs
/// `retain(|view| all_views.contains(view))`), so every face activation (once per 12 frames) re-specialized and
/// re-binned up to `mesh_limit` cube meshes. Same faces, same cadence, same image.
#[derive(Component)]
struct PersistentFace;

/// Cube face format: the scene's HDR main-texture format (holds sqrt-encoded colour > 1).
const FACE_FORMAT: TextureFormat = TextureFormat::Rgba16Float;

/// Live cube edge. GUESSED: the size comes through a virtual call (vtable slot 0x823B4838) that
/// isn't traced; 128 is FM4-era typical. Override with FH1_ENVCUBE_SIZE.
pub const DEFAULT_CUBE_SIZE: u32 = 128;

/// TrackSettings.xml `<Cubemap>`, `<Mirror>`, `<Performance DynamicCubemap>`, `<screenAreaDistances>`
/// (reader 0x82D505C0; Colorado values VERIFIED from the file).
#[derive(Clone, Debug)]
pub struct CubemapSettings {
    pub dynamic: bool,
    pub mesh_size_threshold: f32,
    pub mesh_limit: u32,
    pub faces_per_frame: u32,
    pub far_distance: f32,
    pub visibility_threshold: f32,
    pub mirror_visibility_threshold: f32,
    /// screenAreaDistances: mainScene, cubeMap, mirror, depth.
    pub screen_area: [f32; 4],
}

impl Default for CubemapSettings {
    fn default() -> Self {
        // Colorado.
        Self {
            dynamic: true,
            mesh_size_threshold: 0.06,
            mesh_limit: 300,
            faces_per_frame: 6,
            far_distance: 200.0,
            visibility_threshold: 5000.0,
            mirror_visibility_threshold: 32.0,
            screen_area: [350.0, 70.0, 8.0, 60.0],
        }
    }
}

fn xml_attr(xml: &str, tag: &str, attr: &str) -> Option<f32> {
    let start = xml.find(&format!("<{tag} "))?;
    let el = &xml[start..start + xml[start..].find('>')?];
    let key = format!(" {attr}=\"");
    let a = el.find(&key)? + key.len();
    el[a..a + el[a..].find('"')?].trim().parse().ok()
}

impl CubemapSettings {
    pub fn parse(xml: &str) -> Self {
        let d = Self::default();
        let f = |tag: &str, attr: &str, v: f32| xml_attr(xml, tag, attr).unwrap_or(v);
        Self {
            dynamic: f("Performance", "DynamicCubemap", d.dynamic as u8 as f32) != 0.0,
            mesh_size_threshold: f("Cubemap", "MeshSizeThreshold", d.mesh_size_threshold),
            mesh_limit: f("Cubemap", "MeshLimit", d.mesh_limit as f32) as u32,
            faces_per_frame: f("Cubemap", "FacesPerFrame", d.faces_per_frame as f32) as u32,
            far_distance: f("Cubemap", "FarDistance", d.far_distance),
            visibility_threshold: f("Cubemap", "VisibilityThreshold", d.visibility_threshold),
            mirror_visibility_threshold: f("Mirror", "VisibilityThreshold", d.mirror_visibility_threshold),
            screen_area: [
                f("screenAreaDistances", "mainScene", d.screen_area[0]),
                f("screenAreaDistances", "cubeMap", d.screen_area[1]),
                f("screenAreaDistances", "mirror", d.screen_area[2]),
                f("screenAreaDistances", "depth", d.screen_area[3]),
            ],
        }
    }
}

/// The environment cubes for car (and other) shaders.
///
/// Binding for the car shaders (fa): `envSampler` ← `dynamic` while `use_dynamic`, with the bool
/// `useStaticCubeMap` = !use_dynamic; `envStaticSampler` ← `static_cube`. Both are sqrt-encoded
/// colour (the shaders square the fetch). Directions are engine space (the cube is rendered in it).
#[derive(Resource, Clone, ExtractResource)]
pub struct EnvCube {
    /// Live cube: Rgba16Float, 6 layers, cube view, one mip.
    pub dynamic: Handle<Image>,
    /// The track's staticCarCubemap (BC3 on Colorado), if installed.
    pub static_cube: Option<Handle<Image>>,
    pub use_dynamic: bool,
    pub size: u32,
    pub settings: CubemapSettings,
    /// Cube centre above the anchor's origin (car mesh origin = body bottom). GUESSED.
    pub height: f32,
    /// Near plane of the face cameras: keeps the focus car out of its own cube. GUESSED (the game
    /// most likely skips the focus car instead).
    pub near: f32,
    /// World-space bounding radius (m) a mesh needs to be a live-cube candidate (FH1_ENVCUBE_MINR, default 2):
    /// small props, grass and crowds never are. Candidates are then picked by `size_threshold` / `mesh_limit`.
    /// GUESSED value.
    pub min_radius: f32,
    /// Largest world bounding radius (m) drawn into the live cube (FH1_ENVCUBE_MAXR). Map-scale backdrop meshes
    /// (Colorado's far terrain, r 10-14 km) lie almost wholly beyond the faces' FarDistance (200 m), where the faces
    /// clear to the fog colour anyway; and the 14 km one, once on the cube layer, also drew a screen-fixed band of
    /// horizon in the MAIN view (VERIFIED 2026-10-04, any camera angle, 1 or 6 faces/frame; cause untraced). GUESSED value.
    pub max_radius: f32,
    /// Faces rendered per frame, round-robin (FH1_ENVCUBE_FACES, default 1; the game uses TrackSettings
    /// FacesPerFrame = 6 on Colorado).
    pub faces_per_frame: u32,
    /// Render faces only every n-th frame (FH1_ENVCUBE_EVERY). The engine's own cost knob; the game
    /// renders FacesPerFrame every frame.
    pub every: u32,
    /// Screen-size rule for meshes in the cube: bounding radius / distance from the cube centre >= this
    /// (FH1_ENVCUBE_SIZE_T; default TrackSettings MeshSizeThreshold 0.06; the game's metric is untraced, GUESSED),
    /// largest first, at most `mesh_limit` (FH1_ENVCUBE_LIMIT; default MeshLimit 300).
    pub size_threshold: f32,
    pub mesh_limit: usize,
}

/// Put on the focus car: the live cube follows it. Without one, the cube follows
/// the `FxPostCamera`.
#[derive(Component, Default)]
pub struct EnvCubeAnchor;

/// One face camera of the live cube (wgpu/D3D face order +X −X +Y −Y +Z −Z).
#[derive(Component, Clone, Copy, PartialEq, ExtractComponent)]
pub struct EnvCubeCamera(pub u8);

pub struct FxReflectPlugin;

impl Plugin for FxReflectPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractComponentPlugin::<EnvCubeCamera>::default())
            .add_plugins(ExtractResourcePlugin::<EnvCube>::default())
            .add_systems(Startup, setup_reflections)
            .add_systems(Update, reload_track_cube.run_if(on_message::<crate::postfx::FxTrackChanged>).after(crate::postfx::reload_post))
            .add_systems(PostUpdate, follow_anchor.after(bevy::transform::TransformSystems::Propagate))
            .add_systems(Update, (env_cube_shot, frame_time_log, schedule_faces))
            .add_systems(PostUpdate, (tag_cube_meshes, select_cube_meshes).chain().after(follow_anchor))
            .add_systems(
                PostUpdate,
                no_face_cascades
                    .after(bevy::light::SimulationLightSystems::UpdateDirectionalLightCascades)
                    .before(bevy::light::SimulationLightSystems::UpdateLightFrusta),
            );
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app.add_systems(Core3d, env_cube_blit.in_set(Core3dSystems::PostProcess).before(tonemapping));
    }

    fn finish(&self, app: &mut App) {
        // Shader assets live in the main world; the render world only gets the handle.
        let shader = app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(BLIT_WGSL, "fh1_env_cube_blit.wgsl"));
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app.insert_resource(BlitShader(shader));
        render_app.init_resource::<BlitPipeline>();
    }
}

fn env_flag(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Install `assets/private` (from `shaders/xex`) and the track folder (TimeOfDay.xml's: `tracks/<trk>`, or an import's
/// `imported/<id>/tracks`), from the post config.
fn track_paths(config: &crate::postfx::FxPostConfig) -> Option<(PathBuf, PathBuf)> {
    let track_dir = config.0.timeofday.parent()?.to_path_buf();
    let assets = config.0.xex_dir.parent()?.parent()?.to_path_buf();
    Some((assets, track_dir))
}

/// The track's TrackSettings cube settings and static car cube (`cars/cubemaps/<trk>.dds`; Colorado's when the track
/// has none converted, e.g. FH2's Anthem, whose `staticcarcubemap.xpr` format isn't decoded yet).
fn track_cube(config: Option<&crate::postfx::FxPostConfig>, images: &mut Assets<Image>) -> (CubemapSettings, Option<Handle<Image>>) {
    let paths = config.and_then(track_paths);
    let settings = paths
        .as_ref()
        .and_then(|(_, t)| std::fs::read_to_string(t.join("TrackSettings.xml")).ok())
        .map(|x| CubemapSettings::parse(&x))
        .unwrap_or_default();
    let static_cube = paths.as_ref().and_then(|(a, t)| {
        let name = t.file_name()?.to_string_lossy().to_ascii_lowercase();
        let cubes = a.join("cars/cubemaps");
        crate::scenery::read_dds(&cubes.join(format!("{name}.dds"))).or_else(|| crate::scenery::read_dds(&cubes.join("colorado.dds"))).map(|i| images.add(i))
    });
    (settings, static_cube)
}

/// In-process map change ([`crate::postfx::FxTrackChanged`]): the new track's static cube and cube settings.
fn reload_track_cube(config: Option<Res<crate::postfx::FxPostConfig>>, cube: Option<ResMut<EnvCube>>, mut images: ResMut<Assets<Image>>) {
    let Some(mut cube) = cube else { return };
    let (settings, static_cube) = track_cube(config.as_deref(), &mut images);
    cube.static_cube = static_cube;
    cube.settings = settings;
}

/// Face camera orientation: (forward, up) with the image's right = −(forward × up) (see the blit).
fn face_basis(k: usize) -> (Vec3, Vec3) {
    match k {
        0 => (Vec3::X, Vec3::Y),
        1 => (Vec3::NEG_X, Vec3::Y),
        2 => (Vec3::Y, Vec3::NEG_Z),
        3 => (Vec3::NEG_Y, Vec3::Z),
        4 => (Vec3::Z, Vec3::Y),
        _ => (Vec3::NEG_Z, Vec3::Y),
    }
}

fn setup_reflections(mut commands: Commands, config: Option<Res<crate::postfx::FxPostConfig>>, mut images: ResMut<Assets<Image>>) {
    let (settings, static_cube) = track_cube(config.as_deref(), &mut images);
    let size = env_flag("FH1_ENVCUBE_SIZE").and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_CUBE_SIZE).clamp(16, 2048);
    // On where the track asks for it (TrackSettings DynamicCubemap; Colorado = 1); FH1_ENVCUBE=0 = off. Default cost
    // knobs FACES=1, EVERY=2 (the game does 6 faces every frame): 2026-10-04 interleaved 45 s autodrive runs at 16:00
    // measured off 22.2 ms vs on 20.6 ms pooled (within the run noise; the old all-mesh rule cost +23 ms).
    let use_dynamic = settings.dynamic && env_flag("FH1_ENVCUBE").is_none_or(|v| v != "0");

    let mut cube = Image::new_uninit(
        Extent3d { width: size, height: size, depth_or_array_layers: 6 },
        TextureDimension::D2,
        FACE_FORMAT,
        RenderAssetUsages::RENDER_WORLD,
    );
    cube.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC;
    cube.texture_view_descriptor = Some(TextureViewDescriptor { dimension: Some(TextureViewDimension::Cube), ..default() });
    let dynamic = images.add(cube);

    let faces_per_frame = env_flag("FH1_ENVCUBE_FACES").and_then(|s| s.parse().ok()).unwrap_or(1).clamp(1, 6);
    // One persistent camera serves one face per update; FACES > 1 keeps the six cameras.
    let persist = faces_per_frame == 1 && env_flag("FH1_ENVCUBE_PERSIST").is_none_or(|v| v != "0");
    if use_dynamic {
        // FH1_K3_AB=1 (car_shadow.rs): both sets, the persistent camera (index 6 here) and the old six, used in turn.
        let ab = persist && crate::car_shadow::k3_ab();
        for k in 0..if ab { 7 } else if persist { 1 } else { 6u8 } {
            let persist = persist && (!ab || k == 6);
            // A small target the camera's output (upscaling) can write; the scene itself is taken
            // from the main texture by the blit. Also what FH1_ENVCUBE_SHOT reads back.
            let mut face = Image::new_target_texture(size, size, FACE_FORMAT, None);
            face.texture_descriptor.usage |= TextureUsages::COPY_SRC;
            let face = images.add(face);
            let cam = commands.spawn((
                Name::new(format!("env cube face {k}")),
                Camera3d::default(),
                // Default output mode: a camera with `CameraOutputMode::Skip` has no output attachment, and
                // Bevy 0.19 then drops a clearing camera before rendering (prepare_view_targets).
                Camera { order: -20 + (k % 6) as isize, is_active: persist, ..default() },
                RenderTarget::Image(ImageRenderTarget::from(face)),
                bevy::camera::Hdr,
                Tonemapping::None,
                Msaa::Off,
                Projection::Perspective(PerspectiveProjection {
                    fov: std::f32::consts::FRAC_PI_2,
                    aspect_ratio: 1.0,
                    // The focus car is not on CUBE_LAYER (its parts are below min_radius), so the faces
                    // can see the ground right under it.
                    near: 0.3,
                    far: settings.far_distance,
                    ..default()
                }),
                EnvCubeCamera(if persist { NO_FACE } else { k }),
                bevy::camera::visibility::RenderLayers::layer(if persist { CUBE_IDLE_LAYER } else { CUBE_LAYER }),
                Transform::default(),
            )).id();
            if persist {
                commands.entity(cam).insert(PersistentFace);
            }
        }
    }

    info!(
        "fh1-render: reflections: live cube {} ({size}², far {} m), static cube {}",
        if use_dynamic { "on" } else { "off" },
        settings.far_distance,
        if static_cube.is_some() { "loaded" } else { "missing" }
    );
    let min_radius = env_flag("FH1_ENVCUBE_MINR").and_then(|s| s.parse().ok()).unwrap_or(2.0);
    let max_radius = env_flag("FH1_ENVCUBE_MAXR").and_then(|s| s.parse().ok()).unwrap_or(8000.0);
    let every = env_flag("FH1_ENVCUBE_EVERY").and_then(|s| s.parse().ok()).unwrap_or(2u32).max(1);
    let size_threshold = env_flag("FH1_ENVCUBE_SIZE_T").and_then(|s| s.parse().ok()).unwrap_or(settings.mesh_size_threshold);
    let mesh_limit = env_flag("FH1_ENVCUBE_LIMIT").and_then(|s| s.parse().ok()).unwrap_or(settings.mesh_limit as usize);
    commands.insert_resource(EnvCube {
        dynamic,
        static_cube,
        use_dynamic,
        size,
        settings,
        height: 0.8,
        near: 0.3,
        faces_per_frame,
        every,
        min_radius,
        max_radius,
        size_threshold,
        mesh_limit,
    });
}

/// A mesh that may be drawn into the live cube: world bounding radius (m) and local Aabb centre.
#[derive(Component)]
pub struct CubeCandidate {
    radius: f32,
    centre: Vec3,
}

/// Marks a mesh `select_cube_meshes` put on the cube layer (it owns that entity's `RenderLayers`).
#[derive(Component)]
struct OnCubeLayer;

/// Mark newly spawned meshes whose world bounding radius is in [`min_radius`, `max_radius`] as cube
/// candidates. Entities that already carry `RenderLayers` (UI, minimap) and the anchored car are left alone.
#[allow(clippy::type_complexity)]
fn tag_cube_meshes(
    mut commands: Commands,
    names: Query<&Name>,
    cube: Option<Res<EnvCube>>,
    added: Query<(Entity, &bevy::camera::primitives::Aabb, &GlobalTransform, Option<&Name>, Option<&ChildOf>, Option<&Visibility>), (Or<(Added<Mesh3d>, Added<bevy::camera::primitives::Aabb>)>, With<Mesh3d>, Without<bevy::camera::visibility::RenderLayers>, Without<bevy::camera::visibility::NoFrustumCulling>)>,
) {
    let Some(cube) = cube.filter(|c| c.use_dynamic) else { return };
    for (e, aabb, g, name, parent, vis) in &added {
        let scale = g.to_scale_rotation_translation().0.abs().max_element();
        let r = Vec3::from(aabb.half_extents).length() * scale;
        if r >= 50.0 && env_flag("FH1_ENVCUBE_LOG").is_some() {
            let pname = parent.and_then(|p| names.get(p.parent()).ok());
            info!("env cube tag: {e} r={r:.0} vis={vis:?} name={name:?} parent={pname:?} at {:?} aabb {:?} ± {:?}", g.translation(), aabb.center, aabb.half_extents);
        }
        if r >= cube.min_radius && r <= cube.max_radius {
            commands.entity(e).insert(CubeCandidate { radius: r, centre: aabb.center.into() });
        }
    }
}

/// The game's `<Cubemap MeshSizeThreshold MeshLimit>` rule: every 8 frames, the candidates whose radius /
/// distance from the cube centre is at least `size_threshold` and that reach inside FarDistance go on the
/// cube layer, largest first, at most `mesh_limit`. Only changes touch `RenderLayers`.
#[allow(clippy::type_complexity)]
fn select_cube_meshes(
    mut commands: Commands,
    cube: Option<Res<EnvCube>>,
    faces: Query<&GlobalTransform, With<EnvCubeCamera>>,
    cands: Query<(Entity, &CubeCandidate, &GlobalTransform, Option<&bevy::camera::visibility::RenderLayers>, Has<OnCubeLayer>)>,
    parents: Query<&ChildOf>,
    anchors: Query<(), With<EnvCubeAnchor>>,
    mut frame: Local<u32>,
    mut picked: Local<Vec<(f32, Entity)>>,
    mut want: Local<bevy::platform::collections::HashSet<Entity>>,
) {
    let Some(cube) = cube.filter(|c| c.use_dynamic) else { return };
    *frame = frame.wrapping_add(1);
    if *frame % 8 != 1 {
        return;
    }
    let Some(centre) = faces.iter().next().map(|g| g.translation()) else { return };
    let far = cube.settings.far_distance;
    picked.clear();
    for (e, c, g, _, _) in &cands {
        let d = g.transform_point(c.centre).distance(centre).max(c.radius).max(1.0);
        let s = c.radius / d;
        // The focus car stays out of its own cube.
        if d - c.radius < far && s >= cube.size_threshold && !parents.iter_ancestors(e).any(|a| anchors.contains(a)) {
            picked.push((s, e));
        }
    }
    let limit = cube.mesh_limit.min(picked.len());
    if limit < picked.len() {
        picked.select_nth_unstable_by(limit, |a, b| b.0.total_cmp(&a.0));
        picked.truncate(limit);
    }
    want.clear();
    want.extend(picked.iter().map(|p| p.1));
    let both = bevy::camera::visibility::RenderLayers::from_layers(&[0, CUBE_LAYER]);
    let (mut added, mut removed) = (0, 0);
    for (e, _, _, layers, on) in &cands {
        match (want.contains(&e), on) {
            // A RenderLayers someone else set (minimap ribbons, possibly inserted after the candidate tag) wins.
            (true, false) if layers.is_none() => {
                commands.entity(e).insert((both.clone(), OnCubeLayer));
                added += 1;
            }
            (false, true) => {
                commands.entity(e).remove::<OnCubeLayer>();
                if layers == Some(&both) {
                    commands.entity(e).remove::<bevy::camera::visibility::RenderLayers>();
                }
                removed += 1;
            }
            _ => {}
        }
    }
    if env_flag("FH1_ENVCUBE_LOG").is_some() && (added > 0 || removed > 0) {
        info!("env cube meshes: {} of {} candidates (+{added} -{removed})", want.len(), cands.iter().len());
    }
}

/// The face views get no directional-light cascades: Bevy builds a full set (and shadow passes) for every
/// active camera, so each face would add 3 shadow views with FH1_SHADOWS=1. The game's cube faces
/// (RealtimeCubemap technique) read the main view's split 0 instead. Bevy's prepare_lights unwraps the
/// per-view entry, so the faces keep an empty one.
fn no_face_cascades(faces: Query<Entity, With<EnvCubeCamera>>, mut lights: Query<&mut bevy::light::cascade::Cascades>) {
    for mut c in &mut lights {
        for face in &faces {
            if let Some(v) = c.cascades.get_mut(&face) {
                v.clear();
            }
        }
    }
}

/// Activate `faces_per_frame` face cameras per frame, round-robin. Every active camera costs a full
/// view (culling, its own directional shadow cascades, the draw), so fewer faces per frame is the
/// main cost knob.
fn schedule_faces(
    cube: Option<Res<EnvCube>>,
    globals: Option<Res<crate::FxGlobals>>,
    mut faces: Query<(&mut EnvCubeCamera, &mut Camera, &mut bevy::camera::visibility::RenderLayers, Has<PersistentFace>)>,
    mut frame: Local<u32>,
    time: Res<Time<Real>>,
    mut ab: Local<CubeAb>,
) {
    let Some(cube) = cube.filter(|c| c.use_dynamic) else { return };
    let idle = bevy::camera::visibility::RenderLayers::layer(CUBE_IDLE_LAYER);
    if !ab.step(time.elapsed_secs(), time.delta_secs()) {
        for (mut face, mut cam, mut layers, persistent) in &mut faces {
            if persistent {
                // Stays active looking at nothing, so its view caches survive.
                face.set_if_neq(EnvCubeCamera(NO_FACE));
                layers.set_if_neq(idle.clone());
            } else if cam.is_active {
                cam.is_active = false;
            }
        }
        return;
    }
    // The sky dome (sky.rs, radius 300 m) follows the main camera and lies beyond the cube's FarDistance, so
    // the faces clear to the TOD fog colour (the horizon) in the scene's sqrt encoding. INFERRED stand-in
    // for the game's sky in the cube.
    let fog = globals.as_ref().and_then(|g| g.get("FogColor")).map_or(Vec3::splat(0.3), |v| v.truncate());
    let clear = ClearColorConfig::Custom(Color::linear_rgb(fog.x.max(0.0).sqrt(), fog.y.max(0.0).sqrt(), fog.z.max(0.0).sqrt()));
    let n = cube.faces_per_frame.clamp(1, 6);
    let every = match P6_CUBE_EVERY.load(Ordering::Relaxed) {
        0 => cube.every,
        e => e,
    };
    let on = *frame % every == 0;
    let first = ((*frame / every) * n) % 6;
    *frame = frame.wrapping_add(1);
    let old_only = !crate::car_shadow::K3_NEW.load(std::sync::atomic::Ordering::Relaxed);
    let has_persistent = faces.iter().any(|f| f.3);
    for (mut face, mut cam, mut layers, persistent) in &mut faces {
        if persistent && old_only {
            face.set_if_neq(EnvCubeCamera(NO_FACE));
            layers.set_if_neq(idle.clone());
            continue;
        }
        if !persistent && has_persistent && !old_only {
            if cam.is_active {
                cam.is_active = false;
            }
            continue;
        }
        if persistent {
            // One face per update (FACES is 1 here): aim at face `first` and draw the cube layer, else nothing.
            // Idle frames either keep the view (looking at an empty layer) or switch the camera off (`idle_mode`).
            let mode = idle_mode();
            let on = on && mode != IdleMode::Off;
            let (k, l) = if on { (first as u8, bevy::camera::visibility::RenderLayers::layer(CUBE_LAYER)) } else { (NO_FACE, idle.clone()) };
            face.set_if_neq(EnvCubeCamera(k));
            layers.set_if_neq(l);
            if on {
                cam.clear_color = clear;
            }
            let active = on || mode == IdleMode::Keep;
            if cam.is_active != active {
                cam.is_active = active;
            }
            continue;
        }
        let active = on && (face.0 as u32 + 6 - first) % 6 < n;
        if active {
            cam.clear_color = clear;
        }
        if cam.is_active != active {
            cam.is_active = active;
        }
    }
}

#[allow(clippy::type_complexity)]
fn follow_anchor(
    cube: Option<Res<EnvCube>>,
    anchors: Query<&GlobalTransform, (With<EnvCubeAnchor>, Without<EnvCubeCamera>)>,
    main_cam: Query<&GlobalTransform, (With<crate::post::FxPostCamera>, Without<EnvCubeCamera>)>,
    mut faces: Query<(&EnvCubeCamera, &mut Transform, &mut GlobalTransform)>,
) {
    let anchor = anchors.iter().next().or_else(|| main_cam.iter().next()).copied();
    let Some(anchor) = anchor else { return };
    if let Some(cube) = cube {
        let c = anchor.translation() + Vec3::Y * cube.height;
        for (face, mut t, mut g) in &mut faces {
            if face.0 == NO_FACE {
                continue;
            }
            let (f, u) = face_basis(face.0 as usize);
            *t = Transform::from_translation(c).looking_to(f, u);
            *g = GlobalTransform::from(*t);
        }
    }
}

// ---------------------------------------------------------------- face → cube blit (render world)

const BLIT_WGSL: &str = r"
@group(0) @binding(0) var src: texture_2d<f32>;
struct V { @builtin(position) pos: vec4<f32> };
@vertex fn vertex(@builtin(vertex_index) i: u32) -> V {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: V;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}
// Bevy cameras are right-handed, cube faces left-handed: mirror u.
@fragment fn fragment(v: V) -> @location(0) vec4<f32> {
    let d = vec2<i32>(textureDimensions(src));
    let p = vec2<i32>(v.pos.xy);
    return textureLoad(src, vec2<i32>(d.x - 1 - p.x, p.y), 0);
}
";

#[derive(Resource)]
struct BlitShader(Handle<Shader>);

#[derive(Resource)]
struct BlitPipeline {
    layout: BindGroupLayoutDescriptor,
    id: CachedRenderPipelineId,
}

impl FromWorld for BlitPipeline {
    fn from_world(world: &mut World) -> Self {
        let shader = world.resource::<BlitShader>().0.clone();
        let built = [texture_2d(TextureSampleType::Float { filterable: false }).build(0, ShaderStages::FRAGMENT)];
        let layout = BindGroupLayoutDescriptor::new("fh1_env_cube_blit", &built);
        let id = world.resource::<PipelineCache>().queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("fh1_env_cube_blit".into()),
            layout: vec![layout.clone()],
            vertex: VertexState { shader: shader.clone(), shader_defs: vec![], entry_point: Some("vertex".into()), buffers: vec![] },
            fragment: Some(FragmentState {
                shader,
                shader_defs: vec![],
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState { format: FACE_FORMAT, blend: None, write_mask: ColorWrites::ALL })],
            }),
            ..default()
        });
        Self { layout, id }
    }
}

fn env_cube_blit(
    view: ViewQuery<(&ViewTarget, &EnvCubeCamera)>,
    cube: Option<Res<EnvCube>>,
    pipe: Res<BlitPipeline>,
    pipeline_cache: Res<PipelineCache>,
    render_device: Res<RenderDevice>,
    images: Res<RenderAssets<GpuImage>>,
    mut ctx: RenderContext,
) {
    let (target, face) = view.into_inner();
    if face.0 == NO_FACE {
        return;
    }
    let (Some(cube), Some(pipeline)) = (cube, pipeline_cache.get_render_pipeline(pipe.id)) else { return };
    let Some(gpu) = images.get(&cube.dynamic) else { return };
    let layer = gpu.texture.create_view(&TextureViewDescriptor {
        label: Some("fh1_env_cube_layer"),
        dimension: Some(TextureViewDimension::D2),
        base_array_layer: face.0 as u32,
        array_layer_count: Some(1),
        ..default()
    });
    let bind_group = render_device.create_bind_group(
        "fh1_env_cube_blit",
        &pipeline_cache.get_bind_group_layout(&pipe.layout),
        &[BindGroupEntry { binding: 0, resource: BindingResource::TextureView(target.main_texture_view()) }],
    );
    let mut rp = ctx.command_encoder().begin_render_pass(&RenderPassDescriptor {
        label: Some("fh1_env_cube_blit"),
        color_attachments: &[Some(RenderPassColorAttachment { view: &layer, depth_slice: None, resolve_target: None, ops: Operations::default() })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, &bind_group, &[]);
    rp.draw(0..3, 0..1);
}

// ---------------------------------------------------------------- debug: cube readback

/// FH1_ENVCUBE_SHOT=out.ppm: after FH1_ENVCUBE_SHOT_AT s (default 3), read the cube back and save the six faces as a strip
/// (+X −X +Y −Y +Z −Z, sqrt-encoded colour clamped to 0..1).
fn env_cube_shot(mut commands: Commands, cube: Option<Res<EnvCube>>, time: Res<Time<Real>>, mut done: Local<bool>) {
    let (Some(cube), Some(path)) = (cube, env_flag("FH1_ENVCUBE_SHOT")) else { return };
    let at = env_flag("FH1_ENVCUBE_SHOT_AT").and_then(|s| s.parse().ok()).unwrap_or(3.0);
    if *done || time.elapsed_secs() < at {
        return;
    }
    *done = true;
    let n = cube.size as usize;
    commands.spawn(Readback::texture(cube.dynamic.clone())).observe(move |ev: On<ReadbackComplete>, mut commands: Commands| {
        let px = |o: usize| -> [u8; 3] {
            let h = |i: usize| half_to_f32(u16::from_le_bytes([ev.data[o + 2 * i], ev.data[o + 2 * i + 1]]));
            [0, 1, 2].map(|c| (h(c).clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        };
        // Layers are tightly packed (rows padded to 256 bytes).
        let row = (n * 8).div_ceil(256) * 256;
        let mut ppm = format!("P6\n{} {}\n255\n", n * 6, n).into_bytes();
        for y in 0..n {
            for f in 0..6 {
                for x in 0..n {
                    let o = (f * n + y) * row + x * 8;
                    ppm.extend_from_slice(&if o + 8 <= ev.data.len() { px(o) } else { [255, 0, 255] });
                }
            }
        }
        match std::fs::write(&path, ppm) {
            Ok(()) => info!("fh1-render: env cube saved to {path}"),
            Err(e) => warn!("fh1-render: env cube shot failed: {e}"),
        }
        commands.entity(ev.entity).despawn();
    });
}

fn half_to_f32(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1f) as i32;
    let m = (h & 0x3ff) as f32;
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// FH1_FRAMETIME=1: log the mean/max frame time every 2 s.
/// FH1_ENVCUBE_AB=1: alternate the live cube on/off every 5 s; logs per-mode mean frame times (first 10 s dropped).
#[derive(Default)]
struct CubeAb {
    /// (sum ms, frames) for off / on.
    acc: [(f64, u32); 2],
    last_log: f32,
}

impl CubeAb {
    /// Is the cube on this frame?
    fn step(&mut self, t: f32, dt: f32) -> bool {
        if env_flag("FH1_ENVCUBE_AB").is_none_or(|v| v == "0") {
            return true;
        }
        let on = (t / 5.0) as u32 % 2 == 0;
        // Skip the first frame of each half (the switch frame) and the loading period.
        if t > 10.0 && (t % 5.0) > 0.25 {
            let a = &mut self.acc[on as usize];
            a.0 += dt as f64 * 1000.0;
            a.1 += 1;
        }
        if t - self.last_log >= 20.0 {
            self.last_log = t;
            let m = |a: (f64, u32)| if a.1 > 0 { a.0 / a.1 as f64 } else { 0.0 };
            info!("env cube A/B: off {:.2} ms ({} frames), on {:.2} ms ({} frames)", m(self.acc[0]), self.acc[0].1, m(self.acc[1]), self.acc[1].1);
        }
        on
    }
}

fn frame_time_log(time: Res<Time<Real>>, mut acc: Local<(f32, f32, u32, f32)>) {
    if env_flag("FH1_FRAMETIME").is_none() {
        return;
    }
    let dt = time.delta_secs() * 1000.0;
    acc.0 += dt;
    acc.1 = acc.1.max(dt);
    acc.2 += 1;
    acc.3 += time.delta_secs();
    if acc.3 >= 2.0 {
        info!("frame time: mean {:.2} ms, max {:.2} ms over {} frames", acc.0 / acc.2 as f32, acc.1, acc.2);
        *acc = (0.0, 0.0, 0, 0.0);
    }
}

// ---------------------------------------------------------------- LogLuv (UI tracks)

/// Decode one LogLuv texel to linear RGB, exactly as the game's BlitLightingCubeFaceFromCubeMap_LogLuv
/// (PS 0x821B4698) does (VERIFIED: constants from the shader; the installed DDS channel order
/// [u, v, Le hi, Le lo] gives a clean HDR showroom, no negative texels).
/// Note the shader's blue row uses −5.772 for Y (the textbook inverse matrix has −0.572); the
/// game's value is kept, it decodes grey to grey with the game's encode.
pub fn logluv_to_rgb(t: [u8; 4]) -> [f32; 3] {
    let [u, v, hi, lo] = t.map(|b| b as f32 / 255.0);
    let le = hi * 255.0 + lo;
    let y = ((le - 127.0) * 0.5).exp2();
    let z = y / v.max(1e-6);
    let x = u * z;
    [6.0013 * x - 1.332 * y + 0.3007 * z, -2.7 * x + 3.1029 * y - 1.088 * z, -1.7995 * x - 5.772 * y + 5.6268 * z]
}

/// Decode an installed LogLuv cube (Rgba8 DDS, e.g. `cars/cubemaps/uiautoshow.dds`) to an
/// Rgba16Float image with the same layout (faces × mips), in the game's linear colour.
pub fn decode_logluv(path: &Path) -> Option<Image> {
    let src = crate::scenery::read_dds(path)?;
    if !matches!(src.texture_descriptor.format, TextureFormat::Rgba8Unorm | TextureFormat::Rgba8UnormSrgb) {
        return None;
    }
    let data = src.data.as_ref()?;
    let mut out = Vec::with_capacity(data.len() * 2);
    for p in data.chunks_exact(4) {
        let c = logluv_to_rgb([p[0], p[1], p[2], p[3]]);
        for v in [c[0], c[1], c[2], 1.0] {
            out.extend_from_slice(&f32_to_half(v).to_le_bytes());
        }
    }
    let mut img = src.clone();
    img.texture_descriptor.format = TextureFormat::Rgba16Float;
    img.data = Some(out);
    Some(img)
}

fn f32_to_half(v: f32) -> u16 {
    let b = v.to_bits();
    let s = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let m = b & 0x7f_ffff;
    if e <= 0 {
        return s;
    }
    if e >= 31 {
        return s | 0x7c00;
    }
    s | ((e as u16) << 10) | ((m + 0x1000) >> 13).min(0x3ff) as u16
}
