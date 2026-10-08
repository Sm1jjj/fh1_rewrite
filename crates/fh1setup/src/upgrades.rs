//! `upgrades` group: what the Customize menu can offer per car (fh1-engine ui/customize.rs, docs/CUSTOMIZE.md).
//!
//! Sources (EU disc, gamedb.slt): every `List_Upgrade*` table with an `IsStock` column (stock row + the Street / Sport /
//! Race levels; engine swaps are `List_UpgradeEngine` rows with IsStock=0), keyed per car by Ordinal, CarBodyID,
//! EngineID or DrivetrainID as the cars group does; `List_Wheels` aftermarket rims (IsStock=0) with their
//! `List_PartManufacturer` maker; `List_SpecialColors` (string-table names); `CarUpgradeExceptions` (NoRimStyles).
//!
//! Output (engine reads `<assets>/upgrades/..`):
//! - `cars/<MediaName>.json`: `{id, media_name, body_id, engine_id, drivetrain_id, no_rim_styles, parts: {table: [row..]}}`,
//!   each table's rows ordered stock first, then by Level, Id. Rows are the gamedb rows as they are; for engine swaps
//!   the swapped engine's Data_Engine row is added under `swap_engine` (the physics for swaps is a later step).
//!   Physics rows a level references are inlined (upgrades-2), in physics.json's shapes: Camshaft rows `_torque_curve`
//!   ({id, torque_scale_nm, max_rpm, samples}), SpringDamper rows `_front` / `_rear` (List_SpringDamperPhysics), AntiSway
//!   rows `_physics` (List_AntiSwayPhysics), TireCompound rows `_tires` ({compound, friction_lateral,
//!   friction_longitudinal, affect_curves}). upgrades-3: front bumper / rear wing rows `_aero` (their List_AeroPhysics row,
//!   physics.json `aero.front_bumper` / `aero.rear_wing` shape); engine swap rows `_engine_parts` ({table: [rows]}: every
//!   engine-keyed part table's rows for the swapped EngineID, inlined the same way), since a swap replaces the whole
//!   engine part set (docs/CUSTOMIZE.md "Upgrade rules").
//! - `rims.json`: `[{id, media_name, name, maker, mass, price, type, exception}]`, aftermarket rims whose folder the
//!   cars group installs (`cars/wheels/<MediaName>`).
//! - `special_colors.json`: `[{id, name, finish, primary, secondary, two_tone: [scale, bias, power]}]`.
//! - `pi.json` (upgrades-3): `physics.zip/PI.xml` for the PI calculator (fh1-engine pi.rs, docs/PI.md): `{min_time,
//!   max_time, track_width, corner_scale, straight_scale, lap_scale: [FWD, RWD, AWD], gears_mph: [..], track:
//!   [{straight, radius, angle}]}`.
//! - `car_classes.json`: CarClasses rows `[{id, max_pi, max_display, letter}]` (letter from BadgeTexturePathPrefix).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

