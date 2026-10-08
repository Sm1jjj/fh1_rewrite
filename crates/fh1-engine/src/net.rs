//! Free-roam multiplayer (docs/MULTIPLAYER.md). Off unless `FH1_SERVER` is set (`FH1_SERVER_PASSWORD` for a locked
//! server, `FH1_NAME` for the name on the wire).
//!
//! The local car stays on the 120 Hz sim. Twenty times a second its motion goes to [`fh1_net::Client`], and its name / car
//! / paint whenever they change. Other players arrive as [`RemoteCar`] puppets: drawn on the sender's own timeline
//! (`sent_ms`) 100 ms behind its newest snapshot, shaded like an AI car, and solid only on this machine
//! ([`fh1_engine::vehicle::contact::collide_kinematic`] against their pose extrapolated to now). The server does not load
//! the world.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::gltf::GltfAssetLabel;
use bevy::prelude::*;
use bevy::world_serialization::WorldAssetRoot;
use fh1_engine::data::CarData;
use fh1_engine::vehicle::{contact, Vehicle};
use fh1_net::{Event, PlayerInfo, RejectReason, Snapshot, FLAG_BRAKE, FLAG_CUSTOM_PAINT, FLAG_METALLIC, FLAG_REVERSE};

use crate::track::Track;
use crate::ui::customize::{CarLooks, Paint};
use crate::ui::notify::HudNotify;
use crate::{Car, Garage, Input};

const SEND_NOTE: &str = "multiplayer";
/// Draw puppets out to here. Past that they still exist (and still bump you, if you reach them).
const DRAW: f32 = 2_000.0;
/// Puppets are drawn this far behind the sender's newest snapshot (2 packets at 20 Hz).
const INTERP_DELAY: f64 = 0.10;
const SAMPLE_CAP: usize = 24;
/// A puppet with no snapshot for this long is held where it is; it is removed after [`DROP_AFTER`].
const DROP_AFTER: f64 = 3.5;
/// The sender-to-local clock offset drifts up this fast (s/s) so a lower-latency packet can pull it back down.
const OFFSET_DRIFT: f64 = 0.02;
/// Collision pose: the newest snapshot extrapolated to now, at most this far.
const CONTACT_LEAD: f32 = 0.25;

pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Session>().init_resource::<NetConnect>().add_systems(Startup, connect).add_systems(
            Update,
            (exchange, sync_remotes, tag_remote_wheels, remote_shadow, puppets_posed).chain(),
        );
        // After the local step, AI, traffic and rewind, so the pose we push is the one that will be shown.
        app.add_systems(FixedUpdate, bump_players.after(crate::physics_settled));
    }
}

/// A server to join, set by the main menu's ONLINE browser (ui/online.rs): (address, password, the server's map). The
/// next frame drops any current session; the new one starts talking once that map is the loaded world.
#[derive(Resource, Default)]
pub struct NetConnect(pub Option<(String, String, String)>);

#[derive(Resource, Default)]
struct Session {
    client: Option<fh1_net::Client>,
    server: String,
    slot: Option<u8>,
    map: String,
    players: HashMap<u8, Entity>,
    /// Name / car / paint per player id (PLAYER packets).
    infos: HashMap<u8, PlayerInfo>,
    /// Car folders we refused or couldn't load, so a stranger's missing car doesn't warn every snapshot.
    missing: HashSet<String>,
    /// Latest snapshot for a player whose puppet isn't queryable yet (spawned this frame, or no PLAYER yet).
    pending: HashMap<u8, Snapshot>,
    /// Players whose join was announced (not again when a car change respawns the puppet).
    announced: HashSet<u8>,
    /// Map the ONLINE menu is loading for this server: nothing is sent until it is the live world.
    waiting_for: Option<String>,
}

