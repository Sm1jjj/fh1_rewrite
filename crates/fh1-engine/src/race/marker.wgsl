// Race visuals (race/visuals.rs): event start beams and rings, checkpoint gates, floor chevrons, the finish.
// One additive, unlit material for all of them; the shape comes from uv1.x (style) and is drawn as a soft SDF so no
// edge is a hard polygon. Outputs display-linear HDR colour (> 1 blooms), not exposed by the view (like flame.wgsl),
// so the markers read the same at noon and at night. Shared look with the world map's chevrons (e4): soft SDF,
// HDR x2, a travelling pulse at 1.2 Hz, near / far distance fades.
//
// styles: 0 beam (vertical cylinder: bright core, fades up, rising bands)   1 ground ring (annulus, rotating dashes)
//         7-10 the game's checkpoint / finish laser (visuals.rs module docs; merged only): 7 beam, 8 emitter plate,
//         9 beam haze, 10 base glow. 7 / 9 / 10 are camera-facing ribbons about the beam axis, swung in the vertex shader
//         (`laser_dir`); k.x = beam index (0..2) + 4 for the finish colours, k.w >= 0 = switching off (beams retract).
//         2 ripple (expanding ground ring)   3 gate curtain (edges + rising scan sweep, see-through middle)
//         4 floor chevron (pointing +v, pulse travels along the row)   5 finish curtain (chequer)   6 bar (glowing tube)
//         20-23 mission markers (missions/markers.rs: outpost / photo emblems, ground pad, beacon; see mission_marker)
//         30 speed camera glint (camera-facing sprite, merged only)   31 trap road line (missions/markers.rs)
//
// MARKER_MERGED (default; FH1_MARKER_MERGE=0 = off): all pieces in one mesh, the per-piece parameters come from
// vertex attributes instead of the uniform, and the pass ripple grows in the vertex shader from its birth time.

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
#ifdef MARKER_MERGED
    // Merged mesh (visuals.rs, FH1_MARKER_MERGE): every piece's parameters per vertex, the uniform is 1.
    // rgb colour, a intensity
    @location(4) colour: vec4<f32>,
    // near fade start / end, far fade start / end (m)
    @location(5) fade: vec4<f32>,
    // x pulse Hz, y template-local height, z emphasis, w pass-ripple birth (globals.time clock; < 0 = static piece)
    @location(6) k: vec4<f32>,
#endif
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) shape: vec2<f32>,
    @location(2) world: vec3<f32>,
    @location(3) normal: vec3<f32>,
    // local height (m) for beams
    @location(4) local_y: f32,
    // The piece's parameters (MarkerParams layout): from the uniform, or per vertex in the merged mesh.
    @location(5) @interpolate(flat) colour: vec4<f32>,
    @location(6) @interpolate(flat) fade: vec4<f32>,
    @location(7) @interpolate(flat) k: vec4<f32>,
}

// Pass ripple growth (visuals.rs FLASH_GROW / FLASH_LIFE): scale x exp(1.5 age) for 0.9 s, then gone.
const FLASH_GROW: f32 = 1.5;
const FLASH_LIFE: f32 = 0.9;
// A passed laser's beams retract into the plate over this long (visuals.rs LASER_OFF_S).
const LASER_OFF_S: f32 = 0.6;

// Axis of laser beam `b` (0..2) at time `t`: a fit to ANIM_GPLY_*_On (5 s loop, decoded with fh1-formats granny): the
// three beams stand 120 degrees apart and lean out from ~0.5 to ~5.5 degrees and back twice a loop while the fan turns
// about 0.4 times a second. INFERRED shape (the keys swing a little faster in the second half), VERIFIED range.
fn laser_dir(t: f32, b: f32) -> vec3<f32> {
    let ph = fract(t / 2.5);
    let tilt = radians(0.5 + 5.0 * sin(3.14159265 * pow(ph, 0.8)));
    let az = t * 2.513 + b * 2.0943951;
    return vec3<f32>(sin(tilt) * cos(az), cos(tilt), sin(tilt) * sin(az));
}

