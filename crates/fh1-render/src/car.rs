//! Car materials: carbin subsection name → technique in the car shader library, and the
//! ShaderSettings XML chain that sets the technique's parameters. Rules reverse-engineered from
//! default.xex (docs/SHADERS.md, "Car materials"; resolver 0x82DA03D0).

use std::collections::{BTreeMap, HashMap};

/// Which mesh a subsection belongs to; changes the technique rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct CarMeshKind {
    /// Traffic car (simplified techniques).
    pub traffic: bool,
    /// Cockpit / high-detail model (`cockpit_` techniques on interior sections).
    pub cockpit: bool,
    /// SuperLOD (Forzavista) model.
    pub superlod: bool,
}

const INTERIOR_WORDS: [&str; 22] = [
    "interior", "seat", "steering_wheel", "speed", "fuel", "boost", "oiltemp", "oilpressure", "watertemp", "volt", "gaugea",
    "gaugeb", "gaugec", "gauged", "gaugee", "gaugef", "power", "doorcard", "tach", "boot", "gauge", "door_card",
];

const RIM_PREFIXES: [&str; 13] = [
    "rim", "ghostRim", "inner_rim", "outer_rim", "chrome_rim", "chrome_blur_rim", "chrome_blur_lip", "wheel_emblem", "wheel_black",
    "blur_rim", "blur_lip", "dropShadowRim", "geoShadowRim",
];
const TIRE_PREFIXES: [&str; 7] = ["tire_", "tread", "sidewall", "ghostTire", "dropShadowTire", "geoShadowTire", "scaling_text"];

/// Candidate technique names for (section, material), best first. The caller takes the first one
/// the library has (the game falls back to the plain name when a suffixed variant is missing;
/// INFERRED).
pub fn techniques(section: &str, material: &str, kind: CarMeshKind) -> Vec<String> {
    let s = section.to_ascii_lowercase();
    let m = material;
    let ml = m.to_ascii_lowercase();
    let mut out = Vec::new();
    let special = ["shadow", "smask", "black_glass", "blur", "ghost"].iter().any(|w| ml.contains(w));
    if kind.traffic && !special {
        let keep = ["light", "badge", "detail_glass", "numberplate", "matte_colors", "chrome"].iter().any(|w| ml.contains(w));
        if !keep {
            if ["body", "bumper", "trunk", "wing", "hood", "exhaust"].iter().any(|w| ml.contains(w)) {
                out.push("body_traffic".into());
            } else if ["interior", "seat", "gauge", "undercarriage", "rubber_trim"].iter().any(|w| ml.contains(w)) {
                out.push("black".into());
            } else if ml.starts_with("window") {
                out.push("window_traffic_dark".into());
            } else if ml.contains("glass") {
                out.push("window_traffic".into());
            }
        }
    }
    if (s.contains("rotor") || s.contains("caliper")) && !["ghost", "shadow", "brake_badge"].iter().any(|w| ml.contains(w)) {
        out.push(if s.contains("rotor") { "rotor".into() } else { "caliper".into() });
    }
    if kind.cockpit && INTERIOR_WORDS.iter().any(|w| s.contains(w)) {
        out.push(format!("cockpit_{m}"));
    }
    if RIM_PREFIXES.iter().any(|p| m.starts_with(p)) || TIRE_PREFIXES.iter().any(|p| m.starts_with(p)) || m == "tire" {
        out.push(format!("{m}_V2"));
    }
    if kind.superlod {
        if s.contains("engine") {
            out.push(format!("engine_{m}"));
        }
        if m != "grille1_alpha" {
            out.push(format!("{m}_s_lod0"));
        }
    }
    if s.starts_with("glassf") || s.starts_with("lexanf") {
        match m {
            "window" => out.push("frontwindow".into()),
            "window_2" => out.push("frontwindow_2".into()),
            _ => {}
        }
    }
    out.push(m.to_string());
    out
}

/// A parameter value from ShaderSettings: `value="f"` or `r g b a`.
pub type ParamValue = [f32; 4];

/// sRGB decode of one channel (0..1).
pub fn srgb_degamma(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// ShaderSettings colour channel as the game uploads it (sRGB-degammed; FH1_CARFX_COLOR_SRGB=0 = raw).
fn colour_degamma(c: f32) -> f32 {
    static RAW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *RAW.get_or_init(|| std::env::var("FH1_CARFX_COLOR_SRGB").is_ok_and(|v| v == "0")) {
        c
    } else {
        srgb_degamma(c)
    }
}

/// technique → (constant name → value); `samplers` lists `<XxxSampler/>` switches.
#[derive(Debug, Clone, Default)]
pub struct ShaderSettings {
    pub params: BTreeMap<String, BTreeMap<String, ParamValue>>,
    pub samplers: BTreeMap<String, Vec<String>>,
}

impl ShaderSettings {
    pub fn parse(xml: &str) -> Self {
        let mut out = Self::default();
        let mut tech: Option<String> = None;
        for raw in xml.split('<').skip(1) {
            let tag = raw.split('>').next().unwrap_or("").trim();
            if tag.starts_with('?') || tag.starts_with('!') || tag.is_empty() {
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                if tech.as_deref() == Some(name.trim()) {
                    tech = None;
                }
                continue;
            }
            let self_closing = tag.ends_with('/');
            let body = tag.trim_end_matches('/').trim();
            let name = body.split_whitespace().next().unwrap_or("").to_string();
            if name == "ShaderSettings" {
                continue;
            }
            match &tech {
                None => {
                    if !self_closing {
                        tech = Some(name);
                    }
                }
                Some(t) => {
                    let attr = |a: &str| -> Option<f32> {
                        let pat = format!("{a}=\"");
                        let i = body.find(&format!(" {pat}"))? + pat.len() + 1;
                        body[i..i + body[i..].find('"')?].trim().parse().ok()
                    };
                    let value = if let Some(v) = attr("value") {
                        Some([v, 0.0, 0.0, 0.0])
                    } else {
                        // Colour parameters (r/g/b/a form) reach the shader with rgb sRGB-degammed and alpha as written:
                        // GlassColor 0.1 -> 0.01002, SpecularColor 0.8 -> 0.6038, DiffuseColor 0.15 -> 0.01961,
                        // AmbientColor 0.7 -> 0.448 in the game's per-draw constants (Xenia capture 4543, Corrado;
                        // VERIFIED on 5 names). FH1_CARFX_COLOR_SRGB=0 keeps the raw values.
                        match (attr("r"), attr("g"), attr("b")) {
                            (Some(r), Some(g), Some(b)) => Some([colour_degamma(r), colour_degamma(g), colour_degamma(b), attr("a").unwrap_or(0.0)]),
                            _ => None,
                        }
                    };
                    match value {
                        Some(v) => {
                            out.params.entry(t.clone()).or_default().insert(name, v);
                        }
                        None if name.ends_with("Sampler") => out.samplers.entry(t.clone()).or_default().push(name),
                        None => {}
                    }
                }
            }
        }
        out
    }

    /// Overlay `later` on top of `self` (later files override per parameter; INFERRED).
    pub fn merge(&mut self, later: &ShaderSettings) {
        for (t, ps) in &later.params {
            let e = self.params.entry(t.clone()).or_default();
            for (k, v) in ps {
                e.insert(k.clone(), *v);
            }
        }
        for (t, s) in &later.samplers {
            let e = self.samplers.entry(t.clone()).or_default();
            for x in s {
                if !e.contains(x) {
                    e.push(x.clone());
                }
            }
        }
    }

    /// The game's chain: Normal → [Metallic|Matte] → [Slod] → car → ColorShaderSettings<N>.
    /// Missing files are skipped.
    pub fn chain(files: &[Option<&str>]) -> Self {
        let mut s = Self::default();
        for f in files.iter().flatten() {
            s.merge(&Self::parse(f));
        }
        s
    }

    /// Parameters for a technique (empty if none).
    pub fn for_technique(&self, t: &str) -> HashMap<String, ParamValue> {
        self.params.get(t).map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_like_the_game() {
        let k = CarMeshKind::default();
        assert_eq!(techniques("glassLTL", "detail_glass_red", k).last().unwrap(), "detail_glass_red");
        assert_eq!(techniques("wheelLF", "rim", k)[0], "rim_V2");
        assert_eq!(techniques("caliperLF", "red_paint", k)[0], "caliper");
        let c = CarMeshKind { cockpit: true, ..k };
        assert_eq!(techniques("seatL", "leather", c)[0], "cockpit_leather");
        assert_eq!(techniques("glassF", "window", k)[0], "frontwindow");
    }

    #[test]
    fn parses_settings() {
        let x = r#"<ShaderSettings>
  <glass>
    <GlassMaxOpacity value="0.3000000"/>
    <GlassColor r="0.1" g="0.2" b="0.3" a="0.07"/>
    <DirtSampler/>
  </glass>
</ShaderSettings>"#;
        let s = ShaderSettings::parse(x);
        assert_eq!(s.params["glass"]["GlassMaxOpacity"][0], 0.3);
        let c = s.params["glass"]["GlassColor"];
        assert!((c[0] - 0.01002).abs() < 1e-4 && (c[2] - 0.07324).abs() < 1e-4 && c[3] == 0.07);
        assert_eq!(s.samplers["glass"], vec!["DirtSampler".to_string()]);
    }
}

#[cfg(test)]
mod disc_tests {
    #[test]
    fn normal_xml_keys() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../re/out/carsh/Shared/ShaderSettings/Normal.xml");
        let Ok(x) = std::fs::read_to_string(p) else { return };
        let s = super::ShaderSettings::parse(&x);
        println!("techniques: {}", s.params.len() + s.samplers.keys().filter(|k| !s.params.contains_key(*k)).count());
        assert!(s.params.contains_key("body"));
    }
}

/// One vertex pool of an `.fxcar` section (raw big-endian bytes as stored in the carbin).
#[derive(Debug, Clone, Default)]
pub struct FxCarPool {
    pub stride: u32,
    pub count: u32,
    pub pool_min: [f32; 3],
    pub pool_max: [f32; 3],
    pub extra_stride: u32,
    pub data: Vec<u8>,
    pub extra: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FxCarSubsection {
    pub name: String,
    pub lod: i32,
    /// (x_off, x_scale, y_off, y_scale) for uv0 then uv1.
    pub uv_transform: [f32; 8],
    pub indices: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct FxCarSection {
    pub name: String,
    pub offset: [f32; 3],
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    /// LOD1-5 pool and LOD0 pool.
    pub pools: [FxCarPool; 2],
    pub subsections: Vec<FxCarSubsection>,
}

impl FxCarSection {
    pub fn pool_for(&self, sub: &FxCarSubsection) -> &FxCarPool {
        if sub.lod == 0 {
            &self.pools[1]
        } else {
            &self.pools[0]
        }
    }
}

#[derive(Debug, Clone)]
pub struct FxCar {
    pub type_id: u32,
    pub sections: Vec<FxCarSection>,
    /// shCompressionFactors (offset, scale) of this carbin (v3+; the game reads it from the model
    /// header, default.xex 0x82daa7fc).
    pub sh_compression: Option<[f32; 2]>,
}

/// Parse an `.fxcar` (written by fh1setup `carfx.rs`, which documents the layout).
pub fn parse_fxcar(b: &[u8]) -> Option<FxCar> {
    if b.get(..8)? != b"FH1FXCAR" {
        return None;
    }
    let mut p = 8;
    let u32n = |p: &mut usize| -> Option<u32> {
        let v = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
        *p += 4;
        Some(v)
    };
    let f32n = |p: &mut usize| -> Option<f32> { u32n(p).map(f32::from_bits) };
    let v3 = |p: &mut usize| -> Option<[f32; 3]> { Some([f32n(p)?, f32n(p)?, f32n(p)?]) };
    let name = |p: &mut usize| -> Option<String> {
        let n = *b.get(*p)? as usize;
        let s = String::from_utf8_lossy(b.get(*p + 1..*p + 1 + n)?).into_owned();
        *p += 1 + n;
        Some(s)
    };
    let version = u32n(&mut p)?;
    if version != 2 && version != 3 {
        return None;
    }
    let type_id = u32n(&mut p)?;
    let n = u32n(&mut p)? as usize;
    let mut sections = Vec::with_capacity(n);
    for _ in 0..n {
        let sname = name(&mut p)?;
        let offset = v3(&mut p)?;
        let bounds_min = v3(&mut p)?;
        let bounds_max = v3(&mut p)?;
        let pool = |p: &mut usize| -> Option<FxCarPool> {
            let stride = u32n(p)?;
            let count = u32n(p)?;
            let pool_min = v3(p)?;
            let pool_max = v3(p)?;
            let extra_stride = u32n(p)?;
            let extra_len = u32n(p)? as usize;
            let len = (stride * count) as usize;
            let data = b.get(*p..*p + len)?.to_vec();
            *p += len;
            let extra = b.get(*p..*p + extra_len)?.to_vec();
            *p += extra_len;
            Some(FxCarPool { stride, count, pool_min, pool_max, extra_stride, data, extra })
        };
        let pools = [pool(&mut p)?, pool(&mut p)?];
        let ns = u32n(&mut p)? as usize;
        let mut subsections = Vec::with_capacity(ns);
        for _ in 0..ns {
            let sub = name(&mut p)?;
            let lod = u32n(&mut p)? as i32;
            let mut uv = [0f32; 8];
            for x in &mut uv {
                *x = f32n(&mut p)?;
            }
            let ni = u32n(&mut p)? as usize;
            let idx = b.get(p..p + ni * 4)?;
            p += ni * 4;
            subsections.push(FxCarSubsection {
                name: sub,
                lod,
                uv_transform: uv,
                indices: idx.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect(),
            });
        }
        sections.push(FxCarSection { name: sname, offset, bounds_min, bounds_max, pools, subsections });
    }
    let sh_compression = if version >= 3 { Some([f32n(&mut p)?, f32n(&mut p)?]) } else { None };
    Some(FxCar { type_id, sections, sh_compression })
}

#[cfg(test)]
mod fxcar_tests {
    #[test]
    fn parses_installed_fxcar() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/installations");
        let Some(inst) = std::fs::read_dir(&root).ok().and_then(|mut r| r.next()).and_then(|e| e.ok()) else { return };
        let dir = inst.path().join("assets/private/cars/ALF_8C_08/fx");
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "fxcar") {
                let car = super::parse_fxcar(&std::fs::read(e.path()).unwrap()).expect("parse");
                let subs: usize = car.sections.iter().map(|s| s.subsections.len()).sum();
                let s0 = &car.sections[0];
                let p = &s0.pools[0];
                // Uniform pack scale check: (bounds extent) / (pool extent) per axis.
                let sc: Vec<f32> = (0..3).map(|i| (s0.bounds_max[i] - s0.bounds_min[i]) / (p.pool_max[i] - p.pool_min[i]).max(1e-9)).collect();
                println!("{:?}: {} sections, {subs} subs; {} stride {} count {} extra {} ; scale {:?}", e.file_name(), car.sections.len(), s0.name, p.stride, p.count, p.extra_stride, sc);
            }
        }
    }
}