/// Another player's car. The [`Vehicle`] is posed from the snapshots for contact and engine audio; it is not stepped.
#[derive(Component)]
pub struct RemoteCar {
    id: u8,
    name: String,
    pub vehicle: Vehicle,
    samples: VecDeque<Sample>,
    last_seq: u32,
    have_seq: bool,
    /// Local time of the last snapshot (s).
    seen: f64,
    /// Local time minus sender time (s): the smallest seen, drifting up slowly ([`OFFSET_DRIFT`]).
    offset: Option<f64>,
    spin: f32,
    steer: f32,
    drops: [i8; 4],
    gear: u8,
    flags: u8,
}

#[derive(Clone, Copy)]
struct Sample {
    seq: u32,
    /// On the local clock, from the sender's `sent_ms`.
    at: f64,
    pos: Vec3,
    rot: Quat,
    vel: Vec3,
    ang: Vec3,
    steer: f32,
    rpm: f32,
    gear: u8,
    throttle: f32,
    flags: u8,
}

#[derive(Component)]
struct RemoteWheel {
    car: Entity,
    index: usize,
    hub: Vec3,
    scale: Vec3,
}

/// Marker so other-car audio runs after puppet poses are written. No work of its own.
pub(crate) fn puppets_posed() {}

fn connect(mut session: ResMut<Session>, track: Option<Res<Track>>) {
    let Ok(addr) = std::env::var("FH1_SERVER") else { return };
    let password = std::env::var("FH1_SERVER_PASSWORD").unwrap_or_default();
    // exchange sets the live map again before the first HELLO goes out.
    let map = track.map_or(String::new(), |t| t.id.clone());
    open(&mut session, &addr, &password, &map);
}

fn player_name() -> String {
    std::env::var("FH1_NAME").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "Driver".into()))
}

