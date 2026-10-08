//! Remaster lighting (W3): Bevy's own lights, shadows, environment light and exposure, driven by FH1's
//! time-of-day data (TimeOfDayA.xml via fh1-render's [`FxTimeOfDay`], which keeps running the clock, the zone
//! fog-template blend and FH1_TOD).
//!
//! Units are physical (lux, nits, EV100), so W1/W2's PBR materials need nothing but albedo/roughness/metal:
//! - Sun: a `DirectionalLight` (the engine's) in the game's sun direction (SunPos - SunTargetPos). Its colour is
//!   the TOD SunColor with the atmosphere's own transmittance divided out (sky.rs), so lit surfaces get the game's
//!   colour while the sky is coloured by the atmosphere. Illuminance [`SUN_LUX`] (exo-atmospheric; the atmosphere
//!   dims it on the way down).
//! - Dusk/dawn: FH1's sun never sets (20:00 is still 23 degrees up, its SunColorMult just falls to 0.01). Here the
//!   sun sinks below the horizon as SunColorMult falls ([`twilight`]), so the atmosphere draws a real sunset.
//! - Moon: a second `DirectionalLight` at night (SunObjectMoon), the game's night "sun" direction and colour,
//!   [`MOON_LUX`]. Shadows are cast by whichever of the two is up (one set of cascades).
//! - Environment: `AtmosphereEnvironmentMapLight` on the main camera (diffuse + specular for every PBR material),
//!   no live cube camera.
//! - Exposure: EV100 from the estimated ground illuminance (sun through the atmosphere + sky), compressed towards
//!   the daylight EV ([`EV_COMPRESS`]) so dusk and night stay darker than day, like the game's mood.
//! - Bevy bloom, then FH1's own filmic curve + colour-grading LUT (post.rs).
//!
//! Env (all optional): FH1_RM_SUN_LUX, FH1_RM_MOON_LUX, FH1_RM_EV_BIAS (EV, + = darker), FH1_RM_EV_COMPRESS (0..1),
//! FH1_RM_TONEMAP=game|tony|agx|aces|none (default game = FH1's filmic curve in post.rs), FH1_RM_BLOOM=0|intensity, FH1_RM_ENV=0 (no env map light) / FH1_RM_ENV_SIZE=n / FH1_RM_ENV_INTENSITY=k,
//! FH1_RM_SHADOW_RES=n (quality preset, High 2048), FH1_RM_SHADOW_DIST=m (300; 202 = the game's InGameShadowEnd, measured costlier),
//! FH1_RM_CASCADES=n|old (quality preset, High 2; old = 3 x 1024² and the old far-cascade refresh rules), FH1_RM_SHADOW_NEAR=m (first cascade, 9),
//! FH1_RM_SHADOW_FILTER=gaussian|temporal|hard, FH1_RM_SHADOWS=0 (no sun/moon shadows),
//! FH1_RM_ATMOSPHERE=0 (no Bevy atmosphere: no sky, haze or env light; perf A/B), FH1_RM_ENV_EVERY=1 (re-bake the env
//! map every frame instead of on change), FH1_RM_TWILIGHT=0 (keep the game's sun elevation), FH1_RM_TWILIGHT_HOLD=0 (old: the
//! sun climbs back above the horizon during the one-game-minute sun/moon swap, a daylight flash; see `twilight_sink`), FH1_RM_SUN_DIR=shadow (TrackSettings shadow azimuth instead of
//! the TOD sun's; default TOD since 2026-10-07), FH1_RM_SUN_TINT=0..1 (0.5),
//! FH1_RM_LIGHT_LOG=1 (log the values every 2 s), FH1_RM_FOG=0 (no distance fog) / FH1_RM_FOG=k (density scale, 1).
//!
//! Contact shadows (2026-10-08, default; FH1_RM_CONTACT_SHADOWS=0 = off): Bevy's screen-space contact shadows on the main
//! camera for the sun and moon (FH1_RM_CONTACT_STEPS 8, FH1_RM_CONTACT_LENGTH 0.4 m, thickness 0.15 m): fine shadowing the
//! 1024² cascades can't resolve (tyres on the road, panel gaps, mirrors, arches). They need a depth prepass; the scenery
//! material keeps its prepass off (material.rs), so only cars and Bevy-standard meshes write prepass depth: contact
//! shadows are cast by cars (onto the road and themselves), the cost is the cars' depth-only draws plus a short
//! screen-space march per shadowed pixel.

use bevy::camera::Exposure;
use bevy::color::Mix;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::light::cascade::{Cascade, CascadeShadowConfig, CascadeShadowConfigBuilder, Cascades};
use bevy::light::{AtmosphereEnvironmentMapLight, DirectionalLightShadowMap, EnvironmentMapLight, SimulationLightSystems, SunDisk};
use bevy::light::ShadowFilteringMethod;
use bevy::pbr::AtmosphereSettings;
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;

use fh1_render::lighting::FxTimeOfDay;
use fh1_render::post::FxPostCamera;
use fh1_render::quality::GraphicsQuality;

use crate::sky;

/// Sun illuminance above the atmosphere (lux): Bevy's `RAW_SUNLIGHT` order.
pub const SUN_LUX: f32 = 120_000.0;
/// Moon illuminance (lux). A full moon is ~0.3 lux; games light the night brighter so it reads.
pub const MOON_LUX: f32 = 2.0;
/// How much of the physical exposure change is kept (0 = fixed daylight exposure, 1 = fully adapted).
pub const EV_COMPRESS: f32 = 0.72;

