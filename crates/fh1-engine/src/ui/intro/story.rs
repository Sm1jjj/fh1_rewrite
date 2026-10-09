//! The scripted first-time career (data/extracted/plans/story.md §2 steps 1-10, §5 P1-1..P1-4, changed by the user's request
//! of 2026-10-09): drive to the festival in the Viper, festival arrival, the starter car choice, Race Central arrival and the
//! first wristband. A stage machine over the profile's `story_flags` ("intro_done", "festival_arrived", "starter_chosen",
//! "race_central_seen", "first_wristband"; written at step ends, so a crash resumes at the last unfinished step) and
//! `fmv_seen` (FMV_01 / FMV_02, written as the movie starts like the original's SetMovieSeen).
//!
//! Labels: VERIFIED = read from game_free_roam_flow.xml / first_time_career.xml / cutscenes_*.xml; INFERRED = ours.
//!
//! 1. Intro drive (new career, Colorado, Horizon; after the covers): the Viper (CSetCarForInitialDrive; VERIFIED call, the car is
//!    INFERRED = [`VIPER`]), CPlaceCar (-1361.97, 47.32, 1881.99) rot 94.07 (VERIFIED values; z negated into engine space like
//!    every other CPlaceCar, yaw = -rot, see [`start_pose`]), 09:15 (VERIFIED, 33300) set once and left running, radio off
//!    (VERIFIED), objective "Drive to the festival". The player drives it. NO Opening_Cutscene, NO AI takeover, NO DJ line (the
//!    user removed the opening on 2026-10-09). "intro_done" is written when the control is handed over (so a quit on the way
//!    resumes in the player's saved car, never in the Viper).
//!    Start pose: the flow placement, not the cutscene's (-146.7, 85.2, -1962.5) / 114.2. Checked against `colorado.nav`
//!    (collision space = the raw CPlaceCar values): the flow point is 2.3 m from a type-a road whose height there is 47.0 (the
//!    placement's y is 47.32), the heading 94.07 is within 1 degree of the road's (cos 1.00), and the satnav route from it to
//!    the festival entrance is 3,014 m with the first 120 m straight ahead. The cutscene point is on a road too (0.0 m, y 84.8)
//!    but faces 180 degrees against the festival route (3,110 m): the player would have to turn round. The installed AI racing
//!    lines are no help (the nearest is 249 m away: they only cover the race routes).
//! 2. Festival arrival (after intro_done): within [`ENTRANCE_M`] of a festival entrance ([`FESTIVAL_ENTRANCES`], INFERRED
//!    positions): `Festival_Race_Outro` (loops; stopped at 18.3 s, VERIFIED length), FMV_01, teleport to the workshop
//!    (-1027.26, -8.68, -145.57) rot 100 (VERIFIED), 09:20 (VERIFIED, held through the choice), "festival_arrived".
//! 3. Starter car (after festival_arrived, INFERRED, user request): the choice screen (ui/intro/starter.rs) with
//!    [`pick_starters`] (three cars of the Corrado's class and PI band, never the Corrado itself); on confirm the car is
//!    owned for free (`CarSource::Starter`, 0 CR), the default Corrado that `wallet::migrate` gave the new profile is removed
//!    from `owned` when untouched (no event run; its garage.json look too; `drop_default_starter`), so the chosen car is the
//!    player's only one, the player is switched into it at the workshop, it becomes `settings.car`, "starter_chosen". The Viper is never owned, never `settings.car`, and its
//!    saved look (garage.json) is dropped; see [`is_viper`] / `viper_hygiene`. Then `Radio_Unlock` (8.7 s, VERIFIED), radio
//!    back, "Drive to Race Central to collect your first wristband".
//! 4. Race Central first arrival (after starter_chosen): within 24 m (VERIFIED zone) of `minimap::RACE_CENTRAL`: 08:11
//!    (VERIFIED, 29460), FMV_02, `Wristband_Intro` (33.5 s, VERIFIED), +40,000 CR once ("First wristband", VERIFIED amount),
//!    "race_central_seen" + "first_wristband". The ROOKIE achievement, the satnav to FR02 and the popup are not built.
//!
//! A career made by the build before this change (festival_arrived, no starter_chosen) gets the choice once on its next load:
//! at the workshop with Radio_Unlock when it has not seen Race Central yet, in place and without a teleport when it has
//! (`Seq::late`). A missing asset skips its step (never a soft lock). Every stage runs only in Colorado Horizon with no race
//! and no cover up; leaving that world mid-sequence aborts to Idle (the next visit resumes from the flags).
//! `FH1_INTRO=0` = none of this; `FH1_INTRO=force` = every step once per run on any career, nothing written (the starter is
//! driven for the session but not added to the garage nor saved as `settings.car`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use bevy::prelude::*;
use fh1_engine::vehicle::Controls;
use fh1_render::lighting::FxTimeOfDay;

use super::starter::{self, Starter};
use super::Intro;
use crate::cutscene::{Anchor, CutsceneOpts, Cutscenes};
use crate::progression::data::CarInfo;
use crate::progression::profile::{CarSource, ProfileData};
use crate::progression::Profile;
use crate::race::{Events, RacePhase, RaceState};
use crate::track::Track;
use crate::ui::customize::{CarLook, CarLooks};
use crate::ui::fmv::{Fmv, Target};
use crate::ui::loading::Loading;
use crate::ui::notify::Objective;
use crate::ui::world_load::{GameMode, Mode};
use crate::ui::{GameAction, Menu, Settings, SettingsPath};
use crate::{Car, Garage, Input};

// ---- Data ----

/// CSetCarForInitialDrive's car: INFERRED (the disc names only "Viper"); the first that is in the garage list wins.
pub(super) const VIPER: [&str; 2] = ["VIP_Viper_13", "DOD_ViperSRT10ACRX_12"];
/// The starter pick is built around this car: the game's default (`wallet::STARTER_CARS`, the VW Corrado 95).
pub(super) const STARTER_ANCHOR: &str = crate::progression::wallet::STARTER_CARS[0];
/// CPlaceCar initial drive, raw (VERIFIED, game_free_roam_flow.xml `place_car_initial_drive`). Rotation in degrees.
const START_RAW: (f32, f32, f32) = (-1361.97, 47.32, 1881.99);
const START_ROT_DEG: f32 = 94.07;
/// CPlaceCar at the workshop, raw (VERIFIED, first_time_career.xml).
const WORKSHOP_RAW: (f32, f32, f32) = (-1027.26, -8.68, -145.57);
const WORKSHOP_ROT_DEG: f32 = 100.0;
/// Clock (s): 09:15 intro (VERIFIED 33300), 09:20 after the festival arrival, 08:11 Race Central.
const T_INTRO: f32 = 33_300.0;
const T_FESTIVAL: f32 = 33_600.0;
const T_RACE_CENTRAL: f32 = 29_460.0;

const OUTRO: &str = "Festival_Race_Outro";
const OUTRO_S: f32 = 18.3;
const RADIO_UNLOCK: &str = "Radio_Unlock";
const RADIO_UNLOCK_S: f32 = 8.7;
const WRISTBAND: &str = "Wristband_Intro";
const WRISTBAND_S: f32 = 33.5;

