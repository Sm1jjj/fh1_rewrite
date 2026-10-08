// FH1 light glows (CProceduralLightGlows), ported by hand from the game's xex shaders; docs/SHADERS.md
// "Light glows". VERIFIED from the microcode unless marked:
// - sprite (VS 0x82174858 / PS 0x82153A58): camera-facing quad of half-size min(rec28, 20) along the camera
//   X/Y axes, pulled rec20 towards the camera; intensity = on (L >= 2 * threshold) * angle fade
//   sat((dot(dir, toCam) - cos a2) / (cos a1 - cos a2)); the quad collapses at 0. PS rgb = tex.rgb * colour,
//   alpha = tex.b * colour.b. The distance fade only runs in the game's pass 9 (not the main view).
// - beam (VS 0x82173CA0 / PS 0x82153520): ribbon along dir, side = normalize(cross(viewDir, dir)), off-axis
//   fade sat((|dot(viewDir, dir)| - 0.98) / (0.95 - 0.98)); PS tex.rgb * colour * sat((camDist - 2) / 4).
//   Beam UV corners and output alpha (1) are INFERRED.
// - animated sprite (VS 0x82175680 / PS 0x82153BA0): as the sprite but without the colour input; the
//   colour is tex1D(Animation, frac(T * rec34 + rec30)) with T = TimeGain + BaseAnimationTime (c249.x + c156.x),
//   PS rgb = tex.rgb * strip.rgb * I, alpha = tex.b * strip.b * I. The strip is a 1D texture stored 2D with
//   identical rows: sampled at v = 0.5 (INFERRED).
// Blending is SRCALPHA / ONE in the game's gamma-2 target; the output is written as is (squared when the
// FH1 post chain is off).

#import bevy_pbr::mesh_view_bindings::view

struct GlowParams {
    // x = SwitchOnLights L (-1 when off), y = 1 for beams, z = 1: raw output (FH1 post chain), w = output gain (non-raw).
    p: vec4<f32>,
    // xy = UVScale (group flags), z = 1: animated sprite, w = T (TimeGain.x).
    uv_scale: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> g: GlowParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var anim: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var anim_samp: sampler;

struct In {
    @location(0) pos: vec3<f32>,
    @location(1) dir: vec3<f32>,
    // Sprite: corner (+-1, +-1). Beam: (side offset m, length m).
    @location(2) a: vec2<f32>,
    // Sprite: (cos a1, cos a2). Beam: uv.
    @location(3) b: vec2<f32>,
    // Sprite: (half-size, depth pull, animation phase rec30, animation rate rec34).
    @location(4) c: vec4<f32>,
    // rgb, a = switch-on threshold.
    @location(5) colour: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) colour: vec3<f32>,
    @location(2) world: vec3<f32>,
    // Animated sprites: the strip coordinate.
    @location(3) anim_u: f32,
}

// T = c249.x TimeGain (seconds, wrapped at 8 h; lighting.rs) + c156.x BaseAnimationTime (0, VERIFIED).
fn glow_time() -> f32 {
    return g.uv_scale.w;
}

@vertex
fn vertex(v: In) -> Out {
    let cam = view.world_position;
    let on = select(0.0, 1.0, g.p.x >= 2.0 * v.colour.a);
    var o: Out;
    var p: vec3<f32>;
    var i: f32;
    if (g.p.y < 0.5) {
        let to_cam = normalize(cam - v.pos);
        let c = dot(v.dir, to_cam);
        let angle = clamp((c - v.b.y) / (v.b.x - v.b.y), 0.0, 1.0);
        i = on * angle;
        let right = view.world_from_view[0].xyz;
        let up = view.world_from_view[1].xyz;
        p = v.pos + to_cam * v.c.y + (right * v.a.x + up * v.a.y) * v.c.x * select(0.0, 1.0, i > 0.0);
        o.uv = (v.a * 0.5 + 0.5) * g.uv_scale.xy;
        o.anim_u = fract(glow_time() * v.c.w + v.c.z);
    } else {
        let view_dir = normalize(v.pos - cam);
        var side = cross(view_dir, v.dir);
        if (dot(side, side) < 1e-8) {
            side = vec3<f32>(1.0, 0.0, 0.0);
        }
        side = normalize(side);
        let look = clamp((abs(dot(view_dir, v.dir)) - 0.98) / (0.95 - 0.98), 0.0, 1.0);
        i = on * look;
        p = v.pos + (v.dir * v.a.y + side * v.a.x) * select(0.0, 1.0, i > 0.0);
        o.uv = v.b;
    }
    o.clip = view.clip_from_world * vec4<f32>(p, 1.0);
    o.colour = v.colour.rgb * i;
    o.world = p;
    return o;
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    var t = textureSample(tex, samp, i.uv);
    if (g.uv_scale.z > 0.5) {
        let a = textureSample(anim, anim_samp, vec2<f32>(i.anim_u, 0.5)).rgb;
        t = vec4<f32>(t.rgb * a, t.b * a.b);
    }
    var c: vec4<f32>;
    if (g.p.y < 0.5) {
        c = vec4<f32>(t.rgb * i.colour, t.b * i.colour.b);
    } else {
        let d = distance(i.world, view.world_position);
        c = vec4<f32>(t.rgb * i.colour * clamp((d - 2.0) / 4.0, 0.0, 1.0), 1.0);
    }
    if (g.p.z < 0.5) {
        c = vec4<f32>(c.rgb * c.rgb * g.p.w, c.a);
    }
    return c;
}
