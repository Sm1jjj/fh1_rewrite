// Half-resolution effects (fx_half_res.rs): the two fullscreen passes around the half-res effect draws.
//
// `downsample` (pass "fx_half_depth"): the main depth -> the half-res Depth32Float, one 2x2 block per texel, keeping the
// NEAREST (reverse-Z max) on odd checkerboard texels and the FARTHEST (min) on even ones (the "min-max depth" trick:
// along an edge half the texels hold each side, so the composite always finds a half-res sample of the surface a full-res
// pixel sits on, instead of a min-only / max-only depth that leaks over or eats into every silhouette).
//
// `composite` (pass "fx_half_composite", COMPOSITE): the half-res (premultiplied light, transmittance) into the main HDR
// target as `dst = light + dst * transmittance` (blend One, SrcAlpha). Depth-aware upsample: the 4 half-res texels
// around the full-res pixel are bilinear-weighted when all their depths lie within FX_HALF_DEPTH_TOL (per mille,
// relative view distance) of the pixel's own depth; otherwise the texel whose depth is nearest the pixel's wins
// (nearest-depth upsampling), so smoke behind a car body stays behind its edge instead of bleeding a half-res halo.
// Reverse-Z infinite perspective: view distance = near / d, so the relative distance difference of two depths is
// |d1 - d2| / min(d1, d2) and the near plane is not needed.

#import bevy_render::view::View

@group(0) @binding(0) var<uniform> view: View;
#ifdef MULTISAMPLED
@group(0) @binding(1) var full_depth: texture_depth_multisampled_2d;
#else
@group(0) @binding(1) var full_depth: texture_depth_2d;
#endif
#ifdef COMPOSITE
@group(0) @binding(2) var half_colour: texture_2d<f32>;
@group(0) @binding(3) var half_depth: texture_depth_2d;
#endif

@vertex
fn vertex(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32(vi >> 1u), f32(vi & 1u)) * 2.0;
    return vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}

// Main depth at a target pixel (sample 0 when multisampled), clamped to the texture.
fn load_full(p: vec2<i32>) -> f32 {
    let n = vec2<i32>(textureDimensions(full_depth)) - 1;
    return textureLoad(full_depth, clamp(p, vec2<i32>(0), n), 0);
}

@fragment
fn downsample(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    let h = vec2<i32>(p.xy);
    let o = vec2<i32>(view.viewport.xy) + h * 2;
    let a = load_full(o);
    let b = load_full(o + vec2<i32>(1, 0));
    let c = load_full(o + vec2<i32>(0, 1));
    let d = load_full(o + vec2<i32>(1, 1));
    let near = max(max(a, b), max(c, d));
    let far = min(min(a, b), min(c, d));
    return select(far, near, ((h.x + h.y) & 1) == 1);
}

#ifdef COMPOSITE
// Relative view-distance difference of two reverse-Z depths (0 = same distance; sky = 0 matches only sky).
fn rel(z: f32, full: f32) -> f32 {
    return abs(z - full) / max(min(z, full), 1e-7);
}

@fragment
fn composite(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<i32>(textureDimensions(half_colour)) - 1;
    // Half-res texel i covers full-res pixels 2i, 2i + 1: its centre is at full-res 2i + 1.
    let h = (p.xy - view.viewport.xy) * 0.5 - 0.5;
    let base = floor(h);
    let f = h - base;
    let b = vec2<i32>(base);
    let c0 = clamp(b, vec2<i32>(0), dims);
    let c1 = clamp(b + vec2<i32>(1, 0), vec2<i32>(0), dims);
    let c2 = clamp(b + vec2<i32>(0, 1), vec2<i32>(0), dims);
    let c3 = clamp(b + vec2<i32>(1, 1), vec2<i32>(0), dims);
    let s0 = textureLoad(half_colour, c0, 0);
    let s1 = textureLoad(half_colour, c1, 0);
    let s2 = textureLoad(half_colour, c2, 0);
    let s3 = textureLoad(half_colour, c3, 0);
    // Nothing drawn around this pixel: leave the target untouched (no blend traffic for the empty screen).
    let lit = max(max(s0.rgb, s1.rgb), max(s2.rgb, s3.rgb));
    if (min(min(s0.a, s1.a), min(s2.a, s3.a)) >= 0.9999 && max(lit.r, max(lit.g, lit.b)) <= 0.0) {
        discard;
    }
    let full = load_full(vec2<i32>(p.xy));
    let r0 = rel(textureLoad(half_depth, c0, 0), full);
    let r1 = rel(textureLoad(half_depth, c1, 0), full);
    let r2 = rel(textureLoad(half_depth, c2, 0), full);
    let r3 = rel(textureLoad(half_depth, c3, 0), full);
    let tol = f32(#{FX_HALF_DEPTH_TOL}) * 0.001;
    if (max(max(r0, r1), max(r2, r3)) <= tol) {
        let top = mix(s0, s1, f.x);
        let bottom = mix(s2, s3, f.x);
        return mix(top, bottom, f.y);
    }
    var best = s0;
    var r = r0;
    if (r1 < r) { best = s1; r = r1; }
    if (r2 < r) { best = s2; r = r2; }
    if (r3 < r) { best = s3; r = r3; }
    return best;
}
#endif
