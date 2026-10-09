//! Setup group `story`: the disc's cutscene / camera / airborne-challenge / trigger XML as JSON the engine reads at
//! runtime (`<assets>/story/`). Port of data/extracted/story/_scripts/build_story.py (dev-only reference), reading the
//! disc directly: `media/gamemodes.zip` (`Colorado/Cutscenes/**`, `TestBed/Cutscenes/cutscenes_default.xml`, flow XML for
//! TriggerZones, `Colorado/Animations/*.xml`) and the loose `media/tracks/colorado/{airborne_challenges.xml,
//! Ribbon_00/GameObjs.xml}`.
//!
//! Output (group dir):
//! - `cutscenes/<name>.json` (one per cutscene name, last wins on a duplicate), `cutscenes/index.json`
//! - `airborne.json` `{ "challenges": [...] }`
//! - `triggers.json` `{ "zones": [...], "animations": [...], "markers": [...] }`
//!
//! Handedness: the gamemode XML is left-handed (+Z north); every position is written as engine space `[x, y, -z]`.
//! Angles (yaw/pitch/roll/fov) are the raw file values; their convention is the engine's business.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;

use anyhow::{bail, Context, Result};
use fh1_formats::zip::{Archive, Entry};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde_json::{json, Map, Value};

const GAMEMODES_ZIP: &str = "media/gamemodes.zip";
const AIRBORNE_XML: &str = "media/tracks/colorado/airborne_challenges.xml";
const GAMEOBJS_XML: &str = "media/tracks/colorado/Ribbon_00/GameObjs.xml";

// ---------------------------------------------------------------- tiny DOM

/// One XML element: tag, attributes in file order, child elements (text is dropped).
#[derive(Debug, Clone)]
pub struct El {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub kids: Vec<El>,
}

impl El {
    fn get(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    fn child(&self, tag: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.tag == tag)
    }

    /// All descendants named `tag`, document order.
    fn descendants<'a>(&'a self, tag: &str, out: &mut Vec<&'a El>) {
        for k in &self.kids {
            if k.tag == tag {
                out.push(k);
            }
            k.descendants(tag, out);
        }
    }
}

fn ga<'a>(e: Option<&'a El>, k: &str) -> Option<&'a str> {
    e.and_then(|e| e.get(k))
}

fn open_el(e: &BytesStart) -> Result<El> {
    let tag = AsRef::<str>::as_ref(&e.name()).to_owned();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a?;
        let k = AsRef::<str>::as_ref(&a.key).to_owned();
        let v = a.normalized_value(XmlVersion::Implicit1_0)?.into_owned();
        attrs.push((k, v));
    }
    Ok(El { tag, attrs, kids: Vec::new() })
}

