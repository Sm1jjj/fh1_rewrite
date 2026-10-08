//! Colorado objects (docs/PROPS.md). The static ones (GameObjs flyers / speed cameras / barns, the
//! animated scenes' rest-pose templates, the free-roam conditional `.pgeo` groups) are written by
//! fh1setup's scenery group and streamed with the other props (`scenery.rs`); this module holds what
//! the props path doesn't cover.
//!
//! Debug hook for checking placements: `FH1_TELEPORT=x,y,z[,yaw_deg]` (engine space) holds the car
//! at that point, on the collision surface below it when there is one, so the scenery streams
//! around it (e.g. with `FH1_SHOT` for a screenshot of an object).

use bevy::prelude::*;


use crate::track::Track;
use crate::Car;

/// Keeps the car at `FH1_TELEPORT` (re-placed every frame so it can't fall through open terrain
/// without collision).
pub fn teleport(track: Res<Track>, mut cars: Query<&mut Car>, mut target: Local<Option<Option<(Vec3, f32)>>>) {
    let t = *target.get_or_insert_with(|| {
        let v: Vec<f32> = std::env::var("FH1_TELEPORT").ok()?.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        let p = Vec3::new(*v.first()?, *v.get(1)?, *v.get(2)?);
        let yaw = v.get(3).copied().unwrap_or(0.0).to_radians();
        let ground = track.ground.ray(p + Vec3::Y * 20.0, Vec3::NEG_Y, 60.0).map_or(p, |h| h.point);
        info!("FH1_TELEPORT: holding the car at {ground} (asked {p}), yaw {yaw}");
        Some((ground, yaw))
    });
    let Some((point, yaw)) = t else { return };
    for mut car in &mut cars {
        car.0.place(point, yaw);
    }
}
