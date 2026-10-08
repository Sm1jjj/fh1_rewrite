//! Scenery with the game's own materials: `FH1TILE4` tiles + `materials.json` (written by fh1setup).
//!
//! Format: see the header of `crates/fh1setup/src/scenery.rs` (authoritative). In short: tiles are
//! `b"FH1TILE4"`, u32 batch count, per batch `u32 material, flags, attributes, vertex count, index
//! count`, then position f32x3, normal f32x3, [tangent f32x3], [uv0], [uv1], [uv2] f32x2, [colour
//! A,R,G,B bytes], u32 indices. `materials.json` is an array of `{shader, technique, vs, ps,
//! textures: [null | {id, flags, file}]}`.
//! Use: [`parse_tile`] on a worker thread, then [`SceneryMaterials::material`] for each batch's
//! material on the main thread, and call [`SceneryMaterials::poll`] every frame to attach textures
//! as they finish loading.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use crate::material::{ATTRIBUTE_BINORMAL, ATTRIBUTE_COLOR, ATTRIBUTE_TANGENT, ATTRIBUTE_UV2, ATTRIBUTE_UV3};
use crate::{FxGlobals, FxLibrary, FxMaterial};

pub struct TileBatch {
    pub material: u32,
    /// fh1setup batch flags (bit 0 `STANDIN`: far stand-in with a placeholder texture).
    pub flags: u32,
    pub mesh: Mesh,
}

pub const STANDIN: u32 = 1;
const ATTR_TANGENT: u32 = 1;
const ATTR_UV0: u32 = 2;
const ATTR_UV1: u32 = 4;
const ATTR_UV2: u32 = 8;
const ATTR_COLOR: u32 = 16;
/// Tile version 5 (FM4 tracks): uv3 f32x2 then binormal f32x3 after the colour.
const ATTR_UV3: u32 = 32;
const ATTR_BINORMAL: u32 = 64;

/// Parse an `FH1TILE4` / `FH1TILE5` tile into one mesh per batch (safe to call off the main thread).
pub fn parse_tile(b: &[u8]) -> Option<Vec<TileBatch>> {
    if !matches!(b.get(..8)?, b"FH1TILE4" | b"FH1TILE5") {
        return None;
    }
    let mut p = 8usize;
    let u32n = |p: &mut usize| -> Option<u32> {
        let v = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
        *p += 4;
        Some(v)
    };
    let floats = |count: usize, p: &mut usize| -> Option<Vec<f32>> {
        let bytes = b.get(*p..*p + count * 4)?;
        *p += count * 4;
        Some(bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
    };
    let v3 = |f: Vec<f32>| f.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect::<Vec<[f32; 3]>>();
    let v2 = |f: Vec<f32>| f.chunks_exact(2).map(|c| [c[0], c[1]]).collect::<Vec<[f32; 2]>>();
    let n = u32n(&mut p)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let material = u32n(&mut p)?;
        let flags = u32n(&mut p)?;
        let mask = u32n(&mut p)?;
        let nv = u32n(&mut p)? as usize;
        let ni = u32n(&mut p)? as usize;
        let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
        m.insert_attribute(Mesh::ATTRIBUTE_POSITION, v3(floats(nv * 3, &mut p)?));
        m.insert_attribute(Mesh::ATTRIBUTE_NORMAL, v3(floats(nv * 3, &mut p)?));
        if mask & ATTR_TANGENT != 0 {
            m.insert_attribute(ATTRIBUTE_TANGENT, v3(floats(nv * 3, &mut p)?));
        }
        if mask & ATTR_UV0 != 0 {
            m.insert_attribute(Mesh::ATTRIBUTE_UV_0, v2(floats(nv * 2, &mut p)?));
        }
        if mask & ATTR_UV1 != 0 {
            m.insert_attribute(Mesh::ATTRIBUTE_UV_1, v2(floats(nv * 2, &mut p)?));
        }
        if mask & ATTR_UV2 != 0 {
            m.insert_attribute(ATTRIBUTE_UV2, v2(floats(nv * 2, &mut p)?));
        }
        if mask & ATTR_COLOR != 0 {
            let c = b.get(p..p + nv * 4)?;
            p += nv * 4;
            m.insert_attribute(ATTRIBUTE_COLOR, VertexAttributeValues::Unorm8x4(c.chunks_exact(4).map(|x| [x[0], x[1], x[2], x[3]]).collect()));
        }
        if mask & ATTR_UV3 != 0 {
            m.insert_attribute(ATTRIBUTE_UV3, v2(floats(nv * 2, &mut p)?));
        }
        if mask & ATTR_BINORMAL != 0 {
            m.insert_attribute(ATTRIBUTE_BINORMAL, v3(floats(nv * 3, &mut p)?));
        }
        let idx = b.get(p..p + ni * 4)?;
        p += ni * 4;
        m.insert_indices(Indices::U32(idx.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()));
        out.push(TileBatch { material, flags, mesh: m });
    }
    Some(out)
}

#[derive(Debug, Clone)]
struct MaterialDef {
    shader: String,
    vs: Vec<[f32; 4]>,
    ps: Vec<[f32; 4]>,
    textures: Vec<Option<u32>>,
}

