//! Race visuals in the world (remaster style, 2026-10-08; `FH1_RACE_FX=0` = the old gizmo lines in race.rs).
//!
//! FH1's look, rebuilt: festival event starts are tall columns of light in the wristband colour with a ground ring
//! and outward ripples over the 25 m TriggerZone; street-race starts the same in blue; checkpoints are glowing gates
//! (posts, top bar, a see-through curtain with a rising sweep) with floor chevrons leading in; the finish has a
//! chequered curtain; passing a gate fires a ripple. One additive unlit material (race/marker.wgsl), soft SDF shapes,
//! HDR colour so bloom catches it, a 1.2 Hz pulse, near / far distance fades (no depth prepass in the remaster, so no
//! scene-depth soft edges: shapes fade out at their foot instead of cutting into the ground). Shares its look with
//! the world map's road chevrons (e4): track pink for the race, tier colours for event starts, group blue for street.
//!
//! Merged draw (P14, 2026-10-08 night; `FH1_MARKER_MERGE=0` = the old path, one entity + one sorted transparent draw
//! per piece: 3-4 per visible event marker, 13 for the two gates + chevrons, 1 per pass ripple): every piece is baked
//! into ONE world-space mesh on ONE entity with ONE material (one transparent draw), rebuilt only when the set of
//! pieces changes (marker visible / look, gate progress, a pass ripple starting or ending). Per-piece colour,
//! intensity, fades and emphasis ride in vertex attributes; pulse, sweeps and the pass ripple's growth are animated
//! in the shader from `globals.time`, so nothing is rewritten per frame. The mesh is padded to power-of-two vertex /
//! triangle capacities (as fh1-render particles `pad_quad_mesh`) so the mesh allocator reuses the freed range.
//! Gameplay (gate crossing in race.rs) never reads these entities.
//!
//! Checkpoint / finish lasers (redone 2026-10-09 after "ugly, miscoloured, positioned wrong"; `FH1_RACE_LASERS=0` =
//! the posts + curtain + chevrons above). docs/RACES.md "Checkpoint lasers" has the evidence.
//! - VERIFIED (default.xex strings + loader 0x824D4BA0, media/animatedobjects.zip, TrackRoute*.xml): the game loads two
//!   gameplay objects, `ANIM_GPLY_Laser_Checkpoint` and `ANIM_GPLY_Final_Laser`, each with `_On` / `_Off` animations.
//!   One object = a ~1.8 m emitter plate on the road (`checkpoint_laser`, white `_EMIS` lens) and three beams from that
//!   one point, 120 degrees apart, each a crossed ribbon ~600 m long (`GR_Laser_Blue_DIFF`: pure blue 0,0,115..214;
//!   finish `GR_Laser_Final_DIFF` 16,211,0; the top cap fades out, `_End`), each wrapped in a ~3-6 m light haze
//!   (`OBJ_LightLaser_DIFF`: lavender 156,138,214 at the source fading to 8,8,123; finish mint 239,251,247 -> 99,223,71),
//!   swinging out to ~6 degrees and back twice per 5 s loop while the fan turns. The blue one also has a 17 x 139 m glow
//!   rising from the plate. `_Off` scales the beams to zero (only the plate is left). Additive (SRCALPHA / ONE).
//! - VERIFIED placement data: every street `route_checkpoint_NN` has one `route_checkpoint_indicator_NN` on the road
//!   centre line a few metres past the trigger (735 checkpoints: median 6.1 m ahead, 0.0 m across); the loader reads the
//!   second `_NNb` indicator for the LAST checkpoint only, and the finish pairs straddle the road at its edges (131
//!   pairs, median 13.2 m apart). Routes without `route_checkpoint_NN` (festival races: gates on the sparse waypoints)
//!   make the loader return before any laser is set up. A reviewer (Saving Content, 2012): "checkpoints that appear as
//!   beams of light", hard to see by day, "wonderful" at night.
//! - INFERRED: one laser on (the next checkpoint) at a time, the one just passed switching off (beams retract into the
//!   plate); the indicators themselves aren't in events.json, so the single laser stands `CHECKPOINT_AHEAD_M` past the
//!   checkpoint on its facing and the finish pair at the road edges found by ground rays (`road_edges`); the beam
//!   motion is a fit to the decoded animation (marker.wgsl `laser_dir`).
//! - Festival races: the loader reading above says they get no checkpoint lasers, but the user's playtest (2026-10-09:
//!   "the finish lights were there but no checkpoint ones") remembers FH1 showing them in every race, which wins: their
//!   waypoint gates get the blue laser too, on the race's road path at the gate (`on_path`; waypoints have no
//!   indicator and can sit off the tarmac). `FH1_RACE_FESTIVAL_LASERS=0` = the strict reading (finish only).
//!
//! Procedural in marker.wgsl (merged path only): 7 beam, 9 haze, 10 base glow (camera-facing ribbons swung in the
//! vertex shader, so the mesh is only rebuilt when the target gate changes), 8 emitter plate. The game's texture colours
//! at their own (non-HDR) strength: overlapping hazes near the plate are what blooms.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use bevy::asset::embedded_asset;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::math::Affine3A;
use bevy::mesh::{Indices, MeshVertexAttribute, MeshVertexBufferLayoutRef, PrimitiveTopology, VertexFormat};
use bevy::pbr::{MaterialPipeline, MaterialPipelineKey};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;

use super::{gate, total_gates, Events, RacePhase, RaceState};
use crate::progression::{EventCatalog, EventState};
use crate::Car;

/// `FH1_RACE_FX=0`: the old gizmo markers.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_FX").map_or(true, |v| v != "0"))
}

/// `FH1_RACE_LASERS=0`: the old posts / curtain / chevron gates (merged path only; the lasers are the game's own
/// checkpoint look, see the module docs).
pub fn lasers_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_LASERS").map_or(true, |v| v != "0") && merge_on())
}

/// `FH1_RACE_FESTIVAL_LASERS=0`: no checkpoint laser on festival races' waypoint gates (only the finish), the strict
/// reading of the game's loader. Default on: the user's playtest (2026-10-09) remembers FH1 showing them in every race.
fn festival_lasers_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_FESTIVAL_LASERS").map_or(true, |v| v != "0"))
}

/// `FH1_MARKER_MERGE=0`: one entity / draw per marker piece (the path before P14) instead of one merged mesh.
pub fn merge_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_MARKER_MERGE").map_or(true, |v| v != "0"))
}

/// Merged mesh: per-piece colour (rgb display-linear HDR, a = intensity).
const ATTRIBUTE_MARKER_COLOUR: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_MarkerColour", 0x4648_0060, VertexFormat::Float32x4);
/// Merged mesh: near fade start / end, far fade start / end (m).
const ATTRIBUTE_MARKER_FADE: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_MarkerFade", 0x4648_0061, VertexFormat::Float32x4);
/// Merged mesh: x pulse Hz (laser styles: beam index + 4 for the finish), y template-local height (beam bands), z
/// emphasis, w pass-ripple / laser-off birth (wrapped seconds, `globals.time` clock) or -1 for a static piece.
const ATTRIBUTE_MARKER_K: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_MarkerK", 0x4648_0062, VertexFormat::Float32x4);
/// Pass ripple: scale grows by `exp(FLASH_GROW * age)` for `FLASH_LIFE` s (the old per-frame `scale *= 1 + 1.5 dt`,
/// despawned after 0.9 s). Mirrored in marker.wgsl.
const FLASH_GROW: f32 = 1.5;
const FLASH_LIFE: f32 = 0.9;

