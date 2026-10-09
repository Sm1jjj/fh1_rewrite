//! World impact sounds: car vs wall / ground / car / smashable props, from the game's own `CollisionData.xml`.
//!
//! Physics code reports contacts through `fh1_engine::sfx_queue` (`sfx_world/queue.rs`, a library module: one-line hooks in
//! vehicle.rs `collide_body`, vehicle/contact.rs and smash.rs); [`drain_queue`] turns them into [`ImpactSfx`] messages
//! (surface ids resolved to `CollisionData` surface names); [`play_impacts`] picks the FEV event from `CollisionData.xml`
//! and plays it through `sfx_bank` on `Bus::Sfx`: an impact one-shot above [`IMPACT_MIN`] m/s (+ the LFE thump on hard
//! hits), a looped scrape voice while a sliding contact persists. Player, race AI and traffic share the path (3D at the
//! contact point). Needs the install's `audio/collisions/CollisionData.xml`, `audio/fev/{Collisions,Glass}.fev` and the
//! `Collisions` / `Collisions_HiRes` / `Glass` banks; without them it stays silent.
//!
//! `FH1_IMPACT_SFX=0` = off (the queue never enables, hooks cost one atomic load). `FH1_IMPACT_SFX_LOG=1` logs each
//! played event.
//!
//! ## CollisionData.xml schema (VERIFIED on the disc file, 2243 lines)
//! ```text
//! <CollisionData MSBetweenDiffCollisions="75">          min ms between two DIFFERENT collisions (global gap)
//!   <CollisionEvent Surface1="Asphalt" Surface2="CarAluminum">      403 of them; unordered pair, no duplicates
//!     <Impact3D Group="Collisions/Collisions/Asphalt/CarAluminum" Event="IMPACT_3D"/>   all 403
//!     <ImpactLFE Group="Collisions/Collisions/LFE" Event="IMPACT_LFE"/>                 89 (car-body pairs only)
//!     <Scrape3D Group="Collisions/Collisions/Asphalt/CarAluminum" Event="SCRAPE_3D"/>   89 (same pairs as the LFE)
//!   </CollisionEvent>
//!   <BreakEvent Smashable="co_housemailbox_001">                    212 of them; key = lower-case scenery type
//!     <Impact3D Group="Collisions/Smashables/Break/Smallsign" Event="IMPACT_3D"/>
//!     <Whoosh3D Group="AMB_Default/TrackAmbient/Track3D" Event="Default_Reflections_Smashable"/>   (or Default_Blank)
//!   </BreakEvent>
//! </CollisionData>
//! ```
//! - Surface names are the track's `surfaceTypes` names (same strings as `surfaces.json`: Asphalt, Dirt, Grass, GuardRail,
//!   ConcreteBarrier, RubberTire, WireFence, Smashable*, CarAluminum, CarCarbonFiber, CarPart*...), so the lookup is direct.
//!   Colorado surfaces without a pair: Leaf1-5, Litter, GravelBits, Kerb, Trackway, Concrete2Slow, CameraCollision,
//!   BowlingPinGold ([`fallback`] maps Kerb / Trackway / Concrete2Slow, the rest stay silent).
//! - `Group` = `<FEV project>/<FEV group path>`; the event is `Group` minus the project + `/` + `Event`
//!   (`Collisions/Asphalt/CarAluminum/IMPACT_3D` in `Collisions.fev`; `Smashables/Break/Bench/IMPACT_3D`).
//!   `Whoosh3D` points into `AMB_Default.fev` (a reflection trigger, not a sample): parsed, not played.
//! - There are NO speed thresholds or volume curves in the XML. The FEV events are driven by parameters (strings in
//!   `Collisions.fev`: `RelativeVelocity`, `CarSpeedMph`, `MomentumDiff`, `BreakState`, `OnRoadRoll`...); `play_event`
//!   picks a wave without them (UNKNOWN which sample the game plays at a given speed: Pinyon capture Q1).
//! - The car side of a pair is `CarAluminum` (Data_Car.MaterialTypeID 1, 119 cars) or `CarCarbonFiber` (2, 56 cars)
//!   (`List_MaterialType`, VERIFIED); the engine's `CarData` does not carry the column, so every car is `CarAluminum` (INFERRED).
//! - Glass: nothing in `CollisionData.xml` references `Glass.fev`; the car-glass events are not triggered by this module
//!   (UNKNOWN what triggers them in the game). A smashable whose type name contains "glass" plays `WINDSHIELD_SMASH`.
//!
//! Thresholds and curves below are guesses (tag GUESS): impact above 1.5 m/s, 1..15 m/s -> -24..0 dB, LFE above 8 m/s,
//! scrape above 2 m/s, per-pair cooldown 0.15 s, scrape ends 0.1 s after the last contact message.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use fh1_audio::ambient::{Bus, VoiceId};
use fh1_engine::sfx_queue::{self, Raw, RawKind};

