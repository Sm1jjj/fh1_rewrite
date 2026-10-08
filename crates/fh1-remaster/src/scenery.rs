//! Remaster scenery/props (W1): game material id -> shared [`RemasterMaterial`] for the engine's zone and prop
//! batches (fh1-engine scenery.rs seam, `FH1_RENDERER=remaster`), and the CPU mesh preparation.
//!
//! - Tables: `<private>/remaster/scenery/<track>/materials.bin` next to `<private>/scenery/<track>` (setup group
//!   `remaster`). A track without a table (imported maps, group not installed) keeps the faithful path.
//! - Materials are shared per distinct record (class, texture set, constants) and mirroring, never per entity:
//!   13-16k game materials collapse to the distinct ones, all in one bindless pipeline family.
//! - Textures: the scenery group's DDS (game mips), uploaded as sRGB when the game fetches them with gamma (the
//!   `.bix` word / BC4-BC5 rule), so the hardware decodes them; one cache per track, never evicted (v1).
//! - Night: [`RemasterNight`] changes (steps of 1/16, dusk / dawn) go only to the materials whose output depends on them, a batch per frame (PERF P8-B).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::asset::AssetHandleProvider;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use crate::material::{self, Class, Record, RemasterMaterial, RemasterNight, SceneryExt, ROLE_A, ROLE_N, ROLE_NB};

/// What the engine does with a game material's batch.
pub enum RemasterBatch {
    /// Draw with this material.
    Material(Handle<RemasterMaterial>, Class),
    /// The class is not drawn by the remaster (e.g. `light_pollution`): drop the batch.
    Skip,
    /// No remaster table for this track or material: keep the faithful path.
    Faithful,
}

struct Track {
    records: Vec<Record>,
    scenery_dir: PathBuf,
}

/// Distinct-material key: the record's bytes that affect drawing, plus mirroring.
#[derive(PartialEq, Eq, Hash, Clone)]
struct MatKey {
    track: usize,
    class: Class,
    layering: u8,
    flags: u16,
    tex: [Option<u32>; 12],
    uv_set: [u8; 12],
    floats: Vec<u32>,
    mirrored: bool,
    /// Per-placement night lightmap (texture id), for FLAG_INSTANCE_LM materials.
    lm: Option<u32>,
}

#[derive(Resource)]
pub struct RemasterScenery {
    tracks: Vec<Track>,
    /// Scenery dir -> index into `tracks` (None = no table: faithful).
    by_dir: HashMap<PathBuf, Option<usize>>,
    materials: HashMap<MatKey, Handle<RemasterMaterial>>,
    /// Handle -> (track, record index, mirrored), to rebuild / mirror.
    origin: HashMap<AssetId<RemasterMaterial>, (usize, usize, bool, Option<u32>)>,
    images: HashMap<(usize, u32), Handle<Image>>,
    loading: Vec<(Handle<Image>, bool, usize, Task<Option<Image>>)>,
    provider: AssetHandleProvider,
    /// Night values last written into the materials.
    night: RemasterNight,
    /// Materials still to receive `night` (P8: a few per frame, only the ones whose output depends on it).
    night_queue: Vec<Handle<RemasterMaterial>>,
    pub loaded_textures: usize,
    /// Bytes of each resident texture (CPU-side data size = its VRAM, mips included).
    bytes: HashMap<AssetId<Image>, usize>,
    pub resident_bytes: usize,
    /// Cache entries only the cache still holds: id -> first seen idle (s). None = eviction off (`FH1_RM_EVICT=0`).
    idle: HashMap<bevy::asset::UntypedAssetId, f32>,
    last_sweep: Option<f32>,
}

impl RemasterScenery {
    fn new(provider: AssetHandleProvider) -> Self {
        Self {
            tracks: Vec::new(),
            by_dir: HashMap::new(),
            materials: HashMap::new(),
            origin: HashMap::new(),
            images: HashMap::new(),
            loading: Vec::new(),
            provider,
            night: RemasterNight::default(),
            night_queue: Vec::new(),
            loaded_textures: 0,
            bytes: HashMap::new(),
            resident_bytes: 0,
            idle: HashMap::new(),
            last_sweep: (!std::env::var("FH1_RM_EVICT").is_ok_and(|v| v == "0")).then_some(0.0),
        }
    }

