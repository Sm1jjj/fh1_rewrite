//! Races (R1, docs/RACES.md): Colorado's career events as in FH1 — festival circuits / point-to-points, street
//! races, showcases, nemesis and headline races — from the `events` setup group (`<assets>/events/colorado/
//! events.json`, fh1setup events.rs: gamedb Races/Events + the TrackRoute files).
//!
//! Flow: free roam shows each event's marker (festival = its GameObjs node, street = its grid); stop inside the
//! marker's 25 m TriggerZone (game: radius 25, under 150 mph) and press A / Enter, or pick it from the event list
//! (F6, or the pause menu once wired). Then: grid (player last, `PIWithPlayerLast`), the event's collision barrier
//! bits and event-only objects, 3-2-1-GO, gates / laps, wrong way, reset to track (R / Y: the last gate), timing,
//! finish, results with positions and the event's credits per place (EventScoring), back to free roam at the
//! post-race location. Opponents come from R2 (`race/ai_link.rs`); without them the race runs solo.

use std::path::Path;

use bevy::prelude::*;

use crate::Car;

mod ai_link;
pub mod airborne;
pub mod airborne_link;
pub mod states;
pub mod field;
pub mod anark_hud;
mod hud;
pub mod postrace;
pub mod visuals;

pub use hud::{race_hud, spawn_race_hud};

/// One gate: centre, forward (unit, race direction, x/z) and half width (m).
#[derive(Clone, Copy, Debug)]
pub struct Gate {
    pub centre: Vec3,
    pub forward: Vec2,
    pub half_width: f32,
}

/// One AI entrant from gamedb EventParticipants.
#[derive(Clone, Debug)]
pub struct Entrant {
    pub car: Option<String>,
    pub driver: u32,
    pub color_seq: u32,
}

/// A race as installed (engine space).
#[derive(Clone, Debug)]
pub struct RaceDef {
    pub horizon_id: String,
    pub name: String,
    pub kind: String,
    pub mode: u32,
    pub laps: u32,
    pub circuit: bool,
    pub credits: u32,
    pub drivers: u32,
    /// gamedb Events.TrackID, parsed with the race (not read yet).
    #[allow(dead_code)]
    pub track_id: u32,
    pub route_file: String,
    pub length_m: f32,
    pub marker: (Vec3, f32),
    pub grid: Vec<(Vec3, f32)>,
    pub gates: Vec<Gate>,
    /// Circuits: lap gates already behind the grid (lap 1 starts there; the lap line is the finish).
    pub start_gate: u32,
    pub path: Vec<Vec3>,
    pub post_race: Option<(Vec3, f32)>,
    pub barrier_bits: u16,
    pub objects: Vec<(u16, Mat4)>,
    pub field: Vec<Entrant>,
    /// AISkills / AITemperaments / AIRubberbands ids per difficulty column (Easy, Med, Hard, Pro; Options "AI difficulty" picks one).
    pub ai: [(u32, u32, u32); 4],
    /// Career columns (events-2, docs/PROGRESSION.md; defaults on older installs): Events.Id, CareerTypeId, Level
    /// (wristband 0..6, -1 = intro), HubId (0 = festival, 1..3 = street hubs), UnlockPointsReq (XP), PopularityPointsReq
    /// (popularity rank needed, 0 = none), EventOrder.
    pub event_id: u32,
    pub career_type: u32,
    pub level: i32,
    pub hub: u32,
    pub unlock_xp: u64,
    pub popularity_req: u32,
    pub event_order: u32,
    /// Events.TargetClass (CarClasses id), None = open.
    pub target_class: Option<u32>,
    /// Forced player car (exhibitions / nemesis), RestrictionDescription, prize car (Rewards_EventPrizes), recommended cars.
    #[allow(dead_code)]
    pub player_car: Option<String>,
    pub restriction: Option<String>,
    pub prize_car: Option<String>,
    pub recommended: Vec<String>,
    /// Finish cannons (left row, right row: position, facing x/z) and the start gantry cannon (events-3).
    pub cannons: [Vec<(Vec3, Vec2)>; 2],
    pub start_cannon: Option<(Vec3, Vec2)>,
}

/// All installed races (`events.json`).
#[derive(Resource, Default)]
pub struct Events {
    pub races: Vec<RaceDef>,
    /// Credits per place out of 1000 (gamedb EventScoring, ScoringID 1).
    pub scoring: Vec<u32>,
    /// Event-object template -> template-space bounds (their race colliders).
    pub object_bounds: std::collections::HashMap<u16, (Vec3, Vec3)>,
    /// Event-object templates that are barriers (first submodel name contains "Barrier"): the event walls stand under them.
    pub barrier_templates: std::collections::HashSet<u16>,
    /// Career tables (wristbands, hubs, classes, drivers, cars, popularity ladder).
    pub career: crate::progression::data::CareerData,
    pub assets: std::path::PathBuf,
}

