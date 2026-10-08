//! The player's career save: `<data>/profile.json` (beside settings.json / garage.json). Written atomically (temp file
//! + rename) after every change that matters (race result, banked skill chain, tier up). Unknown / missing fields
//! default, so older saves keep loading.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Best result per event (key = HorizonEventID).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EventRecord {
    /// Best finishing place (1-based).
    pub best_place: u8,
    pub best_time_s: Option<f32>,
    pub runs: u32,
    pub wins: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillStats {
    /// Awards per skill name.
    pub counts: BTreeMap<String, u32>,
    /// Best banked chain (fame).
    pub best_chain: u64,
    pub chains_banked: u32,
    pub chains_lost: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileData {
    pub version: u32,
    /// Wristband XP (race results).
    pub xp: u64,
    pub credits: i64,
    /// Popularity (banked skill chains).
    pub fame: u64,
    pub events: BTreeMap<String, EventRecord>,
    /// Prize / wristband reward cars won (MediaName).
    pub cars_won: Vec<String>,
    /// Highest wristband already announced (tier-up banner once).
    pub tier_seen: u32,
    /// Best popularity rank already paid out (sponsor milestones once).
    pub rank_paid: u32,
    pub skills: SkillStats,
}

#[derive(Resource)]
pub struct Profile {
    pub data: ProfileData,
    path: PathBuf,
    /// Bumped on every change (catalog / screens rebuild).
    pub generation: u32,
    /// `FH1_PROGRESSION=0`: nothing is read or written.
    persist: bool,
}

impl Profile {
    pub fn load(path: PathBuf, persist: bool) -> Self {
        let data = if persist { read(&path) } else { ProfileData::default() };
        let data = ProfileData { version: 1, rank_paid: if data.rank_paid == 0 { 250 } else { data.rank_paid }, ..data };
        Self { data, path, generation: 1, persist }
    }

    /// Mark changed and save.
    pub fn commit(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if !self.persist {
            return;
        }
        // Serialised here, written (tmp + rename) on the background writer (perf/writer.rs; a save never stalls a frame).
        match serde_json::to_vec_pretty(&self.data) {
            Ok(b) => crate::perf::writer::replace(self.path.clone(), b),
            Err(e) => warn!("progression: saving {}: {e}", self.path.display()),
        }
    }
}

fn read(path: &Path) -> ProfileData {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            // Keep the unreadable file for the user instead of overwriting it with a fresh career.
            let keep = path.with_extension("json.bad");
            warn!("progression: {}: {e}; kept as {}", path.display(), keep.display());
            let _ = std::fs::copy(path, keep);
            ProfileData::default()
        }),
        Err(_) => ProfileData::default(),
    }
}
