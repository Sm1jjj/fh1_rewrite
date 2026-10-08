// FH1 UI (Anark Gameface) material: unlit diffuse x up to four texture slots, each with its own
// UV transform (docs/UI.md). Written from the decoded scene data, not translated from the game.

#ifdef HUD_2D
#import bevy_sprite::mesh2d_vertex_output::VertexOutput
#else
#import bevy_pbr::forward_io::VertexOutput
#endif

struct AnarkUniform {
    // Material diffuse (0..1) with the node's accumulated opacity folded into alpha.
    color: vec4<f32>,
    // Per slot: rows of the 2x3 UV transform, u' = dot(uv_x.xyz, (u, v, 1)), v' = dot(uv_y.xyz, ...).
    uv_x: array<vec4<f32>, 4>,
    uv_y: array<vec4<f32>, 4>,
    // x = texture slots in use (0..4), y = 1 for FH1's gamma pipeline (see materials.rs GAMMA2),
    // z = bit i set when slot i's texture is gamma-flagged, w = 1 for additive (colour weighted by alpha).
    flags: vec4<u32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> m: AnarkUniform;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var tex0: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var samp0: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var tex1: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var samp1: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(5) var tex2: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(6) var samp2: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(7) var tex3: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(8) var samp3: sampler;

// Xenos texture-fetch gamma (piecewise-linear ≈ x², Xenia's XeGammaToLinear). Slots whose texture
// is gamma-flagged (flags.z bit i) are decoded like this, as the 360 does on fetch.
fn xe_gamma_to_linear(c: vec3<f32>) -> vec3<f32> {
    let a = c * 0.25;
    let b = c * 0.5 - 0.0625;
    let d = c - 0.25;
    let e = c * 2.0 - 1.0;
    return select(select(select(e, d, c < vec3<f32>(0.75)), b, c < vec3<f32>(0.375)), a, c < vec3<f32>(0.25));
}

fn fetch(i: u32, s: vec4<f32>) -> vec4<f32> {
    if (m.flags.y == 1u && ((m.flags.z >> i) & 1u) == 1u) {
        return vec4<f32>(xe_gamma_to_linear(s.rgb), s.a);
    }
    return s;
}

fn slot_uv(i: u32, uv: vec2<f32>) -> vec2<f32> {
    let p = vec3<f32>(uv, 1.0);
    return vec2<f32>(dot(m.uv_x[i].xyz, p), dot(m.uv_y[i].xyz, p));
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
#ifdef HUD_2D
    let uv = in.uv;
#else ifdef VERTEX_UVS_A
    let uv = in.uv;
#else
    let uv = vec2<f32>(0.0);
#endif
    // Material colour: passed raw (byte / 255), as the GPU never gamma-decodes shader constants.
    // GUESS that Anark's host code uploads it unconverted (only the host code could show otherwise).
    var c = m.color;
    let n = m.flags.x;
    if (n > 0u) { c *= fetch(0u, textureSample(tex0, samp0, slot_uv(0u, uv))); }
    if (n > 1u) { c *= fetch(1u, textureSample(tex1, samp1, slot_uv(1u, uv))); }
    if (n > 2u) { c *= fetch(2u, textureSample(tex2, samp2, slot_uv(2u, uv))); }
    if (n > 3u) { c *= fetch(3u, textureSample(tex3, samp3, slot_uv(3u, uv))); }
    if (m.flags.y == 1u) {
        c = vec4<f32>(srgb_to_linear(sqrt(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)))), c.a);
    }
    // Additive (flags.w): FH1 blends SRCALPHA/ONE. Both our additive pipelines (2D premultiplied, 3D Add) add `rgb`
    // and scale the destination by 1 - alpha, so weight by alpha here and keep the destination.
    if (m.flags.w == 1u) {
        c = vec4<f32>(c.rgb * c.a, 0.0);
    }
    return c;
}

// Standard sRGB decode: Bevy re-encodes the view target, so the screen byte is FH1's sqrt value.
fn srgb_to_linear(v: vec3<f32>) -> vec3<f32> {
    let lo = v / 12.92;
    let hi = pow((v + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, v <= vec3<f32>(0.04045));
}
