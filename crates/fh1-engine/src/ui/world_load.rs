//! L1b: choosing the world before loading it. One place that turns a map id into the world resources (`load_world`),
//! used by main.rs at startup (`--track`, automation, no main menu) and by the main menu (launch.rs) after the player
//! picks a mode / map / car. With the main menu first, main.rs starts on a placeholder (`Track::flat`, no scenery,
//! no car) with [`PendingWorld`] inserted; `track::setup` and `spawn_car` wait (`world_ready`), and [`apply_choice`]
//! loads the chosen map in-process, behind the loading screen, then runs them.

use std::path::Path;

use bevy::ecs::system::RunSystemOnce;
use bevy::prelude::*;

use crate::track::{self, Track};
use crate::{imported, race, scenery, Garage, SpawnIndex};

/// The world resources of one map.
pub struct LoadedWorld {
    pub track: Track,
    pub events: race::Events,
    pub scenery: Option<scenery::Scenery>,
    pub imported: Option<imported::ImportedScenery>,
}

impl LoadedWorld {
    /// Main menu first: nothing loaded yet.
    pub fn placeholder() -> Self {
        Self { track: Track::flat(), events: race::Events::default(), scenery: None, imported: None }
    }
}

/// Free roam (Horizon) or a circuit (Motorsport).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Horizon,
    Motorsport,
}

/// What the main menu picked.
#[derive(Clone, Debug)]
pub struct WorldChoice {
    pub track: String,
    /// Index into `Garage::cars` (None = keep the current one).
    pub car: Option<usize>,
    pub mode: Mode,
    /// Motorsport start: spawns.json `kind` (`grid` = pole, `pit`, `flying`); None = the track's home spawn.
    pub start: Option<String>,
}

/// Present while the world isn't loaded (main menu first); `choice` is set by the menu and consumed by `apply_choice`.
#[derive(Resource, Default)]
pub struct PendingWorld {
    pub choice: Option<WorldChoice>,
}

/// The mode the loaded world was started in.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct GameMode(pub Mode);

/// Run condition: the world is loaded (no main menu pending).
pub fn world_ready(p: Option<Res<PendingWorld>>) -> bool {
    p.is_none()
}

/// Whether this run starts on the main menu (and loads the world after the choice): the launch screen is on
/// (no `--track`, no automation, FH1_LAUNCH_SCREEN not 0).
pub fn menu_first() -> bool {
    super::launch::enabled()
}

/// The map to load without the menu: `--track`, else settings.json `map` (not for agent runs), else Colorado
/// (the logic main.rs had inline).
pub fn track_name(data_dir: &Path, given: Option<String>) -> String {
    let agent_run = std::env::var_os("CLAUDECODE").is_some();
    let name = given
        .or_else(|| (track::IMPORTED_MAPS && !agent_run).then(|| saved_map(data_dir)).flatten())
        .unwrap_or_else(|| "colorado".into());
    if track::IMPORTED_MAPS || name == "flat" {
        name
    } else {
        "colorado".into()
    }
}

/// settings.json `map`.
pub fn saved_map(data_dir: &Path) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(&std::fs::read(data_dir.join("settings.json")).ok()?).ok()?["map"].as_str().map(str::to_owned)
}

