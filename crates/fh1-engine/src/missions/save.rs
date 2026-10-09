//! What missions keep in the career save (profile.json `missions`, progression/profile.rs). Every field defaults, so
//! older saves load.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One outpost mission (speed stunt / PR stunt / photo shoot) or encounter record.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MissionRecord {
    pub completed: bool,
    /// Stars 0..3 (difficulty targets met: easy / medium / hard).
    pub stars: u8,
    /// Best score: speed (mph) for speed stunts, skill points for PR stunts, landmarks in shot for photo shoots.
    pub best: f32,
    pub runs: u32,
}

/// Barn-find progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BarnState {
    /// Not rumoured yet.
    #[default]
    Hidden,
    /// Rumour: the hint circle is on the map.
    Rumoured,
    /// Found; Dak is restoring it.
    Restoring,
    /// In the garage.
    Collected,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BarnRecord {
    pub state: BarnState,
    /// `MissionsSave::miles` when the state last changed.
    pub at_miles: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EncounterStats {
    pub won: u32,
    pub lost: u32,
    pub streak: u32,
    pub best_streak: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MissionsSave {
    /// Best speed (mph) per speed camera (`speed_camera_NN`).
    pub speed_cameras: BTreeMap<String, f32>,
    /// Best average speed (mph) per zone (`average_speed_NN`).
    pub average_speed: BTreeMap<String, f32>,
    /// Outposts discovered (`GasStation_NN`).
    pub outposts: Vec<String>,
    /// Per mission name (`speedstunt_01`, `prstunt_01`, `photoshoot_01`).
    pub missions: BTreeMap<String, MissionRecord>,
    /// Per barn find (`barnfind_01`).
    pub barns: BTreeMap<String, BarnRecord>,
    /// Miles driven in free roam (barn-find rumours and restoration run on it).
    pub miles: f32,
    /// Miles at which the next barn rumour spawns (0 = not rolled yet).
    pub next_rumour_miles: f32,
    pub encounters: EncounterStats,
    /// Display-only achievements (`ACHIEVEMENT_BARN_THIS_WAY`).
    pub achievements: Vec<String>,
}
