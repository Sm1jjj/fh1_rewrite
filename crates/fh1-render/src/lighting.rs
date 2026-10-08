//! Time of day → the game's global shader parameters.
//!
//! Formulas reverse-engineered from default.xex (2026-10-03; addresses in docs/SHADERS.md,
//! "Lighting constants"). VERIFIED = read from the game's code.

use bevy::math::{Vec3, Vec4};

use crate::tod::TimeOfDay;
use crate::FxGlobals;

/// XML (left-handed, +Z north) → engine space.
fn mirror_z(v: [f32; 3]) -> Vec3 {
    Vec3::new(v[0], v[1], -v[2])
}

/// A `Fog_Templates.xml` entry (dynamicpost.zip `Tracks/<track>/Fog_Templates.xml`). The game
/// multiplies each TOD fog channel by these. Without a template: all 1, distances 2000 (VERIFIED).
#[derive(Debug, Clone, Copy)]
pub struct FogTemplate {
    pub color: Vec3,
    pub density: f32,
    pub start_distance: f32,
    pub sun_scatter_color: Vec3,
    pub sun_scatter_power: f32,
    pub height_fade_amount: f32,
    pub height_fade_start: f32,
    pub height_fade_end: f32,
    pub sun_scatter_start_distance: f32,
    pub sun_scatter_rampup_distance: f32,
}

impl Default for FogTemplate {
    fn default() -> Self {
        Self {
            color: Vec3::ONE,
            density: 1.0,
            start_distance: 1.0,
            sun_scatter_color: Vec3::ONE,
            sun_scatter_power: 1.0,
            height_fade_amount: 1.0,
            height_fade_start: 1.0,
            height_fade_end: 1.0,
            sun_scatter_start_distance: 2000.0,
            sun_scatter_rampup_distance: 2000.0,
        }
    }
}

impl FogTemplate {
    /// Field-wise blend, as the zone change lerps every template (LerpTime; INFERRED like bloom/colour).
    pub fn lerp(&self, b: &FogTemplate, t: f32) -> FogTemplate {
        let f = |x: f32, y: f32| x + (y - x) * t;
        FogTemplate {
            color: self.color.lerp(b.color, t),
            density: f(self.density, b.density),
            start_distance: f(self.start_distance, b.start_distance),
            sun_scatter_color: self.sun_scatter_color.lerp(b.sun_scatter_color, t),
            sun_scatter_power: f(self.sun_scatter_power, b.sun_scatter_power),
            height_fade_amount: f(self.height_fade_amount, b.height_fade_amount),
            height_fade_start: f(self.height_fade_start, b.height_fade_start),
            height_fade_end: f(self.height_fade_end, b.height_fade_end),
            sun_scatter_start_distance: f(self.sun_scatter_start_distance, b.sun_scatter_start_distance),
            sun_scatter_rampup_distance: f(self.sun_scatter_rampup_distance, b.sun_scatter_rampup_distance),
        }
    }

    /// Parse every `<Template Name=..>` of a Fog_Templates.xml.
    pub fn parse_all(xml: &str) -> Vec<(String, FogTemplate)> {
        let mut out = Vec::new();
        for raw in xml.split("<Template ").skip(1) {
            let tag = raw.split('>').next().unwrap_or("");
            let get = |n: &str| -> Option<String> {
                let pat = format!("{n}=\"");
                let s = tag.find(&pat)? + pat.len();
                Some(tag[s..s + tag[s..].find('"')?].to_string())
            };
            let f = |n: &str, d: f32| get(n).and_then(|v| v.trim().parse().ok()).unwrap_or(d);
            let t = FogTemplate {
                color: Vec3::new(f("ColorR", 1.0), f("ColorG", 1.0), f("ColorB", 1.0)),
                density: f("Density", 1.0),
                start_distance: f("StartDistance", 1.0),
                sun_scatter_color: Vec3::new(f("SunScatterColorR", 1.0), f("SunScatterColorG", 1.0), f("SunScatterColorB", 1.0)),
                sun_scatter_power: f("SunScatterPower", 1.0),
                height_fade_amount: f("HeightFadeAmount", 1.0),
                height_fade_start: f("HeightFadeStart", 1.0),
                height_fade_end: f("HeightFadeEnd", 1.0),
                sun_scatter_start_distance: f("SunScatterStartDistance", 2000.0),
                sun_scatter_rampup_distance: f("SunScatterRampupDistance", 2000.0),
            };
            out.push((get("Name").unwrap_or_default(), t));
        }
        out
    }
}

