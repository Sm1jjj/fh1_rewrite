//! The full-screen world map (pause menu "Map", or pad Back tap / Tab while driving): FH1's own Colorado map art
//! with pan/zoom, the roads (driven ones in colour, as in FH1), every event and point of interest drawn with the game's
//! own icon recipes, filters, a details card with "Set waypoint" / "Start event" / "Fast travel", and the route preview
//! before a waypoint is confirmed. Waypoints, the GPS line and the road chevrons: `worldmap/waypoint.rs`; discovered
//! roads: `worldmap/discovery.rs`.
//!
//! Map art (VERIFIED 2026-10-08): the `ui` group's `ui/textures/horizon/map/background/low-0-0.png` (2048×1229) and
//! `high-<col>-<row>.png` (5×3 tiles of 1024², row 0 at the top, same extent at 2.5× the resolution). Placement from
//! `MapProfileFullscreen.xml` group "background" (`tiles`: Size 11500, Offset x −9583.3, z −6900): the art spans
//! collision x −9583.3.. (+11500 × 2048/1229) and collision z −6900..4600, north up. The colorado.nav roads drawn with
//! this transform land on the painted roads, the festival ring and the rivers (overlay check; the same fit as
//! COLORADO_RECON's 0.1075 px/m). The game switches to the high tiles inside 3000..4000 m; here below [`HIGH_BELOW`].
//!
//! Styles from `MapProfileFullscreen.xml` (VERIFIED values, sizes in the profile's 720p units, scaled by
//! [`ui_scale`]):
//! - Roads: `line_fade_lr` black shadow + `line_coloured` dirt 9 / b, a 12 / freeway 14; ColourPrimary grey
//!   (120,120,120) = not driven yet, ColourSecondary = driven: dirt (194,120,76), b/a (200,200,200), freeway
//!   (199,182,99).
//! - Route `line_coloured` Size 14 (74,238,97) and its end icon (2,4) DynamicSize 75 in the same green.
//! - Icons: `icon_up` symbolizers = sheet cells, DynamicSize and DynamicOffset (x right, z up). Career events: shadow
//!   (0,2) at (−4,−4), glyph (1,0), tier ring (0,0) (`icon_up_career_colour`), wreath (5,1) once finished, note (6,1),
//!   tilted 3°. Street (1,1) a220 at (−3,−3) + (2,1). Exhibition (4,0) + (0,1). Nemesis 75: (3,1) + (4,1) + ring (5,0).
//!   Festival venues 35 on (0,2) with per-venue DynamicFinalOffsets (a cluster around race central). Gas (6,0) + (1,2).
//!   Barn find (0,4), speed camera (1,3). Player: (2,3) a220 at (3,−3), ring (4,4), arrow (3,3), 45. Cursor "hotspot"
//!   (3,5), 75.
//! - Animations: intro "grow", hover "bulge", locked "ghost".
//!
//! Drawing: an orthographic Camera2d (order 11, opaque clear: it covers the paused world and the HUD) on
//! [`WORLDMAP_LAYER`], active only while the page is up. Pan/zoom only move that camera; icons keep their screen size
//! by scale; ribbons are rebuilt when the zoom moved by more than [`REBUILD_ZOOM`]. The UI is bevy_ui on
//! `UiTargetCamera(map camera)`.
//!
//! Improvements over FH1 (user request): instant open, free cursor + pad snapping, filter chips (= legend), LB/RB jump
//! between icons, route preview with distance/ETA before confirming, waypoints anywhere (snapped to the nearest road),
//! waypoint kept across fast travel, fast travel and event start from the map.
//!
//! Flags: `FH1_WORLDMAP=0` = no map (the pause row stays "Fast travel"), `FH1_MAP_ROADS=0` = art only,
//! `FH1_MAP_DISCOVERY=0` = every road drawn as driven.

mod discovery;
mod waypoint;

pub use waypoint::{distance_text, Waypoint};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::OnceLock;

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ClearColorConfig, ScalingMode};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::input::gamepad::{Gamepad, GamepadButton};
use bevy::input::mouse::{AccumulatedMouseScroll, MouseScrollUnit};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::sprite_render::AlphaMode2d;
use bevy::ui::UiTargetCamera;
use bevy::window::PrimaryWindow;
use fh1_engine::vehicle::Ground;

use self::discovery::Discovered;
use super::minimap::{NavGraph, SatNav};
use super::scene::{UiCamera, UiData};
use super::UiFont;
use crate::progression::{EventCatalog, EventKind, EventState, OpenCareer, StartEvent};
use crate::track::Track;
use crate::Car;

/// Render layer of the world map's own scene (unique: memory feedback-render-layers).
pub const WORLDMAP_LAYER: usize = 9;
/// A pad Back press shorter than this opens the map; holding it longer is rewind (ui/assists.rs).
pub const BACK_TAP_S: f32 = 0.25;

/// Map art extent (collision space, metres): see the module doc.
const ART_X0: f32 = -9583.3;
const ART_Y0: f32 = -6900.0;
const ART_H: f32 = 11500.0;
const ART_W: f32 = ART_H * 2048.0 / 1229.0;
/// High-resolution tiles below this many metres per screen pixel (the low image is ~9.4 m per texel).
const HIGH_BELOW: f32 = 4.0;
/// Closest zoom (m per logical pixel) and the zoom the map opens at.
const ZOOM_MIN: f32 = 0.3;
const ZOOM_OPEN: f32 = 2.2;
/// Ribbons (roads, routes) keep their pixel width: rebuilt when the zoom changed by this factor.
const REBUILD_ZOOM: f32 = 1.1;
/// Pan speed at full stick (logical px per second).
const PAN_PX_S: f32 = 900.0;
const BG: Color = Color::srgb(0.035, 0.045, 0.055);
const ACCENT: Color = Color::srgb(0.93, 0.16, 0.48);
const DIM: Color = Color::srgba(1.0, 1.0, 1.0, 0.6);
/// Profile colours.
const ROUTE_GREEN: Color = Color::srgb(74.0 / 255.0, 238.0 / 255.0, 97.0 / 255.0);
const STREET_BLUE: Color = Color::srgb(56.0 / 255.0, 167.0 / 255.0, 1.0);
const FAST_TRAVEL_CYAN: Color = Color::srgb(0.3, 0.88, 1.0);
const ROAD_GREY: Color = Color::srgb(120.0 / 255.0, 120.0 / 255.0, 120.0 / 255.0);
/// Route-time estimate for the details card (m/s, a brisk free-roam pace).
const ETA_SPEED: f32 = 24.0;
/// Seconds of the open "grow" and the hover "bulge" factor.
const GROW_S: f32 = 0.22;
const BULGE: f32 = 1.28;

static OPEN_REQ: AtomicBool = AtomicBool::new(false);
static CLOSE_REQ: AtomicU8 = AtomicU8::new(0);
static AVAILABLE: AtomicBool = AtomicBool::new(false);

/// How the map asks the pause menu to leave its page.
pub enum Leave {
    /// B / Back / Tab / Esc: back to the pause menu (or resume, if the map was opened from driving).
    Back,
    /// Start, a fast travel or an event start: resume driving.
    Resume,
}

/// The current map has a world map (Colorado with the `ui` group installed, `FH1_WORLDMAP` not 0).
pub fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

/// ui.rs: open the map page from driving (pad Back tap / Tab).
pub fn take_open_request() -> bool {
    OPEN_REQ.swap(false, Ordering::Relaxed)
}

/// ui.rs: the map page wants to leave.
pub fn take_close_request() -> Option<Leave> {
    match CLOSE_REQ.swap(0, Ordering::Relaxed) {
        1 => Some(Leave::Back),
        2 => Some(Leave::Resume),
        _ => None,
    }
}

fn request_close(l: Leave) {
    CLOSE_REQ.store(if matches!(l, Leave::Back) { 1 } else { 2 }, Ordering::Relaxed);
}

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_WORLDMAP").map_or(true, |v| v != "0"))
}

fn roads_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_MAP_ROADS").map_or(true, |v| v != "0"))
}

/// Engine (x, z) -> map plane (x east, y north).
fn plane(p: Vec2) -> Vec2 {
    Vec2::new(p.x, -p.y)
}

/// Map plane -> engine (x, z).
fn engine(p: Vec2) -> Vec2 {
    Vec2::new(p.x, -p.y)
}

/// Logical pixels per profile unit (the profile is authored at 1280×720; ×0.7 keeps the overview readable).
fn ui_scale(win: Vec2) -> f32 {
    win.y / 720.0 * 0.7
}

/// Ribbon width factor: full width close in, thinner over the whole-map view.
fn line_scale(zoom: f32) -> f32 {
    (1.8 / zoom).clamp(0.3, 1.0)
}

/// Icon groups: filter chips and legend (label, the sheet cell shown on the chip).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cat {
    Events,
    Street,
    Specials,
    Festival,
    Fuel,
    Barns,
    Cameras,
    Travel,
}

const CATS: [(Cat, &str, (u32, u32)); 8] = [
    (Cat::Events, "Events", (1, 0)),
    (Cat::Street, "Street races", (2, 1)),
    (Cat::Specials, "Showcases & rivals", (4, 1)),
    (Cat::Festival, "Festival & shops", (4, 2)),
    (Cat::Fuel, "Gas stations", (1, 2)),
    (Cat::Barns, "Barn finds", (0, 4)),
    (Cat::Cameras, "Speed cameras", (1, 3)),
    (Cat::Travel, "Fast travel", (5, 3)),
];

fn cat_index(c: Cat) -> usize {
    CATS.iter().position(|x| x.0 == c).unwrap_or(0)
}

