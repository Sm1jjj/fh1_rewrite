//! fh1-engine: first playable slice — one real FH1 car on a flat test plane.
//!
//! ```text
//! fh1-engine [--data <dir>] [--car <MediaName>] [--track colorado|flat]
//! ```
//! Keyboard: W/S or arrows throttle/brake (S reverses when stopped), A/D steer, Space handbrake,
//! R reset, C camera, N/P next/previous car, T traction control, G next start location,
//! V collision overlay, Esc pause menu (see `ui`).
//! Multiplayer (off unless set): `FH1_SERVER=host:7777` `FH1_NAME=name` (docs/MULTIPLAYER.md).
//! Gamepad (XInput): RT throttle, LT brake, left stick steer, A handbrake, Y reset,
//! RB camera, B/X shift up/down, LB clutch, Back hold rewind (ui/assists.rs), Start pause menu.
//! Camera: right stick / mouse drag free look, wheel or D-pad zoom, F photo mode (also in the
//! pause menu; A in photo mode or F12 saves a screenshot to screenshots/).

#[path = "ai/plugin.rs"]
mod ai_plugin;
mod logpipe;
#[path = "traffic/plugin.rs"]
mod traffic_plugin;
mod anim;
mod audio;
mod backfire;
mod camera;
mod diag;
mod grass;
mod crowd;
mod effects;
mod effects_surface;
mod fx_overrides;
mod imported;
mod mipgen;
mod objects;
mod perf;
mod race;
mod progression;
mod radio;
mod scenery;
mod skidmarks;
mod net;
mod smoke;
mod smash;
mod track;
mod ui;

use fh1_engine::{data, vehicle};

use std::path::PathBuf;

use bevy::gltf::GltfAssetLabel;
use bevy::world_serialization::WorldAssetRoot;
use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::prelude::*;

use bevy::render::view::screenshot::{save_to_disk, Screenshot};

use camera::CameraRig;
use track::Track;
use data::CarData;
use vehicle::{Controls, Vehicle};

const PHYSICS_HZ: f64 = 120.0;
const SUBSTEPS: usize = 4;

/// `FH1_WINDOW=borderless` (borderless fullscreen on the current monitor), `fullscreen` (exclusive)
/// or `windowed` / unset (a 1280×720 window; launch.bat's test default, and agent / autotest runs).
fn game_window() -> Window {
    use bevy::window::{MonitorSelection, VideoModeSelection, WindowMode};
    let mode = match std::env::var("FH1_WINDOW").as_deref() {
        Ok("borderless") => WindowMode::BorderlessFullscreen(MonitorSelection::Current),
        Ok("fullscreen") => WindowMode::Fullscreen(MonitorSelection::Current, VideoModeSelection::Current),
        _ => WindowMode::Windowed,
    };
    let mut window = Window { title: "FH1 Rewrite".into(), mode, ..default() };
    // Two clients side by side. Fullscreen modes keep Bevy's own size.
    if matches!(mode, WindowMode::Windowed) {
        window.resolution = (1280, 720).into();
    }
    window
}

#[derive(Resource)]
struct Garage {
    assets: PathBuf,
    cars: Vec<String>,
    current: usize,
}

#[derive(Resource, Default)]
struct Input(Controls);

#[derive(Component)]
pub struct Car(pub Vehicle);

/// The glTF scene of the car (child of the physics body, offset by the centre of mass).
#[derive(Component)]
struct CarModel;

/// Wheel visual: index LF, RF, LR, RR, plus the hub's rest translation and base scale.
#[derive(Component)]
struct WheelVisual {
    index: usize,
    hub: Vec3,
    scale: Vec3,
}

/// Which of the track's start locations to use.
#[derive(Resource, Default)]
struct SpawnIndex(usize);

/// Car (and its Combo_Colors paint sequence) of a plain launch: the red Corrado of the Xenia reference shots.
const DEFAULT_CAR: (&str, u32) = ("VW_Corrado_95", 2);

/// Last pose with all four wheels on the ground, to recover a car that falls off the world.
#[derive(Resource, Default)]
struct SafePose {
    point: Option<(Vec3, f32)>,
    timer: f32,
}