/// Scenery FxMaterials made so far: base materials, per-object variants, per-instance lightmap variants (FH1_FX_STATS).
static MADE: [std::sync::atomic::AtomicUsize; 3] = [const { std::sync::atomic::AtomicUsize::new(0) }; 3];

fn count_made(k: usize) {
    MADE[k].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Debug: `FH1_FX_STATS=1` logs every 5 s the FxMaterial assets, the scenery ones by origin, and how many distinct
/// materials the FX mesh entities use (each is its own bind group, so draws of different ones never batch).
pub fn fx_stats(materials: Res<Assets<FxMaterial>>, meshes: Query<(&MeshMaterial3d<FxMaterial>, &ViewVisibility)>, time: Res<Time<Real>>, mut last: Local<f32>) {
    if !std::env::var("FH1_FX_STATS").is_ok_and(|v| v == "1") || time.elapsed_secs() - *last < 5.0 {
        return;
    }
    *last = time.elapsed_secs();
    let made = MADE.each_ref().map(|a| a.load(std::sync::atomic::Ordering::Relaxed));
    let (mut used, mut visible) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let (mut entities, mut vis_entities) = (0, 0);
    for (m, v) in &meshes {
        entities += 1;
        used.insert(m.0.id());
        if v.get() {
            vis_entities += 1;
            visible.insert(m.0.id());
        }
    }
    info!(
        "fx stats: {} FxMaterial assets (scenery made: {} base, {} object variants, {} lightmap variants); {entities} FX entities use {} materials; {vis_entities} visible use {}",
        materials.len(),
        made[0],
        made[1],
        made[2],
        used.len(),
        visible.len()
    );
}

/// materials.json index of every scenery FxMaterial (bases and per-object variants), for `FH1_FX_PICK`.
static PICK_INDEX: std::sync::RwLock<Option<HashMap<AssetId<FxMaterial>, u32>>> = std::sync::RwLock::new(None);

fn note_pick(id: AssetId<FxMaterial>, idx: u32) {
    if std::env::var_os("FH1_FX_PICK").is_some() {
        PICK_INDEX.write().unwrap().get_or_insert_with(HashMap::new).insert(id, idx);
    }
}

/// Debug: `FH1_FX_PICK=<m>` logs, every 3 s, the scenery FX meshes under the point `m` metres ahead of the
/// shadow camera (level with the camera, smallest bounds first): materials.json index, effect, PS constants.
pub fn fx_pick(
    cams: Query<&GlobalTransform, With<crate::shadow::FxShadowCamera>>,
    meshes: Query<(&MeshMaterial3d<FxMaterial>, &bevy::camera::primitives::Aabb, &GlobalTransform)>,
    materials: Res<Assets<FxMaterial>>,
    shaders: Res<Assets<Shader>>,
    time: Res<Time<Real>>,
    mut last: Local<f32>,
) {
    let Some(dist) = std::env::var("FH1_FX_PICK").ok().and_then(|v| v.parse::<f32>().ok()) else { return };
    if time.elapsed_secs() - *last < 3.0 {
        return;
    }
    *last = time.elapsed_secs();
    let Some(cam) = cams.iter().next() else { return };
    let fwd = (cam.forward().as_vec3() * Vec3::new(1.0, 0.0, 1.0)).normalize_or(Vec3::NEG_Z);
    let p = cam.translation() + fwd * dist;
    let index = PICK_INDEX.read().unwrap();
    let mut hits = Vec::new();
    for (m, aabb, t) in &meshes {
        let a = t.affine();
        let (c, h) = (Vec3::from(aabb.center), Vec3::from(aabb.half_extents));
        // World bounds of the box.
        let wc = a.transform_point3(c);
        let wh = a.matrix3.abs() * h;
        if (p.x - wc.x).abs() > wh.x || (p.z - wc.z).abs() > wh.z || wc.y - wh.y > p.y {
            continue;
        }
        let idx = index.as_ref().and_then(|i| i.get(&m.0.id()).copied());
        hits.push((wh.x * wh.z, wc.y + wh.y, idx, m.0.id()));
    }
    hits.sort_by(|a, b| a.0.total_cmp(&b.0));
    info!("FX pick at {:.1?} ({} hits):", p, hits.len());
    for (area, top, idx, id) in hits.iter().take(10) {
        let m = materials.get(*id);
        let program = m.map(|m| m.program);
        let shader = program.and_then(|p| crate::material::PROGRAMS.read().unwrap().get(p as usize).map(|e| e.shader.clone())).and_then(|h| shaders.get(&h).map(|s| s.path.clone()));
        info!(
            "  area {area:.0} m² top {top:.1}: material {idx:?} program {program:?} {shader:?} ps0..3 {:?} object {:?}",
            m.map(|m| &m.consts.ps[..4]),
            m.map(|m| m.consts.object)
        );
    }
}

/// Debug: `FH1_FX_RAY=<x>,<y>` (viewport fractions, 0,0 = top left) logs, every 3 s, every mesh whose bounds the
/// camera ray through that pixel hits, nearest first: names up the hierarchy, material kind, FX effect, empty slots.
#[allow(clippy::type_complexity)]
pub fn fx_ray(
    cams: Query<(&Camera, &GlobalTransform), With<crate::shadow::FxShadowCamera>>,
    meshes: Query<(
        Entity,
        &bevy::camera::primitives::Aabb,
        &GlobalTransform,
        Option<&MeshMaterial3d<FxMaterial>>,
        Option<&MeshMaterial3d<StandardMaterial>>,
        Has<Mesh3d>,
    )>,
    names: Query<&Name>,
    parents: Query<&ChildOf>,
    unbounded: Query<
        (Entity, &GlobalTransform, Has<MeshMaterial3d<FxMaterial>>, Has<MeshMaterial3d<StandardMaterial>>, Option<&bevy::camera::visibility::RenderLayers>),
        (With<Mesh3d>, Without<bevy::camera::primitives::Aabb>),
    >,
    materials: Res<Assets<FxMaterial>>,
    std_materials: Res<Assets<StandardMaterial>>,
    shaders: Res<Assets<Shader>>,
    time: Res<Time<Real>>,
    mut last: Local<f32>,
) {
    let Some(at) = std::env::var("FH1_FX_RAY").ok().and_then(|v| {
        let (x, y) = v.split_once(',')?;
        Some(Vec2::new(x.trim().parse().ok()?, y.trim().parse().ok()?))
    }) else {
        return;
    };
    if time.elapsed_secs() - *last < 3.0 {
        return;
    }
    *last = time.elapsed_secs();
    let Some((cam, cam_t)) = cams.iter().next() else { return };
    let Some(size) = cam.logical_viewport_size() else { return };
    let Ok(ray) = cam.viewport_to_world(cam_t, at * size) else { return };
    let mut hits = Vec::new();
    for (e, aabb, t, fx, std, has_mesh) in &meshes {
        if !has_mesh {
            continue;
        }
        // Ray into the mesh's local space, slab test against the local box.
        let inv = t.affine().inverse();
        let o = inv.transform_point3(ray.origin);
        let d = inv.transform_vector3(*ray.direction);
        let (c, h) = (Vec3::from(aabb.center), Vec3::from(aabb.half_extents));
        let t0 = (c - h - o) / d;
        let t1 = (c + h - o) / d;
        let near = t0.min(t1).max_element();
        let far = t0.max(t1).min_element();
        // Boxes around the camera (whole zone batches) say nothing about what is under the pixel.
        if near > far || near <= 0.0 {
            continue;
        }
        let dist = (t.affine().transform_point3(o + d * near.max(0.0)) - ray.origin).length();
        hits.push((dist, e, fx.map(|m| m.0.id()), std.map(|m| m.0.id())));
    }
    hits.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Meshes without bounds (NoFrustumCulling, skinned) can't be ray-tested: list them.
    for (e, t, fx, std, layers) in &unbounded {
        let mut chain = Vec::new();
        let mut cur = Some(e);
        while let Some(x) = cur {
            if let Ok(n) = names.get(x) {
                chain.push(n.as_str().to_string());
            }
            cur = parents.get(x).ok().map(|p| p.parent());
        }
        info!("  unbounded {e:?} at {:.1?} {chain:?} fx {} std {} layers {layers:?}", t.translation(), fx, std);
    }
    info!("FX ray at {at:?} ({} hits):", hits.len());
    for (dist, e, fx, std) in hits.iter().take(12) {
        let mut chain = Vec::new();
        let mut cur = Some(*e);
        while let Some(x) = cur {
            if let Ok(n) = names.get(x) {
                chain.push(n.as_str().to_string());
            }
            cur = parents.get(x).ok().map(|p| p.parent());
        }
        let kind = if let Some(m) = fx.and_then(|id| materials.get(id)) {
            let shader = crate::material::PROGRAMS.read().unwrap().get(m.program as usize).map(|p| p.shader.clone()).and_then(|h| shaders.get(&h).map(|s| s.path.clone()));
            let empty: Vec<usize> = (0..16).filter(|&k| m.clone().slot_mut(k as u32).is_some_and(|s| s.is_none())).collect();
            format!("fx {shader:?} empty slots {empty:?} ps0 {:?}", m.consts.ps[0])
        } else if let Some(m) = std.and_then(|id| std_materials.get(id)) {
            format!("std base {:?} tex {}", m.base_color.to_srgba(), m.base_color_texture.is_some())
        } else {
            "other material".to_string()
        };
        info!("  {dist:.1} m {e:?} {chain:?}: {kind}");
    }
}

/// Lazily built scenery materials and their textures.
#[derive(Resource)]
pub struct SceneryMaterials {
    dir: PathBuf,
    shaders_dir: PathBuf,
    defs: Vec<MaterialDef>,
    dds: HashMap<u32, Option<String>>,
    built: HashMap<u32, Option<Handle<FxMaterial>>>,
    images: HashMap<u32, Handle<Image>>,
    /// Texture id → gamma-signed (decided from its format, see `texture_is_gamma`).
    gamma: HashMap<u32, bool>,
    /// Texture id → is a cube map.
    cube: HashMap<u32, bool>,
    /// Gamma flags read from materials.json (take precedence over the format rule).
    gamma_known: HashMap<u32, bool>,
    loading: HashMap<u32, Task<Option<Image>>>,
    /// Materials waiting for a texture: texture id → (material, sampler register).
    waiting: HashMap<u32, Vec<(Handle<FxMaterial>, u32)>>,
    /// Materials whose program reads the per-object registers, with the textures they requested
    /// (texture id, slot), so variants can wait for the same ones.
    object_users: HashMap<AssetId<FxMaterial>, Vec<(u32, u32)>>,
    /// Per-object variants: (base material, quantised object values) → handle.
    variants: HashMap<(AssetId<FxMaterial>, [i16; 8]), Handle<FxMaterial>>,
    /// Materials whose tf7 is the per-instance lightmap (rmb slot -1; BLACK until an instance names one).
    lm_users: std::collections::HashSet<AssetId<FxMaterial>>,
    /// Per-instance lightmap variants: (material, PVS texture index) → handle.
    lm_variants: HashMap<(AssetId<FxMaterial>, u32), Handle<FxMaterial>>,
    /// Cache entries no entity or material references any more: asset id → when first seen idle (s).
    idle: HashMap<bevy::asset::UntypedAssetId, f32>,
    /// Time of the last [`Self::maintain`] sweep (s); `None` = eviction off (`FH1_SCENERY_EVICT=0`).
    last_sweep: Option<f32>,
    /// A missing tf7 stays Bevy's white fallback instead of BLACK: index.json `"runtime_lightmap": "white"` (FM4,
    /// where tf7 is the daytime track lightmap and the runtime-composed atlas tiles have no file; black darkened every
    /// road to DarkColor, docs/FM4_RECON.md). FH1 tracks don't set it (tf7 = the night lamp lightmap, black = off).
    runtime_lm_white: bool,
    /// Single-channel (BC4, Xenos DXT5A) textures sample as `.xxxx` (`gamma.y` bit per slot): index.json
    /// `"single_channel": "replicate"` (FM4: its road blend masks are DXT5A read through `.y`, which BC4's (r, 0, 0, 1)
    /// zeroed, so every road went black; the 360's single-channel fetch replicates). FH1 tracks don't set it.
    replicate_single: bool,
    /// Texture id -> single-channel (BC4).
    single: HashMap<u32, bool>,
    /// Swap the effects' cull direction (set if the mesh winding turns out mirrored).
    pub flip_cull: bool,
    /// Draw both sides regardless of the effects' cull mode.
    pub no_cull: bool,
}

fn parse_vec4s(v: &serde_json::Value) -> Vec<[f32; 4]> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let x = x.as_array()?;
                    Some([0, 1, 2, 3].map(|i| x.get(i).and_then(|f| f.as_f64()).unwrap_or(0.0) as f32))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl SceneryMaterials {
    /// `scenery_dir` holds materials.json and textures/; `shaders_dir` the track .fxobj files.
    pub fn load(scenery_dir: &Path, shaders_dir: &Path) -> Option<Self> {
        let j: serde_json::Value = serde_json::from_slice(&std::fs::read(scenery_dir.join("materials.json")).ok()?).ok()?;
        let index: serde_json::Value =
            std::fs::read(scenery_dir.join("index.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut dds = HashMap::new();
        let mut words: HashMap<u32, bool> = HashMap::new();
        let defs = j
            .as_array()?
            .iter()
            .map(|m| MaterialDef {
                // "shaders\track\h_....fx" -> "h_..."
                shader: m["shader"].as_str().unwrap_or("").rsplit(['\\', '/']).next().unwrap_or("").trim_end_matches(".fx").to_ascii_lowercase(),
                vs: parse_vec4s(&m["vs"]),
                ps: parse_vec4s(&m["ps"]),
                textures: m["textures"]
                    .as_array()
                    .map(|t| {
                        t.iter()
                            .map(|x| {
                                let id = x["id"].as_u64()? as u32;
                                dds.insert(id, x["file"].as_str().map(str::to_owned));
                                // The .bix format word's sign byte (fh1-rewrite-d8): 0x3F = gamma RGB.
                                if let Some(w) = x["word"].as_u64() {
                                    words.insert(id, ((w >> 8) & 0x3F) == 0x3F);
                                }
                                Some(id)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        Some(Self {
            dir: scenery_dir.to_owned(),
            shaders_dir: shaders_dir.to_owned(),
            defs,
            dds,
            built: HashMap::new(),
            images: HashMap::new(),
            gamma: words.clone(),
            gamma_known: words,
            cube: HashMap::new(),
            loading: HashMap::new(),
            waiting: HashMap::new(),
            object_users: HashMap::new(),
            variants: HashMap::new(),
            lm_users: Default::default(),
            lm_variants: HashMap::new(),
            idle: HashMap::new(),
            last_sweep: (!std::env::var("FH1_SCENERY_EVICT").is_ok_and(|v| v == "0")).then_some(0.0),
            runtime_lm_white: index["runtime_lightmap"] == "white",
            replicate_single: index["single_channel"] == "replicate",
            single: HashMap::new(),
            flip_cull: false,
            // Winding checked on Colorado: the effects' cull modes are right as authored.
            no_cull: false,
        })
    }

    /// Call every frame from the owner (engine scenery `stream`, next to `poll`): every SWEEP_EVERY s releases
    /// scenery materials/textures no loaded tile, zone or prop uses any more, so the resident set follows the
    /// streaming window instead of growing with the distance driven (perf P1: a full-map tour kept 23.6k
    /// FxMaterials and 5.1k textures). `FH1_SCENERY_EVICT=0` keeps everything.
    pub fn maintain(&mut self, now: f32) {
        let Some(last) = self.last_sweep else { return };
        if now - last < SWEEP_EVERY {
            return;
        }
        self.last_sweep = Some(now);
        let (m, t) = self.sweep(now, SWEEP_IDLE);
        if m + t > 0 {
            debug!("scenery caches: released {m} materials, {t} textures");
        }
    }

    /// Frees cached materials and textures that nothing else holds any more:
    /// an entry whose strong handle is only the cache's, idle for `idle_s`, is dropped, so its asset goes
    /// with it; a later request rebuilds it through [`Self::material`] / [`Self::object_variant`] /
    /// [`Self::lightmap_variant`] / `request`. Returns (materials, textures) evicted.
    pub fn sweep(&mut self, now: f32, idle_s: f32) -> (usize, usize) {
        fn only_cache<A: Asset>(h: &Handle<A>) -> bool {
            matches!(h, Handle::Strong(a) if std::sync::Arc::strong_count(a) == 1)
        }
        let old = std::mem::take(&mut self.idle);
        let mut idle = HashMap::new();
        let mut gone: Vec<AssetId<FxMaterial>> = Vec::new();
        let mut evict = |id: bevy::asset::UntypedAssetId, free: bool| -> bool {
            if !free {
                return false;
            }
            let since = old.get(&id).copied().unwrap_or(now);
            if now - since >= idle_s {
                return true;
            }
            idle.insert(id, since);
            false
        };
        // Variants first (nothing else in the cache points at them), then the bases, then the textures,
        // which the freed materials held (they go on a later sweep, once their materials are gone).
        let mut mats = 0;
        self.variants.retain(|_, h| {
            let e = evict(h.id().untyped(), only_cache(h));
            if e {
                gone.push(h.id());
            }
            !e
        });
        self.lm_variants.retain(|_, h| {
            let e = evict(h.id().untyped(), only_cache(h));
            if e {
                gone.push(h.id());
            }
            !e
        });
        self.built.retain(|_, h| match h {
            Some(h) => {
                let e = evict(h.id().untyped(), only_cache(h));
                if e {
                    gone.push(h.id());
                }
                !e
            }
            None => true,
        });
        let loading = &self.loading;
        let before = self.images.len();
        self.images.retain(|id, h| loading.contains_key(id) || !evict(h.id().untyped(), only_cache(h)));
        let textures = before - self.images.len();
        mats += gone.len();
        debug!(
            "scenery caches: {} built, {} variants, {} lm variants, {} images; {} idle candidates",
            self.built.len(),
            self.variants.len(),
            self.lm_variants.len(),
            self.images.len(),
            idle.len()
        );
        for id in &gone {
            self.object_users.remove(id);
            self.lm_users.remove(id);
        }
        self.idle = idle;
        (mats, textures)
    }

    fn find_effect(&self, name: &str) -> Option<Vec<u8>> {
        let rd = std::fs::read_dir(&self.shaders_dir).ok()?;
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().to_ascii_lowercase();
            if f == format!("{name}.fxobj") {
                return std::fs::read(e.path()).ok();
            }
        }
        None
    }

    /// The [`FxLibrary`] name of a track effect. Each imported game ships its own builds of same-named effects (FH2's
    /// Anthem: 52 names shared with Colorado, none byte-identical; FM4's differ in vertex layout), and the library keeps
    /// the first effect per name for the whole session, so an in-process map change would draw the new map with the old
    /// one's programs. Effects from an import's own folder (`imported/<id>/shaders/track`) get a per-folder suffix;
    /// the install's `shaders/track` (Colorado) keep their plain names.
    fn effect_key(&self, shader: &str) -> String {
        if !self.shaders_dir.components().any(|c| c.as_os_str() == "imported") {
            return shader.to_owned();
        }
        let mut h: u32 = 0x811C_9DC5;
        for b in self.shaders_dir.to_string_lossy().to_ascii_lowercase().bytes() {
            h = (h ^ b as u32).wrapping_mul(0x0100_0193);
        }
        format!("{shader}@{h:08x}")
    }

    /// Translate and register every scenery program up front (P5b). A program is otherwise built the first time one of
    /// its materials comes into view, and Bevy then processes the new Shader inside extract: 30-85 ms frames (traced)
    /// while driving into new areas. Run once while loading; `FH1_WARM_PROGRAMS=0` = on demand (old). Returns the count.
    pub fn warm_programs(&mut self, lib: &mut FxLibrary, globals: &mut FxGlobals, shaders: &mut Assets<Shader>) -> usize {
        if std::env::var("FH1_WARM_PROGRAMS").is_ok_and(|v| v == "0") {
            return 0;
        }
        let mut n = 0;
        for name in self.program_names() {
            n += self.warm_program(&name, lib, globals, shaders) as usize;
        }
        n
    }

    /// Every scenery shader this track's materials use (sorted, deduplicated): the work list of [`Self::warm_program`],
    /// for callers that spread the warm-up over frames (engine scenery.rs, loading cover).
    pub fn program_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.defs.iter().map(|d| d.shader.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Translate and register one scenery shader's Default program (see [`Self::warm_programs`]).
    pub fn warm_program(&mut self, name: &str, lib: &mut FxLibrary, globals: &mut FxGlobals, shaders: &mut Assets<Shader>) -> bool {
        if self.built.is_empty() {
            set_object_defaults(globals);
        }
        let key = self.effect_key(name);
        if !lib.has_effect(&key) {
            let Some(bytes) = self.find_effect(name) else { return false };
            if lib.add_effect(&key, &bytes).is_err() {
                return false;
            }
        }
        lib.program(&key, "Default", shaders, globals).is_some()
    }

    /// The material for index `idx` (created on first use; textures attach later via `poll`).
    #[allow(clippy::too_many_arguments)]
    pub fn material(
        &mut self,
        idx: u32,
        lib: &mut FxLibrary,
        globals: &mut FxGlobals,
        shaders: &mut Assets<Shader>,
        materials: &mut Assets<FxMaterial>,
    ) -> Option<Handle<FxMaterial>> {
        if let Some(h) = self.built.get(&idx) {
            return h.clone();
        }
        if self.built.is_empty() {
            set_object_defaults(globals);
        }
        let built = (|| {
            let def = self.defs.get(idx as usize)?.clone();
            let key = self.effect_key(&def.shader);
            if !lib.has_effect(&key) {
                let bytes = self.find_effect(&def.shader)?;
                lib.add_effect(&key, &bytes).ok()?;
            }
            let (pid, program) = lib.program(&key, "Default", shaders, globals)?;
            let mut m = lib.material((pid, &program), &def.vs, &def.ps, globals);
            m.flip_cull = self.flip_cull;
            m.no_cull = self.no_cull;
            let mut pending = Vec::new();
            for (reg, tex) in def.textures.iter().enumerate() {
                let Some(id) = *tex else { continue };
                // Runtime-supplied textures with no file: the night lamp lightmap (slot 7) is black
                // when absent, AO slots stay white (the default). (docs/LIGHTMAPS.md, UNVERIFIED.)
                if reg == 7 && self.dds.get(&id).is_none_or(|f| f.is_none()) {
                    if self.runtime_lm_white {
                        continue;
                    }
                    if let Some(s) = m.slot_mut(7) {
                        *s = Some(BLACK.clone());
                    }
                    continue;
                }
                // Cube fetches go to a cube slot (encoded as 100 + slot).
                let slot = match program.textures.iter().find(|(tf, _)| *tf == reg as u32) {
                    Some((tf, 3)) => 100 + crate::program::cube_slot(*tf) as u32,
                    _ => reg as u32,
                };
                if let Some(img) = self.images.get(&id) {
                    // A cube slot takes only cube textures and a 2D slot only 2D ones; otherwise
                    // the slot keeps Bevy's neutral fallback (bind groups would be invalid).
                    let cube = self.cube.get(&id).copied().unwrap_or(false);
                    if cube == (slot >= 100) {
                        if let Some(s) = m.slot_mut(slot) {
                            *s = Some(img.clone());
                        }
                    }
                    if self.gamma.get(&id).copied().unwrap_or(false) && slot < 32 {
                        m.consts.gamma.x |= 1 << slot;
                    }
                    if self.replicate_single && self.single.get(&id).copied().unwrap_or(false) && slot < 32 {
                        m.consts.gamma.y |= 1 << slot;
                    }
                } else {
                    pending.push((id, slot));
                }
            }
            // tf7 Light_MapSampler with no texture in the rmb (slot -1 = the per-instance lightmap from the .pvs
            // record, not bound yet; docs/LIGHTMAPS.md): black, not Bevy's white fallback, which lit every `_lm`
            // building at full strength at night (lm · max(c213.y, c6.z), c213.y = 1 after dark).
            let per_instance_lm = !self.runtime_lm_white && program.textures.iter().any(|&(tf, d)| tf == 7 && d != 3) && m.t7.is_none() && !pending.iter().any(|&(_, s)| s == 7);
            if per_instance_lm {
                m.t7 = Some(BLACK.clone());
            }
            let h = materials.add(m);
            count_made(0);
            if per_instance_lm {
                self.lm_users.insert(h.id());
            }
            note_pick(h.id(), idx);
            if program.uses_object {
                self.object_users.insert(h.id(), pending.clone());
            }
            for (id, reg) in pending {
                self.request(id);
                self.waiting.entry(id).or_default().push((h.clone(), reg));
            }
            Some(h)
        })();
        self.built.insert(idx, built.clone());
        built
    }

    /// `base` (a material from [`Self::material`]) with an instance's own per-object values
    /// (`material::object_consts`: tint and ground normal). Returns `base` itself when its program doesn't
    /// read them. Values are quantised so placements share variants: tint to 1/16 per channel, the normal to
    /// steps of 0.2 (~11 degrees); the quantised value is what the material gets.
    pub fn object_variant(&mut self, base: &Handle<FxMaterial>, object: [Vec4; 2], materials: &mut Assets<FxMaterial>) -> Handle<FxMaterial> {
        let Some(pending) = self.object_users.get(&base.id()).cloned() else { return base.clone() };
        let q = |v: f32, s: f32| (v * s).round() as i16;
        let o = object;
        let k = [q(o[0].x, 16.0), q(o[0].y, 16.0), q(o[0].z, 16.0), q(o[0].w, 16.0), q(o[1].x, 5.0), q(o[1].y, 5.0), q(o[1].z, 5.0), q(o[1].w, 5.0)];
        let key = (base.id(), k);
        if let Some(h) = self.variants.get(&key) {
            return h.clone();
        }
        let Some(mut m) = materials.get(base).cloned() else { return base.clone() };
        let n = Vec3::new(k[4] as f32, k[5] as f32, k[6] as f32).normalize_or(Vec3::Y);
        m.consts.object = [Vec4::new(k[0] as f32, k[1] as f32, k[2] as f32, k[3] as f32) / 16.0, n.extend(o[1].w)];
        let h = materials.add(m);
        count_made(1);
        if self.lm_users.contains(&base.id()) {
            self.lm_users.insert(h.id());
        }
        let idx = PICK_INDEX.read().unwrap().as_ref().and_then(|m| m.get(&base.id()).copied());
        if let Some(idx) = idx {
            note_pick(h.id(), idx);
        }
        // Textures that haven't arrived yet: the variant waits for them like the base material.
        for (id, slot) in pending {
            if self.loading.contains_key(&id) {
                self.waiting.entry(id).or_default().push((h.clone(), slot));
            }
        }
        self.variants.insert(key, h.clone());
        h
    }

    /// `base` (from [`Self::material`] or [`Self::object_variant`]; call this one last) with a placement's own
    /// night lightmap: the `.pvs` record's u32 @8 texture when its u32 @12 names a list position (docs/LIGHTMAPS.md
    /// "Per-instance lightmaps"), given as its file id like materials.json texture ids. Only materials whose tf7 is
    /// rmb slot -1 take it; others return `base`. The texture is `textures/<id as %08x>.dds`; a
    /// missing file keeps tf7 black, which is also right by day (lm × max(c213.y, c6.z), c6.z = 0).
    pub fn lightmap_variant(&mut self, base: &Handle<FxMaterial>, lightmap: u32, materials: &mut Assets<FxMaterial>) -> Handle<FxMaterial> {
        // FH1_INST_LM=0: leave the per-instance lightmaps black (A/B).
        if !self.lm_users.contains(&base.id()) || std::env::var("FH1_INST_LM").is_ok_and(|v| v == "0") {
            return base.clone();
        }
        let key = (base.id(), lightmap);
        if let Some(h) = self.lm_variants.get(&key) {
            return h.clone();
        }
        let Some(mut m) = materials.get(base).cloned() else { return base.clone() };
        if let Some(img) = self.images.get(&lightmap) {
            m.t7 = Some(img.clone());
            if self.gamma.get(&lightmap).copied().unwrap_or(true) {
                m.consts.gamma.x |= 1 << 7;
            }
        }
        let h = materials.add(m);
        count_made(2);
        if !self.images.contains_key(&lightmap) {
            self.dds.entry(lightmap).or_insert_with(|| Some(format!("textures/{lightmap:08x}.dds")));
            self.request(lightmap);
            if self.loading.contains_key(&lightmap) {
                self.waiting.entry(lightmap).or_default().push((h.clone(), 7));
            }
        }
        self.lm_variants.insert(key, h.clone());
        let n = self.lm_variants.len();
        if n == 1 || n % 200 == 0 {
            info!("scenery: {n} per-instance lightmap materials (latest texture {lightmap:08x})");
        }
        h
    }

    fn request(&mut self, id: u32) {
        if self.loading.contains_key(&id) || self.images.contains_key(&id) {
            return;
        }
        let Some(Some(rel)) = self.dds.get(&id).cloned() else { return };
        let path = self.dir.join(rel);
        self.loading.insert(id, AsyncComputeTaskPool::get().spawn(async move { read_dds(&path) }));
    }

    /// Attach finished textures to the materials waiting for them.
    pub fn poll(&mut self, images: &mut Assets<Image>, materials: &mut Assets<FxMaterial>) {
        if !images.contains(&BLACK) {
            let _ = images.insert(&BLACK, black_image());
        }
        let done: Vec<u32> = self.loading.iter().filter(|(_, t)| t.is_finished()).map(|(k, _)| *k).collect();
        for id in done {
            let task = self.loading.remove(&id).unwrap();
            let Some(img) = block_on(future::poll_once(task)).flatten() else { continue };
            let gamma = self.gamma_known.get(&id).copied().unwrap_or_else(|| texture_is_gamma(img.texture_descriptor.format));
            self.gamma.insert(id, gamma);
            let single = img.texture_descriptor.format == bevy::render::render_resource::TextureFormat::Bc4RUnorm;
            self.single.insert(id, single);
            let cube = img.texture_descriptor.size.depth_or_array_layers == 6;
            self.cube.insert(id, cube);
            let h = images.add(img);
            self.images.insert(id, h.clone());
            for (mh, reg) in self.waiting.remove(&id).unwrap_or_default() {
                if let Some(mut m) = materials.get_mut(&mh) {
                    if cube == (reg >= 100) {
                        if let Some(s) = m.slot_mut(reg) {
                            *s = Some(h.clone());
                        }
                    }
                    if self.replicate_single && single && reg < 32 {
                        m.consts.gamma.y |= 1 << reg;
                    }
                    if gamma && reg < 32 {
                        m.consts.gamma.x |= 1 << reg;
                    }
                }
            }
        }
    }
}

/// Per-object registers the game sets for every draw. Their constant-table defaults are all zero, which
/// turns the trees (86% of the scenery triangles) black. ModelData and SurfaceNormalAndShadowPower now
/// live in each material (`FxMaterialConsts::object`, per instance via `material_variant`); the global
/// values below remain for any program outside the track family. The values:
/// - `ModelData` c148: word 8 of the instance, D3DCOLOR `ff808080` on every instance sampled (RGBA / 255).
///   Trees and the tint shaders multiply by `2 × ModelData.rgb` (0.5 = neutral); the emissive shaders
///   switch on when `SwitchOnLights >= ModelData.x`. (Values VERIFIED from the files, mapping INFERRED.)
/// - `SurfaceNormalAndShadowPower` c162: tree VS computes `pow(saturate(dot(c162.xyz, sunDir)), c162.w)`
///   (VERIFIED from the microcode). xyz = the ground normal under the tree (instance word 3, a signed
///   10:10:10 unit vector, mostly near +Y; INFERRED); +Y here. w = 1 is a GUESS (zero gives pow(0, 0)).
/// - `FadeValues` c157 is the trees' name for DistanceFadeValues (same register): no fade.
/// - `DirectFadeValues` c153: h_diff_fade multiplies output alpha by `.x`; 1 = opaque (GUESS).
/// - `objTintColour` VS c198 (h_diff_mask_fade_tint_1): tint = `2 × ModelData.rgb`, times `c198.rgb` when
///   `ModelData.w > 0.5` (VERIFIED from the microcode). The constant-table default (0, 1, 0, 1) turns every
///   instance with tint alpha `ff` green; white = no extra tint (GUESS, the game's writer is untraced).
fn set_object_defaults(globals: &mut FxGlobals) {
    let grey = 128.0 / 255.0;
    globals.set_vec("ModelData", Vec4::new(grey, grey, grey, 1.0));
    globals.set_vec("SurfaceNormalAndShadowPower", Vec4::new(0.0, 1.0, 0.0, 1.0));
    globals.set_vec("FadeValues", Vec4::new(1.0e7, 1.0e7 - 1.0, 1.0, 0.0));
    globals.set_vec("DirectFadeValues", Vec4::new(1.0, 0.0, 0.0, 0.0));
    globals.set_vec("objTintColour", Vec4::ONE);
}

/// 1×1 black texture for absent night lightmaps (inserted by `poll`).
const BLACK: Handle<Image> = bevy::asset::uuid_handle!("6d1c2f4e-9a7b-4f3e-8c21-5b0e7a9d3f11");

fn black_image() -> Image {
    Image::new_fill(
        Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// Whether a track texture is fetched with the Xenos gamma (PWL) decode. Car `.xds` textures store
/// the fetch constant: every colour format is gamma-signed (RGB), DXN normal maps are linear. Track
/// `.bix` files don't store that word; the same rule is applied (INFERRED, docs/SHADERS.md): BC5
/// (DXN) and single-channel BC4 (DXT5A) linear, everything else gamma.
pub fn texture_is_gamma(f: TextureFormat) -> bool {
    !matches!(f, TextureFormat::Bc5RgUnorm | TextureFormat::Bc4RUnorm)
}

/// DDS with a DX10 header (as fh1setup writes them): BCn levels, largest first. Sampled as UNORM:
/// the game's shaders do their own gamma handling (see docs/SHADERS.md, "Texture gamma").
pub fn read_dds(path: &Path) -> Option<Image> {
    read_dds_bytes(&std::fs::read(path).ok()?)
}

/// [`read_dds`] through the RAM file cache (crate::files): for car textures, which the car-load task prefetches.
pub fn read_dds_cached(path: &Path) -> Option<Image> {
    read_dds_bytes(&crate::files::read(path)?)
}

/// [`read_dds`] of bytes already in memory.
pub fn read_dds_bytes(b: &[u8]) -> Option<Image> {
    if b.get(..4)? != b"DDS " || b.len() < 148 || b.get(84..88)? != b"DX10" {
        return None;
    }
    let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let (height, width, mips) = (u(12), u(16), u(28).max(1));
    let format = match u(128) {
        71 | 72 => TextureFormat::Bc1RgbaUnorm,
        74 | 75 => TextureFormat::Bc2RgbaUnorm,
        77 | 78 => TextureFormat::Bc3RgbaUnorm,
        80 => TextureFormat::Bc4RUnorm,
        83 => TextureFormat::Bc5RgUnorm,
        28 | 29 => TextureFormat::Rgba8Unorm,
        _ => return None,
    };
    // DX10 misc flag 0x4 = TEXTURECUBE: six faces, each with its own mip chain (layer-major, as wgpu
    // expects for array uploads).
    let cube = u(136) & 4 != 0;
    let layers = if cube { 6 } else { 1 };
    let mut image = Image::new_uninit(Extent3d { width, height, depth_or_array_layers: layers }, TextureDimension::D2, format, RenderAssetUsages::RENDER_WORLD);
    image.texture_descriptor.mip_level_count = mips;
    image.data = Some(b[148..].to_vec());
    if cube {
        image.texture_view_descriptor = Some(bevy::render::render_resource::TextureViewDescriptor {
            dimension: Some(bevy::render::render_resource::TextureViewDimension::Cube),
            ..default()
        });
    }
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 8,
        ..ImageSamplerDescriptor::linear()
    });
    Some(image)
}

/// Seconds between cache sweeps, and how long an unreferenced entry is kept (tile-boundary churn).
const SWEEP_EVERY: f32 = 5.0;
const SWEEP_IDLE: f32 = 30.0;
