//! In-world markers for the free-roam missions on Colorado (docs/MISSIONS.md): Horizon Outposts, speed cameras,
//! average-speed gates and the photo-shoot spots, drawn in FH1's marker look so the player can see them while driving.
//! `FH1_MISSION_MARKERS=0` = none. Needs the race marker shader (`FH1_RACE_FX` on, race/visuals.rs embeds it).
//!
//! INFERRED style (no FH1 captures of these; the shapes and colours are ours, the look is race/visuals.rs):
//! - Outpost: a ground ring over the TriggerZone (`radius`) and a tall soft light column at the forecourt, Horizon
//!   orange. Available = full column + ring; Done (all three missions completed) = desaturated, dim, short column;
//!   Locked (not discovered) = no column, faint ring (as the map's padlock icon).
//! - Speed camera: a glowing stripe across the road on the ground between the two posts, a low see-through curtain
//!   over it and a lit post at each end. Yellow, Done (has a best speed) = green. Average-speed zone: the same at
//!   both gates, blue, Done (has a best) = green.
//! - Photo shoot: shown only while that mission runs ([`MarkerFocus`], set by outpost.rs): a cyan column with a
//!   floating viewfinder frame (the "camera icon") and a radius ring at each zone.
//! - Visible within a draw distance (outposts 1200 m, cameras / gates 400 m, photo zones 800 m, +60 m hysteresis);
//!   the shader fades them in / out by distance. Hidden during races (`RaceState::race`) and cutscenes.
//!
//! Perf (docs/PERF.md, PERF_P15_B marker merge): the same trick as race/visuals.rs `merged_markers`. Every visible
//! marker is baked into ONE world-space mesh on ONE entity with ONE additive material (one transparent draw call in
//! total, whatever is in range). Per-piece colour / intensity / fades ride in vertex attributes; pulse and rising
//! bands are animated in marker.wgsl from `globals.time`, so nothing is rewritten per frame. The mesh is rebuilt only
//! when the visible set, the profile (`Profile.generation` / `MissionMapIcons.generation`) or the focus changes,
//! padded to power-of-two capacities so the allocator reuses the range. Ground heights (a ray per ring vertex /
//! gate post) are taken once per marker, the first time it comes into range. The entity is a `WorldEntity`, so a
//! map switch despawns it; a new `WorldGeneration` rebuilds the item list. The material is this file's own copy of
//! race/visuals.rs `MarkerMaterial` (that one's fields are private) on the same shader.

use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::{Indices, MeshVertexAttribute, MeshVertexBufferLayoutRef, PrimitiveTopology, VertexFormat};
use bevy::pbr::{MaterialPipeline, MaterialPipelineKey};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;

use super::data::MissionData;
use super::map::{IconState, MissionMapIcons};
use super::save::MissionsSave;
use super::Missions;
use crate::Car;

/// `FH1_MISSION_MARKERS=0`: no in-world markers.
pub fn markers_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_MISSION_MARKERS"))
}

/// The photo-shoot mission that is running (`photoshoot_NN`, a `PhotoShoot::name`), set by outpost.rs; its zones get
/// markers while it is `Some`.
#[derive(Resource, Default, Clone, Debug, PartialEq)]
pub struct MarkerFocus(pub Option<String>);

pub fn register(app: &mut App) {
    // Always there, so outpost.rs can write it whatever the flag says.
    app.init_resource::<MarkerFocus>();
    if !markers_on() || !crate::race::visuals::enabled() {
        return;
    }
    app.add_plugins(MaterialPlugin::<MissionMarkerMaterial>::default())
        .init_resource::<Markers>()
        .add_systems(Update, update_markers.run_if(super::on_colorado));
}

// ---------------------------------------------------------------- material (copy of race/visuals.rs MarkerMaterial)

/// Merged mesh: per-piece colour (rgb display-linear HDR, a = intensity). Same ids as race/visuals.rs (the shader's
/// MARKER_MERGED inputs).
const ATTRIBUTE_MARKER_COLOUR: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_MarkerColour", 0x4648_0060, VertexFormat::Float32x4);
/// Near fade start / end, far fade start / end (m).
const ATTRIBUTE_MARKER_FADE: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_MarkerFade", 0x4648_0061, VertexFormat::Float32x4);
/// x pulse Hz, y local height (beam bands), z emphasis, w pass-ripple birth (-1 = static piece).
const ATTRIBUTE_MARKER_K: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_MarkerK", 0x4648_0062, VertexFormat::Float32x4);

