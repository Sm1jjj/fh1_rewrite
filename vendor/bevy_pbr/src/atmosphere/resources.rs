use crate::{
    ExtractedAtmosphere, GpuLights, GpuScatteringMedium, LightMeta, ScatteringMediumSampler,
};
use bevy_asset::{load_embedded_asset, AssetId, Handle};
use bevy_camera::{Camera, Camera3d};
use bevy_core_pipeline::FullscreenShader;
use bevy_derive::Deref;
use bevy_ecs::{
    component::Component,
    entity::Entity,
    error::BevyError,
    query::With,
    resource::Resource,
    system::{Commands, Local, Query, Res, ResMut},
    world::{FromWorld, World},
};
use bevy_image::ToExtents;
use bevy_light::atmosphere::ScatteringMedium;
use bevy_math::{Affine3A, Mat4, UVec2, Vec3, Vec3A};
use bevy_render::{
    extract_component::ComponentUniforms,
    render_asset::RenderAssets,
    render_resource::{binding_types::*, *},
    renderer::{RenderDevice, RenderQueue},
    texture::{CachedTexture, TextureCache},
    view::{ExtractedView, Msaa, ViewDepthTexture, ViewUniform, ViewUniforms},
};
use bevy_shader::Shader;
use bevy_utils::default;

use super::GpuAtmosphereSettings;

#[derive(Resource)]
pub(crate) struct AtmosphereBindGroupLayouts {
    pub transmittance_lut: BindGroupLayoutDescriptor,
    pub multiscattering_lut: BindGroupLayoutDescriptor,
    pub sky_view_lut: BindGroupLayoutDescriptor,
    pub aerial_view_lut: BindGroupLayoutDescriptor,
}

#[derive(Resource)]
pub(crate) struct RenderSkyBindGroupLayouts {
    pub render_sky: BindGroupLayoutDescriptor,
    pub render_sky_msaa: BindGroupLayoutDescriptor,
    pub fullscreen_shader: FullscreenShader,
    pub fragment_shader: Handle<Shader>,
}

impl AtmosphereBindGroupLayouts {
    pub fn new() -> Self {
        let transmittance_lut = BindGroupLayoutDescriptor::new(
            "transmittance_lut_bind_group_layout",
            &BindGroupLayoutEntries::with_indices(
                ShaderStages::COMPUTE,
                (
                    (0, uniform_buffer::<GpuAtmosphere>(true)),
                    (1, uniform_buffer::<GpuAtmosphereSettings>(true)),
                    // scattering medium luts and sampler
                    (5, texture_2d(TextureSampleType::default())),
                    (6, texture_2d(TextureSampleType::default())),
                    (7, sampler(SamplerBindingType::Filtering)),
                    // transmittance lut storage texture
                    (
                        13,
                        texture_storage_2d(
                            TextureFormat::Rgba16Float,
                            StorageTextureAccess::WriteOnly,
                        ),
                    ),
                ),
            ),
        );

        let multiscattering_lut = BindGroupLayoutDescriptor::new(
            "multiscattering_lut_bind_group_layout",
            &BindGroupLayoutEntries::with_indices(
                ShaderStages::COMPUTE,
                (
                    (0, uniform_buffer::<GpuAtmosphere>(true)),
                    (1, uniform_buffer::<GpuAtmosphereSettings>(true)),
                    // scattering medium luts and sampler
                    (5, texture_2d(TextureSampleType::default())),
                    (6, texture_2d(TextureSampleType::default())),
                    (7, sampler(SamplerBindingType::Filtering)),
                    // atmosphere luts and sampler
                    (8, texture_2d(TextureSampleType::default())), // transmittance
                    (12, sampler(SamplerBindingType::Filtering)),
                    // multiscattering lut storage texture
                    (
                        13,
                        texture_storage_2d(
                            TextureFormat::Rgba16Float,
                            StorageTextureAccess::WriteOnly,
                        ),
                    ),
                ),
            ),
        );

        let sky_view_lut = BindGroupLayoutDescriptor::new(
            "sky_view_lut_bind_group_layout",
            &BindGroupLayoutEntries::with_indices(
                ShaderStages::COMPUTE,
                (
                    (0, uniform_buffer::<GpuAtmosphere>(true)),
                    (1, uniform_buffer::<GpuAtmosphereSettings>(true)),
                    (2, uniform_buffer::<AtmosphereTransform>(true)),
                    (3, uniform_buffer::<ViewUniform>(true)),
                    (4, uniform_buffer::<GpuLights>(true)),
                    // scattering medium luts and sampler
                    (5, texture_2d(TextureSampleType::default())),
                    (6, texture_2d(TextureSampleType::default())),
                    (7, sampler(SamplerBindingType::Filtering)),
                    // atmosphere luts and sampler
                    (8, texture_2d(TextureSampleType::default())), // transmittance
                    (9, texture_2d(TextureSampleType::default())), // multiscattering
                    (12, sampler(SamplerBindingType::Filtering)),
                    // sky view lut storage texture
                    (
                        13,
                        texture_storage_2d(
                            TextureFormat::Rgba16Float,
                            StorageTextureAccess::WriteOnly,
                        ),
                    ),
                ),
            ),
        );

        let aerial_view_lut = BindGroupLayoutDescriptor::new(
            "aerial_view_lut_bind_group_layout",
            &BindGroupLayoutEntries::with_indices(
                ShaderStages::COMPUTE,
                (
                    (0, uniform_buffer::<GpuAtmosphere>(true)),
                    (1, uniform_buffer::<GpuAtmosphereSettings>(true)),
                    (3, uniform_buffer::<ViewUniform>(true)),
                    (4, uniform_buffer::<GpuLights>(true)),
                    // scattering medium luts and sampler
                    (5, texture_2d(TextureSampleType::default())),
                    (6, texture_2d(TextureSampleType::default())),
                    (7, sampler(SamplerBindingType::Filtering)),
                    // atmosphere luts and sampler
                    (8, texture_2d(TextureSampleType::default())), // transmittance
                    (9, texture_2d(TextureSampleType::default())), // multiscattering
                    (12, sampler(SamplerBindingType::Filtering)),
                    // eerial view lut storage texture
                    (
                        13,
                        texture_storage_3d(
                            TextureFormat::Rgba16Float,
                            StorageTextureAccess::WriteOnly,
                        ),
                    ),
                ),
            ),
        );

        Self {
            transmittance_lut,
            multiscattering_lut,
            sky_view_lut,
            aerial_view_lut,
        }
    }
}

