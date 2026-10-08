//! The points system on FH1's own HUD widgets (947_HUD): the skill chain (`DrivingSkill`), the popularity bar
//! (`PopularityBar`), wristband unlocks (`PopularityUnlock`) and career notices through the notification tape
//! (ui/notify.rs). The data comes from progression (fa): `SkillEvent` / `CareerNotice` messages, `Skills`, `Profile`.
//! `FH1_SKILL_HUD=0` = progression's plain-text skill HUD and banners instead.
//!
//! Scene facts (VERIFIED with the fh1-ui `contracts` / `events` / `tree` dumps of 947_HUD, 2026-10-08):
//! - **DrivingSkill (569)** holds contract HUD_DRIVING_FEATS. Its fields are TEXT_TOTAL (601), TEXT_MULTIPLIER (580;
//!   TEXT_X 577 is the authored "x"), Skill1 (610) and Skill2 (638).
//!   - Its handler (570) takes these events:
//!     - SHOW_SCORE: ScoreGroup SHOW, Total DEFAULT.
//!     - FAIL: ScoreGroup / Multiplier / Total ABORT.
//!     - SUCCEED: Multiplier HIDDEN, Total BANK (2.62 s).
//!     - RESET: Multiplier SHOWN, Total DEFAULT.
//!     - INCREASE_MULTIPLIER: Multiplier ANIMATE.
//!     - HIDDEN_MULTIPLIER and HIDDEN_SCORE.
//!     - START_SKILL1 / START_SKILL2: the SkillGroup slide that puts that slot on top.
//!     - MAKE_SIZE_1..5: the DrivingSkill size slides.
//! - **Skill1 / Skill2** each hold contract HUD_DRIVING_FEAT: TEXT_POP_SKILLNAME, SkillValue, and ComboIcon (628/656),
//!   whose slides are the combo names (DAREDEVIL, STUNTMAN, SUPERMAN, KANGAROO, TRIPLEPASS, SHOWOFF, LUCKYESCAPE,
//!   SLINGSHOT, ...).
//!   - Their handlers (611/639) take SHOW, SHOWN, WARNING, ABORT, HIDDEN and SHOW_COMBO, SHOWN_COMBO, WARNING_COMBO,
//!     ABORT_COMBO.
//! - **PopularityBar (679)** holds contract HUD_POPULARITY_BAR: TEXT_LEVEL (742), TEXT_LEVEL_SUFFIX (743), Bar (697).
//!   - Bar slides DEFAULT and GOLD are code-driven (flags 13), so the fill is the slide progress.
//!   - Its handler (680) takes SHOW, HIDE, RESET, LEVEL_UP, LEVEL_UP_FIRST (+ Bar GOLD), LEVEL_UP_FINAL,
//!     SET_TEXT_SIZE_SMALL/MEDIUM/LARGE (3/2/1 digits) and SET_STARS_1..5.
//! - **PopularityUnlock (755)** holds contract HUD_SHOWCASE_UNLOCK_CONTROL: MainMessage.TEXT_TITLE (764) and
//!   ShowMe.TEXT_SHOW (776). Handler 756 takes SHOW, HIDE, HIDE_INSTANT; the SHOW slide lasts 5 s.
//!
//! Strings come from the disc (RaceFeats.str: "GREAT DRIFT", "AIRBORNE PASS"…; InGame `IDS_PopularitySuperscript_n`;
//! PostRaceFlow `IDS_WristbandUnlocked`). Grades follow from the award's fame (HorizonFeats fame per grade).
//! GUESSES (how the game sequences the events; no xex trace yet):
//! - Each new skill takes the other slot (START_SKILLk). A running skill shows SHOW and then SHOWN once it's awarded;
//!   an instant skill gets SHOW alone.
//! - WARNING fires when 30 % of the chain window is left.
//! - MAKE_SIZE_n is picked by the total's digit count.
//! - The popularity bar shows only on a rank-up (LEVEL_UP) and hides after it (user, 2026-10-08); its flash /
//!   smoke-ring halo layers stay hidden; the chain total is centred over the skill line.

use std::sync::OnceLock;

use bevy::prelude::*;
use fh1_ui::player::{Player, Value};

use super::hud::Fh1Hud;
use super::notify::HudNotify;
use super::scene::{AnarkScene, UiData};
use crate::progression::skill::{SkillEvent, SkillKind, Skills};
use crate::progression::{fmt_num, CareerNotice, Profile};