/// Shared with the map's chevrons (e4): MapProfileFullscreen.xml "track" pink and group-route blue, x2 HDR.
fn pink() -> LinearRgba {
    Color::srgb_u8(250, 0, 100).to_linear() * 2.0
}
/// The game's laser textures (module docs), as stored: the beam's bright stripe (`GR_Laser_Blue_DIFF` 0,0,214;
/// finish `GR_Laser_Final_DIFF` 16,211,0) and the haze at its source (`OBJ_LightLaser_DIFF` 156,138,214; finish
/// `OBJ_LightLaser_Final_DIFF` 239,251,247). The haze's far colour is in marker.wgsl.
fn beam_colour(finish: bool) -> LinearRgba {
    if finish { Color::srgb_u8(16, 211, 0) } else { Color::srgb_u8(0, 0, 214) }.to_linear()
}
fn haze_colour(finish: bool) -> LinearRgba {
    if finish { Color::srgb_u8(239, 251, 247) } else { Color::srgb_u8(156, 138, 214) }.to_linear()
}
/// Beam length (bind pose 614 m; whether the groups' 0.7 scale shortens them to ~430 m is not resolved) and width
/// (~1 m ribbon x 2 in `_On`, x 0.7).
const LASER_LEN: f32 = 600.0;
const BEAM_W: f32 = 1.2;
/// Haze ribbon: ~1 m wide at the plate growing to `HAZE_W` (the strobe planes are 2.5-6.3 m wide; the texture's glow
/// widens along them).
const HAZE_W: f32 = 6.0;
const HAZE_LEN: f32 = 640.0;
/// The blue object's base glow plane (`LightStrobe_009`: 17 x 139 m).
const GLOW_W: f32 = 17.0;
const GLOW_LEN: f32 = 139.0;
/// Emitter plate radius (`Cylinder001` 1.2 m across x 1.5).
const PLATE_R: f32 = 0.9;
/// Where the single checkpoint laser stands past the trigger line (indicator median 6.1 m; module docs).
const CHECKPOINT_AHEAD_M: f32 = 6.0;
/// Half the finish pair's spacing where the road edge isn't found (indicator pairs: median 13.2 m apart).
const FINISH_HALF_M: f32 = 6.6;
/// A passed laser's beams retract into the plate over this long (mirrored in marker.wgsl `LASER_OFF_S`).
const LASER_OFF_S: f32 = 0.6;
fn street_blue() -> LinearRgba {
    Color::srgb_u8(56, 167, 255).to_linear() * 2.0
}
const PULSE_HZ: f32 = 1.2;
/// Event markers drawn within this distance (beams fade out before it).
const MARKER_SHOW_M: f32 = 2600.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType)]
pub struct MarkerParams {
    colour: Vec4,
    fade: Vec4,
    k: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct MarkerMaterial {
    #[uniform(0)]
    params: MarkerParams,
}

impl Material for MarkerMaterial {
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
        if layout.0.contains(ATTRIBUTE_MARKER_K) {
            // The merged mesh (see the module docs): per-piece parameters per vertex.
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
        } else {
            d.vertex.buffers = vec![layout.0.get_layout(&[
                Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
                Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
                Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
                Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
            ])?];
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

pub struct RaceVisualsPlugin;

impl Plugin for RaceVisualsPlugin {
    fn build(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        embedded_asset!(app, "marker.wgsl");
        app.add_plugins(MaterialPlugin::<MarkerMaterial>::default())
            .init_resource::<Visuals>()
            .add_systems(Startup, build_meshes)
            .add_systems(Update, cannons.after(super::race_update));
        if merge_on() {
            app.init_resource::<Merged>().add_systems(Update, merged_markers.after(super::race_update));
        } else {
            app.add_systems(Update, (event_markers, gate_visuals, pass_flash).after(super::race_update));
        }
    }
}

// ---------------------------------------------------------------- meshes

const BEAM: u8 = 0;
const RING: u8 = 1;
const RIPPLE: u8 = 2;
const CURTAIN: u8 = 3;
const CHEVRON: u8 = 4;
const FINISH: u8 = 5;
const BAR: u8 = 6;
const LASER: u8 = 7;
const EMITTER: u8 = 8;
const HAZE: u8 = 9;
const GLOW: u8 = 10;

struct Builder {
    pos: Vec<[f32; 3]>,
    nrm: Vec<[f32; 3]>,
    uv: Vec<[f32; 2]>,
    shape: Vec<[f32; 2]>,
    idx: Vec<u32>,
}

impl Builder {
    fn new() -> Self {
        Self { pos: Vec::new(), nrm: Vec::new(), uv: Vec::new(), shape: Vec::new(), idx: Vec::new() }
    }
    fn v(&mut self, p: Vec3, n: Vec3, uv: Vec2, style: u8, phase: f32) -> u32 {
        self.pos.push(p.into());
        self.nrm.push(n.into());
        self.uv.push(uv.into());
        self.shape.push([style as f32, phase]);
        self.pos.len() as u32 - 1
    }
    /// A grid of quads: `f(i, j)` = (position, normal) for i in 0..=nu, j in 0..=nv; uv = (i / nu, j / nv).
    fn grid(&mut self, nu: u32, nv: u32, style: u8, phase: f32, f: impl Fn(f32, f32) -> (Vec3, Vec3)) {
        let base = self.pos.len() as u32;
        for j in 0..=nv {
            for i in 0..=nu {
                let (u, w) = (i as f32 / nu as f32, j as f32 / nv as f32);
                let (p, n) = f(u, w);
                self.v(p, n, Vec2::new(u, w), style, phase);
            }
        }
        let row = nu + 1;
        for j in 0..nv {
            for i in 0..nu {
                let a = base + j * row + i;
                self.idx.extend_from_slice(&[a, a + 1, a + row, a + 1, a + row + 1, a + row]);
            }
        }
    }
    fn build(self) -> Mesh {
        Mesh::new(PrimitiveTopology::TriangleList, bevy::asset::RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.nrm)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, self.uv)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_1, self.shape)
            .with_inserted_indices(Indices::U32(self.idx))
    }
}

/// Open cylinder along +Y (radius 1, height 1; scale it), uv = (around, up).
fn cylinder(style: u8) -> Mesh {
    cylinder_b(style).build()
}

fn cylinder_b(style: u8) -> Builder {
    let mut b = Builder::new();
    b.grid(32, 8, style, 0.0, |u, w| {
        let a = u * std::f32::consts::TAU;
        let n = Vec3::new(a.cos(), 0.0, a.sin());
        (n + Vec3::Y * w, n)
    });
    b
}

/// Flat annulus on y = 0 from radius `r0` to 1, uv = (around, radial 0..1).
fn annulus(r0: f32, style: u8) -> Mesh {
    annulus_b(r0, style).build()
}

fn annulus_b(r0: f32, style: u8) -> Builder {
    let mut b = Builder::new();
    b.grid(64, 4, style, 0.0, |u, w| {
        let a = u * std::f32::consts::TAU;
        let r = r0 + (1.0 - r0) * w;
        (Vec3::new(a.cos() * r, 0.0, a.sin() * r), Vec3::Y)
    });
    b
}

/// Vertical quad across X (-0.5..0.5) and up Y (0..1), facing Z.
fn curtain(style: u8) -> Mesh {
    curtain_b(style).build()
}

fn curtain_b(style: u8) -> Builder {
    let mut b = Builder::new();
    b.grid(16, 8, style, 0.0, |u, w| (Vec3::new(u - 0.5, w, 0.0), Vec3::Z));
    b
}

/// A laser ribbon from the emitter (origin) up +Y, unit length, `width(w)` across X; uv = (across, up). marker.wgsl
/// turns it to face the camera about its (swinging) axis, from the vertex's offset to the emitter (the merged mesh's
/// normal, see [`Piece::offset_normal`]), so there are no crossed-plane seams. Only u = 0 / 1 columns: every vertex is
/// on an edge.
fn ribbon_b(style: u8, rows: u32, width: impl Fn(f32) -> f32) -> Builder {
    let mut b = Builder::new();
    b.grid(1, rows, style, 0.0, |u, w| (Vec3::new((u - 0.5) * width(w), w, 0.0), Vec3::Z));
    b
}

/// Two unindexed vertices (never drawn) spanning the unit box -1..1 x 0..1 x -1..1: widens the merged mesh's Aabb to
/// where the swinging beams reach.
fn bounds_b() -> Builder {
    let mut b = Builder::new();
    b.v(Vec3::new(-1.0, 0.0, -1.0), Vec3::Y, Vec2::ZERO, LASER, 0.0);
    b.v(Vec3::new(1.0, 1.0, 1.0), Vec3::Y, Vec2::ZERO, LASER, 0.0);
    b
}

/// Floor quad (X -0.5..0.5, forward = -Z 0..1); uv.y grows forwards.
fn floor_quad(style: u8, phase: f32) -> Mesh {
    floor_quad_b(style, phase).build()
}

fn floor_quad_b(style: u8, phase: f32) -> Builder {
    let mut b = Builder::new();
    b.grid(2, 2, style, phase, |u, w| (Vec3::new(u - 0.5, 0.0, -w), Vec3::Y));
    b
}

#[derive(Resource, Default)]
struct Visuals {
    meshes: HashMap<&'static str, Handle<Mesh>>,
    chevrons: Vec<Handle<Mesh>>,
    materials: HashMap<String, Handle<MarkerMaterial>>,
    /// Event marker roots by race index, and the key of their look (re-materialised on change).
    markers: Vec<(usize, Entity, String)>,
    markers_for: Option<usize>,
    /// Gate visuals: (race, gates_done) and the root.
    gates: Option<((usize, u32), Entity)>,
}

fn build_meshes(mut v: ResMut<Visuals>, mut meshes: ResMut<Assets<Mesh>>, merged: Option<ResMut<Merged>>) {
    if let Some(mut m) = merged {
        // Merged path: CPU templates only (indexed by the T_* constants), baked into the merged mesh.
        m.templates = vec![cylinder_b(BEAM), cylinder_b(BAR), annulus_b(0.7, RING), annulus_b(0.05, RIPPLE), curtain_b(CURTAIN), curtain_b(FINISH)];
        m.templates.extend((0..5).map(|k| floor_quad_b(CHEVRON, k as f32)));
        m.templates.extend([
            ribbon_b(LASER, 1, |_| 1.0),
            ribbon_b(GLOW, 8, |w| 0.08 + 0.92 * w),
            annulus_b(0.0, EMITTER),
            ribbon_b(HAZE, 16, |w| (1.0 + (HAZE_W - 1.0) * w.sqrt()) / HAZE_W),
            bounds_b(),
        ]);
        return;
    }
    v.meshes.insert("beam", meshes.add(cylinder(BEAM)));
    v.meshes.insert("bar", meshes.add(cylinder(BAR)));
    v.meshes.insert("ring", meshes.add(annulus(0.7, RING)));
    v.meshes.insert("ripple", meshes.add(annulus(0.05, RIPPLE)));
    v.meshes.insert("curtain", meshes.add(curtain(CURTAIN)));
    v.meshes.insert("finish", meshes.add(curtain(FINISH)));
    v.chevrons = (0..5).map(|k| meshes.add(floor_quad(CHEVRON, k as f32))).collect();
}

/// A cached material: colour (linear HDR), intensity, fades (near0, near1, far0, far1), emphasis.
fn material(v: &mut Visuals, mats: &mut Assets<MarkerMaterial>, c: LinearRgba, intensity: f32, fade: [f32; 4], emphasis: f32) -> Handle<MarkerMaterial> {
    let key = format!("{:.3},{:.3},{:.3},{intensity:.3},{:?},{emphasis:.2}", c.red, c.green, c.blue, fade);
    v.materials
        .entry(key)
        .or_insert_with(|| {
            mats.add(MarkerMaterial {
                params: MarkerParams { colour: Vec4::new(c.red, c.green, c.blue, intensity), fade: Vec4::from_array(fade), k: Vec4::new(PULSE_HZ, 0.0, emphasis, 0.0) },
            })
        })
        .clone()
}

fn mesh(v: &Visuals, name: &str) -> Handle<Mesh> {
    v.meshes.get(name).cloned().unwrap_or_default()
}

fn ground(track: &crate::track::Track, p: Vec3) -> Vec3 {
    track.ground.ray(p + Vec3::Y * 10.0, Vec3::NEG_Y, 40.0).map_or(p, |h| h.point)
}

/// Yaw that turns local -Z onto `forward` (x, z).
fn yaw_of(forward: Vec2) -> f32 {
    (-forward.x).atan2(-forward.y)
}

fn piece(commands: &mut Commands, parent: Entity, m: Handle<Mesh>, mat: Handle<MarkerMaterial>, t: Transform) {
    commands.spawn((Mesh3d(m), MeshMaterial3d(mat), t, Visibility::Inherited, NotShadowCaster, NotShadowReceiver, ChildOf(parent)));
}

// ---------------------------------------------------------------- event start markers

/// The look of one event marker: (key, colour, intensity, emphasis, ripple, beam height).
fn marker_look(state: Option<EventState>, tier: u8, street: bool, recommended: bool, prompt: bool) -> (String, LinearRgba, f32, f32, bool, f32) {
    let base = if street { street_blue() } else { crate::progression::tier_color(tier).to_linear() * 2.0 };
    let height = if street { 60.0 } else { 90.0 };
    match state {
        _ if prompt => ("prompt".into(), LinearRgba::rgb(2.0, 1.7, 0.4), 1.2, 0.6, true, height),
        Some(EventState::Locked) => ("locked".into(), LinearRgba::rgb(0.55, 0.55, 0.6), 0.4, 0.0, false, height * 0.6),
        Some(EventState::Completed { .. }) => (format!("done{tier}{street}"), base, 0.5, 0.0, false, height * 0.8),
        _ if recommended => (format!("rec{tier}{street}"), base, 1.25, 1.0, true, height * 1.4),
        _ => (format!("open{tier}{street}"), base, 0.9, 0.0, true, height),
    }
}

#[allow(clippy::too_many_arguments)]
fn event_markers(
    mut commands: Commands,
    mut v: ResMut<Visuals>,
    mut mats: ResMut<Assets<MarkerMaterial>>,
    events: Res<Events>,
    rs: Res<RaceState>,
    cat: Option<Res<EventCatalog>>,
    states: Option<Res<super::states::MarkerStates>>,
    track: Res<crate::track::Track>,
    cars: Query<&Car>,
    mut vis: Query<&mut Visibility>,
) {
    let on = super::races_on() && track.id == "colorado" && !events.races.is_empty();
    // (Re)build on a new event list (map switch / reinstall).
    let ptr = std::ptr::from_ref(&*events) as usize ^ events.races.len();
    if events.is_changed() || v.markers_for != Some(ptr) || !on {
        for (_, e, _) in std::mem::take(&mut v.markers) {
            commands.entity(e).try_despawn();
        }
        v.markers_for = None;
        if !on || v.meshes.is_empty() {
            return;
        }
        v.markers_for = Some(ptr);
        for (i, r) in events.races.iter().enumerate() {
            let p = ground(&track, r.marker.0);
            let root = commands.spawn((Transform::from_translation(p), Visibility::Hidden, crate::ui::world_load::WorldEntity)).id();
            v.markers.push((i, root, String::new()));
        }
    }
    let Some(pos) = cars.iter().next().map(|c| c.0.position) else { return };
    let idle = rs.phase == RacePhase::Idle;
    let mut markers = std::mem::take(&mut v.markers);
    for (i, root, key) in markers.iter_mut() {
        let r = &events.races[*i];
        let near = r.marker.0.distance(pos) < MARKER_SHOW_M;
        let emits = super::states::state_visible(cat.as_ref().and_then(|c| c.events.get(*i)).filter(|e| e.race == *i).map(|e| e.state))
            && super::states::icon_visible(states.as_ref().and_then(|s| s.get(*i)));
        let want = if idle && near && emits { Visibility::Inherited } else { Visibility::Hidden };
        if let Ok(mut vv) = vis.get_mut(*root) {
            if *vv != want {
                *vv = want;
            }
        }
        if want == Visibility::Hidden {
            continue;
        }
        let info = cat.as_ref().and_then(|c| c.events.get(*i)).filter(|e| e.race == *i);
        let street = r.kind.starts_with("Street") || r.hub > 0;
        let (k, colour, intensity, emph, ripple, height) =
            super::states::restyle(
                marker_look(info.map(|e| e.state), info.map_or(crate::progression::event_tier(r), |e| e.tier), street, info.is_some_and(|e| e.recommended), rs.prompt == Some(*i)),
                states.as_ref().and_then(|s| s.get(*i)),
                rs.prompt == Some(*i),
            );
        if *key == k {
            continue;
        }
        // Rebuild this marker's pieces in the new look.
        *key = k;
        commands.entity(*root).despawn_related::<Children>();
        let beam = material(&mut v, &mut mats, colour, intensity, [10.0, 40.0, 1800.0, 2500.0], emph);
        let halo = material(&mut v, &mut mats, colour, intensity * 0.22, [10.0, 40.0, 1500.0, 2200.0], emph);
        let ring = material(&mut v, &mut mats, colour, intensity * 0.9, [2.0, 6.0, 300.0, 500.0], emph);
        let ripple_m = material(&mut v, &mut mats, colour, intensity * 0.6, [2.0, 6.0, 250.0, 400.0], 0.0);
        let (bm, rm, pm) = (mesh(&v, "beam"), mesh(&v, "ring"), mesh(&v, "ripple"));
        piece(&mut commands, *root, bm.clone(), beam, Transform::from_scale(Vec3::new(1.4, height, 1.4)));
        piece(&mut commands, *root, bm, halo, Transform::from_scale(Vec3::new(4.0, height * 0.7, 4.0)));
        piece(&mut commands, *root, rm, ring, Transform::from_xyz(0.0, 0.15, 0.0).with_scale(Vec3::splat(10.0)));
        if ripple {
            // Over the game's 25 m TriggerZone.
            piece(&mut commands, *root, pm, ripple_m, Transform::from_xyz(0.0, 0.12, 0.0).with_scale(Vec3::splat(super::MARKER_RADIUS)));
        }
    }
    v.markers = markers;
}

// ---------------------------------------------------------------- checkpoint gates

/// Ripple left at a gate the player just passed (seconds left).
#[derive(Component)]
struct PassFlash(f32);

#[allow(clippy::too_many_arguments)]
fn gate_visuals(
    mut commands: Commands,
    mut v: ResMut<Visuals>,
    mut mats: ResMut<Assets<MarkerMaterial>>,
    events: Res<Events>,
    rs: Res<RaceState>,
    track: Res<crate::track::Track>,
) {
    let key = match (rs.phase, rs.race, rs.racers.first()) {
        (RacePhase::Idle | RacePhase::Results, _, _) | (_, None, _) | (_, _, None) => None,
        (_, Some(i), Some(p)) if p.finished_s.is_none() => Some((i, p.gates_done)),
        _ => None,
    };
    if v.gates.map(|g| g.0) == key {
        return;
    }
    let old = v.gates.take();
    if let Some(((i, done), root)) = old {
        commands.entity(root).try_despawn();
        // Passed a gate: a ripple where it stood.
        if key.is_some_and(|k| k.0 == i && k.1 == done + 1) {
            if let Some(def) = events.races.get(i) {
                let g = gate(def, done);
                let m = material(&mut v, &mut mats, pink(), 1.2, [1.0, 4.0, 400.0, 600.0], 0.0);
                commands.spawn((
                    Mesh3d(mesh(&v, "ripple")),
                    MeshMaterial3d(m),
                    Transform::from_translation(ground(&track, g.centre) + Vec3::Y * 0.2).with_scale(Vec3::splat(g.half_width.clamp(6.0, 18.0))),
                    Visibility::Inherited,
                    NotShadowCaster,
                    NotShadowReceiver,
                    PassFlash(0.9),
                    crate::ui::world_load::WorldEntity,
                ));
            }
        }
    }
    let Some((i, done)) = key else { return };
    let Some(def) = events.races.get(i) else { return };
    let total = total_gates(def);
    let root = commands.spawn((Transform::IDENTITY, Visibility::Inherited, crate::ui::world_load::WorldEntity)).id();
    v.gates = Some(((i, done), root));
    for k in 0..2u32 {
        let n = done + k;
        if n >= total {
            break;
        }
        let g = *gate(def, n);
        let last = n + 1 == total;
        let strength = if k == 0 { 1.0 } else { 0.35 };
        let colour = if last { LinearRgba::rgb(1.8, 1.8, 1.8) } else { pink() };
        let hw = g.half_width.clamp(6.0, 18.0);
        let base = ground(&track, g.centre);
        let t = Transform::from_translation(base).with_rotation(Quat::from_rotation_y(yaw_of(g.forward)));
        let gate_root = commands.spawn((t, Visibility::Inherited, ChildOf(root))).id();
        let height = 7.5;
        let posts = material(&mut v, &mut mats, colour, 1.1 * strength, [3.0, 10.0, 1100.0, 1500.0], 0.0);
        let veil = material(&mut v, &mut mats, if last { pink() } else { colour }, 0.9 * strength, [3.0, 12.0, 900.0, 1300.0], if k == 0 { 0.5 } else { 0.0 });
        let (bar, cur) = (mesh(&v, "bar"), mesh(&v, if last { "finish" } else { "curtain" }));
        for s in [-1.0, 1.0] {
            piece(&mut commands, gate_root, bar.clone(), posts.clone(), Transform::from_xyz(s * hw, 0.0, 0.0).with_scale(Vec3::new(0.28, height, 0.28)));
        }
        // Top bar: the unit cylinder laid along X.
        piece(
            &mut commands,
            gate_root,
            bar,
            posts,
            Transform::from_xyz(-hw, height, 0.0).with_rotation(Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2)).with_scale(Vec3::new(0.22, hw * 2.0, 0.22)),
        );
        piece(&mut commands, gate_root, cur, veil, Transform::from_scale(Vec3::new(hw * 2.0, height, 1.0)));
        if k == 0 {
            // Floor chevrons leading into the gate (the pulse runs towards it), each on the ground.
            let chev = material(&mut v, &mut mats, colour, 1.0, [2.0, 6.0, 70.0, 140.0], 0.0);
            let fwd = Vec3::new(g.forward.x, 0.0, g.forward.y);
            for (j, h) in v.chevrons.clone().into_iter().enumerate() {
                let back = 6.0 + (4 - j) as f32 * 6.0;
                let p = ground(&track, g.centre - fwd * back) + Vec3::Y * 0.12;
                let rot = Quat::from_rotation_y(yaw_of(g.forward));
                commands.spawn((
                    Mesh3d(h),
                    MeshMaterial3d(chev.clone()),
                    Transform::from_translation(p).with_rotation(rot).with_scale(Vec3::new(4.0, 1.0, 3.0)),
                    Visibility::Inherited,
                    NotShadowCaster,
                    NotShadowReceiver,
                    ChildOf(root),
                ));
            }
        }
    }
}

