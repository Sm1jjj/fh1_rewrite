//! AI cars in the game (docs/AI.md "Engine"): spawns opponents for races ([`SpawnRaceAi`]), steps each with its own
//! `Vehicle` + `Driver` in FixedUpdate after the player's `step_physics`, resolves car-vs-car contact (vehicle/contact.rs),
//! draws them (game-shaded body + wheels, lamps, drop shadow), and runs the player assists that follow an event's line
//! (driving line, assisted braking / steering; ai/assist.rs).
//!
//! AI cars carry `AiCar`, not the player's `Car`, so the player-only systems stay single-car. Included from main.rs as
//! `#[path = "ai/plugin.rs"] mod ai_plugin;` (the lib's `ai` module holds the headless parts).
//!
//! Dev / playtest hook: `FH1_AI_RACE=<route>[:<cars>[:<car>]]` (e.g. `5:5`, `49:7:ALF_8C_08`) grids `cars` AI opponents
//! (default 5; default cars = a mixed field) on that route 2 s after launch with the player at the back, a 3 s hold, and
//! the driving line on. FH1_AI_DRAW_DIST (default 1500 m) hides AI cars beyond it.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::NoFrustumCulling;
use bevy::gltf::GltfAssetLabel;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::world_serialization::WorldAssetRoot;
use fh1_engine::ai::assist::{ratio_colour, PlayerLine, STRIP_BEHIND, STRIP_AHEAD, STRIP_HALF_WIDTH, STRIP_LIFT, STRIP_STEP};
use fh1_engine::ai::driver::{Driver, Obstacle, Situation};
use fh1_engine::ai::line::RacingLine;
use fh1_engine::ai::tables::AiTables;
use fh1_engine::ai::{line_path, route_id, AiCar, AiRaceControl, AiRacer, AiWheel, DespawnRaceAi, DrivingLine, SpawnRaceAi};
use fh1_engine::data::CarData;
use fh1_engine::vehicle::{contact, SteeringAssist, Vehicle};
use fh1_engine::world::MIRROR_Z;

use crate::track::Track;
use crate::{Car, Garage};

const SUBSTEPS: usize = 4;

pub struct AiPlugin;

impl Plugin for AiPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<SpawnRaceAi>()
            .add_message::<DespawnRaceAi>()
            .init_resource::<AiRaceControl>()
            .init_resource::<AiLines>()
            .init_resource::<PlayerAssist>()
            .init_resource::<RawInput>()
            .init_resource::<DevRace>()
            .add_systems(Startup, (setup_driving_line, setup_game_line))
            .add_systems(Update, (dev_race, spawn_ai, despawn_ai).chain())
            .add_systems(Update, tag_ai_wheels.before(crate::tag_wheels))
            .add_systems(Update, (sync_ai_visuals, ai_drop_shadow).chain().after(crate::sync_visuals))
            .add_systems(Update, (draw_driving_line, draw_game_line).after(crate::sync_visuals))
            .add_systems(Update, keep_raw_input.after(crate::read_input))
            .add_systems(FixedUpdate, player_assist.before(crate::step_physics))
            .add_systems(FixedUpdate, drive_ai.after(crate::step_physics).before(crate::physics_settled));
    }
}

/// Racing lines (per route) and the AI tables, loaded on first use.
#[derive(Resource, Default)]
pub struct AiLines {
    tables: Option<Arc<AiTables>>,
    lines: HashMap<u32, Option<Arc<RacingLine>>>,
}

impl AiLines {
    fn tables(&mut self, assets: &std::path::Path) -> Arc<AiTables> {
        self.tables
            .get_or_insert_with(|| {
                Arc::new(AiTables::load(&assets.join("ailines/ai_tables.json")).unwrap_or_else(|e| {
                    warn!("AI tables: {e:#} (run fh1setup --only ailines); using defaults");
                    AiTables::default()
                }))
            })
            .clone()
    }

