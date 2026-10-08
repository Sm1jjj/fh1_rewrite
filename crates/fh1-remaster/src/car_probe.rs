//! Remaster car reflections (W2): a light probe around the player car whose cube is captured from the scene, so paint,
//! glass and chrome reflect the festival, the road and the sky instead of only the atmosphere gradient (c3's camera env).
//!
//! - One persistent face camera, always active (layer 0 while capturing, the empty IDLE_LAYER between bakes = the main view's world incl. the game's sky parts; Hdr, no tonemapping, no
//!   bloom, no `RemasterView`, so c3's grade, main-only cascades and env bake skip it) renders one cube face per frame,
//!   centred just above the car; a render-world blit copies its HDR main texture into that cube layer.
//! - Faces are aimed so no mirror is needed: Bevy samples cubes with z negated (environment_map.wgsl), which makes the
//!   cube right-handed in world space; layer k looks along `face_basis(k)`.
//! - The near plane (FH1_RM_CAR_PROBE_NEAR, 2.4 m) keeps the car itself out of its own cube; what it clips below the car
//!   clears to a dark ground tone.
//! - After six faces a `GeneratedEnvironmentMapLight` filters the cube (diffuse + GGX mips); after a few frames its
//!   `EnvironmentMapLight` is copied onto the `LightProbe` and the generator is dropped (c3's env_refresh pattern).
//!   Units: the texels are exposed values (L x exposure), so the probe intensity is 1 / exposure = 1.2 x 2^ev100.
//! - The probe follows the car every frame (box FH1_RM_CAR_PROBE_SIZE, 14 m); a new capture starts every
//!   FH1_RM_CAR_PROBE_EVERY s (2) or after FH1_RM_CAR_PROBE_DIST m of travel (15), but never sooner than
//!   FH1_RM_CAR_PROBE_GAP s (0.25 since 2026-10-08, was 1) after the previous one. Inside the box it replaces the camera's atmosphere env.
//! - Cost (user perf logs 2026-10-07): each face is a full extra view, so a capture = 6 heavy frames (26 ms vs 18 ms at the
//!   festival); at 150 km/h the 15 m trigger kept it capturing almost all the time. The faces cull at FH1_RM_CAR_PROBE_FAR
//!   (300 m; 150 on 2026-10-07 was reverted 2026-10-08 with the face spread, the user missed the reflections; originally 1,500: Bevy's projection is infinite, `far` is only the culling plane, so the sky dome still fills the
//!   rest; at a 128 px face a 0.2 deg band below the horizon is all that changes) and the gap caps captures at four per second.
//!
//! 2026-10-08 (user: "I can see it updating visually which is jarring"):
//! - **Crossfade** (FH1_RM_CAR_PROBE_FADE s, 0.5; 0 = the old instant swap): two probe entities share the car's box; a new
//!   bake goes onto the idle one and the intensities cross over time. Bevy 0.19 blends overlapping reflection probes as
//!   sum(sample x intensity x weight) / sum(weight) (environment_map.wgsl), so both run at 2x intensity x their share
//!   while fading: the sum is exactly lerp(old, new). The box is 7 m with a 0.12 falloff so the whole car sits in the
//!   full-weight interior (with weights < 1 the double count would pulse the bumpers during a fade).
//! - **Steady pacing**: one face every FH1_RM_CAR_PROBE_SPREAD frames (2), a bake every FH1_RM_CAR_PROBE_EVERY s (1) or
//!   FH1_RM_CAR_PROBE_DIST m (10), no new capture while a fade runs: a light, even load instead of 6-face bursts.
//! - **Cheap down face**: the -Y face culls at FH1_RM_CAR_PROBE_DOWN_FAR (8 m). With the full far plane its frustum swept a
//!   600 m square under the road, so every terrain tile in it was prepared and drawn for a view of the tarmac under the
//!   car; the horizontal and up faces keep FH1_RM_CAR_PROBE_FAR.
//! - **Parallax**: Bevy auto-adds `ParallaxCorrection::Auto` to probes = the reflected world treated as the probe's own
//!   box (3 m half extents): a panel 2.3 m from the centre had its side reflections skewed by up to ~36 deg, and the skew
//!   moved as bakes changed. Now `Custom` with FH1_RM_CAR_PROBE_PARALLAX m half extents (60; 0 = Bevy's Auto): near
//!   infinity for the sky and far scenery, mild parallax for nearby walls.
//! - **Car-shaped box** (FH1_RM_CAR_PROBE_BOX = "x,y,z" m, 2.9,2.25,6.75; 0 = the axis-aligned FH1_RM_CAR_PROBE_SIZE cube):
//!   the probes turn with the car and cover the body (full weight to +-1.16 / 2.7 m with the 0.1 falloff), so the
//!   road around the car keeps the camera's env. Floor (2026-10-08, user: "a rectangle of light cast below the car"):
//!   Bevy 0.19 light probes light every mesh inside them (no per-mesh / layer opt-out), so the box starts at the body's
//!   bottom (the glTF origin = the body's bottom centre, above the road by the ride height; FH1_RM_CAR_PROBE_FLOOR m
//!   offset, 0) and rises FH1_RM_CAR_PROBE_HEIGHT m (1.9), with a 3 % vertical falloff: the road under the car is outside
//!   it. Without a loaded body the centred box (y from FH1_RM_CAR_PROBE_BOX) is used. The 7 m cube lit the ground around the car with the cube's content:
//!   at night the car's own headlight pool and tail-lamp glow showed as white pools at the box corners and red
//!   blotches (user report 2026-10-08).
//! - **Faces follow the car** (FH1_RM_CAR_PROBE_FOLLOW=0 = fixed capture point): each face is rendered from the car's
//!   current position. From a fixed point, a car moving on during the spread capture came into the later faces beyond
//!   the near plane (its own tail lamps / boot reflected on itself).
//! - **Own lights hidden from the cube** (FH1_RM_PROBE_OWN_LIGHTS=1 = old): the player car's headlights (night.rs) and
//!   backfire flash (engine backfire.rs) sit on [`OWN_LIGHT_LAYER`], which the main camera renders (light.rs) and the
//!   face camera (layer 0) does not; Bevy culls clustered lights per view by render layers (cluster assign.rs).
//!
//! - **Sleep between captures** (2026-10-08, e1's analysis of the user's 093405 log; FH1_RM_CAR_PROBE_SLEEP=0 = always
//!   active): the face camera is active only for a capture (one warm-up frame, then the faces), not during the filter,
//!   the fade or the idle time. An active camera on the empty IDLE_LAYER still ran a whole view's fixed passes every
//!   frame (main passes 2.26x per frame with the minimap). Re-activating re-queues the view's bins once per bake (the P6
//!   note below); the warm-up frame lets visibility catch up before face 0, so no face renders from a stale (empty) view.
//!
//! - **Cluster-safe slots** (2026-10-08, "Couldn't find clustered object NNNv0 in the main world" every frame): Bevy's CPU
//!   cluster assignment takes every visible `LightProbe`, with or without an env map (a bare one counts as an irradiance
//!   volume), but extraction only maps entities with `EnvironmentMapLight` / `IrradianceVolume`: the idle slot (bare
//!   between fades, and both slots before the first bake / at dusk) logged that error every frame. A slot now carries
//!   `LightProbe` only together with its env map: activated by inserting both after cluster assignment (PostUpdate:
//!   not used until the next frame's assignment), deactivated by removing both in Update (before assignment, so the
//!   cluster list never names a probe that lost its map). The crossfade follows that timing: the old probe doubles from
//!   the frame the new one is clustered (the fade loop), and the new one drops to 1x the frame after the old one is gone.
//!
//! - **Cheap faces** (2026-10-08, e4's view audit; FH1_RM_PROBE_CHEAP=0 = old): small scenery pieces render on the
//!   main-view-only layer ([`main_only_layers`]), so the faces skip them; parked, a bake comes every
//!   FH1_RM_CAR_PROBE_EVERY_PARKED s (4), easing to FH1_RM_CAR_PROBE_EVERY (1) by 10 m/s.
//!
//! - **Paired slots** (2026-10-08, user: "the car's paint flashes light/dark when moving", after the cluster-safe fix):
//!   whether a probe inserted in PostUpdate is already clustered that frame depends on system order and command flush
//!   points, so any crossfade step that adds or removes a probe could render one frame at the wrong level. Now, from the
//!   first bake on, BOTH slots are always present with a valid env map and the same weight (inserted together, removed
//!   together at dusk), and only intensities move: the active slot at 2x, the idle one at 0, crossing during a fade.
//!   sum(intensity x weight) / sum(weight) is then the same on every frame whatever the timing. A new bake replaces the
//!   idle slot's maps at intensity 0 (invisible in any frame). FH1_RM_CAR_PROBE_FADE=0 = instant swap.
//!
//! Anchor: the player car's `fh1_render::reflect::EnvCubeAnchor` (main.rs). Default on (FH1_RM_CAR_PROBE=0 = off, the camera's
//! atmosphere env only); FH1_RM_CAR_PROBE_RES=n face size (256 since 2026-10-08, was 128). The face camera gets no sun cascades: light.rs empties every
//! non-main view's entry (it must stay present, see `main_view_cascades_only`).

