//! Colorado's animated objects (`.pgeo` types 4/5: windmills, wind turbines, fair rides, birds, crop
//! duster, stage lights, fireworks...), drawn from the game's own meshes and animated with the
//! embedded Granny curves (`fh1_formats::granny`, docs/PROPS.md "Animated objects").
//!
//! Input: the `anim` setup group (`anim/colorado/index.json`, `objects/*.pgeo`, `textures/*.dds`).
//! Free-roam scene instances stream in within their cull distance (record float 3, UNVERIFIED reading);
//! the LOD switches at floats 1 / 2 (UNVERIFIED). Each frame, every model's bones are posed at the
//! global clock (animations loop on their duration): rigid meshes get `S * place * pose * S` as their
//! transform (S = Z mirror: mesh and Granny data are left-handed like the collision space), skinned
//! meshes (per-vertex blend indices) use Bevy skinning with one joint entity per bone.
//! Light attachments (granny::AnimLightGroup) are handed to fh1-render's `DynamicGlows` from the posed
//! bones, within 140 m of the camera (rule 0x82DF7108; the glow shader does the night switch-on).
//! `FH1_ANIM=0` turns it off, `FH1_ANIM_LIGHTS=0` only the lights. `FH1_ANIM_VIEW=<object name part>[,distance,height]` (screenshots) puts the
//! camera near the first free-roam instance of that object, looking at it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::NoFrustumCulling;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::skinning::{SkinnedMesh, SkinnedMeshInverseBindposes};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use fh1_formats::granny::{self, Mat4 as GMat4};
use fh1_render::car_material::{FxRawOutput, FxRawStandard};
use fh1_render::{FxGlobals, FxLibrary, FxMaterial};

/// Extra range kept loaded past an instance's cull distance.
const KEEP: f32 = 50.0;

pub struct AnimPlugin;

impl Plugin for AnimPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AnimStore>()
            .init_resource::<fh1_render::glow::DynamicGlows>()
            .init_resource::<AnimView>()
            .add_systems(Update, (stream, animate).chain())
            .add_systems(PostUpdate, debug_view.before(bevy::transform::TransformSystems::Propagate));
    }
}

struct Instance {
    object: usize,
    /// Collision-space placement (column-major).
    place: GMat4,
    /// Engine-space position, for distances.
    at: Vec3,
    distances: [f32; 3],
}

/// One drawable piece of an object: a mesh of one model / LOD, split by texture.
struct Piece {
    model: usize,
    lod: usize,
    /// Rigid: the bone; None = skinned.
    bone: Option<u32>,
    mesh: Handle<Mesh>,
    material: PieceMaterial,
    /// Casts into the sun's cascades: false for blended draws (additive / light shaft / alpha pass), see `casts`.
    cast: bool,
}

/// Shadow casting rules (`FH1_ANIM_SHADOW_RULES=0` = old: every piece casts): blended pieces (the main / sub stage
/// light shafts, lasers, strobes, fireworks) are light, not geometry; the hot-air balloons and birds fly far above
/// the ground, and their cascade shadows read as large dark blobs sweeping the festival (user report 2026-10-09).
fn shadow_rules_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| !std::env::var("FH1_ANIM_SHADOW_RULES").is_ok_and(|v| v == "0"))
}

/// Objects that never cast (by name part, lower case).
const NO_SHADOW_OBJECTS: [&str; 4] = ["hotairballoon", "birds_flying", "firework", "showcase_event"];

fn casts(d: Option<&DrawSpec>) -> bool {
    !shadow_rules_on() || d.is_none_or(|d| d.flags[1] == 0 && !d.light_shaft && d.pass == 0)
}

/// StandardMaterial, or its FxRawStandard copy while the FH1 post chain expects sqrt-encoded colour
/// (`FxLibrary::raw_output`), as scenery.rs does.
#[derive(Clone)]
enum PieceMaterial {
    Std(Handle<StandardMaterial>),
    Raw(Handle<FxRawStandard>),
    /// The game's PROC_ANIM_OBJ shaders.
    Fx(Handle<FxMaterial>),
}

/// One file draw's material inputs (granny::AnimDraw + its mesh), shared by the draws merged into a piece.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct DrawSpec {
    /// 0 = first draw list, 1 = second.
    pass: u8,
    textures: [i32; 4],
    flags: [u32; 3],
    /// f32 bits.
    params: [u32; 2],
    format: u32,
    light_shaft: bool,
    model_data: u32,
    /// Per-vertex blend indices (SKINNED techniques): drawn with the matching RIGID VS after Bevy skinning.
    skinned: bool,
}

struct Loaded {
    granny: granny::Granny,
    pieces: Vec<Piece>,
    bindposes: Handle<SkinnedMeshInverseBindposes>,
    lights: Vec<LightSrc>,
}

/// A light attachment's sprite on a rigid mesh: drawn at the bone's pose (docs/PROPS.md "Light attachments").
struct LightSrc {
    model: usize,
    bone: u32,
    /// Mesh +0x3C ModelData.x as f32 (INFERRED; 0 for all Colorado lights = on at night).
    threshold: f32,
    texture: PathBuf,
    anim_texture: Option<PathBuf>,
    uv_scale: u32,
    light: granny::AnimLight,
}

/// Lights are registered within 140 m of the camera (0x832B9C70; VERIFIED rule 0x82DF7108).
const LIGHT_RANGE: f32 = 140.0;

/// CPU-side build result (meshes not yet in Assets).
struct Built {
    object: granny::AnimObject,
    /// (model, lod, bone, mesh, diffuse texture slot, game draw for the FX path)
    pieces: Vec<(usize, usize, Option<u32>, Mesh, Option<usize>, Option<DrawSpec>)>,
    /// Textures the pieces need, read and decoded by the build task (file -> image): game-shader UNORM copies and
    /// StandardMaterial (image, alpha) ones. Empty with FH1_ANIM_ASYNC_IO=0.
    pre_fx: HashMap<String, Image>,
    pre_std: HashMap<String, (Image, bool)>,
}

