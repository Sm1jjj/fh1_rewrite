//! The race HUD the FH1 way (2026-10-08, job fa): the game's own `947_HUD` race widgets, driven with our race state,
//! in place of the plain-text panel (race/hud.rs; `FH1_RACE_HUD_OLD=1` = that panel).
//!
//! Widgets (names / slides / events from the scene, docs/UI.md "Race HUD"):
//! - `PlaceBlock`: TEXT_PLACE / TEXT_PLACE_MAX ("4 / 8"), label InGame:IDS_Place.
//! - `EndConditionBlock`: TEXT_ENDCON_CURRENT / TEXT_ENDCON_FINAL, units InGame:IDS_Laps (circuits) or IDS_Checkpoints.
//! - `HUDTimes` (HUD_TIMERS, 5 lines LINEn.TEXT_TITLE_n / LINEn.value.TEXT_VALUE_n; `SB_LINES_n` shows n lines): race
//!   time, current lap, best lap, split against the best lap.
//! - `PositionIndicators` > `LeaderboardBars` > 8 `HUDFullPositionBar*` rows (POSITION_BAR: Bar.TEXT_POSITION,
//!   Bar.TEXT_GAMERTAG, TEXT_TIME; row events FIRST_PLACE / LOCAL_PLAYER / OTHER_PLAYER, SHOW_TIME / HIDE_TIME).
//! - `I_321_Go` (HUD_321GO: Group.TEXT_COUNT; COUNTDOWN_THREE / TWO / ONE / GO).
//! - `EndOfRaceCountdown` (SET_MODE_RACE_START + SHOW: "RACE STARTS IN" while the grid settles).
//! - `WrongWay` (slide Slide1; no text: the sign itself).
//! - `ActivationPrompt` (event start prompt: SHOW / HIDE, SET_EVENTICON_*, SET_MODE_AVAILABLE / LOCKED).
//! Each widget is element-scoped (`fire_at`). ui/hud.rs re-hides them on every Player::reset; this re-applies.

use bevy::prelude::*;
use fh1_ui::player::{Player, Value};

use super::{Events, RacePhase, RaceState};
use crate::ui::hud::Fh1Hud;
use crate::ui::scene::{AnarkScene, UiData};

/// `FH1_RACE_HUD_OLD=1`: the plain-text race panel.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("FH1_RACE_HUD_OLD").is_ok_and(|v| v == "1"))
}

/// Whether the Anark race HUD is actually running (enabled and the 947 scene found).
#[derive(Resource, Default)]
pub struct AnarkRaceHud {
    pub active: bool,
    o: Option<Objs>,
    labels: Option<Labels>,
    resets: u32,
    shown: bool,
    lines: usize,
    count: Option<u32>,
    go_fired: bool,
    start_shown: bool,
    wrong: bool,
    rows: Vec<u8>,
    times_shown: Vec<bool>,
    prompt: Option<(usize, bool)>,
}

struct Labels {
    place: String,
    laps: String,
    checkpoints: String,
    time: String,
    lap: String,
    best: String,
    split: String,
    go: String,
    starts_in: String,
}

struct Row {
    root: usize,
    pos: Option<usize>,
    tag: Option<usize>,
    time: Option<usize>,
}

#[derive(Default)]
struct Objs {
    place: Option<usize>,
    place_label: Option<usize>,
    place_val: Option<usize>,
    place_max: Option<usize>,
    endcon: Option<usize>,
    endcon_units: Option<usize>,
    endcon_cur: Option<usize>,
    endcon_final: Option<usize>,
    times: Option<usize>,
    titles: Vec<Option<usize>>,
    values: Vec<Option<usize>>,
    positions: Option<usize>,
    rows: Vec<Row>,
    go: Option<usize>,
    go_text: Option<usize>,
    eorc: Option<usize>,
    eorc_title: Option<usize>,
    eorc_value: Option<usize>,
    wrong: Option<usize>,
    prompt: Option<usize>,
    prompt_title: Option<usize>,
}

