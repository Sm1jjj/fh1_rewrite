//! Race stingers (audio_inventory C2): the 3-2-1-GO countdown, checkpoints, the finish, a new personal best and the
//! results pop-up, from `UIInGame.fev` through [`crate::sfx_bank::SfxBank`] (2D, `Bus::Sfx`). `FH1_RACE_SFX=0` = off.
//!
//! No edits to race.rs / postrace.rs: the system reads the public [`RaceState`] every frame and edge-detects.
//! - Countdown (`RacePhase::Countdown { left_s }`, 3.0 -> 0): `321MomentStart<K>` when it starts (T-3), `321Count<K>` at
//!   T-2 and T-1, `321Go<K>` at GO (the frame the phase becomes `Racing`, T-0), K = Festival | Showcase | Street
//!   (Nemesis and headline races use Festival). The cue schedule is VERIFIED against the game's own "3, 2, 1, GO" steps
//!   of `COUNTDOWN_S` = 3.0 (race.rs); which event the original plays at each step is INFERRED (the `play321moment`
//!   CPlayAudioCue is commented out in game_race_flow_states.xml:223, the camera sequence plays it; Pinyon Q3).
//!   `321CountStreet` maps to the `Blank` cue in ui4audio.xml (VERIFIED), so street races are silent at T-2 / T-1.
//! - Gate passed (player `gates_done` grows, not the finishing gate): `CheckpointPassed` (wave `HUD_CheckpointPassed`).
//! - Finish (`RacePhase::Finished`): `RaceComplete<Festival|Street|Nemesis|Showcase>` + the stereo finish sequence
//!   (`Race_Finish_ST` in UIInGame_Streams, `RaceFinishStreet_ST` for street races; the `*_LFE` channels are not played:
//!   no LFE channel in a stereo mix). Which finish sound goes with which mode is INFERRED (ui_audio.md row 24).
//! - New personal best (finish time under the profile's previous best for the event; 1 s after the finish):
//!   `HUD_NewPersonalBest`. The original plays it for speed-zone / challenge PBs (ui4audio `SpeedZoneNewPB`); using it
//!   for race times is INFERRED.
//! - Entering `RacePhase::Results`: `PR_RaceResultsPopUp`.
//!
//! Cue lookup: `UIInGame.fev` event by name, else the wave of that name in `banks/<Bank>.json` (the FSB sample names),
//! so a missing FEV parse still plays the plain samples. Samples are prefetched when the grid forms.
//! Not done: `ThreeTwoOneDSPConfig` (the 3-2-1 low-pass filter, 6.5-9.5 s): the mixer has no per-bus filter; skipped.
//! Also owns the shared cue helper [`play_item`] / [`SfxNames`] that sfx_horn.rs uses.

use std::collections::HashMap;
use std::path::Path;

use bevy::prelude::*;
use fh1_audio::install::BankInfo;
use fh1_audio::pcm::{Bus, VoiceId, VoiceParams};

use crate::progression::{EventKind, Profile};
use crate::race::{Events, RaceDef, RacePhase, RaceState};
use crate::sfx_bank::{spatial, Listener, SfxBank};

/// Seconds on the countdown (race.rs `COUNTDOWN_S`; its steps are 3, 2, 1, GO).
pub const COUNTDOWN_S: f32 = 3.0;
/// The personal-best sound follows the finish stinger by this long.
const PB_DELAY_S: f32 = 1.0;

/// `FH1_RACE_SFX=0` turns the race stingers off.
pub fn race_sfx_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_SFX").map_or(true, |v| v != "0"))
}

pub struct RaceSfxPlugin;

impl Plugin for RaceSfxPlugin {
    fn build(&self, app: &mut App) {
        if !race_sfx_on() {
            return;
        }
        app.init_resource::<SfxNames>().add_systems(Update, race_sfx);
    }
}

/// The kind of race, for the cue names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Festival,
    Street,
    Showcase,
    Nemesis,
}

impl Flavor {
    /// Suffix of `RaceComplete<K>`.
    fn complete(self) -> &'static str {
        match self {
            Flavor::Festival => "Festival",
            Flavor::Street => "Street",
            Flavor::Showcase => "Showcase",
            Flavor::Nemesis => "Nemesis",
        }
    }

    /// Suffix of the `321*` cues (there is no Nemesis countdown: Festival, INFERRED).
    fn countdown(self) -> &'static str {
        match self {
            Flavor::Street => "Street",
            Flavor::Showcase => "Showcase",
            Flavor::Festival | Flavor::Nemesis => "Festival",
        }
    }
}