// ---------------------------------------------------------------- building cars

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::prelude::*;

use crate::car_material::{FxCarConsts, FxCarMaterial};
use crate::material::{ATTRIBUTE_CAR_POSITION, ATTRIBUTE_CAR_SH0, ATTRIBUTE_CAR_TANFRAME, ATTRIBUTE_CAR_UV0, ATTRIBUTE_CAR_UV1};
use crate::program::{Family, Program};
use crate::{FxCarGlobals, FxLibrary};

/// Per-part and paint constants that live in the car material register file.
pub const PART_CONSTANTS: [&str; 25] = [
    "pack_partPosition",
    "uv1CompressionFactors",
    "uv2CompressionFactors",
    "shCompressionFactors",
    "boundingBoxScale",
    "boundingBoxOffset",
    "damageClamps",
    "vsUseMorph",
    "PaintColor",
    "psPaintColor",
    "TwoToneColor",
    "TwoToneScale",
    "TwoToneBias",
    "TwoTonePower",
    "psPaintTwoToneColor",
    "psPaintTwoToneScale",
    "psPaintTwoToneBias",
    "psPaintTwoTonePower",
    "GlassColor",
    "FresnelIndex",
    "FresnelScalar",
    "MaxGlassOpacity",
    "GlassMaxOpacity",
    "StripeColor",
    "caliperTintColor",
];

/// Constants the game sets from code that are not per-frame globals: they sit at different registers in
/// different v16 programs (LightmapPower c124/c125/c127/c159/c169, ...), so a register-indexed global bank
/// mixes them up: the interior PS read PristinePaintScale at c159 as another program's SpecularPower
/// default 128 (white interiors; VERIFIED from the constant tables). As material registers they take each
/// program's own constant-table default, and `CarShading::build` sets the ones without one.
pub const CODE_CONSTANTS: [&str; 16] = [
    // detail_glass pass 1 fresnel (PS 288/290/292): set per draw by the game, values from Slod.xml (see LENS_FRESNEL).
    "Direct",
    "Glancing",
    "FresnelBiasA",
    "DamageAlphaEnvironmentKillAmount",
    "DamageAlphaScale",
    "DarkValues",
    "LightValues",
    "FogRedColor",
    "FogWhiteColor",
    "HeadlightPower",
    "LightmapPower",
    "PristinePaintScale",
    "SHBTransitionHi",
    "SHBTransitionLo",
    "StripeMetallicAmt",
    "StripePaintScale",
];

/// detail_glass_* pass-1 fresnel constants (Slod.xml values; see `CarShading::build`).
const LENS_FRESNEL: [(&str, f32); 3] = [("Direct", 0.07), ("Glancing", 0.15), ("FresnelBiasA", 0.0)];

/// Names that are material registers for car programs: every ShaderSettings parameter plus the
/// per-part constants.
pub fn material_names(settings: &ShaderSettings) -> Arc<HashSet<String>> {
    let mut s: HashSet<String> = settings.params.values().flat_map(|m| m.keys().cloned()).collect();
    s.extend(PART_CONSTANTS.iter().map(|x| x.to_string()));
    s.extend(CODE_CONSTANTS.iter().map(|x| x.to_string()));
    Arc::new(s)
}

/// A car mesh for one subsection: raw pool vertices, byte-swapped to little-endian, as the
/// translated car vertex shader expects them.
pub fn subsection_mesh(section: &FxCarSection, sub: &FxCarSubsection) -> Option<Mesh> {
    let pool = section.pool_for(sub);
    let stride = pool.stride as usize;
    if stride < 28 || sub.indices.is_empty() {
        return None;
    }
    let mut remap = HashMap::new();
    let mut order = Vec::new();
    let mut indices = Vec::with_capacity(sub.indices.len());
    for &i in &sub.indices {
        if (i as usize) >= pool.count as usize {
            return None;
        }
        let n = *remap.entry(i).or_insert_with(|| {
            order.push(i as usize);
            (order.len() - 1) as u32
        });
        indices.push(n);
    }
    let d = &pool.data;
    let i16be = |o: usize| i16::from_be_bytes([d[o], d[o + 1]]);
    let u16be = |o: usize| u16::from_be_bytes([d[o], d[o + 1]]);
    let mut pos = Vec::with_capacity(order.len());
    let mut uv0 = Vec::with_capacity(order.len());
    let mut uv1 = Vec::with_capacity(order.len());
    let mut quat = Vec::with_capacity(order.len());
    let mut sh = Vec::with_capacity(order.len());
    for &v in &order {
        let o = v * stride;
        pos.push([i16be(o), i16be(o + 2), i16be(o + 4), i16be(o + 6)]);
        uv0.push([u16be(o + 8), u16be(o + 10)]);
        uv1.push([u16be(o + 12), u16be(o + 14)]);
        quat.push([i16be(o + 16), i16be(o + 18), i16be(o + 20), i16be(o + 22)]);
        sh.push(u32::from_be_bytes(d[o + 24..o + 28].try_into().unwrap()));
    }
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
    m.insert_attribute(ATTRIBUTE_CAR_POSITION, VertexAttributeValues::Snorm16x4(pos));
    m.insert_attribute(ATTRIBUTE_CAR_UV0, VertexAttributeValues::Unorm16x2(uv0));
    m.insert_attribute(ATTRIBUTE_CAR_UV1, VertexAttributeValues::Unorm16x2(uv1));
    m.insert_attribute(ATTRIBUTE_CAR_TANFRAME, VertexAttributeValues::Snorm16x4(quat));
    m.insert_attribute(ATTRIBUTE_CAR_SH0, VertexAttributeValues::Uint32(sh));
    m.insert_indices(Indices::U32(indices));
    Some(m)
}

/// pack_partPosition = (section offset, |bounds half-diagonal|): the VS computes
/// pos = xyz·w·S + T. Its setter in default.xex is untraced; this matches the carbin pool-bbox
/// remap within 0.08% on all 14,833 pools of 1,880 fxcars (the box centre is ~0) (INFERRED).
pub fn pack_part_position(section: &FxCarSection, _pool: &FxCarPool) -> [f32; 4] {
    let h = Vec3::from_array(section.bounds_max) - Vec3::from_array(section.bounds_min);
    let o = section.offset;
    [o[0], o[1], o[2], (h * 0.5).length()]
}

/// boundingBoxOffset / boundingBoxScale (damage lattice space): min/max of section offset +
/// bounds over the car's sections, then min.y -= 0.3 (VERIFIED, default.xex 0x82da2298; the game
/// skips detached sections and those without flag +0x118, which the fxcar doesn't carry).
pub fn car_bounds<'a>(sections: impl IntoIterator<Item = &'a FxCarSection>) -> Option<(Vec3, Vec3)> {
    let mut b: Option<(Vec3, Vec3)> = None;
    for s in sections {
        let o = Vec3::from_array(s.offset);
        let (lo, hi) = (o + Vec3::from_array(s.bounds_min), o + Vec3::from_array(s.bounds_max));
        b = Some(b.map_or((lo, hi), |(a, c)| (a.min(lo), c.max(hi))));
    }
    b.map(|(lo, hi)| (lo - Vec3::Y * 0.3, hi))
}

/// shCompressionFactors when the fxcar predates v3: the typical (offset, scale) seen in carbin
/// headers (e.g. -0.33793, 0.67674).
pub const SH_COMPRESSION_DEFAULT: [f32; 2] = [-0.33793, 0.67674];

/// Set a named constant in a car material's register files.
fn set_named(consts: &mut FxCarConsts, program: &Program, name: &str, values: &[[f32; 4]]) {
    for (n, stage, reg, count) in &program.named {
        if n != name {
            continue;
        }
        let file = if *stage == fh1_shaders::container::Stage::Vertex { &mut consts.vs } else { &mut consts.ps };
        for (k, v) in values.iter().take(*count as usize).enumerate() {
            if let Some(r) = file.get_mut(*reg as usize + k) {
                *r = Vec4::from_array(*v);
            }
        }
    }
}

/// Everything needed to turn `.fxcar` subsections into game-shaded meshes.
pub struct CarShading {
    /// Library effect name registered in the FxLibrary (e.g. "shaders_v16").
    pub library: String,
    pub settings: ShaderSettings,
    pub family: Family,
    pub kind: CarMeshKind,
    /// Technique names the library has.
    pub techniques: HashSet<String>,
    /// Car-wide damage-lattice bounds (see `car_bounds`).
    pub bounds: Option<(Vec3, Vec3)>,
    /// shCompressionFactors (offset, scale) of the carbin being built.
    pub sh_compression: [f32; 2],
    /// Paint colour (gamedb Combo_Colors.RGB / 255), if chosen.
    pub paint: Option<Vec3>,
}