const PULSE_HZ: f32 = 1.2;

#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType)]
pub struct MissionParams {
    colour: Vec4,
    fade: Vec4,
    k: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct MissionMarkerMaterial {
    #[uniform(0)]
    params: MissionParams,
}

impl Material for MissionMarkerMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/race/marker.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/race/marker.wgsl".into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Add
    }
    fn enable_prepass() -> bool {
        false
    }
    fn enable_shadows() -> bool {
        false
    }
    fn specialize(_: &MaterialPipeline, d: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, _: MaterialPipelineKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        // Always the merged layout (per-piece parameters per vertex).
        d.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
            ATTRIBUTE_MARKER_COLOUR.at_shader_location(4),
            ATTRIBUTE_MARKER_FADE.at_shader_location(5),
            ATTRIBUTE_MARKER_K.at_shader_location(6),
        ])?];
        d.vertex.shader_defs.push("MARKER_MERGED".into());
        if let Some(f) = d.fragment.as_mut() {
            f.shader_defs.push("MARKER_MERGED".into());
        }
        d.primitive.cull_mode = None;
        if let Some(f) = d.fragment.as_mut() {
            for t in f.targets.iter_mut().flatten() {
                t.blend = Some(BlendState {
                    color: BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                    alpha: BlendComponent { src_factor: BlendFactor::Zero, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                });
            }
        }
        if let Some(ds) = d.depth_stencil.as_mut() {
            ds.depth_write_enabled = Some(false).into();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- style (pure)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Outpost,
    Camera,
    Average,
    Photo,
}

impl Kind {
    /// Draw distance (m).
    pub fn show_m(self) -> f32 {
        match self {
            Kind::Outpost => 1200.0,
            Kind::Camera | Kind::Average => 400.0,
            Kind::Photo => 800.0,
        }
    }
}

/// Extra metres a shown marker stays shown (no flicker at the edge).
const HYSTERESIS_M: f32 = 60.0;

/// How a marker looks: colour (display-linear HDR), intensity, column height (None = no column), ring / stripe
/// intensity, pulse emphasis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub colour: LinearRgba,
    pub intensity: f32,
    pub column: Option<f32>,
    pub ring: f32,
    pub emphasis: f32,
}

fn hdr(r: u8, g: u8, b: u8, k: f32) -> LinearRgba {
    Color::srgb_u8(r, g, b).to_linear() * k
}

/// State -> look (see the module docs).
pub fn style(kind: Kind, state: IconState) -> Style {
    let done_green = hdr(90, 230, 170, 2.0);
    match (kind, state) {
        (Kind::Outpost, IconState::Available) => Style { colour: hdr(255, 140, 10, 2.0), intensity: 1.0, column: Some(110.0), ring: 0.9, emphasis: 0.4 },
        (Kind::Outpost, IconState::Done) => Style { colour: hdr(205, 175, 145, 1.6), intensity: 0.45, column: Some(45.0), ring: 0.4, emphasis: 0.0 },
        (Kind::Outpost, IconState::Locked) => Style { colour: hdr(255, 140, 10, 1.6), intensity: 0.35, column: None, ring: 0.35, emphasis: 0.0 },
        (Kind::Camera, IconState::Done) | (Kind::Average, IconState::Done) => Style { colour: done_green, intensity: 0.6, column: None, ring: 0.6, emphasis: 0.0 },
        (Kind::Camera, s) => Style { colour: hdr(255, 215, 60, 2.0), intensity: if s == IconState::Locked { 0.4 } else { 1.0 }, column: None, ring: 1.0, emphasis: 0.0 },
        (Kind::Average, s) => Style { colour: hdr(110, 190, 255, 2.0), intensity: if s == IconState::Locked { 0.4 } else { 1.0 }, column: None, ring: 1.0, emphasis: 0.0 },
        (Kind::Photo, IconState::Done) => Style { colour: hdr(120, 230, 255, 1.6), intensity: 0.5, column: Some(40.0), ring: 0.5, emphasis: 0.0 },
        (Kind::Photo, _) => Style { colour: hdr(120, 230, 255, 2.0), intensity: 1.0, column: Some(70.0), ring: 0.9, emphasis: 0.4 },
    }
}

/// Outpost state: not discovered = Locked, all its missions completed = Done (same rule as map.rs).
pub fn outpost_state(found: bool, completed: usize, total: usize) -> IconState {
    if !found {
        IconState::Locked
    } else if total > 0 && completed == total {
        IconState::Done
    } else {
        IconState::Available
    }
}

