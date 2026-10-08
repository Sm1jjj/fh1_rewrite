//! `scenery` group: Colorado's visual meshes (`.rmb.bin`) cut into 256 m tiles for streaming, with the
//! game's own materials (shader, constants, every texture slot) so a renderer can run the real shaders.
//!
//! Output (`scenery/colorado/`):
//! - `index.json`: tile size, per tile its file, bounds and counts; the converted textures
//!   (`{id, file, alpha}`).
//! - `materials.json`: `[{shader, technique, vs, ps, textures}]`, deduplicated across models. `shader` is the
//!   rmb's path (`shaders\track\<name>.fx`; its compiled `.fxobj` is in the `shaders` group). `vs` / `ps` are
//!   the material's float4 constants (VS c[3+k] / PS c[k], see docs/SHADERS.md). `textures[slot]` (= sampler
//!   register) is null for unused slots, else `{id, flags, file, word}` (`word` = the game's format word, see
//!   fh1setup textures.rs) with `file` null when the texture isn't on
//!   the disc and has no `.bundle` preview. PVS flags bit 0 (supplied at runtime): `file` is the 4x4
//!   `.bundle` preview when there is one (constant colour: black lightmaps, white AO; docs/LIGHTMAPS.md).
//! - `props/templates/<n>.bin` (tile format, LOCAL space): the `.rmb.bin` templates of the `.pgeo`
//!   prop placements (trees, bushes, fences, parked cars...); `props/tiles/<x>_<z>.bin`: placements (see
//!   `write_props`); listed under `props` in index.json with `lods` (per LOD0 template: LOD1/LOD2 template
//!   numbers, switch and fade distances from the game's LOD tables). Templates are not drawn in the world tiles.
//!   `props/glows.json` (index.json `props.glows`): the `.pgeo` light glows (night lamp sprites), see `write_glows`.
//! - `models/<n>.bin` (tile format, world space) + `zones.bin` (listed as `zones` in index.json): every model the
//!   game's PVS zones list, at its own LOD, and each zone's list; the engine draws exactly the current zone's set
//!   (docs/WORLD_LOD.md, `write_zones`). Tiles and `far.bin` remain for the fallback mode.
//! - `far.bin` (same format as a tile, listed as `far` in index.json): the distant backdrop, LOD0 of the
//!   `MIDDIST` / `_MID_` and `TERR_UberLOD` submodels, meant to be drawn at any distance.
//! - `tiles/<x>_<z>.bin`, little-endian: `b"FH1TILE4"`, u32 batch count, then per batch
//!   `u32 material, u32 flags, u32 attributes, u32 vertex count, u32 index count`, followed by that batch's
//!   arrays in this order: position f32x3, normal f32x3 (always), then if present in `attributes`:
//!   tangent f32x3 (`ATTR_TANGENT`), uv0 / uv1 / uv2 f32x2 (`ATTR_UV0..2`), colour 4 bytes as in the file
//!   (A, R, G, B; `ATTR_COLOR`), uv3 f32x2 (`ATTR_UV3`), binormal f32x3 (`ATTR_BINORMAL`); then u32 indices
//!   (triangle list). `ATTR_UV3` / `ATTR_BINORMAL` are only written for FM4 tracks, and a file that uses them starts
//!   `b"FH1TILE5"` instead (version 5 = version 4 + those two bits; Colorado output stays FH1TILE4). Texcoords have the mesh's
//!   uv offset/scale baked in (offset + raw * scale, all three sets, as the track vertex shaders do).
//!   Normals are the baked DEC3N ones (computed when the layout has none). Batch flags: `STANDIN`.
//! - `textures/<id:08x>.dds`: every texture a material slot names that is on the disc (`.bix` and 2D CAFF),
//!   BCn + mips, DX10 header, UNORM (the renderer picks sRGB per slot).
//!
//! Engine space (right-handed) as stored on disc: no axis flip, unlike the collision. Tiles hold the
//! full-detail submodels (LOD 0, `rmb::Class::Normal`, which includes the `*_NOLOD` ones). Bindings from the PVS (`fh1_formats::pvs`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use fh1_formats::zip::{Archive, Entry};
use fh1_formats::{bundle, fxobj, props, pvs, rmb};

pub const TILE: f32 = 256.0;
/// Tile key of the always-drawn distant set (`far.bin`).
const FAR: (i32, i32) = (i32::MAX, i32::MAX);
/// Tile keys `(PROP, model number)` hold prop templates (`props/templates/<n>.bin`, local space).
const PROP: i32 = i32::MIN;

/// Batch flag: distant stand-in geometry bound to the flat yellow placeholder texture `PLACEHOLDER`
/// (`FarTerrain_*` and ~240k triangles under other names); the only geometry on some far hillsides.
/// Draw it without its texture.
pub const STANDIN: u32 = 1;
/// The flat yellow placeholder diffuse (h_diff_1, slot 0) the stand-ins are bound to.
const PLACEHOLDER: u32 = 0x2CA9;

pub const ATTR_TANGENT: u32 = 1;
pub const ATTR_UV0: u32 = 2;
pub const ATTR_UV1: u32 = 4;
pub const ATTR_UV2: u32 = 8;
pub const ATTR_COLOR: u32 = 16;
/// FM4 only (tile version 5): fourth texcoord set and the binormal.
pub const ATTR_UV3: u32 = 32;
pub const ATTR_BINORMAL: u32 = 64;

#[derive(Default, Clone)]
struct Batch {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    tangents: Vec<[f32; 3]>,
    uvs: [Vec<[f32; 2]>; 3],
    colours: Vec<[u8; 4]>,
    uv3: Vec<[f32; 2]>,
    binormals: Vec<[f32; 3]>,
    indices: Vec<u32>,
}

/// Global materials, deduplicated by content.
#[derive(Default)]
struct Materials {
    list: Vec<Value>,
    by_key: HashMap<String, u32>,
}

impl Materials {
    fn get(&mut self, model: &rmb::TrackModel, mat: &rmb::Material, number: Option<usize>, pvs: &pvs::Pvs) -> u32 {
        let textures: Vec<Option<pvs::Texture>> = mat
            .texture_slots
            .iter()
            .map(|&s| if s < 0 { None } else { number.and_then(|n| pvs.texture(n, s)).copied() })
            .collect();
        let shader = model.shaders.get(mat.shader as usize).cloned().unwrap_or_default();
        let bits = |v: &[[f32; 4]]| v.iter().flatten().map(|f| f.to_bits()).collect::<Vec<_>>();
        let key = format!(
            "{shader}|{}|{:?}|{:?}|{:?}",
            mat.technique,
            bits(&mat.vs_constants),
            bits(&mat.ps_constants),
            textures.iter().map(|t| t.map(|t| (t.file_id, t.flags))).collect::<Vec<_>>()
        );
        if let Some(&i) = self.by_key.get(&key) {
            return i;
        }
        let i = self.list.len() as u32;
        self.list.push(json!({
            "shader": shader,
            "technique": mat.technique,
            "vs": mat.vs_constants,
            "ps": mat.ps_constants,
            "textures": textures.iter().map(|t| t.map(|t| json!({"id": t.file_id, "flags": t.flags}))).collect::<Vec<_>>(),
        }));
        self.by_key.insert(key, i);
        i
    }
}

/// A native-format track (FH1 Colorado, or another Horizon-engine track such as FH2's Anthem, docs/FH2_RECON.md).
pub struct TrackSrc {
    /// `media/tracks/<track>` on the disc.
    pub dir: std::path::PathBuf,
    /// `Ribbon_00/<stem>.pvs` / `.hex` (`Colorado_00`).
    pub stem: String,
    /// Model file prefix in `bin.zip`: `<prefix>.%05d.rmb.bin` (`coloradoout`; FH2 Anthem: `aout`).
    pub model_prefix: String,
    /// Output folder name under the group's output (`colorado`).
    pub name: String,
}

impl TrackSrc {
    pub fn colorado(disc: &Path) -> Self {
        Self { dir: disc.join("media/tracks/colorado"), stem: "Colorado_00".into(), model_prefix: "coloradoout".into(), name: "colorado".into() }
    }

