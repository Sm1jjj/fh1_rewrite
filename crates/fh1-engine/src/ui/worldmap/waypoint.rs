//! Waypoints set on the world map (any map point or icon), the GPS line (next turn + distance, top centre) and the
//! in-world route chevrons on the road ahead.
//!
//! The waypoint is the free-roam source of the satnav target: while it is set it drives [`SatNav::target`] (the
//! minimap route and its pin) and the HUD objective line. A race owns both while `RaceState::owns_nav()`; when it lets
//! go it clears them and the waypoint is applied again here (agreed with the race session). The waypoint is not tied
//! to the car, so it survives fast travel and car changes; an in-process map switch keeps it for the return to
//! Colorado. Arriving within [`ARRIVE_M`] clears it.
//!
//! The GPS line reads the minimap's route ([`SatNav::path`], re-routed twice a second): the next junction (graph
//! degree >= 3) where the route bends more than [`TURN_MIN_DEG`], measured between the route 20 m before and after
//! the node. Not FH1 (the game had no turn-by-turn text): an improvement asked for by the user.
//!
//! Flags: `FH1_GPS_LINE=1` shows the GPS line (off by default, user request), `FH1_WAYPOINT_CHEVRONS=0` hides the
//! road chevrons.

use std::sync::OnceLock;

use bevy::prelude::*;
use fh1_engine::vehicle::Ground;

use crate::ui::minimap::{NavGraph, SatNav};
use crate::ui::notify::{HudNotify, Objective};
use crate::ui::UiFont;
use crate::Car;

/// A waypoint clears when the car gets this close (m). A little wider than the objective line's 24 m so the
/// notification fires before that clears the line on its own.
pub const ARRIVE_M: f32 = 30.0;
/// Smallest bend at a junction that the GPS calls a turn.
const TURN_MIN_DEG: f32 = 28.0;
/// The bend is measured between the route this far before and after the junction (m).
const TURN_ARM_M: f32 = 20.0;
/// Junctions further than this are not announced (the line then shows the distance to the destination).
const TURN_LOOKAHEAD_M: f32 = 3000.0;
/// Chevrons: count, spacing and the first one's distance ahead of the car (m).
const CHEVRONS: usize = 12;
const CHEVRON_GAP: f32 = 16.0;
const CHEVRON_START: f32 = 22.0;
/// The route green of MapProfileMinimap.xml (`route` group, 74,238,97).
const ROUTE_GREEN: Color = Color::srgb(74.0 / 255.0, 238.0 / 255.0, 97.0 / 255.0);

/// The next-turn line at the top of the screen: OFF by default (user 2026-10-08: the minimap already navigates);
/// `FH1_GPS_LINE=1` shows it.
fn gps_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_GPS_LINE").is_ok_and(|v| v == "1"))
}

fn chevrons_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_WAYPOINT_CHEVRONS").map_or(true, |v| v != "0"))
}

/// The player's waypoint (engine x, z).
#[derive(Resource, Default)]
pub struct Waypoint {
    pub target: Option<Vec2>,
    pub label: String,
    /// The map icon it was set on (its key), so the map can offer "Remove waypoint" there.
    pub source: Option<String>,
    /// Set or cleared since the last apply.
    dirty: bool,
}

impl Waypoint {
    pub fn set(&mut self, at: Vec2, label: impl Into<String>, source: Option<String>) {
        self.target = Some(at);
        self.label = label.into();
        self.source = source;
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.target = None;
        self.source = None;
        self.dirty = true;
    }
}

/// The next instruction of the GPS line.
#[derive(Resource, Default, Clone, PartialEq)]
pub struct Gps {
    /// Signed bend of the next turn (radians, + = left) and the distance to it (m).
    pub turn: Option<(f32, f32)>,
    /// Route distance left to the destination (m).
    pub remaining_m: Option<f32>,
}

pub(super) fn build(app: &mut App) {
    app.init_resource::<Waypoint>()
        .init_resource::<Gps>()
        .add_systems(Startup, spawn_gps_ui)
        .add_systems(
            Update,
            (apply.after(crate::race::race_nav), gps.after(apply), (draw_gps, chevrons).after(gps)).run_if(crate::ui::minimap::on_colorado),
        )
        .add_systems(Update, hide_off_colorado.run_if(not(crate::ui::minimap::on_colorado)));
}

