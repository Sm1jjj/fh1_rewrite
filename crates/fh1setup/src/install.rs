//! installation.json bookkeeping and per-group staging.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{cars, extract::hex, GROUPS};

#[derive(Serialize, Deserialize, Default)]
pub struct Installation {
    /// Directory name under `installations/`.
    pub id: String,
    pub source: String,
    /// SHA-256 of default.xex.
    pub source_hash: String,
    /// group -> pipeline hash of the output currently on disk.
    pub pipelines: BTreeMap<String, String>,
}

fn pipeline_hash(group: &str, version: &str) -> String {
    let mut h = Sha256::new();
    h.update(group.as_bytes());
    h.update([0]);
    h.update(version.as_bytes());
    hex(&h.finalize())[..16].to_owned()
}

pub fn run(disc: &Path, data: &Path, force: bool, only: Option<&[String]>) -> Result<()> {
    let xex = std::fs::read(disc.join("default.xex")).context("reading default.xex")?;
    let source_hash = hex(&Sha256::digest(&xex));
    let id = source_hash[..16].to_owned();

    let manifest_path = data.join("installation.json");
    let mut inst: Installation = std::fs::read(&manifest_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|i: &Installation| i.source_hash == source_hash)
        .unwrap_or_default();
    inst.id = id.clone();
    inst.source = disc.display().to_string();
    inst.source_hash = source_hash;

    let private = data.join("installations").join(&id).join("assets").join("private");
    std::fs::create_dir_all(&private)?;

    for &(group, version) in GROUPS {
        if only.is_some_and(|o| !o.iter().any(|g| g == group)) {
            continue;
        }
        let hash = pipeline_hash(group, version);
        let out = private.join(group);
        if !force && out.is_dir() && inst.pipelines.get(group) == Some(&hash) {
            println!("[{group}] up to date");
            continue;
        }
        // Build into a staging folder, then swap, so a failed run never leaves half a group.
        let stage = private.join(format!("{group}.staging"));
        if stage.exists() {
            std::fs::remove_dir_all(&stage)?;
        }
        std::fs::create_dir_all(&stage)?;
        println!("[{group}] building ({version})");
        match group {
            "cars" => cars::build(disc, &stage)?,
            "world" => crate::world::build(disc, &stage)?,
            "scenery" => crate::scenery::build(disc, &stage)?,
            "shaders" => crate::shaders::build(disc, &stage)?,
            "tracks" => crate::tracks::build(disc, &stage)?,
            "dynamicpost" => crate::dynamicpost::build(disc, &stage)?,
            "grass" => crate::grass::build(disc, &stage)?,
            "crowd" => crate::crowd::build(disc, &stage)?,
            "anim" => crate::anim::build(disc, &stage)?,
            "sky" => crate::sky::build(disc, &stage)?,
            "camera" => crate::camera::build(disc, &stage)?,
            "effects" => crate::effects::build(disc, &stage)?,
            "events" => crate::events::build(disc, &stage)?,
            "ailines" => crate::ailines::build(disc, &stage)?,
            "traffic" => crate::traffic::build(disc, &stage)?,
            "upgrades" => crate::upgrades::build(disc, &stage)?,
            "variants" => crate::variants::build(disc, &stage)?,
            "remaster" => crate::remaster::build(disc, &stage)?,
            "radio" => fh1_radio::install::build(disc, &stage)?,
            "ui" => fh1_ui::install::build(disc, &stage)?,
            "audio" => crate::audio::build(disc, &stage)?,
            _ => unreachable!(),
        }
        // Move the old group aside before deleting it: a running engine can hold files open, and
        // deleting first would leave the group empty if the swap then fails.
        let old = private.join(format!("{group}.old"));
        if old.exists() {
            std::fs::remove_dir_all(&old)?;
        }
        if out.exists() {
            std::fs::rename(&out, &old).with_context(|| format!("{} is in use (close fh1-engine and retry)", out.display()))?;
        }
        std::fs::rename(&stage, &out)?;
        if old.exists() {
            if let Err(e) = std::fs::remove_dir_all(&old) {
                println!("[{group}] could not remove {}: {e} (removed on the next run)", old.display());
            }
        }
        inst.pipelines.insert(group.to_owned(), hash);
        // Record progress after every group so an interrupted run resumes.
        std::fs::write(&manifest_path, serde_json::to_vec_pretty(&inst)?)?;
    }
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&inst)?)?;
    println!("installation {id} ready in {}", private.display());
    Ok(())
}
