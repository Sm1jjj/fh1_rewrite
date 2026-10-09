//! Race physics settings for the AI cars (and drafting for the player too), from PhysicsSettings.ini (docs/AI.md "P17 AI
//! physics: TCS, contact, drafting"). Everything here is plain functions over numbers so it tests headless; drive_ai
//! (ai/plugin.rs) calls [`begin_tick`] once per fixed tick, then [`set_ai`] on each AI car and [`set_player`] on the player's.
//!
//! * AI traction control: TractionControlSpeedAI 22 mph, TCSFullEffectFricDiffAI 0.25, TCSFullSteerSlipScale*AI
//!   (`FH1_AI_TCS_GAME=0` = the player's numbers, as before). The AI's own on/off stays `FH1_AI_TCS`.
//! * Car-car contact: CarCarResitution -0.1 and the race torque scales (`FH1_AI_CONTACT_GAME=0` = restitution 0.2, no scale).
//! * Drafting: the Drafting block, for every race car including the player (`FH1_DRAFT=0` = none).
//!
//! Hooks in the vehicle code: `Vehicle::tcs_params`, `Vehicle::aero_scale`, `contact::collide_with`. All default to the
//! player's current physics, so a car nobody touches here behaves exactly as before.

use bevy::math::Vec3;
use fh1_engine::vehicle::contact::ContactParams;
use fh1_engine::vehicle::{AeroScale, TcsParams, Vehicle};

fn flag_on(name: &str) -> bool {
    std::env::var(name).map_or(true, |v| v != "0")
}

/// `FH1_AI_TCS_GAME=0`: AI cars use the player's TCS numbers.
fn tcs_game() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| flag_on("FH1_AI_TCS_GAME"))
}

/// `FH1_AI_CONTACT_GAME=0`: AI car-car contact with restitution 0.2 and unscaled torque, as before.
fn contact_game() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| flag_on("FH1_AI_CONTACT_GAME"))
}

/// `FH1_DRAFT=0`: no drafting.
fn draft_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| flag_on("FH1_DRAFT"))
}

// ---- traction control ----

/// The AI's traction-control numbers (PhysicsSettings.ini; VERIFIED values). `speed` is TractionControlSpeedAI 22 read as mph
/// like the player's 30 (INFERRED, as steering.rs). `steer_scale` is [FWD, RWD, AWD] (steering.rs order):
/// TCSFullSteerSlipScaleFWDAI 1.7, RWDAI 1.3, AWDAI 0.71 (the player's are 1.2 / 1.1 / 2.0). TCSFullEffectFricDiffAI 0.25.
pub const AI_TCS: TcsParams = TcsParams { speed: 22.0 * 0.447_04, full_effect: 0.25, steer_scale: [1.7, 1.3, 0.71] };

/// The TCS numbers an AI car uses: the game's AI set, or `None` (= the player's) with `game` off.
pub fn tcs_params_for(game: bool) -> Option<TcsParams> {
    if game {
        Some(AI_TCS)
    } else {
        None
    }
}

pub fn tcs_params() -> Option<TcsParams> {
    tcs_params_for(tcs_game())
}

// ---- contact ----

/// Which pair is in contact (all AI cars here are in a race, so the "InRace" values apply).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairKind {
    /// Race AI vs race AI: RaceCarTorqueScaleInRace 0.2 (VERIFIED value; written `= 0.2` in the ini).
    AiAi,
    /// The player vs a race AI: pitch/roll RaceCarTorqueScaleInRace 0.2, yaw HumanCarAICarCollYawTorqueScale0 0.75 (INFERRED
    /// split: the yaw key is the human-vs-AI override of the yaw part).
    HumanAi,
    /// Race AI vs traffic: RaceCarTorqueScaleVsTrafficInRace 0.05. Not used yet: traffic/plugin.rs only collides traffic with
    /// the player and traffic.
    #[allow(dead_code)]
    AiTraffic,
}

/// CarCarResitution (VERIFIED value, the game's misspelling). INFERRED reading: the e of j = -(1 + e) v_n m_eff, so -0.1 = 90%
/// of a perfectly inelastic impulse: cars stay in soft contact instead of bouncing apart (ours was 0.2).
const CAR_CAR_RESTITUTION: f32 = -0.1;

