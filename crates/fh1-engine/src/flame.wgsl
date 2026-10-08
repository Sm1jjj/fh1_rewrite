// Exhaust flames (backfire.rs "flame shader"). One additive mesh for every burning element:
//   kind 0 = flame tongue: a quad along the exhaust axis turned to face the camera; the flame body is a teardrop profile
//            distorted by scrolling fbm (licks toward the tip), coloured through a blackbody-style ramp from a blue-white
//            core at the pipe over yellow and orange to dark red tips. HDR (> 1) so bloom catches it.
//   kind 1 = outlet glow: a camera-facing radial glow at the pipe, strongest when looking down the pipe (where the
//            tongue is foreshortened to a sliver).
// Outputs display-linear colour, not exposed by the view (the old rigs' emission x 1.2 x 2^ev100 x view.exposure came
// to the same), so a flame reads the same at noon and at night.

#import bevy_pbr::mesh_view_bindings::{view, globals}
#import bevy_pbr::mesh_functions::get_world_from_local

struct FlameParams {
    // x = global brightness (FH1_FLAME_BRIGHT), yzw unused.
    k: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> p: FlameParams;

struct In {
    @builtin(instance_index) instance: u32,
    @location(0) pos: vec3<f32>,
    @location(1) corner: vec2<f32>,
    // (length or radius m, width m)
    @location(2) size: vec2<f32>,
    // (seed, intensity, life fraction, kind)
    @location(3) data: vec4<f32>,
    // xyz = axis (unit), w = facing weight scratch
    @location(4) axis: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) data: vec4<f32>,
    @location(2) facing: f32,
}

@vertex
fn vertex(v: In) -> Out {
    let base = (get_world_from_local(v.instance) * vec4<f32>(v.pos, 1.0)).xyz;
    let to_cam = normalize(view.world_position - base);
    let kind = v.data.w;
    var world: vec3<f32>;
    var uv: vec2<f32>;
    let a = normalize(v.axis.xyz + vec3<f32>(1e-5, 0.0, 0.0));
    let facing = abs(dot(a, to_cam));
    if (kind < 0.5) {
        // Axial billboard: length along the axis, width across it facing the camera. The tongue starts a little inside
        // the pipe (the pipe hides it) so its rounded base reads as coming out of the opening.
        var side = cross(a, to_cam);
        if (dot(side, side) < 1e-6) {
            side = cross(a, vec3<f32>(0.0, 1.0, 0.0));
        }
        side = normalize(side);
        let t = v.corner.y * 0.5 + 0.5;
        world = base + a * ((t - 0.12) * v.size.x) + side * (v.corner.x * v.size.y);
        uv = vec2<f32>(v.corner.x, t);
    } else {
        let right = view.world_from_view[0].xyz;
        let up = view.world_from_view[1].xyz;
        world = base + (right * v.corner.x + up * v.corner.y) * v.size.x + to_cam * 0.05;
        uv = v.corner;
    }
    var o: Out;
    o.clip = view.clip_from_world * vec4<f32>(world, 1.0);
    o.uv = uv;
    o.data = v.data;
    o.facing = facing;
    return o;
}

fn hash2(q: vec2<f32>) -> f32 {
    let h = dot(q, vec2<f32>(127.1, 311.7));
    return fract(sin(h) * 43758.5453);
}

fn vnoise(x: vec2<f32>) -> f32 {
    let i = floor(x);
    let f = fract(x);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash2(i);
    let b = hash2(i + vec2<f32>(1.0, 0.0));
    let c = hash2(i + vec2<f32>(0.0, 1.0));
    let d = hash2(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(x: vec2<f32>) -> f32 {
    var s = 0.0;
    var amp = 0.5;
    var q = x;
    for (var o = 0; o < 4; o++) {
        s += vnoise(q) * amp;
        q = q * 2.03 + vec2<f32>(17.3, 9.1);
        amp *= 0.5;
    }
    return s / 0.9375;
}

// Blackbody-style ramp, t = 0 (cool, dark red) .. 1 (hottest, blue-white). Display-linear, HDR at the hot end.
fn heat_colour(t: f32) -> vec3<f32> {
    let c0 = vec3<f32>(0.35, 0.02, 0.0);
    let c1 = vec3<f32>(1.6, 0.28, 0.03);
    let c2 = vec3<f32>(3.2, 1.25, 0.25);
    let c3 = vec3<f32>(4.5, 3.4, 1.6);
    let c4 = vec3<f32>(4.2, 4.6, 6.5);
    if (t < 0.3) {
        return mix(c0, c1, t / 0.3);
    } else if (t < 0.6) {
        return mix(c1, c2, (t - 0.3) / 0.3);
    } else if (t < 0.85) {
        return mix(c2, c3, (t - 0.6) / 0.25);
    }
    return mix(c3, c4, (t - 0.85) / 0.15);
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    let seed = i.data.x;
    let intensity = i.data.y;
    let life = i.data.z;
    let kind = i.data.w;
    let time = globals.time;
    var rgb = vec3<f32>(0.0);
    if (kind < 0.5) {
        let u = clamp(i.uv.y, 0.0, 1.0);
        // Turbulent licks: the flame races out of the pipe, so the noise scrolls fast along the axis.
        let n1 = fbm(vec2<f32>(i.uv.x * 1.7 + seed * 13.0, u * 3.2 - time * 11.0 + seed * 7.0));
        let n2 = fbm(vec2<f32>(i.uv.x * 3.9 - seed * 5.0, u * 6.5 - time * 17.0));
        // Sideways wobble growing toward the tip.
        let x = i.uv.x + (n1 - 0.5) * 0.9 * u + (n2 - 0.5) * 0.25 * u;
        // Teardrop: opens fast from the pipe, tapers to a ragged tip.
        let w = 1.15 * pow(smoothstep(0.0, 0.22, u + 0.06), 0.6) * pow(max(1.0 - u, 0.0), 0.55) + 0.02;
        let f = 1.0 - abs(x) / w;
        // The tip is eaten away by the noise; the body stays solid.
        let body = f - (1.0 - n2) * pow(u, 1.4) * 0.9 - (1.0 - n1) * 0.18;
        let d = smoothstep(0.0, 0.45, body);
        if (d <= 0.001) {
            discard;
        }
        // Temperature: hottest in the core near the pipe, cooling along the flame and toward its edge; dying flames cool.
        let core = clamp(f * 1.25, 0.0, 1.0);
        var t = core * (1.0 - 0.8 * pow(u, 0.8)) * (1.0 - 0.45 * life) + 0.12 * n2;
        t = clamp(t * (0.75 + 0.35 * intensity), 0.0, 1.0);
        // Blue base cone right at the opening.
        let blue = smoothstep(0.25, 0.0, u) * smoothstep(0.4, 0.9, core) * 0.8;
        rgb = heat_colour(t) * d + vec3<f32>(0.4, 0.8, 2.6) * blue * d;
    } else {
        let r2 = dot(i.uv, i.uv);
        if (r2 >= 1.0) {
            discard;
        }
        let g = exp(-r2 * 5.0) - exp(-5.0);
        let hot = exp(-r2 * 28.0);
        // Down the pipe the glow carries the flame; side-on it is a soft halo.
        let k = 0.25 + 0.75 * i.facing * i.facing;
        rgb = (vec3<f32>(2.2, 0.75, 0.18) * g + vec3<f32>(3.0, 2.6, 2.0) * hot * i.facing) * k;
    }
    let c = rgb * intensity * p.k.x;
    return vec4<f32>(c, 0.0);
}
