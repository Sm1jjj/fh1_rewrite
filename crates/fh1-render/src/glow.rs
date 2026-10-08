//! FH1's night light glows (`CProceduralLightGlows`, `.pgeo` type 6): camera-facing glow sprites and
//! light beams, switched on by the time of day's SwitchOnLights. RE: docs/SHADERS.md "Light glows";
//! data: the scenery group's `props/glows.json` (docs/PROPS.md "Light glows"; kind "cone" = sprite,
//! "halo" = beam). Only the free-roam groups (no event) are drawn. Shader: `glow.wgsl`.
//! Moving sprites (the anim objects' light attachments) come in through [`DynamicGlows`], refilled by
//! their owner every frame and drawn the same way. `FH1_GLOWS=0` turns them all off.
//!
//! Merged draws (default; `FH1_GLOW_MERGE=0` = one draw per (beam, texture, strip, UVScale) batch, 14 static in
//! Colorado + one per dynamic key): every static glow goes into ONE mesh and every dynamic glow into ONE more, each
//! with one material holding up to [`MERGE_TEX`] sprite textures and [`MERGE_STRIPS`] animation strips. A per-vertex
//! [`ATTRIBUTE_GLOW_SEL`] picks the texture / strip / UVScale / beam that were uniforms before (`GLOW_MERGED` in
//! glow.wgsl). Blending is additive (SRCALPHA / ONE, no depth write), so draw order inside a mesh does not matter.
//! Static and dynamic stay apart so the per-frame dynamic re-upload never carries the ~4k static glows; more
//! textures than the slots spill into another merged draw.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::camera::visibility::NoFrustumCulling;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, MeshVertexAttribute, MeshVertexBufferLayoutRef, PrimitiveTopology, VertexFormat};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError};
use bevy::shader::ShaderRef;

pub struct FxGlowPlugin;

impl Plugin for FxGlowPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "glow.wgsl");
        app.add_plugins(MaterialPlugin::<GlowMaterial>::default())
            .init_resource::<DynamicGlows>()
            .init_resource::<GlowTextures>()
            .add_systems(Startup, setup_glows)
            // In-process map change: the new track's glows replace the old ones.
            .add_systems(Update, setup_glows.run_if(on_message::<crate::postfx::FxTrackChanged>).after(crate::postfx::reload_post))
            .add_systems(
                PostUpdate,
                (draw_dynamic_glows.run_if(|| !glow_merge()), draw_dynamic_glows_merged.run_if(glow_merge), update_glows)
                    .chain()
                    .after(crate::lighting::update_time_of_day),
            );
    }
}

/// `FH1_GLOW_MERGE=0`: one draw per glow batch (the pre-2026-10-08 path). Default: merged draws (module docs).
fn glow_merge() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("FH1_GLOW_MERGE").is_ok_and(|v| v == "0"))
}

/// Merged glows: x = sprite texture slot, y = animation strip slot + 1 (0 = none), z = UVScale index 0..3,
/// w = 1 for beams. Its presence in the mesh layout selects `GLOW_MERGED` in glow.wgsl.
pub const ATTRIBUTE_GLOW_SEL: MeshVertexAttribute = MeshVertexAttribute::new("Fh1_GlowSel", 0x4648_0E10, VertexFormat::Uint32x4);
/// Sprite texture slots per merged draw (`texture`, `tex1`..`tex15`).
pub const MERGE_TEX: usize = 16;
/// Animation strip slots per merged draw (`anim`, `anim1`..`anim7`).
pub const MERGE_STRIPS: usize = 8;

/// `glow.wgsl` `GlowParams`.
#[derive(Clone, Copy, ShaderType, Debug, PartialEq, Default)]
pub struct GlowParams {
    /// x = SwitchOnLights (c157.z), y = 1 for beams, z = 1 to write raw (FH1 post chain on).
    pub p: Vec4,
    /// xy = UVScale (c160) from the group flags, z = 1 for animated sprites (t1 strip), w = T (TimeGain.x).
    pub uv_scale: Vec4,
}

