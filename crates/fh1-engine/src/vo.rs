//! Character VO (audio_inventory C4): the installed `audio/dialogue/<LANG>/` clips (fh1_audio::dialogue) played by
//! trigger id. `FH1_VO=0` = off; `FH1_VO_LANG` (else `FH1_RADIO_LANG`, else EN) picks the language; `FH1_VO_VOLUME`
//! scales it (default 1).
//!
//! - [`PlayVo`] message `{ trigger }`: any system can ask for a line. One line at a time; later requests queue
//!   (at most [`MAX_QUEUE`]); a trigger sounds at most once per [`TRIGGER_COOLDOWN_S`]; among a trigger's recorded
//!   lines a random one is picked, never the one played last for that trigger.
//! - The MP3 is decoded on a worker thread (fh1-radio's symphonia decoder, mono clips come out as stereo) and played 2D
//!   on `Bus::Sfx`. While a line plays (and decodes) [`VoDuck`] is true: radio.rs switches the mixer to the `VOPlaying`
//!   snapshot (music 0.25, DJ 0; hook in the report).
//! - Triggers fired by systems that exist today (no edits elsewhere; messages and `RaceState` are read):
//!   wristband tier up (`CareerNotice::Wristband`) -> `<Colour>WristbandUnlock`; popularity ranks (`CareerNotice::RankUp`)
//!   -> `249/240/230PopularityReached`, `Almost<N>PopularityReached`, `Reached1Popularity`; a nemesis race on the grid ->
//!   `NemesisPreRace<n>`, a street race on the grid -> `StreetRacePreRace`; finishing (`RaceFinished`) -> nemesis win
//!   `NemesisPostRaceWin<n>`, street `StreetRaceWin` / `StreetRaceNonWin`; the first skill / combo ever ->
//!   `PlayerDoesFirstSkills` / `PlayerDoesFirstCombo`. Which trigger the original fires for which event is INFERRED from the
//!   ids (Pinyon Q4). No barn finds, speed traps, autoshow or hub systems exist, so their ids have no hook yet.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use fh1_audio::dialogue::TriggerEntry;
use fh1_audio::pcm::{Bus, Pcm, VoiceId, VoiceParams};
use fh1_radio::decode::Mp3Stream;

use crate::progression::{event_tier, CareerNotice, EventKind, RaceFinished, Profile, TIER_NAMES};
use crate::race::{Events, RacePhase, RaceState};
use crate::sfx_bank::SfxBank;
use crate::sfx_race::sfx_allowed;

const MAX_QUEUE: usize = 3;
const TRIGGER_COOLDOWN_S: f32 = 20.0;
/// Seconds after the finish before the post-race line, after a tier / rank change before its line.
const POST_RACE_DELAY_S: f32 = 3.5;
const NOTICE_DELAY_S: f32 = 1.5;
/// The radio stays ducked this long after the line ends (the snapshot's fade).
const DUCK_TAIL_S: f32 = 0.3;

/// `FH1_VO=0` turns the character VO off.
pub fn vo_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_VO").map_or(true, |v| v != "0"))
}

/// Ask for a VO line by `DialogueScript` trigger id (e.g. `"FirstWristbandCollect"`). Unknown ids are ignored.
#[derive(Message, Clone, Debug)]
pub struct PlayVo {
    pub trigger: String,
}

/// True while a VO line plays: the radio should use the `VOPlaying` mixer snapshot.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct VoDuck(pub bool);

pub struct VoPlugin;

impl Plugin for VoPlugin {
    fn build(&self, app: &mut App) {
        if !vo_on() {
            return;
        }
        app.init_resource::<VoDuck>()
            .add_message::<PlayVo>()
            .add_systems(Startup, start_vo)
            .add_systems(Update, (vo_career, vo_race, vo_play).chain());
    }
}

#[derive(Resource)]
struct VoState {
    dir: PathBuf,
    /// Trigger id -> sample indices of its recorded lines.
    table: HashMap<String, Vec<u32>>,
    /// Lines due later: (real time, trigger).
    pending: Vec<(f32, String)>,
    queue: VecDeque<String>,
    /// A clip being decoded.
    loading: Option<String>,
    tx: Sender<(String, Option<Arc<Pcm>>)>,
    rx: Mutex<Receiver<(String, Option<Arc<Pcm>>)>>,
    playing: Option<VoiceId>,
    /// Real time the duck ends (while something plays or loads it is held open).
    duck_until: f32,
    last_clip: HashMap<String, u32>,
    last_played: HashMap<String, f32>,
    rng: u64,
    volume: f32,
}

impl VoState {
    fn request(&mut self, trigger: &str, delay: f32, now: f32) {
        if self.table.contains_key(trigger) {
            self.pending.push((now + delay, trigger.to_owned()));
        } else {
            debug!("vo: unknown trigger {trigger}");
        }
    }

    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
}

