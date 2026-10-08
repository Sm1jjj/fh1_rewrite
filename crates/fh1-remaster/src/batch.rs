//! W4 static-geometry merging (docs/REMASTER.md "Batching"): many placements of a few template meshes -> one
//! mesh per (key, layout), with the placement transform baked into the vertices. The per-placement data that the
//! merge would otherwise lose rides in two extra vertex attributes:
//!
//! - [`ATTRIBUTE_INSTANCE_LOD`]: placement centre (world xyz) + its LOD draw range `[start, end)` packed as two f16
//!   in `w`. `batch_lod.wgsl` collapses the vertex outside the range and dithers the fade margins, so a merged tile
//!   keeps per-placement LOD switching and cross-fades (the faithful path does this with one entity + `VisibilityRange`
//!   per placement).
//! - [`ATTRIBUTE_INSTANCE_TINT`]: the placement's tint (`fh1_render::material::object_consts` c0 order: RGBA).
//!
//! Pure CPU work, meant for the async pool: templates come in as `Arc<Mesh>` kept from load time (meshes added to
//! `Assets<Mesh>` with `RENDER_WORLD` usage lose their data after upload).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::camera::primitives::Aabb;
use bevy::mesh::{Indices, MeshVertexAttribute, MeshVertexAttributeId, PrimitiveTopology, VertexAttributeValues, VertexFormat};
use bevy::prelude::*;

/// Placement centre + packed LOD range (see the module doc).
pub const ATTRIBUTE_INSTANCE_LOD: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_InstanceLod", 0x4648_0040, VertexFormat::Float32x4);
/// Placement tint (RGBA).
pub const ATTRIBUTE_INSTANCE_TINT: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_InstanceTint", 0x4648_0041, VertexFormat::Unorm8x4);

/// A merged mesh is closed once it holds this many vertices; the next placement starts a new chunk (keeps each
/// buffer small enough to upload in one frame and the culling AABB meaningful).
pub const MAX_VERTS: usize = 1 << 20;

/// `(start, end)` with `end = f32::INFINITY` for "never culled".
#[derive(Clone, Copy, Debug)]
pub struct Instance {
    pub transform: Mat4,
    pub tint: [u8; 4],
    pub lod: (f32, f32),
}

/// `f32` pair -> `pack2x16float` bits (WGSL unpack2x16float order: low half = x). Infinity stays infinity.
pub fn pack_lod(start: f32, end: f32) -> f32 {
    let h = |v: f32| half_bits(v) as u32;
    f32::from_bits(h(start) | (h(end) << 16))
}

fn half_bits(v: f32) -> u16 {
    // IEEE f32 -> f16, round toward zero; LOD distances are 0..~5000 m so no subnormal handling is needed.
    if v.is_infinite() || v > 65504.0 {
        return 0x7C00;
    }
    if v <= 0.0 {
        return 0;
    }
    let b = v.to_bits();
    let exp = ((b >> 23) & 0xFF) as i32 - 127 + 15;
    if exp <= 0 {
        return 0;
    }
    ((exp as u16) << 10) | ((b >> 13) & 0x3FF) as u16
}

/// How an attribute changes under the placement transform.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Point,
    /// Direction transformed by the normal matrix (normals).
    Normal,
    /// Direction transformed by the model matrix (tangents, binormals); `w` (if any) flips with mirroring.
    Direction,
    Copy,
}

fn kind(id: MeshVertexAttributeId) -> Kind {
    if id == Mesh::ATTRIBUTE_POSITION.id {
        Kind::Point
    } else if id == Mesh::ATTRIBUTE_NORMAL.id {
        Kind::Normal
    } else if id == Mesh::ATTRIBUTE_TANGENT.id || id == fh1_tangent_id() || id == fh1_binormal_id() {
        Kind::Direction
    } else {
        Kind::Copy
    }
}

// fh1-render's scenery tangent frame (Float32x3), named here by id so this crate doesn't depend on fh1-render.
fn fh1_tangent_id() -> MeshVertexAttributeId {
    MeshVertexAttribute::new("Fx_Tangent", 0x4648_0002, VertexFormat::Float32x3).id
}
fn fh1_binormal_id() -> MeshVertexAttributeId {
    MeshVertexAttribute::new("Fx_Binormal", 0x4648_0005, VertexFormat::Float32x3).id
}

