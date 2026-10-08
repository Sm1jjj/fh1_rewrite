//! Materials for FH1's UI scenes: `AnarkMaterial` (unlit diffuse × up to four UV-transformed
//! texture slots) and `VectorTextMaterial` (the game's Loop-Blinn glyph coverage). Shaders are
//! embedded: `ui/anark.wgsl`, `ui/vtext.wgsl`.
//!
//! Both are `Material` (Mesh3d, the old HUD Camera3d) and `Material2d` (Mesh2d, drawn by the HUD Camera2d, default;
//! see scene.rs [`super::scene::hud_2d`]): same bind groups and fragment code (shader def `HUD_2D` picks the vertex
//! output struct), same blend states.

use bevy::asset::embedded_asset;
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, BlendState, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};

pub struct UiMaterialsPlugin;

impl Plugin for UiMaterialsPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "anark.wgsl");
        embedded_asset!(app, "vtext.wgsl");
        app.add_plugins((MaterialPlugin::<AnarkMaterial>::default(), MaterialPlugin::<VectorTextMaterial>::default()));
        app.add_plugins((Material2dPlugin::<AnarkMaterial>::default(), Material2dPlugin::<VectorTextMaterial>::default()));
    }
}

/// Reproduce FH1's UI output encoding (VERIFIED in the translated UI shaders: the quad PS
/// 0x82246498 and every text PS end with `sqrt(colour)`, written into the same gamma-2 front buffer
/// as the post chain's FinalCombine). Textures are then sampled raw (loaded as linear), the colour is
/// computed as the game does (gamma-flagged textures decoded with the Xenos PWL curve, colour
/// constants raw), and the shader returns `srgb_to_linear(sqrt(c))` so Bevy's sRGB encode
/// puts the game's byte on screen (same convention as fh1-render's final pass). Alpha blending still
/// happens in Bevy's linear space, not the 360's gamma space.
pub const GAMMA2: bool = true;

#[derive(Clone, Copy, ShaderType, Debug)]
pub struct AnarkUniform {
    pub color: Vec4,
    pub uv_x: [Vec4; 4],
    pub uv_y: [Vec4; 4],
    pub flags: UVec4,
}

impl Default for AnarkUniform {
    fn default() -> Self {
        Self { color: Vec4::ONE, uv_x: [Vec4::X; 4], uv_y: [Vec4::Y; 4], flags: UVec4::new(0, GAMMA2 as u32, 0, 0) }
    }
}

/// Pipeline key: additive vs normal alpha (the 2D pipeline has no `AlphaMode::Add`; set in `Material2d::specialize`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnarkKey {
    additive: bool,
}

impl From<&AnarkMaterial> for AnarkKey {
    fn from(m: &AnarkMaterial) -> Self {
        Self { additive: m.additive }
    }
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
#[bind_group_data(AnarkKey)]
pub struct AnarkMaterial {
    #[uniform(0)]
    pub u: AnarkUniform,
    #[texture(1)]
    #[sampler(2)]
    pub tex0: Option<Handle<Image>>,
    #[texture(3)]
    #[sampler(4)]
    pub tex1: Option<Handle<Image>>,
    #[texture(5)]
    #[sampler(6)]
    pub tex2: Option<Handle<Image>>,
    #[texture(7)]
    #[sampler(8)]
    pub tex3: Option<Handle<Image>>,
    /// Additive blending (fbf blend word 1) instead of normal alpha (7).
    pub additive: bool,
    /// Draw order (higher = in front). Bevy retains transparent phase items and only refreshes
    /// their sort distance when re-queued, so the order is carried here, not in the transform.
    pub order: f32,
}

impl Material for AnarkMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/ui/anark.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        if self.additive { AlphaMode::Add } else { AlphaMode::Blend }
    }

    fn depth_bias(&self) -> f32 {
        self.order
    }

    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        false
    }

    fn specialize(_: &MaterialPipeline, d: &mut RenderPipelineDescriptor, _: &MeshVertexBufferLayoutRef, _: MaterialPipelineKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        // Back faces are dropped when the mesh is built; mirrored (negative-scale) nodes must
        // still draw.
        d.primitive.cull_mode = None;
        Ok(())
    }
}

impl Material2d for AnarkMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/ui/anark.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }

    fn depth_bias(&self) -> f32 {
        self.order
    }

    fn specialize(d: &mut RenderPipelineDescriptor, _: &MeshVertexBufferLayoutRef, key: Material2dKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        d.primitive.cull_mode = None;
        if let Some(f) = d.fragment.as_mut() {
            f.shader_defs.push("HUD_2D".into());
            // The 3D path's AlphaMode::Add = Bevy's premultiplied blend with this shader's raw output.
            if key.bind_group_data.additive {
                for t in f.targets.iter_mut().flatten() {
                    t.blend = Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, ShaderType, Debug)]
pub struct TextUniform {
    pub text_colour: Vec4,
    pub outline_colour: Vec4,
    /// x = outline width (px), y = sign (+1 inner mesh, −1 outer mesh), z = gamma-2 flag.
    pub params: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct VectorTextMaterial {
    #[uniform(0)]
    pub u: TextUniform,
    /// Draw order, as [`AnarkMaterial::order`].
    pub order: f32,
}

impl VectorTextMaterial {
    pub fn new(colour: Vec4, outline: Option<(Vec4, f32)>, outer_mesh: bool) -> Self {
        let (oc, ow) = outline.unwrap_or((colour, 0.0));
        let sign = if outer_mesh { -1.0 } else { 1.0 };
        Self { u: TextUniform { text_colour: colour, outline_colour: oc, params: Vec4::new(ow, sign, GAMMA2 as u32 as f32, 0.0) }, order: 0.0 }
    }
}

impl Material for VectorTextMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/ui/vtext.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }

    fn depth_bias(&self) -> f32 {
        self.order
    }

    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        false
    }

    fn specialize(_: &MaterialPipeline, d: &mut RenderPipelineDescriptor, _: &MeshVertexBufferLayoutRef, _: MaterialPipelineKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        d.primitive.cull_mode = None;
        Ok(())
    }
}

impl Material2d for VectorTextMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_engine/ui/vtext.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }

    fn depth_bias(&self) -> f32 {
        self.order
    }

    fn specialize(d: &mut RenderPipelineDescriptor, _: &MeshVertexBufferLayoutRef, _: Material2dKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        d.primitive.cull_mode = None;
        if let Some(f) = d.fragment.as_mut() {
            f.shader_defs.push("HUD_2D".into());
        }
        Ok(())
    }
}
