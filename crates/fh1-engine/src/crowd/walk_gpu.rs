//! GPU walkers (perf push 2026-10-08; `FH1_CROWD_GPU_WALK=0` = the CPU walkers as before). The P2 merged walkers rewrote
//! one big card mesh every frame (positions + headings of every walker: mesh allocator churn and a re-prepare on the
//! render thread, worst at the festival) and the default per-walker walkers moved ~11k entities every frame. Here each
//! path is baked once into evenly spaced points (`STEP` m, a storage buffer shared by all paths), each loaded path is one
//! static card mesh (a quad per walker: offset along the path, first point, span count, path length, atlas model) and
//! `crowd_walk.wgsl` moves the cards from a time uniform. The CPU only places the few 3D figures within `near`, from the
//! same baked points (`at`), so a figure stands where its card would.

use super::*;
use bevy::render::storage::ShaderBuffer;

/// Spacing of the baked path points (m). The Bézier spans are a few metres long; 0.5 m keeps the chords within
/// centimetres of the curve.
const STEP: f32 = 0.5;

/// `FH1_CROWD_GPU_WALK=0`: CPU walkers (per-walker entities, or the merged mesh with FH1_CROWD_MERGED=1).
pub(super) fn enabled() -> bool {
    std::env::var("FH1_CROWD_GPU_WALK").map_or(true, |v| v != "0")
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct WalkerMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,
    /// The crowd cards' parameters (copied from the `CrowdMaterial` when they change).
    #[uniform(2)]
    pub params: CrowdParams,
    /// x = time (s), y = walking speed (m/s).
    #[uniform(3)]
    pub walk: Vec4,
    #[storage(4, read_only)]
    pub points: Handle<ShaderBuffer>,
}

impl Material for WalkerMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/crowd_walk.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/crowd_walk.wgsl".into()
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

/// The baked paths: per path its first point and span count (`count + 1` points; 0 = no path).
pub(super) struct Baked {
    first: Vec<u32>,
    count: Vec<u32>,
    pts: Vec<[f32; 4]>,
}

pub(super) fn bake(paths: &[fh1_formats::crowd::WalkerPath]) -> Baked {
    let mut b = Baked { first: Vec::with_capacity(paths.len()), count: Vec::with_capacity(paths.len()), pts: Vec::new() };
    for p in paths {
        b.first.push(b.pts.len() as u32);
        if p.knots.len() < 2 || p.length <= 0.0 {
            b.count.push(0);
            continue;
        }
        let n = ((p.length / STEP).ceil() as u32).max(2);
        for i in 0..=n {
            // The last point is the path's end, not its wrap to the start (open paths jump back, as on the CPU).
            let d = (i as f32 * p.length / n as f32).min(p.length - 1e-3).max(0.0);
            let q = p.point_at(d).map_or([0.0; 3], |(q, _)| q);
            b.pts.push([q[0], q[1], q[2], 0.0]);
        }
        b.count.push(n);
    }
    // A storage buffer can't be empty.
    if b.pts.is_empty() {
        b.pts.push([0.0; 4]);
    }
    b
}

impl Baked {
    /// The shader's placement on the CPU: position and horizontal facing `s` metres along path `i`.
    fn at(&self, i: usize, length: f32, s: f32) -> Option<(Vec3, Vec3)> {
        let n = *self.count.get(i)?;
        if n == 0 {
            return None;
        }
        let len = length.max(1e-3);
        let x = s.rem_euclid(len) / len * n as f32;
        let k = (x.floor() as u32).min(n - 1);
        let f = x - k as f32;
        let j = (self.first[i] + k) as usize;
        let (p0, p1) = (Vec3::from_slice(&self.pts[j][..3]), Vec3::from_slice(&self.pts[j + 1][..3]));
        let dir = Vec3::new(p1.x - p0.x, 0.0, p1.z - p0.z).normalize_or(Vec3::NEG_Z);
        Some((p0.lerp(p1, f), dir))
    }

    fn bytes(&self) -> Vec<u8> {
        self.pts.iter().flatten().flat_map(|x| x.to_le_bytes()).collect()
    }
}

