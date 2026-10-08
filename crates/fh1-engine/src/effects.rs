//! Tyre smoke (FX1, docs/EFFECTS.md "Tyre smoke"): the effects.zip `Smoke` system (DefaultTrack.xml system "Smoke") from
//! each tyre's contact patch, plus the per-wheel `SmokeRim0..3` systems around the wheel, through fh1-render particles.rs.
//!
//! Surface-aware from the game's own data: the surface's `SkidData/SmokeType@Smoke` weight (1 on Asphalt, Concrete,
//! Brick, Kerb, Trackway, Grasscrete..., 0.15 on Gravel, 0 off-road; physics.zip surfaceTypes.xml). Off-road kick-up is
//! FX3's (effects_surface.rs).
//!
//! The emission rule is INFERRED (the game's per-wheel smoke code in default.xex is not traced): the tread's speed over
//! the ground (wheelspin, lock-up, slide; the same tread vector as effects_surface.rs) drives it. Smoke starts at
//! [`SLIP_START`] m/s and `Quantity` reads as particles per metre of tread slip, so `budget` (180) fills at ~10 m/s of
//! slip with the 1.6 s life (a per-second reading would never get near the budget). Puffs grow with the slip.
//!
//! `FH1_TYRE_SMOKE=0` turns it off (`FH1_PARTICLES=0` turns off every particle).

use bevy::prelude::*;
use fh1_render::particles::{EffectId, FxParticles, Spawn};

use crate::track::Track;
use crate::Car;

/// Tread slip (m/s) where smoke starts, and the extra slip for full intensity (INFERRED).
const SLIP_START: f32 = 2.5;
const SLIP_FULL: f32 = 8.0;
/// Particles started per frame by this module at most (CPU cost).
const MAX_SPAWNS_PER_FRAME: u32 = 64;

/// `FH1_SMOKE_OLD=1`: the first tuning (puffs × (1 + 0.8 intensity) on top of the XML size, every particle of a frame
/// started at one point). Default: XML sizes, particles spread along the contact's path over the frame.
fn old() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("FH1_SMOKE_OLD").is_ok_and(|v| v == "1"))
}

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // The Remaster smoke (smoke.rs) replaces this unless FH1_SMOKE=old.
    *ON.get_or_init(|| std::env::var("FH1_TYRE_SMOKE").map_or(true, |v| v != "0") && crate::smoke::mode() == crate::smoke::Mode::Old)
}

/// `FH1_TYRE_SMOKE=force` (debug): every wheel smokes as at full slip, on any surface.
fn forced() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("FH1_TYRE_SMOKE").is_ok_and(|v| v == "force"))
}

#[derive(Default)]
pub struct SmokeState {
    /// `Smoke` and `SmokeRim0..3`.
    ids: Option<(EffectId, [Option<EffectId>; 4])>,
    /// SmokeType@Smoke by surface id (None = no world: smoke everywhere).
    weights: Option<Vec<f32>>,
    carry: [[f32; 2]; 4],
}

pub fn tyre_smoke(mut st: Local<SmokeState>, particles: Option<ResMut<FxParticles>>, track: Res<Track>, cars: Query<&Car>, fixed: Res<Time<Fixed>>, time: Res<Time>) {
    let Some(mut particles) = particles else { return };
    if !on() || !particles.enabled() {
        return;
    }
    let Ok(car) = cars.single() else { return };
    let st = &mut *st;
    if st.ids.is_none() {
        let Some(smoke) = particles.effect("Smoke") else { return };
        st.ids = Some((smoke, std::array::from_fn(|i| particles.effect(&format!("SmokeRim{i}")))));
        st.weights = track.world.as_ref().map(|w| w.world.surfaces.iter().map(|s| s.param("SkidData/SmokeType@Smoke").unwrap_or(0.0)).collect());
    }
    let (smoke, rims) = st.ids.unwrap();
    let dt = time.delta_secs().min(0.1);
    let v = &car.0;
    let (pos, rot) = v.render_pose(fixed.overstep_fraction());
    let up = rot * Vec3::Y;
    let (inherit, rim_inherit) = (particles.def(smoke).inherit, 0.9);
    let old = old();
    let mut spawned = 0u32;
    for (wi, w) in v.wheels.iter().enumerate() {
        let weight = match &st.weights {
            _ if forced() => 1.0,
            Some(t) => t.get(w.surface as usize).copied().unwrap_or(0.0),
            None => 1.0,
        };
        if !(w.grounded || forced()) || weight <= 0.0 {
            st.carry[wi] = [0.0; 2];
            continue;
        }
        let r = v.data.tyre_radius[wi / 2];
        let hub = Vec3::from(v.data.hubs[wi]) - v.cg_model + Vec3::Y * v.wheel_drop(wi);
        let hub_w = pos + rot * hub;
        let contact = hub_w - up * r;
        let fwd = rot * (Quat::from_rotation_y(w.steer) * Vec3::NEG_Z);
        let right = fwd.cross(up).normalize_or_zero();
        let v_point = v.velocity + v.angular_velocity.cross(rot * hub);
        let tread = (v_point.dot(fwd) - w.omega * r) * fwd + v_point.dot(right) * right;
        let slip = if forced() { SLIP_START + SLIP_FULL } else { tread.length() };
        let k = ((slip - SLIP_START) / SLIP_FULL).clamp(0.0, 1.0);
        if k <= 0.0 {
            st.carry[wi] = [0.0; 2];
            continue;
        }
        // Tread slip speed (m/s, × the surface weight): `count` × dt gives the metres slid → `Quantity` per metre.
        let metres = slip * weight * k.sqrt();
        let side = if wi % 2 == 0 { -1.0 } else { 1.0 };
        let axis = (up + tread.normalize_or_zero() * 0.3).normalize();
        let n = particles.count(smoke, metres, dt, &mut st.carry[wi][0]).min(MAX_SPAWNS_PER_FRAME.saturating_sub(spawned));
        if n > 0 {
            let mut s = Spawn::new(contact + right * (side * 0.05) + up * 0.08, axis, n);
            s.inherit = v_point * inherit;
            s.intensity = k;
            s.size_scale = if old { 1.0 + 0.8 * k } else { 1.0 };
            s.ground_y = contact.y;
            if !old {
                s.sweep = v_point * dt;
            }
            particles.spawn(smoke, &s);
            spawned += n;
        }
        // Rim smoke: the dense cloud that clings to the wheel (WheelAttractor; approximated by inheriting the wheel's
        // velocity, VelInhMin 0.9).
        if let Some(rim) = rims[wi] {
            let n = particles.count(rim, metres * 0.5, dt, &mut st.carry[wi][1]).min(MAX_SPAWNS_PER_FRAME.saturating_sub(spawned));
            if n > 0 {
                let mut s = Spawn::new(hub_w - up * (0.6 * r) + right * (side * 0.1), up, n);
                s.inherit = v_point * rim_inherit;
                s.intensity = k;
                s.ground_y = contact.y;
                if !old {
                    s.sweep = v_point * dt;
                }
                particles.spawn(rim, &s);
                spawned += n;
            }
        }
    }
}
