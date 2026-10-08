//! `fh1setup import-fh2 <FH2 disc.iso | extracted folder> [--data <dir>] [--only cars,map]`: Forza Horizon 2 (Xbox 360)
//! cars and its open world (Southern Europe, `media/tracks/Anthem`) into the ACTIVE install, through the same readers
//! and converters as FH1 (the formats are FH1's, docs/FH2_RECON.md). Output (git-ignored install data only):
//!
//! - `imported/fh2/cars/<MediaName>/` exactly like `cars/<MediaName>/` (the cars group), plus FH2's own shared
//!   folders (`wheels/`, `shared/`, `carlights/`, ...) next to them. `physics.json` gets `imported_from: "fh2"` and,
//!   when FH1's audio has no bank for the car, `sound_donor` = the closest FH1 car by engine layout.
//! - `imported/fh2/cars/index.json`: the import contract rows `{id, media_name, name, maker, year, class, pi, drive,
//!   has_model}` (shared with the other imported games; the engine addresses a car as `../imported/fh2/cars/<media>`).
//! - `imported/fh2/anthem/`: `world/` (collision + spawns, world group), `scenery/` (scenery group format), `tracks/`
//!   (TimeOfDay, TrackSettings, post zones), `shaders/track/` (FH2's own track `.fxobj`), `dynamicpost/` (FH2's post
//!   templates + grading LUTs, `Tracks/Anthem`), `track.json` `{name, game}`.
//! - `imported/fh2/maps.json`: `[{id: "fh2/anthem", name}]` (map id = folder under `imported/`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

/// Map id and display name of FH2's open world.
pub const MAP_ID: &str = "fh2/anthem";
pub const MAP_NAME: &str = "Southern Europe (FH2)";

pub fn run() -> Result<()> {
    let mut it = std::env::args().skip(2);
    let (mut source, mut data, mut only) = (None, PathBuf::from("data"), None::<Vec<String>>);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data" => data = it.next().context("--data needs a path")?.into(),
            "--only" => only = Some(it.next().context("--only needs cars,map")?.split(',').map(str::to_owned).collect()),
            _ => source = Some(PathBuf::from(a)),
        }
    }
    let source = source.context("usage: fh1setup import-fh2 <FH2 disc.iso | extracted folder> [--data <dir>] [--only cars,map]")?;
    let disc = crate::extract::resolve_disc(&source, &data.join("work_fh2"))?;
    anyhow::ensure!(disc.join("media/tracks/Anthem/bin.zip").exists(), "{}: no media/tracks/Anthem/bin.zip (not a Forza Horizon 2 disc?)", disc.display());
    // The active FH1 install (installation.json), like the engine's data::private_assets.
    let inst: Value = serde_json::from_slice(&std::fs::read(data.join("installation.json")).context("installation.json: run fh1setup on the FH1 disc first")?)?;
    let private = data.join("installations").join(inst["id"].as_str().context("installation.json id")?).join("assets/private");
    let root = private.join("imported/fh2");
    std::fs::create_dir_all(&root)?;
    let want = |p: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == p));
    if want("cars") {
        swap(&root.join("cars"), |stage| cars(&disc, &private, stage))?;
    }
    if want("map") {
        swap(&root.join("anthem"), |stage| map(&disc, stage))?;
        std::fs::write(root.join("maps.json"), serde_json::to_vec_pretty(&json!([{ "id": MAP_ID, "name": MAP_NAME }]))?)?;
    }
    println!("[fh2] done: {}", root.display());
    Ok(())
}

