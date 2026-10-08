//! texsurvey <install>/assets/private [shader-filter] — which scenery materials draw with a
//! placeholder: for every material in `scenery/colorado/materials.json`, the samplers its effect's
//! Default pass reads (VS + PS constant tables) against the material's texture slots, weighted by
//! the triangles that use it (world tiles + prop templates x placements).
//!
//! Categories per used sampler: `ok`, `null` (no slot value: the material leaves it unbound),
//! `nofile` (runtime texture with no disc data), `missing` (file named but not on disk), `cube?`
//! (2D/cube mismatch between the fetch and the DDS). Also reports effects that fail to load,
//! per-object globals the engine never sets (no constant-table default) and the gamma flag vs
//! DDS format split.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use fh1_shaders::container::RegisterSet;
use fh1_shaders::effect::Effect;

const PER_OBJECT: &[&str] = &[
    "ModelData", "SurfaceNormalAndShadowPower", "FadeValues", "DistanceFadeValues", "DirectFadeValues", "Diffuse_Texture_uvOS",
    "diffuse_uvOS", "opacity_uvOS", "objTintColour", "HackMatrix", "RoadSpecularParams2", "SunGainScale",
];

/// Per-object globals fh1-render's scenery.rs now sets (`set_object_defaults`).
const PER_OBJECT_SET: &[&str] = &["ModelData", "SurfaceNormalAndShadowPower", "FadeValues", "DirectFadeValues"];

/// Triangles per material in an `FH1TILE4` file.
fn tile_tris(b: &[u8], out: &mut HashMap<u32, u64>, mul: u64) {
    if !matches!(b.get(..8), Some(b"FH1TILE4" | b"FH1TILE5")) {
        return;
    }
    let u = |p: usize| b.get(p..p + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()));
    let Some(n) = u(8) else { return };
    let mut p = 12;
    for _ in 0..n {
        let (Some(mat), Some(mask), Some(nv), Some(ni)) = (u(p), u(p + 8), u(p + 12), u(p + 16)) else { return };
        p += 20;
        let (nv, ni) = (nv as usize, ni as usize);
        let mut per = 24;
        for (bit, sz) in [(1, 12), (2, 8), (4, 8), (8, 8), (16, 4), (32, 8), (64, 12)] {
            if mask & bit != 0 {
                per += sz;
            }
        }
        p += nv * per + ni * 4;
        *out.entry(mat).or_default() += (ni / 3) as u64 * mul;
    }
}

