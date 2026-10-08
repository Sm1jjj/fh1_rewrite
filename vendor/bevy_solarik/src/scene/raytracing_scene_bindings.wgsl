enable wgpu_ray_query;

#define_import_path bevy_solarik::scene_bindings

#import bevy_pbr::lighting::perceptualRoughnessToRoughness
#import bevy_pbr::pbr_functions::calculate_tbn_mikktspace

struct InstanceGeometryIds {
    vertex_buffer_id: u32,
    vertex_buffer_offset: u32,
    index_buffer_id: u32,
    index_buffer_offset: u32,
    triangle_count: u32,
    light_probability: f32,
}

struct VertexBuffer { vertices: array<PackedVertex> }

struct IndexBuffer { indices: array<u32> }

struct PackedVertex {
    a: vec4<f32>,
    b: vec4<f32>,
    tangent: vec4<f32>,
}

struct Vertex {
    position: vec3<f32>,
    normal: vec3<f32>,
    uv: vec2<f32>,
    tangent: vec4<f32>,
}

fn unpack_vertex(packed: PackedVertex) -> Vertex {
    var vertex: Vertex;
    vertex.position = packed.a.xyz;
    vertex.normal = vec3(packed.a.w, packed.b.xy);
    vertex.uv = packed.b.zw;
    vertex.tangent = packed.tangent;
    return vertex;
}

struct Material {
    normal_map_texture_id: u32,
    base_color_texture_id: u32,
    emissive_texture_id: u32,
    metallic_roughness_texture_id: u32,

    base_color: vec3<f32>,
    perceptual_roughness: f32,
    emissive: vec3<f32>,
    metallic: f32,
    // Alpha handling for the rays (upstream's padding): the mask cutoff, the
    // MATERIAL_FLAG_* bits and the base colour's alpha factor.
    alpha_cutoff: f32,
    flags: u32,
    base_color_alpha: f32,
    reflectance: f32,
}

// Mirrored in binder.rs. Exactly one of OPAQUE / ALPHA_MASK / ALPHA_BLEND.
const MATERIAL_FLAG_OPAQUE = 1u;
const MATERIAL_FLAG_ALPHA_MASK = 2u;
const MATERIAL_FLAG_ALPHA_BLEND = 4u;
const MATERIAL_FLAG_DOUBLE_SIDED = 8u;

const TEXTURE_MAP_NONE = 0xFFFFFFFFu;

const MIRROR_ROUGHNESS_THRESHOLD = 0.001f;

struct LightSource {
    // Low bit clear: an emissive mesh, its triangle count in the bits above
    // (upstream). Low bit set: another kind of light, selected by the bits
    // above. Mirrored in binder.rs.
    kind: u32,
    id: u32,
    selection_probability: f32,
    alias_threshold: f32,
    alias_index: u32,
}

const LIGHT_SOURCE_KIND_EMISSIVE_MESH = 0u;
const LIGHT_SOURCE_KIND_DIRECTIONAL = 1u;
const LIGHT_SOURCE_KIND_POINT = 3u;
const LIGHT_SOURCE_KIND_SPOT = 5u;

fn light_source_is_emissive_mesh(light_source: LightSource) -> bool {
    return (light_source.kind & 1u) == 0u;
}

// A point or spot light as a sphere light of the light's radius, in the
// raster path's units (bevy_pbr's candela). Mirrors GpuLocalLight in
// binder.rs.
struct LocalLight {
    position: vec3<f32>,
    radius: f32,
    // Radiance of the sphere's surface, intensity / (π r²).
    radiance: vec3<f32>,
    // The sphere's area, the inverse pdf of a uniform area sample.
    inverse_pdf: f32,
    // Spot cone axis (unused for point lights).
    direction: vec3<f32>,
    // The raster path's cut-off (its smooth window applies).
    range: f32,
    // cos of the outer and inner cone angles; -1 for both means no cone.
    cos_outer: f32,
    cos_inner: f32,
    _padding: vec2<f32>,
}