/// Festival entrances (`festival_entrance_01..04`, Colorado/festival_entrances.xml): their positions are in no extracted
/// file (the objects are not in triggers.json: `resolved.match = none`), so these are INFERRED from the road geometry:
/// 01 = TrackRoute170 `route_waypoint_10` (the route the original loads for the intro drive, VERIFIED on the AI lines
/// to 0.2 m) where it crosses the festival's north edge (the DLC centre / car club markers, festival_03/04, sit just south
/// of it); 02 = `route_waypoint_11`, the same road at the festival's south side; 03 and 04 = the first and a middle
/// point of the AI lines inside the grounds (route_016 start, route_110 near the DLC centre), so a player who comes in
/// by any other road is caught before the workshop. Engine (x, z).
pub(super) const FESTIVAL_ENTRANCES: [Vec2; 4] =
    [Vec2::new(-978.04, 377.91), Vec2::new(-951.90, 36.80), Vec2::new(-971.0, 315.0), Vec2::new(-1023.0, 284.0)];
/// INFERRED trigger radius (the zones are radius 10 / 10 mph; a wider ring is used so a fast approach still triggers).
const ENTRANCE_M: f32 = 40.0;
/// Race central's TriggerZone radius (VERIFIED, as ui/notify.rs).
const RACE_CENTRAL_M: f32 = 24.0;
const WRISTBAND_CREDITS: i64 = 40_000;
/// Seconds the world must be calm before the intro starts (as the story movies).
const START_DELAY_S: f32 = 1.0;

pub(super) const F_INTRO: &str = "intro_done";
pub(super) const F_FESTIVAL: &str = "festival_arrived";
pub(super) const F_STARTER: &str = "starter_chosen";
pub(super) const F_CENTRAL: &str = "race_central_seen";
pub(super) const F_WRISTBAND: &str = "first_wristband";
const ALL_FLAGS: [&str; 5] = [F_INTRO, F_FESTIVAL, F_STARTER, F_CENTRAL, F_WRISTBAND];
const FMV_FESTIVAL: &str = "FMV_01";
const FMV_CENTRAL: &str = "FMV_02";

// ---- Pure helpers (unit-tested) ----

/// `FH1_INTRO`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum IntroMode {
    /// `0` / `off`: no scripted intro (the career starts at the festival; FMV_01 / FMV_02 never auto-play).
    Off,
    On,
    /// `force`: every step once per run, on any career, nothing written.
    Force,
}

pub(super) fn mode_from(v: Option<&str>) -> IntroMode {
    match v.map(str::to_ascii_lowercase).as_deref() {
        Some("0") | Some("off") => IntroMode::Off,
        Some("force") => IntroMode::Force,
        _ => IntroMode::On,
    }
}

fn intro_mode() -> IntroMode {
    static M: std::sync::OnceLock<IntroMode> = std::sync::OnceLock::new();
    *M.get_or_init(|| mode_from(std::env::var("FH1_INTRO").ok().as_deref()))
}

/// What a profile is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Career {
    /// No flags, no results, no XP (and no story movie seen): the first-time career.
    New,
    /// Results / XP (or a story movie) but no flags: played before the intro existed.
    Existing,
    /// Has flags: the stage is read from them.
    Flagged,
}

pub(super) fn classify(d: &ProfileData) -> Career {
    if !d.story_flags.is_empty() {
        Career::Flagged
    } else if !d.events.is_empty() || d.xp > 0 || d.fmv_seen.iter().any(|m| m == FMV_FESTIVAL || m == FMV_CENTRAL) {
        // INFERRED: FMV_01 / FMV_02 in fmv_seen = a build before the intro already took the career past the festival arrival.
        Career::Existing
    } else {
        Career::New
    }
}

/// The scripted steps, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Step {
    Intro,
    Festival,
    Starter,
    RaceCentral,
}

/// First step whose flag is missing (None = all done).
pub(super) fn next_step(flags: &[String]) -> Option<Step> {
    let has = |f: &str| flags.iter().any(|x| x == f);
    if !has(F_INTRO) {
        Some(Step::Intro)
    } else if !has(F_FESTIVAL) {
        Some(Step::Festival)
    } else if !has(F_STARTER) {
        Some(Step::Starter)
    } else if !has(F_CENTRAL) {
        Some(Step::RaceCentral)
    } else {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plan {
    /// Nothing to do (off, automation run, or all steps done).
    Skip,
    /// An existing career: write the five flags and both movies silently.
    Migrate,
    Run(Step),
}

/// What to do with this profile. `local` = the flags of a forced run (nothing is read from the profile then).
pub(super) fn plan(mode: IntroMode, automation: bool, d: &ProfileData, local: &[String]) -> Plan {
    match mode {
        IntroMode::Off => Plan::Skip,
        IntroMode::Force => next_step(local).map_or(Plan::Skip, Plan::Run),
        IntroMode::On if automation => Plan::Skip,
        IntroMode::On => match classify(d) {
            Career::Existing => Plan::Migrate,
            Career::New | Career::Flagged => next_step(&d.story_flags).map_or(Plan::Skip, Plan::Run),
        },
    }
}

/// CPlaceCar raw (x, y, z) -> engine: z negated (collision space -> engine, world.rs MIRROR_Z; checked against the
/// post-credits placement, which lands 7 m from Race Central's engine position, and against colorado.nav: the workshop and
/// the intro start both sit on roads in collision space).
pub(super) fn engine_pos(raw: (f32, f32, f32)) -> Vec3 {
    Vec3::new(raw.0, raw.1, -raw.2)
}

/// CPlaceCar rotation (degrees) -> engine yaw (radians about +Y, 0 = facing -Z). INFERRED: the same mirror flips the sign;
/// consistent with track.rs FESTIVAL_HOME (game yaw 133.888 deg = engine yaw -2.33679 rad) and with the road heading at the
/// intro start (see the module doc).
pub(super) fn engine_yaw(raw_deg: f32) -> f32 {
    -raw_deg.to_radians()
}

pub(super) fn near_entrance(p: Vec2) -> bool {
    FESTIVAL_ENTRANCES.iter().any(|e| e.distance(p) < ENTRANCE_M)
}

/// The Viper of the intro drive (any name of [`VIPER`], case-insensitive).
pub(super) fn is_viper(name: &str) -> bool {
    VIPER.iter().any(|v| name.eq_ignore_ascii_case(v))
}

/// The starter offer (INFERRED, the user's request; docs/PROGRESSION.md): [`STARTER_COUNT`] cars from the class of the
/// `anchor` (the game's default car, the VW Corrado 95) with PI within [`STARTER_PI_WINDOW`] of it (widened 30 / 60 / 120 /
/// any while fewer than three qualify), installed, selectable, not unicorn, price > 0 (the buyable cars) and listed in the
/// garage (`listed`). The anchor is only the class / PI reference and is never offered (every new profile already owns it,
/// progression/wallet.rs `migrate`). Order the pool by |PI - anchor PI|, then media name (deterministic), then choose
/// greedily for variety from empty sets: each pick maximises (new drivetrain x 2 + new make x 1) over what is already
/// chosen, ties to the earlier in that order. Anchor missing from the catalog, or no candidate at all: `[anchor]` alone
/// (when listed); nothing listed: empty.
pub(super) fn pick_starters(cars: &HashMap<String, CarInfo>, anchor: &str, listed: &dyn Fn(&str) -> bool) -> Vec<String> {
    let fallback = || if listed(anchor) { vec![anchor.to_owned()] } else { Vec::new() };
    let Some((anchor_key, a)) = cars.iter().find(|(k, _)| k.eq_ignore_ascii_case(anchor)) else {
        return fallback();
    };
    let mut pool: Vec<(u32, &String, &CarInfo)> = Vec::new();
    for window in [STARTER_PI_WINDOW, 30, 60, 120, u32::MAX] {
        pool = cars
            .iter()
            .filter(|(k, i)| {
                !k.eq_ignore_ascii_case(anchor_key)
                    && i.class == a.class
                    && i.pi.abs_diff(a.pi) <= window
                    && i.installed
                    && i.selectable
                    && !i.unicorn
                    && i.price > 0
                    && listed(k)
            })
            .map(|(k, i)| (i.pi.abs_diff(a.pi), k, i))
            .collect();
        if pool.len() >= STARTER_COUNT {
            break;
        }
    }
    if pool.is_empty() {
        return fallback();
    }
    pool.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(y.1)));
    let mut out: Vec<String> = Vec::new();
    let mut makes: HashSet<String> = HashSet::new();
    let mut drives: HashSet<u32> = HashSet::new();
    while out.len() < STARTER_COUNT && !pool.is_empty() {
        let mut best = (0usize, 0u32);
        for (n, (_, _, c)) in pool.iter().enumerate() {
            let score = 2 * u32::from(!drives.contains(&c.drive)) + u32::from(!makes.contains(&c.make.to_lowercase()));
            if n == 0 || score > best.1 {
                best = (n, score);
            }
        }
        let (_, k, c) = pool.remove(best.0);
        makes.insert(c.make.to_lowercase());
        drives.insert(c.drive);
        out.push(k.clone());
    }
    out
}

