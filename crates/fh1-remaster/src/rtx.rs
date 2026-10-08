//! Remaster RTX mode (cargo feature `rtx`, runtime `FH1_RTX=1` with `FH1_RENDERER=remaster`; remaster-rtx.bat).
//!
//! Ray-traced lighting through bevy_solarik v0.1.0 (vendor/bevy_solarik, MIT/Apache: a bevy_solari 0.19.1 fork with a sky
//! term, alpha-tested geometry, point/spot lights and DLSS Ray Reconstruction). Findings reused from the rtx-solari
//! experiment (branch `rtx-solari`, its docs/RTX.md):
//!
//! - Solarik traces and GBuffers only `StandardMaterial` meshes drawn DEFERRED, with exactly POSITION / NORMAL / UV_0 /
//!   TANGENT and u32 indices. So in RTX mode every remaster scenery entity (`RemasterMaterial` = StandardMaterial +
//!   SceneryExt) and every car paint entity (`CarPaintMaterial`) is switched to a StandardMaterial PROXY built from its
//!   base: the DOMINANT layer of layered ground (road / vblend: mean vertex-colour weights of the mesh; splat: the weight
//!   map's mean from its 1x1 mip) with that layer's UV scale, else layer A (no per-pixel blending, vertex colour, AO,
//!   normal maps or night lightmaps), roughness/reflectance from the record's spec level/power (docs/REMASTER.md
//!   "Shading"), paint colour for cars. The original material handle stays on the entity (`RtxSource`) so W1's
//!   cache/eviction see it in use.
//! - Raster-only classes (Decal, Water, Additive, Unlit) are never traced, so they keep their own `RemasterMaterial`
//!   (forward, vertex alpha / blending intact). As proxies the 1,096 road `_blend` overlays (opacity = 1 - vertex colour
//!   b) drew OPAQUE layer A over roads and the festival plaza: the flat brown "missing ground". `FH1_RTX_PROXY_ALL=1` =
//!   old (every class proxied, layer A only).
//! - Meshes are stripped in PostUpdate, before their first extraction (RENDER_WORLD meshes lose their data then).
//!   Tangents are dummies (no normal maps; mikktspace on the whole scenery would hitch). Skinned meshes are not traced.
//! - Raytracing ignores visibility: only visible entities, and for `VisibilityRange` LODs only the band holding the camera
//!   distance, are put in the TLAS (`sync_traced`; else hidden collision walls and overlapping LODs shadow the world).
//! - Blended / additive / unlit materials stay forward-rendered and untraced; Solarik replaces the shadow maps.
//! - Sky light: the remaster's baked atmosphere environment map (the camera's `EnvironmentMapLight`, light.rs) is the cube
//!   that escaped rays read (`SolarikSkyLight`); the visible sky is still Bevy's atmosphere.
//! - Denoise/upscale: DLSS Ray Reconstruction when `nvngx_dlssd.dll` sits next to the exe (remaster-rtx.bat copies it),
//!   else TAA. `FH1_RTX_DLSS=dlaa|quality|balanced|performance|ultra` (default auto).
//!
//! Tuning: `FH1_RTX_SKY=<x>` sky light scale (default 1 x the env map intensity), `FH1_RTX_EV=<stops>` exposure offset.

use bevy::anti_alias::dlss::{Dlss, DlssPerfQualityMode, DlssProjectId, DlssRayReconstructionFeature, DlssRayReconstructionSupported};
use bevy::anti_alias::taa::TemporalAntiAliasing;
use bevy::camera::visibility::{VisibilityRange, VisibilitySystems};
use bevy::camera::CameraMainTextureUsages;
use bevy::light::EnvironmentMapLight;
use bevy::material::OpaqueRendererMethod;
use bevy::mesh::skinning::SkinnedMesh;
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::render::render_resource::TextureUsages;
use bevy::render::view::ColorGrading;
use bevy_solarik::prelude::{RaytracingMesh3d, SolarikLighting, SolarikPlugins, SolarikSkyLight};
use fh1_render::post::FxPostCamera;

use crate::car_paint::CarPaintMaterial;
use crate::material::{RemasterMaterial, ROLE_A, ROLE_B, ROLE_C, ROLE_W};
use crate::scenery::RemasterScenery;