    /// `media/tracks/<track>`: the `.pvs` stem from `Ribbon_00/*_00.pvs`, the model prefix from `bin.zip`'s names.
    #[cfg_attr(not(any(feature = "fh2", feature = "fm4")), allow(dead_code))]
    pub fn find(disc: &Path, track: &str, name: &str) -> Result<Self> {
        let dir = disc.join("media/tracks").join(track);
        let stem = std::fs::read_dir(dir.join("Ribbon_00"))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".pvs")).map(str::to_owned))
            .find(|n| n.ends_with("_00"))
            .ok_or_else(|| anyhow::anyhow!("no Ribbon_00/*_00.pvs in {}", dir.display()))?;
        Ok(Self { stem, ..Self::find_in(dir, name)? })
    }

    /// `dir` with the model prefix from its `bin.zip` (stem left empty).
    #[cfg_attr(not(any(feature = "fh2", feature = "fm4")), allow(dead_code))]
    fn find_in(dir: std::path::PathBuf, name: &str) -> Result<Self> {
        let ar = Archive::open(dir.join("bin.zip"))?;
        let model_prefix = ar
            .entries
            .iter()
            .find_map(|e| {
                let n = e.name.to_ascii_lowercase();
                let s = n.strip_suffix(".rmb.bin")?;
                let (prefix, num) = s.rsplit_once('.')?;
                num.parse::<u32>().is_ok().then(|| prefix.to_owned())
            })
            .ok_or_else(|| anyhow::anyhow!("no <prefix>.NNNNN.rmb.bin in {}/bin.zip", dir.display()))?;
        Ok(Self { dir, stem: String::new(), model_prefix, name: name.into() })
    }

    /// FM4 (docs/FM4_RECON.md): layout `ribbon` of `media/tracks/<track>` (`Ribbon_NN/<track>_NN.pvs`; every layout
    /// shares the track's `bin.zip`).
    #[cfg_attr(not(any(feature = "fh2", feature = "fm4")), allow(dead_code))]
    pub fn fm4(disc: &Path, track: &str, ribbon: u32, name: &str) -> Result<Self> {
        let dir = disc.join("media/tracks").join(track);
        let suffix = format!("_{ribbon:02}");
        let stem = std::fs::read_dir(dir.join(format!("Ribbon_{ribbon:02}")))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".pvs")).map(str::to_owned))
            .find(|n| n.ends_with(&suffix))
            .ok_or_else(|| anyhow::anyhow!("no Ribbon_{ribbon:02}/*{suffix}.pvs in {}", dir.display()))?;
        Ok(Self { stem, ..Self::find_in(dir, name)? })
    }

    /// The ribbon folder holding `<stem>.pvs`: `Ribbon_NN` from the stem's `_NN` suffix (`Colorado_00` -> `Ribbon_00`).
    pub fn ribbon_dir(&self) -> std::path::PathBuf {
        let nn = self.stem.rsplit('_').next().filter(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit())).unwrap_or("00");
        self.dir.join(format!("Ribbon_{nn}"))
    }

    pub fn model_file(&self, n: impl std::fmt::Display) -> String {
        format!("{}.{n:0>5}.rmb.bin", self.model_prefix)
    }
}

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    build_track(&TrackSrc::colorado(disc), out)
}