/// (Re)connects the session to `addr`; any previous server is told we're leaving.
fn open(session: &mut Session, addr: &str, password: &str, map: &str) {
    let addr = addr.trim().to_string();
    if addr.is_empty() || addr == "0" {
        return;
    }
    if let Some(mut old) = session.client.take() {
        old.leave();
    }
    session.slot = None;
    let name = player_name();
    match fh1_net::Client::connect(&addr, &name, password, map) {
        Ok(client) => {
            info!("{SEND_NOTE}: {name} -> {addr}");
            session.server = addr;
            session.client = Some(client);
        }
        Err(e) => warn!("{SEND_NOTE}: {addr}: {e}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn exchange(
    mut session: ResMut<Session>,
    mut commands: Commands,
    time: Res<Time<Real>>,
    track: Res<Track>,
    garage: Res<Garage>,
    looks: Res<CarLooks>,
    input: Res<Input>,
    asset_server: Res<AssetServer>,
    cars: Query<(&Car, &fh1_render::car::FxCarLamps)>,
    mut remotes: Query<&mut RemoteCar>,
    mut notify: MessageWriter<HudNotify>,
    mut join: ResMut<NetConnect>,
) {
    if let Some((addr, password, map)) = join.0.take() {
        // Puppets of the previous server go; the map check below re-sets the client's map.
        let old: Vec<Entity> = session.players.drain().map(|(_, e)| e).collect();
        for e in old {
            despawn(&mut commands, e);
        }
        session.infos.clear();
        session.pending.clear();
        session.announced.clear();
        session.map.clear();
        open(&mut session, &addr, &password, &map);
        session.waiting_for = Some(map);
    }
    if session.client.is_none() {
        return;
    }
    if let Some(w) = &session.waiting_for {
        if track.id != *w {
            return;
        }
        session.waiting_for = None;
    }
    let now = time.elapsed_secs_f64();
    if session.map != track.id {
        let old: Vec<Entity> = session.players.drain().map(|(_, e)| e).collect();
        for e in old {
            despawn(&mut commands, e);
        }
        session.pending.clear();
        session.infos.clear();
        session.map = track.id.clone();
        session.client.as_mut().unwrap().set_map(&track.id);
    }
    // A puppet spawned last frame takes the snapshot that arrived with it.
    let pending: Vec<(u8, Snapshot)> = session.pending.drain().collect();
    for (_, snap) in pending {
        apply_state(&mut session, &mut commands, &garage, &asset_server, &track.id, &mut remotes, &mut notify, snap, now);
    }
    let events = session.client.as_mut().unwrap().poll();
    for ev in events {
        match ev {
            Event::Welcome { id, max_players, server_name, motd, .. } => {
                session.slot = Some(id);
                info!("{SEND_NOTE}: slot {id} of {max_players} on {} ({server_name})", session.server);
                let mut lines = vec![if server_name.is_empty() { "Online".into() } else { server_name }, format!("slot {id} of {max_players}")];
                if !motd.is_empty() {
                    lines.push(motd);
                }
                notify.write(HudNotify { lines });
            }
            Event::Full => {
                warn!("{SEND_NOTE}: {} is full", session.server);
                notify.write(HudNotify { lines: vec!["Server full".into(), "retrying".into()] });
            }
            Event::Rejected { reason, server_version, map } => {
                let why = match reason {
                    RejectReason::Version => format!("server runs protocol {server_version}, this game {}", fh1_net::VERSION),
                    RejectReason::WrongMap => format!("server runs the map {map}"),
                    RejectReason::BadPassword => "wrong password (FH1_SERVER_PASSWORD)".into(),
                    RejectReason::TooMany => "too many players from this address".into(),
                    RejectReason::Banned => "banned from this server".into(),
                };
                warn!("{SEND_NOTE}: {} refused the join: {why}", session.server);
                notify.write(HudNotify { lines: vec!["Can't join".into(), why] });
            }
            Event::Lost => {
                warn!("{SEND_NOTE}: lost {}, reconnecting", session.server);
                session.slot = None;
                notify.write(HudNotify { lines: vec!["Connection lost".into(), "reconnecting".into()] });
            }
            Event::Leave { id } => drop_player(&mut session, &mut commands, &remotes, &mut notify, id),
            Event::Player(info) => {
                let id = info.id;
                let changed = session.infos.get(&id).is_none_or(|old| old.car != info.car || paint_key(old) != paint_key(&info));
                session.infos.insert(id, info);
                if changed {
                    if let Some(e) = session.players.remove(&id) {
                        // New car or paint: respawn at the last pose.
                        let last = remotes.get(e).ok().and_then(|r| r.samples.back().copied());
                        despawn(&mut commands, e);
                        if let Some(s) = last {
                            session.pending.insert(id, sample_snapshot(id, &s));
                        }
                    }
                }
            }
            Event::State(snap) => apply_state(&mut session, &mut commands, &garage, &asset_server, &track.id, &mut remotes, &mut notify, snap, now),
        }
    }
    let stale: Vec<u8> = remotes.iter().filter(|r| now - r.seen > DROP_AFTER).map(|r| r.id).collect();
    for id in stale {
        drop_player(&mut session, &mut commands, &remotes, &mut notify, id);
    }
    let name = garage.cars[garage.current].clone();
    let (paint_seq, paint_rgb, paint_flags) = paint_of(&looks, &name);
    let me = PlayerInfo { id: 0, name: String::new(), car: name, paint_seq, paint_rgb, paint_flags };
    let state = cars.iter().next().map(|(car, _)| {
        let v = &car.0;
        let mut wheel_drop_mm = [0i8; 4];
        for (i, d) in wheel_drop_mm.iter_mut().enumerate() {
            *d = (v.wheel_drop(i) * 1000.0).round().clamp(-127.0, 127.0) as i8;
        }
        Snapshot {
            id: 0,
            seq: 0,
            sent_ms: 0,
            flags: if input.0.brake > 0.05 { FLAG_BRAKE } else { 0 } | if v.gear == 0 { FLAG_REVERSE } else { 0 },
            gear: v.gear.min(255) as u8,
            rpm: v.rpm.clamp(0.0, 19_999.0),
            steer: v.wheels[0].steer.clamp(-1.2, 1.2),
            throttle: input.0.throttle.clamp(0.0, 1.0),
            brake: input.0.brake.clamp(0.0, 1.0),
            wheel_drop_mm,
            position: v.position.to_array(),
            rotation: v.rotation.normalize().to_array(),
            velocity: v.velocity.to_array(),
            angular: v.angular_velocity.to_array(),
        }
    });
    session.client.as_mut().unwrap().tick(state.as_ref().filter(|s| s.sane()), &me);
}

fn paint_of(looks: &CarLooks, car: &str) -> (u32, u32, u8) {
    match looks.get(car).and_then(|l| l.paint) {
        Some(Paint::Factory { sequence }) => (sequence, 0, 0),
        Some(Paint::Custom { rgb, metallic }) => (0, rgb, FLAG_CUSTOM_PAINT | if metallic { FLAG_METALLIC } else { 0 }),
        None if car == crate::DEFAULT_CAR.0 => (crate::DEFAULT_CAR.1, 0, 0),
        None => (0, 0, 0),
    }
}

fn paint_key(info: &PlayerInfo) -> (u32, u32, u8) {
    (info.paint_seq, info.paint_rgb, info.paint_flags & (FLAG_CUSTOM_PAINT | FLAG_METALLIC))
}

/// A snapshot rebuilt from a puppet's last sample (to respawn it in place after a car change).
fn sample_snapshot(id: u8, s: &Sample) -> Snapshot {
    Snapshot {
        id,
        seq: s.seq,
        sent_ms: 0,
        flags: s.flags,
        gear: s.gear,
        rpm: s.rpm,
        steer: s.steer,
        throttle: s.throttle,
        brake: 0.0,
        wheel_drop_mm: [0; 4],
        position: s.pos.to_array(),
        rotation: s.rot.to_array(),
        velocity: s.vel.to_array(),
        angular: s.ang.to_array(),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_state(
    session: &mut Session,
    commands: &mut Commands,
    garage: &Garage,
    assets: &AssetServer,
    track_id: &str,
    remotes: &mut Query<&mut RemoteCar>,
    notify: &mut MessageWriter<HudNotify>,
    snap: Snapshot,
    now: f64,
) {
    let id = snap.id;
    if id == 0 || session.slot == Some(id) {
        return;
    }
    if let Some(&e) = session.players.get(&id) {
        match remotes.get_mut(e) {
            Ok(mut remote) => push_sample(&mut remote, &snap, now),
            // Spawned this frame: next frame.
            Err(_) => {
                session.pending.insert(id, snap);
            }
        }
        return;
    }
    // Who it is comes with PLAYER (sent on join and every few seconds); until then keep the newest pose.
    let Some(info) = session.infos.get(&id).cloned() else {
        session.pending.insert(id, snap);
        return;
    };
    spawn_remote(session, commands, garage, assets, track_id, notify, &info, snap, now);
}

#[allow(clippy::too_many_arguments)]
fn spawn_remote(
    session: &mut Session,
    commands: &mut Commands,
    garage: &Garage,
    asset_server: &AssetServer,
    track_id: &str,
    notify: &mut MessageWriter<HudNotify>,
    info: &PlayerInfo,
    snap: Snapshot,
    now: f64,
) {
    if info.car.is_empty() || session.missing.contains(&info.car) {
        return;
    }
    // Only cars this install has (the garage list): a peer's car name never becomes an arbitrary path.
    if !garage.cars.iter().any(|c| *c == info.car) {
        warn!("{SEND_NOTE}: {} drives {}, which isn't installed here", info.name, info.car);
        session.missing.insert(info.car.clone());
        return;
    }
    let dir = garage.assets.join("cars").join(&info.car);
    let data = match CarData::load(&dir) {
        Ok(d) => d,
        Err(e) => {
            warn!("{SEND_NOTE}: {} has no car files ({e:#})", info.car);
            session.missing.insert(info.car.clone());
            return;
        }
    };
    let mut vehicle = Vehicle::new(data, Vec3::ZERO);
    pose_vehicle(&mut vehicle, &snap);
    let cg = vehicle.cg_model;
    let lamp = Vec3::new(0.0, vehicle.data.hubs[0][1] + 0.25, vehicle.data.hubs[0][2] - 0.8) - cg;
    let paint = paint_key(info);
    let who = if info.name.is_empty() { "Driver".to_string() } else { info.name.clone() };
    if session.announced.insert(snap.id) {
        info!("{SEND_NOTE}: {who} joined in {}", info.car);
        notify.write(HudNotify { lines: vec![who.clone(), "joined".into()] });
    }
    let scene = asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("cars/{}/model.gltf", info.car)));
    let mut remote = RemoteCar {
        id: snap.id,
        name: who,
        vehicle,
        samples: VecDeque::new(),
        last_seq: 0,
        have_seq: false,
        seen: now,
        offset: None,
        spin: 0.0,
        steer: snap.steer,
        drops: snap.wheel_drop_mm,
        gear: snap.gear,
        flags: snap.flags,
    };
    push_sample(&mut remote, &snap, now);
    let id = snap.id;
    let (paint_seq, paint_rgb, paint_flags) = paint;
    let e = commands
        .spawn((
            remote,
            Transform::from_translation(Vec3::from_array(snap.position)).with_rotation(Quat::from_array(snap.rotation)),
            Visibility::default(),
            fh1_render::headlight::FxHeadlightSource { player: false, lamp },
            fh1_render::car_shadow::drop_shadow::FxDropShadow::default(),
            fh1_render::car::FxCarLamps::default(),
            Name::new(format!("net {id}")),
        ))
        .with_children(|p| {
            let mut body = p.spawn((
                WorldAssetRoot(scene),
                Transform::from_translation(-cg),
                fh1_render::car::FxCarBody { assets: garage.assets.clone(), car: info.car.clone(), track: track_id.to_owned() },
            ));
            if paint_flags & FLAG_CUSTOM_PAINT != 0 {
                body.insert(fh1_render::car::FxCarPaintRgb { rgb: paint_rgb, metallic: paint_flags & FLAG_METALLIC != 0 });
            } else if paint_seq != 0 {
                body.insert(fh1_render::car::FxCarPaint { sequence: paint_seq });
            }
        })
        .id();
    session.players.insert(id, e);
}

fn push_sample(remote: &mut RemoteCar, snap: &Snapshot, now: f64) {
    remote.seen = now;
    if remote.have_seq {
        let d = snap.seq.wrapping_sub(remote.last_seq);
        if d == 0 || d >= 0x8000_0000 {
            return;
        }
    }
    remote.have_seq = true;
    remote.last_seq = snap.seq;
    remote.drops = snap.wheel_drop_mm;
    // Jitter buffer: place the sample on the sender's timeline (sent_ms), shifted by the smallest observed
    // local-minus-sender offset (= the fastest delivery), which drifts up slowly to follow clock drift and route changes.
    let sent = snap.sent_ms as f64 / 1000.0;
    let raw = now - sent;
    let offset = match remote.offset {
        Some(o) => {
            let since = remote.samples.back().map_or(0.0, |s| (sent - (s.at - o)).max(0.0));
            (o + OFFSET_DRIFT * since).min(raw)
        }
        None => raw,
    };
    // A big jump (sender restarted its clock) resets the timeline.
    let offset = if remote.offset.is_some_and(|o| (raw - o).abs() > 5.0) { raw } else { offset };
    if remote.offset.is_some_and(|o| (o - offset).abs() > 5.0) {
        remote.samples.clear();
    }
    remote.offset = Some(offset);
    remote.samples.push_back(Sample {
        seq: snap.seq,
        at: sent + offset,
        pos: Vec3::from_array(snap.position),
        rot: Quat::from_array(snap.rotation).normalize(),
        vel: Vec3::from_array(snap.velocity),
        ang: Vec3::from_array(snap.angular),
        steer: snap.steer,
        rpm: snap.rpm,
        gear: snap.gear,
        throttle: snap.throttle,
        flags: snap.flags,
    });
    while remote.samples.len() > SAMPLE_CAP {
        remote.samples.pop_front();
    }
}

fn pose_vehicle(v: &mut Vehicle, snap: &Snapshot) {
    v.position = Vec3::from_array(snap.position);
    v.rotation = Quat::from_array(snap.rotation).normalize();
    v.prev_position = v.position;
    v.prev_rotation = v.rotation;
    v.velocity = Vec3::from_array(snap.velocity);
    v.angular_velocity = Vec3::from_array(snap.angular);
    v.rpm = snap.rpm;
    v.gear = snap.gear as usize;
    v.torque_fraction = snap.throttle;
}

fn drop_player(session: &mut Session, commands: &mut Commands, remotes: &Query<&mut RemoteCar>, notify: &mut MessageWriter<HudNotify>, id: u8) {
    session.pending.remove(&id);
    let left = session.infos.remove(&id);
    session.announced.remove(&id);
    let Some(e) = session.players.remove(&id) else { return };
    let name = remotes.get(e).ok().map(|r| r.name.clone()).or(left.map(|i| i.name)).unwrap_or_default();
    despawn(commands, e);
    if !name.is_empty() {
        info!("{SEND_NOTE}: {name} left");
        notify.write(HudNotify { lines: vec![name, "left".into()] });
    }
}

fn despawn(commands: &mut Commands, e: Entity) {
    if let Ok(mut ec) = commands.get_entity(e) {
        ec.try_despawn();
    }
}

/// The pose shown at local time `now`: interpolated [`INTERP_DELAY`] behind, or the newest coasted on its velocity for up
/// to 0.2 s when packets are late.
fn shown(samples: &VecDeque<Sample>, now: f64) -> Option<Sample> {
    let first = samples.front()?;
    let last = samples.back()?;
    let t = now - INTERP_DELAY;
    if samples.len() == 1 || t >= last.at {
        let extra = if t > last.at { ((t - last.at) as f32).clamp(0.0, 0.2) } else { 0.0 };
        let mut s = *last;
        s.pos += s.vel * extra;
        s.rot = (Quat::from_scaled_axis(s.ang * extra) * s.rot).normalize();
        return Some(s);
    }
    if t <= first.at {
        return Some(*first);
    }
    let i = samples.iter().position(|s| s.at >= t).unwrap_or(samples.len() - 1).max(1);
    let a = &samples[i - 1];
    let b = &samples[i];
    let u = ((t - a.at) / (b.at - a.at).max(1e-4)).clamp(0.0, 1.0) as f32;
    Some(Sample {
        seq: b.seq,
        at: t,
        pos: a.pos.lerp(b.pos, u),
        rot: a.rot.slerp(b.rot, u).normalize(),
        vel: a.vel.lerp(b.vel, u),
        ang: a.ang.lerp(b.ang, u),
        steer: a.steer + (b.steer - a.steer) * u,
        rpm: a.rpm + (b.rpm - a.rpm) * u,
        gear: if u < 0.5 { a.gear } else { b.gear },
        throttle: a.throttle + (b.throttle - a.throttle) * u,
        flags: if u < 0.5 { a.flags } else { b.flags },
    })
}

fn sync_remotes(time: Res<Time<Real>>, player: Query<&Car>, mut cars: Query<(&mut RemoteCar, &mut Transform, &mut Visibility, &mut fh1_render::car::FxCarLamps), Without<RemoteWheel>>, mut wheels: Query<(&RemoteWheel, &mut Transform), Without<RemoteCar>>) {
    let now = time.elapsed_secs_f64();
    let dt = time.delta_secs();
    let eye = player.iter().next().map(|c| c.0.position);
    for (mut car, mut t, mut vis, mut lamps) in &mut cars {
        let Some(s) = shown(&car.samples, now) else { continue };
        t.translation = s.pos;
        t.rotation = s.rot;
        let show = eye.is_none_or(|e| e.distance(s.pos) < DRAW);
        let want = if show { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
        lamps.brake = if s.flags & FLAG_BRAKE != 0 { 1.0 } else { 0.0 };
        lamps.reverse = if s.flags & FLAG_REVERSE != 0 { 1.0 } else { 0.0 };
        let radius = car.vehicle.data.tyre_radius[0].max(0.25);
        let speed = s.vel.dot(s.rot * Vec3::NEG_Z);
        // Contact pose: the newest snapshot extrapolated to now (the drawn pose is 100 ms+ old, so you'd hit where they
        // were). Audio uses the same vehicle.
        let contact = car.samples.back().copied().map(|n| {
            let lead = ((now - n.at) as f32).clamp(0.0, CONTACT_LEAD);
            (n.pos + n.vel * lead, (Quat::from_scaled_axis(n.ang * lead) * n.rot).normalize())
        });
        let (pos, rot) = contact.unwrap_or((s.pos, s.rot));
        car.vehicle.position = pos;
        car.vehicle.rotation = rot;
        car.vehicle.velocity = s.vel;
        car.vehicle.angular_velocity = s.ang;
        car.vehicle.rpm = s.rpm;
        car.vehicle.gear = s.gear as usize;
        car.vehicle.torque_fraction = s.throttle;
        car.steer = s.steer;
        car.gear = s.gear;
        car.flags = s.flags;
        car.spin = (car.spin + speed / radius * dt).rem_euclid(std::f32::consts::TAU);
    }
    for (w, mut wt) in &mut wheels {
        let Ok((car, ..)) = cars.get(w.car) else { continue };
        let drop = car.drops[w.index] as f32 * 0.001;
        let steer = if w.index < 2 { car.steer } else { 0.0 };
        wt.translation = w.hub + Vec3::Y * (drop + crate::tyre_vis_lift(0.0));
        wt.rotation = Quat::from_rotation_y(steer) * Quat::from_rotation_x(-car.spin);
        wt.scale = w.scale;
    }
}

fn tag_remote_wheels(mut commands: Commands, named: Query<(Entity, &Name, &Transform), (Added<Name>, Without<RemoteWheel>)>, parents: Query<&ChildOf>, cars: Query<(), With<RemoteCar>>) {
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
            commands.entity(e).insert(RemoteWheel { car, index, hub: t.translation, scale: t.scale });
        }
    }
}

fn remote_shadow(track: Res<Track>, mut cars: Query<(&RemoteCar, &Visibility, &mut fh1_render::car_shadow::drop_shadow::FxDropShadow)>) {
    for (car, vis, mut ds) in &mut cars {
        if *vis == Visibility::Hidden {
            continue;
        }
        let v = &car.vehicle;
        let (pos, rot) = (v.position, v.rotation);
        let inv = rot.inverse();
        let width = v.data.steer.front_tire_width_mm * 0.001;
        let mut gap_sum = 0.0;
        for i in 0..4 {
            let r = v.data.tyre_radius[i / 2];
            let drop = car.drops[i] as f32 * 0.001;
            let hub = Vec3::from(v.data.hubs[i]) + Vec3::Y * drop - v.cg_model;
            let hub_w = pos + rot * hub;
            const REACH: f32 = 3.0;
            let (ground, gap) = match track.ground.ray(hub_w, Vec3::NEG_Y, r + REACH) {
                Some(h) => (inv * (h.point - pos), (h.distance - r).max(0.0)),
                None => (hub - inv * Vec3::Y * r, REACH),
            };
            gap_sum += gap;
            let steer = if i < 2 { car.steer } else { 0.0 };
            ds.wheels[i] = fh1_render::car_shadow::drop_shadow::FxDropShadowWheel { hub, radius: r, width, steer, ground, alpha: (1.0 - 4.0 * gap).clamp(0.0, 1.0) };
        }
        ds.height = gap_sum * 0.25;
    }
}

fn bump_players(mut player: Query<&mut Car>, remotes: Query<&RemoteCar>) {
    let Ok(mut player) = player.single_mut() else { return };
    for remote in &remotes {
        if player.0.position.distance_squared(remote.vehicle.position) > 400.0 {
            continue;
        }
        contact::collide_kinematic(&mut player.0, &remote.vehicle);
    }
}