/// FH1's Anark skill / popularity HUD (`FH1_SKILL_HUD=0` = progression's text HUD).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SKILL_HUD").map_or(true, |v| v != "0"))
}

/// The chain window fraction under which the top skill blinks (WARNING).
const WARN_FRAC: f32 = 0.3;
/// Seconds after a bank / loss before the chain widgets go (Total BANK is 2.62 s; ABORT 1 s).
const BANK_HOLD_S: f32 = 2.8;
const LOST_HOLD_S: f32 = 1.2;
/// Seconds the popularity bar stays after the chain ends (then its 3 s HIDE slide).
const BAR_HOLD_S: f32 = 3.5;
/// Seconds the bar fill takes to move to a new value.
const BAR_FILL_S: f32 = 0.9;
/// Wristband unlock message (PopularityUnlock SHOW slide = 5 s).
const UNLOCK_S: f32 = 5.0;

/// HorizonFeats fame per grade (1..4), as progression/skill.rs uses them: the grade of an award from its fame.
fn grade(kind: SkillKind, fame: u32) -> usize {
    let table: [u32; 4] = match kind {
        SkillKind::Drift | SkillKind::NearMiss => [100, 250, 500, 1000],
        SkillKind::CleanSpeed => [100, 150, 250, 1000],
        _ => [100, 150, 250, 500],
    };
    table.iter().position(|&f| f == fame).map_or(1, |i| i + 1)
}

/// RaceFeats.str key stem per skill (grade appended) and the English fallbacks.
fn feat_key(kind: SkillKind) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        SkillKind::Drift => ("Drift", "DRIFT"),
        SkillKind::Air => ("Air", "AIR"),
        SkillKind::NearMiss => ("NearMiss", "NEAR MISS"),
        SkillKind::Pass => ("Overtake", "PASS"),
        SkillKind::CleanSpeed => ("CleanSpeed", "SPEED"),
        SkillKind::Draft => ("Draft", "DRAFTING"),
        SkillKind::Burnout => ("Burnout", "BURNOUT"),
        SkillKind::Combo => return None,
    })
}

/// A combo's (ComboIcon slide, RaceFeats key) from progression's label.
fn combo(label: &str) -> Option<(&'static str, &'static str)> {
    Some(match label {
        "DAREDEVIL" => ("DAREDEVIL", "IDS_DareDevil"),
        "STUNTMAN" => ("STUNTMAN", "IDS_StuntMan"),
        "SUPERMAN" => ("SUPERMAN", "IDS_Superman"),
        "KANGAROO" => ("KANGAROO", "IDS_Kangaroo"),
        "TRIPLE PASS" => ("TRIPLEPASS", "IDS_TriplePass"),
        "SHOW OFF" => ("SHOWOFF", "IDS_ShowOff"),
        "LUCKY ESCAPE" => ("LUCKYESCAPE", "IDS_LuckyEscape"),
        "SLINGSHOT" => ("SLINGSHOT", "IDS_SlingShot"),
        _ => return None,
    })
}

/// What a skill slot shows.
#[derive(Clone, Copy, PartialEq, Default)]
enum Slot {
    #[default]
    Empty,
    /// A skill in progress (its live value updates).
    Live,
    Shown { combo: bool },
}

/// The scene's objects (looked up once per HUD scene).
#[derive(Clone, Copy)]
struct Objs {
    skill: usize,
    total: usize,
    mult: usize,
    slots: [(usize, usize, usize, usize); 2],
    bar_root: usize,
    bar: usize,
    level: usize,
    suffix: usize,
    unlock: usize,
    unlock_title: usize,
    unlock_show: usize,
    /// PopularityBar's flash / smoke-ring layers (an authored 40 % halo behind the ring): kept hidden (user, 2026-10-08).
    flashes: [Option<usize>; 3],
}

