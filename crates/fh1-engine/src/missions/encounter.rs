//! Race encounters (docs/MISSIONS.md): a roadside one-on-one street race against an AI driver, offered while free-roaming.
//!
//! VERIFIED (missions.json, EU disc): `race_encounter.wristband_multipliers` (payout factor per wristband tier, Yellow 1.0
//! .. 1.5). Everything else is INFERRED (the constants below): when and where an encounter is offered, the opponent's
//! car / driver / skill, the course (a stretch of one of the 46 installed AI racing lines), the lane layout, the payout
//! base and the popularity gain.
//!
//! Flow: [`update_offer`] (idle: timer -> prompt -> accept -> teleport both cars onto the line, spawn the AI) ->
//! [`update_run`] (countdown, race, result, clean-up) with [`drive_control`] telling the AI plugin to hold / which line
//! the player's driving line follows. One activity at a time ([`Activity::Encounter`]); aborted when a race starts.
//! `FH1_ENCOUNTERS=0` = none of this. Needs the AI plugin (`SpawnRaceAi` queue + `AiRaceControl`), else nothing is offered.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::ecs::message::Messages;
use bevy::ecs::system::SystemParam;
use bevy::input::gamepad::Gamepad;
use bevy::prelude::*;
use fh1_engine::ai::line::RacingLine;
use fh1_engine::ai::{route_id, AiCar, AiRaceControl, AiRacer, DespawnRaceAi, SpawnRaceAi};
use fh1_engine::world::MIRROR_Z;

use super::hud::{self, MissionHud, NavGuard};
use super::map::{IconKind, MapIcon, MissionMapIcons};
use super::{Activity, Missions, Player, Rng};
use crate::progression::{Banners, Profile};
use crate::race::{Events, RaceState};
use crate::track::Track;
use crate::Car;

/// INFERRED: free-roam seconds between encounters (random in this range, rolled again after each one).
const ENCOUNTER_COOLDOWN_S: (f32, f32) = (120.0, 240.0);
/// INFERRED: seconds before the next try after an offer was ignored / declined / failed.
const ENCOUNTER_DECLINE_COOLDOWN_S: (f32, f32) = (60.0, 120.0);
/// INFERRED: how long the prompt stays up.
const ENCOUNTER_OFFER_S: f32 = 15.0;
/// INFERRED: the player must be above this speed (mph) to be challenged.
const ENCOUNTER_MIN_MPH: f32 = 25.0;
/// INFERRED: the player must be within this many metres of an AI racing line to be challenged.
const ENCOUNTER_LINE_RANGE_M: f32 = 25.0;
/// INFERRED: the player's heading must agree with the line's direction (dot of the 2D unit vectors).
const ENCOUNTER_MIN_ALIGN: f32 = 0.6;
/// INFERRED: race length along the line (m).
const ENCOUNTER_LENGTH_M: f32 = 2000.0;
/// INFERRED: open lines with less than this left ahead of the player are not offered (m).
const ENCOUNTER_MIN_REMAINING_M: f32 = 800.0;
/// INFERRED: open lines end this far before the line's end (m).
const ENCOUNTER_END_MARGIN_M: f32 = 50.0;
/// INFERRED: a closed line is never raced for more than this fraction of its lap.
const ENCOUNTER_MAX_LAP_FRAC: f32 = 0.9;
/// INFERRED: lateral grid offset of each car from the road centre (m); capped at 0.6 of the half road width.
const ENCOUNTER_LANE_M: f32 = 3.5;
/// INFERRED: countdown (s).
const ENCOUNTER_COUNTDOWN_S: f32 = 3.0;
/// INFERRED: after the rival finishes first, the player has this long to finish before it counts as a loss (s).
const ENCOUNTER_LOSS_GRACE_S: f32 = 30.0;
/// INFERRED: abort (no result) when the player is farther than this from the line (m) for [`ENCOUNTER_OFF_LINE_S`].
const ENCOUNTER_OFF_LINE_M: f32 = 250.0;
const ENCOUNTER_OFF_LINE_S: f32 = 8.0;
/// INFERRED: the rival spawns within this many seconds, else the encounter is dropped (car failed to load).
const ENCOUNTER_SPAWN_TIMEOUT_S: f32 = 8.0;
/// INFERRED: how long the result stays up with the rival cruising before it is removed (s).
const ENCOUNTER_RESULT_S: f32 = 4.0;
/// INFERRED: credits for a win at the Yellow wristband before the multiplier.
const ENCOUNTER_BASE_CR: f32 = 2000.0;
/// INFERRED: popularity (fame) for a win.
const ENCOUNTER_FAME: u64 = 500;
/// INFERRED: AISkills id per Options AI difficulty (Easy, Medium, Hard, Pro); 1 = fastest. Each wristband tier above
/// Yellow lowers the id by 2 (as race/field.rs scales a replayed event).
const ENCOUNTER_SKILL: [u32; 4] = [64, 36, 16, 5];
/// INFERRED: the rival's car is one of this many same-class cars closest in PI to the player's.
const ENCOUNTER_CAR_POOL: usize = 8;

