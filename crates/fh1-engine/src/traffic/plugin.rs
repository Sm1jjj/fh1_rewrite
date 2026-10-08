//! Free-roam traffic in the game (docs/TRAFFIC.md): spawns road traffic and festival drivers around the player on
//! Colorado's lanes with AIOpenWorld.xml's densities and car mix, drives them (lib `traffic::driver`), and despawns
//! them beyond the spawn-out distance.
//!
//! Two modes per car: within NEAR of the player (or after a knock) the car runs the full `Vehicle` sim and touches the
//! player / other cars (vehicle/contact.rs); further out it slides along its lane (far mode: no physics, one ground ray
//! every few ticks), so dozens of cars cost little. Traffic cars carry `AiCar` + `TrafficCar` (no `AiBrain` / `AiRacer`).
//!
//! Flags: FH1_TRAFFIC=0 off, FH1_TRAFFIC_DENSITY=<x> density scale, FH1_TRAFFIC_MAX=<n> road cars cap (default 28),
//! FH1_TRAFFIC_FESTIVAL=0 no festival drivers, FH1_TRAFFIC_RACE=1 keep traffic in races (the game's `race` settings),
//! FH1_TRAFFIC_NEAR=<m> full-sim radius (default 110), FH1_TRAFFIC_DRAW=<m> draw distance (default 350),
//! FH1_TRAFFIC_FRESH=<s> seconds between new body builds (default 4), FH1_TRAFFIC_DEBUG=1 logs.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::gltf::GltfAssetLabel;
use bevy::prelude::*;
use bevy::world_serialization::{WorldAsset, WorldAssetRoot};
use fh1_engine::ai::driver::Obstacle;
use fh1_engine::ai::{AiCar, AiWheel};
use fh1_engine::data::CarData;
use fh1_engine::traffic::config::{self, Density, SettingsSet};
use fh1_engine::traffic::driver::{cruise_rpm, mph, TrafficDriver};
use fh1_engine::traffic::network::{Network, RoadType};
use fh1_engine::traffic::{density_scale, enabled, TrafficCar, TrafficData, TrafficKind, TrafficParked, TrafficWarm};
use fh1_engine::vehicle::{contact, Controls, Vehicle};
use fh1_engine::world::MIRROR_Z;

use crate::track::Track;
use crate::{Car, Garage};

const SUBSTEPS: usize = 4;
/// Seconds between spawn decisions.
const SPAWN_PERIOD: f32 = 0.25;
/// Seconds between spawns that build a new body (no parked car of the model to reuse): each costs a frame hitch
/// (fh1-render builds the game-shaded body synchronously), so the pool fills slowly (FH1_TRAFFIC_FRESH).
fn fresh_period() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| env_f32("FH1_TRAFFIC_FRESH", 4.0))
}
/// Parked cars kept for reuse (the pre-built pool is ~40).
const POOL_MAX: usize = 48;
/// Pre-build: bodies started per frame behind the loading card, and the longest the card waits for them (s).
/// Pool bodies spawned per frame while pre-building. A faithful body is built in the frame it spawns (fh1-render
/// spawn_fx_car_bodies, synchronous): 2 per frame gave 250-930 ms frames on the loading card, so 1, and none in the
/// frame after a slow one (`WARM_SLOW_MS`) so the cover's spinner gets a normal frame between builds.
/// FH1_TRAFFIC_WARM_FAST=1 = the old 2 per frame, no pause.
const WARM_PER_FRAME: usize = 1;
const WARM_SLOW_MS: f32 = 60.0;
const WARM_TIMEOUT_S: f32 = 60.0;
/// Pre-built bodies per road model (maxactive 1 models: 1) and per festival model.
const WARM_PER_MODEL: u32 = 2;
/// Distinct festival driver models loaded at once (OUR cap, like maxLoadedTrafficModels for road traffic).
const FESTIVAL_MODELS: usize = 4;
/// Where parked cars wait (far below the map: no light, sound or contact reaches the player).
const PARK_Y: f32 = -10000.0;

fn env_f32(k: &str, d: f32) -> f32 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn flag(k: &str, default: bool) -> bool {
    std::env::var(k).map_or(default, |v| v != "0")
}

/// FH1_TRAFFIC_DRAW: traffic cars further than this are not drawn (m).
fn draw_dist() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    // 350 since 2026-10-07 (was 150; user: "load them in from further away"); the game spawns them at 380 m.
    *V.get_or_init(|| env_f32("FH1_TRAFFIC_DRAW", 350.0))
}

fn near_radius() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| env_f32("FH1_TRAFFIC_NEAR", 110.0))
}

fn debug() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("FH1_TRAFFIC_DEBUG").is_some())
}

pub struct TrafficPlugin;

impl Plugin for TrafficPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TrafficState>()
            .init_resource::<TrafficWarm>()
            .add_systems(Update, warm_pool)
            .add_observer(mark_scene_ready)
            .init_resource::<PendingDespawn>()
            .add_systems(Last, apply_despawns)
            .add_systems(Update, traffic_spawn.run_if(crate::ui::driving))
            .add_systems(Update, sync_traffic_visuals.after(crate::sync_visuals))
            .add_systems(FixedUpdate, step_traffic.after(crate::step_physics).before(crate::physics_settled).run_if(crate::ui::driving));
    }
}

/// A traffic car's model, loaded once and kept while cars use it.
struct Model {
    data: CarData,
    scene: Handle<WorldAsset>,
    paints: Vec<u32>,
    last_used: f32,
}

#[derive(Resource, Default)]
pub struct TrafficState {
    data: Option<Arc<TrafficData>>,
    tried: bool,
    /// Background load of the traffic data (lane fit raycasts ~0.2 s: off the frame), with its time.
    loading: Option<std::sync::Mutex<std::sync::mpsc::Receiver<(Result<TrafficData, String>, std::time::Duration)>>>,
    models: HashMap<String, Model>,
    /// Junction reservations: node -> (car, road rank, expiry clock).
    reservations: HashMap<u32, (Entity, RoadType, f32)>,
    clock: f32,
    next_spawn: f32,
    next_fresh: f32,
    last_player: Option<Vec3>,
    /// The initial population (numInitialTrafficCars / numInitialFestivalCars) is placed.
    initial_done: bool,
    rng: u32,
    /// Pre-build plan (model, kind) and how far it got.
    warm_plan: Option<Vec<(String, TrafficKind)>>,
    warm_next: usize,
    /// Debug: CPU time of step_traffic (us, summed since the last log) and ticks.
    step_us: f32,
    step_ticks: u32,
}