/// RTX mode: remaster renderer + `FH1_RTX=1` (rtx builds only).
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::enabled() && std::env::var("FH1_RTX").is_ok_and(|v| v == "1"))
}

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Before `DefaultPlugins`: the `dlss` Bevy feature needs the project id to exist when its plugin builds (RTX mode or not).
pub fn pre_default_plugins(app: &mut App) {
    app.insert_resource(DlssProjectId(bevy::asset::uuid::Uuid::from_u128(0xb1279bd4_dff5_4cfc_83d4_d2909726b9dc)));
}

pub struct RtxPlugin;

impl Plugin for RtxPlugin {
    fn build(&self, app: &mut App) {
        if !on() {
            return;
        }
        app.add_plugins(SolarikPlugins);
        // Solarik makes DEFERRED the default for every material; only the StandardMaterials this module picks go deferred.
        // Otherwise every Auto opaque on a camera without a deferred prepass (minimap ribbons, UI scene) is never drawn.
        app.insert_resource(bevy::pbr::DefaultOpaqueRendererMethod::forward());
        app.init_resource::<RtxCache>()
            .add_systems(Update, (setup_camera, update_sky))
            // After light.rs update_lights (before Propagate), which re-enables shadow maps every frame.
            .add_systems(PostUpdate, lights_no_shadow_maps.after(bevy::transform::TransformSystems::Propagate))
            // PostUpdate: meshes spawned this frame still hold their data (RENDER_WORLD-only meshes drop it at extraction).
            .add_systems(PostUpdate, ((proxy_scenery, proxy_paint), ApplyDeferred, trace_meshes, sync_traced).chain().after(VisibilitySystems::VisibilityPropagate));
        info!("rtx: remaster RTX mode ON (bevy_solarik ray-traced lighting)");
    }
}

#[derive(Resource, Default)]
struct RtxCache {
    /// Mesh -> made traceable (true) or not usable (false).
    meshes: HashMap<AssetId<Mesh>, bool>,
    /// StandardMaterials already switched to deferred.
    deferred: HashSet<AssetId<StandardMaterial>>,
    /// (Remaster scenery material, base layer role) -> its StandardMaterial proxy.
    scenery: HashMap<(AssetId<RemasterMaterial>, usize), Handle<StandardMaterial>>,
    /// Road / vblend mesh -> its dominant layer (from the vertex colours, read once while the mesh still has data).
    mesh_layer: HashMap<AssetId<Mesh>, usize>,
    /// Splat weight map -> mean (r, g, b) from its 1x1 mip (None = unreadable).
    weight_mean: HashMap<AssetId<Image>, Option<[f32; 3]>>,
    /// Car paint material -> its proxy.
    paint: HashMap<AssetId<CarPaintMaterial>, Handle<StandardMaterial>>,
    traced: usize,
    logged: f32,
}

/// The material the entity had before RTX mode swapped in its proxy (kept alive while the entity lives).
#[derive(Component)]
#[allow(dead_code)]
enum RtxSource {
    Scenery(Handle<RemasterMaterial>),
    Paint(Handle<CarPaintMaterial>),
}

/// Mesh entities already looked at.
#[derive(Component)]
struct RtxSeen;

/// Remaster scenery entities that keep their own material (raster-only classes).
#[derive(Component)]
struct RtxKeep;

#[derive(Component)]
struct RtxCamera;

/// A traceable mesh entity (its `RaytracingMesh3d` comes and goes with `sync_traced`).
#[derive(Component)]
struct RtxTraceable(Handle<Mesh>);

