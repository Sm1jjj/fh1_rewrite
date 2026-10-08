//! Graphics quality preset (P8-A, 2026-10-08): one resource that the owners of shadows, the car probe, draw distance,
//! particles and crowds read. fh1-engine `ui/graphics.rs` fills it from settings.json `graphics.quality`
//! (Options > Graphics > Quality) and replaces it when the preset changes, so readers can use `Res::is_changed`.
//! High = the values the game ran with before the preset existed; nothing reads it unless its owner wired it.

use bevy::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum QualityPreset {
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
    /// Multiplier on stream / LOD / cull distances (1 = today's).
    pub draw_distance: f32,
    /// Car reflection probe: seconds between bakes and face size.
    pub probe_interval_s: f32,
    pub probe_res: u32,
    /// Multiplier on particle spawn counts (smoke, surface FX).
    pub particles: f32,
    /// Multiplier on crowd figure counts.
    pub crowd_density: f32,
}

impl GraphicsQuality {
    pub fn from_preset(preset: QualityPreset) -> Self {
        // Shadows (2026-10-08 night, P14 "3 -> 2 cascades"): High = 2 cascades (9 m car cascade + one 9-300 m cascade,
        // cached 3 frames in 4) at 2048², so the 9-50 m band keeps ~0.4 m texels instead of 0.75 m at 1024² (the old 3 x
        // 1024² had ~0.13 m there). One shadow view fewer per frame. FH1_RM_CASCADES=old = 3 x 1024² (the old High).
        let (shadow_res, cascades, shadow_far_refresh) = match preset {
            QualityPreset::Low => (1024, 2, 8),
            QualityPreset::Medium => (1024, 2, 4),
            QualityPreset::High => (2048, 2, 4),
            QualityPreset::Ultra => (2048, 3, 2),
        };
        let (draw_distance, probe_interval_s, probe_res, particles, crowd_density) = match preset {
            QualityPreset::Low => (0.7, 1.0, 64, 0.5, 0.3),
            QualityPreset::Medium => (0.85, 0.5, 128, 0.75, 0.6),
            QualityPreset::High => (1.0, 0.25, 256, 1.0, 1.0),
            QualityPreset::Ultra => (1.25, 0.125, 256, 1.0, 1.0),
        };
        Self { preset, shadow_res, cascades, shadow_far_refresh, draw_distance, probe_interval_s, probe_res, particles, crowd_density }
    }
}

impl Default for GraphicsQuality {
    fn default() -> Self {
        Self::from_preset(QualityPreset::High)
    }
}