/// One glow batch (`FH1_GLOW_MERGE=0`) or one merged draw. Merged draws also fill `tex1..tex15` / `anim1..anim7`
/// (slot 0 = `texture` / `anim`; their samplers serve every slot: all sprite textures share one descriptor, all strips
/// another, see `GlowTextures::get`). Unused slots bind the fallback image.
#[derive(Asset, TypePath, AsBindGroup, Clone, Debug, Default)]
pub struct GlowMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,
    #[uniform(2)]
    pub params: GlowParams,
    /// Animated sprites: the group's second .pvs texture ref, a 1D strip (sampler 1 "Animation").
    #[texture(3)]
    #[sampler(4)]
    pub anim: Option<Handle<Image>>,
    #[texture(5, visibility(fragment))]
    pub tex1: Option<Handle<Image>>,
    #[texture(6, visibility(fragment))]
    pub tex2: Option<Handle<Image>>,
    #[texture(7, visibility(fragment))]
    pub tex3: Option<Handle<Image>>,
    #[texture(8, visibility(fragment))]
    pub tex4: Option<Handle<Image>>,
    #[texture(9, visibility(fragment))]
    pub tex5: Option<Handle<Image>>,
    #[texture(10, visibility(fragment))]
    pub tex6: Option<Handle<Image>>,
    #[texture(11, visibility(fragment))]
    pub tex7: Option<Handle<Image>>,
    #[texture(12, visibility(fragment))]
    pub tex8: Option<Handle<Image>>,
    #[texture(13, visibility(fragment))]
    pub tex9: Option<Handle<Image>>,
    #[texture(14, visibility(fragment))]
    pub tex10: Option<Handle<Image>>,
    #[texture(15, visibility(fragment))]
    pub tex11: Option<Handle<Image>>,
    #[texture(16, visibility(fragment))]
    pub tex12: Option<Handle<Image>>,
    #[texture(17, visibility(fragment))]
    pub tex13: Option<Handle<Image>>,
    #[texture(18, visibility(fragment))]
    pub tex14: Option<Handle<Image>>,
    #[texture(19, visibility(fragment))]
    pub tex15: Option<Handle<Image>>,
    #[texture(20, visibility(fragment))]
    pub anim1: Option<Handle<Image>>,
    #[texture(21, visibility(fragment))]
    pub anim2: Option<Handle<Image>>,
    #[texture(22, visibility(fragment))]
    pub anim3: Option<Handle<Image>>,
    #[texture(23, visibility(fragment))]
    pub anim4: Option<Handle<Image>>,
    #[texture(24, visibility(fragment))]
    pub anim5: Option<Handle<Image>>,
    #[texture(25, visibility(fragment))]
    pub anim6: Option<Handle<Image>>,
    #[texture(26, visibility(fragment))]
    pub anim7: Option<Handle<Image>>,
}

impl GlowMaterial {
    /// Sets sprite texture slot `i` (`strip` false) or animation strip slot `i` of a merged draw.
    fn set_slot(&mut self, strip: bool, i: usize, h: Handle<Image>) {
        if !strip && i == 0 {
            self.texture = h;
            return;
        }
        let h = Some(h);
        let slot = match (strip, i) {
            (true, 0) => &mut self.anim,
            (true, 1) => &mut self.anim1,
            (true, 2) => &mut self.anim2,
            (true, 3) => &mut self.anim3,
            (true, 4) => &mut self.anim4,
            (true, 5) => &mut self.anim5,
            (true, 6) => &mut self.anim6,
            (true, 7) => &mut self.anim7,
            (false, 1) => &mut self.tex1,
            (false, 2) => &mut self.tex2,
            (false, 3) => &mut self.tex3,
            (false, 4) => &mut self.tex4,
            (false, 5) => &mut self.tex5,
            (false, 6) => &mut self.tex6,
            (false, 7) => &mut self.tex7,
            (false, 8) => &mut self.tex8,
            (false, 9) => &mut self.tex9,
            (false, 10) => &mut self.tex10,
            (false, 11) => &mut self.tex11,
            (false, 12) => &mut self.tex12,
            (false, 13) => &mut self.tex13,
            (false, 14) => &mut self.tex14,
            (false, 15) => &mut self.tex15,
            _ => return,
        };
        *slot = h;
    }
}

impl Material for GlowMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_render/glow.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://fh1_render/glow.wgsl".into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Add
    }
    fn enable_prepass() -> bool {
        false
    }
    fn enable_shadows() -> bool {
        false
    }
    fn specialize(_: &MaterialPipeline, d: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, _: MaterialPipelineKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        let mut attributes = vec![
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_UV_1.at_shader_location(3),
            Mesh::ATTRIBUTE_TANGENT.at_shader_location(4),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(5),
        ];
        // Merged draws: per-vertex texture / strip / UVScale / beam selection (the mesh layout keys the pipeline).
        if layout.0.contains(ATTRIBUTE_GLOW_SEL) {
            attributes.push(ATTRIBUTE_GLOW_SEL.at_shader_location(6));
            d.vertex.shader_defs.push("GLOW_MERGED".into());
            if let Some(f) = d.fragment.as_mut() {
                f.shader_defs.push("GLOW_MERGED".into());
            }
        }
        d.vertex.buffers = vec![layout.0.get_layout(&attributes)?];
        d.primitive.cull_mode = None;
        // SRCALPHA / ONE, no depth write (VERIFIED, begin pass 0x82E0A510).
        if let Some(f) = d.fragment.as_mut() {
            for t in f.targets.iter_mut().flatten() {
                t.blend = Some(BlendState {
                    color: BlendComponent { src_factor: BlendFactor::SrcAlpha, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                    alpha: BlendComponent { src_factor: BlendFactor::Zero, dst_factor: BlendFactor::One, operation: BlendOperation::Add },
                });
            }
        }
        if let Some(ds) = d.depth_stencil.as_mut() {
            ds.depth_write_enabled = Some(false).into();
        }
        Ok(())
    }
}

/// UVScale by sprite group flags (table 0x834AD630, VERIFIED).
const UV_SCALE: [[f32; 2]; 4] = [[1.0, 1.0], [2.0, 1.0], [1.0, 2.0], [2.0, 2.0]];
/// MaxGlowSize (c157.y, VERIFIED).
const MAX_GLOW_SIZE: f32 = 20.0;

