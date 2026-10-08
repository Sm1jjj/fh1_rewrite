//! Career progression (docs/PROGRESSION.md): FH1's wristbands, popularity and rewards from gamedb, with a few
//! improvements (no grind walls, replays pay, a career screen that says what's next, a fitting AI field).
//!
//! FH1 (gamedb, verified on the EU disc): 7 wristbands (CareerWristbandLevels: Yellow 0, Green 220, Blue 750, Pink 1450,
//! Orange 2500, Purple 4100, Gold 6400 XP); race XP per place by the worn wristband (WristbandScoring); each event has a
//! wristband level (Events.Level) and an XP requirement (UnlockPointsReq); street races live in 3 hubs (EventHubs, 310 /
//! 1700 / 4500 XP, EventHubInitialEvents); showcases / exhibitions need a popularity rank (PopularityPointsReq = rank,
//! Fame.xml ladder 249 -> 1); one nemesis race per wristband (NemesisBonus credits); the Headline needs 10,000 XP.
//! Credits = Events.CashPrize x EventScoring per place / 1000 (street races pay the top three, the Headline only the winner).
//!
//! Ours on top: replays always pay (half XP / credits unless the place improves), every unlocked event stays open,
//! popularity milestones pay credits (SponsorshipChallenges FameRankChallenge amounts), the "recommended" next event.
//!
//! Flags: `FH1_PROGRESSION=0` = everything unlocked, nothing saved or paid (old behaviour). Skills: progression/skill.rs.
//!
//! Shared with the world map (e4): `EventCatalog` (every event + state, `generation` bumps on change), `StartEvent`
//! (start an unlocked event: teleport to its grid), `OpenCareer` (open the career screen).

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::race::{Events, RaceDef};
use crate::Car;

pub mod data;
pub mod profile;
pub mod screen;
pub mod skill;

pub use profile::Profile;

/// Wristband names by tier (0..6), gamedb CareerWristbandLevels.
pub const TIER_NAMES: [&str; 7] = ["Yellow", "Green", "Blue", "Pink", "Orange", "Purple", "Gold"];

/// The wristband's colour (CareerWristbandLevels.Color, ARGB).
pub fn tier_color(tier: u8) -> Color {
    const ARGB: [u32; 7] = [0xFFFFDA00, 0xFF42CC42, 0xFF0467FC, 0xFFE51F79, 0xFFFF5B00, 0xFF8D23FF, 0xFFBCA12E];
    let c = ARGB[(tier as usize).min(6)];
    Color::srgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// `FH1_PROGRESSION=0`: no career (all events open, nothing saved / paid).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_PROGRESSION").map_or(true, |v| v != "0"))
}

/// Replays (no better place than before) pay this share of XP and credits.
const REPLAY_SHARE: f32 = 0.5;
/// Popularity milestones (SponsorshipChallenges FameRankChallenge): rank reached -> credits.
const RANK_PAYOUTS: [(u32, i64); 10] =
    [(225, 5_000), (200, 10_000), (175, 15_000), (150, 20_000), (125, 25_000), (100, 30_000), (75, 35_000), (50, 40_000), (25, 50_000), (1, 100_000)];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Circuit,
    Sprint,
    Street,
    Drag,
    Elimination,
    Showcase,
    Headline,
    Nemesis,
    PrStunt,
    Other,
}