impl FromWorld for RenderSkyBindGroupLayouts {
    fn from_world(world: &mut World) -> Self {
        let render_sky = BindGroupLayoutDescriptor::new(
            "render_sky_bind_group_layout",
            &BindGroupLayoutEntries::with_indices(
                ShaderStages::FRAGMENT,
                (
                    (0, uniform_buffer::<GpuAtmosphere>(true)),
                    (1, uniform_buffer::<GpuAtmosphereSettings>(true)),
                    (2, uniform_buffer::<AtmosphereTransform>(true)),
                    (3, uniform_buffer::<ViewUniform>(true)),
                    (4, uniform_buffer::<GpuLights>(true)),
                    // scattering medium luts and sampler
                    (5, texture_2d(TextureSampleType::default())),
                    (6, texture_2d(TextureSampleType::default())),
                    (7, sampler(SamplerBindingType::Filtering)),
                    // atmosphere luts and sampler
                    (8, texture_2d(TextureSampleType::default())), // transmittance
                    (9, texture_2d(TextureSampleType::default())), // multiscattering
                    (10, texture_2d(TextureSampleType::default())), // sky view
                    (11, texture_3d(TextureSampleType::default())), // aerial view
                    (12, sampler(SamplerBindingType::Filtering)),
                    // view depth texture
                    (13, texture_2d(TextureSampleType::Depth)),
                ),
            ),
        );

        let render_sky_msaa = BindGroupLayoutDescriptor::new(
            "render_sky_msaa_bind_group_layout",
            &BindGroupLayoutEntries::with_indices(
                ShaderStages::FRAGMENT,
                (
                    (0, uniform_buffer::<GpuAtmosphere>(true)),
                    (1, uniform_buffer::<GpuAtmosphereSettings>(true)),
                    (2, uniform_buffer::<AtmosphereTransform>(true)),
                    (3, uniform_buffer::<ViewUniform>(true)),
                    (4, uniform_buffer::<GpuLights>(true)),
                    // scattering medium luts and sampler
                    (5, texture_2d(TextureSampleType::default())),
                    (6, texture_2d(TextureSampleType::default())),
                    (7, sampler(SamplerBindingType::Filtering)),
                    // atmosphere luts and sampler
                    (8, texture_2d(TextureSampleType::default())), // transmittance
                    (9, texture_2d(TextureSampleType::default())), // multiscattering
                    (10, texture_2d(TextureSampleType::default())), // sky view
                    (11, texture_3d(TextureSampleType::default())), // aerial view
                    (12, sampler(SamplerBindingType::Filtering)),
                    // view depth texture
                    (13, texture_2d_multisampled(TextureSampleType::Depth)),
                ),
            ),
        );

        Self {
            render_sky,
            render_sky_msaa,
            fullscreen_shader: world.resource::<FullscreenShader>().clone(),
            fragment_shader: load_embedded_asset!(world, "render_sky.wgsl"),
        }
    }
}