    /// `<private>/<rel>` -> `<private>/remaster/<rel>` (Colorado `scenery/colorado`, imported maps
    /// `imported/<game>/<id>/scenery`), as fh1setup remaster.rs writes them.
    fn table_dir(scenery_dir: &Path) -> Option<PathBuf> {
        let mut private = scenery_dir;
        while private.file_name()? != "private" {
            private = private.parent()?;
        }
        Some(private.join("remaster").join(scenery_dir.strip_prefix(private).ok()?))
    }

    fn track(&mut self, scenery_dir: &Path) -> Option<usize> {
        if let Some(t) = self.by_dir.get(scenery_dir) {
            return *t;
        }
        let table = Self::table_dir(scenery_dir).and_then(|d| std::fs::read(d.join("materials.bin")).ok()).and_then(|b| material::parse_table(&b));
        let t = table.map(|records| {
            info!("remaster: {} material records for {}", records.len(), scenery_dir.display());
            self.tracks.push(Track { records, scenery_dir: scenery_dir.to_owned() });
            self.tracks.len() - 1
        });
        if t.is_none() {
            warn!("remaster: no material table for {} (fh1setup --only remaster); faithful path", scenery_dir.display());
        }
        self.by_dir.insert(scenery_dir.to_owned(), t);
        t
    }

    /// The DDS file behind a scenery texture handle (RTX proxies read a splat weight map's 1x1 mip, rtx.rs).
    pub fn image_path(&self, id: AssetId<Image>) -> Option<PathBuf> {
        let (&(track, tex), _) = self.images.iter().find(|(_, h)| h.id() == id)?;
        Some(self.tracks[track].scenery_dir.join("textures").join(format!("{tex:08x}.dds")))
    }

    fn image(&mut self, track: usize, id: u32, srgb: bool, role: usize) -> Handle<Image> {
        if let Some(h) = self.images.get(&(track, id)) {
            return h.clone();
        }
        let h: Handle<Image> = self.provider.reserve_handle().typed();
        let path = self.tracks[track].scenery_dir.join("textures").join(format!("{id:08x}.dds"));
        let task = AsyncComputeTaskPool::get().spawn(async move { fh1_render::scenery::read_dds(&path) });
        self.loading.push((h.clone(), srgb, role, task));
        self.images.insert((track, id), h.clone());
        h
    }

    fn build(&mut self, track: usize, index: usize, mirrored: bool, lm: Option<u32>, night: RemasterNight, materials: &mut Assets<RemasterMaterial>) -> Handle<RemasterMaterial> {
        let r = self.tracks[track].records[index].clone();
        let floats = r.uv_scale.iter().flatten().chain(r.params.iter().flatten()).map(|f| f.to_bits()).chain([r.srgb]).collect();
        let key = MatKey { track, class: r.class, layering: r.layering, flags: r.flags, tex: r.tex, uv_set: r.uv_set, floats, mirrored, lm };
        if let Some(h) = self.materials.get(&key) {
            return h.clone();
        }
        let mut ext = SceneryExt { params: material::uniform(&r, night), ..default() };
        let mut base = material::base(&r, mirrored);
        for (role, t) in r.tex.iter().enumerate() {
            if let Some(id) = t {
                let srgb = r.srgb & (1 << role) != 0;
                let h = self.image(track, *id, srgb, role);
                if role == ROLE_A && r.class == Class::Cutout {
                    base.base_color_texture = Some(h.clone());
                }
                *ext.slot_mut(role) = Some(h);
            }
        }
        if let Some(id) = lm {
            // Per-placement lightmaps: textures/<id>.dds of the same scenery folder; gamma by format (BC4/BC5 linear).
            let img = self.image(track, id, true, material::ROLE_LM);
            ext.lightmap = Some(img);
            ext.params.info.y |= 1 << material::ROLE_LM;
        }
        let h = materials.add(RemasterMaterial { base, extension: ext });
        self.origin.insert(h.id(), (track, index, mirrored, lm));
        self.materials.insert(key, h.clone());
        if self.materials.len() % 500 == 0 {
            info!("remaster: {} distinct scenery materials", self.materials.len());
        }
        h
    }