fn pass_flash(mut commands: Commands, time: Res<Time>, mut q: Query<(Entity, &mut PassFlash, &mut Transform)>) {
    let dt = time.delta_secs();
    for (e, mut f, mut t) in &mut q {
        f.0 -= dt;
        t.scale *= 1.0 + dt * 1.5;
        if f.0 <= 0.0 {
            commands.entity(e).try_despawn();
        }
    }
}

// ---------------------------------------------------------------- start / finish cannons

/// FH1's race cannons from the route files (events-3): at GO the start gantry's spark cannon, when the player finishes
/// the two rows of finish cannons, sparks and confetti, fired pairwise 0.1 s apart at 45 degrees (GlobalRegistry
/// `Race/FX/EndRaceCannons` TimeBetweenCannons 0.1, AngleOfCannons 45), three volleys. effects.zip XML through
/// fh1-render particles. `FH1_RACE_CANNONS=0` off.
#[derive(Default)]
struct CannonState {
    /// (fire at, position, axis, finish volley).
    queue: Vec<(f32, Vec3, Vec3, bool)>,
    finished: bool,
    racing: bool,
}

fn cannons(mut st: Local<CannonState>, rs: Res<RaceState>, events: Res<Events>, time: Res<Time>, particles: Option<ResMut<fh1_render::particles::FxParticles>>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("FH1_RACE_CANNONS").map_or(true, |v| v != "0")) {
        return;
    }
    let now = time.elapsed_secs();
    let def = rs.race.and_then(|i| events.races.get(i));
    let racing = rs.phase == RacePhase::Racing;
    let finished = rs.racers.first().is_some_and(|p| p.finished_s.is_some());
    let axis = |f: Vec2| Vec3::new(f.x, 0.0, f.y).normalize_or(Vec3::Z) + Vec3::Y;
    if let Some(def) = def {
        if racing && !st.racing && rs.clock_s < 0.5 {
            if let Some((p, f)) = def.start_cannon {
                for k in 0..2 {
                    st.queue.push((now + k as f32 * 0.35, p, axis(f).normalize(), false));
                }
            }
        }
        if finished && !st.finished {
            for volley in 0..3 {
                for (row, list) in def.cannons.iter().enumerate() {
                    for (k, (p, f)) in list.iter().enumerate() {
                        let at = now + volley as f32 * 0.6 + k as f32 * 0.1 + row as f32 * 0.02;
                        st.queue.push((at, *p, axis(*f).normalize(), true));
                    }
                }
            }
        }
    }
    st.racing = racing;
    st.finished = finished;
    if rs.race.is_none() {
        st.queue.clear();
    }
    let Some(mut fx) = particles else { return };
    let due: Vec<_> = st.queue.iter().filter(|q| q.0 <= now).copied().collect();
    st.queue.retain(|q| q.0 > now);
    for (_, pos, ax, finish) in due {
        let names: &[(&str, u32)] = if finish {
            &[("AMB_SparkCannon_Flare", 20), ("AMB_SparkCannon_Sparks", 60), ("AMB_Confetti_Canon_Burst", 80), ("AMB_Confetti_Smoke", 12)]
        } else {
            &[("AMB_SparkCannon_Flare", 20), ("AMB_SparkCannon_Sparks", 60), ("AMB_StartSparkCannon_Smoke", 20)]
        };
        for (name, n) in names {
            if let Some(id) = fx.effect(name) {
                let mut s = fh1_render::particles::Spawn::new(pos + Vec3::Y * 0.5, ax, *n);
                s.ground_y = pos.y;
                fx.spawn(id, &s);
            }
        }
    }
}

