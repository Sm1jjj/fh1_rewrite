//! Exhaust flames (docs/AUDIO.md "Backfire flames"): every burble pop / backfire bang the car synth plays (fh1-audio
//! `synth::Backfire`, collected by audio.rs into [`BackfireFired`]) fires the game's own effects.zip `Backfire` effect
//! (GR_Backfire_DIFF flame atlas) from each exhaust outlet, for the player and for the AI / traffic cars with a voice.
//!
//! Outlets: the game names the exhaust carbin sections `exhausta` / `exhaustLa` / `exhaustRa` (default.xex part list
//! 82BF6988) and CarAttribs.xml only carries a direction override (`OverrideBackireDirection`, set on 2 of 176 cars),
//! so the outlet positions come from the exhaust geometry: the glTF `exhaust*` nodes' vertices within 8 cm of the
//! rearmost one, clustered in the cross-section (3 cm links, then fragments merged within one pipe width, 11 cm): one
//! outlet per tip, so twin / quad / side exits all fire. OUR rule (the game's emitter placement isn't traced); cars
//! without an exhaust mesh (F40, Exige, RAV4...) fire from the rear centre of the body. Flames point out the back.
//!
//! Exaggerated by request: bangs fire 3 flames per tip at 1.43x size (was 2.2x, then 1.3x) plus a smoke puff, pops 1 flame. `FH1_BACKFIRE_FX=0`
//! turns it off.
//!
//! Sync (2026-10-08, user: "lots of flame with no bang"): a flame fires only for a pop that is actually heard. The synth
//! (fh1-audio `synth::Backfire`) now reports a pop only when its output peak stands clear of the engine (prominence over
//! the block's engine RMS, `FH1_BACKFIRE_MIN_PROM`, default 1.5) and gives its loudness 0..1 as `strength`; quiet crackle
//! under the engine and far cars' inaudible pops fire nothing. `FH1_BACKFIRE_SYNC=0` = every queued pop as before.
//!
//! Flame shader (2026-10-08, user: the cone flames look "like an SVG"; default, `FH1_FLAME_STYLE=old` = the cone rigs):
//! flame.wgsl draws every tongue and outlet glow in one additive mesh. Tongues are noise-distorted teardrops along
//! the pipe axis with a blackbody ramp (blue-white core -> yellow -> orange -> dark red tips), HDR so bloom catches them,
//! length jittered every frame. Size and life follow the loudness: a quiet pop is a short lick, a loud bang a proper burst
//! with an exhaust smoke puff (Remaster smoke) and, for the player's car, a short orange flash point light on the
//! bumper / road (`FH1_FLAME_LIGHT=0` off, `FH1_FLAME_LIGHT_LM` peak lumens). `FH1_FLAME_BRIGHT` (1.0) scales the flames.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::visibility::NoFrustumCulling;
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology, VertexAttributeValues};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;
use bevy::world_serialization::WorldAssetRoot;
use fh1_engine::ai::AiCar;
use fh1_render::particles::{EffectId, FxParticles, Spawn};

use crate::{Car, Garage};

pub struct BackfirePlugin;

impl Plugin for BackfirePlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = std::sync::mpsc::channel();
        app.add_message::<BackfireFired>()
            .insert_resource(TipCache { tips: HashMap::new(), tx, rx: Mutex::new(rx) })
            .init_resource::<FlameAssetsRes>()
            .add_systems(Update, (find_tips, flames, own_flames, queue_burns, debug_tips).chain());
        if on() && own_style() && !old_flames() {
            embedded_asset!(app, "flame.wgsl");
            app.add_plugins(MaterialPlugin::<FlameMaterial>::default())
                .init_resource::<Burning>()
                .add_systems(PostUpdate, burn.after(bevy::transform::TransformSystems::Propagate));
        }
    }
}

/// A pop or bang from `car`'s exhaust (written by audio.rs).
#[derive(Message, Clone, Copy)]
pub struct BackfireFired {
    pub car: Entity,
    pub strength: f32,
    pub bang: bool,
}

/// Bang flame size over the game's (was 2.2: a 0.9 x 1.8 m sprite hid the pipes it came from, user 2026-10-07).
const BANG_SIZE: f32 = 1.43;

/// `FH1_BACKFIRE_DEBUG=1`: a marker at every outlet of every car each frame (red = out the back, blue = side exit), and
/// each car's outlets logged once, to check the placement apart from the flame sprites.
fn debug_tips(mut gizmos: Gizmos, cars: Query<(Entity, &ExhaustTips, Option<&Car>, Option<&AiCar>)>, fixed: Res<Time<Fixed>>, mut logged: Local<Vec<(Entity, String)>>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("FH1_BACKFIRE_DEBUG").is_ok_and(|v| v == "1")) {
        return;
    }
    for (e, tips, car, ai) in &cars {
        let Some(v) = car.map(|c| &c.0).or(ai.map(|a| &a.0)) else { continue };
        let (p, r) = v.render_pose(fixed.overstep_fraction());
        let pose = Transform::from_translation(p).with_rotation(r);
        for t in tips.tips.iter() {
            let w = pose.transform_point(t.pos);
            let color = if t.dir.z > 0.5 { Color::srgb(1.0, 0.1, 0.1) } else { Color::srgb(0.1, 0.3, 1.0) };
            gizmos.sphere(Isometry3d::from_translation(w), 0.04, color);
            gizmos.line(w, w + r * t.dir * 0.3, color);
        }
        if !logged.iter().any(|(le, m)| *le == e && *m == tips.model) {
            info!("backfire debug: {} outlets (car-local, m): {:?}", tips.model, tips.tips.iter().map(|t| (t.pos, t.dir)).collect::<Vec<_>>());
            logged.push((e, tips.model.clone()));
        }
    }
}

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_BACKFIRE_FX").map_or(true, |v| v != "0"))
}

/// One exhaust outlet: position and the direction its flame points (+Z = out the back; ±X for side exits).
#[derive(Clone, Copy, Debug)]
pub struct Tip {
    pub pos: Vec3,
    pub dir: Vec3,
}

