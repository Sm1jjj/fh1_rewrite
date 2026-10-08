//! `FxMaterial`: a Bevy material that runs one translated FH1 effect pass.
//!
//! Every distinct (effect, technique) becomes a [`Program`] registered once in a global
//! registry; the material only carries its program id, its constants and textures. The
//! pipeline (shader, vertex layout, cull/blend/depth state) is chosen in `specialize` from the
//! registry, so all materials of one program share a pipeline.

use std::sync::RwLock;

use bevy::mesh::{MeshVertexAttribute, MeshVertexBufferLayoutRef, VertexFormat};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, CompareFunction, Face, RenderPipelineDescriptor,
    ShaderType, SpecializedMeshPipelineError,
};
use bevy::render::storage::ShaderBuffer;
use bevy::shader::ShaderRef;

use crate::program::{Blend, Cull, PassState, Program};

pub const ATTRIBUTE_UV2: MeshVertexAttribute = MeshVertexAttribute::new("Fx_Uv2", 0x4648_0001, VertexFormat::Float32x2);
pub const ATTRIBUTE_TANGENT: MeshVertexAttribute = MeshVertexAttribute::new("Fx_Tangent", 0x4648_0002, VertexFormat::Float32x3);
/// D3DCOLOR bytes in file order (A, R, G, B).
pub const ATTRIBUTE_COLOR: MeshVertexAttribute = MeshVertexAttribute::new("Fx_Color", 0x4648_0003, VertexFormat::Unorm8x4);
pub const ATTRIBUTE_UV3: MeshVertexAttribute = MeshVertexAttribute::new("Fx_Uv3", 0x4648_0004, VertexFormat::Float32x2);
pub const ATTRIBUTE_BINORMAL: MeshVertexAttribute = MeshVertexAttribute::new("Fx_Binormal", 0x4648_0005, VertexFormat::Float32x3);
pub const ATTRIBUTE_COLOR1: MeshVertexAttribute = MeshVertexAttribute::new("Fx_Color1", 0x4648_0006, VertexFormat::Unorm8x4);

/// Car pool attributes (raw carbin vertex, byte-swapped to little-endian).
pub const ATTRIBUTE_CAR_POSITION: MeshVertexAttribute = MeshVertexAttribute::new("Fx_CarPosition", 0x4648_0010, VertexFormat::Snorm16x4);
pub const ATTRIBUTE_CAR_UV0: MeshVertexAttribute = MeshVertexAttribute::new("Fx_CarUv0", 0x4648_0011, VertexFormat::Unorm16x2);
pub const ATTRIBUTE_CAR_UV1: MeshVertexAttribute = MeshVertexAttribute::new("Fx_CarUv1", 0x4648_0012, VertexFormat::Unorm16x2);
pub const ATTRIBUTE_CAR_TANFRAME: MeshVertexAttribute = MeshVertexAttribute::new("Fx_CarTanFrameQuat", 0x4648_0013, VertexFormat::Snorm16x4);
pub const ATTRIBUTE_CAR_SH0: MeshVertexAttribute = MeshVertexAttribute::new("Fx_CarSh0", 0x4648_0014, VertexFormat::Uint32);

/// Mesh attribute for a program attribute location (see `program::family_attribute_location`).
pub fn attribute_for_location(loc: u32) -> MeshVertexAttribute {
    match loc {
        20 => return ATTRIBUTE_CAR_POSITION,
        21 => return ATTRIBUTE_CAR_UV0,
        22 => return ATTRIBUTE_CAR_UV1,
        23 => return ATTRIBUTE_CAR_TANFRAME,
        24 => return ATTRIBUTE_CAR_SH0,
        // Skinned anim objects (anim.rs patches the rigid PROC_ANIM_OBJ VS to skin with Bevy's joints; 0f).
        25 => return Mesh::ATTRIBUTE_JOINT_INDEX,
        26 => return Mesh::ATTRIBUTE_JOINT_WEIGHT,
        _ => {}
    }
    match loc {
        0 => Mesh::ATTRIBUTE_POSITION,
        1 => Mesh::ATTRIBUTE_NORMAL,
        2 => Mesh::ATTRIBUTE_UV_0,
        3 => Mesh::ATTRIBUTE_UV_1,
        4 => ATTRIBUTE_UV2,
        5 => ATTRIBUTE_TANGENT,
        6 => ATTRIBUTE_COLOR,
        7 => ATTRIBUTE_UV3,
        8 => ATTRIBUTE_BINORMAL,
        _ => ATTRIBUTE_COLOR1,
    }
}

pub(crate) struct ProgramEntry {
    pub shader: Handle<Shader>,
    pub attributes: Vec<u32>,
    pub state: PassState,
}

/// Programs by id. Written in the main world when a program is created, read in `specialize`.
pub(crate) static PROGRAMS: RwLock<Vec<ProgramEntry>> = RwLock::new(Vec::new());

