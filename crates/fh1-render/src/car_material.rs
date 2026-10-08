//! `FxCarMaterial`: runs a car-library technique (shaders_v16 etc.). Same pipeline logic as
//! `FxMaterial`, but car shaders keep their per-material constants at arbitrary registers, so the
//! material carries full 256-register files, plus per-texture gamma flags and the car sampler slots.

use std::collections::HashMap;

use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError};
use bevy::render::storage::ShaderBuffer;

use crate::material::{specialize_fx, FxKey};

#[derive(Clone, Copy, Debug, ShaderType)]
pub struct FxCarConsts {
    pub vs: [Vec4; 256],
    pub ps: [Vec4; 256],
    /// x: bit tf = texture tf is gamma-signed.
    pub gamma: UVec4,
}

impl Default for FxCarConsts {
    fn default() -> Self {
        Self { vs: [Vec4::ZERO; 256], ps: [Vec4::ZERO; 256], gamma: UVec4::ZERO }
    }
}

/// Pipeline key: the shared fx key plus the car's code-set blend (the car library's passes carry
/// no blend states; the game sets them in code).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FxCarKey {
    pub fx: FxKey,
    pub alpha_blend: bool,
    pub additive: bool,
    pub depth_write: bool,
}

impl From<&FxCarMaterial> for FxCarKey {
    fn from(m: &FxCarMaterial) -> Self {
        Self {
            fx: FxKey { program: m.program, flip_cull: m.flip_cull, no_cull: m.no_cull },
            alpha_blend: m.alpha_blend,
            additive: m.additive,
            depth_write: m.depth_write,
        }
    }
}

#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
#[bind_group_data(FxCarKey)]
pub struct FxCarMaterial {
    #[uniform(0)]
    pub consts: FxCarConsts,
    /// The car globals bank (`FxCarGlobals`).
    #[storage(1, read_only)]
    pub globals: Handle<ShaderBuffer>,
    #[texture(2)]
    #[sampler(3)]
    pub t0: Option<Handle<Image>>,
    #[texture(4)]
    #[sampler(5)]
    pub t1: Option<Handle<Image>>,
    #[texture(6)]
    #[sampler(7)]
    pub t2: Option<Handle<Image>>,
    #[texture(8)]
    #[sampler(9)]
    pub t3: Option<Handle<Image>>,
    #[texture(10)]
    #[sampler(11)]
    pub t4: Option<Handle<Image>>,
    #[texture(12)]
    #[sampler(13)]
    pub t5: Option<Handle<Image>>,
    #[texture(14)]
    #[sampler(15)]
    pub t6: Option<Handle<Image>>,
    #[texture(16)]
    #[sampler(17)]
    pub t7: Option<Handle<Image>>,
    /// tf10: trackLightmapSampler.
    #[texture(22)]
    #[sampler(23)]
    pub t10: Option<Handle<Image>>,
    /// tf13: ShadowMaskSamp / s_varianceShadowMap.
    #[texture(28)]
    #[sampler(29)]
    pub t13: Option<Handle<Image>>,
    /// Cube slots (program::cube_slot): tf0/4, tf2/5, other.
    #[texture(34, dimension = "cube")]
    #[sampler(35)]
    pub cube0: Option<Handle<Image>>,
    #[texture(36, dimension = "cube")]
    #[sampler(37)]
    pub cube1: Option<Handle<Image>>,
    #[texture(38, dimension = "cube")]
    #[sampler(39)]
    pub cube2: Option<Handle<Image>>,
    /// Car headlight records + dip beam (crate::headlight, shared).
    #[storage(40, read_only)]
    pub headlights: Handle<bevy::render::storage::ShaderBuffer>,
    pub program: u32,
    pub flip_cull: bool,
    pub no_cull: bool,
    pub alpha_blend: bool,
    /// Additive (src·alpha + dst) over what is drawn, no depth write: the lens reflection pass (detail_glass pass 1).
    pub additive: bool,
    /// Extra transparent sort bias (m, positive = drawn later); see [`FxCarMaterial::depth_bias`].
    pub sort_bias: f32,
    /// A blended part that still writes depth: the lamp covers (lights_gls_*, detail_glass pass 0); see car.rs.
    pub depth_write: bool,
    /// A lamp cover technique (car.rs `lens_cover`).
    pub lens: bool,
    /// Draw index of the part in carbin order (car.rs spawn); see [`FxCarMaterial::depth_bias`].
    pub order: u32,
}

