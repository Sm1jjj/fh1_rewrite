use super::{
    RaytracingMesh3d,
    blas::BlasManager,
    extract::StandardMaterialAssets,
    light_sampling::{build_alias_table, local_flux, luminance},
};
use bevy_asset::{AssetId, Handle};
use bevy_color::{ColorToComponents, LinearRgba};
use bevy_ecs::{
    entity::{Entity, EntityHashMap},
    resource::Resource,
    system::{Query, Res, ResMut},
};
use bevy_image::Image;
use bevy_material::AlphaMode;
use bevy_math::{Mat4, Vec3, ops::cos};
use bevy_mesh::Mesh;
use bevy_pbr::{
    DfgLut, ExtractedDirectionalLight, ExtractedPointLight, MeshMaterial3d,
    PreviousGlobalTransform, StandardMaterial,
};
use bevy_platform::{
    collections::{HashMap, HashSet},
    hash::FixedHasher,
};
use bevy_render::{
    extract_resource::ExtractResource,
    mesh::allocator::MeshAllocator,
    render_asset::RenderAssets,
    render_resource::{binding_types::*, *},
    renderer::{RenderDevice, RenderQueue},
    texture::{FallbackImage, GpuImage},
};
use bevy_transform::components::GlobalTransform;
use core::{
    f32::consts::{PI, TAU},
    hash::Hash,
    num::NonZeroU32,
    ops::Deref,
};

const MAX_MESH_SLAB_COUNT: NonZeroU32 = NonZeroU32::new(500).unwrap();
const MAX_TEXTURE_COUNT: NonZeroU32 = NonZeroU32::new(5_000).unwrap();

const TEXTURE_MAP_NONE: u32 = u32::MAX;
const LIGHT_NOT_PRESENT_THIS_FRAME: u32 = u32::MAX;

/// The sky: a cubemap image (the one a `Skybox` or an `EnvironmentMapLight`
/// would show, in the scene's radiance units) scaled by `intensity`. Rays
/// that leave the scene see it: the `ReSTIR` GI first bounce, the world
/// cache's GI rays, the specular GI paths and the pathtracer. No image, or an
/// intensity of zero, means no sky (upstream behaviour: escaped rays are
/// black).
#[derive(Resource, ExtractResource, Clone)]
pub struct SolarikSkyLight {
    /// A cubemap (`TextureViewDimension::Cube`) in the scene's radiance units.
    pub image: Option<Handle<Image>>,
    /// Multiplier on the cubemap's radiance.
    pub intensity: f32,
}

impl Default for SolarikSkyLight {
    fn default() -> Self {
        Self {
            image: None,
            intensity: 1.0,
        }
    }
}

/// Whether alpha-masked and blended materials get alpha-tested acceleration
/// structures (default) or are treated as opaque quads the way upstream
/// does. Off is the escape hatch for a scene where the any-hit cost is too
/// high.
#[derive(Resource, ExtractResource, Clone, Copy, Debug)]
pub struct SolarikAlphaTesting(pub bool);

impl Default for SolarikAlphaTesting {
    fn default() -> Self {
        Self(true)
    }
}

/// The intensity the shaders see: the sky's own when its image is bound,
/// zero when the fallback cubemap stands in for a missing or unloaded image
/// (the fallback is white, and a white sky is not what "no sky" means).
fn sky_shader_intensity(intensity: f32, image_bound: bool) -> f32 {
    if image_bound { intensity.max(0.0) } else { 0.0 }
}

#[derive(Resource)]
pub struct RaytracingSceneBindings {
    pub bind_group: Option<BindGroup>,
    pub bind_group_layout: BindGroupLayoutDescriptor,
    previous_frame_light_entities: Vec<Entity>,
}