// The sky: a cubemap in the scene's radiance units, scaled by `intensity`
// (0 when the scene has no sky).
struct SkyLight {
    intensity: f32,
    _padding: vec3<f32>,
}

struct DirectionalLight {
    direction_to_light: vec3<f32>,
    cos_theta_max: f32,
    luminance: vec3<f32>,
    inverse_pdf: f32,
}

const LIGHT_NOT_PRESENT_THIS_FRAME = 0xFFFFFFFFu;

@group(0) @binding(0) var<storage> vertex_buffers: binding_array<VertexBuffer>;
@group(0) @binding(1) var<storage> index_buffers: binding_array<IndexBuffer>;
@group(0) @binding(2) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(3) var samplers: binding_array<sampler>;
@group(0) @binding(4) var<storage> materials: array<Material>;
@group(0) @binding(5) var tlas: acceleration_structure;
@group(0) @binding(6) var<storage> transforms: array<mat4x4<f32>>; // TODO: Use mat3x4<f32>?
@group(0) @binding(7) var<storage> previous_frame_transforms: array<mat4x4<f32>>; // TODO: Use mat3x4<f32>?
@group(0) @binding(8) var<storage> geometry_ids: array<InstanceGeometryIds>;
@group(0) @binding(9) var<storage> material_ids: array<u32>; // TODO: Store material_id in instance_custom_index instead?
@group(0) @binding(10) var<storage> light_sources: array<LightSource>;
@group(0) @binding(11) var<storage> directional_lights: array<DirectionalLight>;
@group(0) @binding(12) var<storage> local_lights: array<LocalLight>;
@group(0) @binding(13) var<storage> previous_frame_light_id_translations: array<u32>;
@group(0) @binding(14) var brdf_dfg_lut: texture_2d<f32>;
@group(0) @binding(15) var brdf_dfg_lut_sampler: sampler;
@group(0) @binding(16) var sky_texture: texture_cube<f32>;
@group(0) @binding(17) var sky_sampler: sampler;
@group(0) @binding(18) var<storage> sky_light: SkyLight; // storage: uniforms can't share a group with binding arrays

// Radiance arriving from the sky along `direction` (world space, pointing
// away from the surface), for a ray that left the scene. Black when the
// scene has no sky. Cube maps are left-handed, so z is negated the way
// Bevy's skybox and environment map shaders do it.
fn sample_sky(direction: vec3<f32>) -> vec3<f32> {
    let cube_direction = vec3(direction.xy, -direction.z);
    return textureSampleLevel(sky_texture, sky_sampler, cube_direction, 0.0).rgb * sky_light.intensity;
}

const RAY_T_MIN = 0.001f;
const RAY_T_MAX = 100000.0f;

const RAY_NO_CULL = 0xFFu;

fn trace_ray(ray_origin: vec3<f32>, ray_direction: vec3<f32>, ray_t_min: f32, ray_t_max: f32, ray_flag: u32) -> RayIntersection {
    let ray = RayDesc(ray_flag, RAY_NO_CULL, ray_t_min, ray_t_max, ray_origin, ray_direction);
    var rq: ray_query;
    rayQueryInitialize(&rq, tlas, ray);
    // Opaque geometry commits in hardware and never shows up here. Meshes
    // with an alpha-masked or blended material are built non-opaque, and each
    // of their hits is a candidate the shader confirms or drops: masked cards
    // by their texture alpha, blended glass never (it is transparent to light).
    while rayQueryProceed(&rq) {
        let candidate = rayQueryGetCandidateIntersection(&rq);
        if candidate.kind == RAY_QUERY_INTERSECTION_TRIANGLE && candidate_is_solid(candidate) {
            rayQueryConfirmIntersection(&rq);
        }
    }
    return rayQueryGetCommittedIntersection(&rq);
}

