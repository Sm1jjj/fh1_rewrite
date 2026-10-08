//! FH1's radio system (`CRadioSystem`, default.xex 0x82BD5450..0x82BDE2F0) as a pure state
//! machine: no audio, no files. The mixer drives it at the game's update rate ([`TICK`]) and plays
//! what its four channels say.
//!
//! Every rule here comes from the clean-room spec in docs/RADIO.md ("default.xex"), which cites
//! the function address for each behaviour. Comments name those addresses; a few rules that the
//! code doesn't fully prove are marked INFERRED.
//!
//! Channels (like the game's FMOD channels): music, DJ (regular / special / immediate lines and the
//! festival update body), lead (festival lead-in / lead-out) and ident. Positions are in
//! milliseconds; sync points and ends fire as callbacks when a tick crosses them.

use std::sync::Arc;

use crate::config::{self, Station};
use crate::install::{Clip, RadioData};
use crate::snapshots;

/// The radio updates when 0.03 s have accumulated (CAudioManager::Update 0x824B7EE0).
pub const TICK: f64 = 0.03;

/// The game's per-system LCG (FUN_82CFC808 / FUN_82CFC7C0).
#[derive(Debug, Clone)]
pub struct Lcg(u32);

impl Lcg {
    pub fn new(seed: u32) -> Lcg {
        Lcg(seed)
    }
    fn sample(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(0x41C6_4E6D).wrapping_add(0x3039);
        ((self.0 >> 16) & 0x7FFF) as f64
    }
    /// The 15-bit sample's scale (float at 0x82000E88, ~1/32767; exact value INFERRED).
    const K: f64 = 1.0 / 32767.0;
    pub fn rand_float(&mut self, lo: f64, hi: f64) -> f64 {
        self.sample() * Self::K * (hi - lo) + lo
    }
    /// `int(sample*K*(hi-lo) + lo + 0.5)`: the end values get half weight.
    pub fn rand_int(&mut self, lo: i64, hi: i64) -> i64 {
        (self.sample() * Self::K * (hi - lo) as f64 + lo as f64 + 0.5) as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chan {
    Music,
    Dj,
    Lead,
    Ident,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bank {
    Music,
    Vo,
}

/// What a channel is playing.
#[derive(Debug, Clone)]
pub struct Play {
    pub bank: Bank,
    pub clip: String,
    /// Unique per started clip, so the mixer knows when to open a new stream.
    pub id: u64,
    pub pos_ms: f64,
    pub len_ms: f64,
    /// The clip's sync point that fires a callback (music: IdentStart, ident: StartNextTrack).
    sync_ms: Option<f64>,
    sync_fired: bool,
}

/// A HUD post (RS HUD callback, receiver 0x82801158): shown after `delay` seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct HudPost {
    pub delay: f32,
    /// Station index 0..3 (Radio1..3, Radio4_Silent = Off). Higher indices are never posted.
    pub station: usize,
    pub title: String,
    pub artist: String,
    /// Station changed: the widget raises SHOW_STATION (logo) instead of SHOW_INFO.
    pub show_station: bool,
}

/// Item pool with per-item cooldowns (0x82BD63D0 idents, 0x82BD68C0 dialogue).
#[derive(Debug, Clone, Default)]
struct PickList {
    /// 0 = available, > 0 = cooling, -1 = group disabled (dialogue only).
    cd: Vec<i64>,
    unavailable: i64,
    no_repeat: i64,
}

impl PickList {
    fn new(len: usize, no_repeat: usize) -> PickList {
        PickList { cd: vec![0; len], unavailable: 0, no_repeat: no_repeat as i64 }
    }

    /// A uniform pick among available items; every cooling item counts down in the same walk.
    fn pick(&mut self, rng: &mut Lcg, ident: bool) -> Option<usize> {
        let n = self.cd.len() as i64;
        if n == 0 {
            return None;
        }
        let avail = n - self.unavailable - 1;
        let idx = if avail > 0 { rng.rand_int(0, avail) } else { 0 };
        let mut count = 0;
        let mut chosen = None;
        for i in 0..self.cd.len() {
            let c = self.cd[i];
            let available = if ident { c <= 0 } else { c == 0 };
            if available {
                if chosen.is_none() && count == idx {
                    chosen = Some(i);
                }
                count += 1;
            } else if c > 0 {
                self.cd[i] -= 1;
                if self.cd[i] == 0 {
                    self.unavailable -= 1;
                }
            }
        }
        if let Some(i) = chosen {
            if self.no_repeat > 0 {
                self.cd[i] = self.no_repeat;
                self.unavailable += 1;
            }
        }
        chosen
    }
}

/// Per-station runtime state.
#[derive(Debug, Clone)]
struct StationRt {
    /// Playlist indices in the game's map order (keyed by track title, byte order).
    key_order: Vec<usize>,
    track_cd: Vec<i64>,
    /// Effective playlist noRepeat: the attribute is ignored, it is track count - 1 (0x82BDBA28).
    track_no_repeat: i64,
    idents: PickList,
    regular: PickList,
    lead_in: PickList,
    lead_out: PickList,
    /// Remembered track, the position it started at (s) and the radio clock then (2.5).
    cur_track: Option<usize>,
    start_offset_s: f64,
    start_clock: f64,
    last_bookkept: Option<usize>,
}

/// Resolved track: (station index, playlist index).
type TrackRef = (usize, usize);

/// Pending station switch state (0x82BDA820).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Idle,
    FadingOut,
    FadingIn,
}

/// Listener options and inputs set from outside.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// Profile "RADIO VOLUME" (0..1).
    pub radio_volume: f32,
    /// Profile "SFX VOLUME" (0..1): 3D venue music follows it.
    pub sfx_volume: f32,
    /// Mixer GameMusic / DJModifier levels from the active snapshots.
    pub music_mix: f32,
    pub dj_mix: f32,
}

