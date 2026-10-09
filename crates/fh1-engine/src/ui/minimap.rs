//! FH1's minimap: the road network from `colorado.nav` drawn heading-up into a 256×256 render
//! target, which the HUD scene shows in its `MATERIAL_VIEWPORT` quad (its `PLACEHOLDER.TGA` slot,
//! masked by `SATNAVAREA`), as the game's CUSTREND_MAPRENDERER does. Also the satnav route
//! ([`SatNav`]) and the map icons (`map/pois.tsv`, written by `fh1_ui::mappois`).
//!
//! VERIFIED (default.xex, CUIMinimapRenderer draw 0x8263F438):
//! - The renderer gets a 2D centre, a heading and a scale per frame.
//! - Scale (0x82637AA8) = zoom / (render-target width × 0.5), with zoom 0.55 in free roam. So the map's
//!   half-width is 128 / 0.55 ≈ 233 m.
//! - The heading follows the car through a clamped spring (0x82637C70), see [`HeadingSpring`].
//! - The HUD layer's own camera is orthographic, so the tilt comes from the map renderer's
//!   `g_WorldViewProjectionMesh` (map VS 0x821a161c).
//! - No background: the target is cleared transparent and the HUD multiplies its alpha by
//!   SATNAVAREA (Xenia refs: roads over the scene).
//!
//! From `MapProfileMinimap.xml` (VERIFIED values):
//! - Road shadows `line_fade_lr`: black, alpha 128, sort 90.
//! - Roads `line_coloured_noalpha`: dirt 10, b/a 14, freeway 14 wide (sort 100..130), Primary under
//!   Secondary.
//! - Route `line_coloured` (74,238,97), Size 14, sort 165.
//! - Icons `icon_facing`: atlas cells, DynamicSize, VisibleRange 2400..2800 m.
//! - Player arrow last: icon sheet cell (2,3) shadow at alpha 220 under (3,3).
//!
//! Drawn by a Camera2d (perf, 2026-10-08): the map is Mesh2d in the engine's XZ plane under the same tilted perspective,
//! so its view runs the cheap 2D graph (no shadow cascades, atmosphere, prepass or MSAA as the old Camera3d did).
//! Layer order = a sub-millimetre `translation.z` per layer (the 2D phase sorts by z, and z is a ground axis here);
//! the view-depth Secondary→Primary road tint is `RoadFogMaterial` (ui/minimap_road.wgsl) instead of DistanceFog.
//!
//! Sharpness (2026-10-08 pm, user: fuzzy / pixelated since the Camera2d): the target is sized to the disc's on-screen
//! pixels (256 × the HUD's 1280×720 → window scale, rounded up to 32, 256..1024; resized with the window) and the map
//! camera draws with 4x MSAA (the old Camera3d had Bevy's default 4x; the Camera2d had none, so every road edge
//! stair-stepped and the 1440p HUD magnified a 256² image 2x). Sampled linear. The world scale ([`RT_SIZE`],
//! [`HALF_EXTENT`], [`M_PER_PX`]) stays the game's 256 px logical target. The camera targets its own image, so the
//! Graphics AA / render-scale settings (ui/graphics.rs, window cameras only) never touch it.
//! `FH1_MINIMAP_RES=<px>` fixes the size (256 = old), `FH1_MINIMAP_MSAA=0` = no MSAA (old).
//!
//! GUESSES until the traced WVP answers them:
//! - The perspective camera ([`TILT_DEG`], [`FOV_DEG`], [`LOOK_AHEAD`]).
//! - Size = render-target pixels at the ortho scale.
//! - Secondary (near) → Primary (far) lerped by view depth (Xenia refs: far roads read grey).
//! - The shadow width and the icon sizes.

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ClearColorConfig, RenderTarget};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, ShaderType, TextureFormat};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dPlugin};

use std::sync::Arc;

use super::scene::{UiCamera, UiData};
use super::worldmap::circle;
use super::worldmap::state::{self, Badge, IconState};
use crate::Car;

/// Render layer of the minimap's own world (roads, arrow).
pub const MAP_LAYER: usize = 8;
/// Logical render target size (the HUD quad is 256×256 at 1280×720; `MODEL_VIEWPORT` is 256² too). The map's world
/// scale is defined on it; the image itself is [`rt_pixels`].
const RT_SIZE: u32 = 256;

/// The target's pixel size: the disc's size on screen (the HUD scales 1280×720 to the window, AutoMin), rounded up to
/// 32 and clamped to 256..1024. `FH1_MINIMAP_RES=<px>` fixes it.
fn rt_pixels(window: Option<&Window>) -> u32 {
    if let Some(px) = std::env::var("FH1_MINIMAP_RES").ok().and_then(|v| v.parse::<u32>().ok()) {
        return px.clamp(64, 2048);
    }
    let Some(w) = window else { return RT_SIZE };
    let phys = w.physical_size().as_vec2();
    let scale = (phys.x / 1280.0).min(phys.y / 720.0);
    let px = (RT_SIZE as f32 * scale).ceil() as u32;
    px.div_ceil(32).saturating_mul(32).clamp(RT_SIZE, 1024)
}

/// The map render target (resized with the window by [`resize_target`]).
#[derive(Resource)]
struct MinimapImage(Handle<Image>);
/// Free-roam zoom (0x82637AA8: 0.55; 0.2 when the renderer's flag +0x1305 is set).
const ZOOM: f32 = 0.55;
/// World metres from the map centre to its edge: (RT_SIZE / 2) / ZOOM.
const HALF_EXTENT: f32 = RT_SIZE as f32 * 0.5 / ZOOM;
/// Metres per render-target pixel at the map centre (profile `Size` unit, GUESS).
const M_PER_PX: f32 = HALF_EXTENT / (RT_SIZE as f32 * 0.5);
/// Camera elevation above the map plane, vertical fov and the focus point's distance ahead of the
/// car (GUESS; the Xenia refs show the festival ring at about 150×60 px and the arrow ~40 px
/// below the quad centre). To be replaced by the traced `g_WorldViewProjectionMesh`.
const TILT_DEG: f32 = 28.0;
const FOV_DEG: f32 = 40.0;
const LOOK_AHEAD: f32 = 110.0;
/// The player arrow in render-target pixels (profile DynamicSize 45 is at fullscreen scale; GUESS).
const ARROW_PX: f32 = 22.0;