use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::core_pipeline::schedule::Core3d;
use bevy::core_pipeline::tonemapping::{tonemapping, Tonemapping};
use bevy::core_pipeline::Core3dSystems;
use bevy::light::{EnvironmentMapLight, GeneratedEnvironmentMapLight, LightProbe};
use bevy::prelude::*;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::texture_2d;
use bevy::render::render_resource::{
    BindGroupEntry, BindGroupLayoutDescriptor, BindingResource, CachedRenderPipelineId, ColorTargetState, ColorWrites, Extent3d,
    FragmentState, Operations, PipelineCache, RenderPassColorAttachment, RenderPassDescriptor, RenderPipelineDescriptor,
    ShaderStages, TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureViewDescriptor, TextureViewDimension,
    VertexState,
};
use bevy::render::renderer::{RenderContext, RenderDevice, ViewQuery};
use bevy::render::texture::GpuImage;
use bevy::render::view::{Msaa, ViewTarget};
use bevy::render::RenderApp;

/// The HDR main texture format (what the blit reads) = the cube format.
const FORMAT: TextureFormat = TextureFormat::Rgba16Float;
/// Render layer nothing is on: the face camera looks at it between bakes. It stays active (toggling `is_active` drops
/// the view's caches and re-specializes its meshes, PERF P6). Unique number (7 UI, 8 minimap, 27-31 taken).
const IDLE_LAYER: usize = 26;
/// Layer of the player car's own lights (module doc). Unique number (7 UI, 8 minimap, 26-31 taken).
pub const OWN_LIGHT_LAYER: usize = 25;