pub(crate) fn env_f32(name: &str, d: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn env_is(name: &str, v: &str) -> bool {
    std::env::var(name).is_ok_and(|x| x.eq_ignore_ascii_case(v))
}

/// This frame's lighting, for other remaster modules (W1 emissive switch-on, W2, night.rs).
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct RemasterLighting {
    /// Direction towards the sun (engine space), after the twilight sink.
    pub sun_dir: Vec3,
    /// Direction towards the moon.
    pub moon_dir: Vec3,
    /// 0 = day, 1 = night (TOD SunObjectMoon).
    pub night: f32,
    /// Lamps on: the TOD SwitchOnLights curve (0..1), what the game's emissive switch reads.
    pub lights_on: f32,
    /// The camera exposure in use (EV100). Emissive in nits is scaled by Bevy with it.
    pub ev100: f32,
    /// Estimated ground illuminance (lux) behind that exposure.
    pub ground_lux: f32,
    /// Game clock (minutes).
    pub minutes: f32,
    /// [`twilight`] amount in use (0 = sun up or FH1_RM_TWILIGHT=0, 1 = sun fully sunk).
    pub twilight: f32,
}

/// Marks the second directional light.
#[derive(Component)]
pub struct MoonLight;

/// Marks the main (FxPostCamera) view in both worlds: post.rs's grade pass runs on it.
#[derive(Component, Clone, Copy, Default, bevy::render::extract_component::ExtractComponent)]
pub struct RemasterView;

pub struct RemasterLightPlugin;

impl Plugin for RemasterLightPlugin {
    fn build(&self, app: &mut App) {
        // Game shaders still drawn in game units (clouds/stars/moon, glows, particles) land where the faithful frame has
        // them under the game curve. Set before any program is built (the gain is baked into each program).
        fh1_render::set_output_gain(crate::post::game_unit_scale());
        // The quality preset's size (High) until apply_quality reads the real preset (same frame as the first light update).
        app.init_resource::<RemasterLighting>()
            .insert_resource(DirectionalLightShadowMap { size: want_shadow_res(None) })
            .add_plugins(bevy::render::extract_component::ExtractComponentPlugin::<RemasterView>::default())
            .add_systems(Startup, (sky::setup_sky, spawn_moon))
            .add_systems(Update, main_only_light_layers)
            .add_systems(Update, (setup_camera, sky::update_sky, env_refresh.after(setup_camera)))
            .add_systems(Update, (game_shader_casters, game_shader_probe_skip, hiz_depth_usage))
            .add_systems(Update, apply_quality)
            .add_systems(PostUpdate, game_fog.after(update_lights))
            .add_systems(PostUpdate, update_lights.before(bevy::transform::TransformSystems::Propagate))
            .insert_resource(bevy::pbr::DirectionalShadowCache::default())
            .add_plugins(bevy::render::extract_resource::ExtractResourcePlugin::<bevy::pbr::DirectionalShadowCache>::default())
            .add_systems(
                PostUpdate,
                (main_view_cascades_only, cache_far_cascade)
                    .chain()
                    .after(SimulationLightSystems::UpdateDirectionalLightCascades)
                    .before(SimulationLightSystems::UpdateLightFrusta),
            );
        crate::post::plugin(app);
        crate::night::plugin(app);
    }
}

fn spawn_moon(mut commands: Commands) {
    commands.spawn((
        DirectionalLight { illuminance: 0.0, shadow_maps_enabled: false, ..default() },
        // No atmosphere disk: fh1-render's sky draws the game's Moon0 sprite (sky.rs, kept in the remaster).
        SunDisk { angular_size: 0.0092, intensity: 0.0 },
        cascade_config(),
        MoonLight,
        Transform::default(),
        Name::new("fh1_remaster_moon"),
    ));
}

/// Sun / moon on `[0, OWN_LIGHT_LAYER]` while main-view-only entities exist (car_probe.rs `main_only_layers`): their
/// shadow casters are culled by the light's layers, so small scenery on the main-only layer keeps its shadow.
fn main_only_light_layers(mut commands: Commands, lights: Query<Entity, (With<DirectionalLight>, Without<bevy::camera::visibility::RenderLayers>)>) {
    if crate::car_probe::main_only_layers().is_none() {
        return;
    }
    // Static-only car probe faces render [PROBE_LAYER] (+ the env-cube sky): the sun / moon must be on it to light the
    // static world's face draw (car_probe.rs `static_faces_on`). Nothing that casts is on that layer.
    let layers = if crate::car_probe::static_faces_on() {
        bevy::camera::visibility::RenderLayers::from_layers(&[0, crate::car_probe::OWN_LIGHT_LAYER, crate::car_probe::PROBE_LAYER])
    } else {
        bevy::camera::visibility::RenderLayers::from_layers(&[0, crate::car_probe::OWN_LIGHT_LAYER])
    };
    for e in &lights {
        commands.entity(e).insert(layers.clone());
    }
}

/// `FH1_RM_CASCADES=old`: the layout before P14 (3 cascades x 1024², far cascade refreshed by fit / age / light only, no
/// camera-move refresh), whatever the quality preset says. FH1_RM_SHADOW_RES / FH1_RM_CASCADE_CACHE_FRAMES still win.
fn old_cascades() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| env_is("FH1_RM_CASCADES", "old"))
}

/// A numeric env override, read once.
fn env_num(name: &'static str, cell: &'static std::sync::OnceLock<Option<f32>>) -> Option<f32> {
    *cell.get_or_init(|| std::env::var(name).ok().and_then(|v| v.trim().parse::<f32>().ok()))
}

/// Cascade count in effect: FH1_RM_CASCADES=n wins, =old -> 3, else the quality preset (High 2; no resource = High).
fn want_cascades(q: Option<&GraphicsQuality>) -> usize {
    static ENV: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    let n = match env_num("FH1_RM_CASCADES", &ENV) {
        Some(n) => n as u32,
        None if old_cascades() => 3,
        None => q.map_or_else(|| GraphicsQuality::default().cascades, |q| q.cascades),
    };
    (n as usize).clamp(1, 4)
}

/// Shadow map size in effect (every cascade; Bevy has one size for the array): FH1_RM_SHADOW_RES=n wins,
/// FH1_RM_CASCADES=old -> 1024, else the quality preset (High 2048).
fn want_shadow_res(q: Option<&GraphicsQuality>) -> usize {
    static ENV: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    let r = match env_num("FH1_RM_SHADOW_RES", &ENV) {
        Some(r) => r.max(1.0) as u32,
        None if old_cascades() => 1024,
        None => q.map_or_else(|| GraphicsQuality::default().shadow_res, |q| q.shadow_res),
    };
    (r.max(1).next_power_of_two() as usize).clamp(512, 8192)
}

/// Frames between far-cascade refreshes: FH1_RM_CASCADE_CACHE_FRAMES=n wins, =old -> 4, else the quality preset (High 4).
fn want_far_refresh(q: Option<&GraphicsQuality>) -> u32 {
    static ENV: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    match env_num("FH1_RM_CASCADE_CACHE_FRAMES", &ENV) {
        Some(n) => n.max(1.0) as u32,
        None if old_cascades() => 4,
        None => q.map_or_else(|| GraphicsQuality::default().shadow_far_refresh, |q| q.shadow_far_refresh).max(1),
    }
}