/// Drive the satnav target and objective from the waypoint (outside races), and clear it on arrival.
#[allow(clippy::too_many_arguments)]
fn apply(
    mut wp: ResMut<Waypoint>,
    race: Option<Res<crate::race::RaceState>>,
    mut nav: ResMut<SatNav>,
    graph: Option<Res<NavGraph>>,
    mut obj: ResMut<Objective>,
    cars: Query<&Car>,
    mut notes: MessageWriter<HudNotify>,
) {
    if race.is_some_and(|r| r.owns_nav()) {
        return;
    }
    if let (Some(t), Ok(car)) = (wp.target, cars.single()) {
        let here = Vec2::new(car.0.position.x, car.0.position.z);
        // Icons can sit off the road (a gas station forecourt): reaching the route's last road node counts too.
        let road_end = (nav.target == Some(t)).then(|| nav.path.last().copied()).flatten().zip(graph.as_ref()).map(|(n, g)| Vec2::from(g.graph.pos[n as usize]));
        if here.distance(t) < ARRIVE_M || road_end.is_some_and(|e| here.distance(e) < ARRIVE_M * 0.6 && e.distance(t) < 300.0) {
            notes.write(HudNotify { lines: vec!["WAYPOINT REACHED".into(), wp.label.clone()] });
            wp.clear();
        }
    }
    let dirty = std::mem::take(&mut wp.dirty);
    match wp.target {
        Some(t) => {
            if dirty || nav.target != Some(t) || obj.target != Some(t) {
                nav.target = Some(t);
                obj.text = Some(format!("Waypoint: {}", wp.label));
                obj.target = Some(t);
            }
        }
        None if dirty => {
            // Cleared by the player or on arrival: the route and the objective it set go too.
            nav.target = None;
            nav.distance_m = None;
            nav.path.clear();
            obj.text = None;
            obj.target = None;
        }
        None => {}
    }
}

/// Points of the route from the car: the car's projection onto the first route segments, then the node path, then
/// the target. Returns (points, cumulative metres, node index per point (None for the ends)).
fn route_points(car: Vec2, nav: &SatNav, g: &fh1_ui::nav::Graph) -> Option<(Vec<Vec2>, Vec<f32>, Vec<Option<u32>>)> {
    let target = nav.target?;
    if nav.path.is_empty() {
        return None;
    }
    let pos = |i: u32| Vec2::from(g.pos[i as usize]);
    // The route was made up to 0.5 s ago from where the car was then: start from the nearest point on its first
    // segments so the distances don't jump back and forth.
    let mut best = (f32::INFINITY, 0usize, car);
    let mut prev = car;
    for (k, &n) in nav.path.iter().enumerate().take(12) {
        let p = pos(n);
        let q = if k == 0 { p } else { closest_on_segment(car, prev, p) };
        let d = q.distance(car);
        if d < best.0 {
            best = (d, k, q);
        }
        prev = p;
    }
    let (_, k, start) = best;
    let mut pts = vec![car, start];
    let mut nodes = vec![None, None];
    for &n in &nav.path[k..] {
        pts.push(pos(n));
        nodes.push(Some(n));
    }
    pts.push(target);
    nodes.push(None);
    let mut cum = Vec::with_capacity(pts.len());
    let mut acc = 0.0;
    for (i, p) in pts.iter().enumerate() {
        if i > 0 {
            acc += p.distance(pts[i - 1]);
        }
        cum.push(acc);
    }
    Some((pts, cum, nodes))
}