fn main() -> AppExit {
    // Before anything logs: stdout/stderr through a buffered pipe so no game thread waits on the log file (logpipe.rs).
    logpipe::install();
    let mut data_dir = PathBuf::from("data");
    let mut car = None;
    let mut track_name: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data" => data_dir = args.next().expect("--data <dir>").into(),
            "--car" => car = args.next(),
            "--track" => track_name = Some(args.next().expect("--track colorado|flat|<imported map>")),
            _ => {}
        }
    }
    // Renderer (Options > Shaders; FH1_RENDERER overrides): remaster by default, the game's own shaders on request.
    ui::apply_renderer_setting(&data_dir.join("settings.json"));
    let assets = match data::private_assets(&data_dir) {
        Ok(p) => std::path::absolute(p).unwrap(),
        Err(e) => {
            eprintln!("{e:#}");
            return AppExit::error();
        }
    };
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(assets.join("cars/index.json")).expect("cars/index.json")).unwrap();
    let cars: Vec<String> = index
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["has_model"].as_bool() == Some(true))
        .filter_map(|c| c["media_name"].as_str().map(str::to_owned))
        .collect();
    let mut cars = cars;
    // Imported games in the index.json contract (FH2, FM4; ui/browser.rs).
    cars.extend(ui::browser::imported_car_folders(&assets));
    let current = car
        .and_then(|c| cars.iter().position(|x| x.eq_ignore_ascii_case(&c) || x.rsplit('/').next().is_some_and(|t| t.eq_ignore_ascii_case(&c))))
        .or_else(|| cars.iter().position(|x| x == DEFAULT_CAR.0))
        .or_else(|| cars.iter().position(|x| x == "ALF_8C_08"))
        .unwrap_or(0);

    // L1b main menu (ui/launch.rs, ui/world_load.rs): without --track / automation the world is chosen first and loaded
    // in-process after the choice (world_load::apply_choice); until then a placeholder (test plane, no scenery, no car).
    // Otherwise: --track, else settings.json `map` (not for agent runs), else Colorado, loaded now as before.
    let menu_first = ui::world_load::menu_first();
    let world = if menu_first {
        ui::world_load::LoadedWorld::placeholder()
    } else {
        ui::world_load::load_world(&assets, &ui::world_load::track_name(&data_dir, track_name))
    };
    let ui::world_load::LoadedWorld { track, events, scenery, imported: imported_scenery } = world;
    let home = track.home;
    let mut app = App::new();
    if menu_first {
        app.insert_resource(ui::world_load::PendingWorld::default());
    }
    if let Some(sc) = scenery {
        app.insert_resource(sc);
    }
    if let Some(sc) = imported_scenery {
        app.insert_resource(sc);
    }
    // FH1 lighting for the game shaders: Colorado's time-of-day curves. 16:00 like the game, clock at the
    // game's TimeSpeed rate (FH1_TOD / FH1_TOD_SPEED override; frozen under FH1_SHOT).
    if let Some(t) = fh1_render::lighting::FxTimeOfDay::load(&track::time_of_day_path(&assets, &track.id), 960.0) {
        let mut t = t;
        t.rate_scale = 1.0;
        app.insert_resource(t);
    }
    // FH1's own post chain (bloom, adaptation, filmic, grading LUTs, vignette); neutral without the files.
    app.insert_resource(track::post_config(&assets, &track.id));
    app.insert_resource(camera::CameraData::load(&assets));
    fh1_remaster::pre_default_plugins(&mut app);
    // GPU upload budget per frame (2026-10-08, user: small skips with steady fps; log 133825 hitches = PrepareAssets
    // 50-93 ms while scenery streams in): Bevy uploads every new mesh/texture in the frame it appears; the limiter spreads a
    // burst over frames (a soft cap: whole assets only). FH1_UPLOAD_MB=n MB per frame. OFF by default since log 135653: the
    // hitches are mesh allocation (allocate_and_free_meshes), not images, and with the cap on the festival showed missing
    // barrier/road textures (a deferred image never reached its material).
    let upload_mb = std::env::var("FH1_UPLOAD_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if upload_mb > 0 {
        app.insert_resource(bevy::render::render_asset::RenderAssetBytesPerFrame::new(upload_mb << 20));
    }
    app
        .add_plugins(
            DefaultPlugins
                .set(AssetPlugin { file_path: assets.to_string_lossy().into_owned(), ..default() })
                // Stall watchdog: tracks running systems by their trace spans (perf/watchdog.rs).
                .set(bevy::log::LogPlugin { custom_layer: perf::system_span_layer, ..default() })
                .set(WindowPlugin {
                    primary_window: Some(game_window()),
                    ..default()
                })
                .set(task_pools()),
        )
        // The game's own shaders for the scenery (fh1-render, docs/SHADERS.md).
        .add_plugins(fh1_render::Fh1RenderPlugin)
        .add_plugins(fh1_render::car::FxCarPlugin)
        .add_plugins(fh1_remaster::RemasterPlugin)
        .insert_resource(Garage { assets, cars, current })
        .insert_resource(track)
        // FH1_SPAWN=<n>: start at the track's n-th start location (autotests); default = the track's home.
        .insert_resource(SpawnIndex(std::env::var("FH1_SPAWN").ok().and_then(|v| v.parse().ok()).unwrap_or(home)))
        .init_resource::<SafePose>()
        .insert_resource(Time::<Fixed>::from_hz(PHYSICS_HZ))
        .insert_resource(ClearColor(Color::srgb(0.53, 0.72, 0.9)))
        .init_resource::<Input>()
        .init_resource::<CameraRig>()
        .add_plugins(audio::CarAudioPlugin)
        .add_plugins(backfire::BackfirePlugin)
        .add_plugins(smoke::SmokePlugin)
        .add_plugins(radio::RadioPlugin)
        .add_plugins(grass::GrassPlugin)
        .add_plugins(fx_overrides::FxOverridesPlugin)
        .add_plugins(imported::ImportedSceneryPlugin)
        .add_plugins(mipgen::MipGenPlugin)
        .add_plugins(anim::AnimPlugin)
        .add_plugins(ai_plugin::AiPlugin)
        .add_plugins(net::NetPlugin)
        .add_plugins(traffic_plugin::TrafficPlugin)
        .add_plugins(skidmarks::DrivingEffectsPlugin)
        .add_plugins(crowd::CrowdPlugin)
        .add_plugins(perf::PerfPlugin)
        .add_plugins(scenery::P2Plugin)
        .add_plugins(ui::UiPlugin { settings_path: data_dir.join("settings.json") })
        .add_plugins(diag::DiagPlugin)
        .init_resource::<PendingCar>()
        .add_systems(Startup, (setup_world, (track::setup, spawn_car).run_if(ui::world_load::world_ready)))
        .add_systems(
            Update,
            (
                read_input.run_if(ui::driving).run_if(|ui: Option<Res<progression::screen::CareerUi>>| ui.is_none_or(|u| !u.open)),
                switch_car.run_if(ui::driving),
                apply_actions,
                finish_car,
                update_lamps,
                tag_wheels,
                sync_visuals,
                update_drop_shadow,
                camera::camera_input.run_if(ui::menu_closed),
                camera::follow_camera,
            )
                .chain(),
        )
        .add_systems(FixedUpdate, step_physics)
        .add_systems(FixedUpdate, physics_settled.after(step_physics))
        .add_systems(Update, camera::drive_mirror.after(camera::follow_camera))
        .add_systems(Update, (scenery::stream, toggle_collision_view.run_if(ui::driving)))
        .init_resource::<effects_surface::SurfaceFx>()
        .add_systems(Update, effects_surface::emit_surface_fx.run_if(ui::driving))
        .add_systems(Update, effects::tyre_smoke.run_if(ui::driving))
        .add_plugins(progression::ProgressionPlugin { path: data_dir.join("profile.json") })
        .insert_resource(events)
        .init_resource::<race::RaceState>()
        .add_systems(Startup, race::spawn_race_hud)
        .add_systems(Update, (race::race_update, race::race_objects, race::race_markers).run_if(ui::driving))
        .add_systems(Update, race::race_hud)
        .add_systems(Update, race::race_nav)
        .add_systems(Update, objects::teleport.before(scenery::stream))
        .init_resource::<smash::PropCollision>()
        .add_systems(Update, (smash::update.after(scenery::stream), smash::test_drive))
        .add_systems(Update, autotest);
    // Mesh slabs (2026-10-08, 47's analysis of log 133825: the streaming hitches are bevy's allocate_and_free_meshes, which
    // regrows a slab by x1.5 from 1 MiB = a new buffer + a full copy each time the resident scenery set grows): start at
    // FH1_MESH_SLAB_MB (16; global, so every vertex layout's slab starts that big: watch vram_mb) and double.
    // FH1_MESH_SLAB=0 = Bevy's defaults (1 MiB, x1.5).
    if !std::env::var("FH1_MESH_SLAB").is_ok_and(|v| v == "0") {
        if let Some(render_app) = app.get_sub_app_mut(bevy::render::RenderApp) {
            let mut s = bevy::render::mesh::allocator::MeshAllocatorSettings::default();
            let mb = std::env::var("FH1_MESH_SLAB_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(16).max(1);
            s.min_slab_size = mb << 20;
            s.growth_factor = 2.0;
            render_app.insert_resource(s);
        }
    }
    app.run()
}