/// Camera / average zone state: a best speed on record = Done.
pub fn best_state(has_best: bool) -> IconState {
    if has_best {
        IconState::Done
    } else {
        IconState::Available
    }
}

// ---------------------------------------------------------------- items

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    /// A ring of this radius (m) around the item's position.
    Disc(f32),
    /// A gate between two posts (data positions).
    Gate(Vec3, Vec3),
}

/// Ground-snapped geometry, found once when the item first comes into range.
#[derive(Clone, Debug, PartialEq)]
enum Prep {
    /// Ground point at the centre, and the ground height at each ring vertex angle (`ring_segments + 1` values).
    Disc { ground: Vec3, ys: Vec<f32> },
    Gate { l: Vec3, r: Vec3 },
}

#[derive(Clone, Debug)]
struct Item {
    kind: Kind,
    name: String,
    /// Photo zones: the photo shoot they belong to.
    shoot: String,
    state: IconState,
    /// Data position (centre of a ring, middle of a gate).
    pos: Vec3,
    shape: Shape,
    prep: Option<Prep>,
}

impl Item {
    fn extent(&self) -> f32 {
        match self.shape {
            Shape::Disc(r) => r,
            Shape::Gate(l, r) => l.distance(r) * 0.5,
        }
    }
    /// Metres from `p` to the marker's edge (x, z).
    fn dist(&self, p: Vec3) -> f32 {
        (Vec2::new(self.pos.x - p.x, self.pos.z - p.z).length() - self.extent()).max(0.0)
    }
    /// Photo zones only count while their mission runs.
    fn active(&self, focus: &Option<String>) -> bool {
        self.kind != Kind::Photo || focus.as_deref() == Some(self.shoot.as_str())
    }
}

fn build_items(d: &MissionData) -> Vec<Item> {
    let mut out = Vec::new();
    for o in &d.outposts {
        out.push(Item {
            kind: Kind::Outpost,
            name: o.name.clone(),
            shoot: String::new(),
            state: IconState::Available,
            pos: Vec3::from_array(o.pos),
            shape: Shape::Disc(o.radius.max(15.0)),
            prep: None,
        });
    }
    for c in &d.speed_cameras {
        let (l, r) = (Vec3::from_array(c.left), Vec3::from_array(c.right));
        out.push(Item { kind: Kind::Camera, name: c.name.clone(), shoot: String::new(), state: IconState::Available, pos: (l + r) * 0.5, shape: Shape::Gate(l, r), prep: None });
    }
    for z in &d.average_speed {
        for g in [z.start, z.end] {
            let (l, r) = (Vec3::from_array(g[0]), Vec3::from_array(g[1]));
            out.push(Item { kind: Kind::Average, name: z.name.clone(), shoot: String::new(), state: IconState::Available, pos: (l + r) * 0.5, shape: Shape::Gate(l, r), prep: None });
        }
    }
    for s in &d.photo_shoots {
        for z in &s.zones {
            out.push(Item {
                kind: Kind::Photo,
                name: s.name.clone(),
                shoot: s.name.clone(),
                state: IconState::Available,
                pos: Vec3::from_array(z.pos),
                shape: Shape::Disc(z.radius.max(10.0)),
                prep: None,
            });
        }
    }
    out
}

/// Recompute every item's state from the profile.
fn restyle(items: &mut [Item], d: &MissionData, s: &MissionsSave) {
    for it in items {
        it.state = match it.kind {
            Kind::Outpost => match d.outposts.iter().find(|o| o.name == it.name) {
                Some(o) => {
                    let done = o.missions.iter().filter(|m| s.missions.get(*m).is_some_and(|r| r.completed)).count();
                    outpost_state(s.outposts.contains(&o.name), done, o.missions.len())
                }
                None => IconState::Available,
            },
            Kind::Camera => best_state(s.speed_cameras.contains_key(&it.name)),
            Kind::Average => best_state(s.average_speed.contains_key(&it.name)),
            Kind::Photo => IconState::Available,
        };
    }
}

/// Ground height under `p`: a ray from just above `ref_y` (stays under station canopies), else from 50 m up (a hit
/// within 30 m of `ref_y` only, so a roof is not taken for the ground), else `ref_y`.
fn ground_y(track: &crate::track::Track, p: Vec3, ref_y: f32) -> f32 {
    if let Some(h) = track.ground.ray(Vec3::new(p.x, ref_y + 3.0, p.z), Vec3::NEG_Y, 10.0) {
        return h.point.y;
    }
    if let Some(h) = track.ground.ray(Vec3::new(p.x, ref_y + 50.0, p.z), Vec3::NEG_Y, 100.0) {
        if (h.point.y - ref_y).abs() < 30.0 {
            return h.point.y;
        }
    }
    ref_y
}