/// Road styles from MapProfileMinimap.xml: (road_type, Size, ColourPrimary, ColourSecondary, sort).
const ROADS: [(&str, f32, [u8; 3], [u8; 3], f32); 4] = [
    ("dirt", 10.0, [124, 96, 63], [253, 193, 145], 100.0),
    ("b", 14.0, [122, 122, 122], [255, 255, 255], 110.0),
    ("a", 14.0, [122, 122, 122], [255, 255, 255], 110.0),
    ("freeway", 14.0, [122, 122, 122], [199, 182, 99], 130.0),
];

/// Route style from the profile's `route` group (line_coloured, Size 14, sort 165).
const ROUTE: (f32, [u8; 3], f32) = (14.0, [74, 238, 97], 165.0);

/// Race central (`race_central.xml` TriggerZone `festival_02`, mapTag racecentral): the "Head to
/// the Horizon Heats" objective of the Xenia refs. Engine space (collision (-843.628, -256.318) on
/// the EU disc); used when `map/pois.tsv` isn't installed yet.
pub const RACE_CENTRAL: Vec2 = Vec2::new(-843.628, 256.318);

/// Minimap icon styles from MapProfileMinimap.xml (`icon_facing` groups): activity_type, icon
/// sheet cell, DynamicSize, shown in free roam without progress. Free roam shows the festival venues, gas
/// stations, workshops and street race hubs (GUESS; career events, barn finds and speed cameras
/// depend on progress: `FH1_MAP_POIS=all` shows them too).
pub(super) const POI_STYLES: [(&str, (u32, u32), f32, bool); 13] = [
    ("racecentral", (4, 2), 35.0, true),
    ("workshop", (3, 2), 35.0, true),
    ("autoshow", (6, 2), 35.0, true),
    ("paintshop", (2, 2), 35.0, true),
    ("carclub", (5, 2), 35.0, true),
    ("dlccenter", (7, 2), 35.0, true),
    ("gas_station", (1, 2), 45.0, true),
    ("streetrace", (2, 1), 45.0, true),
    ("race", (1, 0), 45.0, false),
    ("exhibition", (0, 1), 45.0, false),
    ("nemesisrace", (4, 1), 55.0, false),
    ("barnfind", (0, 4), 45.0, false),
    ("speed_camera", (1, 3), 45.0, false),
];
/// VisibleRange of every icon group: full to 2400 m, gone at 2800 m.
const POI_FADE: (f32, f32) = (2400.0, 2800.0);

/// Satnav state: gameplay (or `FH1_ROUTE=x,z`, engine metres) sets `target`; the minimap draws
/// the road route there and fills `distance_m` (route length) for the HUD's TEXT_DISTANCE.
/// On Colorado it defaults to race central (free roam's first objective).
#[derive(Resource, Default)]
pub struct SatNav {
    pub target: Option<Vec2>,
    pub distance_m: Option<f32>,
    /// The route's graph nodes from the car to the target ([`NavGraph`] indices; ui/worldmap's GPS line and chevrons).
    pub path: Vec<u32>,
    /// Route on past the target (engine x, z), drawn after the car -> target line: a race's road path through its next
    /// few gates (race.rs `race_nav`). Empty = none. Bump `beyond_gen` when it changes so the route is redrawn.
    pub beyond: Vec<Vec2>,
    pub beyond_gen: u32,
}

/// Colorado's road network shared with the world map (ui/worldmap.rs): the satnav graph, each node's height (engine
/// y; for the in-world route chevrons and fast travel) and the drawable roads (`Nav::roads`).
#[derive(Resource, Clone)]
pub struct NavGraph {
    pub graph: Arc<fh1_ui::nav::Graph>,
    pub heights: Arc<Vec<f32>>,
    pub roads: Arc<Vec<(String, Vec<[f32; 2]>)>>,
}

/// The road graph and the route mesh being rewritten.
#[derive(Resource)]
struct RouteData {
    graph: Arc<fh1_ui::nav::Graph>,
    mesh: Handle<Mesh>,
    timer: Timer,
    /// Free roam's objective until gameplay sets one (applied once per world: an in-process map change clears the
    /// satnav and Colorado gets it back, X1d).
    default_target: Option<Vec2>,
    applied_for: Option<u32>,
    /// Target of the last route: a new one (a waypoint) re-routes at once.
    routed_to: Option<Vec2>,
    /// `SatNav::beyond_gen` of the last route: a new look-ahead redraws at once.
    routed_beyond: u32,
}

/// Minimap render rate (Hz) when something changed (`FH1_MINIMAP_HZ`, 0 = every frame). The map
/// camera is a full Bevy view (visibility over every mesh entity + its own passes), so it renders
/// only at this rate and only when the car moved/turned or the route/icons changed; an inactive
/// camera skips visibility and extraction and its target keeps the last image.
const MINIMAP_HZ: f32 = 20.0;
/// Seconds per `FH1_MINIMAP_AB=1` mode.
const AB_SECS: f32 = 5.0;
/// A/B modes: map camera off / every frame / paced.
const AB_MODES: [&str; 3] = ["off", "every_frame", "paced"];

/// Map camera pacing (and the optional in-process A/B).
#[derive(Resource)]
struct MapPace {
    hz: f32,
    acc: f32,
    /// Car (x, z) and map heading at the last render.
    last: Option<(Vec2, f32)>,
    /// Route or icons changed since the last render.
    dirty: bool,
    since_start: f32,
    ab: bool,
    mode: usize,
    mode_t: f32,
    frames: Vec<Vec<f32>>,
    renders: Vec<u32>,
}

impl Default for MapPace {
    fn default() -> Self {
        let hz = std::env::var("FH1_MINIMAP_HZ").ok().and_then(|v| v.parse().ok()).unwrap_or(MINIMAP_HZ);
        let ab = std::env::var("FH1_MINIMAP_AB").is_ok_and(|v| v == "1");
        Self { hz, acc: 0.0, last: None, dirty: true, since_start: 0.0, ab, mode: 0, mode_t: 0.0, frames: vec![Vec::new(); AB_MODES.len()], renders: vec![0; AB_MODES.len()] }
    }
}

