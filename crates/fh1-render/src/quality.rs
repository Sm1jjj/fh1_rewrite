//! Graphics quality preset (P8-A, 2026-10-08): one resource that the owners of shadows, the car probe, draw distance,
//! particles and crowds read. fh1-engine `ui/graphics.rs` fills it from settings.json `graphics.quality`
//! (Options > Graphics > Quality) and replaces it when the preset changes, so readers can use `Res::is_changed`.
//! High = the values the game ran with before the preset existed; nothing reads it unless its owner wired it.

use bevy::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum QualityPreset {
    /// P17-A: 360-class hardware (i3 + integrated graphics): the game's own budget (720p cap, 512² shadows, shorter
    /// draw distance, no contact shadows, rare probe bakes, thin crowds / particles).
    Console,
    Low,
    Medium,
    #[default]
    High,
    Ultra,
}

#[derive(Resource, Clone, Debug, PartialEq)]
pub struct GraphicsQuality {
    pub preset: QualityPreset,
    /// Directional shadow map size per cascade (texels). Bevy has one size for every cascade (one texture array).
    pub shadow_res: u32,
    /// Main-camera shadow cascades. The first always ends at 9 m (the player car); the last one is the cached far cascade.
    pub cascades: u32,
    /// Frames between refreshes of the cached far (last) cascade (fh1-remaster light.rs `cache_far_cascade`; 1 = every
    /// frame, no caching). Camera moves / turns / cuts and light changes refresh it sooner.
    pub shadow_far_refresh: u32,
    /// Multiplier on LOD / cull distances (1 = today's; read through [`draw_distance`]).
    pub draw_distance: f32,
    /// Car reflection probe: seconds between bakes and face size.
    pub probe_interval_s: f32,
    pub probe_res: u32,
    /// Multiplier on particle spawn counts (smoke, surface FX; read through [`particles`]).
    pub particles: f32,
    /// Multiplier on the crowd's 3D figure count (read through [`crowd_density`]).
    pub crowd_density: f32,
    /// Screen-space contact shadows on the sun / moon (fh1-remaster light.rs; FH1_RM_CONTACT_SHADOWS=0 forces off).
    pub contact_shadows: bool,
}

impl GraphicsQuality {
    pub fn from_preset(preset: QualityPreset) -> Self {
        // Shadows (2026-10-08 night, P14 "3 -> 2 cascades"): High = 2 cascades (9 m car cascade + one 9-300 m cascade,
        // cached 3 frames in 4) at 2048², so the 9-50 m band keeps ~0.4 m texels instead of 0.75 m at 1024² (the old 3 x
        // 1024² had ~0.13 m there). One shadow view fewer per frame. FH1_RM_CASCADES=old = 3 x 1024² (the old High).
        // Console: the car cascade + a far cascade at 512² (the game's own size), the far one refreshed every 8 frames.
        let (shadow_res, cascades, shadow_far_refresh) = match preset {
            QualityPreset::Console => (512, 2, 8),
            QualityPreset::Low => (1024, 2, 8),
            QualityPreset::Medium => (1024, 2, 4),
            QualityPreset::High => (2048, 2, 4),
            QualityPreset::Ultra => (2048, 3, 2),
        };
        // Console draw distance 0.65: props at ~0.8x the game's LOD distances (the baked bands are 1.25x), zone culls at
        // the game's (1.6 x 0.65 -> 1.0). Probe: a 64² bake every 8 s moving (the probe camera sleeps in between).
        let (draw_distance, probe_interval_s, probe_res, particles, crowd_density) = match preset {
            QualityPreset::Console => (0.65, 2.0, 64, 0.4, 0.2),
            QualityPreset::Low => (0.7, 1.0, 64, 0.5, 0.3),
            QualityPreset::Medium => (0.85, 0.5, 128, 0.75, 0.6),
            QualityPreset::High => (1.0, 0.25, 256, 1.0, 1.0),
            QualityPreset::Ultra => (1.25, 0.125, 256, 1.0, 1.0),
        };
        let contact_shadows = preset != QualityPreset::Console;
        Self { preset, shadow_res, cascades, shadow_far_refresh, draw_distance, probe_interval_s, probe_res, particles, crowd_density, contact_shadows }
    }
}

/// The preset values in effect (P17-A), as f32 bits, for readers without access to the resource (the render world's
/// static world cull, scenery streaming's free functions, particle pools). 1.0 until [`GraphicsQuality::publish`] runs.
static DRAW_DISTANCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f80_0000);
static PARTICLES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f80_0000);
static CROWD_DENSITY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f80_0000);

fn load(a: &std::sync::atomic::AtomicU32) -> f32 {
    f32::from_bits(a.load(std::sync::atomic::Ordering::Relaxed))
}

impl GraphicsQuality {
    /// Makes this preset's values the ones [`draw_distance`], [`particles`] and [`crowd_density`] return (fh1-engine
    /// ui/graphics.rs calls it whenever the resource changes).
    pub fn publish(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        DRAW_DISTANCE.store(self.draw_distance.to_bits(), Relaxed);
        PARTICLES.store(self.particles.to_bits(), Relaxed);
        CROWD_DENSITY.store(self.crowd_density.to_bits(), Relaxed);
    }
}

/// Multiplier on particle spawn counts (P17-A; particles.rs `count`: tyre smoke and surface FX; backfire bursts and the
/// volume smoke are not scaled). 1 = today's.
pub fn particles() -> f32 {
    load(&PARTICLES).clamp(0.0, 1.0)
}

/// Multiplier on the crowd's 3D figure count (P17-A; fh1-engine crowd.rs: fewer skinned figures near the car, the sprite
/// cards take over sooner). 1 = today's.
pub fn crowd_density() -> f32 {
    load(&CROWD_DENSITY).clamp(0.0, 1.0)
}

/// Multiplier on LOD / cull distances (P17-A): the static world's LOD bands (cull uniform `eye.w` = 1 / this), prop LOD
/// level streaming and the zone culls. Capped at 1 (shrink only): pushing bands further out would ask for LODs the
/// streaming radii never load. `FH1_DRAW_DISTANCE=<x>` overrides the preset; 1 = today's distances (old).
pub fn draw_distance() -> f32 {
    static ENV: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    let env = *ENV.get_or_init(|| std::env::var("FH1_DRAW_DISTANCE").ok().and_then(|v| v.parse().ok()).filter(|k: &f32| *k > 0.0));
    env.unwrap_or_else(|| load(&DRAW_DISTANCE)).clamp(0.25, 1.0)
}

impl Default for GraphicsQuality {
    fn default() -> Self {
        Self::from_preset(QualityPreset::High)
    }
}
