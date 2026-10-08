//! Surface particles from the tyres (FX3, docs/EFFECTS.md "Surface particles"): dirt / gravel kick-up, dust
//! trails, grass clippings, leaves and litter, per surface.
//!
//! What emits where is the game's own data: every Colorado surface carries `SkidData/SmokeType@<system>` weights
//! (surfaces.json from the track's `.fiz` surface block; `Dirt` 1 + `Dust` 0.5 on Dirt, `Grass` 1 on Grass,
//! `GravelBits` 1 on Gravel, `Dust` 1 on Sand, `Leaf1..5`, `Litter`) and `SkidData/MinIntensity` (0.4 on the
//! off-road surfaces, 0 on tarmac). The systems are `media/effects.zip` `DefaultTrack.xml` SystemEffects
//! ([`SYSTEMS`]: `Dirt` = Dirt1.xml, ...), loaded and drawn by fh1-render `particles.rs` (FX1). `Smoke` (tyre smoke)
//! is FX1's and is skipped here.
//!
//! How the game turns slip / speed into an emission intensity lives in default.xex and is not decoded: the rule
//! below is INFERRED (intensity = max(slip term, MinIntensity x speed term)). No entities: each frame spawns into
//! FX1's pools, capped per effect by the game's `budget` (live particles) and globally by [`MAX_SPAWNS_PER_FRAME`].
//!
//! `FH1_SURFACE_FX=0` turns it off; `FH1_SURFACE_FX_LOG=1` logs the spawns once a second.

use bevy::prelude::*;
use fh1_render::particles::{EffectId, FxParticles, Spawn};

use crate::track::Track;
use crate::Car;

/// Surface `SmokeType@<system_name>` -> effects.zip file stem (`DefaultTrack.xml`). Leaf2..5 are their own files.
const SYSTEMS: &[(&str, &str)] = &[
    ("Dirt", "Dirt1"),
    ("Dust", "Dust1"),
    ("Grass", "Grass1"),
    ("GravelBits", "GravelBits"),
    ("Leaf1", "Leaf1"),
    ("Leaf2", "Leaf2"),
    ("Leaf3", "Leaf3"),
    ("Leaf4", "Leaf4"),
    ("Leaf5", "Leaf5"),
    ("Litter", "Litter"),
];
const DUST: usize = 1;

/// Global cap on particles started per frame by this module (P5: CPU cost).
const MAX_SPAWNS_PER_FRAME: u32 = 256;
/// Slip speed (m/s) of the tread over the ground at which kick-up starts, and the extra slip for full intensity.
const SLIP_START: f32 = 1.0;
const SLIP_FULL: f32 = 6.0;
/// Rolling emission on surfaces with MinIntensity: none below `ROLL_START`, full at `ROLL_FULL` (m/s).
const ROLL_START: f32 = 3.0;
const ROLL_FULL: f32 = 25.0;

#[derive(Resource, Default)]
pub struct SurfaceFx {
    /// surface id -> ([(system, weight)], MinIntensity), from the track's surfaces.
    table: Option<Vec<(Vec<(usize, f32)>, f32)>>,
    /// Effect ids per system (None until FX1 has the data root, or the file is missing: retried each frame).
    ids: [Option<EffectId>; SYSTEMS.len()],
    /// Fractional particles carried to the next frame, per wheel and system.
    carry: [[f32; SYSTEMS.len()]; 4],
    log: Option<(f64, u32)>,
}

pub fn surface_fx_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SURFACE_FX").map_or(true, |v| v != "0"))
}

fn build_table(world: &fh1_engine::world::WorldGround) -> Vec<(Vec<(usize, f32)>, f32)> {
    world
        .world
        .surfaces
        .iter()
        .map(|s| {
            let weights = s
                .params
                .iter()
                .filter_map(|(k, &v)| {
                    let name = k.strip_prefix("SkidData/SmokeType@")?;
                    // `Smoke` (tyre smoke) is FX1's; unknown names are skipped.
                    Some((SYSTEMS.iter().position(|s| s.0 == name)?, v)).filter(|_| v > 0.0)
                })
                .collect();
            (weights, s.param("SkidData/MinIntensity").unwrap_or(0.0))
        })
        .collect()
}

/// Small deterministic hash -> 0..1 (spawn-point jitter without an RNG resource).
fn hash01(mut x: u32) -> f32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

