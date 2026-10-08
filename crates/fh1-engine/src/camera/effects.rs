//! The game's camera effect stacks (`CameraPhysics.xml` / `CameraPhysicsSansEffects.xml`): per gameplay camera a list
//! of layers, each mapping one car signal through curves (and optionally noise) onto an offset, impulse, FOV or
//! post-effect channel, optionally through a spring. Port of the stack update 0x82857E50 and the layer step
//! 0x82857B00 (docs/CAMERA.md "Effect layers"). Shared by every gameplay camera: chase.rs (FollowCam/FollowCam2)
//! and views.rs (DriverCam/Hood/BumperHigh).

use bevy::prelude::*;

use fh1_formats::camera::{Curve, Layer, Mapping};

use crate::vehicle::Vehicle;

/// Signals a layer can read (input enum at 0x82074100; 0x82857230 gathers them). Wheel arrays are in the game's
/// order FL, FR, RR, RL (the SurfaceBumpOffset_* enum names).
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    /// m/s.
    pub speed: f32,
    /// Speed / the car's top speed (car vtable +1188; INFERRED: SimTopSpeed).
    pub speed_norm: f32,
    /// RPM / max RPM (car vtable +212 / +408).
    pub rpm_norm: f32,
    /// Acceleration along the car's right / forward axes over g.
    pub gs_lat: f32,
    pub gs_long: f32,
    /// Per wheel: averaging weight (car vtable +748, INFERRED ground contact), normalised combined / lateral /
    /// longitudinal slip (1 = the tyre curve's peak).
    pub wheel_weight: [f32; 4],
    pub slip: [f32; 4],
    pub slip_lat: [f32; 4],
    pub slip_long: [f32; 4],
    /// Suspension compression (car vtable +788, INFERRED fraction of travel) and surface bump height (+780, m).
    pub suspension: [f32; 4],
    pub surface_bump: [f32; 4],
    pub pitch_deg: f32,
    /// Collision events (World/Car x total/lateral/longitudinal): written by the game's collision code straight into
    /// the layer input. Our collision system does not report them yet (0).
    pub world_collision: [f32; 3],
    pub car_collision: [f32; 3],
    /// `ExternalSource` (stack +128).
    pub external: f32,
}

impl Inputs {
    /// Fills what our vehicle model provides. `pose` = the render pose rotation.
    pub fn from_vehicle(v: &Vehicle, rotation: Quat) -> Inputs {
        const G: f32 = 9.806_65;
        let right = rotation * Vec3::X;
        let fwd = rotation * Vec3::NEG_Z;
        let speed = v.speed().abs();
        // Our wheels: LF, RF, LR, RR -> game FL, FR, RR, RL.
        let order = [0, 1, 3, 2];
        let mut i = Inputs {
            speed,
            speed_norm: if v.data.reference_top_speed > 0.0 { speed / (v.data.reference_top_speed * 0.447_04) } else { 0.0 },
            rpm_norm: if v.data.rev_limit_rpm > 0.0 { v.rpm / v.data.rev_limit_rpm } else { 0.0 },
            gs_lat: v.acceleration.dot(right) / G,
            gs_long: v.acceleration.dot(fwd) / G,
            pitch_deg: fwd.y.clamp(-1.0, 1.0).asin().to_degrees(),
            ..default()
        };
        for (g, &o) in order.iter().enumerate() {
            let w = &v.wheels[o];
            i.wheel_weight[g] = if w.grounded { 1.0 } else { 0.0 };
            i.slip_lat[g] = w.norm_slip_angle;
            i.slip_long[g] = w.norm_slip;
            i.slip[g] = w.norm_slip.hypot(w.norm_slip_angle);
            let travel = v.data.suspension[o / 2].max_compress.max(0.03);
            i.suspension[g] = (1.0 - w.length / travel).clamp(-1.0, 1.0);
        }
        i
    }

    fn weighted(&self, v: &[f32; 4]) -> f32 {
        let (mut sum, mut w) = (0.0, 0.0);
        for k in 0..4 {
            sum += v[k] * self.wheel_weight[k];
            w += self.wheel_weight[k];
        }
        if w > 0.0 { sum / w } else { 0.0 }
    }

