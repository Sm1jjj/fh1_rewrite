//! Streams Colorado's visual scenery tiles (`scenery/colorado`, written by fh1setup) around the car.
//! Tiles and their diffuse textures load on background threads; tiles unload once they're far behind.
//! Textures stay resident. Batches draw with the game's own shaders (fh1-render `FxMaterial`, see
//! docs/SHADERS.md); the simple StandardMaterial path (diffuse only) remains for stand-ins and as a
//! fallback when the shaders group isn't installed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use fh1_render::car_material::{FxRawOutput, FxRawStandard};
use fh1_render::scenery::SceneryMaterials;
use fh1_render::{FxGlobals, FxLibrary, FxMaterial};

use crate::Car;

mod p2;
pub use p2::{P2Plugin, PropMesh, ZoneMesh};

/// Load tiles whose centre is within this distance of the car; unload past it plus a margin.
const LOAD_RADIUS: f32 = 1800.0;
const UNLOAD_MARGIN: f32 = 400.0;
/// New tile loads started per frame (keeps the first frames responsive).
const STARTS_PER_FRAME: usize = 8;
/// Key of the distant backdrop (`far.bin`: mid-distance + uber-LOD terrain), loaded once, never unloaded.
const FAR: (i32, i32) = (i32::MAX, i32::MAX);
/// FH1TILE4 batch flag: stand-in geometry drawn without its (placeholder) texture.
const STANDIN: u32 = 1;
/// FH1TILE4 attribute bits (see fh1setup `scenery.rs`).
const ATTR_TANGENT: u32 = 1;
const ATTR_UV0: u32 = 2;
const ATTR_COLOR: u32 = 16;

/// What this renderer uses of a game material.
struct MaterialInfo {
    /// Slot-0 texture: (id, file relative to `dir`, has cut-out alpha).
    diffuse: Option<(u32, String, bool)>,
    /// Vertex-blend decal / opacity shaders (h_vblnd_decal*, *_opac*) fade by vertex alpha.
    blend: bool,
    /// The game shader (for the fallback log).
    shader: String,
}

/// One drawable batch of a tile: game material, FH1TILE4 flags, mesh.
type Batch = (u32, u32, Mesh);

/// A tile as loaded: game-shader batches (fh1-render) or the simple path's batches.
enum TileData {
    Fx(Vec<fh1_render::scenery::TileBatch>),
    Simple(Vec<Batch>),
}

#[derive(Resource)]
pub struct Scenery {
    /// True for FH1's Colorado; false for another Horizon-engine track (FH2's Anthem, docs/FH2_RECON.md), where the
    /// Colorado-only systems (grass, crowds, animated objects, races) stay off.
    pub colorado: bool,
    dir: PathBuf,
    tile_size: f32,
    /// Tile key -> file (relative to `dir`).
    files: HashMap<(i32, i32), String>,
    /// The distant backdrop file, when installed.
    far: Option<String>,
    props: Props,
    game_materials: Vec<MaterialInfo>,
    loaded: HashMap<(i32, i32), Entity>,
    pending: HashMap<(i32, i32), Task<Option<TileData>>>,
    /// Game materials through the game's shaders (None: shaders group not installed).
    fx: Option<SceneryMaterials>,
    /// (diffuse texture id, blend) -> material.
    materials: HashMap<(Option<u32>, bool), Handle<StandardMaterial>>,
    /// Texture images still being read, for these materials (white until then).
    pending_images: Vec<(Handle<StandardMaterial>, Task<Option<Image>>)>,
    textures_loaded: usize,
    /// Game shaders already reported as falling back to the simple path.
    fallback_logged: std::collections::HashSet<String>,
    /// StandardMaterial -> its FxRawStandard copy, used while the FH1 post chain is on (`FxLibrary::raw_output`):
    /// plain StandardMaterial output would be squared twice by the post chain.
    raw: HashMap<AssetId<StandardMaterial>, Handle<FxRawStandard>>,
    /// The game's PVS zones (index.json `zones`): when installed they replace the tile + far streaming.
    zones: Option<Zones>,
    /// PERF P2 diagnostics and A/B (p2.rs).
    p2: p2::P2,
    /// Streaming budgets (P5b): entities still allowed to spawn this frame, and roots waiting to be despawned
    /// (hidden at once, despawned a few thousand entities per frame) with their entity counts.
    budget: usize,
    /// End of this frame's streaming time ([`stream_ms`]); None = no time budget.
    deadline: Option<std::time::Instant>,
    retire: std::collections::VecDeque<(Entity, usize)>,
    /// P8: scenery mesh -> its bounds, computed once when prepared ([`Scenery::static_bounds`]).
    aabbs: HashMap<AssetId<Mesh>, bevy::camera::primitives::Aabb>,
}

/// Entities streaming may spawn per frame, and despawn per frame (P5b). A whole prop tile (hundreds of placements x LODs
/// x batches) or a 2 s zone sweep went into one frame: the user's drive showed 130-270 ms hitches. `FH1_STREAM_BUDGET=0`
/// = unlimited (old); `=ab` switches every 20 s for a one-run A/B.
const SPAWN_BUDGET: usize = 1500;
const DESPAWN_BUDGET: usize = 3000;

fn stream_budget_on(now: f64) -> bool {
    static MODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    match MODE.get_or_init(|| std::env::var("FH1_STREAM_BUDGET").unwrap_or_default()).as_str() {
        "0" => false,
        "ab" => (now / 20.0) as u64 % 2 == 0,
        _ => true,
    }
}

/// Main-thread time per frame for placing props and finishing zone models (2026-10-08 perf: `stream` took 6-10 ms of
/// main thread while streaming; the entity budget let 1,500 spawns through per frame). FH1_STREAM_MS (default 3 ms;
/// 0 = entity budgets only, as before). Each still makes some progress per frame so neither starves.
fn stream_ms() -> f32 {
    static MS: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *MS.get_or_init(|| std::env::var("FH1_STREAM_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0))
}

/// Placements per frame that ignore the time budget (progress floor).
const MIN_PLACE: usize = 64;

impl Scenery {
    fn over_time(&self) -> bool {
        self.deadline.is_some_and(|d| std::time::Instant::now() >= d)
    }

    /// Unload a streamed root: hidden now, despawned within the per-frame budget (or at once with budgets off).
    fn retire(&mut self, commands: &mut Commands, e: Entity, entities: usize, budget_on: bool) {
        if budget_on {
            commands.entity(e).insert(Visibility::Hidden);
            self.retire.push_back((e, entities.max(1)));
        } else {
            commands.entity(e).despawn();
        }
    }

    fn drain_retired(&mut self, commands: &mut Commands, budget_on: bool) {
        let mut left = if budget_on { DESPAWN_BUDGET } else { usize::MAX };
        while let Some(&(e, n)) = self.retire.front() {
            if n > left && left < DESPAWN_BUDGET {
                break;
            }
            self.retire.pop_front();
            commands.entity(e).despawn();
            left = left.saturating_sub(n);
        }
    }
}

/// One model drawn through the zones.
struct ZoneModel {
    file: String,
    lod: u32,
    group: usize,
    /// Casts sun shadows (.pvs flag; missing in older installs = casts).
    shadow: bool,
    /// The game's LOD slot range from the `.pvsz` entries (fh1setup `write_zones`): draws while the 2D distance to
    /// `bounds` is in [start, end); `culls` = the end is the object's cull (else a switch to a coarser LOD).
    range: Option<LodRange>,
    /// Own bounds (x, z) min / max.
    bounds: ([f32; 2], [f32; 2]),
}

/// A loaded zone model: its parent entity, mesh entities, and its fade state (P3, `zone_fade_s`).
struct ZoneLoaded {
    parent: Entity,
    children: Vec<Entity>,
    /// Whether the parent is shown (Visibility::Inherited), and the fade (0 = gone, 1 = fully drawn).
    visible: bool,
    alpha: f32,
    /// Dither level last written to the children's MeshTag.
    level: i32,
    /// The model's meshes and materials, parked on unload (`Zones::park`).
    parts: Vec<(Handle<Mesh>, BatchMaterial)>,
}

/// Unloaded zone models kept ready to respawn (2026-10-08 perf: a revisit re-read, re-parsed and re-uploaded every model;
/// RAM 2.2 / 32 GB and VRAM ~3.7 / 12 GB left plenty of room). Up to FH1_ZONE_PARK models (default 6000), least recently
/// unloaded dropped first; 0 = off (old: re-read from disk).
fn zone_park_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| std::env::var("FH1_ZONE_PARK").ok().and_then(|v| v.parse().ok()).unwrap_or(6000))
}

#[derive(Clone, Copy)]
struct LodRange {
    start: f32,
    end: f32,
    culls: bool,
}

/// PVS zone streaming (docs/WORLD_LOD.md): the car's 100 m hex cell picks a zone; the zone's list (from its
/// `.pvsz`) is exactly the set of models the game draws there, near detail, MIDDIST and UberLOD alike. When a
/// zone lists several LOD models of one object, the LOD is picked by 2D distance to the object's bounds with
/// switch distances derived from the zones at setup.
struct Zones {
    size: f32,
    origin: [f32; 2],
    cols: i32,
    rows: i32,
    cells: Vec<u32>,
    lists: Vec<Vec<u16>>,
    models: HashMap<u16, ZoneModel>,
    /// Per group: union bounds (x, z) and the LOD <= k / > k switch distances.
    groups: Vec<([f32; 2], [f32; 2], [f32; 4])>,
    /// Zone the car is in (its models load first), and the zone whose set is on screen (switches once loaded).
    current: Option<usize>,
    shown: Option<usize>,
    /// Shown zone: group -> bitmask of the LOD levels it lists.
    shown_lods: HashMap<usize, u32>,
    loaded: HashMap<u16, ZoneLoaded>,
    pending: HashMap<u16, Task<Option<TileData>>>,
    /// Zone fades (P3) start once the first zone is on screen (the start-up set appears at once).
    fade_ready: bool,
    /// Pop-in (P2): zones near the car and ahead of it, nearest first, whose models load (hidden) before the car
    /// gets there, so a zone switch finds its models loaded. Refreshed every few frames.
    preload: Vec<usize>,
    /// Pop-in (P4): models listed by zones within `zone_union_radius` (not by the shown zone) whose LOD slot band holds
    /// the car (plus `ZONE_UNION_MARGIN`), drawn by distance like the shown zone's. Refreshed with `preload`.
    extra: std::collections::HashSet<u16>,
    /// Smoothed car velocity (m/s, x/z), the last position (look-ahead), frames seen and the last unload sweep (s).
    vel: Vec2,
    last_here: Option<Vec2>,
    frames: u32,
    swept: f64,
    /// FH1_ZONE_STATS=1: per-second frame-time / streaming log, and zone switch latencies.
    stats: Option<ZoneStats>,
    /// Parked models ([`zone_park_cap`]): parts and the park order.
    parked: HashMap<u16, (Vec<(Handle<Mesh>, BatchMaterial)>, u64)>,
    park_tick: u64,
}

#[derive(Default)]
struct ZoneStats {
    /// Window start (s), frames, summed and worst frame time (s) in the window.
    window: f64,
    frames: u32,
    sum: f32,
    worst: f32,
    /// When the car entered the zone that isn't on screen yet.
    entered: Option<f64>,
}

