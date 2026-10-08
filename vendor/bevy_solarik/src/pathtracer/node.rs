use super::{Pathtracer, prepare::PathtracerAccumulationTexture};
use crate::scene::RaytracingSceneBindings;
use bevy_asset::{AssetServer, load_embedded_asset};
use bevy_ecs::{prelude::*, resource::Resource, system::Commands};
use bevy_render::{
    camera::ExtractedCamera,
    render_resource::{
        BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
        CachedComputePipelineId, ComputePassDescriptor, ComputePipelineDescriptor,
        ImageSubresourceRange, PipelineCache, ShaderStages, StorageTextureAccess, TextureFormat,
        binding_types::{texture_storage_2d, uniform_buffer},
    },
    renderer::{RenderContext, RenderDevice, ViewQuery},
    view::{ViewTarget, ViewUniform, ViewUniformOffset, ViewUniforms},
};
use bevy_utils::default;

/// Resource holding the pathtracer pipeline configuration.
#[derive(Resource)]
pub struct PathtracerPipelines {
    bind_group_layout: BindGroupLayoutDescriptor,
    pipeline: CachedComputePipelineId,
}

/// Initializes the pathtracer pipelines at render startup.
pub fn init_pathtracer_pipelines(
    mut commands: Commands,
    pipeline_cache: Res<PipelineCache>,
    scene_bindings: Res<RaytracingSceneBindings>,
    asset_server: Res<AssetServer>,
) {
    let bind_group_layout = BindGroupLayoutDescriptor::new(
        "pathtracer_bind_group_layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                texture_storage_2d(TextureFormat::Rgba32Float, StorageTextureAccess::ReadWrite),
                texture_storage_2d(TextureFormat::Rgba16Float, StorageTextureAccess::WriteOnly),
                uniform_buffer::<ViewUniform>(true),
            ),
        ),
    );

    let pipeline = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("pathtracer_pipeline".into()),
        layout: vec![
            scene_bindings.bind_group_layout.clone(),
            bind_group_layout.clone(),
        ],
        shader: load_embedded_asset!(asset_server.as_ref(), "pathtracer.wgsl"),
        shader_defs: if std::env::var("SOLARIK_PATHTRACER_DEBUG_SAMPLE_COUNT").is_ok() {
            vec!["PATHTRACER_DEBUG_SAMPLE_COUNT".into()]
        } else {
            vec![]
        },
        ..default()
    });

    commands.insert_resource(PathtracerPipelines {
        bind_group_layout,
        pipeline,
    });
}

pub fn pathtracer(
    view: ViewQuery<(
        &Pathtracer,
        &PathtracerAccumulationTexture,
        &ExtractedCamera,
        &ViewTarget,
        &ViewUniformOffset,
    )>,
    pathtracer_pipelines: Option<Res<PathtracerPipelines>>,
    pipeline_cache: Res<PipelineCache>,
    scene_bindings: Res<RaytracingSceneBindings>,
    view_uniforms: Res<ViewUniforms>,
    render_device: Res<RenderDevice>,
    mut reset_stats: Local<(u32, u32)>,
    mut ctx: RenderContext,
) {
    let (pathtracer_settings, accumulation_texture, camera, view_target, view_uniform_offset) =
        view.into_inner();

    let Some(pathtracer_pipelines) = pathtracer_pipelines else {
        return;
    };

    // Accumulation diagnostics: how often the history was dropped (a camera
    // that holds still should show 0; `SOLARIK_PATHTRACER_DEBUG_SAMPLE_COUNT`
    // in the environment at startup draws the per-pixel sample count instead
    // of the image, white at 512).
    reset_stats.0 += 1;
    reset_stats.1 += u32::from(pathtracer_settings.reset);
    if reset_stats.0.is_multiple_of(100) {
        tracing::debug!(
            "pathtracer: {} of the last 100 frames reset the accumulation",
            reset_stats.1
        );
        reset_stats.1 = 0;
    }

    let (Some(pipeline), Some(scene_bind_group), Some(viewport), Some(view_uniforms_binding)) = (
        pipeline_cache.get_compute_pipeline(pathtracer_pipelines.pipeline),
        &scene_bindings.bind_group,
        camera.physical_viewport_size,
        view_uniforms.uniforms.binding(),
    ) else {
        return;
    };

    let bind_group = render_device.create_bind_group(
        "pathtracer_bind_group",
        &pipeline_cache.get_bind_group_layout(&pathtracer_pipelines.bind_group_layout),
        &BindGroupEntries::sequential((
            &accumulation_texture.0.default_view,
            view_target.get_unsampled_color_attachment().view,
            view_uniforms_binding,
        )),
    );

    let command_encoder = ctx.command_encoder();

    if pathtracer_settings.reset {
        command_encoder.clear_texture(
            &accumulation_texture.0.texture,
            &ImageSubresourceRange::default(),
        );
    }

    let mut pass = command_encoder.begin_compute_pass(&ComputePassDescriptor {
        label: Some("pathtracer"),
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, scene_bind_group, &[]);
    pass.set_bind_group(1, &bind_group, &[view_uniform_offset.offset]);
    pass.dispatch_workgroups(viewport.x.div_ceil(8), viewport.y.div_ceil(8), 1);
}