pub fn prepare_raytracing_scene_bindings(
    instances_query: Query<(
        Entity,
        &RaytracingMesh3d,
        &MeshMaterial3d<StandardMaterial>,
        &GlobalTransform,
        Option<&PreviousGlobalTransform>,
    )>,
    directional_lights_query: Query<(Entity, &ExtractedDirectionalLight)>,
    // Point and spot lights: bevy_pbr extracts both into ExtractedPointLight
    // (spot_light_angles tells them apart).
    local_lights_query: Query<(Entity, &ExtractedPointLight)>,
    mesh_allocator: Res<MeshAllocator>,
    mut blas_manager: ResMut<BlasManager>,
    material_assets: Res<StandardMaterialAssets>,
    texture_assets: Res<RenderAssets<GpuImage>>,
    fallback_texture: Res<FallbackImage>,
    dfg_lut: Res<DfgLut>,
    sky_light: Res<SolarikSkyLight>,
    alpha_testing: Res<SolarikAlphaTesting>,
    render_device: Res<RenderDevice>,
    pipeline_cache: Res<PipelineCache>,
    render_queue: Res<RenderQueue>,
    mut raytracing_scene_bindings: ResMut<RaytracingSceneBindings>,
) {
    raytracing_scene_bindings.bind_group = None;

    let mut this_frame_entity_to_light_id = EntityHashMap::<u32>::default();
    let previous_frame_light_entities: Vec<_> = raytracing_scene_bindings
        .previous_frame_light_entities
        .drain(..)
        .collect();

    if instances_query.iter().len() == 0 {
        return;
    }

    // Meshes with an alpha-masked or blended material on any instance need a
    // BLAS the shader can alpha-test; the manager rebuilds the ones built the
    // other way (they are missing from the TLAS for the frame it takes).
    let non_opaque_meshes: HashSet<AssetId<Mesh>> = instances_query
        .iter()
        .filter(|(_, _, material, _, _)| {
            alpha_testing.0
                && material_assets.get(&material.id()).is_some_and(|material| {
                    material_alpha(material).flags & MATERIAL_FLAG_OPAQUE == 0
                })
        })
        .map(|(_, mesh, _, _, _)| mesh.id())
        .collect();
    blas_manager.set_non_opaque_meshes(non_opaque_meshes);

    let mut vertex_buffers = CachedBindingArray::new();
    let mut index_buffers = CachedBindingArray::new();
    let mut textures = CachedBindingArray::new();
    let mut samplers = Vec::new();
    let mut materials = StorageBufferList::<GpuMaterial>::default();
    let mut tlas = render_device
        .wgpu_device()
        .create_tlas(&CreateTlasDescriptor {
            label: Some("tlas"),
            flags: AccelerationStructureFlags::PREFER_FAST_TRACE,
            update_mode: AccelerationStructureUpdateMode::Build,
            max_instances: instances_query.iter().len() as u32,
        });
    let mut transforms = StorageBufferList::<Mat4>::default();
    let mut previous_frame_transforms = StorageBufferList::<Mat4>::default();
    let mut geometry_ids = StorageBufferList::<GpuInstanceGeometryIds>::default();
    let mut material_ids = StorageBufferList::<u32>::default();
    let mut light_sources = StorageBufferList::<GpuLightSource>::default();
    let mut light_fluxes = Vec::new();
    let mut directional_lights = StorageBufferList::<GpuDirectionalLight>::default();
    let mut local_lights = StorageBufferList::<GpuLocalLight>::default();
    let mut previous_frame_light_id_translations = StorageBufferList::<u32>::default();

    let mut material_id_map: HashMap<AssetId<StandardMaterial>, u32, FixedHasher> =
        HashMap::default();
    let mut material_id = 0;
    let mut process_texture = |texture_handle: &Option<Handle<_>>| -> Option<u32> {
        match texture_handle {
            Some(texture_handle) => match texture_assets.get(texture_handle.id()) {
                Some(texture) => {
                    let (texture_id, is_new) =
                        textures.push_if_absent(texture.texture_view.deref(), texture_handle.id());
                    if is_new {
                        samplers.push(texture.sampler.deref());
                    }
                    Some(texture_id)
                }
                None => None,
            },
            None => Some(TEXTURE_MAP_NONE),
        }
    };
    for (asset_id, material) in material_assets.iter() {
        let Some(base_color_texture_id) = process_texture(&material.base_color_texture) else {
            continue;
        };
        let Some(normal_map_texture_id) = process_texture(&material.normal_map_texture) else {
            continue;
        };
        let Some(emissive_texture_id) = process_texture(&material.emissive_texture) else {
            continue;
        };
        let Some(metallic_roughness_texture_id) =
            process_texture(&material.metallic_roughness_texture)
        else {
            continue;
        };

        let alpha = material_alpha(material);
        materials.get_mut().push(GpuMaterial {
            normal_map_texture_id,
            base_color_texture_id,
            emissive_texture_id,
            metallic_roughness_texture_id,

            base_color: LinearRgba::from(material.base_color).to_vec3(),
            perceptual_roughness: material.perceptual_roughness,
            emissive: material.emissive.to_vec3(),
            metallic: material.metallic,
            alpha_cutoff: alpha.cutoff,
            flags: alpha.flags,
            base_color_alpha: LinearRgba::from(material.base_color).alpha,
            reflectance: material.reflectance,
        });

        material_id_map.insert(*asset_id, material_id);
        material_id += 1;
    }

    if material_id == 0 {
        return;
    }

    if textures.is_empty() {
        textures.vec.push(fallback_texture.d2.texture_view.deref());
        samplers.push(fallback_texture.d2.sampler.deref());
    }

    let mut instance_id = 0;
    for (entity, mesh, material, transform, previous_frame_transform) in &instances_query {
        let Some(blas) = blas_manager.get(&mesh.id()) else {
            continue;
        };
        let Some(vertex_slice) = mesh_allocator.mesh_vertex_slice(&mesh.id()) else {
            continue;
        };
        let Some(index_slice) = mesh_allocator.mesh_index_slice(&mesh.id()) else {
            continue;
        };
        let Some(material_id) = material_id_map.get(&material.id()).copied() else {
            continue;
        };
        let Some(material) = materials.get().get(material_id as usize) else {
            continue;
        };

        let transform = transform.to_matrix();
        *tlas.get_mut_single(instance_id).unwrap() = Some(TlasInstance::new(
            blas,
            tlas_transform(&transform),
            Default::default(),
            0xFF,
        ));

        transforms.get_mut().push(transform);
        previous_frame_transforms.get_mut().push(
            previous_frame_transform
                .map(|t| Mat4::from(t.0))
                .unwrap_or(transform),
        );

        let (vertex_buffer_id, _) = vertex_buffers.push_if_absent(
            vertex_slice.buffer.as_entire_buffer_binding(),
            vertex_slice.buffer.id(),
        );
        let (index_buffer_id, _) = index_buffers.push_if_absent(
            index_slice.buffer.as_entire_buffer_binding(),
            index_slice.buffer.id(),
        );

        geometry_ids.get_mut().push(GpuInstanceGeometryIds {
            vertex_buffer_id,
            vertex_buffer_offset: vertex_slice.range.start,
            index_buffer_id,
            index_buffer_offset: index_slice.range.start,
            triangle_count: (index_slice.range.len() / 3) as u32,
            light_probability: 0.0,
        });

        material_ids.get_mut().push(material_id);

        if material.emissive != Vec3::ZERO {
            // Texture-average emission is not available on the CPU. The
            // material factor is a power estimate; PDFs remain exact.
            light_fluxes.push(
                luminance(material.emissive)
                    * blas_manager.mesh_world_area(&mesh.id(), transform)
                    * f64::from(PI),
            );
            light_sources
                .get_mut()
                .push(GpuLightSource::new_emissive_mesh_light(
                    instance_id as u32,
                    (index_slice.range.len() / 3) as u32,
                ));

            this_frame_entity_to_light_id.insert(entity, light_sources.get().len() as u32 - 1);
            raytracing_scene_bindings
                .previous_frame_light_entities
                .push(entity);
        }

        instance_id += 1;
    }

    if instance_id == 0 {
        return;
    }

    for (entity, directional_light) in &directional_lights_query {
        let directional_lights = directional_lights.get_mut();
        let directional_light_id = directional_lights.len() as u32;

        directional_lights.push(GpuDirectionalLight::new(directional_light));
        // A nominal 1 m² collection area puts lux in the same power scale
        // as local lumens. This changes variance, never emitted radiance.
        light_fluxes.push(
            luminance(directional_light.color.to_vec3())
                * f64::from(directional_light.illuminance.max(0.0)),
        );

        light_sources
            .get_mut()
            .push(GpuLightSource::new_directional_light(directional_light_id));

        this_frame_entity_to_light_id.insert(entity, light_sources.get().len() as u32 - 1);
        raytracing_scene_bindings
            .previous_frame_light_entities
            .push(entity);
    }

    // Point and spot lights are sphere lights of the light's radius, in the
    // raster path's candela; a light that cannot light anything (no power)
    // is left out so it costs no candidates.
    for (entity, local_light) in &local_lights_query {
        if local_light.intensity <= 0.0 || local_light.color == LinearRgba::BLACK {
            continue;
        }
        let local_lights = local_lights.get_mut();
        let local_light_id = local_lights.len() as u32;

        local_lights.push(GpuLocalLight::new(local_light));
        light_fluxes.push(local_flux(
            local_light.color.to_vec3(),
            local_light.intensity,
            local_light.spot_light_angles,
        ));

        light_sources
            .get_mut()
            .push(if local_light.spot_light_angles.is_some() {
                GpuLightSource::new_spot_light(local_light_id)
            } else {
                GpuLightSource::new_point_light(local_light_id)
            });

        this_frame_entity_to_light_id.insert(entity, light_sources.get().len() as u32 - 1);
        raytracing_scene_bindings
            .previous_frame_light_entities
            .push(entity);
    }

    for previous_frame_light_entity in previous_frame_light_entities {
        let current_frame_index = this_frame_entity_to_light_id
            .get(&previous_frame_light_entity)
            .copied()
            .unwrap_or(LIGHT_NOT_PRESENT_THIS_FRAME);
        previous_frame_light_id_translations
            .get_mut()
            .push(current_frame_index);
    }

    if light_sources.get().len() > u16::MAX as usize {
        panic!("Too many light sources in the scene, maximum is 65535.");
    }

    for (source, bin) in light_sources
        .get_mut()
        .iter_mut()
        .zip(build_alias_table(&light_fluxes))
    {
        source.selection_probability = bin.probability;
        source.alias_threshold = bin.threshold;
        source.alias_index = bin.alias;
        if source.kind & 1 == 0 {
            geometry_ids.get_mut()[source.id as usize].light_probability = bin.probability;
        }
    }

    materials.write_buffer(&render_device, &render_queue);
    transforms.write_buffer(&render_device, &render_queue);
    previous_frame_transforms.write_buffer(&render_device, &render_queue);
    geometry_ids.write_buffer(&render_device, &render_queue);
    material_ids.write_buffer(&render_device, &render_queue);
    light_sources.write_buffer(&render_device, &render_queue);
    directional_lights.write_buffer(&render_device, &render_queue);
    local_lights.write_buffer(&render_device, &render_queue);
    previous_frame_light_id_translations.write_buffer(&render_device, &render_queue);

    let mut command_encoder = render_device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("build_tlas_command_encoder"),
    });
    command_encoder.build_acceleration_structures(&[], [&tlas]);
    render_queue.submit([command_encoder.finish()]);

    let (dfg_view, dfg_sampler) = texture_assets
        .get(&dfg_lut.texture)
        .map(|img| (&img.texture_view, &img.sampler))
        .unwrap_or((
            &fallback_texture.d2.texture_view,
            &fallback_texture.d2.sampler,
        ));

    let sky_image = sky_light
        .image
        .as_ref()
        .and_then(|image| texture_assets.get(image.id()));
    let (sky_view, sky_sampler) = sky_image
        .map(|image| (&image.texture_view, &image.sampler))
        .unwrap_or((
            &fallback_texture.cube.texture_view,
            &fallback_texture.cube.sampler,
        ));
    // A storage buffer: wgpu refuses a uniform buffer in a bind group that
    // also holds binding arrays (the textures and mesh slabs above).
    let mut sky_buffer = StorageBuffer::from(GpuSkyLight {
        intensity: sky_shader_intensity(sky_light.intensity, sky_image.is_some()),
        _padding: Vec3::ZERO,
    });
    sky_buffer.write_buffer(&render_device, &render_queue);

    raytracing_scene_bindings.bind_group = Some(render_device.create_bind_group(
        "raytracing_scene_bind_group",
        &pipeline_cache.get_bind_group_layout(&raytracing_scene_bindings.bind_group_layout),
        &BindGroupEntries::sequential((
            vertex_buffers.as_slice(),
            index_buffers.as_slice(),
            textures.as_slice(),
            samplers.as_slice(),
            materials.binding().unwrap(),
            tlas.as_binding(),
            transforms.binding().unwrap(),
            previous_frame_transforms.binding().unwrap(),
            geometry_ids.binding().unwrap(),
            material_ids.binding().unwrap(),
            light_sources.binding().unwrap(),
            directional_lights.binding().unwrap(),
            local_lights.binding().unwrap(),
            previous_frame_light_id_translations.binding().unwrap(),
            dfg_view,
            dfg_sampler,
            sky_view,
            sky_sampler,
            sky_buffer.binding().unwrap(),
        )),
    ));
}

