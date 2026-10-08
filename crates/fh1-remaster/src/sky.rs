//! Remaster sky (W3): Bevy's physically based atmosphere instead of FH1's sky dome shaders, sized from the
//! game's own fog data. The haze term's density = the TOD `FogDensity` × the zone's Fog_Templates.xml density
//! (the same product the game uploads as FogConsts.w), so Redrock is hazier than the festival and the sky, the
//! aerial perspective and the sun's colour on surfaces all come from one medium.
//!
//! The camera's `AtmosphereEnvironmentMapLight` (light.rs) bakes this sky into the environment map that lights
//! every PBR material (diffuse + specular), replacing the faithful live cube camera.
//!
//! Env: `FH1_RM_HAZE=k` scales the haze (default 0.12; 0 = clear earth air).

use bevy::light::atmosphere::{Falloff, PhaseFunction, ScatteringMedium, ScatteringTerm};
use bevy::prelude::*;

use fh1_render::lighting::FxTimeOfDay;

/// Planet and atmosphere radii (Bevy's earth).
pub const INNER_RADIUS: f32 = 6_360_000.0;
pub const OUTER_RADIUS: f32 = 6_460_000.0;
const HEIGHT: f32 = OUTER_RADIUS - INNER_RADIUS;
/// Haze scale height (m). Ground haze, like the game's height-faded fog.
const HAZE_SCALE_HEIGHT: f32 = 1_500.0;
/// Haze single-scattering albedo (dust/water haze: mostly scattering).
const HAZE_ALBEDO: f32 = 0.9;

/// The atmosphere entity and its medium.
#[derive(Resource)]
pub struct RemasterSky {
    pub medium: Handle<ScatteringMedium>,
    /// Haze extinction at the ground (1/m) the medium was last built with.
    pub haze: f32,
}