/// Writes `<out>/<src.name>/` (see the module docs).
pub fn build_track(src: &TrackSrc, out: &Path) -> Result<()> {
    let track = src.dir.clone();
    let mut ar = Archive::open(track.join("bin.zip"))?;
    let ribbon = src.ribbon_dir();
    let pvs_bytes = std::fs::read(ribbon.join(format!("{}.pvs", src.stem)))?;
    let pvs = pvs::parse(&pvs_bytes)?;
    // Forza Motorsport 4 circuits (docs/FM4_RECON.md) have no .pgeo props, CollObjs/GameObjs placements, .pvsz zones or
    // .hex grid: every model goes into the tiles, which the engine streams by distance (index.json "zones": null).
    let fm4 = pvs_bytes.get(4..8) == Some(&pvs::FM4_VERSION.to_be_bytes()[..]);
    // Props (`.pgeo` placements of origin-centred `.rmb.bin` templates; fh1_formats::props, docs/PROPS.md).
    // Their template models are written once in local space, not into the tiles.
    let (mut placements, unresolved) = if fm4 { (Vec::new(), 0) } else { props::track_placements(&mut ar, &pvs_bytes)? };
    let mut seen = HashSet::new();
    // Tile -> (material, flags, attributes) -> batch.
    let mut tiles: BTreeMap<(i32, i32), BTreeMap<(u32, u32, u32), Batch>> = BTreeMap::new();
    let mut by_name = HashMap::new();
    for e in &ar.entries {
        by_name.entry(e.name.to_ascii_lowercase().replace(char::from(92), "/")).or_insert_with(|| e.clone());
    }
    // CollObjs.xml smashables (signs, armco, bins, benches...) mapped to their templates through the PVS.
    let objs = if fm4 { Vec::new() } else { props::parse_obj_xml(&std::fs::read_to_string(ribbon.join("CollObjs.xml"))?) };
    let mut collobj_map = if fm4 {
        BTreeMap::new()
    } else {
        let cell = std::cell::RefCell::new(&mut ar);
        props::collobj_templates(&objs, &pvs_bytes, |n| {
            let e = by_name.get(&src.model_file(n))?.clone();
            let m = rmb::parse(&cell.borrow_mut().read(&e).ok()?).ok()?;
            Some(m.submodels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(";"))
        })?
    };
    // Free roam only: each object's .pvsz zone instance carries its activity ids (event-only cones, festival boards,
    // barrels laid across roads); FH1_EVENT_OBJECTS=1 at setup keeps every object. docs/PROPS.md. (fh1-rewrite-c1)
    let conds = if fm4 { props::ObjectConditions::new() } else { props::track_object_conditions(&mut ar, &pvs_bytes)? };
    for (t, old, new) in props::refine_collobj_templates(&objs, &mut collobj_map, &conds) {
        println!("[scenery] CollObjs {t}: template {old} -> {new} (zones)");
    }
    let all_objects = std::env::var("FH1_EVENT_OBJECTS").as_deref() == Ok("1");
    let smashables = if all_objects { props::collobj_placements(&objs, &collobj_map) } else { props::collobj_free_roam_placements(&objs, &collobj_map, &conds) };
    let smashable_count = smashables.len();
    placements.extend(smashables);
    // GameObjs.xml free-roam objects (discount-sign flyers, speed cameras, barn-find barns) and the
    // animated scenes' rest-pose templates (windmills, tractors, combine harvesters); docs/PROPS.md. (fh1-rewrite-b6)
    let game_xml = if fm4 { Vec::new() } else { props::parse_obj_xml(&std::fs::read_to_string(ribbon.join("GameObjs.xml"))?) };
    let gameobjs = if all_objects { props::gameobj_placements(&game_xml) } else { props::gameobj_free_roam_placements(&game_xml, &conds) };
    let gameobj_count = gameobjs.len();
    placements.extend(gameobjs);
    // The animated scenes (windmills, tractors, harvesters...) are drawn animated by the engine's anim.rs from the
    // game's Granny meshes (setup group `anim`, fh1-rewrite-10); their rest-pose templates are no longer placed here.
    let anim_count = 0usize;
    // The game's per-template LOD models and distances (.pgeo LOD tables; smashables have none).
    let mut distances = if fm4 { BTreeMap::new() } else { props::track_template_distances(&mut ar, &pvs_bytes)? };
    // PVS zone instances (.pvsz placement sections): festival marquees, grandstands, houses...
    // docs/PROPS.md. Templates the other paths place are skipped. (fh1-rewrite-b6)
    let already: HashSet<u16> = placements.iter().map(|p| p.model_number).collect();
    let (zone_placed, zone_distances) =
        if fm4 { (Vec::new(), BTreeMap::new()) } else { props::track_zone_instances(&mut ar, &pvs_bytes, |m| already.contains(&m))? };
    let zone_count = zone_placed.len();
    placements.extend(zone_placed);
    for (t, d) in zone_distances {
        distances.entry(t).or_insert(d);
    }
    // Smashables (docs/SMASH.md): whole-object template -> its shard templates (written as templates too).
    let smash = smash_table(&mut ar, &by_name, &objs, &collobj_map, src)?;
    let templates: HashSet<u16> = placements
        .iter()
        .map(|p| p.model_number)
        .chain(distances.values().flat_map(|d| [d.lod1_model, d.lod2_model]).flatten())
        .chain(smash.values().flat_map(|(_, shards)| shards.iter().copied()))
        .collect();
    // Template -> its first submodel name and bounds (for the collision table).
    let mut template_names: HashMap<u16, String> = HashMap::new();
    let mut template_bounds: HashMap<u16, ([f32; 3], [f32; 3])> = HashMap::new();
    // Vertex declarations by shader file (the submodel's first mesh's material picks its shader).
    let mut decls: HashMap<String, Option<fxobj::VertexDecl>> = Default::default();
    let mut materials = Materials::default();
    let mut baked_normals = 0usize;
    let cubes = std::env::var_os("FH1_SCENERY_CUBES").is_some();
    let mut far_pieces: Vec<BTreeMap<(u32, u32, u32), Batch>> = Vec::new();
    let (mut models, mut subs, mut tris, mut failed) = (0usize, 0usize, 0usize, 0usize);
    // The game's PVS zones (docs/WORLD_LOD.md): every model a zone lists, except prop templates, is written
    // on its own (`models/<n>.bin`) so the engine can draw exactly the zone's set.
    let zones = if fm4 { Zones::none() } else { read_zones(&mut ar, &by_name, &ribbon, &src.stem, &pvs_bytes)? };
    let keep_fm4_sky = std::env::var("FH1_FM4_SKYDOME").is_ok_and(|v| v == "1");
    let mut skipped_sky = 0usize;
    let zone_models: HashSet<u16> = zones.lists.iter().flatten().copied().filter(|n| !templates.contains(n)).collect();
    let dir = out.join(&src.name);
    std::fs::create_dir_all(dir.join("models"))?;
    let mut model_info: Vec<ZoneModel> = Vec::new();
    let mut skipped_local = 0usize;
    for e in ar.entries.clone() {
        let name = e.name.to_ascii_lowercase();
        if !name.ends_with(".rmb.bin") || !seen.insert(name.clone()) {
            continue;
        }
        // `coloradoout.NNNNN.rmb.bin`: N picks the model's texture bindings in the PVS.
        let number = name.strip_suffix(".rmb.bin").and_then(|s| s.rsplit('.').next()).and_then(|s| s.parse::<usize>().ok());
        let model = match rmb::parse(&ar.read(&e)?) {
            Ok(m) => m,
            Err(err) => {
                failed += 1;
                println!("[scenery] {}: {err}", e.name);
                continue;
            }
        };
        models += 1;
        // A template with no LOD0 geometry (e.g. a gate whose only mesh is named `_LOD001`) uses its lowest LOD.
        let is_template = number.and_then(|n| u16::try_from(n).ok()).is_some_and(|n| templates.contains(&n));
        if let (true, Some(n), Some(s)) = (is_template, number.and_then(|n| u16::try_from(n).ok()), model.submodels.first()) {
            template_names.insert(n, s.name.clone());
        }
        let base_lod = if is_template {
            model.submodels.iter().filter(|s| s.class() == rmb::Class::Normal && !s.positions.is_empty()).map(|s| s.lod()).min().unwrap_or(0)
        } else {
            0
        };
        // Zone-drawn model: its drawable submodels at its own (lowest) LOD level (files are one LOD each).
        // Origin-centred models are local-space templates the game places per instance (props path); drawn as zone
        // models they would pile up at the world origin.
        let (lo, hi) = (model.bounds_min, model.bounds_max);
        let local = (lo[0] + hi[0]).abs() < 2.0 && (lo[2] + hi[2]).abs() < 2.0 && hi[0] - lo[0] < 400.0;
        let zone_model = number.and_then(|n| u16::try_from(n).ok()).filter(|n| zone_models.contains(n) && !local);
        skipped_local += (local && number.and_then(|n| u16::try_from(n).ok()).is_some_and(|n| zone_models.contains(&n))) as usize;
        let drawable = |s: &rmb::SubModel| matches!(s.class(), rmb::Class::Normal | rmb::Class::MidDistance | rmb::Class::UberLod);
        let model_lod = model.submodels.iter().filter(|s| drawable(s) && !s.positions.is_empty()).map(|s| s.lod()).min();
        let mut model_batches: BTreeMap<(u32, u32, u32), Batch> = BTreeMap::new();
        let mut model_group = None;
        for s in &model.submodels {
            // Full detail goes into 256 m tiles; mid-distance / uber-LOD backdrop (LOD0) into `far.bin`.
            // FH1_SCENERY_CUBES=1 also draws TERR_CUBE_* (purpose unconfirmed; for inspection).
            let class = s.class();
            let near = class == rmb::Class::Normal || (class == rmb::Class::TerrainCube && cubes);
            let far = matches!(class, rmb::Class::MidDistance | rmb::Class::UberLod);
            let in_scenery = s.lod() == base_lod && (near || far);
            let in_model = zone_model.is_some() && drawable(s) && Some(s.lod()) == model_lod;
            if !(in_scenery || in_model) || s.positions.is_empty() {
                continue;
            }
            // FM4 sky domes (SKY_* shaders, e.g. Sebring's SKY_Sebr_Skydome_001 on SKY_DIFF_1): FM4 draws them around the
            // camera (TrackSettings SkyboxHeightOffset); as world scenery their lower edge floats as a band across the
            // sky, and FH1's own sky already fills it. Skipped; FH1_FM4_SKYDOME=1 (setup) keeps them.
            if fm4 && !keep_fm4_sky && model.submodel_shader(s).is_some_and(|sh| sh.rsplit([char::from(92), '/']).next().is_some_and(|f| f.to_ascii_uppercase().starts_with("SKY_"))) {
                skipped_sky += 1;
                continue;
            }
            if in_model && model_group.is_none() {
                model_group = Some(strip_lod(&s.name));
            }
            let template = number.and_then(|n| u16::try_from(n).ok()).filter(|n| templates.contains(n));
            let key = match template {
                Some(n) => (PROP, n as i32),
                None if far => FAR,
                None => ((s.offset[0] / TILE).floor() as i32, (s.offset[2] / TILE).floor() as i32),
            };
            // Each submodel builds its own batches, then goes to its tile / the far set / its model file.
            let mut own: BTreeMap<(u32, u32, u32), Batch> = BTreeMap::new();
            let far = far && template.is_none();
            let tile = &mut own;
            let all: Vec<u32> = s.meshes.iter().flat_map(|m| m.indices.iter().copied()).collect();
            let decl = model.submodel_shader(s).and_then(|sh| {
                let file = format!("shaders/track/{}obj", sh.rsplit([char::from(92), '/']).next()?.to_ascii_lowercase());
                decls
                    .entry(file.clone())
                    .or_insert_with(|| by_name.get(&file).and_then(|e| ar.read(e).ok()).and_then(|d| fxobj::vertex_decl(&d).ok()))
                    .clone()
            });
            let attrs = decl.filter(|d| d.stride() == s.stride).map(|d| s.attributes(&d)).unwrap_or_default();
            let n = s.positions.len();
            let normals = if attrs.normals.len() == n {
                baked_normals += 1;
                attrs.normals
            } else {
                smooth_normals(&s.positions, &all)
            };
            let mut mask = 0;
            for (bit, len) in [
                (ATTR_TANGENT, attrs.tangents.len()),
                (ATTR_UV0, attrs.uv0.len()),
                (ATTR_UV1, attrs.uv1.len()),
                (ATTR_UV2, attrs.uv2.len()),
                (ATTR_COLOR, attrs.colors.len()),
                (if fm4 { ATTR_UV3 } else { 0 }, attrs.uv3.len()),
                (if fm4 { ATTR_BINORMAL } else { 0 }, attrs.binormals.len()),
            ] {
                if len == n {
                    mask |= bit;
                }
            }
            let farterrain = s.name.to_ascii_uppercase().starts_with("FARTERRAIN");
            let raw_uvs = [&attrs.uv0, &attrs.uv1, &attrs.uv2];
            // Each mesh gets its own copy of the vertices it uses (uv offset/scale is per mesh).
            for m in &s.meshes {
                if in_scenery {
                    tris += m.indices.len() / 3;
                }
                let Some(mat) = model.materials.get(m.material as usize) else { continue };
                let material = materials.get(&model, mat, number, &pvs);
                let placeholder = number.zip(mat.texture_slots.first()).and_then(|(n, &s0)| pvs.texture(n, s0)).is_some_and(|t| t.file_id == PLACEHOLDER);
                let flags = if farterrain || placeholder { STANDIN } else { 0 };
                let os = m.uv_offset_scale;
                let os = if os.iter().all(|v| v.is_finite()) { os } else { [0.0, 0.0, 1.0, 1.0] };
                let b = tile.entry((material, flags, mask)).or_default();
                let mut remap = HashMap::new();
                for &i in &m.indices {
                    let v = *remap.entry(i).or_insert_with(|| {
                        let i = i as usize;
                        b.positions.push(s.positions[i]);
                        b.normals.push(normals[i]);
                        if mask & ATTR_TANGENT != 0 {
                            b.tangents.push(attrs.tangents[i]);
                        }
                        for k in 0..3 {
                            if mask & (ATTR_UV0 << k) != 0 {
                                let uv = raw_uvs[k][i];
                                b.uvs[k].push([os[0] + uv[0] * os[2], os[1] + uv[1] * os[3]]);
                            }
                        }
                        if mask & ATTR_COLOR != 0 {
                            let c = attrs.colors[i];
                            b.colours.push([c[3], c[0], c[1], c[2]]);
                        }
                        if mask & ATTR_UV3 != 0 {
                            // Same uv offset/scale as the other sets (FM4's track vertex shaders not checked).
                            let uv = attrs.uv3[i];
                            b.uv3.push([os[0] + uv[0] * os[2], os[1] + uv[1] * os[3]]);
                        }
                        if mask & ATTR_BINORMAL != 0 {
                            b.binormals.push(attrs.binormals[i]);
                        }
                        b.positions.len() as u32 - 1
                    });
                    b.indices.push(v);
                }
            }
            if in_model {
                if in_scenery {
                    append_batches(&mut model_batches, own.clone());
                } else {
                    append_batches(&mut model_batches, std::mem::take(&mut own));
                }
            }
            if in_scenery {
                subs += 1;
                if far {
                    far_pieces.push(own);
                } else {
                    append_batches(tiles.entry(key).or_default(), own);
                }
            }
        }
        if let (Some(n), Some(lod), false) = (zone_model, model_lod, model_batches.is_empty()) {
            let file = format!("models/{n}.bin");
            let (min, max, _, _) = write_tile(&dir.join(&file), &model_batches)?;
            model_info.push(ZoneModel { n, file, lod, group: model_group.unwrap_or_default(), min, max, shadow: zones.casters.contains(&n), range: zones.ranges.get(&n).copied() });
        }
    }
    println!(
        "[scenery] zones: {} zones, {} zone-drawn models written, {skipped_local} origin-centred templates left to the props path",
        zones.lists.len(),
        model_info.len()
    );

    // The backdrop overlaps the drivable area in places (a mid-distance wall right by the Colorado start).
    // Keep a far piece only when most of its vertices lie outside the tiles that hold full detail.
    // (UNVERIFIED stand-in for the game's distance-based LOD switching.)
    let near_tiles: HashSet<(i32, i32)> = tiles.keys().copied().filter(|k| k.0 != PROP).collect();
    let (mut far_kept, mut far_dropped) = (0usize, 0usize);
    for piece in far_pieces {
        let (inside, total) = piece.values().flat_map(|b| &b.positions).fold((0usize, 0usize), |(i, t), p| {
            let k = ((p[0] / TILE).floor() as i32, (p[2] / TILE).floor() as i32);
            (i + near_tiles.contains(&k) as usize, t + 1)
        });
        if total == 0 || inside * 2 > total {
            far_dropped += 1;
            continue;
        }
        far_kept += 1;
        append_batches(tiles.entry(FAR).or_default(), piece);
    }
    println!("[scenery] far set: {far_kept} pieces kept, {far_dropped} dropped (mostly over full-detail tiles)");
    let zone_index = if fm4 { Value::Null } else { write_zones(&dir, &zones, &model_info)? };

    std::fs::create_dir_all(dir.join("tiles"))?;
    std::fs::create_dir_all(dir.join("textures"))?;
    // Every texture any material slot names that has a file on the disc.
    let mut wanted = BTreeMap::new();
    for m in &materials.list {
        for t in m["textures"].as_array().into_iter().flatten().filter(|t| !t.is_null()) {
            let (id, flags) = (t["id"].as_u64().unwrap_or(0) as u32, t["flags"].as_u64().unwrap_or(1) as u32);
            if flags & 1 == 0 {
                wanted.insert(id, flags & 4 != 0);
            }
        }
    }
    // Light glows (.pgeo type 6; docs/PROPS.md "Light glows"): their sprite textures are converted with the rest.
    let glows = props::track_glows(&mut ar)?;
    for t in glows.iter().flat_map(|g| &g.glows).flat_map(|l| l.textures.into_iter().flatten()) {
        if let Some(t) = pvs.textures.get(t as usize).filter(|t| t.flags & 1 == 0) {
            wanted.insert(t.file_id, t.flags & 4 != 0);
        }
    }
    let mut textures = convert_textures(&mut ar, &by_name, &wanted, &dir.join("textures"))?;
    // PVS runtime textures (flags bit 0, no file): the disc only has their .bundle previews (docs/LIGHTMAPS.md).
    let runtime: HashSet<u32> = materials
        .list
        .iter()
        .flat_map(|m| m["textures"].as_array().cloned().unwrap_or_default())
        .filter(|t| t["flags"].as_u64().is_some_and(|f| f & 1 != 0))
        .filter_map(|t| t["id"].as_u64().map(|v| v as u32))
        .collect();
    textures.extend(convert_previews(&mut ar, &pvs, &runtime, &dir.join("textures"))?);
    // Per-instance night lightmaps of the zone placements (props::TrackPlacement::lightmaps; docs/LIGHTMAPS.md
    // "Per-instance lightmaps"). Only those with a file on the disc: the runtime ones' previews are blank 4x4s.
    let lightmaps: BTreeMap<u32, bool> = placements
        .iter()
        .flat_map(|p| p.lightmaps)
        .filter_map(|i| pvs.textures.get(i as usize))
        .filter(|t| t.flags & 1 == 0 && !textures.contains_key(&t.file_id))
        .map(|t| (t.file_id, t.flags & 4 != 0))
        .collect();
    let lightmap_count = lightmaps.len();
    textures.extend(convert_textures(&mut ar, &by_name, &lightmaps, &dir.join("textures"))?);
    println!("[scenery] per-instance lightmaps: {lightmap_count} textures converted");
    for m in &mut materials.list {
        for t in m["textures"].as_array_mut().into_iter().flatten().filter(|t| !t.is_null()) {
            let id = t["id"].as_u64().unwrap_or(0) as u32;
            match textures.get(&id) {
                Some(&(_, word)) => {
                    t["file"] = json!(format!("textures/{id:08x}.dds"));
                    t["word"] = json!(word);
                }
                None => t["file"] = Value::Null,
            }
        }
    }

    let mut index = Vec::new();
    let (mut batches, mut textured_tris) = (0usize, 0usize);
    let mut far = None;
    std::fs::create_dir_all(dir.join("props/templates"))?;
    std::fs::create_dir_all(dir.join("props/tiles"))?;
    let mut written_templates = Vec::new();
    for (&(x, z), t) in &tiles {
        let file = match (x, z) {
            FAR => "far.bin".to_owned(),
            (PROP, n) => format!("props/templates/{n}.bin"),
            _ => format!("tiles/{x}_{z}.bin"),
        };
        for (&(material, flags, _), batch) in t {
            batches += 1;
            let diffuse = materials.list[material as usize]["textures"][0]["file"].is_string();
            if diffuse && flags & STANDIN == 0 {
                textured_tris += batch.indices.len() / 3;
            }
        }
        let (lo, hi, nv, ni) = write_tile(&dir.join(&file), t)?;
        let entry = json!({"x": x, "z": z, "file": file, "min": lo, "max": hi, "vertices": nv, "triangles": ni / 3});
        if (x, z) == FAR {
            far = Some(entry);
        } else if x == PROP {
            written_templates.push(z as u16);
            template_bounds.insert(z as u16, (lo, hi));
        } else {
            index.push(entry);
        }
    }
    let props = write_props(&dir, &placements, &written_templates, &pvs, &textures)?;
    let glow_index = write_glows(&dir, &glows, &pvs, &textures)?;
    // The game's prop rigid bodies (docs/SMASH.md "Physics definitions"); optional so a missing file only loses shapes.
    let physics = match std::fs::read(track.join("PhysicsDefinitions.bin")).map(|d| props::physdef::parse(&d)) {
        Ok(Ok(p)) => Some(p),
        Ok(Err(e)) => {
            println!("[scenery] PhysicsDefinitions.bin: {e}");
            None
        }
        Err(_) => None,
    };
    // Per LOD0 template: its LOD models and switch / cull distances (metres), from the game's LOD tables.
    let lods: Vec<serde_json::Value> = distances
        .iter()
        .map(|(n, d)| {
            json!({"n": n, "lod1": d.lod1_model, "lod2": d.lod2_model, "lod1_m": d.lod1_m, "lod2_m": d.lod2_m,
                   "fade_m": d.cull_m, "from_table": d.from_lod_table})
        })
        .collect();
    println!(
        "[scenery] props: {} placements ({smashable_count} CollObjs smashables, {gameobj_count} GameObjs, {anim_count} animated at rest, {zone_count} zone-placed) of {} templates in {} tiles ({unresolved} unresolved in the .pgeo, {} without a template)",
        props.0,
        written_templates.len(),
        props.1.len(),
        placements.len() - props.0
    );
    let tex_index: Vec<_> =
        textures.iter().map(|(id, (alpha, _))| json!({"id": id, "file": format!("textures/{id:08x}.dds"), "alpha": alpha})).collect();
    let mut index_json = json!({
            "tile_size": TILE, "tiles": index, "far": far, "textures": tex_index, "zones": zone_index,
            "props": {"tile_size": TILE, "tiles": props.1, "templates": written_templates, "lods": lods,
                      "collision": collision_table(&smash, &template_names, &template_bounds, physics.as_ref()), "glows": glow_index},
        });
    // FM4: tf7 is the daytime lightmap and its runtime atlas tiles have no file -> white in fh1-render, not FH1's black.
    if fm4 {
        index_json["runtime_lightmap"] = json!("white");
        // FM4 samples its DXT5A (BC4) masks through .y/.w: fh1-render replicates the channel.
        index_json["single_channel"] = json!("replicate");
    }
    std::fs::write(dir.join("index.json"), serde_json::to_vec_pretty(&index_json)?)?;
    std::fs::write(dir.join("materials.json"), serde_json::to_vec(&materials.list)?)?;
    println!(
        "[scenery] {}: {models} models ({failed} failed), {subs} full-detail submodels ({baked_normals} with baked normals), \
         {tris} triangles ({textured_tris} with a diffuse), {} materials, {batches} batches, {} textures in {} tiles",
        src.name,
        materials.list.len(),
        textures.len(),
        tiles.len()
    );
    if fm4 {
        println!("[scenery] {}: {skipped_sky} FM4 sky-dome submodels skipped (FH1_FM4_SKYDOME=1 keeps them)", src.name);
    }
    Ok(())
}