/// Main camera: Solarik + DLSS Ray Reconstruction (else TAA). Every other camera: MSAA off (one MSAA per window).
#[allow(clippy::type_complexity)]
fn setup_camera(
    mut commands: Commands,
    cams: Query<Entity, (With<FxPostCamera>, Without<RtxCamera>)>,
    others: Query<Entity, (With<Camera>, Without<FxPostCamera>, Without<RtxCamera>)>,
    rr: Option<Res<DlssRayReconstructionSupported>>,
) {
    for e in &cams {
        let mut grading = ColorGrading::default();
        grading.global.exposure = env_f32("FH1_RTX_EV", 0.0);
        let mut ec = commands.entity(e);
        ec.insert((RtxCamera, SolarikLighting::default(), CameraMainTextureUsages::default().with(TextureUsages::STORAGE_BINDING), Msaa::Off, grading));
        if rr.is_some() {
            let perf_quality_mode = match std::env::var("FH1_RTX_DLSS").as_deref() {
                Ok("dlaa") => DlssPerfQualityMode::Dlaa,
                Ok("quality") => DlssPerfQualityMode::Quality,
                Ok("balanced") => DlssPerfQualityMode::Balanced,
                Ok("performance") => DlssPerfQualityMode::Performance,
                Ok("ultra") => DlssPerfQualityMode::UltraPerformance,
                _ => DlssPerfQualityMode::Auto,
            };
            ec.insert(Dlss::<DlssRayReconstructionFeature> { perf_quality_mode, reset: Default::default(), _phantom_data: Default::default() });
            info!("rtx: main camera = Solarik + DLSS Ray Reconstruction ({perf_quality_mode:?})");
        } else {
            ec.insert(TemporalAntiAliasing::default());
            warn!("rtx: DLSS Ray Reconstruction not available (nvngx_dlssd.dll next to the exe? NVIDIA RTX GPU?): TAA only, the frame will be noisy");
        }
    }
    for e in &others {
        commands.entity(e).insert((RtxCamera, Msaa::Off));
    }
}

/// Solarik replaces shadow maps (light.rs turns them on every frame).
fn lights_no_shadow_maps(mut lights: Query<&mut DirectionalLight>) {
    for mut l in &mut lights {
        if l.shadow_maps_enabled {
            l.shadow_maps_enabled = false;
        }
    }
}

/// Sky light for escaped rays = the camera's baked atmosphere environment map (light.rs env_refresh swaps in a new one
/// when the sun moves), its specular cube at mip 0, at the env map's intensity x `FH1_RTX_SKY`.
fn update_sky(mut commands: Commands, cams: Query<&EnvironmentMapLight, (With<RtxCamera>, With<FxPostCamera>)>, mut last: Local<Option<(AssetId<Image>, f32)>>) {
    let Some(env) = cams.iter().next() else { return };
    let intensity = env.intensity * env_f32("FH1_RTX_SKY", 1.0);
    let now = (env.specular_map.id(), intensity);
    if *last != Some(now) {
        *last = Some(now);
        commands.insert_resource(SolarikSkyLight { image: Some(env.specular_map.clone()), intensity });
        info!("rtx: sky light = atmosphere env map {:?} x {intensity}", env.specular_map.id());
    }
}

/// Bevy roughness/reflectance from the game's Blinn-Phong spec level / power (docs/REMASTER.md "Shading").
fn spec_to_pbr(level: f32, power: f32) -> (f32, f32) {
    if level <= 0.0 || power <= 0.0 {
        return (0.95, 0.25);
    }
    let alpha = (2.0 / (power + 2.0)).sqrt();
    (alpha.sqrt().clamp(0.08, 1.0), 0.25 + (0.6 - 0.25) * (2.0 * level).clamp(0.0, 1.0))
}

/// Material class (record byte 0; material.rs `Class`) of a remaster material.
fn class_of(m: &RemasterMaterial) -> u32 {
    (m.extension.params.info.z >> 16) & 0xff
}

/// Decal / Water / Additive / Skip / Unlit: blended or unlit, never traced: they keep the remaster material.
fn raster_class(class: u32) -> bool {
    matches!(class, 2 | 4 | 5 | 6 | 7)
}

/// `FH1_RTX_PROXY_ALL=1`: the old behaviour (every class proxied, layer A only).
fn proxy_all() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RTX_PROXY_ALL").is_ok_and(|v| v == "1"))
}

/// Layer weights (A, B, C) of a road / vblend mesh: the remaster shader's per-vertex blend (material.rs: road wb =
/// sat((vc.r - 0.5) c4.x + (noise.r - 0.5) c4.y + 0.5), wc likewise from vc.g / c4.z, w; vblend only wb from vc.r), with
/// the noise term at its mean (0) and summed over the vertices. None when the mesh has no vertex colours (or no data).
fn vertex_layer_weights(mesh: &Mesh, c4: Vec4, road: bool, has_b: bool, has_c: bool) -> Option<[f32; 3]> {
    if mesh.try_attributes().is_err() {
        return None;
    }
    let Some(VertexAttributeValues::Unorm8x4(c)) = mesh.attribute(fh1_render::material::ATTRIBUTE_COLOR) else { return None };
    if c.is_empty() {
        return None;
    }
    let mut w = [0.0f32; 3];
    for x in c {
        // Fx_Color bytes are A, R, G, B (material.rs `.yzwx`).
        let (r, g) = (x[1] as f32 / 255.0, x[2] as f32 / 255.0);
        let wb = if has_b { ((r - 0.5) * c4.x + 0.5).clamp(0.0, 1.0) } else { 0.0 };
        let wc = if has_c && road { ((g - 0.5) * c4.z + 0.5).clamp(0.0, 1.0) } else { 0.0 };
        w[0] += (1.0 - wb) * (1.0 - wc);
        w[1] += wb * (1.0 - wc);
        w[2] += wc;
    }
    Some(w)
}

