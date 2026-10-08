//! In-game UI: the driving HUD (tachometer dial, speed, gear, assists), a car card on spawn,
//! the telemetry overlay, and the pause menu (Esc / Start) with Options, Car select and Controls.
//!
//! STOPGAP: layout, colours and wording are placeholders, not FH1's. The real strings
//! (`stringtables/<LANG>.zip`), HUD textures (`ui/Textures.zip`) and fonts (`ui/Fonts.zip`) are
//! being reverse-engineered separately (docs/UI.md). Until then the font is Windows' Bahnschrift
//! when present (never shipped), otherwise Bevy's built-in font.
//!
//! Settings persist in `<data>/settings.json`. `FH1_MENU=main|options|cars|controls` opens the
//! menu on that page at start (for screenshots).

use std::path::PathBuf;

use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::prelude::*;
use fh1_ui::player::{Player, Value};
use serde::{Deserialize, Serialize};

pub mod assists;
pub mod browser;
pub mod customize;
pub mod customize_upgrades;
pub mod garage;
pub mod graphics;
pub mod thumbs;
pub mod hud;
// L1: launch screen + loading covers.
pub mod laptimer;
pub mod launch;
pub mod loading;
pub mod world_load;
pub mod worldmap;
pub mod materials;
pub mod minimap;
pub mod notify;
pub mod scene;
pub mod skillhud;

use crate::camera::CameraRig;
use crate::track::Track;
use crate::{Car, Garage, Input};
use fh1_engine::vehicle::{Shifting, SteeringAssist};

pub struct UiPlugin {
    pub settings_path: PathBuf,
}

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        let settings = std::fs::read(&self.settings_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_default();
        let mut menu = Menu::default();
        // Maps the "Map" option can switch to (Colorado + converted imports, docs/RENDERING.md).
        let maps = MapChoices(app.world().get_resource::<Garage>().map(|g| Track::available(&g.assets)).unwrap_or_default());
        if let Some(g) = app.world().get_resource::<Garage>() {
            browser::prefetch_catalog(&g.assets, &g.cars);
        }
        if let Ok(p) = std::env::var("FH1_MENU") {
            menu.open = true;
            menu.page = match p.as_str() {
                "options" => Page::Options,
                "cars" => Page::Cars,
                "controls" => Page::Controls,
                "travel" => Page::FastTravel,
                "maps" => Page::Maps,
                _ => Page::Main,
            };
            if let Some(g) = app.world().get_resource::<Garage>() {
                menu.open_browsers(g, &std::env::var("FH1_MENU_MAP").unwrap_or_else(|_| "colorado".into()));
            }
            menu.dirty = true;
        }
        app.insert_resource(settings)
            .insert_resource(SettingsPath(self.settings_path.clone()))
            .insert_resource(menu)
            .insert_resource(maps)
            .init_resource::<UiFont>()
            .add_plugins((materials::UiMaterialsPlugin, scene::AnarkPlugin, hud::HudPlugin, minimap::MinimapPlugin, notify::NotifyPlugin, worldmap::WorldMapPlugin, skillhud::SkillHudPlugin))
            .add_plugins(assists::AssistsPlugin)
            // Options > Graphics: AA, render scale, quality preset (P8).
            .add_plugins(graphics::GraphicsPlugin)
            // Garage: My cars / Autoshow on the credits ledger (P10).
            .add_plugins(garage::GaragePlugin)
            // Rendered car photos for cars without the game's own (ui/thumbs.rs).
            .add_plugins(thumbs::ThumbPlugin)
            // Garage > Customize (ui/customize.rs; garage.json beside settings.json).
            .add_plugins(customize::CustomizePlugin { path: self.settings_path.with_file_name("garage.json") })
            // L1: launch screen and loading covers (FH1_LAUNCH_SCREEN=0 / FH1_LOADING=0).
            .add_plugins(loading::LoadingPlugin)
            // L1b: Motorsport hot-lap timer (tracks with track.json timing).
            .add_plugins(laptimer::LapTimerPlugin)
            .add_systems(Startup, load_fh1_ui)
            .add_systems(Update, spawn_world_ui.run_if(world_load::world_ready))
            .init_resource::<world_load::MapSwitch>()
            .init_resource::<world_load::WorldGeneration>()
            .add_systems(Update, (world_load::autopick, world_load::map_tour, world_load::apply_choice).chain())
            // Colorado's collision debug view is built on the first V press (track.rs CollisionViewLazy).
            .add_systems(Update, crate::track::collision_view_on_demand)
            // Unload at the start of a frame: no commands queued against the old world's entities yet (X1c).
            .add_systems(First, world_load::switch_world)
            .add_message::<GameAction>()
            .add_systems(Startup, (spawn_hud, spawn_menu))
            .add_systems(
                Update,
                (
                    (menu_input, menu_mouse, sync_pause, draw_menu, drive_fh1_pause).chain().before(crate::read_input),
                    (rebuild_dial, update_hud, car_card).chain().after(crate::sync_visuals),
                ),
            );
    }
}

/// Player options, saved to `settings.json` when the pause menu closes.
#[derive(Resource, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// km/h (true) or mph.
    pub metric: bool,
    pub tcs: bool,
    pub abs: bool,
    /// Stability management (FH1 "Stability Control"). The game's profile default is on; off here when the file
    /// predates the option (the player's other assists were already off).
    pub stm: bool,
    /// FH1 "Steering": Assisted / Normal / Simulation (Assisted also steers along an event's racing line, ai/assist.rs).
    pub steering: SteeringAssist,
    /// FH1 "Braking" Assisted: brakes for the corners of an event's racing line (ABS stays on; ai/assist.rs).
    pub braking_assist: bool,
    /// FH1 "Driving line": Full / Braking only / Off (drawn during events, ai/assist.rs).
    pub driving_line: fh1_engine::ai::DrivingLine,
    /// FH1 AI difficulty (Events Easy / Med / Hard / Pro columns); the game's default is Medium.
    pub ai_difficulty: fh1_engine::ai::AiDifficulty,
    /// FH1 "Shifting": Automatic / Manual / Manual with clutch.
    pub shifting: Shifting,
    /// FH1 "Rewind": hold X / Back to rewind up to 15 s.
    pub rewind: bool,
    pub hud: bool,
    pub telemetry: bool,
    pub engine_volume: f32,
    pub radio_volume: f32,
    /// Map to load when no `--track` is given (`colorado` or an imported map id); None = Colorado.
    pub map: Option<String>,
    /// L1b main menu: last Motorsport track (map id) and the last car picked there or in the pause menu (media name).
    pub motorsport_track: Option<String>,
    pub car: Option<String>,
    /// Video: the game's original translated shaders (faithful renderer) instead of the remaster. Superseded by
    /// `renderer`; still read when `renderer` is absent (older settings files).
    pub original_shaders: bool,
    /// Retired renderer choice (always Remaster now); kept so older settings files still load.
    pub renderer: Option<String>,
    /// Options > Graphics (ui/graphics.rs, P8): quality preset, anti-aliasing, render scale.
    pub graphics: graphics::GraphicsSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            metric: true,
            tcs: true,
            abs: true,
            stm: false,
            steering: SteeringAssist::Normal,
            braking_assist: false,
            driving_line: fh1_engine::ai::DrivingLine::Full,
            ai_difficulty: fh1_engine::ai::AiDifficulty::Medium,
            shifting: Shifting::Automatic,
            rewind: true,
            hud: true,
            telemetry: false,
            engine_volume: 1.0,
            radio_volume: 1.0,
            map: None,
            motorsport_track: None,
            car: None,
            original_shaders: false,
            renderer: None,
            graphics: graphics::GraphicsSettings::default(),
        }
    }
}

/// Public: ui/launch.rs `main_menu` (pub) takes it.
#[derive(Resource)]
pub struct SettingsPath(PathBuf);

/// (map id, display name) for the map picker.
#[derive(Resource, Default)]
struct MapChoices(Vec<(String, String)>);

