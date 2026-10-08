//! L1 loading screens: one full-screen overlay (Bevy UI on the UI camera, above the HUD and the pause menu) that
//! covers the world while it isn't ready to be seen, with FH1's own loading backdrops and logo when the `ui` group
//! is installed.
//!
//! Covers (what triggers each, what ends it):
//! - **Launch** (launch.rs): at startup, the title and "Press Start / Enter" while the world loads behind it.
//!   A press fades into the game if the world is ready, else it turns into the Startup card.
//! - **Startup**: at startup without the launch screen, or after the press. Ends when the player car is in
//!   (its wheels are tagged = its glTF scene spawned) and the scenery around it is ready (`Scenery::readiness`: zone
//!   complete and shown, prop templates loaded, the props right around the car placed), held for `SETTLE_S`, and the
//!   traffic pool is pre-built (`traffic::TrafficWarm`, "Preparing traffic n/N"; traffic/plugin.rs has its own timeout).
//! - **Race**: a race starting (RacePhase Idle -> Grid: the grid teleport, barrier objects + colliders, AI spawn).
//!   Ends like Startup at the grid, after at least `RACE_MIN_S`. `ui::driving` is false while covered, so race.rs's grid
//!   timer only runs after the card (and R1's grid settle is shortened to `race::GRID_SETTLE_LOADER_S`).
//! - **Travel**: the player car jumping more than `JUMP_M` in a frame (fast travel, restart, G, FH1_TELEPORT, the
//!   post-race teleport = "Free roam", race resets) to a place whose scenery isn't ready. Small moves into ready areas
//!   show nothing.
//! - **Car**: a new player car entity (car select, N/P) whose model isn't in yet.
//!
//! The teleport has already happened when a transition is seen, so the overlay cuts in (alpha 1 the same frame,
//! before anything of the unstreamed area is drawn) and fades out over `FADE_S`. Every cover has a timeout, so it
//! can never hang. Driving input is blocked while covered (and through the launch screen's fade); the pause menu
//! can't open meanwhile.
//!
//! Flags: `FH1_LOADING=0` = no loading covers (launch screen still per `FH1_LAUNCH_SCREEN`); `=1` = on even under
//! screenshot / perf automation (FH1_SHOT, FH1_P2_*, FH1_PERF_TOUR, FH1_P6_AB, FH1_MENU, FH1_UI_SCENE turn it off by
//! default, since an overlay would end up in their shots or timings).
//!
//! Polish (user, 2026-10-06: "the load screen is there to hide it"):
//! - **World audio gate** ([`world_audio_allowed`]): while a cover is up (main menu or loading card, not its fade-out)
//!   the radio is paused (radio.rs, the game's SetRadioPaused) and car / AI / traffic engine audio is at volume 0
//!   (audio.rs). `FH1_MENU_AUDIO_GATE=0` = old behaviour (radio and engines audible behind the covers).
//! - **Preloaded images** ([`LoadingImages`]): the logo, the spinner and all ten backdrops are requested at boot and
//!   held by strong handles for the whole run (never unloaded, never decoded again); the first cover's contents fade
//!   in once they are decoded (at most `REVEAL_WAIT_S`), so nothing pops in. `FH1_LOADING_PRELOAD=0` = old lazy load.
//! - No background-work text on the card: "Preparing traffic n/N" only with `FH1_LOADING_DETAIL=1`. The overlay sits
//!   above the F3 diagnostics (FPS / CPU / VRAM) while it covers.
//! - Frame pacing under a cover is logged when it ends ("loading: Startup frames: ...").

use std::sync::atomic::{AtomicBool, Ordering};

use bevy::input::gamepad::Gamepad;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use super::launch::{self, LaunchPart, PressPrompt};
use super::{UiFont, ACCENT};
use crate::race::{Events, RacePhase, RaceState};
use crate::track::Track;
use crate::{Car, Garage, Input, WheelVisual};

/// Fade out (s).
const FADE_S: f32 = 0.3;
/// The world must stay ready this long before the fade (new meshes / pipelines get a few frames to appear).
const SETTLE_S: f32 = 0.4;
/// Minimum time on screen per cover (s): a card that flashes for one frame looks like a glitch.
const STARTUP_MIN_S: f32 = 0.8;
const RACE_MIN_S: f32 = 1.5;
const TRAVEL_MIN_S: f32 = 0.35;
/// Never cover longer than this (s).
const STARTUP_TIMEOUT_S: f32 = 45.0;
const TRANSITION_TIMEOUT_S: f32 = 15.0;
/// A car without wheels tagged after this long counts as ready (s).
const CAR_TIMEOUT_S: f32 = 8.0;
/// A per-frame move above this is a teleport (m).
const JUMP_M: f32 = 60.0;
/// FH1's loading backdrops (`ui/textures/horizon/loading/bgloadingimages/image<n>.png`, 1280x720).
const BACKDROPS: u32 = 10;

static BLOCK: AtomicBool = AtomicBool::new(false);
/// A cover is up and not fading out (the world behind it must not be heard).
static COVERED: AtomicBool = AtomicBool::new(false);
/// First cover: wait at most this long for the preloaded images before showing its contents (s).
const REVEAL_WAIT_S: f32 = 3.0;
/// Fade-in of the first cover's contents once the images are in (s).
const REVEAL_FADE_S: f32 = 0.25;