// Alpha test for a candidate hit on non-opaque geometry.
fn candidate_is_solid(candidate: RayIntersection) -> bool {
    let material = materials[material_ids[candidate.instance_index]];
    if (material.flags & MATERIAL_FLAG_ALPHA_BLEND) != 0u {
        return false;
    }
    if (material.flags & MATERIAL_FLAG_ALPHA_MASK) == 0u {
        return true;
    }
    var alpha = material.base_color_alpha;
    if material.base_color_texture_id != TEXTURE_MAP_NONE {
        let barycentrics = vec3(1.0 - candidate.barycentrics.x - candidate.barycentrics.y, candidate.barycentrics);
        let vertices = load_vertices(geometry_ids[candidate.instance_index], candidate.primitive_index);
        let uv = mat3x2(vertices[0].uv, vertices[1].uv, vertices[2].uv) * barycentrics;
        alpha *= sample_texture_alpha(material.base_color_texture_id, uv);
    }
    return alpha >= material.alpha_cutoff;
}

fn sample_texture(id: u32, uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(textures[id], samplers[id], uv, 0.0).rgb; // TODO: Mipmap
}

fn sample_texture_alpha(id: u32, uv: vec2<f32>) -> f32 {
    return textureSampleLevel(textures[id], samplers[id], uv, 0.0).a;
}

struct ResolvedMaterial {
    base_color: vec3<f32>,
    emissive: vec3<f32>,
    reflectance: f32,
    perceptual_roughness: f32,
    roughness: f32,
    metallic: f32,
}

struct ResolvedRayHitFull {
    world_position: vec3<f32>,
    previous_frame_world_position: vec3<f32>,
    world_normal: vec3<f32>,
    geometric_world_normal: vec3<f32>,
    world_tangent: vec4<f32>,
    uv: vec2<f32>,
    triangle_area: f32,
    triangle_count: u32,
    light_probability: f32,
    material: ResolvedMaterial,
}

fn resolve_material(material: Material, uv: vec2<f32>) -> ResolvedMaterial {
    var m: ResolvedMaterial;

    m.base_color = material.base_color.rgb;
    if material.base_color_texture_id != TEXTURE_MAP_NONE {
        m.base_color *= sample_texture(material.base_color_texture_id, uv);
    }

    m.emissive = material.emissive.rgb;
    if material.emissive_texture_id != TEXTURE_MAP_NONE {
        m.emissive *= sample_texture(material.emissive_texture_id, uv);
    }

    m.reflectance = material.reflectance;

    m.perceptual_roughness = material.perceptual_roughness;
    m.metallic = material.metallic;
    if material.metallic_roughness_texture_id != TEXTURE_MAP_NONE {
        let metallic_roughness = sample_texture(material.metallic_roughness_texture_id, uv);
        m.perceptual_roughness *= metallic_roughness.g;
        m.metallic *= metallic_roughness.b;
    }

    m.roughness = m.perceptual_roughness * m.perceptual_roughness;

    return m;
}

fn resolve_ray_hit_full(ray_hit: RayIntersection) -> ResolvedRayHitFull {
    let barycentrics = vec3(1.0 - ray_hit.barycentrics.x - ray_hit.barycentrics.y, ray_hit.barycentrics);
    var hit = resolve_triangle_data_full(ray_hit.instance_index, ray_hit.primitive_index, barycentrics);
    // A double-sided surface hit from behind is shaded as the side the ray
    // arrived on (the rasteriser does the same for the gbuffer); a
    // single-sided back face keeps upstream's raw normal.
    let material = materials[material_ids[ray_hit.instance_index]];
    if !ray_hit.front_face && (material.flags & MATERIAL_FLAG_DOUBLE_SIDED) != 0u {
        hit.world_normal = -hit.world_normal;
        hit.geometric_world_normal = -hit.geometric_world_normal;
    }
    return hit;
}