impl Zones {
    fn load(dir: &Path, v: &serde_json::Value) -> Option<Self> {
        let b = std::fs::read(dir.join(v["file"].as_str()?)).ok()?;
        if b.get(..8)? != b"FH1ZONE1" {
            return None;
        }
        let mut o = 8;
        let mut u32_at = || -> Option<u32> {
            let x = u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?);
            o += 4;
            Some(x)
        };
        let (size, ox, oz) = (f32::from_bits(u32_at()?), f32::from_bits(u32_at()?), f32::from_bits(u32_at()?));
        let (cols, rows) = (u32_at()?, u32_at()?);
        let cells = (0..cols * rows).map(|_| u32_at()).collect::<Option<Vec<_>>>()?;
        let n = u32_at()?;
        let lists = (0..n)
            .map(|_| {
                let k = u32_at()?;
                (0..k).map(|_| u32_at().map(|m| m as u16)).collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()?;
        let mut groups: Vec<([f32; 2], [f32; 2], [f32; 4])> = v["groups"]
            .as_array()?
            .iter()
            .map(|g| ([f32::MAX; 2], [f32::MIN; 2], std::array::from_fn(|k| g[k].as_f64().unwrap_or(1e6) as f32)))
            .collect();
        let mut models = HashMap::new();
        for m in v["models"].as_array()? {
            let (n, group) = (m["n"].as_u64()? as u16, m["group"].as_u64()? as usize);
            let (min, max) = (&m["min"], &m["max"]);
            let g = groups.get_mut(group)?;
            g.0 = [g.0[0].min(min[0].as_f64()? as f32), g.0[1].min(min[2].as_f64()? as f32)];
            g.1 = [g.1[0].max(max[0].as_f64()? as f32), g.1[1].max(max[2].as_f64()? as f32)];
            let range = m["range"].as_array().and_then(|r| {
                Some(LodRange { start: r.first()?.as_f64()? as f32, end: r.get(1)?.as_f64().map_or(f32::INFINITY, |e| e as f32), culls: r.get(2)?.as_bool()? })
            });
            let bounds = ([min[0].as_f64()? as f32, min[2].as_f64()? as f32], [max[0].as_f64()? as f32, max[2].as_f64()? as f32]);
            models.insert(n, ZoneModel { file: m["file"].as_str()?.to_owned(), lod: m["lod"].as_u64()? as u32, group, shadow: m["shadow"].as_bool().unwrap_or(true), range, bounds });
        }
        Some(Self {
            size,
            origin: [ox, oz],
            cols: cols as i32,
            rows: rows as i32,
            cells,
            lists,
            models,
            groups,
            current: None,
            shown: None,
            shown_lods: HashMap::new(),
            loaded: HashMap::new(),
            pending: HashMap::new(),
            fade_ready: false,
            preload: Vec::new(),
            extra: std::collections::HashSet::new(),
            vel: Vec2::ZERO,
            last_here: None,
            frames: 0,
            swept: 0.0,
            stats: std::env::var_os("FH1_ZONE_STATS").map(|_| ZoneStats::default()),
            parked: HashMap::new(),
            park_tick: 0,
        })
    }

    /// Keep an unloaded model's parts for a later respawn (dropping the oldest parked ones over the cap).
    fn park(&mut self, n: u16, parts: Vec<(Handle<Mesh>, BatchMaterial)>) {
        let cap = zone_park_cap();
        if cap == 0 || parts.is_empty() {
            return;
        }
        self.park_tick += 1;
        self.parked.insert(n, (parts, self.park_tick));
        if self.parked.len() > cap {
            // Drop the oldest tenth at once (one scan per batch, not per model).
            let mut ticks: Vec<u64> = self.parked.values().map(|p| p.1).collect();
            ticks.sort_unstable();
            let cut = ticks[(self.parked.len() - cap + cap / 10).min(ticks.len() - 1)];
            self.parked.retain(|_, p| p.1 > cut);
        }
    }

    /// Engine-space centre of hex cell (col, row) (same formula as fh1setup `Zones::centre`).
    fn centre(&self, col: i32, row: i32) -> Vec2 {
        let w = self.size * 3f32.sqrt();
        let x = self.origin[0] + col as f32 * self.size * 1.5 + self.size;
        let z = self.origin[1] + row as f32 * w + w * 0.5 + if col % 2 == 1 { w * 0.5 } else { 0.0 };
        Vec2::new(x, -z)
    }

    /// The zone of the nearest hex cell that has one (off the road network: the nearest zone cell, up to ~1.5 km).
    fn zone_at(&self, p: Vec2) -> Option<usize> {
        let w = self.size * 3f32.sqrt();
        let col = ((p.x - self.origin[0] - self.size) / (self.size * 1.5)).round() as i32;
        let row = ((-p.y - self.origin[1] - w * 0.5) / w).round() as i32;
        for reach in [1, 4, 10] {
            let mut best: Option<(f32, usize)> = None;
            for c in col - reach..=col + reach {
                for r in row - reach..=row + reach {
                    if c < 0 || r < 0 || c >= self.cols || r >= self.rows {
                        continue;
                    }
                    let z = self.cells[(r * self.cols + c) as usize];
                    if z == u32::MAX || z as usize >= self.lists.len() {
                        continue;
                    }
                    let d = self.centre(c, r).distance_squared(p);
                    if best.is_none_or(|b| d < b.0) {
                        best = Some((d, z as usize));
                    }
                }
            }
            if let Some((_, z)) = best {
                return Some(z);
            }
        }
        None
    }

    /// Zones of the hex cells within `r` of `p`, with their distance.
    fn zones_near(&self, p: Vec2, r: f32, out: &mut Vec<(usize, f32)>) {
        let w = self.size * 3f32.sqrt();
        let col = ((p.x - self.origin[0] - self.size) / (self.size * 1.5)).round() as i32;
        let row = ((-p.y - self.origin[1] - w * 0.5) / w).round() as i32;
        let (dc, dr) = ((r / (self.size * 1.5)).ceil() as i32 + 1, (r / w).ceil() as i32 + 1);
        for c in (col - dc).max(0)..=(col + dc).min(self.cols - 1) {
            for rw in (row - dr).max(0)..=(row + dr).min(self.rows - 1) {
                let z = self.cells[(rw * self.cols + c) as usize];
                if z == u32::MAX || z as usize >= self.lists.len() {
                    continue;
                }
                let d = self.centre(c, rw).distance(p);
                if d <= r {
                    out.push((z as usize, d));
                }
            }
        }
    }

    /// Recompute the preload list: zones within `ZONE_PRELOAD_RADIUS` of the car and around the look-ahead point.
    fn refresh_preload(&mut self, here: Vec2) {
        let mut near = Vec::new();
        self.zones_near(here, ZONE_PRELOAD_RADIUS, &mut near);
        let ahead = here + self.vel * ZONE_LOOKAHEAD_S;
        if ahead.distance(here) > 50.0 {
            let mut more = Vec::new();
            self.zones_near(ahead, ZONE_PRELOAD_RADIUS * 0.5, &mut more);
            // Ahead ones rank as if a quarter of the look-ahead distance away.
            let k = 0.25 * ahead.distance(here);
            near.extend(more.into_iter().map(|(z, d)| (z, d + k)));
        }
        near.sort_by(|a, b| a.1.total_cmp(&b.1));
        self.preload.clear();
        for (z, _) in near {
            if Some(z) != self.current && !self.preload.contains(&z) {
                self.preload.push(z);
            }
        }
    }

    /// Whether model `n` (listed by the shown zone) draws at car position `p`.
    fn draws(&self, n: u16, p: Vec2) -> bool {
        let Some(m) = self.models.get(&n) else { return false };
        let mask = self.shown_lods.get(&m.group).copied().unwrap_or(0);
        let l = m.lod.min(31);
        let (finer, coarser) = (mask & ((1u32 << l) - 1) != 0, mask >> (l + 1) != 0);
        if let Some(r) = m.range {
            // The game's slots (docs/WORLD_LOD.md): a finer / coarser LOD takes over only where the zone lists one;
            // the cull applies always.
            let (lo, hi) = m.bounds;
            let d = Vec2::new((lo[0] - p.x).max(p.x - hi[0]).max(0.0), (lo[1] - p.y).max(p.y - hi[1]).max(0.0)).length();
            // Pure culls (no coarser LOD takes over) are pushed out (P3, `zone_cull_scale`); LOD switches stay the game's.
            let end = if !coarser && r.culls { r.end * zone_cull_scale() } else { r.end };
            return (!finer || d >= r.start) && (!(coarser || r.culls) || d < end);
        }
        if mask.count_ones() <= 1 {
            return true;
        }
        let (lo, hi, t) = &self.groups[m.group];
        let d = Vec2::new((lo[0] - p.x).max(p.x - hi[0]).max(0.0), (lo[1] - p.y).max(p.y - hi[1]).max(0.0)).length();
        let below = mask & ((1u32 << l) - 1);
        let finer_ok = below == 0 || d > t[(31 - below.leading_zeros()).min(3) as usize];
        let coarser_ok = mask >> (l + 1) == 0 || d <= t[l.min(3) as usize];
        finer_ok && coarser_ok
    }

    /// Whether model `n`, not listed by the shown zone, draws at `p` (within `margin` m of its band, for loading):
    /// only objects the shown zone lists nothing of (so no LOD of theirs is drawn twice), by the model's own slot band
    /// with the pushed-out final cull. Never-culled bands (UberLOD backdrop) stay with the zone lists.
    fn union_draws(&self, n: u16, p: Vec2, margin: f32) -> bool {
        let Some(m) = self.models.get(&n) else { return false };
        let Some(r) = m.range.filter(|r| r.end.is_finite()) else { return false };
        if self.shown_lods.contains_key(&m.group) {
            return false;
        }
        let (lo, hi) = m.bounds;
        let d = Vec2::new((lo[0] - p.x).max(p.x - hi[0]).max(0.0), (lo[1] - p.y).max(p.y - hi[1]).max(0.0)).length();
        let end = if r.culls { r.end * zone_cull_scale() } else { r.end };
        d + margin >= r.start && d < end + margin
    }

    /// Recompute `extra`: models of the zones within `zone_union_radius` of the car that `union_draws` near it.
    fn refresh_union(&mut self, here: Vec2) {
        self.extra.clear();
        let r = zone_union_radius();
        if r <= 0.0 {
            return;
        }
        let shown = self.shown.or(self.current);
        let mut near = Vec::new();
        self.zones_near(here, r, &mut near);
        let mut extra = std::mem::take(&mut self.extra);
        for (zi, _) in near.into_iter().filter(|&(zi, _)| Some(zi) != shown) {
            for &n in &self.lists[zi] {
                if !extra.contains(&n) && self.union_draws(n, here, ZONE_UNION_MARGIN) {
                    extra.insert(n);
                }
            }
        }
        // The shown zone's own models follow its rules.
        if let Some(s) = shown {
            for n in &self.lists[s] {
                extra.remove(n);
            }
        }
        self.extra = extra;
    }
}

/// Zone model loads turned into entities per frame.
const ZONE_FINISH_PER_FRAME: usize = 6;
/// Main-thread time (ms) per frame for finishing zone models while stream budgets are on (P5b).
const ZONE_FINISH_MS: f32 = 4.0;
/// Pop-in (P2): the zones of the hex cells (100 m) within this distance of the car load ahead of time (hidden),
/// plus those within half of it around the point `ZONE_LOOKAHEAD_S` ahead along the car's velocity, so the zone
/// switch (which waits for the complete zone) is immediate. Models that no zone within `ZONE_PRELOAD_RADIUS +
/// ZONE_KEEP_MARGIN` lists are unloaded every 2 s. FH1_ZONE_PRELOAD=0 = old behaviour (current zone only).
const ZONE_PRELOAD_RADIUS: f32 = 300.0;
const ZONE_LOOKAHEAD_S: f32 = 4.0;
const ZONE_KEEP_MARGIN: f32 = 200.0;
/// Preload model loads in flight at most (the current zone's loads are not capped).
const ZONE_MAX_PENDING: usize = 24;

/// Pop-in (P3): zone models fade in / out over this many seconds (dither levels in the MeshTag, read by the FX
/// shaders, fh1-render program.rs) whenever the LOD rules or a zone switch show / hide them, so LOD switches
/// cross-fade too. `FH1_ZONE_FADE=<s>`; 0 = the old hard toggle.
const ZONE_FADE_S: f32 = 0.6;

fn zone_fade_s() -> f32 {
    static S: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *S.get_or_init(|| std::env::var("FH1_ZONE_FADE").ok().and_then(|v| v.parse().ok()).filter(|s: &f32| *s >= 0.0).unwrap_or(ZONE_FADE_S))
}

/// Pop-in (P3, raised in P4): scale on the game's zone-model cull distances where no coarser LOD takes over (the user
/// allows drawing further than the game; armco final slots 500 / 700 m -> 800 / 1120 m). `FH1_ZONE_CULL_SCALE`; 1 = the
/// game's, 1.25 = P3.
const ZONE_CULL_SCALE: f32 = 1.6;

/// Pop-in (P4): the game's PVS lists leave an object out of many zones inside its cull band (armco runs: listed by 43% of
/// the zone cells in their band, the nearest unlisting cell at a median 362 m), so guard rails appeared when the car
/// crossed into a listing zone. Models listed by any zone within this radius are drawn by their own slot band too
/// (`Zones::union_draws`; ~100 extra models drawn, 350 at most, on top of the shown zone's ~190).
/// `FH1_ZONE_UNION=<m>`; 0 = the shown zone's list only (old).
const ZONE_UNION_RADIUS: f32 = 900.0;
/// Union models start loading this far (m) before their band reaches the car.
const ZONE_UNION_MARGIN: f32 = 150.0;

fn zone_union_radius() -> f32 {
    static R: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *R.get_or_init(|| std::env::var("FH1_ZONE_UNION").ok().and_then(|v| v.parse().ok()).filter(|r: &f32| *r >= 0.0).unwrap_or(ZONE_UNION_RADIUS))
}

fn zone_cull_scale() -> f32 {
    static K: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *K.get_or_init(|| std::env::var("FH1_ZONE_CULL_SCALE").ok().and_then(|v| v.parse().ok()).filter(|k: &f32| *k > 0.0).unwrap_or(ZONE_CULL_SCALE))
}

/// A range that never culls but has crossfade margins: it only makes Bevy compile the mesh with
/// `VISIBILITY_RANGE_DITHER`, so the FX shader reads the fade level from the MeshTag.
fn zone_fade_range() -> bevy::camera::visibility::VisibilityRange {
    bevy::camera::visibility::VisibilityRange { start_margin: -2.0..-1.0, end_margin: 1.0e7..2.0e7, use_aabb: false }
}

/// MeshTag for dither level `level` (-16 = gone .. 0 = drawn .. 16 = gone; program.rs `fx_vr_level`).
fn zone_fade_tag(level: i32) -> bevy::mesh::MeshTag {
    bevy::mesh::MeshTag(0x8000_0000 | (level + 16).clamp(0, 32) as u32)
}

fn zone_preload_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_ZONE_PRELOAD").map_or(true, |v| v != "0"))
}