impl Default for Inputs {
    fn default() -> Self {
        Inputs { radio_volume: 1.0, sfx_volume: 1.0, music_mix: 1.0, dj_mix: 1.0 }
    }
}

/// Final channel volumes (RS +0x14C / +0x150 / +0x154).
#[derive(Debug, Clone, Copy, Default)]
pub struct Volumes {
    pub music: f32,
    pub dialogue: f32,
    pub ident: f32,
}

pub struct RadioSystem {
    data: Arc<RadioData>,
    lang: String,
    rng: Lcg,
    st: Vec<StationRt>,
    /// Index of Radio4_Silent (+0xA8) and the first 3D station (+0xB0).
    silent: usize,
    first_3d: usize,

    pub music: Option<Play>,
    pub dj: Option<Play>,
    pub lead: Option<Play>,
    pub ident: Option<Play>,
    next_play_id: u64,
    /// Channels paused (SetRadioPaused +0x15A, the silent station, or category 0).
    user_paused: bool,
    cat_paused: bool,

    current: usize,
    /// Last non-3D station (+0xB8), "user" station (+0xAC), DJ for immediate lines (+0xB4).
    last_2d: usize,
    user_station: usize,
    dj_station: usize,

    // Flags (RS +0x74..+0x80).
    dj_disabled: bool,
    dj_option: bool,
    switching_locked: bool,
    idents_audible: bool,

    // Playing / previous / last 3D / last audible track.
    track: Option<TrackRef>,
    last_3d_track: Option<TrackRef>,
    last_audible_track: Option<TrackRef>,
    use_existing_track: bool,

    // DJ scheduling (RS +0xD0..+0x145).
    pending_special: Option<(String, bool)>,
    pending_festival: Option<String>,
    lead_in_line: Option<String>,
    lead_out_line: Option<String>,
    regular_line: Option<String>,
    special_line: Option<String>,
    immediate_line: Option<String>,
    festival_item: Option<String>,
    block_ms: f64,
    tracks_since_dj: u32,
    dj_pending: bool,
    festival_pending: bool,
    retain_duck: bool,

    // Faders (RS +0x16C..+0x180, +0x160).
    level: (f32, f32),
    level_up_rate: f32,
    duck: (f32, f32),
    scale: (f32, f32),
    station_fade: f32,
    change: Change,
    change_target: Option<(usize, bool)>,
    change_mode: u32,
    /// Fade-out-then-start request (slot 11): (rate, event args).
    fade_then_start: Option<(f32, FlowArgs)>,

    // HUD (+0x159, +0x1BC, +0x230).
    hud_posted: bool,
    hud_delay: f32,
    show_station: bool,
    hud_posts: Vec<HudPost>,

    flow_running: bool,
    is_event_flow: bool,
    music_was_playing: bool,
    stop_timer: Option<f64>,
    /// Radio clock (+0x2D0), advanced only while not paused.
    clock: f64,
    /// Time-of-day bucket (+0x11C): 0 Daytime, 1 Morning, 2 Evening, 3 Night. Nothing in the game
    /// sets it (INFERRED: always 0).
    pub bucket: usize,
    /// Scripted playlist (slot 46): tracks, cursor, play once, force SongStart.
    playlist: Option<(Vec<TrackRef>, usize, bool, bool)>,

    /// Mixer snapshot the radio asked for (RadioDJSpeaking / RadioFestivalUpdate / RadioNormal).
    pub snapshot: &'static str,
    pub inputs: Inputs,
    pub volumes: Volumes,
    /// Station whose music is positional (3D venue stations Radio5..9).
    pub is_3d: bool,
    /// D-pad cooldown (CRadioDJManager +0xA4, 1.0 s, 0x82596958).
    dpad_cooldown: f64,
}

#[derive(Debug, Clone)]
struct FlowArgs {
    time: f64,
    track: String,
    immediate: bool,
    from_start: bool,
}

fn ms(clip: &Clip, frames: u64) -> f64 {
    frames as f64 * 1000.0 / clip.rate as f64
}

impl RadioSystem {
    /// Init (slot 7, 0x82BDE1B8): load config, find the silent station, seed the LCG.
    pub fn new(data: Arc<RadioData>, lang: &str, seed: u32) -> RadioSystem {
        let lang = if data.vo.contains_key(lang) { lang.to_owned() } else { "EN".to_owned() };
        let stations = &data.radio.stations;
        let silent = stations.iter().position(|s| s.is_off).unwrap_or(stations.len());
        let first_3d = silent + 1;
        let st = stations.iter().map(StationRt::new).collect();
        RadioSystem {
            rng: Lcg::new(seed),
            st,
            silent,
            first_3d,
            music: None,
            dj: None,
            lead: None,
            ident: None,
            next_play_id: 1,
            user_paused: false,
            cat_paused: false,
            current: 0,
            last_2d: 0,
            user_station: 0,
            dj_station: 0,
            dj_disabled: false,
            dj_option: true,
            switching_locked: false,
            idents_audible: true,
            track: None,
            last_3d_track: None,
            last_audible_track: None,
            use_existing_track: false,
            pending_special: None,
            pending_festival: None,
            lead_in_line: None,
            lead_out_line: None,
            regular_line: None,
            special_line: None,
            immediate_line: None,
            festival_item: None,
            block_ms: 0.0,
            tracks_since_dj: 0,
            dj_pending: false,
            festival_pending: false,
            retain_duck: false,
            level: (0.0, 0.0),
            level_up_rate: 1.0 / data.radio.system.level_fade_up_time.max(1e-3),
            duck: (1.0, 1.0),
            scale: (1.0, 1.0),
            station_fade: 1.0,
            change: Change::Idle,
            change_target: None,
            change_mode: 0,
            fade_then_start: None,
            hud_posted: true,
            hud_delay: data.radio.system.hud_new_track_delay_free_roam,
            show_station: false,
            hud_posts: Vec::new(),
            flow_running: false,
            is_event_flow: false,
            music_was_playing: false,
            stop_timer: None,
            clock: 0.0,
            bucket: 0,
            playlist: None,
            snapshot: snapshots::RADIO_NORMAL,
            inputs: Inputs::default(),
            volumes: Volumes::default(),
            is_3d: false,
            dpad_cooldown: 0.0,
            lang,
            data,
        }
    }