fn load_vertices(instance_geometry_ids: InstanceGeometryIds, triangle_id: u32) -> array<Vertex, 3> {
    let index_buffer = &index_buffers[instance_geometry_ids.index_buffer_id].indices;
    let vertex_buffer = &vertex_buffers[instance_geometry_ids.vertex_buffer_id].vertices;

    let indices_i = (triangle_id * 3u) + vec3(0u, 1u, 2u) + instance_geometry_ids.index_buffer_offset;
    let indices = vec3((*index_buffer)[indices_i.x], (*index_buffer)[indices_i.y], (*index_buffer)[indices_i.z]) + instance_geometry_ids.vertex_buffer_offset;

    return array<Vertex, 3>(
        unpack_vertex((*vertex_buffer)[indices.x]),
        unpack_vertex((*vertex_buffer)[indices.y]),
        unpack_vertex((*vertex_buffer)[indices.z])
    );
}

fn transform_positions(transform: mat4x4<f32>, vertices: array<Vertex, 3>) -> array<vec3<f32>, 3> {
    return array<vec3<f32>, 3>(
        (transform * vec4(vertices[0].position, 1.0)).xyz,
        (transform * vec4(vertices[1].position, 1.0)).xyz,
        (transform * vec4(vertices[2].position, 1.0)).xyz
    );
}

fn resolve_triangle_data_full(instance_id: u32, triangle_id: u32, barycentrics: vec3<f32>) -> ResolvedRayHitFull {
    let material_id = material_ids[instance_id];
    let material = materials[material_id];

    let transform = transforms[instance_id];
    let previous_frame_transform = previous_frame_transforms[instance_id];

    let instance_geometry_ids = geometry_ids[instance_id];
    let vertices = load_vertices(instance_geometry_ids, triangle_id);

    let world_vertices = transform_positions(transform, vertices);
    let world_position = mat3x3(world_vertices[0], world_vertices[1], world_vertices[2]) * barycentrics;

    let previous_frame_world_vertices = transform_positions(previous_frame_transform, vertices);
    let previous_frame_world_position = mat3x3(previous_frame_world_vertices[0], previous_frame_world_vertices[1], previous_frame_world_vertices[2]) * barycentrics;

    let uv = mat3x2(vertices[0].uv, vertices[1].uv, vertices[2].uv) * barycentrics;

    let local_tangent = mat3x3(vertices[0].tangent.xyz, vertices[1].tangent.xyz, vertices[2].tangent.xyz) * barycentrics;
    let world_tangent = vec4(
        normalize(mat3x3(transform[0].xyz, transform[1].xyz, transform[2].xyz) * local_tangent),
        vertices[0].tangent.w,
    );

    let local_normal = mat3x3(vertices[0].normal, vertices[1].normal, vertices[2].normal) * barycentrics; // TODO: Use barycentric lerp, ray_hit.object_to_world, cross product geo normal
    var world_normal = normalize(mat3x3(transform[0].xyz, transform[1].xyz, transform[2].xyz) * local_normal);
    let geometric_world_normal = world_normal;
    if material.normal_map_texture_id != TEXTURE_MAP_NONE {
        let TBN = calculate_tbn_mikktspace(world_normal, world_tangent);
        let T = TBN[0];
        let B = TBN[1];
        let N = TBN[2];
        var Nt = sample_texture(material.normal_map_texture_id, uv) * 2.0 - 1.0;
        Nt.z = sqrt(max(1.0 - dot(Nt.xy, Nt.xy), 0.0)); // Reconstruct Z to support two-channel normal maps
        world_normal = normalize(Nt.x * T + Nt.y * B + Nt.z * N);
    }

    let triangle_edge0 = world_vertices[0] - world_vertices[1];
    let triangle_edge1 = world_vertices[0] - world_vertices[2];
    let triangle_area = length(cross(triangle_edge0, triangle_edge1)) / 2.0;

    let resolved_material = resolve_material(material, uv);

    return ResolvedRayHitFull(
        world_position,
        previous_frame_world_position,
        world_normal,
        geometric_world_normal,
        world_tangent,
        uv,
        triangle_area,
        instance_geometry_ids.triangle_count,
        instance_geometry_ids.light_probability,
        resolved_material,
    );
}
