//! FH1 sky clouds: far cloud cap (draw 0x82406030), close cloud dome (main view 0x82D71228) and its env-cube pass.
//! Meshes, scroll state and per-draw constants; sky.rs spawns and updates the parts. RE notes: docs/SHADERS.md "Sky"
//! (far / close clouds). Owner: c7 (moved out of sky.rs 2026-10-05 with a2's OK).

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

use super::{tod_pos, PartConsts, GAIN_REG, RADIUS};
use crate::lighting::FxTimeOfDay;

/// Far cloud shaders (VERIFIED, 0x82406030).
pub(super) const FARCLOUD_VS: u32 = 0x8218_2420;
pub(super) const FARCLOUD_PS: u32 = 0x8215_6BA8;
/// Close cloud main-view shaders (VERIFIED, 0x82D71228; the cube pass 82181F78/82156910 is not drawn).
pub(super) const CLOSECLOUD_VS: u32 = 0x8218_1C68;
pub(super) const CLOSECLOUD_PS: u32 = 0x8215_64E0;
/// Close clouds, env cube mode 1 (C+0x10C / C+0x114): the VS also computes the atmosphere colour.
pub(super) const CUBECLOUD_VS: u32 = 0x8218_1F78;
pub(super) const CUBECLOUD_PS: u32 = 0x8215_6910;
/// Far cloud cap (init 0x82D73940, VERIFIED): a sphere of radius 10 km centred 9.7 km below the eye,
/// cut where a ray 6° below the horizon meets it. 15 rings × 100 segments + apex (1,501 vertices,
/// 8,700 indices). Ring k at sphere elevation φ = θ·k/15 + (90° − θ), where θ = the central angle
/// of that cut; azimuth a = j/100·2π. Texcoord = (sin a·r/2, cos a·r/2, r) with r = 1 − k/15 (1 at
/// the rim, 0 at the apex); (x, y) go to UV_0, r to UV_1.x. Built in the game's left-handed space,
/// then z is negated (and the winding reversed) so the UVs scroll the right way in engine space.
pub fn far_cloud_mesh() -> Mesh {
    const R: f64 = 10_000.0;
    const H: f64 = 9_700.0;
    // Ray from the eye (0, H, 0) about the sphere centre at 6° (0.10471967 rad) down.
    let a = 0.104_719_670_613_606_76f64;
    let (b, c) = (-H * a.sin(), H * H - R * R);
    let disc = (4.0 * b * b - 4.0 * c).sqrt();
    let mut t = (disc - 2.0 * b) / 2.0;
    if t < 0.0 {
        t = (-2.0 * b - disc) / 2.0;
    }
    // Central angle by the law of cosines (0x82459fb8 → 0x82a7ef40 flag 1 = acos, INFERRED).
    let theta = ((t * t - H * H - R * R) * (-1.0 / (2.0 * H * R))).acos() as f32;
    let half_pi = 1.570_795_1f32;
    let edge = half_pi - theta;
    let (mut pos, mut uv, mut uv1) = (Vec::new(), Vec::new(), Vec::new());
    for k in 0..15 {
        let kf = k as f32 / 15.0;
        let phi = (half_pi - edge) * kf + edge;
        let r = 1.0 - kf;
        for j in 0..100 {
            let az = j as f32 * 0.01 * 6.283_18;
            pos.push([phi.cos() * az.cos() * R as f32, phi.sin() * R as f32 - H as f32, -(phi.cos() * az.sin() * R as f32)]);
            uv.push([az.sin() * r * 0.5, az.cos() * r * 0.5]);
            uv1.push([r, 0.0]);
        }
    }
    pos.push([0.0, 300.0, 0.0]);
    uv.push([0.0, 0.0]);
    uv1.push([0.0, 0.0]);
    let mut idx = Vec::with_capacity(8700);
    for k in 0..14u32 {
        for j in 0..100u32 {
            let v = k * 100 + j;
            let n = k * 100 + (j + 1) % 100;
            // Game order (v, n, n+100), (v, n+100, v+100), reversed for the z mirror.
            idx.extend_from_slice(&[v, n + 100, n, v, v + 100, n + 100]);
        }
    }
    for i in 1..=100u32 {
        idx.extend_from_slice(&[1400 + i - 1, 1500, 1400 + i % 100]);
    }
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    m.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    m.insert_attribute(Mesh::ATTRIBUTE_UV_1, uv1);
    m.insert_indices(Indices::U32(idx));
    m
}

