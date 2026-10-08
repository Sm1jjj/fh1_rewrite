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
// GLOW_MERGED (glow.rs merged draws, FH1_GLOW_MERGE): many batches in one mesh; the per-batch uniforms (beam, UVScale,
// animated) and the texture / strip choice come per vertex from `sel` (flat). Additive blend, so one draw is exact.
// Texture slots are picked in non-uniform flow: textureSampleGrad with the uv derivatives taken up front (same mip as
// textureSample); the strip at level 0 (its u is constant over a quad, so textureSample's derivative is 0 too).

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
#ifdef GLOW_MERGED
@group(#{MATERIAL_BIND_GROUP}) @binding(5) var tex1: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(6) var tex2: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(7) var tex3: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(8) var tex4: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(9) var tex5: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(10) var tex6: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(11) var tex7: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(12) var tex8: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(13) var tex9: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(14) var tex10: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(15) var tex11: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(16) var tex12: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(17) var tex13: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(18) var tex14: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(19) var tex15: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(20) var anim1: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(21) var anim2: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(22) var anim3: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(23) var anim4: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(24) var anim5: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(25) var anim6: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(26) var anim7: texture_2d<f32>;

// Sprite texture slot s (glow.rs MERGE_TEX = 16).
fn sample_slot(s: u32, uv: vec2<f32>, ddx: vec2<f32>, ddy: vec2<f32>) -> vec4<f32> {
    var r: vec4<f32>;
    switch s {
        case 1u: { r = textureSampleGrad(tex1, samp, uv, ddx, ddy); }
        case 2u: { r = textureSampleGrad(tex2, samp, uv, ddx, ddy); }
        case 3u: { r = textureSampleGrad(tex3, samp, uv, ddx, ddy); }
        case 4u: { r = textureSampleGrad(tex4, samp, uv, ddx, ddy); }
        case 5u: { r = textureSampleGrad(tex5, samp, uv, ddx, ddy); }
        case 6u: { r = textureSampleGrad(tex6, samp, uv, ddx, ddy); }
        case 7u: { r = textureSampleGrad(tex7, samp, uv, ddx, ddy); }
        case 8u: { r = textureSampleGrad(tex8, samp, uv, ddx, ddy); }
        case 9u: { r = textureSampleGrad(tex9, samp, uv, ddx, ddy); }
        case 10u: { r = textureSampleGrad(tex10, samp, uv, ddx, ddy); }
        case 11u: { r = textureSampleGrad(tex11, samp, uv, ddx, ddy); }
        case 12u: { r = textureSampleGrad(tex12, samp, uv, ddx, ddy); }
        case 13u: { r = textureSampleGrad(tex13, samp, uv, ddx, ddy); }
        case 14u: { r = textureSampleGrad(tex14, samp, uv, ddx, ddy); }
        case 15u: { r = textureSampleGrad(tex15, samp, uv, ddx, ddy); }
        default: { r = textureSampleGrad(tex, samp, uv, ddx, ddy); }
    }
    return r;
}

// Animation strip slot s (glow.rs MERGE_STRIPS = 8).
fn sample_strip(s: u32, uv: vec2<f32>) -> vec4<f32> {
    var r: vec4<f32>;
    switch s {
        case 1u: { r = textureSampleLevel(anim1, anim_samp, uv, 0.0); }
        case 2u: { r = textureSampleLevel(anim2, anim_samp, uv, 0.0); }
        case 3u: { r = textureSampleLevel(anim3, anim_samp, uv, 0.0); }
        case 4u: { r = textureSampleLevel(anim4, anim_samp, uv, 0.0); }
        case 5u: { r = textureSampleLevel(anim5, anim_samp, uv, 0.0); }
        case 6u: { r = textureSampleLevel(anim6, anim_samp, uv, 0.0); }
        case 7u: { r = textureSampleLevel(anim7, anim_samp, uv, 0.0); }
        default: { r = textureSampleLevel(anim, anim_samp, uv, 0.0); }
    }
    return r;
}
#endif

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
#ifdef GLOW_MERGED
    // x = texture slot, y = strip slot + 1 (0 = none), z = UVScale index (bit 0: u x2, bit 1: v x2), w = 1 for beams.
    @location(6) sel: vec4<u32>,
#endif
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) colour: vec3<f32>,
    @location(2) world: vec3<f32>,
    // Animated sprites: the strip coordinate.
    @location(3) anim_u: f32,
#ifdef GLOW_MERGED
    @location(4) @interpolate(flat) sel: vec4<u32>,
#endif
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
#ifdef GLOW_MERGED
    let beam = v.sel.w != 0u;
    let uv_scale = vec2<f32>(select(1.0, 2.0, (v.sel.z & 1u) != 0u), select(1.0, 2.0, (v.sel.z & 2u) != 0u));
    o.sel = v.sel;
#else
    let beam = g.p.y >= 0.5;
    let uv_scale = g.uv_scale.xy;
#endif
    if (!beam) {
        let to_cam = normalize(cam - v.pos);
        let c = dot(v.dir, to_cam);
        let angle = clamp((c - v.b.y) / (v.b.x - v.b.y), 0.0, 1.0);
        i = on * angle;
        let right = view.world_from_view[0].xyz;
        let up = view.world_from_view[1].xyz;
        p = v.pos + to_cam * v.c.y + (right * v.a.x + up * v.a.y) * v.c.x * select(0.0, 1.0, i > 0.0);
        o.uv = (v.a * 0.5 + 0.5) * uv_scale;
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
#ifdef GLOW_MERGED
    var t = sample_slot(i.sel.x, i.uv, dpdx(i.uv), dpdy(i.uv));
    if (i.sel.y != 0u) {
        let a = sample_strip(i.sel.y - 1u, vec2<f32>(i.anim_u, 0.5)).rgb;
        t = vec4<f32>(t.rgb * a, t.b * a.b);
    }
    let beam = i.sel.w != 0u;
#else
    var t = textureSample(tex, samp, i.uv);
    if (g.uv_scale.z > 0.5) {
        let a = textureSample(anim, anim_samp, vec2<f32>(i.anim_u, 0.5)).rgb;
        t = vec4<f32>(t.rgb * a, t.b * a.b);
    }
    let beam = g.p.y >= 0.5;
#endif
    var c: vec4<f32>;
    if (!beam) {
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
