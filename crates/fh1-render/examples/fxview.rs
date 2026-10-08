//! fxview â€” render Colorado models around a point with their real FH1 shaders.
//!
//! `fxview <disc dir> <install assets dir> [x z radius] [--shot out.png] [--yaw deg] [--pitch deg] [--height m]`
//! Loads every model in bin.zip whose bounds touch the circle, builds meshes with all the
//! attributes their shader's vertex declaration has (UV offset/scale baked per mesh), creates
//! an FxMaterial per material (constants + PVS-bound textures from `<assets>/scenery/colorado/
//! textures/<id>.dds`), sets placeholder lighting globals and renders.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use fh1_formats::{fxobj, pvs, rmb, zip::Archive};
use fh1_render::material::{ATTRIBUTE_COLOR, ATTRIBUTE_TANGENT, ATTRIBUTE_UV2};
use fh1_render::{Fh1RenderPlugin, FxGlobals, FxLibrary, FxMaterial};

#[derive(Resource, Clone)]
struct Args {
    disc: PathBuf,
    assets: PathBuf,
    centre: Vec2,
    radius: f32,
    shot: Option<String>,
    yaw: f32,
    pitch: f32,
    height: f32,
    minutes: f32,
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let num = |i: usize, d: f32| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let args = Args {
        disc: PathBuf::from(&a[1]),
        assets: PathBuf::from(&a[2]),
        // start_location_00 (engine space).
        centre: Vec2::new(num(3, 1584.97), num(4, 1843.62)),
        radius: num(5, 150.0),
        shot: flag("--shot"),
        yaw: flag("--yaw").and_then(|s| s.parse().ok()).unwrap_or(0.0),
        pitch: flag("--pitch").and_then(|s| s.parse().ok()).unwrap_or(-10.0),
        height: flag("--height").and_then(|s| s.parse().ok()).unwrap_or(6.0),
        minutes: flag("--time").and_then(|s| s.parse().ok()).unwrap_or(720.0),
    };
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { title: "fxview".into(), resolution: (1280u32, 720u32).into(), ..default() }),
            ..default()
        }))
        .add_plugins(Fh1RenderPlugin)
        .insert_resource(args)
        .insert_resource(ClearColor(Color::srgb(0.5, 0.6, 0.7)))
        .add_systems(Startup, load_scene)
        .add_systems(Update, (lighting, screenshot))
        .run();
}

fn lighting(mut g: ResMut<FxGlobals>, args: Res<Args>, mut tod: Local<Option<fh1_render::tod::TimeOfDay>>) {
    let t = tod.get_or_insert_with(|| {
        let p = args.disc.join("media/tracks/colorado/Ribbon_00/TimeOfDayA.xml");
        fh1_render::tod::TimeOfDay::parse(&std::fs::read_to_string(p).unwrap_or_default())
    });
    fh1_render::lighting::apply_time_of_day(&mut g, t, args.minutes * 60.0, &Default::default());
}

fn screenshot(mut commands: Commands, args: Res<Args>, mut frame: Local<u32>, mut exit: MessageWriter<AppExit>) {
    let Some(path) = &args.shot else { return };
    *frame += 1;
    if *frame == 90 {
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(path.clone()));
    }
    if *frame == 100 {
        exit.write(AppExit::Success);
    }
}