/// What the secondary button (X / F / right click) does on an icon.
#[derive(Clone)]
enum Action {
    None,
    /// Place the car here (engine ground point, yaw).
    Travel(Vec3, f32),
    /// Place the car on the road nearest the icon.
    TravelRoad,
    /// progression::StartEvent (teleports to the grid).
    Start(String),
}

/// One drawn layer of an icon (profile `icon_up` symbolizer): sheet cell, tint, DynamicSize, DynamicOffset (x right,
/// y up; profile units).
#[derive(Clone, Copy)]
struct Layer {
    cell: (u32, u32),
    color: Color,
    size: f32,
    off: Vec2,
}

fn layer(cell: (u32, u32), size: f32) -> Layer {
    Layer { cell, color: Color::WHITE, size, off: Vec2::ZERO }
}

impl Layer {
    fn tint(mut self, c: Color) -> Self {
        self.color = c;
        self
    }

    fn at(mut self, x: f32, y: f32) -> Self {
        self.off = Vec2::new(x, y);
        self
    }
}

struct Icon {
    key: String,
    cat: Cat,
    /// Engine (x, z).
    pos: Vec2,
    name: String,
    kind: String,
    /// Accent colour of the card (tier colour for events).
    accent: Color,
    lines: Vec<String>,
    layers: Vec<Layer>,
    /// Whole-icon offset (profile DynamicFinalOffset) and tilt (profile Angle).
    offset: Vec2,
    tilt_deg: f32,
    /// Hit size (profile units).
    size: f32,
    locked: bool,
    /// Pulses (the career's recommended event).
    pulse: bool,
    action: Action,
    root: Option<Entity>,
}

/// What the cursor is on.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hover {
    Icon(usize),
    Waypoint,
    /// A free map point (engine x, z).
    Point(Vec2),
}

enum Pending {
    Travel(Option<(Vec3, f32)>, Vec2),
    Start(String),
    Career,
}

#[derive(Resource)]
struct WorldMap {
    open: bool,
    built: bool,
    cam: Option<Entity>,
    /// Map plane centre and zoom (metres per logical pixel), and where the zoom eases to.
    center: Vec2,
    zoom: f32,
    zoom_to: f32,
    /// The cursor in the map plane; the pad cursor stays at the screen centre.
    cursor: Vec2,
    pad_mode: bool,
    /// Last mouse position (logical px) and the drag in progress (press position, dragged).
    mouse: Option<Vec2>,
    drag: Option<(Vec2, bool)>,
    icons: Vec<Icon>,
    /// Catalog generation (0 = the race::Events fallback) the icons were built from.
    icons_from: Option<u32>,
    hover: Option<Hover>,
    /// Seconds the cursor has rested on a free point (its route preview waits a moment).
    rest: f32,
    /// Seconds since the page opened (icon grow).
    opened_s: f32,
    /// Hover bulge, eased.
    bulge: f32,
    filters: [bool; CATS.len()],
    chip: usize,
    /// Route preview: target (engine) it was made for, polyline (plane) and length.
    preview: Option<(Vec2, Vec<Vec2>, f32)>,
    /// Zoom the ribbons were built at, and what the live route / driven roads were built from.
    ribbons_zoom: f32,
    live_key: Option<(Vec2, usize, Option<u32>, u32)>,
    /// LB/RB icon cycling position.
    cycle: usize,
    panel_dirty: bool,
    /// Frames to ignore input after opening (the button that opened the page).
    skip: u8,
    meshes: HashMap<(u32, u32), Handle<Mesh>>,
    mats: HashMap<[u8; 4], Handle<ColorMaterial>>,
    /// Road meshes: fade shadow, undriven grey, then driven dirt / b+a / freeway.
    road_meshes: Vec<Handle<Mesh>>,
    live_mesh: Option<(Handle<Mesh>, Handle<Mesh>)>,
    preview_mesh: Option<Handle<Mesh>>,
    art: Vec<(Entity, bool)>,
    /// A map action for `act`.
    pending: Option<Pending>,
}

impl Default for WorldMap {
    fn default() -> Self {
        Self {
            open: false,
            built: false,
            cam: None,
            center: Vec2::ZERO,
            zoom: ZOOM_OPEN,
            zoom_to: ZOOM_OPEN,
            cursor: Vec2::ZERO,
            pad_mode: true,
            mouse: None,
            drag: None,
            icons: Vec::new(),
            icons_from: None,
            hover: None,
            rest: 0.0,
            opened_s: 0.0,
            bulge: 1.0,
            filters: [true; CATS.len()],
            chip: 0,
            preview: None,
            ribbons_zoom: 0.0,
            live_key: None,
            cycle: 0,
            panel_dirty: true,
            skip: 0,
            meshes: HashMap::new(),
            mats: HashMap::new(),
            road_meshes: Vec::new(),
            live_mesh: None,
            preview_mesh: None,
            art: Vec::new(),
            pending: None,
        }
    }
}

#[derive(Component)]
struct MapCamera;
#[derive(Component)]
struct MapUiRoot;
/// Icon root: its index in `WorldMap::icons`.
#[derive(Component)]
struct MapIconRoot(usize);
#[derive(Component, Clone, Copy, PartialEq)]
enum Marker {
    Player,
    PlayerShadow,
    PlayerRing,
    Waypoint,
    Cursor,
}
#[derive(Component)]
struct Chip(usize);
#[derive(Component)]
struct ChipIcon(usize);
#[derive(Component)]
struct ChipText(usize);
#[derive(Component)]
struct Card;
#[derive(Component)]
struct HintText;
#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct CareerButton;

pub struct WorldMapPlugin;

impl Plugin for WorldMapPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldMap>()
            .add_systems(Update, (availability, hotkeys).chain())
            .add_systems(Update, (sync_open, input, act, draw, panel, sync_hdr).chain().after(availability).run_if(resource_exists::<UiData>));
        waypoint::build(app);
        discovery::build(app);
    }
}

/// Colorado with the map art installed.
fn availability(track: Res<Track>, data: Option<Res<UiData>>, mut art: Local<Option<bool>>) {
    if data.is_none() {
        *art = None;
    }
    let has_art = *art.get_or_insert_with(|| data.as_ref().is_some_and(|d| d.dir.join("textures/horizon/map/background/low-0-0.png").exists()));
    AVAILABLE.store(enabled() && has_art && track.id == "colorado", Ordering::Relaxed);
}

/// Driving: Tab, or a short pad Back press (a long one is rewind), opens the map.
fn hotkeys(keys: Res<ButtonInput<KeyCode>>, pads: Query<&Gamepad>, real: Res<Time<Real>>, menu: Res<super::Menu>, rig: Res<crate::camera::CameraRig>, mut held: Local<Option<f32>>) {
    if !available() || menu.open || rig.photo || super::loading::blocking() {
        *held = None;
        return;
    }
    if keys.just_pressed(KeyCode::Tab) {
        OPEN_REQ.store(true, Ordering::Relaxed);
    }
    // Timed from a fresh press only: the press that closed the map (Back there) mustn't reopen it on release.
    let fresh = pads.iter().any(|p| p.just_pressed(GamepadButton::Select));
    let pressed = pads.iter().any(|p| p.pressed(GamepadButton::Select));
    *held = match (*held, pressed) {
        (None, true) if fresh => Some(0.0),
        (None, true) => None,
        (Some(t), true) => Some(t + real.delta_secs()),
        (Some(t), false) => {
            if t < BACK_TAP_S {
                OPEN_REQ.store(true, Ordering::Relaxed);
            }
            None
        }
        (None, false) => None,
    };
}

/// Which event list the icons come from: the catalog generation, or 0 for the race list fallback.
fn icon_source(catalog: Option<&EventCatalog>) -> u32 {
    catalog.filter(|c| !c.events.is_empty()).map_or(0, |c| c.generation.max(1))
}

/// Follow the pause menu's Map page: build on first open, (re)build the icons when the events changed, centre on the
/// car, switch the camera and UI on/off.
#[allow(clippy::too_many_arguments)]
fn sync_open(
    mut commands: Commands,
    menu: Res<super::Menu>,
    mut st: ResMut<WorldMap>,
    (data, font, assets): (Res<UiData>, Res<UiFont>, Res<AssetServer>),
    graph: Option<Res<NavGraph>>,
    (track, events, catalog): (Res<Track>, Option<Res<crate::race::Events>>, Option<Res<EventCatalog>>),
    (mut meshes, mut mats): (ResMut<Assets<Mesh>>, ResMut<Assets<ColorMaterial>>),
    cars: Query<&Car>,
    mut cams: Query<&mut Camera, With<MapCamera>>,
    mut ui: Query<&mut Visibility, With<MapUiRoot>>,
    disc: Option<ResMut<Discovered>>,
    real: Res<Time<Real>>,
) {
    let want = menu.map_open();
    if want && st.open {
        st.opened_s += real.delta_secs();
        // The events changed while the page is up (a catalog refresh): rebuild without the grow.
        let from = icon_source(catalog.as_deref());
        if st.icons_from != Some(from) {
            rebuild_icons(&mut commands, &mut st, &data, events.as_deref(), catalog.as_deref(), &track, &assets, &mut meshes, &mut mats);
            st.icons_from = Some(from);
            st.panel_dirty = true;
        }
    }
    if want == st.open {
        return;
    }
    st.open = want;
    if want {
        if !st.built {
            build(&mut commands, &mut st, &data, &font, &assets, graph.as_deref(), &mut meshes, &mut mats);
        }
        let from = icon_source(catalog.as_deref());
        if st.icons_from != Some(from) {
            rebuild_icons(&mut commands, &mut st, &data, events.as_deref(), catalog.as_deref(), &track, &assets, &mut meshes, &mut mats);
            st.icons_from = Some(from);
        }
        if let Ok(car) = cars.single() {
            st.center = plane(Vec2::new(car.0.position.x, car.0.position.z));
        }
        st.cursor = st.center;
        st.hover = None;
        st.preview = None;
        st.pad_mode = true;
        st.drag = None;
        st.skip = 2;
        st.opened_s = 0.0;
        st.panel_dirty = true;
        st.live_key = None;
    } else if let Some(mut d) = disc {
        d.save();
    }
    for mut c in &mut cams {
        c.is_active = want;
    }
    for mut v in &mut ui {
        *v = if want { Visibility::Inherited } else { Visibility::Hidden };
    }
}