impl Events {
    /// `<assets>/events/colorado/events.json`; empty when the `events` group isn't installed.
    pub fn load(assets: &Path) -> Self {
        let path = assets.join("events/colorado/events.json");
        let Ok(bytes) = std::fs::read(&path) else {
            info!("race: no events installed ({})", path.display());
            return Self::default();
        };
        let Ok(j) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            warn!("race: {} is not valid JSON", path.display());
            return Self::default();
        };
        let v3 = |v: &serde_json::Value| Some(Vec3::new(v[0].as_f64()? as f32, v[1].as_f64()? as f32, v[2].as_f64()? as f32));
        let pose = |v: &serde_json::Value| Some((v3(v)?, v[3].as_f64()? as f32));
        let u = |v: &serde_json::Value| v.as_u64().unwrap_or(0) as u32;
        let cannon = |v: &serde_json::Value| Some((v3(v)?, Vec2::new(v[3].as_f64()? as f32, v[4].as_f64()? as f32)));
        let races: Vec<RaceDef> = j["races"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let grid: Vec<(Vec3, f32)> = r["grid"].as_array()?.iter().filter_map(pose).collect();
                let gates: Vec<Gate> = r["gates"]
                    .as_array()?
                    .iter()
                    .filter_map(|g| {
                        Some(Gate {
                            centre: v3(&g["p"])?,
                            forward: Vec2::new(g["f"][0].as_f64()? as f32, g["f"][1].as_f64()? as f32).normalize_or(Vec2::Y),
                            half_width: g["w"].as_f64()? as f32,
                        })
                    })
                    .collect();
                if grid.is_empty() || gates.is_empty() {
                    return None;
                }
                let c = &r["career"];
                let objects = r["event_objects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| {
                        let m: Vec<f32> = o["m"].as_array()?.iter().filter_map(|x| x.as_f64().map(|x| x as f32)).collect();
                        Some((o["t"].as_u64()? as u16, Mat4::from_cols_slice(m.get(..16)?)))
                    })
                    .collect();
                let field = r["field"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| Entrant { car: e["car"].as_str().map(str::to_owned), driver: u(&e["driver"]), color_seq: u(&e["color_seq"]) })
                    .collect();
                Some(RaceDef {
                    horizon_id: r["horizon_id"].as_str().unwrap_or("").to_owned(),
                    name: r["name"].as_str().unwrap_or("?").to_owned(),
                    kind: r["type"].as_str().unwrap_or("Race").to_owned(),
                    mode: u(&r["mode"]),
                    laps: u(&r["laps"]).max(1),
                    circuit: r["circuit"].as_bool().unwrap_or(false),
                    credits: u(&r["credits"]),
                    drivers: u(&r["drivers"]),
                    track_id: u(&r["track_id"]),
                    route_file: r["route_file"].as_str().unwrap_or("").to_owned(),
                    length_m: r["length_m"].as_f64().unwrap_or(0.0) as f32,
                    start_gate: u(&r["start_gate"]).min(gates.len().saturating_sub(1) as u32),
                    marker: (v3(&r["marker"]["pos"]).unwrap_or(grid[0].0), r["marker"]["yaw"].as_f64().map_or(grid[0].1, |y| y as f32)),
                    path: r["path"].as_array().into_iter().flatten().filter_map(v3).collect(),
                    post_race: pose(&r["post_race"]),
                    barrier_bits: r["barrier_bits"].as_u64().unwrap_or(0) as u16,
                    objects,
                    field,
                    ai: [0, 1, 2, 3].map(|k| (u(&r["ai_skills"][k]), u(&r["ai_temperaments"][k]), u(&r["ai_rubberbands"][k]))),
                    event_id: u(&r["event_id"]),
                    career_type: u(&r["career_type"]),
                    level: c["level"].as_i64().unwrap_or(0) as i32,
                    hub: u(&c["hub"]),
                    unlock_xp: c["unlock_xp"].as_u64().unwrap_or(0),
                    popularity_req: u(&c["popularity_req"]),
                    event_order: u(&c["event_order"]),
                    target_class: Some(u(&r["target_class"])).filter(|&t| t > 0),
                    player_car: c["player_car"].as_str().map(str::to_owned),
                    restriction: c["restriction"].as_str().filter(|t| !t.is_empty() && !t.starts_with("_&")).map(str::to_owned),
                    prize_car: r["prize_car"]["car"].as_str().map(str::to_owned),
                    cannons: [&r["cannons_left"], &r["cannons_right"]].map(|a| a.as_array().into_iter().flatten().filter_map(cannon).collect()),
                    start_cannon: cannon(&r["start_cannon"]),
                    recommended: r["recommended_cars"].as_array().into_iter().flatten().filter_map(|x| x["car"].as_str().map(str::to_owned)).collect(),
                    grid,
                    gates,
                })
            })
            .collect();
        let scoring = j["scoring"].as_array().into_iter().flatten().map(u).collect();
        let object_bounds = j["object_templates"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.parse().ok()?, (v3(&v["lo"])?, v3(&v["hi"])?))))
            .collect();
        let barrier_templates = j["object_templates"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, v)| v["name"].as_str().is_some_and(|n| n.to_ascii_uppercase().contains("BARRIER")))
            .filter_map(|(k, _)| k.parse().ok())
            .collect();
        let career = crate::progression::data::CareerData::from_json(&j["progression"], assets);
        info!("race: {} events installed (career tables: {})", races.len(), if career.installed { "events-2" } else { "built-in" });
        Self { races, scoring, object_bounds, barrier_templates, career, assets: assets.to_path_buf() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RacePhase {
    Idle,
    /// On the grid; the countdown starts after a short settle (s left).
    Grid { left_s: f32 },
    Countdown { left_s: f32 },
    Racing,
    /// The player has finished; results show after `left_s`.
    Finished { left_s: f32 },
    Results,
}

/// One car in the race (index 0 = the player).
#[derive(Clone, Debug)]
pub struct Racer {
    pub entity: Option<Entity>,
    pub is_player: bool,
    pub name: String,
    /// Driver (AIPlayers name; "YOU" for the player) and car label, for the HUD position list and results.
    pub driver: String,
    pub car: String,
    /// The car's class (CarClasses id) and PI (gamedb, the car actually driven), if known.
    pub class_pi: Option<(u32, u32)>,
    /// Gates passed in total (all laps): progress.
    pub gates_done: u32,
    pub lap: u32,
    /// Distance to the next gate (m), for ordering between gates.
    pub to_next: f32,
    pub position: u32,
    pub finished_s: Option<f32>,
    pub last_pos: Option<Vec3>,
}

/// The running race (read by R2's AI and by the HUD).
#[derive(Resource)]
pub struct RaceState {
    pub phase: RacePhase,
    pub race: Option<usize>,
    pub racers: Vec<Racer>,
    /// Race time (s, 0 at GO).
    pub clock_s: f32,
    /// Player lap times (s) and the time at the last gate / lap start.
    pub laps: Vec<f32>,
    pub lap_start_s: f32,
    /// Player split at the last gate and the best previous lap's time at the same gate (delta display).
    pub last_split: Option<(f32, Option<f32>)>,
    splits: Vec<Vec<Option<f32>>>,
    /// Wrong way: seconds the player has been going backwards along the route.
    pub wrong_way_s: f32,
    /// Off-track / stuck timers (s) for the reset to track (GameTunableSettings HandOfGodReset).
    off_track_s: f32,
    stuck_s: f32,
    /// Event list overlay (F6): open + cursor.
    pub list_open: bool,
    pub list_cursor: usize,
    /// Marker the player is standing in (race index), for the start prompt.
    pub prompt: Option<usize>,
    /// Why the prompted event is locked (progression), None = open.
    pub prompt_locked: Option<String>,
    /// Event objects root and its object parents (distance-culled).
    objects: Option<(Entity, Vec<(Entity, Vec3, bool)>)>,
    /// Event-object colliders: waiting for the prop colliders to load, and the ids added (removed after the race).
    collision_pending: Vec<(u16, Mat4, Vec3, Vec3)>,
    collision_ids: Vec<u32>,
    /// Colliders of finished races still to remove (race_objects; a new race may start before they are gone).
    collision_stale: Vec<u32>,
    /// Pending reset to the last gate (applied next frame, after free roam's upright reset).
    reset_next: bool,
    /// Message for the HUD (e.g. "Reset to track"), with time left.
    pub flash: Option<(String, f32)>,
    /// Post-race screens (race/postrace.rs): the page shown in Results, and how many there are (A / Enter steps
    /// through them; the last one returns to free roam). 1 = the old single results panel.
    pub results_page: u8,
    pub results_pages: u8,
}

impl Default for RaceState {
    fn default() -> Self {
        Self {
            phase: RacePhase::Idle,
            race: None,
            racers: Vec::new(),
            clock_s: 0.0,
            laps: Vec::new(),
            lap_start_s: 0.0,
            last_split: None,
            splits: Vec::new(),
            wrong_way_s: 0.0,
            off_track_s: 0.0,
            stuck_s: 0.0,
            list_open: false,
            list_cursor: 0,
            prompt: None,
            prompt_locked: None,
            objects: None,
            collision_pending: Vec::new(),
            collision_ids: Vec::new(),
            collision_stale: Vec::new(),
            reset_next: false,
            flash: None,
            results_page: 0,
            results_pages: 1,
        }
    }
}

impl RaceState {
    /// Seconds the player has been off the route or stuck (the reset hint shows after a while).
    pub fn lost_s(&self) -> f32 {
        self.off_track_s.max(self.stuck_s).max(self.wrong_way_s)
    }

    /// Whether the race drives the satnav / objective line (grid to finish; not on the results screen).
    pub fn owns_nav(&self) -> bool {
        self.race.is_some() && !matches!(self.phase, RacePhase::Idle | RacePhase::Results)
    }
}

/// Seconds on the grid before the countdown (scenery streams in after the teleport), and per countdown step ("_321Type Full": 3, 2, 1, GO).
const GRID_SETTLE_S: f32 = 3.0;
/// L1: with loading covers on, the race card covers the streaming (the grid timer waits under it), so the grid only
/// needs a moment before the countdown.
pub const GRID_SETTLE_LOADER_S: f32 = 0.75;
const COUNTDOWN_S: f32 = 3.0;
/// After the player finishes, results show after this long.
const RESULTS_AFTER_S: f32 = 3.0;
/// Festival marker TriggerZone radius (career_event_activations.xml: radius 25, maxMPH 150) and the start speed.
const MARKER_RADIUS: f32 = 25.0;
const MARKER_MAX_SPEED: f32 = 150.0 * 0.44704;
/// Markers are drawn within this distance.
const MARKER_DRAW: f32 = 400.0;
/// HandOfGodReset (GameTunableSettings.ini, VERIFIED values): off track by OffTrackAmountMeters1 = 15 m for
/// OffTrackTimeBeforeReset1 = 3 s, or OffTrackAmountMeters0 = 5 m for 6 s; no progress (< 5 mph) for 3 s.
/// "Off track" here = distance from the race's road path (the setup's A* path through the game's waypoints, which
/// can deviate) and from the line between the last and next gate: the 5 m rule is not applied, the 15 m rule only
/// together with being stuck (< 5 mph for 3 s), plus a lost rule (60 m for 6 s).
const OFF_TRACK_FAR_M: f32 = 15.0;
/// Far off any route (60 m for 6 s; the 15 m / 3 s rule alone would misfire where the road path is approximate).
const OFF_TRACK_LOST_M: f32 = 60.0;
const OFF_TRACK_LOST_S: f32 = 6.0;
const STUCK_MPH: f32 = 5.0;
const STUCK_S: f32 = 3.0;
/// Event objects are shown within this distance of the player.
const OBJECT_DRAW: f32 = 350.0;
/// Event-wall triangles within this distance of a barrier object's centre are solid during its race.
const BARRIER_WALL_R: f32 = 2.5;

/// `FH1_RACE_GHOST_SOLIDS=1`: the old event-object colliders, a box for every object with known bounds even when its
/// template isn't installed (no mesh: an invisible wall).
fn ghost_solids() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_GHOST_SOLIDS").is_ok_and(|v| v == "1"))
}

