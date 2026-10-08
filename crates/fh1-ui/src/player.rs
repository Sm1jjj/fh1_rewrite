//! Scene player: runs an Anark scene's components (slides, keyframe tracks, events) and produces
//! a draw list. Engine independent; the semantics follow `docs/UI.md` / the reference renderer
//! that reproduced FH1's pause menu and HUD (scratch `anark.py` + `render.py`).
//!
//! Per frame: [`Player::update`] advances component clocks, [`Player::evaluate`] resolves every
//! object's properties (S1 master values → bsg fallback → Master Slide → current slide → event
//! overrides), composes transforms/opacity down the bsg tree and returns models and texts sorted
//! in draw order. Matrices are row-major 4×4 (`m[row][col]`, column vectors).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::anark::bgf::{Command, ObjectKind, Slide};
use crate::anark::fbf::{self, Payload};
use crate::anark::{typed, PropValue, Scene};
use crate::hash::{ahash31, MASK27};
use crate::names as n;

pub type Mat4 = [[f32; 4]; 4];

/// FxHash (rustc's): the property maps are keyed by small integers and rebuilt per frame, where
/// SipHash dominated the cost.
#[derive(Default, Clone, Copy)]
pub struct Fx(u64);

impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.write_u64(i as u64);
    }
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

pub const IDENTITY: Mat4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];

pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut r = [[0.0; 4]; 4];
    for (i, row) in r.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    r
}

pub fn apply(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    let f = |i: usize| m[i][0] * p[0] + m[i][1] * p[1] + m[i][2] * p[2] + m[i][3];
    [f(0), f(1), f(2)]
}

fn translate(p: [f32; 3]) -> Mat4 {
    let mut m = IDENTITY;
    (m[0][3], m[1][3], m[2][3]) = (p[0], p[1], p[2]);
    m
}

fn scale(s: [f32; 3]) -> Mat4 {
    let mut m = IDENTITY;
    (m[0][0], m[1][1], m[2][2]) = (s[0], s[1], s[2]);
    m
}

/// Euler XYZ (`rotationorder` 4) as Rz·Ry·Rx; positive z is counter-clockwise on screen
/// (VERIFIED by the tach ticks). x/y signs are a GUESS.
/// Anark's space is left-handed (D3D, +z into the screen) while these matrices are right-handed, so
/// x and y rotations take the opposite sign; z is unchanged. VERIFIED on the tach redline: its
/// `HalfRectan` quad (modelled in X–Z, turned up by rotation.x) only puts the red arc between the
/// redline and the dial's end, as the Xenia frames show, with the sign flipped.
fn rotate(r: [f32; 3]) -> Mat4 {
    let (sx, cx) = (-r[0]).sin_cos();
    let (sy, cy) = (-r[1]).sin_cos();
    let (sz, cz) = r[2].sin_cos();
    let x = [[1.0, 0.0, 0.0, 0.0], [0.0, cx, -sx, 0.0], [0.0, sx, cx, 0.0], [0.0, 0.0, 0.0, 1.0]];
    let y = [[cy, 0.0, sy, 0.0], [0.0, 1.0, 0.0, 0.0], [-sy, 0.0, cy, 0.0], [0.0, 0.0, 0.0, 1.0]];
    let z = [[cz, -sz, 0.0, 0.0], [sz, cz, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
    mul(&z, &mul(&y, &x))
}

/// Play mode from a slide's flags (GUESS: bit 0 = play on enter, bits 2..4 = end behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndMode {
    /// 0: static / paused.
    None,
    /// 2: stop at the end.
    Stop,
    /// 3: loop.
    Loop,
    /// 4: play through to the next slide.
    Next,
    Other(u8),
}

pub fn end_mode(flags: u8) -> EndMode {
    match (flags >> 2) & 7 {
        0 => EndMode::None,
        2 => EndMode::Stop,
        3 => EndMode::Loop,
        4 => EndMode::Next,
        o => EndMode::Other(o),
    }
}

/// A component's playhead.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    /// Index into `bgf.slides`.
    pub slide: usize,
    /// ms, in the same time base as the slide's `start..end` and its tracks.
    pub time: f32,
    pub playing: bool,
}

/// One texture slot of a material, ready to draw.
#[derive(Debug, Clone)]
pub struct DrawTexture {
    /// Normalised asset path: `GAME:\MEDIA\UI\TEXTURES\HORIZON\HUD\X.TGA` → `horizon/hud/x.png`.
    pub path: String,
    /// `u' = u*m[0] + v*m[1] + m[2]`, `v' = u*m[3] + v*m[4] + m[5]`.
    pub uv: [f32; 6],
    /// `tilingmodehorz`, `tilingmodevert`: 0 and 2 wrap (2 is used by the pause tapes whose second
    /// slot is offset by U 0.86), 1 mirror (GUESS).
    pub tiling: [u32; 2],
    /// The slot's UV rotation is animated (a track drives `rotationuv`), e.g. the tach redline.
    pub rotating: bool,
}

