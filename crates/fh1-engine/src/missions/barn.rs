//! Barn finds (docs/MISSIONS.md "Barn finds").
//!
//! VERIFIED (barnfinds.xml + GameObjs.xml): 9 `ActivityBarnFind`s, each a `TriggerZone` on its `BARNFIND_*` GameObjs node
//! (radius 10, maxMPH 50), `SpawnInfo` (weighting, min/max_probability at min/max_distance), `HintRegion` (x/y offset,
//! x/y radius), `DistanceDriven` (min 5, max 10, threshold 10; the XML's example says "SET IN MILES"), `UnlockCar id`,
//! `CAwardAchievement ACHIEVEMENT_BARN_THIS_WAY`; strings "NEW BARN FIND RUMOR", "BARN FIND READY - {0}", "Head to the
//! garage to collect it!", "You cannot sell Barn finds." (wallet.rs already refuses the sale).
//! INFERRED:
//! - A rumour spawns after `DistanceDriven` min..max miles of free-roam driving (rolled), one at a time.
//! - Which barn: weight = `weighting` x lerp(min_probability, max_probability) over the player's distance (m) from
//!   min_distance to max_distance (clamped): nearer barns more likely, as the min > max probabilities read.
//! - Hint circle: centre = barn + (x_offset east, y_offset north), radius = max(x_radius, y_radius) m.
//! - The find: inside the TriggerZone radius (+ [`reach_m`]) under maxMPH (no button: `requires_input false`), rumoured or
//!   not, and within [`FIND_BAND_M`] of the barn's height. The zone is centred on the dusty car INSIDE the barn
//!   (`BARNFIND_*` GameObj), 4..6 m behind the closed door's origin (`BF_*_CLOSED`, VERIFIED in GameObjs.xml), and the closed
//!   door has collision, so the car can only reach the zone from the door side: radius 10 m = about 4..6 m in front of it.
//! - Restoration ("Dak will call you"): ready after `threshold` miles more; the car then goes straight into the
//!   garage (CarSource::BarnFind) with the "BARN FIND READY" notice ([`BARN_FAME`] popularity on the find).
//! Not done: the barn door swap (props::is_closed_barn_door, `DoorInfo`), the Discover / outro cutscenes, the radio
//! hint VO (`BarnFindLocationHintN`).

use bevy::prelude::*;

use super::data::BarnFind;
use super::hud::{notify, MissionHud};
use super::save::{BarnRecord, BarnState};
use super::{player, race_running, xz, Activity, Missions, Rng};
use crate::progression::profile::CarSource;
use crate::Car;

/// INFERRED: popularity for finding a barn.
pub const BARN_FAME: u64 = 5_000;
/// The achievement id (barnfinds.xml CAwardAchievement).
pub const ACHIEVEMENT: &str = "ACHIEVEMENT_BARN_THIS_WAY";

pub fn barns_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_BARN_FINDS"))
}

/// A car this far above or below the barn (m) is on another level (a bridge, a cliff road): no find.
pub const FIND_BAND_M: f32 = 8.0;

/// `FH1_BARN_RUMOUR_NOW=1`: the next rumour spawns at once (testing).
fn rumour_now() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_BARN_RUMOUR_NOW").is_ok_and(|v| v == "1"))
}

/// `FH1_BARN_RESTORE_MI=<miles>` overrides the restoration distance (testing).
fn restore_miles(b: &BarnFind) -> f32 {
    static OVERRIDE: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    OVERRIDE.get_or_init(|| std::env::var("FH1_BARN_RESTORE_MI").ok().and_then(|v| v.parse().ok())).unwrap_or(b.distance_miles.threshold.max(0.0))
}

/// `FH1_BARN_REACH_M=<m>` (default 4, 0 = the authored radius only): metres added to the TriggerZone radius, because the
/// zone's centre is the car inside the barn and the closed door keeps the player outside it.
fn reach_m() -> f32 {
    static REACH: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *REACH.get_or_init(|| std::env::var("FH1_BARN_REACH_M").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(4.0).max(0.0))
}

/// The rumour's hint circle (engine x, z) and radius (m).
pub fn hint_circle(b: &BarnFind) -> (Vec2, f32) {
    (Vec2::new(b.pos[0] + b.hint.x_offset, b.pos[2] - b.hint.y_offset), b.hint.x_radius.max(b.hint.y_radius).max(50.0))
}

#[derive(Resource)]
struct BarnRun {
    rng: Rng,
    /// Barn index whose hint circle the player was last inside (one flash per entry).
    inside: Option<usize>,
}

impl Default for BarnRun {
    fn default() -> Self {
        Self { rng: Rng::seeded(), inside: None }
    }
}

pub fn register(app: &mut App) {
    if !barns_on() {
        return;
    }
    app.init_resource::<BarnRun>().add_systems(Update, barn_finds.run_if(super::on_colorado).run_if(crate::ui::driving));
}