pub fn races_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACES").map_or(true, |v| v != "0"))
}

/// The player car's pose and speed.
fn player(cars: &Query<&mut Car>) -> Option<(Vec3, Vec3, Quat)> {
    cars.iter().next().map(|c| (c.0.position, c.0.velocity, c.0.rotation))
}

/// Gate crossing: the segment a -> b passes the gate's line (within its half width + 5 m) going forward, or ends
/// inside its half-width circle.
fn crossed(g: &Gate, a: Vec3, b: Vec3) -> bool {
    let c = Vec2::new(g.centre.x, g.centre.z);
    let (pa, pb) = (Vec2::new(a.x, a.z) - c, Vec2::new(b.x, b.z) - c);
    if pb.length() < g.half_width.min(20.0) {
        return true;
    }
    let (da, db) = (pa.dot(g.forward), pb.dot(g.forward));
    if !(da < 0.0 && db >= 0.0) {
        return false;
    }
    let t = da / (da - db);
    let p = pa + (pb - pa) * t;
    p.perp_dot(g.forward).abs() <= g.half_width + 5.0
}

/// Distance (2D) from `p` to the race path, and the path tangent there.
fn path_info(path: &[Vec3], p: Vec3) -> Option<(f32, Vec2)> {
    let q = Vec2::new(p.x, p.z);
    let mut best: Option<(f32, Vec2)> = None;
    for s in path.windows(2) {
        let (a, b) = (Vec2::new(s[0].x, s[0].z), Vec2::new(s[1].x, s[1].z));
        let ab = b - a;
        let l2 = ab.length_squared();
        if l2 < 1e-4 {
            continue;
        }
        let t = ((q - a).dot(ab) / l2).clamp(0.0, 1.0);
        let d = (a + ab * t).distance(q);
        if best.is_none_or(|x| d < x.0) {
            best = Some((d, ab / l2.sqrt()));
        }
    }
    best
}

/// Gate `i` of lap-relative progress `done` (gates repeat per lap).
fn gate(def: &RaceDef, done: u32) -> &Gate {
    &def.gates[done as usize % def.gates.len()]
}

fn total_gates(def: &RaceDef) -> u32 {
    def.gates.len() as u32 * def.laps
}

/// `FH1_RACE_AUTOFINISH=<seconds after GO>` (dev hook).
fn autofinish_s() -> Option<f32> {
    static AT: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *AT.get_or_init(|| std::env::var("FH1_RACE_AUTOFINISH").ok().and_then(|v| v.parse().ok()))
}

/// Ground point below `p` (or `p`).
fn ground(track: &crate::track::Track, p: Vec3) -> Vec3 {
    track.ground.ray(p + Vec3::Y * 10.0, Vec3::NEG_Y, 40.0).map_or(p, |h| h.point)
}