/// Loads map `name`: Colorado, `flat`, a native imported map (FH2's `imported/fh2/anthem`: scenery through the game
/// shaders), or a glTF-tiled import (FM4). Falls back to the test plane.
pub fn load_world(assets: &Path, name: &str) -> LoadedWorld {
    let track = if name == "flat" {
        Track::flat()
    } else if name != "colorado" {
        // An imported map (docs/RENDERING.md).
        match Track::imported(assets, name, &track::imported_name_at(assets, name)) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Map {name} not available ({e:#}); using the test plane. Convert it first (docs/RENDERING.md).");
                Track::flat()
            }
        }
    } else {
        match Track::colorado(assets) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Colorado not available ({e:#}); using the test plane. Run fh1setup to convert the world.");
                Track::flat()
            }
        }
    };
    // Colorado's scenery (and with it grass, crowds, props, smashables) only on Colorado; a native import (FH2) has its
    // own scenery group; other imported maps stream their own glTF tiles.
    let colorado = track.id == "colorado";
    let events = if colorado { race::Events::load(assets) } else { race::Events::default() };
    let native = (!colorado && track.world.is_some()).then(|| track::native_track_dir(assets, &track.id)).flatten();
    let scenery = if colorado && track.world.is_some() {
        scenery::Scenery::load(assets)
    } else if let Some(dir) = native.as_ref() {
        let own = dir.join("shaders/track");
        let shaders = if own.exists() { own } else { assets.join("shaders/track") };
        scenery::Scenery::load_track(dir.join("scenery"), &shaders, false)
    } else {
        None
    };
    let imported = (!colorado && scenery.is_none() && track.world.is_some()).then(|| imported::ImportedScenery::load(assets, &track.id)).flatten();
    if track.world.is_some() && scenery.is_none() && imported.is_none() {
        eprintln!("No converted scenery; showing the collision mesh. Run fh1setup to convert it.");
    }
    LoadedWorld { track, events, scenery, imported }
}

/// Start location: FH1_SPAWN=<n>, else the track's home.
pub fn spawn_index(track: &Track) -> usize {
    std::env::var("FH1_SPAWN").ok().and_then(|v| v.parse().ok()).unwrap_or(track.home)
}

/// Everything `apply_choice` reads from disk for a choice (built on a worker thread, see [`WorldLoadTask`]).
struct PreparedWorld {
    w: LoadedWorld,
    spawn: usize,
    flying_mph: f32,
    laptimer: Option<super::laptimer::LapTimer>,
    tod: Option<fh1_render::lighting::FxTimeOfDay>,
    post: fh1_render::postfx::FxPostConfig,
    ms: f32,
}

fn prepare(assets: &Path, choice: &WorldChoice) -> PreparedWorld {
    let t = std::time::Instant::now();
    let w = load_world(assets, &choice.track);
    let (spawn, flying_mph) = choice.start.as_deref().and_then(|k| start_spawn(assets, &w.track.id, k)).unwrap_or((spawn_index(&w.track), 0.0));
    let laptimer = super::laptimer::LapTimer::load(assets, &w.track.id);
    // The map's time-of-day curves and post chain inputs (track::time_of_day_path / post_config: Colorado's unless a
    // native import has its own, FH2's Anthem).
    let tod = fh1_render::lighting::FxTimeOfDay::load(&track::time_of_day_path(assets, &choice.track), 960.0);
    let post = track::post_config(assets, &choice.track);
    PreparedWorld { w, spawn, flying_mph, laptimer, tod, post, ms: t.elapsed().as_secs_f32() * 1000.0 }
}

/// The chosen world being read on a worker thread while the loading card keeps animating (`FH1_ASYNC_WORLD_LOAD=0`:
/// read it inside the frame as before, which froze the card for ~0.65 s on Colorado).
#[derive(Resource)]
pub struct WorldLoadTask {
    rx: std::sync::Mutex<std::sync::mpsc::Receiver<PreparedWorld>>,
    choice: WorldChoice,
}

fn async_world_load() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_ASYNC_WORLD_LOAD").as_deref(), Ok("0") | Ok("off")))
}

