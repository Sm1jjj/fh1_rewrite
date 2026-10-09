//! P12 static world (docs/PERF.md "P12 static world"; default-on, `FH1_STATIC_WORLD=0` = old ECS path). Remaster scenery (zone models, tiles,
//! props) as GPU-resident geometry + draw records instead of ECS meshes, so the per-frame CPU cost no longer scales with the
//! ~40k scenery entities.
//!
//! CHUNK 2: under the flag the scenery parts are NOT ECS meshes any more: `spawn` gives a bare streaming-handle entity
//! (ChildOf + Visibility + StaticInstance) and `draw.rs` draws every record through the remaster shader (STATIC_WORLD
//! variant) with Bevy's own bindless RemasterMaterial bind groups. Only opaque / cutout / unlit materials go static
//! (decals, water, additive stay ECS: they need sorting).
//! - Geometry: every remaster scenery mesh is packed (vertex pulling, [`VERTEX_WORDS`] words / vertex: position, normal,
//!   uv0, uv1, uv2, colour) when it is prepared, keyed by its `AssetId<Mesh>`, and freed when that asset goes.
//! - Instances: a [`StaticInstance`] component on the scenery entity (the entity stays the streaming handle: tiles, LOD
//!   levels, zone parking and smashing keep working unchanged); its `on_remove` hook frees the record. Zone fades write
//!   the dither tag through [`set_tag`].
//! - Main -> render: a global op queue (`Op`), drained each frame by the render world into the [`Arena`]: one vertex
//!   buffer, one index buffer (first-fit ranges, grown x1.5 with a GPU copy) and a record buffer (one 96-byte
//!   [`GpuRecord`] per instance slot). Uploads only what changed.
//! Next chunks: material table + draw (2), GPU cull (3), shadows / probe (4), parity (5).
//! Bake path (P12 chunk 5, fh1-rewrite-15): geometry is keyed by [`GeoKey`] (a mesh asset, or a baked key from a bundle
//! file); [`bake`] writes / reads per-bundle files (prop tiles, zone models, the prop templates) and loads them as blocks
//! of geometry + instances without mesh assets or ECS entities.

use bevy::asset::UntypedAssetId;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use bevy::ecs::lifecycle::HookContext;
use bevy::ecs::world::DeferredWorld;
use bevy::mesh::VertexAttributeValues as V;
use bevy::prelude::*;
use bevy::render::render_resource::{Buffer, BufferDescriptor, BufferUsages, CommandEncoderDescriptor};
use bevy::render::renderer::{RenderDevice, RenderQueue};
use bevy::render::{Render, RenderApp, RenderSystems};

pub mod bake;
mod draw;
/// Hi-Z occlusion for the main view (chunk 5, 47).
mod hiz;

/// What a geometry in the arena is keyed by: a scenery mesh asset (the live path), or a baked bundle geometry ([`bake`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum GeoKey {
    Mesh(AssetId<Mesh>),
    Baked(u64),
}

/// Default-on (remaster only); `FH1_STATIC_WORLD=0` = old ECS scenery path.
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Bake mode (bake.rs, FH1_BAKE_CELLS) records what the static world receives, so it needs it on. Never under RTX:
    // rtx.rs traces Mesh3d entities, so static scenery would vanish from the ray-traced scene (dc's review).
    *ON.get_or_init(|| {
        #[cfg(feature = "rtx")]
        let rtx = crate::rtx::on();
        #[cfg(not(feature = "rtx"))]
        let rtx = false;
        crate::enabled() && !rtx && (std::env::var("FH1_STATIC_WORLD").map_or(true, |v| v != "0") || bake::baking())
    })
}

/// The static world is on and draws its scenery into the car probe faces (`FH1_STATIC_WORLD_PROBE`, default on).
pub(crate) fn probe_faces_on() -> bool {
    on() && draw::probe_faces()
}

/// Packed vertex: position (3), normal (3), uv0 (2), uv1 (2), uv2 (2) as f32, colour as unorm8x4 = 13 words.
pub const VERTEX_WORDS: usize = 13;

