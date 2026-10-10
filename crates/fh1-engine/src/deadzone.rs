//! Controller deadzones, exactly as FH1's own profile schema defines them
//! (`media/profileschema/ForzaProfile.sch`, the `ControllerAdvancedOptions` property bag).
//!
//! Each axis has an *inside* and an *outside* deadzone: the inside is the fraction of travel ignored
//! while the stick / trigger rests (so a worn stick does not steer the car), and the outside is the
//! fraction at which the input already counts as full. Between the two the response is linear, so
//! inside `SteeringAxisDeadzoneInside = 0.24` the game ignores the first 24 % of stick travel.
//!
//! The game stores them per device: `ControllerAdvancedOptions` (gamepad, the defaults here),
//! `WheelAdvancedOptions` and `BristolAdvancedOptions` (steering wheels). We only model the gamepad.

use serde::{Deserialize, Serialize};

/// Gamepad deadzones, mirroring FH1's `ControllerAdvancedOptions`. Fractions of full travel, 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Deadzones {
    pub steering_inside: f32,
    pub steering_outside: f32,
    pub throttle_inside: f32,
    pub throttle_outside: f32,
    pub brake_inside: f32,
    pub brake_outside: f32,
}

impl Default for Deadzones {
    /// FH1's shipped `ControllerAdvancedOptions` defaults (ForzaProfile.sch). Bump the field comments if
    /// the schema ever changes; these are the original game's own numbers, not our tuning.
    fn default() -> Self {
        Self {
            steering_inside: 0.24,
            steering_outside: 0.95,
            throttle_inside: 0.15,
            throttle_outside: 0.90,
            brake_inside: 0.15,
            brake_outside: 0.95,
        }
    }
}

/// Rescale one raw axis reading through an inside/outside deadzone.
///
/// The sign is preserved (steering is -1..1): the magnitude maps 0 at or below `inside`, 1 at or above
/// `outside`, linearly in between. `outside <= inside` degenerates to a plain threshold.
pub fn rescale(x: f32, inside: f32, outside: f32) -> f32 {
    let inside = inside.clamp(0.0, 1.0);
    let outside = outside.clamp(0.0, 1.0);
    if outside <= inside {
        return if x.abs() >= outside { x.signum() } else { 0.0 };
    }
    let a = x.abs();
    if a <= inside {
        0.0
    } else {
        (((a - inside) / (outside - inside)).min(1.0)) * x.signum()
    }
}

impl Deadzones {
    /// Left stick X (-1..1) through the steering deadzone. The keyboard's digital steer is not passed here.
    pub fn steer(&self, raw: f32) -> f32 {
        rescale(raw, self.steering_inside, self.steering_outside)
    }

    /// Right trigger (0..1) through the throttle deadzone.
    pub fn throttle(&self, raw: f32) -> f32 {
        rescale(raw.max(0.0), self.throttle_inside, self.throttle_outside)
    }

    /// Left trigger (0..1) through the brake deadzone.
    pub fn brake(&self, raw: f32) -> f32 {
        rescale(raw.max(0.0), self.brake_inside, self.brake_outside)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_is_zero_and_outside_is_full() {
        let d = Deadzones::default();
        assert_eq!(d.steer(0.0), 0.0);
        assert_eq!(d.steer(0.24), 0.0);
        assert_eq!(d.steer(-0.24), 0.0);
        assert_eq!(d.steer(0.95), 1.0);
        assert_eq!(d.steer(-0.95), -1.0);
        assert_eq!(d.steer(-1.0), -1.0);
    }

    #[test]
    fn between_is_linear() {
        // Midpoint of 0.24..0.95 is 0.595 -> 0.5.
        let d = Deadzones::default();
        assert!((d.steer(0.595) - 0.5).abs() < 1e-4);
        // The default 0.08 the code used before now reads as dead.
        assert_eq!(d.steer(0.08), 0.0);
    }

    #[test]
    fn triggers_use_their_own_pair() {
        let d = Deadzones::default();
        assert_eq!(d.throttle(0.15), 0.0);
        assert!((d.throttle(0.90) - 1.0).abs() < 1e-6);
        assert_eq!(d.brake(0.15), 0.0);
        assert!((d.brake(0.95) - 1.0).abs() < 1e-6);
        // Negative readings (a stick pushed the "wrong" way) clamp to 0 for a pedal.
        assert_eq!(d.throttle(-0.5), 0.0);
    }

    #[test]
    fn degenerate_pair_is_a_threshold() {
        assert_eq!(rescale(0.5, 0.6, 0.6), 0.0);
        assert_eq!(rescale(0.7, 0.6, 0.6), 1.0);
        assert_eq!(rescale(-0.7, 0.6, 0.6), -1.0);
    }
}