/// Parse a document into its root element.
pub fn parse(xml: &str) -> Result<El> {
    let xml = xml.trim_start_matches('\u{feff}');
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<El> = vec![El { tag: String::new(), attrs: Vec::new(), kids: Vec::new() }];
    loop {
        match reader.read_event()? {
            Event::Start(e) => stack.push(open_el(&e)?),
            Event::Empty(e) => {
                let el = open_el(&e)?;
                stack.last_mut().unwrap().kids.push(el);
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    bail!("unbalanced end tag");
                }
                let el = stack.pop().unwrap();
                stack.last_mut().unwrap().kids.push(el);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        bail!("unclosed element <{}>", stack.last().map(|e| e.tag.as_str()).unwrap_or(""));
    }
    let mut root = stack.pop().unwrap();
    if root.kids.is_empty() {
        bail!("no root element");
    }
    Ok(root.kids.swap_remove(0))
}

/// The disc's airborne_challenges.xml closes one element as `</DifficulySettings` (no `>`): repair it in memory.
fn repair_tags(xml: &str) -> String {
    const BAD: &str = "</DifficulySettings";
    let mut out = String::with_capacity(xml.len() + 8);
    let mut rest = xml;
    while let Some(i) = rest.find(BAD) {
        let end = i + BAD.len();
        out.push_str(&rest[..end]);
        rest = &rest[end..];
        if !rest.starts_with('>') {
            out.push('>');
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------- value helpers

/// Python `num()` of the reference script: integer-looking text without '.' becomes an integer, other numbers a float,
/// anything else stays a string.
fn num(s: &str) -> Value {
    let t = s.trim();
    let numeric = !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'));
    if numeric {
        if let Ok(f) = t.parse::<f64>() {
            if f.is_finite() {
                if f.fract() == 0.0 && !s.contains('.') && f.abs() < 9.0e15 {
                    return Value::from(f as i64);
                }
                if let Some(n) = serde_json::Number::from_f64(f) {
                    return Value::Number(n);
                }
            }
        }
    }
    Value::String(s.to_string())
}

fn opt_num(s: Option<&str>) -> Value {
    s.map_or(Value::Null, num)
}

fn opt_str(s: Option<&str>) -> Value {
    s.map_or(Value::Null, |s| Value::String(s.to_string()))
}

fn fnum(s: Option<&str>, default: f64) -> f64 {
    s.and_then(|s| s.trim().parse::<f64>().ok()).filter(|f| f.is_finite()).unwrap_or(default)
}

/// `[x, y, -z]`: left-handed gamemode space -> engine space.
fn eng(x: f64, y: f64, z: f64) -> Value {
    json!([x, y, -z])
}

fn attrs_obj(el: &El) -> Map<String, Value> {
    el.attrs.iter().map(|(k, v)| (k.clone(), num(v))).collect()
}

fn opt_attrs(el: Option<&El>) -> Value {
    el.map_or(Value::Null, |e| Value::Object(attrs_obj(e)))
}

/// Attributes plus nested children (`children`, each with its `tag`).
fn elem_json(el: &El) -> Value {
    let mut m = attrs_obj(el);
    if !el.kids.is_empty() {
        let kids: Vec<Value> = el
            .kids
            .iter()
            .map(|k| {
                let mut o = Map::new();
                o.insert("tag".into(), Value::String(k.tag.clone()));
                if let Value::Object(rest) = elem_json(k) {
                    o.extend(rest);
                }
                Value::Object(o)
            })
            .collect();
        m.insert("children".into(), Value::Array(kids));
    }
    Value::Object(m)
}

// ---------------------------------------------------------------- cutscenes

fn key_json(k: &El) -> Value {
    const CORE: [&str; 7] = ["Time", "PosX", "PosY", "PosZ", "Yaw", "Pitch", "Roll"];
    let mut m = Map::new();
    m.insert("t".into(), opt_num(k.get("Time")));
    m.insert("pos".into(), eng(fnum(k.get("PosX"), 0.0), fnum(k.get("PosY"), 0.0), fnum(k.get("PosZ"), 0.0)));
    m.insert("yaw".into(), opt_num(k.get("Yaw")));
    m.insert("pitch".into(), opt_num(k.get("Pitch")));
    m.insert("roll".into(), opt_num(k.get("Roll")));
    m.insert("fov".into(), opt_num(k.get("FOV")));
    let extra: Map<String, Value> =
        k.attrs.iter().filter(|(a, _)| !CORE.contains(&a.as_str()) && a != "FOV").map(|(a, v)| (a.clone(), num(v))).collect();
    if !extra.is_empty() {
        m.insert("extra".into(), Value::Object(extra));
    }
    Value::Object(m)
}

fn fade_json(el: Option<&El>) -> Value {
    let Some(e) = el else { return Value::Null };
    json!({
        "duration": fnum(e.get("Duration"), 0.0),
        "hold": fnum(e.get("HoldDuration"), 0.0),
        "curve": opt_str(e.get("CurveType")),
        "color": [fnum(e.get("Color.x"), 0.0), fnum(e.get("Color.y"), 0.0), fnum(e.get("Color.z"), 0.0)],
    })
}

fn cam_json(cam: &El) -> Value {
    const CAM_USED: [&str; 5] = ["Type", "StartCut", "duration", "TargetName", "DisableCarRendering"];
    const ANIM_USED: [&str; 6] = ["PosAnimType", "RotAnimType", "TimeAnimType", "IsInCockpit", "EaseIn", "EaseOut"];
    let anim = cam.child("Anim");
    let mut m = Map::new();
    m.insert("type".into(), opt_str(cam.get("Type")));
    m.insert("start_cut".into(), opt_num(cam.get("StartCut")));
    m.insert("duration".into(), opt_num(cam.get("duration")));
    m.insert("target_name".into(), opt_str(cam.get("TargetName")));
    m.insert("pos_space".into(), opt_str(ga(anim, "PosAnimType")));
    m.insert("rot_space".into(), opt_str(ga(anim, "RotAnimType")));
    m.insert("time_anim".into(), opt_str(ga(anim, "TimeAnimType")));
    m.insert("is_in_cockpit".into(), opt_num(ga(anim, "IsInCockpit")));
    m.insert("ease_in".into(), opt_num(ga(anim, "EaseIn")));
    m.insert("ease_out".into(), opt_num(ga(anim, "EaseOut")));
    m.insert("disable_car_rendering".into(), opt_num(cam.get("DisableCarRendering")));
    m.insert("fade_in".into(), fade_json(cam.child("FadeInOptions")));
    m.insert("fade_out".into(), fade_json(cam.child("FadeOutOptions")));

    // Keys: the <Anim> keys, then any <Key> directly under <Cam> (reference script order).
    let mut keys: Vec<Value> = Vec::new();
    if let Some(a) = anim {
        keys.extend(a.kids.iter().filter(|k| k.tag == "Key").map(key_json));
    }
    keys.extend(cam.kids.iter().filter(|k| k.tag == "Key").map(key_json));
    m.insert("keys".into(), Value::Array(keys));

    let pe = cam.child("PostEffectsAnim");
    let post_keys: Vec<Value> = pe
        .map(|p| {
            p.kids
                .iter()
                .filter(|k| k.tag == "Key")
                .map(|k| {
                    let mut o = Map::new();
                    o.insert("t".into(), opt_num(k.get("Time")));
                    for (a, v) in &k.attrs {
                        if a != "Time" {
                            o.insert(a.clone(), num(v));
                        }
                    }
                    Value::Object(o)
                })
                .collect()
        })
        .unwrap_or_default();
    m.insert("post_keys".into(), Value::Array(post_keys));

    let mut raw = Map::new();
    for (a, v) in &cam.attrs {
        if !CAM_USED.contains(&a.as_str()) {
            raw.insert(a.clone(), num(v));
        }
    }
    if let Some(an) = anim {
        for (a, v) in &an.attrs {
            if !ANIM_USED.contains(&a.as_str()) {
                raw.insert(a.clone(), num(v));
            }
        }
    }
    m.insert("raw".into(), Value::Object(raw));

    // Additions beyond the fixed schema: the remaining per-cam elements.
    if let Some(p) = pe {
        m.insert("post_effects".into(), Value::Object(attrs_obj(p)));
    }
    let mut elements = Map::new();
    for tag in ["CameraPhysics", "CarPositioning", "DynamicLimiting"] {
        if let Some(e) = cam.child(tag) {
            elements.insert(tag.to_string(), elem_json(e));
        }
    }
    if !elements.is_empty() {
        m.insert("elements".into(), Value::Object(elements));
    }
    Value::Object(m)
}

fn events_json(cs: &El) -> Vec<Value> {
    let Some(evs) = cs.child("Events") else { return Vec::new() };
    evs.kids
        .iter()
        .filter(|e| e.tag == "Event")
        .map(|ev| {
            let triggers: Vec<Value> = ev
                .kids
                .iter()
                .filter(|t| t.tag == "EventTrigger")
                .map(|t| {
                    let engine = match (t.get("x"), t.get("y"), t.get("z")) {
                        (Some(x), Some(y), Some(z)) => eng(fnum(Some(x), 0.0), fnum(Some(y), 0.0), fnum(Some(z), 0.0)),
                        _ => Value::Null,
                    };
                    json!({
                        "class": opt_str(t.get("id")),
                        "name": opt_str(t.get("name")),
                        "parent": opt_str(t.get("parent")),
                        "engine": engine,
                        "attrs": Value::Object(attrs_obj(t)),
                    })
                })
                .collect();
            json!({ "time": opt_num(ev.get("time")), "triggers": triggers })
        })
        .collect()
}

fn cutscene_json(cs: &El, file: &str, group: &str) -> Value {
    let cams: Vec<Value> = cs.child("Cams").map(|c| c.kids.iter().filter(|k| k.tag == "Cam").map(cam_json).collect()).unwrap_or_default();
    json!({
        "name": opt_str(cs.get("name")),
        "group": group,
        "file": file,
        "playback_mode": opt_str(cs.get("PlaybackMode")),
        "duration_s": opt_num(cs.get("Duration")),
        "loop_from_s": opt_num(cs.get("LoopFrom")),
        "mirror_rhd": opt_num(cs.get("MirrorForRightHandDrive")),
        "cams": cams,
        "events": events_json(cs),
    })
}

/// Every `<Cutscene>` directly under the root of one cutscene XML, as schema JSON.
pub fn parse_cutscenes(xml: &str, file: &str, group: &str) -> Result<Vec<Value>> {
    let root = parse(xml)?;
    Ok(root.kids.iter().filter(|c| c.tag == "Cutscene").map(|cs| cutscene_json(cs, file, group)).collect())
}

/// (camera count, key count) of one cutscene document.
fn doc_counts(doc: &Value) -> (usize, usize) {
    let cams = doc["cams"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let keys = cams.iter().map(|c| c["keys"].as_array().map_or(0, |k| k.len())).sum();
    (cams.len(), keys)
}

// ---------------------------------------------------------------- airborne challenges

fn airborne_json(xml: &str) -> Result<Vec<Value>> {
    let root = parse(&repair_tags(xml)).context("airborne_challenges.xml")?;
    let mut out = Vec::new();
    for ce in root.kids.iter().filter(|k| k.tag == "ChallengeEvent") {
        let names = ce.child("Names");
        let timing = ce.child("Timing");
        let mut cps_el = Vec::new();
        ce.descendants("CheckPoint", &mut cps_el);
        let cps: Vec<Value> = cps_el.iter().map(|c| Value::Object(attrs_obj(c))).collect();
        let diffs: Vec<Value> =
            ce.kids.iter().filter(|k| k.tag == "DifficulySettings").map(|d| opt_attrs(d.child("AnimSpeeds"))).collect();
        let diff = |i: usize| diffs.get(i).cloned().unwrap_or(Value::Null);
        out.push(json!({
            "event_id": opt_num(ce.get("eventid")),
            "ref_name": opt_str(ce.get("refname")),
            "anim_object": opt_str(ga(names, "objectfilename")),
            "anim_pre_race_cs": opt_str(ga(names, "anim_pre_race_CS")),
            "anim_in_race": opt_str(ga(names, "anim_in_race")),
            "anim_post_race_cs": opt_str(ga(names, "anim_post_race_CS")),
            "origin_offset_in_max": opt_attrs(ce.child("OriginOffsetInMax")),
            "opponent": opt_attrs(ce.child("Opponent")),
            "anim_start_on_countdown": opt_str(ga(ce.child("AnimStart"), "on_countdown")),
            "in_race_cross_start_s": opt_num(ga(timing, "in_race_anim_cross_start_time")),
            "in_race_cross_finish_s": opt_num(ga(timing, "in_race_anim_cross_finish_time")),
            "after_finish": opt_attrs(ce.child("AfterFinish")),
            "checkpoints": cps,
            "checkpoint_count": cps_el.len(),
            "rubber_banding": opt_attrs(ce.child("RubberBanding")),
            "difficulty_anim_speeds": { "Easy": diff(0), "Medium": diff(1), "Hard": diff(2), "Pro": diff(3) },
            "rumble_near_player": opt_attrs(ce.child("RumbleWhenNearPlayer")),
            "check_progress": opt_attrs(ce.child("CheckPointProgress")),
        }));
    }
    Ok(out)
}

// ---------------------------------------------------------------- triggers

fn categorise(gid: &str) -> &'static str {
    const PRE: [(&str, &str); 12] = [
        ("BF_", "barnfind_door"),
        ("BARNFIND_", "barnfind_car"),
        ("PLANE_RACE", "plane_race_marker"),
        ("EXHIBITION_", "exhibition_marker"),
        ("FESTIVAL_", "festival_marker"),
        ("flyer_", "flyer_sign"),
        ("speed_", "speed_camera"),
        ("average_", "average_speed_camera"),
        ("FR", "festival_race_marker"),
        ("NR", "nemesis_race_marker"),
        ("HEADLINE", "headline_marker"),
        ("STREET", "street_race_marker"),
    ];
    PRE.iter().find(|(p, _)| gid.starts_with(p)).map_or("other", |(_, c)| c)
}

struct Marker {
    gameplay_id: String,
    engine: [f64; 3],
    json: Value,
}

fn markers(xml: &str) -> Result<Vec<Marker>> {
    let root = parse(xml).context("GameObjs.xml")?;
    let mut out = Vec::new();
    for el in &root.kids {
        let (Some(gid), Some(pos)) = (el.get("GameplayID"), el.child("Pos")) else { continue };
        let (x, y, z) = (fnum(pos.get("x"), 0.0), fnum(pos.get("y"), 0.0), fnum(pos.get("z"), 0.0));
        let mut axes = Map::new();
        if let Some(o) = el.child("Orientation") {
            for ax in ["XAxis", "YAxis", "ZAxis"] {
                if let Some(a) = o.child(ax) {
                    axes.insert(ax.to_string(), eng(fnum(a.get("x"), 0.0), fnum(a.get("y"), 0.0), fnum(a.get("z"), 0.0)));
                }
            }
        }
        out.push(Marker {
            gameplay_id: gid.to_string(),
            engine: [x, y, -z],
            json: json!({
                "gameplay_id": gid,
                "engine": eng(x, y, z),
                "axes": Value::Object(axes),
                "tag": el.tag,
                "category": categorise(gid),
            }),
        });
    }
    Ok(out)
}

/// Walk `el`, collecting TriggerZones with the name of their enclosing Activity.
fn collect_zones<'a>(el: &'a El, activity: Option<&'a str>, out: &mut Vec<(&'a El, Option<&'a str>)>) {
    for k in &el.kids {
        if k.tag == "TriggerZone" {
            out.push((k, activity));
        }
        let act = if k.tag == "Activity" { k.get("name").or(activity) } else { activity };
        collect_zones(k, act, out);
    }
}

struct Resolver {
    exact: HashMap<String, usize>,
    ci: HashMap<String, usize>,
}

impl Resolver {
    fn new(m: &[Marker]) -> Self {
        let mut exact = HashMap::new();
        let mut ci = HashMap::new();
        for (i, mk) in m.iter().enumerate() {
            exact.insert(mk.gameplay_id.clone(), i);
            ci.entry(mk.gameplay_id.to_lowercase()).or_insert(i);
        }
        Resolver { exact, ci }
    }

    fn find(&self, obj: &str) -> Option<usize> {
        self.exact.get(obj).or_else(|| self.ci.get(&obj.to_lowercase())).copied()
    }
}

fn bool_or_str(s: Option<&str>) -> Value {
    match s {
        Some("true") => Value::Bool(true),
        Some("false") => Value::Bool(false),
        other => opt_str(other),
    }
}

/// (zones, animations) of one flow XML.
fn triggers_of(root: &El, source: &str, res: &Resolver, marks: &[Marker], zones: &mut Vec<Value>, anims: &mut Vec<Value>) {
    let mut found = Vec::new();
    collect_zones(root, None, &mut found);
    for (tz, activity) in found {
        if let (Some(x), Some(y), Some(z)) = (tz.get("x"), tz.get("y"), tz.get("z")) {
            anims.push(json!({
                "source": source,
                "name": opt_str(activity.or(tz.get("name"))),
                "cutscene": opt_str(tz.get("cutscene")),
                "engine": eng(fnum(Some(x), 0.0), fnum(Some(y), 0.0), fnum(Some(z), 0.0)),
                "radius": opt_num(tz.get("radius")),
                "preload_radius": opt_num(tz.get("preLoadRadius")),
                "replay_time": opt_num(tz.get("replayTime")),
                "repeatable": bool_or_str(tz.get("repeatable")),
                "attrs": Value::Object(attrs_obj(tz)),
            }));
            continue;
        }
        let Some(obj) = tz.get("object") else { continue };
        let hit = res.find(obj);
        let engine = hit.map_or(Value::Null, |i| json!(marks[i].engine));
        let matched = if hit.is_some() { "exact" } else { "none" };
        zones.push(json!({
            "source": source,
            "activity": opt_str(activity),
            "object": obj,
            "name": opt_str(tz.get("name")),
            "radius": opt_num(tz.get("radius")),
            "max_mph": opt_num(tz.get("maxMPH")),
            "prompt": opt_str(tz.get("prompt")),
            "engine": engine,
            "match": matched,
        }));
    }
}

// ---------------------------------------------------------------- disc access

fn read_text(ar: &mut Archive<File>, e: &Entry) -> Result<String> {
    let bytes = ar.read(e).with_context(|| format!("reading {} from {GAMEMODES_ZIP}", e.name))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// (rank, group) of a cutscene XML in gamemodes.zip (lower-cased, '/'-separated name).
fn classify_cutscene(lname: &str) -> Option<(u8, &'static str)> {
    if let Some(rest) = lname.strip_prefix("colorado/cutscenes/") {
        if !rest.ends_with(".xml") {
            return None;
        }
        return match rest.split_once('/') {
            None => Some((0, "root")),
            Some(("prhubs", r)) if !r.contains('/') => Some((1, "PRHubs")),
            Some(("tracks", r)) if !r.contains('/') => Some((2, "Tracks")),
            _ => None,
        };
    }
    (lname == "testbed/cutscenes/cutscenes_default.xml").then_some((3, "TestBed"))
}

/// Rank of a state-flow / activity XML whose TriggerZones the reference script scans.
fn classify_flow(lname: &str) -> Option<u8> {
    if !lname.ends_with(".xml") {
        return None;
    }
    if !lname.contains('/') {
        return Some(0);
    }
    if let Some(rest) = lname.strip_prefix("colorado/animations/") {
        return (!rest.contains('/')).then_some(2);
    }
    if let Some(rest) = lname.strip_prefix("colorado/") {
        return (!rest.contains('/')).then_some(1);
    }
    None
}

fn safe_file_name(name: &str) -> String {
    name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect()
}

fn write_json(path: &Path, v: &Value) -> Result<usize> {
    let bytes = serde_json::to_vec(v)?;
    std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(bytes.len())
}

#[derive(Debug, Default)]
pub struct Stats {
    /// Cutscenes parsed from the disc (before name de-duplication).
    pub cutscenes_parsed: usize,
    /// Files written (unique names).
    pub cutscenes_written: usize,
    pub duplicate_names: Vec<String>,
    pub cams: usize,
    /// Cam keys over every parsed cutscene (duplicates included).
    pub keys: usize,
    pub challenges: usize,
    pub zones: usize,
    pub zones_resolved: usize,
    pub animations: usize,
    pub markers: usize,
}

pub fn build_stats(disc: &Path, out: &Path) -> Result<Stats> {
    let mut stats = Stats::default();
    let mut ar = Archive::open(disc.join(GAMEMODES_ZIP)).with_context(|| format!("FH1 disc is missing {GAMEMODES_ZIP}"))?;
    let entries: Vec<Entry> = ar.entries.clone();

    // --- cutscenes
    let mut cs_files: Vec<(u8, String, &'static str, &Entry)> = entries
        .iter()
        .filter_map(|e| {
            let l = e.name.replace('\\', "/").to_lowercase();
            classify_cutscene(&l).map(|(r, g)| (r, l, g, e))
        })
        .collect();
    cs_files.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    if cs_files.is_empty() {
        bail!("{GAMEMODES_ZIP} holds no Colorado/Cutscenes XML");
    }
    let mut docs: BTreeMap<String, Value> = BTreeMap::new();
    for (_, _, group, e) in &cs_files {
        let text = read_text(&mut ar, e)?;
        let file = format!("{GAMEMODES_ZIP}/{}", e.name.replace('\\', "/"));
        let list = parse_cutscenes(&text, &file, group).with_context(|| format!("parsing {}", e.name))?;
        for doc in list {
            let Some(name) = doc["name"].as_str().map(str::to_owned) else {
                eprintln!("  story: cutscene without a name in {}, skipped", e.name);
                continue;
            };
            let (c, k) = doc_counts(&doc);
            stats.cutscenes_parsed += 1;
            stats.cams += c;
            stats.keys += k;
            if let Some(prev) = docs.insert(name.clone(), doc) {
                println!("  story: duplicate cutscene name {name:?} (was {}); last wins", prev["file"].as_str().unwrap_or("?"));
                stats.duplicate_names.push(name);
            }
        }
    }
    let cdir = out.join("cutscenes");
    std::fs::create_dir_all(&cdir)?;
    let mut index = Vec::with_capacity(docs.len());
    let mut used: HashMap<String, String> = HashMap::new();
    for (name, doc) in &docs {
        let mut fname = safe_file_name(name);
        if fname.eq_ignore_ascii_case("index") {
            fname = format!("{fname}_cutscene");
        }
        if let Some(other) = used.insert(fname.to_lowercase(), name.clone()) {
            bail!("cutscene names {other:?} and {name:?} map to the same file {fname}.json");
        }
        write_json(&cdir.join(format!("{fname}.json")), doc)?;
        let (c, k) = doc_counts(doc);
        index.push(json!({
            "name": name,
            "group": doc["group"],
            "duration_s": doc["duration_s"],
            "cams": c,
            "keys": k,
            "playback_mode": doc["playback_mode"],
        }));
    }
    stats.cutscenes_written = docs.len();
    write_json(&cdir.join("index.json"), &Value::Array(index))?;

    // --- airborne challenges
    let air_xml = std::fs::read(disc.join(AIRBORNE_XML)).with_context(|| format!("FH1 disc is missing {AIRBORNE_XML}"))?;
    let challenges = airborne_json(&String::from_utf8_lossy(&air_xml))?;
    stats.challenges = challenges.len();
    write_json(&out.join("airborne.json"), &json!({ "challenges": challenges }))?;

    // --- triggers
    let obj_xml = std::fs::read(disc.join(GAMEOBJS_XML)).with_context(|| format!("FH1 disc is missing {GAMEOBJS_XML}"))?;
    let marks = markers(&String::from_utf8_lossy(&obj_xml))?;
    let res = Resolver::new(&marks);
    let mut flow_files: Vec<(u8, String, &Entry)> = entries
        .iter()
        .filter_map(|e| {
            let l = e.name.replace('\\', "/").to_lowercase();
            classify_flow(&l).map(|r| (r, l, e))
        })
        .collect();
    flow_files.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let (mut zones, mut anims) = (Vec::new(), Vec::new());
    for (_, _, e) in &flow_files {
        let text = read_text(&mut ar, e)?;
        let source = format!("{GAMEMODES_ZIP}/{}", e.name.replace('\\', "/"));
        match parse(&text) {
            Ok(root) => triggers_of(&root, &source, &res, &marks, &mut zones, &mut anims),
            Err(err) => eprintln!("  story: {} unparsable, TriggerZones skipped: {err:#}", e.name),
        }
    }
    stats.zones = zones.len();
    stats.zones_resolved = zones.iter().filter(|z| z["match"] == "exact").count();
    stats.animations = anims.len();
    stats.markers = marks.len();
    let marker_json: Vec<Value> = marks.into_iter().map(|m| m.json).collect();
    write_json(&out.join("triggers.json"), &json!({ "zones": zones, "animations": anims, "markers": marker_json }))?;
    Ok(stats)
}

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let s = build_stats(disc, out)?;
    println!(
        "  story: {} cutscenes ({} cams, {} keys), {} airborne challenges, {} trigger zones ({} resolved), {} animation triggers, {} markers",
        s.cutscenes_written, s.cams, s.keys, s.challenges, s.zones, s.zones_resolved, s.animations, s.markers
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SNIPPET: &str = r#"<CutsceneManager>
  <Cutscene name="T" PlaybackMode="Normal" Duration="2.5" MirrorForRightHandDrive="0" LoopFrom="0.0">
    <Cams CutQueueType="Cutscene" NumCams="1">
      <Cam Version="3" Group="0" Type="Animateable" StartCut="0.0" TargetName="N" duration="2.5" DisableCarRendering="1">
        <Anim PosAnimType="CarSpace" RotAnimType="CarSpace" TimeAnimType="AffectsPath" IsInCockpit="0" EaseIn="0.5" EaseOut="0.25" NumKeys="2">
          <Key Time="0.0" PosX="1" PosY="2" PosZ="3" Yaw="10" Pitch="-5" Roll="0" FOV="50" TimeC0="0"/>
          <Key Time="1.5" PosX="1.5" PosY="2" PosZ="-3" Yaw="11" Pitch="-5.5" Roll="0" FOV="40"/>
        </Anim>
        <PostEffectsAnim DOFMode="Manual" NumKeys="1"><Key Time="0.0" Blur="0.5" VignetteTopName=""/></PostEffectsAnim>
        <FadeInOptions Duration="2.0" HoldDuration="0.5" CurveType="S" Color.x="0" Color.y="0" Color.z="1" Color.w="0"/>
      </Cam>
    </Cams>
    <Events>
      <Event time="1.0"><EventTrigger name="a" id="CCutsceneAudioTrigger" x="1" y="2" z="3" parent="P"/></Event>
      <Event time="2.0"><EventTrigger name="b" id="CCutsceneShowDriverTrigger" Hide="1"/></Event>
    </Events>
  </Cutscene>
</CutsceneManager>"#;

    #[test]
    fn snippet_cutscene() {
        let v = parse_cutscenes(SNIPPET, "f.xml", "root").unwrap();
        assert_eq!(v.len(), 1);
        let c = &v[0];
        assert_eq!(c["name"], "T");
        assert_eq!(c["group"], "root");
        assert_eq!(c["playback_mode"], "Normal");
        assert_eq!(c["duration_s"].as_f64(), Some(2.5));
        assert_eq!(c["mirror_rhd"], json!(0));
        let cam = &c["cams"][0];
        assert_eq!(cam["type"], "Animateable");
        assert_eq!(cam["target_name"], "N");
        assert_eq!(cam["pos_space"], "CarSpace");
        assert_eq!(cam["disable_car_rendering"], json!(1));
        assert_eq!(cam["ease_in"].as_f64(), Some(0.5));
        assert_eq!(cam["fade_in"]["color"], json!([0.0, 0.0, 1.0]));
        assert_eq!(cam["fade_in"]["curve"], "S");
        assert!(cam["fade_out"].is_null());
        let keys = cam["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0]["pos"], json!([1.0, 2.0, -3.0]));
        assert_eq!(keys[1]["pos"], json!([1.5, 2.0, 3.0]));
        assert_eq!(keys[0]["yaw"].as_f64(), Some(10.0));
        assert_eq!(keys[1]["pitch"].as_f64(), Some(-5.5));
        assert_eq!(keys[0]["fov"].as_f64(), Some(50.0));
        assert_eq!(keys[0]["extra"]["TimeC0"], json!(0));
        assert!(keys[1].get("extra").is_none());
        assert_eq!(cam["post_keys"][0]["Blur"].as_f64(), Some(0.5));
        assert_eq!(cam["raw"]["Version"], json!(3));
        assert_eq!(cam["raw"]["NumKeys"], json!(2));
        assert!(cam["raw"].get("Type").is_none());
        let ev = c["events"].as_array().unwrap();
        assert_eq!(ev.len(), 2);
        let t = &ev[0]["triggers"][0];
        assert_eq!(t["class"], "CCutsceneAudioTrigger");
        assert_eq!(t["parent"], "P");
        assert_eq!(t["engine"], json!([1.0, 2.0, -3.0]));
        assert_eq!(t["attrs"]["x"], json!(1));
        assert!(ev[1]["triggers"][0]["engine"].is_null());
        assert!(ev[1]["triggers"][0]["parent"].is_null());
        assert_eq!(doc_counts(c), (1, 2));
    }

    #[test]
    fn repairs_malformed_close_tag() {
        let xml = "<A><DifficulySettings><AnimSpeeds base_mult=\"1\"/></DifficulySettings\n</A>";
        assert!(parse(xml).is_err());
        let root = parse(&repair_tags(xml)).unwrap();
        assert_eq!(root.kids[0].tag, "DifficulySettings");
    }

    #[test]
    fn num_matches_reference() {
        assert_eq!(num("3"), json!(3));
        assert_eq!(num("3.0").as_f64(), Some(3.0));
        assert!(num("3.0").is_f64());
        assert_eq!(num("abc"), json!("abc"));
        assert_eq!(num("nan"), json!("nan"));
        assert_eq!(num(""), json!(""));
    }

    #[test]
    fn zones_resolve_case_insensitively() {
        let marks = vec![Marker { gameplay_id: "Festival_01".into(), engine: [1.0, 2.0, -3.0], json: Value::Null }];
        let res = Resolver::new(&marks);
        assert_eq!(res.find("Festival_01"), Some(0));
        assert_eq!(res.find("festival_01"), Some(0));
        assert_eq!(res.find("festival_07"), None);
        let xml = r#"<ActivityManager><Activity type="X" name="Act1"><TriggerZone object="festival_01" name="z" radius="3" maxMPH="10" prompt="P"/>
            <TriggerZone object="nope" radius="1"/><TriggerZone x="1" y="2" z="3" radius="10" preLoadRadius="20" repeatable="false" replayTime="10" cutscene="C"/></Activity></ActivityManager>"#;
        let root = parse(xml).unwrap();
        let (mut z, mut a) = (Vec::new(), Vec::new());
        triggers_of(&root, "s", &res, &marks, &mut z, &mut a);
        assert_eq!(z.len(), 2);
        assert_eq!(z[0]["match"], "exact");
        assert_eq!(z[0]["engine"], json!([1.0, 2.0, -3.0]));
        assert_eq!(z[0]["activity"], "Act1");
        assert_eq!(z[1]["match"], "none");
        assert!(z[1]["engine"].is_null());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0]["engine"], json!([1.0, 2.0, -3.0]));
        assert_eq!(a[0]["repeatable"], json!(false));
        assert_eq!(a[0]["preload_radius"], json!(20));
    }

    fn disc_root() -> Option<PathBuf> {
        let root = std::env::var_os("FH1_DISC")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
        root.join("media").is_dir().then_some(root)
    }

    fn read_json(p: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    }

    fn close(a: &Value, b: &Value) -> bool {
        match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => (x - y).abs() < 1e-4,
            _ => a.is_null() && b.is_null(),
        }
    }

    fn compare_cutscene(out: &Path, tracks: &[Value], name: &str) {
        let doc = read_json(&out.join("cutscenes").join(format!("{name}.json")));
        let reference: Vec<&Value> = tracks.iter().filter(|t| t["cutscene"] == name).collect();
        let cams = doc["cams"].as_array().unwrap();
        assert_eq!(cams.len(), reference.len(), "{name}: cam count");
        for (i, (cam, r)) in cams.iter().zip(&reference).enumerate() {
            let rk = r["cam"]["keys"].as_array().unwrap();
            let keys = cam["keys"].as_array().unwrap();
            assert_eq!(keys.len(), rk.len(), "{name} cam {i}: key count");
            for (j, (k, g)) in keys.iter().zip(rk).enumerate() {
                for f in ["t", "yaw", "pitch", "roll", "fov"] {
                    assert!(close(&k[f], &g[f]), "{name} cam {i} key {j} {f}: {} vs {}", k[f], g[f]);
                }
                for a in 0..3 {
                    assert!(close(&k["pos"][a], &g["pos"][a]), "{name} cam {i} key {j} pos[{a}]");
                }
            }
        }
    }

    #[test]
    fn disc_story() {
        let Some(disc) = disc_root() else {
            eprintln!("story: disc absent, skipping");
            return;
        };
        let out = std::env::temp_dir().join(format!("fh1_story_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let s = build_stats(&disc, &out).unwrap();
        assert_eq!(s.cutscenes_parsed, 576);
        assert_eq!(s.keys, 20_316);
        assert_eq!(s.challenges, 7);
        assert!(s.markers > 1000);
        assert!(s.zones > 0 && s.zones_resolved > 0 && s.zones_resolved < s.zones);

        let index = read_json(&out.join("cutscenes/index.json"));
        let index = index.as_array().unwrap();
        assert_eq!(index.len(), s.cutscenes_written);
        let names: Vec<&str> = index.iter().map(|e| e["name"].as_str().unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);

        let open = read_json(&out.join("cutscenes/Opening_Cutscene.json"));
        assert_eq!(open["duration_s"].as_f64(), Some(39.0));
        assert_eq!(doc_counts(&open), (11, 202));

        let air = read_json(&out.join("airborne.json"));
        let mut ids: Vec<i64> = air["challenges"].as_array().unwrap().iter().map(|c| c["event_id"].as_i64().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, vec![101, 168, 172, 220, 222, 223, 246]);

        let trig = read_json(&out.join("triggers.json"));
        assert!(trig["animations"].as_array().unwrap().len() >= 2);
        let unresolved: Vec<&str> =
            trig["zones"].as_array().unwrap().iter().filter(|z| z["engine"].is_null()).map(|z| z["object"].as_str().unwrap()).collect();
        assert!(unresolved.iter().any(|o| o.starts_with("festival_")), "{unresolved:?}");

        // Reference output of the Python script (dev-only data/extracted), when present.
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/extracted/story");
        if base.join("camera_tracks.json").exists() && base.join("cutscenes.json").exists() {
            let tracks = read_json(&base.join("camera_tracks.json"));
            let tracks = tracks["tracks"].as_array().unwrap().clone();
            let refc = read_json(&base.join("cutscenes.json"));
            let refc = refc["cutscenes"].as_array().unwrap();
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for c in refc {
                *counts.entry(c["name"].as_str().unwrap()).or_default() += 1;
            }
            let mut picks: Vec<&str> = vec!["Opening_Cutscene"];
            let cand: Vec<&str> = refc
                .iter()
                .filter(|c| c["key_count"].as_i64().unwrap_or(0) > 0 && counts[c["name"].as_str().unwrap()] == 1)
                .map(|c| c["name"].as_str().unwrap())
                .filter(|n| *n != "Opening_Cutscene")
                .collect();
            picks.push(cand[0]);
            picks.push(cand[cand.len() - 1]);
            for n in picks {
                compare_cutscene(&out, &tracks, n);
            }
            let ref_air = read_json(&base.join("challenges.json"));
            assert_eq!(air["challenges"], ref_air["airborne_challenges"], "airborne challenges differ from the reference");
        } else {
            eprintln!("story: reference data absent, skipping comparison");
        }
        let _ = std::fs::remove_dir_all(&out);
    }
}