/// Spawns this frame's surface particles from the four tyre contacts.
pub fn emit_surface_fx(
    mut fx: ResMut<SurfaceFx>,
    mut particles: Option<ResMut<FxParticles>>,
    track: Res<Track>,
    cars: Query<&Car>,
    fixed: Res<Time<Fixed>>,
    time: Res<Time>,
    mut frame: Local<u32>,
) {
    let Some(particles) = particles.as_deref_mut() else { return };
    if !surface_fx_on() || !particles.enabled() {
        return;
    }
    let Ok(car) = cars.single() else { return };
    let fx = &mut *fx;
    if fx.table.is_none() {
        let Some(world) = &track.world else { return };
        fx.table = Some(build_table(world));
        fx.log = std::env::var_os("FH1_SURFACE_FX_LOG").map(|_| (0.0, 0));
    }
    for (i, (_, file)) in SYSTEMS.iter().enumerate() {
        if fx.ids[i].is_none() {
            fx.ids[i] = particles.effect(file);
        }
    }
    *frame = frame.wrapping_add(1);
    let dt = time.delta_secs().min(0.1);
    let v = &car.0;
    let (pos, rot) = v.render_pose(fixed.overstep_fraction());
    let up = rot * Vec3::Y;

    let mut spawned = 0u32;
    let table = fx.table.as_ref().unwrap();
    for (wi, w) in v.wheels.iter().enumerate() {
        let weights = table.get(w.surface as usize).filter(|_| w.grounded);
        let Some((weights, min_intensity)) = weights.filter(|r| !r.0.is_empty()) else {
            fx.carry[wi] = [0.0; SYSTEMS.len()];
            continue;
        };
        let r = v.data.tyre_radius[wi / 2];
        // Contact patch: hub (+ suspension drop) minus the tyre radius along the car's up.
        let hub = Vec3::from(v.data.hubs[wi]) - v.cg_model + Vec3::Y * v.wheel_drop(wi);
        let contact = pos + rot * hub - up * r;
        let fwd = rot * (Quat::from_rotation_y(w.steer) * Vec3::NEG_Z);
        let right = fwd.cross(up).normalize_or_zero();
        let v_point = v.velocity + v.angular_velocity.cross(rot * hub);
        let (v_long, v_lat) = (v_point.dot(fwd), v_point.dot(right));
        // Tread velocity over the ground: what drags the surface material along (wheelspin throws it backwards,
        // a slide pushes it outwards).
        let tread = (v_long - w.omega * r) * fwd + v_lat * right;
        let slip = tread.length();
        let speed = v_point.length();
        let slip_term = ((slip - SLIP_START) / SLIP_FULL).clamp(0.0, 1.0);
        let roll = ((speed - ROLL_START) / (ROLL_FULL - ROLL_START)).clamp(0.0, 1.0);
        let intensity = slip_term.max(min_intensity * roll);
        if intensity <= 0.0 {
            fx.carry[wi] = [0.0; SYSTEMS.len()];
            continue;
        }
        // Emit axis: along the tread's motion (backwards from the travel when only rolling), tipped up.
        let along = if slip > 0.5 { tread / slip } else { -v_point.normalize_or(fwd) };
        let axis = (along.reject_from(up).normalize_or(-fwd) + up * 0.6).normalize();
        for &(si, weight) in weights {
            let Some(id) = fx.ids[si] else { continue };
            let (budget, inherit) = {
                let d = particles.def(id);
                (d.budget as usize, d.inherit)
            };
            // Dust is the trail behind the car: it follows speed as much as slip.
            let k = if si == DUST { intensity.max(min_intensity * roll) } else { intensity };
            let n = particles.count(id, k * weight, dt, &mut fx.carry[wi][si]);
            // The game's budget is per emitter (one per wheel); FX1 caps the pool at budget x 4.
            if n == 0 || particles.live(id) >= budget * 4 {
                continue;
            }
            let n = n.min(MAX_SPAWNS_PER_FRAME.saturating_sub(spawned));
            if n == 0 {
                break;
            }
            spawned += n;
            // A little jitter across the tyre width.
            let j = hash01(*frame ^ (wi as u32).wrapping_mul(7919) ^ (si as u32).wrapping_mul(104_729)) - 0.5;
            let mut s = Spawn::new(contact + right * (j * 0.15) + up * 0.03, axis, n);
            s.inherit = v.velocity * inherit;
            s.intensity = k;
            if si == DUST {
                // Big soft puffs: fade where they meet the ground. Small bits start on it, so they don't.
                s.ground_y = contact.y;
            }
            particles.spawn(id, &s);
        }
    }
    if let Some((t, sum)) = fx.log.as_mut() {
        *sum += spawned;
        let now = time.elapsed_secs_f64();
        if now - *t >= 1.0 {
            let live: Vec<String> = SYSTEMS.iter().zip(&fx.ids).filter_map(|(s, id)| Some(format!("{} {}", s.0, particles.live((*id)?)))).collect();
            info!("surface fx: {sum} spawned in the last second; live {live:?}");
            *t = now;
            *sum = 0;
        }
    }
}
