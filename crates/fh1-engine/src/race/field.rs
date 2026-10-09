//! The AI field for one race (docs/PROGRESSION.md "AI that fits the stage"). Base = the game's own field
//! (EventParticipants: car, AIPlayer, paint) with the Events AISkill column picked by Options "AI difficulty". On top:
//! - class match (`FH1_AI_FIELD=0` off): when the player's car is in another class than the event's TargetClass, every
//!   opponent gets an installed, selectable car of the player's class with the closest PI (same drive type preferred);
//!   FH1 would refuse the car instead;
//! - wristband scaling (`FH1_AI_TIER_SCALE=0` off): replaying an event below the player's wristband steps the AISkills id
//!   2 rows harder per tier above (ids 1..80 are one ladder, 1 = fastest; 201+ are special and kept);
//! - named rivals (`FH1_AI_RIVALS=0` off): the two grid slots nearest the player go to the drivers ranked just above
//!   the player on the popularity board (nemesis drivers stay in their own races);
//! - the game's difficulty tables (P17, `FH1_AI_GAME_DIFF=0` = the P9 caps 80/20/8/1 and the event's temperament):
//!   Easy = the Events column as is; Medium / Hard / Pro cap the skill id at the DynamicDifficulty upper bound of the tier
//!   (16 / 4 / 1; GameTunableSettings.ini, VERIFIED values, tier mapping INFERRED). When the cap lowers the skill, the
//!   temperament follows the game's `SkillToTemperamentRatio` 4 (VERIFIED value): min(event temperament, ceil(skill / 4))
//!   (lower id = more aggressive, AITemperaments AggressionLevel 1.0 -> 0.0; INFERRED rule);
//! - upgraded opponents (P17, `FH1_AI_UPGRADE=0` off): each entry carries an [`AiTuneReq`] (the class band of the race and
//!   a target PI); src/ai/upgrade.rs tunes the car at spawn. Events.UpgradeAI = 0 events and entries without a class band
//!   (open events) stay stock.

use std::collections::HashMap;
use std::path::Path;

use bevy::prelude::*;
use fh1_engine::ai::AiTuneReq;

use super::RaceDef;
use crate::progression::data::CareerData;

#[derive(Clone, Debug)]
pub struct FieldEntry {
    pub car: String,
    pub driver: u32,
    /// Driver name (AIPlayers), else the car label.
    pub name: String,
    /// "Volkswagen Corrado" (gamedb make + the MediaName's model token).
    pub car_label: String,
    pub color_seq: u32,
    pub skill: u32,
    pub temperament: u32,
    pub rubberband: u32,
    /// Tune the car to the race's class band (None = stock).
    pub tune: Option<AiTuneReq>,
}

fn flag(name: &str) -> bool {
    std::env::var(name).map_or(true, |v| v != "0")
}

/// Small deterministic hash (stable picks per event / slot).
fn mix(a: u32, b: u32) -> u32 {
    let mut x = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^ (x >> 12)
}

pub struct FieldCtx<'a> {
    pub career: &'a CareerData,
    pub player_car: &'a str,
    /// Player's wristband tier.
    pub tier: usize,
    /// Player's popularity rank (250 = unranked).
    pub rank: u32,
    /// Options "AI difficulty" column (0 Easy .. 3 Pro).
    pub difficulty: usize,
    /// The installed private assets (`ailines/ai_tables.json` for Events.UpgradeAI / EventParticipants.TuningLevel);
    /// None = no tuning data (every event upgrades, every entry TuningLevel 0).
    pub assets: Option<&'a Path>,
}

/// gamedb CarClasses.MaxPerformanceIndex by class id (VERIFIED, data/extracted/story/ai_tables/CarClasses.csv; R1's
/// 0.999999999999 written as 1). Class n holds PI in (MaxPI[n-1], MaxPI[n]] (pi.rs `class_index`).
const CLASS_MAX_PI: [f32; 11] = [0.005, 0.459, 0.539, 0.5975, 0.6505, 0.739, 0.81, 0.8665, 0.8991, 1.0, 1.0];

/// Raw-PI band (floor, cap) of class `class` (C..R1 are what events use); None for F, E and U.
pub(crate) fn class_band(class: u32) -> Option<(f32, f32)> {
    (1..=9).contains(&class).then(|| (CLASS_MAX_PI[class as usize - 1], CLASS_MAX_PI[class as usize]))
}

