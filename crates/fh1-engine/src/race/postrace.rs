//! Post-race screens the FH1 way (2026-10-08, job fa): the game's own H_POST_RACE_* Anark scenes in sequence, in
//! place of the text results panel (`FH1_RACE_HUD_OLD=1` = that panel).
//!
//! Flow (scene names / slides / events from the ui install, docs/UI.md "Race HUD"):
//! 1. player finishes: `H_POST_RACE_FINISHED` (the position tape: TEXT_TITLE event name, TEXT_POSITION "1ST";
//!    SHOW_POSITION_TAPE_FIRST / _NON_FIRST);
//! 2. results (A / Enter steps on): `H_POST_RACE_RESULTS` (title tape; 8 rows I_LIST_ITEM, I_LIST_ITEM2..8 with
//!    Pill.TEXT_POSITION / TEXT_PLAYER / CarInfoGroup.TEXT_CAR / TEXT_BEST_LAP / TEXT_TOTAL_TIME; row slide PLAYER or
//!    OPPONENT; SHOWTOTALTIME, SHOW_BEST_LAP, SHOW_CAR_INFO);
//! 3. `H_POST_RACE_CREDITS_BREAKDOWN` (Item1..3 label / value, total, balance; INTRO_3);
//! 4. `H_POST_RACE_CAREER_POINTS` (points earned, the wristband bar SP_DRIVER_LEVEL_BAR seeked on PROGRESS, points to
//!    the next wristband, SHOW_LEVEL_UP on a new wristband); then back to free roam.
//! Pages 3 and 4 only with progression on (`FH1_PROGRESSION`). A scene that isn't installed is skipped.

use std::collections::HashMap;

use bevy::prelude::*;
use fh1_ui::player::Player;

use super::anark_hud::{below, fmt_time, place_text, s, text};
use super::{Events, RacePhase, RaceState};
use crate::progression::{fmt_num, LastRewards, Profile};
use crate::ui::scene::{AnarkScene, UiData, TEXT_PT_TO_PX};

const FINISHED: &str = "H_POST_RACE_FINISHED";
const RESULTS: &str = "H_POST_RACE_RESULTS";
const CREDITS: &str = "H_POST_RACE_CREDITS_BREAKDOWN";
const POINTS: &str = "H_POST_RACE_CAREER_POINTS";

#[derive(Resource, Default)]
pub struct PostRace {
    scenes: HashMap<&'static str, Option<Entity>>,
    /// The scene shown and the race / page it was filled for.
    shown: Option<(&'static str, usize, i32)>,
}

impl PostRace {
    /// Load (once) and return a scene's entity.
    fn entity(&mut self, name: &'static str, data: &UiData, commands: &mut Commands) -> Option<Entity> {
        *self.scenes.entry(name).or_insert_with(|| {
            let p = data.scene(name)?;
            let mut sc = AnarkScene::new(p, 30);
            sc.visible = false;
            sc.text_scale = TEXT_PT_TO_PX;
            Some(commands.spawn(sc).id())
        })
    }
}

/// Pages of the results phase (after the FINISHED tape).
fn pages() -> &'static [&'static str] {
    if crate::progression::enabled() {
        &[RESULTS, CREDITS, POINTS]
    } else {
        &[RESULTS]
    }
}

