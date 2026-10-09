//! UI sound events: `ui4audio.xml` -> `ui/ui4audio.json` (setup), and play key -> FSB sample (runtime).
//!
//! The XML is not well-formed (duplicate `cue` attributes, a `ggroup` typo), so [`parse`] is a tolerant
//! hand-written port of the Python regex tokenizer in `convert.py` (`convert_ui`). Values stay raw (no unescaping).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One event as ordered key/value pairs (serde_json's `Map` would sort the keys without `preserve_order`).
pub type Row = Vec<(String, Value)>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SceneOverride {
    pub scene: String,
    pub r#override: String,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub events: usize,
    pub defaults: usize,
    pub overrides: usize,
    pub non_blank: usize,
    pub looping: usize,
    pub with_duration: usize,
    pub scene_overrides: usize,
    pub cues_matching_sample: usize,
    pub cues_distinct_non_blank: usize,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} events ({} defaults, {} override rows), {} non-Blank, {} looping, {} with duration, {} scene overrides, {}/{} distinct non-Blank cues equal a sample name",
            self.events, self.defaults, self.overrides, self.non_blank, self.looping, self.with_duration,
            self.scene_overrides, self.cues_matching_sample, self.cues_distinct_non_blank
        )
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

struct Tag<'a> {
    end: usize,
    close: bool,
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
}

/// Matches `<(/?)([A-Za-z_]\w*)((?:\s+[\w.:-]+\s*=\s*"[^"]*")*)\s*/?>` at `i` (which is a '<').
fn try_tag(text: &str, i: usize) -> Option<Tag<'_>> {
    let b = text.as_bytes();
    let n = b.len();
    let mut p = i + 1;
    let close = p < n && b[p] == b'/';
    if close {
        p += 1;
    }
    if p >= n || !(b[p].is_ascii_alphabetic() || b[p] == b'_') {
        return None;
    }
    let ns = p;
    while p < n && is_word(b[p]) {
        p += 1;
    }
    let name = &text[ns..p];
    let mut attrs = Vec::new();
    loop {
        let mut q = p;
        while q < n && is_ws(b[q]) {
            q += 1;
        }
        if q == p {
            break;
        }
        let ks = q;
        while q < n && (is_word(b[q]) || matches!(b[q], b'.' | b':' | b'-')) {
            q += 1;
        }
        if q == ks {
            break;
        }
        let key = &text[ks..q];
        while q < n && is_ws(b[q]) {
            q += 1;
        }
        if q >= n || b[q] != b'=' {
            break;
        }
        q += 1;
        while q < n && is_ws(b[q]) {
            q += 1;
        }
        if q >= n || b[q] != b'"' {
            break;
        }
        q += 1;
        let vs = q;
        while q < n && b[q] != b'"' {
            q += 1;
        }
        if q >= n {
            break;
        }
        attrs.push((key, &text[vs..q]));
        p = q + 1;
    }
    while p < n && is_ws(b[p]) {
        p += 1;
    }
    if p < n && b[p] == b'/' {
        p += 1;
    }
    if p < n && b[p] == b'>' {
        Some(Tag { end: p + 1, close, name, attrs })
    } else {
        None
    }
}