#[allow(clippy::too_many_arguments)]
fn start_race(
    i: usize,
    events: &Events,
    rs: &mut RaceState,
    cars: &mut Query<&mut Car>,
    track: &crate::track::Track,
    commands: &mut Commands,
    scenery: Option<&crate::scenery::Scenery>,
    ai: &mut ai_link::AiLink,
    profile: Option<&crate::progression::profile::ProfileData>,
) {
    let def = &events.races[i];
    // The AI field (race/field.rs): the game's entrants, matched to the player's class / wristband / rank.
    let player_car = cars.iter().next().map(|c| c.0.data.media_name.clone()).unwrap_or_default();
    let (tier, rank) = profile.map_or((0, 250), |p| (events.career.tier(p.xp), events.career.rank(p.fame)));
    let ctx = field::FieldCtx { career: &events.career, player_car: &player_car, tier, rank, difficulty: ai.difficulty(), assets: Some(events.assets.as_path()) };
    let want = (def.drivers as usize).min(def.grid.len().saturating_sub(1));
    let (entries, note) = field::build(def, &ctx, want);
    // Grid: the player starts last (CareerRaceModes GridOrderingType PIWithPlayerLast); solo = pole.
    let ai_count = if ai.available() { entries.len().min(want) } else { 0 };
    let (slot_pos, slot_yaw) = def.grid[ai_count];
    for mut car in cars.iter_mut() {
        car.0.place(ground(track, slot_pos), slot_yaw);
    }
    let you = "You".to_owned();
    rs.racers = vec![Racer { entity: None, is_player: true, name: you.clone(), driver: you, car: field::car_label(&events.career, &player_car), class_pi: crate::progression::player_class(&events.career, &player_car), gates_done: def.start_gate, lap: 1, to_next: 0.0, position: 1, finished_s: None, last_pos: None }];
    for (slot, e) in entries.iter().take(ai_count).enumerate() {
        let (p, yaw) = def.grid[slot];
        ai.spawn(slot as u32 + 1, e, (ground(track, p), yaw), def);
        rs.racers.push(Racer {
            entity: None,
            is_player: false,
            name: if e.name == e.car_label { e.car_label.clone() } else { format!("{} · {}", e.name, e.car_label) },
            driver: e.name.clone(),
            car: e.car_label.clone(),
            class_pi: crate::progression::player_class(&events.career, &e.car),
            gates_done: def.start_gate,
            lap: 1,
            to_next: 0.0,
            position: 1,
            finished_s: None,
            last_pos: None,
        });
    }
    // The event's walls (collision) and its event-only objects. The event walls are the event triangles under the race's
    // barrier objects (`FH1_RACE_BARRIER_TRIS=0`: the setup's inferred route bits, which also switch on walls elsewhere).
    let tris_on = ai_link::barrier_tris_on();
    ai_link::set_barriers(track, if tris_on { 0 } else { def.barrier_bits });
    let mut shown: Vec<(u16, Mat4)> = Vec::new();
    if let Some(sc) = scenery {
        let root = commands.spawn((Transform::IDENTITY, Visibility::Inherited, crate::ui::world_load::WorldEntity)).id();
        let mut list = Vec::new();
        let mut missing = 0;
        for (t, m) in &def.objects {
            let parent = commands.spawn((Transform::IDENTITY, Visibility::Hidden, ChildOf(root))).id();
            if sc.spawn_template(commands, *t, Transform::from_matrix(*m), parent) {
                list.push((parent, m.w_axis.truncate(), false));
                shown.push((*t, *m));
            } else {
                missing += 1;
                commands.entity(parent).despawn();
            }
        }
        info!("race: {} event objects ({missing} templates not installed), barrier bits {:#06x}", list.len(), def.barrier_bits);
        // Solid colliders for them (concrete / metal barriers, signs); cone-sized objects stay ghosts (solid cones
        // would stop the car dead). Only objects that are drawn: a template the scenery group didn't install has no mesh,
        // and its box was an invisible wall (FH1_RACE_GHOST_SOLIDS=1: old, boxes for every object).
        let solids: &[(u16, Mat4)] = if ghost_solids() { &def.objects } else { &shown };
        rs.collision_pending = solids
            .iter()
            .filter_map(|(t, m)| {
                let (lo, hi) = *events.object_bounds.get(t)?;
                let size = (hi - lo) * m.to_scale_rotation_translation().0.abs();
                (size.x.max(size.z) >= 1.0 || size.y >= 1.2).then_some((*t, *m, lo, hi))
            })
            .collect();
        rs.objects = Some((root, list));
    }
    if tris_on {
        // Event walls only where this race shows a barrier: the barrier objects' event triangles (route bits 0-14), within
        // BARRIER_WALL_R of the object's centre (every barrier object of every race stands within 2.1 m of an event wall,
        // 99th percentile; the walls without a barrier of this race nearby belong to other events).
        if let Some(w) = &track.world {
            let probes = shown.iter().filter(|(t, _)| events.barrier_templates.contains(t)).map(|(t, m)| {
                let c = events.object_bounds.get(t).map_or(Vec3::ZERO, |(lo, hi)| (*lo + *hi) * 0.5);
                m.transform_point3(c)
            });
            let mut tris = w.event_tris_near(probes, BARRIER_WALL_R);
            let all = tris.len();
            w.drop_crossing_tris(&mut tris, &def.path);
            info!(
                "race: {} event wall triangles under {} barrier objects ({} cut the road path: dropped)",
                tris.len(),
                shown.iter().filter(|(t, _)| events.barrier_templates.contains(t)).count(),
                all - tris.len()
            );
            ai_link::set_barrier_tris(track, tris);
        }
    }
    rs.race = Some(i);
    rs.phase = RacePhase::Grid { left_s: if crate::ui::loading::enabled() { GRID_SETTLE_LOADER_S } else { GRID_SETTLE_S } };
    rs.clock_s = 0.0;
    rs.laps.clear();
    rs.splits = vec![Vec::new()];
    rs.lap_start_s = 0.0;
    rs.last_split = None;
    rs.wrong_way_s = 0.0;
    rs.off_track_s = 0.0;
    rs.stuck_s = 0.0;
    rs.list_open = false;
    rs.prompt = None;
    rs.prompt_locked = None;
    rs.flash = note.filter(|_| ai_count > 0).map(|n| (n, 5.0));
    info!("race: start {} ({}, {} laps, {} gates, {} AI)", def.name, def.kind, def.laps, def.gates.len(), ai_count);
}

fn end_race(events: &Events, rs: &mut RaceState, cars: &mut Query<&mut Car>, track: &crate::track::Track, commands: &mut Commands, ai: &mut ai_link::AiLink, to_post: bool) {
    if let Some(def) = rs.race.and_then(|i| events.races.get(i)) {
        if let (true, Some((p, yaw))) = (to_post, def.post_race) {
            // The original leaves you where you finished; only a car that is off the world (no ground below) goes to post_race.
            let legacy = std::env::var("FH1_RACE_POST_TELEPORT").is_ok_and(|v| v == "1");
            for mut car in cars.iter_mut() {
                let at = car.0.position;
                let on_ground = track.ground.ray(at + Vec3::Y * 10.0, Vec3::NEG_Y, 60.0).is_some();
                if legacy || !on_ground || !at.is_finite() {
                    car.0.place(ground(track, p), yaw);
                }
            }
        }
    }
    ai_link::set_barriers(track, 0);
    ai_link::set_barrier_tris(track, Default::default());
    ai.despawn_all();
    if let Some((root, _)) = rs.objects.take() {
        commands.entity(root).despawn();
    }
    let list = (rs.list_open, rs.list_cursor);
    let mut stale = std::mem::take(&mut rs.collision_stale);
    stale.extend(std::mem::take(&mut rs.collision_ids));
    *rs = RaceState { list_open: list.0, list_cursor: list.1, collision_stale: stale, ..default() };
}