/// `FH1_MENU_AUDIO_GATE` (default on).
fn audio_gate_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_MENU_AUDIO_GATE").as_deref(), Ok("0") | Ok("off")))
}

/// `FH1_LOADING_PRELOAD` (default on).
fn preload_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_LOADING_PRELOAD").as_deref(), Ok("0") | Ok("off")))
}

/// `FH1_LOADING_DETAIL=1`: show background-work detail on the card ("Preparing traffic n/N").
fn detail_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("FH1_LOADING_DETAIL").as_deref(), Ok("1") | Ok("on")))
}

/// `FH1_BOOT_HIDDEN` (default on).
fn boot_hidden_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_BOOT_HIDDEN").as_deref(), Ok("0") | Ok("off")))
}

/// Boot: shows the window (hidden in `LoadingPlugin::build`) two frames after Bevy UI's pipeline has compiled and the
/// preloaded images are in, so its first visible frame is the finished main menu / loading card. Never later than
/// `HOLD_MAX_S` + 1 s.
fn make_visible(mut windows: Query<&mut Window, With<PrimaryWindow>>, ld: Res<Loading>, mut ready_frames: Local<u32>, mut done: Local<bool>) {
    if *done {
        return;
    }
    let Ok(mut w) = windows.single_mut() else { return };
    if w.visible {
        *done = true;
        return;
    }
    let t = START.get().map_or(0.0, |t| t.elapsed().as_secs_f32());
    if UI_READY.load(Ordering::Relaxed) && ld.images_ready {
        *ready_frames += 1;
    }
    if *ready_frames >= 2 || t > HOLD_MAX_S + 1.0 {
        info!("loading: window shown at {t:.2} s (UI pipeline {}, images {})", UI_READY.load(Ordering::Relaxed), ld.images_ready);
        w.visible = true;
        *done = true;
    }
}

/// Whether the world may be heard: false while the main menu or a loading card covers it (radio paused, car / AI /
/// traffic engines silent). True during a cover's fade-out, so sound comes back with the picture.
pub fn world_audio_allowed() -> bool {
    !audio_gate_on() || !COVERED.load(Ordering::Relaxed)
}

/// The loading screen's images, requested at boot and kept alive (strong handles) for the whole run.
#[derive(Resource, Default)]
pub struct LoadingImages {
    pub logo: Option<Handle<Image>>,
    pub spinner: Option<Handle<Image>>,
    pub backdrops: Vec<Handle<Image>>,
}

impl LoadingImages {
    fn all(&self) -> impl Iterator<Item = &Handle<Image>> {
        self.logo.iter().chain(self.spinner.iter()).chain(self.backdrops.iter())
    }
}

/// Frame pacing while a cover is up: frames, worst frame and frames over 50 / 100 ms.
#[derive(Default)]
struct CoverFrames {
    frames: u32,
    worst_ms: f32,
    over_50: u32,
    over_100: u32,
    sum_ms: f32,
}
/// Bevy UI's pipeline has compiled (set from the render world). Pipelines compile on the async compute pool, which
/// the scenery's tile / zone / texture reads fill at startup: the overlay stayed undrawn for seconds (seen at 5 s).
static UI_READY: AtomicBool = AtomicBool::new(false);
/// Startup hold: scenery streaming waits for the overlay's pipeline at most this long (s).
const HOLD_MAX_S: f32 = 4.0;
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Hook for engine scenery.rs `stream`: true while streaming should wait so the startup cover can be drawn first.
pub fn hold_streaming() -> bool {
    if UI_READY.load(Ordering::Relaxed) || !BLOCK.load(Ordering::Relaxed) {
        return false;
    }
    let held = START.get().map_or(0.0, |t| t.elapsed().as_secs_f32());
    if held >= HOLD_MAX_S {
        UI_READY.store(true, Ordering::Relaxed);
        warn!("loading: UI pipeline not ready after {HOLD_MAX_S} s; streaming anyway");
        return false;
    }
    true
}

/// Render pipelines still queued / compiling (counted while a cover is up; read by the settle check).
static PIPELINES_PENDING: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Settle check (`FH1_LOADING_SETTLE`, default on): a cover only fades once the world is ready AND the frame is calm:
/// `SETTLE_FRAMES` consecutive frames under the calm threshold, no render pipeline compiling, no imported tile still
/// waiting for its scene. Never more than `SETTLE_CAP_S` past world-ready.
fn settle_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_LOADING_SETTLE").as_deref(), Ok("0") | Ok("off")))
}
const SETTLE_FRAMES: u32 = 12;
/// Calm frame: under this (ms), or under 1.6x the median of the last frames on a machine that runs slower anyway.
const CALM_MS: f32 = 25.0;
const SETTLE_CAP_S: f32 = 10.0;

/// Render world: count pipelines still compiling while covered.
fn watch_pipelines(cache: Res<bevy::render::render_resource::PipelineCache>) {
    use bevy::render::render_resource::CachedPipelineState;
    if !COVERED.load(Ordering::Relaxed) {
        return;
    }
    let n = cache.pipelines().filter(|p| matches!(p.state, CachedPipelineState::Queued | CachedPipelineState::Creating(_))).count();
    PIPELINES_PENDING.store(n as u32, Ordering::Relaxed);
}

