//! `fh1setup import-fm4 <FM4 root> [--data <dir>] [--only cars]`: Forza Motorsport 4 (Xbox 360) cars into the ACTIVE
//! install, through the same readers and converters as FH1 (FM4 is the same engine family, docs/FM4_RECON.md).
//!
//! `<FM4 root>` is a folder laid out like an FH1 disc (`media/db/gamedb.slt`, `media/cars/<CAR>.zip`, ...): the Play
//! Disc's `Media` plus the Content Install Disc's car packs, which ship as folders inside one zip per pack and are
//! repacked to one zip per car / rim. `tools/fm4/merge.py` builds it from the two extracted discs (STFS packages
//! included); the gamedb on the Play Disc already lists every pack car (503 rows).
//!
//! Output (git-ignored install data only):
//! - `imported/fm4/cars/<MediaName>/` exactly like `cars/<MediaName>/` (the cars group), plus FM4's shared folders.
//!   `physics.json` gets `imported_from: "fm4"`, FH1-only `Data_Car` columns FM4 lacks (defaulted, see
//!   [`FH1_ONLY_CAR_COLUMNS`]) and, when FH1's audio has no bank for the car, `sound_donor` (closest FH1 car).
//! - `imported/fm4/cars/index.json`: the import contract rows `{id, media_name, name, maker, year, class, pi, drive,
//!   has_model}` (shared with the other imported games; the engine addresses a car as `../imported/fm4/cars/<media>`).
//! - `imported/fm4/<track>_<NN>/` per circuit layout (`media/tracks/<Track>/Ribbon_NN`, gamedb `Tracks` row, not the
//!   reverse rows): `world/` (collision from the layout's `.col` + `spawns.json`), `scenery/` (scenery group format,
//!   no zones; hard links of the track's first layout, all layouts share `bin.zip`), `shaders/track/`,
//!   `tracks/TrackSettings.xml`, `ai/line.json` and `track.json`. See [`maps`].
//! - `imported/fm4/maps.json`: `[{id: "fm4/<track>_<NN>", name, track, layout, length_m, type}]`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::fh2::{swap, Donors};

/// FH1 `Data_Car` columns FM4's gamedb doesn't have, with the value used for FM4 cars: every FM4 car is a player car,
/// FM4 has no off-road scaling (1.0 = FH1's neutral value) and no camera-collision flags.
const FH1_ONLY_CAR_COLUMNS: &[(&str, f64)] = &[
    ("IsSelectable", 1.0),
    ("IsRentable", 0.0),
    ("Specials", 0.0),
    ("OffRoadEnginePowerScale", 1.0),
    ("OffRoadFrontWheelGripScale", 1.0),
    ("OffRoadRearWheelGripScale", 1.0),
    ("OffRoadTCSFullEffectMultiplier", 1.0),
    ("IsCameraCollidable", 0.0),
    ("UseBoxCameraCollision", 0.0),
];

pub fn run() -> Result<()> {
    let mut it = std::env::args().skip(2);
    let (mut source, mut data, mut only) = (None, PathBuf::from("data"), None::<Vec<String>>);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data" => data = it.next().context("--data needs a path")?.into(),
            "--only" => only = Some(it.next().context("--only needs cars,maps")?.split(',').map(str::to_owned).collect()),
            _ => source = Some(PathBuf::from(a)),
        }
    }
    let disc = source.context("usage: fh1setup import-fm4 <FM4 root (tools/fm4/merge.py)> [--data <dir>] [--only cars]")?;
    anyhow::ensure!(disc.join("media/db/gamedb.slt").exists(), "{}: no media/db/gamedb.slt (run tools/fm4/merge.py first)", disc.display());
    let inst: Value = serde_json::from_slice(&std::fs::read(data.join("installation.json")).context("installation.json: run fh1setup on the FH1 disc first")?)?;
    let private = data.join("installations").join(inst["id"].as_str().context("installation.json id")?).join("assets/private");
    let root = private.join("imported/fm4");
    std::fs::create_dir_all(&root)?;
    let want = |p: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == p));
    if want("cars") {
        swap(&root.join("cars"), |stage| cars(&disc, &private, stage))?;
    }
    if want("maps") {
        maps(&disc, &root)?;
    }
    println!("[fm4] done: {}", root.display());
    Ok(())
}