/// INFERRED: no offer (and no start) within this many metres of a race event's start marker: standing on the grid in a
/// marker's zone would let a press of A / Enter start that event on top of the encounter (race.rs shares the button).
const ENCOUNTER_MARKER_CLEAR_M: f32 = 80.0;
/// A car this many metres from where it was last frame was teleported (fast travel, garage, reset): the run is dropped.
const ENCOUNTER_JUMP_M: f32 = 150.0;
/// A step along the line bigger than this in one frame is a mis-projection (a part of the route that comes back close to
/// the road), not driving: it isn't counted.
const ENCOUNTER_MAX_STEP_M: f32 = 100.0;

/// `FH1_ENCOUNTER_GUARDS=0`: the first version's rules (no marker clearance, no jump / mis-projection guards).
fn guards_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_ENCOUNTER_GUARDS"))
}

/// Seconds between line-proximity checks while an offer is due.
const CHECK_S: f32 = 0.75;

#[derive(Clone)]
struct Offer {
    route: u32,
    line: Arc<RacingLine>,
    driver: u32,
    driver_name: String,
    car: String,
    label: String,
    skill: u32,
    left: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Countdown,
    Racing,
    /// Result shown; seconds until the rival is removed.
    Done(f32),
}

/// Progress along the line since the start (m), with the wrap handling of ai/plugin.rs `AiBrain::track_player`.
struct Prog {
    hint: Option<usize>,
    last_s: f32,
    prog: f32,
}

impl Prog {
    fn new(start_s: f32) -> Self {
        Self { hint: None, last_s: start_s, prog: 0.0 }
    }

    /// Feed a position; returns its distance from the line.
    fn advance(&mut self, line: &RacingLine, p: Vec3) -> f32 {
        let mut proj = line.project(p, self.hint);
        if self.hint.is_some() && proj.distance > 40.0 {
            proj = line.project(p, None);
        }
        let mut ds = proj.s - self.last_s;
        if line.closed {
            if ds < -0.5 * line.length {
                ds += line.length;
            } else if ds > 0.5 * line.length {
                ds -= line.length;
            }
        }
        if guards_on() && ds.abs() > ENCOUNTER_MAX_STEP_M {
            // A jump along the line (the road crosses or comes back near itself): keep the old place.
            return proj.distance;
        }
        self.prog += ds;
        self.hint = Some(proj.index);
        self.last_s = proj.s;
        proj.distance
    }
}

struct Run {
    route: u32,
    line: Arc<RacingLine>,
    driver: String,
    phase: Phase,
    count: f32,
    clock: f32,
    since_spawn: f32,
    ai_seen: bool,
    /// Race length (m) and the finish point.
    dist: f32,
    finish: Vec3,
    /// The player's grid point (held there during the countdown).
    grid: Vec3,
    player: Prog,
    rival: Prog,
    player_done: Option<f32>,
    rival_done: Option<f32>,
    off_s: f32,
    held: f32,
    /// The player's position last frame (jump detection).
    last_pos: Option<Vec3>,
}

#[derive(Resource)]
struct EncounterState {
    rng: Rng,
    lines: Vec<(u32, Arc<RacingLine>)>,
    pending: Vec<PathBuf>,
    listed: bool,
    /// Free-roam seconds until the next offer (None = not rolled yet).
    cooldown: Option<f32>,
    check: f32,
    offer: Option<Offer>,
    /// The MissionHud prompt is ours.
    prompt_ours: bool,
    run: Option<Run>,
}

