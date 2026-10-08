//! FH1's post-processing chain, rebuilt from the host code (docs/SHADERS.md, "Post chain";
//! default.xex chain function 0x82465460). Every pass runs the game's own post shaders.
//!
//! Per frame:
//! 1. `DownSample16XGammaCorrect` scene → ¼ view (c64), then → 256×128 (e6c).
//! 2. `DownSampleBiased` → 64×32 (ea0).
//! 3. `MultiSampleAndAccumulate`: adapted luminance (persistent 1×1; last frame's value blended in
//!    by `tonemapDelay` per frame).
//! 4. Bloom: `Add_HotExtract` (¼-view image + last frame's bloom × persist), 4 Gaussian levels
//!    (X then Y, ½..¹⁄₁₆ of the bloom size), combined bottom-up with `Add2Textures_UVScale` into
//!    the persistent bloom target.
//! 5. Light rays (host 0x82479240, after bloom): `PPLightRaysZPass` (sky visibility from depth along
//!    the ray to the sun, ½ view), `PPDownSample16XLightRays` (⅛), `PPLightRaysComposite` (scene +
//!    rays, full view; FinalCombine reads it as the scene).
//! 6. `FinalCombine_BloomTonemap_Vignette`: exposure from the adapted luminance (piecewise key
//!    map), Hable filmic curve, 16³ colour-grading LUT, colorTransform, vignette.
//!
//! Inputs: `Ribbon_00/TimeOfDay.xml` <posteffects>, dynamicpost's `Bloom/ColorSettings/Vignette`
//! templates, `PostProcessingZones_Safe.xml` zones and `ColourGradingMaps/*.dds` LUTs, and the
//! TimeOfDayA multiplier channels.
//!
//! Deviations, documented: bloom blur targets use exact sizes instead of the game's ×32-rounded
//! render targets with sub-rect viewports (a few % in blur texel size), and the LUT day/night/zone
//! weighting is INFERRED.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::math::{Mat3, Vec2, Vec3, Vec4};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use fh1_shaders::container::Stage;

use crate::lighting::FxTimeOfDay;
use crate::post::{load_xex_shader, FxPostCamera, FxPostChain, PostPass, PostSource, PostTarget};

// Shaders (default.xex virtual addresses).
const VS_BLIT: u32 = 0x821a_9d00;
const PS_DOWN16_GAMMA: u32 = 0x821a_be70;
const PS_DOWN_BIASED: u32 = 0x821a_c44c;
const PS_LUM_ACCUM: u32 = 0x821a_c8e8;
const VS_HOT: u32 = 0x821a_a81c;
const PS_HOT: u32 = 0x821b_568c;
const VS_GAUSS: u32 = 0x821a_b300;
const PS_GAUSS_X: u32 = 0x821b_c9f0;
const PS_GAUSS_Y: u32 = 0x821b_cc14;
const PS_ADD2: u32 = 0x821b_54c8;
const VS_LR: u32 = 0x821a_9e6c;
const PS_LR_Z: u32 = 0x821b_19c4;
const PS_LR_DOWN: u32 = 0x821b_1be0;
const PS_LR_COMPOSITE: u32 = 0x821b_1d78;
const VS_FINAL: u32 = 0x821a_ab54;
const PS_FINAL_VIGNETTE: u32 = 0x821b_5d4c;

// Persistent targets.
const SLOT_LUM: u32 = 1;
const SLOT_BLOOM: u32 = 2;

// Pass indices in the chain.
const P_DOWN1: usize = 0;
const P_DOWN2: usize = 1;
const P_BIASED: usize = 2;
const P_LUM: usize = 3;
const P_HOT: usize = 4;
const P_GAUSS: usize = 5; // 8 passes: (X, Y) per level
const P_ADD: usize = 13; // 4 passes: L3+L2, +L1, +L0, +hot
const P_LR_Z: usize = 17;
const P_LR_DOWN: usize = 18;
const P_LR_COMPOSITE: usize = 19;
const P_FINAL: usize = 20;

/// The game's camera near plane (live PF +0x1F0 = 0.3, far 20000; Xenia). Its depth buffer is
/// INFERRED reversed (near/z, sky 0), so Bevy's near/z depth is rescaled by 0.3 / near.
const GAME_NEAR: f32 = 0.3;

/// Light-ray peak (scene units) at or below which the light-ray passes are skipped (see `update_post`).
const LR_SKIP_PEAK: f32 = 0.0;

/// Gaussian tap weights (VERIFIED, 0x82405850).
const GAUSS_WEIGHTS: [f32; 4] = [0.24095, 0.21264, 0.12200, 0.04488];

fn attr(tag: &str, name: &str) -> Option<f32> {
    let pat = format!(" {name}=\"");
    let s = tag.find(&pat)? + pat.len();
    tag[s..s + tag[s..].find('"')?].trim().parse().ok()
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let s = xml.find(&format!("<{name} "))?;
    Some(&xml[s..s + xml[s..].find('>')?])
}

/// `<filmicTone>` (Hable curve parameters).
#[derive(Debug, Clone, Copy)]
pub struct FilmicTone {
    pub white: f32,
    pub shoulder_strength: f32,
    pub linear_strength: f32,
    pub linear_angle: f32,
    pub toe_strength: f32,
    pub toe_numerator: f32,
    pub toe_denominator: f32,
    pub exposure: f32,
    /// TrackSettings' `*Night` variants of (white, shoulderStrength, linearStrength, linearAngle, toeStrength); the
    /// game blends day -> night by the TOD `DayNightPostProcess` channel (VERIFIED, Pinyon 20:00 capture).
    pub night: Option<[f32; 5]>,
}

impl Default for FilmicTone {
    /// The game's built-in defaults (0x8256e648) for a missing tag.
    fn default() -> Self {
        Self { white: 10.0, shoulder_strength: 0.11, linear_strength: 0.01, linear_angle: 0.1, toe_strength: 0.2, toe_numerator: 0.01, toe_denominator: 0.3, exposure: 0.0, night: None }
    }
}

impl FilmicTone {
    /// The curve at `DayNightPostProcess` = `k` (0 day, 1 night): lerp of the five parameters that have a night
    /// variant (VERIFIED: Pinyon tod_frame7448 at 20:00, k = 0.90167, uploads white 10.017, shoulder 0.27622,
    /// linear 0.093903 / 0.3635, toe 0.31017 = lerp(day, night, k); 08:00 and 16:00 (k = 0) upload the day values).
    pub fn at_night_amount(&self, k: f32) -> Self {
        let Some(n) = self.night else { return *self };
        let k = k.clamp(0.0, 1.0);
        let l = |a: f32, b: f32| a + (b - a) * k;
        Self {
            white: l(self.white, n[0]),
            shoulder_strength: l(self.shoulder_strength, n[1]),
            linear_strength: l(self.linear_strength, n[2]),
            linear_angle: l(self.linear_angle, n[3]),
            toe_strength: l(self.toe_strength, n[4]),
            ..*self
        }
    }
}

/// `Ribbon_00/TimeOfDay.xml` <posteffects> (the game's PF struct, 0x8256a0a8).
#[derive(Debug, Clone)]
pub struct PostEffects {
    pub bloom_scale: f32,
    pub bloom_cutoff: f32,
    pub bloom_persist: f32,
    pub bloom_lower: f32,
    pub bloom_upper: f32,
    pub bloom_weights: [f32; 5],
    pub desat_amount: f32,
    pub desat_lum: Vec3,
    pub desat_post_mod: Vec3,
    pub tonemap_delay: f32,
    pub keys: [f32; 4],
    pub exps: [f32; 4],
    pub filmic: FilmicTone,
    pub brightness: f32,
    pub contrast: f32,
    pub exposure_bias: f32,
    /// The vignette used outside every zone.
    pub vignette: VignetteTemplate,
    /// `<lightrays>`: enabled, (sunAnglePower, intensity, lengthScale, depthMultiplier).
    pub lightrays_enabled: bool,
    pub lightrays: Vec4,
}