/// Outlets of one car in its root's local space (body transform applied), and the model they came from.
#[derive(Component)]
struct ExhaustTips {
    model: String,
    tips: Arc<Vec<Tip>>,
    /// Our flame rigs (children of the body model, one per outlet), see `own_flames`.
    rigs: Vec<Entity>,
}

/// Outlets per model path (model space); `None` = loading on a worker thread.
#[derive(Resource)]
struct TipCache {
    tips: HashMap<String, Option<Arc<Vec<Tip>>>>,
    tx: Sender<(String, Vec<Tip>)>,
    rx: Mutex<Receiver<(String, Vec<Tip>)>>,
}

/// Gives every car (player / AI / traffic) its outlets once its body's glTF path is known.
fn find_tips(
    mut commands: Commands,
    mut cache: ResMut<TipCache>,
    garage: Res<Garage>,
    // Player and race AI only: traffic shows no flames (user 2026-10-07).
    cars: Query<(Entity, &Children, Option<&ExhaustTips>), (Or<(With<Car>, With<AiCar>)>, Without<fh1_engine::traffic::TrafficCar>)>,
    bodies: Query<(&WorldAssetRoot, &Transform)>,
    mut flame_assets: ResMut<FlameAssetsRes>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let _watch = crate::perf::watch("find_tips");
    if !on() {
        return;
    }
    let done: Vec<_> = cache.rx.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default();
    for (model, tips) in done {
        cache.tips.insert(model, Some(Arc::new(tips)));
    }
    for (car, children, have) in &cars {
        let Some((body, (root, body_t))) = children.iter().find_map(|c| bodies.get(c).ok().map(|b| (c, b))) else { continue };
        let Some(path) = root.0.path() else { continue };
        let model = path.path().to_string_lossy().into_owned();
        if have.is_some_and(|h| h.model == model) {
            continue;
        }
        match cache.tips.get(&model) {
            Some(Some(tips)) => {
                let local: Vec<Tip> = tips.iter().map(|t| Tip { pos: body_t.transform_point(t.pos), dir: body_t.rotation * t.dir }).collect();
                let rigs = if own_style() && old_flames() { spawn_rigs(&mut commands, &mut flame_assets.0, &mut meshes, &mut materials, body, tips) } else { Vec::new() };
                commands.entity(car).insert(ExhaustTips { model, tips: Arc::new(local), rigs });
            }
            Some(None) => {}
            None => {
                cache.tips.insert(model.clone(), None);
                let (tx, file) = (cache.tx.clone(), garage.assets.join(&model));
                let _ = std::thread::Builder::new().name("fh1-exhaust-tips".into()).spawn(move || {
                    let tips = exhaust_tips(&file).unwrap_or_else(|e| {
                        warn!("backfire: {}: {e:#}", file.display());
                        Vec::new()
                    });
                    let _ = tx.send((model, tips));
                });
            }
        }
    }
}

#[derive(Default)]
struct FlameIds {
    flame: Option<EffectId>,
    smoke: Option<EffectId>,
    looked: bool,
}

fn flames(
    mut fired: MessageReader<BackfireFired>,
    mut ids: Local<FlameIds>,
    particles: Option<ResMut<FxParticles>>,
    cars: Query<(&GlobalTransform, &ExhaustTips, Option<&Car>, Option<&AiCar>)>,
    fixed: Res<Time<Fixed>>,
) {
    let _watch = crate::perf::watch("flames");
    let Some(mut particles) = particles else {
        fired.clear();
        return;
    };
    if !on() || !particles.enabled() {
        fired.clear();
        return;
    }
    if !ids.looked {
        ids.flame = particles.effect("Backfire");
        ids.smoke = particles.effect("Smoke");
        ids.looked = ids.flame.is_some();
    }
    let Some(flame) = ids.flame else {
        fired.clear();
        return;
    };
    for f in fired.read() {
        let Ok((gt, tips, car, ai)) = cars.get(f.car) else { continue };
        let v = car.map(|c| &c.0).or(ai.map(|a| &a.0));
        let vel = v.map_or(Vec3::ZERO, |v| v.velocity);
        // This frame's drawn pose (the same interpolation sync_visuals gives the body), not the GlobalTransform, which is
        // last frame's until PostUpdate: a frame behind at speed.
        let (pos0, rot) = v.map_or((gt.translation(), gt.rotation()), |v| v.render_pose(fixed.overstep_fraction()));
        let pose = Transform::from_translation(pos0).with_rotation(rot);
        let up = rot * Vec3::Y;
        let k = f.strength.clamp(0.0, 1.5);
        for tip in tips.tips.iter() {
            let back = rot * tip.dir;
            // The particle renderer aligns Backfire's 1:2 flame quads with the emission direction and puts their base at the
            // particle (fh1-render particles.rs `align_travel`), so the particle starts at the pipe opening.
            let size = if f.bang { BANG_SIZE } else { 1.1 * (1.0 + 0.3 * k) };
            let pos = pose.transform_point(tip.pos) + back * 0.03;
            let mut s = Spawn::new(pos, (back - up * 0.05).normalize(), if f.bang { 3 } else { 1 });
            // Backfire.xml inherits no motion (MotionInheritance multiplier 0), which only works for an emitter attached to
            // the exhaust: with free particles a 0.1-0.15 s flame was left 3-4 m behind the car at 100 km/h. The flames
            // carry the car's full velocity instead (INFERRED: the game's car effects are attached).
            s.inherit = vel;
            s.intensity = 1.0;
            s.size_scale = size;
            if !own_style() {
                particles.spawn(flame, &s);
            }
            // The flame shader path makes its own exhaust puff (Remaster smoke) in `burn`.
            if f.bang && (old_flames() || !own_style()) {
                if let Some(smoke) = ids.smoke {
                    let mut s = Spawn::new(pos + back * 0.1, back, 2);
                    s.inherit = vel * 0.8;
                    s.intensity = 0.35;
                    s.size_scale = 0.5;
                    particles.spawn(smoke, &s);
                }
            }
        }
    }
}