/// The PI an opponent in slot `slot` is tuned to inside the band [lo, hi]. TuningLevel 1: just under the cap. Level 0 on an
/// UpgradeAI event: a per-slot deterministic point in the upper half of the band, at most 0.002 under the cap. INFERRED:
/// the game's own target PI rule is not decoded; the margins cover pi.rs's mean error 0.0018.
pub(crate) fn tune_target(lo: f32, hi: f32, tuning_level: u32, event: u32, slot: u32) -> f32 {
    let t = if tuning_level >= 1 {
        hi - 0.004
    } else {
        let u = (mix(event, slot) % 1000) as f32 / 1000.0;
        (lo + (hi - lo) * (0.5 + 0.5 * u)).min(hi - 0.002)
    };
    t.clamp(lo, hi)
}

/// Skill id cap per Options difficulty (0 Easy .. 3 Pro). `game` = the DynamicDifficulty upper bounds Medium 16, Hard 4,
/// Pro 1 (VERIFIED values; Easy's 20 is not applied: Easy keeps the column, INFERRED), else the P9 caps 80/20/8/1.
pub(crate) fn skill_cap(difficulty: usize, game: bool) -> u32 {
    let caps = if game { [80, 16, 4, 1] } else { [80, 20, 8, 1] };
    caps[difficulty.min(3)]
}

/// The temperament id after the cap lowered the skill to `skill`: SkillToTemperamentRatio 4 (VERIFIED value), never calmer
/// than the event's (0 = the default 5): min(event, ceil(skill / 4)) in 1..=10 (INFERRED rule).
pub(crate) fn game_temperament(event: u32, skill: u32) -> u32 {
    let base = if event == 0 { 5 } else { event };
    base.min(skill.div_ceil(4).clamp(1, 10))
}

/// Events.UpgradeAI and EventParticipants.TuningLevel from `ailines/ai_tables.json` (fh1setup ailines-2).
#[derive(Debug, Default)]
pub(crate) struct TuneTables {
    /// Events.Id -> UpgradeAI.
    upgrade_ai: HashMap<u32, bool>,
    /// (EventParticipants.EventID, AIPlayerID) -> TuningLevel (no event has the same driver twice, VERIFIED).
    tuning: HashMap<(u32, u32), u32>,
}

impl TuneTables {
    pub(crate) fn from_json(v: &serde_json::Value) -> Self {
        let u = |x: &serde_json::Value| x.as_u64().unwrap_or(0) as u32;
        let rows = |t: &str| v[t].as_array().map(Vec::as_slice).unwrap_or_default();
        Self {
            upgrade_ai: rows("Events").iter().map(|r| (u(&r["Id"]), r["UpgradeAI"].as_i64().unwrap_or(1) != 0)).collect(),
            tuning: rows("EventParticipants").iter().map(|r| ((u(&r["EventID"]), u(&r["AIPlayerID"])), u(&r["TuningLevel"]))).collect(),
        }
    }

    /// Events.UpgradeAI (VERIFIED 1 on 116 of 119 events); unknown events upgrade.
    pub(crate) fn upgrade_ai(&self, event: u32) -> bool {
        self.upgrade_ai.get(&event).copied().unwrap_or(true)
    }

    /// EventParticipants.TuningLevel of the driver's entry (VERIFIED 1 on 103 of 742); unknown = 0.
    pub(crate) fn tuning(&self, event: u32, driver: u32) -> u32 {
        self.tuning.get(&(event, driver)).copied().unwrap_or(0)
    }
}

/// The tables, read once. Without `assets`, or on an ailines-1 install (no Events / EventParticipants rows) the fallback:
/// UpgradeAI = 1, TuningLevel = 0 (logged once).
pub(crate) fn tune_tables(assets: Option<&Path>) -> &'static TuneTables {
    static T: std::sync::OnceLock<TuneTables> = std::sync::OnceLock::new();
    static NONE: std::sync::OnceLock<TuneTables> = std::sync::OnceLock::new();
    let Some(a) = assets else { return NONE.get_or_init(TuneTables::default) };
    T.get_or_init(|| {
        let path = a.join("ailines/ai_tables.json");
        let v: Option<serde_json::Value> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        match v.filter(|v| v["Events"].is_array() && v["EventParticipants"].is_array()) {
            Some(v) => TuneTables::from_json(&v),
            None => {
                info!("AI upgrade: {} has no Events / EventParticipants rows (re-run fh1setup --only ailines); UpgradeAI = 1, TuningLevel = 0", path.display());
                TuneTables::default()
            }
        }
    })
}