/// Render world: note when every `ui_pipeline` variant has compiled.
fn watch_ui_pipeline(cache: Res<bevy::render::render_resource::PipelineCache>) {
    use bevy::render::render_resource::{CachedPipelineState, PipelineDescriptor};
    if UI_READY.load(Ordering::Relaxed) {
        return;
    }
    let (mut ok, mut waiting) = (0, 0);
    for p in cache.pipelines() {
        if let PipelineDescriptor::RenderPipelineDescriptor(d) = &p.descriptor {
            if d.label.as_deref() == Some("ui_pipeline") {
                if matches!(p.state, CachedPipelineState::Ok(_)) {
                    ok += 1;
                } else {
                    waiting += 1;
                }
            }
        }
    }
    if ok > 0 && waiting == 0 {
        let t = START.get().map_or(0.0, |t| t.elapsed().as_secs_f32());
        info!("loading: UI pipeline ready at {t:.2} s");
        UI_READY.store(true, Ordering::Relaxed);
    }
}

/// True while a cover blocks driving input and the pause menu (read by `ui::driving` and `menu_input`).
pub fn blocking() -> bool {
    BLOCK.load(Ordering::Relaxed)
}

/// Whether loading covers are on this run (`FH1_LOADING`).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("FH1_LOADING").as_deref() {
        Ok("0") | Ok("off") => false,
        Ok("1") | Ok("on") => true,
        _ => {
            let shots = ["FH1_SHOT", "FH1_PERF_TOUR", "FH1_P6_AB", "FH1_MENU", "FH1_UI_SCENE"].iter().any(|k| std::env::var_os(k).is_some());
            !shots && !std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("FH1_P2_"))
        }
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cover {
    Launch,
    Startup,
    Race,
    Travel,
    Car,
}

/// Overlay image / text with its base alpha (multiplied by the overlay's fade).
#[derive(Component)]
pub struct Fade(pub f32);

/// Overlay node background with its base alpha. Separate from `Fade`: every UI node carries a (transparent black)
/// BackgroundColor, which must stay transparent on images and texts.
#[derive(Component)]
pub struct FadeBg(pub f32);

/// Drop shadow for overlay texts (faded with them).
pub fn shadow() -> TextShadow {
    TextShadow { offset: Vec2::splat(2.0), color: Color::linear_rgba(0.0, 0.0, 0.0, SHADOW_A) }
}
const SHADOW_A: f32 = 0.6;

#[derive(Component)]
pub struct LoadingRoot;

/// Loading-card widgets (hidden on the launch screen).
#[derive(Component)]
struct CardPart;

#[derive(Component)]
struct Backdrop;

#[derive(Component)]
struct CardTitle;

#[derive(Component)]
struct CardSub;

#[derive(Component)]
struct CardTip;

#[derive(Component)]
struct Spinner;

#[derive(Component)]
struct ProgressFill;

#[derive(Resource)]
pub struct Loading {
    pub cover: Option<Cover>,
    fading_out: bool,
    alpha: f32,
    /// Real time the cover started, and since when the world has been ready.
    since: f32,
    ready_since: Option<f32>,
    progress: f32,
    /// The cover started as the launch screen (input stays blocked through its fade).
    from_launch: bool,
    title: String,
    sub: String,
    tip: String,
    texts_dirty: bool,
    backdrop: Option<Handle<Image>>,
    backdrop_dirty: bool,
    seed: u32,
    /// Transition detection: last player position, car entity (+ when it appeared) and race phase.
    last_pos: Option<Vec3>,
    last_car: Option<Entity>,
    car_since: f32,
    last_idle: bool,
    transitions: bool,
    /// The first cover's contents (everything but the opaque root): 0 until the preloaded images are in, then
    /// fades to 1 over `REVEAL_FADE_S` (stays 1 for the rest of the run).
    reveal: f32,
    /// The preloaded images are decoded (or failed, or `REVEAL_WAIT_S` passed).
    images_ready: bool,
    /// Frame pacing of the current cover.
    pacing: CoverFrames,
    /// Consecutive calm frames and the last frame times (settle check).
    calm_frames: u32,
    recent_ms: std::collections::VecDeque<f32>,
    /// Hitches in the first `AFTER_S` after a cover ends are logged (did it fade too early?).
    after_until: f32,
}

const AFTER_S: f32 = 5.0;

pub struct LoadingPlugin;