/// Unit quad (side 1, centred) showing one sheet cell.
fn cell_quad((cx, cy): (u32, u32)) -> Mesh {
    let (u0, v0) = (cx as f32 / 8.0, cy as f32 / 8.0);
    let (u1, v1) = (u0 + 0.125, v0 + 0.125);
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[-0.5, 0.5, 0.0], [0.5, 0.5, 0.0], [0.5, -0.5, 0.0], [-0.5, -0.5, 0.0]])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[u0, v0], [u1, v0], [u1, v1], [u0, v1]])
        .with_inserted_indices(Indices::U32(vec![0, 2, 1, 0, 3, 2]))
}

/// A rectangle of the map plane showing a whole image.
fn image_quad(x0: f32, y0: f32, w: f32, h: f32) -> Mesh {
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[x0, y0 + h, 0.0], [x0 + w, y0 + h, 0.0], [x0 + w, y0, 0.0], [x0, y0, 0.0]])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
        .with_inserted_indices(Indices::U32(vec![0, 2, 1, 0, 3, 2]))
}

fn sheet(assets: &AssetServer) -> Handle<Image> {
    // The cells we use carry no text; `en` is always installed.
    assets.load("ui/textures/horizon/map/icons/mapicons/en/mapiconsheet.png")
}

fn color_key(c: Color) -> [u8; 4] {
    c.to_srgba().to_u8_array()
}

impl WorldMap {
    fn cell_mesh(&mut self, meshes: &mut Assets<Mesh>, cell: (u32, u32)) -> Handle<Mesh> {
        self.meshes.entry(cell).or_insert_with(|| meshes.add(cell_quad(cell))).clone()
    }

    fn icon_mat(&mut self, mats: &mut Assets<ColorMaterial>, tex: &Handle<Image>, c: Color) -> Handle<ColorMaterial> {
        self.mats.entry(color_key(c)).or_insert_with(|| mats.add(ColorMaterial { color: c, texture: Some(tex.clone()), alpha_mode: AlphaMode2d::Blend, ..default() })).clone()
    }
}

/// Camera, art, road/route meshes, markers and the UI (once).
#[allow(clippy::too_many_arguments)]
fn build(commands: &mut Commands, st: &mut WorldMap, data: &UiData, font: &UiFont, assets: &AssetServer, graph: Option<&NavGraph>, meshes: &mut Assets<Mesh>, mats: &mut Assets<ColorMaterial>) {
    st.built = true;
    let render = RenderLayers::layer(WORLDMAP_LAYER);
    let cam = commands
        .spawn((
            MapCamera,
            // Systems looking for the main 3D camera skip UI cameras.
            UiCamera,
            Camera2d,
            Camera { order: 11, clear_color: ClearColorConfig::Custom(BG), is_active: false, ..default() },
            Projection::Orthographic(OrthographicProjection { scaling_mode: ScalingMode::FixedVertical { viewport_height: 1000.0 }, ..OrthographicProjection::default_2d() }),
            Tonemapping::None,
            bevy::core_pipeline::tonemapping::DebandDither::Enabled,
            Transform::default(),
            render.clone(),
        ))
        .id();
    st.cam = Some(cam);
    // Art: the low image always, the high tiles on top when zoomed in.
    let dir = "ui/textures/horizon/map/background";
    let low = assets.load(format!("{dir}/low-0-0.png"));
    let mat = mats.add(ColorMaterial { color: Color::WHITE, texture: Some(low), ..default() });
    let e = commands.spawn((Mesh2d(meshes.add(image_quad(ART_X0, ART_Y0, ART_W, ART_H))), MeshMaterial2d(mat), Transform::from_xyz(0.0, 0.0, 0.0), render.clone())).id();
    st.art.push((e, false));
    let (tw, th) = (ART_W / 5.0, ART_H / 3.0);
    for col in 0..5 {
        for row in 0..3 {
            if !data.dir.join(format!("textures/horizon/map/background/high-{col}-{row}.png")).exists() {
                continue;
            }
            let img = assets.load(format!("{dir}/high-{col}-{row}.png"));
            let mat = mats.add(ColorMaterial { color: Color::WHITE, texture: Some(img), ..default() });
            let (x0, y0) = (ART_X0 + col as f32 * tw, ART_Y0 + ART_H - (row + 1) as f32 * th);
            let e = commands.spawn((Mesh2d(meshes.add(image_quad(x0, y0, tw, th))), MeshMaterial2d(mat), Transform::from_xyz(0.0, 0.0, 1.0), Visibility::Hidden, render.clone())).id();
            st.art.push((e, true));
        }
    }
    // Roads (built at the current zoom by `draw`): shadow, undriven grey, driven dirt / b+a / freeway.
    if graph.is_some() && roads_on() {
        let colors = [
            (Color::srgba(0.0, 0.0, 0.0, 0.55), 2.0),
            (ROAD_GREY, 2.1),
            (Color::srgb_u8(194, 120, 76), 2.2),
            (Color::srgb_u8(200, 200, 200), 2.2),
            (Color::srgb_u8(199, 182, 99), 2.3),
        ];
        for (c, z) in colors {
            let mesh = meshes.add(Rib2::default().mesh());
            let mat = mats.add(ColorMaterial { color: c, alpha_mode: AlphaMode2d::Blend, ..default() });
            commands.spawn((Mesh2d(mesh.clone()), MeshMaterial2d(mat), Transform::from_xyz(0.0, 0.0, z), render.clone()));
            st.road_meshes.push(mesh);
        }
    }
    // Live route (fade shadow + green), preview (white).
    let outline = meshes.add(Rib2::default().mesh());
    let route = meshes.add(Rib2::default().mesh());
    let preview = meshes.add(Rib2::default().mesh());
    for (m, c, z) in [(&outline, Color::srgba(0.0, 0.0, 0.0, 0.7), 4.0), (&route, ROUTE_GREEN, 4.1), (&preview, Color::srgba(1.0, 1.0, 1.0, 0.9), 4.2)] {
        let mat = mats.add(ColorMaterial { color: c, alpha_mode: AlphaMode2d::Blend, ..default() });
        commands.spawn((Mesh2d(m.clone()), MeshMaterial2d(mat), Transform::from_xyz(0.0, 0.0, z), render.clone()));
    }
    st.live_mesh = Some((outline, route));
    st.preview_mesh = Some(preview);
    // Markers (profile vehicle_player_local, the route's end icon, the hotspot cursor); the child carries the offset.
    let tex = sheet(assets);
    for (m, l, z) in [
        (Marker::PlayerShadow, layer((2, 3), 45.0).tint(Color::srgba(1.0, 1.0, 1.0, 220.0 / 255.0)).at(3.0, -3.0), 30.0),
        (Marker::PlayerRing, layer((4, 4), 45.0), 30.1),
        (Marker::Player, layer((3, 3), 45.0), 30.2),
        (Marker::Waypoint, layer((2, 4), 75.0).tint(ROUTE_GREEN), 29.0),
        (Marker::Cursor, layer((3, 5), 75.0), 35.0),
    ] {
        let mesh = st.cell_mesh(meshes, l.cell);
        let mat = st.icon_mat(mats, &tex, l.color);
        let root = commands.spawn((m, Transform::from_xyz(0.0, 0.0, z), Visibility::Hidden, render.clone())).id();
        commands.spawn((Mesh2d(mesh), MeshMaterial2d(mat), Transform::from_xyz(l.off.x, l.off.y, 0.0).with_scale(Vec3::splat(l.size)), render.clone(), ChildOf(root)));
    }
    spawn_ui(commands, cam, font, &tex);
}

