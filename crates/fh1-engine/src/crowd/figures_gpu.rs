//! GPU crowd figures (perf push 2026-10-08, user: "does it have to be 7,000 entities?"; `FH1_CROWD_GPU_FIGURES=0` = the
//! CPU figures). The near 3D spectators were Bevy skinned meshes: a root, a body and ~36 joint entities each, every joint
//! Transform written by `animate` every frame (up to ~200 figures at the festival = ~7k entities through transform
//! propagation, skin extraction and the render world). Here a figure is ONE entity (mesh + material + Transform + a
//! `MeshTag` slot): the clips' skinning matrices (bone world x inverse bind, every frame of every clip in use) live in one
//! storage buffer, each slot holds its current and next clip and their start times on the engine clock, and
//! `crowd_skin.wgsl` / `crowd_skin_prepass.wgsl` skin the skinbin mesh on the GPU. Identical meshes batch into a few draws.
//! The clip choice is the game's as before (the class's 50-slot table when a clip ends), decided on the CPU ~1 s ahead
//! and written in batches (`WRITE_EVERY`), so the slot buffer is uploaded a few times a second, not every frame.
//! Remaster / StandardMaterial path only: the faithful renderer's figures (FxRawStandard) stay on the CPU.

use super::*;
use bevy::mesh::{MeshTag, MeshVertexAttribute, VertexFormat};
use bevy::pbr::{ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline};
use bevy::render::storage::ShaderBuffer;

/// Joints 0 / 1 and weights 0 / 1 per vertex (not Bevy's joint attributes: those make the mesh "skinned" and Bevy
/// would want a SkinnedMesh with joint entities).
pub const ATTRIBUTE_BONES: MeshVertexAttribute = MeshVertexAttribute::new("Fh1CrowdBones", 0x0F1C_B0E5, VertexFormat::Float32x4);
/// Figure slots (3 vec4 each). Beyond this many live figures, new ones fall back to CPU figures.
const SLOTS: usize = 512;
/// The next clip is picked this long before the current one ends.
const LOOKAHEAD: f32 = 1.2;
/// Slot buffer uploads at most this often (s), plus at once for new figures.
const WRITE_EVERY: f32 = 0.5;

pub(super) fn enabled() -> bool {
    std::env::var("FH1_CROWD_GPU_FIGURES").map_or(true, |v| v != "0")
}

#[derive(Asset, AsBindGroup, TypePath, Clone, Debug)]
pub struct CrowdSkin {
    #[storage(100, read_only)]
    pub mats: Handle<ShaderBuffer>,
    #[storage(101, read_only)]
    pub figs: Handle<ShaderBuffer>,
}

impl MaterialExtension for CrowdSkin {
    fn vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/crowd_skin.wgsl".into()
    }
    fn prepass_vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/crowd_skin_prepass.wgsl".into()
    }
    fn deferred_vertex_shader() -> ShaderRef {
        "embedded://fh1_engine/crowd_skin_prepass.wgsl".into()
    }
    fn specialize(_: &MaterialExtensionPipeline, d: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, _: MaterialExtensionKey<Self>) -> Result<(), SpecializedMeshPipelineError> {
        let prepass = d.vertex.shader_defs.iter().any(|s| matches!(s, bevy::shader::ShaderDefVal::Bool(n, true) if n.as_str() == "PREPASS_PIPELINE"));
        let attrs = if prepass {
            vec![
                Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
                Mesh::ATTRIBUTE_UV_0.at_shader_location(1),
                Mesh::ATTRIBUTE_NORMAL.at_shader_location(3),
                ATTRIBUTE_BONES.at_shader_location(8),
            ]
        } else {
            vec![
                Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
                Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
                Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
                ATTRIBUTE_BONES.at_shader_location(8),
            ]
        };
        d.vertex.buffers = vec![layout.0.get_layout(&attrs)?];
        Ok(())
    }
}

pub type FigureMaterial = ExtendedMaterial<StandardMaterial, CrowdSkin>;

/// Marks a GPU figure entity (its slot is freed once the entity is gone).
#[derive(Component)]
pub struct GpuFigure;

