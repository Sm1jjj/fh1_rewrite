//! Airborne showcase challenges (gamedb events 101, 168, 172, 220, 222, 223, 246 = PLANE_RACE_001..007): the player's
//! car races an animated aircraft (P-51, helicopter, biplane, balloon) that flies its own animation clip.
//!
//! Data: `<assets>/story/airborne.json` = `{ "challenges": [ ... ] }` (the `airborne_challenges` of data/extracted/story/
//! challenges.json, from `airborne_challenges.xml`); absent = the feature is off (one log line) and the events run as
//! plain races. `FH1_AIRBORNE=0` = the old behaviour too. The aircraft objects are appended to the `anim` group by
//! fh1setup `airborne_objects.rs` (`anim/colorado/index.json`, names `ANIM_Showcase_Event_*` / `ANIM_Timed_Event_Test_Animation`).
//!
//! How it works (VERIFIED = checked against the data in this repo, INFERRED = common sense, the original is not known):
//! - The races are installed like every other (grid + 4..13 gates, `drivers` 0 = no AI). When such a race goes to
//!   Grid / Countdown / Racing, a session starts: the aircraft clip is loaded (Granny, `fh1_formats::granny`, VERIFIED to
//!   parse and to be 120..200 s long, longer than every finish time) and a *virtual racer* joins `RaceState::racers` at
//!   GO, so the HUD shows P1 / P2, the player's place at the line (1 of 2 or 2 of 2) and the credits per place work.
//! - Placement: the aircraft is drawn by `anim.rs` at the identity placement (hook: [`AirborneDrive`]), i.e. the clip's
//!   own space is the world's collision space (`OriginOffsetInMax` = (0,0,0) for all seven, VERIFIED). INFERRED: the
//!   original authored the paths in the map's Max scene. [`Curve::position`] gives the engine-space position; the log
//!   prints it at the cross-start / cross-finish clip times next to the race's grid / last gate so the guess can be
//!   checked in one run (the aircraft's route need not equal the road route: the P-51 flies ~15 km for a 6.7 km race).
//! - Timing: the clip starts at GO (balloon, `anim_start_on_countdown`: at the countdown). VERIFIED: every challenge has
//!   exactly gates + 2 timing checkpoints (cp[0] = clip start, cp[1] = `in_race_cross_start_s` = the start line, cp[k+2] =
//!   gate k, the last one = the finish line, within 0.05 s of `in_race_cross_finish_s`). So the aircraft's progress is
//!   read off the checkpoint *times* (piecewise linear between them) and compared with the player's gates done + the
//!   share of the current gate segment. `SplineDistToNext` is NOT used: it holds placeholders (100.0 / 1.0 /
//!   103.424 / 309.594 repeated) in events 168, 172, 220 and 223 and sums to 6 for the balloon (VERIFIED).
//! - Rubber band (INFERRED, formula unknown): `target = base * (1 + spring / 10000 * gap)` with `gap` = player progress -
//!   aircraft progress (fractions of the race) + `rubber_band_offset_ahead` + the interpolated `AdditionalRubberBandOffset`,
//!   clamped to the difficulty's `[rubber_band_min_mult, rubber_band_max_mult]`; the clip speed moves toward the target
//!   by at most `accel_rate` (speeding up) / `decel_rate` (slowing down) per second. Difficulty column = Options "AI
//!   difficulty" (Easy / Medium / Hard / Pro, as `race/field.rs`). Before GO and after the aircraft finishes the speed is
//!   `base_mult`.
//! - Result: the aircraft finishes when the clip reaches `in_race_cross_finish_s` (race time interpolated inside the
//!   frame); race.rs then ranks it like an AI racer (finished first by time, else gates done / distance).
//! - After the finish (INFERRED reading of `AfterFinish`): `stopanim` seconds later the clip freezes (-1 = never),
//!   `hidevehicle` seconds later the aircraft is hidden (-1 = never; the balloon: stops after 30 s, never hidden).
//! - Rumble near the player (`RumbleWhenNearPlayer`): linear from 0 at `distance_outer` to `strength` at `distance_inner`,
//!   sent as a Bevy `GamepadRumbleRequest`; also kept in [`Airborne::rumble`].
//! - Opponent name: `AirChallenges:IDS_Pilot*` from the EN string table ("Plane", "Chopper", "Biplane", "Balloon"); the
//!   `IDS_Vehicle*` strings are all "---", so the vehicle label comes from the object name.
//! - Not done: `check_progress.force_sync_dist` (-1 = off in all seven), the pre/post-race cutscene clips.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bevy::ecs::message::Messages;
use bevy::input::gamepad::{GamepadRumbleIntensity, GamepadRumbleRequest};
use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use fh1_formats::granny;
use serde::Deserialize;

use super::{gate, total_gates, Events, RaceDef, RacePhase, RaceState, Racer};
use crate::Car;

/// `FH1_AIRBORNE=0`: the airborne events run as plain races (no aircraft, no virtual opponent).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_AIRBORNE").map_or(true, |v| v != "0"))
}

/// `rubber_band_spring` is in unknown units (30000 for the aircraft, 300 for the balloon); the speed multiplier changes
/// by `spring / SPRING_UNIT` per unit of race fraction between the player and the aircraft (INFERRED). With 30000 an
/// aircraft one gate (of 9) behind or ahead of the player is at the clamp (+-30 %), so it hugs the player; the balloon
/// (300) is nearly un-banded.
const SPRING_UNIT: f32 = 10_000.0;
/// Seconds between rumble updates.
const RUMBLE_TICK_S: f32 = 0.1;

