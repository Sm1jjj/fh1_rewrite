//! Horizon Outposts and their missions (docs/MISSIONS.md "Outposts"): speed stunts, PR stunts and photo shoots.
//!
//! VERIFIED (gas_stations.xml, mission_*.xml, TrackRoute NamedTransforms): 10 outposts (`GASSTATION_NNN` TriggerZone:
//! radius, maxMPH, triggerZoneRadius; `OUTPOST_NNN_NODE` = where the game places the car), three missions each
//! (`<Missions>`: speedstunt_NN, photoshoot_NN, prstunt_NN); every mission gives its own car (`Car carID`,
//! `CSetCarToMissionCar`) and starts at its route's `mission_start_NN` (`CLoadRoute route_id`).
//! - Speed stunt: reach `SpeedTrap` (a speed camera) at `Speed` within `Time` (easy / medium / hard); texts "Race to the
//!   Speed Trap and smash the speed target!", "YOU WERE {0} TOO SLOW", "YOU FAILED TO SET A SPEED".
//! - PR stunt: drive to the skills arena (`EntranceZone` left/right nodes), then "Beat the Skills Target along the
//!   suggested route!" (`Popularity` = the target, `TimeLimit` 120 s).
//! - Photo shoot: drive to one of the `PhotoZone`s (radius 50, some with maxMPH) "without wrecking the car" (`Damage`,
//!   `Collisions` low_speed_collision_mph / damage_from_low_speed_collisions), then take a photo with the landmark in it
//!   (`PhotoNodes` min_in_shot of the route's `mission_photo_01_NN` points); fail text "YOU WRECKED YOUR CAR!".
//! INFERRED (docs/MISSIONS.md): units mph / seconds / metres; the outpost menu opens when stopped (< [`STOP_MPH`]) inside
//! the TriggerZone radius; the PR-stunt clock starts at the arena entrance and the score is the skill chains banked
//! there plus the running chain at the bell; the suggested route = the arena's `mission_prstunt_end_NN` points in order;
//! damage = impact speed (mph, horizontal) per collision at or above low_speed_collision_mph, else
//! damage_from_low_speed_collisions (a collision = a horizontal speed loss over [`IMPACT_DV_MPH`] within one frame); the
//! photo counts a landmark point when it projects inside the frame within [`PHOTO_RANGE_M`] and the car is in frame too;
//! stars = difficulty targets met; rewards [`MISSION_CREDITS`] / [`MISSION_FAME`] (FH1's payout is not in the data),
//! replays pay [`REPLAY_SHARE`].
//! Not done: the mission intro cutscenes, time-of-day locks (`CSetTimeOfDay 42600`), fast travel to outposts.
//!
//! P18 (flags, all default on, `=0` = the old behaviour): `FH1_OUTPOST_ABORT` (a mission ends cleanly and the player's own
//! car comes back when the map changes, a cutscene or loading cover starts, the car is swapped by hand or the player is
//! teleported; the missions keep their state across the pause menu / photo mode), `FH1_OUTPOST_MENU` (mission details in
//! the outpost menu, it closes when you drive away), `FH1_PHOTO_CLOSE` (a good shot closes photo mode by itself, a shot
//! needs the photo mode to have been up for a frame, Enter takes it too).

use bevy::ecs::system::SystemParam;
use bevy::input::gamepad::{Gamepad, GamepadButton};
use bevy::prelude::*;

use super::data::Pose;
use super::hud::{clock, notify, speed_text, MissionHud, NavGuard};
use super::map::{IconKind, MapIcon, MissionMapIcons};
use super::save::MissionRecord;
use super::{cancel_held, confirm, crosses_gate, race_running, xz, Activity, Missions, Player, MPH};
use crate::progression::skill::{SkillEvent, Skills};
use crate::Car;

/// INFERRED: the outpost menu needs the car (nearly) stopped (mph).
pub const STOP_MPH: f32 = 8.0;
/// INFERRED: credits for a first completion.
pub const MISSION_CREDITS: i64 = 5_000;
/// INFERRED: popularity for a first completion (speed stunt, photo shoot; a PR stunt's skills already pay popularity).
pub const MISSION_FAME: u64 = 2_500;
/// INFERRED: replays pay this share (as progression.rs REPLAY_SHARE).
pub const REPLAY_SHARE: f32 = 0.5;
/// INFERRED: a horizontal speed loss over this within one frame is a collision (braking can't do it at 60+ fps).
pub const IMPACT_DV_MPH: f32 = 4.0;
/// INFERRED: landmark points further than this don't count in a photo.
pub const PHOTO_RANGE_M: f32 = 2_000.0;
/// INFERRED: the PR arena entrance also counts when passing this close to its midpoint (m).
const ENTRANCE_NEAR_M: f32 = 12.0;
/// Seconds to wait for a mission car (or the player's own) to load.
const CAR_LOAD_TIMEOUT_S: f32 = 25.0;
/// INFERRED: the open menu closes when the car is this far outside the TriggerZone (m) or faster than this (mph).
const MENU_LEAVE_M: f32 = 40.0;
const MENU_LEAVE_MPH: f32 = 25.0;
/// INFERRED: a one-frame move over this (m) is a teleport (fast travel, restart, reset to a safe pose): the mission ends.
const TELEPORT_M: f32 = 150.0;

pub fn outposts_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_OUTPOSTS"))
}

/// `FH1_OUTPOST_ABORT=0`: no watchdog / teleport / car-swap / cutscene aborts, the old gating (nothing runs while paused).
fn abort_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_OUTPOST_ABORT"))
}

/// `FH1_OUTPOST_CARFIX=0`: the old mission start: refuse a car that isn't installed, abort on the loading cover the car
/// swap raises itself, cancel the mission when the car doesn't load.
fn carfix_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_OUTPOST_CARFIX"))
}

/// `FH1_OUTPOST_MENU=0`: the old one-line-per-mission menu that stays up when you drive away.
fn menu_detail_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_OUTPOST_MENU"))
}

/// `FH1_PHOTO_CLOSE=0`: a good shot leaves photo mode to the player; the shot also counts on the frame photo mode opens.
fn photo_close_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_PHOTO_CLOSE"))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Speed(usize),
    Pr(usize),
    Photo(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Success { stars: u8, score: f32 },
    Fail { score: f32 },
    Quit,
}