/// Zone streaming (replaces the tile + far streaming when `zones` is installed).
/// Loads model `n`: respawned from its parked parts when it has them, else read and built on a task.
fn start_zone_model(commands: &mut Commands, sc: &mut Scenery, z: &mut Zones, n: u16, fade_s: f32) {
    if let Some((parts, _)) = z.parked.remove(&n) {
        let cast = z.models.get(&n).map_or(true, |m| m.shadow);
        z.loaded.insert(n, spawn_zone_model(commands, sc, cast, fade_s, parts));
        return;
    }
    let task = sc.load_task(&z.models[&n].file);
    z.pending.insert(n, task);
}

/// A zone model's entities (hidden; `stream_zones` shows and fades them) from its prepared parts.
fn spawn_zone_model(commands: &mut Commands, sc: &mut Scenery, cast: bool, fade_s: f32, parts: Vec<(Handle<Mesh>, BatchMaterial)>) -> ZoneLoaded {
    let parent = commands.spawn((Transform::IDENTITY, Visibility::Hidden, crate::ui::world_load::WorldEntity)).id();
    let mut children = Vec::new();
    for (mesh, material) in &parts {
        sc.p2.spawned += 1;
        STREAMED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        sc.budget = sc.budget.saturating_sub(1);
        let e = material.spawn(commands, mesh.clone(), Transform::IDENTITY, parent);
        // Category tag for the perf CSV (perf/record.rs) and the P2 stats.
        commands.entity(e).insert(p2::ZoneMesh);
        no_cpu_cull(commands, e);
        sc.static_bounds(commands, e, mesh);
        if !cast {
            commands.entity(e).insert(bevy::light::NotShadowCaster);
        }
        if fade_s > 0.0 {
            // With the material, so the shadow caster proxy (fh1-render shadow.rs) copies the range.
            commands.entity(e).insert((zone_fade_range(), zone_fade_tag(-16)));
        }
        children.push(e);
    }
    ZoneLoaded { parent, children, visible: false, alpha: 0.0, level: -16, parts }
}

fn stream_zones(commands: &mut Commands, sc: &mut Scenery, here: Vec2, time: &Time, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, fx: &mut FxParams) {
    let Some(mut z) = sc.zones.take() else { return };
    let (now_s, dt) = (time.elapsed_secs_f64(), time.delta_secs());
    let (loaded_n, pending_n) = (z.loaded.len(), z.pending.len());
    if let Some(st) = z.stats.as_mut() {
        st.frames += 1;
        st.sum += dt;
        st.worst = st.worst.max(dt);
        if now_s - st.window >= 1.0 {
            info!(
                "zone stats: t {now_s:.1} s, {} frames, mean {:.1} ms, worst {:.1} ms, {loaded_n} models loaded, {pending_n} pending",
                st.frames,
                st.sum / st.frames.max(1) as f32 * 1000.0,
                st.worst * 1000.0
            );
            *st = ZoneStats { window: now_s, entered: st.entered, ..default() };
        }
    }
    let mut instant = false;
    if let Some(last) = z.last_here.replace(here) {
        // Teleports (FH1_TELEPORT, fast travel) don't count as speed, and switch the set without fading.
        instant = last.distance(here) >= 100.0;
        if dt > 0.0 && !instant {
            let v = (here - last) / dt;
            z.vel = z.vel.lerp(v, (dt * 2.0).min(1.0));
        }
    }
    z.frames = z.frames.wrapping_add(1);
    let preload = zone_preload_on();
    let fade_s = zone_fade_s();
    if let Some(now) = z.zone_at(here) {
        if z.current != Some(now) {
            debug!("zones: entered zone {now} ({} models)", z.lists[now].len());
            z.current = Some(now);
            if let Some(st) = z.stats.as_mut() {
                st.entered = Some(now_s);
            }
        }
    }
    // Finish loads: a few per frame (mesh + material creation for a whole zone in one frame hitched 100+ ms).
    let ready: Vec<u16> = z.pending.iter().filter_map(|(k, t)| t.is_finished().then_some(*k)).take(ZONE_FINISH_PER_FRAME).collect();
    let budget_on = sc.budget != usize::MAX;
    let started = std::time::Instant::now();
    let mut finished = 0usize;
    for n in ready {
        // Spawn budget shared with the props (P5b): a model is spawned whole, the next waits for the next frame. Mesh
        // and material building also stops after ZONE_FINISH_MS (big models took ~15 ms of main thread per frame).
        if sc.budget == 0 || (budget_on && started.elapsed().as_secs_f32() * 1000.0 > ZONE_FINISH_MS) {
            break;
        }
        // The shared stream deadline, after at least one model this frame (props ran first).
        if sc.over_time() && finished > 0 {
            break;
        }
        finished += 1;
        let task = z.pending.remove(&n).unwrap();
        let parts = match block_on(future::poll_once(task)).flatten() {
            Some(data) => {
                sc.p2.zone_loads += 1;
                STREAMED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                sc.prepare(data, meshes, materials, fx)
            }
            None => Vec::new(),
        };
        let cast = z.models.get(&n).map_or(true, |m| m.shadow);
        z.loaded.insert(n, spawn_zone_model(commands, sc, cast, fade_s, parts));
    }
    if let Some(cur) = z.current {
        // Load the current zone's models, nearest first.
        let mut wanted: Vec<(u16, f32)> = z.lists[cur]
            .iter()
            .copied()
            .filter(|n| !z.loaded.contains_key(n) && !z.pending.contains_key(n))
            .filter_map(|n| {
                let g = &z.groups[z.models.get(&n)?.group];
                Some((n, Vec2::new((g.0[0] - here.x).max(here.x - g.1[0]).max(0.0), (g.0[1] - here.y).max(here.y - g.1[1]).max(0.0)).length()))
            })
            .collect();
        wanted.sort_by(|a, b| a.1.total_cmp(&b.1));
        let current_wants = !wanted.is_empty();
        for (n, _) in wanted.into_iter().take(STARTS_PER_FRAME * 2) {
            start_zone_model(commands, sc, &mut z, n, fade_s);
        }
        // Preload the zones around and ahead once all of the current zone's loads have started.
        if preload && z.frames % 8 == 1 {
            z.refresh_preload(here);
        }
        if z.frames % 8 == 1 {
            z.refresh_union(here);
        }
        // Only once the current zone is on screen: preloads must not delay it (startup, fast travel).
        if (preload || !z.extra.is_empty()) && !current_wants && z.shown == Some(cur) && z.pending.len() < ZONE_MAX_PENDING {
            let mut room = (ZONE_MAX_PENDING - z.pending.len()).min(STARTS_PER_FRAME);
            let mut starts: Vec<u16> = Vec::new();
            let want = |n: u16, starts: &Vec<u16>| z.models.contains_key(&n) && !z.loaded.contains_key(&n) && !z.pending.contains_key(&n) && !starts.contains(&n);
            'zones: for &zi in z.preload.iter().filter(|_| preload) {
                for &n in &z.lists[zi] {
                    if room == 0 {
                        break 'zones;
                    }
                    if want(n, &starts) {
                        starts.push(n);
                        room -= 1;
                    }
                }
            }
            // Union models (P4) after the preloads, nearest first.
            if room > 0 {
                let dist = |n: &u16| {
                    let (lo, hi) = z.models[n].bounds;
                    Vec2::new((lo[0] - here.x).max(here.x - hi[0]).max(0.0), (lo[1] - here.y).max(here.y - hi[1]).max(0.0)).length()
                };
                let mut extra: Vec<u16> = z.extra.iter().copied().filter(|&n| want(n, &starts)).collect();
                extra.sort_by(|a, b| dist(a).total_cmp(&dist(b)));
                starts.extend(extra.into_iter().take(room));
            }
            for n in starts {
                start_zone_model(commands, sc, &mut z, n, fade_s);
            }
        }
        // Show the current zone once all its models are in (until then the previous zone stays on screen;
        // the very first zone shows as it loads).
        let complete = z.lists[cur].iter().all(|n| z.loaded.contains_key(n) || !z.models.contains_key(n));
        if z.shown != Some(cur) && (complete || z.shown.is_none()) {
            if complete {
                let after = z.stats.as_mut().and_then(|st| st.entered.take()).map(|t| format!(", {:.0} ms after entering", (now_s - t) * 1000.0));
                info!("zones: showing zone {cur} ({} models{})", z.lists[cur].len(), after.unwrap_or_default());
                z.shown = Some(cur);
            }
            z.shown_lods.clear();
            for n in &z.lists[cur] {
                if let Some(m) = z.models.get(n) {
                    *z.shown_lods.entry(m.group).or_default() |= 1 << m.lod.min(31);
                }
            }
            // Unload models neither zone needs (with preloading, the sweep below unloads instead).
            let keep: std::collections::HashSet<u16> = z.lists[cur].iter().copied().collect();
            let gone: Vec<u16> = z.loaded.keys().copied().filter(|n| !keep.contains(n) && !z.extra.contains(n)).collect();
            if complete && !preload {
                for n in gone {
                    if let Some(l) = z.loaded.remove(&n) {
                        sc.retire(commands, l.parent, l.children.len() + 1, budget_on);
                        z.park(n, l.parts);
                    }
                }
            }
        }
    }
    // Preload sweep every 2 s: unload models that no zone near the car (nor the shown / current zone) lists.
    if preload && now_s - z.swept >= 2.0 {
        z.swept = now_s;
        let mut near = Vec::new();
        z.zones_near(here, ZONE_PRELOAD_RADIUS + ZONE_KEEP_MARGIN, &mut near);
        let mut keep: std::collections::HashSet<u16> = std::collections::HashSet::new();
        for zi in near.iter().map(|n| n.0).chain(z.shown).chain(z.current).chain(z.preload.iter().copied()) {
            keep.extend(z.lists[zi].iter().copied());
        }
        keep.extend(z.extra.iter().copied());
        let gone: Vec<u16> = z.loaded.keys().copied().filter(|n| !keep.contains(n)).collect();
        for n in gone {
            if let Some(l) = z.loaded.remove(&n) {
                sc.retire(commands, l.parent, l.children.len() + 1, budget_on);
                z.park(n, l.parts);
            }
        }
        z.pending.retain(|n, _| keep.contains(n));
    }
    // Visibility: the shown zone's models (the current one while the first zone loads), LOD by distance.
    if let Some(list) = z.shown.or(z.current) {
        let listed: std::collections::HashSet<u16> = z.lists[list].iter().copied().collect();
        let hide = sc.p2.hide_zones();
        let updates: Vec<(u16, bool)> = z
            .loaded
            .keys()
            .map(|&n| {
                let on = if listed.contains(&n) { z.draws(n, here) } else { z.extra.contains(&n) && z.union_draws(n, here, 0.0) };
                (n, !hide && on)
            })
            .collect();
        // Fades start once the first zone is complete on screen; before that (and on teleports) the set switches at once.
        let fading = fade_s > 0.0 && z.fade_ready && !instant;
        let step = if fading { dt / fade_s } else { 1.0 };
        for (n, on) in updates {
            let l = z.loaded.get_mut(&n).unwrap();
            let target = if on { 1.0 } else { 0.0 };
            if l.alpha == target && l.visible == on {
                continue;
            }
            if on && !l.visible {
                l.visible = true;
                commands.entity(l.parent).insert(Visibility::Inherited);
            }
            l.alpha = if on { (l.alpha + step).min(1.0) } else { (l.alpha - step).max(0.0) };
            if !on && l.alpha <= 0.0 {
                l.visible = false;
                commands.entity(l.parent).insert(Visibility::Hidden);
                continue;
            }
            if fade_s > 0.0 {
                // Appearing counts -16 -> 0, disappearing 0 -> 16: a cross-fading LOD pair covers each pixel once.
                let level = if on { -16 + (l.alpha * 16.0).round() as i32 } else { ((1.0 - l.alpha) * 16.0).round() as i32 };
                if level != l.level {
                    l.level = level;
                    for &c in &l.children {
                        commands.entity(c).insert(zone_fade_tag(level));
                    }
                }
            }
        }
    }
    if z.shown.is_some() {
        z.fade_ready = true;
    }
    sc.zones = Some(z);
}

impl Scenery {
    pub fn load(assets: &Path) -> Option<Self> {
        Self::load_track(assets.join("scenery/colorado"), &assets.join("shaders/track"), true)
    }