/// The track's Fog_Templates.xml entries by name; `follow_zones` = false under `FH1_FOG_TEMPLATE`.
#[derive(bevy::prelude::Resource, Default)]
pub struct FogTemplates {
    pub by_name: std::collections::HashMap<String, FogTemplate>,
    pub follow_zones: bool,
}

impl FogTemplates {
    /// The fog template for a zone blend: a zone without a (known) template = the default (×1, 2000 m).
    pub fn blend(&self, prev: Option<&str>, cur: Option<&str>, t: f32) -> FogTemplate {
        let get = |n: Option<&str>| n.and_then(|n| self.by_name.get(n)).copied().unwrap_or_default();
        get(prev).lerp(&get(cur), t)
    }
}

/// The `TrackSettings.xml` values that feed shader constants.
#[derive(Debug, Clone, Copy)]
pub struct TrackSettings {
    pub dark_color: Vec4,
    pub light_color: Vec4,
    pub global_specular_level: f32,
    pub min_spec_level: f32,
    pub sky_gain: f32,
    pub sky_scale: f32,
    pub cube_sky_saturation: f32,
    pub cube_sky_scale: f32,
    pub cube_refl_ground_scale: f32,
    /// `<SHLighting>` (parsed by the game into TrackSettings +0x470..0x4ac, VERIFIED offsets).
    pub sh_top: Vec3,
    pub sh_bottom: Vec3,
    pub sh_dir: Vec3,
    pub sh_env_top: f32,
    pub sh_env_bottom: f32,
}

impl Default for TrackSettings {
    fn default() -> Self {
        Self {
            dark_color: Vec4::new(0.0, 0.0, 0.0, 1.0),
            light_color: Vec4::ONE,
            global_specular_level: 8.0,
            min_spec_level: 0.0,
            sky_gain: 0.0,
            sky_scale: 1.0,
            cube_sky_saturation: 0.6,
            cube_sky_scale: 0.0,
            cube_refl_ground_scale: 1.0,
            sh_top: Vec3::ZERO,
            sh_bottom: Vec3::ZERO,
            sh_dir: Vec3::ONE,
            sh_env_top: 1.0,
            sh_env_bottom: 0.15,
        }
    }
}

