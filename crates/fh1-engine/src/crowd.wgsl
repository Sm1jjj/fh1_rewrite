// FH1 spectators as sprite cards from Spectators.zip/sprites.xds (the game's far crowd LOD). Written for
// this engine; the game's own crowd sprite shader isn't translated (UNVERIFIED look). Each card stands
// upright, turns about Y to face the camera, and shows the atlas view nearest to the angle between the
// spectator's heading and the camera: standing cells (back, front, side), seated cells (back, front, then
// four views turning to the side; their angles are a GUESS: 22.5, 45, 67.5, 90 degrees). Side views are
// mirrored for the other side. Output is sqrt(colour) when the FH1 post chain runs (it squares it back).

#import bevy_pbr::mesh_functions::get_world_from_local
#import bevy_pbr::mesh_view_bindings::view

struct CrowdParams {
    // x = brightness, y = alpha cut, z = 1: write sqrt(colour), w = heading sign (+1 / -1).
    p: vec4<f32>,
    // x = cells per row, y = rows, z = fade start (m), w = fade end (m).
    atlas: vec4<f32>,
    // x = cards with data.w = 1 vanish within this distance (a 3D model is drawn there instead).
    lod: vec4<f32>,
    // rgb = the light on the cards (crowd.rs `card_light`).
    light: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> g: CrowdParams;

struct In {
    @builtin(instance_index) instance: u32,
    // Spectator's foot point (all four corners share it).
    @location(0) base: vec3<f32>,
    // Corner in the cell (0..1, v down).
    @location(2) uv: vec2<f32>,
    // Corner offset: x across the card (m), y up (m).
    @location(3) corner: vec2<f32>,
    // x = heading (radians, collision space), y = first atlas cell of the model, z = 1 seated,
    // w = 1 replaced by a 3D model near the camera.
    @location(5) data: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vertex(v: In) -> Out {
    let world = get_world_from_local(v.instance);
    let base = (world * vec4<f32>(v.base, 1.0)).xyz;
    var d = view.world_position - base;
    let dist = length(d);
    d.y = 0.0;
    var to_cam = vec3<f32>(0.0, 0.0, 1.0);
    if (dot(d, d) > 1e-6) {
        to_cam = normalize(d);
    }
    let right = vec3<f32>(to_cam.z, 0.0, -to_cam.x);
    // Heading in collision space (left-handed, +Z north) -> engine space (Z negated).
    let a = v.data.x * g.p.w;
    // The entity's rotation turns the heading too (walkers move a single card along their path).
    var facing = (world * vec4<f32>(sin(a), 0.0, -cos(a), 0.0)).xyz;
    facing.y = 0.0;
    facing = normalize(facing + vec3<f32>(0.0, 0.0, 1e-6));
    let cosang = clamp(dot(facing, to_cam), -1.0, 1.0);
    let ang = degrees(acos(cosang));
    // Which side of the spectator the camera is on.
    let side = facing.x * to_cam.z - facing.z * to_cam.x;
    var cell = 0.0;
    var mirror = side < 0.0;
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
    // Shrink cards over the last metres of the range instead of popping.
    var s = 1.0 - smoothstep(g.atlas.z, g.atlas.w, dist);
    // Horizontal distance, as the engine uses to spawn the models.
    if (v.data.w > 0.5 && length(d) < g.lod.x) {
        s = 0.0;
    }
    let p = base + (right * v.corner.x + vec3<f32>(0.0, v.corner.y, 0.0)) * s;
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