// ---- Data (airborne.json) -------------------------------------------------------------------------------------

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
struct ChallengeFile {
    challenges: Vec<Challenge>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Challenge {
    pub event_id: u32,
    pub ref_name: String,
    pub anim_object: String,
    pub anim_in_race: String,
    /// "true" / "false" (a string in the XML export).
    pub anim_start_on_countdown: serde_json::Value,
    pub in_race_cross_start_s: f32,
    pub in_race_cross_finish_s: f32,
    pub after_finish: AfterFinish,
    pub checkpoints: Vec<Checkpoint>,
    pub rubber_banding: RubberBanding,
    pub difficulty_anim_speeds: Difficulty,
    pub rumble_near_player: Rumble,
    pub opponent: Opponent,
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(default)]
pub struct AfterFinish {
    /// Seconds after the aircraft's finish when the clip stops (-1 = never).
    pub stopanim: f32,
    /// Seconds after the finish when the aircraft is hidden (-1 = never).
    pub hidevehicle: f32,
}

impl Default for AfterFinish {
    fn default() -> Self {
        Self { stopanim: -1.0, hidevehicle: -1.0 }
    }
}

#[derive(Deserialize, Default, Clone, Copy, Debug)]
#[serde(default)]
pub struct Checkpoint {
    #[serde(rename = "Time")]
    pub time: f32,
    #[serde(rename = "SplineDistToNext")]
    pub spline_dist_to_next: f32,
    #[serde(rename = "AdditionalRubberBandOffset")]
    pub offset: f32,
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(default)]
pub struct RubberBanding {
    pub rubber_band_spring: f32,
    pub accel_rate: f32,
    pub decel_rate: f32,
    pub rubber_band_offset_ahead: f32,
    pub rubber_band_checkpoint_adjustment: f32,
}

impl Default for RubberBanding {
    fn default() -> Self {
        Self { rubber_band_spring: 30000.0, accel_rate: 0.2, decel_rate: 0.1, rubber_band_offset_ahead: 0.0, rubber_band_checkpoint_adjustment: 0.0 }
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(default)]
pub struct DiffSpeed {
    pub base_mult: f32,
    pub rubber_band_min_mult: f32,
    pub rubber_band_max_mult: f32,
}

impl Default for DiffSpeed {
    fn default() -> Self {
        Self { base_mult: 1.0, rubber_band_min_mult: 0.8, rubber_band_max_mult: 1.2 }
    }
}

#[derive(Deserialize, Default, Clone, Copy, Debug)]
#[serde(default)]
pub struct Difficulty {
    #[serde(rename = "Easy")]
    pub easy: DiffSpeed,
    #[serde(rename = "Medium")]
    pub medium: DiffSpeed,
    #[serde(rename = "Hard")]
    pub hard: DiffSpeed,
    #[serde(rename = "Pro")]
    pub pro: DiffSpeed,
}

impl Difficulty {
    /// Options "AI difficulty" column: 0 Easy .. 3 Pro.
    pub fn pick(&self, index: usize) -> DiffSpeed {
        match index {
            0 => self.easy,
            1 => self.medium,
            2 => self.hard,
            _ => self.pro,
        }
    }
}

#[derive(Deserialize, Default, Clone, Copy, Debug)]
#[serde(default)]
pub struct Rumble {
    pub strength: f32,
    pub distance_outer: f32,
    pub distance_inner: f32,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Opponent {
    #[serde(rename = "LocStringPilotName")]
    pub pilot: String,
    #[serde(rename = "LocStringAirVehName")]
    pub vehicle: String,
}

impl Challenge {
    /// The in-race object's name without ".pgeo" (`anim_in_race`, else `anim_object`).
    pub fn object_stem(&self) -> String {
        let s = if self.anim_in_race.trim().is_empty() { self.anim_object.trim() } else { self.anim_in_race.trim() };
        if s.len() >= 5 && s.is_char_boundary(s.len() - 5) && s[s.len() - 5..].eq_ignore_ascii_case(".pgeo") {
            s[..s.len() - 5].to_owned()
        } else {
            s.to_owned()
        }
    }

    pub fn starts_on_countdown(&self) -> bool {
        truthy(&self.anim_start_on_countdown)
    }