#[derive(Default)]
struct AnimWorld {
    dir: PathBuf,
    files: Vec<String>,
    names: Vec<String>,
    /// Per object: texture slot -> file.
    textures: Vec<HashMap<usize, String>>,
    instances: Vec<Instance>,
    drive_inst: Option<usize>,
    objects: HashMap<usize, Arc<Loaded>>,
    pending: HashMap<usize, Task<Option<Built>>>,
    images: HashMap<String, (Handle<Image>, bool)>,
    /// UNORM copies for the game's shaders (they degamma in the shader): file -> (image, gamma-signed).
    fx_images: HashMap<String, (Handle<Image>, bool)>,
    /// Spawned instances: index -> (root entity, per model: joint entities, mesh entities (piece index, entity)).
    spawned: HashMap<usize, Spawned>,
}

struct Spawned {
    root: Entity,
    joints: Vec<Vec<Entity>>,
    meshes: Vec<(usize, Entity)>,
}

impl AnimWorld {
    fn load(assets: &Path) -> Option<Self> {
        let dir = assets.join("anim/colorado");
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).ok()?).ok()?;
        let mut w = AnimWorld { dir, ..default() };
        for o in index["objects"].as_array()? {
            w.files.push(o["file"].as_str()?.to_owned());
            w.textures.push(o["textures"].as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_str()?.to_owned()))).collect()).unwrap_or_default());
        }
        for i in index["instances"].as_array()? {
            if i["free_roam"].as_bool() != Some(true) {
                continue;
            }
            let m = i["matrix"].as_array()?;
            let row = |k: usize| -> Option<[f32; 3]> { let r = m.get(k)?.as_array()?; Some([r[0].as_f64()? as f32, r[1].as_f64()? as f32, r[2].as_f64()? as f32]) };
            let (x, y, z, p) = (row(0)?, row(1)?, row(2)?, row(3)?);
            let place = [[x[0], x[1], x[2], 0.0], [y[0], y[1], y[2], 0.0], [z[0], z[1], z[2], 0.0], [p[0], p[1], p[2], 1.0]];
            let d = i["distances"].as_array()?;
            w.instances.push(Instance {
                object: i["object"].as_u64()? as usize,
                place,
                at: Vec3::new(p[0], p[1], -p[2]),
                distances: [0, 1, 2].map(|k| d.get(k).and_then(|v| v.as_f64()).unwrap_or(500.0) as f32),
            });
        }
        info!("anim: {} objects, {} free-roam instances", w.files.len(), w.instances.len());
        w.names = index["objects"].as_array()?.iter().map(|o| o["name"].as_str().unwrap_or("").to_owned()).collect();
        Some(w)
    }
}

/// Collision-space column-major matrix -> engine-space Mat4 (`S * m * S`, S = diag(1, 1, -1)).
fn to_engine(m: &GMat4) -> Mat4 {
    let mut c = Mat4::from_cols_array_2d(m);
    // S * M * S: negate the Z row and the Z column (the (z, z) entry twice = unchanged).
    for col in 0..4 {
        c.col_mut(col)[2] = -c.col(col)[2];
    }
    let z = c.col(2);
    *c.col_mut(2) = -z;
    c
}

/// The game's shaders draw rigid meshes (`FH1_ANIM_FX=0` = StandardMaterial for everything). Skinned
/// meshes stay on StandardMaterial + Bevy skinning: the game's bone upload (device vfunc 0x7C) is not decoded.
/// Skinned meshes through the game shaders too (`FH1_ANIM_FX_SKIN=0` = StandardMaterial for them).
fn skinned_fx_enabled() -> bool {
    !std::env::var("FH1_ANIM_FX_SKIN").is_ok_and(|v| v == "0")
}

fn fx_enabled() -> bool {
    !std::env::var("FH1_ANIM_FX").is_ok_and(|v| v == "0")
}

