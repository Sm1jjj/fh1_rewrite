//! Free-roam activities (docs/MISSIONS.md): speed cameras and average-speed zones, the Horizon Outposts' three mission
//! kinds (speed stunt, PR stunt, photo shoot), barn finds and race encounters, on Colorado. Data: the `missions` setup
//! group (`<assets>/missions/colorado/missions.json`, fh1setup missions.rs: the gamemodes XML + GameObjs + TrackRoute
//! NamedTransforms + gamedb, VERIFIED on the EU disc). Rules the data doesn't give are INFERRED and listed in the doc.
//!
//! Layout: `data` (the install file), `save` (profile.json `missions`), `reward` (credits / popularity / cars through the
//! career's paths), `hud` (our text lines + the objective / satnav hand-over), `speedtrap` (cameras, average zones),
//! `outpost` (outposts, mission select, the mission runner and the three mission kinds), `barn` (barn finds),
//! `encounter` (race encounters), `map` (icons for the world map / minimap owners).
//!
//! One activity at a time ([`Activity`]); nothing runs during a race (`race::RaceState::race`), in menus or photo mode
//! (except the photo shot itself), or off Colorado.
//!
//! Flags: `FH1_MISSIONS=0` = none of this; `FH1_MISSION_REWARDS=0` = no payouts; `FH1_SPEED_TRAPS=0`, `FH1_OUTPOSTS=0`,
//! `FH1_BARN_FINDS=0`, `FH1_ENCOUNTERS=0` per family. P18 (docs/MISSIONS.md "P18 fixes"): `FH1_OUTPOST_ABORT`,
//! `FH1_OUTPOST_MENU`, `FH1_PHOTO_CLOSE`, `FH1_TRAP_BAND`, `FH1_TRAP_PAY_ONCE`, `FH1_BARN_REACH_M`, `FH1_ENCOUNTER_GUARDS`,
//! `FH1_MISSION_MARKERS` (=0 = the first version's behaviour each).

use bevy::input::gamepad::{Gamepad, GamepadButton};
use bevy::prelude::*;

use crate::Car;

pub mod barn;
pub mod data;
pub mod encounter;
pub mod hud;
pub mod map;
pub mod markers;
pub mod outpost;
pub mod reward;
pub mod save;
pub mod speedtrap;

pub use data::MissionData;

/// m/s per mph.
pub const MPH: f32 = 0.44704;
/// Metres per mile.
pub const MILE_M: f32 = 1609.344;

fn flag_on(name: &str) -> bool {
    std::env::var(name).map_or(true, |v| v != "0")
}

/// `FH1_MISSIONS=0`: no missions at all.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| flag_on("FH1_MISSIONS"))
}

/// The installed mission data (empty without the `missions` group or off Colorado).
#[derive(Resource, Default)]
pub struct Missions {
    pub data: MissionData,
}

/// Which activity owns the player right now (one at a time).
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Activity {
    #[default]
    None,
    /// Outpost menu or an outpost mission (outpost.rs).
    Outpost,
    /// A race encounter (encounter.rs).
    Encounter,
    /// A barn-find discovery moment (barn.rs).
    Barn,
}

impl Activity {
    pub fn free(self) -> bool {
        self == Activity::None
    }
}

/// The player's car state this frame.
#[derive(Clone, Debug)]
pub struct Player {
    pub pos: Vec3,
    pub vel: Vec3,
    pub rot: Quat,
    pub media: String,
}

impl Player {
    pub fn speed_mph(&self) -> f32 {
        self.vel.length() / MPH
    }
    /// Unit forward (x, z); the car's front is -Z in model space.
    pub fn forward2(&self) -> Vec2 {
        let f = self.rot * Vec3::NEG_Z;
        Vec2::new(f.x, f.z).normalize_or_zero()
    }
}

pub fn player(cars: &Query<&Car>) -> Option<Player> {
    cars.iter().next().map(|c| Player { pos: c.0.position, vel: c.0.velocity, rot: c.0.rotation, media: c.0.data.media_name.clone() })
}

/// A race owns the player (from its start to its results).
pub fn race_running(rs: &Option<Res<crate::race::RaceState>>) -> bool {
    rs.as_ref().is_some_and(|r| r.race.is_some())
}

/// Colorado + missions on.
pub fn on_colorado(track: Res<crate::track::Track>) -> bool {
    enabled() && track.id == "colorado"
}