/// Loads the chosen world (exclusive; runs every frame, acts after the main menu's choice). The disk part runs on a
/// worker thread ([`WorldLoadTask`]); the resources are inserted and the world / car spawned once it is done.
pub fn apply_choice(world: &mut World) {
    if world.contains_resource::<WorldLoadTask>() {
        use std::sync::mpsc::TryRecvError;
        let got = world.resource::<WorldLoadTask>().rx.lock().map(|rx| rx.try_recv()).unwrap_or(Err(TryRecvError::Disconnected));
        match got {
            Ok(p) => {
                let task = world.remove_resource::<WorldLoadTask>().expect("checked above");
                finish_choice(world, task.choice, p);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // The loader panicked: load in the frame instead (the card still covers it).
                let task = world.remove_resource::<WorldLoadTask>().expect("checked above");
                warn!("world: loader thread died; loading {} in the frame", task.choice.track);
                let assets = world.resource::<Garage>().assets.clone();
                let p = prepare(&assets, &task.choice);
                finish_choice(world, task.choice, p);
            }
        }
        return;
    }
    let Some(choice) = world.get_resource_mut::<PendingWorld>().and_then(|mut p| p.choice.take()) else { return };
    let assets = world.resource::<Garage>().assets.clone();
    if async_world_load() {
        let (tx, rx) = std::sync::mpsc::channel();
        let c = choice.clone();
        let spawned = std::thread::Builder::new().name("fh1-world-load".into()).spawn(move || {
            let _ = tx.send(prepare(&assets, &c));
        });
        match spawned {
            Ok(_) => {
                info!("world: loading {} on a worker thread", choice.track);
                world.insert_resource(WorldLoadTask { rx: std::sync::Mutex::new(rx), choice });
                return;
            }
            Err(e) => warn!("world: no loader thread ({e}); loading in the frame"),
        }
    }
    let assets = world.resource::<Garage>().assets.clone();
    let p = prepare(&assets, &choice);
    finish_choice(world, choice, p);
}

/// The main-thread half of a world load: resources in, track + car spawned.
fn finish_choice(world: &mut World, choice: WorldChoice, p: PreparedWorld) {
    let t = std::time::Instant::now();
    let PreparedWorld { w, spawn, flying_mph, laptimer, tod, post, ms } = p;
    info!("world: loaded {} ({:?}) in {ms:.0} ms", w.track.id, choice.mode);
    world.remove_resource::<PendingWorld>();
    if let Some(i) = choice.car {
        let mut g = world.resource_mut::<Garage>();
        g.current = i.min(g.cars.len().saturating_sub(1));
    }
    world.insert_resource(SpawnIndex(spawn));
    if let Some(lt) = laptimer {
        world.insert_resource(lt);
    }
    world.insert_resource(w.track);
    world.insert_resource(w.events);
    if let Some(sc) = w.scenery {
        world.insert_resource(sc);
    }
    if let Some(sc) = w.imported {
        world.insert_resource(sc);
    }
    world.insert_resource(GameMode(choice.mode));
    // A changed FxPostConfig makes fh1-render rebuild the post chain and reload glows, the static car cube and fog
    // templates in-process (postfx::reload_post, FxTrackChanged).
    if let Some(mut tod) = tod {
        tod.rate_scale = 1.0;
        world.insert_resource(tod);
    }
    world.insert_resource(post);
    // Systems that build per-world state from `Track` (HUD, minimap, traffic pool) did so for the placeholder while
    // this loaded: a new generation makes them build again for this world. (Before, the menu-first boot never
    // pre-built the traffic pool: it planned for the placeholder test plane.)
    world.resource_mut::<WorldGeneration>().0 += 1;
    if let Err(e) = world.run_system_once(track::setup) {
        warn!("world: track setup: {e}");
    }
    if let Err(e) = world.run_system_once(crate::spawn_car) {
        warn!("world: car spawn: {e}");
    }
    {
        let t = world.resource::<Track>();
        let p = t.spawns.get(spawn % t.spawns.len().max(1)).map(|s| s.0);
        let mut q = world.query::<&crate::Car>();
        let cars: Vec<Vec3> = q.iter(world).map(|c| c.0.position).collect();
        info!("world: spawn {spawn} at {p:?}; cars {cars:?}");
    }
    // Flying start: already at speed along the car's heading (front = -Z).
    if flying_mph > 0.0 {
        let mut q = world.query::<&mut crate::Car>();
        for mut car in q.iter_mut(world) {
            let fwd = car.0.rotation * Vec3::NEG_Z;
            car.0.velocity = fwd * (flying_mph * 0.44704);
        }
    }
    info!("world: spawned in {:.0} ms", t.elapsed().as_secs_f32() * 1000.0);
}