/// Layers for a light of the player car's own (headlight, backfire flash): `Some` while the remaster car probe runs and
/// hides them from its cube. The main camera gets `[0, OWN_LIGHT_LAYER]` in the same case (light.rs `setup_camera`).
/// Also the "main view only" layer for things the cube doesn't need (see [`main_only_layers`]).
pub fn own_light_layers() -> Option<bevy::camera::visibility::RenderLayers> {
    let on = crate::enabled() && enabled() && std::env::var("FH1_RM_PROBE_OWN_LIGHTS").map_or(true, |v| v != "1");
    on.then(|| bevy::camera::visibility::RenderLayers::layer(OWN_LIGHT_LAYER))
}

/// Layers that keep an entity out of the car probe's cube but in the main view (2026-10-08 perf, e4's view audit):
/// small scenery pieces (batch.rs) now; tyre smoke, flames, crowds, grass and particles are candidates (not wired yet). `None` = leave the entity on layer 0 (faithful renderer, probe off, FH1_RM_PROBE_CHEAP=0). The sun
/// and moon render `[0, OWN_LIGHT_LAYER]` (light.rs) so shadows from these pieces stay.
pub fn main_only_layers() -> Option<bevy::camera::visibility::RenderLayers> {
    let cheap = std::env::var("FH1_RM_PROBE_CHEAP").map_or(true, |v| v != "0");
    own_light_layers().filter(|_| cheap)
}

/// Probe box size (m) turned with the car, or None = the axis-aligned cube (module doc).
fn probe_box() -> Option<Vec3> {
    let v = std::env::var("FH1_RM_CAR_PROBE_BOX").unwrap_or_else(|_| "2.9,2.25,6.75".into());
    let p: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (p.len() == 3 && p.iter().all(|x| *x > 0.0)).then(|| Vec3::new(p[0], p[1], p[2]))
}

/// [`CarProbeFace`] value for "no face this frame".
const NO_FACE: u8 = 255;
/// Frames the generator runs before its filtered maps are taken (c3's BAKE_FRAMES).
/// 6 since 2026-10-08 (was 4): margin for the filtered maps' GPU upload under load before a crossfade starts.
const FILTER_FRAMES: u32 = 6;