impl TrackSettings {
    pub fn parse(xml: &str) -> Self {
        let tag = |name: &str| -> Option<&str> {
            let s = xml.find(&format!("<{name} "))?;
            Some(&xml[s..s + xml[s..].find('>')?])
        };
        let attr = |name: &str, a: &str| -> Option<f32> {
            let t = tag(name)?;
            let pat = format!(" {a}=\"");
            let s = t.find(&pat)? + pat.len();
            t[s..s + t[s..].find('"')?].trim().parse().ok()
        };
        let colour = |name: &str, d: Vec4| -> Vec4 {
            match (attr(name, "r"), attr(name, "g"), attr(name, "b"), attr(name, "a")) {
                (Some(r), Some(g), Some(b), a) => Vec4::new(r, g, b, a.unwrap_or(1.0)),
                _ => d,
            }
        };
        let sh3 = |p: &str| -> Option<Vec3> {
            Some(Vec3::new(attr("SHLighting", &format!("{p}Red"))?, attr("SHLighting", &format!("{p}Green"))?, attr("SHLighting", &format!("{p}Blue"))?))
        };
        let d = Self::default();
        Self {
            dark_color: colour("DarkColor", d.dark_color),
            light_color: colour("LightColor", d.light_color),
            global_specular_level: attr("GlobalSpecularLevel", "value").unwrap_or(d.global_specular_level),
            min_spec_level: attr("MinSpecLevel", "value").unwrap_or(d.min_spec_level),
            sky_gain: attr("skyGain", "value").unwrap_or(d.sky_gain),
            sky_scale: attr("skyScale", "value").unwrap_or(d.sky_scale),
            cube_sky_saturation: attr("cubeSkySaturation", "value").unwrap_or(d.cube_sky_saturation),
            cube_sky_scale: attr("cubeSkyScale", "value").unwrap_or(d.cube_sky_scale),
            cube_refl_ground_scale: attr("cubeReflGroundScale", "value").unwrap_or(d.cube_refl_ground_scale),
            sh_top: sh3("SHTop").unwrap_or(d.sh_top),
            sh_bottom: sh3("SHBottom").unwrap_or(d.sh_bottom),
            sh_dir: sh3("SHDir").unwrap_or(d.sh_dir),
            sh_env_top: attr("SHLighting", "SHEnvMapTop").unwrap_or(d.sh_env_top),
            sh_env_bottom: attr("SHLighting", "SHEnvMapBottom").unwrap_or(d.sh_env_bottom),
        }
    }
}

/// Track-level constants (VERIFIED sources: TrackSettings fields, upload 0x82413DA0).
pub fn apply_track_settings(g: &mut FxGlobals, t: &TrackSettings, timer: f32) {
    g.set_vec("V2LightmapColor1", t.dark_color);
    g.set_vec("V2LightmapColor2", t.light_color);
    g.set_vec("SkyScale", Vec4::new(t.sky_scale, t.cube_sky_saturation, t.cube_sky_scale, t.cube_refl_ground_scale));
    // TimeGain = (timer, skyGain, 0, GlobalSpecularLevel): VERIFIED against the game's own c249 at the festival
    // (RenderDoc frame 4543, 15:48: (76.8, 0, 0, 8); docs/GPU_CAPTURE.md). .z was a guessed 1 before.
    g.set_vec("TimeGain", Vec4::new(timer, t.sky_gain, 0.0, t.global_specular_level));
}

/// sunColor.w as the game sets it (c251.w = 20 in RenderDoc frame 4543; source untraced).
const SUN_COLOR_W: f32 = 20.0;