/// Options > Graphics > Quality (fh1-render quality.rs): shadow map size and cascade count. Compared every frame (a few
/// lights, env read once), not only on `is_changed`: `update_lights` configures the sun on its first frame and a map
/// switch can respawn lights, and both must end up on the preset. Writes only on a difference (no change ticks).
/// FH1_RM_SHADOW_RES / FH1_RM_CASCADES (=n or =old), when set, win over the preset.
fn apply_quality(
    quality: Option<Res<GraphicsQuality>>,
    mut map: ResMut<DirectionalLightShadowMap>,
    mut lights: Query<&mut CascadeShadowConfig, With<DirectionalLight>>,
) {
    let q = quality.as_deref();
    let size = want_shadow_res(q);
    if map.size != size {
        map.size = size;
    }
    let n = want_cascades(q);
    for mut c in &mut lights {
        if c.bounds.len() != n {
            *c = cascade_config_n(n);
        }
    }
}

/// FH1_RM_CONTACT_SHADOWS=0 = off (module doc).
fn contact_shadows_on() -> bool {
    std::env::var("FH1_RM_CONTACT_SHADOWS").map_or(true, |v| v != "0")
}

fn cascade_config() -> CascadeShadowConfig {
    cascade_config_n(want_cascades(None))
}

fn cascade_config_n(n: usize) -> CascadeShadowConfig {
    CascadeShadowConfigBuilder {
        num_cascades: n.clamp(1, 4),
        minimum_distance: 0.1,
        // A small first cascade around the camera: the player car (chase cam ~5-7 m away) gets most of its 1024² texels
        // (~1 cm/texel); only near geometry is drawn into it. FH1_RM_SHADOW_NEAR=m.
        first_cascade_far_bound: env_f32("FH1_RM_SHADOW_NEAR", 9.0),
        // 300 m. The game's InGameShadowEnd (202 m) was tried 2026-10-08 and measured worse in the user's log: festival
        // shadow draws 137-150 -> 162-221, per-view shadow pass 1.85 -> 2.09 ms (the split bounds moved inward and more
        // casters landed in the mid / far cascades). FH1_RM_SHADOW_DIST=202 = the game's end.
        maximum_distance: env_f32("FH1_RM_SHADOW_DIST", 300.0),
        overlap_proportion: 0.2,
    }
    .build()
}

/// The main camera becomes a remaster view: tonemapping, bloom, exposure, atmosphere + its env light.
#[allow(clippy::type_complexity)]
fn setup_camera(mut commands: Commands, cams: Query<Entity, (With<FxPostCamera>, Without<RemasterView>)>) {
    for e in &cams {
        let tonemap = match std::env::var("FH1_RM_TONEMAP").unwrap_or_default().to_ascii_lowercase().as_str() {
            _ if crate::post::game_tonemap() => Tonemapping::None,
            "agx" => Tonemapping::AgX,
            "aces" => Tonemapping::AcesFitted,
            "none" => Tonemapping::None,
            _ => Tonemapping::TonyMcMapface,
        };
        let mut c = commands.entity(e);
        c.insert((RemasterView, tonemap, Exposure { ev100: 14.0 }, bevy::camera::Hdr));
        // The player car's own lights live on their own layer so the car probe's cube doesn't see them (car_probe.rs).
        if crate::car_probe::own_light_layers().is_some() {
            c.insert(bevy::camera::visibility::RenderLayers::from_layers(&[0, crate::car_probe::OWN_LIGHT_LAYER]));
        }
        if contact_shadows_on() {
            c.insert(bevy::pbr::ContactShadows {
                linear_steps: env_f32("FH1_RM_CONTACT_STEPS", 8.0).clamp(1.0, 32.0) as u32,
                thickness: env_f32("FH1_RM_CONTACT_THICKNESS", 0.15),
                length: env_f32("FH1_RM_CONTACT_LENGTH", 0.4),
            });
        }
        // Soft shadow edges (FH1_RM_SHADOW_FILTER=hard|gaussian|temporal; default gaussian).
        c.insert(match std::env::var("FH1_RM_SHADOW_FILTER").unwrap_or_default().to_ascii_lowercase().as_str() {
            "hard" => ShadowFilteringMethod::Hardware2x2,
            "temporal" => ShadowFilteringMethod::Temporal,
            _ => ShadowFilteringMethod::Gaussian,
        });
        let bloom = std::env::var("FH1_RM_BLOOM").ok();
        if bloom.as_deref() != Some("0") {
            let intensity = bloom.and_then(|v| v.parse().ok()).unwrap_or(0.12);
            // P8-B: 256 px top mip (Bevy 512; 15's GPU analysis). FH1_RM_BLOOM_MIP=n, 512 = old.
            let mip = std::env::var("FH1_RM_BLOOM_MIP").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(256).clamp(64, 2048);
            c.insert(Bloom { intensity, max_mip_dimension: mip, ..Bloom::NATURAL });
        }
        if atmosphere_on() {
            c.insert(remaster_atmosphere_settings());
            if env_on() && env_every_frame() {
                c.insert(env_light());
            }
        }
        info!("fh1-remaster: main camera = remaster view ({tonemap:?})");
    }
}

/// The atmosphere settings of the remaster views (main camera, car probe face camera). Smaller LUTs than Bevy's
/// defaults (they are rebuilt every frame): the sky is smooth, the haze low-frequency.
pub(crate) fn remaster_atmosphere_settings() -> AtmosphereSettings {
    AtmosphereSettings {
        sky_view_lut_size: UVec2::new(256, 128),
        sky_view_lut_samples: 12,
        multiscattering_lut_dirs: 32,
        aerial_view_lut_size: UVec3::new(32, 32, 16),
        aerial_view_lut_samples: 8,
        aerial_view_lut_max_distance: 20_000.0,
        ..default()
    }
}

/// XML (left-handed, +Z north) to engine space.
fn mirror_z(v: [f32; 3]) -> Vec3 {
    Vec3::new(v[0], v[1], -v[2])
}

/// How far into dusk/dawn the game is: 0 = sun up, 1 = the game has switched the sun off (SunColorMult ~0) but
/// it is not night yet (no moon). The remaster sinks the sun below the horizon by this amount.
pub fn twilight(sun_mult: f32, moon: f32) -> f32 {
    ((1.0 - sun_mult / 0.6).clamp(0.0, 1.0) * (1.0 - moon)).clamp(0.0, 1.0)
}

/// How far the sun is sunk below the horizon (0 = the game's elevation, 1 = -9 degrees). Unlike [`twilight`] it does not
/// fall back to 0 while the moon comes in: the TOD swaps sun for moon within ONE game minute (Colorado 20:02 and 06:28,
/// SunObjectMoon keys 1202/1203 and 388/389; TimeSpeed 220 there = 0.27 real seconds), and with `twilight` x (1 - moon)
/// the sun rose from -9 to the game's night "sun" elevation (73 degrees) during that minute at up to 66,000 lux (half moon:
/// 28 degrees, 29,000 lux): a daylight flash of the sky, the env map, shadows and the car probe at every dusk and dawn.
/// Fully sunk from 10 % moon on; equal to the raw sun fade without the moon.
pub fn twilight_sink(sun_mult: f32, moon: f32) -> f32 {
    (1.0 - sun_mult / 0.6).clamp(0.0, 1.0).max((moon * 10.0).clamp(0.0, 1.0))
}