fn spawn_ui(commands: &mut Commands, cam: Entity, font: &UiFont, tex: &Handle<Image>) {
    let panel = Color::srgba(0.03, 0.04, 0.05, 0.8);
    commands
        .spawn((MapUiRoot, UiTargetCamera(cam), Node { width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() }, Visibility::Hidden))
        .with_children(|p| {
            // Title, top left.
            p.spawn(Node { position_type: PositionType::Absolute, left: Val::Px(28.0), top: Val::Px(18.0), flex_direction: FlexDirection::Column, ..default() }).with_children(|p| {
                p.spawn((Text::new("WORLD MAP"), font.text(34.0), TextColor(Color::WHITE)));
                p.spawn((Text::new("COLORADO"), font.text(14.0), TextColor(ACCENT)));
            });
            // Waypoint status + Career button, top right.
            p.spawn(Node { position_type: PositionType::Absolute, right: Val::Px(28.0), top: Val::Px(22.0), flex_direction: FlexDirection::Column, align_items: AlignItems::End, row_gap: Val::Px(6.0), ..default() }).with_children(|p| {
                p.spawn((Node { padding: UiRect::axes(Val::Px(12.0), Val::Px(6.0)), border_radius: BorderRadius::all(Val::Px(5.0)), ..default() }, BackgroundColor(panel))).with_children(|p| {
                    p.spawn((StatusText, Text::new(""), font.text(16.0), TextColor(Color::WHITE)));
                });
                p.spawn((
                    CareerButton,
                    Button,
                    Interaction::default(),
                    Node { padding: UiRect::axes(Val::Px(12.0), Val::Px(5.0)), border: UiRect::all(Val::Px(1.0)), border_radius: BorderRadius::all(Val::Px(5.0)), ..default() },
                    BackgroundColor(panel),
                    BorderColor::all(ACCENT),
                ))
                .with_children(|p| {
                    p.spawn((Text::new("CAREER  ·  LS / K"), font.text(14.0), TextColor(Color::WHITE)));
                });
            });
            // Filter chips (= legend), top centre.
            p.spawn(Node { position_type: PositionType::Absolute, top: Val::Px(84.0), width: Val::Percent(100.0), justify_content: JustifyContent::Center, ..default() }).with_children(|p| {
                p.spawn(Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(8.0), row_gap: Val::Px(6.0), flex_wrap: FlexWrap::Wrap, max_width: Val::Percent(92.0), justify_content: JustifyContent::Center, ..default() }).with_children(|p| {
                    for (i, (_, label, cell)) in CATS.iter().enumerate() {
                        p.spawn((
                            Chip(i),
                            Button,
                            Interaction::default(),
                            Node {
                                flex_direction: FlexDirection::Row,
                                align_items: AlignItems::Center,
                                column_gap: Val::Px(6.0),
                                padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                                border: UiRect::all(Val::Px(2.0)),
                                border_radius: BorderRadius::all(Val::Px(14.0)),
                                ..default()
                            },
                            BackgroundColor(panel),
                            BorderColor::all(Color::NONE),
                        ))
                        .with_children(|p| {
                            let (cx, cy) = (cell.0 as f32 * 128.0, cell.1 as f32 * 128.0);
                            p.spawn((ChipIcon(i), Node { width: Val::Px(24.0), height: Val::Px(24.0), ..default() }, ImageNode { image: tex.clone(), rect: Some(Rect::new(cx, cy, cx + 128.0, cy + 128.0)), ..default() }));
                            p.spawn((ChipText(i), Text::new(format!("{}  {label}", i + 1)), font.text(14.0), TextColor(Color::WHITE)));
                        });
                    }
                });
            });
            // Details card, bottom right (filled by `panel`).
            p.spawn((
                Card,
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(28.0),
                    bottom: Val::Px(64.0),
                    width: Val::Px(390.0),
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(4.0),
                    padding: UiRect::all(Val::Px(14.0)),
                    border: UiRect::left(Val::Px(5.0)),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    ..default()
                },
                BackgroundColor(panel),
                BorderColor::all(ACCENT),
                Visibility::Hidden,
            ));
            // Controls, bottom left.
            p.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(28.0),
                    bottom: Val::Px(22.0),
                    // Wraps instead of running under the details card on narrow windows (16:10 at 1280).
                    max_width: Val::Percent(55.0),
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(6.0)),
                    border_radius: BorderRadius::all(Val::Px(5.0)),
                    ..default()
                },
                BackgroundColor(panel),
            ))
            .with_children(|p| {
                p.spawn((HintText, Text::new(""), font.text(14.0), TextColor(DIM)));
            });
        });
}

/// Profile recipes for the event icons (group, layers, hit size, tilt in degrees). `ring` = the tier colour
/// (`icon_up_career_colour`).
fn event_layers(kind: EventKind, circuit: bool, ring: Color, completed: bool, recommended: bool) -> (Cat, Vec<Layer>, f32, f32) {
    let shadow_a = Color::srgba(1.0, 1.0, 1.0, 220.0 / 255.0);
    let (cat, mut l, size) = match kind {
        EventKind::Street => (Cat::Street, vec![layer((1, 1), 45.0).tint(shadow_a).at(-3.0, -3.0), layer((2, 1), 45.0)], 45.0),
        EventKind::Showcase => (Cat::Specials, vec![layer((4, 0), 45.0).at(-4.0, -4.0), layer((0, 1), 45.0)], 45.0),
        EventKind::Nemesis => (Cat::Specials, vec![layer((3, 1), 75.0).at(-4.0, -4.0), layer((4, 1), 75.0), layer((5, 0), 75.0).tint(ring)], 75.0),
        EventKind::Headline => (Cat::Specials, vec![layer((0, 2), 45.0).at(-4.0, -4.0), layer((7, 1), 45.0), layer((0, 0), 45.0).tint(ring)], 45.0),
        _ => {
            let glyph = if circuit { (2, 0) } else { (1, 0) };
            (Cat::Events, vec![layer((0, 2), 45.0).at(-4.0, -4.0), layer(glyph, 45.0), layer((0, 0), 45.0).tint(ring)], 45.0)
        }
    };
    if completed {
        l.push(layer((5, 1), size));
    }
    if recommended {
        l.push(layer((6, 1), 45.0).at(size * 0.42, size * 0.42));
    }
    (cat, l, size, if kind == EventKind::Nemesis { 0.0 } else { 3.0 })
}

/// Replace the icon set (events changed or first open).
#[allow(clippy::too_many_arguments)]
fn rebuild_icons(
    commands: &mut Commands,
    st: &mut WorldMap,
    data: &UiData,
    events: Option<&crate::race::Events>,
    catalog: Option<&EventCatalog>,
    track: &Track,
    assets: &AssetServer,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<ColorMaterial>,
) {
    for ic in st.icons.drain(..) {
        if let Some(r) = ic.root {
            commands.entity(r).despawn();
        }
    }
    st.hover = None;
    st.icons = collect_icons(data, events, catalog, track);
    spawn_icons(commands, st, assets, meshes, mats);
}

/// Events (progression catalog, else the race list), points of interest and fast-travel points.
fn collect_icons(data: &UiData, events: Option<&crate::race::Events>, catalog: Option<&EventCatalog>, track: &Track) -> Vec<Icon> {
    let mut out = Vec::new();
    let races = events.map_or(&[][..], |e| &e.races[..]);
    match catalog.filter(|c| !c.events.is_empty()) {
        Some(cat) => {
            for e in &cat.events {
                let circuit = races.get(e.race).is_some_and(|r| r.circuit) || e.kind == EventKind::Circuit;
                let accent = if e.kind == EventKind::Street { STREET_BLUE } else { crate::progression::tier_color(e.tier) };
                let completed = matches!(e.state, EventState::Completed { .. });
                let locked = e.state == EventState::Locked;
                let (group, layers, size, tilt) = event_layers(e.kind, circuit, accent, completed, e.recommended);
                let mut lines = Vec::new();
                if e.kind != EventKind::Street {
                    lines.push(format!("{} wristband", crate::progression::TIER_NAMES[(e.tier as usize).min(6)]));
                }
                if let Some(c) = &e.class {
                    lines.push(format!("Class  {c}"));
                }
                match (e.laps, e.length_m) {
                    (Some(l), Some(m)) if l > 1 => lines.push(format!("{l} laps  ·  {} per lap", distance_text(m, true))),
                    (_, Some(m)) => lines.push(distance_text(m, true)),
                    _ => {}
                }
                if let Some(r) = e.reward {
                    lines.push(format!("Reward  {r} CR"));
                }
                match e.state {
                    EventState::Completed { place } => lines.push(format!("Best finish  {}", ordinal(place))),
                    EventState::Locked => lines.push(format!("Locked  ·  {}", e.lock_reason.clone().unwrap_or_else(|| "not yet available".into()))),
                    EventState::Unlocked => {}
                }
                if e.recommended {
                    lines.push("Recommended next event".into());
                }
                out.push(Icon {
                    key: format!("event:{}", e.id),
                    cat: group,
                    pos: Vec2::new(e.pos.x, e.pos.z),
                    name: e.name.clone(),
                    kind: kind_name(e.kind).into(),
                    accent,
                    lines,
                    layers,
                    offset: Vec2::ZERO,
                    tilt_deg: tilt,
                    size,
                    locked,
                    pulse: e.recommended,
                    action: if locked { Action::None } else { Action::Start(e.id.clone()) },
                    root: None,
                });
            }
        }
        None => {
            for r in races {
                let k = r.kind.to_ascii_lowercase();
                let kind = if k.contains("street") {
                    EventKind::Street
                } else if k.contains("nemesis") || k.contains("rival") {
                    EventKind::Nemesis
                } else if k.contains("showcase") || k.contains("exhibition") {
                    EventKind::Showcase
                } else if r.circuit {
                    EventKind::Circuit
                } else {
                    EventKind::Sprint
                };
                let (group, layers, size, tilt) = event_layers(kind, r.circuit, Color::WHITE, false, false);
                let mut lines = Vec::new();
                let len = distance_text(r.length_m, true);
                lines.push(if r.circuit && r.laps > 1 { format!("{} laps  ·  {len} per lap", r.laps) } else { len });
                if r.credits > 0 {
                    lines.push(format!("Reward  {} CR", r.credits));
                }
                out.push(Icon {
                    key: format!("event:{}", r.horizon_id),
                    cat: group,
                    pos: Vec2::new(r.marker.0.x, r.marker.0.z),
                    name: r.name.clone(),
                    kind: r.kind.clone(),
                    accent: ACCENT,
                    lines,
                    layers,
                    offset: Vec2::ZERO,
                    tilt_deg: tilt,
                    size,
                    locked: false,
                    pulse: false,
                    action: Action::Travel(r.marker.0, r.marker.1),
                    root: None,
                });
            }
        }
    }
    // Points of interest (map/pois.tsv). Their event tags duplicate the events above (used only without events).
    let pois = std::fs::read_to_string(data.dir.join("map/pois.tsv")).map(|t| super::minimap::load_pois(&t)).unwrap_or_default();
    let no_events = out.is_empty();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let venue = |glyph: (u32, u32)| vec![layer((0, 2), 35.0), layer(glyph, 35.0)];
    for (tag, at) in &pois {
        let n = seen.entry(tag.as_str()).or_insert(0);
        *n += 1;
        // (group, name, kind, layers, final offset, action)
        let (cat, name, kind, layers, offset, action): (Cat, &str, &str, Vec<Layer>, Vec2, Action) = match tag.as_str() {
            "racecentral" => (Cat::Festival, "Horizon Festival", "Race Central", venue((4, 2)), Vec2::new(15.0, 0.0), Action::TravelRoad),
            "workshop" => (Cat::Festival, "Workshop", "Upgrades & tuning", venue((3, 2)), Vec2::new(-5.0, 15.0), Action::TravelRoad),
            "autoshow" => (Cat::Festival, "Autoshow", "Buy cars", venue((6, 2)), Vec2::new(5.0, 15.0), Action::TravelRoad),
            "paintshop" => (Cat::Festival, "Paint Shop", "Paint & liveries", venue((2, 2)), Vec2::new(-15.0, 0.0), Action::TravelRoad),
            "carclub" => (Cat::Festival, "Car Club", "Car club", venue((5, 2)), Vec2::new(10.0, -15.0), Action::TravelRoad),
            "dlccenter" => (Cat::Festival, "DLC Centre", "Downloadable content", venue((7, 2)), Vec2::new(3.0, -20.0), Action::TravelRoad),
            "gas_station" => (Cat::Fuel, "Gas Station", "Gas station", vec![layer((6, 0), 45.0).at(-4.0, -4.0), layer((1, 2), 45.0)], Vec2::ZERO, Action::TravelRoad),
            "barnfind" => (Cat::Barns, "Barn Find", "Rumoured barn find", vec![layer((0, 4), 45.0)], Vec2::ZERO, Action::None),
            "speed_camera" => (Cat::Cameras, "Speed Camera", "Speed camera", vec![layer((1, 3), 45.0)], Vec2::ZERO, Action::None),
            "race" | "exhibition" | "nemesisrace" | "streetrace" if no_events => {
                let kind = match tag.as_str() {
                    "streetrace" => EventKind::Street,
                    "nemesisrace" => EventKind::Nemesis,
                    "exhibition" => EventKind::Showcase,
                    _ => EventKind::Sprint,
                };
                let (cat, l, _, _) = event_layers(kind, false, Color::WHITE, false, false);
                (cat, "Event", kind_name(kind), l, Vec2::ZERO, Action::TravelRoad)
            }
            _ => continue,
        };
        let size = layers.iter().map(|l| l.size).fold(0.0, f32::max);
        out.push(Icon {
            key: format!("poi:{tag}:{n}"),
            cat,
            pos: Vec2::new(at.x, at.z),
            name: name.into(),
            kind: kind.into(),
            accent: ACCENT,
            lines: Vec::new(),
            layers,
            offset,
            tilt_deg: 0.0,
            size,
            locked: false,
            pulse: false,
            action,
            root: None,
        });
    }
    // Fast-travel points: the track's start locations (styled like the profile's vehicle dots).
    for (i, ((p, yaw), name)) in track.spawns.iter().zip(&track.spawn_names).enumerate() {
        out.push(Icon {
            key: format!("travel:{i}"),
            cat: Cat::Travel,
            pos: Vec2::new(p.x, p.z),
            name: name.clone(),
            kind: "Fast travel point".into(),
            accent: FAST_TRAVEL_CYAN,
            lines: Vec::new(),
            layers: vec![layer((4, 3), 28.0).tint(Color::srgba(1.0, 1.0, 1.0, 0.86)).at(-2.0, -2.0), layer((5, 3), 28.0).tint(FAST_TRAVEL_CYAN)],
            offset: Vec2::ZERO,
            tilt_deg: 0.0,
            size: 28.0,
            locked: false,
            pulse: false,
            action: Action::Travel(*p, *yaw),
            root: None,
        });
    }
    out
}