/// `props/glows.json` (index.json `props.glows` = {file, groups, glows}): the light-glow groups, ENGINE space
/// (z negated). Per group {name, event (null = free roam), glows: [{kind "cone"|"halo", tex: [file|null; 2],
/// flags, pos, dir, params [8], colour (u32 RGBx)}]}; fields as `fh1_formats::props::LightGlow`.
fn write_glows(dir: &Path, groups: &[props::LightGlows], pvs: &pvs::Pvs, textures: &BTreeMap<u32, (bool, u32)>) -> Result<Value> {
    let file = |t: Option<u32>| {
        let id = pvs.textures.get(t? as usize)?.file_id;
        textures.contains_key(&id).then(|| format!("textures/{id:08x}.dds"))
    };
    let flip = |v: [f32; 3]| [v[0], v[1], -v[2]];
    let list: Vec<Value> = groups
        .iter()
        .map(|g| {
            let glows: Vec<Value> = g
                .glows
                .iter()
                .map(|l| {
                    json!({"kind": match l.kind { props::GlowKind::Cone => "cone", props::GlowKind::Halo => "halo" },
                           "tex": [file(l.textures[0]), file(l.textures[1])], "flags": l.flags,
                           "pos": flip(l.position), "dir": flip(l.direction), "params": l.params, "colour": l.colour})
                })
                .collect();
            json!({"name": g.name, "event": g.event, "glows": glows})
        })
        .collect();
    let n: usize = groups.iter().map(|g| g.glows.len()).sum();
    std::fs::write(dir.join("props/glows.json"), serde_json::to_vec(&list)?)?;
    println!("[scenery] glows: {} groups ({} event-only), {n} glows", groups.len(), groups.iter().filter(|g| g.event.is_some()).count());
    Ok(json!({"file": "props/glows.json", "groups": groups.len(), "glows": n}))
}

