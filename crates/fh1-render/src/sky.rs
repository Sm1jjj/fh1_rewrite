//! FH1's real-time sky, drawn with the game's own sky shaders (the containers embedded in
//! default.xex) and driven by the TimeOfDayA curves. RE notes: docs/SHADERS.md "Sky".
//!
//! Parts (Sky::Draw 0x82D756A8 order): atmosphere dome, stars, sun/moon, far clouds, close clouds,
//! fog sky. All implemented (close clouds: main-view pass only; the env-cube pass is not drawn).
//!
//! The assets come from the install: `shaders/xex/<addr>.bin` (shaders group) and `sky/` (sky
//! group: realtimesky.zip textures). The install root is found through
//! [`crate::postfx::FxPostConfig`], so the engine needs no extra resource. `FH1_SKY=0` turns it off,
//! `FH1_SKY_CLOUDS=0` only the clouds;
//! `FH1_SKY_DUMP=<prefix>` writes the translated WGSL.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::NoFrustumCulling;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use fh1_shaders::effect::{rs, Effect, Pass, Technique};

use crate::lighting::FxTimeOfDay;
use crate::{FxGlobals, FxLibrary, FxMaterial};

mod clouds;
pub use clouds::{close_cloud_mesh, far_cloud_mesh, CloseCloudScroll, FarCloudScroll};
use clouds::{normal_w_from_uv0, texcoord_z_from_uv1, CLOSECLOUD_PS, CLOSECLOUD_VS, CUBECLOUD_PS, CUBECLOUD_VS, FARCLOUD_PS, FARCLOUD_VS};

pub struct FxSkyPlugin;

impl Plugin for FxSkyPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, (crate::lighting::apply_tod_env, setup_sky))
            .add_systems(PostUpdate, crate::lighting::update_bevy_lights.after(crate::lighting::update_time_of_day).run_if(|| !crate::remaster()))
            .add_systems(PostUpdate, update_sky.after(crate::lighting::update_time_of_day).before(bevy::transform::TransformSystems::Propagate));
    }
}

/// Dome radius (VERIFIED, 0x82D723A0).
const RADIUS: f32 = 300.0;
/// Atmosphere shaders (VERIFIED, Sky::Draw → 0x823FADC0).
const ATMOSPHERE_VS: u32 = 0x8218_12C8;
const ATMOSPHERE_PS: u32 = 0x8215_5780;
/// Sun/moon shaders (VERIFIED, 0x82447720).
const SUN_VS: u32 = 0x8218_2AD8;
const SUN_PS: u32 = 0x8215_7388;
/// Fog sky shaders (VERIFIED, 0x823FB4D0).
const FOGSKY_VS: u32 = 0x8218_1998;
const FOGSKY_PS: u32 = 0x8215_5B50;
/// The fog sky's PS c40-42 (FogColor, FogScatterColor, FogParams) are above the material range; they
/// move to material c12-14, which its PS doesn't use.
const FOGSKY_REMAP: &[(u32, u32)] = &[(40, 12), (41, 13), (42, 14)];
/// Stars shaders (VERIFIED, 0x823FA680).
const STARS_VS: u32 = 0x8218_2DF8;
const STARS_PS: u32 = 0x8215_7518;
/// StarPowers defaults (P+0x230..0x238, set by the parameter block constructor 0x825C7C98 and
/// never written by the TOD evaluator; VERIFIED): (low, high, power), alpha × StarFade.
const STAR_POWERS: [f32; 3] = [0.25, 0.97, 7.0];
/// The prologue's c100 (GainAndMinSkyHeight / Gain) lives in a global register in the game; here
/// it is moved to the last material register, which the sky shaders don't use.
const GAIN_REG: usize = 15;

/// Install root (`.../assets/private`), from the post config's `shaders/xex` folder.
fn install_root(config: &crate::postfx::FxPostConfig) -> Option<PathBuf> {
    Some(config.0.xex_dir.parent()?.parent()?.to_path_buf())
}