fn state(profile: &crate::progression::Profile, b: &BarnFind) -> BarnState {
    profile.data.missions.barns.get(&b.name).map(|r| r.state).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn barn_finds(
    missions: Res<Missions>,
    cars: Query<&Car>,
    rs: Option<Res<crate::race::RaceState>>,
    activity: Res<Activity>,
    mut run: ResMut<BarnRun>,
    mut profile: ResMut<crate::progression::Profile>,
    events: Res<crate::race::Events>,
    mut banners: ResMut<crate::progression::Banners>,
    mut hud: ResMut<MissionHud>,
    mut pop: MessageWriter<crate::ui::notify::HudNotify>,
    mut snd: MessageWriter<crate::ui::sfx::UiSfx>,
) {
    let barns = &missions.data.barn_finds;
    if barns.is_empty() || race_running(&rs) {
        return;
    }
    let Some(p) = player(&cars) else { return };
    let miles = profile.data.missions.miles;
    let texts = &missions.data;
    let mut changed = false;

    // Restoration done -> into the garage.
    for b in barns {
        let Some(rec) = profile.data.missions.barns.get(&b.name).cloned() else { continue };
        if rec.state != BarnState::Restoring || miles - rec.at_miles < restore_miles(b) {
            continue;
        }
        let Some(media) = b.car.media.clone() else { continue };
        let added = super::reward::grant_car(&mut profile, &media, CarSource::BarnFind);
        profile.data.missions.barns.insert(b.name.clone(), BarnRecord { state: BarnState::Collected, at_miles: miles });
        let ready = texts.text("IDS_Ready_Message", "BARN FIND READY - {0}").replace("{0}", &b.car.label());
        notify(&mut pop, &[&ready, &texts.text("IDS_Title", "Head to the garage to collect it!"), ""]);
        if added {
            snd.write(crate::ui::sfx::UiSfx::play(crate::ui::sfx::keys::CAR_ADDED_TO_GARAGE));
        }
        info!("missions: barn find {} ({media}) restored, {} the garage", b.name, if added { "added to" } else { "already in" });
        changed = true;
    }

    // Rumours: one at a time, after DistanceDriven miles.
    let any_rumour = barns.iter().any(|b| state(&profile, b) == BarnState::Rumoured);
    let hidden: Vec<usize> = (0..barns.len()).filter(|&i| state(&profile, &barns[i]) == BarnState::Hidden).collect();
    if !any_rumour && !hidden.is_empty() && activity.free() {
        if profile.data.missions.next_rumour_miles <= 0.0 {
            let d = &barns[hidden[0]].distance_miles;
            let (lo, hi) = (d.min.max(0.0), d.max.max(d.min).max(0.0));
            profile.data.missions.next_rumour_miles = miles + if rumour_now() { 0.0 } else { run.rng.range(lo, hi) } + 1e-3;
            changed = true;
        }
        if miles >= profile.data.missions.next_rumour_miles {
            let weights: Vec<f32> = hidden
                .iter()
                .map(|&i| {
                    let s = &barns[i].spawn;
                    let dist = xz(p.pos).distance(Vec2::new(barns[i].pos[0], barns[i].pos[2]));
                    let span = (s.max_distance - s.min_distance).max(1.0);
                    let t = ((dist - s.min_distance) / span).clamp(0.0, 1.0);
                    (s.weighting.max(0.0) * (s.min_probability + (s.max_probability - s.min_probability) * t)).max(0.01)
                })
                .collect();
            let total: f32 = weights.iter().sum();
            let mut pick = run.rng.range(0.0, total);
            let mut chosen = hidden[hidden.len() - 1];
            for (k, w) in weights.iter().enumerate() {
                if pick < *w {
                    chosen = hidden[k];
                    break;
                }
                pick -= w;
            }
            let b = &barns[chosen];
            profile.data.missions.barns.insert(b.name.clone(), BarnRecord { state: BarnState::Rumoured, at_miles: miles });
            profile.data.missions.next_rumour_miles = 0.0;
            notify(&mut pop, &[&texts.text("IDS_Spawned_Body", "NEW BARN FIND RUMOR"), "Check the map for the search area", ""]);
            info!("missions: barn rumour {} ({})", b.name, b.car.label());
            changed = true;
        }
    }

    // Inside a rumour's circle: a reminder once per entry.
    let inside = barns.iter().position(|b| state(&profile, b) == BarnState::Rumoured && xz(p.pos).distance(hint_circle(b).0) < hint_circle(b).1);
    if inside.is_some() && inside != run.inside {
        hud.flash("BARN FIND RUMOUR - SEARCH THE AREA", 3.0);
    }
    run.inside = inside;

    // The find.
    if activity.free() {
        for b in barns {
            let st = state(&profile, b);
            if !matches!(st, BarnState::Hidden | BarnState::Rumoured) {
                continue;
            }
            if xz(p.pos).distance(Vec2::new(b.pos[0], b.pos[2])) > b.radius.max(5.0) + reach_m() || (p.pos.y - b.pos[1]).abs() > FIND_BAND_M || p.speed_mph() > b.max_mph.max(5.0) {
                continue;
            }
            profile.data.missions.barns.insert(b.name.clone(), BarnRecord { state: BarnState::Restoring, at_miles: miles });
            super::reward::popularity(&mut profile, &events, &mut banners, BARN_FAME);
            notify(&mut pop, &["BARN FIND!", &b.car.label().to_uppercase(), "DAK WILL CALL YOU WHEN IT'S READY"]);
            hud.flash(format!("BARN FIND  {}", b.car.label().to_uppercase()), 4.0);
            if super::reward::achievement(&mut profile, ACHIEVEMENT) {
                notify(&mut pop, &["ACHIEVEMENT UNLOCKED", "BARN THIS WAY", "Find your first barn find"]);
            }
            info!("missions: barn find {} discovered ({})", b.name, b.car.label());
            changed = true;
            break;
        }
    }
    if changed {
        profile.commit();
    }
}