fn start_vo(mut commands: Commands, garage: Res<crate::Garage>) {
    if crate::audio::audio_disabled() {
        return;
    }
    let lang = std::env::var("FH1_VO_LANG").or_else(|_| std::env::var("FH1_RADIO_LANG")).unwrap_or_else(|_| "EN".into()).to_ascii_uppercase();
    let dir = garage.assets.join("audio/dialogue").join(&lang);
    let rows: Vec<TriggerEntry> = match std::fs::read(dir.join("triggers.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(r) => r,
        None => return info!("vo: no dialogue installed for {lang} ({}); run fh1setup to convert it", dir.display()),
    };
    let mut table: HashMap<String, Vec<u32>> = HashMap::new();
    for r in rows {
        table.entry(r.trigger).or_default().push(r.index);
    }
    info!("vo: {} triggers ({lang})", table.len());
    let (tx, rx) = channel();
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
    let volume = std::env::var("FH1_VO_VOLUME").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    commands.insert_resource(VoState {
        dir,
        table,
        pending: Vec::new(),
        queue: VecDeque::new(),
        loading: None,
        tx,
        rx: Mutex::new(rx),
        playing: None,
        duck_until: 0.0,
        last_clip: HashMap::new(),
        last_played: HashMap::new(),
        rng: seed,
        volume,
    });
}

/// Wristband (colour from the tier) for `CareerNotice::Wristband`: Green..Gold have a `<Colour>WristbandUnlock`
/// trigger; Yellow (tier 0, the start) has none.
pub fn wristband_trigger(tier: u8) -> Option<String> {
    (1..7).contains(&tier).then(|| format!("{}WristbandUnlock", TIER_NAMES[tier as usize]))
}

/// Popularity trigger for a rank change `prev` -> `rank` (rank 250 = unranked, 1 = best; INFERRED: the `N` ids are the
/// rank reached, `Almost<N>` fires 5 ranks before N). Returns the most significant crossing.
pub fn popularity_trigger(prev: u32, rank: u32) -> Option<String> {
    if rank >= prev {
        return None;
    }
    let crossed = |t: u32| prev > t && rank <= t;
    if crossed(1) {
        return Some("Reached1Popularity".into());
    }
    for n in [5u32, 10, 50, 100, 150] {
        if crossed(n + 5) {
            return Some(format!("Almost{n}PopularityReached"));
        }
    }
    [230u32, 240, 249].into_iter().find(|&t| crossed(t)).map(|t| format!("{t}PopularityReached"))
}

/// Nemesis number 1..7 for an event's wristband tier (INFERRED: one nemesis per wristband).
pub fn nemesis_number(tier: u8) -> u8 {
    tier.min(6) + 1
}

#[allow(clippy::too_many_arguments)]
fn vo_career(
    state: Option<ResMut<VoState>>,
    mut notices: MessageReader<CareerNotice>,
    mut asks: MessageReader<PlayVo>,
    mut skills: MessageReader<crate::progression::skill::SkillEvent>,
    profile: Option<Res<Profile>>,
    real: Res<Time<Real>>,
    mut prev_rank: Local<Option<u32>>,
    mut first_skill: Local<bool>,
) {
    let Some(mut st) = state else {
        notices.clear();
        asks.clear();
        skills.clear();
        return;
    };
    let now = real.elapsed_secs();
    for a in asks.read() {
        st.request(&a.trigger, 0.0, now);
    }
    for n in notices.read() {
        match n {
            CareerNotice::Wristband { tier } => {
                if let Some(t) = wristband_trigger(*tier) {
                    st.request(&t, NOTICE_DELAY_S, now);
                }
            }
            CareerNotice::RankUp { rank, .. } => {
                let prev = prev_rank.replace(*rank).unwrap_or(250);
                if let Some(t) = popularity_trigger(prev, *rank) {
                    st.request(&t, NOTICE_DELAY_S, now);
                }
            }
            _ => {}
        }
    }
    // The first skill / combo of the profile (nothing banked yet).
    for e in skills.read() {
        if let crate::progression::skill::SkillEvent::Award { combo, .. } = e {
            let fresh = profile.as_ref().is_some_and(|p| p.data.skills.best_chain == 0);
            if fresh && !*first_skill {
                *first_skill = true;
                st.request("PlayerDoesFirstSkills", 0.5, now);
            } else if fresh && *combo {
                st.request("PlayerDoesFirstCombo", 0.5, now);
            }
        }
    }
}

fn vo_race(
    state: Option<ResMut<VoState>>,
    rs: Option<Res<RaceState>>,
    events: Option<Res<Events>>,
    mut finished: MessageReader<RaceFinished>,
    real: Res<Time<Real>>,
    mut gridded: Local<Option<usize>>,
) {
    let (Some(mut st), Some(rs), Some(events)) = (state, rs, events) else {
        finished.clear();
        return;
    };
    let now = real.elapsed_secs();
    match (rs.phase, rs.race) {
        (RacePhase::Grid { .. }, Some(i)) if *gridded != Some(i) => {
            *gridded = Some(i);
            if let Some(def) = events.races.get(i) {
                match EventKind::of(def) {
                    EventKind::Nemesis => st.request(&format!("NemesisPreRace{}", nemesis_number(event_tier(def))), 0.5, now),
                    EventKind::Street => st.request("StreetRacePreRace", 0.5, now),
                    _ if def.hub > 0 => st.request("StreetRacePreRace", 0.5, now),
                    _ => {}
                }
            }
        }
        (RacePhase::Idle, _) => *gridded = None,
        _ => {}
    }
    for f in finished.read() {
        let (Some(def), Some(place)) = (events.races.get(f.race), f.place) else { continue };
        let street = EventKind::of(def) == EventKind::Street || def.hub > 0;
        if EventKind::of(def) == EventKind::Nemesis {
            if place == 1 {
                st.request(&format!("NemesisPostRaceWin{}", nemesis_number(event_tier(def))), POST_RACE_DELAY_S, now);
            }
        } else if street {
            st.request(if place == 1 { "StreetRaceWin" } else { "StreetRaceNonWin" }, POST_RACE_DELAY_S, now);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn vo_play(
    state: Option<ResMut<VoState>>,
    mut duck: ResMut<VoDuck>,
    sfx: Option<Res<SfxBank>>,
    virt: Res<Time<Virtual>>,
    real: Res<Time<Real>>,
) {
    let _watch = crate::perf::watch("vo_play");
    let Some(mut st) = state else { return };
    let st = &mut *st;
    let now = real.elapsed_secs();
    let allowed = sfx_allowed(&virt);
    // Due requests -> queue (dropped while the world can't be heard, or when the trigger spoke recently).
    let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut st.pending).into_iter().partition(|(t, _)| *t <= now);
    st.pending = rest;
    for (_, trig) in due {
        let recent = st.last_played.get(&trig).is_some_and(|t| now - t < TRIGGER_COOLDOWN_S);
        if allowed && !recent && !st.queue.contains(&trig) && st.queue.len() < MAX_QUEUE {
            st.queue.push_back(trig);
        }
    }
    let mixer = sfx.as_ref().and_then(|s| s.mixer());
    // Cover or pause: cut the line.
    if !allowed {
        if let (Some(id), Some(m)) = (st.playing.take(), mixer) {
            m.stop(id, 0.2);
        }
        st.queue.clear();
        st.loading = None;
    }
    // A decoded clip arrived.
    let arrived: Vec<_> = st.rx.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default();
    for (trig, pcm) in arrived {
        if st.loading.as_deref() != Some(trig.as_str()) {
            continue;
        }
        st.loading = None;
        if let (Some(pcm), Some(m)) = (pcm, mixer) {
            st.playing = m.play(pcm, VoiceParams { gain: st.volume, bus: Bus::Sfx, ..Default::default() });
            st.last_played.insert(trig, now);
        }
    }
    if let (Some(id), Some(m)) = (st.playing, mixer) {
        if !m.is_playing(id) {
            st.playing = None;
        }
    }
    // Next line.
    if st.playing.is_none() && st.loading.is_none() && allowed {
        if let Some(trig) = st.queue.pop_front() {
            let lines = st.table.get(&trig).cloned().unwrap_or_default();
            let last = st.last_clip.get(&trig).copied();
            let pool: Vec<u32> = lines.iter().copied().filter(|i| Some(*i) != last || lines.len() == 1).collect();
            if !pool.is_empty() {
                let idx = pool[(st.rand() % pool.len() as u64) as usize];
                st.last_clip.insert(trig.clone(), idx);
                let path = st.dir.join(format!("{idx:03}.mp3"));
                let (tx, name) = (st.tx.clone(), trig.clone());
                st.loading = Some(trig);
                let _ = std::thread::Builder::new().name("fh1-vo-decode".into()).spawn(move || {
                    let _ = tx.send((name, decode(&path)));
                });
            }
        }
    }
    let busy = st.playing.is_some() || st.loading.is_some();
    if busy {
        st.duck_until = now + DUCK_TAIL_S;
    }
    let want = busy || now < st.duck_until;
    if duck.0 != want {
        duck.0 = want;
    }
}

/// Whole clip -> stereo PCM (the decoder duplicates mono).
fn decode(path: &std::path::Path) -> Option<Arc<Pcm>> {
    let mut s = Mp3Stream::open(path, 0).map_err(|e| warn!("vo: {e:#}")).ok()?;
    let mut data = Vec::new();
    while s.decode_into(&mut data) {}
    if data.is_empty() {
        return None;
    }
    Some(Arc::new(Pcm { rate: s.rate, channels: 2, data: data.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        assert_eq!(wristband_trigger(0), None);
        assert_eq!(wristband_trigger(1).as_deref(), Some("GreenWristbandUnlock"));
        assert_eq!(wristband_trigger(6).as_deref(), Some("GoldWristbandUnlock"));
        assert_eq!(popularity_trigger(250, 249).as_deref(), Some("249PopularityReached"));
        assert_eq!(popularity_trigger(231, 230).as_deref(), Some("230PopularityReached"));
        assert_eq!(popularity_trigger(160, 155).as_deref(), Some("Almost150PopularityReached"));
        assert_eq!(popularity_trigger(2, 1).as_deref(), Some("Reached1Popularity"));
        assert_eq!(popularity_trigger(100, 100), None);
        assert_eq!(nemesis_number(0), 1);
        assert_eq!(nemesis_number(9), 7);
    }
}