impl Plugin for LoadingPlugin {
    fn build(&self, app: &mut App) {
        let launch_on = launch::enabled();
        let loading_on = enabled();
        let cover = if launch_on {
            Some(Cover::Launch)
        } else if loading_on {
            Some(Cover::Startup)
        } else {
            None
        };
        BLOCK.store(cover.is_some(), Ordering::Relaxed);
        COVERED.store(cover.is_some(), Ordering::Relaxed);
        let _ = START.set(std::time::Instant::now());
        // Boot: keep the window hidden until the overlay can be drawn (no frames of the world / an empty window
        // before the main menu). `make_visible` shows it; FH1_BOOT_HIDDEN=0 = old behaviour.
        if cover.is_some() && boot_hidden_on() {
            let mut q = app.world_mut().query_filtered::<&mut Window, With<PrimaryWindow>>();
            if let Ok(mut w) = q.single_mut(app.world_mut()) {
                w.visible = false;
                info!("loading: window hidden until the overlay is ready");
            }
        }
        if cover.is_none() {
            UI_READY.store(true, Ordering::Relaxed);
        } else if let Some(render_app) = app.get_sub_app_mut(bevy::render::RenderApp) {
            render_app.add_systems(bevy::render::Render, watch_ui_pipeline.in_set(bevy::render::RenderSystems::Cleanup));
        }
        if (launch_on || loading_on) && settle_on() {
            if let Some(render_app) = app.get_sub_app_mut(bevy::render::RenderApp) {
                render_app.add_systems(bevy::render::Render, watch_pipelines.in_set(bevy::render::RenderSystems::Cleanup));
            }
        }
        info!("loading: launch screen {}, loading covers {}", if launch_on { "on" } else { "off" }, if loading_on { "on" } else { "off" });
        let assets = app.world().get_resource::<Garage>().map(|g| g.assets.clone()).unwrap_or_default();
        app.insert_resource(launch::MainMenu::new(&assets));
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(7, |d| d.subsec_nanos() ^ d.as_secs() as u32);
        app.insert_resource(Loading {
            cover,
            fading_out: false,
            alpha: if cover.is_some() { 1.0 } else { 0.0 },
            since: 0.0,
            ready_since: None,
            progress: 0.0,
            from_launch: launch_on,
            title: String::new(),
            sub: String::new(),
            tip: String::new(),
            texts_dirty: true,
            backdrop: None,
            backdrop_dirty: true,
            seed,
            last_pos: None,
            last_car: None,
            car_since: 0.0,
            last_idle: true,
            transitions: loading_on,
            reveal: if cover.is_some() && preload_on() { 0.0 } else { 1.0 },
            images_ready: !preload_on(),
            pacing: CoverFrames::default(),
            calm_frames: 0,
            recent_ms: Default::default(),
            after_until: 0.0,
        })
        .add_systems(Startup, spawn_overlay)
        .add_systems(Update, make_visible)
        .add_systems(
            Update,
            (
                launch::main_menu.before(super::world_load::apply_choice),
                update_loading.after(crate::apply_actions).after(crate::race::race_update).after(crate::objects::teleport),
                draw_overlay,
                launch::draw_main_menu,
            )
                .chain(),
        );
    }
}

/// Short tips about this game's controls (FH1's own tips are about features not built yet).
const TIPS: [&str; 9] = [
    "Esc / Start opens the pause menu: fast travel, car select, options and photo mode.",
    "Stop inside a festival event marker and press A / Enter to start a race. F6 lists every event.",
    "RB / C cycles the camera: chase, hood, bumper and cockpit views.",
    "N / P switch cars. G jumps to the next start location.",
    "Off the road in a race? Y / R puts you back on the track.",
    "ABS, traction control, steering and shifting assists are in Options.",
    "Hold Back to rewind a mistake.",
    "F opens photo mode; A or F12 saves a screenshot.",
    "D-pad left / right changes the radio station.",
];

impl Loading {
    /// The launch screen / main menu is up (not fading out).
    pub fn on_launch(&self) -> bool {
        self.cover == Some(Cover::Launch) && !self.fading_out
    }

    /// Main menu choice made: the loading card covers the world load (input stays blocked through its fade).
    pub fn start_world(&mut self, now: f32, title: String, sub: String) {
        self.start(Cover::Startup, now, title, sub);
        self.from_launch = true;
    }

    /// Main menu choice = the world already loading behind it: fade out when ready, else show the card.
    pub fn continue_loaded(&mut self, now: f32, title: String, sub: String) {
        if self.ready_since.is_some_and(|t| now - t >= SETTLE_S) || !self.transitions {
            self.fading_out = true;
        } else {
            self.start_world(now, title, sub);
        }
    }

    fn next_rand(&mut self) -> u32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        self.seed
    }

    fn start(&mut self, cover: Cover, now: f32, title: String, sub: String) {
        info!("loading: {cover:?} cover ({title})");
        self.cover = Some(cover);
        self.fading_out = false;
        self.alpha = 1.0;
        self.since = now;
        self.ready_since = None;
        self.progress = 0.0;
        self.from_launch = false;
        self.pacing = CoverFrames::default();
        self.calm_frames = 0;
        self.set_texts(title, sub);
    }

    fn set_texts(&mut self, title: String, sub: String) {
        self.title = title;
        self.sub = sub;
        self.tip = TIPS[self.next_rand() as usize % TIPS.len()].to_string();
        self.texts_dirty = true;
        self.backdrop_dirty = true;
    }
}

/// `ui/textures/horizon/loading/bgloadingimages/image<n>.png` if installed.
fn backdrop_path(garage: &Garage, n: u32) -> Option<String> {
    let path = format!("ui/textures/horizon/loading/bgloadingimages/image{n}.png");
    garage.assets.join(&path).exists().then_some(path)
}

fn track_title(track: &Track) -> String {
    let name = if track.name.is_empty() { track.id.rsplit('/').next().unwrap_or(&track.id).to_string() } else { track.name.clone() };
    name.to_uppercase()
}