#[derive(Clone, Debug)]
struct Run {
    kind: Kind,
    name: String,
    outpost: usize,
    /// The player's own car (restored after).
    prev_car: String,
    car: String,
    /// Difficulty column 0..2.
    diff: usize,
    clock: f32,
    last: Option<Vec3>,
    /// Car position of the previous frame, paused or not (teleport detection).
    seen: Option<Vec3>,
    /// Where the own car goes back to (position, yaw) when the mission ended by the player's own teleport / car swap;
    /// None = the outpost's `OUTPOST_NNN_NODE`.
    return_pose: Option<(Vec3, f32)>,
    // Speed stunt.
    best_mph: f32,
    // PR stunt: entered the arena at (clock), banked score, next route point.
    entered: Option<f32>,
    banked: u64,
    path_next: usize,
    // Photo shoot.
    damage: f32,
    last_vel: Option<Vec3>,
    in_zone: bool,
    photo_feedback: Option<String>,
    /// Seconds (not covered by a loading screen) after the start during which the cover / teleport aborts are ignored:
    /// the car swap and the move to the mission start raise those themselves.
    grace: f32,
}

/// The garage car that stands in for `want` (a MediaName like `FER_599XX_10`): exact (case-insensitive), else the same
/// maker with the closest model year, else None. The bool is true for an exact match.
fn resolve_car(garage: &[String], want: &str) -> Option<(usize, bool)> {
    if let Some(i) = garage.iter().position(|c| c.eq_ignore_ascii_case(want)) {
        return Some((i, true));
    }
    let parts = |s: &str| (s.split('_').next().unwrap_or("").to_ascii_lowercase(), s.rsplit('_').next().and_then(|y| y.parse::<i32>().ok()));
    let (maker, year) = parts(want);
    if maker.is_empty() {
        return None;
    }
    let year = year.unwrap_or(0);
    garage
        .iter()
        .enumerate()
        .filter(|(_, c)| parts(c).0 == maker)
        .min_by_key(|(_, c)| (parts(c).1.unwrap_or(0) - year).abs())
        .map(|(i, _)| (i, false))
}

/// Seconds the abort checks stay off after a mission starts.
const START_GRACE_S: f32 = 1.5;

#[derive(Clone, Debug, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Menu {
        outpost: usize,
        cursor: usize,
    },
    /// Waiting for the mission car (s waited).
    Loading(f32),
    Active,
    /// Result shown; waiting for the player's own car back (s waited, swap requested).
    Returning {
        waited: f32,
        sent: bool,
    },
}

#[derive(Resource, Default)]
struct OutpostRun {
    phase: Phase,
    run: Option<Run>,
    cancel_held: f32,
    outcome: Option<Outcome>,
}

pub fn register(app: &mut App) {
    if !outposts_on() {
        return;
    }
    app.init_resource::<OutpostRun>()
        .add_systems(Update, outposts.run_if(super::on_colorado).run_if(crate::ui::driving))
        // The runner also runs paused (menu / photo mode) and under cutscenes: it holds the clock then, and finishes
        // or aborts a mission cleanly (it checks the pause itself).
        .add_systems(Update, missions_tick.after(outposts).run_if(super::on_colorado).run_if(tick_gate))
        // The photo itself is taken in photo mode (ui::driving is false there).
        .add_systems(Update, photo_shot.run_if(super::on_colorado));
    if abort_on() {
        app.add_systems(Update, watchdog);
    }
    // Photo-shoot spots are drawn (missions/markers.rs) only while their shoot runs.
    app.add_systems(Update, marker_focus.run_if(super::on_colorado));
}

fn marker_focus(st: Res<OutpostRun>, mut focus: ResMut<super::markers::MarkerFocus>) {
    let want = match (&st.phase, &st.run) {
        (Phase::Active, Some(r)) if matches!(r.kind, Kind::Photo(_)) => Some(r.name.clone()),
        _ => None,
    };
    if focus.0 != want {
        focus.0 = want;
    }
}

/// With `FH1_OUTPOST_ABORT=0` the runner keeps its old gate (driving only).
fn tick_gate(menu: Res<crate::ui::Menu>, rig: Res<crate::camera::CameraRig>) -> bool {
    abort_on() || (!menu.open && !rig.photo && !crate::ui::loading::blocking() && !crate::cutscene::active())
}

/// The HUD / map / career outputs, grouped (system parameter limit).
#[derive(SystemParam)]
struct Out<'w> {
    hud: ResMut<'w, MissionHud>,
    guard: ResMut<'w, NavGuard>,
    objective: ResMut<'w, crate::ui::notify::Objective>,
    satnav: Option<ResMut<'w, crate::ui::minimap::SatNav>>,
    icons: ResMut<'w, MissionMapIcons>,
    pop: MessageWriter<'w, crate::ui::notify::HudNotify>,
    actions: MessageWriter<'w, crate::ui::GameAction>,
}

#[derive(SystemParam)]
struct Career<'w> {
    profile: ResMut<'w, crate::progression::Profile>,
    events: Res<'w, crate::race::Events>,
    banners: ResMut<'w, crate::progression::Banners>,
    settings: Option<Res<'w, crate::ui::Settings>>,
}

#[derive(SystemParam)]
struct Inputs<'w, 's> {
    time: Res<'w, Time>,
    keys: Res<'w, ButtonInput<KeyCode>>,
    pads: Query<'w, 's, &'static Gamepad>,
}

fn player_mut(cars: &Query<&mut Car>) -> Option<Player> {
    cars.iter().next().map(|c| Player { pos: c.0.position, vel: c.0.velocity, rot: c.0.rotation, media: c.0.data.media_name.clone() })
}

fn ground(track: &crate::track::Track, p: Vec3) -> Vec3 {
    track.ground.ray(p + Vec3::Y * 10.0, Vec3::NEG_Y, 40.0).map_or(p, |h| h.point)
}

fn place(cars: &mut Query<&mut Car>, track: &crate::track::Track, pose: Pose) {
    let g = ground(track, pose.point());
    for mut c in cars.iter_mut() {
        c.0.place(g, pose.yaw);
    }
}

/// Heading as `Vehicle::yaw` (0 = facing -Z).
fn yaw_of(rot: Quat) -> f32 {
    let f = rot * Vec3::NEG_Z;
    (-f.x).atan2(-f.z)
}

fn kind_of(d: &super::MissionData, name: &str) -> Option<Kind> {
    if let Some(i) = d.speed_stunts.iter().position(|m| m.name == name) {
        return Some(Kind::Speed(i));
    }
    if let Some(i) = d.pr_stunts.iter().position(|m| m.name == name) {
        return Some(Kind::Pr(i));
    }
    d.photo_shoots.iter().position(|m| m.name == name).map(Kind::Photo)
}