/// One growing merged mesh.
struct Accum {
    /// World position the vertices are relative to (the first placement's origin; P8): the entity's translation, so
    /// Bevy's VisibilityRange (and its dither in the prepass / shadow shaders) measure from inside the chunk.
    origin: Vec3,
    layout: Vec<MeshVertexAttribute>,
    values: Vec<VertexAttributeValues>,
    indices: Vec<u32>,
    lod: Vec<[f32; 4]>,
    tint: Vec<[u8; 4]>,
    min: Vec3,
    max: Vec3,
    verts: usize,
    instances: u32,
    /// Sums of the placements' LOD starts / ends (finite ends only) for the entity-level shadow range.
    start_sum: f32,
    end_sum: f32,
    finite_ends: u32,
    /// Largest placement half-diagonal (m) in this chunk (small-caster shadow cull, [`ShadowLod::max_radius`]).
    max_radius: f32,
    /// Smallest placement LOD start / largest end (infinite if any) in this chunk: the entity's coarse range (P8).
    min_start: f32,
    max_end: f32,
}

impl Accum {
    fn new(template: &Mesh, origin: Vec3) -> Self {
        let layout: Vec<MeshVertexAttribute> = template.attributes().map(|(a, _)| a.clone()).collect();
        let values = template.attributes().map(|(_, v)| empty_like(v)).collect();
        Self { origin, layout, values, indices: Vec::new(), lod: Vec::new(), tint: Vec::new(), min: Vec3::MAX, max: Vec3::MIN, verts: 0, instances: 0, start_sum: 0.0, end_sum: 0.0, finite_ends: 0, max_radius: 0.0, min_start: f32::MAX, max_end: 0.0 }
    }

    fn push(&mut self, template: &Mesh, inst: &Instance) {
        let n = template.count_vertices();
        let base = self.verts as u32;
        let m = inst.transform;
        let mirrored = m.determinant() < 0.0;
        let m3 = Mat3::from_mat4(m);
        let nm = m3.inverse().transpose();
        for ((attr, out), (_, src)) in self.layout.iter().zip(&mut self.values).zip(template.attributes()) {
            let k = kind(attr.id);
            match (k, src, out) {
                (Kind::Point, VertexAttributeValues::Float32x3(s), VertexAttributeValues::Float32x3(o)) => {
                    let (mut pmin, mut pmax) = (Vec3::MAX, Vec3::MIN);
                    for p in s {
                        let w = m.transform_point3(Vec3::from_array(*p)) - self.origin;
                        pmin = pmin.min(w);
                        pmax = pmax.max(w);
                        o.push(w.to_array());
                    }
                    if !s.is_empty() {
                        self.min = self.min.min(pmin);
                        self.max = self.max.max(pmax);
                        self.max_radius = self.max_radius.max(0.5 * (pmax - pmin).length());
                    }
                }
                (Kind::Normal, VertexAttributeValues::Float32x3(s), VertexAttributeValues::Float32x3(o)) => {
                    o.extend(s.iter().map(|v| (nm * Vec3::from_array(*v)).normalize_or(Vec3::Y).to_array()));
                }
                (Kind::Direction, VertexAttributeValues::Float32x3(s), VertexAttributeValues::Float32x3(o)) => {
                    o.extend(s.iter().map(|v| (m3 * Vec3::from_array(*v)).normalize_or(Vec3::X).to_array()));
                }
                (Kind::Direction, VertexAttributeValues::Float32x4(s), VertexAttributeValues::Float32x4(o)) => {
                    let flip = if mirrored { -1.0 } else { 1.0 };
                    o.extend(s.iter().map(|v| (m3 * Vec3::new(v[0], v[1], v[2])).normalize_or(Vec3::X).extend(v[3] * flip).to_array()));
                }
                (_, s, o) => extend(o, s),
            }
        }
        let center = m.transform_point3(Vec3::ZERO);
        let lod = [center.x, center.y, center.z, pack_lod(inst.lod.0, inst.lod.1)];
        self.lod.extend(std::iter::repeat_n(lod, n));
        self.tint.extend(std::iter::repeat_n(inst.tint, n));
        let idx: Vec<u32> = match template.indices() {
            Some(Indices::U16(i)) => i.iter().map(|&x| x as u32).collect(),
            Some(Indices::U32(i)) => i.clone(),
            None => (0..n as u32).collect(),
        };
        if mirrored {
            // A mirrored transform turns the winding inside out: swap two corners of each triangle.
            for t in idx.chunks_exact(3) {
                self.indices.extend([base + t[0], base + t[2], base + t[1]]);
            }
        } else {
            self.indices.extend(idx.iter().map(|&i| base + i));
        }
        self.verts += n;
        self.instances += 1;
        self.start_sum += inst.lod.0;
        self.min_start = self.min_start.min(inst.lod.0);
        self.max_end = self.max_end.max(inst.lod.1);
        if inst.lod.1.is_finite() {
            self.end_sum += inst.lod.1;
            self.finite_ends += 1;
        }
    }