/// Register a program's shader; returns its id.
pub(crate) fn register(program: &Program, shader: Handle<Shader>) -> u32 {
    let mut p = PROGRAMS.write().unwrap();
    p.push(ProgramEntry { shader, attributes: program.attributes.clone(), state: program.state });
    (p.len() - 1) as u32
}

#[derive(Clone, Copy, Debug, ShaderType)]
pub struct FxMaterialConsts {
    /// VS c0..c15 (uvOffsetScale c0, positionScale c1, positionOffset c2, material c3..).
    pub vs: [Vec4; 16],
    /// PS c0..c15 (material constants c0..).
    pub ps: [Vec4; 16],
    /// x: bit tf = texture tf is gamma-signed (Xenos PWL degamma on fetch).
    pub gamma: UVec4,
    /// Per-object registers (`program::object_slot`): [0] ModelData c148 = instance tint RGBA/255,
    /// [1] SurfaceNormalAndShadowPower c162 = ground normal (engine space) + power.
    pub object: [Vec4; 2],
}

/// The object values most `.pgeo` instances carry (docs/PROPS.md "Per-instance shader values"):
/// tint `ff808080` (VERIFIED as the most common value), normal +Y, power 1 (GUESS).
pub const DEFAULT_OBJECT: [Vec4; 2] = [Vec4::new(128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 1.0), Vec4::new(0.0, 1.0, 0.0, 1.0)];

impl Default for FxMaterialConsts {
    fn default() -> Self {
        Self { vs: [Vec4::ZERO; 16], ps: [Vec4::ZERO; 16], gamma: UVec4::ZERO, object: DEFAULT_OBJECT }
    }
}

/// Per-object values from a `.pgeo` instance: D3DCOLOR tint (A,R,G,B in the high-to-low bytes) and the
/// ground normal in engine space.
pub fn object_consts(tint: u32, normal: Vec3) -> [Vec4; 2] {
    let c = |shift: u32| ((tint >> shift) & 0xFF) as f32 / 255.0;
    [Vec4::new(c(16), c(8), c(0), c(24)), normal.normalize_or(Vec3::Y).extend(DEFAULT_OBJECT[1].w)]
}

/// Pipeline key: program id plus options that change the pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FxKey {
    pub program: u32,
    /// Swap the cull direction (for meshes whose handedness differs from the original).
    pub flip_cull: bool,
    /// Ignore the effect's cull mode (draw both sides).
    pub no_cull: bool,
}

impl From<&FxMaterial> for FxKey {
    fn from(m: &FxMaterial) -> Self {
        Self { program: m.program, flip_cull: m.flip_cull, no_cull: m.no_cull }
    }
}

#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
#[bind_group_data(FxKey)]
pub struct FxMaterial {
    #[uniform(0)]
    pub consts: FxMaterialConsts,
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
    /// tf8: CSMShadowMapLevel0Samp.
    #[texture(18)]
    #[sampler(19)]
    pub t8: Option<Handle<Image>>,
    /// tf9..tf12: FH2's track effects go past FH1's register use (`sea_anim_norm_grad_refl_2/3` read the scene depth
    /// `g_DepthSampler` at tf10, docs/FH2_RECON.md); unbound = the fallback image. Same meaning as FxCarMaterial's t10.
    #[texture(20)]
    #[sampler(21)]
    pub t9: Option<Handle<Image>>,
    #[texture(22)]
    #[sampler(23)]
    pub t10: Option<Handle<Image>>,
    #[texture(24)]
    #[sampler(25)]
    pub t11: Option<Handle<Image>>,
    #[texture(26)]
    #[sampler(27)]
    pub t12: Option<Handle<Image>>,
    /// tf13: ShadowMaskSamp (screen-space sun shadow mask).
    #[texture(28)]
    #[sampler(29)]
    pub t13: Option<Handle<Image>>,
    /// tf14: s_headlightFalloffMap (1D in the original, bound as a 1-row 2D texture).
    #[texture(30)]
    #[sampler(31)]
    pub t14: Option<Handle<Image>>,
    /// tf15: s_headlightBeamMap.
    #[texture(32)]
    #[sampler(33)]
    pub t15: Option<Handle<Image>>,
    #[texture(34, dimension = "cube")]
    #[sampler(35)]
    pub cube0: Option<Handle<Image>>,
    #[texture(36, dimension = "cube")]
    #[sampler(37)]
    pub cube1: Option<Handle<Image>>,
    /// Cube slot 2 (FM4 track effects; FxCarMaterial uses the same binding).
    #[texture(38, dimension = "cube")]
    #[sampler(39)]
    pub cube2: Option<Handle<Image>>,
    /// Car headlight records + dip beam (crate::headlight, shared by every material).
    #[storage(40, read_only)]
    pub headlights: Handle<ShaderBuffer>,
    pub program: u32,
    pub flip_cull: bool,
    pub no_cull: bool,
    pub alpha_blend: bool,
}

