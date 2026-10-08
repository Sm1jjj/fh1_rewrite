//! Colorado spectator crowds (`CProceduralCharacters`, `.pgeo` type 3; fh1_formats::crowd, docs/PROPS.md
//! "Crowds"): sprite cards from the game's crowd atlas (`crowd.wgsl`) for the far crowd, and the game's
//! skinned spectator models (`models/*.skinbin`) near the camera.
//!
//! Input: the `crowd` setup group (`crowd/colorado/crowds.bin`, `index.json`, `crowd/sprites.dds`,
//! `crowd/models`, `crowd/textures`). By default only the named `crowd_*` objects are drawn (festival
//! site, hubs, town: the free-roam crowds); the `0xFFFFFFFF`-named event crowds (race starts, showcases,
//! multiplayer) are skipped. `FH1_CROWD=all` draws every object, `FH1_CROWD=0` none, `FH1_CROWD=walkers`
//! only the walkers, `FH1_CROWD_NEAR=<m>`
//! sets the model distance (0 = sprites only). One card mesh per object, spawned within `RANGE`.
//!
//! Models vs sprites (docs/PROPS.md "Crowds"): the game (0x82DE2E50, VERIFIED) draws a whole crowd group as
//! models when the distance from the camera to the group's sphere is under `SpriteDistance` (TrackSettings.xml
//! `<Crowds>`, 60 m) x `CrowdSpriteDistScale` (1.0), as sprites otherwise. Here: spectators within that
//! distance, nearest first, capped at `lod0 + lod1 + lod2 + lod3` (5 / 20 / 50 / 125) models, the k-th band
//! drawing skinbin LOD k (INFERRED: read as per-LOD model counts, scaled by `CrowdLODCountScales`); cards
//! are cut at the same distance. Models are skinned to their skeleton and play their class's clips
//! (fh1_formats::crowd::anim): when a clip ends a new one is drawn from a 50-slot table built from the
//! class's weights (0x82DE34B8 / 0x82DE2130, VERIFIED): a cheer when the class has cheers (CrowdExcitement 1,
//! as on every gamedb event; the free-roam value is UNVERIFIED), else the idle.
//!
//! Walkers (the 5 festival objects with paths) move along their Bézier paths at the walk clip's pace,
//! 1 per 12 m of path (GUESS), each a card far away and an animated figure within `near`.
//!
//! Not done: per-class random size (`randomsize`, applied in the VMX128 draw code, not traced) and the
//! game's 3-frame clip blend.

// Cards never cast: CrowdMaterial has no shadow pass, and without the marker the remaster's Bevy cascades marked every
// card in range visible (P2 count 5,634 vs faithful 2,866).
use bevy::light::NotShadowCaster;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::primitives::Aabb;
use bevy::mesh::skinning::{SkinnedMesh, SkinnedMeshInverseBindposes};
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology, VertexAttributeValues};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError, TextureFormat};
use bevy::shader::ShaderRef;
use fh1_formats::crowd::anim;
use fh1_render::car_material::{FxRawOutput, FxRawStandard};

/// Objects are spawned when the camera is within this distance of their bounds (m).
const RANGE: f32 = 600.0;
/// Extra distance before an object is removed again (hysteresis).
const KEEP: f32 = 50.0;
/// Card size (m): the atlas cells are 64x128 px and a standing figure fills ~105 px of the 128
/// (feet ~6 px above the cell bottom); 2.15 m cells give 1.75 m people (GUESS, no scale in the files).
const CARD_W: f32 = 1.075;
const CARD_H: f32 = 2.15;
const FOOT: f32 = -0.1;
/// TrackSettings.xml `<Crowds>` values used when the file isn't installed (Colorado's): SpriteDistance (m)
/// and the lod0..lod3 model counts.
const SPRITE_DISTANCE: f32 = 60.0;
const LOD_COUNTS: [usize; 4] = [5, 20, 50, 125];
/// Slots in the game's per-class idle / cheer pick tables.
const PICK_SLOTS: usize = 50;

mod figures_gpu;
mod walk_gpu;

pub use figures_gpu::GpuFigure;
pub use walk_gpu::WalkerMaterial;

pub struct CrowdPlugin;

impl Plugin for CrowdPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "crowd.wgsl");
        embedded_asset!(app, "crowd_walk.wgsl");
        embedded_asset!(app, "crowd_skin.wgsl");
        embedded_asset!(app, "crowd_skin_prepass.wgsl");
        app.add_plugins((
            MaterialPlugin::<CrowdMaterial>::default(),
            MaterialPlugin::<walk_gpu::WalkerMaterial>::default(),
            MaterialPlugin::<figures_gpu::FigureMaterial>::default(),
        ))
            .add_systems(Update, (stream, animate).chain());
    }
}

#[derive(Clone, Copy, ShaderType, Debug)]
pub struct CrowdParams {
    /// x = brightness, y = alpha cut, z = 1: write sqrt(colour), w = heading sign.
    pub p: Vec4,
    /// x = cells per row, y = rows, z = fade start (m), w = fade end (m).
    pub atlas: Vec4,
    /// x = no card within this distance (the models take over), yzw unused.
    pub lod: Vec4,
    /// rgb = the light on the cards (`card_light`), w unused.
    pub light: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct CrowdMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,
    #[uniform(2)]
    pub params: CrowdParams,
}

impl Material for CrowdMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/crowd.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/crowd.wgsl".into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Mask(0.5)
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
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(5),
        ])?];
        d.primitive.cull_mode = None;
        Ok(())
    }
}

struct Object {
    first: usize,
    count: usize,
    min: Vec3,
    max: Vec3,
}

#[derive(Clone, Default)]
struct Class {
    sitting: bool,
    /// Model file suffix of the class's skeleton (`_Sit` / `_Stand`).
    suffix: String,
    /// Atlas model indices the class picks from (its modelset).
    models: Vec<u32>,
    /// Animation names: the idle first, then the cheers.
    anims: Vec<String>,
    idle: Vec<String>,
    cheer: Vec<String>,
}

/// One spectator, decoded from crowds.bin.
#[derive(Clone, Copy)]
struct Spectator {
    position: Vec3,
    heading: u8,
    class: u8,
    /// Atlas model index (also the 3D model).
    model: u32,
}