/// A baked clip: its first matrix, frame count, frame time and duration.
#[derive(Clone, Copy)]
struct BakedClip {
    first: u32,
    frames: u32,
    frame_time: f32,
    duration: f32,
}

struct Slot {
    entity: Entity,
    table: ClipTable,
    rng: u32,
    /// The entity has existed (spawn commands apply after the frame's update): free the slot once it's gone.
    seen: bool,
    bones: u32,
    cur: (BakedClip, f32),
    next: Option<(ClipArc, BakedClip, f32)>,
}

#[derive(Default)]
pub(super) struct GpuFigures {
    mats: Vec<[f32; 4]>,
    clips: HashMap<usize, BakedClip>,
    mats_buffer: Option<Handle<ShaderBuffer>>,
    figs_buffer: Option<Handle<ShaderBuffer>>,
    mats_dirty: bool,
    figs_dirty: bool,
    last_write: f32,
    slots: Vec<Option<Slot>>,
    materials: HashMap<String, Handle<FigureMaterial>>,
    meshes: HashMap<(String, usize), Option<Handle<Mesh>>>,
}

/// Engine clock as the shaders see it (`globals.time` = Time::elapsed_secs_wrapped, 3600 s period).
pub(super) fn clock(time: &Time) -> f32 {
    time.elapsed_secs_wrapped()
}

fn wrap(t: f32) -> f32 {
    t.rem_euclid(3600.0)
}

fn dt(now: f32, start: f32) -> f32 {
    let mut d = now - start;
    if d < -1800.0 {
        d += 3600.0;
    }
    if d > 1800.0 {
        d -= 3600.0;
    }
    d
}

impl GpuFigures {
    fn buffers(&mut self, buffers: &mut Assets<ShaderBuffer>) -> (Handle<ShaderBuffer>, Handle<ShaderBuffer>) {
        if self.mats_buffer.is_none() {
            self.slots = (0..SLOTS).map(|_| None).collect();
            self.mats.push([0.0; 4]);
            self.mats_buffer = Some(buffers.add(ShaderBuffer::new(&bytes(&self.mats), RenderAssetUsages::RENDER_WORLD)));
            self.figs_buffer = Some(buffers.add(ShaderBuffer::new(&vec![0u8; SLOTS * 48], RenderAssetUsages::RENDER_WORLD)));
        }
        (self.mats_buffer.clone().unwrap(), self.figs_buffer.clone().unwrap())
    }

    /// The clip's skinning matrices (bone world x inverse bind per frame, rows of the 3x4), baked once.
    fn bake(&mut self, clip: &ClipArc) -> BakedClip {
        let key = Arc::as_ptr(clip) as usize;
        if let Some(b) = self.clips.get(&key) {
            return *b;
        }
        let (skel, c) = (&clip.0, &clip.1);
        let frames = c.rotations.len().max(1);
        let first = (self.mats.len() / 3) as u32;
        let first = if self.mats.len() % 3 != 0 {
            // Keep matrices 3-row aligned (the buffer starts with one padding vec4).
            while self.mats.len() % 3 != 0 {
                self.mats.push([0.0; 4]);
            }
            (self.mats.len() / 3) as u32
        } else {
            first
        };
        let n = skel.bones.len();
        for f in 0..frames {
            let mut world: Vec<Mat4> = Vec::with_capacity(n);
            for (b, bone) in skel.bones.iter().enumerate() {
                let (q, t) = if c.rotations.is_empty() {
                    (Quat::IDENTITY, Vec3::from_array(bone.local))
                } else {
                    (Quat::from_array(c.rotations[f][b]).normalize(), Vec3::from_array(c.local_translation(skel, f, b)))
                };
                let local = Mat4::from_rotation_translation(q, t);
                let w = bone.parent.and_then(|p| world.get(p as usize).copied()).map_or(local, |pw| pw * local);
                world.push(w);
            }
            for (b, bone) in skel.bones.iter().enumerate() {
                let m = world[b] * Mat4::from_translation(-Vec3::from_array(bone.world));
                for r in 0..3 {
                    self.mats.push(m.row(r).to_array());
                }
            }
        }
        let baked = BakedClip { first, frames: frames as u32, frame_time: c.frame_time.max(1e-4), duration: c.duration.max(0.0) };
        self.clips.insert(key, baked);
        self.mats_dirty = true;
        baked
    }