fn kind_label(d: &super::MissionData, k: Kind) -> String {
    let s = match k {
        Kind::Speed(_) => d.text("IDS_SpeedStunt_01", "SPEED STUNT"),
        Kind::Pr(_) => d.text("IDS_PRStunt_01", "PR STUNT"),
        Kind::Photo(_) => d.text("IDS_PhotoShoot_01", "PHOTO SHOOT"),
    };
    s.to_uppercase()
}

fn mission_car(d: &super::MissionData, k: Kind) -> &super::data::CarRef {
    match k {
        Kind::Speed(i) => &d.speed_stunts[i].car,
        Kind::Pr(i) => &d.pr_stunts[i].car,
        Kind::Photo(i) => &d.photo_shoots[i].car,
    }
}

fn mission_start(d: &super::MissionData, k: Kind) -> Option<Pose> {
    match k {
        Kind::Speed(i) => d.speed_stunts[i].start,
        Kind::Pr(i) => d.pr_stunts[i].start,
        Kind::Photo(i) => d.photo_shoots[i].start,
    }
}

/// Difficulty column from Options "AI difficulty" (Easy 0, Medium 1, Hard / Pro 2).
fn difficulty(settings: &Option<Res<crate::ui::Settings>>) -> usize {
    settings.as_ref().map_or(1, |s| s.ai_difficulty.index().min(2))
}

fn stars(score: f32, targets: [f32; 3]) -> u8 {
    targets.iter().filter(|&&t| score >= t).count() as u8
}

/// The speed camera's name for the player: its IDS text when the install has it, else "SPEED TRAP NN" (the install's
/// `trap_label` is an unresolved `IDS_SpeedCamera_2NN` id).
fn trap_name(d: &super::MissionData, cam: &str, label: &str) -> String {
    let t = d.text(label, "");
    if !t.is_empty() {
        return t.to_uppercase();
    }
    format!("SPEED TRAP {}", cam.rsplit('_').next().unwrap_or(cam))
}

fn or_text(s: &str, fallback: &str) -> String {
    if s.is_empty() {
        fallback.to_owned()
    } else {
        s.to_owned()
    }
}

/// The best score as the menu shows it.
fn best_text(k: Kind, best: f32, metric: bool) -> String {
    match k {
        Kind::Speed(_) if best > 0.0 => speed_text(best, metric),
        Kind::Pr(_) if best > 0.0 => format!("{} SKILL POINTS", crate::progression::fmt_num(best as i64)),
        _ => String::new(),
    }
}

/// Description and target lines of the highlighted mission.
fn detail_lines(d: &super::MissionData, k: Kind, diff: usize, metric: bool) -> Vec<String> {
    match k {
        Kind::Speed(i) => {
            let m = &d.speed_stunts[i];
            vec![
                or_text(&m.instruction, "Race to the Speed Trap and smash the speed target!"),
                format!("TARGET {} WITHIN {:.0} S  -  {}", speed_text(m.speed_mph[diff], metric), m.time_s[diff], trap_name(d, &m.trap, &m.trap_label)),
            ]
        }
        Kind::Pr(i) => {
            let m = &d.pr_stunts[i];
            vec![
                or_text(&m.instruction_to, "Get over to the Skills Arena and put on a show for the crowds!"),
                format!("TARGET {} SKILL POINTS IN {:.0} S", crate::progression::fmt_num(m.target[diff] as i64), m.time_s[diff]),
            ]
        }
        Kind::Photo(i) => {
            let m = &d.photo_shoots[i];
            vec![or_text(&m.requirements, "Take a picture of the car with the landmark in the background."), format!("{}  -  DON'T WRECK THE CAR", m.location.to_uppercase())]
        }
    }
}

/// The car is inside one of the photo zones (and under its maxMPH).
fn in_photo_zone(m: &super::data::PhotoShoot, pos: Vec3, mph: f32) -> bool {
    m.zones.iter().any(|z| xz(pos).distance(Vec2::new(z.pos[0], z.pos[2])) < z.radius.max(10.0) && z.max_mph.is_none_or(|mx| mph <= mx.max(5.0)))
}

fn owns_prompt(hud: &MissionHud) -> bool {
    hud.prompt.as_deref().is_some_and(|s| s.starts_with("A: HORIZON OUTPOST") || s.starts_with("HORIZON OUTPOST"))
}

/// Start giving the own car back (the result is already settled).
fn begin_return(s: &mut OutpostRun, pose: Option<(Vec3, f32)>) {
    if let Some(r) = s.run.as_mut() {
        r.return_pose = pose.or(r.return_pose);
    }
    s.phase = Phase::Returning { waited: 0.0, sent: false };
}