/// The player's class / PI (current build) and drive type for the class match.
pub(crate) struct PlayerPi {
    class: u32,
    pi: u32,
    drive: u32,
}

/// Up to `count` opponents, grid slot order; plus a note for the HUD when the field was changed.
pub fn build(def: &RaceDef, ctx: &FieldCtx, count: usize) -> (Vec<FieldEntry>, Option<String>) {
    let c = ctx.career;
    let (skill0, temperament0, rubberband) = def.ai[ctx.difficulty.min(3)];
    let (mut skill, mut temperament) = (skill0, temperament0);
    // P9 pace (user: AI "pose no threat"): the Events columns give early events AISkills 64-74 (cornering 0.5-0.6 of the
    // car's grip), FH1's own gentle start. Difficulty now also caps the id (1 = fastest): Easy = the game's column as is,
    // Medium <= 16, Hard <= 4, Pro <= 1 (P17: the game's DynamicDifficulty bounds; `FH1_AI_GAME_DIFF=0` = P9's 20 / 8 / 1;
    // Corrado route 005 laps, docs/AI.md Tests: skill 1 65.3 s, 10 66.6, 30 71.2, 50 77.6, 70 83.6). A capped skill pulls the
    // temperament to the game's skill:temperament ratio (`game_temperament`). FH1_AI_SKILL_CAP=n overrides the cap,
    // FH1_AI_PACE=0 = old (columns only). Ids 201+ untouched.
    if flag("FH1_AI_PACE") && (1..=80).contains(&skill) {
        let game = flag("FH1_AI_GAME_DIFF");
        let cap = std::env::var("FH1_AI_SKILL_CAP").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(skill_cap(ctx.difficulty, game)).max(1);
        if skill > cap {
            skill = cap;
            if game {
                temperament = game_temperament(temperament, skill);
            }
        }
    }
    if flag("FH1_AI_TIER_SCALE") && crate::progression::enabled() && (1..=80).contains(&skill) {
        let above = (ctx.tier as i32 - def.level.max(0)).max(0) as u32;
        skill = skill.saturating_sub(2 * above).max(1);
    }
    let mut out: Vec<FieldEntry> = def
        .field
        .iter()
        .filter_map(|e| {
            Some(FieldEntry {
                car: e.car.clone()?,
                driver: e.driver,
                name: String::new(),
                car_label: String::new(),
                color_seq: e.color_seq,
                skill,
                temperament,
                rubberband,
                tune: None,
            })
        })
        .take(count)
        .collect();
    let mut note = None;
    // TuningLevel of each original entry (before class match / rivals swap cars and drivers).
    let tables = tune_tables(ctx.assets);
    let levels: Vec<u32> = out.iter().map(|e| tables.tuning(def.event_id, e.driver)).collect();
    // The class the field races in: the event's TargetClass, or the player's class when the cars were replaced.
    let mut band_class = def.target_class;

    // Class match against the player's CURRENT build (P10: upgrades change the class / PI, as the game rewrites the
    // garage car's ClassID / PI on upgrade, 82546650); drive type from the car's data.
    let player = c.cars.get(ctx.player_car).map(|i| {
        let (class, pi) = crate::progression::player_class(c, ctx.player_car).unwrap_or((i.class, i.pi));
        super::field::PlayerPi { class, pi, drive: i.drive }
    });
    if let (true, Some(pi), Some(target)) = (flag("FH1_AI_FIELD"), player, def.target_class) {
        if pi.class != target && !out.is_empty() {
            let mut pool: Vec<(&String, u32)> = c
                .cars
                .iter()
                .filter(|(_, i)| i.installed && i.selectable && i.class == pi.class)
                .map(|(m, i)| (m, i.pi.abs_diff(pi.pi) * 4 + if i.drive == pi.drive { 0 } else { 20 }))
                .collect();
            pool.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(b.0)));
            pool.truncate((out.len() * 2).max(8));
            if !pool.is_empty() {
                let mut used: Vec<usize> = Vec::new();
                for (slot, e) in out.iter_mut().enumerate() {
                    let mut k = mix(def.event_id, slot as u32) as usize % pool.len();
                    for _ in 0..pool.len() {
                        if !used.contains(&k) {
                            break;
                        }
                        k = (k + 1) % pool.len();
                    }
                    used.push(k);
                    e.car = pool[k].0.clone();
                    e.color_seq = 0;
                }
                band_class = Some(pi.class);
                note = Some(format!("Field matched to your class {} {}", c.class_name(pi.class), pi.pi));
            }
        }
    }

    // Named rivals: the drivers ranked just above the player take the slots nearest the player (last on the grid).
    if flag("FH1_AI_RIVALS") && crate::progression::enabled() && !matches!(crate::progression::EventKind::of(def), crate::progression::EventKind::Nemesis) {
        let board = crate::progression::rival_board(c);
        let in_field: Vec<u32> = out.iter().map(|e| e.driver).collect();
        let mut above = (1..ctx.rank.min(250)).rev().filter_map(|r| board.get(r as usize - 1).copied()).filter(|id| !in_field.contains(id));
        let n = out.len();
        for e in out.iter_mut().skip(n.saturating_sub(2)) {
            if c.drivers.get(&e.driver).is_some_and(|d| d.nemesis) {
                continue;
            }
            if let Some(id) = above.next() {
                e.driver = id;
            }
        }
    }

    // Upgraded opponents (P17): the tune request per slot; src/ai/upgrade.rs plans the parts at spawn.
    if let (true, Some((lo, hi)), true) = (flag("FH1_AI_UPGRADE"), band_class.and_then(class_band), tables.upgrade_ai(def.event_id)) {
        for (slot, e) in out.iter_mut().enumerate() {
            let level = levels.get(slot).copied().unwrap_or(0);
            e.tune = Some(AiTuneReq { lo, hi, target: tune_target(lo, hi, level, def.event_id, slot as u32), tuning_level: level });
        }
    }

    for e in &mut out {
        e.car_label = car_label(c, &e.car);
        e.name = c.driver_name(e.driver).map(str::to_owned).unwrap_or_else(|| e.car_label.clone());
    }
    if let Some(n) = &note {
        info!("race: {n}");
    }
    (out, note)
}

