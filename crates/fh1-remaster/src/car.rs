//! Remaster cars (W2): every car (player, AI, traffic; FH1, FH2 and FM4 imports) drawn from its own glTF scene
//! (`cars/<CAR>/model.gltf`) with a handful of PBR materials instead of the translated car effects (fh1-render car.rs).
//!
//! - The glTF primitives keep their meshes, textures and UVs; each one's material is restyled by its Turn 10 material
//!   name (= the game's technique, docs/WHEELS.md "Car materials"): paint (clear coat + flake, car_paint.rs), glass,
//!   chrome, lamp lenses/covers, emissive lamps, trim, rubber, interior, wheels and brakes (wheel.rs).
//! - Paint = the body's `FxCarPaint` Combo_Colors row (`physics.json` `colors`), else the stock `model.json` `paint`.
//! - Lamps follow the car's `FxCarLamps` (brake/reverse/indicators/fog) and `FxHeadlightState` (headlights), as the
//!   faithful car bank does (docs/SHADERS.md "Lamp emissive"); lamp materials are per car, all others are shared by every
//!   car with the same glTF material and paint.
//! - Reflections: Bevy's environment map light on the camera (W3), no cube of our own.
//!
//! Hooks onto the faithful spawn (`fh1_render::car::FxCarBody` on the glTF holder); fh1-render skips its own car and
//! wheel parts when FH1_RENDERER=remaster. `FxCarDrawn` (empty: nothing to hide) marks the body as built for the traffic
//! pool. Flags: FH1_RM_CAR=0 leaves the glTF materials as they are; FH1_RM_LAMP=<nits> lamp emissive at full on (default 2e4; W3 exposure is physical EV, emissive in nits);
//! FH1_RM_FLAKE=<strength> metallic flake (0 = off); FH1_RM_PAINT_SHEEN=0 = old metallic paint base (0.55 metal / 0.38 rough;
//! new FH1_RM_PAINT_METALLIC 0.3 / FH1_RM_PAINT_METAL_ROUGH 0.55, see `restyle_car_materials`); FH1_RM_PAINTCOLOR=0 draws non-body atlas techniques without their
//! ShaderSettings PaintColor (the old khaki matte_colors); FH1_RM_PAINTSCALE=0 ignores PaintScale; FH1_RM_LAMP_TINT=0
//! leaves red/amber lamp parts untinted.
//!
//! Lamp look (2026-10-08, user: lamps "close but something looks off": white hot spots, a red wash over the rear panel,
//! no lens depth):
//! - Emission is capped at FH1_RM_LAMP_CAP display units (3; 0 = uncapped, old): nits = min(FH1_RM_LAMP, cap x 1.2 x
//!   2^ev100). The physical 2e4 nits sat at ~500x display white at night exposure: bloom spread it over the whole rear
//!   panel and the curve flattened every lamp to a clipped blob. Capped, a full brake lamp is ~3x white (bloom still
//!   catches it, the hue stays) in any light; by day the physical level is below the cap and unchanged.
//! - Lenses and covers get a clear coat (FH1_RM_LAMP_COAT, 1; 0 = none): a sharp sun / env highlight and Fresnel on
//!   the outer plastic over the emission, so the lamp reads as a lit unit behind a clear cover.
//!
//! Transparency LOD (P8, 2026-10-08): every blended car material (glass, lamp layers, lenses, covers) has a far twin
//! out of the transparent phase (glass opaque with its premultiplied tint, lamp layers alpha-masked at 0.5), and parts
//! farther than FH1_RM_CAR_TRANSP_LOD_M (60 m, +-5 m hysteresis) from the main camera swap to it, checked 4x a second.
//! That moves traffic / AI glass and lamps out of the sorted one-draw-per-item transparent pass. The player car is
//! always nearer than that to the driving camera, so it keeps the full layering. FH1_RM_CAR_TRANSP_LOD=0 = old.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::gltf::GltfMaterialName;
use bevy::prelude::*;
use bevy::gltf::{GltfAssetLabel, GltfMeshName};
use bevy::world_serialization::{WorldAssetRoot, WorldInstanceReady};
use fh1_render::car::{FxCarBody, FxCarDrawn, FxCarKit, FxCarLamps, FxCarPaint, FxCarPaintRgb, FxCarRim, ShaderSettings, StockKit};
use fh1_render::headlight::FxHeadlightState;

use crate::car_paint::{CarPaint, CarPaintMaterial};

/// A lamp a material lights with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lamp {
    /// Head / xenon / HID / full beam / daytime running lamps (headlight amount).
    Head,
    /// Tail lamps: running light with the headlights, full on the brake.
    Tail,
    Reverse,
    IndicatorLeft,
    IndicatorRight,
    Fog,
    /// Side markers and the number-plate lamp (dim, with the headlights).
    Marker,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// The glTF base colour and texture as they are, with this look's PBR values.
    Surface,
    /// Body paint (car_paint.rs).
    Paint,
    /// Polished metal (glTF texture dropped).
    Chrome,
    /// Window glass: premultiplied blend, tint and opacity from the glTF factor.
    Glass,
    /// Dark glass (`black_glass*`: the black frit band / tinted panes): like glass, but keeps the glTF opacity (0.9).
    DarkGlass,
    /// Rear-window heater lines (`defrost_lines`): a sheet over the glass that is transparent but for the lines in the
    /// atlas alpha, so alpha-masked.
    Defrost,
    /// Lamp lens drawn opaque from the lights atlas (`detail_glass_*`, the game's pass 0), lit with its lamp.
    Lens(Option<Lamp>),
    /// Blended cover over a lamp (`lights_gls_*`, `lights_glass`), lit with its lamp.
    Cover(Option<Lamp>),
    /// A lamp (lights atlas), emissive while its lamp is on.
    Lamp(Lamp),
}

/// How one car material is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub kind: Kind,
    pub metallic: f32,
    pub roughness: f32,
    pub reflectance: f32,
    pub clearcoat: f32,
    pub clearcoat_roughness: f32,
}

impl Look {
    /// A textured dielectric (the glTF base colour and texture kept).
    pub const fn textured() -> Self {
        Self { kind: Kind::Surface, metallic: 0.0, roughness: 0.5, reflectance: 0.5, clearcoat: 0.0, clearcoat_roughness: 0.1 }
    }
    /// The glTF constant colour kept (same as [`Look::textured`]: the factor and any texture stay).
    pub const fn keep_colour() -> Self {
        Self::textured()
    }
    pub const fn chrome(roughness: f32) -> Self {
        Self { kind: Kind::Chrome, metallic: 1.0, roughness, ..Self::textured() }
    }
    const fn rough(roughness: f32) -> Self {
        Self { roughness, ..Self::textured() }
    }
    const fn coated(roughness: f32, metallic: f32) -> Self {
        Self { roughness, metallic, clearcoat: 1.0, clearcoat_roughness: 0.04, ..Self::textured() }
    }
    const fn of(kind: Kind) -> Self {
        Self { kind, roughness: 0.12, clearcoat: 1.0, clearcoat_roughness: 0.03, ..Self::textured() }
    }
}