    /// 0x82857230 jump table (case = input enum).
    fn get(&self, name: &str) -> Option<f32> {
        let s = &self.suspension;
        Some(match name {
            "ConstantOne" => 1.0,
            "Gs_Lateral" => self.gs_lat,
            "Gs_Longitudinal" => self.gs_long,
            "WorldCollision" => self.world_collision[0],
            "WorldCollision_Lateral" => self.world_collision[1],
            "WorldCollision_Longitudinal" => self.world_collision[2],
            "CarCollision" => self.car_collision[0],
            "CarCollision_Lateral" => self.car_collision[1],
            "CarCollision_Longitudinal" => self.car_collision[2],
            "Slip" => self.weighted(&self.slip),
            "Slip_Lateral" => self.weighted(&self.slip_lat),
            "Slip_Longitudinal" => self.weighted(&self.slip_long),
            "Speed" => self.speed_norm,
            "SpeedMPH" => self.speed * 2.236_94,
            "RPM" => self.rpm_norm,
            // (FL + RL - FR - RR) / 2 and (RR + RL - FL - FR) / 2.
            "SuspensionBias_Lateral" => 0.5 * (s[0] + s[3] - s[1] - s[2]),
            "SuspensionBias_Longitudinal" => 0.5 * (s[2] + s[3] - s[0] - s[1]),
            "SurfaceBumpOffset_FL" => self.surface_bump[0] * self.wheel_weight[0] * 12.5,
            "SurfaceBumpOffset_FR" => self.surface_bump[1] * self.wheel_weight[1] * 12.5,
            "SurfaceBumpOffset_RR" => self.surface_bump[2] * self.wheel_weight[2] * 12.5,
            "SurfaceBumpOffset_RL" => self.surface_bump[3] * self.wheel_weight[3] * 12.5,
            // 1 at rest, 0 from 2 m/s.
            "OneWhenStationary" => ((2.0 - self.speed) * 0.5).max(0.0),
            "ExternalSource" => self.external,
            "PitchAngleDegrees" => self.pitch_deg,
            // WindBuffer (nearest car), shoulder / head tracking (Kinect): not modelled.
            _ => return None,
        })
    }
}

/// Summed outputs (stack +16..+112), converted to our axes: +Y up, front -Z, right +X. Angles in radians.
#[derive(Clone, Copy, Debug, Default)]
pub struct Effects {
    /// Euler angles about our Y (yaw), X (pitch) and Z (roll) axes, right-handed, in the car's frame.
    pub car_ypr: Vec3,
    /// Translation in the car's frame (m).
    pub car_xyz: Vec3,
    /// The same in the camera's own frame.
    pub cam_ypr: Vec3,
    pub cam_xyz: Vec3,
    /// Added to the camera's FOV (radians).
    pub fov: f32,
    pub saturation: f32,
    pub tone: Vec3,
    pub sepia: f32,
    pub blur: f32,
    pub vignette: f32,
    /// Cockpit steering-wheel shake (radians).
    pub steering_wheel: f32,
}