/// Race kind from the event (street hubs and street-type events = Street).
pub fn flavor_of(def: &RaceDef) -> Flavor {
    match EventKind::of(def) {
        EventKind::Showcase => Flavor::Showcase,
        EventKind::Nemesis => Flavor::Nemesis,
        EventKind::Street => Flavor::Street,
        _ if def.hub > 0 => Flavor::Street,
        _ => Flavor::Festival,
    }
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    Moment,
    Count,
    Go,
    Finish,
    Checkpoint,
    PersonalBest,
    Results,
}

/// One sound: a `UIInGame.fev` event name, and the plain sample (bank stem, sample name) to play when the FEV or the
/// event isn't there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub event: String,
    pub wave: Option<(&'static str, &'static str)>,
}

fn item(event: impl Into<String>, wave: Option<(&'static str, &'static str)>) -> Item {
    Item { event: event.into(), wave }
}

/// The sounds for a cue (module doc). Names are from `UIInGame.fev` / ui4audio.xml (VERIFIED strings).
pub fn items(cue: Cue, fl: Flavor) -> Vec<Item> {
    let k = fl.countdown();
    match cue {
        Cue::Moment => vec![item(format!("321MomentStart{k}"), Some(("UIInGame", "CountdownThreeTwoOne")))],
        Cue::Count if fl == Flavor::Street => Vec::new(),
        Cue::Count => vec![item(format!("321Count{k}"), None)],
        Cue::Go => vec![item(format!("321Go{k}"), Some(("UIInGame", "CountdownGo")))],
        Cue::Finish if fl == Flavor::Street => vec![item("RaceCompleteStreet", None), item("RaceFinishStreet_ST", Some(("UIInGame", "RaceFinishStreet_ST")))],
        Cue::Finish => vec![item(format!("RaceComplete{}", fl.complete()), None), item("Race_Finish_ST", Some(("UIInGame_Streams", "Race_Finish_ST")))],
        Cue::Checkpoint => vec![item("CheckpointPassed", Some(("UIInGame", "HUD_CheckpointPassed")))],
        Cue::PersonalBest => vec![item("HUD_NewPersonalBest", Some(("UIInGame", "HUD_NewPersonalBest")))],
        Cue::Results => vec![item("PR_RaceResultsPopUp", Some(("UIInGame", "PR_RaceResultsPopUp")))],
    }
}

/// The countdown cues crossed when the seconds left go from `prev` to `now` (`prev` None = the countdown just began).
/// T-3 = Moment (the start), T-2 and T-1 = Count, T-0 = Go. Each mark fires once, even across a long frame.
pub fn countdown_cues(prev: Option<f32>, now: f32) -> Vec<Cue> {
    const MARKS: [(f32, Cue); 4] = [(COUNTDOWN_S, Cue::Moment), (2.0, Cue::Count), (1.0, Cue::Count), (0.0, Cue::Go)];
    MARKS
        .iter()
        .filter(|(m, _)| match prev {
            None => *m >= COUNTDOWN_S && now <= *m,
            Some(p) => p > *m && now <= *m,
        })
        .map(|(_, c)| *c)
        .collect()
}

/// Sample name -> index per bank (`banks/<stem>.json`), loaded on first use. Fallback for events the FEV doesn't have.
#[derive(Resource, Default)]
pub struct SfxNames {
    banks: HashMap<String, HashMap<String, u32>>,
}

impl SfxNames {
    /// Index of the sample called `name` (case-insensitive, `.wav` ignored) in `<assets>/audio/banks/<stem>.json`.
    pub fn index(&mut self, assets: &Path, stem: &str, name: &str) -> Option<u32> {
        let map = self.banks.entry(stem.to_owned()).or_insert_with(|| {
            let path = assets.join("audio/banks").join(format!("{stem}.json"));
            let info: Option<BankInfo> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
            info.map(|i| i.samples.iter().enumerate().map(|(k, s)| (s.name.to_ascii_lowercase(), k as u32)).collect()).unwrap_or_default()
        });
        let want = name.strip_suffix(".wav").unwrap_or(name).to_ascii_lowercase();
        map.get(&want).copied()
    }
}