/// Lower case, without the `_noOcclude` / `_NoShadow(s)` tool suffixes.
fn base_name(name: &str) -> String {
    let mut n = name.to_ascii_lowercase();
    for s in ["_noshadows", "_noshadow", "_noocclude"] {
        while let Some(i) = n.find(s) {
            n.replace_range(i..i + s.len(), "");
        }
    }
    n
}

fn lamp(n: &str) -> Option<Lamp> {
    let n = n.trim_end_matches("_lod0");
    if n.contains("noemit") {
        return None;
    }
    if n.contains("reverse") {
        return Some(Lamp::Reverse);
    }
    if n.contains("indicator_left") || n.starts_with("slinl") || n.starts_with("lindtail") || n.starts_with("drlinl") {
        return Some(Lamp::IndicatorLeft);
    }
    if n.contains("indicator_right") || n.starts_with("slinr") || n.starts_with("rindtail") || n.starts_with("drlinr") {
        return Some(Lamp::IndicatorRight);
    }
    if n.starts_with("fog") {
        return Some(Lamp::Fog);
    }
    if n.contains("tail") || n == "linred" || n == "rinred" || n == "brinl" || n == "brinr" || n == "drlred" {
        return Some(Lamp::Tail);
    }
    if n.contains("head") || n == "fullbeam" || n.starts_with("drl") {
        return Some(Lamp::Head);
    }
    if n == "numplate" || (n.starts_with("sl") && n.ends_with("orange")) || n == "sidelightorange" {
        return Some(Lamp::Marker);
    }
    None
}

/// The look of a glTF car material by name.
pub fn look(name: &str) -> Look {
    let n = base_name(name);
    let n = n.as_str();
    if let Some(r) = n.strip_prefix("lights_gls_") {
        return Look::of(Kind::Cover(lamp(r)));
    }
    if n == "lights_glass" {
        return Look::of(Kind::Cover(None));
    }
    if let Some(c) = n.strip_prefix("detail_glass_") {
        let l = match c {
            "red" => Some(Lamp::Tail),
            "clear" => Some(Lamp::Head),
            _ => None,
        };
        return Look::of(Kind::Lens(l));
    }
    // Paint techniques (fh1-render car.rs PAINT_TECHS).
    const PAINT: [&str; 9] = ["body", "body_2", "body_3", "body_sh", "body_traffic", "engine_body", "headlight_paint", "lights_body", "stripe_badge"];
    if PAINT.contains(&n) {
        return Look { kind: Kind::Paint, ..Look::textured() };
    }
    if let Some(w) = crate::wheel::look(n) {
        return w;
    }
    if let Some(l) = lamp(n) {
        return Look::of(Kind::Lamp(l));
    }
    if n.contains("reflector") {
        return Look::of(Kind::Surface);
    }
    if n.starts_with("mirror") {
        return Look::chrome(0.0);
    }
    if n.starts_with("chrome") || n == "badge_chrome" {
        return Look::chrome(0.07);
    }
    if n.contains("black_glass") {
        return Look { kind: Kind::DarkGlass, roughness: 0.05, ..Look::textured() };
    }
    if n.starts_with("window") || n.starts_with("glass") || n == "frontwindow" || n.ends_with("_glass") {
        return Look { kind: Kind::Glass, roughness: 0.03, ..Look::textured() };
    }
    if n.starts_with("carbon_fiber") {
        return Look::coated(0.35, 0.0);
    }
    if n.starts_with("emblem") || n.starts_with("badge") || n.starts_with("liverybadge") {
        return Look::coated(0.3, 0.3);
    }
    if n.starts_with("grille") {
        return Look { metallic: 0.2, ..Look::rough(0.45) };
    }
    if n.starts_with("matte_colors") || n.starts_with("matt_colors") {
        return Look::rough(0.62);
    }
    if n.starts_with("rubber") || n == "weatherstrip" {
        return Look { reflectance: 0.35, ..Look::rough(0.85) };
    }
    if n == "black" || n == "bottom" || n.starts_with("plastic") || n.starts_with("bump_plastic") {
        return Look::rough(0.58);
    }
    if n.contains("interior") || n.contains("leather") || n.contains("seat") || n == "undercarriage" {
        return Look { reflectance: 0.35, ..Look::rough(0.8) };
    }
    if n == "defrost_lines" {
        return Look { kind: Kind::Defrost, ..Look::rough(0.4) };
    }
    Look::textured()
}

/// Interior materials (seats, dash, door cards, steering wheel, gauges, headliner).
fn is_cabin(name: &str) -> bool {
    let n = base_name(name);
    ["interior", "seat", "leather", "steering", "gauge", "dash", "door_card", "doorcard", "headliner", "suede", "carpet"].iter().any(|w| n.contains(w))
}

/// Linear paint colour from Combo_Colors RGB (gamma-encoded; the game uploads it sRGB-degammed, VERIFIED in
/// fh1-render car.rs).
fn paint_colour(rgb: u32) -> LinearRgba {
    let d = |s: u32| fh1_render::car::srgb_degamma((s & 255) as f32 / 255.0);
    LinearRgba::rgb(d(rgb >> 16), d(rgb >> 8), d(rgb))
}

/// (RGB, metallic) of a car body: the `FxCarPaint` row of `physics.json` `colors`, else `model.json` `paint`.
fn body_paint(car_dir: &Path, choice: Option<u32>) -> Option<(u32, bool)> {
    let json = |f: &str| -> serde_json::Value { fh1_render::files::read_json(&car_dir.join(f)).unwrap_or_default() };
    if let Some(seq) = choice {
        let physics = json("physics.json");
        if let Some(r) = physics["colors"].as_array().and_then(|rows| rows.iter().find(|r| r["Sequence"].as_u64() == Some(seq as u64))) {
            if let Some(rgb) = r["RGB"].as_u64() {
                return Some((rgb as u32, r["Metallic"].as_u64().unwrap_or(0) != 0));
            }
        }
    }
    let model = json("model.json");
    let p = &model["paint"];
    Some((p["rgb"].as_u64()? as u32, p["metallic"].as_bool().unwrap_or(false)))
}