fn haze_scale() -> f32 {
    static K: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *K.get_or_init(|| std::env::var("FH1_RM_HAZE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.12))
}

/// Bevy's earth terms plus FH1's haze (extinction `haze` 1/m at the ground).
pub fn medium_terms(haze: f32) -> Vec<ScatteringTerm> {
    vec![
        // Rayleigh, Mie and ozone as ScatteringMedium::earth.
        ScatteringTerm {
            absorption: Vec3::ZERO,
            scattering: Vec3::new(5.802e-6, 13.558e-6, 33.100e-6),
            falloff: Falloff::Exponential { scale: 8.0 / 60.0 },
            phase: PhaseFunction::Rayleigh,
        },
        ScatteringTerm {
            absorption: Vec3::splat(3.996e-6),
            scattering: Vec3::splat(0.444e-6),
            falloff: Falloff::Exponential { scale: 1.2 / 60.0 },
            phase: PhaseFunction::Mie { asymmetry: 0.8 },
        },
        ScatteringTerm {
            absorption: Vec3::new(0.650e-6, 1.881e-6, 0.085e-6),
            scattering: Vec3::ZERO,
            falloff: Falloff::Tent { center: 0.75, width: 0.3 },
            phase: PhaseFunction::Isotropic,
        },
        // FH1 haze.
        ScatteringTerm {
            absorption: Vec3::splat(haze * (1.0 - HAZE_ALBEDO)),
            scattering: Vec3::splat(haze * HAZE_ALBEDO),
            falloff: Falloff::Exponential { scale: HAZE_SCALE_HEIGHT / HEIGHT },
            phase: PhaseFunction::Mie { asymmetry: 0.75 },
        },
    ]
}

fn build_medium(haze: f32) -> ScatteringMedium {
    ScatteringMedium::new(256, 256, medium_terms(haze)).with_label("fh1_remaster_atmosphere")
}

/// Extinction (1/m, rgb) of the medium at altitude `h` (m above the planet surface).
fn extinction(terms: &[ScatteringTerm], h: f32) -> Vec3 {
    let p = 1.0 - (h / HEIGHT).clamp(0.0, 1.0);
    terms.iter().map(|t| (t.absorption + t.scattering) * t.falloff.sample(p)).sum()
}

/// Transmittance from altitude `h0` towards a light at elevation `sin_el` (CPU copy of the GPU transmittance LUT,
/// used to divide the atmosphere's own reddening out of the sun colour so surfaces get the TOD colour). Zero
/// when the ray hits the planet.
pub fn transmittance(terms: &[ScatteringTerm], h0: f32, sin_el: f32) -> Vec3 {
    let r0 = INNER_RADIUS + h0.max(0.0);
    let mu = sin_el.clamp(-1.0, 1.0);
    // Ray r0 + t·d against the planet and the top of the atmosphere.
    let b = r0 * mu;
    let c_ground = r0 * r0 - INNER_RADIUS * INNER_RADIUS;
    if mu < 0.0 && b * b - c_ground >= 0.0 {
        return Vec3::ZERO;
    }
    let c_top = r0 * r0 - OUTER_RADIUS * OUTER_RADIUS;
    let t_max = -b + (b * b - c_top).max(0.0).sqrt();
    const STEPS: usize = 64;
    // Quadratic step spacing: dense near the ground where the haze is.
    let mut depth = Vec3::ZERO;
    let mut t_prev = 0.0;
    for i in 1..=STEPS {
        let f = i as f32 / STEPS as f32;
        let t = t_max * f * f;
        let tm = 0.5 * (t + t_prev);
        let r = (r0 * r0 + 2.0 * r0 * mu * tm + tm * tm).sqrt();
        depth += extinction(terms, r - INNER_RADIUS) * (t - t_prev);
        t_prev = t;
    }
    (-depth).exp()
}

/// The haze extinction (1/m) the game's data asks for now: TOD FogDensity × zone template density × 0.01
/// (FogConsts.w packing, fh1-render lighting.rs), × FH1_RM_HAZE.
pub fn haze_now(t: &FxTimeOfDay) -> f32 {
    t.tod.scalar("FogDensity", t.minutes()) * t.fog.density * 0.01 * haze_scale()
}

pub(crate) fn setup_sky(mut commands: Commands, mut media: ResMut<Assets<ScatteringMedium>>, tod: Option<Res<FxTimeOfDay>>) {
    let haze = tod.as_deref().map(haze_now).unwrap_or(0.0);
    let medium = media.add(build_medium(haze));
    // Planet centre below the origin (Colorado's ground is within a few hundred metres of y = 0).
    commands.spawn((
        bevy::light::Atmosphere { inner_radius: INNER_RADIUS, outer_radius: OUTER_RADIUS, ground_albedo: Vec3::splat(0.25), medium: medium.clone() },
        Transform::from_translation(-Vec3::Y * INNER_RADIUS),
        Name::new("fh1_remaster_atmosphere"),
    ));
    commands.insert_resource(RemasterSky { medium, haze });
}

/// Follows the time of day and the zone fog template. Rebuilding the medium re-creates its LUTs, so it only
/// happens when the haze moved by more than 4 % (zone blends take ~15 s; TOD curves are slow).
pub(crate) fn update_sky(tod: Option<Res<FxTimeOfDay>>, sky: Option<ResMut<RemasterSky>>, mut media: ResMut<Assets<ScatteringMedium>>) {
    let (Some(t), Some(mut sky)) = (tod, sky) else { return };
    let haze = haze_now(&t);
    let rel = (haze - sky.haze).abs() / sky.haze.max(1e-7);
    if rel < 0.04 && (haze > 0.0) == (sky.haze > 0.0) {
        return;
    }
    sky.haze = haze;
    if let Some(mut m) = media.get_mut(&sky.medium) {
        *m = build_medium(haze);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmittance_sane() {
        let terms = medium_terms(0.0);
        let zenith = transmittance(&terms, 0.0, 1.0);
        // Clear earth air straight up: ~0.9 red, ~0.7 blue.
        assert!(zenith.x > 0.85 && zenith.x < 0.99 && zenith.z > 0.55 && zenith.z < zenith.x, "{zenith}");
        let low = transmittance(&terms, 0.0, 0.05);
        assert!(low.z < zenith.z && low.x > low.z);
        assert_eq!(transmittance(&terms, 0.0, -0.2), Vec3::ZERO);
        let hazy = transmittance(&medium_terms(1.5e-4), 0.0, 0.5);
        assert!(hazy.y < transmittance(&terms, 0.0, 0.5).y);
    }
}
