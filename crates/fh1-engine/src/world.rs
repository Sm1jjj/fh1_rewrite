//! The open world as the car sees it: `fh1_world::World` (Colorado collision) behind the
//! vehicle's [`Ground`] trait.

use std::path::Path;

use anyhow::Result;
use bevy::math::Vec3;
use fh1_world::World;

use crate::vehicle::{Ground, GroundHit, SphereContact, TyreSurface};

/// The disc's world data is left-handed (Xbox D3D): +X east, +Z **north**, +Y up. Bevy is
/// right-handed, so Z is negated wherever world data enters the engine (+Z = south here), the
/// same flip that separates gamedb/MAXData from the car meshes. Verified by fitting the collision
/// roads to the in-game map texture: Z-flipped fits clearly best (docs/COLORADO_RECON.md).
pub const MIRROR_Z: bool = true;

/// Collision triangles carry a 16-bit routes mask. Bit 15 (0x8000) = always present (1.07M floor
/// and 223k wall triangles on Colorado); bits 0-14 select event-specific barriers (~100k
/// triangles, nearly all walls) that close roads off for particular races. Free roam uses only
/// bit 15. Verified at TrackRoute000's start_location_00: a route-1 (0x0001) wall crosses the road
/// there and traps the car if every route's barriers are active at once.
pub const FREE_ROAM: u16 = 0x8000;

pub struct WorldGround {
    pub world: World,
    /// Index of the "Invisible" surface: kept for collision, hidden when rendering.
    pub invisible: Option<u8>,
    /// Which routes' barriers are active ([`FREE_ROAM`] plus a race's own bits).
    pub routes: u16,
    /// A running race's own barrier bits (race.rs sets them at the start, clears them at the end); OR'd into `routes`.
    pub event_routes: std::sync::atomic::AtomicU16,
    /// A running race's event-wall triangles, by index (race.rs: the event triangles standing where the race's barrier
    /// objects are); active whatever their route bits. Empty outside races and with `FH1_RACE_BARRIER_TRIS=0`.
    event_tris: std::sync::RwLock<std::collections::HashSet<u32>>,
    event_tris_on: std::sync::atomic::AtomicBool,
    /// Tyre terms per surface id (surfaceTypes.xml `<Friction>`), built once.
    tyre: Vec<TyreSurface>,
    /// Surfaces a car drives on (roads, verges, off-road): their low wall triangles are kerb faces, not barriers.
    drivable: Vec<bool>,
}

impl WorldGround {
    pub fn load(dir: &Path) -> Result<Self> {
        let world = World::load(dir)?;
        let invisible = world.surfaces.iter().position(|s| s.name == "Invisible").map(|i| i as u8);
        let tyre = world.surfaces.iter().map(|s| TyreSurface::from_params(|k| s.param(k))).collect();
        const DRIVABLE: [&str; 15] = [
            "Asphalt", "Asphault", "Concrete", "Brick", "Rumble", "Trackway", "Kerb", "Cobble", "Dirt", "Gravel", "Grass", "Sand", "Leaf", "Litter",
            "Grasscrete",
        ];
        let drivable = world.surfaces.iter().map(|s| DRIVABLE.iter().any(|k| s.name.contains(k)) && !s.name.contains("Barrier")).collect();
        Ok(Self {
            world,
            invisible,
            routes: FREE_ROAM,
            event_routes: std::sync::atomic::AtomicU16::new(0),
            event_tris: Default::default(),
            event_tris_on: std::sync::atomic::AtomicBool::new(false),
            tyre,
            drivable,
        })
    }

    fn to_world(v: Vec3) -> [f32; 3] {
        if MIRROR_Z { [v.x, v.y, -v.z] } else { v.to_array() }
    }

    fn from_world(v: [f32; 3]) -> Vec3 {
        if MIRROR_Z { Vec3::new(v[0], v[1], -v[2]) } else { Vec3::from(v) }
    }

