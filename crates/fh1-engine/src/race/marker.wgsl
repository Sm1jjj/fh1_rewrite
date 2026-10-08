// Race visuals (race/visuals.rs): event start beams and rings, checkpoint gates, floor chevrons, the finish.
// One additive, unlit material for all of them; the shape comes from uv1.x (style) and is drawn as a soft SDF so no
// edge is a hard polygon. Outputs display-linear HDR colour (> 1 blooms), not exposed by the view (like flame.wgsl),
// so the markers read the same at noon and at night. Shared look with the world map's chevrons (e4): soft SDF,
// HDR x2, a travelling pulse at 1.2 Hz, near / far distance fades.
//
// styles: 0 beam (vertical cylinder: bright core, fades up, rising bands)   1 ground ring (annulus, rotating dashes)
//         2 ripple (expanding ground ring)   3 gate curtain (edges + rising scan sweep, see-through middle)
//         4 floor chevron (pointing +v, pulse travels along the row)   5 finish curtain (chequer)   6 bar (glowing tube)

#import bevy_pbr::mesh_view_bindings::{view, globals}
#import bevy_pbr::mesh_functions::get_world_from_local

struct MarkerParams {
    // rgb = display-linear colour, a = intensity
    colour: vec4<f32>,
    // near fade start / end, far fade start / end (m)
    fade: vec4<f32>,
    // x = pulse rate (Hz), y = phase offset, z = emphasis (0..1: pulsing "recommended"), w = unused
    k: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> p: MarkerParams;

struct In {
    @builtin(instance_index) instance: u32,
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    // x = style, y = per-vertex phase (chevron index),
    @location(3) shape: vec2<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) shape: vec2<f32>,
    @location(2) world: vec3<f32>,
    @location(3) normal: vec3<f32>,
    // local height (m) for beams
    @location(4) local_y: f32,
}

@vertex
fn vertex(v: In) -> Out {
    let m = get_world_from_local(v.instance);
    let world = (m * vec4<f32>(v.pos, 1.0)).xyz;
    var o: Out;
    o.clip = view.clip_from_world * vec4<f32>(world, 1.0);
    o.uv = v.uv;
    o.shape = v.shape;
    o.world = world;
    o.normal = normalize((m * vec4<f32>(v.normal, 0.0)).xyz);
    o.local_y = v.pos.y;
    return o;
}

fn band(x: f32, centre: f32, width: f32) -> f32 {
    let d = (x - centre) / width;
    return exp(-d * d);
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    let t = globals.time + p.k.y;
    let style = i32(round(i.shape.x));
    let to_cam = view.world_position - i.world;
    let dist = length(to_cam);
    let vdir = to_cam / max(dist, 1e-3);
    // Distance fades: never blinding up close (driving through a gate), gone before the fog would hide it.
    let fade = smoothstep(p.fade.x, p.fade.y, dist) * (1.0 - smoothstep(p.fade.z, p.fade.w, dist));
    let pulse = 0.5 + 0.5 * sin(6.2831853 * p.k.x * t);
    let emph = 1.0 + p.k.z * (0.35 + 0.65 * pulse);
    var a = 0.0;
    var col = p.colour.rgb;
    let u = i.uv.x;
    let w = i.uv.y;
    if (style == 0) {
        // Beam: bright where the cylinder faces the camera, soft at the silhouette; fades out upwards; bands rise.
        let facing = abs(dot(i.normal, vdir));
        let core = pow(facing, 2.2);
        let up = pow(1.0 - clamp(w, 0.0, 1.0), 1.6);
        let foot = smoothstep(0.0, 0.015, w);
        let bands = 0.75 + 0.25 * smoothstep(0.6, 1.0, fract(i.local_y / 7.0 - t * 0.5));
        a = core * up * foot * bands;
        // Far away the beam is thin on screen: keep it visible (screen-size floor).
        a = a * (1.0 + clamp(dist / 400.0, 0.0, 3.0));
    } else if (style == 1) {
        // Ground ring: soft annulus profile, rotating dashes, bright inner edge.
        let prof = band(w, 0.5, 0.28);
        let dash = 0.55 + 0.45 * smoothstep(0.35, 0.5, abs(fract(u * 18.0 - t * 0.12) - 0.5));
        a = prof * dash + 0.6 * band(w, 0.15, 0.08);
    } else if (style == 2) {
        // Ripple: rings travelling outwards at the pulse rate.
        let ph = fract(t * p.k.x * 0.5);
        a = band(w, ph, 0.06) * (1.0 - ph) * 0.9 + band(w, fract(ph + 0.5), 0.06) * (1.0 - fract(ph + 0.5)) * 0.9;
        a = a * smoothstep(0.0, 0.08, w);
    } else if (style == 3 || style == 5) {
        // Gate curtain: glowing frame (sides + top), a sweep rising through it, see-through middle, soft foot.
        let side = min(u, 1.0 - u);
        let edge = exp(-side * 40.0) + 0.7 * exp(-(1.0 - w) * 28.0);
        let sweep = band(fract(w - t * 0.6), 0.5, 0.05) * 0.35;
        let veil = 0.06 + 0.05 * pulse;
        a = (edge + sweep + veil) * smoothstep(0.0, 0.06, w);
        if (style == 5) {
            // Finish: chequer in the curtain (white / colour), brighter at the foot.
            let c = (i32(floor(u * 16.0)) + i32(floor(w * 4.0))) % 2;
            let chk = select(0.25, 0.7, c == 0);
            a = a + chk * (1.0 - w) * 0.6 * smoothstep(0.0, 0.06, w);
            col = select(p.colour.rgb, vec3<f32>(1.6, 1.6, 1.6), c == 0);
        }
        // Up close the curtain thins out so the road stays readable as the car goes through.
        a = a * clamp(dist / 25.0, 0.25, 1.0);
    } else if (style == 4) {
        // Floor chevron pointing +v: SDF of a ">" turned forward; the pulse travels along the row (shape.y = index).
        let x = abs(u - 0.5) * 2.0;
        let d = abs(w - (0.8 - x * 0.45));
        let body = 1.0 - smoothstep(0.06, 0.16, d);
        let glow = exp(-d * 9.0) * 0.35;
        let mask = smoothstep(1.0, 0.85, x);
        let run = fract(t * p.k.x - i.shape.y * 0.18);
        let travel = 0.35 + 0.65 * exp(-pow((run - 0.5) * 4.0, 2.0));
        a = (body + glow) * mask * travel;
    } else {
        // Bar: a tube glowing along its length.
        let facing = abs(dot(i.normal, vdir));
        a = 0.35 + 0.65 * pow(facing, 1.5);
    }
    let out = col * p.colour.a * a * fade * emph;
    return vec4<f32>(out, 0.0);
}