    pub fn data(&self) -> &RadioData {
        &self.data
    }
    pub fn clock(&self) -> f64 {
        self.clock
    }
    pub fn station(&self) -> usize {
        self.current
    }
    pub fn station_def(&self) -> &Station {
        &self.data.radio.stations[self.current]
    }
    pub fn silent_station(&self) -> usize {
        self.silent
    }
    /// The playing track: (station, playlist index).
    pub fn track(&self) -> Option<(usize, usize)> {
        self.track
    }
    pub fn flow_running(&self) -> bool {
        self.flow_running
    }
    pub fn take_hud_posts(&mut self) -> Vec<HudPost> {
        std::mem::take(&mut self.hud_posts)
    }
    /// Channels are paused (radio paused, silent station, or radio volume 0).
    pub fn paused(&self) -> bool {
        self.user_paused || self.cat_paused || self.current == self.silent
    }
    /// The DJ (or festival lead) channel, for the voice-activity duck.
    pub fn voice_channel(&self) -> Option<&Play> {
        self.dj.as_ref().or(self.lead.as_ref())
    }

    // ----- clips -----

    fn vo_clip(&self, name: &str) -> Option<&Clip> {
        self.data.vo.get(&self.lang).and_then(|b| b.get(name))
    }
    fn track_clip(&self, t: TrackRef) -> Option<&Clip> {
        let tr = &self.data.radio.stations[t.0].playlist.items[t.1];
        self.data.music.get(&tr.clip)
    }
    /// (SongStart, EventStart, IdentStart, length) in ms.
    fn track_times(&self, t: TrackRef) -> (f64, f64, f64, f64) {
        let Some(c) = self.track_clip(t) else { return (0.0, 0.0, 0.0, 0.0) };
        let len = ms(c, c.frames);
        let s = |k: &str| c.sync.get(k).map_or(0.0, |&f| ms(c, f));
        let ident = c.sync.get("IdentStart").map_or(len, |&f| ms(c, f));
        (s("SongStart"), s("EventStart"), ident, len)
    }

    fn new_play(&mut self, bank: Bank, clip: &str, pos_ms: f64, sync: Option<&str>) -> Option<Play> {
        let c = match bank {
            Bank::Music => self.data.music.get(clip),
            Bank::Vo => self.vo_clip(clip),
        }?;
        let sync_ms = sync.and_then(|k| c.sync.get(k)).map(|&f| ms(c, f));
        let p = Play {
            bank,
            clip: clip.to_owned(),
            id: self.next_play_id,
            pos_ms,
            len_ms: ms(c, c.frames),
            sync_fired: sync_ms.is_some_and(|s| pos_ms > s),
            sync_ms,
        };
        self.next_play_id += 1;
        Some(p)
    }

    fn is_audible_station(&self) -> bool {
        self.current != self.silent && !self.is_3d
    }

    // ----- picks -----

    /// Track pick (0x82BD6BF0): likelihood-weighted among eligible tracks, else the cooling track
    /// closest to eligible, else the first in map order.
    fn pick_track(&mut self, station: usize) -> Option<TrackRef> {
        let items = &self.data.radio.stations[station].playlist.items;
        if items.is_empty() {
            return None;
        }
        // Config stores likelihoods as morning, daytime, evening, night; buckets are
        // Daytime, Morning, Evening, Night.
        let li = [1, 0, 2, 3][self.bucket.min(3)];
        let rt = &self.st[station];
        let total: f64 = (0..items.len()).filter(|&i| rt.track_cd[i] <= 0).map(|i| items[i].likelihood[li] as f64).sum();
        let r = self.rng.rand_float(0.0, total);
        let rt = &self.st[station];
        let (mut acc, mut chosen, mut fallback, mut best) = (0.0, None, None, rt.track_no_repeat);
        for &i in &rt.key_order {
            if rt.track_cd[i] <= 0 {
                let w = items[i].likelihood[li] as f64;
                if chosen.is_none() && r <= acc + w {
                    chosen = Some(i);
                }
                acc += w;
            } else if rt.track_cd[i] < best {
                fallback = Some(i);
                best = rt.track_cd[i];
            }
        }
        Some((station, chosen.or(fallback).unwrap_or(rt.key_order[0])))
    }

    fn pick_dialogue(&mut self, which: fn(&mut StationRt) -> &mut PickList, pool: fn(&Station) -> Vec<String>) -> Option<String> {
        let names = pool(&self.data.radio.stations[self.current]);
        let i = which(&mut self.st[self.current]).pick(&mut self.rng, false)?;
        names.get(i).cloned()
    }

    // ----- PlayTrack (0x82BD87A0) -----

