//! `ailines` group: the AI racing lines and the gamedb AI tables, for the engine's AI drivers (fh1-engine ai/, docs/AI.md).
//!
//! Output:
//! - `<track>/route_NNN.owt`: the racing line of route NNN as stored in `media/aiopenworld.zip` (`OWTM`; parsed by
//!   `fh1_engine::ai::line`). Route NNN belongs to `Ribbon_00/TrackRouteNNN.xml` (file number = gamedb Tracks.id).
//! - `index.json`: `{ "<track>": [route ids...] }`.
//! - `ai_tables.json`: every row of the AI tables (AISkills, AIRubberbands, AIRubberbandModeChoices, AITemperaments,
//!   AILineChoices, AIPlayers, AISkillAdjustments, MPAIDifficultyMap, TrackSpecificAI, FreeRoamDrivers, AIDiff), keyed by
//!   table, with the original column names.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use fh1_formats::zip::Archive;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};

const TABLES: &[&str] = &[
    "AISkills",
    "AIRubberbands",
    "AIRubberbandModeChoices",
    "AITemperaments",
    "AILineChoices",
    "AIPlayers",
    "AISkillAdjustments",
    "MPAIDifficultyMap",
    "TrackSpecificAI",
    "FreeRoamDrivers",
    "AIDiff",
];

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let mut ar = Archive::open(disc.join("media/aiopenworld.zip")).context("media/aiopenworld.zip")?;
    let mut index: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for e in ar.entries.clone() {
        // colorado/Ribbon_00/route_002.owt
        let name = e.name.replace('\\', "/");
        let mut parts = name.split('/');
        let (Some(track), Some(file)) = (parts.next(), name.rsplit('/').next()) else { continue };
        let Some(id) = file.strip_prefix("route_").and_then(|s| s.strip_suffix(".owt")).and_then(|s| s.parse::<u32>().ok()) else { continue };
        let bytes = ar.read(&e)?;
        anyhow::ensure!(bytes.starts_with(b"OWTM"), "{name}: not an OWTM line");
        let dir = out.join(track.to_ascii_lowercase());
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("route_{id:03}.owt")), &bytes)?;
        index.entry(track.to_ascii_lowercase()).or_default().push(id);
    }
    for ids in index.values_mut() {
        ids.sort_unstable();
    }
    std::fs::write(out.join("index.json"), serde_json::to_vec_pretty(&index)?)?;

    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut tables = Map::new();
    for t in TABLES {
        tables.insert((*t).into(), Value::Array(rows(&db, &format!("SELECT * FROM {t}"))?.into_iter().map(Value::Object).collect()));
    }
    std::fs::write(out.join("ai_tables.json"), serde_json::to_vec_pretty(&Value::Object(tables))?)?;
    println!("[ailines] {} lines, {} tables", index.values().map(Vec::len).sum::<usize>(), TABLES.len());
    Ok(())
}

fn rows(db: &Connection, sql: &str) -> Result<Vec<Map<String, Value>>> {
    let mut stmt = db.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut q = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = q.next()? {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(x) => x.into(),
                ValueRef::Real(x) => serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null),
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
                ValueRef::Blob(_) => Value::Null,
            };
            m.insert(n.clone(), v);
        }
        out.push(m);
    }
    Ok(out)
}