impl RaytracingSceneBindings {
    pub fn new() -> Self {
        Self {
            bind_group: None,
            bind_group_layout: BindGroupLayoutDescriptor::new(
                "raytracing_scene_bind_group_layout",
                &BindGroupLayoutEntries::sequential(
                    ShaderStages::COMPUTE,
                    (
                        storage_buffer_read_only_sized(false, None).count(MAX_MESH_SLAB_COUNT),
                        storage_buffer_read_only_sized(false, None).count(MAX_MESH_SLAB_COUNT),
                        texture_2d(TextureSampleType::Float { filterable: true })
                            .count(MAX_TEXTURE_COUNT),
                        sampler(SamplerBindingType::Filtering).count(MAX_TEXTURE_COUNT),
                        storage_buffer_read_only_sized(false, None),
                        acceleration_structure(),
                        storage_buffer_read_only_sized(false, None),
                        storage_buffer_read_only_sized(false, None),
                        storage_buffer_read_only_sized(false, None),
                        storage_buffer_read_only_sized(false, None),
                        storage_buffer_read_only_sized(false, None),
                        storage_buffer_read_only_sized(false, None),
                        storage_buffer_read_only_sized(false, None),
                        storage_buffer_read_only_sized(false, None),
                        texture_2d(TextureSampleType::Float { filterable: true }),
                        sampler(SamplerBindingType::Filtering),
                        texture_cube(TextureSampleType::Float { filterable: true }),
                        sampler(SamplerBindingType::Filtering),
                        storage_buffer_read_only::<GpuSkyLight>(false),
                    ),
                ),
            ),
            previous_frame_light_entities: Vec::new(),
        }
    }
}