/// Close cloud dome (init 0x82D73468, VERIFIED): 16 rings r = −1..14 at elevation r/15·90°, each 100
/// segments plus a seam copy with u = 1, then the apex (1,617 vertices, 9,300 indices). The game's
/// float4 normal = (−cos az cos e, −sin e, −sin az cos e, u = j/100) and float4 pos = (−300·normal,
/// (r+1)/15). Engine attributes: NORMAL = normal.xyz, UV_0 = (pos.w, u) (patched back into the w
/// components). Built left-handed, then z is negated and the winding reversed (as the far clouds).
pub fn close_cloud_mesh() -> Mesh {
    let (mut pos, mut nrm, mut uv) = (Vec::new(), Vec::new(), Vec::new());
    for k in 0..16 {
        let e = (k as f32 - 1.0) / 15.0 * std::f32::consts::FRAC_PI_2;
        for j in 0..=100 {
            let u = j as f32 * 0.01;
            let az = u * std::f32::consts::TAU;
            let n = Vec3::new(-az.cos() * e.cos(), -e.sin(), az.sin() * e.cos());
            pos.push((-n * RADIUS).to_array());
            nrm.push(n.to_array());
            uv.push([k as f32 / 15.0, u]);
        }
    }
    pos.push([0.0, RADIUS, 0.0]);
    nrm.push([0.0, -1.0, 0.0]);
    uv.push([1.0, 0.5]);
    let mut idx = Vec::with_capacity(9300);
    for k in 0..15u32 {
        for j in 0..100u32 {
            let v = k * 101 + j;
            // Game order (v, v+1, v+102), (v, v+102, v+101), reversed for the z mirror.
            idx.extend_from_slice(&[v, v + 102, v + 1, v, v + 101, v + 102]);
        }
    }
    for i in 0..100u32 {
        idx.extend_from_slice(&[1515 + i, 1616, 1516 + i]);
    }
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    m.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nrm);
    m.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    m.insert_indices(Indices::U32(idx));
    m
}

/// NORMAL with a w component (the close clouds' u): w comes from UV_0.y.
pub(super) fn normal_w_from_uv0(p: &mut crate::Program) -> Option<()> {
    let w = p.wgsl.replace("fx_vin.a1 = vec4<f32>(input.a1, 1.0);", "fx_vin.a1 = vec4<f32>(input.a1, input.a2.y);");
    w.contains("input.a2.y").then(|| p.wgsl = w)
}

/// TEXCOORD0 with three components (the far clouds' (u, v, r)): z comes from UV_1.x.
pub(super) fn texcoord_z_from_uv1(p: &mut crate::Program) -> Option<()> {
    let w = p
        .wgsl
        .replace("    @location(2) a2: vec2<f32>,\n", "    @location(2) a2: vec2<f32>,\n    @location(3) a3: vec2<f32>,\n")
        .replace("fx_vin.a2 = vec4<f32>(input.a2, 0.0, 1.0);", "fx_vin.a2 = vec4<f32>(input.a2, input.a3.x, 1.0);");
    if !w.contains("input.a3.x") || p.attributes.contains(&3) {
        return None;
    }
    p.wgsl = w;
    p.attributes.push(3);
    p.attributes.sort();
    Some(())
}

/// Far cloud UV state (far cloud object +0x15c..0x198, ctor 0x82D75C58, VERIFIED): per layer
/// (front, back) the diffuse offset, UVSettings1 offset and UVSettings2 offset. The scales are
/// constants: 1.01 (UVSettings1), 3.2 (UVSettings2), 5 (diffuse).
#[derive(Default, Clone, Copy)]
pub struct FarCloudScroll {
    /// [layer][diffuse, settings1, settings2] UV offsets, each kept in [0, 1].
    pub off: [[Vec2; 3]; 2],
    primed: bool,
}

