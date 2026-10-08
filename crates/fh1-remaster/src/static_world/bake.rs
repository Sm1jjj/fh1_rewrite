//! P12 bake path (docs/PERF.md "P12 static world", chunk 5; fh1-rewrite-15): the static world's geometry and instances
//! written per bundle at setup time and loaded as blocks at runtime, so streaming a prop tile or a zone model costs one
//! file read and a few ops instead of Mesh assets, a merge and ECS entities.
//!
//! Bundles (dc's split, matching streaming): `templates` (every prop template's packed parts, loaded once), one per prop
//! tile `props_<x>_<z>` (every non-smashable placement x LOD level, referencing template geometry), one per zone model
//! `zone_<n>` (its own geometry + identity records). Zone streaming, LOD by distance, fades and the tile rings stay
//! runtime; the bundle only replaces the per-object work.
//!
//! Baking (engine `FH1_BAKE_CELLS=<dir>`, driven by scenery.rs): the same loaders run with the static world on; between
//! [`begin`] and [`end`] every [`super::add_packed`] / [`super::add_instance`] is recorded; [`end`] returns the bundle,
//! whose materials the caller turns into [`MatKey`]s (`fh1_remaster::scenery` knows each material's origin) before
//! [`write`]. Geometry recorded in one bundle and used by another (templates) is referenced by its key.
//!
//! File (little-endian): `FH1BAKE\0`, u32 [`VERSION`], u32 header length + header JSON ({"name", "stamp": {group ->
//! version / hash}}), u32 geometry count + per geometry: u64 key, u32 flags, u32 words, u32 indices, words, indices;
//! u32 instance count + per instance: u64 geometry key, u32 game material, u32 lightmap (u32::MAX = none), u32 mirrored,
//! 12 f32 affine (3 rows of the transposed matrix, as GpuRecord), 3 + 3 f32 local bounds, u32 has range + 4 f32 range,
//! u32 casts, i32 tag; u32 extra count + u32 extra (prop tiles: the baked placement indices). A file whose version or
//! stamp differs from the running install is ignored (the live path runs).

use std::collections::HashMap;
use std::sync::Mutex;

use bevy::asset::UntypedAssetId;
use bevy::prelude::*;

use super::{GeoKey, InstanceDesc, Packed, VERTEX_WORDS};

/// Bump when the file layout or what goes into a bundle changes.
pub const VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"FH1BAKE\0";

/// A material as the bake stores it: the game material (remaster table index), its night lightmap variant and mirroring.
/// Tint never reaches a single placement's RemasterMaterial, so it isn't part of the key (dc).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MatKey {
    pub game_material: u32,
    pub lightmap: Option<u32>,
    pub mirrored: bool,
}

/// One baked geometry.
#[derive(Clone, Debug)]
pub struct BakedGeo {
    pub key: u64,
    pub flags: u32,
    pub words: Vec<u32>,
    pub indices: Vec<u32>,
}

/// One baked instance (material as recorded: an asset id while baking, a key in a file).
#[derive(Clone, Debug)]
pub struct BakedInstance<M> {
    pub geo: u64,
    pub material: M,
    pub transform: Mat4,
    pub local_min: Vec3,
    pub local_max: Vec3,
    pub range: Option<[f32; 4]>,
    pub casts: bool,
    pub tag: i32,
}

/// A bundle: its own geometry and its instances.
#[derive(Clone, Debug)]
pub struct Bundle<M> {
    pub name: String,
    pub geometries: Vec<BakedGeo>,
    pub instances: Vec<BakedInstance<M>>,
    /// Caller data (prop tiles: the placement indices the bundle holds; the rest stay on the live path).
    pub extra: Vec<u32>,
}

impl<M> Default for Bundle<M> {
    fn default() -> Self {
        Self { name: String::new(), geometries: Vec::new(), instances: Vec::new(), extra: Vec::new() }
    }
}

/// The stable key of geometry `index` of bundle `name`.
pub fn geo_key(name: &str, index: usize) -> u64 {
    // FNV-1a over the name, then the index in the low bits.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h << 20) ^ index as u64
}

