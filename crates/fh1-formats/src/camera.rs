//! `media/camera.zip`: the game's camera tuning.
//!
//! - `CameraSettings.ini`: flat `Section\Sub\Key value` lines (FollowLowCam / FollowHighCam / HoodCam / BumperHighCam /
//!   Driver / PhotoModeCam / FreeCam ...). [`Settings`] keeps every key.
//! - `CameraPhysics.xml` (+ `CameraPhysicsSansEffects.xml`, the reduced stack): per gameplay camera
//!   (`FollowCam`, `FollowCam2`, `DriverCam`, `Hood`, `BumperHigh`) a list of effect [`Layer`]s: an input signal (speed,
//!   g-forces, slip, collisions ...) mapped through a curve (optionally times noise) onto an output (car/camera space
//!   offset or impulse, FOV, tone, vignette), optionally through a spring.
//! - `CarRelativeCams.xml`: the replay/TV car-relative cameras ([`CarRelativeCam`], `Group` = CameraGroups id 4-18), each
//!   an animated key path plus its own optional layer stack.
//! - `CameraGroups.xml`: id -> string-table name ([`Group`]).
//!
//! Per-car offsets for the gameplay cameras live in gamedb `CameraOverrides` (keyed by CarId), not here.
//! Attribute meanings beyond the names are documented in docs/CAMERA.md.

use std::collections::BTreeMap;

/// `CameraSettings.ini`: `path value` pairs, path as written (backslash separated).
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub values: BTreeMap<String, f32>,
}

impl Settings {
    pub fn parse(text: &str) -> Settings {
        let mut values = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once(char::is_whitespace) else { continue };
            if let Ok(v) = val.trim().parse::<f32>() {
                values.insert(key.to_string(), v);
            }
        }
        Settings { values }
    }

    /// `get("FollowLowCam\\FOV")`.
    pub fn get(&self, key: &str) -> Option<f32> {
        self.values.get(key).copied()
    }

    pub fn get_or(&self, key: &str, default: f32) -> f32 {
        self.get(key).unwrap_or(default)
    }
}

/// `InputToParamMapping` / `InputToNoiseFreqMapping`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// No curve: the (clamped) input itself.
    Linear,
    ConstantOne,
    /// Piecewise curve through 3 or 5 points ([`Curve::points`]).
    ThreePoint,
    FivePoint,
    Other,
}

#[derive(Debug, Clone)]
pub struct Curve {
    pub mapping: Mapping,
    /// (input, output) points, as stored (3 for ThreePoint, 5 for FivePoint, empty otherwise).
    pub points: Vec<(f32, f32)>,
    /// `...CurveMirrored`: the curve is applied to |input| with the input's sign (INFERRED from the name).
    pub mirrored: bool,
}

/// One effect layer (`<Layer .../>`). Strings are kept verbatim (enum names as the game writes them).
#[derive(Debug, Clone)]
pub struct Layer {
    pub enabled: bool,
    /// `Gs_Longitudinal`, `Gs_Lateral`, `SpeedMPH`, `Speed`, `Slip_Lateral`, `WorldCollision_Longitudinal`, ...
    pub input: String,
    /// `None`, `Perlin`, `Simplex`, `HeightGrid`, `Sin`.
    pub noise: String,
    /// `CarSpaceXYZOffset`, `CarSpaceYPROffset`, `CameraSpaceXYZOffset`, `CameraSpaceYPROffset`, `...Impulse`, `FOV`,
    /// `Tone`, `Vignette`, `SteeringWheelAngle`.
    pub output: String,
    pub take_derivative: bool,
    pub clamp: (f32, f32),
    /// `InputMagAttackDecay`: the input magnitude rises at `attack` and falls at `decay` (per second).
    pub attack_decay: Option<(f32, f32)>,
    pub param_curve: Curve,
    /// Noise: frequency curve over the input, base frequency, seed, output range.
    pub noise_freq_curve: Option<Curve>,
    pub noise_freq: f32,
    pub noise_seed: f32,
    pub noise_range: (f32, f32),
    /// `Param.x/y/z/w`: the output vector scaled by the curve value.
    pub param: [f32; 4],
    /// `Sprung` (offsets) / always for impulses: spring stiffness and damping as a fraction of critical.
    pub spring: Option<(f32, f32)>,
    /// Impulse layers: `ImpulseInputSizeCutoff`, `ImpulseDelay`.
    pub impulse_cutoff: f32,
    pub impulse_delay: f32,
}