/// One draw record on the GPU (std430, 96 bytes). `world_from_local` rows (3 x vec4: affine transposed), world bounds with
/// the LOD band (`lod` = start margin start / end, end margin start / end, m), the draw's ranges and keys.
#[derive(Clone, Copy, Default, Debug)]
#[repr(C)]
pub struct GpuRecord {
    pub rows: [[f32; 4]; 3],
    pub aabb_min: [f32; 4],
    pub aabb_max: [f32; 4],
    pub lod: [f32; 4],
    pub first_index: u32,
    pub index_count: u32,
    pub base_vertex: u32,
    /// Material slot (chunk 2; 0 until then).
    pub material: u32,
    pub flags: u32,
    /// Zone fade dither level (-16 gone .. 0 drawn .. 16), as the MeshTag path.
    pub tag: i32,
    pub _pad: [u32; 2],
}

const RECORD_BYTES: u64 = std::mem::size_of::<GpuRecord>() as u64;

/// Record flags.
pub const FLAG_CASTS: u32 = 1;
/// Mirrored placement (negative determinant): culling flipped.
pub const FLAG_MIRRORED: u32 = 2;
/// The record is live (a freed slot has 0).
pub const FLAG_LIVE: u32 = 4;
/// Geometry / record flags: alpha-tested (cutout) material, double-sided material, hidden (parent Visibility).
pub const FLAG_MASK: u32 = 8;
pub const FLAG_TWO_SIDED: u32 = 16;
pub const FLAG_HIDDEN: u32 = 32;
/// P18: the record's material may draw in the P15-A depth pre-pass (opaque class, no cloth wave, no decal mask). Set by
/// the render world (draw.rs resolve_materials) from [`pre_ok`]; read by the pre-pass instead of the material table, so
/// the pre-pass binds no material slab (`FH1_SW_PREPASS_NOMAT=0` = old: the shader reads the table).
pub const FLAG_PRE_OK: u32 = 64;

/// P18: whether a RemasterMaterial may be pre-passed: the same rule as the pre-pass shader's old material test (info.z:
/// no cloth 0x80 / decal mask 0x8000 flag; class not cutout 1, decal 2, water 4, additive 5).
pub fn pre_ok(m: &crate::material::RemasterMaterial) -> bool {
    let z = m.extension.params.info.z;
    let cls = (z >> 16) & 0xff;
    z & 0x8080 == 0 && !matches!(cls, 1 | 2 | 4 | 5)
}

/// P18: the pre-pass reads FLAG_PRE_OK and binds no material (`FH1_SW_PREPASS_NOMAT=0` = old).
pub fn prepass_nomat_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SW_PREPASS_NOMAT").map_or(true, |v| v != "0"))
}

/// What a scenery entity draws (main world): its slot in the record buffer.
#[derive(Component, Debug)]
#[component(on_remove = free_instance)]
pub struct StaticInstance(pub u32);

fn free_instance(world: DeferredWorld, ctx: HookContext) {
    if let Some(i) = world.get::<StaticInstance>(ctx.entity) {
        push(Op::RemoveInstance(i.0));
    }
}

/// A new instance (main world API).
pub struct InstanceDesc {
    pub mesh: AssetId<Mesh>,
    /// The entity's material (RemasterMaterial asset id, resolved to a table slot by the render world in chunk 2).
    pub material: UntypedAssetId,
    pub transform: Mat4,
    /// Mesh-space bounds.
    pub local_min: Vec3,
    pub local_max: Vec3,
    /// VisibilityRange margins (m): start margin, end margin. `None` = always in range.
    pub range: Option<(std::ops::Range<f32>, std::ops::Range<f32>)>,
    pub casts: bool,
    pub tag: i32,
}

enum Op {
    AddGeometry { mesh: GeoKey, vertices: Vec<u32>, indices: Vec<u32> },
    RemoveGeometry(GeoKey),
    AddInstance { slot: u32, mesh: GeoKey, material: UntypedAssetId, record: GpuRecord },
    RemoveInstance(u32),
    SetTag(u32, i32),
    SetHidden(u32, bool),
    /// A material was modified (night writes): re-resolve its records' bindless slot / slab.
    Rebind(UntypedAssetId),
    /// P18: a material's pre-pass eligibility ([`pre_ok`]); sent before its Rebind.
    PreClass(UntypedAssetId, bool),
}

/// Main-world bookkeeping: packed geometry ids and free instance slots.
#[derive(Default)]
struct MainState {
    /// Packed geometry and its flags (FLAG_MASK / FLAG_TWO_SIDED).
    geometry: HashMap<GeoKey, u32>,
    next_slot: u32,
    free_slots: Vec<u32>,
    ops: Vec<Op>,
    stats: Stats,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub geometries: u32,
    pub instances: u32,
    pub vertex_bytes: u64,
    pub index_bytes: u64,
}