@vertex
fn vertex(v: In) -> Out {
    let m = get_world_from_local(v.instance);
    var o: Out;
#ifdef MARKER_MERGED
    var lp = v.pos;
    var ln = v.normal;
    var alive = 1.0;
    let vst = i32(round(v.shape.x));
    let laser = vst == 7 || vst == 9 || vst == 10;
    if (v.k.w >= 0.0 && !laser) {
        // Pass ripple: `normal` = this vertex's offset from the centre at the final scale; pull it in by age.
        var age = globals.time - v.k.w;
        if (age < 0.0) {
            // globals.time wraps (Time::elapsed_secs_wrapped, 1 h).
            age = age + 3600.0;
        }
        alive = select(0.0, 1.0, age < FLASH_LIFE);
        let s = exp(FLASH_GROW * (min(age, FLASH_LIFE) - FLASH_LIFE));
        // Dead: every vertex on the centre (degenerate, nothing rasterised).
        lp = v.pos - v.normal * (1.0 - s * alive);
        ln = vec3<f32>(0.0, 1.0, 0.0);
    }
    var world = (m * vec4<f32>(lp, 1.0)).xyz;
    o.normal = normalize((m * vec4<f32>(ln, 0.0)).xyz);
    if (i32(round(v.shape.x)) == 30) {
        // Glint sprite (missions/markers.rs `Bake::glint`): every corner sits on the centre; `normal` = corner offset
        // in the view plane (x right, y up, m) and z = pull towards the camera (m).
        let toc = normalize(view.world_position - world);
        world = world + view.world_from_view[0].xyz * v.normal.x + view.world_from_view[1].xyz * v.normal.y + toc * v.normal.z;
        o.normal = toc;
    }
    let mst = i32(round(v.shape.x));
    if (mst == 20 || mst == 21 || mst == 23) {
        // Mission marker billboards (missions/markers.rs `Bake::emblem` / `Bake::beacon`): upright, turned about Y to
        // face the camera. `pos` = the corner as if facing +Z (so the mesh Aabb is right), `normal.xy` = its offset
        // from the anchor (x across, y up, m), k.y = the emblem's half height. Emblems keep a minimum size on screen
        // (x1 up to 220 m, then growing to x2.5) and grow upwards from their bottom edge; the beacon only widens.
        let base = world - vec3<f32>(v.normal.x, v.normal.y, 0.0);
        var hz = view.world_position - base;
        hz.y = 0.0;
        let f = hz / max(length(hz), 1e-3);
        let right = vec3<f32>(f.z, 0.0, -f.x);
        let dc = length(view.world_position - base);
        var sx = clamp(dc / 220.0, 1.0, 2.5);
        var sy = sx;
        var lift = v.k.y * (sx - 1.0);
        if (mst == 23) {
            sx = clamp(dc / 350.0, 1.0, 4.0);
            sy = 1.0;
            lift = 0.0;
        }
        world = base + right * (v.normal.x * sx) + vec3<f32>(0.0, v.normal.y * sy + lift, 0.0);
        o.normal = f;
    }
    // Laser ribbons (visuals.rs `ribbon_b`): `normal` = the vertex's offset from the emitter (x across, y along, metres,
    // unturned). The ribbon is rebuilt about the beam's axis facing the camera, at least ~1-1.5 px wide (thinner beams
    // dim instead of shimmering; the gain goes to the fragment in local_y), and retracts into the plate when switched off.
    var lgain = 1.0;
    if (laser) {
        let pivot = (m * vec4<f32>(v.pos - v.normal, 1.0)).xyz;
        var along = v.normal.y;
        let half = length(v.normal.xz) * sign(v.uv.x - 0.5);
        if (v.k.w >= 0.0) {
            var age = globals.time - v.k.w;
            if (age < 0.0) {
                age = age + 3600.0;
            }
            alive = select(0.0, 1.0, age < LASER_OFF_S);
            along = along * (1.0 - smoothstep(0.0, LASER_OFF_S, age));
        }
        var axis = vec3<f32>(0.0, 1.0, 0.0);
        if (vst != 10) {
            axis = laser_dir(globals.time, v.k.x - 4.0 * floor(v.k.x / 4.0));
        }
        let c = pivot + axis * along;
        let toc = view.world_position - c;
        let sx = cross(axis, toc);
        let sl = length(sx);
        let side = select(vec3<f32>(1.0, 0.0, 0.0), sx / max(sl, 1e-6), sl > 1e-4);
        let px = 2.0 * length(toc) / (max(view.clip_from_view[1][1], 1e-3) * max(view.viewport.w, 1.0));
        let w = max(abs(half), select(1.5, 1.0, vst == 7) * px);
        lgain = pow(abs(half) / max(w, 1e-4), 0.6);
        world = c + side * (w * sign(half));
        o.normal = normalize(toc);
    }
    o.local_y = select(v.k.y, lgain, laser);
    o.colour = vec4<f32>(v.colour.rgb, v.colour.a * p.colour.a * alive);
    o.fade = v.fade;
    o.k = vec4<f32>(v.k.x, 0.0, v.k.z, 0.0);
#else
    let world = (m * vec4<f32>(v.pos, 1.0)).xyz;
    o.normal = normalize((m * vec4<f32>(v.normal, 0.0)).xyz);
    o.local_y = v.pos.y;
    o.colour = p.colour;
    o.fade = p.fade;
    o.k = p.k;
#endif
    o.clip = view.clip_from_world * vec4<f32>(world, 1.0);
    o.uv = v.uv;
    o.shape = v.shape;
    o.world = world;
    return o;
}