#[derive(Resource, Deref)]
pub struct AtmosphereSampler(Sampler);

impl FromWorld for AtmosphereSampler {
    fn from_world(world: &mut World) -> Self {
        let render_device = world.resource::<RenderDevice>();

        let sampler = render_device.create_sampler(&SamplerDescriptor {
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self(sampler)
    }
}

#[derive(Resource)]
pub struct AtmosphereLutPipelines {
    pub transmittance_lut: CachedComputePipelineId,
    pub multiscattering_lut: CachedComputePipelineId,
    pub sky_view_lut: CachedComputePipelineId,
    pub aerial_view_lut: CachedComputePipelineId,
}

impl FromWorld for AtmosphereLutPipelines {
    fn from_world(world: &mut World) -> Self {
        let pipeline_cache = world.resource::<PipelineCache>();
        let layouts = world.resource::<AtmosphereBindGroupLayouts>();

        let transmittance_lut = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
            label: Some("transmittance_lut_pipeline".into()),
            layout: vec![layouts.transmittance_lut.clone()],
            shader: load_embedded_asset!(world, "transmittance_lut.wgsl"),
            ..default()
        });

        let multiscattering_lut =
            pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
                label: Some("multi_scattering_lut_pipeline".into()),
                layout: vec![layouts.multiscattering_lut.clone()],
                shader: load_embedded_asset!(world, "multiscattering_lut.wgsl"),
                ..default()
            });

        let sky_view_lut = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
            label: Some("sky_view_lut_pipeline".into()),
            layout: vec![layouts.sky_view_lut.clone()],
            shader: load_embedded_asset!(world, "sky_view_lut.wgsl"),
            ..default()
        });

        let aerial_view_lut = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
            label: Some("aerial_view_lut_pipeline".into()),
            layout: vec![layouts.aerial_view_lut.clone()],
            shader: load_embedded_asset!(world, "aerial_view_lut.wgsl"),
            ..default()
        });

        Self {
            transmittance_lut,
            multiscattering_lut,
            sky_view_lut,
            aerial_view_lut,
        }
    }
}

#[derive(Component)]
pub struct RenderSkyPipelineId(pub CachedRenderPipelineId);

#[derive(Copy, Clone, Hash, PartialEq, Eq)]
pub(crate) struct RenderSkyPipelineKey {
    pub msaa_samples: u32,
    pub dual_source_blending: bool,
}

impl SpecializedRenderPipeline for RenderSkyBindGroupLayouts {
    type Key = RenderSkyPipelineKey;

    fn specialize(&self, key: Self::Key) -> RenderPipelineDescriptor {
        let mut shader_defs = Vec::new();

        if key.msaa_samples > 1 {
            shader_defs.push("MULTISAMPLED".into());
        }
        if key.dual_source_blending {
            shader_defs.push("DUAL_SOURCE_BLENDING".into());
        }

        let dst_factor = if key.dual_source_blending {
            BlendFactor::Src1
        } else {
            BlendFactor::SrcAlpha
        };

        RenderPipelineDescriptor {
            label: Some(format!("render_sky_pipeline_{}", key.msaa_samples).into()),
            layout: vec![if key.msaa_samples == 1 {
                self.render_sky.clone()
            } else {
                self.render_sky_msaa.clone()
            }],
            vertex: self.fullscreen_shader.to_vertex_state(),
            fragment: Some(FragmentState {
                shader: self.fragment_shader.clone(),
                shader_defs,
                targets: vec![Some(ColorTargetState {
                    format: TextureFormat::Rgba16Float,
                    blend: Some(BlendState {
                        color: BlendComponent {
                            src_factor: BlendFactor::One,
                            dst_factor,
                            operation: BlendOperation::Add,
                        },
                        alpha: BlendComponent {
                            src_factor: BlendFactor::Zero,
                            dst_factor: BlendFactor::One,
                            operation: BlendOperation::Add,
                        },
                    }),
                    write_mask: ColorWrites::ALL,
                })],
                ..default()
            }),
            multisample: MultisampleState {
                count: key.msaa_samples,
                ..default()
            },
            ..default()
        }
    }
}

