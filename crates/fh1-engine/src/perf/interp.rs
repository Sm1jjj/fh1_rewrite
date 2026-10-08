//! Render-rate interpolation of the car's per-tick wheel state (job P5, docs/PERF.md "Frame pacing").
//!
//! Physics runs at a fixed 120 Hz; the chassis is drawn at `Vehicle::render_pose` (lerp between the last two ticks by the
//! fixed overstep). The wheels' steer, spin and suspension drop were drawn from the last tick only, so at 300 Hz they
//! moved in 120 Hz steps against a smoothly moving body (and the drop shadow with them). [`WheelHistory`] keeps the
//! previous tick's values (captured in `FixedFirst`, the same moment `begin_tick` saves the chassis pose) so the visuals
//! use the same interpolation as the body. `FH1_INTERP_WHEELS=0` = last tick only (old).

use bevy::prelude::*;

use crate::vehicle::Vehicle;
use crate::Car;

pub fn plugin(app: &mut App) {
    let on = std::env::var("FH1_INTERP_WHEELS").map_or(true, |v| v != "0");
    app.insert_resource(WheelHistory { on, prev: None }).add_systems(FixedFirst, capture);
}

/// Per wheel: steer (rad), spin angle (rad, wrapped to TAU), drop (m).
type WheelSnap = [(f32, f32, f32); 4];

#[derive(Resource)]
pub struct WheelHistory {
    on: bool,
    /// The player car's wheels at the start of the latest tick.
    prev: Option<(Entity, WheelSnap)>,
}

fn snap(v: &Vehicle) -> WheelSnap {
    std::array::from_fn(|i| (v.wheels[i].steer, v.wheels[i].angle, v.wheel_drop(i)))
}

fn capture(cars: Query<(Entity, &Car)>, mut h: ResMut<WheelHistory>) {
    h.prev = cars.iter().next().map(|(e, c)| (e, snap(&c.0)));
}

impl WheelHistory {
    /// Wheel `i` of the car on `entity` drawn `alpha` (fixed overstep fraction) of the way from the previous tick to
    /// the latest: (steer, spin angle, drop).
    pub fn wheel(&self, entity: Entity, v: &Vehicle, i: usize, alpha: f32) -> (f32, f32, f32) {
        let cur = (v.wheels[i].steer, v.wheels[i].angle, v.wheel_drop(i));
        let Some(prev) = self.prev.filter(|(e, _)| self.on && *e == entity).map(|(_, p)| p[i]) else { return cur };
        let a = alpha.clamp(0.0, 1.0);
        // Spin wraps at TAU: take the short way (under half a turn per tick up to ~400 km/h on a 0.3 m tyre).
        let tau = std::f32::consts::TAU;
        let d_spin = (cur.1 - prev.1 + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI;
        (prev.0 + (cur.0 - prev.0) * a, prev.1 + d_spin * a, prev.2 + (cur.2 - prev.2) * a)
    }
}