struct CrowdWorld {
    dir: PathBuf,
    /// `anims/` and `models/` files read ahead on a thread (relative path -> bytes; [`preload`]).
    preloaded: Preloaded,
    blob: Vec<u8>,
    objects: Vec<Object>,
    classes: Vec<Class>,
    /// Atlas order: (model name, texture name).
    models: Vec<(String, String)>,
    per_model: u32,
    material: Handle<CrowdMaterial>,
    loaded: Vec<Option<Entity>>,
    /// PERF P2: card objects merged per `CELL` m grid cell (one mesh + entity per cell instead of per object; the
    /// cards were ~2.2k of the festival's ~3.8k visible draws). Bounds (first/count unused), member objects, entity.
    cells: Vec<(Object, Vec<usize>, Option<Entity>)>,
    /// Per-object cards + per-walker entities (default) instead of cells + the merged walker mesh (FH1_CROWD_MERGED=1).
    per_object: bool,
    post: bool,
    near: f32,
    heading_sign: f32,
    model_yaw: f32,
    /// Model count per skinbin LOD (TrackSettings lod0..lod3).
    lod_counts: [usize; 4],
    /// Cards cut / models drawn within this distance this frame (`near`, less when the model cap is hit).
    cut: f32,
    /// (model name + suffix, LOD) -> mesh (None if missing).
    meshes: HashMap<(String, usize), Option<Handle<Mesh>>>,
    /// Per class: the 50-slot clip table the game picks from when a clip ends.
    tables: HashMap<u8, Option<ClipTable>>,
    textures: HashMap<String, MaterialHandle>,
    /// Spawned 3D spectators by global index.
    near_loaded: HashMap<usize, Figure>,
    /// Decoded clips by animation name (None if missing / undecodable).
    clips: HashMap<String, Option<Arc<(anim::Skeleton, anim::Clip)>>>,
    /// Inverse bind poses per skeleton suffix.
    bindposes: HashMap<String, Handle<SkinnedMeshInverseBindposes>>,
    /// Walker paths (engine space) and the walker entities of each active path.
    paths: Vec<fh1_formats::crowd::WalkerPath>,
    path_loaded: Vec<Option<Vec<Entity>>>,
    /// One card mesh per atlas model, standing, at the local origin (walkers move it by Transform).
    walker_cards: HashMap<u32, Handle<Mesh>>,
    /// Walking speed (m/s), from the walk clip.
    walk_speed: f32,
    /// PERF P2 walkers (default): per path, its walkers while loaded. All cards go into one dynamic mesh rebuilt each
    /// frame (`walk_mesh`); only walkers within `near` get entities (their 3D figure). The old per-walker entities
    /// (a moving parent + card child, ~11k entities at the festival) remain the default; FH1_CROWD_MERGED=1 opts in.
    walk: Vec<Option<Vec<WalkerState>>>,
    /// The walkers' card mesh and its entity; quads currently in its index buffer.
    walk_mesh: Option<(Handle<Mesh>, Entity)>,
    walk_quads: usize,
    /// GPU walkers (default; walk_gpu.rs): baked paths, their material and per-path card entities.
    gpu_walk: bool,
    gpu: walk_gpu::GpuWalk,
    /// GPU figures (default in the remaster; figures_gpu.rs) and the engine clock they run on this frame.
    gpu_figures: bool,
    gpu_fig: figures_gpu::GpuFigures,
    gpu_now: f32,
}

/// A walker in the merged path (P2): its offset along the path, atlas model, seed and 3D figure root.
struct WalkerState {
    offset: f32,
    model: u32,
    seed: u32,
    figure: Option<Entity>,
}

/// A walker on a path (`fh1_formats::crowd::WalkerPath`); its card is a child, its 3D figure too when near.
#[derive(Component)]
struct Walker {
    path: usize,
    offset: f32,
    model: u32,
    seed: u32,
    figure: Option<Entity>,
}

type ClipArc = Arc<(anim::Skeleton, anim::Clip)>;
type ClipTable = Arc<Vec<ClipArc>>;

/// A spawned 3D spectator: root and body (mesh) entities, mesh name and its LOD.
struct Figure {
    root: Entity,
    body: Entity,
    name: String,
    lod: usize,
}

/// A 3D spectator playing a clip: its joint entities in bone order, the class's pick table, when the
/// current clip started (set on the first frame, `phase` seconds in) and its random state.
#[derive(Component)]
struct CrowdAnimated {
    clip: ClipArc,
    table: ClipTable,
    joints: Vec<Entity>,
    start: Option<f32>,
    phase: f32,
    rng: u32,
}

#[derive(Clone)]
enum MaterialHandle {
    Raw(Handle<FxRawStandard>),
    Standard(Handle<StandardMaterial>),
}

/// Crowd clip and model files (~7 MB) read on a background thread when the crowd loads, so spawning spectators near the
/// car never reads a file inside the frame (2026-10-08 freezes: every stall site was a file read). FH1_CROWD_PRELOAD=0 =
/// read on first use, as before.
type Preloaded = Arc<std::sync::Mutex<HashMap<String, Arc<Vec<u8>>>>>;

fn preload(root: &Path) -> Preloaded {
    let out: Preloaded = Default::default();
    if std::env::var("FH1_CROWD_PRELOAD").is_ok_and(|v| v == "0") {
        return out;
    }
    let (root, fill) = (root.to_owned(), out.clone());
    let _ = std::thread::Builder::new().name("fh1-crowd-preload".into()).spawn(move || {
        for sub in ["anims", "models"] {
            for e in std::fs::read_dir(root.join(sub)).into_iter().flatten().flatten() {
                let Some(name) = e.file_name().to_str().map(|n| format!("{sub}/{n}")) else { continue };
                if let Ok(b) = std::fs::read(e.path()) {
                    if let Ok(mut m) = fill.lock() {
                        m.insert(name, Arc::new(b));
                    }
                }
            }
        }
    });
    out
}