impl Effects {
    /// Rotation for a YPR triple (yaw, then pitch, then roll; INFERRED order).
    pub fn rotation(ypr: Vec3) -> Quat {
        Quat::from_euler(EulerRot::YXZ, ypr.x, ypr.y, ypr.z)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Out {
    CarYpr,
    CarXyz,
    CamYpr,
    CamXyz,
    Fov,
    Saturation,
    Tone,
    Sepia,
    Blur,
    Vignette,
    SteeringWheel,
}

struct State {
    layer: Layer,
    out: Option<Out>,
    impulse: bool,
    /// Input this step and last step (derivative), attack/decay follower.
    prev: f32,
    follow: f32,
    /// Noise phase (double in the game).
    phase: f64,
    /// Time since the last impulse.
    timer: f64,
    pos: Vec3,
    vel: Vec3,
}

/// One camera's layer stack.
pub struct Stack {
    layers: Vec<State>,
}

impl Stack {
    pub fn new(layers: &[Layer]) -> Stack {
        let layers = layers
            .iter()
            .filter(|l| l.enabled)
            .map(|l| {
                let (out, impulse) = match l.output.as_str() {
                    "CarSpaceYPROffset" => (Some(Out::CarYpr), false),
                    "CarSpaceYPRImpulse" => (Some(Out::CarYpr), true),
                    "CarSpaceXYZOffset" => (Some(Out::CarXyz), false),
                    "CarSpaceXYZImpulse" => (Some(Out::CarXyz), true),
                    "CameraSpaceYPROffset" => (Some(Out::CamYpr), false),
                    "CameraSpaceYPRImpulse" => (Some(Out::CamYpr), true),
                    "CameraSpaceXYZOffset" => (Some(Out::CamXyz), false),
                    "CameraSpaceXYZImpulse" => (Some(Out::CamXyz), true),
                    "FOV" => (Some(Out::Fov), false),
                    "Saturation" => (Some(Out::Saturation), false),
                    "Tone" => (Some(Out::Tone), false),
                    "Sepia" => (Some(Out::Sepia), false),
                    "Blur" => (Some(Out::Blur), false),
                    "Vignette" => (Some(Out::Vignette), false),
                    "SteeringWheelAngle" => (Some(Out::SteeringWheel), false),
                    _ => (None, false),
                };
                State { layer: l.clone(), out, impulse, prev: 0.0, follow: 0.0, phase: 0.0, timer: 0.0, pos: Vec3::ZERO, vel: Vec3::ZERO }
            })
            .collect();
        Stack { layers }
    }

    /// Clears springs and followers (camera cut / mode change).
    pub fn reset(&mut self) {
        for s in &mut self.layers {
            (s.prev, s.follow, s.timer, s.pos, s.vel) = (0.0, 0.0, 0.0, Vec3::ZERO, Vec3::ZERO);
        }
    }

    pub fn step(&mut self, inputs: &Inputs, dt: f32) -> Effects {
        // 0x82857E50: dt clamped to 1/15 s.
        let dt = dt.abs().min(1.0 / 15.0);
        let mut e = Effects::default();
        let mut car_ypr = Vec3::ZERO;
        let mut car_xyz = Vec3::ZERO;
        let mut cam_ypr = Vec3::ZERO;
        let mut cam_xyz = Vec3::ZERO;
        for s in &mut self.layers {
            if dt == 0.0 {
                break;
            }
            let Some(out) = s.out else { continue };
            let Some(raw) = inputs.get(&s.layer.input) else { continue };
            s.step(raw, dt, out);
            let p = s.pos;
            match out {
                Out::CarYpr => car_ypr += p,
                Out::CarXyz => car_xyz += p,
                Out::CamYpr => cam_ypr += p,
                Out::CamXyz => cam_xyz += p,
                Out::Fov => e.fov += p.x,
                Out::Saturation => e.saturation += p.x,
                Out::Tone => e.tone += p,
                Out::Sepia => e.sepia += p.x,
                Out::Blur => e.blur += p.x,
                Out::Vignette => e.vignette += p.x,
                Out::SteeringWheel => e.steering_wheel += p.x,
            }
        }
        // Game frame (+Z front, left-handed, D3D rotation matrices: + yaw turns right, + pitch noses down) -> ours:
        // z negated, yaw and pitch negated, roll kept (INFERRED signs; the Pinyon probe checks them).
        let ypr = |v: Vec3| Vec3::new(-v.x, -v.y, v.z);
        e.car_ypr = ypr(car_ypr);
        e.cam_ypr = ypr(cam_ypr);
        e.car_xyz = Vec3::new(car_xyz.x, car_xyz.y, -car_xyz.z);
        e.cam_xyz = Vec3::new(cam_xyz.x, cam_xyz.y, -cam_xyz.z);
        e
    }
}

impl State {
    /// 0x82857B00.
    fn step(&mut self, raw: f32, dt: f32, out: Out) {
        let l = &self.layer;
        let collision = l.input.contains("Collision");
        let mut x = raw;
        // Derivative: per-frame difference, scaled to 60 Hz (collision inputs excluded).
        if !collision && l.take_derivative {
            let d = x - self.prev;
            self.prev = x;
            x = d * (1.0 / (dt * 60.0)).clamp(-1.0, 1.0);
        }
        x = x.clamp(l.clamp.0.min(l.clamp.1), l.clamp.0.max(l.clamp.1));
        // Magnitude follower: rises at most `attack` (per frame for collision inputs, else per second) and falls at
        // most `decay` per second.
        if let Some((attack, decay)) = l.attack_decay {
            x = x.abs();
            let up = if collision { attack } else { attack * dt };
            let delta = (x - self.follow).clamp(-decay * dt, up);
            self.follow += delta;
            x = self.follow;
        }
        // Noise phase advances by NoiseFreq x freq curve(x); the param curve scales the output vector.
        let freq = l.noise_freq_curve.as_ref().map_or(1.0, |c| curve(c, x));
        self.phase += f64::from(l.noise_freq * freq * dt);
        let noise = noise(&l.noise, self.phase as f32, l.noise_seed, l.noise_range);
        let amount = curve(&l.param_curve, x) * noise;
        // Param vector: angles (YPR, FOV, steering wheel) are degrees.
        let mut vec = Vec3::new(l.param[0], l.param[1], l.param[2]);
        if matches!(out, Out::CarYpr | Out::CamYpr | Out::Fov | Out::SteeringWheel) {
            vec *= std::f32::consts::PI / 180.0;
        }
        let target = if self.impulse {
            // Kick the spring once the delay has passed and the input is big enough.
            self.timer += f64::from(dt);
            if self.timer >= f64::from(l.impulse_delay) && x.abs() >= l.impulse_cutoff {
                self.vel += vec * amount;
                self.timer = 0.0;
            }
            Vec3::ZERO
        } else {
            vec * amount
        };
        match l.spring {
            Some((k, d_percent)) if self.impulse || k > 0.0 => {
                let c = 2.0 * k.max(0.0).sqrt() * d_percent;
                spring(dt, k, c, &mut self.pos, &mut self.vel, target);
            }
            _ => {
                self.pos = target;
                self.vel = Vec3::ZERO;
            }
        }
    }
}

/// 0x82850B38: implicit spring step towards `target` (unit mass).
fn spring(dt: f32, k: f32, c: f32, pos: &mut Vec3, vel: &mut Vec3, target: Vec3) {
    if dt <= 0.0 {
        return;
    }
    let old = *pos;
    *pos += *vel * dt;
    let den = 1.0 + c * dt + k * dt * dt;
    *vel += (k * dt * (target - old) - k * dt * dt * *vel - c * dt * *vel) / den;
}

/// 0x8284D0D8 (+ piecewise linear 0x82CB84B8): mapping enum 0 ConstantOne, 1 Linear, 2 Abs, 3/4 three/five points.
fn curve(c: &Curve, x: f32) -> f32 {
    match c.mapping {
        Mapping::ConstantOne => 1.0,
        Mapping::Linear => x,
        // The parser maps the game's `Abs` to Other (no other value occurs).
        Mapping::Other => x.abs(),
        Mapping::ThreePoint | Mapping::FivePoint => {
            let pts = &c.points;
            if pts.is_empty() {
                return 1.0;
            }
            if c.mirrored {
                // Inside ±in0: the straight line through (-in0, -out0) and (in0, out0); outside: odd extension.
                let (i0, o0) = pts[0];
                if (-i0..=i0).contains(&x) && i0 > 0.0 {
                    return o0 * x / i0;
                }
                if x < 0.0 {
                    return -piecewise(pts, -x);
                }
            }
            piecewise(pts, x)
        }
    }
}

fn piecewise(pts: &[(f32, f32)], x: f32) -> f32 {
    for i in 1..pts.len() {
        let (a, b) = (pts[i - 1], pts[i]);
        if x <= b.0 {
            let (lo, hi) = (a.0.min(b.0), a.0.max(b.0));
            if x <= lo {
                return a.1;
            }
            return a.1 + (x - lo) / (hi - lo) * (b.1 - a.1);
        }
    }
    pts.last().map_or(1.0, |p| p.1)
}

/// 0x8284D500: noise in [-1, 1] mapped onto the layer's output range; `None` = 1. Inputs: x = the phase, y = the seed.
fn noise(kind: &str, phase: f32, seed: f32, range: (f32, f32)) -> f32 {
    let v = match kind {
        "Sin" => (seed + phase).sin(),
        "Perlin" => perlin(phase, seed),
        "Simplex" => simplex(phase, seed),
        "HeightGrid" => height_grid(phase, seed),
        _ => return 1.0,
    };
    range.0 + (range.1 - range.0) * (v + 1.0) * 0.5
}

/// Ken Perlin's reference permutation (the game's table at 0x82231140 holds it twice).
const PERM: [u8; 256] = [
    151, 160, 137, 91, 90, 15, 131, 13, 201, 95, 96, 53, 194, 233, 7, 225, 140, 36, 103, 30, 69, 142, 8, 99, 37, 240, 21, 10, 23,
    190, 6, 148, 247, 120, 234, 75, 0, 26, 197, 62, 94, 252, 219, 203, 117, 35, 11, 32, 57, 177, 33, 88, 237, 149, 56, 87, 174, 20,
    125, 136, 171, 168, 68, 175, 74, 165, 71, 134, 139, 48, 27, 166, 77, 146, 158, 231, 83, 111, 229, 122, 60, 211, 133, 230, 220, 105, 92,
    41, 55, 46, 245, 40, 244, 102, 143, 54, 65, 25, 63, 161, 1, 216, 80, 73, 209, 76, 132, 187, 208, 89, 18, 169, 200, 196, 135, 130,
    116, 188, 159, 86, 164, 100, 109, 198, 173, 186, 3, 64, 52, 217, 226, 250, 124, 123, 5, 202, 38, 147, 118, 126, 255, 82, 85, 212, 207,
    206, 59, 227, 47, 16, 58, 17, 182, 189, 28, 42, 223, 183, 170, 213, 119, 248, 152, 2, 44, 154, 163, 70, 221, 153, 101, 155, 167, 43,
    172, 9, 129, 22, 39, 253, 19, 98, 108, 110, 79, 113, 224, 232, 178, 185, 112, 104, 218, 246, 97, 228, 251, 34, 242, 193, 238, 210, 144,
    12, 191, 179, 162, 241, 81, 51, 145, 235, 249, 14, 239, 107, 49, 192, 214, 31, 181, 199, 106, 157, 184, 84, 204, 176, 115, 121, 50, 45,
    127, 4, 150, 254, 138, 236, 205, 93, 222, 114, 67, 29, 24, 72, 243, 141, 128, 195, 78, 66, 215, 61, 156, 180,
];

fn perm(i: usize) -> usize {
    PERM[i & 255] as usize
}

/// 0x82CB9998: 2-D improved Perlin noise with axis-only gradients (hash bit 1 picks the axis, bit 0 the sign),
/// quintic fade, scaled by 1.8681990 (0x82231A38).
fn perlin(x: f32, y: f32) -> f32 {
    let (xf, yf) = (x.floor(), y.floor());
    let (fx, fy) = (x - xf, y - yf);
    let (xi, yi) = ((xf as i32 & 255) as usize, (yf as i32 & 255) as usize);
    let fade = |t: f32| ((t * 6.0 - 15.0) * t + 10.0) * t * t * t;
    let (u, v) = (fade(fx), fade(fy));
    let grad = |h: usize, dx: f32, dy: f32| {
        let g = if h & 2 == 0 { dx } else { dy };
        if h & 1 == 0 { g } else { -g }
    };
    let (a, b) = (perm(xi), perm(xi + 1));
    let g00 = grad(perm(a + yi), fx, fy);
    let g10 = grad(perm(b + yi), fx - 1.0, fy);
    let g01 = grad(perm(a + yi + 1), fx, fy - 1.0);
    let g11 = grad(perm(b + yi + 1), fx - 1.0, fy - 1.0);
    let x0 = g00 + u * (g10 - g00);
    let x1 = g01 + u * (g11 - g01);
    (x0 + v * (x1 - x0)) * 1.868_199
}

/// 0x82CB9B78: 2-D simplex noise (Gustavson's: 12 grad3 gradients via perm mod 12, falloff (0.5 - r²)⁴, x70), in
/// doubles as the game computes it.
fn simplex(x: f32, y: f32) -> f32 {
    const GRAD: [(f64, f64); 12] =
        [(1.0, 1.0), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0), (1.0, 0.0), (-1.0, 0.0), (1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0), (0.0, 1.0), (0.0, -1.0)];
    let (x, y) = (f64::from(x), f64::from(y));
    let f2 = 0.366_025_388_240_814_2; // 0x82231A58 (f32-rounded (√3−1)/2)
    let g2 = 0.211_324_870_586_395_26; // 0x82231A50
    let floor = |v: f64| (if v >= 0.0 { v } else { v - 1.0 }).trunc();
    let s = (x + y) * f2;
    let (i, j) = (floor(x + s), floor(y + s));
    let t = (i + j) * g2;
    let (x0, y0) = (x - (i - t), y - (j - t));
    let (i1, j1) = if x0 > y0 { (1, 0) } else { (0, 1) };
    let (x1, y1) = (x0 - i1 as f64 + g2, y0 - j1 as f64 + g2);
    let (x2, y2) = (x0 - 1.0 + 2.0 * g2, y0 - 1.0 + 2.0 * g2);
    let (ii, jj) = ((i as i64 & 255) as usize, (j as i64 & 255) as usize);
    let gi = |a: usize, b: usize| GRAD[perm(a + perm(b)) % 12];
    let corner = |g: (f64, f64), dx: f64, dy: f64| {
        let t = 0.5 - dx * dx - dy * dy;
        if t >= 0.0 { t * t * t * t * (g.0 * dx + g.1 * dy) } else { 0.0 }
    };
    let n = corner(gi(ii, jj), x0, y0) + corner(gi(ii + i1, jj + j1), x1, y1) + corner(gi(ii + 1, jj + 1), x2, y2);
    (n * 70.0) as f32
}

/// 0x82CBA770: Catmull-Rom interpolation over a 4×4 grid of hashed values. Each value = Bob Jenkins' one-at-a-time
/// hash of the f32 bit patterns of (cell x + {-1,0,1,2}) and (cell y + ...), as a signed int / 2³¹; result x0.7575.
fn height_grid(x: f32, y: f32) -> f32 {
    let floor = |v: f32| (if v >= 0.0 { v } else { v - 1.0 }).trunc();
    let (cx, cy) = (floor(x), floor(y));
    let (fx, fy) = (x - cx, y - cy);
    let weights = |t: f32| {
        let (t2, t3) = (t * t, t * t * t);
        [0.5 * (2.0 * t2 - t3 - t), 0.5 * (3.0 * t3 - 5.0 * t2 + 2.0), 0.5 * (4.0 * t2 - 3.0 * t3 + t), 0.5 * (t3 - t2)]
    };
    let (wx, wy) = (weights(fx), weights(fy));
    let off = [-1.0f32, 0.0, 1.0, 2.0];
    let hash = |a: u32, b: u32| {
        let mut h = a;
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        h = h.wrapping_add(b);
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        h = h.wrapping_add(h << 3);
        h ^= h >> 11;
        h = h.wrapping_add(h << 15);
        h as i32 as f32 / 2_147_483_648.0
    };
    let mut sum = 0.0;
    for (r, oy) in off.iter().enumerate() {
        let yb = (cy + oy).to_bits();
        let row: f32 = off.iter().enumerate().map(|(c, ox)| hash((cx + ox).to_bits(), yb) * wx[c]).sum();
        sum += row * wy[r];
    }
    sum * 0.7575
}
