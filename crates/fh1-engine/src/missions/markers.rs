//! In-world markers for the free-roam missions on Colorado (docs/MISSIONS.md): Horizon Outposts, speed cameras,
//! average-speed gates and the photo-shoot spots, drawn in FH1's marker look so the player can see them while driving.
//! `FH1_MISSION_MARKERS=0` = none. Needs the race marker shader (`FH1_RACE_FX` on, race/visuals.rs embeds it).
//!
//! - Horizon Outpost (P19 redesign; the orange TriggerZone ring + 110 m light column were not FH1 and stood in the
//!   wrong place). VERIFIED: the outposts are physical Horizon-branded gas stations; their gas_stations.xml
//!   TriggerZone says `is_activation="false"` (as every venue: festival buildings, street race hubs; race events
//!   leave it out), the game ships no marker model / effect for them (no ANIM_GPLY_* or effects.zip piece but the race
//!   lasers), and the map icon is MapIconSheet cell (1,2): a white "H" in a white-rimmed black diamond, layer colour
//!   233,233,233 (MapProfileFullscreen.xml `gas_station`). VERIFIED (offline probe of Colorado's collision, test
//!   `anchors_have_ground`): 8 of the 10 `GASSTATION_NNN` points have no ground under them at all (they sit in the
//!   building), so the old column stood inside the shop and the 29-76 m ring cut through buildings; every
//!   `OUTPOST_NNN_NODE` (where the game parks the car for the outpost cutscene) is on open forecourt ground, no roof
//!   within 20 m. INFERRED (ours, restrained): at the node, one upright camera-facing emblem drawn as that map icon
//!   (style 20, 4 m, centre 6.5 m up, dark backing so it reads in daylight, a minimum screen size far away) and a soft
//!   6 m ground pad (style 22). Available = full; Done (all three missions) = dimmer, same hue (the map greys it);
//!   Locked (not discovered) = faint emblem, no pad. Emblem gone inside 6 m, in from 18 m; out by 700 m.
//! - Speed camera / average-speed zone (P19 redesign; the earlier stripe + curtain + glowing posts were not FH1):
//!   VERIFIED FH1 draws no marker effect for them. The traps are the physical festival camera posts ("a set of speed
//!   cameras mounted on top of posts", Forza Wiki "Speed Trap"; "speed trap photo boxes", "speed zones are defined
//!   by two linked speed trap devices", IGN FH1 guide), which we already place from GameObjs (`O_CO_FEST_SpeedCamera_001`
//!   677 / `O_CO_Fest_SpeedCamera_Average` 683, docs/PROPS.md). Their material is plain `h_diff_1.fx` (one diffuse
//!   texture, no emissive), and speed_camera.xml / average_speed.xml / ambient_challenge_activations.xml hold no
//!   marker, effect or beam behaviour (only game control + the scoreboard screen). The map icon is a camera: white
//!   for a speed trap, the same icon on yellow for a speed zone (Forza Wiki "Lawbreaker", xboxachievements.com).
//!   So ours stays minimal: one small soft glint (camera-facing sprite, style 30) on each post's camera box, white for
//!   a camera, yellow for a zone (the map icon colours), dimmer once it has a best (Done, INFERRED; same hue). The box
//!   is VERIFIED from the template bounds (rmb_list 677: box 0.09..1.32 m towards the road, 3.26..4.26 m up; 683: the
//!   arm's box ~2.8 m towards the road, 3.90..4.59 m up) and the GameObjs axes (every post's box points at its partner
//!   post), placed from the post's own origin (the prop's, not ground-snapped). It fades in from 30 m to 10 m (gone
//!   as you pass under it) and out by 400 m. `FH1_TRAP_LINE=1` (INFERRED, not FH1, default off) adds a faint soft
//!   line on the road between the two boxes, ground-sampled every ~1.5 m so it follows camber and slopes.
//! - Photo shoot: shown only while that mission runs ([`MarkerFocus`], set by outpost.rs). VERIFIED
//!   (mission_photoshoots.xml + TrackRoute NamedTransforms): a shoot's 1-32 `PhotoZone`s (radius 50, one 600) are
//!   overlapping circles 11-95 m apart that tile the area where the shot counts, not destinations, so the old ring +
//!   column on every zone was a field of up to 32 beacons; the one authored spot is `mission_photo_pose_01`, where
//!   the satnav already points (outpost.rs). The map's route and destination pin are green 74,238,97
//!   (MapProfileFullscreen.xml `route`). INFERRED: ONE marker at the pose: a viewfinder emblem (style 21), a soft
//!   9 m pad and a slender green beacon (style 23, 60 m) to find it from afar; gone up close; out by 1,500 m.
//! - Visible within a draw distance (outposts 700 m, camera / zone glints 400 m, photo spot 1,500 m, +60 m hysteresis);
//!   the shader fades them in / out by distance. Hidden during races (`RaceState::race`) and cutscenes.
//!
//! Perf (docs/PERF.md, PERF_P15_B marker merge): the same trick as race/visuals.rs `merged_markers`. Every visible
//! marker is baked into ONE world-space mesh on ONE entity with ONE material (additive, premultiplied so the emblems
//! can darken their backing; one transparent draw call in total, whatever is in range). Per-piece colour / intensity / fades ride in vertex attributes; pulse and rising
//! bands are animated in marker.wgsl from `globals.time`, so nothing is rewritten per frame. The mesh is rebuilt only
//! when the visible set, the profile (`Profile.generation` / `MissionMapIcons.generation`) or the focus changes,
//! padded to power-of-two capacities so the allocator reuses the range. Ground heights (a ray per ring vertex /
//! trap road-line point) are taken once per marker, the first time it comes into range. The entity is a `WorldEntity`, so a
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