    /// Starts `t` on the music channel. Modes: 0 from 0, 1 SongStart, 2 EventStart - time,
    /// 3 SongStart (or earlier when the DJ block runs past EventStart), 4 random mid-song, 5 `time` ms.
    fn play_track(&mut self, time: f64, t: TrackRef, mode: u8) {
        let (mut t, mut mode) = (t, mode);
        // Scripted playlist replaces the track (slot 46).
        if let Some((list, cursor, once, force_song_start)) = &mut self.playlist {
            if !list.is_empty() {
                t = list[*cursor % list.len()];
                *cursor += 1;
                if *force_song_start {
                    mode = 1;
                }
                if *once && *cursor >= list.len() {
                    self.playlist = None;
                }
            }
        }
        let (song, event, ident, _) = self.track_times(t);
        let start = match mode {
            0 => 0.0,
            1 => song,
            2 => event - time * 1000.0,
            3 => {
                if self.block_ms > event - song {
                    (event - self.block_ms).max(0.0)
                } else {
                    song
                }
            }
            4 => {
                let body = ident - song;
                self.rng.rand_float(song + 0.1 * body, ident - 0.1 * body)
            }
            _ => time,
        }
        .max(0.0);
        // No-repeat bookkeeping (0x82BD6D28).
        let rt = &mut self.st[t.0];
        if rt.last_bookkept != Some(t.1) {
            rt.last_bookkept = Some(t.1);
            for c in &mut rt.track_cd {
                *c = (*c - 1).max(0);
            }
            rt.track_cd[t.1] = rt.track_no_repeat;
        }
        // One music channel: the previous song is hard-stopped.
        let clip = self.data.radio.stations[t.0].playlist.items[t.1].clip.clone();
        self.music = self.new_play(Bank::Music, &clip, start, Some("IdentStart"));
        self.track = Some(t);
        if self.current != self.silent {
            let rt = &mut self.st[self.current];
            rt.cur_track = Some(t.1);
            rt.start_offset_s = start / 1000.0;
            rt.start_clock = self.clock;
        }
        if self.is_audible_station() {
            self.last_audible_track = Some(t);
        }
        if self.current < self.first_3d {
            self.tracks_since_dj += 1;
        } else {
            self.last_3d_track = Some(t);
        }
    }

    fn next_track(&mut self) -> Option<TrackRef> {
        if self.is_3d && self.data.radio.stations[self.current].music_loops {
            return self.last_3d_track;
        }
        self.pick_track(self.current)
    }

    // ----- idents (0x82BD72D0) -----

    fn play_ident(&mut self) {
        // With silent idents or the DJ option off, 2D stations use the silent station's
        // (blank) idents.
        let from = if (!self.idents_audible || !self.dj_option) && !self.is_3d { self.silent } else { self.current };
        if from >= self.st.len() {
            return;
        }
        let Some(i) = self.st[from].idents.pick(&mut self.rng, true) else {
            // Mod stations have no idents: go straight to the next track (their IdentStart is
            // the track's last frame).
            if from == self.current && self.is_audible_station() && self.data.radio.stations[from].idents.items.is_empty() {
                self.on_ident_next();
            }
            return;
        };
        let name = self.data.radio.stations[from].idents.items[i].clone();
        self.ident = self.new_play(Bank::Vo, &name, 0.0, Some("StartNextTrack"));
    }

    // ----- DJ block (0x82BD7F08, 0x82BD9480, 0x82BD8E88, 0x82BDA0C0) -----

    fn schedule_block(&mut self) {
        let special_wins = self.pending_special.as_ref().is_some_and(|(_, prio)| *prio);
        if self.pending_festival.is_none() || special_wins {
            match self.pending_special.take() {
                None => {
                    self.regular_line = self.pick_dialogue(|s| &mut s.regular, |s| s.dj_regular.items.iter().map(|d| d.clip.clone()).collect());
                    self.block_ms = self.regular_line.clone().and_then(|l| self.vo_clip(&l).map(|c| ms(c, c.frames))).unwrap_or(0.0);
                }
                Some((event, _)) => {
                    let st = &self.data.radio.stations[self.current];
                    self.special_line = st.dj_special.iter().find(|e| e.event == event).map(|e| e.clip.clone());
                    self.block_ms = self.special_line.clone().and_then(|l| self.vo_clip(&l).map(|c| ms(c, c.frames))).unwrap_or(0.0);
                }
            }
            self.dj_pending = true;
        } else {
            let update = self.pending_festival.take().unwrap();
            self.lead_in_line = self.pick_dialogue(|s| &mut s.lead_in, |s| s.festival_lead_in.items.clone());
            self.lead_out_line = self.pick_dialogue(|s| &mut s.lead_out, |s| s.festival_lead_out.items.clone());
            let len = |n: &Option<String>| n.as_ref().and_then(|l| self.vo_clip(l).map(|c| ms(c, c.frames))).unwrap_or(0.0);
            let fest_clip = self.data.radio.festival_updates.iter().find(|e| e.event == update).map(|e| e.clip.clone());
            self.block_ms = len(&self.lead_in_line) + len(&fest_clip) + len(&self.lead_out_line);
            self.festival_item = fest_clip;
            self.festival_pending = true;
        }
    }

    fn play_dj(&mut self) {
        if !self.dj_option {
            self.play_ident();
            return;
        }
        self.dj = None;
        let (line, snap) = if let Some(f) = self.festival_item.clone() {
            (Some(f), snapshots::RADIO_FESTIVAL_UPDATE)
        } else {
            (self.special_line.clone().or(self.regular_line.clone()).or(self.immediate_line.clone()), snapshots::RADIO_DJ_SPEAKING)
        };
        if let Some(l) = line {
            self.dj = self.new_play(Bank::Vo, &l, 0.0, None);
            if self.dj.is_some() {
                self.snapshot = snap;
            }
        }
    }

    fn play_lead(&mut self) {
        let line = self.lead_in_line.clone().or(self.lead_out_line.clone());
        if let Some(l) = line {
            self.lead = self.new_play(Bank::Vo, &l, 0.0, None);
            self.snapshot = snapshots::RADIO_FESTIVAL_UPDATE;
        }
    }

    fn clear_dj_items(&mut self) {
        self.regular_line = None;
        self.special_line = None;
        self.immediate_line = None;
        self.festival_item = None;
        self.lead_in_line = None;
        self.lead_out_line = None;
        self.dj_pending = false;
        self.festival_pending = false;
        self.block_ms = 0.0;
    }

