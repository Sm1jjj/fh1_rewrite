//! World ambience (W9): Colorado soundscape tiles -> emitters, ambience zones with the speed-gated quad bed, trees,
//! crowds and reverb-zone queries. Plan and tags: data/extracted/plans/world_audio.md, docs/AMBIENCE.md.
//!
//! Data: `<assets>/audio/soundscape/index.json` + all tiles (installed copy, z ALREADY negated) are read on a worker
//! thread when Colorado is the loaded world and re-read on every `WorldGeneration` / map change; they become compact
//! arrays plus 64 m XZ grids. Playback goes through [`SfxBank`] (FEV events, samples, 3D law) on the Ambience bus,
//! 48 voices at most (beds > emitters > crowds > trees, farthest stolen). Bus gain 0 behind pause / covers / mute is
//! sfx_bank's job; this module also stops updating while gated and stops every voice on a map change.
//!
//! The plugin also adds `SfxBankPlugin` (even with `FH1_AMBIENCE=0`, other players need the bank).
//!
//! Flags: `FH1_AMBIENCE=0` off; `FH1_AMB_LOG=1` prints active voices once a second; `FH1_AMB_REFLECTIONS=1` is reserved
//! for the `Default_Reflections_*` whoosh (phase 3, needs a capture) and does nothing yet (those 30,509 points are
//! skipped at load).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use fh1_audio::ambient::{Bus, Pcm, VoiceId, VoiceParams};
use fh1_audio::fev::Fev;
use serde::Deserialize;

use crate::race::{Events, RacePhase, RaceState};
use crate::sfx_bank::{roll_for, spatial_roll, world_sfx_allowed, Listener, Played, Roll, SfxBank, SfxBankPlugin};
use crate::track::Track;
use crate::ui::world_load::WorldGeneration;
use crate::Car;

const CELL: f32 = 64.0;
/// Emitter activation radius (200 m + 20 m hysteresis, world_audio.md section 4).
const EMIT_RADIUS: f32 = 220.0;
const EMIT_MAX: usize = 24;
/// Trees: nearest 16 within ~40 m (lead's brief; the game's own numbers are UNKNOWN).
const TREE_RANGE: f32 = 40.0;
const TREE_MAX: usize = 16;
const CROWD_RANGE: f32 = 60.0;
const CROWD_MAX: usize = 12;
const BUDGET: usize = 48;
/// Leave an event's range at 1.1 x (guess).
const HYST: f32 = 1.1;
const SELECT_S: f32 = 0.1;
const CROSSFADE_S: f32 = 2.0;
/// Template changes after the listener stays in the new zone this long (guess).
const ZONE_DEBOUNCE_S: f32 = 0.5;
const MPS_TO_MPH: f32 = 2.236_936;
const MAX_STARTS_PER_FRAME: usize = 4;

// Active-voice classes = priorities (lower = more important; beds are 0 and live in `Bed`).
const EMITTER: u8 = 1;
const CROWD: u8 = 2;
const TREE: u8 = 3;

pub struct AmbiencePlugin;

impl Plugin for AmbiencePlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<SfxBankPlugin>() {
            app.add_plugins(SfxBankPlugin);
        }
        if std::env::var("FH1_AMBIENCE").is_ok_and(|v| v == "0") {
            return;
        }
        if std::env::var("FH1_AMB_REFLECTIONS").is_ok_and(|v| v == "1") {
            info!("ambience: FH1_AMB_REFLECTIONS=1 is reserved (Default_Reflections_* whoosh, phase 3); nothing to enable yet");
        }
        app.init_resource::<Ambience>().add_systems(Update, ambience_update);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Pure geometry / logic (unit-tested)
// ---------------------------------------------------------------------------------------------------------------

/// Barycentric weights of `p` in the XZ triangle, or None when outside (edges inclusive) or degenerate.
pub fn bary(p: [f32; 2], t: &[[f32; 2]; 3]) -> Option<[f32; 3]> {
    let [a, b, c] = *t;
    let den = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
    if den.abs() < 1e-9 {
        return None;
    }
    let l1 = ((b[1] - c[1]) * (p[0] - c[0]) + (c[0] - b[0]) * (p[1] - c[1])) / den;
    let l2 = ((c[1] - a[1]) * (p[0] - c[0]) + (a[0] - c[0]) * (p[1] - c[1])) / den;
    let l3 = 1.0 - l1 - l2;
    const E: f32 = -1e-5;
    (l1 >= E && l2 >= E && l3 >= E).then_some([l1, l2, l3])
}

pub fn point_in_tri(p: [f32; 2], t: &[[f32; 2]; 3]) -> bool {
    bary(p, t).is_some()
}

/// Bed on/off with hysteresis (world_audio A8): on below `start_below`, off above `stop_above`, held in between.
pub fn schmitt(on: bool, speed_mph: f32, start_below: f32, stop_above: f32) -> bool {
    if on {
        speed_mph <= stop_above
    } else {
        speed_mph < start_below
    }
}

/// Route gate. Empty list = unrestricted (INFERRED: 26,625 of 32,214 trees and crowds have none); a list with 0 = free
/// roam; other ids = the TrackRoute number of the active race (INFERRED).
pub fn route_active(routes: &[u16], race_route: Option<u16>) -> bool {
    routes.is_empty() || routes.contains(&0) || race_route.is_some_and(|r| routes.contains(&r))
}

/// Nearest point of the segment a-b to p and its distance.
pub fn nearest_on_segment(p: Vec3, a: Vec3, b: Vec3) -> (Vec3, f32) {
    let ab = b - a;
    let l2 = ab.length_squared();
    let q = if l2 < 1e-6 { a } else { a + ab * ((p - a).dot(ab) / l2).clamp(0.0, 1.0) };
    (q, p.distance(q))
}

/// Uniform XZ grid of ids (64 m cells).
#[derive(Default)]
pub struct Grid {
    cells: HashMap<(i32, i32), Vec<u32>>,
}

impl Grid {
    fn cell(v: f32) -> i32 {
        (v / CELL).floor() as i32
    }