/// Discovery, the outpost prompt and menu, starting a mission.
#[allow(clippy::too_many_arguments)]
fn outposts(
    inp: Inputs,
    missions: Res<Missions>,
    cars: Query<&Car>,
    rs: Option<Res<crate::race::RaceState>>,
    mut activity: ResMut<Activity>,
    mut st: ResMut<OutpostRun>,
    mut career: Career,
    mut out: Out,
    garage: Res<crate::Garage>,
    career_ui: Option<Res<crate::progression::screen::CareerUi>>,
    skills: Option<Res<Skills>>,
) {
    let d = &missions.data;
    let Some(p) = super::player(&cars) else { return };
    if race_running(&rs) {
        // A race took over while the menu was up.
        if matches!(st.phase, Phase::Menu { .. }) {
            st.phase = Phase::Idle;
            *activity = Activity::None;
            out.hud.clear();
            out.hud.prompt = None;
        } else if owns_prompt(&out.hud) {
            out.hud.prompt = None;
        }
        return;
    }

    // Discovery (triggerZoneRadius): XZ distance, any approach, any height (the radius is 70-214 m).
    for o in &d.outposts {
        let r = if o.discover_radius > 0.0 { o.discover_radius } else { o.radius.max(50.0) };
        if xz(p.pos).distance(Vec2::new(o.pos[0], o.pos[2])) < r && !career.profile.data.missions.outposts.contains(&o.name) {
            career.profile.data.missions.outposts.push(o.name.clone());
            career.profile.commit();
            notify(&mut out.pop, &[&d.text("IDS_DiscoverGasStation_Title", "HORIZON OUTPOST DISCOVERED"), &o.title, ""]);
            info!("missions: outpost {} discovered", o.name);
        }
    }

    // The career screen (F6) reads Enter / A itself.
    if career_ui.as_ref().is_some_and(|u| u.open) && st.phase == Phase::Idle {
        if owns_prompt(&out.hud) {
            out.hud.prompt = None;
        }
        return;
    }
    let metric = career.settings.as_ref().is_some_and(|s| s.metric);

    match st.phase.clone() {
        Phase::Idle => {
            if !activity.free() {
                if owns_prompt(&out.hud) {
                    out.hud.prompt = None;
                }
                return;
            }
            let near = d.outposts.iter().position(|o| xz(p.pos).distance(Vec2::new(o.pos[0], o.pos[2])) < o.radius.max(15.0));
            let prompt_owned = owns_prompt(&out.hud);
            match near {
                Some(i) if p.speed_mph() < STOP_MPH && rs.as_ref().is_none_or(|r| r.prompt.is_none()) => {
                    let o = &d.outposts[i];
                    out.hud.prompt = Some(format!("A: HORIZON OUTPOST  -  {}", o.title.to_uppercase()));
                    if confirm(&inp.keys, &inp.pads) {
                        st.phase = Phase::Menu { outpost: i, cursor: 0 };
                        *activity = Activity::Outpost;
                        out.hud.prompt = None;
                    }
                }
                Some(i) => {
                    if p.speed_mph() < d.outposts[i].max_mph.max(STOP_MPH) {
                        out.hud.prompt = Some("HORIZON OUTPOST  -  stop to use it".into());
                    } else if prompt_owned {
                        out.hud.prompt = None;
                    }
                }
                None if prompt_owned => out.hud.prompt = None,
                None => {}
            }
        }
        Phase::Menu { outpost, mut cursor } => {
            let o = &d.outposts[outpost];
            // Driving away closes the menu (it would otherwise keep every other activity locked out).
            let away = xz(p.pos).distance(Vec2::new(o.pos[0], o.pos[2])) > o.radius.max(15.0) + MENU_LEAVE_M || p.speed_mph() > MENU_LEAVE_MPH;
            if menu_detail_on() && away {
                st.phase = Phase::Idle;
                *activity = Activity::None;
                out.hud.clear();
                out.hud.prompt = None;
                return;
            }
            let list: Vec<(String, Option<Kind>)> = o.missions.iter().map(|m| (m.clone(), kind_of(d, m))).collect();
            let n = list.len().max(1);
            let pad = |b: GamepadButton| inp.pads.iter().any(|g| g.just_pressed(b));
            if inp.keys.just_pressed(KeyCode::Tab) || pad(GamepadButton::DPadDown) {
                cursor = (cursor + 1) % n;
            }
            if pad(GamepadButton::DPadUp) {
                cursor = (cursor + n - 1) % n;
            }
            for (k, key) in [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3].iter().enumerate() {
                if inp.keys.just_pressed(*key) && k < n {
                    cursor = k;
                }
            }
            let diff = difficulty(&career.settings);
            out.hud.title = Some(if menu_detail_on() { format!("HORIZON OUTPOST  -  {}", o.title.to_uppercase()) } else { o.title.to_uppercase() });
            let mut lines: Vec<String> = list
                .iter()
                .enumerate()
                .map(|(k, (name, kind))| {
                    let rec = career.profile.data.missions.missions.get(name).cloned().unwrap_or_default();
                    let label = kind.map_or_else(|| name.clone(), |k| kind_label(d, k));
                    let car = kind.map(|k| mission_car(d, k).label()).unwrap_or_default();
                    let state = if rec.completed {
                        let best = if menu_detail_on() { kind.map(|k| best_text(k, rec.best, metric)).unwrap_or_default() } else { String::new() };
                        let best = if best.is_empty() { best } else { format!("  BEST {best}") };
                        format!("  {} DONE{best}", "*".repeat(rec.stars as usize))
                    } else {
                        String::new()
                    };
                    format!("{} {}. {label}  ({car}){state}", if k == cursor { ">" } else { " " }, k + 1)
                })
                .collect();
            if menu_detail_on() {
                if let Some((_, Some(kind))) = list.get(cursor) {
                    lines.push(String::new());
                    lines.extend(detail_lines(d, *kind, diff, metric));
                }
            }
            out.hud.lines = lines;
            out.hud.prompt = Some("Tab / D-pad: choose   A / Enter: start   Backspace / B: leave".into());
            let back = inp.keys.just_pressed(KeyCode::Backspace) || pad(GamepadButton::East);
            if back {
                st.phase = Phase::Idle;
                *activity = Activity::None;
                out.hud.clear();
                out.hud.prompt = None;
                return;
            }
            st.phase = Phase::Menu { outpost, cursor };
            if confirm(&inp.keys, &inp.pads) {
                let Some((name, Some(kind))) = list.get(cursor).cloned() else { return };
                if matches!(kind, Kind::Pr(_)) && skills.is_none() {
                    out.hud.flash("PR STUNTS NEED THE CAREER (FH1_PROGRESSION)", 3.0);
                    return;
                }
                let wanted = mission_car(d, kind).media.clone();
                let exact = wanted.as_deref().and_then(|w| garage.cars.iter().position(|c| *c == w));
                let (media, idx) = match (exact, carfix_on()) {
                    (Some(i), _) => (garage.cars[i].clone(), i),
                    (None, false) => {
                        match &wanted {
                            None => out.hud.flash("MISSION CAR NOT IN GAMEDB", 3.0),
                            Some(m) => out.hud.flash(format!("MISSION CAR {m} NOT INSTALLED"), 3.0),
                        }
                        return;
                    }
                    (None, true) => {
                        // A similar installed car (same maker, closest year), else the player's own: never refuse the mission.
                        let sub = wanted.as_deref().and_then(|w| resolve_car(&garage.cars, w)).map(|(i, _)| i).or_else(|| garage.cars.iter().position(|c| *c == p.media));
                        let Some(i) = sub else {
                            out.hud.flash("MISSION CAR NOT INSTALLED", 3.0);
                            return;
                        };
                        warn!("missions: {} car {:?} ({}) is not installed, using {}", name, wanted, mission_car(d, kind).label(), garage.cars[i]);
                        (garage.cars[i].clone(), i)
                    }
                };
                if p.media != media {
                    out.actions.write(crate::ui::GameAction::SelectCar(idx));
                }
                st.run = Some(Run {
                    kind,
                    name,
                    outpost,
                    prev_car: p.media.clone(),
                    car: media,
                    diff,
                    clock: 0.0,
                    last: None,
                    seen: None,
                    return_pose: None,
                    best_mph: 0.0,
                    entered: None,
                    banked: 0,
                    path_next: 0,
                    damage: 0.0,
                    last_vel: None,
                    in_zone: false,
                    photo_feedback: None,
                    grace: START_GRACE_S,
                });
                st.outcome = None;
                st.cancel_held = 0.0;
                st.phase = Phase::Loading(0.0);
                out.hud.prompt = None;
                out.hud.title = Some(kind_label(d, kind));
                out.hud.lines = vec!["LOADING THE MISSION CAR...".into()];
            }
        }
        _ => {}
    }
    let _ = inp.time.delta_secs();
}