// ---------------------------------------------------------------- merged draw (FH1_MARKER_MERGE, default on)

/// Template indices into [`Merged::templates`] (built in [`build_meshes`]); chevron k = `T_CHEVRON + k`.
const T_BEAM: usize = 0;
const T_BAR: usize = 1;
const T_RING: usize = 2;
const T_RIPPLE: usize = 3;
const T_CURTAIN: usize = 4;
const T_FINISH: usize = 5;
const T_CHEVRON: usize = 6;
const T_LASER: usize = 11;
const T_GLOW: usize = 12;
const T_EMITTER: usize = 13;
const T_HAZE: usize = 14;
const T_BOUNDS: usize = 15;

/// Marks the one merged marker entity.
#[derive(Component)]
struct MergedMarkers;

/// One piece of the merged mesh: a template placed in the world with its material parameters (the per-entity
/// path's `material(..)` arguments).
#[derive(Clone, Copy)]
struct Piece {
    tpl: usize,
    xf: Affine3A,
    colour: LinearRgba,
    intensity: f32,
    fade: [f32; 4],
    emphasis: f32,
    /// Pass ripple: `Some(birth)` (wrapped seconds); `xf` is then the ripple at its final scale. Laser pieces: the
    /// time their beams start retracting (the passed gate's `_Off`).
    flash: Option<f32>,
    /// Vertex attribute k.x: the pulse rate, or for the laser styles the beam index (0..2) + 4 for the finish colours.
    kx: f32,
    /// The normal attribute carries each vertex's offset from the piece's origin (camera-facing laser ribbons).
    offset_normal: bool,
}

