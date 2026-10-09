//! `missions` group: Colorado's free-roam activities for the engine's missions module (fh1-engine missions.rs,
//! docs/MISSIONS.md): speed cameras, average-speed zones, Horizon Outposts (gas stations) with their three missions
//! (speed stunt, PR stunt, photo shoot), barn finds and the race-encounter settings.
//!
//! Sources (EU disc, VERIFIED file names): `media/gamemodes.zip` `Colorado/{speed_camera, average_speed, gas_stations,
//! mission_speedstunts, mission_prstunts, mission_photoshoots, barnfinds, race_encounters}.xml`;
//! `media/tracks/colorado/Ribbon_00/GameObjs.xml` (`GameplayID` positions: speed camera posts, average-speed gates,
//! `GASSTATION_NNN`, `OUTPOST_NNN_NODE`, `BARNFIND_*`); `Ribbon_00/TrackRoute<route_id>.xml` NamedTransforms (each mission's
//! `CLoadRoute route_id`: `mission_start_NN`, `mission_photo_end_NN`, `mission_photo_pose_NN`, `mission_photo_01_NN`);
//! the PR-stunt arenas `TrackRoute086..095` (`mission_prstunt_entrance_l/r_01`, `mission_prstunt_end_NN`); gamedb
//! `Data_Car` (carID -> MediaName); the EN string tables (mission texts, car names).
//!
//! INFERRED (docs/MISSIONS.md): every PR stunt names the same `entrance_l_01` / `entrance_r_01`, so the arena per stunt
//! is not in its XML; it is assigned here as the unique arena nearest to the stunt's `mission_start` (greedy by
//! distance over TrackRoute086..095). `CSetSatNavDestinationPos` values do not match any arena and are ignored.
//!
//! Output: `missions/colorado.json` (engine space: Z negated; yaw 0 = facing -Z).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use fh1_formats::props;
use fh1_formats::zip::Archive;

/// String files tried for an `IDS_*` id (which .str holds each is not always known).
/// `List_SpeedCameras` (P18) holds every `IDS_SpeedCamera_NNN` name: 2NN = "Gladstone Speed Trap", 1NN = the average zones
/// ("I-76 Speed Zone"); the first version left all 41 labels as raw ids.
const STRING_FILES: [&str; 11] = ["Missions", "Barnfind", "Activities", "InGame", "Map", "GameStrings", "HelpButtons", "Main", "RaceCentral", "PostRaceFlow", "List_SpeedCameras"];

/// Engine-space point + yaw (yaw 0 = facing -Z).
#[derive(Clone, Copy, Debug)]
struct Xf {
    pos: [f32; 3],
    yaw: f32,
}

fn xf_json(x: &Xf) -> Value {
    json!({"pos": x.pos, "yaw": x.yaw})
}

/// `TrackRoute` NamedTransforms (name -> engine pose).
fn route_named(ribbon: &Path, route: i64) -> BTreeMap<String, Xf> {
    let mut out = BTreeMap::new();
    let Ok(xml) = std::fs::read_to_string(ribbon.join(format!("TrackRoute{route:03}.xml"))) else { return out };
    let Ok(doc) = crate::xml::to_json(&xml) else { return out };
    for n in as_list(&doc["TrackRoute"]["NamedTransforms"]["NamedTransform"]) {
        let Some(name) = n["name"].as_str() else { continue };
        let t = as_list(&n["Transform"]).into_iter().next().unwrap_or(Value::Null);
        let g = |k: &str| num(&t[k]) as f32;
        let (fx, fz) = (g("facing.x"), -g("facing.z"));
        let l = (fx * fx + fz * fz).sqrt().max(1e-6);
        let (dx, dz) = (fx / l, fz / l);
        out.insert(name.to_owned(), Xf { pos: [g("pos.x"), g("pos.y"), -g("pos.z")], yaw: (-dx).atan2(-dz) });
    }
    out
}

fn as_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        v => vec![v.clone()],
    }
}

fn num(v: &Value) -> f64 {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).unwrap_or(0.0)
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// `easy/medium/hard` attributes as [e, m, h].
fn emh(v: &Value) -> [f64; 3] {
    [num(&v["easy"]), num(&v["medium"]), num(&v["hard"])]
}