    fn line(&mut self, assets: &std::path::Path, track: &str, route: u32) -> Option<Arc<RacingLine>> {
        self.lines
            .entry(route)
            .or_insert_with(|| match RacingLine::load(&line_path(assets, track, route), MIRROR_Z) {
                Ok(l) => Some(Arc::new(l)),
                Err(e) => {
                    warn!("AI line {track} route {route}: {e:#} (run fh1setup --only ailines)");
                    None
                }
            })
            .clone()
    }
}

/// An AI car's driver and its view of the player's progress on the same line.
#[derive(Component)]
pub struct AiBrain {
    pub driver: Driver,
    player_hint: Option<usize>,
    player_s: f32,
    player_progress: f64,
}

impl AiBrain {
    fn track_player(&mut self, p: Vec3) -> f64 {
        let line = &self.driver.line;
        let proj = line.project(p, self.player_hint);
        let proj = if proj.distance > 40.0 { line.project(p, None) } else { proj };
        if self.player_hint.is_none() {
            self.player_progress = proj.s as f64;
        } else {
            let mut ds = proj.s - self.player_s;
            if line.closed {
                if ds < -0.5 * line.length {
                    ds += line.length;
                } else if ds > 0.5 * line.length {
                    ds -= line.length;
                }
            }
            self.player_progress += ds as f64;
        }
        self.player_hint = Some(proj.index);
        self.player_s = proj.s;
        self.player_progress
    }
}

fn spawn_ai(
    mut commands: Commands,
    mut msgs: MessageReader<SpawnRaceAi>,
    garage: Res<Garage>,
    track: Res<Track>,
    mut lines: ResMut<AiLines>,
    asset_server: Res<AssetServer>,
) {
    for m in msgs.read() {
        let Some(route) = route_id(&m.route_file) else {
            warn!("AI slot {}: no route number in {:?}", m.slot, m.route_file);
            continue;
        };
        let Some(line) = lines.line(&garage.assets, &track.id, route) else { continue };
        let tables = lines.tables(&garage.assets);
        let dir = garage.assets.join("cars").join(&m.car_id);
        let data = match CarData::load(&dir) {
            Ok(d) => d,
            Err(e) => {
                warn!("AI slot {}: {}: {e:#}", m.slot, m.car_id);
                continue;
            }
        };
        let lamp_hub = data.hubs[0];
        let paint = if m.paint != 0 { m.paint } else { pick_paint(&dir, m.slot) };
        let (point, yaw) = m.pose;
        let mut vehicle = Vehicle::new(data, point);
        vehicle.place(point, yaw);
        let params = tables.driver(m.skill, m.temperament, m.rubberband, m.driver_id);
        let driver = Driver::new(&line, &vehicle, params, m.slot.wrapping_mul(7919) ^ route);
        info!(
            "AI slot {}: {} on route {route} ({:.0} m), skill {} temperament {} rubberband {}, predicted {:.1} s",
            m.slot, m.car_id, line.length, params.skill.id, params.temperament.id, params.rubberband.id, driver.profile_max.time
        );
        let cg = vehicle.cg_model;
        let scene = asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("cars/{}/model.gltf", m.car_id)));
        commands
            .spawn((
                AiCar(vehicle),
                AiBrain { driver, player_hint: None, player_s: 0.0, player_progress: 0.0 },
                AiRacer { slot: m.slot },
                Transform::from_translation(point),
                Visibility::default(),
                fh1_render::headlight::FxHeadlightSource { player: false, lamp: Vec3::new(0.0, lamp_hub[1] + 0.25, lamp_hub[2] - 0.8) - cg },
                fh1_render::car_shadow::drop_shadow::FxDropShadow::default(),
                fh1_render::car::FxCarLamps::default(),
                Name::new(format!("AI {} {}", m.slot, m.car_id)),
            ))
            .with_children(|p| {
                let mut body = p.spawn((
                    WorldAssetRoot(scene),
                    Transform::from_translation(-cg),
                    fh1_render::car::FxCarBody { assets: garage.assets.clone(), car: m.car_id.clone(), track: track.id.clone() },
                ));
                if paint != 0 {
                    body.insert(fh1_render::car::FxCarPaint { sequence: paint });
                }
            });
    }
}