impl Piece {
    fn new(tpl: usize, xf: Affine3A, colour: LinearRgba, intensity: f32, fade: [f32; 4], emphasis: f32) -> Self {
        Self { tpl, xf, colour, intensity, fade, emphasis, flash: None, kx: PULSE_HZ, offset_normal: false }
    }

    /// A laser piece (styles 7-10): no pulse, `kx` as above.
    fn laser(tpl: usize, xf: Affine3A, colour: LinearRgba, intensity: f32, fade: [f32; 4], kx: f32) -> Self {
        Self { kx, offset_normal: true, ..Self::new(tpl, xf, colour, intensity, fade, 0.0) }
    }
}

/// Something left behind at a gate just passed: the pink ripple (`FH1_RACE_LASERS=0`) or the passed laser retracting.
#[derive(Clone)]
struct Flash {
    /// The pieces, `flash` set to `birth`.
    pieces: Vec<Piece>,
    /// Birth on the wrapped clock (`globals.time` in the shader).
    birth: f32,
    /// End on the elapsed clock.
    until: f32,
}

#[derive(Resource, Default)]
struct Merged {
    templates: Vec<Builder>,
    entity: Option<Entity>,
    mesh: Handle<Mesh>,
    material: Option<Handle<MarkerMaterial>>,
    /// Ground point under each event marker (by race index) and the event list they were found for.
    grounds: Vec<Vec3>,
    grounds_for: Option<usize>,
    /// Gate progress (race, gates_done) the gate pieces are for, and those pieces.
    gates_key: Option<(usize, u32)>,
    gate_pieces: Vec<Piece>,
    flashes: Vec<Flash>,
    /// Signature of the pieces in the mesh now (0 = nothing built).
    sig: u64,
    /// Padded capacities of the mesh now (vertices, triangles).
    vcap: usize,
    tcap: usize,
}

/// Power-of-two capacity for `n` items given the current capacity `cur` (from `min`; grown at once, shrunk only below
/// a quarter), like fh1-render particles `quad_capacity`.
fn capacity(n: usize, cur: usize, min: usize) -> usize {
    let want = n.max(1).next_power_of_two().max(min);
    if want > cur || want.saturating_mul(4) <= cur {
        want
    } else {
        cur
    }
}

fn tf(t: Transform) -> Affine3A {
    t.compute_affine()
}

/// The pieces of the gates for (race, gates_done): the same layout and parameters as [`gate_visuals`], in world space.
fn gate_pieces(def: &super::RaceDef, done: u32, track: &crate::track::Track) -> Vec<Piece> {
    if lasers_on() {
        return laser_gate(def, done, track);
    }
    let mut out = Vec::new();
    let total = total_gates(def);
    for k in 0..2u32 {
        let n = done + k;
        if n >= total {
            break;
        }
        let g = *gate(def, n);
        let last = n + 1 == total;
        let strength = if k == 0 { 1.0 } else { 0.35 };
        let colour = if last { LinearRgba::rgb(1.8, 1.8, 1.8) } else { pink() };
        let hw = g.half_width.clamp(6.0, 18.0);
        let base = ground(track, g.centre);
        let root = tf(Transform::from_translation(base).with_rotation(Quat::from_rotation_y(yaw_of(g.forward))));
        let height = 7.5;
        let posts_i = 1.1 * strength;
        let posts_f = [3.0, 10.0, 1100.0, 1500.0];
        let veil_c = if last { pink() } else { colour };
        let veil_e = if k == 0 { 0.5 } else { 0.0 };
        for s in [-1.0, 1.0] {
            let t = Transform::from_xyz(s * hw, 0.0, 0.0).with_scale(Vec3::new(0.28, height, 0.28));
            out.push(Piece::new(T_BAR, root * tf(t), colour, posts_i, posts_f, 0.0));
        }
        // Top bar: the unit cylinder laid along X.
        let top = Transform::from_xyz(-hw, height, 0.0).with_rotation(Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2)).with_scale(Vec3::new(0.22, hw * 2.0, 0.22));
        out.push(Piece::new(T_BAR, root * tf(top), colour, posts_i, posts_f, 0.0));
        let cur = if last { T_FINISH } else { T_CURTAIN };
        let veil = Transform::from_scale(Vec3::new(hw * 2.0, height, 1.0));
        out.push(Piece::new(cur, root * tf(veil), veil_c, 0.9 * strength, [3.0, 12.0, 900.0, 1300.0], veil_e));
        if k == 0 {
            // Floor chevrons leading into the gate, each on the ground.
            let fwd = Vec3::new(g.forward.x, 0.0, g.forward.y);
            let rot = Quat::from_rotation_y(yaw_of(g.forward));
            for j in 0..5usize {
                let back = 6.0 + (4 - j) as f32 * 6.0;
                let p = ground(track, g.centre - fwd * back) + Vec3::Y * 0.12;
                let t = Transform::from_translation(p).with_rotation(rot).with_scale(Vec3::new(4.0, 1.0, 3.0));
                out.push(Piece::new(T_CHEVRON + j, tf(t), colour, 1.0, [2.0, 6.0, 70.0, 140.0], 0.0));
            }
        }
    }
    out
}