impl TrafficState {
    fn rand(&mut self) -> f32 {
        if self.rng == 0 {
            self.rng = 0x9E37_79B9;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / (1u32 << 24) as f32
    }

    fn data(&mut self, garage: &Garage, track: &Track) -> Option<Arc<TrafficData>> {
        if track.id != "colorado" {
            return None;
        }
        if !self.tried {
            self.tried = true;
            // On a thread: the lane fit (network.rs) raycasts across every traffic way.
            let (assets, ground) = (garage.assets.clone(), track.ground.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                let ground: &dyn fh1_engine::vehicle::Ground = &*ground;
                let r = TrafficData::load_on(&assets, MIRROR_Z, Some(ground)).map_err(|e| format!("{e:#}"));
                let _ = tx.send((r, t0.elapsed()));
            });
            self.loading = Some(std::sync::Mutex::new(rx));
        }
        let got = self.loading.as_ref().map(|rx| rx.lock().map_or(Err(std::sync::mpsc::TryRecvError::Disconnected), |rx| rx.try_recv()));
        match got {
            Some(Ok((r, took))) => {
                self.loading = None;
                match r {
                    Ok(d) => {
                        info!(
                            "traffic: {} lanes, {:.0} km; {} festival drivers; loaded in {:.0} ms (lane fit incl.)",
                            d.network.lanes.len(),
                            d.network.lanes.iter().map(|l| l.length).sum::<f32>() / 1000.0,
                            d.festival_cars.len(),
                            took.as_secs_f32() * 1000.0
                        );
                        self.data = Some(Arc::new(d));
                    }
                    Err(e) => warn!("traffic off: {e}"),
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return None,
            Some(Err(_)) => {
                self.loading = None;
                warn!("traffic off: the load thread stopped");
            }
            None => {}
        }
        self.data.clone()
    }

    /// The traffic data is still loading (on its thread).
    fn pending(&self) -> bool {
        self.loading.is_some()
    }
}

/// Traffic cars to remove, despawned in `Last`: other systems may still have commands queued this frame for a car
/// (and its body / scene children) that was spawned a frame ago; despawning in Update made those commands fail.
/// Also tagged with the world generation it was filled in: after an in-process map change (ui/world_load.rs
/// switch_world, which despawns every traffic car itself) the old ids are dropped, never despawned (their indices may
/// belong to the new map's entities by then).
#[derive(Resource, Default)]
struct PendingDespawn(Vec<Entity>, u32);

fn apply_despawns(mut commands: Commands, mut pending: ResMut<PendingDespawn>, generation: Res<crate::ui::world_load::WorldGeneration>) {
    if pending.1 != generation.0 {
        pending.0.clear();
        pending.1 = generation.0;
        return;
    }
    for e in pending.0.drain(..) {
        if let Ok(mut ec) = commands.get_entity(e) {
            ec.try_despawn();
        }
    }
}

/// Per-car simulation state.
#[derive(Component)]
pub struct TrafficSim {
    driver: TrafficDriver,
    /// Cruise speed of the current road (m/s).
    cruise: f32,
    /// CG height above the ground at rest.
    ride_h: f32,
    /// Far mode ground: last ray hit height, the lane height there, and the ground normal.
    ground_y: f32,
    lane_y: f32,
    normal: Vec3,
    /// Lane-change smoothing offset (far mode).
    blend: Vec3,
    last_lane: u32,
    /// Knocked off its lane: seconds since.
    wrecked: Option<f32>,
    /// Seconds stopped behind the player.
    blocked_s: f32,
    phase: u32,
    half_len: f32,
    reserved: Option<u32>,
    accel: f32,
}

// ---------------------------------------------------------------------------------------------------------------------
// Spawning

fn settings_name(race: Option<&crate::race::RaceState>) -> Option<&'static str> {
    let racing = race.is_some_and(|r| !matches!(r.phase, crate::race::RacePhase::Idle));
    if !racing {
        Some("freeroam")
    } else if flag("FH1_TRAFFIC_RACE", false) {
        Some("race")
    } else {
        None
    }
}

/// The density value now: between min and max. What the game lerps by is not traced: OUR rule, a slow per-id swell.
fn density_value(d: &Density, clock: f32) -> f32 {
    let busy = 0.5 + 0.5 * (clock / 180.0 + d.id as f32 * 1.7).sin();
    d.min + (d.max - d.min) * busy
}

#[allow(clippy::too_many_arguments)]
fn traffic_spawn(
    mut commands: Commands,
    mut st: ResMut<TrafficState>,
    mut pending: ResMut<PendingDespawn>,
    warm: Res<TrafficWarm>,
    time: Res<Time>,
    garage: Res<Garage>,
    track: Res<Track>,
    race: Option<Res<crate::race::RaceState>>,
    asset_server: Res<AssetServer>,
    player: Query<&Car>,
    cars: Query<(Entity, &AiCar, &TrafficCar, &TrafficSim, Has<TrafficParked>)>,
) {
    let despawn_all = |pending: &mut PendingDespawn| {
        for (e, ..) in &cars {
            if !pending.0.contains(&e) {
                pending.0.push(e);
            }
        }
    };
    let Some(data) = enabled().then(|| st.data(&garage, &track)).flatten() else {
        if !cars.is_empty() {
            despawn_all(&mut pending);
        }
        st.initial_done = false;
        return;
    };
    // A race without traffic: park everything (the pool survives for after the race).
    let Some(set_name) = settings_name(race.as_deref()) else {
        for (e, _, _, _, parked) in &cars {
            if !parked {
                commands.entity(e).insert((TrafficParked, Visibility::Hidden));
            }
        }
        st.initial_done = false;
        st.reservations.clear();
        return;
    };
    let Some(set) = data.config.set(set_name).or_else(|| data.config.set("freeroam")) else { return };
    let Some(p) = player.iter().next().map(|c| c.0.position) else { return };
    if warm.pending() {
        return;
    }
    st.clock += time.delta_secs();
    // Teleport (fast travel, reset): start over around the new spot.
    if st.last_player.is_some_and(|l| l.distance(p) > 150.0) {
        for (e, _, _, _, parked) in &cars {
            if !parked {
                commands.entity(e).insert((TrafficParked, Visibility::Hidden));
            }
        }
        st.initial_done = false;
        st.reservations.clear();
        st.last_player = Some(p);
        return;
    }
    st.last_player = Some(p);

    // Despawn beyond spawn-out.
    let net = &data.network;
    let mut active = (0usize, 0usize);
    let mut per_model: HashMap<String, u32> = HashMap::new();
    let mut positions: Vec<Vec3> = Vec::new();
    let mut pool: Vec<(Entity, String)> = Vec::new();
    let mut loaded: HashMap<String, TrafficKind> = HashMap::new();
    for (e, car, tc, sim, parked) in &cars {
        loaded.insert(car.0.data.media_name.clone(), tc.kind);
        if parked {
            if !pending.0.contains(&e) {
                pool.push((e, car.0.data.media_name.clone()));
            }
            continue;
        }
        let d = net.lanes[sim.driver.lane as usize].density;
        let out = set.traffic.get(&d).and_then(|d| d.spawn_out).unwrap_or(config::SPAWN_OUT);
        if car.0.position.distance(p) > out + 30.0 {
            commands.entity(e).insert((TrafficParked, Visibility::Hidden));
            pool.push((e, car.0.data.media_name.clone()));
            continue;
        }
        match tc.kind {
            TrafficKind::Road => active.0 += 1,
            TrafficKind::Festival => active.1 += 1,
        }
        *per_model.entry(car.0.data.media_name.clone()).or_default() += 1;
        positions.push(car.0.position);
    }
    // Pool too big: drop parked cars of the models least in use.
    if pool.len() > POOL_MAX {
        pool.sort_by_key(|(_, n)| std::cmp::Reverse(per_model.get(n).copied().unwrap_or(0)));
        for (e, n) in pool.drain(POOL_MAX..) {
            pending.0.push(e);
            if !per_model.contains_key(&n) {
                loaded.remove(&n);
            }
        }
    }
    // Forget models nobody has used for two minutes.
    let clock = st.clock;
    st.models.retain(|name, m| loaded.contains_key(name) || clock - m.last_used < 120.0);
    if st.clock < st.next_spawn {
        return;
    }
    st.next_spawn = st.clock + SPAWN_PERIOD;

    // Wanted counts: density per 100 m of lane (INFERRED unit) over the lanes within spawn-out.
    let scale = density_scale();
    let festival_on = flag("FH1_TRAFFIC_FESTIVAL", true) && !data.festival_cars.is_empty();
    let (mut want_road, mut want_fest) = (0.0f32, 0.0f32);
    for smp in net.samples_near(p, config::SPAWN_OUT) {
        let id = net.lanes[smp.lane as usize].density;
        if let Some(d) = set.traffic.get(&id).filter(|_| net.lanes[smp.lane as usize].road_traffic) {
            want_road += density_value(d, clock) * 0.01 * fh1_engine::traffic::network::SAMPLE_STEP;
        }
        if let Some(d) = set.festival.get(&id) {
            want_fest += density_value(d, clock) * 0.01 * fh1_engine::traffic::network::SAMPLE_STEP;
        }
    }
    // 28 since 2026-10-07 (was 16; user: "we can have more of them"); the body pool holds 48.
    let cap_road = env_f32("FH1_TRAFFIC_MAX", 28.0);
    let want_road = (want_road * scale).min(cap_road * scale.max(1.0)).round() as usize;
    let want_fest = if festival_on { (want_fest * scale).min(8.0 * scale.max(1.0)).round() as usize } else { 0 };

    if debug() && (st.clock % 5.0) < SPAWN_PERIOD {
        let simulated = cars.iter().filter(|c| !c.4 && c.2.simulated).count();
        info!(
            "traffic: player {p:.0}, wanted road {want_road} festival {want_fest}, active {active:?} ({simulated} simulated), pool {}, step {:.3} ms/tick",
            pool.len(),
            st.step_us / st.step_ticks.max(1) as f32 / 1000.0
        );
        (st.step_us, st.step_ticks) = (0.0, 0);
    }
    // Initial population: evenly spaced radii up to SpawnInDistance (0x82B552C8, VERIFIED shape), then ring spawns.
    let initial = !st.initial_done;
    st.initial_done = true;
    let mut jobs: Vec<(TrafficKind, Option<f32>)> = Vec::new();
    if initial {
        let n_road = (set.initial_traffic as usize).min(want_road);
        let n_fest = (set.initial_festival as usize).min(want_fest);
        let n = (n_road + n_fest).max(1);
        for k in 0..n_road + n_fest {
            let r = 80.0 + (config::SPAWN_IN - 80.0) * (k as f32 + 0.5) / n as f32;
            jobs.push((if k < n_road { TrafficKind::Road } else { TrafficKind::Festival }, Some(r)));
        }
    } else if active.0 < want_road {
        jobs.push((TrafficKind::Road, None));
    } else if active.1 < want_fest {
        jobs.push((TrafficKind::Festival, None));
    }
    for (kind, radius) in jobs {
        let Some((lane, s)) = pick_spot(&mut st, net, set, kind, p, radius, &positions) else {
            if debug() {
                info!("traffic: no {kind:?} spot (radius {radius:?})");
            }
            continue;
        };
        let Some(name) = pick_car(&mut st, &data, set, kind, net.lanes[lane as usize].density, &per_model, &loaded, &pool, &garage) else {
            if debug() {
                info!("traffic: no {kind:?} car for density {}", net.lanes[lane as usize].density);
            }
            continue;
        };
        let reuse = pool.iter().position(|(_, n)| *n == name).map(|i| pool.swap_remove(i).0);
        if reuse.is_none() {
            if st.clock < st.next_fresh {
                continue;
            }
            st.next_fresh = st.clock + fresh_period();
        }
        let Some(e) = spawn_car(&mut commands, &mut st, &garage, &track, &asset_server, net, set, kind, &name, lane, s, p, reuse) else { continue };
        *per_model.entry(name.clone()).or_default() += 1;
        loaded.insert(name.clone(), kind);
        positions.push(net.lanes[lane as usize].at(s).0);
        if debug() {
            info!("traffic: {} {kind:?} {name} ({e:?}) on lane {lane} ({:?}, density {}), {:.0} m away; road {}/{want_road}, festival {}/{want_fest}", if reuse.is_some() { "reused" } else { "spawned" }, net.lanes[lane as usize].road, net.lanes[lane as usize].density, net.lanes[lane as usize].at(s).0.distance(p), active.0, active.1);
        }
    }
}

/// Pre-build the traffic pool behind the startup loading card (ui/loading.rs waits on [`TrafficWarm`]): every road
/// model of the freeroam CarList (2 each, maxactive-1 models once) and 2 each of 4 festival driver models, spawned parked.
/// Free roam then reuses them instead of building a body on the fly (each build used to hitch 100-700 ms).
#[allow(clippy::too_many_arguments)]
fn warm_pool(
    mut commands: Commands,
    mut st: ResMut<TrafficState>,
    mut warm: ResMut<TrafficWarm>,
    time: Res<Time<Real>>,
    garage: Res<Garage>,
    track: Res<Track>,
    asset_server: Res<AssetServer>,
    built: Query<(), (With<fh1_render::car::FxCarDrawn>, With<fh1_render::car::FxCarBodyShared>)>,
    scene_ready: Query<(), (With<BodySceneReady>, With<fh1_render::car::FxCarBodyShared>)>,
    generation: Res<crate::ui::world_load::WorldGeneration>,
    mut built_for: Local<Option<u32>>,
) {
    let now = time.elapsed_secs();
    // A map change (switch_world resets TrafficState and despawns the cars): plan again for the new map.
    if *built_for != Some(generation.0) {
        *built_for = Some(generation.0);
        st.warm_plan = None;
        st.warm_next = 0;
        *warm = TrafficWarm::default();
    }
    if st.warm_plan.is_none() {
        if !enabled() || track.id != "colorado" {
            st.warm_plan = Some(Vec::new());
            return;
        }
        let Some(data) = st.data(&garage, &track) else {
            // Still loading: try again next frame (the pool waits for it).
            if !st.pending() {
                st.warm_plan = Some(Vec::new());
            }
            return;
        };
        let Some(set) = data.config.set("freeroam") else {
            st.warm_plan = Some(Vec::new());
            return;
        };
        let installed = |name: &String| garage.assets.join("cars").join(name).join("physics.json").exists();
        let mut plan = Vec::new();
        for c in &set.cars {
            let Some(name) = data.cars.get(&c.model).filter(|n| installed(n)) else { continue };
            for _ in 0..c.max_active.unwrap_or(WARM_PER_MODEL).min(WARM_PER_MODEL) {
                plan.push((name.clone(), TrafficKind::Road));
            }
        }
        if flag("FH1_TRAFFIC_FESTIVAL", true) {
            let mut fest: Vec<String> = data.festival_cars.iter().filter_map(|id| data.cars.get(id).cloned()).filter(|n| installed(n)).collect();
            fest.sort();
            fest.dedup();
            for _ in 0..FESTIVAL_MODELS.min(fest.len()) {
                let k = ((st.rand() * fest.len() as f32) as usize).min(fest.len() - 1);
                let name = fest.swap_remove(k);
                for _ in 0..WARM_PER_MODEL {
                    plan.push((name.clone(), TrafficKind::Festival));
                }
            }
        }
        info!("traffic: pre-building {} bodies behind the loading card", plan.len());
        *warm = TrafficWarm { total: plan.len() as u32, done: 0, started_at: Some(now) };
        st.warm_plan = Some(plan);
    }
    if !warm.pending() {
        return;
    }
    let Some(data) = st.data.clone() else { return };
    let Some(set) = data.config.set("freeroam") else { return };
    let net = &data.network;
    let plan = st.warm_plan.clone().unwrap_or_default();
    let lane = 0u32;
    let fast = flag("FH1_TRAFFIC_WARM_FAST", false);
    let per_frame = if fast { 2 } else { WARM_PER_FRAME };
    let after_slow = !fast && time.delta_secs() * 1000.0 > WARM_SLOW_MS;
    for _ in 0..if after_slow { 0 } else { per_frame } {
        let Some((name, kind)) = plan.get(st.warm_next).cloned() else { break };
        st.warm_next += 1;
        if let Some(e) = spawn_car(&mut commands, &mut st, &garage, &track, &asset_server, net, set, kind, &name, lane, 0.0, Vec3::splat(1e6), None) {
            commands.entity(e).insert((TrafficParked, Visibility::Hidden));
        }
    }
    // Faithful: FxCarDrawn = the game-shaded body is built. Remaster marks FxCarDrawn at once (the glTF body draws
    // itself), so there the body counts once its glTF instance has spawned; else the card lifted while ~40 scenes were
    // still instantiating. FH1_TRAFFIC_WARM_SCENE=0 = count FxCarDrawn in remaster too.
    let n = if fh1_remaster::enabled() && flag("FH1_TRAFFIC_WARM_SCENE", true) { scene_ready.iter().count() } else { built.iter().count() };
    warm.done = (n as u32).min(warm.total);
    let waited = warm.started_at.map_or(0.0, |t| now - t);
    if warm.pending() && waited > WARM_TIMEOUT_S {
        warn!("traffic: pool pre-build timed out at {}/{} after {waited:.0} s", warm.done, warm.total);
        warm.done = warm.total;
    } else if !warm.pending() {
        info!("traffic: pool of {} bodies built in {waited:.1} s", warm.total);
    }
}

/// A spawn point: on a lane at the spawn-in ring (or at `radius` for the initial cars), weighted by density, facing
/// away from the player with the road's spawnAwayChance, clear of other cars.
fn pick_spot(st: &mut TrafficState, net: &Network, set: &SettingsSet, kind: TrafficKind, p: Vec3, radius: Option<f32>, others: &[Vec3]) -> Option<(u32, f32)> {
    let list = match kind {
        TrafficKind::Road => &set.traffic,
        TrafficKind::Festival => &set.festival,
    };
    let clock = st.clock;
    let mut cands: Vec<(u32, f32, Vec3, f32)> = Vec::new();
    let mut total = 0.0;
    for smp in net.samples_near(p, radius.unwrap_or(config::SPAWN_IN) + 5.0) {
        let lane = &net.lanes[smp.lane as usize];
        if kind == TrafficKind::Road && !lane.road_traffic {
            continue;
        }
        let Some(d) = list.get(&lane.density) else { continue };
        let ring = radius.unwrap_or(d.spawn_in.unwrap_or(config::SPAWN_IN));
        let dist = Vec3::new(smp.pos.x - p.x, 0.0, smp.pos.z - p.z).length();
        if !(ring - 30.0..=ring + 5.0).contains(&dist) {
            continue;
        }
        let w = density_value(d, clock);
        if w <= 0.0 {
            continue;
        }
        total += w;
        cands.push((smp.lane, smp.s, smp.pos, w));
    }
    for _ in 0..6 {
        if total <= 0.0 {
            return None;
        }
        let mut r = st.rand() * total;
        let &(mut lane, mut s, pos, _) = cands.iter().find(|c| {
            r -= c.3;
            r <= 0.0
        })?;
        if others.iter().any(|o| o.distance(pos) < 30.0) {
            continue;
        }
        // Direction: away from the player with spawnAwayChance.
        let l = &net.lanes[lane as usize];
        let chance = list.get(&l.density).and_then(|d| d.away_chance).unwrap_or(0.5 * (config::AWAY_CHANCE_MIN + config::AWAY_CHANCE_MAX));
        let want_away = st.rand() < chance;
        let away = l.at(s).1.dot(pos - p) > 0.0;
        if away != want_away {
            if let Some(t) = l.twin {
                let tl = &net.lanes[t as usize];
                lane = t;
                s = tl.project(pos, tl.length - s).0;
            }
        }
        return Some((lane, s));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn pick_car(
    st: &mut TrafficState,
    data: &TrafficData,
    set: &SettingsSet,
    kind: TrafficKind,
    density: u32,
    per_model: &HashMap<String, u32>,
    loaded: &HashMap<String, TrafficKind>,
    parked: &[(Entity, String)],
    garage: &Garage,
) -> Option<String> {
    let installed = |name: &String| garage.cars.iter().any(|c| c == name) || garage.assets.join("cars").join(name).join("physics.json").exists();
    let mut cands: Vec<String> = match kind {
        TrafficKind::Road => {
            let d = set.traffic.get(&density)?;
            set.cars
                .iter()
                .filter(|c| set.allowed(d, c.model))
                .filter_map(|c| {
                    let name = data.cars.get(&c.model)?;
                    let n = per_model.get(name).copied().unwrap_or(0);
                    (c.max_active.is_none_or(|m| n < m)).then(|| name.clone())
                })
                .collect()
        }
        TrafficKind::Festival => data.festival_cars.iter().filter_map(|id| data.cars.get(id).cloned()).collect(),
    };
    cands.sort();
    cands.dedup();
    cands.retain(installed);
    // A parked car of an allowed model: reuse it (no new body to build).
    let reusable: Vec<String> = cands.iter().filter(|n| parked.iter().any(|(_, p)| p == *n)).cloned().collect();
    if !reusable.is_empty() {
        cands = reusable;
    } else {
        // maxLoadedTrafficModels (road) / FESTIVAL_MODELS: with the budget full, only models already loaded.
        let budget = match kind {
            TrafficKind::Road => set.max_loaded_models as usize,
            TrafficKind::Festival => FESTIVAL_MODELS,
        };
        let mine: Vec<&String> = loaded.iter().filter(|(_, k)| **k == kind).map(|(n, _)| n).collect();
        if mine.len() >= budget {
            let in_budget: Vec<String> = cands.iter().filter(|n| mine.contains(n)).cloned().collect();
            if !in_budget.is_empty() {
                cands = in_budget;
            }
        }
    }
    if cands.is_empty() {
        return None;
    }
    let k = ((st.rand() * cands.len() as f32) as usize).min(cands.len() - 1);
    Some(cands.swap_remove(k))
}

#[allow(clippy::too_many_arguments)]
fn spawn_car(
    commands: &mut Commands,
    st: &mut TrafficState,
    garage: &Garage,
    track: &Track,
    asset_server: &AssetServer,
    net: &Network,
    set: &SettingsSet,
    kind: TrafficKind,
    name: &str,
    lane: u32,
    s: f32,
    player: Vec3,
    reuse: Option<Entity>,
) -> Option<Entity> {
    let clock = st.clock;
    if !st.models.contains_key(name) {
        let dir = garage.assets.join("cars").join(name);
        let data = match CarData::load(&dir) {
            Ok(d) => d,
            Err(e) => {
                warn!("traffic: {name}: {e:#}");
                return None;
            }
        };
        let paints = std::fs::read(dir.join("physics.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["colors"].as_array().map(|a| dull_paints(a)))
            .unwrap_or_default();
        let scene = asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("cars/{name}/model.gltf")));
        st.models.insert(name.to_owned(), Model { data, scene, paints, last_used: clock });
    }
    let paint_pick = st.rand();
    let seed = (st.rand() * u32::MAX as f32) as u32;
    let m = st.models.get_mut(name)?;
    m.last_used = clock;
    let l = &net.lanes[lane as usize];
    let (pos, tangent) = l.at(s);
    let ground = track.ground.ray(pos + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0).map_or(pos, |h| h.point);
    let yaw = (-tangent.x).atan2(-tangent.z);
    let mut v = Vehicle::new(m.data.clone(), ground);
    v.direct_steer = fh1_engine::traffic::driver::direct_steer();
    let ride_h = v.position.y - ground.y;
    v.place(ground, yaw);
    let driver = TrafficDriver::with_kind(net, lane, s, seed, kind == TrafficKind::Road);
    let cruise = cruise_speed(set, kind, l.density);
    let mut driver = driver;
    driver.speed = cruise * 0.8;
    v.velocity = tangent * driver.speed;
    let simulated = ground.distance(player) < near_radius();
    if !simulated {
        v.position = ground + Vec3::Y * ride_h;
        v.prev_position = v.position;
    }
    let [a, b] = v.data.bbox;
    let half_len = 0.5 * (b.z - a.z).abs().max(3.0);
    let lamp_hub = v.data.hubs[0];
    let cg = v.cg_model;
    let paint = if m.paints.is_empty() { 0 } else { m.paints[((paint_pick * m.paints.len() as f32) as usize).min(m.paints.len() - 1)] };
    let scene = m.scene.clone();
    let sim = TrafficSim {
        driver,
        cruise,
        ride_h,
        ground_y: ground.y,
        lane_y: pos.y,
        normal: Vec3::Y,
        blend: Vec3::ZERO,
        last_lane: lane,
        wrecked: None,
        blocked_s: 0.0,
        phase: seed % 4,
        half_len,
        reserved: None,
        accel: 0.0,
    };
    if let Some(e) = reuse {
        // The parked car keeps its body, wheels and paint.
        commands.entity(e).remove::<TrafficParked>().insert((
            AiCar(v),
            TrafficCar { kind, simulated, horn: false },
            sim,
            Transform::from_translation(ground).with_rotation(Quat::from_rotation_y(yaw)),
            Visibility::Hidden,
        ));
        return Some(e);
    }
    let e = commands
        .spawn((
            AiCar(v),
            TrafficCar { kind, simulated, horn: false },
            sim,
            Transform::from_translation(ground).with_rotation(Quat::from_rotation_y(yaw)),
            Visibility::Hidden,
            fh1_render::headlight::FxHeadlightSource { player: false, lamp: Vec3::new(0.0, lamp_hub[1] + 0.25, lamp_hub[2] - 0.8) - cg },
            fh1_render::car_shadow::drop_shadow::FxDropShadow::default(),
            fh1_render::car::FxCarLamps::default(),
            Name::new(format!("Traffic {name}")),
        ))
        .with_children(|c| {
            let mut body = c.spawn((
                WorldAssetRoot(scene),
                Transform::from_translation(-cg),
                fh1_render::car::FxCarBody { assets: garage.assets.clone(), car: name.to_owned(), track: track.id.clone() },
                fh1_render::car::FxCarBodyShared,
            ));
            if paint != 0 {
                body.insert(fh1_render::car::FxCarPaint { sequence: paint });
            }
        })
        .id();
    Some(e)
}

/// A pooled car body whose glTF scene instance has spawned (remaster pre-build readiness, see `warm_pool`).
#[derive(Component)]
struct BodySceneReady;

fn mark_scene_ready(ev: On<bevy::world_serialization::WorldInstanceReady>, bodies: Query<(), With<fh1_render::car::FxCarBodyShared>>, mut commands: Commands) {
    if bodies.contains(ev.entity) {
        commands.entity(ev.entity).insert(BodySceneReady);
    }
}

/// Cruise speed on a road: the density's `speed` (mph, INFERRED unit); festival densities have none, so festival
/// drivers take the traffic speed of the road × 1.2 (OUR rule).
fn cruise_speed(set: &SettingsSet, kind: TrafficKind, density: u32) -> f32 {
    let base = set.traffic.get(&density).and_then(|d| d.speed_mph).unwrap_or(30.0);
    mph(base) * if kind == TrafficKind::Festival { 1.2 } else { 1.0 }
}

// ---------------------------------------------------------------------------------------------------------------------
// Driving

/// One obstacle for the leader search.
#[derive(Clone, Copy)]
struct Other {
    e: Option<Entity>,
    o: Obstacle,
    player: bool,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn step_traffic(
    time: Res<Time<Fixed>>,
    track: Res<Track>,
    race: Option<Res<crate::race::RaceState>>,
    mut st: ResMut<TrafficState>,
    mut props: ResMut<crate::smash::PropCollision>,
    mut traffic: Query<(Entity, &mut AiCar, &mut TrafficSim, &mut TrafficCar, Has<TrafficParked>)>,
    racers: Query<&AiCar, Without<TrafficCar>>,
    mut player: Query<&mut Car>,
) {
    if traffic.is_empty() {
        return;
    }
    let t0 = std::time::Instant::now();
    let Some(data) = st.data.clone() else { return };
    let net = &data.network;
    let set_name = settings_name(race.as_deref()).unwrap_or("freeroam");
    let Some(set) = data.config.set(set_name).or_else(|| data.config.set("freeroam")) else { return };
    let dt = time.delta_secs();
    let clock = st.clock;
    let player_pos = player.iter().next().map(|c| c.0.position).unwrap_or(Vec3::ZERO);

    let mut others: Vec<Other> = traffic.iter().filter(|t| !t.4).map(|(e, c, ..)| Other { e: Some(e), o: Obstacle::of(&c.0), player: false }).collect();
    others.extend(racers.iter().map(|c| Other { e: None, o: Obstacle::of(&c.0), player: false }));
    if let Some(p) = player.iter().next() {
        others.push(Other { e: None, o: Obstacle::of(&p.0), player: true });
    }
    st.reservations.retain(|_, r| r.2 > clock);

    for (e, mut car, mut sim, mut tc, parked) in &mut traffic {
        let v = &mut car.0;
        if parked {
            if v.position.y != PARK_Y {
                v.position = Vec3::new(0.0, PARK_Y, 0.0);
                v.prev_position = v.position;
                v.velocity = Vec3::ZERO;
                v.rpm = v.data.idle_rpm;
                tc.simulated = false;
                tc.horn = false;
                if let Some(n) = sim.reserved.take() {
                    if st.reservations.get(&n).is_some_and(|r| r.0 == e) {
                        st.reservations.remove(&n);
                    }
                }
            }
            continue;
        }
        let dist = v.position.distance(player_pos);
        let up = v.rotation * Vec3::Y;
        // Mode switch.
        let want_sim = dist < near_radius() || sim.wrecked.is_some();
        if want_sim && !tc.simulated {
            let (pos, t) = sim.driver.ahead(net, 0.0);
            let ground = track.ground.ray(pos + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0).map_or(Vec3::new(pos.x, sim.ground_y, pos.z), |h| h.point);
            let speed = sim.driver.speed;
            v.place(ground, (-t.x).atan2(-t.z));
            v.velocity = t * speed;
            for i in 0..4 {
                v.wheels[i].omega = speed / v.data.tyre_radius[i / 2].max(0.2);
            }
            // place() rebuilds the car in 1st gear at idle: at cruise speed that is a huge engine-braking / clutch-slip
            // jolt on the driven wheels the moment a far car enters the sim near the player (the headless test starts from
            // rest and never takes this path). Pick the gear for the speed with the clutch locked. FH1_TRAFFIC_SYNC_GEAR=0 = old.
            if speed > 1.0 && flag("FH1_TRAFFIC_SYNC_GEAR", true) {
                v.sync_drivetrain();
            }
            tc.simulated = true;
        } else if !want_sim && tc.simulated && dist > near_radius() + 30.0 && up.y > 0.8 {
            let err = sim.driver.sync(net, v.position);
            if err < 2.5 {
                sim.driver.speed = v.forward_speed().max(0.0);
                sim.blend = Vec3::new(v.position.x, 0.0, v.position.z) - {
                    let p = sim.driver.ahead(net, 0.0).0;
                    Vec3::new(p.x, 0.0, p.z)
                };
                sim.ground_y = v.position.y - sim.ride_h;
                sim.lane_y = sim.driver.ahead(net, 0.0).0.y;
                sim.last_lane = sim.driver.lane;
                tc.simulated = false;
            }
        }

        // Leader along the path.
        let speed = if tc.simulated { v.forward_speed().max(0.0) } else { sim.driver.speed };
        let look = (speed * 3.0 + 25.0).min(90.0);
        let mut leader: Option<(f32, f32)> = None;
        let mut leader_is_player = false;
        let fwd = (v.rotation * Vec3::NEG_Z).normalize_or(Vec3::NEG_Z);
        let path: Vec<(f32, Vec3, Vec3)> = (1..=(look / 4.0) as usize).map(|k| {
            let d = k as f32 * 4.0;
            let (p, t) = sim.driver.ahead(net, d);
            (d, p, t)
        }).collect();
        for o in &others {
            if o.e == Some(e) {
                continue;
            }
            let rel = o.o.position - v.position;
            if rel.length_squared() > (look + 10.0) * (look + 10.0) || rel.dot(fwd) <= 0.0 {
                continue;
            }
            let reach = o.o.half_width + 1.1;
            for &(d, p, t) in &path {
                let off = o.o.position - p;
                if Vec3::new(off.x, 0.0, off.z).length() < reach {
                    let gap = d - sim.half_len - o.o.half_length;
                    let vl = o.o.velocity.dot(t).max(0.0);
                    if leader.is_none_or(|l| gap < l.0) {
                        leader = Some((gap, vl));
                        leader_is_player = o.player;
                    }
                    break;
                }
            }
        }
        // Junction yield: reserve the node ahead; give way to a car of an equal or bigger road holding it.
        let (to_end, node) = sim.driver.to_lane_end(net);
        let lane = &net.lanes[sim.driver.lane as usize];
        if sim.reserved.is_some_and(|n| n != node || sim.driver.s > 12.0 && lane.from == n) {
            if let Some(n) = sim.reserved.take() {
                if st.reservations.get(&n).is_some_and(|r| r.0 == e) {
                    st.reservations.remove(&n);
                }
            }
        }
        if net.is_junction(node) && to_end < 30.0 {
            // Turning here (the next lane leaves at an angle)?
            let turning = sim.driver.plan.first().is_some_and(|&n| lane.at(lane.length).1.angle_between(net.lanes[n as usize].at(0.0).1) > 0.6);
            match st.reservations.get(&node) {
                Some(&(holder, rank, _)) if holder != e && (rank > lane.road || rank == lane.road && turning) => {
                    let gap = (to_end - 2.0 - sim.half_len).max(0.0);
                    if leader.is_none_or(|l| gap < l.0) {
                        leader = Some((gap, 0.0));
                        leader_is_player = false;
                    }
                }
                _ => {
                    st.reservations.insert(node, (e, lane.road, clock + 6.0));
                    sim.reserved = Some(node);
                }
            }
        }
        let cruise = cruise_speed(set, if tc.kind == TrafficKind::Festival { TrafficKind::Festival } else { TrafficKind::Road }, lane.density);
        sim.cruise = cruise;
        let v0 = sim.driver.target_speed(net, cruise);
        let accel = TrafficDriver::idm(speed, v0, leader);
        sim.accel = accel;
        // Horn: stuck behind the player (OUR rule).
        if leader_is_player && speed < 1.0 && leader.is_some_and(|l| l.0 < 8.0) {
            sim.blocked_s += dt;
        } else {
            sim.blocked_s = 0.0;
        }
        tc.horn = (1.5..4.0).contains(&sim.blocked_s);

        v.begin_tick();
        if tc.simulated {
            let err = sim.driver.sync(net, v.position);
            if err > 6.0 || up.y < 0.5 {
                sim.wrecked.get_or_insert(0.0);
            }
            let mut recover = false;
            let controls = if let Some(w) = sim.wrecked {
                sim.wrecked = Some(w + dt);
                // Back on the lane when the player has left it behind (OUR rule; the game's recovery isn't traced).
                recover = w + dt > 5.0 && dist > 50.0;
                Controls { brake: 1.0, ..Default::default() }
            } else {
                sim.driver.controls(net, v, accel)
            };
            let ground = crate::smash::PropGround::new(track.ground.as_ref(), Some(&*props), v);
            for _ in 0..SUBSTEPS {
                v.step(controls, dt / SUBSTEPS as f32, &ground);
            }
            let hits = ground.take_hits();
            drop(ground);
            props.apply_hits(v, hits);
            sim.driver.speed = v.forward_speed().max(0.0);
            if recover {
                if let Some((lane, s, _)) = net.nearest(v.position, 30.0, None) {
                    let (p, t) = net.lanes[lane as usize].at(s);
                    let ground = track.ground.ray(p + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0).map_or(p, |h| h.point);
                    v.place(ground, (-t.x).atan2(-t.z));
                    sim.driver = TrafficDriver::with_kind(net, lane, s, (e.to_bits() as u32).wrapping_mul(2654435761), tc.kind == TrafficKind::Road);
                    sim.last_lane = lane;
                }
                sim.wrecked = None;
            }
        } else {
            let (pos, t) = sim.driver.kinematic(net, accel, dt);
            if sim.driver.lane != sim.last_lane {
                // Lanes of different roads don't meet exactly at a junction: ease over the step.
                sim.blend = Vec3::new(v.position.x - pos.x, 0.0, v.position.z - pos.z);
                sim.last_lane = sim.driver.lane;
            }
            sim.blend *= (-dt / 0.6).exp();
            sim.phase = sim.phase.wrapping_add(1);
            if sim.phase % 4 == 0 {
                if let Some(h) = track.ground.ray(pos + Vec3::Y * 4.0, Vec3::NEG_Y, 12.0) {
                    sim.ground_y = h.point.y;
                    sim.lane_y = pos.y;
                    sim.normal = sim.normal.lerp(h.normal, 0.5).normalize_or(Vec3::Y);
                }
            }
            let gy = sim.ground_y + (pos.y - sim.lane_y);
            let upn = sim.normal;
            let fwd = (t - upn * t.dot(upn)).normalize_or(t);
            let right = fwd.cross(upn).normalize_or(Vec3::X);
            let upn = right.cross(fwd);
            v.rotation = Quat::from_mat3(&Mat3::from_cols(right, upn, -fwd));
            v.position = Vec3::new(pos.x, gy, pos.z) + sim.blend + upn * sim.ride_h;
            v.velocity = fwd * sim.driver.speed;
            v.angular_velocity = Vec3::ZERO;
            let (gear, rpm) = cruise_rpm(v, sim.driver.speed);
            v.gear = gear;
            v.rpm = rpm;
            v.torque_fraction = (0.25 + 0.3 * accel).clamp(-0.2, 1.0);
            for i in 0..4 {
                let r = v.data.tyre_radius[i / 2].max(0.2);
                v.wheels[i].omega = sim.driver.speed / r;
                v.wheels[i].angle += sim.driver.speed / r * dt;
                v.wheels[i].steer = 0.0;
            }
        }
    }

    // Car-vs-car contact: simulated traffic against the player, race AI and each other (pairs within 8 m).
    let mut sims: Vec<(Mut<AiCar>, Mut<TrafficSim>)> = traffic.iter_mut().filter(|(_, _, _, tc, parked)| tc.simulated && !parked).map(|(_, c, s, ..)| (c, s)).collect();
    let mut p = player.iter_mut().next();
    for i in 0..sims.len() {
        if let Some(pc) = p.as_mut() {
            if pc.0.position.distance(sims[i].0 .0.position) < 8.0 {
                let before = sims[i].0 .0.velocity;
                contact::collide(&mut pc.0, &mut sims[i].0 .0);
                if (sims[i].0 .0.velocity - before).length() > 3.0 {
                    sims[i].1.wrecked.get_or_insert(0.0);
                }
            }
        }
        for j in i + 1..sims.len() {
            let (a, b) = sims.split_at_mut(j);
            if a[i].0 .0.position.distance(b[0].0 .0.position) < 8.0 {
                contact::collide(&mut a[i].0 .0, &mut b[0].0 .0);
            }
        }
    }
    st.step_us += t0.elapsed().as_secs_f32() * 1e6;
    st.step_ticks += 1;
}

// ---------------------------------------------------------------------------------------------------------------------
// Visuals

#[allow(clippy::type_complexity)]
fn sync_traffic_visuals(
    fixed: Res<Time<Fixed>>,
    player: Query<&Car>,
    mut cars: Query<(&AiCar, &TrafficSim, &mut Transform, &mut Visibility, &mut fh1_render::car::FxCarLamps), (With<TrafficCar>, Without<AiWheel>, Without<TrafficParked>)>,
    mut wheels: Query<(&AiWheel, &mut Transform), Without<AiCar>>,
) {
    let alpha = fixed.overstep_fraction();
    let eye = player.iter().next().map(|c| c.0.position);
    let draw = draw_dist();
    for (car, sim, mut t, mut vis, mut lamps) in &mut cars {
        let (p, r) = car.0.render_pose(alpha);
        t.translation = p;
        t.rotation = r;
        // Each game-shaded body is ~140 draws: far traffic stays hidden (OUR rule, FH1_TRAFFIC_DRAW).
        let d = eye.map_or(0.0, |e| e.distance(p));
        let want = if d < draw || (*vis != Visibility::Hidden && d < draw + 15.0) { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
        let braking = sim.accel < -0.8 || (sim.driver.speed < 0.3 && sim.accel < 0.0);
        lamps.brake = if braking { 1.0 } else { 0.0 };
        lamps.reverse = 0.0;
    }
    for (w, mut wt) in &mut wheels {
        let Ok((car, _, _, vis, _)) = cars.get(w.car) else { continue };
        if *vis == Visibility::Hidden {
            continue;
        }
        let v = &car.0;
        wt.translation = w.hub + Vec3::Y * (v.wheel_drop(w.index) + crate::tyre_vis_lift(v.wheels[w.index].tyre_deflection));
        wt.rotation = Quat::from_rotation_y(v.wheels[w.index].steer) * Quat::from_rotation_x(-v.wheels[w.index].angle);
        wt.scale = w.scale;
    }
}

/// Everyday colours for traffic (user 2026-10-07: "a few boring colours like regular traffic"): the car's own factory
/// colours (Combo_Colors) that are greys / white / silver / black or very dark; if it has none, its least colourful one.
/// `FH1_TRAFFIC_ALL_PAINTS=1` = every factory colour (before).
fn dull_paints(colors: &[serde_json::Value]) -> Vec<u32> {
    let all: Vec<(u32, f32, f32)> = colors
        .iter()
        .filter_map(|c| {
            let seq = c["Sequence"].as_u64()? as u32;
            let rgb = c["RGB"].as_u64().unwrap_or(0) as u32;
            let (r, g, b) = (((rgb >> 16) & 255) as f32 / 255.0, ((rgb >> 8) & 255) as f32 / 255.0, (rgb & 255) as f32 / 255.0);
            let (max, min) = (r.max(g).max(b), r.min(g).min(b));
            let sat = if max > 1e-3 { (max - min) / max } else { 0.0 };
            Some((seq, sat, max))
        })
        .collect();
    if std::env::var("FH1_TRAFFIC_ALL_PAINTS").is_ok_and(|v| v == "1") {
        return all.iter().map(|c| c.0).collect();
    }
    // Greys / white / silver / black, plus muted colours (dark blue, maroon, forest green, beige): user 2026-10-07,
    // "all silver / black, maybe add a few more colours". Loud colours (bright, saturated) stay out.
    let dull: Vec<u32> = all.iter().filter(|(_, sat, val)| *sat < 0.25 || *val < 0.22 || (*sat < 0.7 && *val < 0.6)).map(|c| c.0).collect();
    if !dull.is_empty() {
        return dull;
    }
    all.iter().min_by(|a, b| a.1.total_cmp(&b.1)).map(|c| vec![c.0]).unwrap_or_default()
}