/// Contact response for a pair kind. Torque scales act on the angular part of the impulse response (INFERRED): `ang_scale` on
/// the pitch / roll axes, `yaw_scale` on the vertical axis, both cars of the pair. CollisionBiasAI 0.5 / AIInRace 1.0 (who gets
/// pushed, INFERRED) and PlayerTorqueScale 0 are NOT used: no safe reading.
pub fn contact_params_for(game: bool, kind: PairKind) -> ContactParams {
    if !game {
        return ContactParams::DEFAULT;
    }
    let (ang_scale, yaw_scale) = match kind {
        PairKind::AiAi => (0.2, 0.2),
        PairKind::HumanAi => (0.2, 0.75),
        PairKind::AiTraffic => (0.05, 0.05),
    };
    ContactParams { restitution: CAR_CAR_RESTITUTION, friction: ContactParams::DEFAULT.friction, ang_scale, yaw_scale }
}

pub fn contact_params(kind: PairKind) -> ContactParams {
    contact_params_for(contact_game(), kind)
}

// ---- drafting ----

// PhysicsSettings.ini "Drafting" block, VERIFIED values; index 0 = at DraftSpeed0, 1 = at DraftSpeed1, linear in between.
/// DraftSpeed0 / 1: 40 / 150 mph.
const DRAFT_SPEED: [f32; 2] = [17.8816, 67.056];
/// DraftStartZFade0/1, DraftStartZBox0/1, DraftEndZFadeBox0/1 (m).
const START_Z_FADE: [f32; 2] = [6.0, 9.0];
const START_Z_BOX: [f32; 2] = [5.0, 5.0];
const END_Z_FADE_BOX: [f32; 2] = [30.0, 60.0];
/// DraftStartXFadeCarWidthScale0/1, DraftEndXFadeCarWidthScale0/1 (x the follower's car width).
const START_X: [f32; 2] = [1.0, 1.0];
const END_X: [f32; 2] = [2.5, 2.5];
/// DraftFollowingFront/Body/RearDragScale0/1 are equal (0.93 / 0.72), so one drag scale.
const FOLLOW_DRAG: [f32; 2] = [0.93, 0.72];
/// DraftFollowing{Front,Rear}DownforceScale0/1: [speed end][axle].
const FOLLOW_DOWN: [[f32; 2]; 2] = [[0.93, 0.97], [0.47, 0.61]];
/// DraftLeadingFront/Body/RearDragScale 0.96; DraftLeadingFront/RearDownforceScale 0.99 / 0.95.
const LEAD_DRAG: f32 = 0.96;
const LEAD_DOWN: [f32; 2] = [0.99, 0.95];
/// Cars farther than this ahead (or the max EndZFadeBox) cannot draft.
const MAX_Z: f32 = 60.0;
/// A leader this far above or below does not shield (bridges, ramps).
const MAX_DY: f32 = 3.0;

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// 0 at DraftSpeed0 and below, 1 at DraftSpeed1 and above.
fn speed_t(v: f32) -> f32 {
    ((v - DRAFT_SPEED[0]) / (DRAFT_SPEED[1] - DRAFT_SPEED[0])).clamp(0.0, 1.0)
}

fn at(pair: [f32; 2], t: f32) -> f32 {
    lerp(pair[0], pair[1], t)
}

/// How much of the slipstream a follower going `v` m/s feels from a car `z` m ahead (centre to centre, along the follower's
/// heading) and `x` m to the side, `width` the follower's width (m). INFERRED model of the ini keys: the weight ramps 0 -> 1
/// from StartZBox to StartZFade, then fades linearly to 0 at EndZFadeBox; sideways it is 1 within StartXFade car widths and
/// fades to 0 at EndXFade; below DraftSpeed0 it fades with speed (no wake at standstill). 0..=1.
pub fn draft_weight(v: f32, z: f32, x: f32, width: f32) -> f32 {
    let t = speed_t(v);
    let (z_box, z_fade, z_end) = (at(START_Z_BOX, t), at(START_Z_FADE, t), at(END_Z_FADE_BOX, t));
    let wz = if z <= z_box || z >= z_end {
        0.0
    } else if z < z_fade {
        (z - z_box) / (z_fade - z_box).max(1e-3)
    } else {
        (z_end - z) / (z_end - z_fade).max(1e-3)
    };
    let ax = x.abs() / width.max(0.5);
    let (x0, x1) = (at(START_X, t), at(END_X, t));
    let wx = if ax <= x0 {
        1.0
    } else if ax >= x1 {
        0.0
    } else {
        (x1 - ax) / (x1 - x0).max(1e-3)
    };
    let slow = (v / DRAFT_SPEED[0]).clamp(0.0, 1.0);
    wz * wx * slow
}

/// What drafting needs of one car.
#[derive(Clone, Copy, Debug)]
pub struct DraftCar {
    pub pos: Vec3,
    /// Unit forward axis.
    pub fwd: Vec3,
    pub vel: Vec3,
    /// Body width (m).
    pub width: f32,
}