static STATE: Mutex<Option<MainState>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut MainState) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(MainState::default))
}

fn push(op: Op) {
    with(|s| {
        if let Op::RemoveInstance(slot) = op {
            s.free_slots.push(slot);
            s.stats.instances = s.stats.instances.saturating_sub(1);
        }
        s.ops.push(op);
    });
}

/// Render-world counters (perf CSV): draw candidates and views culled in the last frame.
pub fn render_stats() -> (u32, u32) {
    use std::sync::atomic::Ordering::Relaxed;
    (draw::CANDIDATES.load(Relaxed), draw::VIEWS.load(Relaxed))
}

/// Main-world counters (perf CSV / logs).
pub fn stats() -> Stats {
    with(|s| s.stats)
}

/// A scenery mesh packed for the arena (vertex words + u32 indices).
pub struct Packed {
    vertices: Vec<u32>,
    indices: Vec<u32>,
}

/// Packs `mesh` (a remaster scenery mesh, `scenery::prepare_mesh` layout) before it goes into `Assets<Mesh>` (its CPU
/// data is dropped after upload). None when off or for another layout.
pub fn pack_mesh(mesh: &Mesh) -> Option<Packed> {
    if !on() {
        return None;
    }
    pack(mesh).map(|(vertices, indices)| Packed { vertices, indices })
}

/// Registers packed geometry for mesh asset `id` (once; later calls for the same id are ignored). `flags`: FLAG_MASK /
/// FLAG_TWO_SIDED of its material.
pub fn add_packed(id: AssetId<Mesh>, p: Packed, flags: u32) {
    bake::record_geometry(id, &p, flags);
    add_geometry(GeoKey::Mesh(id), p.vertices, p.indices, flags);
}

/// Registers geometry under `key` (once). False when it was already there.
fn add_geometry(key: GeoKey, vertices: Vec<u32>, indices: Vec<u32>, flags: u32) -> bool {
    with(|s| {
        if s.geometry.insert(key, flags).is_some() {
            return false;
        }
        s.stats.geometries += 1;
        s.stats.vertex_bytes += vertices.len() as u64 * 4;
        s.stats.index_bytes += indices.len() as u64 * 4;
        s.ops.push(Op::AddGeometry { mesh: key, vertices, indices });
        true
    })
}

/// Frees geometry `key` (baked bundles; mesh-asset geometry goes with its asset).
fn remove_geometry(key: GeoKey) {
    let gone = with(|s| {
        let had = s.geometry.remove(&key).is_some();
        if had {
            s.stats.geometries = s.stats.geometries.saturating_sub(1);
        }
        had
    });
    if gone {
        push(Op::RemoveGeometry(key));
    }
}

/// Whether `id` has packed geometry.
pub fn has_geometry(id: AssetId<Mesh>) -> bool {
    on() && with(|s| s.geometry.contains_key(&GeoKey::Mesh(id)))
}

/// Adds an instance; returns the component for its entity (None when off or the mesh isn't packed).
pub fn add_instance(d: InstanceDesc) -> Option<StaticInstance> {
    if !on() {
        return None;
    }
    bake::record_instance(&d);
    add_instance_key(GeoKey::Mesh(d.mesh), d.material, d.transform, d.local_min, d.local_max, d.range, d.casts, d.tag).map(StaticInstance)
}

/// [`add_instance`] for any geometry key; returns the record slot.
#[allow(clippy::too_many_arguments)]
fn add_instance_key(
    geo: GeoKey,
    material: UntypedAssetId,
    transform: Mat4,
    local_min: Vec3,
    local_max: Vec3,
    range: Option<(std::ops::Range<f32>, std::ops::Range<f32>)>,
    casts: bool,
    tag: i32,
) -> Option<u32> {
    let geo_flags = with(|s| s.geometry.get(&geo).copied())?;
    let d = InstanceDescKey { transform, local_min, local_max, range, casts, tag };
    let (wmin, wmax) = world_bounds(d.transform, d.local_min, d.local_max);
    let t = d.transform.transpose();
    let rows = [t.x_axis.to_array(), t.y_axis.to_array(), t.z_axis.to_array()];
    let lod = d.range.map_or([-2.0, -1.0, 1.0e7, 2.0e7], |(s, e)| [s.start, s.end, e.start, e.end]);
    let mirrored = d.transform.determinant() < 0.0;
    let flags = FLAG_LIVE | geo_flags | if d.casts { FLAG_CASTS } else { 0 } | if mirrored { FLAG_MIRRORED } else { 0 };
    let record = GpuRecord { rows, aabb_min: wmin.extend(0.0).to_array(), aabb_max: wmax.extend(0.0).to_array(), lod, flags, tag: d.tag, ..default() };
    let slot = with(|s| {
        let slot = s.free_slots.pop().unwrap_or_else(|| {
            s.next_slot += 1;
            s.next_slot - 1
        });
        s.stats.instances += 1;
        s.ops.push(Op::AddInstance { slot, mesh: geo, material, record });
        slot
    });
    Some(slot)
}