impl Default for RaytracingSceneBindings {
    fn default() -> Self {
        Self::new()
    }
}

struct CachedBindingArray<T, I: Eq + Hash> {
    map: HashMap<I, u32>,
    vec: Vec<T>,
}

impl<T, I: Eq + Hash> CachedBindingArray<T, I> {
    fn new() -> Self {
        Self {
            map: HashMap::default(),
            vec: Vec::default(),
        }
    }

    fn push_if_absent(&mut self, item: T, item_id: I) -> (u32, bool) {
        let mut is_new = false;
        let i = *self.map.entry(item_id).or_insert_with(|| {
            is_new = true;
            let i = self.vec.len() as u32;
            self.vec.push(item);
            i
        });
        (i, is_new)
    }

    fn is_empty(&self) -> bool {
        self.vec.is_empty()
    }

    fn as_slice(&self) -> &[T] {
        self.vec.as_slice()
    }
}

type StorageBufferList<T> = StorageBuffer<Vec<T>>;

#[derive(ShaderType)]
struct GpuInstanceGeometryIds {
    vertex_buffer_id: u32,
    vertex_buffer_offset: u32,
    index_buffer_id: u32,
    index_buffer_offset: u32,
    triangle_count: u32,
    light_probability: f32,
}

#[derive(ShaderType)]
struct GpuMaterial {
    normal_map_texture_id: u32,
    base_color_texture_id: u32,
    emissive_texture_id: u32,
    metallic_roughness_texture_id: u32,