    // ----- callbacks -----

    /// Music callback (0x82BD7F08).
    fn on_music_ident_start(&mut self) {
        if self.current == self.silent {
            return;
        }
        if self.current < self.first_3d {
            if !self.dj_option {
                return;
            }
            self.play_ident();
            if !self.dj_disabled && self.tracks_since_dj >= self.data.radio.system.max_tracks_between_dj {
                self.schedule_block();
            }
        } else {
            self.play_ident();
        }
    }

    fn on_music_end(&mut self) {
        self.music = None;
        self.track = None;
        if self.ident.is_none() && (!self.dj_option || self.current == self.silent) {
            self.play_ident();
        }
    }

    /// Ident callback (0x82BDA948) at StartNextTrack.
    fn on_ident_next(&mut self) {
        self.music = None;
        if (!self.dj_pending && !self.festival_pending) || self.dj_disabled {
            if let Some(t) = self.next_track() {
                self.hud_delay = self.data.radio.system.hud_new_track_delay_free_roam;
                self.play_track(0.0, t, 1);
                self.hud_posted = false;
            }
        } else {
            self.dj_pending = false;
            self.tracks_since_dj = 0;
            if self.lead_in_line.is_some() {
                self.play_lead();
            } else {
                self.play_dj();
            }
            if !self.festival_pending && self.dj_option {
                if let Some(t) = self.next_track() {
                    self.hud_delay = self.data.radio.system.hud_new_track_delay_free_roam;
                    self.play_track(0.0, t, 3);
                }
            }
        }
    }

    /// DJ channel END (0x82BD8E88).
    fn on_dj_end(&mut self) {
        self.dj = None;
        if self.festival_pending {
            self.festival_pending = false;
            if let Some(t) = self.next_track() {
                self.play_track(0.0, t, 1);
            }
        }
        self.immediate_line = None;
        if self.lead_out_line.is_some() {
            self.play_lead();
        } else {
            self.hud_posted = false;
            self.snapshot = snapshots::RADIO_NORMAL;
            self.retain_duck = false;
        }
        self.regular_line = None;
        self.special_line = None;
        self.festival_item = None;
    }

    /// Lead channel END (0x82BD9A30).
    fn on_lead_end(&mut self) {
        self.lead = None;
        if self.lead_in_line.take().is_some() {
            self.play_dj();
        } else if self.lead_out_line.take().is_some() {
            self.block_ms = 0.0;
            self.hud_posted = false;
            self.snapshot = snapshots::RADIO_NORMAL;
        }
    }

    // ----- flows (section 6) -----

    /// StartFlow (0x82BDA1F8).
    fn start_flow(&mut self, a: FlowArgs) {
        let sys = self.data.radio.system.clone();
        if !a.track.is_empty() {
            // Event flow.
            if self.last_2d == self.silent {
                return;
            }
            if self.is_3d {
                self.switch_to(self.last_2d, false);
            }
            let name = match (self.use_existing_track, self.last_audible_track) {
                (true, Some(t)) => self.data.radio.stations[t.0].playlist.items[t.1].clip.clone(),
                _ => a.track.clone(),
            };
            let t = if name == "Random" {
                self.pick_track(self.user_station)
            } else {
                self.find_track(&name).or_else(|| self.find_track("R1_Everyday"))
            };
            let Some(t) = t else { return };
            if t.0 != self.current {
                self.switch_to(t.0, false);
            }
            self.hud_delay = sys.hud_new_track_delay_race;
            self.play_track(a.time, t, if a.from_start { 0 } else { 2 });
            self.level_up_rate = 1.0 / sys.level_fade_up_time_for_event.max(1e-3);
            self.retain_duck = false;
        } else {
            self.hud_delay = if a.immediate { 0.0 } else { sys.hud_new_track_delay_free_roam };
            let cur = self.current;
            if self.is_3d {
                if let Some(t) = self.pick_track(cur) {
                    self.play_track(0.0, t, 2);
                }
            } else if let Some(i) = self.st[cur].cur_track {
                // Resume as if it had kept playing; past 90% of the body, a new track.
                let t = (cur, i);
                let (song, _, ident, _) = self.track_times(t);
                let pos = (self.clock - self.st[cur].start_clock) * 1000.0 + self.st[cur].start_offset_s * 1000.0;
                if pos < ident - 0.1 * (ident - song) {
                    self.play_track(pos, t, 5);
                } else if let Some(n) = self.pick_track(cur) {
                    self.play_track(0.0, n, 1);
                }
            } else if let Some(t) = self.pick_track(cur) {
                self.play_track(0.0, t, 4);
            }
            self.level_up_rate = 1.0 / sys.level_fade_up_time.max(1e-3);
        }
        self.level.0 = 1.0;
        self.hud_posted = false;
        self.flow_running = true;
        self.music_was_playing = self.music.is_some();
        if !a.immediate && self.change == Change::Idle {
            self.station_fade = 0.0;
            self.change = Change::FadingIn;
        }
    }

    fn find_track(&self, clip: &str) -> Option<TrackRef> {
        self.data.radio.stations.iter().enumerate().find_map(|(s, st)| st.playlist.items.iter().position(|t| t.clip == clip).map(|i| (s, i)))
    }

    /// StartRadioFlowForFreeRoam(fade, immediate) (CAudioManager 0x824C1DF0).
    pub fn start_free_roam(&mut self, fade: f32, immediate: bool) {
        self.is_event_flow = false;
        let args = FlowArgs { time: 3.0, track: String::new(), immediate, from_start: false };
        // No fade, or the saved station is Off: start now.
        if fade <= 0.0 || self.last_2d == self.silent {
            self.start_flow(args);
        } else {
            self.fade_then_start = Some((1.0 / fade, FlowArgs { immediate: false, ..args }));
        }
    }