fn closest_on_segment(p: Vec2, a: Vec2, b: Vec2) -> Vec2 {
    let ab = b - a;
    let t = if ab.length_squared() > 1e-6 { ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
    a + ab * t
}

/// The point `s` metres along a polyline with cumulative lengths `cum`, and the direction there.
fn along(pts: &[Vec2], cum: &[f32], s: f32) -> (Vec2, Vec2) {
    let s = s.clamp(0.0, *cum.last().unwrap_or(&0.0));
    let i = cum.partition_point(|&c| c < s).clamp(1, pts.len().saturating_sub(1).max(1));
    let (a, b) = (pts[i - 1], pts[i.min(pts.len() - 1)]);
    let seg = cum[i.min(cum.len() - 1)] - cum[i - 1];
    let t = if seg > 1e-4 { (s - cum[i - 1]) / seg } else { 0.0 };
    (a.lerp(b, t), (b - a).normalize_or_zero())
}

fn gps(nav: Res<SatNav>, graph: Option<Res<NavGraph>>, cars: Query<&Car>, race: Option<Res<crate::race::RaceState>>, mut out: ResMut<Gps>) {
    let mut next = Gps::default();
    let racing = race.is_some_and(|r| r.owns_nav());
    if let (Some(graph), Ok(car), false) = (graph, cars.single(), racing) {
        let here = Vec2::new(car.0.position.x, car.0.position.z);
        if let Some((pts, cum, nodes)) = route_points(here, &nav, &graph.graph) {
            let total = *cum.last().unwrap_or(&0.0);
            next.remaining_m = Some(total);
            for i in 2..pts.len().saturating_sub(1) {
                let Some(n) = nodes[i] else { continue };
                let d = cum[i];
                if d > TURN_LOOKAHEAD_M {
                    break;
                }
                if d < 6.0 || graph.graph.adj[n as usize].len() < 3 {
                    continue;
                }
                let (before, _) = along(&pts, &cum, d - TURN_ARM_M);
                let (after, _) = along(&pts, &cum, d + TURN_ARM_M);
                // Map plane (x east, y north = −engine z): + = counter-clockwise = left.
                let to_plane = |v: Vec2| Vec2::new(v.x, -v.y);
                let (a, b) = (to_plane(pts[i] - before), to_plane(after - pts[i]));
                if a.length() < 1.0 || b.length() < 1.0 {
                    continue;
                }
                let ang = a.angle_to(b);
                if ang.abs() >= TURN_MIN_DEG.to_radians() {
                    next.turn = Some((ang, d));
                    break;
                }
            }
        }
    }
    if *out != next {
        *out = next;
    }
}

/// "TURN LEFT" / "KEEP RIGHT" / "SHARP LEFT" for a signed bend (radians, + = left).
pub fn turn_text(ang: f32) -> String {
    let side = if ang > 0.0 { "LEFT" } else { "RIGHT" };
    let deg = ang.abs().to_degrees();
    let verb = if deg < 50.0 {
        "KEEP"
    } else if deg < 130.0 {
        "TURN"
    } else {
        "SHARP"
    };
    format!("{verb} {side}")
}

/// A distance for the HUD in the player's units (Options "Speed units").
pub fn distance_text(m: f32, metric: bool) -> String {
    if metric {
        if m < 1000.0 {
            format!("{:.0} m", (m / 10.0).round() * 10.0)
        } else {
            format!("{:.1} km", m / 1000.0)
        }
    } else {
        let ft = m * 3.28084;
        if ft < 1000.0 {
            format!("{:.0} ft", (ft / 50.0).round() * 50.0)
        } else {
            format!("{:.1} mi", m / 1609.344)
        }
    }
}

#[derive(Component)]
struct GpsRoot;
#[derive(Component)]
struct GpsArrow;
#[derive(Component)]
struct GpsLine(u8);

fn spawn_gps_ui(mut commands: Commands, font: Res<UiFont>, assets: Res<AssetServer>) {
    let sheet: Handle<Image> = assets.load("ui/textures/horizon/map/icons/mapicons/en/mapiconsheet.png");
    commands
        .spawn((
            GpsRoot,
            Node { position_type: PositionType::Absolute, top: Val::Px(14.0), width: Val::Percent(100.0), justify_content: JustifyContent::Center, ..default() },
            Visibility::Hidden,
            GlobalZIndex(40),
        ))
        .with_children(|p| {
            p.spawn((
                Node {
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    column_gap: Val::Px(10.0),
                    padding: UiRect::axes(Val::Px(14.0), Val::Px(6.0)),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.03, 0.04, 0.05, 0.62)),
            ))
            .with_children(|p| {
                p.spawn((
                    GpsArrow,
                    Node { width: Val::Px(34.0), height: Val::Px(34.0), ..default() },
                    // The map's player arrow (sheet cell 3,3) points up = straight on; turned to the bend.
                    ImageNode { image: sheet, rect: Some(Rect::new(384.0, 384.0, 512.0, 512.0)), color: ROUTE_GREEN, ..default() },
                    UiTransform::IDENTITY,
                ));
                p.spawn(Node { flex_direction: FlexDirection::Column, ..default() }).with_children(|p| {
                    p.spawn((GpsLine(0), Text::new(""), font.text(22.0), TextColor(Color::WHITE)));
                    p.spawn((GpsLine(1), Text::new(""), font.text(14.0), TextColor(Color::srgba(1.0, 1.0, 1.0, 0.7))));
                });
            });
        });
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn draw_gps(
    gps: Res<Gps>,
    wp: Res<Waypoint>,
    nav: Res<SatNav>,
    settings: Res<crate::ui::Settings>,
    menu: Res<crate::ui::Menu>,
    rig: Res<crate::camera::CameraRig>,
    mut root: Query<&mut Visibility, With<GpsRoot>>,
    mut arrow: Query<&mut UiTransform, With<GpsArrow>>,
    mut lines: Query<(&GpsLine, &mut Text)>,
) {
    let Ok(mut vis) = root.single_mut() else { return };
    let show = gps_on() && !menu.open && !rig.photo && !crate::ui::loading::blocking() && nav.target.is_some() && gps.remaining_m.is_some();
    let want = if show { Visibility::Inherited } else { Visibility::Hidden };
    if *vis != want {
        *vis = want;
    }
    if !show || !(gps.is_changed() || settings.is_changed() || wp.is_changed()) {
        return;
    }
    let remaining = gps.remaining_m.unwrap_or(0.0);
    let (head, rot) = match gps.turn {
        Some((ang, d)) => (format!("{}  ·  {}", turn_text(ang), distance_text(d, settings.metric)), ang.clamp(-2.1, 2.1)),
        None => (format!("DESTINATION  ·  {}", distance_text(remaining, settings.metric)), 0.0),
    };
    let label = if wp.target.is_some() && wp.target == nav.target { wp.label.clone() } else { "Objective".into() };
    let sub = format!("{label}  ·  {}", distance_text(remaining, settings.metric));
    for (l, mut t) in &mut lines {
        let want = if l.0 == 0 { &head } else { &sub };
        if t.0 != *want {
            t.0 = want.clone();
        }
    }
    if let Ok(mut t) = arrow.single_mut() {
        // UiTransform rotates clockwise; a left bend (+) turns the arrow counter-clockwise.
        t.rotation = Rot2::radians(-rot);
    }
}

#[derive(Component)]
struct Chevron {
    mat: Handle<StandardMaterial>,
    /// Route point it was last dropped onto the ground for (re-raycast only when it moves).
    at: Option<Vec2>,
}

/// A flat quad on the ground (2.8 m), UV v = 0 at the front (−Z), for the chevron texture.
fn chevron_mesh() -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};
    let h = 1.4;
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]])
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 1.0, 0.0]; 4])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
        .with_inserted_indices(Indices::U32(vec![0, 2, 1, 0, 3, 2]))
}

