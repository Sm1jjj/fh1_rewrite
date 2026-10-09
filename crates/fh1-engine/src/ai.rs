//! AI drivers (docs/AI.md): the game's racing lines (`line`), a speed profile from the car's grip (`profile`), the driver
//! that turns line + profile + traffic into [`Controls`](crate::vehicle::Controls) (`driver`), and the gamedb AI tables
//! (`tables`). Everything here is headless (no Bevy systems), so tests can lap a route; the game's plugin that spawns and
//! steps AI cars lives in the engine binary (src/ai/plugin.rs).
//!
//! Race API (agreed with the race session, race/*): the race writes [`SpawnRaceAi`] / [`DespawnRaceAi`] and the
//! [`AiRaceControl`] resource; AI cars carry [`AiCar`] (their simulation) and [`AiRacer`] (their grid slot). The player's
//! car is `Car` in the binary, marked [`PlayerCar`].

pub mod assist;
pub mod driver;
pub mod line;
pub mod profile;
pub mod start;
pub mod tables;

use bevy::prelude::*;

use crate::vehicle::Vehicle;

/// Marks the player's car entity (the one with the binary's `Car` component).
#[derive(Component, Default)]
pub struct PlayerCar;

/// An AI car's simulation. AI cars don't carry the player's `Car` component: player-only systems (camera, HUD, audio...)
/// keep seeing one car.
#[derive(Component)]
pub struct AiCar(pub Vehicle);

/// An AI car spawned for a race: its grid slot (1..).
#[derive(Component, Clone, Copy, Debug)]
pub struct AiRacer {
    pub slot: u32,
}

/// A wheel node of an AI car's glTF (LF, RF, LR, RR), driven by the AI plugin rather than the player's wheel sync.
#[derive(Component, Clone, Copy, Debug)]
pub struct AiWheel {
    pub car: Entity,
    pub index: usize,
    pub hub: Vec3,
    pub scale: Vec3,
}

/// Race -> AI: spawn one opponent at a grid pose (engine space). Build it with `..default()` so new fields don't break.
#[derive(Message, Clone, Debug, Default)]
pub struct SpawnRaceAi {
    /// Grid index, 1.. (0 = the player).
    pub slot: u32,
    /// Data_Car MediaName, e.g. "VW_Corrado_95" (a folder under cars/).
    pub car_id: String,
    pub pose: (Vec3, f32),
    /// AISkills id (Events.AISkill<Difficulty>).
    pub skill: u32,
    /// AITemperaments id (Events.AITemperament<Difficulty>), 0 = default (5).
    pub temperament: u32,
    /// AIRubberbands id (Events.AIRubberband<Difficulty>), 0 = the skill's Rubberbands_id.
    pub rubberband: u32,
    /// AIPlayers.Id (its Skill/Aggro/RubberBand modifiers), 0 = none.
    pub driver_id: u32,
    /// The event's route, "Ribbon_00/TrackRouteNNN.xml": the AI drives aiopenworld route_NNN.
    pub route_file: String,
    /// Laps wrap (the line is a closed loop).
    pub circuit: bool,
    /// Combo_Colors Sequence (EventParticipants colour), 0 = picked from the slot.
    pub paint: u32,
    /// Tune this car to the event's class band (src/ai/upgrade.rs; None = the stock car).
    pub tune: Option<AiTuneReq>,
}

/// What the race asks of an opponent's car (raw PI 0..1; built by race/field.rs from the event's TargetClass, the
/// EventParticipants.TuningLevel and the slot): the class band the field races in and the PI to tune towards.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AiTuneReq {
    /// Class floor (MaxPerformanceIndex of the class below) and cap (of the class itself).
    pub lo: f32,
    pub hi: f32,
    /// The PI to tune to (lo..=hi).
    pub target: f32,
    /// EventParticipants.TuningLevel (1 = tuned entrant: tuned whenever below `target`).
    pub tuning_level: u32,
}

/// Race -> AI: despawn every race AI car.
#[derive(Message, Clone, Copy, Debug, Default)]
pub struct DespawnRaceAi;

/// Race -> AI, every frame while an event runs.
#[derive(Resource, Clone, Debug)]
pub struct AiRaceControl {
    /// Hold the brakes (grid, countdown): true unless the race is running.
    pub hold: bool,
    /// Slots that crossed the finish: they slow to a cruise.
    pub finished: Vec<u32>,
    /// The active event's route ("Ribbon_00/TrackRouteNNN.xml"), for the player's driving line and assisted
    /// braking / steering; None in free roam.
    pub route_file: Option<String>,
}

impl Default for AiRaceControl {
    fn default() -> Self {
        Self { hold: false, finished: Vec::new(), route_file: None }
    }
}

/// FH1's "Driving line" assist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DrivingLine {
    #[default]
    Full,
    BrakingOnly,
    Off,
}

impl DrivingLine {
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::BrakingOnly => "Braking only",
            Self::Off => "Off",
        }
    }

    pub fn next(self, back: bool) -> Self {
        const ALL: [DrivingLine; 3] = [DrivingLine::Full, DrivingLine::BrakingOnly, DrivingLine::Off];
        let i = ALL.iter().position(|&x| x == self).unwrap_or(0);
        ALL[(i + if back { 2 } else { 1 }) % 3]
    }
}

/// FH1's AI difficulty: which of the Events AISkill / AITemperament / AIRubberband columns (Easy, Med, Hard, Pro) a
/// race's opponents use. The game's default is Medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum AiDifficulty {
    Easy,
    #[default]
    Medium,
    Hard,
    Pro,
}

impl AiDifficulty {
    pub fn name(self) -> &'static str {
        match self {
            Self::Easy => "Easy",
            Self::Medium => "Medium",
            Self::Hard => "Hard",
            Self::Pro => "Pro",
        }
    }

    /// Column index in the Events AI arrays (0 = Easy .. 3 = Pro).
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn next(self, back: bool) -> Self {
        const ALL: [AiDifficulty; 4] = [AiDifficulty::Easy, AiDifficulty::Medium, AiDifficulty::Hard, AiDifficulty::Pro];
        ALL[(self.index() + if back { 3 } else { 1 }) % 4]
    }
}

/// Route number of a route file name ("Ribbon_00/TrackRoute012.xml" -> 12; also accepts "route_012", "12").
pub fn route_id(route_file: &str) -> Option<u32> {
    let name = route_file.rsplit(['/', '\\']).next()?;
    let stem = name.split('.').next()?;
    let digits = stem.trim_start_matches(|c: char| !c.is_ascii_digit());
    digits.parse().ok()
}

/// Installed racing line file of a route (`<assets>/ailines/<track>/route_NNN.owt`).
pub fn line_path(assets: &std::path::Path, track: &str, route: u32) -> std::path::PathBuf {
    assets.join("ailines").join(track).join(format!("route_{route:03}.owt"))
}

#[cfg(test)]
mod tests;
