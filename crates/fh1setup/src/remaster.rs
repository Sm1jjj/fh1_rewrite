//! Setup group `remaster` (docs/REMASTER.md): the remaster renderer's material table, derived from the installed
//! `scenery` group (materials.json) and the track effects (`shaders/track/*.fxobj`, for each family's sampler names).
//! No disc access: this group must run after `scenery` and `shaders`.
//!
//! Output: `remaster/scenery/<track>/materials.bin` (+ `materials.json`, the same records for debugging), one record
//! per game material, index = game material id. The rules (one per shader family) are in docs/REMASTER.md
//! "Material rules"; this file implements them. The reader is fh1-remaster `material.rs` (keep both in step).
//!
//! `materials.bin`: `b"FH1RMAT1"`, u32 count, u32 record size (= RECORD_SIZE), then records, little-endian:
//! `u8 class, u8 layering, u16 flags, u32 tex[ROLES] (u32::MAX = none), u8 uv_set[ROLES], u8 pad[3],
//! f32 uv_scale[ROLES][2], f32 params[4][4], u32 srgb (bit per role)`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Texture roles (index into the record's texture arrays).
pub const ROLE_NAMES: [&str; ROLES] = ["a", "b", "c", "weight", "normal", "normal_b", "modulate", "ao", "lightmap", "emissive", "specular", "mask"];
pub const ROLES: usize = 12;
const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
const W: usize = 3;
const N: usize = 4;
const NB: usize = 5;
const MODULATE: usize = 6;
const AO: usize = 7;
const LM: usize = 8;
const EM: usize = 9;
const SP: usize = 10;
const MASK: usize = 11;

pub const RECORD_SIZE: usize = 4 + 4 * ROLES + ROLES + 3 + 8 * ROLES + 64 + 4;

/// Material class (how it is drawn).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Class {
    Opaque = 0,
    /// Alpha-tested (the game's alpha-to-coverage `_opac` / `_mask` / tree effects).
    Cutout = 1,
    /// Alpha-blended ground overlay (road/vblnd `_blend`, `_opac`, `_decal`): no depth write, depth bias.
    Decal = 2,
    Water = 4,
    /// Additive glow (`diff_glow_1`, `add_diff_opac_rgba`).
    Additive = 5,
    Skip = 6,
    /// Unlit board (`*_nolight_*`: countdown / flipbook signs).
    Unlit = 7,
}

/// How the base colour is assembled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Layering {
    Single = 0,
    /// `h_blnd*`: A/B by splat.r, C by splat.g × C.a.
    Splat = 1,
    /// `h_road*`: A/B/C by vertex colour + noise (c4), modulate by vertex colour b.
    Road = 2,
    /// `h_vblnd*`: A/B by vertex colour r (c4.x).
    VertexBlend = 3,
}

pub const FLAG_TWO_SIDED: u16 = 1;
pub const FLAG_DEPTH_BIAS: u16 = 2;
pub const FLAG_NIGHT_EMISSIVE: u16 = 4;
/// Vertex animation in the game (flags, bunting, tree bend): drawn static.
pub const FLAG_ANIMATED: u16 = 8;
/// Base colour × 2 × the placement tint (the game's ModelData / objTintColour; trees and `_tint`).
pub const FLAG_OBJECT_TINT: u16 = 16;
/// Cube-map reflection in the game (`_refl`): environment reflections stronger (lower roughness floor).
pub const FLAG_REFLECTIVE: u16 = 32;
/// The family samples Light_Map but the material's slot is empty (rmb slot -1): the night lightmap is per placement
/// (docs/LIGHTMAPS.md "Per-instance lightmaps"), bound by the engine's prop placement.
pub const FLAG_INSTANCE_LM: u16 = 64;
/// Cloth (`anim_flag*`, `bunting_*`): the remaster vertex shader waves it (REMASTER approximation of the game's
/// cloth VS, which isn't ported); amplitude p3.y m, frequency p3.z Hz.
pub const FLAG_CLOTH: u16 = 128;
/// FM4: tf7 is the DAYTIME baked track lightmap (docs/FM4_RECON.md), multiplied into the diffuse light, not FH1's night
/// lamp map: the engine applies it as diffuse occlusion.
pub const FLAG_LM_BAKED: u16 = 256;
/// Road surface (FM4 road / shoulder / road line): dry-tarmac roughness floor as the FH1 road layering.
pub const FLAG_ROAD: u16 = 512;
/// Tree / foliage cards (`tree_*`, `diff_treebend*`, `treecard_*`): the game lights them per instance without vertex
/// normals; the remaster gives them a spherical foliage normal from the tree origin (no back-face flip).
pub const FLAG_TREE: u16 = 1024;

