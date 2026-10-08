//! Race visuals in the world (remaster style, 2026-10-08; `FH1_RACE_FX=0` = the old gizmo lines in race.rs).
//!
//! FH1's look, rebuilt: festival event starts are tall columns of light in the wristband colour with a ground ring
//! and outward ripples over the 25 m TriggerZone; street-race starts the same in blue; checkpoints are glowing gates
//! (posts, top bar, a see-through curtain with a rising sweep) with floor chevrons leading in; the finish has a
//! chequered curtain; passing a gate fires a ripple. One additive unlit material (race/marker.wgsl), soft SDF shapes,
//! HDR colour so bloom catches it, a 1.2 Hz pulse, near / far distance fades (no depth prepass in the remaster, so no
//! scene-depth soft edges: shapes fade out at their foot instead of cutting into the ground). Shares its look with
//! the world map's road chevrons (e4): track pink for the race, tier colours for event starts, group blue for street.

use std::collections::HashMap;

use bevy::asset::embedded_asset;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology};
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

/// Shared with the map's chevrons (e4): MapProfileFullscreen.xml "track" pink and group-route blue, x2 HDR.
fn pink() -> LinearRgba {
    Color::srgb_u8(250, 0, 100).to_linear() * 2.0
}
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
        d.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
        ])?];
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
            .add_systems(Update, (event_markers, gate_visuals, pass_flash, cannons).after(super::race_update));
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
    let mut b = Builder::new();
    b.grid(32, 8, style, 0.0, |u, w| {
        let a = u * std::f32::consts::TAU;
        let n = Vec3::new(a.cos(), 0.0, a.sin());
        (n + Vec3::Y * w, n)
    });
    b.build()
}

/// Flat annulus on y = 0 from radius `r0` to 1, uv = (around, radial 0..1).
fn annulus(r0: f32, style: u8) -> Mesh {
    let mut b = Builder::new();
    b.grid(64, 4, style, 0.0, |u, w| {
        let a = u * std::f32::consts::TAU;
        let r = r0 + (1.0 - r0) * w;
        (Vec3::new(a.cos() * r, 0.0, a.sin() * r), Vec3::Y)
    });
    b.build()
}

/// Vertical quad across X (-0.5..0.5) and up Y (0..1), facing Z.
fn curtain(style: u8) -> Mesh {
    let mut b = Builder::new();
    b.grid(16, 8, style, 0.0, |u, w| (Vec3::new(u - 0.5, w, 0.0), Vec3::Z));
    b.build()
}

/// Floor quad (X -0.5..0.5, forward = -Z 0..1); uv.y grows forwards.
fn floor_quad(style: u8, phase: f32) -> Mesh {
    let mut b = Builder::new();
    b.grid(2, 2, style, phase, |u, w| (Vec3::new(u - 0.5, 0.0, -w), Vec3::Y));
    b.build()
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

fn build_meshes(mut v: ResMut<Visuals>, mut meshes: ResMut<Assets<Mesh>>) {
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
        let want = if idle && near { Visibility::Inherited } else { Visibility::Hidden };
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
            marker_look(info.map(|e| e.state), info.map_or(crate::progression::event_tier(r), |e| e.tier), street, info.is_some_and(|e| e.recommended), rs.prompt == Some(*i));
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