impl CrowdWorld {
    /// A crowd file (path under `crowd/`): from the preload when it got there, else from disk.
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        if let Some(b) = self.preloaded.lock().ok().and_then(|m| m.get(rel).cloned()) {
            return Some(b.to_vec());
        }
        std::fs::read(self.dir.join(rel)).ok()
    }

    fn load(assets: &Path, post: bool, images: &mut Assets<Image>, materials: &mut Assets<CrowdMaterial>) -> Option<Self> {
        let root = assets.join("crowd");
        let dir = root.join("colorado");
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).ok()?).ok()?;
        let blob = std::fs::read(dir.join("crowds.bin")).ok()?;
        if blob.get(..8)? != b"FH1CRWD1" {
            return None;
        }
        let mode = std::env::var("FH1_CROWD").unwrap_or_default();
        let all = mode.eq_ignore_ascii_case("all");
        let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let v3 = |v: &serde_json::Value| Some(Vec3::new(v[0].as_f64()? as f32, v[1].as_f64()? as f32, v[2].as_f64()? as f32));
        let objects: Vec<Object> = index["objects"]
            .as_array()?
            .iter()
            .filter(|o| all || o["named"].as_bool() == Some(true))
            // FH1_CROWD=walkers: the walkers alone (for checking them).
            .filter(|_| !mode.eq_ignore_ascii_case("walkers"))
            .filter_map(|o| Some(Object { first: o["first"].as_u64()? as usize, count: o["count"].as_u64()? as usize, min: v3(&o["min"])?, max: v3(&o["max"])? }))
            .collect();
        let mut classes = vec![Class::default(); 32];
        for c in index["classes"].as_array()? {
            let Some(i) = c["index"].as_u64().map(|i| i as usize).filter(|&i| i < classes.len()) else { continue };
            classes[i] = Class {
                sitting: c["sitting"].as_bool().unwrap_or(false),
                suffix: c["suffix"].as_str().unwrap_or("").to_owned(),
                models: c["models"].as_array().map(|m| m.iter().filter_map(|x| x.as_u64().map(|x| x as u32)).collect()).unwrap_or_default(),
                anims: ["idle", "cheer"].iter().flat_map(|k| c[*k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned))).collect(),
                idle: c["idle"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
                cheer: c["cheer"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
            };
        }
        let models = index["models"]
            .as_array()
            .map(|m| m.iter().map(|x| (x["name"].as_str().unwrap_or("").to_owned(), x["texture"].as_str().unwrap_or("").to_owned())).collect())
            .unwrap_or_default();
        let atlas = &index["atlas"];
        let cell = atlas["cell"].as_array()?;
        let (cw, ch) = (cell[0].as_f64()? as f32, cell[1].as_f64()? as f32);
        let (cols, rows) = (atlas["width"].as_f64()? as f32 / cw, atlas["height"].as_f64()? as f32 / ch);
        let image = srgb(fh1_render::scenery::read_dds(&dir.join(atlas["file"].as_str()?))?);
        let (sprite_distance, lod_counts) = track_crowd_settings(assets);
        let near = env("FH1_CROWD_NEAR", sprite_distance);
        let heading_sign = env("FH1_CROWD_HEADING_SIGN", 1.0);
        let material = materials.add(CrowdMaterial {
            texture: images.add(image),
            params: CrowdParams {
                p: Vec4::new(env("FH1_CROWD_BRIGHTNESS", 1.0), 0.5, if post { 1.0 } else { 0.0 }, heading_sign),
                atlas: Vec4::new(cols, rows, RANGE - 60.0, RANGE),
                lod: Vec4::new(near, 0.0, 0.0, 0.0),
                light: Vec4::ONE,
            },
        });
        // Walker paths of the drawn objects (stored in engine space; Bézier evaluation doesn't care).
        let walkers: serde_json::Value = std::fs::read(dir.join("walkers.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let f3 = |v: &serde_json::Value| [0, 1, 2].map(|i| v[i].as_f64().unwrap_or(0.0) as f32);
        let paths: Vec<fh1_formats::crowd::WalkerPath> = walkers["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| all || p["named"].as_bool() == Some(true))
            .map(|p| fh1_formats::crowd::WalkerPath {
                class: p["class"].as_u64().unwrap_or(3) as u32,
                bbox_min: f3(&p["min"]),
                bbox_max: f3(&p["max"]),
                length: p["length"].as_f64().unwrap_or(0.0) as f32,
                knots: p["knots"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|k| fh1_formats::crowd::Knot {
                        centre: f3(&k["c"]),
                        in_handle: f3(&k["i"]),
                        out_handle: f3(&k["o"]),
                        distances: {
                            let mut d = [0f32; 8];
                            for (j, x) in d.iter_mut().enumerate() {
                                *x = k["d"][j].as_f64().unwrap_or(0.0) as f32;
                            }
                            d
                        },
                    })
                    .collect(),
            })
            .collect();
        let total: usize = objects.iter().map(|o| o.count).sum();
        info!("crowd: {} objects, {total} spectators, {} walker paths{}, models within {near} m", objects.len(), paths.len(), if all { "" } else { " (named crowd_* only; FH1_CROWD=all for event crowds)" });
        let n = objects.len();
        Some(Self {
            preloaded: preload(&root),
            dir: root,
            blob,
            objects,
            classes,
            models,
            per_model: atlas["per_model"].as_u64().unwrap_or(9) as u32,
            material,
            loaded: vec![None; n],
            cells: Vec::new(),
            // Opt-in (FH1_CROWD_MERGED=1): the in-process A/B (FH1_CROWD_AB=1) measured no frame-time gain at the festival
            // (merged 25.9 vs old 24.8 ms median, render-world bound elsewhere), though it removes ~11k entities.
            // Since the GPU walkers (2026-10-08) the cards are merged per cell by default too (the A/B above included the
            // per-frame walker mesh rewrite): FH1_CROWD_CELLS=0 = per-object cards; FH1_CROWD_GPU_WALK=0 = everything as before.
            per_object: if std::env::var("FH1_CROWD_MERGED").is_ok_and(|v| v == "1") {
                false
            } else {
                std::env::var("FH1_CROWD_CELLS").is_ok_and(|v| v == "0") || !walk_gpu::enabled()
            },
            post,
            near,
            heading_sign,
            // The skinned figures face +Z in their own space: with 0 the barrier leaners showed their backs and leaned
            // away from the barrier and walkers walked backwards (user report; close A/B shot at the festival barrier,
            // 2026-10-06). FH1_CROWD_FACING_OLD=1 = the old 0; FH1_CROWD_MODEL_YAW still overrides.
            model_yaw: env("FH1_CROWD_MODEL_YAW", if std::env::var("FH1_CROWD_FACING_OLD").is_ok_and(|v| v == "1") { 0.0 } else { 180.0 }).to_radians(),
            lod_counts,
            cut: near,
            meshes: HashMap::new(),
            tables: HashMap::new(),
            textures: HashMap::new(),
            near_loaded: HashMap::new(),
            clips: HashMap::new(),
            bindposes: HashMap::new(),
            path_loaded: vec![None; paths.len()],
            walk: (0..paths.len()).map(|_| None).collect(),
            walk_mesh: None,
            walk_quads: 0,
            gpu_walk: walk_gpu::enabled(),
            gpu: Default::default(),
            // The faithful renderer's figures use FxRawStandard (the FH1 post encoding): CPU figures there.
            gpu_figures: figures_gpu::enabled() && !post,
            gpu_fig: Default::default(),
            gpu_now: 0.0,
            paths,
            walker_cards: HashMap::new(),
            walk_speed: 1.3,
        })
    }

    fn spectator(&self, i: usize) -> Spectator {
        let r = &self.blob[12 + i * 16..][..16];
        let f = |o: usize| f32::from_le_bytes(r[o..o + 4].try_into().unwrap());
        let class = r[13];
        let c = self.classes.get(class as usize);
        // Model: a fixed pick from the class's modelset (the game's random choice is not reproduced).
        let h = (i as u32).wrapping_mul(2_654_435_761) >> 16;
        let model = c.filter(|c| !c.models.is_empty()).map_or(0, |c| c.models[h as usize % c.models.len()]);
        Spectator { position: Vec3::new(f(0), f(4), f(8)), heading: r[12], class, model }
    }

    /// One card mesh for objects `objs` (one object, or a P2 cell of them), positions relative to `bounds.min`.
    fn build(&self, objs: &[usize], bounds: &Object) -> (Mesh, Aabb) {
        let n: usize = objs.iter().map(|&i| self.objects[i].count).sum();
        let (mut pos, mut uv, mut corner, mut data) = (Vec::with_capacity(4 * n), Vec::with_capacity(4 * n), Vec::with_capacity(4 * n), Vec::with_capacity(4 * n));
        let mut idx = Vec::with_capacity(6 * n);
        for i in objs.iter().flat_map(|&i| self.objects[i].first..self.objects[i].first + self.objects[i].count) {
            let s = self.spectator(i);
            let p = s.position - bounds.min;
            let sitting = self.classes.get(s.class as usize).is_some_and(|c| c.sitting);
            let cell = (s.model * self.per_model) as f32;
            // w = 1: a 3D model replaces the card within `near`.
            let modelled = self.near > 0.0 && self.has_model(&s);
            let d = [s.heading as f32 / 256.0 * std::f32::consts::TAU, cell, if sitting { 1.0 } else { 0.0 }, if modelled { 1.0 } else { 0.0 }];
            let i = pos.len() as u32;
            let w = CARD_W * 0.5;
            for (cr, u) in [([-w, FOOT], [0.0, 1.0]), ([w, FOOT], [1.0, 1.0]), ([w, FOOT + CARD_H], [1.0, 0.0]), ([-w, FOOT + CARD_H], [0.0, 0.0])] {
                pos.push(p.to_array());
                corner.push(cr);
                uv.push(u);
                data.push(d);
            }
            idx.extend_from_slice(&[i, i + 1, i + 2, i, i + 2, i + 3]);
        }
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, corner);
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, data);
        mesh.insert_indices(Indices::U32(idx));
        let pad = Vec3::new(CARD_H, CARD_H + 1.0, CARD_H);
        (mesh, Aabb::from_min_max(-pad, bounds.max - bounds.min + pad))
    }

    /// Whether a spectator gets a 3D model (its class names a skeleton and an animation).
    fn has_model(&self, s: &Spectator) -> bool {
        self.classes.get(s.class as usize).is_some_and(|c| !c.suffix.is_empty() && !c.anims.is_empty())
    }

    fn clip(&mut self, name: &str) -> Option<Arc<(anim::Skeleton, anim::Clip)>> {
        if let Some(c) = self.clips.get(name) {
            return c.clone();
        }
        let c = self.read(&format!("anims/{name}.anim.bin")).and_then(|d| anim::parse(&d).map_err(|e| warn!("crowd: {name}: {e}")).ok()).map(Arc::new);
        self.clips.insert(name.to_owned(), c.clone());
        c
    }

    fn bindposes(&mut self, suffix: &str, skel: &anim::Skeleton, assets: &mut Assets<SkinnedMeshInverseBindposes>) -> Handle<SkinnedMeshInverseBindposes> {
        self.bindposes
            .entry(suffix.to_owned())
            // Bind rotations are identity (the stored world positions are the sums of the locals).
            .or_insert_with(|| assets.add(SkinnedMeshInverseBindposes::from(skel.bones.iter().map(|b| Mat4::from_translation(-Vec3::from_array(b.world))).collect::<Vec<_>>())))
            .clone()
    }

    fn mesh(&mut self, name: &str, lod: usize, meshes: &mut Assets<Mesh>) -> Option<Handle<Mesh>> {
        let key = (name.to_owned(), lod);
        if let Some(m) = self.meshes.get(&key) {
            return m.clone();
        }
        let m = (|| {
            let d = self.read(&format!("models/{name}.skinbin"))?;
            let sk = fh1_formats::crowd::parse_skinbin(&d).ok()?;
            let range = sk.lods.get(lod).or(sk.lods.last())?.clone();
            let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
            mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, sk.vertices.iter().map(|v| v.position).collect::<Vec<_>>());
            mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, sk.vertices.iter().map(|v| v.normal).collect::<Vec<_>>());
            mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, sk.vertices.iter().map(|v| v.uv).collect::<Vec<_>>());
            mesh.insert_attribute(Mesh::ATTRIBUTE_JOINT_INDEX, VertexAttributeValues::Uint16x4(sk.vertices.iter().map(|v| [v.bones[0] as u16, v.bones[1] as u16, 0, 0]).collect()));
            mesh.insert_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT, sk.vertices.iter().map(|v| [v.weights[0] as f32 / 255.0, v.weights[1] as f32 / 255.0, 0.0, 0.0]).collect::<Vec<_>>());
            mesh.insert_indices(Indices::U32(sk.indices[range].iter().map(|&i| i as u32).collect()));
            Some(meshes.add(mesh))
        })();
        if m.is_none() {
            warn!("crowd: model {name} LOD {lod} missing");
        }
        self.meshes.insert(key, m.clone());
        m
    }

    /// The class's 50-slot pick table (0x82DE34B8): its cheers if it has any (CrowdExcitement 1), else its
    /// idles, each filling `weight / total * 50` slots by a running sum, leftover slots repeating the first.
    /// Every spectators.xml weight is 1.0 (VERIFIED), so equal weights here.
    fn table(&mut self, class: u8) -> Option<ClipTable> {
        if let Some(t) = self.tables.get(&class) {
            return t.clone();
        }
        let c = self.classes.get(class as usize)?.clone();
        let names = if c.cheer.is_empty() { &c.idle } else { &c.cheer };
        let clips: Vec<ClipArc> = names.iter().filter_map(|n| self.clip(n)).collect();
        let t = (!clips.is_empty()).then(|| {
            let mut slots = Vec::with_capacity(PICK_SLOTS);
            let mut acc = 0.0f32;
            for clip in &clips {
                acc += PICK_SLOTS as f32 / clips.len() as f32;
                while acc > 0.0 && slots.len() < PICK_SLOTS {
                    acc -= 1.0;
                    slots.push(clip.clone());
                }
            }
            slots.resize(PICK_SLOTS, clips[0].clone());
            Arc::new(slots)
        });
        self.tables.insert(class, t.clone());
        t
    }

    /// Skinbin LOD of the `rank`-th nearest model (0-based): LOD k takes the next `lod_counts[k]` models.
    fn lod_for_rank(&self, rank: usize) -> Option<usize> {
        let mut end = 0;
        for (k, n) in self.lod_counts.iter().enumerate() {
            end += n;
            if rank < end {
                return Some(k);
            }
        }
        None
    }

    fn texture(&mut self, name: &str, images: &mut Assets<Image>, raw: &mut Assets<FxRawStandard>, std_mats: &mut Assets<StandardMaterial>) -> Option<MaterialHandle> {
        if let Some(m) = self.textures.get(name) {
            return Some(m.clone());
        }
        let img = srgb(fh1_render::scenery::read_dds(&self.dir.join("textures").join(format!("{name}.dds")))?);
        let base = StandardMaterial { base_color_texture: Some(images.add(img)), perceptual_roughness: 0.9, alpha_mode: AlphaMode::Mask(0.5), double_sided: true, cull_mode: None, ..default() };
        let m = if self.post { MaterialHandle::Raw(raw.add(FxRawStandard { base, extension: FxRawOutput {} })) } else { MaterialHandle::Standard(std_mats.add(base)) };
        self.textures.insert(name.to_owned(), m.clone());
        Some(m)
    }
}