/// Mean colour (0..1) of a DDS from its last (1x1) mip block: BC1/2/3 the two colour endpoints, BC4 the two red
/// endpoints, RGBA8 the texel. None unless the file holds one 2D image with the full mip chain.
fn dds_mean(path: &std::path::Path) -> Option<[f32; 3]> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = [0u8; 148];
    f.read_exact(&mut h[..128]).ok()?;
    if &h[..4] != b"DDS " {
        return None;
    }
    let u32_at = |h: &[u8], o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
    let (height, width, mips) = (u32_at(&h, 12), u32_at(&h, 16), u32_at(&h, 28).max(1));
    if width == 0 || height == 0 || mips != width.max(height).ilog2() + 1 {
        return None;
    }
    let format = match &h[84..88] {
        b"DX10" => {
            f.read_exact(&mut h[128..148]).ok()?;
            u32_at(&h, 128)
        }
        b"DXT1" => 71,
        b"DXT3" => 74,
        b"DXT5" => 77,
        b"ATI1" | b"BC4U" => 80,
        _ => return None,
    };
    // DXGI: BC1 71/72, BC2 74/75, BC3 77/78, BC4 80/81, RGBA8 28/29, BGRA8 87/91.
    let size: u64 = match format {
        71 | 72 | 80 | 81 => 8,
        74 | 75 | 77 | 78 => 16,
        28 | 29 | 87 | 91 => 4,
        _ => return None,
    };
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.checked_sub(size)?)).ok()?;
    let mut b = [0u8; 16];
    f.read_exact(&mut b[..size as usize]).ok()?;
    let rgb565 = |v: u16| [((v >> 11) & 31) as f32 / 31.0, ((v >> 5) & 63) as f32 / 63.0, (v & 31) as f32 / 31.0];
    let endpoints = |o: usize| {
        let (c0, c1) = (rgb565(u16::from_le_bytes([b[o], b[o + 1]])), rgb565(u16::from_le_bytes([b[o + 2], b[o + 3]])));
        [0.5 * (c0[0] + c1[0]), 0.5 * (c0[1] + c1[1]), 0.5 * (c0[2] + c1[2])]
    };
    Some(match format {
        71 | 72 => endpoints(0),
        74 | 75 | 77 | 78 => endpoints(8),
        80 | 81 => {
            let r = 0.5 * (b[0] as f32 + b[1] as f32) / 255.0;
            [r, r, r]
        }
        87 | 91 => [b[2] as f32 / 255.0, b[1] as f32 / 255.0, b[0] as f32 / 255.0],
        _ => [b[0] as f32 / 255.0, b[1] as f32 / 255.0, b[2] as f32 / 255.0],
    })
}