    /// The remaster material for a game material of the track in `scenery_dir` (engine scenery.rs seam).
    pub fn batch(&mut self, scenery_dir: &Path, game_material: u32, materials: &mut Assets<RemasterMaterial>) -> RemasterBatch {
        let Some(track) = self.track(scenery_dir) else { return RemasterBatch::Faithful };
        let Some(r) = self.tracks[track].records.get(game_material as usize) else { return RemasterBatch::Faithful };
        let class = r.class;
        if class == Class::Skip {
            return RemasterBatch::Skip;
        }
        let night = self.night;
        RemasterBatch::Material(self.build(track, game_material as usize, false, None, night, materials), class)
    }

    /// P12 static world: (class, double-sided) of a game material's record (None = no remaster record).
    pub fn record_info(&mut self, scenery_dir: &Path, game_material: u32) -> Option<(Class, bool)> {
        let track = self.track(scenery_dir)?;
        let r = self.tracks[track].records.get(game_material as usize)?;
        let two_sided = r.flags & material::FLAG_TWO_SIDED != 0 || r.class == Class::Water;
        Some((r.class, two_sided))
    }

    /// The same material with culling flipped, for mirrored placements (negative determinant).
    pub fn mirrored(&mut self, h: &Handle<RemasterMaterial>, materials: &mut Assets<RemasterMaterial>) -> Handle<RemasterMaterial> {
        let Some(&(track, index, m, lm)) = self.origin.get(&h.id()) else { return h.clone() };
        let night = self.night;
        self.build(track, index, !m, lm, night, materials)
    }

    /// `h` with a placement's own night lightmap (the `.pvs` record texture id, as fh1-render `lightmap_variant`) when
    /// its game material takes per-placement lightmaps (FLAG_INSTANCE_LM); `h` itself otherwise. `FH1_INST_LM=0` = off.
    pub fn lightmap_variant(&mut self, h: &Handle<RemasterMaterial>, lightmap: u32, materials: &mut Assets<RemasterMaterial>) -> Handle<RemasterMaterial> {
        let Some(&(track, index, m, _)) = self.origin.get(&h.id()) else { return h.clone() };
        if self.tracks[track].records[index].flags & material::FLAG_INSTANCE_LM == 0 || std::env::var("FH1_INST_LM").is_ok_and(|v| v == "0") {
            return h.clone();
        }
        let night = self.night;
        self.build(track, index, m, Some(lightmap), night, materials)
    }

    /// The material for a W4 merged batch (meshes already in world space; mirrored parts carry flipped winding).
    pub fn lookup(&mut self, scenery_dir: &Path, game_material: u32, materials: &mut Assets<RemasterMaterial>) -> Option<(Handle<RemasterMaterial>, Class)> {
        match self.batch(scenery_dir, game_material, materials) {
            RemasterBatch::Material(h, c) => Some((h, c)),
            _ => None,
        }
    }
}

/// The resources the engine's `stream` system needs (add as a field of its `FxParams`).
#[derive(SystemParam)]
pub struct RemasterParams<'w> {
    pub scenery: Option<ResMut<'w, RemasterScenery>>,
    pub materials: Option<ResMut<'w, Assets<RemasterMaterial>>>,
}

impl RemasterParams<'_> {
    /// See [`RemasterScenery::batch`]; `Faithful` when the remaster isn't running.
    pub fn batch(&mut self, scenery_dir: &Path, game_material: u32) -> RemasterBatch {
        match (self.scenery.as_mut(), self.materials.as_mut()) {
            (Some(s), Some(m)) if crate::enabled() => s.batch(scenery_dir, game_material, m),
            _ => RemasterBatch::Faithful,
        }
    }

    /// See [`RemasterScenery::record_info`].
    pub fn record_info(&mut self, scenery_dir: &Path, game_material: u32) -> Option<(Class, bool)> {
        self.scenery.as_mut().and_then(|s| s.record_info(scenery_dir, game_material))
    }

    pub fn mirrored(&mut self, h: &Handle<RemasterMaterial>) -> Handle<RemasterMaterial> {
        match (self.scenery.as_mut(), self.materials.as_mut()) {
            (Some(s), Some(m)) => s.mirrored(h, m),
            _ => h.clone(),
        }
    }

    /// See [`RemasterScenery::lightmap_variant`].
    pub fn lightmap_variant(&mut self, h: &Handle<RemasterMaterial>, lightmap: u32) -> Handle<RemasterMaterial> {
        match (self.scenery.as_mut(), self.materials.as_mut()) {
            (Some(s), Some(m)) => s.lightmap_variant(h, lightmap, m),
            _ => h.clone(),
        }
    }
}