#[derive(Default)]
struct Batch {
    pos: Vec<[f32; 3]>,
    dir: Vec<[f32; 3]>,
    a: Vec<[f32; 2]>,
    b: Vec<[f32; 2]>,
    c: Vec<[f32; 4]>,
    colour: Vec<[f32; 4]>,
    /// Merged draws only ([`ATTRIBUTE_GLOW_SEL`]); empty = a per-batch mesh without the attribute.
    sel: Vec<[u32; 4]>,
    idx: Vec<u32>,
}

impl Batch {
    /// Merged draws: appends `o`'s quads, every vertex tagged `sel`.
    fn append(&mut self, o: &Batch, sel: [u32; 4]) {
        let base = self.pos.len() as u32;
        self.pos.extend_from_slice(&o.pos);
        self.dir.extend_from_slice(&o.dir);
        self.a.extend_from_slice(&o.a);
        self.b.extend_from_slice(&o.b);
        self.c.extend_from_slice(&o.c);
        self.colour.extend_from_slice(&o.colour);
        self.sel.resize(self.pos.len(), sel);
        self.idx.extend(o.idx.iter().map(|i| i + base));
    }

    /// Merged draws: tags the vertices added since the last tag with `sel`.
    fn tag(&mut self, sel: [u32; 4]) {
        self.sel.resize(self.pos.len(), sel);
    }

    /// Pads to `quads` quads of zeros (degenerate triangles on vertex 0), keeping the mesh's buffer sizes stable so
    /// the render world's mesh allocator reuses the slab range (particles.rs `pad_quad_mesh`).
    fn pad(&mut self, quads: usize) {
        let v = quads * 4;
        if self.pos.len() < v {
            self.pos.resize(v, [0.0; 3]);
            self.dir.resize(v, [0.0; 3]);
            self.a.resize(v, [0.0; 2]);
            self.b.resize(v, [0.0; 2]);
            self.c.resize(v, [0.0; 4]);
            self.colour.resize(v, [0.0; 4]);
            if !self.sel.is_empty() {
                self.sel.resize(v, [0; 4]);
            }
        }
        if self.idx.len() < quads * 6 {
            self.idx.resize(quads * 6, 0);
        }
    }