/// The placement half of [`InstanceDesc`] (shared by the live and the baked path).
struct InstanceDescKey {
    transform: Mat4,
    local_min: Vec3,
    local_max: Vec3,
    range: Option<(std::ops::Range<f32>, std::ops::Range<f32>)>,
    casts: bool,
    tag: i32,
}

/// Hidden flag of a record slot (baked bundles: zone switches).
fn set_hidden_slot(slot: u32, hidden: bool) {
    with(|s| s.ops.push(Op::SetHidden(slot, hidden)));
}

/// Zone fade dither level of an instance (record slot).
pub fn set_tag(slot: u32, level: i32) {
    if on() {
        with(|s| s.ops.push(Op::SetTag(slot, level)));
    }
}

/// Spawns the streaming-handle entity of a static scenery part under `parent` (no Mesh3d: the static world draws it).
/// None when the part isn't static (off, not packed, or a sorted material): spawn the ECS mesh instead.
pub fn spawn(commands: &mut Commands, parent: Entity, d: InstanceDesc) -> Option<(Entity, u32)> {
    let i = add_instance(d)?;
    let slot = i.0;
    let e = commands.spawn((ChildOf(parent), Visibility::Inherited, i)).id();
    Some((e, slot))
}

/// Parent visibility (zone switches, tiles being placed, retired roots, P2 A/B) -> the record's hidden flag.
fn sync_visibility(q: Query<(&StaticInstance, &InheritedVisibility), Changed<InheritedVisibility>>) {
    let mut ops: Vec<Op> = q.iter().map(|(i, v)| Op::SetHidden(i.0, !v.get())).collect();
    if !ops.is_empty() {
        with(|s| s.ops.append(&mut ops));
    }
}

fn world_bounds(m: Mat4, lo: Vec3, hi: Vec3) -> (Vec3, Vec3) {
    let (mut a, mut b) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for k in 0..8 {
        let p = Vec3::new(if k & 1 == 0 { lo.x } else { hi.x }, if k & 2 == 0 { lo.y } else { hi.y }, if k & 4 == 0 { lo.z } else { hi.z });
        let w = m.transform_point3(p);
        a = a.min(w);
        b = b.max(w);
    }
    (a, b)
}

/// The scenery layout -> packed words (+ u32 indices). None for other layouts / no indices.
fn pack(mesh: &Mesh) -> Option<(Vec<u32>, Vec<u32>)> {
    let n = mesh.count_vertices();
    let f3 = |a: Option<&V>| match a {
        Some(V::Float32x3(v)) if v.len() == n => Some(v.clone()),
        _ => None,
    };
    let f2 = |a: Option<&V>| match a {
        Some(V::Float32x2(v)) if v.len() == n => v.clone(),
        _ => vec![[0.0; 2]; n],
    };
    let pos = f3(mesh.attribute(Mesh::ATTRIBUTE_POSITION))?;
    let nrm = f3(mesh.attribute(Mesh::ATTRIBUTE_NORMAL)).unwrap_or_else(|| vec![[0.0, 1.0, 0.0]; n]);
    let uv0 = f2(mesh.attribute(Mesh::ATTRIBUTE_UV_0));
    let uv1 = f2(mesh.attribute(Mesh::ATTRIBUTE_UV_1));
    let uv2 = f2(mesh.attribute(fh1_render::material::ATTRIBUTE_UV2));
    let col: Vec<[u8; 4]> = match mesh.attribute(fh1_render::material::ATTRIBUTE_COLOR) {
        Some(V::Unorm8x4(v)) if v.len() == n => v.clone(),
        _ => vec![[255; 4]; n],
    };
    let mut out = Vec::with_capacity(n * VERTEX_WORDS);
    for i in 0..n {
        for v in pos[i].iter().chain(&nrm[i]).chain(&uv0[i]).chain(&uv1[i]).chain(&uv2[i]) {
            out.push(v.to_bits());
        }
        out.push(u32::from_le_bytes(col[i]));
    }
    let idx: Vec<u32> = match mesh.indices()? {
        bevy::mesh::Indices::U16(i) => i.iter().map(|&x| x as u32).collect(),
        bevy::mesh::Indices::U32(i) => i.clone(),
    };
    Some((out, idx))
}