/// Input and the race state machine.
#[allow(clippy::too_many_arguments)]
pub fn race_update(
    events: Res<Events>,
    mut rs: ResMut<RaceState>,
    mut cars: Query<&mut Car>,
    track: Res<crate::track::Track>,
    scenery: Option<Res<crate::scenery::Scenery>>,
    keys: Res<ButtonInput<KeyCode>>,
    pads: Query<&Gamepad>,
    time: Res<Time>,
    mut commands: Commands,
    mut ai: ai_link::AiLink,
    mut link: crate::progression::RaceLink,
    mut auto_step: Local<f32>,
) {
    if !races_on() || events.races.is_empty() || track.id != "colorado" {
        return;
    }
    let starts = link.take_starts();
    // Race clock and timers: real frame time (a long loading frame still counts; capped against hitches > 0.25 s).
    let dt = time.delta_secs().min(0.25);
    let pad = |b: GamepadButton| pads.iter().any(|p| p.just_pressed(b));
    let confirm = keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter) || pad(GamepadButton::South);
    let reset = keys.just_pressed(KeyCode::KeyR) || pad(GamepadButton::North);
    if let Some((_, t)) = rs.flash.as_mut() {
        *t -= dt;
        if *t <= 0.0 {
            rs.flash = None;
        }
    }
    let Some((pos, vel, rot)) = player(&cars) else { return };
    let profile = link.data().cloned();
    let prof = profile.as_ref();
    let locked = |i: usize| prof.and_then(|p| crate::progression::lock_reason(&events.races[i], p, &events.career));

    // FH1_RACE=<HorizonEventID or index>: start that race once, a few seconds in (testing).
    if rs.phase == RacePhase::Idle && time.elapsed_secs() > 3.0 {
        static AUTO: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(want) = AUTO.get_or_init(|| std::env::var("FH1_RACE").ok()) {
            if !DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let i = events.races.iter().position(|r| r.horizon_id.eq_ignore_ascii_case(want)).or_else(|| want.parse().ok().filter(|&i: &usize| i < events.races.len()));
                match i {
                    Some(i) => start_race(i, &events, &mut rs, &mut cars, &track, &mut commands, scenery.as_deref(), &mut ai, prof),
                    None => warn!("FH1_RACE={want}: no such race"),
                }
                return;
            }
        }
    }

    // Map / career screen: start an event (teleports to its grid). Locked ones are refused with the reason.
    if rs.phase == RacePhase::Idle {
        for id in starts {
            let Some(i) = events.races.iter().position(|r| r.horizon_id.eq_ignore_ascii_case(&id)) else { continue };
            match locked(i) {
                Some(why) => rs.flash = Some((format!("{}: locked ({why})", events.races[i].name), 3.0)),
                None => {
                    start_race(i, &events, &mut rs, &mut cars, &track, &mut commands, scenery.as_deref(), &mut ai, prof);
                    return;
                }
            }
        }
    }
    // F6: the career screen (progression/screen.rs) with progression on, else the plain event list (up / down,
    // Enter / A starts, F6 / Backspace closes).
    if keys.just_pressed(KeyCode::F6) {
        if crate::progression::enabled() && link.has_screen() {
            if rs.phase == RacePhase::Idle {
                link.toggle_career();
            }
        } else {
            rs.list_open = !rs.list_open;
        }
    }
    if link.career_open() {
        return;
    }
    if rs.list_open && rs.phase == RacePhase::Idle {
        let n = events.races.len();
        if keys.just_pressed(KeyCode::ArrowDown) || keys.just_pressed(KeyCode::PageDown) {
            rs.list_cursor = (rs.list_cursor + if keys.just_pressed(KeyCode::PageDown) { 10 } else { 1 }).min(n - 1);
        }
        if keys.just_pressed(KeyCode::ArrowUp) || keys.just_pressed(KeyCode::PageUp) {
            rs.list_cursor = rs.list_cursor.saturating_sub(if keys.just_pressed(KeyCode::PageUp) { 10 } else { 1 });
        }
        if keys.just_pressed(KeyCode::Backspace) {
            rs.list_open = false;
        }
        if confirm {
            let i = rs.list_cursor;
            start_race(i, &events, &mut rs, &mut cars, &track, &mut commands, scenery.as_deref(), &mut ai, prof);
        }
        return;
    }

    match rs.phase {
        RacePhase::Idle => {
            // Marker TriggerZones: stop inside one and press A / Enter.
            rs.prompt = events
                .races
                .iter()
                .enumerate()
                .filter(|(_, r)| Vec2::new(r.marker.0.x - pos.x, r.marker.0.z - pos.z).length() < MARKER_RADIUS && (r.marker.0.y - pos.y).abs() < 15.0)
                .filter(|(i, r)| {
                    !states::filter_on()
                        || states::marker_visible(locked(*i).is_none(), prof.is_some_and(|p| p.events.get(&r.horizon_id).is_some_and(|e| e.best_place > 0)))
                })
                .min_by(|a, b| a.1.marker.0.distance(pos).total_cmp(&b.1.marker.0.distance(pos)))
                .map(|(i, _)| i)
                .filter(|_| vel.length() < MARKER_MAX_SPEED);
            rs.prompt_locked = rs.prompt.and_then(locked);
            if let (Some(i), true) = (rs.prompt, confirm && vel.length() < 3.0) {
                match rs.prompt_locked.clone() {
                    Some(why) => rs.flash = Some((format!("Locked: {why}"), 3.0)),
                    None => start_race(i, &events, &mut rs, &mut cars, &track, &mut commands, scenery.as_deref(), &mut ai, prof),
                }
            }
        }
        RacePhase::Grid { left_s } | RacePhase::Countdown { left_s } => {
            if let Some(g) = rs.race.map(|i| *gate(&events.races[i], events.races[i].start_gate)) {
                rs.racers[0].to_next = Vec2::new(g.centre.x - pos.x, g.centre.z - pos.z).length();
            }
            // Held on the grid: no creeping (the game holds the cars until GO).
            if let Some(i) = rs.race {
                let slot = events.races[i].grid[(rs.racers.len() - 1).min(events.races[i].grid.len() - 1)];
                for mut car in cars.iter_mut() {
                    let v = &mut car.0;
                    if Vec2::new(v.position.x - slot.0.x, v.position.z - slot.0.z).length() > 0.3 || v.velocity.length() > 0.5 {
                        v.velocity = Vec3::new(0.0, v.velocity.y.min(0.0), 0.0);
                        v.angular_velocity = Vec3::ZERO;
                        let keep_y = v.position.y;
                        v.position = Vec3::new(slot.0.x, keep_y, slot.0.z);
                        v.prev_position = v.position;
                    }
                }
            }
            // Grid order until GO: AI in slots 1.., the player behind them.
            let n = rs.racers.len() as u32;
            for (k, r) in rs.racers.iter_mut().enumerate() {
                r.position = if r.is_player { n } else { k as u32 };
            }
            let left = left_s - dt;
            rs.phase = match rs.phase {
                RacePhase::Grid { .. } if left <= 0.0 => RacePhase::Countdown { left_s: COUNTDOWN_S },
                RacePhase::Grid { .. } => RacePhase::Grid { left_s: left },
                _ if left <= 0.0 => {
                    rs.clock_s = 0.0;
                    RacePhase::Racing
                }
                _ => RacePhase::Countdown { left_s: left },
            };
            if keys.just_pressed(KeyCode::F7) {
                end_race(&events, &mut rs, &mut cars, &track, &mut commands, &mut ai, false);
            }
        }
        RacePhase::Racing | RacePhase::Finished { .. } => {
            let i = rs.race.unwrap();
            let def = &events.races[i];
            rs.clock_s += dt;
            let total = total_gates(def);
            // FH1_RACE_AUTOFINISH=<s>: finish the player where they are, s seconds after GO (testing the post-race pages).
            if let Some(at) = autofinish_s() {
                if rs.racers[0].finished_s.is_none() && rs.clock_s >= at {
                    let clock = rs.clock_s;
                    rs.racers[0].gates_done = total;
                    rs.racers[0].finished_s = Some(clock);
                    rs.phase = RacePhase::Finished { left_s: RESULTS_AFTER_S };
                    let place = 1 + rs.racers.iter().skip(1).filter(|r| r.finished_s.is_some()).count() as u32;
                    link.finished(crate::progression::RaceFinished { race: i, place: Some(place), time_s: Some(clock), field: rs.racers.len() as u32 });
                    info!("race: FH1_RACE_AUTOFINISH at {clock:.1} s, place {place}");
                }
            }
            // Player progress.
            let clock = rs.clock_s;
            let last = rs.racers[0].last_pos.replace(pos);
            if let (Some(a), None) = (last, rs.racers[0].finished_s) {
                let g = *gate(def, rs.racers[0].gates_done);
                if crossed(&g, a, pos) {
                    let done = rs.racers[0].gates_done + 1;
                    rs.racers[0].gates_done = done;
                    info!("race: gate {done}/{total} at {clock:.2} s");
                    let per_lap = def.gates.len() as u32;
                    let lap_gate = (done - 1) % per_lap;
                    let split = clock - rs.lap_start_s;
                    let best = rs.splits.iter().rev().skip(1).filter_map(|l| l.get(lap_gate as usize).copied().flatten()).reduce(f32::min);
                    if let Some(l) = rs.splits.last_mut() {
                        if l.len() <= lap_gate as usize {
                            l.resize(lap_gate as usize + 1, None);
                        }
                        l[lap_gate as usize] = Some(split);
                    }
                    rs.last_split = Some((split, best));
                    if done % per_lap == 0 {
                        rs.laps.push(split);
                        rs.lap_start_s = clock;
                        rs.splits.push(Vec::new());
                    }
                    if done >= total {
                        rs.racers[0].finished_s = Some(clock);
                        rs.phase = RacePhase::Finished { left_s: RESULTS_AFTER_S };
                        info!("race: finished {} in {:.2} s", def.name, clock);
                        // Place: everyone already finished is ahead (finish times are final).
                        let place = 1 + rs.racers.iter().skip(1).filter(|r| r.finished_s.is_some_and(|t| t <= clock)).count() as u32;
                        link.finished(crate::progression::RaceFinished { race: i, place: Some(place), time_s: Some(clock), field: rs.racers.len() as u32 });
                    } else {
                        rs.racers[0].lap = done / per_lap + 1;
                    }
                }
            }
            let next = *gate(def, rs.racers[0].gates_done.min(total.saturating_sub(1)));
            rs.racers[0].to_next = Vec2::new(next.centre.x - pos.x, next.centre.z - pos.z).length();
            // AI progress (R2's cars).
            ai.update_racers(&mut rs.racers, def, clock);
            // Positions: finished first (by time), then gates done, then distance to the next gate.
            let mut order: Vec<usize> = (0..rs.racers.len()).collect();
            order.sort_by(|&a, &b| {
                let (ra, rb) = (&rs.racers[a], &rs.racers[b]);
                match (ra.finished_s, rb.finished_s) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    _ => rb.gates_done.cmp(&ra.gates_done).then(ra.to_next.total_cmp(&rb.to_next)),
                }
            });
            for (p, &k) in order.iter().enumerate() {
                rs.racers[k].position = p as u32 + 1;
            }
            // Wrong way: driving away from the next gate for 2 s (the gates are the game's own route points; the A*
            // road path between them can detour, so it isn't used for direction).
            let speed = vel.length();
            let to_gate = Vec2::new(next.centre.x - pos.x, next.centre.z - pos.z).normalize_or_zero();
            let along = Vec2::new(vel.x, vel.z).dot(to_gate);
            rs.wrong_way_s = if speed > 5.0 && along < -0.5 * speed && rs.racers[0].to_next > 40.0 { rs.wrong_way_s + dt } else { 0.0 };
            // Off track = away from both the road path and the chain of gates.
            let prev_centre = if rs.racers[0].gates_done <= def.start_gate { def.grid[0].0 } else { gate(def, rs.racers[0].gates_done - 1).centre };
            let chain = path_info(&[prev_centre, next.centre], pos).map_or(f32::INFINITY, |x| x.0);
            let off = path_info(&def.path, pos).map_or(f32::INFINITY, |x| x.0).min(chain);
            rs.off_track_s = if off > OFF_TRACK_LOST_M { rs.off_track_s + dt } else { 0.0 };
            rs.stuck_s = if speed < STUCK_MPH * 0.44704 && off > OFF_TRACK_FAR_M { rs.stuck_s + dt } else { 0.0 };
            let _ = rot;
            let auto = matches!(rs.phase, RacePhase::Racing) && (rs.off_track_s > OFF_TRACK_LOST_S || rs.stuck_s > STUCK_S);
            if rs.reset_next || (auto && rs.racers[0].finished_s.is_none()) {
                // Reset to the last gate passed (the grid before the first), facing the next one.
                rs.reset_next = false;
                let done = rs.racers[0].gates_done;
                let (p, yaw) = if done <= def.start_gate {
                    def.grid[(rs.racers.len() - 1).min(def.grid.len() - 1)]
                } else {
                    let g = gate(def, done - 1);
                    let n = gate(def, done);
                    let d = Vec2::new(n.centre.x - g.centre.x, n.centre.z - g.centre.z).normalize_or(g.forward);
                    (g.centre, (-d.x).atan2(-d.y))
                };
                for mut car in cars.iter_mut() {
                    car.0.place(ground(&track, p), yaw);
                }
                rs.racers[0].last_pos = None;
                rs.off_track_s = 0.0;
                rs.stuck_s = 0.0;
                rs.flash = Some(("Reset to track".into(), 2.0));
            } else if reset {
                // Free roam's reset (main.rs) rights the car this frame; put it on the track next frame.
                rs.reset_next = true;
            }
            if let RacePhase::Finished { left_s } = rs.phase {
                let left = left_s - dt;
                rs.phase = if left <= 0.0 || confirm { RacePhase::Results } else { RacePhase::Finished { left_s: left } };
            }
            if keys.just_pressed(KeyCode::F7) {
                end_race(&events, &mut rs, &mut cars, &track, &mut commands, &mut ai, false);
            }
        }
        RacePhase::Results => {
            // FH1_RACE_AUTOFINISH: step through the post-race pages every 3 s.
            let confirm = confirm || {
                *auto_step += dt;
                let page_s = std::env::var("FH1_RACE_AUTOFINISH_PAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0f32);
                let step = autofinish_s().is_some() && *auto_step > page_s;
                if step {
                    *auto_step = 0.0;
                }
                step
            };
            // AI keep driving while the results show; anyone unfinished is placed by progress.
            if let Some(def) = rs.race.map(|i| &events.races[i]) {
                let clock = rs.clock_s + dt;
                rs.clock_s = clock;
                ai.update_racers(&mut rs.racers, def, clock);
            }
            if confirm {
                rs.results_page += 1;
                if rs.results_page >= rs.results_pages.max(1) {
                    end_race(&events, &mut rs, &mut cars, &track, &mut commands, &mut ai, true);
                }
            }
        }
    }
    ai.set_control(rs.phase, rs.race.map(|i| events.races[i].route_file.clone()), &rs.racers);
}