    /// A scenery group folder and the track shaders it names (`shaders/track` of the same game).
    pub fn load_track(dir: PathBuf, track_shaders: &Path, colorado: bool) -> Option<Self> {
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).ok()?).ok()?;
        let tile_size = index["tile_size"].as_f64()? as f32;
        let files = index["tiles"]
            .as_array()?
            .iter()
            .filter_map(|t| Some(((t["x"].as_i64()? as i32, t["z"].as_i64()? as i32), t["file"].as_str()?.to_owned())))
            .collect();
        let alpha: HashMap<u64, bool> = index["textures"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| Some((t["id"].as_u64()?, t["alpha"].as_bool().unwrap_or(false))))
            .collect();
        // The backdrop is opt-in (FH1_FAR=1): without the game's distance-based LOD rules it still shows
        // stray pieces near the playable area (big untextured boxes, a floating terrain sheet).
        let far = index["far"]["file"].as_str().filter(|_| std::env::var_os("FH1_FAR").is_some()).map(str::to_owned);
        let mats: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("materials.json")).ok()?).ok()?;
        let game_materials = mats
            .as_array()?
            .iter()
            .map(|m| {
                let t = &m["textures"][0];
                let diffuse = t["file"].as_str().zip(t["id"].as_u64()).map(|(f, id)| (id as u32, f.to_owned(), alpha.get(&id).copied().unwrap_or(false)));
                let sh = m["shader"].as_str().unwrap_or("").to_ascii_lowercase();
                MaterialInfo { diffuse, blend: sh.contains("_vblnd_") && (sh.contains("decal") || sh.contains("opac")), shader: sh }
            })
            .collect();
        let fx = SceneryMaterials::load(&dir, track_shaders);
        if fx.is_none() {
            warn!("scenery: game shaders not installed; using the simple material path");
        }
        // FH1_ZONES=0 falls back to distance streaming of the full-detail tiles (+ FH1_FAR=1 backdrop).
        let zones = Some(&index["zones"]).filter(|_| std::env::var("FH1_ZONES").as_deref() != Ok("0")).and_then(|v| Zones::load(&dir, v));
        match &zones {
            Some(z) => info!("scenery: PVS zones: {} zones, {} models", z.lists.len(), z.models.len()),
            None => info!("scenery: no PVS zones installed; streaming tiles"),
        }
        Some(Self {
            colorado,
            p2: p2::P2::new(),
            zones,
            fx,
            dir,
            tile_size,
            files,
            far,
            props: Props {
                files: index["props"]["tiles"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| Some(((t["x"].as_i64()? as i32, t["z"].as_i64()? as i32), t["file"].as_str()?.to_owned())))
                    .collect(),
                template_files: index["props"]["templates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|n| n.as_u64())
                    .map(|n| (n as u16, format!("props/templates/{n}.bin")))
                    .collect(),
                lods: index["props"]["lods"].as_array().into_iter().flatten().filter_map(Props::lod_chain).collect(),
                ..default()
            },
            game_materials,
            loaded: HashMap::new(),
            pending: HashMap::new(),
            materials: HashMap::new(),
            pending_images: Vec::new(),
            textures_loaded: 0,
            raw: HashMap::new(),
            fallback_logged: Default::default(),
            budget: usize::MAX,
            deadline: None,
            retire: Default::default(),
            aabbs: HashMap::new(),
        })
    }

    /// The Bevy material for a batch, and whether it is an alpha-blended decal.
    fn material(&mut self, game_material: u32, flags: u32, materials: &mut Assets<StandardMaterial>) -> (Handle<StandardMaterial>, bool) {
        let info = self.game_materials.get(game_material as usize);
        let diffuse = info.and_then(|i| i.diffuse.clone()).filter(|_| flags & STANDIN == 0);
        let blend = diffuse.is_some() && info.is_some_and(|i| i.blend);
        let key = (diffuse.as_ref().map(|d| d.0), blend);
        if let Some(m) = self.materials.get(&key) {
            return (m.clone(), blend);
        }
        let base = StandardMaterial {
            perceptual_roughness: 0.92,
            // Strip winding isn't consistent across the source meshes yet.
            double_sided: true,
            cull_mode: None,
            ..default()
        };
        let m = match diffuse {
            Some((_, file, alpha)) => {
                let path = self.dir.join(file);
                let alpha_mode = if blend {
                    AlphaMode::Blend
                } else if alpha {
                    AlphaMode::Mask(0.5)
                } else {
                    AlphaMode::Opaque
                };
                // Decals sit on the surface below them: pull them forward so they don't z-fight.
                let depth_bias = if blend { 4.0 } else { 0.0 };
                let m = materials.add(StandardMaterial { base_color: Color::WHITE, alpha_mode, depth_bias, ..base });
                self.pending_images.push((m.clone(), AsyncComputeTaskPool::get().spawn(async move { read_dds(&path) })));
                m
            }
            // Runtime-supplied or unconverted textures, and stand-ins: dry Colorado ground.
            None => materials.add(StandardMaterial { base_color: Color::srgb_u8(128, 112, 88), ..base }),
        };
        self.materials.insert(key, m.clone());
        (m, blend)
    }
}

/// A batch's material: the game's shader (fh1-render) or the simple fallback.
#[derive(Clone)]
enum BatchMaterial {
    Fx(Handle<FxMaterial>),
    Simple(Handle<StandardMaterial>),
    /// The simple path's material with the FH1 post chain's output encoding.
    Raw(Handle<FxRawStandard>),
    Remaster(Handle<fh1_remaster::material::RemasterMaterial>),
}

/// P8 (2026-10-08): static scenery meshes (zone models, prop placements, tiles) carry Bevy's `NoCpuCulling`: they skip
/// the CPU visibility systems (frustum, VisibilityRange, directional-light cascades; ~1.5 ms/frame in log 105206) and
/// are frustum- and range-culled in the GPU mesh preprocess instead. Their ViewVisibility then mirrors
/// InheritedVisibility (the perf CSV's visible counts count them as visible). Not on smash pieces, cars, crowds,
/// particles. `FH1_NO_CPU_CULL=0` = CPU culling (old).
fn no_cpu_cull_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_NO_CPU_CULL").map_or(true, |v| v != "0"))
}

fn no_cpu_cull(commands: &mut Commands, e: Entity) {
    if no_cpu_cull_on() {
        commands.entity(e).insert(bevy::camera::visibility::NoCpuCulling);
    }
}

impl BatchMaterial {
    fn id(&self) -> bevy::asset::UntypedAssetId {
        match self {
            BatchMaterial::Fx(m) => m.id().untyped(),
            BatchMaterial::Simple(m) => m.id().untyped(),
            BatchMaterial::Raw(m) => m.id().untyped(),
            BatchMaterial::Remaster(m) => m.id().untyped(),
        }
    }

    fn spawn(&self, commands: &mut Commands, mesh: Handle<Mesh>, transform: Transform, parent: Entity) -> Entity {
        match self {
            BatchMaterial::Fx(m) => commands.spawn((Mesh3d(mesh), MeshMaterial3d(m.clone()), transform, ChildOf(parent))).id(),
            BatchMaterial::Simple(m) => commands.spawn((Mesh3d(mesh), MeshMaterial3d(m.clone()), transform, ChildOf(parent))).id(),
            BatchMaterial::Raw(m) => commands.spawn((Mesh3d(mesh), MeshMaterial3d(m.clone()), transform, ChildOf(parent))).id(),
            BatchMaterial::Remaster(m) => commands.spawn((Mesh3d(mesh), MeshMaterial3d(m.clone()), transform, ChildOf(parent))).id(),
        }
    }
}

/// P8 lever 2 (2026-10-08): a model's / template's / tile's parts that end up with the same material (the setup splits
/// batches by game material x flags x vertex attribute set; the remaster gives every part ONE vertex layout, so splits by
/// attribute set share a material there) become one mesh, one entity. Only parts with identical topology and attribute
/// sets merge. `FH1_PART_MERGE=0` = one entity per part (old).
fn part_merge_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_PART_MERGE").map_or(true, |v| v != "0"))
}

fn merge_parts(parts: Vec<(Mesh, BatchMaterial)>) -> Vec<(Mesh, BatchMaterial)> {
    if !part_merge_on() || parts.len() < 2 {
        return parts;
    }
    let layout = |m: &Mesh| {
        let mut ids: Vec<_> = m.attributes().map(|(a, _)| (a.id, a.format)).collect();
        ids.sort_by_key(|(id, _)| *id);
        (m.primitive_topology(), ids, m.indices().is_some())
    };
    let mut out: Vec<(Mesh, BatchMaterial)> = Vec::with_capacity(parts.len());
    for (mesh, material) in parts {
        let id = material.id();
        let into = out.iter_mut().find(|(m, mat)| mat.id() == id && m.primitive_topology() == PrimitiveTopology::TriangleList && layout(m) == layout(&mesh));
        // Same layout and topology: the merge can't fail.
        if let Some((m, _)) = into {
            if m.merge(&mesh).is_ok() {
                continue;
            }
        }
        out.push((mesh, material));
    }
    out
}

/// P8 (2026-10-08): static scenery entities get their mesh's bounds (computed once per mesh in `prepare`) plus
/// `NoAutoAabb`, so Bevy's calculate_bounds no longer computes one per spawned entity nor scans them every frame for
/// changed mesh assets (particles / skid marks / smoke modify meshes every frame). `FH1_STATIC_AABB=0` = Bevy computes
/// them (old).
fn static_aabb_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_STATIC_AABB").map_or(true, |v| v != "0"))
}

impl Scenery {
    /// Inserts `mesh`'s precomputed bounds + `NoAutoAabb` on static scenery entity `e` (`static_aabb_on`).
    fn static_bounds(&self, commands: &mut Commands, e: Entity, mesh: &Handle<Mesh>) {
        if !static_aabb_on() {
            return;
        }
        if let Some(a) = self.aabbs.get(&mesh.id()) {
            commands.entity(e).insert((*a, bevy::camera::visibility::NoAutoAabb));
        }
    }

    /// The batch material for a simple-path StandardMaterial: itself, or its FxRawStandard copy while the post
    /// chain is on.
    fn simple(&mut self, m: Handle<StandardMaterial>, materials: &Assets<StandardMaterial>, fx: &mut FxParams) -> BatchMaterial {
        if !fx.lib.raw_output {
            return BatchMaterial::Simple(m);
        }
        let raw = self.raw.entry(m.id()).or_insert_with(|| {
            let base = materials.get(&m).cloned().unwrap_or_default();
            fx.raw_materials.add(FxRawStandard { base, extension: FxRawOutput {} })
        });
        BatchMaterial::Raw(raw.clone())
    }

    /// Meshes + materials for a loaded tile / template.
    fn prepare(
        &mut self,
        data: TileData,
        meshes: &mut Assets<Mesh>,
        materials: &mut Assets<StandardMaterial>,
        fx: &mut FxParams,
    ) -> Vec<(Handle<Mesh>, BatchMaterial)> {
        let mut out: Vec<(Mesh, BatchMaterial)> = Vec::new();
        match data {
            TileData::Fx(batches) => {
                for b in batches {
                    if b.flags & STANDIN == 0 {
                        match fx.remaster.batch(&self.dir, b.material) {
                            fh1_remaster::scenery::RemasterBatch::Material(m, _) => {
                                out.push((fh1_remaster::scenery::prepare_mesh(b.mesh), BatchMaterial::Remaster(m)));
                                continue;
                            }
                            fh1_remaster::scenery::RemasterBatch::Skip => continue,
                            fh1_remaster::scenery::RemasterBatch::Faithful => {}
                        }
                    }
                    let fx_material = if b.flags & STANDIN == 0 {
                        let f = self.fx.as_mut().unwrap();
                        f.material(b.material, &mut fx.lib, &mut fx.globals, &mut fx.shaders, &mut fx.materials)
                    } else {
                        None
                    };
                    match fx_material {
                        Some(m) => out.push((b.mesh, BatchMaterial::Fx(m))),
                        None => {
                            // Stand-ins (untextured) and materials whose game shader is missing (diffuse only).
                            if b.flags & STANDIN == 0 {
                                let sh = self.game_materials.get(b.material as usize).map(|m| m.shader.clone()).unwrap_or_default();
                                if self.fallback_logged.insert(sh.clone()) {
                                    warn!("scenery: game material {} ({sh}) has no game shader; simple fallback", b.material);
                                }
                            }
                            let (material, blend) = self.material(b.material, b.flags, materials);
                            let mut mesh = b.mesh;
                            // Raw file-order colour (A, R, G, B): only decals use it, for their opacity.
                            let raw = mesh.remove_attribute(fh1_render::material::ATTRIBUTE_COLOR);
                            if let (true, Some(bevy::mesh::VertexAttributeValues::Unorm8x4(c))) = (blend, raw) {
                                let c: Vec<[f32; 4]> = c.iter().map(|x| [1.0, 1.0, 1.0, x[0] as f32 / 255.0]).collect();
                                mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, c);
                            }
                            let m = self.simple(material, materials, fx);
                            out.push((mesh, m));
                        }
                    }
                }
            }
            TileData::Simple(batches) => {
                for (game_material, flags, mut mesh) in batches {
                    let (material, blend) = self.material(game_material, flags, materials);
                    // The game's vertex colours are AO / blend weights, not tint: only decals use their alpha.
                    let colours = mesh.remove_attribute(Mesh::ATTRIBUTE_COLOR);
                    if let (true, Some(bevy::mesh::VertexAttributeValues::Float32x4(c))) = (blend, colours) {
                        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, c);
                    }
                    let m = self.simple(material, materials, fx);
                    out.push((mesh, m));
                }
            }
        }
        merge_parts(out)
            .into_iter()
            .map(|(mesh, m)| {
                use bevy::camera::primitives::MeshAabb;
                let aabb = mesh.compute_aabb();
                let h = meshes.add(mesh);
                if let Some(a) = aabb {
                    self.aabbs.insert(h.id(), a);
                }
                (h, m)
            })
            .collect()
    }

    fn load_task(&self, file: &str) -> Task<Option<TileData>> {
        let path = self.dir.join(file);
        let game_shaders = self.fx.is_some();
        AsyncComputeTaskPool::get().spawn(async move {
            if game_shaders {
                fh1_render::scenery::parse_tile(&std::fs::read(&path).ok()?).map(TileData::Fx)
            } else {
                read_tile(&path).map(TileData::Simple)
            }
        })
    }
}

