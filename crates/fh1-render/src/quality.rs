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
    /// Directional shadow map size per cascade (texels).
    pub shadow_res: u32,
    /// Main-camera shadow cascades.
    pub cascades: u32,
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
        let (shadow_res, cascades, draw_distance, probe_interval_s, probe_res, particles, crowd_density) = match preset {
            QualityPreset::Low => (512, 2, 0.7, 1.0, 64, 0.5, 0.3),
            QualityPreset::Medium => (1024, 2, 0.85, 0.5, 128, 0.75, 0.6),
            QualityPreset::High => (1024, 3, 1.0, 0.25, 256, 1.0, 1.0),
            QualityPreset::Ultra => (2048, 4, 1.25, 0.125, 256, 1.0, 1.0),
        };
        Self { preset, shadow_res, cascades, draw_distance, probe_interval_s, probe_res, particles, crowd_density }
    }
}

impl Default for GraphicsQuality {
    fn default() -> Self {
        Self::from_preset(QualityPreset::High)
    }
}