#[derive(Component)]
struct MinimapCamera;

/// perf/p6.rs A/B: keep the map camera off (measurement only).
pub static P6_MAP_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The player arrow (shadow / arrow), with its draw-order z.
#[derive(Component)]
struct MinimapArrow(f32);

/// The satnav destination's pin (ui/worldmap.rs waypoints and gameplay targets).
#[derive(Component)]
struct MinimapTarget;

/// A map icon: billboard facing the map camera, faded by distance from the car.
#[derive(Component)]
struct MapPoi {
    pos: Vec3,
    mat: Handle<ColorMaterial>,
    /// `map/pois.tsv` tag ("" for mission icons) and whether `missions::map` hides it (undiscovered barn find).
    tag: String,
    hidden: bool,
    /// Alpha factor of the icon's state style (Done 0.6, Locked 0.45, else 1); `pois` multiplies the distance fade by it.
    alpha: f32,
    /// A catalog event icon (`event_pois`) stands in for this `pois.tsv` event icon.
    covered: bool,
}

/// A race event icon from `progression::EventCatalog` (done / locked styled), respawned when the catalog changes.
#[derive(Component)]
struct EventPoi;

/// Badge textures (tick, padlock), the unit quad and one material per badge colour, made on first use.
#[derive(Resource, Default)]
struct MinimapBadges {
    tex: [Option<Handle<Image>>; 2],
    unit: Option<Handle<Mesh>>,
    mats: std::collections::HashMap<Badge, Handle<ColorMaterial>>,
}

impl MinimapBadges {
    fn get(&mut self, b: Badge, images: &mut Assets<Image>, meshes: &mut Assets<Mesh>, mats: &mut Assets<ColorMaterial>) -> (Handle<Mesh>, Handle<ColorMaterial>) {
        let slot = usize::from(b == Badge::Lock);
        let tex = self.tex[slot].get_or_insert_with(|| images.add(circle::badge_image(b))).clone();
        let unit = self.unit.get_or_insert_with(|| meshes.add(circle::unit_quad())).clone();
        let mat = self
            .mats
            .entry(b)
            .or_insert_with(|| {
                // Gamma-space bytes straight into the target, like `raw` in `spawn`.
                let [r, g, bl] = state::badge_rgb(b);
                mats.add(ColorMaterial { color: Color::linear_rgb(r, g, bl), texture: Some(tex), alpha_mode: AlphaMode2d::Blend, ..default() })
            })
            .clone();
        (unit, mat)
    }
}

/// One styled icon: the billboard (tinted / dimmed by `state`) with its badge as a child. `base` = RGBA as gamma values
/// (the target holds gamma-space colour). Returns the icon entity.
#[allow(clippy::too_many_arguments)]
fn spawn_styled(
    commands: &mut Commands,
    layer: &RenderLayers,
    sheet: &Handle<Image>,
    (meshes, mats, images): (&mut Assets<Mesh>, &mut Assets<ColorMaterial>, &mut Assets<Image>),
    badges: &mut MinimapBadges,
    cell: (u32, u32),
    dsize: f32,
    base: [f32; 4],
    at: Vec2,
    (ist, medal): (IconState, Option<u8>),
    tag: &str,
) -> Entity {
    let styled = state::enabled();
    let st = if styled { ist } else { IconState::Available };
    let c = state::style_rgba(base, st);
    let size = dsize * ARROW_PX / 45.0 * M_PER_PX;
    let mat = mats.add(ColorMaterial { color: Color::linear_rgba(c[0], c[1], c[2], c[3]), texture: Some(sheet.clone()), alpha_mode: AlphaMode2d::Blend, ..default() });
    let pos = Vec3::new(at.x, size * 0.5 + 1.0, at.y + order_z(200.0));
    let e = commands
        .spawn((Mesh2d(meshes.add(billboard_quad(cell, size))), MeshMaterial2d(mat.clone()), Transform::from_translation(pos), layer.clone(), MapPoi { pos, mat, tag: tag.into(), hidden: false, alpha: c[3], covered: false }))
        .id();
    if let (true, Some(b)) = (styled, state::badge_of(ist, medal)) {
        let (unit, bmat) = badges.get(b, images, meshes, mats);
        commands.spawn((Mesh2d(unit), MeshMaterial2d(bmat), Transform::from_xyz(size * 0.34, -size * 0.34, 0.05).with_scale(Vec3::splat(size * 0.5)), layer.clone(), ChildOf(e)));
    }
    e
}

/// A free-roam mission icon (`missions::map::MissionMapIcons`), respawned when its generation changes.
#[derive(Component)]
struct MissionPoi;

/// The 2D draw order of a minimap layer: the profile's sort order as a sub-millimetre shift along z (z is a ground
/// axis of the map plane; the 2D phase draws back to front by z).
fn order_z(sort: f32) -> f32 {
    sort * 1e-5
}

/// Minimap roads: profile ColourSecondary near the camera, ColourPrimary far away (ui/minimap_road.wgsl).
#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct RoadFogMaterial {
    #[uniform(0)]
    u: RoadFogUniform,
}

#[derive(Clone, Copy, ShaderType, Debug)]
struct RoadFogUniform {
    near: Vec4,
    far: Vec4,
    eye: Vec4,
    range: Vec4,
}

impl Material2d for RoadFogMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/ui/minimap_road.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }
}

/// The road materials, whose camera position is updated on the frames the map renders.
#[derive(Resource)]
struct RoadMats(Vec<Handle<RoadFogMaterial>>);

/// The map heading, smoothed as 0x82637C70 does: `vel = clamp(vel + (wrap(target − h)·5 −
/// vel·3)·dt, −3, 3)`, `h += vel·dt` (radians, rad/s). Starts snapped to the car.
#[derive(Resource, Default)]
struct HeadingSpring {
    heading: Option<f32>,
    vel: f32,
}

