//! Driving assists from the player's settings, the manual gearbox / clutch bindings and rewind (docs/ASSISTS.md).
//!
//! Bindings (driving only): shift up E / B, shift down Q / X, clutch Left Shift / LB (Manual and Manual with clutch), rewind (hold)
//! X key / Back. TCS and ABS reach the car through `Controls` (main.rs `read_input`); the rest goes straight into the
//! player's `Vehicle` here.

use std::collections::VecDeque;

use bevy::input::gamepad::{Gamepad, GamepadButton};
use bevy::prelude::*;
use fh1_engine::vehicle::{Assists, Vehicle};

use super::Settings;
use crate::Car;

/// Seconds of driving kept for rewind (one snapshot per fixed physics tick).
const REWIND_SECONDS: f32 = 15.0;

/// Rewind buffer: the car's state after each fixed tick, newest last.
#[derive(Resource, Default)]
pub struct Rewind {
    history: VecDeque<Vehicle>,
    /// The rewind button is held (set in Update, used by the fixed-tick system).
    held: bool,
    /// The car the history belongs to (cleared on a car change).
    car: String,
}

pub struct AssistsPlugin;

impl Plugin for AssistsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Rewind>()
            .add_systems(Update, assist_input.run_if(super::driving).after(crate::read_input))
            .add_systems(Update, release_rewind.run_if(not(super::driving)))
            .add_systems(FixedUpdate, rewind_tick.after(crate::step_physics).before(crate::physics_settled));
    }
}

impl Settings {
    /// The assists the vehicle reads itself (TCS and ABS travel in `Controls`).
    pub fn assists(&self) -> Assists {
        Assists { stm: self.stm, steering: self.steering, shifting: self.shifting }
    }
}

fn assist_input(settings: Res<Settings>, keys: Res<ButtonInput<KeyCode>>, pads: Query<&Gamepad>, mut cars: Query<&mut Car>, mut rewind: ResMut<Rewind>, real: Res<Time<Real>>, mut back_s: Local<f32>) {
    let mut up = keys.just_pressed(KeyCode::KeyE);
    let mut down = keys.just_pressed(KeyCode::KeyQ);
    let mut clutch = if keys.pressed(KeyCode::ShiftLeft) { 1.0f32 } else { 0.0 };
    let mut hold = keys.pressed(KeyCode::KeyX);
    for pad in &pads {
        up |= pad.just_pressed(GamepadButton::East);
        down |= pad.just_pressed(GamepadButton::West);
        if pad.pressed(GamepadButton::LeftTrigger) {
            clutch = 1.0;
        }
    }
    // Pad Back: a tap opens the world map (ui/worldmap.rs), so rewind starts only once Back is held past the tap time.
    let back = pads.iter().any(|p| p.pressed(GamepadButton::Select));
    *back_s = if back { *back_s + real.delta_secs() } else { 0.0 };
    hold |= *back_s >= crate::ui::worldmap::BACK_TAP_S;
    rewind.held = hold && settings.rewind;
    for mut car in &mut cars {
        let v = &mut car.0;
        v.assists = settings.assists();
        v.clutch_pedal = clutch;
        if up {
            v.shift_request = 1;
        } else if down {
            v.shift_request = -1;
        }
    }
}

fn release_rewind(mut rewind: ResMut<Rewind>) {
    rewind.held = false;
}

/// After each fixed tick: record the car, or while rewinding replace it with the previous tick's state (1x speed). The
/// render pose interpolates from the pose on screen to the restored one, so the car slides back smoothly.
fn rewind_tick(mut rewind: ResMut<Rewind>, mut cars: Query<&mut Car>) {
    let Ok(mut car) = cars.single_mut() else { return };
    let max = (REWIND_SECONDS * 120.0) as usize;
    if rewind.car != car.0.data.media_name {
        rewind.history.clear();
        rewind.car = car.0.data.media_name.clone();
    }
    if rewind.held {
        // Keep the oldest state so holding past the start parks the car there.
        let snap = if rewind.history.len() > 1 { rewind.history.pop_back() } else { rewind.history.back().cloned() };
        if let Some(mut s) = snap {
            let v = &car.0;
            (s.prev_position, s.prev_rotation) = (v.position, v.rotation);
            // Keep the player's current assists and drop pending gear changes.
            (s.assists, s.shift_request, s.clutch_pedal) = (v.assists, 0, v.clutch_pedal);
            car.0 = s;
        }
        return;
    }
    if rewind.history.len() >= max {
        rewind.history.pop_front();
    }
    rewind.history.push_back(car.0.clone());
}