/// The layer role (material.rs ROLE_A / ROLE_B / ROLE_C) that covers most of an entity's surface, for its proxy's base.
fn dominant_layer(m: &RemasterMaterial, mesh: Option<(AssetId<Mesh>, Option<&Mesh>)>, sc: Option<&RemasterScenery>, cache: &mut RtxCache) -> usize {
    let p = &m.extension.params;
    let e = &m.extension;
    let present = |role: usize| p.info.y & (1 << role) != 0;
    let (has_b, has_c) = (present(ROLE_B) && e.b.is_some(), present(ROLE_C) && e.c.is_some());
    if !has_b && !has_c {
        return ROLE_A;
    }
    let w = match p.info.w {
        // Splat: col = mix(A, B, w.r), then mix(.., C, w.g x c.a) with w = the weight map (material.rs LAYER_SPLAT).
        1 => {
            let Some(h) = e.weight.as_ref().filter(|_| present(ROLE_W)) else { return ROLE_A };
            let mean = *cache.weight_mean.entry(h.id()).or_insert_with(|| sc.and_then(|sc| sc.image_path(h.id())).and_then(|path| dds_mean(&path)));
            let Some(mean) = mean else { return ROLE_A };
            let wb = if has_b { mean[0] } else { 0.0 };
            let wc = if has_c { mean[1] } else { 0.0 };
            [(1.0 - wb) * (1.0 - wc), wb * (1.0 - wc), wc]
        }
        // Road / vblend: per-vertex weights from the vertex colours, read while the mesh still has its data.
        layering @ (2 | 3) => {
            let Some((id, data)) = mesh else { return ROLE_A };
            if let Some(&layer) = cache.mesh_layer.get(&id) {
                return layer;
            }
            let Some(w) = data.and_then(|d| vertex_layer_weights(d, p.p[1], layering == 2, has_b, has_c)) else { return ROLE_A };
            let layer = [ROLE_A, ROLE_B, ROLE_C][(0..3).max_by(|&a, &b| w[a].total_cmp(&w[b])).unwrap_or(0)];
            cache.mesh_layer.insert(id, layer);
            return layer;
        }
        _ => return ROLE_A,
    };
    [ROLE_A, ROLE_B, ROLE_C][(0..3).max_by(|&a, &b| w[a].total_cmp(&w[b])).unwrap_or(0)]
}

/// The StandardMaterial that stands in for a remaster scenery material under ray tracing, with `layer` as its base.
fn scenery_proxy(m: &RemasterMaterial, layer: usize) -> StandardMaterial {
    let mut s = m.base.clone();
    let p0 = m.extension.params.p[0];
    let uv = m.extension.params.uv;
    let (tex, scale) = match layer {
        ROLE_B => (m.extension.b.clone(), Vec2::new(uv[0].z, uv[0].w)),
        ROLE_C => (m.extension.c.clone(), Vec2::new(uv[1].x, uv[1].y)),
        _ => (None, Vec2::new(uv[0].x, uv[0].y)),
    };
    if let Some(t) = tex {
        // A blend layer as the base: its own texture and UV scale (every B / C layer is on UV set 0).
        s.base_color_texture = Some(t);
        s.uv_transform = bevy::math::Affine2::from_scale(scale);
    } else if s.base_color_texture.is_none() {
        s.base_color_texture = m.extension.a.clone();
    }
    let class = class_of(m);
    let (rough, refl) = spec_to_pbr(p0.y, p0.z);
    s.perceptual_roughness = rough;
    s.reflectance = refl;
    let albedo = if p0.x > 0.0 && p0.x <= 4.0 { p0.x } else { 1.0 };
    match class {
        // Decal: texture alpha x vertex alpha in the remaster shader; texture alpha here.
        2 => s.base_color = Color::WHITE,
        // Water placeholder: dark, glossy, mostly opaque.
        4 => {
            s.base_color = Color::srgba(0.03, 0.05, 0.06, 0.85);
            s.base_color_texture = None;
            s.perceptual_roughness = 0.08;
            s.reflectance = 0.5;
        }
        // Additive glows: the texture, added.
        5 => s.base_color = Color::WHITE,
        _ => s.base_color = Color::linear_rgb(albedo, albedo, albedo),
    }
    s
}