impl CarShading {
    pub fn new(library: &str, library_bytes: &[u8], settings: ShaderSettings, lib: &mut FxLibrary) -> Option<Self> {
        lib.add_effect(library, library_bytes).ok()?;
        let fx = fh1_shaders::effect::Effect::parse(library_bytes).ok()?;
        let techniques = fx.techniques.iter().map(|t| t.name.clone()).collect();
        let family = Family::Car { material_names: material_names(&settings) };
        Some(Self {
            library: library.to_string(),
            settings,
            family,
            kind: CarMeshKind::default(),
            techniques,
            bounds: None,
            sh_compression: SH_COMPRESSION_DEFAULT,
            paint: None,
        })
    }

    fn family_is_material(&self, name: &str) -> bool {
        match &self.family {
            Family::Car { material_names } => material_names.contains(name),
            _ => false,
        }
    }

    /// The technique a (section, material) resolves to, if the library has one.
    pub fn technique(&self, section: &str, material: &str) -> Option<String> {
        techniques(section, material, self.kind).into_iter().find(|t| self.techniques.contains(t))
    }

    /// A material for one subsection, untextured.
    pub fn material(
        &self,
        section: &FxCarSection,
        sub: &FxCarSubsection,
        lib: &mut FxLibrary,
        globals: &mut FxCarGlobals,
        shaders: &mut Assets<Shader>,
    ) -> Option<FxCarMaterial> {
        self.build(section, sub, 0, lib, globals, shaders).map(|(m, _, _)| m)
    }

    /// A material for one subsection with its textures bound.
    #[allow(clippy::too_many_arguments)]
    pub fn material_textured(
        &self,
        section: &FxCarSection,
        sub: &FxCarSubsection,
        lib: &mut FxLibrary,
        globals: &mut FxCarGlobals,
        shaders: &mut Assets<Shader>,
        textures: &mut CarTextures,
        images: &mut Assets<Image>,
    ) -> Option<FxCarMaterial> {
        self.material_textured_pass(section, sub, 0, lib, globals, shaders, textures, images)
    }

    /// [`Self::material_textured`] for pass `pass` of the technique (None when it has no such pass).
    #[allow(clippy::too_many_arguments)]
    pub fn material_textured_pass(
        &self,
        section: &FxCarSection,
        sub: &FxCarSubsection,
        pass: usize,
        lib: &mut FxLibrary,
        globals: &mut FxCarGlobals,
        shaders: &mut Assets<Shader>,
        textures: &mut CarTextures,
        images: &mut Assets<Image>,
    ) -> Option<FxCarMaterial> {
        let (mut m, program, tech) = self.build(section, sub, pass, lib, globals, shaders)?;
        let bound = textures.bind(&mut m, &program, &tech, sub.lod == 0, images);
        // FH1_CARFX_LOG=1: one line per part (section, material, technique, sampler -> texture).
        if std::env::var_os("FH1_CARFX_LOG").is_some() {
            info!("carfx {} / {} -> {tech} blend={} cull={:?} [{}]", section.name, sub.name, m.alpha_blend, program.state.cull, bound.join(", "));
        }
        // FH1_CARFX_DUMP=<file>: one JSON line per part with every named constant the program reads (material
        // registers by value, globals as "G"), to diff against a game capture (re/xenia/carfx_cmp.py).
        if let Ok(path) = std::env::var("FH1_CARFX_DUMP") {
            let mut regs = serde_json::Map::new();
            for (n, st, reg, count) in &program.named {
                for k in 0..*count {
                    let r = reg + k;
                    let key = format!("{}{r}", if *st == fh1_shaders::container::Stage::Vertex { "vs" } else { "ps" });
                    let v = if self.family_is_material(n) || PART_CONSTANTS.contains(&n.as_str()) {
                        let file = if *st == fh1_shaders::container::Stage::Vertex { &m.consts.vs } else { &m.consts.ps };
                        serde_json::json!([n, file[r as usize].to_array()])
                    } else {
                        serde_json::json!([n, "G"])
                    };
                    regs.insert(key, v);
                }
            }
            // The effect's shader indices of this pass (fxdump numbering = rdoc_query "<fxobj>#<k>" ids), so carfx_cmp.py can
            // tell same-layout PS programs apart (plastic2 vs matte_colors, the lamp family).
            let (vs_shader, ps_shader) = lib
                .effects
                .get(&self.library.to_ascii_lowercase())
                .and_then(|fx| fx.techniques.iter().find(|t| t.name == tech))
                .and_then(|t| t.passes.get(pass))
                .map_or((None, None), |p| (p.vs, p.ps));
            let line = serde_json::json!({"section": section.name, "material": sub.name, "tech": tech, "pass": pass, "lod": sub.lod,
                "blend": m.alpha_blend, "additive": m.additive, "bound": bound, "library": self.library,
                "vs_shader": vs_shader, "ps_shader": ps_shader, "regs": regs});
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(f, "{line}");
            }
        }
        Some(m)
    }

    fn build(
        &self,
        section: &FxCarSection,
        sub: &FxCarSubsection,
        pass: usize,
        lib: &mut FxLibrary,
        globals: &mut FxCarGlobals,
        shaders: &mut Assets<Shader>,
    ) -> Option<(FxCarMaterial, Arc<Program>, String)> {
        let tech = self.technique(&section.name, &sub.name)?;
        // FH1_CARFX_SKIP=tech,tech: leave those techniques out (debugging aid).
        if std::env::var("FH1_CARFX_SKIP").is_ok_and(|v| v.split(',').any(|t| t == tech)) {
            return None;
        }
        // program::build reads `name#k` as pass k of the technique.
        let program_name = if pass == 0 { tech.clone() } else { format!("{tech}#{pass}") };
        let (pid, program) = lib.program_family(&self.library, &program_name, &self.family, shaders, &mut globals.0)?;
        if std::env::var_os("FH1_CARFX_LOG").is_some() {
            // The effect hash FH1_FX_DEBUG=<hash>:<mode> matches (every technique of the car library).
            if let Some(fx) = lib.effects.get(&self.library.to_ascii_lowercase()) {
                info!("carfx library {} effect hash {:08x}", self.library, fx.hash);
            }
        }
        // FH1_CARFX_WGSL=<dir>: write each technique's translated WGSL (debugging aid).
        if let Some(dir) = std::env::var_os("FH1_CARFX_WGSL") {
            let dir = PathBuf::from(dir);
            let _ = std::fs::write(dir.join(format!("{tech}.wgsl")), &program.wgsl);
            let mut t = String::new();
            for (n, st, reg, count) in &program.named {
                let mat = self.family_is_material(n);
                let d = program.material_defaults.get(&(*st, *reg));
                t += &format!("{st:?} c{reg} x{count} {n} {} default={d:?}
", if mat { "MAT" } else { "GLOB" });
            }
            let _ = std::fs::write(dir.join(format!("{tech}.consts.txt")), t);
        }
        let mut consts = FxCarConsts::default();
        for (&(stage, reg), v) in &program.material_defaults {
            let file = if stage == fh1_shaders::container::Stage::Vertex { &mut consts.vs } else { &mut consts.ps };
            if let Some(r) = file.get_mut(reg as usize) {
                *r = Vec4::from_array(*v);
            }
        }
        for (k, v) in self.settings.for_technique(&tech) {
            set_named(&mut consts, &program, &k, &[v]);
        }
        if tech.starts_with("detail_glass") {
            // Lens reflection fresnel, set by game code in race (untraced); the values are Slod.xml's detail_glass_*
            // entries (Direct 0.07, Glancing 0.15, FresnelBias 0). INFERRED for the race library.
            for (n, v) in LENS_FRESNEL {
                set_named(&mut consts, &program, n, &[[v, 0.0, 0.0, 0.0]]);
            }
        }
        // The paint setter (0x82DAEC98) writes PaintColor only on the paint techniques: the game's emblem draws keep
        // the ShaderSettings PaintColor 0 (Xenia 4543, VERIFIED); before, every technique with a PaintColor took the
        // car colour. FH1_CARFX_PAINT_ALL=1 = old.
        // Paint techniques = those whose Normal.xml PaintColor is the body placeholder (body, body_sh, body_traffic,
        // cockpit_body, engine_body, headlight_paint, lights_body) plus stripe_badge and body_2/_3 (untested, kept painted).
        const PAINT_TECHS: [&str; 10] = ["body", "body_2", "body_3", "body_sh", "body_traffic", "cockpit_body", "engine_body", "headlight_paint", "lights_body", "stripe_badge"];
        let paint_tech = PAINT_TECHS.contains(&tech.as_str()) || std::env::var("FH1_CARFX_PAINT_ALL").is_ok_and(|v| v == "1");
        if let Some(p) = self.paint.filter(|_| paint_tech) {
            // PaintColor = Combo_Colors.RGB degammed (setter 0x82DAEC98, VERIFIED; docs/SHADERS.md).
            let c = p.extend(1.0).to_array();
            set_named(&mut consts, &program, "PaintColor", &[c]);
            set_named(&mut consts, &program, "psPaintColor", &[c]);
        }
        // TwoTone* come from code (List_SpecialColors; no special colour = Color 0, Scale 0, Bias 0, Power 20) on every
        // technique: the game's emblem has (0, 0, 0, 20) where Normal.xml says Bias 0.08 / Power 2 (Xenia 4543, VERIFIED).
        if !std::env::var("FH1_CARFX_PAINT_ALL").is_ok_and(|v| v == "1") {
            for (n, v) in [("TwoToneColor", [0.0, 0.0, 0.0, 1.0]), ("TwoToneScale", [0.0; 4]), ("TwoToneBias", [0.0; 4]), ("TwoTonePower", [20.0, 0.0, 0.0, 0.0])] {
                set_named(&mut consts, &program, n, &[v]);
                set_named(&mut consts, &program, &format!("psPaint{n}"), &[v]);
            }
        }
        // FH1_CARFX_CONST=tech:Name=x,y,z,w;...: override a material constant (debugging aid; `*` = every technique).
        if let Ok(spec) = std::env::var("FH1_CARFX_CONST") {
            for item in spec.split(';') {
                let Some((t, rest)) = item.split_once(':') else { continue };
                let Some((n, v)) = rest.split_once('=') else { continue };
                if t != "*" && t != tech {
                    continue;
                }
                let mut x = [0f32; 4];
                for (d, s) in x.iter_mut().zip(v.split(',')) {
                    *d = s.trim().parse().unwrap_or(0.0);
                }
                set_named(&mut consts, &program, n, &[x]);
            }
        }
        let pool = section.pool_for(sub);
        set_named(&mut consts, &program, "pack_partPosition", &[pack_part_position(section, pool)]);
        let uv = sub.uv_transform;
        set_named(&mut consts, &program, "uv1CompressionFactors", &[[uv[0], uv[1], uv[2], uv[3]]]);
        set_named(&mut consts, &program, "uv2CompressionFactors", &[[uv[4], uv[5], uv[6], uv[7]]]);
        let [sh_off, sh_scale] = self.sh_compression;
        set_named(&mut consts, &program, "shCompressionFactors", &[[sh_off, sh_scale, 0.0, 0.0]]);
        let (lo, hi) = self.bounds.unwrap_or((Vec3::new(-1.0, -0.3, -2.5), Vec3::new(1.0, 1.5, 2.5)));
        set_named(&mut consts, &program, "boundingBoxOffset", &[lo.extend(0.0).to_array()]);
        set_named(&mut consts, &program, "boundingBoxScale", &[(Vec3::ONE / (hi - lo).max(Vec3::splat(1e-3))).extend(1.0).to_array()]);
        // (0, R+0x74, TextureDamageScale, MorphDamageScale) from GlobalCarAttribs DamageAttribs
        // (VERIFIED); R+0x74 is unknown, 0 clamps damage to none. No morph buffer: vsUseMorph 0.
        set_named(&mut consts, &program, "damageClamps", &[[0.0, 0.0, 1.0, 6.0]]);
        set_named(&mut consts, &program, "vsUseMorph", &[[0.0; 4]]);
        // Undamaged glass: the window PS fetches DamageGlassSampler when glassdamageAmount[glassdamageAmountIndex] > 0.
        set_named(&mut consts, &program, "glassdamageAmount", &[[0.0; 4]; 40]);
        set_named(&mut consts, &program, "glassdamageAmountIndex", &[[0.0; 4]]);
        // FH1_CARFX_CULL=tech:none|flip,...: override the pass cull (debugging aid; `*` = every technique).
        let cull_override = std::env::var("FH1_CARFX_CULL").ok().and_then(|v| {
            v.split(',').filter_map(|i| i.split_once(':')).find(|(t, _)| *t == "*" || *t == tech).map(|(_, m)| m.to_string())
        });
        let m = FxCarMaterial {
            consts,
            globals: globals.0.buffer(),
            t0: None,
            t1: None,
            t2: None,
            t3: None,
            t4: None,
            t5: None,
            t6: None,
            t7: None,
            t10: None,
            t13: None,
            cube0: None,
            cube1: None,
            cube2: None,
            headlights: crate::headlight::HEADLIGHT_BUFFER,
            program: pid,
            flip_cull: cull_override.as_deref() == Some("flip"),
            no_cull: cull_override.as_deref() == Some("none"),
            // Glass and lamp-lens techniques (PS reads GlassMaxOpacity) blend over the car; the car
            // library's passes carry no blend state, the game sets it in code (INFERRED).
            alpha_blend: program.state.blend.is_some()
                || program.named.iter().any(|(n, st, _, _)| n == "GlassMaxOpacity" && *st == fh1_shaders::container::Stage::Pixel),
            additive: false,
            sort_bias: 0.0,
            // Lamp covers write depth although blended. The game draws a lamp's cover (glassLTL lights_gls_*) before
            // the layers behind it (taillightL reflector / taillight2S / reverse_light, carbin order; Xenia 4543 eids
            // 9996-10020 before 10088-10098) and still shows a solid cover, so the later layers must fail the depth
            // test there. Without it the bulb/reflector layers painted over the covers (covers popping in and out
            // under Bevy's tied transparent sort, gone with a stable carbin order). FH1_CARFX_LENS_DEPTH=0 = off.
            depth_write: lens_cover(&tech) && !std::env::var("FH1_CARFX_LENS_DEPTH").is_ok_and(|v| v == "0"),
            lens: lens_cover(&tech),
            order: 0,
        };
        Some((m, program, tech))
    }
}