/// Whether the race's gates are the game's street `route_checkpoint_NN` (laser just past the checkpoint) rather than
/// festival gates on the sparse `route_waypoint_NN` (laser on the road path, see [`laser_gate`]): fh1setup events.rs
/// gives waypoint gates a half width of exactly 30 m, checkpoint gates width / 2 (the widest checkpoint is 50 m).
fn checkpoint_route(def: &super::RaceDef) -> bool {
    let n = def.gates.len();
    n <= 1 || def.gates[..n - 1].iter().any(|g| (g.half_width - 30.0).abs() > 0.01)
}

/// Ground under `p`: looked for just above it first, so a bridge or overhang above the road isn't taken for it.
fn road_ground(track: &crate::track::Track, p: Vec3) -> Option<Vec3> {
    track.ground.ray(p + Vec3::Y * 2.5, Vec3::NEG_Y, 8.0).or_else(|| track.ground.ray(p + Vec3::Y * 10.0, Vec3::NEG_Y, 40.0)).map(|h| h.point)
}

/// The road's edges either side of ground point `centre` along `right` (left, right point), as the finish pair stands:
/// walked out in 0.5 m steps until the surface turns off-road (offroadness changes) or the ground steps (kerb, ditch,
/// wall foot), then 0.3 m back in. A side whose edge isn't found between 3 and 12 m keeps the game's median spacing.
fn road_edges(track: &crate::track::Track, centre: Vec3, right: Vec3) -> [Vec3; 2] {
    let fallback = |s: f32| road_ground(track, centre + right * (s * FINISH_HALF_M)).unwrap_or(centre + right * (s * FINISH_HALF_M));
    let Some(c) = track.ground.ray(centre + Vec3::Y * 2.5, Vec3::NEG_Y, 8.0) else { return [fallback(-1.0), fallback(1.0)] };
    [-1.0f32, 1.0].map(|s| {
        let mut last = c.point;
        let mut edge = None;
        let mut d = 0.5;
        while d <= 12.0 {
            let q = Vec3::new(c.point.x, last.y, c.point.z) + right * (s * d);
            match track.ground.ray(q + Vec3::Y * 2.0, Vec3::NEG_Y, 4.0) {
                Some(h) if (h.point.y - last.y).abs() < 0.25 && (h.tyre.offroadness - c.tyre.offroadness).abs() < 0.25 => last = h.point,
                _ => {
                    edge = Some(d - 0.5);
                    break;
                }
            }
            d += 0.5;
        }
        match edge {
            Some(e) if e >= 3.0 => road_ground(track, c.point + right * (s * (e - 0.3))).unwrap_or(last),
            _ => fallback(s),
        }
    })
}

/// The lasers of the race's next gate `done` (module docs): the next checkpoint's single blue laser on the road (a few
/// metres past a street checkpoint, on the road path at a festival waypoint gate), or the green pair across the road
/// at the finish.
fn laser_gate(def: &super::RaceDef, done: u32, track: &crate::track::Track) -> Vec<Piece> {
    let mut out = Vec::new();
    let total = total_gates(def);
    if done >= total {
        return out;
    }
    let g = *gate(def, done);
    let fwd = Vec3::new(g.forward.x, 0.0, g.forward.y).normalize_or(Vec3::NEG_Z);
    let right = Vec3::new(-fwd.z, 0.0, fwd.x);
    if done + 1 == total {
        let centre = road_ground(track, g.centre).unwrap_or(g.centre);
        for p in road_edges(track, centre, right) {
            laser(&mut out, p, true);
        }
    } else if checkpoint_route(def) {
        // A street checkpoint: where its indicator stands, on the road centre line just past the trigger.
        let p = g.centre + fwd * CHECKPOINT_AHEAD_M;
        laser(&mut out, road_ground(track, p).unwrap_or(p), false);
    } else if festival_lasers_on() {
        // A festival waypoint gate: no indicator, and the waypoint itself can sit off the tarmac (sparse, hand placed), so
        // the laser stands where the race's road path (the game's racing line, else the A* road) passes the gate.
        let p = on_path(&def.path, g.centre).unwrap_or(g.centre);
        laser(&mut out, road_ground(track, p).unwrap_or(p), false);
    }
    out
}