    fn slot_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SLOTS * 48);
        for s in &self.slots {
            let v: [[f32; 4]; 3] = match s {
                Some(s) => {
                    let (c, start) = s.cur;
                    let (n, nd, ns) = s.next.as_ref().map_or(([0.0; 3], 0.0, 0.0), |(_, b, st)| ([b.first as f32, b.frames as f32, b.frame_time], b.duration, *st));
                    [
                        [c.first as f32, c.frames as f32, c.frame_time, start],
                        [n[0], n[1], n[2], ns],
                        [s.bones as f32, c.duration, nd, 0.0],
                    ]
                }
                None => [[0.0; 4]; 3],
            };
            for r in v {
                for x in r {
                    out.extend(x.to_le_bytes());
                }
            }
        }
        out
    }

    /// Per frame: free slots of despawned figures, schedule next clips ahead, upload what changed.
    pub(super) fn update(&mut self, now: f32, alive: &Query<(), With<GpuFigure>>, buffers: &mut Assets<ShaderBuffer>) {
        if self.mats_buffer.is_none() {
            return;
        }
        for k in 0..self.slots.len() {
            let Some(s) = self.slots[k].as_mut() else { continue };
            if !alive.contains(s.entity) {
                if s.seen {
                    self.slots[k] = None;
                    self.figs_dirty = true;
                }
                continue;
            }
            s.seen = true;
            // Current clip over: the next one takes over (the shader already switched at its start time).
            if let Some((_, b, st)) = s.next.as_ref() {
                if dt(now, *st) >= 0.0 {
                    s.cur = (*b, *st);
                    s.next = None;
                }
            }
            if s.next.is_none() && dt(now, s.cur.1) >= s.cur.0.duration - LOOKAHEAD {
                s.rng = s.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let clip = s.table[(s.rng >> 16) as usize % s.table.len()].clone();
                let start = wrap(s.cur.1 + s.cur.0.duration);
                s.next = Some((clip, BakedClip { first: 0, frames: 0, frame_time: 1.0, duration: 0.0 }, start));
            }
        }
        // Bake the newly picked clips (outside the slot borrow).
        for k in 0..self.slots.len() {
            let pending = self.slots[k].as_ref().and_then(|s| s.next.as_ref().filter(|n| n.1.frames == 0).map(|n| n.0.clone()));
            if let Some(clip) = pending {
                let b = self.bake(&clip);
                if let Some((_, nb, _)) = self.slots[k].as_mut().and_then(|s| s.next.as_mut()) {
                    *nb = b;
                }
                self.figs_dirty = true;
            }
        }
        if self.mats_dirty {
            self.mats_dirty = false;
            if let Some(mut b) = self.mats_buffer.as_ref().and_then(|h| buffers.get_mut(h)) {
                b.data = Some(bytes(&self.mats));
            }
        }
        if self.figs_dirty && dt(now, self.last_write).abs() >= WRITE_EVERY {
            self.figs_dirty = false;
            self.last_write = now;
            let data = self.slot_bytes();
            if let Some(mut b) = self.figs_buffer.as_ref().and_then(|h| buffers.get_mut(h)) {
                b.data = Some(data);
            }
        }
    }
}

fn bytes(v: &[[f32; 4]]) -> Vec<u8> {
    v.iter().flatten().flat_map(|x| x.to_le_bytes()).collect()
}