/// Set the lighting/fog globals for `seconds` (0..86400) of TimeOfDayA.
pub fn apply_time_of_day(g: &mut FxGlobals, tod: &TimeOfDay, seconds: f32, fog: &FogTemplate) {
    // The game multiplies key times by 60 at load and runs its clock in seconds; our curves keep
    // minutes.
    let m = seconds / 60.0;
    let v3 = |n: &str| Vec3::from_array(tod.get(n, m));
    let s = |n: &str| tod.scalar(n, m);

    // VERIFIED: sunDir = normalize(SunPos - SunTargetPos) (points at the sun), VS and PS c250.
    // TOD positions are in the game's left-handed space (INFERRED), hence the Z mirror.
    let sun = (mirror_z(tod.get("SunPos", m)) - mirror_z(tod.get("SunTargetPos", m))).normalize_or(Vec3::Y);
    g.set_vec("sunDir", sun.extend(0.0));
    // VERIFIED: sunColor = SunColor × SunColorMult × IBLDirectScaleEnvironment (rgb matches the game's own
    // c251 at 15:48 to 4 digits, RenderDoc frame 4543). .w = 20 in that capture (was 1 here); its source is
    // untraced (no TrackSettings field holds 20), so the captured constant is used.
    let sun_colour = v3("SunColor") * s("SunColorMult") * s("IBLDirectScaleEnvironment");
    g.set_vec("sunColor", sun_colour.extend(SUN_COLOR_W));
    // VERIFIED: hemisphere top/bottom straight from the curves; ambient = 0.7 top + 0.3 bottom.
    // (The TOD AmbientColour channel is never read.)
    let top = v3("HLTopColour");
    let bottom = v3("HLBottomColour");
    g.set_vec("skyColor", top.extend(1.0));
    g.set_vec("groundColor", bottom.extend(1.0));
    g.set_vec("ambColor", (top * 0.7 + bottom * 0.3).extend(1.0));

    // VERIFIED fog packing (TOD channel × fog template).
    g.set_vec("FogConsts", Vec4::new(s("FogStartDistance") * fog.start_distance, 0.0, 0.0, s("FogDensity") * fog.density * 0.01));
    g.set_vec("FogColor", (v3("FogColour") * fog.color).extend(fog.sun_scatter_start_distance));
    g.set_vec(
        "FogConsts2",
        Vec4::new(
            s("FogHeightFadeStart") * fog.height_fade_start,
            s("FogHeightFadeEnd") * fog.height_fade_end,
            s("FogHeightFadeAmount") * fog.height_fade_amount,
            s("FogSunScatterPower") * fog.sun_scatter_power,
        ),
    );
    g.set_vec("FogColor2", (v3("FogSunScatterColour") * fog.sun_scatter_color).extend(fog.sun_scatter_rampup_distance));

    // EmissiveSwitchOnThreshold c213: (L, saturate((L - 0.5) * 4) * k, 1, timer); L = SwitchOnLights
    // with values below 0.05 forced to 0 and 0 stored as -1 (VERIFIED, 0x825CC250; no hysteresis).
    // k = G+0x21E8 = 1 (ctor 0x82DD1C38; an untraced settings block can override it). The timer (.w,
    // seconds since the race countdown started; only countdown boards read it) is set per frame in
    // update_time_of_day (builder 0x82413DA0 @0x82414038, VERIFIED).
    let l = s("SwitchOnLights");
    let l = if l < 0.05 { -1.0 } else { l };
    g.set_vec("EmissiveSwitchOnThreshold", Vec4::new(l, ((l - 0.5) * 4.0).clamp(0.0, 1.0), 1.0, 0.0));
    g.set_bool("bEnableDeferredLightContribution", false);
    // DistanceFadeValues c157 (per object in the game): the *_fade_* track shaders output
    // alpha = tex.a * z * saturate((dist - x) / (y - x)), so x = fade end, y = fade start, z = 1
    // (VERIFIED from h_diff_mask_fade_1 VS/PS). Left at 0 the alpha test kills every pixel. The
    // engine switches prop LODs with VisibilityRange, so default to no fade (per-object fade
    // distances are untraced).
    g.set_vec("DistanceFadeValues", Vec4::new(1.0e7, 1.0e7 - 1.0, 1.0, 0.0));
}

/// Drives the global shader parameters from TimeOfDayA. Insert it to enable FH1 lighting.
#[derive(bevy::prelude::Resource)]
pub struct FxTimeOfDay {
    pub tod: TimeOfDay,
    /// Game clock in seconds (0..86400). The game starts at 57600 (16:00).
    pub seconds: f32,
    /// Clock speed: 0 = frozen; 1 = the game's rate (TimeSpeed curve: game seconds per real second).
    pub rate_scale: f32,
    pub fog: FogTemplate,
    pub track: TrackSettings,
}

impl FxTimeOfDay {
    /// Load `TimeOfDayA.xml` (the only set the game evaluates) at `minutes`, clock frozen.
    /// TrackSettings.xml is read from the same folder if present.
    pub fn load(path: &std::path::Path, minutes: f32) -> Option<Self> {
        let track = path
            .parent()
            .and_then(|d| std::fs::read_to_string(d.join("TrackSettings.xml")).ok())
            .map(|x| TrackSettings::parse(&x))
            .unwrap_or_default();
        Some(Self { tod: TimeOfDay::parse(&std::fs::read_to_string(path).ok()?), seconds: minutes * 60.0, rate_scale: 0.0, fog: FogTemplate::default(), track })
    }

    pub fn minutes(&self) -> f32 {
        self.seconds / 60.0
    }
}