/// The paint's own finish: `fx/ColorShaderSettings<seq>.xml` (the last file of the game's chain) with a body FresnelScalar
/// below 0.2 is a matte paint (e.g. FOR_FocusRS500 colour 1, LAM_Reventon_08, LAM_SestoElemento_11; faithful applies the
/// same file). FH1_RM_MATTE=0 = always glossy.
pub fn matte_finish(car_dir: &Path, choice: Option<u32>) -> bool {
    if std::env::var("FH1_RM_MATTE").is_ok_and(|v| v == "0") {
        return false;
    }
    let seq = choice.or_else(|| {
        let model: serde_json::Value = fh1_render::files::read_json(&car_dir.join("model.json")).unwrap_or_default();
        model["paint"]["sequence"].as_u64().map(|v| v as u32)
    });
    let Some(seq) = seq else { return false };
    let want = format!("colorshadersettings{seq}.xml");
    let Some(file) = std::fs::read_dir(car_dir.join("fx")).ok().and_then(|d| d.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(&want))) else { return false };
    let Some(xml) = fh1_render::files::read_to_string(&file.path()) else { return false };
    let s = ShaderSettings::parse(&xml);
    s.params.get("body").and_then(|b| b.get("FresnelScalar")).is_some_and(|v| v[0] < 0.2)
}

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Restyled materials shared between cars: (glTF material, paint) -> handle.
#[derive(Resource, Default)]
struct CarMaterials {
    standard: HashMap<AssetId<StandardMaterial>, Handle<StandardMaterial>>,
    paint: HashMap<(AssetId<StandardMaterial>, [u32; 3], bool, bool), Handle<CarPaintMaterial>>,
    /// The ShaderSettings chain (shared Normal.xml + the car's fx/ShaderSettings.xml) per car dir.
    settings: HashMap<PathBuf, Arc<ShaderSettings>>,
    /// Body paint per (car dir, Combo_Colors sequence, custom colour).
    paints: HashMap<(PathBuf, Option<u32>, Option<(u32, bool)>), (Option<(u32, bool)>, bool)>,
    /// Handles this module made (so a restyled primitive is never restyled again).
    made: HashSet<AssetId<StandardMaterial>>,
    /// Transparency LOD: the far (non-blended) twin of each shared blended material.
    far: HashMap<AssetId<StandardMaterial>, Handle<StandardMaterial>>,
}

/// On a car part with a blended material: its near (blended) and far (opaque / masked) materials.
#[derive(Component)]
struct TranspLod {
    near: Handle<StandardMaterial>,
    far: Handle<StandardMaterial>,
    far_on: bool,
}

/// FH1_RM_CAR_TRANSP_LOD=0: car glass and lamps blend at every distance (old).
fn transp_lod_on() -> bool {
    !std::env::var("FH1_RM_CAR_TRANSP_LOD").is_ok_and(|v| v == "0")
}

fn blended(m: &StandardMaterial) -> bool {
    matches!(m.alpha_mode, AlphaMode::Blend | AlphaMode::Premultiplied | AlphaMode::Add | AlphaMode::Multiply)
}

/// The far twin: premultiplied glass drawn opaque with its tinted (premultiplied) colour; blended lamp layers, lenses and
/// covers alpha-masked by the atlas alpha (the alpha-mask phase is batched like the opaque one).
fn far_twin(near: &StandardMaterial) -> StandardMaterial {
    let mut m = near.clone();
    m.alpha_mode = if near.alpha_mode == AlphaMode::Premultiplied { AlphaMode::Opaque } else { AlphaMode::Mask(0.5) };
    if m.alpha_mode == AlphaMode::Opaque {
        let c = m.base_color.to_linear();
        m.base_color = Color::LinearRgba(LinearRgba::new(c.red, c.green, c.blue, 1.0));
    }
    m.depth_bias = 0.0;
    m
}

/// On the `FxCarBody` holder: this car's own lamp materials (glTF material -> (lamp, emissive scale, restyled)).
#[derive(Component, Default)]
pub struct RemasterLamps {
    /// (lamp, emissive scale, restyled, far twin for the transparency LOD).
    lamps: HashMap<AssetId<StandardMaterial>, (Lamp, LinearRgba, Handle<StandardMaterial>, Option<Handle<StandardMaterial>>)>,
    /// Last levels written, per lamp.
    levels: HashMap<Lamp, f32>,
}

/// Normal.xml then the car's own ShaderSettings.xml (the paint finish files only touch the paint techniques, which
/// take their colour from Combo_Colors here).
fn load_settings(assets: &Path, car_dir: &Path) -> ShaderSettings {
    let read = |p: PathBuf| fh1_render::files::read_to_string(&p);
    ShaderSettings::chain(&[read(assets.join("cars/shared/ShaderSettings/Normal.xml")).as_deref(), read(car_dir.join("fx/ShaderSettings.xml")).as_deref()])
}

/// A technique's colour constant (linear: the parser degammes ShaderSettings colours like the game).
fn setting_colour(settings: &ShaderSettings, technique: &str, names: &[&str]) -> Option<LinearRgba> {
    let t = settings.params.get(technique).or_else(|| settings.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(technique)).map(|(_, v)| v))?;
    names.iter().find_map(|n| t.get(*n)).map(|c| LinearRgba::rgb(c[0], c[1], c[2]))
}

/// A technique's `PaintScale` (x), the game's scale on the atlas colour (Corrado matte_colors 0.08 = the black rear
/// panel; emblem 0.4; lamps 0.3-0.7). 1 when missing or with FH1_RM_PAINTSCALE=0.
fn paint_scale(settings: &ShaderSettings, technique: &str) -> f32 {
    if std::env::var("FH1_RM_PAINTSCALE").is_ok_and(|v| v == "0") {
        return 1.0;
    }
    let t = settings.params.get(technique).or_else(|| settings.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(technique)).map(|(_, v)| v));
    t.and_then(|t| t.get("PaintScale")).map_or(1.0, |v| v[0].clamp(0.0, 2.0))
}

/// Glass reflectance from the technique's FresnelIndex / FresnelScalar: the game's head-on reflection is
/// F0 = ((n - 1) / (n + 1))^2 x scalar (window n 4.35 x 0.7 = 0.27, glass n 3.55 x 0.6 = 0.19), not the physical 0.04
/// (2026-10-07: rear glass 0.41x the faithful luma at reflectance 0.5). Bevy F0 = 0.16 x reflectance^2.
/// FH1_RM_GLASS_FRESNEL=0 = old (0.5).
fn glass_reflectance(settings: &ShaderSettings, technique: &str) -> Option<f32> {
    if std::env::var("FH1_RM_GLASS_FRESNEL").is_ok_and(|v| v == "0") {
        return None;
    }
    let t = settings.params.get(technique).or_else(|| settings.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(technique)).map(|(_, v)| v))?;
    let n = t.get("FresnelIndex")?[0];
    let k = t.get("FresnelScalar").map_or(1.0, |v| v[0]);
    let f0 = (((n - 1.0) / (n + 1.0)).powi(2) * k * env_f32("FH1_RM_GLASS_F0", GLASS_F0)).clamp(0.02, 0.5);
    Some((f0 / 0.16).sqrt())
}