/// Loading the mission car, the running mission, the result and the way back.
#[allow(clippy::too_many_arguments)]
fn missions_tick(
    inp: Inputs,
    missions: Res<Missions>,
    mut cars: Query<&mut Car>,
    track: Res<crate::track::Track>,
    rs: Option<Res<crate::race::RaceState>>,
    mut activity: ResMut<Activity>,
    mut st: ResMut<OutpostRun>,
    mut career: Career,
    mut out: Out,
    mut garage: ResMut<crate::Garage>,
    mut skill_events: MessageReader<SkillEvent>,
    skills: Option<Res<Skills>>,
    menu: Option<Res<crate::ui::Menu>>,
    rig: Option<Res<crate::camera::CameraRig>>,
) {
    let d = &missions.data;
    let dt = inp.time.delta_secs();
    let banked: u64 = skill_events.read().filter_map(|e| if let SkillEvent::Banked { value, .. } = e { Some(*value) } else { None }).sum();
    let Some(p) = player_mut(&cars) else { return };
    let metric = career.settings.as_ref().is_some_and(|s| s.metric);
    // Paused (pause menu, photo mode): the clock and the checks hold, results and the way back still go on.
    let paused = menu.as_ref().is_some_and(|m| m.open) || rig.as_ref().is_some_and(|r| r.photo);
    let blocked = crate::ui::loading::blocking() || crate::cutscene::active();

    match st.phase.clone() {
        Phase::Loading(waited) => {
            let Some(run) = st.run.clone() else {
                st.phase = Phase::Idle;
                *activity = Activity::None;
                return;
            };
            if race_running(&rs) {
                // A race started while the car loaded: no mission, own car back after it.
                out.hud.clear();
                begin_return(&mut st, None);
                return;
            }
            let waited_out = waited > CAR_LOAD_TIMEOUT_S;
            if carfix_on() && waited_out && p.media != run.car {
                // The mission car never arrived: go on in the car that is there instead of cancelling.
                warn!("missions: {} car {} did not load, using {}", run.name, run.car, p.media);
                out.hud.flash("MISSION CAR FAILED TO LOAD - USING YOUR CAR", 3.0);
                if let Some(i) = garage.cars.iter().position(|c| *c == p.media) {
                    garage.current = i;
                }
                if let Some(r) = st.run.as_mut() {
                    r.car = p.media.clone();
                    r.prev_car = p.media.clone();
                }
                st.phase = Phase::Loading(0.0);
                return;
            }
            // The cover of the car swap (a new car entity whose model isn't in yet) must be over before the mission starts.
            if p.media == run.car && !(carfix_on() && blocked && !waited_out) {
                if let Some(pose) = mission_start(d, run.kind) {
                    place(&mut cars, &track, pose);
                }
                if let Some(r) = st.run.as_mut() {
                    r.seen = None;
                    r.last = None;
                }
                st.phase = Phase::Active;
                start_guidance(d, &run, &mut out);
                let (title, line) = intro(d, run.kind);
                notify(&mut out.pop, &[&kind_label(d, run.kind), &line, &title]);
                info!("missions: {} started in {}", run.name, run.car);
            } else if waited > CAR_LOAD_TIMEOUT_S {
                warn!("missions: {} car {} did not load", run.name, run.car);
                out.hud.flash("MISSION CAR FAILED TO LOAD", 3.0);
                out.hud.clear();
                begin_return(&mut st, None);
            } else {
                st.phase = Phase::Loading(waited + dt);
            }
        }
        Phase::Active => {
            let mut held = st.cancel_held;
            let s = &mut *st;
            let Some(run) = s.run.as_mut() else {
                s.phase = Phase::Idle;
                *activity = Activity::None;
                return;
            };
            let mut seen = run.seen.replace(p.pos);
            // Just started: the move to the start and the car swap can raise a cover; that isn't the player's doing.
            let settling = carfix_on() && run.grace > 0.0;
            if settling {
                if !blocked {
                    run.grace -= dt;
                }
                seen = None;
            }
            if s.outcome.is_none() {
                if race_running(&rs) {
                    s.outcome = Some(Outcome::Quit);
                } else if settling {
                    // No abort checks yet.
                } else if abort_on() && (blocked || p.media != run.car || seen.is_some_and(|a| a.distance(p.pos) > TELEPORT_M)) {
                    // Cutscene / loading cover, the car swapped by hand, a teleport: end it here, own car back there.
                    s.outcome = Some(Outcome::Quit);
                    run.return_pose = Some((p.pos, yaw_of(p.rot)));
                    info!("missions: {} aborted (cutscene / car swap / teleport)", run.name);
                }
            }
            let quit_held = cancel_held(&inp.keys, &inp.pads, &mut held, dt);
            s.cancel_held = held;
            if quit_held && !paused && s.outcome.is_none() {
                s.outcome = Some(Outcome::Quit);
            }
            if s.outcome.is_none() && !paused && !(settling && blocked) {
                let skills_live = skills.as_ref().map_or(0, |s| s.chain.value());
                run.clock += dt;
                let last = run.last.replace(p.pos);
                s.outcome = tick(d, run, &p, last, banked, skills_live, metric, &mut out.hud, &mut out.guard, &mut out.objective, out.satnav.as_deref_mut(), &mut out.icons);
            }
            if let Some(outcome) = s.outcome {
                let run = run.clone();
                settle(d, &run, outcome, &mut career, &mut out, metric);
                out.guard.restore(&mut out.objective, out.satnav.as_deref_mut());
                out.icons.set_dynamic(Vec::new());
                out.hud.clear();
                out.hud.prompt = None;
                s.phase = Phase::Returning { waited: 0.0, sent: false };
            }
        }
        Phase::Returning { waited, sent } => {
            let Some(run) = st.run.clone() else {
                st.phase = Phase::Idle;
                *activity = Activity::None;
                return;
            };
            // Not while a race owns the car: swapping it now would respawn it at the start point.
            if race_running(&rs) {
                return;
            }
            let idx = garage.cars.iter().position(|c| *c == run.prev_car);
            let timed_out = sent && waited > CAR_LOAD_TIMEOUT_S;
            if p.media == run.prev_car || idx.is_none() || timed_out {
                if timed_out && p.media != run.prev_car {
                    warn!("missions: own car {} did not come back", run.prev_car);
                }
                // The car on screen is the player's own again: the garage selection must be too (a swap that never
                // loaded would otherwise leave the mission car as the one pause menu / map changes bring back).
                if let Some(i) = idx {
                    if garage.current != i && p.media == run.prev_car {
                        garage.current = i;
                    }
                }
                match run.return_pose {
                    Some((pos, yaw)) => place(&mut cars, &track, Pose { pos: pos.to_array(), yaw }),
                    None => {
                        if let Some(o) = d.outposts.get(run.outpost) {
                            place(&mut cars, &track, o.place);
                        }
                    }
                }
                st.phase = Phase::Idle;
                st.run = None;
                *activity = Activity::None;
            } else if !sent {
                if let Some(i) = idx {
                    out.actions.write(crate::ui::GameAction::SelectCar(i));
                }
                st.phase = Phase::Returning { waited: 0.0, sent: true };
            } else {
                st.phase = Phase::Returning { waited: waited + dt, sent };
            }
        }
        _ => {}
    }
}

