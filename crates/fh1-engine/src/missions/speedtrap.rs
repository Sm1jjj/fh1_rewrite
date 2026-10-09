//! Speed cameras and average-speed zones (docs/MISSIONS.md "Speed traps").
//!
//! VERIFIED (speed_camera.xml / average_speed.xml / GameObjs.xml): 22 cameras `speed_camera_NN` with `scoreboard_id`
//! 201..230 and `SpeedThreshold minimumSpeed 30`, each a GameObjs post pair `_left` / `_right`; 9 average zones with two
//! gate pairs (`left1/right1`, `left2/right2`) and minimumSpeed 30.
//! P18 audit (VERIFIED against `ui/map/colorado.nav`): all 22 cameras and all 18 zone gates sit within 2 m of the minimap
//! road graph, perpendicular to it (83..90 degrees), 12..32 m wide with the posts 2..6 m beyond the road edge and level to
//! within 1.6 m (`speed_camera_24`'s gate is 31.9 m wide: a dual carriageway, and the graph has a second layer at y 0
//! under it), so no setup change is needed (docs/MISSIONS.md "P18 fixes").
//! INFERRED: the trap is the line between the two posts (crossed either way, 4 m past the posts counts, within
//! [`GATE_BAND_M`] of the gate's height so a bridge over the road doesn't trigger it); speeds are mph; the average zone
//! runs from the first gate pair crossed to the other one, over the distance driven (crossing the first pair again
//! restarts it); popularity per counted pass ([`CAMERA_FAME_PER_MPH`], [`PB_BONUS_FAME`]) — FH1's own payout is not in the
//! data. `FH1_TRAP_PAY_ONCE=0`: every pass at or above the minimum pays (farmable by driving back and forth).
//!
//! Every crossing (any speed) is also sent as [`SpeedTrapCrossed`] for the speed-stunt missions (outpost.rs).

use bevy::prelude::*;

use super::data::AverageZone;
use super::hud::{notify, speed_text, MissionHud};
use super::{crosses_gate, player, race_running, Activity, Missions, MPH};
use crate::Car;

/// INFERRED: popularity per mph of a counted pass.
pub const CAMERA_FAME_PER_MPH: f32 = 5.0;
/// INFERRED: popularity bonus for a new personal best.
pub const PB_BONUS_FAME: u64 = 1_000;
/// INFERRED: an average-speed run is dropped after this long (s), or [`AVERAGE_S_PER_STRAIGHT_M`] per metre between its
/// two gates when that is longer.
pub const AVERAGE_MAX_S: f32 = 180.0;
/// INFERRED: seconds per metre between the two gates of a zone (the roads between them are not straight; the longest pair
/// is 1,000 m apart, so 250 s).
const AVERAGE_S_PER_STRAIGHT_M: f32 = 0.25;
/// INFERRED: posts are a lane apart; a crossing this far past either post still counts.
const GATE_MARGIN_M: f32 = 4.0;
/// The same camera can't fire twice within this (s).
const RETRIGGER_S: f32 = 3.0;
/// INFERRED: a car this far above or below the gate's height (m) is on another road level (a bridge) and doesn't cross
/// it. `FH1_TRAP_BAND=0`: no height test (the first version).
pub const GATE_BAND_M: f32 = 6.0;
/// INFERRED: a repeat pass pays only when it beats the best by this much (mph); `FH1_TRAP_PAY_ONCE=0` pays every pass.
pub const PB_MIN_GAIN_MPH: f32 = 1.0;

pub fn traps_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_SPEED_TRAPS"))
}

fn band_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_TRAP_BAND"))
}

fn pay_once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::flag_on("FH1_TRAP_PAY_ONCE"))
}