fn read_dds(path: &Path) -> Option<Image> {
    let b = std::fs::read(path).ok()?;
    if b.get(..4)? != b"DDS " || b.len() < 148 || b.get(84..88)? != b"DX10" {
        return None;
    }
    let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let (height, width, mips) = (u(12), u(16), u(28).max(1));
    let format = match u(128) {
        71 | 72 => TextureFormat::Bc1RgbaUnorm,
        74 | 75 => TextureFormat::Bc2RgbaUnorm,
        77 | 78 => TextureFormat::Bc3RgbaUnorm,
        80 => TextureFormat::Bc4RUnorm,
        83 => TextureFormat::Bc5RgUnorm,
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
    Some(image)
}

#[allow(clippy::too_many_arguments)]
fn load_scene(
    mut commands: Commands,
    args: Res<Args>,
    mut lib: ResMut<FxLibrary>,
    mut globals: ResMut<FxGlobals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FxMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    let colorado = args.disc.join("media/tracks/colorado");
    let mut ar = Archive::open(colorado.join("bin.zip")).expect("bin.zip");
    let pvs = pvs::parse(&std::fs::read(colorado.join("Ribbon_00/Colorado_00.pvs")).expect("pvs")).expect("pvs parse");
    let tex_dir = args.assets.join("scenery/colorado/textures");
    let entries = ar.entries.clone();
    let mut fx_bytes: HashMap<String, Vec<u8>> = HashMap::new();
    for e in entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".fxobj")) {
        let stem = e.name.rsplit(['/', '\\']).next().unwrap().to_ascii_lowercase().trim_end_matches(".fxobj").to_string();
        fx_bytes.insert(stem, ar.read(e).unwrap());
    }
    let mut image_cache: HashMap<u32, Option<Handle<Image>>> = HashMap::new();
    let (mut n_models, mut n_meshes, mut n_tris, mut ground) = (0, 0, 0usize, f32::MIN);
    let mut seen = std::collections::HashSet::new();
    for e in &entries {
        let name = e.name.to_ascii_lowercase();
        if !name.ends_with(".rmb.bin") || !seen.insert(name.clone()) {
            continue;
        }
        let Ok(model) = rmb::parse(&ar.read(e).unwrap()) else { continue };
        let (lo, hi) = (model.bounds_min, model.bounds_max);
        let (c, r) = (args.centre, args.radius);
        if lo[0] > c.x + r || hi[0] < c.x - r || lo[2] > c.y + r || hi[2] < c.y - r {
            continue;
        }
        let number: Option<usize> = name.split('.').nth(1).and_then(|s| s.parse().ok());
        n_models += 1;
        for sub in model.submodels.iter().filter(|s| s.lod() == 0 && !s.is_helper()) {
            for mesh in &sub.meshes {
                let Some(mat) = model.materials.get(mesh.material as usize) else { continue };
                let Some(shader_path) = model.shaders.get(mat.shader as usize) else { continue };
                let shader = shader_path.rsplit(['\\', '/']).next().unwrap().trim_end_matches(".fx").to_ascii_lowercase();
                let Some(bytes) = fx_bytes.get(&shader) else { continue };
                let Ok(decl) = fxobj::vertex_decl(bytes) else { continue };
                if !lib.has_effect(&shader) && lib.add_effect(&shader, bytes).is_err() {
                    continue;
                }
                let Some((pid, program)) = lib.program(&shader, "Default", &mut shaders, &mut globals) else { continue };
                let Some(m) = build_mesh(sub, mesh, &decl, &program.attributes) else { continue };
                n_tris += mesh.indices.len() / 3;
                if (Vec2::new(sub.positions[mesh.indices[0] as usize][0], sub.positions[mesh.indices[0] as usize][2]) - c).length() < 20.0 {
                    ground = ground.max(sub.positions[mesh.indices[0] as usize][1]);
                }
                let mut fm = lib.material((pid, &program), &mat.vs_constants, &mat.ps_constants, &globals);
                for (slot_reg, &slot) in mat.texture_slots.iter().enumerate() {
                    let Some(t) = number.and_then(|n| pvs.texture(n, slot)) else { continue };
                    let h = image_cache
                        .entry(t.file_id)
                        .or_insert_with(|| read_dds(&tex_dir.join(format!("{:08x}.dds", t.file_id))).map(|i| images.add(i)))
                        .clone();
                    if let (Some(h), Some(s)) = (h, fm.slot_mut(slot_reg as u32)) {
                        *s = Some(h);
                    }
                }
                commands.spawn((Mesh3d(meshes.add(m)), MeshMaterial3d(materials.add(fm)), Transform::IDENTITY));
                n_meshes += 1;
            }
        }
    }
    info!("fxview: {n_models} models, {n_meshes} meshes, {n_tris} triangles, {} textures", image_cache.values().filter(|h| h.is_some()).count());
    let ground = if ground == f32::MIN { 100.0 } else { ground };
    commands.spawn((
        Camera3d::default(),
        bevy::core_pipeline::tonemapping::Tonemapping::None,
        Projection::Perspective(PerspectiveProjection { far: 20_000.0, fov: 60f32.to_radians(), ..default() }),
        Transform {
            translation: Vec3::new(args.centre.x, ground + args.height, args.centre.y),
            rotation: Quat::from_euler(EulerRot::YXZ, args.yaw.to_radians(), args.pitch.to_radians(), 0.0),
            ..default()
        },
    ));
}