#[derive(Debug, Clone)]
pub struct DrawMaterial {
    /// Index group of the model's mesh this material covers.
    pub submesh: usize,
    /// 0..1.
    pub diffuse: [f32; 3],
    /// Material opacity 0..1 (node opacity is separate).
    pub opacity: f32,
    pub additive: bool,
    pub textures: Vec<DrawTexture>,
}

#[derive(Debug, Clone)]
pub struct DrawText {
    pub string: String,
    pub loc_key: String,
    pub font: String,
    pub size: f32,
    pub horzalign: u32,
    pub vertalign: u32,
    pub leading: f32,
    pub tracking: f32,
    pub wordwrap: bool,
    /// 0..1.
    pub color: [f32; 4],
    /// Authored width of the default string at `size` (v1008 scenes).
    pub text_width: Option<f32>,
}

#[derive(Debug, Clone)]
pub enum DrawKind {
    /// `mesh` indexes `fbf.meshes`.
    Model { mesh: usize, materials: Vec<DrawMaterial> },
    Text(DrawText),
}

#[derive(Debug, Clone)]
pub struct Draw {
    /// bsg node index.
    pub node: usize,
    pub name: String,
    pub layer: u8,
    pub world: Mat4,
    /// Accumulated node opacity 0..1.
    pub opacity: f32,
    pub kind: DrawKind,
}

/// The camera of a layer. Ortho: 1 unit = 1 px, `sx = w/2 + (x - pos.x)`, `sy = h/2 - (y - pos.y)`.
#[derive(Debug, Clone, Copy)]
pub struct LayerCamera {
    pub position: [f32; 3],
    pub orthographic: bool,
    /// Degrees (vertical: GUESS).
    pub fov: f32,
}

impl Default for LayerCamera {
    fn default() -> Self {
        Self { position: [0.0, 0.0, -600.0], orthographic: true, fov: 60.0 }
    }
}

#[derive(Debug, Default)]
pub struct Frame {
    /// Back to front.
    pub draws: Vec<Draw>,
    pub cameras: HashMap<u8, LayerCamera>,
    /// World matrix and layer of every bgf object that has a scene node (and isn't under a fully
    /// transparent node).
    pub objects: FxMap<usize, (Mat4, u8)>,
}

impl Frame {
    pub fn camera(&self, layer: u8) -> LayerCamera {
        self.cameras.get(&layer).or_else(|| self.cameras.values().next()).copied().unwrap_or_default()
    }
}

/// A data-binding contract (behaviour record): its type and field paths.
#[derive(Debug, Clone)]
pub struct Contract {
    /// bgf index of the behaviour record.
    pub behavior: usize,
    /// The object the behaviour sits on.
    pub owner: usize,
    pub kind: String,
    /// (27-bit field-name hash, path such as `parent.Layer.ScreenTitle.TEXT_SCREEN_TITLE`).
    /// Paths are relative to the behaviour record: `parent` is the owner (VERIFIED on 925: only
    /// then do the SUPER_STACKER `parent.parent.MessageCount` paths resolve).
    pub fields: Vec<(u32, String)>,
}

impl Contract {
    /// The object a field path points at.
    pub fn target(&self, player: &Player, field: usize) -> Option<usize> {
        player.resolve_path(self.behavior, &self.fields.get(field)?.1)
    }
}

pub struct Player {
    pub scene: Scene,
    /// bgf object index of the first non-behaviour object with each scene id.
    by_id: HashMap<u32, usize>,
    /// Component → its slide indices (Master Slide first).
    slides_of: HashMap<usize, Vec<usize>>,
    /// Object → owning component (time context).
    comp_of: Vec<Option<usize>>,
    /// (slide, object) → on that slide.
    on_slide: FxMap<(usize, usize), bool>,
    /// fbf record indices: model id → materials (sorted by submesh); material id → images.
    materials_of: HashMap<u32, Vec<usize>>,
    images_of: HashMap<u32, Vec<usize>>,
    /// fbf record index by id.
    record_of: HashMap<u32, usize>,
    /// Children names: bgf object → (name → child object), for contract paths.
    children: Vec<Vec<usize>>,
    clocks: FxMap<usize, Clock>,
    /// Image objects whose UV rotation is animated by a track.
    uv_rotating: std::collections::HashSet<usize>,
    /// Values set by events / the game: object → key27 → value.
    overrides: HashMap<usize, HashMap<u32, Value>>,
    /// Static part of every object's properties, built once: S1 props, then the Master Slide entry
    /// props (always applied: every component has a clock after `reset`).
    base: Vec<Vals>,
    /// Entry props of each non-master slide, pre-converted.
    slide_props: Vec<Vec<(usize, u32, Value)>>,
    /// Per bsg node: its bgf object and fbf record (precomputed id lookups).
    node_obj: Vec<Option<usize>>,
    node_rec: Vec<Option<usize>>,
    /// fbf record index → engine texture path of that Image record.
    tex_paths: FxMap<usize, String>,
    /// Per bsg node, from the static base only: local matrix, position, own opacity 0..1. Used for
    /// nodes whose object has no overlay entry this frame.
    static_local: Vec<(Mat4, [f32; 3], f32)>,
    /// Bumped by every change that can alter `evaluate()`: reset, slide changes, seeks, property overrides that differ,
    /// play-state changes and clock advances (see [`Player::revision`]).
    revision: u64,
}