/// Cars on offer.
pub(super) const STARTER_COUNT: usize = 3;
/// PI band around the anchor's PI (inclusive).
pub(super) const STARTER_PI_WINDOW: u32 = 15;

// ---- State ----

#[derive(Clone, Copy, PartialEq, Debug, Default)]
enum Stage {
    #[default]
    Idle,
    /// A stale Viper is swapped for a safe car before a later step starts; back to Idle once the car changed.
    Swap { old: Option<Entity>, since: f32 },
    IntroSelect { old: Option<Entity>, since: f32 },
    IntroGo { at: f32 },
    IntroPlaced { at: f32 },
    FestDrive,
    FestOutro { since: f32 },
    FestMovie0,
    FestMovie,
    FestPlace,
    /// At the workshop: wait for the teleport's cover, write "festival_arrived", open the choice.
    FestSettle { at: f32 },
    /// The choice screen is up.
    Choose,
    ChooseSwap { old: Option<Entity>, since: f32 },
    ChoosePlace,
    FestPlaced { at: f32 },
    FestRadio { since: f32 },
    FestEnd,
    /// A late choice (career past Race Central): nothing follows it.
    ChooseEnd,
    RcDrive,
    RcMovie0,
    RcMovie,
    RcWrist0,
    RcWrist { since: f32 },
    RcEnd,
    Done,
}

/// Who drives the player's car.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Drive {
    /// The human.
    #[default]
    Off,
    /// Brakes (cutscene / movie / choice running).
    Hold,
}

#[derive(Resource, Default)]
pub(super) struct Seq {
    stage: Stage,
    calm_since: Option<f32>,
    drive: Drive,
    /// `FH1_INTRO=force`: flags in `local`, nothing written to the profile.
    force: bool,
    local: Vec<String>,
    tod_hold: Option<f32>,
    saved_rate: Option<f32>,
    radio_off: bool,
    logged_done: bool,
    /// The choice of a career that is already past Race Central (made by the previous build): no teleport, no Radio_Unlock.
    late: bool,
    /// Where the car stood when a late choice opened (the car swap respawns it at the spawn point).
    keep_pose: Option<(Vec3, f32)>,
}

/// The intro keeps the radio off (VERIFIED: radio off until Radio_Unlock). Read by radio.rs `radio_volume`.
static RADIO_OFF: AtomicBool = AtomicBool::new(false);

pub fn radio_off() -> bool {
    RADIO_OFF.load(Ordering::Relaxed)
}

fn opts() -> CutsceneOpts {
    CutsceneOpts { skippable: true, anchor: Anchor::PlayerCar, hide_hud: true, ..Default::default() }
}

fn has_flag(seq: &Seq, profile: &Option<ResMut<Profile>>, flag: &str) -> bool {
    if seq.force {
        seq.local.iter().any(|f| f == flag)
    } else {
        profile.as_deref().is_some_and(|p| p.data.story_flags.iter().any(|f| f == flag))
    }
}

fn set_flag(seq: &mut Seq, profile: &mut Option<ResMut<Profile>>, flag: &str) {
    if seq.force {
        if !seq.local.iter().any(|f| f == flag) {
            seq.local.push(flag.to_owned());
        }
    } else if let Some(p) = profile.as_deref_mut() {
        if !p.data.story_flags.iter().any(|f| f == flag) {
            p.data.story_flags.push(flag.to_owned());
            p.commit();
        }
    }
}

fn movie_seen(seq: &Seq, profile: &Option<ResMut<Profile>>, name: &str) -> bool {
    !seq.force && profile.as_deref().is_some_and(|p| p.data.fmv_seen.iter().any(|m| m == name))
}

/// Starts a story movie (marked seen as it starts, like the original). False = skipped (movies off, missing, ...).
fn play_movie(seq: &Seq, fmv: &mut Fmv, profile: &mut Option<ResMut<Profile>>, name: &str, volume: f32) -> bool {
    if !super::story_on() || !fmv.available() || !fmv.play(name, Target::FullScreen, false, volume) {
        return false;
    }
    if !seq.force {
        if let Some(p) = profile.as_deref_mut() {
            if !p.data.fmv_seen.iter().any(|m| m == name) {
                p.data.fmv_seen.push(name.to_owned());
                p.commit();
            }
        }
    }
    true
}

fn set_objective(obj: &mut Option<ResMut<Objective>>, text: Option<&str>, target: Option<Vec2>) {
    if let Some(o) = obj.as_deref_mut() {
        o.text = text.map(str::to_owned);
        o.target = target;
    }
}

/// Ground under (x, z) from above, else `fallback`.
fn ground_at(track: &Track, x: f32, z: f32, fallback: Vec3) -> Vec3 {
    track.ground.ray(Vec3::new(x, 500.0, z), Vec3::NEG_Y, 2000.0).map_or(fallback, |h| h.point)
}

/// The intro start on the ground and its yaw: the flow's CPlaceCar (see the module doc for why it, not the cutscene's).
fn start_pose(track: &Track) -> (Vec3, f32) {
    let p = engine_pos(START_RAW);
    (ground_at(track, p.x, p.z, p), engine_yaw(START_ROT_DEG))
}

