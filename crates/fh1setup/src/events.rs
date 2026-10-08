//! `events` group: Colorado's races for the engine's race module (fh1-engine race.rs, docs/RACES.md).
//!
//! Sources (EU disc): gamedb `Races` x `Events` x `CareerEventTypes` x `Tracks` (laps, prize, drivers, AI skills,
//! race mode), `EventParticipants` (the AI field: car, driver, paint), `EventScoring` (credits per place),
//! `Ribbon_00/TrackRoute<Tracks.id>.xml` (grid `start_location_NN`, `route_waypoint_NN`, `route_checkpoint_NN` +
//! width, `end_race_cannon_trigger` + width = finish, `post_race_location`), `GameObjs.xml` `GameplayID` = the race's
//! `HorizonEventID` (festival marker), `colorado.nav` (road path between waypoints, A*), the collision's routes
//! mask (event barrier bits, INFERRED from geometry) and the event-only objects (`.pvsz` activity id = track id).
//!
//! Output: `events/colorado.json` (engine space: collision Z negated; yaw 0 = facing -Z).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use fh1_formats::props;
use fh1_formats::rmb;
use fh1_formats::zip::Archive;

/// A NamedTransform of a TrackRoute file, in engine space.
#[derive(Clone, Copy)]
struct Xf {
    pos: [f32; 3],
    /// Unit facing (x, z) in engine space.
    dir: [f32; 2],
    width: f32,
}

impl Xf {
    fn yaw(&self) -> f32 {
        (-self.dir[0]).atan2(-self.dir[1])
    }
    fn p2(&self) -> [f32; 2] {
        [self.pos[0], self.pos[2]]
    }
}