    fn quad(&mut self, v: [([f32; 2], [f32; 2]); 4], pos: [f32; 3], dir: [f32; 3], c: [f32; 4], colour: [f32; 4]) {
        let base = self.pos.len() as u32;
        for (a, b) in v {
            self.pos.push(pos);
            self.dir.push(dir);
            self.a.push(a);
            self.b.push(b);
            self.c.push(c);
            self.colour.push(colour);
        }
        self.idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    fn mesh(self) -> Mesh {
        let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
        m.insert_attribute(Mesh::ATTRIBUTE_POSITION, self.pos);
        m.insert_attribute(Mesh::ATTRIBUTE_NORMAL, self.dir);
        m.insert_attribute(Mesh::ATTRIBUTE_UV_0, self.a);
        m.insert_attribute(Mesh::ATTRIBUTE_UV_1, self.b);
        m.insert_attribute(Mesh::ATTRIBUTE_TANGENT, self.c);
        m.insert_attribute(Mesh::ATTRIBUTE_COLOR, self.colour);
        if !self.sel.is_empty() {
            m.insert_attribute(ATTRIBUTE_GLOW_SEL, self.sel);
        }
        m.insert_indices(Indices::U32(self.idx));
        m
    }
}

/// Batch key: (beam, texture, animation strip, group flags).
type BatchKey = (bool, String, Option<String>, u32);

/// A sprite: rec18/rec1C = view angles (cosines in the vertex), rec20 = depth pull, rec28 = size,
/// rec2C = threshold, rec30/rec34 = animation phase/rate (VERIFIED). The animated VS (0x82175680) has
/// no colour input: its colour is the strip's.
#[allow(clippy::too_many_arguments)]
fn sprite_quad(batch: &mut Batch, pos: [f32; 3], dir: [f32; 3], angles: [f32; 2], pull: f32, size: f32, threshold: f32, phase: f32, rate: f32, rgb: [f32; 3], animated: bool) {
    let colour = if animated { [1.0, 1.0, 1.0, threshold] } else { [rgb[0], rgb[1], rgb[2], threshold] };
    let cs = [angles[0].cos(), angles[1].cos()];
    let corners = [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
    // TEX5 HEND3N: x/y packed ×1023, rounded (0x82E11484; VERIFIED).
    let hend = |x: f32| (x.clamp(-1.0, 1.0) * 1023.0).round() / 1023.0;
    batch.quad(corners.map(|k| (k, cs)), pos, dir, [size.clamp(0.0, MAX_GLOW_SIZE), pull, hend(phase), hend(rate)], colour);
}

/// Builds the batches from `glows.json`'s free-roam groups.
fn build_batches(json: &serde_json::Value) -> BTreeMap<BatchKey, Batch> {
    let mut out: BTreeMap<BatchKey, Batch> = BTreeMap::new();
    let v3 = |v: &serde_json::Value| -> [f32; 3] { std::array::from_fn(|i| v[i].as_f64().unwrap_or(0.0) as f32) };
    for group in json.as_array().into_iter().flatten().filter(|g| g["event"].is_null()) {
        for l in group["glows"].as_array().into_iter().flatten() {
            let Some(tex) = l["tex"][0].as_str() else { continue };
            let beam = l["kind"] == "halo";
            let flags = l["flags"].as_u64().unwrap_or(0) as u32;
            let p: [f32; 8] = std::array::from_fn(|i| l["params"][i].as_f64().unwrap_or(0.0) as f32);
            let c = l["colour"].as_u64().unwrap_or(0) as u32;
            let rgb = |s: u32| ((c >> s) & 0xFF) as f32 / 255.0;
            let (pos, dir) = (v3(&l["pos"]), v3(&l["dir"]));
            // A second texture ref selects the animated shaders (sprites only in Colorado; VERIFIED).
            let anim = if beam { None } else { l["tex"][1].as_str().map(str::to_string) };
            let batch = out.entry((beam, tex.to_string(), anim.clone(), if beam { 0 } else { flags & 3 })).or_default();
            if beam {
                // rec18/rec1C = half-width at base/end, rec20 = length, rec24 = threshold (VERIFIED);
                // two mirrored quads, u 0 at the edges' outside .. 0.5 at the axis (INFERRED).
                let colour = [rgb(24), rgb(16), rgb(8), p[3]];
                let (w0, w1, len) = (p[0], p[1], p[2]);
                for s in [-1.0f32, 1.0] {
                    let u = 0.5 + 0.5 * s;
                    batch.quad([([s * w0, 0.0], [u, 0.0]), ([0.0, 0.0], [0.5, 0.0]), ([0.0, len], [0.5, 1.0]), ([s * w1, len], [u, 1.0])], pos, dir, [0.0; 4], colour);
                }
            } else {
                sprite_quad(batch, pos, dir, [p[0], p[1]], p[2], p[4], p[5], p[6], p[7], [rgb(24), rgb(16), rgb(8)], anim.is_some());
            }
        }
    }
    out
}

#[derive(Component)]
struct Glow;

/// One moving glow sprite for this frame, in engine world space: a static Cone sprite record placed
/// by its owner (e.g. the anim objects' light attachments, docs/PROPS.md "Light attachments").
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicGlow {
    pub position: Vec3,
    /// Unit facing direction (the view-angle fade is measured against it).
    pub direction: Vec3,
    /// Inner / outer view angles, radians.
    pub angles: [f32; 2],
    /// Depth pull towards the camera, m.
    pub pull: f32,
    pub half_size: f32,
    /// Animation strip phase / rate (cycles per second), used with `anim_texture`.
    pub phase: f32,
    pub rate: f32,
    pub colour: [u8; 3],
    /// On when SwitchOnLights >= 2 × threshold.
    pub threshold: f32,
    /// DDS paths: absolute, or relative to the installed `scenery/colorado`.
    pub texture: PathBuf,
    pub anim_texture: Option<PathBuf>,
    /// UVScale index 0..3 (the static glows' group flags table).
    pub uv_scale: u32,
}

/// This frame's moving glows. Owners clear and refill it every frame (in Update); the glow plugin
/// draws it in PostUpdate.
#[derive(Resource, Default, Debug)]
pub struct DynamicGlows(pub Vec<DynamicGlow>);

/// Glow textures by (path, is animation strip), shared by the static and dynamic glows.
#[derive(Resource, Default)]
struct GlowTextures {
    root: Option<PathBuf>,
    loaded: HashMap<(PathBuf, bool), Option<Handle<Image>>>,
}

impl GlowTextures {
    fn get(&mut self, images: &mut Assets<Image>, tex: &Path, strip: bool) -> Option<Handle<Image>> {
        let root = self.root.as_ref()?;
        self.loaded
            .entry((tex.to_path_buf(), strip))
            .or_insert_with(|| {
                let mut img = crate::scenery::read_dds(&root.join(tex))?;
                // Linear filtering; sprite textures address mode 1 (INFERRED mirror). The strip's u is
                // frac()'d in the VS, so it repeats.
                let address = if strip { ImageAddressMode::Repeat } else { ImageAddressMode::MirrorRepeat };
                img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
                    address_mode_u: address,
                    address_mode_v: address,
                    mag_filter: ImageFilterMode::Linear,
                    min_filter: ImageFilterMode::Linear,
                    mipmap_filter: ImageFilterMode::Linear,
                    ..default()
                });
                Some(images.add(img))
            })
            .clone()
    }
}

/// The textures of one merged draw, by slot.
#[derive(Default)]
struct MergedSlots {
    tex: Vec<(PathBuf, Handle<Image>)>,
    strips: Vec<(PathBuf, Handle<Image>)>,
}