/// A Combo_Colors sequence for an AI car without one: the slot picks through the car's list (0 = stock).
fn pick_paint(dir: &std::path::Path, slot: u32) -> u32 {
    let Ok(bytes) = std::fs::read(dir.join("physics.json")) else { return 0 };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return 0 };
    let seqs: Vec<u32> = v["colors"].as_array().map(|a| a.iter().filter_map(|c| c["Sequence"].as_u64().map(|s| s as u32)).collect()).unwrap_or_default();
    if seqs.is_empty() {
        0
    } else {
        seqs[(slot as usize * 3) % seqs.len()]
    }
}

fn despawn_ai(mut commands: Commands, mut msgs: MessageReader<DespawnRaceAi>, cars: Query<Entity, With<AiRacer>>) {
    if msgs.read().count() == 0 {
        return;
    }
    for e in &cars {
        commands.entity(e).despawn();
    }
}

/// Step every AI car one fixed tick: driver -> controls, physics substeps (props near the player), then car-vs-car contact
/// between all cars including the player's.
#[allow(clippy::type_complexity)]
fn drive_ai(
    time: Res<Time<Fixed>>,
    track: Res<Track>,
    control: Res<AiRaceControl>,
    dev: Res<DevRace>,
    mut props: ResMut<crate::smash::PropCollision>,
    mut ai: Query<(&mut AiCar, &mut AiBrain, &AiRacer)>,
    mut player: Query<&mut Car>,
) {
    if ai.is_empty() {
        return;
    }
    let dt = time.delta_secs();
    let player_pos = player.iter().next().map(|c| c.0.position);
    let mut obstacles: Vec<(u32, Obstacle)> = ai.iter().map(|(c, _, r)| (r.slot, Obstacle::of(&c.0))).collect();
    if let Some(p) = player.iter().next() {
        obstacles.push((0, Obstacle::of(&p.0)));
    }
    // Race order by progress (for the catch-up allowance per position).
    let mut order: Vec<(u32, f64)> = ai.iter().map(|(_, b, r)| (r.slot, b.driver.progress)).collect();
    let player_progress = ai.iter().next().map(|(_, b, _)| b.player_progress);
    if let Some(pp) = player_progress {
        order.push((0, pp));
    }
    order.sort_by(|a, b| b.1.total_cmp(&a.1));
    let rank = |slot: u32| order.iter().position(|o| o.0 == slot).unwrap_or(0) as i32;
    let mut others = Vec::with_capacity(obstacles.len());
    for (mut car, mut brain, racer) in &mut ai {
        let pp = player_pos.map(|p| brain.track_player(p));
        others.clear();
        others.extend(obstacles.iter().filter(|(s, _)| *s != racer.slot).map(|(_, o)| *o));
        let sit = Situation {
            hold: control.hold || dev.hold,
            finished: control.finished.contains(&racer.slot),
            player_progress: pp,
            positions_from_player: rank(racer.slot) - rank(0),
            obstacles: &others,
        };
        let v = &mut car.0;
        v.begin_tick();
        let dec = brain.driver.update(v, sit, dt);
        v.torque_mult = dec.torque_mult;
        let ground = crate::smash::PropGround::new(track.ground.as_ref(), Some(&*props), v);
        for _ in 0..SUBSTEPS {
            v.step(dec.controls, dt / SUBSTEPS as f32, &ground);
        }
        let hits = ground.take_hits();
        drop(ground);
        props.apply_hits(v, hits);
    }
    // Car-vs-car contact (once per tick).
    let mut cars: Vec<Mut<AiCar>> = ai.iter_mut().map(|(c, _, _)| c).collect();
    for i in 0..cars.len() {
        for j in i + 1..cars.len() {
            let (a, b) = cars.split_at_mut(j);
            contact::collide(&mut a[i].0, &mut b[0].0);
        }
        if let Some(mut p) = player.iter_mut().next() {
            contact::collide(&mut p.0, &mut cars[i].0);
        }
    }
}