/// Always on: the map changed or we left Colorado while an outpost menu / mission was up. Everything is dropped and the
/// garage selection goes back to the player's own car (the new world spawns its car from it, or a swap is requested when
/// that car is already there).
#[allow(clippy::too_many_arguments)]
fn watchdog(
    track: Res<crate::track::Track>,
    generation: Res<crate::ui::world_load::WorldGeneration>,
    mut st: ResMut<OutpostRun>,
    mut activity: ResMut<Activity>,
    mut hud: ResMut<MissionHud>,
    mut guard: ResMut<NavGuard>,
    mut icons: ResMut<MissionMapIcons>,
    mut garage: ResMut<crate::Garage>,
    cars: Query<&Car>,
    mut actions: MessageWriter<crate::ui::GameAction>,
    mut seen_gen: Local<Option<u32>>,
) {
    let changed = seen_gen.replace(generation.0).is_some_and(|g| g != generation.0);
    if st.phase == Phase::Idle {
        return;
    }
    let off = track.id != "colorado";
    if !off && !changed {
        return;
    }
    let run = st.run.take();
    st.phase = Phase::Idle;
    st.outcome = None;
    st.cancel_held = 0.0;
    if *activity == Activity::Outpost {
        *activity = Activity::None;
    }
    hud.reset();
    guard.forget();
    icons.set_dynamic(Vec::new());
    if let Some(run) = run {
        if let Some(i) = garage.cars.iter().position(|c| *c == run.prev_car) {
            if garage.current != i {
                garage.current = i;
            }
            if !off && cars.iter().next().is_some_and(|c| c.0.data.media_name != run.prev_car) {
                actions.write(crate::ui::GameAction::SelectCar(i));
            }
        }
    }
    info!("missions: outpost state dropped (map changed)");
}

fn intro(d: &super::MissionData, k: Kind) -> (String, String) {
    match k {
        Kind::Speed(i) => {
            let m = &d.speed_stunts[i];
            (trap_name(d, &m.trap, &m.trap_label), or_text(&m.instruction, "Race to the Speed Trap and smash the speed target!"))
        }
        Kind::Pr(i) => {
            let m = &d.pr_stunts[i];
            (m.car.label(), or_text(&m.instruction_to, "Get over to the Skills Arena and put on a show for the crowds!"))
        }
        Kind::Photo(i) => {
            let m = &d.photo_shoots[i];
            (m.location.clone(), or_text(&m.instruction_to, "Drive to the location without wrecking the car."))
        }
    }
}

fn mid(a: [f32; 3], b: [f32; 3]) -> Vec2 {
    Vec2::new((a[0] + b[0]) * 0.5, (a[2] + b[2]) * 0.5)
}

/// Objective, satnav and map target at the start.
fn start_guidance(d: &super::MissionData, run: &Run, out: &mut Out) {
    let (target, name) = match run.kind {
        Kind::Speed(i) => {
            let m = &d.speed_stunts[i];
            (d.camera(&m.trap).map(|c| mid(c.left, c.right)), trap_name(d, &m.trap, &m.trap_label))
        }
        Kind::Pr(i) => {
            let m = &d.pr_stunts[i];
            (Some(mid(m.entrance[0], m.entrance[1])), "Skills arena".to_owned())
        }
        Kind::Photo(i) => {
            let m = &d.photo_shoots[i];
            (m.pose.map(|p| xz(p.point())).or_else(|| m.zones.first().map(|z| Vec2::new(z.pos[0], z.pos[2]))), m.location.clone())
        }
    };
    let text = intro(d, run.kind).1;
    out.guard.set(&mut out.objective, out.satnav.as_deref_mut(), Some(text), target);
    if let Some(t) = target {
        out.icons.set_dynamic(vec![MapIcon { key: format!("mission:{}", run.name), kind: IconKind::Target, pos: t, radius: 0.0, name, lines: Vec::new(), ..Default::default() }]);
    }
}