/// Exhaust outlets of a car glTF in model space (module doc). Falls back to the rear centre of the whole model.
pub fn exhaust_tips(gltf: &Path) -> anyhow::Result<Vec<Tip>> {
    let g: serde_json::Value = serde_json::from_slice(&std::fs::read(gltf)?)?;
    let dir = gltf.parent().map(Path::to_path_buf).unwrap_or_default();
    let bin_name = g["buffers"][0]["uri"].as_str().unwrap_or("model.bin");
    let bin = std::fs::read(dir.join(bin_name))?;
    let nodes = g["nodes"].as_array().cloned().unwrap_or_default();
    let mut parent = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        for c in n["children"].as_array().into_iter().flatten() {
            if let Some(c) = c.as_u64() {
                parent.insert(c as usize, i);
            }
        }
    }
    let local = |n: &serde_json::Value| -> Mat4 {
        let f = |k: &str, d: &[f32]| -> Vec<f32> {
            n[k].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()).unwrap_or_else(|| d.to_vec())
        };
        if n["matrix"].is_array() {
            return Mat4::from_cols_slice(&f("matrix", &[0.0; 16]));
        }
        let (t, r, s) = (f("translation", &[0.0; 3]), f("rotation", &[0.0, 0.0, 0.0, 1.0]), f("scale", &[1.0; 3]));
        Mat4::from_scale_rotation_translation(Vec3::from_slice(&s), Quat::from_slice(&r), Vec3::from_slice(&t))
    };
    let world = |mut i: usize| {
        let mut m = local(&nodes[i]);
        while let Some(&p) = parent.get(&i) {
            m = local(&nodes[p]) * m;
            i = p;
        }
        m
    };
    let positions = |mesh: usize, m: Mat4, out: &mut Vec<Vec3>| {
        for prim in g["meshes"][mesh]["primitives"].as_array().into_iter().flatten() {
            let Some(a) = prim["attributes"]["POSITION"].as_u64() else { continue };
            let acc = &g["accessors"][a as usize];
            let Some(bv) = acc["bufferView"].as_u64() else { continue };
            let view = &g["bufferViews"][bv as usize];
            let off = view["byteOffset"].as_u64().unwrap_or(0) as usize + acc["byteOffset"].as_u64().unwrap_or(0) as usize;
            let stride = view["byteStride"].as_u64().unwrap_or(12) as usize;
            for k in 0..acc["count"].as_u64().unwrap_or(0) as usize {
                let o = off + k * stride;
                let Some(b) = bin.get(o..o + 12) else { break };
                let v = |j: usize| f32::from_le_bytes(b[j * 4..j * 4 + 4].try_into().unwrap());
                out.push(m.transform_point3(Vec3::new(v(0), v(1), v(2))));
            }
        }
    };
    // Triangle indices of one primitive (u8 / u16 / u32), or a plain triangle list.
    let indices = |prim: &serde_json::Value, count: usize| -> Vec<u32> {
        let Some(a) = prim["indices"].as_u64() else { return (0..count as u32).collect() };
        let acc = &g["accessors"][a as usize];
        let Some(bv) = acc["bufferView"].as_u64() else { return Vec::new() };
        let view = &g["bufferViews"][bv as usize];
        let off = view["byteOffset"].as_u64().unwrap_or(0) as usize + acc["byteOffset"].as_u64().unwrap_or(0) as usize;
        let size = match acc["componentType"].as_u64() {
            Some(5121) => 1,
            Some(5123) => 2,
            _ => 4,
        };
        (0..acc["count"].as_u64().unwrap_or(0) as usize)
            .map_while(|k| {
                let b = bin.get(off + k * size..off + (k + 1) * size)?;
                Some(match size {
                    1 => b[0] as u32,
                    2 => u16::from_le_bytes([b[0], b[1]]) as u32,
                    _ => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                })
            })
            .collect()
    };
    let mut pts = Vec::new();
    let mut all = Vec::new();
    // Exhaust primitives as (positions, triangle indices), for the per-pipe outlets.
    let mut pipes: Vec<(Vec<Vec3>, Vec<u32>)> = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let name = n["name"].as_str().unwrap_or("").to_ascii_lowercase();
        if let Some(mesh) = n["mesh"].as_u64() {
            positions(mesh as usize, world(i), &mut all);
            if name.starts_with("exhaust") {
                positions(mesh as usize, world(i), &mut pts);
                let m = world(i);
                for prim in g["meshes"][mesh as usize]["primitives"].as_array().into_iter().flatten() {
                    let Some(a) = prim["attributes"]["POSITION"].as_u64() else { continue };
                    let acc = &g["accessors"][a as usize];
                    let Some(bv) = acc["bufferView"].as_u64() else { continue };
                    let view = &g["bufferViews"][bv as usize];
                    let off = view["byteOffset"].as_u64().unwrap_or(0) as usize + acc["byteOffset"].as_u64().unwrap_or(0) as usize;
                    let stride = view["byteStride"].as_u64().unwrap_or(12) as usize;
                    let mut p = Vec::new();
                    for k in 0..acc["count"].as_u64().unwrap_or(0) as usize {
                        let o = off + k * stride;
                        let Some(b) = bin.get(o..o + 12) else { break };
                        let v = |j: usize| f32::from_le_bytes(b[j * 4..j * 4 + 4].try_into().unwrap());
                        p.push(m.transform_point3(Vec3::new(v(0), v(1), v(2))));
                    }
                    let idx = indices(prim, p.len());
                    pipes.push((p, idx));
                }
            }
        }
    }
    let zmax = all.iter().map(|p| p.z).fold(f32::NEG_INFINITY, f32::max);
    if !zmax.is_finite() {
        return Ok(Vec::new());
    }
    if pts.is_empty() {
        // No exhaust mesh: rear centre of the body, low.
        let ymin = all.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        return Ok(vec![Tip { pos: Vec3::new(0.0, ymin + 0.25, zmax - 0.05), dir: Vec3::Z }]);
    }
    let half_width = all.iter().map(|p| p.x.abs()).fold(0.0f32, f32::max);
    if per_pipe() {
        let tips = pipe_tips(&pipes, zmax, half_width);
        if !tips.is_empty() {
            return Ok(tips);
        }
        // Slot / oblong outlets (Koenigsegg CCXR, McLaren MP4-12C...): the clustering below.
    }
    Ok(cluster_tips(&pts)
        .into_iter()
        .map(|pos| {
            // Side exit (e.g. the Viper ACR's sill pipes): the outlet sits at the body side well ahead of the rear bumper,
            // so the flame points outward, not back.
            let side = pos.z < zmax - 0.5 && pos.x.abs() > 0.75 * half_width;
            Tip { pos, dir: if side { Vec3::new(pos.x.signum(), 0.0, 0.0) } else { Vec3::Z } }
        })
        .collect())
}

