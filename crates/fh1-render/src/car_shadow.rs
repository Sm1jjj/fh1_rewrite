//! Car shadows (docs/SHADOWS.md "Car").
//!
//! The game-shaded car body ([`FxCarMaterial`]) has no shadow pass, so it never cast into the CSM while the
//! glTF wheels did. Each opaque body part gets a shadow-only child on [`CASTER_LAYER`] (only the sun sees it),
//! drawn with the shared [`FxCasterMaterial`]. Car meshes carry packed positions (`Fx_CarPosition`, Snorm16x4,
//! unpacked by the car VS as `xyz·w·S + T` with `pack_partPosition`), which Bevy's depth-only shadow pipeline
//! can't read, so the proxy gets its own mesh with plain unpacked positions.
//!
//! The game's own contact shadow under the car (`CDropShader`) is [`drop_shadow`]; this plugin adds it.

pub mod drop_shadow;

use bevy::asset::RenderAssetUsages;
use bevy::camera::primitives::Aabb;
use bevy::camera::visibility::RenderLayers;
use bevy::light::NotShadowCaster;
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::prelude::*;

use crate::car_material::FxCarMaterial;
use crate::material::ATTRIBUTE_CAR_POSITION;
use crate::shadow::{FxCasterMaterial, CASTER_LAYER};

pub struct CarShadowPlugin;

impl Plugin for CarShadowPlugin {
    fn build(&self, app: &mut App) {
        // Drawn whatever the CSM does (FH1_DROPSHADOW=0 = off).
        app.add_plugins(drop_shadow::DropShadowPlugin);
        if std::env::var("FH1_CAR_CSM").is_ok_and(|v| v == "0") {
            return;
        }
        // PostUpdate: the body parts spawn in Update and their meshes are RENDER_WORLD only, so they must be read
        // before the next extract.
        app.add_systems(PostUpdate, spawn_body_casters);
        if k3_ab() {
            app.add_systems(Update, k3_ab_toggle);
        }
    }
}

/// FH1_K3_AB=1: in one run, switch every 5 s between the new car-draw paths (merged car CSM proxies here, the persistent
/// live-cube face camera in reflect.rs) and the old ones (per-part proxies, six toggled face cameras), and log each
/// mode's mean frame time (first 10 s and each switch frame dropped). Both modes see the same machine load.
pub(crate) fn k3_ab() -> bool {
    std::env::var("FH1_K3_AB").is_ok_and(|v| v == "1")
}

/// The FH1_K3_AB mode: true = the new paths.
pub(crate) static K3_NEW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// FH1_K3_AB: the per-part proxy spawned next to a merged one.
#[derive(Component)]
struct PerPartProxy;

/// FH1_K3_AB: a merged proxy.
#[derive(Component)]
struct MergedProxy;

#[allow(clippy::type_complexity)]
fn k3_ab_toggle(
    time: Res<Time<Real>>,
    mut acc: Local<([(f64, u32); 2], f32)>,
    mut per_part: Query<&mut Visibility, (With<PerPartProxy>, Without<MergedProxy>)>,
    mut merged: Query<&mut Visibility, (With<MergedProxy>, Without<PerPartProxy>)>,
) {
    use std::sync::atomic::Ordering;
    let t = time.elapsed_secs();
    let new = (t / 5.0) as u32 % 2 == 0;
    K3_NEW.store(new, Ordering::Relaxed);
    let vis = |on: bool| if on { Visibility::Inherited } else { Visibility::Hidden };
    per_part.iter_mut().for_each(|mut v| {
        v.set_if_neq(vis(!new));
    });
    merged.iter_mut().for_each(|mut v| {
        v.set_if_neq(vis(new));
    });
    if t > 10.0 && t % 5.0 > 0.25 {
        let a = &mut acc.0[new as usize];
        a.0 += time.delta_secs_f64() * 1000.0;
        a.1 += 1;
    }
    if t - acc.1 >= 20.0 {
        acc.1 = t;
        let m = |a: (f64, u32)| if a.1 > 0 { a.0 / a.1 as f64 } else { 0.0 };
        info!("K3 A/B: old {:.2} ms ({} frames), new {:.2} ms ({} frames)", m(acc.0[0]), acc.0[0].1, m(acc.0[1]), acc.0[1].1);
    }
}

/// On a game-shaded wheel, rotor or caliper part (fh1-render wheel.rs): the VS's c59 `wheelScale` and whether it is tyre geometry (two-zone radial scale),
/// so the CSM caster proxy (car_shadow.rs) can size it like the shader does. Wheel parts are not part of the
/// top-down body silhouette of the drop shadow.
#[derive(Component, Clone, Copy, Debug)]
pub struct FxWheelPart {
    pub scale: Vec4,
    pub tyre: bool,
    /// Pool correction applied before the VS scaling, p·w + xyz (LOD0 rims are refitted onto the LOD1 rim; identity
    /// = (0, 0, 0, 1)).
    pub fit: Vec4,
}