use crate::sfx_bank::{spatial_event, Listener, SfxBank};
use crate::track::Track;

/// Closing speed (m/s) below which a contact makes no impact sound. GUESS.
pub const IMPACT_MIN: f32 = 1.5;
/// Closing speed (m/s) from which the LFE thump joins the impact. GUESS.
pub const LFE_MIN: f32 = 8.0;
/// Sliding speed (m/s) above which a contact scrapes. GUESS.
pub const SCRAPE_MIN: f32 = 2.0;
/// A scrape stops (0.15 s fade) when no contact message arrived for this long (s). GUESS.
pub const SCRAPE_GAP: f64 = 0.1;
const SCRAPE_FADE: f32 = 0.15;
/// Cooldown per contact pair (s) for impacts. GUESS.
const PAIR_COOLDOWN: f64 = 0.15;
const MAX_SCRAPES: usize = 4;
/// The car side of every pair (see the module comment).
const CAR_MATERIAL: &str = "CarAluminum";

pub fn impact_sfx_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_IMPACT_SFX").map_or(true, |v| v != "0"))
}

fn log_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_IMPACT_SFX_LOG").is_ok_and(|v| v == "1"))
}

/// What touched what (the message's `kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImpactKind {
    Wall,
    Car,
    Smash,
    Ground,
}

/// One contact frame of a car with something. `surface_a` = the other thing (world surface name, car material, or the
/// lower-case prop type for `Smash`), `surface_b` = the car's material.
#[derive(Message, Clone, Debug)]
pub struct ImpactSfx {
    pub pos: Vec3,
    /// Closing speed along the contact normal (m/s).
    pub normal_speed: f32,
    /// Sliding speed along the surface (m/s).
    pub tangent_speed: f32,
    pub kind: ImpactKind,
    pub surface_a: String,
    pub surface_b: String,
}

// ---------------------------------------------------------------------------------------------------------------------
// CollisionData.xml
// ---------------------------------------------------------------------------------------------------------------------

/// One `Group` + `Event` reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRef {
    pub group: String,
    pub event: String,
}