type Row = Map<String, Value>;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let strings = fh1_ui::strtable::StringTables::load_language(disc, "EN").context("EN string tables")?;
    let text = |v: &Value| -> String {
        let r = v.as_str().unwrap_or("");
        strings.resolve(r).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| r.to_owned())
    };
    let tables = upgrade_tables(&db)?;
    let no_rims: HashSet<i64> =
        rows(&db, "SELECT CarID FROM CarUpgradeExceptions WHERE NoRimStyles<>0", [])?.iter().filter_map(|r| r["CarID"].as_i64()).collect();

    let cars_dir = out.join("cars");
    std::fs::create_dir_all(&cars_dir)?;
    let mut written = 0;
    for car in rows(&db, "SELECT Id, MediaName FROM Data_Car ORDER BY Id", [])? {
        let (Some(id), Some(media)) = (car["Id"].as_i64(), car["MediaName"].as_str()) else { continue };
        let stock_id = |table: &str, col: &str| -> Result<Option<i64>> {
            Ok(rows(&db, &format!("SELECT \"{col}\" FROM \"{table}\" WHERE Ordinal=?1 AND IsStock=1"), [id])?.first().and_then(|r| r[col].as_i64()))
        };
        let body = stock_id("List_UpgradeCarBody", "CarBodyID")?;
        let engine = stock_id("List_UpgradeEngine", "EngineID")?;
        let drivetrain = stock_id("List_UpgradeDrivetrain", "DrivetrainID")?;
        let mut parts = Map::new();
        for (table, col, key) in &tables {
            let Some(key_id) = (match key {
                Key::Car => Some(id),
                Key::Body => body,
                Key::Engine => engine,
                Key::Drivetrain => drivetrain,
            }) else {
                continue;
            };
            let mut list = rows(&db, &format!("SELECT * FROM \"{table}\" WHERE \"{col}\"=?1 ORDER BY IsStock DESC, Level, Id"), [key_id])?;
            if list.is_empty() {
                continue;
            }
            if table == "List_UpgradeEngine" {
                for r in list.iter_mut().filter(|r| r["IsStock"].as_i64() == Some(0)) {
                    let e = r["EngineID"].as_i64();
                    let swap = e.map(|e| rows(&db, "SELECT * FROM Data_Engine WHERE EngineID=?1", [e])).transpose()?.and_then(|v| v.into_iter().next());
                    r.insert("swap_engine".into(), swap.map(Value::Object).unwrap_or(Value::Null));
                    // The swapped engine's whole part set (its stock rows + upgrade levels), keyed by the new EngineID.
                    if let Some(e) = e {
                        let mut set = Map::new();
                        for (t, c, k) in &tables {
                            if !matches!(k, Key::Engine) {
                                continue;
                            }
                            let mut l = rows(&db, &format!("SELECT * FROM \"{t}\" WHERE \"{c}\"=?1 ORDER BY IsStock DESC, Level, Id"), [e])?;
                            for x in l.iter_mut() {
                                inline_physics(&db, t, x)?;
                            }
                            if !l.is_empty() {
                                set.insert(t.clone(), Value::Array(l.into_iter().map(Value::Object).collect()));
                            }
                        }
                        r.insert("_engine_parts".into(), Value::Object(set));
                    }
                }
            }
            for r in list.iter_mut() {
                inline_physics(&db, table, r)?;
            }
            parts.insert(table.clone(), Value::Array(list.into_iter().map(Value::Object).collect()));
        }
        let doc = json!({
            "id": id,
            "media_name": media,
            "body_id": body,
            "engine_id": engine,
            "drivetrain_id": drivetrain,
            "no_rim_styles": no_rims.contains(&id),
            "parts": parts,
        });
        std::fs::write(cars_dir.join(format!("{media}.json")), serde_json::to_vec(&doc)?)?;
        written += 1;
    }

    // Aftermarket rims the cars group installs (cars/wheels/<MediaName>/<MediaName>.fxcar).
    let exceptions: HashSet<i64> = rows(&db, "SELECT WheelID FROM WheelExceptions", [])?.iter().filter_map(|r| r["WheelID"].as_i64()).collect();
    let rims: Vec<Value> = rows(
        &db,
        "SELECT w.ID, w.MediaName, w.DisplayName, m.PartManufacturer, w.Mass, w.Price, w.Type FROM List_Wheels w \
         LEFT JOIN List_PartManufacturer m ON m.Id = w.PartManufacturerID WHERE w.IsStock=0 ORDER BY m.PartManufacturer, w.DisplayName",
        [],
    )?
    .into_iter()
    .map(|r| {
        json!({
            "id": r["ID"],
            "media_name": r["MediaName"],
            "name": text(&r["DisplayName"]),
            "maker": text(&r["PartManufacturer"]),
            "mass": r["Mass"],
            "price": r["Price"],
            "type": r["Type"],
            "exception": r["ID"].as_i64().is_some_and(|i| exceptions.contains(&i)),
        })
    })
    .collect();
    std::fs::write(out.join("rims.json"), serde_json::to_vec_pretty(&rims)?)?;

    let specials: Vec<Value> = rows(&db, "SELECT * FROM List_SpecialColors ORDER BY ID", [])?
        .into_iter()
        .map(|r| {
            json!({
                "id": r["ID"],
                "name": text(&r["DisplayName"]),
                "finish": r["Finish"],
                "primary": if r["PriColorValid"].as_i64() == Some(0) { Value::Null } else { r["PriColorRGB"].clone() },
                "secondary": if r["SecColorValid"].as_i64() == Some(0) { Value::Null } else { r["SecColorRGB"].clone() },
                "two_tone": [r["TwoToneScale"], r["TwoToneBias"], r["TwoTonePower"]],
            })
        })
        .collect();
    std::fs::write(out.join("special_colors.json"), serde_json::to_vec_pretty(&specials)?)?;
    pi_tables(disc, &db, out)?;
    println!("[upgrades] {written} cars, {} aftermarket rims, {} special colours", rims.len(), specials.len());
    Ok(())
}