fn route_transforms(xml: &str) -> Result<BTreeMap<String, Vec<(u32, Xf)>>> {
    let doc = crate::xml::to_json(xml)?;
    let named = match &doc["TrackRoute"]["NamedTransforms"]["NamedTransform"] {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        v => vec![v.clone()],
    };
    let mut out: BTreeMap<String, Vec<(u32, Xf)>> = BTreeMap::new();
    for n in &named {
        let name = n["name"].as_str().unwrap_or("");
        let (kind, idx) = match name.rsplit_once('_') {
            Some((k, i)) if i.chars().all(|c| c.is_ascii_digit()) && !i.is_empty() => (k, i.parse().unwrap_or(0)),
            _ => (name, 0),
        };
        let t = match &n["Transform"] {
            Value::Array(a) => a.first().cloned().unwrap_or(Value::Null),
            v => v.clone(),
        };
        let g = |k: &str| t[k].as_f64().or_else(|| t[k].as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0) as f32;
        let (fx, fz) = (g("facing.x"), -g("facing.z"));
        let l = (fx * fx + fz * fz).sqrt().max(1e-6);
        let width = n["width"].as_f64().or_else(|| n["width"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0) as f32;
        out.entry(kind.to_owned()).or_default().push((idx, Xf { pos: [g("pos.x"), g("pos.y"), -g("pos.z")], dir: [fx / l, fz / l], width }));
    }
    for v in out.values_mut() {
        v.sort_by_key(|x| x.0);
    }
    Ok(out)
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// Road path through the waypoints (A* on the nav graph per leg, straight where the graph has no route), with
/// heights interpolated between the waypoints.
fn route_path(graph: &fh1_ui::nav::Graph, wps: &[Xf]) -> Vec<[f32; 3]> {
    let mut path: Vec<[f32; 3]> = Vec::new();
    for leg in wps.windows(2) {
        let (a, b) = (leg[0], leg[1]);
        let pts = match graph.route(a.p2(), b.p2()) {
            // A route much longer than the straight leg is a detour around a one-way / missing link: go straight.
            Some((p, len)) if len < dist2(a.p2(), b.p2()) * 3.0 + 200.0 => p,
            _ => vec![a.p2(), b.p2()],
        };
        let total: f32 = pts.windows(2).map(|s| dist2(s[0], s[1])).sum::<f32>().max(1e-3);
        let mut run = 0.0;
        for (i, p) in pts.iter().enumerate() {
            if i > 0 {
                run += dist2(pts[i - 1], *p);
            }
            let y = a.pos[1] + (b.pos[1] - a.pos[1]) * (run / total);
            let q = [p[0], y, p[1]];
            if path.last().is_none_or(|l| dist2([l[0], l[2]], [q[0], q[2]]) > 1.0) {
                path.push(q);
            }
        }
    }
    path
}

/// Nearest point of the path to `p` at or after arc length `s_min`: (distance, arc length).
fn project(path: &[[f32; 3]], p: [f32; 2], s_min: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    let mut run = 0.0;
    for seg in path.windows(2) {
        let (a, b) = ([seg[0][0], seg[0][2]], [seg[1][0], seg[1][2]]);
        let d = [b[0] - a[0], b[1] - a[1]];
        let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
        if l > 1e-3 && run + l >= s_min {
            let t = (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / (l * l)).clamp(0.0, 1.0);
            let s = run + t * l;
            if s >= s_min {
                let dist = dist2(p, [a[0] + d[0] * t, a[1] + d[1] * t]);
                if best.is_none_or(|x| dist < x.0 - 1e-3) {
                    best = Some((dist, s));
                }
            }
        }
        run += l;
    }
    best
}

/// The last point along the path within 30 m of `p` (else the nearest): the finish of a loop that passes its own
/// start.
fn project_last(path: &[[f32; 3]], p: [f32; 2]) -> Option<(f32, f32)> {
    let mut last: Option<(f32, f32)> = None;
    let mut run = 0.0;
    for seg in path.windows(2) {
        let (a, b) = ([seg[0][0], seg[0][2]], [seg[1][0], seg[1][2]]);
        let d = [b[0] - a[0], b[1] - a[1]];
        let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
        if l > 1e-3 {
            let t = (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / (l * l)).clamp(0.0, 1.0);
            let dist = dist2(p, [a[0] + d[0] * t, a[1] + d[1] * t]);
            if dist < 30.0 {
                last = Some((dist, run + t * l));
            }
        }
        run += l;
    }
    last.or_else(|| project(path, p, 0.0))
}

/// The path cut at arc length `s_max`.
fn truncate(path: &[[f32; 3]], s_max: f32) -> Vec<[f32; 3]> {
    let mut out = vec![path[0]];
    let mut run = 0.0;
    for seg in path.windows(2) {
        let l = dist2([seg[0][0], seg[0][2]], [seg[1][0], seg[1][2]]);
        if run + l >= s_max {
            let t = ((s_max - run) / l.max(1e-3)).clamp(0.0, 1.0);
            out.push(std::array::from_fn(|k| seg[0][k] + (seg[1][k] - seg[0][k]) * t));
            return out;
        }
        out.push(seg[1]);
        run += l;
    }
    out
}

fn path_len(path: &[[f32; 3]]) -> f32 {
    path.windows(2).map(|s| dist2([s[0][0], s[0][2]], [s[1][0], s[1][2]])).sum()
}

/// Event barrier bits (INFERRED, docs/RACES.md): route-mask bits 0-14 whose walls stand along the race path
/// (>= `NEAR_MIN` triangles within 40 m) without any standing on it (within 5 m of the centre line).
struct Barriers {
    /// Barrier triangles (bit mask without 0x8000, engine-space centroid x/z), bucketed by 20 m cell.
    cells: HashMap<(i32, i32), Vec<(u16, [f32; 2])>>,
}

const CELL: f32 = 20.0;
const NEAR_MIN: u32 = 20;

impl Barriers {
    fn new(world: &fh1_world::World) -> Self {
        let mut cells: HashMap<(i32, i32), Vec<(u16, [f32; 2])>> = HashMap::new();
        for (i, t) in world.tris.iter().enumerate() {
            let bits = t.routes & 0x7FFF;
            if bits == 0 {
                continue;
            }
            let p = world.tri_points(i as u32);
            let c = [(p[0][0] + p[1][0] + p[2][0]) / 3.0, -(p[0][2] + p[1][2] + p[2][2]) / 3.0];
            cells.entry(((c[0] / CELL).floor() as i32, (c[1] / CELL).floor() as i32)).or_default().push((bits, c));
        }
        Self { cells }
    }

    fn bits_for(&self, path: &[[f32; 3]]) -> (u16, [u32; 15], [u32; 15]) {
        // Distance from each nearby barrier triangle to the path (sampled every 2 m).
        let mut best: HashMap<(i32, i32, usize), f32> = HashMap::new();
        let mut samples = Vec::new();
        for s in path.windows(2) {
            let (a, b) = ([s[0][0], s[0][2]], [s[1][0], s[1][2]]);
            let n = (dist2(a, b) / 2.0).ceil().max(1.0) as usize;
            for k in 0..n {
                let t = k as f32 / n as f32;
                samples.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            }
        }
        for p in &samples {
            let (cx, cz) = ((p[0] / CELL).floor() as i32, (p[1] / CELL).floor() as i32);
            for dx in -2..=2 {
                for dz in -2..=2 {
                    let Some(list) = self.cells.get(&(cx + dx, cz + dz)) else { continue };
                    for (i, (_, c)) in list.iter().enumerate() {
                        let d = dist2(*p, *c);
                        let e = best.entry((cx + dx, cz + dz, i)).or_insert(f32::INFINITY);
                        *e = e.min(d);
                    }
                }
            }
        }
        let (mut near, mut on) = ([0u32; 15], [0u32; 15]);
        for (&(x, z, i), &d) in &best {
            let bits = self.cells[&(x, z)][i].0;
            for b in 0..15 {
                if bits & (1 << b) != 0 {
                    if d < 5.0 {
                        on[b] += 1;
                    } else if d < 40.0 {
                        near[b] += 1;
                    }
                }
            }
        }
        let mut mask = 0u16;
        for b in 0..15 {
            if near[b] >= NEAR_MIN && on[b] == 0 {
                mask |= 1 << b;
            }
        }
        (mask, near, on)
    }
}

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let track = disc.join("media/tracks/colorado");
    let ribbon = track.join("Ribbon_00");
    let db = Connection::open_with_flags(disc.join("media/db/gamedb.slt"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let strings = fh1_ui::strtable::StringTables::load_language(disc, "EN").context("EN string tables")?;
    let text = |r: &str| strings.resolve(r).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| r.to_owned());
    let nav = fh1_ui::nav::Nav::parse(&std::fs::read(track.join("colorado.nav"))?).map_err(|e| anyhow::anyhow!("colorado.nav: {e:?}"))?;
    let graph = nav.graph();

    // Festival markers: GameObjs `GameplayID` -> position.
    let game_xml = std::fs::read_to_string(ribbon.join("GameObjs.xml"))?;
    let mut markers: HashMap<String, ([f32; 3], f32)> = HashMap::new();
    for o in props::parse_obj_xml(&game_xml) {
        let p = [o.position[0], o.position[1], -o.position[2]];
        // ZAxis (x, z) as the object's facing, Z negated.
        let yaw = (-o.z_axis[0]).atan2(o.z_axis[2]);
        markers.entry(o.kind.clone()).or_insert((p, yaw));
    }

    // Event-only objects: CollObjs templates (as the scenery group maps them) and the activity ids of their zone
    // instances; `.pgeo` event groups.
    let mut ar = Archive::open(track.join("bin.zip"))?;
    let pvs_bytes = std::fs::read(ribbon.join("Colorado_00.pvs"))?;
    let objs = props::parse_obj_xml(&std::fs::read_to_string(ribbon.join("CollObjs.xml"))?);
    let mut by_name = HashMap::new();
    for e in &ar.entries {
        by_name.entry(e.name.to_ascii_lowercase().replace('\\', "/")).or_insert_with(|| e.clone());
    }
    let mut collobj_map = {
        let cell = std::cell::RefCell::new(&mut ar);
        props::collobj_templates(&objs, &pvs_bytes, |n| {
            let e = by_name.get(&format!("coloradoout.{n:05}.rmb.bin"))?.clone();
            let m = rmb::parse(&cell.borrow_mut().read(&e).ok()?).ok()?;
            Some(m.submodels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(";"))
        })?
    };
    let conds = props::track_object_conditions(&mut ar, &pvs_bytes)?;
    let _ = props::refine_collobj_templates(&objs, &mut collobj_map, &conds);
    let groups = props::track_event_groups(&mut ar, &pvs_bytes)?;
    let mut event_objects: HashMap<u8, Vec<Value>> = HashMap::new();
    for o in &objs {
        let t = o.kind.split('.').next().unwrap_or(&o.kind);
        let Some(&(n, _)) = collobj_map.get(t) else { continue };
        let Some(c) = props::object_condition(&conds, n, o.position) else { continue };
        if c.free_roam {
            continue;
        }
        for p in props::collobj_placements(std::slice::from_ref(o), &collobj_map) {
            for &a in &c.activities {
                event_objects.entry(a).or_default().push(json!({"t": p.model_number, "m": p.matrix}));
            }
        }
    }
    for g in &groups {
        for p in &g.placements {
            for &a in &g.activities {
                event_objects.entry(a).or_default().push(json!({"t": p.model_number, "m": p.matrix}));
            }
        }
    }

    // Local bounds + first submodel name of every event-object template (the engine's race colliders).
    let mut templates = serde_json::Map::new();
    let mut wanted: Vec<u16> = event_objects.values().flatten().filter_map(|o| o["t"].as_u64().map(|t| t as u16)).collect();
    wanted.sort_unstable();
    wanted.dedup();
    for t in wanted {
        let Some(e) = by_name.get(&format!("coloradoout.{t:05}.rmb.bin")).cloned() else { continue };
        let Ok(m) = rmb::parse(&ar.read(&e)?) else { continue };
        templates.insert(t.to_string(), json!({"lo": m.bounds_min, "hi": m.bounds_max, "name": m.submodels.first().map(|s| s.name.clone())}));
    }

    // Collision for the barrier bits.
    let (world, _) = fh1_world::World::from_disc(disc, "colorado")?;
    let barriers = Barriers::new(&world);

    let scoring: Vec<i64> = db.prepare("SELECT Credits FROM EventScoring WHERE ScoringID = 1 ORDER BY place")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let cars: HashMap<i64, String> = db.prepare("SELECT Id, MediaName FROM Data_Car")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let mut stmt = db.prepare(
        "SELECT r.Id, r.EventId, r.TrackId, r.NumLaps, r.HorizonEventID, e.Name, e.Description, e.CareerTypeId, e.NumberOfDrivers, \
         e.CashPrize, e.TargetClass, e.AISkillEasy, e.AISkillMed, e.AISkillHard, e.AISkillPro, ct.Name, ct.RaceModeId, t.Length, \
         e.AITemperamentEasy, e.AITemperamentMed, e.AITemperamentHard, e.AITemperamentPro, \
         e.AIRubberbandEasy, e.AIRubberbandMed, e.AIRubberbandHard, e.AIRubberbandPro \
         FROM Races r JOIN Events e ON e.Id = r.EventId LEFT JOIN CareerEventTypes ct ON ct.id = e.CareerTypeId \
         LEFT JOIN Tracks t ON t.id = r.TrackId ORDER BY r.Id",
    )?;
    let rows: Vec<(i64, i64, i64, i64, String, String, String, i64, i64, i64, i64, [i64; 4], Option<String>, Option<i64>, Option<f64>, [i64; 4], [i64; 4])> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                [r.get(11)?, r.get(12)?, r.get(13)?, r.get(14)?],
                r.get(15)?,
                r.get(16)?,
                r.get(17)?,
                [r.get::<_, Option<i64>>(18)?.unwrap_or(0), r.get::<_, Option<i64>>(19)?.unwrap_or(0), r.get::<_, Option<i64>>(20)?.unwrap_or(0), r.get::<_, Option<i64>>(21)?.unwrap_or(0)],
                [r.get::<_, Option<i64>>(22)?.unwrap_or(0), r.get::<_, Option<i64>>(23)?.unwrap_or(0), r.get::<_, Option<i64>>(24)?.unwrap_or(0), r.get::<_, Option<i64>>(25)?.unwrap_or(0)],
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut participants_q = db.prepare("SELECT Ordinal, AIPlayerID, ColorSequence FROM EventParticipants WHERE EventID = ?1 ORDER BY Priority, ID")?;

    // Progression (events-2, docs/PROGRESSION.md): per-event career columns, wristbands, hubs, classes, named drivers,
    // car class / PI, prize and recommended cars, skill and fame tables.
    let career_cols: HashMap<i64, Value> = db
        .prepare(
            "SELECT Id, Level, HubId, UnlockPointsReq, PopularityPointsReq, EventOrder, PlayerCarId, PlayerCarTargetClass, \
             RestrictionDescription, CareerEventStyle FROM Events",
        )?
        .query_map([], |r| {
            let i = |k: usize| r.get::<_, Option<i64>>(k).map(|v| v.unwrap_or(0));
            Ok((r.get::<_, i64>(0)?, [i(1)?, i(2)?, i(3)?, i(4)?, i(5)?, i(6)?, i(7)?], r.get::<_, Option<String>>(8)?, i(9)?))
        })?
        .filter_map(|r| r.ok())
        .map(|(id, v, restriction, style)| {
            (
                id,
                json!({"level": v[0], "hub": v[1], "unlock_xp": v[2], "popularity_req": v[3], "event_order": v[4],
                    "player_car": cars.get(&v[5]), "player_class": v[6], "restriction": restriction.as_deref().map(text), "style": style}),
            )
        })
        .collect();
    let mut prize_cars: HashMap<i64, Value> = HashMap::new();
    for (ev, car, colour, class) in db
        .prepare("SELECT EventId, RewardCarId, RewardCarColorSequence, RewardCarUpgradeClass FROM Rewards_EventPrizes")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?)))?
        .filter_map(|r| r.ok())
    {
        prize_cars.insert(ev, json!({"car": cars.get(&car), "color_seq": colour.unwrap_or(0), "class": class.unwrap_or(0)}));
    }
    let mut recommended: HashMap<i64, Vec<Value>> = HashMap::new();
    for (ev, car, first) in db
        .prepare("SELECT EventID, CarID, FirstChoice FROM EventRecommendedCars ORDER BY FirstChoice DESC")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?)))?
        .filter_map(|r| r.ok())
    {
        if let Some(c) = cars.get(&car) {
            recommended.entry(ev).or_default().push(json!({"car": c, "first": first.unwrap_or(0) != 0}));
        }
    }
    let mut progression = progression_tables(&db, &text, &cars)?;
    progression["tunables"] = tunables(disc).unwrap_or_else(|e| {
        println!("[events] gametunablesettings.zip: {e:#} (skills / fame use the engine's built-in copy)");
        Value::Null
    });

    let mut races = Vec::new();
    let (mut skipped, mut ratios) = (Vec::new(), Vec::new());
    for (race_id, event_id, track_id, laps, horizon, name, desc, career, drivers, prize, class, skills, type_name, mode, length, temperaments, rubberbands) in rows {
        let file = ribbon.join(format!("TrackRoute{track_id:03}.xml"));
        let Ok(xml) = std::fs::read_to_string(&file) else {
            skipped.push(format!("{horizon} (track {track_id}: no route file)"));
            continue;
        };
        let t = route_transforms(&xml)?;
        let get = |k: &str| t.get(k).map(|v| v.iter().map(|x| x.1).collect::<Vec<_>>()).unwrap_or_default();
        let (grid, wps, cps) = (get("start_location"), get("route_waypoint"), get("route_checkpoint"));
        let finish = get("end_race_cannon_trigger").first().copied().or_else(|| cps.last().copied()).or_else(|| wps.last().copied());
        let (Some(finish), true) = (finish, !grid.is_empty() && wps.len() >= 2) else {
            skipped.push(format!("{horizon} (track {track_id}: no grid / waypoints / finish)"));
            continue;
        };
        let laps = laps.max(1) as u32;
        // Laps repeat only when there's more than one; a one-lap "circuit" runs grid -> finish like a point-to-point.
        let circuit = laps > 1;
        // Route points: the checkpoints for street races (the game's own sequence), the waypoints for festival races.
        let mut route: Vec<Xf> = if cps.is_empty() { wps.clone() } else { std::iter::once(wps[0]).chain(cps.iter().copied()).collect() };
        if circuit && dist2(route[0].p2(), route[route.len() - 1].p2()) > 30.0 {
            route.push(route[0]); // close the loop
        }
        let mut path = route_path(&graph, &route);
        if !circuit && project(&path, finish.p2(), 0.0).is_none_or(|x| x.0 > 30.0) {
            // The finish lies beyond the last route point: drive on to it.
            route.push(finish);
            path = route_path(&graph, &route);
        }
        let total_len = path_len(&path);
        // Gate candidates in race order with their position along the path (searched forward from the last one).
        let candidates: Vec<Xf> = if cps.is_empty() { wps[1..].to_vec() } else { cps.clone() };
        let mut placed: Vec<(Xf, f32)> = Vec::new();
        let mut s_min = 0.0f32;
        for c in candidates {
            if let Some((d, s)) = project(&path, c.p2(), (s_min - 30.0).max(0.0)) {
                if d < 60.0 {
                    s_min = s;
                    placed.push((c, s));
                }
            }
        }
        // The finish (lap line): its last position along the path.
        let s_fin = project_last(&path, finish.p2()).map_or(total_len, |x| x.1);
        let half = |c: &Xf, default: f32| if c.width > 0.0 { (c.width * 0.5).max(15.0) } else { default };
        let mut start_gate = 0u32;
        let mut seq: Vec<Xf> = if circuit {
            // Lap = finish -> finish around the loop; the grid sits part-way in, so lap 1 skips the gates behind it.
            let l = total_len.max(1.0);
            let rel = |s: f32| (s - s_fin).rem_euclid(l);
            let mut v: Vec<(Xf, f32)> = placed.iter().map(|(c, s)| (*c, rel(*s))).filter(|(_, r)| *r > 30.0 && *r < l - 30.0).collect();
            v.sort_by(|a, b| a.1.total_cmp(&b.1));
            let r_grid = project(&path, grid[0].p2(), 0.0).map_or(0.0, |x| rel(x.1));
            start_gate = v.iter().filter(|(_, r)| *r < r_grid).count() as u32;
            v.into_iter().map(|x| x.0).collect()
        } else {
            placed.iter().filter(|(_, s)| *s < s_fin - 30.0).map(|x| x.0).collect()
        };
        seq.push(finish);
        if !circuit {
            // Run-out past the finish isn't part of the race.
            path = truncate(&path, s_fin + 50.0);
        }
        let len = path_len(&path);
        if let Some(l) = length.filter(|l| *l > 0.0) {
            ratios.push(len / l as f32);
        }
        // Gate forward = from the previous gate to the next one (the files' facings don't follow the race direction:
        // waypoints point backwards 1 in 4). Circuits wrap.
        let n = seq.len();
        let at = |k: isize| -> [f32; 2] {
            if k < 0 {
                if circuit { seq[n - 1].p2() } else { grid[0].p2() }
            } else if k as usize >= n {
                if circuit { seq[0].p2() } else { seq[n - 1].p2() }
            } else {
                seq[k as usize].p2()
            }
        };
        let gates: Vec<([f32; 3], [f32; 2], f32)> = seq
            .iter()
            .enumerate()
            .map(|(k, c)| {
                let (a, b) = (at(k as isize - 1), at(k as isize + 1));
                let d = [b[0] - a[0], b[1] - a[1]];
                let l = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-3);
                let w = if k + 1 == n { half(c, 25.0) } else if cps.is_empty() { 30.0 } else { half(c, 25.0) };
                (c.pos, [d[0] / l, d[1] / l], w)
            })
            .collect();
        let (bits, near, on) = barriers.bits_for(&path);
        let objects = u8::try_from(track_id).ok().and_then(|a| event_objects.get(&a)).cloned().unwrap_or_default();
        let marker = markers.get(&horizon).copied();
        let field: Vec<Value> = participants_q
            .query_map([event_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
            .filter_map(|r| r.ok())
            .map(|(car, driver, colour)| json!({"car": cars.get(&car), "car_id": car, "driver": driver, "color_seq": colour}))
            .collect();
        let post = get("post_race_location").first().copied();
        races.push(json!({
            "race_id": race_id,
            "event_id": event_id,
            "horizon_id": horizon,
            "name": text(&name),
            "description": text(&desc),
            "career_type": career,
            "type": type_name.as_deref().map(text),
            "mode": mode,
            "laps": laps,
            "circuit": circuit,
            "drivers": drivers,
            "credits": prize,
            "target_class": class,
            "ai_skills": skills,
            "ai_temperaments": temperaments,
            "ai_rubberbands": rubberbands,
            "track_id": track_id,
            "route_file": format!("Ribbon_00/TrackRoute{track_id:03}.xml"),
            "length_m": len,
            "start_gate": start_gate,
            "game_length_m": length,
            "marker": marker.map(|(p, yaw)| json!({"pos": p, "yaw": yaw})).unwrap_or_else(|| json!({"pos": grid[0].pos, "yaw": grid[0].yaw()})),
            "marker_kind": if marker.is_some() { "festival" } else { "grid" },
            "grid": grid.iter().map(|g| [g.pos[0], g.pos[1], g.pos[2], g.yaw()]).collect::<Vec<_>>(),
            "gates": gates.iter().map(|g| json!({"p": g.0, "f": g.1, "w": g.2})).collect::<Vec<_>>(),
            "path": path,
            "post_race": post.map(|p| [p.pos[0], p.pos[1], p.pos[2], p.yaw()]),
            "barrier_bits": bits,
            "barrier_near": near,
            "barrier_on": on,
            "event_objects": objects,
            "field": field,
            // Finish spark / confetti cannons (two rows of 5) and the start gantry cannon: [x, y, z, facing x, facing z].
            "cannons_left": get("end_race_cannon_left").iter().map(|x| [x.pos[0], x.pos[1], x.pos[2], x.dir[0], x.dir[1]]).collect::<Vec<_>>(),
            "cannons_right": get("end_race_cannon_right").iter().map(|x| [x.pos[0], x.pos[1], x.pos[2], x.dir[0], x.dir[1]]).collect::<Vec<_>>(),
            "start_cannon": get("start_gantry_cannon").first().map(|x| [x.pos[0], x.pos[1], x.pos[2], x.dir[0], x.dir[1]]),
            "career": career_cols.get(&event_id),
            "prize_car": prize_cars.get(&event_id),
            "recommended_cars": recommended.get(&event_id),
        }));
    }
    ratios.sort_by(|a, b| a.total_cmp(b));
    let median = ratios.get(ratios.len() / 2).copied().unwrap_or(0.0);
    let with_bits = races.iter().filter(|r| r["barrier_bits"].as_u64().unwrap_or(0) != 0).count();
    let with_objects = races.iter().filter(|r| r["event_objects"].as_array().is_some_and(|a| !a.is_empty())).count();
    println!(
        "[events] {} races ({} skipped), path / gamedb length median {median:.2}, {with_bits} with barrier bits, {with_objects} with event objects",
        races.len(),
        skipped.len()
    );
    for s in &skipped {
        println!("[events] skipped {s}");
    }
    let dir = out.join("colorado");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("events.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "progression": progression,
            "scoring": scoring,
            "object_templates": templates,
            "hand_of_god": {"facing_mph": 40.0, "facing_cos": 0.342, "facing_s": 3.0, "progress_mph": 5.0, "progress_s": 3.0,
                "offtrack_m": [5.0, 15.0], "offtrack_s": [6.0, 3.0], "noncollide_s": 3.0},
            "races": races,
        }))?,
    )?;
    Ok(())
}

