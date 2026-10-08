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
    /// Tyre terms per surface id (surfaceTypes.xml `<Friction>`), built once.
    tyre: Vec<TyreSurface>,
}

impl WorldGround {
    pub fn load(dir: &Path) -> Result<Self> {
        let world = World::load(dir)?;
        let invisible = world.surfaces.iter().position(|s| s.name == "Invisible").map(|i| i as u8);
        let tyre = world.surfaces.iter().map(|s| TyreSurface::from_params(|k| s.param(k))).collect();
        Ok(Self { world, invisible, routes: FREE_ROAM, event_routes: std::sync::atomic::AtomicU16::new(0), tyre })
    }

    fn to_world(v: Vec3) -> [f32; 3] {
        if MIRROR_Z { [v.x, v.y, -v.z] } else { v.to_array() }
    }

    fn from_world(v: [f32; 3]) -> Vec3 {
        if MIRROR_Z { Vec3::new(v[0], v[1], -v[2]) } else { Vec3::from(v) }
    }

    /// Whether a triangle exists for the active routes.
    pub fn active(&self, tri: u32) -> bool {
        self.world.tris[tri as usize].routes & (self.routes | self.event_routes.load(std::sync::atomic::Ordering::Relaxed)) != 0
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
        self.world.sphere_contacts(Self::to_world(center), radius, &mut raw);
        out.clear();
        out.extend(raw.iter().filter(|c| self.active(c.tri)).map(|c| SphereContact { point: Self::from_world(c.point), normal: Self::from_world(c.normal), depth: c.depth, surface: c.surface }));
    }
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