/// Props (`.pgeo` placements: trees, bushes, fences, parked cars...), streamed by 256 m tile within
/// `PROP_RADIUS`. Templates load once (all of them, ~2.5 MB); each placement is a child entity per template
/// batch sharing the template's mesh + material, which Bevy batches automatically.
#[derive(Default)]
struct Props {
    /// Tile key -> placement file.
    files: HashMap<(i32, i32), String>,
    template_files: Vec<(u16, String)>,
    templates: Option<HashMap<u16, Vec<(Handle<Mesh>, BatchMaterial)>>>,
    template_tasks: Vec<(u16, Task<Option<TileData>>)>,
    /// Loaded tiles: parent entity and the ring it was spawned for (0 = all placements, 1 / 2 = only the chain parts
    /// ending past `ring_min_end`).
    loaded: HashMap<(i32, i32), (Entity, u8)>,
    /// Loads in flight: task and ring.
    pending: HashMap<(i32, i32), (Task<Option<Vec<(u16, Mat4, Vec3, u32, [u32; 2])>>>, u8)>,
    logged: bool,
    /// LOD0 template -> [(template, from m, to m)] per LOD, from the game's LOD tables (index.json `props.lods`).
    lods: HashMap<u16, Vec<(u16, f32, f32)>>,
    /// Game material -> its copy with flipped culling, for mirrored placements.
    flipped: HashMap<AssetId<FxMaterial>, Handle<FxMaterial>>,
    /// (tile, placement index in the tile file) -> its entities, for smashing (smash.rs).
    placed: HashMap<(i32, i32, u32), Vec<Entity>>,
    /// Placements smashed this session: not respawned when their tile reloads.
    broken: std::collections::HashSet<(i32, i32, u32)>,
    /// Tiles being placed over several frames (spawn budget), oldest first.
    placing: std::collections::VecDeque<PlaceJob>,
    /// Loaded tile -> entities spawned for it (despawn budget).
    tile_entities: HashMap<(i32, i32), usize>,
    /// Remaster (W4): placements merged per tile x LOD level x material (fh1_remaster::batch; opt-in `FH1_BATCH=1`).
    merge: fh1_remaster::batch::PropMerge,
    /// Template -> bounding radius around its origin (m, template space), for the remaster's small-caster cut.
    radius: HashMap<u16, f32>,
    /// Template -> largest |coordinate| of its vertices (m, template space), for the far cull (`prop_far_end`).
    extent: HashMap<u16, f32>,
    /// P8 lever 1 (`level_stream_on`): placed tile -> its LOD level entries; the car position and speed this frame.
    levels: HashMap<(i32, i32), TileLevels>,
    eye: Vec3,
    speed: f32,
}

/// Remaster: props whose placed bounding radius is under this (m) don't cast sun shadows. Bevy's cascades have no
/// per-cascade caster culling, so every small prop in range was drawn into all three (W4, c3's CSM numbers).
/// `FH1_RM_SMALL_CASTERS=0` = they cast as before.
const SMALL_CASTER_RADIUS: f32 = 2.0;

fn small_casters_off() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| fh1_remaster::enabled() && std::env::var("FH1_RM_SMALL_CASTERS").as_deref() != Ok("0"))
}

/// Largest |coordinate| of any vertex (half the size of the template's origin-centred box).
fn template_extent(data: &TileData) -> f32 {
    let r = |m: &Mesh| match m.attribute(Mesh::ATTRIBUTE_POSITION) {
        Some(bevy::mesh::VertexAttributeValues::Float32x3(p)) => p.iter().map(|v| Vec3::from_array(*v).abs().max_element()).fold(0.0, f32::max),
        _ => 0.0,
    };
    match data {
        TileData::Fx(b) => b.iter().map(|b| r(&b.mesh)).fold(0.0, f32::max),
        TileData::Simple(b) => b.iter().map(|b| r(&b.2)).fold(0.0, f32::max),
    }
}

/// Largest distance of a vertex from the template origin.
fn template_radius(data: &TileData) -> f32 {
    let r = |m: &Mesh| match m.attribute(Mesh::ATTRIBUTE_POSITION) {
        Some(bevy::mesh::VertexAttributeValues::Float32x3(p)) => p.iter().map(|v| Vec3::from_array(*v).length()).fold(0.0, f32::max),
        _ => 0.0,
    };
    match data {
        TileData::Fx(b) => b.iter().map(|b| r(&b.mesh)).fold(0.0, f32::max),
        TileData::Simple(b) => b.iter().map(|b| r(&b.2)).fold(0.0, f32::max),
    }
}

/// A loaded placement tile being spawned a budget's worth per frame; its parent stays hidden until it is complete.
struct PlaceJob {
    k: (i32, i32),
    ring: u8,
    list: Vec<(u16, Mat4, Vec3, u32, [u32; 2])>,
    next: usize,
    parent: Entity,
    spawned: usize,
    /// (tile, placement) -> entity of the new near placements, registered for smashing once the tile is complete.
    placed: Vec<((i32, i32, u32), Entity)>,
    /// Remaster: the tile's merged meshes being built; `merged` = merging placements are skipped by `place_props`.
    merge: Option<fh1_remaster::batch::MergeTask>,
    merged: bool,
    /// P8 lever 1: every placement LOD level of the tile, spawned or not (empty with `FH1_PROP_LEVEL_STREAM=0`).
    levels: Vec<LevelEntry>,
}

/// Scale on the game's prop LOD / fade distances (pop-in, P2: LODs switch further out, where the change is
/// smaller on screen). `FH1_PROP_LOD_SCALE` overrides; 1 = the game's distances.
const PROP_LOD_SCALE: f32 = 1.25;

fn prop_lod_scale() -> f32 {
    static K: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *K.get_or_init(|| std::env::var("FH1_PROP_LOD_SCALE").ok().and_then(|v| v.parse().ok()).filter(|k: &f32| *k > 0.0).unwrap_or(PROP_LOD_SCALE))
}

/// Props without a LOD table (CollObjs smashables): drawn out to this distance (times `prop_lod_scale`).
const PROP_DEFAULT_FADE: f32 = 300.0;

/// Pop-in (P3): no prop is culled closer than this (m; the game's smallest props fade at 50-100 m, x1.25 still
/// popped in plain view). `FH1_PROP_MIN_FADE` overrides; 0 = the scaled game distances.
const PROP_MIN_FADE: f32 = 100.0;

fn prop_min_fade() -> f32 {
    static K: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *K.get_or_init(|| std::env::var("FH1_PROP_MIN_FADE").ok().and_then(|v| v.parse().ok()).unwrap_or(PROP_MIN_FADE))
}

/// Pop-in (P4, 2026-10-06): the game culls festival barriers at 60-160 m, fences / walls at 60-100 m and trees at 220 m
/// (x1.25 here), which popped in plain view. Placements of templates at least `PROP_FAR_MIN_EXTENT` m in size (largest
/// |coordinate|: excludes the 120k creosote / sage / silvery bushes and the rocks) keep their last LOD out to
/// `extent x scale x PROP_FAR_K` m (projected half-size ~2-3 px at 1080p), at least `PROP_FAR_FLOOR`, at most
/// `prop_far_max`: festival barriers / fences / armco arrows 600 m, trees and parked cars 800 m. Survey (Colorado,
/// placements within their cull of a road cell): median 1.1k -> 6.2k, worst 3.1k -> 13.5k.
/// `FH1_PROP_FAR_MAX=<m>` (also the middle streaming ring, `stream_prop_loads`); 0 = the game's culls (old).
const PROP_FAR_K: f32 = 300.0;
const PROP_FAR_MIN_EXTENT: f32 = 1.25;
const PROP_FAR_FLOOR: f32 = 600.0;
const PROP_FAR_MAX: f32 = 800.0;

fn prop_far_max() -> f32 {
    static K: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *K.get_or_init(|| std::env::var("FH1_PROP_FAR_MAX").ok().and_then(|v| v.parse().ok()).filter(|k: &f32| *k >= 0.0).unwrap_or(PROP_FAR_MAX))
}

/// The pushed-out cull (m) of a placement of a template with `extent` at placement `scale`, if it gets one.
fn prop_far_end(extent: Option<f32>, scale: f32) -> Option<f32> {
    let max = prop_far_max();
    let e = extent.filter(|&e| max > 0.0 && e >= PROP_FAR_MIN_EXTENT)?;
    Some((e * scale * PROP_FAR_K).max(PROP_FAR_FLOOR).min(max))
}

/// Pop-in (P3): props dither-fade at their cull distance and cross-fade between LODs (Bevy `VisibilityRange`
/// margins; the FX shaders honour them, fh1-render program.rs `VR_DITHER_WGSL`) instead of a hard cut.
/// `FH1_PROP_FADE=0` = the old abrupt ranges.
fn prop_fade_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_PROP_FADE").map_or(true, |v| v != "0"))
}

/// The visibility range of one LOD of a placement drawn over [from, to) m: LOD switches cross-fade over a band
/// centred on the switch distance (the neighbouring LOD gets the same band), the final cull fades out over the
/// last stretch before `to`, so nothing draws past the distance the prop streaming guarantees.
fn prop_range(from: f32, to: f32, last: bool) -> bevy::camera::visibility::VisibilityRange {
    use bevy::camera::visibility::VisibilityRange;
    if !prop_fade_on() {
        return VisibilityRange::abrupt(from, to);
    }
    let swap = |d: f32| (0.1 * d).clamp(4.0, 20.0) * 0.5;
    let start_margin = if from <= 0.0 { -2.0..-1.0 } else { from - swap(from)..from + swap(from) };
    let end_margin = if last {
        let w = (0.15 * to).clamp(6.0, 40.0);
        to - w..to
    } else {
        to - swap(to)..to + swap(to)
    };
    // Short LOD segments: the two bands must not overlap.
    let (mut start_margin, mut end_margin) = (start_margin, end_margin);
    if start_margin.end > end_margin.start {
        let mid = 0.5 * (start_margin.end + end_margin.start);
        start_margin.end = mid.max(start_margin.start);
        end_margin.start = mid.min(end_margin.end);
    }
    VisibilityRange { start_margin, end_margin, use_aabb: false }
}

impl Props {
    /// The material for a mirrored placement (negative determinant): culling flipped for game shaders; the
    /// fallback StandardMaterial draws both sides already.
    fn mirrored(&mut self, material: &BatchMaterial, materials: &mut Assets<FxMaterial>) -> BatchMaterial {
        match material {
            BatchMaterial::Fx(h) => {
                let flipped = self.flipped.entry(h.id()).or_insert_with(|| {
                    let mut m = materials.get(h).cloned().expect("loaded FxMaterial");
                    m.flip_cull = !m.flip_cull;
                    materials.add(m)
                });
                BatchMaterial::Fx(flipped.clone())
            }
            other => other.clone(),
        }
    }

    fn lod_chain(v: &serde_json::Value) -> Option<(u16, Vec<(u16, f32, f32)>)> {
        let n = v["n"].as_u64()? as u16;
        // Pop-in (P2; the user allows drawing further than the game): every switch / fade distance is scaled.
        let k = prop_lod_scale();
        let fade = (v["fade_m"].as_f64()? as f32 * k).max(prop_min_fade());
        let (l1, l2) = (v["lod1"].as_u64().map(|x| x as u16), v["lod2"].as_u64().map(|x| x as u16));
        let (d1, d2) = (v["lod1_m"].as_f64().map(|x| x as f32 * k), v["lod2_m"].as_f64().map(|x| x as f32 * k));
        let mut chain = Vec::new();
        // LOD0 until LOD1 takes over (if it exists), LOD1 until LOD2, the last one until the fade distance.
        let lod1_at = l1.and(d1).unwrap_or(fade).min(fade);
        chain.push((n, 0.0, lod1_at));
        if let Some(l1) = l1 {
            let lod2_at = l2.and(d2).unwrap_or(fade).min(fade);
            if lod1_at < lod2_at {
                chain.push((l1, lod1_at, lod2_at));
            }
            if let Some(l2) = l2.filter(|_| lod2_at < fade) {
                chain.push((l2, lod2_at, fade));
            }
        }
        Some((n, chain))
    }
}