/// The track's fog templates after an in-process map change ([`crate::postfx::FxTrackChanged`]), or when the time of
/// day arrived after Startup (main menu first: [`apply_tod_env`] ran before any map was loaded).
pub(crate) fn reload_fog_templates(
    mut commands: bevy::prelude::Commands,
    mut changed: bevy::prelude::MessageReader<crate::postfx::FxTrackChanged>,
    tod: Option<bevy::prelude::Res<FxTimeOfDay>>,
    templates: Option<bevy::prelude::Res<FogTemplates>>,
    config: Option<bevy::prelude::Res<crate::postfx::FxPostConfig>>,
) {
    let changed = changed.read().count() > 0;
    let Some(config) = config else { return };
    if !(changed || (tod.is_some() && templates.is_none())) {
        return;
    }
    if templates.as_ref().is_some_and(|t| !t.follow_zones) {
        return; // FH1_FOG_TEMPLATE pins one template.
    }
    let xml = std::fs::read_to_string(config.0.templates_dir.join("Fog_Templates.xml")).unwrap_or_default();
    commands.insert_resource(FogTemplates { by_name: FogTemplate::parse_all(&xml).into_iter().collect(), follow_zones: true });
}

/// Debug/autotest clock overrides (Startup): `FH1_TOD=hh:mm` sets the time, `FH1_TOD_SPEED=x` the
/// clock rate (1 = the game's TimeSpeed rate, 0 = frozen). The clock is frozen whenever `FH1_SHOT`
/// is set, so screenshots are repeatable.
pub(crate) fn apply_tod_env(
    mut commands: bevy::prelude::Commands,
    tod: Option<bevy::prelude::ResMut<FxTimeOfDay>>,
    config: Option<bevy::prelude::Res<crate::postfx::FxPostConfig>>,
) {
    let Some(mut t) = tod else { return };
    // Fog templates (zones pick them by name). FH1_FOG_TEMPLATE=<name> pins one everywhere (debug).
    let xml = config.map(|c| std::fs::read_to_string(c.0.templates_dir.join("Fog_Templates.xml")).unwrap_or_default()).unwrap_or_default();
    let mut templates = FogTemplates { by_name: FogTemplate::parse_all(&xml).into_iter().collect(), follow_zones: true };
    if let Ok(name) = std::env::var("FH1_FOG_TEMPLATE") {
        match templates.by_name.get(&name) {
            Some(f) => {
                t.fog = *f;
                templates.follow_zones = false;
            }
            None => bevy::log::warn!("FH1_FOG_TEMPLATE: no template {name}"),
        }
    }
    commands.insert_resource(templates);
    if let Ok(v) = std::env::var("FH1_TOD") {
        let mut it = v.split(':').map(|x| x.trim().parse::<f32>().ok());
        if let (Some(Some(h)), m) = (it.next(), it.next()) {
            t.seconds = ((h * 60.0 + m.flatten().unwrap_or(0.0)) * 60.0).rem_euclid(86_400.0);
        }
    }
    if let Some(s) = std::env::var("FH1_TOD_SPEED").ok().and_then(|v| v.trim().parse().ok()) {
        t.rate_scale = s;
    }
    if std::env::var_os("FH1_SHOT").is_some() {
        t.rate_scale = 0.0;
    }
}