/// `pi.json` (PI.xml) and `car_classes.json` for the engine's PI calculator (docs/PI.md).
fn pi_tables(disc: &Path, db: &Connection, out: &Path) -> Result<()> {
    let mut ar = fh1_formats::zip::Archive::open(disc.join("media/physics.zip")).context("media/physics.zip")?;
    let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case("PI.xml")).cloned().context("physics.zip: PI.xml")?;
    let text = String::from_utf8_lossy(&ar.read(&e)?).into_owned();
    let doc = crate::xml::to_json(text.trim_start_matches('\u{feff}'))?;
    let pi = &doc["PI"];
    let misc = &pi["Misc"];
    let three = |k: &str| json!([misc[format!("FWD{k}")], misc[format!("RWD{k}")], misc[format!("AWD{k}")]]);
    // A single child element is an object, several an array (xml::to_json).
    let list = |v: &Value| -> Vec<Value> {
        match v {
            Value::Array(a) => a.clone(),
            Value::Null => Vec::new(),
            other => vec![other.clone()],
        }
    };
    let gears: Vec<Value> = list(&pi["Gears"]["Gear"]).iter().map(|g| g["RedlineSpeedMPH"].clone()).collect();
    let track: Vec<Value> = list(&pi["Track"]["Segment"])
        .iter()
        .map(|s| json!({"straight": s["StraightLengthMeters"], "radius": s["CornerRadiusMeters"], "angle": s["CornerAngleDegrees"]}))
        .collect();
    let segments = track.len();
    let cfg = json!({
        "min_time": misc["MinPITimeSeconds"],
        "max_time": misc["MaxPITimeSeconds"],
        "track_width": misc["TrackWidthMeters"],
        "corner_scale": three("AccelFrictionScaleWhileCornering"),
        "straight_scale": three("AccelFrictionScaleWhileStraight"),
        "lap_scale": three("LapTimeScale"),
        "gears_mph": gears,
        "track": track,
    });
    std::fs::write(out.join("pi.json"), serde_json::to_vec_pretty(&cfg)?)?;
    let classes: Vec<Value> = rows(db, "SELECT Id, MaxPerformanceIndex, MaxDisplayPerformanceIndex, BadgeTexturePathPrefix FROM CarClasses ORDER BY Id", [])?
        .into_iter()
        .map(|r| {
            let badge = r["BadgeTexturePathPrefix"].as_str().unwrap_or("");
            json!({
                "id": r["Id"],
                "max_pi": r["MaxPerformanceIndex"],
                "max_display": r["MaxDisplayPerformanceIndex"],
                "letter": badge.strip_prefix("CLASS_").unwrap_or(badge),
            })
        })
        .collect();
    std::fs::write(out.join("car_classes.json"), serde_json::to_vec_pretty(&classes)?)?;
    println!("[upgrades] PI track {segments} segments, {} car classes", classes.len());
    Ok(())
}