/// Event objects: show those within `OBJECT_DRAW` of the player (checked a few times a second).
pub fn race_objects(mut rs: ResMut<RaceState>, events: Res<Events>, cars: Query<&Car>, mut tick: Local<f32>, time: Res<Time>, mut commands: Commands, props: Option<ResMut<crate::smash::PropCollision>>) {
    // Colliders: added once the prop colliders are loaded; removed after the race (end_race leaves the ids).
    if let Some(mut props) = props {
        if !rs.collision_stale.is_empty() {
            let ids = std::mem::take(&mut rs.collision_stale);
            props.remove(&ids);
        }
        if props.ready() && props.edge_suppress_race() != rs.race {
            match rs.race {
                Some(i) => props.set_edge_suppress(Some(i), &events.races[i].path),
                None => props.set_edge_suppress(None, &[]),
            }
        }
        if rs.race.is_some() && !rs.collision_pending.is_empty() && props.ready() {
            let objs = std::mem::take(&mut rs.collision_pending);
            let keep_clear: Vec<Vec3> = cars.iter().map(|c| c.0.position).chain(events.races[rs.race.unwrap_or(0)].grid.iter().map(|g| g.0)).collect();
            let ids = props.add_solid_boxes(&objs, &keep_clear);
            info!("race: {} event-object colliders", ids.len());
            rs.collision_ids.extend(ids);
        }
    }
    *tick += time.delta_secs();
    if *tick < 0.25 {
        return;
    }
    *tick = 0.0;
    let Some(p) = cars.iter().next().map(|c| c.0.position) else { return };
    let Some((_, list)) = rs.objects.as_mut() else { return };
    for (e, at, shown) in list.iter_mut() {
        let on = at.distance(p) < OBJECT_DRAW;
        if on != *shown {
            *shown = on;
            commands.entity(*e).insert(if on { Visibility::Inherited } else { Visibility::Hidden });
        }
    }
}