// ---------------------------------------------------------------- recording (bake mode)

#[derive(Default)]
struct Recorder {
    /// The bundle being recorded.
    open: Option<Bundle<UntypedAssetId>>,
    /// Every geometry recorded so far (any bundle): mesh asset -> its baked key.
    keys: HashMap<AssetId<Mesh>, u64>,
}

static REC: Mutex<Option<Recorder>> = Mutex::new(None);

fn rec<R>(f: impl FnOnce(&mut Recorder) -> R) -> R {
    let mut g = REC.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Recorder::default))
}

/// Bake mode: `FH1_BAKE_CELLS=1` (the install's bake folder) or `=<dir>`; the engine forces the static world on and
/// drives the loaders (engine scenery.rs `bake_cells`).
pub fn bake_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("FH1_BAKE_CELLS").filter(|v| !v.is_empty() && v != "0").map(std::path::PathBuf::from)
}

/// Bake mode is on.
pub fn baking() -> bool {
    bake_dir().is_some()
}

/// Starts recording bundle `name` (any open one is dropped).
pub fn begin(name: &str) {
    rec(|r| r.open = Some(Bundle { name: name.to_owned(), ..default() }));
}

/// Ends the recording: the bundle with materials as asset ids (map them with [`Bundle::map_materials`]).
pub fn end() -> Option<Bundle<UntypedAssetId>> {
    rec(|r| r.open.take())
}

pub(super) fn record_geometry(id: AssetId<Mesh>, p: &Packed, flags: u32) {
    if !baking() {
        return;
    }
    rec(|r| {
        let Some(b) = r.open.as_mut() else { return };
        if r.keys.contains_key(&id) {
            return;
        }
        let key = geo_key(&b.name, b.geometries.len());
        b.geometries.push(BakedGeo { key, flags, words: p.vertices.clone(), indices: p.indices.clone() });
        r.keys.insert(id, key);
    });
}

pub(super) fn record_instance(d: &InstanceDesc) {
    if !baking() {
        return;
    }
    rec(|r| {
        let Some(&geo) = r.keys.get(&d.mesh) else { return };
        let Some(b) = r.open.as_mut() else { return };
        b.instances.push(BakedInstance {
            geo,
            material: d.material,
            transform: d.transform,
            local_min: d.local_min,
            local_max: d.local_max,
            range: d.range.as_ref().map(|(s, e)| [s.start, s.end, e.start, e.end]),
            casts: d.casts,
            tag: d.tag,
        });
    });
}

impl<M> Bundle<M> {
    /// The same bundle with its materials mapped (asset id -> [`MatKey`] when baking; key -> asset id when loading).
    /// Instances whose material doesn't map are dropped (counted in the returned number).
    pub fn map_materials<N>(self, mut f: impl FnMut(&M) -> Option<N>) -> (Bundle<N>, usize) {
        let mut dropped = 0;
        let instances = self
            .instances
            .into_iter()
            .filter_map(|i| match f(&i.material) {
                Some(material) => Some(BakedInstance { geo: i.geo, material, transform: i.transform, local_min: i.local_min, local_max: i.local_max, range: i.range, casts: i.casts, tag: i.tag }),
                None => {
                    dropped += 1;
                    None
                }
            })
            .collect();
        (Bundle { name: self.name, geometries: self.geometries, instances, extra: self.extra }, dropped)
    }
}

// ---------------------------------------------------------------- files

/// The install stamp a bundle must match: setup group -> version (or hash) of every group the bake depends on.
pub type Stamp = std::collections::BTreeMap<String, String>;

pub fn file_name(name: &str) -> String {
    format!("{name}.bake")
}