/// Asset stores a 3D figure needs.
struct Assets3d<'a> {
    meshes: &'a mut Assets<Mesh>,
    images: &'a mut Assets<Image>,
    raw: &'a mut Assets<FxRawStandard>,
    std_mats: &'a mut Assets<StandardMaterial>,
    bindposes: &'a mut Assets<SkinnedMeshInverseBindposes>,
    buffers: &'a mut Assets<bevy::render::storage::ShaderBuffer>,
    fig_mats: &'a mut Assets<figures_gpu::FigureMaterial>,
}

impl CrowdWorld {
    /// Spawns a skinned spectator (model `model` of the atlas order, skinbin LOD `lod`, the class's skeleton)
    /// playing clips from the class's pick table, the first picked by `seed` from a seed phase. Returns the
    /// figure (root + body entities).
    fn spawn_figure(&mut self, commands: &mut Commands, a: &mut Assets3d, seed: u32, class: u8, model: u32, lod: usize, t: Transform) -> Option<Figure> {
        if self.gpu_figures {
            if let Some(f) = self.spawn_gpu_figure(commands, a, seed, class, model, lod, t) {
                return Some(f);
            }
        }
        let (name, tex) = self.models.get(model as usize).cloned()?;
        let c = self.classes.get(class as usize)?.clone();
        if c.anims.is_empty() {
            return None;
        }
        let mesh_name = format!("{name}{}", c.suffix);
        let mesh = self.mesh(&mesh_name, lod, a.meshes)?;
        let mat = self.texture(&tex, a.images, a.raw, a.std_mats)?;
        let h = seed.wrapping_mul(0x9E37_79B9).rotate_left(13);
        let table = self.table(class)?;
        let clip = table[h as usize % table.len()].clone();
        let inv = self.bindposes(&c.suffix, &clip.0, a.bindposes);
        let root = commands.spawn((t, crate::ui::world_load::WorldEntity)).id();
        // Joints in bone order; parents precede children in the files.
        let mut joints: Vec<Entity> = Vec::with_capacity(clip.0.bones.len());
        for b in &clip.0.bones {
            let parent = b.parent.and_then(|p| joints.get(p as usize).copied()).unwrap_or(root);
            let j = commands.spawn((Transform::from_translation(Vec3::from_array(b.local)), crate::ui::world_load::WorldEntity)).id();
            commands.entity(parent).add_child(j);
            joints.push(j);
        }
        let skinned = SkinnedMesh { inverse_bindposes: inv, joints: joints.clone() };
        let body = match mat {
            MaterialHandle::Raw(m) => commands.spawn((Mesh3d(mesh), MeshMaterial3d(m), skinned, Transform::IDENTITY, crate::ui::world_load::WorldEntity)).id(),
            MaterialHandle::Standard(m) => commands.spawn((Mesh3d(mesh), MeshMaterial3d(m), skinned, Transform::IDENTITY, crate::ui::world_load::WorldEntity)).id(),
        };
        commands.entity(root).add_child(body);
        let phase = (h >> 8) as f32 / (1u32 << 24) as f32 * clip.1.duration.max(0.01);
        commands.entity(root).insert(CrowdAnimated { clip, table, joints, start: None, phase, rng: h | 1 });
        Some(Figure { root, body, name: mesh_name, lod })
    }
}