/// The faithful tile mesh -> the attributes the remaster shader reads: position, normal, uv0-2, Fx_Color.
/// Tangents / binormals are dropped (normal maps use the screen-space cotangent frame). Missing uv0-2 / colour are
/// filled with the shader's defaults (0 / white) so every scenery mesh has ONE vertex layout: one mesh slab and one
/// pipeline bin per (alpha mode, cull) instead of up to 8 (draw count, W4).
pub fn prepare_mesh(mut mesh: Mesh) -> Mesh {
    use bevy::mesh::VertexAttributeValues as V;
    mesh.remove_attribute(fh1_render::material::ATTRIBUTE_TANGENT);
    mesh.remove_attribute(fh1_render::material::ATTRIBUTE_BINORMAL);
    mesh.remove_attribute(fh1_render::material::ATTRIBUTE_UV3);
    let n = mesh.count_vertices();
    for uv in [Mesh::ATTRIBUTE_UV_0, Mesh::ATTRIBUTE_UV_1, fh1_render::material::ATTRIBUTE_UV2] {
        if !mesh.contains_attribute(uv.id) {
            mesh.insert_attribute(uv, V::Float32x2(vec![[0.0; 2]; n]));
        }
    }
    if !mesh.contains_attribute(fh1_render::material::ATTRIBUTE_COLOR.id) {
        mesh.insert_attribute(fh1_render::material::ATTRIBUTE_COLOR, V::Unorm8x4(vec![[255; 4]; n]));
    }
    mesh
}

fn srgb_format(f: TextureFormat) -> TextureFormat {
    match f {
        TextureFormat::Bc1RgbaUnorm => TextureFormat::Bc1RgbaUnormSrgb,
        TextureFormat::Bc2RgbaUnorm => TextureFormat::Bc2RgbaUnormSrgb,
        TextureFormat::Bc3RgbaUnorm => TextureFormat::Bc3RgbaUnormSrgb,
        TextureFormat::Rgba8Unorm => TextureFormat::Rgba8UnormSrgb,
        other => other,
    }
}

/// 1x1 stand-in for a texture that failed to load (so the material still draws).
fn fallback(role: usize) -> Image {
    let px = match role {
        ROLE_A => [128, 128, 128, 255],
        ROLE_N | ROLE_NB => [128, 128, 255, 255],
        _ => [255, 255, 255, 255],
    };
    Image::new_fill(Extent3d { width: 1, height: 1, depth_or_array_layers: 1 }, TextureDimension::D2, &px, TextureFormat::Rgba8Unorm, default())
}

/// Display-linear scale of the night lightmaps (emissive exposure weight 0: the game's own value, 1 = as the game).
/// `FH1_RM_EMISSIVE_NITS` overrides (name kept).
fn emissive_nits() -> f32 {
    std::env::var("FH1_RM_EMISSIVE_NITS").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0)
}

/// Lamp emissive maps, display-linear (x3 so lamp heads cross 1.0 and bloom; c3). `FH1_RM_LAMP_NITS` overrides.
fn lamp_nits() -> f32 {
    std::env::var("FH1_RM_LAMP_NITS").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0)
}