impl HeadingSpring {
    fn step(&mut self, target: f32, dt: f32) -> f32 {
        let Some(h) = self.heading else {
            self.heading = Some(target);
            return target;
        };
        let err = (target - h + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI;
        self.vel = ((err * 5.0 - self.vel * 3.0) * dt + self.vel).clamp(-3.0, 3.0);
        let h = h + self.vel * dt;
        self.heading = Some(h);
        h
    }
}

pub struct MinimapPlugin;

impl Plugin for MinimapPlugin {
    fn build(&self, app: &mut App) {
        let target = route_override();
        embedded_asset!(app, "minimap_road.wgsl");
        app.add_plugins(Material2dPlugin::<RoadFogMaterial>::default());
        app.init_resource::<HeadingSpring>()
            .init_resource::<MinimapBadges>()
            .init_resource::<MapPace>()
            .insert_resource(SatNav { target, ..default() })
            // Colorado's road network: the map, satnav and icons run on Colorado only (X1d; other maps have no nav data
            // yet, and the HUD hides the disc there). The map camera is switched off elsewhere.
            .add_systems(
                Update,
                (
                    follow.after(crate::sync_visuals),
                    route.run_if(resource_exists::<RouteData>),
                    mission_pois.run_if(resource_exists::<MinimapImage>),
                    event_pois.run_if(resource_exists::<MinimapImage>),
                    pois.after(follow).after(mission_pois).after(event_pois),
                    pace.after(pois).after(route),
                )
                    .run_if(on_colorado),
            )
            .add_systems(Update, park_camera.run_if(not(on_colorado)))
            .add_systems(Update, resize_target.before(pace).run_if(resource_exists::<MinimapImage>));
    }
}

/// The minimap's road network is Colorado's (`colorado.nav`).
pub fn on_colorado(track: Res<crate::track::Track>) -> bool {
    track.id == "colorado"
}

/// Off Colorado the map camera doesn't render (its target keeps the last image; the HUD hides the disc).
/// Keeps the target at the disc's on-screen pixel size; a resize re-renders the map.
fn resize_target(windows: Query<&Window, With<bevy::window::PrimaryWindow>>, target: Res<MinimapImage>, mut images: ResMut<Assets<Image>>, mut pace: ResMut<MapPace>) {
    let px = rt_pixels(windows.single().ok());
    if images.get(&target.0).is_some_and(|i| i.size() != UVec2::splat(px)) {
        if let Some(mut i) = images.get_mut(&target.0) {
            i.resize(bevy::render::render_resource::Extent3d { width: px, height: px, depth_or_array_layers: 1 });
            i.data = None;
            // Re-rendered anyway: Bevy's copy of the old contents needs COPY_SRC, which a render target lacks (the
            // 2026-10-08 validation-error crash on load).
            i.copy_on_resize = false;
            pace.dirty = true;
            info!("minimap: target {px}x{px}");
        }
    }
}

fn park_camera(mut cam: Query<&mut Camera, With<MinimapCamera>>) {
    for mut c in &mut cam {
        if c.is_active {
            c.is_active = false;
        }
    }
}

/// `FH1_ROUTE=x,z`: a satnav target in engine metres (debug).
fn route_override() -> Option<Vec2> {
    let v = std::env::var("FH1_ROUTE").ok()?;
    let mut it = v.split(',').filter_map(|x| x.trim().parse::<f32>().ok());
    Some(Vec2::new(it.next()?, it.next()?))
}

/// `map/pois.tsv` rows (`tag \t object \t source \t x \t y \t z`, collision space; format owned by
/// `fh1_ui::mappois`) as (tag, engine position).
pub(super) fn load_pois(tsv: &str) -> Vec<(String, Vec3)> {
    tsv.lines()
        .filter_map(|l| {
            let c: Vec<&str> = l.split('\t').collect();
            let f = |i: usize| c.get(i)?.parse::<f32>().ok();
            Some((c.first()?.to_string(), Vec3::new(f(3)?, f(4)?, -f(5)?)))
        })
        .collect()
}

/// Build the road meshes and the map camera (called from `ui::load_fh1_ui`); returns the render
/// target for the HUD's placeholder texture slot.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    commands: &mut Commands,
    data: &UiData,
    images: &mut Assets<Image>,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<ColorMaterial>,
    road_mats: &mut Assets<RoadFogMaterial>,
    assets: &AssetServer,
) -> Option<Handle<Image>> {
    let nav = match std::fs::read(data.dir.join("map/colorado.nav")).map_err(|e| e.to_string()).and_then(|b| fh1_ui::nav::Nav::parse(&b).map_err(|e| e.to_string())) {
        Ok(n) => n,
        Err(e) => {
            warn!("minimap: no road network ({e}); run fh1setup to convert the `ui` group");
            return None;
        }
    };
    let roads = nav.roads();
    let graph = Arc::new(nav.graph());
    commands.insert_resource(NavGraph { graph: graph.clone(), heights: Arc::new(nav.nodes.iter().map(|n| n.pos[1]).collect()), roads: Arc::new(roads.clone()) });
    let layer = RenderLayers::layer(MAP_LAYER);
    // The render target holds gamma-space colour bytes; the HUD material decodes it like a
    // gamma-flagged texture and re-encodes, so the profile colours land on screen as authored.
    let raw = |[r, g, b]: [u8; 3]| Color::linear_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    // Shadows (line_fade_lr, sort 90): black alpha 128 in the middle fading to 0 at both sides, a
    // little wider than the road so the fade shows (GUESS width).
    let mut shadow = Ribbons::default();
    for (kind, pts) in &roads {
        let size = ROADS.iter().find(|r| r.0 == kind).map_or(14.0, |r| r.1);
        shadow.add(pts, size * M_PER_PX * 1.6, true);
    }
    let blend = |c: Color, tex: Option<Handle<Image>>| ColorMaterial { color: c, texture: tex, alpha_mode: AlphaMode2d::Blend, ..default() };
    let shadow_mat = mats.add(blend(Color::linear_rgba(0.0, 0.0, 0.0, 128.0 / 255.0), None));
    commands.spawn((Mesh2d(meshes.add(shadow.mesh())), MeshMaterial2d(shadow_mat), Transform::from_xyz(0.0, 0.09, order_z(90.0)), layer.clone()));
    // Roads: Secondary near, fading to Primary with view depth (the camera's fog carries Primary;
    // all four profile Primaries are the same grey except dirt's brown: GUESS mechanism).
    let dist = camera_distance();
    let fog_grey = raw([122, 122, 122]).to_linear().to_vec4();
    let mut road_handles = Vec::new();
    for (kind, size, _, secondary, sort) in ROADS {
        let mut rib = Ribbons::default();
        for (_, pts) in roads.iter().filter(|(k, _)| k == kind) {
            rib.add(pts, size * M_PER_PX, false);
        }
        // Primary grey with depth: Secondary at the focus, Primary at the far rim (GUESS range, as the old DistanceFog).
        let u = RoadFogUniform { near: raw(secondary).to_linear().to_vec4(), far: fog_grey, eye: Vec4::ZERO, range: Vec4::new(dist, dist + 2.0 * HALF_EXTENT, 0.0, 0.0) };
        let mat = road_mats.add(RoadFogMaterial { u });
        road_handles.push(mat.clone());
        commands.spawn((Mesh2d(meshes.add(rib.mesh())), MeshMaterial2d(mat), Transform::from_xyz(0.0, sort / 1000.0, order_z(sort)), layer.clone()));
    }
    commands.insert_resource(RoadMats(road_handles));
    let sheet: Handle<Image> = assets
        .load_builder()
        .with_settings(|s: &mut bevy::image::ImageLoaderSettings| s.is_srgb = false)
        .load(format!("ui/textures/horizon/map/icons/mapicons/{}/mapiconsheetsmall.png", data.lang.to_ascii_lowercase()));
    // Satnav route: one mesh rewritten by `route`. Drawn solid in ColourPrimary without depth
    // fade (Xenia refs keep it green far away); the profile's Secondary (alpha 0) end fade is not
    // modelled yet.
    let route_mesh = meshes.add(Ribbons::default().mesh());
    let route_mat = mats.add(blend(raw(ROUTE.1), None));
    commands.spawn((Mesh2d(route_mesh.clone()), MeshMaterial2d(route_mat), Transform::from_xyz(0.0, ROUTE.2 / 1000.0, order_z(ROUTE.2)), layer.clone()));
    // Points of interest (`map/pois.tsv`, written by the `ui` install).
    let poi_list = std::fs::read_to_string(data.dir.join("map/pois.tsv")).map(|t| load_pois(&t)).unwrap_or_default();
    let all = std::env::var("FH1_MAP_POIS").is_ok_and(|v| v == "all");
    let mut shown = 0;
    for (tag, at) in &poi_list {
        let Some(&(_, cell, dsize, free_roam)) = POI_STYLES.iter().find(|s| s.0 == tag) else { continue };
        if !(free_roam || all) {
            continue;
        }
        // DynamicSize scaled like the arrow (DynamicSize 45 = ARROW_PX): GUESS until the trace.
        let size = dsize * ARROW_PX / 45.0 * M_PER_PX;
        let mat = mats.add(blend(Color::WHITE, Some(sheet.clone())));
        // Icons draw over the roads and the route (profile sort orders 200+).
        let pos = Vec3::new(at.x, size * 0.5 + 1.0, at.z + order_z(200.0));
        commands.spawn((Mesh2d(meshes.add(billboard_quad(cell, size))), MeshMaterial2d(mat.clone()), Transform::from_translation(pos), layer.clone(), MapPoi { pos, mat, tag: tag.clone(), hidden: false, alpha: 1.0, covered: false }));
        shown += 1;
    }
    info!("minimap: {shown} of {} map POIs", poi_list.len());
    let default_target = poi_list.iter().find(|(t, _)| t == "racecentral").map(|(_, p)| Vec2::new(p.x, p.z)).unwrap_or(RACE_CENTRAL);
    // Destination pin: the profile's route end icon (2,4) in the route colour (hidden without a target).
    let pin_mat = mats.add(blend(raw(ROUTE.1), Some(sheet.clone())));
    let pin_size = 55.0 * ARROW_PX / 45.0 * M_PER_PX;
    commands.spawn((Mesh2d(meshes.add(billboard_quad((2, 4), pin_size))), MeshMaterial2d(pin_mat), Transform::from_xyz(0.0, pin_size * 0.5 + 1.2, order_z(300.0)), Visibility::Hidden, layer.clone(), MinimapTarget));
    commands.insert_resource(RouteData {
        graph,
        mesh: route_mesh,
        timer: Timer::from_seconds(0.5, TimerMode::Repeating),
        default_target: route_override().is_none().then_some(default_target),
        applied_for: None,
        routed_to: None,
        routed_beyond: 0,
    });
    // Player arrow: shadow (2,3) at alpha 220 under arrow (3,3) of the 8×8 small icon sheet.
    let size = ARROW_PX * M_PER_PX;
    for (cell, alpha, y) in [((2, 3), 220.0 / 255.0, 1.0), ((3, 3), 1.0, 1.1)] {
        let mat = mats.add(blend(Color::linear_rgba(1.0, 1.0, 1.0, alpha), Some(sheet.clone())));
        let z = order_z(400.0 + y);
        commands.spawn((Mesh2d(meshes.add(icon_quad(cell, size))), MeshMaterial2d(mat), Transform::from_xyz(0.0, y, z), layer.clone(), MinimapArrow(z)));
    }
    // Sized by resize_target on the first frame (the window is not known here).
    let mut target = Image::new_target_texture(RT_SIZE, RT_SIZE, TextureFormat::Rgba8Unorm, None);
    target.data = None;
    target.sampler = bevy::image::ImageSampler::linear();
    let image = images.add(target);
    commands.insert_resource(MinimapImage(image.clone()));
    let msaa = if std::env::var("FH1_MINIMAP_MSAA").is_ok_and(|v| v == "0") { Msaa::Off } else { Msaa::Sample4 };
    commands.spawn((
        MinimapCamera,
        // Marked as a UI camera so systems that look for the main 3D camera skip it.
        UiCamera,
        // A 2D view under a tilted perspective: the cheap 2D graph (no cascades / atmosphere / prepass). 4x MSAA on the
        // road edges (module doc); it renders at 20 Hz into its own target, so the cost is small.
        Camera2d,
        msaa,
        Camera { order: 5, clear_color: ClearColorConfig::Custom(Color::NONE), ..default() },
        RenderTarget::Image(image.clone().into()),
        Projection::Perspective(PerspectiveProjection { fov: FOV_DEG.to_radians(), aspect_ratio: 1.0, near: 1.0, far: dist * 4.0, ..default() }),
        Tonemapping::None,
        Transform::from_xyz(0.0, dist, 0.0).looking_to(Vec3::NEG_Y, Vec3::NEG_Z),
        layer,
    ));
    Some(image)
}