impl FxWheelPart {
    /// Brake rotors/calipers: drawn unscaled.
    pub const RIGID: Self = Self { scale: Vec4::new(1.0, 1.0, 1.0, 0.0), tyre: false, fit: Vec4::new(0.0, 0.0, 0.0, 1.0) };

    /// The tyre_V2 / rim_V2 VS position scaling (shaders 220 / 225, VERIFIED).
    pub fn apply(&self, p: Vec3) -> Vec3 {
        let p = p * self.fit.w + self.fit.truncate();
        let s = self.scale;
        let radial = if self.tyre && p.y * p.y + p.z * p.z >= s.w { s.z } else { s.y };
        Vec3::new(p.x * s.x, p.y * radial, p.z * radial)
    }
}

/// On a car body part: its shadow-only child.
#[derive(Component)]
pub struct CarCasterProxy(pub Entity);

/// Car parts that cast through one merged proxy (see [`spawn_body_casters`]).
#[allow(clippy::type_complexity)]
fn spawn_body_casters(
    mut commands: Commands,
    new: Query<(Entity, &Mesh3d, &MeshMaterial3d<FxCarMaterial>, &Aabb, Option<&FxWheelPart>, &Transform, Option<&ChildOf>), Added<MeshMaterial3d<FxCarMaterial>>>,
    materials: Res<Assets<FxCarMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut casters: ResMut<Assets<FxCasterMaterial>>,
    mut caster: Local<Option<Handle<FxCasterMaterial>>>,
) {
    if new.is_empty() {
        return;
    }
    // Back faces cast, like the game's ShadowDepthOnly passes (GUESSED for the car; two-sided gave acne all over
    // the body). FH1_CAR_CSM_CULL = 0 two-sided, 1 back faces culled, 2 front faces culled (default).
    let material = caster
        .get_or_insert_with(|| {
            let cull = std::env::var("FH1_CAR_CSM_CULL").ok().and_then(|v| v.parse().ok()).unwrap_or(2u8).min(2);
            casters.add(FxCasterMaterial::opaque(cull))
        })
        .clone();
    // Default: the parts spawned under one parent this frame (the body root, a wheel_XX / rotor_XX / caliper_XX node,
    // the cockpit root) cast through ONE merged proxy under that parent, baked with each part's local transform.
    // Same triangles, same caster material, so the same shadow, but one draw per cascade instead of ~140
    // (FH1_CAR_CSM_MERGE=0 = one proxy per part, the old way). A part hidden on its own (exterior cabin parts in the
    // cockpit view) keeps casting through the merged proxy; the parent's visibility still applies.
    let merge = std::env::var("FH1_CAR_CSM_MERGE").map_or(true, |v| v != "0");
    let mut groups: Vec<(Entity, Vec<Entity>, Vec<Mesh>)> = Vec::new();
    for (e, mesh, m, aabb, wheel, local, parent) in &new {
        // Glass and other blended parts don't cast (the game's shadow passes are opaque/alpha-tested only, INFERRED).
        if materials.get(&m.0).is_none_or(|m| m.alpha_blend) {
            continue;
        }
        let Some(mut proxy_mesh) = meshes.get(&mesh.0).and_then(|src| unpacked_mesh(src, aabb, wheel)) else { continue };
        match parent.filter(|_| merge) {
            Some(p) => {
                if k3_ab() {
                    commands.spawn((
                        Mesh3d(meshes.add(proxy_mesh.clone())),
                        MeshMaterial3d(material.clone()),
                        Transform::IDENTITY,
                        Visibility::Hidden,
                        RenderLayers::layer(CASTER_LAYER),
                        PerPartProxy,
                        ChildOf(e),
                    ));
                }
                if *local != Transform::IDENTITY {
                    proxy_mesh = proxy_mesh.transformed_by(*local);
                }
                match groups.iter_mut().find(|g| g.0 == p.parent()) {
                    Some(g) => {
                        g.1.push(e);
                        g.2.push(proxy_mesh);
                    }
                    None => groups.push((p.parent(), vec![e], vec![proxy_mesh])),
                }
            }
            None => {
                let proxy = commands
                    .spawn((Mesh3d(meshes.add(proxy_mesh)), MeshMaterial3d(material.clone()), Transform::IDENTITY, RenderLayers::layer(CASTER_LAYER), ChildOf(e)))
                    .id();
                commands.entity(e).insert((NotShadowCaster, CarCasterProxy(proxy)));
            }
        }
    }
    for (parent, parts, part_meshes) in groups {
        let Some((merged, aabb)) = merge_meshes(&part_meshes) else { continue };
        let proxy = commands
            .spawn((Mesh3d(meshes.add(merged)), MeshMaterial3d(material.clone()), aabb, Transform::IDENTITY, RenderLayers::layer(CASTER_LAYER), ChildOf(parent)))
            .id();
        if k3_ab() {
            commands.entity(proxy).insert(MergedProxy);
        }
        for e in parts {
            commands.entity(e).insert((NotShadowCaster, CarCasterProxy(proxy)));
        }
    }
}