/// Bevy's own lights follow the time of day: they light everything still drawn with StandardMaterial
/// (the glTF car without FH1_CARFX, STANDIN/fallback scenery), which otherwise stays at noon brightness
/// all night. The game lights its surfaces as albedo × (sat(N·L) × sunColor × shadow + ambColor)
/// (VERIFIED, H_DIFF_1; no 1/π), with sunColor/ambColor as in [`apply_time_of_day`]. Bevy's Lambert
/// is albedo/π × illuminance and its ambient albedo × brightness, both × the camera exposure, so the
/// lights are set to illuminance = π·sunColor / exposure and brightness = ambColor / exposure, with
/// the TOD colours (INFERRED bridge; the game has no such lights). The engine's starting values
/// (9,000 lx, 600) had about 3× the game's noon light, which showed at dusk as an ambient-lit car.
/// `FH1_BEVY_LIGHTS=relative` keeps the old behaviour (starting values = noon, scaled by luminance).
/// The light's direction and shadow settings belong to shadow.rs.
pub(crate) fn update_bevy_lights(
    tod: Option<bevy::prelude::Res<FxTimeOfDay>>,
    mut lights: bevy::prelude::Query<&mut bevy::prelude::DirectionalLight>,
    ambient: Option<bevy::prelude::ResMut<bevy::prelude::GlobalAmbientLight>>,
    cams: bevy::prelude::Query<Option<&bevy::camera::Exposure>, bevy::prelude::With<crate::post::FxPostCamera>>,
    mut base: bevy::prelude::Local<Option<(f32, f32)>>,
) {
    use bevy::prelude::Color;
    let Some(t) = tod else { return };
    let lum = |v: Vec3| v.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    let at = |m: f32| {
        let v3 = |n: &str| Vec3::from_array(t.tod.get(n, m));
        let sun = v3("SunColor") * t.tod.scalar("SunColorMult", m) * t.tod.scalar("IBLDirectScaleEnvironment", m);
        (sun, v3("HLTopColour") * 0.7 + v3("HLBottomColour") * 0.3)
    };
    let (sun, amb) = at(t.minutes());
    // A colour split into (normalised colour, its largest component).
    let split = |v: Vec3| {
        let m = v.max_element().max(1e-6);
        (Color::linear_rgb(v.x / m, v.y / m, v.z / m), v.max_element().max(0.0))
    };
    let (sun_colour, sun_k) = split(sun);
    let (amb_colour, amb_k) = split(amb);
    let (want_sun, want_amb) = if std::env::var("FH1_BEVY_LIGHTS").is_ok_and(|v| v == "relative") {
        if base.is_none() {
            let Some(illum) = lights.iter().next().map(|l| l.illuminance) else { return };
            *base = Some((illum, ambient.as_ref().map_or(0.0, |a| a.brightness)));
        }
        let Some((illum, bright)) = *base else { return };
        let (noon_sun, noon_amb) = at(720.0);
        (illum * lum(sun) / lum(noon_sun).max(1e-6), bright * lum(amb) / lum(noon_amb).max(1e-6))
    } else {
        let exposure = cams.iter().next().flatten().copied().unwrap_or_default().exposure();
        (std::f32::consts::PI * sun_k / exposure, amb_k / exposure)
    };
    for mut l in &mut lights {
        if (l.illuminance - want_sun).abs() > 1e-3 || l.color != sun_colour {
            l.illuminance = want_sun;
            l.color = sun_colour;
        }
    }
    if let Some(mut a) = ambient {
        if (a.brightness - want_amb).abs() > 1e-3 || a.color != amb_colour {
            a.brightness = want_amb;
            a.color = amb_colour;
        }
    }
}