impl MergedSlots {
    /// `[texture slot, strip slot + 1 (0 = none)]` for this draw, adding the textures if there is room (`.1` = added);
    /// None when the draw is full.
    fn slot(&mut self, tex: (&Path, &Handle<Image>), strip: Option<(&Path, &Handle<Image>)>) -> Option<([u32; 2], bool)> {
        let ti = self.tex.iter().position(|(p, _)| p == tex.0);
        let si = strip.map(|s| self.strips.iter().position(|(p, _)| p == s.0));
        if (ti.is_none() && self.tex.len() >= MERGE_TEX) || (si == Some(None) && self.strips.len() >= MERGE_STRIPS) {
            return None;
        }
        let mut added = false;
        let ti = match ti {
            Some(i) => i,
            None => {
                self.tex.push((tex.0.to_path_buf(), tex.1.clone()));
                added = true;
                self.tex.len() - 1
            }
        };
        let si = match (strip, si) {
            (_, Some(Some(i))) => i + 1,
            (Some(s), Some(None)) => {
                self.strips.push((s.0.to_path_buf(), s.1.clone()));
                added = true;
                self.strips.len()
            }
            _ => 0,
        };
        Some(([ti as u32, si as u32], added))
    }

    /// Writes the slots into `m` (uv_scale.z = 1 when any strip, so update_glows keeps advancing T).
    fn fill(&self, m: &mut GlowMaterial) {
        for (i, (_, h)) in self.tex.iter().enumerate() {
            m.set_slot(false, i, h.clone());
        }
        for (i, (_, h)) in self.strips.iter().enumerate() {
            m.set_slot(true, i, h.clone());
        }
        m.params.uv_scale.z = !self.strips.is_empty() as u32 as f32;
    }

    fn material(&self, params: GlowParams) -> GlowMaterial {
        let mut m = GlowMaterial { params, ..default() };
        self.fill(&mut m);
        m
    }
}

/// The merged draw for `(tex, strip)`: the first in `draws` with the textures or room for them, else a new one.
/// Returns (draw index, `[texture slot, strip slot + 1]`, textures added to that draw).
fn place(draws: &mut Vec<MergedSlots>, tex: (&Path, &Handle<Image>), strip: Option<(&Path, &Handle<Image>)>) -> (usize, [u32; 2], bool) {
    for (i, d) in draws.iter_mut().enumerate() {
        if let Some((s, added)) = d.slot(tex, strip) {
            return (i, s, added);
        }
    }
    let mut d = MergedSlots::default();
    let (s, _) = d.slot(tex, strip).expect("an empty merged draw has room");
    draws.push(d);
    (draws.len() - 1, s, true)
}

/// Quad capacity of a re-uploaded merged mesh (particles.rs, `FH1_FX_MESH_CAP=0` = exact size).
fn merged_capacity(n: usize, cur: usize) -> usize {
    if crate::particles::fx_mesh_cap_on() {
        crate::particles::quad_capacity(n, cur)
    } else {
        n
    }
}

fn setup_glows(
    mut commands: Commands,
    config: Option<Res<crate::postfx::FxPostConfig>>,
    lib: Option<Res<crate::FxLibrary>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<GlowMaterial>>,
    mut textures: ResMut<GlowTextures>,
    old: Query<Entity, With<Glow>>,
) {
    for e in &old {
        commands.entity(e).despawn();
    }
    if std::env::var("FH1_GLOWS").is_ok_and(|v| v == "0") {
        return;
    }
    let Some(config) = config else { return };
    let root = config.0.scenery_dir.clone();
    textures.root = Some(root.clone());
    let Ok(text) = std::fs::read(root.join("props/glows.json")) else {
        info!("fh1-render glows: none ({}/props/glows.json not installed)", root.display());
        return;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&text) else { return };
    let raw = lib.is_some_and(|l| l.raw_output);
    let (mut n, mut animated, mut draws) = (0, 0, 0);
    if glow_merge() {
        // Every batch into one mesh (more textures than the slots: another), selected per vertex.
        let (mut slots, mut merged): (Vec<MergedSlots>, Vec<Batch>) = (Vec::new(), Vec::new());
        for ((beam, tex, anim, flags), batch) in build_batches(&json) {
            let Some(texture) = textures.get(&mut images, Path::new(&tex), false) else { continue };
            let anim_h = anim.as_ref().and_then(|a| textures.get(&mut images, Path::new(a), true));
            let count = batch.pos.len() / if beam { 8 } else { 4 };
            n += count;
            animated += if anim_h.is_some() { count } else { 0 };
            let strip = anim.as_deref().map(Path::new).zip(anim_h.as_ref());
            let (i, s, _) = place(&mut slots, (Path::new(&tex), &texture), strip);
            merged.resize_with(slots.len(), Batch::default);
            merged[i].append(&batch, [s[0], s[1], flags.min(3), beam as u32]);
        }
        let params = GlowParams { p: Vec4::new(-1.0, 0.0, raw as u32 as f32, crate::output_gain()), uv_scale: Vec4::new(1.0, 1.0, 0.0, 0.0) };
        for (s, batch) in slots.iter().zip(merged) {
            if batch.pos.is_empty() {
                continue;
            }
            draws += 1;
            commands.spawn((
                Glow,
                Mesh3d(meshes.add(batch.mesh())),
                MeshMaterial3d(materials.add(s.material(params))),
                Transform::default(),
                // One mesh spread over the whole map: its Aabb would never cull, and the vertex work is ~20k verts.
                NoFrustumCulling,
                bevy::light::NotShadowCaster,
                Name::new("FH1 light glows (merged)"),
            ));
        }
        info!("fh1-render glows: {n} free-roam glows ({animated} animated) in {draws} merged draws");
        return;
    }
    for ((beam, tex, anim, flags), batch) in build_batches(&json) {
        let Some(texture) = textures.get(&mut images, Path::new(&tex), false) else { continue };
        let anim = anim.and_then(|a| textures.get(&mut images, Path::new(&a), true));
        let count = batch.pos.len() / if beam { 8 } else { 4 };
        n += count;
        animated += if anim.is_some() { count } else { 0 };
        draws += 1;
        let uv = UV_SCALE[flags as usize];
        let params = GlowParams {
            p: Vec4::new(-1.0, beam as u32 as f32, raw as u32 as f32, crate::output_gain()),
            uv_scale: Vec4::new(uv[0], uv[1], anim.is_some() as u32 as f32, 0.0),
        };
        commands.spawn((
            Glow,
            Mesh3d(meshes.add(batch.mesh())),
            MeshMaterial3d(materials.add(GlowMaterial { texture, params, anim, ..default() })),
            Transform::default(),
            NoFrustumCulling,
            bevy::light::NotShadowCaster,
            Name::new("FH1 light glows"),
        ));
    }
    info!("fh1-render glows: {n} free-roam glows ({animated} animated) in {draws} draws");
}

