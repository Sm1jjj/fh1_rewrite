//! Our own camera shots for the car-relative cutscene cams (the decoded CarSpace / PartSpace keys clip through the car and
//! frame it wrong on our cars). Pure maths on `bevy::math` only, so the tests run without an app.
//!
//! A car cam of `duration` s is cut into segments of 4.5..8 s (one segment for a short cam); every segment gets one shot
//! from [`Shot::ALL`], picked deterministically per cutscene: a hashed start offset per cutscene, then the shots rotate in
//! order (never the same shot twice in a row), each with a hashed side (left / right) and small variations. Everything is in
//! the CAR frame: the model origin (body bottom-centre), +Y up, front -Z, X right; the box is `CarData::bbox` (model space).
//!
//! Guarantees here (frame-local): the eye is outside the car box grown by [`MARGIN`] and above the box bottom; the look-at
//! point is the box centre slightly raised; the FOV is within [`FOV_MIN`]..[`FOV_MAX`]. The world pass in `cutscene.rs`
//! adds the ground clamp and the occlusion pull-in, then re-applies the box test in the car's true pose.

use bevy::math::Vec3;

/// Clearance kept between the eye and the car's bounding box (m).
pub const MARGIN: f32 = 0.6;
/// Vertical FOV range (degrees).
pub const FOV_MIN: f32 = 28.0;
pub const FOV_MAX: f32 = 58.0;
/// Segment length range for long cams (s).
const SEG_MIN: f32 = 4.5;
const SEG_MAX: f32 = 8.0;

/// The car's bounding box in its own frame (model space): min, max.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CarBox {
    pub min: Vec3,
    pub max: Vec3,
}

impl CarBox {
    /// A sane box from `CarData::bbox` (a degenerate or tiny box becomes a 4.4 x 1.8 x 1.4 m car).
    pub fn new(min: Vec3, max: Vec3) -> CarBox {
        let (lo, hi) = (min.min(max), min.max(max));
        let size = hi - lo;
        if !(size.x > 0.5 && size.y > 0.4 && size.z > 1.5) || !size.is_finite() {
            return CarBox { min: Vec3::new(-0.9, 0.0, -2.2), max: Vec3::new(0.9, 1.4, 2.2) };
        }
        CarBox { min: lo, max: hi }
    }

    pub fn center(&self) -> Vec3 {
        0.5 * (self.min + self.max)
    }

    pub fn size(&self) -> Vec3 {
        self.max - self.min
    }

    /// The point a shot looks at: the box centre, raised by 15 % of the height.
    pub fn look_at(&self) -> Vec3 {
        self.center() + Vec3::Y * (0.15 * self.size().y)
    }

    /// Whether `p` is inside the box grown by `m`.
    pub fn inside(&self, p: Vec3, m: f32) -> bool {
        p.cmpgt(self.min - Vec3::splat(m)).all() && p.cmplt(self.max + Vec3::splat(m)).all()
    }

    /// `p` moved out of the box grown by `m` through the nearest side face or the top (never the bottom: below the car is
    /// the road). Unchanged when already outside.
    pub fn push_out(&self, p: Vec3, m: f32) -> Vec3 {
        if !self.inside(p, m) {
            return p;
        }
        let (lo, hi) = (self.min - Vec3::splat(m), self.max + Vec3::splat(m));
        // Exit distances: -x, +x, top, -z, +z.
        let exits = [(p.x - lo.x, 0), (hi.x - p.x, 1), (hi.y - p.y, 2), (p.z - lo.z, 3), (hi.z - p.z, 4)];
        let (_, side) = exits.iter().copied().fold((f32::INFINITY, 2), |b, e| if e.0 < b.0 { e } else { b });
        let eps = 1e-3;
        let mut q = p;
        match side {
            0 => q.x = lo.x - eps,
            1 => q.x = hi.x + eps,
            2 => q.y = hi.y + eps,
            3 => q.z = lo.z - eps,
            _ => q.z = hi.z + eps,
        }
        q
    }
}