/// `CameraPhysics.xml`: layer stacks by camera element name (`FollowCam`, `FollowCam2`, `DriverCam`, `Hood`, `BumperHigh`).
#[derive(Debug, Clone, Default)]
pub struct Physics {
    pub cams: Vec<(String, Vec<Layer>)>,
}

impl Physics {
    pub fn parse(xml: &str) -> Physics {
        let mut out = Physics::default();
        for t in tags(xml) {
            match t.name {
                "CameraPhysics" | "/CameraPhysics" => {}
                "Layer" => {
                    if let Some((_, layers)) = out.cams.last_mut() {
                        layers.push(layer(&t));
                    }
                }
                n if !n.starts_with('/') => out.cams.push((n.to_string(), Vec::new())),
                _ => {}
            }
        }
        out
    }

    pub fn cam(&self, name: &str) -> &[Layer] {
        self.cams.iter().find(|(n, _)| n == name).map(|(_, l)| l.as_slice()).unwrap_or(&[])
    }
}

/// `CameraGroups.xml` entry.
#[derive(Debug, Clone)]
pub struct Group {
    pub id: u32,
    /// String-table id (`IDS_CameraGroupGame`, `IDS_CameraGroupCarRelative1`, ...).
    pub name: String,
    pub user_facing: bool,
}

pub fn parse_groups(xml: &str) -> Vec<Group> {
    tags(xml)
        .iter()
        .filter(|t| t.name == "Group")
        .map(|t| Group { id: t.num("Id") as u32, name: t.get("Name").unwrap_or_default().to_string(), user_facing: t.get("userfacing") == Some("true") })
        .collect()
}

/// One `<Key>` of a camera path. Position in the space named by [`Anim::pos_space`], angles in degrees, FOV degrees.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub time: f32,
    pub pos: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    pub fov: f32,
}

#[derive(Debug, Clone)]
pub struct Anim {
    /// `PosAnimType`: `BoundingBoxSpace`, `CarSpace`, `WorldOffsetCar`, `WorldSpace`.
    pub pos_space: String,
    /// `RotAnimType`: `CarSpace`, `TargetCar`, `TargetPart`, `WorldSpace`, ...
    pub rot_space: String,
    /// `TimeAnimType`: `AffectsPath`, `PathIndependent`.
    pub time_type: String,
    pub in_cockpit: bool,
    pub const_vel_speed: f32,
    pub ease_in: f32,
    pub ease_out: f32,
    pub keys: Vec<Key>,
}

/// `CarRelativeCams.xml` `<Cam>` (replay / TV cameras). Every attribute of `<Cam>` is kept in `attrs`.
#[derive(Debug, Clone)]
pub struct CarRelativeCam {
    pub group: u32,
    pub kind: String,
    pub attrs: BTreeMap<String, String>,
    pub anim: Anim,
    pub dof_mode: String,
    pub motion_blur: f32,
    pub fade_in: f32,
    pub fade_out: f32,
    pub layers: Vec<Layer>,
    /// `<ShakyCamParams>` / `<TargetSmoothingParams>` attributes when present.
    pub shaky: Option<BTreeMap<String, String>>,
    pub target_smoothing: Option<BTreeMap<String, String>>,
}

