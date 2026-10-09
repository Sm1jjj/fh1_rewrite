//! Car-vs-car contact: the MAXData Flags 2 spheres (the big body shapes; `CarData::car_spheres`) of two cars pushed apart
//! with an impulse, the same rigid-body response as the body-vs-world contact in vehicle.rs. Our rule (the game's car-car
//! solver is not decoded): restitution 0.2, friction 0.3, deepest overlapping pair per call, called once per substep or tick.

use bevy::math::{Quat, Vec3};

use super::Vehicle;

const RESTITUTION: f32 = 0.2;
const FRICTION: f32 = 0.3;

/// Contact response for one pair kind (ai/race_physics.rs). `DEFAULT` = the old numbers. `ang_scale` scales the angular part
/// of the impulse response about the horizontal axes (pitch / roll), `yaw_scale` about the vertical axis; both cars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactParams {
    pub restitution: f32,
    pub friction: f32,
    pub ang_scale: f32,
    pub yaw_scale: f32,
}

impl ContactParams {
    pub const DEFAULT: Self = Self { restitution: RESTITUTION, friction: FRICTION, ang_scale: 1.0, yaw_scale: 1.0 };
}

fn scale_ang(w: Vec3, p: &ContactParams) -> Vec3 {
    Vec3::new(w.x * p.ang_scale, w.y * p.yaw_scale, w.z * p.ang_scale)
}

/// One contact between the two cars, if any: (point, normal from `a` towards `b`, depth). `None` when their bounding
/// circles don't meet (the cheap test every pair goes through).
pub fn overlap(a: &Vehicle, b: &Vehicle) -> Option<(Vec3, Vec3, f32)> {
    let reach = a.data.car_spheres_reach() + b.data.car_spheres_reach();
    if a.position.distance_squared(b.position) > reach * reach {
        return None;
    }
    let mut best: Option<(Vec3, Vec3, f32)> = None;
    for &(ca, ra) in &a.data.car_spheres {
        let pa = a.position + a.rotation * (ca - a.cg_model);
        for &(cb, rb) in &b.data.car_spheres {
            let pb = b.position + b.rotation * (cb - b.cg_model);
            let d = pb - pa;
            let dist = d.length();
            let depth = ra + rb - dist;
            if depth > 0.0 && best.is_none_or(|x| depth > x.2) {
                // Horizontal-biased normal: stacked spheres would otherwise launch the cars upwards.
                let n = Vec3::new(d.x, d.y * 0.25, d.z).try_normalize().unwrap_or((b.position - a.position).normalize_or(Vec3::X));
                best = Some((pa + d * (ra / (ra + rb).max(1e-4)), n, depth));
            }
        }
    }
    best
}

/// Resolve the contact between two cars (if they touch). Returns the closing speed of the hit (m/s, 0 if none) for sounds,
/// damage or the AI's "was hit" logic.
pub fn collide(a: &mut Vehicle, b: &mut Vehicle) -> f32 {
    collide_with(a, b, &ContactParams::DEFAULT)
}

/// [`collide`] with the response constants of `p` (the race AI's, ai/race_physics.rs).
pub fn collide_with(a: &mut Vehicle, b: &mut Vehicle, p: &ContactParams) -> f32 {
    let Some((point, n, depth)) = overlap(a, b) else { return 0.0 };
    let (ma, mb) = (a.data.mass, b.data.mass);
    // Push apart by mass share.
    let share_a = mb / (ma + mb);
    a.position -= n * depth * share_a;
    b.position += n * depth * (1.0 - share_a);
    let ra = point - a.position;
    let rb = point - b.position;
    let va = a.velocity + a.angular_velocity.cross(ra);
    let vb = b.velocity + b.angular_velocity.cross(rb);
    let rel = vb - va;
    let vn = rel.dot(n);
    if vn >= 0.0 {
        return 0.0;
    }
    let inv_i = |v: &Vehicle, t: Vec3| v.rotation * ((v.rotation.inverse() * t) / v.inertia);
    let k = |dir: Vec3| 1.0 / ma + 1.0 / mb + dir.dot(inv_i(a, ra.cross(dir)).cross(ra)) + dir.dot(inv_i(b, rb.cross(dir)).cross(rb));
    let jn = -(1.0 + p.restitution) * vn / k(n);
    let mut impulse = n * jn;
    let vt = rel - n * vn;
    crate::sfx_queue::car(point, -vn, vt.length());
    if let Some(t) = vt.try_normalize() {
        impulse -= t * (vt.length() / k(t)).min(p.friction * jn);
    }
    // impulse acts on b, -impulse on a.
    b.velocity += impulse / mb;
    b.angular_velocity += scale_ang(inv_i(b, rb.cross(impulse)), p);
    a.velocity -= impulse / ma;
    a.angular_velocity -= scale_ang(inv_i(a, ra.cross(impulse)), p);
    -vn
}

/// `wall` stays where it is (a remote player's displayed pose). `body` is pushed out and takes the whole impulse,
/// so one tick clears the overlap instead of sharing it with a body the next snapshot will snap back.
pub fn collide_kinematic(body: &mut Vehicle, wall: &Vehicle) -> f32 {
    let Some((point, n, depth)) = overlap(body, wall) else { return 0.0 };
    let mass = body.data.mass.max(1.0);
    body.position -= n * depth;
    let ra = point - body.position;
    let rb = point - wall.position;
    let va = body.velocity + body.angular_velocity.cross(ra);
    let vb = wall.velocity + wall.angular_velocity.cross(rb);
    let rel = vb - va;
    let vn = rel.dot(n);
    if vn >= 0.0 {
        return 0.0;
    }
    let inv_i = |rot: Quat, inertia: Vec3, t: Vec3| rot * ((rot.inverse() * t) / inertia);
    let k = |dir: Vec3| 1.0 / mass + dir.dot(inv_i(body.rotation, body.inertia, ra.cross(dir)).cross(ra));
    let jn = -(1.0 + RESTITUTION) * vn / k(n).max(1e-6);
    let mut impulse = n * jn;
    let vt = rel - n * vn;
    crate::sfx_queue::car(point, -vn, vt.length());
    if let Some(t) = vt.try_normalize() {
        impulse -= t * (vt.length() / k(t).max(1e-6)).min(FRICTION * jn);
    }
    body.velocity -= impulse / mass;
    body.angular_velocity -= inv_i(body.rotation, body.inertia, ra.cross(impulse));
    -vn
}