/// Position-only triangle meshes (from [`unpacked_mesh`]) concatenated into one (u32 indices), and its bounds (the mesh is
/// render-world only, so Bevy can't compute them later).
fn merge_meshes(parts: &[Mesh]) -> Option<(Mesh, Aabb)> {
    let (mut pos, mut idx): (Vec<[f32; 3]>, Vec<u32>) = (Vec::new(), Vec::new());
    for m in parts {
        let Some(VertexAttributeValues::Float32x3(p)) = m.attribute(Mesh::ATTRIBUTE_POSITION) else { continue };
        let base = pos.len() as u32;
        pos.extend_from_slice(p);
        idx.extend(m.indices()?.iter().map(|i| base + i as u32));
    }
    if idx.is_empty() {
        return None;
    }
    let (lo, hi) = pos.iter().fold((Vec3::MAX, Vec3::MIN), |(lo, hi), p| (lo.min(Vec3::from(*p)), hi.max(Vec3::from(*p))));
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    m.insert_indices(Indices::U32(idx));
    Some((m, Aabb::from_min_max(lo, hi)))
}

/// The part's triangles with plain positions (see [`unpack_positions`]), pushed along their normals by
/// `FH1_CAR_CSM_NOFFSET` metres (default 0.03). Only back faces cast, so moving them outward moves them away from the
/// light: a caster-side normal-offset bias against the acne on the body's lit side (the rear quarter, where the
/// body is thin seen from the sun). Normals are averaged over vertices welded by position (the carbin pools split
/// vertices at UV seams), so the offset doesn't open cracks.
fn unpacked_mesh(src: &Mesh, aabb: &Aabb, wheel: Option<&FxWheelPart>) -> Option<Mesh> {
    let mut pos = unpack_positions(src, aabb)?;
    // Wheels: the size the rim_V2 / tire_V2 VS gives them (c59).
    if let Some(w) = wheel {
        pos.iter_mut().for_each(|p| *p = w.apply(*p));
    }
    let indices: Vec<usize> = src.indices()?.iter().collect();
    let offset = std::env::var("FH1_CAR_CSM_NOFFSET").ok().and_then(|v| v.parse().ok()).unwrap_or(0.03f32);
    if offset != 0.0 {
        let key = |p: Vec3| (p * 2000.0).round().as_ivec3();
        let mut welded: std::collections::HashMap<IVec3, Vec3> = std::collections::HashMap::new();
        for t in indices.chunks_exact(3) {
            let [a, b, c] = [pos[t[0]], pos[t[1]], pos[t[2]]];
            // Area-weighted face normal; counter-clockwise = front (Bevy's default front face).
            let n = (b - a).cross(c - a);
            for &v in t {
                *welded.entry(key(pos[v])).or_default() += n;
            }
        }
        for p in pos.iter_mut() {
            if let Some(n) = welded.get(&key(*p)) {
                *p += n.normalize_or_zero() * offset;
            }
        }
    }
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos.into_iter().map(|p| p.to_array()).collect::<Vec<_>>());
    m.insert_indices(match src.indices()? {
        Indices::U16(i) => Indices::U16(i.clone()),
        Indices::U32(i) => Indices::U32(i.clone()),
    });
    Some(m)
}

/// A car part's vertex positions in part space. `pack_partPosition` = (section offset T, bounds half-diagonal S)
/// (car.rs `pack_part_position`); the part's Aabb is offset + bounds, whose centre is the offset because the
/// section bounds are centred (~0, INFERRED in car.rs), so T = Aabb centre and S = |Aabb half extents|.
pub fn unpack_positions(src: &Mesh, aabb: &Aabb) -> Option<Vec<Vec3>> {
    let Some(VertexAttributeValues::Snorm16x4(packed)) = src.attribute(ATTRIBUTE_CAR_POSITION) else { return None };
    let t = Vec3::from(aabb.center);
    let s = Vec3::from(aabb.half_extents).length();
    let snorm = |v: i16| (v as f32 / 32767.0).max(-1.0);
    Some(packed.iter().map(|p| Vec3::new(snorm(p[0]), snorm(p[1]), snorm(p[2])) * snorm(p[3]) * s + t).collect())
}
