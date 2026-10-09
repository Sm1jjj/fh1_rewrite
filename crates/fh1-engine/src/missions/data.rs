//! `missions/colorado/missions.json` (fh1setup missions.rs, group `missions`), engine space (yaw 0 = facing -Z).
// Some install fields are kept for the doc / later use (scoreboard ids, doors, arena route).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use bevy::prelude::*;
use serde::Deserialize;

#[derive(Deserialize, Clone, Copy, Debug, Default)]
pub struct Pose {
    pub pos: [f32; 3],
    pub yaw: f32,
}

impl Pose {
    pub fn point(&self) -> Vec3 {
        Vec3::from_array(self.pos)
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct CarRef {
    /// Data_Car.Id.
    pub id: i64,
    /// MediaName (`cars/<media>`), None when gamedb has no such id.
    pub media: Option<String>,
    pub name: Option<String>,
    #[allow(dead_code)]
    pub colour: i64,
}

impl CarRef {
    pub fn label(&self) -> String {
        self.name.clone().or_else(|| self.media.clone()).unwrap_or_else(|| format!("car {}", self.id))
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SpeedCamera {
    pub name: String,
    pub scoreboard_id: i64,
    pub min_mph: f32,
    pub left: [f32; 3],
    pub right: [f32; 3],
    pub label: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct AverageZone {
    pub name: String,
    #[allow(dead_code)]
    pub scoreboard_id: i64,
    pub min_mph: f32,
    /// (left, right) gate posts.
    pub start: [[f32; 3]; 2],
    pub end: [[f32; 3]; 2],
    pub label: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Outpost {
    pub name: String,
    pub object: String,
    pub pos: [f32; 3],
    /// Activation zone (m) and its speed limit (mph).
    pub radius: f32,
    pub max_mph: f32,
    /// Discovery radius (TriggerZone triggerZoneRadius).
    pub discover_radius: f32,
    pub title: String,
    /// Where mission cars are placed back (OUTPOST_NNN_NODE).
    pub place: Pose,
    /// Mission names (`speedstunt_01`, `photoshoot_01`, `prstunt_01`).
    pub missions: Vec<String>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SpeedStunt {
    pub name: String,
    pub start: Option<Pose>,
    /// The speed camera (`speed_camera_NN`).
    pub trap: String,
    pub trap_label: String,
    /// easy / medium / hard.
    pub speed_mph: [f32; 3],
    pub time_s: [f32; 3],
    pub car: CarRef,
    pub instruction: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PrStunt {
    pub name: String,
    pub arena_route: i64,
    pub start: Option<Pose>,
    /// Entrance gate posts (left, right).
    pub entrance: [[f32; 3]; 2],
    /// `mission_prstunt_end_NN`: the suggested route through the arena.
    pub path: Vec<[f32; 3]>,
    /// Skill-points target (Popularity) easy / medium / hard.
    pub target: [f32; 3],
    pub time_s: [f32; 3],
    pub car: CarRef,
    pub instruction_to: String,
    pub instruction_in: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PhotoZone {
    pub pos: [f32; 3],
    pub radius: f32,
    pub max_mph: Option<f32>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PhotoShoot {
    pub name: String,
    pub start: Option<Pose>,
    pub zones: Vec<PhotoZone>,
    pub pose: Option<Pose>,
    /// Landmark points; `min_in_shot` of them must be in frame.
    pub nodes: Vec<[f32; 3]>,
    pub min_in_shot: f32,
    /// Damage allowed (easy / medium / hard).
    pub damage: [f32; 3],
    pub low_speed_collision_mph: f32,
    pub damage_low_speed: f32,
    pub damage_smashable: f32,
    pub car: CarRef,
    pub requirements: String,
    pub instruction_to: String,
    pub instruction_in: String,
    pub location: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SpawnInfo {
    pub weighting: f32,
    pub min_probability: f32,
    pub min_distance: f32,
    pub max_probability: f32,
    pub max_distance: f32,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct HintRegion {
    pub x_offset: f32,
    pub y_offset: f32,
    #[allow(dead_code)]
    pub angle: f32,
    pub x_radius: f32,
    pub y_radius: f32,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct DistanceMiles {
    pub min: f32,
    pub max: f32,
    pub threshold: f32,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct BarnDoors {
    pub open: String,
    pub closed: String,
    pub open_coll: String,
    pub closed_coll: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct BarnFind {
    pub name: String,
    pub object: String,
    pub pos: [f32; 3],
    pub radius: f32,
    pub max_mph: f32,
    pub car: CarRef,
    pub spawn: SpawnInfo,
    pub hint: HintRegion,
    pub distance_miles: DistanceMiles,
    #[allow(dead_code)]
    pub doors: BarnDoors,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct RaceEncounter {
    #[allow(dead_code)]
    pub map_tag: String,
    pub description: String,
    /// Reward multiplier per wristband tier (0 = Yellow ..).
    pub wristband_multipliers: Vec<f32>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct MissionData {
    pub version: u32,
    pub speed_cameras: Vec<SpeedCamera>,
    pub average_speed: Vec<AverageZone>,
    pub outposts: Vec<Outpost>,
    pub speed_stunts: Vec<SpeedStunt>,
    pub pr_stunts: Vec<PrStunt>,
    pub photo_shoots: Vec<PhotoShoot>,
    pub barn_finds: Vec<BarnFind>,
    pub race_encounter: RaceEncounter,
    pub texts: BTreeMap<String, String>,
}

impl MissionData {
    /// `<assets>/missions/colorado/missions.json`; empty when the group is not installed.
    pub fn load(assets: &Path) -> Self {
        let path = assets.join("missions/colorado/missions.json");
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                warn!("missions: {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => {
                info!("missions: {} not installed (fh1setup group `missions`)", path.display());
                Self::default()
            }
        }
    }

    /// A UI text by its IDS id (EN, baked at setup), else `fallback`.
    pub fn text(&self, id: &str, fallback: &str) -> String {
        self.texts.get(id).filter(|t| !t.is_empty() && t.as_str() != id).cloned().unwrap_or_else(|| fallback.to_owned())
    }

    pub fn camera(&self, name: &str) -> Option<&SpeedCamera> {
        self.speed_cameras.iter().find(|c| c.name == name)
    }
}
