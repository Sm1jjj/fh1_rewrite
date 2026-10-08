//! The HUD's objective line (top left, `947_HUD` HUDInstruction) and pop-up notifications
//! (CombinedNotification), driven through the scene's own contracts and event handlers.
//!
//! From the scene (VERIFIED with the `events` / `contracts` examples):
//! - HUD_INSTRUCTION contract on HUDInstruction (406): `InstructionText.Line1.Bar.Text`,
//!   `InstructionText.Line2.Bar.Text`, `InstructionText.Line1.Bar.Icon` (material).
//! - Handler 407: `SHOW` → InstructionText SHOW (0-1000 ms), `SHOWN` → SHOWN, `HIDE` → HIDE (0-1000 ms),
//!   `HIDDEN` → HIDDEN.
//! - HUD_COMBINED_NOTIFICATION on CombinedNotification (1278): `State.DoubleTape.TEXT1..3` and
//!   `RankIcon.Bar.Text`.
//! - Handler 1279: `SET_MODE_GENERAL` (State DEFAULT), `SHOW` → SHOW (0-1000 ms),
//!   `SHOW_COMPLETE` → ON, `HIDE` → HIDE, `HIDE_COMPLETE` → OFF.
//!
//! Free roam's objective is race central ("Head to the Horizon Heats" in the Xenia refs; xex string
//! `IDS_HudInstruction_Travel_To_RaceCentral`), the minimap's default satnav target. It clears on
//! arrival inside race central's TriggerZone radius (24 m, `race_central.xml`).
//! Credits (P10, 2026-10-08): every credit gain (progression::wallet `CreditsChanged`: race payouts, sponsors,
//! popularity milestones, car sales) goes on this tape as `+N CR`, its reason and the new balance, the changes of one frame summed into
//! one notification. 947_HUD has no credits counter of its own (its only reward text, TEXT_REWARD, is the street-race
//! encounter's), so the tape is FH1's place for it. Spending is not toasted (the garage / shop menus show it).
//! `FH1_CREDIT_TOAST=0` = off (then ui/skillhud.rs shows CareerNotice::Payout as before).
//! GUESSES:
//! - The game raises the `*_COMPLETE` / `SHOWN` events when the 1 s show/hide slides end.
//! - A notification stays up for [`NOTIFY_HOLD`].

use std::collections::VecDeque;

use bevy::prelude::*;

use super::scene::{AnarkScene, UiData};
use crate::Car;

/// Seconds a notification stays fully shown (GUESS).
const NOTIFY_HOLD: f32 = 4.0;
/// Length of the authored SHOW / HIDE slides (0-1000 ms).
const SLIDE: f32 = 1.0;
/// Race central's TriggerZone radius (`race_central.xml` festival_02, radius 24).
const ARRIVE_M: f32 = 24.0;
/// String files tried for `IDS_HudInstruction_*` (which .str holds them isn't known yet).
const STRING_FILES: [&str; 7] = ["InGame", "GameStrings", "Activities", "RaceCentral", "Main", "HelpButtons", "Missions"];

/// The objective line: gameplay sets `text` (None hides it). On Colorado free roam it starts as
/// race central's instruction.
#[derive(Resource, Default)]
pub struct Objective {
    pub text: Option<String>,
    /// Where it clears on arrival (engine x, z), if anywhere.
    pub target: Option<Vec2>,
}

/// Post a CombinedNotification: `lines[0]` is the large title (TEXT2, top tape), `lines[1]` the
/// small line under it (TEXT1), `lines[2]` TEXT3 (seen in engine shots of the authored layout).
#[derive(Message, Clone)]
pub struct HudNotify {
    pub lines: Vec<String>,
}

pub struct NotifyPlugin;

impl Plugin for NotifyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Objective>()
            .init_resource::<NotifyState>()
            .add_message::<HudNotify>()
            .add_systems(Update, (default_objective, debug_notify, objective, notifications).chain().after(crate::sync_visuals).after(super::hud::drive_hud));
        if credit_toast() {
            app.add_systems(
                Update,
                credits_toast.before(notifications).run_if(resource_exists::<bevy::ecs::message::Messages<crate::progression::wallet::CreditsChanged>>),
            );
        }
    }
}

/// `FH1_CREDIT_TOAST=0`: no `+N CR` notifications (module doc).
pub fn credit_toast() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_CREDIT_TOAST").map_or(true, |v| v != "0"))
}