/// The authored shot types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shot {
    /// Slow orbit at eye height around the car.
    Orbit,
    /// Low front three-quarter dolly in towards the car.
    FrontDolly,
    /// Side tracking: the camera slides along the car's flank, panning with it.
    SideTrack,
    /// Rising crane: low at the rear quarter, rising and pulling back to reveal the car from above.
    Crane,
    /// Rear chase pull-back: behind the car, pulling away and up.
    RearPull,
}

impl Shot {
    pub const ALL: [Shot; 5] = [Shot::Orbit, Shot::FrontDolly, Shot::SideTrack, Shot::Crane, Shot::RearPull];
}

/// One camera pose in the car frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShotPose {
    pub eye: Vec3,
    pub look: Vec3,
    /// Vertical FOV, degrees.
    pub fov: f32,
}

/// FNV-1a of the cutscene name (stable shot picks per cutscene).
pub fn hash(s: &str) -> u32 {
    s.bytes().fold(0x811C_9DC5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

fn mix(a: u32, b: u32) -> u32 {
    let mut x = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^ (x >> 12)
}

/// 0..1 from a hash.
fn unit(h: u32) -> f32 {
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Segments of a car cam of `duration` s: (count, length).
pub fn segments(duration: f32) -> (u32, f32) {
    if !(duration > SEG_MAX) {
        return (1, duration.max(0.0));
    }
    let n = (duration / (0.5 * (SEG_MIN + SEG_MAX))).round().max(1.0);
    let n = n.max((duration / SEG_MAX).ceil());
    (n as u32, duration / n)
}

/// Shot and seed of global segment `k` (segments counted over the cutscene's car cams in order) of cutscene `name`.
/// Consecutive segments never repeat a shot.
pub fn pick(name: &str, k: u32) -> (Shot, u32) {
    let h = hash(name);
    let shot = forced().unwrap_or(Shot::ALL[((h % 5) + k) as usize % 5]);
    (shot, mix(h, k))
}

/// Dev preview: `FH1_CUTSCENE_SHOT=orbit|dolly|side|crane|rear` plays every car cam as that shot (with `FH1_CUTSCENE=<name>`).
fn forced() -> Option<Shot> {
    static F: std::sync::OnceLock<Option<Shot>> = std::sync::OnceLock::new();
    *F.get_or_init(|| match std::env::var("FH1_CUTSCENE_SHOT").map(|v| v.to_ascii_lowercase()).as_deref() {
        Ok("orbit") => Some(Shot::Orbit),
        Ok("dolly") | Ok("front") => Some(Shot::FrontDolly),
        Ok("side") | Ok("track") => Some(Shot::SideTrack),
        Ok("crane") | Ok("reveal") => Some(Shot::Crane),
        Ok("rear") | Ok("chase") => Some(Shot::RearPull),
        _ => None,
    })
}

/// Ease in-out (smootherstep): zero velocity at both ends, so cuts start and end calm.
pub fn ease(u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    u * u * u * (u * (u * 6.0 - 15.0) + 10.0)
}

/// A point at `angle` (radians, 0 = straight ahead of the car, positive = towards +X / the car's right), `dist` from the
/// box centre in the ground plane, at `height` above the box bottom.
fn around(b: &CarBox, angle: f32, dist: f32, height: f32) -> Vec3 {
    let c = b.center();
    Vec3::new(c.x + angle.sin() * dist, b.min.y + height, c.z - angle.cos() * dist)
}

/// The pose of `shot` at segment time `u` (0..1) for a segment of `seg_len` s.
pub fn pose(shot: Shot, seed: u32, u: f32, seg_len: f32, b: &CarBox) -> ShotPose {
    let size = b.size();
    let (half_len, half_w, h) = (0.5 * size.z, 0.5 * size.x, size.y);
    let side = if seed & 1 == 0 { 1.0 } else { -1.0 };
    let r1 = unit(mix(seed, 1));
    let r2 = unit(mix(seed, 2));
    let e = ease(u);
    let look = b.look_at();
    let deg = std::f32::consts::PI / 180.0;
    let p = match shot {
        Shot::Orbit => {
            // ~9 degrees/s, 30..80 degrees per segment, starting at a front or rear quarter.
            let sweep = (9.0 * seg_len).clamp(30.0, 80.0) * deg;
            let a0 = side * (30.0 + 110.0 * r1) * deg;
            let a = a0 + side * sweep * e;
            let dist = half_len + 3.2 + 1.5 * r2;
            ShotPose { eye: around(b, a, dist, 0.65 * h + 0.6), look, fov: 40.0 }
        }
        Shot::FrontDolly => {
            let a = side * (28.0 + 14.0 * r1) * deg;
            let dist = half_len + 6.0 - 2.6 * e;
            let low = 0.45 + 0.15 * r2;
            ShotPose { eye: around(b, a, dist, low), look: look - Vec3::Y * (0.05 * h), fov: 48.0 - 8.0 * e }
        }
        Shot::SideTrack => {
            let x = b.center().x + side * (half_w + 3.4 + 0.8 * r1);
            let z0 = b.center().z - (0.55 + 0.25 * r2) * half_len;
            let z1 = b.center().z + 0.6 * half_len;
            let eye = Vec3::new(x, b.min.y + 0.55 * h + 0.35, z0 + (z1 - z0) * e);
            ShotPose { eye, look, fov: 36.0 }
        }
        Shot::Crane => {
            let a = side * (125.0 + 25.0 * r1) * deg;
            let dist = half_len + 3.5 + 3.5 * e;
            let height = 0.5 + (h + 3.0 + 1.5 * r2) * e;
            ShotPose { eye: around(b, a, dist, height), look: look + Vec3::Y * (0.1 * h * e), fov: 44.0 + 6.0 * e }
        }
        Shot::RearPull => {
            let a = (180.0 - side * (8.0 + 10.0 * r1)) * deg;
            let dist = half_len + 2.6 + 6.5 * e;
            let height = 1.1 + 1.2 * e + 0.3 * r2;
            ShotPose { eye: around(b, a, dist, height), look, fov: 46.0 + 8.0 * e }
        }
    };
    guard(p, b)
}

/// The frame-local guarantees: eye outside the grown box and above the box bottom, FOV in range, finite numbers.
pub fn guard(mut p: ShotPose, b: &CarBox) -> ShotPose {
    if !p.eye.is_finite() {
        p.eye = around(b, 0.6, 0.5 * b.size().z + 4.0, 1.3);
    }
    p.eye.y = p.eye.y.max(b.min.y + 0.3);
    p.eye = b.push_out(p.eye, MARGIN);
    if !p.look.is_finite() || p.look.distance_squared(p.eye) < 1e-4 {
        p.look = b.look_at();
    }
    p.fov = if p.fov.is_finite() { p.fov.clamp(FOV_MIN, FOV_MAX) } else { 40.0 };
    p
}

/// The pose at local time `local` of a car cam lasting `duration` s whose first segment is global segment `k0`.
pub fn cam_pose(name: &str, k0: u32, local: f32, duration: f32, b: &CarBox) -> ShotPose {
    let (n, len) = segments(duration);
    let (seg, u) = if len > 1e-3 {
        let s = ((local / len).floor().max(0.0) as u32).min(n - 1);
        (s, ((local - s as f32 * len) / len).clamp(0.0, 1.0))
    } else {
        (0, 0.0)
    };
    let (shot, seed) = pick(name, k0 + seg);
    pose(shot, seed, u, len.max(1e-3), b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxes() -> Vec<CarBox> {
        vec![
            CarBox::new(Vec3::new(-0.9, 0.0, -2.2), Vec3::new(0.9, 1.35, 2.2)),
            CarBox::new(Vec3::new(-1.05, 0.0, -2.5), Vec3::new(1.05, 1.1, 2.4)),
            CarBox::new(Vec3::new(-1.0, 0.0, -2.9), Vec3::new(1.0, 2.0, 3.0)),
            CarBox::new(Vec3::new(-0.8, 0.0, -1.8), Vec3::new(0.8, 1.5, 1.8)),
            // A degenerate box falls back to a default car.
            CarBox::new(Vec3::ZERO, Vec3::ZERO),
        ]
    }

    #[test]
    fn eye_never_enters_the_car_and_stays_up() {
        for b in boxes() {
            for k in 0..60u32 {
                let (shot, seed) = pick("Opening_Cutscene", k);
                for len in [0.5f32, 2.5, 6.0, 8.0] {
                    for i in 0..=100 {
                        let p = pose(shot, seed, i as f32 / 100.0, len, &b);
                        assert!(!b.inside(p.eye, MARGIN - 1e-3), "{shot:?} seed {seed} u {i} eye {:?} box {b:?}", p.eye);
                        assert!(p.eye.y >= b.min.y + 0.3 - 1e-4);
                        assert!((FOV_MIN..=FOV_MAX).contains(&p.fov));
                        assert!(p.eye.is_finite() && p.look.is_finite());
                        // The look-at point is the car's (inside its box, above the centre line).
                        assert!(b.inside(p.look, 0.05));
                    }
                }
            }
        }
    }

    #[test]
    fn shots_move_smoothly_and_rotate() {
        let b = boxes()[0];
        for k in 0..20u32 {
            let (shot, seed) = pick("ANIM_NEMESIS_ali_howard", k);
            let (next, _) = pick("ANIM_NEMESIS_ali_howard", k + 1);
            assert_ne!(shot, next, "consecutive segments repeat {shot:?}");
            let mut last = pose(shot, seed, 0.0, 6.0, &b).eye;
            for i in 1..=600 {
                let e = pose(shot, seed, i as f32 / 600.0, 6.0, &b).eye;
                // 6 s at 100 fps: no jump over 0.15 m per frame.
                assert!(e.distance(last) < 0.15, "{shot:?} jump {} at {i}", e.distance(last));
                last = e;
            }
            // Calm ends (smootherstep).
            let a = pose(shot, seed, 0.0, 6.0, &b).eye;
            let a2 = pose(shot, seed, 0.01, 6.0, &b).eye;
            assert!(a.distance(a2) < 0.01);
        }
        // Every shot type shows up.
        let kinds: Vec<Shot> = (0..5).map(|k| pick("x", k).0).collect();
        for s in Shot::ALL {
            assert!(kinds.contains(&s));
        }
    }

    #[test]
    fn segments_and_cam_pose() {
        assert_eq!(segments(2.5), (1, 2.5));
        assert_eq!(segments(0.0), (1, 0.0));
        let (n, len) = segments(39.0);
        assert!(n >= 5 && (SEG_MIN - 0.01..=SEG_MAX + 0.01).contains(&len), "{n} {len}");
        let (n, len) = segments(1200.0);
        assert!((len * n as f32 - 1200.0).abs() < 0.1 && len <= SEG_MAX + 1e-3);
        let b = boxes()[0];
        // Zero-length cams hold the first frame; out-of-range times clamp.
        let p = cam_pose("x", 3, 5.0, 0.0, &b);
        assert!(p.eye.is_finite());
        let q = cam_pose("x", 3, 99.0, 6.0, &b);
        assert_eq!(q, cam_pose("x", 3, 6.0, 6.0, &b));
    }

    #[test]
    fn push_out_leaves_through_side_or_top() {
        let b = boxes()[0];
        let q = b.push_out(Vec3::new(0.0, 0.2, 0.0), MARGIN);
        assert!(!b.inside(q, MARGIN - 1e-3) && q.y >= 0.2);
        let q = b.push_out(Vec3::new(0.0, 1.0, -2.5), MARGIN);
        assert!(q.z < -2.2 - MARGIN + 1e-2);
    }
}