/// Default on; FH1_RM_CAR_PROBE=0 = off. The 2026-10-06 panic after the first bake (bevy_pbr render/light.rs:1831,
/// `light.cascades.get(&view).unwrap()`) was light.rs removing the face camera's cascade entry once it switched to layer 0;
/// it now keeps an empty one. ("Couldn't find clustered object" is only an error log in Bevy 0.19's cluster extraction.)
pub fn enabled() -> bool {
    std::env::var("FH1_RM_CAR_PROBE").map_or(true, |v| v != "0")
}

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Face camera marker: which cube layer it renders this frame.
#[derive(Component, Clone, Copy, PartialEq, ExtractComponent)]
pub struct CarProbeFace(pub u8);

/// The captured cube (6 layers).
#[derive(Resource, Clone, ExtractResource)]
pub struct CarProbeCube(pub Handle<Image>);

/// The light probe entities around the car (two, for the crossfade): slot 0 / 1.
#[derive(Component)]
struct CarProbe(u8);

/// The temporary filter entity.
#[derive(Component)]
struct CarProbeFilter {
    frames: u32,
    intensity: f32,
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    /// Next face to render.
    Capture(u8),
    /// The generator entity is filtering.
    Filter,
}

#[derive(Resource, Default)]
struct ProbeState {
    phase: Phase,
    last_bake: Option<(f32, Vec3)>,
    /// Capture centre of the bake in progress.
    centre: Vec3,
    ev100: f32,
    /// Frames left before the next face (faces are spread out, see `face_spread`).
    gap: u32,
    /// Slot holding the current bake (None = no bake yet) and its intensity.
    active: Option<(u8, f32)>,
    /// Crossfade in progress: (incoming slot, its intensity, start time).
    fade: Option<(u8, f32, f32)>,
    /// Slots to deactivate (remove `LightProbe` + env) in Update, before cluster assignment.
    remove: Vec<Entity>,
    /// The slots' `LightProbe` falloff (inserted with each env map).
    falloff: Vec3,
}

/// Crossfade time (s); 0 = instant swap (before 2026-10-08).
fn fade_secs() -> f32 {
    env_f32("FH1_RM_CAR_PROBE_FADE", 0.5).max(0.0)
}

/// Frames per face: 1 = the six faces back to back (before 2026-10-07). Each face re-prepares and redraws the visible world
/// as a second view (+8-12 ms on the ground-facing faces in the user's festival log), so six in a row made a burst of slow
/// frames every 1.5-2 s; spread 4 (2026-10-07) cost reflection quality and did not cause the freezes, so the default is 1
/// again (2026-10-08, user). FH1_RM_CAR_PROBE_SPREAD=n.
/// 2 since 2026-10-08 with the crossfade (staleness no longer shows as a pop; half the per-frame load).
fn sleep_on() -> bool {
    std::env::var("FH1_RM_CAR_PROBE_SLEEP").map_or(true, |v| v != "0")
}

fn face_spread() -> u32 {
    (env_f32("FH1_RM_CAR_PROBE_SPREAD", 2.0) as u32).max(1)
}

/// World look direction and up of cube layer k (see the module doc).
fn face_basis(k: u8) -> (Vec3, Vec3) {
    match k {
        0 => (Vec3::X, Vec3::Y),
        1 => (Vec3::NEG_X, Vec3::Y),
        2 => (Vec3::Y, Vec3::Z),
        3 => (Vec3::NEG_Y, Vec3::NEG_Z),
        4 => (Vec3::NEG_Z, Vec3::Y),
        _ => (Vec3::Z, Vec3::Y),
    }
}

pub struct CarProbePlugin;

impl Plugin for CarProbePlugin {
    fn build(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        app.add_plugins(ExtractComponentPlugin::<CarProbeFace>::default())
            .add_plugins(ExtractResourcePlugin::<CarProbeCube>::default())
            .init_resource::<ProbeState>()
            .add_systems(Startup, setup)
            .add_systems(Update, deactivate_slots)
            .add_systems(PostUpdate, drive.after(bevy::transform::TransformSystems::Propagate));
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app.add_systems(Core3d, blit.in_set(Core3dSystems::PostProcess).before(tonemapping));
    }

    fn finish(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        let shader = app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(BLIT_WGSL, "fh1_remaster/car_probe_blit.wgsl"));
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app.insert_resource(BlitShader(shader));
        render_app.init_resource::<BlitPipeline>();
    }
}