/// A resolved property value. Strings are owned so the game can set any text.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(u32),
    Float(f32),
    Bool(bool),
    Str(String),
}

impl Value {
    fn f(&self) -> Option<f32> {
        match *self {
            Value::Float(f) => Some(f),
            Value::Int(i) => Some(i as f32),
            Value::Bool(b) => Some(b as u32 as f32),
            Value::Str(_) => None,
        }
    }

    fn i(&self) -> Option<u32> {
        match *self {
            Value::Int(i) => Some(i),
            Value::Float(f) => Some(f as u32),
            Value::Bool(b) => Some(b as u32),
            Value::Str(_) => None,
        }
    }
}

type Vals = FxMap<u32, Value>;

/// A per-frame property: borrowed from the static base / slide props / overrides, or a track value.
#[derive(Clone, Copy)]
enum Pv<'a> {
    V(&'a Value),
    F(f32),
}

impl Pv<'_> {
    fn f(self) -> Option<f32> {
        match self {
            Pv::V(v) => v.f(),
            Pv::F(f) => Some(f),
        }
    }
    fn i(self) -> Option<u32> {
        match self {
            Pv::V(v) => v.i(),
            Pv::F(f) => Some(f as u32),
        }
    }
}

/// The frame's dynamic layer over `Player::base`: (object << 32 | key27) → value.
type Overlay<'a> = FxMap<u64, Pv<'a>>;

/// One object's resolved properties: overlay first, then the static base.
#[derive(Clone, Copy)]
struct Props<'a> {
    base: Option<&'a Vals>,
    over: &'a Overlay<'a>,
    object: u64,
}

impl<'a> Props<'a> {
    fn get(&self, k: u32) -> Option<Pv<'a>> {
        if let Some(v) = self.over.get(&(self.object << 32 | k as u64)) {
            return Some(*v);
        }
        self.base?.get(&k).map(Pv::V)
    }
    fn has(&self, k: u32) -> bool {
        self.get(k).is_some()
    }
    fn f(&self, k: u32, d: f32) -> f32 {
        self.get(k).and_then(Pv::f).unwrap_or(d)
    }
    fn s(&self, k: u32) -> Option<&'a str> {
        match self.get(k) {
            Some(Pv::V(Value::Str(s))) => Some(s.as_str()),
            _ => None,
        }
    }
}