const FAR_SCALE1: f32 = 1.01;
const FAR_SCALE2: f32 = 3.2;
const FAR_DIFFUSE_SCALE: f32 = 5.0;
/// Diffuse (alpha detail) scroll relative to the cover maps (far cloud +0x1a0 = 0.4, VERIFIED).
const FAR_DIFFUSE_SPEED: f32 = 0.4;
/// Cloud cover threshold lerp(+0x1a8, +0x1a4, Coverage) and sharpness +0x1ac (ctor, VERIFIED).
const FAR_COVER: (f32, f32, f32) = (0.616, 0.43, 0.984);

impl FarCloudScroll {
    /// Tick 0x823e6060 (VERIFIED): offset += speed/3600 · dt (diffuse × 0.4), wrapped into [0, 1].
    /// Speeds are the TOD FarCloud[Back]Speed{X,Z}; dt INFERRED to be the frame time in seconds.
    pub fn advance(&mut self, speeds: [Vec2; 2], dt: f32) {
        let dt = dt + start_offset(&mut self.primed);
        let wrap = |v: f32| {
            let mut v = v;
            while v < 0.0 {
                v += 1.0;
            }
            while v > 1.0 {
                v -= 1.0;
            }
            v
        };
        for (layer, s) in self.off.iter_mut().zip(speeds) {
            for (i, o) in layer.iter_mut().enumerate() {
                let k = if i == 0 { FAR_DIFFUSE_SPEED } else { 1.0 };
                let v = *o + s * k / 3600.0 * dt;
                *o = Vec2::new(wrap(v.x), wrap(v.y));
            }
        }
    }
}

/// Close cloud scroll (close cloud object C = S+0x100; VERIFIED): Movement = (C+0x1d4 front phase,
/// C+0x1d0 back phase), initialised to (0.5, 0) by the ctor 0x82D75B90; UVScales C+0xd4/0xd8 = (1, 1)
/// from the block defaults 0x825C6878 and never written by the TOD evaluator (INFERRED constant).
#[derive(Clone, Copy)]
pub struct CloseCloudScroll {
    pub movement: Vec2,
    pub uv_scales: Vec2,
    primed: bool,
}

impl Default for CloseCloudScroll {
    fn default() -> Self {
        Self { movement: Vec2::new(0.5, 0.0), uv_scales: Vec2::ONE, primed: false }
    }
}

impl CloseCloudScroll {
    /// Tick 0x824733A0 (VERIFIED): phase += CloseCloud{Front,Back}Revs/3600 · dt (revolutions per
    /// hour, u = azimuth turns), wrapped into [0, 1]. dt INFERRED to be the frame time in seconds.
    pub fn advance(&mut self, revs: Vec2, dt: f32) {
        let dt = dt + start_offset(&mut self.primed);
        let mut v = self.movement + revs / 3600.0 * dt;
        for c in [&mut v.x, &mut v.y] {
            while *c < 0.0 {
                *c += 1.0;
            }
            while *c > 1.0 {
                *c -= 1.0;
            }
        }
        self.movement = v;
    }
}

/// The cloud phases are pure time since the sky was created (VERIFIED: a Pinyon frame's close phases (0.5525, 0.9667)
/// and far offsets (0.2355, 0.2355) / (0.6914, 0.0468) all give ~94 s with the 16:00 revs / speeds), so A/B shots need
/// the same elapsed time: `FH1_CLOUD_T=<seconds>` adds it to the first tick of each scroll.
fn start_offset(primed: &mut bool) -> f32 {
    if std::mem::replace(primed, true) {
        return 0.0;
    }
    std::env::var("FH1_CLOUD_T").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0)
}