/// FH1_RM_TWILIGHT_HOLD=0 = old (the sun climbs back up during the sun/moon swap, see [`twilight_sink`]).
fn twilight_hold() -> bool {
    !flag_off("FH1_RM_TWILIGHT_HOLD")
}

/// Rotate `dir` (towards the light) to elevation `el` (radians), keeping its azimuth.
fn with_elevation(dir: Vec3, el: f32) -> Vec3 {
    let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or(Vec3::X);
    flat * el.cos() + Vec3::Y * el.sin()
}

/// Sky (diffuse) illuminance on the ground as a fraction of the light's exo-atmospheric illuminance, for the
/// exposure estimate: ~12 % at high sun, ~0.5 % at sunset, falling ~10x per 2.5 degrees below the horizon
/// (civil twilight). APPROXIMATE fit of measured daylight/twilight illuminance; only feeds exposure.
fn sky_fraction(sin_el: f32) -> f32 {
    if sin_el >= 0.0 {
        0.005 + 0.115 * sin_el.powf(0.6)
    } else {
        let deg = sin_el.asin().to_degrees();
        0.005 * 10f32.powf(deg / 2.5)
    }
}

fn split(v: Vec3) -> (Color, f32) {
    let m = v.max_element().max(1e-6);
    (Color::linear_rgb(v.x / m, v.y / m, v.z / m), m)
}