pub(crate) fn update_time_of_day(
    tod: Option<bevy::prelude::ResMut<FxTimeOfDay>>,
    time: bevy::prelude::Res<bevy::prelude::Time>,
    mut globals: bevy::prelude::ResMut<FxGlobals>,
    mut last: bevy::prelude::Local<Option<f32>>,
    templates: Option<bevy::prelude::Res<FogTemplates>>,
    post: Option<bevy::prelude::Res<crate::postfx::FxPost>>,
    mut fog_zone: bevy::prelude::Local<Option<Option<String>>>,
) {
    let Some(mut t) = tod else { return };
    // The zone's Fog_Templates.xml entry, blended over the zone change like the other post templates
    // (packer 0x823F97E8; zone from PostProcessingZones_Safe.xml `Fog=`). Zone state is last frame's.
    if let (Some(f), Some(p)) = (templates.as_ref().filter(|f| f.follow_zones), post) {
        let (prev, cur, k) = p.zone_blend();
        let name = cur.map(|z| z.fog.clone());
        if *fog_zone != Some(name.clone()) {
            bevy::log::info!("fog template: {} (zone {})", name.as_deref().unwrap_or("none"), cur.map_or("none", |z| z.name.as_str()));
            *fog_zone = Some(name);
        }
        t.fog = f.blend(prev.map(|z| z.fog.as_str()), cur.map(|z| z.fog.as_str()), k);
    }
    if t.rate_scale != 0.0 {
        // VERIFIED: t += TimeSpeed(t) * dt, wrapping at 86400.
        let rate = t.tod.scalar("TimeSpeed", t.seconds / 60.0);
        t.seconds = (t.seconds + rate * t.rate_scale * time.delta_secs()).rem_euclid(86_400.0);
    }
    // TimeGain.x is a running timer, so this updates every frame.
    let _ = &mut *last;
    *last = Some(t.seconds);
    let fog = t.fog;
    apply_time_of_day(&mut globals, &t.tod, t.seconds, &fog);
    // Both clocks += dt and wrap at 8 h (0x82461D80 / 0x82461988; VERIFIED): TimeGain.x from 0, the
    // countdown timer from 3600 (the game resets it to 0 when a race countdown starts; no races yet).
    let e = time.elapsed_secs_f64();
    apply_track_settings(&mut globals, &t.track, (e % 28_800.0) as f32);
    if let Some(v) = globals.get("EmissiveSwitchOnThreshold") {
        globals.set_vec("EmissiveSwitchOnThreshold", v.with_w(((3_600.0 + e) % 28_800.0) as f32));
    }
}

// ---------------------------------------------------------------- car lighting

/// Marks the car whose orientation the car SH lighting is rotated into (FxCarGlobals is shared,
/// so only one car is lit exactly; others get its object-space SH).
#[derive(bevy::prelude::Component, Default)]
pub struct FxCarLit;

/// D3DX order-2 SH basis (D3DXSHEvalDirection): (Y00, Y1-1, Y10, Y11) = (c0, -c1·y, c1·z, -c1·x).
/// The car SH0 vertex transfer uses this basis in raw mesh coordinates (VERIFIED by data, see
/// program.rs `fx_sh0`).
fn sh_basis(d: Vec3) -> Vec4 {
    const C0: f32 = 0.282_094_8;
    const C1: f32 = 0.488_602_5;
    Vec4::new(C0, -C1 * d.y, C1 * d.z, -C1 * d.x)
}

/// SH projection of a hemisphere light: `top` over the half-space towards `up`, `bottom` below.
/// With the game's transfer (stored as projection / π) an unoccluded surface facing `up` gets
/// exactly `top`.
fn sh_hemisphere(up: Vec3, top: f32, bottom: f32) -> Vec4 {
    let b = sh_basis(up);
    Vec4::new(2.0 * std::f32::consts::PI * 0.282_094_8 * (top + bottom), 0.0, 0.0, 0.0)
        + Vec4::new(0.0, b.y, b.z, b.w) * std::f32::consts::PI * (top - bottom)
}

/// SH of a directional light, normalised so an unoccluded surface facing it gets 1: the
/// order-2 transfer (projection / π) dotted with sh_basis(d) at n = d gives 0.2387 = 3/(4π).
fn sh_directional(d: Vec3) -> Vec4 {
    sh_basis(d) * (4.0 * std::f32::consts::PI / 3.0)
}