/// The photo-shoot mission that is running (`photoshoot_NN`, a `PhotoShoot::name`), set by outpost.rs; its photo spot
/// gets a marker while it is `Some`.
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
                // Premultiplied: additive for every style that writes alpha 0, and the emblems' dark backing
                // (styles 20 / 21 write alpha) darkens what is behind them.
                t.blend = Some(BlendState {
                    color: BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::OneMinusSrcAlpha, operation: BlendOperation::Add },
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
            Kind::Outpost => 700.0,
            Kind::Camera | Kind::Average => 400.0,
            Kind::Photo => 1500.0,
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
    match (kind, state) {
        // Outpost: the map icon's white (layer colour 233); `intensity` = the emblem, `ring` = the ground pad, no
        // beacon (`column`). Done = greyed (dimmer), Locked = faint, no pad.
        (Kind::Outpost, IconState::Available) => Style { colour: hdr(233, 233, 233, 1.4), intensity: 1.0, column: None, ring: 0.45, emphasis: 0.2 },
        (Kind::Outpost, IconState::Done) => Style { colour: hdr(233, 233, 233, 1.4), intensity: 0.55, column: None, ring: 0.25, emphasis: 0.0 },
        (Kind::Outpost, IconState::Locked) => Style { colour: hdr(233, 233, 233, 1.4), intensity: 0.35, column: None, ring: 0.0, emphasis: 0.0 },
        // Glint colour = the FH1 map icon's (white trap, yellow zone); `ring` = the optional road line's intensity.
        (Kind::Camera | Kind::Average, s) => {
            let colour = if kind == Kind::Camera { hdr(255, 246, 228, 1.6) } else { hdr(255, 196, 40, 1.6) };
            let intensity = match s {
                IconState::Done => 0.5,
                IconState::Locked => 0.35,
                _ => 0.9,
            };
            Style { colour, intensity, column: None, ring: 0.35, emphasis: 0.0 }
        }
        // Photo spot: the map's route / destination green; `column` = the beacon height (m). Only shown while its
        // mission runs, so it is never Done / Locked in practice.
        (Kind::Photo, IconState::Available) => Style { colour: hdr(74, 238, 97, 1.4), intensity: 1.0, column: Some(PHOTO_BEACON_M), ring: 0.5, emphasis: 0.2 },
        (Kind::Photo, _) => Style { colour: hdr(74, 238, 97, 1.4), intensity: 0.5, column: Some(PHOTO_BEACON_M), ring: 0.3, emphasis: 0.0 },
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
    /// Ground points of the optional road line between the two camera boxes (empty without `FH1_TRAP_LINE=1`).
    Gate { line: Vec<Vec3> },
}

#[derive(Clone, Debug)]
struct Item {
    kind: Kind,
    name: String,
    /// Photo spot: the photo shoot it belongs to.
    shoot: String,
    state: IconState,
    /// Data position (outpost node / photo pose: the pad's centre; middle of a gate).
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
            pos: outpost_anchor(o),
            shape: Shape::Disc(OUTPOST_PAD_M),
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
        let Some(pos) = photo_anchor(s) else { continue };
        out.push(Item { kind: Kind::Photo, name: s.name.clone(), shoot: s.name.clone(), state: IconState::Available, pos, shape: Shape::Disc(PHOTO_PAD_M), prep: None });
    }
    out
}