type DynamicKey = (PathBuf, Option<PathBuf>, u32);

fn glow_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("FH1_GLOW_GUARD").is_ok_and(|v| v == "0"))
}

/// Same vertex attributes and indices (false when either mesh's data lives only in the render world).
fn same_mesh(a: &Mesh, b: &Mesh) -> bool {
    let (Ok(aa), Ok(ba)) = (a.try_attributes(), b.try_attributes()) else { return false };
    let (aa, ba): (Vec<_>, Vec<_>) = (aa.collect(), ba.collect());
    aa.len() == ba.len()
        && aa.iter().all(|(k, v)| ba.iter().any(|(k2, v2)| k.id == k2.id && v == v2))
        && match (a.try_indices(), b.try_indices()) {
            (Ok(x), Ok(y)) => x == y,
            (Err(_), Err(_)) => true,
            _ => false,
        }
}

/// Draws [`DynamicGlows`]: one mesh per (texture, strip, UVScale), rebuilt every frame (a few dozen
/// quads) with the static sprites' packing and material. Unused draws are hidden, not despawned.
#[allow(clippy::too_many_arguments)]
fn draw_dynamic_glows(
    mut commands: Commands,
    glows: Res<DynamicGlows>,
    globals: Res<crate::FxGlobals>,
    lib: Option<Res<crate::FxLibrary>>,
    mut textures: ResMut<GlowTextures>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<GlowMaterial>>,
    mut vis: Query<&mut Visibility, With<Glow>>,
    mut draws: Local<HashMap<DynamicKey, (Entity, Handle<Mesh>)>>,
) {
    if textures.root.is_none() || (draws.is_empty() && glows.0.is_empty()) {
        return;
    }
    let mut batches: HashMap<DynamicKey, Batch> = HashMap::new();
    for g in &glows.0 {
        let b = batches.entry((g.texture.clone(), g.anim_texture.clone(), g.uv_scale.min(3))).or_default();
        let rgb = g.colour.map(|c| c as f32 / 255.0);
        sprite_quad(b, g.position.to_array(), g.direction.to_array(), g.angles, g.pull, g.half_size, g.threshold, g.phase, g.rate, rgb, g.anim_texture.is_some());
    }
    for (key, (e, _)) in draws.iter() {
        if let Ok(mut v) = vis.get_mut(*e) {
            let want = if batches.contains_key(key) { Visibility::Inherited } else { Visibility::Hidden };
            v.set_if_neq(want);
        }
    }
    for (key, batch) in batches {
        if let Some((_, mesh)) = draws.get(&key) {
            // Replaced only when it changed (2026-10-08 perf: every batch was re-inserted every frame, and each insert is
            // re-extracted and re-allocated by the render world). FH1_GLOW_GUARD=0 = insert every frame.
            let new = batch.mesh();
            if !glow_guard() || !meshes.get(mesh.id()).is_some_and(|old| same_mesh(old, &new)) {
                let _ = meshes.insert(mesh.id(), new);
            }
            continue;
        }
        let Some(texture) = textures.get(&mut images, &key.0, false) else { continue };
        let anim = key.1.as_ref().and_then(|a| textures.get(&mut images, a, true));
        let uv = UV_SCALE[key.2 as usize];
        let raw = lib.as_ref().is_some_and(|l| l.raw_output);
        // update_glows only rewrites static materials on a switch change: start from the current value.
        let params = GlowParams {
            p: Vec4::new(globals.get("EmissiveSwitchOnThreshold").map_or(-1.0, |v| v.x), 0.0, raw as u32 as f32, crate::output_gain()),
            uv_scale: Vec4::new(uv[0], uv[1], anim.is_some() as u32 as f32, globals.get("TimeGain").map_or(0.0, |v| v.x)),
        };
        let mesh = meshes.add(batch.mesh());
        let e = commands
            .spawn((
                Glow,
                Mesh3d(mesh.clone()),
                MeshMaterial3d(materials.add(GlowMaterial { texture, params, anim, ..default() })),
                Transform::default(),
                Visibility::Inherited,
                NoFrustumCulling,
                bevy::light::NotShadowCaster,
                Name::new("FH1 dynamic glows"),
            ))
            .id();
        draws.insert(key, (e, mesh));
    }
}