/// Scale on [`glass_reflectance`]'s F0 (FH1_RM_GLASS_F0). 0.5 = the 2026-10-07 balance: Corrado side glass 1.65x / rear glass
/// 0.67x the faithful luma (1.0: 3.5x / 1.05x; the remaster probe reflects the crowd where the game's cube shows less).
const GLASS_F0: f32 = 0.5;

/// `m`'s base colour (rgb) times `k`.
fn scale_base(m: &mut StandardMaterial, k: f32) {
    let c = m.base_color.to_linear();
    m.base_color = Color::LinearRgba(LinearRgba::new(c.red * k, c.green * k, c.blue * k, c.alpha));
}

/// The emissive colour of a lamp technique, scaled so its brightest channel is 1 (the gain sets the level).
fn lamp_colour(settings: &ShaderSettings, technique: &str, lamp: Lamp) -> LinearRgba {
    let names: &[&str] = match lamp {
        Lamp::Head => &["XenonHeadlightColor", "HidHeadlightColor", "OldHeadlightColor", "FullbeamColor"],
        Lamp::Tail => &["BrakeColor", "TailLightColor", "WhiteBrakeColor"],
        Lamp::Reverse => &["ReverseLightColor"],
        Lamp::IndicatorLeft | Lamp::IndicatorRight => &["IndicatorLightColor"],
        Lamp::Fog => &["BrakeColor", "TailLightColor"],
        Lamp::Marker => &["NumberPlateLightColor", "IndicatorLightColor"],
    };
    let fallback = match lamp {
        Lamp::Tail | Lamp::Fog => LinearRgba::rgb(1.0, 0.0, 0.0),
        Lamp::IndicatorLeft | Lamp::IndicatorRight => LinearRgba::rgb(1.0, 0.28, 0.0),
        _ => LinearRgba::WHITE,
    };
    let c = setting_colour(settings, technique, names).filter(|c| c.red.max(c.green).max(c.blue) > 1e-3).unwrap_or(fallback);
    let m = c.red.max(c.green).max(c.blue);
    LinearRgba::rgb(c.red / m, c.green / m, c.blue / m)
}

pub struct RemasterCarPlugin;

impl Plugin for RemasterCarPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var("FH1_RM_CAR").is_ok_and(|v| v == "0") {
            return;
        }
        app.add_plugins(MaterialPlugin::<CarPaintMaterial>::default()).init_resource::<CarMaterials>().init_resource::<CarFill>();
        let _ = app.world_mut().resource_mut::<Assets<Shader>>().insert(&crate::car_paint::SHADER, Shader::from_wgsl(crate::car_paint::wgsl(), "fh1_remaster/car_paint.wgsl"));
        app.add_systems(Update, (update_car_fill, mark_bodies, request_variants, restyle_car_materials, update_lamps).chain()).add_observer(apply_variants);
        if transp_lod_on() {
            app.add_systems(Update, transparency_lod.after(restyle_car_materials));
        }
    }
}

/// The game's car hemisphere fill for this time of day, in post-exposure units (car_paint.rs module doc).
#[derive(Resource, Default, Clone, Copy, PartialEq)]
pub struct CarFill {
    pub top: LinearRgba,
    pub bottom: LinearRgba,
}

/// Fill = TOD HLTopColour / HLBottomColour x IBLAmbientScale x FH1_RM_CAR_FILL x game_unit_scale. The scale stands in for
/// the game's fitted car SH (docs/SHADERS.md: DC x 0.39 of this hemisphere) plus what the remaster env map already gives.
/// FH1_RM_CAR_FILL=0 = old (env map only). Paint materials are rewritten only when the fill moved by > 2 %.
fn update_car_fill(tod: Option<Res<fh1_render::lighting::FxTimeOfDay>>, mut fill: ResMut<CarFill>, mut paints: ResMut<Assets<CarPaintMaterial>>) {
    let Some(t) = tod else { return };
    let m = t.minutes();
    let k = env_f32("FH1_RM_CAR_FILL", CAR_FILL) * t.tod.scalar_or("IBLAmbientScale", m, 1.0) * crate::post::game_unit_scale();
    let c = |n: &str| {
        let v = Vec3::from_array(t.tod.get(n, m)) * k;
        LinearRgba::rgb(v.x, v.y, v.z)
    };
    // Car sun share (car_paint.rs): IBLDirectScaleCar / IBLDirectScaleEnvironment, in fill_top.alpha. FH1_RM_CAR_SUN=0 = 1 (old).
    let keep = if std::env::var("FH1_RM_CAR_SUN").is_ok_and(|v| v == "0") {
        1.0
    } else {
        (t.tod.scalar_or("IBLDirectScaleCar", m, 1.0) / t.tod.scalar_or("IBLDirectScaleEnvironment", m, 1.0).max(1e-3)).clamp(0.0, 1.0)
    };
    let mut top = c("HLTopColour");
    top.alpha = keep;
    let want = CarFill { top, bottom: c("HLBottomColour") };
    let moved = |a: LinearRgba, b: LinearRgba| {
        (a.to_vec3() - b.to_vec3()).abs().max_element() > 0.02 * b.to_vec3().max_element().max(1e-4) || (a.alpha - b.alpha).abs() > 0.01
    };
    if !moved(want.top, fill.top) && !moved(want.bottom, fill.bottom) {
        return;
    }
    *fill = want;
    for (_, p) in paints.iter_mut() {
        p.extension.fill_top = want.top;
        p.extension.fill_bottom = want.bottom;
    }
}

/// Default [`update_car_fill`] scale (2026-10-07 same-pose A/B vs faithful, car close-up 16:00).
const CAR_FILL: f32 = 0.8;

/// The faithful path marks a body built with `FxCarDrawn` (the traffic pool counts them); the glTF body draws itself.
fn mark_bodies(mut commands: Commands, added: Query<Entity, (Added<FxCarBody>, Without<FxCarDrawn>)>) {
    for e in &added {
        commands.entity(e).insert((FxCarDrawn(HashSet::new()), RemasterLamps::default()));
    }
}