pub(super) fn queue_render_sky_pipelines(
    views: Query<(Entity, &Msaa), (With<Camera>, With<ExtractedAtmosphere>)>,
    pipeline_cache: Res<PipelineCache>,
    layouts: Res<RenderSkyBindGroupLayouts>,
    mut specializer: ResMut<SpecializedRenderPipelines<RenderSkyBindGroupLayouts>>,
    render_device: Res<RenderDevice>,
    mut commands: Commands,
) {
    for (entity, msaa) in &views {
        let id = specializer.specialize(
            &pipeline_cache,
            &layouts,
            RenderSkyPipelineKey {
                msaa_samples: msaa.samples(),
                dual_source_blending: render_device
                    .features()
                    .contains(WgpuFeatures::DUAL_SOURCE_BLENDING),
            },
        );
        commands.entity(entity).insert(RenderSkyPipelineId(id));
    }
}

#[derive(Component)]
pub struct AtmosphereTextures {
    pub transmittance_lut: CachedTexture,
    pub multiscattering_lut: CachedTexture,
    pub sky_view_lut: CachedTexture,
    pub aerial_view_lut: CachedTexture,
}

/// FH1 patch 11 (P18): the transmittance and multiscattering LUTs depend only on the planet (radii, ground albedo), the
/// scattering medium and the LUT settings, not on the view or the sun, yet upstream takes them from the per-frame
/// `TextureCache` and re-renders both every frame. With `static_luts_on()` each view keeps its own pair (re-created when
/// a size changes) and [`AtmosphereStaticLutsValid`] tells `atmosphere_luts` to skip their dispatches while their inputs
/// are unchanged. `FH1_ATMO_STATIC_LUTS=0` = old.
pub(crate) fn static_luts_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_ATMO_STATIC_LUTS").map_or(true, |v| v != "0"))
}

/// FH1 patch 11: this view's transmittance + multiscattering LUTs already hold this frame's inputs (rendered on an
/// earlier frame into the same persistent textures): `atmosphere_luts` skips those two dispatches.
#[derive(Component)]
pub struct AtmosphereStaticLutsValid;

/// FH1 patch 11: a view's persistent static LUT pair, by size.
pub(super) struct StaticLutTextures {
    sizes: (UVec2, UVec2),
    transmittance_lut: CachedTexture,
    multiscattering_lut: CachedTexture,
}