/// Career tables (events-2): wristbands (CareerWristbandLevels + WristbandScoring), street-race hubs (EventHubs +
/// EventHubInitialEvents), car classes (CarClasses), named drivers (AIPlayers), every car's class / PI / drive type
/// and the wristband reward cars.
fn progression_tables(db: &Connection, text: &dyn Fn(&str) -> String, cars: &HashMap<i64, String>) -> Result<Value> {
    let mut points: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (tier, pts) in db
        .prepare("SELECT WristbandLevel, Points FROM WristbandScoring ORDER BY WristbandLevel, Place")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .filter_map(|r| r.ok())
    {
        points.entry(tier).or_default().push(pts);
    }
    let wristbands: Vec<Value> = db
        .prepare("SELECT Id, XP, Name, Thumbnail, RivalScoreMultiplier, NemesisBonus, Color FROM CareerWristbandLevels ORDER BY Id")?
        .query_map([], |r| {
            let colour = r.get::<_, Option<i64>>(6)?.unwrap_or(0) as u32;
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<f64>>(4)?, r.get::<_, Option<i64>>(5)?, colour))
        })?
        .filter_map(|r| r.ok())
        .map(|(id, xp, name, thumb, mult, nemesis, argb)| {
            // Name: the string table, else the Thumbnail column ("Yellow", "Green", ..). Color is ARGB as a signed int.
            let name = name.as_deref().map(text).filter(|n| !n.starts_with("_&")).or(thumb);
            json!({"id": id, "xp": xp, "name": name, "argb": argb, "rival_mult": mult.unwrap_or(1.0),
                "nemesis_bonus": nemesis.unwrap_or(0), "points": points.get(&id)})
        })
        .collect();
    let mut initial: HashMap<i64, Vec<i64>> = HashMap::new();
    for (hub, ev) in db.prepare("SELECT HubId, EventId FROM EventHubInitialEvents")?.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?.filter_map(|r| r.ok()) {
        initial.entry(hub).or_default().push(ev);
    }
    let hubs: Vec<Value> = db
        .prepare("SELECT Id, Name, UnlockPointsReq FROM EventHubs ORDER BY Id")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?)))?
        .filter_map(|r| r.ok())
        .map(|(id, name, xp)| json!({"id": id, "name": name.as_deref().map(text), "unlock_xp": xp.unwrap_or(0), "initial_events": initial.get(&id)}))
        .collect();
    let classes: Vec<(i64, String, f64, i64, i64)> = db
        .prepare("SELECT Id, BadgeTexturePathPrefix, MaxPerformanceIndex, MaxDisplayPerformanceIndex, CareerMinEligiblePI FROM CarClasses ORDER BY Id")?
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?, r.get(3)?, r.get::<_, Option<i64>>(4)?.unwrap_or(0))))?
        .collect::<Result<_, _>>()?;
    // Display PI: the internal 0..1 index mapped piecewise-linearly between the classes' Max / MaxDisplay pairs (INFERRED).
    let display_pi = |f: f64| -> i64 {
        let (mut lo_f, mut lo_d) = (0.0, 0.0);
        for (_, _, max_f, max_d, _) in &classes {
            if f <= *max_f {
                let t = ((f - lo_f) / (max_f - lo_f).max(1e-9)).clamp(0.0, 1.0);
                return (lo_d + t * (*max_d as f64 - lo_d)).round() as i64;
            }
            (lo_f, lo_d) = (*max_f, *max_d as f64);
        }
        999
    };
    let class_json: Vec<Value> = classes
        .iter()
        .map(|(id, badge, _, max_d, min_pi)| json!({"id": id, "name": badge.trim_start_matches("CLASS_"), "max_pi": max_d, "career_min_pi": min_pi}))
        .collect();
    let mut car_info = serde_json::Map::new();
    for (media, class, pi, drive, year, make, selectable, display, cost, unicorn) in db
        .prepare("SELECT MediaName, ClassID, PerformanceIndex, DriveTypeID, Year, MakeName, IsSelectable, DisplayName, BaseCost, IsUnicorn FROM Data_Car")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                r.get::<_, Option<i64>>(9)?.unwrap_or(0),
            ))
        })?
        .filter_map(|r| r.ok())
    {
        // `price`: Data_Car.BaseCost (autoshow price, credits; 0 = not for sale); `unicorn`: IsUnicorn (events-5).
        car_info.insert(
            media,
            json!({"class": class, "pi": display_pi(pi), "drive": drive, "year": year, "make": make.as_deref().map(text), "selectable": selectable != 0,
                "name": display.as_deref().map(text).filter(|n| !n.starts_with("_&")), "price": cost.max(0.0) as i64, "unicorn": unicorn != 0}),
        );
    }
    let drivers: Vec<Value> = db
        .prepare(
            "SELECT Id, Name, FirstName, LastName, Rank, WristbandLevel, Nemesis, SkillModifier, AggroModifier, RubberBandModifier, \
             IsFreeRoam, UISkill FROM AIPlayers ORDER BY Id",
        )?
        .query_map([], |r| {
            let i = |k: usize| r.get::<_, Option<i64>>(k).map(|v| v.unwrap_or(0));
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, [i(4)?, i(5)?, i(6)?, i(7)?, i(8)?, i(9)?, i(10)?, i(11)?]))
        })?
        .filter_map(|r| r.ok())
        .map(|(id, name, first, last, v)| {
            let first = first.as_deref().map(text).unwrap_or_default();
            let last = last.as_deref().map(text).filter(|l| !l.starts_with("_&")).unwrap_or_default();
            let full = format!("{first} {last}").trim().to_owned();
            let display = name.as_deref().map(text).filter(|n| !n.starts_with("_&") && !n.is_empty()).unwrap_or_else(|| full.clone());
            json!({"id": id, "name": display, "full_name": full, "rank": v[0], "tier": v[1], "nemesis": v[2] != 0, "skill_mod": v[3],
                "aggro_mod": v[4], "rubber_mod": v[5], "free_roam": v[6] != 0, "ui_skill": v[7]})
        })
        .collect();
    let wristband_rewards: Vec<Value> = db
        .prepare("SELECT WristbandLevel, RewardCarId FROM Rewards_Wristband ORDER BY WristbandLevel")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .filter_map(|r| r.ok())
        .map(|(tier, car)| json!({"tier": tier, "car": cars.get(&car)}))
        .collect();
    println!("[events] progression: {} wristbands, {} hubs, {} drivers, {} cars", wristbands.len(), hubs.len(), drivers.len(), car_info.len());
    Ok(json!({
        "wristbands": wristbands,
        "hubs": hubs,
        "classes": class_json,
        "cars": car_info,
        "drivers": drivers,
        "wristband_rewards": wristband_rewards,
    }))
}

