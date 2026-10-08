// GPU crowd figures, prepass / shadow pipelines: crowd_skin.wgsl's skinning (keep both in step) with Bevy's prepass
// vertex outputs. The prepass view bind group has the globals at binding 1.

#import bevy_pbr::{
    mesh_functions,
    prepass_io::VertexOutput,
    view_transformations::position_world_to_clip,
}
#import bevy_render::globals::Globals

@group(0) @binding(1) var<uniform> crowd_globals: Globals;
@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<storage, read> crowd_mats: array<vec4<f32>>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var<storage, read> crowd_figs: array<vec4<f32>>;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(3) normal: vec3<f32>,
    @location(8) bones: vec4<f32>,
}

fn crowd_dt(now: f32, start: f32) -> f32 {
    var d = now - start;
    if (d < -1800.0) {
        d = d + 3600.0;
    }
    if (d > 1800.0) {
        d = d - 3600.0;
    }
    return d;
}

fn crowd_bone(i: u32) -> mat4x4<f32> {
    let r0 = crowd_mats[i * 3u];
    let r1 = crowd_mats[i * 3u + 1u];
    let r2 = crowd_mats[i * 3u + 2u];
    return transpose(mat4x4<f32>(r0, r1, r2, vec4<f32>(0.0, 0.0, 0.0, 1.0)));
}

fn crowd_skin(slot: u32, bones: vec4<f32>, now: f32) -> mat4x4<f32> {
    let s0 = crowd_figs[slot * 3u];
    let s1 = crowd_figs[slot * 3u + 1u];
    let s2 = crowd_figs[slot * 3u + 2u];
    var c = s0;
    var dur = s2.y;
    var t = crowd_dt(now, s0.w);
    let tn = crowd_dt(now, s1.w);
    if (s1.y > 0.0 && tn >= 0.0) {
        c = s1;
        dur = s2.z;
        t = tn;
    }
    let nb = u32(s2.x);
    let frames = max(c.y, 1.0);
    let x = clamp(t, 0.0, max(dur, 0.0)) / max(c.z, 1e-4);
    let f0 = min(floor(x), frames - 1.0);
    let f1 = min(f0 + 1.0, frames - 1.0);
    let k = clamp(x - f0, 0.0, 1.0);
    let b0 = u32(c.x) + u32(f0) * nb;
    let b1 = u32(c.x) + u32(f1) * nb;
    let j0 = u32(bones.x);
    let j1 = u32(bones.y);
    let m0 = crowd_bone(b0 + j0) * (1.0 - k) + crowd_bone(b1 + j0) * k;
    let m1 = crowd_bone(b0 + j1) * (1.0 - k) + crowd_bone(b1 + j1) * k;
    return m0 * bones.z + m1 * bones.w;
}

@vertex
fn vertex(v: Vertex) -> VertexOutput {
    var out: VertexOutput;
    let slot = mesh_functions::get_tag(v.instance_index);
    let skin = crowd_skin(slot, v.bones, crowd_globals.time);
    let local = skin * vec4<f32>(v.position, 1.0);
    let world_from_local = mesh_functions::get_world_from_local(v.instance_index);
    out.world_position = mesh_functions::mesh_position_local_to_world(world_from_local, vec4<f32>(local.xyz, 1.0));
    out.position = position_world_to_clip(out.world_position.xyz);
#ifdef UNCLIPPED_DEPTH_ORTHO_EMULATION
    out.unclipped_depth = out.position.z;
    out.position.z = min(out.position.z, 1.0);
#endif
#ifdef VERTEX_UVS_A
    out.uv = v.uv;
#endif
#ifdef NORMAL_PREPASS_OR_DEFERRED_PREPASS
    out.world_normal = mesh_functions::mesh_normal_local_to_world((skin * vec4<f32>(v.normal, 0.0)).xyz, v.instance_index);
#endif
#ifdef MOTION_VECTOR_PREPASS
    // Previous pose not kept: the motion vector covers the body's movement only (spectators barely move).
    out.previous_world_position = mesh_functions::mesh_position_local_to_world(
        mesh_functions::get_previous_world_from_local(v.instance_index), vec4<f32>(local.xyz, 1.0));
#endif
#ifdef VERTEX_OUTPUT_INSTANCE_INDEX
    out.instance_index = v.instance_index;
#endif
#ifdef VISIBILITY_RANGE_DITHER
    out.visibility_range_dither = mesh_functions::get_visibility_range_dither_level(v.instance_index, world_from_local[3]);
#endif
    return out;
}
