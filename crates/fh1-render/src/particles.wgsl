// FH1 particles (particles.rs; docs/EFFECTS.md). The game's particle PS (media/shaders/v2/effects/particle_smoke.fxobj,
// VERIFIED by fxdump) is `rgb = (tex.rgb * colour.rgb - FogColor) * fog + FogColor`, `a = tex.a * colour.a`; lighting is
// in the CPU-built vertex colour. Here the quads are expanded in the VS (camera-facing, rotated by the spin angle), the
// fog factor is the scene's fog model (docs/SHADERS.md "Fog model") per vertex, and the particle fades near the ground
// it was spawned on (stand-in for the `_soft` variant's depth fade). The colour is written into the FH1 post chain's
// sqrt-encoded buffer (the CPU encodes the light; squared back when the post chain is off).

#import bevy_pbr::mesh_view_bindings::view
#import bevy_pbr::mesh_functions::get_world_from_local

struct ParticleParams {
    fog: vec4<f32>,
    fog_colour: vec4<f32>,
    fog2: vec4<f32>,
    fog_colour2: vec4<f32>,
    sun_dir: vec4<f32>,
    // x = 1: raw output, y = soft fade height (m), z = 1: gradient texture.
    flags: vec4<f32>,
    // x = max_screen_size: the largest quad as a fraction of the viewport height (0 = no cap); y = quad height / width;
    // z = atlas frame height (1 / rows).
    shape: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> p: ParticleParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var grad: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var grad_samp: sampler;

struct In {
    @builtin(instance_index) instance: u32,
    // Particle centre relative to the batch entity.
    @location(0) pos: vec3<f32>,
    // Corner (+-1, +-1).
    @location(1) corner: vec2<f32>,
    // (half size m, rotation rad)
    @location(2) size_rot: vec2<f32>,
    // XML rgb × sqrt(linear light), alpha.
    @location(3) colour: vec4<f32>,
    // (ground y, atlas u0, atlas frame width, life fraction)
    @location(4) extra: vec4<f32>,
}

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) colour: vec4<f32>,
    // (fog amount, soft fade, life fraction)
    @location(2) misc: vec3<f32>,
    @location(3) fog_rgb: vec3<f32>,
}

@vertex
fn vertex(v: In) -> Out {
    let centre = (get_world_from_local(v.instance) * vec4<f32>(v.pos, 1.0)).xyz;
    let right = view.world_from_view[0].xyz;
    let up = view.world_from_view[1].xyz;
    let s = sin(v.size_rot.y);
    let c = cos(v.size_rot.y);
    // XML max_screen_size: half height in NDC <= the fraction, i.e. half size (m) <= fraction * w / P11.
    var hs = v.size_rot.x;
    if (p.shape.x > 0.0) {
        let w = (view.clip_from_world * vec4<f32>(centre, 1.0)).w;
        hs = min(hs, p.shape.x * max(w, 1e-3) / view.clip_from_view[1][1]);
    }
    // shape.y = quad height / width (1 = square): the corner's y is stretched before the rotation.
    let aspect = select(1.0, p.shape.y, p.shape.y > 0.0);
    let cy = v.corner.y * aspect;
    let k = vec2<f32>(v.corner.x * c - cy * s, v.corner.x * s + cy * c) * hs;
    let world = centre + right * k.x + up * k.y;
    var o: Out;
    o.clip = view.clip_from_world * vec4<f32>(world, 1.0);
    let uv = vec2<f32>(v.corner.x * 0.5 + 0.5, 0.5 - v.corner.y * 0.5);
    // extra.y = atlas row (integer part) + column u offset; shape.z = frame height (1 / rows; 0 = one row).
    let row = floor(v.extra.y);
    let fh = select(1.0, p.shape.z, p.shape.z > 0.0);
    o.uv = vec2<f32>(fract(v.extra.y) + uv.x * v.extra.z, (row + uv.y) * fh);
    o.colour = v.colour;

    // Scene fog (docs/SHADERS.md "Fog model").
    let to = world - view.world_position;
    let d = length(to);
    var amount = 1.0 - clamp(1.0 / exp2(p.fog.w * max(d - p.fog.x, 0.0)), 0.0, 1.0);
    let span = p.fog2.y - p.fog2.x;
    if (abs(span) > 1e-4) {
        let h = clamp((world.y - p.fog2.x) / span, 0.0, 1.0);
        amount = amount * (1.0 - p.fog2.z * h * h * (3.0 - 2.0 * h));
    }
    var scatter = 0.0;
    if (d > 1e-3 && abs(p.fog_colour2.w) > 1e-4) {
        scatter = pow(clamp(dot(to / d, p.sun_dir.xyz), 0.0, 1.0), max(p.fog2.w, 1e-3)) * clamp((d - p.fog_colour.w) / p.fog_colour2.w, 0.0, 1.0);
    }
    o.fog_rgb = p.fog_colour.rgb + (p.fog_colour2.rgb - p.fog_colour.rgb) * scatter;
    // Soft fade above the spawn ground (only the quad's lower corners sit near it).
    let soft = clamp((world.y - v.extra.x) / max(p.flags.y, 1e-3), 0.0, 1.0);
    o.misc = vec3<f32>(amount, soft, v.extra.w);
    return o;
}

@fragment
fn fragment(i: Out) -> @location(0) vec4<f32> {
    var t = textureSample(tex, samp, i.uv);
    if (p.flags.z > 0.5) {
        t = vec4<f32>(t.rgb * textureSample(grad, grad_samp, vec2<f32>(i.misc.z, 0.5)).rgb, t.a);
    }
    // The vertex colour already holds the encoded light (particles.rs draw).
    let lit = t.rgb * i.colour.rgb;
    var rgb = mix(lit, i.fog_rgb, i.misc.x);
    if (p.flags.x < 0.5) {
        rgb = rgb * rgb * p.flags.w;
    }
    return vec4<f32>(rgb, t.a * i.colour.a * i.misc.y);
}