/// Mark the wheel nodes of AI cars' glTF scenes (main.rs tag_wheels skips nodes with `AiWheel`).
fn tag_ai_wheels(mut commands: Commands, named: Query<(Entity, &Name, &Transform), (Added<Name>, Without<AiWheel>)>, parents: Query<&ChildOf>, cars: Query<(), With<AiCar>>) {
    const NAMES: [&str; 4] = ["wheel_LF", "wheel_RF", "wheel_LR", "wheel_RR"];
    for (e, name, t) in &named {
        let Some(index) = NAMES.iter().position(|n| name.as_str() == *n) else { continue };
        let mut cur = e;
        let car = loop {
            match parents.get(cur) {
                Ok(p) => {
                    cur = p.parent();
                    if cars.contains(cur) {
                        break Some(cur);
                    }
                }
                Err(_) => break None,
            }
        };
        if let Some(car) = car {
            commands.entity(e).insert(AiWheel { car, index, hub: t.translation, scale: t.scale });
        }
    }
}

fn draw_dist() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_AI_DRAW_DIST").ok().and_then(|v| v.parse().ok()).unwrap_or(1500.0))
}

#[allow(clippy::type_complexity)]
fn sync_ai_visuals(
    fixed: Res<Time<Fixed>>,
    player: Query<&Car>,
    mut cars: Query<(&AiCar, &AiBrain, &mut Transform, &mut Visibility, &mut fh1_render::car::FxCarLamps), Without<AiWheel>>,
    mut wheels: Query<(&AiWheel, &mut Transform), Without<AiCar>>,
) {
    let alpha = fixed.overstep_fraction();
    let eye = player.iter().next().map(|c| c.0.position);
    for (car, brain, mut t, mut vis, mut lamps) in &mut cars {
        let (p, r) = car.0.render_pose(alpha);
        t.translation = p;
        t.rotation = r;
        let show = eye.is_none_or(|e| e.distance(p) < draw_dist());
        let want = if show { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
        lamps.brake = if brain.driver.last.brake > 0.05 { 1.0 } else { 0.0 };
        lamps.reverse = if car.0.gear == 0 { 1.0 } else { 0.0 };
    }
    for (w, mut wt) in &mut wheels {
        let Ok((car, ..)) = cars.get(w.car) else { continue };
        let v = &car.0;
        wt.translation = w.hub + Vec3::Y * (v.wheel_drop(w.index) + crate::tyre_vis_lift(v.wheels[w.index].tyre_deflection));
        wt.rotation = Quat::from_rotation_y(v.wheels[w.index].steer) * Quat::from_rotation_x(-v.wheels[w.index].angle);
        wt.scale = w.scale;
    }
}

/// Drop shadow inputs for AI cars (as main.rs update_drop_shadow does for the player).
fn ai_drop_shadow(fixed: Res<Time<Fixed>>, track: Res<Track>, mut cars: Query<(&AiCar, &Visibility, &mut fh1_render::car_shadow::drop_shadow::FxDropShadow)>) {
    let alpha = fixed.overstep_fraction();
    for (car, vis, mut ds) in &mut cars {
        if *vis == Visibility::Hidden {
            continue;
        }
        let v = &car.0;
        let (pos, rot) = v.render_pose(alpha);
        let inv = rot.inverse();
        let width = v.data.steer.front_tire_width_mm * 0.001;
        let mut gap_sum = 0.0;
        for i in 0..4 {
            let r = v.data.tyre_radius[i / 2];
            let hub = Vec3::from(v.data.hubs[i]) + Vec3::Y * v.wheel_drop(i) - v.cg_model;
            let hub_w = pos + rot * hub;
            const REACH: f32 = 3.0;
            let (ground, gap) = match track.ground.ray(hub_w, Vec3::NEG_Y, r + REACH) {
                Some(h) => (inv * (h.point - pos), (h.distance - r).max(0.0)),
                None => (hub - inv * Vec3::Y * r, REACH),
            };
            gap_sum += gap;
            ds.wheels[i] = fh1_render::car_shadow::drop_shadow::FxDropShadowWheel { hub, radius: r, width, steer: v.wheels[i].steer, ground, alpha: (1.0 - 4.0 * gap).clamp(0.0, 1.0) };
        }
        ds.height = gap_sum * 0.25;
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Dev race (FH1_AI_RACE)

/// FH1_AI_RACE state. Kept apart from AiRaceControl, which the race module (race/*) rewrites every frame.
#[derive(Resource, Default)]
struct DevRace {
    started: bool,
    go_at: f32,
    hold: bool,
    route_file: Option<String>,
}

fn dev_race(
    mut st: ResMut<DevRace>,
    time: Res<Time<Real>>,
    garage: Res<Garage>,
    track: Res<Track>,
    mut lines: ResMut<AiLines>,
    mut spawn: MessageWriter<SpawnRaceAi>,
    mut player: Query<&mut Car>,
) {
    let Ok(spec) = std::env::var("FH1_AI_RACE") else { return };
    let t = time.elapsed_secs();
    if st.started {
        if st.hold && t > st.go_at {
            st.hold = false;
        }
        return;
    }
    if t < 2.0 {
        return;
    }
    st.started = true;
    let mut parts = spec.split(':');
    let route: u32 = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(5);
    let n: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(5).clamp(1, 11);
    let only_car = parts.next().map(str::to_owned);
    let Some(line) = lines.line(&garage.assets, &track.id, route) else { return };
    const FIELD: [&str; 7] = ["ALF_8C_08", "BMW_M3E92_08", "CHE_CamaroSS_69", "DOD_ViperSRT10ACRX_12", "FOR_MustangBOSS429_70", "VW_Corrado_95", "AUD_R8_08"];
    let route_file = format!("Ribbon_00/TrackRoute{route:03}.xml");
    // Two-wide grid, 8 m rows, player last; circuits start just behind the line's start.
    let total = n + 1;
    let rows = total.div_ceil(2);
    let base = if line.closed { 0.0 } else { 8.0 * rows as f32 + 4.0 };
    let pose = |k: u32| {
        let s = base - 8.0 * (k / 2) as f32 - 4.0;
        let (c, lat, _) = line.road_at(s);
        let side = if k % 2 == 0 { 0.35 } else { -0.35 };
        (c + lat * side, line.yaw_at(s))
    };
    for k in 0..n {
        let car = only_car.clone().unwrap_or_else(|| FIELD[k as usize % FIELD.len()].to_owned());
        let car = if garage.cars.iter().any(|c| *c == car) { car } else { garage.cars[garage.current].clone() };
        spawn.write(SpawnRaceAi { slot: k + 1, car_id: car, pose: pose(k), skill: 20 + 8 * k, route_file: route_file.clone(), circuit: line.closed, ..default() });
    }
    if let Some(mut p) = player.iter_mut().next() {
        let (point, yaw) = pose(n);
        p.0.place(point, yaw);
    }
    st.hold = true;
    st.route_file = Some(route_file);
    st.go_at = t + 3.0;
    info!("FH1_AI_RACE: route {route} ({:.0} m, {}), {n} AI cars, go in 3 s", line.length, if line.closed { "circuit" } else { "sprint" });
}

// ---------------------------------------------------------------------------------------------------------------------
// Player assists: driving line, assisted braking / steering

#[derive(Resource, Default)]
pub struct PlayerAssist {
    line: Option<PlayerLine>,
}

/// The player's own input of the last frame (main.rs `Input` before the assists change it).
#[derive(Resource, Default)]
struct RawInput(Option<fh1_engine::vehicle::Controls>);

fn keep_raw_input(input: Res<crate::Input>, mut raw: ResMut<RawInput>) {
    raw.0 = Some(input.0);
}

/// Keep the player's line state in step with the active event and apply Assisted braking / steering to the input the
/// physics step reads.
#[allow(clippy::too_many_arguments)]
fn player_assist(
    time: Res<Time<Fixed>>,
    control: Res<AiRaceControl>,
    dev: Res<DevRace>,
    garage: Res<Garage>,
    track: Res<Track>,
    settings: Res<crate::ui::Settings>,
    mut lines: ResMut<AiLines>,
    mut assist: ResMut<PlayerAssist>,
    raw: Res<RawInput>,
    mut input: ResMut<crate::Input>,
    mut player: Query<&mut Car>,
) {
    let route = control.route_file.as_deref().or(dev.route_file.as_deref()).and_then(route_id);
    let Some(mut car) = player.iter_mut().next() else { return };
    let Some(route) = route else {
        assist.line = None;
        return;
    };
    let stale = assist.line.as_ref().is_none_or(|l| l.route != route || l.car != car.0.data.media_name);
    if stale {
        assist.line = lines.line(&garage.assets, &track.id, route).map(|l| PlayerLine::new(route, &l, &car.0));
    }
    let Some(line) = assist.line.as_mut() else { return };
    line.update(&mut car.0, time.delta_secs());
    let braking = settings.braking_assist;
    let steering = settings.steering == SteeringAssist::Assisted;
    if let (Some(c), true) = (raw.0, braking || steering) {
        if !control.hold && !dev.hold {
            input.0 = line.apply(c, braking, steering);
        }
    }
}

/// The driving line mesh: chevrons every 3 m from 4 to 150 m ahead (fixed-size buffer, unused chevrons collapsed).
#[derive(Resource)]
struct DrivingLineMesh {
    mesh: Handle<Mesh>,
    entity: Entity,
}

const CHEVRONS: usize = 50;
const CHEVRON_STEP: f32 = 3.0;

fn setup_driving_line(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>, mut materials: ResMut<Assets<StandardMaterial>>) {
    let n = CHEVRONS * 6;
    let mut indices = Vec::with_capacity(CHEVRONS * 12);
    for k in 0..CHEVRONS as u16 {
        let b = k * 6;
        // Outer edge L, tip, R; inner edge L, tip, R: two quads (left and right arms).
        indices.extend_from_slice(&[b, b + 1, b + 3, b + 3, b + 1, b + 4, b + 1, b + 2, b + 4, b + 4, b + 2, b + 5]);
    }
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD);
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; n]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; n]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vec![[0.0f32; 4]; n]);
    mesh.insert_indices(Indices::U16(indices));
    let mesh = meshes.add(mesh);
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        cull_mode: None,
        double_sided: true,
        depth_bias: 60.0,
        fog_enabled: false,
        ..default()
    });
    let entity = commands
        .spawn((Mesh3d(mesh.clone()), MeshMaterial3d(material), Transform::default(), Visibility::Hidden, NoFrustumCulling, NotShadowCaster, NotShadowReceiver, Name::new("driving line")))
        .id();
    commands.insert_resource(DrivingLineMesh { mesh, entity });
}