/// GPU walker state kept in `CrowdWorld`.
#[derive(Default)]
pub(super) struct GpuWalk {
    baked: Option<Baked>,
    material: Option<Handle<WalkerMaterial>>,
    /// Per path: its card entity while loaded (the walkers' figure state is `CrowdWorld::walk`).
    entities: Vec<Option<Entity>>,
}

/// The assets the GPU walkers need beyond `Assets3d`.
pub(super) struct GpuAssets<'a> {
    pub materials: &'a mut Assets<WalkerMaterial>,
    /// The crowd cards' texture and current parameters.
    pub texture: Handle<Image>,
    pub params: CrowdParams,
}

/// Same spawn rule, speed, models and figure range as `stream_walkers_merged`, without any per-frame mesh write.
pub(super) fn stream(commands: &mut Commands, w: &mut CrowdWorld, here: Vec3, now: f32, a: &mut Assets3d, g: &mut GpuAssets) {
    if let Some(c) = w.classes.get(3).and_then(|c| c.anims.first().cloned()).and_then(|n| w.clip(&n)) {
        w.walk_speed = 1.4 / c.1.duration.max(0.1);
    }
    if w.gpu.baked.is_none() {
        let t0 = std::time::Instant::now();
        let baked = bake(&w.paths);
        let buffer = a.buffers.add(ShaderBuffer::new(&baked.bytes(), RenderAssetUsages::RENDER_WORLD));
        info!("crowd: GPU walkers, {} paths baked to {} points in {:.1} ms", w.paths.len(), baked.pts.len(), t0.elapsed().as_secs_f32() * 1000.0);
        w.gpu.material = Some(g.materials.add(WalkerMaterial { texture: g.texture.clone(), params: g.params, walk: Vec4::new(now, w.walk_speed, 0.0, 0.0), points: buffer }));
        w.gpu.entities = vec![None; w.paths.len()];
        w.gpu.baked = Some(baked);
    }
    let Some(material) = w.gpu.material.clone() else { return };
    // The one per-frame GPU write: the clock (a 16-byte uniform), plus the card parameters when the light / cut moved.
    if let Some(mut m) = g.materials.get_mut(&material) {
        m.walk = Vec4::new(now, w.walk_speed, 0.0, 0.0);
        m.params = g.params;
    }
    let per_model = w.per_model;
    for i in 0..w.paths.len() {
        let p = &w.paths[i];
        let (length, class, knots) = (p.length, p.class, p.knots.len());
        let c = Vec2::new(here.x.clamp(p.bbox_min[0].min(p.bbox_max[0]), p.bbox_min[0].max(p.bbox_max[0])), here.z.clamp(p.bbox_min[2].min(p.bbox_max[2]), p.bbox_min[2].max(p.bbox_max[2])));
        let d = c.distance(Vec2::new(here.x, here.z));
        if w.walk[i].is_some() && d > RANGE + KEEP {
            for f in w.walk[i].take().into_iter().flatten().filter_map(|k| k.figure) {
                commands.entity(f).despawn();
            }
            if let Some(e) = w.gpu.entities[i].take() {
                commands.entity(e).despawn();
            }
        } else if w.walk[i].is_none() && d <= RANGE && knots >= 2 {
            let Some(baked) = w.gpu.baked.as_ref() else { return };
            let (first, count) = (baked.first[i], baked.count[i]);
            if count == 0 {
                continue;
            }
            let n = ((length * WALKERS_PER_M) as usize).max(1);
            let models = w.classes.get(class as usize).map(|c| c.models.clone()).unwrap_or_default();
            let list: Vec<WalkerState> = (0..n)
                .map(|k| {
                    let seed = (i as u32).wrapping_mul(7919).wrapping_add(k as u32).wrapping_mul(2_654_435_761);
                    let model = if models.is_empty() { 0 } else { models[(seed >> 16) as usize % models.len()] };
                    WalkerState { offset: length * k as f32 / n as f32, model, seed, figure: None }
                })
                .collect();
            // The path's static card mesh: one quad per walker.
            let half = CARD_W * 0.5;
            let corners = [([-half, FOOT], [0.0, 1.0]), ([half, FOOT], [1.0, 1.0]), ([half, FOOT + CARD_H], [1.0, 0.0]), ([-half, FOOT + CARD_H], [0.0, 0.0])];
            let (mut pos, mut uv, mut corner, mut data) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for wk in &list {
                for (cr, u) in corners {
                    pos.push([wk.offset, first as f32, count as f32]);
                    uv.push(u);
                    corner.push(cr);
                    data.push([length, (wk.model * per_model) as f32, 0.0, 1.0]);
                }
            }
            let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
            mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
            mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
            mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, corner);
            mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, data);
            mesh.insert_indices(Indices::U32((0..list.len() as u32).flat_map(|q| [0, 1, 2, 0, 2, 3].map(|k| q * 4 + k)).collect()));
            // The vertex positions aren't positions: the bounds are the path's box plus a card.
            let (lo, hi) = (Vec3::from_array(p.bbox_min).min(Vec3::from_array(p.bbox_max)), Vec3::from_array(p.bbox_min).max(Vec3::from_array(p.bbox_max)));
            let aabb = Aabb::from_min_max(lo - Vec3::new(1.5, 1.0, 1.5), hi + Vec3::new(1.5, CARD_H + 1.0, 1.5));
            let e = commands
                .spawn((Mesh3d(a.meshes.add(mesh)), MeshMaterial3d(material.clone()), aabb, Transform::IDENTITY, NotShadowCaster, crate::ui::world_load::WorldEntity))
                .id();
            w.gpu.entities[i] = Some(e);
            w.walk[i] = Some(list);
        }
    }
    // 3D figures within `near`: placed from the baked points at the shader's clock.
    let (near, speed, turn) = (w.near, w.walk_speed, Quat::from_rotation_y(w.model_yaw));
    if near <= 0.0 {
        return;
    }
    let mut walk = std::mem::take(&mut w.walk);
    for (i, list) in walk.iter_mut().enumerate() {
        let Some(list) = list else { continue };
        let p = &w.paths[i];
        // Whole path out of figure range: only drop figures (no per-walker work).
        let (lo, hi) = (Vec3::from_array(p.bbox_min).min(Vec3::from_array(p.bbox_max)), Vec3::from_array(p.bbox_min).max(Vec3::from_array(p.bbox_max)));
        let c = Vec2::new(here.x.clamp(lo.x, hi.x), here.z.clamp(lo.z, hi.z));
        let path_far = c.distance(Vec2::new(here.x, here.z)) > near + 8.0;
        let (length, class) = (p.length, p.class as u8);
        for wk in list.iter_mut() {
            if path_far {
                if let Some(f) = wk.figure.take() {
                    commands.entity(f).despawn();
                }
                continue;
            }
            let Some((at, dir)) = w.gpu.baked.as_ref().and_then(|b| b.at(i, length, wk.offset + speed * now)) else { continue };
            let dist = Vec2::new(at.x - here.x, at.z - here.z).length();
            let t = Transform::from_translation(at).with_rotation(Quat::from_rotation_y((-dir.x).atan2(-dir.z)) * turn);
            match wk.figure {
                Some(f) if dist > near + 8.0 => {
                    commands.entity(f).despawn();
                    wk.figure = None;
                }
                Some(f) => {
                    commands.entity(f).insert(t);
                }
                None if dist <= near + 3.0 => {
                    if let Some(f) = w.spawn_figure(commands, a, wk.seed, class, wk.model, 0, t) {
                        wk.figure = Some(f.root);
                    }
                }
                _ => {}
            }
        }
    }
    w.walk = walk;
}

/// Despawn every GPU walker entity and figure (A/B mode switch).
pub(super) fn clear(commands: &mut Commands, w: &mut CrowdWorld) {
    for e in w.gpu.entities.iter_mut().filter_map(Option::take) {
        commands.entity(e).despawn();
    }
}