/// Parses an object and builds its Bevy meshes (engine space: Z negated, winding reversed).
fn build(bytes: Vec<u8>) -> Option<Built> {
    let object = granny::parse_anim_object(&bytes).map_err(|e| warn!("anim object: {e}")).ok()?;
    let fx = fx_enabled();
    let mut pieces = Vec::new();
    for (mi, model) in object.models.iter().enumerate() {
        for (lod, meshes) in model.lods.iter().enumerate() {
            for mesh in meshes {
                let game = fx && (mesh.bone.is_some() || skinned_fx_enabled());
                // Group the draws: by game draw (both lists) for the FX path, else first-list draws by
                // diffuse slot.
                let mut groups: HashMap<(Option<usize>, Option<DrawSpec>), Vec<u32>> = HashMap::new();
                for d in mesh.draws.iter().filter(|d| game || d.pass == 0) {
                    let slot = (d.textures[0] >= 0).then_some(d.textures[0] as usize);
                    let spec = game.then_some(DrawSpec {
                        pass: d.pass,
                        textures: d.textures,
                        flags: d.flags,
                        params: d.params.map(f32::to_bits),
                        format: mesh.format,
                        // Mode 3 (LIGHT_SHAFT) needs a rigid mesh (selector 0x82DF5ED0).
                        light_shaft: mesh.light_shaft && mesh.bone.is_some(),
                        model_data: mesh.model_data,
                        skinned: mesh.bone.is_none(),
                    });
                    let tri = groups.entry((slot, spec)).or_default();
                    for t in d.first_triangle..d.first_triangle + d.triangles {
                        let t = 3 * t as usize;
                        if t + 2 < mesh.indices.len() {
                            // Reversed winding for the mirrored space.
                            tri.extend_from_slice(&[mesh.indices[t] as u32, mesh.indices[t + 2] as u32, mesh.indices[t + 1] as u32]);
                        }
                    }
                }
                if groups.is_empty() || mesh.vertices.is_empty() {
                    continue;
                }
                let pos: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| [v.position[0], v.position[1], -v.position[2]]).collect();
                let nrm: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| { let n = Vec3::new(v.normal[0], v.normal[1], -v.normal[2]).normalize_or(Vec3::Y); n.to_array() }).collect();
                let uv: Vec<[f32; 2]> = mesh.vertices.iter().map(|v| v.uv).collect();
                // Tangent formats: xyz mirrored; w goes through UV_1.x and flips sign, since the mirror
                // reverses cross(normal, tangent) (INFERRED).
                let tangent = matches!(mesh.format, 0 | 4).then(|| {
                    let t: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| [v.tangent[0], v.tangent[1], -v.tangent[2]]).collect();
                    let w: Vec<[f32; 2]> = mesh.vertices.iter().map(|v| [-v.tangent[3], 0.0]).collect();
                    (t, w)
                });
                for ((slot, spec), idx) in groups {
                    if idx.is_empty() {
                        continue;
                    }
                    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
                    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos.clone());
                    m.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nrm.clone());
                    m.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv.clone());
                    if let (Some(_), Some((t, w))) = (spec, &tangent) {
                        m.insert_attribute(fh1_render::material::ATTRIBUTE_TANGENT, t.clone());
                        m.insert_attribute(Mesh::ATTRIBUTE_UV_1, w.clone());
                    }
                    if mesh.bone.is_none() {
                        let joints: Vec<[u16; 4]> = mesh.vertices.iter().map(|v| v.joints.map(|j| j as u16)).collect();
                        let weights: Vec<[f32; 4]> = mesh
                            .vertices
                            .iter()
                            .map(|v| {
                                let s: f32 = v.weights.iter().sum();
                                if s > 0.0 { v.weights.map(|w| w / s) } else { [1.0, 0.0, 0.0, 0.0] }
                            })
                            .collect();
                        m.insert_attribute(Mesh::ATTRIBUTE_JOINT_INDEX, bevy::mesh::VertexAttributeValues::Uint16x4(joints));
                        m.insert_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT, weights);
                    }
                    m.insert_indices(Indices::U32(idx));
                    pieces.push((mi, lod, mesh.bone, m, slot, spec));
                }
            }
        }
    }
    Some(Built { object, pieces, pre_fx: HashMap::new(), pre_std: HashMap::new() })
}

// D3DRS byte offsets (fh1_shaders::effect::rs).
const RS_ZWRITEENABLE: u32 = 0x30;
const RS_CULLMODE: u32 = 0x38;
const RS_ALPHABLENDENABLE: u32 = 0x3C;
const RS_SRCBLEND: u32 = 0x48;
const RS_DESTBLEND: u32 = 0x4C;
const RS_ALPHATOMASKENABLE: u32 = 0x150;
const RS_ALPHATOMASKOFFSETS: u32 = 0x154;

/// PROC_ANIM_OBJ rigid colour techniques 0-7 (= emissive + 2 normal + 4 spec): VS and mode-0 / mode-2 (SSS)
/// pixel shaders, as default.xex containers (table at 0x834ACA70 filled by 0x82E031E8; VERIFIED).
const RIGID_VS: [u32; 8] = [0x82161EA8, 0x82163520, 0x82162F40, 0x82163A78, 0x82164060, 0x82164BE8, 0x821645E0, 0x82165170];
const COLOUR_PS: [u32; 8] = [0x8214B3B8, 0x8214BE38, 0x8214B878, 0x8214C3B8, 0x8214CA40, 0x8214D6E0, 0x8214D020, 0x8214DD70];
const SSS_PS: [u32; 8] = [0x8214E4D8, 0x8214F138, 0x8214EA88, 0x8214F778, 0x8214FEC0, 0x82150CC0, 0x82150550, 0x82151420];
/// Mode 1 (additive): RIGID_DIFF_ONLY VS / DIFF_ONLY PS. Mode 3: RIGID_LIGHT_SHAFT VS / LIGHT_SHAFT PS.
const DIFF_ONLY: (u32, u32) = (0x821626A0, 0x8214A7F8);
const LIGHT_SHAFT: (u32, u32) = (0x82162A00, 0x8214AE90);
/// Material register for VS c156 UOffset (per-instance scroll; its writer is not found, 0 = GUESSED).
const UOFFSET_REG: usize = 15;

/// The draw 0x82414A00's shader pair and render states for a rigid colour draw (VERIFIED from the
/// disassembly, except where noted).
fn rigid_technique(d: &DrawSpec) -> (u32, u32, Vec<(u32, u32)>) {
    let emissive = d.textures[2] >= 0;
    let normal = d.textures[1] >= 0 && matches!(d.format, 0 | 2 | 4);
    let spec = d.textures[3] >= 0;
    let t = emissive as usize + 2 * normal as usize + 4 * spec as usize;
    // Later rules override earlier ones; starting at mode 0 is INFERRED.
    let mut mode = 0;
    if d.flags[1] != 0 {
        mode = 1;
    }
    if d.flags[2] != 0 {
        mode = 2;
    }
    if d.light_shaft {
        mode = 3;
    }
    let (vs, ps) = match mode {
        1 => DIFF_ONLY,
        2 => (RIGID_VS[t], SSS_PS[t]),
        3 => LIGHT_SHAFT,
        _ => (RIGID_VS[t], COLOUR_PS[t]),
    };
    let mut st = vec![(RS_CULLMODE, if d.flags[0] != 0 { 0 } else { 6 })];
    if d.flags[1] != 0 {
        st.extend([(RS_ALPHABLENDENABLE, 1), (RS_SRCBLEND, 6), (RS_DESTBLEND, 1), (RS_ZWRITEENABLE, 0)]);
    } else if d.pass == 1 {
        st.extend([(RS_ALPHABLENDENABLE, 1), (RS_SRCBLEND, 6), (RS_DESTBLEND, 7), (RS_ZWRITEENABLE, 1)]);
    } else {
        st.extend([(RS_ALPHATOMASKENABLE, 1), (RS_ALPHATOMASKOFFSETS, 0xAA), (RS_ZWRITEENABLE, 1)]);
    }
    (vs, ps, st)
}