/// The chevron as a soft signed-distance shape (the marker family agreed with the race visuals): a feathered core
/// stroke plus a glow halo, white (tinted by the material), pointing to v = 0.
fn chevron_texture() -> Image {
    use bevy::asset::RenderAssetUsages;
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
    const N: u32 = 128;
    let seg = |p: Vec2, a: Vec2, b: Vec2| {
        let ab = b - a;
        let t = ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
        p.distance(a + ab * t)
    };
    let (l, t, r) = (Vec2::new(-0.72, 0.42), Vec2::new(0.0, -0.36), Vec2::new(0.72, 0.42));
    let mut data = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let p = Vec2::new((x as f32 + 0.5) / N as f32 * 2.0 - 1.0, (y as f32 + 0.5) / N as f32 * 2.0 - 1.0);
            let d = seg(p, l, t).min(seg(p, t, r));
            let core = 1.0 - ((d - 0.09) / 0.06).clamp(0.0, 1.0);
            let glow = (-d * 7.0).exp() * 0.4;
            // Fade to nothing at the quad border so the halo never shows its square.
            let edge = (1.0 - p.abs().max_element()).clamp(0.0, 0.15) / 0.15;
            let a = core.max(glow) * edge;
            data.extend([255, 255, 255, (a * 255.0).round() as u8]);
        }
    }
    Image::new(Extent3d { width: N, height: N, depth_or_array_layers: 1 }, TextureDimension::D2, data, TextureFormat::Rgba8UnormSrgb, RenderAssetUsages::RENDER_WORLD)
}

/// Pulse rate shared with the race markers (Hz).
const PULSE_HZ: f32 = 1.2;
/// HDR gain so the chevrons bloom like the other markers.
const CHEVRON_GAIN: f32 = 2.0;