fn draw_driving_line(
    settings: Res<crate::ui::Settings>,
    assist: Res<PlayerAssist>,
    track: Res<Track>,
    dl: Option<Res<DrivingLineMesh>>,
    player: Query<&Car>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut vis: Query<&mut Visibility>,
) {
    let Some(dl) = dl else { return };
    if !old_driving_line() {
        return;
    }
    let mode = settings.driving_line;
    let (Some(line), Some(car)) = (assist.line.as_ref(), player.iter().next()) else {
        if let Ok(mut v) = vis.get_mut(dl.entity) {
            *v = Visibility::Hidden;
        }
        return;
    };
    if let Ok(mut v) = vis.get_mut(dl.entity) {
        let want = if mode == DrivingLine::Off { Visibility::Hidden } else { Visibility::Inherited };
        if *v != want {
            *v = want;
        }
    }
    if mode == DrivingLine::Off {
        return;
    }
    let speed = car.0.forward_speed().max(0.0);
    let pts = line.points(speed, 4.0, 4.0 + CHEVRON_STEP * (CHEVRONS as f32 - 1.0), CHEVRON_STEP);
    if std::env::var_os("FH1_AI_DEBUG").is_some() {
        info!("driving line: {} pts, car {:?}, first {:?} ratio {:?}, s {:.0}", pts.len(), car.0.position, pts.first().map(|p| p.position), pts.first().map(|p| p.ratio), line.s());
    }
    let n = CHEVRONS * 6;
    let mut pos = vec![[0.0f32; 3]; n];
    let mut col = vec![[0.0f32; 4]; n];
    for (k, w) in pts.windows(2).enumerate().take(CHEVRONS) {
        let (a, b) = (w[0], w[1]);
        if mode == DrivingLine::BrakingOnly && a.ratio < 0.97 {
            continue;
        }
        let fwd = (b.position - a.position).normalize_or(Vec3::NEG_Z);
        // Sit on the road: ray down from 2 m above the line point.
        let y = track.ground.ray(a.position + Vec3::Y * 2.0, Vec3::NEG_Y, 6.0).map_or(a.position.y, |h| h.point.y) + 0.06;
        let c = Vec3::new(a.position.x, y, a.position.z);
        let (half, len, thick) = (0.8, 1.0, 0.7);
        let outer = [c + a.left * half, c + fwd * len, c - a.left * half];
        let inner = outer.map(|p| p + fwd * thick);
        let [r, g, bl] = ratio_colour(a.ratio);
        // Fade in near the car and out at the far end.
        let d = 4.0 + CHEVRON_STEP * k as f32;
        let alpha = 0.85 * (d / 8.0).min(1.0) * (1.0 - (d - 120.0).max(0.0) / 34.0).max(0.0);
        for (i, p) in outer.iter().chain(inner.iter()).enumerate() {
            pos[k * 6 + i] = p.to_array();
            col[k * 6 + i] = [r, g, bl, alpha];
        }
    }
    if let Some(mut m) = meshes.get_mut(&dl.mesh) {
        m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
        m.insert_attribute(Mesh::ATTRIBUTE_COLOR, col);
    }
}