/// English strings: the Play Disc's `EN.zip`, with each car pack's `Data_Car.str` merged into its `Data_Car` table
/// (the game mounts the packs' StringTables zips over `game:\Media\StringTables\`; `merge.py` copies them to
/// `media/stringtables/dlc/`).
fn strings(disc: &Path) -> Option<fh1_ui::strtable::StringTables> {
    let mut s = fh1_ui::strtable::StringTables::load_language(disc, "EN").ok()?;
    let dlc = disc.join("media/stringtables/dlc");
    let mut zips: Vec<_> = std::fs::read_dir(&dlc).into_iter().flatten().flatten().map(|e| e.path()).collect();
    zips.sort();
    for z in zips {
        let Ok(mut ar) = fh1_formats::zip::Archive::open(&z) else { continue };
        for e in ar.entries.clone() {
            let n = e.name.replace('\\', "/");
            let Some((lang, file)) = n.split_once('/') else { continue };
            let Some(stem) = file.strip_suffix(".str") else { continue };
            if !lang.eq_ignore_ascii_case("EN") {
                continue;
            }
            let Some(t) = ar.read(&e).ok().and_then(|b| fh1_ui::strtable::StrTable::parse(&b).ok()) else { continue };
            match s.files.get_mut(&stem.to_lowercase()) {
                Some((_, base)) => base.texts.extend(t.texts),
                None => s.insert(stem, t),
            }
        }
    }
    Some(s)
}

fn cars(disc: &Path, private: &Path, out: &Path) -> Result<()> {
    crate::cars::build(disc, out)?;
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let strings = strings(disc);
    let text = |r: &str| strings.as_ref().and_then(|s| s.resolve(r)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| r.to_owned());
    // Class letter from the badge prefix (`CLASS_R3`), and the game's PI display scale (normalised PI -> 100..999).
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
    let (mut rows, mut donated, mut models) = (Vec::new(), 0, 0);
    for c in &index {
        let media = c["media_name"].as_str().unwrap_or_default().to_owned();
        let phys_path = out.join(&media).join("physics.json");
        let mut phys: Value = serde_json::from_slice(&std::fs::read(&phys_path)?)?;
        for &(k, v) in FH1_ONLY_CAR_COLUMNS {
            if phys["car"].get(k).is_none_or(Value::is_null) {
                phys["car"][k] = json!(v);
            }
        }
        let car = phys["car"].clone();
        phys["imported_from"] = json!("fm4");
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
        models += usize::from(c["has_model"].as_bool() == Some(true));
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
            "selectable": true,
            "has_model": c["has_model"],
        }));
    }
    std::fs::write(out.join("index.json"), serde_json::to_vec_pretty(&rows)?)?;
    println!("[fm4] {} cars ({models} with a model), {donated} with an FH1 sound donor", rows.len());
    Ok(())
}

/// One drivable layout: a gamedb `Tracks` row (forward direction only) whose `Ribbon_NN` exists.
struct Layout {
    track: String,
    ribbon: u32,
    name: String,
    length: f64,
    grid: i64,
    flying: i64,
    /// `Environments.DisplayName`: the venue ("Sebring International Raceway"), the map group.
    venue: String,
    /// `Tracks.StartLine` / `FinishLine`: fractions of the `.geo` loop (0 / 1 = a full lap; stages and drag strips use
    /// part of the loop, e.g. Nurburgring 01-04, Fujimi Kaido 01-04, the 402 m strips).
    start_line: f64,
    finish_line: f64,
}