/// One `+N CR` notification per frame of credit gains.
fn credits_toast(mut changes: MessageReader<crate::progression::wallet::CreditsChanged>, mut out: MessageWriter<HudNotify>) {
    let mut total = 0i64;
    let mut balance = None;
    let mut reasons: Vec<String> = Vec::new();
    for c in changes.read().filter(|c| c.delta > 0) {
        total += c.delta;
        balance = Some(c.balance);
        if !reasons.contains(&c.reason) {
            reasons.push(c.reason.clone());
        }
    }
    if total <= 0 {
        return;
    }
    let reason = match reasons.len() {
        1 => reasons.remove(0),
        n => format!("{} +{} more", reasons[0], n - 1),
    };
    // TEXT3: the new balance.
    let mut lines = vec![format!("+{} CR", crate::progression::fmt_num(total)), reason.to_uppercase()];
    if let Some(b) = balance {
        lines.push(format!("{} CR", crate::progression::fmt_num(b)));
    }
    out.write(HudNotify { lines });
}

#[derive(Resource, Default)]
pub(crate) struct NotifyState {
    /// The HUD scene entity and its HUDInstruction / CombinedNotification objects.
    hud: Option<(Entity, usize, usize)>,
    /// The default objective was decided (reset by an in-process map change, world_load::switch_world).
    pub(crate) defaulted: bool,
    debug_posted: bool,
    /// The objective text currently on screen (cleared by an in-process map change: the HUD reset puts the widget back
    /// on its authored placeholder, which only the "nothing shown" path hides).
    pub(crate) shown: Option<String>,
    /// `Fh1Hud::resets` when the instruction was last set (HIDE included). Until it is set after the HUD's reset the
    /// scene shows its authored placeholder text, so a map with no objective (anything but Colorado) must still HIDE.
    synced: Option<u32>,
    /// Seconds until the pending `SHOWN` / `HIDDEN` event.
    instr_timer: Option<(f32, &'static str)>,
    queue: VecDeque<Vec<String>>,
    /// Current notification phase: (event to raise next, seconds until then).
    note: Option<(&'static str, f32)>,
}

/// The HUD scene: the one holding both widgets.
/// Looked up by name once (`Player::find` is a linear scan), then cached in `NotifyState::hud`.
fn hud_scene<'a>(cache: &mut Option<(Entity, usize, usize)>, scenes: &'a mut Query<(Entity, &mut AnarkScene)>) -> Option<(Mut<'a, AnarkScene>, usize, usize)> {
    if cache.is_none_or(|(e, _, _)| !scenes.contains(e)) {
        *cache = scenes.iter().find_map(|(e, sc)| Some((e, sc.player.find("HUDInstruction")?, sc.player.find("CombinedNotification")?)));
    }
    let (e, i, n) = (*cache)?;
    Some((scenes.get_mut(e).ok()?.1, i, n))
}

/// Colorado free roam: the race-central objective (once, when the UI data is there).
fn default_objective(
    mut st: ResMut<NotifyState>,
    mut obj: ResMut<Objective>,
    data: Option<Res<UiData>>,
    track: Res<crate::track::Track>,
    race: Option<Res<crate::race::RaceState>>,
) {
    // Not while a race owns the objective (fa): decided after it.
    if st.defaulted || race.is_some_and(|r| r.race.is_some()) {
        return;
    }
    let Some(data) = data else { return };
    st.defaulted = true;
    if track.id != "colorado" || obj.text.is_some() {
        return;
    }
    let found = data.strings.as_ref().and_then(|s| STRING_FILES.iter().find_map(|f| Some((f, s.get(f, "IDS_HudInstruction_Travel_To_RaceCentral")?))));
    match found {
        Some((file, t)) => {
            info!("objective: IDS_HudInstruction_Travel_To_RaceCentral from {file}.str");
            obj.text = Some(fh1_ui::strtable::strip_markup(t));
        }
        None => {
            warn!("objective: IDS_HudInstruction_Travel_To_RaceCentral not in {STRING_FILES:?}; English fallback");
            obj.text = Some("Head to the Horizon Heats".into());
        }
    }
    obj.target = Some(super::minimap::RACE_CENTRAL);
}

/// `FH1_NOTIFY=line1|line2|line3`: post one notification at start (debug).
fn debug_notify(mut st: ResMut<NotifyState>, mut out: MessageWriter<HudNotify>, time: Res<Time>) {
    if st.debug_posted || time.elapsed_secs() < 3.0 {
        return;
    }
    st.debug_posted = true;
    if let Ok(v) = std::env::var("FH1_NOTIFY") {
        out.write(HudNotify { lines: v.split('|').map(str::to_owned).collect() });
    }
}

#[allow(clippy::too_many_arguments)]
fn objective(
    time: Res<Time>,
    hud: Option<Res<super::hud::Fh1Hud>>,
    mut st: ResMut<NotifyState>,
    mut obj: ResMut<Objective>,
    cars: Query<&Car>,
    mut scenes: Query<(Entity, &mut AnarkScene)>,
    race: Option<Res<crate::race::RaceState>>,
) {
    // During a race with FH1's race widgets (race/anark_hud.rs, fa) the instruction line stays hidden: the laps block
    // sits in its place. Shown as `None` without touching Objective (free roam's comes back after the race).
    let race_hides = crate::race::anark_hud::enabled() && race.is_some_and(|r| r.race.is_some());
    // Arrival clears it.
    if let (Some(t), Ok(car)) = (obj.target, cars.single()) {
        if Vec2::new(car.0.position.x, car.0.position.z).distance(t) < ARRIVE_M {
            obj.text = None;
            obj.target = None;
        }
    }
    let Some((mut sc, instr, _)) = hud_scene(&mut st.hud, &mut scenes) else { return };
    let p = &mut sc.player;
    if let Some((left, ev)) = st.instr_timer {
        let left = left - time.delta_secs();
        st.instr_timer = if left <= 0.0 {
            p.fire_at(ev, instr);
            None
        } else {
            Some((left, ev))
        };
    }
    let resets = hud.map(|h| h.resets);
    if resets == Some(0) {
        return; // the HUD's first reset is still to come
    }
    let want = if race_hides { None } else { obj.text.clone() };
    if st.synced == resets && st.shown == want {
        return;
    }
    st.synced = resets;
    match &want {
        Some(t) => {
            if let Some(l1) = p.resolve_path(instr, "InstructionText.Line1.Bar.Text") {
                p.set_text(l1, t.clone());
            }
            if let Some(l2) = p.resolve_path(instr, "InstructionText.Line2.Bar.Text") {
                p.set_text(l2, String::new());
            }
            p.set(instr, fh1_ui::names::OPACITY, fh1_ui::player::Value::Float(100.0));
            p.fire_at("SHOW", instr);
            st.instr_timer = Some((SLIDE, "SHOWN"));
        }
        // Nothing shown yet (a map with no objective): HIDE doesn't move the never-shown widget off its authored
        // placeholder ("You're INFECTED!..."), so make it transparent.
        None if st.shown.is_none() => {
            p.set(instr, fh1_ui::names::OPACITY, fh1_ui::player::Value::Float(0.0));
            st.instr_timer = None;
        }
        None => {
            p.fire_at("HIDE", instr);
            st.instr_timer = Some((SLIDE, "HIDDEN"));
        }
    }
    st.shown = want;
}

fn notifications(time: Res<Time>, mut st: ResMut<NotifyState>, mut posts: MessageReader<HudNotify>, mut scenes: Query<(Entity, &mut AnarkScene)>) {
    for n in posts.read() {
        st.queue.push_back(n.lines.clone());
    }
    let Some((mut sc, _, note)) = hud_scene(&mut st.hud, &mut scenes) else { return };
    let p = &mut sc.player;
    let dt = time.delta_secs();
    match st.note {
        None => {
            let Some(lines) = st.queue.pop_front() else { return };
            p.fire_at("SET_MODE_GENERAL", note);
            for (i, path) in ["State.DoubleTape.TEXT2", "State.DoubleTape.TEXT1", "State.DoubleTape.TEXT3"].iter().enumerate() {
                if let Some(o) = p.resolve_path(note, path) {
                    p.set_text(o, lines.get(i).cloned().unwrap_or_default());
                }
            }
            p.fire_at("SHOW", note);
            st.note = Some(("SHOW_COMPLETE", SLIDE));
        }
        Some((ev, left)) => {
            let left = left - dt;
            if left > 0.0 {
                st.note = Some((ev, left));
                return;
            }
            p.fire_at(ev, note);
            st.note = match ev {
                "SHOW_COMPLETE" => Some(("HIDE", NOTIFY_HOLD)),
                "HIDE" => Some(("HIDE_COMPLETE", SLIDE)),
                _ => None,
            };
        }
    }
}