impl EventKind {
    pub fn of(def: &RaceDef) -> Self {
        match (def.mode, def.career_type) {
            (_, 8) => Self::Showcase,
            (2, _) => Self::Sprint,
            (3, _) => Self::Circuit,
            (4, _) => Self::Elimination,
            (5, _) | (_, 4) => Self::Street,
            (6, _) => Self::Drag,
            (7 | 8 | 9 | 14, _) => Self::Showcase,
            (12, _) | (_, 11) => Self::Nemesis,
            (13, _) | (_, 12) => Self::Headline,
            _ if def.circuit => Self::Circuit,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Circuit => "Circuit",
            Self::Sprint => "Sprint",
            Self::Street => "Street race",
            Self::Drag => "Drag",
            Self::Elimination => "Elimination",
            Self::Showcase => "Showcase",
            Self::Headline => "Headline",
            Self::Nemesis => "Nemesis",
            Self::PrStunt => "PR stunt",
            Self::Other => "Event",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventState {
    Locked,
    Unlocked,
    /// Finished at least once; best place (1-based).
    Completed { place: u8 },
}

#[derive(Clone, Debug)]
pub struct EventInfo {
    /// HorizonEventID ("FR02"), unique.
    pub id: String,
    pub name: String,
    pub kind: EventKind,
    /// "B 500" (+ restriction text), None = open.
    pub class: Option<String>,
    /// Wristband tier 0..6 (intro events = 0).
    pub tier: u8,
    /// 0 = festival map, 1..3 = street-race hubs.
    pub hub: u8,
    /// Marker, engine space.
    pub pos: Vec3,
    pub yaw: f32,
    pub state: EventState,
    /// Credits for a win.
    pub reward: Option<u32>,
    pub laps: Option<u8>,
    pub length_m: Option<f32>,
    /// The career's "next up" pick (one event at most).
    pub recommended: bool,
    /// Why it's locked ("Blue wristband", "Popularity #150", "1,700 XP").
    pub lock_reason: Option<String>,
    /// Index into `race::Events::races`.
    pub race: usize,
}

/// Every installed event and its state for the map / career screen. `generation` bumps on any change.
#[derive(Resource, Default)]
pub struct EventCatalog {
    pub events: Vec<EventInfo>,
    pub generation: u32,
}

impl EventCatalog {
    pub fn get(&self, id: &str) -> Option<&EventInfo> {
        self.events.iter().find(|e| e.id.eq_ignore_ascii_case(id))
    }
}

/// Map / screens -> race: start this event (teleports to its grid). Ignored for locked events or during a race.
#[derive(Message, Clone, Debug)]
pub struct StartEvent {
    pub id: String,
}

/// Open the career screen (map button, F6).
#[derive(Message, Clone, Copy, Default, Debug)]
pub struct OpenCareer;

/// Race -> progression: the player finished (or quit) an event.
#[derive(Message, Clone, Debug)]
pub struct RaceFinished {
    pub race: usize,
    /// 1-based; None = did not finish (no rewards).
    pub place: Option<u32>,
    pub time_s: Option<f32>,
    pub field: u32,
}

/// What the last race paid (results panel).
#[derive(Resource, Default, Clone, Debug)]
pub struct LastRewards {
    pub race: Option<usize>,
    pub lines: Vec<String>,
    /// Structured (the post-race screens, race/postrace.rs): place, prize for the place, bonus lines, total credits,
    /// XP gained, replay (half pay), balance after, XP after, wristband before / after.
    pub place: u32,
    pub prize: i64,
    pub bonuses: Vec<(String, i64)>,
    pub credits: i64,
    pub xp: u64,
    pub replay: bool,
    pub balance: i64,
    pub xp_after: u64,
    pub tier_before: usize,
    pub tier_after: usize,
}

/// Career notices for the HUD feed (e4's notifications), sent on the frame they happen.
#[derive(Message, Clone, Debug)]
pub enum CareerNotice {
    RankUp { rank: u32, passed: Option<String> },
    Wristband { tier: u8 },
    Payout { credits: i64, reason: String },
    Fame { amount: u64 },
    PrizeCar { car: String },
}

/// Banners for the text HUD (tier up, rank up, unlocks), oldest first: (text, seconds left); `notices` = the same
/// events as `CareerNotice` messages (flushed every frame).
#[derive(Resource, Default)]
pub struct Banners(pub Vec<(String, f32)>, pub Vec<CareerNotice>);

impl Banners {
    pub fn push(&mut self, s: impl Into<String>) {
        self.0.push((s.into(), 5.0));
    }
}

fn flush_notices(mut banners: ResMut<Banners>, mut out: MessageWriter<CareerNotice>) {
    if !banners.1.is_empty() {
        out.write_batch(std::mem::take(&mut banners.1));
    }
}

pub struct ProgressionPlugin {
    /// `<data>/profile.json`.
    pub path: std::path::PathBuf,
}

impl Plugin for ProgressionPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((crate::race::visuals::RaceVisualsPlugin, crate::race::anark_hud::RaceUiPlugin))
            .insert_resource(Profile::load(self.path.clone(), enabled()))
            .init_resource::<EventCatalog>()
            .init_resource::<LastRewards>()
            .init_resource::<Banners>()
            .init_resource::<screen::CareerUi>()
            .init_resource::<skill::Skills>()
            .add_message::<StartEvent>()
            .add_message::<OpenCareer>()
            .add_message::<RaceFinished>()
            .add_message::<CareerNotice>()
            .add_message::<skill::SkillEvent>()
            .add_systems(PostUpdate, (flush_notices, skill::flush_skill_events))
            .add_systems(Startup, (screen::spawn_career_ui, skill::spawn_skill_hud))
            .add_systems(Update, (apply_results, update_catalog).chain())
            .add_systems(Update, (screen::career_input, screen::draw_career).chain().after(update_catalog))
            .add_systems(Update, (skill::detect_skills.run_if(crate::ui::driving), skill::draw_skill_hud).chain())
            .add_systems(Update, tick_banners);
    }
}

/// The race module's handle on the career (all optional: races work without the plugin).
#[derive(SystemParam)]
pub struct RaceLink<'w> {
    profile: Option<Res<'w, Profile>>,
    start: Option<ResMut<'w, bevy::ecs::message::Messages<StartEvent>>>,
    finished: Option<ResMut<'w, bevy::ecs::message::Messages<RaceFinished>>>,
    ui: Option<ResMut<'w, screen::CareerUi>>,
}

