// FH1 vector text (Loop-Blinn glyph meshes from the .dt fonts). Same maths as the game's text
// pixel shader (docs/UI.md "Vector-text shading"): f = (u^2 - v) * sign, d = f / |grad f| in
// pixels, alpha = saturate(0.5 - d), colour = lerp(text, outline, saturate(0.5 + d + width/2)).
// The inner mesh is drawn with sign +1, the outer (mirrored) mesh with sign -1.

#ifdef HUD_2D
#import bevy_sprite::mesh2d_vertex_output::VertexOutput
#else
#import bevy_pbr::forward_io::VertexOutput
#endif

struct TextUniform {
    text_colour: vec4<f32>,
    outline_colour: vec4<f32>,
    // x = outline width (px), y = sign (+1 inner, -1 outer), z = 1 for FH1's gamma-2 output.
    params: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> t: TextUniform;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
#ifdef HUD_2D
    let uv = in.uv;
#else ifdef VERTEX_UVS_A
    let uv = in.uv;
#else
    let uv = vec2<f32>(0.0);
#endif
    let f = (uv.x * uv.x - uv.y) * t.params.y;
    let g = vec2<f32>(dpdx(f), dpdy(f));
    let d = f / max(length(g), 1e-6);
    let cover = clamp(0.5 - d, 0.0, 1.0);
    let k = clamp(0.5 + d + 0.5 * t.params.x, 0.0, 1.0);
    var c = mix(t.text_colour, t.outline_colour, select(0.0, k, t.params.x > 0.0));
    // Text colours are shader constants: raw, as uploaded (no hardware gamma decode).
    if (t.params.z > 0.5) {
        c = vec4<f32>(srgb_to_linear(sqrt(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)))), c.a);
    }
    if (cover * c.a <= 0.0) {
        discard;
    }
    return vec4<f32>(c.rgb, c.a * cover);
}

// Standard sRGB decode: Bevy re-encodes the view target, so the screen byte is FH1's sqrt value.
fn srgb_to_linear(v: vec3<f32>) -> vec3<f32> {
    let lo = v / 12.92;
    let hi = pow((v + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, v <= vec3<f32>(0.04045));
}