/// `FH1_EXHAUST_PER_PIPE=0`: only the 10-07 rim clustering (it merged pipes closer than 11 cm: one flame between the two
/// pipes of each BMW M5 pair, user 2026-10-08).
fn per_pipe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_EXHAUST_PER_PIPE").map_or(true, |v| v != "0"))
}

/// One outlet per pipe (2026-10-08): every connected piece of the exhaust mesh (vertices welded at 0.1 mm) whose last
/// 10 cm along its outward direction has a round-ish cross-section (radius 1.2-12 cm, extents within 2.2:1) is a pipe
/// end; the outlet is the centre of that cross-section (a bounding-box centre, right for slash-cut tips too), 1 cm inside
/// the end, pointing out of it. Outward = back (+Z), or sideways for pieces at the body side well ahead of the rear
/// (Viper side exits). Rear outlets more than 20 cm ahead of the rearmost are inner pieces; outlets within 3 cm across
/// (inner / outer walls, LOD copies) are one pipe. Survey of the 176 install cars (2026-10-08, scratch script): M5 4 (was
/// 2), 250 GTO / Diablo SV / LP700-4 / M3 E92 4, 458 / F40 LM 3 centre pipes, Viper side exits 2; 13 cars have an exhaust
/// mesh without a round end (slots / ovals) and keep the clustering.
fn pipe_tips(pipes: &[(Vec<Vec3>, Vec<u32>)], body_zmax: f32, half_width: f32) -> Vec<Tip> {
    let mut found: Vec<(Tip, f32)> = Vec::new();
    for (pts, idx) in pipes {
        // Weld, then union-find over the triangle edges.
        let mut weld: HashMap<[i32; 3], usize> = HashMap::new();
        let id: Vec<usize> = pts
            .iter()
            .enumerate()
            .map(|(k, p)| *weld.entry([(p.x * 1e4).round() as i32, (p.y * 1e4).round() as i32, (p.z * 1e4).round() as i32]).or_insert(k))
            .collect();
        let mut parent: Vec<usize> = (0..pts.len()).collect();
        fn find(p: &mut [usize], mut i: usize) -> usize {
            while p[i] != i {
                p[i] = p[p[i]];
                i = p[i];
            }
            i
        }
        for t in idx.chunks_exact(3) {
            let (Some(&a), Some(&b), Some(&c)) = (id.get(t[0] as usize), id.get(t[1] as usize), id.get(t[2] as usize)) else { continue };
            for (u, v) in [(a, b), (b, c)] {
                let (ru, rv) = (find(&mut parent, u), find(&mut parent, v));
                parent[ru] = rv;
            }
        }
        let mut comps: HashMap<usize, Vec<Vec3>> = HashMap::new();
        for k in 0..pts.len() {
            let r = find(&mut parent, id[k]);
            comps.entry(r).or_default().push(pts[k]);
        }
        for c in comps.values() {
            let centre = c.iter().copied().sum::<Vec3>() / c.len() as f32;
            let xmax = c.iter().map(|p| p.x.abs()).fold(0.0f32, f32::max);
            let side = centre.z < body_zmax - 0.5 && xmax > 0.75 * half_width;
            let dir = if side { Vec3::new(centre.x.signum(), 0.0, 0.0) } else { Vec3::Z };
            let top = c.iter().map(|p| p.dot(dir)).fold(f32::NEG_INFINITY, f32::max);
            let slab: Vec<Vec3> = c.iter().copied().filter(|p| p.dot(dir) > top - 0.1).collect();
            if slab.len() < 6 {
                continue;
            }
            let lo = slab.iter().copied().fold(Vec3::splat(f32::INFINITY), Vec3::min);
            let hi = slab.iter().copied().fold(Vec3::splat(f32::NEG_INFINITY), Vec3::max);
            let ext = hi - lo;
            let (a, b) = if side { (ext.y, ext.z) } else { (ext.x, ext.y) };
            let r = 0.25 * (a + b);
            if !(0.012..=0.12).contains(&r) || a.max(b) > 2.2 * a.min(b).max(1e-3) {
                continue;
            }
            let mid = (lo + hi) * 0.5;
            let pos = mid - dir * mid.dot(dir) + dir * (top - 0.01);
            // The pipe's own axis: from the cross-section centre 5-10 cm in to the last 5 cm (kept within ~45 deg of out).
            let half_mid = |near: bool| {
                let s: Vec<Vec3> = slab.iter().copied().filter(|p| (p.dot(dir) > top - 0.05) != near).collect();
                (s.len() >= 6).then(|| {
                    let lo = s.iter().copied().fold(Vec3::splat(f32::INFINITY), Vec3::min);
                    let hi = s.iter().copied().fold(Vec3::splat(f32::NEG_INFINITY), Vec3::max);
                    (lo + hi) * 0.5
                })
            };
            let axis = match (half_mid(true), half_mid(false)) {
                (Some(n), Some(f)) if (f - n).length() > 0.005 && (f - n).normalize().dot(dir) > 0.7 => (f - n).normalize(),
                _ => dir,
            };
            found.push((Tip { pos, dir: axis }, top));
        }
    }
    let rear_max = found.iter().filter(|(t, _)| t.dir.z > 0.7).map(|(_, top)| *top).fold(f32::NEG_INFINITY, f32::max);
    found.retain(|(t, top)| t.dir.z <= 0.7 || *top > rear_max - 0.2);
    // Most outward first, then one outlet per pipe (same way out, within 3 cm across it).
    found.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut tips: Vec<Tip> = Vec::new();
    for (t, _) in found {
        let out = if t.dir.z > 0.7 { Vec3::Z } else { Vec3::new(t.dir.x.signum(), 0.0, 0.0) };
        let across = |p: Vec3| p - out * p.dot(out);
        if tips.iter().all(|u| u.dir.dot(out) < 0.7 || across(u.pos).distance(across(t.pos)) > 0.03) {
            tips.push(t);
        }
    }
    tips.truncate(8);
    tips
}