/// FH1_DRIVING_LINE_OLD=1: R2's chevrons sliding with the car instead of the game's strip.
fn old_driving_line() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_DRIVING_LINE_OLD").is_ok_and(|v| v == "1"))
}

/// The game's driving line (CRaceLineRenderer, ai/assist.rs `strip`): one chevron quad per two waypoints, world-anchored,
/// raceline texture x vertex colour, alpha-blended.
#[derive(Resource)]
struct GameLineMesh {
    mesh: Handle<Mesh>,
    entity: Entity,
}

const GAME_SEGMENTS: usize = ((STRIP_BEHIND + STRIP_AHEAD) / STRIP_STEP + 2) as usize;

fn setup_game_line(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>, mut materials: ResMut<Assets<StandardMaterial>>, asset_server: Res<AssetServer>) {
    let n = GAME_SEGMENTS * 4;
    let mut indices = Vec::with_capacity(GAME_SEGMENTS * 6);
    for k in 0..GAME_SEGMENTS as u16 {
        let b = k * 4;
        indices.extend_from_slice(&[b, b + 1, b + 2, b + 2, b + 1, b + 3]);
    }
    let mut uv = Vec::with_capacity(n);
    for _ in 0..GAME_SEGMENTS {
        // Near end (v = 1, the chevron's legs) to far end (v = 0, its tip); left u = 0, right u = 1.
        uv.extend_from_slice(&[[0.0f32, 1.0], [1.0, 1.0], [0.0, 0.0], [1.0, 0.0]]);
    }
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD);
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; n]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; n]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, vec![[0.0f32; 4]; n]);
    mesh.insert_indices(Indices::U16(indices));
    let mesh = meshes.add(mesh);
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        base_color_texture: Some(asset_server.load("ui/textures/raceline.png")),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        cull_mode: None,
        double_sided: true,
        depth_bias: 60.0,
        fog_enabled: false,
        ..default()
    });
    let entity = commands
        .spawn((Mesh3d(mesh.clone()), MeshMaterial3d(material), Transform::default(), Visibility::Hidden, NoFrustumCulling, NotShadowCaster, NotShadowReceiver, Name::new("driving line (game)")))
        .id();
    commands.insert_resource(GameLineMesh { mesh, entity });
}

