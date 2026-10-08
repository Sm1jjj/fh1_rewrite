//! fxcar — render a car with the game's car shaders (shaders_v16) from the installed `.fxcar`
//! streams and ShaderSettings chain.
//!
//! `fxcar <install assets/private dir> [CAR] [--shot out.png] [--yaw deg] [--dist m]`

use std::path::PathBuf;

use bevy::camera::visibility::NoFrustumCulling;
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use fh1_render::car::load_body;
use fh1_render::car_material::FxCarMaterial;
use fh1_render::{Fh1RenderPlugin, FxCarGlobals, FxLibrary};

#[derive(Resource, Clone)]
struct Args {
    assets: PathBuf,
    car: String,
    shot: Option<String>,
    yaw: f32,
    dist: f32,
    paint: Option<u32>,
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let args = Args {
        assets: PathBuf::from(&a[1]),
        car: a.get(2).filter(|s| !s.starts_with("--")).cloned().unwrap_or("ALF_8C_08".into()),
        shot: flag("--shot"),
        yaw: flag("--yaw").and_then(|s| s.parse().ok()).unwrap_or(35.0),
        dist: flag("--dist").and_then(|s| s.parse().ok()).unwrap_or(7.0),
        paint: flag("--paint").and_then(|s| u32::from_str_radix(&s, 16).ok()),
    };
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { title: "fxcar".into(), resolution: (1280u32, 720u32).into(), ..default() }),
            ..default()
        }))
        .add_plugins(Fh1RenderPlugin)
        .insert_resource(args)
        .insert_resource(ClearColor(Color::srgb(0.45, 0.5, 0.55)))
        .add_systems(Startup, load)
        .add_systems(Update, screenshot)
        .run();
}

fn load(
    mut commands: Commands,
    args: Res<Args>,
    mut lib: ResMut<FxLibrary>,
    mut globals: ResMut<FxCarGlobals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FxCarMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    // Colorado's time-of-day lighting at noon drives the car SH constants (fh1-render lighting).
    if let Some(t) = fh1_render::lighting::FxTimeOfDay::load(&args.assets.join("tracks/colorado/TimeOfDayA.xml"), 720.0) {
        commands.insert_resource(t);
    }
    // --paint RRGGBB (gamedb Combo_Colors.RGB; ALF_8C_08 stock = 8D0000, metallic, sequence 1).
    let paint = args.paint.map(|v| (v, true, 1));
    let parts = load_body(&args.assets, &args.car, "colorado", paint, &mut lib, &mut globals, &mut shaders, &mut images).expect("car fx files");
    info!("fxcar: {} body parts", parts.len());
    for (mesh, mut m) in parts {
        if std::env::var("FX_NOCULL").is_ok() {
            m.no_cull = true;
        }
        commands.spawn((Mesh3d(meshes.add(mesh)), MeshMaterial3d(materials.add(m)), NoFrustumCulling));
    }
    let yaw = args.yaw.to_radians();
    let eye = Vec3::new(yaw.sin() * args.dist, 1.6, yaw.cos() * args.dist);
    commands.spawn((
        Camera3d::default(),
        bevy::core_pipeline::tonemapping::Tonemapping::None,
        Projection::Perspective(PerspectiveProjection { fov: 45f32.to_radians(), ..default() }),
        Transform::from_translation(eye).looking_at(Vec3::new(0.0, 0.6, 0.0), Vec3::Y),
    ));
}

fn screenshot(mut commands: Commands, args: Res<Args>, mut frame: Local<u32>, mut exit: MessageWriter<AppExit>) {
    let Some(path) = &args.shot else { return };
    *frame += 1;
    if *frame == 120 {
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(path.clone()));
    }
    if *frame == 220 {
        exit.write(AppExit::Success);
    }
}