/// Rear rim (8 cm slab, model +Z = back) -> 3 cm single-linkage clusters in x/y -> merged within 11 cm -> at most 8.
fn cluster_tips(pts: &[Vec3]) -> Vec<Vec3> {
    let zmax = pts.iter().map(|p| p.z).fold(f32::NEG_INFINITY, f32::max);
    let rim: Vec<Vec3> = pts.iter().copied().filter(|p| p.z > zmax - 0.08).collect();
    let n = rim.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    for i in 0..n {
        for j in i + 1..n {
            if rim[i].truncate().distance(rim[j].truncate()) < 0.03 {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                parent[a] = b;
            }
        }
    }
    let mut groups: HashMap<usize, (Vec3, f32)> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let e = groups.entry(r).or_insert((Vec3::ZERO, 0.0));
        e.0 += rim[i];
        e.1 += 1.0;
    }
    let mut frags: Vec<(Vec3, f32)> = groups.into_values().map(|(s, c)| (s / c, c)).collect();
    'merge: loop {
        for i in 0..frags.len() {
            for j in i + 1..frags.len() {
                if frags[i].0.truncate().distance(frags[j].0.truncate()) < 0.11 {
                    let ((a, na), (b, nb)) = (frags[i], frags[j]);
                    frags[i] = ((a * na + b * nb) / (na + nb), na + nb);
                    frags.swap_remove(j);
                    continue 'merge;
                }
            }
        }
        break;
    }
    frags.retain(|f| f.1 >= 3.0);
    frags.sort_by(|a, b| b.1.total_cmp(&a.1));
    frags.truncate(8);
    frags.into_iter().map(|f| f.0).collect()
}

// ---------------------------------------------------------------- our own flames (user 2026-10-07)
//
// "Maybe our own flames rather than the game's? It could be more satisfying." Cone rigs parented to the body model at each
// outlet in MODEL space (where the exhaust mesh ends), so they move with the drawn body: no world pose maths, no
// interpolation, no particle drift. An orange outer flame with a blue-white core, additive, flickering per burst.

/// `FH1_BACKFIRE_STYLE=game`: the game's Backfire particles instead of our flame rigs.
fn own_style() -> bool {
    static OWN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OWN.get_or_init(|| !std::env::var("FH1_BACKFIRE_STYLE").is_ok_and(|v| v == "game"))
}

/// Flame size over the defaults (a bang is ~0.65 m long, a pop ~0.3 m).
const FLAME_SCALE: f32 = 1.1;
/// Display-linear emission of the outer flame and the core (above 1 so they bloom).
const OUTER_RGB: Vec3 = Vec3::new(4.0, 1.3, 0.25);
const CORE_RGB: Vec3 = Vec3::new(2.0, 3.0, 6.0);

/// Shared cone mesh (unit radius / height, base at the origin, tip at +Y) and the two additive materials, made once and
/// used by every rig (find_tips) and the exposure update (own_flames).
#[derive(Resource, Default)]
struct FlameAssetsRes(Option<FlameAssets>);

#[derive(Clone)]
struct FlameAssets {
    cone: Handle<Mesh>,
    outer: Handle<StandardMaterial>,
    core: Handle<StandardMaterial>,
}

/// One outlet's flame: a child of the body model, +Y = out of the pipe; scaled to (width, length, width) while burning.
#[derive(Component, Default)]
struct FlameRig {
    left: f32,
    total: f32,
    length: f32,
    width: f32,
    seed: u32,
}

fn flame_material(rgb: Vec3) -> StandardMaterial {
    StandardMaterial {
        // Black base, all light from the emission (which Bevy scales by the camera exposure; own_flames compensates).
        base_color: Color::BLACK,
        emissive: LinearRgba::rgb(rgb.x, rgb.y, rgb.z),
        perceptual_roughness: 1.0,
        reflectance: 0.0,
        alpha_mode: AlphaMode::Add,
        double_sided: true,
        cull_mode: None,
        fog_enabled: false,
        ..default()
    }
}

fn flame_assets(cache: &mut Option<FlameAssets>, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>) -> FlameAssets {
    cache
        .get_or_insert_with(|| FlameAssets {
            cone: meshes.add(Mesh::from(Cone { radius: 1.0, height: 1.0 }).translated_by(Vec3::Y * 0.5)),
            outer: materials.add(flame_material(OUTER_RGB)),
            core: materials.add(flame_material(CORE_RGB)),
        })
        .clone()
}

fn spawn_rigs(
    commands: &mut Commands,
    cache: &mut Option<FlameAssets>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    body: Entity,
    tips: &[Tip],
) -> Vec<Entity> {
    let a = flame_assets(cache, meshes, materials);
    tips.iter()
        .enumerate()
        .map(|(i, t)| {
            let rot = Quat::from_rotation_arc(Vec3::Y, t.dir.normalize_or(Vec3::Z));
            commands
                .spawn((
                    Name::new("fh1 backfire flame"),
                    FlameRig { seed: 0x9E37_79B9 ^ (i as u32).wrapping_mul(7919).wrapping_add(1), ..default() },
                    Transform::from_translation(t.pos).with_rotation(rot).with_scale(Vec3::splat(1e-4)),
                    Visibility::Hidden,
                    ChildOf(body),
                ))
                .with_children(|p| {
                    p.spawn((Mesh3d(a.cone.clone()), MeshMaterial3d(a.outer.clone()), bevy::light::NotShadowCaster));
                    p.spawn((
                        Mesh3d(a.cone.clone()),
                        MeshMaterial3d(a.core.clone()),
                        Transform::from_scale(Vec3::new(0.45, 0.55, 0.45)),
                        bevy::light::NotShadowCaster,
                    ));
                })
                .id()
        })
        .collect()
}