/// Prop placements per 256 m tile: `props/tiles/<x>_<z>.bin` = `b"FH1PROP3"`, u32 count, then per
/// placement `u32 template model number, f32x16 column-major engine-space matrix, f32x3 ground normal (engine
/// space), u32 tint (D3DCOLOR), u32 x 2 night lightmap of the LOD0 / LOD1 template (texture file id as in
/// materials.json = `textures/<id>.dds`, u32::MAX = none)` (LE, 92 bytes). FH1PROP2 = the same without the
/// lightmaps (84). Only placements whose template was written. Returns (placements written, index entries).
fn write_props(
    dir: &Path,
    placements: &[props::TrackPlacement],
    templates: &[u16],
    pvs: &pvs::Pvs,
    textures: &BTreeMap<u32, (bool, u32)>,
) -> Result<(usize, Vec<serde_json::Value>)> {
    // PVS texture index -> converted file id.
    let lightmap = |i: u32| pvs.textures.get(i as usize).map(|t| t.file_id).filter(|id| textures.contains_key(id)).unwrap_or(u32::MAX);
    let have: HashSet<u16> = templates.iter().copied().collect();
    let mut tiles: BTreeMap<(i32, i32), Vec<&props::TrackPlacement>> = BTreeMap::new();
    for p in placements.iter().filter(|p| have.contains(&p.model_number)) {
        let k = ((p.matrix[12] / TILE).floor() as i32, (p.matrix[14] / TILE).floor() as i32);
        tiles.entry(k).or_default().push(p);
    }
    let (mut n, mut index) = (0, Vec::new());
    for ((x, z), list) in tiles {
        let mut b = Vec::with_capacity(12 + list.len() * 92);
        b.extend_from_slice(b"FH1PROP3");
        b.extend_from_slice(&(list.len() as u32).to_le_bytes());
        for p in &list {
            b.extend_from_slice(&(p.model_number as u32).to_le_bytes());
            p.matrix.iter().for_each(|v| b.extend_from_slice(&v.to_le_bytes()));
            p.normal.iter().for_each(|v| b.extend_from_slice(&v.to_le_bytes()));
            b.extend_from_slice(&p.tint.to_le_bytes());
            p.lightmaps.iter().for_each(|&i| b.extend_from_slice(&lightmap(i).to_le_bytes()));
        }
        let file = format!("props/tiles/{x}_{z}.bin");
        std::fs::write(dir.join(&file), b)?;
        n += list.len();
        index.push(json!({"x": x, "z": z, "file": file, "count": list.len()}));
    }
    Ok((n, index))
}

/// Runtime textures (PVS flags bit 0) -> DDS from their `.bundle` previews (keyed by PVS texture index).
/// Returns file id -> (has alpha, format word). (fh1-rewrite-d8)
fn convert_previews(ar: &mut Archive<std::fs::File>, pvs: &pvs::Pvs, ids: &HashSet<u32>, dir: &Path) -> Result<BTreeMap<u32, (bool, u32)>> {
    let index: HashMap<u32, u32> =
        pvs.textures.iter().enumerate().filter(|(_, t)| ids.contains(&t.file_id)).map(|(i, t)| (i as u32, t.file_id)).collect();
    let (mut done, mut seen) = (BTreeMap::new(), HashSet::new());
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !n.ends_with(".bundle") || !seen.insert(n) {
            continue;
        }
        let d = ar.read(&e)?;
        for r in bundle::parse(&d)? {
            let Some(&id) = index.get(&r.texture_index) else { continue };
            if done.contains_key(&id) {
                continue;
            }
            let c = crate::textures::preview_to_dds(bundle::decode_mips(&d, &r)?, r.raw_format)?;
            std::fs::write(dir.join(format!("{id:08x}.dds")), &c.dds)?;
            done.insert(id, (c.alpha, c.word));
        }
    }
    Ok(done)
}

/// Converts the wanted textures (id -> is `.bix`) to DDS, in parallel. Returns file id -> (has alpha, format word).
fn convert_textures(
    ar: &mut Archive<std::fs::File>,
    by_name: &HashMap<String, Entry>,
    ids: &BTreeMap<u32, bool>,
    dir: &Path,
) -> Result<BTreeMap<u32, (bool, u32)>> {
    use rayon::prelude::*;
    let mut done = BTreeMap::new();
    let ids: Vec<(u32, bool)> = ids.iter().map(|(k, v)| (*k, *v)).collect();
    // The archive reader isn't shared across threads: read a chunk, convert it in parallel.
    for chunk in ids.chunks(64) {
        let mut raw = Vec::new();
        for &(id, is_bix) in chunk {
            if is_bix {
                if let (Some(h), Some(b)) = (by_name.get(&format!("_0x{id:08x}.bix")), by_name.get(&format!("_0x{id:08x}_b.bix"))) {
                    raw.push((id, ar.read(h)?, Some(ar.read(b)?)));
                }
            } else if let Some(e) = by_name.get(&format!("_0x{id:08x}.bin")) {
                raw.push((id, ar.read(e)?, None));
            }
        }
        let out: Vec<_> = raw
            .par_iter()
            .map(|(id, a, b)| {
                let r = match b {
                    Some(b) => crate::textures::bix_to_dds(a, b),
                    None => crate::textures::caff_to_dds(a),
                };
                (*id, r)
            })
            .collect();
        for (id, r) in out {
            match r {
                Ok(c) => {
                    std::fs::write(dir.join(format!("{id:08x}.dds")), &c.dds)?;
                    done.insert(id, (c.alpha, c.word));
                }
                Err(e) => println!("[scenery] texture _0x{id:08X}: {e}"),
            }
        }
    }
    Ok(done)
}