    /// Whether a triangle exists for the active routes.
    pub fn active(&self, tri: u32) -> bool {
        let routes = self.world.tris[tri as usize].routes;
        routes & (self.routes | self.event_routes.load(std::sync::atomic::Ordering::Relaxed)) != 0 || (routes & 0x7FFF != 0 && self.event_tri_active(tri))
    }

    fn is_ground_tri(&self, tri: u32) -> bool {
        let p = self.world.tri_points(tri);
        let (u, v) = (Vec3::from(p[1]) - Vec3::from(p[0]), Vec3::from(p[2]) - Vec3::from(p[0]));
        u.cross(v).normalize_or_zero().y.abs() >= 0.5
    }

    fn event_tri_active(&self, tri: u32) -> bool {
        self.event_tris_on.load(std::sync::atomic::Ordering::Relaxed) && self.event_tris.read().is_ok_and(|s| s.contains(&tri))
    }

    /// Makes exactly these event triangles (route bits 0-14) solid for the running race (empty = none).
    pub fn set_event_tris(&self, tris: std::collections::HashSet<u32>) {
        self.event_tris_on.store(!tris.is_empty(), std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut s) = self.event_tris.write() {
            *s = tris;
        }
    }

    /// Drops the wall triangles whose footprint cuts the engine-space `path` within their height: the game's races are
    /// drivable along their racing line, so such a wall (e.g. the end of a diagonal closure that runs on across the road)
    /// isn't part of the event's closure (offline: 10 wall triangles cut the racing line in 6 routes, all of them within
    /// reach of the event's barrier objects).
    pub fn drop_crossing_tris(&self, tris: &mut std::collections::HashSet<u32>, path: &[Vec3]) {
        let segs: Vec<([f32; 3], [f32; 3])> = path.windows(2).map(|s| (Self::to_world(s[0]), Self::to_world(s[1]))).collect();
        let cross2 = |a: [f32; 2], b: [f32; 2]| a[0] * b[1] - a[1] * b[0];
        tris.retain(|&t| {
            let p = self.world.tri_points(t);
            let (u, v) = ([p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]], [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]]);
            let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12);
            if (n[1] / len).abs() >= 0.5 {
                return true; // ground
            }
            // The wall's footprint: its longest edge in x / z.
            let xz = |i: usize| [p[i][0], p[i][2]];
            let (a, b) = [(0, 1), (1, 2), (0, 2)]
                .into_iter()
                .map(|(i, j)| (xz(i), xz(j)))
                .max_by(|x, y| {
                    let (lx, ly) = ((x.0[0] - x.1[0]).hypot(x.0[1] - x.1[1]), (y.0[0] - y.1[0]).hypot(y.0[1] - y.1[1]));
                    lx.total_cmp(&ly)
                })
                .unwrap();
            let (ylo, yhi) = (p[0][1].min(p[1][1]).min(p[2][1]), p[0][1].max(p[1][1]).max(p[2][1]));
            let (x0, x1, z0, z1) = (a[0].min(b[0]) - 1.0, a[0].max(b[0]) + 1.0, a[1].min(b[1]) - 1.0, a[1].max(b[1]) + 1.0);
            !segs.iter().any(|(q, q2)| {
                if q[0].max(q2[0]) < x0 || q[0].min(q2[0]) > x1 || q[2].max(q2[2]) < z0 || q[2].min(q2[2]) > z1 {
                    return false;
                }
                let y = (q[1] + q2[1]) * 0.5;
                if y < ylo - 4.5 || y > yhi + 0.5 {
                    return false;
                }
                let (d1, d2) = ([b[0] - a[0], b[1] - a[1]], [q2[0] - q[0], q2[2] - q[2]]);
                let den = cross2(d1, d2);
                if den.abs() < 1e-9 {
                    return false;
                }
                let qp = [q[0] - a[0], q[2] - a[1]];
                let (t, u) = (cross2(qp, d2) / den, cross2(qp, d1) / den);
                (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)
            })
        });
    }

    /// Event triangles (route bits 0-14, not free roam's) within `radius` of any of the engine-space `probes`.
    pub fn event_tris_near(&self, probes: impl IntoIterator<Item = Vec3>, radius: f32) -> std::collections::HashSet<u32> {
        let mut out = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for p in probes {
            self.world.sphere_contacts(Self::to_world(p), radius, &mut hits);
            // Walls only: event ground would be a hidden floor / ramp beside the barrier.
            out.extend(hits.iter().map(|c| c.tri).filter(|&t| self.world.tris[t as usize].routes & 0x7FFF != 0 && !self.is_ground_tri(t)));
        }
        out
    }

    /// A body sphere's contact (collision space; the sphere moved from `from` to `to` this step) for the vehicle, with the
    /// ground / step classification of [`SphereContact::face`] and [`SphereContact::step_top`] (2026-10-09, wheels and
    /// bodies snagging on unwelded patch seams: tools/ground_seams.py). A ground triangle the sphere is above, or crossed
    /// from above this step, pushes along its upward face normal by its plane distance; one the sphere is below and came
    /// at from the side is a step (its lip / plane height); a wall under 0.4 m of a drivable surface is a kerb face.
    /// `FH1_STEP_CONTACT=0`: the raw contact.
    fn contact(&self, c: &fh1_world::Contact, from: [f32; 3], to: [f32; 3], radius: f32) -> Option<SphereContact> {
        let mut out = SphereContact { point: Self::from_world(c.point), normal: Self::from_world(c.normal), depth: c.depth, surface: c.surface, face: None, step_top: None };
        if crate::vehicle::step_climb().is_none() {
            return Some(out);
        }
        let p = self.world.tri_points(c.tri).map(Vec3::from);
        let n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
        if n.y.abs() < 0.5 {
            let (lo, hi) = (p[0].y.min(p[1].y).min(p[2].y), p[0].y.max(p[1].y).max(p[2].y));
            if hi - lo < 0.4 && self.drivable.get(c.surface as usize).copied().unwrap_or(false) {
                out.step_top = Some(hi);
            }
            return Some(out);
        }
        let up = n * n.y.signum();
        let (s_from, s_to) = ((Vec3::from(from) - p[0]).dot(up), (Vec3::from(to) - p[0]).dot(up));
        let proj = Vec3::from(to) - up * s_to;
        let inside = point_in_tri(proj, p, up);
        if s_to >= 0.0 || s_from >= 0.0 {
            // Resting on / landing on it: straight up out of the plane (an edge of the face can't push sideways).
            out.normal = Self::from_world(up.to_array());
            out.face = Some(out.normal);
            if inside || s_to < 0.0 {
                out.depth = radius - s_to;
            }
            return (out.depth > 0.0).then_some(out);
        }
        out.step_top = Some(if inside { proj.y } else { c.point[1] });
        Some(out)
    }

    pub fn surface_name(&self, id: u8) -> &str {
        self.world.surface(id).map(|s| s.name.as_str()).unwrap_or("?")
    }
}

