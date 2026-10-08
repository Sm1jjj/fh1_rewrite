// Shader import for effects drawn by the half-res effects pass (fx_half_res.rs). The pass compiles an effect's own
// shader with the defs `FX_HALF_RES` and `MATERIAL_BIND_GROUP` = 1; the effect swaps its Bevy mesh-view imports for this
// module:
//
//   #ifdef FX_HALF_RES
//   #import fh1_render::fx_half::{view, globals, get_world_from_local}
//   #else
//   #import bevy_pbr::mesh_view_bindings::{view, globals}
//   #import bevy_pbr::mesh_functions::get_world_from_local
//   #endif
//
// Group 0 is Bevy's own view uniform (the same `View` struct and buffer the mesh pipeline binds, so every field reads as
// it does there; `viewport` is the FULL-res viewport, the half-res target is half its size) and Bevy's globals (time,
// delta_time, frame_count). Batches are world space, so the "mesh transform" is the identity. Group 1 = the material:
// 0 texture_2d<f32>, 1 sampler, 2 uniform (FxBatch::params), 3 texture_2d<f32>, 4 sampler (missing textures = white).

#define_import_path fh1_render::fx_half

#import bevy_render::view::View
#import bevy_render::globals::Globals

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<uniform> globals: Globals;

fn get_world_from_local(instance: u32) -> mat4x4<f32> {
    return mat4x4<f32>(
        vec4<f32>(1.0, 0.0, 0.0, 0.0),
        vec4<f32>(0.0, 1.0, 0.0, 0.0),
        vec4<f32>(0.0, 0.0, 1.0, 0.0),
        vec4<f32>(0.0, 0.0, 0.0, 1.0),
    );
}
