//! Remaster car paint (W2): `StandardMaterial` + a small bindless extension.
//!
//! - Base colour = lerp(paint, atlas.rgb, atlas.a): the exterior atlas (nodamage) is white with alpha 0 where the body is
//!   painted and carries badges / lettering / trim with alpha 1 (seen on the Corrado atlas; the game's body technique
//!   reads PaintColor, docs/SHADERS.md "Car materials").
//! - Clear coat from StandardMaterial (`clearcoat`, `clearcoat_perceptual_roughness`); the coat normal stays the smooth
//!   geometric one.
//! - Metallic flake: per-texel-cell random normal jitter on the base layer only, faded out where a cell gets smaller
//!   than a pixel (no sparkle aliasing at distance), and off on the decals (atlas alpha).
//! - Fill (2026-10-07): the game lights cars with a sky/ground hemisphere (TOD HLTopColour / HLBottomColour as an SH,
//!   docs/SHADERS.md "Car globals") on top of the sun; the remaster's env map alone left shade-side panels at ~0.2x the
//!   faithful luma. The extension adds diffuse x lerp(bottom, top, n.y) in the game's display units x game_unit_scale
//!   (like the game-shader sky), so it lands where the faithful car's hemisphere term does. car.rs `update_car_fill`.
//! - Car sun: the game lights cars with sunColor x IBLDirectScaleCar where scenery gets x IBLDirectScaleEnvironment
//!   (VERIFIED, docs/SHADERS.md; Colorado day 0.63 vs 1.3). One Bevy sun lights both, so the extension takes
//!   (1 - fill_top.w) of the directional lights' Lambert diffuse back off the paint (fill_top.w = the ratio; 1 = no change).

use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, ShaderType};
use bevy::shader::ShaderRef;

pub type CarPaintMaterial = ExtendedMaterial<StandardMaterial, CarPaint>;

/// Paint parameters on top of the StandardMaterial base (whose base colour must be white, its texture the atlas).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
#[data(50, CarPaintUniform, binding_array(101))]
#[bindless(index_table(range(50..51), binding(100)))]
pub struct CarPaint {
    /// Paint colour, linear (Combo_Colors RGB sRGB-degammed).
    pub colour: LinearRgba,
    /// Flake normal jitter (0 = solid paint).
    pub flake: f32,
    /// Flake cells per UV unit.
    pub flake_scale: f32,
    /// Hemisphere fill above / below (post-exposure units, see the module doc; 0 = none). `fill_top.alpha` = the share of
    /// the directional lights' diffuse kept on the paint (car sun, module doc).
    pub fill_top: LinearRgba,
    pub fill_bottom: LinearRgba,
}

#[derive(Clone, Default, ShaderType)]
pub struct CarPaintUniform {
    pub colour: Vec4,
    /// x flake strength, y flake cells per UV unit.
    pub params: Vec4,
    pub fill_top: Vec4,
    pub fill_bottom: Vec4,
}

impl From<&CarPaint> for CarPaintUniform {
    fn from(p: &CarPaint) -> Self {
        Self { colour: p.colour.to_vec4(), params: Vec4::new(p.flake, p.flake_scale, 0.0, 0.0), fill_top: p.fill_top.to_vec4(), fill_bottom: p.fill_bottom.to_vec4() }
    }
}

pub(crate) const SHADER: Handle<Shader> = bevy::asset::uuid_handle!("6f1c2a8e-93d4-4b7a-a51e-2c0d9e7b3f12");

pub(crate) const WGSL: &str = r#"
#import bevy_render::maths::PI
#import bevy_pbr::{
    mesh_view_bindings as view_bindings,
    mesh_view_types,
    mesh_types::MESH_FLAGS_SHADOW_RECEIVER_BIT,
    shadows,
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
    mesh_bindings::mesh,
}

struct CarPaint { colour: vec4<f32>, params: vec4<f32>, fill_top: vec4<f32>, fill_bottom: vec4<f32> }

#ifdef BINDLESS
struct CarPaintIndices { material: u32 }
@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<storage> car_paint_indices: array<CarPaintIndices>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var<storage> car_paint: array<CarPaint>;
#else
@group(#{MATERIAL_BIND_GROUP}) @binding(50) var<uniform> car_paint: CarPaint;
#endif

fn hash3(p: vec2<f32>) -> vec3<f32> {
    var q = vec3<f32>(dot(p, vec2<f32>(127.1, 311.7)), dot(p, vec2<f32>(269.5, 183.3)), dot(p, vec2<f32>(419.2, 371.9)));
    return fract(sin(q) * 43758.5453);
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
#ifdef BINDLESS
    let slot = mesh[in.instance_index].material_and_lightmap_bind_group_slot & 0xffffu;
    let paint = car_paint[car_paint_indices[slot].material];
#else
    let paint = car_paint;
#endif
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    let atlas = pbr_input.material.base_color;
    let decal = atlas.a;
    pbr_input.material.base_color = vec4<f32>(mix(paint.colour.rgb, atlas.rgb, decal), 1.0);
#ifdef VERTEX_UVS_A
    if paint.params.x > 0.0 {
        let cells = in.uv * paint.params.y;
        let px = max(length(fwidth(cells)), 1e-4);
        let fade = (1.0 - smoothstep(0.35, 1.0, px)) * (1.0 - decal);
        let j = hash3(floor(cells)) * 2.0 - 1.0;
        pbr_input.N = normalize(pbr_input.N + j * (paint.params.x * fade));
    }
#endif
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    // Game hemisphere fill (module doc): diffuse albedo x lerp(ground, sky, n.y), already in post-exposure units.
    let diffuse = pbr_input.material.base_color.rgb * (1.0 - pbr_input.material.metallic);
    let hemi = mix(paint.fill_bottom.rgb, paint.fill_top.rgb, clamp(pbr_input.N.y * 0.5 + 0.5, 0.0, 1.0));
    out.color = vec4<f32>(out.color.rgb + diffuse * hemi * pbr_input.diffuse_occlusion, out.color.a);
    // Car sun (module doc): remove (1 - keep) of the directional lights' Lambert diffuse.
    let keep = paint.fill_top.w;
    if keep < 0.999 {
        let view_z = dot(vec4<f32>(
            view_bindings::view.view_from_world[0].z,
            view_bindings::view.view_from_world[1].z,
            view_bindings::view.view_from_world[2].z,
            view_bindings::view.view_from_world[3].z
        ), in.world_position);
        var direct = vec3<f32>(0.0);
        for (var i = 0u; i < view_bindings::lights.n_directional_lights; i = i + 1u) {
            let light = &view_bindings::lights.directional_lights[i];
            let n_dot_l = max(dot(pbr_input.N, (*light).direction_to_light), 0.0);
            if n_dot_l <= 0.0 {
                continue;
            }
            var shadow = 1.0;
            if ((*light).flags & mesh_view_types::DIRECTIONAL_LIGHT_FLAGS_SHADOWS_ENABLED_BIT) != 0u
                && (pbr_input.flags & MESH_FLAGS_SHADOW_RECEIVER_BIT) != 0u {
                shadow = shadows::fetch_directional_shadow(i, in.world_position, pbr_input.world_normal, view_z, in.position.xy);
            }
            direct = direct + (*light).color.rgb * n_dot_l * shadow;
        }
        let removed = diffuse * direct * (view_bindings::view.exposure * (1.0 - keep) / PI);
        out.color = vec4<f32>(max(out.color.rgb - removed, vec3<f32>(0.0)), out.color.a);
    }
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
"#;

impl MaterialExtension for CarPaint {
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Handle(SHADER)
    }
}
