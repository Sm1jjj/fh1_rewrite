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
//! - **Even cadence** (2026-10-08 pm, P8-B; FH1_RM_PROBE_EVEN=0 = the bursts above): the user felt micro-stutter, a camera
//!   log showed alternating 13-23 ms frames ~every 0.12 s. With spread 2 a whole-world face came every other frame and a
//!   bake every 0.25 s at speed. Now the face camera stays awake and renders ONE face EVERY frame, round-robin
//!   (+X -X +Y -Y +Z -Z), into a rolling cube, so each frame carries the same small extra view instead of alternating
//!   heavy / light frames. A bake is only the filter + crossfade of that cube (no capture phase); bakes keep the speed-based
//!   interval (GraphicsQuality / FH1_RM_CAR_PROBE_EVERY*). Faces cull at FH1_RM_CAR_PROBE_FAR (150 m in this mode; 300 in
//!   the old one), the down face at 8 m; small scenery stays on the main-only layer. The face exposure follows the
//!   lighting's EV each frame it changes by > 0.05.
//!
//! - **Time of day** (2026-10-08 late, user: "at night the car goes like a metallic green colour and when dusk hits some
//!   weird flashing happens"):
//!   - Green: the faces drew the MAIN view's game sky. Its horizon colours are green at night and dusk in the TOD data
//!     (Colorado AtmosphereBottomColour 0.0134/0.0168/0.0152 at night, AtmosphereHazeColour 0.37/0.57/0.47 at 19:00),
//!     while the game's own reflection cube draws the env-cube sky pass with the Cube* haze channels (blue 0.10/0.12/0.18
//!     at night, orange at dusk; fh1-render sky/clouds.rs `cube_consts`). The car reflected a green horizon all round.
//!     Now the faces render `SkyPart::CubeClouds` (CUBE_LAYER) and the main sky parts sit on the main-view-only layer
//!     (FH1_RM_PROBE_CUBE_SKY=0 = old). Also the clear colour (the down face is all clear: the road is inside the near
//!     plane) was a fixed exposed value, ~8x the moonlit road and brown at night; it now follows the exposed ground level
//!     ([`face_clear`], FH1_RM_PROBE_CLEAR_EV=0 = old).
//!   - Dusk flashes: the face camera had no atmosphere, so the sun reached its walls unattenuated: during the sunset sink
//!     (light.rs twilight: up to 117,000 lx exo-atmospheric at 1-5 degrees, where the main view's transmittance passes a
//!     few %) each bake lit the car far brighter than the scene, then the probe was dropped as the sun crossed the horizon
//!     and came back at the moon swap. The face camera now carries the main view's `AtmosphereSettings` (transmittance,
//!     aerial perspective, sky; FH1_RM_PROBE_ATMOSPHERE=0 = old), so the probe keeps baking through dusk (the drop only
//!     applies to the old path). Cost: the atmosphere LUTs for the face view on the frames it renders (asleep otherwise).
//!
//! - **Static-only faces** (P13 micro-stutter, 2026-10-08; FH1_RM_PROBE_STATIC_ONLY=0 = old): the P12 static world draws
//!   the scenery into each face itself (GPU-culled, static_world/draw.rs keys on `CarProbeFace`), so a face now renders
//!   the ECS world only on [`PROBE_LAYER`] (sun / moon, sky parts) plus the env-cube sky. Before, every face view also took
//!   every layer-0 ECS mesh (cars, traffic, crowds, rides: ~4k entities): visibility, queue and batching for a second view,
//!   and on each capture's wake the re-specialisation of all of them. At speed a capture starts every ~0.3 s (the 10 m
//!   trigger), so faces made a heavy frame every other frame for most of a drive: the micro-stutter the user felt. The cube
//!   loses other cars / crowds / rides (64-256 px, behind a crossfade). While the lamps are on the faces take layer 0 as
//!   before, so street lights (point lights on layer 0) still light the cube's scenery.
//!
//! - **Hot-spot cap** (2026-10-08 late, user: orange metallic paint "hit by acidic light"; FH1_RM_PROBE_CLAMP=0 = old): the
//!   face -> cube blit scales texels above 4 game display units down to that level (hue kept). With the face atmosphere
//!   the sun disk landed in the cube (~1e4-1e5 exposed units in a texel or two), doubling the analytic sun highlight and
//!   smeared by the GGX / irradiance filter into blotches over the panels; see [`probe_clamp`].
//!
//! - **P16-B** (2026-10-09, docs/PERF_P16_B.md; flag = old): the blit makes every face texel finite before the hot-spot cap
//!   (the sun disk overflowed f16 to +inf, the cap made it NaN, the filters scattered NaN texels = black dots on the flake
//!   paint; [`probe_finite_on`], FH1_RM_PROBE_FINITE). **Gap frames off**: the face camera is inactive between faces
//!   instead of rendering an empty view ([`gap_sleep_on`], FH1_RM_PROBE_GAP_SLEEP). **Filter once**: Bevy's generator is
//!   removed after one complete run instead of re-filtering every FILTER_FRAMES frame ([`filter_once_on`],
//!   FH1_RM_PROBE_FILTER_ONCE). [`set_paused`] = the in-run A/B mode `probeoff` (views.rs).
//!
//! Anchor: the player car's `fh1_render::reflect::EnvCubeAnchor` (main.rs). OFF by default since 2026-10-08 (user: the best
//! run so far was without it, ~80 fps; FH1_RM_CAR_PROBE=1 = on, the camera's atmosphere env only otherwise; before: on, 0 = off, the camera's
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
/// Generator runs of the car cube's filter seen by the render world since the current filter entity spawned
/// ([`count_generation`]; reset by `drive`).
static GEN_RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// FH1_RM_PROBE_FILTER_ONCE=0 = old (P16-B): Bevy's environment-map generator runs the whole filter (SPD downsample, one
/// GGX pass per mip, irradiance, ~12 fresh bind groups + 11 uniform buffers) on EVERY frame its entity exists
/// (bevy_pbr light_probe/generate.rs: extract -> prepare bind groups -> downsampling_system / filtering_system, no "done"
/// state), so the 6 FILTER_FRAMES of each bake ran it ~4 times (the first frames wait for the output `GpuImage`s). The
/// output is the same each time (same source cube). Now the generator component is removed from the filter entity once
/// the render world has seen one complete run for the car cube ([`count_generation`]: its bind groups present and all
/// five compute pipelines compiled, so a first bake during pipeline compilation still waits); SyncComponent then drops the
/// render-side `RenderEnvironmentMap`. The filter entity keeps its `EnvironmentMapLight` (the written maps) until
/// FILTER_FRAMES as before, so the crossfade timing is unchanged.
fn filter_once_on() -> bool {
    std::env::var("FH1_RM_PROBE_FILTER_ONCE").map_or(true, |v| v != "0")
}