/// Everything a cue needs to play.
pub struct Ctx<'a> {
    pub sfx: &'a SfxBank,
    pub listener: &'a Listener,
    pub names: &'a mut SfxNames,
    /// `Garage.assets`.
    pub assets: &'a Path,
}

/// Queues the samples of `item` for loading (so the first play doesn't miss).
pub fn prefetch_item(ctx: &mut Ctx, project: &str, item: &Item) {
    if let Some(ev) = ctx.sfx.fev(project).and_then(|f| f.event(&item.event).cloned()) {
        for w in &ev.waves {
            ctx.sfx.prefetch(&w.bank, w.index);
        }
    } else if let Some((stem, name)) = item.wave {
        if let Some(i) = ctx.names.index(ctx.assets, stem, name) {
            ctx.sfx.prefetch(stem, i);
        }
    }
}

/// Plays `item`: the FEV event (volume, pitch, random picks, 3D from `at`) or else the plain sample. None = nothing
/// started (not found, or the sample isn't loaded yet: retry next frame).
pub fn play_item(ctx: &mut Ctx, project: &str, item: &Item, at: Option<Vec3>, gain: f32) -> Option<VoiceId> {
    if let Some(fev) = ctx.sfx.fev(project) {
        if let Some(ev) = fev.event(&item.event) {
            return ctx.sfx.play_event(ev, at, ctx.listener, gain, Bus::Sfx);
        }
    }
    let (stem, name) = item.wave?;
    let idx = ctx.names.index(ctx.assets, stem, name)?;
    let pcm = ctx.sfx.sample(stem, idx)?;
    let (mut g, pan) = at.map_or((1.0, 0.0), |p| spatial(ctx.listener, p, 5.0, 100.0));
    g *= gain;
    if g <= 0.001 {
        return None;
    }
    ctx.sfx.mixer()?.play(pcm, VoiceParams { gain: g, pan, bus: Bus::Sfx, ..Default::default() })
}

/// Whether world sound may play now (audio on, no cover, not paused).
pub fn sfx_allowed(virt: &Time<Virtual>) -> bool {
    !crate::audio::audio_disabled() && crate::ui::loading::world_audio_allowed() && !virt.is_paused()
}

#[derive(Default)]
struct Tracker {
    race: Option<usize>,
    /// Countdown seconds left last frame (None = not counting down).
    prev_left: Option<f32>,
    gates: u32,
    finished: bool,
    results: bool,
    /// The event's best time before this run.
    prev_best: Option<f32>,
    /// Cues waiting for their time (real seconds).
    due: Vec<(f32, Cue)>,
}