// ---------------------------------------------------------------- car textures

/// One installed texture (fh1setup `tex.json` entry, written by the cars group).
#[derive(Clone)]
struct TexEntry {
    file: PathBuf,
    /// Fetch-constant dword 0: sign bits (w0 >> 2) & 0xFF, 2 bits per channel, 3 = gamma.
    w0: u32,
    cube: bool,
}

/// Car textures by name (the path inside the zip without extension, e.g. `nodamage_LOD0`),
/// searched per car first, then Shared, then the track cube maps. Images load on first use.
pub struct CarTextures {
    dirs: Vec<HashMap<String, TexEntry>>,
    loaded: HashMap<PathBuf, (Handle<Image>, bool)>,
    /// Static environment cube for this track (`cubemaps/<track>`).
    pub track: String,
    /// The live env cube (`reflect::EnvCube::dynamic`) for `envSampler`, when the track renders one.
    pub env_dynamic: Option<Handle<Image>>,
}

impl CarTextures {
    /// `assets` = install `assets/private`.
    pub fn new(assets: &std::path::Path, car: &str, track: &str) -> Self {
        let cars = assets.join("cars");
        let dirs = [cars.join(car).join("fx/tex"), cars.join("shared/tex"), cars.join("cubemaps")]
            .iter()
            .map(|d| {
                let Some(j) = crate::files::read_json(&d.join("tex.json")) else {
                    return HashMap::new();
                };
                j.as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, v)| {
                        // Keys keep the disc's casing (FER_Enzo_02 has `Nodamage`, `Lights`); the game's file
                        // system is case-insensitive, so lookups are too (an unbound baseSampler drew white bodies).
                        Some((
                            k.to_ascii_lowercase(),
                            TexEntry { file: d.join(v["file"].as_str()?), w0: v["w0"].as_u64()? as u32, cube: v["cube"].as_bool().unwrap_or(false) },
                        ))
                    })
                    .collect()
            })
            .collect();
        Self { dirs, loaded: HashMap::new(), track: track.to_string(), env_dynamic: None }
    }

    fn entry(&self, name: &str) -> Option<&TexEntry> {
        let name = name.to_ascii_lowercase();
        self.dirs.iter().find_map(|d| d.get(&name))
    }

    /// The image and its gamma flag; `lod0` prefers the `_LOD0` variant.
    pub fn get(&mut self, name: &str, lod0: bool, images: &mut Assets<Image>) -> Option<(Handle<Image>, bool, bool)> {
        let e = if lod0 { self.entry(&format!("{name}_LOD0")).or_else(|| self.entry(name)) } else { self.entry(name).or_else(|| self.entry(&format!("{name}_LOD0"))) }?.clone();
        if let Some((h, g)) = self.loaded.get(&e.file) {
            return Some((h.clone(), *g, e.cube));
        }
        let image = crate::scenery::read_dds_cached(&e.file)?;
        let gamma = (e.w0 >> 2) & 0x3F == 0x3F;
        let h = images.add(image);
        self.loaded.insert(e.file.clone(), (h.clone(), gamma));
        Some((h, gamma, e.cube))
    }

    /// The texture a car sampler binds (INFERRED by name until the default.xex binding is traced).
    fn source(&self, sampler: &str, technique: &str) -> Option<String> {
        let t = technique.to_ascii_lowercase();
        Some(
            match sampler {
                // The exterior model's `interior` technique samples the body atlas (docs/CAR_INTERIOR.md);
                // interior_LOD0 is the cockpit model's.
                "baseSampler" if t.starts_with("cockpit_") => "interior",
                "baseSampler" => "nodamage",
                "damageSampler" => "damage",
                "LightsSampler" => "lights",
                "DamageLightsSampler" => "damagelights",
                "CockpitInteriorHiSampler" => "interior",
                "CockpitGaugeEmissiveSampler" => "interior_emissive",
                "DirtMaskSampler" => "dirt_mask",
                "DirtSampler" => "dirt_pattern",
                "CarbonFiberSampler" | "CarbonFiberDiffuseSampler" => "carbonFiber",
                "BumperFrameSampler" => "bumper_frame",
                "Grille1Sampler" => "grille1",
                "Grille2Sampler" => "grille2",
                "CloudsSampler" => "clouds_s",
                "DamageGlassSampler" => "glass_texture",
                // envStaticSampler = tracks\<trk>\staticCarCubemap (loader 0x82D5EBB0, docs/SHADERS.md
                // "Reflections"); envSampler is the live cube (bound in `bind`), else the same static cube.
                "envStaticSampler" | "envSampler" | "envBackgroundSampler" => return Some(self.track.clone()),
                _ => return None,
            }
            .to_string(),
        )
    }

    /// Bind every sampler of `program` that has a known source into `m`.
    /// Returns `sampler=texture` per sampler (`?` = no source, `!` = missing file, `~` = 2D/cube mismatch).
    pub fn bind(&mut self, m: &mut FxCarMaterial, program: &Program, technique: &str, lod0: bool, images: &mut Assets<Image>) -> Vec<String> {
        let mut log = Vec::new();
        for (tf, dim) in &program.textures {
            let Some(name) = program.samplers.iter().find(|(r, _)| r == tf).map(|(_, n)| n.clone()) else { continue };
            if let (Some(h), "envSampler", 3) = (&self.env_dynamic, name.as_str(), *dim) {
                log.push(format!("{name}=dynamicCube"));
                if let Some(s) = m.slot_mut(100 + crate::program::cube_slot(*tf) as u32) {
                    *s = Some(h.clone());
                }
                continue;
            }
            let Some(src) = self.source(&name, technique) else {
                log.push(format!("{name}=?"));
                continue;
            };
            let Some((h, gamma, cube)) = self.get(&src, lod0, images) else {
                log.push(format!("{name}=!{src}"));
                continue;
            };
            let want_cube = *dim == 3;
            if cube != want_cube {
                log.push(format!("{name}=~{src}"));
                continue;
            }
            log.push(format!("{name}={src}"));
            let slot = if want_cube { 100 + crate::program::cube_slot(*tf) as u32 } else { *tf };
            if let Some(s) = m.slot_mut(slot) {
                *s = Some(h);
                if gamma && !want_cube {
                    m.consts.gamma.x |= 1 << tf;
                }
            }
        }
        log
    }
}

// ---------------------------------------------------------------- part selection

/// The stock body-kit letter per kit stem, from `model.json` `kit` (fh1setup model.rs `Kit`: `a` + the stock row's
/// gamedb Sequence; only stems whose letter isn't `a` are written, so VW_IECorrado_95 = bumperr c, skirtl/r b).
/// Missing file or key = `a` everywhere (the old rule).
#[derive(Clone, Debug, Default)]
pub struct StockKit(pub Vec<(String, char)>);

impl StockKit {
    /// `model.json` `kit` of `cars/<car>/`.
    pub fn load(assets: &std::path::Path, car: &str) -> Self {
        let model: Option<serde_json::Value> = crate::files::read_json(&assets.join("cars").join(car).join("model.json"));
        Self::from_model(model.as_ref())
    }

    pub fn from_model(model: Option<&serde_json::Value>) -> Self {
        let Some(kit) = model.and_then(|m| m.get("kit")).and_then(|k| k.as_object()) else { return Self::default() };
        Self(
            kit.iter()
                .filter_map(|(stem, l)| Some((stem.to_ascii_lowercase(), l.as_str()?.chars().next()?.to_ascii_lowercase())))
                .collect(),
        )
    }

    pub fn letter(&self, stem: &str) -> char {
        self.0.iter().find(|(s, _)| s == stem).map_or('a', |(_, l)| *l)
    }
}

/// Stock parts only: drop body-kit letters other than the stock one ([`StockKit`], `a` on all but one car), race
/// parts, lexan windows and cages (same rule as fh1setup's glTF export, model.rs `is_stock`).
pub fn is_stock(name: &str, kit: &StockKit) -> bool {
    let n = name.to_ascii_lowercase();
    if n.starts_with("lexan") || n.starts_with("cage") || n.ends_with("race") {
        return false;
    }
    const STEMS: [&str; 10] = ["bumperf", "bumperr", "hood", "wing", "skirtl", "skirtr", "exhaustl", "exhaustr", "exhaust", "undercarriage"];
    for stem in STEMS {
        if let Some(rest) = n.strip_prefix(stem) {
            let mut ch = rest.chars();
            if let Some(letter) = ch.next().filter(|c| c.is_ascii_lowercase()) {
                let tail = ch.as_str();
                if tail.is_empty() || tail.starts_with('_') {
                    return letter == kit.letter(stem);
                }
            }
        }
    }
    true
}

/// Is this a wheel section (drawn per corner by the caller, not with the body)?
pub fn is_wheel(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("wheel")
}

/// Artist placeholders: subsections all named `<section>_LOD<n>` (an 8-triangle 0.2 m cube).
fn is_placeholder(s: &FxCarSection) -> bool {
    let name = s.name.to_ascii_lowercase();
    !s.subsections.is_empty()
        && s.subsections.iter().all(|x| x.name.to_ascii_lowercase().strip_prefix(name.as_str()).is_some_and(|r| r.starts_with("_lod")))
}

fn best_lod(s: &FxCarSection, lod0: bool) -> Option<i32> {
    s.subsections.iter().filter(|x| (x.lod == 0) == lod0).map(|x| x.lod).min()
}