/// The DDS files are written UNORM; colour textures are sampled as sRGB.
fn srgb(mut image: Image) -> Image {
    image.texture_descriptor.format = match image.texture_descriptor.format {
        TextureFormat::Bc3RgbaUnorm => TextureFormat::Bc3RgbaUnormSrgb,
        TextureFormat::Bc1RgbaUnorm => TextureFormat::Bc1RgbaUnormSrgb,
        f => f,
    };
    image
}

/// Horizontal distance from `p` to the object's bounds.
/// Card cell size (m), PERF P2.
const CELL: f32 = 64.0;
/// FH1_CROWD_AB seconds per mode.
const AB_SECS: f64 = 5.0;

#[derive(Default)]
struct CrowdAb {
    start: f64,
    /// Frame times (ms): [cells, per-object].
    frames: [Vec<f32>; 2],
}

/// Groups the card objects by the `CELL` grid cell of their bounds' minimum: (union bounds, members, entity).
fn cells(objects: &[Object]) -> Vec<(Object, Vec<usize>, Option<Entity>)> {
    let mut by: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, o) in objects.iter().enumerate() {
        by.entry(((o.min.x / CELL).floor() as i32, (o.min.z / CELL).floor() as i32)).or_default().push(i);
    }
    let mut keys: Vec<_> = by.keys().copied().collect();
    keys.sort();
    keys.into_iter()
        .map(|k| {
            let members = by.remove(&k).unwrap();
            let (min, max) = members.iter().fold((Vec3::MAX, Vec3::MIN), |(lo, hi), &i| (lo.min(objects[i].min), hi.max(objects[i].max)));
            (Object { first: 0, count: 0, min, max }, members, None)
        })
        .collect()
}

fn distance(o: &Object, p: Vec3) -> f32 {
    let c = Vec2::new(p.x.clamp(o.min.x, o.max.x), p.z.clamp(o.min.z, o.max.z));
    c.distance(Vec2::new(p.x, p.z))
}