impl Objs {
    fn find(p: &Player) -> Option<Self> {
        let skill = p.find("DrivingSkill")?;
        let slot = |name: &str| -> Option<(usize, usize, usize, usize)> {
            let s = p.resolve_path(skill, &format!("CompleteComponent.SkillGroup.{name}"))?;
            Some((s, p.resolve_path(s, "anim_helper.Skill.TEXT_POP_SKILLNAME")?, p.resolve_path(s, "anim_helper.Skill.SkillValue")?, p.resolve_path(s, "anim_helper.Skill.ComboIcon")?))
        };
        let bar_root = p.find("PopularityBar")?;
        let unlock = p.find("PopularityUnlock")?;
        Some(Self {
            skill,
            total: p.resolve_path(skill, "CompleteComponent.ScoreGroup.anim_helper.Total.anim_helper.TEXT_TOTAL")?,
            mult: p.resolve_path(skill, "CompleteComponent.ScoreGroup.anim_helper.Multiplier.anim_helper.anim_helper.TEXT_MULTIPLIER")?,
            slots: [slot("Skill1")?, slot("Skill2")?],
            bar_root,
            bar: p.resolve_path(bar_root, "Bar")?,
            level: p.resolve_path(bar_root, "ValueScale.ValueType.TEXT_LEVEL")?,
            suffix: p.resolve_path(bar_root, "ValueScale.ValueType.TEXT_LEVEL_SUFFIX")?,
            unlock,
            unlock_title: p.resolve_path(unlock, "MainMessage.TEXT_TITLE")?,
            unlock_show: p.resolve_path(unlock, "ShowMe.TEXT_SHOW")?,
            flashes: [p.resolve_path(bar_root, "CircleFlash"), p.resolve_path(bar_root, "Flash_SmokeRing"), p.resolve_path(bar_root, "Flash_SmokeRing2")],
        })
    }
}

/// Centre the chain total over the skill line (the screen's centre line), as FH1 shows it: its SUPER_STACKER layout
/// isn't implemented, so the authored left-aligned spot sits up-left of the skill name. Returns the local x that puts
/// the text's centre on x = 0, measured from its parent's world transform. The score group is only evaluated while
/// shown (after SHOW_SCORE), so this is measured while a chain is on screen (VERIFIED offline: parent at x −32.8,
/// scale 0.8 once shown; None right after a HUD reset).
fn measure_total_x(p: &Player, o: &Objs) -> Option<f32> {
    let parent = p.scene.bgf.objects.get(o.total).map(|ob| ob.parent).filter(|&x| x >= 0)? as usize;
    let frame = p.evaluate();
    let (m, _) = frame.objects.get(&parent)?;
    let origin = fh1_ui::player::apply(m, [0.0; 3]);
    let sx = (m[0][0] * m[0][0] + m[0][1] * m[0][1]).sqrt();
    (sx > 1e-4).then(|| (0.0 - origin[0]) / sx)
}

fn place_total(p: &mut Player, o: &Objs, x: f32) {
    p.set(o.total, fh1_ui::names::HORZALIGN, Value::Int(1));
    p.set(o.total, fh1_ui::names::POSITION_X, Value::Float(x));
}

#[derive(Resource, Default)]
struct SkillHudState {
    objs: Option<Objs>,
    /// Fh1Hud reset count the objects / visibility were applied for.
    applied: Option<u32>,
    active: bool,
    /// Slot index (0/1) on top, and what each shows.
    top: usize,
    slots: [Slot; 2],
    mult: u32,
    warned: bool,
    /// Seconds until the chain widgets go after a bank / loss.
    ending: Option<f32>,
    /// Popularity bar: shown, seconds left before HIDE, displayed rank and fill (animated), fill target.
    bar_shown: bool,
    bar_hide: Option<f32>,
    bar_rank: Option<u32>,
    bar_fill: f32,
    bar_to: f32,
    unlock_left: Option<f32>,
    /// The chain total's centred local x once measured (kept across HUD resets), and seconds of measuring so far.
    total_x: Option<f32>,
    total_measure_s: f32,
}

pub struct SkillHudPlugin;

impl Plugin for SkillHudPlugin {
    fn build(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        app.init_resource::<crate::progression::skill::ExternalSkillHud>()
            .init_resource::<SkillHudState>()
            .add_systems(Update, drive.after(super::hud::drive_hud).run_if(resource_exists::<Skills>));
    }
}

fn opacity(p: &mut Player, o: usize, on: bool) {
    p.set(o, fh1_ui::names::OPACITY, Value::Float(if on { 100.0 } else { 0.0 }));
}