/// Frees geometry when its mesh asset goes (main world).
fn free_geometry(mut events: MessageReader<AssetEvent<Mesh>>) {
    for e in events.read() {
        if let AssetEvent::Removed { id } = e {
            remove_geometry(GeoKey::Mesh(*id));
        }
    }
}

/// Material changes (47's dusk / dawn night writes, 256 per frame): Bevy re-prepares the material, which may move it to
/// another bindless slot or slab; the records using it are re-resolved (draw.rs resolve_materials).
/// `FH1_STATIC_WORLD_REBIND=0` = never re-resolve (chunk 4).
fn watch_materials(mut events: MessageReader<AssetEvent<crate::material::RemasterMaterial>>, materials: Res<Assets<crate::material::RemasterMaterial>>) {
    let rebind = !std::env::var("FH1_STATIC_WORLD_REBIND").is_ok_and(|v| v == "0");
    let mut ops: Vec<Op> = Vec::new();
    for e in events.read() {
        if let AssetEvent::Modified { id } | AssetEvent::Added { id } = e {
            // P18: the pre-pass class first, so the re-resolve below sees it.
            if prepass_nomat_on() {
                if let Some(m) = materials.get(*id) {
                    ops.push(Op::PreClass(id.untyped(), pre_ok(m)));
                }
            }
            if rebind {
                ops.push(Op::Rebind(id.untyped()));
            }
        }
    }
    if !ops.is_empty() {
        with(|s| s.ops.append(&mut ops));
    }
}

fn log_stats(time: Res<Time<Real>>, mut last: Local<f32>) {
    let now = time.elapsed_secs();
    if now - *last < 10.0 {
        return;
    }
    *last = now;
    let s = stats();
    info!(
        "static world: {} geometries ({:.1} MB vertices, {:.1} MB indices), {} instances",
        s.geometries,
        s.vertex_bytes as f64 / 1048576.0,
        s.index_bytes as f64 / 1048576.0,
        s.instances
    );
}

pub fn plugin(app: &mut App) {
    if !on() {
        return;
    }
    info!("static world: ON (P12: scenery drawn from the GPU arena, GPU-culled per view, shadows + car probe)");
    draw::register_shaders(app);
    app.add_systems(Last, (free_geometry, log_stats, watch_materials)).add_systems(PostUpdate, sync_visibility.after(bevy::camera::visibility::VisibilitySystems::VisibilityPropagate));
    if let Some(ra) = app.get_sub_app_mut(RenderApp) {
        ra.init_resource::<Arena>().add_systems(Render, apply_ops.in_set(RenderSystems::PrepareResources));
        draw::plugin(ra);
    }
}

// ---------------------------------------------------------------- render world

/// First-fit range allocator over u32 units (free ranges coalesced).
#[derive(Default)]
struct RangeAlloc {
    cap: u32,
    free: BTreeMap<u32, u32>,
}

impl RangeAlloc {
    fn alloc(&mut self, n: u32) -> Option<u32> {
        let (&start, &len) = self.free.iter().find(|(_, &len)| len >= n)?;
        self.free.remove(&start);
        if len > n {
            self.free.insert(start + n, len - n);
        }
        Some(start)
    }

    fn release(&mut self, start: u32, n: u32) {
        let (mut s, mut len) = (start, n);
        if let Some((&ps, &pl)) = self.free.range(..start).next_back() {
            if ps + pl == start {
                self.free.remove(&ps);
                s = ps;
                len += pl;
            }
        }
        if let Some(&nl) = self.free.get(&(start + n)) {
            self.free.remove(&(start + n));
            len += nl;
        }
        self.free.insert(s, len);
    }