/// Area-weighted vertex normals, oriented upwards where ambiguous (strip winding varies).
fn smooth_normals(pos: &[[f32; 3]], idx: &[u32]) -> Vec<[f32; 3]> {
    let mut n = vec![[0.0f32; 3]; pos.len()];
    for t in idx.chunks_exact(3) {
        let [a, b, c] = [t[0], t[1], t[2]].map(|i| pos[i as usize]);
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let mut f = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        if f[1] < 0.0 {
            f = [-f[0], -f[1], -f[2]];
        }
        for &i in t {
            for k in 0..3 {
                n[i as usize][k] += f[k];
            }
        }
    }
    for v in &mut n {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        *v = if l > 1e-12 { [v[0] / l, v[1] / l, v[2] / l] } else { [0.0, 1.0, 0.0] };
    }
    n
}

/// Appends `src`'s batches onto `dst` (same keys merge, indices rebased).
fn append_batches(dst: &mut BTreeMap<(u32, u32, u32), Batch>, src: BTreeMap<(u32, u32, u32), Batch>) {
    for (k, mut b) in src {
        let d = dst.entry(k).or_default();
        let base = d.positions.len() as u32;
        d.positions.append(&mut b.positions);
        d.normals.append(&mut b.normals);
        d.tangents.append(&mut b.tangents);
        for i in 0..3 {
            d.uvs[i].append(&mut b.uvs[i]);
        }
        d.colours.append(&mut b.colours);
        d.uv3.append(&mut b.uv3);
        d.binormals.append(&mut b.binormals);
        d.indices.extend(b.indices.iter().map(|i| i + base));
    }
}