/// [`crosses_gate`] plus a height test: the car's height at the crossing (mean of the two frames) is within
/// [`GATE_BAND_M`] of the gate's (mean of the posts). The outposts' speed stunts can use it too (outpost.rs).
pub fn crosses_gate_band(l: Vec3, r: Vec3, a: Vec3, b: Vec3, margin: f32) -> bool {
    crosses_gate(l, r, a, b, margin) && (!band_on() || ((a.y + b.y) * 0.5 - (l.y + r.y) * 0.5).abs() <= GATE_BAND_M)
}

/// (new personal best?, popularity) of a pass at `mph` against the previous best.
fn pass_fame(mph: f32, best: Option<f32>) -> (bool, u64) {
    let pb = best.is_none_or(|b| mph > b);
    let paid = !pay_once() || best.is_none_or(|b| mph >= b + PB_MIN_GAIN_MPH);
    let fame = if paid { (mph * CAMERA_FAME_PER_MPH).round() as u64 + if pb { PB_BONUS_FAME } else { 0 } } else { 0 };
    (pb, fame)
}

/// Seconds an average-speed run may last.
fn zone_timeout(z: &AverageZone) -> f32 {
    let mid = |g: &[[f32; 3]; 2]| (Vec3::from_array(g[0]) + Vec3::from_array(g[1])) * 0.5;
    AVERAGE_MAX_S.max(mid(&z.start).distance(mid(&z.end)) * AVERAGE_S_PER_STRAIGHT_M)
}

fn zone_title(z: &AverageZone) -> String {
    if z.label.is_empty() || z.label.starts_with("IDS_") {
        "AVERAGE SPEED ZONE".to_owned()
    } else {
        z.label.to_uppercase()
    }
}

fn best_line(pb: bool, best: Option<f32>, metric: bool) -> String {
    match (pb, best) {
        (true, Some(b)) => format!("NEW PERSONAL BEST  (was {})", speed_text(b, metric)),
        (true, None) => "NEW PERSONAL BEST".to_owned(),
        (false, Some(b)) => format!("BEST {}", speed_text(b, metric)),
        (false, None) => String::new(),
    }
}

/// A speed camera was crossed (any speed). `name` = `speed_camera_NN`.
#[derive(Message, Clone, Debug)]
pub struct SpeedTrapCrossed {
    pub name: String,
    pub mph: f32,
}

#[derive(Resource, Default)]
struct TrapState {
    last: Option<Vec3>,
    /// Camera name -> time of its last trigger.
    fired: std::collections::HashMap<String, f32>,
    /// Running average zone: (index, entered at the end gate?, start time, distance m).
    average: Option<(usize, bool, f32, f32)>,
}

pub fn register(app: &mut App) {
    app.add_message::<SpeedTrapCrossed>();
    if !traps_on() {
        return;
    }
    app.init_resource::<TrapState>().add_systems(Update, speed_traps.run_if(super::on_colorado).run_if(crate::ui::driving));
}

