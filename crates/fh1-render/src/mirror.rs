//! The rear-view mirror render: the game's CCamRearView drawn into `RearViewMirrorTexture`, shown by the HUD mirror
//! (`I_HUD_Mirror`, CUSTREND_MIRRORRENDERER) in the hood and bumper views. Findings: docs/CAMERA.md "Rear-view mirror".
//!
//! The engine drives [`MirrorView`] each frame (camera/views.rs: `enabled` + the rear camera's world transform).
//! Cost: like the live env cube (reflect.rs), the view renders only `CUBE_LAYER`, which holds the largest scenery
//! meshes (selected by reflect.rs), plus [`MIRROR_LAYER`] for anything that asks to be seen in the mirror. It renders
//! every second frame (the game has `HalfRateMirror`) at 256×64, with no shadow cascades.
//! Env: FH1_MIRROR=1 (on; off by default), FH1_MIRROR_EVERY=n (render every n-th frame), FH1_MIRROR_ALL=1 (full scene, layer 0
//! too; for comparisons).

use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ClearColorConfig, ImageRenderTarget, RenderTarget};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;

use crate::reflect::CUBE_LAYER;

/// Extra layer for meshes that should show in the mirror besides the cube's scenery (e.g. other cars).
pub const MIRROR_LAYER: usize = 28;

/// Render-target size (4:1 like the HUD mirror's slot).
const SIZE: UVec2 = UVec2::new(256, 64);
/// Far plane: the cube's FarDistance-like range; the clear colour stands in for the sky beyond.
const FAR: f32 = 400.0;

/// The rear-view mirror render. Colour is the scene's sqrt encoding, as in the env cube faces.
#[derive(Resource, Clone)]
pub struct MirrorView {
    pub image: Handle<Image>,
    pub size: UVec2,
    /// Set by the engine each frame: draw the mirror this frame (hood/bumper view and the HUD option on).
    pub enabled: bool,
    /// World transform of the rear camera (looking along its -Z), set by the engine.
    pub transform: Transform,
    /// Vertical field of view, radians.
    pub fov: f32,
    /// Allowed at all (FH1_MIRROR != 0).
    pub allowed: bool,
}

#[derive(Component)]
pub struct MirrorCamera;

pub struct FxMirrorPlugin;

impl Plugin for FxMirrorPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup).add_systems(PostUpdate, drive.before(bevy::transform::TransformSystems::Propagate)).add_systems(
            PostUpdate,
            no_mirror_cascades
                .after(bevy::light::SimulationLightSystems::UpdateDirectionalLightCascades)
                .before(bevy::light::SimulationLightSystems::UpdateLightFrusta),
        );
    }
}

fn setup(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let image = images.add(Image::new_target_texture(SIZE.x, SIZE.y, TextureFormat::Rgba16Float, None));
    // Opt-in (FH1_MIRROR=1): the game shows no HUD mirror in free roam (Pinyon, hood/bumper refs); the HUD option and
    // the race scenarios (MaxCarsInMirror) suggest races only, which do not exist here yet.
    let allowed = std::env::var("FH1_MIRROR").is_ok_and(|v| v == "1");
    let view = MirrorView { image: image.clone(), size: SIZE, enabled: false, transform: Transform::default(), fov: 53f32.to_radians(), allowed };
    let layers = if std::env::var("FH1_MIRROR_ALL").is_ok_and(|v| v == "1") {
        RenderLayers::from_layers(&[0, CUBE_LAYER, MIRROR_LAYER])
    } else {
        RenderLayers::from_layers(&[CUBE_LAYER, MIRROR_LAYER])
    };
    commands.spawn((
        Name::new("rear-view mirror"),
        Camera3d::default(),
        Camera { order: -30, is_active: false, ..default() },
        RenderTarget::Image(ImageRenderTarget::from(image)),
        bevy::camera::Hdr,
        Tonemapping::None,
        Msaa::Off,
        Projection::Perspective(PerspectiveProjection { fov: view.fov, aspect_ratio: SIZE.x as f32 / SIZE.y as f32, near: 0.3, far: FAR, ..default() }),
        MirrorCamera,
        layers,
        Transform::default(),
    ));
    commands.insert_resource(view);
}

/// Places the mirror camera and renders it every `FH1_MIRROR_EVERY`-th frame (default 2) while enabled.
fn drive(
    view: Option<Res<MirrorView>>,
    globals: Option<Res<crate::FxGlobals>>,
    mut cam: Query<(&mut Camera, &mut Transform, &mut Projection), With<MirrorCamera>>,
    mut frame: Local<u32>,
    mut every: Local<Option<u32>>,
) {
    let (Some(view), Ok((mut c, mut t, mut proj))) = (view, cam.single_mut()) else { return };
    let every = *every.get_or_insert_with(|| std::env::var("FH1_MIRROR_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(2).max(1));
    *frame = frame.wrapping_add(1);
    let active = view.allowed && view.enabled && *frame % every == 0;
    if c.is_active != active {
        c.is_active = active;
    }
    if !active {
        return;
    }
    *t = view.transform;
    if let Projection::Perspective(p) = &mut *proj {
        p.fov = view.fov;
    }
    // Like the cube faces: clear to the TOD fog colour in the scene's sqrt encoding (stand-in for the sky).
    let fog = globals.as_ref().and_then(|g| g.get("FogColor")).map_or(Vec3::splat(0.3), |v| v.truncate());
    c.clear_color = ClearColorConfig::Custom(Color::linear_rgb(fog.x.max(0.0).sqrt(), fog.y.max(0.0).sqrt(), fog.z.max(0.0).sqrt()));
}

/// No directional shadow passes for the mirror view (as for the cube faces).
fn no_mirror_cascades(mirror: Query<Entity, With<MirrorCamera>>, mut lights: Query<&mut bevy::light::cascade::Cascades>) {
    for mut c in &mut lights {
        for m in &mirror {
            if let Some(v) = c.cascades.get_mut(&m) {
                v.clear();
            }
        }
    }
}