impl Default for EncounterState {
    fn default() -> Self {
        Self { rng: Rng::seeded(), lines: Vec::new(), pending: Vec::new(), listed: false, cooldown: None, check: 0.0, offer: None, prompt_ours: false, run: None }
    }
}

impl EncounterState {
    fn roll(&mut self, range: (f32, f32)) {
        self.cooldown = Some(self.rng.range(range.0, range.1));
    }

    /// List `<assets>/ailines/colorado/route_NNN.owt` once, then parse a couple per frame.
    fn load_some(&mut self, assets: &Path) {
        if !self.listed {
            self.listed = true;
            if let Ok(rd) = std::fs::read_dir(assets.join("ailines/colorado")) {
                self.pending = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "owt")).collect();
                self.pending.sort();
                self.pending.reverse();
            }
        }
        for _ in 0..2 {
            let Some(path) = self.pending.pop() else { break };
            let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(route_id) else { continue };
            match RacingLine::load(&path, MIRROR_Z) {
                Ok(l) => self.lines.push((id, Arc::new(l))),
                Err(e) => warn!("encounters: {}: {e:#}", path.display()),
            }
        }
    }
}

/// AI plugin hand-over: spawn / despawn queues, the control resource and the AI cars.
#[derive(SystemParam)]
struct AiOut<'w, 's> {
    spawn: Option<ResMut<'w, Messages<SpawnRaceAi>>>,
    despawn: Option<ResMut<'w, Messages<DespawnRaceAi>>>,
    control: Option<ResMut<'w, AiRaceControl>>,
    cars: Query<'w, 's, (&'static AiRacer, &'static AiCar)>,
}

impl AiOut<'_, '_> {
    fn available(&self) -> bool {
        self.spawn.is_some() && self.control.is_some()
    }

    /// The rival's (slot 1) position.
    fn rival(&self) -> Option<Vec3> {
        self.cars.iter().find(|(r, _)| r.slot == 1).map(|(_, c)| c.0.position)
    }

    fn despawn_all(&mut self) {
        if let Some(q) = self.despawn.as_mut() {
            q.write(DespawnRaceAi);
        }
        if let Some(c) = self.control.as_mut() {
            if c.hold || !c.finished.is_empty() || c.route_file.is_some() {
                c.hold = false;
                c.finished.clear();
                c.route_file = None;
            }
        }
    }
}

/// Objective line, satnav and map icon of the running encounter.
#[derive(SystemParam)]
struct Nav<'w> {
    guard: ResMut<'w, NavGuard>,
    objective: ResMut<'w, crate::ui::notify::Objective>,
    satnav: Option<ResMut<'w, crate::ui::minimap::SatNav>>,
    icons: ResMut<'w, MissionMapIcons>,
}

impl Nav<'_> {
    fn begin(&mut self, finish: Vec3, text: &str) {
        let target = super::xz(finish);
        self.guard.set(&mut self.objective, self.satnav.as_deref_mut(), Some(text.to_owned()), Some(target));
        self.icons.set_dynamic(vec![MapIcon { key: "encounter:finish".into(), kind: IconKind::Target, pos: target, radius: 0.0, name: "Street race finish".into(), lines: vec![], ..Default::default() }]);
    }

    fn end(&mut self) {
        self.guard.restore(&mut self.objective, self.satnav.as_deref_mut());
        self.icons.set_dynamic(vec![]);
    }
}

/// Payout paths.
#[derive(SystemParam)]
struct Pay<'w> {
    profile: ResMut<'w, Profile>,
    banners: ResMut<'w, Banners>,
    notify: MessageWriter<'w, crate::ui::notify::HudNotify>,
}

pub fn register(app: &mut App) {
    if !super::flag_on("FH1_ENCOUNTERS") {
        return;
    }
    app.init_resource::<EncounterState>()
        .add_systems(Update, (update_offer, update_run).chain().run_if(super::on_colorado).run_if(crate::ui::driving))
        .add_systems(Update, drive_control.after(crate::race::race_update).after(update_run).run_if(super::on_colorado))
        .add_systems(Update, abandon.run_if(off_colorado));
}

fn off_colorado(track: Res<Track>) -> bool {
    !(super::enabled() && track.id == "colorado")
}

