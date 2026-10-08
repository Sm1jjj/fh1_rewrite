//! Remaster renderer (docs/REMASTER.md): FH1's own data drawn through a few bindless uber-shaders.
//! Module owners: material/scenery = W1, car/wheel = W2, light/sky/post/night = W3, batch/instancing = W4.
//! This file is the orchestrator's: send the `mod`/plugin lines you need instead of editing it.

use bevy::prelude::*;

pub mod batch;
pub mod car;
pub mod car_probe;
pub mod car_paint;
pub mod light;
pub mod material;
pub mod night;
pub mod post;
#[cfg(feature = "rtx")]
pub mod rtx;
pub mod scenery;
pub mod sky;
pub mod views;
pub mod wheel;

/// The remaster renderer is on unless `FH1_RENDERER=faithful`. fh1-engine main.rs always sets the variable at startup
/// (Options > Shaders, saved as settings.json `original_shaders`), so an unset variable only occurs in tools/tests.
pub fn enabled() -> bool {
    std::env::var("FH1_RENDERER").map(|v| v.eq_ignore_ascii_case("remaster")).unwrap_or(false)
}

/// Call before `DefaultPlugins` (the `rtx` feature's DLSS needs its project id first). No-op in normal builds.
pub fn pre_default_plugins(_app: &mut App) {
    #[cfg(feature = "rtx")]
    rtx::pre_default_plugins(_app);
}

pub struct RemasterPlugin;

impl Plugin for RemasterPlugin {
    fn build(&self, app: &mut App) {
        batch::plugin(app);
        views::plugin(app);
        if enabled() {
            app.add_plugins(light::RemasterLightPlugin);
            app.add_plugins(car::RemasterCarPlugin);
            app.add_plugins(car_probe::CarProbePlugin);
            scenery::plugin(app);
            #[cfg(feature = "rtx")]
            app.add_plugins(rtx::RtxPlugin);
        }
    }
}