/// Adapts a translated PROC_ANIM_OBJ VS to the engine: InstanceMatrix (c152-155, columns of the
/// object-to-world matrix: the VS does pos.x c152 + pos.y c153 + pos.z c154 + c155) = Bevy's mesh
/// transform; WorldViewProjMatrix (c128) is the view-projection only; UOffset c156 -> material;
/// position.w = U (UV_0.x) and normal.w = 2 V - 1 (UV_0.y); tangent.w from UV_1.x.
fn patch_anim_vs(p: &mut fh1_render::Program) -> Option<()> {
    if let Some(d) = std::env::var_os("FH1_ANIM_DUMP") {
        let _ = std::fs::write(format!("{}_{:08x}.wgsl", d.to_string_lossy(), p.wgsl.len()), &p.wgsl);
    }
    let mut w = p.wgsl.clone();
    for k in 0..4 {
        w = w.replace(&format!("fx_glob.vs[{}]", 152 + k), &format!("fx_world[{k}]"));
    }
    w = w
        .replace("fx_row(fx_wvp,", "fx_row(fx_view_proj,")
        .replace("fx_wvp[", "fx_view_proj[")
        .replace("fx_glob.vs[156]", &format!("fx_mat.vs[{UOFFSET_REG}]"));
    let a0 = "fx_vin.a0 = vec4<f32>(input.a0, 1.0);";
    if w.contains(a0) {
        if !w.contains("@location(2) a2:") {
            w = w.replace("    @location(0) a0: vec3<f32>,\n", "    @location(0) a0: vec3<f32>,\n    @location(2) a2: vec2<f32>,\n");
            p.attributes.push(2);
        }
        w = w.replace(a0, "fx_vin.a0 = vec4<f32>(input.a0, input.a2.x);");
    }
    w = w.replace("fx_vin.a1 = vec4<f32>(input.a1, 1.0);", "fx_vin.a1 = vec4<f32>(input.a1, input.a2.y * 2.0 - 1.0);");
    let a5 = "fx_vin.a5 = vec4<f32>(input.a5, 1.0);";
    if w.contains(a5) {
        if !w.contains("@location(3) a3:") {
            w = w.replace("    @location(5) a5: vec3<f32>,\n", "    @location(3) a3: vec2<f32>,\n    @location(5) a5: vec3<f32>,\n");
            p.attributes.push(3);
        }
        w = w.replace(a5, "fx_vin.a5 = vec4<f32>(input.a5, input.a3.x);");
    }
    p.attributes.sort();
    p.attributes.dedup();
    // Nothing of the old routing may remain, and every patched input must be declared.
    let bad = ["fx_glob.vs[152]", "fx_glob.vs[153]", "fx_glob.vs[154]", "fx_glob.vs[155]", "fx_glob.vs[156]", "fx_row(fx_wvp", "fx_wvp["];
    if bad.iter().any(|b| w.contains(b)) || (w.contains("input.a2.") && !w.contains("@location(2) a2:")) {
        return None;
    }
    p.wgsl = w;
    Some(())
}