    /// Clip time of the finish line.
    fn finish_s(&self, stations: &Stations) -> f32 {
        if self.in_race_cross_finish_s > 0.0 {
            self.in_race_cross_finish_s
        } else {
            stations.finish_time()
        }
    }
}

/// "true" / true / 1.
pub fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => s.eq_ignore_ascii_case("true") || s == "1",
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        _ => false,
    }
}

fn load_challenges(assets: &Path) -> Vec<Challenge> {
    let path = assets.join("story/airborne.json");
    let Ok(bytes) = std::fs::read(&path) else {
        info!("airborne: {} not installed; the airborne events run as plain races", path.display());
        return Vec::new();
    };
    match serde_json::from_slice::<ChallengeFile>(&bytes) {
        Ok(f) => {
            let list: Vec<Challenge> = f.challenges.into_iter().filter(|c| c.event_id != 0).collect();
            info!("airborne: {} challenges from {}", list.len(), path.display());
            list
        }
        Err(e) => {
            warn!("airborne: {} is not valid: {e}; the airborne events run as plain races", path.display());
            Vec::new()
        }
    }
}

// ---- Pure logic -----------------------------------------------------------------------------------------------

/// The aircraft's schedule: `times[j]` = clip time at the j-th station (0 = the start line, last = the finish line;
/// every gate in between), `t0` = the clip start (checkpoint 0). Progress is a fraction of the race, piecewise linear in
/// time between stations.
#[derive(Clone, Debug, PartialEq)]
pub struct Stations {
    t0: f32,
    times: Vec<f32>,
    offsets: Vec<f32>,
}

impl Stations {
    /// From the checkpoint `(Time, AdditionalRubberBandOffset)` list; with fewer than 3 checkpoints a straight schedule
    /// `0 -> start_s -> finish_s` is used.
    pub fn new(cps: &[(f32, f32)], start_s: f32, finish_s: f32) -> Self {
        let (mut ts, mut off): (Vec<f32>, Vec<f32>) = cps.iter().copied().unzip();
        if ts.len() < 3 {
            let start = start_s.max(0.0);
            ts = vec![0.0, start, finish_s.max(start + 1.0)];
            off = vec![0.0; 3];
        }
        for i in 1..ts.len() {
            if ts[i] < ts[i - 1] {
                ts[i] = ts[i - 1];
            }
        }
        Self { t0: ts[0], times: ts[1..].to_vec(), offsets: off[1..].to_vec() }
    }

    /// Number of race segments (= gates): stations - 1.
    pub fn segments(&self) -> usize {
        self.times.len() - 1
    }

    /// Clip time at the finish line.
    pub fn finish_time(&self) -> f32 {
        *self.times.last().unwrap_or(&0.0)
    }

    /// Fraction of the race flown at clip time `t`: 0 at the start line, 1 at the finish line, negative before the
    /// start (one segment's worth over the pre-start interval, i.e. -1 / segments at the clip start), 1 after the finish.
    pub fn progress(&self, t: f32) -> f32 {
        let n = self.segments() as f32;
        let first = self.times[0];
        if t <= first {
            let span = first - self.t0;
            if span <= 1e-4 {
                return 0.0;
            }
            return -((first - t) / span).clamp(0.0, 1.0) / n;
        }
        if t >= self.finish_time() {
            return 1.0;
        }
        let (j, f) = self.segment(t);
        (j as f32 + f) / n
    }

    /// `AdditionalRubberBandOffset` at clip time `t`, interpolated between stations.
    pub fn offset(&self, t: f32) -> f32 {
        if t <= self.times[0] {
            return self.offsets[0];
        }
        if t >= self.finish_time() {
            return *self.offsets.last().unwrap_or(&0.0);
        }
        let (j, f) = self.segment(t);
        self.offsets[j] + (self.offsets[j + 1] - self.offsets[j]) * f
    }

    /// Segment index and fraction for `first < t < last`.
    fn segment(&self, t: f32) -> (usize, f32) {
        let j = (self.times.partition_point(|&x| x <= t).max(1) - 1).min(self.times.len() - 2);
        let (a, b) = (self.times[j], self.times[j + 1]);
        (j, if b - a > 1e-6 { ((t - a) / (b - a)).clamp(0.0, 1.0) } else { 0.0 })
    }
}

/// The aircraft's target clip-speed multiplier (see the module docs): `gap` = player progress - aircraft progress (+
/// offsets), in race fractions.
pub fn rb_target(base: f32, min: f32, max: f32, spring: f32, gap: f32) -> f32 {
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
    let m = base * (1.0 + spring / SPRING_UNIT * gap);
    if m.is_finite() {
        m.clamp(lo, hi)
    } else {
        base.clamp(lo, hi)
    }
}

/// Moves `cur` toward `target` by at most `accel * dt` (up) or `decel * dt` (down).
pub fn slew(cur: f32, target: f32, accel: f32, decel: f32, dt: f32) -> f32 {
    if target >= cur {
        cur + (accel.max(0.0) * dt).min(target - cur)
    } else {
        cur - (decel.max(0.0) * dt).min(cur - target)
    }
}

/// The player's share of the race: gates done plus the part of the current gate segment covered (`to_next` = distance
/// to the next gate, `seg_len` = length of the segment), over `total` gates.
pub fn route_fraction(done: u32, total: u32, to_next: f32, seg_len: f32) -> f32 {
    if total == 0 {
        return 0.0;
    }
    if done >= total {
        return 1.0;
    }
    let within = if seg_len > 1.0 { (1.0 - to_next / seg_len).clamp(0.0, 1.0) } else { 0.0 };
    ((done as f32 + within) / total as f32).clamp(0.0, 1.0)
}

/// The aircraft has crossed the finish line.
pub fn plane_finished(clip_t: f32, finish_s: f32) -> bool {
    clip_t >= finish_s
}

/// Race clock time of the finish: the frame's clock minus the time since the clip crossed `finish_s`.
pub fn finish_time(clock: f32, clip_t: f32, finish_s: f32, mult: f32) -> f32 {
    (clock - (clip_t - finish_s).max(0.0) / mult.max(0.05)).max(0.0)
}

/// `AfterFinish`: (clip still advances, aircraft visible) `since` seconds after the finish.
pub fn after_finish(since: f32, stopanim: f32, hidevehicle: f32) -> (bool, bool) {
    (stopanim < 0.0 || since < stopanim, hidevehicle < 0.0 || since < hidevehicle)
}

/// Rumble strength at `dist` metres: `strength` inside `inner`, 0 beyond `outer`, linear between.
pub fn rumble_strength(dist: f32, outer: f32, inner: f32, strength: f32) -> f32 {
    if strength <= 0.0 || dist >= outer {
        0.0
    } else if dist <= inner {
        strength
    } else {
        strength * (outer - dist) / (outer - inner).max(1e-3)
    }
}

/// A virtual racer (the aircraft) in `RaceState::racers`: no entity, not the player, lap 0 (real racers start at lap 1).
/// race.rs can use this to count only real grid slots.
pub fn is_virtual(r: &Racer) -> bool {
    !r.is_player && r.entity.is_none() && r.lap == 0
}

/// "P-51 Mustang" / "Helicopter" / "Biplane" / "Hot Air Balloon" from the object and event names.
pub fn vehicle_label(object: &str, ref_name: &str) -> &'static str {
    let s = format!("{object} {ref_name}").to_ascii_lowercase();
    if s.contains("p51") || s.contains("p-51") || s.contains("mustang") || s.contains("timed_event_test") {
        "P-51 Mustang"
    } else if s.contains("heli") || s.contains("chopper") {
        "Helicopter"
    } else if s.contains("biplane") {
        "Biplane"
    } else if s.contains("balloon") {
        "Hot Air Balloon"
    } else {
        "Aircraft"
    }
}