impl FxCarMaterial {
    /// Texture slot by sampler register (tf#), or 100 + k for cube slot k.
    pub fn slot_mut(&mut self, tf: u32) -> Option<&mut Option<Handle<Image>>> {
        Some(match tf {
            0 => &mut self.t0,
            1 => &mut self.t1,
            2 => &mut self.t2,
            3 => &mut self.t3,
            4 => &mut self.t4,
            5 => &mut self.t5,
            6 => &mut self.t6,
            7 => &mut self.t7,
            10 => &mut self.t10,
            13 => &mut self.t13,
            100 => &mut self.cube0,
            101 => &mut self.cube1,
            102 => &mut self.cube2,
            _ => return None,
        })
    }
}

/// Lamp sort order (opt-in FH1_CARFX_LENS_SORT=1; `ab` = flipped every 20 frames by car.rs `lamp_sort_ab`). Same-run
/// A/Bs showed no difference: the lamp flicker was the car's self-shadow (shadow.rs `car_shadow_bias`).
pub static LAMP_SORT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Car parts sort in carbin order (default on; FH1_CARFX_ORDER=0 off, `ab` = flipped every 20 frames by car.rs).
pub static PART_ORDER_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Transparent sort step per part (m): 150 parts span 1.5 cm, far below the distances to other transparent objects.
pub const PART_ORDER_STEP: f32 = 1e-4;

impl Material for FxCarMaterial {
    fn alpha_mode(&self) -> AlphaMode {
        if self.alpha_blend || self.additive {
            AlphaMode::Blend
        } else {
            AlphaMode::Opaque
        }
    }
    /// Additive overlays (lamp lens pass 1) reuse their pass-0 mesh, so both sort at the same mesh centre and Bevy's
    /// transparent order between them changed from frame to frame (the lens plastic flickered). A tiny bias keeps
    /// pass 1 drawn after pass 0, as the game draws its passes.
    /// Opt-in: lamp parts sort back to front by role (`sort_bias`, set in car.rs `lamp_sort_bias`). All car parts share
    /// the car root's translation, so Bevy's transparent sort ties them and the order falls to queue order.
    /// Default: every car part shares the car root's translation, so Bevy's transparent sort ties for all of them and
    /// the blended lamp layers (taillight, reflector, lights_gls_* lenses) drew in queue order, which changes between
    /// frames: the lamps flickered. The game draws the car parts in carbin order with LESS_EQUAL, so the parts sort
    /// by their carbin index here (later = on top); an additive pass 1 sits half a step after its pass 0.
    fn depth_bias(&self) -> f32 {
        if PART_ORDER_ON.load(std::sync::atomic::Ordering::Relaxed) {
            return self.order as f32 * PART_ORDER_STEP + if self.additive { PART_ORDER_STEP * 0.5 } else { 0.0 };
        }
        let lamp = if LAMP_SORT_ON.load(std::sync::atomic::Ordering::Relaxed) { self.sort_bias } else { 0.0 };
        lamp + if self.additive { 0.01 } else { 0.0 }
    }
    fn enable_prepass() -> bool {
        false
    }
    fn enable_shadows() -> bool {
        false
    }
    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        specialize_fx(descriptor, layout, key.bind_group_data.fx)?;
        if key.bind_group_data.additive {
            use bevy::render::render_resource::{BlendComponent, BlendFactor, BlendOperation, BlendState};
            if let Some(Some(t)) = descriptor.fragment.as_mut().and_then(|f| f.targets.first_mut()) {
                t.blend = Some(BlendState {
                    color: BlendComponent { src_factor: BlendFactor::SrcAlpha, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                    alpha: BlendComponent { src_factor: BlendFactor::Zero, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                });
            }
            if let Some(ds) = descriptor.depth_stencil.as_mut() {
                ds.depth_write_enabled = Some(false);
            }
            return Ok(());
        }
        if key.bind_group_data.alpha_blend {
            // Glass: src-alpha blend over the opaque car, no depth write (INFERRED: the window PS writes
            // alpha = min(opacity, GlassMaxOpacity) x alphaDirtAmount, a straight alpha).
            if let Some(Some(t)) = descriptor.fragment.as_mut().and_then(|f| f.targets.first_mut()) {
                if t.blend.is_none() {
                    t.blend = Some(bevy::render::render_resource::BlendState::ALPHA_BLENDING);
                }
            }
            if let Some(ds) = descriptor.depth_stencil.as_mut() {
                ds.depth_write_enabled = Some(key.bind_group_data.depth_write);
            }
        }
        Ok(())
    }
}

// ---- Bevy StandardMaterial objects under the FH1 post chain ----------------------------------
//
// The game's shaders write `sqrt(colour)` (docs/SHADERS.md, track main pass) and the post chain's
// first pass (`DownSample16XGammaCorrect`) squares the scene back. A plain StandardMaterial writes
// linear colour, so the chain squared it a second time: the glTF car's sun highlights (window glass
// roughness 0.05, chrome 0.1: linear ~10^2..10^4) became ~10^4..10^8 and Add_HotExtract + bloom
// persistence spread them into the big white blobs. `FxRawStandard` is StandardMaterial with the
// game's output encoding; `swap_raw_standard` puts it on every glTF scene the main view draws while
// the post chain is on (`FxLibrary::raw_output`).

use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::shader::ShaderRef;

pub type FxRawStandard = ExtendedMaterial<StandardMaterial, FxRawOutput>;

/// StandardMaterial extension: lit colour → `sqrt` (the FH1 scene encoding), before fog/premultiply.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone, Default)]
pub struct FxRawOutput {}

pub const RAW_STANDARD_SHADER: Handle<Shader> = bevy::asset::uuid_handle!("6f1d3a52-8c47-4e0b-9a1e-2b7c5d9e0f31");

const RAW_STANDARD_WGSL: &str = r#"
#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::alpha_discard,
}
#ifdef PREPASS_PIPELINE
#import bevy_pbr::{prepass_io::{VertexOutput, FragmentOutput}, pbr_deferred_functions::deferred_output}
#else
#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    pbr_types::STANDARD_MATERIAL_FLAGS_UNLIT_BIT,
}
#endif

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);
#ifdef PREPASS_PIPELINE
    let out = deferred_output(in, pbr_input);