/// The nearest point of the road path `path` to `p` (in plan; height interpolated), if within 40 m.
fn on_path(path: &[Vec3], p: Vec3) -> Option<Vec3> {
    let q = Vec2::new(p.x, p.z);
    let mut best: Option<(f32, Vec3)> = None;
    for s in path.windows(2) {
        let (a, b) = (Vec2::new(s[0].x, s[0].z), Vec2::new(s[1].x, s[1].z));
        let ab = b - a;
        let t = if ab.length_squared() < 1e-6 { 0.0 } else { ((q - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) };
        let d = (a + ab * t).distance(q);
        if best.is_none_or(|x| d < x.0) {
            best = Some((d, s[0].lerp(s[1], t)));
        }
    }
    best.filter(|x| x.0 < 40.0).map(|x| x.1)
}

/// One laser object standing at ground point `p` (blue checkpoint or green finish): three beams with their hazes, the
/// blue one's base glow, the emitter plate.
fn laser(out: &mut Vec<Piece>, p: Vec3, finish: bool) {
    let (beam, haze) = (beam_colour(finish), haze_colour(finish));
    let at = Affine3A::from_translation(p);
    let fin = if finish { 4.0 } else { 0.0 };
    for b in 0..3 {
        let kx = b as f32 + fin;
        // A little over the textures' own strength (beam alpha ~0.85; the haze x 1/3 as three overlap at the plate) so the
        // next checkpoint still reads by day, where the game's are faint.
        out.push(Piece::laser(T_LASER, at * tf(Transform::from_scale(Vec3::new(BEAM_W, LASER_LEN, 1.0))), beam, 1.3, [0.6, 3.0, 2600.0, 3800.0], kx));
        out.push(Piece::laser(T_HAZE, at * tf(Transform::from_scale(Vec3::new(HAZE_W, HAZE_LEN, 1.0))), haze, 0.45, [4.0, 16.0, 2000.0, 3200.0], kx));
    }
    if !finish {
        out.push(Piece::laser(T_GLOW, at * tf(Transform::from_scale(Vec3::new(GLOW_W, GLOW_LEN, 1.0))), haze, 0.3, [4.0, 18.0, 1500.0, 2500.0], fin));
    }
    let plate = Transform::from_xyz(0.0, 0.06, 0.0).with_scale(Vec3::splat(PLATE_R));
    out.push(Piece::new(T_EMITTER, at * tf(plate), beam, 1.0, [0.5, 1.5, 150.0, 300.0], 0.0));
    // The beams swing out to ~6 degrees: 600 m x tan(6.5 deg) ~ 70 m.
    let reach = Transform::from_scale(Vec3::new(72.0, LASER_LEN + 20.0, 72.0));
    out.push(Piece::new(T_BOUNDS, at * tf(reach), LinearRgba::NONE, 0.0, [0.0; 4], 0.0));
}

/// The pieces of one event marker standing at ground point `p` (same layout and parameters as [`event_markers`]).
fn marker_pieces(out: &mut Vec<Piece>, p: Vec3, colour: LinearRgba, intensity: f32, emph: f32, ripple: bool, height: f32) {
    let root = Affine3A::from_translation(p);
    let beam = Transform::from_scale(Vec3::new(1.4, height, 1.4));
    let halo = Transform::from_scale(Vec3::new(4.0, height * 0.7, 4.0));
    let ring = Transform::from_xyz(0.0, 0.15, 0.0).with_scale(Vec3::splat(10.0));
    out.push(Piece::new(T_BEAM, root * tf(beam), colour, intensity, [10.0, 40.0, 1800.0, 2500.0], emph));
    out.push(Piece::new(T_BEAM, root * tf(halo), colour, intensity * 0.22, [10.0, 40.0, 1500.0, 2200.0], emph));
    out.push(Piece::new(T_RING, root * tf(ring), colour, intensity * 0.9, [2.0, 6.0, 300.0, 500.0], emph));
    if ripple {
        // Over the game's 25 m TriggerZone.
        let t = Transform::from_xyz(0.0, 0.12, 0.0).with_scale(Vec3::splat(super::MARKER_RADIUS));
        out.push(Piece::new(T_RIPPLE, root * tf(t), colour, intensity * 0.6, [2.0, 6.0, 250.0, 400.0], 0.0));
    }
}

/// Bakes `pieces` (world space) into one mesh in the frame of `anchor`, padded to power-of-two capacities.
fn bake(m: &mut Merged, pieces: &[Piece], anchor: Vec3) -> Mesh {
    let (mut nv, mut nt) = (0usize, 0usize);
    for p in pieces {
        if let Some(t) = m.templates.get(p.tpl) {
            nv += t.pos.len();
            nt += t.idx.len() / 3;
        }
    }
    m.vcap = capacity(nv, m.vcap, 1024);
    m.tcap = capacity(nt, m.tcap, 1024);
    let (vcap, tcap) = (m.vcap, m.tcap);
    let mut pos: Vec<[f32; 3]> = Vec::with_capacity(vcap);
    let mut nrm: Vec<[f32; 3]> = Vec::with_capacity(vcap);
    let mut uv: Vec<[f32; 2]> = Vec::with_capacity(vcap);
    let mut shape: Vec<[f32; 2]> = Vec::with_capacity(vcap);
    let mut colour: Vec<[f32; 4]> = Vec::with_capacity(vcap);
    let mut fade: Vec<[f32; 4]> = Vec::with_capacity(vcap);
    let mut k: Vec<[f32; 4]> = Vec::with_capacity(vcap);
    let mut idx: Vec<u32> = Vec::with_capacity(tcap * 3);
    let to_local = Affine3A::from_translation(-anchor);
    for p in pieces {
        let Some(t) = m.templates.get(p.tpl) else { continue };
        let base = pos.len() as u32;
        let xf = to_local * p.xf;
        let c = [p.colour.red, p.colour.green, p.colour.blue, p.intensity];
        for (vi, v) in t.pos.iter().enumerate() {
            let lp = Vec3::from_array(*v);
            pos.push(xf.transform_point3(lp).into());
            nrm.push(if p.flash.is_some() || p.offset_normal {
                // The vertex's offset from the piece's origin: the ripple's at its final scale (the shader pulls it in by
                // age), a laser ribbon's from its emitter (the shader turns it to the camera).
                p.xf.transform_vector3(lp).into()
            } else {
                // The old vertex shader's `normalize(m * n)` (the model matrix, as before).
                p.xf.transform_vector3(Vec3::from_array(t.nrm[vi])).normalize_or_zero().into()
            });
            uv.push(t.uv[vi]);
            shape.push(t.shape[vi]);
            colour.push(c);
            fade.push(p.fade);
            k.push([p.kx, lp.y, p.emphasis, p.flash.unwrap_or(-1.0)]);
        }
        idx.extend(t.idx.iter().map(|i| base + i));
    }
    // Padding: unused vertices on the first vertex (keeps the auto Aabb tight), degenerate (0, 0, 0) triangles.
    let first = pos.first().copied().unwrap_or([0.0; 3]);
    pos.resize(vcap, first);
    nrm.resize(vcap, [0.0, 1.0, 0.0]);
    uv.resize(vcap, [0.0; 2]);
    shape.resize(vcap, [0.0; 2]);
    colour.resize(vcap, [0.0; 4]);
    fade.resize(vcap, [0.0; 4]);
    k.resize(vcap, [0.0, 0.0, 0.0, -1.0]);
    idx.resize(tcap * 3, 0);
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

/// Every race marker piece in one mesh / one draw (see the module docs). Same rules as [`event_markers`],
/// [`gate_visuals`] and [`pass_flash`]; the mesh is rebuilt only when the signature of the pieces changes.
#[allow(clippy::too_many_arguments)]
fn merged_markers(
    mut commands: Commands,
    mut m: ResMut<Merged>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<MarkerMaterial>>,
    events: Res<Events>,
    rs: Res<RaceState>,
    cat: Option<Res<EventCatalog>>,
    states: Option<Res<super::states::MarkerStates>>,
    track: Res<crate::track::Track>,
    time: Res<Time>,
    cars: Query<&Car>,
    mut ent: Query<(&mut Visibility, &mut Transform), With<MergedMarkers>>,
) {
    if m.templates.is_empty() {
        return;
    }
    let now = time.elapsed_secs();
    let mut sig = std::hash::DefaultHasher::new();

    // Event start markers: shown when idle and within MARKER_SHOW_M of the player.
    let on = super::races_on() && track.id == "colorado" && !events.races.is_empty();
    let ptr = std::ptr::from_ref(&*events) as usize ^ events.races.len();
    if !on {
        m.grounds.clear();
        m.grounds_for = None;
    } else if events.is_changed() || m.grounds_for != Some(ptr) {
        m.grounds = events.races.iter().map(|r| ground(&track, r.marker.0)).collect();
        m.grounds_for = Some(ptr);
    }
    let car = cars.iter().next().map(|c| c.0.position);
    let mut shown: Vec<(usize, LinearRgba, f32, f32, bool, f32)> = Vec::new();
    if let (true, Some(pos), RacePhase::Idle) = (on, car, rs.phase) {
        for (i, r) in events.races.iter().enumerate() {
            if i >= m.grounds.len() || r.marker.0.distance(pos) >= MARKER_SHOW_M {
                continue;
            }
            let info = cat.as_ref().and_then(|c| c.events.get(i)).filter(|e| e.race == i);
            if !super::states::state_visible(info.map(|e| e.state)) || !super::states::icon_visible(states.as_ref().and_then(|s| s.get(i))) {
                continue;
            }
            let street = r.kind.starts_with("Street") || r.hub > 0;
            let (key, colour, intensity, emph, ripple, height) =
                super::states::restyle(
                marker_look(info.map(|e| e.state), info.map_or(crate::progression::event_tier(r), |e| e.tier), street, info.is_some_and(|e| e.recommended), rs.prompt == Some(i)),
                states.as_ref().and_then(|s| s.get(i)),
                rs.prompt == Some(i),
            );
            (i, key).hash(&mut sig);
            m.grounds[i].to_array().map(f32::to_bits).hash(&mut sig);
            shown.push((i, colour, intensity, emph, ripple, height));
        }
    }

    // Checkpoint gates (lasers: the next one; old look: current + next) and what is left at a gate just passed.
    let key = match (rs.phase, rs.race, rs.racers.first()) {
        (RacePhase::Idle | RacePhase::Results, _, _) | (_, None, _) | (_, _, None) => None,
        (_, Some(i), Some(p)) if p.finished_s.is_none() => Some((i, p.gates_done)),
        _ => None,
    };
    if m.gates_key != key {
        if let Some((i, done)) = m.gates_key {
            // Passed: the next gate of the same race, or the finish (the gates go away with the finish).
            let passed = match key {
                Some(k) => k == (i, done + 1),
                None => rs.race == Some(i) && rs.racers.first().is_some_and(|p| p.finished_s.is_some() && p.gates_done == done + 1),
            };
            if let (true, Some(def)) = (passed, events.races.get(i)) {
                let birth = time.elapsed_secs_wrapped();
                let (mut pieces, life) = if lasers_on() {
                    // The passed laser switches off: its beams retract into the plate (the plate goes with them).
                    (std::mem::take(&mut m.gate_pieces), LASER_OFF_S)
                } else {
                    let g = gate(def, done);
                    let centre = ground(&track, g.centre) + Vec3::Y * 0.2;
                    let xf = Affine3A::from_scale_rotation_translation(Vec3::splat(g.half_width.clamp(6.0, 18.0) * (FLASH_GROW * FLASH_LIFE).exp()), Quat::IDENTITY, centre);
                    (vec![Piece::new(T_RIPPLE, xf, pink(), 1.2, [1.0, 4.0, 400.0, 600.0], 0.0)], FLASH_LIFE)
                };
                pieces.retain(|p| p.tpl != T_EMITTER);
                for p in &mut pieces {
                    p.flash = Some(birth);
                }
                if !pieces.is_empty() {
                    m.flashes.push(Flash { pieces, birth, until: now + life + 0.05 });
                }
            }
        }
        m.gates_key = key;
        m.gate_pieces = key.and_then(|(i, done)| events.races.get(i).map(|def| gate_pieces(def, done, &track))).unwrap_or_default();
    }
    m.flashes.retain(|f| f.until > now);
    key.hash(&mut sig);
    for f in &m.flashes {
        f.birth.to_bits().hash(&mut sig);
    }
    let sig = sig.finish() | 1;
    if m.entity.is_some_and(|e| ent.get(e).is_err()) {
        // Despawned elsewhere (it is spawned by commands in an earlier frame, so it exists by now): respawn.
        m.entity = None;
        m.sig = 0;
    }
    if sig == m.sig {
        return;
    }
    m.sig = sig;

    // Rebuild.
    let mut pieces = m.gate_pieces.clone();
    for &(i, colour, intensity, emph, ripple, height) in &shown {
        marker_pieces(&mut pieces, m.grounds[i], colour, intensity, emph, ripple, height);
    }
    for f in &m.flashes {
        pieces.extend_from_slice(&f.pieces);
    }
    let live = m.entity.and_then(|e| ent.get_mut(e).ok());
    if pieces.is_empty() {
        if let Some((mut vis, _)) = live {
            vis.set_if_neq(Visibility::Hidden);
        }
        return;
    }
    // The entity's translation is what transparent items sort by: the next gate, else the marker nearest the player.
    let at = car.unwrap_or(Vec3::ZERO);
    let anchor = m.gate_pieces.first().map(|p| Vec3::from(p.xf.translation)).unwrap_or_else(|| {
        pieces.iter().map(|p| Vec3::from(p.xf.translation)).min_by(|a, b| a.distance_squared(at).total_cmp(&b.distance_squared(at))).unwrap_or(Vec3::ZERO)
    });
    let mesh = bake(&mut m, &pieces, anchor);
    match live {
        Some((mut vis, mut t)) => {
            // Same handle, new asset: the entity keeps its draw; Bevy re-uploads and recomputes the Aabb
            // (calculate_bounds on AssetChanged<Mesh3d>).
            let _ = meshes.insert(&m.mesh, mesh);
            vis.set_if_neq(Visibility::Inherited);
            if t.translation != anchor {
                t.translation = anchor;
            }
        }
        None => {
            // Uniform = 1 (a global multiplier in the merged shader path); per-piece values are vertex attributes.
            let material = m.material.get_or_insert_with(|| mats.add(MarkerMaterial { params: MarkerParams { colour: Vec4::ONE, fade: Vec4::ZERO, k: Vec4::ZERO } })).clone();
            m.mesh = meshes.add(mesh);
            let e = commands
                .spawn((
                    Mesh3d(m.mesh.clone()),
                    MeshMaterial3d(material),
                    Transform::from_translation(anchor),
                    Visibility::Inherited,
                    NotShadowCaster,
                    NotShadowReceiver,
                    MergedMarkers,
                    Name::new("FH1 race markers (merged)"),
                ))
                .id();
            m.entity = Some(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fh1_engine::vehicle::{Ground, GroundHit, SphereContact, TyreSurface};

    /// Flat ground at y = 0: asphalt for x in `road`, grass (offroadness 1) outside, raised by `kerb` m outside.
    struct Road {
        road: (f32, f32),
        kerb: f32,
    }

    impl Ground for Road {
        fn ray(&self, o: Vec3, d: Vec3, max: f32) -> Option<GroundHit> {
            let off = o.x < self.road.0 || o.x > self.road.1;
            let y = if off { self.kerb } else { 0.0 };
            if d.y >= -1e-6 || o.y < y {
                return None;
            }
            let t = (o.y - y) / -d.y;
            let tyre = TyreSurface { offroadness: if off && self.kerb == 0.0 { 1.0 } else { 0.0 }, ..Default::default() };
            (t <= max).then(|| GroundHit { distance: t, point: o + d * t, normal: Vec3::Y, tyre, surface: 0 })
        }
        fn sphere(&self, _: Vec3, _: f32, out: &mut Vec<SphereContact>) {
            out.clear();
        }
    }

    fn track(road: (f32, f32), kerb: f32) -> crate::track::Track {
        crate::track::Track { ground: std::sync::Arc::new(Road { road, kerb }), ..crate::track::Track::flat() }
    }

    #[test]
    fn finish_pair_stands_at_the_road_edges() {
        // Grass beyond -4.5 / +5.5: the pair 0.3 m inside each edge.
        let [l, r] = road_edges(&track((-4.5, 5.5), 0.0), Vec3::ZERO, Vec3::X);
        assert!((l.x + 4.2).abs() < 0.01 && (r.x - 5.2).abs() < 0.01, "{l} {r}");
        // A kerb (0.3 m step) at +-6 m.
        let [l, r] = road_edges(&track((-6.0, 6.0), 0.3), Vec3::ZERO, Vec3::X);
        assert!((l.x + 5.7).abs() < 0.01 && (r.x - 5.7).abs() < 0.01 && l.y.abs() < 0.01, "{l} {r}");
        // No edge within 12 m (a plaza): the game's median spacing.
        let [l, r] = road_edges(&track((-100.0, 100.0), 0.0), Vec3::ZERO, Vec3::X);
        assert!((l.x + FINISH_HALF_M).abs() < 0.01 && (r.x - FINISH_HALF_M).abs() < 0.01, "{l} {r}");
    }

    #[test]
    fn every_race_type_lights_its_next_checkpoint() {
        let path = [(0.0, 0.0), (300.0, 0.0)];
        let at = |def: &super::super::RaceDef, done: u32| -> Vec<Vec3> {
            laser_gate(def, done, &crate::track::Track::flat()).iter().filter(|p| p.tpl == T_LASER).map(|p| Vec3::from(p.xf.translation)).collect()
        };
        // Festival (waypoint gates, half width 30): one laser where the road path passes the off-road waypoint.
        let fest = crate::race::tests::test_def(&[(100.0, 12.0), (200.0, 0.0)], 30.0, &path, 1, false);
        assert!(!checkpoint_route(&fest));
        let v = at(&fest, 0);
        assert!(v.len() == 3 && v.iter().all(|p| p.distance(Vec3::new(100.0, 0.0, 0.0)) < 1e-3), "{v:?}");
        // Street (checkpoint gates): CHECKPOINT_AHEAD_M past the checkpoint along its facing (+X).
        let street = crate::race::tests::test_def(&[(100.0, 0.0), (200.0, 0.0)], 25.0, &path, 1, false);
        assert!(at(&street, 0).iter().all(|p| p.distance(Vec3::new(100.0 + CHECKPOINT_AHEAD_M, 0.0, 0.0)) < 1e-3));
        // Circuit, lap 1's lap line (not the final gate): a blue laser, not the finish pair.
        let circuit = crate::race::tests::test_def(&[(100.0, 0.0), (200.0, 0.0)], 30.0, &path, 2, true);
        assert_eq!(at(&circuit, 1).len(), 3);
        assert_eq!(at(&circuit, 3).len(), 6);
    }

    #[test]
    fn festival_laser_snaps_to_the_road_path() {
        let path = [Vec3::new(0.0, 10.0, 0.0), Vec3::new(100.0, 20.0, 0.0)];
        let p = on_path(&path, Vec3::new(50.0, 0.0, 12.0)).unwrap();
        assert!((p - Vec3::new(50.0, 15.0, 0.0)).length() < 1e-3, "{p}");
        assert!(on_path(&path, Vec3::new(50.0, 0.0, 60.0)).is_none());
    }

    #[test]
    fn laser_objects() {
        let mut blue = Vec::new();
        laser(&mut blue, Vec3::new(1.0, 2.0, 3.0), false);
        let count = |v: &[Piece], t: usize| v.iter().filter(|p| p.tpl == t).count();
        assert_eq!((count(&blue, T_LASER), count(&blue, T_HAZE), count(&blue, T_GLOW), count(&blue, T_EMITTER)), (3, 3, 1, 1));
        // Beams 120 degrees apart (k.x = index), the finish's flagged + 4 and without the base glow.
        let mut green = Vec::new();
        laser(&mut green, Vec3::ZERO, true);
        assert_eq!(count(&green, T_GLOW), 0);
        let kx: Vec<f32> = green.iter().filter(|p| p.tpl == T_LASER).map(|p| p.kx).collect();
        assert_eq!(kx, [4.0, 5.0, 6.0]);
        assert!(blue.iter().filter(|p| p.tpl != T_EMITTER && p.tpl != T_BOUNDS).all(|p| p.offset_normal && Vec3::from(p.xf.translation) == Vec3::new(1.0, 2.0, 3.0)));
    }
}