    pub fn insert(&mut self, min: [f32; 2], max: [f32; 2], id: u32) {
        for cx in Self::cell(min[0])..=Self::cell(max[0]) {
            for cz in Self::cell(min[1])..=Self::cell(max[1]) {
                self.cells.entry((cx, cz)).or_default().push(id);
            }
        }
    }

    pub fn at(&self, x: f32, z: f32) -> &[u32] {
        self.cells.get(&(Self::cell(x), Self::cell(z))).map_or(&[], |v| v.as_slice())
    }

    /// Ids in the cells touching the square of half-size `r` around (x, z); sorted, unique (candidates only: the caller
    /// still tests the true distance).
    pub fn query(&self, x: f32, z: f32, r: f32, out: &mut Vec<u32>) {
        out.clear();
        for cx in Self::cell(x - r)..=Self::cell(x + r) {
            for cz in Self::cell(z - r)..=Self::cell(z + r) {
                if let Some(v) = self.cells.get(&(cx, cz)) {
                    out.extend_from_slice(v);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Soundscape data
// ---------------------------------------------------------------------------------------------------------------

/// An FEV event reference: project (first path component), path inside the project and bare name.
#[derive(Clone, Debug, Default)]
pub struct EvName {
    pub project: String,
    pub path: String,
    pub name: String,
    pub key: String,
}

impl EvName {
    /// `prefix` = soundscape eventPath ("AMB_Default/TrackAmbient/Track3D"), `name` = eventName.
    pub fn new(prefix: &str, name: &str) -> Self {
        let (project, rest) = prefix.split_once('/').unwrap_or((prefix, ""));
        let path = if rest.is_empty() { name.to_owned() } else { format!("{rest}/{name}") };
        Self { project: project.to_owned(), key: format!("{project}/{path}"), path, name: name.to_owned() }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Template {
    pub bed: EvName,
    pub crowd: EvName,
    pub tree: EvName,
    pub start_below: f32,
    pub stop_above: f32,
}

struct Emitter {
    a: Vec3,
    b: Vec3,
    ev: u16,
    rs: u32,
    rn: u8,
}

struct Pt {
    pos: Vec3,
    rs: u32,
    rn: u8,
}

struct Zone {
    template: usize,
    min_h: f32,
    max_h: f32,
    centroid: [f32; 2],
}

struct Tri {
    p: [[f32; 2]; 3],
    zone: u32,
    /// Reverb only: per-point value 0/1 (Template1 side).
    v: [f32; 3],
}

struct RZone {
    t0: u16,
    t1: u16,
    min_h: f32,
    max_h: f32,
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub struct Totals {
    pub track3d: usize,
    pub trees: usize,
    pub crowds: usize,
    pub zones: usize,
    pub zone_tris: usize,
    pub rzones: usize,
    pub rzone_tris: usize,
}

#[derive(Default)]
pub struct Soundscape {
    names: Vec<EvName>,
    emitters: Vec<Emitter>,
    trees: Vec<Pt>,
    crowds: Vec<Pt>,
    routes: Vec<u16>,
    em_grid: Grid,
    tree_grid: Grid,
    crowd_grid: Grid,
    zones: Vec<Zone>,
    ztris: Vec<Tri>,
    zgrid: Grid,
    rzones: Vec<RZone>,
    rtris: Vec<Tri>,
    rgrid: Grid,
    templates: Vec<Template>,
    pub totals: Totals,
}

impl Soundscape {
    fn routes_of(&self, rs: u32, rn: u8) -> &[u16] {
        &self.routes[rs as usize..rs as usize + rn as usize]
    }

    /// Ambience zone index at `pos` (inside a triangle, y in the band). Overlaps: nearest zone centroid
    /// (world_audio 3.5 proposal, INFERRED).
    pub fn zone_at(&self, pos: Vec3) -> Option<usize> {
        let p = [pos.x, pos.z];
        let mut best: Option<(usize, f32)> = None;
        for &ti in self.zgrid.at(pos.x, pos.z) {
            let t = &self.ztris[ti as usize];
            let z = &self.zones[t.zone as usize];
            if pos.y < z.min_h || pos.y > z.max_h || !point_in_tri(p, &t.p) {
                continue;
            }
            let d = (z.centroid[0] - p[0]).hypot(z.centroid[1] - p[1]);
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((t.zone as usize, d));
            }
        }
        best.map(|(z, _)| z)
    }

    /// Ambience template at `pos`; no zone = template 0 (INFERRED: 0 zones use it).
    pub fn template_at(&self, pos: Vec3) -> usize {
        self.zone_at(pos).map_or(0, |z| self.zones[z].template).min(self.templates.len().saturating_sub(1))
    }

    /// (template0, template1, blend 0..1 towards template1) of the reverb zone at `pos`; (0, 0, 0) outside every zone.
    /// Blend = barycentric mix of the per-point values (INFERRED). No DSP consumes it yet.
    pub fn reverb_at(&self, pos: Vec3) -> (u16, u16, f32) {
        let p = [pos.x, pos.z];
        for &ti in self.rgrid.at(pos.x, pos.z) {
            let t = &self.rtris[ti as usize];
            let z = &self.rzones[t.zone as usize];
            if pos.y < z.min_h || pos.y > z.max_h {
                continue;
            }
            if let Some(w) = bary(p, &t.p) {
                return (z.t0, z.t1, (w[0] * t.v[0] + w[1] * t.v[1] + w[2] * t.v[2]).clamp(0.0, 1.0));
            }
        }
        (0, 0, 0.0)
    }
}

#[derive(Deserialize)]
struct P3 {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Deserialize)]
struct P2 {
    x: f32,
    z: f32,
    #[serde(default)]
    value: f32,
}

#[derive(Deserialize)]
struct ObjJ {
    event_path: String,
    event_name: String,
    #[serde(default)]
    route_ids: Vec<u32>,
    position1: P3,
    position2: P3,
}

#[derive(Deserialize, Default)]
struct TrackAudioJ {
    #[serde(default)]
    track3d: Vec<ObjJ>,
}

#[derive(Deserialize)]
struct PtJ {
    #[serde(default)]
    route_ids: Vec<u32>,
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Deserialize)]
struct AZoneJ {
    template: i64,
    min_height: f32,
    max_height: f32,
    triangles: Vec<Vec<P2>>,
}

#[derive(Deserialize)]
struct RZoneJ {
    template0: i64,
    template1: i64,
    min_height: f32,
    max_height: f32,
    triangles: Vec<Vec<P2>>,
}

#[derive(Deserialize, Default)]
struct TileJson {
    #[serde(default)]
    track_audio: TrackAudioJ,
    #[serde(default)]
    trees: Vec<PtJ>,
    #[serde(default)]
    crowds: Vec<PtJ>,
    #[serde(default)]
    ambience_zones: Vec<AZoneJ>,
    #[serde(default)]
    reverb_zones: Vec<RZoneJ>,
}

#[derive(Deserialize)]
struct IndexJson {
    #[serde(default)]
    tiles: Vec<IndexTile>,
}

#[derive(Deserialize)]
struct IndexTile {
    file: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct TplJ {
    #[serde(rename = "index")]
    index: usize,
    background_stream_name: String,
    background_stream_path: String,
    background_stream_start_below_speed: f32,
    background_stream_stop_above_speed: f32,
    crowd_event_name: String,
    crowd_event_path: String,
    tree_event_name: String,
    tree_event_path: String,
}

#[derive(Deserialize)]
struct TplFile {
    templates: Vec<TplJ>,
}

fn tri3(t: &[P2]) -> Option<[[f32; 2]; 3]> {
    (t.len() >= 3).then(|| [[t[0].x, t[0].z], [t[1].x, t[1].z], [t[2].x, t[2].z]])
}

fn tri_bbox(t: &[[f32; 2]; 3]) -> ([f32; 2], [f32; 2]) {
    let mn = [t[0][0].min(t[1][0]).min(t[2][0]), t[0][1].min(t[1][1]).min(t[2][1])];
    let mx = [t[0][0].max(t[1][0]).max(t[2][0]), t[0][1].max(t[1][1]).max(t[2][1])];
    (mn, mx)
}

/// Collects tiles into a [`Soundscape`].
#[derive(Default)]
struct Builder {
    s: Soundscape,
    intern: HashMap<String, u16>,
}

impl Builder {
    fn route_slice(&mut self, ids: &[u32]) -> (u32, u8) {
        let rs = self.s.routes.len() as u32;
        let n = ids.len().min(255);
        self.s.routes.extend(ids[..n].iter().map(|&r| r.min(u16::MAX as u32) as u16));
        (rs, n as u8)
    }

    /// `reflections`: keep the `Default_Reflections_*` emitters (phase 3; off).
    fn ingest(&mut self, t: TileJson, reflections: bool) {
        for o in t.track_audio.track3d {
            self.s.totals.track3d += 1;
            if !reflections && o.event_name.starts_with("Default_Reflections") {
                continue;
            }
            let key = format!("{}|{}", o.event_path, o.event_name);
            let ev = match self.intern.get(&key) {
                Some(&i) => i,
                None => {
                    let i = self.s.names.len() as u16;
                    self.s.names.push(EvName::new(&o.event_path, &o.event_name));
                    self.intern.insert(key, i);
                    i
                }
            };
            let a = Vec3::new(o.position1.x, o.position1.y, o.position1.z);
            let b = Vec3::new(o.position2.x, o.position2.y, o.position2.z);
            let (rs, rn) = self.route_slice(&o.route_ids);
            let id = self.s.emitters.len() as u32;
            let (mn, mx) = (a.min(b), a.max(b));
            self.s.em_grid.insert([mn.x, mn.z], [mx.x, mx.z], id);
            self.s.emitters.push(Emitter { a, b, ev, rs, rn });
        }
        for (list, crowd) in [(t.trees, false), (t.crowds, true)] {
            for p in list {
                let (rs, rn) = self.route_slice(&p.route_ids);
                let (pts, grid) = if crowd { (&mut self.s.crowds, &mut self.s.crowd_grid) } else { (&mut self.s.trees, &mut self.s.tree_grid) };
                grid.insert([p.x, p.z], [p.x, p.z], pts.len() as u32);
                pts.push(Pt { pos: Vec3::new(p.x, p.y, p.z), rs, rn });
                if crowd {
                    self.s.totals.crowds += 1;
                } else {
                    self.s.totals.trees += 1;
                }
            }
        }
        for z in t.ambience_zones {
            self.s.totals.zones += 1;
            let zi = self.s.zones.len() as u32;
            let mut sum = [0.0f32; 2];
            let mut n = 0.0;
            for tri in &z.triangles {
                self.s.totals.zone_tris += 1;
                let Some(p) = tri3(tri) else { continue };
                for q in p {
                    sum[0] += q[0] / 3.0;
                    sum[1] += q[1] / 3.0;
                }
                n += 1.0;
                let (mn, mx) = tri_bbox(&p);
                self.s.zgrid.insert(mn, mx, self.s.ztris.len() as u32);
                self.s.ztris.push(Tri { p, zone: zi, v: [0.0; 3] });
            }
            let c = if n > 0.0 { [sum[0] / n, sum[1] / n] } else { [0.0; 2] };
            self.s.zones.push(Zone { template: z.template.max(0) as usize, min_h: z.min_height, max_h: z.max_height, centroid: c });
        }
        for z in t.reverb_zones {
            self.s.totals.rzones += 1;
            let zi = self.s.rzones.len() as u32;
            for tri in &z.triangles {
                self.s.totals.rzone_tris += 1;
                let Some(p) = tri3(tri) else { continue };
                let (mn, mx) = tri_bbox(&p);
                self.s.rgrid.insert(mn, mx, self.s.rtris.len() as u32);
                self.s.rtris.push(Tri { p, zone: zi, v: [tri[0].value, tri[1].value, tri[2].value] });
            }
            self.s.rzones.push(RZone { t0: z.template0.max(0) as u16, t1: z.template1.max(0) as u16, min_h: z.min_height, max_h: z.max_height });
        }
    }
}

fn load_templates(dir: &Path) -> Vec<Template> {
    let path = dir.join("soundscape/_templates/ambience.json");
    let parsed = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<TplFile>(&b).ok());
    let Some(file) = parsed else {
        warn!("ambience: {}: unreadable; no beds", path.display());
        return vec![Template { start_below: 35.0, stop_above: 40.0, ..Default::default() }];
    };
    let n = file.templates.iter().map(|t| t.index + 1).max().unwrap_or(1);
    let mut out = vec![Template { start_below: 35.0, stop_above: 40.0, ..Default::default() }; n];
    for t in file.templates {
        out[t.index] = Template {
            bed: EvName::new(&t.background_stream_path, &t.background_stream_name),
            crowd: EvName::new(&t.crowd_event_path, &t.crowd_event_name),
            tree: EvName::new(&t.tree_event_path, &t.tree_event_name),
            // The data says 35 / 40 everywhere; keep those if a field is missing.
            start_below: if t.background_stream_start_below_speed > 0.0 { t.background_stream_start_below_speed } else { 35.0 },
            stop_above: if t.background_stream_stop_above_speed > 0.0 { t.background_stream_stop_above_speed } else { 40.0 },
        };
    }
    out
}

/// Reads `<dir>/soundscape/index.json` (or lists the folder) and every tile.
pub fn load_soundscape(dir: &Path) -> anyhow::Result<Soundscape> {
    use anyhow::Context;
    let sdir = dir.join("soundscape");
    let mut files: Vec<PathBuf> = std::fs::read(sdir.join("index.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<IndexJson>(&b).ok())
        .map(|i| i.tiles.iter().filter_map(|t| Path::new(&t.file).file_name().map(|n| sdir.join(n))).collect())
        .unwrap_or_default();
    if files.is_empty() {
        files = std::fs::read_dir(&sdir)
            .with_context(|| format!("{}", sdir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("colorado_")))
            .collect();
        files.sort();
    }
    let mut b = Builder::default();
    for f in &files {
        let tile: TileJson = serde_json::from_slice(&std::fs::read(f).with_context(|| f.display().to_string())?).with_context(|| f.display().to_string())?;
        b.ingest(tile, false);
    }
    let mut s = b.s;
    s.templates = load_templates(dir);
    Ok(s)
}

// ---------------------------------------------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------------------------------------------

/// An FEV event resolved to what the per-frame code needs (the Event itself is re-looked-up through `fev` to play).
struct EvInfo {
    fev: Arc<Fev>,
    path: String,
    volume: f32,
    is_3d: bool,
    min: f32,
    max: f32,
    roll: Roll,
    looped: bool,
    interval: Option<(f32, f32)>,
    waves: Vec<(String, u32)>,
}

struct Active {
    info: Arc<EvInfo>,
    voice: Option<Played>,
    next_fire: f32,
    dist: f32,
    pos: Vec3,
    seg: Option<(Vec3, Vec3)>,
    seen: u32,
}

struct Bed {
    template: usize,
    info: Arc<EvInfo>,
    voices: Vec<VoiceId>,
    level: f32,
}

struct Cand {
    idx: u32,
    d: f32,
    pos: Vec3,
    seg: Option<(Vec3, Vec3)>,
    info: Option<Arc<EvInfo>>,
}

type Loaded = (u32, Option<Soundscape>);

#[derive(Resource)]
pub struct Ambience {
    tx: Sender<Loaded>,
    rx: Mutex<Receiver<Loaded>>,
    token: u32,
    key: Option<(u32, String)>,
    data: Option<Arc<Soundscape>>,
    resolved: HashMap<String, Option<Arc<EvInfo>>>,
    active: HashMap<(u8, u32), Active>,
    beds: Vec<Bed>,
    bed_on: bool,
    cur_template: usize,
    pending_template: Option<(usize, f32)>,
    bed_retry: f32,
    tick: u32,
    next_select: f32,
    next_log: f32,
    ids: Vec<u32>,
    cands: Vec<Cand>,
    due: Vec<(u8, f32, (u8, u32))>,
    reverb_logged: bool,
}

impl Default for Ambience {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx: Mutex::new(rx),
            token: 0,
            key: None,
            data: None,
            resolved: HashMap::new(),
            active: HashMap::new(),
            beds: Vec::new(),
            bed_on: true,
            cur_template: 0,
            pending_template: None,
            bed_retry: 0.0,
            tick: 0,
            next_select: 0.0,
            next_log: 0.0,
            ids: Vec::new(),
            cands: Vec::new(),
            due: Vec::new(),
            reverb_logged: false,
        }
    }
}

impl Ambience {
    /// Reverb zone at `pos` (see [`Soundscape::reverb_at`]); (0, 0, 0) while no data is loaded. No DSP yet.
    pub fn reverb_at(&self, pos: Vec3) -> (u16, u16, f32) {
        self.data.as_ref().map_or((0, 0, 0.0), |d| d.reverb_at(pos))
    }

    /// Ambience template index at `pos` (0 = default).
    pub fn template_at(&self, pos: Vec3) -> usize {
        self.data.as_ref().map_or(0, |d| d.template_at(pos))
    }

    fn live_voices(&self) -> usize {
        self.active.values().filter(|a| a.voice.is_some()).count() + self.beds.iter().map(|b| b.voices.len()).sum::<usize>()
    }

    /// Stops everything and forgets the data (map change / despawn).
    fn clear(&mut self, bank: &SfxBank) {
        for a in self.active.values() {
            if let Some(v) = a.voice {
                mx_stop(bank, v.id, 0.2);
            }
        }
        for b in &self.beds {
            for &v in &b.voices {
                mx_stop(bank, v, 0.5);
            }
        }
        self.active.clear();
        self.beds.clear();
        self.data = None;
        self.resolved.clear();
        self.token = self.token.wrapping_add(1);
        self.cur_template = 0;
        self.pending_template = None;
        self.bed_on = true;
    }
}

// The only place that touches the mixer API (a signature change in fh1_audio::ambient is a fix here).
fn mx_play(bank: &SfxBank, pcm: Arc<Pcm>, p: VoiceParams) -> Option<VoiceId> {
    bank.mixer()?.play(pcm, p)
}
fn mx_set(bank: &SfxBank, id: VoiceId, gain: f32, pan: f32, pitch: f32) {
    if let Some(m) = bank.mixer() {
        m.set(id, gain, pan, pitch);
    }
}
fn mx_stop(bank: &SfxBank, id: VoiceId, fade_s: f32) {
    if let Some(m) = bank.mixer() {
        m.stop(id, fade_s);
    }
}
fn mx_playing(bank: &SfxBank, id: VoiceId) -> bool {
    bank.mixer().is_some_and(|m| m.is_playing(id))
}

fn load_info(bank: &SfxBank, n: &EvName) -> Option<Arc<EvInfo>> {
    let fev = bank.fev(&n.project)?;
    let (path, volume, is_3d, min, max, roll, looped, interval, waves) = {
        let ev = fev.event(&n.path).or_else(|| fev.event(&n.name))?;
        (
            ev.path.clone(),
            ev.volume,
            ev.is_3d,
            ev.min_dist,
            ev.max_dist,
            roll_for(ev),
            ev.looped,
            ev.spawn_interval_s,
            ev.waves.iter().map(|w| (w.bank.clone(), w.index)).collect::<Vec<_>>(),
        )
    };
    for (b, i) in &waves {
        bank.prefetch(b, *i);
    }
    Some(Arc::new(EvInfo { fev, path, volume, is_3d, min, max, roll, looped, interval, waves }))
}

/// Cached lookup; a miss is remembered and warned about once.
fn resolve(cache: &mut HashMap<String, Option<Arc<EvInfo>>>, bank: &SfxBank, n: &EvName) -> Option<Arc<EvInfo>> {
    if n.key.is_empty() {
        return None;
    }
    if let Some(r) = cache.get(n.key.as_str()) {
        return r.clone();
    }
    let r = load_info(bank, n);
    if r.is_none() {
        warn!("ambience: event {} not found (FEV missing or name unresolved)", n.key);
    }
    cache.insert(n.key.clone(), r.clone());
    r
}

fn default_gap(class: u8) -> (f32, f32) {
    // Re-trigger gaps for one-shot events that carry no spawn interval: guesses (UNKNOWN until the FEV / a capture).
    match class {
        CROWD => (2.0, 6.0),
        _ => (4.0, 10.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn ambience_update(
    mut st: ResMut<Ambience>,
    bank: Option<Res<SfxBank>>,
    listener: Res<Listener>,
    track: Res<Track>,
    generation: Res<WorldGeneration>,
    garage: Option<Res<crate::Garage>>,
    cars: Query<&Car>,
    race: Option<Res<RaceState>>,
    events: Option<Res<Events>>,
    virt: Res<Time<Virtual>>,
    time: Res<Time<Real>>,
) {
    let _watch = crate::perf::watch("ambience_update");
    let Some(bank) = bank else { return };
    let bank = &*bank;
    let st = &mut *st;
    let now = time.elapsed_secs();

    // Map change: drop everything, reload for Colorado.
    if st.key.as_ref().is_none_or(|k| k.0 != generation.0 || k.1 != track.id) {
        st.key = Some((generation.0, track.id.clone()));
        st.clear(bank);
        if track.id == "colorado" {
            if let Some(g) = &garage {
                let (dir, tx, token) = (g.assets.join("audio"), st.tx.clone(), st.token);
                let _ = std::thread::Builder::new().name("fh1-ambience-load".into()).spawn(move || {
                    let t = std::time::Instant::now();
                    let s = match load_soundscape(&dir) {
                        Ok(s) => {
                            info!("ambience: {} emitters, {} trees, {} crowds, {} zones, {} reverb zones in {:.0} ms", s.emitters.len(), s.trees.len(), s.crowds.len(), s.zones.len(), s.rzones.len(), t.elapsed().as_secs_f32() * 1e3);
                            Some(s)
                        }
                        Err(e) => {
                            warn!("ambience: soundscape not loaded ({e:#}); run fh1setup audio");
                            None
                        }
                    };
                    let _ = tx.send((token, s));
                });
            }
        }
    }
    let arrived: Vec<Loaded> = st.rx.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default();
    for (token, s) in arrived {
        if token == st.token {
            if let Some(s) = s {
                if !s.rzones.is_empty() && !st.reverb_logged {
                    st.reverb_logged = true;
                    info!("ambience: {} reverb zones parsed; Ambience::reverb_at is queryable but there is no reverb DSP yet", s.rzones.len());
                }
                st.data = Some(Arc::new(s));
            }
        }
    }
    let Some(data) = st.data.clone() else { return };
    if bank.mixer().is_none() || !world_sfx_allowed(virt.is_paused()) {
        return;
    }
    let l = *listener;
    let dt = time.delta_secs().min(0.1);
    let speed_mph = cars.single().map_or(0.0, |Car(v)| v.speed() * MPS_TO_MPH);
    let race_route: Option<u16> = match (&race, &events) {
        (Some(r), Some(e)) if matches!(r.phase, RacePhase::Grid { .. } | RacePhase::Countdown { .. } | RacePhase::Racing | RacePhase::Finished { .. }) => {
            r.race.and_then(|i| e.races.get(i)).map(|d| d.track_id.min(u16::MAX as u32) as u16)
        }
        _ => None,
    };

    if now >= st.next_select {
        st.next_select = now + SELECT_S;
        update_template(st, &data, l.pos, now);
        select(st, &data, bank, l.pos, race_route, now);
    }
    update_beds(st, &data, bank, dt, speed_mph, now);
    update_voices(st, bank, &l);
    start_due(st, bank, &l, now);
    if std::env::var_os("FH1_AMB_LOG").is_some_and(|v| v == "1") && now >= st.next_log {
        st.next_log = now + 1.0;
        log_voices(st);
    }
}

fn update_template(st: &mut Ambience, data: &Soundscape, pos: Vec3, now: f32) {
    let t = data.template_at(pos);
    if t == st.cur_template {
        st.pending_template = None;
    } else if let Some((p, since)) = st.pending_template {
        if p != t {
            st.pending_template = Some((t, now));
        } else if now - since >= ZONE_DEBOUNCE_S {
            st.cur_template = t;
            st.pending_template = None;
        }
    } else {
        st.pending_template = Some((t, now));
    }
}

fn touch(st: &mut Ambience, class: u8, c: &Cand, tick: u32) -> bool {
    if let Some(a) = st.active.get_mut(&(class, c.idx)) {
        a.seen = tick;
        a.dist = c.d;
        a.pos = c.pos;
        true
    } else {
        false
    }
}

fn insert_active(st: &mut Ambience, bank: &SfxBank, class: u8, c: &Cand, info: Arc<EvInfo>, tick: u32, now: f32) {
    let next_fire = if info.looped {
        now
    } else {
        let lo = info.interval.unwrap_or(default_gap(class)).0;
        now + bank.rand() * lo
    };
    st.active.insert((class, c.idx), Active { info, voice: None, next_fire, dist: c.d, pos: c.pos, seg: c.seg, seen: tick });
}

fn select(st: &mut Ambience, data: &Soundscape, bank: &SfxBank, p: Vec3, race_route: Option<u16>, now: f32) {
    st.tick = st.tick.wrapping_add(1);
    let tick = st.tick;
    let mut ids = std::mem::take(&mut st.ids);
    let mut cands = std::mem::take(&mut st.cands);

    // Emitters (track3d): in range of their own event, route-gated, nearest point of a segment.
    cands.clear();
    data.em_grid.query(p.x, p.z, EMIT_RADIUS, &mut ids);
    for &i in &ids {
        let e = &data.emitters[i as usize];
        if !route_active(data.routes_of(e.rs, e.rn), race_route) {
            continue;
        }
        let Some(info) = resolve(&mut st.resolved, bank, &data.names[e.ev as usize]) else { continue };
        let (q, d) = nearest_on_segment(p, e.a, e.b);
        let was = st.active.contains_key(&(EMITTER, i));
        let range = (if info.is_3d { info.max } else { EMIT_RADIUS }).min(EMIT_RADIUS * HYST) * if was { HYST } else { 1.0 };
        if d <= range {
            cands.push(Cand { idx: i, d, pos: q, seg: (e.a != e.b).then_some((e.a, e.b)), info: Some(info) });
        }
    }
    cands.sort_by(|a, b| a.d.total_cmp(&b.d));
    cands.truncate(EMIT_MAX);
    for c in cands.iter_mut() {
        if !touch(st, EMITTER, c, tick) {
            if let Some(info) = c.info.take() {
                insert_active(st, bank, EMITTER, c, info, tick, now);
            }
        }
    }

    // Trees then crowds: nearest N of their class, event from the zone template at the point.
    for (class, pts, grid, range, max) in [(TREE, &data.trees, &data.tree_grid, TREE_RANGE, TREE_MAX), (CROWD, &data.crowds, &data.crowd_grid, CROWD_RANGE, CROWD_MAX)] {
        cands.clear();
        grid.query(p.x, p.z, range * HYST, &mut ids);
        for &i in &ids {
            let pt = &pts[i as usize];
            if !route_active(data.routes_of(pt.rs, pt.rn), race_route) {
                continue;
            }
            let d = p.distance(pt.pos);
            let r = if st.active.contains_key(&(class, i)) { range * HYST } else { range };
            if d <= r {
                cands.push(Cand { idx: i, d, pos: pt.pos, seg: None, info: None });
            }
        }
        cands.sort_by(|a, b| a.d.total_cmp(&b.d));
        cands.truncate(max);
        for c in cands.iter() {
            if touch(st, class, c, tick) {
                continue;
            }
            let tpl = &data.templates[data.template_at(c.pos)];
            let name = if class == TREE { &tpl.tree } else { &tpl.crowd };
            if let Some(info) = resolve(&mut st.resolved, bank, name) {
                insert_active(st, bank, class, c, info, tick, now);
            }
        }
    }
    st.ids = ids;
    st.cands = cands;

    st.active.retain(|_, a| {
        if a.seen == tick {
            return true;
        }
        if let (Some(v), true) = (a.voice, a.info.looped) {
            mx_stop(bank, v.id, 0.3);
        }
        false
    });
}

/// Per frame: positions, gain / pan updates, finished voices.
fn update_voices(st: &mut Ambience, bank: &SfxBank, l: &Listener) {
    for a in st.active.values_mut() {
        if let Some((s0, s1)) = a.seg {
            a.pos = nearest_on_segment(l.pos, s0, s1).0;
        }
        a.dist = l.pos.distance(a.pos);
        let Some(v) = a.voice else { continue };
        if mx_playing(bank, v.id) {
            let (g, pan) = if a.info.is_3d { spatial_roll(l, a.pos, a.info.min, a.info.max, &a.info.roll) } else { (1.0, 0.0) };
            mx_set(bank, v.id, v.base * g, pan, v.pitch);
        } else {
            a.voice = None;
        }
    }
}

fn start_due(st: &mut Ambience, bank: &SfxBank, l: &Listener, now: f32) {
    let mut due = std::mem::take(&mut st.due);
    due.clear();
    for (k, a) in &st.active {
        if now >= a.next_fire && (!a.info.looped || a.voice.is_none()) {
            due.push((k.0, a.dist, *k));
        }
    }
    due.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    for &(_, _, k) in due.iter().take(MAX_STARTS_PER_FRAME) {
        start_one(st, bank, l, k, now);
    }
    st.due = due;
}

fn start_one(st: &mut Ambience, bank: &SfxBank, l: &Listener, key: (u8, u32), now: f32) {
    let (info, pos, dist) = match st.active.get(&key) {
        Some(a) => (a.info.clone(), a.pos, a.dist),
        None => return,
    };
    if !make_room(st, bank, key.0, dist, now) {
        if let Some(a) = st.active.get_mut(&key) {
            a.next_fire = now + 0.25;
        }
        return;
    }
    // A silence entry won the pick (FEV wave kind 2): count it as a trigger and wait for the next one.
    if !info.looped && info.fev.event(&info.path).is_some_and(|ev| bank.rolls_silence(ev)) {
        if let Some(a) = st.active.get_mut(&key) {
            let (lo, hi) = info.interval.unwrap_or(default_gap(key.0));
            a.next_fire = now + bank.rand_range(lo, hi.max(lo));
        }
        return;
    }
    let played = info.fev.event(&info.path).and_then(|ev| bank.play_event_ex(ev, info.is_3d.then_some(pos), l, 1.0, Bus::Ambience));
    let Some(a) = st.active.get_mut(&key) else { return };
    match played {
        Some(p) => {
            a.next_fire = if info.looped {
                now
            } else {
                let (lo, hi) = info.interval.unwrap_or(default_gap(key.0));
                // Without an explicit spawn interval the next one waits for this clip to end.
                now + if info.interval.is_some() { 0.0 } else { p.duration_s } + bank.rand_range(lo, hi.max(lo))
            };
            a.voice = Some(p);
        }
        None => a.next_fire = now + 0.15,
    }
}

/// Frees a slot when the 48-voice budget is full by stopping the lowest-priority, farthest voice, if it ranks below
/// the newcomer.
fn make_room(st: &mut Ambience, bank: &SfxBank, prio: u8, dist: f32, now: f32) -> bool {
    if st.live_voices() < BUDGET {
        return true;
    }
    let mut worst: Option<((u8, u32), f32)> = None;
    for (k, a) in &st.active {
        if a.voice.is_some() && worst.is_none_or(|(wk, wd)| k.0 > wk.0 || (k.0 == wk.0 && a.dist > wd)) {
            worst = Some((*k, a.dist));
        }
    }
    let Some((wk, wd)) = worst else { return false };
    if wk.0 > prio || (wk.0 == prio && wd > dist) {
        if let Some(a) = st.active.get_mut(&wk) {
            if let Some(v) = a.voice.take() {
                mx_stop(bank, v.id, 0.05);
            }
            a.next_fire = now + 0.5;
        }
        true
    } else {
        false
    }
}

/// Quad beds: Schmitt-gated on speed, crossfade between templates.
fn update_beds(st: &mut Ambience, data: &Soundscape, bank: &SfxBank, dt: f32, speed_mph: f32, now: f32) {
    let cur = st.cur_template.min(data.templates.len().saturating_sub(1));
    let tpl = &data.templates[cur];
    st.bed_on = schmitt(st.bed_on, speed_mph, tpl.start_below, tpl.stop_above);
    let on = st.bed_on;
    for b in st.beds.iter_mut() {
        let target = if on && b.template == cur { 1.0 } else { 0.0 };
        let step = dt / CROSSFADE_S;
        b.level = if target > b.level { (b.level + step).min(target) } else { (b.level - step).max(target) };
        for &v in &b.voices {
            mx_set(bank, v, b.info.volume * b.level, 0.0, 1.0);
        }
    }
    st.beds.retain(|b| {
        let dead = b.level <= 0.0 && !(on && b.template == cur);
        if dead {
            for &v in &b.voices {
                mx_stop(bank, v, 0.1);
            }
        }
        !dead
    });
    if !on || now < st.bed_retry || st.beds.iter().any(|b| b.template == cur) || tpl.bed.key.is_empty() {
        return;
    }
    st.bed_retry = now + 0.25;
    let Some(info) = resolve(&mut st.resolved, bank, &tpl.bed) else { return };
    // F and R of the event's stereo pair: all waves, looped, head-relative.
    let pcms: Vec<Arc<Pcm>> = info.waves.iter().take(4).filter_map(|(b, i)| bank.sample(b, *i)).collect();
    if pcms.is_empty() || pcms.len() < info.waves.len().min(4) {
        return;
    }
    let voices: Vec<VoiceId> = pcms
        .into_iter()
        .filter_map(|p| mx_play(bank, p, VoiceParams { looped: true, gain: 0.0, pan: 0.0, pitch: 1.0, bus: Bus::Ambience, ..Default::default() }))
        .collect();
    if !voices.is_empty() {
        st.beds.push(Bed { template: cur, info, voices, level: 0.0 });
    }
}

fn log_voices(st: &Ambience) {
    let mut n = [0usize; 4];
    for (k, a) in &st.active {
        if a.voice.is_some() {
            n[k.0 as usize] += 1;
        }
    }
    info!(
        "ambience: voices {} (beds {} emitters {} crowds {} trees {}) tracked {} template {} bed_on {}",
        st.live_voices(),
        st.beds.iter().map(|b| b.voices.len()).sum::<usize>(),
        n[EMITTER as usize],
        n[CROWD as usize],
        n[TREE as usize],
        st.active.len(),
        st.cur_template,
        st.bed_on
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri() -> [[f32; 2]; 3] {
        [[0.0, 0.0], [10.0, 0.0], [0.0, 10.0]]
    }

    #[test]
    fn point_in_triangle() {
        let t = tri();
        assert!(point_in_tri([1.0, 1.0], &t));
        assert!(point_in_tri([5.0, 5.0], &t)); // on the hypotenuse
        assert!(!point_in_tri([6.0, 6.0], &t));
        assert!(!point_in_tri([-0.1, 1.0], &t));
        // reversed winding
        let r = [t[0], t[2], t[1]];
        assert!(point_in_tri([1.0, 1.0], &r));
        assert!(!point_in_tri([1.0, 1.0], &[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]]));
    }

    fn zone_scene() -> Soundscape {
        let mut b = Builder::default();
        let square = |x0: f32, template: i64, min: f32, max: f32| AZoneJ {
            template,
            min_height: min,
            max_height: max,
            triangles: vec![
                vec![P2 { x: x0, z: 0.0, value: 0.0 }, P2 { x: x0 + 100.0, z: 0.0, value: 0.0 }, P2 { x: x0, z: 100.0, value: 0.0 }],
                vec![P2 { x: x0 + 100.0, z: 100.0, value: 0.0 }, P2 { x: x0 + 100.0, z: 0.0, value: 0.0 }, P2 { x: x0, z: 100.0, value: 0.0 }],
            ],
        };
        b.ingest(
            TileJson { ambience_zones: vec![square(0.0, 2, -10.0, 500.0), square(50.0, 3, 100.0, 200.0)], ..Default::default() },
            false,
        );
        b.s.templates = vec![Template::default(); 6];
        b.s
    }

    #[test]
    fn zone_pick_template_and_height_band() {
        let s = zone_scene();
        assert_eq!(s.template_at(Vec3::new(10.0, 50.0, 50.0)), 2);
        // Overlap x 50..100, inside both bands: the nearer centroid wins (zone 3 centroid x 100 vs zone 2 at 50).
        assert_eq!(s.template_at(Vec3::new(90.0, 150.0, 50.0)), 3);
        // Overlap but above zone 3's band: zone 2.
        assert_eq!(s.template_at(Vec3::new(90.0, 300.0, 50.0)), 2);
        // Outside every zone: template 0.
        assert_eq!(s.template_at(Vec3::new(-500.0, 50.0, 50.0)), 0);
        assert_eq!(s.template_at(Vec3::new(10.0, 900.0, 50.0)), 0);
    }

    #[test]
    fn reverb_blend_is_barycentric() {
        let mut b = Builder::default();
        b.ingest(
            TileJson {
                reverb_zones: vec![RZoneJ {
                    template0: 0,
                    template1: 17,
                    min_height: 0.0,
                    max_height: 100.0,
                    triangles: vec![vec![P2 { x: 0.0, z: 0.0, value: 0.0 }, P2 { x: 10.0, z: 0.0, value: 1.0 }, P2 { x: 0.0, z: 10.0, value: 1.0 }]],
                }],
                ..Default::default()
            },
            false,
        );
        let (t0, t1, w) = b.s.reverb_at(Vec3::new(5.0, 10.0, 5.0));
        assert_eq!((t0, t1), (0, 17));
        assert!((w - 1.0).abs() < 1e-4);
        let (_, _, w) = b.s.reverb_at(Vec3::new(0.0, 10.0, 0.0));
        assert!(w.abs() < 1e-4);
        assert_eq!(b.s.reverb_at(Vec3::new(5.0, 500.0, 5.0)), (0, 0, 0.0));
    }

    #[test]
    fn bed_schmitt_trigger() {
        // world_audio A8: 36 -> 34 -> 36 -> 41, bed starts below 35 and stops above 40.
        let mut on = false;
        let mut seen = Vec::new();
        for s in [36.0, 34.0, 36.0, 41.0, 38.0, 34.9] {
            on = schmitt(on, s, 35.0, 40.0);
            seen.push(on);
        }
        assert_eq!(seen, [false, true, true, false, false, true]);
        assert!(schmitt(true, 40.0, 35.0, 40.0));
    }

    #[test]
    fn grid_query_finds_neighbours_and_segments() {
        let mut g = Grid::default();
        g.insert([10.0, 10.0], [10.0, 10.0], 1);
        g.insert([500.0, 500.0], [500.0, 500.0], 2);
        g.insert([-300.0, 0.0], [300.0, 0.0], 3); // long segment: several cells
        let mut out = Vec::new();
        g.query(0.0, 0.0, 100.0, &mut out);
        assert_eq!(out, vec![1, 3]);
        g.query(500.0, 500.0, 10.0, &mut out);
        assert_eq!(out, vec![2]);
        g.query(250.0, 0.0, 10.0, &mut out);
        assert_eq!(out, vec![3]);
        assert!(g.at(10.0, 10.0).contains(&1));
    }

    #[test]
    fn route_gate() {
        assert!(route_active(&[], None));
        assert!(route_active(&[0, 5], None));
        assert!(!route_active(&[5], None));
        assert!(route_active(&[5, 9], Some(9)));
        assert!(!route_active(&[5, 9], Some(7)));
    }

    #[test]
    fn segment_nearest_point() {
        let (q, d) = nearest_on_segment(Vec3::new(5.0, 0.0, 3.0), Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0));
        assert!((q - Vec3::new(5.0, 0.0, 0.0)).length() < 1e-5 && (d - 3.0).abs() < 1e-5);
        let (q, _) = nearest_on_segment(Vec3::new(20.0, 0.0, 0.0), Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0));
        assert_eq!(q, Vec3::new(10.0, 0.0, 0.0));
    }

    #[test]
    fn emitter_attenuation_monotone() {
        // world_audio A9 through the shared law (sfx_bank): non-increasing over 0..200 m, zero beyond.
        let l = Listener { pos: Vec3::ZERO, right: Vec3::X, forward: Vec3::NEG_Z };
        let mut prev = 2.0;
        for i in 0..=2100 {
            let g = spatial_roll(&l, Vec3::new(0.0, 0.0, -(i as f32) * 0.1), 6.0, 200.0, &Roll::Inverse).0;
            assert!(g <= prev + 1e-6);
            prev = g;
        }
        assert_eq!(spatial_roll(&l, Vec3::new(0.0, 0.0, -201.0), 6.0, 200.0, &Roll::Inverse).0, 0.0);
    }

    #[test]
    fn event_names_split_project_and_path() {
        let n = EvName::new("AMB_Default/TrackAmbient/Track3D", "Default_Tree");
        assert_eq!((n.project.as_str(), n.path.as_str()), ("AMB_Default", "TrackAmbient/Track3D/Default_Tree"));
    }

    /// Disc data (converted copy: NOT z-negated, which does not matter for counts). Skipped when absent.
    #[test]
    fn disc_totals() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/extracted/converted/audio");
        if !dir.join("soundscape/index.json").is_file() {
            return;
        }
        let mut b = Builder::default();
        let mut tiles = 0;
        for e in std::fs::read_dir(dir.join("soundscape")).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("colorado_") && n.ends_with(".json") {
                b.ingest(serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap(), true);
                tiles += 1;
            }
        }
        assert_eq!(tiles, 1450);
        let t = b.s.totals;
        assert_eq!((t.track3d, t.trees, t.crowds, t.zones, t.rzones), (33_356, 25_845, 6_400, 475, 377));
        assert_eq!((t.zone_tris, t.rzone_tris), (3_646, 4_076));
    }
}