/// First descendant of `from` with this name.
pub fn below(p: &Player, from: usize, name: &str) -> Option<usize> {
    let mut stack = vec![from];
    while let Some(o) = stack.pop() {
        for &c in p.children(o) {
            if p.name(c) == Some(name) {
                return Some(c);
            }
            stack.push(c);
        }
    }
    None
}

/// First descendant whose name looks like a text object (`TEXT_*`, `Text*`).
pub fn text_below(p: &Player, from: usize) -> Option<usize> {
    let mut stack = vec![from];
    while let Some(o) = stack.pop() {
        for &c in p.children(o).iter().rev() {
            if p.name(c).is_some_and(|n| n.starts_with("TEXT") || n.starts_with("Text")) {
                return Some(c);
            }
            stack.push(c);
        }
    }
    None
}

pub fn opacity(p: &mut Player, o: Option<usize>, v: f32) {
    if let Some(o) = o {
        p.set(o, fh1_ui::names::OPACITY, Value::Float(v));
    }
}

pub fn text(p: &mut Player, o: Option<usize>, s: impl Into<String>) {
    if let Some(o) = o {
        p.set_text(o, s);
    }
}

/// A string-table entry (markup stripped), else `d`.
pub fn s(data: &UiData, r: &str, d: &str) -> String {
    data.strings.as_ref().and_then(|t| t.resolve(r)).map(fh1_ui::strtable::strip_markup).filter(|x| !x.is_empty()).unwrap_or_else(|| d.to_owned())
}

/// "1ST" etc. (InGame:IDS_Place_n suffixes: 1 ST, 2 ND, 3 RD, else TH).
pub fn place_text(data: &UiData, n: u32) -> String {
    let k = match n % 100 {
        11..=13 => 0,
        _ => match n % 10 {
            1 => 1,
            2 => 2,
            3 => 3,
            _ => 0,
        },
    };
    let d = ["TH", "ST", "ND", "RD"][k];
    format!("{n}{}", s(data, &format!("InGame:IDS_Place_{k}"), d))
}

/// FH1 timer text: m:ss.mmm.
pub fn fmt_time(t: f32) -> String {
    let t = t.max(0.0);
    let ms = (t * 1000.0).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60000, (ms / 1000) % 60, ms % 1000)
}

impl Objs {
    fn find(p: &Player) -> Self {
        let f = |n: &str| p.find(n);
        let place = f("PlaceBlock");
        let endcon = f("EndConditionBlock");
        let times = f("HUDTimes");
        let positions = f("PositionIndicators");
        let go = f("I_321_Go");
        let eorc = f("EndOfRaceCountdown");
        let prompt = f("ActivationPrompt");
        let u = |root: Option<usize>, n: &str| root.and_then(|r| below(p, r, n));
        let rows = u(positions, "LeaderboardBars")
            .map(|lb| p.children(lb).to_vec())
            .unwrap_or_default()
            .into_iter()
            .filter(|&c| p.name(c).is_some_and(|n| n.starts_with("HUDFullPositionBar")))
            .map(|r| Row { root: r, pos: below(p, r, "TEXT_POSITION"), tag: below(p, r, "TEXT_GAMERTAG"), time: below(p, r, "TEXT_TIME") })
            .collect();
        let line = |k: usize, leaf: &str| times.and_then(|t| below(p, t, &format!("LINE{k}"))).and_then(|l| below(p, l, &format!("{leaf}_{k}")));
        let prompt_title = u(prompt, "Title").map(|t| text_below(p, t).unwrap_or(t));
        Self {
            place,
            place_label: u(place, "TEXT_PLACE_LABEL"),
            place_val: u(place, "TEXT_PLACE"),
            place_max: u(place, "TEXT_PLACE_MAX"),
            endcon,
            endcon_units: u(endcon, "TEXT_ENDCON_UNITS"),
            endcon_cur: u(endcon, "TEXT_ENDCON_CURRENT"),
            endcon_final: u(endcon, "TEXT_ENDCON_FINAL"),
            times,
            titles: (1..=5).map(|k| line(k, "TEXT_TITLE")).collect(),
            values: (1..=5).map(|k| line(k, "TEXT_VALUE")).collect(),
            positions,
            rows,
            go,
            go_text: u(go, "TEXT_COUNT"),
            eorc,
            eorc_title: u(eorc, "TEXT_RACE_STARTS_IN"),
            eorc_value: u(eorc, "TEXT_END_OF_RACE_COUNTDOWN"),
            wrong: f("WrongWay"),
            prompt,
            prompt_title,
        }
    }
}