    fn finish(self) -> Merged {
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
        for (a, v) in self.layout.into_iter().zip(self.values) {
            mesh.insert_attribute(a, v);
        }
        mesh.insert_attribute(ATTRIBUTE_INSTANCE_LOD, self.lod);
        mesh.insert_attribute(ATTRIBUTE_INSTANCE_TINT, VertexAttributeValues::Unorm8x4(self.tint));
        let indices = if self.verts <= u16::MAX as usize + 1 {
            Indices::U16(self.indices.into_iter().map(|i| i as u16).collect())
        } else {
            Indices::U32(self.indices)
        };
        mesh.insert_indices(indices);
        let start = self.start_sum / self.instances.max(1) as f32;
        // Most placements culled -> the mean finite end; else never culled.
        let end = if self.finite_ends * 2 > self.instances { self.end_sum / self.finite_ends as f32 } else { f32::INFINITY };
        let range = (self.min_start.min(self.max_end), self.max_end);
        let origin = self.origin;
        Merged { mesh, aabb: Aabb::from_min_max(self.min, self.max), origin, instances: self.instances, range, casts: true, shadow: ShadowLod { centre: origin + (self.min + self.max) * 0.5, start, end, max_radius: self.max_radius } }
    }
}

/// A finished merged mesh: vertices relative to `origin` (spawn it with `Transform::from_translation(origin)`) and its
/// bounds (relative to `origin`).
pub struct Merged {
    pub mesh: Mesh,
    pub aabb: Aabb,
    pub origin: Vec3,
    pub instances: u32,
    /// Smallest LOD start / largest LOD end (m, `INFINITY` = never culled) of its placements (P8: the cell's coarse
    /// VisibilityRange, widened by the bounds' half diagonal by the caller).
    pub range: (f32, f32),
    /// Insert on the spawned entity: picks whether it casts shadows (see [`ShadowLod`]).
    pub shadow: ShadowLod,
    /// False: the chunk never casts sun shadows (P8 cell shadows: a non-final LOD level of its placements, whose last
    /// level casts instead). Set by [`merge_props`].
    pub casts: bool,
}

/// Collects placements per key `K` (e.g. (LOD level, material)) and merges them. Templates with a different
/// attribute layout under the same key go to separate meshes.
pub struct Batcher<K: Hash + Eq + Clone> {
    open: HashMap<(K, u64), Accum>,
    done: Vec<(K, Merged)>,
}

impl<K: Hash + Eq + Clone> Default for Batcher<K> {
    fn default() -> Self {
        Self { open: HashMap::new(), done: Vec::new() }
    }
}

impl<K: Hash + Eq + Clone> Batcher<K> {
    pub fn add(&mut self, key: K, template: &Arc<Mesh>, inst: Instance) {
        if template.primitive_topology() != PrimitiveTopology::TriangleList || template.count_vertices() == 0 {
            return;
        }
        let k = (key, layout_hash(template));
        if let Some(a) = self.open.get(&k) {
            if a.verts + template.count_vertices() > MAX_VERTS {
                let a = self.open.remove(&k).unwrap();
                self.done.push((k.0.clone(), a.finish()));
            }
        }
        self.open.entry(k.clone()).or_insert_with(|| Accum::new(template, inst.transform.w_axis.truncate())).push(template, &inst);
    }

    pub fn finish(mut self) -> Vec<(K, Merged)> {
        self.done.extend(self.open.into_iter().map(|((k, _), a)| (k, a.finish())));
        self.done
    }
}

fn layout_hash(m: &Mesh) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (a, _) in m.attributes() {
        a.id.hash(&mut h);
        (a.format as u32).hash(&mut h);
    }
    h.finish()
}

macro_rules! per_variant {
    ($($v:ident)*) => {
        fn empty_like(v: &VertexAttributeValues) -> VertexAttributeValues {
            use VertexAttributeValues as V;
            match v {
                $(V::$v(_) => V::$v(Vec::new()),)*
            }
        }

        fn extend(out: &mut VertexAttributeValues, src: &VertexAttributeValues) {
            use VertexAttributeValues as V;
            match (out, src) {
                $((V::$v(o), V::$v(s)) => o.extend_from_slice(s),)*
                // Same layout hash = same formats; a mismatch can't happen.
                _ => {}
            }
        }
    };
}

