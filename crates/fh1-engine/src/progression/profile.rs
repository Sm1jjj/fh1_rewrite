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

/// How an owned car was acquired (wallet.rs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CarSource {
    #[default]
    Starter,
    Bought,
    /// Event prize (Rewards_EventPrizes).
    Prize,
    /// Wristband reward (Rewards_Wristband).
    Wristband,
    BarnFind,
}

/// One car in the player's garage (profile.json `owned`, version 2).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnedCar {
    /// MediaName.
    pub car: String,
    pub source: CarSource,
    /// Credits paid (0 for starter / prize / reward cars).
    pub paid: i64,
}

/// One credits change (profile.json `ledger`, the last [`LEDGER_LEN`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LedgerEntry {
    pub delta: i64,
    pub balance: i64,
    pub reason: String,
}

pub const LEDGER_LEN: usize = 100;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileData {
    pub version: u32,
    /// Wristband XP (race results).
    pub xp: u64,
    pub credits: i64,
    /// Popularity (banked skill chains).
    pub fame: u64,
    /// Story movies already shown (ui/intro.rs: FMV_01 / FMV_02 / FMV_04), once per profile.
    pub fmv_seen: Vec<String>,
    /// First-time career steps done (ui/intro.rs): "intro_done", "festival_arrived", "race_central_seen", "first_wristband".
    pub story_flags: Vec<String>,
    /// Free-roam activities (missions.rs, docs/MISSIONS.md).
    #[serde(default)]
    pub missions: crate::missions::save::MissionsSave,
    pub events: BTreeMap<String, EventRecord>,
    /// Prize / wristband reward cars won (MediaName).
    pub cars_won: Vec<String>,
    /// Highest wristband already announced (tier-up banner once).
    pub tier_seen: u32,
    /// Best popularity rank already paid out (sponsor milestones once).
    pub rank_paid: u32,
    pub skills: SkillStats,
    /// Version 2 (P10 economy, 2026-10-08): the garage, the last credit changes, sponsor ranks paid per challenge id,
    /// and skill awards per sponsor skill name and grade (0 = Ultimate .. 3 = base grade).
    pub owned: Vec<OwnedCar>,
    pub ledger: Vec<LedgerEntry>,
    pub sponsor_paid: BTreeMap<String, u32>,
    pub skill_grades: BTreeMap<String, [u32; 4]>,
}

#[derive(Resource)]
pub struct Profile {
    pub data: ProfileData,
    /// Credit changes not yet sent as `CreditsChanged` messages (wallet.rs; flushed every frame).
    pub(crate) credit_events: Vec<super::wallet::CreditsChanged>,
    path: PathBuf,
    /// Bumped on every change (catalog / screens rebuild).
    pub generation: u32,
    /// `FH1_PROGRESSION=0`: nothing is read or written.
    persist: bool,
}

impl Profile {
    pub fn load(path: PathBuf, persist: bool) -> Self {
        let data = if persist { read(&path) } else { ProfileData::default() };
        let mut data = ProfileData { rank_paid: if data.rank_paid == 0 { 250 } else { data.rank_paid }, ..data };
        super::wallet::migrate(&mut data);
        Self { data, credit_events: Vec::new(), path, generation: 1, persist }
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