impl RaceLink<'_> {
    /// StartEvent requests since the last frame (race.rs is their only reader).
    pub fn take_starts(&mut self) -> Vec<String> {
        self.start.as_mut().map(|m| m.drain().map(|s| s.id).collect()).unwrap_or_default()
    }

    pub fn finished(&mut self, m: RaceFinished) {
        if let Some(q) = self.finished.as_mut() {
            q.write(m);
        }
    }

    /// The profile (None without the plugin or with `FH1_PROGRESSION=0`: nothing locked).
    pub fn data(&self) -> Option<&profile::ProfileData> {
        self.profile.as_ref().filter(|_| enabled()).map(|p| &p.data)
    }

    pub fn has_screen(&self) -> bool {
        self.ui.is_some()
    }

    pub fn career_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|u| u.open)
    }

    pub fn toggle_career(&mut self) {
        if let Some(u) = self.ui.as_mut() {
            u.open = !u.open;
        }
    }
}

/// Wristband tier of an event (intro = 0).
pub fn event_tier(def: &RaceDef) -> u8 {
    def.level.clamp(0, 6) as u8
}

/// Why `def` is locked for this profile (None = open).
pub fn lock_reason(def: &RaceDef, p: &profile::ProfileData, c: &data::CareerData) -> Option<String> {
    if !enabled() {
        return None;
    }
    let xp = p.xp;
    let tier = c.tier(xp);
    if def.popularity_req > 0 {
        let rank = c.rank(p.fame);
        return (rank > def.popularity_req).then(|| format!("Popularity #{}", def.popularity_req));
    }
    if def.hub > 0 {
        if let Some(h) = c.hubs.iter().find(|h| h.id == def.hub) {
            if xp < h.unlock_xp {
                return Some(format!("{} XP (street hub)", fmt_num(h.unlock_xp as i64)));
            }
            if h.initial.contains(&def.event_id) {
                return None;
            }
        }
    }
    if (tier as i32) < def.level {
        return Some(format!("{} wristband", c.wristbands.get(def.level as usize).map_or("next", |w| w.name.as_str())));
    }
    (xp < def.unlock_xp).then(|| format!("{} XP", fmt_num(def.unlock_xp as i64)))
}

pub fn event_state(def: &RaceDef, p: &profile::ProfileData, c: &data::CareerData) -> EventState {
    if let Some(r) = p.events.get(&def.horizon_id).filter(|r| r.best_place > 0) {
        return EventState::Completed { place: r.best_place };
    }
    if lock_reason(def, p, c).is_some() {
        EventState::Locked
    } else {
        EventState::Unlocked
    }
}

/// Credits for `place` (1-based): CashPrize x EventScoring / 1000; street races pay the top three
/// (CareerRaceModes TopThreeFinish), the Headline only the winner (WinnerTakesAll).
pub fn place_credits(def: &RaceDef, scoring: &[u32], place: u32) -> u32 {
    let kind = EventKind::of(def);
    if place == 0 || (kind == EventKind::Street && place > 3) || (kind == EventKind::Headline && place > 1) {
        return 0;
    }
    let share = scoring.get(place as usize - 1).copied().unwrap_or(0);
    (def.credits as u64 * share as u64 / 1000) as u32
}