per_variant!(Uint8 Uint8x2 Uint8x4 Sint8 Sint8x2 Sint8x4 Unorm8 Unorm8x2 Unorm8x4 Snorm8 Snorm8x2 Snorm8x4 Uint16 Uint16x2 Uint16x4 Sint16 Sint16x2 Sint16x4 Unorm16 Unorm16x2 Unorm16x4 Snorm16 Snorm16x2 Snorm16x4 Float16 Float16x2 Float16x4 Float32 Float32x2 Float32x3 Float32x4 Uint32 Uint32x2 Uint32x3 Uint32x4 Sint32 Sint32x2 Sint32x3 Sint32x4 Float64 Float64x2 Float64x3 Float64x4 Unorm8x4Bgra Unorm10_10_10_2);

#[cfg(test)]
mod tests {
    use super::*;

    fn tri() -> Arc<Mesh> {
        let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
        m.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        m.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 0.0, 1.0]; 3]);
        m.insert_indices(Indices::U16(vec![0, 1, 2]));
        Arc::new(m)
    }

    #[test]
    fn merges_and_mirrors() {
        let mut b = Batcher::default();
        let t = tri();
        b.add(0u8, &t, Instance { transform: Mat4::from_translation(Vec3::X * 10.0), tint: [255; 4], lod: (0.0, 100.0) });
        b.add(0u8, &t, Instance { transform: Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)), tint: [0; 4], lod: (0.0, f32::INFINITY) });
        let out = b.finish();
        assert_eq!(out.len(), 1);
        let m = &out[0].1;
        assert_eq!(m.instances, 2);
        assert_eq!(m.mesh.count_vertices(), 6);
        let Some(Indices::U16(i)) = m.mesh.indices() else { panic!() };
        assert_eq!(i, &[0, 1, 2, 3, 5, 4]);
        // Mirrored normal: (0,0,1) under diag(-1,1,1) stays (0,0,1).
        let Some(VertexAttributeValues::Float32x3(n)) = m.mesh.attribute(Mesh::ATTRIBUTE_NORMAL) else { panic!() };
        assert_eq!(n[3], [0.0, 0.0, 1.0]);
        // Vertices relative to the first placement's origin.
        assert_eq!(m.origin, Vec3::X * 10.0);
        assert_eq!(m.aabb.min().x + m.origin.x, -1.0);
        assert_eq!(m.aabb.max().x + m.origin.x, 11.0);
    }

    #[test]
    fn lod_packing() {
        let bits = pack_lod(70.0, f32::INFINITY).to_bits();
        assert_eq!(bits >> 16, 0x7C00);
        assert_eq!(bits & 0xFFFF, 0x5460); // 70.0 in f16
    }
}

/// Registers `batch_lod.wgsl` as the shader import `fh1_remaster::batch_lod` (call from the crate plugin).
pub fn plugin(app: &mut App) {
    bevy::shader::load_shader_library!(app, "batch_lod.wgsl");
    app.init_resource::<SmallCasters>().add_systems(PostUpdate, (shadow_lod, (collect_small_casters, small_casters).chain()));
    if let Some(render_app) = app.get_sub_app_mut(bevy::render::RenderApp) {
        render_app.add_systems(bevy::render::Render, phase_stats.in_set(bevy::render::RenderSystems::PhaseSort).after(bevy::render::render_phase::sort_phase_system::<bevy::core_pipeline::core_3d::Transparent3d>));
    }
}

/// Transparent-pass census (2026-10-08 perf: main transparent pass 1.19 ms in the user's log, contents unknown): every
/// ~5 s with `FH1_RM_PHASE_STATS=1` (no longer with `FH1_PERF_REC=1`: in user log 20261008_125629 one run froze the
/// render thread for 34.6 s inside PhaseSort), logs per view the `Transparent3d` items grouped by their
/// pipeline's fragment shader (embedded path for custom materials, handle id otherwise; the remaster scenery shader is
/// named). Transparent items are drawn one by one after a per-frame sort, so this is where draws hide.
fn phase_stats(
    phases: Res<bevy::render::render_phase::ViewSortedRenderPhases<bevy::core_pipeline::core_3d::Transparent3d>>,
    cache: Res<bevy::render::render_resource::PipelineCache>,
    mut frame: Local<u32>,
) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("FH1_RM_PHASE_STATS").is_ok_and(|v| v == "1")) {
        return;
    }
    *frame = frame.wrapping_add(1);
    if *frame % 300 != 0 {
        return;
    }
    for (view, phase) in phases.0.iter() {
        let mut groups: HashMap<String, u32> = HashMap::new();
        for item in phase.items.values() {
            let name = match cache.get_render_pipeline_state(item.pipeline) {
                bevy::render::render_resource::CachedPipelineState::Ok(_) => {
                    let d = cache.get_render_pipeline_descriptor(item.pipeline);
                    match &d.fragment {
                        Some(f) if f.shader.id() == crate::material::SHADER.id() => "remaster scenery".to_string(),
                        Some(f) => f.shader.path().map_or_else(|| format!("{:?}", f.shader.id()), |p| p.to_string()),
                        None => "no fragment".to_string(),
                    }
                }
                _ => "pending".to_string(),
            };
            *groups.entry(name).or_default() += 1;
        }
        let mut g: Vec<(String, u32)> = groups.into_iter().collect();
        g.sort_by(|a, b| b.1.cmp(&a.1));
        g.truncate(10);
        info!("rm phase stats: view {view:?} transparent {} items: {g:?}", phase.items.len());
    }
}

