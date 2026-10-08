//! Colorado grass (and the far crowd cards that use the same system): the game's `Grass_*` objects
//! scattered at runtime exactly like FH1 does (`fh1_formats::grass`, docs/PROPS.md), streamed around
//! the camera per object and sub-layer, drawn as camera-facing cards (`grass.wgsl`).
//!
//! Input: the `grass` setup group (`grass/colorado/grass.bin`, `index.json`, `textures/*.dds`).
//! Each object has three sub-layers; sub-layer k is drawn within `distances[k]` of the camera
//! (VERIFIED against the `.pvsz` zone lists: docs/PROPS.md "Grass"). One mesh
//! per (object, sub-layer, texture), built on the async pool; 4 vertices per blade.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::primitives::Aabb;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, Extent3d, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError, TextureDimension, TextureFormat};
use bevy::shader::ShaderRef;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use fh1_formats::grass;

/// Extra range kept loaded past a sub-layer's distance (hysteresis).
const KEEP: f32 = 15.0;
/// Builds started per frame.
const STARTS_PER_FRAME: usize = 24;

pub struct GrassPlugin;

impl Plugin for GrassPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "grass.wgsl");
        app.add_plugins(MaterialPlugin::<GrassMaterial>::default()).add_systems(Update, stream);
    }
}

/// `grass.wgsl` `GrassParams`: per-material fade + the game's lighting/fog globals (copied from
/// fh1-render's FxGlobals each frame, so grass follows the time of day like the scenery).
#[derive(Clone, Copy, ShaderType, Debug, PartialEq)]
pub struct GrassParams {
    /// x = fade start, y = fade end, z = c0.z (1, UNVERIFIED), w = 1 to write sqrt(colour).
    pub fade: Vec4,
    pub sun_dir: Vec4,
    pub sun_color: Vec4,
    pub amb_color: Vec4,
    pub fog_consts: Vec4,
    pub fog_consts2: Vec4,
    pub fog_color: Vec4,
    pub fog_color2: Vec4,
}

/// Lighting/fog globals shared by every grass material.
#[derive(Clone, Copy, PartialEq)]
struct Globals {
    raw_output: bool,
    v: [Vec4; 7],
}

impl Default for Globals {
    fn default() -> Self {
        // Without fh1-render's time of day: a plain noon sun, no fog.
        Self { raw_output: false, v: [Vec4::new(0.4, 0.8, 0.3, 0.0).normalize(), Vec4::splat(1.0), Vec4::splat(0.4), Vec4::new(1e7, 0.0, 0.0, 0.0), Vec4::new(0.0, 1.0, 0.0, 1.0), Vec4::ZERO, Vec4::new(0.0, 0.0, 0.0, 1.0)] }
    }
}

impl Globals {
    fn read(fx: Option<&fh1_render::FxGlobals>, lib: Option<&fh1_render::FxLibrary>) -> Self {
        let mut g = Self { raw_output: lib.is_some_and(|l| l.raw_output), ..default() };
        if let Some(fx) = fx {
            for (slot, name) in g.v.iter_mut().zip(["sunDir", "sunColor", "ambColor", "FogConsts", "FogConsts2", "FogColor", "FogColor2"]) {
                if let Some(v) = fx.get(name) {
                    *slot = v;
                }
            }
        }
        g
    }

    fn params(&self, range: [f32; 2]) -> GrassParams {
        let v = self.v;
        GrassParams {
            fade: Vec4::new(range[0], range[1], 1.0, self.raw_output as u32 as f32),
            sun_dir: v[0],
            sun_color: v[1],
            amb_color: v[2],
            fog_consts: v[3],
            fog_consts2: v[4],
            fog_color: v[5],
            fog_color2: v[6],
        }
    }
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct GrassMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,
    #[uniform(2)]
    pub params: GrassParams,
}

impl Material for GrassMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/grass.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/grass.wgsl".into()
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
            Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(5),
        ])?];
        d.primitive.cull_mode = None;
        Ok(())
    }
}

struct Object {
    offset: usize,
    len: usize,
    min: Vec3,
    max: Vec3,
    distances: [f32; 4],
}

/// Per texture: the mesh data of one (object, sub-layer).
type Built = Vec<(u32, Mesh, Aabb)>;