/// Prop tiles load within this distance; each placement's LODs show / hide by camera distance
/// (VisibilityRange) with the game's LOD-table distances (trees ~220 m, small props ~70 m).
const PROP_RADIUS: f32 = 300.0;
/// Far ring (docs/WORLD_LOD.md): out to here, tiles spawn only the placements whose LOD chain reaches past
/// `PROP_RADIUS` (zone-placed buildings, festival marquees and grandstands: game culls 700-1800 m or none).
const FAR_RADIUS: f32 = 1500.0;

/// Rings (P4): 0 = within `PROP_RADIUS` (every placement), 1 = middle ring out to `prop_far_max` (chain parts ending past
/// `PROP_RADIUS`: the pushed-out barriers / trees and the far buildings), 2 = out to `FAR_RADIUS` (parts ending past the
/// middle ring). A tile of ring r lies past ring r - 1's radius, so parts ending sooner never show there.
fn ring_radius(ring: u8) -> f32 {
    match ring {
        0 => PROP_RADIUS,
        1 => prop_far_max().max(PROP_RADIUS),
        _ => FAR_RADIUS.max(prop_far_max()),
    }
}

/// Chain parts ending at or before this are skipped in a ring-`ring` tile.
fn ring_min_end(ring: u8) -> f32 {
    if ring == 0 { f32::NEG_INFINITY } else { ring_radius(ring - 1) }
}

fn stream_props(commands: &mut Commands, sc: &mut Scenery, here: Vec2, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, fx: &mut FxParams) {
    // Templates: start all loads once, collect as they finish.
    if sc.props.templates.is_none() && sc.props.template_tasks.is_empty() && !sc.props.template_files.is_empty() {
        let files = std::mem::take(&mut sc.props.template_files);
        sc.props.template_tasks = files.iter().map(|(n, f)| (*n, sc.load_task(f))).collect();
        sc.props.templates = Some(HashMap::new());
    }
    let mut i = 0;
    while i < sc.props.template_tasks.len() {
        if sc.props.template_tasks[i].1.is_finished() {
            let (n, task) = sc.props.template_tasks.swap_remove(i);
            if let Some(data) = block_on(future::poll_once(task)).flatten() {
                if small_casters_off() {
                    sc.props.radius.insert(n, template_radius(&data));
                }
                sc.props.extent.insert(n, template_extent(&data));
                if fh1_remaster::batch::PropMerge::on() {
                    let parts = sc.template_parts(&data, fx);
                    sc.props.merge.add_template(n, parts);
                }
                let prepared = sc.prepare(data, meshes, materials, fx);
                sc.props.templates.get_or_insert_with(HashMap::new).insert(n, prepared);
            }
        } else {
            i += 1;
        }
    }
    if !sc.props.template_tasks.is_empty() {
        return; // place nothing until every template is ready
    }
    if !sc.props.logged {
        sc.props.logged = true;
        let t = sc.props.templates.as_ref().map_or(0, |t| t.len());
        info!("props: {t} templates ready, {} placement tiles", sc.props.files.len());
    }

    let size = sc.tile_size;
    let centre = |k: (i32, i32)| Vec2::new((k.0 as f32 + 0.5) * size, (k.1 as f32 + 0.5) * size);
    let budget_on = sc.budget != usize::MAX;
    // Finish placement loads: each becomes a placing job (spawned within the frame budget, parent hidden till done).
    let merging = fh1_remaster::batch::PropMerge::on();
    // Merged tiles wait for the smashable set (smashables stay entities).
    let ready: Vec<(i32, i32)> = if merging && !sc.props.merge.ready() {
        Vec::new()
    } else {
        sc.props.pending.iter().filter_map(|(k, t)| t.0.is_finished().then_some(*k)).collect()
    };
    for k in ready {
        let (task, ring) = sc.props.pending.remove(&k).unwrap();
        let Some(list) = block_on(future::poll_once(task)).flatten() else { continue };
        if sc.props.loaded.is_empty() && sc.props.placing.is_empty() {
            let p = list.first().map(|(n, m, ..)| (*n, m.w_axis));
            info!("props: first tile {k:?}: {} placements, e.g. {p:?}", list.len());
        }
        let parent = commands.spawn((Transform::IDENTITY, Visibility::Hidden, crate::ui::world_load::WorldEntity)).id();
        let merge = merging.then(|| {
            let placements = list.iter().map(|&(model, transform, _, tint, _)| fh1_remaster::batch::Placement { model, transform, tint }).collect();
            let broken = (0..list.len() as u32).filter(|&i| sc.props.broken.contains(&(k.0, k.1, i))).collect();
            let min_end = ring_min_end(ring);
            sc.props.merge.start(placements, &sc.props.lods, PROP_DEFAULT_FADE * prop_lod_scale(), min_end, broken)
        });
        sc.props.placing.push_back(PlaceJob { k, ring, list, next: 0, parent, spawned: 0, placed: Vec::new(), merged: merge.is_some(), merge, levels: Vec::new() });
    }
    // Level changes of placed tiles first: a late one shows as a missing LOD, a late tile only as later detail.
    update_prop_levels(commands, sc, fx);
    while sc.budget > 0 {
        let Some(mut job) = sc.props.placing.pop_front() else { break };
        if let Some(task) = job.merge.take() {
            if !task.is_finished() {
                job.merge = Some(task);
                sc.props.placing.push_front(job);
                break;
            }
            for ((_, material), m) in block_on(future::poll_once(task)).unwrap_or_default() {
                let fh1_remaster::scenery::RemasterBatch::Material(h, _) = fx.remaster.batch(&sc.dir, material) else { continue };
                let e = commands.spawn((Mesh3d(meshes.add(m.mesh)), MeshMaterial3d(h), m.aabb, m.shadow, Transform::IDENTITY, ChildOf(job.parent))).id();
                no_cpu_cull(commands, e);
                if static_aabb_on() {
                    commands.entity(e).insert(bevy::camera::visibility::NoAutoAabb);
                }
                sc.p2.spawned += 1;
                STREAMED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                job.spawned += 1;
                sc.budget = sc.budget.saturating_sub(1);
            }
        }
        place_props(commands, sc, &mut job, fx);
        if job.next < job.list.len() {
            sc.props.placing.push_front(job);
            break;
        }
        let k = job.k;
        // A tile changing ring replaces its previous load (now that the new one is complete: no blink).
        if let Some((e, _)) = sc.props.loaded.remove(&k) {
            let n = sc.props.tile_entities.remove(&k).unwrap_or(1) + sc.props.levels.remove(&k).map_or(0, |l| l.live);
            sc.retire(commands, e, n, budget_on);
            sc.props.placed.retain(|p, _| (p.0, p.1) != k);
        }
        // Entities of the level entries are counted in the entries' `live` (they despawn individually).
        let mut level_live = 0;
        if !job.levels.is_empty() {
            level_live = job.levels.iter().map(|e| e.spawned.len()).sum();
            // Checked again next frame: the car has moved while the tile was being placed.
            sc.props.levels.insert(k, TileLevels { entries: std::mem::take(&mut job.levels), checked_at: Vec3::splat(1.0e9), live: level_live });
        }
        // Smash bookkeeping for the new placements (after the old tile's entries are gone).
        for (key, e) in job.placed.drain(..) {
            sc.props.placed.entry(key).or_default().push(e);
        }
        commands.entity(job.parent).insert(if sc.p2.hide_props() { Visibility::Hidden } else { Visibility::Inherited });
        sc.p2.prop_tiles += 1;
        STREAMED[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        debug!("props: tile {k:?} placed");
        sc.props.loaded.insert(k, (job.parent, job.ring));
        sc.props.tile_entities.insert(k, (job.spawned + 1).saturating_sub(level_live));
    }
    // Unload, then start nearby loads (nearest first, a few per frame).
    let reach = [0, 1, 2].map(|r| ring_radius(r) + size * 0.75);
    let gone: Vec<(i32, i32)> = sc.props.loaded.keys().copied().filter(|&k| centre(k).distance(here) > reach[2] + 100.0).collect();
    for k in gone {
        if let Some((e, _)) = sc.props.loaded.remove(&k) {
            let n = sc.props.tile_entities.remove(&k).unwrap_or(1) + sc.props.levels.remove(&k).map_or(0, |l| l.live);
            sc.retire(commands, e, n, budget_on);
            sc.props.placed.retain(|p, _| (p.0, p.1) != k);
        }
    }
    stream_prop_loads(sc, here, reach);
}

/// P8 lever 1 (2026-10-08): a prop LOD level is spawned only while the car is near its distance band, not for the whole
/// tile lifetime (a ring-0 tile spawned every placement x every LOD level x batch: ~25-35k festival entities, ~85% of
/// them range-hidden, all walked by visibility / extract every frame). Levels spawn within [from - margin, to + margin]
/// (3D distance to the car) and despawn past that plus `LEVEL_HYST`; the VisibilityRange fades stay. Margin = camera
/// offset + fade band + `LEVEL_STEP` + `LEVEL_LOOKAHEAD_S` x speed. `FH1_PROP_LEVEL_STREAM=0` = every level for the
/// tile's lifetime (old).
fn level_stream_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_PROP_LEVEL_STREAM").map_or(true, |v| v != "0"))
}

/// Fixed part of the level margin (m): chase camera offset (~10 m) + the widest LOD cross-fade half band (10 m) + slack.
const LEVEL_MARGIN: f32 = 24.0;
/// Levels are re-checked once the car has moved this far (m) since a tile's last check.
const LEVEL_STEP: f32 = 8.0;
/// Speed look-ahead (s) on the margin.
const LEVEL_LOOKAHEAD_S: f32 = 1.5;
/// Extra distance (m) past the spawn band before a level despawns (no churn at the band edge).
const LEVEL_HYST: f32 = 30.0;
/// Level spawns per frame allowed past the streaming budgets.
const LEVEL_MIN_SPAWNS: usize = 256;

/// One LOD level of one placement (P8 lever 1): what `spawn_level` needs, and its entities while spawned.
struct LevelEntry {
    /// Placement index in the tile file (smash key).
    placement: u32,
    lod: u16,
    m: Mat4,
    normal: Vec3,
    tint: u32,
    lightmap: Option<u32>,
    small: bool,
    from: f32,
    to: f32,
    last: bool,
    spawned: Vec<Entity>,
}

impl LevelEntry {
    fn wanted(&self, eye: Vec3, margin: f32) -> bool {
        let d = self.m.w_axis.truncate().distance(eye);
        d >= self.from - margin && d <= self.to + margin
    }
}

/// A placed tile's level entries, where the car was at their last check, and the entities they have spawned.
struct TileLevels {
    entries: Vec<LevelEntry>,
    checked_at: Vec3,
    live: usize,
}

/// The level spawn margin at `speed` (m/s).
fn level_margin(speed: f32) -> f32 {
    LEVEL_MARGIN + LEVEL_STEP + speed.min(120.0) * LEVEL_LOOKAHEAD_S
}

/// Spawns one placement LOD level's batches under `parent` (shared by `place_props` and `update_prop_levels`).
fn spawn_level(
    commands: &mut Commands,
    sc: &mut Scenery,
    templates: &HashMap<u16, Vec<(Handle<Mesh>, BatchMaterial)>>,
    fx: &mut FxParams,
    parent: Entity,
    e: &LevelEntry,
) -> Vec<Entity> {
    let Some(batches) = templates.get(&e.lod) else { return Vec::new() };
    let t = Transform::from_matrix(e.m);
    // ~2% of placements are mirrored: authored culling would draw them inside out.
    let mirrored = e.m.determinant() < 0.0;
    let object = fh1_render::material::object_consts(e.tint, e.normal);
    let range = prop_range(e.from, e.to, e.last);
    let mut out = Vec::with_capacity(batches.len());
    for (mesh, material) in batches {
        let material = match material {
            BatchMaterial::Fx(h) => BatchMaterial::Fx(sc.fx.as_mut().map_or(h.clone(), |f| {
                let v = f.object_variant(h, object, &mut fx.materials);
                e.lightmap.map_or(v.clone(), |l| f.lightmap_variant(&v, l, &mut fx.materials))
            })),
            BatchMaterial::Remaster(h) => BatchMaterial::Remaster(e.lightmap.map_or(h.clone(), |l| fx.remaster.lightmap_variant(h, l))),
            other => other.clone(),
        };
        let material = match (mirrored, material) {
            (true, BatchMaterial::Remaster(h)) => BatchMaterial::Remaster(fx.remaster.mirrored(&h)),
            (true, m) => sc.props.mirrored(&m, &mut fx.materials),
            (false, m) => m,
        };
        let id = material.spawn(commands, mesh.clone(), t, parent);
        sc.static_bounds(commands, id, mesh);
        sc.p2.spawned += 1;
        STREAMED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        sc.budget = sc.budget.saturating_sub(1);
        // PropMesh: category tag for the perf CSV (perf/record.rs) and the P2 stats.
        let mut ec = commands.entity(id);
        ec.insert((range.clone(), p2::PropMesh));
        if no_cpu_cull_on() {
            ec.insert(bevy::camera::visibility::NoCpuCulling);
        }
        if e.small {
            ec.insert(bevy::light::NotShadowCaster);
        }
        out.push(id);
    }
    out
}