#else
    var out: FragmentOutput;
    if (pbr_input.material.flags & STANDARD_MATERIAL_FLAGS_UNLIT_BIT) == 0u {
        out.color = apply_pbr_lighting(pbr_input);
    } else {
        out.color = pbr_input.material.base_color;
    }
    out.color = vec4<f32>(sqrt(max(out.color.rgb, vec3<f32>(0.0))), out.color.a);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
#endif
    return out;
}
"#;

impl MaterialExtension for FxRawOutput {
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Handle(RAW_STANDARD_SHADER)
    }
}

pub(crate) fn add_raw_standard(app: &mut App) {
    app.add_plugins(bevy::pbr::MaterialPlugin::<FxRawStandard>::default())
        .add_systems(PostUpdate, swap_raw_standard);
    let shader = Shader::from_wgsl(RAW_STANDARD_WGSL, "fh1/raw_standard.wgsl");
    let _ = app.world_mut().resource_mut::<Assets<Shader>>().insert(&RAW_STANDARD_SHADER, shader);
}

/// Swap StandardMaterial → FxRawStandard on meshes of glTF scenes (`WorldAssetRoot`) drawn by
/// the main view (no RenderLayers or layer 0; the UI/minimap scenes use their own layers and
/// their own non-FH1 output). Scenery stand-ins aren't scenes and are left alone.
#[allow(clippy::type_complexity)]
fn swap_raw_standard(
    mut commands: Commands,
    lib: Res<crate::FxLibrary>,
    added: Query<(Entity, &MeshMaterial3d<StandardMaterial>, Option<&bevy::camera::visibility::RenderLayers>), Added<MeshMaterial3d<StandardMaterial>>>,
    parents: Query<&ChildOf>,
    roots: Query<(), With<bevy::world_serialization::WorldAssetRoot>>,
    std_mats: Res<Assets<StandardMaterial>>,
    mut raw_mats: ResMut<Assets<FxRawStandard>>,
    mut cache: Local<HashMap<AssetId<StandardMaterial>, Handle<FxRawStandard>>>,
    mut frame: Local<u32>,
) {
    if !lib.raw_output {
        return;
    }
    // Drop entries only this cache still holds (P5b): every car shown in the car select left its materials and their
    // textures alive for the session (user drive: RawStdMat 66 -> 1,120, textures +860 MB). FH1_RAW_CACHE_SWEEP=0 = keep.
    *frame = frame.wrapping_add(1);
    if *frame % 300 == 0 && std::env::var("FH1_RAW_CACHE_SWEEP").map_or(true, |v| v != "0") {
        cache.retain(|_, h| match h {
            Handle::Strong(a) => std::sync::Arc::strong_count(a) > 1,
            _ => false,
        });
    }
    for (e, mat, layers) in &added {
        if layers.is_some_and(|l| !l.intersects(&bevy::camera::visibility::RenderLayers::layer(0))) {
            continue;
        }
        if !parents.iter_ancestors(e).any(|a| roots.contains(a)) {
            continue;
        }
        let handle = match cache.get(&mat.id()) {
            Some(h) => h.clone(),
            None => {
                let Some(base) = std_mats.get(&mat.0) else { continue };
                let h = raw_mats.add(FxRawStandard { base: base.clone(), extension: FxRawOutput {} });
                cache.insert(mat.id(), h.clone());
                h
            }
        };
        commands.entity(e).remove::<MeshMaterial3d<StandardMaterial>>().insert(MeshMaterial3d(handle));
    }
}