fn kind_name(k: EventKind) -> &'static str {
    match k {
        EventKind::Circuit => "Circuit race",
        EventKind::Sprint => "Sprint race",
        EventKind::Street => "Street race",
        EventKind::Drag => "Drag race",
        EventKind::Elimination => "Elimination",
        EventKind::Showcase => "Showcase",
        EventKind::Headline => "Headline event",
        EventKind::Nemesis => "Rival race",
        EventKind::PrStunt => "PR stunt",
        EventKind::Other => "Event",
    }
}

fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn cat_z(c: Cat) -> f32 {
    match c {
        Cat::Travel => 8.0,
        Cat::Cameras => 9.0,
        Cat::Fuel | Cat::Barns => 10.0,
        Cat::Festival => 11.0,
        Cat::Events | Cat::Street => 12.0,
        Cat::Specials => 13.0,
    }
}

fn spawn_icons(commands: &mut Commands, st: &mut WorldMap, assets: &AssetServer, meshes: &mut Assets<Mesh>, mats: &mut Assets<ColorMaterial>) {
    let render = RenderLayers::layer(WORLDMAP_LAYER);
    let tex = sheet(assets);
    for i in 0..st.icons.len() {
        let (pos, cat, locked, layers, tilt) = {
            let ic = &st.icons[i];
            (plane(ic.pos), ic.cat, ic.locked, ic.layers.clone(), ic.tilt_deg)
        };
        // Later icons draw on top within a group (tiny z step).
        let z = cat_z(cat) + i as f32 * 1e-4;
        let root = commands
            .spawn((MapIconRoot(i), Transform::from_xyz(pos.x, pos.y, z).with_rotation(Quat::from_rotation_z(tilt.to_radians())), Visibility::Inherited, render.clone()))
            .id();
        for (k, l) in layers.into_iter().enumerate() {
            // Locked = the profile's "ghost": greyed and see-through.
            let color = if locked {
                let c = l.color.to_srgba();
                let g = (c.red + c.green + c.blue) / 3.0 * 0.8;
                Color::srgba(g, g, g, c.alpha * 0.5)
            } else {
                l.color
            };
            let mesh = st.cell_mesh(meshes, l.cell);
            let mat = st.icon_mat(mats, &tex, color);
            commands.spawn((Mesh2d(mesh), MeshMaterial2d(mat), Transform::from_xyz(l.off.x, l.off.y, 0.01 * k as f32).with_scale(Vec3::splat(l.size)), render.clone(), ChildOf(root)));
        }
        st.icons[i].root = Some(root);
    }
}

fn window_size(win: &Window) -> Vec2 {
    Vec2::new(win.width(), win.height()).max(Vec2::ONE)
}

fn zoom_max(win: Vec2) -> f32 {
    (ART_W / win.x).max(ART_H / win.y) * 1.05
}

/// Screen (logical px, top-left origin) -> map plane.
fn to_plane(st: &WorldMap, win: Vec2, px: Vec2) -> Vec2 {
    st.center + (px - win * 0.5) * Vec2::new(1.0, -1.0) * st.zoom
}

/// An icon's centre on the map plane (its DynamicFinalOffset applied at the current zoom).
fn icon_center(ic: &Icon, zoom: f32, s: f32) -> Vec2 {
    plane(ic.pos) + ic.offset * s * zoom
}