fn band(x: f32, centre: f32, width: f32) -> f32 {
    let d = (x - centre) / width;
    return exp(-d * d);
}

fn sd_box(p: vec2<f32>, half: vec2<f32>) -> f32 {
    let q = abs(p) - half;
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0);
}

// 1 inside the SDF shape, 0 outside, a soft edge `aa` wide.
fn fill(d: f32, aa: f32) -> f32 {
    return 1.0 - smoothstep(-aa, aa, d);
}

// Mission markers (missions/markers.rs, styles 20-29). Premultiplied: rgb = light added, a = how much of the scene
// behind is darkened (the dark backing of the emblems, as the FH1 map icons: a white / coloured glyph on black).
// The material blends One / OneMinusSrcAlpha, so every other style (a = 0) stays purely additive.
//   20 Horizon Outpost emblem: the map's gas_station icon (MapIconSheet cell 1,2): white "H" in a white-rimmed dark
//      diamond.   21 photo-shoot emblem: camera viewfinder (corner brackets + lens) on a dark rounded plate.
//   22 ground pad (flat annulus, w = inner -> outer): one soft line near the edge over a faint fill.
//   23 beacon (upright camera-facing ribbon, w = up): soft gaussian core, fading upwards.
fn mission_marker(i: Out, style: i32, dist: f32, fade: f32, pulse: f32, emph: f32) -> vec4<f32> {
    let k = i.colour.a;
    let col = i.colour.rgb * k;
    let u = i.uv.x;
    let w = i.uv.y;
    if (style == 20 || style == 21) {
        // Edge softness = about 1.5 px at this distance (no fwidth: the style branch is not uniform control flow).
        // local_y = the emblem's half height (m), times the vertex shader's screen-size scale.
        let half_m = max(i.local_y, 0.1) * clamp(dist / 220.0, 1.0, 2.5);
        let px = 2.0 * dist / (max(view.clip_from_view[1][1], 1e-3) * max(view.viewport.w, 1.0)) / half_m;
        let aa = clamp(px * 1.5, 0.01, 0.2);
        // p: y up, units of the half height (x wider on the 4:3 photo plate).
        let aspect = select(1.0, 4.0 / 3.0, style == 21);
        let p = vec2<f32>((u * 2.0 - 1.0) * aspect, w * 2.0 - 1.0);
        var back_d = 0.0;
        var glyph = 0.0;
        if (style == 20) {
            let dd = abs(p.x) + abs(p.y);
            back_d = (dd - 0.82) * 0.7071;
            let rim = fill((abs(dd - 0.775) - 0.04) * 0.7071, aa);
            let bars = sd_box(vec2<f32>(abs(p.x) - 0.235, p.y), vec2<f32>(0.075, 0.33));
            let cross = sd_box(p, vec2<f32>(0.24, 0.06));
            glyph = max(rim, fill(min(bars, cross), aa));
        } else {
            back_d = sd_box(p, vec2<f32>(1.08, 0.78)) - 0.16;
            let frame = abs(sd_box(p, vec2<f32>(0.92, 0.62))) - 0.055;
            let corners = smoothstep(0.50, 0.58, abs(p.x)) * smoothstep(0.24, 0.32, abs(p.y));
            let lens = abs(length(p) - 0.30) - 0.055;
            glyph = max(fill(frame, aa) * corners, max(fill(lens, aa), fill(length(p) - 0.08, aa)));
        }
        let back = fill(back_d, aa);
        // A faint glow just outside the plate, gone well before the quad's edge.
        let glow = exp(-max(back_d, 0.0) * 14.0) * (1.0 - back) * 0.22;
        let light = (glyph * (0.9 + 0.1 * pulse) * emph + glow) * fade;
        return vec4<f32>(col * light, back * 0.62 * clamp(k, 0.0, 1.0) * fade);
    }
    var a = 0.0;
    if (style == 22) {
        a = band(w, 0.8, 0.07) + 0.2 * w * w * (1.0 - smoothstep(0.8, 0.9, w));
        a = a * (0.85 + 0.15 * pulse) * emph;
    } else if (style == 23) {
        let across = 1.0 - abs(2.0 * u - 1.0);
        let prof = pow(across, 3.0) * 0.55 + pow(across, 14.0) * 0.6;
        let up = exp(-w * 2.4) * (1.0 - smoothstep(0.8, 1.0, w)) * smoothstep(0.0, 0.03, w);
        let shimmer = 0.9 + 0.1 * sin(w * 40.0 - globals.time * 1.5);
        a = prof * up * shimmer * (1.0 + clamp(dist / 500.0, 0.0, 2.0));
    }
    return vec4<f32>(col * a * fade, 0.0);
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    let t = globals.time + i.k.y;
    let style = i32(round(i.shape.x));
    let to_cam = view.world_position - i.world;
    let dist = length(to_cam);
    let vdir = to_cam / max(dist, 1e-3);
    // Distance fades: never blinding up close (driving through a gate), gone before the fog would hide it.
    let fade = smoothstep(i.fade.x, i.fade.y, dist) * (1.0 - smoothstep(i.fade.z, i.fade.w, dist));
    let pulse = 0.5 + 0.5 * sin(6.2831853 * i.k.x * t);
    let emph = 1.0 + i.k.z * (0.35 + 0.65 * pulse);
    if (style >= 20 && style < 30) {
        return mission_marker(i, style, dist, fade, pulse, emph);
    }
    var a = 0.0;
    var col = i.colour.rgb;
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
        let ph = fract(t * i.k.x * 0.5);
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
            col = select(i.colour.rgb, vec3<f32>(1.6, 1.6, 1.6), c == 0);
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
        let run = fract(t * i.k.x - i.shape.y * 0.18);
        let travel = 0.35 + 0.65 * exp(-pow((run - 0.5) * 4.0, 2.0));
        a = (body + glow) * mask * travel;
    } else if (style == 7) {
        // Laser beam (GR_Laser_Blue_DIFF / GR_Laser_Final_DIFF, once over the beam's length): a flat band of pure colour
        // with a soft edge, slow long brightness streaks (the texture's alpha, 0.4-1 over tens of metres), the top cap
        // fading out (_End). u across, w along (0 = plate). local_y = the vertex shader's thin-line gain.
        let x = abs(2.0 * u - 1.0);
        let prof = 1.0 - smoothstep(0.35, 1.0, x);
        let streak = 0.7 + 0.3 * sin(w * 41.0 + 1.3) * sin(w * 13.0 + 0.4);
        let ends = smoothstep(0.0, 0.0008, w) * (1.0 - smoothstep(0.7, 1.0, w));
        a = prof * streak * ends * i.local_y;
    } else if (style == 9 || style == 10) {
        // Beam haze (9) and the blue laser's base glow (10): OBJ_LightLaser_DIFF, a soft glow narrow and pale at the plate
        // (lavender / mint), widening (the ribbon's shape) and going over to the beam's colour, gone by ~90% of its length.
        let fin = i.k.x >= 3.5;
        let far_col = select(vec3<f32>(0.054, 0.053, 0.332), vec3<f32>(0.125, 0.737, 0.063), fin);
        let x = 2.0 * u - 1.0;
        let prof = exp(-x * x * 4.0) * (1.0 - smoothstep(0.75, 1.0, abs(x)));
        let fall = pow(max(1.0 - w / 0.9, 0.0), select(2.0, 2.4, style == 10));
        col = mix(i.colour.rgb, far_col, smoothstep(0.0, 0.45, w));
        a = 0.94 * prof * fall * smoothstep(0.0, 0.004, w) * i.local_y;
    } else if (style == 8) {
        // Laser emitter plate (checkpoint_laser, lit by checkpoint_laser_EMIS: a white disc in the middle of a grey
        // housing): only the light shows in an additive pass, the white lens and a faint rim of the beam colour on the
        // housing. w = radius 0..1 (PLATE_R).
        let r = w;
        let lens = 1.0 - smoothstep(0.24, 0.32, r);
        let rim = band(r, 0.42, 0.06) * 0.25 + exp(-r * 4.0) * 0.2;
        a = lens * 1.2 + rim;
        col = mix(col, vec3<f32>(1.0, 1.0, 1.0), lens);
    } else if (style == 30) {
        // Speed camera / zone glint (missions/markers.rs): a small hot core in a soft round halo, zero at the quad's
        // edge; a little brighter far away so the camera box stays findable when it is a few pixels.
        let r = length(i.uv * 2.0 - 1.0);
        let core = exp(-r * r * 45.0);
        let halo = exp(-r * r * 7.0) * 0.4;
        a = (core * 1.3 + halo) * (1.0 - smoothstep(0.75, 1.0, r));
        col = mix(col, vec3<f32>(1.0, 1.0, 1.0) * max(col.r, max(col.g, col.b)), 0.45 * core);
        a = a * (1.0 + clamp(dist / 250.0, 0.0, 1.5));
    } else if (style == 31) {
        // Road line across a speed camera / zone gate (FH1_TRAP_LINE=1): soft across (u along, w across), soft ends,
        // thinner up close so it never flares as the car goes over it.
        let ends = smoothstep(0.0, 0.08, u) * smoothstep(1.0, 0.92, u);
        a = band(w, 0.5, 0.2) * ends * clamp(dist / 20.0, 0.35, 1.0);
    } else {
        // Bar: a tube glowing along its length.
        let facing = abs(dot(i.normal, vdir));
        a = 0.35 + 0.65 * pow(facing, 1.5);
    }
    let out = col * i.colour.a * a * fade * emph;
    return vec4<f32>(out, 0.0);
}