type SunQuery<'w, 's> = Query<'w, 's, (Entity, &'static mut DirectionalLight, &'static mut Transform, Has<SunDisk>, &'static mut CascadeShadowConfig), Without<MoonLight>>;

#[allow(clippy::too_many_arguments)]
fn update_lights(
    tod: Option<Res<FxTimeOfDay>>,
    sky_res: Option<Res<sky::RemasterSky>>,
    mut state: ResMut<RemasterLighting>,
    mut sun: SunQuery,
    mut moon: Query<(&mut DirectionalLight, &mut Transform), With<MoonLight>>,
    mut cams: Query<&mut Exposure, With<RemasterView>>,
    ambient: Option<ResMut<GlobalAmbientLight>>,
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut last_log: Local<f32>,
    mut configured: Local<bool>,
    post_config: Option<Res<fh1_render::postfx::FxPostConfig>>,
    mut track_shadow: Local<Option<(Option<Vec3>, std::path::PathBuf)>>,
    quality: Option<Res<GraphicsQuality>>,
) {
    if let Some(cfg) = post_config {
        let path = cfg.0.track_settings.clone();
        if track_shadow.as_ref().is_none_or(|t| t.1 != path) {
            *track_shadow = Some((std::fs::read_to_string(&path).ok().and_then(|x| shadow_light_direction(&x)), path));
        }
    }
    let Some(t) = tod else { return };
    let m = t.minutes();
    let v3 = |n: &str| Vec3::from_array(t.tod.get(n, m));
    let s = |n: &str| t.tod.scalar(n, m);
    let haze = sky_res.as_ref().map_or(0.0, |r| r.haze);
    let terms = sky::medium_terms(haze);

    // The game's light: towards SunPos from SunTargetPos (VERIFIED, lighting.rs), colour × mult × IBL scale.
    let game_dir = (mirror_z(t.tod.get("SunPos", m)) - mirror_z(t.tod.get("SunTargetPos", m))).normalize_or(Vec3::Y);
    let colour = v3("SunColor");
    let mult = s("SunColorMult") * s("IBLDirectScaleEnvironment");
    let night = t.tod.scalar_or("SunObjectMoon", m, 0.0).clamp(0.0, 1.0);
    let k_twi = if env_is("FH1_RM_TWILIGHT", "0") { 0.0 } else { twilight(mult, night) };
    let game_el = game_dir.y.clamp(-1.0, 1.0).asin();
    // The game shades with the TOD sun but casts every shadow from TrackSettings <ShadowLightDirection> (fixed; Colorado
    // azimuth ~88 degrees from the 16:00 sun). One light here, so one of the two has to give. Default = the TOD azimuth
    // (2026-10-07 same-pose A/B: with the shadow azimuth the festival stage, banners and the chase-cam car rear were
    // back-lit, 0.4-0.6x the faithful luma, and the sun glare sat over the stage; with the TOD sun they match and only the
    // shadow layout differs). FH1_RM_SUN_DIR=shadow = the TrackSettings shadow azimuth (old).
    let azimuth_dir = match (track_shadow.as_ref().and_then(|t| t.0), env_is("FH1_RM_SUN_DIR", "shadow")) {
        (Some(to_light), true) => to_light,
        _ => game_dir,
    };
    // Sink to -9 degrees at full twilight: the game's 20:00 is already blue hour (lights on, dark sky), so the sun
    // sets during 19:00-20:00 and the sky goes blue/dark rather than staying orange.
    // Stays sunk through the sun/moon swap ([`twilight_sink`]; FH1_RM_TWILIGHT_HOLD=0 = old).
    let sink = match (env_is("FH1_RM_TWILIGHT", "0"), twilight_hold()) {
        (true, _) => 0.0,
        (false, true) => twilight_sink(mult, night),
        (false, false) => k_twi,
    };
    let sun_el = game_el + (-9f32.to_radians() - game_el) * sink;
    let sun_dir = with_elevation(azimuth_dir, sun_el);

    // Sun: the TOD colour on surfaces = light colour × transmittance, so divide it out (clamped: near and below
    // the horizon the atmosphere decides). Brightness: the game's colour × mult (16:00 ~0.9), except in twilight
    // where the sinking sun does the dimming.
    // Only part of the TOD tint goes into the light (FH1_RM_SUN_TINT, default 0.5): the same light colours the
    // atmosphere, and the game's strongly orange 08:00 sun turned the whole sky yellow. The atmosphere adds its own
    // warmth near the horizon.
    // 0.5 since 2026-10-07: at 0.75 sunlit pale ground was too orange vs faithful (blue/red 0.65 vs 0.98).
    let tint = env_f32("FH1_RM_SUN_TINT", 0.5).clamp(0.0, 1.0);
    let tr_sun = sky::transmittance(&terms, 150.0, sun_dir.y);
    let (tod_colour, _) = split(colour / tr_sun.max(Vec3::splat(0.25)));
    let tod_colour = tod_colour.to_linear();
    let sun_colour = Color::LinearRgba(LinearRgba::WHITE.mix(&tod_colour, tint));
    let strength = (colour.max_element() * mult).clamp(0.0, 1.5);
    let sun_scale = strength + (1.0 - strength).max(0.0) * k_twi;
    let sun_lux = env_f32("FH1_RM_SUN_LUX", SUN_LUX) * sun_scale * (1.0 - night);

    // Moon: the game's night light (the same channels; the game re-uses its sun for the moon).
    let moon_dir = game_dir;
    let tr_moon = sky::transmittance(&terms, 150.0, moon_dir.y);
    let (moon_colour, _) = split(colour / tr_moon.max(Vec3::splat(0.25)));
    let moon_colour = Color::LinearRgba(LinearRgba::WHITE.mix(&moon_colour.to_linear(), 0.6));
    let moon_lux = env_f32("FH1_RM_MOON_LUX", MOON_LUX) * night;

    let moon_casts = night > 0.5;
    for (e, mut l, mut tf, has_disk, mut cfg) in &mut sun {
        l.illuminance = sun_lux;
        l.color = sun_colour;
        l.shadow_maps_enabled = shadows_on() && !moon_casts && sun_dir.y > 0.0;
        l.contact_shadows_enabled = l.shadow_maps_enabled && contact_shadows_on();
        l.shadow_depth_bias = 0.02;
        l.shadow_normal_bias = 1.0;
        *tf = Transform::default().looking_to(-sun_dir, Vec3::Y);
        if !has_disk {
            commands.entity(e).insert(SunDisk::EARTH);
        }
        if !*configured {
            // The preset's count (not the High default): apply_quality ran earlier this frame and must not be undone.
            *cfg = cascade_config_n(want_cascades(quality.as_deref()));
        }
    }
    *configured = !sun.is_empty();
    for (mut l, mut tf) in &mut moon {
        l.illuminance = moon_lux;
        l.color = moon_colour;
        l.shadow_maps_enabled = shadows_on() && moon_casts;
        l.contact_shadows_enabled = l.shadow_maps_enabled && contact_shadows_on();
        l.shadow_depth_bias = 0.02;
        l.shadow_normal_bias = 1.0;
        *tf = Transform::default().looking_to(-moon_dir, Vec3::Y);
    }

    // Exposure: ground illuminance = direct (through the atmosphere, on flat ground) + sky.
    let lum = |v: Vec3| v.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    let direct = |lux: f32, dir: Vec3, c: Color| {
        let c = c.to_linear();
        let tr = sky::transmittance(&terms, 150.0, dir.y);
        lux * dir.y.max(0.0) * lum(Vec3::new(c.red, c.green, c.blue) * tr)
    };
    let ground = direct(sun_lux, sun_dir, sun_colour)
        + sun_lux * sky_fraction(sun_dir.y)
        + direct(moon_lux, moon_dir, moon_colour)
        + moon_lux * sky_fraction(moon_dir.y)
        + 0.05;
    // EV100 at which an 18 % grey lit by `ground` lux lands on Bevy's mid grey: ev = log2(E / (1.2 pi)).
    let ev_phys = (ground / (1.2 * std::f32::consts::PI)).log2();
    let ev_day = ((env_f32("FH1_RM_SUN_LUX", SUN_LUX) * 0.75) / (1.2 * std::f32::consts::PI)).log2();
    let k = env_f32("FH1_RM_EV_COMPRESS", EV_COMPRESS).clamp(0.0, 1.0);
    let ev = ev_day - k * (ev_day - ev_phys) + env_f32("FH1_RM_EV_BIAS", 0.0);
    for mut e in &mut cams {
        if (e.ev100 - ev).abs() > 1e-3 {
            e.ev100 = ev;
        }
    }

    // Bevy's flat ambient: only a faint night floor (sky glow / light pollution) in the sky's colour; the
    // env map does the rest.
    if let Some(mut a) = ambient {
        let (c, _) = split(v3("HLTopColour").max(Vec3::splat(1e-4)));
        // Also through twilight (2026-10-07): with the sun sunk and no moon yet the env map is near black, and the car
        // at 20:00 (only env + sun, no lightmaps) went black where the faithful car is dark red.
        // With the twilight hold, the dusk share is the raw sun fade (not masked by the moon): `k_twi` dips to ~0.5 at the
        // swap's half-way point and the ambient blinked to half for that game minute.
        let dusk = match (env_is("FH1_RM_DUSK_AMBIENT", "0"), twilight_hold()) {
            (true, _) => 0.0,
            (false, true) if !env_is("FH1_RM_TWILIGHT", "0") => (1.0 - mult / 0.6).clamp(0.0, 1.0),
            _ => k_twi,
        };
        let want = env_f32("FH1_RM_NIGHT_AMBIENT", 0.3) * night.max(dusk);
        if (a.brightness - want).abs() > 1e-3 || a.color != c {
            a.brightness = want;
            a.color = c;
        }
    }

    *state = RemasterLighting { sun_dir, moon_dir, night, lights_on: t.tod.scalar_or("SwitchOnLights", m, 0.0), ev100: ev, ground_lux: ground, minutes: m, twilight: k_twi };

    if std::env::var_os("FH1_RM_LIGHT_LOG").is_some() && time.elapsed_secs() - *last_log >= 2.0 {
        *last_log = time.elapsed_secs();
        info!(
            "remaster light: {:02}:{:02} sun el {:.1} (game {:.1}, twi {:.2}) {:.0} lx {:?} | moon {:.2} lx | night {:.2} haze {:.2e} | ground {:.1} lx ev {:.2} (phys {:.2})",
            (m / 60.0) as u32,
            (m % 60.0) as u32,
            sun_el.to_degrees(),
            game_el.to_degrees(),
            k_twi,
            sun_lux,
            sun_colour.to_linear(),
            moon_lux,
            night,
            haze,
            ground,
            ev,
            ev_phys
        );
    }
}

/// Only the main camera gets cascades: the UI scene and minimap cameras' cascades would be empty shadow views
/// that still cost the render thread (P6 "main-only"; fh1-render's shadow.rs, which did this, is off here).
#[allow(clippy::type_complexity)]
fn main_view_cascades_only(
    main: Query<Entity, With<RemasterView>>,
    cams: Query<Has<Camera3d>, With<Camera>>,
    mut lights: Query<&mut Cascades>,
) {
    let Some(cam) = main.iter().next() else { return };
    // Emptied, not removed: Bevy's prepare_lights unwraps the per-view entry (bevy_pbr render/light.rs
    // `light.cascades.get(&entity).unwrap()`) for every active view that sees the light's layers. Removing it panicked as
    // soon as the car probe's face camera (car_probe.rs) switched to layer 0 for its first bake. An empty list = no
    // shadow views for that camera (fh1-render shadow.rs / reflect.rs keep their extra cameras the same way).
    // P8 (2026-10-08 pm): Bevy builds cascades for every active camera with a projection, incl. the HUD / world map /
    // minimap Camera2d; an empty entry still made check_dir_light_mesh_visibility walk every caster once more per frame
    // for that view. Entries of views that are not Camera3d are removed: prepare_lights only takes Camera3d views, so it
    // never looks them up. (The first version pruned by render layers and crashed on map load: the probe face switches
    // from its idle layer to layer 0 in car_probe.rs `drive`, which can run after this system in the same frame, so the
    // extracted view saw the light and unwrapped the removed entry; vendor/bevy_pbr now also skips a missing entry.)
    // FH1_RM_CASCADE_PRUNE=0 = old (emptied only).
    let prune = cascade_prune();
    for mut c in &mut lights {
        if prune {
            let blind = |v: &Entity| *v != cam && cams.get(*v).is_ok_and(|is_3d| !is_3d);
            if c.cascades.keys().any(&blind) {
                c.cascades.retain(|v, _| !blind(v));
            }
        }
        if c.cascades.iter().any(|(v, l)| *v != cam && !l.is_empty()) {
            for (v, l) in c.cascades.iter_mut() {
                if *v != cam {
                    l.clear();
                }
            }
        }
    }
}

/// Far cascade caching (P8-B, docs/PERF.md; needs the vendored bevy_pbr patch, vendor/bevy_pbr/FH1_PATCHES.md).
/// The last cascade of the main view is drawn on a refresh frame with matrices fitted to Bevy's own cascade grown by
/// FH1_RM_CASCADE_CACHE_MARGIN (0.15 = 15 % wider and deeper, texel-snapped), and then kept: the same matrices are put
/// back every frame and bevy_pbr skips that cascade's clear + draws (`DirectionalShadowCache::skip_mask`), so the
/// shadow map holds the refresh frame's depth. A refresh comes when Bevy's fresh cascade no longer fits inside the kept
/// one (camera moved / turned), the light turned > 0.05 deg (TOD clock), after the quality preset's `shadow_far_refresh`
/// frames (High 4; FH1_RM_CASCADE_CACHE_FRAMES=n wins; 1 = no caching), or when the light, cascade count or map size changed.
/// Nearer cascades render every frame.
///
/// P14 (2 cascades): the cached cascade now starts at 9 m, so moving casters near the player (the car's own shadow tip
/// past 9 m, traffic alongside) would lag by up to N-1 frames. Camera-move refresh: when the main camera has moved more
/// than FH1_RM_CASCADE_CACHE_MOVE (2) kept-cascade texels since the refresh (~0.85 m at 2048² / 300 m), it refreshes
/// this frame, so the lag of anything moving with the camera stays under ~2 texels. A camera cut / teleport / fast turn is
/// the same case (the move test, or the fresh cascade no longer fitting): the cascade is re-drawn in full on THAT frame
/// and the cadence restarts from it, never a stale frame. The talk's "render it at reduced resolution on the cut frame"
/// is not done: our cost is per-draw encode + vertex work, which a smaller viewport doesn't cut, and sampling a sub-rect
/// would need a bevy_pbr shader patch (UV scale + texel size per cascade). A refresh frame costs what every frame cost
/// before the cache (no stall). FH1_RM_CASCADE_CACHE_MOVE=0 or FH1_RM_CASCADES=old = no move refresh (old).
/// `FH1_RM_CASCADE_CACHE=0` = off (every cascade every frame, Bevy's matrices).
#[derive(Default)]
struct FarCascadeCache {
    light: Option<Entity>,
    count: usize,
    map_size: usize,
    light_rot: Mat3,
    kept: Option<Cascade>,
    /// Kept cascade in light space: centre (x, y), near-plane z, depth range, diameter.
    centre: Vec2,
    near: f32,
    depth: f32,
    diameter: f32,
    age: u32,
    /// Main camera position at the refresh.
    cam_pos: Vec3,
}

/// FH1_RM_CASCADE_CACHE_MOVE=texels (2; 0 = off, also off with FH1_RM_CASCADES=old): camera-move refresh of the cached cascade.
fn cache_move_texels() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let d = if old_cascades() { 0.0 } else { 2.0 };
        env_f32("FH1_RM_CASCADE_CACHE_MOVE", d).max(0.0)
    })
}