/// One merged dynamic draw: its entity, mesh, material and padded quad capacity.
struct MergedDraw {
    entity: Entity,
    mesh: Handle<Mesh>,
    material: Handle<GlowMaterial>,
    cap: usize,
}

/// [`draw_dynamic_glows_merged`] state: texture slots and draws (parallel; a draw spawns with its first glows).
#[derive(Default)]
struct DynamicMerged {
    slots: Vec<MergedSlots>,
    draws: Vec<Option<MergedDraw>>,
}

/// Draws [`DynamicGlows`] as merged draws (normally one): rebuilt every frame, re-uploaded only when the padded mesh
/// changed (FH1_GLOW_GUARD), at a stable power-of-two quad capacity. Empty draws are hidden, not despawned; a
/// texture new to a draw rewrites its material once.
#[allow(clippy::too_many_arguments)]
fn draw_dynamic_glows_merged(
    mut commands: Commands,
    glows: Res<DynamicGlows>,
    globals: Res<crate::FxGlobals>,
    lib: Option<Res<crate::FxLibrary>>,
    mut textures: ResMut<GlowTextures>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<GlowMaterial>>,
    mut vis: Query<&mut Visibility, With<Glow>>,
    mut st: Local<DynamicMerged>,
) {
    if textures.root.is_none() || (st.slots.is_empty() && glows.0.is_empty()) {
        return;
    }
    let st = &mut *st;
    // A map change despawns every Glow (setup_glows): drop draws whose entity is gone, they respawn below.
    for d in st.draws.iter_mut() {
        if d.as_ref().is_some_and(|d| vis.get(d.entity).is_err()) {
            *d = None;
        }
    }
    let mut batches: Vec<Batch> = Vec::new();
    let mut dirty: Vec<bool> = Vec::new();
    for g in &glows.0 {
        let Some(texture) = textures.get(&mut images, &g.texture, false) else { continue };
        let strip_h = g.anim_texture.as_ref().and_then(|a| textures.get(&mut images, a, true));
        let strip = g.anim_texture.as_deref().zip(strip_h.as_ref());
        let (i, s, added) = place(&mut st.slots, (g.texture.as_path(), &texture), strip);
        if batches.len() <= i {
            batches.resize_with(i + 1, Batch::default);
            dirty.resize(i + 1, false);
        }
        dirty[i] |= added;
        let b = &mut batches[i];
        let rgb = g.colour.map(|c| c as f32 / 255.0);
        sprite_quad(b, g.position.to_array(), g.direction.to_array(), g.angles, g.pull, g.half_size, g.threshold, g.phase, g.rate, rgb, g.anim_texture.is_some());
        b.tag([s[0], s[1], g.uv_scale.min(3), 0]);
    }
    batches.resize_with(st.slots.len(), Batch::default);
    dirty.resize(st.slots.len(), false);
    st.draws.resize_with(st.slots.len(), || None);
    for (i, mut batch) in batches.into_iter().enumerate() {
        let n = batch.idx.len() / 6;
        if let Some(d) = &mut st.draws[i] {
            let Ok(mut v) = vis.get_mut(d.entity) else { continue };
            if n == 0 {
                v.set_if_neq(Visibility::Hidden);
                continue;
            }
            v.set_if_neq(Visibility::Inherited);
            if dirty[i] {
                if let Some(mut m) = materials.get_mut(&d.material) {
                    st.slots[i].fill(&mut *m);
                }
            }
            d.cap = merged_capacity(n, d.cap);
            batch.pad(d.cap);
            let new = batch.mesh();
            if !glow_guard() || !meshes.get(d.mesh.id()).is_some_and(|old| same_mesh(old, &new)) {
                let _ = meshes.insert(d.mesh.id(), new);
            }
            continue;
        }
        if n == 0 {
            continue;
        }
        let raw = lib.as_ref().is_some_and(|l| l.raw_output);
        // update_glows only rewrites static materials on a switch change: start from the current value.
        let params = GlowParams {
            p: Vec4::new(globals.get("EmissiveSwitchOnThreshold").map_or(-1.0, |v| v.x), 0.0, raw as u32 as f32, crate::output_gain()),
            uv_scale: Vec4::new(1.0, 1.0, 0.0, globals.get("TimeGain").map_or(0.0, |v| v.x)),
        };
        let cap = merged_capacity(n, 0);
        batch.pad(cap);
        let mesh = meshes.add(batch.mesh());
        let material = materials.add(st.slots[i].material(params));
        let entity = commands
            .spawn((
                Glow,
                Mesh3d(mesh.clone()),
                MeshMaterial3d(material.clone()),
                Transform::default(),
                Visibility::Inherited,
                NoFrustumCulling,
                bevy::light::NotShadowCaster,
                Name::new("FH1 dynamic glows (merged)"),
            ))
            .id();
        st.draws[i] = Some(MergedDraw { entity, mesh, material, cap });
    }
}

fn anim_hz() -> f32 {
    static HZ: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *HZ.get_or_init(|| std::env::var("FH1_GLOW_ANIM_HZ").ok().and_then(|v| v.parse().ok()).unwrap_or(20.0))
}