#[derive(Resource)]
struct GrassWorld {
    dir: PathBuf,
    blob: Arc<Vec<u8>>,
    objects: Vec<Object>,
    textures: HashMap<u32, String>,
    images: HashMap<u32, Handle<Image>>,
    materials: HashMap<(u32, [u32; 2]), Handle<GrassMaterial>>,
    globals: Globals,
    /// Seconds since the materials last took the globals (throttle, see `stream`).
    globals_age: f32,
    loaded: HashMap<(usize, usize), Vec<Entity>>,
    pending: HashMap<(usize, usize), Task<Built>>,
    /// Blades per loaded (object, sub-layer), for FH1_GRASS_STATS.
    blades: HashMap<(usize, usize), usize>,
}

impl GrassWorld {
    fn load(assets: &Path) -> Option<Self> {
        let dir = assets.join("grass/colorado");
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).ok()?).ok()?;
        let blob = Arc::new(std::fs::read(dir.join("grass.bin")).ok()?);
        let v3 = |v: &serde_json::Value| Some(Vec3::new(v[0].as_f64()? as f32, v[1].as_f64()? as f32, v[2].as_f64()? as f32));
        let objects = index["objects"]
            .as_array()?
            .iter()
            .filter_map(|o| {
                let d = o["distances"].as_array()?;
                Some(Object {
                    offset: o["offset"].as_u64()? as usize,
                    len: o["len"].as_u64()? as usize,
                    min: v3(&o["min"])?,
                    max: v3(&o["max"])?,
                    distances: [0, 1, 2, 3].map(|k| d.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32),
                })
            })
            .collect::<Vec<_>>();
        let textures = index["textures"].as_object()?.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_str()?.to_owned()))).collect();
        info!("grass: {} objects", objects.len());
        Some(Self { dir, blob, objects, textures, images: HashMap::new(), materials: HashMap::new(), globals: Globals::default(), globals_age: 0.0, loaded: HashMap::new(), pending: HashMap::new(), blades: HashMap::new() })
    }

    fn material(&mut self, texture: u32, range: [f32; 2], images: &mut Assets<Image>, materials: &mut Assets<GrassMaterial>) -> Option<Handle<GrassMaterial>> {
        let key = (texture, range.map(f32::to_bits));
        if let Some(m) = self.materials.get(&key) {
            return Some(m.clone());
        }
        let image = match self.images.get(&texture) {
            Some(h) => h.clone(),
            None => {
                let h = images.add(read_dds(&self.dir.join(self.textures.get(&texture)?))?);
                self.images.insert(texture, h.clone());
                h
            }
        };
        let m = materials.add(GrassMaterial { texture: image, params: self.globals.params(range) });
        self.materials.insert(key, m.clone());
        Some(m)
    }
}


/// Scatters one sub-layer of an object and builds a card mesh per texture, positions relative to
/// `origin` (engine space).
fn build(bytes: &[u8], sub: usize, origin: Vec3) -> Built {
    let Ok(g) = grass::parse(bytes) else { return Vec::new() };
    let blades = g.scatter(sub);
    let mut by_tex: HashMap<u32, Vec<&grass::Blade>> = HashMap::new();
    for b in &blades {
        by_tex.entry(g.batches[b.batch as usize].texture).or_default().push(b);
    }
    by_tex
        .into_iter()
        .map(|(tex, list)| {
            let n = list.len();
            let mut nrm = Vec::with_capacity(4 * n);
            let (mut pos, mut uv, mut corner, mut col) = (Vec::with_capacity(4 * n), Vec::with_capacity(4 * n), Vec::with_capacity(4 * n), Vec::with_capacity(4 * n));
            let mut idx = Vec::with_capacity(6 * n);
            let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for b in list {
                // Collision space (left-handed) -> engine space: negate Z.
                let p = Vec3::new(b.position[0], b.position[1], -b.position[2]) - origin;
                let r = (b.width * 0.5).max(b.height);
                lo = lo.min(p - Vec3::splat(r));
                hi = hi.max(p + Vec3::new(r, b.height, r));
                let t = &g.types[b.blade_type as usize];
                let [ul, ur, vb, vt] = t.uv;
                let w = b.width * 0.5;
                let i = pos.len() as u32;
                for (c, u) in [([-w, 0.0], [ul, vb]), ([w, 0.0], [ur, vb]), ([w, b.height], [ur, vt]), ([-w, b.height], [ul, vt])] {
                    pos.push(p.to_array());
                    nrm.push([b.normal[0], b.normal[1], -b.normal[2]]);
                    corner.push(c);
                    uv.push(u);
                    let s = b.shade();
                    col.push([s, s, s, 1.0]);
                }
                idx.extend_from_slice(&[i, i + 1, i + 2, i, i + 2, i + 3]);
            }
            let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
            mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
            mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nrm);
            mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
            mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, corner);
            mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, col);
            mesh.insert_indices(Indices::U32(idx));
            (tex, mesh, Aabb::from_min_max(lo, hi))
        })
        .collect()
}