fn read_player(cars: &Query<&mut Car>) -> Option<Player> {
    cars.iter().next().map(|c| Player { pos: c.0.position, vel: c.0.velocity, rot: c.0.rotation, media: c.0.data.media_name.clone() })
}

fn ground(track: &Track, p: Vec3) -> Vec3 {
    track.ground.ray(p + Vec3::Y * 10.0, Vec3::NEG_Y, 40.0).map_or(p, |h| h.point)
}

fn route_file(route: u32) -> String {
    format!("Ribbon_00/TrackRoute{route:03}.xml")
}

fn set_prompt(hud: &mut MissionHud, ours: &mut bool, text: String) {
    if hud.prompt.is_none() || *ours {
        if hud.prompt.as_deref() != Some(text.as_str()) {
            hud.prompt = Some(text);
        }
        *ours = true;
    }
}

fn clear_prompt(hud: &mut MissionHud, ours: &mut bool) {
    if *ours {
        hud.prompt = None;
        *ours = false;
    }
}

/// Race length from `s` on this line (None = not enough road / lap).
fn race_length(line: &RacingLine, s: f32) -> Option<f32> {
    if line.closed {
        (line.length >= ENCOUNTER_MIN_REMAINING_M).then(|| ENCOUNTER_LENGTH_M.min(line.length * ENCOUNTER_MAX_LAP_FRAC))
    } else {
        let left = line.length - s;
        (left >= ENCOUNTER_MIN_REMAINING_M).then(|| ENCOUNTER_LENGTH_M.min(left - ENCOUNTER_END_MARGIN_M))
    }
}

#[allow(clippy::too_many_arguments)]
fn update_offer(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    pads: Query<&Gamepad>,
    track: Res<Track>,
    garage: Res<crate::Garage>,
    events: Res<Events>,
    profile: Res<Profile>,
    rs: Option<Res<RaceState>>,
    settings: Option<Res<crate::ui::Settings>>,
    mut st: ResMut<EncounterState>,
    mut act: ResMut<Activity>,
    mut hud: ResMut<MissionHud>,
    mut cars: Query<&mut Car>,
    mut ai: AiOut,
    mut nav: Nav,
) {
    let st = &mut *st;
    if st.run.is_some() {
        return;
    }
    let dt = time.delta_secs().min(0.25);
    st.load_some(&garage.assets);
    let busy = super::race_running(&rs) || rs.as_ref().is_some_and(|r| r.prompt.is_some() || (guards_on() && r.list_open));
    if !act.free() || busy || !ai.available() {
        if st.offer.take().is_some() {
            clear_prompt(&mut hud, &mut st.prompt_ours);
        }
        return;
    }
    let Some(p) = read_player(&cars) else { return };
    let near_marker = guards_on() && events.races.iter().any(|r| Vec2::new(r.marker.0.x - p.pos.x, r.marker.0.z - p.pos.z).length() < ENCOUNTER_MARKER_CLEAR_M);

    // An open offer: accept, expire, or keep showing.
    if let Some(mut o) = st.offer.take() {
        o.left -= dt;
        if o.left <= 0.0 || near_marker {
            clear_prompt(&mut hud, &mut st.prompt_ours);
            st.roll(ENCOUNTER_DECLINE_COOLDOWN_S);
            return;
        }
        if super::confirm(&keys, &pads) {
            clear_prompt(&mut hud, &mut st.prompt_ours);
            if !begin(st, &o, &p, &track, &mut cars, &mut ai, &mut nav, &mut hud, &mut act) {
                st.roll(ENCOUNTER_DECLINE_COOLDOWN_S);
            }
            return;
        }
        set_prompt(&mut hud, &mut st.prompt_ours, format!("STREET RACE: {} ({}) wants to race  -  press A / Enter", o.driver_name, o.label));
        st.offer = Some(o);
        return;
    }

    // Free-roam timer, then look for a line next to a fast-moving player.
    if st.cooldown.is_none() {
        st.roll(ENCOUNTER_COOLDOWN_S);
    }
    if let Some(c) = st.cooldown.as_mut() {
        if *c > 0.0 {
            *c -= dt;
            return;
        }
    }
    st.check -= dt;
    if st.check > 0.0 || p.speed_mph() < ENCOUNTER_MIN_MPH || near_marker {
        return;
    }
    st.check = CHECK_S;
    let tier = super::reward::tier(&profile, &events);
    let difficulty = settings.as_ref().map_or(1, |s| s.ai_difficulty.index()).min(3);
    st.offer = try_offer(st, &p, &events, tier, difficulty);
}