/// Requests from the menu, carried out by the game systems in main.rs.
#[derive(Message, Clone, Copy)]
pub enum GameAction {
    Restart,
    SelectCar(usize),
    /// Go to the track's n-th start location (fast-travel menu).
    FastTravel(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Page {
    #[default]
    Main,
    Options,
    Cars,
    Controls,
    /// The current map's start locations.
    FastTravel,
    /// Maps to switch to (relaunches with `--track`).
    Maps,
    /// Change car / Customize (pause menu "Change car"; FH1_CUSTOMIZE=0 skips it).
    Garage,
    /// ui/customize.rs: paint, rims, body kit, upgrades.
    Customize,
    /// ui/worldmap.rs: the full-screen world map (Colorado; replaces the Fast travel row there).
    Map,
    /// Options > Graphics (ui/graphics.rs): quality preset, anti-aliasing, render scale.
    Graphics,
}

/// Row of "Graphics" on the Options page (back from Options > Graphics lands on it).
const GRAPHICS_ROW: usize = 14;

/// Long list pages: the cursor indexes the whole list and the rows scroll (like the car list).
fn is_list(p: Page) -> bool {
    matches!(p, Page::Cars | Page::FastTravel | Page::Maps)
}

#[derive(Resource, Default)]
pub struct Menu {
    pub open: bool,
    page: Page,
    cursor: usize,
    /// Car browser (browser.rs), opened on the current car whenever the car page opens; its catalog is built once.
    cars: Option<browser::CarBrowser>,
    catalog: Option<std::sync::Arc<browser::CarCatalog>>,
    /// Map browser (browser.rs), grouped by game.
    maps: browser::MapBrowser,
    /// Settings when the menu opened, to save only on change.
    saved: Option<Settings>,
    dirty: bool,
    /// Held-direction auto-repeat: direction and time until the next step.
    repeat: (i32, f32),
    /// The browsers' own auto-repeat (browser::read_input).
    browse_repeat: browser::Repeat,
    /// The Customize page (ui/customize.rs).
    custom: customize::CustomizeMenu,
    /// The world map was opened straight from driving (pad Back tap / Tab): leaving it resumes.
    map_direct: bool,
    /// Garage modes of the car page (My cars / Autoshow), buy / sell dialogs (ui/garage.rs).
    shop: garage::ShopState,
}

impl Menu {
    /// (Re)open both browsers on the current car / map.
    /// ui/worldmap.rs: the world map page is up.
    pub fn map_open(&self) -> bool {
        self.open && self.page == Page::Map
    }

    fn open_browsers(&mut self, garage: &Garage, map: &str) {
        let catalog = self.catalog.get_or_insert_with(|| browser::catalog_for(&garage.assets, &garage.cars)).clone();
        self.cars = Some(browser::CarBrowser::new(catalog, garage.current));
        // Optional games that aren't imported: greyed (FM3, not importable yet, isn't listed here).
        let locked = crate::track::locked_maps(&garage.assets).into_iter().filter(|l| !l.coming_soon);
        self.maps = browser::MapBrowser::new(Track::maps_by_game(&garage.assets), map).with_locked(locked);
    }
}

/// Not paused and not in photo mode: driving input is live.
pub fn driving(menu: Res<Menu>, rig: Res<CameraRig>) -> bool {
    // L1: not while the launch screen or a loading cover is up.
    !menu.open && !rig.photo && !loading::blocking()
}

pub fn menu_closed(menu: Res<Menu>) -> bool {
    !menu.open
}

#[derive(Resource)]
pub struct UiFont(pub Handle<Font>);

impl FromWorld for UiFont {
    fn from_world(world: &mut World) -> Self {
        // Bahnschrift (DIN-style, ships with Windows 10+) reads like a racing HUD; on Linux a condensed system sans
        // stands in. Loaded from the user's system and never redistributed.
        let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let candidates = [
            windir.join("Fonts").join("bahnschrift.ttf"),
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSansCondensed.ttf"),
            PathBuf::from("/usr/share/fonts/TTF/DejaVuSansCondensed.ttf"),
            PathBuf::from("/usr/share/fonts/truetype/liberation/LiberationSansNarrow-Regular.ttf"),
            PathBuf::from("/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf"),
            PathBuf::from("/usr/share/fonts/noto/NotoSans-Regular.ttf"),
        ];
        match candidates.iter().find_map(|p| std::fs::read(p).ok()).ok_or(()) {
            Ok(bytes) => UiFont(world.resource_mut::<Assets<Font>>().add(Font::from_bytes(bytes))),
            Err(_) => UiFont(Handle::default()),
        }
    }
}

impl UiFont {
    pub fn text(&self, px: f32) -> TextFont {
        TextFont { font: FontSource::Handle(self.0.clone()), font_size: FontSize::Px(px), ..default() }
    }
}

const ACCENT: Color = Color::srgb(0.93, 0.16, 0.48);
const DIM: Color = Color::srgba(1.0, 1.0, 1.0, 0.55);
const PANEL: Color = Color::srgba(0.04, 0.05, 0.07, 0.72);

// ---------------------------------------------------------------- FH1 scenes

/// Load the converted `ui` group (fonts, strings) and, for testing, show a scene:
/// `FH1_UI_SCENE=<name>` (e.g. `925_PAUSE_MENU`), `FH1_UI_EVENTS=SHOWN,...` fired after loading,
/// `FH1_UI_SLIDES=<component>=<slide>,...` applied after the events.
fn load_fh1_ui(
    mut commands: Commands,
    garage: Res<Garage>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    assets: Res<AssetServer>,
    track: Res<Track>,
) {
    let lang = std::env::var("FH1_UI_LANG").unwrap_or_else(|_| "EN".into()).to_ascii_uppercase();
    let Some(data) = scene::UiData::load(&garage.assets, &lang) else {
        info!("no FH1 UI installed; run fh1setup to convert the `ui` group");
        return;
    };
    if let Ok(name) = std::env::var("FH1_UI_SCENE") {
        match data.scene(&name) {
            Some(mut p) => {
                for ev in std::env::var("FH1_UI_EVENTS").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
                    info!("ui {name}: {ev} ran {} actions", p.fire(ev));
                }
                for spec in std::env::var("FH1_UI_SLIDES").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
                    if let Some((comp, slide)) = spec.split_once('=') {
                        let ok = p.find(comp).is_some_and(|c| p.goto_slide(c, slide));
                        info!("ui {name}: {comp} -> {slide}: {ok}");
                    }
                }
                commands.spawn(scene::AnarkScene::new(p, 0));
            }
            None => warn!("ui scene {name} not found"),
        }
    }
    // L1b: the HUD and minimap depend on the map; they spawn in `spawn_world_ui` once the world is loaded (after the
    // main menu's choice, or on the first frame without it).
    let _ = (&track, &mut images, &mut meshes, &mut mats, &assets);
    if let Some(p) = data.scene("925_PAUSE_MENU") {
        let mut sc = scene::AnarkScene::new(Player::new(p.scene.clone()), 20);
        sc.visible = false;
        let entity = commands.spawn(sc).id();
        commands.insert_resource(Fh1Pause::new(entity, &p, &data));
    }
    commands.insert_resource(data);
}

/// L1b: the driving HUD and the minimap, once the world is loaded (main menu first: after the choice).
#[allow(clippy::too_many_arguments)]
fn spawn_world_ui(
    mut commands: Commands,
    data: Option<Res<scene::UiData>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    (mut mats, mut road_mats): (ResMut<Assets<ColorMaterial>>, ResMut<Assets<minimap::RoadFogMaterial>>),
    assets: Res<AssetServer>,
    track: Res<Track>,
    mut done: Local<bool>,
) {
    let Some(data) = data else { return };
    if *done {
        return;
    }
    *done = true;
    // The minimap draws Colorado's road network (`colorado.nav`). Spawned whatever the first map is, so an in-process
    // switch to Colorado has it; its systems run and the HUD shows it on Colorado only (X1d).
    let _ = &track;
    let map = minimap::spawn(&mut commands, &data, &mut images, &mut meshes, &mut mats, &mut road_mats, &assets);
    hud::spawn(&mut commands, &data, map);
}

/// Labels of the seven FH1 pause buttons as string-table refs (same order as `items()` Page::Main).
const FH1_PAUSE_ITEMS: [(&str, &str); 7] = [
    ("PauseMenu:IDS_Resume", "RESUME"),
    ("PauseMenu:IDS_Restart", "RESTART"),
    ("PauseMenu:IDS_FastTravel", "FAST TRAVEL"),
    ("InGame:IDS_ChangeCar", "CHANGE CAR"),
    ("PauseMenu:IDS_Photo", "PHOTO MODE"),
    ("PauseMenu:IDS_Options", "OPTIONS"),
    ("PauseMenu:IDS_Quit", "QUIT"),
];

/// FH1's pause menu scene (`925_PAUSE_MENU`), shown for the menu's main page: the objects the
/// game drives and the localised labels.
#[derive(Resource)]
pub struct Fh1Pause {
    entity: Entity,
    /// Button components Button0..6 and their `Text` objects.
    buttons: Vec<(usize, Option<usize>)>,
    labels: Vec<String>,
    /// Row 2's label on maps with a world map (ui/worldmap.rs).
    map_label: String,
    title: Option<usize>,
    title_text: String,
    /// Help-button components HelpButton0..6 and their texts.
    help: Vec<(usize, Option<usize>)>,
    help_texts: Vec<String>,
    player_bar: Option<usize>,
    /// Cursor the button slides currently show (None = not shown).
    shown: Option<usize>,
}