    /// StartRadioFlowForEvent(track, timeToEvent, fade, fromStart) (0x824BDED8): the track's
    /// EventStart is reached `time_to_event` s later; `"Random"` = a random track of the user station.
    pub fn start_event(&mut self, track: &str, time_to_event: f32, fade: f32, from_start: bool) {
        self.is_event_flow = true;
        let args = FlowArgs { time: time_to_event as f64, track: track.to_owned(), immediate: false, from_start };
        if fade <= 0.0 {
            self.start_flow(args);
        } else {
            self.fade_then_start = Some((1.0 / fade, args));
        }
    }

    /// StopRadio(delay) (slot 14, 0x82BD7450): fade down over levelFadeDownTime, then stop.
    pub fn stop(&mut self, delay: f32) {
        if delay > 0.0 {
            self.stop_timer = Some(delay as f64);
            return;
        }
        if self.is_3d {
            self.switch_to(self.last_2d, false);
        }
        self.level.0 = 0.0;
        self.flow_running = false;
    }

    /// SetRadioPaused / SetRadioUnpaused (slot 51).
    pub fn set_paused(&mut self, paused: bool) {
        self.user_paused = paused;
    }

    // ----- options and triggers -----

    /// EnableRadioDJ / DisableRadioDJ (+0x74). Disabled = only immediate dialogue.
    pub fn enable_dj(&mut self, on: bool) {
        self.dj_disabled = !on;
    }

    /// The "RADIO DJ" profile option (SetRadioDJOn / Off, +0x78). Off also drops the idents.
    pub fn set_dj_option(&mut self, on: bool) {
        self.dj_option = on;
        if !on {
            // Slot 21 (0x82BD7AB0).
            self.clear_dj_items();
            self.dj = None;
            self.lead = None;
            if self.music.is_none() {
                self.play_ident();
            }
            self.snapshot = snapshots::RADIO_NORMAL;
        }
    }

    pub fn dj_option(&self) -> bool {
        self.dj_option
    }

    /// Enable/DisableRadioStationSwitching (+0x7C).
    pub fn set_switching_enabled(&mut self, on: bool) {
        self.switching_locked = !on;
    }

    /// Enable/DisableRadioSilentIdents (+0x80): enabled = blank idents.
    pub fn set_silent_idents(&mut self, on: bool) {
        self.idents_audible = !on;
    }

    /// RadioTriggerDJFestivalUpdate (slot 16): played at the next transition.
    pub fn trigger_festival(&mut self, event: &str, only_if_empty: bool) -> bool {
        if only_if_empty && self.pending_festival.is_some() {
            return true;
        }
        let found = self.data.radio.festival_updates.iter().any(|e| e.event == event);
        self.pending_festival = found.then(|| event.to_owned());
        found
    }

    /// RadioTriggerDJSpecialDialogue (slot 17): a newer trigger replaces an older one.
    pub fn trigger_special(&mut self, event: &str, priority: bool) {
        self.pending_special = Some((event.to_owned(), priority));
    }

    /// RadioTriggerDJImmediateDialogue (slot 18): only while the DJ is disabled (+0x74).
    pub fn trigger_immediate(&mut self, event: &str, duck: bool) -> bool {
        if !self.dj_disabled {
            return false;
        }
        let st = &self.data.radio.stations[self.dj_station.min(self.data.radio.stations.len() - 1)];
        let Some(e) = st.dj_immediate.iter().find(|e| e.event == event && !config::is_blank(&e.clip)) else { return false };
        self.immediate_line = Some(e.clip.clone());
        self.retain_duck = duck;
        self.play_dj();
        true
    }

    /// RadioDJRegularDialogueEnable(group, enable) (slot 32), on every DJ station.
    pub fn enable_dialogue_group(&mut self, group: u32, enable: bool) {
        for s in 0..self.silent.min(self.st.len()) {
            let groups: Vec<u32> = self.data.radio.stations[s].dj_regular.items.iter().map(|d| d.group).collect();
            let l = &mut self.st[s].regular;
            for (i, g) in groups.into_iter().enumerate() {
                if g != group {
                    continue;
                }
                match (enable, l.cd[i]) {
                    (true, -1) => {
                        l.cd[i] = 0;
                        l.unavailable -= 1;
                    }
                    (false, 0) => {
                        l.cd[i] = -1;
                        l.unavailable += 1;
                    }
                    (false, c) if c > 0 => l.cd[i] = -1,
                    _ => {}
                }
            }
        }
    }

    /// RadioSystemSetPlaylist(names) (slot 46).
    pub fn set_playlist(&mut self, clips: &[&str], play_once: bool, force_song_start: bool) {
        let list: Vec<TrackRef> = clips.iter().filter_map(|c| self.find_track(c)).collect();
        self.playlist = (!list.is_empty()).then_some((list, 0, play_once, force_song_start));
        self.dj_option = true;
    }

    pub fn cancel_playlists(&mut self) {
        self.playlist = None;
    }

    /// RadioSystemUseExistingTrackOnRestart (slot 52).
    pub fn use_existing_track_on_restart(&mut self, on: bool) {
        self.use_existing_track = on;
        if !on {
            self.last_audible_track = None;
        }
    }

    /// RadioSystemSetRetainMusicDuck (slot 38).
    pub fn set_retain_music_duck(&mut self, on: bool) {
        self.retain_duck = on;
    }

    /// RadioSystemShowHud (slot 53): re-post with the logo.
    pub fn show_hud(&mut self) {
        self.hud_posted = false;
        self.show_station = true;
    }

    /// RadioSystemReset (slot 54): clear every pending / scheduled DJ item.
    pub fn reset(&mut self) {
        self.clear_dj_items();
        self.pending_festival = None;
        self.pending_special = None;
    }