fn tris(s: &FxCarSection, lod: Option<i32>) -> usize {
    s.subsections.iter().filter(|x| Some(x.lod) == lod).map(|x| x.indices.len() / 3).sum()
}

/// The body subsections to draw: for every stock, non-wheel section of the main carbin, the `_lod0` file's LOD0
/// geometry when it has that section with at least half the LOD1 triangles (else the stub is skipped and LOD1
/// drawn), plus the sections only the `_lod0` file has when the LOD0 body itself is drawn.
pub fn body_parts<'a>(main: &'a FxCar, lod0: Option<&'a FxCar>, kit: &StockKit) -> Vec<(&'a FxCarSection, &'a FxCarSubsection)> {
    let mut out = Vec::new();
    let mut body_is_lod0 = false;
    for s in main.sections.iter().filter(|s| is_stock(&s.name, kit) && !is_wheel(&s.name) && !is_placeholder(s)) {
        let l1 = best_lod(s, false);
        let hi = lod0
            .and_then(|c| c.sections.iter().find(|x| x.name.eq_ignore_ascii_case(&s.name)))
            .filter(|h| !is_placeholder(h) && best_lod(h, true).is_some() && tris(h, Some(0)) * 2 >= tris(s, l1));
        if s.name.eq_ignore_ascii_case("body") {
            body_is_lod0 = hi.is_some();
        }
        let (sec, lod) = match hi {
            Some(h) => (h, Some(0)),
            None => (s, l1),
        };
        out.extend(sec.subsections.iter().filter(|x| Some(x.lod) == lod).map(|x| (sec, x)));
    }
    if let (Some(l0), true) = (lod0, body_is_lod0 && std::env::var("FH1_LOD0_ONLY").as_deref() != Ok("0")) {
        for h in &l0.sections {
            if main.sections.iter().any(|s| s.name.eq_ignore_ascii_case(&h.name)) || is_wheel(&h.name) || !is_stock(&h.name, kit) || is_placeholder(h) {
                continue;
            }
            if tris(h, Some(0)) >= 16 {
                out.extend(h.subsections.iter().filter(|x| x.lod == 0).map(|x| (h, x)));
            }
        }
    }
    out
}

/// Everything for drawing a car body with the game's shaders: (mesh, material) per stock body
/// part, in carbin mesh space (the same space as fh1setup's model.gltf). `paint` = gamedb
/// Combo_Colors (RGB, Metallic, Sequence) of the chosen colour. Wheels are not included.
#[allow(clippy::too_many_arguments)]
pub fn load_body(
    assets: &std::path::Path,
    car: &str,
    track: &str,
    paint: Option<(u32, bool, u32)>,
    lib: &mut FxLibrary,
    globals: &mut FxCarGlobals,
    shaders: &mut Assets<Shader>,
    images: &mut Assets<Image>,
) -> Option<Vec<(Mesh, FxCarMaterial)>> {
    Some(load_body_parts(assets, car, track, paint, lib, globals, shaders, images)?.into_iter().map(|p| (p.mesh, p.material)).collect())
}

// ---------------------------------------------------------------- engine hook

/// Draw a car through the game's car shaders. Insert on the entity that holds the car's glTF
/// scene (`cars/<CAR>/model.gltf`, whose root node carries `model.json` `mesh_offset`). The body
/// parts spawn as children in the same frame as the glTF root, and the glTF nodes they replace are
/// hidden once the scene has spawned; wheels, rotors and calipers stay glTF for now. The entity also
/// gets `FxCarLit` (the car SH lighting is rotated into its orientation). Needs [`FxCarPlugin`].
#[derive(Component, Clone)]
pub struct FxCarBody {
    /// Install `assets/private`.
    pub assets: PathBuf,
    pub car: String,
    /// Track whose static cube map the car reflects (`cars/cubemaps/<track>`).
    pub track: String,
}

/// On an [`FxCarBody`]: share the built body parts (meshes + materials) with every other shared body of the same car,
/// paint and cube: the first one builds them (.fxcar reads, mesh decode, materials), the next ones only spawn entities.
/// For pools of AI / traffic cars; the player's car doesn't carry it, so its path is unchanged. Lamps live in the one
/// global car bank (`update_car_lamps`), so sharing materials changes nothing per car.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct FxCarBodyShared;

/// One shared body's parts: section, mesh, material, mesh-space box, cockpit-replaced flag; plus the root offset.
struct CachedBody {
    offset: Vec3,
    parts: Vec<(String, Handle<Mesh>, Handle<FxCarMaterial>, bevy::camera::primitives::Aabb, bool)>,
}

/// [`FxCarBodyShared`] builds by (assets, car, cube track, paint, live cube).
#[derive(Resource, Default)]
struct FxBodyCache(HashMap<(PathBuf, String, String, Option<(u32, bool, u32)>, bool, Option<Vec<(String, char)>>), std::sync::Arc<CachedBody>>);

/// A non-stock paint for an [`FxCarBody`]: the car's Combo_Colors row with this Sequence (`physics.json`
/// `colors`, cars group cars-17). Without it the body has the stock paint (lowest Sequence). FH1_PAINT still wins.
#[derive(Component, Clone, Copy, Debug)]
pub struct FxCarPaint {
    pub sequence: u32,
}

/// A custom paint colour for an [`FxCarBody`] (the Customize menu): `rgb` 0xRRGGBB gamma-encoded like Combo_Colors.RGB.
/// The finish file (ColorShaderSettings<seq>) still comes from the [`FxCarPaint`] row, else the stock one. FH1_PAINT wins.
/// Read when the body is added (respawn the body to change it).
#[derive(Component, Clone, Copy, Debug)]
pub struct FxCarPaintRgb {
    pub rgb: u32,
    pub metallic: bool,
}

/// An aftermarket rim for an [`FxCarBody`] (the Customize menu): a `cars/wheels/<MediaName>` folder used instead of
/// `model.json` `rim`. Sizes stay the car's (wheelScale fits any rim to the axle). Read when the body is added.
#[derive(Component, Clone, Debug)]
pub struct FxCarRim(pub String);

/// A body kit for an [`FxCarBody`] (the Customize menu): the letter per kit stem, used instead of `model.json` `kit`.
/// Read when the body is added.
#[derive(Component, Clone, Debug)]
pub struct FxCarKit(pub StockKit);

/// Section names an [`FxCarBody`] draws itself; glTF nodes with these names are hidden.
#[derive(Component)]
pub struct FxCarDrawn(pub HashSet<String>);

/// Cockpit view: insert on the [`FxCarBody`] entity while the camera is inside the car. The cockpit model
/// (`<CAR>_cockpit.fxcar`) is drawn and the exterior model's cabin parts are hidden. Removing it hides the
/// cockpit again; the cockpit stays loaded for the next time.
#[derive(Component)]
pub struct FxCockpitView;

/// Driver eye point in the [`FxCarBody`] entity's local space (+Y up, front -Z), set when the cockpit loads.
/// = steering wheel centre (cockpit model's `steering_wheel` box, + gamedb CockpitWheelPositionOffset) +
/// gamedb CameraOverrides CamCockpitOffset, or camera.zip CameraSettings.ini `Driver\DefaultOffsetFromSteeringWheel`
/// (0, 0.21, -0.51) when the car has none (all zero). Z is negated from game space. The combine rule is INFERRED
/// (only the gamedb loader 0x824C2B08 is traced; docs/CAR_INTERIOR.md "Cockpit camera").
#[derive(Component, Clone, Copy, Debug)]
pub struct FxCockpitEye(pub Vec3);

/// The cockpit model's root (child of the [`FxCarBody`] entity).
#[derive(Component)]
struct FxCockpitRoot;

/// An exterior-model part the cockpit model replaces (interior, seats, steering wheel).
#[derive(Component)]
struct FxExteriorCabinPart;

fn is_cockpit_replaced(section: &str) -> bool {
    let s = section.to_ascii_lowercase();
    s == "interior" || s.starts_with("seat") || s == "steering_wheel"
}

/// camera.zip CameraSettings.ini `Driver\DefaultOffsetFromSteeringWheel{X,Y,Z}` (game space, +Z front).
const DRIVER_DEFAULT_OFFSET: Vec3 = Vec3::new(0.0, 0.21, -0.51);

/// The driver eye from the cockpit model and `physics.json` `camera` (gamedb CameraOverrides row), in the
/// cockpit's mesh space (before `mesh_offset`).
fn cockpit_eye(cockpit: &[FxCarPart], camera: &serde_json::Value) -> Option<Vec3> {
    let wheel = cockpit.iter().filter(|p| p.section.eq_ignore_ascii_case("steering_wheel")).fold(None, |acc: Option<(Vec3, Vec3)>, p| {
        Some(acc.map_or((p.min, p.max), |(a, b)| (a.min(p.min), b.max(p.max))))
    })?;
    let f = |k: &str| camera[k].as_f64().unwrap_or(0.0) as f32;
    let game = |p: &str| Vec3::new(f(&format!("{p}X")), f(&format!("{p}Y")), f(&format!("{p}Z")));
    let mut offset = game("CamCockpitOffset");
    if offset == Vec3::ZERO {
        offset = DRIVER_DEFAULT_OFFSET;
    }
    let wheel_offset = game("CockpitWheelPositionOffset");
    let to_mesh = |v: Vec3| Vec3::new(v.x, v.y, -v.z);
    Some((wheel.0 + wheel.1) * 0.5 + to_mesh(wheel_offset) + to_mesh(offset))
}

#[allow(clippy::too_many_arguments)]
fn update_fx_cockpit(
    mut commands: Commands,
    added: Query<(Entity, &FxCarBody, Option<&FxCarPaint>, Option<&FxCarPaintRgb>), Added<FxCockpitView>>,
    mut removed: RemovedComponents<FxCockpitView>,
    viewing: Query<(), With<FxCockpitView>>,
    roots: Query<(Entity, &ChildOf), With<FxCockpitRoot>>,
    children: Query<&Children>,
    mut vis: Query<&mut Visibility>,
    cabin: Query<(), With<FxExteriorCabinPart>>,
    mut lib: ResMut<FxLibrary>,
    mut globals: ResMut<FxCarGlobals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FxCarMaterial>>,
    env: Option<Res<crate::reflect::EnvCube>>,
) {
    let set = |e: Entity, inside: bool, vis: &mut Query<&mut Visibility>| {
        for d in children.iter_descendants(e) {
            if cabin.contains(d) {
                if let Ok(mut v) = vis.get_mut(d) {
                    v.set_if_neq(if inside { Visibility::Hidden } else { Visibility::Inherited });
                }
            }
        }
        for (r, parent) in &roots {
            if parent.parent() == e {
                if let Ok(mut v) = vis.get_mut(r) {
                    v.set_if_neq(if inside { Visibility::Inherited } else { Visibility::Hidden });
                }
            }
        }
    };
    for e in removed.read() {
        if !viewing.contains(e) {
            set(e, false, &mut vis);
        }
    }
    for (e, body, choice, custom) in &added {
        set(e, true, &mut vis);
        if roots.iter().any(|(_, p)| p.parent() == e) {
            continue;
        }
        let car_dir = body.assets.join("cars").join(&body.car);
        let read_json = |f: &str| -> serde_json::Value { crate::files::read_json(&car_dir.join(f)).unwrap_or_default() };
        let model = read_json("model.json");
        let offset = model["mesh_offset"].as_array().map_or(Vec3::ZERO, |a| Vec3::from_array(std::array::from_fn(|i| a.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32)));
        let track = if crate::files::exists(&body.assets.join("cars/cubemaps").join(format!("{}.dds", body.track))) { body.track.as_str() } else { "colorado" };
        let env_dynamic = env.as_ref().filter(|c| c.use_dynamic).map(|c| c.dynamic.clone());
        let Some(parts) = load_cockpit_parts_env(&body.assets, &body.car, track, body_paint(&car_dir, &model, choice, custom), env_dynamic, &mut lib, &mut globals, &mut shaders, &mut images) else {
            warn!("{}: no cockpit model (fx/{}_cockpit.fxcar)", body.car, body.car);
            continue;
        };
        if let Some(eye) = cockpit_eye(&parts, &read_json("physics.json")["camera"]) {
            info!("{}: cockpit eye {:?} (mesh space)", body.car, eye);
            commands.entity(e).insert(FxCockpitEye(offset + eye));
        }
        let root = commands.spawn((FxCockpitRoot, Transform::from_translation(offset), Visibility::Inherited, ChildOf(e))).id();
        for (order, mut p) in parts.into_iter().enumerate() {
            p.material.order = order as u32;
            let aabb = bevy::camera::primitives::Aabb::from_min_max(p.min, p.max);
            commands.spawn((Mesh3d(meshes.add(p.mesh)), MeshMaterial3d(materials.add(p.material)), aabb, Transform::default(), ChildOf(root)));
        }
    }
}