    base_color: Vec3,
    perceptual_roughness: f32,
    emissive: Vec3,
    metallic: f32,
    // Upstream's vec3 padding, spent on the alpha handling the rays need.
    alpha_cutoff: f32,
    flags: u32,
    base_color_alpha: f32,
    reflectance: f32,
}

/// `Material.flags` bits, mirrored in `raytracing_scene_bindings.wgsl`.
const MATERIAL_FLAG_OPAQUE: u32 = 1;
const MATERIAL_FLAG_ALPHA_MASK: u32 = 2;
const MATERIAL_FLAG_ALPHA_BLEND: u32 = 4;
const MATERIAL_FLAG_DOUBLE_SIDED: u32 = 8;

/// How the rays treat a material's alpha: exactly one of `OPAQUE`,
/// `ALPHA_MASK` (test the base colour alpha against `cutoff`) or
/// `ALPHA_BLEND` (never a hit, glass is transparent to light), plus
/// `DOUBLE_SIDED`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MaterialAlpha {
    flags: u32,
    cutoff: f32,
}

fn material_alpha(material: &StandardMaterial) -> MaterialAlpha {
    let (mode, cutoff) = match material.alpha_mode {
        AlphaMode::Opaque => (MATERIAL_FLAG_OPAQUE, 0.0),
        AlphaMode::Mask(cutoff) => (MATERIAL_FLAG_ALPHA_MASK, cutoff),
        AlphaMode::AlphaToCoverage => (MATERIAL_FLAG_ALPHA_MASK, 0.5),
        AlphaMode::Blend | AlphaMode::Premultiplied | AlphaMode::Add | AlphaMode::Multiply => {
            (MATERIAL_FLAG_ALPHA_BLEND, 0.0)
        }
    };
    let double_sided = if material.double_sided {
        MATERIAL_FLAG_DOUBLE_SIDED
    } else {
        0
    };
    MaterialAlpha {
        flags: mode | double_sided,
        cutoff,
    }
}