/// The sky dome (0x82D723A0, VERIFIED): 16 rings × 100 segments, then the apex. Ring k sits at
/// elevation (k−1)/15·90° with w = k/15; normals point inwards. w goes to UV_0.x (the game's
/// position.w).
pub fn dome_mesh() -> Mesh {
    let (mut pos, mut nrm, mut uv) = (Vec::new(), Vec::new(), Vec::new());
    for k in 0..16 {
        let e = (k as f32 - 1.0) / 15.0 * std::f32::consts::FRAC_PI_2;
        for j in 0..100 {
            let a = j as f32 / 100.0 * std::f32::consts::TAU;
            let p = Vec3::new(e.cos() * a.cos(), e.sin(), e.cos() * a.sin());
            pos.push((p * RADIUS).to_array());
            nrm.push((-p).to_array());
            uv.push([k as f32 / 15.0, 0.0]);
        }
    }
    pos.push([0.0, RADIUS, 0.0]);
    nrm.push([0.0, -1.0, 0.0]);
    uv.push([1.0, 0.0]);
    let mut idx = Vec::with_capacity(9300);
    for k in 0..15u32 {
        for j in 0..100u32 {
            let v = k * 100 + j;
            let n = k * 100 + (j + 1) % 100;
            idx.extend_from_slice(&[v, n, n + 100, v, n + 100, v + 100]);
        }
    }
    for i in 1..=100u32 {
        idx.extend_from_slice(&[1500 + i - 1, 1500 + i % 100, 1600]);
    }
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    m.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nrm);
    m.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    m.insert_indices(Indices::U32(idx));
    m
}

/// Sun/moon billboard (0x82447720): corners (−1,−1), (−1,1), (1,1), (1,−1); the VS places it.
fn quad_mesh() -> Mesh {
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[-1.0f32, -1.0, 0.0], [-1.0, 1.0, 0.0], [1.0, 1.0, 0.0], [1.0, -1.0, 0.0]]);
    m.insert_indices(Indices::U32(vec![0, 1, 2, 0, 2, 3]));
    m
}