fn attr_last<'a>(attrs: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    attrs.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Tolerant tokenizer port of `convert.py`. Events keep XML attribute order (last value wins, first position kept),
/// then `scope`, `section`, `line`, and `duplicate_attributes` (object of key -> all values) only when present.
pub fn parse(xml: &str) -> Result<(Vec<Row>, Vec<SceneOverride>)> {
    let b = xml.as_bytes();
    let mut events: Vec<Row> = Vec::new();
    let mut scenes = Vec::new();
    let mut scope: Option<String> = Some("defaults".into());
    let mut section: Option<String> = None;
    let mut line = 1usize;
    let mut counted = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("<!--") {
            if let Some(off) = xml[i + 4..].find("-->") {
                section = Some(xml[i + 4..i + 4 + off].trim().to_string());
                i = i + 4 + off + 3;
                continue;
            }
        }
        let Some(tag) = try_tag(xml, i) else {
            i += 1;
            continue;
        };
        if tag.close {
            if tag.name == "defaults" || tag.name == "override" {
                scope = None;
            }
        } else if tag.name == "defaults" {
            scope = Some("defaults".into());
        } else if tag.name == "override" {
            let n = attr_last(&tag.attrs, "name").ok_or_else(|| anyhow!("<override> without name at byte {i}"))?;
            scope = Some(format!("override:{n}"));
        } else if tag.name == "scene" {
            let s = attr_last(&tag.attrs, "name").ok_or_else(|| anyhow!("<scene> without name at byte {i}"))?;
            let o = attr_last(&tag.attrs, "override").ok_or_else(|| anyhow!("<scene> without override at byte {i}"))?;
            scenes.push(SceneOverride { scene: s.to_string(), r#override: o.to_string() });
        } else if tag.name == "event" {
            let mut row: Row = Vec::new();
            let mut dups: Vec<(String, Vec<Value>)> = Vec::new();
            for (k, v) in &tag.attrs {
                if let Some(pos) = row.iter().position(|(rk, _)| rk.as_str() == *k) {
                    let first = row[pos].1.clone();
                    match dups.iter_mut().find(|(dk, _)| dk.as_str() == *k) {
                        Some((_, list)) => list.push(Value::String(v.to_string())),
                        None => dups.push((k.to_string(), vec![first, Value::String(v.to_string())])),
                    }
                    row[pos].1 = Value::String(v.to_string());
                } else {
                    row.push((k.to_string(), Value::String(v.to_string())));
                }
            }
            line += b[counted..i].iter().filter(|&&c| c == b'\n').count();
            counted = i;
            row.push(("scope".into(), scope.clone().map_or(Value::Null, Value::String)));
            row.push(("section".into(), section.clone().map_or(Value::Null, Value::String)));
            row.push(("line".into(), Value::from(line)));
            if !dups.is_empty() {
                let mut m = serde_json::Map::new();
                for (k, list) in dups {
                    m.insert(k, Value::Array(list));
                }
                row.push(("duplicate_attributes".into(), Value::Object(m)));
            }
            events.push(row);
        }
        i = tag.end;
    }
    Ok((events, scenes))
}

struct Ordered<'a>(&'a Row);

impl Serialize for Ordered<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

fn row_str<'a>(r: &'a Row, key: &str) -> Option<&'a str> {
    r.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_str())
}

/// Writes `ui4audio.json` and `ui4audio_scene_overrides.json` into `out_ui_dir`.
/// `sample_names`: bank file name (`UIInGame.fsb`) -> FSB sample names in header order.
pub fn write(out_ui_dir: &Path, ui4audio_xml: &str, sample_names: &BTreeMap<String, Vec<String>>) -> Result<Stats> {
    let (events, scenes) = parse(ui4audio_xml)?;
    std::fs::create_dir_all(out_ui_dir).with_context(|| out_ui_dir.display().to_string())?;
    let ordered: Vec<Ordered> = events.iter().map(Ordered).collect();
    let p = out_ui_dir.join("ui4audio.json");
    std::fs::write(&p, serde_json::to_string_pretty(&ordered)?).with_context(|| p.display().to_string())?;
    let p = out_ui_dir.join("ui4audio_scene_overrides.json");
    std::fs::write(&p, serde_json::to_string_pretty(&scenes)?).with_context(|| p.display().to_string())?;

    let all: HashSet<&str> = sample_names.values().flatten().map(|s| s.as_str()).collect();
    let mut cues: HashSet<&str> = HashSet::new();
    let mut st = Stats { events: events.len(), scene_overrides: scenes.len(), ..Default::default() };
    for r in &events {
        match row_str(r, "scope") {
            Some("defaults") => st.defaults += 1,
            Some(s) if s.starts_with("override:") => st.overrides += 1,
            _ => {}
        }
        if let Some(c) = row_str(r, "cue") {
            if c != "Blank" {
                st.non_blank += 1;
                cues.insert(c);
            }
        }
        if row_str(r, "isLooping") == Some("true") {
            st.looping += 1;
        }
        if r.iter().any(|(k, _)| k == "duration") {
            st.with_duration += 1;
        }
    }
    st.cues_distinct_non_blank = cues.len();
    st.cues_matching_sample = cues.iter().filter(|c| all.contains(*c)).count();
    Ok(st)
}