#[allow(clippy::too_many_arguments)]
fn input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    scroll: Res<AccumulatedMouseScroll>,
    pads: Query<&Gamepad>,
    windows: Query<&Window, With<PrimaryWindow>>,
    real: Res<Time<Real>>,
    mut st: ResMut<WorldMap>,
    (mut wp, graph): (ResMut<Waypoint>, Option<Res<NavGraph>>),
    cars: Query<&Car>,
    chips: Query<(&Interaction, &Chip), Changed<Interaction>>,
    career: Query<&Interaction, (Changed<Interaction>, With<CareerButton>)>,
    race: Option<Res<crate::race::RaceState>>,
) {
    if !st.open {
        return;
    }
    if st.skip > 0 {
        st.skip -= 1;
        return;
    }
    let Ok(win) = windows.single() else { return };
    let size = window_size(win);
    let s = ui_scale(size);
    let dt = real.delta_secs().min(0.1);
    let kp = |k: KeyCode| keys.just_pressed(k);
    let pp = |b: GamepadButton| pads.iter().any(|p| p.just_pressed(b));
    // Leave.
    if pp(GamepadButton::Start) {
        request_close(Leave::Resume);
        return;
    }
    if kp(KeyCode::Escape) || kp(KeyCode::Backspace) || kp(KeyCode::Tab) || pp(GamepadButton::East) || pp(GamepadButton::Select) {
        request_close(Leave::Back);
        return;
    }
    // Career screen (progression): LS click / K / the button.
    if pp(GamepadButton::LeftThumb) || kp(KeyCode::KeyK) || career.iter().any(|i| *i == Interaction::Pressed) {
        st.pending = Some(Pending::Career);
        return;
    }
    // Pan: pad sticks, keys.
    let mut pan = Vec2::ZERO;
    for p in &pads {
        for v in [p.left_stick(), p.right_stick() * 1.6] {
            if v.length() > 0.15 {
                pan += v * v.length();
            }
        }
    }
    let key = |k: &[KeyCode]| k.iter().any(|&k| keys.pressed(k));
    pan += Vec2::new(
        (key(&[KeyCode::KeyD, KeyCode::ArrowRight]) as i32 - key(&[KeyCode::KeyA, KeyCode::ArrowLeft]) as i32) as f32,
        (key(&[KeyCode::KeyW, KeyCode::ArrowUp]) as i32 - key(&[KeyCode::KeyS, KeyCode::ArrowDown]) as i32) as f32,
    );
    if pan != Vec2::ZERO {
        st.pad_mode = true;
        // Over an icon the pad cursor slows down (easier to stop on small icons).
        let friction = if matches!(st.hover, Some(Hover::Icon(_))) && pan.length() < 0.8 { 0.45 } else { 1.0 };
        let z = st.zoom;
        st.center += pan * PAN_PX_S * z * dt * friction;
    }
    // Zoom: triggers, Q/E, +/-, wheel (about the mouse).
    let mut zin = 0.0f32;
    for p in &pads {
        zin += p.get(GamepadButton::RightTrigger2).unwrap_or(0.0) - p.get(GamepadButton::LeftTrigger2).unwrap_or(0.0);
    }
    zin += (key(&[KeyCode::KeyE, KeyCode::Equal, KeyCode::NumpadAdd, KeyCode::PageUp]) as i32 - key(&[KeyCode::KeyQ, KeyCode::Minus, KeyCode::NumpadSubtract, KeyCode::PageDown]) as i32) as f32;
    let zmax = zoom_max(size);
    if zin.abs() > 0.05 {
        st.zoom_to = (st.zoom_to * (-zin * 2.2 * dt).exp()).clamp(ZOOM_MIN, zmax);
    }
    let lines = match scroll.unit {
        MouseScrollUnit::Line => scroll.delta.y,
        MouseScrollUnit::Pixel => scroll.delta.y / 60.0,
    };
    // Mouse: a move switches to the free cursor; drag pans; a click is the primary action.
    let mouse = win.cursor_position();
    if let (Some(m), Some(last)) = (mouse, st.mouse) {
        if m.distance(last) > 0.5 {
            st.pad_mode = false;
        }
    }
    if lines.abs() > 0.01 {
        let anchor = mouse.map(|m| to_plane(&st, size, m));
        let old = st.zoom;
        let new = (old * 0.82f32.powf(lines)).clamp(ZOOM_MIN, zmax);
        st.zoom = new;
        st.zoom_to = new;
        if let (Some(a), false) = (anchor, st.pad_mode) {
            // Keep the point under the mouse fixed.
            st.center = a + (st.center - a) * (new / old);
        }
    }
    let mut primary = kp(KeyCode::Enter) || kp(KeyCode::Space) || pp(GamepadButton::South);
    let secondary = kp(KeyCode::KeyF) || pp(GamepadButton::West) || mouse_buttons.just_pressed(MouseButton::Right);
    if let Some(m) = mouse {
        if mouse_buttons.just_pressed(MouseButton::Left) {
            st.drag = Some((m, false));
        }
        if let (Some((start, moved)), Some(last)) = (st.drag, st.mouse) {
            if mouse_buttons.pressed(MouseButton::Left) {
                let moved = moved || m.distance(start) > 5.0;
                if moved {
                    let z = st.zoom;
                    st.center -= (m - last) * Vec2::new(1.0, -1.0) * z;
                }
                st.drag = Some((start, moved));
            }
        }
        if mouse_buttons.just_released(MouseButton::Left) {
            if st.drag.is_some_and(|(_, moved)| !moved) {
                primary = true;
            }
            st.drag = None;
        }
    }
    st.mouse = mouse;
    // Filters: 1-8, D-pad left/right + Y, chip clicks.
    let digits = [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4, KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8];
    let mut toggled = None;
    for (i, d) in digits.iter().enumerate() {
        if kp(*d) {
            toggled = Some(i);
        }
    }
    if pp(GamepadButton::DPadRight) {
        st.chip = (st.chip + 1) % CATS.len();
        st.panel_dirty = true;
    }
    if pp(GamepadButton::DPadLeft) {
        st.chip = (st.chip + CATS.len() - 1) % CATS.len();
        st.panel_dirty = true;
    }
    if pp(GamepadButton::North) {
        toggled = Some(st.chip);
    }
    for (i, c) in &chips {
        if *i == Interaction::Pressed {
            toggled = Some(c.0);
            // The click was on the chip, not the map.
            primary = false;
            st.drag = None;
        }
    }
    if let Some(i) = toggled {
        st.filters[i] = !st.filters[i];
        st.chip = i;
        st.panel_dirty = true;
    }
    // Recentre on the car: right stick click / C / Home.
    let car = cars.single().ok().map(|c| plane(Vec2::new(c.0.position.x, c.0.position.z)));
    if pp(GamepadButton::RightThumb) || kp(KeyCode::KeyC) || kp(KeyCode::Home) {
        if let Some(c) = car {
            st.center = c;
            st.pad_mode = true;
        }
    }
    // LB/RB: previous/next shown icon (by distance from the car).
    let cyc = pp(GamepadButton::RightTrigger) as i32 - pp(GamepadButton::LeftTrigger) as i32 + (kp(KeyCode::BracketRight) as i32 - kp(KeyCode::BracketLeft) as i32);
    if cyc != 0 {
        let from = car.unwrap_or(st.center);
        let mut order: Vec<usize> = (0..st.icons.len()).filter(|&i| st.filters[cat_index(st.icons[i].cat)]).collect();
        order.sort_by(|&a, &b| plane(st.icons[a].pos).distance(from).total_cmp(&plane(st.icons[b].pos).distance(from)));
        if !order.is_empty() {
            st.cycle = (st.cycle as i32 + cyc).rem_euclid(order.len() as i32) as usize;
            st.center = icon_center(&st.icons[order[st.cycle]], st.zoom, s);
            st.pad_mode = true;
        }
    }
    // Zoom eases to its target (pad / keys); the wheel set both.
    let k = 1.0 - (-14.0 * dt).exp();
    st.zoom = st.zoom + (st.zoom_to - st.zoom) * k;
    // Keep the view over the art.
    let half = size * 0.5 * st.zoom;
    let (lo, hi) = (Vec2::new(ART_X0, ART_Y0), Vec2::new(ART_X0 + ART_W, ART_Y0 + ART_H));
    st.center = Vec2::new(
        if hi.x - lo.x > 2.0 * half.x { st.center.x.clamp(lo.x + half.x * 0.2, hi.x - half.x * 0.2) } else { (lo.x + hi.x) * 0.5 },
        if hi.y - lo.y > 2.0 * half.y { st.center.y.clamp(lo.y + half.y * 0.2, hi.y - half.y * 0.2) } else { (lo.y + hi.y) * 0.5 },
    );
    st.cursor = match (st.pad_mode, mouse) {
        (false, Some(m)) => to_plane(&st, size, m),
        _ => st.center,
    };
    // Hover: the nearest shown icon under the cursor (wider for the pad), else the waypoint, else the point.
    let reach = if st.pad_mode { 22.0 } else { 4.0 };
    let cursor = st.cursor;
    let zoom = st.zoom;
    let mut best: Option<(f32, usize)> = None;
    for (i, ic) in st.icons.iter().enumerate() {
        if !st.filters[cat_index(ic.cat)] {
            continue;
        }
        let d = icon_center(ic, zoom, s).distance(cursor) / zoom;
        if d < ic.size * s * 0.42 + reach && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, i));
        }
    }
    let hover = match (best, wp.target) {
        (Some((_, i)), _) => Hover::Icon(i),
        (None, Some(t)) if plane(t).distance(cursor) / zoom < 75.0 * s * 0.35 + reach => Hover::Waypoint,
        _ => Hover::Point(engine(cursor)),
    };
    // The pad cursor eases onto an icon it hovers when the stick is let go (snapping).
    if let (true, Hover::Icon(i), true) = (st.pad_mode, hover, pan == Vec2::ZERO) {
        let target = icon_center(&st.icons[i], zoom, s);
        st.center = st.center.lerp(target, 1.0 - (-12.0 * dt).exp());
    }
    let changed = match (st.hover, hover) {
        (Some(Hover::Point(a)), Hover::Point(b)) => a.distance(b) > 0.5 * zoom,
        (a, b) => a != Some(b),
    };
    if changed {
        st.panel_dirty = true;
        st.rest = 0.0;
    } else {
        st.rest += dt;
    }
    st.hover = Some(hover);
    // Route preview: at once for icons, after a short rest for free points.
    let preview_to = match hover {
        Hover::Icon(i) => Some(st.icons[i].pos),
        Hover::Point(p) if st.rest > 0.18 => Some(p),
        _ => None,
    };
    match (preview_to, &graph, car) {
        (Some(to), Some(g), Some(from)) if st.preview.as_ref().is_none_or(|p| p.0.distance(to) > 1.0) => {
            let from = engine(from);
            st.preview = g.graph.route(from.into(), to.into()).map(|(pts, len)| (to, pts.into_iter().map(|p| plane(Vec2::from(p))).collect(), len));
            st.ribbons_zoom = 0.0;
            st.panel_dirty = true;
        }
        (None, _, _) if st.preview.is_some() && (changed || !matches!(hover, Hover::Point(_))) => {
            st.preview = None;
            st.ribbons_zoom = 0.0;
            st.panel_dirty = true;
        }
        _ => {}
    }
    // Actions.
    if primary {
        match hover {
            Hover::Icon(i) => {
                let ic = &st.icons[i];
                if wp.source.as_deref() == Some(ic.key.as_str()) {
                    wp.clear();
                } else {
                    wp.set(ic.pos, ic.name.clone(), Some(ic.key.clone()));
                }
            }
            Hover::Waypoint => wp.clear(),
            Hover::Point(p) => {
                // Snap to the nearest road (as FH's waypoints do): an off-road point could never be "reached".
                let at = graph.as_ref().and_then(|g| g.graph.nearest(p.into()).map(|n| Vec2::from(g.graph.pos[n as usize]))).unwrap_or(p);
                wp.set(at, "Map location", None);
            }
        }
        st.panel_dirty = true;
    }
    // No fast travel or event start in the middle of a race (it would strand the race); waypoints are fine.
    let racing = race.as_ref().is_some_and(|r| r.owns_nav());
    if secondary && !racing {
        if let Hover::Icon(i) = hover {
            st.pending = match st.icons[i].action.clone() {
                Action::Travel(p, yaw) => Some(Pending::Travel(Some((p, yaw)), st.icons[i].pos)),
                Action::TravelRoad => Some(Pending::Travel(None, st.icons[i].pos)),
                Action::Start(id) => Some(Pending::Start(id)),
                Action::None => None,
            };
        }
    }
}