pub struct FxCarPlugin;

impl Plugin for FxCarPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FxBodyCache>();
        app.add_systems(Startup, lamp_sort_setup).add_systems(First, lamp_sort_ab);
        app.add_systems(Update, (spawn_fx_car_bodies, hide_replaced_gltf_nodes, update_fx_cockpit).chain()).add_systems(
            PostUpdate,
            update_car_lamps.after(crate::lighting::update_car_lighting).before(crate::upload_globals),
        );
        if std::env::var("FH1_CARFX_AB").is_ok_and(|v| v != "0") {
            app.add_systems(Update, carfx_ab);
        }
        if std::env::var_os("FH1_CARFX_SWITCH_AT").is_some() {
            app.add_systems(PreUpdate, debug_switch_key.after(bevy::input::InputSystems));
        }
        // Car globals vs the game's per-draw constants (Xenia 4543 + Pinyon 6631/08:00): FH1_CAR_LIGHT_GAME=0 = off.
        if !std::env::var("FH1_CAR_LIGHT_GAME").is_ok_and(|v| v == "0") {
            app.add_systems(
                PostUpdate,
                car_light_parity.after(crate::lighting::update_car_lighting).before(update_car_lamps).before(crate::upload_globals),
            );
        }
        if std::env::var_os("FH1_CARFX_DUMP").is_some() {
            app.add_systems(PostUpdate, dump_car_globals.after(crate::upload_globals));
        }
    }
}

/// Corrections of the car globals `lighting::apply_car_lighting` writes, from the game's own per-draw constants
/// (docs/SHADERS.md "Car lighting vs the game"):
/// - shDirectionalColour(VS) = sunColor x TimeOfDayA IBLDirectScaleCar: the game's car value is the scenery sunColor x
///   0.62664 at 15:48 = IBLDirectScaleCar(948 min) exactly (VERIFIED, Xenia 4543).
/// - shCubeMapDampnerLight: DC = 3.0575 at 08:00, 15:48 and 16:00 (two places) = 1.5 x the SHEnvMapTop/Bottom hemisphere
///   DC; the linear terms already match (VERIFIED).
/// - shHemisphericalLightR/G/B: the game's filler is untraced and its values depend on the place (horizontal terms, ~20%
///   between the festival and Redstone at the same TOD). FITTED over 3 captures x 3 channels: DC x 0.39, linear x 0.65
///   (ours was 2-3x too bright). FH1_CAR_SH_FIT=dc,l1 overrides.
/// - psFogColor = the scenery FogColor rgb, w 0 (VERIFIED; the track bank has no psFogColor, so ours stayed 0.1).
/// - reflectionScaler = (1, 1, 1, 1) (VERIFIED value; its setter is still untraced).
fn car_light_parity(tod: Option<Res<crate::lighting::FxTimeOfDay>>, track: Res<crate::FxGlobals>, mut car: ResMut<FxCarGlobals>) {
    let g = &mut car.0;
    if let Some(t) = tod.as_ref() {
        let k = t.tod.scalar_or("IBLDirectScaleCar", t.minutes(), 1.0);
        for n in ["shDirectionalColour", "shDirectionalColourVS"] {
            if let Some(v) = g.get(n) {
                let rgb = v.truncate() * k;
                g.set_vec(n, rgb.extend(rgb.dot(Vec3::new(0.2126, 0.7152, 0.0722))));
            }
        }
    }
    static FIT: std::sync::OnceLock<(f32, f32)> = std::sync::OnceLock::new();
    let (dc, l1) = *FIT.get_or_init(|| {
        let v: Vec<f32> = std::env::var("FH1_CAR_SH_FIT").ok().map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default();
        (v.first().copied().unwrap_or(0.39), v.get(1).copied().unwrap_or(0.65))
    });
    for n in ["shHemisphericalLightR", "shHemisphericalLightG", "shHemisphericalLightB"] {
        if let Some(v) = g.get(n) {
            g.set_vec(n, Vec4::new(v.x * dc, v.y * l1, v.z * l1, v.w * l1));
        }
    }
    if let Some(v) = g.get("shCubeMapDampnerLight") {
        g.set_vec("shCubeMapDampnerLight", Vec4::new(v.x * 1.5, v.y, v.z, v.w));
    }
    if let Some(f) = track.get("FogColor") {
        g.set_vec("psFogColor", f.truncate().extend(0.0));
    }
    g.set_vec("reflectionScaler", Vec4::splat(std::env::var("FH1_CAR_REFL").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0)));
}

/// FH1_CARFX_DUMP=<file>: append the car global register files once, at FH1_CARFX_DUMP_AT seconds (default 10).
fn dump_car_globals(globals: Res<FxCarGlobals>, time: Res<Time<Real>>, mut done: Local<bool>) {
    let at: f32 = std::env::var("FH1_CARFX_DUMP_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0);
    if *done || time.elapsed_secs() < at {
        return;
    }
    *done = true;
    let line = serde_json::json!({"globals": {"vs": globals.0.vs, "ps": globals.0.ps, "bools": globals.0.bools}});
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::var("FH1_CARFX_DUMP").unwrap_or_default()) {
        let _ = writeln!(f, "{line}");
    }
}

/// The car's lamp switches besides the headlights, set by the game code (put it next to
/// [`crate::headlight::FxHeadlightSource`]). The per-car light update 0x824A3558 hands the car's light object 0/1 per
/// lamp (VERIFIED): blinkers (+0xF0, left/right, flashing), +0xEC, brake (+0xE8), headlights (+0xF8 =
/// SetHeadLightAmount) and +0xF4 (fog lamps INFERRED).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct FxCarLamps {
    pub brake: f32,
    pub reverse: f32,
    pub indicator_left: f32,
    pub indicator_right: f32,
    pub fog: f32,
}

/// Lamp emissive and the body's headlight terms, in the car global bank:
/// - dynamicLightsAmount c58 = (brake, reverse, indicator L, indicator R) and dynamicLights2 c108 = (headlights, 0, 0,
///   fog). INFERRED from the readers in shaders_v16: tail_light/taillight2S .x of c58, reverse_light .y,
///   indicator_left .z; xenonhead/numplate/taillight2S running light c108.x, fogred c108.w. The upload is untraced.
/// - dynamicLights4 c110 / dynamicLights5 c111 = HeadLightParams1 / (HeadLightParams2.x) (INFERRED, docs/SHADERS.md "Car
///   headlights"); b142 is set by headlight.rs.
///
/// One bank for every car: the player's lamps win (AI cars share it until per-car banks exist).
fn update_car_lamps(
    frame: Option<Res<crate::headlight::FxHeadlightFrame>>,
    cars: Query<(&crate::headlight::FxHeadlightSource, &crate::headlight::FxHeadlightState, Option<&FxCarLamps>)>,
    mut globals: ResMut<FxCarGlobals>,
) {
    let (mut heads, mut lamps) = (0.0, FxCarLamps::default());
    if let Some((_, s, l)) = cars.iter().max_by_key(|(src, ..)| src.player) {
        heads = s.amount;
        lamps = l.copied().unwrap_or_default();
    }
    let amount = Vec4::new(lamps.brake, lamps.reverse, lamps.indicator_left, lamps.indicator_right);
    let lights2 = Vec4::new(heads, 0.0, 0.0, lamps.fog);
    if globals.get("dynamicLightsAmount") != Some(amount) {
        globals.set_vec("dynamicLightsAmount", amount);
    }
    if globals.get("dynamicLights2") != Some(lights2) {
        globals.set_vec("dynamicLights2", lights2);
    }
    if let Some(f) = frame {
        // The game's car bank holds dynamicLights4/5 = 0 while no headlight is on (K2 capture diff: c111 0 in game vs
        // 0.5 here in daytime; c110 already 0). They only matter under b142 psDeferredHeadlightEnable = count > 0, so
        // zeroing them when count = 0 changes no pixel, only the bank. FH1_CARFX_DL_GATE=0 = the old always-set values.
        let on = f.count > 0 || std::env::var("FH1_CARFX_DL_GATE").is_ok_and(|v| v == "0");
        let (l4, l5) = if on { (f.params1, Vec4::new(f.params2.x, 0.0, 0.0, 0.0)) } else { (Vec4::ZERO, Vec4::ZERO) };
        if globals.get("dynamicLights4") != Some(l4) || globals.get("dynamicLights5") != Some(l5) {
            globals.set_vec("dynamicLights4", l4);
            globals.set_vec("dynamicLights5", l5);
        }
    }
}

/// FH1_CARFX_AB=1: hide the game-shaded car (body root + every FX car part) every other 5 s and log each mode's mean
/// frame time (first 10 s dropped), to measure the car's draw cost under one machine load.
fn carfx_ab(
    time: Res<Time<Real>>,
    mut acc: Local<([(f64, u32); 2], f32)>,
    mut parts: Query<&mut Visibility, With<MeshMaterial3d<FxCarMaterial>>>,
) {
    let t = time.elapsed_secs();
    let on = (t / 5.0) as u32 % 2 == 0;
    for mut v in &mut parts {
        v.set_if_neq(if on { Visibility::Inherited } else { Visibility::Hidden });
    }
    if t > 10.0 && t % 5.0 > 0.25 {
        let a = &mut acc.0[on as usize];
        a.0 += time.delta_secs_f64() * 1000.0;
        a.1 += 1;
    }
    if t - acc.1 >= 20.0 {
        acc.1 = t;
        let m = |a: (f64, u32)| if a.1 > 0 { a.0 / a.1 as f64 } else { 0.0 };
        info!("car fx A/B: hidden {:.2} ms ({} frames), drawn {:.2} ms ({} frames)", m(acc.0[0]), acc.0[0].1, m(acc.0[1]), acc.0[1].1);
    }
}