#[allow(clippy::too_many_arguments)]
fn restyle_car_materials(
    mut commands: Commands,
    added: Query<(Entity, &GltfMaterialName, &MeshMaterial3d<StandardMaterial>), Added<GltfMaterialName>>,
    parents: Query<&ChildOf>,
    bodies: Query<(&FxCarBody, Option<&FxCarPaint>, Option<&FxCarPaintRgb>)>,
    mut lamps: Query<&mut RemasterLamps>,
    mut cache: ResMut<CarMaterials>,
    mut standard: ResMut<Assets<StandardMaterial>>,
    mut paints: ResMut<Assets<CarPaintMaterial>>,
    fill: Res<CarFill>,
) {
    let flake = env_f32("FH1_RM_FLAKE", 0.12);
    for (e, name, mat) in &added {
        let src_id = mat.0.id();
        if cache.made.contains(&src_id) {
            continue;
        }
        let Some(body) = parents.iter_ancestors(e).find(|a| bodies.contains(*a)) else { continue };
        let Ok((fx, choice, custom)) = bodies.get(body) else { continue };
        let Some(src) = standard.get(src_id).cloned() else { continue };
        let lk = look(&name.0);
        // The cabin seen through the glass: the roof's cascade shadow plus the remaster's thin shade ambient made it
        // near-black behind the windows, where faithful shows the interior (its car lighting has no cabin shadow).
        // FH1_RM_CABIN_SHADOW=1 = shadowed (old).
        if is_cabin(&name.0) && !std::env::var("FH1_RM_CABIN_SHADOW").is_ok_and(|v| v == "1") {
            commands.entity(e).insert(bevy::light::NotShadowReceiver);
        }
        let dir = fx.assets.join("cars").join(&fx.car);
        let settings = cache.settings.entry(dir.clone()).or_insert_with(|| Arc::new(load_settings(&fx.assets, &dir))).clone();
        // Atlas-alpha paint: the body takes the car's paint; other exterior-atlas techniques with a PaintColor in
        // ShaderSettings (matte_colors black, emblem, plastics...) draw lerp(PaintColor, atlas, alpha) the same way
        // (the game's emblem keeps the ShaderSettings PaintColor, fh1-render car.rs).
        let painted = match lk.kind {
            Kind::Paint => {
                let seq = choice.map(|c| c.sequence);
                // The Customize menu's colour (FxCarPaintRgb) replaces the row's RGB; the finish stays the row's.
                let custom = custom.map(|c| (c.rgb & 0xFF_FFFF, c.metallic));
                let (paint, matte) = *cache.paints.entry((dir.clone(), seq, custom)).or_insert_with(|| (custom.or_else(|| body_paint(&dir, seq)), matte_finish(&dir, seq)));
                // No paint row at all: keep the glTF factor (it holds the stock paint).
                Some(match paint {
                    Some((rgb, metallic)) => (paint_colour(rgb), metallic, matte),
                    None => (src.base_color.to_linear(), false, matte),
                })
            }
            Kind::Surface if src.base_color_texture.is_some() && !std::env::var("FH1_RM_PAINTCOLOR").is_ok_and(|v| v == "0") => {
                setting_colour(&settings, &name.0, &["PaintColor"]).map(|c| (c, false, false))
            }
            _ => None,
        };
        if let Some((colour, metallic, matte)) = painted {
            let key = (src_id, [colour.red.to_bits(), colour.green.to_bits(), colour.blue.to_bits()], metallic, matte);
            let body_paint = lk.kind == Kind::Paint;
            let handle = cache
                .paint
                .entry(key)
                .or_insert_with(|| {
                    let mut base = restyle(&src, lk);
                    // The atlas part takes the technique's PaintScale (the body's own paint does not).
                    let k = if body_paint { 1.0 } else { paint_scale(&settings, &name.0) };
                    base.base_color = Color::linear_rgb(k, k, k);
                    let mut colour = colour;
                    if body_paint {
                        // Metallic paint = a clear coat over a broad, softly tinted flake sheen (FH1_RM_PAINT_SHEEN=0 = old
                        // 0.55 / 0.38). The game's Metallic.xml body: SpecularPower2 6 / SpecularPower3 4 (Phong; GGX
                        // roughness ~0.7) with PaintScale 0.5, the env reflection on top white x Fresnel (FresnelIndex 1.55).
                        // At metallic 0.55 / roughness 0.38 the base layer was a tinted near-mirror: F0 = 0.57 x paint, so an
                        // orange body (0xFF6B2B -> 1.0, 0.15, 0.02) reflected the sky and sun as pure red-orange that clipped
                        // to yellow in the curve ("acidic light", 2026-10-08), with dark panels in between (diffuse 0.45x).
                        let (metal, rough) = if std::env::var("FH1_RM_PAINT_SHEEN").is_ok_and(|v| v == "0") {
                            (0.55, 0.38)
                        } else {
                            (env_f32("FH1_RM_PAINT_METALLIC", 0.3), env_f32("FH1_RM_PAINT_METAL_ROUGH", 0.55))
                        };
                        base.metallic = if metallic { metal } else { 0.0 };
                        base.perceptual_roughness = if metallic { rough } else { 0.42 };
                        base.reflectance = 0.5;
                        base.clearcoat = 1.0;
                        base.clearcoat_perceptual_roughness = 0.03;
                        // Paint albedo scale. c3 (2026-10-06) measured the raw Combo_Colors colour ~0.6 EV too bright under
                        // the TrackSettings-shadow-azimuth sun; with the TOD sun + frame EV (2026-10-07) the raw colour
                        // matches faithful on sunlit panels (roof 0.82x, hood 0.90x vs 0.64x / 0.78x at 0.6).
                        // FH1_RM_PAINT_ALBEDO=0.6 = old.
                        colour = colour * env_f32("FH1_RM_PAINT_ALBEDO", 1.0);
                        if matte {
                            base.metallic = 0.0;
                            base.perceptual_roughness = 0.7;
                            base.clearcoat = 0.0;
                        }
                        // FH1_RM_PAINT_DEBUG=mirror: a white mirror body, to see what the env light gives the car.
                        if std::env::var("FH1_RM_PAINT_DEBUG").is_ok_and(|v| v == "mirror") {
                            base.metallic = 1.0;
                            base.perceptual_roughness = 0.02;
                            colour = LinearRgba::rgb(0.95, 0.95, 0.95);
                        }
                    }
                    let ext = CarPaint {
                        colour,
                        flake: if metallic && !matte { flake } else { 0.0 },
                        flake_scale: 2048.0,
                        fill_top: fill.top,
                        fill_bottom: fill.bottom,
                    };
                    paints.add(CarPaintMaterial { base, extension: ext })
                })
                .clone();
            commands.entity(e).remove::<MeshMaterial3d<StandardMaterial>>().insert(MeshMaterial3d(handle));
            continue;
        }
        match lk.kind {
            Kind::Lamp(_) | Kind::Lens(Some(_)) | Kind::Cover(Some(_)) => {
                let Ok(mut set) = lamps.get_mut(body) else { continue };
                let (l, scale) = match lk.kind {
                    Kind::Lamp(l) => (l, 1.0),
                    Kind::Lens(Some(l)) => (l, 0.6),
                    Kind::Cover(Some(l)) => (l, 0.35),
                    _ => unreachable!(),
                };
                let lod = transp_lod_on();
                let (handle, far) = match set.lamps.get(&src_id) {
                    Some((_, _, h, f)) => (h.clone(), f.clone()),
                    None => {
                        let mut m = restyle(&src, lk);
                        scale_base(&mut m, paint_scale(&settings, &name.0));
                        let colour = lamp_colour(&settings, &name.0, l);
                        // The lights atlas is mostly grey: the game colours red/amber lamps and their covers by the lamp
                        // colour (lens pass 0 reads tex.r as the on/off mix, docs/WHEELS.md), so tint the unlit base too.
                        // FH1_RM_LAMP_TINT=0 = untinted atlas.
                        if matches!(l, Lamp::Tail | Lamp::Fog | Lamp::IndicatorLeft | Lamp::IndicatorRight) && !std::env::var("FH1_RM_LAMP_TINT").is_ok_and(|v| v == "0") {
                            let c = m.base_color.to_linear();
                            m.base_color = Color::LinearRgba(LinearRgba::new(c.red * colour.red, c.green * colour.green, c.blue * colour.blue, c.alpha));
                        }
                        m.emissive_texture = m.base_color_texture.clone();
                        m.emissive = LinearRgba::BLACK;
                        // Clear outer plastic over lenses and covers (module doc).
                        let coat = env_f32("FH1_RM_LAMP_COAT", 1.0);
                        if coat > 0.0 && matches!(lk.kind, Kind::Lens(_) | Kind::Cover(_)) {
                            m.clearcoat = coat.min(1.0);
                            m.clearcoat_perceptual_roughness = 0.04;
                        }
                        let far = (lod && blended(&m)).then(|| standard.add(far_twin(&m)));
                        let h = standard.add(m);
                        cache.made.insert(h.id());
                        if let Some(f) = &far {
                            cache.made.insert(f.id());
                        }
                        set.lamps.insert(src_id, (l, colour * scale, h.clone(), far.clone()));
                        set.levels.clear();
                        (h, far)
                    }
                };
                if let Some(far) = far {
                    commands.entity(e).insert(TranspLod { near: handle.clone(), far, far_on: false });
                }
                commands.entity(e).insert(MeshMaterial3d(handle));
            }
            _ => {
                let handle = match cache.standard.get(&src_id) {
                    Some(h) => h.clone(),
                    None => {
                        let mut m = restyle(&src, lk);
                        // Wheels keep their atlas as is (the rim techniques are the game's `_V2` ones; PaintScale greyed the rims).
                        let wheel = crate::wheel::look(&base_name(&name.0)).is_some();
                        if matches!(lk.kind, Kind::Surface | Kind::Lens(_) | Kind::Cover(_)) && src.base_color_texture.is_some() && !wheel {
                            scale_base(&mut m, paint_scale(&settings, &name.0));
                        }
                        if matches!(lk.kind, Kind::Glass | Kind::DarkGlass) {
                            if let Some(r) = glass_reflectance(&settings, &name.0) {
                                m.reflectance = r;
                            }
                        }
                        let far = (transp_lod_on() && blended(&m)).then(|| standard.add(far_twin(&m)));
                        let h = standard.add(m);
                        cache.made.insert(h.id());
                        cache.standard.insert(src_id, h.clone());
                        if let Some(f) = far {
                            cache.made.insert(f.id());
                            cache.far.insert(h.id(), f);
                        }
                        h
                    }
                };
                if let Some(far) = cache.far.get(&handle.id()) {
                    commands.entity(e).insert(TranspLod { near: handle.clone(), far: far.clone(), far_on: false });
                }
                commands.entity(e).insert(MeshMaterial3d(handle));
            }
        }
    }
}

