//! FMOD Designer event projects (`.fev`, "FEV1") as shipped with Forza Horizon (Xbox 360).
//!
//! Byte layout, verified by parsing all 46 disc files to the last byte (see `docs/FEV.md`):
//! little-endian; strings are `u32 len` (including the NUL) + bytes; floats are IEEE f32.
//! `"FEV1"`, version (0x0040_0000, or 0x0037_0000 for `Damage`/`ExhNoise`), two unknown u32,
//! (v0x40 only) a `(type id, count)` table, project name, wave banks, category tree, event group
//! tree (events inline), sound-definition templates, sound definitions (waves), a reverb count and a
//! music chunk. Anything not decoded is kept raw (`unknown`, `header`, `raw`) and tagged in the docs.
//!
//! Facade: [`Fev::parse`] / [`Fev::load`] / [`Fev::event`] and the flat [`Event`] list (with
//! [`WaveRef`]s resolved through layers -> sound instances -> sound definitions). Pure parsing, no
//! flags, no audio output, works with `default-features = false`.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// FMOD_MODE bits found in the event `mode` word (header word 7).
pub const MODE_2D: u32 = 0x0000_0008;
pub const MODE_3D: u32 = 0x0000_0010;
pub const MODE_3D_LOGROLLOFF: u32 = 0x0010_0000;
pub const MODE_3D_LINEARROLLOFF: u32 = 0x0020_0000;
pub const MODE_3D_CUSTOMROLLOFF: u32 = 0x0400_0000;