/// One family's sampler register -> role.
fn sampler_role(name: &str) -> Option<usize> {
    Some(match name {
        "Diffuse_TextureSampler" | "Blend_ASampler" | "diffuseSampler" | "DiffuseSampler" | "Diff_Texture_DetailGrainSampler" => A,
        "Blend_BSampler" => B,
        "Blend_CSampler" => C,
        "Splat_Sampler" | "Noise_Sampler" | "Blend_ValueSampler" => W,
        "NormalMapSampler" | "NormalMapASampler" | "FlagNormalASampler" | "Normal_MapSampler" => N,
        "NormalMapBSampler" | "FlagNormalBSampler" => NB,
        "Modulate_Sampler" => MODULATE,
        "AO_Sampler" | "AOMapSampler" => AO,
        "Light_MapSampler" => LM,
        "EmissiveMapSampler" => EM,
        "SpecularMapSampler" | "SPECSampler" | "SPECR_MapSampler" | "SpecularSampler" => SP,
        "Mask_TextureSampler" | "opacitySampler" | "OpacitySampler" | "Road_AlphaSampler" => MASK,
        _ => return None,
    })
}

/// Per family (effect name): sampler register -> role, and the Default pass's render states.
struct Family {
    roles: HashMap<u32, usize>,
    blend: bool,
    additive: bool,
    alpha_to_coverage: bool,
    two_sided: bool,
}

fn read_family(path: &Path) -> Option<Family> {
    use fh1_shaders::container::RegisterSet;
    use fh1_shaders::effect::rs;
    let fx = fh1_shaders::effect::Effect::parse(&std::fs::read(path).ok()?).ok()?;
    let pass = fx.technique("Default")?.passes.first()?;
    let mut roles = HashMap::new();
    if let Some(ps) = pass.ps.and_then(|i| fx.shaders.get(i)) {
        for c in ps.constants.iter().filter(|c| c.set == RegisterSet::Sampler) {
            if let Some(r) = sampler_role(&c.name) {
                roles.insert(c.register as u32, r);
            }
        }
    }
    let state = |id: u32| pass.render_states.iter().find(|s| s.0 == id).map(|s| s.1);
    Some(Family {
        roles,
        blend: state(rs::ALPHABLENDENABLE) == Some(1),
        // DESTBLEND = ONE.
        additive: state(rs::DESTBLEND) == Some(1),
        alpha_to_coverage: state(rs::ALPHATOMASKENABLE) == Some(1),
        two_sided: state(rs::CULLMODE) == Some(0),
    })
}

struct Record {
    class: Class,
    layering: Layering,
    flags: u16,
    tex: [u32; ROLES],
    uv_set: [u8; ROLES],
    uv_scale: [[f32; 2]; ROLES],
    params: [[f32; 4]; 4],
    srgb: u32,
}