/// Camera distance from the focus point so that the map's half-width at the focus is
/// [`HALF_EXTENT`] (the verified 2D scale) for the guessed fov.
fn camera_distance() -> f32 {
    HALF_EXTENT / (FOV_DEG.to_radians() * 0.5).tan()
}

/// Heading-up: the camera looks at a point [`LOOK_AHEAD`] metres ahead of the car along the
/// smoothed heading, tilted [`TILT_DEG`] above the map plane; the arrow sits on the car pointing
/// along the car's own heading.
fn follow(time: Res<Time>, mut spring: ResMut<HeadingSpring>, cars: Query<&Car>, mut cam: Query<&mut Transform, (With<MinimapCamera>, Without<MinimapArrow>)>, mut arrows: Query<(&MinimapArrow, &mut Transform), Without<MinimapCamera>>) {
    let Ok(car) = cars.single() else { return };
    let v = &car.0;
    let fwd = (v.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(Vec3::NEG_Z);
    let yaw = fwd.x.atan2(fwd.z) + std::f32::consts::PI;
    let h = spring.step(yaw, time.delta_secs());
    let map_fwd = Quat::from_rotation_y(h) * Vec3::NEG_Z;
    if let Ok(mut t) = cam.single_mut() {
        let focus = Vec3::new(v.position.x, 0.0, v.position.z) + map_fwd * LOOK_AHEAD;
        let tilt = TILT_DEG.to_radians();
        let eye = focus - map_fwd * camera_distance() * tilt.cos() + Vec3::Y * camera_distance() * tilt.sin();
        *t = Transform::from_translation(eye).looking_at(focus, Vec3::Y);
    }
    for (arrow, mut t) in &mut arrows {
        t.translation.x = v.position.x;
        t.translation.z = v.position.z + arrow.0;
        t.rotation = Quat::from_rotation_y(yaw);
    }
}

/// Re-route from the car to `SatNav::target` twice a second (GUESS rate) and rewrite the route mesh.
#[allow(clippy::too_many_arguments)]
fn route(
    time: Res<Time>,
    mut data: ResMut<RouteData>,
    mut nav: ResMut<SatNav>,
    mut pace: ResMut<MapPace>,
    cars: Query<&Car>,
    mut meshes: ResMut<Assets<Mesh>>,
    generation: Res<super::world_load::WorldGeneration>,
) {
    if data.applied_for != Some(generation.0) {
        data.applied_for = Some(generation.0);
        if let Some(t) = data.default_target {
            nav.target.get_or_insert(t);
        }
        data.timer.reset();
        pace.dirty = true;
    }
    let first = data.timer.elapsed_secs() == 0.0;
    if !data.timer.tick(time.delta()).just_finished() && !first && data.routed_to == nav.target && data.routed_beyond == nav.beyond_gen {
        return;
    }
    data.routed_to = nav.target;
    data.routed_beyond = nav.beyond_gen;
    let Ok(car) = cars.single() else { return };
    let mut rib = Ribbons::default();
    nav.distance_m = None;
    nav.path.clear();
    if let Some(t) = nav.target {
        let from = [car.0.position.x, car.0.position.z];
        if let Some(nodes) = data.graph.route_nodes(from, t.into()) {
            let mut pts = vec![from];
            pts.extend(nodes.iter().map(|&i| data.graph.pos[i as usize]));
            pts.push(t.into());
            rib.add(&pts, ROUTE.0 * M_PER_PX, false);
            nav.distance_m = Some(pts.windows(2).map(|w| Vec2::from(w[0]).distance(Vec2::from(w[1]))).sum());
            nav.path = nodes;
        }
        // On past the target (a race's next few gates): drawn only, not in the distance or the GPS path.
        if nav.beyond.len() >= 2 {
            let pts: Vec<[f32; 2]> = nav.beyond.iter().map(|p| p.to_array()).collect();
            rib.add(&pts, ROUTE.0 * M_PER_PX, false);
        }
    }
    let _ = meshes.insert(data.mesh.id(), rib.mesh());
    pace.dirty = true;
}

/// Free-roam mission icons (`missions::map`): when the icon set changes, re-evaluate which `pois.tsv` icons it hides and
/// respawn the mission icons (outposts, speed traps, barn rumours / finds, the activity target).
#[allow(clippy::too_many_arguments)]
fn mission_pois(
    mut commands: Commands,
    mission_icons: Option<Res<crate::missions::map::MissionMapIcons>>,
    mut seen: Local<Option<u32>>,
    data: Res<UiData>,
    assets: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<ColorMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut badges: ResMut<MinimapBadges>,
    mut pace: ResMut<MapPace>,
    old: Query<Entity, With<MissionPoi>>,
    mut statics: Query<&mut MapPoi, Without<MissionPoi>>,
) {
    use crate::missions::map::IconKind;
    let Some(mi) = mission_icons else { return };
    if *seen == Some(mi.generation) {
        return;
    }
    *seen = Some(mi.generation);
    pace.dirty = true;
    for mut p in &mut statics {
        p.hidden = mi.hides(&p.tag, p.pos);
    }
    for e in &old {
        commands.entity(e).despawn();
    }
    let sheet: Handle<Image> = assets
        .load_builder()
        .with_settings(|s: &mut bevy::image::ImageLoaderSettings| s.is_srgb = false)
        .load(format!("ui/textures/horizon/map/icons/mapicons/{}/mapiconsheetsmall.png", data.lang.to_ascii_lowercase()));
    let layer = RenderLayers::layer(MAP_LAYER);
    let green = [74.0 / 255.0, 238.0 / 255.0, 97.0 / 255.0, 1.0];
    for m in mi.all() {
        // (cell, DynamicSize, tint)
        let (cell, dsize, tint) = match m.kind {
            IconKind::Outpost => ((1, 2), 45.0, [1.0; 4]),
            IconKind::SpeedCamera | IconKind::AverageSpeed => ((1, 3), 45.0, [1.0; 4]),
            IconKind::BarnHint | IconKind::BarnFound => ((0, 4), 45.0, [1.0; 4]),
            IconKind::Target => ((2, 4), 55.0, green),
            IconKind::Encounter => continue,
        };
        let e = spawn_styled(&mut commands, &layer, &sheet, (&mut meshes, &mut mats, &mut images), &mut badges, cell, dsize, tint, m.pos, (m.state, m.medal), "");
        commands.entity(e).insert(MissionPoi);
        // The barn rumour's hint circle: a flat ring + faint fill on the map plane (the tilted camera foreshortens it).
        if m.radius > 0.0 && state::enabled() {
            let mesh = meshes.add(circle::ring_mesh(m.radius, 3.0, circle::Plane::Xz, [1.0, 0.75, 0.25], 0.08));
            let cmat = mats.add(ColorMaterial { color: Color::WHITE, alpha_mode: AlphaMode2d::Blend, ..default() });
            commands.spawn((Mesh2d(mesh), MeshMaterial2d(cmat), Transform::from_xyz(m.pos.x, 0.6, m.pos.y + order_z(150.0)), layer.clone(), MissionPoi));
        }
    }
}

/// Race events (`progression::EventCatalog`) with their done / locked style. Rebuilt when the catalog changes (it bumps on
/// every profile change). The `pois.tsv` event icons within 40 m of a catalog event are covered by it. `FH1_MAP_STATES=0`
/// leaves the old `pois.tsv` icons alone.
#[allow(clippy::too_many_arguments)]
fn event_pois(
    mut commands: Commands,
    catalog: Option<Res<crate::progression::EventCatalog>>,
    mut seen: Local<Option<u32>>,
    assets: Res<AssetServer>,
    data: Res<UiData>,
    (mut meshes, mut mats, mut images): (ResMut<Assets<Mesh>>, ResMut<Assets<ColorMaterial>>, ResMut<Assets<Image>>),
    mut badges: ResMut<MinimapBadges>,
    mut pace: ResMut<MapPace>,
    old: Query<Entity, With<EventPoi>>,
    mut statics: Query<&mut MapPoi, Without<EventPoi>>,
) {
    use crate::progression::EventKind;
    let Some(cat) = catalog else { return };
    if !state::enabled() || cat.events.is_empty() || *seen == Some(cat.generation) {
        return;
    }
    *seen = Some(cat.generation);
    pace.dirty = true;
    for e in &old {
        commands.entity(e).despawn();
    }
    for mut p in &mut statics {
        p.covered = matches!(p.tag.as_str(), "race" | "exhibition" | "nemesisrace" | "streetrace")
            && cat.events.iter().any(|e| Vec2::new(e.pos.x, e.pos.z).distance(Vec2::new(p.pos.x, p.pos.z)) < 40.0);
    }
    let sheet: Handle<Image> = assets
        .load_builder()
        .with_settings(|s: &mut bevy::image::ImageLoaderSettings| s.is_srgb = false)
        .load(format!("ui/textures/horizon/map/icons/mapicons/{}/mapiconsheetsmall.png", data.lang.to_ascii_lowercase()));
    let layer = RenderLayers::layer(MAP_LAYER);
    for ev in &cat.events {
        let (cell, dsize) = match ev.kind {
            EventKind::Street => ((2, 1), 45.0),
            EventKind::Showcase => ((0, 1), 45.0),
            EventKind::Nemesis => ((4, 1), 55.0),
            _ => ((1, 0), 45.0),
        };
        let e = spawn_styled(&mut commands, &layer, &sheet, (&mut meshes, &mut mats, &mut images), &mut badges, cell, dsize, [1.0; 4], Vec2::new(ev.pos.x, ev.pos.z), state::info_state(ev), "");
        commands.entity(e).insert(EventPoi);
    }
}

/// Icons face the map camera and fade over [`POI_FADE`] from the car.
#[allow(clippy::type_complexity)]
fn pois(
    cars: Query<&Car>,
    mut pace: ResMut<MapPace>,
    nav: Res<SatNav>,
    cam: Query<&Transform, (With<MinimapCamera>, Without<MapPoi>, Without<MinimapTarget>)>,
    mut icons: Query<(&MapPoi, &mut Transform, &mut Visibility), Without<MinimapTarget>>,
    mut pin: Query<(&mut Transform, &mut Visibility), With<MinimapTarget>>,
    mut mats: ResMut<Assets<ColorMaterial>>,
) {
    let (Ok(car), Ok(cam)) = (cars.single(), cam.single()) else { return };
    if let Ok((mut t, mut vis)) = pin.single_mut() {
        let want = if nav.target.is_some() { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
            pace.dirty = true;
        }
        if let Some(p) = nav.target {
            let z = p.y + order_z(300.0);
            if (t.translation.x - p.x).abs() + (t.translation.z - z).abs() > 0.01 {
                pace.dirty = true;
            }
            t.translation.x = p.x;
            t.translation.z = z;
            t.rotation = cam.rotation;
        }
    }
    let here = Vec2::new(car.0.position.x, car.0.position.z);
    for (poi, mut t, mut vis) in &mut icons {
        let d = here.distance(Vec2::new(poi.pos.x, poi.pos.z));
        let a = ((POI_FADE.1 - d) / (POI_FADE.1 - POI_FADE.0)).clamp(0.0, 1.0);
        let a = if poi.hidden || poi.covered { 0.0 } else { a * poi.alpha };
        let want = if a > 0.0 { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
            pace.dirty = true;
        }
        if a <= 0.0 {
            continue;
        }
        t.rotation = cam.rotation;
        if mats.get(&poi.mat).is_some_and(|m| (m.color.alpha() - a).abs() > 0.01) {
            if let Some(mut m) = mats.get_mut(&poi.mat) {
                m.color.set_alpha(a);
                pace.dirty = true;
            }
        }
    }
}

/// Turn the map camera on only for the frames that need a new image (see [`MINIMAP_HZ`]).
/// `FH1_MINIMAP_AB=1` cycles [`AB_MODES`] every [`AB_SECS`] and logs the frame time per mode.
#[allow(clippy::too_many_arguments)]
fn pace(
    real: Res<Time<Real>>,
    mut p: ResMut<MapPace>,
    spring: Res<HeadingSpring>,
    cars: Query<&Car>,
    mut cam: Query<(&mut Camera, &Transform), With<MinimapCamera>>,
    roads: Option<Res<RoadMats>>,
    mut road_mats: ResMut<Assets<RoadFogMaterial>>,
) {
    let Ok((mut camera, cam_t)) = cam.single_mut() else { return };
    let dt = real.delta_secs();
    p.since_start += dt;
    p.acc += dt;
    let pose = cars.single().ok().map(|c| (Vec2::new(c.0.position.x, c.0.position.z), spring.heading.unwrap_or(0.0)));
    let moved = match (p.last, pose) {
        (Some((a, ha)), Some((b, hb))) => a.distance(b) > 0.05 || (ha - hb).abs() > 0.002,
        _ => true,
    };
    let due = p.hz <= 0.0 || p.acc >= 1.0 / p.hz;
    // The first seconds render every frame (icon sheet loading, first route).
    let mut active = p.since_start < 3.0 || (due && (moved || p.dirty));
    // perf/p6.rs in-run A/B: the map camera off.
    if P6_MAP_OFF.load(std::sync::atomic::Ordering::Relaxed) {
        active = false;
    }
    if p.ab {
        let m = p.mode;
        p.frames[m].push(dt * 1000.0);
        p.mode_t += dt;
        active = match AB_MODES[m] {
            "off" => false,
            "every_frame" => true,
            _ => active,
        };
        if active {
            p.renders[m] += 1;
        }
        if p.mode_t >= AB_SECS {
            p.mode_t = 0.0;
            p.mode = (m + 1) % AB_MODES.len();
            if p.mode == 0 {
                let line: Vec<String> = AB_MODES
                    .iter()
                    .zip(&p.frames)
                    .zip(&p.renders)
                    .map(|((name, f), r)| {
                        let mut v = f.clone();
                        v.sort_by(f32::total_cmp);
                        let mean = v.iter().sum::<f32>() / v.len().max(1) as f32;
                        let p99 = v.get(v.len() * 99 / 100).copied().unwrap_or(0.0);
                        format!("{name}: mean {mean:.2} ms p99 {p99:.2} ({} frames, {r} map renders)", v.len())
                    })
                    .collect();
                info!("minimap AB: {}", line.join(" | "));
            }
        }
    }
    if camera.is_active != active {
        camera.is_active = active;
    }
    if active {
        p.acc = 0.0;
        p.dirty = false;
        p.last = pose;
        // The road tint measures from the camera: update it only for the frames that render.
        if let Some(roads) = roads {
            for h in &roads.0 {
                if let Some(mut m) = road_mats.get_mut(h) {
                    m.u.eye = cam_t.translation.extend(0.0);
                }
            }
        }
    }
}

/// Flat ribbons (y = 0) along polylines; each segment is a quad extended by half the width at
/// both ends so joins overlap without gaps. `fade_lr` splits it along its centre line with alpha
/// 1 there and 0 at both sides (line_fade_lr).
#[derive(Default)]
struct Ribbons {
    pos: Vec<[f32; 3]>,
    col: Vec<[f32; 4]>,
    idx: Vec<u32>,
}

impl Ribbons {
    fn add(&mut self, pts: &[[f32; 2]], width: f32, fade_lr: bool) {
        let h = width / 2.0;
        for w in pts.windows(2) {
            let (a, b) = (Vec2::from(w[0]), Vec2::from(w[1]));
            let Some(d) = (b - a).try_normalize() else { continue };
            let n = Vec2::new(-d.y, d.x) * h;
            let (a, b) = (a - d * h, b + d * h);
            let base = self.pos.len() as u32;
            if fade_lr {
                // a+n, a, a−n, b−n, b, b+n
                for (p, al) in [(a + n, 0.0), (a, 1.0), (a - n, 0.0), (b - n, 0.0), (b, 1.0), (b + n, 0.0)] {
                    self.pos.push([p.x, 0.0, p.y]);
                    self.col.push([1.0, 1.0, 1.0, al]);
                }
                self.idx.extend([base, base + 1, base + 4, base, base + 4, base + 5, base + 1, base + 2, base + 3, base + 1, base + 3, base + 4]);
            } else {
                for p in [a + n, a - n, b - n, b + n] {
                    self.pos.push([p.x, 0.0, p.y]);
                    self.col.push([1.0; 4]);
                }
                self.idx.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
    }

    fn mesh(mut self) -> Mesh {
        // Never an empty vertex buffer (an empty route): one degenerate triangle.
        if self.pos.is_empty() {
            self.pos = vec![[0.0; 3]; 3];
            self.col = vec![[0.0; 4]; 3];
            self.idx = vec![0, 1, 2];
        }
        let n = self.pos.len();
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 1.0, 0.0]; n])
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.col)
            .with_inserted_indices(Indices::U32(self.idx))
    }
}