/// Marks a scenery entity whose shadow [`small_casters`] switched off (only those are switched back on: other systems
/// set `NotShadowCaster` for their own reasons).
#[derive(Component)]
struct SmallCulled;

/// Small static scenery pieces (world centre), collected once as they spawn; [`small_casters`] walks a slice per frame.
#[derive(Resource, Default)]
struct SmallCasters {
    centre: HashMap<Entity, Vec3>,
    order: Vec<Entity>,
    cursor: usize,
}

/// Collects small casters: remaster scenery entities whose bounds / placement just became known (static scenery gets
/// its `Aabb` and final `GlobalTransform` once after spawning). Also sorts entities out of the car probe's cube: small
/// pieces always (main-only layer); with the lean faces (car_probe.rs `lean_on`) also every transparent / additive
/// material (decals, water, glass) and anything under `probe_min_radius` (merged prop chunks: their largest placement).
#[allow(clippy::type_complexity)]
fn collect_small_casters(
    mut commands: Commands,
    mut cache: ResMut<SmallCasters>,
    new: Query<
        (Entity, &Aabb, &GlobalTransform, Has<bevy::camera::visibility::RenderLayers>, &MeshMaterial3d<crate::material::RemasterMaterial>, Option<&ShadowLod>),
        Or<(Added<Aabb>, Changed<GlobalTransform>)>,
    >,
    materials: Res<Assets<crate::material::RemasterMaterial>>,
) {
    if !crate::enabled() {
        return;
    }
    let cull = !std::env::var("FH1_SHADOW_SMALL_CULL").is_ok_and(|v| v == "0");
    // Small pieces stay out of the car probe's cube (car_probe.rs `main_only_layers`), set once here.
    let main_only = crate::car_probe::main_only_layers();
    let lean = crate::car_probe::probe_skip_layers();
    if !cull && main_only.is_none() {
        return;
    }
    for (e, aabb, gt, has_layers, m, lod) in &new {
        let (scale, _, _) = gt.to_scale_rotation_translation();
        let radius = (Vec3::from(aabb.half_extents) * scale.abs()).length();
        if !has_layers {
            if let Some(l) = &lean {
                let transparent = materials.get(&m.0).is_some_and(|m| !matches!(m.base.alpha_mode, AlphaMode::Opaque | AlphaMode::Mask(_)));
                if transparent || lod.map_or(radius, |l| l.max_radius) < crate::car_probe::probe_min_radius() {
                    commands.entity(e).try_insert(l.clone());
                }
            } else if let (Some(l), true) = (&main_only, lod.is_none() && radius < SMALL_RADIUS) {
                commands.entity(e).try_insert(l.clone());
            }
        }
        if lod.is_some() || radius >= SMALL_RADIUS || !cull {
            continue;
        }
        let centre = gt.transform_point(Vec3::from(aabb.center));
        if cache.centre.insert(e, centre).is_none() {
            cache.order.push(e);
        }
    }
}