// ---------------------------------------------------------------- Customize-menu variants (rims, body kits)
//
// fh1setup variants.rs (docs/CUSTOMIZE.md "Remaster export") writes, under <assets>/variants/:
// - wheels/<RIM>/model.gltf: the rim only, node `rim`, in rim model space (left wheel, axle along X, hub-centred), and
//   rim.json {model_rim_d, model_tyre_d, model_width} (m, the game's 82DA1CF8 measures);
// - cars/<CAR>/model.gltf: the car's NON-stock kit sections, one node per carbin section (bumperFb, skirtLc, ...), under a
//   root `<CAR>_kit` that already carries model.gltf's -BottomCenterWheelbasePos.
// The body's own scene keeps the stock parts; once it has spawned, the chosen variants replace them.

/// What a body's [`FxCarRim`] / [`FxCarKit`] ask for, resolved against the stock kit when the body is added.
#[derive(Component)]
struct VariantRequest {
    rim: Option<String>,
    /// (stem, stock letter, chosen letter) for every stem whose letter differs from stock.
    kit: Vec<(String, char, char)>,
}

/// On the variant kit scene: the kit nodes to keep (lower case `stem` + letter); every other kit node is hidden.
#[derive(Component)]
struct KitSelect(Vec<String>);

fn request_variants(mut commands: Commands, added: Query<(Entity, &FxCarBody, Option<&FxCarRim>, Option<&FxCarKit>), Added<FxCarBody>>) {
    for (e, body, rim, kit) in &added {
        let stock = StockKit::load(&body.assets, &body.car);
        let kit: Vec<(String, char, char)> = kit
            .map(|k| k.0 .0.iter().filter_map(|(s, l)| (stock.letter(s) != *l).then(|| (s.to_ascii_lowercase(), stock.letter(s), l.to_ascii_lowercase()))).collect())
            .unwrap_or_default();
        if rim.is_some() || !kit.is_empty() {
            commands.entity(e).insert(VariantRequest { rim: rim.map(|r| r.0.clone()), kit });
        }
    }
}

/// The letter standing for a `<stem>race` section (fh1setup variants.rs RACE; Customize's race rows).
pub const RACE: char = '#';

/// Section name of `stem` + `letter` (`wingrace` for [`RACE`]).
fn kit_key(stem: &str, letter: char) -> String {
    if letter == RACE { format!("{stem}race") } else { format!("{stem}{letter}") }
}

/// Is `name` the kit section `stem` + `letter` (optionally `_tail` or another `_` suffix)?
fn kit_node(name: &str, stem: &str, letter: char) -> bool {
    let n = name.to_ascii_lowercase();
    n.strip_prefix(&kit_key(stem, letter)).is_some_and(|r| r.is_empty() || r.starts_with('_'))
}