/// Far cloud constants (see the comments inside).
pub(super) fn far_consts(t: &FxTimeOfDay, scroll: &FarCloudScroll, gain: Vec4) -> PartConsts {
    let m = t.minutes();
    // Draw 0x82406030 (VERIFIED). VS c5/c6/c7 = front UVSettings1/2 and diffuse (offset.xy,
    // scale), c8/c9/c10 = the back layer. PS per layer: cover = (lerp(0.616, 0.43, Coverage),
    // 0.984), dense/thin colour × intensity, alpha = (Centre × fade, Edge × fade, AlphaPower);
    // the texture-swap fade (+0x108 / +0x14c) stays 1 with CloudDefs' single set.
    let vs_layer = |o: &[Vec2; 3]| {
        [o[1].extend(FAR_SCALE1).extend(0.0), o[2].extend(FAR_SCALE2).extend(0.0), o[0].extend(FAR_DIFFUSE_SCALE).extend(0.0)]
    };
    let [f1, f2, fd] = vs_layer(&scroll.off[0]);
    let [b1, b2, bd] = vs_layer(&scroll.off[1]);
    // Channels absent from TimeOfDayA keep the defaults of 0x825C6D70 (VERIFIED values; that
    // the evaluator skips absent channels is INFERRED).
    let layer = |p: &str| {
        let f = |n: &str, d: f32| t.tod.scalar_or(&format!("FarCloud{p}{n}"), m, d);
        let c = |n: &str, d: f32| Vec3::from_array(t.tod.get_or(&format!("FarCloud{p}{n}"), m, [d; 3]));
        let cover = FAR_COVER.0 + (FAR_COVER.1 - FAR_COVER.0) * f("Coverage", 1.0);
        [
            Vec4::new(cover, FAR_COVER.2, 0.0, 0.0),
            (c("DenseColour", 0.49) * f("DenseIntensity", 1.0)).extend(0.0),
            (c("ThinColour", 0.64) * f("ThinIntensity", 1.0)).extend(0.0),
            Vec4::new(f("CentreAlpha", 0.0), f("EdgeAlpha", 0.0), f("AlphaPower", 5.0), 0.0),
        ]
    };
    let (fr, bk) = (layer(""), layer("Back"));
    PartConsts {
        vs: vec![(5, f1), (6, f2), (7, fd), (8, b1), (9, b2), (10, bd), (GAIN_REG, gain)],
        ps: vec![(1, fr[0]), (2, fr[1]), (3, fr[2]), (4, fr[3]), (5, bk[0]), (6, bk[1]), (7, bk[2]), (8, bk[3]), (GAIN_REG, gain)],
        visible: true,
    }
}

/// Close cloud main-view constants.
pub(super) fn close_consts(t: &FxTimeOfDay, close: &CloseCloudScroll, gain: Vec4) -> PartConsts {
    let m = t.minutes();
    let s = |n: &str| t.tod.scalar(n, m);
    // Draw 0x82D71228 main view (VERIFIED): VS c5 Movement = (C+0x1d4, C+0x1d0), c12 UVScales,
    // c4 SunDirection (prologue). PS c1 GradientFadeValues, c2 LayerFadeValuesMaskPowers (front
    // alpha × slot fade, which stays 1 with CloudDefs' single set), c3/c4 dense/thin ambient,
    // c7 back, c8 top, c9 bottom light (colour × intensity, power). c5/c6/c10 (back layer) are
    // set by the host but read by neither close-cloud PS.
    let c = |n: &str| Vec3::from_array(t.tod.get(&format!("CloseCloud{n}"), m));
    let f = |n: &str| s(&format!("CloseCloud{n}"));
    let sun = (tod_pos(t, "SunObjectTargetPos", m) - tod_pos(t, "SunObjectPos", m)).normalize_or(Vec3::NEG_Y);
    PartConsts {
        vs: vec![(4, sun.extend(0.0)), (5, close.movement.extend(0.0).extend(0.0)), (12, close.uv_scales.extend(0.0).extend(0.0)), (GAIN_REG, gain)],
        ps: vec![
            (1, Vec4::new(f("TopAlpha"), f("BottomAlpha"), f("AlphaCurve"), 0.0)),
            (2, Vec4::new(f("FrontAlpha"), f("BackAlpha"), f("TopMaskPower"), f("BottomMaskPower"))),
            (3, (c("DenseColour") * f("DenseColourIntensity")).extend(0.0)),
            (4, (c("ThinColour") * f("ThinColourIntensity")).extend(0.0)),
            (7, (c("BackColour") * f("BackIntensity")).extend(f("BackPower"))),
            (8, (c("TopColour") * f("TopIntensity")).extend(f("TopPower"))),
            (9, (c("BottomColour") * f("BottomIntensity")).extend(f("BottomPower"))),
            (GAIN_REG, gain),
        ],
        visible: true,
    }
}