/// Bursts on pops / bangs, flicker, fade back into the pipe; the emission follows the remaster exposure (the car
/// probe's 1.2 x 2^ev100 rule) so a flame is as bright at noon as at night.
#[allow(clippy::too_many_arguments)]
fn own_flames(
    mut fired: MessageReader<BackfireFired>,
    tips: Query<&ExhaustTips>,
    mut rigs: Query<(&mut FlameRig, &mut Transform, &mut Visibility)>,
    time: Res<Time>,
    lighting: Option<Res<fh1_remaster::light::RemasterLighting>>,
    flame_assets: Res<FlameAssetsRes>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut last_ev: Local<f32>,
) {
    if !own_style() || !on() || !old_flames() {
        fired.clear();
        return;
    }
    for f in fired.read() {
        let Ok(t) = tips.get(f.car) else { continue };
        let k = f.strength.clamp(0.0, 1.5);
        let (len, wid, dur) = if f.bang { (0.65, 0.09, 0.16) } else { (0.3 + 0.1 * k, 0.05, 0.08) };
        for &r in &t.rigs {
            if let Ok((mut rig, ..)) = rigs.get_mut(r) {
                rig.left = dur;
                rig.total = dur;
                rig.length = len * FLAME_SCALE;
                rig.width = wid * FLAME_SCALE;
            }
        }
    }
    let dt = time.delta_secs();
    for (mut rig, mut tr, mut vis) in &mut rigs {
        if rig.left <= 0.0 {
            vis.set_if_neq(Visibility::Hidden);
            continue;
        }
        rig.left -= dt;
        rig.seed ^= rig.seed << 13;
        rig.seed ^= rig.seed >> 17;
        rig.seed ^= rig.seed << 5;
        let r = (rig.seed >> 8) as f32 / (1u32 << 24) as f32;
        // Shoots out fast, flickers, shrinks back into the pipe.
        let life = 1.0 - (rig.left / rig.total.max(1e-3)).clamp(0.0, 1.0);
        let env = (life * 6.0).min(1.0) * (1.0 - life).powf(0.6);
        let len = rig.length * env * (0.7 + 0.6 * r);
        let wid = rig.width * (0.6 + 0.4 * env) * (0.85 + 0.3 * r);
        tr.scale = Vec3::new(wid.max(1e-4), len.max(1e-4), wid.max(1e-4));
        vis.set_if_neq(Visibility::Inherited);
    }
    // Exposure compensation: emission = display-linear colour x 1.2 x 2^ev100.
    let ev = lighting.map_or(9.7, |l| l.ev100);
    if (ev - *last_ev).abs() > 0.05 {
        if let Some(a) = flame_assets.0.clone() {
            *last_ev = ev;
            let k = 1.2 * 2f32.powf(ev);
            for (h, rgb) in [(&a.outer, OUTER_RGB), (&a.core, CORE_RGB)] {
                if let Some(mut m) = materials.get_mut(h) {
                    m.emissive = LinearRgba::rgb(rgb.x * k, rgb.y * k, rgb.z * k);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- flame shader (2026-10-08, module doc)

/// `FH1_FLAME_STYLE=old`: the 2026-10-07 cone rigs instead of the flame shader.
/// `FH1_FX_HALF_FLAMES=1`: the flame tongues + outlet glows go through the half-res effects pass (fh1-render
/// fx_half_res.rs) like the smoke. Default off (native resolution): a tongue is 5-13 cm wide (~10-20 px in the chase
/// camera) with high-frequency noise edges, i.e. the "fine sprite" case the half-res technique leaves at full resolution,
/// and it lives 0.1-0.2 s, so the fill it would save is negligible next to the blur it adds.
fn half_flames() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| fh1_render::fx_half_res::on() && std::env::var("FH1_FX_HALF_FLAMES").is_ok_and(|v| v == "1"))
}

fn old_flames() -> bool {
    static OLD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OLD.get_or_init(|| std::env::var("FH1_FLAME_STYLE").is_ok_and(|v| v == "old"))
}

fn env_f(name: &str, d: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType)]
pub struct FlameParams {
    /// x = brightness (`FH1_FLAME_BRIGHT`), yzw unused.
    k: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct FlameMaterial {
    /// Binding 2 (flame.wgsl): matches the half-res pass's fixed material layout (fh1-render fx_half_res.rs).
    #[uniform(2)]
    params: FlameParams,
}

impl Material for FlameMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/flame.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/flame.wgsl".into()
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
            Mesh::ATTRIBUTE_UV_0.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(2),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(3),
            Mesh::ATTRIBUTE_TANGENT.at_shader_location(4),
        ])?];
        d.primitive.cull_mode = None;
        if let Some(f) = d.fragment.as_mut() {
            for t in f.targets.iter_mut().flatten() {
                // Pure additive light.
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

/// One burning tongue at one outlet (car-local, follows the drawn body).
#[derive(Clone, Copy)]
struct Tongue {
    car: Entity,
    tip: usize,
    age: f32,
    life: f32,
    length: f32,
    width: f32,
    k: f32,
    seed: f32,
}

#[derive(Resource)]
struct Burning {
    /// Pops heard this frame, burnt in PostUpdate (after transform propagation = the drawn pose).
    pending: Vec<BackfireFired>,
    tongues: Vec<Tongue>,
    /// Flash light entity, time left, total, peak lumens.
    flash: Option<Entity>,
    flash_t: f32,
    flash_total: f32,
    flash_lm: f32,
    rng: u32,
    draw: Option<(Entity, Handle<Mesh>, Handle<FlameMaterial>)>,
    /// flame.wgsl for the half-res pass (`half_flames`), loaded on first use.
    half_shader: Option<Handle<Shader>>,
    pos: Vec<[f32; 3]>,
    corner: Vec<[f32; 2]>,
    size: Vec<[f32; 2]>,
    data: Vec<[f32; 4]>,
    axis: Vec<[f32; 4]>,
}

impl Default for Burning {
    fn default() -> Self {
        Burning {
            pending: Vec::new(),
            tongues: Vec::new(),
            flash: None,
            flash_t: 0.0,
            flash_total: 0.1,
            flash_lm: 0.0,
            rng: 0x6C8E_9CF5,
            draw: None,
            half_shader: None,
            pos: Vec::new(),
            corner: Vec::new(),
            size: Vec::new(),
            data: Vec::new(),
            axis: Vec::new(),
        }
    }
}

impl Burning {
    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
    fn quad(&mut self, pos: Vec3, size: [f32; 2], data: [f32; 4], axis: Vec3) {
        for c in [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]] {
            self.pos.push(pos.to_array());
            self.corner.push(c);
            self.size.push(size);
            self.data.push(data);
            self.axis.push(axis.extend(0.0).to_array());
        }
    }
}

fn queue_burns(mut fired: MessageReader<BackfireFired>, burning: Option<ResMut<Burning>>) {
    let Some(mut b) = burning else {
        fired.clear();
        return;
    };
    if !on() {
        fired.clear();
        return;
    }
    b.pending.extend(fired.read().copied());
}

const MAX_TONGUES: usize = 256;

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn burn(
    mut commands: Commands,
    mut burning: ResMut<Burning>,
    time: Res<Time>,
    cars: Query<(&GlobalTransform, &ExhaustTips, Option<&Car>, Option<&AiCar>)>,
    mut smoke: Option<ResMut<crate::smoke::Smoke>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FlameMaterial>>,
    mut ents: Query<(&mut Visibility, &mut Transform, &mut GlobalTransform, Option<&mut PointLight>), (Without<ExhaustTips>, Without<fh1_render::post::FxPostCamera>)>,
    (mut half, assets, cams): (Option<ResMut<fh1_render::fx_half_res::FxHalfRes>>, Res<AssetServer>, Query<(&GlobalTransform, &Camera), With<fh1_render::post::FxPostCamera>>),
) {
    let _watch = crate::perf::watch("burn");
    let b = &mut *burning;
    let dt = time.delta_secs().min(0.1);
    let light_on = std::env::var("FH1_FLAME_LIGHT").map_or(true, |v| v != "0");

    // --- pops heard -> tongues, exhaust puffs, flash.
    let pending = std::mem::take(&mut b.pending);
    for f in &pending {
        let Ok((gt, tips, car, ai)) = cars.get(f.car) else { continue };
        let k = f.strength.clamp(0.0, 1.0);
        let vel = car.map(|c| c.0.velocity).or(ai.map(|a| a.0.velocity)).unwrap_or(Vec3::ZERO);
        let rot = gt.rotation();
        // Loudness -> size and life: a quiet pop is a short lick, a loud bang a long burst.
        let (length, width, life) = if f.bang { (0.45 + 0.5 * k, 0.085 + 0.05 * k, 0.12 + 0.09 * k) } else { (0.12 + 0.28 * k, 0.045 + 0.03 * k, 0.05 + 0.06 * k) };
        for (ti, tip) in tips.tips.iter().enumerate() {
            // At most 3 live tongues per outlet: busy burbles overlap, the oldest gives way.
            if b.tongues.iter().filter(|t| t.car == f.car && t.tip == ti).count() >= 3 {
                if let Some(i) = b.tongues.iter().position(|t| t.car == f.car && t.tip == ti) {
                    b.tongues.remove(i);
                }
            }
            if b.tongues.len() >= MAX_TONGUES {
                b.tongues.remove(0);
            }
            let seed = b.rand();
            let jitter = 0.85 + 0.3 * b.rand();
            b.tongues.push(Tongue { car: f.car, tip: ti, age: 0.0, life: life * jitter, length: length * jitter, width, k, seed });
            if f.bang {
                let at = gt.transform_point(tip.pos);
                let dir = (rot * tip.dir).normalize_or(Vec3::Z);
                if let Some(smoke) = smoke.as_deref_mut() {
                    smoke.exhaust_puff(at + dir * 0.35, vel * 0.85 + dir * 1.5, k);
                }
            }
        }
        // Flash on the bumper / road: the player's car, bangs and loud pops.
        if light_on && car.is_some() && (f.bang || k > 0.7) && !tips.tips.is_empty() {
            let lm = env_f("FH1_FLAME_LIGHT_LM", 300_000.0) * (0.35 + 0.65 * k) * if f.bang { 1.0 } else { 0.4 };
            if b.flash_t <= 0.0 || lm > b.flash_lm * (b.flash_t / b.flash_total.max(1e-3)) {
                b.flash_lm = lm;
                b.flash_total = if f.bang { 0.09 + 0.05 * k } else { 0.05 };
                b.flash_t = b.flash_total;
            }
            let mean = tips.tips.iter().map(|t| t.pos).sum::<Vec3>() / tips.tips.len() as f32;
            let at = gt.transform_point(mean + Vec3::Z * 0.35 + Vec3::Y * 0.05);
            match b.flash {
                Some(e) => {
                    if let Ok((_, mut t, mut g, _)) = ents.get_mut(e) {
                        t.translation = at;
                        *g = GlobalTransform::from_translation(at);
                    }
                }
                None => {
                    // One entity kept for the session (hidden between flashes): the light count stays put (perf/p6.rs
                    // switches clustering back on above 4 point/spot lights; the player's 2 headlights + this = 3).
                    let e = commands
                        .spawn((
                            PointLight { intensity: 0.0, range: 7.0, radius: 0.12, color: Color::srgb(1.0, 0.55, 0.2), ..default() },
                            Transform::from_translation(at),
                            GlobalTransform::from_translation(at),
                            Visibility::Hidden,
                            Name::new("fh1 backfire flash"),
                        ))
                        .id();
                    // Not in the remaster car probe's cube (it would light the car / road through the probe).
                    if let Some(layers) = fh1_remaster::car_probe::own_light_layers() {
                        commands.entity(e).insert(layers);
                    }
                    b.flash = Some(e);
                }
            }
        }
    }

    // --- flash decay.
    if let Some(e) = b.flash {
        b.flash_t -= dt;
        let r = b.rand();
        if let Ok((mut v, _, _, Some(mut l))) = ents.get_mut(e) {
            if b.flash_t > 0.0 {
                let x = b.flash_t / b.flash_total.max(1e-3);
                l.intensity = b.flash_lm * x * x * (0.8 + 0.4 * r);
                v.set_if_neq(Visibility::Inherited);
            } else if l.intensity != 0.0 || *v != Visibility::Hidden {
                l.intensity = 0.0;
                v.set_if_neq(Visibility::Hidden);
            }
        }
    }

    // --- simulate.
    b.tongues.retain_mut(|t| {
        t.age += dt;
        t.age < t.life
    });

    // --- build the mesh.
    b.pos.clear();
    b.corner.clear();
    b.size.clear();
    b.data.clear();
    b.axis.clear();
    let tongues = std::mem::take(&mut b.tongues);
    let mut centroid = Vec3::ZERO;
    let mut count = 0usize;
    for t in &tongues {
        let Ok((gt, tips, ..)) = cars.get(t.car) else { continue };
        let Some(tip) = tips.tips.get(t.tip) else { continue };
        let pos = gt.transform_point(tip.pos);
        let dir = (gt.rotation() * tip.dir).normalize_or(Vec3::Z);
        let u = t.age / t.life;
        // Shoots out, flickers (fresh length jitter every frame), shrinks back into the pipe.
        let env = (u * 7.0).min(1.0) * (1.0 - u).powf(0.45);
        let len = t.length * env * (0.72 + 0.56 * b.rand());
        let wid = t.width * (0.65 + 0.35 * env) * (0.85 + 0.3 * b.rand());
        let intensity = (0.75 + 0.6 * t.k) * (1.0 - u * u).max(0.0) * (0.85 + 0.3 * b.rand());
        centroid += pos;
        count += 1;
        b.quad(pos, [len.max(1e-3), wid.max(1e-3)], [t.seed, intensity, u, 0.0], dir);
        // Outlet glow (carries the flame when looking down the pipe).
        let r = t.width * 2.8 * (0.6 + 0.4 * env);
        b.quad(pos + dir * 0.02, [r, r], [t.seed, intensity * 0.8, u, 1.0], dir);
    }
    b.tongues = tongues;

    if count == 0 {
        if let Some((e, ..)) = b.draw {
            if let Ok((mut v, ..)) = ents.get_mut(e) {
                v.set_if_neq(Visibility::Hidden);
            }
        }
        return;
    }
    centroid /= count as f32;
    if let Some(half) = half.as_deref_mut().filter(|_| half_flames()) {
        // Half-res effects pass: world-space quads (b.pos is still world space here), no entity.
        if let Some((e, ..)) = b.draw {
            if let Ok((mut v, ..)) = ents.get_mut(e) {
                v.set_if_neq(Visibility::Hidden);
            }
        }
        use fh1_render::fx_half_res::{FxBatch, FxBlend, FxVertex};
        let cam = cams.iter().find(|c| c.1.is_active).map_or(centroid, |c| c.0.translation());
        let shader = b.half_shader.get_or_insert_with(|| assets.load("embedded://fh1_engine/flame.wgsl")).clone();
        let verts = (0..b.pos.len())
            .map(|i| FxVertex { pos: b.pos[i], corner: b.corner[i], size_rot: b.size[i], colour: b.data[i], extra: b.axis[i] })
            .collect();
        let params = FlameParams { k: Vec4::new(env_f("FH1_FLAME_BRIGHT", 1.0), 0.0, 0.0, 0.0) };
        half.push(FxBatch { shader, blend: FxBlend::AddPremul, tex0: None, tex1: None, params: FxBatch::uniform(&params), verts, dist2: centroid.distance_squared(cam) });
        return;
    }
    for p in b.pos.iter_mut() {
        *p = (Vec3::from(*p) - centroid).to_array();
    }
    let params = FlameParams { k: Vec4::new(env_f("FH1_FLAME_BRIGHT", 1.0), 0.0, 0.0, 0.0) };
    let n = b.pos.len() / 4;
    match b.draw.clone() {
        Some((ent, mesh, _)) => {
            if let Some(mut m) = meshes.get_mut(&mesh) {
                fill_flames(&mut m, b, n);
            }
            if let Ok((mut v, mut t, mut gt, _)) = ents.get_mut(ent) {
                v.set_if_neq(Visibility::Inherited);
                t.translation = centroid;
                // After propagation: write the GlobalTransform the renderer extracts this frame.
                *gt = GlobalTransform::from_translation(centroid);
            }
        }
        None => {
            let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
            m.insert_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_UV_0, Vec::<[f32; 2]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_UV_1, Vec::<[f32; 2]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new());
            m.insert_attribute(Mesh::ATTRIBUTE_TANGENT, Vec::<[f32; 4]>::new());
            m.insert_indices(Indices::U32(Vec::new()));
            fill_flames(&mut m, b, n);
            let mesh = meshes.add(m);
            let mat = materials.add(FlameMaterial { params });
            let ent = commands
                .spawn((
                    Mesh3d(mesh.clone()),
                    MeshMaterial3d(mat.clone()),
                    Transform::from_translation(centroid),
                    GlobalTransform::from_translation(centroid),
                    Visibility::Inherited,
                    NoFrustumCulling,
                    bevy::light::NotShadowCaster,
                    bevy::light::NotShadowReceiver,
                    Name::new("fh1 backfire flames"),
                ))
                .id();
            b.draw = Some((ent, mesh, mat));
        }
    }
}

