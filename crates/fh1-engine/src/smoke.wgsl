// Remaster tyre smoke (smoke.rs). Camera-facing quads; the silhouette and a fake sphere normal come from two scrolling
// layers of a tiling billow-noise texture, lit by the scene sun (lux) + sky ambient and scaled by the view exposure, so
// it sits right in the Remaster HDR frame. Output is premultiplied alpha.
//
// Volume mode (2026-10-08, default; FH1_SMOKE_VOLUME=0 = the flat quads before): every puff is a sphere ("spherical
// billboard"). Per pixel the view ray's chord through the sphere is clipped analytically against the near distance and
// the puff's ground plane, then weighted along the chord by a soft car hull (rounded lower-body + cabin boxes per nearby
// car, from the car's PristineBoundingBox), and the puff's opacity is scaled by the part of the chord that is left. Smoke now thins smoothly into the car body and the road
// instead of cutting a hard line where a flat sprite pierces them, without needing a scene depth texture (the Remaster
// has no depth prepass). The quad is moved to the front of the sphere (and grown to cover its silhouette) so the depth
// test does not cut the part of the puff in front of a surface. Lighting uses the sphere: entry-point normal + noise
// bumps, and the sun's path length through the puff for self-shadowing.

#import bevy_pbr::mesh_view_bindings::{view, globals}
#import bevy_pbr::mesh_functions::get_world_from_local

struct SmokeParams {
    // xyz toward the sun, w = sun lux.
    sun: vec4<f32>,
    // rgb sun colour, w = ambient lux.
    sun_colour: vec4<f32>,
    // rgb albedo, w = ground fade height (m).
    albedo: vec4<f32>,
    // x = volume mode (1) / flat quads (0), y = number of boxes, z = near clip (m), w = detail noise strength.
    mode: vec4<f32>,
    // World -> unit-cube box space (box = [-1, 1]^3), up to 8.
    boxes: array<mat4x4<f32>, 8>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var noise: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> p: SmokeParams;

struct In {
    @builtin(instance_index) instance: u32,
    @location(0) pos: vec3<f32>,
    @location(1) corner: vec2<f32>,
    // (half size m, rotation)
    @location(2) size_rot: vec2<f32>,
    // (seed, alpha, life fraction, ground y)
    @location(3) data: vec4<f32>,
    @location(4) extra: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) corner: vec2<f32>,
    @location(1) data: vec4<f32>,
    @location(2) world: vec3<f32>,
    @location(3) right: vec3<f32>,
    @location(4) up: vec3<f32>,
    @location(5) to_cam: vec3<f32>,
    @location(6) rot: vec2<f32>,
    // xyz = sphere centre, w = radius.
    @location(7) sphere: vec4<f32>,
    // Bit k = the sphere overlaps box k (the only boxes the fragment tests).
    @location(8) @interpolate(flat) mask: u32,
    // xyz = stretch axis (unit, in the view plane), w = stretch factor (1 = sphere): the puff is an ellipsoid r x w along it.
    @location(9) stretch: vec4<f32>,
}