/// Spawn `job`'s placements until the frame's spawn budget is used up.
fn place_props(commands: &mut Commands, sc: &mut Scenery, job: &mut PlaceJob, fx: &mut FxParams) {
    let (k, ring, parent) = (job.k, job.ring, job.parent);
    let min_end = ring_min_end(ring);
    let streaming = level_stream_on();
    let (eye, margin) = (sc.props.eye, level_margin(sc.props.speed));
    // Taken out while placing (mirrored placements need `sc.props` mutably); put back below.
    let templates = sc.props.templates.take().unwrap_or_default();
    let default_chain = |n: u16| vec![(n, 0.0, PROP_DEFAULT_FADE * prop_lod_scale())];
    let first = job.next;
    while job.next < job.list.len() && sc.budget > 0 && (job.next - first < MIN_PLACE || !sc.over_time()) {
        let i = job.next;
        job.next += 1;
        let (n, m, normal, tint, lightmaps) = job.list[i];
        let key = (k.0, k.1, i as u32);
        if sc.props.broken.contains(&key) {
            continue;
        }
        if job.merged && sc.props.merge.merges(n, sc.props.lods.get(&n)) {
            continue;
        }
        // Placement scale (largest axis), for the small-caster radius test and the far cull.
        let scale = m.x_axis.truncate().length().max(m.y_axis.truncate().length()).max(m.z_axis.truncate().length());
        let mut chain = sc.props.lods.get(&n).cloned().unwrap_or_else(|| default_chain(n));
        // P4: the last LOD stays out to the size-based far cull.
        if let (Some(last), Some(end)) = (chain.last_mut(), prop_far_end(sc.props.extent.get(&n).copied(), scale)) {
            last.2 = last.2.max(end);
        }
        if chain.iter().all(|c| c.2 <= min_end) {
            continue;
        }
        let levels = chain.len();
        // Ring 1 / 2 tiles lie past the previous ring's radius (their centre is past its reach): parts ending sooner never show.
        for (level, (lod, from, to)) in chain.into_iter().enumerate().filter(|(_, c)| c.2 > min_end) {
            let small = small_casters_off() && sc.props.radius.get(&lod).is_some_and(|r| r * scale < SMALL_CASTER_RADIUS);
            // Night lightmap of this LOD's draw record (LOD0 / LOD1 only; fh1-render lightmap_variant).
            let lightmap = lightmaps.get(level).copied().filter(|&l| l != u32::MAX);
            let mut entry = LevelEntry { placement: i as u32, lod, m, normal, tint, lightmap, small, from, to, last: level + 1 == levels, spawned: Vec::new() };
            if !streaming || entry.wanted(eye, margin) {
                entry.spawned = spawn_level(commands, sc, &templates, fx, parent, &entry);
                job.spawned += entry.spawned.len();
                if !streaming && ring == 0 {
                    job.placed.extend(entry.spawned.iter().map(|&e| (key, e)));
                }
            }
            if streaming {
                job.levels.push(entry);
            }
        }
    }
    sc.props.templates = Some(templates);
}

/// P8 lever 1: spawn / despawn the placed tiles' LOD levels as the car moves (`level_stream_on`). A tile is re-checked
/// once the car is `LEVEL_STEP` from its last check; a check cut short by the spawn budget resumes next frame. The first
/// `LEVEL_MIN_SPAWNS` spawns of a frame ignore the frame budgets (progress floor while tiles stream).
fn update_prop_levels(commands: &mut Commands, sc: &mut Scenery, fx: &mut FxParams) {
    if !level_stream_on() || sc.props.levels.is_empty() {
        return;
    }
    let (eye, margin) = (sc.props.eye, level_margin(sc.props.speed));
    let keep = margin + LEVEL_HYST;
    let templates = sc.props.templates.take().unwrap_or_default();
    let mut levels = std::mem::take(&mut sc.props.levels);
    let mut spawned = 0;
    for (k, tl) in levels.iter_mut() {
        if tl.checked_at.distance_squared(eye) < LEVEL_STEP * LEVEL_STEP {
            continue;
        }
        let Some(&(parent, _)) = sc.props.loaded.get(k) else { continue };
        let mut complete = true;
        for e in tl.entries.iter_mut() {
            if e.spawned.is_empty() {
                if e.wanted(eye, margin) {
                    if spawned >= LEVEL_MIN_SPAWNS && (sc.budget == 0 || sc.over_time()) {
                        complete = false;
                        break;
                    }
                    e.spawned = spawn_level(commands, sc, &templates, fx, parent, e);
                    tl.live += e.spawned.len();
                    spawned += e.spawned.len();
                }
            } else if !e.wanted(eye, keep) {
                tl.live = tl.live.saturating_sub(e.spawned.len());
                for id in e.spawned.drain(..) {
                    commands.entity(id).despawn();
                }
            }
        }
        if complete {
            tl.checked_at = eye;
        }
    }
    sc.props.levels = levels;
    sc.props.templates = Some(templates);
}

// Start placement loads for tiles entering the rings (`ring_radius`; `reach` = each ring's tile-centre reach).
fn stream_prop_loads(sc: &mut Scenery, here: Vec2, reach: [f32; 3]) {
    let size = sc.tile_size;
    let centre = |k: (i32, i32)| Vec2::new((k.0 as f32 + 0.5) * size, (k.1 as f32 + 0.5) * size);
    // Wanted ring per tile: the innermost ring whose reach holds it; a tile moves to a finer ring at once and to a
    // coarser one only 100 m past its ring's reach (hysteresis).
    let ring_at = |d: f32| (0u8..3).find(|&r| d <= reach[r as usize]);
    let mut wanted: Vec<((i32, i32), f32, u8)> = sc
        .props
        .files
        .keys()
        .copied()
        .filter(|k| !sc.props.pending.contains_key(k) && !sc.props.placing.iter().any(|j| j.k == *k))
        .filter_map(|k| {
            let d = centre(k).distance(here);
            let want = ring_at(d)?;
            let want = match sc.props.loaded.get(&k) {
                None => want,
                Some(&(_, cur)) if want < cur => want,
                Some(&(_, cur)) if want > cur && d > reach[cur as usize] + 100.0 => want,
                _ => return None,
            };
            Some((k, d, want))
        })
        .collect();
    // Inner rings first, nearest first.
    wanted.sort_by(|a, b| (a.2, a.1).partial_cmp(&(b.2, b.1)).unwrap_or(std::cmp::Ordering::Equal));
    for (k, _, ring) in wanted.into_iter().take(STARTS_PER_FRAME) {
        let path = sc.dir.join(&sc.props.files[&k]);
        sc.props.pending.insert(k, (AsyncComputeTaskPool::get().spawn(async move { read_placements(&path) }), ring));
    }
}

/// `props/tiles/<x>_<z>.bin` (fh1setup `write_props`): `b"FH1PROP3"`, u32 count, (u32 template, f32x16 matrix,
/// f32x3 ground normal, u32 tint, u32 x 2 LOD0 / LOD1 night lightmap); `b"FH1PROP2"` has no lightmaps,
/// `b"FH1PROP1"` (older installs) no normal/tint either.
fn read_placements(path: &Path) -> Option<Vec<(u16, Mat4, Vec3, u32, [u32; 2])>> {
    let b = std::fs::read(path).ok()?;
    let version = match b.get(..8)? {
        b"FH1PROP3" => 3,
        b"FH1PROP2" => 2,
        b"FH1PROP1" => 1,
        _ => return None,
    };
    let v2 = version >= 2;
    let stride = [68, 84, 92][version - 1];
    let n = u32::from_le_bytes(b.get(8..12)?.try_into().ok()?) as usize;
    if b.len() < 12 + n * stride {
        return None;
    }
    let f = |o: usize| f32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    Some(
        (0..n)
            .map(|i| {
                let o = 12 + i * stride;
                let model = u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u16;
                let (normal, tint) = if v2 {
                    (Vec3::new(f(o + 68), f(o + 72), f(o + 76)), u32::from_le_bytes(b[o + 80..o + 84].try_into().unwrap()))
                } else {
                    (Vec3::Y, 0xFF80_8080)
                };
                let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
                let lightmaps = if version >= 3 { [u(o + 84), u(o + 88)] } else { [u32::MAX; 2] };
                (model, Mat4::from_cols_array(&std::array::from_fn(|k| f(o + 4 + k * 4))), normal, tint, lightmaps)
            })
            .collect(),
    )
}

/// Game-shader resources used by `stream`.
#[derive(bevy::ecs::system::SystemParam)]
pub struct FxParams<'w> {
    lib: ResMut<'w, FxLibrary>,
    globals: ResMut<'w, FxGlobals>,
    shaders: ResMut<'w, Assets<Shader>>,
    materials: ResMut<'w, Assets<FxMaterial>>,
    raw_materials: ResMut<'w, Assets<FxRawStandard>>,
    remaster: fh1_remaster::scenery::RemasterParams<'w>,
}

pub fn stream(
    mut commands: Commands,
    scenery: Option<ResMut<Scenery>>,
    cars: Query<&Car>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut fx: FxParams,
    time: Res<Time>,
    mut p2q: p2::P2Params,
    mut budget_mode: Local<Option<bool>>,
    mut warm: Local<Option<WarmQueue>>,
) {
    let _watch = crate::perf::watch("stream");
    let (Some(mut sc), Ok(car)) = (scenery, cars.single()) else { return };
    // L1: at startup, wait (at most a few seconds) until the loading cover's UI pipeline has compiled; the tile reads
    // fill the async compute pool that Bevy compiles pipelines on. FH1_LOADING=0 = no wait.
    if crate::ui::loading::hold_streaming() {
        return;
    }
    // Per-frame streaming budgets (P5b); retired roots despawn a budget's worth per frame.
    let budget_on = stream_budget_on(time.elapsed_secs_f64());
    if budget_mode.replace(budget_on) != Some(budget_on) {
        info!("scenery: stream budgets {}", if budget_on { "on" } else { "off" });
    }
    sc.budget = if budget_on { SPAWN_BUDGET } else { usize::MAX };
    let ms = stream_ms();
    sc.deadline = (budget_on && ms > 0.0).then(|| std::time::Instant::now() + std::time::Duration::from_secs_f32(ms / 1000.0));
    {
        let sc = &mut *sc;
        sc.drain_retired(&mut commands, budget_on);
    }
    let p = car.0.position;
    let size = sc.tile_size;
    let centre = |k: (i32, i32)| Vec2::new((k.0 as f32 + 0.5) * size, (k.1 as f32 + 0.5) * size);
    let here = Vec2::new(p.x, p.z);
    sc.props.eye = p;
    sc.props.speed = car.0.velocity.length();

    let zones_loaded = sc.zones.as_ref().map_or(0, |z| z.loaded.len());
    let props_loaded = sc.props.loaded.len();
    if p2::tick(&mut sc.p2, &mut p2q, props_loaded, zones_loaded, fx.materials.len(), here) {
        let vis = if sc.p2.hide_props() { Visibility::Hidden } else { Visibility::Inherited };
        for (e, _) in sc.props.loaded.values() {
            commands.entity(*e).insert(vis);
        }
        info!("p2 A/B: mode {}", p2::AB_MODES[sc.p2.mode]);
    }

    // Give materials their textures once read (editing the material makes Bevy rebuild its bind group).
    let mut i = 0;
    while i < sc.pending_images.len() {
        if sc.pending_images[i].1.is_finished() {
            let (material, task) = sc.pending_images.swap_remove(i);
            if let (Some(img), Some(mut m)) = (block_on(future::poll_once(task)).flatten(), materials.get_mut(&material)) {
                let img = images.add(img);
                m.base_color_texture = Some(img.clone());
                if let Some(mut r) = sc.raw.get(&material.id()).and_then(|h| fx.raw_materials.get_mut(h)) {
                    r.base.base_color_texture = Some(img);
                }
                sc.textures_loaded += 1;
                if sc.textures_loaded % 100 == 1 {
                    info!("scenery: {} textures loaded", sc.textures_loaded);
                }
            }
        } else {
            i += 1;
        }
    }

    // Every scenery program translated and registered during loading, not on first sight (P5b). Spread over frames
    // (WARM_MS each) so the loading cover keeps animating: all at once took 1.8-3.4 s in one frame. Per world: a map
    // switch (new scenery dir) warms its own programs. FH1_WARM_PROGRAMS=0 = on demand.
    if warm.as_ref().is_none_or(|w| w.dir != sc.dir) {
        let names = match sc.fx.as_ref() {
            Some(f) if std::env::var("FH1_WARM_PROGRAMS").as_deref() != Ok("0") => f.program_names(),
            _ => Vec::new(),
        };
        WARM_LEFT.store(names.len(), std::sync::atomic::Ordering::Relaxed);
        *warm = Some(WarmQueue { dir: sc.dir.clone(), total: names.len(), names, built: 0, busy_ms: 0.0, frames: 0 });
    }
    if let (Some(w), Some(f)) = (warm.as_mut(), sc.fx.as_mut()) {
        if !w.names.is_empty() {
            let t = std::time::Instant::now();
            while let Some(name) = w.names.pop() {
                w.built += f.warm_program(&name, &mut fx.lib, &mut fx.globals, &mut fx.shaders) as usize;
                if t.elapsed().as_secs_f32() * 1000.0 >= WARM_MS {
                    break;
                }
            }
            w.busy_ms += t.elapsed().as_secs_f32() * 1000.0;
            w.frames += 1;
            WARM_LEFT.store(w.names.len(), std::sync::atomic::Ordering::Relaxed);
            if w.names.is_empty() {
                info!("scenery: warmed {} of {} game shader programs in {:.0} ms over {} frames", w.built, w.total, w.busy_ms, w.frames);
            }
        }
    }
    // Attach game textures as they finish loading.
    if let Some(f) = sc.fx.as_mut() {
        f.poll(&mut images, &mut fx.materials);
        f.maintain(time.elapsed_secs());
    }

    // Finish tile loads that are ready: one child entity per texture batch.
    let ready: Vec<(i32, i32)> = sc.pending.iter_mut().filter_map(|(k, t)| t.is_finished().then_some(*k)).collect();
    for k in ready {
        let task = sc.pending.remove(&k).unwrap();
        if let Some(data) = block_on(future::poll_once(task)).flatten() {
            let parent = commands.spawn((Transform::IDENTITY, Visibility::default(), crate::ui::world_load::WorldEntity)).id();
            for (mesh, material) in sc.prepare(data, &mut meshes, &mut materials, &mut fx) {
                let e = material.spawn(&mut commands, mesh.clone(), Transform::IDENTITY, parent);
                no_cpu_cull(&mut commands, e);
                sc.static_bounds(&mut commands, e, &mesh);
            }
            sc.loaded.insert(k, parent);
        }
    }

    stream_props(&mut commands, &mut sc, here, &mut meshes, &mut materials, &mut fx);
    if sc.zones.is_some() {
        stream_zones(&mut commands, &mut sc, here, &time, &mut meshes, &mut materials, &mut fx);
        return;
    }

    // Unload far tiles.
    let far: Vec<(i32, i32)> = sc.loaded.keys().copied().filter(|&k| k != FAR && centre(k).distance(here) > LOAD_RADIUS + UNLOAD_MARGIN).collect();
    for k in far {
        if let Some(e) = sc.loaded.remove(&k) {
            commands.entity(e).despawn();
        }
    }

    // Start loads for missing nearby tiles, nearest first.
    let mut wanted: Vec<((i32, i32), f32)> = sc
        .files
        .keys()
        .copied()
        .filter(|k| !sc.loaded.contains_key(k) && !sc.pending.contains_key(k))
        .map(|k| (k, centre(k).distance(here)))
        .filter(|(_, d)| *d <= LOAD_RADIUS)
        .collect();
    wanted.sort_by(|a, b| a.1.total_cmp(&b.1));
    let far_file = sc.far.clone().filter(|_| !sc.loaded.contains_key(&FAR) && !sc.pending.contains_key(&FAR));
    let starts = far_file.map(|f| (FAR, f)).into_iter().chain(wanted.into_iter().take(STARTS_PER_FRAME).map(|(k, _)| (k, sc.files[&k].clone())));
    for (k, file) in starts.collect::<Vec<_>>() {
        let task = sc.load_task(&file);
        sc.pending.insert(k, task);
    }
}