#[allow(clippy::too_many_arguments)]
fn speed_traps(
    time: Res<Time>,
    missions: Res<Missions>,
    activity: Res<Activity>,
    cars: Query<&Car>,
    rs: Option<Res<crate::race::RaceState>>,
    mut st: ResMut<TrapState>,
    mut profile: ResMut<crate::progression::Profile>,
    events: Res<crate::race::Events>,
    mut banners: ResMut<crate::progression::Banners>,
    mut hud: ResMut<MissionHud>,
    mut out: MessageWriter<SpeedTrapCrossed>,
    mut pop: MessageWriter<crate::ui::notify::HudNotify>,
    settings: Option<Res<crate::ui::Settings>>,
) {
    let Some(p) = player(&cars) else { return };
    let Some(a) = st.last.replace(p.pos) else { return };
    if race_running(&rs) || a.distance(p.pos) > 60.0 {
        st.average = None;
        return;
    }
    let now = time.elapsed_secs();
    let mph = p.speed_mph();
    let metric = settings.as_ref().is_some_and(|s| s.metric);
    let mut changed = false;

    for c in &missions.data.speed_cameras {
        let (l, r) = (Vec3::from_array(c.left), Vec3::from_array(c.right));
        if !crosses_gate_band(l, r, a, p.pos, GATE_MARGIN_M) {
            continue;
        }
        if st.fired.get(&c.name).is_some_and(|&t| now - t < RETRIGGER_S) {
            continue;
        }
        st.fired.insert(c.name.clone(), now);
        out.write(SpeedTrapCrossed { name: c.name.clone(), mph });
        // A speed stunt (outpost mission) or encounter owns this pass: no free-roam popup / pay on top.
        if !activity.free() {
            continue;
        }
        if mph < c.min_mph {
            continue;
        }
        let best = profile.data.missions.speed_cameras.get(&c.name).copied();
        let (pb, fame) = pass_fame(mph, best);
        if pb {
            profile.data.missions.speed_cameras.insert(c.name.clone(), mph);
        }
        super::reward::popularity(&mut profile, &events, &mut banners, fame);
        let title = if c.label.is_empty() || c.label.starts_with("IDS_") { "SPEED CAMERA".to_owned() } else { c.label.to_uppercase() };
        let second = best_line(pb, best, metric);
        hud.flash(format!("{}  {}", title, speed_text(mph, metric)), 3.0);
        notify(&mut pop, &[&speed_text(mph, metric), &title, &second]);
        info!("missions: {} at {mph:.1} mph (best {:?}), +{fame} popularity", c.name, best);
        changed = true;
    }

    // Average-speed zones: enter at either gate pair, finish at the other (not while another activity owns the player).
    if !activity.free() {
        st.average = None;
    }
    let cross = |g: &[[f32; 3]; 2]| crosses_gate_band(Vec3::from_array(g[0]), Vec3::from_array(g[1]), a, p.pos, GATE_MARGIN_M);
    if let Some((i, from_end, t0, dist)) = st.average {
        let z = &missions.data.average_speed[i];
        let dist = dist + a.distance(p.pos);
        st.average = Some((i, from_end, t0, dist));
        let (origin, target) = if from_end { (&z.end, &z.start) } else { (&z.start, &z.end) };
        if now - t0 > zone_timeout(z) {
            st.average = None;
        } else if cross(target) {
            st.average = None;
            let avg = dist / (now - t0).max(0.1) / MPH;
            let title = zone_title(z);
            if avg >= z.min_mph {
                let best = profile.data.missions.average_speed.get(&z.name).copied();
                let (pb, fame) = pass_fame(avg, best);
                if pb {
                    profile.data.missions.average_speed.insert(z.name.clone(), avg);
                }
                super::reward::popularity(&mut profile, &events, &mut banners, fame);
                hud.flash(format!("{title}  AVERAGE {}", speed_text(avg, metric)), 3.0);
                let second = best_line(pb, best, metric);
                notify(&mut pop, &[&speed_text(avg, metric), &title, &second]);
                info!("missions: {} average {avg:.1} mph over {dist:.0} m (best {:?}), +{fame} popularity", z.name, best);
                changed = true;
            } else {
                hud.flash(format!("{title}  {} TOO SLOW", speed_text(z.min_mph - avg, metric)), 3.0);
            }
        } else if cross(origin) {
            // Back over the gate it started at (a U-turn): start again.
            st.average = Some((i, from_end, now, 0.0));
            hud.flash(zone_title(z), 2.0);
        } else {
            hud.lines.retain(|l| !l.starts_with("AVERAGE "));
            hud.lines.push(format!("AVERAGE {}", speed_text(dist / (now - t0).max(0.1) / MPH, metric)));
        }
    } else if activity.free() {
        for (i, z) in missions.data.average_speed.iter().enumerate() {
            let start = cross(&z.start);
            let end = !start && cross(&z.end);
            if start || end {
                st.average = Some((i, end, now, 0.0));
                hud.flash(zone_title(z), 2.0);
                break;
            }
        }
    }
    if st.average.is_none() {
        hud.lines.retain(|l| !l.starts_with("AVERAGE "));
    }
    if changed {
        profile.commit();
    }
}