/// One frame of a running mission; Some = it ended.
#[allow(clippy::too_many_arguments)]
fn tick(
    d: &super::MissionData,
    run: &mut Run,
    p: &Player,
    last: Option<Vec3>,
    banked: u64,
    live: u64,
    metric: bool,
    hud: &mut MissionHud,
    guard: &mut NavGuard,
    objective: &mut crate::ui::notify::Objective,
    satnav: Option<&mut crate::ui::minimap::SatNav>,
    icons: &mut MissionMapIcons,
) -> Option<Outcome> {
    let label = kind_label(d, run.kind);
    match run.kind {
        Kind::Speed(i) => {
            let m = &d.speed_stunts[i];
            let (target, limit) = (m.speed_mph[run.diff], m.time_s[run.diff]);
            let left = limit - run.clock;
            hud.title = Some(format!("{label}  {}", clock(left)));
            hud.lines = vec![format!("TARGET {}", speed_text(target, metric))];
            if run.best_mph > 0.0 {
                hud.lines.push(format!("BEST {}", speed_text(run.best_mph, metric)));
            }
            if let (Some(a), Some(cam)) = (last, d.camera(&m.trap)) {
                if a.distance(p.pos) < 60.0 && super::speedtrap::crosses_gate_band(Vec3::from_array(cam.left), Vec3::from_array(cam.right), a, p.pos, 4.0) {
                    let mph = p.speed_mph();
                    run.best_mph = run.best_mph.max(mph);
                    if mph >= target {
                        return Some(Outcome::Success { stars: stars(mph, m.speed_mph), score: mph });
                    }
                    let slow = d.text("IDS_SpeedStuntFailSpeed", "YOU WERE {0} TOO SLOW").replace("{0}", &speed_text(target - mph, metric));
                    hud.flash(slow.to_uppercase(), 3.0);
                }
            }
            if left <= 0.0 {
                return Some(Outcome::Fail { score: run.best_mph });
            }
        }
        Kind::Pr(i) => {
            let m = &d.pr_stunts[i];
            let (target, limit) = (m.target[run.diff], m.time_s[run.diff]);
            match run.entered {
                None => {
                    let ent = mid(m.entrance[0], m.entrance[1]);
                    hud.title = Some(label.clone());
                    hud.lines = vec![format!("SKILLS ARENA  {:.0} m", xz(p.pos).distance(ent))];
                    let crossed = last.is_some_and(|a| a.distance(p.pos) < 60.0 && crosses_gate(Vec3::from_array(m.entrance[0]), Vec3::from_array(m.entrance[1]), a, p.pos, 4.0));
                    if crossed || xz(p.pos).distance(ent) < ENTRANCE_NEAR_M {
                        run.entered = Some(run.clock);
                        run.banked = 0;
                        let text = or_text(&m.instruction_in, "Beat the Skills Target along the suggested route!");
                        hud.flash("GO!", 1.5);
                        let next = m.path.first().map(|q| Vec2::new(q[0], q[2]));
                        guard.set(objective, satnav, Some(text), next);
                    }
                }
                Some(t0) => {
                    run.banked += banked;
                    let left = limit - (run.clock - t0);
                    let shown = run.banked + live;
                    hud.title = Some(format!("{label}  {}", clock(left)));
                    hud.lines = vec![format!("SKILLS {} / {}", crate::progression::fmt_num(shown as i64), crate::progression::fmt_num(target as i64))];
                    // Suggested route: the arena's end points in order.
                    if let Some(q) = m.path.get(run.path_next) {
                        if xz(p.pos).distance(Vec2::new(q[0], q[2])) < 40.0 {
                            run.path_next += 1;
                            let next = m.path.get(run.path_next).map(|q| Vec2::new(q[0], q[2]));
                            if let Some(n) = satnav {
                                n.target = next;
                            }
                            if let Some(n) = next {
                                icons.set_dynamic(vec![MapIcon { key: format!("mission:{}:{}", run.name, run.path_next), kind: IconKind::Target, pos: n, radius: 0.0, name: "Skills route".into(), lines: Vec::new(), ..Default::default() }]);
                            }
                        }
                    }
                    if run.banked as f32 >= target {
                        return Some(Outcome::Success { stars: stars(run.banked as f32, m.target), score: run.banked as f32 });
                    }
                    if left <= 0.0 {
                        // The bell: the running chain counts.
                        let score = shown as f32;
                        return Some(if score >= target { Outcome::Success { stars: stars(score, m.target), score } } else { Outcome::Fail { score } });
                    }
                }
            }
        }
        Kind::Photo(i) => {
            let m = &d.photo_shoots[i];
            // Damage from collisions (module doc). Horizontal speed only: a landing from a jump is not a crash.
            if let Some(v0) = run.last_vel.replace(p.vel) {
                let moved = last.is_some_and(|a| a.distance(p.pos) < 30.0);
                let horizontal = |v: Vec3| Vec2::new(v.x, v.z).length();
                let dv = (horizontal(v0) - horizontal(p.vel)) / MPH;
                if moved && dv > IMPACT_DV_MPH {
                    run.damage += if dv >= m.low_speed_collision_mph { dv } else { m.damage_low_speed };
                }
            }
            let limit = m.damage[run.diff].max(1.0);
            run.in_zone = in_photo_zone(m, p.pos, p.speed_mph());
            hud.title = Some(format!("{label}  {}", m.location.to_uppercase()));
            hud.lines = vec![format!("DAMAGE {:.0}%", (run.damage / limit * 100.0).min(100.0))];
            if run.in_zone {
                hud.lines.push(or_text(&m.instruction_in, "Take a picture of the car with the landmark in the background."));
                hud.lines.push("F: photo mode, then A / F12 to take the photo".into());
            }
            if let Some(f) = &run.photo_feedback {
                hud.lines.push(f.clone());
            }
            if run.damage >= limit {
                hud.flash(d.text("IDS_PhotoShootFail", "YOU WRECKED YOUR CAR!").to_uppercase(), 3.0);
                return Some(Outcome::Fail { score: 0.0 });
            }
        }
    }
    None
}

/// Records, rewards and the result pop-up.
fn settle(d: &super::MissionData, run: &Run, outcome: Outcome, career: &mut Career, out: &mut Out, metric: bool) {
    let label = kind_label(d, run.kind);
    let score = match outcome {
        Outcome::Success { score, .. } | Outcome::Fail { score } => score,
        Outcome::Quit => 0.0,
    };
    // The record first (its borrow ends before the rewards).
    let first = {
        let rec = career.profile.data.missions.missions.entry(run.name.clone()).or_insert_with(MissionRecord::default);
        if outcome != Outcome::Quit {
            rec.runs += 1;
            rec.best = rec.best.max(score);
        }
        let first = !rec.completed;
        if let Outcome::Success { stars, .. } = outcome {
            rec.completed = true;
            rec.stars = rec.stars.max(stars);
        }
        first
    };
    let score_text = match run.kind {
        Kind::Speed(_) => speed_text(score, metric),
        Kind::Pr(_) => format!("{} SKILL POINTS", crate::progression::fmt_num(score as i64)),
        Kind::Photo(_) => String::new(),
    };
    match outcome {
        Outcome::Success { stars, .. } => {
            let share = if first { 1.0 } else { REPLAY_SHARE };
            let credits = (MISSION_CREDITS as f32 * share).round() as i64;
            let fame = if matches!(run.kind, Kind::Pr(_)) { 0 } else { (MISSION_FAME as f32 * share).round() as u64 };
            super::reward::credits(&mut career.profile, credits, &format!("{} {}", label, run.name));
            super::reward::popularity(&mut career.profile, &career.events, &mut career.banners, fame);
            notify(&mut out.pop, &[&format!("{label} COMPLETE"), &score_text, &"*".repeat(stars as usize)]);
            info!("missions: {} complete ({score:.0}, {stars} stars), +{credits} CR +{fame} popularity", run.name);
        }
        Outcome::Fail { .. } => {
            let why = match run.kind {
                Kind::Speed(_) if score <= 0.0 => d.text("IDS_SpeedStuntFailTimeout", "YOU FAILED TO SET A SPEED"),
                Kind::Photo(_) => d.text("IDS_PhotoShootFail", "YOU WRECKED YOUR CAR!"),
                _ => "OUT OF TIME".into(),
            };
            notify(&mut out.pop, &[&format!("{label} FAILED"), &why.to_uppercase(), &score_text]);
            info!("missions: {} failed ({score:.0})", run.name);
        }
        Outcome::Quit => {
            notify(&mut out.pop, &[&format!("{label} ABANDONED"), "", ""]);
            info!("missions: {} quit", run.name);
        }
    }
    career.profile.commit();
}

