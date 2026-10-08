// Minimap roads (ui/minimap.rs RoadFogMaterial): MapProfileMinimap's ColourSecondary near the camera, lerped to
// ColourPrimary with view distance, as the old Camera3d's linear DistanceFog did (start = camera distance to the
// focus, end = start + the map's width). Colours are raw profile bytes / 255 (the render target is Rgba8Unorm).
#import bevy_sprite::mesh2d_vertex_output::VertexOutput

struct RoadFog {
    near: vec4<f32>,
    far: vec4<f32>,
    // xyz = camera position (engine space).
    eye: vec4<f32>,
    // x = fog start, y = fog end (metres from the camera).
    range: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> fog: RoadFog;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let d = distance(in.world_position.xyz, fog.eye.xyz);
    let t = clamp((d - fog.range.x) / max(fog.range.y - fog.range.x, 0.001), 0.0, 1.0);
    return mix(fog.near, fog.far, t);
}