/// FH1_CARFX_SWITCH_AT=s: press N (the engine's next-car key) once after s seconds, to test a car switch
/// in automated runs.
fn debug_switch_key(mut keys: ResMut<ButtonInput<KeyCode>>, time: Res<Time<Real>>, mut state: Local<u8>) {
    let at: f32 = std::env::var("FH1_CARFX_SWITCH_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(f32::MAX);
    match *state {
        0 if time.elapsed_secs() > at => {
            keys.press(KeyCode::KeyN);
            *state = 1;
        }
        1 => {
            keys.release(KeyCode::KeyN);
            *state = 2;
        }
        _ => {}
    }
}

/// Xenos piecewise-linear gamma -> linear for an 8-bit value (program.rs `fx_pwl_degamma`, Xenia xenos.cc).
fn pwl_degamma(g: u32) -> f32 {
    let g = g.min(255) as f32;
    let (scale, offset) = match g as u32 {
        0..64 => (1.0, 0.0),
        64..96 => (2.0, -64.0),
        96..192 => (4.0, -256.0),
        _ => (8.0, -1024.0),
    };
    let l: f32 = g * scale + offset;
    (l + (l * scale / 1024.0).trunc()) / 1023.0
}

/// The body paint (RGB, Metallic, Sequence): FH1_PAINT, else the [`FxCarPaint`] row from `physics.json` `colors`,
/// else the stock paint.
fn body_paint(car_dir: &std::path::Path, model: &serde_json::Value, choice: Option<&FxCarPaint>, custom: Option<&FxCarPaintRgb>) -> Option<(u32, bool, u32)> {
    if let Some(c) = custom.filter(|_| std::env::var_os("FH1_PAINT").is_none()) {
        let seq = body_paint(car_dir, model, choice, None).map_or(0, |p| p.2);
        return Some((c.rgb & 0xFF_FFFF, c.metallic, seq));
    }
    if std::env::var_os("FH1_PAINT").is_none() {
        // FH1_PAINT_SEQ=<n>: that Sequence for any car (debugging aid).
        let env_seq = std::env::var("FH1_PAINT_SEQ").ok().and_then(|v| v.parse().ok());
        if let Some(seq) = env_seq.or(choice.map(|c| c.sequence)) {
            let physics: serde_json::Value = crate::files::read_json(&car_dir.join("physics.json")).unwrap_or_default();
            let row = physics["colors"].as_array().and_then(|rows| rows.iter().find(|r| r["Sequence"].as_u64() == Some(seq as u64)));
            match row {
                Some(r) => return Some((r["RGB"].as_u64()? as u32, r["Metallic"].as_u64().unwrap_or(0) != 0, seq)),
                None => warn!("{}: no Combo_Colors sequence {seq} in physics.json `colors` (re-run fh1setup --only cars); stock paint", car_dir.display()),
            }
        }
    }
    model_paint(model)
}

/// Stock paint from `model.json` `paint` (gamedb Combo_Colors RGB, Metallic, Sequence).
fn model_paint(model: &serde_json::Value) -> Option<(u32, bool, u32)> {
    // FH1_PAINT=rrggbb[,metallic 0/1[,sequence]]: another Combo_Colors row (debugging aid; the stock paint is seq 1).
    if let Ok(v) = std::env::var("FH1_PAINT") {
        let mut it = v.split(',');
        if let Some(rgb) = it.next().and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()) {
            let metallic = it.next().is_some_and(|s| s == "1");
            return Some((rgb, metallic, it.next().and_then(|s| s.parse().ok()).unwrap_or(1)));
        }
    }
    let p = &model["paint"];
    Some((p["rgb"].as_u64()? as u32, p["metallic"].as_bool().unwrap_or(true), p["sequence"].as_u64().unwrap_or(0) as u32))
}

#[allow(clippy::too_many_arguments)]
fn spawn_fx_car_bodies(
    mut commands: Commands,
    added: Query<(Entity, &FxCarBody, Option<&FxCarPaint>, Option<&FxCarPaintRgb>, Option<&FxCarKit>, Has<FxCarBodyShared>), Added<FxCarBody>>,
    mut cache: ResMut<FxBodyCache>,
    mut lib: ResMut<FxLibrary>,
    mut globals: ResMut<FxCarGlobals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FxCarMaterial>>,
    env: Option<Res<crate::reflect::EnvCube>>,
) {
    // Default on; FH1_CARFX=0 keeps the glTF body (wheels, rotors and calipers are still glTF either way).
    if std::env::var("FH1_CARFX").is_ok_and(|v| v == "0") || std::env::var("FH1_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("remaster")) {
        return;
    }
    // Body PS (v16 #258): useStaticCubeMap false -> envSampler (live cube), true -> envStaticSampler with
    // the height ramp; psUseBackgroundMap (2D envBackgroundSampler) is off in race (docs/SHADERS.md "Reflections").
    let env_dynamic = env.as_ref().filter(|c| c.use_dynamic).map(|c| c.dynamic.clone());
    if !added.is_empty() {
        globals.0.set_bool("useStaticCubeMap", env_dynamic.is_none());
        globals.0.set_bool("psUseBackgroundMap", false);
        // Mirrors (mirrorLeft/Right, cockpit_mirrorMiddle): the game's mirrorUsesCubeMap path reads the env cube, so
        // no rear-view render is needed (mirrorSamplerDefault was unbound = white). Only the mirror programs use
        // this bool (PS b4). The 2D rear view (reflect.rs MirrorView, a full extra scene render) is not bound yet.
        globals.0.set_bool("mirrorUsesCubeMap", true);
        // reflectionScaler (PS c52, a global in 113 v16 shaders): the body PS blends the lit colour toward the env
        // fetch by saturate(reflectionScaler x ... x FresnelScalar x fresnel) (PS 258 instr 81/100/144-145, VERIFIED);
        // 0 = no reflection at all. Its setter is untraced: 1.0 (INFERRED, FH1_CAR_REFL overrides).
        let refl = std::env::var("FH1_CAR_REFL").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0f32);
        globals.0.set_vec("reflectionScaler", Vec4::new(refl, 0.0, 0.0, 0.0));
    }
    for (e, body, choice, custom, kit, shared) in &added {
        // Tracks without a static car cube of their own (the test plane, imported maps) use Colorado's.
        let cubes = body.assets.join("cars/cubemaps");
        let track = if crate::files::exists(&cubes.join(format!("{}.dds", body.track))) { body.track.as_str() } else { "colorado" };
        let model: serde_json::Value = crate::files::read_json(&body.assets.join("cars").join(&body.car).join("model.json")).unwrap_or_default();
        let offset = model["mesh_offset"].as_array().map_or(Vec3::ZERO, |a| Vec3::from_array(std::array::from_fn(|i| a.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32)));
        let paint = body_paint(&body.assets.join("cars").join(&body.car), &model, choice, custom);
        let kit = kit.map(|k| &k.0);
        let key = (body.assets.clone(), body.car.clone(), track.to_owned(), paint, env_dynamic.is_some(), kit.map(|k| k.0.clone()));
        if let Some(c) = cache.0.get(&key).filter(|_| shared).cloned() {
            let mut drawn = HashSet::new();
            let root = commands.spawn((Transform::from_translation(c.offset), Visibility::default(), ChildOf(e))).id();
            for (section, mesh, material, aabb, cockpit) in &c.parts {
                drawn.insert(section.clone());
                let mut part = commands.spawn((Mesh3d(mesh.clone()), MeshMaterial3d(material.clone()), *aabb, Transform::default(), ChildOf(root)));
                if *cockpit {
                    part.insert(FxExteriorCabinPart);
                }
            }
            commands.entity(e).insert((FxCarDrawn(drawn), crate::lighting::FxCarLit));
            continue;
        }
        let Some(parts) = load_body_parts_kit(&body.assets, &body.car, track, paint, env_dynamic.clone(), kit, &mut lib, &mut globals, &mut shaders, &mut images) else {
            warn!("{}: no car shader data (fx/*.fxcar); keeping the glTF body", body.car);
            continue;
        };
        let mut drawn = HashSet::new();
        let root = commands.spawn((Transform::from_translation(offset), Visibility::default(), ChildOf(e))).id();
        let mut cached = shared.then(|| CachedBody { offset, parts: Vec::new() });
        for (order, mut p) in parts.into_iter().enumerate() {
            p.material.order = order as u32;
            drawn.insert(p.section.clone());
            let aabb = bevy::camera::primitives::Aabb::from_min_max(p.min, p.max);
            let cockpit = is_cockpit_replaced(&p.section);
            if shared {
                // Later bodies' systems (car_shadow casters, ...) read the mesh data: keep the main-world copy.
                p.mesh.asset_usage = bevy::asset::RenderAssetUsages::all();
            }
            let (mesh, material) = (meshes.add(p.mesh), materials.add(p.material));
            if let Some(c) = cached.as_mut() {
                c.parts.push((p.section.clone(), mesh.clone(), material.clone(), aabb, cockpit));
            }
            let mut part = commands.spawn((Mesh3d(mesh), MeshMaterial3d(material), aabb, Transform::default(), ChildOf(root)));
            if cockpit {
                part.insert(FxExteriorCabinPart);
            }
        }
        if let Some(c) = cached {
            cache.0.insert(key, std::sync::Arc::new(c));
        }
        commands.entity(e).insert((FxCarDrawn(drawn), crate::lighting::FxCarLit));
        if std::env::var_os("FH1_CARFX_LOG").is_some() {
            // Body PS v16 #258 globals: c42 shDirectionalColour, c43 psLightDirWS, c47 psFogColor, c52 reflectionScaler,
            // c57 alphaDirtAmount, c59 eyePos, c110/111 dynamicLights4/5, c113 psLightMapColour, c125 LightmapPower, c126 HeadlightPower.
            let vals: Vec<String> = [42, 43, 47, 52, 57, 59, 110, 111, 113, 124, 125, 126, 159, 168, 169].iter().map(|&r| format!("c{r}={:?}", globals.0.ps[r])).collect();
            let b: Vec<u32> = (128..144).map(|i| globals.0.bools[i]).collect();
            info!("carfx globals: {} | ps bools 0-15 {b:?}", vals.join(" "));
        }
    }
}

/// Hide the glTF section nodes an [`FxCarBody`] draws (runs until the scene has spawned them).
/// P8 (2026-10-08): change-driven instead of walking every car's whole hierarchy every frame (0.14 ms with traffic):
/// a car's descendants are walked when its [`FxCarDrawn`] is added / changed, newly named entities (scene nodes spawning
/// later) are checked against their car ancestor, and every car is re-walked once a second as a safety net.
/// `FH1_HIDE_NODES_SCAN=1` = the full walk every frame (old).
#[allow(clippy::too_many_arguments)]
fn hide_replaced_gltf_nodes(
    cars: Query<(Entity, &FxCarDrawn)>,
    changed: Query<(Entity, &FxCarDrawn), Changed<FxCarDrawn>>,
    new_named: Query<Entity, Added<Name>>,
    drawn_q: Query<&FxCarDrawn>,
    parents: Query<&ChildOf>,
    children: Query<&Children>,
    mut named: Query<(&Name, &mut Visibility)>,
    time: Res<Time<Real>>,
    mut last_scan: Local<f32>,
) {
    static SCAN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let scan_all = *SCAN.get_or_init(|| std::env::var("FH1_HIDE_NODES_SCAN").as_deref() == Ok("1"));
    fn hide(d: Entity, drawn: &FxCarDrawn, named: &mut Query<(&Name, &mut Visibility)>) {
        if let Ok((name, mut vis)) = named.get_mut(d) {
            if drawn.0.contains(name.as_str()) && *vis != Visibility::Hidden {
                *vis = Visibility::Hidden;
            }
        }
    }
    let now = time.elapsed_secs();
    if scan_all || now - *last_scan >= 1.0 {
        *last_scan = now;
        for (e, drawn) in &cars {
            for d in children.iter_descendants(e) {
                hide(d, drawn, &mut named);
            }
        }
        return;
    }
    for (e, drawn) in &changed {
        for d in children.iter_descendants(e) {
            hide(d, drawn, &mut named);
        }
    }
    for d in &new_named {
        if let Some(drawn) = parents.iter_ancestors(d).find_map(|a| drawn_q.get(a).ok()) {
            hide(d, drawn, &mut named);
        }
    }
}

/// One game-shaded body part: carbin section name, mesh, material and its mesh-space box.
pub struct FxCarPart {
    pub section: String,
    pub mesh: Mesh,
    pub material: FxCarMaterial,
    pub min: Vec3,
    pub max: Vec3,
}

/// [`load_body`] with each part's section name and bounding box.
#[allow(clippy::too_many_arguments)]
pub fn load_body_parts(
    assets: &std::path::Path,
    car: &str,
    track: &str,
    paint: Option<(u32, bool, u32)>,
    lib: &mut FxLibrary,
    globals: &mut FxCarGlobals,
    shaders: &mut Assets<Shader>,
    images: &mut Assets<Image>,
) -> Option<Vec<FxCarPart>> {
    load_body_parts_env(assets, car, track, paint, None, lib, globals, shaders, images)
}