#[allow(clippy::too_many_arguments)]
fn race_sfx(
    rs: Option<Res<RaceState>>,
    events: Option<Res<Events>>,
    profile: Option<Res<Profile>>,
    sfx: Option<Res<SfxBank>>,
    listener: Option<Res<Listener>>,
    mut names: ResMut<SfxNames>,
    garage: Res<crate::Garage>,
    virt: Res<Time<Virtual>>,
    real: Res<Time<Real>>,
    mut tr: Local<Tracker>,
) {
    let _watch = crate::perf::watch("race_sfx");
    let (Some(rs), Some(events), Some(sfx), Some(listener)) = (rs, events, sfx, listener) else { return };
    let Some(def) = rs.race.and_then(|i| events.races.get(i)) else {
        *tr = Tracker::default();
        return;
    };
    let fl = flavor_of(def);
    let now = real.elapsed_secs();
    let allowed = sfx_allowed(&virt);
    let mut ctx = Ctx { sfx: &sfx, listener: &listener, names: &mut names, assets: &garage.assets };
    let mut fire: Vec<Cue> = Vec::new();

    match rs.phase {
        RacePhase::Idle => *tr = Tracker::default(),
        RacePhase::Grid { .. } => {
            if tr.race != rs.race {
                *tr = Tracker { race: rs.race, gates: rs.racers.first().map_or(0, |r| r.gates_done), ..Default::default() };
                tr.prev_best = profile.as_ref().and_then(|p| p.data.events.get(&def.horizon_id)).and_then(|r| r.best_time_s);
                for cue in [Cue::Moment, Cue::Count, Cue::Go, Cue::Finish, Cue::Checkpoint, Cue::Results] {
                    for it in items(cue, fl) {
                        prefetch_item(&mut ctx, "UIInGame", &it);
                    }
                }
            }
        }
        RacePhase::Countdown { left_s } => {
            fire.extend(countdown_cues(tr.prev_left, left_s));
            tr.prev_left = Some(left_s);
        }
        RacePhase::Racing | RacePhase::Finished { .. } | RacePhase::Results => {
            // Countdown -> Racing: race.rs switches at left <= 0, so the GO mark is crossed here.
            if let Some(p) = tr.prev_left.take() {
                fire.extend(countdown_cues(Some(p), 0.0));
            }
            if let Some(me) = rs.racers.first() {
                let finished = me.finished_s.is_some();
                if me.gates_done > tr.gates {
                    tr.gates = me.gates_done;
                    if !finished {
                        fire.push(Cue::Checkpoint);
                    }
                }
                if finished && !tr.finished {
                    tr.finished = true;
                    fire.push(Cue::Finish);
                    if let (Some(t), Some(best)) = (me.finished_s, tr.prev_best) {
                        if t < best {
                            tr.due.push((now + PB_DELAY_S, Cue::PersonalBest));
                        }
                    }
                }
            }
            if rs.phase == RacePhase::Results && !tr.results {
                tr.results = true;
                fire.push(Cue::Results);
            }
        }
    }
    let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut tr.due).into_iter().partition(|(t, _)| *t <= now);
    tr.due = rest;
    fire.extend(due.into_iter().map(|(_, c)| c));
    if !allowed {
        return;
    }
    for cue in fire {
        for it in items(cue, fl) {
            if play_item(&mut ctx, "UIInGame", &it, None, 1.0).is_none() {
                debug!("race sfx: {cue:?} {} not played (not found or not loaded)", it.event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_schedule() {
        // 60 fps from the first countdown frame (3.0 s left) until the phase flips to Racing (left <= 0).
        let dt = 1.0 / 60.0;
        let mut prev = None;
        let mut left = COUNTDOWN_S;
        let mut got: Vec<(Cue, f32)> = Vec::new();
        let mut t = 0.0;
        loop {
            for c in countdown_cues(prev, left) {
                got.push((c, t));
            }
            prev = Some(left);
            if left <= 0.0 {
                break;
            }
            left -= dt;
            t += dt;
        }
        let cues: Vec<Cue> = got.iter().map(|g| g.0).collect();
        assert_eq!(cues, [Cue::Moment, Cue::Count, Cue::Count, Cue::Go]);
        // T-3, T-2, T-1, T-0 within a frame.
        for (g, want) in got.iter().zip([0.0, 1.0, 2.0, 3.0]) {
            assert!((g.1 - want).abs() < 2.0 * dt, "{g:?} vs {want}");
        }
        // A 0.25 s hitch across T-2 fires it once; a jump from 3.0 straight to 0 fires the rest.
        assert_eq!(countdown_cues(Some(2.1), 1.9), [Cue::Count]);
        assert_eq!(countdown_cues(Some(3.0), -0.1), [Cue::Count, Cue::Count, Cue::Go]);
        assert!(countdown_cues(Some(2.5), 2.4).is_empty());
    }

    #[test]
    fn cue_names() {
        let names = |c, f| items(c, f).into_iter().map(|i| i.event).collect::<Vec<_>>();
        assert_eq!(names(Cue::Moment, Flavor::Festival), ["321MomentStartFestival"]);
        assert_eq!(names(Cue::Count, Flavor::Showcase), ["321CountShowcase"]);
        assert!(names(Cue::Count, Flavor::Street).is_empty());
        assert_eq!(names(Cue::Go, Flavor::Street), ["321GoStreet"]);
        assert_eq!(names(Cue::Go, Flavor::Nemesis), ["321GoFestival"]);
        assert_eq!(names(Cue::Finish, Flavor::Nemesis), ["RaceCompleteNemesis", "Race_Finish_ST"]);
        assert_eq!(names(Cue::Finish, Flavor::Street), ["RaceCompleteStreet", "RaceFinishStreet_ST"]);
        assert_eq!(names(Cue::Checkpoint, Flavor::Festival), ["CheckpointPassed"]);
    }
}