impl FxMaterial {
    /// Texture slot by sampler register (tf#), or 100 + k for cube slot k; `None` if unbound.
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
            8 => &mut self.t8,
            9 => &mut self.t9,
            10 => &mut self.t10,
            11 => &mut self.t11,
            12 => &mut self.t12,
            13 => &mut self.t13,
            14 => &mut self.t14,
            15 => &mut self.t15,
            100 => &mut self.cube0,
            101 => &mut self.cube1,
            102 => &mut self.cube2,
            _ => return None,
        })
    }
}

fn blend_factor(x: u32) -> BlendFactor {
    match x {
        0 => BlendFactor::Zero,
        1 => BlendFactor::One,
        4 => BlendFactor::Src,
        5 => BlendFactor::OneMinusSrc,
        6 => BlendFactor::SrcAlpha,
        7 => BlendFactor::OneMinusSrcAlpha,
        8 => BlendFactor::Dst,
        9 => BlendFactor::OneMinusDst,
        10 => BlendFactor::DstAlpha,
        11 => BlendFactor::OneMinusDstAlpha,
        16 => BlendFactor::SrcAlphaSaturated,
        _ => BlendFactor::One,
    }
}

impl Material for FxMaterial {
    fn alpha_mode(&self) -> AlphaMode {
        crate::shadow::alpha_mode(self.program, self.alpha_blend)
    }

    /// The shadow pass draws the casters (crate::shadow). No depth prepass: the mask is evaluated per
    /// fragment, and a prepass that doesn't match the colour pass's discards punches holes.
    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        true
    }

    fn vertex_shader() -> ShaderRef {
        ShaderRef::Default
    }

    fn prepass_fragment_shader() -> ShaderRef {
        crate::shadow::prepass_fragment_shader()
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        if crate::shadow::is_prepass(descriptor) {
            crate::shadow::specialize_prepass(descriptor, layout, key.mesh_key, key.bind_group_data);
            return Ok(());
        }
        specialize_fx(descriptor, layout, key.bind_group_data)
    }
}

/// Pipeline setup shared by the FX material types: the program's shader, vertex layout and pass
/// states.
pub(crate) fn specialize_fx(descriptor: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, key: FxKey) -> Result<(), SpecializedMeshPipelineError> {
    let programs = PROGRAMS.read().unwrap();
    // Programs are registered before any material that uses them is created.
    let p = &programs[key.program as usize];
    let attrs: Vec<_> = p.attributes.iter().map(|&loc| attribute_for_location(loc).at_shader_location(loc)).collect();
    descriptor.vertex.buffers = vec![layout.0.get_layout(&attrs)?];
    descriptor.vertex.shader = p.shader.clone();
    descriptor.vertex.entry_point = Some("vertex".into());
    if let Some(f) = descriptor.fragment.as_mut() {
        f.shader = p.shader.clone();
        f.entry_point = Some("fragment".into());
        if let Some(Some(t)) = f.targets.first_mut() {
            t.blend = p.state.blend.map(|Blend { src, dst }| {
                let c = BlendComponent { src_factor: blend_factor(src), dst_factor: blend_factor(dst), operation: BlendOperation::Add };
                BlendState { color: c, alpha: c }
            });
        }
    }
    let st = p.state;
    // D3D culls by screen-space winding; wgpu's front face is CCW by default.
    let cull = if key.no_cull {
        Cull::None
    } else {
        match (st.cull, key.flip_cull) {
            (Cull::Cw, true) => Cull::Ccw,
            (Cull::Ccw, true) => Cull::Cw,
            (c, _) => c,
        }
    };
    descriptor.primitive.cull_mode = match cull {
        Cull::None => None,
        Cull::Cw => Some(Face::Back),
        Cull::Ccw => Some(Face::Front),
    };
    if let Some(ds) = descriptor.depth_stencil.as_mut() {
        ds.depth_write_enabled = Some(st.z_write);
        if !st.z_enable {
            ds.depth_compare = Some(CompareFunction::Always);
        }
        let slope = f32::from_bits(st.depth_bias.0);
        let constant = f32::from_bits(st.depth_bias.1);
        if slope != 0.0 || constant != 0.0 {
            // Reverse-Z: a bias towards the camera is positive.
            ds.bias.slope_scale = slope;
            ds.bias.constant = (constant * 16_777_216.0) as i32;
        }
        // Slope-only blended families (11 of 19 track decal families) held on the 360's 24-bit depth but z-fight on
        // Depth32Float reverse-Z where they face the camera: give them a small constant bias too (FH1_FX_DECAL_BIAS=0 = off).
        static DECAL_BIAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *DECAL_BIAS.get_or_init(|| !std::env::var("FH1_FX_DECAL_BIAS").is_ok_and(|v| v == "0"))
            && ds.bias.constant == 0
            && (st.blend.is_some() || slope > 0.0)
            && matches!(
                descriptor.primitive.topology,
                bevy::render::render_resource::PrimitiveTopology::TriangleList | bevy::render::render_resource::PrimitiveTopology::TriangleStrip
            )
        {
            ds.bias.constant = 256;
        }
    }
    descriptor.multisample.alpha_to_coverage_enabled = st.alpha_to_coverage && descriptor.multisample.count > 1;
    Ok(())
}