/// Ring vertices around a circle of radius `r`: about one per 1.5 m of arc.
fn ring_segments(r: f32) -> u32 {
    ((r / 1.5) as u32).clamp(24, 96)
}

/// Ring band width (m) for a zone radius.
fn ring_width(r: f32) -> f32 {
    (r * 0.06).clamp(2.0, 4.5)
}

fn prep_item(it: &mut Item, y_at: &dyn Fn(Vec3, f32) -> f32) {
    if it.prep.is_some() {
        return;
    }
    it.prep = Some(match it.shape {
        Shape::Disc(r) => {
            let y = y_at(it.pos, it.pos.y);
            let c = Vec3::new(it.pos.x, y, it.pos.z);
            let n = ring_segments(r);
            let ys = (0..=n)
                .map(|k| {
                    let a = k as f32 / n as f32 * std::f32::consts::TAU;
                    y_at(Vec3::new(c.x + a.cos() * r, y, c.z + a.sin() * r), y)
                })
                .collect();
            Prep::Disc { ground: c, ys }
        }
        Shape::Gate(l, r) => Prep::Gate { l: Vec3::new(l.x, y_at(l, l.y), l.z), r: Vec3::new(r.x, y_at(r, r.y), r.z) },
    });
}

// ---------------------------------------------------------------- mesh bake

const BEAM: u8 = 0;
const RING: u8 = 1;
const CURTAIN: u8 = 3;
const BAR: u8 = 6;

/// Per-piece shader parameters.
#[derive(Clone, Copy, Debug)]
struct Attr {
    colour: [f32; 4],
    fade: [f32; 4],
    emph: f32,
}

impl Attr {
    fn new(c: LinearRgba, intensity: f32, fade: [f32; 4], emph: f32) -> Self {
        Self { colour: [c.red, c.green, c.blue, intensity], fade, emph }
    }
}

/// CPU mesh being filled, positions relative to `origin`.
#[derive(Default)]
struct Bake {
    origin: Vec3,
    pos: Vec<[f32; 3]>,
    nrm: Vec<[f32; 3]>,
    uv: Vec<[f32; 2]>,
    shape: Vec<[f32; 2]>,
    colour: Vec<[f32; 4]>,
    fade: Vec<[f32; 4]>,
    k: Vec<[f32; 4]>,
    idx: Vec<u32>,
}

impl Bake {
    fn new(origin: Vec3) -> Self {
        Self { origin, ..Default::default() }
    }

    /// A grid of quads: `f(i, j, u, w)` = (world position, normal, local height) for i in 0..=nu, j in 0..=nv;
    /// uv = (u, w) = (i / nu, j / nv).
    fn grid(&mut self, nu: u32, nv: u32, style: u8, a: &Attr, f: impl Fn(u32, u32, f32, f32) -> (Vec3, Vec3, f32)) {
        let base = self.pos.len() as u32;
        for j in 0..=nv {
            for i in 0..=nu {
                let (u, w) = (i as f32 / nu as f32, j as f32 / nv as f32);
                let (p, n, ly) = f(i, j, u, w);
                self.pos.push((p - self.origin).into());
                self.nrm.push(n.into());
                self.uv.push([u, w]);
                self.shape.push([style as f32, 0.0]);
                self.colour.push(a.colour);
                self.fade.push(a.fade);
                self.k.push([PULSE_HZ, ly, a.emph, -1.0]);
            }
        }
        let row = nu + 1;
        for j in 0..nv {
            for i in 0..nu {
                let q = base + j * row + i;
                self.idx.extend_from_slice(&[q, q + 1, q + row, q + 1, q + row + 1, q + row]);
            }
        }
    }

    /// Open cylinder (vertical light column) of radius `r` and height `h` standing on `base`.
    fn beam(&mut self, base: Vec3, r: f32, h: f32, a: &Attr) {
        self.grid(16, 4, BEAM, a, |_, _, u, w| {
            let ang = u * std::f32::consts::TAU;
            let n = Vec3::new(ang.cos(), 0.0, ang.sin());
            (base + n * r + Vec3::Y * (w * h), n, w * h)
        });
    }