@vertex
fn vertex(v: In) -> Out {
    let centre = (get_world_from_local(v.instance) * vec4<f32>(v.pos, 1.0)).xyz;
    let right = view.world_from_view[0].xyz;
    let up = view.world_from_view[1].xyz;
    let s = sin(v.size_rot.y);
    let c = cos(v.size_rot.y);
    let r = v.size_rot.x;
    let to_c = view.world_position - centre;
    let dist = length(to_c);
    let to_cam = to_c / max(dist, 1e-4);
    var half = r;
    var off = 0.0;
    var mask = 0u;
    if (p.mode.x > 0.5) {
        // Sphere within the soft edge of box k (metres).
        let n = u32(p.mode.y);
        for (var k = 0u; k < n; k++) {
            let m = p.boxes[k];
            let lb = (m * vec4<f32>(centre, 1.0)).xyz;
            let dl = (lb - clamp(lb, vec3<f32>(-1.0), vec3<f32>(1.0))) * box_half(m);
            if (dot(dl, dl) < (r + HULL_OUT) * (r + HULL_OUT)) {
                mask |= 1u << k;
            }
        }
        // Front of the sphere, grown to its silhouette from there; back to the plain centred quad as the camera nears
        // the sphere (inside it the quad would have to cover the whole screen).
        let k = smoothstep(1.0, 1.3, dist / max(r, 1e-3));
        off = min(r, max(dist - 0.3, 0.0)) * k;
        let dq = dist - off;
        let sil = r * dq / sqrt(max(dist * dist - r * r, 1e-4));
        half = mix(r, min(sil, dq * 4.0), k);
    }
    // Motion smear (smoke.rs: motion relative to the camera x the stretch time): in the view plane, an ellipsoid up to 3x
    // long along it, the quad and the noise frame aligned with it. Otherwise the puff's own rotation.
    let sm = v.extra.xyz - dot(v.extra.xyz, to_cam) * to_cam;
    let sl = length(sm);
    var e1 = right * c + up * s;
    var sf = 1.0;
    if (sl > 0.02 * r) {
        e1 = sm / sl;
        sf = min(1.0 + 0.5 * sl / max(r, 1e-3), 3.0);
    }
    let e2 = normalize(cross(to_cam, e1));
    let world = centre + to_cam * off + e1 * (v.corner.x * half * sf) + e2 * (v.corner.y * half);
    var o: Out;
    o.clip = view.clip_from_world * vec4<f32>(world, 1.0);
    o.corner = v.corner;
    o.data = v.data;
    o.world = world;
    o.right = right;
    o.up = up;
    o.to_cam = to_cam;
    // Noise frame: e1 as the x axis (the fragment rebuilds it from right / up and this (cos, sin)).
    o.rot = vec2<f32>(dot(e1, right), dot(e1, up));
    o.sphere = vec4<f32>(centre, r);
    o.mask = mask;
    o.stretch = vec4<f32>(e1, sf);
    return o;
}

// Half extents (m) of box k: its world -> unit rows are axis / half extent.
fn box_half(m: mat4x4<f32>) -> vec3<f32> {
    return 1.0 / vec3<f32>(length(vec3<f32>(m[0].x, m[1].x, m[2].x)), length(vec3<f32>(m[0].y, m[1].y, m[2].y)), length(vec3<f32>(m[0].z, m[1].z, m[2].z)));
}

// Signed distance (m, < 0 inside) from `pw` to car box k with its corners and edges rounded by 85 % of its smallest half
// extent: a pill-like hull with no sharp edges or corners for the smoke to be cut along.
fn hull_sdf(k: u32, pw: vec3<f32>) -> f32 {
    let m = p.boxes[k];
    let h = box_half(m);
    let local = (m * vec4<f32>(pw, 1.0)).xyz * h;
    let rr = 0.85 * min(h.x, min(h.y, h.z));
    let q = abs(local) - (h - vec3<f32>(rr));
    return length(max(q, vec3<f32>(0.0))) + min(max(q.x, max(q.y, q.z)), 0.0) - rr;
}