/// SKINNED / SK_* techniques (VS 0x82165780..0x8216A320; 1-bone 0x8216AB58..) are the RIGID VS of the same
/// variant plus skinning: identical constants and outputs, and the bones (a 3x4 matrix per index, fetched
/// from the instance stream device vfunc 0x7C binds) blend position / normal / tangent before the rigid
/// maths, with InstanceMatrix = the instance only (VERIFIED from the VS declarations; skinning maths
/// INFERRED from the fetch pattern). The engine runs the RIGID VS on Bevy-skinned inputs: the joints are
/// the posed bones in world space and the entity transform (InstanceMatrix) is identity.
fn patch_skinned_vs(p: &mut fh1_render::Program) -> Option<()> {
    patch_anim_vs(p)?;
    let mut w = p.wgsl.clone();
    let head = "struct FxVertexIn {
    @builtin(instance_index) instance_index: u32,
    @builtin(vertex_index) vertex_index: u32,
";
    let call = "    let o = fx_vs_main(input.vertex_index);
";
    let import = "#import bevy_pbr::mesh_functions::get_world_from_local
";
    if !w.contains(head) || !w.contains(call) || !w.contains(import) {
        return None;
    }
    w = w.replace(import, &format!("{import}#import bevy_pbr::skinning::skin_model
"));
    w = w.replace(head, &format!("{head}    @location(25) joints: vec4<u32>,
    @location(26) weights: vec4<f32>,
"));
    let mut skin = String::from("    let fx_skin = skin_model(input.joints, input.weights, input.instance_index);
");
    skin += "    fx_vin.a0 = vec4<f32>((fx_skin * vec4<f32>(fx_vin.a0.xyz, 1.0)).xyz, fx_vin.a0.w);
";
    skin += "    fx_vin.a1 = vec4<f32>(normalize((fx_skin * vec4<f32>(fx_vin.a1.xyz, 0.0)).xyz), fx_vin.a1.w);
";
    if w.contains("fx_vin.a5 = ") {
        skin += "    fx_vin.a5 = vec4<f32>(normalize((fx_skin * vec4<f32>(fx_vin.a5.xyz, 0.0)).xyz), fx_vin.a5.w);
";
    }
    w = w.replace(call, &format!("{skin}{call}"));
    p.attributes.extend([25, 26]);
    p.wgsl = w;
    Some(())
}

/// FH1_ANIM_ASYNC_IO=0: read anim object files and their textures inside the `stream` system again. A slow read there
/// parks the whole frame (2026-10-08 freezes: an 8.8 s stall had this system running).
fn async_io() -> bool {
    !std::env::var("FH1_ANIM_ASYNC_IO").is_ok_and(|v| v == "0")
}

/// The build task: the object file, then every texture its pieces will ask for that isn't cached yet (`cached_*` are
/// the stream system's cache keys when the task started).
fn load_object(path: PathBuf, dir: PathBuf, slots: HashMap<usize, String>, cached_fx: HashSet<String>, cached_std: HashSet<String>) -> Option<Built> {
    let mut b = build(std::fs::read(&path).ok()?)?;
    let Built { pieces, pre_fx, pre_std, .. } = &mut b;
    for (_, _, _, _, slot, spec) in pieces.iter() {
        match spec {
            Some(d) => {
                for &t in &d.textures {
                    let Some(f) = (t >= 0).then(|| slots.get(&(t as usize))).flatten() else { continue };
                    if !cached_fx.contains(f) && !pre_fx.contains_key(f) {
                        if let Some(img) = fh1_render::scenery::read_dds(&dir.join(f)) {
                            pre_fx.insert(f.clone(), img);
                        }
                    }
                }
            }
            None => {
                let Some(f) = slot.and_then(|s| slots.get(&s)) else { continue };
                if !cached_std.contains(f) && !pre_std.contains_key(f) {
                    if let Some(x) = read_dds(&dir.join(f)) {
                        pre_std.insert(f.clone(), x);
                    }
                }
            }
        }
    }
    Some(b)
}

fn read_dds(path: &Path) -> Option<(Image, bool)> {
    let b = std::fs::read(path).ok()?;
    if b.get(..4)? != b"DDS " || b.len() < 148 || b.get(84..88)? != b"DX10" {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let (height, width, mips) = (u32_at(12), u32_at(16), u32_at(28).max(1));
    let (format, alpha) = match u32_at(128) {
        28 | 29 => (TextureFormat::Rgba8UnormSrgb, true),
        71 | 72 => (TextureFormat::Bc1RgbaUnormSrgb, false),
        74 | 75 => (TextureFormat::Bc2RgbaUnormSrgb, true),
        77 | 78 => (TextureFormat::Bc3RgbaUnormSrgb, true),
        _ => return None,
    };
    let mut image = Image::new_uninit(Extent3d { width, height, depth_or_array_layers: 1 }, TextureDimension::D2, format, RenderAssetUsages::RENDER_WORLD);
    image.texture_descriptor.mip_level_count = mips;
    image.data = Some(b[148..].to_vec());
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 8,
        ..ImageSamplerDescriptor::linear()
    });
    Some((image, alpha))
}

#[allow(clippy::too_many_arguments)]
fn stream(
    mut commands: Commands,
    mut world: Local<Option<AnimWorld>>,
    // World generation the cache was built for (X1c: an in-process map change drops it; its entities are WorldEntity).
    mut tried: Local<Option<u32>>,
    (garage, generation): (Res<crate::Garage>, Res<crate::ui::world_load::WorldGeneration>),
    scenery: Option<Res<crate::scenery::Scenery>>,
    cameras: Query<&GlobalTransform, With<fh1_render::post::FxPostCamera>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
    mut raw_materials: ResMut<Assets<FxRawStandard>>,
    mut fx: FxParams,
    mut store: ResMut<AnimStore>,
    mut view: ResMut<AnimView>,
    drive: Option<Res<crate::race::airborne::AirborneDrive>>,
) {
    // Colorado only (FH2's Anthem has no converted crowd/animated-object data).
    if !scenery.as_ref().is_some_and(|s| s.colorado) {
        return;
    }
    if *tried != Some(generation.0) {
        *tried = Some(generation.0);
        *world = None;
        if std::env::var("FH1_ANIM").is_ok_and(|v| v == "0") {
            info!("anim: off (FH1_ANIM=0)");
            return;
        }
        *world = AnimWorld::load(&garage.assets);
        if world.is_none() {
            info!("anim: not installed (run fh1setup --only anim)");
        }
    }
    let (Some(w), Some(cam)) = (world.as_mut(), cameras.iter().next()) else { return };
    let here = cam.translation();
    if view.target.is_none() {
        if let Ok(v) = std::env::var("FH1_ANIM_VIEW") {
            let mut parts = v.split(',');
            let want = parts.next().unwrap_or("").to_ascii_lowercase();
            let dist: f32 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(60.0);
            let height: f32 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(15.0);
            if let Some(inst) = w.instances.iter().find(|i| w.names.get(i.object).is_some_and(|n| n.to_ascii_lowercase().contains(&want))) {
                view.target = Some((inst.at, dist, height));
                info!("anim: viewing {} at {}", want, inst.at);
            }
        }
    }

    // Finished object builds.
    let done: Vec<usize> = w.pending.iter().filter(|(_, t)| t.is_finished()).map(|(k, _)| *k).collect();
    for k in done {
        let Some(mut b) = block_on(future::poll_once(w.pending.remove(&k).unwrap())).flatten() else {
            // Failed: remember an empty object so it isn't retried.
            w.objects.insert(k, Arc::new(Loaded { granny: Default::default(), pieces: Vec::new(), bindposes: bindposes.add(SkinnedMeshInverseBindposes::from(Vec::new())), lights: Vec::new() }));
            continue;
        };
        let max_bones = b.object.granny.models.iter().map(|m| m.bones.len()).max().unwrap_or(0).max(1);
        let bp = bindposes.add(SkinnedMeshInverseBindposes::from(vec![Mat4::IDENTITY; max_bones]));
        let mut pieces = Vec::new();
        for (model, lod, bone, mesh, slot, spec) in b.pieces {
            let cast = casts(spec.as_ref());
            if let Some(d) = spec {
                if let Some(m) = fx_material(&d, &w.dir, &w.textures[k], &mut w.fx_images, &mut b.pre_fx, &mut images, &mut fx) {
                    pieces.push(Piece { model, lod, bone, mesh: meshes.add(mesh), material: PieceMaterial::Fx(m), cast });
                    continue;
                }
                // StandardMaterial fallback: first-list draws only.
                if d.pass != 0 {
                    continue;
                }
            }
            let file = slot.and_then(|s| w.textures[k].get(&s).cloned());
            let (image, alpha) = match &file {
                Some(f) => match w.images.get(f) {
                    Some(x) => (Some(x.0.clone()), x.1),
                    None => match b.pre_std.remove(f).or_else(|| read_dds(&w.dir.join(f))) {
                        Some((img, a)) => {
                            let h = images.add(img);
                            w.images.insert(f.clone(), (h.clone(), a));
                            (Some(h), a)
                        }
                        None => (None, false),
                    },
                },
                None => (None, false),
            };
            let base = StandardMaterial {
                base_color_texture: image,
                alpha_mode: if alpha { AlphaMode::Mask(0.5) } else { AlphaMode::Opaque },
                perceptual_roughness: 0.8,
                double_sided: true,
                cull_mode: None,
                ..default()
            };
            let material = if fx.0.as_ref().is_some_and(|l| l.raw_output) {
                PieceMaterial::Raw(raw_materials.add(FxRawStandard { base, extension: FxRawOutput {} }))
            } else {
                PieceMaterial::Std(materials.add(base))
            };
            pieces.push(Piece { model, lod, bone, mesh: meshes.add(mesh), material, cast });
        }
        let lights = light_sources(&b.object, &w.dir, &w.textures[k]);
        w.objects.insert(k, Arc::new(Loaded { granny: b.object.granny, pieces, bindposes: bp, lights }));
    }

    let driven = drive.as_ref().filter(|d| d.visible).and_then(|d| {
        let name = d.object.as_deref()?;
        Some((w.names.iter().position(|n| n.eq_ignore_ascii_case(name))?, d.clip_t))
    });
    if w.drive_inst.is_none() {
        w.instances.push(Instance { object: 0, place: [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]], at: here, distances: [f32::MAX; 3] });
        w.drive_inst = Some(w.instances.len() - 1);
    }
    let di = w.drive_inst.unwrap();
    w.instances[di].at = here;
    if let Some((o, _)) = driven {
        if !w.spawned.contains_key(&di) {
            w.instances[di].object = o;
        }
    }
    // Spawn / despawn instances by distance.
    for (i, inst) in w.instances.iter().enumerate() {
        let d = inst.at.distance(here);
        let want = if w.drive_inst == Some(i) { driven.is_some_and(|(o, _)| o == inst.object) } else { d <= inst.distances[2] + if w.spawned.contains_key(&i) { KEEP } else { 0.0 } };
        match (want, w.spawned.contains_key(&i)) {
            (false, true) => {
                let s = w.spawned.remove(&i).unwrap();
                commands.entity(s.root).despawn();
                for j in s.joints.into_iter().flatten() {
                    commands.entity(j).despawn();
                }
                for (_, e) in s.meshes {
                    commands.entity(e).despawn();
                }
            }
            (true, false) => {
                let Some(obj) = w.objects.get(&inst.object).cloned() else {
                    if !w.pending.contains_key(&inst.object) {
                        let path = w.dir.join(&w.files[inst.object]);
                        if async_io() {
                            let (dir, slots) = (w.dir.clone(), w.textures[inst.object].clone());
                            let (cached_fx, cached_std) = (w.fx_images.keys().cloned().collect(), w.images.keys().cloned().collect());
                            w.pending.insert(inst.object, AsyncComputeTaskPool::get().spawn(async move { load_object(path, dir, slots, cached_fx, cached_std) }));
                        } else if let Ok(bytes) = std::fs::read(path) {
                            w.pending.insert(inst.object, AsyncComputeTaskPool::get().spawn(async move { build(bytes) }));
                        }
                    }
                    continue;
                };
                let name = w.names.get(inst.object).map(|n| n.to_ascii_lowercase()).unwrap_or_default();
                let object_casts = !shadow_rules_on() || !NO_SHADOW_OBJECTS.iter().any(|n| name.contains(n));
                let root = commands.spawn((Transform::default(), Visibility::default(), crate::ui::world_load::WorldEntity)).id();
                let joints: Vec<Vec<Entity>> = obj.granny.models.iter().map(|m| m.bones.iter().map(|_| commands.spawn((Transform::default(), Visibility::default(), crate::ui::world_load::WorldEntity)).id()).collect()).collect();
                let mut ents = Vec::new();
                for (pi, p) in obj.pieces.iter().enumerate() {
                    let mut e = commands.spawn((Mesh3d(p.mesh.clone()), Transform::default(), Visibility::Hidden, crate::ui::world_load::WorldEntity));
                    match &p.material {
                        PieceMaterial::Std(m) => e.insert(MeshMaterial3d(m.clone())),
                        PieceMaterial::Raw(m) => e.insert(MeshMaterial3d(m.clone())),
                        PieceMaterial::Fx(m) => e.insert(MeshMaterial3d(m.clone())),
                    };
                    if !(object_casts && p.cast) {
                        e.insert(bevy::light::NotShadowCaster);
                    }
                    if p.bone.is_none() {
                        let mut js = joints.get(p.model).cloned().unwrap_or_default();
                        let n = bindposes.get(&obj.bindposes).map_or(0, |b| b.len());
                        js.resize(n.max(js.len()), root);
                        e.insert((SkinnedMesh { inverse_bindposes: obj.bindposes.clone(), joints: js }, NoFrustumCulling));
                    }
                    ents.push((pi, e.id()));
                }
                w.spawned.insert(i, Spawned { root, joints, meshes: ents });
            }
            _ => {}
        }
    }
    // Hand the animation step what it needs.
    store.0.clear();
    for (i, s) in &w.spawned {
        let inst = &w.instances[*i];
        let Some(obj) = w.objects.get(&inst.object) else { continue };
        let d = inst.at.distance(here);
        store.0.push(Live { place: inst.place, lod_distance: d, distances: inst.distances, object: obj.clone(), joints: s.joints.clone(), meshes: s.meshes.clone(), clock: (w.drive_inst == Some(*i)).then(|| driven.map_or(0.0, |d| d.1)) });
    }
}

type FxParams<'w> = (
    Option<ResMut<'w, FxLibrary>>,
    Option<ResMut<'w, FxGlobals>>,
    ResMut<'w, Assets<Shader>>,
    ResMut<'w, Assets<FxMaterial>>,
    Option<Res<'w, fh1_render::postfx::FxPostConfig>>,
);

/// The game material for a rigid draw (draw 0x82414A00): technique by texture slots / flags, the draw's
/// constants and textures (slot 0 -> s0 diffuse, 1 -> s1 normal, 2 -> s3 emissive, 3 -> s4 specular; VERIFIED).
fn fx_material(
    d: &DrawSpec,
    dir: &Path,
    slots: &HashMap<usize, String>,
    cache: &mut HashMap<String, (Handle<Image>, bool)>,
    // Decoded by the build task (load_object); read here only when missing.
    pre: &mut HashMap<String, Image>,
    images: &mut Assets<Image>,
    fx: &mut FxParams,
) -> Option<Handle<FxMaterial>> {
    let (lib, globals, shaders, materials, config) = fx;
    let (lib, globals, config) = (lib.as_mut()?, globals.as_mut()?, config.as_ref()?);
    let (vs, ps, states) = rigid_technique(d);
    let skin = if d.skinned { "_sk" } else { "" };
    let label = format!("anim_{vs:08x}_{ps:08x}{skin}_{}", states.iter().map(|(k, v)| format!("{k:x}-{v:x}")).collect::<Vec<_>>().join("_"));
    let patch = if d.skinned { patch_skinned_vs } else { patch_anim_vs };
    let (id, program) = lib.xex_program(&config.0.xex_dir, vs, Some(ps), &states, &label, shaders, globals, patch)?;
    let mut m = lib.material((id, &program), &[], &[], globals);
    let (p0, p1) = (f32::from_bits(d.params[0]), f32::from_bits(d.params[1]));
    // PS c0 = (1, 1, 1, f@0x28); c1 = (f@0x24 * 3 / view[+0x258], 1, 1, 0.7 if SSS mode 2 else 1) (VERIFIED);
    // view[+0x258] is unknown: taken as 3 (GUESSED), so c1.x = f@0x24.
    m.consts.ps[0] = Vec4::new(1.0, 1.0, 1.0, p1);
    m.consts.ps[1] = Vec4::new(p0, 1.0, 1.0, if d.flags[2] == 2 { 0.7 } else { 1.0 });
    m.consts.vs[UOFFSET_REG] = Vec4::ZERO;
    // c148 ModelData = (mesh +0x3C, 0, 0, 0) (VERIFIED as passed; f32 INFERRED).
    m.consts.object[0] = Vec4::new(f32::from_bits(d.model_data), 0.0, 0.0, 0.0);
    m.no_cull = std::env::var("FH1_ANIM_CULL").is_ok_and(|v| v == "none");
    // The meshes are Z-mirrored with reversed winding; the game's cull mode then removes the front faces
    // here, so it is flipped (INFERRED from screenshots: no cull == flipped). `FH1_ANIM_CULL=game|none` debug.
    m.flip_cull = !std::env::var("FH1_ANIM_CULL").is_ok_and(|v| v == "game");
    for (slot, tf) in [(0usize, 0u32), (1, 1), (2, 3), (3, 4)] {
        let Some(file) = (d.textures[slot] >= 0).then(|| slots.get(&(d.textures[slot] as usize))).flatten() else { continue };
        let (h, gamma) = match cache.get(file) {
            Some(x) => x.clone(),
            None => {
                let img = match pre.remove(file) {
                    Some(img) => img,
                    None => fh1_render::scenery::read_dds(&dir.join(file))?,
                };
                // Gamma from the format as scenery does without a .bix word; normal maps linear (INFERRED).
                let gamma = tf != 1 && fh1_render::scenery::texture_is_gamma(img.texture_descriptor.format);
                let h = images.add(img);
                cache.insert(file.clone(), (h.clone(), gamma));
                (h, gamma)
            }
        };
        if gamma {
            m.consts.gamma.x |= 1 << tf;
        }
        match tf {
            0 => m.t0 = Some(h),
            1 => m.t1 = Some(h),
            3 => m.t3 = Some(h),
            _ => m.t4 = Some(h),
        }
    }
    Some(materials.add(m))
}

/// Each model's light attachments on rigid meshes, from its first LOD that has any (INFERRED: the LODs
/// would otherwise repeat the same lights). Groups whose texture is missing are dropped.
fn light_sources(object: &granny::AnimObject, dir: &Path, slots: &HashMap<usize, String>) -> Vec<LightSrc> {
    let tex = |r: i32| (r >= 0).then(|| slots.get(&(r as usize))).flatten().map(|f| dir.join(f));
    let mut out = Vec::new();
    for (mi, model) in object.models.iter().enumerate() {
        let Some(meshes) = model.lods.iter().find(|l| l.iter().any(|m| !m.lights.is_empty())) else { continue };
        for mesh in meshes {
            let Some(bone) = mesh.bone else { continue };
            for g in &mesh.lights {
                let Some(texture) = tex(g.texture) else { continue };
                let anim_texture = tex(g.anim_texture);
                for l in &g.lights {
                    out.push(LightSrc { model: mi, bone, threshold: f32::from_bits(mesh.model_data), texture: texture.clone(), anim_texture: anim_texture.clone(), uv_scale: g.uv_scale, light: *l });
                }
            }
        }
    }
    out
}

struct Live {
    place: GMat4,
    lod_distance: f32,
    distances: [f32; 3],
    object: Arc<Loaded>,
    joints: Vec<Vec<Entity>>,
    meshes: Vec<(usize, Entity)>,
    clock: Option<f32>,
}

#[derive(Resource, Default)]
pub struct AnimStore(Vec<Live>);

/// Pose update rate by distance (2026-10-08 perf: every spawned object posed every bone and wrote every joint / mesh
/// Transform every frame, 0.77 ms of main thread plus the transform propagation and render re-extraction those writes
/// cause): every frame within [`ANIM_NEAR`] m, every 2nd frame within [`ANIM_MID`] m, every 4th beyond (staggered), and
/// Transforms only written when they change. Objects with lights in range stay at full rate (their glows are rebuilt
/// each frame). FH1_ANIM_RATE=0 = every object every frame, as before.
const ANIM_NEAR: f32 = 150.0;
const ANIM_MID: f32 = 400.0;

fn anim_rate_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("FH1_ANIM_RATE").is_ok_and(|v| v == "0"))
}