fn cascade_cache_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| !flag_off("FH1_RM_CASCADE_CACHE"))
}

/// Light-space parameters of a Bevy cascade (bevy_light cascade.rs `calculate_cascade`): rotation (world from light),
/// near-plane centre (light space), depth range, diameter.
fn cascade_params(c: &Cascade) -> (Mat3, Vec3, f32, f32) {
    let rot = Mat3::from_mat4(c.world_from_cascade);
    let centre = rot.transpose() * c.world_from_cascade.w_axis.truncate();
    let depth = 1.0 / c.clip_from_cascade.z_axis.z.max(1e-12);
    let diameter = 2.0 / c.clip_from_cascade.x_axis.x.max(1e-12);
    (rot, centre, depth, diameter)
}

/// A cascade built like Bevy's from light-space parameters (same matrix forms, reverse Z).
fn build_cascade(rot: Mat3, centre: Vec3, depth: f32, diameter: f32, texel: f32) -> Cascade {
    let wfl = Mat4::from_mat3(rot);
    let t = wfl.transpose();
    let cascade_from_world = Mat4::from_cols(t.x_axis, t.y_axis, t.z_axis, (-centre).extend(1.0));
    let world_from_cascade = Mat4::from_cols(wfl.x_axis, wfl.y_axis, wfl.z_axis, wfl * centre.extend(1.0));
    let clip_from_cascade = Mat4::from_cols(
        Vec4::new(2.0 / diameter, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 2.0 / diameter, 0.0, 0.0),
        Vec4::new(0.0, 0.0, 1.0 / depth, 0.0),
        Vec4::new(0.0, 0.0, 1.0, 1.0),
    );
    Cascade { world_from_cascade, clip_from_cascade, clip_from_world: clip_from_cascade * cascade_from_world, texel_size: texel }
}

