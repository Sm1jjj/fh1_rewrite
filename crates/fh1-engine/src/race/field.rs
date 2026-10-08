//! The AI field for one race (docs/PROGRESSION.md "AI that fits the stage"). Base = the game's own field
//! (EventParticipants: car, AIPlayer, paint) with the Events AISkill column picked by Options "AI difficulty". On top:
//! - class match (`FH1_AI_FIELD=0` off): when the player's car is in another class than the event's TargetClass, every
//!   opponent gets an installed, selectable car of the player's class with the closest PI (same drive type preferred);
//!   FH1 would refuse the car instead;
//! - wristband scaling (`FH1_AI_TIER_SCALE=0` off): replaying an event below the player's wristband steps the AISkills id
//!   2 rows harder per tier above (ids 1..80 are one ladder, 1 = fastest; 201+ are special and kept);
//! - named rivals (`FH1_AI_RIVALS=0` off): the two grid slots nearest the player go to the drivers ranked just above
//!   the player on the popularity board (nemesis drivers stay in their own races).

use bevy::prelude::*;

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
}

/// Up to `count` opponents, grid slot order; plus a note for the HUD when the field was changed.
pub fn build(def: &RaceDef, ctx: &FieldCtx, count: usize) -> (Vec<FieldEntry>, Option<String>) {
    let c = ctx.career;
    let (skill0, temperament, rubberband) = def.ai[ctx.difficulty.min(3)];
    let mut skill = skill0;
    // P9 pace (user: AI "pose no threat"): the Events columns give early events AISkills 64-74 (cornering 0.5-0.6 of the
    // car's grip), FH1's own gentle start. Difficulty now also caps the id (1 = fastest): Easy = the game's column as is,
    // Medium <= 20, Hard <= 8, Pro <= 1 (Corrado route 005 laps, docs/AI.md Tests: skill 1 65.3 s, 10 66.6, 30 71.2,
    // 50 77.6, 70 83.6). FH1_AI_SKILL_CAP=n overrides the cap, FH1_AI_PACE=0 = old (columns only). Ids 201+ untouched.
    if flag("FH1_AI_PACE") && (1..=80).contains(&skill) {
        let cap = std::env::var("FH1_AI_SKILL_CAP").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(match ctx.difficulty {
            0 => 80,
            1 => 20,
            2 => 8,
            _ => 1,
        });
        skill = skill.min(cap.max(1));
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
            })
        })
        .take(count)
        .collect();
    let mut note = None;

    // Class match.
    let player = c.cars.get(ctx.player_car);
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
