// FH1 car drop shadow (CDropShader), media/shaders/Cars/DropShadow.fxobj "Default" technique translated by hand
// (VERIFIED from the microcode; re/out/dropshadow/NOTES.md, docs/SHADOWS.md "Car"):
// VS: P = D[vid] (positions come only from c_DisplaceDropShadow; here the CPU writes them into the mesh, world
//     space), d = |P - cam|, fog = sat(1 - 2^-(max(d - c14.x, 0) * c14.w)), o1.x = d * c16.x + c16.y.
// PS: a = sat(tex.r * lerp(1, c3.z, sat(o1.x))) * vertexAlpha * edge fade (fadeDistance c1.xy on u and v);
//     rgb = sqrt(lerp(c_shadowColor, fogColor, fog)). Blend SRCALPHA / INVSRCALPHA in the game's gamma-2 target.

#import bevy_pbr::mesh_view_bindings::view

struct DropShadowParams {
    // rgb = DropShadowColor, w = 1: raw output (FH1 post chain on).
    colour: vec4<f32>,
    // xy = fadeDistance, z = psSceneShadowFadeParams.z.
    fade: vec4<f32>,
    fog_colour: vec4<f32>,
    // vsFogParameters: x = start, w = density.
    fog: vec4<f32>,
    // vsSceneShadowFadeParams xy.
    scene_fade: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> g: DropShadowParams;

struct In {
    @location(0) pos: vec3<f32>,
    @location(1) uv: vec2<f32>,
    // a = the corner's alpha (D[vid].w).
    @location(2) colour: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    // uv, vertex alpha, fog.
    @location(0) o0: vec4<f32>,
    @location(1) fade: f32,
}

@vertex
fn vertex(v: In) -> Out {
    var o: Out;
    o.clip = view.clip_from_world * vec4<f32>(v.pos, 1.0);
    let d = distance(v.pos, view.world_position);
    let fog = clamp(1.0 - exp2(-(max(d - g.fog.x, 0.0) * g.fog.w)), 0.0, 1.0);
    o.o0 = vec4<f32>(v.uv, v.colour.a, fog);
    o.fade = d * g.scene_fade.x + g.scene_fade.y;
    return o;
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    let uv = i.o0.xy;
    let t = textureSample(tex, samp, uv).r;
    let scene = mix(1.0, g.fade.z, clamp(i.fade, 0.0, 1.0));
    let fx = max(g.fade.x, 1e-5);
    let fy = max(g.fade.y, 1e-5);
    let edge = clamp(uv.x / fx, 0.0, 1.0) * clamp((1.0 - uv.x) / fx, 0.0, 1.0) * clamp(uv.y / fy, 0.0, 1.0) * clamp((1.0 - uv.y) / fy, 0.0, 1.0);
    let a = clamp(t * scene, 0.0, 1.0) * i.o0.z * edge;
    let c = mix(g.colour.rgb, g.fog_colour.rgb, i.o0.w);
    // FH1_DROPSHADOW_DEBUG: opaque, r = final alpha, g = silhouette, b = vertex alpha.
    if (g.colour.w > 1.5) {
        return vec4<f32>(a, t, i.o0.z, 1.0);
    }
    if (g.colour.w > 0.5) {
        return vec4<f32>(sqrt(max(c, vec3<f32>(0.0))), a);
    }
    // Without the FH1 post chain the target holds linear colour: black blends as dst·(1-a)² in the game's gamma-2 space.
    let lin_a = 1.0 - (1.0 - a) * (1.0 - a);
    return vec4<f32>(c, lin_a);
}