    /// RadioSystemStopCurrentDJSpeech (slot 25).
    pub fn stop_dj_speech(&mut self) {
        self.ident = None;
        self.lead = None;
        self.dj = None;
    }

    // ----- station switching (section 5) -----

    /// The D-pad (0x824FABD8): Radio1 -> 2 -> 3 -> Off, wrapping; 1 s cooldown; needs radio volume.
    /// Mod stations sit before Off, so the wrap point is the silent station (3 on the disc).
    /// Returns the new dial position when it switched.
    pub fn dpad(&mut self, left: bool) -> Option<usize> {
        if self.inputs.radio_volume <= 0.0 || self.dpad_cooldown > 0.0 || self.switching_locked {
            return None;
        }
        let off = self.silent;
        let cur = self.user_station.min(off);
        let new = if left { if cur == 0 { off } else { cur - 1 } } else if cur == off { 0 } else { cur + 1 };
        self.dpad_cooldown = 1.0;
        self.user_station = new;
        self.select_external(new, true, 2);
        Some(new)
    }

    /// SetRadioStationByExternalIndex(idx, showStation, mode) (slot 29): mode 0 = immediate,
    /// 2 = fade out / switch / fade in with an immediate HUD post.
    pub fn select_external(&mut self, idx: usize, show_station: bool, mode: u32) {
        if idx >= self.data.radio.stations.len() {
            return;
        }
        if idx <= self.silent {
            self.user_station = idx;
        }
        self.change_mode = mode;
        self.show_station = show_station;
        self.select_station(idx, false);
    }

    /// SelectStation (slot 30, 0x82BDA720).
    fn select_station(&mut self, idx: usize, forced: bool) {
        if !matches!(self.change, Change::Idle | Change::FadingIn) || (self.switching_locked && !forced) || idx == self.current {
            return;
        }
        let user = self.flow_running;
        if self.change_mode != 0 {
            if self.change == Change::FadingIn {
                self.switch_to(idx, user);
            } else {
                self.change_target = Some((idx, user));
                self.change = Change::FadingOut;
            }
        } else {
            self.switch_to(idx, user);
        }
    }

    /// Switch (0x82BD9698).
    fn switch_to(&mut self, idx: usize, user: bool) {
        let was_audible = self.is_audible_station();
        self.current = idx;
        self.is_3d = idx >= self.first_3d;
        if !self.is_3d {
            self.last_2d = idx;
        }
        self.dj_station = idx;
        self.ident = None;
        self.lead = None;
        let audible = self.is_audible_station();
        if self.festival_pending && was_audible && audible {
            // A running festival block survives; its lead-out is re-picked from the new station.
            if self.lead_out_line.is_some() {
                self.lead_out_line = self.pick_dialogue(|s| &mut s.lead_out, |s| s.festival_lead_out.items.clone());
            }
        } else {
            self.dj = None;
            self.regular_line = None;
            if !audible {
                self.clear_dj_items();
            }
            self.snapshot = snapshots::RADIO_NORMAL;
        }
        self.scale.0 = if audible { 1.0 } else { 0.0 };
        if user {
            let mode = self.change_mode;
            self.music = None;
            self.start_flow(FlowArgs { time: 0.0, track: String::new(), immediate: mode == 0, from_start: false });
            if mode == 2 || mode == 1 {
                self.hud_delay = 0.0;
                self.show_station = true;
            }
            if mode != 0 {
                self.change = Change::FadingIn;
            }
        }
    }

    /// 3D venue station on (RadioSystemSetIs3DStation(1, name)) or off (back to the last 2D one).
    pub fn set_3d_station(&mut self, name: Option<&str>) {
        let idx = match name {
            Some(n) => self.data.radio.stations.iter().position(|s| s.name == n),
            None => Some(self.last_2d),
        };
        if let Some(i) = idx {
            self.change_mode = 0;
            self.select_station(i, true);
            if !self.flow_running {
                self.start_free_roam(0.0, true);
            }
        }
    }

    // ----- update (0x82BDAD10) -----