/// X1c: a root entity that belongs to the loaded world (track meshes, scenery / prop roots, debris, grass, crowds,
/// animated objects, race objects). [`switch_world`] despawns them (with their children) to unload the map.
#[derive(Component, Default, Clone, Copy)]
pub struct WorldEntity;

/// Bumped on every in-process map change. Systems that cache world state in `Local`s (grass, crowds, animated objects,
/// smash colliders, traffic) drop their cache when it differs from the one they built for.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct WorldGeneration(pub u32);

/// Pause menu Map page: switch to this map in-process (handled by [`switch_world`]).
#[derive(Resource, Default)]
pub struct MapSwitch(pub Option<String>);

/// In-process map change (pause menu, X1c): unload the current world, then let [`apply_choice`] load the new one
/// behind the loading card. Despawns every world root ([`WorldEntity`], the player car, AI and traffic cars, imported
/// tiles), drops the world resources, bumps [`WorldGeneration`] and makes fh1-render reload its track data even if the
/// new map shares the old one's post inputs.
pub fn switch_world(world: &mut World) {
    let Some(track) = world.get_resource_mut::<MapSwitch>().and_then(|mut s| s.0.take()) else { return };
    let t = std::time::Instant::now();
    let before = world.entities().count_spawned();
    let mut roots: Vec<Entity> = Vec::new();
    roots.extend(world.query_filtered::<Entity, With<WorldEntity>>().iter(world));
    roots.extend(world.query_filtered::<Entity, With<crate::Car>>().iter(world));
    roots.extend(world.query_filtered::<Entity, With<fh1_engine::ai::AiCar>>().iter(world));
    roots.extend(world.query_filtered::<Entity, With<crate::traffic_plugin::TrafficSim>>().iter(world));
    roots.extend(world.query_filtered::<Entity, With<imported::ImportedTile>>().iter(world));
    let mut despawned = 0;
    for e in roots {
        // A marked child goes with its marked parent.
        if world.get_entity(e).is_ok() && world.despawn(e) {
            despawned += 1;
        }
    }
    world.remove_resource::<scenery::Scenery>();
    world.remove_resource::<imported::ImportedScenery>();
    world.remove_resource::<super::laptimer::LapTimer>();
    world.remove_resource::<track::CollisionViewLazy>();
    // The old map's Track stays out of the frames the new one loads in (worker thread): systems that build from it on a
    // new generation (traffic pool, HUD, minimap) see the empty test plane until `finish_choice` bumps it again.
    world.insert_resource(Track::flat());
    world.insert_resource(race::RaceState::default());
    world.insert_resource(crate::smash::PropCollision::default());
    world.insert_resource(crate::traffic_plugin::TrafficState::default());
    world.insert_resource(crate::anim::AnimStore::default());
    // The rewind history is the old map's (same car = not cleared by rewind_tick).
    world.insert_resource(super::assists::Rewind::default());
    // The fell-off-the-world rescue point is the old map's: on the new one it would teleport the car back there.
    world.insert_resource(crate::SafePose::default());
    // The old map's objective line; the new one picks its own default (Colorado: Race Central).
    if let Some(mut o) = world.get_resource_mut::<super::notify::Objective>() {
        (o.text, o.target) = (None, None);
    }
    if let Some(mut n) = world.get_resource_mut::<super::notify::NotifyState>() {
        n.defaulted = false;
        n.shown = None;
    }
    // The satnav routes on Colorado's road network only; its target comes back per world (minimap.rs route).
    if let Some(mut nav) = world.get_resource_mut::<super::minimap::SatNav>() {
        (nav.target, nav.distance_m) = (None, None);
        nav.path.clear();
    }
    world.resource_mut::<WorldGeneration>().0 += 1;
    fh1_render::postfx::invalidate_track(world);
    let now = world.resource::<Time<Real>>().elapsed_secs();
    let assets = world.resource::<Garage>().assets.clone();
    let name = if track == "colorado" { "Colorado".to_string() } else { track::imported_name_at(&assets, &track) };
    if let Some(mut ld) = world.get_resource_mut::<super::loading::Loading>() {
        ld.start_world(now, name.to_uppercase(), if motorsport(&track) { "Loading the circuit" } else { "Loading the world" }.into());
    }
    let after = world.entities().count_spawned();
    info!("world: unloaded for {track}: {despawned} roots, entities {before} -> {after} in {:.0} ms", t.elapsed().as_secs_f32() * 1000.0);
    let mode = if motorsport(&track) { Mode::Motorsport } else { Mode::Horizon };
    world.insert_resource(PendingWorld { choice: Some(WorldChoice { track, car: None, mode, start: None }) });
}