/// Road height under each strip point's two edges, looked up once per waypoint (route, index) so the strip never jitters.
#[derive(Default)]
struct StripHeights {
    route: u32,
    edges: HashMap<usize, [Vec3; 2]>,
}

#[allow(clippy::too_many_arguments)]
fn draw_game_line(
    settings: Res<crate::ui::Settings>,
    assist: Res<PlayerAssist>,
    track: Res<Track>,
    gl: Option<Res<GameLineMesh>>,
    player: Query<&Car>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut vis: Query<&mut Visibility>,
    mut heights: Local<StripHeights>,
) {
    let Some(gl) = gl else { return };
    let mode = settings.driving_line;
    let show = !old_driving_line() && mode != DrivingLine::Off;
    let (Some(line), Some(car), true) = (assist.line.as_ref(), player.iter().next(), show) else {
        if let Ok(mut v) = vis.get_mut(gl.entity) {
            if *v != Visibility::Hidden {
                *v = Visibility::Hidden;
            }
        }
        return;
    };
    if let Ok(mut v) = vis.get_mut(gl.entity) {
        if *v != Visibility::Inherited {
            *v = Visibility::Inherited;
        }
    }
    if heights.route != line.route {
        heights.route = line.route;
        heights.edges.clear();
    }
    let speed = car.0.forward_speed().max(0.0);
    let pts = line.strip(speed, mode == DrivingLine::BrakingOnly);
    // The strip's two edges on the road: the line point +- half width along the road's lateral, each dropped onto the
    // collision surface when that is within 1 m (the line data sits on the road; this only removes camber gaps), + lift.
    let ground = track.ground.as_ref();
    let mut edge = |p: &fh1_engine::ai::assist::StripPoint| -> [Vec3; 2] {
        *heights.edges.entry(p.index).or_insert_with(|| {
            [p.position + p.left * STRIP_HALF_WIDTH, p.position - p.left * STRIP_HALF_WIDTH].map(|e| {
                let y = ground.ray(e + Vec3::Y * 1.0, Vec3::NEG_Y, 2.0).map_or(e.y, |h| h.point.y);
                Vec3::new(e.x, y + STRIP_LIFT, e.z)
            })
        })
    };
    let n = GAME_SEGMENTS * 4;
    let mut pos = vec![[0.0f32; 3]; n];
    let mut col = vec![[0.0f32; 4]; n];
    let lin = |c: [f32; 4]| -> [f32; 4] {
        let l = Color::srgba(c[0], c[1], c[2], c[3]).to_linear();
        [l.red, l.green, l.blue, l.alpha]
    };
    let mut k = 0;
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        // Neighbouring waypoints only (an open line's end or a skipped point leaves a gap).
        if k >= GAME_SEGMENTS || a.position.distance(b.position) > 12.0 {
            continue;
        }
        let [al, ar] = edge(&a);
        let [bl, br] = edge(&b);
        let (ca, cb) = (lin(a.rgba), lin(b.rgba));
        for (j, (p, c)) in [(al, ca), (ar, ca), (bl, cb), (br, cb)].into_iter().enumerate() {
            pos[k * 4 + j] = p.to_array();
            col[k * 4 + j] = c;
        }
        k += 1;
    }
    // Unchanged strip (parked, or the same waypoints and colours): no write, so the render world doesn't re-extract and
    // re-allocate the mesh (fh1-render particles.rs `fx_mesh_cap_on`; FH1_FX_MESH_CAP=0 = write every frame, old).
    if fh1_render::particles::fx_mesh_cap_on() {
        if let Some(m) = meshes.get(&gl.mesh) {
            let same_pos = matches!(m.attribute(Mesh::ATTRIBUTE_POSITION), Some(bevy::mesh::VertexAttributeValues::Float32x3(v)) if *v == pos);
            let same_col = matches!(m.attribute(Mesh::ATTRIBUTE_COLOR), Some(bevy::mesh::VertexAttributeValues::Float32x4(v)) if *v == col);
            if same_pos && same_col {
                return;
            }
        }
    }
    if let Some(mut m) = meshes.get_mut(&gl.mesh) {
        m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
        m.insert_attribute(Mesh::ATTRIBUTE_COLOR, col);
    }
}