/// The family rules (docs/REMASTER.md "Material rules").
fn record(shader: &str, ps: &[[f32; 4]], textures: &[Value], fam: Option<&Family>, fm4: bool) -> Record {
    let c = |k: usize| ps.get(k).copied().unwrap_or([0.0; 4]);
    let one = |v: f32| if v == 0.0 { 1.0 } else { v };
    let mut r = Record {
        class: Class::Opaque,
        layering: Layering::Single,
        flags: 0,
        tex: [u32::MAX; ROLES],
        uv_set: [0; ROLES],
        uv_scale: [[1.0, 1.0]; ROLES],
        // p0 = (albedo scale, spec level, spec power, normal strength); p1 = blend weights (c4);
        // p2 = (luminance scales c5.xyz, alpha cutoff); p3 = (lightmap floor, -, -, -).
        params: [[1.0, 0.0, 0.0, 1.0], [0.0; 4], [1.0, 1.0, 1.0, 0.5], [0.0; 4]],
        srgb: 0,
    };
    let Some(fam) = fam else {
        r.class = Class::Skip;
        return r;
    };
    // Textures by role; runtime-only textures (flags bit 0: blank 4x4 previews) count as absent except the base colour.
    for (slot, t) in textures.iter().enumerate() {
        let Some(&role) = fam.roles.get(&(slot as u32)) else { continue };
        if t.is_null() || t["file"].is_null() {
            continue;
        }
        let runtime = t["flags"].as_u64().unwrap_or(1) & 1 != 0;
        if runtime && role != A {
            continue;
        }
        r.tex[role] = t["id"].as_u64().unwrap_or(0) as u32;
        // The .bix format word's sign byte: 0x3F = gamma RGB (fh1-render SceneryMaterials). Without a word: the
        // format rule (BC4/BC5 linear) is applied by the engine when the image loads.
        let gamma = match t["word"].as_u64() {
            Some(w) => ((w >> 8) & 0x3F) == 0x3F,
            None => !matches!(role, N | NB),
        };
        if gamma {
            r.srgb |= 1 << role;
        }
    }
    let has = |r: &Record, role: usize| r.tex[role] != u32::MAX;
    let lm_slot = fam.roles.iter().find(|(_, &role)| role == LM).map(|(&slot, _)| slot as usize);
    if lm_slot.is_some_and(|s| textures.get(s).is_none_or(Value::is_null)) {
        r.flags |= FLAG_INSTANCE_LM;
    }

    // Class.
    // Track-owned sky meshes / sun sprites (FM4 `sky_diff_1`, `sun_contribution`, `flare_*`): the remaster sky (W3)
    // replaces them; drawn opaque they cast one shadow over the whole map.
    r.class = if shader == "light_pollution" || shader.starts_with("sky_") || shader.starts_with("sun_") || shader.starts_with("flare_") {
        Class::Skip
    } else if shader.starts_with("lake_") {
        Class::Water
    } else if shader.contains("_nolight_") {
        Class::Unlit
    } else if fam.blend && fam.additive {
        Class::Additive
    } else if fam.blend {
        Class::Decal
    } else if fam.alpha_to_coverage {
        Class::Cutout
    } else {
        Class::Opaque
    };
    if fam.two_sided || shader.starts_with("tree_") {
        r.flags |= FLAG_TWO_SIDED;
    }
    if shader.contains("depthbias") || shader.contains("_bias") || r.class == Class::Decal {
        r.flags |= FLAG_DEPTH_BIAS;
    }
    if shader.starts_with("anim_") || shader.starts_with("bunting_") || shader.contains("treebend") || shader.starts_with("tree_") {
        r.flags |= FLAG_ANIMATED;
    }
    if shader.starts_with("anim_flag_") || shader.starts_with("bunting_") {
        r.flags |= FLAG_CLOTH;
        r.params[3][1] = if shader.starts_with("bunting_") { 0.04 } else { 0.12 };
        r.params[3][2] = if shader.starts_with("bunting_") { 0.6 } else { 0.9 };
    }
    if shader.starts_with("tree_") || shader.contains("treebend") || shader.starts_with("treecard_") {
        r.flags |= FLAG_TREE;
    }
    if shader.starts_with("tree_") || shader.contains("_tint") || shader.contains("treebend") {
        r.flags |= FLAG_OBJECT_TINT;
    }
    if shader.contains("_refl") {
        r.flags |= FLAG_REFLECTIVE;
    }
    if has(&r, EM) {
        r.flags |= FLAG_NIGHT_EMISSIVE;
    }

    // Layering, UV sets and constants per family prefix (uv0-2 already carry the mesh's uvOffsetScale).
    if fm4 && !shader.starts_with("anim_") && !shader.starts_with("tree_") && !shader.contains("treebend") {
        fm4_rules(shader, &c, &mut r);
        return r;
    }
    if shader.starts_with("h_blnd") {
        // A uv0 × c0.xy, B uv0 × c0.zw, C uv0 × c1.xy; splat, AO, lightmap uv1 × c1.zw. Spec level c3.x, power c3.y.
        r.layering = Layering::Splat;
        r.uv_scale[A] = [c(0)[0], c(0)[1]];
        r.uv_scale[B] = [c(0)[2], c(0)[3]];
        r.uv_scale[C] = [c(1)[0], c(1)[1]];
        r.uv_scale[N] = r.uv_scale[A];
        for role in [W, AO, LM] {
            r.uv_set[role] = 1;
            r.uv_scale[role] = [one(c(1)[2]), one(c(1)[3])];
        }
        r.params[0] = [1.0, c(3)[0], c(3)[1], 1.0];
        r.params[2] = [c(5)[0], c(5)[1], c(5)[2], 0.5];
        r.params[3][0] = c(3)[3];
    } else if shader.starts_with("h_road") || shader.starts_with("h_vblnd") {
        // Road: A uv0 × c0.xy, B uv0 × c0.zw, C uv0 × c1.xy, noise uv0 × c1.zw, normal uv0 × c0.xy (strength c3.z),
        // modulate uv1 × c2.xy, AO + lightmap uv2. Weights: B = sat((vc.r-.5)c4.x + (noise.r-.5)c4.y + .5),
        // C = sat((vc.g-.5)c4.z + (noise.g-.5)c4.w + .5); modulate × vc.b. vblnd: B = sat((vc.r-.5)c4.x + .5),
        // the other roles as road (INFERRED for the vblnd mask/ovly variants).
        let road = shader.starts_with("h_road");
        r.layering = if road { Layering::Road } else { Layering::VertexBlend };
        r.uv_scale[A] = [c(0)[0], c(0)[1]];
        r.uv_scale[B] = [c(0)[2], c(0)[3]];
        r.uv_scale[C] = [one(c(1)[0]), one(c(1)[1])];
        r.uv_scale[W] = [one(c(1)[2]), one(c(1)[3])];
        r.uv_scale[N] = r.uv_scale[A];
        r.uv_scale[NB] = r.uv_scale[B];
        r.uv_set[MODULATE] = 1;
        r.uv_scale[MODULATE] = [one(c(2)[0]), one(c(2)[1])];
        r.uv_set[AO] = 2;
        r.uv_set[LM] = 2;
        r.params[0] = [1.0, c(3)[0], c(3)[1], if road { one(c(3)[2]) } else { 1.0 }];
        r.params[1] = c(4);
        r.params[2] = [one(c(5)[0]), one(c(5)[1]), one(c(5)[2]), 0.5];
        r.params[3][0] = c(3)[3];
    } else if shader.starts_with("anim_flag") || shader.starts_with("lake_") {
        // Flag / lake constants drive their vertex animation and scrolling normals, not the UVs.
    } else {
        // Diffuse family (h_diff*, bunting, trees, glow): diffuse/normal/spec/emissive uv0 × c0.xy, AO and lightmap uv1.
        // Albedo × c0.z, spec level c0.w, power c1.x (h_diff_* only; the others' c0 is the UV scale alone).
        let h_diff = shader.starts_with("h_diff");
        r.uv_scale[A] = [one(c(0)[0]), one(c(0)[1])];
        for role in [N, SP, EM, MASK] {
            r.uv_scale[role] = r.uv_scale[A];
        }
        r.uv_set[AO] = 1;
        r.uv_set[LM] = 1;
        if h_diff {
            r.params[0] = [one(c(0)[2]), if shader.contains("_spec") { c(0)[3] } else { 0.0 }, c(1)[0], 1.0];
        }
        // `_lm` floor: max(c213.y, c6.z) (docs/LIGHTMAPS.md; every installed value is 0).
        r.params[3][0] = c(6)[2];
    }
    r
}