    /// One radio update. `voice_active` = the DJ/lead output is above the voice threshold
    /// (mean square of 256 samples > 0.001), measured by the mixer.
    pub fn update(&mut self, voice_active: bool) {
        let dt = TICK;
        let sys = self.data.radio.system.clone();
        self.dpad_cooldown = (self.dpad_cooldown - dt).clamp(0.0, 10.0);

        // Category volume (+0x148): radio volume x mixer music level; 3D: station volume x SFX.
        let category = if self.is_3d {
            self.data.radio.stations[self.current].volume * self.inputs.sfx_volume * self.inputs.music_mix
        } else {
            self.inputs.radio_volume * self.inputs.music_mix
        };
        self.cat_paused = category <= 0.0 && self.current != self.silent;

        // Channels advance and fire their callbacks.
        if !self.paused() {
            self.clock += dt;
            self.advance(dt * 1000.0);
        }

        // Faders.
        let step = |f: &mut (f32, f32), up: f32, down: f32| {
            let r = if f.0 > f.1 { up } else { down } * dt as f32;
            f.1 += (f.0 - f.1).clamp(-r, r);
        };
        let rate = |t: f32| if t <= 0.0 { f32::MAX } else { 1.0 / t };
        let falling = self.level.0 < self.level.1;
        step(&mut self.level, self.level_up_rate, rate(sys.level_fade_down_time));
        if falling && self.level.1 <= self.level.0 && self.level.0 <= 0.0 {
            // 0x82BD5A28: a finished fade-down stops everything.
            self.music = None;
            self.dj = None;
            self.lead = None;
            self.ident = None;
            self.track = None;
            self.clear_dj_items();
        }
        self.duck.0 = if (voice_active && !self.user_paused) || self.retain_duck { sys.duck_value } else { 1.0 };
        step(&mut self.duck, rate(sys.duck_fade_up_time), rate(sys.duck_fade_down_time));
        step(&mut self.scale, rate(sys.scale_fade_up_time), rate(sys.scale_fade_down_time));

        // Station change (0x82BDA820) and fade-then-start (slot 11).
        let change_rate = rate(sys.station_change_fade_time_normal) * dt as f32;
        if let Some((r, args)) = self.fade_then_start.clone() {
            self.station_fade -= r * dt as f32;
            if self.station_fade <= 0.0 {
                self.station_fade = 0.0;
                self.fade_then_start = None;
                self.start_flow(args);
            }
        } else {
            match self.change {
                Change::FadingOut => {
                    self.station_fade -= change_rate;
                    if self.station_fade <= 0.0 {
                        self.station_fade = 0.0;
                        self.change = Change::Idle;
                        if let Some((idx, user)) = self.change_target.take() {
                            self.switch_to(idx, user);
                        }
                        if self.change == Change::Idle && self.change_mode != 0 {
                            self.change = Change::FadingIn;
                        }
                    }
                }
                Change::FadingIn => {
                    self.station_fade += change_rate;
                    if self.station_fade >= 1.0 {
                        self.station_fade = 1.0;
                        self.change = Change::Idle;
                    }
                }
                Change::Idle => {}
            }
        }

        // Delayed stop.
        if let Some(t) = &mut self.stop_timer {
            *t -= dt;
            if *t <= 0.0 {
                self.stop_timer = None;
                self.stop(0.0);
            }
        }

        // Safety net: music had been playing, nothing is now.
        let any = self.music.is_some() || self.dj.is_some() || self.lead.is_some() || self.ident.is_some();
        if self.flow_running && self.music_was_playing && !any && self.level.0 > 0.0 {
            if let Some(t) = self.pick_track(self.current) {
                self.play_track(0.0, t, 1);
            }
        }
        self.music_was_playing = self.music.is_some();

        // Volumes.
        let base = sys.master_level * self.station_fade * self.scale.1 * self.level.1 * category;
        self.volumes = Volumes {
            music: sys.music_level * self.duck.1 * base,
            dialogue: self.inputs.dj_mix * sys.dialogue_level * base,
            ident: sys.ident_level * base,
        };

        // HUD post.
        if !self.hud_posted && !self.is_3d && category > 0.0 && self.current <= self.silent {
            let (title, artist) = match (self.current == self.silent, self.track) {
                (false, Some(t)) => {
                    let tr = &self.data.radio.stations[t.0].playlist.items[t.1];
                    (tr.title.clone(), tr.artist.clone())
                }
                _ => (String::new(), String::new()),
            };
            self.hud_posts.push(HudPost { delay: self.hud_delay, station: self.current, title, artist, show_station: self.show_station });
            self.hud_posted = true;
            self.show_station = false;
        }
    }

    /// Advances every channel by `d` ms and fires sync / end callbacks in channel order.
    fn advance(&mut self, d: f64) {
        #[derive(PartialEq)]
        enum Ev {
            Sync,
            End,
        }
        let tick = |p: &mut Option<Play>| -> Vec<Ev> {
            let mut ev = Vec::new();
            if let Some(play) = p {
                play.pos_ms += d;
                if let Some(s) = play.sync_ms {
                    if !play.sync_fired && play.pos_ms >= s {
                        play.sync_fired = true;
                        ev.push(Ev::Sync);
                    }
                }
                if play.pos_ms >= play.len_ms {
                    ev.push(Ev::End);
                }
            }
            ev
        };
        // Advance every channel first, then fire the callbacks, so a clip started by a callback
        // begins at its first frame on the next update.
        let ids = |p: &Option<Play>| p.as_ref().map(|p| p.id);
        let (m, i, j, l) = (tick(&mut self.music), tick(&mut self.ident), tick(&mut self.dj), tick(&mut self.lead));
        let (mid, iid, jid, lid) = (ids(&self.music), ids(&self.ident), ids(&self.dj), ids(&self.lead));
        for e in m {
            if ids(&self.music) != mid {
                break;
            }
            match e {
                Ev::Sync => self.on_music_ident_start(),
                Ev::End => self.on_music_end(),
            }
        }
        for e in i {
            if ids(&self.ident) != iid {
                break;
            }
            match e {
                Ev::Sync => self.on_ident_next(),
                Ev::End => self.ident = None,
            }
        }
        if j.contains(&Ev::End) && ids(&self.dj) == jid {
            self.on_dj_end();
        }
        if l.contains(&Ev::End) && ids(&self.lead) == lid {
            self.on_lead_end();
        }
    }
}

impl StationRt {
    fn new(s: &Station) -> StationRt {
        let mut key_order: Vec<usize> = (0..s.playlist.items.len()).collect();
        key_order.sort_by(|&a, &b| s.playlist.items[a].title.as_bytes().cmp(s.playlist.items[b].title.as_bytes()));
        let n = s.playlist.items.len();
        StationRt {
            key_order,
            track_cd: vec![0; n],
            track_no_repeat: n.saturating_sub(1) as i64,
            idents: PickList::new(s.idents.items.len(), s.idents.no_repeat),
            regular: PickList::new(s.dj_regular.items.len(), s.dj_regular.no_repeat),
            lead_in: PickList::new(s.festival_lead_in.items.len(), s.festival_lead_in.no_repeat),
            lead_out: PickList::new(s.festival_lead_out.items.len(), s.festival_lead_out.no_repeat),
            cur_track: None,
            start_offset_s: 0.0,
            start_clock: 0.0,
            last_bookkept: None,
        }
    }
}

#[cfg(test)]
#[path = "system_tests.rs"]
mod tests;