pub(super) fn prepare_atmosphere_textures(
    views: Query<(Entity, &GpuAtmosphereSettings), With<ExtractedAtmosphere>>,
    render_device: Res<RenderDevice>,
    mut texture_cache: ResMut<TextureCache>,
    mut commands: Commands,
    mut static_luts: Local<bevy_platform::collections::HashMap<Entity, StaticLutTextures>>,
) {
    let descriptor = |label: &'static str, size: UVec2| TextureDescriptor {
        label: Some(label),
        size: size.to_extents(),
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba16Float,
        usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    if static_luts_on() {
        static_luts.retain(|e, _| views.contains(*e));
    }
    for (entity, lut_settings) in &views {
        let (transmittance_lut, multiscattering_lut) = if static_luts_on() {
            let sizes = (lut_settings.transmittance_lut_size, lut_settings.multiscattering_lut_size);
            let new = || StaticLutTextures {
                sizes,
                transmittance_lut: persistent_texture(&render_device, &descriptor("transmittance_lut", sizes.0)),
                multiscattering_lut: persistent_texture(&render_device, &descriptor("multiscattering_lut", sizes.1)),
            };
            let entry = static_luts.entry(entity).or_insert_with(new);
            if entry.sizes != sizes {
                *entry = new();
            }
            (entry.transmittance_lut.clone(), entry.multiscattering_lut.clone())
        } else {
            (
                texture_cache.get(&render_device, descriptor("transmittance_lut", lut_settings.transmittance_lut_size)),
                texture_cache.get(&render_device, descriptor("multiscattering_lut", lut_settings.multiscattering_lut_size)),
            )
        };

        let sky_view_lut = texture_cache.get(
            &render_device,
            TextureDescriptor {
                label: Some("sky_view_lut"),
                size: lut_settings.sky_view_lut_size.to_extents(),
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Rgba16Float,
                usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
        );

        let aerial_view_lut = texture_cache.get(
            &render_device,
            TextureDescriptor {
                label: Some("aerial_view_lut"),
                size: lut_settings.aerial_view_lut_size.to_extents(),
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D3,
                format: TextureFormat::Rgba16Float,
                usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
        );

        commands.entity(entity).insert({
            AtmosphereTextures {
                transmittance_lut,
                multiscattering_lut,
                sky_view_lut,
                aerial_view_lut,
            }
        });
    }
}

/// FH1 patch 11: a texture + default view that lives as long as its owner (not the per-frame `TextureCache`).
fn persistent_texture(render_device: &RenderDevice, descriptor: &TextureDescriptor) -> CachedTexture {
    let texture = render_device.create_texture(descriptor);
    let default_view = texture.create_view(&TextureViewDescriptor::default());
    CachedTexture { texture, default_view }
}

#[derive(Copy, Clone, Debug, thiserror::Error)]
#[error("ScatteringMedium missing with id {0:?}: make sure the asset was not removed.")]
struct ScatteringMediumMissingError(AssetId<ScatteringMedium>);

/// The shader-uniform representation of an Atmosphere.
#[derive(Clone, Component, ShaderType)]
pub struct GpuAtmosphere {
    //TODO: rename to Planet later?
    pub ground_albedo: Vec3,
    pub inner_radius: f32,
    pub outer_radius: f32,
    pub world_to_atmosphere: Mat4,
}

pub fn prepare_atmosphere_uniforms(
    mut commands: Commands,
    atmospheres: Query<(Entity, &ExtractedAtmosphere)>,
) -> Result<(), BevyError> {
    for (entity, atmosphere) in atmospheres {
        commands.entity(entity).insert(GpuAtmosphere {
            ground_albedo: atmosphere.ground_albedo,
            inner_radius: atmosphere.inner_radius,
            outer_radius: atmosphere.outer_radius,
            world_to_atmosphere: atmosphere.world_to_atmosphere,
        });
    }
    Ok(())
}

#[derive(Resource, Default)]
pub struct AtmosphereTransforms {
    uniforms: DynamicUniformBuffer<AtmosphereTransform>,
}

impl AtmosphereTransforms {
    #[inline]
    pub fn uniforms(&self) -> &DynamicUniformBuffer<AtmosphereTransform> {
        &self.uniforms
    }
}

/// Transforms between world space and atmosphere space.
///
/// Up is the local planet surface normal, so the horizon stays along the x-z
/// plane for horizon-detail parameterization. Back is chosen from a constant
/// world-horizontal direction such as `Vec3A::NEG_Z`, then projected orthogonal
/// to up. It may drift slightly from world-horizontal but stays camera-independent.
#[derive(ShaderType)]
pub struct AtmosphereTransform {
    world_from_atmosphere: Mat4,
    atmosphere_from_world: Mat4,
}

#[derive(Component)]
pub struct AtmosphereTransformsOffset {
    index: u32,
}

impl AtmosphereTransformsOffset {
    #[inline]
    pub fn index(&self) -> u32 {
        self.index
    }
}

pub(super) fn prepare_atmosphere_transforms(
    views: Query<
        (Entity, &ExtractedView, &GpuAtmosphere),
        (With<ExtractedAtmosphere>, With<Camera3d>),
    >,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut atmo_uniforms: ResMut<AtmosphereTransforms>,
    mut commands: Commands,
) {
    let atmo_count = views.iter().len();
    let Some(mut writer) =
        atmo_uniforms
            .uniforms
            .get_writer(atmo_count, &render_device, &render_queue)
    else {
        return;
    };

    for (entity, view, gpu_atmosphere) in &views {
        // Camera position in atmosphere space
        let cam_world = view.world_from_view.translation();
        let cam_pos = Vec3A::from(
            gpu_atmosphere
                .world_to_atmosphere
                .transform_point3(cam_world),
        );

        // Up is the local planet surface normal.
        let atmo_y = cam_pos.try_normalize().unwrap_or(Vec3A::Y);

        // World-horizontal reference for back, projected orthogonal to atmo_y.
        let world_ref = Vec3A::NEG_Z;
        let ref_horizontal = world_ref - atmo_y * atmo_y.dot(world_ref);
        let atmo_z = ref_horizontal.try_normalize().unwrap_or_else(|| {
            // `NEG_Z` is degenerate at the poles of a Z-up world.
            let fallback_ref = Vec3A::NEG_Y;
            (fallback_ref - atmo_y * atmo_y.dot(fallback_ref)).normalize()
        });
        let atmo_x = atmo_y.cross(atmo_z).normalize();

        let world_from_atmosphere = Mat4::from(Affine3A::from_cols(
            atmo_x,
            atmo_y,
            atmo_z,
            view.world_from_view.translation_vec3a(),
        ));
        // The shader only uses the upper-left 3x3 block, where transpose equals inverse for
        // orthonormal matrices and is cheaper than computing the full inverse.
        let atmosphere_from_world = world_from_atmosphere.transpose();

        commands.entity(entity).insert(AtmosphereTransformsOffset {
            index: writer.write(&AtmosphereTransform {
                world_from_atmosphere,
                atmosphere_from_world,
            }),
        });
    }
}

#[derive(Component)]
pub struct AtmosphereBindGroups {
    pub transmittance_lut: BindGroup,
    pub multiscattering_lut: BindGroup,
    pub sky_view_lut: BindGroup,
    pub aerial_view_lut: BindGroup,
    pub render_sky: BindGroup,
}

#[derive(Copy, Clone, Debug, thiserror::Error)]
enum AtmosphereBindGroupError {
    #[error("Failed to prepare atmosphere bind groups. Atmosphere uniform buffer missing")]
    Atmosphere,
    #[error(
        "Failed to prepare atmosphere bind groups. AtmosphereTransforms uniform buffer missing"
    )]
    Transforms,
    #[error("Failed to prepare atmosphere bind groups. AtmosphereSettings uniform buffer missing")]
    Settings,
    #[error("Failed to prepare atmosphere bind groups. View uniform buffer missing")]
    ViewUniforms,
    #[error("Failed to prepare atmosphere bind groups. Light uniform buffer missing")]
    LightUniforms,
}