/// [`RemasterNight`] from W3's lighting state: the game's c213.y = saturate((L - 0.5) x 4) (L < 0.05 -> off) and L itself.
fn night_from_lighting(lighting: Option<Res<crate::light::RemasterLighting>>, mut night: ResMut<RemasterNight>) {
    let Some(l) = lighting else { return };
    let on = l.lights_on;
    let lm = if on < 0.05 { 0.0 } else { ((on - 0.5) * 4.0).clamp(0.0, 1.0) };
    // x game_unit_scale: c3's tone curve input calibration, so game-unit emission lands where faithful puts it.
    let k = crate::post::game_unit_scale();
    let want = RemasterNight { lightmap: lm, switch_on: on, emissive_scale: emissive_nits() * k, lamp_scale: lamp_nits() * k };
    if *night != want {
        *night = want;
    }
}

/// Insert finished textures; apply night changes.
fn poll(mut sc: ResMut<RemasterScenery>, mut images: ResMut<Assets<Image>>, night: Res<RemasterNight>, mut materials: ResMut<Assets<RemasterMaterial>>, time: Res<Time<Real>>) {
    let sc = &mut *sc;
    sc.maintain(time.elapsed_secs());
    let mut i = 0;
    while i < sc.loading.len() {
        if !sc.loading[i].3.is_finished() {
            i += 1;
            continue;
        }
        let (h, srgb, role, task) = sc.loading.swap_remove(i);
        let img = match block_on(future::poll_once(task)).flatten() {
            Some(mut img) => {
                // Cube maps aren't bound by the remaster (env light comes from W3).
                if img.texture_descriptor.size.depth_or_array_layers != 1 {
                    fallback(role)
                } else {
                    if srgb {
                        img.texture_descriptor.format = srgb_format(img.texture_descriptor.format);
                    }
                    img
                }
            }
            None => fallback(role),
        };
        let n = img.data.as_ref().map_or(0, Vec::len);
        sc.bytes.insert(h.id(), n);
        sc.resident_bytes += n;
        let _ = images.insert(&h, img);
        sc.loaded_textures += 1;
        if sc.loaded_textures % 500 == 1 {
            info!("remaster: {} scenery textures loaded, {} resident ({} MB)", sc.loaded_textures, sc.images.len(), sc.resident_bytes >> 20);
        }
    }
    // Night: quantised to 1/16 so a TOD sweep changes the values ~16 times per dusk, not every frame.
    let q = |v: f32| (v * 16.0).round() / 16.0;
    let want = RemasterNight { lightmap: q(night.lightmap), switch_on: q(night.switch_on), ..*night };
    let n = Vec4::new(want.lightmap, want.switch_on, want.emissive_scale, want.lamp_scale);
    if want != sc.night {
        sc.night = want;
        if night_all() {
            // Old: every material, at once (P8: ~13k bindless re-prepares = a 90 ms PrepareAssets hitch per step).
            for h in sc.materials.values() {
                if let Some(mut m) = materials.get_mut(h) {
                    m.extension.params.night = n;
                }
            }
        } else {
            // Only the materials whose shader output changes (night lightmap / switched emissive); `get` doesn't mark
            // the asset changed. Applied a batch per frame below, always with the latest value.
            sc.night_queue = sc.materials.values().filter(|h| materials.get(*h).is_some_and(|m| night_matters(&m.extension.params, n))).cloned().collect();
        }
    }
    let per_frame = night_batch();
    for _ in 0..per_frame {
        let Some(h) = sc.night_queue.pop() else { break };
        if let Some(mut m) = materials.get_mut(&h) {
            m.extension.params.night = n;
        }
    }
}

/// `FH1_RM_NIGHT_ALL=1`: the old night update (every material in one frame).
fn night_all() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_RM_NIGHT_ALL").is_ok_and(|v| v == "1"))
}

/// Night material updates per frame (`FH1_RM_NIGHT_BATCH`, default 256).
fn night_batch() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_RM_NIGHT_BATCH").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(256))
}