impl EventRef {
    /// (FEV project, event path inside it): `Collisions/Collisions/Asphalt/CarAluminum` + `IMPACT_3D` ->
    /// (`Collisions`, `Collisions/Asphalt/CarAluminum/IMPACT_3D`).
    pub fn project_path(&self) -> (&str, String) {
        match self.group.split_once('/') {
            Some((project, rest)) => (project, format!("{rest}/{}", self.event)),
            None => (self.group.as_str(), self.event.clone()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PairEntry {
    pub impact: Option<EventRef>,
    pub lfe: Option<EventRef>,
    pub scrape: Option<EventRef>,
}

#[derive(Clone, Debug, Default)]
pub struct BreakEntry {
    pub impact: Option<EventRef>,
    pub whoosh: Option<EventRef>,
}

#[derive(Debug, Default)]
pub struct CollisionData {
    /// `MSBetweenDiffCollisions` (ms).
    pub ms_between: f32,
    /// Keyed by the two lower-cased surface names, ordered (smaller first), so lookups are symmetric.
    pairs: HashMap<(String, String), PairEntry>,
    /// Keyed by the lower-cased scenery type.
    breaks: HashMap<String, BreakEntry>,
}

fn pair_key(a: &str, b: &str) -> (String, String) {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Collision surfaces the XML has no pair for, mapped to the nearest one that has.
pub fn fallback(name: &str) -> Option<&'static str> {
    Some(match name {
        "Kerb" | "Trackway" => "Concrete",
        "Concrete2Slow" => "Concrete2",
        _ => return None,
    })
}

/// Attributes of one tag body (`Name a="x" b='y'`): (tag name, attrs). Entities are not used in the file.
fn parse_tag(body: &str) -> (&str, Vec<(&str, &str)>) {
    let body = body.trim().trim_end_matches('/').trim_end();
    let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let (name, mut rest) = (&body[..name_end], body[name_end..].trim_start());
    let mut attrs = Vec::new();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
        let Some(end) = after[1..].find(q) else { break };
        attrs.push((key, &after[1..1 + end]));
        rest = after[end + 2..].trim_start();
    }
    (name, attrs)
}

fn attr<'a>(attrs: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

impl CollisionData {
    /// Parses the file text (a small tag scanner: the file is flat, with comments).
    pub fn parse(text: &str) -> Self {
        let mut out = CollisionData { ms_between: 75.0, ..Default::default() };
        enum Cur {
            None,
            Pair(String, String, PairEntry),
            Break(String, BreakEntry),
        }
        let mut cur = Cur::None;
        let mut rest = text;
        while let Some(lt) = rest.find('<') {
            rest = &rest[lt..];
            if let Some(after) = rest.strip_prefix("<!--") {
                rest = after.find("-->").map_or("", |e| &after[e + 3..]);
                continue;
            }
            let Some(gt) = rest.find('>') else { break };
            let body = &rest[1..gt];
            rest = &rest[gt + 1..];
            if body.starts_with('?') || body.starts_with('!') {
                continue;
            }
            let self_closing = body.trim_end().ends_with('/');
            if let Some(closing) = body.strip_prefix('/') {
                match (closing.trim(), std::mem::replace(&mut cur, Cur::None)) {
                    ("CollisionEvent", Cur::Pair(a, b, e)) => {
                        out.pairs.insert(pair_key(&a, &b), e);
                    }
                    ("BreakEvent", Cur::Break(k, e)) => {
                        out.breaks.insert(k.to_ascii_lowercase(), e);
                    }
                    (_, other) => cur = other,
                }
                continue;
            }
            let (name, attrs) = parse_tag(body);
            let event_ref = || Some(EventRef { group: attr(&attrs, "Group")?.to_string(), event: attr(&attrs, "Event")?.to_string() });
            match name {
                "CollisionData" => {
                    if let Some(ms) = attr(&attrs, "MSBetweenDiffCollisions").and_then(|v| v.parse().ok()) {
                        out.ms_between = ms;
                    }
                }
                "CollisionEvent" => {
                    let (a, b) = (attr(&attrs, "Surface1").unwrap_or("").to_string(), attr(&attrs, "Surface2").unwrap_or("").to_string());
                    if self_closing {
                        out.pairs.insert(pair_key(&a, &b), PairEntry::default());
                    } else {
                        cur = Cur::Pair(a, b, PairEntry::default());
                    }
                }
                "BreakEvent" => {
                    let k = attr(&attrs, "Smashable").unwrap_or("").to_string();
                    if self_closing {
                        out.breaks.insert(k.to_ascii_lowercase(), BreakEntry::default());
                    } else {
                        cur = Cur::Break(k, BreakEntry::default());
                    }
                }
                "Impact3D" => match &mut cur {
                    Cur::Pair(_, _, e) => e.impact = event_ref(),
                    Cur::Break(_, e) => e.impact = event_ref(),
                    Cur::None => {}
                },
                "ImpactLFE" => {
                    if let Cur::Pair(_, _, e) = &mut cur {
                        e.lfe = event_ref();
                    }
                }
                "Scrape3D" => {
                    if let Cur::Pair(_, _, e) = &mut cur {
                        e.scrape = event_ref();
                    }
                }
                "Whoosh3D" => {
                    if let Cur::Break(_, e) = &mut cur {
                        e.whoosh = event_ref();
                    }
                }
                _ => {}
            }
        }
        out
    }

    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let d = Self::parse(&text);
        (!d.pairs.is_empty()).then_some(d)
    }

    pub fn pair_count(&self) -> usize {
        self.pairs.len()
    }

    pub fn break_count(&self) -> usize {
        self.breaks.len()
    }

    /// The entry for two surfaces, in either order.
    pub fn pair(&self, a: &str, b: &str) -> Option<&PairEntry> {
        self.pairs.get(&pair_key(a, b))
    }

    /// [`Self::pair`] for a world surface against a car material, trying the [`fallback`] surface.
    pub fn world_pair(&self, surface: &str, car: &str) -> Option<&PairEntry> {
        self.pair(surface, car).or_else(|| self.pair(fallback(surface)?, car))
    }

    pub fn break_event(&self, prop_type: &str) -> Option<&BreakEntry> {
        self.breaks.get(&prop_type.to_ascii_lowercase())
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Curves and the scrape state machine (pure)
// ---------------------------------------------------------------------------------------------------------------------

/// Impact gain from the closing speed: 1..15 m/s -> -24..0 dB (linear in dB), clamped. GUESS.
pub fn speed_gain(v: f32) -> f32 {
    let t = ((v - 1.0) / 14.0).clamp(0.0, 1.0);
    10f32.powf((-24.0 + 24.0 * t) / 20.0)
}

/// Scrape loudness from the sliding speed: 2..20 m/s -> 0.25..1. GUESS.
pub fn scrape_gain(v: f32) -> f32 {
    0.25 + 0.75 * ((v - SCRAPE_MIN) / 18.0).clamp(0.0, 1.0)
}

/// Scrape pitch from the sliding speed: 2..20 m/s -> 0.85..1.15. GUESS.
pub fn scrape_pitch(v: f32) -> f32 {
    0.85 + 0.3 * ((v - SCRAPE_MIN) / 18.0).clamp(0.0, 1.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrapeAct {
    /// Not sliding fast enough (the scrape, if any, is left to lapse).
    Idle,
    Start,
    Update,
}

/// One scrape voice's lifetime: starts on the first sliding contact, lives while contact messages keep arriving,
/// ends [`SCRAPE_GAP`] after the last one.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrapeSm {
    pub active: bool,
    last: f64,
}

impl ScrapeSm {
    /// A contact message at `now` sliding at `tangent` m/s.
    pub fn contact(&mut self, now: f64, tangent: f32) -> ScrapeAct {
        if tangent <= SCRAPE_MIN {
            return ScrapeAct::Idle;
        }
        self.last = now;
        if self.active {
            ScrapeAct::Update
        } else {
            self.active = true;
            ScrapeAct::Start
        }
    }

    /// Whether a running scrape has lapsed at `now` (and ends it).
    pub fn expire(&mut self, now: f64) -> bool {
        if self.active && now - self.last > SCRAPE_GAP {
            self.active = false;
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Resource)]
struct ImpactData(CollisionData);

struct ScrapeSlot {
    sm: ScrapeSm,
    voice: Option<VoiceId>,
    group: String,
    event: EventRef,
    pos: Vec3,
    speed: f32,
}

#[derive(Resource, Default)]
struct ImpactState {
    cooldown: HashMap<u64, f64>,
    last_any: Option<(f64, u64)>,
    scrapes: Vec<ScrapeSlot>,
}

/// Scenery template index -> lower-case prop type (`index.json` `props.collision.smash[].type`), read once on a thread.
#[derive(Resource, Default)]
struct PropNames {
    dir: Option<PathBuf>,
    slot: Arc<Mutex<Option<HashMap<u16, String>>>>,
}

pub struct SfxWorldPlugin;

impl Plugin for SfxWorldPlugin {
    fn build(&self, app: &mut App) {
        if !impact_sfx_on() || crate::audio::audio_disabled() {
            return;
        }
        sfx_queue::set_enabled(true);
        app.add_message::<ImpactSfx>()
            .init_resource::<ImpactState>()
            .init_resource::<PropNames>()
            .add_systems(Startup, load_data)
            .add_systems(Update, (drain_queue, play_impacts).chain());
    }
}

fn load_data(mut commands: Commands, garage: Res<crate::Garage>) {
    let path = garage.assets.join("audio").join("collisions").join("CollisionData.xml");
    match CollisionData::load(&path) {
        Some(d) => {
            info!("impact sfx: {} surface pairs, {} smashables ({})", d.pair_count(), d.break_count(), path.display());
            commands.insert_resource(ImpactData(d));
        }
        None => info!("impact sfx: no CollisionData.xml at {} (run fh1setup audio); silent", path.display()),
    }
}

fn read_prop_names(dir: &Path) -> HashMap<u16, String> {
    let Ok(bytes) = std::fs::read(dir.join("index.json")) else { return HashMap::new() };
    let Ok(idx) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return HashMap::new() };
    idx["props"]["collision"]["smash"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| Some((s["n"].as_u64()? as u16, s["type"].as_str()?.to_ascii_lowercase()))).collect())
        .unwrap_or_default()
}

/// Queue (physics hooks) -> [`ImpactSfx`] messages.
fn drain_queue(
    mut out: MessageWriter<ImpactSfx>,
    track: Option<Res<Track>>,
    scenery: Option<Res<crate::scenery::Scenery>>,
    mut names: ResMut<PropNames>,
    mut buf: Local<Vec<Raw>>,
) {
    // Start the template-name read when the scenery (re)appears.
    if let Some(sc) = scenery {
        if names.dir.as_deref() != Some(sc.dir()) {
            let dir = sc.dir().to_path_buf();
            names.dir = Some(dir.clone());
            let slot = Arc::new(Mutex::new(None));
            names.slot = slot.clone();
            let _ = std::thread::Builder::new().name("fh1-impact-names".into()).spawn(move || {
                let m = read_prop_names(&dir);
                if let Ok(mut s) = slot.lock() {
                    *s = Some(m);
                }
            });
        }
    }
    buf.clear();
    sfx_queue::drain(&mut buf);
    for r in buf.drain(..) {
        let (kind, a) = match r.kind {
            RawKind::Wall | RawKind::Ground => {
                let name = track.as_ref().and_then(|t| t.world.as_ref()).map(|w| w.surface_name(r.surface)).filter(|n| *n != "?").unwrap_or("Asphalt");
                (if r.kind == RawKind::Wall { ImpactKind::Wall } else { ImpactKind::Ground }, name.to_string())
            }
            RawKind::Car => (ImpactKind::Car, CAR_MATERIAL.to_string()),
            RawKind::Smash => {
                let ty = names.slot.lock().ok().and_then(|s| s.as_ref()?.get(&r.prop).cloned());
                let Some(ty) = ty else { continue };
                (ImpactKind::Smash, ty)
            }
        };
        out.write(ImpactSfx { pos: r.pos, normal_speed: r.normal_speed, tangent_speed: r.tangent_speed, kind, surface_a: a, surface_b: CAR_MATERIAL.to_string() });
    }
}

fn hash_key(m: &ImpactSfx) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (m.kind, &m.surface_a, &m.surface_b, (m.pos.x / 3.0).floor() as i32, (m.pos.z / 3.0).floor() as i32).hash(&mut h);
    h.finish()
}

/// Plays one event reference: looks the FEV event up (`Collisions` / `Glass` project) and hands it to the bank.
fn play(bank: &SfxBank, listener: &Listener, r: &EventRef, at: Option<Vec3>, gain: f32) -> Option<VoiceId> {
    let (project, path) = r.project_path();
    let fev = bank.fev(project)?;
    let ev = fev.event(&path).or_else(|| fev.event(&format!("{}/{}", r.group, r.event)))?;
    bank.play_event(ev, at, listener, gain, Bus::Sfx)
}

/// Starts or updates one scrape slot's voice.
fn drive_scrape(bank: &SfxBank, listener: &Listener, slot: &mut ScrapeSlot) {
    let (project, path) = slot.event.project_path();
    let Some(fev) = bank.fev(project) else { return };
    let Some(ev) = fev.event(&path) else { return };
    let gain = scrape_gain(slot.speed);
    let playing = match (slot.voice, bank.mixer()) {
        (Some(v), Some(m)) => m.is_playing(v),
        _ => false,
    };
    if playing {
        if let (Some(v), Some(m)) = (slot.voice, bank.mixer()) {
            let (sg, pan) = spatial_event(listener, slot.pos, ev);
            m.set(v, ev.volume * gain * sg, pan, ev.pitch * scrape_pitch(slot.speed));
        }
    } else {
        // Also restarts a scrape event that is not a loop while the contact lasts.
        slot.voice = bank.play_event(ev, Some(slot.pos), listener, gain, Bus::Sfx);
    }
}

fn stop_scrape(bank: Option<&SfxBank>, slot: &mut ScrapeSlot) {
    if let (Some(v), Some(m)) = (slot.voice.take(), bank.and_then(|b| b.mixer())) {
        m.stop(v, SCRAPE_FADE);
    }
}

/// [`ImpactSfx`] -> sounds.
fn play_impacts(
    mut msgs: MessageReader<ImpactSfx>,
    time: Res<Time>,
    virt: Res<Time<Virtual>>,
    bank: Option<Res<SfxBank>>,
    listener: Option<Res<Listener>>,
    data: Option<Res<ImpactData>>,
    mut st: ResMut<ImpactState>,
) {
    let now = time.elapsed_secs_f64();
    // The same gate as the car audio (sfx_bank::world_sfx_allowed: agent launch, mute, pause, loading / menu cover).
    let allowed = crate::sfx_bank::world_sfx_allowed(virt.is_paused());
    let (Some(bank), Some(listener), Some(data)) = (bank.as_deref(), listener.as_deref(), data.as_deref()) else {
        msgs.clear();
        return;
    };
    let data = &data.0;
    for m in msgs.read() {
        if !allowed {
            continue;
        }
        // Which event pair?
        let (pair, brk) = match m.kind {
            ImpactKind::Smash => (None, data.break_event(&m.surface_a)),
            _ => (data.world_pair(&m.surface_a, &m.surface_b), None),
        };
        let key = hash_key(m);
        // Scrape: a sliding contact with a scrape event.
        if m.kind != ImpactKind::Smash && m.tangent_speed > SCRAPE_MIN {
            if let Some(ev) = pair.and_then(|p| p.scrape.as_ref()) {
                let at = st.scrapes.iter().position(|s| s.group == ev.group && s.pos.distance_squared(m.pos) < 64.0).or_else(|| (st.scrapes.len() < MAX_SCRAPES).then(|| {
                    st.scrapes.push(ScrapeSlot { sm: ScrapeSm::default(), voice: None, group: ev.group.clone(), event: ev.clone(), pos: m.pos, speed: 0.0 });
                    st.scrapes.len() - 1
                }));
                if let Some(i) = at {
                    let s = &mut st.scrapes[i];
                    s.pos = m.pos;
                    s.speed = s.speed.max(m.tangent_speed);
                    if s.sm.contact(now, m.tangent_speed) == ScrapeAct::Start && log_on() {
                        info!("impact sfx: scrape start {} v={:.1}", ev.group, m.tangent_speed);
                    }
                }
            }
        }
        // Impact one-shot.
        if m.normal_speed <= IMPACT_MIN {
            continue;
        }
        let cooled = st.cooldown.get(&key).is_none_or(|t| now - t >= PAIR_COOLDOWN);
        let gap_ok = st.last_any.is_none_or(|(t, k)| k == key || (now - t) * 1000.0 >= data.ms_between as f64);
        if !cooled || !gap_ok {
            continue;
        }
        let gain = speed_gain(m.normal_speed);
        let main = pair.and_then(|p| p.impact.as_ref()).or_else(|| brk.and_then(|b| b.impact.as_ref()));
        let Some(main) = main else { continue };
        if play(bank, listener, main, Some(m.pos), gain).is_some() {
            if log_on() {
                info!("impact sfx: {:?} {} / {} -> {}/{} v={:.1} gain={:.2}", m.kind, m.surface_a, m.surface_b, main.group, main.event, m.normal_speed, gain);
            }
            st.cooldown.insert(key, now);
            st.last_any = Some((now, key));
            if m.normal_speed >= LFE_MIN {
                if let Some(lfe) = pair.and_then(|p| p.lfe.as_ref()) {
                    // The LFE is a sub thump: not positional. 0.6 = GUESS.
                    let _ = play(bank, listener, lfe, None, gain * 0.6);
                }
            }
            // Glass props only (nothing in CollisionData.xml ties glass to car hits).
            if m.kind == ImpactKind::Smash && m.surface_a.contains("glass") {
                let _ = play(bank, listener, &EventRef { group: "Glass".into(), event: "WINDSHIELD_SMASH".into() }, Some(m.pos), gain);
            }
        }
    }
    if st.cooldown.len() > 512 {
        st.cooldown.retain(|_, t| now - *t < 2.0);
    }
    // Scrape upkeep: stop lapsed ones, drive the live ones, drop finished slots.
    let st = &mut *st;
    for s in &mut st.scrapes {
        if s.sm.expire(now) || !allowed {
            stop_scrape(Some(bank), s);
            s.sm.active = false;
        } else if s.sm.active {
            drive_scrape(bank, listener, s);
        }
        // The speed is a per-frame maximum.
        if s.sm.active {
            s.speed = 0.0;
        }
    }
    st.scrapes.retain(|s| s.sm.active);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<CollisionData MSBetweenDiffCollisions="75">
<!-- comment -->
<CollisionEvent Surface1="Asphalt" Surface2="CarAluminum">
	<Impact3D Group="Collisions/Collisions/Asphalt/CarAluminum" Event="IMPACT_3D"/>
  <ImpactLFE Group="Collisions/Collisions/LFE" Event="IMPACT_LFE"/>
	<Scrape3D Group="Collisions/Collisions/Asphalt/CarAluminum" Event="SCRAPE_3D"/>
</CollisionEvent>
<CollisionEvent Surface1="Kerb" Surface2="Foo"></CollisionEvent>
<CollisionEvent Surface1="Concrete" Surface2="CarAluminum">
	<Impact3D Group="Collisions/Collisions/Asphalt/CarAluminum" Event="IMPACT_3D"/>
</CollisionEvent>
<BreakEvent Smashable="CO_Bench_001">
  <Impact3D Group="Collisions/Smashables/Break/Bench" Event="IMPACT_3D"/>
  <Whoosh3D Group="AMB_Default/TrackAmbient/Track3D" Event="Default_Reflections_Smashable"/>
</BreakEvent>
</CollisionData>"#;

    #[test]
    fn parses_sample() {
        let d = CollisionData::parse(SAMPLE);
        assert_eq!(d.ms_between, 75.0);
        assert_eq!(d.pair_count(), 3);
        assert_eq!(d.break_count(), 1);
        let p = d.pair("Asphalt", "CarAluminum").unwrap();
        assert_eq!(p.lfe.as_ref().unwrap().event, "IMPACT_LFE");
        let (proj, path) = p.impact.as_ref().unwrap().project_path();
        assert_eq!((proj, path.as_str()), ("Collisions", "Collisions/Asphalt/CarAluminum/IMPACT_3D"));
        let b = d.break_event("co_bench_001").unwrap();
        assert!(b.impact.is_some() && b.whoosh.is_some());
    }

    #[test]
    fn pair_lookup_is_symmetric_and_falls_back() {
        let d = CollisionData::parse(SAMPLE);
        assert!(d.pair("CarAluminum", "Asphalt").is_some());
        assert!(d.pair("asphalt", "caraluminum").is_some());
        assert!(d.pair("Asphalt", "Nope").is_none());
        // Kerb has no pair with the car: falls back to Concrete.
        assert!(d.world_pair("Kerb", "CarAluminum").is_some());
        assert!(d.world_pair("Leaf1", "CarAluminum").is_none());
    }

    #[test]
    fn disc_file_parses() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../disc/media/audio/collisions/CollisionData.xml");
        let Some(d) = CollisionData::load(&p) else { return };
        assert_eq!(d.pair_count(), 403);
        assert_eq!(d.break_count(), 212);
        assert_eq!(d.ms_between, 75.0);
        let a = d.pair("Asphalt", "CarAluminum").unwrap();
        let b = d.pair("CarAluminum", "Asphalt").unwrap();
        assert_eq!(a.impact, b.impact);
        assert!(a.scrape.is_some() && a.lfe.is_some());
        assert!(d.break_event("co_bench_001").unwrap().impact.is_some());
        for e in d.pairs.values() {
            assert!(e.impact.is_some());
        }
    }

    #[test]
    fn speed_gain_is_monotone() {
        let mut last = 0.0;
        for i in 0..400 {
            let g = speed_gain(i as f32 * 0.1);
            assert!(g >= last && g > 0.0 && g <= 1.0);
            last = g;
        }
        assert!((speed_gain(15.0) - 1.0).abs() < 1e-6);
        assert!((speed_gain(1.0) - 10f32.powf(-24.0 / 20.0)).abs() < 1e-6);
        assert!(scrape_gain(3.0) < scrape_gain(10.0) && scrape_pitch(3.0) < scrape_pitch(10.0));
    }

    #[test]
    fn scrape_starts_updates_and_lapses() {
        let mut s = ScrapeSm::default();
        assert_eq!(s.contact(0.0, 1.0), ScrapeAct::Idle);
        assert!(!s.expire(1.0));
        assert_eq!(s.contact(1.0, 5.0), ScrapeAct::Start);
        assert_eq!(s.contact(1.05, 5.0), ScrapeAct::Update);
        assert!(!s.expire(1.10));
        assert!(s.expire(1.20));
        assert!(!s.expire(1.30), "ends once");
        assert_eq!(s.contact(2.0, 6.0), ScrapeAct::Start);
        // A slow contact does not keep it alive.
        assert_eq!(s.contact(2.05, 0.5), ScrapeAct::Idle);
        assert!(s.expire(2.2));
    }
}