/// `FH1TILE4` (see fh1setup `scenery.rs`) -> (game material, flags, mesh) per batch. Uses position,
/// normal, uv0 and colour (alpha = the file's A byte); tangent / uv1 / uv2 are skipped.
fn read_tile(path: &Path) -> Option<Vec<Batch>> {
    let b = std::fs::read(path).ok()?;
    if !matches!(b.get(..8)?, b"FH1TILE4" | b"FH1TILE5") {
        return None;
    }
    let u32_at = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().unwrap())) };
    let f32_at = |o: usize| f32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let nb = u32_at(8)? as usize;
    let mut o = 12;
    let mut out = Vec::with_capacity(nb);
    for _ in 0..nb {
        let (material, flags, mask, nv, ni) = (u32_at(o)?, u32_at(o + 4)?, u32_at(o + 8)?, u32_at(o + 12)? as usize, u32_at(o + 16)? as usize);
        o += 20;
        let uv_sets = (mask >> 1 & 7).count_ones() as usize;
        // Version 5 (FM4): uv3 f32x2 (bit 32) + binormal f32x3 (bit 64) after the colour; skipped here.
        let v5_extra = if mask & 32 != 0 { 8 } else { 0 } + if mask & 64 != 0 { 12 } else { 0 };
        let len = nv * (24 + if mask & ATTR_TANGENT != 0 { 12 } else { 0 } + uv_sets * 8 + if mask & ATTR_COLOR != 0 { 4 } else { 0 } + v5_extra) + ni * 4;
        if b.len() < o + len {
            return None;
        }
        let vec3s = |o: &mut usize| -> Vec<[f32; 3]> {
            let v = (0..nv).map(|i| [f32_at(*o + i * 12), f32_at(*o + i * 12 + 4), f32_at(*o + i * 12 + 8)]).collect();
            *o += nv * 12;
            v
        };
        let positions = vec3s(&mut o);
        let normals = vec3s(&mut o);
        if mask & ATTR_TANGENT != 0 {
            o += nv * 12;
        }
        let uv0 = (mask & ATTR_UV0 != 0).then(|| (0..nv).map(|i| [f32_at(o + i * 8), f32_at(o + i * 8 + 4)]).collect::<Vec<_>>());
        o += uv_sets * nv * 8;
        let colours = (mask & ATTR_COLOR != 0).then(|| (0..nv).map(|i| [1.0, 1.0, 1.0, b[o + i * 4] as f32 / 255.0]).collect::<Vec<[f32; 4]>>());
        if mask & ATTR_COLOR != 0 {
            o += nv * 4;
        }
        o += nv * v5_extra;
        let indices: Vec<u32> = (0..ni).map(|i| u32::from_le_bytes(b[o + i * 4..o + i * 4 + 4].try_into().unwrap())).collect();
        o += ni * 4;
        if indices.iter().any(|&i| i as usize >= nv) {
            return None;
        }
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv0.unwrap_or_else(|| vec![[0.0; 2]; nv]));
        if let Some(c) = colours {
            mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, c);
        }
        mesh.insert_indices(Indices::U32(indices));
        out.push((material, flags, mesh));
    }
    Some(out)
}

/// DDS with a DX10 header (as fh1setup writes them): BCn levels, largest first. Colour formats are read as
/// sRGB (this renderer only samples diffuse textures).
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
        80 => TextureFormat::Bc4RUnorm,
        83 => TextureFormat::Bc5RgUnorm,
        _ => return None,
    };
    let mut image = Image::new_uninit(
        Extent3d { width, height, depth_or_array_layers: 1 },
        TextureDimension::D2,
        format,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.mip_level_count = mips;
    image.data = Some(b[148..].to_vec());
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

/// Main-thread time per frame for the scenery program warm-up (ms; at least one program per frame).
const WARM_MS: f32 = 6.0;

/// Programs the warm-up still has to translate (read by [`Scenery::readiness`]).
static WARM_LEFT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Scenery programs still to translate for the world in `dir` (see the warm-up in `stream`).
pub struct WarmQueue {
    dir: PathBuf,
    names: Vec<String>,
    total: usize,
    built: usize,
    busy_ms: f32,
    frames: u32,
}

/// Cumulative streaming counters for the gameplay perf recorder (perf/record.rs): zone model loads, prop tiles
/// placed, scenery entities spawned.
pub static STREAMED: [std::sync::atomic::AtomicU32; 3] = [const { std::sync::atomic::AtomicU32::new(0) }; 3];

impl Scenery {
    /// Streaming state for the perf recorder: (zone models loaded, zone loads pending, prop tiles loaded, prop tile
    /// loads pending, prop tiles being placed).
    pub fn stream_gauges(&self) -> [usize; 5] {
        let (zl, zp) = self.zones.as_ref().map_or((0, 0), |z| (z.loaded.len(), z.pending.len()));
        [zl, zp, self.props.loaded.len(), self.props.pending.len(), self.props.placing.len()]
    }

    /// Remaster merge (W4): the smashable prop templates (smash.rs, once its colliders are loaded; empty without them).
    pub fn set_smashable(&mut self, templates: std::collections::HashSet<u16>) {
        self.props.merge.set_smashable(templates);
    }

    /// A prop template's parts as CPU meshes for merging, or `None` when a part stays on the faithful path.
    fn template_parts(&self, data: &TileData, fx: &mut FxParams) -> Option<Vec<fh1_remaster::batch::TemplatePart>> {
        let TileData::Fx(batches) = data else { return None };
        let mut parts = Vec::new();
        for b in batches {
            if b.flags & STANDIN != 0 {
                return None;
            }
            match fx.remaster.batch(&self.dir, b.material) {
                fh1_remaster::scenery::RemasterBatch::Material(..) => parts.push(fh1_remaster::batch::TemplatePart {
                    mesh: std::sync::Arc::new(fh1_remaster::scenery::prepare_mesh(b.mesh.clone())),
                    material: b.material,
                }),
                fh1_remaster::scenery::RemasterBatch::Skip => {}
                fh1_remaster::scenery::RemasterBatch::Faithful => return None,
            }
        }
        Some(parts)
    }

    /// Removes a prop placement (tile, index in its tile file) from view for the rest of the session (smash.rs).
    pub fn break_prop(&mut self, commands: &mut Commands, key: (i32, i32, u32)) {
        self.props.broken.insert(key);
        for e in self.props.placed.remove(&key).unwrap_or_default() {
            commands.entity(e).despawn();
        }
        // P8 lever 1: the placement's level entries go (spawned ones despawn).
        if let Some(tl) = self.props.levels.get_mut(&(key.0, key.1)) {
            let live = &mut tl.live;
            tl.entries.retain_mut(|e| {
                if e.placement != key.2 {
                    return true;
                }
                *live = live.saturating_sub(e.spawned.len());
                for id in e.spawned.drain(..) {
                    commands.entity(id).despawn();
                }
                false
            });
        }
    }

    /// Prop LOD level entries currently not spawned (P8 lever 1; perf CSV `prop_levels_deferred`).
    pub fn deferred_prop_levels(&self) -> usize {
        self.props.levels.values().map(|t| t.entries.iter().filter(|e| e.spawned.is_empty()).count()).sum()
    }

    /// Spawns prop template `n` (LOD0 batches) as children of `parent` at `transform` (local to the parent).
    /// False when the template isn't loaded.
    pub fn spawn_template(&self, commands: &mut Commands, n: u16, transform: Transform, parent: Entity) -> bool {
        let Some(batches) = self.props.templates.as_ref().and_then(|t| t.get(&n)) else { return false };
        for (mesh, material) in batches {
            material.spawn(commands, mesh.clone(), transform, parent);
        }
        true
    }

    /// The installed scenery directory (`scenery/colorado`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// L1 loading screens (ui/loading.rs): how far streaming around `here` (x, z) is, 0..1, and whether the world
    /// there is ready to be seen: the zone at `here` is complete and on screen (or, without zones, the nearby
    /// tiles are in), the prop templates are loaded and the prop tiles right around `here` are placed in full.
    /// Read-only; it doesn't change what streams.
    pub fn readiness(&self, here: Vec2) -> (f32, bool) {
        let size = self.tile_size;
        let centre = |k: (i32, i32)| Vec2::new((k.0 as f32 + 0.5) * size, (k.1 as f32 + 0.5) * size);
        let (world, world_ready) = match self.zones.as_ref() {
            Some(z) => match z.zone_at(here) {
                Some(cur) => {
                    let list = &z.lists[cur];
                    let total = list.iter().filter(|n| z.models.contains_key(n)).count();
                    let done = list.iter().filter(|n| z.models.contains_key(n) && z.loaded.contains_key(n)).count();
                    let frac = if total == 0 { 1.0 } else { done as f32 / total as f32 };
                    (frac, done == total && z.shown == Some(cur))
                }
                None => (1.0, true),
            },
            None => {
                let near: Vec<(i32, i32)> = self.files.keys().copied().filter(|&k| centre(k).distance(here) <= 600.0).collect();
                let done = near.iter().filter(|k| self.loaded.contains_key(k)).count();
                (if near.is_empty() { 1.0 } else { done as f32 / near.len() as f32 }, done == near.len())
            }
        };
        let templates_ready = self.props.template_tasks.is_empty() && (self.props.templates.is_some() || self.props.template_files.is_empty());
        let near_props: Vec<(i32, i32)> = self.props.files.keys().copied().filter(|&k| centre(k).distance(here) <= size).collect();
        let props_done = near_props.iter().filter(|k| matches!(self.props.loaded.get(k), Some((_, 0)))).count();
        let props = if !templates_ready { 0.0 } else if near_props.is_empty() { 1.0 } else { props_done as f32 / near_props.len() as f32 };
        // The spread program warm-up (WarmQueue) must finish under the cover too.
        let warming = WARM_LEFT.load(std::sync::atomic::Ordering::Relaxed) > 0;
        (world * 0.75 + props * 0.25, world_ready && templates_ready && props_done == near_props.len() && !warming)
    }
}