/// Circuit maps (FM4 imports) start in Motorsport mode.
fn motorsport(track: &str) -> bool {
    track.starts_with("fm4/")
}

/// Dev hook (X1c verify): `FH1_MAP_TOUR=fh2/anthem,fm4/alps_00,colorado` switches to each map in turn through the
/// pause menu's path ([`MapSwitch`]) every `FH1_MAP_TOUR_S` (25) seconds, logging the entity count before each switch.
pub fn map_tour(time: Res<Time<Real>>, mut switch: ResMut<MapSwitch>, pending: Option<Res<PendingWorld>>, mut next: Local<(usize, f32)>, world_entities: Query<(), With<WorldEntity>>, all: Query<()>, cars: Query<&crate::Car>) {
    let Ok(list) = std::env::var("FH1_MAP_TOUR") else { return };
    let every = std::env::var("FH1_MAP_TOUR_S").ok().and_then(|v| v.parse().ok()).unwrap_or(25.0);
    let maps: Vec<&str> = list.split(',').map(str::trim).filter(|m| !m.is_empty()).collect();
    if pending.is_some() || next.0 >= maps.len() {
        return;
    }
    if next.1 == 0.0 {
        next.1 = time.elapsed_secs() + every;
    }
    if time.elapsed_secs() < next.1 {
        return;
    }
    let car: Vec<Vec3> = cars.iter().map(|c| c.0.position).collect();
    info!("world: map tour: entities {} ({} WorldEntity), cars at {car:?} -> switching to {}", all.iter().count(), world_entities.iter().count(), maps[next.0]);
    switch.0 = Some(maps[next.0].to_owned());
    next.0 += 1;
    next.1 = time.elapsed_secs() + every;
}

/// Dev hook: `FH1_WORLD_AUTOPICK=<map id>` with the main menu first (`FH1_LAUNCH_SCREEN=1`) picks that map (Horizon,
/// current car) 2 s after start, as the menu would: tests the in-process load path without input.
pub fn autopick(pending: Option<ResMut<PendingWorld>>, time: Res<Time<Real>>, loading: Option<ResMut<super::loading::Loading>>, mut done: Local<bool>) {
    let Some(mut p) = pending else { return };
    if *done || time.elapsed_secs() < 2.0 {
        return;
    }
    let Ok(track) = std::env::var("FH1_WORLD_AUTOPICK") else { return };
    *done = true;
    info!("world: FH1_WORLD_AUTOPICK {track}");
    // The hand-over the main menu does: the loading card replaces the launch screen.
    if let Some(mut ld) = loading {
        ld.start_world(time.elapsed_secs(), track.to_uppercase(), "Loading the world".into());
    }
    p.choice = Some(WorldChoice { track, car: None, mode: Mode::Horizon, start: None });
}

/// Index of the first spawn of `kind` in `imported/<id>/world/spawns.json` (counting only spawns `Track::open_world`
/// keeps, i.e. with `ground_y`), and its `speed_mph` (flying starts).
fn start_spawn(assets: &Path, id: &str, kind: &str) -> Option<(usize, f32)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(assets.join("imported").join(id).join("world/spawns.json")).ok()?).ok()?;
    let kept: Vec<&serde_json::Value> = v["spawns"].as_array()?.iter().filter(|s| s["ground_y"].as_f64().is_some()).collect();
    let i = kept.iter().position(|s| s["kind"].as_str() == Some(kind))?;
    Some((i, kept[i]["speed_mph"].as_f64().unwrap_or(0.0) as f32))
}
