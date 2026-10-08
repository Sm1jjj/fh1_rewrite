//! fxtiles — render installed FH1TILE4 scenery tiles with the game's shaders (the same path the
//! engine uses through `fh1_render::scenery`).
//!
//! `fxtiles <disc dir> <install assets/private dir> [x z radius] [--shot out.png] [--yaw deg]
//!  [--pitch deg] [--height m] [--time minutes]`

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use fh1_render::scenery::{parse_tile, SceneryMaterials, STANDIN};
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
    post: bool,
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let num = |i: usize, d: f32| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let f = |n: &str, d: f32| flag(n).and_then(|s| s.parse().ok()).unwrap_or(d);
    let args = Args {
        disc: PathBuf::from(&a[1]),
        assets: PathBuf::from(&a[2]),
        centre: Vec2::new(num(3, 1584.97), num(4, 1843.62)),
        radius: num(5, 600.0),
        shot: flag("--shot"),
        yaw: f("--yaw", 0.0),
        pitch: f("--pitch", -10.0),
        height: f("--height", 6.0),
        minutes: f("--time", 720.0),
        post: a.iter().any(|x| x == "--post"),
    };
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { title: "fxtiles".into(), resolution: (1280u32, 720u32).into(), ..default() }),
            ..default()
        }))
        .add_plugins(Fh1RenderPlugin)
        .insert_resource(args)
        .insert_resource(ClearColor(Color::srgb(0.5, 0.6, 0.7)))
        .add_systems(Startup, load)
        .add_systems(Update, (poll, lighting, screenshot))
        .run();
}

#[allow(clippy::too_many_arguments)]
fn load(
    mut commands: Commands,
    args: Res<Args>,
    mut lib: ResMut<FxLibrary>,
    mut globals: ResMut<FxGlobals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FxMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    if args.post {
        // FH1 post chain: materials write the game's sqrt-encoded colour into an HDR target.
        lib.raw_output = true;
        let dynpost = std::env::var("FX_DYNAMICPOST").map(PathBuf::from).unwrap_or_else(|_| args.assets.join("dynamicpost"));
        let paths = fh1_render::postfx::PostPaths {
            xex_dir: args.assets.join("shaders/xex"),
            timeofday: args.disc.join("media/tracks/colorado/Ribbon_00/TimeOfDay.xml"),
            track_settings: args.disc.join("media/tracks/colorado/TrackSettings.xml"),
            templates_dir: dynpost.join("Tracks/Colorado"),
            luts_dir: dynpost.join("ColourGradingMaps"),
            zones: args.disc.join("media/tracks/colorado/Ribbon_00/PostProcessingZones_Safe.xml"),
            default_luts: (args.disc.join("media/tracks/colorado/ColorGradingLookup.dds"), args.disc.join("media/tracks/colorado/ColorGradingLookup_Night.dds")),
            scenery_dir: args.assets.join("scenery/colorado"),
        };
        match fh1_render::postfx::setup(&paths, &mut shaders, &mut images) {
            Some((chain, post)) => {
                commands.insert_resource(chain);
                commands.insert_resource(post);
            }
            None => warn!("post chain shaders missing"),
        }
        let mut t = fh1_render::lighting::FxTimeOfDay::load(&args.disc.join("media/tracks/colorado/Ribbon_00/TimeOfDayA.xml"), args.minutes).unwrap();
        t.rate_scale = 0.0;
        commands.insert_resource(t);
    }
    let dir = args.assets.join("scenery/colorado");
    let mut sm = SceneryMaterials::load(&dir, &args.assets.join("shaders/track")).expect("materials.json");
    // FX_CULL=on|flip: use the effects' cull modes (as authored, or with the winding swapped).
    match std::env::var("FX_CULL").as_deref() {
        Ok("on") => sm.no_cull = false,
        Ok("flip") => {
            sm.no_cull = false;
            sm.flip_cull = true;
        }
        _ => {}
    }
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
    let size = index["tile_size"].as_f64().unwrap() as f32;
    let (mut batches, mut ground) = (0, f32::MIN);
    for t in index["tiles"].as_array().unwrap() {
        let (x, z) = (t["x"].as_i64().unwrap() as f32, t["z"].as_i64().unwrap() as f32);
        let c = Vec2::new((x + 0.5) * size, (z + 0.5) * size);
        if c.distance(args.centre) > args.radius {
            continue;
        }
        let Some(tile) = std::fs::read(dir.join(t["file"].as_str().unwrap())).ok().and_then(|b| parse_tile(&b)) else { continue };
        for b in tile {
            if b.flags & STANDIN != 0 {
                continue;
            }
            if let Some(bevy::mesh::VertexAttributeValues::Float32x3(p)) = b.mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
                for v in p.iter().filter(|v| Vec2::new(v[0], v[2]).distance(args.centre) < 15.0) {
                    ground = ground.max(v[1]);
                }
            }
            let Some(m) = sm.material(b.material, &mut lib, &mut globals, &mut shaders, &mut materials) else { continue };
            commands.spawn((Mesh3d(meshes.add(b.mesh)), MeshMaterial3d(m)));
            batches += 1;
        }
    }
    info!("fxtiles: {batches} batches");
    commands.insert_resource(sm);
    let ground = if ground == f32::MIN { 100.0 } else { ground };
    let mut cam = commands.spawn((
        Camera3d::default(),
        bevy::core_pipeline::tonemapping::Tonemapping::None,
        Projection::Perspective(PerspectiveProjection { far: 20_000.0, fov: 60f32.to_radians(), ..default() }),
        Transform {
            translation: Vec3::new(args.centre.x, ground + args.height, args.centre.y),
            rotation: Quat::from_euler(EulerRot::YXZ, args.yaw.to_radians(), args.pitch.to_radians(), 0.0),
            ..default()
        },
    ));
    if args.post {
        cam.insert((bevy::camera::Hdr, fh1_render::post::FxPostCamera));
    }
    // FX_EXTRA_CAMS: mimic the engine's extra cameras (UI overlay + render-target minimap).
    if std::env::var("FX_EXTRA_CAMS").is_ok() {
        commands.spawn((Camera2d, Camera { order: 10, clear_color: ClearColorConfig::None, ..default() }, bevy::camera::Hdr));
        let size = bevy::render::render_resource::Extent3d { width: 256, height: 256, depth_or_array_layers: 1 };
        let mut img = Image::new_fill(size, bevy::render::render_resource::TextureDimension::D2, &[0, 0, 0, 255], bevy::render::render_resource::TextureFormat::Bgra8UnormSrgb, bevy::asset::RenderAssetUsages::default());
        img.texture_descriptor.usage = bevy::render::render_resource::TextureUsages::TEXTURE_BINDING | bevy::render::render_resource::TextureUsages::COPY_DST | bevy::render::render_resource::TextureUsages::RENDER_ATTACHMENT;
        let target = images.add(img);
        commands.spawn((Camera3d::default(), Camera { order: -1, ..default() }, bevy::camera::RenderTarget::Image(target.into()), Transform::from_xyz(args.centre.x, 400.0, args.centre.y).looking_at(Vec3::new(args.centre.x, 0.0, args.centre.y), Vec3::Z)));
    }
}

fn poll(sm: Option<ResMut<SceneryMaterials>>, mut images: ResMut<Assets<Image>>, mut materials: ResMut<Assets<FxMaterial>>) {
    if let Some(mut sm) = sm {
        sm.poll(&mut images, &mut materials);
    }
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
    if *frame == 240 {
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(path.clone()));
    }
    if *frame == 250 {
        exit.write(AppExit::Success);
    }
}