#[allow(clippy::too_many_arguments)]
pub fn drive_postrace(
    mut st: ResMut<PostRace>,
    mut rs: ResMut<RaceState>,
    events: Res<Events>,
    data: Option<Res<UiData>>,
    rewards: Res<LastRewards>,
    profile: Res<Profile>,
    hud: Res<super::anark_hud::AnarkRaceHud>,
    mut scenes: Query<&mut AnarkScene>,
    mut commands: Commands,
    assets: Res<AssetServer>,
) {
    let Some(data) = data else { return };
    if !super::anark_hud::enabled() || !hud.active {
        return;
    }
    // Which scene, for which race / page (-1 = the finish tape).
    let want: Option<(&'static str, usize, i32)> = match (rs.phase, rs.race) {
        (RacePhase::Finished { .. }, Some(i)) => Some((FINISHED, i, -1)),
        (RacePhase::Results, Some(i)) => {
            let pages = pages();
            if rs.results_pages != pages.len() as u8 {
                rs.results_pages = pages.len() as u8;
            }
            pages.get(rs.results_page as usize).map(|n| (*n, i, rs.results_page as i32))
        }
        _ => None,
    };
    if want == st.shown {
        return;
    }
    // Hide the old one.
    if let Some((name, _, _)) = st.shown.take() {
        if let Some(e) = st.scenes.get(name).copied().flatten() {
            if let Ok(mut sc) = scenes.get_mut(e) {
                sc.visible = false;
            }
        }
    }
    let Some((name, race, page)) = want else { return };
    st.shown = Some((name, race, page));
    let Some(e) = st.entity(name, &data, &mut commands) else {
        warn!("race: post-race scene {name} not installed");
        return;
    };
    let Some(def) = events.races.get(race) else { return };
    // A scene spawned this frame is filled next frame.
    let Ok(mut sc) = scenes.get_mut(e) else {
        st.shown = None;
        return;
    };
    sc.visible = true;
    let p = &mut sc.player;
    p.reset();
    let me = rs.racers.first();
    let place = me.map_or(0, |m| m.position);
    match name {
        FINISHED => fill_finished(p, &data, place),
        RESULTS => fill_results(p, &data, def, &rs),
        CREDITS => fill_credits(p, &data, &rewards),
        _ => fill_points(p, &data, &rewards, &events.career, profile.data.xp),
    }
    common(p, &data);
    // Class badges: every row draws CLASS_A.TGA; the field shares the player's class (race/field.rs), so one override
    // per scene (class_<x>.png, CarClasses BadgeTexturePathPrefix).
    if name == RESULTS {
        if let Some((class, _)) = me.and_then(|m| m.class_pi) {
            let letter = events.career.class_name(class).to_ascii_lowercase();
            sc.texture_overrides.insert("horizon/piclass/class_a.png".into(), assets.load(format!("ui/textures/horizon/piclass/class_{letter}.png")));
        }
    }
}

/// Set the text of the first object with this name.
fn set_named(p: &mut Player, name: &str, v: impl Into<String>) {
    if let Some(o) = p.find(name) {
        p.set_text(o, v);
    }
}

/// Every object with this name (depth-first from the scene root), in file order.
fn all_named(p: &Player, name: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut stack = vec![0usize];
    while let Some(o) = stack.pop() {
        if p.name(o) == Some(name) {
            out.push(o);
        }
        stack.extend(p.children(o).iter().rev());
    }
    out
}

/// The parts every post-race scene shares: the button bar (A CONTINUE, the other six hidden), the timer (hidden: no
/// countdown offline) and the online / offline upload notes (hidden).
fn common(p: &mut Player, data: &UiData) {
    for k in 0..7 {
        let Some(b) = p.find(&format!("HelpButton{k}")) else { continue };
        if k == 0 {
            if let Some(t) = below(p, b, "Text_BUTTON") {
                p.set_text(t, s(data, "HelpButtons:IDS_Continue", "CONTINUE"));
            }
            p.fire_at("SHOWN", b);
        } else {
            p.fire_at("HIDDEN", b);
        }
    }
    if let Some(t) = p.find("Timer") {
        p.fire_at("HIDE", t);
        p.fire_at("HIDDEN", t);
    }
    for ev in ["HIDE_ONLINE_TEXT", "HIDE_OFFLINE_TEXT"] {
        p.fire(ev);
    }
}

fn fill_finished(p: &mut Player, data: &UiData, place: u32) {
    set_named(p, "TEXT_TITLE", s(data, "ScreenTitles:IDS_PostRace_Finished", "FINISHED"));
    set_named(p, "TEXT_POSITION", place_text(data, place));
    p.fire(if place == 1 { "SHOW_POSITION_TAPE_FIRST" } else { "SHOW_POSITION_TAPE_NON_FIRST" });
}