/// Remaster scenery entities -> StandardMaterial proxies (raster-only classes keep their material, `RtxKeep`).
#[allow(clippy::too_many_arguments)]
fn proxy_scenery(
    mut commands: Commands,
    q: Query<(Entity, &MeshMaterial3d<RemasterMaterial>, Option<&Mesh3d>), Without<RtxKeep>>,
    rm: Res<Assets<RemasterMaterial>>,
    meshes: Res<Assets<Mesh>>,
    sc: Option<Res<RemasterScenery>>,
    mut std_mats: ResMut<Assets<StandardMaterial>>,
    mut cache: ResMut<RtxCache>,
    time: Res<Time>,
    mut pruned: Local<f32>,
) {
    let all = proxy_all();
    for (e, h, mesh) in &q {
        let Some(m) = rm.get(&h.0) else { continue };
        if !all && raster_class(class_of(m)) {
            commands.entity(e).insert(RtxKeep);
            continue;
        }
        let layer = if all { ROLE_A } else { dominant_layer(m, mesh.map(|x| (x.0.id(), meshes.get(&x.0))), sc.as_deref(), &mut cache) };
        let proxy = match cache.scenery.get(&(h.0.id(), layer)) {
            Some(p) => p.clone(),
            None => {
                let p = std_mats.add(scenery_proxy(m, layer));
                cache.scenery.insert((h.0.id(), layer), p.clone());
                p
            }
        };
        commands.entity(e).remove::<MeshMaterial3d<RemasterMaterial>>().insert((MeshMaterial3d(proxy), RtxSource::Scenery(h.0.clone())));
    }
    // Proxies of evicted remaster materials go too, and the per-mesh layer picks of meshes that are gone.
    if time.elapsed_secs() - *pruned > 10.0 {
        *pruned = time.elapsed_secs();
        cache.scenery.retain(|(id, _), _| rm.contains(*id));
        cache.mesh_layer.retain(|id, _| meshes.contains(*id));
    }
}

/// Car paint entities -> StandardMaterial proxies (paint colour x atlas; no flakes).
fn proxy_paint(mut commands: Commands, q: Query<(Entity, &MeshMaterial3d<CarPaintMaterial>)>, pm: Res<Assets<CarPaintMaterial>>, mut std_mats: ResMut<Assets<StandardMaterial>>, mut cache: ResMut<RtxCache>) {
    for (e, h) in &q {
        let proxy = match cache.paint.get(&h.0.id()) {
            Some(p) => p.clone(),
            None => {
                let Some(m) = pm.get(&h.0) else { continue };
                let mut s = m.base.clone();
                let k = s.base_color.to_linear();
                let c = m.extension.colour;
                s.base_color = Color::LinearRgba(LinearRgba::new(c.red * k.red, c.green * k.green, c.blue * k.blue, 1.0));
                let p = std_mats.add(s);
                cache.paint.insert(h.0.id(), p.clone());
                p
            }
        };
        commands.entity(e).remove::<MeshMaterial3d<CarPaintMaterial>>().insert((MeshMaterial3d(proxy), RtxSource::Paint(h.0.clone())));
    }
}

/// Every new StandardMaterial mesh: material to deferred (GBuffer), mesh made traceable, `RtxTraceable`.
#[allow(clippy::type_complexity)]
fn trace_meshes(
    mut commands: Commands,
    new: Query<(Entity, &Mesh3d, &MeshMaterial3d<StandardMaterial>, Has<SkinnedMesh>), Without<RtxSeen>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut cache: ResMut<RtxCache>,
    time: Res<Time>,
) {
    for (e, mesh, material, skinned) in &new {
        let Some(m) = materials.get(&material.0) else { continue };
        let blended = matches!(m.alpha_mode, AlphaMode::Blend | AlphaMode::Premultiplied | AlphaMode::Add | AlphaMode::Multiply | AlphaMode::AlphaToCoverage);
        let raster_only = blended || m.unlit;
        let normal_mapped = m.normal_map_texture.is_some();
        // The mesh first: a RENDER_WORLD-only mesh loses its data once extracted.
        let id = mesh.0.id();
        let usable = match cache.meshes.get(&id) {
            Some(&u) => u,
            // Skinned meshes keep their joints (stripping them breaks the skinned prepass); raster-only ones are left alone.
            None if skinned || raster_only => false,
            None => {
                // Read-only check first: get_mut on an already-extracted mesh re-queues it (extraction errors).
                let Some(mesh_ref) = meshes.get(&mesh.0) else { continue };
                let u = if mesh_ref.try_attributes().is_err() || mesh_ref.primitive_topology() != PrimitiveTopology::TriangleList {
                    false
                } else {
                    make_traceable(&mut meshes.get_mut(&mesh.0).unwrap(), normal_mapped)
                };
                cache.meshes.insert(id, u);
                u
            }
        };
        if !raster_only && cache.deferred.insert(material.0.id()) {
            if let Some(mut m) = materials.get_mut(&material.0) {
                m.opaque_render_method = OpaqueRendererMethod::Deferred;
            }
        }
        commands.entity(e).insert(RtxSeen);
        if usable && !skinned && !raster_only {
            commands.entity(e).insert(RtxTraceable(mesh.0.clone()));
            cache.traced += 1;
        }
    }
    let t = time.elapsed_secs();
    if t - cache.logged > 10.0 {
        cache.logged = t;
        let ok = cache.meshes.values().filter(|&&u| u).count();
        info!(
            "rtx: {} traced instances, {ok}/{} meshes traceable, {} deferred materials, {} scenery + {} paint proxies",
            cache.traced,
            cache.meshes.len(),
            cache.deferred.len(),
            cache.scenery.len(),
            cache.paint.len()
        );
    }
}

