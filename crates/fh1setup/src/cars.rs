//! `cars` group: per car, `physics.json` (gamedb.slt + MAXData.xml) and the model
//! (`model.gltf`, see [`crate::model`]).
//!
//! Rows keep their original gamedb column names: we don't know every column's meaning yet, so
//! nothing is renamed or dropped. Only references are resolved (stock parts, curves).
//! Known units: `List_UpgradeCarBodyWeight.Mass` kg, torque curve samples are normalised and
//! scaled by `TorqueScale` (N·m) over 0..=`TorqueCurveMaxRPM`; tire curve loads are in kgf.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

use fh1_formats::zip::Archive;

use crate::model;

type Row = Map<String, Value>;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let db = Connection::open_with_flags(
        disc.join("media/db/gamedb.slt"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let upgrade_tables = upgrade_tables(&db)?;
    let mut cars = rows(&db, "SELECT * FROM Data_Car ORDER BY Id", [])?;
    // FH1_CARS=A,B,...: convert only these cars (development runs into a scratch --data dir).
    if let Ok(only) = std::env::var("FH1_CARS") {
        let only: Vec<&str> = only.split(',').map(str::trim).collect();
        cars.retain(|c| c["MediaName"].as_str().is_some_and(|m| only.iter().any(|o| o.eq_ignore_ascii_case(m))));
    }

    let mut index = Vec::new();
    let mut fx_unparsed = 0;
    // Shared.zip textures some car materials sample (textures.xml): tyre (`tireA0`; tireA1-3 are its
    // motion-blur versions), grilles (the `_alpha` versions, as Grille1/2Sampler load), bumper frame,
    // undercarriage, carbon fibre. Written next to every model.
    let mut shared = Vec::new();
    for (key, file) in [("tire", "tireA0.xds"), ("grille1", "grille1_alpha.xds"), ("grille2", "grille2_alpha.xds"), ("bumper_frame", "bumper_frame.xds"), ("undercarriage", "undercarriage.xds"), ("carbon", "carbonFiber.xds")] {
        match shared_texture(disc, file) {
            Ok(Some(t)) => shared.push((key, format!("{key}.png"), t)),
            Ok(None) => println!("[cars] Shared.zip has no {file}"),
            Err(e) => println!("[cars] {file}: {e:#}"),
        }
    }
    for car in &cars {
        let media = car["MediaName"].as_str().context("MediaName")?.to_owned();
        let id = car["Id"].as_i64().context("Id")?;
        // FH1 has every car's zip; imported games may list cars that only exist as showroom models (FM4's
        // Autovista-only BUN_Warthog_10 / BEN_8Litre_31 have just `_SLOD` folders).
        if !disc.join("media/cars").join(format!("{media}.zip")).exists() {
            println!("[cars] {media}: no media/cars/{media}.zip, skipped");
            continue;
        }
        let physics = car_physics(&db, &upgrade_tables, car, disc)
            .with_context(|| format!("car {media}"))?;
        let dir = out.join(&media);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("physics.json"), serde_json::to_vec_pretty(&physics)?)?;

        // Stock paint: the car's lowest colour sequence (its BaseTexture is nodamage).
        let paint = rows(&db, "SELECT RGB, Metallic, Sequence FROM Combo_Colors WHERE Ordinal=?1 ORDER BY Sequence LIMIT 1", [id])?
            .into_iter()
            .next()
            .map(|r| model::Paint {
                rgb: r["RGB"].as_i64().unwrap_or(0x808080) as u32,
                metallic: r["Metallic"].as_i64() == Some(1),
                sequence: r["Sequence"].as_i64().unwrap_or(0) as u32,
            })
            .unwrap_or(model::Paint { rgb: 0x808080, metallic: false, sequence: 0 });
        let zip = disc.join("media/cars").join(format!("{media}.zip"));
        let kit = stock_kit(&db, id, physics["body"]["Id"].as_i64())?;
        // Hub height above the model origin (bottom of the body) = tyre radius - ride height.
        let num = |k: &str| car[k].as_f64().unwrap_or(0.0) as f32;
        let tyre = |w: &str, a: &str, d: &str| num(d) * 0.0254 * 0.5 + num(w) * 0.001 * num(a) * 0.01;
        let hub_height = [
            tyre("FrontTireWidthMM", "FrontTireAspect", "FrontWheelDiameterIN") - num("FrontStockRideHeight"),
            tyre("RearTireWidthMM", "RearTireAspect", "RearWheelDiameterIN") - num("RearStockRideHeight"),
        ];
        let axle = |w: &str, a: &str, d: &str| model::Axle {
            tyre_radius: tyre(w, a, d),
            tyre_width: num(w) * 0.001,
            rim_radius: num(d) * 0.0254 * 0.5,
        };
        let rim_media = physics["wheel"]["MediaName"].as_str().unwrap_or(&media).to_owned();
        let rim_zip = disc.join("media/wheels").join(format!("{rim_media}.zip"));
        let wheels = model::Wheels {
            rim_zip: rim_zip.exists().then_some(rim_zip.as_path()),
            rim_media: &rim_media,
            axles: [
                axle("FrontTireWidthMM", "FrontTireAspect", "FrontWheelDiameterIN"),
                axle("RearTireWidthMM", "RearTireAspect", "RearWheelDiameterIN"),
            ],
            centre: ["BottomCenterWheelbasePosx", "BottomCenterWheelbasePosy", "BottomCenterWheelbasePosZ"]
                .map(|k| physics["body"][k].as_f64().unwrap_or(0.0) as f32),
        };
        for (_, png, (w, h, rgba)) in &shared {
            model::write_png(&dir.join(png), *w, *h, rgba)?;
        }
        let shared_pngs: Vec<(&'static str, String)> = shared.iter().map(|(k, png, _)| (*k, png.clone())).collect();
        let has_model = match model::export(&zip, &media, &paint, &kit, &physics["maxdata"], hub_height, &wheels, &shared_pngs, &dir) {
            Ok(_) => true,
            Err(e) => {
                println!("[cars] {media}: no model ({e:#})");
                false
            }
        };
        // Raw streams + shader settings for the game's car shaders.
        if zip.exists() {
            let (_, bad) = crate::carfx::export_car(&zip, &dir)?;
            fx_unparsed += bad;
            crate::cartex::build_car(&zip, &dir)?;
            if has_model {
                crate::cockpit::build_car(&zip, &media, &paint, &dir)?;
            }
        }
        index.push(json!({
            "id": id,
            "media_name": media,
            "year": car["Year"],
            "display_name": car["DisplayName"],
            "make_id": car["MakeID"],
            "class_id": car["ClassID"],
            "drive_type_id": car["DriveTypeID"],
            "performance_index": car["PerformanceIndex"],
            "has_model": has_model,
        }));
    }
    std::fs::write(out.join("index.json"), serde_json::to_vec_pretty(&index)?)?;
    crate::carfx::export_shared(disc, out)?;
    crate::cartex::build_shared(disc, out)?;
    println!("[cars] {} cars ({fx_unparsed} car carbins didn't parse for fx)", index.len());
    Ok(())
}

/// Which id a `List_Upgrade*` table is keyed by.
#[derive(Clone, Copy)]
enum Key {
    Car,
    Body,
    Engine,
    Drivetrain,
}

fn upgrade_tables(db: &Connection) -> Result<Vec<(String, String, Key)>> {
    let names: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'List_Upgrade%' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for name in names {
        let cols: Vec<String> = db
            .prepare(&format!("PRAGMA table_info(\"{name}\")"))?
            .query_map([], |r| r.get(1))?
            .collect::<Result<_, _>>()?;
        if !cols.iter().any(|c| c == "IsStock") {
            continue;
        }
        let find = |k: &str| cols.iter().find(|c| c.eq_ignore_ascii_case(k)).cloned();
        // A table with an Ordinal column is per car even if it also names a part id.
        let key = if let Some(c) = find("Ordinal") {
            (c, Key::Car)
        } else if let Some(c) = find("CarBodyID") {
            (c, Key::Body)
        } else if let Some(c) = find("EngineID") {
            (c, Key::Engine)
        } else if let Some(c) = find("DrivetrainID") {
            (c, Key::Drivetrain)
        } else {
            continue; // electric motor tables (empty in FH1)
        };
        out.push((name, key.0, key.1));
    }
    Ok(out)
}

fn car_physics(
    db: &Connection,
    tables: &[(String, String, Key)],
    car: &Row,
    disc: &Path,
) -> Result<Value> {
    let car_id = car["Id"].as_i64().unwrap();
    let stock = |table: &str, col: &str, id: i64| -> Result<Option<Row>> {
        Ok(rows(
            db,
            &format!("SELECT * FROM \"{table}\" WHERE \"{col}\"=?1 AND IsStock=1"),
            [id],
        )?
        .into_iter()
        .next())
    };
    let int = |row: &Option<Row>, col: &str| row.as_ref().and_then(|r| r.get(col)?.as_i64());

    let body_id = int(&stock("List_UpgradeCarBody", "Ordinal", car_id)?, "CarBodyID");
    let engine_id = int(&stock("List_UpgradeEngine", "Ordinal", car_id)?, "EngineID");
    let drivetrain_id = int(&stock("List_UpgradeDrivetrain", "Ordinal", car_id)?, "DrivetrainID");

    let mut parts = Map::new();
    for (table, col, key) in tables {
        let id = match key {
            Key::Car => Some(car_id),
            Key::Body => body_id,
            Key::Engine => engine_id,
            Key::Drivetrain => drivetrain_id,
        };
        if let Some(row) = id.map(|id| stock(table, col, id)).transpose()?.flatten() {
            parts.insert(table.clone(), Value::Object(row));
        }
    }
    let part = |t: &str, c: &str| parts.get(t).and_then(|r| r.get(c)).and_then(Value::as_i64);

    let one = |sql: &str, id: Option<i64>| -> Result<Value> {
        Ok(match id {
            Some(id) => rows(db, sql, [id])?.into_iter().next().map(Value::Object).unwrap_or(Value::Null),
            None => Value::Null,
        })
    };

    // Engine torque curve: normalised samples over 0..=TorqueCurveMaxRPM.
    let cam = parts.get("List_UpgradeEngineCamshaft");
    let torque_curve = match cam.and_then(|c| c.get("TorqueCurveFullThrottleID")?.as_i64()) {
        Some(tc) => {
            let row = rows(db, "SELECT * FROM List_TorqueCurve WHERE TorqueCurveID=?1", [tc])?
                .into_iter()
                .next()
                .context("torque curve")?;
            let n = row["NumTorqueValues"].as_i64().unwrap_or(0) as usize;
            json!({
                "id": tc,
                "torque_scale_nm": row["TorqueScale"],
                // Engine drag scale at zero throttle (N·m; 82D22D58, docs/DRIVETRAIN.md "Engine torque").
                "zero_throttle_nm": row["ZeroThrottleTorqueScale"],
                "max_rpm": cam.unwrap()["TorqueCurveMaxRPM"],
                "samples": samples(&row, n),
            })
        }
        None => Value::Null,
    };

    let spring = |col: &str| one("SELECT * FROM List_SpringDamperPhysics WHERE SpringDamperPhysicsID=?1", part("List_UpgradeSpringDamper", col));
    let sway = |t: &str| one("SELECT * FROM List_AntiSwayPhysics WHERE AntiSwayPhysicsID=?1", part(t, "AntiSwayPhysicsID"));
    let aero = |t: &str| one("SELECT * FROM List_AeroPhysics WHERE AeroPhysicsID=?1", part(t, "AeroPhysicsID"));

    let compound_id = part("List_UpgradeTireCompound", "TireCompoundID");
    let compound = one("SELECT * FROM List_TireCompound WHERE TireCompoundID=?1", compound_id)?;
    let tires = match compound.as_object() {
        Some(c) => {
            let mut affect = Map::new();
            for (k, v) in c {
                if let (Some(name), Some(id)) = (k.strip_prefix("AffectCurve").and_then(|n| n.strip_suffix("ID")), v.as_i64()) {
                    affect.insert(name.to_owned(), affect_curve(db, id)?);
                }
            }
            json!({
                "compound": compound,
                "friction_lateral": multi_curve(db, c.get("FrictionMultiCurveLateralID").and_then(Value::as_i64))?,
                "friction_longitudinal": multi_curve(db, c.get("FrictionMultiCurveLongitudinalID").and_then(Value::as_i64))?,
                "affect_curves": affect,
            })
        }
        None => Value::Null,
    };

    let maxdata = read_maxdata(disc, car["MediaName"].as_str().unwrap())?;

    Ok(json!({
        "id": car_id,
        "media_name": car["MediaName"],
        "car": car,
        "body": one("SELECT * FROM Data_CarBody WHERE Id=?1", body_id)?,
        "engine": one("SELECT * FROM Data_Engine WHERE EngineID=?1", engine_id)?,
        "drivetrain": one("SELECT * FROM Data_Drivetrain WHERE DrivetrainID=?1", drivetrain_id)?,
        "torque_curve": torque_curve,
        "suspension": {
            "front": spring("FrontSpringDamperPhysicsID")?,
            "rear": spring("RearSpringDamperPhysicsID")?,
            "anti_sway_front": sway("List_UpgradeAntiSwayFront")?,
            "anti_sway_rear": sway("List_UpgradeAntiSwayRear")?,
        },
        "aero": {
            "front_bumper": aero("List_UpgradeCarBodyFrontBumper")?,
            "rear_wing": aero("List_UpgradeRearWing")?,
        },
        "tires": tires,
        "wheel": one("SELECT * FROM List_Wheels WHERE ID=?1", car["StockWheelID"].as_i64())?,
        "camera": one("SELECT * FROM CameraOverrides WHERE CarId=?1", Some(car_id))?,
        // Every Combo_Colors row of the car (Sequence order), for a non-stock paint (fh1-render car.rs FxCarPaint).
        "colors": rows(db, "SELECT Sequence, RGB, Metallic FROM Combo_Colors WHERE Ordinal=?1 ORDER BY Sequence", [car_id])?,
        "stock_parts": parts,
        "maxdata": maxdata,
    }))
}

pub(crate) fn multi_curve(db: &Connection, id: Option<i64>) -> Result<Value> {
    let Some(id) = id else { return Ok(Value::Null) };
    let Some(mut m) = rows(db, "SELECT * FROM List_TireFrictionMultiCurve WHERE FrictionMultiCurveID=?1", [id])?.into_iter().next() else {
        return Ok(Value::Null);
    };
    // Curve 0 applies at MinLoadCurve, curve 1 at MaxLoadCurve (both kgf).
    let mut curves = Vec::new();
    for col in ["TireFrictionCurveID0", "TireFrictionCurveID1"] {
        let cid = m[col].as_i64().unwrap_or(-1);
        let c = rows(db, "SELECT * FROM List_TireFrictionCurve WHERE FrictionCurveID=?1", [cid])?
            .into_iter()
            .next()
            .context("friction curve")?;
        let n = c["NumCurveValues"].as_i64().unwrap_or(0) as usize;
        curves.push(json!({"id": cid, "friction_scale": c["FrictionScale"], "samples": samples(&c, n)}));
    }
    m.insert("curves".into(), Value::Array(curves));
    Ok(Value::Object(m))
}

pub(crate) fn affect_curve(db: &Connection, id: i64) -> Result<Value> {
    let Some(r) = rows(db, "SELECT * FROM List_TireAffectCurve WHERE AffectCurveID=?1", [id])?.into_iter().next() else {
        return Ok(Value::Null);
    };
    let n = r["NumValues"].as_i64().unwrap_or(0) as usize;
    Ok(json!({
        "id": id,
        "min_input": r["MinInput"],
        "max_input": r["MaxInput"],
        "output_scale": r["OutputScale"],
        "samples": samples(&r, n),
    }))
}

/// Stock body-kit letters from gamedb (see [`model::Kit`]). FH1_KIT_SEQ=0 = always `a` (the old rule).
pub(crate) fn stock_kit(db: &Connection, car_id: i64, body_id: Option<i64>) -> Result<model::Kit> {
    let mut kit = model::Kit::default();
    if std::env::var("FH1_KIT_SEQ").as_deref() == Ok("0") {
        return Ok(kit);
    }
    let tables: [(&str, &str, Option<i64>, &[&'static str]); 5] = [
        ("List_UpgradeCarBodyFrontBumper", "CarBodyID", body_id, &["bumperf"]),
        ("List_UpgradeCarBodyRearBumper", "CarBodyID", body_id, &["bumperr"]),
        ("List_UpgradeCarBodyHood", "CarBodyID", body_id, &["hood"]),
        ("List_UpgradeCarBodySideSkirt", "CarBodyID", body_id, &["skirtl", "skirtr"]),
        ("List_UpgradeRearWing", "Ordinal", Some(car_id), &["wing"]),
    ];
    for (table, col, id, stems) in tables {
        let Some(id) = id else { continue };
        let seq = rows(db, &format!("SELECT Sequence FROM \"{table}\" WHERE \"{col}\"=?1 AND IsStock=1"), [id])?
            .first()
            .and_then(|r| r["Sequence"].as_i64())
            .unwrap_or(0);
        if (1..26).contains(&seq) {
            for stem in stems {
                kit.0.push((stem, (b'a' + seq as u8) as char));
            }
        }
    }
    Ok(kit)
}

/// Columns v0..v{n-1}.
pub(crate) fn samples(row: &Row, n: usize) -> Vec<Value> {
    (0..n).map(|i| row.get(&format!("v{i}")).cloned().unwrap_or(Value::Null)).collect()
}

/// A `media/cars/Shared.zip` texture decoded to RGBA8.
fn shared_texture(disc: &Path, name: &str) -> Result<Option<(u32, u32, Vec<u8>)>> {
    let mut ar = Archive::open(&disc.join("media/cars/Shared.zip"))?;
    let Some(e) = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned() else { return Ok(None) };
    let (_, img) = fh1_formats::xds::decode_base(&ar.read(&e)?)?;
    Ok(Some((img.width, img.height, fh1_formats::xds::to_rgba8(&img)?)))
}

fn read_maxdata(disc: &Path, media: &str) -> Result<Value> {
    let zip = disc.join("media/cars").join(format!("{media}.zip"));
    let mut ar = Archive::open(&zip).with_context(|| zip.display().to_string())?;
    let Some(e) = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case("Physics/MAXData.xml")).cloned() else {
        return Ok(Value::Null);
    };
    let text = String::from_utf8_lossy(&ar.read(&e)?).into_owned();
    Ok(crate::xml::to_json(&text)?.get("MAXData").cloned().unwrap_or(Value::Null))
}

pub(crate) fn rows<P: rusqlite::Params>(db: &Connection, sql: &str, params: P) -> Result<Vec<Row>> {
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
                ValueRef::Blob(b) => Value::String(crate::extract::hex(b)),
            };
            m.insert(n.clone(), v);
        }
        out.push(m);
    }
    Ok(out)
}