/// Parses a `<Cams>` file (`CarRelativeCams.xml`; the menu cam files share the grammar).
pub fn parse_car_relative(xml: &str) -> Vec<CarRelativeCam> {
    let mut out: Vec<CarRelativeCam> = Vec::new();
    for t in tags(xml) {
        let cur = out.last_mut();
        match (t.name, cur) {
            ("Cam", _) => out.push(CarRelativeCam {
                group: t.num("Group") as u32,
                kind: t.get("Type").unwrap_or_default().to_string(),
                attrs: t.attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                anim: Anim {
                    pos_space: String::new(),
                    rot_space: String::new(),
                    time_type: String::new(),
                    in_cockpit: false,
                    const_vel_speed: 0.0,
                    ease_in: 0.0,
                    ease_out: 0.0,
                    keys: Vec::new(),
                },
                dof_mode: String::new(),
                motion_blur: 0.0,
                fade_in: 0.0,
                fade_out: 0.0,
                layers: Vec::new(),
                shaky: None,
                target_smoothing: None,
            }),
            ("Anim", Some(c)) => {
                c.anim.pos_space = t.get("PosAnimType").unwrap_or_default().to_string();
                c.anim.rot_space = t.get("RotAnimType").unwrap_or_default().to_string();
                c.anim.time_type = t.get("TimeAnimType").unwrap_or_default().to_string();
                c.anim.in_cockpit = t.num("IsInCockpit") != 0.0;
                c.anim.const_vel_speed = t.num("ConstVelCamSpeed");
                c.anim.ease_in = t.num("EaseIn");
                c.anim.ease_out = t.num("EaseOut");
            }
            ("Key", Some(c)) => c.anim.keys.push(Key {
                time: t.num("Time"),
                pos: [t.num("PosX"), t.num("PosY"), t.num("PosZ")],
                yaw: t.num("Yaw"),
                pitch: t.num("Pitch"),
                roll: t.num("Roll"),
                fov: t.num("FOV"),
            }),
            ("PostEffectsAnim", Some(c)) => {
                c.dof_mode = t.get("DOFMode").unwrap_or_default().to_string();
                c.motion_blur = t.num("MotionBlurAmount");
            }
            ("FadeInOptions", Some(c)) => c.fade_in = t.num("Duration"),
            ("FadeOutOptions", Some(c)) => c.fade_out = t.num("Duration"),
            ("Layer", Some(c)) => c.layers.push(layer(&t)),
            ("ShakyCamParams", Some(c)) => c.shaky = Some(t.map()),
            ("TargetSmoothingParams", Some(c)) => c.target_smoothing = Some(t.map()),
            _ => {}
        }
    }
    out
}

fn curve(t: &Tag, prefix: &str) -> Curve {
    let mapping = match t.get(&format!("{prefix}Mapping")) {
        Some("Linear") => Mapping::Linear,
        Some("ConstantOne") => Mapping::ConstantOne,
        Some("ThreePoint") => Mapping::ThreePoint,
        Some("FivePoint") => Mapping::FivePoint,
        _ => Mapping::Other,
    };
    let n = match mapping {
        Mapping::ThreePoint => 3,
        Mapping::FivePoint => 5,
        _ => 0,
    };
    let points = (0..n).map(|i| (t.num(&format!("{prefix}CurveInput{i}")), t.num(&format!("{prefix}CurveOutput{i}")))).collect();
    Curve { mapping, points, mirrored: t.num(&format!("{prefix}CurveMirrored")) != 0.0 }
}

