//! gamedb AI tables (`ailines/ai_tables.json`, written by fh1setup ailines.rs) as typed rows (docs/AI.md "Tables").
//!
//! AISkills: ids 1..80 easiest->hardest is the WRONG way round: id 1 has the highest braking/cornering factors
//! (Braking 0.95-1.0, Cornering 1.188-1.25) and id 80 the lowest (0.585-0.65 / 0.495-0.55); 201-207 are fixed test skills.
//! AIPlayers.SkillModifier (-20..2) and AggroModifier (-3..2) are offsets on the skill / temperament ids (INFERRED from
//! their ranges).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub struct Skill {
    pub id: u32,
    pub traction_assist: f32,
    pub braking: [f32; 2],
    pub cornering: [f32; 2],
    /// Metres behind the player at which braking / cornering reach their maximum (SoloLapping_DistBehindForMaxPerformance).
    pub dist_behind_for_max: f32,
    pub nominal_torque_scale: f32,
    pub dist_behind_for_torque_boost: f32,
    pub far_behind_torque_boost: f32,
    pub rubberband: u32,
}

impl Default for Skill {
    fn default() -> Self {
        // AISkills 40 (a middle row).
        Self {
            id: 40,
            traction_assist: 0.0,
            braking: [0.763, 0.825],
            cornering: [0.74, 0.8],
            dist_behind_for_max: 15.0,
            nominal_torque_scale: 1.0,
            dist_behind_for_torque_boost: 0.0,
            far_behind_torque_boost: 1.0,
            rubberband: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Rubberband {
    pub id: u32,
    pub start_rubberbanding: f32,
    pub max_rubberbanding_effect: f32,
    pub start_torque_cut: f32,
    pub max_torque_cut: f32,
    pub torque_cut_factor: f32,
    pub start_catch_up: f32,
    pub stop_catch_up: f32,
    pub catch_up_factor: f32,
    pub catch_up_gap_per_position: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct Temperament {
    pub id: u32,
    pub aggression: f32,
    pub start_pass_at_impact_time: f32,
    pub trailing_distance: f32,
    pub interaction_line_pass_probability: f32,
    pub low_rel_vel_pass_probability: f32,
    /// Metres kept from other cars / from the road edge when passing.
    pub car_clearance: f32,
    pub track_edge_clearance: f32,
}

impl Default for Temperament {
    fn default() -> Self {
        Self {
            id: 5,
            aggression: 0.6,
            start_pass_at_impact_time: 4.0,
            trailing_distance: 7.0,
            interaction_line_pass_probability: 0.73,
            low_rel_vel_pass_probability: 0.8,
            car_clearance: 0.3,
            track_edge_clearance: 0.4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Player {
    pub id: u32,
    pub skill_modifier: i32,
    pub aggro_modifier: i32,
    pub rubberband_modifier: i32,
}

#[derive(Debug, Clone, Default)]
pub struct AiTables {
    pub skills: HashMap<u32, Skill>,
    pub rubberbands: HashMap<u32, Rubberband>,
    pub temperaments: HashMap<u32, Temperament>,
    pub players: HashMap<u32, Player>,
    /// AILineChoices: skill id -> (LineID, RelativeFrequency).
    pub line_choices: HashMap<u32, Vec<(u32, f32)>>,
}

/// One driver's resolved parameters.
#[derive(Debug, Clone, Copy, Default)]
pub struct DriverParams {
    pub skill: Skill,
    pub rubberband: Rubberband,
    pub temperament: Temperament,
}

impl AiTables {
    pub fn load(path: &Path) -> Result<Self> {
        let v: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        Ok(Self::from_json(&v))
    }

    pub fn from_json(v: &Value) -> Self {
        let rows = |t: &str| v[t].as_array().cloned().unwrap_or_default();
        let f = |r: &Value, k: &str| r[k].as_f64().unwrap_or(0.0) as f32;
        let id = |r: &Value, k: &str| r[k].as_i64().unwrap_or(0) as u32;
        let mut t = Self::default();
        for r in rows("AISkills") {
            let s = Skill {
                id: id(&r, "id"),
                traction_assist: f(&r, "SoloLapping_OfflineTractionAssistFactor"),
                braking: [f(&r, "SoloLapping_BrakingMinimum"), f(&r, "SoloLapping_BrakingMaximum")],
                cornering: [f(&r, "SoloLapping_CorneringMinimum"), f(&r, "SoloLapping_CorneringMaximum")],
                dist_behind_for_max: f(&r, "SoloLapping_DistBehindForMaxPerformance"),
                nominal_torque_scale: f(&r, "SoloLapping_NominalTorqueScale"),
                dist_behind_for_torque_boost: f(&r, "SoloLapping_DistBehindForTorqueBoost"),
                far_behind_torque_boost: f(&r, "SoloLapping_FarBehindTorqueBoost"),
                rubberband: id(&r, "Rubberbands_id"),
            };
            t.skills.insert(s.id, s);
        }
        for r in rows("AIRubberbands") {
            let b = Rubberband {
                id: id(&r, "id"),
                start_rubberbanding: f(&r, "StartRubberbandingDistance"),
                max_rubberbanding_effect: f(&r, "MaxRubberbandingEffectDistance"),
                start_torque_cut: f(&r, "StartTorqueCutDistance"),
                max_torque_cut: f(&r, "MaxTorqueCutDistance"),
                torque_cut_factor: f(&r, "TorqueCutFactor"),
                start_catch_up: f(&r, "StartCatchUpDistance"),
                stop_catch_up: f(&r, "StopCatchUpDistance"),
                catch_up_factor: f(&r, "CatchUpFactor"),
                catch_up_gap_per_position: f(&r, "CatchUpGapPerPosition"),
            };
            t.rubberbands.insert(b.id, b);
        }
        for r in rows("AITemperaments") {
            let m = Temperament {
                id: id(&r, "id"),
                aggression: f(&r, "Passing_AggressionLevel"),
                start_pass_at_impact_time: f(&r, "Passing_StartPassAtImpactTime"),
                trailing_distance: f(&r, "Passing_TrailingDistance"),
                interaction_line_pass_probability: f(&r, "Passing_InteractionLinePassProbability"),
                low_rel_vel_pass_probability: f(&r, "Passing_LowRelVelPassProbability"),
                car_clearance: f(&r, "Passing_CarClearance"),
                track_edge_clearance: f(&r, "Passing_TrackEdgeClearance"),
            };
            t.temperaments.insert(m.id, m);
        }
        for r in rows("AIPlayers") {
            let p = Player {
                id: id(&r, "Id"),
                skill_modifier: r["SkillModifier"].as_i64().unwrap_or(0) as i32,
                aggro_modifier: r["AggroModifier"].as_i64().unwrap_or(0) as i32,
                rubberband_modifier: r["RubberBandModifier"].as_i64().unwrap_or(0) as i32,
            };
            t.players.insert(p.id, p);
        }
        for r in rows("AILineChoices") {
            t.line_choices.entry(id(&r, "AISkills_id")).or_default().push((id(&r, "LineID"), f(&r, "RelativeFrequency")));
        }
        t
    }

    /// Resolve a driver: skill / temperament / rubber band ids (0 = defaults) plus the AIPlayers row's modifiers.
    pub fn driver(&self, skill: u32, temperament: u32, rubberband: u32, player: u32) -> DriverParams {
        let p = self.players.get(&player).copied().unwrap_or_default();
        let skill_id = if skill == 0 { 40 } else { skill };
        // Modifiers shift within the 1..80 band (ids 201+ are fixed test skills and aren't shifted).
        let skill_id = if skill_id <= 80 { (skill_id as i32 + p.skill_modifier).clamp(1, 80) as u32 } else { skill_id };
        let skill = self.skills.get(&skill_id).copied().unwrap_or_default();
        let temp_id = if temperament == 0 { 5 } else { temperament };
        let temp_id = (temp_id as i32 + p.aggro_modifier).clamp(1, 10) as u32;
        let temperament = self.temperaments.get(&temp_id).copied().unwrap_or_default();
        let rb_id = if rubberband != 0 { rubberband } else { skill.rubberband };
        let rubberband = self.rubberbands.get(&rb_id).copied().unwrap_or_default();
        DriverParams { skill, rubberband, temperament }
    }
}