/// Ground pad radii (m): a parking spot at the outpost, a little wider at the photo spot.
const OUTPOST_PAD_M: f32 = 6.0;
const PHOTO_PAD_M: f32 = 9.0;
/// Emblem centre above the ground (m) at its base size: its bottom edge is well over a car / van roof.
const EMBLEM_Y_M: f32 = 6.5;
/// Emblem half sizes (m, x across / y up): the outpost diamond, the 4:3 photo plate.
const OUTPOST_EMBLEM_HALF: (f32, f32) = (2.0, 2.0);
const PHOTO_EMBLEM_HALF: (f32, f32) = (2.4, 1.8);
/// Photo beacon height and half width (m).
const PHOTO_BEACON_M: f32 = 60.0;
const PHOTO_BEACON_HALF_W: f32 = 0.9;

/// Where an outpost's marker stands: `OUTPOST_NNN_NODE` (the game parks the car there for the outpost cutscene; on
/// the forecourt), not the `GASSTATION_NNN` TriggerZone centre (inside the building at 8 of 10 outposts, see the
/// module docs). An install without the node (all zero) falls back to the zone centre.
fn outpost_anchor(o: &super::data::Outpost) -> Vec3 {
    let node = Vec3::from_array(o.place.pos);
    if node == Vec3::ZERO {
        Vec3::from_array(o.pos)
    } else {
        node
    }
}

/// A photo shoot's one marker: `mission_photo_pose_01` (the satnav target), else its first zone.
fn photo_anchor(s: &super::data::PhotoShoot) -> Option<Vec3> {
    s.pose.map(|p| p.point()).or_else(|| s.zones.first().map(|z| Vec3::from_array(z.pos)))
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
    ground_at(&*track.ground, p, ref_y)
}

/// [`ground_y`] on any ground (the offline placement audit uses the world's directly).
fn ground_at(g: &dyn crate::vehicle::Ground, p: Vec3, ref_y: f32) -> f32 {
    if let Some(h) = g.ray(Vec3::new(p.x, ref_y + 3.0, p.z), Vec3::NEG_Y, 10.0) {
        return h.point.y;
    }
    if let Some(h) = g.ray(Vec3::new(p.x, ref_y + 50.0, p.z), Vec3::NEG_Y, 100.0) {
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
        Shape::Gate(l, r) => Prep::Gate { line: if trap_line_on() { gate_line(it.kind, l, r, y_at) } else { Vec::new() } },
    });
}

/// `FH1_TRAP_LINE=1`: a faint line on the road across each speed camera / zone gate (not FH1; default off).
fn trap_line_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_TRAP_LINE").is_ok_and(|v| v == "1"))
}

/// VERIFIED (bin.zip template bounds, `rmb_list` 677 / 683, and the GameObjs axes: every post's box overhangs towards
/// its partner post): the camera box centre from a post's origin, (metres towards the other post, metres up), and
/// the glint's half size (m). 677 `O_CO_FEST_SpeedCamera_001`: pole 3.68 m, box x 0.09..1.32, y 3.26..4.26.
/// 683 `O_CO_Fest_SpeedCamera_Average`: pole 3.82 m, an arm out to the box at ~2.8 m, y 3.90..4.59.
fn camera_head(kind: Kind) -> (f32, f32, f32) {
    if kind == Kind::Average {
        (2.78, 4.25, 0.95)
    } else {
        (0.70, 3.76, 1.05)
    }
}