/// (x, z) of a 3D point.
pub fn xz(p: Vec3) -> Vec2 {
    Vec2::new(p.x, p.z)
}

/// The segment a -> b (car positions of two frames) crosses the gate line between posts `l` and `r` (either
/// direction), within `margin` m past the posts.
pub fn crosses_gate(l: Vec3, r: Vec3, a: Vec3, b: Vec3, margin: f32) -> bool {
    let (l, r, a, b) = (xz(l), xz(r), xz(a), xz(b));
    let gate = r - l;
    let len = gate.length();
    if len < 1e-3 {
        return false;
    }
    let n = gate.perp() / len;
    let (da, db) = ((a - l).dot(n), (b - l).dot(n));
    if da.signum() == db.signum() || da == db {
        return false;
    }
    let t = da / (da - db);
    let p = a + (b - a) * t;
    let along = (p - l).dot(gate / len);
    along >= -margin && along <= len + margin
}

/// Confirm (Enter / pad A), as race.rs.
pub fn confirm(keys: &ButtonInput<KeyCode>, pads: &Query<&Gamepad>) -> bool {
    keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter) || pads.iter().any(|p| p.just_pressed(GamepadButton::South))
}

/// Cancel held (Backspace / pad B): true once it has been held `CANCEL_HOLD_S` (B alone also downshifts in manual).
pub fn cancel_held(keys: &ButtonInput<KeyCode>, pads: &Query<&Gamepad>, held: &mut f32, dt: f32) -> bool {
    const CANCEL_HOLD_S: f32 = 1.0;
    if keys.pressed(KeyCode::Backspace) || pads.iter().any(|p| p.pressed(GamepadButton::East)) {
        *held += dt;
        if *held >= CANCEL_HOLD_S {
            *held = f32::NEG_INFINITY;
            return true;
        }
    } else {
        *held = 0.0;
    }
    false
}

/// Small deterministic RNG (no rand crate in the engine).
#[derive(Clone, Copy, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn seeded() -> Self {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x9E37_79B9);
        Self(t | 1)
    }
    pub fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 40) as f32 / (1u64 << 24) as f32
    }
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f32()
    }
}

/// Free-roam miles (barn finds run on them): every frame's move, teleports (> 40 m in a frame) skipped. Saved every
/// 0.25 mi.
fn track_miles(cars: Query<&Car>, mut profile: ResMut<crate::progression::Profile>, mut last: Local<Option<Vec3>>, mut unsaved: Local<f32>) {
    let Some(p) = cars.iter().next().map(|c| c.0.position) else { return };
    if let Some(a) = last.replace(p) {
        let d = xz(p).distance(xz(a));
        if d > 0.05 && d < 40.0 {
            let mi = d / MILE_M;
            profile.data.missions.miles += mi;
            *unsaved += mi;
            if *unsaved >= 0.25 {
                *unsaved = 0.0;
                profile.commit();
            }
        }
    }
}

fn load_data(mut missions: ResMut<Missions>, garage: Res<crate::Garage>) {
    missions.data = MissionData::load(&garage.assets);
    let d = &missions.data;
    info!(
        "missions: {} speed cameras, {} average zones, {} outposts ({} speed / {} PR / {} photo), {} barn finds",
        d.speed_cameras.len(),
        d.average_speed.len(),
        d.outposts.len(),
        d.speed_stunts.len(),
        d.pr_stunts.len(),
        d.photo_shoots.len(),
        d.barn_finds.len()
    );
}

pub struct MissionsPlugin;

impl Plugin for MissionsPlugin {
    fn build(&self, app: &mut App) {
        if !enabled() {
            return;
        }
        app.init_resource::<Missions>()
            .init_resource::<Activity>()
            .init_resource::<hud::MissionHud>()
            .init_resource::<hud::NavGuard>()
            .init_resource::<map::MissionMapIcons>()
            .add_systems(Startup, (load_data, hud::spawn_hud))
            .add_systems(Update, track_miles.run_if(on_colorado).run_if(crate::ui::driving))
            .add_systems(Update, (hud::draw_hud, map::update_icons).run_if(on_colorado));
        speedtrap::register(app);
        outpost::register(app);
        barn::register(app);
        encounter::register(app);
        markers::register(app);
    }
}