/// Build into `<out>.staging`, then replace `out` (a failed run never leaves half a folder).
pub(crate) fn swap(out: &Path, build: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let stage = out.with_extension("staging");
    if stage.exists() {
        std::fs::remove_dir_all(&stage)?;
    }
    std::fs::create_dir_all(&stage)?;
    build(&stage)?;
    let old = out.with_extension("old");
    if old.exists() {
        std::fs::remove_dir_all(&old)?;
    }
    if out.exists() {
        std::fs::rename(out, &old).with_context(|| format!("{} is in use (close fh1-engine and retry)", out.display()))?;
    }
    std::fs::rename(&stage, out)?;
    if old.exists() {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

fn map(disc: &Path, dir: &Path) -> Result<()> {
    crate::world::build_track(disc, "Anthem", &dir.join("world"))?;
    crate::tracks::build_track(disc, "Anthem", &dir.join("tracks"))?;
    let n = crate::shaders::track_shaders(&disc.join("media/tracks/Anthem/bin.zip"), &dir.join("shaders/track"))?;
    println!("[fh2] {n} track shaders");
    let src = crate::scenery::TrackSrc::find(disc, "Anthem", "scenery")?;
    println!("[fh2] scenery: {} / {}.NNNNN.rmb.bin", src.stem, src.model_prefix);
    crate::scenery::build_track(&src, dir)?;
    // FH2's post templates + grading LUTs (dynamicpost group layout), and Anthem's default LUTs next to its templates.
    crate::dynamicpost::build(disc, &dir.join("dynamicpost"))?;
    for f in ["ColorGradingLookup.dds", "ColorGradingLookup_Night.dds"] {
        let src = disc.join("media/tracks/Anthem").join(f);
        if src.exists() {
            std::fs::copy(&src, dir.join("dynamicpost/Tracks/Anthem").join(f))?;
        }
    }
    std::fs::write(dir.join("track.json"), serde_json::to_vec_pretty(&json!({ "name": MAP_NAME, "game": "fh2" }))?)?;
    Ok(())
}

fn cars(disc: &Path, private: &Path, out: &Path) -> Result<()> {
    crate::cars::build(disc, out)?;
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let strings = fh1_ui::strtable::StringTables::load_language(disc, "EN").ok();
    let text = |r: &str| strings.as_ref().and_then(|s| s.resolve(r)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| r.to_owned());
    // Class letter from the badge prefix (`CLASS_S1`), and the game's PI display scale (normalised PI -> 100..999).
    let classes: Vec<(i64, String, f64, f64)> = db
        .prepare("SELECT Id, BadgeTexturePathPrefix, MaxPerformanceIndex, MaxDisplayPerformanceIndex FROM CarClasses ORDER BY Id")?
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    let display_pi = |pi: f64| -> i64 {
        let (mut lo, mut lo_d) = (0.0, 100.0);
        for &(_, _, hi, hi_d) in &classes {
            if pi <= hi {
                return (lo_d + (pi - lo) / (hi - lo).max(1e-9) * (hi_d - lo_d)).round() as i64;
            }
            (lo, lo_d) = (hi, hi_d);
        }
        lo_d as i64
    };
    let makes: HashMap<i64, String> = db
        .prepare("SELECT ID, DisplayName FROM List_CarMake")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(id, n)| (id, text(&n)))
        .collect();
    let donors = Donors::load(private);
    let index: Vec<Value> = serde_json::from_slice(&std::fs::read(out.join("index.json"))?)?;
    let (mut rows, mut donated) = (Vec::new(), 0);
    for c in &index {
        let media = c["media_name"].as_str().unwrap_or_default().to_owned();
        let phys_path = out.join(&media).join("physics.json");
        let mut phys: Value = serde_json::from_slice(&std::fs::read(&phys_path)?)?;
        let car = phys["car"].clone();
        phys["imported_from"] = json!("fh2");
        if let Some(d) = donors.pick(&media, &car) {
            phys["sound_donor"] = json!(d);
            donated += 1;
        }
        std::fs::write(&phys_path, serde_json::to_vec_pretty(&phys)?)?;
        let class = classes.iter().find(|k| Some(k.0) == car["ClassID"].as_i64()).map(|k| k.1.trim_start_matches("CLASS_").to_owned());
        let drive = match car["DriveTypeID"].as_i64() {
            Some(1) => "FWD",
            Some(2) => "RWD",
            Some(3) => "AWD",
            _ => "",
        };
        let model = car["ModelShort"].as_str().map(text).unwrap_or_default();
        let name = car["DisplayName"].as_str().map(text).unwrap_or_else(|| media.clone());
        rows.push(json!({
            "id": c["id"],
            "media_name": media,
            "name": name,
            "model": model,
            "maker": car["MakeID"].as_i64().and_then(|m| makes.get(&m).cloned()).unwrap_or_default(),
            "year": c["year"],
            "class": class,
            "pi": car["PerformanceIndex"].as_f64().map(display_pi),
            "drive": drive,
            "selectable": car["IsSelectable"].as_i64() == Some(1),
            "has_model": c["has_model"],
        }));
    }
    std::fs::write(out.join("index.json"), serde_json::to_vec_pretty(&rows)?)?;
    println!("[fh2] {} cars, {donated} with an FH1 sound donor", rows.len());
    Ok(())
}

/// FH1 cars that have an engine sound bank (`audio/cars/<media>.json`), with their engine layout.
pub(crate) struct Donors {
    cars: Vec<(String, Value)>,
}

impl Donors {
    pub(crate) fn load(private: &Path) -> Self {
        let mut cars = Vec::new();
        for e in std::fs::read_dir(private.join("audio/cars")).into_iter().flatten().flatten() {
            let Some(media) = e.path().file_stem().and_then(|s| s.to_str()).map(str::to_owned) else { continue };
            let Some(car) = std::fs::read(private.join("cars").join(&media).join("physics.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .map(|p| p["car"].clone())
            else {
                continue;
            };
            cars.push((media, car));
        }
        cars.sort_by(|a, b| a.0.cmp(&b.0));
        Self { cars }
    }

    /// None when FH1 has the car's own bank; else the closest FH1 car: same cylinder count / aspiration / engine
    /// config weigh most, then displacement, power and year.
    pub(crate) fn pick(&self, media: &str, car: &Value) -> Option<String> {
        if self.cars.iter().any(|(m, _)| m.eq_ignore_ascii_case(media)) {
            return None;
        }
        let f = |c: &Value, k: &str| c[k].as_f64().unwrap_or(0.0);
        let cost = |c: &Value| {
            let mut d = 0.0;
            for (k, w) in [("CylinderID", 4.0), ("AspirationTypeId", 2.0), ("EngineConfigID", 2.0), ("EnginePlacementID", 0.5)] {
                d += if f(c, k) == f(car, k) { 0.0 } else { w };
            }
            d + (f(c, "Displacement") - f(car, "Displacement")).abs() / 1000.0
                + (f(c, "SimPeakPower") - f(car, "SimPeakPower")).abs() / 1500.0
                + (f(c, "Year") - f(car, "Year")).abs() / 40.0
        };
        self.cars.iter().min_by(|a, b| cost(&a.1).total_cmp(&cost(&b.1))).map(|(m, _)| m.clone())
    }
}