    /// Glowing tube of radius `r` from `a0` to `a1`.
    fn tube(&mut self, a0: Vec3, a1: Vec3, r: f32, a: &Attr) {
        let axis = (a1 - a0).normalize_or(Vec3::Y);
        let u_ax = axis.any_orthonormal_vector();
        let v_ax = axis.cross(u_ax);
        let len = a0.distance(a1);
        self.grid(8, 1, BAR, a, |_, _, u, w| {
            let ang = u * std::f32::consts::TAU;
            let n = u_ax * ang.cos() + v_ax * ang.sin();
            (a0 + (a1 - a0) * w + n * r, n, w * len)
        });
    }

    /// Flat ring band from `radius - width` to `radius` around `c` (x, z), each angular step at its own height
    /// `ys[i]` (+ 0.15 m); `ys` has `segs + 1` entries.
    fn ring(&mut self, c: Vec3, radius: f32, width: f32, ys: &[f32], segs: u32, a: &Attr) {
        let inner = (radius - width).max(0.5);
        self.grid(segs, 4, RING, a, |i, _, u, w| {
            let ang = u * std::f32::consts::TAU;
            let rr = inner + (radius - inner) * w;
            let y = ys.get(i as usize).copied().unwrap_or(c.y) + 0.15;
            (Vec3::new(c.x + ang.cos() * rr, y, c.z + ang.sin() * rr), Vec3::Y, 0.0)
        });
    }

    /// See-through curtain, `width` wide along `dir` (unit, x / z), `height` tall, centred on `base`.
    fn curtain(&mut self, base: Vec3, dir: Vec3, width: f32, height: f32, a: &Attr) {
        let n = Vec3::new(-dir.z, 0.0, dir.x);
        self.grid(4, 2, CURTAIN, a, |_, _, u, w| (base + dir * ((u - 0.5) * width) + Vec3::Y * (w * height), n, w * height));
    }

    fn into_mesh(self, vcap: usize, tcap: usize) -> Mesh {
        let Bake { mut pos, mut nrm, mut uv, mut shape, mut colour, mut fade, mut k, mut idx, .. } = self;
        // Padding: unused vertices on the first vertex (keeps the auto Aabb tight), degenerate triangles.
        let first = pos.first().copied().unwrap_or([0.0; 3]);
        pos.resize(vcap.max(pos.len()), first);
        let n = pos.len();
        nrm.resize(n, [0.0, 1.0, 0.0]);
        uv.resize(n, [0.0; 2]);
        shape.resize(n, [0.0; 2]);
        colour.resize(n, [0.0; 4]);
        fade.resize(n, [0.0; 4]);
        k.resize(n, [0.0, 0.0, 0.0, -1.0]);
        let ni = (tcap * 3).max(idx.len());
        idx.resize(ni, 0);
        Mesh::new(PrimitiveTopology::TriangleList, bevy::asset::RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, nrm)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uv)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_1, shape)
            .with_inserted_attribute(ATTRIBUTE_MARKER_COLOUR, colour)
            .with_inserted_attribute(ATTRIBUTE_MARKER_FADE, fade)
            .with_inserted_attribute(ATTRIBUTE_MARKER_K, k)
            .with_inserted_indices(Indices::U32(idx))
    }
}

/// Power-of-two capacity for `n` items given the current capacity `cur` (from `min`; grown at once, shrunk only below
/// a quarter), as race/visuals.rs.
fn capacity(n: usize, cur: usize, min: usize) -> usize {
    let want = n.max(1).next_power_of_two().max(min);
    if want > cur || want.saturating_mul(4) <= cur {
        want
    } else {
        cur
    }
}

/// Floating viewfinder frame (the photo "camera icon"): two crossing frames of four tubes, `w` x `h` m, centred on `c`.
fn viewfinder(b: &mut Bake, c: Vec3, w: f32, h: f32, a: &Attr) {
    for dir in [Vec3::X, Vec3::Z] {
        let (hw, hh) = (w * 0.5, h * 0.5);
        let p = [c - dir * hw - Vec3::Y * hh, c + dir * hw - Vec3::Y * hh, c + dir * hw + Vec3::Y * hh, c - dir * hw + Vec3::Y * hh];
        for k in 0..4 {
            b.tube(p[k], p[(k + 1) % 4], 0.18, a);
        }
    }
}