/// Length (XZ) of the gate segment that ends at gate `done` (from the grid for the first one).
fn seg_len(def: &RaceDef, done: u32) -> f32 {
    let total = total_gates(def);
    if total == 0 || def.gates.is_empty() {
        return 0.0;
    }
    let done = done.min(total - 1);
    let g = gate(def, done).centre;
    let prev = if done == 0 { def.grid.first().map_or(g, |p| p.0) } else { gate(def, done - 1).centre };
    Vec2::new(g.x - prev.x, g.z - prev.z).length()
}

// ---- The aircraft clip ---------------------------------------------------------------------------------------

/// The parsed clip: the aircraft's main bone follows the path. Used for the world position (rumble, the placement log)
/// and the clip's duration; the drawing is `anim.rs`'s (hook: [`AirborneDrive`]).
pub struct Curve {
    granny: granny::Granny,
    model: usize,
    bone: usize,
    /// Inverse of the bone's InverseWorld = its bind-pose world matrix.
    bind: Mat4,
    pub duration: f32,
}

impl Curve {
    fn load(path: &Path) -> Option<Curve> {
        let bytes = std::fs::read(path).ok()?;
        let o = granny::parse_anim_object(&bytes).ok()?;
        // The bone of the biggest rigid LOD0 mesh = the aircraft's body (the helicopter files also carry a smoke trail model).
        let mut best: Option<(usize, usize, usize)> = None;
        for (mi, m) in o.models.iter().enumerate() {
            for mesh in &m.lods[0] {
                if let Some(b) = mesh.bone {
                    let n = mesh.vertices.len();
                    if best.is_none_or(|x| n > x.0) {
                        best = Some((n, mi, b as usize));
                    }
                }
            }
        }
        let (_, model, bone) = best.unwrap_or((0, 0, 0));
        let bind = Mat4::from_cols_array_2d(&o.granny.models.get(model)?.bones.get(bone)?.inverse_world).inverse();
        let duration = o.granny.duration();
        Some(Curve { granny: o.granny, model, bone, bind, duration })
    }

    /// Engine-space position of the body bone's origin at clip time `t` (collision space Z mirrored, as anim.rs).
    pub fn position(&self, t: f32) -> Option<Vec3> {
        let pose = self.granny.pose(self.model, t);
        let m = Mat4::from_cols_array_2d(pose.get(self.bone)?) * self.bind;
        let p = m.w_axis;
        Some(Vec3::new(p.x, p.y, -p.z))
    }
}

/// `anim/colorado/index.json` object file by name (case-insensitive).
fn find_object_file(assets: &Path, stem: &str) -> Option<PathBuf> {
    let dir = assets.join("anim/colorado");
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).ok()?).ok()?;
    let o = index["objects"].as_array()?.iter().find(|o| o["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(stem)))?;
    Some(dir.join(o["file"].as_str()?))
}

// ---- Resources ------------------------------------------------------------------------------------------------

/// What `anim.rs` draws (hook): the object to pose (`anim/colorado/index.json` name, case-insensitive), the clip time to
/// pose it at, and whether it is shown. Placement = identity (the clip carries the world path).
#[derive(Resource, Default, Clone, Debug)]
pub struct AirborneDrive {
    pub object: Option<String>,
    pub clip_t: f32,
    pub visible: bool,
}

struct Session {
    race: usize,
    ch: Challenge,
    stations: Stations,
    diff: DiffSpeed,
    object: String,
    on_countdown: bool,
    curve: Option<Arc<Curve>>,
    clip_t: f32,
    mult: f32,
    started: bool,
    pushed: bool,
    racer_idx: usize,
    /// Race clock when the aircraft crossed the finish.
    finished_clock: Option<f32>,
    since_finish: f32,
    driver: String,
    car: String,
    name: String,
    rumble_t: f32,
    log_t: f32,
}

#[derive(Resource, Default)]
pub struct Airborne {
    loaded: bool,
    challenges: Vec<Challenge>,
    strings: Option<Option<fh1_ui::strtable::StringTables>>,
    session: Option<Session>,
    /// Rumble strength (0..1) of the last rumble tick.
    pub rumble: f32,
}

pub struct AirbornePlugin;

impl Plugin for AirbornePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Airborne>().init_resource::<AirborneDrive>().add_systems(Update, airborne_update.after(super::race_update).run_if(crate::ui::driving));
    }
}