fn workshop(track: &Track) -> (Vec3, f32) {
    let p = engine_pos(WORKSHOP_RAW);
    (ground_at(track, p.x, p.z, p), engine_yaw(WORKSHOP_ROT_DEG))
}

fn place(cars: &mut Query<(Entity, &mut Car)>, point: Vec3, yaw: f32) {
    for (_, mut car) in cars.iter_mut() {
        car.0.place(point, yaw);
    }
}

/// Holds the player's car on the brakes while a movie / cutscene / the starter choice is up (after `read_input`, before the
/// physics tick).
pub(super) fn hold_input(seq: Res<Seq>, mut input: ResMut<Input>, settings: Res<Settings>, cars: Query<&Car>) {
    if seq.drive != Drive::Hold || cars.single().is_err() {
        return;
    }
    input.0 = Controls { brake: 1.0, tcs: settings.tcs, abs: settings.abs, ..Controls::default() };
}

// ---- The Viper never stays ----

fn viper_owned(p: &Profile) -> bool {
    VIPER.iter().any(|v| crate::progression::wallet::owns(p, v))
}

/// A car to be in instead of the Viper: the saved `settings.car`, then the owned cars in order, then the anchor; the first
/// that is in the garage list and is not a Viper.
fn safe_car(garage: &Garage, profile: &Option<ResMut<Profile>>, settings: &Settings) -> Option<usize> {
    let owned = profile.as_deref().map(|p| p.data.owned.iter().map(|o| o.car.as_str()).collect::<Vec<_>>()).unwrap_or_default();
    settings
        .car
        .iter()
        .map(String::as_str)
        .chain(owned)
        .chain(std::iter::once(STARTER_ANCHOR))
        .filter(|c| !is_viper(c))
        .find_map(|c| garage.cars.iter().position(|g| g.eq_ignore_ascii_case(c)))
}

fn save_settings(settings: &Settings, path: &SettingsPath) {
    if let Some(dir) = path.0.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_vec_pretty(settings) {
        Ok(b) => crate::perf::writer::replace(path.0.clone(), b),
        Err(e) => warn!("intro: saving settings: {e}"),
    }
}

/// Sets and saves the remembered car (settings.json `car`) when it changes.
fn remember_car(settings: &mut Settings, path: &SettingsPath, car: Option<String>) {
    if settings.car != car {
        settings.car = car;
        save_settings(settings, path);
    }
}

/// The choice replaces the default Corrado that `wallet::migrate` gave the new profile: it leaves `owned` when it is still
/// the untouched starter (source Starter, paid 0, no event ever run) and is not the car just chosen. Does not save (the
/// caller's flag write does).
fn drop_default_starter(p: &mut Profile, chosen: &str) {
    if !p.data.events.is_empty() || STARTER_ANCHOR.eq_ignore_ascii_case(chosen) {
        return;
    }
    let before = p.data.owned.len();
    p.data.owned.retain(|o| !(o.source == CarSource::Starter && o.paid == 0 && o.car.eq_ignore_ascii_case(STARTER_ANCHOR)));
    if p.data.owned.len() != before {
        info!("intro: the default {STARTER_ANCHOR} is replaced by {chosen}");
    }
}

/// The dropped Corrado's saved look (garage.json) goes too, unless it is owned again.
fn purge_default_look(looks: &mut CarLooks, profile: &Option<ResMut<Profile>>, chosen: &str) {
    if STARTER_ANCHOR.eq_ignore_ascii_case(chosen) || profile.as_deref().is_some_and(|p| crate::progression::wallet::owns(p, STARTER_ANCHOR)) {
        return;
    }
    if looks.saved(STARTER_ANCHOR).is_some() {
        looks.commit(STARTER_ANCHOR, CarLook::default());
    }
}

/// Drops a Viper's saved look (garage.json) unless the player really owns that Viper (bought it in the Autoshow).
fn purge_viper_looks(looks: &mut CarLooks, profile: &Option<ResMut<Profile>>) {
    let owns = |v: &str| profile.as_deref().is_some_and(|p| crate::progression::wallet::owns(p, v));
    for v in VIPER {
        if !owns(v) && looks.saved(v).is_some() {
            looks.commit(v, CarLook::default());
            info!("intro: dropped the saved look of {v} (garage.json)");
        }
    }
}

// ---- The sequence ----

fn open_choice(starter: &mut Starter, garage: &Garage, events: &Option<Res<Events>>) -> bool {
    let fallback;
    let career = match events.as_deref() {
        Some(e) => &e.career,
        None => {
            fallback = crate::progression::data::CareerData::default();
            &fallback
        }
    };
    let picks = starter::build(garage, career);
    if picks.is_empty() {
        return false;
    }
    info!("intro: starter choice: {}", picks.iter().map(|p| garage.cars[p.index].as_str()).collect::<Vec<_>>().join(", "));
    starter.open(picks);
    true
}