fn fill_results(p: &mut Player, data: &UiData, def: &super::RaceDef, rs: &RaceState) {
    set_named(p, "TitleTapeText", def.name.to_uppercase());
    if let Some(h) = p.find("Heading") {
        for (n, r, d) in [
            ("TEXT_POSITION", "Scoreboard:IDS_Pos", "POS"),
            ("TEXT_PLAYER", "Scoreboard:IDS_Driver", "DRIVER"),
            ("TEXT_CLASS", "Scoreboard:IDS_Class", "CLASS"),
            ("TEXT_CAR", "Scoreboard:IDS_Car", "CAR"),
            ("TEXT_BEST_LAP", "InGame:IDS_Header_Best_Lap", "BEST LAP"),
            ("TEXT_TOTAL_TIME", "InGame:IDS_Header_Total", "TOTAL"),
        ] {
            if let Some(t) = below(p, h, n) {
                p.set_text(t, s(data, r, d));
            }
        }
    }
    let mut order: Vec<&super::Racer> = rs.racers.iter().collect();
    order.sort_by_key(|r| r.position);
    let best_lap = rs.laps.iter().copied().reduce(f32::min);
    let total_gates = super::total_gates(def).max(1);
    for k in 0..8 {
        let name = if k == 0 { "I_LIST_ITEM".to_owned() } else { format!("I_LIST_ITEM{}", k + 1) };
        let Some(row) = p.find(&name) else { continue };
        let Some(r) = order.get(k) else {
            p.fire_at("HIDE", row);
            continue;
        };
        let f = |p: &Player, n: &str| below(p, row, n);
        let (pos, player, car, best, total, pi) = (f(p, "TEXT_POSITION"), f(p, "TEXT_PLAYER"), f(p, "TEXT_CAR"), f(p, "TEXT_BEST_LAP"), f(p, "TEXT_TOTAL_TIME"), f(p, "TEXT_PI"));
        // FOCUS = the PLAYER slide (the highlighted row), SHOW = OPPONENT.
        p.fire_at(if r.is_player { "FOCUS" } else { "SHOW" }, row);
        text(p, pos, (k + 1).to_string());
        text(p, player, r.driver.to_uppercase());
        text(p, car, r.car.to_uppercase());
        // Only the player's laps are timed (AI lap splits are not kept).
        text(p, best, if r.is_player { best_lap.map_or("--".into(), fmt_time) } else { "--".into() });
        // Not finished when the results came up: the finish time projected from the share of the race done (marked
        // with a ~); DNF without any progress.
        let shown_total = match r.finished_s {
            Some(t) => fmt_time(t),
            None if r.gates_done > def.start_gate && rs.clock_s > 0.0 => {
                let done = (r.gates_done - def.start_gate) as f32 / total_gates.saturating_sub(def.start_gate).max(1) as f32;
                format!("~{}", fmt_time(rs.clock_s / done.max(0.05)))
            }
            None => s(data, "InGame:IDS_Place_DNF", "DNF"),
        };
        text(p, total, shown_total);
        match r.class_pi {
            Some((_, v)) => {
                text(p, pi, v.to_string());
                p.fire_at("SHOW_CAR_CLASS", row);
            }
            None => {
                p.fire_at("HIDE_CAR_CLASS", row);
            }
        }
        for ev in ["SHOWTOTALTIME", "SHOW_BEST_LAP", "SHOW_CAR_INFO", "HIDE_WRISTBAND", "HIDE_TEAM_COLOR"] {
            p.fire_at(ev, row);
        }
    }
}