/// The ray-traced scene = what is drawn: visible entities, and for LODs only the band holding the camera distance
/// (switching at the middle of the cross-fade margins). Not frustum based: off-screen geometry still casts.
#[allow(clippy::type_complexity)]
fn sync_traced(
    mut commands: Commands,
    cams: Query<&GlobalTransform, (With<RtxCamera>, With<FxPostCamera>)>,
    q: Query<(Entity, &RtxTraceable, &InheritedVisibility, &GlobalTransform, Option<&VisibilityRange>, Has<RaytracingMesh3d>)>,
    mut stats: Local<f32>,
    time: Res<Time>,
) {
    let Some(cam) = cams.iter().map(|g| g.translation()).next() else { return };
    let mut on = 0;
    for (e, t, vis, g, range, traced) in &q {
        let mut want = vis.get();
        if want {
            if let Some(r) = range {
                let d = g.translation().distance(cam);
                let from = 0.5 * (r.start_margin.start + r.start_margin.end);
                let to = 0.5 * (r.end_margin.start + r.end_margin.end);
                want = d >= from && d < to;
            }
        }
        on += want as usize;
        if want != traced {
            if want {
                commands.entity(e).insert(RaytracingMesh3d(t.0.clone()));
            } else {
                commands.entity(e).remove::<RaytracingMesh3d>();
            }
        }
    }
    if time.elapsed_secs() - *stats > 10.0 {
        *stats = time.elapsed_secs();
        info!("rtx: {on} of {} traceable instances in the ray-traced scene", q.iter().len());
    }
}

/// Strip / fill the mesh to the tracer's layout. False = can't be traced (lines, no normals...).
fn make_traceable(mesh: &mut Mesh, normal_mapped: bool) -> bool {
    if mesh.primitive_topology() != PrimitiveTopology::TriangleList || mesh.try_attributes().is_err() {
        return false;
    }
    let n = mesh.count_vertices();
    if n == 0 || !matches!(mesh.attribute(Mesh::ATTRIBUTE_POSITION), Some(VertexAttributeValues::Float32x3(_))) {
        return false;
    }
    if !matches!(mesh.attribute(Mesh::ATTRIBUTE_NORMAL), Some(VertexAttributeValues::Float32x3(_))) {
        return false;
    }
    let keep = [Mesh::ATTRIBUTE_POSITION.id, Mesh::ATTRIBUTE_NORMAL.id, Mesh::ATTRIBUTE_UV_0.id, Mesh::ATTRIBUTE_TANGENT.id];
    let drop: Vec<_> = mesh.attributes().map(|(a, _)| a.clone()).filter(|a| !keep.contains(&a.id)).collect();
    for a in drop {
        mesh.remove_attribute(a);
    }
    if !matches!(mesh.attribute(Mesh::ATTRIBUTE_UV_0), Some(VertexAttributeValues::Float32x2(_))) {
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0f32, 0.0]; n]);
    }
    match mesh.indices() {
        Some(Indices::U32(_)) => {}
        Some(Indices::U16(i)) => {
            let i: Vec<u32> = i.iter().map(|&x| x as u32).collect();
            mesh.insert_indices(Indices::U32(i));
        }
        None => mesh.insert_indices(Indices::U32((0..n as u32).collect())),
    }
    // Dummy tangents unless the material has a normal map (scenery proxies carry none): mikktspace over the whole
    // scenery would hitch the main thread.
    if !matches!(mesh.attribute(Mesh::ATTRIBUTE_TANGENT), Some(VertexAttributeValues::Float32x4(_))) && !(normal_mapped && mesh.generate_tangents().is_ok()) {
        mesh.insert_attribute(Mesh::ATTRIBUTE_TANGENT, vec![[1.0f32, 0.0, 0.0, 1.0]; n]);
    }
    mesh.enable_raytracing = true;
    true
}
