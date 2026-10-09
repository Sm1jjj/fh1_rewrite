//! Contact hand-off from the physics code (lib) to the impact sounds (`sfx_world.rs`, game binary).
//!
//! Compiled into the library as `fh1_engine::sfx_queue` (`#[path]` in lib.rs) so `vehicle.rs` / `vehicle/contact.rs` can
//! call it without knowing about Bevy audio. Off (one relaxed atomic load per call) until the game's `SfxWorldPlugin`
//! enables it, so the parity / probe binaries and the tests never fill it. Consecutive pushes of the same kind and
//! surface within [`MERGE_DIST`] keep the largest speeds (the 480 Hz substeps would otherwise queue hundreds of entries).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use bevy::math::Vec3;

/// What touched what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawKind {
    /// Body contact point against a wall (collision-mesh normal mostly horizontal).
    Wall,
    /// Body contact point against ground-like geometry (normal mostly up): bottoming out, landing on the body.
    Ground,
    /// Car against car.
    Car,
    /// A smashable prop broke (`prop` = scenery template index).
    Smash,
}

/// One queued contact.
#[derive(Clone, Copy, Debug)]
pub struct Raw {
    pub kind: RawKind,
    pub pos: Vec3,
    /// Closing speed along the contact normal (m/s); <= 0 when the points are separating.
    pub normal_speed: f32,
    /// Sliding speed along the surface (m/s).
    pub tangent_speed: f32,
    /// Collision-mesh surface id (`Wall` / `Ground`), else 0.
    pub surface: u8,
    /// Scenery template index (`Smash`), else 0.
    pub prop: u16,
}

const MERGE_DIST: f32 = 1.5;
const CAP: usize = 256;

static ENABLED: AtomicBool = AtomicBool::new(false);
static QUEUE: Mutex<Vec<Raw>> = Mutex::new(Vec::new());

/// Turns collection on / off (the plugin; off clears the queue).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if !on {
        if let Ok(mut q) = QUEUE.lock() {
            q.clear();
        }
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn push(r: Raw) {
    let Ok(mut q) = QUEUE.lock() else { return };
    if let Some(last) = q.last_mut() {
        if last.kind == r.kind && last.surface == r.surface && last.prop == r.prop && r.kind != RawKind::Smash && last.pos.distance_squared(r.pos) < MERGE_DIST * MERGE_DIST {
            last.normal_speed = last.normal_speed.max(r.normal_speed);
            last.tangent_speed = last.tangent_speed.max(r.tangent_speed);
            last.pos = r.pos;
            return;
        }
    }
    if q.len() < CAP {
        q.push(r);
    }
}

/// A car body point touched the world (vehicle.rs `collide_body`, every substep with a contact). `normal` is the push-out
/// direction, `closing` the speed into it (negative when separating), `sliding` the speed along the surface.
pub fn wall(point: Vec3, normal: Vec3, closing: f32, sliding: f32, surface: u8) {
    if !enabled() {
        return;
    }
    let kind = if normal.y > 0.7 { RawKind::Ground } else { RawKind::Wall };
    push(Raw { kind, pos: point, normal_speed: closing, tangent_speed: sliding, surface, prop: 0 });
}

/// Two cars touched (contact.rs `collide` / `collide_kinematic`, only when closing).
pub fn car(point: Vec3, closing: f32, sliding: f32) {
    if !enabled() {
        return;
    }
    push(Raw { kind: RawKind::Car, pos: point, normal_speed: closing, tangent_speed: sliding, surface: 0, prop: 0 });
}

/// A smashable broke (smash.rs `update`).
pub fn smash(template: u16, pos: Vec3, speed: f32) {
    if !enabled() {
        return;
    }
    push(Raw { kind: RawKind::Smash, pos, normal_speed: speed, tangent_speed: 0.0, surface: 0, prop: template });
}

/// Moves the queued contacts to `out` (the plugin, once a frame).
pub fn drain(out: &mut Vec<Raw>) {
    if let Ok(mut q) = QUEUE.lock() {
        out.append(&mut q);
    }
}