/// World markers (gizmos): event markers in free roam; during a race the next gate (and the one after) and
/// the route ahead.
pub fn race_markers(
    events: Res<Events>,
    rs: Res<RaceState>,
    cars: Query<&Car>,
    track: Res<crate::track::Track>,
    cat: Option<Res<crate::progression::EventCatalog>>,
    mut gizmos: Gizmos,
) {
    // The remaster markers (race/visuals.rs) replace these lines unless FH1_RACE_FX=0.
    if !races_on() || events.races.is_empty() || track.id != "colorado" || visuals::enabled() {
        return;
    }
    let Some(pos) = cars.iter().next().map(|c| c.0.position) else { return };
    let ring = |g: &mut Gizmos, c: Vec3, r: f32, col: Color| {
        for k in 0..3 {
            g.circle(Isometry3d::new(c + Vec3::Y * (0.3 + k as f32 * 1.2), Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)), r, col);
        }
    };
    match (rs.phase, rs.race) {
        (RacePhase::Idle, _) => {
            for (i, r) in events.races.iter().enumerate() {
                let d = r.marker.0.distance(pos);
                if d > MARKER_DRAW {
                    continue;
                }
                // Career state (progression.rs): locked = grey, done = dim, the recommended next event = its wristband colour.
                let info = cat.as_ref().and_then(|c| c.events.get(i).filter(|e| e.race == i));
                if !states::state_visible(info.map(|e| e.state)) {
                    continue;
                }
                let col = if Some(i) == rs.prompt {
                    Color::srgb(1.0, 0.85, 0.1)
                } else if info.is_some_and(|e| e.state == crate::progression::EventState::Locked) {
                    Color::srgba(0.6, 0.6, 0.6, 0.5)
                } else if info.is_some_and(|e| e.recommended) {
                    crate::progression::tier_color(info.map_or(0, |e| e.tier))
                } else if r.kind.starts_with("Street") {
                    Color::srgb(0.2, 0.7, 1.0)
                } else {
                    Color::srgb(1.0, 0.3, 0.6)
                };
                let col = if info.is_some_and(|e| matches!(e.state, crate::progression::EventState::Completed { .. })) && Some(i) != rs.prompt { col.with_alpha(0.45) } else { col };
                ring(&mut gizmos, r.marker.0, MARKER_RADIUS * 0.4, col);
                gizmos.line(r.marker.0, r.marker.0 + Vec3::Y * 25.0, col);
            }
        }
        (_, Some(i)) => {
            let def = &events.races[i];
            let racer = &rs.racers[0];
            if racer.finished_s.is_some() {
                return;
            }
            let total = total_gates(def);
            for k in 0..2u32 {
                let done = racer.gates_done + k;
                if done >= total {
                    break;
                }
                let g = gate(def, done);
                let last = done + 1 == total;
                let col = if last { Color::srgb(1.0, 1.0, 1.0) } else if k == 0 { Color::srgb(1.0, 0.85, 0.1) } else { Color::srgba(1.0, 0.85, 0.1, 0.35) };
                let side = Vec3::new(-g.forward.y, 0.0, g.forward.x) * g.half_width.min(18.0);
                let base = g.centre;
                // Two posts and a banner across the road.
                for s in [-1.0, 1.0] {
                    let p = base + side * s;
                    gizmos.line(p, p + Vec3::Y * 8.0, col);
                }
                gizmos.line(base - side + Vec3::Y * 8.0, base + side + Vec3::Y * 8.0, col);
                gizmos.line(base - side + Vec3::Y * 6.5, base + side + Vec3::Y * 6.5, col);
                if last {
                    // Chequered-ish finish banner.
                    for s in 0..8 {
                        let t0 = -1.0 + s as f32 * 0.25;
                        gizmos.line(base + side * t0 + Vec3::Y * 6.5, base + side * (t0 + 0.25) + Vec3::Y * 8.0, col);
                    }
                }
            }
        }
        _ => {}
    }
}

/// During a race the satnav routes to the next gate and the objective line names the event and checkpoint. The
/// minimap's line goes on along the race's road path through the next [`NAV_AHEAD_GATES`] gates ([`lookahead`],
/// `SatNav::beyond`; user 2026-10-09: "should extend to the next few so you can see further ahead").
/// Contract with the world map (e4): while `RaceState::owns_nav()` the race owns `SatNav.target` / `Objective`; on
/// release both are cleared (None) and the map's waypoint system re-applies the waypoint or free roam's default.
/// `FH1_RACE_NAV_RESTORE=1`: the old behaviour (restore the values saved at the start).
pub fn race_nav(
    events: Res<Events>,
    rs: Res<RaceState>,
    mut satnav: Option<ResMut<crate::ui::minimap::SatNav>>,
    mut objective: Option<ResMut<crate::ui::notify::Objective>>,
    mut saved: Local<Option<(Option<Vec2>, Option<String>, Option<Vec2>)>>,
    mut ahead_for: Local<Option<(usize, u32)>>,
) {
    static RESTORE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let restore = *RESTORE.get_or_init(|| std::env::var("FH1_RACE_NAV_RESTORE").is_ok_and(|v| v == "1"));
    let active = rs.owns_nav();
    match (active, saved.is_some()) {
        (true, false) => {
            *saved = Some((satnav.as_ref().and_then(|s| s.target), objective.as_ref().and_then(|o| o.text.clone()), objective.as_ref().and_then(|o| o.target)));
        }
        (false, true) => {
            let (t, text, ot) = saved.take().unwrap();
            let (t, text, ot) = if restore { (t, text, ot) } else { (None, None, None) };
            if let Some(s) = satnav.as_mut() {
                s.target = t;
                s.beyond.clear();
                s.beyond_gen = s.beyond_gen.wrapping_add(1);
            }
            *ahead_for = None;
            if let Some(o) = objective.as_mut() {
                o.text = text;
                o.target = ot;
            }
            return;
        }
        _ => {}
    }
    if !active {
        return;
    }
    let (Some(def), Some(p)) = (rs.race.map(|i| &events.races[i]), rs.racers.first()) else { return };
    let total = total_gates(def);
    let g = gate(def, p.gates_done.min(total.saturating_sub(1)));
    if let Some(s) = satnav.as_mut() {
        let t = Some(Vec2::new(g.centre.x, g.centre.z));
        if s.target != t {
            s.target = t;
        }
        // The minimap line runs on past the next gate through the following ones (computed once per gate).
        let key = rs.race.map(|i| (i, p.gates_done));
        if *ahead_for != key {
            *ahead_for = key;
            s.beyond = lookahead(def, p.gates_done);
            s.beyond_gen = s.beyond_gen.wrapping_add(1);
        }
    }
    if let Some(o) = objective.as_mut() {
        // With FH1's race widgets on screen (race/anark_hud.rs) the objective line stays empty, as in the game: the
        // laps / checkpoints block sits where it would be.
        let text = if anark_hud::enabled() {
            None
        } else {
            Some(if p.gates_done + 1 >= total { format!("{}: finish", def.name) } else { format!("{}: next checkpoint", def.name) })
        };
        if o.text != text {
            o.text = text;
            o.target = None;
        }
    }
}