// ---------------------------------------------------------------- runtime

#[derive(Clone, Debug)]
pub struct UiEvent {
    pub play: String,
    /// `group` or the `ggroup` typo.
    pub group: Option<String>,
    pub cue: String,
    pub looping: bool,
    pub duration_ms: Option<u32>,
    /// "defaults" | "override:PostRace" ...
    pub scope: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    /// Cue "Blank".
    Silent,
    /// `bank` is the FSB stem: "UIInGame" | "UIInGame_Streams" | "General". `gain` (linear) and `pitch` (rate multiplier)
    /// come from the FEV event x wave (1.0 when resolved by name).
    Sample { bank: String, index: usize, looping: bool, gain: f32, pitch: f32 },
    /// Needs the FEV or is not on disc; the caller logs once and stays silent.
    Unresolved { cue: String },
}

/// (group, cue, looping) -> the resolved sound, `None` = not in the FEV (the name rules follow).
type FevResolver = Box<dyn Fn(&str, &str, bool) -> Option<Resolved> + Send + Sync>;

pub struct UiEvents {
    events: Vec<UiEvent>,
    index: HashMap<String, Vec<usize>>,
    scene_map: HashMap<String, String>,
    banks: Vec<(String, Vec<String>)>,
    general: Vec<String>,
    fev: Option<FevResolver>,
}

const BANK_ORDER: [&str; 2] = ["UIInGame", "UIInGame_Streams"];

fn bank_stem(name: &str) -> &str {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    name.strip_suffix(".fsb").or_else(|| name.strip_suffix(".FSB")).unwrap_or(name)
}

fn match_name(names: &[String], cue: &str) -> Option<usize> {
    if let Some(i) = names.iter().position(|n| n == cue) {
        return Some(i);
    }
    if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(cue)) {
        return Some(i);
    }
    let lc = cue.to_ascii_lowercase();
    names.iter().position(|n| n.len() == 30 && lc.starts_with(&n.to_ascii_lowercase()))
}

fn event_from_value(v: &Value) -> Option<UiEvent> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    Some(UiEvent {
        play: s("play")?,
        group: s("group").or_else(|| s("ggroup")),
        cue: s("cue").unwrap_or_default(),
        looping: v.get("isLooping").and_then(|x| x.as_str()) == Some("true"),
        duration_ms: v.get("duration").and_then(|x| x.as_str()).and_then(|x| x.trim().parse().ok()),
        scope: s("scope"),
    })
}

/// One play of a FEV event (site-73's rule, sfx_bank::rolls_silence): a weighted random wave, silent when the roll lands
/// on the event's silence weight; gain = event volume x wave gain, pitch = event pitch x wave pitch. The wave index is
/// trusted over its name (42 UIInGame refs carry stale names). `r` in [0, 1).
fn fev_pick(e: &crate::fev::Event, looping: bool, r: f32) -> Option<Resolved> {
    let total: f32 = e.waves.iter().map(|w| w.weight.max(0.0)).sum();
    if e.waves.is_empty() {
        return None;
    }
    let silence = e.silence_weight.max(0.0);
    let mut x = r * (total + silence);
    if x < silence {
        return Some(Resolved::Silent);
    }
    x -= silence;
    let w = e.waves.iter().find(|w| {
        x -= w.weight.max(0.0);
        x < 0.0
    });
    let w = w.unwrap_or(&e.waves[e.waves.len() - 1]);
    Some(Resolved::Sample { bank: w.bank.clone(), index: w.index as usize, looping, gain: e.volume * w.gain, pitch: e.pitch * w.pitch })
}

/// Cheap process-wide uniform [0, 1) (xorshift; only picks variants).
fn rand01() -> f32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static S: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let mut x = S.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    S.store(x, Ordering::Relaxed);
    (x >> 40) as f32 / (1u64 << 24) as f32
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    let text = std::fs::read_to_string(p).with_context(|| p.display().to_string())?;
    serde_json::from_str(&text).with_context(|| p.display().to_string())
}