/// When a body's scene has spawned: swap its rims and kit parts; when a variant kit scene has spawned: keep only the
/// chosen sections.
#[allow(clippy::too_many_arguments)]
fn apply_variants(
    ev: On<WorldInstanceReady>,
    requests: Query<(&VariantRequest, &FxCarBody)>,
    selects: Query<&KitSelect>,
    children: Query<&Children>,
    names: Query<&Name>,
    mesh_names: Query<&GltfMeshName>,
    assets: Res<AssetServer>,
    mut commands: Commands,
) {
    let root = ev.entity;
    if let Ok(KitSelect(keep)) = selects.get(root) {
        for d in children.iter_descendants(root) {
            let Ok(n) = names.get(d) else { continue };
            let n = n.as_str().to_ascii_lowercase();
            if n.ends_with("_kit") {
                continue;
            }
            if !keep.iter().any(|k| n == *k || n.strip_prefix(k.as_str()).is_some_and(|r| r.starts_with('_'))) {
                commands.entity(d).insert(Visibility::Hidden);
            }
        }
        return;
    }
    let Ok((req, body)) = requests.get(root) else { return };
    let json = |p: std::path::PathBuf| -> Option<serde_json::Value> { fh1_render::files::read_json(&p) };
    let car_name = Path::new(&body.car).file_name().and_then(|s| s.to_str()).unwrap_or(&body.car).to_owned();
    if let Some(rim) = &req.rim {
        let measures = json(body.assets.join("variants/wheels").join(rim).join("rim.json"));
        let model = json(body.assets.join("cars").join(&body.car).join("model.json"));
        match (measures, model) {
            (Some(m), Some(model)) => {
                let f = |v: &serde_json::Value, k: &str| v[k].as_f64().unwrap_or(0.0) as f32;
                let (mw, md) = (f(&m, "model_width").max(1e-3), f(&m, "model_rim_d").max(1e-3));
                let path = format!("variants/wheels/{rim}/model.gltf");
                for d in children.iter_descendants(root) {
                    let Some(corner) = names.get(d).ok().and_then(|n| n.as_str().strip_prefix("wheel_")) else { continue };
                    let axle = &model["axles"][if corner.ends_with('F') { 0 } else { 1 }];
                    // The game's wheelScale fit (fit_to): width to the tyre width, diameter to the rim diameter.
                    let s = Vec3::new(f(axle, "tyre_width") / mw, 2.0 * f(axle, "rim_radius") / md, 2.0 * f(axle, "rim_radius") / md);
                    for p in children.iter_descendants(d) {
                        if mesh_names.get(p).is_ok_and(|m| m.0.starts_with("rim_")) {
                            commands.entity(p).insert(Visibility::Hidden);
                        }
                    }
                    commands.spawn((WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(path.clone()))), Transform::from_scale(s), Visibility::default(), ChildOf(d)));
                }
            }
            _ => warn!("{}: rim {rim}: no variants/wheels/{rim}/rim.json (run fh1setup variants); stock rim kept", body.car),
        }
    }
    if !req.kit.is_empty() {
        let rel = format!("variants/cars/{car_name}/model.gltf");
        if body.assets.join(&rel).exists() {
            // Only the sections this car has (variants-2 kit.json; an older export without it: all requested). A missing
            // one keeps the stock part rather than leaving a hole.
            let have: Option<Vec<String>> = json(body.assets.join("variants/cars").join(&car_name).join("kit.json"))
                .and_then(|k| k["sections"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()));
            let kit: Vec<&(String, char, char)> = req.kit.iter().filter(|(stem, _, l)| have.as_ref().is_none_or(|h| h.iter().any(|n| kit_node(n, stem, *l)))).collect();
            for d in children.iter_descendants(root) {
                let Ok(n) = names.get(d) else { continue };
                if kit.iter().any(|(stem, stock, _)| kit_node(n.as_str(), stem, *stock)) {
                    commands.entity(d).insert(Visibility::Hidden);
                }
            }
            let keep = kit.iter().map(|(stem, _, l)| kit_key(stem, *l)).collect();
            commands.spawn((WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(rel))), KitSelect(keep), Transform::default(), Visibility::default(), ChildOf(root)));
        } else {
            warn!("{}: no {rel} (run fh1setup variants); stock kit kept", body.car);
        }
    }
}

/// A StandardMaterial for every look but paint.
fn restyle(src: &StandardMaterial, lk: Look) -> StandardMaterial {
    let mut m = src.clone();
    m.metallic = lk.metallic;
    m.perceptual_roughness = lk.roughness;
    m.reflectance = lk.reflectance;
    m.clearcoat = lk.clearcoat;
    m.clearcoat_perceptual_roughness = lk.clearcoat_roughness;
    m.metallic_roughness_texture = None;
    match lk.kind {
        Kind::Chrome => {
            m.base_color_texture = None;
            m.base_color = Color::srgb(0.93, 0.93, 0.95);
        }
        Kind::Glass | Kind::DarkGlass => {
            // Premultiplied: the tint dims what is behind by alpha, the reflection stays whole.
            let c = src.base_color.to_linear();
            // The glTF opacity (window 0.45) over a dark, unlit-looking cabin read as near-opaque next to faithful's glass,
            // which shows the interior: scale it (FH1_RM_GLASS_ALPHA, 1 = the glTF value). Dark glass (black_glass: the
            // frit band round the windscreen, black tinted panes) keeps its 0.9: scaled to 0.5 it read as a see-through
            // gap between the window and the body (FH1_RM_BLACK_GLASS_ALPHA, 0.55 = old).
            let k = if lk.kind == Kind::DarkGlass { env_f32("FH1_RM_BLACK_GLASS_ALPHA", 1.0) } else { env_f32("FH1_RM_GLASS_ALPHA", 0.55) };
            let a = (c.alpha * k).clamp(0.1, 0.95);
            m.base_color = Color::LinearRgba(LinearRgba::new(c.red * a, c.green * a, c.blue * a, a));
            m.alpha_mode = AlphaMode::Premultiplied;
            // Back faces culled: cars whose panes are seen from both sides (roadster screens, classic racers) carry an
            // inner and an outer shell (LOT_Elise_99, JAG_DType_56, FER_250TestaRossa_57, SHE_Cobra427_65: 70-95 % of the
            // glass has an opposite-facing twin within 1.5 cm). Drawn two-sided, both shells tinted and reflected: a
            // frosted pane. FH1_RM_GLASS_CULL=0 = two-sided (old).
            if std::env::var("FH1_RM_GLASS_CULL").is_ok_and(|v| v == "0") {
                m.double_sided = true;
                m.cull_mode = None;
            } else {
                m.double_sided = false;
                m.cull_mode = Some(bevy::render::render_resource::Face::Back);
            }
        }
        Kind::Defrost if !std::env::var("FH1_RM_DEFROST_MASK").is_ok_and(|v| v == "0") => {
            // The lines are the atlas alpha; drawn opaque, the whole sheet covered the rear window in the atlas colour
            // there (ALF_8C_08 / LEX_LFA_10 98-100 % alpha 0; FH1_RM_DEFROST_MASK=0 = opaque, old).
            m.alpha_mode = AlphaMode::Mask(0.5);
        }
        Kind::Cover(_) => {
            // The game's covers blend by the lights atlas alpha (lights_glass: GlassMaxOpacity 0.8).
            m.alpha_mode = AlphaMode::Blend;
        }
        _ => {}
    }
    // Lamp layers blend by the lights atlas alpha (most lamp texels are 0.1-0.9), drawn back to front like the game's carbin
    // order (fh1-render car.rs lamp_sort_bias: lamp layers 0.2, lenses and covers 0.3 on top). Opaque, every lamp shell
    // showed its whole quad and glowed at full strength behind and around the covers. depth_bias < 1 only moves the blended
    // sort (it is an i32 for the GPU bias). FH1_RM_LAMP_BLEND=0 = opaque lamps and lenses (old).
    if lamp_blend() {
        match lk.kind {
            Kind::Lamp(_) => {
                m.alpha_mode = AlphaMode::Blend;
                m.depth_bias = 0.2;
            }
            Kind::Lens(_) | Kind::Cover(_) => {
                m.alpha_mode = AlphaMode::Blend;
                m.depth_bias = 0.3;
            }
            _ => {}
        }
    }
    m
}