/// Thread pools (2026-10-08 perf, e1): the render thread rose and fell with the main world's load (r = 0.92), because both
/// worlds' parallel systems share the Compute pool, which Bevy sizes at half the cores. IO 1-2 and AsyncCompute 1-3
/// threads leave the rest (11 of 16) to Compute. FH1_POOL=0 = Bevy's default split.
fn task_pools() -> bevy::app::TaskPoolPlugin {
    use bevy::app::{TaskPoolOptions, TaskPoolPlugin, TaskPoolThreadAssignmentPolicy};
    if std::env::var("FH1_POOL").is_ok_and(|v| v == "0") {
        return TaskPoolPlugin::default();
    }
    let policy = |max_threads: usize, percent: f32| TaskPoolThreadAssignmentPolicy { min_threads: 1, max_threads, percent, on_thread_spawn: None, on_thread_destroy: None };
    TaskPoolPlugin { task_pool_options: TaskPoolOptions { io: policy(2, 0.125), async_compute: policy(3, 0.2), ..default() } }
}

fn setup_world(mut commands: Commands) {
    commands.spawn((
        DirectionalLight { illuminance: 9_000.0, shadow_maps_enabled: true, ..default() },
        Transform::from_xyz(40.0, 80.0, 30.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.insert_resource(GlobalAmbientLight { brightness: 600.0, ..default() });

    commands.spawn((
        Camera3d::default(),
        // The game shaders output FH1's linear colour; fh1-render's FH1 post chain will tonemap it.
        bevy::core_pipeline::tonemapping::Tonemapping::None,
        // HDR + the FH1 post chain. Every camera drawing to the window must match (ui::scene::UiCamera is Hdr too).
        bevy::camera::Hdr,
        fh1_render::post::FxPostCamera,
        // The world is ~13 km across.
        Projection::Perspective(PerspectiveProjection { far: 20_000.0, ..default() }),
        Transform::from_xyz(0.0, 3.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// A car switch whose physics files (physics.json, model.json, the upgrades doc) load on a task, so a slow read can't
/// park the frame (2026-10-08 freezes: a 28.9 s stall ended with the car-switch log line). (car index, load).
#[derive(Resource, Default)]
struct PendingCar(Option<(usize, bevy::tasks::Task<anyhow::Result<CarData>>)>);

/// FH1_CAR_ASYNC_LOAD=0: load the car in the requesting system, as before.
fn car_async_load() -> bool {
    !std::env::var("FH1_CAR_ASYNC_LOAD").is_ok_and(|v| v == "0")
}

/// Starts loading `garage.current`; a newer request replaces (cancels) an unfinished one.
fn request_car(garage: &Garage, track: &str, looks: &ui::customize::CarLooks, pending: &mut PendingCar) {
    let name = garage.cars[garage.current].clone();
    let dir = garage.assets.join("cars").join(&name);
    let patch = looks.physics_patcher(&name);
    let (assets, car, track) = (garage.assets.clone(), name.clone(), track.to_owned());
    pending.0 = Some((garage.current, bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
        let data = CarData::load_with(&dir, patch);
        // Car files into RAM (fh1-render files.rs) so the body/cockpit/wheel systems never read the disk.
        fh1_render::files::prefetch_car(&assets, &car, &track);
        data
    })));
}

/// Spawns the car once its task is done (if it is still the chosen car). The old car stays until then.
fn finish_car(mut pending: ResMut<PendingCar>, commands: Commands, garage: Res<Garage>, track: Res<Track>, spawn: Res<SpawnIndex>, asset_server: Res<AssetServer>, existing: Query<Entity, With<Car>>, looks: Res<ui::customize::CarLooks>) {
    if !pending.0.as_ref().is_some_and(|(_, t)| t.is_finished()) {
        return;
    }
    let (index, task) = pending.0.take().unwrap();
    if index != garage.current {
        return;
    }
    match bevy::tasks::block_on(bevy::tasks::futures_lite::future::poll_once(task)) {
        Some(Ok(data)) => place_car(commands, &garage, &track, &spawn, &asset_server, &existing, &looks, data),
        Some(Err(e)) => error!("{}: {e:#}", garage.cars[index]),
        None => {}
    }
}

fn spawn_car(commands: Commands, garage: Res<Garage>, track: Res<Track>, spawn: Res<SpawnIndex>, asset_server: Res<AssetServer>, existing: Query<Entity, With<Car>>, looks: Res<ui::customize::CarLooks>) {
    let name = &garage.cars[garage.current];
    // Customize upgrades edit physics.json before it is read (ui/customize_upgrades.rs; FH1_UPGRADES=0 = stock).
    match CarData::load_with(&garage.assets.join("cars").join(name), |p| looks.patch_physics(name, p)) {
        Ok(data) => place_car(commands, &garage, &track, &spawn, &asset_server, &existing, &looks, data),
        Err(e) => error!("{name}: {e:#}"),
    }
}

/// Replaces the player car with `data`. The old car is despawned only once the new one has loaded.
#[allow(clippy::too_many_arguments)]
fn place_car(mut commands: Commands, garage: &Garage, track: &Track, spawn: &SpawnIndex, asset_server: &AssetServer, existing: &Query<Entity, With<Car>>, looks: &ui::customize::CarLooks, data: CarData) {
    for e in existing {
        commands.entity(e).despawn();
    }
    let name = &garage.cars[garage.current];
    info!(
        "{name}: {:.0} kg, {:.0} N·m, redline {:.0} rpm, {} gears, drive type {}",
        data.mass,
        data.torque_scale,
        data.redline_rpm,
        data.gears.len(),
        data.drive_type
    );
    let (point, yaw) = track.spawns[spawn.0 % track.spawns.len()];
    // Headlight lamp midpoint in model space (+Y up, front -Z): above and ahead of the front hubs (INFERRED;
    // the game reads car-local lamp positions, fh1-render headlight.rs).
    let lamp_hub = data.hubs[0];
    let mut vehicle = Vehicle::new(data, point);
    vehicle.place(point, yaw);
    let cg = vehicle.cg_model;
    commands
        .spawn((
            Car(vehicle),
            Transform::default(),
            Visibility::default(),
            imported::SceneryFocus,
            fh1_render::reflect::EnvCubeAnchor,
            fh1_render::headlight::FxHeadlightSource { player: true, lamp: Vec3::new(0.0, lamp_hub[1] + 0.25, lamp_hub[2] - 0.8) - cg },
            fh1_render::car_shadow::drop_shadow::FxDropShadow::default(),
            fh1_render::car::FxCarLamps::default(),
            fh1_engine::ai::PlayerCar,
        ))
        .with_children(|p| spawn_body(p, garage, &track.id, name, cg, asset_server, looks));
}

/// The player car's visual body under its `Car` root: the glTF scene, drawn through the game's car shaders, with the
/// garage look (ui/customize.rs, which also calls this to preview a look live).
fn spawn_body(p: &mut ChildSpawnerCommands, garage: &Garage, track_id: &str, name: &str, cg: Vec3, asset_server: &AssetServer, looks: &ui::customize::CarLooks) {
    let scene = asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("cars/{name}/model.gltf")));
    let mut body = p.spawn((
        WorldAssetRoot(scene),
        Transform::from_translation(-cg),
        CarModel,
        // The body through the game's car shaders (fh1-render car.rs; FH1_CARFX=0 = glTF body).
        fh1_render::car::FxCarBody { assets: garage.assets.clone(), car: name.to_owned(), track: track_id.to_owned() },
    ));
    // The default car wears the colour of the Xenia reference shots (a Combo_Colors sequence, not the stock one).
    if name == DEFAULT_CAR.0 {
        body.insert(fh1_render::car::FxCarPaint { sequence: DEFAULT_CAR.1 });
    }
    looks.apply(&mut body, name);
}

fn read_input(track: Res<Track>, mut settings: ResMut<ui::Settings>, keys: Res<ButtonInput<KeyCode>>, pads: Query<&Gamepad>, mut input: ResMut<Input>, mut cars: Query<&mut Car>, mut cam: ResMut<CameraRig>) {
    let key = |k: KeyCode| if keys.pressed(k) { 1.0f32 } else { 0.0 };
    let mut c = Controls {
        throttle: key(KeyCode::KeyW).max(key(KeyCode::ArrowUp)),
        brake: key(KeyCode::KeyS).max(key(KeyCode::ArrowDown)),
        steer: key(KeyCode::KeyD).max(key(KeyCode::ArrowRight)) - key(KeyCode::KeyA).max(key(KeyCode::ArrowLeft)),
        handbrake: key(KeyCode::Space),
        tcs: settings.tcs,
        abs: settings.abs,
    };
    if keys.just_pressed(KeyCode::KeyT) {
        settings.tcs = !settings.tcs;
    }
    let mut reset = keys.just_pressed(KeyCode::KeyR);
    let mut camera = keys.just_pressed(KeyCode::KeyC);
    for pad in &pads {
        let axis = |a: GamepadAxis| pad.get(a).unwrap_or(0.0);
        let stick = axis(GamepadAxis::LeftStickX);
        if stick.abs() > 0.08 {
            c.steer = stick;
        }
        c.throttle = c.throttle.max(pad.get(GamepadButton::RightTrigger2).unwrap_or(0.0));
        c.brake = c.brake.max(pad.get(GamepadButton::LeftTrigger2).unwrap_or(0.0));
        if pad.pressed(GamepadButton::South) {
            c.handbrake = 1.0;
        }
        reset |= pad.just_pressed(GamepadButton::North);
        // RB cycles the camera, as in FH1 (VERIFIED order: camera/views.rs next_mode).
        camera |= pad.just_pressed(GamepadButton::RightTrigger);
    }
    if std::env::var_os("FH1_AUTODRIVE").is_some() {
        c.throttle = 1.0;
        c.steer = std::env::var("FH1_AUTODRIVE_STEER").ok().and_then(|v| v.parse().ok()).unwrap_or(0.15);
    }
    input.0 = c;
    if reset {
        for mut car in &mut cars {
            car.0.reset(track.ground.as_ref());
        }
    }
    if camera {
        cam.mode = camera::next_mode(cam.mode);
    }
}

/// Carry out pause-menu requests (restart, fast travel, car select).
#[allow(clippy::too_many_arguments)]
fn apply_actions(mut actions: MessageReader<ui::GameAction>, mut garage: ResMut<Garage>, track: Res<Track>, mut spawn: ResMut<SpawnIndex>, mut cars: Query<&mut Car>, commands: Commands, asset_server: Res<AssetServer>, existing: Query<Entity, With<Car>>, looks: Res<ui::customize::CarLooks>, mut pending_car: ResMut<PendingCar>) {
    let mut select = None;
    for a in actions.read() {
        match *a {
            ui::GameAction::Restart => {
                let (point, yaw) = track.spawns[spawn.0 % track.spawns.len()];
                for mut car in &mut cars {
                    car.0.place(point, yaw);
                }
            }
            ui::GameAction::SelectCar(i) => select = Some(i),
            ui::GameAction::FastTravel(i) => {
                spawn.0 = i % track.spawns.len();
                let (point, yaw) = track.spawns[spawn.0];
                for mut car in &mut cars {
                    car.0.place(point, yaw);
                }
            }
        }
    }
    if let Some(i) = select {
        garage.current = i.min(garage.cars.len() - 1);
        if car_async_load() {
            request_car(&garage, &track.id, &looks, &mut pending_car);
        } else {
            spawn_car(commands, garage.into(), track, spawn.into(), asset_server, existing, looks);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn switch_car(keys: Res<ButtonInput<KeyCode>>, mut garage: ResMut<Garage>, track: Res<Track>, mut spawn: ResMut<SpawnIndex>, mut cars: Query<&mut Car>, commands: Commands, asset_server: Res<AssetServer>, existing: Query<Entity, With<Car>>, time: Res<Time<Real>>, mut pending: Local<Option<f32>>, looks: Res<ui::customize::CarLooks>, mut pending_car: ResMut<PendingCar>) {
    // G: next start location (D-pad left/right belong to the radio).
    let next_spawn = keys.just_pressed(KeyCode::KeyG);
    if next_spawn {
        spawn.0 = (spawn.0 + 1) % track.spawns.len();
        let (point, yaw) = track.spawns[spawn.0];
        for mut car in &mut cars {
            car.0.place(point, yaw);
        }
    }
    let mut delta = 0i32;
    if keys.just_pressed(KeyCode::KeyN) {
        delta = 1;
    }
    if keys.just_pressed(KeyCode::KeyP) {
        delta = -1;
    }
    if delta != 0 {
        let n = garage.cars.len() as i32;
        garage.current = ((garage.current as i32 + delta).rem_euclid(n)) as usize;
        info!("car {} / {}: {}", garage.current + 1, n, garage.cars[garage.current]);
        *pending = Some(time.elapsed_secs());
    }
    // P6: N/P spawned every car pressed past in full (130-270 ms hitches). Spawn once the keys rest 0.35 s. FH1_CAR_DEBOUNCE=0 = old.
    let rest = std::env::var("FH1_CAR_DEBOUNCE").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.35);
    if pending.is_some_and(|t| time.elapsed_secs() - t >= rest) {
        *pending = None;
        if car_async_load() {
            request_car(&garage, &track.id, &looks, &mut pending_car);
        } else {
            spawn_car(commands, garage.into(), track, spawn.into(), asset_server, existing, looks);
        }
    }
}

/// Once the glTF scene has spawned, find the `wheel_XX` nodes so they can spin and steer.
fn tag_wheels(mut commands: Commands, named: Query<(Entity, &Name, &Transform), (Without<WheelVisual>, Without<fh1_engine::ai::AiWheel>)>) {
    const NAMES: [&str; 4] = ["wheel_LF", "wheel_RF", "wheel_LR", "wheel_RR"];
    for (e, name, t) in &named {
        if let Some(index) = NAMES.iter().position(|n| name.as_str() == *n) {
            commands.entity(e).insert(WheelVisual { index, hub: t.translation, scale: t.scale });
        }
    }
}

/// Runs after the player's step, the AI, traffic and rewind. Multiplayer bumps the car after this.
fn physics_settled() {}

fn step_physics(
    input: Res<Input>,
    track: Res<Track>,
    mut safe: ResMut<SafePose>,
    mut cars: Query<&mut Car>,
    time: Res<Time<Fixed>>,
    mut props: ResMut<smash::PropCollision>,
) {
    let dt = time.delta_secs() / SUBSTEPS as f32;
    for mut car in &mut cars {
        car.0.begin_tick();
        // Props (trees, rocks, smashables) on top of the track's collision (smash.rs).
        let ground = smash::PropGround::new(track.ground.as_ref(), Some(&*props), &car.0);
        for _ in 0..SUBSTEPS {
            car.0.step(input.0, dt, &ground);
        }
        let hits = ground.take_hits();
        drop(ground);
        props.apply_hits(&mut car.0, hits);
        // The collision mesh only covers roads and verges: remember where the car last had all
        // four wheels down, and put it back there if it drops off the edge of the world.
        safe.timer += time.delta_secs();
        let v = &car.0;
        if v.wheels.iter().all(|w| w.grounded) && v.speed() < 80.0 && safe.timer > 0.5 {
            let below = v.position - (v.rotation * Vec3::Y) * 0.6;
            safe.point = Some((below, v.yaw()));
            safe.timer = 0.0;
        }
        if let Some((point, yaw)) = safe.point {
            if car.0.position.y < point.y - 30.0 {
                car.0.place(point, yaw);
            }
        }
    }
}

fn sync_visuals(fixed: Res<Time<Fixed>>, hist: Res<perf::interp::WheelHistory>, mut cars: Query<(Entity, &Car, &mut Transform), Without<WheelVisual>>, mut wheels: Query<(&WheelVisual, &mut Transform), Without<Car>>) {
    let Ok((entity, car, mut t)) = cars.single_mut() else { return };
    let alpha = fixed.overstep_fraction();
    let (position, rotation) = car.0.render_pose(alpha);
    t.translation = position;
    t.rotation = rotation;
    for (w, mut wt) in &mut wheels {
        // Steer / spin / drop interpolated like the chassis (perf/interp.rs).
        let (steer, angle, drop) = hist.wheel(entity, &car.0, w.index, alpha);
        wt.translation = w.hub + Vec3::Y * (drop + tyre_vis_lift(car.0.wheels[w.index].tyre_deflection));
        // Rolling forward (-Z) is a negative rotation about +X; mirrored wheels share the rotation.
        wt.rotation = Quat::from_rotation_y(steer) * Quat::from_rotation_x(-angle);
        wt.scale = w.scale;
    }
}

/// Brake and reverse lamps (fh1-render car.rs FxCarLamps -> c58 dynamicLightsAmount). Indicators and fog have no input yet.
fn update_lamps(input: Res<Input>, mut cars: Query<(&Car, &mut fh1_render::car::FxCarLamps)>) {
    for (car, mut l) in &mut cars {
        // Pulse with the ABS (user 2026-10-07): dark while ABS holds a wheel released. FH1_BRAKE_LIGHT_ABS=0 = steady.
        static ABS_PULSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let pulse = *ABS_PULSE.get_or_init(|| std::env::var("FH1_BRAKE_LIGHT_ABS").map_or(true, |v| v != "0"));
        let released = pulse && car.0.abs_active();
        l.brake = if input.0.brake > 0.05 && !released { 1.0 } else { 0.0 };
        l.reverse = if car.0.gear == 0 { 1.0 } else { 0.0 };
    }
}

/// Drop shadow inputs (fh1-render car_shadow/drop_shadow.rs), in the car entity's space (= the render pose that
/// sync_visuals just set): wheel hubs, tyre sizes, steer, the ground below each wheel and the body height.
fn update_drop_shadow(fixed: Res<Time<Fixed>>, hist: Res<perf::interp::WheelHistory>, track: Res<Track>, mut cars: Query<(Entity, &Car, &mut fh1_render::car_shadow::drop_shadow::FxDropShadow)>) {
    for (entity, car, mut ds) in &mut cars {
        let v = &car.0;
        let alpha = fixed.overstep_fraction();
        let (pos, rot) = v.render_pose(alpha);
        let inv = rot.inverse();
        // Rear width isn't in CarData; the front width stands in for both axles.
        let width = v.data.steer.front_tire_width_mm * 0.001;
        let mut gap_sum = 0.0;
        for i in 0..4 {
            let (steer, _, drop) = hist.wheel(entity, v, i, alpha);
            let r = v.data.tyre_radius[i / 2];
            let hub = Vec3::from(v.data.hubs[i]) + Vec3::Y * drop - v.cg_model;
            let hub_w = pos + rot * hub;
            const REACH: f32 = 3.0;
            let (ground, gap) = match track.ground.ray(hub_w, Vec3::NEG_Y, r + REACH) {
                Some(h) => (inv * (h.point - pos), (h.distance - r).max(0.0)),
                None => (hub - inv * Vec3::Y * r, REACH),
            };
            gap_sum += gap;
            ds.wheels[i] = fh1_render::car_shadow::drop_shadow::FxDropShadowWheel { hub, radius: r, width, steer, ground, alpha: (1.0 - 4.0 * gap).clamp(0.0, 1.0) };
        }
        ds.height = gap_sum * 0.25;
    }
}

/// Automated check: `FH1_SHOT=<png>` saves a screenshot after `FH1_SHOT_AT` seconds (default 4)
/// and quits shortly after. `FH1_AUTODRIVE=1` holds full throttle with a little steering.
fn autotest(mut commands: Commands, time: Res<Time<Real>>, mut done: Local<u8>, mut exit: MessageWriter<AppExit>) {
    let Some(path) = std::env::var_os("FH1_SHOT") else { return };
    let at: f32 = std::env::var("FH1_SHOT_AT").ok().and_then(|s| s.parse().ok()).unwrap_or(4.0);
    let t = time.elapsed_secs();
    if *done == 0 && t > at {
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(PathBuf::from(path)));
        *done = 1;
    } else if *done == 1 && t > at + 0.1 {
        // FH1_SHOT2=<png>: a second frame ~0.1 s later in the same run (flicker / z-fight checks).
        if let Some(p2) = std::env::var_os("FH1_SHOT2") {
            commands.spawn(Screenshot::primary_window()).observe(save_to_disk(PathBuf::from(p2)));
        }
        *done = 2;
    } else if *done == 2 && t > at + 1.5 {
        exit.write(AppExit::Success);
        *done = 3;
    }
}

/// V toggles the collision mesh overlay (drawn in the developers' DebugColors).
fn toggle_collision_view(keys: Res<ButtonInput<KeyCode>>, mut views: Query<&mut Visibility, With<track::CollisionView>>) {
    if keys.just_pressed(KeyCode::KeyV) {
        for mut v in &mut views {
            *v = if *v == Visibility::Hidden { Visibility::Inherited } else { Visibility::Hidden };
        }
    }
}


/// Drawn wheels are lifted by the physics tyre-spring deflection: the hub sits at r - deflection while the drawn tyre is
/// rigid at r, so it sank ~1 cm into the ground (more on tall sidewalls). Visual only. FH1_TYRE_VIS_LIFT=0 = old.
pub(crate) fn tyre_vis_lift(deflection: f32) -> f32 {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| !std::env::var("FH1_TYRE_VIS_LIFT").is_ok_and(|v| v == "0")) { deflection } else { 0.0 }
}