/// `media/gametunablesettings.zip`: the popularity ladder (Fame.xml `<Level rank target>`, rank 249 -> 1, cumulative
/// fame) and the free-roam skills (HorizonFeats.xml, converted as is for later use; the engine has its own copy of the
/// grades it uses).
fn tunables(disc: &Path) -> Result<Value> {
    let mut ar = Archive::open(disc.join("media/gametunablesettings.zip"))?;
    let mut read = |want: &str| -> Result<Value> {
        let e = ar.entries.iter().find(|e| e.name.replace('\\', "/").rsplit('/').next().is_some_and(|n| n.eq_ignore_ascii_case(want))).cloned();
        let e = e.with_context(|| format!("{want} not in the archive"))?;
        let bytes = ar.read(&e)?;
        let s = String::from_utf8_lossy(&bytes);
        crate::xml::to_json(s.trim_start_matches('\u{feff}'))
    };
    let fame_doc = read("Fame.xml")?;
    let mut fame: Vec<(i64, i64)> = Vec::new();
    collect_levels(&fame_doc, &mut fame);
    fame.sort_by_key(|x| -x.0);
    let feats = read("HorizonFeats.xml").unwrap_or(Value::Null);
    let sponsors = read("SponsorshipChallenges.xml").map(|v| sponsor_challenges(&v)).unwrap_or_default();
    // GameTunableSettings.ini: `Section\Key value` lines (CarValueScales, RaceWinnings...), events-5.
    let mut economy = serde_json::Map::new();
    if let Some(e) = ar.entries.iter().find(|e| e.name.replace('\\', "/").rsplit('/').next().is_some_and(|n| n.eq_ignore_ascii_case("GameTunableSettings.ini"))).cloned() {
        let text = String::from_utf8_lossy(&ar.read(&e)?).into_owned();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                if k.starts_with("CarValueScales\\") || k.starts_with("RaceWinnings\\") {
                    if let Ok(x) = v.parse::<f64>() {
                        economy.insert(k.replace('\\', "/"), json!(x));
                    }
                }
            }
        }
    }
    println!(
        "[events] tunables: {} fame levels, feats {}, {} sponsor challenges, {} economy values",
        fame.len(),
        if feats.is_null() { "missing" } else { "ok" },
        sponsors.len(),
        economy.len()
    );
    Ok(json!({"fame": fame, "feats": feats, "sponsors": sponsors, "economy": economy}))
}