/// The nearest installed line within range, aligned with the player's heading, plus an opponent.
fn try_offer(st: &mut EncounterState, p: &Player, events: &Events, tier: usize, difficulty: usize) -> Option<Offer> {
    let fwd = p.forward2();
    let (route, line) = st
        .lines
        .iter()
        .map(|(id, l)| (*id, l, l.project(p.pos, None)))
        .filter(|(_, l, pr)| {
            let t = l.tangent_at(pr.s);
            pr.distance <= ENCOUNTER_LINE_RANGE_M && Vec2::new(t.x, t.z).normalize_or_zero().dot(fwd) >= ENCOUNTER_MIN_ALIGN && race_length(l, pr.s).is_some()
        })
        .min_by(|a, b| a.2.distance.total_cmp(&b.2.distance))
        .map(|(id, l, _)| (id, l.clone()))?;
    let c = &events.career;
    let (class, pi) = crate::progression::player_class(c, &p.media)?;
    let mut pool: Vec<(&String, u32)> = c.cars.iter().filter(|(_, i)| i.installed && i.selectable && i.class == class).map(|(m, i)| (m, i.pi.abs_diff(pi))).collect();
    pool.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(b.0)));
    pool.truncate(ENCOUNTER_CAR_POOL);
    let mut drivers: Vec<(u32, &str)> = c.drivers.values().filter(|d| !d.nemesis && !d.name.is_empty()).map(|d| (d.id, d.name.as_str())).collect();
    drivers.sort();
    if pool.is_empty() {
        return None;
    }
    let car = pool[(st.rng.next_f32() * pool.len() as f32) as usize % pool.len()].0.clone();
    let label = crate::race::field::car_label(c, &car);
    let (driver, driver_name) = if drivers.is_empty() {
        (0, label.clone())
    } else {
        let d = drivers[(st.rng.next_f32() * drivers.len() as f32) as usize % drivers.len()];
        (d.0, d.1.to_owned())
    };
    let skill = ENCOUNTER_SKILL[difficulty].saturating_sub(2 * tier as u32).max(1);
    Some(Offer { route, line, driver, driver_name, car, label, skill, left: ENCOUNTER_OFFER_S })
}

/// Accepted: put both cars on the line, spawn the rival, start the countdown. False = could not start.
#[allow(clippy::too_many_arguments)]
fn begin(st: &mut EncounterState, o: &Offer, p: &Player, track: &Track, cars: &mut Query<&mut Car>, ai: &mut AiOut, nav: &mut Nav, hud: &mut MissionHud, act: &mut Activity) -> bool {
    let line = &o.line;
    let proj = line.project(p.pos, None);
    if proj.distance > ENCOUNTER_LINE_RANGE_M * 2.5 {
        return false;
    }
    let Some(dist) = race_length(line, proj.s) else { return false };
    let s = proj.s;
    let t = line.tangent_at(s);
    let left = Vec3::Y.cross(Vec3::new(t.x, 0.0, t.z)).normalize_or_zero();
    let (centre, lateral, _) = line.road_at(s);
    let off = ENCOUNTER_LANE_M.min(lateral.length() * 0.6).max(1.0);
    let yaw = line.yaw_at(s);
    let player_pos = ground(track, centre + left * off);
    let rival_pos = ground(track, centre - left * off);
    let finish = line.point_at(line.wrap_s(s + dist));

    let Some(q) = ai.spawn.as_mut() else { return false };
    q.write(SpawnRaceAi {
        slot: 1,
        car_id: o.car.clone(),
        pose: (rival_pos, yaw),
        skill: o.skill,
        temperament: 0,
        rubberband: 0,
        driver_id: o.driver,
        route_file: route_file(o.route),
        circuit: line.closed,
        paint: 0,
        ..Default::default()
    });
    if let Some(mut car) = cars.iter_mut().next() {
        car.0.place(player_pos, yaw);
    }
    nav.begin(finish, "Beat the rival to the finish");
    hud.clear();
    *act = Activity::Encounter;
    st.run = Some(Run {
        route: o.route,
        line: line.clone(),
        driver: o.driver_name.clone(),
        phase: Phase::Countdown,
        count: ENCOUNTER_COUNTDOWN_S,
        clock: 0.0,
        since_spawn: 0.0,
        ai_seen: false,
        dist,
        finish,
        grid: player_pos,
        player: Prog::new(s),
        rival: Prog::new(s),
        player_done: None,
        rival_done: None,
        off_s: 0.0,
        held: 0.0,
        last_pos: None,
    });
    info!("encounters: street race vs {} ({}) on route {}, {:.0} m, skill {}", o.driver_name, o.car, o.route, dist, o.skill);
    true
}