/// Close clouds, env-cube pass constants.
pub(super) fn cube_consts(t: &FxTimeOfDay, close: &CloseCloudScroll, gain: Vec4) -> PartConsts {
    let m = t.minutes();
    let s = |n: &str| t.tod.scalar(n, m);
    let v3 = |n: &str| Vec3::from_array(t.tod.get(n, m));
    // Mode-1 draw (0x82D71228; constants VERIFIED from the cube VS 82181F78 / PS 82156910 declarations):
    // VS c4-c11 = the atmosphere block with the Cube* haze channels, c5 Movement, c12 UVScales; the VS
    // writes o1 = sqrt(atmosphere colour × gain), o1.w = sat(5w). PS: rgb = lerp(CubeClear, lerp(o1,
    // sqrt(lerp(c3, c4, Amb.y·fade)), AD.x·fade), o1.w). INFERRED: c4 = hazeDir (it takes the
    // atmosphere's haze role), gain = SkyGain × S+0x24 with S+0x24 untraced (1), CubeClear c12
    // (from 0x82DB7580, untraced) = sqrt(FogColour), what reflect.rs clears the faces to.
    let c = |n: &str| Vec3::from_array(t.tod.get(&format!("CloseCloud{n}"), m));
    let f = |n: &str| s(&format!("CloseCloud{n}"));
    let haze = (tod_pos(t, "HazeObjectTargetPos", m) - tod_pos(t, "HazeObjectPos", m)).normalize_or(Vec3::NEG_Y);
    let tangent = haze.cross(haze.cross(Vec3::Y)).normalize_or_zero();
    let fog = (v3("FogColour") * t.fog.color).max(Vec3::ZERO);
    PartConsts {
        vs: vec![
            (4, haze.extend(0.0)),
            (5, close.movement.extend(0.0).extend(0.0)),
            (6, Vec4::new(s("AtmosphereClampPower"), s("AtmosphereBottomClampPower"), 0.0, 0.0)),
            (7, tangent.extend(s("CubeAtmosphereHazeEllipticalScale"))),
            (8, Vec4::new(s("AtmosphereColourPower").max(0.01), s("CubeAtmosphereHazePower"), 0.0, 0.0)),
            (9, (v3("CubeAtmosphereHazeColour") * s("CubeAtmosphereHazeIntensity")).extend(s("AtmosphereHazeAlpha"))),
            (10, v3("AtmosphereTopColour").extend(1.0)),
            (11, v3("AtmosphereBottomColour").extend(1.0)),
            (12, close.uv_scales.extend(0.0).extend(0.0)),
            (GAIN_REG, gain),
        ],
        ps: vec![
            (2, Vec4::new(f("FrontAlpha"), f("BackAlpha"), f("TopMaskPower"), f("BottomMaskPower"))),
            (3, (c("DenseColour") * f("DenseColourIntensity")).extend(0.0)),
            (4, (c("ThinColour") * f("ThinColourIntensity")).extend(0.0)),
            (12, Vec4::new(fog.x.sqrt(), fog.y.sqrt(), fog.z.sqrt(), 0.0)),
        ],
        visible: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn far_cloud_counts() {
        let m = far_cloud_mesh();
        assert_eq!(m.count_vertices(), 1501);
        assert_eq!(m.indices().unwrap().len(), 8700);
    }

    #[test]
    fn close_cloud_counts() {
        let m = close_cloud_mesh();
        assert_eq!(m.count_vertices(), 1617);
        let Some(Indices::U32(i)) = m.indices() else { panic!() };
        assert_eq!(i.len(), 9300);
        assert!(i.iter().all(|&v| v < 1617));
    }
}