impl Fh1Pause {
    fn new(entity: Entity, p: &Player, data: &scene::UiData) -> Self {
        let text = |r: &str, d: &str| data.strings.as_ref().and_then(|s| s.resolve(r)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| d.to_owned());
        let buttons = (0..7).filter_map(|i| p.find(&format!("Button{i}"))).map(|b| (b, p.resolve_path(b, "Button.Text"))).collect();
        let help = (0..7).filter_map(|i| p.find(&format!("HelpButton{i}"))).map(|b| (b, find_below(p, b, "Text_BUTTON"))).collect();
        Self {
            entity,
            buttons,
            labels: FH1_PAUSE_ITEMS.iter().map(|(r, d)| text(r, d)).collect(),
            map_label: "MAP".into(),
            title: p.find("TEXT_SCREEN_TITLE"),
            title_text: text("InGame:IDS_Paused", "PAUSED"),
            help,
            help_texts: vec![text("HelpButtons:IDS_Select", "SELECT")],
            player_bar: p.find("PlayerBar"),
            shown: None,
        }
    }

    /// Apply the game's values (after the scene state was reset).
    fn bind(&self, p: &mut Player) {
        for (i, (button, text)) in self.buttons.iter().enumerate() {
            // Row 2 is the world map where there is one (items(): Page::Main).
            let label = if i == 2 && worldmap::available() { Some(&self.map_label) } else { self.labels.get(i) };
            match (label, text) {
                (Some(l), Some(t)) => p.set_text(*t, l.clone()),
                // More buttons than items: hide the spare rows.
                _ => p.set(*button, fh1_ui::names::OPACITY, Value::Float(0.0)),
            }
        }
        if let Some(t) = self.title {
            p.set_text(t, self.title_text.clone());
        }
        for (i, (hb, text)) in self.help.iter().enumerate() {
            match (self.help_texts.get(i), text) {
                (Some(l), Some(t)) => p.set_text(*t, l.clone()),
                _ => p.set(*hb, fh1_ui::names::OPACITY, Value::Float(0.0)),
            }
        }
        // Profile bar (car, credits, level): no career data yet, so it's hidden. STOPGAP.
        if let Some(pb) = self.player_bar {
            p.goto_slide(pb, "HIDE");
        }
    }
}

/// First descendant of `from` with this name.
fn find_below(p: &Player, from: usize, name: &str) -> Option<usize> {
    let objs = &p.scene.bgf.objects;
    let mut stack = vec![from];
    while let Some(o) = stack.pop() {
        for (i, c) in objs.iter().enumerate() {
            if c.parent == o as i32 {
                if p.name(i) == Some(name) {
                    return Some(i);
                }
                stack.push(i);
            }
        }
    }
    None
}

/// Show FH1's pause scene for the main page and drive its button slides from the cursor.
fn drive_fh1_pause(fh1: Option<ResMut<Fh1Pause>>, menu: Res<Menu>, mut scenes: Query<&mut scene::AnarkScene>) {
    let Some(mut fh1) = fh1 else { return };
    let Ok(mut sc) = scenes.get_mut(fh1.entity) else { return };
    if !(menu.open && menu.page == Page::Main) {
        sc.visible = false;
        fh1.shown = None;
        return;
    }
    let cursor = menu.cursor;
    let sc = &mut *sc;
    let (p, visible) = (&mut sc.player, &mut sc.visible);
    match fh1.shown {
        None => {
            // Opened: fresh state, SHOWN + the scene's INTRO, then the game's values (SHOWN's
            // actions set opacities, so binding first would be undone), static focus.
            p.reset();
            p.fire("SHOWN");
            p.goto_slide(0, "INTRO");
            fh1.bind(p);
            for (i, (b, _)) in fh1.buttons.iter().enumerate() {
                let slide = match i.cmp(&cursor) {
                    std::cmp::Ordering::Less => "BLURREDABOVE",
                    std::cmp::Ordering::Equal => "FOCUSED",
                    std::cmp::Ordering::Greater => "BLURREDBELOW",
                };
                p.goto_slide(*b, slide);
            }
            *visible = true;
        }
        Some(old) if old != cursor => {
            // GUESS on the naming: FOCUS<where the row was>, BLUR<where it goes> (the opposite
            // reading looks the same: both crossfade the highlight).
            let down = cursor > old;
            if let Some((b, _)) = fh1.buttons.get(cursor) {
                p.goto_slide(*b, if down { "FOCUSBELOW" } else { "FOCUSABOVE" });
            }
            if let Some((b, _)) = fh1.buttons.get(old) {
                p.goto_slide(*b, if down { "BLURABOVE" } else { "BLURBELOW" });
            }
        }
        _ => {}
    }
    // A finished transition settles into the static slide for the row's place (the transition's
    // last frame isn't the static look).
    for (i, (b, _)) in fh1.buttons.iter().enumerate() {
        let Some(clock) = p.clock(*b) else { continue };
        let name = p.scene.bgf.slides[clock.slide].name.clone();
        if !clock.playing && matches!(name.as_str(), "FOCUSABOVE" | "FOCUSBELOW" | "BLURABOVE" | "BLURBELOW") {
            let settle = match i.cmp(&cursor) {
                std::cmp::Ordering::Less => "BLURREDABOVE",
                std::cmp::Ordering::Equal => "FOCUSED",
                std::cmp::Ordering::Greater => "BLURREDBELOW",
            };
            let ok = p.goto_slide(*b, settle);
            debug!("pause button {i}: {name} -> {settle} ({ok})");
        }
    }
    fh1.shown = Some(cursor);
}


// ---------------------------------------------------------------- HUD

#[derive(Component)]
pub struct Hud;

#[derive(Component)]
struct Telemetry;

#[derive(Component)]
struct Dial;

#[derive(Component)]
struct DialTick(f32);

#[derive(Component)]
struct DialLabel;

#[derive(Component)]
struct GearText;

#[derive(Component)]
struct SpeedText;

#[derive(Component)]
struct UnitText;

#[derive(Component)]
struct AssistText;

#[derive(Component)]
struct CarCard;

const DIAL: f32 = 230.0;
const TICKS: usize = 54;
/// The dial sweeps 270°, open at the bottom.
const SWEEP: f32 = 1.5 * std::f32::consts::PI;

fn dial_angle(f: f32) -> f32 {
    -SWEEP / 2.0 + SWEEP * f
}

/// Position + rotation for a node centred on the dial, pushed `r` px out at fraction `f`.
fn on_dial(f: f32, r: f32, w: f32, h: f32, rotate: bool) -> (Node, UiTransform) {
    let a = dial_angle(f);
    let node = Node {
        position_type: PositionType::Absolute,
        left: Val::Px(DIAL / 2.0 - w / 2.0),
        top: Val::Px(DIAL / 2.0 - h / 2.0),
        width: Val::Px(w),
        height: Val::Px(h),
        justify_content: JustifyContent::Center,
        align_items: AlignItems::Center,
        ..default()
    };
    let mut t = UiTransform::from_xy(Val::Px(r * a.sin()), Val::Px(-r * a.cos()));
    if rotate {
        t.rotation = Rot2::radians(a);
    }
    (node, t)
}

fn spawn_hud(mut commands: Commands, font: Res<UiFont>) {
    commands.spawn((
        Telemetry,
        Hud,
        Text::new(""),
        font.text(15.0),
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.85)),
        Node { position_type: PositionType::Absolute, left: Val::Px(12.0), top: Val::Px(10.0), ..default() },
    ));

    commands
        .spawn((
            Hud,
            Dial,
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(28.0),
                bottom: Val::Px(24.0),
                width: Val::Px(DIAL),
                height: Val::Px(DIAL),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.05, 0.55)),
        ))
        .with_children(|d| {
            for i in 0..TICKS {
                let f = i as f32 / (TICKS - 1) as f32;
                d.spawn((on_dial(f, DIAL / 2.0 - 16.0, 5.0, 16.0, true), BackgroundColor(DIM), DialTick(f)));
            }
            d.spawn(Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|c| {
                c.spawn((GearText, Text::new("N"), font.text(64.0), TextColor(Color::WHITE)));
                c.spawn((SpeedText, Text::new("0"), font.text(36.0), TextColor(Color::WHITE)));
                c.spawn((UnitText, Text::new("KM/H"), font.text(14.0), TextColor(DIM)));
            });
            d.spawn((
                AssistText,
                Text::new(""),
                font.text(13.0),
                TextColor(ACCENT),
                TextLayout::justify(Justify::Center),
                Node { position_type: PositionType::Absolute, bottom: Val::Px(14.0), width: Val::Percent(100.0), ..default() },
            ));
        });

    commands.spawn((
        CarCard,
        Text::new(""),
        font.text(26.0),
        TextColor(Color::WHITE.with_alpha(0.0)),
        Node { position_type: PositionType::Absolute, left: Val::Px(28.0), bottom: Val::Px(28.0), ..default() },
    ));
}