/// Small-caster shadow cull for the default (unmerged) props: one entity per placement part with the remaster scenery
/// material. Same rule as [`ShadowLod`]'s: world half-diagonal under [`SMALL_RADIUS`] and farther than
/// `FH1_SHADOW_SMALL_DIST` (80 m, [`SMALL_HYSTERESIS`] band) from the main camera = no shadow. Each frame looks at 1/8
/// of the cached candidates (2026-10-08: walking every scenery entity to pick a slice cost 0.25 ms/frame in the user's
/// log); the 8 m band covers what a car travels in 8 frames. Despawned entities drop out of the cache when met. Cars
/// (StandardMaterial / paint), crowds and grass are untouched. `FH1_SHADOW_SMALL_CULL=0` = off.
#[allow(clippy::type_complexity)]
fn small_casters(
    mut commands: Commands,
    mut cache: ResMut<SmallCasters>,
    cams: Query<(&Camera, &GlobalTransform), (With<Camera3d>, Without<bevy::ui::IsDefaultUiCamera>)>,
    state: Query<(Has<bevy::light::NotShadowCaster>, Has<SmallCulled>)>,
) {
    if !crate::enabled() || std::env::var("FH1_SHADOW_SMALL_CULL").is_ok_and(|v| v == "0") || cache.order.is_empty() {
        return;
    }
    let dist: f32 = std::env::var("FH1_SHADOW_SMALL_DIST").ok().and_then(|v| v.parse().ok()).unwrap_or(80.0);
    let Some(eye) = cams.iter().filter(|(c, _)| c.is_active && c.order >= 0).min_by_key(|(c, _)| c.order).map(|(_, t)| t.translation()) else { return };
    let cache = &mut *cache;
    let n = cache.order.len().div_ceil(8);
    let mut done = 0;
    while done < n && !cache.order.is_empty() {
        if cache.cursor >= cache.order.len() {
            cache.cursor = 0;
        }
        let e = cache.order[cache.cursor];
        done += 1;
        let Ok((no_cast, culled)) = state.get(e) else {
            // Despawned: drop it (swap_remove brings the last one to this slot; it's looked at next).
            cache.order.swap_remove(cache.cursor);
            cache.centre.remove(&e);
            continue;
        };
        cache.cursor += 1;
        // Not ours to touch: made non-casting elsewhere.
        if no_cast && !culled {
            continue;
        }
        let Some(&centre) = cache.centre.get(&e) else { continue };
        let d = eye.distance(centre);
        let limit = if culled { dist - SMALL_HYSTERESIS } else { dist };
        let want_cull = d >= limit;
        if want_cull && !culled {
            commands.entity(e).try_insert((bevy::light::NotShadowCaster, SmallCulled));
        } else if !want_cull && culled {
            commands.entity(e).try_remove::<(bevy::light::NotShadowCaster, SmallCulled)>();
        }
    }
}

/// Shadow LOD of a merged mesh. The main pass picks each placement's LOD per vertex (batch_lod.wgsl), but shadow
/// views have no player-camera position, so Bevy's default shadow pass would draw every LOD level at once. Instead
/// each merged (tile, LOD level) entity casts only while the main camera's distance to its bounds centre lies in
/// the placements' mean LOD range: one level per tile casts, switching per tile (fine at shadow resolution).
/// `FH1_BATCH_SHADOW_LOD=0`: every level casts (A/B).
///
/// Small casters (2026-10-08, perf: shadow pass ~1.9 ms at the festival): a chunk whose largest placement is under
/// [`SMALL_RADIUS`] (cones, signs, bollards, barriers, small rocks; trees and buildings stay) stops casting beyond
/// `FH1_SHADOW_SMALL_DIST` m (80) from the main camera and starts again inside it minus [`SMALL_HYSTERESIS`]: at 80 m
/// such a shadow is a few texels of the far cascade, and they are most of its draws. `FH1_SHADOW_SMALL_CULL=0` = off.
#[derive(Component, Clone, Copy, Debug)]
pub struct ShadowLod {
    pub centre: Vec3,
    pub start: f32,
    pub end: f32,
    /// Largest placement half-diagonal in the chunk (m).
    pub max_radius: f32,
}

/// Placement half-diagonal (m) below which a chunk counts as small casters.
pub const SMALL_RADIUS: f32 = 1.5;
/// Band (m) below the cull distance where a culled small chunk starts casting again.
pub const SMALL_HYSTERESIS: f32 = 8.0;

#[allow(clippy::type_complexity)]
fn shadow_lod(
    mut commands: Commands,
    mut frame: Local<u32>,
    cams: Query<(&Camera, &GlobalTransform), (With<Camera3d>, Without<bevy::ui::IsDefaultUiCamera>)>,
    lods: Query<(Entity, &ShadowLod, Has<bevy::light::NotShadowCaster>)>,
) {
    *frame = frame.wrapping_add(1);
    // Every 4th frame: a tile crosses a band edge rarely, and the toggle re-specializes its shadow bins.
    if *frame % 4 != 0 || lods.is_empty() {
        return;
    }
    let all = std::env::var("FH1_BATCH_SHADOW_LOD").as_deref() == Ok("0");
    let small_cull = std::env::var("FH1_SHADOW_SMALL_CULL").map_or(true, |v| v != "0");
    let small_dist: f32 = std::env::var("FH1_SHADOW_SMALL_DIST").ok().and_then(|v| v.parse().ok()).unwrap_or(80.0);
    // The main camera: the active window camera with the lowest order >= 0 (the env cube renders at -20).
    let Some(eye) = cams.iter().filter(|(c, _)| c.is_active && c.order >= 0).min_by_key(|(c, _)| c.order).map(|(_, t)| t.translation()) else { return };
    for (e, l, off) in &lods {
        let d = eye.distance(l.centre);
        let mut cast = all || (d >= l.start && d < l.end);
        if cast && small_cull && l.max_radius < SMALL_RADIUS {
            // Hysteresis: a casting chunk stops beyond the distance, a culled one resumes inside it minus the band.
            let limit = if off { small_dist - SMALL_HYSTERESIS } else { small_dist };
            cast = d < limit;
        }
        if cast == off {
            if cast {
                commands.entity(e).remove::<bevy::light::NotShadowCaster>();
            } else {
                commands.entity(e).insert(bevy::light::NotShadowCaster);
            }
        }
    }
}