/// 3D distance attenuation shape of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rolloff {
    Inverse,
    Linear,
    /// Gain comes from the volume envelope driven by the `(distance)` parameter.
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaveBank {
    pub name: String,
    /// 0x200 = load into memory, 0x80 = stream from disk (INFERRED from FMOD flags).
    pub stream_type: u32,
    pub max_streams: u32,
    /// 8 bytes after max_streams (build hash, UNKNOWN meaning).
    pub hash: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    pub volume: f32,
    pub pitch: f32,
    /// Two u32 after pitch, always 0 on the disc.
    pub unknown: [u32; 2],
    pub children: Vec<Category>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PropValue {
    Int(u32),
    Float(f32),
    Str(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProp {
    pub name: String,
    pub value: PropValue,
}

/// One node of the event group tree; `events` index into [`Fev::events`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventGroup {
    pub name: String,
    pub user_props: Vec<UserProp>,
    pub groups: Vec<EventGroup>,
    pub events: Vec<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EnvPoint {
    /// Position along the controlling parameter, normalised 0..1 of its range.
    pub x: f32,
    /// Value (linear gain for volume envelopes; dB-exact values like 0.7079 = -3 dB).
    pub y: f32,
    /// Curve shape to the next point (1 = linear, 2/4 = curved, INFERRED).
    pub shape: u32,
}

/// A layer or effect envelope: `[owner:i32][b:str|u32][c:u32][target:u32][e:u32 (v0x40)][n pts][tail:2xu32]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// -1 for the first envelope of a layer-level property or of an effect, otherwise a small index.
    pub owner: i32,
    /// Effect name when the `b` word is a string (`"FMOD Highpass"`, `"Speaker Level"`...).
    pub effect: Option<String>,
    pub c: u32,
    /// Controlled property: 12 = volume (INFERRED), 20 = pitch? (UNKNOWN), 4 = effect parameter,
    /// 1028 / 132 / 260 / 36 / 13 UNKNOWN.
    pub target: u32,
    pub e: u32,
    pub points: Vec<EnvPoint>,
    /// Index into the event's parameter list that drives this envelope (VERIFIED on `(distance)`, `CarSpeed`).
    pub param_index: u32,
    /// Second tail word (0, or 1 on some effect envelopes), UNKNOWN.
    pub tail1: u32,
}

impl Envelope {
    /// Linear interpolation of the point list at normalised position `x` (clamped).
    pub fn eval(&self, x: f32) -> f32 {
        let p = &self.points;
        let (first, last) = match (p.first(), p.last()) {
            (Some(a), Some(b)) => (a, b),
            _ => return 1.0,
        };
        if x <= first.x {
            return first.y;
        }
        if x >= last.x {
            return last.y;
        }
        for w in p.windows(2) {
            if x <= w[1].x {
                let span = (w[1].x - w[0].x).max(1e-9);
                return w[0].y + (w[1].y - w[0].y) * ((x - w[0].x) / span);
            }
        }
        last.y
    }
}

/// A sound instance inside a layer: a window of a sound definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundInstance {
    /// Index into [`Fev::sound_defs`] (VERIFIED: `Default_Dogs` -> `/Wildlife/Dogs/Dogs_Day`).
    pub sound_def: u16,
    pub start: f32,
    pub length: f32,
    /// 0 = loop, 1 = one-shot (INFERRED from beds vs spawned one-shots; words[1]).
    pub loop_mode: u32,
    /// The 12 words after `length`: [0]=0, [1]=loop_mode, [2]=-1, [3..7]=UNKNOWN (mostly 0),
    /// [7]=1.0, [8],[9]=-1.0 or small random floats, [10],[11]=2 (mostly).
    pub raw: [u32; 12],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    /// 2 for sound layers (0 on placeholder layers of UI/VO events).
    pub kind: u16,
    /// Controlling parameter index (-1 = none on every disc layer).
    pub control_param: i16,
    /// UNKNOWN (0..3, 0xffff on empty layers).
    pub flags: u16,
    pub instances: Vec<SoundInstance>,
    pub envelopes: Vec<Envelope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    /// Auto-advance speed (units/s); 0 for game-driven parameters (`Timer` = 0.4).
    pub velocity: f32,
    pub min: f32,
    pub max: f32,
    /// 3 / 2 / 8 / 9 / 5 (bit 0 and bit 3 UNKNOWN).
    pub flags: u32,
    /// Seek speed (f32) on the one non-zero case, else 0.
    pub seek_speed: f32,
    /// Number of envelopes referring to this parameter (VERIFIED on Wind), kept as written.
    pub envelope_refs: u32,
    pub key_on: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundDefTemplate {
    /// 0..7. 1 = random, 2 = random no repeat, 3 = sequential no repeat, 0 = sequential (INFERRED, xoreos
    /// enum order); 4..7 UNKNOWN.
    pub play_mode: u32,
    /// Re-trigger interval in ms (VERIFIED against `Default_Dogs`/`Default_Whistle` behaviour).
    pub spawn_min_ms: u32,
    pub spawn_max_ms: u32,
    pub max_spawned: u32,
    /// Linear gain.
    pub volume: f32,
    /// Octaves (v0x40 only, else 0).
    pub pitch: f32,
    /// Octaves (v0x40 only, else 0), INFERRED.
    pub pitch_rand: f32,
    /// All template bytes (70 on v0x40, 60 on v0x37).
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundDefWave {
    /// 0 = wave-table entry, 2 = silence, 3 = programmer-supplied.
    pub kind: u32,
    /// Percent weight (100 default).
    pub weight: u32,
    pub file: Option<String>,
    /// FSB stem (`AMB_Default`, `AMB_Quads_Stream`, `VO_EN_Stream`).
    pub bank: Option<String>,
    /// Sample index inside the FSB (VERIFIED against FSB names, see tests).
    pub index: u32,
    /// Length (ms), UNKNOWN unit.
    pub length: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundDef {
    pub name: String,
    pub template: u32,
    pub waves: Vec<SoundDefWave>,
}

/// One candidate sample of an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaveRef {
    /// FSB stem, e.g. `"AMB_Default"`.
    pub bank: String,
    pub index: u32,
    /// Wave file base name (no extension), e.g. `"Tree_Rustle_01"`.
    pub name: String,
    pub weight: f32,
    /// Sound definition template gain (linear).
    pub gain: f32,
    /// Sound definition template pitch as a rate multiplier.
    pub pitch: f32,
    /// Owning sound definition path, e.g. `/Natural/River`.
    pub sound_def: String,
}

/// Facade over one event (flat list, file order). Volumes are linear, distances in metres.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub project: String,
    /// `Group/SubGroup/Event` (no project).
    pub path: String,
    pub name: String,
    /// 8 = complex event (layers), 16 = simple event (one instance).
    pub kind: u32,
    pub guid: Option<Vec<u8>>,
    pub volume: f32,
    /// Rate multiplier (2^octaves).
    pub pitch: f32,
    pub is_3d: bool,
    pub min_dist: f32,
    pub max_dist: f32,
    pub rolloff: Rolloff,
    /// True when any sound instance loops (`loop_mode == 0`).
    pub looped: bool,
    /// Re-trigger interval for scattered one-shots, seconds.
    pub spawn_interval_s: Option<(f32, f32)>,
    /// Header word 3 (0 on every disc event).
    pub volume_rand_db: f32,
    /// Header word 2, octaves.
    pub pitch_rand: f32,
    pub waves: Vec<WaveRef>,
    /// Sum of weights of silence entries in the used sound definitions (percent units like `weight`).
    pub silence_weight: f32,
    pub params: Vec<Param>,
    pub layers: Vec<Layer>,
    pub priority: u32,
    pub max_playbacks: u32,
    /// FMOD_MODE word (bits as `MODE_*`).
    pub mode: u32,
    pub fade_in_ms: u32,
    pub fade_out_ms: u32,
    /// Template play mode of the first used sound definition (`None` when no sound definition).
    pub play_mode: Option<u32>,
    pub category: String,
    pub user_props: Vec<UserProp>,
    /// Normalised 33 header words (v0x37 gets 10000 inserted at index 6). Index map in docs/FEV.md.
    pub header: Vec<u32>,
    /// Instance of a simple (kind 16) event.
    pub simple_instance: Option<SoundInstance>,
}

impl Event {
    /// `Project/Group/SubGroup/Event`.
    pub fn full_path(&self) -> String {
        format!("{}/{}", self.project, self.path)
    }

    /// Index of a parameter by name (case-insensitive).
    pub fn param_index(&self, name: &str) -> Option<usize> {
        self.params.iter().position(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Volume-vs-distance curve of a custom-rolloff event as `(metres, gain)` points, taken from the
    /// first volume envelope (target 12) driven by the `(distance)` parameter.
    pub fn distance_curve(&self) -> Option<Vec<(f32, f32)>> {
        let pi = self.param_index("(distance)")? as u32;
        let max = self.params.get(pi as usize)?.max;
        for l in &self.layers {
            for e in &l.envelopes {
                if e.param_index == pi && e.target == 12 && e.effect.is_none() && !e.points.is_empty() {
                    return Some(e.points.iter().map(|p| (p.x * max, p.y)).collect());
                }
            }
        }
        None
    }

    /// Product of all volume envelopes driven by `param` (by name) evaluated at `value` (parameter units).
    pub fn param_volume(&self, param: &str, value: f32) -> f32 {
        let Some(pi) = self.param_index(param) else { return 1.0 };
        let (min, max) = (self.params[pi].min, self.params[pi].max);
        let x = if max > min { ((value - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 };
        let mut g = 1.0;
        for l in &self.layers {
            for e in &l.envelopes {
                if e.param_index as usize == pi && e.target == 12 && e.effect.is_none() {
                    g *= e.eval(x);
                }
            }
        }
        g
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fev {
    /// 0x0040_0000, or 0x0037_0000 (Damage, ExhNoise).
    pub version: u32,
    /// Two u32 after the version (section sizes? UNKNOWN).
    pub header_unknown: [u32; 2],
    /// v0x40: `(type id, count)` x 33 allocation counts (UNKNOWN per id).
    pub table: Vec<(u32, u32)>,
    pub project: String,
    /// Wave bank names (FSB stems the sounds were built into).
    pub banks: Vec<String>,
    pub wave_banks: Vec<WaveBank>,
    /// Root category (`master`) with children (`music`...).
    pub category: Category,
    pub groups: Vec<EventGroup>,
    /// Flat events, file order.
    pub events: Vec<Event>,
    pub templates: Vec<SoundDefTemplate>,
    pub sound_defs: Vec<SoundDef>,
    /// u32 after the sound definitions (0 on every disc file; reverb definition count, INFERRED).
    pub reverb_count: u32,
    /// Music chunk body (`comp` [+ `sett` 1.0 1.0]), kept raw.
    pub music: Vec<u8>,
}

// ----------------------------------------------------------------------------------------------
// Reader
// ----------------------------------------------------------------------------------------------

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    fn left(&self) -> usize {
        self.b.len().saturating_sub(self.p)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p.checked_add(n).context("FEV offset overflow")?;
        let s = self
            .b
            .get(self.p..end)
            .with_context(|| format!("truncated FEV at 0x{:x} (+{n})", self.p))?;
        self.p = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into()?))
    }
    /// A count of items that each need at least `min_item` bytes; rejects absurd counts.
    fn count(&mut self, what: &str, min_item: usize) -> Result<usize> {
        let at = self.p;
        let n = self.u32()? as usize;
        ensure!(
            n.checked_mul(min_item.max(1)).is_some_and(|need| need <= self.left()),
            "implausible {what} count {n} at 0x{at:x}"
        );
        Ok(n)
    }
    fn string(&mut self) -> Result<String> {
        let at = self.p;
        let n = self.u32()? as usize;
        ensure!((1..=4096).contains(&n), "bad string length {n} at 0x{at:x}");
        let s = self.take(n)?;
        ensure!(s[n - 1] == 0, "string at 0x{at:x} not NUL terminated");
        Ok(String::from_utf8_lossy(&s[..n - 1]).into_owned())
    }
    /// True when a length-prefixed printable ASCII string starts here.
    fn peek_string(&self) -> bool {
        let Some(h) = self.b.get(self.p..self.p + 4) else { return false };
        let n = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
        if !(2..=200).contains(&n) {
            return false;
        }
        let Some(s) = self.b.get(self.p + 4..self.p + 4 + n) else { return false };
        s[n - 1] == 0 && s[..n - 1].iter().all(|&c| (32..127).contains(&c))
    }
}

fn read_category(r: &mut Rd, depth: usize) -> Result<Category> {
    ensure!(depth < 16, "category tree too deep");
    let name = r.string()?;
    let volume = r.f32()?;
    let pitch = r.f32()?;
    let unknown = [r.u32()?, r.u32()?];
    let n = r.count("subcategory", 4)?;
    let mut children = Vec::new();
    for _ in 0..n {
        children.push(read_category(r, depth + 1)?);
    }
    Ok(Category { name, volume, pitch, unknown, children })
}

fn read_props(r: &mut Rd) -> Result<Vec<UserProp>> {
    let n = r.count("user property", 8)?;
    let mut out = Vec::new();
    for _ in 0..n {
        let name = r.string()?;
        let ty = r.u32()?;
        let value = match ty {
            0 => PropValue::Int(r.u32()?),
            1 => PropValue::Float(r.f32()?),
            2 => PropValue::Str(r.string()?),
            t => bail!("unknown user property type {t} at 0x{:x}", r.p),
        };
        out.push(UserProp { name, value });
    }
    Ok(out)
}

fn read_instance(r: &mut Rd) -> Result<SoundInstance> {
    let sound_def = r.u16()?;
    let start = r.f32()?;
    let length = r.f32()?;
    let mut raw = [0u32; 12];
    for w in raw.iter_mut() {
        *w = r.u32()?;
    }
    Ok(SoundInstance { sound_def, start, length, loop_mode: raw[1], raw })
}

fn read_envelope(r: &mut Rd, v40: bool) -> Result<Envelope> {
    let owner = r.i32()?;
    let effect = if r.peek_string() { Some(r.string()?) } else {
        let _ = r.u32()?;
        None
    };
    let c = r.u32()?;
    let target = r.u32()?;
    let e = if v40 { r.u32()? } else { 0 };
    let n = r.count("envelope point", 12)?;
    let mut points = Vec::new();
    for _ in 0..n {
        points.push(EnvPoint { x: r.f32()?, y: r.f32()?, shape: r.u32()? });
    }
    let param_index = r.u32()?;
    let tail1 = r.u32()?;
    Ok(Envelope { owner, effect, c, target, e, points, param_index, tail1 })
}

fn read_layer(r: &mut Rd, v40: bool) -> Result<Layer> {
    let kind = r.u16()?;
    let control_param = r.i16()?;
    let flags = r.u16()?;
    let ninst = r.u16()? as usize;
    let nenv = r.u16()? as usize;
    ensure!(ninst * 58 + nenv * 24 <= r.left(), "layer counts exceed file at 0x{:x}", r.p);
    let mut instances = Vec::new();
    for _ in 0..ninst {
        instances.push(read_instance(r)?);
    }
    let mut envelopes = Vec::new();
    for _ in 0..nenv {
        envelopes.push(read_envelope(r, v40)?);
    }
    Ok(Layer { kind, control_param, flags, instances, envelopes })
}

fn read_param(r: &mut Rd) -> Result<Param> {
    let name = r.string()?;
    let velocity = r.f32()?;
    let min = r.f32()?;
    let max = r.f32()?;
    let flags = r.u32()?;
    let seek_speed = r.f32()?;
    let envelope_refs = r.u32()?;
    let nkeys = r.count("key-on", 4)?;
    let mut key_on = Vec::new();
    for _ in 0..nkeys {
        key_on.push(r.f32()?);
    }
    Ok(Param { name, velocity, min, max, flags, seek_speed, envelope_refs, key_on })
}

/// Raw event before it is joined with sound definitions.
struct RawEvent {
    kind: u32,
    name: String,
    guid: Option<Vec<u8>>,
    header: Vec<u32>,
    layers: Vec<Layer>,
    params: Vec<Param>,
    user_props: Vec<UserProp>,
    category: String,
    simple_instance: Option<SoundInstance>,
}

fn read_event(r: &mut Rd, v40: bool) -> Result<RawEvent> {
    let kind = r.u32()?;
    ensure!(kind == 8 || kind == 0x10, "unknown event type {kind} at 0x{:x}", r.p);
    let name = r.string()?;
    let guid = if v40 { Some(r.take(16)?.to_vec()) } else { None };
    let nwords = if v40 { 33 } else { 32 };
    let mut header = Vec::with_capacity(33);
    for _ in 0..nwords {
        header.push(r.u32()?);
    }
    if !v40 {
        header.insert(6, 10000);
    }
    let nlayers = r.count("layer", 2)?;
    if kind == 0x10 {
        ensure!(nlayers == 1, "simple event with {nlayers} layers");
        let inst = read_instance(r)?;
        let _one = r.u32()?;
        let category = r.string()?;
        return Ok(RawEvent {
            kind,
            name,
            guid,
            header,
            layers: Vec::new(),
            params: Vec::new(),
            user_props: Vec::new(),
            category,
            simple_instance: Some(inst),
        });
    }
    let mut layers = Vec::new();
    for _ in 0..nlayers {
        layers.push(read_layer(r, v40)?);
    }
    let nparams = r.count("parameter", 28)?;
    let mut params = Vec::new();
    for _ in 0..nparams {
        params.push(read_param(r)?);
    }
    let user_props = read_props(r)?;
    let _one = r.u32()?;
    let category = r.string()?;
    Ok(RawEvent { kind, name, guid, header, layers, params, user_props, category, simple_instance: None })
}

struct Tree {
    groups: Vec<EventGroup>,
    raw: Vec<(String, RawEvent)>,
}

fn read_group(r: &mut Rd, v40: bool, path: &mut Vec<String>, tree: &mut Tree, depth: usize) -> Result<EventGroup> {
    ensure!(depth < 16, "event group tree too deep");
    let name = r.string()?;
    let user_props = read_props(r)?;
    let nsub = r.count("event group", 8)?;
    let nev = r.count("event", 8)?;
    path.push(name.clone());
    let mut groups = Vec::new();
    for _ in 0..nsub {
        groups.push(read_group(r, v40, path, tree, depth + 1)?);
    }
    let mut events = Vec::new();
    for _ in 0..nev {
        let ev = read_event(r, v40)?;
        let full = format!("{}/{}", path.join("/"), ev.name);
        events.push(tree.raw.len());
        tree.raw.push((full, ev));
    }
    path.pop();
    Ok(EventGroup { name, user_props, groups, events })
}

fn bool_bits(mode: u32, bit: u32) -> bool {
    mode & bit != 0
}

fn base_name(file: &str) -> String {
    let f = file.rsplit(['/', '\\']).next().unwrap_or(file);
    match f.rfind('.') {
        Some(i) if i > 0 => f[..i].to_string(),
        _ => f.to_string(),
    }
}

fn fl(h: &[u32], i: usize) -> f32 {
    f32::from_bits(h.get(i).copied().unwrap_or(0))
}

fn build_event(project: &str, path: String, raw: RawEvent, templates: &[SoundDefTemplate], sds: &[SoundDef]) -> Event {
    let h = &raw.header;
    let mode = h.get(7).copied().unwrap_or(0);
    let rolloff = if bool_bits(mode, MODE_3D_CUSTOMROLLOFF) {
        Rolloff::Custom
    } else if bool_bits(mode, MODE_3D_LINEARROLLOFF) {
        Rolloff::Linear
    } else {
        Rolloff::Inverse
    };
    let mut instances: Vec<&SoundInstance> = Vec::new();
    for l in &raw.layers {
        instances.extend(l.instances.iter());
    }
    if let Some(i) = &raw.simple_instance {
        instances.push(i);
    }
    let looped = instances.iter().any(|i| i.loop_mode == 0);
    let mut waves: Vec<WaveRef> = Vec::new();
    let mut silence = 0.0f32;
    let mut spawn: Option<(f32, f32)> = None;
    let mut play_mode: Option<u32> = None;
    let mut seen: Vec<u16> = Vec::new();
    for inst in &instances {
        let Some(sd) = sds.get(inst.sound_def as usize) else { continue };
        let tpl = templates.get(sd.template as usize);
        if play_mode.is_none() {
            play_mode = tpl.map(|t| t.play_mode);
        }
        if spawn.is_none() {
            if let Some(t) = tpl {
                if t.spawn_max_ms > 0 && inst.loop_mode != 0 {
                    spawn = Some((t.spawn_min_ms as f32 / 1000.0, t.spawn_max_ms as f32 / 1000.0));
                }
            }
        }
        if seen.contains(&inst.sound_def) {
            continue;
        }
        seen.push(inst.sound_def);
        let gain = tpl.map_or(1.0, |t| t.volume);
        let pitch = tpl.map_or(1.0, |t| 2f32.powf(t.pitch));
        for w in &sd.waves {
            match (w.kind, &w.file, &w.bank) {
                (0, Some(file), Some(bank)) => waves.push(WaveRef {
                    bank: bank.clone(),
                    index: w.index,
                    name: base_name(file),
                    weight: w.weight as f32,
                    gain,
                    pitch,
                    sound_def: sd.name.clone(),
                }),
                (2, _, _) => silence += w.weight as f32,
                _ => {}
            }
        }
    }
    Event {
        project: project.to_string(),
        path,
        name: raw.name,
        kind: raw.kind,
        guid: raw.guid,
        volume: fl(h, 0),
        pitch: 2f32.powf(fl(h, 1)),
        is_3d: bool_bits(mode, MODE_3D),
        min_dist: fl(h, 8),
        max_dist: fl(h, 9),
        rolloff,
        looped,
        spawn_interval_s: spawn,
        volume_rand_db: fl(h, 3),
        pitch_rand: fl(h, 2),
        waves,
        silence_weight: silence,
        params: raw.params,
        layers: raw.layers,
        priority: h.get(4).copied().unwrap_or(0),
        max_playbacks: h.get(5).copied().unwrap_or(0),
        mode,
        fade_in_ms: h.get(27).copied().unwrap_or(0),
        fade_out_ms: h.get(28).copied().unwrap_or(0),
        play_mode,
        category: raw.category,
        user_props: raw.user_props,
        header: raw.header,
        simple_instance: raw.simple_instance,
    }
}

impl Fev {
    /// Parses a whole `.fev`; errors on truncation, unknown structure or leftover bytes.
    pub fn parse(bytes: &[u8]) -> Result<Fev> {
        let mut r = Rd::new(bytes);
        ensure!(r.take(4)? == b"FEV1", "not an FEV1 file");
        let version = r.u32()?;
        ensure!(version == 0x0040_0000 || version == 0x0037_0000, "unsupported FEV version 0x{version:x}");
        let v40 = version >= 0x0040_0000;
        let header_unknown = [r.u32()?, r.u32()?];
        let mut table = Vec::new();
        if v40 {
            let n = r.count("type table", 8)?;
            for _ in 0..n {
                table.push((r.u32()?, r.u32()?));
            }
        }
        let project = r.string()?;
        let nbanks = r.count("wave bank", 12)?;
        let mut wave_banks = Vec::new();
        for _ in 0..nbanks {
            let stream_type = r.u32()?;
            let max_streams = r.u32()?;
            let hash = if v40 { r.take(8)?.to_vec() } else { Vec::new() };
            let name = r.string()?;
            wave_banks.push(WaveBank { name, stream_type, max_streams, hash });
        }
        let category = read_category(&mut r, 0)?;
        let ngroups = r.count("event group", 8)?;
        let mut tree = Tree { groups: Vec::new(), raw: Vec::new() };
        let mut path = Vec::new();
        for _ in 0..ngroups {
            let g = read_group(&mut r, v40, &mut path, &mut tree, 0)?;
            tree.groups.push(g);
        }
        // Sound definition templates (70 bytes on v0x40, 60 on v0x37).
        let tsize = if v40 { 70 } else { 60 };
        let ntpl = r.count("sound definition template", tsize)?;
        let mut templates = Vec::new();
        for _ in 0..ntpl {
            let raw = r.take(tsize)?.to_vec();
            let u = |o: usize| u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
            let f = |o: usize| f32::from_bits(u(o));
            templates.push(SoundDefTemplate {
                play_mode: u(0),
                spawn_min_ms: u(4),
                spawn_max_ms: u(8),
                max_spawned: u(12),
                volume: f(16),
                pitch: if v40 { f(36) } else { 0.0 },
                pitch_rand: if v40 { f(52) } else { 0.0 },
                raw,
            });
        }
        let nsd = r.count("sound definition", 12)?;
        let mut sound_defs = Vec::new();
        for _ in 0..nsd {
            let name = r.string()?;
            let template = r.u32()?;
            let nw = r.count("wave entry", 8)?;
            let mut waves = Vec::new();
            for _ in 0..nw {
                let kind = r.u32()?;
                let weight = r.u32()?;
                match kind {
                    0 => {
                        let file = r.string()?;
                        let bank = r.string()?;
                        let index = r.u32()?;
                        let length = r.u32()?;
                        waves.push(SoundDefWave { kind, weight, file: Some(file), bank: Some(bank), index, length });
                    }
                    2 | 3 => waves.push(SoundDefWave { kind, weight, file: None, bank: None, index: 0, length: 0 }),
                    k => bail!("unknown wave entry type {k} at 0x{:x}", r.p),
                }
            }
            sound_defs.push(SoundDef { name, template, waves });
        }
        let reverb_count = r.u32()?;
        let chunk = r.u32()? as usize;
        ensure!(chunk >= 4, "bad music chunk size {chunk}");
        let music = r.take(chunk - 4).or_else(|_| r.take(r.left()))?.to_vec();
        ensure!(r.left() == 0, "{} leftover bytes after the music chunk", r.left());

        let events = tree
            .raw
            .into_iter()
            .map(|(full, raw)| build_event(&project, full, raw, &templates, &sound_defs))
            .collect();
        Ok(Fev {
            version,
            header_unknown,
            table,
            banks: wave_banks.iter().map(|b| b.name.clone()).collect(),
            project,
            wave_banks,
            category,
            groups: tree.groups,
            events,
            templates,
            sound_defs,
            reverb_count,
            music,
        })
    }

    pub fn load(path: &Path) -> Result<Fev> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Fev::parse(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    /// Looks an event up by `Project/Group/Sub/Event`, `Group/Sub/Event` or a bare event name
    /// (first match in file order). Case-insensitive; `\` accepted.
    pub fn event(&self, path_or_name: &str) -> Option<&Event> {
        let q = path_or_name.replace('\\', "/");
        let q = q.trim_matches('/');
        let proj_prefix = format!("{}/", self.project);
        let q = match q.get(..proj_prefix.len()) {
            Some(head) if q.len() > proj_prefix.len() && head.eq_ignore_ascii_case(&proj_prefix) => &q[proj_prefix.len()..],
            _ => q,
        };
        if let Some(e) = self.events.iter().find(|e| e.path.eq_ignore_ascii_case(q)) {
            return Some(e);
        }
        if !q.contains('/') {
            return self.events.iter().find(|e| e.name.eq_ignore_ascii_case(q));
        }
        None
    }

    /// True when `name` is a wave file base name used by any sound definition.
    pub fn has_wave(&self, name: &str) -> bool {
        self.sound_defs.iter().any(|sd| {
            sd.waves.iter().any(|w| w.file.as_deref().is_some_and(|f| base_name(f).eq_ignore_ascii_case(name)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, ext, out);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
                out.push(p);
            }
        }
    }

    /// Every `.fev` under disc/media/audio plus the extracted world banks; empty when `disc/` is absent.
    fn all_fev() -> Vec<PathBuf> {
        let r = root();
        let mut v = Vec::new();
        walk(&r.join("disc/media/audio"), "fev", &mut v);
        walk(&r.join("data/extracted/world/bin_zip"), "fev", &mut v);
        v.sort();
        v
    }

    #[test]
    fn rejects_garbage() {
        assert!(Fev::parse(b"").is_err());
        assert!(Fev::parse(b"FEV1\0\0@\0").is_err());
        assert!(Fev::parse(&[0u8; 64]).is_err());
        let mut v = b"FEV1".to_vec();
        v.extend_from_slice(&0x0040_0000u32.to_le_bytes());
        v.extend_from_slice(&[0xff; 200]);
        assert!(Fev::parse(&v).is_err());
    }

    #[test]
    fn all_disc_fevs_parse_without_leftover() {
        let files = all_fev();
        if files.is_empty() {
            return;
        }
        let (mut events, mut waves) = (0, 0);
        for f in &files {
            let fev = Fev::load(f).unwrap_or_else(|e| panic!("{}: {e:#}", f.display()));
            events += fev.events.len();
            waves += fev.sound_defs.iter().map(|s| s.waves.len()).sum::<usize>();
        }
        eprintln!("fev: {} files, {events} events, {waves} wave entries", files.len());
        assert!(files.len() >= 46);
    }

    #[test]
    fn wave_refs_match_fsb_samples() {
        let files = all_fev();
        if files.is_empty() {
            return;
        }
        let mut fsbs: HashMap<String, PathBuf> = HashMap::new();
        for d in ["disc/media/audio", "data/extracted/world/bin_zip"] {
            let mut v = Vec::new();
            walk(&root().join(d), "fsb", &mut v);
            for p in v {
                if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                    fsbs.entry(s.to_string()).or_insert(p);
                }
            }
        }
        let mut cache: HashMap<String, Vec<crate::fsb::Sample>> = HashMap::new();
        let (mut total, mut oob, mut name_bad, mut no_fsb) = (0, 0, 0, 0);
        for f in &files {
            let fev = Fev::load(f).unwrap();
            for sd in &fev.sound_defs {
                for w in &sd.waves {
                    let (Some(bank), Some(file)) = (&w.bank, &w.file) else { continue };
                    total += 1;
                    let Some(path) = fsbs.get(bank) else {
                        no_fsb += 1;
                        continue;
                    };
                    if !cache.contains_key(bank) {
                        let bytes = std::fs::read(path).unwrap();
                        cache.insert(bank.clone(), crate::fsb::parse(&bytes).unwrap());
                    }
                    let samples = &cache[bank];
                    let Some(s) = samples.get(w.index as usize) else {
                        oob += 1;
                        continue;
                    };
                    let want = base_name(file).to_ascii_lowercase();
                    let got = s.name.to_ascii_lowercase();
                    let got = got.trim_end_matches(".wav");
                    let n = want.len().min(got.len()).min(25);
                    if want.as_bytes()[..n] != got.as_bytes()[..n] {
                        name_bad += 1;
                    }
                }
            }
        }
        eprintln!("fev waves: {total} refs, {oob} out of range, {name_bad} name mismatches, {no_fsb} without fsb");
        assert_eq!(no_fsb, 0);
        // Measured 62 of 6061 refs differ (stale FEV vs shipped FSB: Damage, Main_Town, UIInGame...).
        assert!(oob <= 10, "{oob} wave refs out of range");
        assert!(name_bad <= 70, "{name_bad} wave name mismatches");
        assert!(total > 6000);
    }

    #[test]
    fn soundscape_events_resolve() {
        let dir = root().join("data/extracted/converted/audio/soundscape");
        let files = all_fev();
        if files.is_empty() || !dir.is_dir() {
            return;
        }
        let mut by_project: HashMap<String, Fev> = HashMap::new();
        for f in &files {
            let fev = Fev::load(f).unwrap();
            by_project.entry(fev.project.to_ascii_lowercase()).or_insert(fev);
        }
        let (mut total, mut ok) = (0usize, 0usize);
        let mut bad: HashMap<String, usize> = HashMap::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !stem.starts_with("colorado_") || !matches!(p.extension().and_then(|x| x.to_str()), Some("json")) {
                continue;
            }
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            for key in ["track3d", "track2d"] {
                let Some(arr) = v["track_audio"][key].as_array() else { continue };
                for o in arr {
                    let path = o["event_path"].as_str().unwrap_or("");
                    let name = o["event_name"].as_str().unwrap_or("");
                    total += 1;
                    let proj = path.split('/').next().unwrap_or("").to_ascii_lowercase();
                    let hit = by_project.get(&proj).and_then(|f| f.event(&format!("{path}/{name}"))).is_some();
                    if hit {
                        ok += 1;
                    } else {
                        *bad.entry(format!("{path}|{name}")).or_default() += 1;
                    }
                }
            }
        }
        eprintln!("soundscape objects: {ok}/{total} resolve; unresolved {bad:?}");
        // The only misses are 15 objects with an empty event_path (Mountains_Car/House_Door_Slam).
        assert!(total == 0 || ok * 1000 >= total * 999, "{ok}/{total}");
    }

    #[test]
    fn ui_cues_resolve() {
        let ui = root().join("data/extracted/converted/audio/ui/ui4audio.json");
        let files = all_fev();
        if files.is_empty() || !ui.is_file() {
            return;
        }
        let fevs: Vec<Fev> = files.iter().filter_map(|f| Fev::load(f).ok()).collect();
        let uiin = fevs.iter().find(|f| f.project == "UIInGame").expect("UIInGame.fev");
        let general = fevs.iter().find(|f| f.project == "General");
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&ui).unwrap()).unwrap();
        let mut cues = std::collections::BTreeSet::new();
        let mut resolved = std::collections::BTreeSet::new();
        for row in &rows {
            let Some(cue) = row["cue"].as_str().filter(|c| !c.trim().is_empty()) else { continue };
            cues.insert(cue.to_string());
            // `duplicate_attributes.cue` holds [first, last]; the loader keeps the last, FMOD sees both.
            let mut cands = vec![cue.to_string()];
            if let Some(d) = row["duplicate_attributes"]["cue"].as_array() {
                cands.extend(d.iter().filter_map(|x| x.as_str().map(String::from)));
            }
            let hit = cands.iter().any(|c| {
                uiin.event(c).is_some() || uiin.has_wave(c) || general.is_some_and(|g| g.event(c).is_some())
            });
            if hit {
                resolved.insert(cue.to_string());
            }
        }
        eprintln!("ui cues: {}/{} resolve (UIInGame + General, duplicates counted)", resolved.len(), cues.len());
        assert!(cues.len() >= 300);
        assert_eq!(resolved.len(), cues.len(), "unresolved: {:?}", cues.difference(&resolved).collect::<Vec<_>>());
    }

    #[test]
    fn facade_basics() {
        let p = root().join("data/extracted/world/bin_zip/AMB_Default.fev");
        if !p.is_file() {
            return;
        }
        let f = Fev::load(&p).unwrap();
        let river = f.event("TrackAmbient/Track3D/Default_River").or_else(|| f.event("Default_River")).unwrap();
        assert!(river.looped && river.is_3d && river.rolloff == Rolloff::Custom);
        assert!(!river.waves.is_empty());
        let whistle = f.event("AMB_Default/TrackAmbient/Track3D/Default_Whistle").unwrap();
        assert!(!whistle.looped && whistle.spawn_interval_s.is_some());
        assert!(river.distance_curve().is_some());
    }
}