/// The dial's top end: the next 1000 rpm above redline.
fn dial_max(redline: f32) -> f32 {
    ((redline + 500.0) / 1000.0).ceil() * 1000.0
}

/// Number labels (thousands of rpm) depend on the car's redline: rebuild them on car change.
fn rebuild_dial(mut commands: Commands, cars: Query<&Car>, dial: Query<Entity, With<Dial>>, labels: Query<Entity, With<DialLabel>>, font: Res<UiFont>, mut last: Local<String>) {
    let (Ok(car), Ok(dial)) = (cars.single(), dial.single()) else { return };
    if *last == car.0.data.media_name {
        return;
    }
    *last = car.0.data.media_name.clone();
    for e in &labels {
        commands.entity(e).despawn();
    }
    let max = dial_max(car.0.data.redline_rpm);
    let n = (max / 1000.0) as usize;
    commands.entity(dial).with_children(|d| {
        for k in 0..=n {
            let f = k as f32 / n as f32;
            let red = k as f32 * 1000.0 >= car.0.data.redline_rpm;
            d.spawn((
                DialLabel,
                on_dial(f, DIAL / 2.0 - 40.0, 30.0, 20.0, false),
                children![(Text::new(k.to_string()), font.text(15.0), TextColor(if red { ACCENT } else { DIM }))],
            ));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn update_hud(
    cars: Query<&Car>,
    settings: Res<Settings>,
    menu: Res<Menu>,
    rig: Res<CameraRig>,
    garage: Res<Garage>,
    track: Res<Track>,
    input: Res<Input>,
    mut hud: Query<(&mut Visibility, Has<Telemetry>), With<Hud>>,
    mut ticks: Query<(&DialTick, &mut BackgroundColor)>,
    mut texts: ParamSet<(
        Query<(&mut Text, &mut TextColor), With<GearText>>,
        Query<&mut Text, With<SpeedText>>,
        Query<&mut Text, With<UnitText>>,
        Query<&mut Text, With<AssistText>>,
        Query<&mut Text, With<Telemetry>>,
    )>,
    fh1: Option<Res<hud::Fh1Hud>>,
) {
    let show = settings.hud && !menu.open && !rig.photo;
    for (mut v, telemetry) in &mut hud {
        // The placeholder dial only stands in when FH1's own HUD isn't installed.
        let on = if telemetry { settings.telemetry } else { fh1.is_none() };
        let want = if show && on { Visibility::Inherited } else { Visibility::Hidden };
        v.set_if_neq(want);
    }
    let Ok(car) = cars.single() else { return };
    let v = &car.0;
    let max = dial_max(v.data.redline_rpm);
    let rpm_f = (v.rpm / max).clamp(0.0, 1.0);
    let red_f = v.data.redline_rpm / max;
    for (tick, mut bg) in &mut ticks {
        let lit = tick.0 <= rpm_f + 1e-3;
        let red = tick.0 >= red_f - 1e-3;
        let c = match (lit, red) {
            (true, true) => ACCENT,
            (true, false) => Color::WHITE,
            (false, true) => ACCENT.with_alpha(0.35),
            (false, false) => Color::srgba(1.0, 1.0, 1.0, 0.18),
        };
        bg.0 = c;
    }
    let gear = if v.gear == 0 { "R".to_string() } else { v.gear.to_string() };
    let shift = v.rpm > v.data.redline_rpm * 0.94;
    if let Ok((mut t, mut c)) = texts.p0().single_mut() {
        t.0 = gear.clone();
        c.0 = if shift { ACCENT } else { Color::WHITE };
    }
    let speed = if settings.metric { v.speed() * 3.6 } else { v.speed() * 2.23694 };
    if let Ok(mut t) = texts.p1().single_mut() {
        t.0 = format!("{:.0}", speed.abs());
    }
    if let Ok(mut t) = texts.p2().single_mut() {
        t.0 = if settings.metric { "KM/H" } else { "MPH" }.into();
    }
    if let Ok(mut t) = texts.p3().single_mut() {
        let mut off = Vec::new();
        if !settings.tcs {
            off.push("TCS OFF");
        }
        if !settings.abs {
            off.push("ABS OFF");
        }
        t.0 = off.join("  ");
    }
    if settings.telemetry {
        let slip: Vec<String> = v.wheels.iter().map(|w| format!("{:+.2}", w.slip_ratio)).collect();
        let surface = match &track.world {
            Some(w) => w.surface_name(v.wheels[2].surface).to_owned(),
            None => "test plane".into(),
        };
        if let Ok(mut t) = texts.p4().single_mut() {
            t.0 = format!(
                "{} ({})   [{}/{}]   {}  {} ({:.0}, {:.0})\n{:>5.0} rpm  gear {}  throttle {:.2} brake {:.2}\nloads {:.0} {:.0} / {:.0} {:.0} N\nslip {}",
                v.data.media_name,
                v.data.display_year,
                garage.current + 1,
                garage.cars.len(),
                track.name,
                surface,
                v.position.x,
                v.position.z,
                v.rpm,
                gear,
                input.0.throttle,
                input.0.brake,
                v.wheels[0].load,
                v.wheels[1].load,
                v.wheels[2].load,
                v.wheels[3].load,
                slip.join(" "),
            );
        }
    }
}

/// Name card in the bottom left for a few seconds after a car spawns.
fn car_card(cars: Query<&Car>, mut card: Query<(&mut Text, &mut TextColor), With<CarCard>>, menu: Res<Menu>, time: Res<Time<Real>>, mut shown: Local<(String, f32)>, fh1: Option<Res<hud::Fh1Hud>>) {
    let (Ok(car), Ok((mut text, mut color))) = (cars.single(), card.single_mut()) else { return };
    if fh1.is_some() {
        // FH1 has no such card; it was a placeholder.
        color.0 = Color::NONE;
        return;
    }
    let d = &car.0.data;
    let now = time.elapsed_secs();
    if shown.0 != d.media_name {
        *shown = (d.media_name.clone(), now);
        text.0 = format!("{} {}\n{:.0} kg  ·  {} speed\nEsc / Start  pause menu", d.display_year, d.media_name, d.mass, d.gears.len());
    }
    let age = now - shown.1;
    let a = if menu.open { 0.0 } else { (1.0 - (age - 5.0) / 0.6).clamp(0.0, 1.0) };
    color.0 = Color::WHITE.with_alpha(a);
}

// ---------------------------------------------------------------- pause menu

#[derive(Component)]
struct MenuRoot;

#[derive(Component)]
struct MenuPanel;

#[derive(Component)]
struct MenuRow(usize);

#[derive(Clone, Copy)]
enum Opt {
    Units,
    Tcs,
    Abs,
    Stm,
    Steering,
    Shifting,
    Rewind,
    Hud,
    Telemetry,
    EngineVolume,
    RadioVolume,
    DrivingLine,
    AiDifficulty,
    Quality,
    AntiAlias,
    RenderScale,
}

#[derive(Clone, Copy)]
enum Act {
    Resume,
    Restart,
    Open(Page),
    /// The car page in a garage mode (My cars / Autoshow).
    Cars(garage::Mode),
    Photo,
    Quit,
    Opt(Opt),
    /// Row of the car / map browser (browser.rs).
    Browse(usize),
    /// Row of the Customize page (ui/customize.rs).
    Custom(usize),
    Travel(usize),
    None,
}

struct Item {
    label: String,
    value: Option<String>,
    act: Act,
}

fn item(label: impl Into<String>, act: Act) -> Item {
    Item { label: label.into(), value: None, act }
}

fn steering_name(s: SteeringAssist) -> &'static str {
    match s {
        SteeringAssist::Assisted => "Assisted",
        SteeringAssist::Normal => "Normal",
        SteeringAssist::Simulation => "Simulation",
    }
}

fn shifting_name(s: Shifting) -> &'static str {
    match s {
        Shifting::Automatic => "Automatic",
        Shifting::Manual => "Manual",
        Shifting::ManualClutch => "Manual w/ clutch",
    }
}

fn on_off(b: bool) -> String {
    if b { "On" } else { "Off" }.into()
}

const CAR_ROWS: usize = 10;

/// The rows of the current page, and the first car index shown (car page only).
fn items(menu: &Menu, settings: &Settings, garage: &Garage, track: &Track, maps: &MapChoices) -> (Vec<Item>, usize) {
    // Scrolling window over a long list: (first row, rows).
    let window = |cursor: usize, n: usize| {
        let first = cursor.saturating_sub(CAR_ROWS / 2).min(n.saturating_sub(CAR_ROWS));
        (first, first..(first + CAR_ROWS).min(n))
    };
    match menu.page {
        Page::Main => (
            vec![
                // FH1's pause menu has seven buttons; labels in `FH1_PAUSE_ITEMS`.
                item("Resume", Act::Resume),
                item("Restart at start", Act::Restart),
                // The world map holds fast travel on Colorado; other maps keep the start-location list.
                if worldmap::available() { item("Map", Act::Open(Page::Map)) } else { item("Fast travel", Act::Open(Page::FastTravel)) },
                item("Change car", Act::Open(if customize::enabled() { Page::Garage } else { Page::Cars })),
                item("Photo mode", Act::Photo),
                item("Options", Act::Open(Page::Options)),
                item("Quit to desktop", Act::Quit),
            ],
            0,
        ),
        Page::Options => {
            let pct = |v: f32| format!("{:.0}%", v * 100.0);
            let opt = |l: &str, v: String, o: Opt| Item { label: l.into(), value: Some(v), act: Act::Opt(o) };
            (
                vec![
                    opt("Speed units", if settings.metric { "km/h" } else { "mph" }.into(), Opt::Units),
                    // FH1's assists (Assists.str), in the game's order. The driving line and the auto-brake / auto-steer
                    // halves of Assisted braking / steering follow an event's racing line (ai/assist.rs, docs/ASSISTS.md).
                    opt("Braking", if settings.braking_assist { "Assisted" } else if settings.abs { "ABS on" } else { "ABS off" }.into(), Opt::Abs),
                    opt("Steering", steering_name(settings.steering).into(), Opt::Steering),
                    opt("Traction control", on_off(settings.tcs), Opt::Tcs),
                    opt("Stability control", on_off(settings.stm), Opt::Stm),
                    opt("Shifting", shifting_name(settings.shifting).into(), Opt::Shifting),
                    opt("Driving line", settings.driving_line.name().into(), Opt::DrivingLine),
                    opt("AI difficulty", settings.ai_difficulty.name().into(), Opt::AiDifficulty),
                    opt("Rewind", on_off(settings.rewind), Opt::Rewind),
                    opt("HUD", on_off(settings.hud), Opt::Hud),
                    opt("Telemetry overlay", on_off(settings.telemetry), Opt::Telemetry),
                    opt("Engine volume", pct(settings.engine_volume), Opt::EngineVolume),
                    opt("Radio volume", pct(settings.radio_volume), Opt::RadioVolume),
                    item("Graphics", Act::Open(Page::Graphics)),
                    Item { label: "Map".into(), value: Some(track.name.clone()), act: Act::Open(Page::Maps) },
                    item("Controls", Act::Open(Page::Controls)),
                ],
                0,
            )
        }
        Page::Graphics => {
            let opt = |l: &str, v: String, o: Opt| Item { label: l.into(), value: Some(v), act: Act::Opt(o) };
            let g = &settings.graphics;
            (
                vec![
                    opt("Quality", g.quality.name().into(), Opt::Quality),
                    opt("Anti-aliasing", graphics::aa_value(g), Opt::AntiAlias),
                    opt("Render scale", graphics::scale_value(g), Opt::RenderScale),
                ],
                0,
            )
        }
        Page::Garage if garage::shop_on() => (
            vec![item("My cars", Act::Cars(garage::Mode::Owned)), item("Autoshow", Act::Cars(garage::Mode::Shop)), item("Customize", Act::Open(Page::Customize))],
            0,
        ),
        Page::Garage => (vec![item("Change car", Act::Open(Page::Cars)), item("Customize", Act::Open(Page::Customize))], 0),
        Page::Customize => {
            let all = &menu.custom.rows;
            let (first, range) = window(menu.custom.cursor, all.len());
            let rows = range.map(|k| Item { label: all[k].label.clone(), value: all[k].value.clone(), act: if all[k].selectable { Act::Custom(k) } else { Act::None } }).collect();
            (rows, first)
        }
        Page::FastTravel => {
            let (first, range) = window(menu.cursor, track.spawn_names.len());
            (range.map(|i| item(track.spawn_names[i].clone(), Act::Travel(i))).collect(), first)
        }
        Page::Maps | Page::Cars => {
            let _ = (garage, track, maps);
            let (all, cursor) = match (menu.page, &menu.cars) {
                (Page::Cars, Some(b)) => (b.rows(), b.cursor),
                (Page::Cars, None) => (Vec::new(), 0),
                _ => (menu.maps.rows(), menu.maps.cursor),
            };
            let (first, range) = window(cursor, all.len());
            let rows = range.map(|k| Item { label: all[k].label.clone(), value: all[k].value.clone(), act: Act::Browse(k) }).collect();
            (rows, first)
        }
        Page::Map => (Vec::new(), 0),
        Page::Controls => {
            let rows = [
                ("Throttle / brake", "W S  ·  RT LT"),
                ("Steer", "A D  ·  left stick"),
                ("Handbrake", "Space  ·  A"),
                ("Shift up / down (manual)", "E Q  ·  B X"),
                ("Clutch (manual / manual with clutch)", "Left Shift  ·  LB"),
                ("Rewind (hold)", "X  ·  Back"),
                ("Reset car", "R  ·  Y"),
                ("Camera", "C  ·  RB"),
                ("Look / zoom", "mouse drag, wheel  ·  right stick, D-pad up/down"),
                ("Radio station", ", .  ·  D-pad left/right"),
                ("Next / previous car", "N P  (pause menu: Car select)"),
                ("Traction control", "T"),
                ("Next start location", "G  (pause menu: Fast travel)"),
                ("Photo mode / screenshot", "F, F12  ·  A in photo mode"),
                ("Mute engine / collision view", "M  /  V"),
                ("Pause", "Esc  ·  Start"),
            ];
            (rows.iter().map(|(l, v)| Item { label: (*l).into(), value: Some((*v).into()), act: Act::None }).collect(), 0)
        }
    }
}

fn page_title(p: Page) -> &'static str {
    match p {
        Page::Main => "PAUSED",
        Page::Options => "OPTIONS",
        Page::Cars => "CHANGE CAR",
        Page::Controls => "CONTROLS",
        Page::FastTravel => "FAST TRAVEL",
        Page::Maps => "MAP",
        Page::Garage => "GARAGE",
        Page::Customize => "CUSTOMIZE",
        Page::Map => "WORLD MAP",
        Page::Graphics => "GRAPHICS",
    }
}

fn spawn_menu(mut commands: Commands) {
    commands
        .spawn((
            MenuRoot,
            GlobalZIndex(100),
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                padding: UiRect::left(Val::Percent(7.0)),
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.5)),
        ))
        .with_child((
            MenuPanel,
            Node {
                flex_direction: FlexDirection::Column,
                min_width: Val::Px(560.0),
                padding: UiRect::all(Val::Px(24.0)),
                row_gap: Val::Px(4.0),
                border: UiRect::left(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(PANEL),
            BorderColor::all(ACCENT),
        ));
}

fn open_menu(menu: &mut Menu, settings: &Settings, garage: &Garage) {
    menu.open = true;
    menu.page = Page::Main;
    menu.cursor = 0;
    let _ = garage;
    menu.saved = Some(settings.clone());
    menu.dirty = true;
}

fn close_menu(menu: &mut Menu, settings: &Settings, path: &SettingsPath) {
    menu.open = false;
    menu.dirty = true;
    if menu.saved.as_ref() != Some(settings) {
        if let Some(dir) = path.0.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_vec_pretty(settings) {
            // On the background writer (perf/writer.rs): a save never stalls a frame.
            Ok(b) => crate::perf::writer::replace(path.0.clone(), b),
            Err(e) => warn!("settings: {e}"),
        }
    }
    menu.saved = None;
}

fn adjust(settings: &mut Settings, o: Opt, dir: i32) {
    let delta = if dir < 0 { -1.0 } else { 1.0 };
    let step = |v: &mut f32| *v = (*v * 10.0 + delta).round().clamp(0.0, 10.0) / 10.0;
    match o {
        Opt::Units => settings.metric = !settings.metric,
        Opt::Tcs => settings.tcs = !settings.tcs,
        // Braking cycles Assisted -> ABS on -> ABS off (Assisted keeps ABS on).
        Opt::Abs => {
            let i = if settings.braking_assist { 0 } else if settings.abs { 1 } else { 2 };
            let i = (i + if dir < 0 { 2 } else { 1 }) % 3;
            settings.braking_assist = i == 0;
            settings.abs = i != 2;
        }
        Opt::DrivingLine => settings.driving_line = settings.driving_line.next(dir < 0),
        Opt::Quality => settings.graphics.quality = settings.graphics.quality.next(dir < 0),
        Opt::AntiAlias => settings.graphics.aa = settings.graphics.aa.next(dir < 0),
        Opt::RenderScale => graphics::step_scale(&mut settings.graphics, dir),
        Opt::AiDifficulty => settings.ai_difficulty = settings.ai_difficulty.next(dir < 0),
        Opt::Stm => settings.stm = !settings.stm,
        Opt::Rewind => settings.rewind = !settings.rewind,
        Opt::Steering => {
            const ALL: [SteeringAssist; 3] = [SteeringAssist::Assisted, SteeringAssist::Normal, SteeringAssist::Simulation];
            let i = ALL.iter().position(|&x| x == settings.steering).unwrap_or(1);
            settings.steering = ALL[(i as i32 + if dir < 0 { 2 } else { 1 }) as usize % 3];
        }
        Opt::Shifting => {
            const ALL: [Shifting; 3] = [Shifting::Automatic, Shifting::Manual, Shifting::ManualClutch];
            let i = ALL.iter().position(|&x| x == settings.shifting).unwrap_or(0);
            settings.shifting = ALL[(i as i32 + if dir < 0 { 2 } else { 1 }) as usize % 3];
        }
        Opt::Hud => settings.hud = !settings.hud,
        Opt::Telemetry => settings.telemetry = !settings.telemetry,
        // Enter (dir 0) steps up and wraps to 0 past 100%.
        Opt::EngineVolume | Opt::RadioVolume => {
            let v = if matches!(o, Opt::EngineVolume) { &mut settings.engine_volume } else { &mut settings.radio_volume };
            if dir == 0 && *v >= 0.999 {
                *v = 0.0;
            } else {
                step(v);
            }
        }
    }
}

#[derive(Default)]
struct Nav {
    vertical: i32,
    horizontal: i32,
    confirm: bool,
    back: bool,
    toggle: bool,
    /// Car browser sort order (Tab / Y).
    sort: bool,
}

fn read_nav(keys: &ButtonInput<KeyCode>, pads: &Query<&Gamepad>, menu: &mut Menu, dt: f32) -> Nav {
    let mut n = Nav::default();
    let kp = |k: KeyCode| keys.just_pressed(k);
    n.toggle = kp(KeyCode::Escape);
    n.back = kp(KeyCode::Backspace);
    n.confirm = kp(KeyCode::Enter) || kp(KeyCode::Space) || kp(KeyCode::NumpadEnter);
    n.sort = kp(KeyCode::Tab);
    if kp(KeyCode::ArrowLeft) || kp(KeyCode::KeyA) {
        n.horizontal = -1;
    }
    if kp(KeyCode::ArrowRight) || kp(KeyCode::KeyD) {
        n.horizontal = 1;
    }
    // Up/down auto-repeat while held (keys, D-pad or stick).
    let mut held = 0;
    if keys.pressed(KeyCode::ArrowUp) || keys.pressed(KeyCode::KeyW) {
        held = -1;
    }
    if keys.pressed(KeyCode::ArrowDown) || keys.pressed(KeyCode::KeyS) {
        held = 1;
    }
    for pad in pads {
        n.toggle |= pad.just_pressed(GamepadButton::Start);
        n.back |= pad.just_pressed(GamepadButton::East);
        n.confirm |= pad.just_pressed(GamepadButton::South);
        n.sort |= pad.just_pressed(GamepadButton::North);
        if pad.just_pressed(GamepadButton::DPadLeft) {
            n.horizontal = -1;
        }
        if pad.just_pressed(GamepadButton::DPadRight) {
            n.horizontal = 1;
        }
        let y = pad.get(GamepadAxis::LeftStickY).unwrap_or(0.0);
        if pad.pressed(GamepadButton::DPadUp) || y > 0.5 {
            held = -1;
        }
        if pad.pressed(GamepadButton::DPadDown) || y < -0.5 {
            held = 1;
        }
    }
    if held == 0 {
        menu.repeat = (0, 0.0);
    } else if menu.repeat.0 != held {
        menu.repeat = (held, 0.4);
        n.vertical = held;
    } else {
        menu.repeat.1 -= dt;
        if menu.repeat.1 <= 0.0 {
            menu.repeat.1 = 0.07;
            n.vertical = held;
        }
    }
    n
}

#[allow(clippy::too_many_arguments)]
fn menu_input(
    keys: Res<ButtonInput<KeyCode>>,
    pads: Query<&Gamepad>,
    mut menu: ResMut<Menu>,
    mut settings: ResMut<Settings>,
    path: Res<SettingsPath>,
    garage: Res<Garage>,
    (track, maps): (Res<Track>, Res<MapChoices>),
    mut rig: ResMut<CameraRig>,
    mut actions: MessageWriter<GameAction>,
    mut exit: MessageWriter<AppExit>,
    mut switch: ResMut<world_load::MapSwitch>,
    time: Res<Time<Real>>,
) {
    // L1: no pause menu under the launch screen / a loading cover (Start there means "press start").
    if loading::blocking() && !menu.open {
        return;
    }
    if rig.photo {
        // Leave photo mode back to driving.
        let leave = keys.just_pressed(KeyCode::Escape) || pads.iter().any(|p| p.just_pressed(GamepadButton::Start) || p.just_pressed(GamepadButton::East));
        if leave {
            rig.photo = false;
        }
        return;
    }
    let mut nav = read_nav(&keys, &pads, &mut menu, time.delta_secs());
    // Automation: `FH1_MENU_DOWN_AT=<s>` presses "down" once at that time (focus animation tests).
    if let Some(at) = std::env::var("FH1_MENU_DOWN_AT").ok().and_then(|v| v.parse::<f32>().ok()) {
        let t = time.elapsed_secs();
        if t >= at && t - time.delta_secs() < at {
            nav.vertical = 1;
        }
    }
    if !menu.open {
        if nav.toggle {
            open_menu(&mut menu, &settings, &garage);
        } else if worldmap::take_open_request() {
            open_menu(&mut menu, &settings, &garage);
            menu.page = Page::Map;
            menu.map_direct = true;
        }
        return;
    }
    if menu.page == Page::Map {
        // ui/worldmap.rs owns the page's input; it asks to leave (B / Back / Tab = back, Start / a fast travel = resume).
        match worldmap::take_close_request() {
            Some(worldmap::Leave::Back) if !menu.map_direct => {
                menu.page = Page::Main;
                menu.cursor = 2;
                menu.dirty = true;
            }
            Some(_) => {
                menu.map_direct = false;
                close_menu(&mut menu, &settings, &path);
            }
            None => {}
        }
        return;
    }
    if nav.toggle && menu.page == Page::Main {
        close_menu(&mut menu, &settings, &path);
        return;
    }
    if menu.page == Page::Cars {
        // Garage (ui/garage.rs): a car just bought is driven.
        if let Some(i) = menu.shop.drive.take() {
            browser_pick(browser::Pick::Car(i), &mut menu, &mut settings, &path, &garage, &track, &mut actions, &mut exit, &mut switch);
            return;
        }
        // The buy / sell dialog takes the input while it is up.
        if menu.shop.confirm.is_some() {
            if nav.confirm || nav.back || nav.toggle {
                menu.shop.answer(nav.confirm);
                menu.dirty = true;
            }
            return;
        }
        // My cars: Delete / R3 sells the selected car (after the dialog).
        let sell = keys.just_pressed(KeyCode::Delete) || pads.iter().any(|p| p.just_pressed(GamepadButton::RightThumb));
        if sell && menu.shop.mode == garage::Mode::Owned {
            if let Some(i) = menu.cars.as_ref().and_then(|b| b.selected()).map(|e| e.index) {
                menu.shop.ask_sell(i);
                menu.dirty = true;
            }
            return;
        }
    }
    if matches!(menu.page, Page::Cars | Page::Maps) && !nav.toggle {
        // B1: controller mapping of browser.rs (LB/RB games, LT/RT jumps, X/View filters, Y sort, held repeat).
        let input = browser::read_input(&keys, &pads, &mut menu.browse_repeat, time.delta_secs());
        let pick = match menu.page {
            Page::Cars => menu.cars.as_mut().map_or(browser::Pick::Leave, |b| b.step(input)),
            _ => menu.maps.step(input),
        };
        browser_pick(pick, &mut menu, &mut settings, &path, &garage, &track, &mut actions, &mut exit, &mut switch);
        return;
    }
    if menu.page == Page::Customize {
        let step = menu.custom.step(customize::Nav { vertical: nav.vertical, horizontal: nav.horizontal, confirm: nav.confirm, back: nav.back || nav.toggle });
        match step {
            customize::Step::Leave => {
                menu.page = Page::Garage;
                menu.cursor = garage::customize_row();
                menu.dirty = true;
            }
            customize::Step::Stay { changed } => menu.dirty |= changed,
        }
        return;
    }
    if nav.back || nav.toggle {
        if menu.page == Page::Main {
            close_menu(&mut menu, &settings, &path);
        } else {
            let from = menu.page;
            // Graphics is a sub-page of Options: back returns to its row there.
            menu.page = if from == Page::Graphics { Page::Options } else { Page::Main };
            menu.cursor = match from {
                Page::FastTravel | Page::Map => 2,
                Page::Cars | Page::Garage | Page::Customize => 3,
                Page::Options | Page::Controls | Page::Maps => 5,
                Page::Graphics => GRAPHICS_ROW,
                Page::Main => 0,
            };
            menu.dirty = true;
        }
        return;
    }
    let (rows, first) = items(&menu, &settings, &garage, &track, &maps);
    if nav.vertical != 0 {
        if is_list(menu.page) {
            let n = if menu.page == Page::FastTravel { track.spawn_names.len() } else { maps.0.len() };
            menu.cursor = (menu.cursor as i32 + nav.vertical).rem_euclid(n.max(1) as i32) as usize;
        } else {
            let n = rows.len() as i32;
            menu.cursor = (menu.cursor as i32 + nav.vertical).rem_euclid(n) as usize;
        }
        menu.dirty = true;
    }
    let sel = selected_row(&menu, first);
    let Some(row) = rows.get(sel) else { return };
    if nav.horizontal != 0 {
        if let Act::Opt(o) = row.act {
            adjust(&mut settings, o, nav.horizontal);
            menu.dirty = true;
        }
    }
    if nav.confirm {
        activate(row.act, &mut menu, &mut settings, &path, &garage, &track, &mut rig, &mut actions, &mut exit, &mut switch);
    }
}

/// Carry out a browser step: a car or map choice, or leaving the browser for the main page.
#[allow(clippy::too_many_arguments)]
fn browser_pick(
    pick: browser::Pick,
    menu: &mut Menu,
    settings: &mut Settings,
    path: &SettingsPath,
    garage: &Garage,
    track: &Track,
    actions: &mut MessageWriter<GameAction>,
    exit: &mut MessageWriter<AppExit>,
    switch: &mut world_load::MapSwitch,
) {
    match pick {
        browser::Pick::Browsing { changed } => menu.dirty |= changed,
        browser::Pick::Leave => {
            if menu.page == Page::Cars && customize::enabled() {
                menu.cursor = if menu.shop.mode == garage::Mode::Shop { 1 } else { 0 };
                menu.page = Page::Garage;
            } else {
                menu.cursor = if menu.page == Page::Cars { 3 } else { 5 };
                menu.page = Page::Main;
            }
            menu.dirty = true;
        }
        browser::Pick::Car(i) => {
            // Autoshow: a car not owned asks to buy it first (ui/garage.rs).
            if menu.page == Page::Cars && !menu.shop.pick(i) {
                menu.dirty = true;
                return;
            }
            if i != garage.current {
                actions.write(GameAction::SelectCar(i));
            }
            // L1b: remembered for the main menu (Horizon starts with the last car).
            settings.car = garage.cars.get(i).cloned();
            close_menu(menu, settings, path);
        }
        browser::Pick::Map(id) => {
            if id == track.id {
                close_menu(menu, settings, path);
                return;
            }
            // Saved first, so later plain launches open this map; the switch itself is in-process (X1c:
            // world_load::switch_world unloads this world and loads the new one behind the loading card).
            settings.map = Some(id.clone());
            close_menu(menu, settings, path);
            info!("switching map to {id} in-process");
            switch.0 = Some(id);
            let _ = exit;
        }
    }
}

/// Index of the selected row among the shown rows.
fn selected_row(menu: &Menu, first: usize) -> usize {
    match menu.page {
        Page::Cars => menu.cars.as_ref().map_or(0, |b| b.cursor.saturating_sub(first)),
        Page::Maps => menu.maps.cursor.saturating_sub(first),
        Page::Customize => menu.custom.cursor.saturating_sub(first),
        p if is_list(p) => menu.cursor - first,
        _ => menu.cursor,
    }
}

#[allow(clippy::too_many_arguments)]
fn activate(
    act: Act,
    menu: &mut Menu,
    settings: &mut Settings,
    path: &SettingsPath,
    garage: &Garage,
    track: &Track,
    rig: &mut CameraRig,
    actions: &mut MessageWriter<GameAction>,
    exit: &mut MessageWriter<AppExit>,
    switch: &mut world_load::MapSwitch,
) {
    match act {
        Act::Resume => close_menu(menu, settings, path),
        Act::Restart => {
            actions.write(GameAction::Restart);
            close_menu(menu, settings, path);
        }
        Act::Open(p) => {
            menu.page = p;
            menu.cursor = 0;
            if p == Page::Cars {
                menu.shop.open(garage::Mode::All);
            }
            if matches!(p, Page::Cars | Page::Maps) {
                menu.open_browsers(garage, &track.id);
            }
            if p == Page::Customize {
                menu.custom.open();
            }
            menu.dirty = true;
        }
        Act::Cars(mode) => {
            menu.page = Page::Cars;
            menu.cursor = 0;
            menu.shop.open(mode);
            // ui/garage.rs replaces this list with the mode's (same frame, before drawing).
            menu.open_browsers(garage, &track.id);
            menu.dirty = true;
        }
        Act::Custom(k) => {
            let _ = menu.custom.activate(k);
            menu.dirty = true;
        }
        Act::Browse(k) => {
            let pick = match menu.page {
                Page::Cars => menu.cars.as_mut().map_or(browser::Pick::Browsing { changed: false }, |b| b.click(k)),
                _ => menu.maps.click(k),
            };
            browser_pick(pick, menu, settings, path, garage, track, actions, exit, switch);
        }
        Act::Photo => {
            close_menu(menu, settings, path);
            rig.photo = true;
        }
        Act::Quit => {
            close_menu(menu, settings, path);
            exit.write(AppExit::Success);
        }
        Act::Opt(o) => {
            adjust(settings, o, 0);
            menu.dirty = true;
        }
        Act::Travel(i) => {
            actions.write(GameAction::FastTravel(i));
            close_menu(menu, settings, path);
        }
        Act::None => {}
    }
}

/// Mouse: moving onto a row selects it, clicking activates it. A row that appears under a resting
/// pointer (menu opened, list scrolled) doesn't steal the selection.
#[allow(clippy::too_many_arguments)]
fn menu_mouse(
    rows: Query<(&Interaction, &MenuRow), Changed<Interaction>>,
    motion: Res<bevy::input::mouse::AccumulatedMouseMotion>,
    mut menu: ResMut<Menu>,
    mut settings: ResMut<Settings>,
    path: Res<SettingsPath>,
    garage: Res<Garage>,
    (track, maps): (Res<Track>, Res<MapChoices>),
    mut rig: ResMut<CameraRig>,
    mut actions: MessageWriter<GameAction>,
    mut exit: MessageWriter<AppExit>,
    mut switch: ResMut<world_load::MapSwitch>,
) {
    if !menu.open {
        return;
    }
    for (interaction, row) in &rows {
        let (list, first) = items(&menu, &settings, &garage, &track, &maps);
        let Some(it) = list.get(row.0) else { continue };
        let act = it.act;
        match interaction {
            Interaction::Hovered if motion.delta != Vec2::ZERO => {
                if menu.page == Page::Cars {
                    if let Some(b) = menu.cars.as_mut() {
                        b.cursor = first + row.0;
                    }
                } else if menu.page == Page::Maps {
                    menu.maps.cursor = first + row.0;
                } else if menu.page == Page::Customize {
                    menu.custom.hover(first + row.0);
                } else if is_list(menu.page) {
                    menu.cursor = first + row.0;
                } else {
                    menu.cursor = row.0;
                }
                menu.dirty = true;
            }
            Interaction::Pressed => {
                activate(act, &mut menu, &mut settings, &path, &garage, &track, &mut rig, &mut actions, &mut exit, &mut switch);
                return;
            }
            _ => {}
        }
    }
}

/// Pause the simulation (virtual time) while the menu or photo mode is up.
fn sync_pause(menu: Res<Menu>, rig: Res<CameraRig>, mut virt: ResMut<Time<Virtual>>, mut input: ResMut<Input>) {
    let want = menu.open || rig.photo;
    if want != virt.is_paused() {
        if want {
            virt.pause();
            let (tcs, abs) = (input.0.tcs, input.0.abs);
            input.0 = crate::vehicle::Controls { tcs, abs, ..default() };
        } else {
            virt.unpause();
        }
    }
}

fn draw_menu(
    mut commands: Commands,
    mut menu: ResMut<Menu>,
    settings: Res<Settings>,
    garage: Res<Garage>,
    (track, maps): (Res<Track>, Res<MapChoices>),
    font: Res<UiFont>,
    mut root: Query<&mut Visibility, With<MenuRoot>>,
    panel: Query<Entity, With<MenuPanel>>,
    fh1: Option<Res<Fh1Pause>>,
    (profile, asset_server, mut studio, images): (Option<Res<crate::progression::Profile>>, Res<AssetServer>, Option<ResMut<thumbs::ThumbStudio>>, Res<Assets<Image>>),
) {
    if !menu.dirty && !(menu.open && settings.is_changed()) {
        return;
    }
    menu.dirty = false;
    let Ok(mut vis) = root.single_mut() else { return };
    // The main page is FH1's own scene when the `ui` group is installed.
    let placeholder = menu.open && !(menu.page == Page::Main && fh1.is_some()) && menu.page != Page::Map;
    *vis = if placeholder { Visibility::Inherited } else { Visibility::Hidden };
    let Ok(panel) = panel.single() else { return };
    commands.entity(panel).despawn_children();
    if !placeholder {
        return;
    }
    let (rows, first) = items(&menu, &settings, &garage, &track, &maps);
    let sel = selected_row(&menu, first);
    let car = &garage.cars[garage.current];
    commands.entity(panel).with_children(|p| {
        p.spawn((Text::new(page_title(menu.page)), font.text(44.0), TextColor(Color::WHITE)));
        let sub = match menu.page {
            Page::Cars => menu.cars.as_ref().map(|b| b.title()).unwrap_or_default(),
            Page::FastTravel => format!("{}  ·  {} / {}", track.name, menu.cursor + 1, track.spawn_names.len()),
            Page::Maps => menu.maps.title(),
            Page::Customize => menu.custom.title(),
            _ => car.clone(),
        };
        // Garage pages: the credits balance (progression::wallet).
        let shop_page = matches!(menu.page, Page::Garage | Page::Customize) || (menu.page == Page::Cars && menu.shop.mode != garage::Mode::All);
        let sub = match profile.as_deref().filter(|_| shop_page && garage::shop_on()) {
            Some(pr) => format!("{sub}    ·    {} CR", crate::progression::fmt_num(crate::progression::wallet::credits(pr))),
            None => sub,
        };
        p.spawn((Text::new(sub), font.text(16.0), TextColor(DIM), Node { margin: UiRect::bottom(Val::Px(14.0)), ..default() }));
        if menu.page == Page::Cars {
            if let Some(n) = &menu.shop.notice {
                p.spawn((Text::new(n.clone()), font.text(18.0), TextColor(ACCENT), Node { margin: UiRect::bottom(Val::Px(10.0)), ..default() }));
            }
            // The buy / sell dialog replaces the list.
            if let Some(c) = &menu.shop.confirm {
                p.spawn((Text::new(c.title.clone()), font.text(30.0), TextColor(Color::WHITE), Node { margin: UiRect::top(Val::Px(20.0)), ..default() }));
                p.spawn((Text::new(c.message.clone()), font.text(20.0), TextColor(Color::WHITE), Node { margin: UiRect::vertical(Val::Px(12.0)), max_width: Val::Px(760.0), ..default() }));
                p.spawn((Text::new("Enter / A  confirm      Esc / B  cancel"), font.text(15.0), TextColor(DIM), Node { margin: UiRect::top(Val::Px(16.0)), ..default() }));
                return;
            }
        }
        if menu.page == Page::Cars {
            // The car page: rows on the left, the selected car's photo and details on the right.
            p.spawn(Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(28.0), align_items: AlignItems::FlexStart, ..default() }).with_children(|c| {
                c.spawn(Node { flex_direction: FlexDirection::Column, ..default() }).with_children(|l| spawn_rows(l, &menu, &rows, sel, first, &font));
                let Some(b) = menu.cars.as_ref() else { return };
                let selected = b.selected();
                let view = b.view();
                // The game's own photo for FH1 cars, else one rendered by the thumbnail studio (imported cars).
                let photo = match selected {
                    Some(e) if e.id.is_some() => Some(asset_server.load(format!("ui/textures/thumbnails/thumbnail_{}.png", e.id.unwrap_or(0)))),
                    Some(e) => studio.as_deref_mut().zip(garage.cars.get(e.index)).and_then(|(s, car)| s.photo(car, &images)),
                    None => None,
                };
                c.spawn(Node { flex_direction: FlexDirection::Column, row_gap: Val::Px(4.0), width: Val::Px(384.0), ..default() }).with_children(|d| {
                    if let Some(image) = photo {
                        d.spawn((
                            ImageNode { image, image_mode: NodeImageMode::Stretch, ..default() },
                            Node { width: Val::Px(384.0), height: Val::Px(144.0), margin: UiRect::bottom(Val::Px(8.0)), ..default() },
                        ));
                    }
                    if let Some((title, lines)) = view.details {
                        d.spawn((Text::new(title), font.text(20.0), TextColor(Color::WHITE)));
                        for (k, v) in lines {
                            let line = if k.is_empty() { v } else { format!("{k}:  {v}") };
                            d.spawn((Text::new(line), font.text(16.0), TextColor(DIM)));
                        }
                    }
                });
            });
        } else {
            spawn_rows(p, &menu, &rows, sel, first, &font);
        }
        let hint: String = match menu.page {
            Page::Main => "Enter / A  select      Esc / B  resume".into(),
            Page::Options | Page::Graphics => "Left / Right  change      Esc / B  back".into(),
            Page::Cars => menu.cars.as_ref().map_or("Esc / B  back".into(), |b| format!("{}{}", b.hint(), menu.shop.hint_extra())),
            Page::Maps => menu.maps.hint(),
            Page::Customize => menu.custom.hint(),
            _ => "Enter / A  select      Esc / B  back".into(),
        };
        p.spawn((Text::new(hint), font.text(15.0), TextColor(DIM), Node { margin: UiRect::top(Val::Px(16.0)), ..default() }));
    });
}