/// FM4 track families (docs/REMASTER.md "FM4"; INFERRED from sampler names, constant patterns and the Default PS
/// fetch routing of road_blnd_2, shldr_blnd_spec_3, terr_blnd_spec_3, barr_shad_diff_spec_1, rdline_blnd_spec_opac_3,
/// diff_spec_1). The meshes carry texcoord0 + texcoord2: tiled layers on uv0, the baked lightmap (tf7) on uv2.
fn fm4_rules(shader: &str, c: &dyn Fn(usize) -> [f32; 4], r: &mut Record) {
    let one = |v: f32| if v == 0.0 { 1.0 } else { v };
    r.flags |= FLAG_LM_BAKED;
    r.uv_set[LM] = 2;
    r.uv_set[AO] = 2;
    // A missing base with a B layer (road_blnd_2, the shoulders' B-first order): B is the base.
    if r.tex[A] == u32::MAX && r.tex[B] != u32::MAX {
        r.tex[A] = r.tex[B];
        r.tex[B] = u32::MAX;
        let b = r.srgb & (1 << B) != 0;
        r.srgb = (r.srgb & !(1 << A) & !(1 << B)) | if b { 1 << A } else { 0 };
    }
    let road = shader.starts_with("road_") || shader.starts_with("shldr_") || shader.starts_with("rdline_") || shader.starts_with("rdedg_");
    if road {
        r.flags |= FLAG_ROAD;
    }
    if shader.starts_with("terr_blnd") {
        // A uv0 x c1.xy, B uv0 x c2.xy, mask (Blend_Value) untiled; tint c0.x; spec level c1.z, power c1.w.
        r.layering = Layering::Splat;
        r.uv_scale[A] = [one(c(1)[0]), one(c(1)[1])];
        r.uv_scale[B] = [one(c(2)[0]), one(c(2)[1])];
        r.params[0] = [one(c(0)[0]), c(1)[2], c(1)[3], 1.0];
    } else if shader.starts_with("shldr_blnd") || shader.starts_with("rdedg_shldr_blnd") {
        // A uv0 x c1.xy, B uv0 x c3.xy, mask untiled; spec power c3.z.
        r.layering = Layering::Splat;
        r.uv_scale[A] = [one(c(1)[0]), one(c(1)[1])];
        r.uv_scale[B] = [one(c(3)[0]), one(c(3)[1])];
        r.params[0] = [one(c(0)[0]), 0.3, c(3)[2], 1.0];
    } else if shader.starts_with("road_blnd") {
        // Base (Blend_B) uv0 x c1.x; the blend value only modulates detail in the game: single layer here.
        r.uv_scale[A] = [one(c(1)[0]), one(c(1)[0])];
        r.tex[W] = u32::MAX;
        r.params[0] = [1.0, 0.3, c(3)[0].max(4.0), 1.0];
    } else if shader.starts_with("rdline") {
        // Painted lines: colour uv0, opacity (Road_Alpha) uv0 x c2.xy, specular uv0 x c1.xy.
        r.uv_scale[MASK] = [one(c(2)[0]), one(c(2)[1])];
        r.uv_scale[SP] = [one(c(1)[0]), one(c(1)[1])];
        r.params[0] = [1.0, 0.3, 20.0, 1.0];
    } else if shader.starts_with("barr_shad") {
        // Barriers: diffuse uv0 x c0.xy, albedo x c0.z, spec level c0.w, power c1.y.
        r.uv_scale[A] = [one(c(0)[0]), one(c(0)[1])];
        r.params[0] = [one(c(0)[2]), c(0)[3], c(1)[1], 1.0];
    } else {
        // diff_* / sign_* / chain / treecard / co_* / rdedg_diff / grass: diffuse uv0 x c0.xy, albedo x c0.z,
        // spec level c0.w (when `_spec`), power c1.x.
        r.uv_scale[A] = [one(c(0)[0]), one(c(0)[1])];
        let spec = shader.contains("spec");
        r.params[0] = [one(c(0)[2]), if spec { c(0)[3] } else { 0.0 }, if spec { c(1)[0] } else { 0.0 }, 1.0];
    }
    for role in [N, SP, EM] {
        if r.uv_scale[role] == [1.0, 1.0] {
            r.uv_scale[role] = r.uv_scale[A];
        }
    }
}