/// Does the scenery shader's output change from `p.night` to `n`? (material.rs fragment, "Night": role 8 = night
/// lightmap unless FLAG_LM_BAKED, weight max(night.x, p3.x), scale night.z; role 9 + FLAG_NIGHT_EMISSIVE = emissive
/// when night.y >= 0.5, scale night.w. Everything else ignores `night`.)
fn night_matters(p: &material::SceneryUniform, n: Vec4) -> bool {
    let o = p.night;
    let flags = (p.info.z & 0xFFFF) as u16;
    let lm = p.info.y & (1 << 8) != 0 && flags & material::FLAG_LM_BAKED == 0;
    let em = p.info.y & (1 << 9) != 0 && flags & material::FLAG_NIGHT_EMISSIVE != 0;
    let floor = p.p[3].x;
    let lm_changed = o.x.max(floor) != n.x.max(floor) || ((o.x.max(floor) > 0.0 || n.x.max(floor) > 0.0) && o.z != n.z);
    let em_on = |v: Vec4| v.y >= 0.5;
    let em_changed = em_on(o) != em_on(n) || (em_on(n) && o.w != n.w);
    (lm && lm_changed) || (em && em_changed)
}

/// Seconds between cache sweeps, and how long an entry only the cache holds is kept (tile-boundary churn); the
/// faithful cache's values (fh1-render scenery.rs).
const SWEEP_EVERY: f32 = 5.0;
const SWEEP_IDLE: f32 = 30.0;

/// Keep everything resident while under these budgets (2026-10-08 perf: VRAM ~3.7 of 12 GB, and every evicted texture was
/// read and decoded again on a revisit, every evicted material re-specialised). Textures: FH1_RM_TEX_BUDGET_MB (default
/// 4096 MB of BCn data), materials: FH1_RM_MAT_KEEP (default 24000; Colorado has 16,149 records). Over budget the idle
/// rule above applies. FH1_RM_KEEP=0 = the old idle-only rule.
fn keep_budgets() -> Option<(usize, usize)> {
    if std::env::var("FH1_RM_KEEP").is_ok_and(|v| v == "0") {
        return None;
    }
    let var = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    Some((var("FH1_RM_TEX_BUDGET_MB", 4096) << 20, var("FH1_RM_MAT_KEEP", 24000)))
}

impl RemasterScenery {
    /// Every SWEEP_EVERY s: drop materials no entity uses any more (their textures lose their last user with them)
    /// and textures no material uses, once idle for SWEEP_IDLE s. A later request rebuilds / reloads them. Same rule
    /// as the faithful `SceneryMaterials::sweep`, so the resident set follows the streaming window.
    fn maintain(&mut self, now: f32) {
        let Some(last) = self.last_sweep else { return };
        if now - last < SWEEP_EVERY {
            return;
        }
        self.last_sweep = Some(now);
        fn only_cache<A: Asset>(h: &Handle<A>) -> bool {
            matches!(h, Handle::Strong(a) if std::sync::Arc::strong_count(a) == 1)
        }
        let budgets = keep_budgets();
        let keep_materials = budgets.is_some_and(|(_, n)| self.materials.len() <= n);
        let keep_textures = budgets.is_some_and(|(b, _)| self.resident_bytes <= b);
        if keep_materials && keep_textures {
            return;
        }
        let old = std::mem::take(&mut self.idle);
        let mut idle = HashMap::new();
        let mut evict = |id: bevy::asset::UntypedAssetId, free: bool| -> bool {
            if !free {
                return false;
            }
            let since = old.get(&id).copied().unwrap_or(now);
            if now - since >= SWEEP_IDLE {
                return true;
            }
            idle.insert(id, since);
            false
        };
        let mut gone_m = Vec::new();
        self.materials.retain(|_, h| {
            if keep_materials {
                return true;
            }
            let e = evict(h.id().untyped(), only_cache(h));
            if e {
                gone_m.push(h.id());
            }
            !e
        });
        let mut gone_t = 0;
        let (bytes, mut freed) = (&mut self.bytes, 0usize);
        self.images.retain(|_, h| {
            if keep_textures {
                return true;
            }
            let e = evict(h.id().untyped(), only_cache(h));
            if e {
                gone_t += 1;
                freed += bytes.remove(&h.id()).unwrap_or(0);
            }
            !e
        });
        for id in &gone_m {
            self.origin.remove(id);
        }
        self.resident_bytes = self.resident_bytes.saturating_sub(freed);
        self.idle = idle;
        if !gone_m.is_empty() || gone_t > 0 {
            info!(
                "remaster: evicted {} materials, {} textures ({} MB); resident {} materials, {} textures, {} MB",
                gone_m.len(),
                gone_t,
                freed >> 20,
                self.materials.len(),
                self.images.len(),
                self.resident_bytes >> 20
            );
        }
    }
}