impl CrowdWorld {
    /// The skinbin mesh with `ATTRIBUTE_BONES` instead of Bevy's joint attributes.
    pub(super) fn gpu_mesh(&mut self, name: &str, lod: usize, meshes: &mut Assets<Mesh>) -> Option<Handle<Mesh>> {
        let key = (name.to_owned(), lod);
        if let Some(m) = self.gpu_fig.meshes.get(&key) {
            return m.clone();
        }
        let m = (|| {
            let d = self.read(&format!("models/{name}.skinbin"))?;
            let sk = fh1_formats::crowd::parse_skinbin(&d).ok()?;
            let range = sk.lods.get(lod).or(sk.lods.last())?.clone();
            let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
            mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, sk.vertices.iter().map(|v| v.position).collect::<Vec<_>>());
            mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, sk.vertices.iter().map(|v| v.normal).collect::<Vec<_>>());
            mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, sk.vertices.iter().map(|v| v.uv).collect::<Vec<_>>());
            mesh.insert_attribute(
                ATTRIBUTE_BONES,
                sk.vertices.iter().map(|v| [v.bones[0] as f32, v.bones[1] as f32, v.weights[0] as f32 / 255.0, v.weights[1] as f32 / 255.0]).collect::<Vec<[f32; 4]>>(),
            );
            mesh.insert_indices(Indices::U32(sk.indices[range].iter().map(|&i| i as u32).collect()));
            Some(meshes.add(mesh))
        })();
        self.gpu_fig.meshes.insert(key, m.clone());
        m
    }

    /// The figure's material (StandardMaterial as the CPU figures' + the skinning extension), per texture.
    fn gpu_material(&mut self, tex: &str, a: &mut Assets3d) -> Option<Handle<FigureMaterial>> {
        if let Some(m) = self.gpu_fig.materials.get(tex) {
            return Some(m.clone());
        }
        let (mats, figs) = self.gpu_fig.buffers(a.buffers);
        let img = srgb(fh1_render::scenery::read_dds(&self.dir.join("textures").join(format!("{tex}.dds")))?);
        let base = StandardMaterial { base_color_texture: Some(a.images.add(img)), perceptual_roughness: 0.9, alpha_mode: AlphaMode::Mask(0.5), double_sided: true, cull_mode: None, ..default() };
        let m = a.fig_mats.add(ExtendedMaterial { base, extension: CrowdSkin { mats, figs } });
        self.gpu_fig.materials.insert(tex.to_owned(), m.clone());
        Some(m)
    }

    /// `spawn_figure` on the GPU path: one entity. None = no free slot / missing data (the caller falls back).
    pub(super) fn spawn_gpu_figure(&mut self, commands: &mut Commands, a: &mut Assets3d, seed: u32, class: u8, model: u32, lod: usize, t: Transform) -> Option<Figure> {
        let (name, tex) = self.models.get(model as usize).cloned()?;
        let c = self.classes.get(class as usize)?.clone();
        if c.anims.is_empty() {
            return None;
        }
        let mesh_name = format!("{name}{}", c.suffix);
        let mesh = self.gpu_mesh(&mesh_name, lod, a.meshes)?;
        let mat = self.gpu_material(&tex, a)?;
        let k = self.gpu_fig.slots.iter().position(Option::is_none)?;
        let h = seed.wrapping_mul(0x9E37_79B9).rotate_left(13);
        let table = self.table(class)?;
        let clip = table[h as usize % table.len()].clone();
        let baked = self.gpu_fig.bake(&clip);
        // Start part-way into the first clip, as the CPU figures' phase.
        let phase = (h >> 8) as f32 / (1u32 << 24) as f32 * baked.duration.max(0.01);
        let now = self.gpu_now;
        let root = commands.spawn((Mesh3d(mesh), MeshMaterial3d(mat), t, MeshTag(k as u32), GpuFigure, crate::ui::world_load::WorldEntity)).id();
        self.gpu_fig.slots[k] = Some(Slot { entity: root, table, rng: h, seen: false, bones: clip.0.bones.len() as u32, cur: (baked, wrap(now - phase)), next: None });
        self.gpu_fig.figs_dirty = true;
        // A new figure must be in the buffer before it draws: write now.
        self.gpu_fig.last_write = now - WRITE_EVERY;
        Some(Figure { root, body: root, name: mesh_name, lod })
    }
}