impl DraftCar {
    pub fn of(v: &Vehicle) -> Self {
        Self { pos: v.position, fwd: v.rotation * Vec3::NEG_Z, vel: v.velocity, width: v.data.block_dims[0].max(1.5) }
    }
}

fn mul(a: AeroScale, b: AeroScale) -> AeroScale {
    AeroScale { drag: a.drag * b.drag, down: [a.down[0] * b.down[0], a.down[1] * b.down[1]] }
}

/// The aero scales of every car for this tick (same order as `cars`). Each car is shielded by the car ahead that gives the
/// largest weight (following scales at speed `v`, lerped from 1 by that weight); the cars with a follower get the Leading
/// scales (weighted the same way). A car in a train of three has both.
pub fn draft_scales(cars: &[DraftCar]) -> Vec<AeroScale> {
    let n = cars.len();
    let mut follow_w = vec![0.0f32; n];
    let mut lead_w = vec![0.0f32; n];
    let mut speed = vec![0.0f32; n];
    for (i, f) in cars.iter().enumerate() {
        let fl = Vec3::new(f.fwd.x, 0.0, f.fwd.z).normalize_or_zero();
        let vf = f.vel.dot(fl);
        speed[i] = vf;
        if fl == Vec3::ZERO || vf <= 1.0 {
            continue;
        }
        let right = Vec3::new(fl.z, 0.0, -fl.x);
        for (j, l) in cars.iter().enumerate() {
            if i == j {
                continue;
            }
            let rel = l.pos - f.pos;
            let z = rel.dot(fl);
            if z <= 0.0 || z >= MAX_Z || rel.y.abs() > MAX_DY {
                continue;
            }
            // The leader's wake needs it to be moving along our heading.
            let wake = (l.vel.dot(fl) / DRAFT_SPEED[0]).clamp(0.0, 1.0);
            let w = draft_weight(vf, z, rel.dot(right), f.width) * wake;
            if w > follow_w[i] {
                follow_w[i] = w;
            }
            if w > lead_w[j] {
                lead_w[j] = w;
            }
        }
    }
    (0..n)
        .map(|i| {
            let t = speed_t(speed[i]);
            let w = follow_w[i];
            let follow = AeroScale {
                drag: lerp(1.0, at(FOLLOW_DRAG, t), w),
                down: [lerp(1.0, lerp(FOLLOW_DOWN[0][0], FOLLOW_DOWN[1][0], t), w), lerp(1.0, lerp(FOLLOW_DOWN[0][1], FOLLOW_DOWN[1][1], t), w)],
            };
            let lw = lead_w[i];
            let lead = AeroScale { drag: lerp(1.0, LEAD_DRAG, lw), down: [lerp(1.0, LEAD_DOWN[0], lw), lerp(1.0, LEAD_DOWN[1], lw)] };
            mul(follow, lead)
        })
        .collect()
}

/// This tick's draft scales: one per AI car (in the order `ai` yields them) and the player's.
pub struct TickPhysics {
    pub ai: Vec<AeroScale>,
    pub player: AeroScale,
}

/// Compute the draft for all race cars (the AI cars, then the player's if there is one). Call once at the top of drive_ai,
/// before the cars step. All `ONE` with `FH1_DRAFT=0`.
pub fn begin_tick<'a>(ai: impl Iterator<Item = &'a Vehicle>, player: Option<&Vehicle>) -> TickPhysics {
    let mut cars: Vec<DraftCar> = ai.map(DraftCar::of).collect();
    let n_ai = cars.len();
    if !draft_on() {
        return TickPhysics { ai: vec![AeroScale::ONE; n_ai], player: AeroScale::ONE };
    }
    if let Some(p) = player {
        cars.push(DraftCar::of(p));
    }
    let mut scales = draft_scales(&cars);
    let player = if player.is_some() { scales.pop().unwrap_or(AeroScale::ONE) } else { AeroScale::ONE };
    TickPhysics { ai: scales, player }
}

/// Hooks on one AI car for this tick (`i` = its index in the order given to [`begin_tick`]).
pub fn set_ai(v: &mut Vehicle, tp: &TickPhysics, i: usize) {
    v.tcs_params = tcs_params();
    v.aero_scale = tp.ai.get(i).copied().unwrap_or(AeroScale::ONE);
}

/// The player's car draft for this tick.
pub fn set_player(v: &mut Vehicle, tp: &TickPhysics) {
    v.aero_scale = tp.player;
}