/// The photo-shoot shot: in photo mode, F12 / A / Enter inside a photo zone. Counts the landmark points in frame.
#[allow(clippy::too_many_arguments)]
fn photo_shot(
    inp: Inputs,
    missions: Res<Missions>,
    rig: Option<ResMut<crate::camera::CameraRig>>,
    cam: Query<(&Camera, &GlobalTransform), With<fh1_render::post::FxPostCamera>>,
    cars: Query<&Car>,
    mut st: ResMut<OutpostRun>,
    mut hud: ResMut<MissionHud>,
    mut was_photo: Local<bool>,
) {
    let Some(mut rig) = rig else { return };
    // The pause menu's "Photo" entry is picked with the same A press that would take the shot: wait one frame.
    let was = std::mem::replace(&mut *was_photo, rig.photo);
    if st.phase != Phase::Active || st.outcome.is_some() || !rig.photo || (photo_close_on() && !was) {
        return;
    }
    let shot = inp.keys.just_pressed(KeyCode::F12) || (photo_close_on() && inp.keys.just_pressed(KeyCode::Enter)) || inp.pads.iter().any(|p| p.just_pressed(GamepadButton::South));
    if !shot {
        return;
    }
    let Some(kind) = st.run.as_ref().map(|r| r.kind) else { return };
    let Kind::Photo(i) = kind else { return };
    let m = &missions.data.photo_shoots[i];
    let Some((camera, gt)) = cam.iter().find(|(c, _)| c.is_active).or_else(|| cam.iter().next()) else { return };
    let Some((car, mph)) = cars.iter().next().map(|c| (c.0.position, c.0.velocity.length() / MPH)) else { return };
    let in_zone = in_photo_zone(m, car, mph);
    let mut success = false;
    let note = if !in_zone {
        "Not at the photo location yet".to_owned()
    } else {
        let size = camera.logical_viewport_size().unwrap_or(Vec2::new(1920.0, 1080.0));
        let eye = gt.translation();
        let fwd = gt.forward().as_vec3();
        let in_frame = |q: Vec3| -> bool {
            if (q - eye).dot(fwd) <= 0.5 || q.distance(eye) > PHOTO_RANGE_M {
                return false;
            }
            camera.world_to_viewport(gt, q).is_ok_and(|v| v.x >= 0.0 && v.y >= 0.0 && v.x <= size.x && v.y <= size.y)
        };
        let seen = m.nodes.iter().filter(|q| in_frame(Vec3::from_array(**q))).count();
        let need = (m.min_in_shot.max(1.0) as usize).min(m.nodes.len().max(1));
        let car_seen = in_frame(car + Vec3::Y * 0.6);
        info!("missions: photo {}: {seen}/{need} landmark points, car in frame {car_seen}", m.name);
        if seen >= need && car_seen {
            success = true;
            st.outcome = Some(Outcome::Success { stars: 3, score: seen as f32 });
            if photo_close_on() {
                format!("GREAT SHOT  ({seen} landmark points)")
            } else {
                format!("GREAT SHOT  ({seen} landmark points)  -  leave photo mode")
            }
        } else if !car_seen {
            "The car must be in the photo".to_owned()
        } else {
            format!("Get more of the landmark in the shot ({seen}/{need})")
        }
    };
    hud.lines.retain(|l| l != "F: photo mode, then A / F12 to take the photo");
    hud.lines.push(note.clone());
    if success && photo_close_on() {
        // Back to driving: the runner settles the result on the next frame.
        rig.photo = false;
        hud.flash("GREAT SHOT!", 2.5);
    }
    if let Some(r) = st.run.as_mut() {
        r.photo_feedback = Some(note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_exact_then_same_maker() {
        let garage = g(&["FER_F50GT_96", "FER_599XX_10", "ALF_8C_08"]);
        assert_eq!(resolve_car(&garage, "fer_599xx_10"), Some((1, true)));
        assert_eq!(resolve_car(&garage, "FER_F430Scuderia_07"), Some((1, false)));
        assert_eq!(resolve_car(&garage, "FER_250GTO_64"), Some((0, false)));
        assert_eq!(resolve_car(&garage, "TVR_Sagaris_05"), None);
    }

    /// Every outpost mission's car against the install (data/installations/<id>/assets/private): in cars/index.json with a
    /// model, physics.json present and loadable. Unresolved ones are listed (and fail the test). Skips without an install.
    #[test]
    fn every_outpost_mission_car_resolves() {
        let data_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let Ok(assets) = fh1_engine::data::private_assets(&data_dir) else {
            eprintln!("no installation, skipped");
            return;
        };
        let Ok(bytes) = std::fs::read(assets.join("cars/index.json")) else {
            eprintln!("no cars group, skipped");
            return;
        };
        let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let garage: Vec<String> = index.as_array().unwrap().iter().filter(|c| c["has_model"].as_bool() == Some(true)).filter_map(|c| c["media_name"].as_str().map(str::to_owned)).collect();
        let d = super::super::MissionData::load(&assets);
        if d.outposts.is_empty() {
            eprintln!("no missions group, skipped");
            return;
        }
        let cars: Vec<(String, &super::super::data::CarRef)> = d
            .speed_stunts
            .iter()
            .map(|m| (m.name.clone(), &m.car))
            .chain(d.pr_stunts.iter().map(|m| (m.name.clone(), &m.car)))
            .chain(d.photo_shoots.iter().map(|m| (m.name.clone(), &m.car)))
            .collect();
        let mut bad = Vec::new();
        for (mission, car) in &cars {
            let Some(media) = car.media.as_deref() else {
                bad.push(format!("{mission}: car id {} not in gamedb", car.id));
                continue;
            };
            if !garage.iter().any(|c| c == media) {
                let sub = resolve_car(&garage, media).map(|(i, _)| garage[i].clone());
                bad.push(format!("{mission}: {media} (id {}) not installed, fallback {sub:?}", car.id));
                continue;
            }
            if let Err(e) = fh1_engine::data::CarData::load(&assets.join("cars").join(media)) {
                bad.push(format!("{mission}: {media} fails to load: {e:#}"));
            }
        }
        assert!(bad.is_empty(), "unresolved outpost mission cars:\n{}", bad.join("\n"));
    }
}