fn dds_info(path: &Path) -> Option<(u32, bool)> {
    let b = std::fs::read(path).ok()?;
    let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    (b.len() >= 148).then(|| (u(128), u(136) & 4 != 0))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(&args[1]);
    let filter = args.get(2).map(|s| s.to_ascii_lowercase());
    let dir = root.join("scenery/colorado");
    let shaders = root.join("shaders/track");
    let mats: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("materials.json")).unwrap()).unwrap();
    let mats = mats.as_array().unwrap();

    // Triangle weights.
    let mut tris: HashMap<u32, u64> = HashMap::new();
    for e in std::fs::read_dir(dir.join("tiles")).unwrap().flatten() {
        tile_tris(&std::fs::read(e.path()).unwrap(), &mut tris, 1);
    }
    let mut placed: HashMap<u32, u64> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir.join("props/tiles")) {
        for e in rd.flatten() {
            let b = std::fs::read(e.path()).unwrap();
            // FH1PROP1 = 68-byte records, FH1PROP2 adds ground normal + tint (84), FH1PROP3 two lightmap ids (92); see fh1-engine scenery.rs.
            let stride = match b.get(..8) {
                Some(b"FH1PROP3") => 92,
                Some(b"FH1PROP2") => 84,
                Some(b"FH1PROP1") => 68,
                _ => continue,
            };
            let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
            for i in 0..n {
                let o = 12 + i * stride;
                *placed.entry(u32::from_le_bytes(b[o..o + 4].try_into().unwrap())).or_default() += 1;
            }
        }
    }
    for (model, count) in &placed {
        if let Ok(b) = std::fs::read(dir.join(format!("props/templates/{model}.bin"))) {
            tile_tris(&b, &mut tris, *count);
        }
    }
    let total: u64 = tris.values().sum();

    // Effects: Default technique, first pass.
    let mut effects: HashMap<String, Option<Effect>> = HashMap::new();
    let mut files: HashMap<String, PathBuf> = HashMap::new();
    for e in std::fs::read_dir(&shaders).unwrap().flatten() {
        let f = e.file_name().to_string_lossy().to_ascii_lowercase();
        if let Some(stem) = f.strip_suffix(".fxobj") {
            files.insert(stem.to_string(), e.path());
        }
    }
    let mut dds_cache: HashMap<String, Option<(u32, bool)>> = HashMap::new();

    // (shader, sampler register, sampler name, category) -> (materials, triangles)
    let mut cats: BTreeMap<(String, u32, String, &'static str), (u64, u64)> = BTreeMap::new();
    let mut by_cat: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let mut no_effect: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut unset_globals: BTreeMap<(String, u16), (u64, u64)> = BTreeMap::new();
    let mut gamma_fmt: BTreeMap<(u32, bool), u64> = BTreeMap::new();
    let mut unused_slots = (0u64, 0u64);
    // Per-object globals (set by the game per draw): constant-table defaults seen.
    let mut per_object: BTreeMap<(String, u16), BTreeMap<String, u64>> = BTreeMap::new();

    // Globals the engine sets (fh1-render lighting/program, fh1-engine) or that are matrices.
    let engine_sets = |n: &str| {
        matches!(
            n,
            "sunDir" | "sunColor" | "ambColor" | "skyColor" | "groundColor" | "FogConsts" | "FogColor" | "FogConsts2" | "FogColor2"
                | "SkyScale" | "TimeGain" | "V2LightmapColor1" | "V2LightmapColor2" | "EmissiveSwitchOnThreshold"
                | "DistanceFadeValues" | "CamPosWorld" | "camPosWorld"
        ) || n.contains("Matrix")
            || matches!(n, "wvp" | "world" | "worldI" | "worldIT" | "viewInv" | "viewProj")
            || n.starts_with("CSM")
            || n.starts_with("HeadLight")
    };

    for (idx, m) in mats.iter().enumerate() {
        let w = tris.get(&(idx as u32)).copied().unwrap_or(0);
        let shader = m["shader"].as_str().unwrap_or("").rsplit(['\\', '/']).next().unwrap().trim_end_matches(".fx").to_ascii_lowercase();
        if filter.as_ref().is_some_and(|f| !shader.contains(f.as_str())) {
            continue;
        }
        let fx = effects
            .entry(shader.clone())
            .or_insert_with(|| files.get(&shader).and_then(|p| Effect::parse(&std::fs::read(p).ok()?).ok()));
        let Some(fx) = fx.as_ref() else {
            let e = no_effect.entry(shader.clone()).or_default();
            e.0 += 1;
            e.1 += w;
            continue;
        };
        let Some(pass) = fx.technique("Default").or(fx.techniques.first()).and_then(|t| t.passes.first()) else { continue };
        let blobs: Vec<_> = [pass.vs, pass.ps].into_iter().flatten().filter_map(|i| fx.shaders.get(i)).collect();
        let slots = m["textures"].as_array().cloned().unwrap_or_default();
        let mut used = vec![false; slots.len().max(16)];
        for blob in &blobs {
            for c in &blob.constants {
                if c.set == RegisterSet::Sampler {
                    let r = c.register as u32;
                    if let Some(u) = used.get_mut(r as usize) {
                        *u = true;
                    }
                    let slot = slots.get(r as usize).cloned().unwrap_or(serde_json::Value::Null);
                    let cat: &'static str = if slot.is_null() {
                        "null"
                    } else if let Some(f) = slot["file"].as_str() {
                        let info = dds_cache.entry(f.to_string()).or_insert_with(|| dds_info(&dir.join(f)));
                        match info {
                            None => "missing",
                            Some((fmt, cube)) => {
                                let word = slot["word"].as_u64().unwrap_or(0) as u32;
                                *gamma_fmt.entry((*fmt, (word >> 8) & 0x3F == 0x3F)).or_default() += w;
                                let wants_cube = c.name.to_ascii_lowercase().contains("cube");
                                if wants_cube != *cube {
                                    "cube?"
                                } else {
                                    "ok"
                                }
                            }
                        }
                    } else {
                        "nofile"
                    };
                    let e = cats.entry((shader.clone(), r, c.name.clone(), cat)).or_default();
                    e.0 += 1;
                    e.1 += w;
                    let e = by_cat.entry(cat).or_default();
                    e.0 += 1;
                    e.1 += w;
                } else if c.set == RegisterSet::Float4 && PER_OBJECT.contains(&c.name.as_str()) {
                    let d = c.default.as_ref().map(|d| format!("{:?}", d.iter().map(|x| f32::from_bits(*x)).collect::<Vec<_>>())).unwrap_or("none".into());
                    *per_object.entry((c.name.clone(), c.register)).or_default().entry(format!("{d} [{shader}]")).or_default() += w;
                }
                if c.set == RegisterSet::Float4 && c.register >= 16 && !engine_sets(&c.name) && !PER_OBJECT_SET.contains(&c.name.as_str()) {
                    let d = c.default.as_ref().map(|d| format!("{:?}", d.iter().take(4).map(|x| f32::from_bits(*x)).collect::<Vec<_>>())).unwrap_or("none".into());
                    let e = unset_globals.entry((format!("{:<28} {d}", c.name), c.register)).or_default();
                    e.0 += 1;
                    e.1 += w;
                }
            }
        }
        for (i, s) in slots.iter().enumerate() {
            if !s.is_null() && !used[i] {
                unused_slots.0 += 1;
                unused_slots.1 += w;
            }
        }
    }

    let pct = |t: u64| 100.0 * t as f64 / total.max(1) as f64;
    println!("{} materials, {} triangles (world tiles + placed props)\n", mats.len(), total);
    println!("== used samplers by category (bindings, % of triangles touched)");
    for (k, (n, t)) in &by_cat {
        println!("{k:>8} {n:>7} {:>6.2}%", pct(*t));
    }
    println!("unused slot values: {} ({:.2}% of triangles)", unused_slots.0, pct(unused_slots.1));
    println!("\n== effects that fail to load (StandardMaterial fallback)");
    for (k, (n, t)) in &no_effect {
        println!("{n:>6} {:>6.2}% {k}", pct(*t));
    }
    println!("\n== non-ok bindings, by triangles");
    let mut v: Vec<_> = cats.iter().filter(|(k, _)| k.3 != "ok").collect();
    v.sort_by_key(|(_, (_, t))| std::cmp::Reverse(*t));
    for ((sh, r, name, cat), (n, t)) in v.iter().take(60) {
        println!("{cat:>8} {n:>6} {:>6.2}%  tf{r:<2} {name:<32} {sh}", pct(*t));
    }
    println!("\n== bindings other than null/ok (runtime textures, missing files, 2D/cube mismatch)");
    for ((sh, r, name, cat), (n, t)) in v.iter().filter(|(k, _)| k.3 != "null") {
        println!("{cat:>8} {n:>6} {:>6.3}%  tf{r:<2} {name:<32} {sh}", pct(*t));
    }
    println!("\n== float globals not set by the engine (constant-table default used, first 4 floats)");
    let mut v: Vec<_> = unset_globals.iter().collect();
    v.sort_by_key(|(_, (_, t))| std::cmp::Reverse(*t));
    for ((name, reg), (n, t)) in v {
        println!("{n:>6} {:>6.2}%  c{reg:<4} {name}", pct(*t));
    }
    println!("\n== per-object globals: constant-table defaults (% of triangles)");
    for ((name, reg), ds) in &per_object {
        let mut ds: Vec<_> = ds.iter().collect();
        ds.sort_by_key(|(_, t)| std::cmp::Reverse(**t));
        for (d, t) in ds.iter().take(4) {
            println!("c{reg:<4} {name:<28} {:>6.2}% {d}", pct(**t));
        }
    }
    println!("\n== DXGI format x gamma flag (word bits 8-13 = 0x3F), % of triangles");
    for ((f, g), t) in &gamma_fmt {
        println!("dxgi {f:>3} gamma {g:<5} {:>6.2}%", pct(*t));
    }
}
