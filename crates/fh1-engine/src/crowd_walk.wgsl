// GPU walkers (crowd/walk_gpu.rs, perf 2026-10-08): the festival walkers' sprite cards moved along their paths in the
// vertex shader. Each path is baked to evenly spaced points (a storage buffer); a walker's quad carries its offset along
// the path, the path's first point and point count, and the path length; the shader places it at
// offset + speed x time (wrapping at the length, as the CPU walkers did) and faces it along the path. The card itself
// (atlas view by the angle to the camera, distance fade, cut where the 3D figures take over, lighting) is crowd.wgsl's.

#import bevy_pbr::mesh_view_bindings::view

struct CrowdParams {
    p: vec4<f32>,
    atlas: vec4<f32>,
    lod: vec4<f32>,
    light: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> g: CrowdParams;
// x = time (s, the engine's virtual clock), y = walking speed (m/s).
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> walk: vec4<f32>;
// Path points (engine space, w unused): per path `count + 1` of them, `length / count` metres apart.
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var<storage, read> pts: array<vec4<f32>>;

struct In {
    // x = offset along the path (m), y = index of the path's first point, z = point spans (count).
    @location(0) path: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) corner: vec2<f32>,
    // x = path length (m), y = first atlas cell of the model, z = 1 seated, w = 1 replaced by a 3D figure when near.
    @location(5) data: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vertex(v: In) -> Out {
    let len = max(v.data.x, 1e-3);
    var s = (v.path.x + walk.y * walk.x) % len;
    if (s < 0.0) {
        s = s + len;
    }
    let n = max(v.path.z, 1.0);
    let x = s / len * n;
    let k = min(floor(x), n - 1.0);
    let f = x - k;
    let i = u32(v.path.y + k);
    let p0 = pts[i].xyz;
    let p1 = pts[i + 1u].xyz;
    let base = mix(p0, p1, f);
    var facing = vec3<f32>(p1.x - p0.x, 0.0, p1.z - p0.z);
    if (dot(facing, facing) > 1e-10) {
        facing = normalize(facing);
    } else {
        facing = vec3<f32>(0.0, 0.0, -1.0);
    }

    var d = view.world_position - base;
    let dist = length(d);
    d.y = 0.0;
    var to_cam = vec3<f32>(0.0, 0.0, 1.0);
    if (dot(d, d) > 1e-6) {
        to_cam = normalize(d);
    }
    let right = vec3<f32>(to_cam.z, 0.0, -to_cam.x);
    let cosang = clamp(dot(facing, to_cam), -1.0, 1.0);
    let ang = degrees(acos(cosang));
    let side = facing.x * to_cam.z - facing.z * to_cam.x;
    var cell = 0.0;
    let mirror = side < 0.0;
    if (v.data.z > 0.5) {
        if (ang < 11.25) {
            cell = 4.0;
        } else if (ang < 101.25) {
            cell = 5.0 + clamp(round(ang / 22.5) - 1.0, 0.0, 3.0);
        } else if (ang < 135.0) {
            cell = 8.0;
        } else {
            cell = 3.0;
        }
    } else {
        if (ang < 45.0) {
            cell = 1.0;
        } else if (ang < 135.0) {
            cell = 2.0;
        } else {
            cell = 0.0;
        }
    }
    let idx = v.data.y + cell;
    let col = idx % g.atlas.x;
    let row = floor(idx / g.atlas.x);
    var u = v.uv.x;
    if (mirror && (cell == 2.0 || cell >= 5.0)) {
        u = 1.0 - u;
    }
    var sc = 1.0 - smoothstep(g.atlas.z, g.atlas.w, dist);
    if (v.data.w > 0.5 && length(d) < g.lod.x) {
        sc = 0.0;
    }
    let p = base + (right * v.corner.x + vec3<f32>(0.0, v.corner.y, 0.0)) * sc;
    var o: Out;
    o.clip = view.clip_from_world * vec4<f32>(p, 1.0);
    o.uv = vec2<f32>((col + u) / g.atlas.x, (row + v.uv.y) / g.atlas.y);
    return o;
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    let t = textureSample(tex, samp, i.uv);
    if (t.a < g.p.y) {
        discard;
    }
    var c = t.rgb * g.p.x * g.light.rgb;
    if (g.p.z > 0.5) {
        c = sqrt(max(c, vec3<f32>(0.0)));
    }
    return vec4<f32>(c, 1.0);
}