/// A string from the disc's tables, else the fallback.
fn text(data: Option<&UiData>, file: &str, id: &str, fallback: &str) -> String {
    data.and_then(|d| d.strings.as_ref()).and_then(|s| s.get(file, id)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| fallback.to_owned())
}

/// The skill's on-screen name: RaceFeats `IDS_<Skill><grade>` ("GREAT DRIFT"), or the combo's name.
fn skill_name(data: Option<&UiData>, kind: SkillKind, fame: u32, label: &str) -> String {
    if let Some((key, _)) = combo(label) {
        return text(data, "RaceFeats", key, label);
    }
    match feat_key(kind) {
        Some((stem, fallback)) => text(data, "RaceFeats", &format!("IDS_{stem}{}", grade(kind, fame)), fallback),
        None => label.to_owned(),
    }
}

/// "ST" / "ND" / "RD" / "TH" (InGame `IDS_PopularitySuperscript_0..19`, indexed by the last two digits under 20, else the last digit).
fn suffix(data: Option<&UiData>, rank: u32) -> String {
    let i = if rank % 100 < 20 { rank % 100 } else { rank % 10 };
    let fallback = match i {
        1 => "ST",
        2 => "ND",
        3 => "RD",
        _ => "TH",
    };
    text(data, "InGame", &format!("IDS_PopularitySuperscript_{i}"), fallback)
}

/// Fill of the popularity bar towards the next rank (rank 250 = last, 1 = top).
fn bar_fill(career: &crate::progression::data::CareerData, fame: u64) -> (u32, f32) {
    let rank = career.rank(fame);
    if rank <= 1 {
        return (1, 1.0);
    }
    let (lo, hi) = (career.fame_for(rank), career.fame_for(rank - 1));
    let f = if hi > lo { (fame.saturating_sub(lo)) as f32 / (hi - lo) as f32 } else { 1.0 };
    (rank, f.clamp(0.0, 1.0))
}