/// FH1_RM_PROBE_GAP_SLEEP=0 = old (P16-B): with spread 2 the frames between faces kept the face camera active on the empty
/// IDLE_LAYER, i.e. a whole extra Core3d view (view uniforms / bind groups, preprocess, the atmosphere LUT passes the face
/// camera carries, empty phases, sky) for nothing, 5 per bake. Now the camera is inactive on those frames. P6 kept it
/// awake because a wake re-specialised every layer-0 mesh in the view; the static-only faces see only PROBE_LAYER (sun /
/// moon, sky parts), so a wake is cheap. Inactive, `check_visibility` skips the camera and its `VisibleEntities` keep the
/// last face's set (bevy_camera visibility/mod.rs), which is why the gap frames keep the capture layers. Only with
/// [`static_faces_on`] and the lamps off (layer-0 faces = the old P6 case); the wake's warm-up frame is unchanged.
fn gap_sleep_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_RM_PROBE_GAP_SLEEP").map_or(true, |v| v != "0")) && static_faces_on() && !LAMPS_ON.load(std::sync::atomic::Ordering::Relaxed)
}

/// In-run A/B (views.rs `FH1_VIEWS_AB=probeoff`): the probe stops capturing and its two slots are dropped, i.e. the
/// render-thread / GPU state of FH1_RM_CAR_PROBE=0 inside the same run.
static PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// See [`PAUSED`]. No-op when the probe isn't built.
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, std::sync::atomic::Ordering::Relaxed);
}

/// ON by default again since 2026-10-08 late night (user: the P15 build was "the smoothest run", GPU 7.6 -> 5.0 ms, so the
/// probe fits; FH1_RM_CAR_PROBE=0 = off). It was off for the 200 fps push earlier that day. The 2026-10-06 panic after the
/// first bake (bevy_pbr render/light.rs:1831, `light.cascades.get(&view).unwrap()`) was light.rs removing the face camera's
/// cascade entry once it switched to layer 0; it now keeps an empty one. ("Couldn't find clustered object" is only an error
/// log in Bevy 0.19's cluster extraction.)
pub fn enabled() -> bool {
    !std::env::var("FH1_RM_CAR_PROBE").is_ok_and(|v| v == "0")
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
    /// Generator removed ([`filter_once_on`]).
    stopped: bool,
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
    /// Face camera target and the current face size (resized by the quality preset).
    target: Handle<Image>,
    size: u32,
    /// Even cadence: next face to render, faces rendered since the last bake, exposure the faces render at.
    next_face: u8,
    faces_since_bake: u32,
    face_ev: Option<f32>,
    /// Parked rest: seconds stood still, faces left in a rest refresh cycle.
    rest_t: f32,
    rest_cycle: u8,
    /// [`set_paused`] state applied.
    paused: bool,
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

/// Lean faces (P8-B, log 20261008_140912: the every-frame face cost ~4 ms, transparent pass +1.0 ms, opaque +0.5, queue
/// 1.1): the cube sees only the big opaque world (either cadence). FH1_RM_PROBE_LEAN=0 = the old face content.
pub fn lean_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Independent of the cadence (the even cadence is opt-in since d24cb7a; the bursts carry the lean content too).
    *V.get_or_init(|| std::env::var("FH1_RM_PROBE_LEAN").map_or(true, |v| v != "0"))
}