/// [`load_body_parts`] with the live env cube for `envSampler` (`reflect::EnvCube::dynamic`).
#[allow(clippy::too_many_arguments)]
pub fn load_body_parts_env(
    assets: &std::path::Path,
    car: &str,
    track: &str,
    paint: Option<(u32, bool, u32)>,
    env_dynamic: Option<Handle<Image>>,
    lib: &mut FxLibrary,
    globals: &mut FxCarGlobals,
    shaders: &mut Assets<Shader>,
    images: &mut Assets<Image>,
) -> Option<Vec<FxCarPart>> {
    load_body_parts_kit(assets, car, track, paint, env_dynamic, None, lib, globals, shaders, images)
}

/// [`load_body_parts_env`] with a body kit other than the stock one ([`FxCarKit`]).
#[allow(clippy::too_many_arguments)]
pub fn load_body_parts_kit(
    assets: &std::path::Path,
    car: &str,
    track: &str,
    paint: Option<(u32, bool, u32)>,
    env_dynamic: Option<Handle<Image>>,
    kit: Option<&StockKit>,
    lib: &mut FxLibrary,
    globals: &mut FxCarGlobals,
    shaders: &mut Assets<Shader>,
    images: &mut Assets<Image>,
) -> Option<Vec<FxCarPart>> {
    load_parts(assets, car, track, paint, env_dynamic, kit, CarMeshKind::default(), lib, globals, shaders, images)
}

/// The cockpit model (`<car>_cockpit.fxcar`, LOD0 only) with the `cockpit_` techniques on interior sections.
#[allow(clippy::too_many_arguments)]
pub fn load_cockpit_parts_env(
    assets: &std::path::Path,
    car: &str,
    track: &str,
    paint: Option<(u32, bool, u32)>,
    env_dynamic: Option<Handle<Image>>,
    lib: &mut FxLibrary,
    globals: &mut FxCarGlobals,
    shaders: &mut Assets<Shader>,
    images: &mut Assets<Image>,
) -> Option<Vec<FxCarPart>> {
    load_parts(assets, car, track, paint, env_dynamic, None, CarMeshKind { cockpit: true, ..default() }, lib, globals, shaders, images)
}

#[allow(clippy::too_many_arguments)]
fn load_parts(
    assets: &std::path::Path,
    car: &str,
    track: &str,
    paint: Option<(u32, bool, u32)>,
    env_dynamic: Option<Handle<Image>>,
    kit: Option<&StockKit>,
    kind: CarMeshKind,
    lib: &mut FxLibrary,
    globals: &mut FxCarGlobals,
    shaders: &mut Assets<Shader>,
    images: &mut Assets<Image>,
) -> Option<Vec<FxCarPart>> {
    let dir = assets.join("cars").join(car).join("fx");
    let shared = assets.join("cars/shared/ShaderSettings");
    let read = |p: PathBuf| crate::files::read_to_string(&p);
    // The game keeps three chains indexed by the paint's finish (0x82548E78, paint+0x24, VERIFIED): 0 = Normal.xml,
    // 1 = Normal;Metallic, 2 = Normal;Matte. Combo_Colors.Metallic only gives 0/1, so a non-metallic stock colour is
    // the glossy Normal finish. Matte.xml (FresnelScalar 0, specular power 1) drew every non-metallic car flat brown.
    let metallic = paint.is_none_or(|p| p.1);
    let finish = if metallic { read(shared.join("Metallic.xml")) } else { None };
    let colour = paint.and_then(|p| read(dir.join(format!("ColorShaderSettings{}.xml", p.2))));
    let settings = ShaderSettings::chain(&[read(shared.join("Normal.xml")).as_deref(), finish.as_deref(), read(dir.join("ShaderSettings.xml")).as_deref(), colour.as_deref()]);
    let lib_bytes = crate::files::read(&assets.join("shaders/media/Cars/shaders_v16.fxobj"))?;
    let mut shading = CarShading::new("shaders_v16", &lib_bytes, settings, lib)?;
    shading.kind = kind;
    // Combo_Colors.RGB is gamma-encoded and the game uploads it sRGB-degammed: the Corrado's DC001F is PaintColor
    // (0.7157, 0, 0.0137) in the Xenia capture 4543 = sRGB exactly (VERIFIED; the Xenos PWL curve gave 0.7243 / 0.0303,
    // the same rule as the ShaderSettings colours). FH1_CARFX_COLOR_SRGB=0 = the old PWL decode.
    let degamma = |g: u32| if std::env::var("FH1_CARFX_COLOR_SRGB").is_ok_and(|v| v == "0") { pwl_degamma(g) } else { srgb_degamma(g as f32 / 255.0) };
    shading.paint = paint.map(|(v, _, _)| Vec3::new(degamma(v >> 16 & 255), degamma(v >> 8 & 255), degamma(v & 255)));
    let read_fx = |f: String| crate::files::read(&dir.join(f)).and_then(|b| parse_fxcar(&b));
    // Imported cars are addressed by path (`../imported/fm4/cars/<media>`); the streams are named after the media name.
    let stem = std::path::Path::new(car).file_name().and_then(|s| s.to_str()).unwrap_or(car);
    let (lod0, main) = if kind.cockpit {
        (None, read_fx(format!("{stem}_cockpit.fxcar"))?)
    } else {
        (read_fx(format!("{stem}_lod0.fxcar")), read_fx(format!("{stem}.fxcar"))?)
    };
    shading.bounds = car_bounds(&lod0.as_ref().unwrap_or(&main).sections);
    let mut textures = CarTextures::new(assets, car, track);
    textures.env_dynamic = env_dynamic;
    let mut out = Vec::new();
    let kit = kit.cloned().unwrap_or_else(|| StockKit::load(assets, car));
    for (s, sub) in body_parts(&main, lod0.as_ref(), &kit) {
        let fx = if sub.lod == 0 { lod0.as_ref().unwrap_or(&main) } else { &main };
        shading.sh_compression = fx.sh_compression.unwrap_or(SH_COMPRESSION_DEFAULT);
        let (Some(mesh), Some(mut material)) = (subsection_mesh(s, sub), shading.material_textured(s, sub, lib, globals, shaders, &mut textures, images)) else { continue };
        let o = Vec3::from_array(s.offset);
        let (min, max) = (o + Vec3::from_array(s.bounds_min), o + Vec3::from_array(s.bounds_max));
        // Lamp lenses: pass 1 (GlassColor tint) is drawn over pass 0 (the lights atlas) as a second, blended part.
        // In shaders_v16 only detail_glass_{red,clear,amber} have a pass 1 (PS 288/290/292); lights_glass has one pass.
        let tech = shading.technique(&s.name, &sub.name);
        let lens = tech.as_deref().is_some_and(|t| t.starts_with("detail_glass"));
        if material.alpha_blend {
            material.sort_bias = tech.as_deref().map_or(0.0, lamp_sort_bias);
        }
        let overlay = if lens && std::env::var("FH1_CARFX_LENS1").map_or(true, |v| v != "0") {
            shading.material_textured_pass(s, sub, 1, lib, globals, shaders, &mut textures, images).map(|mut m| {
                m.sort_bias = material.sort_bias;
                // Pass 1 = the lens's env reflection x Schlick fresnel (Direct..Glancing), fogged. The game sets the
                // blend in code (untraced): additive (INFERRED: it adds a reflection layer over pass 0's lamp).
                m.alpha_blend = false;
                m.additive = true;
                (mesh.clone(), m)
            })
        } else {
            None
        };
        out.push(FxCarPart { section: s.name.clone(), mesh, material, min, max });
        if let Some((mesh, material)) = overlay {
            out.push(FxCarPart { section: s.name.clone(), mesh, material, min, max });
        }
    }
    Some(out)
}

/// Lamp cover techniques (the coloured/clear plastic lens over a lamp).
pub fn lens_cover(tech: &str) -> bool {
    let t = tech.to_ascii_lowercase();
    t.starts_with("lights_gls") || t.starts_with("detail_glass")
}

/// Transparent sort bias (m, later = on top; FxCarMaterial::depth_bias) of a blended car part by technique, so a lamp
/// draws back to front: reflector bowl, then the lit lamp layers (bulb / lights atlas), then the lens plastics over them.
/// The steps beat the centimetre centre differences inside a lamp and are too small to reorder against scenery.
/// Windows and other glass keep 0.
fn lamp_sort_bias(tech: &str) -> f32 {
    let t = tech.to_ascii_lowercase();
    if t.starts_with("lights_gls") || t.starts_with("detail_glass") {
        0.3
    } else if t.contains("reflector") {
        0.1
    } else if ["light", "head", "beam", "indicator", "fog", "xenon", "numplate"].iter().any(|w| t.contains(w)) {
        0.2
    } else {
        0.0
    }
}

/// `FH1_CARFX_LENS_SORT=1`: lamp sort order on (opt-in, no measured effect); `=ab`: flips it every [`LAMP_AB_FRAMES`] frames (same-run check, with
/// FH1_P2_BURST: the frames are tagged with [`LampSortAb`]).
fn lamp_sort_setup(mut commands: Commands) {
    let v = std::env::var("FH1_CARFX_LENS_SORT").unwrap_or_default();
    let on = v == "1" || v == "ab";
    crate::car_material::LAMP_SORT_ON.store(on, std::sync::atomic::Ordering::Relaxed);
    // FH1_CARFX_ORDER=0: no carbin-order sort (old tie behaviour); `ab`: flipped every LAMP_AB_FRAMES frames instead.
    let order = std::env::var("FH1_CARFX_ORDER").unwrap_or_default();
    crate::car_material::PART_ORDER_ON.store(order != "0", std::sync::atomic::Ordering::Relaxed);
    let order_ab = order == "ab";
    // FH1_CARFX_LENS_DEPTH=ab: flips the lamp covers' depth write instead (same-run check).
    let depth_ab = std::env::var("FH1_CARFX_LENS_DEPTH").is_ok_and(|v| v == "ab");
    commands.insert_resource(LampSortAb { ab: v == "ab" || order_ab || depth_ab, order_ab, depth_ab, on: if order_ab || depth_ab { true } else { on }, frame: 0 });
}

pub const LAMP_AB_FRAMES: u32 = 20;

#[derive(Resource)]
pub struct LampSortAb {
    pub ab: bool,
    /// The A/B flips the carbin part order (FH1_CARFX_ORDER=ab) instead of the lamp role sort.
    pub order_ab: bool,
    /// The A/B flips the lamp covers' depth write (FH1_CARFX_LENS_DEPTH=ab).
    pub depth_ab: bool,
    /// Lamp sort order state of this frame.
    pub on: bool,
    frame: u32,
}

fn lamp_sort_ab(mut ab: ResMut<LampSortAb>, mut mats: ResMut<Assets<FxCarMaterial>>) {
    if !ab.ab {
        return;
    }
    ab.frame += 1;
    let on = (ab.frame / LAMP_AB_FRAMES) % 2 == 0;
    if on != ab.on {
        ab.on = on;
        if ab.depth_ab {
            let ids: Vec<_> = mats.iter().filter(|(_, m)| m.lens).map(|(id, _)| id).collect();
            for id in ids {
                if let Some(mut m) = mats.get_mut(id) {
                    m.depth_write = on;
                }
            }
            return;
        }
        let flag = if ab.order_ab { &crate::car_material::PART_ORDER_ON } else { &crate::car_material::LAMP_SORT_ON };
        flag.store(on, std::sync::atomic::Ordering::Relaxed);
        // Touch the materials so Bevy re-prepares them (depth_bias is read when the material is prepared).
        let order_ab = ab.order_ab;
        let ids: Vec<_> = mats.iter().filter(|(_, m)| order_ab || m.sort_bias != 0.0).map(|(id, _)| id).collect();
        for id in ids {
            let _ = mats.get_mut(id);
        }
    }
}