// ---- Session ----------------------------------------------------------------------------------------------------

fn begin(i: usize, events: &Events, air: &mut Airborne, assets: &Path, difficulty: usize, task: &mut Option<Task<Option<Curve>>>) -> Option<Session> {
    let def = events.races.get(i)?;
    let ch = air.challenges.iter().find(|c| c.event_id == def.event_id)?.clone();
    let object = ch.object_stem();
    let cps: Vec<(f32, f32)> = ch.checkpoints.iter().map(|c| (c.time, c.offset)).collect();
    let stations = Stations::new(&cps, ch.in_race_cross_start_s, ch.in_race_cross_finish_s);
    if stations.segments() != def.gates.len() {
        info!("airborne: {} has {} checkpoints for {} gates (expected gates + 2); progress is matched by fraction", def.name, ch.checkpoints.len(), def.gates.len());
    }
    let diff = ch.difficulty_anim_speeds.pick(difficulty);
    // Names.
    let strings = air.strings.get_or_insert_with(|| fh1_ui::strtable::StringTables::load_zip(assets.join("ui/strings/EN.zip")).map_err(|e| warn!("airborne: strings: {e}")).ok());
    let text = |id: &str| -> Option<String> {
        if id.is_empty() {
            return None;
        }
        let t = strings.as_ref()?.resolve(&format!("AirChallenges:{id}")).map(fh1_ui::strtable::strip_markup)?;
        t.chars().any(|c| c.is_alphanumeric()).then_some(t)
    };
    let car = text(&ch.opponent.vehicle).unwrap_or_else(|| vehicle_label(&object, &ch.ref_name).to_owned());
    let driver = text(&ch.opponent.pilot).unwrap_or_else(|| car.clone());
    let name = if driver == car { car.clone() } else { format!("{driver} · {car}") };
    // The clip (async: the file is 0.4..2 MB of Granny data).
    match find_object_file(assets, &object) {
        Some(path) => *task = Some(AsyncComputeTaskPool::get().spawn(async move { Curve::load(&path) })),
        None => warn!("airborne: object {object} is not in anim/colorado/index.json (run fh1setup --only anim); the opponent races invisibly by its timing"),
    }
    info!(
        "airborne: {} (event {}): object {object}, {} stations, finish at clip {:.2} s, base x{:.2} band {:.2}..{:.2} (difficulty {difficulty}), spring {}, start on countdown {}",
        def.name,
        def.event_id,
        stations.times.len(),
        ch.finish_s(&stations),
        diff.base_mult,
        diff.rubber_band_min_mult,
        diff.rubber_band_max_mult,
        ch.rubber_banding.rubber_band_spring,
        ch.starts_on_countdown()
    );
    Some(Session {
        race: i,
        on_countdown: ch.starts_on_countdown(),
        ch,
        stations,
        mult: diff.base_mult,
        diff,
        object,
        curve: None,
        clip_t: 0.0,
        started: false,
        pushed: false,
        racer_idx: 0,
        finished_clock: None,
        since_finish: 0.0,
        driver,
        car,
        name,
        rumble_t: 0.0,
        log_t: 0.0,
    })
}