/// Every FM4 circuit layout -> `imported/fm4/<track>_<NN>/` + `maps.json` (`FM4_TRACKS=sebring,...` limits the tracks).
/// Spawns, timing gates and the AI line come from the game's own data (docs/FM4_RECON.md "Tracks"):
/// - `media/AI/Tracks/<track>_NN.geo` (MLP data): the centreline every 2 m (`fWaypointX/Y/Z` = collision x, z, height),
///   `fNormal` (points left, length = half width), walls / kerbs / optimal line as multiples of the normal (`*Chi`).
///   Waypoint 0 is the start/finish line (gamedb `Tracks.StartLine` = 0).
/// - gamedb `StartGridPositions` (metres back from the line / right of the centreline) and `FlyingStartPositions`.
/// - `.pit`: pit-lane splines and the 12 boxes' exit paths (x, height, z).
///
/// Everything is in the collision's space (left-handed, before the engine's MIRROR_Z), like FH1's spawns.
fn maps(disc: &Path, root: &Path) -> Result<()> {
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let strings = strings(disc);
    let text = |r: &str| strings.as_ref().and_then(|s| s.resolve(r)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| r.to_owned());
    let only: Option<Vec<String>> = std::env::var("FM4_TRACKS").ok().map(|v| v.split(',').map(|t| t.trim().to_ascii_lowercase()).collect());
    let mut layouts: Vec<Layout> = Vec::new();
    let mut stmt = db.prepare(
        "SELECT t.MediaName, t.RibbonIndex, t.DisplayName, t.Length, t.DefaultStartGridPositionsId, t.FlyingStartPosId, \
         COALESCE(e.DisplayName, t.MediaName), t.StartLine, t.FinishLine FROM Tracks t LEFT JOIN Environments e ON e.Id = t.EnvironmentId \
         WHERE t.IsReverse = 0 AND t.IsHomespace = 0 ORDER BY t.MediaName, t.RibbonIndex",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Layout {
            track: r.get(0)?,
            ribbon: r.get::<_, i64>(1)? as u32,
            name: r.get(2)?,
            length: r.get(3)?,
            grid: r.get(4)?,
            flying: r.get(5)?,
            venue: r.get(6)?,
            start_line: r.get(7)?,
            finish_line: r.get(8)?,
        })
    })?;
    for l in rows {
        let l = l?;
        let dup = layouts.iter().any(|o| o.track == l.track && o.ribbon == l.ribbon);
        let ui = l.track.to_ascii_lowercase().starts_with("uithum");
        if dup || ui || only.as_ref().is_some_and(|o| !o.contains(&l.track.to_ascii_lowercase())) {
            continue;
        }
        if !disc.join("media/tracks").join(&l.track).join(format!("Ribbon_{:02}", l.ribbon)).is_dir() {
            println!("[fm4] {} ribbon {}: no Ribbon_{:02} folder", l.track, l.ribbon, l.ribbon);
            continue;
        }
        layouts.push(l);
    }
    drop(stmt);
    // Venue (map group) and layout names.
    let names: Vec<(String, String)> = layouts.iter().map(|l| (text(&l.venue), text(&l.name))).collect();
    let mut maps: Vec<Value> = std::fs::read(root.join("maps.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    // First built layout of each track: its scenery is linked into the track's other layouts.
    let mut scenery_of: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (i, l) in layouts.iter().enumerate() {
        let id = format!("{}_{:02}", l.track.to_ascii_lowercase(), l.ribbon);
        let (group, layout_name) = names[i].clone();
        let full = format!("{group} - {layout_name}");
        let dir = root.join(&id);
        let from = scenery_of.get(&l.track).cloned();
        let res = swap(&dir, |stage| build_layout(disc, l, &full, &group, &layout_name, stage, from.as_deref()));
        let kind = match res {
            Ok(()) => serde_json::from_slice::<Value>(&std::fs::read(dir.join("track.json"))?)?["type"].clone(),
            Err(e) => {
                println!("[fm4] {id}: FAILED {e:#}");
                // Not listed (a previous run's folder and entry go too).
                maps.retain(|m| m["id"].as_str() != Some(format!("fm4/{id}").as_str()));
                let _ = std::fs::remove_dir_all(dir.with_extension("staging"));
                let _ = std::fs::remove_dir_all(&dir);
                continue;
            }
        };
        scenery_of.entry(l.track.clone()).or_insert_with(|| dir.join("scenery"));
        let map_id = format!("fm4/{id}");
        maps.retain(|m| m["id"].as_str() != Some(map_id.as_str()));
        maps.push(json!({
            "id": map_id, "name": format!("{full} (FM4)"), "track": group, "layout": layout_name, "length_m": l.length, "type": kind,
        }));
        std::fs::write(root.join("maps.json"), serde_json::to_vec_pretty(&maps)?)?;
    }
    println!("[fm4] {} maps in maps.json", maps.len());
    Ok(())
}

/// One layout into `dir` (the swap stage).
fn build_layout(disc: &Path, l: &Layout, name: &str, group: &str, layout_name: &str, dir: &Path, scenery_from: Option<&Path>) -> Result<()> {
    let track_dir = disc.join("media/tracks").join(&l.track);
    let ribbon = track_dir.join(format!("Ribbon_{:02}", l.ribbon));
    // Sub-section layouts (Nurburgring 01-04, Fujimi Kaido 01-04/10/11: sprint stages of the full course) have no
    // .col / .pvs of their own and drive on Ribbon_00's.
    let find_col = |r: &Path| -> Option<PathBuf> {
        std::fs::read_dir(r).ok()?.filter_map(|e| e.ok().map(|e| e.path())).find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("col")))
    };
    let col = find_col(&ribbon).or_else(|| find_col(&track_dir.join("Ribbon_00"))).with_context(|| format!("no .col in {} or Ribbon_00", ribbon.display()))?;
    let (world, stats) = fh1_world::World::from_col(disc, &l.track, &col)?;
    println!("[fm4] {} {:02}: {} squares, {} triangles, {} surfaces", l.track, l.ribbon, stats.squares, world.tris.len(), world.surfaces.len());
    world.save(&dir.join("world"))?;

    // The AI centreline (`.geo`). Collision space = (x, height, z) = (X, Z, Y).
    let stem = format!("{}_{:02}", l.track.to_ascii_lowercase(), l.ribbon);
    let ai = disc.join("media/AI/Tracks");
    let geo = mlp(&ai.join(format!("{stem}.geo")))?;
    let f = |k: &str| -> Result<Vec<f32>> { geo.get(k).cloned().with_context(|| format!("{stem}.geo: no {k}")) };
    let (x, y, z) = (f("fWaypointX")?, f("fWaypointY")?, f("fWaypointZ")?);
    let (nx, ny) = (f("fNormalX")?, f("fNormalY")?);
    let (lw, rw, opt) = (f("fLeftWallChi")?, f("fRightWallChi")?, f("fChiOptimal")?);
    let n = x.len();
    anyhow::ensure!(n >= 3 && [&y, &z, &nx, &ny, &lw, &rw, &opt].iter().all(|v| v.len() == n), "{stem}.geo: bad arrays");
    let wp = |i: usize| [x[i], z[i], y[i]];
    let normal = |i: usize| [nx[i], 0.0, ny[i]];
    let flat = |v: [f32; 3]| (v[0] * v[0] + v[2] * v[2]).sqrt().max(1e-6);
    let fwd = |i: usize| {
        let i = i.min(n - 2);
        let (a, b) = (wp(i), wp(i + 1));
        let d = [b[0] - a[0], 0.0, b[2] - a[2]];
        [d[0] / flat(d), 0.0, d[2] / flat(d)]
    };
    let gap = |a: [f32; 3], b: [f32; 3]| ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    let closed = gap(wp(0), wp(n - 1)) < 30.0;
    // Start / finish waypoints; a stage (not the whole loop) is point-to-point, a short one a drag strip.
    let start = ((l.start_line * n as f64).round() as usize).min(n - 1);
    let finish = ((l.finish_line * n as f64).round() as usize).min(n - 1);
    let lap = l.start_line <= 0.0 && l.finish_line >= 1.0;
    let kind = match (lap && closed, l.length) {
        (true, _) => "circuit",
        (false, len) if len < 2000.0 => "drag",
        _ => "point-to-point",
    };

    // Spawns: grid (pole first), pit box 1, flying start.
    let mut spawns: Vec<Value> = Vec::new();
    let mut push = |kind: &str, name: String, pos: [f32; 3], facing: [f32; 3], extra: Value| {
        let ground = world.raycast([pos[0], pos[1] + 3.0, pos[2]], [0.0, -1.0, 0.0], 8.0);
        let clear = crate::world::clear_ahead(&world, [pos[0], pos[1] + 1.0, pos[2]], [facing[0], 0.0, facing[2]], 300.0);
        let mut s = json!({
            "name": name, "kind": kind, "position": pos, "facing": facing,
            "ground_y": ground.map(|h| h.point[1]),
            "surface": ground.and_then(|h| world.surface(h.surface)).map(|s| s.name.clone()),
            "clear_ahead": clear,
        });
        if let (Value::Object(s), Value::Object(e)) = (&mut s, extra) {
            s.extend(e);
        }
        spawns.push(s);
    };
    // The centreline point `back` metres behind the line (round the loop on a circuit), with its forward and normal.
    let behind = |back: f32| -> ([f32; 3], [f32; 3], [f32; 3]) {
        let (mut i, mut left) = (start, back);
        loop {
            let j = match (i, closed) {
                (0, true) => n - 1,
                (0, false) => return (wp(0), fwd(0), normal(0)),
                _ => i - 1,
            };
            let (a, b) = (wp(i), wp(j));
            let d = gap(a, b);
            if d >= left {
                let t = left / d.max(1e-6);
                return ([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t], fwd(j), normal(j));
            }
            left -= d;
            i = j;
        }
    };
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let grid: Vec<(i64, f64, f64)> = db
        .prepare("SELECT StartIndex, MetersBackFromStartLine, MetersRightOfCenterLine FROM StartGridPositions WHERE id = ?1 ORDER BY StartIndex")?
        .query_map([l.grid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (index, back, right) in &grid {
        let (p, k, nrm) = behind(*back as f32);
        // The normal points left, so right of the centreline is along -normal.
        let s = -(*right as f32) / flat(nrm);
        push("grid", format!("grid_{index:02}"), [p[0] + nrm[0] * s, p[1], p[2] + nrm[2] * s], k, json!({ "index": index }));
    }
    if let Some(v) = mlp(&ai.join(format!("{stem}.pit"))).ok().and_then(|p| p.get("pitparkout1").cloned()).filter(|v| v.len() >= 6) {
        let (a, b) = ([v[0], v[1], v[2]], [v[3], v[4], v[5]]);
        let d = [b[0] - a[0], 0.0, b[2] - a[2]];
        push("pit", "pit_box_1".into(), a, [d[0] / flat(d), 0.0, d[2] / flat(d)], json!({}));
    }
    let flying = db.query_row("SELECT PosX, PosY, PosZ, HeadingX, HeadingZ, Speed FROM FlyingStartPositions WHERE id = ?1", [l.flying], |r| {
        Ok([r.get::<_, f64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?].map(|v| v as f32))
    });
    if let Ok([px, py, pz, hx, hz, mph]) = flying {
        let h = flat([hx, 0.0, hz]);
        push("flying", "flying_start".into(), [px, py, pz], [hx / h, 0.0, hz / h], json!({ "speed_mph": mph }));
    }
    let on_road = spawns.iter().filter(|s| !s["ground_y"].is_null()).count();
    println!("[fm4] {stem}: {} spawns ({} grid), {on_road} on the collision mesh, {kind}", spawns.len(), grid.len());
    // The drag strips (Autopark 06-09) have no collision under their grid: not drivable from these files.
    anyhow::ensure!(spawns.iter().any(|s| s["kind"] == "grid" && !s["ground_y"].is_null()), "{stem}: no grid slot on the collision mesh");
    std::fs::write(dir.join("world/spawns.json"), serde_json::to_vec_pretty(&json!({ "spawns": spawns }))?)?;

    // Timing gates across the track between the walls: finish = waypoint 0, sectors at 1/3 and 2/3 of the waypoints.
    let gate = |i: usize| {
        let (p, k, nrm) = (wp(i), fwd(i), normal(i));
        let mid = (lw[i] + rw[i]) * 0.5;
        json!({"centre": [p[0] + nrm[0] * mid, p[1], p[2] + nrm[2] * mid], "forward": [k[0], k[2]],
               "width": flat(nrm) * (lw[i] - rw[i]).abs() + 4.0, "waypoint": i})
    };
    // Lap: finish = the start line; stage: its own finish waypoint. Sectors at 1/3 and 2/3 of the way.
    let span = if lap { n } else { (finish + n - start) % n };
    let at = |k: usize| (start + span * k / 3) % n;
    let timing = json!({ "finish": gate(if lap { start } else { finish }), "sectors": [gate(at(1)), gate(at(2))] });
    // AI line: the game's optimal line (centre + normal x fChiOptimal), the centreline and the wall offsets (metres).
    std::fs::create_dir_all(dir.join("ai"))?;
    let along = |chi: &dyn Fn(usize) -> f32| -> Vec<[f32; 3]> {
        (0..n).map(|i| { let (p, m, c) = (wp(i), normal(i), chi(i)); [p[0] + m[0] * c, p[1], p[2] + m[2] * c] }).collect()
    };
    std::fs::write(dir.join("ai/line.json"), serde_json::to_vec(&json!({
        "points": along(&|i| opt[i]), "centre": along(&|_| 0.0),
        "left_wall_m": (0..n).map(|i| flat(normal(i)) * lw[i]).collect::<Vec<_>>(),
        "right_wall_m": (0..n).map(|i| -flat(normal(i)) * rw[i]).collect::<Vec<_>>(),
        "closed": closed, "spacing_m": 2.0, "start": start, "finish": finish,
    }))?)?;

    // Scenery (one build per track; later layouts link the first one's files) and the track's shaders.
    match scenery_from {
        Some(src) => link_tree(src, &dir.join("scenery"))?,
        None => crate::scenery::build_track(&crate::scenery::TrackSrc::fm4(disc, &l.track, l.ribbon, "scenery")?, dir)?,
    }
    let shaders = crate::shaders::track_shaders(&track_dir.join("bin.zip"), &dir.join("shaders/track"))?;
    std::fs::create_dir_all(dir.join("tracks"))?;
    if track_dir.join("TrackSettings.xml").exists() {
        std::fs::copy(track_dir.join("TrackSettings.xml"), dir.join("tracks/TrackSettings.xml"))?;
        let xml = std::fs::read_to_string(track_dir.join("TrackSettings.xml"))?;
        std::fs::write(dir.join("tracks/fx_globals.json"), serde_json::to_vec_pretty(&fx_globals(&xml))?)?;
    }
    std::fs::write(dir.join("track.json"), serde_json::to_vec_pretty(&json!({
        "name": format!("{name} (FM4)"), "game": "fm4", "track": group, "layout": layout_name, "length_m": l.length, "type": kind,
        "timing": timing, "ai_line": "ai/line.json",
    }))?)?;
    println!("[fm4] {stem}: {shaders} track shaders");
    Ok(())
}

/// `tracks/fx_globals.json` = `{globals: {name: [x, y, z, w]}, suffix_defaults: {suffix: [x, y, z, w]}}`: shader globals
/// FM4's track shaders read that FH1's lighting never sets (the engine applies them on FM4 maps every frame).
/// From the track's `TrackSettings.xml` (FM4 version 58); mapping read from the shaders' use (road_2blnd_3 PS):
/// - `V2LightmapColor1` / `2` = `DarkColor` / `LightColor`: the lit colour is lerp(1, 2, lightmap) (VERIFIED by use).
/// - `RoadSpecularParams2` = (RoadGlossiness, RoadGlossiness2, RoadLightmapDarkPoint, RoadLightmapLightPoint): z/w are
///   used as saturate((lm - z) / (w - z)) (VERIFIED by use); `RoadSpecularParams1` = (RoadSpecular, RoadSpecular2,
///   RoadSkid, RoadSkid2) (INFERRED).
/// - `shad_color` = `ShadowColor`, `SunGainScale` = sunGainScale (threshold, power, scale, 0), `TimeGain` = 1 (INFERRED).
/// - every `*_uvOS` (per-texture uv offset.xy / scale.zw: `uv * c.zw + c.xy`) = identity (0, 0, 1, 1).
fn fx_globals(xml: &str) -> Value {
    // `<tag a="1" b="2"/>` attribute values of the first `tag` element.
    let attrs = |tag: &str, keys: &[&str]| -> Option<Vec<f32>> {
        let start = xml.find(&format!("<{tag} "))?;
        let el = &xml[start..start + xml[start..].find('>')?];
        keys.iter()
            .map(|k| {
                let p = el.find(&format!(" {k}=\""))? + k.len() + 3;
                el[p..p + el[p..].find('"')?].trim().parse::<f32>().ok()
            })
            .collect()
    };
    let rgb1 = |tag: &str| attrs(tag, &["r", "g", "b"]).map(|v| [v[0], v[1], v[2], 1.0]);
    let mut g = serde_json::Map::new();
    let mut put = |name: &str, v: Option<[f32; 4]>| {
        if let Some(v) = v {
            g.insert(name.into(), json!(v));
        }
    };
    put("V2LightmapColor1", rgb1("DarkColor"));
    put("V2LightmapColor2", rgb1("LightColor"));
    put("shad_color", rgb1("ShadowColor"));
    put("RoadSpecularParams1", attrs("RoadSpecular", &["RoadSpecular", "RoadSpecular2", "RoadSkid", "RoadSkid2"]).map(|v| [v[0], v[1], v[2], v[3]]));
    put(
        "RoadSpecularParams2",
        attrs("RoadSpecular", &["RoadGlossiness", "RoadGlossiness2", "RoadLightmapDarkPoint", "RoadLightmapLightPoint"]).map(|v| [v[0], v[1], v[2], v[3]]),
    );
    put("SunGainScale", attrs("sunGainScale", &["threshold", "power", "scale"]).map(|v| [v[0], v[1], v[2], 0.0]));
    put("TimeGain", Some([1.0; 4]));
    json!({ "globals": g, "suffix_defaults": { "_uvOS": [0.0, 0.0, 1.0, 1.0] } })
}

/// Hard-links every file of `src` into `dst` (same tree); copies when linking fails.
fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if e.file_type()?.is_dir() {
            link_tree(&s, &d)?;
        } else if std::fs::hard_link(&s, &d).is_err() {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// FM4's self-describing "MLP" data files (`media/AI/Tracks/*.geo|.pit|.seg`): text header `MLPDataStart:`, lines
/// `name:type:n:k: offset`, `MLPDataEnd:`; each field is n x k big-endian values at `offset`. Float fields only.
fn mlp(path: &Path) -> Result<HashMap<String, Vec<f32>>> {
    let d = std::fs::read(path).with_context(|| path.display().to_string())?;
    let end = d.windows(10).position(|w| w == b"MLPDataEnd").context("MLP: no MLPDataEnd")?;
    let head = String::from_utf8_lossy(&d[..end]);
    let mut out = HashMap::new();
    for line in head.lines().skip(1) {
        let parts: Vec<&str> = line.split(':').map(str::trim).collect();
        let [name, "float", n, k, off] = parts[..] else { continue };
        let (Ok(n), Ok(k), Ok(off)) = (n.parse::<usize>(), k.parse::<usize>(), off.parse::<usize>()) else { continue };
        let v = (0..n * k)
            .map(|i| d.get(off + i * 4..off + i * 4 + 4).map(|b| f32::from_be_bytes(b.try_into().unwrap())))
            .collect::<Option<Vec<f32>>>()
            .with_context(|| format!("{}: {name} past the end", path.display()))?;
        out.insert(name.to_owned(), v);
    }
    Ok(out)
}