fn layer(t: &Tag) -> Layer {
    let noise = t.get("NoiseType").unwrap_or("None").to_string();
    let output = t.get("OutputType").unwrap_or_default().to_string();
    // Impulse layers carry SpringK without a Sprung flag.
    let sprung = t.num("Sprung") != 0.0 || (t.get("Sprung").is_none() && t.get("SpringK").is_some());
    Layer {
        enabled: t.num("Enabled") != 0.0,
        input: t.get("InputType").unwrap_or_default().to_string(),
        take_derivative: t.num("InputTakeDerivative") != 0.0,
        clamp: (t.num("InputClampMin"), t.num("InputClampMax")),
        attack_decay: (t.num("InputMagAttackDecay") != 0.0).then(|| (t.num("InputAttack"), t.num("InputDecay"))),
        param_curve: curve(t, "InputToParam"),
        noise_freq_curve: t.get("InputToNoiseFreqMapping").map(|_| curve(t, "InputToNoiseFreq")),
        noise_freq: t.num("NoiseFreq"),
        noise_seed: t.num("NoiseSeed"),
        noise_range: (t.num("NoiseOutputRangeMin"), t.num("NoiseOutputRangeMax")),
        param: [t.num("Param.x"), t.num("Param.y"), t.num("Param.z"), t.num("Param.w")],
        spring: sprung.then(|| (t.num("SpringK"), t.num("SpringDPercent"))),
        impulse_cutoff: t.num("ImpulseInputSizeCutoff"),
        impulse_delay: t.num("ImpulseDelay"),
        noise,
        output,
    }
}

/// One XML tag: name (with a leading `/` for end tags) and attributes.
struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
}

impl<'a> Tag<'a> {
    fn get(&self, k: &str) -> Option<&'a str> {
        self.attrs.iter().find(|(a, _)| *a == k).map(|(_, v)| *v)
    }
    fn num(&self, k: &str) -> f32 {
        self.get(k).and_then(|v| v.trim().parse().ok()).unwrap_or(0.0)
    }
    fn map(&self) -> BTreeMap<String, String> {
        self.attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
}

fn tags(xml: &str) -> Vec<Tag<'_>> {
    let mut out = Vec::new();
    let mut s = xml;
    while let Some(i) = s.find('<') {
        s = &s[i..];
        if s.starts_with("<!--") {
            s = s.find("-->").map_or("", |e| &s[e + 3..]);
            continue;
        }
        let Some(e) = s.find('>') else { break };
        let body = &s[1..e];
        s = &s[e + 1..];
        if body.starts_with('?') || body.starts_with('!') {
            continue;
        }
        let body = body.trim_end_matches('/');
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let (name, mut rest) = body.split_at(name_end);
        let mut attrs = Vec::new();
        while let Some(eq) = rest.find('=') {
            let key = rest[..eq].trim();
            let after = rest[eq + 1..].trim_start();
            let Some(q) = after.chars().next() else { break };
            let Some(close) = after[1..].find(q) else { break };
            attrs.push((key, &after[1..1 + close]));
            rest = &after[close + 2..];
        }
        out.push(Tag { name, attrs });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera_zip() -> Option<crate::zip::Archive<std::fs::File>> {
        let disc = std::env::var("FH1_DISC").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../disc").into());
        crate::zip::Archive::open(std::path::Path::new(&disc).join("media/camera.zip")).ok()
    }

    #[test]
    fn camera_zip_parses() {
        let Some(mut z) = camera_zip() else { return };
        let text = |z: &mut crate::zip::Archive<_>, n: &str| {
            let e = z.entries.iter().find(|e| e.name == n).unwrap().clone();
            String::from_utf8(z.read(&e).unwrap()).unwrap()
        };
        let s = Settings::parse(&text(&mut z, "CameraSettings.ini"));
        assert_eq!(s.get("FollowLowCam\\FOV"), Some(48.5));
        assert_eq!(s.get("Driver\\FOV"), Some(62.0));
        let p = Physics::parse(&text(&mut z, "CameraPhysics.xml"));
        let names: Vec<_> = p.cams.iter().map(|(n, l)| (n.as_str(), l.len())).collect();
        assert_eq!(names, [("FollowCam", 74), ("FollowCam2", 69), ("DriverCam", 51), ("Hood", 47), ("BumperHigh", 46)]);
        let g = parse_groups(&text(&mut z, "CameraGroups.xml"));
        assert_eq!(g.len(), 17);
        let c = parse_car_relative(&text(&mut z, "CarRelativeCams.xml"));
        assert_eq!(c.len(), 138);
        assert_eq!(c.iter().map(|c| c.anim.keys.len()).sum::<usize>(), 546);
    }
}