fn fill_credits(p: &mut Player, data: &UiData, rw: &LastRewards) {
    set_named(p, "TEXT_TITLE", s(data, "ScreenTitles:IDS_PostRace_Winnings", "CASH REWARD"));
    let mut items: Vec<(String, String)> = vec![(format!("{} PLACE", place_text(data, rw.place)), format!("{} CR", fmt_num(rw.prize)))];
    items.extend(rw.bonuses.iter().map(|(l, v)| (l.clone(), format!("{} CR", fmt_num(*v)))));
    items.push(("PAYOUT".into(), if rw.replay { "50% (REPLAY)".into() } else { "100%".into() }));
    let n = items.len().clamp(3, 5);
    let (labels, values) = (all_named(p, "TEXT_LABEL"), all_named(p, "TEXT_VALUE"));
    for k in 0..labels.len().max(values.len()) {
        let (l, v) = items.get(k).cloned().unwrap_or_default();
        text(p, labels.get(k).copied(), l);
        text(p, values.get(k).copied(), v);
    }
    let total = format!("{} CR", fmt_num(rw.credits));
    // TEXT_WINNING: the first is the "TOTAL" label, the second the value (+ its echo).
    let w = all_named(p, "TEXT_WINNING");
    text(p, w.first().copied(), s(data, "InGame:IDS_Header_Total", "TOTAL"));
    text(p, w.get(1).copied(), total.clone());
    set_named(p, "TEXT_WINNING_ECHO", total);
    set_named(p, "TEXT_BALANCE_TITLE", "BALANCE:");
    set_named(p, "TEXT_BALANCE", format!("{} CR", fmt_num(rw.balance)));
    p.fire(&format!("INTRO_{n}"));
}

fn fill_points(p: &mut Player, data: &UiData, rw: &LastRewards, c: &crate::progression::data::CareerData, xp: u64) {
    // TEXT_TITLE x3: the screen title, the place on the main tape, the reward caption.
    let titles = all_named(p, "TEXT_TITLE");
    text(p, titles.first().copied(), s(data, "PostRaceFlow:IDS_PointsEarned", "POINTS EARNED"));
    text(p, titles.get(1).copied(), place_text(data, rw.place));
    text(p, titles.get(2).copied(), "XP REWARD");
    let main = s(data, "PostRaceFlow:IDS_PointsPlr", "{0} POINTS").replace("{0}", &fmt_num(rw.xp as i64));
    set_named(p, "TEXT_MAIN", main);
    let tier = c.tier(xp);
    let (frac, next) = match (c.wristbands.get(tier), c.wristbands.get(tier + 1)) {
        (Some(a), Some(b)) => (((xp - a.xp) as f32 / (b.xp - a.xp).max(1) as f32).clamp(0.0, 1.0), Some(b.xp - xp)),
        _ => (1.0, None),
    };
    let to_next = match next {
        Some(n) => s(data, "PostRaceFlow:IDS_PointsToNextPlr", "{0} POINTS TO NEXT WRISTBAND").replace("{0}", &fmt_num(n as i64)),
        None => s(data, "PostRaceFlow:IDS_HeadlineEventAvailableLn1", "HEADLINE EVENT UNLOCKED!"),
    };
    set_named(p, "TEXT_POINTS_TO_NEXT", to_next.clone());
    let next_name = c
        .wristbands
        .get(tier + 1)
        .or(c.wristbands.get(tier))
        .map_or(String::new(), |w| s(data, "Scoreboard:IDS_FormattedWristband", "{0} WRISTBAND").replace("{0}", &w.name.to_uppercase()));
    set_named(p, "TEXT_CAR_NAME", next_name);
    let level_up = rw.tier_after > rw.tier_before;
    set_named(p, "TEXT_COLLECT", if level_up { s(data, "PostRaceFlow:IDS_WristbandUnlocked", "WRISTBAND UNLOCKED!") } else { to_next });
    p.fire("SET_TARGET_IS_WRISTBAND");
    p.fire("SHOW_CAREER_POINTS_TAPE");
    if let Some(bar) = p.find("SP_DRIVER_LEVEL_BAR") {
        p.goto_slide(bar, "PROGRESS");
        p.set_progress(bar, frac);
        p.set_playing(bar, false);
    }
    if level_up {
        p.fire("SHOW_LEVEL_UP");
    }
}