/// `tag()` inside `<block>..</block>` only (TrackSettings' bloom/vignette children have generic names).
fn block<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let s = xml.find(&format!("<{name}>"))?;
    let e = s + xml[s..].find(&format!("</{name}>"))?;
    Some(&xml[s..e])
}

impl PostEffects {
    pub fn parse(xml: &str) -> Self {
        let f = |t: &str, a: &str, d: f32| tag(xml, t).and_then(|t| attr(t, a)).unwrap_or(d);
        let filmic = tag(xml, "filmicTone")
            .map(|t| {
                let d = FilmicTone::default();
                FilmicTone {
                    white: attr(t, "white").unwrap_or(d.white),
                    shoulder_strength: attr(t, "shoulderStrength").unwrap_or(d.shoulder_strength),
                    linear_strength: attr(t, "linearStrength").unwrap_or(d.linear_strength),
                    linear_angle: attr(t, "linearAngle").unwrap_or(d.linear_angle),
                    toe_strength: attr(t, "toeStrength").unwrap_or(d.toe_strength),
                    toe_numerator: attr(t, "toeNumerator").unwrap_or(d.toe_numerator),
                    toe_denominator: attr(t, "toeDenominator").unwrap_or(d.toe_denominator),
                    exposure: attr(t, "exposure").unwrap_or(d.exposure),
                    night: None,
                }
            })
            .unwrap_or_default();
        Self {
            bloom_scale: f("bloom", "bloomScale", 1.0),
            bloom_cutoff: f("bloom", "bloomCutoff", 1.0),
            bloom_persist: f("bloom", "bloomPersist", 0.0),
            bloom_lower: f("bloom", "bloomLowerGradient", 0.0),
            bloom_upper: f("bloom", "bloomUpperGradient", 1.0),
            bloom_weights: [0, 1, 2, 3, 4].map(|i| f("bloom", &format!("bloomWeights{i}"), 1.0)),
            desat_amount: f("desat", "desaturateAmount", 1.0),
            desat_lum: Vec3::new(f("desatLuminosity", "c0", 0.2125), f("desatLuminosity", "c1", 0.7154), f("desatLuminosity", "c2", 0.0721)),
            desat_post_mod: Vec3::new(f("desatPostMod", "c0", 1.0), f("desatPostMod", "c1", 1.0), f("desatPostMod", "c2", 1.0)),
            tonemap_delay: f("tonemapDelay", "val", 0.075),
            keys: [
                f("toneAdaptiveExposure", "key_extraDark", 0.0),
                f("toneAdaptiveExposure", "key_dark", 255.75),
                f("toneAdaptiveExposure", "key_light", 767.25),
                f("toneAdaptiveExposure", "key_extraLight", 1023.0),
            ],
            exps: [
                f("toneAdaptiveExposure", "exp_extraDark", 0.0),
                f("toneAdaptiveExposure", "exp_dark", 0.0),
                f("toneAdaptiveExposure", "exp_light", 0.0),
                f("toneAdaptiveExposure", "exp_extraLight", 0.0),
            ],
            filmic,
            brightness: f("brightness", "value", 0.0),
            contrast: f("contrast", "value", 0.0),
            exposure_bias: 0.0,
            vignette: VignetteTemplate { scale: Vec2::ONE, power: 3.0, min_intensity: 0.2, max_intensity: 1.0, ..default() },
            lightrays_enabled: f("lightrays", "enabled", 0.0) != 0.0,
            lightrays: Vec4::new(f("lightrays", "sunAnglePower", 1.0), f("lightrays", "intensity", 0.5), f("lightrays", "lengthScale", 0.5), f("lightrays", "depthMultiplier", 90.0)),
        }
    }