/// c157.z = SwitchOnLights, as the scenery's EmissiveSwitchOnThreshold.x (−1 below 0.05; VERIFIED).
/// Animated sprites also get T = TimeGain.x (c249.x: seconds since start, wrapped at 8 h; lighting.rs).
fn update_glows(
    globals: Res<crate::FxGlobals>,
    glows: Query<&MeshMaterial3d<GlowMaterial>, With<Glow>>,
    mut materials: ResMut<Assets<GlowMaterial>>,
    mut last: Local<Option<f32>>,
    real: Res<Time<Real>>,
    mut since: Local<f32>,
) {
    let t = globals.get("TimeGain").map_or(0.0, |v| v.x);
    let l = globals.get("EmissiveSwitchOnThreshold").map_or(-1.0, |v| v.x);
    let switched = *last != Some(l);
    *last = Some(l);
    // Animated sprites take the time at FH1_GLOW_ANIM_HZ (20; 0 = every frame) instead of every frame: each write
    // re-prepares the material in the render world (2026-10-08 perf, user log 101726: GlowMaterial prepare 0.16 ms/frame).
    *since += real.delta_secs();
    let hz = anim_hz();
    let anim_due = hz <= 0.0 || *since >= 1.0 / hz;
    if anim_due {
        *since = 0.0;
    }
    if !switched && !anim_due {
        return;
    }
    for m in &glows {
        let animated = materials.get(&m.0).is_some_and(|m| m.anim.is_some());
        if !switched && !animated {
            continue;
        }
        if let Some(mut m) = materials.get_mut(&m.0) {
            m.params.p.x = l;
            m.params.uv_scale.w = t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches() {
        let j = serde_json::json!([
            {"name": "a", "event": null, "glows": [
                {"kind": "cone", "tex": ["t.dds", null], "flags": 3, "pos": [0, 1, 2], "dir": [0, 1, 0], "params": [6.1, 6.28, 0.1, 0.1, 1.2, 0, 0, 0], "colour": 0xCAE8FF00u32},
                {"kind": "cone", "tex": ["t.dds", "s.dds"], "flags": 0, "pos": [0, 1, 2], "dir": [0, 1, 0], "params": [1.0, 2.2, 0.1, 0.1, 1.2, 0, 0.35, 0.5], "colour": 0x33333300u32},
                {"kind": "halo", "tex": ["t.dds", null], "flags": 0, "pos": [0, 1, 2], "dir": [0, -1, 0], "params": [3.5, 4, 8.1, 0, 0, 0, 0, 0], "colour": 0x0a0a0a00u32}]},
            {"name": "b", "event": "FR01_L", "glows": [
                {"kind": "cone", "tex": ["t.dds", null], "flags": 0, "pos": [0, 0, 0], "dir": [0, 1, 0], "params": [0, 1, 0, 0, 1, 0, 0, 0], "colour": 0}]}
        ]);
        let b = build_batches(&j);
        assert_eq!(b.len(), 3);
        let s = &b[&(false, "t.dds".to_string(), None, 3)];
        assert_eq!(s.pos.len(), 4);
        assert!((s.colour[0][0] - 0xCA as f32 / 255.0).abs() < 1e-6);
        let a = &b[&(false, "t.dds".to_string(), Some("s.dds".to_string()), 0)];
        assert_eq!((a.colour[0][0], a.c[0][2], a.c[0][3]), (1.0, 358.0 / 1023.0, 512.0 / 1023.0));
        assert_eq!(b[&(true, "t.dds".to_string(), None, 0)].pos.len(), 8);
    }

    #[test]
    fn merged_slots() {
        let h = Handle::<Image>::default();
        let mut d = Vec::new();
        let p = |i: usize| PathBuf::from(format!("t{i}.dds"));
        assert_eq!(place(&mut d, (p(0).as_path(), &h), None), (0, [0, 0], true));
        assert_eq!(place(&mut d, (p(0).as_path(), &h), Some((p(100).as_path(), &h))), (0, [0, 1], true));
        assert_eq!(place(&mut d, (p(0).as_path(), &h), Some((p(100).as_path(), &h))), (0, [0, 1], false));
        for i in 1..MERGE_TEX {
            assert_eq!(place(&mut d, (p(i).as_path(), &h), None), (0, [i as u32, 0], true));
        }
        // Full: the next texture spills into a second draw.
        assert_eq!(place(&mut d, (p(99).as_path(), &h), None), (1, [0, 0], true));
        let mut a = Batch::default();
        sprite_quad(&mut a, [0.0; 3], [0.0, 1.0, 0.0], [1.0, 2.0], 0.0, 1.0, 0.0, 0.0, 0.0, [1.0; 3], false);
        let mut m = Batch::default();
        m.append(&a, [1, 0, 0, 0]);
        m.append(&a, [2, 0, 3, 0]);
        assert_eq!((m.pos.len(), m.sel.len(), m.idx[6], m.sel[4]), (8, 8, 4, [2, 0, 3, 0]));
        m.pad(64);
        assert_eq!((m.pos.len(), m.sel.len(), m.idx.len()), (256, 256, 384));
    }
}