/// Gates past the next one the minimap's route line shows.
const NAV_AHEAD_GATES: u32 = 3;

/// The minimap route on past gate `done` (x, z): from that gate along the race's road path through the next
/// [`NAV_AHEAD_GATES`] gates, laps wrapping on circuits, stopping at the finish. Each gate is found on the path searching
/// forwards from the previous one (circuits wrap); a gate more than 60 m off the path is joined straight. Empty when
/// `done` is the finish.
fn lookahead(def: &RaceDef, done: u32) -> Vec<Vec2> {
    let total = total_gates(def);
    if done + 1 >= total || def.gates.is_empty() {
        return Vec::new();
    }
    let last = (done + NAV_AHEAD_GATES).min(total - 1);
    let p2 = |v: Vec3| Vec2::new(v.x, v.z);
    let path: Vec<Vec2> = def.path.iter().map(|&v| p2(v)).collect();
    let mut arc = vec![0.0f32];
    for w in path.windows(2) {
        arc.push(arc[arc.len() - 1] + w[0].distance(w[1]));
    }
    let len = arc[arc.len() - 1];
    let wrap = def.circuit && len > 1.0;
    // (distance off the path, arc length) of `q`, preferring the stretch just ahead of `from`.
    let project = |q: Vec2, from: Option<f32>| -> Option<(f32, f32)> {
        let mut best: Option<(f32, f32, f32)> = None;
        for (k, w) in path.windows(2).enumerate() {
            let ab = w[1] - w[0];
            let l2 = ab.length_squared();
            let t = if l2 < 1e-6 { 0.0 } else { ((q - w[0]).dot(ab) / l2).clamp(0.0, 1.0) };
            let d = (w[0] + ab * t).distance(q);
            let s = arc[k] + (arc[k + 1] - arc[k]) * t;
            let cost = match from {
                None => d,
                Some(f) if wrap => d + 0.05 * (s - f).rem_euclid(len),
                Some(f) => d + if s < f - 20.0 { 1e4 } else { 0.05 * (s - f).max(0.0) },
            };
            if best.is_none_or(|b| cost < b.2) {
                best = Some((d, s, cost));
            }
        }
        best.map(|b| (b.0, b.1))
    };
    let at = |s: f32| -> Vec2 {
        let k = arc.partition_point(|&a| a <= s).clamp(1, arc.len() - 1);
        let span = (arc[k] - arc[k - 1]).max(1e-6);
        path[k - 1].lerp(path[k], ((s - arc[k - 1]) / span).clamp(0.0, 1.0))
    };
    let mut out = vec![p2(gate(def, done).centre)];
    let mut s_prev = if path.len() >= 2 { project(out[0], None).filter(|x| x.0 < 60.0).map(|x| x.1) } else { None };
    for n in done + 1..=last {
        let q = p2(gate(def, n).centre);
        let hit = if path.len() >= 2 { project(q, s_prev).filter(|x| x.0 < 60.0).map(|x| x.1) } else { None };
        match (s_prev, hit) {
            (Some(a), Some(b)) if b >= a || wrap => {
                if b >= a {
                    out.extend((0..path.len()).filter(|&k| arc[k] > a && arc[k] < b).map(|k| path[k]));
                } else {
                    out.extend((0..path.len()).filter(|&k| arc[k] > a).map(|k| path[k]));
                    out.extend((0..path.len()).filter(|&k| arc[k] < b).map(|k| path[k]));
                }
                out.push(at(b));
            }
            _ => out.push(q),
        }
        s_prev = hit;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A bare race: `gates` (centre x, z; forward +X, half width `hw`), road `path` (x, z) at y = 0.
    pub(crate) fn test_def(gates: &[(f32, f32)], hw: f32, path: &[(f32, f32)], laps: u32, circuit: bool) -> RaceDef {
        RaceDef {
            horizon_id: String::new(),
            name: String::new(),
            kind: "Race".into(),
            mode: 2,
            laps,
            circuit,
            credits: 0,
            drivers: 1,
            track_id: 0,
            route_file: String::new(),
            length_m: 0.0,
            marker: (Vec3::ZERO, 0.0),
            grid: vec![(Vec3::ZERO, 0.0)],
            gates: gates.iter().map(|&(x, z)| Gate { centre: Vec3::new(x, 0.0, z), forward: Vec2::X, half_width: hw }).collect(),
            start_gate: 0,
            path: path.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect(),
            post_race: None,
            barrier_bits: 0,
            objects: Vec::new(),
            field: Vec::new(),
            ai: [(0, 0, 0); 4],
            event_id: 0,
            career_type: 0,
            level: 0,
            hub: 0,
            unlock_xp: 0,
            popularity_req: 0,
            event_order: 0,
            target_class: None,
            player_car: None,
            restriction: None,
            prize_car: None,
            recommended: Vec::new(),
            cannons: [Vec::new(), Vec::new()],
            start_cannon: None,
        }
    }

    #[test]
    fn minimap_lookahead_runs_through_the_next_gates() {
        // Point to point along x: gates every 100 m, path every 10 m.
        let path: Vec<(f32, f32)> = (0..=60).map(|k| (k as f32 * 10.0, 0.0)).collect();
        let def = test_def(&[(100.0, 5.0), (200.0, 5.0), (300.0, 5.0), (400.0, 5.0), (500.0, 5.0), (600.0, 0.0)], 30.0, &path, 1, false);
        let v = lookahead(&def, 0);
        // From gate 0 through gates 1-3 (stopping at gate 3's projection, 400 m), monotonic along the road.
        assert_eq!(v.first().copied(), Some(Vec2::new(100.0, 5.0)));
        assert!((v.last().unwrap().x - 400.0).abs() < 1e-3 && v.last().unwrap().y.abs() < 1e-3, "{v:?}");
        assert!(v.windows(2).skip(1).all(|w| w[1].x > w[0].x));
        // Near the end it stops at the finish; at the finish there is nothing.
        assert!((lookahead(&def, 4).last().unwrap().x - 600.0).abs() < 1e-3);
        assert!(lookahead(&def, 5).is_empty());
    }

    #[test]
    fn minimap_lookahead_wraps_laps_on_circuits() {
        // A 400 m square loop, gates at the middle of each side, 2 laps (8 gates).
        let mut path: Vec<(f32, f32)> = Vec::new();
        for k in 0..40 {
            let s = k as f32 * 40.0;
            path.push(match k / 10 {
                0 => (s, 0.0),
                1 => (400.0, s - 400.0),
                2 => (1200.0 - s, 400.0),
                _ => (0.0, 1600.0 - s),
            });
        }
        path.push((0.0, 0.0));
        let def = test_def(&[(200.0, 0.0), (400.0, 200.0), (200.0, 400.0), (0.0, 200.0)], 30.0, &path, 2, true);
        // From the last gate of lap 1 (done 3) on through gates 4-6 = lap 2's first three: across the loop's start.
        let v = lookahead(&def, 3);
        assert_eq!(v[0], Vec2::new(0.0, 200.0));
        assert!(v.contains(&Vec2::new(0.0, 0.0)) && v.contains(&Vec2::new(400.0, 0.0)));
        assert!(v.last().unwrap().distance(Vec2::new(200.0, 400.0)) < 1e-3, "{v:?}");
        // Last lap: stops at the finish (gate 7 = (0, 200)).
        assert!(lookahead(&def, 5).last().unwrap().distance(Vec2::new(0.0, 200.0)) < 1e-3);
    }
}