#[allow(clippy::too_many_arguments)]
fn spawn_overlay(mut commands: Commands, font: Res<UiFont>, garage: Res<Garage>, assets: Res<AssetServer>, loading: Res<Loading>) {
    let logo_path = "ui/textures/horizon/loading/masks/ld_gamelogo.png";
    let logo = garage.assets.join(logo_path).exists().then(|| assets.load(logo_path));
    let spin_path = "ui/textures/horizon/loading/largeloadingspinner.png";
    let spinner = garage.assets.join(spin_path).exists().then(|| assets.load::<Image>(spin_path));
    // Every backdrop up front (FH1_LOADING_PRELOAD=0: one at a time, when a cover picks it).
    let backdrops: Vec<Handle<Image>> = if preload_on() { (1..=BACKDROPS).filter_map(|n| backdrop_path(&garage, n)).map(|p| assets.load(p)).collect() } else { Vec::new() };
    info!("loading: preloading {} loading-screen images", logo.iter().count() + spinner.iter().count() + backdrops.len());
    commands.insert_resource(LoadingImages { logo: logo.clone(), spinner: spinner.clone(), backdrops });
    let root = commands
        .spawn((
            LoadingRoot,
            // Above the F3 diagnostics overlay (diag.rs, 1000): nothing about the work behind the cover shows.
            GlobalZIndex(1001),
            if loading.cover.is_some() { Visibility::Inherited } else { Visibility::Hidden },
            Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), overflow: Overflow::clip(), ..default() },
            BackgroundColor(Color::srgb(0.06, 0.065, 0.075)),
            FadeBg(1.0),
        ))
        .id();
    commands.spawn((
        Backdrop,
        ChildOf(root),
        Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() },
        ImageNode { image_mode: NodeImageMode::Stretch, color: Color::NONE, ..default() },
        Fade(1.0),
    ));
    // Loading card: dark band at the bottom with the title, subtitle and tip on the left, spinner on the right,
    // a thin progress line along the bottom edge.
    let band = commands
        .spawn((
            CardPart,
            ChildOf(root),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                bottom: Val::Px(0.0),
                height: Val::Percent(24.0),
                padding: UiRect::new(Val::Percent(6.0), Val::Percent(6.0), Val::Px(0.0), Val::Percent(2.5)),
                align_items: AlignItems::FlexEnd,
                justify_content: JustifyContent::SpaceBetween,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
            FadeBg(0.6),
        ))
        .id();
    let text_col = commands
        .spawn((CardPart, ChildOf(band), Node { flex_direction: FlexDirection::Column, row_gap: Val::Px(6.0), max_width: Val::Percent(70.0), ..default() }))
        .id();
    commands.spawn((CardPart, CardTitle, ChildOf(text_col), Text::new(""), font.text(46.0), TextColor(Color::WHITE), shadow(), Fade(1.0)));
    commands.spawn((CardPart, CardSub, ChildOf(text_col), Text::new(""), font.text(20.0), TextColor(ACCENT), shadow(), Fade(1.0)));
    commands.spawn((
        CardPart,
        CardTip,
        ChildOf(text_col),
        Node { margin: UiRect::top(Val::Px(10.0)), ..default() },
        Text::new(""),
        font.text(17.0),
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.75)),
        shadow(),
        Fade(0.75),
    ));
    let spin_node = Node { width: Val::Px(56.0), height: Val::Px(56.0), margin: UiRect::bottom(Val::Px(6.0)), ..default() };
    match spinner {
        Some(img) => {
            commands.spawn((CardPart, Spinner, ChildOf(band), spin_node, ImageNode { image: img, image_mode: NodeImageMode::Stretch, ..default() }, UiTransform::IDENTITY, Fade(1.0)));
        }
        None => {
            let node = Node { border: UiRect::all(Val::Px(4.0)), ..spin_node };
            commands.spawn((CardPart, Spinner, ChildOf(band), node, BorderColor::all(ACCENT), UiTransform::IDENTITY));
        }
    }
    // Small logo top-left on the card.
    if let Some(img) = logo.clone() {
        commands.spawn((
            CardPart,
            ChildOf(root),
            Node { position_type: PositionType::Absolute, left: Val::Percent(5.0), top: Val::Percent(6.0), width: Val::Percent(20.0), aspect_ratio: Some(4.0), ..default() },
            ImageNode { image: img, image_mode: NodeImageMode::Stretch, ..default() },
            Fade(0.9),
        ));
    }
    let track_bar = commands
        .spawn((
            CardPart,
            ChildOf(root),
            Node { position_type: PositionType::Absolute, left: Val::Px(0.0), right: Val::Px(0.0), bottom: Val::Px(0.0), height: Val::Px(4.0), ..default() },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.12)),
            FadeBg(0.12),
        ))
        .id();
    commands.spawn((CardPart, ProgressFill, ChildOf(track_bar), Node { width: Val::Percent(0.0), height: Val::Percent(100.0), ..default() }, BackgroundColor(ACCENT), FadeBg(1.0)));
    launch::spawn(&mut commands, root, &font, logo);
}