/// Layers for an entity the lean faces skip (transparent scenery, blended game-shader parts, small props): the main
/// view's own layer only. `None` when the probe or the lean rule is off.
pub fn probe_skip_layers() -> Option<bevy::camera::visibility::RenderLayers> {
    main_only_layers().filter(|_| lean_on())
}

/// World half-diagonal (m) below which a scenery piece / merged prop chunk stays out of the lean faces
/// (FH1_RM_PROBE_MIN_RADIUS, 4).
pub fn probe_min_radius() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_RM_PROBE_MIN_RADIUS").ok().and_then(|v| v.parse().ok()).unwrap_or(4.0))
}

/// Face size cap of the even cadence (FH1_RM_PROBE_EVEN_RES, 128): the quality preset's probe_res, at most this.
fn even_res_cap() -> u32 {
    std::env::var("FH1_RM_PROBE_EVEN_RES").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(64).next_power_of_two().clamp(32, 512)
}

/// FH1_RM_PROBE_EVEN=1 = one face every frame (module doc "Even cadence"). OPT-IN since 2026-10-08 pm: log 140912 fps 67 -> 56
/// (a second 3D view every frame); default = the capture bursts.
fn even_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // P11 (2026-10-08 late): lean content, 64 px faces and the parked rest make the per-frame face small and steady (the
    // bursts' alternating heavy frames + camera-wake re-specialisation were the micro-stutter at speed). Still OPT-IN
    // (FH1_RM_PROBE_EVEN=1) until a user log measures its steady cost (the user reverted the 128 px version for fps).
    *V.get_or_init(|| std::env::var("FH1_RM_PROBE_EVEN").is_ok_and(|v| v == "1"))
}

/// FH1_RM_PROBE_REST=0: the even cadence keeps rendering faces while parked.
fn rest_on() -> bool {
    std::env::var("FH1_RM_PROBE_REST").map_or(true, |v| v != "0")
}

/// FH1_RM_PROBE_ATMOSPHERE=0 = old (module doc "Time of day"): the face camera gets the main view's Bevy atmosphere.
pub fn probe_atmosphere_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| crate::light::atmosphere_on() && std::env::var("FH1_RM_PROBE_ATMOSPHERE").map_or(true, |v| v != "0"))
}

/// FH1_RM_PROBE_CUBE_SKY=0 = old (module doc "Time of day"): the faces draw the game's env-cube sky pass (fh1-render
/// `SkyPart::CubeClouds` on `CUBE_LAYER`) instead of the main view's sky parts. Needs the main-view-only layer
/// ([`own_light_layers`]); fh1-render sky.rs mirrors this switch to spawn the pass.
pub fn cube_sky_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| own_light_layers().is_some() && std::env::var("FH1_RM_PROBE_CUBE_SKY").map_or(true, |v| v != "0"))
}

/// FH1_RM_PROBE_CLEAR_EV=0 = old (module doc "Time of day"): the faces' clear colour follows the scene's exposed ground level.
fn clear_ev_on() -> bool {
    std::env::var("FH1_RM_PROBE_CLEAR_EV").map_or(true, |v| v != "0")
}

/// Dark ground tone the faces clear to (exposed units, tuned by day).
const CLEAR: Vec3 = Vec3::new(0.05, 0.045, 0.04);