pub fn fmt_num(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// The player's car class (CarClasses id) and PI, if the car is an FH1 car with gamedb data.
pub fn player_class(c: &data::CareerData, media: &str) -> Option<(u32, u32)> {
    c.cars.get(media).map(|i| (i.class, i.pi))
}

/// Popularity board: AIPlayers by rank (index 0 = #1). Nemesis drivers at the top (Gold first), then the rest by
/// wristband (high first) and id. INFERRED: FH1 builds its Top 250 at run time; gamedb only lists the drivers.
pub fn rival_board(c: &data::CareerData) -> Vec<u32> {
    let mut d: Vec<&data::Driver> = c.drivers.values().filter(|d| !d.name.is_empty()).collect();
    d.sort_by_key(|d| (!d.nemesis, std::cmp::Reverse(d.tier), d.id));
    d.into_iter().take(249).map(|d| d.id).collect()
}

/// Race results -> XP, credits, records, prize cars, tier ups.
fn apply_results(
    mut finished: MessageReader<RaceFinished>,
    events: Res<Events>,
    mut profile: ResMut<Profile>,
    mut last: ResMut<LastRewards>,
    mut banners: ResMut<Banners>,
) {
    for f in finished.read() {
        let Some(def) = events.races.get(f.race) else { continue };
        let c = &events.career;
        *last = LastRewards { race: Some(f.race), ..default() };
        if !enabled() {
            continue;
        }
        let Some(place) = f.place.filter(|&p| p > 0) else {
            last.lines.push("Did not finish: no rewards".into());
            continue;
        };
        let p = &mut profile.data;
        let tier_before = c.tier(p.xp);
        let rec = p.events.entry(def.horizon_id.clone()).or_default();
        let improved = rec.best_place == 0 || place < rec.best_place as u32;
        let first_win = place == 1 && rec.wins == 0;
        rec.runs += 1;
        if place == 1 {
            rec.wins += 1;
        }
        if improved {
            rec.best_place = place as u8;
        }
        if let Some(t) = f.time_s {
            rec.best_time_s = Some(rec.best_time_s.map_or(t, |b| b.min(t)));
        }
        let share = if improved { 1.0 } else { REPLAY_SHARE };
        let pts = c.wristbands.get(tier_before).map_or(0, |w| w.points.get(place as usize - 1).or(w.points.last()).copied().unwrap_or(0));
        let xp = (pts as f32 * share).round() as u64;
        let mut credits = (place_credits(def, &events.scoring, place) as f32 * share).round() as i64;
        last.prize = credits;
        let mut extra = Vec::new();
        if EventKind::of(def) == EventKind::Nemesis && first_win {
            let bonus = c.wristbands.get(event_tier(def) as usize).map_or(0, |w| w.nemesis_bonus) as i64;
            credits += bonus;
            last.bonuses.push(("NEMESIS BONUS".into(), bonus));
            extra.push(format!("Nemesis beaten: +{} CR", fmt_num(bonus)));
        }
        if let (true, Some(car)) = (first_win, def.prize_car.as_ref()) {
            if !p.cars_won.contains(car) {
                p.cars_won.push(car.clone());
                extra.push(format!("Prize car: {car}"));
                banners.push(format!("PRIZE CAR  {car}"));
                banners.1.push(CareerNotice::PrizeCar { car: car.clone() });
            }
        }
        p.xp += xp;
        p.credits += credits;
        last.place = place;
        last.credits = credits;
        last.xp = xp;
        last.replay = !improved;
        last.balance = p.credits;
        last.xp_after = p.xp;
        last.tier_before = tier_before;
        last.tier_after = c.tier(p.xp);
        last.lines.push(format!("+{} XP{}   +{} CR", xp, if improved { "" } else { " (replay)" }, fmt_num(credits)));
        last.lines.extend(extra);
        let tier = c.tier(p.xp);
        if tier > tier_before && tier as u32 > p.tier_seen {
            p.tier_seen = tier as u32;
            let name = c.wristbands.get(tier).map_or("?", |w| w.name.as_str());
            last.lines.push(format!("{} WRISTBAND!", name.to_uppercase()));
            banners.push(format!("{} WRISTBAND  new events unlocked", name.to_uppercase()));
            banners.1.push(CareerNotice::Wristband { tier: tier as u8 });
            for (_, car) in c.wristband_cars.iter().filter(|(t, _)| *t as usize == tier) {
                if !p.cars_won.contains(car) {
                    p.cars_won.push(car.clone());
                    banners.push(format!("WRISTBAND REWARD  {car}"));
                }
            }
        }
        // Newly opened events.
        let before = profile::ProfileData { xp: p.xp - xp, ..p.clone() };
        let opened: Vec<&str> = events.races.iter().filter(|r| lock_reason(r, &before, c).is_some() && lock_reason(r, p, c).is_none()).map(|r| r.name.as_str()).collect();
        if !opened.is_empty() {
            last.lines.push(format!("Unlocked: {}", opened.join(", ")));
        }
        if let Some(w) = c.wristbands.get(tier + 1) {
            last.lines.push(format!("{} XP to {}", fmt_num((w.xp - p.xp) as i64), w.name));
        }
        info!("progression: {} place {place}: +{xp} XP, +{credits} CR (total {} XP, {} CR)", def.horizon_id, p.xp, p.credits);
        profile.commit();
    }
}

#[derive(SystemParam)]
pub struct CatalogCtx<'w, 's> {
    time: Res<'w, Time>,
    cars: Query<'w, 's, &'static Car>,
    last: Local<'s, (u32, f32, Option<usize>)>,
}

/// Rebuild the catalog on event / profile changes; the recommendation every 2 s (it follows the player).
fn update_catalog(events: Res<Events>, profile: Res<Profile>, mut cat: ResMut<EventCatalog>, mut ctx: CatalogCtx) {
    let now = ctx.time.elapsed_secs();
    let (last_gen, at, rec) = *ctx.last;
    let changed = events.is_changed() || last_gen != profile.generation;
    if !changed && now - at < 2.0 {
        return;
    }
    let car = ctx.cars.iter().next();
    let pos = car.map_or(Vec3::ZERO, |c| c.0.position);
    let class = car.and_then(|c| player_class(&events.career, &c.0.data.media_name));
    let p = &profile.data;
    let c = &events.career;
    let tier = c.tier(p.xp) as i32;
    let mut list: Vec<EventInfo> = events
        .races
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let class_text = r.target_class.and_then(|t| c.class_label(t)).map(|l| match &r.restriction {
                Some(x) => format!("{l} · {x}"),
                None => l,
            });
            EventInfo {
                id: r.horizon_id.clone(),
                name: r.name.clone(),
                kind: EventKind::of(r),
                class: class_text,
                tier: event_tier(r),
                hub: r.hub.min(255) as u8,
                pos: r.marker.0,
                yaw: r.marker.1,
                state: event_state(r, p, c),
                reward: Some(place_credits(r, &events.scoring, 1)).filter(|&x| x > 0),
                laps: Some(r.laps.min(255) as u8),
                length_m: Some(r.length_m * r.laps as f32).filter(|l| *l > 0.0),
                recommended: false,
                lock_reason: lock_reason(r, p, c),
                race: i,
            }
        })
        .collect();
    // Next up: unlocked and not yet completed, closest to the player's wristband, then the class fit, then distance.
    let pick = list
        .iter()
        .filter(|e| e.state == EventState::Unlocked)
        .min_by_key(|e| {
            let r = &events.races[e.race];
            let tier_gap = (tier - r.level.max(0)).unsigned_abs() as f32 * 4000.0;
            let class_gap = match (class, r.target_class) {
                (Some((pc, _)), Some(t)) if pc != t => 2500.0,
                _ => 0.0,
            };
            (tier_gap + class_gap + e.pos.distance(pos)) as u64
        })
        .map(|e| e.race);
    if let Some(i) = pick {
        list[i].recommended = true;
    }
    *ctx.last = (profile.generation, now, pick);
    let differs = changed || pick != rec || cat.events.len() != list.len();
    if differs {
        cat.events = list;
        cat.generation = cat.generation.wrapping_add(1);
    }
}

fn tick_banners(time: Res<Time>, mut banners: ResMut<Banners>) {
    if banners.0.is_empty() {
        return;
    }
    let dt = time.delta_secs();
    if let Some(b) = banners.0.first_mut() {
        b.1 -= dt;
    }
    banners.0.retain(|b| b.1 > 0.0);
}

/// Pay popularity milestones once (FameRankChallenge amounts). Called by skill.rs after a chain is banked.
pub fn pay_rank_milestones(profile: &mut Profile, rank: u32, banners: &mut Banners) {
    let mut paid = 0;
    for (r, cr) in RANK_PAYOUTS {
        if rank <= r && profile.data.rank_paid > r {
            paid += cr;
            banners.push(format!("POPULARITY #{r}  +{} CR", fmt_num(cr)));
            banners.1.push(CareerNotice::Payout { credits: cr, reason: format!("Popularity #{r}") });
        }
    }
    if paid > 0 {
        profile.data.credits += paid;
    }
    profile.data.rank_paid = profile.data.rank_paid.min(rank);
}