fn fire(p: &mut Player, ev: &str, o: Option<usize>) {
    if let Some(o) = o {
        p.fire_at(ev, o);
    }
}

/// Drive the race widgets of 947_HUD (after ui::hud::drive_hud).
#[allow(clippy::too_many_arguments)]
pub fn drive_race_hud(
    mut st: ResMut<AnarkRaceHud>,
    hud: Option<Res<Fh1Hud>>,
    data: Option<Res<UiData>>,
    mut scenes: Query<&mut AnarkScene>,
    rs: Res<RaceState>,
    events: Res<Events>,
    cat: Option<Res<crate::progression::EventCatalog>>,
) {
    let (Some(hud), Some(data)) = (hud, data) else {
        st.active = false;
        return;
    };
    if !enabled() || !super::races_on() {
        st.active = false;
        return;
    }
    let Ok(mut sc) = scenes.get_mut(hud.scene_entity()) else { return };
    let p = &mut sc.player;
    if st.o.is_none() {
        st.o = Some(Objs::find(p));
        st.labels = Some(Labels {
            place: s(&data, "InGame:IDS_Place", "PLACE"),
            laps: s(&data, "InGame:IDS_Laps", "LAPS"),
            checkpoints: s(&data, "InGame:IDS_Checkpoints", "CHECKPOINTS"),
            time: s(&data, "InGame:IDS_Time", "TIME"),
            lap: s(&data, "InGame:IDS_CurrentLapTimeLabel", "LAP"),
            best: s(&data, "InGame:IDS_Best", "BEST"),
            split: s(&data, "InGame:IDS_Split", "SPLIT"),
            go: s(&data, "InGame:IDS_3_2_1_GO", "GO!"),
            starts_in: s(&data, "InGame:IDS_Race_Starts_In", "RACE STARTS IN"),
        });
        let o = st.o.as_ref().unwrap();
        info!("race hud: 947_HUD widgets place {:?} laps {:?} times {:?} rows {} 321 {:?} wrongway {:?}", o.place, o.endcon, o.times, o.rows.len(), o.go, o.wrong);
    }
    st.active = st.o.as_ref().is_some_and(|o| o.place.is_some() && o.times.is_some());
    if !st.active {
        return;
    }
    // ui/hud.rs re-hid everything on a reset.
    if hud.reset_count() != st.resets {
        st.resets = hud.reset_count();
        st.shown = false;
        st.prompt = None;
        st.wrong = false;
    }
    let st = &mut *st;
    let (o, l) = (st.o.as_ref().unwrap(), st.labels.as_ref().unwrap());
    let def = rs.race.and_then(|i| events.races.get(i));
    let racing = def.is_some() && matches!(rs.phase, RacePhase::Grid { .. } | RacePhase::Countdown { .. } | RacePhase::Racing);

    // Event start prompt (free roam, standing in a marker).
    let want_prompt = match (rs.phase, rs.prompt) {
        (RacePhase::Idle, Some(i)) if !rs.list_open => Some((i, rs.prompt_locked.is_some())),
        _ => None,
    };
    if want_prompt != st.prompt {
        match want_prompt {
            Some((i, locked)) => {
                let r = &events.races[i];
                let tier = cat.as_ref().and_then(|c| c.events.get(i)).map_or(crate::progression::event_tier(r), |e| e.tier);
                let icon = match crate::progression::EventKind::of(r) {
                    crate::progression::EventKind::Street => "SET_EVENTICON_STREETRACEHUB".to_owned(),
                    crate::progression::EventKind::Nemesis => format!("SET_EVENTICON_NEMESISEVENT{}", tier.min(6) + 1),
                    _ if !r.circuit => format!("SET_EVENTICON_P2PEVENT{}", tier.min(6) + 1),
                    _ => format!("SET_EVENTICON_FESTIVALEVENT{}", tier.min(6) + 1),
                };
                opacity(p, o.prompt, 100.0);
                text(p, o.prompt_title, super::states::prompt_title(&r.name, rs.prompt_locked.as_deref()));
                fire(p, &icon, o.prompt);
                fire(p, if locked { "SET_MODE_LOCKED" } else { "SET_MODE_AVAILABLE" }, o.prompt);
                fire(p, "SHOW", o.prompt);
                fire(p, "SHOW_TITLE", o.prompt);
            }
            None => fire(p, "HIDE", o.prompt),
        }
        st.prompt = want_prompt;
    }

    if !racing {
        if st.shown {
            for w in [o.place, o.endcon, o.times, o.positions, o.wrong, o.eorc] {
                opacity(p, w, 0.0);
            }
            st.shown = false;
        }
        st.count = None;
        st.go_fired = false;
        st.start_shown = false;
        return;
    }
    let def = def.unwrap();
    if !st.shown {
        st.shown = true;
        for w in [o.place, o.endcon, o.times, o.positions, o.go] {
            opacity(p, w, 100.0);
        }
        opacity(p, o.wrong, 0.0);
        st.lines = 0;
        st.rows = vec![255; o.rows.len()];
        st.times_shown = vec![true; o.rows.len()];
        st.wrong = false;
    }
    let Some(me) = rs.racers.first() else { return };
    let n = rs.racers.len() as u32;

    // Place.
    text(p, o.place_label, l.place.clone());
    text(p, o.place_val, me.position.to_string());
    text(p, o.place_max, n.to_string());

    // Laps, or checkpoints on a point-to-point.
    let per_lap = def.gates.len().max(1) as u32;
    if def.laps > 1 {
        text(p, o.endcon_units, l.laps.clone());
        text(p, o.endcon_cur, me.lap.min(def.laps).to_string());
        text(p, o.endcon_final, def.laps.to_string());
    } else {
        text(p, o.endcon_units, l.checkpoints.clone());
        text(p, o.endcon_cur, (me.gates_done % per_lap + 1).min(per_lap).to_string());
        text(p, o.endcon_final, per_lap.to_string());
    }

    // Timers.
    let clock = me.finished_s.unwrap_or(rs.clock_s);
    let mut lines: Vec<(String, String)> = vec![(l.time.clone(), fmt_time(clock))];
    if def.laps > 1 {
        lines.push((l.lap.clone(), fmt_time(clock - rs.lap_start_s)));
        if let Some(best) = rs.laps.iter().copied().reduce(f32::min) {
            lines.push((l.best.clone(), fmt_time(best)));
        }
    }
    if let Some((split, Some(best))) = rs.last_split {
        let d = split - best;
        lines.push((l.split.clone(), format!("{}{:.3}", if d <= 0.0 { "-" } else { "+" }, d.abs())));
    }
    for (k, (t, v)) in lines.iter().enumerate() {
        text(p, o.titles.get(k).copied().flatten(), t.clone());
        text(p, o.values.get(k).copied().flatten(), v.clone());
    }
    if lines.len() != st.lines {
        st.lines = lines.len();
        fire(p, &format!("SB_LINES_{}", lines.len()), o.times);
    }

    // Position list: everyone by position; the player highlighted, the leader on the 1st-place style.
    let mut order: Vec<&super::Racer> = rs.racers.iter().collect();
    order.sort_by_key(|r| r.position);
    let leader_t = order.first().and_then(|r| r.finished_s);
    for (k, row) in o.rows.iter().enumerate() {
        match order.get(k) {
            Some(r) => {
                opacity(p, Some(row.root), 100.0);
                text(p, row.pos, (k + 1).to_string());
                text(p, row.tag, r.driver.to_uppercase());
                let state = if r.is_player { 1 } else if k == 0 { 0 } else { 2 };
                if st.rows[k] != state {
                    st.rows[k] = state;
                    p.fire_at(["FIRST_PLACE", "LOCAL_PLAYER", "OTHER_PLAYER"][state as usize], row.root);
                }
                // Gap to the leader once both have finished; hidden while racing.
                let gap = match (r.finished_s, leader_t) {
                    (Some(t), Some(l0)) if k > 0 => Some(format!("+{}", fmt_time(t - l0))),
                    (Some(t), _) if k == 0 => Some(fmt_time(t)),
                    _ => None,
                };
                let show_time = gap.is_some();
                if let Some(g) = gap {
                    text(p, row.time, g);
                }
                if st.times_shown[k] != show_time {
                    st.times_shown[k] = show_time;
                    p.fire_at(if show_time { "SHOW_TIME" } else { "HIDE_TIME" }, row.root);
                }
            }
            None => opacity(p, Some(row.root), 0.0),
        }
    }

    // Grid: "RACE STARTS IN n"; countdown 3-2-1-GO.
    match rs.phase {
        RacePhase::Grid { left_s } => {
            if !st.start_shown {
                st.start_shown = true;
                opacity(p, o.eorc, 100.0);
                text(p, o.eorc_title, l.starts_in.clone());
                fire(p, "SET_MODE_RACE_START", o.eorc);
                fire(p, "SHOW", o.eorc);
            }
            text(p, o.eorc_value, format!("{}", (left_s + super::COUNTDOWN_S).ceil() as u32));
        }
        RacePhase::Countdown { left_s } => {
            if st.start_shown {
                st.start_shown = false;
                fire(p, "HIDE", o.eorc);
            }
            let c = left_s.ceil().clamp(1.0, 3.0) as u32;
            if st.count != Some(c) {
                st.count = Some(c);
                text(p, o.go_text, c.to_string());
                fire(p, ["COUNTDOWN_ONE", "COUNTDOWN_TWO", "COUNTDOWN_THREE"][c as usize - 1], o.go);
            }
        }
        RacePhase::Racing => {
            if st.start_shown {
                st.start_shown = false;
                fire(p, "HIDE", o.eorc);
            }
            if !st.go_fired {
                st.go_fired = true;
                text(p, o.go_text, l.go.clone());
                fire(p, "COUNTDOWN_GO", o.go);
            }
        }
        _ => {}
    }

    // Wrong way sign.
    let wrong = rs.phase == RacePhase::Racing && rs.wrong_way_s > 2.0;
    if wrong != st.wrong {
        st.wrong = wrong;
        opacity(p, o.wrong, if wrong { 100.0 } else { 0.0 });
        if wrong {
            if let Some(w) = o.wrong {
                p.goto_slide(w, "Slide1");
            }
        }
    }
}

/// The Anark race HUD and the post-race screens (added by progression::ProgressionPlugin).
pub struct RaceUiPlugin;

impl Plugin for RaceUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AnarkRaceHud>()
            .init_resource::<super::postrace::PostRace>()
            .add_systems(Update, (drive_race_hud, super::postrace::drive_postrace).chain().after(crate::ui::hud::drive_hud).after(super::race_update));
    }
}