/// Remaster mode skips fh1-render's FxShadowPlugin, whose `skip_non_casters` kept the sky and every blended FxMaterial
/// (anim-object light beams, glass, backdrop decals) out of the shadows; Bevy's CSM now draws whatever casts, so the
/// same rule is applied here.
fn fx_no_shadow(
    mut commands: Commands,
    new: Query<(Entity, &MeshMaterial3d<fh1_render::FxMaterial>, Has<fh1_render::sky::SkyPart>), (Added<MeshMaterial3d<fh1_render::FxMaterial>>, Without<bevy::light::NotShadowCaster>)>,
    materials: Res<Assets<fh1_render::FxMaterial>>,
) {
    for (e, m, sky) in &new {
        if sky || materials.get(&m.0).is_some_and(|m| m.alpha_blend) {
            commands.entity(e).try_insert((bevy::light::NotShadowCaster, bevy::light::NotShadowReceiver));
        }
    }
}

/// Debug: `FH1_RM_STATS=1` logs every 5 s what the remaster scenery puts in front of the renderer (the twin of
/// faithful's `FH1_FX_STATS`, which only runs in faithful mode): visible entities, distinct materials / meshes, the
/// pipeline-relevant split (alpha mode x cull), FxMaterial entities still drawn in remaster mode, all visible Mesh3d.
#[allow(clippy::type_complexity)]
fn rm_stats(
    rm: Query<(&MeshMaterial3d<RemasterMaterial>, &Mesh3d, &ViewVisibility)>,
    fx: Query<&ViewVisibility, With<MeshMaterial3d<fh1_render::FxMaterial>>>,
    all: Query<&ViewVisibility, With<Mesh3d>>,
    materials: Res<Assets<RemasterMaterial>>,
    sc: Option<Res<RemasterScenery>>,
    time: Res<Time<Real>>,
    mut last: Local<f32>,
) {
    if !std::env::var("FH1_RM_STATS").is_ok_and(|v| v == "1") || time.elapsed_secs() - *last < 5.0 {
        return;
    }
    *last = time.elapsed_secs();
    let (mut total, mut vis) = (0usize, 0usize);
    let (mut mats, mut meshes) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let mut bins: std::collections::BTreeMap<String, usize> = Default::default();
    for (m, mesh, v) in &rm {
        total += 1;
        if !v.get() {
            continue;
        }
        vis += 1;
        mats.insert(m.0.id());
        meshes.insert(mesh.0.id());
        if let Some(mat) = materials.get(&m.0) {
            *bins.entry(format!("{:?}/{:?}", mat.base.alpha_mode, mat.base.cull_mode)).or_default() += 1;
        }
    }
    let fx_vis = fx.iter().filter(|v| v.get()).count();
    let all_vis = all.iter().filter(|v| v.get()).count();
    info!(
        "rm stats: {total} remaster entities, {vis} visible using {} materials / {} meshes; bins {bins:?}; FxMaterial visible {fx_vis}; all Mesh3d visible {all_vis}; cache {} materials, {} textures, {} MB",
        mats.len(),
        meshes.len(),
        sc.as_ref().map_or(0, |s| s.materials.len()),
        sc.as_ref().map_or(0, |s| s.images.len()),
        sc.as_ref().map_or(0, |s| s.resident_bytes >> 20),
    );
}

fn init(mut commands: Commands, images: Res<Assets<Image>>) {
    commands.insert_resource(RemasterScenery::new(images.get_handle_provider()));
}

/// Registers the material, the scenery resource and its systems (only does work under `FH1_RENDERER=remaster`).
pub fn plugin(app: &mut App) {
    material::plugin(app);
    app.add_systems(Startup, init).add_systems(Update, (night_from_lighting, poll.run_if(resource_exists::<RemasterScenery>)).chain())
        .add_systems(Update, (fx_no_shadow, rm_stats));
}