fn lamp_blend() -> bool {
    !std::env::var("FH1_RM_LAMP_BLEND").is_ok_and(|v| v == "0")
}

/// Lamp emissive from the car's lamp state (on the `FxCarBody`'s parent).
fn update_lamps(
    mut bodies: Query<(&mut RemasterLamps, &ChildOf)>,
    cars: Query<(Option<&FxCarLamps>, Option<&FxHeadlightState>)>,
    mut standard: ResMut<Assets<StandardMaterial>>,
    lighting: Option<Res<crate::light::RemasterLighting>>,
    mut last_gain: Local<f32>,
) {
    // Physical lamp nits, capped at FH1_RM_LAMP_CAP x display white for the current exposure (module doc).
    let cap = env_f32("FH1_RM_LAMP_CAP", 3.0);
    let physical = env_f32("FH1_RM_LAMP", 2.0e4);
    let gain = match lighting.as_ref() {
        Some(l) if cap > 0.0 => physical.min(cap * 1.2 * 2f32.powf(l.ev100)),
        _ => physical,
    };
    // Exposure drift: rewrite every lamp when the gain moved by > 5 %.
    let regain = (gain - *last_gain).abs() > 0.05 * gain.max(1.0);
    if regain {
        *last_gain = gain;
    }
    for (mut set, parent) in &mut bodies {
        if set.lamps.is_empty() {
            continue;
        }
        let (l, h) = cars.get(parent.parent()).unwrap_or((None, None));
        let l = l.copied().unwrap_or_default();
        let heads = h.map_or(0.0, |h| h.amount);
        let level = |lamp: Lamp| match lamp {
            Lamp::Head => heads,
            Lamp::Tail => (0.3 * heads).max(l.brake),
            Lamp::Reverse => l.reverse,
            Lamp::IndicatorLeft => l.indicator_left,
            Lamp::IndicatorRight => l.indicator_right,
            Lamp::Fog => l.fog,
            Lamp::Marker => 0.4 * heads,
        };
        let changed = regain || set.lamps.values().any(|(lamp, ..)| set.levels.get(lamp) != Some(&level(*lamp)));
        if !changed {
            continue;
        }
        let RemasterLamps { lamps, levels } = &mut *set;
        for (lamp, colour, handle, far) in lamps.values() {
            let v = level(*lamp);
            levels.insert(*lamp, v);
            for h in std::iter::once(handle).chain(far.as_ref()) {
                if let Some(mut m) = standard.get_mut(h) {
                    m.emissive = *colour * (v * *last_gain);
                }
            }
        }
    }
}

/// Swaps blended car parts to their far twin beyond FH1_RM_CAR_TRANSP_LOD_M (module doc), 4x a second.
fn transparency_lod(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut next: Local<f32>,
    cams: Query<&GlobalTransform, With<fh1_render::post::FxPostCamera>>,
    mut parts: Query<(Entity, &GlobalTransform, &mut TranspLod)>,
) {
    let now = time.elapsed_secs();
    if now < *next {
        return;
    }
    *next = now + 0.25;
    let Some(cam) = cams.iter().next().map(|c| c.translation()) else { return };
    let at = env_f32("FH1_RM_CAR_TRANSP_LOD_M", 60.0);
    let (out2, in2) = ((at + 5.0).powi(2), (at - 5.0).max(0.0).powi(2));
    for (e, t, mut lod) in &mut parts {
        let d2 = t.translation().distance_squared(cam);
        let far = if lod.far_on { d2 > in2 } else { d2 > out2 };
        if far != lod.far_on {
            lod.far_on = far;
            commands.entity(e).insert(MeshMaterial3d(if far { lod.far.clone() } else { lod.near.clone() }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_turn10_names() {
        assert_eq!(look("body").kind, Kind::Paint);
        assert_eq!(look("body_2").kind, Kind::Paint);
        assert_eq!(look("lights_gls_taillight2S").kind, Kind::Cover(Some(Lamp::Tail)));
        assert_eq!(look("lights_gls_noemit").kind, Kind::Cover(None));
        assert_eq!(look("taillight2S").kind, Kind::Lamp(Lamp::Tail));
        assert_eq!(look("reverse_light_lod0").kind, Kind::Lamp(Lamp::Reverse));
        assert_eq!(look("xenonhead").kind, Kind::Lamp(Lamp::Head));
        assert_eq!(look("indicator_left").kind, Kind::Lamp(Lamp::IndicatorLeft));
        assert_eq!(look("slinrorange").kind, Kind::Lamp(Lamp::IndicatorRight));
        assert_eq!(look("slorange").kind, Kind::Lamp(Lamp::Marker));
        assert_eq!(look("fogred").kind, Kind::Lamp(Lamp::Fog));
        assert_eq!(look("detail_glass_red").kind, Kind::Lens(Some(Lamp::Tail)));
        assert_eq!(look("window").kind, Kind::Glass);
        assert_eq!(look("black_glass_breakable").kind, Kind::DarkGlass);
        assert_eq!(look("defrost_lines").kind, Kind::Defrost);
        assert!(kit_node("bumperFb", "bumperf", 'b'));
        assert!(kit_node("bumperFb_tail", "bumperf", 'b'));
        assert!(!kit_node("bumperFbx", "bumperf", 'b'));
        assert!(!kit_node("bumperRb", "bumperf", 'b'));
        assert!(kit_node("wingRace", "wing", RACE));
        assert!(kit_node("bumperFrace_tail", "bumperf", RACE));
        assert!(!kit_node("wingRace", "wing", 'r'));
        assert_eq!(look("chrome_2").kind, Kind::Chrome);
        assert_eq!(look("misc_NoOcclude_NoShadow").kind, Kind::Surface);
        assert_eq!(look("headlight_paint").kind, Kind::Paint);
        assert_eq!(look("CBase1_CLR_VARI_0").clearcoat, 1.0);
        assert_eq!(look("textured_reflector").kind, Kind::Surface);
        assert_eq!(look("rim").metallic, 0.25);
    }
}