/// Carry out a fast travel / event start / career request and leave the map.
fn act(mut st: ResMut<WorldMap>, mut cars: Query<&mut Car>, graph: Option<Res<NavGraph>>, track: Res<Track>, mut start: MessageWriter<StartEvent>, mut career: MessageWriter<OpenCareer>) {
    let Some(p) = st.pending.take() else { return };
    match p {
        Pending::Start(id) => {
            start.write(StartEvent { id });
        }
        Pending::Career => {
            career.write(OpenCareer);
        }
        Pending::Travel(pose, near) => {
            let pose = pose.or_else(|| {
                // The road node nearest the icon, facing along the road, dropped onto the ground.
                let g = graph.as_ref()?;
                let n = g.graph.nearest(near.into())? as usize;
                let p = Vec2::from(g.graph.pos[n]);
                let next = g.graph.adj[n].first().map(|&(m, _)| Vec2::from(g.graph.pos[m as usize])).unwrap_or(p + Vec2::NEG_Y);
                let d = (next - p).normalize_or(Vec2::NEG_Y);
                let y = g.heights.get(n).copied().unwrap_or(0.0);
                let ground = track.ground.ray(Vec3::new(p.x, y + 4.0, p.y), Vec3::NEG_Y, 20.0).map_or(y, |h| h.point.y);
                Some((Vec3::new(p.x, ground, p.y), (-d.x).atan2(-d.y)))
            });
            let Some((point, yaw)) = pose else { return };
            for mut car in &mut cars {
                car.0.place(point, yaw);
            }
        }
    }
    request_close(Leave::Resume);
}

/// Polyline ribbons in the map plane (z = 0) with vertex alpha; `fade` = the profile's line_fade_lr (opaque centre,
/// transparent sides).
#[derive(Default)]
struct Rib2 {
    pos: Vec<[f32; 3]>,
    col: Vec<[f32; 4]>,
    idx: Vec<u32>,
}

impl Rib2 {
    fn add(&mut self, pts: &[Vec2], half: f32, fade: bool) {
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let Some(d) = (b - a).try_normalize() else { continue };
            let n = Vec2::new(-d.y, d.x) * half;
            // Overlap the joins by most of a half width (no gaps at bends, no visible caps on thin lines).
            let (a, b) = (a - d * half * 0.8, b + d * half * 0.8);
            let base = self.pos.len() as u32;
            if fade {
                for (p, al) in [(a + n, 0.0), (a, 1.0), (a - n, 0.0), (b - n, 0.0), (b, 1.0), (b + n, 0.0)] {
                    self.pos.push([p.x, p.y, 0.0]);
                    self.col.push([1.0, 1.0, 1.0, al]);
                }
                self.idx.extend([base, base + 1, base + 4, base, base + 4, base + 5, base + 1, base + 2, base + 3, base + 1, base + 3, base + 4]);
            } else {
                for p in [a + n, a - n, b - n, b + n] {
                    self.pos.push([p.x, p.y, 0.0]);
                    self.col.push([1.0; 4]);
                }
                self.idx.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
    }

    fn mesh(mut self) -> Mesh {
        if self.pos.is_empty() {
            self.pos = vec![[0.0; 3]; 3];
            self.col = vec![[0.0; 4]; 3];
            self.idx = vec![0, 1, 2];
        }
        let n = self.pos.len();
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0, 0.0]; n])
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.col)
            .with_inserted_indices(Indices::U32(self.idx))
    }
}

/// Camera, art level, icon scales and animations, markers, ribbons.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn draw(
    mut st: ResMut<WorldMap>,
    windows: Query<&Window, With<PrimaryWindow>>,
    (wp, nav, graph, disc): (Res<Waypoint>, Res<SatNav>, Option<Res<NavGraph>>, Option<Res<Discovered>>),
    cars: Query<&Car>,
    real: Res<Time<Real>>,
    mut cam: Query<(&mut Transform, &mut Projection), (With<MapCamera>, Without<MapIconRoot>, Without<Marker>)>,
    mut roots: Query<(&MapIconRoot, &mut Transform, &mut Visibility), (Without<MapCamera>, Without<Marker>)>,
    mut markers: Query<(&Marker, &mut Transform, &mut Visibility), (Without<MapCamera>, Without<MapIconRoot>)>,
    mut art_vis: Query<&mut Visibility, (Without<MapIconRoot>, Without<Marker>, Without<MapCamera>)>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    if !st.open {
        return;
    }
    let Ok(win) = windows.single() else { return };
    let size = window_size(win);
    let s = ui_scale(size);
    let zoom = st.zoom;
    let dt = real.delta_secs().min(0.1);
    let t = real.elapsed_secs();
    if let Ok((mut tr, mut proj)) = cam.single_mut() {
        tr.translation = Vec3::new(st.center.x, st.center.y, 0.0);
        if let Projection::Orthographic(o) = &mut *proj {
            let h = zoom * size.y;
            if !matches!(o.scaling_mode, ScalingMode::FixedVertical { viewport_height } if (viewport_height - h).abs() < 1e-3) {
                o.scaling_mode = ScalingMode::FixedVertical { viewport_height: h };
            }
        }
    }
    // Art level.
    let high = zoom < HIGH_BELOW;
    for &(e, is_high) in &st.art {
        if is_high {
            if let Ok(mut v) = art_vis.get_mut(e) {
                let want = if high { Visibility::Inherited } else { Visibility::Hidden };
                if *v != want {
                    *v = want;
                }
            }
        }
    }
    // Icons: constant screen size; "grow" in on open (staggered out from the centre), "bulge" on hover, the
    // recommended event pulses.
    let hovered = match st.hover {
        Some(Hover::Icon(i)) => Some(i),
        _ => None,
    };
    let b = if hovered.is_some() { BULGE } else { 1.0 };
    st.bulge += (b - st.bulge) * (1.0 - (-18.0 * dt).exp());
    let (bulge, opened, center) = (st.bulge, st.opened_s, st.center);
    for (r, mut tr, mut v) in &mut roots {
        let Some(ic) = st.icons.get(r.0) else { continue };
        let want = if st.filters[cat_index(ic.cat)] { Visibility::Inherited } else { Visibility::Hidden };
        if *v != want {
            *v = want;
        }
        let c = icon_center(ic, zoom, s);
        let delay = (c.distance(center) / zoom / size.length() * 0.25).min(0.2);
        let g = ((opened - delay) / GROW_S).clamp(0.0, 1.0);
        // Ease-out-back: a small overshoot like the game's "grow".
        let grow = 1.0 + 2.2 * (g - 1.0).powi(3) + 1.2 * (g - 1.0).powi(2);
        let mut k = grow.max(0.0);
        if Some(r.0) == hovered {
            k *= bulge;
        }
        if ic.pulse {
            k *= 1.0 + 0.1 * (t * std::f32::consts::TAU * 1.2).sin().max(0.0);
        }
        tr.translation.x = c.x;
        tr.translation.y = c.y;
        tr.scale = Vec3::splat(zoom * s * k.max(1e-3));
    }
    // Markers.
    let car = cars.single().ok();
    let pulse = 1.0 + 0.06 * (t * std::f32::consts::TAU * 1.2).sin();
    for (m, mut tr, mut v) in &mut markers {
        let (pos, scale, rot, show): (Vec2, f32, f32, bool) = match m {
            Marker::Player | Marker::PlayerShadow | Marker::PlayerRing => match car {
                Some(c) => {
                    let fwd = (c.0.rotation * Vec3::NEG_Z).reject_from(Vec3::Y);
                    let d = plane(Vec2::new(fwd.x, fwd.z));
                    let rot = if *m == Marker::PlayerRing { 0.0 } else { (-d.x).atan2(d.y) };
                    (plane(Vec2::new(c.0.position.x, c.0.position.z)), 1.0, rot, true)
                }
                None => (Vec2::ZERO, 1.0, 0.0, false),
            },
            Marker::Waypoint => (wp.target.map(plane).unwrap_or_default(), pulse * if st.hover == Some(Hover::Waypoint) { bulge.max(1.15) } else { 1.0 }, 0.0, wp.target.is_some()),
            Marker::Cursor => (st.cursor, 1.0, 0.0, st.pad_mode && !matches!(st.hover, Some(Hover::Icon(_)))),
        };
        let want = if show { Visibility::Inherited } else { Visibility::Hidden };
        if *v != want {
            *v = want;
        }
        tr.translation.x = pos.x;
        tr.translation.y = pos.y;
        tr.scale = Vec3::splat(scale * zoom * s);
        tr.rotation = Quat::from_rotation_z(rot);
    }
    // Ribbons: rebuilt at a new zoom, a new live route, newly driven roads, or a new preview (it zeroes ribbons_zoom).
    let live_key = (nav.target.unwrap_or_default(), nav.path.len(), nav.path.first().copied(), disc.as_ref().map_or(0, |d| d.version));
    let live_changed = st.live_key != Some(live_key);
    let rezoom = st.ribbons_zoom <= 0.0 || (zoom / st.ribbons_zoom).max(st.ribbons_zoom / zoom) > REBUILD_ZOOM;
    if !(rezoom || live_changed) {
        return;
    }
    st.live_key = Some(live_key);
    st.ribbons_zoom = zoom;
    // Metres per profile width unit.
    let lw = s * line_scale(zoom) * zoom;
    if let Some(g) = graph.as_ref() {
        if st.road_meshes.len() == 5 {
            // Shadow, grey (undriven), dirt, b+a, freeway (driven).
            let mut ribs: [Rib2; 5] = Default::default();
            for (k, (kind, pts)) in g.roads.iter().enumerate() {
                let (w, slot) = match kind.as_str() {
                    "freeway" => (14.0, 4),
                    "a" | "b" => (12.0, 3),
                    _ => (9.0, 2),
                };
                let pts: Vec<Vec2> = pts.iter().map(|p| plane(Vec2::from(*p))).collect();
                let half = w * 0.5 * lw;
                ribs[0].add(&pts, half * 1.9, true);
                // Runs of driven / undriven segments.
                let mut run: Vec<Vec2> = Vec::new();
                let mut run_seen = None;
                for j in 0..pts.len().saturating_sub(1) {
                    let seen = disc.as_ref().is_none_or(|d| d.is_seen(k, j));
                    if run_seen != Some(seen) {
                        if run.len() > 1 {
                            ribs[if run_seen == Some(true) { slot } else { 1 }].add(&run, half, false);
                        }
                        run.clear();
                        run.push(pts[j]);
                        run_seen = Some(seen);
                    }
                    run.push(pts[j + 1]);
                }
                if run.len() > 1 {
                    ribs[if run_seen == Some(true) { slot } else { 1 }].add(&run, half, false);
                }
            }
            let handles = st.road_meshes.clone();
            for (h, rib) in handles.iter().zip(ribs) {
                let _ = meshes.insert(h.id(), rib.mesh());
            }
        }
        // Live route: car -> satnav path -> target (profile route: Size 14, green, over a fade shadow).
        if let Some((outline, route)) = st.live_mesh.clone() {
            let mut pts = Vec::new();
            if let (Some(t), Some(c)) = (nav.target, car) {
                if !nav.path.is_empty() {
                    pts.push(plane(Vec2::new(c.0.position.x, c.0.position.z)));
                    pts.extend(nav.path.iter().map(|&n| plane(Vec2::from(g.graph.pos[n as usize]))));
                    pts.push(plane(t));
                }
            }
            let (mut a, mut b) = (Rib2::default(), Rib2::default());
            a.add(&pts, 14.0 * lw, true);
            b.add(&pts, 14.0 * 0.5 * lw, false);
            let _ = meshes.insert(outline.id(), a.mesh());
            let _ = meshes.insert(route.id(), b.mesh());
        }
    }
    if let Some(pm) = st.preview_mesh.clone() {
        let mut rib = Rib2::default();
        if let Some((_, pts, _)) = &st.preview {
            rib.add(pts, 8.0 * 0.5 * lw, false);
        }
        let _ = meshes.insert(pm.id(), rib.mesh());
    }
}