/// Writes one `FH1TILE4` file (format in the module docs). Returns (min, max, vertices, indices).
fn write_tile(path: &Path, t: &BTreeMap<(u32, u32, u32), Batch>) -> Result<([f32; 3], [f32; 3], usize, usize)> {
    let mut b = Vec::new();
    let v5 = t.keys().any(|k| k.2 & (ATTR_UV3 | ATTR_BINORMAL) != 0);
    b.extend_from_slice(if v5 { b"FH1TILE5" } else { b"FH1TILE4" });
    b.extend_from_slice(&(t.len() as u32).to_le_bytes());
    let (mut lo, mut hi, mut nv, mut ni) = ([f32::MAX; 3], [f32::MIN; 3], 0usize, 0usize);
    for (&(material, flags, mask), batch) in t {
        for v in [material, flags, mask, batch.positions.len() as u32, batch.indices.len() as u32] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let f3 = |b: &mut Vec<u8>, v: &[[f32; 3]]| v.iter().flatten().for_each(|c| b.extend_from_slice(&c.to_le_bytes()));
        f3(&mut b, &batch.positions);
        f3(&mut b, &batch.normals);
        f3(&mut b, &batch.tangents);
        for uv in &batch.uvs {
            uv.iter().flatten().for_each(|c| b.extend_from_slice(&c.to_le_bytes()));
        }
        batch.colours.iter().for_each(|c| b.extend_from_slice(c));
        batch.uv3.iter().flatten().for_each(|c| b.extend_from_slice(&c.to_le_bytes()));
        f3(&mut b, &batch.binormals);
        batch.indices.iter().for_each(|i| b.extend_from_slice(&i.to_le_bytes()));
        for p in &batch.positions {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        nv += batch.positions.len();
        ni += batch.indices.len();
    }
    std::fs::write(path, b)?;
    Ok((lo, hi, nv, ni))
}

/// A submodel name without its LOD token (`Mountains_Area04B_Terrain_LOD00_48` -> `MOUNTAINS_AREA04B_TERRAIN_48`):
/// the separate LOD models of one object share it.
fn strip_lod(name: &str) -> String {
    let u = name.to_ascii_uppercase();
    let mut out = String::new();
    let mut rest = u.as_str();
    while let Some(at) = rest.find("_LOD") {
        out.push_str(&rest[..at]);
        let tail = rest[at + 4..].trim_start_matches('_');
        rest = tail.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    out.push_str(rest);
    out
}

/// A model drawn through the PVS zones.
struct ZoneModel {
    n: u16,
    file: String,
    /// LOD level from the name (0 when it has no `_LODnn`).
    lod: u32,
    /// [`strip_lod`] of its first drawn submodel: the LOD models of one object.
    group: String,
    min: [f32; 3],
    max: [f32; 3],
    /// Casts sun shadows (`.pvs` record byte 4 bit 0x08; docs/SHADOWS.md, INFERRED). (fh1-rewrite-d5)
    shadow: bool,
    /// The game's LOD slot range (metres from the bounds; end `None` = never culled; `true` = the end is the
    /// object's cull, not a switch to a coarser LOD), from `.pvsz` entries.
    range: Option<(f32, Option<f32>, bool)>,
}

/// The track's PVS zones (docs/WORLD_LOD.md): a 100 m flat-top hex grid over the road network
/// (`Ribbon_00/<Track>_00.hex`), each cell one zone, whose `.pvsz` lists the `.pvs` draw records visible there.
struct Zones {
    /// Hex size (centre to corner).
    size: f32,
    /// Grid origin x / z (collision space).
    origin: [f32; 2],
    cols: u32,
    rows: u32,
    /// Cell (row-major) -> zone index, u32::MAX = none.
    cells: Vec<u32>,
    /// Zone -> model numbers of the records it lists that are drawn in the main view (sorted, unique).
    lists: Vec<Vec<u16>>,
    /// Models with a `.pvs` record whose byte 4 has 0x08 (casts sun shadows; docs/SHADOWS.md, INFERRED).
    casters: HashSet<u16>,
    /// Model -> the LOD slot range its world-space records draw in (docs/WORLD_LOD.md "`.pvsz` grammar"):
    /// from the end of the slot before its first band to the end of its last band (`None` = NaN = no cull).
    ranges: HashMap<u16, (f32, Option<f32>, bool)>,
}

impl Zones {
    /// No zone grid (FM4 circuits).
    fn none() -> Self {
        Self { size: 0.0, origin: [0.0; 2], cols: 0, rows: 0, cells: Vec::new(), lists: Vec::new(), casters: HashSet::new(), ranges: HashMap::new() }
    }

    /// Zone centre in engine space (x, z). Flat-top hexes, odd columns shifted half a row (+z collision);
    /// checked on the disc: zones' listed LOD0 roads lie 12 m from these centres on average.
    fn centre(&self, col: u32, row: u32) -> [f32; 2] {
        let w = self.size * 3f32.sqrt();
        let x = self.origin[0] + col as f32 * self.size * 1.5 + self.size;
        let z = self.origin[1] + row as f32 * w + w * 0.5 + if col % 2 == 1 { w * 0.5 } else { 0.0 };
        [x, -z]
    }
}

/// Reads `<Track>_00.hex` (`HEXY`), `PVSZLookup_00.dat` (FilenameMap index, zone), `FilenameMap_00.dat` and every
/// zone's `.pvsz`. A `.pvsz` starts with `u32 n` + n x u32 `.pvs` 18-byte draw record indices; the high bit marks
/// the near set (median 0.5 km from the zone vs 1.5 km without it; fh1-rewrite-10 spotted that both are records).
/// Records kept: draw-band bits (byte 6 & 0x38) set and not cube-map-only
/// (byte 4 bit 0, the TERR_CUBE_* records).
fn read_zones(ar: &mut Archive<std::fs::File>, by_name: &HashMap<String, Entry>, ribbon: &Path, stem: &str, pvs_bytes: &[u8]) -> Result<Zones> {
    let be = |b: &[u8], o: usize| u32::from_be_bytes(b[o..o + 4].try_into().unwrap());
    let hex = std::fs::read(ribbon.join(format!("{stem}.hex")))?;
    anyhow::ensure!(hex.get(..4) == Some(&b"HEXY"[..]), "{stem}.hex: HEXY expected");
    let f = |o: usize| f32::from_bits(be(&hex, o));
    let (size, origin, nzones, cols, rows) = (f(8), [f(12), f(16)], be(&hex, 20) as usize, be(&hex, 24), be(&hex, 28));
    let cells: Vec<u32> = (0..(cols * rows) as usize).map(|i| be(&hex, 32 + i * 4)).collect();
    let fm = std::fs::read(ribbon.join("FilenameMap_00.dat"))?;
    let names: Vec<String> = (0..be(&fm, 0) as usize / 4)
        .map(|i| {
            let o = be(&fm, i * 4) as usize;
            let end = fm[o..].iter().position(|&c| c == 0).map_or(fm.len(), |e| o + e);
            String::from_utf8_lossy(&fm[o..end]).into_owned()
        })
        .collect();
    let lookup = std::fs::read(ribbon.join("PVSZLookup_00.dat"))?;
    let records = props::pvs_records(pvs_bytes)?;
    let casters: HashSet<u16> = records.iter().filter(|r| r[4] & 0x08 != 0).map(|r| u16::from_be_bytes([r[0], r[1]])).collect();
    let mut lists = vec![Vec::new(); nzones];
    let mut ranges: HashMap<u16, (f32, Option<f32>, bool)> = HashMap::new();
    for pair in lookup.chunks_exact(8) {
        let (name, zone) = (be(pair, 0) as usize, be(pair, 4) as usize);
        let Some(e) = names.get(name).and_then(|n| by_name.get(&n.to_ascii_lowercase())) else { continue };
        let d = ar.read(e)?;
        // Every world-space record carries one distance triple in all zones (11,392 checked): slot k ends at d[k].
        for inst in fh1_formats::pvsz::parse(&d)?.instances.iter().filter(|i| !i.is_placed()) {
            let Some(r) = records.get(inst.record as usize) else { continue };
            let bands = (r[6] >> 3) & 7;
            if bands == 0 || !inst.distances.iter().take(2).all(|v| v.is_finite() && *v > 0.0) {
                continue;
            }
            let (k0, k1) = (bands.trailing_zeros() as usize, 7 - bands.leading_zeros() as usize);
            let start = if k0 == 0 { 0.0 } else { inst.distances[k0 - 1] };
            let end = inst.distances[k1];
            let end = (end.is_finite() && end > 0.0).then_some(end);
            let m = u16::from_be_bytes([r[0], r[1]]);
            let v = ranges.entry(m).or_insert((start, end, k1 == 2));
            v.0 = v.0.min(start);
            v.1 = v.1.zip(end).map(|(a, b)| a.max(b));
            v.2 |= k1 == 2;
        }
        let n = be(&d, 0) as usize;
        let mut list: Vec<u16> = (0..n)
            .map(|i| be(&d, 4 + i * 4))
            .filter_map(|v| records.get((v & 0x7FFF_FFFF) as usize))
            .filter(|r| r[6] & 0x38 != 0 && r[4] & 1 == 0)
            .map(|r| u16::from_be_bytes([r[0], r[1]]))
            .collect();
        list.sort_unstable();
        list.dedup();
        if let Some(l) = lists.get_mut(zone) {
            *l = list;
        }
    }
    Ok(Zones { size, origin, cols, rows, cells, lists, casters, ranges })
}

/// Writes `zones.bin` and returns the index.json `zones` entry.
///
/// `zones.bin` (LE): `b"FH1ZONE1"`, f32 hex size, f32 origin x, f32 origin z (collision space), u32 cols, u32 rows,
/// u32 cell -> zone (cols x rows, row-major, u32::MAX = none), u32 zone count, per zone u32 n + n x u32 model.
/// The entry: `{file, models: [{n, file, lod, group, min, max, shadow, range}], groups: [[T0, T1, T2, T3]]}` where `Tk` is the
/// distance (m, 2D to the group's bounds) beyond which a zone that lists both LOD <= k and LOD > k of the group
/// draws the coarser one. Estimated from the zones themselves: a zone lists LOD <= k only if some point of its
/// hex is within Tk, and LOD > k only if some point is beyond it (UNVERIFIED as the game's exact metric).
/// `range` = `[start, end, culls]` metres (end `null` = never culled; `culls` = the end is the cull, not a switch): the game's own LOD slots for the model from the
/// `.pvsz` entries (VERIFIED data, docs/WORLD_LOD.md); the engine prefers it over the group thresholds.
fn write_zones(dir: &Path, z: &Zones, models: &[ZoneModel]) -> Result<Value> {
    let mut b = Vec::new();
    b.extend_from_slice(b"FH1ZONE1");
    for v in [z.size, z.origin[0], z.origin[1]] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for v in [z.cols, z.rows].into_iter().chain(z.cells.iter().copied()).chain([z.lists.len() as u32]) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    let have: HashSet<u16> = models.iter().map(|m| m.n).collect();
    for l in &z.lists {
        let l: Vec<u16> = l.iter().copied().filter(|n| have.contains(n)).collect();
        b.extend_from_slice(&(l.len() as u32).to_le_bytes());
        l.iter().for_each(|&n| b.extend_from_slice(&(n as u32).to_le_bytes()));
    }
    std::fs::write(dir.join("zones.bin"), b)?;

    // Groups (union bounds in x / z) and their switch distances.
    let mut group_ids: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<([f32; 2], [f32; 2])> = Vec::new();
    let mut of_model: HashMap<u16, (usize, u32)> = HashMap::new();
    for m in models {
        let g = *group_ids.entry(m.group.as_str()).or_insert_with(|| {
            groups.push(([f32::MAX; 2], [f32::MIN; 2]));
            groups.len() - 1
        });
        let gb = &mut groups[g];
        gb.0 = [gb.0[0].min(m.min[0]), gb.0[1].min(m.min[2])];
        gb.1 = [gb.1[0].max(m.max[0]), gb.1[1].max(m.max[2])];
        of_model.insert(m.n, (g, m.lod));
    }
    let mut lower = vec![[f32::MIN; 4]; groups.len()];
    let mut upper = vec![[f32::MAX; 4]; groups.len()];
    let inner = z.size * 3f32.sqrt() * 0.5; // hex inner radius
    for (i, &zone) in z.cells.iter().enumerate() {
        let Some(list) = z.lists.get(zone as usize) else { continue };
        let c = z.centre(i as u32 % z.cols, i as u32 / z.cols);
        let mut listed: HashMap<usize, u32> = HashMap::new(); // group -> bitmask of LOD levels
        for n in list {
            if let Some(&(g, lod)) = of_model.get(n) {
                *listed.entry(g).or_default() |= 1 << lod.min(31);
            }
        }
        for (g, mask) in listed {
            let (lo, hi) = groups[g];
            let d = (lo[0] - c[0]).max(c[0] - hi[0]).max(0.0).hypot((lo[1] - c[1]).max(c[1] - hi[1]).max(0.0));
            for k in 0..4 {
                let fine = mask & ((2u32 << k) - 1) != 0;
                let coarse = mask >> (k + 1) != 0;
                if fine {
                    lower[g][k] = lower[g][k].max(d - z.size);
                }
                if fine && !coarse {
                    lower[g][k] = lower[g][k].max(d + inner);
                }
                if coarse {
                    upper[g][k] = upper[g][k].min(d + z.size);
                }
                if coarse && !fine {
                    upper[g][k] = upper[g][k].min(d - inner);
                }
            }
        }
    }
    let mut inconsistent = 0;
    let thresholds: Vec<[f32; 4]> = (0..groups.len())
        .map(|g| {
            std::array::from_fn(|k| {
                let (lo, hi) = (lower[g][k], upper[g][k]);
                match (lo > f32::MIN, hi < f32::MAX) {
                    (true, true) => {
                        if lo > hi {
                            inconsistent += 1;
                        }
                        (lo + hi) * 0.5
                    }
                    (false, true) => hi.max(0.0),
                    _ => 1e6,
                }
            })
        })
        .collect();
    println!("[scenery] zones: {} LOD groups, {inconsistent} inconsistent switch-distance bounds", groups.len());
    let model_json: Vec<Value> = models
        .iter()
        .map(|m| json!({"n": m.n, "file": m.file, "lod": m.lod, "group": of_model[&m.n].0, "min": m.min, "max": m.max, "shadow": m.shadow, "range": m.range.map(|r| json!([r.0, r.1, r.2]))}))
        .collect();
    Ok(json!({"file": "zones.bin", "models": model_json, "groups": thresholds}))
}

/// Whole-object smashable templates -> (type, shard templates). A `CollObjs.xml` type with k parts
/// (`CO_Bench_001.0..12.rmb`) is its whole template (`O_Undamaged`, from `collobj_templates`) followed by up to k-1
/// shard templates, the `S_*` models numbered right after it (bench: 6 + shards 7..18, all in the object's local
/// space). The GameObjs flyers / speed cameras have no part count: their adjacent `S_*` models (up to 8).
fn smash_table(
    ar: &mut Archive<std::fs::File>,
    by_name: &HashMap<String, Entry>,
    objs: &[props::XmlPlacement],
    collobj_map: &BTreeMap<String, (u16, props::CollObjMatch)>,
    src: &TrackSrc,
) -> Result<BTreeMap<u16, (String, Vec<u16>)>> {
    let mut parts: HashMap<&str, usize> = HashMap::new();
    for o in objs {
        let mut it = o.kind.split('.');
        let (Some(t), Some(k)) = (it.next(), it.next().and_then(|k| k.parse::<usize>().ok())) else { continue };
        let e = parts.entry(t).or_default();
        *e = (*e).max(k + 1);
    }
    let mut is_shard = |n: u16| -> bool {
        let Some(e) = by_name.get(&src.model_file(n)) else { return false };
        let Some(m) = ar.read(e).ok().and_then(|d| rmb::parse(&d).ok()) else { return false };
        !m.submodels.is_empty() && m.submodels.iter().all(|s| s.name.to_ascii_uppercase().starts_with("S_"))
    };
    // (whole, type, wanted shard count): CollObjs part counts; GameObjs flyers / cameras up to 8.
    let mut wholes: Vec<(u16, String, usize)> = collobj_map
        .iter()
        .map(|(t, &(n, _))| (n, t.clone(), parts.get(t.as_str()).copied().unwrap_or(1).saturating_sub(1)))
        .collect();
    for &(prefix, n, _) in props::GAMEOBJ_TEMPLATES.iter().filter(|(p, _, _)| *p != "BARNFIND_") {
        wholes.push((n, prefix.trim_end_matches('_').to_owned(), 8));
    }
    let whole_set: HashSet<u16> = wholes.iter().map(|w| w.0).collect();
    // Shards follow their whole template (bench 6: 7..18), or precede it (bin E 23: 19..22): take the run after
    // first, then the run before for the parts still missing, never crossing another whole or a claimed shard.
    let mut claimed: HashSet<u16> = HashSet::new();
    let mut table: BTreeMap<u16, (String, Vec<u16>)> = BTreeMap::new();
    for (n, t, want) in &wholes {
        let mut out = Vec::new();
        let mut k = *n + 1;
        while out.len() < *want && !whole_set.contains(&k) && is_shard(k) {
            claimed.insert(k);
            out.push(k);
            k += 1;
        }
        table.insert(*n, (t.clone(), out));
    }
    for (n, _, want) in &wholes {
        let mut k = *n;
        while table[n].1.len() < *want && k > 0 {
            k -= 1;
            if whole_set.contains(&k) || claimed.contains(&k) || !is_shard(k) {
                break;
            }
            claimed.insert(k);
            table.get_mut(n).unwrap().1.push(k);
        }
    }
    let shards: usize = table.values().map(|(_, s)| s.len()).sum();
    println!("[scenery] smashables: {} types, {shards} shard templates", table.len());
    Ok(table)
}

/// Solid static props without collision in the track mesh (UNVERIFIED which ones the game makes solid): trees and
/// rocks. Walls, fences and barriers are already in the `.fiz` collision (checked with fh1-world `prop_coll`);
/// bushes and shrubs stay passable.
fn is_solid_prop(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    let tree = (n.contains("TREE") || n.contains("PINIONPINE") || n.contains("PINE")) && !n.contains("BUSH") && !n.contains("SHRUB");
    tree || n.starts_with("OBJ_CLRD_ROCK") || is_wall_prop(name)
}

/// Fences, walls, barriers (2026-10-08 barrier_survey: 194 templates with no collider; at paved edges with no .fiz wall
/// the car drove through and fell out of the map). Gates (they open) and bridges (huge bounds) excluded.
fn is_wall_prop(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["FENCE", "WALL", "BARRIER", "BOLLARD", "ARMCO", "RAILINGS"].iter().any(|k| n.contains(k)) && !["GATE", "BRIDGE", "BUSH", "SHRUB", "BLDG"].iter().any(|k| n.contains(k))
}

/// `phys` of a smash entry: the matched global type's top-level definition in TEMPLATE space (body point p ->
/// S·(p - graphics offset), S = diag(1, 1, -1)): {global, err (bounds match, m), break_mph, mass (units UNVERIFIED),
/// spheres [[x, y, z, r]], boxes [{c, axes (3 rows), half}]} (convex hulls as the box of their vertices).
fn physics_json(p: &props::physdef::PhysicsDefinitions, (g, err): (usize, f32)) -> Value {
    use props::physdef::Shape;
    let d = &p.defs[p.global_to_def[g] as usize];
    let go = d.graphics_offset;
    let pt = |v: [f32; 3]| [v[0] - go[0], v[1] - go[1], -(v[2] - go[2])];
    let flip = |r: [f32; 3]| [r[0], r[1], -r[2]];
    let axes = |m: &[f32; 16]| {
        let row = |i: usize| -> [f32; 3] { let r = flip([m[4 * i], m[4 * i + 1], m[4 * i + 2]]); if i == 2 { [-r[0], -r[1], -r[2]] } else { r } };
        [row(0), row(1), row(2)]
    };
    let (mut spheres, mut boxes) = (Vec::new(), Vec::new());
    for s in &d.shapes {
        match s {
            Shape::Sphere { centre, radius } => spheres.push(json!([pt(*centre)[0], pt(*centre)[1], pt(*centre)[2], radius])),
            Shape::Box { matrix, half } => boxes.push(json!({"c": pt([matrix[12], matrix[13], matrix[14]]), "axes": axes(matrix), "half": half})),
            Shape::Convex { matrix, vertices } => {
                let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                for v in vertices {
                    for k in 0..3 {
                        lo[k] = lo[k].min(v[k]);
                        hi[k] = hi[k].max(v[k]);
                    }
                }
                let c: [f32; 3] = std::array::from_fn(|k| (lo[k] + hi[k]) * 0.5);
                let w: [f32; 3] = std::array::from_fn(|k| matrix[12 + k] + (0..3).map(|i| c[i] * matrix[4 * i + k]).sum::<f32>());
                boxes.push(json!({"c": pt(w), "axes": axes(matrix), "half": std::array::from_fn::<f32, 3, _>(|k| (hi[k] - lo[k]) * 0.5)}));
            }
        }
    }
    json!({"global": g, "err": err, "break_mph": d.break_mph, "mass": d.mass, "spheres": spheres, "boxes": boxes})
}

/// index.json `props.collision`: `smash` [{n, type, shards, phys (game shapes, `physics_json`, or null)}], `solid` [n], `bounds` {n: [min, max]} (template local
/// space) for every listed template and shard.
fn collision_table(
    smash: &BTreeMap<u16, (String, Vec<u16>)>,
    names: &HashMap<u16, String>,
    bounds: &HashMap<u16, ([f32; 3], [f32; 3])>,
    physics: Option<&props::physdef::PhysicsDefinitions>,
) -> Value {
    let solid: Vec<u16> = names.iter().filter(|(n, name)| is_solid_prop(name) && !smash.contains_key(n)).map(|(n, _)| *n).collect();
    let mut b = serde_json::Map::new();
    for n in smash.iter().flat_map(|(n, (_, s))| std::iter::once(*n).chain(s.iter().copied())).chain(solid.iter().copied()) {
        if let Some((lo, hi)) = bounds.get(&n) {
            b.insert(n.to_string(), json!([lo, hi]));
        }
    }
    let matched = physics.map_or(0, |p| smash.iter().filter(|(n, (_, s))| bounds.get(n).is_some_and(|&(lo, hi)| p.match_template(**n, lo, hi, s.len()).is_some())).count());
    println!("[scenery] prop collision: {} smashable types ({matched} with game physics shapes), {} solid templates", smash.len(), solid.len());
    let trunk = |n: &u16| names.get(n).is_some_and(|m| !m.to_ascii_uppercase().starts_with("OBJ_CLRD_ROCK") && !is_wall_prop(m));
    json!({
        "smash": smash
            .iter()
            .map(|(n, (t, s))| {
                let phys = physics.zip(bounds.get(n)).and_then(|(p, &(lo, hi))| p.match_template(*n, lo, hi, s.len()).map(|g| physics_json(p, g)));
                json!({"n": n, "type": t, "shards": s, "phys": phys})
            })
            .collect::<Vec<_>>(),
        // Trees: a trunk cylinder; rocks: a box.
        "solid": solid.iter().map(|n| json!({"n": n, "trunk": trunk(n), "wall": names.get(n).is_some_and(|m| is_wall_prop(m))})).collect::<Vec<_>>(),
        "bounds": b,
    })
}
