#![expect(missing_docs, reason = "Not all docs are written yet, see #3492.")]

//! Ray-traced lighting for Bevy: a fork of `bevy_solari` 0.19.1 with a sky term,
//! alpha-tested and light-transparent geometry, and a reference pathtracer that
//! accumulates.
//!
//! See [`SolarikPlugins`] for more info.

extern crate alloc;

pub mod pathtracer;
pub mod realtime;
pub mod scene;

/// The solarik prelude.
///
/// This includes the most common types in this crate, re-exported for your convenience.
pub mod prelude {
    pub use super::SolarikPlugins;
    pub use crate::realtime::SolarikLighting;
    pub use crate::scene::{RaytracingMesh3d, SolarikAlphaTesting, SolarikSkyLight};
}

use crate::realtime::SolarikLightingPlugin;
use crate::scene::RaytracingScenePlugin;
use bevy_app::{PluginGroup, PluginGroupBuilder};
use bevy_render::settings::WgpuFeatures;

/// An experimental set of plugins for raytraced lighting.
///
/// This plugin group provides:
/// * [`SolarikLightingPlugin`] - Raytraced direct and indirect lighting.
/// * [`RaytracingScenePlugin`] - BLAS building, resource and lighting binding.
///
/// There's also:
/// * [`pathtracer::PathtracingPlugin`] - A non-realtime pathtracer for validation purposes (not added by default).
///
/// To get started, add this plugin to your app, and then add `RaytracingMesh3d` and `MeshMaterial3d::<StandardMaterial>` to your entities.
pub struct SolarikPlugins;

impl PluginGroup for SolarikPlugins {
    fn build(self) -> PluginGroupBuilder {
        PluginGroupBuilder::start::<Self>()
            .add(RaytracingScenePlugin)
            .add(SolarikLightingPlugin)
    }
}

impl SolarikPlugins {
    /// [`WgpuFeatures`] required for these plugins to function.
    pub fn required_wgpu_features() -> WgpuFeatures {
        WgpuFeatures::EXPERIMENTAL_RAY_QUERY
            | WgpuFeatures::BUFFER_BINDING_ARRAY
            | WgpuFeatures::TEXTURE_BINDING_ARRAY
            | WgpuFeatures::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING
            | WgpuFeatures::PARTIALLY_BOUND_BINDING_ARRAY
    }
}