/// Advances one frame; returns the rumble strength on rumble ticks.
fn step(s: &mut Session, task: &mut Option<Task<Option<Curve>>>, rs: &mut RaceState, def: &RaceDef, dt: f32, player: Option<Vec3>, drive: &mut AirborneDrive) -> Option<f32> {
    // The clip finished loading.
    if task.as_ref().is_some_and(|t| t.is_finished()) {
        if let Some(t) = task.take() {
            s.curve = block_on(future::poll_once(t)).flatten().map(Arc::new);
        }
        match &s.curve {
            Some(c) => {
                let (a, b) = (c.position(s.ch.in_race_cross_start_s), c.position(s.ch.in_race_cross_finish_s));
                let start = def.grid.first().map_or(Vec3::ZERO, |p| p.0);
                let end = def.gates.last().map_or(Vec3::ZERO, |g| g.centre);
                // INFERRED placement check: the clip's world path against this race (the aircraft's route need not equal the road route).
                info!(
                    "airborne: clip {} is {:.1} s; at the cross-start time ({:.2} s) it is at {} ({:.0} m from the grid), at the cross-finish time ({:.2} s) at {} ({:.0} m from the last gate)",
                    s.object,
                    c.duration,
                    s.ch.in_race_cross_start_s,
                    a.map_or("?".to_owned(), |p| format!("{p:.0}")),
                    a.map_or(-1.0, |p| p.distance(start)),
                    s.ch.in_race_cross_finish_s,
                    b.map_or("?".to_owned(), |p| format!("{p:.0}")),
                    b.map_or(-1.0, |p| p.distance(end))
                );
            }
            None => warn!("airborne: could not load the clip {}; the opponent races invisibly by its timing", s.object),
        }
    }
    let phase = rs.phase;
    let go = matches!(phase, RacePhase::Racing | RacePhase::Finished { .. } | RacePhase::Results);
    let run = go || (s.on_countdown && matches!(phase, RacePhase::Countdown { .. }));
    if run && !s.started {
        s.started = true;
        info!("airborne: clip {} starts ({})", s.object, if go { "GO" } else { "countdown" });
    }
    let total = total_gates(def);
    // The virtual racer joins at GO.
    if go && !s.pushed && !rs.racers.is_empty() {
        rs.racers.push(Racer {
            entity: None,
            is_player: false,
            name: s.name.clone(),
            driver: s.driver.clone(),
            car: s.car.clone(),
            class_pi: None,
            gates_done: def.start_gate,
            lap: 0,
            to_next: 0.0,
            position: 2,
            finished_s: None,
            last_pos: None,
        });
        s.racer_idx = rs.racers.len() - 1;
        s.pushed = true;
    }
    if s.started {
        let player_f = match rs.racers.first() {
            Some(p) if p.finished_s.is_some() => 1.0,
            Some(p) => route_fraction(p.gates_done, total, p.to_next, seg_len(def, p.gates_done)),
            None => 0.0,
        };
        let rb = s.ch.rubber_banding;
        if s.finished_clock.is_some() {
            s.since_finish += dt;
            s.mult = s.diff.base_mult;
        } else if go {
            let plane_f = s.stations.progress(s.clip_t);
            let gap = player_f + rb.rubber_band_offset_ahead + rb.rubber_band_checkpoint_adjustment + s.stations.offset(s.clip_t) - plane_f;
            let target = rb_target(s.diff.base_mult, s.diff.rubber_band_min_mult, s.diff.rubber_band_max_mult, rb.rubber_band_spring, gap);
            s.mult = slew(s.mult, target, rb.accel_rate, rb.decel_rate, dt);
        } else {
            s.mult = s.diff.base_mult;
        }
        let (advance, shown) = if s.finished_clock.is_some() { after_finish(s.since_finish, s.ch.after_finish.stopanim, s.ch.after_finish.hidevehicle) } else { (true, true) };
        if advance {
            s.clip_t += dt * s.mult;
        }
        let duration = s.curve.as_ref().map_or(f32::MAX, |c| c.duration);
        let at_end = s.clip_t >= duration - 0.05;
        s.clip_t = s.clip_t.min(duration - 0.05).max(0.0);
        // The aircraft crosses the finish.
        let finish_s = s.ch.finish_s(&s.stations);
        if s.finished_clock.is_none() && plane_finished(s.clip_t, finish_s) {
            let t = finish_time(rs.clock_s, s.clip_t, finish_s, s.mult);
            s.finished_clock = Some(t);
            info!("airborne: {} finished at race time {t:.2} s (clip {:.2} s, x{:.2}); player at {:.0} %", s.name, s.clip_t, s.mult, player_f * 100.0);
        }
        drive.object = Some(s.object.clone());
        drive.clip_t = s.clip_t;
        drive.visible = shown && !at_end;
        s.log_t += dt;
        if s.log_t >= 5.0 {
            s.log_t = 0.0;
            info!("airborne: clip {:.1} s x{:.2}, aircraft {:.1} %, player {:.1} %", s.clip_t, s.mult, s.stations.progress(s.clip_t) * 100.0, player_f * 100.0);
        }
    } else {
        drive.object = Some(s.object.clone());
        drive.clip_t = 0.0;
        drive.visible = false;
    }
    // The virtual racer's standing for race.rs's ranking (finished by time, else gates done, then distance to the next gate).
    if s.pushed {
        let finished = s.finished_clock;
        let progress = s.stations.progress(s.clip_t).clamp(0.0, 1.0);
        if let Some(r) = rs.racers.get_mut(s.racer_idx).filter(|r| is_virtual(r)) {
            match finished {
                Some(t) => {
                    r.gates_done = total;
                    r.to_next = 0.0;
                    r.finished_s = Some(t);
                }
                None => {
                    let units = progress * total as f32;
                    r.gates_done = (units.floor() as u32).min(total.saturating_sub(1));
                    r.to_next = (1.0 - units.fract()) * seg_len(def, r.gates_done);
                }
            }
        }
    }
    // Rumble near the player.
    s.rumble_t += dt;
    if s.rumble_t >= RUMBLE_TICK_S {
        s.rumble_t = 0.0;
        let r = s.ch.rumble_near_player;
        if drive.visible && r.strength > 0.0 {
            let pos = s.curve.as_ref().and_then(|c| c.position(s.clip_t));
            return Some(match (pos, player) {
                (Some(a), Some(p)) => rumble_strength(a.distance(p), r.distance_outer, r.distance_inner, r.strength),
                _ => 0.0,
            });
        }
        return Some(0.0);
    }
    None
}