/// The pieces of one item (needs `prep`).
fn add_item(b: &mut Bake, it: &Item) {
    let st = style(it.kind, it.state);
    match (&it.shape, &it.prep) {
        (Shape::Disc(r), Some(Prep::Disc { ground, ys })) => {
            let (ring_fade, col_fade) = if it.kind == Kind::Outpost {
                ([2.0, 6.0, 900.0, 1200.0], [10.0, 40.0, 1000.0, 1200.0])
            } else {
                ([2.0, 6.0, 600.0, 800.0], [10.0, 40.0, 650.0, 800.0])
            };
            let segs = ys.len().saturating_sub(1) as u32;
            if segs >= 3 {
                b.ring(*ground, *r, ring_width(*r), ys, segs, &Attr::new(st.colour, st.intensity * st.ring, ring_fade, st.emphasis));
            }
            if let Some(h) = st.column {
                b.beam(*ground, 1.4, h, &Attr::new(st.colour, st.intensity, col_fade, st.emphasis));
                b.beam(*ground, 4.0, h * 0.7, &Attr::new(st.colour, st.intensity * 0.22, col_fade, st.emphasis));
                if it.kind == Kind::Photo {
                    viewfinder(b, *ground + Vec3::Y * (h * 0.35), 6.0, 4.0, &Attr::new(st.colour, st.intensity * 1.2, col_fade, 0.0));
                }
            }
        }
        (Shape::Gate(..), Some(Prep::Gate { l, r })) => {
            let (stripe_f, post_f, curtain_f) = ([1.0, 4.0, 300.0, 400.0], [3.0, 10.0, 300.0, 400.0], [3.0, 12.0, 300.0, 400.0]);
            let lift = Vec3::Y * 0.12;
            b.tube(*l + lift, *r + lift, 0.2, &Attr::new(st.colour, st.intensity * st.ring, stripe_f, 0.0));
            let post = Attr::new(st.colour, st.intensity * 1.1, post_f, 0.0);
            for p in [*l, *r] {
                b.tube(p, p + Vec3::Y * 4.5, 0.22, &post);
            }
            let d = Vec3::new(r.x - l.x, 0.0, r.z - l.z);
            let len = d.length();
            if len > 0.5 {
                let mid = (*l + *r) * 0.5;
                b.curtain(Vec3::new(mid.x, l.y.min(r.y), mid.z), d / len, len, 3.0, &Attr::new(st.colour, st.intensity * 0.7, curtain_f, 0.0));
            }
        }
        _ => {}
    }
}

fn build_bake(items: &[Item], shown: &[bool], origin: Vec3) -> Bake {
    let mut b = Bake::new(origin);
    for (it, on) in items.iter().zip(shown) {
        if *on {
            add_item(&mut b, it);
        }
    }
    b
}

// ---------------------------------------------------------------- system

/// Marks the one merged marker entity.
#[derive(Component)]
struct MissionMarkers;

#[derive(Resource, Default)]
struct Markers {
    items: Vec<Item>,
    /// (world generation, outposts, cameras, average zones, photo shoots) the items were built for.
    items_for: Option<(u32, usize, usize, usize, usize)>,
    /// (Profile.generation, MissionMapIcons.generation) the states are for.
    styled_for: Option<(u32, u32)>,
    shown: Vec<bool>,
    /// Photo focus the mesh is for.
    focus: Option<String>,
    /// The mesh must be rebuilt.
    dirty: bool,
    entity: Option<Entity>,
    mesh: Handle<Mesh>,
    material: Option<Handle<MissionMarkerMaterial>>,
    vcap: usize,
    tcap: usize,
}