#[allow(clippy::too_many_arguments)]
fn update_loading(
    mut ld: ResMut<Loading>,
    time: Res<Time<Real>>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    pads: Query<&Gamepad>,
    cars: Query<(Entity, &Car)>,
    wheels: Query<(), With<WheelVisual>>,
    scenery: Option<Res<crate::scenery::Scenery>>,
    (rs, events, track, garage, warm): (Res<RaceState>, Res<Events>, Res<Track>, Res<Garage>, Option<Res<fh1_engine::traffic::TrafficWarm>>),
    unspawned_tiles: Query<(), (With<crate::imported::ImportedTile>, Without<Children>)>,
    mut input: ResMut<Input>,
) {
    let now = time.elapsed_secs();
    let dt = time.delta_secs().min(0.1);
    let car = cars.iter().next().map(|(e, c)| (e, c.0.position));
    let focus = car.map(|c| c.1);
    // Car readiness: its model's wheels are tagged (glTF scene spawned), or it has been around for a while.
    if let Some((e, _)) = car {
        if ld.last_car != Some(e) {
            ld.car_since = now;
        }
    }
    let car_ready = car.is_some() && (!wheels.is_empty() || now - ld.car_since > CAR_TIMEOUT_S);
    let world = |p: Vec3| scenery.as_ref().map_or((1.0, true), |s| s.readiness(Vec2::new(p.x, p.z)));

    // Transitions (only while nothing is covering, or a cover is fading out).
    if ld.transitions && (ld.cover.is_none() || ld.fading_out) {
        if let Some((e, pos)) = car {
            let idle = rs.phase == RacePhase::Idle;
            let jumped = ld.last_pos.is_some_and(|l| l.distance(pos) > JUMP_M);
            let new_car = ld.last_car.is_some_and(|l| l != e);
            if ld.last_idle && matches!(rs.phase, RacePhase::Grid { .. }) {
                // Race start: grid teleport, event objects, AI.
                let (title, sub) = rs.race.and_then(|i| events.races.get(i)).map_or_else(
                    || ("RACE".to_string(), String::new()),
                    |d| {
                        let laps = if d.circuit && d.laps > 1 { format!("  ·  {} laps", d.laps) } else { String::new() };
                        (d.name.to_uppercase(), format!("{}{laps}", d.kind.to_uppercase()))
                    },
                );
                ld.start(Cover::Race, now, title, sub);
            } else if new_car && !car_ready {
                // Car select / N-P: the new car's model streams in (it also starts at the current start location).
                let name = garage.cars.get(garage.current).cloned().unwrap_or_default().replace('_', " ");
                ld.start(Cover::Car, now, name.to_uppercase(), "Loading car".into());
            } else if jumped && !(world(pos).1 && car_ready) {
                if !ld.last_idle && idle {
                    ld.start(Cover::Travel, now, "FREE ROAM".into(), "Returning to free roam".into());
                } else {
                    ld.start(Cover::Travel, now, track_title(&track), "Loading the area".into());
                }
            }
            ld.last_idle = idle;
        }
    }
    if let Some((e, pos)) = car {
        ld.last_pos = Some(pos);
        ld.last_car = Some(e);
    }

    let Some(cover) = ld.cover else {
        BLOCK.store(false, Ordering::Relaxed);
        set_covered(false, None);
        let ms = time.delta_secs() * 1000.0;
        if now < ld.after_until && ms > 100.0 {
            info!("loading: hitch {ms:.0} ms {:.1} s after the cover ended", now - (ld.after_until - AFTER_S));
        }
        return;
    };
    if !ld.fading_out {
        let ms = time.delta_secs() * 1000.0;
        let p = &mut ld.pacing;
        p.frames += 1;
        p.sum_ms += ms;
        p.worst_ms = p.worst_ms.max(ms);
        p.over_50 += (ms > 50.0) as u32;
        p.over_100 += (ms > 100.0) as u32;
        if ms > 250.0 {
            info!("loading: {cover:?}: long frame {ms:.0} ms (ended at {now:.2} s)");
        }
    }
    let (frac, world_ready) = focus.map_or((0.0, false), world);
    // Startup also waits for the traffic pool (pre-built so free roam never builds a body on the fly).
    let traffic = warm.as_deref().copied().filter(|w| matches!(cover, Cover::Startup | Cover::Launch) && w.pending());
    let ready = world_ready && car_ready && traffic.is_none();
    if !ld.fading_out {
        if ready {
            if ld.ready_since.is_none() {
                info!("loading: {cover:?}: world ready after {:.1} s ({:.1} s since start)", now - ld.since, now);
            }
            ld.ready_since.get_or_insert(now);
        } else {
            ld.ready_since = None;
        }
        // Calm frames: none of the upload / compile / spawn hitches the world still has in it.
        let ms = time.delta_secs() * 1000.0;
        let median = {
            let mut v: Vec<f32> = ld.recent_ms.iter().copied().collect();
            v.sort_by(f32::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(ms)
        };
        let calm = ms <= CALM_MS.max(median * 1.6).min(45.0);
        ld.calm_frames = if calm { ld.calm_frames + 1 } else { 0 };
        if ld.recent_ms.len() >= 30 {
            ld.recent_ms.pop_front();
        }
        ld.recent_ms.push_back(ms);
        let pipelines = PIPELINES_PENDING.load(Ordering::Relaxed);
        let tiles = unspawned_tiles.iter().count();
        let quiet = !settle_on() || (ld.calm_frames >= SETTLE_FRAMES && pipelines == 0 && tiles == 0);
        let capped = settle_on() && ld.ready_since.is_some_and(|t| now - t >= SETTLE_CAP_S);
        if capped && !quiet {
            warn!("loading: {cover:?}: not settled {SETTLE_CAP_S} s after world-ready (calm frames {}, pipelines {pipelines}, tiles {tiles}); fading anyway", ld.calm_frames);
        }
        let settled = ld.ready_since.is_some_and(|t| now - t >= SETTLE_S) && (quiet || capped);
        if settled && settle_on() && cover != Cover::Launch {
            info!("loading: {cover:?}: settled (calm frames {}, median {median:.1} ms, pipelines {pipelines}, unspawned tiles {tiles})", ld.calm_frames);
        }
        let on_screen = now - ld.since;
        match cover {
            Cover::Launch => {
                // The main menu (launch.rs `main_menu`) ends this cover: `start_world` / `continue_loaded`.
                let _ = (&keys, &mouse, &pads);
            }
            _ => {
                let (min, timeout) = match cover {
                    // FH1_LOADING_MIN=<s>: keep the startup card at least that long (screenshots of the card).
                    Cover::Startup => (std::env::var("FH1_LOADING_MIN").ok().and_then(|v| v.parse().ok()).unwrap_or(STARTUP_MIN_S), STARTUP_TIMEOUT_S),
                    Cover::Race => (RACE_MIN_S, TRANSITION_TIMEOUT_S),
                    _ => (TRAVEL_MIN_S, TRANSITION_TIMEOUT_S),
                };
                // On screen too long. (This also counted real time since boot for Startup, which ended the card at
                // once when the main menu had been up for 45 s: the world showed while it loaded.)
                let timed_out = on_screen >= timeout;
                if timed_out {
                    warn!("loading: {cover:?} cover timed out after {on_screen:.1} s (world ready {world_ready}, car ready {car_ready})");
                }
                if (settled && on_screen >= min) || timed_out {
                    info!("loading: {cover:?} done after {on_screen:.1} s");
                    let p = std::mem::take(&mut ld.pacing);
                    info!(
                        "loading: {cover:?} frames: {} in {on_screen:.1} s, mean {:.1} ms, worst {:.0} ms, {} over 50 ms, {} over 100 ms",
                        p.frames,
                        p.sum_ms / p.frames.max(1) as f32,
                        p.worst_ms,
                        p.over_50,
                        p.over_100
                    );
                    ld.fading_out = true;
                    ld.after_until = now + AFTER_S;
                }
            }
        }
        if cover == Cover::Startup && ld.title.is_empty() {
            let title = track_title(&track);
            ld.set_texts(title, "Loading the world".into());
        }
        if let Some(w) = traffic.filter(|_| world_ready && car_ready && detail_on()) {
            let sub = format!("Preparing traffic {}/{}", w.done, w.total);
            if ld.sub != sub {
                ld.sub = sub;
                ld.texts_dirty = true;
            }
        }
        let warm_frac = warm.as_deref().filter(|w| cover == Cover::Startup && w.total > 0).map(|w| w.done as f32 / w.total as f32);
        let target = match warm_frac {
            Some(t) => frac * 0.6 + if car_ready { 0.1 } else { 0.0 } + 0.3 * t,
            None => frac * 0.85 + if car_ready { 0.15 } else { 0.0 },
        }
        .clamp(0.0, 1.0);
        let p = ld.progress;
        ld.progress = (p + (target - p) * (dt * 6.0).min(1.0)).max(p);
    } else {
        ld.progress = 1.0;
        ld.alpha -= dt / FADE_S;
        if ld.alpha <= 0.0 {
            ld.alpha = 0.0;
            ld.cover = None;
            ld.fading_out = false;
        }
    }
    let block = ld.cover.is_some() && (!ld.fading_out || ld.from_launch);
    if block && !BLOCK.load(Ordering::Relaxed) {
        // Drop held throttle / steering from before the cover (like the pause menu does).
        let (tcs, abs) = (input.0.tcs, input.0.abs);
        input.0 = crate::vehicle::Controls { tcs, abs, ..default() };
    }
    BLOCK.store(block, Ordering::Relaxed);
    set_covered(ld.cover.is_some() && !ld.fading_out, ld.cover);
}

/// Updates the world-audio gate, logging each change.
fn set_covered(covered: bool, cover: Option<Cover>) {
    if COVERED.swap(covered, Ordering::Relaxed) != covered && audio_gate_on() {
        info!("loading: world audio {} ({cover:?})", if covered { "gated off" } else { "allowed" });
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn draw_overlay(
    mut ld: ResMut<Loading>,
    time: Res<Time<Real>>,
    (garage, assets, cache): (Res<Garage>, Res<AssetServer>, Res<LoadingImages>),
    windows: Query<&Window, With<PrimaryWindow>>,
    mut root: Query<&mut Visibility, (With<LoadingRoot>, Without<CardPart>, Without<LaunchPart>)>,
    mut parts: Query<(&mut Visibility, Has<CardPart>), (Or<(With<CardPart>, With<LaunchPart>)>, Without<LoadingRoot>)>,
    mut backdrop: Query<(&Fade, &mut ImageNode, &mut Node), (With<Backdrop>, Without<ProgressFill>)>,
    mut fill: Query<&mut Node, (With<ProgressFill>, Without<Backdrop>)>,
    mut spinner: Query<&mut UiTransform, With<Spinner>>,
    mut texts: Query<(&mut Text, Has<CardTitle>, Has<CardSub>, Has<CardTip>)>,
    mut bgs: Query<(&FadeBg, &mut BackgroundColor, Has<LoadingRoot>)>,
    mut shadows: Query<(&Fade, &mut TextShadow)>,
    mut imgs: Query<(&Fade, &mut ImageNode), Without<Backdrop>>,
    mut colors: Query<(&Fade, &mut TextColor, Has<PressPrompt>)>,
    mut shown: Local<Option<Option<Cover>>>,
) {
    let cover = ld.cover;
    let now = time.elapsed_secs();
    let root_vis = if cover.is_some() { Visibility::Inherited } else { Visibility::Hidden };
    for mut v in &mut root {
        v.set_if_neq(root_vis);
    }
    // First cover: hold its contents back until the preloaded images are decoded, then fade them in.
    if !ld.images_ready {
        let settled = |h: &Handle<Image>| assets.is_loaded_with_dependencies(h.id()) || matches!(assets.get_load_state(h.id()), Some(bevy::asset::LoadState::Failed(_)));
        let t = START.get().map_or(0.0, |t| t.elapsed().as_secs_f32());
        if cache.all().all(settled) || t > REVEAL_WAIT_S {
            info!("loading: {} loading-screen images ready at {t:.2} s", cache.all().filter(|h| assets.is_loaded_with_dependencies(h.id())).count());
            ld.images_ready = true;
            // The first cover's backdrop is picked again from the decoded set.
            ld.backdrop_dirty = true;
        }
    }
    if ld.images_ready && ld.reveal < 1.0 {
        ld.reveal = (ld.reveal + time.delta_secs() / REVEAL_FADE_S).min(1.0);
    }
    let Some(cover) = cover else {
        *shown = Some(None);
        return;
    };
    if *shown != Some(Some(cover)) {
        *shown = Some(Some(cover));
        let launch = cover == Cover::Launch;
        for (mut v, card) in &mut parts {
            v.set_if_neq(if card != launch { Visibility::Inherited } else { Visibility::Hidden });
        }
    }
    // Backdrop: a random FH1 loading image per cover, cover-fitted to the window (16:9 source).
    if ld.backdrop_dirty {
        ld.backdrop_dirty = false;
        let r = ld.next_rand();
        ld.backdrop = if cache.backdrops.is_empty() {
            backdrop_path(&garage, r % BACKDROPS + 1).map(|p| assets.load(p))
        } else {
            // Preloaded: a decoded one (the same handle every time, never re-read).
            let loaded: Vec<&Handle<Image>> = cache.backdrops.iter().filter(|h| assets.is_loaded_with_dependencies(h.id())).collect();
            let pool = if loaded.is_empty() { cache.backdrops.iter().collect() } else { loaded };
            Some(pool[r as usize % pool.len()].clone())
        };
    }
    if let Ok(w) = windows.single() {
        let (ww, wh) = (w.width().max(1.0), w.height().max(1.0));
        let s = (ww / 1280.0).max(wh / 720.0);
        let (pw, ph) = (1280.0 * s / ww * 100.0, 720.0 * s / wh * 100.0);
        for (_, mut img, mut node) in &mut backdrop {
            if let Some(h) = ld.backdrop.as_ref() {
                if img.image != *h {
                    img.image = h.clone();
                }
            }
            node.width = Val::Percent(pw);
            node.height = Val::Percent(ph);
            node.left = Val::Percent((100.0 - pw) * 0.5);
            node.top = Val::Percent((100.0 - ph) * 0.5);
        }
    }
    if ld.texts_dirty {
        ld.texts_dirty = false;
        for (mut t, title, sub, tip) in &mut texts {
            if title {
                t.0 = ld.title.clone();
            } else if sub {
                t.0 = ld.sub.clone();
            } else if tip {
                t.0 = ld.tip.clone();
            }
        }
    }
    for mut n in &mut fill {
        n.width = Val::Percent(ld.progress * 100.0);
    }
    for mut t in &mut spinner {
        t.rotation = Rot2::radians(now * 5.0);
    }
    // Fade: every element's base alpha times the overlay's.
    let a = ld.alpha;
    for (f, mut c, is_root) in &mut bgs {
        c.0.set_alpha(f.0 * if is_root { a } else { a * ld.reveal });
    }
    // Contents (not the opaque root) wait for the first reveal.
    let a = a * ld.reveal;
    for (f, mut i) in &mut imgs {
        i.color = Color::WHITE.with_alpha(f.0 * a);
    }
    let has_img = ld.backdrop.as_ref().is_some_and(|h| assets.is_loaded_with_dependencies(h.id()));
    for (f, mut i, _) in &mut backdrop {
        i.color = Color::WHITE.with_alpha(if has_img { f.0 * a } else { 0.0 });
    }
    for (f, mut sh) in &mut shadows {
        sh.color.set_alpha(SHADOW_A * f.0.min(1.0) * a);
    }
    let pulse = 0.55 + 0.45 * (now * 3.0).sin().abs();
    for (f, mut c, prompt) in &mut colors {
        let k = if prompt && !ld.fading_out { pulse } else { 1.0 };
        c.0.set_alpha(f.0 * a * k);
    }
}