    /// Grows to at least `need` more units at the end; returns the new capacity.
    fn grow(&mut self, need: u32) -> u32 {
        let new_cap = ((self.cap as f64 * 1.5) as u32).max(self.cap + need).max(1 << 20);
        self.release(self.cap, new_cap - self.cap);
        self.cap = new_cap;
        new_cap
    }
}

/// A growable GPU buffer of u32 words with a range allocator.
struct WordBuffer {
    label: &'static str,
    usage: BufferUsages,
    buffer: Option<Buffer>,
    alloc: RangeAlloc,
}

impl WordBuffer {
    fn new(label: &'static str, usage: BufferUsages) -> Self {
        Self { label, usage, buffer: None, alloc: RangeAlloc::default() }
    }

    /// Allocates and uploads `data`; returns its first word.
    fn put(&mut self, device: &RenderDevice, queue: &RenderQueue, data: &[u32]) -> u32 {
        let n = data.len().max(1) as u32;
        let start = match self.alloc.alloc(n) {
            Some(s) => s,
            None => {
                let old_cap = self.alloc.cap;
                let cap = self.alloc.grow(n);
                let new = device.create_buffer(&BufferDescriptor {
                    label: Some(self.label),
                    size: cap as u64 * 4,
                    usage: self.usage | BufferUsages::COPY_DST | BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
                if let (Some(old), true) = (&self.buffer, old_cap > 0) {
                    let mut enc = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("static world grow") });
                    enc.copy_buffer_to_buffer(old, 0, &new, 0, old_cap as u64 * 4);
                    queue.submit([enc.finish()]);
                }
                self.buffer = Some(new);
                GROWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.alloc.alloc(n).expect("grown")
            }
        };
        if let Some(b) = &self.buffer {
            if !data.is_empty() {
                queue.write_buffer(b, start as u64 * 4, bytemuck_words(data));
            }
        }
        start
    }
}

/// Bumped when the vertex / index buffer is re-created (draw bind group rebuild).
static GROWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn bytemuck_words(w: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding; any byte view of it is valid.
    unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, std::mem::size_of_val(w)) }
}

/// Geometry ranges in the arena.
#[derive(Clone, Copy)]
struct GeoRange {
    first_vertex: u32,
    vertex_count: u32,
    first_index: u32,
    index_count: u32,
}

/// The render-world arena (chunk 1: buffers and records; drawn from chunk 2).
#[derive(Resource)]
pub struct Arena {
    vertices: WordBuffer,
    indices: WordBuffer,
    geometry: HashMap<GeoKey, GeoRange>,
    /// CPU copy of every record slot (uploaded per changed slot) and the slots' meshes / materials.
    pub(crate) records: Vec<GpuRecord>,
    pub(crate) slot_mesh: Vec<Option<(GeoKey, UntypedAssetId)>>,
    pub(crate) record_buffer: Option<Buffer>,
    record_cap: u32,
    /// Material bind group (bindless slab) per slot, once resolved; slots waiting for their material's binding.
    pub(crate) slot_group: Vec<Option<u32>>,
    pub(crate) pending: Vec<u32>,
    /// Material -> its record slots (re-binding), and slots to re-resolve once more next frame.
    by_material: HashMap<UntypedAssetId, Vec<u32>>,
    pub(crate) recheck: Vec<u32>,
    /// P18: pre-pass eligibility per material ([`pre_ok`]; missing = not pre-passed).
    pub(crate) pre_ok: HashMap<UntypedAssetId, bool>,
    /// Records / buffers changed since the draw lists were built (draw.rs).
    pub(crate) dirty: bool,
    /// Bumped whenever a GPU buffer is re-created (the draw bind group must be rebuilt).
    pub(crate) buffers_generation: u32,
}

impl Default for Arena {
    fn default() -> Self {
        Self {
            vertices: WordBuffer::new("static world vertices", BufferUsages::STORAGE),
            indices: WordBuffer::new("static world indices", BufferUsages::INDEX),
            geometry: HashMap::new(),
            records: Vec::new(),
            slot_mesh: Vec::new(),
            record_buffer: None,
            record_cap: 0,
            slot_group: Vec::new(),
            pending: Vec::new(),
            by_material: HashMap::new(),
            recheck: Vec::new(),
            pre_ok: HashMap::new(),
            dirty: false,
            buffers_generation: 0,
        }
    }
}

