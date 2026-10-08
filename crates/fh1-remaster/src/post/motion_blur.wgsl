// Remaster camera motion blur (post/motion_blur.rs). Camera motion only: each pixel's past screen position comes from its
// depth and the reprojection matrix (this frame's clip -> last frame's clip), no motion-vector prepass. Trails run in ONE
// direction (from the pixel towards where it was), with uniform taps (no noise), then a second short pass along the same
// per-pixel trail fills the gaps between the first pass's taps (the "separable" smoothing of the Fox Engine / CryEngine
// sample). Near pixels (< start m) and the player car are not blurred, and they never bleed into the trails of others.

#ifdef MULTISAMPLED
@group(0) @binding(0) var depth_tex: texture_multisampled_2d<f32>;
#else
@group(0) @binding(0) var depth_tex: texture_2d<f32>;
#endif
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var src_s: sampler;
struct Params {
    // This frame's clip space -> last frame's clip space (camera motion only).
    reproj: mat4x4<f32>,
    // x = trail scale (exposure time / frame time), y = max trail (pixels), z = near plane (m), w = pass-1 taps.
    a: vec4<f32>,
    // x = blur start depth (m), y = full blur depth (m), z = car mask far depth (m), w = pass-2 taps.
    b: vec4<f32>,
    // Player car screen rect (uv min.xy, max.xy); far outside the screen = no car.
    car: vec4<f32>,
    // x = car rect feather (uv), y = car depth feather (m).
    c: vec4<f32>,
};
@group(0) @binding(3) var<uniform> p: Params;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vertex(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

// Reverse-Z depth at a uv (sample 0 of an MSAA depth buffer: the blur is low-frequency, one sample is plenty).
fn depth_at(uv: vec2<f32>) -> f32 {
    let dim = vec2<i32>(textureDimensions(depth_tex));
    let px = clamp(vec2<i32>(uv * vec2<f32>(dim)), vec2<i32>(0), dim - vec2<i32>(1));
    return textureLoad(depth_tex, px, 0).x;
}

// Blur amount of a pixel: 0 near the camera and on the player car, rising smoothly to 1 far away (the sky: depth 0).
fn blur_factor(uv: vec2<f32>, d: f32) -> f32 {
    // View distance for Bevy's infinite reverse-Z perspective: z = near / depth.
    let z = p.a.z / max(d, 1e-7);
    let f = smoothstep(p.b.x, p.b.y, z);
    let fe = max(p.c.x, 1e-4);
    let in_x = smoothstep(p.car.x - fe, p.car.x, uv.x) * (1.0 - smoothstep(p.car.z, p.car.z + fe, uv.x));
    let in_y = smoothstep(p.car.y - fe, p.car.y, uv.y) * (1.0 - smoothstep(p.car.w, p.car.w + fe, uv.y));
    let near_car = 1.0 - smoothstep(p.b.z, p.b.z + max(p.c.y, 1e-3), z);
    return f * (1.0 - in_x * in_y * near_car);
}

// Screen offset (uv) from this pixel to where it was, scaled to the exposure time and clamped in pixels.
fn trail(uv: vec2<f32>, d: f32, dim: vec2<f32>) -> vec2<f32> {
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let prev = p.reproj * vec4<f32>(ndc, d, 1.0);
    if (prev.w <= 1e-6) {
        return vec2<f32>(0.0);
    }
    let pn = prev.xy / prev.w;
    let puv = vec2<f32>(pn.x * 0.5 + 0.5, 0.5 - pn.y * 0.5);
    var v = (puv - uv) * p.a.x * dim;
    let len = length(v);
    if (len > p.a.y) {
        v = v * (p.a.y / len);
    }
    return v / dim;
}

// Average of `n` taps from `uv` along `stp` (tap i at uv + stp * i), each weighted by its own blur factor so near
// pixels and the car don't smear into the background's trail. The centre tap always counts fully.
fn gather(uv: vec2<f32>, c0: vec4<f32>, stp: vec2<f32>, n: i32) -> vec4<f32> {
    var acc = c0.rgb;
    var w = 1.0;
    for (var i = 1; i < n; i++) {
        let suv = uv + stp * f32(i);
        let sw = blur_factor(suv, depth_at(suv));
        acc += textureSampleLevel(src, src_s, suv, 0.0).rgb * sw;
        w += sw;
    }
    return vec4<f32>(acc / w, c0.a);
}

// Pass 1: `a.w` uniform taps over the whole trail (the last one at the pixel's past position).
@fragment
fn mb_blur(in: VsOut) -> @location(0) vec4<f32> {
    let dim = vec2<f32>(textureDimensions(src));
    let c0 = textureSampleLevel(src, src_s, in.uv, 0.0);
    let d = depth_at(in.uv);
    let f = blur_factor(in.uv, d);
    if (f <= 0.001) {
        return c0;
    }
    let v = trail(in.uv, d, dim) * f;
    let vp = v * dim;
    if (dot(vp, vp) < 0.25) {
        return c0;
    }
    let n = max(i32(p.a.w), 2);
    return gather(in.uv, c0, v / f32(n - 1), n);
}

// Pass 2: `b.w` taps spread over ONE pass-1 tap spacing along the same trail (forward only, so the trail stays
// one-directional): pass 1's stepping turns into a smooth ramp.
@fragment
fn mb_smooth(in: VsOut) -> @location(0) vec4<f32> {
    let dim = vec2<f32>(textureDimensions(src));
    let c0 = textureSampleLevel(src, src_s, in.uv, 0.0);
    let d = depth_at(in.uv);
    let f = blur_factor(in.uv, d);
    if (f <= 0.001) {
        return c0;
    }
    let v = trail(in.uv, d, dim) * f;
    let vp = v * dim;
    if (dot(vp, vp) < 0.25) {
        return c0;
    }
    let n1 = max(p.a.w, 2.0);
    let n2 = max(i32(p.b.w), 2);
    return gather(in.uv, c0, v / ((n1 - 1.0) * f32(n2)), n2);
}