/// Writes `b` to `<dir>/<name>.bake` (tmp + rename).
pub fn write(dir: &std::path::Path, b: &Bundle<MatKey>, stamp: &Stamp) -> std::io::Result<()> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    let header = serde_json::json!({ "name": b.name, "stamp": stamp }).to_string();
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    let u32s = |out: &mut Vec<u8>, v: &[u32]| {
        for x in v {
            out.extend_from_slice(&x.to_le_bytes());
        }
    };
    let f32s = |out: &mut Vec<u8>, v: &[f32]| {
        for x in v {
            out.extend_from_slice(&x.to_le_bytes());
        }
    };
    out.extend_from_slice(&(b.geometries.len() as u32).to_le_bytes());
    for g in &b.geometries {
        out.extend_from_slice(&g.key.to_le_bytes());
        u32s(&mut out, &[g.flags, g.words.len() as u32, g.indices.len() as u32]);
        u32s(&mut out, &g.words);
        u32s(&mut out, &g.indices);
    }
    out.extend_from_slice(&(b.instances.len() as u32).to_le_bytes());
    for i in &b.instances {
        out.extend_from_slice(&i.geo.to_le_bytes());
        u32s(&mut out, &[i.material.game_material, i.material.lightmap.unwrap_or(u32::MAX), i.material.mirrored as u32]);
        let t = i.transform.transpose();
        f32s(&mut out, &t.x_axis.to_array());
        f32s(&mut out, &t.y_axis.to_array());
        f32s(&mut out, &t.z_axis.to_array());
        f32s(&mut out, &i.local_min.to_array());
        f32s(&mut out, &i.local_max.to_array());
        u32s(&mut out, &[i.range.is_some() as u32]);
        f32s(&mut out, &i.range.unwrap_or_default());
        u32s(&mut out, &[i.casts as u32]);
        out.extend_from_slice(&i.tag.to_le_bytes());
    }
    out.extend_from_slice(&(b.extra.len() as u32).to_le_bytes());
    u32s(&mut out, &b.extra);
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file_name(&b.name));
    let tmp = path.with_extension("bake.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(tmp, path)
}

/// Reads a bundle file; None when missing, damaged, another version, or made for another install (`stamp`).
pub fn read(path: &std::path::Path, stamp: &Stamp) -> Option<Bundle<MatKey>> {
    let data = std::fs::read(path).ok()?;
    let mut c = Cursor { d: &data, at: 0 };
    if c.bytes(8)? != MAGIC || c.u32()? != VERSION {
        return None;
    }
    let hl = c.u32()? as usize;
    let header: serde_json::Value = serde_json::from_slice(c.bytes(hl)?).ok()?;
    let file_stamp: Stamp = serde_json::from_value(header["stamp"].clone()).ok()?;
    if &file_stamp != stamp {
        return None;
    }
    let mut b = Bundle { name: header["name"].as_str()?.to_owned(), ..default() };
    for _ in 0..c.u32()? {
        let key = c.u64()?;
        let (flags, nw, ni) = (c.u32()?, c.u32()? as usize, c.u32()? as usize);
        if nw % VERTEX_WORDS != 0 {
            return None;
        }
        let words = c.u32s(nw)?;
        let indices = c.u32s(ni)?;
        b.geometries.push(BakedGeo { key, flags, words, indices });
    }
    for _ in 0..c.u32()? {
        let geo = c.u64()?;
        let (game_material, lightmap, mirrored) = (c.u32()?, c.u32()?, c.u32()?);
        let r = c.f32s(12)?;
        let t = Mat4::from_cols(Vec4::new(r[0], r[1], r[2], r[3]), Vec4::new(r[4], r[5], r[6], r[7]), Vec4::new(r[8], r[9], r[10], r[11]), Vec4::W).transpose();
        let lo = c.f32s(3)?;
        let hi = c.f32s(3)?;
        let has_range = c.u32()? != 0;
        let range = c.f32s(4)?;
        let casts = c.u32()? != 0;
        let tag = c.u32()? as i32;
        b.instances.push(BakedInstance {
            geo,
            material: MatKey { game_material, lightmap: (lightmap != u32::MAX).then_some(lightmap), mirrored: mirrored != 0 },
            transform: t,
            local_min: Vec3::from_slice(&lo),
            local_max: Vec3::from_slice(&hi),
            range: has_range.then(|| [range[0], range[1], range[2], range[3]]),
            casts,
            tag,
        });
    }
    let n = c.u32()? as usize;
    b.extra = c.u32s(n)?;
    Some(b)
}

