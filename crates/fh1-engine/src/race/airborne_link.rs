//! Career glue for the seven airborne showcase races (gamedb events 101, 168, 172, 220, 222, 223, 246 = PLANE_RACE_001..007,
//! CareerTypeId 8, race mode 9). `FH1_AIRBORNE_CAREER=0` keeps them out of the career.
//!
//! Where they already are (VERIFIED in `events.json` of the current install + the code): the `events` setup group installs
//! them like every other race (grid, 4..11 gates, marker kind "festival", `career_type` 8, popularity rank gate
//! `PopularityPointsReq` 250 / 175 / 100 / 1 / 75 / 225 / 150, prize car from Rewards_EventPrizes, CashPrize 5,000..25,000).
//! So they are in `Events::races` -> the `EventCatalog` (both maps, career screen "Showcases" page), get the world marker and
//! the A-to-start prompt, and `RaceFinished` (sent by race.rs when the player crosses the line, place counted against the
//! aircraft's virtual racer from race/airborne.rs) feeds `progression::apply_results`: wristband XP by place
//! (WristbandScoring), credits = CashPrize x EventScoring(place) / 1000, the prize car on the first win, and the record
//! (`ProfileData::events[horizon_id]` best place / best time) the Done state reads. Nothing separate is needed for them.
//!
//! What this module adds:
//! - [`outside_career`]: the `FH1_AIRBORNE_CAREER=0` switch (apply_results skips them: no XP, credits, record or prize car).
//! - [`result_line`] / the [`AirborneLinkPlugin`] system: a results line, "You beat the Biplane" / "The Biplane beat you"
//!   (the race is player vs one aircraft; INFERRED that FH1 words it as a win/loss, the data has no strings for it).
//! - [`airborne_races`]: the seven races as (index, def) for any list that wants them separately.

use bevy::prelude::*;

use super::{airborne, Events, RaceDef, RaceState};
use crate::progression::{LastRewards, RaceFinished};

/// `FH1_AIRBORNE_CAREER=0`: the airborne races do not count toward the career (no rewards, no recorded result).
pub fn career_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_AIRBORNE_CAREER").map_or(true, |v| v != "0"))
}

/// An airborne showcase (CareerTypeId 8 = "Showcase Airborne Challenge Race", RaceModeId 9; or a PLANE_RACE_nnn id).
pub fn is_airborne(def: &RaceDef) -> bool {
    def.career_type == 8 || def.mode == 9 || def.horizon_id.starts_with("PLANE_RACE")
}

/// True when `def` is an airborne race and `FH1_AIRBORNE_CAREER=0`: progression skips its results.
pub fn outside_career(def: &RaceDef) -> bool {
    !career_on() && is_airborne(def)
}

/// The airborne races (race index, def), in installed order.
pub fn airborne_races(events: &Events) -> impl Iterator<Item = (usize, &RaceDef)> {
    events.races.iter().enumerate().filter(|(_, r)| is_airborne(r))
}

/// The results line for a finished airborne race: `place` 1 = the player beat the aircraft.
pub fn result_line(aircraft: &str, place: u32) -> String {
    if place <= 1 {
        format!("You beat the {aircraft}")
    } else {
        format!("The {aircraft} beat you")
    }
}

/// The aircraft's display name in the running race ("Biplane", from the virtual racer's car label), if any.
fn aircraft_name(rs: &RaceState) -> Option<String> {
    rs.racers.iter().find(|r| airborne::is_virtual(r)).map(|r| r.car.clone())
}

pub struct AirborneLinkPlugin;

impl Plugin for AirborneLinkPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, result_lines);
    }
}

/// Adds [`result_line`] to the post-race rewards. `apply_results` resets `LastRewards` when it handles the message (in the
/// same frame or the next, depending on system order), so the line waits in `pending` until `LastRewards.race` is this race.
fn result_lines(
    mut finished: MessageReader<RaceFinished>,
    events: Res<Events>,
    rs: Res<RaceState>,
    last: Option<ResMut<LastRewards>>,
    mut pending: Local<Option<(usize, String)>>,
) {
    if !career_on() {
        return;
    }
    for f in finished.read() {
        let (Some(def), Some(place)) = (events.races.get(f.race), f.place) else { continue };
        if is_airborne(def) {
            if let Some(name) = aircraft_name(&rs) {
                *pending = Some((f.race, result_line(&name, place)));
            }
        }
    }
    if let (Some((race, line)), Some(mut last)) = ((*pending).clone(), last) {
        if last.race == Some(race) {
            if !last.lines.contains(&line) {
                last.lines.insert(0, line);
            }
            *pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: &str, career_type: u32, mode: u32) -> RaceDef {
        RaceDef {
            horizon_id: id.into(),
            name: String::new(),
            kind: String::new(),
            mode,
            laps: 1,
            circuit: false,
            credits: 0,
            drivers: 0,
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
            field: Vec::new(),
            ai: [(0, 0, 0); 4],
            event_id: 0,
            career_type,
            level: 0,
            hub: 0,
            unlock_xp: 0,
            popularity_req: 0,
            event_order: 0,
            target_class: None,
            player_car: None,
            restriction: None,
            prize_car: None,
            recommended: Vec::new(),
            cannons: [Vec::new(), Vec::new()],
            start_cannon: None,
        }
    }

    #[test]
    fn detects_the_seven() {
        assert!(is_airborne(&def("PLANE_RACE_004", 8, 9)));
        assert!(is_airborne(&def("PLANE_RACE_004", 0, 0)));
        // Single-make showcases (career type 7, mode 8) are not airborne.
        assert!(!is_airborne(&def("SHOWCASE_X", 7, 8)));
        assert!(!is_airborne(&def("FR02", 2, 3)));
    }

    #[test]
    fn career_switch_default_on() {
        // Without FH1_AIRBORNE_CAREER=0 in the test environment, nothing is outside the career.
        assert!(!outside_career(&def("PLANE_RACE_001", 8, 9)));
    }

    #[test]
    fn lines() {
        assert_eq!(result_line("Biplane", 1), "You beat the Biplane");
        assert_eq!(result_line("Biplane", 2), "The Biplane beat you");
    }
}