impl Ground for WorldGround {
    /// Drivable ground only: walls (guard rails, the tall boundary walls) and triangles of
    /// inactive routes are stepped through, so a suspension ray near the road edge doesn't land a
    /// tyre on a vertical face. Walls are handled by the body's collision spheres instead.
    fn ray(&self, origin: Vec3, dir: Vec3, max: f32) -> Option<GroundHit> {
        let (mut o, mut left) = (Self::to_world(origin), max);
        let d = Self::to_world(dir);
        let mut travelled = 0.0;
        let h = loop {
            let h = self.world.raycast(o, d, left)?;
            if h.normal[1].abs() >= 0.5 && self.active(h.tri) {
                break fh1_world::Hit { t: travelled + h.t, ..h };
            }
            let step = h.t + 1e-3;
            travelled += step;
            left -= step;
            if left <= 0.0 {
                return None;
            }
            o = [o[0] + d[0] * step, o[1] + d[1] * step, o[2] + d[2] * step];
        };
        Some(GroundHit {
            distance: h.t,
            point: Self::from_world(h.point),
            normal: Self::from_world(h.normal),
            tyre: self.tyre.get(h.surface as usize).copied().unwrap_or_default(),
            surface: h.surface,
        })
    }

    fn sphere(&self, center: Vec3, radius: f32, out: &mut Vec<SphereContact>) {
        let mut raw = Vec::new();
        let c = Self::to_world(center);
        self.world.sphere_contacts(c, radius, &mut raw);
        out.clear();
        out.extend(raw.iter().filter(|r| self.active(r.tri)).filter_map(|r| self.contact(r, c, c, radius)));
    }