/// FH1 patch 11: what a view's static LUTs were last rendered from (texture views, medium LUT views, planet, settings).
type StaticLutKey = (
    [WgpuTextureView; 4],
    [u32; 5],
    (UVec2, UVec2, u32, u32, u32),
);

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_atmosphere_bind_groups(
    views: Query<
        (
            Entity,
            &ExtractedAtmosphere,
            &AtmosphereTextures,
            &ViewDepthTexture,
            &Msaa,
            (Option<&GpuAtmosphere>, &GpuAtmosphereSettings),
        ),
        (With<Camera3d>, With<ExtractedAtmosphere>),
    >,
    (lut_pipelines, mut bind_group_cache, mut static_lut_keys): (
        Res<AtmosphereLutPipelines>,
        Local<crate::render::PreprocessBindGroupCache>,
        Local<bevy_platform::collections::HashMap<Entity, StaticLutKey>>,
    ),
    render_device: Res<RenderDevice>,
    layouts: Res<AtmosphereBindGroupLayouts>,
    render_sky_layouts: Res<RenderSkyBindGroupLayouts>,
    atmosphere_sampler: Res<AtmosphereSampler>,
    view_uniforms: Res<ViewUniforms>,
    lights_uniforms: Res<LightMeta>,
    atmosphere_transforms: Res<AtmosphereTransforms>,
    atmosphere_uniforms: Res<ComponentUniforms<GpuAtmosphere>>,
    settings_uniforms: Res<ComponentUniforms<GpuAtmosphereSettings>>,
    gpu_media: Res<RenderAssets<GpuScatteringMedium>>,
    medium_sampler: Res<ScatteringMediumSampler>,
    pipeline_cache: Res<PipelineCache>,
    mut commands: Commands,
) -> Result<(), BevyError> {
    if views.iter().len() == 0 {
        return Ok(());
    }

    let atmosphere_binding = atmosphere_uniforms
        .binding()
        .ok_or(AtmosphereBindGroupError::Atmosphere)?;

    let transforms_binding = atmosphere_transforms
        .uniforms()
        .binding()
        .ok_or(AtmosphereBindGroupError::Transforms)?;

    let settings_binding = settings_uniforms
        .binding()
        .ok_or(AtmosphereBindGroupError::Settings)?;

    let view_binding = view_uniforms
        .uniforms
        .binding()
        .ok_or(AtmosphereBindGroupError::ViewUniforms)?;

    let lights_binding = lights_uniforms
        .view_gpu_lights
        .binding()
        .ok_or(AtmosphereBindGroupError::LightUniforms)?;

    // FH1 patch 11: the LUT pass dispatches only when all four pipelines exist (node.rs); before that nothing is rendered.
    let luts_ready = [lut_pipelines.transmittance_lut, lut_pipelines.multiscattering_lut, lut_pipelines.sky_view_lut, lut_pipelines.aerial_view_lut]
        .iter()
        .all(|id| pipeline_cache.get_compute_pipeline(*id).is_some());
    static_lut_keys.retain(|e, _| views.contains(*e));
    bind_group_cache.begin_frame();
    let cache = core::cell::RefCell::new(core::mem::take(&mut *bind_group_cache));
    let result = (|| -> Result<(), BevyError> {
    for (entity, atmosphere, textures, view_depth_texture, msaa, (gpu_atmosphere, settings)) in &views {
        let gpu_medium = gpu_media
            .get(atmosphere.medium)
            .ok_or(ScatteringMediumMissingError(atmosphere.medium))?;

        // FH1 patch 11: static LUTs still valid = same persistent textures, medium, planet and settings as the frame that
        // rendered them.
        if static_luts_on() {
            let key = gpu_atmosphere.map(|a| -> StaticLutKey {
                (
                    [
                        (*textures.transmittance_lut.default_view).clone(),
                        (*textures.multiscattering_lut.default_view).clone(),
                        (*gpu_medium.density_lut_view).clone(),
                        (*gpu_medium.scattering_lut_view).clone(),
                    ],
                    [a.inner_radius.to_bits(), a.outer_radius.to_bits(), a.ground_albedo.x.to_bits(), a.ground_albedo.y.to_bits(), a.ground_albedo.z.to_bits()],
                    (
                        settings.transmittance_lut_size,
                        settings.multiscattering_lut_size,
                        settings.transmittance_lut_samples,
                        settings.multiscattering_lut_dirs,
                        settings.multiscattering_lut_samples,
                    ),
                )
            });
            let valid = key.is_some() && static_lut_keys.get(&entity) == key.as_ref();
            if valid {
                commands.entity(entity).insert(AtmosphereStaticLutsValid);
            } else {
                commands.entity(entity).remove::<AtmosphereStaticLutsValid>();
                // Rendered this frame (when the pass can run): valid from the next frame on.
                match key {
                    Some(key) if luts_ready => {
                        static_lut_keys.insert(entity, key);
                    }
                    _ => {
                        static_lut_keys.remove(&entity);
                    }
                }
            }
        }

        let transmittance_lut = crate::render::cached_bind_group(&cache, &render_device,
            "transmittance_lut_bind_group",
            &pipeline_cache.get_bind_group_layout(&layouts.transmittance_lut),
            &BindGroupEntries::with_indices((
                // uniforms
                (0, atmosphere_binding.clone()),
                (1, settings_binding.clone()),
                // scattering medium luts and sampler
                (5, &gpu_medium.density_lut_view),
                (6, &gpu_medium.scattering_lut_view),
                (7, medium_sampler.sampler()),
                // transmittance lut storage texture
                (13, &textures.transmittance_lut.default_view),
            )),
        );

        let multiscattering_lut = crate::render::cached_bind_group(&cache, &render_device,
            "multiscattering_lut_bind_group",
            &pipeline_cache.get_bind_group_layout(&layouts.multiscattering_lut),
            &BindGroupEntries::with_indices((
                // uniforms
                (0, atmosphere_binding.clone()),
                (1, settings_binding.clone()),
                // scattering medium luts and sampler
                (5, &gpu_medium.density_lut_view),
                (6, &gpu_medium.scattering_lut_view),
                (7, medium_sampler.sampler()),
                // atmosphere luts and sampler
                (8, &textures.transmittance_lut.default_view),
                (12, &**atmosphere_sampler),
                // multiscattering lut storage texture
                (13, &textures.multiscattering_lut.default_view),
            )),
        );

        let sky_view_lut = crate::render::cached_bind_group(&cache, &render_device,
            "sky_view_lut_bind_group",
            &pipeline_cache.get_bind_group_layout(&layouts.sky_view_lut),
            &BindGroupEntries::with_indices((
                // uniforms
                (0, atmosphere_binding.clone()),
                (1, settings_binding.clone()),
                (2, transforms_binding.clone()),
                (3, view_binding.clone()),
                (4, lights_binding.clone()),
                // scattering medium luts and sampler
                (5, &gpu_medium.density_lut_view),
                (6, &gpu_medium.scattering_lut_view),
                (7, medium_sampler.sampler()),
                // atmosphere luts and sampler
                (8, &textures.transmittance_lut.default_view),
                (9, &textures.multiscattering_lut.default_view),
                (12, &**atmosphere_sampler),
                // sky view lut storage texture
                (13, &textures.sky_view_lut.default_view),
            )),
        );

        let aerial_view_lut = crate::render::cached_bind_group(&cache, &render_device,
            "sky_view_lut_bind_group",
            &pipeline_cache.get_bind_group_layout(&layouts.aerial_view_lut),
            &BindGroupEntries::with_indices((
                // uniforms
                (0, atmosphere_binding.clone()),
                (1, settings_binding.clone()),
                (3, view_binding.clone()),
                (4, lights_binding.clone()),
                // scattering medium luts and sampler
                (5, &gpu_medium.density_lut_view),
                (6, &gpu_medium.scattering_lut_view),
                (7, medium_sampler.sampler()),
                // atmosphere luts and sampler
                (8, &textures.transmittance_lut.default_view),
                (9, &textures.multiscattering_lut.default_view),
                (12, &**atmosphere_sampler),
                // aerial view lut storage texture
                (13, &textures.aerial_view_lut.default_view),
            )),
        );

        let render_sky = crate::render::cached_bind_group(&cache, &render_device,
            "render_sky_bind_group",
            &pipeline_cache.get_bind_group_layout(if *msaa == Msaa::Off {
                &render_sky_layouts.render_sky
            } else {
                &render_sky_layouts.render_sky_msaa
            }),
            &BindGroupEntries::with_indices((
                // uniforms
                (0, atmosphere_binding.clone()),
                (1, settings_binding.clone()),
                (2, transforms_binding.clone()),
                (3, view_binding.clone()),
                (4, lights_binding.clone()),
                // scattering medium luts and sampler
                (5, &gpu_medium.density_lut_view),
                (6, &gpu_medium.scattering_lut_view),
                (7, medium_sampler.sampler()),
                // atmosphere luts and sampler
                (8, &textures.transmittance_lut.default_view),
                (9, &textures.multiscattering_lut.default_view),
                (10, &textures.sky_view_lut.default_view),
                (11, &textures.aerial_view_lut.default_view),
                (12, &**atmosphere_sampler),
                // view depth texture
                (13, view_depth_texture.view()),
            )),
        );

        commands.entity(entity).insert(AtmosphereBindGroups {
            transmittance_lut,
            multiscattering_lut,
            sky_view_lut,
            aerial_view_lut,
            render_sky,
        });
    }

    Ok(())
    })();
    *bind_group_cache = cache.into_inner();
    bind_group_cache.end_frame();
    result
}

pub fn init_atmosphere_buffer(mut commands: Commands) {
    commands.insert_resource(AtmosphereBuffer {
        buffer: StorageBuffer::from(GpuAtmosphere {
            ground_albedo: Vec3::ZERO,
            inner_radius: 0.0,
            outer_radius: 0.0,
            world_to_atmosphere: Mat4::IDENTITY,
        }),
    });
}

#[derive(Resource)]
pub struct AtmosphereBuffer {
    pub(crate) buffer: StorageBuffer<GpuAtmosphere>,
}

pub(crate) fn write_atmosphere_buffer(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    atmosphere_entity: Query<&GpuAtmosphere, With<Camera3d>>,
    mut atmosphere_buffer: ResMut<AtmosphereBuffer>,
) {
    // FH1 patch 4 (FH1_PATCHES.md): first camera's copy, not `single()` (fails with a second atmosphere camera).
    let Some(atmosphere) = atmosphere_entity.iter().next() else {
        return;
    };

    atmosphere_buffer.buffer.set(atmosphere.clone());
    atmosphere_buffer.buffer.write_buffer(&device, &queue);
}