/// The glint centres of a gate's two posts (data positions = the props' origins, so no ground snap).
fn gate_heads(kind: Kind, l: Vec3, r: Vec3) -> [Vec3; 2] {
    let (out, up, _) = camera_head(kind);
    let d = Vec3::new(r.x - l.x, 0.0, r.z - l.z).normalize_or(Vec3::X);
    [l + d * out + Vec3::Y * up, r - d * out + Vec3::Y * up]
}

/// Ground points from under one camera box to under the other, about every 1.5 m, each from its own ray (the line
/// follows camber, slopes and a bridge deck; the reference height is the posts' interpolated).
fn gate_line(kind: Kind, l: Vec3, r: Vec3, y_at: &dyn Fn(Vec3, f32) -> f32) -> Vec<Vec3> {
    let (out, ..) = camera_head(kind);
    let d = Vec3::new(r.x - l.x, 0.0, r.z - l.z);
    let len = d.length();
    if len < 2.0 * out + 1.0 {
        return Vec::new();
    }
    let dir = d / len;
    let (a, b) = (l + dir * out, r - dir * out);
    let n = ((len - 2.0 * out) / 1.5).ceil().max(2.0) as usize;
    (0..=n)
        .map(|k| {
            let p = a.lerp(b, k as f32 / n as f32);
            Vec3::new(p.x, y_at(p, p.y), p.z)
        })
        .collect()
}

// ---------------------------------------------------------------- mesh bake