#[derive(ShaderType, Default)]
struct GpuLightSource {
    kind: u32,
    id: u32,
    selection_probability: f32,
    alias_threshold: f32,
    alias_index: u32,
}

impl GpuLightSource {
    fn new_emissive_mesh_light(instance_id: u32, triangle_count: u32) -> GpuLightSource {
        if triangle_count > u16::MAX as u32 {
            panic!("Too many triangles ({triangle_count}) in an emissive mesh, maximum is 65535.");
        }

        Self {
            kind: triangle_count << 1,
            id: instance_id,
            ..Default::default()
        }
    }

    fn new_directional_light(directional_light_id: u32) -> GpuLightSource {
        Self {
            kind: LIGHT_SOURCE_KIND_DIRECTIONAL,
            id: directional_light_id,
            ..Default::default()
        }
    }

    fn new_point_light(local_light_id: u32) -> GpuLightSource {
        Self {
            kind: LIGHT_SOURCE_KIND_POINT,
            id: local_light_id,
            ..Default::default()
        }
    }

    fn new_spot_light(local_light_id: u32) -> GpuLightSource {
        Self {
            kind: LIGHT_SOURCE_KIND_SPOT,
            id: local_light_id,
            ..Default::default()
        }
    }
}

/// `LightSource.kind` values, mirrored in `raytracing_scene_bindings.wgsl`.
/// The low bit clear is an emissive mesh (upstream: the triangle count sits
/// above it); the low bit set is any other light, with the kind above it.
const LIGHT_SOURCE_KIND_DIRECTIONAL: u32 = 1;
const LIGHT_SOURCE_KIND_POINT: u32 = 3;
const LIGHT_SOURCE_KIND_SPOT: u32 = 5;

/// The smallest sphere a point or spot light is sampled as (metres). glTF
/// and Bevy's defaults give lights a radius of 0, and the sampler needs an
/// area; at a centimetre the light is a point to anything it can light.
const LOCAL_LIGHT_MIN_RADIUS: f32 = 0.01;

