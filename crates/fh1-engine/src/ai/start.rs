//! The AI's race start (docs/AI.md "P17 AI start"): the gamedb TrackStartingMerges merge schedule and the
//! `AI/Racing/RaceBehavior/RaceStartBoost/*` torque boost. Pure functions, so tests can check them without a car.

use super::line::RacingLine;
use super::tables::StartMerge;

// ---- RaceStartBoost (GlobalRegistry.xml AI/Racing/RaceBehavior/RaceStartBoost, VERIFIED values) ----
/// TorqueScaleBoostForFirstCar: the boost of the pole car.
pub const BOOST_FIRST_CAR: f32 = 0.6;
/// TorqueScaleBoostForLastCar: the boost of the last car on the grid.
pub const BOOST_LAST_CAR: f32 = 0.2;
/// TorqueScaleBoostMinimum: the boost never goes below this.
pub const BOOST_MINIMUM: f32 = 0.0;
/// TorqueScaleBoostBehindPlayer: the boost of a car that is behind the player.
pub const BOOST_BEHIND_PLAYER: f32 = 0.0;
/// MinSpeedBoostBehindPlayer. Unit not in the data; m/s here (the engine's speed unit; INFERRED).
pub const MIN_SPEED_BEHIND_PLAYER: f32 = 25.0;
/// GapPerCar (m): a car counts as "behind the player" once the player is this far ahead (INFERRED use of the key).
pub const GAP_PER_CAR: f32 = 20.0;
/// MaxSkillBoostFactor: the boost multiplier of the best skill (AISkills id 1); id 80 gets x1 (INFERRED mapping).
pub const MAX_SKILL_BOOST_FACTOR: f32 = 2.0;
/// KeepFullBoostDistanceFirst / Last (m since GO): the full boost is kept this far (pole / last car, linear between).
pub const KEEP_FULL_FIRST: f32 = 500.0;
pub const KEEP_FULL_LAST: f32 = 250.0;
/// DecayBoostOverThisManyMeters: the boost then fades linearly to 0 over this distance.
pub const DECAY_METERS: f32 = 1000.0;

/// A merge deadline never leaves less than this many metres to merge in (our rule).
const MIN_MERGE_WINDOW: f32 = 20.0;
/// Without a corner on the route's first 3 km: merge over this many metres (our rule).
const OPEN_MERGE_WINDOW: f32 = 150.0;
/// The first "corner" ahead of the grid: a line point whose corner-only speed limit is under this (m/s, INFERRED).
pub const START_CORNER_SPEED: f32 = 40.0;
/// How far ahead of the grid a corner is looked for (m).
const CORNER_SEARCH_M: f32 = 3000.0;

/// The inputs of the start boost.
#[derive(Debug, Clone, Copy)]
pub struct BoostInput {
    /// Grid position, 0 = pole.
    pub index: u32,
    /// Cars on the grid (the player's included).
    pub count: u32,
    /// AISkills id of the driver (1 = best).
    pub skill_id: u32,
    /// Metres driven since GO.
    pub driven: f32,
    /// The player is more than GAP_PER_CAR ahead.
    pub behind_player: bool,
    /// Road speed (m/s).
    pub speed: f32,
}

/// Boost multiplier of a skill id: MaxSkillBoostFactor at id 1 down to 1 at id 80; fixed test skills (201+) get 1. INFERRED.
pub fn skill_boost_factor(skill_id: u32) -> f32 {
    if (1..=80).contains(&skill_id) {
        MAX_SKILL_BOOST_FACTOR + (1.0 - MAX_SKILL_BOOST_FACTOR) * (skill_id - 1) as f32 / 79.0
    } else {
        1.0
    }
}

/// The start torque boost (added to the driver's torque scale; INFERRED from the key names, behaviour not traced):
/// base = lerp(ForFirstCar, ForLastCar, grid fraction), >= Minimum, x the skill factor, full for the keep distance
/// (lerp(500, 250) m) and then fading to 0 over 1000 m. A car behind the player that is already at or over
/// MinSpeedBoostBehindPlayer gets BehindPlayer (0) as its base instead.
pub fn start_boost(i: &BoostInput) -> f32 {
    let f = if i.count > 1 { i.index.min(i.count - 1) as f32 / (i.count - 1) as f32 } else { 0.0 };
    let grid = BOOST_FIRST_CAR + (BOOST_LAST_CAR - BOOST_FIRST_CAR) * f;
    let base = if i.behind_player && i.speed >= MIN_SPEED_BEHIND_PLAYER { BOOST_BEHIND_PLAYER } else { grid };
    let keep = KEEP_FULL_FIRST + (KEEP_FULL_LAST - KEEP_FULL_FIRST) * f;
    let fade = 1.0 - ((i.driven - keep) / DECAY_METERS).clamp(0.0, 1.0);
    base.max(BOOST_MINIMUM) * skill_boost_factor(i.skill_id) * fade
}

/// Where a driver leaves its grid lane: metres after the start line it begins to merge, and the metres by which it must
/// be within MaxStartOfflineDistance of the line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MergeSchedule {
    pub from: f32,
    pub by: f32,
}

/// TrackStartingMerges as a schedule (a "waypoint" is one racing-line point, `spacing` m apart, ~2 m; the route_waypoint
/// nodes of TrackRouteNNN are ~140 m apart, which would put -420 waypoints 59 km before a corner; INFERRED reading):
/// - merging starts WaypointsToFirstMerge + grid index x WaypointsBetweenMerges waypoints after the start (cars merge one
///   after another, pole first);
/// - the car is within MaxStartOfflineDistance of the line `WaypointsBeforeCorner` waypoints (negative = before) from the
///   first corner (`corner` = metres to it, None = none ahead), but never earlier than MIN_MERGE_WINDOW after the first
///   merge could start; a close corner squeezes the stagger (the later cars start earlier than their turn).
pub fn merge_schedule(m: &StartMerge, index: u32, spacing: f32, corner: Option<f32>) -> MergeSchedule {
    let first = m.waypoints_to_first_merge.max(0.0) * spacing;
    let staggered = (m.waypoints_to_first_merge + index as f32 * m.waypoints_between_merges).max(0.0) * spacing;
    let by = match corner {
        Some(c) => (c + m.waypoints_before_corner * spacing).max(first + MIN_MERGE_WINDOW),
        None => staggered + OPEN_MERGE_WINDOW,
    };
    let from = staggered.min(by - MIN_MERGE_WINDOW).max(first.min(by - MIN_MERGE_WINDOW));
    MergeSchedule { from, by }
}

/// Metres from `s0` to the first line point whose corner-only speed (`v_corner`, SpeedProfile) is under
/// START_CORNER_SPEED, within CORNER_SEARCH_M; None when there is none.
pub fn first_corner(line: &RacingLine, v_corner: &[f32], s0: f32) -> Option<f32> {
    let n = line.len();
    if n == 0 || v_corner.len() != n {
        return None;
    }
    let i0 = line.index_at(s0);
    for k in 0..n {
        let j = i0 + k;
        if j >= n && !line.closed {
            break;
        }
        let i = j % n;
        let mut d = line.s[i] - s0;
        if line.closed && i < i0 {
            d += line.length;
        }
        let d = d.max(0.0);
        if d > CORNER_SEARCH_M {
            break;
        }
        if v_corner[i] < START_CORNER_SPEED {
            return Some(d);
        }
    }
    None
}