/// Whether `path` is a bundle of this version made for `stamp` (reads the header only; resumable bakes).
pub fn valid(path: &std::path::Path, stamp: &Stamp) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; 16];
    if f.read_exact(&mut head).is_err() || &head[..8] != MAGIC || u32::from_le_bytes([head[8], head[9], head[10], head[11]]) != VERSION {
        return false;
    }
    let hl = u32::from_le_bytes([head[12], head[13], head[14], head[15]]) as usize;
    let mut h = vec![0u8; hl];
    if f.read_exact(&mut h).is_err() {
        return false;
    }
    let Ok(header) = serde_json::from_slice::<serde_json::Value>(&h) else { return false };
    serde_json::from_value::<Stamp>(header["stamp"].clone()).is_ok_and(|s| &s == stamp)
}

struct Cursor<'a> {
    d: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.d.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
    fn u32s(&mut self, n: usize) -> Option<Vec<u32>> {
        let b = self.bytes(n.checked_mul(4)?)?;
        Some(b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }
    fn f32s(&mut self, n: usize) -> Option<Vec<f32>> {
        Some(self.u32s(n)?.into_iter().map(f32::from_bits).collect())
    }
}

// ---------------------------------------------------------------- runtime

/// A loaded bundle: the geometry it added and its record slots. Clones refer to the same records (unload one only).
#[derive(Debug, Default, Clone)]
pub struct BundleHandle {
    geometries: Vec<u64>,
    slots: Vec<u32>,
}

impl BundleHandle {
    pub fn instances(&self) -> usize {
        self.slots.len()
    }
}

/// Loads `b` into the static world: its geometry (once per key) and every instance whose geometry is present (its own, or
/// another loaded bundle's: the templates). Materials are asset ids already ([`Bundle::map_materials`] with the caller's
/// RemasterParams). Hidden starts as given (zone models load hidden until their zone shows).
pub fn load(b: Bundle<UntypedAssetId>, hidden: bool) -> BundleHandle {
    let mut h = BundleHandle::default();
    if !super::on() {
        return h;
    }
    for g in b.geometries {
        if super::add_geometry(GeoKey::Baked(g.key), g.words, g.indices, g.flags) {
            h.geometries.push(g.key);
        }
    }
    for i in b.instances {
        let range = i.range.map(|r| (r[0]..r[1], r[2]..r[3]));
        if let Some(slot) = super::add_instance_key(GeoKey::Baked(i.geo), i.material, i.transform, i.local_min, i.local_max, range, i.casts, i.tag) {
            if hidden {
                super::set_hidden_slot(slot, true);
            }
            h.slots.push(slot);
        }
    }
    h
}

/// Unloads a bundle: frees its records and the geometry it added (template geometry stays with the templates bundle).
pub fn unload(h: BundleHandle) {
    for slot in h.slots {
        super::push(super::Op::RemoveInstance(slot));
    }
    for key in h.geometries {
        super::remove_geometry(GeoKey::Baked(key));
    }
}

/// On a streaming parent entity (zone model, prop tile): unloads its bundle when the entity goes (retired tiles, zone
/// unloads, map switches despawning the world), so no path leaks records.
#[derive(Component, Debug)]
#[component(on_remove = unload_on_remove)]
pub struct BakedBundle(pub BundleHandle);

fn unload_on_remove(world: bevy::ecs::world::DeferredWorld, ctx: bevy::ecs::lifecycle::HookContext) {
    if let Some(b) = world.get::<BakedBundle>(ctx.entity) {
        unload(b.0.clone());
    }
}

/// Shows / hides every record of a bundle (zone switches).
pub fn set_hidden(h: &BundleHandle, hidden: bool) {
    for &slot in &h.slots {
        super::set_hidden_slot(slot, hidden);
    }
}

/// Zone fade dither level of every record of a bundle.
pub fn set_tag(h: &BundleHandle, level: i32) {
    for &slot in &h.slots {
        super::set_tag(slot, level);
    }
}