#[allow(clippy::too_many_arguments)]
fn update_markers(
    mut commands: Commands,
    mut m: ResMut<Markers>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<MissionMarkerMaterial>>,
    missions: Res<Missions>,
    profile: Res<crate::progression::Profile>,
    icons: Res<MissionMapIcons>,
    focus: Res<MarkerFocus>,
    track: Res<crate::track::Track>,
    generation: Res<crate::ui::world_load::WorldGeneration>,
    rs: Option<Res<crate::race::RaceState>>,
    cars: Query<&Car>,
    mut ent: Query<(&mut Visibility, &mut Transform), With<MissionMarkers>>,
) {
    let m = &mut *m;
    if m.entity.is_some_and(|e| ent.get(e).is_err()) {
        // Despawned with the world (map switch): spawn again when needed.
        m.entity = None;
        m.dirty = true;
    }
    if super::race_running(&rs) || crate::cutscene::active() {
        if let Some((mut v, _)) = m.entity.and_then(|e| ent.get_mut(e).ok()) {
            v.set_if_neq(Visibility::Hidden);
        }
        m.dirty = true;
        return;
    }
    let d = &missions.data;
    let key = (generation.0, d.outposts.len(), d.speed_cameras.len(), d.average_speed.len(), d.photo_shoots.len());
    if m.items_for != Some(key) {
        m.items = build_items(d);
        m.items_for = Some(key);
        m.styled_for = None;
        m.shown.clear();
        m.dirty = true;
    }
    let sk = (profile.generation, icons.generation);
    if m.styled_for != Some(sk) {
        restyle(&mut m.items, d, &profile.data.missions);
        m.styled_for = Some(sk);
        m.dirty = true;
    }
    if m.focus != focus.0 {
        m.focus = focus.0.clone();
        m.dirty = true;
    }
    let Some(pos) = cars.iter().next().map(|c| c.0.position) else { return };
    if m.shown.len() != m.items.len() {
        m.shown = vec![false; m.items.len()];
        m.dirty = true;
    }
    for i in 0..m.items.len() {
        let it = &m.items[i];
        let lim = it.kind.show_m() + if m.shown[i] { HYSTERESIS_M } else { 0.0 };
        let on = it.active(&m.focus) && crate::race::states::icon_visible(Some(it.state)) && it.dist(pos) < lim;
        if on != m.shown[i] {
            m.shown[i] = on;
            m.dirty = true;
        }
    }
    if !m.dirty {
        return;
    }
    m.dirty = false;

    // Ground-snap what just came into range; the nearest shown item anchors the mesh.
    let y_at = |p: Vec3, r: f32| ground_y(&track, p, r);
    let mut anchor: Option<(f32, Vec3)> = None;
    for i in 0..m.items.len() {
        if !m.shown[i] {
            continue;
        }
        prep_item(&mut m.items[i], &y_at);
        let it = &m.items[i];
        let dd = it.dist(pos);
        if anchor.is_none_or(|a| dd < a.0) {
            anchor = Some((dd, it.pos));
        }
    }
    let Some((_, origin)) = anchor else {
        if let Some((mut v, _)) = m.entity.and_then(|e| ent.get_mut(e).ok()) {
            v.set_if_neq(Visibility::Hidden);
        }
        return;
    };
    let bake = build_bake(&m.items, &m.shown, origin);
    m.vcap = capacity(bake.pos.len(), m.vcap, 1024);
    m.tcap = capacity(bake.idx.len() / 3, m.tcap, 1024);
    let mesh = bake.into_mesh(m.vcap, m.tcap);
    match m.entity.and_then(|e| ent.get_mut(e).ok()) {
        Some((mut vis, mut t)) => {
            // Same handle, new asset: Bevy re-uploads and recomputes the Aabb (as race/visuals.rs).
            let _ = meshes.insert(&m.mesh, mesh);
            vis.set_if_neq(Visibility::Inherited);
            if t.translation != origin {
                t.translation = origin;
            }
        }
        None => {
            let material = m.material.get_or_insert_with(|| mats.add(MissionMarkerMaterial { params: MissionParams { colour: Vec4::ONE, fade: Vec4::ZERO, k: Vec4::ZERO } })).clone();
            m.mesh = meshes.add(mesh);
            let e = commands
                .spawn((
                    Mesh3d(m.mesh.clone()),
                    MeshMaterial3d(material),
                    Transform::from_translation(origin),
                    Visibility::Inherited,
                    NotShadowCaster,
                    NotShadowReceiver,
                    MissionMarkers,
                    crate::ui::world_load::WorldEntity,
                    Name::new("FH1 mission markers (merged)"),
                ))
                .id();
            m.entity = Some(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(_: Vec3, r: f32) -> f32 {
        r
    }

    fn disc(kind: Kind, state: IconState, r: f32) -> Item {
        let mut it = Item { kind, name: "x".into(), shoot: "s".into(), state, pos: Vec3::new(100.0, 5.0, -50.0), shape: Shape::Disc(r), prep: None };
        prep_item(&mut it, &flat);
        it
    }

    #[test]
    fn ring_mesh_counts_and_radii() {
        let it = disc(Kind::Outpost, IconState::Locked, 54.0);
        let Some(Prep::Disc { ground, ys }) = &it.prep else { panic!("prep") };
        let segs = ring_segments(54.0);
        assert_eq!(ys.len() as u32, segs + 1);
        let mut b = Bake::new(*ground);
        let a = Attr::new(LinearRgba::WHITE, 1.0, [0.0; 4], 0.0);
        b.ring(*ground, 54.0, ring_width(54.0), ys, segs, &a);
        assert_eq!(b.pos.len() as u32, (segs + 1) * 5);
        assert_eq!(b.idx.len() as u32, segs * 4 * 6);
        let (inner, outer) = (54.0 - ring_width(54.0), 54.0);
        for p in &b.pos {
            let r = Vec2::new(p[0], p[2]).length();
            assert!(r >= inner - 1e-3 && r <= outer + 1e-3, "radius {r}");
            assert!((p[1] - 0.15).abs() < 1e-4);
        }
        assert!(b.idx.iter().all(|&i| (i as usize) < b.pos.len()));
    }

    #[test]
    fn ring_segments_and_width_clamp() {
        assert_eq!(ring_segments(1.0), 24);
        assert_eq!(ring_segments(1000.0), 96);
        assert_eq!(ring_width(10.0), 2.0);
        assert_eq!(ring_width(500.0), 4.5);
    }

    #[test]
    fn outpost_states_to_style() {
        assert_eq!(outpost_state(false, 0, 3), IconState::Locked);
        assert_eq!(outpost_state(true, 2, 3), IconState::Available);
        assert_eq!(outpost_state(true, 3, 3), IconState::Done);
        assert_eq!(outpost_state(true, 0, 0), IconState::Available);
        let (a, d, l) = (style(Kind::Outpost, IconState::Available), style(Kind::Outpost, IconState::Done), style(Kind::Outpost, IconState::Locked));
        assert!(a.column.unwrap() > d.column.unwrap());
        assert!(a.intensity > d.intensity && d.intensity > l.intensity);
        assert!(l.column.is_none() && l.ring > 0.0);
    }

    #[test]
    fn camera_done_has_other_tint() {
        assert_eq!(best_state(true), IconState::Done);
        assert_eq!(best_state(false), IconState::Available);
        assert_ne!(style(Kind::Camera, IconState::Done).colour, style(Kind::Camera, IconState::Available).colour);
        assert_ne!(style(Kind::Average, IconState::Done).colour, style(Kind::Average, IconState::Available).colour);
        assert!(style(Kind::Camera, IconState::Available).column.is_none());
    }

    #[test]
    fn photo_only_with_focus() {
        let it = disc(Kind::Photo, IconState::Available, 50.0);
        assert!(!it.active(&None));
        assert!(!it.active(&Some("other".into())));
        assert!(it.active(&Some("s".into())));
        assert!(disc(Kind::Outpost, IconState::Available, 54.0).active(&None));
    }

    #[test]
    fn gate_pieces_and_distance() {
        let (l, r) = (Vec3::new(0.0, 1.0, 0.0), Vec3::new(20.0, 1.0, 0.0));
        let mut it = Item { kind: Kind::Camera, name: "c".into(), shoot: String::new(), state: IconState::Available, pos: (l + r) * 0.5, shape: Shape::Gate(l, r), prep: None };
        prep_item(&mut it, &flat);
        let b = build_bake(std::slice::from_ref(&it), &[true], it.pos);
        // stripe + 2 posts (8x1 each) + curtain (4x2).
        assert_eq!(b.pos.len(), 3 * 18 + 5 * 3);
        assert_eq!(b.idx.len(), 3 * 8 * 6 + 8 * 6);
        assert!(build_bake(std::slice::from_ref(&it), &[false], it.pos).pos.is_empty());
        // Inside the gate's reach the distance is zero; 500 m away it is 490.
        assert_eq!(it.dist(Vec3::new(10.0, 0.0, 0.0)), 0.0);
        assert!((it.dist(Vec3::new(510.0, 0.0, 0.0)) - 490.0).abs() < 1e-3);
    }

    #[test]
    fn capacity_grows_and_holds() {
        assert_eq!(capacity(10, 0, 1024), 1024);
        assert_eq!(capacity(2000, 1024, 1024), 2048);
        assert_eq!(capacity(600, 2048, 1024), 2048);
        assert_eq!(capacity(10, 8192, 1024), 1024);
    }

    #[test]
    fn mesh_is_padded() {
        let it = disc(Kind::Photo, IconState::Available, 50.0);
        let b = build_bake(std::slice::from_ref(&it), &[true], it.pos);
        let (nv, nt) = (b.pos.len(), b.idx.len() / 3);
        let (vcap, tcap) = (capacity(nv, 0, 1024), capacity(nt, 0, 1024));
        let mesh = b.into_mesh(vcap, tcap);
        assert_eq!(mesh.count_vertices(), vcap);
        assert_eq!(mesh.indices().map(|i| i.len()), Some(tcap * 3));
    }
}