/// A point or spot light as a sphere light. Mirrors `LocalLight` in
/// `raytracing_scene_bindings.wgsl`.
#[derive(ShaderType, Default, Debug, Clone, Copy, PartialEq)]
struct GpuLocalLight {
    position: Vec3,
    radius: f32,
    /// Radiance of the sphere's surface: the light's intensity (candela, as
    /// `bevy_pbr` extracts it) over π r², so the sphere's intensity in every
    /// direction is the light's.
    radiance: Vec3,
    /// The sphere's area, 4π r²: the inverse pdf of a uniform area sample.
    inverse_pdf: f32,
    /// Spot cone axis (the transform's forward), unused for point lights.
    direction: Vec3,
    /// The raster path's cut-off; its smooth window is applied so the two
    /// paths agree by construction.
    range: f32,
    /// Filament's cone: cos of the outer and inner angles. -1 for both means
    /// no cone (a point light).
    cos_outer: f32,
    cos_inner: f32,
    _padding: bevy_math::Vec2,
}

impl GpuLocalLight {
    fn new(light: &ExtractedPointLight) -> Self {
        let radius = light.radius.max(LOCAL_LIGHT_MIN_RADIUS);
        let area = 4.0 * PI * radius * radius;
        let radiance = light.color.to_vec3() * (light.intensity / (PI * radius * radius));
        let (cos_inner, cos_outer) = match light.spot_light_angles {
            Some((inner, outer)) => (cos(inner), cos(outer)),
            None => (-1.0, -1.0),
        };
        Self {
            position: light.transform.translation(),
            radius,
            radiance,
            inverse_pdf: area,
            direction: light.transform.forward().into(),
            range: light.range,
            cos_outer,
            cos_inner,
            _padding: bevy_math::Vec2::ZERO,
        }
    }
}

#[derive(ShaderType, Default)]
struct GpuDirectionalLight {
    direction_to_light: Vec3,
    cos_theta_max: f32,
    luminance: Vec3,
    inverse_pdf: f32,
}

impl GpuDirectionalLight {
    fn new(directional_light: &ExtractedDirectionalLight) -> Self {
        let cos_theta_max = cos(directional_light.sun_disk_angular_size / 2.0);
        let solid_angle = TAU * (1.0 - cos_theta_max);
        let luminance =
            (directional_light.color.to_vec3() * directional_light.illuminance) / solid_angle;

        Self {
            direction_to_light: directional_light.transform.back().into(),
            cos_theta_max,
            luminance,
            inverse_pdf: solid_angle,
        }
    }
}

/// Mirrors `SkyLight` in `raytracing_scene_bindings.wgsl`.
#[derive(ShaderType)]
struct GpuSkyLight {
    intensity: f32,
    _padding: Vec3,
}