// Soft hull edge: occupancy 1 deep inside (0.35 m) .. 0 at 0.25 m outside.
const HULL_IN: f32 = -0.35;
const HULL_OUT: f32 = 0.25;
// Car interior along the ray that hides everything behind it (m).
const HULL_BLOCK: f32 = 0.3;
const HULL_STEPS: i32 = 5;

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    let seed = i.data.x;
    let life = i.data.z;
    let volume = p.mode.x > 0.5;
    let cam = view.world_position;

    // Disk coordinates in the quad's own (rotated) frame: the quad corner for flat quads; for spheres, the ray's closest
    // approach to the centre over the radius (the same coordinates when the camera is far away).
    var lc = i.corner;
    var frac = 1.0;
    var entry = i.world;
    var mid = i.world;
    var near = 1.0;
    var ground = 1.0;
    let centre = i.sphere.xyz;
    let r = max(i.sphere.w, 1e-3);
    if (volume) {
        let d = normalize(i.world - cam);
        // Ellipsoid (stretched sphere): squash space along the stretch axis by 1/sf and solve the sphere there; the ray
        // parameter t stays in world metres.
        let sa = i.stretch.xyz;
        let ks = 1.0 / max(i.stretch.w, 1.0) - 1.0;
        let oc = (centre - cam) + ks * dot(centre - cam, sa) * sa;
        let dm = d + ks * dot(d, sa) * sa;
        let aa = max(dot(dm, dm), 1e-6);
        let tc = dot(oc, dm) / aa;
        let b2 = dot(oc, oc) - tc * tc * aa;
        if (b2 >= r * r) {
            discard;
        }
        let h = sqrt((r * r - b2) / aa);
        let t0 = tc - h;
        var t1 = tc + h;
        let full = 2.0 * h;
        let q = (dm * tc - oc) / r;
        let qx = dot(q, i.right);
        let qy = dot(q, i.up);
        lc = vec2<f32>(qx * i.rot.x + qy * i.rot.y, -qx * i.rot.y + qy * i.rot.x);
        var ta = max(t0, p.mode.z);
        // Road: the puff's own ground plane.
        if (d.y < -1e-4 && cam.y > i.data.w) {
            t1 = min(t1, (i.data.w - cam.y) / d.y);
        }
        // Cars: a soft hull, sampled along the chord (2026-10-08: analytic box clips showed straight cut edges beside the
        // wheels). At HULL_STEPS jittered points the smoke is weighted by 1 - occupancy of the rounded boxes (soft over
        // HULL_IN..HULL_OUT, the edge roughened by the smoke noise), and car interior met earlier along the ray hides what
        // is behind it (HULL_BLOCK m of it hides everything): smoke inside the body is gone, smoke behind it is hidden,
        // and the transition is a 0.6 m noisy gradient, never a line. Boxes holding the camera (cockpit) are skipped.
        var left = max(t1 - ta, 0.0);
        var hulls = i.mask;
        let n = u32(p.mode.y);
        for (var k = 0u; k < n; k++) {
            if (((hulls >> k) & 1u) != 0u && hull_sdf(k, cam) < HULL_OUT) {
                hulls &= ~(1u << k);
            }
        }
        if (hulls != 0u && left > 0.0) {
            let jit = fract(52.9829189 * fract(dot(i.clip.xy, vec2<f32>(0.06711056, 0.00583715))));
            let wob = (textureSampleLevel(noise, samp, lc * 0.4 + vec2<f32>(seed * 5.3, seed * 2.9), 0.0).r - 0.5) * 0.3;
            let stride = left / f32(HULL_STEPS);
            var block = 0.0;
            var acc = 0.0;
            for (var j = 0; j < HULL_STEPS; j++) {
                let pw = cam + d * (ta + (f32(j) + jit) * stride);
                var occ = 0.0;
                for (var k = 0u; k < n; k++) {
                    if (((hulls >> k) & 1u) != 0u) {
                        occ = max(occ, 1.0 - smoothstep(HULL_IN, HULL_OUT, hull_sdf(k, pw) + wob));
                    }
                }
                acc += (1.0 - occ) * clamp(1.0 - block, 0.0, 1.0);
                block += occ * stride / HULL_BLOCK;
            }
            left *= acc / f32(HULL_STEPS);
        }
        frac = left / max(full, 1e-4);
        if (frac <= 0.002) {
            discard;
        }
        entry = cam + d * ta;
        mid = cam + d * mix(ta, t1, 0.4);
        // Close to the camera: fade what is left within 0.6..2.6 m (as the flat quads did).
        near = clamp((mix(ta, t1, 0.3) - 0.6) / 2.0, 0.0, 1.0);
    } else {
        ground = clamp((i.world.y - i.data.w) / max(p.albedo.w, 1e-3), 0.0, 1.0);
        let cam_d = length(cam - i.world);
        near = clamp((cam_d - 0.6) / 2.0, 0.0, 1.0);
    }

    // Noise in the quad's frame, drifting slowly with time and life so the puff churns; a finer third layer up close.
    let t = globals.time;
    let base = lc * 0.5 + vec2<f32>(seed * 7.13, seed * 3.71);
    let n1 = textureSample(noise, samp, base * 0.55 + vec2<f32>(t * 0.012, life * 0.18));
    let n2 = textureSample(noise, samp, base * 1.3 + vec2<f32>(-t * 0.02, seed + life * 0.3));
    let n3 = textureSample(noise, samp, base * 3.1 + vec2<f32>(t * 0.035, -seed * 2.0 - life * 0.5));
    let r2 = dot(lc, lc);
    // Billowy silhouette, more ragged as the puff ages. Volume mode (2026-10-08, "a literal perfect circle"): three
    // overlapping lobes placed by the puff's seed, in coordinates warped by low-frequency noise, so no outline is round;
    // flat quads keep the single soft sphere.
    let erode = 0.55 + 0.35 * life;
    let detail = select(0.0, p.mode.w, volume);
    var blob = 1.0 - r2;
    if (volume) {
        let nw = textureSampleLevel(noise, samp, lc * 0.3 + vec2<f32>(seed * 11.3, seed * 5.9), 0.0);
        let lw = lc + (vec2<f32>(nw.r, nw.g) - 0.5) * 0.4;
        let a1 = seed * 6.2831853;
        let a2 = a1 + 2.4 + seed * 1.7;
        let c1 = vec2<f32>(cos(a1), sin(a1)) * 0.28;
        let c2 = vec2<f32>(cos(a2), sin(a2)) * 0.26;
        let l0 = 1.0 - dot(lw, lw) / 0.36;
        let l1 = 1.0 - dot(lw - c1, lw - c1) / 0.2;
        let l2 = 1.0 - dot(lw - c2, lw - c2) / 0.16;
        blob = max(l0, max(l1, l2));
    }
    let shape = blob + (n1.r - 0.5) * erode * 1.6 + (n2.g - 0.5) * 0.5 + (n3.r - 0.5) * detail;
    // Edge mask: nothing reaches the quad's border (no straight sprite edges); the lobes stay inside 0.85.
    let edge = 1.0 - smoothstep(select(0.62, 0.8, volume), 0.98, sqrt(r2));
    var density = smoothstep(0.0, 0.55, shape) * edge;
    if (density <= 0.002) {
        discard;
    }
    let l = p.sun.xyz;
    let v = i.to_cam;
    // Normal: the sphere's (entry point) or the flat quad's bulge, bent by the noise gradient.
    let gx = (n1.r - n1.b) * 1.2 + (n3.r - n3.g) * detail;
    let gy = (n2.g - n1.r) * 1.2 + (n3.g - n3.b) * detail;
    var n: vec3<f32>;
    var shadow: f32;
    if (volume) {
        let ns = normalize(entry - centre);
        n = normalize(ns + (i.right * gx + i.up * gy) * 0.6);
        // Self-shadow: the sun's path from inside the puff to its surface (0..2r), as Beer-Lambert.
        let pc = mid - centre;
        let bl = dot(pc, l);
        let path = -bl + sqrt(max(bl * bl - (dot(pc, pc) - r * r), 0.0));
        shadow = mix(0.35, 1.0, exp(-1.6 * density * i.data.y * 2.5 * path / r));
    } else {
        let z = sqrt(max(1.0 - r2, 0.0));
        let lq = vec2<f32>(i.corner.x + gx, i.corner.y + gy);
        n = normalize(i.right * lq.x + i.up * lq.y + i.to_cam * (z + 0.35));
        shadow = mix(1.0, 0.55, clamp(density * (1.0 - r2) * 1.3, 0.0, 1.0));
    }
    // Wrap diffuse + forward scattering when looking toward the sun + darker dense core (self-shadow).
    let wrap = clamp((dot(n, l) + 0.55) / 1.55, 0.0, 1.0);
    let forward = pow(clamp(dot(-v, l), 0.0, 1.0), 6.0) * 1.6;
    let sun_up = clamp(l.y * 4.0 + 0.2, 0.0, 1.0);
    let sun_light = p.sun_colour.rgb * p.sun.w * sun_up * (wrap * shadow + forward * (1.0 - density * 0.6) * mix(1.0, shadow, 0.5));
    // Sky ambient: brighter on top, darker underneath.
    let sky = vec3<f32>(0.72, 0.82, 1.0) * p.sun_colour.w * (0.65 + 0.35 * n.y);
    let radiance = p.albedo.rgb * (sun_light + sky) / 3.14159265;
    let a = clamp(density * i.data.y * ground * near * frac, 0.0, 1.0);
    return vec4<f32>(radiance * view.exposure * a, a);
}