/// marker.wgsl styles 20-23 (`mission_marker`): outpost emblem (the map's "H" diamond), photo emblem (viewfinder),
/// ground pad, beacon ribbon.
const EMBLEM_OUTPOST: u8 = 20;
const EMBLEM_PHOTO: u8 = 21;
const PAD: u8 = 22;
const BEACON: u8 = 23;
/// marker.wgsl style 30: soft camera-facing glint (speed camera / zone box).
const GLINT: u8 = 30;
/// marker.wgsl style 31: faint soft line on the road (`FH1_TRAP_LINE=1`).
const ROAD_LINE: u8 = 31;

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

    /// Upright camera-facing emblem (marker.wgsl 20 / 21) centred on `c`, `hx` x `hy` m half size. Each corner's
    /// position is where it would be facing +Z (so the mesh Aabb covers it), its normal the offset from `c`, and the
    /// local height `hy` (the shader grows the emblem upwards from its bottom edge when it scales it up far away).
    fn emblem(&mut self, c: Vec3, hx: f32, hy: f32, style: u8, a: &Attr) {
        self.grid(1, 1, style, a, |_, _, u, w| {
            let o = Vec3::new((u * 2.0 - 1.0) * hx, (w * 2.0 - 1.0) * hy, 0.0);
            (c + o, o, hy)
        });
    }

    /// Upright camera-facing light ribbon (marker.wgsl 23), `half_w` m half width, `h` m tall, standing on `base`;
    /// the normal carries each vertex's sideways offset (the shader turns it to the camera and widens it far away).
    fn beacon(&mut self, base: Vec3, half_w: f32, h: f32, a: &Attr) {
        self.grid(1, 6, BEACON, a, |_, _, u, w| {
            let o = Vec3::new((u * 2.0 - 1.0) * half_w, 0.0, 0.0);
            (base + Vec3::Y * (w * h) + o, o, w * h)
        });
    }

    /// Flat ground pad (marker.wgsl 22): an annulus from `radius - width` to `radius` around `c` (x, z), each angular
    /// step at its own height `ys[i]` (+ 0.15 m); `ys` has `segs + 1` entries.
    fn ring(&mut self, c: Vec3, radius: f32, width: f32, ys: &[f32], segs: u32, a: &Attr) {
        let inner = (radius - width).max(0.5);
        self.grid(segs, 2, PAD, a, |i, _, u, w| {
            let ang = u * std::f32::consts::TAU;
            let rr = inner + (radius - inner) * w;
            let y = ys.get(i as usize).copied().unwrap_or(c.y) + 0.15;
            (Vec3::new(c.x + ang.cos() * rr, y, c.z + ang.sin() * rr), Vec3::Y, 0.0)
        });
    }

    /// Camera-facing glint sprite at `c`, `half` m half size: all four vertices sit on `c`, the normal carries the
    /// corner offset in the view plane (x right, y up) and z = how far it is pulled towards the camera (so the box
    /// doesn't cut it); marker.wgsl expands it.
    fn glint(&mut self, c: Vec3, half: f32, pull: f32, a: &Attr) {
        self.grid(1, 1, GLINT, a, |_, _, u, w| (c, Vec3::new((u * 2.0 - 1.0) * half, (w * 2.0 - 1.0) * half, pull), 0.0));
    }

    /// Flat strip `width` m wide along the ground points `pts` (+ 0.1 m), u along, w across.
    fn road_line(&mut self, pts: &[Vec3], width: f32, a: &Attr) {
        let (Some(first), Some(last)) = (pts.first(), pts.last()) else { return };
        let d = Vec3::new(last.x - first.x, 0.0, last.z - first.z).normalize_or(Vec3::X);
        let side = Vec3::new(-d.z, 0.0, d.x) * width;
        let n = pts.len().saturating_sub(1) as u32;
        if n == 0 {
            return;
        }
        self.grid(n, 2, ROAD_LINE, a, |i, _, _, w| (pts[i as usize] + Vec3::Y * 0.1 + side * (w - 0.5), Vec3::Y, 0.0));
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

/// The pieces of one item (needs `prep`).
fn add_item(b: &mut Bake, it: &Item) {
    let st = style(it.kind, it.state);
    match (&it.shape, &it.prep) {
        (Shape::Disc(r), Some(Prep::Disc { ground, ys })) => {
            // Fades (near in / out, far in / out, m). The emblem is gone right under it (6 m) and back by 18 m; the pad
            // is a near-field cue only; the photo beacon goes once you are there (60 -> 20 m).
            let (pad_f, emblem_f, beacon_f) = if it.kind == Kind::Outpost {
                ([1.0, 4.0, 200.0, 280.0], [6.0, 18.0, 600.0, 700.0], [0.0; 4])
            } else {
                ([1.0, 4.0, 300.0, 400.0], [6.0, 18.0, 900.0, 1000.0], [20.0, 60.0, 1350.0, 1500.0])
            };
            // Draw order inside the one draw: pad, beacon, then the emblem, so its dark backing sits in front of the beacon.
            let segs = ys.len().saturating_sub(1) as u32;
            if st.ring > 0.0 && segs >= 3 {
                b.ring(*ground, *r, *r * 0.6, ys, segs, &Attr::new(st.colour, st.intensity * st.ring, pad_f, st.emphasis));
            }
            if let Some(h) = st.column {
                b.beacon(*ground, PHOTO_BEACON_HALF_W, h, &Attr::new(st.colour, st.intensity * 0.6, beacon_f, 0.0));
            }
            let (style_id, (hx, hy)) = if it.kind == Kind::Outpost { (EMBLEM_OUTPOST, OUTPOST_EMBLEM_HALF) } else { (EMBLEM_PHOTO, PHOTO_EMBLEM_HALF) };
            b.emblem(*ground + Vec3::Y * EMBLEM_Y_M, hx, hy, style_id, &Attr::new(st.colour, st.intensity, emblem_f, st.emphasis));
        }
        (Shape::Gate(l, r), Some(Prep::Gate { line })) => {
            // Glints: gone as you pass under (30 -> 10 m), out by the draw distance. Road line: near only.
            let (glint_f, line_f) = ([10.0, 30.0, 330.0, 400.0], [2.0, 6.0, 110.0, 160.0]);
            let (.., half) = camera_head(it.kind);
            let glint = Attr::new(st.colour, st.intensity, glint_f, 0.0);
            for c in gate_heads(it.kind, *l, *r) {
                b.glint(c, half, 0.8, &glint);
            }
            if line.len() >= 2 {
                b.road_line(line, 0.6, &Attr::new(st.colour, st.intensity * st.ring, line_f, 0.0));
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
    fn pad_mesh_counts_and_radii() {
        let it = disc(Kind::Outpost, IconState::Available, OUTPOST_PAD_M);
        let Some(Prep::Disc { ground, ys }) = &it.prep else { panic!("prep") };
        let segs = ring_segments(OUTPOST_PAD_M);
        assert_eq!(segs, 24);
        assert_eq!(ys.len() as u32, segs + 1);
        let mut b = Bake::new(*ground);
        let a = Attr::new(LinearRgba::WHITE, 1.0, [0.0; 4], 0.0);
        b.ring(*ground, OUTPOST_PAD_M, OUTPOST_PAD_M * 0.6, ys, segs, &a);
        assert_eq!(b.pos.len() as u32, (segs + 1) * 3);
        assert_eq!(b.idx.len() as u32, segs * 2 * 6);
        let (inner, outer) = (OUTPOST_PAD_M * 0.4, OUTPOST_PAD_M);
        for p in &b.pos {
            let r = Vec2::new(p[0], p[2]).length();
            assert!(r >= inner - 1e-3 && r <= outer + 1e-3, "radius {r}");
            assert!((p[1] - 0.15).abs() < 1e-4);
        }
        assert!(b.shape.iter().all(|s| s[0] == PAD as f32));
        assert!(b.idx.iter().all(|&i| (i as usize) < b.pos.len()));
    }

    #[test]
    fn ring_segments_clamp() {
        assert_eq!(ring_segments(1.0), 24);
        assert_eq!(ring_segments(1000.0), 96);
    }

    #[test]
    fn outpost_states_to_style() {
        assert_eq!(outpost_state(false, 0, 3), IconState::Locked);
        assert_eq!(outpost_state(true, 2, 3), IconState::Available);
        assert_eq!(outpost_state(true, 3, 3), IconState::Done);
        assert_eq!(outpost_state(true, 0, 0), IconState::Available);
        let (a, d, l) = (style(Kind::Outpost, IconState::Available), style(Kind::Outpost, IconState::Done), style(Kind::Outpost, IconState::Locked));
        // The map icon's white in every state (greyed = dimmer), no beacon; Locked has no pad.
        assert!(a.colour == d.colour && d.colour == l.colour);
        assert!((a.colour.red - a.colour.blue).abs() < 1e-6 && (a.colour.red - a.colour.green).abs() < 1e-6);
        assert!(a.column.is_none() && d.column.is_none() && l.column.is_none());
        assert!(a.intensity > d.intensity && d.intensity > l.intensity);
        assert!(a.ring > d.ring && l.ring == 0.0);
    }

    #[test]
    fn outpost_pieces_pad_then_emblem() {
        let it = disc(Kind::Outpost, IconState::Available, OUTPOST_PAD_M);
        let b = build_bake(std::slice::from_ref(&it), &[true], it.pos);
        let segs = ring_segments(OUTPOST_PAD_M) as usize;
        assert_eq!(b.pos.len(), (segs + 1) * 3 + 4);
        assert_eq!(b.shape.last().map(|s| s[0]), Some(EMBLEM_OUTPOST as f32));
        assert!(!b.shape.iter().any(|s| s[0] == BEACON as f32));
        // The emblem: four corners around a centre EMBLEM_Y_M over the ground, normal = offset, k.y = half height.
        let n = b.pos.len();
        let (hx, hy) = OUTPOST_EMBLEM_HALF;
        for v in n - 4..n {
            let (p, o) = (Vec3::from_array(b.pos[v]), Vec3::from_array(b.nrm[v]));
            assert!((p - o - Vec3::Y * EMBLEM_Y_M).length() < 1e-4, "{p} {o}");
            assert!((o.x.abs() - hx).abs() < 1e-5 && (o.y.abs() - hy).abs() < 1e-5 && o.z == 0.0);
            assert_eq!(b.k[v][1], hy);
        }
        // Locked: the emblem only.
        let l = disc(Kind::Outpost, IconState::Locked, OUTPOST_PAD_M);
        assert_eq!(build_bake(std::slice::from_ref(&l), &[true], l.pos).pos.len(), 4);
    }

    #[test]
    fn photo_pieces_pad_beacon_emblem() {
        let it = disc(Kind::Photo, IconState::Available, PHOTO_PAD_M);
        let b = build_bake(std::slice::from_ref(&it), &[true], it.pos);
        let segs = ring_segments(PHOTO_PAD_M) as usize;
        assert_eq!(b.pos.len(), (segs + 1) * 3 + 2 * 7 + 4);
        let styles: Vec<u8> = b.shape.iter().map(|s| s[0] as u8).collect();
        let first = |s: u8| styles.iter().position(|&x| x == s).unwrap();
        assert!(first(PAD) < first(BEACON) && first(BEACON) < first(EMBLEM_PHOTO));
        // The beacon stands on the pad's centre, PHOTO_BEACON_M tall, offsets only sideways.
        let top = (0..b.pos.len()).filter(|&v| styles[v] == BEACON).map(|v| b.pos[v][1]).fold(0.0f32, f32::max);
        assert!((top - PHOTO_BEACON_M).abs() < 1e-3);
        for v in (0..b.pos.len()).filter(|&v| styles[v] == BEACON) {
            let o = Vec3::from_array(b.nrm[v]);
            assert!((o.x.abs() - PHOTO_BEACON_HALF_W).abs() < 1e-5 && o.y == 0.0 && o.z == 0.0);
            assert!((b.pos[v][0] - o.x).abs() < 1e-4 && b.pos[v][2].abs() < 1e-4);
        }
    }

    fn data_with(outposts: Vec<super::super::data::Outpost>, shoots: Vec<super::super::data::PhotoShoot>) -> MissionData {
        MissionData { outposts, photo_shoots: shoots, ..Default::default() }
    }

    #[test]
    fn outposts_stand_at_the_node_and_one_marker_per_shoot() {
        use super::super::data::{Outpost, PhotoShoot, PhotoZone, Pose};
        let o = Outpost { name: "GasStation_01".into(), pos: [100.0, 5.0, 100.0], radius: 54.0, place: Pose { pos: [80.0, 5.2, 120.0], yaw: 0.0 }, ..Default::default() };
        let bare = Outpost { name: "GasStation_02".into(), pos: [300.0, 1.0, 0.0], ..Default::default() };
        let zone = |x: f32| PhotoZone { pos: [x, 0.0, 0.0], radius: 50.0, max_mph: None };
        let s = PhotoShoot { name: "photoshoot_02".into(), zones: (0..32).map(|i| zone(i as f32 * 40.0)).collect(), pose: Some(Pose { pos: [500.0, 2.0, 7.0], yaw: 0.0 }), ..Default::default() };
        let no_pose = PhotoShoot { name: "photoshoot_07".into(), zones: vec![PhotoZone { pos: [9.0, 1.0, 9.0], radius: 600.0, max_mph: None }], ..Default::default() };
        let items = build_items(&data_with(vec![o, bare], vec![s, no_pose]));
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].pos, Vec3::new(80.0, 5.2, 120.0));
        assert_eq!(items[0].shape, Shape::Disc(OUTPOST_PAD_M));
        assert_eq!(items[1].pos, Vec3::new(300.0, 1.0, 0.0));
        assert_eq!((items[2].kind, items[2].pos, items[2].shoot.as_str()), (Kind::Photo, Vec3::new(500.0, 2.0, 7.0), "photoshoot_02"));
        assert_eq!(items[3].pos, Vec3::new(9.0, 1.0, 9.0));
        assert_eq!(items[3].shape, Shape::Disc(PHOTO_PAD_M));
    }

    /// Placement audit on the installed Colorado (skipped without the install): every outpost / photo anchor has
    /// drivable ground within 1.5 m of its data height and no roof over the emblem.
    #[test]
    fn anchors_have_ground() {
        use crate::vehicle::Ground;
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(private) = crate::data::private_assets(&root.join("data")) else { return };
        let Ok(g) = fh1_engine::world::WorldGround::load(&private.join("world/colorado")) else { return };
        let d = MissionData::load(&private);
        let items = build_items(&d);
        let mut zone_centres_in_buildings = 0;
        for o in &d.outposts {
            let p = Vec3::from_array(o.pos);
            if g.ray(p + Vec3::Y * 50.0, Vec3::NEG_Y, 100.0).is_none() {
                zone_centres_in_buildings += 1;
            }
        }
        for it in items.iter().filter(|it| matches!(it.kind, Kind::Outpost | Kind::Photo)) {
            let hit = g.ray(it.pos + Vec3::Y * 3.0, Vec3::NEG_Y, 10.0);
            let Some(h) = hit else { panic!("{}: no ground under the anchor {}", it.name, it.pos) };
            assert!((h.point.y - it.pos.y).abs() < 1.5, "{}: ground {} vs data {}", it.name, h.point.y, it.pos.y);
            assert!((ground_at(&g, it.pos, it.pos.y) - h.point.y).abs() < 1e-3);
            let roof = g.ray(h.point + Vec3::Y * 0.5, Vec3::Y, EMBLEM_Y_M + 4.0);
            assert!(roof.is_none(), "{}: roof {:?} over the emblem", it.name, roof.map(|r| r.point.y - h.point.y));
        }
        eprintln!("{} outposts, {zone_centres_in_buildings} GASSTATION points without ground under them", d.outposts.len());
    }

    #[test]
    fn camera_done_is_dimmer_same_hue() {
        assert_eq!(best_state(true), IconState::Done);
        assert_eq!(best_state(false), IconState::Available);
        for k in [Kind::Camera, Kind::Average] {
            let (a, d) = (style(k, IconState::Available), style(k, IconState::Done));
            assert_eq!(a.colour, d.colour);
            assert!(d.intensity < a.intensity);
            assert!(a.column.is_none());
        }
        // Map icon colours: white trap, yellow zone (blue well below red / green).
        let (c, z) = (style(Kind::Camera, IconState::Available).colour, style(Kind::Average, IconState::Available).colour);
        assert!(c.blue > 0.7 * c.red && z.blue < 0.1 * z.red && z.green > 0.4 * z.red);
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
        // One glint quad per post (the road line is off without FH1_TRAP_LINE=1).
        assert_eq!(b.pos.len(), 2 * 4);
        assert_eq!(b.idx.len(), 2 * 6);
        assert!(b.shape.iter().all(|s| s[0] == GLINT as f32));
        assert!(build_bake(std::slice::from_ref(&it), &[false], it.pos).pos.is_empty());
        // Inside the gate's reach the distance is zero; 500 m away it is 490.
        assert_eq!(it.dist(Vec3::new(10.0, 0.0, 0.0)), 0.0);
        assert!((it.dist(Vec3::new(510.0, 0.0, 0.0)) - 490.0).abs() < 1e-3);
    }

    #[test]
    fn glints_sit_on_the_camera_boxes() {
        // Posts 16 m apart along +z, the left one 0.5 m lower: each glint hangs over the road from its own post.
        let (l, r) = (Vec3::new(5.0, 10.0, 0.0), Vec3::new(5.0, 10.5, 16.0));
        let [a, b] = gate_heads(Kind::Camera, l, r);
        assert!((a - Vec3::new(5.0, 13.76, 0.70)).length() < 1e-4, "{a}");
        assert!((b - Vec3::new(5.0, 14.26, 15.30)).length() < 1e-4, "{b}");
        let [a, b] = gate_heads(Kind::Average, l, r);
        assert!((a.z - 2.78).abs() < 1e-4 && (b.z - (16.0 - 2.78)).abs() < 1e-4);
        assert!((a.y - 14.25).abs() < 1e-4);
    }

    #[test]
    fn road_line_follows_the_ground() {
        let (l, r) = (Vec3::new(0.0, 1.0, 0.0), Vec3::new(20.0, 1.0, 0.0));
        // A cambered road: crown 0.3 m above the edges.
        let camber = |p: Vec3, _: f32| 1.0 + 0.3 * (1.0 - ((p.x - 10.0) / 10.0).abs());
        let pts = gate_line(Kind::Camera, l, r, &camber);
        assert!(pts.len() >= 12);
        assert!((pts[0].x - 0.7).abs() < 1e-4 && (pts.last().unwrap().x - 19.3).abs() < 1e-4);
        for p in &pts {
            assert!((p.y - camber(*p, 0.0)).abs() < 1e-4);
        }
        let mut b = Bake::new(Vec3::ZERO);
        b.road_line(&pts, 0.6, &Attr::new(LinearRgba::WHITE, 1.0, [0.0; 4], 0.0));
        assert_eq!(b.pos.len(), pts.len() * 3);
        assert!(b.idx.iter().all(|&i| (i as usize) < b.pos.len()));
        // Too narrow for a line between the boxes.
        assert!(gate_line(Kind::Average, l, Vec3::new(5.0, 1.0, 0.0), &camber).is_empty());
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