fn fill_flames(m: &mut Mesh, b: &Burning, n: usize) {
    fn put<T: Copy>(dst: &mut Vec<T>, src: &[T]) {
        dst.clear();
        dst.extend_from_slice(src);
    }
    let prev = fh1_render::particles::quad_mesh_capacity(m);
    if let Some(VertexAttributeValues::Float32x3(v)) = m.attribute_mut(Mesh::ATTRIBUTE_POSITION) {
        put(v, &b.pos);
    }
    if let Some(VertexAttributeValues::Float32x2(v)) = m.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        put(v, &b.corner);
    }
    if let Some(VertexAttributeValues::Float32x2(v)) = m.attribute_mut(Mesh::ATTRIBUTE_UV_1) {
        put(v, &b.size);
    }
    if let Some(VertexAttributeValues::Float32x4(v)) = m.attribute_mut(Mesh::ATTRIBUTE_COLOR) {
        put(v, &b.data);
    }
    if let Some(VertexAttributeValues::Float32x4(v)) = m.attribute_mut(Mesh::ATTRIBUTE_TANGENT) {
        put(v, &b.axis);
    }
    if let Some(Indices::U32(idx)) = m.indices_mut() {
        fh1_render::particles::quad_indices(idx, n);
    }
    // Stable allocation size (fh1-render particles.rs `fx_mesh_cap_on`; FH1_FX_MESH_CAP=0 = old).
    fh1_render::particles::pad_quad_mesh(m, n, prev);
}