fn animate(
    time: Res<Time>,
    store: Res<AnimStore>,
    mut q: Query<(&mut Transform, Option<&mut Visibility>)>,
    mut glows: ResMut<fh1_render::glow::DynamicGlows>,
    cameras: Query<&GlobalTransform, With<fh1_render::post::FxPostCamera>>,
    mut frame: Local<u64>,
) {
    let t = time.elapsed_secs();
    let eye = cameras.iter().next().map(|c| c.translation()).filter(|_| !std::env::var("FH1_ANIM_LIGHTS").is_ok_and(|v| v == "0"));
    let before = glows.0.len();
    glows.0.clear();
    *frame = frame.wrapping_add(1);
    let rate_on = anim_rate_on();
    for (k, live) in store.0.iter().enumerate() {
        if rate_on && live.clock.is_none() && (eye.is_none() || live.object.lights.is_empty()) {
            let every = if live.lod_distance < ANIM_NEAR { 1 } else if live.lod_distance < ANIM_MID { 2 } else { 4 };
            if (*frame + k as u64) % every != 0 {
                continue;
            }
        }
        let g = &live.object.granny;
        let poses: Vec<Vec<Mat4>> = (0..g.models.len()).map(|mi| g.pose(mi, live.clock.unwrap_or(t)).iter().map(|p| to_engine(&granny::mat_mul(&live.place, p))).collect()).collect();
        // Light attachments: local (left-handed) -> engine through the bone's engine-space pose.
        for src in &live.object.lights {
            let (Some(eye), Some(m)) = (eye, poses.get(src.model).and_then(|p| p.get(src.bone as usize))) else { continue };
            let l = &src.light;
            let position = m.transform_point3(Vec3::new(l.position[0], l.position[1], -l.position[2]));
            if position.distance_squared(eye) >= LIGHT_RANGE * LIGHT_RANGE {
                continue;
            }
            glows.0.push(fh1_render::glow::DynamicGlow {
                position,
                direction: m.transform_vector3(Vec3::new(l.direction[0], l.direction[1], -l.direction[2])).normalize_or(Vec3::Y),
                angles: l.angles,
                pull: l.pull,
                half_size: l.half_size,
                phase: l.phase,
                rate: l.rate,
                colour: l.colour,
                threshold: src.threshold,
                texture: src.texture.clone(),
                anim_texture: src.anim_texture.clone(),
                uv_scale: src.uv_scale,
            });
        }
        for (mi, js) in live.joints.iter().enumerate() {
            for (bi, &e) in js.iter().enumerate() {
                if let (Ok((mut tr, _)), Some(m)) = (q.get_mut(e), poses.get(mi).and_then(|p| p.get(bi))) {
                    tr.set_if_neq(Transform::from_matrix(*m));
                }
            }
        }
        // LOD: the nearest band whose LOD this object has (LOD0 < d0 < LOD1 < d1 < LOD2).
        let want = if live.lod_distance < live.distances[0] { 0 } else if live.lod_distance < live.distances[1] { 1 } else { 2 };
        // Per model: which LODs it has (bit l), then the closest to `want` (lower first on ties).
        let mut has = vec![0u8; g.models.len().max(1)];
        for x in &live.object.pieces {
            if let Some(b) = has.get_mut(x.model) {
                *b |= 1 << x.lod.min(7);
            }
        }
        let pick = |model: usize| (0..3usize).filter(|&l| has.get(model).is_some_and(|b| b & (1 << l) != 0)).min_by_key(|&l| (l.abs_diff(want), l)).unwrap_or(0);
        for &(pi, e) in &live.meshes {
            let p = &live.object.pieces[pi];
            let visible = p.lod == pick(p.model);
            if let Ok((mut tr, vis)) = q.get_mut(e) {
                if let Some(mut v) = vis {
                    v.set_if_neq(if visible { Visibility::Inherited } else { Visibility::Hidden });
                }
                if let Some(b) = p.bone {
                    if let Some(m) = poses.get(p.model).and_then(|ps| ps.get(b as usize)) {
                        tr.set_if_neq(Transform::from_matrix(*m));
                    }
                }
            }
        }
    }
    if glows.0.len() != before {
        debug!("anim: {} lights within {LIGHT_RANGE} m", glows.0.len());
    }
}

/// `FH1_ANIM_VIEW` target: (instance position, distance, look-at height).
#[derive(Resource, Default)]
struct AnimView {
    target: Option<(Vec3, f32, f32)>,
}

fn debug_view(view: Res<AnimView>, mut cams: Query<&mut Transform, With<fh1_render::post::FxPostCamera>>) {
    let Some((at, dist, height)) = view.target else { return };
    let look = at + Vec3::Y * height;
    for mut t in &mut cams {
        *t = Transform::from_translation(look + Vec3::new(dist * 0.8, dist * 0.25, dist * 0.55)).looking_at(look, Vec3::Y);
    }
}