/// `Stars.bin` (VERIFIED, stars init 0x82D75280): float4 per star, count = size / 16, copied as is
/// into a point-list vertex buffer. xyz = unit direction (both hemispheres), w = brightness 0..1
/// (goes to UV_0.x like the dome's w). Directions are in the game's left-handed space: z negated.
fn stars_mesh(path: &Path) -> Option<Mesh> {
    let d = std::fs::read(path).ok()?;
    let f = |o: usize| f32::from_be_bytes(d[o..o + 4].try_into().unwrap());
    let n = d.len() / 16;
    let pos: Vec<[f32; 3]> = (0..n).map(|i| [f(i * 16), f(i * 16 + 4), -f(i * 16 + 8)]).collect();
    let uv: Vec<[f32; 2]> = (0..n).map(|i| [f(i * 16 + 12), 0.0]).collect();
    let mut m = Mesh::new(PrimitiveTopology::PointList, RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    m.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    Some(m)
}

/// Fog sky grid (generator 0x82D726D8, VERIFIED): 4 rows × 32 columns of quads (D3DPT_QUADLIST), each
/// vertex USHORT2N (column/32, row/4) = (azimuth turns, elevation lerp), here as UV_0. The VS
/// builds the position; POSITION is only there for Bevy.
fn fog_sky_mesh() -> Mesh {
    let (mut uv, mut idx) = (Vec::new(), Vec::new());
    for row in 0..4u32 {
        for col in 0..32u32 {
            let base = uv.len() as u32;
            for (dc, dr) in [(0, 0), (1, 0), (1, 1), (0, 1)] {
                uv.push([(col + dc) as f32 / 32.0, (row + dr) as f32 / 4.0]);
            }
            idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; uv.len()]);
    m.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    m.insert_indices(Indices::U32(idx));
    m
}

/// Star field spin (stars tick 0x8246F620, VERIFIED): angle += rate·dt in degrees, wrapped into
/// [0, 360]; Rotation c5-8 = rotation by the angle about (sin b·cos a, cos b, sin b·sin a) (0x82459690,
/// D3D row-vector). a, b, rate = stars +0x10/+0x14/+0x18 = 10°, 90°, 0.03 from the ctor 0x82D71DC8 (no
/// TOD channel); angle starts at 0. dt = the cloud ticks' (frame time, INFERRED).
#[derive(Default)]
pub struct StarSpin {
    pub angle: f32,
}

const STAR_AXIS_DEG: (f32, f32) = (10.0, 90.0);
const STAR_RATE: f32 = 0.03;

impl StarSpin {
    pub fn advance(&mut self, dt: f32) {
        self.angle = (self.angle + STAR_RATE * dt).rem_euclid(360.0);
    }

    /// Engine-space rotation: the game's axis with z negated, angle negated (the mirror flips handedness).
    pub fn rotation(&self) -> Mat3 {
        let (a, b) = (STAR_AXIS_DEG.0.to_radians(), STAR_AXIS_DEG.1.to_radians());
        let axis = Vec3::new(b.sin() * a.cos(), b.cos(), -(b.sin() * a.sin())).normalize();
        Mat3::from_axis_angle(axis, -self.angle.to_radians())
    }
}

/// A one-pass effect from two default.xex containers, with the render states the host code sets
/// around the draw.
fn xex_effect(dir: &Path, vs: u32, ps: u32, render_states: &[(u32, u32)]) -> Option<Effect> {
    let v = crate::post::load_xex_shader(dir, vs)?;
    let p = crate::post::load_xex_shader(dir, ps)?;
    Some(Effect {
        hash: vs,
        shaders: vec![Arc::unwrap_or_clone(v), Arc::unwrap_or_clone(p)],
        techniques: vec![Technique {
            name: "sky".into(),
            passes: vec![Pass { name: "p0".into(), vs: Some(0), ps: Some(1), render_states: render_states.to_vec() }],
        }],
    })
}

/// Translate a sky shader pair and adapt it to the engine:
/// - MatWVP register k = column k of our column-vector matrix: the sky VSs multiply input x by c0
///   (c0.xwy = clip x, w, y from x; VERIFIED from the atmosphere and sun microcode);
/// - depth 0 = the far plane in Bevy's reverse-Z (the game writes z = −w−1e-6, its far plane);
/// - c100 → material c15;
/// - `w_from_uv`: position.w comes from UV_0.x (Bevy positions are vec3; the dome keeps k/15 there).
/// - `ps_remap`: PS constants (game register → material register) above the material range.
fn sky_program(dir: &Path, vs: u32, ps: u32, render_states: &[(u32, u32)], raw_output: bool, w_from_uv: bool, ps_remap: &[(u32, u32)]) -> Option<crate::Program> {
    let fx = xex_effect(dir, vs, ps, render_states)?;
    let mut p = crate::program::build(&fx, "sky", false, raw_output, &crate::program::Family::Track)?;
    let mut w = p.wgsl.clone();
    for k in 0..4 {
        w = w.replace(&format!("fx_row(fx_wvp, {k})"), &format!("fx_wvp[{k}]"));
    }
    w = w
        .replace("out.position = o.pos;", "out.position = vec4<f32>(o.pos.x, o.pos.y, 0.0, o.pos.w);")
        .replace("fx_glob.vs[100]", &format!("fx_mat.vs[{GAIN_REG}]"))
        .replace("fx_glob.ps[100]", &format!("fx_mat.ps[{GAIN_REG}]"));
    for &(from, to) in ps_remap {
        w = w.replace(&format!("fx_glob.ps[{from}]"), &format!("fx_mat.ps[{to}]"));
    }
    if w_from_uv {
        w = w
            .replace("    @location(0) a0: vec3<f32>,\n", "    @location(0) a0: vec3<f32>,\n    @location(2) a2: vec2<f32>,\n")
            .replace("fx_vin.a0 = vec4<f32>(input.a0, 1.0);", "fx_vin.a0 = vec4<f32>(input.a0, input.a2.x);");
        if !w.contains("input.a2.x") {
            return None;
        }
        if !p.attributes.contains(&2) {
            p.attributes.push(2);
            p.attributes.sort();
        }
    }
    if !w.contains("o.pos.x, o.pos.y, 0.0") || w.contains("fx_row(fx_wvp") {
        return None;
    }
    p.wgsl = w;
    Some(p)
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkyPart {
    Atmosphere,
    Stars,
    FogSky,
    Sun,
    Moon,
    FarClouds,
    CloseClouds,
    /// The close clouds' env-cube pass: the whole sky of the live cube faces (reflect.rs CUBE_LAYER only).
    CubeClouds,
}

fn setup_sky(
    mut commands: Commands,
    config: Option<Res<crate::postfx::FxPostConfig>>,
    lib: Option<Res<FxLibrary>>,
    globals: Res<FxGlobals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FxMaterial>>,
) {
    if std::env::var("FH1_SKY").is_ok_and(|v| v == "0") {
        return;
    }
    let (Some(config), Some(lib)) = (config, lib) else { return };
    let Some(root) = install_root(&config) else { return };
    let dir = &config.0.xex_dir;
    let mut tex = |name: &str| {
        let t = crate::scenery::read_dds(&root.join("sky").join(format!("{name}.dds"))).map(|i| images.add(i));
        if t.is_none() {
            warn!("fh1-render sky: sky/{name}.dds missing (run fh1setup --only sky)");
        }
        t
    };
    let no_z = (rs::ZWRITEENABLE, 0);
    // Sun: srcα / one (additive). Moon: srcα / invsrcα (VERIFIED, 0x82447720).
    let parts = [
        (SkyPart::Atmosphere, ATMOSPHERE_VS, ATMOSPHERE_PS, vec![], true, tex("SkyDither")),
        // Stars: srcα / invsrcα (VERIFIED, 0x823FA680), a point list.
        (SkyPart::Stars, STARS_VS, STARS_PS, vec![(rs::ALPHABLENDENABLE, 1), (rs::SRCBLEND, 6), (rs::DESTBLEND, 7), no_z], true, None),
        // Fog sky: srcα / invsrcα (VERIFIED, 0x823FB4D0).
        (SkyPart::FogSky, FOGSKY_VS, FOGSKY_PS, vec![(rs::ALPHABLENDENABLE, 1), (rs::SRCBLEND, 6), (rs::DESTBLEND, 7), no_z], false, None),
        (SkyPart::Sun, SUN_VS, SUN_PS, vec![(rs::ALPHABLENDENABLE, 1), (rs::SRCBLEND, 6), (rs::DESTBLEND, 1), no_z], false, tex("Sun")),
        // Moon phase index = sun object +0x70; its only writer found, the sun draw 0x82447720, sets 0 when
        // SunObjectMoon crosses 0.5 upwards, so Moon0 (INFERRED: Moon1-4 are loaded but never selected).
        (SkyPart::Moon, SUN_VS, SUN_PS, vec![(rs::ALPHABLENDENABLE, 1), (rs::SRCBLEND, 6), (rs::DESTBLEND, 7), no_z], false, tex("Moon0")),
        // Far clouds: srcα / invsrcα (VERIFIED, 0x82406030). Sampler 0 = FarCloudMask, 1 =
        // FarCloudMap{Front}<set> (CloudDefs.xml: one set "base" = 0). Sampler 2 (FarCloudMapBack0) is
        // bound by the host but the PS only fetches tf0/tf1 (VERIFIED from the microcode).
        (SkyPart::FarClouds, FARCLOUD_VS, FARCLOUD_PS, vec![(rs::ALPHABLENDENABLE, 1), (rs::SRCBLEND, 6), (rs::DESTBLEND, 7), no_z], false, tex("FarCloudMask")),
        // Close clouds: srcα / invsrcα (VERIFIED, 0x82D71228), slot C+0x118 textures: sampler 0 =
        // AlphaDensity, 1 = TopBottomAmbient, 2 = Ambient (CloudDefs set 0, Front).
        (SkyPart::CloseClouds, CLOSECLOUD_VS, CLOSECLOUD_PS, vec![(rs::ALPHABLENDENABLE, 1), (rs::SRCBLEND, 6), (rs::DESTBLEND, 7), no_z], true, tex("CloseCloudAlphaDensityFront0")),
        // Env cube (mode 1): no blend (VERIFIED, 0x82D71228). Slot C+0x170 "FrontSmall" holds the same set-0
        // textures (INFERRED; CloudDefs has one set): sampler 0 = AlphaDensity, 2 = Ambient (the PS reads no tf1).
        (SkyPart::CubeClouds, CUBECLOUD_VS, CUBECLOUD_PS, vec![], true, tex("CloseCloudAlphaDensityFront0")),
    ];
    let far_map = tex("FarCloudMapFront0");
    let close_tb = tex("CloseCloudTopBottomAmbientFront0");
    let close_amb = tex("CloseCloudAmbientFront0");
    let close = meshes.add(close_cloud_mesh());
    let dome = meshes.add(dome_mesh());
    let quad = meshes.add(quad_mesh());
    let fog = meshes.add(fog_sky_mesh());
    let far = meshes.add(far_cloud_mesh());
    let stars = stars_mesh(&root.join("sky/Stars.bin")).map(|m| meshes.add(m));
    let clouds = !std::env::var("FH1_SKY_CLOUDS").is_ok_and(|v| v == "0");
    for (part, vs, ps, states, w_from_uv, t0) in parts {
        if matches!(part, SkyPart::FarClouds | SkyPart::CloseClouds | SkyPart::CubeClouds) && !clouds {
            continue;
        }
        // FH1_SKY_SKIP=fogsky,atmosphere,...: leave parts out (debug A/B; names = SkyPart, lower case).
        if std::env::var("FH1_SKY_SKIP").is_ok_and(|v| v.split(',').any(|n| n.trim().eq_ignore_ascii_case(&format!("{part:?}")))) {
            continue;
        }
        // Remaster: the game's sky (dome, fog sky, sun, clouds, stars, moon; output gain applied) is drawn over Bevy's
        // atmosphere, which still provides the env map, sun transmittance and aerial perspective. No live cube = no cube
        // clouds. FH1_RM_GAME_SKY=0 = Bevy's atmosphere sky instead of the game's dome/fog sky/sun.
        // Exception: the remaster car probe's faces draw the env-cube pass as their sky (fh1-remaster car_probe.rs "Time of
        // day"; same switch as its `cube_sky_on`), the game's reflection sky instead of the main view's (green at night).
        let bevy_sky = std::env::var("FH1_RM_GAME_SKY").as_deref() == Ok("0");
        if crate::remaster() && ((part == SkyPart::CubeClouds && !remaster_probe_cube_sky()) || (bevy_sky && matches!(part, SkyPart::Atmosphere | SkyPart::FogSky | SkyPart::Sun))) {
            continue;
        }
        let program = sky_program(dir, vs, ps, &states, lib.raw_output, w_from_uv, if part == SkyPart::FogSky { FOGSKY_REMAP } else { &[] })
            .and_then(|mut p| (part != SkyPart::FarClouds || texcoord_z_from_uv1(&mut p).is_some()).then_some(p))
            .and_then(|mut p| (!matches!(part, SkyPart::CloseClouds | SkyPart::CubeClouds) || normal_w_from_uv0(&mut p).is_some()).then_some(p));
        let Some(program) = program else {
            warn!("fh1-render sky: {part:?}: shaders missing or WGSL patch failed (run fh1setup shaders)");
            continue;
        };
        if let Some(p) = std::env::var_os("FH1_SKY_DUMP") {
            let _ = std::fs::write(format!("{}_{part:?}.wgsl", p.to_string_lossy()), &program.wgsl);
        }
        let handle = shaders.add(Shader::from_wgsl(program.wgsl.clone(), format!("fh1/fx/sky_{part:?}.wgsl")));
        let id = crate::material::register(&program, handle);
        let mut mat = lib.material((id, &program), &[], &[], &globals);
        mat.t0 = t0;
        if part == SkyPart::FarClouds {
            mat.t1 = far_map.clone();
        }
        if part == SkyPart::CloseClouds {
            mat.t1 = close_tb.clone();
            mat.t2 = close_amb.clone();
        }
        if part == SkyPart::CubeClouds {
            mat.t2 = close_amb.clone();
        }
        let mesh = match part {
            SkyPart::Atmosphere => dome.clone(),
            SkyPart::Stars => match &stars {
                Some(s) => s.clone(),
                None => {
                    warn!("fh1-render sky: sky/Stars.bin missing (run fh1setup --only sky)");
                    continue;
                }
            },
            SkyPart::FogSky => fog.clone(),
            SkyPart::FarClouds => far.clone(),
            SkyPart::CloseClouds | SkyPart::CubeClouds => close.clone(),
            SkyPart::Sun | SkyPart::Moon => quad.clone(),
        };
        let mut e = commands.spawn((part, Mesh3d(mesh), MeshMaterial3d(materials.add(mat)), Transform::default(), NoFrustumCulling, bevy::light::NotShadowCaster, Name::new(format!("FH1 sky {part:?}"))));
        if part == SkyPart::CubeClouds {
            // Only the live cube's face cameras see it (the main view draws the separate passes).
            e.insert(bevy::camera::visibility::RenderLayers::layer(crate::reflect::CUBE_LAYER));
        }
    }
}

/// Mirror of fh1-remaster car_probe.rs `cube_sky_on` (this crate can't depend on it): the remaster car probe is on
/// (FH1_RM_CAR_PROBE=1, opt-in since 2026-10-08), hides the player's own lights on the main-only layer (FH1_RM_PROBE_OWN_LIGHTS != 1, which the
/// sky parts reuse) and FH1_RM_PROBE_CUBE_SKY is not 0.
fn remaster_probe_cube_sky() -> bool {
    let not = |k: &str, v: &str| std::env::var(k).map_or(true, |x| x != v);
    std::env::var("FH1_RM_CAR_PROBE").is_ok_and(|x| x == "1") && not("FH1_RM_PROBE_OWN_LIGHTS", "1") && not("FH1_RM_PROBE_CUBE_SKY", "0")
}

/// Constants of one sky draw, (register, value) per stage, and whether it draws.
pub struct PartConsts {
    pub vs: Vec<(usize, Vec4)>,
    pub ps: Vec<(usize, Vec4)>,
    pub visible: bool,
}

/// Left-handed TOD position → engine space.
fn tod_pos(t: &FxTimeOfDay, name: &str, m: f32) -> Vec3 {
    let v = t.tod.get(name, m);
    Vec3::new(v[0], v[1], -v[2])
}

/// A sky part's constants for the current time of day (prologue 0x82444478 + the part's draw
/// function; VERIFIED unless marked).
pub fn part_consts(part: SkyPart, t: &FxTimeOfDay, scroll: &FarCloudScroll, close: &CloseCloudScroll, stars: &StarSpin) -> PartConsts {
    let m = t.minutes();
    let s = |n: &str| t.tod.scalar(n, m);
    let v3 = |n: &str| Vec3::from_array(t.tod.get(n, m));
    // atan2 (0x823F3DF0, INFERRED) of the fog-sky fade start against a fixed 15 km.
    let min_height = RADIUS * s("FogSkyHeightFadeStart").min(-1000.0).atan2(15_000.0).sin() + 1.0;
    let gain = Vec4::new(s("SkyGain"), min_height, 0.0, 0.0);
    match part {
        SkyPart::Atmosphere => {
            // hazeDir points from the haze object at the origin (dome normals point inwards).
            let haze = (tod_pos(t, "HazeObjectTargetPos", m) - tod_pos(t, "HazeObjectPos", m)).normalize_or(Vec3::NEG_Y);
            let tangent = haze.cross(haze.cross(Vec3::Y)).normalize_or_zero();
            PartConsts {
                vs: vec![(5, Vec4::new(s("AtmosphereClampPower"), s("AtmosphereBottomClampPower"), 0.0, 0.0)), (GAIN_REG, gain)],
                ps: vec![
                    (1, Vec4::new(s("AtmosphereColourPower").max(0.01), s("AtmosphereHazePower"), 0.0, 0.0)),
                    (2, (v3("AtmosphereHazeColour") * s("AtmosphereHazeIntensity")).extend(s("AtmosphereHazeAlpha"))),
                    // DitherSettings = atmosphere +0xBC/+0xC0/+0xC4 = (1/128, 1/128, 1/256) from its ctor
                    // 0x82D74738 (VERIFIED): one texel per pixel of the 128² SkyDither map, ±½ step of 8 bits.
                    (3, Vec4::new(1.0 / 128.0, 1.0 / 128.0, 1.0 / 256.0, 0.0)),
                    (4, haze.extend(0.0)),
                    (5, tangent.extend(s("AtmosphereHazeEllipticalScale"))),
                    (6, v3("AtmosphereTopColour").extend(1.0)),
                    (7, v3("AtmosphereBottomColour").extend(1.0)),
                    (GAIN_REG, gain),
                ],
                visible: true,
            }
        }
        SkyPart::Stars => {
            // Rotation c5-8 (stars +0x40, see StarSpin). The VS computes x·c5 + y·c6 + z·c7 + c8, so the
            // registers hold the engine rotation's columns. Drawn only in Sky::Draw mode 4, INFERRED to be
            // the main view.
            let fade = s("StarFade");
            let r = stars.rotation();
            PartConsts {
                vs: vec![(5, r.x_axis.extend(0.0)), (6, r.y_axis.extend(0.0)), (7, r.z_axis.extend(0.0)), (8, Vec4::W)],
                ps: vec![(1, Vec4::new(STAR_POWERS[0], STAR_POWERS[1], STAR_POWERS[2], fade))],
                visible: fade > 0.0,
            }
        }
        SkyPart::FogSky => {
            // Fog sky block F = P+0x270 (VERIFIED, evaluator 0x825CB718): fog colour, scatter colour,
            // density and scatter power from the TOD fog channels × the fog template; the height-fade
            // pair is FogSkyHeightFade{Start,End} × the template's height fade. (Some values get the
            // template a second time near the end of the evaluator; single here, INFERRED.)
            let fog = &t.fog;
            let start = s("FogSkyHeightFadeStart") * fog.height_fade_start;
            let end = s("FogSkyHeightFadeEnd") * fog.height_fade_end;
            let sun = (tod_pos(t, "SunObjectTargetPos", m) - tod_pos(t, "SunObjectPos", m)).normalize_or(Vec3::NEG_Y);
            PartConsts {
                // Dimensions: elevation range of the band (prologue 0x82444478: atan2 against 15 km).
                vs: vec![(4, Vec4::new(start.min(-1000.0).atan2(15_000.0), end.atan2(15_000.0), 0.0, 0.0))],
                ps: vec![
                    (11, sun.extend(0.0)),
                    (12, (v3("FogColour") * fog.color).extend(0.0)),
                    (13, (v3("FogSunScatterColour") * fog.sun_scatter_color).extend(0.0)),
                    (14, Vec4::new(start, end, (s("FogDensity") * fog.density * 0.01 * 15_000.0).min(1.0), s("FogSunScatterPower") * fog.sun_scatter_power)),
                    (GAIN_REG, gain),
                ],
                visible: true,
            }
        }
        SkyPart::FarClouds => clouds::far_consts(t, scroll, gain),
        SkyPart::CloseClouds => clouds::close_consts(t, close, gain),
        SkyPart::CubeClouds => clouds::cube_consts(t, close, gain),
        SkyPart::Sun | SkyPart::Moon => {
            // SunDirection = normalize(sunObjDir) (VERIFIED, prologue 0x82444478 → VS c4, PS c0/c11) points away from the sun; the VS puts the quad at
            // −dir·300 with half-size SunObjectSize.
            let dir = (tod_pos(t, "SunObjectTargetPos", m) - tod_pos(t, "SunObjectPos", m)).normalize_or(Vec3::NEG_Y);
            let moon = s("SunObjectMoon") > 0.5;
            let colour = v3("SunObjectColour").clamp(Vec3::ZERO, Vec3::ONE);
            let alpha = s("SunObjectAlpha");
            let c1 = if moon { colour.extend(alpha) } else { (colour * alpha).extend(1.0) };
            PartConsts {
                vs: vec![(4, dir.extend(0.0)), (5, Vec4::new(RADIUS, s("SunObjectSize"), 0.0, 0.0))],
                ps: vec![(1, c1)],
                visible: moon == (part == SkyPart::Moon),
            }
        }
    }
}

/// `FH1_SKY_CUBE_AB=1` (perf A/B): the env-cube cloud pass on / off for 5 s each, logging each half's
/// mean frame time (the first 10 s are skipped).
#[derive(Default)]
struct CubeAb {
    t: f32,
    acc: [(f32, u32); 2],
}

impl CubeAb {
    fn step(&mut self, dt: f32) -> bool {
        if !std::env::var("FH1_SKY_CUBE_AB").is_ok_and(|v| v == "1") {
            return true;
        }
        self.t += dt;
        let on = (self.t / 5.0) as u32 % 2 == 0;
        if self.t > 10.0 && self.t % 5.0 > 1.0 {
            let a = &mut self.acc[on as usize];
            a.0 += dt * 1000.0;
            a.1 += 1;
        }
        if self.t % 20.0 < dt {
            let m = |a: (f32, u32)| a.0 / a.1.max(1) as f32;
            info!("cube clouds A/B mean frame time: on {:.2} ms ({}), off {:.2} ms ({})", m(self.acc[1]), self.acc[1].1, m(self.acc[0]), self.acc[0].1);
        }
        on
    }
}

fn scroll_hz() -> f32 {
    static HZ: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *HZ.get_or_init(|| std::env::var("FH1_SKY_SCROLL_HZ").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0))
}

fn clock_secs() -> f32 {
    static S: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *S.get_or_init(|| std::env::var("FH1_SKY_CLOCK_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5f32).max(0.0))
}

fn update_sky(
    tod: Option<Res<FxTimeOfDay>>,
    cams: Query<&GlobalTransform, (With<crate::post::FxPostCamera>, Without<SkyPart>)>,
    mut parts: Query<(&SkyPart, &mut Transform, &mut Visibility, &MeshMaterial3d<FxMaterial>)>,
    mut materials: ResMut<Assets<FxMaterial>>,
    time: Res<Time>,
    mut scroll: Local<FarCloudScroll>,
    mut close: Local<CloseCloudScroll>,
    mut last: Local<Option<f32>>,
    real: Res<Time<Real>>,
    mut ab: Local<CubeAb>,
    mut stars: Local<StarSpin>,
    mut since: Local<(f32, f32)>,
    mut since_clock: Local<f32>,
) {
    let cube_on = ab.step(real.delta_secs());
    if let Some(cam) = cams.iter().next() {
        if std::env::var_os("FH1_SKY_LOG").is_some() && (real.elapsed_secs() % 2.0) < real.delta_secs() {
            info!("sky cam forward {:?} pos {:?}", cam.forward().as_vec3(), cam.translation());
        }
        for (part, mut tf, _, _) in &mut parts {
            // Blended parts sort back to front by their origin's view depth: nudge the origins (a few cm,
            // invisible on a 300 m dome) so they draw in Sky::Draw order: stars, sun/moon, far clouds,
            // fog sky.
            let ahead = match part {
                SkyPart::Atmosphere => 0.0,
                SkyPart::Stars => 0.04,
                SkyPart::Sun | SkyPart::Moon => 0.03,
                SkyPart::FarClouds => 0.02,
                SkyPart::CloseClouds => 0.015,
                SkyPart::CubeClouds => 0.0,
                SkyPart::FogSky => 0.01,
            };
            let p = cam.translation() + cam.forward() * ahead;
            if tf.translation != p {
                tf.translation = p;
            }
        }
    }
    let Some(t) = tod else { return };
    let m = t.minutes();
    // Default speed 9 (0x825C6D70, VERIFIED) when the channel is absent.
    let speed = |p: &str| Vec2::new(t.tod.scalar_or(&format!("FarCloud{p}SpeedX"), m, 9.0), t.tod.scalar_or(&format!("FarCloud{p}SpeedZ"), m, 9.0));
    scroll.advance([speed(""), speed("Back")], time.delta_secs());
    close.advance(Vec2::new(t.tod.scalar("CloseCloudFrontRevs", m), t.tod.scalar("CloseCloudBackRevs", m)), time.delta_secs());
    stars.advance(time.delta_secs());
    // The clouds scroll every frame; the rest only changes with the clock. The scroll / spin advance every frame, but their
    // materials are written at FH1_SKY_SCROLL_HZ (default 10 since 2026-10-08, was 30: a far dome's scroll step per write
    // is far below a pixel; stars at a third of it; 0 = every frame): every write
    // re-prepares the material in the render world (2026-10-08 perf: 4 sky materials rewritten every frame while parked).
    // The TOD clock runs, so `seconds` changes every frame: the clock-driven constants (colours, sun / moon, fog sky) are
    // written at most every FH1_SKY_CLOCK_SECS real seconds (0.5; 0 = every change), at once on a jump (> 2 game minutes:
    // a TOD change from the menu / FH1_TOD). 2026-10-08 perf (user log 101726): every write re-prepares the FxMaterial
    // and re-checks its entities for specialization (~0.4 ms per frame with grass / glows doing the same).
    *since_clock += real.delta_secs();
    let jump = last.is_none_or(|l| (t.seconds - l).abs() > 120.0);
    let clock_changed = *last != Some(t.seconds) && (jump || *since_clock >= clock_secs());
    if clock_changed {
        *last = Some(t.seconds);
        *since_clock = 0.0;
    }
    let hz = scroll_hz();
    since.0 += real.delta_secs();
    since.1 += real.delta_secs();
    let clouds_due = hz <= 0.0 || since.0 >= 1.0 / hz;
    let stars_due = hz <= 0.0 || since.1 >= 3.0 / hz;
    if clouds_due {
        since.0 = 0.0;
    }
    if stars_due {
        since.1 = 0.0;
    }
    for (&part, _, mut vis, mat) in &mut parts {
        let due = match part {
            SkyPart::FarClouds | SkyPart::CloseClouds | SkyPart::CubeClouds => clouds_due,
            SkyPart::Stars => stars_due,
            _ => false,
        };
        if !clock_changed && !due {
            continue;
        }
        let c = part_consts(part, &t, &scroll, &close, &stars);
        // FH1_SKY_LOG=1: the constants per part whenever the clock moves (A/B against a game capture).
        if clock_changed && std::env::var_os("FH1_SKY_LOG").is_some() {
            info!("sky consts {part:?} vs {:?} ps {:?}", c.vs, c.ps);
        }
        let visible = c.visible && (part != SkyPart::CubeClouds || cube_on);
        vis.set_if_neq(if visible { Visibility::Inherited } else { Visibility::Hidden });
        let Some(mut m) = materials.get_mut(&mat.0) else { continue };
        for (r, v) in c.vs {
            m.consts.vs[r] = v;
        }
        for (r, v) in c.ps {
            m.consts.ps[r] = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dome_counts() {
        let m = dome_mesh();
        assert_eq!(m.count_vertices(), 1601);
        assert_eq!(m.indices().unwrap().len(), 9300);
    }

}

#[cfg(test)]
mod star_tests {
    use super::*;

    #[test]
    fn star_spin() {
        let mut s = StarSpin::default();
        assert!(s.rotation().abs_diff_eq(Mat3::IDENTITY, 1e-6));
        s.advance(12_000.0 + 3.0 / STAR_RATE);
        assert!((s.angle - 3.0).abs() < 1e-2, "{}", s.angle);
        // The axis (game (cos 10°, 0, sin 10°) -> engine z negated) stays fixed.
        let axis = Vec3::new(10f32.to_radians().cos(), 0.0, -10f32.to_radians().sin());
        assert!((s.rotation() * axis - axis).length() < 1e-5);
    }
}