fn release_clock(seq: &mut Seq, tod: &mut Option<ResMut<FxTimeOfDay>>) {
    seq.tod_hold = None;
    if let (Some(r), Some(t)) = (seq.saved_rate.take(), tod.as_deref_mut()) {
        t.rate_scale = r;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sequence(
    mut seq: ResMut<Seq>,
    mut intro: ResMut<Intro>,
    mut fmv: ResMut<Fmv>,
    mut cuts: Option<ResMut<Cutscenes>>,
    (ld, garage, track, mut settings, time): (Res<Loading>, Res<Garage>, Res<Track>, ResMut<Settings>, Res<Time<Real>>),
    (menu, mode, race, events): (Option<Res<Menu>>, Option<Res<GameMode>>, Option<Res<RaceState>>, Option<Res<Events>>),
    mut profile: Option<ResMut<Profile>>,
    mut cars: Query<(Entity, &mut Car)>,
    mut actions: MessageWriter<GameAction>,
    mut obj: Option<ResMut<Objective>>,
    mut tod: Option<ResMut<FxTimeOfDay>>,
    (mut starter, path, mut looks): (ResMut<Starter>, Res<SettingsPath>, ResMut<CarLooks>),
) {
    let now = time.elapsed_secs();
    let volume = settings.ui_volume.clamp(0.0, 1.0);
    let ended_movie = intro.ended.take();
    let cs_end = cuts.as_deref_mut().and_then(|c| c.take_ended());
    let cs_over = |name: &str| cs_end.as_ref().is_some_and(|(n, _)| n == name);
    let _ = ended_movie;

    // The Viper is never the remembered car (settings.json `car`): a menu pick of it (ownership off aside) or a stale value
    // is replaced by the first owned car. Nothing else writes the current car to disk (docs/PROGRESSION.md).
    if !seq.force && crate::progression::wallet::ownership_on() && settings.car.as_deref().is_some_and(is_viper) {
        let owns = profile.as_deref().is_some_and(viper_owned);
        if !owns {
            let fix = safe_car(&garage, &profile, &settings).map(|i| garage.cars[i].clone());
            info!("intro: settings.car was {:?}; now {fix:?}", settings.car);
            remember_car(&mut settings, &path, fix);
        }
    }

    // Clock hold.
    if let (Some(secs), Some(t)) = (seq.tod_hold, tod.as_deref_mut()) {
        if seq.saved_rate.is_none() {
            seq.saved_rate = Some(t.rate_scale);
        }
        t.seconds = secs;
        t.rate_scale = 0.0;
    }

    let world_ok = track.id == "colorado" && mode.as_deref().is_none_or(|m| m.0 == Mode::Horizon);
    let calm = world_ok
        && race.as_deref().is_none_or(|r| r.race.is_none() && r.phase == RacePhase::Idle)
        && ld.cover.is_none()
        && fmv.playing().is_none()
        && !crate::cutscene::active()
        && !menu.as_deref().is_some_and(|m| m.open)
        && crate::ui::fmv::dev_movie().is_none()
        && cars.iter().next().is_some();
    seq.calm_since = if calm { Some(seq.calm_since.unwrap_or(now)) } else { None };
    let settled = seq.calm_since.is_some_and(|t| now - t >= START_DELAY_S);
    let car_xz = cars.iter().next().map(|(_, c)| Vec2::new(c.0.position.x, c.0.position.z));

    // Leaving Colorado free roam mid-sequence aborts it (the flags resume it next time).
    if !world_ok && !matches!(seq.stage, Stage::Idle | Stage::Done) {
        info!("intro: aborted at {:?} (left Colorado free roam)", seq.stage);
        if let Some(c) = cuts.as_deref_mut() {
            if c.playing().is_some() {
                c.stop();
            }
        }
        starter.close();
        seq.late = false;
        seq.drive = Drive::Off;
        release_clock(&mut seq, &mut tod);
        seq.stage = Stage::Idle;
    }

    let stage = seq.stage;
    match stage {
        Stage::Idle => {
            let Some(data) = profile.as_deref().map(|p| &p.data) else { return };
            seq.force = intro_mode() == IntroMode::Force;
            match plan(intro_mode(), super::super::launch::automation(), data, &seq.local) {
                Plan::Skip => {
                    seq.radio_off = false;
                    seq.stage = Stage::Done;
                }
                Plan::Migrate => {
                    if let Some(p) = profile.as_deref_mut() {
                        for f in ALL_FLAGS {
                            if !p.data.story_flags.iter().any(|x| x == f) {
                                p.data.story_flags.push(f.to_owned());
                            }
                        }
                        for m in [FMV_FESTIVAL, FMV_CENTRAL] {
                            if !p.data.fmv_seen.iter().any(|x| x == m) {
                                p.data.fmv_seen.push(m.to_owned());
                            }
                        }
                        p.commit();
                        info!("intro: existing career: story flags and FMV_01 / FMV_02 marked done silently");
                    }
                    seq.stage = Stage::Done;
                }
                Plan::Run(Step::Intro) => {
                    if !settled {
                        return;
                    }
                    seq.radio_off = true;
                    set_objective(&mut obj, None, None);
                    let want = garage.cars.iter().position(|c| is_viper(c));
                    let old = cars.iter().next().map(|(e, _)| e);
                    match want {
                        Some(i) if i != garage.current => {
                            info!("intro: selecting {} (CSetCarForInitialDrive)", garage.cars[i]);
                            actions.write(GameAction::SelectCar(i));
                            seq.stage = Stage::IntroSelect { old, since: now };
                        }
                        Some(_) => seq.stage = Stage::IntroGo { at: now - 1.0 },
                        None => {
                            warn!("intro: no Viper in the garage list ({VIPER:?}); keeping the current car");
                            seq.stage = Stage::IntroGo { at: now - 1.0 };
                        }
                    }
                }
                Plan::Run(step) => {
                    // Every later step starts in a car that is not the leftover Viper of an earlier session.
                    let stale = !seq.force
                        && garage.cars.get(garage.current).is_some_and(|c| is_viper(c))
                        && !profile.as_deref().is_some_and(viper_owned);
                    if stale {
                        if let Some(i) = safe_car(&garage, &profile, &settings) {
                            info!("intro: leaving the Viper for {}", garage.cars[i]);
                            actions.write(GameAction::SelectCar(i));
                            seq.stage = Stage::Swap { old: cars.iter().next().map(|(e, _)| e), since: now };
                            return;
                        }
                    }
                    match step {
                        Step::Intro => {}
                        Step::Festival => {
                            seq.radio_off = true;
                            set_objective(&mut obj, Some("Drive to the festival"), Some(FESTIVAL_ENTRANCES[0]));
                            seq.stage = Stage::FestDrive;
                        }
                        Step::Starter => {
                            if !settled {
                                return;
                            }
                            // A career past Race Central (previous build): in place, no radio story. Else back at the workshop.
                            seq.late = has_flag(&seq, &profile, F_CENTRAL);
                            seq.radio_off = !seq.late;
                            set_objective(&mut obj, None, None);
                            seq.drive = Drive::Hold;
                            if seq.late {
                                seq.keep_pose = cars.iter().next().map(|(_, c)| {
                                    let yaw = c.0.rotation.to_euler(EulerRot::YXZ).0;
                                    (c.0.position, yaw)
                                });
                                if open_choice(&mut starter, &garage, &events) {
                                    seq.stage = Stage::Choose;
                                } else {
                                    warn!("intro: no starter cars to offer");
                                    seq.drive = Drive::Off;
                                    seq.stage = Stage::Done;
                                }
                            } else {
                                let (point, yaw) = workshop(&track);
                                place(&mut cars, point, yaw);
                                seq.tod_hold = Some(T_FESTIVAL);
                                info!("intro: resuming the starter choice at the workshop, 09:20");
                                seq.stage = Stage::FestSettle { at: now };
                            }
                        }
                        Step::RaceCentral => {
                            set_objective(&mut obj, Some("Drive to Race Central to collect your first wristband"), Some(crate::ui::minimap::RACE_CENTRAL));
                            seq.stage = Stage::RcDrive;
                        }
                    }
                }
            }
        }

        Stage::Swap { old, since } => {
            let now_car = cars.iter().next().map(|(e, _)| e);
            if (now_car.is_some() && now_car != old) || now - since > 20.0 {
                seq.stage = Stage::Idle;
            }
        }

        // ---- Step 1: the intro drive (the player's own) ----
        Stage::IntroSelect { old, since } => {
            let now_car = cars.iter().next().map(|(e, _)| e);
            if (now_car.is_some() && now_car != old) || now - since > 20.0 {
                seq.stage = Stage::IntroGo { at: now };
            }
        }
        Stage::IntroGo { at } => {
            // The car swap's own cover (ui/loading.rs Car) has to come and go first.
            if now - at < 0.5 || !calm {
                return;
            }
            let (point, yaw) = start_pose(&track);
            place(&mut cars, point, yaw);
            info!("intro: placed at {point} yaw {yaw:.3}, 09:15");
            // 09:15 once, then the clock runs (no hold).
            if let Some(t) = tod.as_deref_mut() {
                t.seconds = T_INTRO;
            }
            seq.drive = Drive::Hold;
            seq.stage = Stage::IntroPlaced { at: now };
        }
        Stage::IntroPlaced { at } => {
            // The teleport's Travel cover (new scenery) comes up a frame or two later and goes when the area is loaded.
            if now - at < 0.8 || !calm {
                return;
            }
            seq.drive = Drive::Off;
            set_flag(&mut seq, &mut profile, F_INTRO);
            set_objective(&mut obj, Some("Drive to the festival"), Some(FESTIVAL_ENTRANCES[0]));
            info!("intro: control handed over");
            seq.stage = Stage::FestDrive;
        }

        // ---- Step 2: festival arrival ----
        Stage::FestDrive => {
            if calm && car_xz.is_some_and(near_entrance) {
                info!("intro: festival entrance reached");
                seq.drive = Drive::Hold;
                if cuts.as_deref_mut().is_some_and(|c| c.play(OUTRO, opts())) {
                    seq.stage = Stage::FestOutro { since: now };
                } else {
                    info!("intro: {OUTRO} missing, skipped");
                    seq.stage = Stage::FestMovie0;
                }
            }
        }
        Stage::FestOutro { since } => {
            // The outro loops: it is cut at its authored length (VERIFIED 18.3 s) unless the player skips first.
            let t = cuts.as_deref().map_or(0.0, |c| c.time());
            let gone = cuts.as_deref().is_some_and(|c| c.playing().is_none()) && now - since > 1.0;
            if t >= OUTRO_S && !gone && !cs_over(OUTRO) {
                if let Some(c) = cuts.as_deref_mut() {
                    c.stop();
                }
                seq.stage = Stage::FestMovie0;
            } else if cs_over(OUTRO) || gone {
                seq.stage = Stage::FestMovie0;
            }
        }
        Stage::FestMovie0 => {
            if !movie_seen(&seq, &profile, FMV_FESTIVAL) && play_movie(&seq, &mut fmv, &mut profile, FMV_FESTIVAL, volume) {
                seq.stage = Stage::FestMovie;
            } else {
                seq.stage = Stage::FestPlace;
            }
        }
        Stage::FestMovie => {
            // Skipping it (flow() stops it on a press) still teleports.
            if fmv.playing().is_none() {
                seq.stage = Stage::FestPlace;
            }
        }
        Stage::FestPlace => {
            let (point, yaw) = workshop(&track);
            place(&mut cars, point, yaw);
            // 09:20, held through the choice and Radio_Unlock; released in FestEnd.
            seq.tod_hold = Some(T_FESTIVAL);
            if let Some(t) = tod.as_deref_mut() {
                t.seconds = T_FESTIVAL;
            }
            seq.drive = Drive::Hold;
            info!("intro: teleported to the workshop, 09:20");
            seq.stage = Stage::FestSettle { at: now };
        }

        // ---- Step 3: the starter car ----
        Stage::FestSettle { at } => {
            if now - at < 0.8 || !calm {
                return;
            }
            set_flag(&mut seq, &mut profile, F_FESTIVAL);
            if open_choice(&mut starter, &garage, &events) {
                seq.stage = Stage::Choose;
            } else {
                warn!("intro: no starter cars to offer; going on");
                seq.stage = Stage::FestPlaced { at: now - 1.0 };
            }
        }
        Stage::Choose => {
            seq.drive = Drive::Hold;
            if !starter.is_open() {
                // Closed from outside (left the world and came back through Idle): nothing to do.
                seq.stage = Stage::Idle;
            } else if let Some(i) = starter.take_choice() {
                let name = garage.cars[i].clone();
                info!("intro: starter car chosen: {name}");
                if !seq.force {
                    // Owned and free through the existing ownership path (missions::reward::grant_car = wallet's add, no
                    // credits), remembered for the next launch, the Viper's look dropped. One save with the flag.
                    if let Some(p) = profile.as_deref_mut() {
                        crate::missions::reward::grant_car(p, &name, CarSource::Starter);
                        drop_default_starter(p, &name);
                    }
                    remember_car(&mut settings, &path, Some(name.clone()));
                    purge_viper_looks(&mut looks, &profile);
                    purge_default_look(&mut looks, &profile, &name);
                }
                set_flag(&mut seq, &mut profile, F_STARTER);
                starter.close();
                if i != garage.current {
                    actions.write(GameAction::SelectCar(i));
                    seq.stage = Stage::ChooseSwap { old: cars.iter().next().map(|(e, _)| e), since: now };
                } else {
                    seq.stage = Stage::ChoosePlace;
                }
            }
        }
        Stage::ChooseSwap { old, since } => {
            let now_car = cars.iter().next().map(|(e, _)| e);
            if (now_car.is_some() && now_car != old) || now - since > 20.0 {
                seq.stage = Stage::ChoosePlace;
            }
        }
        Stage::ChoosePlace => {
            // The car swap's own cover has to go first.
            if ld.cover.is_some() || cars.iter().next().is_none() {
                return;
            }
            if seq.late {
                if let Some((pos, yaw)) = seq.keep_pose.take() {
                    let point = ground_at(&track, pos.x, pos.z, pos);
                    place(&mut cars, point, yaw);
                }
                seq.stage = Stage::ChooseEnd;
            } else {
                // The new car spawns at the spawn point: back to the workshop.
                let (point, yaw) = workshop(&track);
                place(&mut cars, point, yaw);
                seq.stage = Stage::FestPlaced { at: now };
            }
        }
        Stage::ChooseEnd => {
            seq.drive = Drive::Off;
            seq.radio_off = false;
            info!("intro: late starter choice done");
            seq.stage = Stage::Done;
        }
        Stage::FestPlaced { at } => {
            if now - at < 0.8 || !calm {
                return;
            }
            if cuts.as_deref_mut().is_some_and(|c| c.play(RADIO_UNLOCK, opts())) {
                info!("intro: VO RadioFixedByDak (silent)");
                seq.stage = Stage::FestRadio { since: now };
            } else {
                info!("intro: {RADIO_UNLOCK} missing, skipped");
                seq.stage = Stage::FestEnd;
            }
        }
        Stage::FestRadio { since } => {
            let t = cuts.as_deref().map_or(0.0, |c| c.time());
            let gone = cuts.as_deref().is_some_and(|c| c.playing().is_none()) && now - since > 1.0;
            if t > RADIO_UNLOCK_S + 4.0 {
                if let Some(c) = cuts.as_deref_mut() {
                    c.stop();
                }
            }
            if cs_over(RADIO_UNLOCK) || gone {
                seq.stage = Stage::FestEnd;
            }
        }
        Stage::FestEnd => {
            seq.drive = Drive::Off;
            seq.radio_off = false;
            release_clock(&mut seq, &mut tod);
            set_flag(&mut seq, &mut profile, F_FESTIVAL);
            set_objective(&mut obj, Some("Drive to Race Central to collect your first wristband"), Some(crate::ui::minimap::RACE_CENTRAL));
            info!("intro: festival arrival done");
            seq.stage = Stage::RcDrive;
        }

        // ---- Step 4: Race Central and the first wristband ----
        Stage::RcDrive => {
            if calm && car_xz.is_some_and(|p| p.distance(crate::ui::minimap::RACE_CENTRAL) < RACE_CENTRAL_M) {
                info!("intro: Race Central reached, 08:11");
                if let Some(t) = tod.as_deref_mut() {
                    t.seconds = T_RACE_CENTRAL;
                }
                seq.drive = Drive::Hold;
                seq.stage = Stage::RcMovie0;
            }
        }
        Stage::RcMovie0 => {
            if !movie_seen(&seq, &profile, FMV_CENTRAL) && play_movie(&seq, &mut fmv, &mut profile, FMV_CENTRAL, volume) {
                seq.stage = Stage::RcMovie;
            } else {
                seq.stage = Stage::RcWrist0;
            }
        }
        Stage::RcMovie => {
            if fmv.playing().is_none() {
                seq.stage = Stage::RcWrist0;
            }
        }
        Stage::RcWrist0 => {
            if cuts.as_deref_mut().is_some_and(|c| c.play(WRISTBAND, opts())) {
                info!("intro: DJ + PaintShop VO for the first wristband (silent)");
                seq.stage = Stage::RcWrist { since: now };
            } else {
                info!("intro: {WRISTBAND} missing, skipped");
                seq.stage = Stage::RcEnd;
            }
        }
        Stage::RcWrist { since } => {
            let t = cuts.as_deref().map_or(0.0, |c| c.time());
            let gone = cuts.as_deref().is_some_and(|c| c.playing().is_none()) && now - since > 1.0;
            if t > WRISTBAND_S + 4.0 {
                if let Some(c) = cuts.as_deref_mut() {
                    c.stop();
                }
            }
            if cs_over(WRISTBAND) || gone {
                seq.stage = Stage::RcEnd;
            }
        }
        Stage::RcEnd => {
            seq.drive = Drive::Off;
            // Once (VERIFIED amount; the flag and the credit are saved together).
            if !seq.force && !has_flag(&seq, &profile, F_WRISTBAND) {
                if let Some(p) = profile.as_deref_mut() {
                    crate::progression::wallet::apply(p, WRISTBAND_CREDITS, "First wristband");
                }
                set_flag(&mut seq, &mut profile, F_WRISTBAND);
            }
            set_flag(&mut seq, &mut profile, F_CENTRAL);
            set_objective(&mut obj, None, None);
            info!("intro: first wristband done");
            seq.stage = Stage::Done;
        }

        Stage::Done => {
            if !seq.logged_done {
                seq.logged_done = true;
                info!("intro: nothing (more) to do");
            }
        }
    }
    RADIO_OFF.store(seq.radio_off, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progression::profile::EventRecord;

    fn flags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn car(class: u32, pi: u32, drive: u32, make: &str) -> CarInfo {
        CarInfo { class, pi, drive, year: 1990, make: make.into(), name: String::new(), selectable: true, installed: true, price: 10_000, unicorn: false }
    }

    fn catalog(rows: &[(&str, CarInfo)]) -> HashMap<String, CarInfo> {
        rows.iter().map(|(k, c)| (k.to_string(), c.clone())).collect()
    }

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn classify_profiles() {
        let new = ProfileData::default();
        assert_eq!(classify(&new), Career::New);
        let xp = ProfileData { xp: 5, ..Default::default() };
        assert_eq!(classify(&xp), Career::Existing);
        let mut ev = ProfileData::default();
        ev.events.insert("12".into(), EventRecord::default());
        assert_eq!(classify(&ev), Career::Existing);
        let seen = ProfileData { fmv_seen: flags(&["FMV_01"]), ..Default::default() };
        assert_eq!(classify(&seen), Career::Existing);
        let fl = ProfileData { story_flags: flags(&["intro_done"]), ..Default::default() };
        assert_eq!(classify(&fl), Career::Flagged);
        // Flags win over results.
        let both = ProfileData { story_flags: flags(&["intro_done"]), xp: 100, ..Default::default() };
        assert_eq!(classify(&both), Career::Flagged);
    }

    #[test]
    fn steps_follow_the_flags() {
        assert_eq!(next_step(&[]), Some(Step::Intro));
        assert_eq!(next_step(&flags(&["intro_done"])), Some(Step::Festival));
        // The starter choice sits between the festival arrival and Race Central.
        assert_eq!(next_step(&flags(&["intro_done", "festival_arrived"])), Some(Step::Starter));
        assert_eq!(next_step(&flags(&["intro_done", "festival_arrived", "starter_chosen"])), Some(Step::RaceCentral));
        // first_wristband alone (or a crash after the credit) still leaves race_central_seen to write.
        assert_eq!(next_step(&flags(&["intro_done", "festival_arrived", "starter_chosen", "first_wristband"])), Some(Step::RaceCentral));
        assert_eq!(next_step(&flags(&ALL_FLAGS)), None);
        // Out of order flags: the first missing one decides.
        assert_eq!(next_step(&flags(&["festival_arrived"])), Some(Step::Intro));
        assert_eq!(next_step(&flags(&["intro_done", "starter_chosen"])), Some(Step::Festival));
        // A career of the previous build (all four old flags, no starter_chosen) gets the choice once.
        assert_eq!(next_step(&flags(&["intro_done", "festival_arrived", "race_central_seen", "first_wristband"])), Some(Step::Starter));
    }

    #[test]
    fn plans() {
        let new = ProfileData::default();
        let old = ProfileData { xp: 100, ..Default::default() };
        let done = ProfileData { story_flags: flags(&ALL_FLAGS), ..Default::default() };
        let mid = ProfileData { story_flags: flags(&["intro_done"]), ..Default::default() };
        let choosing = ProfileData { story_flags: flags(&["intro_done", "festival_arrived"]), ..Default::default() };
        let previous_build = ProfileData { story_flags: flags(&["intro_done", "festival_arrived", "race_central_seen", "first_wristband"]), ..Default::default() };
        assert_eq!(plan(IntroMode::On, false, &new, &[]), Plan::Run(Step::Intro));
        assert_eq!(plan(IntroMode::On, false, &old, &[]), Plan::Migrate);
        assert_eq!(plan(IntroMode::On, false, &done, &[]), Plan::Skip);
        assert_eq!(plan(IntroMode::On, false, &mid, &[]), Plan::Run(Step::Festival));
        assert_eq!(plan(IntroMode::On, false, &choosing, &[]), Plan::Run(Step::Starter));
        assert_eq!(plan(IntroMode::On, false, &previous_build, &[]), Plan::Run(Step::Starter));
        assert_eq!(plan(IntroMode::On, true, &new, &[]), Plan::Skip);
        assert_eq!(plan(IntroMode::Off, false, &new, &[]), Plan::Skip);
        assert_eq!(plan(IntroMode::Off, false, &old, &[]), Plan::Skip);
        // Force ignores the profile and the automation guard; its own flags advance it.
        assert_eq!(plan(IntroMode::Force, true, &old, &[]), Plan::Run(Step::Intro));
        assert_eq!(plan(IntroMode::Force, false, &done, &flags(&["intro_done"])), Plan::Run(Step::Festival));
        assert_eq!(plan(IntroMode::Force, false, &done, &flags(&["intro_done", "festival_arrived"])), Plan::Run(Step::Starter));
        assert_eq!(plan(IntroMode::Force, false, &done, &flags(&ALL_FLAGS)), Plan::Skip);
    }

    #[test]
    fn flag_modes() {
        assert_eq!(mode_from(None), IntroMode::On);
        assert_eq!(mode_from(Some("1")), IntroMode::On);
        assert_eq!(mode_from(Some("0")), IntroMode::Off);
        assert_eq!(mode_from(Some("OFF")), IntroMode::Off);
        assert_eq!(mode_from(Some("force")), IntroMode::Force);
    }

    #[test]
    fn placements_are_engine_space() {
        assert_eq!(engine_pos(WORKSHOP_RAW), Vec3::new(-1027.26, -8.68, 145.57));
        assert_eq!(engine_pos(START_RAW).z, -1881.99);
        // track.rs FESTIVAL_HOME: game yaw 133.888 deg is engine yaw -2.33679 rad.
        assert!((engine_yaw(133.888) + 2.33679).abs() < 1e-3);
        // The intro faces east-ish (engine +X, slightly -Z): colorado.nav's road heading there.
        let yaw = engine_yaw(START_ROT_DEG);
        let fwd = Vec2::new(-yaw.sin(), -yaw.cos());
        assert!(fwd.x > 0.99 && fwd.y > 0.0 && fwd.y < 0.1, "{fwd}");
    }

    #[test]
    fn entrances_trigger_inside_the_ring_only() {
        assert!(near_entrance(Vec2::new(-978.0, 400.0)));
        assert!(near_entrance(Vec2::new(-951.9, 60.0)));
        assert!(!near_entrance(Vec2::new(-978.0, 520.0)));
        assert!(!near_entrance(Vec2::new(-1361.97, -1881.99)));
    }

    #[test]
    fn the_viper_is_recognised() {
        assert!(is_viper("VIP_Viper_13"));
        assert!(is_viper("vip_viper_13"));
        assert!(is_viper("DOD_ViperSRT10ACRX_12"));
        assert!(!is_viper("VW_Corrado_95"));
        assert_eq!(STARTER_ANCHOR, "VW_Corrado_95");
    }

    #[test]
    fn starters_never_offer_the_anchor_and_spread_makes_and_drives() {
        let cars = catalog(&[
            ("VW_Corrado_95", car(2, 282, 1, "Volkswagen")),
            ("JAG_Etype_61", car(2, 278, 2, "Jaguar")),
            ("FIA_500_10", car(2, 287, 1, "Abarth")),
            ("DOD_Charger_69", car(2, 288, 2, "Dodge")),
            ("CHE_Camaro_69", car(2, 291, 2, "Chevrolet")),
            ("FAR_Class", car(3, 283, 0, "Other")),
        ]);
        // Nearest is the Jaguar (RWD, new make); the Abarth adds the other drivetrain; then drivetrains are used up, so the
        // nearest new make (the Dodge) wins over the Chevrolet. Seeds are empty: the Corrado's FWD / VW are not avoided.
        let got = pick_starters(&cars, "VW_Corrado_95", &all);
        assert_eq!(got, ["JAG_Etype_61", "FIA_500_10", "DOD_Charger_69"]);
        assert!(!got.iter().any(|c| c == "VW_Corrado_95"));
        // Deterministic: the same twice.
        assert_eq!(got, pick_starters(&cars, "VW_Corrado_95", &all));
    }

    #[test]
    fn starters_prefer_a_new_drivetrain_over_a_nearer_pi() {
        let cars = catalog(&[
            ("A_Anchor", car(2, 300, 1, "Alpha")),
            ("B_Near", car(2, 301, 1, "Bravo")),
            ("B_Twin", car(2, 302, 1, "Bravo")),
            ("C_Awd", car(2, 312, 0, "Charlie")),
            ("D_Rwd", car(2, 314, 2, "Delta")),
        ]);
        // B_Near is nearest and first; then C and D each add a drivetrain and a make, B_Twin (same make and drivetrain as
        // B_Near, though nearer than both) adds nothing.
        assert_eq!(pick_starters(&cars, "A_Anchor", &all), ["B_Near", "C_Awd", "D_Rwd"]);
    }

    #[test]
    fn starters_window_and_filters() {
        let mut unicorn = car(2, 300, 2, "Rare");
        unicorn.unicorn = true;
        let mut free = car(2, 300, 2, "Free");
        free.price = 0;
        let mut traffic = car(2, 300, 2, "Traffic");
        traffic.selectable = false;
        let mut missing = car(2, 300, 2, "Missing");
        missing.installed = false;
        let cars = catalog(&[
            ("Anchor", car(2, 300, 1, "Alpha")),
            ("Ok_Edge", car(2, 315, 2, "Edge")),
            ("Far", car(2, 330, 2, "Far")),
            ("Unicorn", unicorn),
            ("Free", free),
            ("Traffic", traffic),
            ("Missing", missing),
            ("Unlisted", car(2, 301, 2, "Unlisted")),
        ]);
        let listed = |n: &str| n != "Unlisted";
        // Ok_Edge (exactly 15 away) and Far (30) are all that qualifies; the unicorn, the free car, the traffic car, the
        // missing model and the unlisted car never do. Fewer than three: the window widens, the offer is what exists.
        assert_eq!(pick_starters(&cars, "Anchor", &listed), ["Ok_Edge", "Far"]);
    }

    #[test]
    fn starters_widen_when_too_few() {
        let cars = catalog(&[("Anchor", car(2, 300, 1, "Alpha")), ("Mid", car(2, 340, 2, "Mid")), ("Far", car(2, 380, 0, "Far")), ("Other_Class", car(3, 301, 0, "Oc"))]);
        // Nothing within 15: the window widens (30, 60, 120) until everything of the class is in. The other class never is.
        assert_eq!(pick_starters(&cars, "Anchor", &all), ["Mid", "Far"]);
    }

    #[test]
    fn starters_fall_back_to_the_anchor() {
        // No catalog: the anchor alone, when the garage lists it.
        assert_eq!(pick_starters(&HashMap::new(), "VW_Corrado_95", &all), ["VW_Corrado_95"]);
        assert!(pick_starters(&HashMap::new(), "VW_Corrado_95", &|_| false).is_empty());
        // Catalog with the anchor but no other candidate: still the anchor alone.
        let lone = catalog(&[("VW_Corrado_95", car(2, 282, 1, "Volkswagen"))]);
        assert_eq!(pick_starters(&lone, "VW_Corrado_95", &all), ["VW_Corrado_95"]);
        assert!(pick_starters(&lone, "VW_Corrado_95", &|_| false).is_empty());
        // Anchor in the catalog but not in the garage: the others are offered all the same.
        let cars = catalog(&[("Anchor", car(2, 300, 1, "A")), ("B", car(2, 301, 2, "B")), ("C", car(2, 302, 0, "C")), ("D", car(2, 303, 1, "D"))]);
        let listed = |n: &str| n != "Anchor";
        let got = pick_starters(&cars, "Anchor", &listed);
        assert_eq!(got.len(), 3);
        assert!(!got.contains(&"Anchor".to_owned()));
    }
}