/// Every `Behaviour` (any depth) of an activity: (id, element).
fn behaviours(v: &Value, out: &mut Vec<(String, Value)>) {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                if k == "Behaviour" {
                    for b in as_list(c) {
                        out.push((s(&b["id"]), b.clone()));
                        behaviours(&b, out);
                    }
                } else {
                    behaviours(c, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| behaviours(c, out)),
        _ => {}
    }
}

/// `CLoadRoute` route_id (Attribute id="route_id" value=N).
fn route_id(act: &Value) -> Option<i64> {
    let mut bs = Vec::new();
    behaviours(act, &mut bs);
    bs.iter().filter(|(id, _)| id == "CLoadRoute").find_map(|(_, b)| as_list(&b["Attribute"]).iter().find(|a| s(&a["id"]) == "route_id").map(|a| num(&a["value"]) as i64))
}

fn first_cutscene(act: &Value, prefix: &str) -> Option<String> {
    let mut bs = Vec::new();
    behaviours(act, &mut bs);
    bs.iter().filter(|(id, _)| id == "CPrepareCutscene").map(|(_, b)| s(&b["cutscene"])).find(|c| c.starts_with(prefix))
}

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let track = disc.join("media/tracks/colorado");
    let ribbon = track.join("Ribbon_00");
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let strings = fh1_ui::strtable::StringTables::load_language(disc, "EN").context("EN string tables")?;
    let text = |id: &str| -> String {
        if id.is_empty() {
            return String::new();
        }
        STRING_FILES.iter().find_map(|f| strings.get(f, id)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| id.to_owned())
    };
    let cars: HashMap<i64, (String, Option<String>)> = db
        .prepare("SELECT Id, MediaName, DisplayName FROM Data_Car")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, (r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))))?
        .filter_map(|r| r.ok())
        .collect();
    let car_media = |id: i64| cars.get(&id).map(|c| c.0.clone());
    let car_name = |id: i64| {
        cars.get(&id).and_then(|c| c.1.as_deref()).map(|n| strings.resolve(n).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| n.to_owned())).filter(|n| !n.starts_with("_&"))
    };

    // GameObjs GameplayID -> engine pose.
    let mut objs: HashMap<String, Xf> = HashMap::new();
    for o in props::parse_obj_xml(&std::fs::read_to_string(ribbon.join("GameObjs.xml"))?) {
        let yaw = (-o.z_axis[0]).atan2(o.z_axis[2]);
        objs.entry(o.kind.clone()).or_insert(Xf { pos: [o.position[0], o.position[1], -o.position[2]], yaw });
    }

    // Activity XMLs.
    let mut ar = Archive::open(disc.join("media/gamemodes.zip"))?;
    let mut acts: HashMap<String, Vec<Value>> = HashMap::new();
    for e in ar.entries.clone() {
        let name = e.name.replace('\\', "/");
        let Some(file) = name.strip_prefix("Colorado/").filter(|f| !f.contains('/') && f.ends_with(".xml")) else { continue };
        let stem = file.trim_end_matches(".xml").to_ascii_lowercase();
        if !["speed_camera", "average_speed", "gas_stations", "mission_speedstunts", "mission_prstunts", "mission_photoshoots", "barnfinds", "race_encounters"].contains(&stem.as_str()) {
            continue;
        }
        let xml = String::from_utf8_lossy(&ar.read(&e)?).into_owned();
        let doc = crate::xml::to_json(&xml).with_context(|| format!("Colorado/{file}"))?;
        acts.insert(stem, as_list(&doc["ActivityManager"]["Activity"]));
    }
    let list = |k: &str| acts.get(k).cloned().unwrap_or_default();
    let mut warnings: Vec<String> = Vec::new();

    // Speed cameras: posts `speed_camera_NN_left/right`.
    let mut cams = Vec::new();
    for a in list("speed_camera") {
        let name = s(&a["name"]);
        let (Some(l), Some(r)) = (objs.get(&format!("{name}_left")), objs.get(&format!("{name}_right"))) else {
            warnings.push(format!("{name}: posts not in GameObjs"));
            continue;
        };
        let sb = num(&a["scoreboard_id"]) as i64;
        cams.push(json!({"name": name, "scoreboard_id": sb, "min_mph": num(&a["SpeedThreshold"]["minimumSpeed"]),
            "left": l.pos, "right": r.pos, "label": text(&format!("IDS_SpeedCamera_{sb}"))}));
    }

    // Average-speed zones: two gate pairs (left1/right1 start, left2/right2 end).
    let mut avgs = Vec::new();
    for a in list("average_speed") {
        let name = s(&a["name"]);
        let g = |k: &str| objs.get(&format!("{name}_{k}")).map(|x| x.pos);
        let (Some(l1), Some(r1), Some(l2), Some(r2)) = (g("left1"), g("right1"), g("left2"), g("right2")) else {
            warnings.push(format!("{name}: gates not in GameObjs"));
            continue;
        };
        let sb = num(&a["scoreboard_id"]) as i64;
        avgs.push(json!({"name": name, "scoreboard_id": sb, "min_mph": num(&a["SpeedThreshold"]["minimumSpeed"]),
            "start": [l1, r1], "end": [l2, r2], "label": text(&format!("IDS_SpeedCamera_{sb}"))}));
    }

    // Outposts (gas stations) and their missions.
    let mut outposts = Vec::new();
    for a in list("gas_stations") {
        let name = s(&a["name"]);
        let tz = as_list(&a["TriggerZone"]).into_iter().next().unwrap_or(Value::Null);
        let object = s(&tz["object"]);
        let Some(at) = objs.get(&object) else {
            warnings.push(format!("{name}: {object} not in GameObjs"));
            continue;
        };
        let idx = object.trim_start_matches("GASSTATION_").to_owned();
        let node = objs.get(&format!("OUTPOST_{idx}_NODE")).copied().unwrap_or(*at);
        let missions: Vec<String> = as_list(&a["Missions"]["Mission"]).iter().map(|m| s(&m["name"])).collect();
        outposts.push(json!({"name": name, "object": object, "pos": at.pos, "radius": num(&tz["radius"]), "max_mph": num(&tz["maxMPH"]),
            "discover_radius": num(&tz["triggerZoneRadius"]), "title": text(&s(&tz["prompt"])), "place": xf_json(&node), "missions": missions}));
    }

    // The mission's start pose: `mission_start_NN` of its route.
    let start_of = |route: Option<i64>| route.map(|r| route_named(&ribbon, r)).and_then(|n| n.iter().find(|(k, _)| k.starts_with("mission_start")).map(|(_, v)| *v));
    let car_of = |a: &Value| {
        let id = num(&a["Car"]["carID"]) as i64;
        json!({"id": id, "media": car_media(id), "name": car_name(id), "colour": num(&a["Car"]["carColour"]) as i64})
    };

    // Speed stunts.
    let mut speed = Vec::new();
    for a in list("mission_speedstunts") {
        let name = s(&a["name"]);
        let route = route_id(&a);
        let trap = s(&a["SpeedTrap"]["name"]);
        speed.push(json!({"name": name, "route_id": route, "start": start_of(route).as_ref().map(xf_json), "trap": trap,
            "trap_label": text(&s(&a["SpeedTrap"]["nameDesc"])), "speed_mph": emh(&a["Speed"]), "time_s": emh(&a["Time"]), "car": car_of(&a),
            "instruction": text(&s(&a["Descriptions"]["instruction_text"]))}));
    }

    // PR stunts: arena per stunt (module doc).
    let arenas: Vec<(i64, BTreeMap<String, Xf>)> = (86..=95).map(|t| (t, route_named(&ribbon, t))).filter(|(_, n)| n.contains_key("mission_prstunt_entrance_l_01")).collect();
    let pr_acts = list("mission_prstunts");
    let starts: Vec<Option<Xf>> = pr_acts.iter().map(|a| start_of(route_id(a))).collect();
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (i, st) in starts.iter().enumerate() {
        let Some(st) = st else { continue };
        for (j, (_, n)) in arenas.iter().enumerate() {
            let e = n["mission_prstunt_entrance_l_01"].pos;
            pairs.push((((e[0] - st.pos[0]).powi(2) + (e[2] - st.pos[2]).powi(2)).sqrt(), i, j));
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut arena_of: HashMap<usize, usize> = HashMap::new();
    let mut taken = std::collections::HashSet::new();
    for (_, i, j) in pairs {
        if !arena_of.contains_key(&i) && taken.insert(j) {
            arena_of.insert(i, j);
        }
    }
    let mut pr = Vec::new();
    for (i, a) in pr_acts.iter().enumerate() {
        let name = s(&a["name"]);
        let ez = &a["EntranceZone"];
        let arena = arena_of.get(&i).map(|&j| &arenas[j]);
        let (Some(start), Some((track_id, n))) = (starts[i], arena) else {
            warnings.push(format!("{name}: no start / arena"));
            continue;
        };
        let (Some(l), Some(r)) = (n.get(&s(&ez["left_node_name"])), n.get(&s(&ez["right_node_name"]))) else {
            warnings.push(format!("{name}: entrance nodes missing in TrackRoute{track_id:03}"));
            continue;
        };
        let path: Vec<[f32; 3]> = n.iter().filter(|(k, _)| k.starts_with("mission_prstunt_end_")).map(|(_, v)| v.pos).collect();
        pr.push(json!({"name": name, "route_id": route_id(a), "arena_route": track_id, "start": xf_json(&start), "entrance": [l.pos, r.pos],
            "path": path, "target": emh(&a["Popularity"]), "time_s": emh(&a["TimeLimit"]), "car": car_of(a),
            "instruction_to": text(&s(&a["Descriptions"]["instruction_to_skills"])), "instruction_in": text(&s(&a["Descriptions"]["instruction_in_skills"]))}));
    }

    // Photo shoots.
    let mut photo = Vec::new();
    for a in list("mission_photoshoots") {
        let name = s(&a["name"]);
        let route = route_id(&a);
        let Some(r) = route else {
            warnings.push(format!("{name}: no route"));
            continue;
        };
        let n = route_named(&ribbon, r);
        let zones: Vec<Value> = as_list(&a["PhotoZone"])
            .iter()
            .filter_map(|z| {
                let x = n.get(&s(&z["node_name"]))?;
                Some(json!({"pos": x.pos, "radius": num(&z["radius"]), "max_mph": z.get("maxMPH").map(num)}))
            })
            .collect();
        let nodes_prefix = s(&a["PhotoNodes"]["node_name"]);
        let nodes: Vec<[f32; 3]> = n.iter().filter(|(k, _)| k.strip_prefix(&nodes_prefix).is_some_and(|r| r.starts_with('_'))).map(|(_, v)| v.pos).collect();
        let d = &a["Descriptions"];
        let col = &a["Collisions"];
        photo.push(json!({"name": name, "route_id": r, "start": n.iter().find(|(k, _)| k.starts_with("mission_start")).map(|(_, v)| xf_json(v)),
            "zones": zones, "pose": n.get(&s(&a["Pose"]["node_name"])).map(xf_json), "nodes": nodes, "min_in_shot": num(&a["PhotoNodes"]["min_in_shot"]),
            "damage": emh(&a["Damage"]), "low_speed_collision_mph": num(&col["low_speed_collision_mph"]),
            "damage_low_speed": num(&col["damage_from_low_speed_collisions"]), "damage_smashable": num(&col["damage_from_break_smashable"]),
            "car": car_of(&a), "requirements": text(&s(&d["requirements_description"])), "instruction_to": text(&s(&d["instruction_to_photo"])),
            "instruction_in": text(&s(&d["instruction_in_photo"])), "location": text(&s(&d["instruction_location"])),
            "intro_cutscene": first_cutscene(&a, "mission_intro")}));
    }

    // Barn finds (the controller activity has no TriggerZone and is skipped).
    let mut barns = Vec::new();
    for a in list("barnfinds") {
        let name = s(&a["name"]);
        let Some(tz) = as_list(&a["TriggerZone"]).into_iter().next() else { continue };
        let object = s(&tz["object"]);
        let Some(at) = objs.get(&object) else {
            warnings.push(format!("{name}: {object} not in GameObjs"));
            continue;
        };
        let id = num(&a["UnlockCar"]["id"]) as i64;
        let si = &a["SpawnInfo"];
        let hr = &a["HintRegion"];
        let dd = &a["DistanceDriven"];
        let di = &a["DoorInfo"];
        let hint_vo = as_list(&a["RadioDJRequests"]["Special"]).iter().map(|x| s(&x["gameplayEvent"])).find(|e| e.starts_with("BarnFindLocationHint"));
        barns.push(json!({"name": name, "object": object, "pos": at.pos, "radius": num(&tz["radius"]), "max_mph": num(&tz["maxMPH"]),
            "car": {"id": id, "media": car_media(id), "name": text(&s(&a["UnlockCar"]["stringTableNameId"]))},
            "spawn": {"weighting": num(&si["weighting"]), "min_probability": num(&si["min_probability"]), "min_distance": num(&si["min_distance"]),
                      "max_probability": num(&si["max_probability"]), "max_distance": num(&si["max_distance"])},
            "hint": {"x_offset": num(&hr["x_offset"]), "y_offset": num(&hr["y_offset"]), "angle": num(&hr["angle"]), "x_radius": num(&hr["x_radius"]), "y_radius": num(&hr["y_radius"])},
            "distance_miles": {"min": num(&dd["distanceMin"]), "max": num(&dd["distanceMax"]), "threshold": num(&dd["threshold"])},
            "doors": {"open": s(&di["open_id"]), "closed": s(&di["closed_id"]), "open_coll": s(&di["open_coll_id"]), "closed_coll": s(&di["closed_coll_id"])},
            "discover_cutscene": first_cutscene(&a, "BARNFIND_"), "hint_vo": hint_vo}));
    }

    // Race encounters.
    let enc = list("race_encounters").into_iter().next().unwrap_or(Value::Null);
    let wm = &enc["Settings"]["WristbandMultipliers"];
    let count = num(&wm["count"]) as usize;
    let mults: Vec<f64> = (0..count).map(|i| num(&wm[format!("Wristband{i}")])).collect();
    let encounter = json!({"map_tag": s(&enc["Settings"]["mapTag"]), "description": text(&s(&enc["Settings"]["mapDescription"])), "wristband_multipliers": mults});

    // UI texts the engine shows (EN).
    let ids = [
        "IDS_SpeedStuntFailTimeout", "IDS_SpeedStuntFailSpeed", "IDS_PhotoShootFail", "IDS_PRStunt_01", "IDS_PhotoShoot_01", "IDS_GasStation_Missions",
        "IDS_DiscoverGasStation_Title", "IDS_DiscoverGasStation_Msg1", "IDS_QuitPhotoModeDesc", "IDS_Spawned_Body", "IDS_Ready_Message", "IDS_Title",
        "IDS_Collect_Popup_Message", "IDS_CarState_Collected", "IDS_CarState_Ready", "IDS_CarState_Wrecked", "IDS_CarState_Locked", "IDS_Trade_BarnFind_Message",
        "IDS_Description_BarnFind", "IDS_Description_GasStation", "IDS_SpeedStunt_01", "IDS_Description_RaceEncounter_Finish",
    ];
    let texts: serde_json::Map<String, Value> = ids.iter().map(|id| ((*id).to_owned(), Value::String(text(id)))).collect();

    for w in &warnings {
        println!("[missions] {w}");
    }
    println!(
        "[missions] {} speed cameras, {} average zones, {} outposts, {} speed / {} PR / {} photo missions, {} barn finds",
        cams.len(),
        avgs.len(),
        outposts.len(),
        speed.len(),
        pr.len(),
        photo.len(),
        barns.len()
    );
    let dir = out.join("colorado");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("missions.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "speed_cameras": cams,
            "average_speed": avgs,
            "outposts": outposts,
            "speed_stunts": speed,
            "pr_stunts": pr,
            "photo_shoots": photo,
            "barn_finds": barns,
            "race_encounter": encounter,
            "texts": texts,
        }))?,
    )?;
    Ok(())
}