/// Shader def a material pushes in `specialize` when the mesh layout has [`ATTRIBUTE_INSTANCE_LOD`].
pub const SHADER_DEF: &str = "INSTANCE_LOD";

/// One template part (a template's per-material batch) as CPU data.
#[derive(Clone)]
pub struct TemplatePart {
    pub mesh: Arc<Mesh>,
    /// Game material id (scenery materials.json index); the seam maps it to the remaster material.
    pub material: u32,
}

/// One prop placement as the scenery files store it (engine scenery.rs `read_placements`).
#[derive(Clone, Copy)]
pub struct Placement {
    pub model: u16,
    pub transform: Mat4,
    /// Packed ARGB (`fh1_render::material::object_consts`).
    pub tint: u32,
}

/// LOD chain of a model: (LOD model, draw from, draw to), as engine scenery.rs `Props::lods` (scaled distances).
pub type LodChain = Vec<(u16, f32, f32)>;

/// P8 lever 3 (2026-10-08): merged prop meshes are cut per `CELL` m square (x, z of the placement origin), so each
/// keeps tight bounds for frustum culling and a coarse per-cell VisibilityRange. `FH1_BATCH_CELL=<m>` overrides;
/// 0 = one mesh per tile (W4, old).
pub const CELL: f32 = 64.0;

pub fn cell_size() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_BATCH_CELL").ok().and_then(|v| v.parse().ok()).filter(|v: &f32| *v >= 0.0).unwrap_or(CELL))
}

/// Merge key: (cell x, cell z, LOD level, final level of its placements, game material).
pub type MergeKey = (i32, i32, u8, bool, u32);

/// P8 cell shadows (2026-10-08): only each placement's LAST (coarsest) LOD level casts sun shadows, over the whole
/// [0, end) range; the finer levels' chunks never cast. Chunks are split by "final level or not" for that. Fewer,
/// cheaper shadow draws (Bevy's shadow pass has no per-vertex LOD: every casting chunk draws all its placements).
/// `FH1_BATCH_SHADOW_FINAL=0` = every level casts within its mean LOD range (ShadowLod, as before).
pub fn shadow_final_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_BATCH_SHADOW_FINAL").map_or(true, |v| v != "0"))
}

/// Merge a prop tile: every placement's every LOD level into one mesh per (cell, LOD level, game material), each vertex
/// carrying its placement's LOD range. `skip(i)` drops placement `i` (broken / smashable / not mergeable). Placements
/// of models without a chain draw their own model from 0 to `default_end`; LOD levels ending at or before `min_end`
/// are left out (the far ring: those never show there). `last_end[i]` (if longer than the chain's) is placement `i`'s
/// pushed-out cull of its last LOD (engine `prop_far_end`); 0 = none.
pub fn merge_props(
    list: &[Placement],
    chains: &HashMap<u16, LodChain>,
    templates: &HashMap<u16, Vec<TemplatePart>>,
    default_end: f32,
    min_end: f32,
    last_end: &[f32],
    skip: impl Fn(usize) -> bool,
) -> Vec<(MergeKey, Merged)> {
    let cell = cell_size();
    let mut b: Batcher<MergeKey> = Batcher::default();
    for (i, p) in list.iter().enumerate() {
        if skip(i) {
            continue;
        }
        let own = [(p.model, 0.0, default_end)];
        let chain: &[(u16, f32, f32)] = chains.get(&p.model).map_or(&own, |c| c.as_slice());
        let c = |s: u32| ((p.tint >> s) & 0xFF) as u8;
        let tint = [c(16), c(8), c(0), c(24)];
        let o = p.transform.w_axis;
        let (cx, cz) = if cell > 0.0 { ((o.x / cell).floor() as i32, (o.z / cell).floor() as i32) } else { (0, 0) };
        let levels = chain.len();
        for (level, &(lod, from, to)) in chain.iter().enumerate() {
            let to = if level + 1 == levels { to.max(last_end.get(i).copied().unwrap_or(0.0)) } else { to };
            if to <= min_end {
                continue;
            }
            let Some(parts) = templates.get(&lod) else { continue };
            for part in parts {
                let last = shadow_final_on() && level + 1 == levels;
                b.add((cx, cz, level as u8, last, part.material), &part.mesh, Instance { transform: p.transform, tint, lod: (from, to) });
            }
        }
    }
    let mut out = b.finish();
    if shadow_final_on() {
        for (k, m) in out.iter_mut() {
            if k.3 {
                // The last level stands in for the finer ones in the shadow maps: cast from 0.
                m.shadow.start = 0.0;
            } else {
                m.casts = false;
            }
        }
    }
    out
}