/// Removes `LightProbe` + env from slots queued by `drive` (see the module doc: before cluster assignment).
fn deactivate_slots(mut commands: Commands, mut state: ResMut<ProbeState>) {
    for e in state.remove.drain(..) {
        commands.entity(e).try_remove::<(LightProbe, EnvironmentMapLight)>();
    }
}

fn setup(mut commands: Commands, mut images: ResMut<Assets<Image>>, mut state: ResMut<ProbeState>) {
    let size = (env_f32("FH1_RM_CAR_PROBE_RES", 256.0) as u32).next_power_of_two().clamp(32, 512);
    let mut cube = Image::new_uninit(Extent3d { width: size, height: size, depth_or_array_layers: 6 }, TextureDimension::D2, FORMAT, bevy::asset::RenderAssetUsages::RENDER_WORLD);
    cube.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::RENDER_ATTACHMENT;
    cube.texture_view_descriptor = Some(TextureViewDescriptor { dimension: Some(TextureViewDimension::Cube), ..default() });
    let cube = images.add(cube);
    commands.insert_resource(CarProbeCube(cube));
    // The camera's own output target (small, unused): the scene is taken from its main texture by the blit.
    let target = images.add(Image::new_target_texture(size, size, FORMAT, None));
    commands.spawn((
        Name::new("fh1_remaster car probe face"),
        Camera3d::default(),
        Camera { order: -30, is_active: !sleep_on(), clear_color: ClearColorConfig::Custom(Color::linear_rgb(0.05, 0.045, 0.04)), ..default() },
        RenderTarget::Image(ImageRenderTarget::from(target)),
        bevy::camera::Hdr,
        Tonemapping::None,
        Msaa::Off,
        Projection::Perspective(PerspectiveProjection {
            fov: std::f32::consts::FRAC_PI_2,
            aspect_ratio: 1.0,
            // 3.0 since 2026-10-07 (was 2.4): the car's own lamps (to ~2.3 m from its centre) bled into the cube and lit the ground.
            near: env_f32("FH1_RM_CAR_PROBE_NEAR", 3.0),
            far: env_f32("FH1_RM_CAR_PROBE_FAR", 300.0),
            ..default()
        }),
        CarProbeFace(NO_FACE),
        bevy::camera::visibility::RenderLayers::layer(IDLE_LAYER),
        Transform::default(),
    ));
    // 6 m since 2026-10-07 (was 14): the probe lights only the car and its footprint, not the road around it (a cube
    // that caught brake / head lights tinted the ground red / white until the next bake, user report). 7 m / falloff 0.12
    // since 2026-10-08: full weight out to 2.66 m so the whole car is in the interior (the crossfade needs weight 1).
    let (scale, falloff) = match probe_box() {
        // Small vertical falloff: the box floor sits just above the road (module doc).
        Some(b) => (b, Vec3::new(1.0, 0.3, 1.0) * env_f32("FH1_RM_CAR_PROBE_FALLOFF", 0.1)),
        None => (Vec3::splat(env_f32("FH1_RM_CAR_PROBE_SIZE", 7.0)), Vec3::splat(env_f32("FH1_RM_CAR_PROBE_FALLOFF", 0.12))),
    };
    let parallax = env_f32("FH1_RM_CAR_PROBE_PARALLAX", 60.0);
    state.falloff = falloff;
    for slot in 0..2u8 {
        // No `LightProbe` until the slot has an env map (module doc).
        let mut e = commands.spawn((Name::new("fh1_remaster car probe"), CarProbe(slot), Transform::from_scale(scale), Visibility::default()));
        if parallax > 0.0 {
            // Half extents in probe space (the probe's scale divides out).
            e.insert(bevy::light::ParallaxCorrection::Custom(Vec3::splat(parallax) / scale));
        }
    }
}