    /// The track's `TrackSettings.xml` overrides the TimeOfDay.xml values: the game's base PF (the
    /// zone manager's record used outside every zone, G+0x330) holds TrackSettings' colour, exposure,
    /// filmic, bloom and vignette values (VERIFIED in Xenia at the festival, 15:48: brightness 0,
    /// contrast 0, desatPostMod 1.04/1.02/1, shoulderStrength 0.7, bloom gradients 1/1, vignette
    /// min 0.25). Light rays, motion blur and DOF stay from TimeOfDay.xml (live PF matches it).
    pub fn apply_track_settings(&mut self, xml: &str) {
        let v = |t: &str, a: &str| tag(xml, t).and_then(|t| attr(t, a));
        let set = |dst: &mut f32, x: Option<f32>| {
            if let Some(x) = x {
                *dst = x;
            }
        };
        set(&mut self.desat_amount, v("DesaturateAmount", "value"));
        for (i, c) in ["r", "g", "b"].iter().enumerate() {
            set(&mut self.desat_lum[i], v("DesatLuminosity", c));
            set(&mut self.desat_post_mod[i], v("DesatPostMod", c));
        }
        set(&mut self.tonemap_delay, v("ToneMapeDelay", "val"));
        for (i, k) in ["ExtraDark", "Dark", "Light", "ExtraLight"].iter().enumerate() {
            set(&mut self.keys[i], v("ToneMapAdaptiveExposure", &format!("Key_{k}")));
            set(&mut self.exps[i], v("ToneMapAdaptiveExposure", &format!("Exp_{k}")));
        }
        if let Some(t) = tag(xml, "filmicTone") {
            // Day values, plus the *Night variants (blended by DayNightPostProcess in update_post).
            let f = &mut self.filmic;
            let night = ["whiteNight", "shoulderStrengthNight", "linearStrengthNight", "linearAngleNight", "toeStrengthNight"].map(|a| attr(t, a));
            for (dst, a) in [
                (&mut f.white, "white"),
                (&mut f.shoulder_strength, "shoulderStrength"),
                (&mut f.linear_strength, "linearStrength"),
                (&mut f.linear_angle, "linearAngle"),
                (&mut f.toe_strength, "toeStrength"),
                (&mut f.toe_numerator, "toeNumerator"),
                (&mut f.toe_denominator, "toeDenominator"),
                (&mut f.exposure, "exposure"),
            ] {
                set(dst, attr(t, a));
            }
            if night.iter().all(Option::is_some) {
                f.night = Some(night.map(|x| x.unwrap_or(0.0)));
            }
        }
        set(&mut self.exposure_bias, v("ExposureBias", "value"));
        set(&mut self.contrast, v("Contrast", "value"));
        set(&mut self.brightness, v("Brightness", "value"));
        if let Some(b) = block(xml, "bloom") {
            let g = |t: &str, a: &str| tag(b, t).and_then(|t| attr(t, a));
            set(&mut self.bloom_cutoff, g("cutoff", "value"));
            set(&mut self.bloom_scale, g("scale", "value"));
            set(&mut self.bloom_persist, g("persist", "value"));
            set(&mut self.bloom_lower, g("curve", "lowerGradient"));
            set(&mut self.bloom_upper, g("curve", "upperGradient"));
            for i in 0..5 {
                set(&mut self.bloom_weights[i], g("weights", &format!("layer{i}")));
            }
        }
        if let Some(b) = block(xml, "vignette") {
            let g = |t: &str, a: &str| tag(b, t).and_then(|t| attr(t, a));
            let vg = &mut self.vignette;
            set(&mut vg.angle, g("angle", "value"));
            set(&mut vg.pos.x, g("pos", "x"));
            set(&mut vg.pos.y, g("pos", "y"));
            set(&mut vg.scale.x, g("scale", "x"));
            set(&mut vg.scale.y, g("scale", "y"));
            set(&mut vg.power, g("power", "value"));
            set(&mut vg.color.x, g("color", "r"));
            set(&mut vg.color.y, g("color", "g"));
            set(&mut vg.color.z, g("color", "b"));
            set(&mut vg.min_intensity, g("minIntensity", "value"));
            set(&mut vg.max_intensity, g("maxIntensity", "value"));
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BloomTemplate {
    pub cutoff: f32,
    pub scale: f32,
    pub persist: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ColorTemplate {
    pub desat_amount: f32,
    pub desat_lum: Vec3,
    pub desat_post_mod: Vec3,
    pub exposure_bias: f32,
    pub contrast: f32,
    pub brightness: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VignetteTemplate {
    pub angle: f32,
    pub pos: Vec2,
    pub scale: Vec2,
    pub power: f32,
    pub color: Vec4,
    pub min_intensity: f32,
    pub max_intensity: f32,
}

fn templates<T>(xml: &str, parse: impl Fn(&str) -> T) -> HashMap<String, T> {
    let mut out = HashMap::new();
    for raw in xml.split("<Template ").skip(1) {
        let t = format!(" {}", raw.split('>').next().unwrap_or(""));
        let name = t.find(" Name=\"").map(|s| {
            let s = s + 7;
            t[s..s + t[s..].find('"').unwrap_or(0)].to_string()
        });
        if let Some(n) = name {
            out.insert(n, parse(&t));
        }
    }
    out
}

pub struct Zone {
    pub name: String,
    pub bloom: String,
    pub color_map: String,
    pub color_map_night: String,
    pub color_settings: String,
    pub fog: String,
    pub vignette: String,
    pub lerp_time: f32,
    /// Triangles in XML space (posX, posZ).
    pub tris: Vec<[Vec2; 3]>,
}

impl Zone {
    fn contains(&self, p: Vec2) -> bool {
        self.tris.iter().any(|t| {
            let s = |a: Vec2, b: Vec2, c: Vec2| (a.x - c.x) * (b.y - c.y) - (b.x - c.x) * (a.y - c.y);
            let (d1, d2, d3) = (s(p, t[0], t[1]), s(p, t[1], t[2]), s(p, t[2], t[0]));
            !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
        })
    }
}

pub fn parse_zones(xml: &str) -> Vec<Zone> {
    let mut out = Vec::new();
    for block in xml.split("<PostProcessingZone ").skip(1) {
        let head = format!(" {}", block.split('>').next().unwrap_or(""));
        let s = |n: &str| -> String {
            let pat = format!(" {n}=\"");
            head.find(&pat)
                .map(|i| {
                    let i = i + pat.len();
                    head[i..i + head[i..].find('"').unwrap_or(0)].to_string()
                })
                .unwrap_or_default()
        };
        let mut tris = Vec::new();
        // Split on the closing tag: "<PostProcessingZoneTriangle" is also a prefix of the point tag.
        let block = block.split("</PostProcessingZone>").next().unwrap_or("");
        for tri in block.split("</PostProcessingZoneTriangle>") {
            let pts: Vec<Vec2> = tri
                .split("<PostProcessingZoneTrianglePoint")
                .skip(1)
                .filter_map(|p| Some(Vec2::new(attr(&format!(" {p}"), "posX")?, attr(&format!(" {p}"), "posZ")?)))
                .collect();
            if pts.len() >= 3 {
                tris.push([pts[0], pts[1], pts[2]]);
            }
        }
        out.push(Zone {
            name: s("Name"),
            bloom: s("BloomTemplate"),
            color_map: s("ColorGradingMap"),
            color_map_night: s("ColorGradingMapNight"),
            color_settings: s("ColorSettings"),
            fog: s("Fog"),
            vignette: s("VignetteTop"),
            lerp_time: s("LerpTime").parse().unwrap_or(15.0),
            tris,
        });
    }
    out
}

/// Where the post data lives.
#[derive(Clone, Debug, PartialEq)]
pub struct PostPaths {
    /// Installed `shaders/xex` (default.xex shader containers).
    pub xex_dir: PathBuf,
    /// `Ribbon_00/TimeOfDay.xml`.
    pub timeofday: PathBuf,
    /// The track's `TrackSettings.xml` (base PF overrides; empty path = none).
    pub track_settings: PathBuf,
    /// dynamicpost `Tracks/<track>` (Bloom/ColorSettings/Fog/Vignette _Templates.xml).
    pub templates_dir: PathBuf,
    /// dynamicpost `ColourGradingMaps`.
    pub luts_dir: PathBuf,
    /// `Ribbon_00/PostProcessingZones_Safe.xml`.
    pub zones: PathBuf,
    /// The track's default (day, night) LUTs, used outside every zone (`ColorGradingLookup{,_Night}.dds`).
    pub default_luts: (PathBuf, PathBuf),
    /// The track's converted scenery folder; `glow.rs` reads `props/glows.json` from it.
    pub scenery_dir: PathBuf,
}

/// A 16³ RGBA colour-grading LUT (RGBA8, x fastest).
type Lut = Vec<[u8; 4]>;

fn read_lut(path: &Path) -> Option<Lut> {
    let b = std::fs::read(path).ok()?;
    if b.get(..4)? != b"DDS " || b.len() < 128 + 16384 {
        return None;
    }
    // A8R8G8B8 (masks R 0xff0000): bytes B, G, R, A.
    Some(b[128..128 + 16384].chunks_exact(4).map(|p| [p[2], p[1], p[0], p[3]]).collect())
}

fn identity_lut() -> Lut {
    let mut v = Vec::with_capacity(4096);
    for z in 0..16u32 {
        for y in 0..16u32 {
            for x in 0..16u32 {
                v.push([(x * 255 / 15) as u8, (y * 255 / 15) as u8, (z * 255 / 15) as u8, 255]);
            }
        }
    }
    v
}

/// Main-world post state: settings, zone tracking, LUT.
#[derive(Resource)]
pub struct FxPost {
    pub base: PostEffects,
    pub bloom_templates: HashMap<String, BloomTemplate>,
    pub color_templates: HashMap<String, ColorTemplate>,
    pub vignette_templates: HashMap<String, VignetteTemplate>,
    pub zones: Vec<Zone>,
    luts_dir: PathBuf,
    default_luts: (PathBuf, PathBuf),
    luts: HashMap<PathBuf, Lut>,
    pub lut_image: Handle<Image>,
    /// (previous zone, current zone, seconds since the change).
    zone: (Option<usize>, Option<usize>, f32),
    /// Last camera position (XML space), to snap the zone blend on teleports.
    last_pos: Option<Vec2>,
    frames: u32,
    last_lut_key: Option<(Option<usize>, Option<usize>, u32, u32, u32)>,
}

impl FxPost {
    /// (previous zone, current zone, blend 0..1 = smoothstep(elapsed / lerp_time)), as `update_post` uses them.
    pub fn zone_blend(&self) -> (Option<&Zone>, Option<&Zone>, f32) {
        let t = self.zone.1.map(|z| self.zones[z].lerp_time).unwrap_or(15.0).max(1e-3);
        (self.zone.0.map(|z| &self.zones[z]), self.zone.1.map(|z| &self.zones[z]), smoothstep01(self.zone.2 / t))
    }

    fn lut(&mut self, path: PathBuf) -> Lut {
        if !self.luts.contains_key(&path) {
            let l = read_lut(&path).unwrap_or_else(identity_lut);
            self.luts.insert(path.clone(), l);
        }
        self.luts[&path].clone()
    }
}

fn lut_image(data: &Lut) -> Image {
    let mut img = Image::new(
        Extent3d { width: 16, height: 16, depth_or_array_layers: 16 },
        TextureDimension::D3,
        data.iter().flatten().copied().collect(),
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        address_mode_w: ImageAddressMode::ClampToEdge,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        ..default()
    });
    img
}

/// Build the chain and the post state. Returns None if a shader is missing.
pub fn setup(paths: &PostPaths, shaders: &mut Assets<Shader>, images: &mut Assets<Image>) -> Option<(FxPostChain, FxPost)> {
    let sh = |a: u32| load_xex_shader(&paths.xex_dir, a);
    let blit = sh(VS_BLIT)?;
    let mut passes = Vec::new();
    let quarter = PostTarget::Scaled { sx: 0.25, sy: 0.25, round: 1 };
    passes.push(PostPass::new("down16_a", blit.clone(), sh(PS_DOWN16_GAMMA)?, vec![(0, PostSource::Scene)], quarter, shaders));
    passes.push(PostPass::new("down16_b", blit.clone(), sh(PS_DOWN16_GAMMA)?, vec![(0, PostSource::Pass(P_DOWN1))], PostTarget::Fixed { w: 256, h: 128 }, shaders));
    passes.push(PostPass::new("down_biased", blit.clone(), sh(PS_DOWN_BIASED)?, vec![(0, PostSource::Pass(P_DOWN2))], PostTarget::Fixed { w: 64, h: 32 }, shaders));
    passes.push(PostPass::new(
        "lum",
        blit.clone(),
        sh(PS_LUM_ACCUM)?,
        vec![(0, PostSource::Pass(P_BIASED)), (1, PostSource::Previous(SLOT_LUM))],
        PostTarget::Persistent { slot: SLOT_LUM, w: 1, h: 1 },
        shaders,
    ));
    let full = PostTarget::Scaled { sx: 1.0, sy: 1.0, round: 1 };
    passes.push(PostPass::new(
        "hot",
        sh(VS_HOT)?,
        sh(PS_HOT)?,
        vec![(1, PostSource::Pass(P_DOWN1)), (0, PostSource::Previous(SLOT_BLOOM)), (17, PostSource::Pass(P_LUM))],
        full,
        shaders,
    ));
    let gauss_vs = sh(VS_GAUSS)?;
    for level in 0..4 {
        let k = 0.5f32.powi(level + 1);
        let size = PostTarget::Scaled { sx: k, sy: k, round: 1 };
        let src = if level == 0 { P_HOT } else { P_GAUSS + 2 * (level as usize - 1) + 1 };
        passes.push(PostPass::new(&format!("gauss{level}x"), gauss_vs.clone(), sh(PS_GAUSS_X)?, vec![(0, PostSource::Pass(src))], size, shaders));
        let x = passes.len() - 1;
        passes.push(PostPass::new(&format!("gauss{level}y"), gauss_vs.clone(), sh(PS_GAUSS_Y)?, vec![(0, PostSource::Pass(x))], size, shaders));
    }
    let add = sh(PS_ADD2)?;
    let level_out = |l: usize| P_GAUSS + 2 * l + 1;
    let half = |l: i32| PostTarget::Scaled { sx: 0.5f32.powi(l + 1), sy: 0.5f32.powi(l + 1), round: 1 };
    passes.push(PostPass::new("add32", blit.clone(), add.clone(), vec![(0, PostSource::Pass(level_out(3))), (1, PostSource::Pass(level_out(2)))], half(2), shaders));
    passes.push(PostPass::new("add1", blit.clone(), add.clone(), vec![(0, PostSource::Pass(P_ADD)), (1, PostSource::Pass(level_out(1)))], half(1), shaders));
    passes.push(PostPass::new("add0", blit.clone(), add.clone(), vec![(0, PostSource::Pass(P_ADD + 1)), (1, PostSource::Pass(level_out(0)))], half(0), shaders));
    passes.push(PostPass::new(
        "add_hot",
        blit.clone(),
        add,
        vec![(0, PostSource::Pass(P_ADD + 2)), (1, PostSource::Pass(P_HOT))],
        PostTarget::PersistentScaled { slot: SLOT_BLOOM, sx: 1.0, sy: 1.0 },
        shaders,
    ));
    // Light rays. ZPass at ½ view (reducedRes = 1; the size is INFERRED), the 4-tap box to ⅛.
    let lr_vs = sh(VS_LR)?;
    // The mask targets are single-channel: the downsample reads the ZPass result from .w (its export
    // writes w = 1), so those fetches replicate red like the game's 1-channel fetch constant (INFERRED).
    let r16 = TextureFormat::R16Float;
    passes.push(PostPass::new("lr_z", lr_vs.clone(), sh(PS_LR_Z)?, vec![(0, PostSource::Depth)], PostTarget::Scaled { sx: 0.5, sy: 0.5, round: 1 }, shaders).with_format(r16));
    let down_target = PostTarget::Scaled { sx: 0.125, sy: 0.125, round: 1 };
    passes.push(PostPass::with_fetch_swizzle("lr_down", blit.clone(), sh(PS_LR_DOWN)?, vec![(0, PostSource::Pass(P_LR_Z))], down_target, &[0], shaders).with_format(r16));
    passes.push(PostPass::with_fetch_swizzle("lr_composite", lr_vs, sh(PS_LR_COMPOSITE)?, vec![(0, PostSource::Pass(P_LR_DOWN)), (1, PostSource::Scene)], full, &[0], shaders));
    let lut = images.add(lut_image(&identity_lut()));
    let mut final_pass = PostPass::new(
        "final",
        sh(VS_FINAL)?,
        sh(PS_FINAL_VIGNETTE)?,
        vec![(0, PostSource::Pass(P_LR_COMPOSITE)), (2, PostSource::Pass(P_ADD + 3)), (7, PostSource::Image(lut.clone())), (17, PostSource::Pass(P_LUM))],
        PostTarget::View,
        shaders,
    );
    final_pass.flip_uv = true;
    passes.push(final_pass);
    debug_assert_eq!(passes.len(), P_FINAL + 1);

    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mut base = PostEffects::parse(&read(&paths.timeofday));
    base.apply_track_settings(&read(&paths.track_settings));
    let bloom_templates = templates(&read(&paths.templates_dir.join("Bloom_Templates.xml")), |t| BloomTemplate {
        cutoff: attr(t, "Cutoff").unwrap_or(1.0),
        scale: attr(t, "Scale").unwrap_or(1.0),
        persist: attr(t, "Persist").unwrap_or(0.0),
    });
    let color_templates = templates(&read(&paths.templates_dir.join("ColorSettings_Templates.xml")), |t| ColorTemplate {
        desat_amount: attr(t, "DesatAmount").unwrap_or(1.0),
        desat_lum: Vec3::new(attr(t, "DesatLuminosityR").unwrap_or(0.2125), attr(t, "DesatLuminosityG").unwrap_or(0.7154), attr(t, "DesatLuminosityB").unwrap_or(0.0721)),
        desat_post_mod: Vec3::new(attr(t, "DesatPostModR").unwrap_or(1.0), attr(t, "DesatPostModG").unwrap_or(1.0), attr(t, "DesatPostModR").unwrap_or(1.0)),
        exposure_bias: attr(t, "ExposureBias").unwrap_or(0.0),
        contrast: attr(t, "Contrast").unwrap_or(0.0),
        brightness: attr(t, "Brightness").unwrap_or(0.0),
    });
    let vignette_templates = templates(&read(&paths.templates_dir.join("Vignette_Templates.xml")), |t| VignetteTemplate {
        angle: attr(t, "Angle").unwrap_or(0.0),
        pos: Vec2::new(attr(t, "PosX").unwrap_or(0.0), attr(t, "PosY").unwrap_or(0.0)),
        scale: Vec2::new(attr(t, "ScaleX").unwrap_or(1.0), attr(t, "ScaleY").unwrap_or(1.0)),
        power: attr(t, "Power").unwrap_or(3.0),
        color: Vec4::new(attr(t, "ColorR").unwrap_or(0.0), attr(t, "ColorG").unwrap_or(0.0), attr(t, "ColorB").unwrap_or(0.0), attr(t, "ColorA").unwrap_or(0.0)),
        min_intensity: attr(t, "MinIntensity").unwrap_or(1.0),
        max_intensity: attr(t, "MaxIntensity").unwrap_or(1.0),
    });
    let zones = parse_zones(&read(&paths.zones));
    let post = FxPost {
        base,
        bloom_templates,
        color_templates,
        vignette_templates,
        zones,
        luts_dir: paths.luts_dir.clone(),
        default_luts: paths.default_luts.clone(),
        luts: HashMap::new(),
        lut_image: lut,
        zone: (None, None, 0.0),
        last_pos: None,
        frames: 0,
        last_lut_key: None,
    };
    Some((FxPostChain { passes, alloc_per_frame: false }, post))
}

fn smoothstep01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Per frame: zone tracking, settings blend and every pass's constants.
pub(crate) fn update_post(
    post: Option<ResMut<FxPost>>,
    chain: Option<ResMut<FxPostChain>>,
    tod: Option<Res<FxTimeOfDay>>,
    cams: Query<(&GlobalTransform, &Camera, &Projection), With<FxPostCamera>>,
    time: Res<Time>,
    mut images: ResMut<Assets<Image>>,
) {
    let (Some(mut post), Some(mut chain)) = (post, chain) else { return };
    if chain.passes.len() != P_FINAL + 1 {
        return;
    }
    let Some((cam_t, cam, projection)) = cams.iter().next() else { return };
    let viewport = cam.physical_viewport_size().unwrap_or(UVec2::new(1280, 720)).as_vec2();

    // Zone at the camera (XML space: z mirrored).
    let p = cam_t.translation();
    let here = Vec2::new(p.x, -p.z);
    let zone = post.zones.iter().position(|z| z.contains(here));
    if zone != post.zone.1 {
        let prev = post.zone.1;
        // Spawn / fast travel (camera jumps > 100 m in a frame): no blend from the old place (INFERRED).
        let jumped = post.last_pos.is_none_or(|l| l.distance(here) > 100.0);
        post.zone = (if jumped { zone } else { prev }, zone, 0.0);
    } else {
        post.zone.2 += time.delta_secs();
    }
    post.last_pos = Some(here);
    let (prev_zone, cur_zone, elapsed) = post.zone;
    if std::env::var_os("FH1_POST_LOG").is_some() && post.frames % 120 == 0 {
        let name = |z: Option<usize>| z.map(|z| post.zones[z].name.clone()).unwrap_or_else(|| "-".into());
        info!("post: xml ({:.0}, {:.0}) zone {} <- {} t {:.2}", here.x, here.y, name(cur_zone), name(prev_zone), elapsed);
    }
    let lerp_time = cur_zone.map(|z| post.zones[z].lerp_time).unwrap_or(15.0).max(1e-3);
    let t = smoothstep01(elapsed / lerp_time);

    let minutes = tod.as_ref().map(|t| t.minutes()).unwrap_or(720.0);
    let mult = |name: &str, d: f32| -> f32 {
        tod.as_ref().and_then(|t| t.tod.channels.get(name).map(|c| c.eval(minutes)[0])).unwrap_or(d)
    };

    // Templates: blend previous → current zone.
    let bloom_of = |z: Option<usize>| -> BloomTemplate {
        z.and_then(|z| post.bloom_templates.get(&post.zones[z].bloom).copied())
            .unwrap_or(BloomTemplate { cutoff: post.base.bloom_cutoff, scale: post.base.bloom_scale, persist: post.base.bloom_persist })
    };
    let (ba, bb) = (bloom_of(prev_zone), bloom_of(cur_zone));
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    let base = post.base.clone();
    let bloom_scale = lerp(ba.scale, bb.scale) * mult("BloomScaleMultiplier", 1.0);
    let bloom_cutoff = lerp(ba.cutoff, bb.cutoff) * mult("BloomCutoffMultiplier", 1.0);
    let bloom_persist = lerp(ba.persist, bb.persist) * mult("BloomPersistMultiplier", 1.0);
    let bloom_lower = base.bloom_lower * mult("BloomLowerGradientMultiplier", 0.0);
    let bloom_upper = base.bloom_upper * mult("BloomUpperGradientMultiplier", 1.0);
    let mut w: [f32; 5] = base.bloom_weights.map(|x| x * 0.2);
    // Debugging aid: FX_POST_DEBUG=nobloom zeroes the bloom weights, =noexposure pins exposure.
    let debug = std::env::var("FX_POST_DEBUG").unwrap_or_default();
    if debug.contains("nobloom") {
        w = [0.0; 5];
    }

    let color_of = |z: Option<usize>| -> ColorTemplate {
        z.and_then(|z| post.color_templates.get(&post.zones[z].color_settings).copied()).unwrap_or(ColorTemplate {
            desat_amount: base.desat_amount,
            desat_lum: base.desat_lum,
            desat_post_mod: base.desat_post_mod,
            // Outside every zone: the base PF = TrackSettings.xml (0 for Colorado; VERIFIED in Xenia).
            // TimeOfDay.xml's brightness/contrast 0.5 never reach the live PF.
            exposure_bias: base.exposure_bias,
            contrast: base.contrast,
            brightness: base.brightness,
        })
    };
    let (ca, cb) = (color_of(prev_zone), color_of(cur_zone));
    let desat = lerp(ca.desat_amount, cb.desat_amount) * mult("DesatAmountMultiplier", 1.0);
    let desat_lum = ca.desat_lum.lerp(cb.desat_lum, t) * mult("DesatLuminosityMultiplier", 1.0);
    let contrast = lerp(ca.contrast, cb.contrast) * mult("ContrastMultiplier", 1.0);
    let desat_post_mod = ca.desat_post_mod.lerp(cb.desat_post_mod, t);
    // Zone lerp × TOD multiplier, component by component (VERIFIED, zone update 0x824f1ae0).
    let brightness = lerp(ca.brightness, cb.brightness) * mult("BrightnessMultiplier", 1.0);
    let exposure_bias = lerp(ca.exposure_bias, cb.exposure_bias) * mult("ExposureBiasMultiplier", 1.0);

    let vig_of = |z: Option<usize>| -> VignetteTemplate {
        // Outside every zone the TrackSettings vignette keeps its colour alpha as a factor (the pre-2026-10-05 rule):
        // with colour.w = VignetteAmount alone its black colour would darken the edges, unverified (docs/SHADERS.md).
        z.and_then(|z| post.vignette_templates.get(&post.zones[z].vignette).copied())
            .unwrap_or(VignetteTemplate { max_intensity: base.vignette.max_intensity * base.vignette.color.w, ..base.vignette })
    };
    let (va, vb) = (vig_of(prev_zone), vig_of(cur_zone));
    let vig = VignetteTemplate {
        angle: lerp(va.angle, vb.angle),
        pos: va.pos.lerp(vb.pos, t),
        scale: va.scale.lerp(vb.scale, t),
        power: lerp(va.power, vb.power),
        color: va.color.lerp(vb.color, t),
        min_intensity: lerp(va.min_intensity, vb.min_intensity),
        max_intensity: lerp(va.max_intensity, vb.max_intensity),
    };

    // Filmic + exposure (VERIFIED packing). Night curve blended in by DayNightPostProcess (VERIFIED at 20:00);
    // FH1_POST_FILMIC_NIGHT=0 = day curve always (the pre-2026-10-05 behaviour).
    let f = if std::env::var("FH1_POST_FILMIC_NIGHT").as_deref() == Ok("0") { base.filmic } else { base.filmic.at_night_amount(mult("DayNightPostProcess", 0.0)) };
    let (a, b, c, d, e, ff) = (f.shoulder_strength, f.linear_strength, f.linear_angle, f.toe_strength, f.toe_numerator, f.toe_denominator);
    let filmic1 = [f.white, a, b, c];
    let filmic2 = [d, e, ff, f.exposure + exposure_bias];
    let filmic_e = [c * b, d * ff, d * e, e / ff];
    let keys = base.keys.map(|k| k / 255.0);
    let exps = if debug.contains("noexposure") { [0.0; 4] } else { base.exps };

    let reset = if post.frames == 0 { 1.0 } else { 0.0 };
    post.frames = post.frames.saturating_add(1);
    let ab_baseline = post_ab_on() && !post_ab_mode(time.elapsed_secs());
    chain.alloc_per_frame = ab_baseline;
    let passes = &mut chain.passes;

    // Luminance adaptation.
    passes[P_LUM].set("previousFrameAmount", &[[base.tonemap_delay; 4]]);
    passes[P_LUM].set("tonemapReset", &[[reset; 4]]);
    passes[P_LUM].set("tonemapWeightRange", &[[0.0; 4]]);

    // Hot extract: srcTexPreMultiply c32.x = persist·60, c34.xyz = 1/255,
    // c35 = (cutoff, scale, lower, upper) (VERIFIED from the shader + 0x82405850).
    let hot = &mut passes[P_HOT];
    hot.set_reg(Stage::Pixel, 32, [bloom_persist * 60.0, 0.0, 0.0, 0.0]);
    hot.set_reg(Stage::Pixel, 33, [0.0; 4]);
    hot.set_reg(Stage::Pixel, 34, [1.0 / 255.0, 1.0 / 255.0, 1.0 / 255.0, 0.0]);
    hot.set_reg(Stage::Pixel, 35, [bloom_cutoff, bloom_scale, bloom_lower, bloom_upper]);
    hot.set("tonemapAdaptiveLuminanceKeys", &[keys]);
    hot.set("tonemapAdaptiveExposure", &[exps]);
    hot.set("filmicParams2", &[filmic2]);

    // Gaussian levels.
    for i in 0..8 {
        let p = &mut passes[P_GAUSS + i];
        p.set("gaussWeight", &[GAUSS_WEIGHTS]);
        p.set("vertexUVScale", &[[1.0, 1.0, 0.0, 0.0]]);
    }
    // Combine: srcTexPreMultiply c32 = (uv scale, weight0), c33 = (uv scale, weight1).
    let add_weights = [(w[4], w[3]), (1.0, w[2]), (1.0, w[1]), (1.0, w[0])];
    for (k, (w0, w1)) in add_weights.iter().enumerate() {
        passes[P_ADD + k].set("srcTexPreMultiply", &[[1.0, 1.0, *w0, 0.0], [1.0, 1.0, *w1, 0.0]]);
    }

    // Light rays (VERIFIED in Xenia at 15:48: sun = normalize(SunObjectPos - SunObjectTargetPos)
    // × 10000, intensity / colour = the TOD LightRaysIntensity / LightRaysColour channels, the rest
    // from TimeOfDay.xml <lightrays>). The sun is projected from the camera (INFERRED) to uv with
    // (0, 0) top-left; behind the camera the rays are off (the host's skip test is not decoded).
    let lr_game = std::env::var("FH1_POST_LR_GAME").as_deref() != Ok("0");
    let mut lr = base.lightrays;
    let mut colour = Vec3::ZERO;
    let mut sun_uv = Vec2::splat(0.5);
    if let (true, Some(tod)) = (base.lightrays_enabled && !debug.contains("nolightrays"), tod.as_ref()) {
        let pos = |n: &str| {
            let v = tod.tod.get(n, minutes);
            Vec3::new(v[0], v[1], -v[2])
        };
        let dir = (pos("SunObjectPos") - pos("SunObjectTargetPos")).normalize_or_zero();
        let fwd = cam_t.forward().as_vec3();
        if dir.dot(fwd) > 0.0 {
            if let Some(ndc) = cam.world_to_ndc(cam_t, cam_t.translation() + dir * 10_000.0) {
                sun_uv = Vec2::new(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
                lr.y = tod.tod.scalar_or("LightRaysIntensity", minutes, lr.y);
                // Pinyon capture at 08:00 (docs/SHADERS.md "Light rays"): the game's intensity = TOD × 0.807 there,
                // = dot(view forward, sun direction) (INFERRED fit). FH1_POST_LR_GAME=0 = the old constants.
                if lr_game {
                    lr.y *= dir.dot(fwd).max(0.0);
                }
                colour = Vec3::from_array(tod.tod.get("LightRaysColour", minutes));
            }
        }
    }
    if colour == Vec3::ZERO {
        lr.y = 0.0;
    }
    if std::env::var_os("FH1_POST_LOG").is_some() && post.frames % 120 == 0 {
        info!("post: light rays sun uv ({:.2}, {:.2}) intensity {:.4} colour {:.3}", sun_uv.x, sun_uv.y, lr.y, colour);
    }
    // FX_POST_DEBUG=lrboost: ×50 intensity, to see the rays' shape by day.
    if debug.contains("lrboost") {
        lr.y *= 50.0;
    }
    let near = match projection {
        Projection::Perspective(p) => p.near,
        _ => GAME_NEAR,
    };
    let mut depth_mul = lr.w * GAME_NEAR / near.max(1e-4);
    // The game's ZPass input is not the depth buffer but a ¼-view 8_8_8_8 mask (Pinyon tod_frame6447, eid 15019 tf0,
    // VERIFIED texels): 0 for sky, >= ~0.1 for every piece of geometry (car 0.7, far mountains 0.3-0.47), so with
    // depthMultiplier 90 any geometry blocks the rays fully. Our reversed-Z depth is 0 only for sky: a huge multiplier
    // gives the same mask (any depth > 1e-7 blocks). FH1_POST_LR_MASK=0 = the old depth × 90 (leaks past ~27 m).
    // FX_POST_DEBUG=lrocclude keeps working (same value).
    if std::env::var("FH1_POST_LR_MASK").as_deref() != Ok("0") || debug.contains("lrocclude") {
        depth_mul = 1e7;
    }
    let settings = [lr.x, lr.y, lr.z, depth_mul];
    // Game values at 08:00 (VERIFIED, Pinyon): sunAnglePower 0.5 (XML 1.0), ZPass length 1.0 and composite 0.2 (XML
    // lengthScale 0.5): mapped as ×0.5 / ×2 / ×0.4 of the XML (scaling INFERRED, values VERIFIED for Colorado).
    let (settings_z, settings_c) = if lr_game {
        ([lr.x * 0.5, lr.y, lr.z * 2.0, depth_mul], [lr.x * 0.5, lr.y, lr.z * 0.4, depth_mul])
    } else {
        (settings, settings)
    };
    let sun = [sun_uv.x, sun_uv.y, 0.0, 0.0];
    for k in [P_LR_Z, P_LR_COMPOSITE] {
        // The VS maps uv × (1280, 720) back to uv through invViewport (texcoord1 = uv).
        passes[k].set("invViewport", &[[1.0 / 1280.0, 0.0, 1.0 / 720.0, 0.0]]);
        passes[k].set("lightRaySettings", &[if k == P_LR_Z { settings_z } else { settings_c }]);
        passes[k].set("lightRaySunPos", &[sun]);
    }
    passes[P_LR_COMPOSITE].set("lightRayColor", &[[colour.x, colour.y, colour.z, 0.0]]);
    // Skip the light rays (depth resolve, ZPass, downsample, full-view composite) when they cannot add a visible
    // amount: the composite adds colour × intensity × mean(8 mask taps ≤ 1) × saturate(1 − |sun − uv| / sunAnglePower)
    // to the scene, so its peak over the screen is bounded by the nearest screen point to the sun.
    // FinalCombine then reads the scene directly. FH1_POST_LR_SKIP=<peak> sets the threshold (0 = only when off).
    let d_min = (sun_uv - sun_uv.clamp(Vec2::ZERO, Vec2::ONE)).length();
    let radial = (1.0 - d_min / lr.x.max(1e-4)).clamp(0.0, 1.0);
    let lr_peak = lr.y.max(0.0) * colour.max_element().max(0.0) * radial;
    let lr_skip_at: f32 = std::env::var("FH1_POST_LR_SKIP").ok().and_then(|v| v.parse().ok()).unwrap_or(LR_SKIP_PEAK);
    let skip_lr = !ab_baseline && lr_peak <= lr_skip_at;
    for k in [P_LR_Z, P_LR_DOWN, P_LR_COMPOSITE] {
        passes[k].skip = skip_lr;
    }
    if std::env::var_os("FH1_POST_LOG").is_some() && post.frames % 120 == 0 {
        info!("post: light rays peak {lr_peak:.4} (radial {radial:.2}) skip {skip_lr}");
    }

    // Final combine.
    let fp = &mut passes[P_FINAL];
    fp.set("filmicParams1", &[filmic1]);
    fp.set("filmicParams2", &[filmic2]);
    fp.set("filmicParamsE", &[filmic_e]);
    fp.set("tonemapAdaptiveLuminanceKeys", &[keys]);
    fp.set("tonemapAdaptiveExposure", &[exps]);
    fp.set("mirrorValue", &[[1.0, 0.0, 0.0, 0.0]]);
    fp.set("motionBlurParams1", &[[0.0; 4]]);
    // Colour grading: 16³ LUT.
    fp.set("colorGradingEnable", &[[1.0; 4]]);
    fp.set("colorGradingTexScale", &[[15.0 / 16.0; 4]]);
    fp.set("colorGradingTexOffset", &[[1.0 / 32.0; 4]]);
    // colorTransform (3×4, on linear colour; VERIFIED, builder 0x82402ab4): contrast
    // (x - 0.5)(1 + c) + 0.5, then + brightness, then saturation lerp(dot(x, lum), x, desat), then
    // × desatPostMod per channel. (The screen-fade step after it is idle in free roam.)
    let sat = Mat3::from_cols(desat_lum, desat_lum, desat_lum).transpose() * (1.0 - desat) + Mat3::IDENTITY * desat;
    let post_mod = Mat3::from_diagonal(desat_post_mod);
    let k = 1.0 + contrast;
    let m = post_mod * sat * k;
    let tr = post_mod * sat * Vec3::splat(-0.5 * contrast + brightness);
    let rows = [0, 1, 2].map(|r| {
        let row = m.row(r);
        [row.x, row.y, row.z, tr[r]]
    });
    fp.set("colorTransform", &rows);
    // Vignette (VERIFIED packing; ScaleOffsetPower.xyz from the host globals at +0x16c, which the
    // VS needs as (1, 0, 0) to map its flipped quad uv back to screen uv).
    let th = vig.angle.to_radians();
    let asp = viewport.x / viewport.y.max(1.0);
    fp.set("vignetteTransform", &[[th.cos() * vig.scale.x * asp, th.sin() * vig.scale.x, -th.sin() * vig.scale.y * asp, th.cos() * vig.scale.y]]);
    fp.set("vignetteScaleOffsetPower", &[[1.0, 0.0, 0.0, vig.power * 0.5]]);
    // The game samples a scene stored rotated 180° (VS: 1 - uv), so its vignette coordinate is r = 0.5 - uv; we feed
    // flipped uv for our upright scene, giving r = uv - 0.5. The mask |T(r + pos)| is invariant under (r, pos) -> -(r, pos),
    // so the negated offset reproduces the game's mask (VERIFIED against a Pinyon capture: strong darkening at the top).
    fp.set("vignettePosOffset", &[[-vig.pos.x, -vig.pos.y, 0.0, 0.0]]);
    let amount = mult("VignetteAmount", 1.0);
    // vignetteColor.w = MaxIntensity × the TOD VignetteAmount; the template's ColorA is not part of it (VERIFIED,
    // Pinyon capture: Redstone, ColorA 0, 16:00 -> 0.999415 = VignetteAmount; MaxIntensity vs MinIntensity INFERRED).
    fp.set("vignetteColor", &[[vig.color.x, vig.color.y, vig.color.z, vig.max_intensity * amount]]);
    if std::env::var_os("FH1_POST_LOG").is_some() && post.frames % 120 == 0 {
        info!(
            "post: final vigT {:?} vigPow {:.3} vigPos {:?} vigCol {:?} amount {amount:.3} colorTransform {rows:?} filmic1 {filmic1:?} filmic2 {filmic2:?}",
            [th.cos() * vig.scale.x * asp, th.sin() * vig.scale.x, -th.sin() * vig.scale.y * asp, th.cos() * vig.scale.y],
            vig.power * 0.5,
            vig.pos,
            vig.color,
        );
    }

    // LUT: one 16³ volume blended on the CPU (VERIFIED, 0x824e3b38): per zone
    // a = (day - I)·DayAmount + (night - I)·NightAmount, out = I + a_prev + t(a_cur - a_prev).
    // Outside every zone the track defaults ColorGradingLookup{,_Night}.dds (handles loaded in Xenia; files INFERRED).
    let day_amt = mult("DayColorGradeAmount", 1.0);
    let night_amt = mult("NightColorGradeAmount", 0.0);
    let key = (prev_zone, cur_zone, (t * 64.0) as u32, (day_amt * 64.0) as u32, (night_amt * 64.0) as u32);
    if post.last_lut_key != Some(key) {
        post.last_lut_key = Some(key);
        let ident = identity_lut();
        let mut graded = |z: Option<usize>| -> Vec<[f32; 4]> {
            let (day, night) = match z {
                Some(z) => {
                    let p = |n: &str| (!n.is_empty()).then(|| post.luts_dir.join(n));
                    (p(&post.zones[z].color_map), p(&post.zones[z].color_map_night))
                }
                // Both default handles (G+0x4d0 day, G+0xa30 night) are loaded in the game (Xenia); that
                // they are these files is INFERRED. FH1_POST_DEFAULT_LUT=0 = identity outside zones.
                None if std::env::var("FH1_POST_DEFAULT_LUT").as_deref() != Ok("0") => (Some(post.default_luts.0.clone()), Some(post.default_luts.1.clone())),
                None => (None, None),
            };
            let dl = day.map(|p| post.lut(p)).unwrap_or_else(|| ident.clone());
            let nl = night.map(|p| post.lut(p)).unwrap_or_else(|| ident.clone());
            (0..4096)
                .map(|i| {
                    let mut o = [0f32; 4];
                    for (c, oc) in o.iter_mut().enumerate() {
                        let id = ident[i][c] as f32;
                        *oc = id + (dl[i][c] as f32 - id) * day_amt + (nl[i][c] as f32 - id) * night_amt;
                    }
                    o
                })
                .collect()
        };
        let a = graded(prev_zone);
        let b = graded(cur_zone);
        let data: Lut = (0..4096).map(|i| [0, 1, 2, 3].map(|c| (a[i][c] + (b[i][c] - a[i][c]) * t).round().clamp(0.0, 255.0) as u8)).collect();
        let handle = post.lut_image.clone();
        if let Some(mut img) = images.get_mut(&handle) {
            img.data = Some(data.iter().flatten().copied().collect());
        }
    }
}

/// Insert to enable the FH1 post chain (then put `bevy::camera::Hdr` + `post::FxPostCamera` on the
/// 3D camera, with `Tonemapping::None`). Read once at startup.
#[derive(Resource, Clone, Debug)]
pub struct FxPostConfig(pub PostPaths);

impl FxPostConfig {
    /// Standard install layout under `<assets>/private`.
    pub fn from_assets(assets: &Path) -> Self {
        Self::from_track(assets, &assets.join("tracks/colorado"), &assets.join("dynamicpost"), "Colorado", &assets.join("scenery/colorado"))
    }

    /// Another Horizon-engine track (e.g. FH2's Anthem under `imported/fh2/anthem`, docs/FH2_RECON.md): `tracks` = the
    /// tracks group folder (TimeOfDay, TrackSettings, post zones), `dynamicpost` = a dynamicpost group folder holding
    /// `Tracks/<track>` and `ColourGradingMaps`, `scenery` = the scenery group folder. Shaders stay the install's.
    pub fn from_track(assets: &Path, tracks: &Path, dynamicpost: &Path, track: &str, scenery: &Path) -> Self {
        let templates = dynamicpost.join("Tracks").join(track);
        Self(PostPaths {
            xex_dir: assets.join("shaders/xex"),
            timeofday: tracks.join("TimeOfDay.xml"),
            track_settings: tracks.join("TrackSettings.xml"),
            luts_dir: dynamicpost.join("ColourGradingMaps"),
            zones: tracks.join("PostProcessingZones_Safe.xml"),
            default_luts: (templates.join("ColorGradingLookup.dds"), templates.join("ColorGradingLookup_Night.dds")),
            templates_dir: templates,
            scenery_dir: scenery.to_path_buf(),
        })
    }
}

/// Startup: build the chain from `FxPostConfig` and switch materials to the game's raw
/// (sqrt-encoded) output, which the chain expects.
pub(crate) fn init_post(
    mut commands: Commands,
    config: Option<Res<FxPostConfig>>,
    mut lib: ResMut<crate::FxLibrary>,
    mut shaders: ResMut<Assets<Shader>>,
    mut images: ResMut<Assets<Image>>,
) {
    let Some(config) = config else { return };
    commands.insert_resource(AppliedPost(config.0.clone()));
    match setup(&config.0, &mut shaders, &mut images) {
        Some((chain, post)) => {
            lib.raw_output = !crate::remaster();
            commands.insert_resource(chain);
            commands.insert_resource(post);
            info!("fh1-render: FH1 post chain enabled");
        }
        None => warn!("fh1-render: post chain shaders not found in {}", config.0.xex_dir.display()),
    }
}

/// The track paths the post chain (and the other track consumers) were last built from.
#[derive(Resource, Clone, Debug, PartialEq)]
pub(crate) struct AppliedPost(pub PostPaths);

/// In-process map change: forget the applied track paths, so the next [`FxPostConfig`] insert rebuilds the chain and
/// sends [`FxTrackChanged`] even when the new map shares the old one's post inputs (its glows still change).
pub fn invalidate_track(world: &mut World) {
    world.remove_resource::<AppliedPost>();
}

/// Sent when [`FxPostConfig`] switched to another track's paths after startup (in-process map change): glows,
/// reflections and fog templates reload their track data on it (the post chain itself is rebuilt by [`reload_post`]).
#[derive(Message, Clone, Debug)]
pub struct FxTrackChanged;

/// In-process map change: a new [`FxPostConfig`] (other track paths) rebuilds the post chain and tells the other
/// track consumers ([`FxTrackChanged`]). Startup's config is applied by [`init_post`] and skipped here.
pub(crate) fn reload_post(
    mut commands: Commands,
    config: Option<Res<FxPostConfig>>,
    applied: Option<Res<AppliedPost>>,
    mut lib: ResMut<crate::FxLibrary>,
    mut shaders: ResMut<Assets<Shader>>,
    mut images: ResMut<Assets<Image>>,
    mut changed: MessageWriter<FxTrackChanged>,
) {
    let Some(config) = config.filter(|c| c.is_changed()) else { return };
    if applied.is_some_and(|a| a.0 == config.0) {
        return;
    }
    commands.insert_resource(AppliedPost(config.0.clone()));
    match setup(&config.0, &mut shaders, &mut images) {
        Some((chain, post)) => {
            lib.raw_output = !crate::remaster();
            commands.insert_resource(chain);
            commands.insert_resource(post);
            info!("fh1-render: post chain rebuilt for {}", config.0.timeofday.parent().map_or_else(String::new, |p| p.display().to_string()));
        }
        None => warn!("fh1-render: post chain shaders not found in {}", config.0.xex_dir.display()),
    }
    changed.write(FxTrackChanged);
}

/// `FH1_POST_AB=1` (perf tool, the FH1_SHADOW_AB pattern; needs FH1_P2_STATS=1 for the GPU spans): `update_post`
/// alternates baseline (every pass runs, per-frame sampler/uniform allocation) and optimised every `AB_SECS`;
/// this logs the per-pass GPU ms (render/fh1_post/*) and frame time per mode, skipping each phase's first second.
pub(crate) fn post_ab_report(time: Res<Time>, store: Option<Res<bevy::diagnostic::DiagnosticsStore>>, mut ab: Local<PostAb>) {
    if !post_ab_on() {
        return;
    }
    let mode = post_ab_mode(time.elapsed_secs());
    let phase_t = time.elapsed_secs() % AB_SECS;
    if ab.last_mode != Some(mode) {
        ab.last_mode = Some(mode);
        ab.phases += 1;
        if ab.phases % 4 == 1 && ab.phases > 1 {
            ab.log();
        }
    }
    if phase_t < 1.0 || time.elapsed_secs() < 20.0 {
        return;
    }
    let m = &mut ab.modes[mode as usize];
    m.frames.push(time.delta_secs() * 1000.0);
    let Some(store) = store else { return };
    for d in store.iter() {
        let path = d.path().as_str();
        let Some(name) = path.strip_prefix("render/").and_then(|p| p.strip_suffix("/elapsed_gpu")) else { continue };
        if !name.starts_with("fh1_post") {
            continue;
        }
        if let Some(v) = d.measurement().map(|x| x.value) {
            let e = m.gpu.entry(name.to_owned()).or_default();
            e.0 += v;
            e.1 += 1;
        }
    }
}

const AB_SECS: f32 = 4.0;

pub(crate) fn post_ab_on() -> bool {
    std::env::var("FH1_POST_AB").is_ok_and(|v| v == "1")
}

/// false = baseline, true = optimised.
pub(crate) fn post_ab_mode(t: f32) -> bool {
    (t / AB_SECS) as u32 % 2 == 1
}

#[derive(Default)]
pub(crate) struct PostAb {
    last_mode: Option<bool>,
    phases: u32,
    modes: [AbMode; 2],
}

#[derive(Default)]
struct AbMode {
    frames: Vec<f32>,
    gpu: std::collections::BTreeMap<String, (f64, u32)>,
}

impl PostAb {
    fn log(&self) {
        for (k, name) in [(0, "baseline"), (1, "optimised")] {
            let m = &self.modes[k];
            let mut f = m.frames.clone();
            f.sort_by(f32::total_cmp);
            let mean = f.iter().sum::<f32>() / f.len().max(1) as f32;
            let p99 = f.get(f.len().saturating_sub(1).min(f.len() * 99 / 100)).copied().unwrap_or(0.0);
            let passes: Vec<String> = m.gpu.iter().map(|(n, (s, c))| format!("{} {:.3}", n.trim_start_matches("fh1_post/"), s / *c as f64)).collect();
            info!("post AB {name}: frame {mean:.2}/{p99:.2} ms ({} frames); GPU ms: {}", f.len(), passes.join(", "));
        }
    }
}