/// Readable car name: gamedb make + the MediaName's model part ("VW_Corrado_95" -> "Volkswagen Corrado").
pub fn car_label(c: &CareerData, media: &str) -> String {
    let parts: Vec<&str> = media.split('_').collect();
    let model = if parts.len() >= 3 { parts[1..parts.len() - 1].join(" ") } else { parts.get(1).copied().unwrap_or(media).to_owned() };
    let info = c.cars.get(media);
    let make = info.map(|i| i.make.as_str()).filter(|m| !m.is_empty() && !m.starts_with("_&"));
    match (make, info.map(|i| i.name.as_str()).filter(|n| !n.is_empty())) {
        // gamedb MakeName + DisplayName ("Chevrolet Camaro SS Coupe"), events-4.
        (Some(make), Some(name)) => format!("{make} {name}"),
        (Some(make), None) => format!("{make} {model}"),
        _ => media.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::race::Entrant;

    fn def(event: u32, target_class: Option<u32>) -> RaceDef {
        RaceDef {
            horizon_id: String::new(),
            name: String::new(),
            kind: "Race".into(),
            mode: 2,
            laps: 1,
            circuit: false,
            credits: 0,
            drivers: 2,
            track_id: 0,
            route_file: String::new(),
            length_m: 0.0,
            marker: (Vec3::ZERO, 0.0),
            grid: Vec::new(),
            gates: Vec::new(),
            start_gate: 0,
            path: Vec::new(),
            post_race: None,
            barrier_bits: 0,
            objects: Vec::new(),
            field: vec![Entrant { car: Some("A".into()), driver: 10, color_seq: 0 }, Entrant { car: Some("B".into()), driver: 20, color_seq: 0 }],
            // FR02 (Events 26): AISkill 79/71/44/36, AITemperament 8/6/4/2.
            ai: [(79, 8, 0), (71, 6, 0), (44, 4, 0), (36, 2, 0)],
            event_id: event,
            career_type: 2,
            level: 0,
            hub: 0,
            unlock_xp: 0,
            popularity_req: 0,
            event_order: 0,
            target_class,
            player_car: None,
            restriction: None,
            prize_car: None,
            recommended: Vec::new(),
            cannons: [Vec::new(), Vec::new()],
            start_cannon: None,
        }
    }

    /// Default flags: skill = min(column, tier bound), temperament = min(event, ceil(skill / 4)) once the cap bites.
    #[test]
    fn game_difficulty_tables() {
        if !flag("FH1_AI_PACE") || !flag("FH1_AI_GAME_DIFF") || !flag("FH1_AI_TIER_SCALE") || std::env::var("FH1_AI_SKILL_CAP").is_ok() {
            eprintln!("game_difficulty_tables: flags not at defaults, skipped");
            return;
        }
        let career = CareerData::default();
        let d = def(26, Some(4));
        // (Easy, Medium, Hard, Pro) -> (skill, temperament)
        let want = [(79, 8), (16, 4), (4, 1), (1, 1)];
        for (difficulty, &(skill, temperament)) in want.iter().enumerate() {
            let ctx = FieldCtx { career: &career, player_car: "", tier: 0, rank: 250, difficulty, assets: None };
            let (out, _) = build(&d, &ctx, 2);
            assert_eq!(out.len(), 2);
            for e in &out {
                assert_eq!((e.skill, e.temperament), (skill, temperament), "difficulty {difficulty}");
            }
        }
        // A column already under the bound keeps its temperament.
        let mut low = def(26, Some(4));
        low.ai[1] = (10, 6, 0);
        let ctx = FieldCtx { career: &career, player_car: "", tier: 0, rank: 250, difficulty: 1, assets: None };
        assert_eq!(build(&low, &ctx, 2).0[0].skill, 10);
        assert_eq!(build(&low, &ctx, 2).0[0].temperament, 6);
        assert_eq!(skill_cap(1, false), 20);
        assert_eq!(skill_cap(2, false), 8);
        assert_eq!(game_temperament(0, 80), 5);
        assert_eq!(game_temperament(8, 2), 1);
    }

    #[test]
    fn tune_band_and_target() {
        assert_eq!(class_band(4), Some((0.5975, 0.6505)));
        assert_eq!(class_band(9), Some((0.8991, 1.0)));
        assert_eq!(class_band(0), None);
        assert_eq!(class_band(10), None);
        for class in 1..=9 {
            let (lo, hi) = class_band(class).unwrap();
            for slot in 0..16 {
                let t0 = tune_target(lo, hi, 0, 26, slot);
                assert!(t0 >= lo + 0.5 * (hi - lo) - 1e-6 && t0 <= hi - 0.002 + 1e-6, "class {class} slot {slot}: {t0}");
                assert_eq!(t0, tune_target(lo, hi, 0, 26, slot), "deterministic");
            }
            let t1 = tune_target(lo, hi, 1, 26, 0);
            assert!((t1 - (hi - 0.004)).abs() < 1e-6 && t1 > lo);
        }
        let v = serde_json::json!({
            "Events": [{"Id": 26, "UpgradeAI": 1}, {"Id": 59, "UpgradeAI": 0}],
            "EventParticipants": [{"EventID": 26, "AIPlayerID": 10, "TuningLevel": 1}, {"EventID": 26, "AIPlayerID": 20, "TuningLevel": 0}]
        });
        let t = TuneTables::from_json(&v);
        assert!(t.upgrade_ai(26) && !t.upgrade_ai(59) && t.upgrade_ai(999));
        assert_eq!((t.tuning(26, 10), t.tuning(26, 20), t.tuning(27, 10)), (1, 0, 0));
        // No data: every entry gets a request in the event's class, stock-level 0.
        if flag("FH1_AI_UPGRADE") {
            let career = CareerData::default();
            let ctx = FieldCtx { career: &career, player_car: "", tier: 0, rank: 250, difficulty: 1, assets: None };
            let (out, _) = build(&def(26, Some(4)), &ctx, 2);
            let r = out[0].tune.expect("tune request");
            assert_eq!((r.lo, r.hi, r.tuning_level), (0.5975, 0.6505, 0));
            assert!(r.target >= r.lo && r.target <= r.hi);
            assert!(build(&def(26, None), &ctx, 2).0[0].tune.is_none(), "open event: stock");
        }
    }
}