fn write(r: &Record, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend([r.class as u8, r.layering as u8]);
    out.extend(r.flags.to_le_bytes());
    for t in r.tex {
        out.extend(t.to_le_bytes());
    }
    out.extend(r.uv_set);
    out.extend([0u8; 3]);
    for s in r.uv_scale {
        out.extend(s[0].to_le_bytes());
        out.extend(s[1].to_le_bytes());
    }
    for p in r.params {
        for v in p {
            out.extend(v.to_le_bytes());
        }
    }
    out.extend(r.srgb.to_le_bytes());
    debug_assert_eq!(out.len() - start, RECORD_SIZE);
}

fn dump(i: usize, shader: &str, r: &Record) -> Value {
    let textures: serde_json::Map<String, Value> = (0..ROLES)
        .filter(|&k| r.tex[k] != u32::MAX)
        .map(|k| {
            (ROLE_NAMES[k].to_owned(), json!({"id": r.tex[k], "uv": r.uv_set[k], "scale": r.uv_scale[k], "srgb": r.srgb & (1 << k) != 0}))
        })
        .collect();
    json!({
        "index": i, "shader": shader, "class": format!("{:?}", r.class), "layering": format!("{:?}", r.layering),
        "flags": r.flags, "textures": textures, "params": r.params,
    })
}