#[allow(clippy::type_complexity)]
fn drive(
    mut commands: Commands,
    time: Res<Time<Real>>,
    cube: Option<Res<CarProbeCube>>,
    lighting: Option<Res<crate::light::RemasterLighting>>,
    mut state: ResMut<ProbeState>,
    mut speed: Local<(Option<Vec3>, f32)>,
    anchors: Query<(&GlobalTransform, Option<&Children>), (With<fh1_render::reflect::EnvCubeAnchor>, Without<CarProbeFace>, Without<CarProbe>)>,
    bodies: Query<&Transform, (With<bevy::world_serialization::WorldAssetRoot>, Without<CarProbe>, Without<CarProbeFace>)>,
    mut face: Query<(Entity, &mut CarProbeFace, &mut bevy::camera::visibility::RenderLayers, &mut Transform, &mut GlobalTransform, &mut Projection, &mut Camera), Without<CarProbe>>,
    mut probe: Query<(Entity, &CarProbe, &mut Transform, Option<&mut EnvironmentMapLight>, &mut GlobalTransform), (Without<CarProbeFace>, Without<CarProbeFilter>)>,
    mut filters: Query<(Entity, &mut CarProbeFilter, Option<&EnvironmentMapLight>), Without<CarProbe>>,
) {
    let (Some(cube), Some((car, car_children))) = (cube, anchors.iter().next()) else { return };
    let Ok((face_e, mut f, mut layers, mut ft, mut fg, mut proj, mut cam)) = face.single_mut() else { return };
    let sleep = sleep_on();
    let idle = bevy::camera::visibility::RenderLayers::layer(IDLE_LAYER);
    let car_pos = car.translation();
    let car_rot = car.to_scale_rotation_translation().1;
    // Car speed (smoothed) for the bake interval: 4 s parked -> FH1_RM_CAR_PROBE_EVERY (1 s) from 10 m/s.
    let dt = time.delta_secs().max(1e-4);
    let v = speed.0.map_or(0.0, |p| (car_pos - p).length() / dt).min(150.0);
    speed.0 = Some(car_pos);
    speed.1 += (v - speed.1) * (1.0 - (-dt / 0.5).exp());
    let every = {
        let fast = env_f32("FH1_RM_CAR_PROBE_EVERY", 1.0);
        let slow = env_f32("FH1_RM_CAR_PROBE_EVERY_PARKED", 4.0).max(fast);
        slow + (fast - slow) * (speed.1 / 10.0).clamp(0.0, 1.0)
    };
    let boxed = probe_box();
    let now = time.elapsed_secs();
    // Car-box mode: from the body's bottom up (module doc), in the car's frame.
    let floor_box = boxed.and_then(|b| {
        let body = car_children?.iter().find_map(|c| bodies.get(c).ok())?;
        let bottom = body.transform_point(Vec3::ZERO).y + env_f32("FH1_RM_CAR_PROBE_FLOOR", 0.0);
        let h = env_f32("FH1_RM_CAR_PROBE_HEIGHT", 1.9).max(0.5);
        Some((car.transform_point(Vec3::new(0.0, bottom + 0.5 * h, 0.0)), Vec3::new(b.x, h, b.z)))
    });
    let mut slots: [Option<Entity>; 2] = [None, None];
    for (e, slot, mut pt, env, mut pg) in &mut probe {
        // The probe boxes follow the car (the cube content is a bake or two old), turned with it in car-box mode.
        match (boxed, floor_box) {
            (Some(_), Some((centre, scale))) => {
                pt.translation = centre;
                pt.rotation = car_rot;
                pt.scale = scale;
            }
            (Some(_), None) => {
                pt.translation = car_pos;
                pt.rotation = car_rot;
            }
            _ => pt.translation = car_pos,
        }
        // After propagation (PostUpdate): write the GlobalTransform the renderer extracts this frame, else the box lags
        // the car by a frame (0.7 m at 40 m/s: the bumpers in the falloff band).
        *pg = GlobalTransform::from(*pt);
        slots[(slot.0 & 1) as usize] = Some(e);
        // Crossfade intensities (module doc): both at 2x their share while fading.
        if let (Some(mut env), Some((incoming, inc_i, t0))) = (env, state.fade) {
            let a = ((now - t0) / fade_secs().max(1e-3)).clamp(0.0, 1.0);
            let want = if slot.0 == incoming {
                2.0 * inc_i * a
            } else {
                2.0 * state.active.map_or(0.0, |(_, i)| i) * (1.0 - a)
            };
            if (env.intensity - want).abs() > 1e-3 * want.abs().max(1.0) {
                env.intensity = want;
            }
        }
    }
    let [Some(slot0), Some(slot1)] = slots else { return };
    let slot_e = |k: u8| if k == 0 { slot0 } else { slot1 };
    // Commands apply before the render world extracts this frame (direct mutations too), so every step keeps
    // sum(intensity x weight) / sum(weight) at the full level in the frame it happens. The 2026-10-08 dark blips were the
    // fade start: the new probe inserted at 0 beside the old one still at 1x, averaged over two weights = half for a frame.
    if let Some((incoming, inc_i, t0)) = state.fade {
        if now - t0 >= fade_secs() {
            // Fade done: the loop above put the old slot at 0 and the new one at 2x; both stay present (module doc).
            state.active = Some((incoming, inc_i));
            state.fade = None;
        }
    }
    let ev100 = lighting.as_ref().map_or(9.7, |l| l.ev100);
    match state.phase {
        Phase::Idle => {
            f.set_if_neq(CarProbeFace(NO_FACE));
            layers.set_if_neq(idle.clone());
            let due = state.last_bake.is_none_or(|(t, p)| {
                let age = now - t;
                age >= every
                    || (age >= env_f32("FH1_RM_CAR_PROBE_GAP", 0.25) && p.distance(car_pos) > env_f32("FH1_RM_CAR_PROBE_DIST", 10.0))
            });
            // Dusk (sun sunk below the horizon by light.rs's twilight, moon not up): the face camera has no atmosphere, so
            // nothing takes the below-horizon sun's ~1e5 lx off walls facing it; the cube blew the car and the ground
            // around it to white at 20:00 (7x the faithful luma). Drop the probe's env (the camera's atmosphere env
            // applies) and skip captures until the sun or the moon is up.
            let dusk = lighting.as_ref().is_some_and(|l| l.sun_dir.y <= 0.0 && l.night < 0.5);
            if dusk {
                if state.last_bake.is_some() {
                    state.remove.extend([slot0, slot1]);
                    state.last_bake = None;
                    state.active = None;
                    state.fade = None;
                }
            } else if due && state.fade.is_none() && lighting.as_ref().is_some_and(|l| l.sun_dir != Vec3::ZERO) {
                state.centre = car_pos + Vec3::Y * 0.8;
                state.ev100 = ev100;
                state.last_bake = Some((now, car_pos));
                state.phase = Phase::Capture(0);
                if sleep {
                    // Wake: one warm-up frame (idle layer) before face 0, see the module doc.
                    if !cam.is_active {
                        cam.is_active = true;
                    }
                    state.gap = 1;
                }
                // The face sees the scene at the main view's exposure (the cube holds exposed values).
                commands.entity(face_e).insert(bevy::camera::Exposure { ev100 });
            }
        }
        Phase::Capture(k) if k < 6 && state.gap > 0 => {
            // Between faces: the face camera idles (no second world view this frame).
            state.gap -= 1;
            f.set_if_neq(CarProbeFace(NO_FACE));
            layers.set_if_neq(idle.clone());
        }
        Phase::Capture(k) if k < 6 => {
            state.gap = face_spread().saturating_sub(1);
            let (fwd, up) = face_basis(k);
            // The down face only needs the road under the car (module doc).
            if let Projection::Perspective(p) = &mut *proj {
                let far = if k == 3 { env_f32("FH1_RM_CAR_PROBE_DOWN_FAR", 8.0) } else { env_f32("FH1_RM_CAR_PROBE_FAR", 300.0) };
                if p.far != far {
                    p.far = far;
                }
            }
            // From the car's current position (module doc), else the bake's start point.
            let at = if std::env::var("FH1_RM_CAR_PROBE_FOLLOW").map_or(true, |v| v != "0") { car_pos + Vec3::Y * 0.8 } else { state.centre };
            *ft = Transform::from_translation(at).looking_to(fwd, up);
            *fg = GlobalTransform::from(*ft);
            f.set_if_neq(CarProbeFace(k));
            layers.set_if_neq(bevy::camera::visibility::RenderLayers::layer(0));
            state.phase = Phase::Capture(k + 1);
        }
        Phase::Capture(_) => {
            // All six faces rendered (the last one in the previous frame): filter.
            f.set_if_neq(CarProbeFace(NO_FACE));
            layers.set_if_neq(idle.clone());
            if sleep && cam.is_active {
                cam.is_active = false;
            }
            let intensity = 1.2 * 2f32.powf(state.ev100);
            commands.spawn((
                Name::new("fh1_remaster car probe filter"),
                GeneratedEnvironmentMapLight { environment_map: cube.0.clone(), intensity, rotation: Quat::IDENTITY, affects_lightmapped_mesh_diffuse: true },
                CarProbeFilter { frames: 0, intensity },
                Transform::default(),
            ));
            state.phase = Phase::Filter;
        }
        Phase::Filter => {
            let mut done = filters.is_empty();
            for (e, mut fl, env) in &mut filters {
                let Some(env) = env else { continue };
                fl.frames += 1;
                if fl.frames >= FILTER_FRAMES {
                    let mut env = env.clone();
                    match state.active {
                        // Both slots present: the new maps replace the idle slot's at 0, then the intensities cross.
                        Some((old, old_i)) => {
                            let incoming = 1 - old;
                            if fade_secs() > 0.0 {
                                env.intensity = 0.0;
                                commands.entity(slot_e(incoming)).insert(env);
                                state.fade = Some((incoming, fl.intensity, now));
                            } else {
                                // Instant swap: both changes land in this frame's extraction (insert flushes before it,
                                // the old one is set directly).
                                env.intensity = 2.0 * fl.intensity;
                                commands.entity(slot_e(incoming)).insert(env);
                                if let Ok((_, _, _, Some(mut old_env), _)) = probe.get_mut(slot_e(old)) {
                                    old_env.intensity = 0.0;
                                }
                                let _ = old_i;
                                state.active = Some((incoming, fl.intensity));
                            }
                        }
                        // First bake (or after dusk): both slots at once, same maps, so they enter the clusters together
                        // with equal weight: the active one at 2x, the idle one at 0.
                        None => {
                            let mut idle = env.clone();
                            idle.intensity = 0.0;
                            env.intensity = 2.0 * fl.intensity;
                            commands.entity(slot0).insert((LightProbe { falloff: state.falloff }, env));
                            commands.entity(slot1).insert((LightProbe { falloff: state.falloff }, idle));
                            state.active = Some((0, fl.intensity));
                        }
                    }
                    commands.entity(e).despawn();
                    done = true;
                }
            }
            if done {
                state.phase = Phase::Idle;
            }
        }
    }
}