/// The physics rows an upgrade row points at, inlined under `_`-prefixed keys (see the module docs).
fn inline_physics(db: &Connection, table: &str, r: &mut Row) -> Result<()> {
    let id = |k: &str| r.get(k).and_then(Value::as_i64);
    let one = |sql: &str, id: Option<i64>| -> Result<Value> {
        Ok(match id {
            Some(id) => rows(db, sql, [id])?.into_iter().next().map(Value::Object).unwrap_or(Value::Null),
            None => Value::Null,
        })
    };
    match table {
        "List_UpgradeEngineCamshaft" => {
            if let Some(tc) = id("TorqueCurveFullThrottleID") {
                if let Some(row) = crate::cars::rows(db, "SELECT * FROM List_TorqueCurve WHERE TorqueCurveID=?1", [tc])?.into_iter().next() {
                    let n = row["NumTorqueValues"].as_i64().unwrap_or(0) as usize;
                    let curve = json!({"id": tc, "torque_scale_nm": row["TorqueScale"], "zero_throttle_nm": row["ZeroThrottleTorqueScale"], "max_rpm": r["TorqueCurveMaxRPM"], "samples": crate::cars::samples(&row, n)});
                    r.insert("_torque_curve".into(), curve);
                }
            }
        }
        "List_UpgradeSpringDamper" => {
            let sql = "SELECT * FROM List_SpringDamperPhysics WHERE SpringDamperPhysicsID=?1";
            let (front, rear) = (one(sql, id("FrontSpringDamperPhysicsID"))?, one(sql, id("RearSpringDamperPhysicsID"))?);
            r.insert("_front".into(), front);
            r.insert("_rear".into(), rear);
        }
        "List_UpgradeCarBodyFrontBumper" | "List_UpgradeRearWing" => {
            let v = one("SELECT * FROM List_AeroPhysics WHERE AeroPhysicsID=?1", id("AeroPhysicsID"))?;
            r.insert("_aero".into(), v);
        }
        "List_UpgradeAntiSwayFront" | "List_UpgradeAntiSwayRear" => {
            let v = one("SELECT * FROM List_AntiSwayPhysics WHERE AntiSwayPhysicsID=?1", id("AntiSwayPhysicsID"))?;
            r.insert("_physics".into(), v);
        }
        "List_UpgradeTireCompound" => {
            let compound = one("SELECT * FROM List_TireCompound WHERE TireCompoundID=?1", id("TireCompoundID"))?;
            if let Some(c) = compound.as_object() {
                // As the cars group's physics.json `tires`.
                let mut affect = Map::new();
                for (k, v) in c {
                    if let (Some(name), Some(cid)) = (k.strip_prefix("AffectCurve").and_then(|n| n.strip_suffix("ID")), v.as_i64()) {
                        affect.insert(name.to_owned(), crate::cars::affect_curve(db, cid)?);
                    }
                }
                let tires = json!({
                    "compound": compound,
                    "friction_lateral": crate::cars::multi_curve(db, c.get("FrictionMultiCurveLateralID").and_then(Value::as_i64))?,
                    "friction_longitudinal": crate::cars::multi_curve(db, c.get("FrictionMultiCurveLongitudinalID").and_then(Value::as_i64))?,
                    "affect_curves": affect,
                });
                r.insert("_tires".into(), tires);
            }
        }
        _ => {}
    }
    Ok(())
}

enum Key {
    Car,
    Body,
    Engine,
    Drivetrain,
}

/// Every List_Upgrade* table with IsStock and its per-car key column (the cars group's rule: Ordinal wins).
fn upgrade_tables(db: &Connection) -> Result<Vec<(String, String, Key)>> {
    let names: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'List_Upgrade%' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for name in names {
        let cols: Vec<String> = db.prepare(&format!("PRAGMA table_info(\"{name}\")"))?.query_map([], |r| r.get(1))?.collect::<Result<_, _>>()?;
        if !cols.iter().any(|c| c == "IsStock") {
            continue;
        }
        let find = |k: &str| cols.iter().find(|c| c.eq_ignore_ascii_case(k)).cloned();
        let key = if let Some(c) = find("Ordinal") {
            (c, Key::Car)
        } else if let Some(c) = find("CarBodyID") {
            (c, Key::Body)
        } else if let Some(c) = find("EngineID") {
            (c, Key::Engine)
        } else if let Some(c) = find("DrivetrainID") {
            (c, Key::Drivetrain)
        } else {
            continue;
        };
        out.push((name, key.0, key.1));
    }
    Ok(out)
}

fn rows<P: rusqlite::Params>(db: &Connection, sql: &str, params: P) -> Result<Vec<Row>> {
    let mut stmt = db.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut q = stmt.query(params)?;
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
