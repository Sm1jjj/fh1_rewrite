// FH1 grass cards, ported by hand from the game's own PROC_VEGETATION_VS (xex container 0x8216F100) and
// PROC_VEGETATION_PS (0x82151C58); docs/PROPS.md "Grass". What is the game's (VERIFIED from the
// disassembly):
// - corners from the vertex index; the card stands along the instance's ground normal N, its right
//   axis = normalize(cross(N, CamLook)); size and alpha shrink to 0 over FadeValues;
// - colour = tex.rgb * c0.z * tint * lerp(ambColor, ambColor + sat(N.sunDir) * sunColor, shadow), fog
//   `colour * fog.a + fog.rgb` from the vertex shader's fog model, alpha = tex.a * fade;
// - output sqrt(colour) (the scene target is gamma-2; fh1-render's post chain squares it).
// - tint = the grey 0.5 + 0.5 w from the instance normal's w (fh1_formats Blade::shade; 1 everywhere in
//   Colorado).
// Not reproduced: the screen-space shadow mask (shadow = 1), c0.z (a material constant; 1 here) and the
// alpha test threshold (0.5 here).

#import bevy_pbr::mesh_functions::get_world_from_local
#import bevy_pbr::mesh_view_bindings::view

struct GrassParams {
    // x = fade start, y = fade end (m), z = c0.z, w = 1: write sqrt(colour) (FH1 post chain on).
    fade: vec4<f32>,
    sun_dir: vec4<f32>,
    sun_color: vec4<f32>,
    amb_color: vec4<f32>,
    fog_consts: vec4<f32>,
    fog_consts2: vec4<f32>,
    fog_color: vec4<f32>,
    fog_color2: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> g: GrassParams;

struct In {
    @builtin(instance_index) instance: u32,
    // Ground point of the blade (all four corners share it).
    @location(0) base: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    // Corner offset: x across the card (m), y along the normal (m).
    @location(3) corner: vec2<f32>,
    @location(5) colour: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    // rgb = tint, a = fade.
    @location(1) colour: vec4<f32>,
    @location(2) lit: vec3<f32>,
    // rgb = fog colour * amount, a = 1 - amount.
    @location(3) fog: vec4<f32>,
}

@vertex
fn vertex(v: In) -> Out {
    let world = get_world_from_local(v.instance);
    let base = (world * vec4<f32>(v.base, 1.0)).xyz;
    let n = normalize(v.normal);
    let cam = view.world_position;
    let look = -view.world_from_view[2].xyz;
    var right = cross(n, look);
    if (dot(right, right) < 1e-8) {
        right = vec3<f32>(1.0, 0.0, 0.0);
    }
    right = normalize(right);
    let d = distance(base, cam);
    let fade = clamp((g.fade.y - d) / max(g.fade.y - g.fade.x, 1e-3), 0.0, 1.0);
    let p = base + (right * v.corner.x + n * v.corner.y) * fade;

    // Fog (the game's VS model, docs/SHADERS.md).
    let fd = distance(p, cam);
    var amount = 1.0 - clamp(1.0 / exp2(g.fog_consts.w * max(fd - g.fog_consts.x, 0.0)), 0.0, 1.0);
    let h = clamp((p.y - g.fog_consts2.x) / max(g.fog_consts2.y - g.fog_consts2.x, 1e-3), 0.0, 1.0);
    amount = amount * (1.0 - g.fog_consts2.z * h * h * (3.0 - 2.0 * h));
    let vdir = (p - cam) / max(fd, 1e-3);
    let s = pow(clamp(dot(vdir, g.sun_dir.xyz), 0.0, 1.0), max(g.fog_consts2.w, 1e-3)) * clamp((fd - g.fog_color.w) / max(g.fog_color2.w, 1e-3), 0.0, 1.0);
    let fog_rgb = g.fog_color.rgb + (g.fog_color2.rgb - g.fog_color.rgb) * s;

    var o: Out;
    o.clip = view.clip_from_world * vec4<f32>(p, 1.0);
    o.uv = v.uv;
    o.colour = vec4<f32>(v.colour.rgb, fade);
    o.lit = g.amb_color.rgb + clamp(dot(n, g.sun_dir.xyz), 0.0, 1.0) * g.sun_color.rgb;
    o.fog = vec4<f32>(fog_rgb * amount, 1.0 - amount);
    return o;
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    let t = textureSample(tex, samp, i.uv);
    let a = t.a * i.colour.a;
    if (a < 0.5) {
        discard;
    }
    var c = t.rgb * g.fade.z * i.colour.rgb * i.lit;
    c = c * i.fog.a + i.fog.rgb;
    if (g.fade.w > 0.5) {
        c = sqrt(max(c, vec3<f32>(0.0)));
    }
    return vec4<f32>(c, 1.0);
}