#[allow(clippy::type_complexity)]
fn cache_far_cascade(
    main: Query<(Entity, &GlobalTransform), With<RemasterView>>,
    mut lights: Query<(Entity, &DirectionalLight, &mut Cascades)>,
    map: Res<DirectionalLightShadowMap>,
    mut cache: ResMut<bevy::pbr::DirectionalShadowCache>,
    quality: Option<Res<GraphicsQuality>>,
    mut st: Local<FarCascadeCache>,
) {
    let off = |cache: &mut bevy::pbr::DirectionalShadowCache| {
        if cache.enabled || cache.skip_mask != 0 {
            cache.enabled = cascade_cache_on();
            cache.skip_mask = 0;
        }
    };
    if !cascade_cache_on() {
        off(&mut *cache);
        return;
    }
    if !cache.enabled {
        cache.enabled = true;
    }
    let Some((cam, cam_tf)) = main.iter().next() else {
        off(&mut *cache);
        return;
    };
    let cam_pos = cam_tf.translation();
    let max_frames = want_far_refresh(quality.as_deref());
    let Some((light, mut cascades)) = lights.iter_mut().find(|(_, l, c)| l.shadow_maps_enabled && c.cascades.get(&cam).is_some_and(|v| !v.is_empty())).map(|(e, _, c)| (e, c)) else {
        *st = FarCascadeCache::default();
        cache.skip_mask = 0;
        return;
    };
    let Some(list) = cascades.cascades.get_mut(&cam) else { return };
    let n = list.len();
    if n < 2 || max_frames <= 1 {
        *st = FarCascadeCache::default();
        cache.skip_mask = 0;
        return;
    }
    let k = n - 1;
    let (rot, fresh_centre, fresh_depth, fresh_d) = cascade_params(&list[k]);
    let size = map.size.max(1);
    let fresh_texel = fresh_d / size as f32;
    let fits = st.kept.is_some() && {
        let half = 0.5 * st.diameter;
        let d = (fresh_centre.truncate() - st.centre).abs();
        d.x + 0.5 * fresh_d + fresh_texel <= half
            && d.y + 0.5 * fresh_d + fresh_texel <= half
            && fresh_centre.z <= st.near
            && fresh_centre.z - fresh_depth >= st.near - st.depth
    };
    let move_texels = cache_move_texels();
    let moved = move_texels > 0.0 && st.kept.is_some() && cam_pos.distance(st.cam_pos) > move_texels * st.diameter / size as f32;
    let refresh = !fits
        || moved
        || st.light != Some(light)
        || st.count != n
        || st.map_size != size
        || st.light_rot.z_axis.dot(rot.z_axis) < 0.05f32.to_radians().cos()
        || st.light_rot.x_axis.dot(rot.x_axis) < 0.05f32.to_radians().cos()
        || st.age + 1 >= max_frames;
    if refresh {
        let margin = env_f32("FH1_RM_CASCADE_CACHE_MARGIN", 0.15).clamp(0.0, 1.0);
        let diameter = (fresh_d * (1.0 + margin)).ceil();
        let texel = diameter / size as f32;
        let centre = (fresh_centre.truncate() / texel).floor() * texel;
        let pad = 0.5 * margin * fresh_d;
        let near = fresh_centre.z + pad;
        let depth = fresh_depth + 2.0 * pad;
        *st = FarCascadeCache {
            light: Some(light),
            count: n,
            map_size: size,
            light_rot: rot,
            kept: Some(build_cascade(rot, centre.extend(near), depth, diameter, texel)),
            centre,
            near,
            depth,
            diameter,
            age: 0,
            cam_pos,
        };
        cache.skip_mask = 0;
    } else {
        // Same rotation as the kept one (within the 0.05 deg refresh threshold).
        st.age += 1;
        cache.skip_mask = 1 << k;
    }
    if let Some(kept) = &st.kept {
        list[k] = kept.clone();
    }
}

/// FH1_RM_CASCADE_PRUNE=0 = keep (empty) cascade entries for views that can't see the light (see `main_view_cascades_only`).
fn cascade_prune() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| !flag_off("FH1_RM_CASCADE_PRUNE"))
}

fn flag_off(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("0")
}

pub fn atmosphere_on() -> bool {
    !flag_off("FH1_RM_ATMOSPHERE")
}

fn env_on() -> bool {
    !flag_off("FH1_RM_ENV")
}

fn shadows_on() -> bool {
    !flag_off("FH1_RM_SHADOWS")
}

fn env_light() -> AtmosphereEnvironmentMapLight {
    let size = env_f32("FH1_RM_ENV_SIZE", 128.0) as u32;
    AtmosphereEnvironmentMapLight { size: UVec2::splat(size.next_power_of_two().clamp(32, 1024)), intensity: env_f32("FH1_RM_ENV_INTENSITY", 1.0), ..default() }
}

fn env_every_frame() -> bool {
    std::env::var("FH1_RM_ENV_EVERY").as_deref() == Ok("1")
}

/// Frames a bake probe lives after its filtered maps exist (atmosphere probe pass + Bevy's GGX/diffuse filter run
/// each of those frames), so the LUTs it reads are current and the filtered maps complete.
const BAKE_FRAMES: u32 = 4;

/// A short-lived entity that bakes the atmosphere into an environment map.
#[derive(Component)]
struct EnvBakeProbe {
    frames: u32,
}

/// The env map is baked on demand, not every frame. Bevy re-renders the atmosphere probe and re-filters the whole
/// cube every frame for as long as an `AtmosphereEnvironmentMapLight` exists, so the generator lives on a bake
/// entity: once its filtered `EnvironmentMapLight` has been written for [`BAKE_FRAMES`] frames, the camera takes a copy
/// (same GPU images) and the bake entity is despawned. A new bake starts when the sun or moon moved > 0.5 degrees, or
/// the night blend or the haze changed (> 3 %). `FH1_RM_ENV_EVERY=1` = Bevy's every-frame generator on the camera.
#[allow(clippy::type_complexity)]
fn env_refresh(
    mut commands: Commands,
    lighting: Res<RemasterLighting>,
    sky_res: Option<Res<sky::RemasterSky>>,
    cams: Query<Entity, With<RemasterView>>,
    mut probes: Query<(Entity, &mut EnvBakeProbe, Option<&EnvironmentMapLight>)>,
    mut state: Local<Option<(Vec3, Vec3, f32, f32)>>,
) {
    if !atmosphere_on() || !env_on() || env_every_frame() {
        return;
    }
    let Some(cam) = cams.iter().next() else { return };
    let haze = sky_res.map_or(0.0, |s| s.haze);
    let now = (lighting.sun_dir, lighting.moon_dir, lighting.night, haze);
    let stale = match *state {
        None => true,
        Some((sun, moon, night, h)) => {
            let cos = 0.5f32.to_radians().cos();
            sun.dot(now.0) < cos || moon.dot(now.1) < cos || (night - now.2).abs() > 0.02 || (h - now.3).abs() > 0.03 * h.max(1e-7)
        }
    };
    if lighting.sun_dir == Vec3::ZERO {
        return; // lights not set yet
    }
    let mut baking = false;
    for (e, mut probe, env) in &mut probes {
        let Some(env) = env else {
            baking = true;
            continue;
        };
        probe.frames += 1;
        if probe.frames >= BAKE_FRAMES {
            commands.entity(cam).insert(env.clone());
            commands.entity(e).despawn();
        } else {
            baking = true;
        }
    }
    if stale && !baking {
        *state = Some(now);
        commands.spawn((env_light(), EnvBakeProbe { frames: 0 }, Transform::default(), Name::new("fh1_remaster_env_bake")));
    }
}