/// Horizontal distance from `p` to the object's bounds.
fn distance(o: &Object, p: Vec3) -> f32 {
    let c = Vec2::new(p.x.clamp(o.min.x, o.max.x), p.z.clamp(o.min.z, o.max.z));
    c.distance(Vec2::new(p.x, p.z))
}

#[allow(clippy::too_many_arguments)]
fn stream(
    mut commands: Commands,
    mut world: Local<Option<GrassWorld>>,
    // World generation the cache was built for (X1c: an in-process map change drops it; its entities are WorldEntity).
    mut tried: Local<Option<u32>>,
    mut stats: Local<(f32, u32, f32)>,
    time: Res<Time>,
    (garage, generation): (Res<crate::Garage>, Res<crate::ui::world_load::WorldGeneration>),
    scenery: Option<Res<crate::scenery::Scenery>>,
    cameras: Query<&GlobalTransform, With<fh1_render::post::FxPostCamera>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<GrassMaterial>>,
    fx: Option<Res<fh1_render::FxGlobals>>,
    lib: Option<Res<fh1_render::FxLibrary>>,
) {
    // Colorado only (the scenery resource exists when the track has a converted world; FH2's Anthem has no grass data).
    if !scenery.as_ref().is_some_and(|s| s.colorado) {
        return;
    }
    // FH1_GRASS_STATS=1: log mean / worst frame time and the blades drawn every 5 s.
    if std::env::var("FH1_GRASS_STATS").is_ok() {
        let dt = time.delta_secs();
        *stats = (stats.0 + dt, stats.1 + 1, stats.2.max(dt));
        if stats.0 >= 5.0 {
            let (blades, meshes, pending) = world.as_ref().map_or((0, 0, 0), |w| (w.blades.values().sum::<usize>(), w.loaded.len(), w.pending.len()));
            info!("grass: {:.2} ms mean, {:.2} ms worst, {} blades in {} sub-layer meshes, {} pending", 1000.0 * stats.0 / stats.1 as f32, 1000.0 * stats.2, blades, meshes, pending);
            *stats = (0.0, 0, 0.0);
        }
    }
    if *tried != Some(generation.0) {
        *tried = Some(generation.0);
        *world = None;
        // FH1_GRASS=0 turns grass off (frame-time comparisons).
        if std::env::var("FH1_GRASS").is_ok_and(|v| v == "0") {
            info!("grass: off (FH1_GRASS=0)");
            return;
        }
        *world = GrassWorld::load(&garage.assets);
        if world.is_none() {
            info!("grass: not installed (run fh1setup --only grass)");
        }
    }
    let (Some(w), Some(cam)) = (world.as_mut(), cameras.iter().next()) else { return };
    let here = cam.translation();

    // Time of day: refresh every material when the game's lighting/fog globals change, at most every 0.5 s
    // (FH1_GRASS_GLOBALS_SECS; 0 = every change): the running clock changes them every frame, and each write re-prepares
    // every grass material in the render world (2026-10-08 perf, user log 101726: GrassMaterial prepare 0.16 ms/frame).
    let globals = Globals::read(fx.as_deref(), lib.as_deref());
    let period = std::env::var("FH1_GRASS_GLOBALS_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5f32);
    w.globals_age += time.delta_secs();
    if globals != w.globals && (w.globals_age >= period || w.globals == Globals::default()) {
        w.globals_age = 0.0;
        w.globals = globals;
        for (key, h) in &w.materials {
            if let Some(mut m) = materials.get_mut(h) {
                m.params = globals.params(key.1.map(f32::from_bits));
            }
        }
    }

    // Finished builds -> entities.
    let done: Vec<(usize, usize)> = w.pending.iter().filter(|(_, t)| t.is_finished()).map(|(k, _)| *k).collect();
    for key in done {
        let built = block_on(future::poll_once(w.pending.remove(&key).unwrap())).unwrap_or_default();
        let o = &w.objects[key.0];
        let (origin, d) = (o.min, o.distances);
        // Cards shrink away over the last stretch of the range (fade length = header d[3], UNVERIFIED).
        let range = [(d[key.1] - d[3].min(d[key.1] * 0.5)).max(0.0), d[key.1]];
        let mut ents = Vec::new();
        w.blades.insert(key, built.iter().map(|b| b.1.count_vertices() / 4).sum());
        for (tex, mesh, aabb) in built {
            let Some(m) = w.material(tex, range, &mut images, &mut materials) else { continue };
            let e = commands.spawn((Mesh3d(meshes.add(mesh)), MeshMaterial3d(m), aabb, Transform::from_translation(origin), crate::ui::world_load::WorldEntity)).id();
            // Remaster: Bevy's sun cascades would draw every grass card into all three (faithful shadows never had them).
            // FH1_RM_SMALL_CASTERS=0 = they cast.
            if fh1_remaster::enabled() && std::env::var("FH1_RM_SMALL_CASTERS").as_deref() != Ok("0") {
                commands.entity(e).insert(bevy::light::NotShadowCaster);
            }
            ents.push(e);
        }
        w.loaded.insert(key, ents);
    }

    // Unload out of range.
    let gone: Vec<(usize, usize)> = w.loaded.keys().filter(|&&(i, s)| distance(&w.objects[i], here) > w.objects[i].distances[s] + KEEP).copied().collect();
    for key in gone {
        w.blades.remove(&key);
        for e in w.loaded.remove(&key).unwrap() {
            commands.entity(e).despawn();
        }
    }
    w.pending.retain(|&(i, s), _| distance(&w.objects[i], here) <= w.objects[i].distances[s] + KEEP);

    // Start builds in range, nearest first.
    let mut want: Vec<(f32, usize, usize)> = Vec::new();
    for (i, o) in w.objects.iter().enumerate() {
        let dist = distance(o, here);
        for s in 0..3 {
            if dist <= o.distances[s] && !w.loaded.contains_key(&(i, s)) && !w.pending.contains_key(&(i, s)) {
                want.push((dist, i, s));
            }
        }
    }
    want.sort_by(|a, b| a.0.total_cmp(&b.0));
    let pool = AsyncComputeTaskPool::get();
    for &(_, i, s) in want.iter().take(STARTS_PER_FRAME) {
        let (blob, o) = (w.blob.clone(), &w.objects[i]);
        let (range, origin) = (o.offset..o.offset + o.len, o.min);
        w.pending.insert((i, s), pool.spawn(async move { build(&blob[range], s, origin) }));
    }
}

/// DX10 DDS as fh1setup writes them (BCn, largest level first), colour read as sRGB.
fn read_dds(path: &Path) -> Option<Image> {
    let b = std::fs::read(path).ok()?;
    if b.get(..4)? != b"DDS " || b.len() < 148 || b.get(84..88)? != b"DX10" {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let (height, width, mips) = (u32_at(12), u32_at(16), u32_at(28).max(1));
    let format = match u32_at(128) {
        28 | 29 => TextureFormat::Rgba8UnormSrgb,
        71 | 72 => TextureFormat::Bc1RgbaUnormSrgb,
        74 | 75 => TextureFormat::Bc2RgbaUnormSrgb,
        77 | 78 => TextureFormat::Bc3RgbaUnormSrgb,
        _ => return None,
    };
    let mut image = Image::new_uninit(Extent3d { width, height, depth_or_array_layers: 1 }, TextureDimension::D2, format, RenderAssetUsages::RENDER_WORLD);
    image.texture_descriptor.mip_level_count = mips;
    image.data = Some(b[148..].to_vec());
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 4,
        ..ImageSamplerDescriptor::linear()
    });
    Some(image)
}