impl UiEvents {
    /// Reads `ui/ui4audio.json`, `ui/ui4audio_scene_overrides.json`, `ui/sample_names.json`, and `banks/General.json` if present.
    pub fn load(audio_dir: &Path) -> Result<UiEvents> {
        let mut ev = Self::load_ui_dir(&audio_dir.join("ui"))?;
        let gp = audio_dir.join("banks").join("General.json");
        if gp.is_file() {
            if let Ok(v) = read_json::<Value>(&gp) {
                if let Some(arr) = v.get("samples").and_then(|s| s.as_array()) {
                    ev.general = arr.iter().filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(String::from)).collect();
                }
            }
        }
        // The FEV event tree (site-73's fev.rs) resolves every cue: `group/cue` (e.g. `UIInGame/UI/Nav/Accept`) -> the
        // event's first wave (bank stem + FSB index; the index is right even where the wave name is stale).
        // FH1_UI_FEV=0 = name matching only.
        if std::env::var("FH1_UI_FEV").map_or(true, |v| v != "0") {
            let fevs: Vec<crate::fev::Fev> = ["UIInGame.fev", "General.fev"]
                .iter()
                .filter_map(|n| {
                    let p = audio_dir.join("fev").join(n);
                    p.is_file().then(|| crate::fev::Fev::load(&p).map_err(|e| eprintln!("fh1-audio ui: {e:#}")).ok()).flatten()
                })
                .collect();
            if !fevs.is_empty() {
                ev.set_fev_resolver(Box::new(move |group: &str, cue: &str, looping: bool| {
                    let path = format!("{group}/{cue}");
                    let e = fevs.iter().find_map(|f| f.event(&path).or_else(|| f.event(cue)))?;
                    fev_pick(e, looping, rand01())
                }));
            }
        }
        Ok(ev)
    }

    /// Reads just the three files in a `ui` directory (no General bank).
    pub fn load_ui_dir(ui_dir: &Path) -> Result<UiEvents> {
        let rows: Vec<Value> = read_json(&ui_dir.join("ui4audio.json"))?;
        let events: Vec<UiEvent> = rows.iter().filter_map(event_from_value).collect();
        let scenes: Vec<SceneOverride> = read_json(&ui_dir.join("ui4audio_scene_overrides.json"))?;
        let names: BTreeMap<String, Vec<String>> = read_json(&ui_dir.join("sample_names.json"))?;
        Ok(Self::from_parts(events, scenes, names))
    }

    pub fn from_parts(events: Vec<UiEvent>, scene_overrides: Vec<SceneOverride>, sample_names: BTreeMap<String, Vec<String>>) -> UiEvents {
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in events.iter().enumerate() {
            index.entry(e.play.clone()).or_default().push(i);
        }
        let scene_map = scene_overrides.into_iter().map(|s| (s.scene, s.r#override)).collect();
        let mut by_stem: BTreeMap<String, Vec<String>> = sample_names.into_iter().map(|(k, v)| (bank_stem(&k).to_string(), v)).collect();
        let mut banks = Vec::new();
        for b in BANK_ORDER {
            if let Some(v) = by_stem.remove(b) {
                banks.push((b.to_string(), v));
            }
        }
        banks.extend(by_stem);
        UiEvents { events, index, scene_map, banks, general: Vec::new(), fev: None }
    }

    /// Hook tried before the name rules: (group, cue, looping) -> the sound (or [`Resolved::Silent`] for a silence roll).
    pub fn set_fev_resolver(&mut self, f: FevResolver) {
        self.fev = Some(f);
    }

    /// The event for `play` in `scene` (e.g. "924_POST_RACE"; None = defaults).
    pub fn event(&self, play: &str, scene: Option<&str>) -> Option<&UiEvent> {
        let idxs = self.index.get(play)?;
        if let Some(ov) = scene.and_then(|s| self.scene_map.get(s)) {
            let want = format!("override:{ov}");
            if let Some(&i) = idxs.iter().rev().find(|&&i| self.events[i].scope.as_deref() == Some(want.as_str())) {
                return Some(&self.events[i]);
            }
        }
        idxs.iter().rev().find(|&&i| self.events[i].scope.as_deref() == Some("defaults")).map(|&i| &self.events[i])
    }

    pub fn resolve(&self, play: &str, scene: Option<&str>) -> Resolved {
        match self.event(play, scene) {
            Some(e) => self.resolve_inner(e.group.as_deref(), &e.cue, e.looping),
            None => Resolved::Unresolved { cue: play.to_string() },
        }
    }

    pub fn resolve_cue(&self, cue: &str, looping: bool) -> Resolved {
        self.resolve_inner(None, cue, looping)
    }

    fn resolve_inner(&self, group: Option<&str>, cue: &str, looping: bool) -> Resolved {
        if cue == "Blank" {
            return Resolved::Silent;
        }
        if let (Some(f), Some(g)) = (&self.fev, group) {
            if let Some(r) = f(g, cue, looping) {
                return r;
            }
        }
        for (stem, names) in &self.banks {
            if let Some(index) = match_name(names, cue) {
                return Resolved::Sample { bank: stem.clone(), index, looping, gain: 1.0, pitch: 1.0 };
            }
        }
        let prefix = match cue {
            "ShutterSound" => Some("Shutter_Sound"),
            "AverageStart" => Some("SpeedCamera_Average"),
            _ => None,
        };
        if let Some(p) = prefix {
            if let Some(index) = self.general.iter().position(|n| n.starts_with(p)) {
                return Resolved::Sample { bank: "General".into(), index, looping, gain: 1.0, pitch: 1.0 };
            }
        }
        Resolved::Unresolved { cue: cue.to_string() }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn plays(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn get<'a>(r: &'a Row, k: &str) -> Option<&'a Value> {
        r.iter().find(|(rk, _)| rk == k).map(|(_, v)| v)
    }

    #[test]
    fn parse_small() {
        let xml = "<root>\n<defaults>\n<!-- Nav -->\n<event play=\"A\" cue=\"Blank\" group=\"g\" cue=\"X\" isLooping=\"true\"/>\n< junk <event play=\"B\" ggroup=\"h\" cue=\"Y\" />\n</defaults>\n<event play=\"Out\" cue=\"Z\"/>\n<overrides><override name=\"Showroom\">\n<event play=\"A\" cue=\"Blank\"/></override></overrides>\n<scene_overrides><scene name=\"s1\" override=\"Showroom\" /></scene_overrides></root>";
        let (ev, sc) = parse(xml).unwrap();
        assert_eq!(ev.len(), 4);
        assert_eq!(sc, vec![SceneOverride { scene: "s1".into(), r#override: "Showroom".into() }]);
        let keys: Vec<&str> = ev[0].iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["play", "cue", "group", "isLooping", "scope", "section", "line", "duplicate_attributes"]);
        assert_eq!(get(&ev[0], "cue"), Some(&Value::String("X".into())));
        assert_eq!(get(&ev[0], "duplicate_attributes").unwrap()["cue"], serde_json::json!(["Blank", "X"]));
        assert_eq!(get(&ev[0], "section"), Some(&Value::String("Nav".into())));
        assert_eq!(get(&ev[0], "line"), Some(&Value::from(4)));
        assert!(get(&ev[1], "ggroup").is_some() && get(&ev[1], "group").is_none());
        assert_eq!(get(&ev[2], "scope"), Some(&Value::Null));
        assert_eq!(get(&ev[3], "scope"), Some(&Value::String("override:Showroom".into())));
    }

    fn disc_xml() -> Option<String> {
        let mut c = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../disc/media/audio/UI/ui4audio.xml")];
        if let Ok(d) = std::env::var("FH1_DISC") {
            c.push(PathBuf::from(d).join("media/audio/UI/ui4audio.xml"));
        }
        c.into_iter().find(|p| p.is_file()).and_then(|p| std::fs::read_to_string(p).ok())
    }

    #[test]
    fn parse_disc() {
        let Some(xml) = disc_xml() else { return };
        let (ev, sc) = parse(&xml).unwrap();
        assert_eq!(ev.len(), 762);
        assert_eq!(sc.len(), 3);
        let n = |s: &str| ev.iter().filter(|r| row_str(r, "scope") == Some(s)).count();
        assert_eq!(n("defaults"), 757);
        assert_eq!(n("override:PostRace"), 2);
        assert_eq!(n("override:Trophy"), 2);
        assert_eq!(n("override:Showroom"), 1);
        assert_eq!(ev.iter().filter(|r| row_str(r, "isLooping") == Some("true")).count(), 14);
        assert_eq!(ev.iter().filter(|r| get(r, "duration").is_some()).count(), 141);
        let a = ev.iter().find(|r| row_str(r, "play") == Some("VO_SEASON_OUT_Amateur")).unwrap();
        assert_eq!(row_str(a, "cue"), Some("Amateur"));
        assert_eq!(get(a, "duplicate_attributes").unwrap()["cue"], serde_json::json!(["Blank", "Amateur"]));
        let o = ev.iter().find(|r| row_str(r, "play") == Some("VO_PopUp_OthersStorefront")).unwrap();
        assert!(get(o, "ggroup").is_some() && get(o, "group").is_none());
    }

    fn ev(play: &str, cue: &str, scope: &str) -> UiEvent {
        UiEvent { play: play.into(), group: None, cue: cue.into(), looping: false, duration_ms: None, scope: Some(scope.into()) }
    }

    #[test]
    fn resolve_rules() {
        let long = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123";
        assert_eq!(long.len(), 30);
        let mut names = BTreeMap::new();
        names.insert("UIInGame.fsb".to_string(), vec!["HUD_Pause".to_string(), long.to_string()]);
        names.insert("UIInGame_Streams.fsb".to_string(), vec!["Stream1".to_string()]);
        let events = vec![
            ev("Pause", "HUD_Pause", "defaults"),
            ev("Silent", "Blank", "defaults"),
            ev("Long", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123_Longer", "defaults"),
            ev("Accept", "HUD_Pause", "defaults"),
            ev("Accept", "Blank", "override:Showroom"),
            ev("Str", "stream1", "defaults"),
        ];
        let sc = vec![SceneOverride { scene: "075_SHOWROOM_HOMESPACE".into(), r#override: "Showroom".into() }];
        let u = UiEvents::from_parts(events, sc, names);
        assert_eq!(u.resolve("Pause", None), Resolved::Sample { bank: "UIInGame".into(), index: 0, looping: false, gain: 1.0, pitch: 1.0 });
        assert_eq!(u.resolve("Silent", None), Resolved::Silent);
        assert_eq!(u.resolve("Long", None), Resolved::Sample { bank: "UIInGame".into(), index: 1, looping: false, gain: 1.0, pitch: 1.0 });
        assert_eq!(u.resolve("Str", None), Resolved::Sample { bank: "UIInGame_Streams".into(), index: 0, looping: false, gain: 1.0, pitch: 1.0 });
        assert_eq!(u.resolve("Accept", Some("075_SHOWROOM_HOMESPACE")), Resolved::Silent);
        assert!(matches!(u.resolve("Accept", None), Resolved::Sample { .. }));
        assert_eq!(u.resolve("Nope", None), Resolved::Unresolved { cue: "Nope".into() });
    }

    #[test]
    fn resolve_disc_staging() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/extracted/converted/audio/ui");
        if !dir.join("ui4audio.json").is_file() || !dir.join("sample_names.json").is_file() {
            return;
        }
        let u = UiEvents::load_ui_dir(&dir).unwrap();
        let cues: HashSet<String> = u.events.iter().map(|e| e.cue.clone()).filter(|c| c != "Blank").collect();
        let n = cues.iter().filter(|c| matches!(u.resolve_cue(c, false), Resolved::Sample { .. })).count();
        println!("distinct non-Blank cues resolving to a sample: {n} of {}", cues.len());
        assert!(n >= 187, "only {n}");
    }
}