/// The page's rows under `p` (the panel, or the car page's list column).
fn spawn_rows(p: &mut ChildSpawnerCommands, menu: &Menu, rows: &[Item], sel: usize, first: usize, font: &UiFont) {
    for (i, it) in rows.iter().enumerate() {
        let on = i == sel;
        // Browser rows of optional games that aren't imported: greyed, a faint cursor instead of the accent.
        let locked = match menu.page {
            Page::Cars => menu.cars.as_ref().is_some_and(|b| b.locked_row(first + i)),
            Page::Maps => menu.maps.locked_row(first + i),
            _ => false,
        };
        let fg = match (locked, on) {
            (true, true) => browser::LOCKED_ON,
            (true, false) => browser::LOCKED,
            (false, true) => Color::WHITE,
            (false, false) => Color::srgba(1.0, 1.0, 1.0, 0.8),
        };
        let selectable = !matches!(it.act, Act::None);
        let mut row = p.spawn((
            MenuRow(i),
            Node {
                height: Val::Px(if menu.page == Page::Controls { 32.0 } else { 42.0 }),
                padding: UiRect::horizontal(Val::Px(14.0)),
                column_gap: Val::Px(40.0),
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(if on && locked { browser::LOCKED_CURSOR } else if on && selectable { ACCENT } else { Color::NONE }),
        ));
        if selectable {
            row.insert(Button);
        }
        row.with_children(|r| {
            r.spawn((Text::new(it.label.clone()), font.text(22.0), TextColor(fg)));
            if let Some(v) = &it.value {
                let v = if on && matches!(it.act, Act::Opt(_)) { format!("‹  {v}  ›") } else { v.clone() };
                let size = if menu.page == Page::Controls { 16.0 } else { 20.0 };
                // Customize paint rows: the value in the paint's own colour.
                let swatch = if menu.page == Page::Customize { menu.custom.rows.get(first + i).and_then(|r| r.swatch) } else { None };
                r.spawn((Text::new(v), font.text(size), TextColor(swatch.unwrap_or(if on { Color::WHITE } else { DIM }))));
            }
        });
    }
}

/// Startup: the game always runs the Remaster renderer (the Original and RTX choices are retired; settings.json
/// `renderer` / `original_shaders` are ignored). `FH1_RENDERER` stays a developer override. Must run before any thread
/// reads the variable (fh1_remaster::enabled, fh1_render::remaster).
pub fn apply_renderer_setting(_settings_path: &std::path::Path) {
    if std::env::var_os("FH1_RENDERER").is_none() {
        std::env::set_var("FH1_RENDERER", "remaster");
    }
}