/// The UI: chips, details card, hints, waypoint status.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn panel(
    mut commands: Commands,
    mut st: ResMut<WorldMap>,
    font: Res<UiFont>,
    (wp, nav, settings, race): (Res<Waypoint>, Res<SatNav>, Res<super::Settings>, Option<Res<crate::race::RaceState>>),
    mut cards: Query<(Entity, &mut Visibility, &mut BorderColor), (With<Card>, Without<Chip>)>,
    mut chips: Query<(&Chip, &mut BackgroundColor, &mut BorderColor), Without<Card>>,
    mut chip_parts: Query<(Option<&ChipIcon>, Option<&ChipText>, Option<&mut ImageNode>, Option<&mut TextColor>), Or<(With<ChipIcon>, With<ChipText>)>>,
    mut texts: Query<(&mut Text, Has<HintText>), Or<(With<HintText>, With<StatusText>)>>,
) {
    if !st.open || !(st.panel_dirty || wp.is_changed() || nav.is_changed()) {
        return;
    }
    let metric = settings.metric;
    let status = match wp.target {
        Some(_) => format!("Waypoint: {}{}", wp.label, nav.distance_m.filter(|_| wp.target == nav.target).map(|d| format!("  ·  {}", distance_text(d, metric))).unwrap_or_default()),
        None => "No waypoint set".into(),
    };
    let hint = if st.pad_mode {
        "LS move   LT/RT zoom   A waypoint   X start / travel   LB/RB next   D-pad + Y filters   RS centre   B back"
    } else {
        "Drag / WASD move   Wheel / Q E zoom   Click waypoint   Right-click / F start / travel   [ ] next   1-8 filters   C centre   Esc back"
    };
    for (mut t, is_hint) in &mut texts {
        let want = if is_hint { hint.to_string() } else { status.clone() };
        if t.0 != want {
            t.0 = want;
        }
    }
    if !st.panel_dirty {
        return;
    }
    st.panel_dirty = false;
    // Chips.
    for (c, mut bg, mut border) in &mut chips {
        let on = st.filters[c.0];
        bg.0 = if on { Color::srgba(0.08, 0.09, 0.11, 0.92) } else { Color::srgba(0.03, 0.04, 0.05, 0.45) };
        *border = BorderColor::all(if st.pad_mode && st.chip == c.0 { Color::WHITE } else { Color::NONE });
    }
    for (icon, text, img, col) in &mut chip_parts {
        let i = icon.map(|c| c.0).or(text.map(|c| c.0)).unwrap_or(0);
        let a = if st.filters[i] { 1.0 } else { 0.35 };
        if let Some(mut img) = img {
            img.color = Color::srgba(1.0, 1.0, 1.0, a);
        }
        if let Some(mut col) = col {
            col.0 = Color::srgba(1.0, 1.0, 1.0, a);
        }
    }
    // Details card.
    let Ok((card, mut vis, mut border)) = cards.single_mut() else { return };
    commands.entity(card).despawn_children();
    let route = st.preview.as_ref().map(|(_, _, len)| format!("Route  {}  ·  ~{} min", distance_text(*len, metric), (len / ETA_SPEED / 60.0).ceil().max(1.0)));
    let (a, x) = if st.pad_mode { ("A", "X") } else { ("Click", "Right-click") };
    let (title, kind, accent, lines, actions): (String, String, Color, Vec<String>, Vec<String>) = match st.hover {
        Some(Hover::Icon(i)) => {
            let ic = &st.icons[i];
            let mut actions = vec![if wp.source.as_deref() == Some(ic.key.as_str()) { format!("{a}  Remove waypoint") } else { format!("{a}  Set waypoint") }];
            let racing = race.as_ref().is_some_and(|r| r.owns_nav());
            match ic.action {
                Action::Start(_) | Action::Travel(..) | Action::TravelRoad if racing => actions.push("Not during a race".into()),
                Action::Start(_) => actions.push(format!("{x}  Start event")),
                Action::Travel(..) | Action::TravelRoad => actions.push(format!("{x}  Fast travel")),
                Action::None => {}
            }
            (ic.name.clone(), ic.kind.clone(), ic.accent, ic.lines.clone(), actions)
        }
        Some(Hover::Waypoint) => (wp.label.clone(), "Waypoint".into(), ROUTE_GREEN, Vec::new(), vec![format!("{a}  Remove waypoint")]),
        Some(Hover::Point(_)) if st.preview.is_some() => ("Map location".into(), String::new(), ROUTE_GREEN, Vec::new(), vec![format!("{a}  Set waypoint")]),
        _ => {
            *vis = Visibility::Hidden;
            return;
        }
    };
    *vis = Visibility::Inherited;
    *border = BorderColor::all(accent);
    commands.entity(card).with_children(|p| {
        p.spawn((Text::new(title), font.text(24.0), TextColor(Color::WHITE)));
        if !kind.is_empty() {
            p.spawn((Text::new(kind.to_uppercase()), font.text(13.0), TextColor(accent)));
        }
        for l in lines {
            p.spawn((Text::new(l), font.text(16.0), TextColor(Color::srgba(1.0, 1.0, 1.0, 0.85))));
        }
        if let Some(r) = route {
            p.spawn((Text::new(r), font.text(16.0), TextColor(ROUTE_GREEN), Node { margin: UiRect::top(Val::Px(4.0)), ..default() }));
        }
        p.spawn(Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(16.0), margin: UiRect::top(Val::Px(8.0)), ..default() }).with_children(|p| {
            for act in actions {
                p.spawn((Text::new(act), font.text(15.0), TextColor(DIM)));
            }
        });
    });
}

/// Share the main camera's texture like the HUD camera does (scene.rs `sync_hdr`): same Hdr and usages.
#[allow(clippy::type_complexity)]
fn sync_hdr(
    mut commands: Commands,
    main: Query<(Has<bevy::camera::Hdr>, Option<&bevy::camera::CameraMainTextureUsages>), With<fh1_render::post::FxPostCamera>>,
    map: Query<(Entity, Has<bevy::camera::Hdr>, Option<&bevy::camera::CameraMainTextureUsages>), With<MapCamera>>,
) {
    let (Ok((main_hdr, main_usages)), Ok((e, hdr, usages))) = (main.single(), map.single()) else { return };
    if hdr != main_hdr {
        if main_hdr {
            commands.entity(e).insert(bevy::camera::Hdr);
        } else {
            commands.entity(e).remove::<bevy::camera::Hdr>();
        }
    }
    let want = main_usages.map(|u| u.0).unwrap_or_else(|| bevy::camera::CameraMainTextureUsages::default().0);
    let have = usages.map(|u| u.0).unwrap_or_else(|| bevy::camera::CameraMainTextureUsages::default().0);
    if want != have {
        commands.entity(e).insert(bevy::camera::CameraMainTextureUsages(want));
    }
}