// ---------------------------------------------------------------- face -> cube blit (render world)

const BLIT_WGSL: &str = r"
@group(0) @binding(0) var src: texture_2d<f32>;
struct V { @builtin(position) pos: vec4<f32> };
@vertex fn vertex(@builtin(vertex_index) i: u32) -> V {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: V;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}
// Straight copy: the faces are aimed for Bevy's z-negated cube lookup (no mirror).
@fragment fn fragment(v: V) -> @location(0) vec4<f32> {
    return textureLoad(src, vec2<i32>(v.pos.xy), 0);
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
        let layout = BindGroupLayoutDescriptor::new("fh1_remaster_car_probe_blit", &built);
        let id = world.resource::<PipelineCache>().queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("fh1_remaster_car_probe_blit".into()),
            layout: vec![layout.clone()],
            vertex: VertexState { shader: shader.clone(), shader_defs: vec![], entry_point: Some("vertex".into()), buffers: vec![] },
            fragment: Some(FragmentState {
                shader,
                shader_defs: vec![],
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState { format: FORMAT, blend: None, write_mask: ColorWrites::ALL })],
            }),
            ..default()
        });
        Self { layout, id }
    }
}

fn blit(
    view: ViewQuery<(&ViewTarget, &CarProbeFace)>,
    cube: Option<Res<CarProbeCube>>,
    pipe: Res<BlitPipeline>,
    pipeline_cache: Res<PipelineCache>,
    render_device: Res<RenderDevice>,
    images: Res<RenderAssets<GpuImage>>,
    mut ctx: RenderContext,
) {
    let (target, face) = view.into_inner();
    if face.0 >= 6 {
        return;
    }
    let (Some(cube), Some(pipeline)) = (cube, pipeline_cache.get_render_pipeline(pipe.id)) else { return };
    let Some(gpu) = images.get(&cube.0) else { return };
    let layer = gpu.texture.create_view(&TextureViewDescriptor {
        label: Some("fh1_remaster_car_probe_layer"),
        dimension: Some(TextureViewDimension::D2),
        base_array_layer: face.0 as u32,
        array_layer_count: Some(1),
        ..default()
    });
    let bind_group = render_device.create_bind_group(
        "fh1_remaster_car_probe_blit",
        &pipeline_cache.get_bind_group_layout(&pipe.layout),
        &[BindGroupEntry { binding: 0, resource: BindingResource::TextureView(target.main_texture_view()) }],
    );
    let mut rp = ctx.command_encoder().begin_render_pass(&RenderPassDescriptor {
        label: Some("fh1_remaster_car_probe_blit"),
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