/// The faces' clear colour for the current lighting: [`CLEAR`] scaled by the scene's exposed ground level relative to
/// daylight. The texels are exposed values while the exposure is compressed (light.rs EV_COMPRESS), so the scene's
/// exposed brightness falls ~25x from day to night; a fixed clear made the cube's floor (all of the down face: the road
/// under the car is inside the near plane) a bright brown at night, ~8x the moonlit road, and it lit the car from below.
fn face_clear(lighting: Option<&crate::light::RemasterLighting>) -> ClearColorConfig {
    let k = match lighting {
        // ground / (1.2 pi 2^ev) = 2^(ev_phys - ev): 1 at the daylight exposure, ~0.04 at night, ~0.02 in blue hour.
        Some(l) if clear_ev_on() && l.ev100.is_finite() && l.ground_lux > 0.0 => {
            (l.ground_lux / (1.2 * std::f32::consts::PI * 2f32.powf(l.ev100)) / 0.9).clamp(0.0, 1.0)
        }
        _ => 1.0,
    };
    let c = CLEAR * k;
    ClearColorConfig::Custom(Color::linear_rgb(c.x, c.y, c.z))
}

/// Layers a face renders while capturing: the world, plus the env-cube sky with [`cube_sky_on`]. With [`static_faces_on`]
/// (lamps off) the "world" is [`PROBE_LAYER`] instead of layer 0: the static world draws the scenery into the face by
/// itself, so no layer-0 ECS mesh joins the face view (module doc "Static-only faces").
fn capture_layers() -> bevy::camera::visibility::RenderLayers {
    let world = if static_faces_on() && !LAMPS_ON.load(std::sync::atomic::Ordering::Relaxed) { PROBE_LAYER } else { 0 };
    if cube_sky_on() {
        bevy::camera::visibility::RenderLayers::from_layers(&[world, fh1_render::reflect::CUBE_LAYER])
    } else {
        bevy::camera::visibility::RenderLayers::layer(world)
    }
}

/// What the static-only faces see of the ECS world (module doc "Static-only faces"): the sun / moon and, without
/// [`cube_sky_on`], the main view's sky parts. Unique number (7 UI, 8 minimap, 9 world map, 23 thumbs, 24 graphics
/// blit, 25-31 taken).
pub const PROBE_LAYER: usize = 22;

/// Lamps on (TOD SwitchOnLights or night), set by [`static_faces_state`]: the faces take layer 0 again so the street
/// lights (point lights on layer 0) still light the cube's scenery.
static LAMPS_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Static-only faces (P13 micro-stutter, 2026-10-08; module doc). Needs the static world's probe draw and the main-only
/// light layers (the sun / moon then carry [`PROBE_LAYER`], light.rs `main_only_light_layers`).
/// FH1_RM_PROBE_STATIC_ONLY=0 = old (faces take every layer-0 ECS mesh).
pub fn static_faces_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        crate::static_world::probe_faces_on()
            && main_only_layers().is_some()
            && std::env::var("FH1_RM_PROBE_STATIC_ONLY").map_or(true, |v| v != "0")
    })
}

/// Static-only faces: the lamps state for [`capture_layers`], and (without [`cube_sky_on`]) the sky parts on
/// `[0, PROBE_LAYER]` so the faces keep the sky (the main view still sees them on layer 0).
#[allow(clippy::type_complexity)]
fn static_faces_state(
    mut commands: Commands,
    lighting: Option<Res<crate::light::RemasterLighting>>,
    sky: Query<Entity, (With<fh1_render::sky::SkyPart>, Without<bevy::camera::visibility::RenderLayers>)>,
) {
    if !static_faces_on() {
        return;
    }
    let lamps = lighting.as_ref().is_some_and(|l| l.lights_on > 0.05 || l.night >= 0.5);
    LAMPS_ON.store(lamps, std::sync::atomic::Ordering::Relaxed);
    if cube_sky_on() {
        return;
    }
    for e in &sky {
        commands.entity(e).try_insert(bevy::camera::visibility::RenderLayers::from_layers(&[0, PROBE_LAYER]));
    }
}

/// With [`cube_sky_on`]: the main view's sky parts (dome, fog sky, sun, moon, stars, clouds) go on the main-view-only
/// layer, so the faces see only the env-cube pass (the game's own reflection sky, as its live cube: reflect.rs).
#[allow(clippy::type_complexity)]
fn probe_sky_layers(
    mut commands: Commands,
    new: Query<(Entity, &fh1_render::sky::SkyPart), (Added<fh1_render::sky::SkyPart>, Without<bevy::camera::visibility::RenderLayers>)>,
) {
    if !cube_sky_on() {
        return;
    }
    let Some(layers) = own_light_layers() else { return };
    let fog_sky = probe_fog_sky_on();
    for (e, part) in &new {
        match part {
            fh1_render::sky::SkyPart::CubeClouds => {}
            // Also in the faces (CUBE_LAYER): see [`probe_fog_sky_on`].
            fh1_render::sky::SkyPart::FogSky if fog_sky => {
                commands.entity(e).try_insert(layers.clone().with(fh1_render::reflect::CUBE_LAYER));
            }
            _ => {
                commands.entity(e).try_insert(layers.clone());
            }
        }
    }
}