#[allow(clippy::too_many_arguments)]
fn stream(
    mut commands: Commands,
    mut world: Local<Option<CrowdWorld>>,
    // World generation the cache was built for (X1c: an in-process map change drops it; its entities are WorldEntity).
    mut tried: Local<Option<u32>>,
    (garage, generation): (Res<crate::Garage>, Res<crate::ui::world_load::WorldGeneration>),
    scenery: Option<Res<crate::scenery::Scenery>>,
    post: Option<Res<fh1_render::postfx::FxPostConfig>>,
    (cameras, suns, ambient, remaster): (
        Query<(&GlobalTransform, Option<&bevy::camera::Exposure>), With<fh1_render::post::FxPostCamera>>,
        Query<(&DirectionalLight, &GlobalTransform)>,
        Option<Res<GlobalAmbientLight>>,
        Option<Res<fh1_remaster::light::RemasterLighting>>,
    ),
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<CrowdMaterial>>,
    (mut raw, mut std_mats, mut bindposes): (ResMut<Assets<FxRawStandard>>, ResMut<Assets<StandardMaterial>>, ResMut<Assets<SkinnedMeshInverseBindposes>>),
    (mut buffers, mut walk_mats, mut fig_mats, alive): (
        ResMut<Assets<bevy::render::storage::ShaderBuffer>>,
        ResMut<Assets<walk_gpu::WalkerMaterial>>,
        ResMut<Assets<figures_gpu::FigureMaterial>>,
        Query<(), With<figures_gpu::GpuFigure>>,
    ),
    time: Res<Time>,
    mut walkers: Query<(Entity, &mut Walker, &mut Transform)>,
    mut ab: Local<Option<Option<CrowdAb>>>,
) {
    let ab = ab.get_or_insert_with(|| std::env::var("FH1_CROWD_AB").is_ok_and(|v| v == "1").then(CrowdAb::default));
    // Colorado only (the scenery resource exists when the track has a converted world; FH2's Anthem has no crowd data).
    if !scenery.as_ref().is_some_and(|s| s.colorado) {
        return;
    }
    if *tried != Some(generation.0) {
        *tried = Some(generation.0);
        *world = None;
        if std::env::var("FH1_CROWD").is_ok_and(|v| v == "0") {
            info!("crowd: off (FH1_CROWD=0)");
            return;
        }
        // The FH1 post chain (sqrt-encoded output) runs only in the faithful renderer; FxPostConfig also exists in the
        // remaster, whose figures then came out sqrt-brightened (pale at night).
        *world = CrowdWorld::load(&garage.assets, post.is_some() && !fh1_remaster::enabled(), &mut images, &mut materials);
        if world.is_none() {
            info!("crowd: not installed (run fh1setup --only crowd)");
        }
    }
    let (Some(w), Some((cam, exposure))) = (world.as_mut(), cameras.iter().next()) else { return };
    w.gpu_now = figures_gpu::clock(&time);
    let here = cam.translation();
    let light = card_light(&suns, ambient.as_deref(), remaster.as_deref(), exposure.copied().unwrap_or_default());
    // Only touch the asset when the light moved (get_mut marks it changed = a re-upload).
    let stale = materials.get(&w.material).is_some_and(|m| {
        let old = m.params.light.truncate();
        (old - light).abs().max_element() > 0.01 * old.max_element().max(0.05)
    });
    if stale {
        if let Some(mut m) = materials.get_mut(&w.material) {
            m.params.light = light.extend(1.0);
        }
    }

    if w.cells.is_empty() && !w.objects.is_empty() {
        w.cells = cells(&w.objects);
        info!("crowd: {} card objects in {} cells of {CELL} m", w.objects.len(), w.cells.len());
    }
    // FH1_CROWD_AB=1: in-process A/B, cells vs per-object cards, switching every AB_SECS (frame means per mode).
    if let Some(ab) = ab.as_mut() {
        let now = time.elapsed_secs_f64();
        if ab.start == 0.0 {
            ab.start = now;
        }
        let phase = ((now - ab.start) / AB_SECS) as u64;
        if phase >= 2 {
            ab.frames[w.per_object as usize].push(time.delta_secs() * 1000.0);
        }
        let want = phase % 2 == 1;
        if want != w.per_object {
            // Despawn everything of the old mode; the new one streams in below.
            for e in w.loaded.iter_mut().chain(w.cells.iter_mut().map(|c| &mut c.2)).filter_map(Option::take) {
                commands.entity(e).despawn();
            }
            for e in w.path_loaded.iter_mut().filter_map(Option::take).flatten() {
                commands.entity(e).despawn();
            }
            for f in w.walk.iter_mut().filter_map(Option::take).flatten().filter_map(|k| k.figure) {
                commands.entity(f).despawn();
            }
            if let Some((_, e)) = w.walk_mesh.take() {
                commands.entity(e).despawn();
            }
            walk_gpu::clear(&mut commands, w);
            w.walk_quads = 0;
            w.per_object = want;
            if phase >= 3 && phase % 2 == 1 {
                let m = |f: &[f32]| f.iter().sum::<f32>() / f.len().max(1) as f32;
                let med = |f: &[f32]| {
                    let mut v = f.to_vec();
                    v.sort_by(f32::total_cmp);
                    v.get(v.len() / 2).copied().unwrap_or(0.0)
                };
                info!(
                    "crowd A/B (merged walkers + card cells vs per-object/per-walker entities), median/mean ms: merged {:.2}/{:.2} ({}), old {:.2}/{:.2} ({})",
                    med(&ab.frames[0]),
                    m(&ab.frames[0]),
                    ab.frames[0].len(),
                    med(&ab.frames[1]),
                    m(&ab.frames[1]),
                    ab.frames[1].len()
                );
            }
        }
    }
    // Far: card meshes, one per cell (default) or per object.
    if w.per_object {
        for i in 0..w.objects.len() {
            let d = distance(&w.objects[i], here);
            match w.loaded[i] {
                Some(e) if d > RANGE + KEEP => {
                    commands.entity(e).despawn();
                    w.loaded[i] = None;
                }
                None if d <= RANGE => {
                    let (mesh, aabb) = w.build(&[i], &w.objects[i]);
                    let e = commands.spawn((Mesh3d(meshes.add(mesh)), MeshMaterial3d(w.material.clone()), aabb, Transform::from_translation(w.objects[i].min), NotShadowCaster, crate::ui::world_load::WorldEntity)).id();
                    w.loaded[i] = Some(e);
                }
                _ => {}
            }
        }
    } else {
        for c in 0..w.cells.len() {
            let d = distance(&w.cells[c].0, here);
            match w.cells[c].2 {
                Some(e) if d > RANGE + KEEP => {
                    commands.entity(e).despawn();
                    w.cells[c].2 = None;
                }
                None if d <= RANGE => {
                    let (mesh, aabb) = w.build(&w.cells[c].1, &w.cells[c].0);
                    let e = commands.spawn((Mesh3d(meshes.add(mesh)), MeshMaterial3d(w.material.clone()), aabb, Transform::from_translation(w.cells[c].0.min), NotShadowCaster, crate::ui::world_load::WorldEntity)).id();
                    w.cells[c].2 = Some(e);
                }
                _ => {}
            }
        }
    }

    let mut a3 = Assets3d { meshes: &mut meshes, images: &mut images, raw: &mut raw, std_mats: &mut std_mats, bindposes: &mut bindposes, buffers: &mut buffers, fig_mats: &mut fig_mats };
    if w.gpu_walk {
        let (texture, params) = materials.get(&w.material).map(|m| (m.texture.clone(), m.params)).unzip();
        if let (Some(texture), Some(params)) = (texture, params) {
            let mut g = walk_gpu::GpuAssets { materials: &mut walk_mats, texture, params };
            walk_gpu::stream(&mut commands, w, here, time.elapsed_secs(), &mut a3, &mut g);
        }
    } else if w.per_object {
        stream_walkers(&mut commands, w, here, time.elapsed_secs(), &mut walkers, &mut a3);
    } else {
        stream_walkers_merged(&mut commands, w, here, time.elapsed_secs(), &mut a3);
    }

    // Near: 3D models for the nearest spectators within `near` (SpriteDistance), at most lod0..lod3 of them,
    // nearest first; the cards are cut at the distance the models reach (`cut`).
    // GPU figures: free despawned slots, schedule clips, upload (figures spawned below are written next frame).
    if w.gpu_figures {
        w.gpu_fig.update(w.gpu_now, &alive, &mut buffers);
    }
    if w.near <= 0.0 {
        return;
    }
    let mut near: Vec<(f32, usize)> = Vec::new();
    for o in w.objects.iter().filter(|o| distance(o, here) <= w.near) {
        for i in o.first..o.first + o.count {
            let s = w.spectator(i);
            let d = s.position.distance(here);
            if d <= w.near && w.has_model(&s) {
                near.push((d, i));
            }
        }
    }
    near.sort_by(|a, b| a.0.total_cmp(&b.0));
    let cap: usize = w.lod_counts.iter().sum();
    let cut = if near.len() > cap { near[cap].0 } else { w.near };
    near.truncate(cap);
    if (cut - w.cut).abs() > 0.25 {
        w.cut = cut;
        if let Some(mut m) = materials.get_mut(&w.material) {
            m.params.lod.x = cut;
        }
    }
    let rank: HashMap<usize, usize> = near.iter().enumerate().map(|(r, &(_, i))| (i, r)).collect();
    let gone: Vec<usize> = w.near_loaded.keys().copied().filter(|i| !rank.contains_key(i)).collect();
    for i in gone {
        if let Some(f) = w.near_loaded.remove(&i) {
            commands.entity(f.root).despawn();
        }
    }
    // New 3D figures per frame (P5b): arriving at the festival spawned the whole cap (each ~36 entities with its joints)
    // in one frame. Nearest first, so the closest appear first. FH1_CROWD_SPAWNS=0 = unlimited (old).
    let mut spawn_left = crowd_spawns_per_frame();
    for (r, &(_, i)) in near.iter().enumerate() {
        let Some(lod) = w.lod_for_rank(r) else { continue };
        if let Some(f) = w.near_loaded.get(&i) {
            // Moved to another LOD band: swap the body's mesh.
            if f.lod != lod {
                let name = f.name.clone();
                // GPU figures are one entity (root = body) with their own mesh variant.
                let m = if f.root == f.body { w.gpu_mesh(&name, lod, &mut meshes) } else { w.mesh(&name, lod, &mut meshes) };
                if let Some(m) = m {
                    let f = w.near_loaded.get_mut(&i).unwrap();
                    commands.entity(f.body).insert(Mesh3d(m));
                    f.lod = lod;
                }
            }
            continue;
        }
        if spawn_left == 0 {
            continue;
        }
        spawn_left -= 1;
        {
            let s = w.spectator(i);
            // Heading in collision space -> engine yaw, plus `model_yaw` (the figures face +Z in their own space).
            let a = s.heading as f32 / 256.0 * std::f32::consts::TAU * w.heading_sign;
            let facing = Vec3::new(a.sin(), 0.0, -a.cos());
            let yaw = (-facing.x).atan2(-facing.z) + w.model_yaw;
            let t = Transform::from_translation(s.position).with_rotation(Quat::from_rotation_y(yaw));
            let mut a3 = Assets3d { meshes: &mut meshes, images: &mut images, raw: &mut raw, std_mats: &mut std_mats, bindposes: &mut bindposes, buffers: &mut buffers, fig_mats: &mut fig_mats };
            let Some(f) = w.spawn_figure(&mut commands, &mut a3, i as u32, s.class, s.model, lod, t) else { continue };
            w.near_loaded.insert(i, f);
        }
    }
}

/// The light on the sprite cards (they were unlit: the same brightness at night as at noon, user report 2026-10-06).
/// Like the 3D figures' Bevy shading of a matte card: exposure x (sun illuminance / pi x colour x the share a standing
/// card gets at the sun's elevation + ambient). Faithful drives those Bevy lights from the TOD tables (fh1-render
/// lighting.rs `update_bevy_lights`, exposure cancels); the remaster's ambient is its environment map, so it uses its
/// ground illuminance estimate (`RemasterLighting::ground_lux`) at its EV instead. FH1_CROWD_CARD_LIGHT=0 = unlit (old).
fn card_light(suns: &Query<(&DirectionalLight, &GlobalTransform)>, ambient: Option<&GlobalAmbientLight>, remaster: Option<&fh1_remaster::light::RemasterLighting>, exposure: bevy::camera::Exposure) -> Vec3 {
    if std::env::var("FH1_CROWD_CARD_LIGHT").is_ok_and(|v| v == "0") {
        return Vec3::ONE;
    }
    if let Some(r) = remaster.filter(|_| fh1_remaster::enabled()) {
        let exposure = 1.0 / (1.2 * 2f32.powf(r.ev100));
        return Vec3::splat(r.ground_lux / std::f32::consts::PI * exposure).clamp(Vec3::ZERO, Vec3::splat(4.0));
    }
    let k = exposure.exposure();
    let mut l = Vec3::ZERO;
    for (sun, t) in suns {
        let elevation = (-t.forward().y).max(0.0);
        let share = 0.5 * (elevation * 4.0).min(1.0);
        l += sun.color.to_linear().to_vec3() * sun.illuminance / std::f32::consts::PI * share * k;
    }
    if let Some(a) = ambient {
        l += a.color.to_linear().to_vec3() * a.brightness * k;
    }
    l.clamp(Vec3::ZERO, Vec3::splat(4.0))
}