#[allow(clippy::too_many_arguments)]
fn drive(
    time: Res<Time<Real>>,
    hud: Option<Res<Fh1Hud>>,
    mut scenes: Query<&mut AnarkScene>,
    mut st: ResMut<SkillHudState>,
    skills: Res<Skills>,
    mut skill_events: MessageReader<SkillEvent>,
    mut notices: MessageReader<CareerNotice>,
    mut notify: MessageWriter<HudNotify>,
    profile: Option<Res<Profile>>,
    events: Option<Res<crate::race::Events>>,
    data: Option<Res<UiData>>,
) {
    let dt = time.delta_secs();
    let data = data.as_deref();
    let Some(hud) = hud else {
        skill_events.clear();
        notices.clear();
        return;
    };
    let Ok(mut sc) = scenes.get_mut(hud.scene_entity()) else { return };
    let p = &mut sc.player;
    // A HUD reset drops every override and slide and re-hides the widgets: start over.
    if st.applied != Some(hud.reset_count()) {
        st.applied = Some(hud.reset_count());
        st.objs = Objs::find(p);
        if st.objs.is_none() {
            warn!("skill HUD: 947_HUD skill / popularity objects not found");
        }
        let keep = (st.bar_rank, st.bar_fill, st.total_x);
        *st = SkillHudState { objs: st.objs, applied: st.applied, ..default() };
        (st.bar_rank, st.bar_fill, st.total_x) = keep;
        if let Some(o) = st.objs {
            for f in o.flashes.into_iter().flatten() {
                opacity(p, f, false);
            }
            if let Some(x) = st.total_x {
                place_total(p, &o, x);
            }
        }
    }
    let Some(o) = st.objs else {
        skill_events.clear();
        notices.clear();
        return;
    };
    let fame = profile.as_ref().map_or(0, |p| p.data.fame);
    let (rank, fill) = events.as_ref().map_or((250, 0.0), |e| bar_fill(&e.career, fame));

    // Skill chain.
    let start = |st: &mut SkillHudState, p: &mut Player| {
        if st.active {
            return;
        }
        st.active = true;
        st.ending = None;
        st.slots = [Slot::Empty; 2];
        st.mult = 1;
        st.warned = false;
        opacity(p, o.skill, true);
        p.fire_at("RESET", o.skill);
        p.fire_at("HIDDEN_MULTIPLIER", o.skill);
        for (s, ..) in o.slots {
            p.fire_at("HIDDEN", s);
        }
        p.set_text(o.total, "0");
        p.fire_at("SHOW_SCORE", o.skill);
    };
    // Bring the other slot to the top for a new skill.
    let next_slot = |st: &mut SkillHudState, p: &mut Player| -> usize {
        let k = if st.slots[st.top] == Slot::Empty { st.top } else { 1 - st.top };
        st.top = k;
        p.fire_at(if k == 0 { "START_SKILL1" } else { "START_SKILL2" }, o.skill);
        k
    };
    for ev in skill_events.read() {
        match ev {
            SkillEvent::Live { label, value_text } => {
                start(&mut st, p);
                let k = if st.slots[st.top] == Slot::Live { st.top } else { next_slot(&mut st, p) };
                let (slot, name, value, _) = o.slots[k];
                // The running skill's grade-0 name ("DRIFT") from progression's label ("DRIFT 42 m" -> "DRIFT").
                p.set_text(name, label.split_whitespace().next().unwrap_or(label).to_owned());
                p.set_text(value, value_text.clone());
                if st.slots[k] != Slot::Live {
                    st.slots[k] = Slot::Live;
                    p.fire_at("SHOW", slot);
                }
                st.warned = false;
            }
            SkillEvent::Award { label, fame, kind, combo: is_combo } => {
                start(&mut st, p);
                let is_combo = *is_combo || *kind == SkillKind::Combo;
                // A running skill completes in its own slot; instant skills and combos take a new one.
                let k = if !is_combo && st.slots[st.top] == Slot::Live { st.top } else { next_slot(&mut st, p) };
                let (slot, name, value, icon) = o.slots[k];
                p.set_text(name, skill_name(data, *kind, *fame, label));
                p.set_text(value, fmt_num(*fame as i64));
                if is_combo {
                    if let Some((slide, _)) = combo(label) {
                        p.goto_slide(icon, slide);
                    }
                    p.fire_at("SHOW_COMBO", slot);
                } else if st.slots[k] == Slot::Live {
                    p.fire_at("SHOWN", slot);
                } else {
                    p.fire_at("SHOW", slot);
                }
                st.slots[k] = Slot::Shown { combo: is_combo };
                st.warned = false;
            }
            SkillEvent::Banked { value, .. } => {
                if st.active {
                    p.set_text(o.total, fmt_num(*value as i64));
                    p.fire_at("SUCCEED", o.skill);
                    st.ending = Some(BANK_HOLD_S);
                }
            }
            SkillEvent::Lost { .. } => {
                if st.active {
                    for (k, (slot, ..)) in o.slots.into_iter().enumerate() {
                        match st.slots[k] {
                            Slot::Shown { combo: true } => p.fire_at("ABORT_COMBO", slot),
                            Slot::Empty => 0,
                            _ => p.fire_at("ABORT", slot),
                        };
                    }
                    p.fire_at("FAIL", o.skill);
                    st.ending = Some(LOST_HOLD_S);
                }
            }
        }
    }
    // First chain after start-up: follow the score group's intro (SHOW_SCORE, 1 s) each frame, then keep the value.
    if st.active && st.total_x.is_none() {
        if let Some(x) = measure_total_x(p, &o) {
            place_total(p, &o, x);
            st.total_measure_s += dt;
            if st.total_measure_s > 1.2 {
                st.total_x = Some(x);
            }
        }
    }
    if st.active && st.ending.is_none() {
        // Running total, its digit-count size, and the multiplier.
        let ch = &skills.chain;
        let total = fmt_num(ch.total as i64);
        p.set_text(o.total, total.clone());
        let digits = total.chars().filter(char::is_ascii_digit).count();
        let size = digits.saturating_sub(2).clamp(1, 5);
        p.fire_at(&format!("MAKE_SIZE_{size}"), o.skill);
        let m = ch.multiplier();
        if m != st.mult {
            if m > 1 {
                p.set_text(o.mult, m.to_string());
                p.fire_at("INCREASE_MULTIPLIER", o.skill);
            } else {
                p.fire_at("HIDDEN_MULTIPLIER", o.skill);
            }
            st.mult = m;
        }
        // The chain is running out: the top skill blinks.
        if !st.warned && !ch.awards.is_empty() && skills.chain_window_frac() < WARN_FRAC {
            st.warned = true;
            let (slot, ..) = o.slots[st.top];
            match st.slots[st.top] {
                Slot::Shown { combo: true } => p.fire_at("WARNING_COMBO", slot),
                Slot::Shown { .. } => p.fire_at("WARNING", slot),
                _ => 0,
            };
        }
    }
    if let Some(left) = st.ending {
        let left = left - dt;
        if left <= 0.0 {
            st.ending = None;
            st.active = false;
            for (slot, ..) in o.slots {
                p.fire_at("HIDDEN", slot);
            }
            p.fire_at("HIDDEN_SCORE", o.skill);
            opacity(p, o.skill, false);
        } else {
            st.ending = Some(left);
        }
    }

    // Career notices: rank-ups on the bar, wristbands on PopularityUnlock, the rest on the notification tape.
    for n in notices.read() {
        match n {
            CareerNotice::RankUp { rank: r, passed } => {
                if !st.bar_shown {
                    st.bar_shown = true;
                    opacity(p, o.bar_root, true);
                    p.fire_at("SHOW", o.bar_root);
                }
                p.fire_at(if *r == 1 { "LEVEL_UP_FIRST" } else { "LEVEL_UP" }, o.bar_root);
                st.bar_hide = Some(BAR_HOLD_S + 1.4);
                let title = text(data, "MyProfile", "IDS_Popularity_Level", "POPULARITY LEVEL");
                let mut lines = vec![title, format!("{r}{}", suffix(data, *r))];
                if let Some(name) = passed {
                    lines.push(format!("PASSED {}", name.to_uppercase()));
                }
                notify.write(HudNotify { lines });
            }
            CareerNotice::Wristband { tier } => {
                let band = crate::progression::TIER_NAMES.get(*tier as usize).copied().unwrap_or("NEW").to_uppercase();
                p.set_text(o.unlock_title, text(data, "PostRaceFlow", "IDS_WristbandUnlocked", "WRISTBAND UNLOCKED!"));
                p.set_text(o.unlock_show, format!("{band} WRISTBAND"));
                opacity(p, o.unlock, true);
                p.fire_at("SHOW", o.unlock);
                st.unlock_left = Some(UNLOCK_S);
            }
            CareerNotice::Payout { credits, reason } => {
                notify.write(HudNotify { lines: vec![format!("{} CR", fmt_num(*credits)), reason.to_uppercase()] });
            }
            CareerNotice::PrizeCar { car } => {
                notify.write(HudNotify { lines: vec!["PRIZE CAR".into(), car.to_uppercase()] });
            }
            // Fame shows on the bar itself.
            CareerNotice::Fame { .. } => {}
        }
    }
    if let Some(left) = st.unlock_left {
        let left = left - dt;
        if left <= 0.0 {
            st.unlock_left = None;
            p.fire_at("HIDE_INSTANT", o.unlock);
            opacity(p, o.unlock, false);
        } else {
            st.unlock_left = Some(left);
        }
    }

    // Popularity bar: rank text + its digit size, fill eased to the profile's fame.
    if st.bar_shown {
        if st.bar_rank != Some(rank) {
            if st.bar_rank.is_some_and(|old| rank > old) || st.bar_rank.is_none() {
                st.bar_fill = fill;
            } else {
                // Ranked up: the fill restarts from the bottom of the new rank.
                st.bar_fill = 0.0;
            }
            st.bar_rank = Some(rank);
            p.set_text(o.level, rank.to_string());
            p.set_text(o.suffix, suffix(data, rank));
            let size = match rank {
                0..=9 => "SET_TEXT_SIZE_LARGE",
                10..=99 => "SET_TEXT_SIZE_MEDIUM",
                _ => "SET_TEXT_SIZE_SMALL",
            };
            p.fire_at(size, o.bar_root);
            p.goto_slide(o.bar, if rank == 1 { "GOLD" } else { "DEFAULT" });
        }
        st.bar_to = fill;
        let step = dt / BAR_FILL_S;
        st.bar_fill += (st.bar_to - st.bar_fill).clamp(-step, step);
        p.set_progress(o.bar, st.bar_fill);
        p.set_playing(o.bar, false);
        if let Some(left) = st.bar_hide {
            let left = left - dt;
            if left <= 0.0 {
                st.bar_hide = None;
                st.bar_shown = false;
                p.fire_at("HIDE", o.bar_root);
            } else {
                st.bar_hide = Some(left);
            }
        }
    }
}