impl Player {
    pub fn new(scene: Scene) -> Self {
        let b = &scene.bgf;
        let mut by_id = HashMap::new();
        let mut children = vec![Vec::new(); b.objects.len()];
        for (i, o) in b.objects.iter().enumerate() {
            if o.kind != ObjectKind::Behavior {
                by_id.entry(o.id).or_insert(i);
            }
            if o.parent >= 0 {
                if let Some(c) = children.get_mut(o.parent as usize) {
                    c.push(i);
                }
            }
        }
        let mut slides_of: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, s) in b.slides.iter().enumerate() {
            slides_of.entry(s.component as usize).or_default().push(i);
        }
        let mut on_slide = FxMap::default();
        for (si, s) in b.slides.iter().enumerate() {
            let master = s.name == "Master Slide";
            for e in &s.entries {
                on_slide.insert((si, e.object as usize), e.active != 0 || master);
            }
        }
        let mut comp_of = vec![None; b.objects.len()];
        for (&c, sl) in &slides_of {
            for e in &b.slides[sl[0]].entries {
                let o = e.object as usize;
                if o != c && o < comp_of.len() {
                    comp_of[o] = Some(c);
                }
            }
        }
        let (mut materials_of, mut images_of, mut record_of) = (HashMap::new(), HashMap::new(), HashMap::new());
        if let Some(f) = &scene.fbf {
            for (ri, r) in f.records.iter().enumerate() {
                record_of.insert(r.id, ri);
                match &r.payload {
                    Payload::Material(m) => materials_of.entry(m.model).or_insert_with(Vec::new).push(ri),
                    Payload::Image(im) => images_of.entry(im.material).or_insert_with(Vec::new).push(ri),
                    _ => {}
                }
            }
            for list in materials_of.values_mut() {
                list.sort_by_key(|&ri| match &f.records[ri].payload {
                    Payload::Material(m) => m.submesh,
                    _ => 0,
                });
            }
        }
        let uv_rotating = scene.bgf.tracks.iter().filter(|t| t.key & MASK27 == n::ROTATIONUV).map(|t| t.object as usize).collect();
        let mut p = Self {
            uv_rotating,
            scene,
            by_id,
            slides_of,
            comp_of,
            on_slide,
            materials_of,
            images_of,
            record_of,
            children,
            clocks: FxMap::default(),
            node_obj: Vec::new(),
            node_rec: Vec::new(),
            tex_paths: FxMap::default(),
            static_local: Vec::new(),
            overrides: HashMap::new(),
            base: Vec::new(),
            slide_props: Vec::new(),
            revision: 0,
        };
        p.build_static();
        p.reset();
        p
    }

    /// Fill `base` and `slide_props` (see their docs).
    fn build_static(&mut self) {
        let b = &self.scene.bgf;
        let mut base: Vec<Vals> = b.objects.iter().map(|o| o.props.iter().map(|p| (p.key & MASK27, self.value(p.key, p.raw))).collect()).collect();
        let mut slide_props = vec![Vec::new(); b.slides.len()];
        let masters: std::collections::HashSet<usize> = self.slides_of.values().map(|sl| sl[0]).collect();
        for (si, sl) in b.slides.iter().enumerate() {
            for e in &sl.entries {
                let o = e.object as usize;
                for p in &e.props {
                    let v = self.value(p.key, p.raw);
                    if masters.contains(&si) {
                        if let Some(d) = base.get_mut(o) {
                            d.insert(p.key & MASK27, v);
                        }
                    } else {
                        slide_props[si].push((o, p.key & MASK27, v));
                    }
                }
            }
        }
        self.base = base;
        self.slide_props = slide_props;
        if let Some(f) = &self.scene.fbf {
            self.tex_paths = f
                .records
                .iter()
                .enumerate()
                .filter_map(|(ri, r)| match &r.payload {
                    Payload::Image(im) => Some((ri, texture_path(&im.path))),
                    _ => None,
                })
                .collect();
        }
        if let Some(bsg) = &self.scene.bsg {
            self.node_obj = bsg.nodes.iter().map(|bn| self.by_id.get(&bn.id).copied()).collect();
            self.node_rec = bsg.nodes.iter().map(|bn| self.record_of.get(&bn.id).copied()).collect();
            let none = Overlay::default();
            let cached: Vec<_> = bsg.nodes.iter().zip(&self.node_obj).map(|(bn, &oi)| Self::node_local(bn, &self.props(&none, oi))).collect();
            self.static_local = cached;
        }
    }

    /// Default state: each component on its first non-master slide, at its end ("fully shown"),
    /// not playing. Matches the reference renders.
    pub fn reset(&mut self) {
        self.revision += 1;
        self.clocks.clear();
        self.overrides.clear();
        for (&c, sl) in &self.slides_of {
            let idx = if sl.len() > 1 { sl[1] } else { sl[0] };
            let end = self.scene.bgf.slides[idx].end as f32;
            self.clocks.insert(c, Clock { slide: idx, time: end, playing: false });
        }
    }

    /// Changes whenever the next `evaluate()` could differ from the last one (UI fast path: a scene whose revision
    /// didn't move needs no re-evaluation or redraw).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn clock(&self, component: usize) -> Option<Clock> {
        self.clocks.get(&component).copied()
    }

    pub fn components(&self) -> impl Iterator<Item = usize> + '_ {
        self.slides_of.keys().copied()
    }

    fn slide(&self, i: usize) -> &Slide {
        &self.scene.bgf.slides[i]
    }

    /// Name of a bgf object (fbf record name), if any.
    pub fn name(&self, object: usize) -> Option<&str> {
        let o = self.scene.bgf.objects.get(object)?;
        if o.kind == ObjectKind::Behavior {
            return None;
        }
        let ri = *self.record_of.get(&o.id)?;
        self.scene.fbf.as_ref().map(|f| f.records[ri].name.as_str())
    }

    /// Child objects (bgf table order).
    pub fn children(&self, object: usize) -> &[usize] {
        self.children.get(object).map_or(&[], Vec::as_slice)
    }

    /// First object with this name (depth-first in table order).
    pub fn find(&self, name: &str) -> Option<usize> {
        (0..self.scene.bgf.objects.len()).find(|&i| self.name(i) == Some(name))
    }

    /// Follow a contract path (`parent.Layer.Title`, `this`) from `from`.
    pub fn resolve_path(&self, from: usize, path: &str) -> Option<usize> {
        let mut cur = from;
        for seg in path.split('.') {
            cur = match seg {
                "this" | "" => cur,
                "parent" => {
                    let p = self.scene.bgf.objects.get(cur)?.parent;
                    (p >= 0).then_some(p as usize)?
                }
                // Contract paths can use display names with spaces for underscores ("Crisp Tape").
                name => *self.children.get(cur)?.iter().find(|&&c| {
                    self.name(c).is_some_and(|n| n == name || n.replace('_', " ") == name.replace('_', " "))
                })?,
            };
        }
        Some(cur)
    }

    /// Every contract behaviour in the scene.
    pub fn contracts(&self) -> Vec<Contract> {
        let b = &self.scene.bgf;
        let s = |v: u32| b.strings.get(v as usize).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for (i, o) in b.objects.iter().enumerate() {
            if o.kind != ObjectKind::Behavior {
                continue;
            }
            let is_contract = o.props.iter().any(|p| p.key & MASK27 == n::CONTRACT_KIND && p.base_type() == 5 && s(p.raw) == "contract");
            if !is_contract {
                continue;
            }
            let kind = o.props.iter().find(|p| p.key & MASK27 == n::CONTRACT_TYPE).map(|p| s(p.raw)).unwrap_or_default();
            let fields = o
                .props
                .iter()
                .filter(|p| p.base_type() == 5 && p.key & MASK27 != n::CONTRACT_KIND && p.key & MASK27 != n::CONTRACT_TYPE)
                .map(|p| (p.key & MASK27, s(p.raw)))
                .collect();
            out.push(Contract { behavior: i, owner: o.parent.max(0) as usize, kind, fields });
        }
        out
    }

    // ------------------------------------------------------------------ control

    /// Switch a component to a slide by name. Starts playing if the slide's flags say so.
    pub fn goto_slide(&mut self, component: usize, name: &str) -> bool {
        let h = ahash31(name.as_bytes());
        let Some(idx) = self.slides_of.get(&component).and_then(|sl| sl.iter().copied().find(|&s| ahash31(self.slide(s).name.as_bytes()) == h)) else {
            return false;
        };
        self.enter(component, idx);
        true
    }

    fn enter(&mut self, component: usize, slide: usize) {
        let s = self.slide(slide);
        let clock = Clock { slide, time: s.start as f32, playing: s.flags & 1 != 0 };
        self.clocks.insert(component, clock);
        self.revision += 1;
    }

    /// Seek a component (ms in its slide's time base).
    pub fn set_time(&mut self, component: usize, ms: f32) {
        if let Some(c) = self.clocks.get_mut(&component) {
            if c.time != ms {
                c.time = ms;
                self.revision += 1;
            }
        }
    }

    /// Seek to a fraction 0..1 of the current slide (gauges, progress bars).
    pub fn set_progress(&mut self, component: usize, f: f32) {
        if let Some(c) = self.clocks.get(&component).copied() {
            let s = self.slide(c.slide);
            let t = s.start as f32 + (s.end as f32 - s.start as f32) * f.clamp(0.0, 1.0);
            self.set_time(component, t);
        }
    }

    /// Set a property on an object (overrides the scene's value until [`Player::reset`]).
    pub fn set(&mut self, object: usize, key27: u32, v: Value) {
        let slot = self.overrides.entry(object).or_default();
        let k = key27 & MASK27;
        if slot.get(&k) != Some(&v) {
            slot.insert(k, v);
            self.revision += 1;
        }
    }

    pub fn set_text(&mut self, object: usize, s: impl Into<String>) {
        self.set(object, n::TEXTSTRING, Value::Str(s.into()));
    }

    /// Start or stop a component's clock (e.g. hold a gauge where `set_progress` put it).
    pub fn set_playing(&mut self, component: usize, playing: bool) {
        if let Some(c) = self.clocks.get_mut(&component) {
            if c.playing != playing {
                c.playing = playing;
                self.revision += 1;
            }
        }
    }

    /// Whether `object` is `ancestor` or below it.
    pub fn is_within(&self, object: usize, ancestor: usize) -> bool {
        let mut cur = object as i32;
        while cur >= 0 {
            if cur as usize == ancestor {
                return true;
            }
            cur = self.scene.bgf.objects.get(cur as usize).map_or(-1, |o| o.parent);
        }
        false
    }

    /// Raise a game event on every handler in the scene. Returns the number of actions run.
    /// Events are element-scoped in FH1 (e.g. 947_HUD has separate `SHOWN` handlers per widget),
    /// so prefer [`Player::fire_at`].
    pub fn fire(&mut self, event: &str) -> usize {
        self.fire_filtered(event, None)
    }

    /// Raise a game event on one element: only handlers whose behaviour sits in its subtree.
    pub fn fire_at(&mut self, event: &str, element: usize) -> usize {
        self.fire_filtered(event, Some(element))
    }

    fn fire_filtered(&mut self, event: &str, element: Option<usize>) -> usize {
        let h = ahash31(event.as_bytes());
        let actions: Vec<_> = self
            .scene
            .bgf
            .handlers
            .iter()
            .filter(|hd| element.is_none_or(|e| self.is_within(hd.object.max(0) as usize, e)))
            .flat_map(|hd| hd.events.iter())
            .filter(|e| e.hash31 & 0x7FFF_FFFF == h)
            .flat_map(|e| e.actions.iter().copied())
            .collect();
        for a in &actions {
            let target = a.target.max(0) as usize;
            match a.command {
                Command::SetProperty => {
                    let key = a.arg1 & !(16 << 27);
                    let v = match typed(key, a.arg2) {
                        PropValue::Int(i) => Value::Int(i),
                        PropValue::Float(f) => Value::Float(f),
                        PropValue::Bool(b) => Value::Bool(b),
                        PropValue::Str(s) => Value::Str(self.scene.string(s).unwrap_or_default().to_owned()),
                        PropValue::Other(o) => Value::Int(o),
                    };
                    self.set(target, key, v);
                }
                Command::GotoSlide => {
                    let want = a.arg1 & 0x7FFF_FFFF;
                    let idx = self.slides_of.get(&target).and_then(|sl| sl.iter().copied().find(|&s| ahash31(self.slide(s).name.as_bytes()) == want));
                    if let Some(idx) = idx {
                        self.enter(target, idx);
                    }
                }
                Command::GotoTime => self.set_time(target, a.arg1 as f32),
                Command::Unknown(_) => {}
            }
        }
        actions.len()
    }

    /// Advance every playing component by `dt_ms`.
    pub fn update(&mut self, dt_ms: f32) {
        let comps: Vec<usize> = self.clocks.keys().copied().collect();
        for c in comps {
            let Some(mut clock) = self.clocks.get(&c).copied() else { continue };
            if !clock.playing {
                continue;
            }
            let s = self.slide(clock.slide);
            let (start, end, flags) = (s.start as f32, s.end as f32, s.flags);
            if dt_ms > 0.0 {
                self.revision += 1;
            }
            clock.time += dt_ms;
            if clock.time >= end {
                match end_mode(flags) {
                    EndMode::Loop if end > start => clock.time = start + (clock.time - start) % (end - start),
                    EndMode::Next => {
                        let next = self.slides_of.get(&c).and_then(|sl| {
                            let k = sl.iter().position(|&x| x == clock.slide)?;
                            sl.get(k + 1).copied()
                        });
                        if let Some(n) = next {
                            self.enter(c, n);
                            continue;
                        }
                        clock.time = end;
                        clock.playing = false;
                    }
                    _ => {
                        clock.time = end;
                        clock.playing = false;
                    }
                }
            }
            self.clocks.insert(c, clock);
        }
    }

    // ------------------------------------------------------------------ evaluation

    fn value(&self, key: u32, raw: u32) -> Value {
        match typed(key, raw) {
            PropValue::Int(i) | PropValue::Other(i) => Value::Int(i),
            PropValue::Float(f) => Value::Float(f),
            PropValue::Bool(b) => Value::Bool(b),
            PropValue::Str(s) => Value::Str(self.scene.string(s).unwrap_or_default().to_owned()),
        }
    }

    /// The dynamic properties of the current state, over `base`: Master Slide tracks, then the
    /// current slide's props and tracks, then event / game overrides (later entries win).
    fn resolve(&self) -> Overlay<'_> {
        let b = &self.scene.bgf;
        let mut over = Overlay::default();
        let key = |o: usize, k: u32| (o as u64) << 32 | k as u64;
        for (c, sl) in &self.slides_of {
            let Some(clock) = self.clocks.get(c) else { continue };
            let mut order = [Some(sl[0]), None];
            if clock.slide != sl[0] {
                order[1] = Some(clock.slide);
            }
            for si in order.into_iter().flatten() {
                let s = &b.slides[si];
                let t = clock.time.min(s.end as f32);
                for (o, k, v) in &self.slide_props[si] {
                    over.insert(key(*o, *k), Pv::V(v));
                }
                for e in &s.entries {
                    for r in &e.refs {
                        let Some(tr) = r.track().and_then(|i| b.tracks.get(i)) else { continue };
                        if let Some(v) = tr.eval(t) {
                            over.insert(key(tr.object as usize, tr.key & MASK27), Pv::F(v));
                        }
                    }
                }
            }
        }
        for (&o, ov) in &self.overrides {
            for (k, v) in ov {
                over.insert(key(o, *k), Pv::V(v));
            }
        }
        over
    }

    /// Profiling hook (examples/evalbench): size of the per-frame overlay.
    #[doc(hidden)]
    pub fn bench_resolve(&self) -> usize {
        self.resolve().len()
    }

    /// A node's local matrix `T(pos)·R(rot)·S(scale)·T(−pivot)`, its position and own opacity 0..1.
    fn node_local(bn: &crate::anark::bsg::Node, v: &Props) -> (Mat4, [f32; 3], f32) {
        let g = |k: u32, d: f32| v.f(k, d);
        let mut pos = [g(n::POSITION_X, bn.position[0]), g(n::POSITION_Y, bn.position[1]), g(n::POSITION_Z, bn.position[2])];
        let rot = [g(n::ROTATION_X, bn.rotation[0]), g(n::ROTATION_Y, bn.rotation[1]), g(n::ROTATION_Z, bn.rotation[2])];
        let scl = [g(n::SCALE_X, bn.scale[0]), g(n::SCALE_Y, bn.scale[1]), g(n::SCALE_Z, bn.scale[2])];
        let piv = [g(n::PIVOT_X, 0.0), g(n::PIVOT_Y, 0.0), g(n::PIVOT_Z, 0.0)];
        if !v.has(n::POSITION_X) && v.has(n::PIVOT_X) {
            pos = [0.0; 3];
        }
        let local = mul(&translate(pos), &mul(&rotate(rot), &mul(&scale(scl), &translate([-piv[0], -piv[1], -piv[2]]))));
        (local, pos, g(n::OPACITY, bn.opacity * 100.0).clamp(0.0, 100.0) / 100.0)
    }

    fn props<'a>(&'a self, over: &'a Overlay<'a>, object: Option<usize>) -> Props<'a> {
        Props { base: object.and_then(|o| self.base.get(o)), over, object: object.map_or(u64::from(u32::MAX), |o| o as u64) }
    }

    /// Whether an object exists now: on its component's current slide and inside its time bar.
    fn alive(&self, object: usize, v: &Props) -> bool {
        let Some(c) = self.comp_of.get(object).copied().flatten() else { return true };
        let Some(clock) = self.clocks.get(&c) else { return true };
        let t = clock.time;
        let t0 = v.f(n::TIMEOFFSET, 0.0);
        let t1 = v.get(n::ENDTIME).and_then(Pv::f);
        t >= t0 && t1.is_none_or(|e| t <= e) && self.on_slide.get(&(clock.slide, object)).copied().unwrap_or(true)
    }

    /// The draw list for the current state, back to front.
    pub fn evaluate(&self) -> Frame {
        let (Some(fbf), Some(bsg)) = (&self.scene.fbf, &self.scene.bsg) else { return Frame::default() };
        let over = self.resolve();
        let mut touched = vec![false; self.scene.bgf.objects.len()];
        for k in over.keys() {
            if let Some(t) = touched.get_mut((k >> 32) as usize) {
                *t = true;
            }
        }
        let mut world = Vec::with_capacity(bsg.nodes.len());
        let mut opacity = Vec::with_capacity(bsg.nodes.len());
        let mut frame = Frame::default();
        let mut items = Vec::new();
        for (ni, bn) in bsg.nodes.iter().enumerate() {
            let oi = self.node_obj[ni];
            let rec = self.node_rec[ni].map(|ri| &fbf.records[ri]);
            let parent = usize::try_from(bn.parent).ok().filter(|&p| p < ni);
            // Below a fully transparent node nothing draws: skip the property and matrix work
            // (cameras are still evaluated). Such nodes get no `frame.objects` entry.
            if let Some(p) = parent {
                if opacity[p] == 0.0 && !matches!(rec.map(|r| &r.payload), Some(Payload::Camera { .. })) {
                    world.push(world[p]);
                    opacity.push(0.0);
                    continue;
                }
            }
            let v = self.props(&over, oi);
            let g = |k: u32, d: f32| v.f(k, d);
            let (local, pos, own) = if oi.is_some_and(|o| touched[o]) { Self::node_local(bn, &v) } else { self.static_local[ni] };
            let alive = oi.is_none_or(|o| self.alive(o, &v));
            let op = if alive { own } else { 0.0 };
            let (w, o) = match parent {
                Some(p) => (mul(&world[p], &local), opacity[p] * op),
                None => (local, op),
            };
            world.push(w);
            opacity.push(o);
            if let Some(oi) = oi {
                frame.objects.insert(oi, (w, bn.layer));
            }
            let Some(rec) = rec else { continue };
            match &rec.payload {
                Payload::Camera { .. } => {
                    let cam = LayerCamera {
                        position: pos,
                        orthographic: v.get(n::ORTHOGRAPHIC).and_then(Pv::i).is_none_or(|x| x != 0),
                        fov: g(n::FOV, 60.0),
                    };
                    frame.cameras.entry(bn.layer).or_insert(cam);
                }
                Payload::Model { mesh, .. } if o > 0.001 => {
                    let materials = self.materials(bn.id, &over);
                    if materials.iter().all(|m| m.textures.is_empty()) && rec.name.to_uppercase().contains("VIEWPORT") {
                        continue; // render-target placeholder (3D minimap etc.)
                    }
                    items.push(Draw { node: ni, name: rec.name.clone(), layer: bn.layer, world: w, opacity: o, kind: DrawKind::Model { mesh: *mesh as usize, materials } });
                }
                Payload::Text(t) if o > 0.001 => {
                    let text = DrawText {
                        string: v.s(n::TEXTSTRING).unwrap_or_default().to_owned(),
                        loc_key: t.loc_key.clone(),
                        font: v.s(n::FONT).map_or_else(|| t.font.clone(), str::to_owned),
                        size: g(n::SIZE, t.size as f32),
                        horzalign: v.get(n::HORZALIGN).and_then(Pv::i).unwrap_or(t.horzalign),
                        vertalign: v.get(n::VERTALIGN).and_then(Pv::i).unwrap_or(t.vertalign),
                        leading: g(n::LEADING, t.leading as f32),
                        tracking: g(n::TRACKING, t.tracking),
                        wordwrap: v.get(n::WORDWRAP).and_then(Pv::i).map_or(t.wordwrap != 0, |x| x != 0),
                        color: [g(n::TEXTCOLOR_R, 255.0) / 255.0, g(n::TEXTCOLOR_G, 255.0) / 255.0, g(n::TEXTCOLOR_B, 255.0) / 255.0, g(n::TEXTCOLOR_A, 255.0) / 255.0],
                        text_width: t.text_width,
                    };
                    items.push(Draw { node: ni, name: rec.name.clone(), layer: bn.layer, world: w, opacity: o, kind: DrawKind::Text(text) });
                }
                _ => {}
            }
        }
        // Layers back to front, then far to near (camera at −z looking +z), then file order.
        // The FIRST layer is the front one (as in Anark Studio's layer list): 947_HUD's first layer
        // holds the full-screen fade curtain, and its NorthArrow (layer 1) sits inside the
        // minimap disc (layer 2). Higher layer indices are therefore drawn first.
        let mut keyed: Vec<(u8, f32, usize, Draw)> = items
            .into_iter()
            .enumerate()
            .map(|(i, d)| {
                let cam = frame.camera(d.layer);
                let depth = apply(&d.world, [0.0; 3])[2] - cam.position[2];
                (d.layer, -depth, i, d)
            })
            .collect();
        keyed.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)));
        frame.draws = keyed.into_iter().map(|k| k.3).collect();
        frame
    }

    fn materials(&self, model_id: u32, over: &Overlay) -> Vec<DrawMaterial> {
        let Some(fbf) = &self.scene.fbf else { return Vec::new() };
        let vals_of = |id: u32| self.props(over, self.by_id.get(&id).copied());
        let mut out = Vec::new();
        for &ri in self.materials_of.get(&model_id).into_iter().flatten() {
            let r = &fbf.records[ri];
            let Payload::Material(m) = &r.payload else { continue };
            let mv = vals_of(r.id);
            let diffuse = [
                mv.f(n::DIFFUSE_R, m.diffuse[0] * 255.0) / 255.0,
                mv.f(n::DIFFUSE_G, m.diffuse[1] * 255.0) / 255.0,
                mv.f(n::DIFFUSE_B, m.diffuse[2] * 255.0) / 255.0,
            ];
            let mut textures = Vec::new();
            for &ii in self.images_of.get(&r.id).into_iter().flatten() {
                let ir = &fbf.records[ii];
                let Payload::Image(im) = &ir.payload else { continue };
                let iv = vals_of(ir.id);
                let animated = [n::POSITIONU, n::POSITIONV, n::ROTATIONUV, n::SCALEU, n::SCALEV].iter().any(|&k| iv.has(k));
                let rotating = self.by_id.get(&ir.id).is_some_and(|o| self.uv_rotating.contains(o));
                let m16 = if animated {
                    fbf::uv_matrix(
                        iv.f(n::ROTATIONUV, im.rotationuv),
                        [iv.f(n::POSITIONU, im.positionu), iv.f(n::POSITIONV, im.positionv)],
                        [iv.f(n::SCALEU, im.scaleu), iv.f(n::SCALEV, im.scalev)],
                        [im.pivotu, im.pivotv],
                    )
                } else {
                    im.uv_matrix
                };
                textures.push(DrawTexture {
                    path: self.tex_paths.get(&ii).cloned().unwrap_or_else(|| texture_path(&im.path)),
                    uv: [m16[0], m16[4], m16[12], m16[1], m16[5], m16[13]],
                    tiling: [im.tilingmodehorz, im.tilingmodevert],
                    rotating,
                });
            }
            out.push(DrawMaterial {
                submesh: m.submesh as usize,
                diffuse,
                opacity: mv.f(n::OPACITY, 100.0) / 100.0,
                additive: m.is_additive(),
                textures,
            });
        }
        out
    }
}

/// `GAME:\MEDIA\UI\TEXTURES\HORIZON\HUD\DIALS\NEEDLE.TGA` → `horizon/hud/dials/needle.png`, the
/// layout the setup tool writes (Horizon.zip under `horizon/`, Textures.zip as-is).
pub fn texture_path(game_path: &str) -> String {
    let p = game_path.replace('\\', "/").to_ascii_lowercase();
    let p = p.split_once("/textures/").map_or(p.as_str(), |(_, r)| r).to_owned();
    match p.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.png"),
        None => format!("{p}.png"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(texture_path(r"GAME:\MEDIA\UI\TEXTURES\HORIZON\DIRTMASKS\BGNOISE01.TGA"), "horizon/dirtmasks/bgnoise01.png");
        assert_eq!(texture_path(r"GAME:\MEDIA\UI\TEXTURES\UI4\A_BUTTON_I.TGA"), "ui4/a_button_i.png");
    }

    #[test]
    fn modes() {
        assert_eq!(end_mode(9), EndMode::Stop);
        assert_eq!(end_mode(13), EndMode::Loop);
        assert_eq!(end_mode(17), EndMode::Next);
        assert_eq!(end_mode(0), EndMode::None);
    }
}