impl Arena {
    pub(crate) fn vertex_buffer(&self) -> Option<&Buffer> {
        self.vertices.buffer.as_ref()
    }

    pub(crate) fn index_buffer(&self) -> Option<&Buffer> {
        self.indices.buffer.as_ref()
    }

    pub(crate) fn write_record(&mut self, device: &RenderDevice, queue: &RenderQueue, slot: u32) {
        let need = slot + 1;
        if need > self.record_cap || self.record_buffer.is_none() {
            let cap = need.max(self.record_cap + self.record_cap / 2).max(16384);
            let new = device.create_buffer(&BufferDescriptor {
                label: Some("static world records"),
                size: cap as u64 * RECORD_BYTES,
                usage: BufferUsages::STORAGE | BufferUsages::COPY_DST | BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            // Re-upload every record (rare: growth).
            self.record_buffer = Some(new);
            self.record_cap = cap;
            self.buffers_generation += 1;
            if let Some(b) = &self.record_buffer {
                let bytes = records_bytes(&self.records);
                if !bytes.is_empty() {
                    queue.write_buffer(b, 0, bytes);
                }
            }
            return;
        }
        if let (Some(b), Some(r)) = (&self.record_buffer, self.records.get(slot as usize)) {
            queue.write_buffer(b, slot as u64 * RECORD_BYTES, records_bytes(std::slice::from_ref(r)));
        }
    }
}

impl Arena {
    /// Uploads the records of `slots` (sorted + deduplicated here) as contiguous runs, one `write_buffer` per run; gaps of
    /// up to 16 slots are bridged (the CPU copy is current for every slot). Growth falls back to `write_record` (which
    /// re-uploads every record). See [`batch_ops_on`].
    pub(crate) fn write_records(&mut self, device: &RenderDevice, queue: &RenderQueue, slots: &mut Vec<u32>) {
        const GAP: u32 = 16;
        if slots.is_empty() {
            return;
        }
        slots.sort_unstable();
        slots.dedup();
        let max = slots[slots.len() - 1];
        if max + 1 > self.record_cap || self.record_buffer.is_none() {
            self.write_record(device, queue, max);
            return;
        }
        let Some(b) = self.record_buffer.as_ref() else { return };
        let n = self.records.len();
        let mut i = 0;
        while i < slots.len() {
            let start = slots[i];
            let mut end = start;
            let mut j = i + 1;
            while j < slots.len() && slots[j] <= end + 1 + GAP {
                end = slots[j];
                j += 1;
            }
            let (lo, hi) = (start as usize, (end as usize + 1).min(n));
            if lo < hi {
                queue.write_buffer(b, lo as u64 * RECORD_BYTES, records_bytes(&self.records[lo..hi]));
            }
            i = j;
        }
    }
}

fn records_bytes(r: &[GpuRecord]) -> &[u8] {
    // SAFETY: GpuRecord is repr(C) of 4-byte fields, no padding beyond explicit fields.
    unsafe { std::slice::from_raw_parts(r.as_ptr() as *const u8, std::mem::size_of_val(r)) }
}

/// Batched record uploads (P13 micro-stutter, 2026-10-08; `FH1_SW_BATCH_OPS=0` = old). A zone fade sets the dither tag
/// of every record of the appearing and the disappearing zone model on each level step (16 steps per fade), and a zone
/// switch hides / shows whole bundles: thousands of ops in one frame. The old path wrote each record with its own
/// `queue.write_buffer` (96 bytes each) AND flagged the arena dirty, so `draw.rs` rebuilt the whole candidate list
/// (every record, hashed into bins; resets the Hi-Z bits) on every fade step. Zone switches come every few seconds at
/// speed, so that was a burst of heavy frames each time. Now the changed slots are collected and uploaded as contiguous
/// runs (one write per run), and only ops that change list membership (instances added / removed, geometry, rebinds,
/// hiding without GPU culling) rebuild the lists: tags and hidden flags are read by the GPU cull from the records.
pub(crate) fn batch_ops_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SW_BATCH_OPS").map_or(true, |v| v != "0"))
}

/// Applies the main world's ops (render world, once per frame).
fn apply_ops(mut arena: ResMut<Arena>, device: Res<RenderDevice>, queue: Res<RenderQueue>) {
    let ops = with(|s| std::mem::take(&mut s.ops));
    let grown = GROWN.swap(0, std::sync::atomic::Ordering::Relaxed);
    if grown > 0 {
        arena.buffers_generation += grown;
    }
    if ops.is_empty() {
        return;
    }
    let a = &mut *arena;
    let batch = batch_ops_on();
    let cull = draw::culling();
    // Slots whose record changed (batch mode: uploaded once, in runs, after the loop) and whether the lists must rebuild.
    let mut touched: Vec<u32> = Vec::new();
    let mut membership = !batch;
    for op in ops {
        match op {
            Op::AddGeometry { mesh, vertices, indices } => {
                membership = true;
                let vc = (vertices.len() / VERTEX_WORDS) as u32;
                let fv = a.vertices.put(&device, &queue, &vertices) / VERTEX_WORDS as u32;
                let fi = a.indices.put(&device, &queue, &indices);
                a.geometry.insert(mesh, GeoRange { first_vertex: fv, vertex_count: vc, first_index: fi, index_count: indices.len() as u32 });
            }
            Op::RemoveGeometry(mesh) => {
                membership = true;
                if let Some(g) = a.geometry.remove(&mesh) {
                    a.vertices.alloc.release(g.first_vertex * VERTEX_WORDS as u32, (g.vertex_count * VERTEX_WORDS as u32).max(1));
                    a.indices.alloc.release(g.first_index, g.index_count.max(1));
                }
            }
            Op::AddInstance { slot, mesh, material, mut record } => {
                membership = true;
                let s = slot as usize;
                if a.records.len() <= s {
                    a.records.resize(s + 1, GpuRecord::default());
                    a.slot_mesh.resize(s + 1, None);
                    a.slot_group.resize(s + 1, None);
                }
                a.slot_group[s] = None;
                a.pending.push(slot);
                if let Some(g) = a.geometry.get(&mesh) {
                    record.first_index = g.first_index;
                    record.index_count = g.index_count;
                    record.base_vertex = g.first_vertex;
                } else {
                    record.flags &= !FLAG_LIVE;
                }
                a.records[s] = record;
                a.slot_mesh[s] = Some((mesh, material));
                a.by_material.entry(material).or_default().push(slot);
                if batch {
                    touched.push(slot);
                } else {
                    a.write_record(&device, &queue, slot);
                }
            }
            Op::RemoveInstance(slot) => {
                membership = true;
                if let Some(Some((_, m))) = a.slot_mesh.get(slot as usize) {
                    if let Some(v) = a.by_material.get_mut(m) {
                        v.retain(|&x| x != slot);
                    }
                }
                if let Some(r) = a.records.get_mut(slot as usize) {
                    *r = GpuRecord::default();
                    a.slot_mesh[slot as usize] = None;
                    a.slot_group[slot as usize] = None;
                    if batch {
                        touched.push(slot);
                    } else {
                        a.write_record(&device, &queue, slot);
                    }
                }
            }
            Op::SetTag(slot, level) => {
                if let Some(r) = a.records.get_mut(slot as usize) {
                    r.tag = level;
                    if batch {
                        touched.push(slot);
                    } else {
                        a.write_record(&device, &queue, slot);
                    }
                }
            }
            Op::PreClass(material, ok) => {
                if ok {
                    a.pre_ok.insert(material, true);
                } else {
                    a.pre_ok.remove(&material);
                }
                // Without rebinding (FH1_STATIC_WORLD_REBIND=0) the records still need the new flag.
                if let Some(v) = a.by_material.get(&material) {
                    a.pending.extend_from_slice(v);
                }
            }
            Op::Rebind(material) => {
                membership = true;
                if let Some(v) = a.by_material.get(&material) {
                    a.pending.extend_from_slice(v);
                    a.recheck.extend_from_slice(v);
                }
            }
            Op::SetHidden(slot, hidden) => {
                if let Some(r) = a.records.get_mut(slot as usize) {
                    if hidden {
                        r.flags |= FLAG_HIDDEN;
                    } else {
                        r.flags &= !FLAG_HIDDEN;
                    }
                    if batch {
                        touched.push(slot);
                        // Without GPU culling the CPU args skip hidden records: the lists must follow.
                        membership |= !cull;
                    } else {
                        a.write_record(&device, &queue, slot);
                    }
                }
            }
        }
    }
    a.write_records(&device, &queue, &mut touched);
    if membership {
        a.dirty = true;
    }
}