/// FH1_RM_PROBE_FOG_SKY=0 = old (the fog sky band in the main view only). The game's dome colours below ~40 degrees are a
/// lerp of AtmosphereBottomColour (no blue) and AtmosphereTopColour (no red), h^ColourPower with ColourPower ~0.054
/// (docs/SHADERS.md "Atmosphere"): at Colorado 20:00 that is (0.20, 0.26, 0.14) display units at the horizon and
/// (1.0, 1.1, 0.13) on the ring below it, i.e. a bright green band, ~20x the exposed scene. The main view never shows it:
/// the fog sky part lays FogColour over -3.8..9.5 degrees (density saturated at every TOD) and the far terrain covers
/// the rest. The faces cull at FH1_RM_CAR_PROBE_FAR (300 m) and had no fog sky, so that band went all round the cube and
/// the car's side panels (whose reflections look just below the horizon) and roof edges came out green at dusk / night
/// (crash-check shot 20:00, 2026-10-08). The fog sky now draws in the faces as in the main view.
fn probe_fog_sky_on() -> bool {
    std::env::var("FH1_RM_PROBE_FOG_SKY").map_or(true, |v| v != "0")
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
            .add_systems(Update, (deactivate_slots, probe_sky_layers, static_faces_state.after(probe_sky_layers)))
            .add_systems(PostUpdate, drive.after(bevy::transform::TransformSystems::Propagate));
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app.add_systems(Core3d, blit.in_set(Core3dSystems::PostProcess).before(tonemapping));
        render_app.add_systems(
            bevy::render::Render,
            count_generation
                .after(bevy::pbr::generate::filtering_system)
                .after(bevy::render::RenderSystems::PrepareBindGroups)
                .before(bevy::render::RenderSystems::Render),
        );
    }

    fn finish(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        let wgsl = BLIT_WGSL
            .replace("const CAP: f32 = 0.0;", &format!("const CAP: f32 = {:?};", probe_clamp()))
            .replace("const FINITE: bool = false;", &format!("const FINITE: bool = {};", probe_finite_on()));
        let shader = app.world_mut().resource_mut::<Assets<Shader>>().add(Shader::from_wgsl(wgsl, "fh1_remaster/car_probe_blit.wgsl"));
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
    // Even cadence: a face every frame, so a small fixed face (FH1_RM_CAR_PROBE_RES, when set, wins).
    let size = if even_on() && std::env::var("FH1_RM_CAR_PROBE_RES").is_err() { size.min(even_res_cap()) } else { size };
    let mut cube = Image::new_uninit(Extent3d { width: size, height: size, depth_or_array_layers: 6 }, TextureDimension::D2, FORMAT, bevy::asset::RenderAssetUsages::RENDER_WORLD);
    cube.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::RENDER_ATTACHMENT;
    cube.texture_view_descriptor = Some(TextureViewDescriptor { dimension: Some(TextureViewDimension::Cube), ..default() });
    let cube = images.add(cube);
    commands.insert_resource(CarProbeCube(cube));
    // The camera's own output target (small, unused): the scene is taken from its main texture by the blit.
    let target = images.add(Image::new_target_texture(size, size, FORMAT, None));
    state.target = target.clone();
    state.size = size;
    let mut face = commands.spawn((
        Name::new("fh1_remaster car probe face"),
        Camera3d::default(),
        Camera { order: -30, is_active: !sleep_on(), clear_color: ClearColorConfig::Custom(Color::linear_rgb(CLEAR.x, CLEAR.y, CLEAR.z)), ..default() },
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
    // The main view's atmosphere (module doc "Time of day"): sun / moon transmittance, aerial perspective and sky.
    if probe_atmosphere_on() {
        face.insert(crate::light::remaster_atmosphere_settings());
    }
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
    quality: Option<Res<fh1_render::quality::GraphicsQuality>>,
    mut images: ResMut<Assets<Image>>,
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
    // Options > Graphics > Quality (fh1-render quality.rs): probe_interval_s = the minimum gap between bakes (High 0.25 =
    // FH1_RM_CAR_PROBE_GAP's old default); the moving / parked intervals scale with it (High 1 / 4 s = the old defaults).
    // The FH1_RM_CAR_PROBE_* variables, when set, win.
    let q_gap = quality.as_ref().map_or(0.25, |q| q.probe_interval_s.max(0.05));
    let every = {
        let fast = env_f32("FH1_RM_CAR_PROBE_EVERY", 4.0 * q_gap);
        let slow = env_f32("FH1_RM_CAR_PROBE_EVERY_PARKED", 16.0 * q_gap).max(fast);
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
    // In-run A/B ([`set_paused`]): no face view, no probes, no filter = the probe-off state; resumes with a fresh bake.
    let paused = PAUSED.load(std::sync::atomic::Ordering::Relaxed);
    if paused != state.paused {
        state.paused = paused;
        if paused {
            if state.active.is_some() || state.fade.is_some() {
                state.remove.extend([slot0, slot1]);
            }
            for (e, _, _) in &filters {
                commands.entity(e).despawn();
            }
            state.phase = Phase::Idle;
            state.last_bake = None;
            state.active = None;
            state.fade = None;
            state.faces_since_bake = 0;
            state.face_ev = None;
            f.set_if_neq(CarProbeFace(NO_FACE));
            layers.set_if_neq(idle.clone());
            if cam.is_active {
                cam.is_active = false;
            }
        } else if !sleep {
            // The always-awake modes expect an active camera.
            cam.is_active = true;
        }
    }
    if paused {
        return;
    }
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
    let even = even_on();
    match state.phase {
        Phase::Idle => {
            // Even cadence: the face block after the match owns the face / layers (no per-frame flip-flop).
            if !even {
                f.set_if_neq(CarProbeFace(NO_FACE));
                layers.set_if_neq(idle.clone());
            }
            let due = state.last_bake.is_none_or(|(t, p)| {
                let age = now - t;
                age >= every
                    || (age >= env_f32("FH1_RM_CAR_PROBE_GAP", q_gap) && p.distance(car_pos) > env_f32("FH1_RM_CAR_PROBE_DIST", 10.0))
            });
            // Face size from the preset (FH1_RM_CAR_PROBE_RES wins): resized in place between bakes (no fade running), so
            // the cube / target handles and every bind group naming them stay; the next bake fills the new size.
            if let (Some(q), Err(_), None) = (quality.as_ref(), std::env::var("FH1_RM_CAR_PROBE_RES"), state.fade) {
                let want = q.probe_res.next_power_of_two().clamp(32, 512);
                let want = if even_on() { want.min(even_res_cap()) } else { want };
                if want != state.size {
                    state.size = want;
                    if let Some(mut c) = images.get_mut(&cube.0) {
                        c.resize(Extent3d { width: want, height: want, depth_or_array_layers: 6 });
                    }
                    if let Some(mut t) = images.get_mut(&state.target) {
                        t.resize(Extent3d { width: want, height: want, depth_or_array_layers: 1 });
                    }
                }
            }
            // Dusk (sun sunk below the horizon by light.rs's twilight, moon not up): the face camera has no atmosphere, so
            // nothing takes the below-horizon sun's ~1e5 lx off walls facing it; the cube blew the car and the ground
            // around it to white at 20:00 (7x the faithful luma). Drop the probe's env (the camera's atmosphere env
            // applies) and skip captures until the sun or the moon is up.
            // With the face camera's atmosphere (FH1_RM_PROBE_ATMOSPHERE) the sunk sun's direct light is zero in the faces as
            // in the main view, so the probe keeps baking through dusk: no pop to the camera env and back at the moon swap.
            let dusk = !probe_atmosphere_on() && lighting.as_ref().is_some_and(|l| l.sun_dir.y <= 0.0 && l.night < 0.5);
            if dusk {
                if state.last_bake.is_some() {
                    state.remove.extend([slot0, slot1]);
                    state.last_bake = None;
                    state.active = None;
                    state.fade = None;
                }
            } else if even && due && state.fade.is_none() && state.faces_since_bake >= 6 {
                // Even cadence: the rolling cube is current (all six faces since the last bake): filter it now.
                state.ev100 = ev100;
                state.last_bake = Some((now, car_pos));
                state.faces_since_bake = 0;
                state.gap = 0;
                state.phase = Phase::Capture(6);
            } else if !even && due && state.fade.is_none() && lighting.as_ref().is_some_and(|l| l.sun_dir != Vec3::ZERO) {
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
                cam.clear_color = face_clear(lighting.as_deref());
            }
        }
        Phase::Capture(k) if k < 6 && state.gap > 0 => {
            // Between faces: the face camera idles (no second world view this frame).
            state.gap -= 1;
            f.set_if_neq(CarProbeFace(NO_FACE));
            if k > 0 && gap_sleep_on() {
                // P16-B: no view at all between faces (module doc "Gap frames off"). The wake's warm-up (k == 0) stays.
                if cam.is_active {
                    cam.is_active = false;
                }
            } else {
                layers.set_if_neq(idle.clone());
            }
        }
        Phase::Capture(k) if k < 6 => {
            if !cam.is_active {
                cam.is_active = true;
            }
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
            layers.set_if_neq(capture_layers());
            state.phase = Phase::Capture(k + 1);
        }
        Phase::Capture(_) => {
            // All six faces rendered (the last one in the previous frame): filter.
            if !even {
                f.set_if_neq(CarProbeFace(NO_FACE));
                layers.set_if_neq(idle.clone());
            }
            if sleep && !even && cam.is_active {
                cam.is_active = false;
            }
            let intensity = 1.2 * 2f32.powf(state.ev100);
            GEN_RUNS.store(0, std::sync::atomic::Ordering::Release);
            commands.spawn((
                Name::new("fh1_remaster car probe filter"),
                GeneratedEnvironmentMapLight { environment_map: cube.0.clone(), intensity, rotation: Quat::IDENTITY, affects_lightmapped_mesh_diffuse: true },
                CarProbeFilter { frames: 0, intensity, stopped: false },
                Transform::default(),
            ));
            state.phase = Phase::Filter;
        }
        Phase::Filter => {
            let mut done = filters.is_empty();
            for (e, mut fl, env) in &mut filters {
                let Some(env) = env else { continue };
                fl.frames += 1;
                if !fl.stopped && filter_once_on() && GEN_RUNS.load(std::sync::atomic::Ordering::Acquire) > 0 {
                    // P16-B: the maps are written; stop Bevy re-running the whole generation every frame (module doc).
                    fl.stopped = true;
                    commands.entity(e).try_remove::<GeneratedEnvironmentMapLight>();
                }
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

    // ---- even cadence: one face every frame (module doc) ----
    if even {
        let lit = lighting.as_ref().is_some_and(|l| l.sun_dir != Vec3::ZERO && (probe_atmosphere_on() || !(l.sun_dir.y <= 0.0 && l.night < 0.5)));
        if !lit {
            f.set_if_neq(CarProbeFace(NO_FACE));
            layers.set_if_neq(idle.clone());
            if sleep && cam.is_active {
                cam.is_active = false;
            }
            state.faces_since_bake = 0;
            return;
        }
        if !cam.is_active {
            // Wake: this frame on the idle layer so visibility catches up before the first face (module doc).
            cam.is_active = true;
            f.set_if_neq(CarProbeFace(NO_FACE));
            layers.set_if_neq(idle.clone());
            return;
        }
        // Parked rest: once the cube is complete and the car has stood for 2 s, no faces (the camera stays awake on the empty
        // layer, so resuming doesn't re-specialise); one 6-face refresh every FH1_RM_PROBE_REST_REFRESH s (10) for the
        // time of day. FH1_RM_PROBE_REST=0 = faces every frame while parked too.
        if speed.1 < 0.5 {
            state.rest_t += dt;
        } else {
            state.rest_t = 0.0;
            state.rest_cycle = 0;
        }
        if rest_on() && state.rest_t > 2.0 && state.faces_since_bake >= 6 && state.rest_cycle == 0 {
            if state.rest_t < env_f32("FH1_RM_PROBE_REST_REFRESH", 10.0).max(2.5) {
                f.set_if_neq(CarProbeFace(NO_FACE));
                layers.set_if_neq(idle.clone());
                return;
            }
            state.rest_t = 2.0;
            state.rest_cycle = 6;
        }
        state.rest_cycle = state.rest_cycle.saturating_sub(1);
        if state.face_ev.is_none_or(|e| (e - ev100).abs() > 0.05) {
            state.face_ev = Some(ev100);
            commands.entity(face_e).insert(bevy::camera::Exposure { ev100 });
            cam.clear_color = face_clear(lighting.as_deref());
        }
        let k = state.next_face % 6;
        state.next_face = (k + 1) % 6;
        state.faces_since_bake += 1;
        let (fwd, up) = face_basis(k);
        if let Projection::Perspective(p) = &mut *proj {
            let far = if k == 3 { env_f32("FH1_RM_CAR_PROBE_DOWN_FAR", 8.0) } else { env_f32("FH1_RM_CAR_PROBE_FAR", 80.0) };
            if p.far != far {
                p.far = far;
            }
        }
        *ft = Transform::from_translation(car_pos + Vec3::Y * 0.8).looking_to(fwd, up);
        *fg = GlobalTransform::from(*ft);
        f.set_if_neq(CarProbeFace(k));
        layers.set_if_neq(capture_layers());
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
// Copy: the faces are aimed for Bevy's z-negated cube lookup (no mirror). Texels brighter than CAP (exposed units; 0 = no
// cap) are scaled down to it, keeping their hue ([`probe_clamp`]). FINITE: non-finite texels are made finite first
// ([`probe_finite_on`]): +inf (f16 overflow, the sun disk) -> the f16 max, NaN -> 0, by bit pattern (fast-math safe).
const CAP: f32 = 0.0;
const FINITE: bool = false;
fn finite(x: f32) -> f32 {
    let b = bitcast<u32>(x);
    if (b & 0x7f800000u) != 0x7f800000u {
        return x;
    }
    // Exponent all ones: inf (mantissa 0) or NaN.
    return select(0.0, select(65504.0, -65504.0, (b & 0x80000000u) != 0u), (b & 0x007fffffu) == 0u);
}
@fragment fn fragment(v: V) -> @location(0) vec4<f32> {
    var c = textureLoad(src, vec2<i32>(v.pos.xy), 0);
    if FINITE {
        c = vec4<f32>(finite(c.r), finite(c.g), finite(c.b), 1.0);
    }
    c = max(c, vec4<f32>(0.0));
    let m = max(c.r, max(c.g, c.b));
    if CAP > 0.0 && m > CAP {
        return vec4<f32>(c.rgb * (CAP / m), c.a);
    }
    return c;
}
";

/// Brightest texel the cube keeps (exposed units, 0 = no cap); FH1_RM_PROBE_CLAMP = the cap in game display units (4; 0 =
/// old, uncapped). 2026-10-08, user: orange metallic paint "hit by acidic light" (LAM_LP7004_12, late light):
/// - Since the faces carry the main view's `AtmosphereSettings` (module doc "Time of day"), Bevy's sky pass draws the sun
///   disk into every face whose pixel is still at the far plane (the game's sky parts are drawn at z = 0, so they do not
///   hide it): `SunDisk::EARTH` = sun illuminance / 6.6e-5 sr, ~1e4-1e5 exposed units in one or two texels of a 128-256 px
///   face. The main camera's env map has no sun (Bevy's atmosphere env bake samples the sky-view LUT only), and the paint
///   already gets the sun's highlight from the directional light, so the cube doubled it, and the GGX / irradiance filter
///   smeared that hot spot into large blotches on every panel facing it, tinted by the paint's metallic F0 (orange ->
///   per-channel clip in the curve -> yellow).
/// - 4 display units = above the brightest cube sky the TOD makes (CubeAtmosphereHaze x SkyGain 2: ~2-3) and the capped
///   lamps (FH1_RM_LAMP_CAP 3), far below the sun disk; the sun's highlight stays the analytic one, like the main env.
fn probe_clamp() -> f32 {
    let display = env_f32("FH1_RM_PROBE_CLAMP", 4.0).max(0.0);
    display * crate::post::game_unit_scale()
}

/// FH1_RM_PROBE_FINITE=0 = old (P16-B, 2026-10-09, user: "subtle black dots on the car" since the probe went back on):
/// the faces' atmosphere draws the sun disk at ~1e4-1e5 exposed units, past Rgba16Float's 65504, so its texel is +inf in
/// the face's main texture; the hot-spot cap then made it inf x (CAP / inf) = NaN in the cube. Bevy's SPD downsample
/// carries that NaN up the mip chain (one texel per level, over the sun), and the GGX / irradiance filters, sampling those
/// levels, scatter NaN texels over the sun-facing half of the filtered maps. The flake-jittered paint normals
/// (car_paint.rs) hit them at random: isolated black (NaN) pixels on the paint only, densest on the sky-facing roof; smooth
/// glass samples one direction and stays clean. Now the blit makes every texel finite before the cap.
fn probe_finite_on() -> bool {
    std::env::var("FH1_RM_PROBE_FINITE").map_or(true, |v| v != "0")
}

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

/// Render world, after Bevy's generator systems: counts a complete generator run for the car cube ([`filter_once_on`]).
/// Bevy's downsampling / filtering systems run for every entity with `GeneratorBindGroups` once all five pipelines are
/// compiled, and silently skip the frame otherwise. Other generators (the camera's atmosphere env) have another source.
fn count_generation(
    gens: Query<&bevy::pbr::generate::RenderEnvironmentMap, With<bevy::pbr::generate::GeneratorBindGroups>>,
    cube: Option<Res<CarProbeCube>>,
    images: Res<RenderAssets<GpuImage>>,
    pipelines: Option<Res<bevy::pbr::generate::GeneratorPipelines>>,
    pipeline_cache: Res<PipelineCache>,
) {
    let (Some(cube), Some(p)) = (cube, pipelines) else { return };
    let Some(src) = images.get(&cube.0) else { return };
    let ready = [p.downsample_first, p.downsample_second, p.copy, p.radiance, p.irradiance]
        .into_iter()
        .all(|id| pipeline_cache.get_compute_pipeline(id).is_some());
    if ready && gens.iter().any(|g| g.environment_map.texture.id() == src.texture.id()) {
        GEN_RUNS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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
