//! Remaster night lights (W3): the player car's headlights as two Bevy spot lights. The faithful path draws
//! headlights through the game's deferred headlight pass (fh1-render headlight.rs, ~1.5 ms at night); here they
//! are ordinary lights so every PBR material receives them.
//!
//! Only the player car (`FxHeadlightSource { player: true }`) gets lights. Two unshadowed spots keep Bevy on its
//! CPU single-cluster path (engine perf/p6.rs keeps `ClusterConfig::Single` for a few lights).
//! Switch-on = the game's rule (headlight.rs, per-car update 0x8249E340, VERIFIED): before 08:10 or after 18:50.
//! Street-light glows stay fh1-render glow.rs sprites (bright enough to bloom); lamp emissives are W1's materials,
//! which can read `RemasterLighting::lights_on`.
//!
//! Env: FH1_HEADLIGHTS=0 off / =1 always on (as the faithful path), FH1_RM_HEADLIGHT_LM=lumens (per lamp),
//! FH1_RM_HEADLIGHT_SHADOWS=1.

use bevy::prelude::*;

use fh1_render::headlight::FxHeadlightSource;

use crate::light::{env_f32, RemasterLighting};

/// Per lamp. Bevy spreads a spot's lumens over the full sphere (intensity = lm / 4π), so a ~20,000 cd low beam
/// hot spot needs ~250k "lumens" here.
const HEADLIGHT_LM: f32 = 250_000.0;
/// Half the lamp spacing (m) either side of the lamp midpoint.
const HALF_SPACING: f32 = 0.62;

/// A spawned headlight; `owner` = the car.
#[derive(Component)]
pub struct RemasterHeadlight {
    pub owner: Entity,
}

pub(crate) fn plugin(app: &mut App) {
    if std::env::var("FH1_HEADLIGHTS").as_deref() == Ok("0") {
        return;
    }
    app.add_systems(Update, headlights);
}

/// Lights exist only while they are on: any `SpotLight` in the world switches Bevy's clustering back on
/// (engine perf/p6.rs), so daytime keeps none.
fn headlights(
    mut commands: Commands,
    lighting: Res<RemasterLighting>,
    cars: Query<(Entity, &FxHeadlightSource)>,
    mut lights: Query<(Entity, &RemasterHeadlight, &mut SpotLight)>,
) {
    let forced = std::env::var("FH1_HEADLIGHTS").as_deref() == Ok("1");
    let m = lighting.minutes;
    let on = forced || !(8.0 * 60.0 + 10.0..=18.0 * 60.0 + 50.0).contains(&m);
    let lm = env_f32("FH1_RM_HEADLIGHT_LM", HEADLIGHT_LM);
    for (e, h, mut l) in &mut lights {
        let owner_ok = cars.get(h.owner).is_ok_and(|(_, s)| s.player);
        if !on || !owner_ok {
            commands.entity(e).despawn();
        } else if l.intensity != lm {
            l.intensity = lm;
        }
    }
    if !on {
        return;
    }
    let shadows = std::env::var("FH1_RM_HEADLIGHT_SHADOWS").as_deref() == Ok("1");
    for (car, src) in &cars {
        if !src.player || lights.iter().any(|(_, h, _)| h.owner == car) {
            continue;
        }
        for side in [-1.0f32, 1.0] {
            let pos = src.lamp + Vec3::X * side * HALF_SPACING;
            // Forward is -Z in the car's space; aimed ~2.5 degrees down (low beam), toed out slightly.
            let target = pos + Vec3::new(side * 0.6, -0.9, -20.0);
            let light = commands
                .spawn((
                    SpotLight {
                        intensity: lm,
                        range: 90.0,
                        radius: 0.08,
                        color: Color::srgb(1.0, 0.95, 0.86),
                        inner_angle: 0.22,
                        outer_angle: 0.55,
                        shadow_maps_enabled: shadows,
                        ..default()
                    },
                    Transform::from_translation(pos).looking_at(target, Vec3::Y),
                    RemasterHeadlight { owner: car },
                    Name::new("fh1_remaster_headlight"),
                ))
                .id();
            // Kept out of the car probe's cube (car_probe.rs `own_light_layers`).
            if let Some(layers) = crate::car_probe::own_light_layers() {
                commands.entity(light).insert(layers);
            }
            commands.entity(car).add_child(light);
        }
    }
}