/// Engine seam state for merged props (scenery.rs `Props`, remaster mode). W4 was opt-in: tile-sized chunks defeated
/// frustum / range culling (more GPU draws than one entity per placement part; f5's FH1_RM_STATS counts, 2026-10-06).
/// P8 lever 3 (2026-10-08): `CELL` m chunks with a coarse per-cell VisibilityRange; DEFAULT-ON, `FH1_BATCH=0` = off.
#[derive(Default)]
pub struct PropMerge {
    /// Mergeable templates: every part has a remaster material (CPU meshes after `prepare_mesh`).
    templates: HashMap<u16, Vec<TemplatePart>>,
    /// Built once every template is in.
    shared: Option<Arc<HashMap<u16, Vec<TemplatePart>>>>,
    /// Templates that can't merge (a faithful-path part).
    unmergeable: std::collections::HashSet<u16>,
    /// LOD0 templates of smashables (smash.rs); `None` until the collision table is loaded.
    smashable: Option<std::collections::HashSet<u16>>,
}

pub type MergeTask = bevy::tasks::Task<Vec<(MergeKey, Merged)>>;

impl PropMerge {
    pub fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // The static world (P12) draws every placement itself: no merged chunks there.
        crate::enabled() && !crate::static_world::on() && *ON.get_or_init(|| std::env::var("FH1_BATCH").as_deref() != Ok("0"))
    }

    /// A loaded template: its CPU parts when all of them have remaster materials, `None` = not mergeable.
    pub fn add_template(&mut self, n: u16, parts: Option<Vec<TemplatePart>>) {
        match parts {
            Some(p) => {
                self.templates.insert(n, p);
            }
            None => {
                self.unmergeable.insert(n);
            }
        }
        self.shared = None;
    }

    pub fn set_smashable(&mut self, templates: std::collections::HashSet<u16>) {
        self.smashable = Some(templates);
    }

    /// Ready to merge tiles: the smashables are known.
    pub fn ready(&self) -> bool {
        self.smashable.is_some()
    }

    /// Whether placements of `model` (with its LOD chain) go into the merged meshes; the others stay entities.
    pub fn merges(&self, model: u16, chain: Option<&LodChain>) -> bool {
        if self.smashable.as_ref().is_none_or(|s| s.contains(&model)) {
            return false;
        }
        let ok = |n: u16| self.templates.contains_key(&n) && !self.unmergeable.contains(&n);
        match chain {
            Some(c) => c.iter().all(|l| ok(l.0)),
            None => ok(model),
        }
    }

    /// Start merging a tile on the async pool. `skip(i)`: placements that stay entities (broken, own lightmaps);
    /// non-merging models are skipped here. `last_end`: see [`merge_props`].
    #[allow(clippy::too_many_arguments)]
    pub fn start(&mut self, list: Vec<Placement>, chains: &HashMap<u16, LodChain>, default_end: f32, min_end: f32, skip: std::collections::HashSet<u32>, last_end: Vec<f32>) -> MergeTask {
        let templates = self.shared.get_or_insert_with(|| Arc::new(self.templates.clone())).clone();
        let keep: Vec<bool> = list.iter().enumerate().map(|(i, p)| !skip.contains(&(i as u32)) && self.merges(p.model, chains.get(&p.model))).collect();
        let chains: HashMap<u16, LodChain> = list.iter().filter_map(|p| Some((p.model, chains.get(&p.model)?.clone()))).collect();
        bevy::tasks::AsyncComputeTaskPool::get().spawn(async move { merge_props(&list, &chains, &templates, default_end, min_end, &last_end, |i| !keep[i]) })
    }
}