/// 3D figures spawned per frame at most (`FH1_CROWD_SPAWNS`, 0 = unlimited).
fn crowd_spawns_per_frame() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| match std::env::var("FH1_CROWD_SPAWNS").ok().and_then(|v| v.parse::<usize>().ok()) {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => 6,
    })
}

/// Plays each 3D spectator's clip: rotations nlerped between the two nearest 30 Hz frames, translations
/// = bind local + the clip's offset (fh1_formats::crowd::anim; the game's own frame interpolation is not
/// traced).
fn animate(time: Res<Time>, mut crowd: Query<&mut CrowdAnimated>, mut joints: Query<&mut Transform>) {
    let now = time.elapsed_secs();
    for mut a in &mut crowd {
        let phase = a.phase;
        let start = *a.start.get_or_insert(now - phase);
        let mut t = now - start;
        // Clip over: the game draws the next one from the class's 50-slot table (0x82DE2130).
        if t >= a.clip.1.duration {
            a.rng = a.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let next = a.table[(a.rng >> 16) as usize % a.table.len()].clone();
            let lead = if a.clip.1.duration > 0.0 { t.rem_euclid(a.clip.1.duration) } else { 0.0 };
            a.clip = next;
            a.start = Some(now - lead);
            t = lead;
        }
        let (skel, clip) = (&a.clip.0, &a.clip.1);
        if clip.frames == 0 || clip.rotations.is_empty() {
            continue;
        }
        let last = clip.rotations.len() - 1;
        let t = t.clamp(0.0, clip.duration.max(0.0));
        let x = t / clip.frame_time.max(1e-4);
        let f0 = (x.floor() as usize).min(last);
        let f1 = (f0 + 1).min(last);
        let k = (x - f0 as f32).clamp(0.0, 1.0);
        for (b, &e) in a.joints.iter().enumerate() {
            let Ok(mut tr) = joints.get_mut(e) else { continue };
            let (q0, q1) = (Quat::from_array(clip.rotations[f0][b]), Quat::from_array(clip.rotations[f1][b]));
            tr.rotation = q0.lerp(if q0.dot(q1) < 0.0 { -q1 } else { q1 }, k).normalize();
            let (p0, p1) = (Vec3::from_array(clip.local_translation(skel, f0, b)), Vec3::from_array(clip.local_translation(skel, f1, b)));
            tr.translation = p0.lerp(p1, k);
        }
    }
}

/// Walkers per metre of path (GUESS: the count is decided at runtime and not traced).
const WALKERS_PER_M: f32 = 1.0 / 12.0;

/// Walkers: spawned per path within `RANGE` (a card each), moved along the path every frame at the walk
/// clip's pace, given an animated 3D figure within `near`.
fn stream_walkers(commands: &mut Commands, w: &mut CrowdWorld, here: Vec3, now: f32, walkers: &mut Query<(Entity, &mut Walker, &mut Transform)>, a: &mut Assets3d) {
    // Walkers class (3) and its clip: one stride cycle is ~1.4 m (the feet swing +-0.35 m; INFERRED).
    if let Some(c) = w.classes.get(3).and_then(|c| c.anims.first().cloned()).and_then(|n| w.clip(&n)) {
        w.walk_speed = 1.4 / c.1.duration.max(0.1);
    }
    // Walkers despawned below this frame: their figure goes with them (it's their child), so the per-walker loop
    // must not touch it again (a second despawn of the same figure = "Entity despawned" command errors at teleports).
    let mut unloaded: Vec<Entity> = Vec::new();
    for i in 0..w.paths.len() {
        let p = &w.paths[i];
        let (length, class, knots) = (p.length, p.class, p.knots.len());
        let c = Vec2::new(here.x.clamp(p.bbox_min[0], p.bbox_max[0]), here.z.clamp(p.bbox_min[2], p.bbox_max[2]));
        let d = c.distance(Vec2::new(here.x, here.z));
        match &w.path_loaded[i] {
            Some(list) if d > RANGE + KEEP => {
                for &e in list {
                    commands.entity(e).despawn();
                    unloaded.push(e);
                }
                w.path_loaded[i] = None;
            }
            None if d <= RANGE && knots >= 2 => {
                let n = ((length * WALKERS_PER_M) as usize).max(1);
                let models = w.classes.get(class as usize).map(|c| c.models.clone()).unwrap_or_default();
                let mut list = Vec::with_capacity(n);
                for k in 0..n {
                    let seed = (i as u32).wrapping_mul(7919).wrapping_add(k as u32).wrapping_mul(2_654_435_761);
                    let model = if models.is_empty() { 0 } else { models[(seed >> 16) as usize % models.len()] };
                    let card = w.walker_card(model, a.meshes);
                    let offset = length * k as f32 / n as f32;
                    let e = commands
                        .spawn((Transform::default(), Visibility::default(), crate::ui::world_load::WorldEntity, Walker { path: i, offset, model, seed, figure: None }))
                        .with_child((Mesh3d(card), MeshMaterial3d(w.material.clone()), Aabb::from_min_max(Vec3::new(-1.0, -0.5, -1.0), Vec3::new(1.0, 2.5, 1.0)), NotShadowCaster))
                        .id();
                    list.push(e);
                }
                w.path_loaded[i] = Some(list);
            }
            _ => {}
        }
    }
    let (near, speed) = (w.near, w.walk_speed);
    let figure_turn = Transform::from_rotation(Quat::from_rotation_y(w.model_yaw));
    for (we, mut wk, mut t) in walkers.iter_mut() {
        if unloaded.contains(&we) {
            continue;
        }
        let Some(p) = w.paths.get(wk.path) else { continue };
        let Some((pos, tan)) = p.point_at(wk.offset + speed * now) else { continue };
        t.translation = Vec3::from_array(pos);
        // Card heading 0 faces -Z locally: turn -Z onto the tangent (the figure child adds `model_yaw`).
        if tan[0].abs() + tan[2].abs() > 1e-4 {
            t.rotation = Quat::from_rotation_y((-tan[0]).atan2(-tan[2]));
        }
        let dist = Vec2::new(pos[0] - here.x, pos[2] - here.z).length();
        match wk.figure {
            Some(f) if dist > near + 8.0 => {
                commands.entity(f).despawn();
                wk.figure = None;
            }
            None if near > 0.0 && dist <= near + 3.0 => {
                // Walkers aren't in the game's group cap (separate paths): LOD0, within `near`.
                if let Some(f) = w.spawn_figure(commands, a, wk.seed, p.class as u8, wk.model, 0, figure_turn) {
                    commands.entity(we).add_child(f.root);
                    wk.figure = Some(f.root);
                }
            }
            _ => {}
        }
    }
}