/// Chevrons on the road for the next ~200 m of the route. They sit at fixed distances from the destination, so they
/// stay put on the road while the car drives over them (re-routing doesn't slide them). Additive, HDR (blooms),
/// a pulse travelling forward along the row, faded in near the car and out at the far end; hidden while a race owns
/// the satnav (the race draws its own direction markers).
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn chevrons(
    mut commands: Commands,
    nav: Res<SatNav>,
    graph: Option<Res<NavGraph>>,
    track: Res<crate::track::Track>,
    menu: Res<crate::ui::Menu>,
    race: Option<Res<crate::race::RaceState>>,
    cars: Query<&Car>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    real: Res<Time<Real>>,
    mut q: Query<(&mut Chevron, &mut Transform, &mut Visibility)>,
    mut spawned: Local<bool>,
) {
    if !chevrons_on() {
        return;
    }
    if !*spawned {
        *spawned = true;
        let mesh = meshes.add(chevron_mesh());
        let tex = images.add(chevron_texture());
        for _ in 0..CHEVRONS {
            let mat = mats.add(StandardMaterial {
                base_color: Color::NONE,
                base_color_texture: Some(tex.clone()),
                unlit: true,
                alpha_mode: AlphaMode::Add,
                cull_mode: None,
                double_sided: true,
                fog_enabled: true,
                depth_bias: 50.0,
                ..default()
            });
            commands.spawn((Chevron { mat: mat.clone(), at: None }, Mesh3d(mesh.clone()), MeshMaterial3d(mat), Transform::default(), Visibility::Hidden, bevy::light::NotShadowCaster, bevy::light::NotShadowReceiver));
        }
        return;
    }
    let racing = race.is_some_and(|r| r.owns_nav());
    let route = match (&graph, cars.single()) {
        (Some(g), Ok(car)) if !racing && !menu.open => {
            let here = Vec2::new(car.0.position.x, car.0.position.z);
            route_points(here, &nav, &g.graph).map(|r| (r, here))
        }
        _ => None,
    };
    let Some(((pts, cum, _), _)) = route else {
        for (_, _, mut vis) in &mut q {
            if *vis != Visibility::Hidden {
                *vis = Visibility::Hidden;
            }
        }
        return;
    };
    let total = *cum.last().unwrap_or(&0.0);
    // Distances from the destination are fixed on the road: the first slot ahead of CHEVRON_START.
    let first_left = ((total - CHEVRON_START) / CHEVRON_GAP).floor() * CHEVRON_GAP;
    let heights = graph.as_ref().map(|g| g.heights.clone());
    for (k, (mut ch, mut t, mut vis)) in q.iter_mut().enumerate() {
        let left = first_left - k as f32 * CHEVRON_GAP;
        let s = total - left;
        if left < 4.0 || s < 0.0 {
            if *vis != Visibility::Hidden {
                *vis = Visibility::Hidden;
            }
            continue;
        }
        let (p, dir) = along(&pts, &cum, s);
        if ch.at.is_none_or(|a| a.distance(p) > 0.05) {
            ch.at = Some(p);
            // Ground: down from a little above the road network's height (not the highest surface: bridges).
            let nav_y = graph.as_ref().and_then(|g| g.graph.nearest(p.into())).and_then(|n| heights.as_ref()?.get(n as usize).copied()).unwrap_or(t.translation.y);
            let hit = track.ground.ray(Vec3::new(p.x, nav_y + 4.0, p.y), Vec3::NEG_Y, 12.0);
            let (y, up) = hit.map_or((nav_y, Vec3::Y), |h| (h.point.y, h.normal));
            let yaw = Quat::from_rotation_y((-dir.x).atan2(-dir.y));
            t.translation = Vec3::new(p.x, y + 0.08, p.y);
            t.rotation = Quat::from_rotation_arc(Vec3::Y, up.normalize_or(Vec3::Y)) * yaw;
        }
        // Fade in over the first slot, out towards the far end.
        let ahead = s - cum.get(1).copied().unwrap_or(0.0);
        let far = CHEVRON_START + CHEVRON_GAP * (CHEVRONS as f32 - 1.0);
        let a = ((ahead - CHEVRON_START * 0.5) / (CHEVRON_GAP)).clamp(0.0, 1.0) * ((far + CHEVRON_GAP - ahead) / (CHEVRON_GAP * 3.0)).clamp(0.0, 1.0) * 0.85;
        // Travelling pulse: brightest slot moves away from the car at PULSE_HZ.
        let wave = 0.5 + 0.5 * (std::f32::consts::TAU * (real.elapsed_secs() * PULSE_HZ - k as f32 * 0.12)).sin();
        let gain = CHEVRON_GAIN * (0.55 + 0.45 * wave);
        let g = ROUTE_GREEN.to_linear();
        if let Some(mut m) = mats.get_mut(&ch.mat) {
            m.base_color = Color::LinearRgba(LinearRgba::new(g.red * gain, g.green * gain, g.blue * gain, a));
        }
        let want = if a > 0.01 { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
    }
}

fn hide_off_colorado(mut root: Query<&mut Visibility, Or<(With<GpsRoot>, With<Chevron>)>>) {
    for mut v in &mut root {
        if *v != Visibility::Hidden {
            *v = Visibility::Hidden;
        }
    }
}