enum Outcome {
    Abort(Option<&'static str>),
    Win,
    Lose,
}

#[allow(clippy::too_many_arguments)]
fn update_run(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    pads: Query<&Gamepad>,
    events: Res<Events>,
    missions: Res<Missions>,
    rs: Option<Res<RaceState>>,
    mut st: ResMut<EncounterState>,
    mut act: ResMut<Activity>,
    mut hud: ResMut<MissionHud>,
    mut cars: Query<&mut Car>,
    mut ai: AiOut,
    mut nav: Nav,
    mut pay: Pay,
) {
    let st = &mut *st;
    let dt = time.delta_secs().min(0.25);
    let Some(run) = st.run.as_mut() else { return };

    // Result on screen: the rival cruises, then goes.
    if let Phase::Done(left) = &mut run.phase {
        *left -= dt;
        if *left <= 0.0 {
            ai.despawn_all();
            st.run = None;
            *act = Activity::None;
            st.roll(ENCOUNTER_COOLDOWN_S);
        }
        return;
    }
    let Some(p) = read_player(&cars) else { return };

    let mut outcome = None;
    if super::race_running(&rs) {
        outcome = Some(Outcome::Abort(None));
    }
    if super::cancel_held(&keys, &pads, &mut run.held, dt) {
        outcome = Some(Outcome::Abort(Some("STREET RACE CANCELLED")));
    }
    // Teleported (fast travel, garage, reset): the race is over.
    if guards_on() && run.last_pos.replace(p.pos).is_some_and(|a| a.distance(p.pos) > ENCOUNTER_JUMP_M) && !matches!(run.phase, Phase::Countdown) {
        outcome = Some(Outcome::Abort(Some("STREET RACE ABANDONED")));
    }
    // The rival must show up.
    run.since_spawn += dt;
    let rival_pos = ai.rival();
    run.ai_seen |= rival_pos.is_some();
    if !run.ai_seen && run.since_spawn > ENCOUNTER_SPAWN_TIMEOUT_S {
        outcome = Some(Outcome::Abort(Some("RIVAL UNAVAILABLE")));
    }
    // Progress of both cars.
    let player_off = run.player.advance(&run.line, p.pos);
    if let Some(rp) = rival_pos {
        run.rival.advance(&run.line, rp);
    }
    if player_off > ENCOUNTER_OFF_LINE_M {
        run.off_s += dt;
        if run.off_s >= ENCOUNTER_OFF_LINE_S {
            outcome = Some(Outcome::Abort(Some("STREET RACE ABANDONED")));
        }
    } else {
        run.off_s = 0.0;
    }

    match run.phase {
        Phase::Countdown => {
            // Held on the grid (race.rs does the same for event grids).
            for mut car in cars.iter_mut() {
                let v = &mut car.0;
                if Vec2::new(v.position.x - run.grid.x, v.position.z - run.grid.z).length() > 0.3 || v.velocity.length() > 0.5 {
                    v.velocity = Vec3::new(0.0, v.velocity.y.min(0.0), 0.0);
                    v.angular_velocity = Vec3::ZERO;
                    let keep_y = v.position.y;
                    v.position = Vec3::new(run.grid.x, keep_y, run.grid.z);
                    v.prev_position = v.position;
                }
            }
            run.count -= dt;
            hud.title = Some(format!("{}", run.count.ceil().max(1.0) as u32));
            hud.lines = vec![format!("vs {}", run.driver)];
            if run.count <= 0.0 {
                run.phase = Phase::Racing;
                run.clock = 0.0;
            }
        }
        Phase::Racing => {
            run.clock += dt;
            if run.rival_done.is_none() && run.rival.prog >= run.dist {
                run.rival_done = Some(run.clock);
            }
            if run.player_done.is_none() && run.player.prog >= run.dist {
                run.player_done = Some(run.clock);
            }
            match (run.player_done, run.rival_done) {
                (Some(pt), Some(rt)) if pt > rt => outcome = outcome.or(Some(Outcome::Lose)),
                (Some(_), _) => outcome = outcome.or(Some(Outcome::Win)),
                (None, Some(rt)) if run.clock - rt > ENCOUNTER_LOSS_GRACE_S => outcome = outcome.or(Some(Outcome::Lose)),
                _ => {}
            }
            hud.title = Some(if run.clock < 1.0 { "GO".to_owned() } else { format!("STREET RACE vs {}", run.driver) });
            hud.lines = vec![
                format!("Finish {:.0} m", (run.dist - run.player.prog).max(0.0)),
                if run.player.prog >= run.rival.prog { "1st" } else { "2nd" }.to_owned(),
                hud::clock(run.clock),
            ];
        }
        Phase::Done(_) => {}
    }

    let Some(outcome) = outcome else { return };
    hud.clear();
    hud.prompt = None;
    nav.end();
    match outcome {
        Outcome::Abort(msg) => {
            if let Some(m) = msg {
                hud.flash(m, 3.0);
            }
            ai.despawn_all();
            st.run = None;
            *act = Activity::None;
            st.roll(ENCOUNTER_DECLINE_COOLDOWN_S);
        }
        Outcome::Win | Outcome::Lose => {
            let won = matches!(outcome, Outcome::Win);
            let driver = run.driver.clone();
            let enc = &mut pay.profile.data.missions.encounters;
            if won {
                enc.won += 1;
                enc.streak += 1;
                enc.best_streak = enc.best_streak.max(enc.streak);
            } else {
                enc.lost += 1;
                enc.streak = 0;
            }
            if won {
                let mults = &missions.data.race_encounter.wristband_multipliers;
                let tier = super::reward::tier(&pay.profile, &events);
                let mult = mults.get(tier).or(mults.last()).copied().unwrap_or(1.0);
                let n = (ENCOUNTER_BASE_CR * mult).round() as i64;
                super::reward::credits(&mut pay.profile, n, "Street race encounter");
                super::reward::popularity(&mut pay.profile, &events, &mut pay.banners, ENCOUNTER_FAME);
                let cr = format!("+{n} CR");
                if super::reward::rewards_on() {
                    hud::notify(&mut pay.notify, &["YOU WON", driver.as_str(), cr.as_str()]);
                } else {
                    hud::notify(&mut pay.notify, &["YOU WON", driver.as_str()]);
                }
            } else {
                hud::notify(&mut pay.notify, &["YOU LOST", driver.as_str()]);
            }
            pay.profile.commit();
            run.phase = Phase::Done(ENCOUNTER_RESULT_S);
        }
    }
}

/// Every frame while an encounter runs (after race.rs wrote its idle values): hold the rival on the grid, tell it who has
/// finished and which line the player's driving line follows.
fn drive_control(st: Res<EncounterState>, control: Option<ResMut<AiRaceControl>>) {
    let (Some(run), Some(mut c)) = (st.run.as_ref(), control) else { return };
    let hold = run.phase == Phase::Countdown;
    let finished = if run.rival_done.is_some() || matches!(run.phase, Phase::Done(_)) { vec![1] } else { Vec::new() };
    let route = Some(route_file(run.route));
    if c.hold != hold || c.finished != finished || c.route_file != route {
        c.hold = hold;
        c.finished = finished;
        c.route_file = route;
    }
}

/// Map switched away from Colorado mid-offer / mid-race: drop everything.
fn abandon(mut st: ResMut<EncounterState>, mut act: ResMut<Activity>, mut hud: ResMut<MissionHud>, mut ai: AiOut, mut nav: Nav) {
    if st.run.is_none() && st.offer.is_none() {
        return;
    }
    let st = &mut *st;
    clear_prompt(&mut hud, &mut st.prompt_ours);
    st.offer = None;
    if st.run.take().is_some() {
        hud.clear();
        nav.end();
        ai.despawn_all();
        if *act == Activity::Encounter {
            *act = Activity::None;
        }
    }
}