/// P2 walkers: same spawn rule, speed and figure range as `stream_walkers`, but the cards of all walkers are one
/// mesh rebuilt every frame (foot point + heading per quad, `crowd.wgsl` orients them) and walkers have no entities
/// unless their 3D figure is spawned.
fn stream_walkers_merged(commands: &mut Commands, w: &mut CrowdWorld, here: Vec3, now: f32, a: &mut Assets3d) {
    if let Some(c) = w.classes.get(3).and_then(|c| c.anims.first().cloned()).and_then(|n| w.clip(&n)) {
        w.walk_speed = 1.4 / c.1.duration.max(0.1);
    }
    for i in 0..w.paths.len() {
        let p = &w.paths[i];
        let (length, class, knots) = (p.length, p.class, p.knots.len());
        let c = Vec2::new(here.x.clamp(p.bbox_min[0], p.bbox_max[0]), here.z.clamp(p.bbox_min[2], p.bbox_max[2]));
        let d = c.distance(Vec2::new(here.x, here.z));
        match &w.walk[i] {
            Some(_) if d > RANGE + KEEP => {
                for f in w.walk[i].take().into_iter().flatten().filter_map(|k| k.figure) {
                    commands.entity(f).despawn();
                }
            }
            None if d <= RANGE && knots >= 2 => {
                let n = ((length * WALKERS_PER_M) as usize).max(1);
                let models = w.classes.get(class as usize).map(|c| c.models.clone()).unwrap_or_default();
                let list = (0..n)
                    .map(|k| {
                        let seed = (i as u32).wrapping_mul(7919).wrapping_add(k as u32).wrapping_mul(2_654_435_761);
                        let model = if models.is_empty() { 0 } else { models[(seed >> 16) as usize % models.len()] };
                        WalkerState { offset: length * k as f32 / n as f32, model, seed, figure: None }
                    })
                    .collect();
                w.walk[i] = Some(list);
            }
            _ => {}
        }
    }
    let (near, speed, per_model, sign, turn) = (w.near, w.walk_speed, w.per_model, w.heading_sign, Quat::from_rotation_y(w.model_yaw));
    let (mut pos, mut data) = (Vec::new(), Vec::new());
    let mut walk = std::mem::take(&mut w.walk);
    for (i, list) in walk.iter_mut().enumerate() {
        let Some(list) = list else { continue };
        let class = w.paths[i].class as u8;
        for wk in list.iter_mut() {
            let Some((at, tan)) = w.paths[i].point_at(wk.offset + speed * now) else { continue };
            let at = Vec3::from_array(at);
            // Card heading: crowd.wgsl faces (sin a, 0, -cos a) with a = heading * sign; turn it onto the tangent.
            let moving = tan[0].abs() + tan[2].abs() > 1e-4;
            let heading = if moving { tan[0].atan2(-tan[2]) / sign } else { 0.0 };
            for _ in 0..4 {
                pos.push(at.to_array());
                data.push([heading, (wk.model * per_model) as f32, 0.0, 1.0]);
            }
            let dist = Vec2::new(at.x - here.x, at.z - here.z).length();
            let t = Transform::from_translation(at).with_rotation(if moving { Quat::from_rotation_y((-tan[0]).atan2(-tan[2])) } else { Quat::IDENTITY } * turn);
            match wk.figure {
                Some(f) if dist > near + 8.0 => {
                    commands.entity(f).despawn();
                    wk.figure = None;
                }
                Some(f) => {
                    commands.entity(f).insert(t);
                }
                None if near > 0.0 && dist <= near + 3.0 => {
                    if let Some(f) = w.spawn_figure(commands, a, wk.seed, class, wk.model, 0, t) {
                        wk.figure = Some(f.root);
                    }
                }
                _ => {}
            }
        }
    }
    w.walk = walk;
    // The card mesh: rebuilt in place (main + render world usage so it can be edited every frame).
    let quads = pos.len() / 4;
    let (handle, entity) = match &w.walk_mesh {
        Some(m) => m.clone(),
        None => {
            let mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD);
            let h = a.meshes.add(mesh);
            let e = commands.spawn((Mesh3d(h.clone()), MeshMaterial3d(w.material.clone()), Transform::IDENTITY, Visibility::Hidden, bevy::camera::visibility::NoFrustumCulling, NotShadowCaster, crate::ui::world_load::WorldEntity)).id();
            w.walk_mesh = Some((h.clone(), e));
            w.walk_quads = usize::MAX;
            (h, e)
        }
    };
    if quads == 0 {
        if w.walk_quads != 0 {
            commands.entity(entity).insert(Visibility::Hidden);
            w.walk_quads = 0;
        }
        return;
    }
    let Some(mut mesh) = a.meshes.get_mut(&handle) else { return };
    if w.walk_quads != quads {
        let wd = CARD_W * 0.5;
        let corners = [([-wd, FOOT], [0.0, 1.0]), ([wd, FOOT], [1.0, 1.0]), ([wd, FOOT + CARD_H], [1.0, 0.0]), ([-wd, FOOT + CARD_H], [0.0, 0.0])];
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, (0..quads).flat_map(|_| corners.map(|c| c.1)).collect::<Vec<[f32; 2]>>());
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, (0..quads).flat_map(|_| corners.map(|c| c.0)).collect::<Vec<[f32; 2]>>());
        mesh.insert_indices(Indices::U32((0..quads as u32).flat_map(|q| [0, 1, 2, 0, 2, 3].map(|k| q * 4 + k)).collect()));
        if w.walk_quads == 0 || w.walk_quads == usize::MAX {
            commands.entity(entity).insert(Visibility::Inherited);
        }
        w.walk_quads = quads;
    }
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, data);
}

impl CrowdWorld {
    /// A standing card at the local origin for atlas model `model`, heading 0, replaced by the 3D figure
    /// within `near` (data.w = 1).
    fn walker_card(&mut self, model: u32, meshes: &mut Assets<Mesh>) -> Handle<Mesh> {
        let per_model = self.per_model;
        self.walker_cards
            .entry(model)
            .or_insert_with(|| {
                let d = [0.0, (model * per_model) as f32, 0.0, 1.0];
                let w = CARD_W * 0.5;
                let (mut pos, mut uv, mut corner, mut data) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for (cr, u) in [([-w, FOOT], [0.0, 1.0]), ([w, FOOT], [1.0, 1.0]), ([w, FOOT + CARD_H], [1.0, 0.0]), ([-w, FOOT + CARD_H], [0.0, 0.0])] {
                    pos.push([0.0f32; 3]);
                    corner.push(cr);
                    uv.push(u);
                    data.push(d);
                }
                let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
                mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
                mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
                mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, corner);
                mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, data);
                mesh.insert_indices(Indices::U32(vec![0, 1, 2, 0, 2, 3]));
                meshes.add(mesh)
            })
            .clone()
    }
}

/// SpriteDistance and the lod0..lod3 model counts from the installed `tracks/colorado/TrackSettings.xml`
/// `<Crowds>` element (Colorado: 60 m, 5 / 20 / 50 / 125), else the built-in Colorado values.
fn track_crowd_settings(assets: &Path) -> (f32, [usize; 4]) {
    let xml = std::fs::read_to_string(assets.join("tracks/colorado/TrackSettings.xml")).unwrap_or_default();
    let Some(el) = xml.find("<Crowds ").map(|p| &xml[p..p + xml[p..].find('>').unwrap_or(0)]) else {
        return (SPRITE_DISTANCE, LOD_COUNTS);
    };
    let attr = |name: &str| -> Option<f32> {
        let p = el.find(&format!(" {name}=\""))? + name.len() + 3;
        el[p..p + el[p..].find('"')?].parse().ok()
    };
    let lods = [0, 1, 2, 3].map(|k| attr(&format!("lod{k}")).map_or(LOD_COUNTS[k], |v| v.max(0.0) as usize));
    (attr("SpriteDistance").unwrap_or(SPRITE_DISTANCE), lods)
}