fn tlas_transform(transform: &Mat4) -> [f32; 12] {
    transform.transpose().to_cols_array()[..12]
        .try_into()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_transform::components::Transform;

    #[test]
    fn sky_intensity_is_zero_without_a_bound_image() {
        assert_eq!(sky_shader_intensity(1.5, true), 1.5);
        assert_eq!(sky_shader_intensity(1.5, false), 0.0);
        assert_eq!(sky_shader_intensity(-2.0, true), 0.0, "no negative sky");
        assert_eq!(sky_shader_intensity(0.0, true), 0.0);
    }

    #[test]
    fn material_alpha_picks_one_mode_and_keeps_double_sidedness() {
        let mut material = StandardMaterial::default();
        assert_eq!(
            material_alpha(&material),
            MaterialAlpha {
                flags: MATERIAL_FLAG_OPAQUE,
                cutoff: 0.0
            }
        );

        material.alpha_mode = AlphaMode::Mask(0.35);
        material.double_sided = true;
        assert_eq!(
            material_alpha(&material),
            MaterialAlpha {
                flags: MATERIAL_FLAG_ALPHA_MASK | MATERIAL_FLAG_DOUBLE_SIDED,
                cutoff: 0.35
            }
        );

        material.alpha_mode = AlphaMode::AlphaToCoverage;
        assert_eq!(material_alpha(&material).cutoff, 0.5);

        for blended in [
            AlphaMode::Blend,
            AlphaMode::Premultiplied,
            AlphaMode::Add,
            AlphaMode::Multiply,
        ] {
            material.alpha_mode = blended;
            let alpha = material_alpha(&material);
            assert_eq!(
                alpha.flags & MATERIAL_FLAG_ALPHA_BLEND,
                MATERIAL_FLAG_ALPHA_BLEND
            );
            assert_eq!(
                alpha.flags & MATERIAL_FLAG_OPAQUE,
                0,
                "{blended:?} blocks no ray"
            );
        }
    }

    #[test]
    fn light_source_kinds_keep_the_low_bit_convention() {
        // Emissive meshes: triangle count in the upper bits, low bit clear;
        // the fork relies on this staying as upstream left it. Every other
        // light sets the low bit and picks its kind above it.
        assert_eq!(GpuLightSource::new_emissive_mesh_light(7, 12).kind, 24);
        assert_eq!(GpuLightSource::new_directional_light(0).kind, 1);
        assert_eq!(GpuLightSource::new_point_light(3).kind, 3);
        assert_eq!(GpuLightSource::new_point_light(3).id, 3);
        assert_eq!(GpuLightSource::new_spot_light(4).kind, 5);
    }

    fn extracted_light(lumens: f32, radius: f32, spot: Option<(f32, f32)>) -> ExtractedPointLight {
        ExtractedPointLight {
            color: LinearRgba::WHITE,
            // What bevy_pbr's extract_lights stores: candela.
            intensity: lumens / (4.0 * PI),
            range: 20.0,
            radius,
            transform: GlobalTransform::from(
                Transform::from_xyz(1.0, 2.0, 3.0).looking_to(Vec3::NEG_Y, Vec3::X),
            ),
            shadow_maps_enabled: false,
            contact_shadows_enabled: false,
            shadow_depth_bias: 0.0,
            shadow_normal_bias: 0.0,
            shadow_map_near_z: 0.1,
            spot_light_angles: spot,
            volumetric: false,
            soft_shadows_enabled: false,
            affects_lightmapped_mesh_diffuse: true,
        }
    }

    #[test]
    fn point_light_photometry_matches_the_raster_path() {
        // An 800 lm bulb: I = 800 / 4π cd. A sphere of uniform radiance L and
        // radius r has intensity L π r², so a receiver facing it at distance d
        // sees I / d², the raster path's value. The sampler's estimator is
        // L × area × cos / d² over the whole sphere, whose expectation on the
        // facing side is L × π r² / d² (a quarter of the area sees the
        // receiver on average), so L × area / 4 must equal I.
        let light = GpuLocalLight::new(&extracted_light(800.0, 0.05, None));
        let intensity = 800.0 / (4.0 * PI);
        assert_eq!(light.radius, 0.05);
        assert!((light.radiance.x * light.inverse_pdf / 4.0 - intensity).abs() < 1e-3);
        assert_eq!(light.radiance.x, light.radiance.z, "white light");
        assert_eq!(light.position, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(light.range, 20.0);
        assert_eq!(light.cos_outer, -1.0, "a point light has no cone");
        assert_eq!(light.cos_inner, -1.0);
    }

    #[test]
    fn zero_radius_lights_get_the_floor_radius() {
        // glTF lights arrive with radius 0; the sphere must stay finite and
        // its photometry unchanged.
        let light = GpuLocalLight::new(&extracted_light(800.0, 0.0, None));
        assert_eq!(light.radius, LOCAL_LIGHT_MIN_RADIUS);
        let intensity = 800.0 / (4.0 * PI);
        assert!((light.radiance.x * light.inverse_pdf / 4.0 - intensity).abs() < 1e-3);
        assert!(light.radiance.x.is_finite());
    }

    #[test]
    fn spot_light_carries_its_cone_and_direction() {
        let inner = 0.3f32;
        let outer = 0.5f32;
        let light = GpuLocalLight::new(&extracted_light(1000.0, 0.0, Some((inner, outer))));
        assert!((light.cos_outer - outer.cos()).abs() < 1e-6);
        assert!((light.cos_inner - inner.cos()).abs() < 1e-6);
        // The cone points along the transform's forward axis, as bevy_pbr.
        assert!((light.direction - Vec3::NEG_Y).length() < 1e-5);
        // Same candela conversion as a point light (Bevy divides spot lumens
        // by 4π too, so a lit patch keeps its brightness when a point light
        // becomes a spot).
        let intensity = 1000.0 / (4.0 * PI);
        assert!((light.radiance.x * light.inverse_pdf / 4.0 - intensity).abs() < 1e-3);
    }
}