/// SponsorshipChallenges.xml -> [{id, type, ranks: [{level?, skill?, grade?, number?, credits}]}] (events-5).
fn sponsor_challenges(doc: &Value) -> Vec<Value> {
    let list = |v: &Value| -> Vec<Value> {
        match v {
            Value::Array(a) => a.clone(),
            Value::Null => Vec::new(),
            other => vec![other.clone()],
        }
    };
    let challenges = list(&doc["SkillChallenges"]["Challenge"]);
    challenges
        .iter()
        .map(|c| {
            let ranks: Vec<Value> = list(&c["Rank"])
                .iter()
                .map(|r| {
                    json!({"level": r["levelRequired"], "skill": r["skill"], "grade": r["grade"], "number": r["number"], "credits": r["credits"]})
                })
                .collect();
            json!({"id": c["id"], "type": c["type"], "ranks": ranks})
        })
        .collect()
}

/// Every object with `rank` and `target` attributes, anywhere in the tree.
fn collect_levels(v: &Value, out: &mut Vec<(i64, i64)>) {
    let num = |x: &Value| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)).or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f as i64));
    match v {
        Value::Object(m) => {
            if let (Some(r), Some(t)) = (m.get("rank").and_then(num), m.get("target").and_then(num)) {
                out.push((r, t));
            }
            m.values().for_each(|c| collect_levels(c, out));
        }
        Value::Array(a) => a.iter().for_each(|c| collect_levels(c, out)),
        _ => {}
    }
}