/// TrackSettings `<ShadowLightDirection>` (when `<OverrideShadowLightDirection value="1">`) as a direction towards the
/// light, engine space (XML is left-handed: Z negated, as fh1-render shadow.rs).
fn shadow_light_direction(xml: &str) -> Option<Vec3> {
    let attr = |tag: &str, name: &str| -> Option<f32> {
        let at = xml.find(&format!("<{tag} "))?;
        let rest = &xml[at..];
        let rest = &rest[..rest.find('>')?];
        let k = rest.find(&format!("{name}=\""))? + name.len() + 2;
        rest[k..].split('"').next()?.trim().parse().ok()
    };
    if attr("OverrideShadowLightDirection", "value").unwrap_or(0.0) == 0.0 {
        return None;
    }
    let travel = Vec3::new(attr("ShadowLightDirection", "x")?, attr("ShadowLightDirection", "y")?, -attr("ShadowLightDirection", "z")?);
    Some(-travel.normalize_or(Vec3::NEG_Y))
}

/// Game-shader meshes still drawn in the remaster (imported FM4/FH2 maps, the game's clouds/stars, glows, leftovers)
/// cast through FxMaterial's own shadow pass now that fh1-render's shadow.rs (which filtered them) is off. Same rule
/// as its `skip_non_casters`: sky parts and blended programs never cast. Without it an imported map's sky dome put
/// the whole track in shadow.
#[allow(clippy::type_complexity)]
fn game_shader_casters(
    mut commands: Commands,
    new: Query<(Entity, &MeshMaterial3d<fh1_render::FxMaterial>, Has<fh1_render::sky::SkyPart>, Option<&bevy::camera::primitives::Aabb>), (Added<MeshMaterial3d<fh1_render::FxMaterial>>, Without<bevy::light::NotShadowCaster>)>,
    materials: Res<Assets<fh1_render::FxMaterial>>,
) {
    for (e, m, sky, aabb) in &new {
        // A mesh spanning kilometres is a dome/backdrop, whatever its program.
        let huge = aabb.is_some_and(|b| b.half_extents.max_element() > 1500.0);
        if sky || huge || materials.get(&m.0).is_some_and(|m| m.alpha_blend) {
            commands.entity(e).try_insert(bevy::light::NotShadowCaster);
        }
    }
}

/// The static world's Hi-Z (static_world/hiz.rs) reads the main camera's depth in a compute pass: its depth texture
/// needs TEXTURE_BINDING (Bevy's default is RENDER_ATTACHMENT only). Only with `FH1_STATIC_WORLD=1` and the Hi-Z on.
fn hiz_depth_usage(mut cams: Query<&mut Camera3d, With<RemasterView>>) {
    // Same switch as static_world/hiz.rs `hiz_on` (FH1_STATIC_WORLD_HIZ=0 = off).
    if !crate::static_world::on() || std::env::var("FH1_STATIC_WORLD_HIZ").is_ok_and(|v| v == "0") {
        return;
    }
    use bevy::render::render_resource::TextureUsages;
    let want = (TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING).bits();
    for mut c in &mut cams {
        if c.depth_texture_usages.0 & want != want {
            c.depth_texture_usages.0 |= want;
        }
    }
}

/// Lean car-probe faces (car_probe.rs `lean_on`): blended game-shader parts (glows, light beams, glass, decals; not the sky,
/// which is most of what the cube is for) render on the main view's layer only.
#[allow(clippy::type_complexity)]
fn game_shader_probe_skip(
    mut commands: Commands,
    new: Query<(Entity, &MeshMaterial3d<fh1_render::FxMaterial>), (Added<MeshMaterial3d<fh1_render::FxMaterial>>, Without<fh1_render::sky::SkyPart>, Without<bevy::camera::visibility::RenderLayers>)>,
    materials: Res<Assets<fh1_render::FxMaterial>>,
) {
    let Some(layers) = crate::car_probe::probe_skip_layers() else { return };
    for (e, m) in &new {
        if materials.get(&m.0).is_some_and(|m| m.alpha_blend) {
            commands.entity(e).try_insert(layers.clone());
        }
    }
}

/// The game's distance fog on the PBR materials (Bevy `DistanceFog`): density and colour from the same packed globals
/// the faithful shaders read (lighting.rs apply_time_of_day: FogConsts.w = FogDensity × zone template × 0.01, FogColor
/// = FogColour × template), so zone templates (Redrock haze) and the TOD carry over. The colour is in game units, which
/// enter the game curve like the game-shader output (× game_unit_scale). Exponential from the camera: the game's
/// FogStartDistance offset (~600-800 m) and height fade are not modelled. Game-shader meshes apply their own fog.
fn game_fog(
    mut commands: Commands,
    globals: Option<Res<fh1_render::FxGlobals>>,
    mut cams: Query<(Entity, Option<&mut bevy::pbr::DistanceFog>), With<RemasterView>>,
) {
    let scale = std::env::var("FH1_RM_FOG").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(1.0);
    let Some(g) = globals else { return };
    let (Some(consts), Some(colour)) = (g.get("FogConsts"), g.get("FogColor")) else { return };
    let gain = crate::post::game_unit_scale();
    let density = (consts.w * scale).max(0.0);
    let c = colour.truncate() * gain;
    let want = bevy::pbr::DistanceFog {
        color: Color::linear_rgb(c.x, c.y, c.z),
        directional_light_color: Color::NONE,
        directional_light_exponent: 8.0,
        falloff: bevy::pbr::FogFalloff::Exponential { density },
    };
    for (e, fog) in &mut cams {
        match fog {
            _ if scale <= 0.0 => {
                commands.entity(e).remove::<bevy::pbr::DistanceFog>();
            }
            Some(mut f) => {
                let same = matches!(f.falloff, bevy::pbr::FogFalloff::Exponential { density: d } if (d - density).abs() < 1e-7) && f.color == want.color;
                if !same {
                    *f = want.clone();
                }
            }
            None => {
                commands.entity(e).insert(want.clone());
            }
        }
    }
}