/// No race running: the player's car back to its own aero (drive_ai calls this when there are no AI cars).
pub fn clear(v: &mut Vehicle) {
    if v.aero_scale != AeroScale::ONE {
        v.aero_scale = AeroScale::ONE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 1.8;

    #[test]
    fn draft_weight_cases() {
        // Behind: 9 m ahead at 60 m/s is inside the fade, nearly full.
        assert!(draft_weight(60.0, 9.0, 0.0, W) > 0.9);
        // Inside the box start / behind us / past the end: nothing.
        assert_eq!(draft_weight(60.0, 4.0, 0.0, W), 0.0);
        assert_eq!(draft_weight(60.0, -10.0, 0.0, W), 0.0);
        assert_eq!(draft_weight(60.0, 70.0, 0.0, W), 0.0);
        // Far but inside: weaker than near.
        assert!(draft_weight(60.0, 40.0, 0.0, W) < draft_weight(60.0, 12.0, 0.0, W));
        assert!(draft_weight(60.0, 40.0, 0.0, W) > 0.0);
        // Beside: partly in the box 1.67 widths over, none past 2.5 widths.
        let side = draft_weight(60.0, 9.0, 3.0, W);
        assert!(side > 0.3 && side < 0.8, "{side}");
        assert_eq!(draft_weight(60.0, 9.0, 5.0, W), 0.0);
        // Slow: no wake at a standstill, little at 2 m/s.
        assert_eq!(draft_weight(0.0, 7.0, 0.0, W), 0.0);
        assert!(draft_weight(2.0, 6.0, 0.0, W) < 0.12);
        // Slower than DraftSpeed0 but moving: full box, weight limited by the speed fade only.
        assert!((draft_weight(DRAFT_SPEED[0], 6.0, 0.0, W) - 1.0).abs() < 1e-5);
    }

    fn car(z: f32, x: f32, speed: f32) -> DraftCar {
        DraftCar { pos: Vec3::new(x, 0.0, z), fwd: Vec3::NEG_Z, vel: Vec3::new(0.0, 0.0, -speed), width: W }
    }

    #[test]
    fn draft_scales_follower_and_leader() {
        // Follower at the origin heading -Z, leader 10 m ahead, both 60 m/s.
        let s = draft_scales(&[car(0.0, 0.0, 60.0), car(-10.0, 0.0, 60.0)]);
        assert!(s[0].drag > 0.72 && s[0].drag < 0.80, "{:?}", s[0]);
        assert!(s[0].down[0] < 0.6 && s[0].down[1] < 0.7 && s[0].down[1] > 0.61, "{:?}", s[0]);
        assert!(s[1].drag > 0.95 && s[1].drag < 0.97, "{:?}", s[1]);
        assert!(s[1].down[1] > 0.95 && s[1].down[1] < 1.0);
        // Far apart, beside, and a stopped leader: nothing.
        for other in [car(-100.0, 0.0, 60.0), car(-10.0, 6.0, 60.0), car(-10.0, 0.0, 0.0)] {
            let s = draft_scales(&[car(0.0, 0.0, 60.0), other]);
            assert_eq!(s[0], AeroScale::ONE);
            assert_eq!(s[1], AeroScale::ONE);
        }
        // A slow follower gets (almost) nothing; a train of three: the middle car has both scales.
        let s = draft_scales(&[car(0.0, 0.0, 0.5), car(-6.0, 0.0, 60.0)]);
        assert_eq!(s[0], AeroScale::ONE);
        let s = draft_scales(&[car(0.0, 0.0, 60.0), car(-10.0, 0.0, 60.0), car(-20.0, 0.0, 60.0)]);
        assert!(s[1].drag < s[0].drag && s[2].drag < 1.0 && s[2].drag > 0.95);
        assert!(s[2].down[0] < 1.0);
    }

    #[test]
    fn tcs_and_contact_selection() {
        assert_eq!(tcs_params_for(false), None);
        let p = tcs_params_for(true).unwrap();
        assert!((p.speed - 22.0 * 0.447_04).abs() < 1e-6);
        assert_eq!(p.full_effect, 0.25);
        // [FWD, RWD, AWD]
        assert_eq!(p.steer_scale, [1.7, 1.3, 0.71]);

        assert_eq!(contact_params_for(false, PairKind::AiAi), ContactParams::DEFAULT);
        assert_eq!(contact_params_for(false, PairKind::HumanAi), ContactParams::DEFAULT);
        let a = contact_params_for(true, PairKind::AiAi);
        assert_eq!((a.restitution, a.ang_scale, a.yaw_scale), (-0.1, 0.2, 0.2));
        assert_eq!(a.friction, ContactParams::DEFAULT.friction);
        let h = contact_params_for(true, PairKind::HumanAi);
        assert_eq!((h.ang_scale, h.yaw_scale), (0.2, 0.75));
        let t = contact_params_for(true, PairKind::AiTraffic);
        assert_eq!((t.ang_scale, t.yaw_scale), (0.05, 0.05));
    }
}
