//! P12 bake path, engine side (fh1-rewrite-15; file format and runtime API: fh1_remaster::static_world::bake).
//!
//! - **Bake mode** `FH1_BAKE_CELLS=1` (the install's `bake/<track>/` folder) or `=<dir>`: `stream` hands the frame to
//!   [`drive`] instead of streaming around the car. It loads every prop template, prop tile and zone model through the SAME
//!   code as the game (`Scenery::prepare`, `spawn_level`, `spawn_zone_model`) with the static world recording, writes one
//!   file per bundle (`templates`, `props_<x>_<z>`, `zone_<n>`) and exits. Resumable: a bundle already valid for this
//!   install is skipped. Progress in the log every few seconds.
//!   One-liner (fh1setup's last step later): `FH1_BAKE_CELLS=1 FH1_RENDERER=remaster fh1-engine --data <data>`.
//! - **What goes in**: a prop placement only when every LOD level of its chain is static-world geometry (opaque / cutout
//!   / unlit) and it isn't smashable; the bundle lists them (`extra`) and the runtime places the rest live. A zone model
//!   only when all its parts are static; else it stays live.
//! - **Runtime** (static world on, `FH1_BAKED=0` = off): a zone model / prop tile with a valid bundle is one file read
//!   plus [`fh1_remaster::static_world::bake::load`]: no Mesh assets, no merge, no entities but the streaming parent
//!   (which owns the bundle and its materials: despawning it unloads them). Missing or stale bundles = today's path.
//! - **Stamp**: [`BAKE_FORMAT`], installation.json pipeline hashes of `scenery` and `remaster`, and the env knobs that
//!   change records (prop LOD scale / min fade / far max / fade, small casters, instance lightmaps, zone fade).

use std::path::{Path, PathBuf};

use bevy::asset::UntypedAssetId;

use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};
use fh1_remaster::static_world::bake::{self as lib, Bundle, BundleHandle, MatKey, Stamp};

use super::*;

/// Bump when VERTEX_WORDS / GpuRecord / the prop LOD rules (prop_range, prop_far_end, ring_min_end, lod_chain) or what a
/// bundle holds change.
pub const BAKE_FORMAT: u32 = 1;

/// Where the bundles are and the stamp they must carry.
pub struct BakeCtx {
    pub dir: PathBuf,
    pub stamp: Stamp,
}

/// The bake context of a track (Colorado only; static world on; installation.json readable).
pub fn ctx(sc_dir: &Path, colorado: bool) -> Option<BakeCtx> {
    if !colorado || !fh1_remaster::static_world::on() {
        return None;
    }
    if !lib::baking() && std::env::var("FH1_BAKED").is_ok_and(|v| v == "0") {
        return None;
    }
    let private = sc_dir.parent()?.parent()?;
    let data = private.ancestors().nth(4)?;
    let inst: serde_json::Value = serde_json::from_slice(&std::fs::read(data.join("installation.json")).ok()?).ok()?;
    let mut stamp = Stamp::new();
    stamp.insert("format".into(), BAKE_FORMAT.to_string());
    for g in ["scenery", "remaster"] {
        stamp.insert(g.into(), inst["pipelines"][g].as_str()?.to_owned());
    }
    for k in ["FH1_PROP_LOD_SCALE", "FH1_PROP_MIN_FADE", "FH1_PROP_FAR_MAX", "FH1_PROP_FADE", "FH1_RM_SMALL_CASTERS", "FH1_INST_LM", "FH1_ZONE_FADE"] {
        stamp.insert(k.into(), std::env::var(k).unwrap_or_default());
    }
    let track = sc_dir.file_name()?.to_string_lossy().into_owned();
    let dir = match lib::bake_dir() {
        Some(d) if d != Path::new("1") => d,
        _ => private.join("bake").join(track),
    };
    Some(BakeCtx { dir, stamp })
}

pub fn zone_name(n: u16) -> String {
    format!("zone_{n}")
}

pub fn tile_name(k: (i32, i32)) -> String {
    format!("props_{}_{}", k.0, k.1)
}