/// Build the table for one installed track scenery folder (`scenery/<track>` with materials.json).
fn build_track(scenery: &Path, track_shaders: &Path, out: &Path, fm4: bool) -> Result<()> {
    let mats: Value = serde_json::from_slice(&std::fs::read(scenery.join("materials.json")).with_context(|| format!("{}/materials.json", scenery.display()))?)?;
    let list = mats.as_array().context("materials.json is not an array")?;
    let mut families: HashMap<String, Option<Family>> = HashMap::new();
    let mut bin = Vec::with_capacity(16 + list.len() * RECORD_SIZE);
    bin.extend(b"FH1RMAT1");
    bin.extend((list.len() as u32).to_le_bytes());
    bin.extend((RECORD_SIZE as u32).to_le_bytes());
    let mut dumps = Vec::with_capacity(list.len());
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for (i, m) in list.iter().enumerate() {
        let shader = m["shader"].as_str().unwrap_or("").rsplit(['\\', '/']).next().unwrap_or("").trim_end_matches(".fx").to_ascii_lowercase();
        let fam = families.entry(shader.clone()).or_insert_with(|| read_family(&track_shaders.join(format!("{shader}.fxobj"))));
        let ps: Vec<[f32; 4]> = m["ps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| [0, 1, 2, 3].map(|k| v.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32))
            .collect();
        let textures = m["textures"].as_array().cloned().unwrap_or_default();
        let r = record(&shader, &ps, &textures, fam.as_ref(), fm4);
        *counts.entry(format!("{:?}", r.class)).or_default() += 1;
        write(&r, &mut bin);
        dumps.push(dump(i, &shader, &r));
    }
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("materials.bin"), &bin)?;
    std::fs::write(out.join("materials.json"), serde_json::to_vec_pretty(&dumps)?)?;
    let missing: Vec<&String> = families.iter().filter(|(_, f)| f.is_none()).map(|(k, _)| k).collect();
    println!("[remaster] {}: {} materials {:?}; families without an effect (skipped): {missing:?}", out.display(), list.len(), counts);
    Ok(())
}

/// `stage` = `<private>/remaster.staging`; reads `<private>/scenery` and `<private>/shaders` (installed groups), and every
/// imported native map (`<private>/imported/<game>/<id>/scenery` + its own `shaders/track`, from `import-fm4` /
/// `import-fh2`). Each table goes to `remaster/<the scenery folder's path under private>`.
pub fn build(_disc: &Path, stage: &Path) -> Result<()> {
    let private = stage.parent().context("remaster stage has no parent")?;
    let scenery = private.join("scenery").join("colorado");
    let shaders = private.join("shaders").join("track");
    anyhow::ensure!(scenery.join("materials.json").is_file(), "remaster needs the scenery group installed first");
    build_track(&scenery, &shaders, &stage.join("scenery").join("colorado"), false)?;
    // Imported maps (optional: only those installed). FM4 has its own family set (fm4_rules); FH2 uses FH1's.
    for game in ["fm4", "fh2"] {
        let Ok(dirs) = std::fs::read_dir(private.join("imported").join(game)) else { continue };
        let mut n = 0;
        for d in dirs.flatten() {
            let map = d.path();
            let sc = map.join("scenery");
            if !sc.join("materials.json").is_file() {
                continue;
            }
            let out = stage.join("imported").join(game).join(d.file_name()).join("scenery");
            if let Err(e) = build_track(&sc, &map.join("shaders").join("track"), &out, game == "fm4") {
                println!("[remaster] {}: skipped ({e})", sc.display());
                continue;
            }
            n += 1;
        }
        println!("[remaster] imported {game}: {n} maps");
    }
    Ok(())
}