/// A quad in the XY plane (faces +Z, i.e. toward a camera when given the camera's rotation)
/// showing one cell of the 8×8 icon sheet.
fn billboard_quad((cx, cy): (u32, u32), size: f32) -> Mesh {
    let s = size / 2.0;
    let (u0, v0) = (cx as f32 / 8.0, cy as f32 / 8.0);
    let (u1, v1) = (u0 + 0.125, v0 + 0.125);
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[-s, s, 0.0], [s, s, 0.0], [s, -s, 0.0], [-s, -s, 0.0]])
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 0.0, 1.0]; 4])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[u0, v0], [u1, v0], [u1, v1], [u0, v1]])
        .with_inserted_indices(Indices::U32(vec![0, 2, 1, 0, 3, 2]))
}

/// A flat quad (y = 0) showing one cell of an 8×8 icon sheet; the icon's top points to −Z
/// (the car's forward at yaw 0).
fn icon_quad((cx, cy): (u32, u32), size: f32) -> Mesh {
    let s = size / 2.0;
    let (u0, v0) = (cx as f32 / 8.0, cy as f32 / 8.0);
    let (u1, v1) = (u0 + 0.125, v0 + 0.125);
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[-s, 0.0, -s], [s, 0.0, -s], [s, 0.0, s], [-s, 0.0, s]])
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 1.0, 0.0]; 4])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[u0, v0], [u1, v0], [u1, v1], [u0, v1]])
        .with_inserted_indices(Indices::U32(vec![0, 2, 1, 0, 3, 2]))
}