/// Car shader globals from the track's lighting (call after `apply_time_of_day`).
///
/// VERIFIED (default.xex 0x82435130): c36..c40 = SHRotate(order 2, transpose(world), view SH) for
/// shDirectionalLightI, shHemisphericalLightR/G/B, shCubeMapDampnerLight; the VS dots each with the
/// vertex transfer. shDirectionalColour(VS) = (rgb, luminance). farClipPlane_BGReflScale = (far, 1).
/// INFERRED: who fills the view SH is untraced, so the hemisphere uses the time-of-day sky/ground
/// colours (same as the scenery's ambient), the directional SH is the unit sun, the dampener uses
/// TrackSettings SHEnvMapTop/Bottom; luminance weights are Rec.709.
pub fn apply_car_lighting(car: &mut FxGlobals, track: &FxGlobals, settings: &TrackSettings, world_to_object: bevy::math::Quat, far: f32) {
    let get = |n: &str, d: Vec4| track.get(n).unwrap_or(d);
    let sun = get("sunDir", Vec4::new(0.4, 0.8, 0.3, 0.0)).truncate().normalize_or(Vec3::Y);
    let sun_colour = get("sunColor", Vec4::ONE).truncate();
    let top = get("skyColor", Vec4::splat(0.3)).truncate();
    let bottom = get("groundColor", Vec4::splat(0.1)).truncate();
    let sun_o = world_to_object * sun;
    let up_o = world_to_object * Vec3::Y;
    car.set_vec("shDirectionalLightI", sh_directional(sun_o));
    car.set_vec("shHemisphericalLightR", sh_hemisphere(up_o, top.x, bottom.x));
    car.set_vec("shHemisphericalLightG", sh_hemisphere(up_o, top.y, bottom.y));
    car.set_vec("shHemisphericalLightB", sh_hemisphere(up_o, top.z, bottom.z));
    car.set_vec("shCubeMapDampnerLight", sh_hemisphere(up_o, settings.sh_env_top, settings.sh_env_bottom));
    let lum = sun_colour.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    for n in ["shDirectionalColour", "shDirectionalColourVS"] {
        car.set_vec(n, sun_colour.extend(lum));
    }
    for n in ["psLightDirWS", "vsLightDirWS"] {
        car.set_vec(n, sun.extend(0.0));
    }
    car.set_vec("farClipPlane_BGReflScale", Vec4::new(far, 1.0, 0.0, 0.0));
    for n in ["FogConsts", "FogColor", "FogConsts2", "FogColor2", "psFogColor"] {
        if let Some(v) = track.get(n) {
            car.set_vec(n, v);
        }
    }
}

pub(crate) fn update_car_lighting(
    tod: Option<bevy::prelude::Res<FxTimeOfDay>>,
    track: bevy::prelude::Res<FxGlobals>,
    mut car: bevy::prelude::ResMut<crate::FxCarGlobals>,
    lit: bevy::prelude::Query<&bevy::prelude::GlobalTransform, bevy::prelude::With<FxCarLit>>,
    cams: bevy::prelude::Query<&bevy::prelude::Projection, bevy::prelude::With<crate::post::FxPostCamera>>,
) {
    let settings = tod.as_ref().map(|t| t.track.clone()).unwrap_or_default();
    let rot = lit.iter().next().map_or(bevy::math::Quat::IDENTITY, |t| t.compute_transform().rotation);
    let far = cams.iter().find_map(|p| if let bevy::prelude::Projection::Perspective(p) = p { Some(p.far) } else { None }).unwrap_or(1000.0);
    apply_car_lighting(&mut car.0, &track, &settings, rot.inverse(), far);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fog_template_blend() {
        let xml = r#"<Templates><Template Name="NoFog" Density="1.0" SunScatterStartDistance="5000.0" SunScatterRampupDistance="40000.0"/>
            <Template Name="Redrock" ColorR="0.84" Density="2.35"/></Templates>"#;
        let f = FogTemplates { by_name: FogTemplate::parse_all(xml).into_iter().collect(), follow_zones: true };
        let a = f.blend(None, Some("NoFog"), 1.0);
        assert_eq!((a.sun_scatter_start_distance, a.sun_scatter_rampup_distance), (5000.0, 40000.0));
        let b = f.blend(Some("NoFog"), Some("Redrock"), 0.5);
        assert!((b.density - 1.675).abs() < 1e-5 && (b.color.x - 0.92).abs() < 1e-5);
        // No zone / unknown name = the default template (×1, 2000 m).
        assert_eq!(f.blend(None, Some("Nope"), 1.0).sun_scatter_start_distance, 2000.0);
    }
}