    fn sphere_sweep(&self, from: Vec3, to: Vec3, radius: f32, out: &mut Vec<SphereContact>) {
        let mut raw = Vec::new();
        let (f, t) = (Self::to_world(from), Self::to_world(to));
        self.world.sphere_sweep(f, t, radius, &mut raw);
        out.clear();
        out.extend(raw.iter().filter(|r| self.active(r.tri)).filter_map(|r| self.contact(r, f, t, radius)));
    }
}

/// Whether `q` (on the plane of `p` with unit normal `n`) lies inside the triangle.
fn point_in_tri(q: Vec3, p: [Vec3; 3], n: Vec3) -> bool {
    (0..3).all(|k| (p[(k + 1) % 3] - p[k]).cross(q - p[k]).dot(n) >= -1e-6) || (0..3).all(|k| (p[(k + 1) % 3] - p[k]).cross(q - p[k]).dot(n) <= 1e-6)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Camera-ray replay (user 2026-10-07: 10-26 s freezes inside follow_camera): every frame of a chase-camera log
    /// (FH1_CAM_LOG, default data/perf_logs/cam_last.csv; columns eye xyz 1..3, target xyz 4..6) casts the camera's
    /// collision sweep (target -> eye) and ground probe (2 m above the eye, 50 m down) against Colorado; prints the
    /// slowest rays. Skipped without the log or the install.
    #[test]
    fn camera_ray_replay() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let log = std::env::var("CAM_REPLAY").map(std::path::PathBuf::from).unwrap_or(root.join("data/perf_logs/cam_last.csv"));
        let Ok(text) = std::fs::read_to_string(&log) else {
            eprintln!("skipped: no camera log");
            return;
        };
        let Ok(private) = crate::data::private_assets(&root.join("data")) else {
            eprintln!("skipped: no install");
            return;
        };
        let g = WorldGround::load(&private.join("world/colorado")).expect("colorado world");
        let mut slow: Vec<(f64, usize, &str)> = Vec::new();
        let mut total = 0.0f64;
        let mut hits = 0usize;
        for (i, line) in text.lines().enumerate() {
            let p: Vec<f32> = line.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if p.len() < 7 {
                continue;
            }
            let (eye, target) = (Vec3::new(p[1], p[2], p[3]), Vec3::new(p[4], p[5], p[6]));
            let to = eye - target;
            let d = to.length();
            if d > 1e-3 {
                let t = std::time::Instant::now();
                let _ = g.ray(target, to / d, d + 0.3);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                total += ms;
                slow.push((ms, i, "sweep"));
            }
            let t = std::time::Instant::now();
            if g.ray(eye + Vec3::Y * 2.0, Vec3::NEG_Y, 50.0).is_some() {
                hits += 1;
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            total += ms;
            slow.push((ms, i, "ground"));
        }
        slow.sort_by(|a, b| b.0.total_cmp(&a.0));
        eprintln!("{} rays, total {:.0} ms, ground probes that hit: {hits}; slowest:", slow.len(), total);
        for (ms, i, k) in slow.iter().take(12) {
            eprintln!("  {ms:9.2} ms  frame {i}  {k}");
        }
    }
}