/// Starts / ends the session with the race and runs the aircraft. Runs after `race_update`.
#[allow(clippy::too_many_arguments)]
fn airborne_update(
    events: Res<Events>,
    mut rs: ResMut<RaceState>,
    time: Res<Time>,
    garage: Res<crate::Garage>,
    settings: Option<Res<crate::ui::Settings>>,
    cars: Query<&Car>,
    pads: Query<Entity, With<Gamepad>>,
    mut rumble: Option<ResMut<Messages<GamepadRumbleRequest>>>,
    mut air: ResMut<Airborne>,
    mut drive: ResMut<AirborneDrive>,
    mut task: Local<Option<Task<Option<Curve>>>>,
) {
    if !enabled() {
        return;
    }
    if !air.loaded {
        air.loaded = true;
        air.challenges = load_challenges(&garage.assets);
    }
    if air.challenges.is_empty() {
        return;
    }
    let dt = time.delta_secs().min(0.25);
    let live = rs.race.filter(|_| rs.phase != RacePhase::Idle);
    let mut session = air.session.take();
    // The race ended (results done / F7): drop the aircraft.
    if session.as_ref().is_some_and(|s| live != Some(s.race)) {
        info!("airborne: race over, aircraft removed");
        session = None;
        *task = None;
        *drive = AirborneDrive::default();
        air.rumble = 0.0;
        if let Some(q) = rumble.as_mut() {
            for g in &pads {
                q.write(GamepadRumbleRequest::Stop { gamepad: g });
            }
        }
    }
    if session.is_none() {
        if let (Some(i), true) = (live, matches!(rs.phase, RacePhase::Grid { .. } | RacePhase::Countdown { .. } | RacePhase::Racing)) {
            if events.races.get(i).is_some_and(|d| air.challenges.iter().any(|c| c.event_id == d.event_id)) {
                let difficulty = settings.as_ref().map_or(1, |s| s.ai_difficulty.index());
                session = begin(i, &events, &mut air, &garage.assets, difficulty, &mut task);
            }
        }
    }
    if let Some(s) = session.as_mut() {
        let def = &events.races[s.race];
        let player = cars.iter().next().map(|c| c.0.position);
        if let Some(v) = step(s, &mut task, &mut rs, def, dt, player, &mut drive) {
            air.rumble = v;
            if let (true, Some(q)) = (v > 0.02, rumble.as_mut()) {
                for g in &pads {
                    q.write(GamepadRumbleRequest::Add {
                        duration: Duration::from_millis(130),
                        intensity: GamepadRumbleIntensity { strong_motor: v.min(1.0), weak_motor: (v * 0.6).min(1.0) },
                        gamepad: g,
                    });
                }
            }
        }
    }
    air.session = session;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> Stations {
        // 2 gates: clip start 0, start line 5, gate 0 at 15, finish at 25.
        Stations::new(&[(0.0, 0.0), (5.0, 0.0), (15.0, 0.01), (25.0, -0.01)], 5.0, 25.0)
    }

    #[test]
    fn progress_follows_checkpoints() {
        let s = st();
        assert_eq!(s.segments(), 2);
        assert!((s.progress(5.0) - 0.0).abs() < 1e-6);
        assert!((s.progress(15.0) - 0.5).abs() < 1e-6);
        assert!((s.progress(25.0) - 1.0).abs() < 1e-6);
        assert!((s.progress(10.0) - 0.25).abs() < 1e-6);
        assert!((s.progress(20.0) - 0.75).abs() < 1e-6);
        // Before the start line: one segment's worth (1/2) at the clip start, linear.
        assert!((s.progress(0.0) + 0.5).abs() < 1e-6);
        assert!((s.progress(2.5) + 0.25).abs() < 1e-6);
        assert_eq!(s.progress(99.0), 1.0);
        // Offsets interpolate.
        assert!((s.offset(10.0) - 0.005).abs() < 1e-6);
        assert!((s.offset(20.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn progress_handles_equal_times_and_short_lists() {
        // Event 168: checkpoints 0 and 1 both at 0.0.
        let s = Stations::new(&[(0.0, 0.0), (0.0, 0.0), (11.2, 0.01), (121.94, 0.0)], 0.0, 121.94);
        assert_eq!(s.progress(0.0), 0.0);
        assert!((s.progress(11.2) - 0.5).abs() < 1e-6);
        assert!(s.progress(60.0).is_finite());
        // No checkpoints: straight schedule start -> finish.
        let s = Stations::new(&[], 10.0, 110.0);
        assert!((s.progress(60.0) - 0.5).abs() < 1e-6);
        assert_eq!(s.segments(), 1);
    }

    #[test]
    fn rubber_band_clamps() {
        // Player far ahead: the aircraft is held at the upper clamp (it speeds up to catch the player); far behind: the lower.
        assert_eq!(rb_target(1.0, 0.75, 1.3, 30000.0, 0.5), 1.3);
        assert_eq!(rb_target(1.0, 0.75, 1.3, 30000.0, -0.5), 0.75);
        // Equal progress: base.
        assert_eq!(rb_target(1.0, 0.75, 1.3, 30000.0, 0.0), 1.0);
        // The balloon's soft spring barely moves it: 10 % gap -> +0.3 %.
        assert!((rb_target(1.0, 0.7, 1.3, 300.0, 0.1) - 1.003).abs() < 1e-5);
        // Swapped bounds and NaN do not panic.
        assert!(rb_target(1.0, 1.3, 0.75, 30000.0, 0.01).is_finite());
        assert_eq!(rb_target(1.0, 0.75, 1.3, f32::NAN, 0.1), 1.0);
        // Never outside the band.
        for k in -20..=20 {
            let m = rb_target(1.0, 0.9, 1.1, 30000.0, k as f32 * 0.05);
            assert!((0.9..=1.1).contains(&m));
        }
    }

    #[test]
    fn rate_limit() {
        // Speeding up at 0.4 / s, slowing down at 0.15 / s.
        assert!((slew(1.0, 1.3, 0.4, 0.15, 0.5) - 1.2).abs() < 1e-6);
        assert!((slew(1.0, 0.75, 0.4, 0.15, 0.5) - 0.925).abs() < 1e-6);
        // Reaches the target without overshoot.
        assert_eq!(slew(1.0, 1.05, 0.4, 0.15, 1.0), 1.05);
        assert_eq!(slew(1.0, 0.95, 0.4, 0.15, 1.0), 0.95);
        // Over a long time the multiplier stays inside the band and changes by at most the rate each step.
        let mut m = 1.0f32;
        for k in 0..2000 {
            let target = if (k / 100) % 2 == 0 { 1.3 } else { 0.75 };
            let n = slew(m, target, 0.4, 0.15, 1.0 / 60.0);
            assert!((n - m).abs() <= 0.4 / 60.0 + 1e-6);
            assert!((0.75..=1.3).contains(&n));
            m = n;
        }
    }

    #[test]
    fn player_progress() {
        // 4 gates; on the second segment, half way.
        assert!((route_fraction(1, 4, 50.0, 100.0) - 0.375).abs() < 1e-6);
        assert_eq!(route_fraction(0, 4, 100.0, 100.0), 0.0);
        assert_eq!(route_fraction(4, 4, 0.0, 100.0), 1.0);
        // Past the segment (to_next > seg_len) does not go negative; a degenerate segment is ignored.
        assert_eq!(route_fraction(2, 4, 500.0, 100.0), 0.5);
        assert_eq!(route_fraction(2, 4, 10.0, 0.0), 0.5);
        assert_eq!(route_fraction(0, 0, 1.0, 1.0), 0.0);
    }

    #[test]
    fn finish_detection() {
        assert!(!plane_finished(171.6, 171.68));
        assert!(plane_finished(171.68, 171.68));
        // Crossed 0.04 clip-seconds into the frame at x1.0: finish 0.04 s before the frame's clock.
        assert!((finish_time(100.0, 171.72, 171.68, 1.0) - 99.96).abs() < 1e-3);
        assert!((finish_time(100.0, 171.76, 171.68, 2.0) - 99.96).abs() < 1e-3);
        assert_eq!(finish_time(0.01, 171.72, 171.68, 0.05), 0.0);
    }

    #[test]
    fn after_finish_rules() {
        // Planes: keep flying, hidden after hidevehicle.
        assert_eq!(after_finish(0.0, -1.0, 14.0), (true, true));
        assert_eq!(after_finish(13.9, -1.0, 14.0), (true, true));
        assert_eq!(after_finish(14.0, -1.0, 14.0), (true, false));
        // Balloon: stops after 30 s, never hidden.
        assert_eq!(after_finish(29.0, 30.0, -1.0), (true, true));
        assert_eq!(after_finish(30.0, 30.0, -1.0), (false, true));
        assert_eq!(after_finish(999.0, 30.0, -1.0), (false, true));
    }

    #[test]
    fn rumble_ramp() {
        assert_eq!(rumble_strength(100.0, 80.0, 15.0, 0.8), 0.0);
        assert_eq!(rumble_strength(10.0, 80.0, 15.0, 0.8), 0.8);
        assert!((rumble_strength(47.5, 80.0, 15.0, 0.8) - 0.4).abs() < 1e-6);
        assert_eq!(rumble_strength(1.0, 8.0, 1.0, 0.0), 0.0);
    }

    #[test]
    fn json_and_names() {
        let j = r#"{"challenges":[{"event_id":223,"ref_name":"Balloon Event","anim_in_race":"ANIM_Showcase_Event_Balloon_Race","anim_object":"ANIM_Timed_Event_Test_Animation.pgeo",
            "anim_start_on_countdown":"true","in_race_cross_start_s":0.2,"in_race_cross_finish_s":96.69,"after_finish":{"stopanim":30.0,"hidevehicle":-1.0},
            "checkpoints":[{"Time":0.0,"SplineDistToNext":1.0,"AdditionalRubberBandOffset":0.0},{"Time":0.2,"SplineDistToNext":1.0,"AdditionalRubberBandOffset":0.0},{"Time":96.69,"SplineDistToNext":1.0,"AdditionalRubberBandOffset":0.0}],
            "difficulty_anim_speeds":{"Pro":{"base_mult":1.0,"rubber_band_min_mult":0.7,"rubber_band_max_mult":1.3}},
            "opponent":{"LocStringPilotName":"IDS_Pilot4","LocStringAirVehName":"IDS_Vehicle4"}}]}"#;
        let f: ChallengeFile = serde_json::from_str(j).unwrap();
        let c = &f.challenges[0];
        assert_eq!(c.object_stem(), "ANIM_Showcase_Event_Balloon_Race");
        assert!(c.starts_on_countdown());
        assert_eq!(c.after_finish.stopanim, 30.0);
        assert_eq!(c.checkpoints.len(), 3);
        assert_eq!(c.difficulty_anim_speeds.pick(3).rubber_band_min_mult, 0.7);
        assert_eq!(c.difficulty_anim_speeds.pick(0).rubber_band_min_mult, 0.8);
        assert_eq!(c.opponent.pilot, "IDS_Pilot4");
        let t: Challenge = serde_json::from_str(r#"{"event_id":101,"anim_in_race":"X.pgeo","anim_start_on_countdown":"false"}"#).unwrap();
        assert_eq!(t.object_stem(), "X");
        assert!(!t.starts_on_countdown());
        assert_eq!(t.after_finish.hidevehicle, -1.0);
        assert_eq!(vehicle_label("ANIM_Timed_Event_Test_Animation", "P51 Mustang Challenge"), "P-51 Mustang");
        assert_eq!(vehicle_label("ANIM_Showcase_Event_Helicopter_Race_2", "x"), "Helicopter");
        assert_eq!(vehicle_label("ANIM_Showcase_Event_Biplane_Race", "x"), "Biplane");
        assert_eq!(vehicle_label("ANIM_Showcase_Event_Balloon_Race", "Balloon Event"), "Hot Air Balloon");
    }
}