/// One Bevy mesh per rmb mesh, with every attribute the program reads.
fn build_mesh(sub: &rmb::SubModel, mesh: &rmb::Mesh, decl: &fxobj::VertexDecl, wanted: &[u32]) -> Option<Mesh> {
    let mut remap = HashMap::new();
    let mut order = Vec::new();
    let indices: Vec<u32> = mesh
        .indices
        .iter()
        .map(|&i| {
            *remap.entry(i).or_insert_with(|| {
                order.push(i as usize);
                (order.len() - 1) as u32
            })
        })
        .collect();
    if indices.is_empty() {
        return None;
    }
    let stride = sub.stride;
    let vd = &sub.vertex_data;
    let be16 = |o: usize| u16::from_be_bytes([vd[o], vd[o + 1]]);
    let be32 = |o: usize| u32::from_be_bytes(vd[o..o + 4].try_into().unwrap());
    let el = |u: u8, i: u8| decl.find(u, i).filter(|e| e.offset as usize + e.size() <= stride);
    let uvos = mesh.uv_offset_scale;
    let uv = |idx: u8| -> Vec<[f32; 2]> {
        match el(fxobj::USAGE_TEXCOORD, idx) {
            Some(e) => order
                .iter()
                .map(|&v| {
                    let o = v * stride + e.offset as usize;
                    let (u0, v0) = (be16(o) as f32 / 65535.0, be16(o + 2) as f32 / 65535.0);
                    [uvos[0] + u0 * uvos[2], uvos[1] + v0 * uvos[3]]
                })
                .collect(),
            None => vec![[0.0; 2]; order.len()],
        }
    };
    let dec = |u: u8| -> Vec<[f32; 3]> {
        match el(u, 0) {
            Some(e) if e.decl_type == fxobj::TYPE_DEC3N => order.iter().map(|&v| fxobj::dec3n(be32(v * stride + e.offset as usize))).collect(),
            Some(e) if e.decl_type == fxobj::TYPE_FLOAT3 => order
                .iter()
                .map(|&v| {
                    let o = v * stride + e.offset as usize;
                    [f32::from_bits(be32(o)), f32::from_bits(be32(o + 4)), f32::from_bits(be32(o + 8))]
                })
                .collect(),
            _ => vec![[0.0, 1.0, 0.0]; order.len()],
        }
    };
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, order.iter().map(|&v| sub.positions[v]).collect::<Vec<_>>());
    for &loc in wanted {
        match loc {
            0 => {}
            1 => m.insert_attribute(Mesh::ATTRIBUTE_NORMAL, dec(fxobj::USAGE_NORMAL)),
            2 => m.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv(0)),
            3 => m.insert_attribute(Mesh::ATTRIBUTE_UV_1, uv(1)),
            4 => m.insert_attribute(ATTRIBUTE_UV2, uv(2)),
            5 => m.insert_attribute(ATTRIBUTE_TANGENT, dec(fxobj::USAGE_TANGENT)),
            6 => {
                let c: Vec<[u8; 4]> = match el(fxobj::USAGE_COLOR, 0) {
                    Some(e) => order
                        .iter()
                        .map(|&v| {
                            let o = v * stride + e.offset as usize;
                            [vd[o], vd[o + 1], vd[o + 2], vd[o + 3]]
                        })
                        .collect(),
                    None => vec![[255; 4]; order.len()],
                };
                m.insert_attribute(ATTRIBUTE_COLOR, bevy::mesh::VertexAttributeValues::Unorm8x4(c));
            }
            _ => return None,
        }
    }
    m.insert_indices(Indices::U32(indices));
    Some(m)
}