/// Reads bundle `name` on the async pool (None = missing / stale: the live path).
pub fn read_task(ctx: &BakeCtx, name: &str) -> Task<Option<Bundle<MatKey>>> {
    let (path, stamp) = (ctx.dir.join(lib::file_name(name)), ctx.stamp.clone());
    AsyncComputeTaskPool::get().spawn(async move { lib::read(&path, &stamp) })
}

/// Reads bundle `name` now (startup: the templates).
pub fn read_now(ctx: &BakeCtx, name: &str) -> Option<Bundle<MatKey>> {
    lib::read(&ctx.dir.join(lib::file_name(name)), &ctx.stamp)
}

/// Strong handles of a baked bundle's materials (the remaster cache evicts materials only it holds).
#[derive(Component)]
pub struct BakedMaterials(#[allow(dead_code)] Vec<Handle<fh1_remaster::material::RemasterMaterial>>);

/// Materials of `b` from their keys (this track's remaster table).
fn resolve(b: Bundle<MatKey>, dir: &Path, fx: &mut FxParams) -> (Bundle<UntypedAssetId>, BakedMaterials) {
    let mut keep: Vec<Handle<fh1_remaster::material::RemasterMaterial>> = Vec::new();
    let mut cache: HashMap<MatKey, Option<UntypedAssetId>> = HashMap::new();
    let (b, dropped) = b.map_materials(|k| {
        *cache.entry(*k).or_insert_with(|| {
            let h = fx.remaster.from_bake_key(dir, *k)?;
            let id = h.id().untyped();
            keep.push(h);
            Some(id)
        })
    });
    if dropped > 0 {
        warn!("bake: {}: {dropped} instances without a material dropped", b.name);
    }
    (b, BakedMaterials(keep))
}

/// Loads `b` (hidden) and spawns its streaming parent, which owns the bundle and its materials.
pub fn spawn_bundle(commands: &mut Commands, b: Bundle<MatKey>, dir: &Path, fx: &mut FxParams, keep: impl Fn(&lib::BakedInstance<MatKey>) -> bool) -> (Entity, BundleHandle) {
    let mut b = b;
    b.instances.retain(|i| keep(i));
    let (b, mats) = resolve(b, dir, fx);
    let h = lib::load(b, true);
    let parent = commands.spawn((Transform::IDENTITY, Visibility::Hidden, crate::ui::world_load::WorldEntity, lib::BakedBundle(h.clone()), mats)).id();
    (parent, h)
}

/// A baked zone model as a loaded one (hidden; `stream_zones` shows and fades it through its handle).
pub fn spawn_zone(commands: &mut Commands, b: Bundle<MatKey>, dir: &Path, fx: &mut FxParams, fade_s: f32) -> ZoneLoaded {
    let (parent, h) = spawn_bundle(commands, b, dir, fx, |_| true);
    if fade_s <= 0.0 {
        lib::set_tag(&h, 0);
    }
    ZoneLoaded { parent, children: Vec::new(), statics: Vec::new(), visible: false, alpha: 0.0, level: -16, parts: Vec::new(), baked: Some(h) }
}

// ---------------------------------------------------------------- bake mode

/// Bake progress (a Local of `stream`).
#[derive(Default)]
pub struct DriveState {
    phase: u8,
    items: Vec<BakeItem>,
    next: usize,
    written: usize,
    skipped: usize,
    live: usize,
    started: Option<std::time::Instant>,
    logged: f32,
    waited: f32,
}

#[derive(Clone, Copy)]
enum BakeItem {
    Tile((i32, i32)),
    Zone(u16),
}

/// Items per frame (bake mode only; the window is hidden, so frames are cheap).
const TILES_PER_FRAME: usize = 8;
const ZONES_PER_FRAME: usize = 8;

/// One frame of bake mode. Phases: 0 templates, 1 wait for the smashable set (smash.rs), 2 tiles + zones, 3 done (exit).
pub fn drive(commands: &mut Commands, sc: &mut Scenery, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, fx: &mut FxParams, st: &mut DriveState, dt: f32, exit: &mut MessageWriter<AppExit>) {
    let Some(ctx) = ctx(&sc.dir, sc.colorado) else {
        error!("bake: no bake context (remaster renderer on Colorado with installation.json needed); nothing baked");
        exit.write(AppExit::error());
        return;
    };
    let started = *st.started.get_or_insert_with(std::time::Instant::now);
    sc.budget = usize::MAX;
    match st.phase {
        0 => {
            // Templates: every one loads (as stream_props does) inside the `templates` bundle.
            lib::begin("templates");
            let files = std::mem::take(&mut sc.props.template_files);
            let mut templates = sc.props.templates.take().unwrap_or_default();
            for (n, f) in &files {
                let Some(data) = block_on(sc.load_task(f)) else { continue };
                if small_casters_off() {
                    sc.props.radius.insert(*n, template_radius(&data));
                }
                sc.props.extent.insert(*n, template_extent(&data));
                let prepared = sc.prepare(data, meshes, materials, fx, true);
                templates.insert(*n, prepared);
            }
            sc.props.templates = Some(templates);
            let b = lib::end().unwrap_or_default();
            write(&ctx, b, fx, &mut st.written);
            info!("bake: {} prop templates", files.len());
            st.phase = 1;
        }
        1 => {
            // Smashable placements stay live: wait for smash.rs's set (with a log while waiting).
            if sc.props.smashable.is_none() {
                st.waited += dt;
                if st.waited > 5.0 {
                    st.waited = 0.0;
                    info!("bake: waiting for the smashable prop set (smash.rs colliders)");
                }
                return;
            }
            let mut tiles: Vec<(i32, i32)> = sc.props.files.keys().copied().collect();
            tiles.sort();
            st.items = tiles.into_iter().map(BakeItem::Tile).collect();
            if let Some(z) = sc.zones.as_ref() {
                let mut zs: Vec<u16> = z.models.keys().copied().collect();
                zs.sort();
                st.items.extend(zs.into_iter().map(BakeItem::Zone));
            }
            info!("bake: {} bundles to check in {}", st.items.len(), ctx.dir.display());
            st.phase = 2;
        }
        2 => {
            let (mut tiles, mut zones) = (0, 0);
            while st.next < st.items.len() && tiles < TILES_PER_FRAME && zones < ZONES_PER_FRAME {
                let item = st.items[st.next];
                st.next += 1;
                let name = match item {
                    BakeItem::Tile(k) => tile_name(k),
                    BakeItem::Zone(n) => zone_name(n),
                };
                if lib::valid(&ctx.dir.join(lib::file_name(&name)), &ctx.stamp) {
                    st.skipped += 1;
                    continue;
                }
                match item {
                    BakeItem::Tile(k) => {
                        tiles += 1;
                        bake_tile(commands, sc, fx, &ctx, k, st);
                    }
                    BakeItem::Zone(n) => {
                        zones += 1;
                        bake_zone(commands, sc, meshes, materials, fx, &ctx, n, st);
                    }
                }
            }
            let t = started.elapsed().as_secs_f32();
            if t - st.logged > 5.0 || st.next >= st.items.len() {
                st.logged = t;
                info!(
                    "bake: {} / {} ({:.0}%), {} written, {} already valid, {} live-only, {:.0} s",
                    st.next,
                    st.items.len(),
                    st.next as f32 * 100.0 / st.items.len().max(1) as f32,
                    st.written,
                    st.skipped,
                    st.live,
                    t
                );
            }
            if st.next >= st.items.len() {
                st.phase = 3;
            }
        }
        _ => {
            info!("bake: done in {:.0} s: {} bundles written, {} already valid, {} live-only", started.elapsed().as_secs_f32(), st.written, st.skipped, st.live);
            exit.write(AppExit::Success);
            st.phase = 4;
        }
    }
}

/// Maps a recorded bundle's materials to keys and writes it.
fn write(ctx: &BakeCtx, b: Bundle<UntypedAssetId>, fx: &mut FxParams, written: &mut usize) {
    let (b, dropped) = b.map_materials(|id| id.try_typed::<fh1_remaster::material::RemasterMaterial>().ok().and_then(|id| fx.remaster.bake_key(id)));
    if dropped > 0 {
        warn!("bake: {}: {dropped} instances without a material key dropped", b.name);
    }
    match lib::write(&ctx.dir, &b, &ctx.stamp) {
        Ok(()) => *written += 1,
        Err(e) => error!("bake: {}: {e}", b.name),
    }
}

/// Whether every batch of template `n` is static-world geometry.
fn template_static(templates: &HashMap<u16, Vec<(Handle<Mesh>, BatchMaterial)>>, n: u16) -> bool {
    templates.get(&n).is_some_and(|b| !b.is_empty() && b.iter().all(|(m, _)| fh1_remaster::static_world::has_geometry(m.id())))
}

/// One prop tile: its fully static, non-smashable placements x every LOD level (ring 0, as `place_props`).
fn bake_tile(commands: &mut Commands, sc: &mut Scenery, fx: &mut FxParams, ctx: &BakeCtx, k: (i32, i32), st: &mut DriveState) {
    let Some(list) = read_placements(&sc.dir.join(&sc.props.files[&k])) else { return };
    let templates = sc.props.templates.take().unwrap_or_default();
    let smashable = sc.props.smashable.clone().unwrap_or_default();
    let default_chain = |n: u16| vec![(n, 0.0, PROP_DEFAULT_FADE * prop_lod_scale())];
    lib::begin(&tile_name(k));
    let parent = commands.spawn((Transform::IDENTITY, Visibility::Hidden)).id();
    let mut baked: Vec<u32> = Vec::new();
    for (i, &(n, m, normal, tint, lightmaps)) in list.iter().enumerate() {
        if smashable.contains(&n) {
            continue;
        }
        let scale = m.x_axis.truncate().length().max(m.y_axis.truncate().length()).max(m.z_axis.truncate().length());
        let mut chain = sc.props.lods.get(&n).cloned().unwrap_or_else(|| default_chain(n));
        if let (Some(last), Some(end)) = (chain.last_mut(), prop_far_end(sc.props.extent.get(&n).copied(), scale)) {
            last.2 = last.2.max(end);
        }
        if !chain.iter().all(|c| template_static(&templates, c.0)) {
            continue;
        }
        let levels = chain.len();
        for (level, (lod, from, to)) in chain.into_iter().enumerate() {
            let small = small_casters_off() && sc.props.radius.get(&lod).is_some_and(|r| r * scale < SMALL_CASTER_RADIUS);
            let lightmap = lightmaps.get(level).copied().filter(|&l| l != u32::MAX);
            let entry = LevelEntry { placement: i as u32, lod, m, normal, tint, lightmap, small, from, to, last: level + 1 == levels, spawned: Vec::new() };
            spawn_level(commands, sc, &templates, fx, parent, &entry);
        }
        baked.push(i as u32);
    }
    sc.props.templates = Some(templates);
    // The handles go with the parent (their records free themselves); the bundle keeps the recording.
    commands.entity(parent).despawn();
    let mut b = lib::end().unwrap_or_default();
    if baked.is_empty() {
        st.live += 1;
        return;
    }
    b.extra = baked;
    write(ctx, b, fx, &mut st.written);
}

/// One zone model: baked only when all its parts are static (else it stays live).
#[allow(clippy::too_many_arguments)]
fn bake_zone(commands: &mut Commands, sc: &mut Scenery, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, fx: &mut FxParams, ctx: &BakeCtx, n: u16, st: &mut DriveState) {
    let Some(z) = sc.zones.as_ref() else { return };
    let (file, cast) = match z.models.get(&n) {
        Some(m) => (m.file.clone(), m.shadow),
        None => return,
    };
    let Some(data) = block_on(sc.load_task(&file)) else { return };
    lib::begin(&zone_name(n));
    let parts = sc.prepare(data, meshes, materials, fx, false);
    let all_static = !parts.is_empty() && parts.iter().all(|(m, _)| fh1_remaster::static_world::has_geometry(m.id()));
    let zl = spawn_zone_model(commands, sc, cast, zone_fade_s(), parts);
    commands.entity(zl.parent).despawn();
    let b = lib::end().unwrap_or_default();
    if !all_static || b.instances.len() != zl.statics.len() || b.instances.is_empty() {
        st.live += 1;
        return;
    }
    write(ctx, b, fx, &mut st.written);
}
