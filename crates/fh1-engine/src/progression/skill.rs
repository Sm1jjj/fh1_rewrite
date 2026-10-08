//! Skill chains (docs/PROGRESSION.md "Skills"): FH1's free-roam / race feats from HorizonFeats.xml (grades and fame
//! values, VERIFIED on the EU disc), banked as popularity (Fame.xml ladder). `FH1_SKILLS=0` off.
//!
//! Skills (grade 1..4 thresholds -> fame): Drift 15-110 deg at >= 25 mph, distance 10/40/70/100 m -> 100/250/500/1000;
//! Air 0.2/0.4/0.6/0.8 s -> 100/150/250/500; Near miss within 4 m at >= 50 mph, by speed 50/100/150/200 mph ->
//! 100/250/500/1000; Pass, by speed difference 0/10/15/20 mph -> 100/150/250/500 (PassResetTime 4 s per car); Clean
//! speed every 450 m above 100 mph, by speed 100/150/175/200 -> 100/150/250/1000; Draft >= 50 mph, 1.5/3/4.5/6 s ->
//! 100/150/250/500; Burnout under 5 mph, 0.5/2/3.5/5 s -> 100/150/250/500. Combos (1000 each): DareDevil (3 near misses
//! in 10 s), Stuntman (air + near miss, 2.5 s), Superman (air + pass, 2.5 s), Kangaroo (3 air, 5 s), Triple pass (3 passes,
//! 2 s), Show off (drift then pass, 2.5 s), Lucky escape (drift then near miss, 2.5 s), Slingshot (draft then pass, 5 s).
//! Also Quick off the mark (burnout then clean speed, 35 s, in order) and Ebisu style (drift of grade >= 2 and air within
//! 0.5 s). RetriggerDelay (HorizonFeats): burnout 5 s, DareDevil 5 s, the other combos 1 s.
//! Multiplier: x2 at 3 skills, x3 at 6 (HorizonFeats Multiplier, the cap). `FH1_SKILL_MULT_EXT=1` = ours before P9 (x4 at
//! 10, x5 at 15).
//! Chain window: 3 s after the last award (GlobalRegistry UI/RaceFeats/Animation/FadeOut_Duration; `FH1_SKILL_WINDOW=<s>`,
//! 4 = before P9), paused while a skill runs (INFERRED: xex not read yet). The HUD total counts up over 1.6 s
//! (Count_Up_Duration) and the multiplier changes 0.24 s after the skill that raised it (WaitTimeToUpdateMultiplier):
//! `Skills::shown_total` / `shown_mult`.
//! A crash (> 12 mph speed change in 0.15 s above 10 mph, WorldCollisionThreshold / MinSpeedToCancelFeats) loses the
//! chain and blocks skills for 3 s (DelayAfterCancelling). Not in the data (ours): the near-miss gap test (centre
//! distance), the draft cone.
//! Rewind-friendly: holding rewind pauses the chain, and rewinding within 15 s of a crash gives the lost chain back.

use std::collections::HashMap;

use bevy::input::gamepad::{Gamepad, GamepadButton};
use bevy::prelude::*;

use fh1_engine::ai::AiCar;

use super::{fmt_num, Banners, Profile};
use crate::race::Events;
use crate::Car;

const MPH: f32 = 0.44704;
/// GlobalRegistry UI/RaceFeats/Animation/FadeOut_Duration (VERIFIED value; its use as the bank delay is INFERRED).
const CHAIN_WINDOW_S: f32 = 3.0;
/// UI/RaceFeats/Animation/Count_Up_Duration: the shown total runs up to the chain total over this long.
const COUNT_UP_S: f32 = 1.6;
/// UI/RaceFeats/Animation/WaitTimeToUpdateMultiplier.
const MULT_WAIT_S: f32 = 0.24;
/// HorizonFeats RetriggerDelay: Burnout and DareDevil 5 s, the other combos 1 s.
const BURNOUT_RETRIGGER_S: f32 = 5.0;
const DAREDEVIL_RETRIGGER_S: f32 = 5.0;
const COMBO_RETRIGGER_S: f32 = 1.0;
const CRASH_DV: f32 = 12.0 * MPH;
const CRASH_MIN_SPEED: f32 = 10.0 * MPH;
const CANCEL_DELAY_S: f32 = 3.0;
const REWIND_RESTORE_S: f32 = 15.0;
const NEAR_MISS_M: f32 = 4.0;

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::enabled() && std::env::var("FH1_SKILLS").map_or(true, |v| v != "0"))
}

/// The chain window (s): FH1's 3 s, `FH1_SKILL_WINDOW` overrides.
fn chain_window_s() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_SKILL_WINDOW").ok().and_then(|v| v.parse().ok()).filter(|v: &f32| *v > 0.0).unwrap_or(CHAIN_WINDOW_S))
}

/// `FH1_SKILL_MULT_EXT=1`: our x4 / x5 steps past FH1's x3 cap (before P9).
fn mult_ext() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_SKILL_MULT_EXT").is_ok_and(|v| v == "1"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SkillKind {
    Drift,
    Air,
    NearMiss,
    Pass,
    CleanSpeed,
    Draft,
    Burnout,
    Combo,
}

impl SkillKind {
    fn label(self) -> &'static str {
        match self {
            Self::Drift => "DRIFT",
            Self::Air => "AIR",
            Self::NearMiss => "NEAR MISS",
            Self::Pass => "PASS",
            Self::CleanSpeed => "SPEED",
            Self::Draft => "DRAFTING",
            Self::Burnout => "BURNOUT",
            Self::Combo => "COMBO",
        }
    }

    /// (grade thresholds ascending, fame per grade).
    fn grades(self) -> ([f32; 4], [u32; 4]) {
        match self {
            Self::Drift => ([10.0, 40.0, 70.0, 100.0], [100, 250, 500, 1000]),
            Self::Air => ([0.2, 0.4, 0.6, 0.8], [100, 150, 250, 500]),
            Self::NearMiss => ([50.0, 100.0, 150.0, 200.0], [100, 250, 500, 1000]),
            Self::Pass => ([0.0, 10.0, 15.0, 20.0], [100, 150, 250, 500]),
            Self::CleanSpeed => ([100.0, 150.0, 175.0, 200.0], [100, 150, 250, 1000]),
            Self::Draft => ([1.5, 3.0, 4.5, 6.0], [100, 150, 250, 500]),
            Self::Burnout => ([0.5, 2.0, 3.5, 5.0], [100, 150, 250, 500]),
            Self::Combo => ([0.0; 4], [1000; 4]),
        }
    }

    /// Fame for a measured value (None below grade 1).
    fn fame(self, value: f32) -> Option<u32> {
        let (t, f) = self.grades();
        (0..4).rev().find(|&g| value >= t[g]).map(|g| f[g])
    }
}

/// One award in the chain.
#[derive(Clone, Debug)]
pub struct Award {
    pub label: String,
    pub fame: u32,
    pub kind: SkillKind,
    pub at: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Chain {
    pub awards: Vec<Award>,
    pub total: u64,
    /// Seconds left before the chain banks.
    pub window: f32,
}

impl Chain {
    pub fn multiplier(&self) -> u32 {
        match self.awards.len() {
            0..=2 => 1,
            3..=5 => 2,
            6..=9 => 3,
            _ if !mult_ext() => 3,
            10..=14 => 4,
            _ => 5,
        }
    }

    /// Skills still needed for the next multiplier step (None at the cap).
    pub fn to_next_multiplier(&self) -> Option<usize> {
        let n = self.awards.len();
        let steps: &[usize] = if mult_ext() { &[3, 6, 10, 15] } else { &[3, 6] };
        steps.iter().find(|&&s| s > n).map(|s| s - n)
    }

    pub fn value(&self) -> u64 {
        self.total * self.multiplier() as u64
    }
}

/// Skill -> HUD, on the frame it happens (e4's Anark skill HUD animates on these).
#[derive(Message, Clone, Debug)]
pub enum SkillEvent {
    Award { label: String, fame: u32, kind: SkillKind, combo: bool },
    Banked { value: u64, mult: u32 },
    Lost { value: u64 },
    /// A skill in progress (every frame while it runs): "DRIFT", "42 m".
    Live { label: String, value_text: String },
}

/// Insert this resource when another skill HUD draws the chain (e4's ui/skillhud.rs): the text HUD here then stays empty.
#[derive(Resource, Default)]
pub struct ExternalSkillHud;

/// Live state of the skill detector.
#[derive(Resource, Default)]
pub struct Skills {
    pub chain: Chain,
    /// Events since the last flush (sent as `SkillEvent` messages by `flush_skill_events`).
    outbox: Vec<SkillEvent>,
    /// A skill in progress: (label, live value text) for the HUD.
    pub live: Option<String>,
    /// Last banked / lost chain message: (text, seconds left).
    pub toast: Option<(String, f32)>,
    lost: Option<(Chain, f32)>,
    cancel_s: f32,
    /// HUD count-up: the shown total, the value it started from and the time (s) since the target changed.
    shown: f64,
    count_from: f64,
    count_t: f32,
    count_target: u64,
    /// Shown multiplier and the time (s) a new one has been waiting (`MULT_WAIT_S`).
    shown_mult: u32,
    mult_wait: f32,
    /// RetriggerDelay: time (s) of the last award per retriggered skill / combo label.
    last_award: HashMap<String, f32>,
    // Running skills.
    drift_m: f32,
    drift_off_s: f32,
    air_s: f32,
    draft_s: f32,
    draft_off_s: f32,
    speed_m: f32,
    speed_t: f32,
    burnout_s: f32,
    /// Velocity samples (time, velocity) over the last 0.15 s (crash test).
    vel_hist: Vec<(f32, Vec3)>,
    /// Last position: a jump means a teleport (race start / reset / fast travel), not a crash.
    last_pos: Option<Vec3>,
    /// Per other car: (last longitudinal offset, closest distance this approach, pass cooldown, near-miss cooldown).
    others: HashMap<Entity, (f32, f32, f32, f32)>,
}

impl Skills {
    /// Chain window left, 0..1 (1 = just extended).
    pub fn chain_window_frac(&self) -> f32 {
        (self.chain.window / chain_window_s()).clamp(0.0, 1.0)
    }

    /// The chain total as the HUD shows it: counting up to `chain.total` over `COUNT_UP_S` (Count_Up_Duration).
    pub fn shown_total(&self) -> u64 {
        self.shown.round() as u64
    }

    /// The multiplier as the HUD shows it: a new one shows `MULT_WAIT_S` after the skill that raised it.
    pub fn shown_mult(&self) -> u32 {
        self.shown_mult.max(1)
    }

    /// Advance the count-up and the multiplier delay (every frame).
    fn tick_display(&mut self, dt: f32) {
        let target = self.chain.total;
        if target != self.count_target {
            // A new award: count from where the display is now.
            self.count_from = if target < self.count_target { 0.0 } else { self.shown };
            self.count_target = target;
            self.count_t = 0.0;
        }
        self.count_t += dt;
        let k = (self.count_t / COUNT_UP_S).clamp(0.0, 1.0) as f64;
        self.shown = self.count_from + (target as f64 - self.count_from) * k;
        let m = self.chain.multiplier();
        if m < self.shown_mult || self.shown_mult == 0 {
            self.shown_mult = m;
            self.mult_wait = 0.0;
        } else if m > self.shown_mult {
            self.mult_wait += dt;
            if self.mult_wait >= MULT_WAIT_S {
                self.shown_mult = m;
                self.mult_wait = 0.0;
            }
        }
    }

    /// RetriggerDelay: true (and noted) when `key` may award now.
    fn retrigger_ok(&mut self, key: &str, delay: f32, now: f32) -> bool {
        if self.last_award.get(key).is_some_and(|&t| now - t < delay) {
            return false;
        }
        self.last_award.insert(key.to_owned(), now);
        true
    }

    fn award(&mut self, kind: SkillKind, fame: u32, label: String, now: f32) {
        self.outbox.push(SkillEvent::Award { label: label.clone(), fame, kind, combo: false });
        self.chain.awards.push(Award { label, fame, kind, at: now });
        self.chain.total += fame as u64;
        self.chain.window = chain_window_s();
        self.combos(now);
    }

    fn combos(&mut self, now: f32) {
        let recent = |k: SkillKind, within: f32| self.chain.awards.iter().filter(|a| a.kind == k && now - a.at <= within).count();
        let last = self.chain.awards.last().map(|a| a.kind);
        let prev_within = |k: SkillKind, within: f32| self.chain.awards.iter().rev().skip(1).any(|a| a.kind == k && now - a.at <= within);
        let mut found: Vec<&str> = Vec::new();
        let last_award = self.chain.awards.last().cloned();
        match last {
            Some(SkillKind::NearMiss) => {
                if recent(SkillKind::NearMiss, 10.0) == 3 {
                    found.push("DAREDEVIL");
                }
                if prev_within(SkillKind::Air, 2.5) {
                    found.push("STUNTMAN");
                }
                if prev_within(SkillKind::Drift, 2.5) {
                    found.push("LUCKY ESCAPE");
                }
            }
            Some(SkillKind::Air) => {
                if recent(SkillKind::Air, 5.0) == 3 {
                    found.push("KANGAROO");
                }
                // Ebisu style (air after a grade >= 2 drift).
                if self.chain.awards.iter().rev().skip(1).any(|a| a.kind == SkillKind::Drift && a.fame >= 250 && now - a.at <= 0.5) {
                    found.push("EBISU STYLE");
                }
                if prev_within(SkillKind::NearMiss, 2.5) {
                    found.push("STUNTMAN");
                }
                if prev_within(SkillKind::Pass, 2.5) {
                    found.push("SUPERMAN");
                }
            }
            Some(SkillKind::CleanSpeed) => {
                // Quick off the mark: burnout, then clean speed within 35 s (InOrder); once per burnout (INFERRED).
                let burnout = self.chain.awards.iter().rev().find(|a| a.kind == SkillKind::Burnout && now - a.at <= 35.0).map(|a| a.at);
                if burnout.is_some_and(|tb| !self.chain.awards.iter().any(|a| a.label == "QUICK OFF THE MARK" && a.at >= tb)) {
                    found.push("QUICK OFF THE MARK");
                }
            }
            Some(SkillKind::Drift) => {
                // Ebisu style: a drift of grade >= 2 (>= 250) and air within 0.5 s, either order.
                if last_award.as_ref().is_some_and(|a| a.fame >= 250) && prev_within(SkillKind::Air, 0.5) {
                    found.push("EBISU STYLE");
                }
            }
            Some(SkillKind::Pass) => {
                if recent(SkillKind::Pass, 2.0) == 3 {
                    found.push("TRIPLE PASS");
                }
                if prev_within(SkillKind::Air, 2.5) {
                    found.push("SUPERMAN");
                }
                if prev_within(SkillKind::Drift, 2.5) {
                    found.push("SHOW OFF");
                }
                if prev_within(SkillKind::Draft, 5.0) {
                    found.push("SLINGSHOT");
                }
            }
            _ => {}
        }
        for name in found {
            let delay = if name == "DAREDEVIL" { DAREDEVIL_RETRIGGER_S } else { COMBO_RETRIGGER_S };
            if !self.retrigger_ok(name, delay, now) {
                continue;
            }
            self.outbox.push(SkillEvent::Award { label: name.into(), fame: 1000, kind: SkillKind::Combo, combo: true });
            self.chain.awards.push(Award { label: name.into(), fame: 1000, kind: SkillKind::Combo, at: now });
            self.chain.total += 1000;
        }
    }

    fn cancel_running(&mut self) {
        self.drift_m = 0.0;
        self.air_s = 0.0;
        self.draft_s = 0.0;
        self.speed_m = 0.0;
        self.speed_t = 0.0;
        self.burnout_s = 0.0;
        self.live = None;
    }
}

#[derive(Component)]
pub struct SkillHud;

#[derive(Component)]
pub struct BannerHud;

pub fn spawn_skill_hud(mut commands: Commands, font: Option<Res<crate::ui::UiFont>>) {
    let f = |px: f32| font.as_ref().map_or_else(|| TextFont { font_size: bevy::text::FontSize::Px(px), ..default() }, |f| f.text(px));
    let shadow = TextShadow { offset: Vec2::new(2.0, 2.0), color: Color::BLACK.with_alpha(0.8) };
    commands.spawn((
        SkillHud,
        Text::new(""),
        f(26.0),
        TextColor(Color::WHITE),
        shadow,
        TextLayout::justify(Justify::Center),
        Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), top: Val::Percent(8.0), justify_content: JustifyContent::Center, ..default() },
    ));
    commands.spawn((
        BannerHud,
        Text::new(""),
        f(40.0),
        TextColor(Color::srgb(1.0, 0.85, 0.1)),
        shadow,
        TextLayout::justify(Justify::Center),
        Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), top: Val::Percent(32.0), justify_content: JustifyContent::Center, ..default() },
    ));
}

/// Detect skills on the player's car, run the chain, bank it into popularity.
#[allow(clippy::too_many_arguments)]
pub fn detect_skills(
    mut sk: ResMut<Skills>,
    cars: Query<&Car>,
    others: Query<(Entity, &AiCar)>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    pads: Query<&Gamepad>,
    settings: Option<Res<crate::ui::Settings>>,
    events: Res<Events>,
    mut profile: ResMut<Profile>,
    mut banners: ResMut<Banners>,
) {
    if !enabled() {
        return;
    }
    let Some(car) = cars.iter().next() else { return };
    let v = &car.0;
    let dt = time.delta_secs().min(0.1);
    let now = time.elapsed_secs();
    if let Some((_, t)) = sk.toast.as_mut() {
        *t -= dt;
        if *t <= 0.0 {
            sk.toast = None;
        }
    }

    // Rewind (ui/assists.rs bindings: hold X key / Back): pause the chain; give back a chain lost to a recent crash.
    let rewinding = settings.as_ref().is_some_and(|s| s.rewind) && (keys.pressed(KeyCode::KeyX) || pads.iter().any(|p| p.pressed(GamepadButton::Select)));
    if rewinding {
        if let Some((chain, at)) = sk.lost.take() {
            if now - at < REWIND_RESTORE_S {
                sk.chain = Chain { window: chain_window_s(), ..chain };
                sk.cancel_s = 0.0;
                sk.toast = Some(("CHAIN RESTORED".into(), 2.0));
            }
        }
        sk.vel_hist.clear();
        sk.cancel_running();
        return;
    }

    let vel = v.velocity;
    let speed = Vec2::new(vel.x, vel.z).length();
    // Teleported (race grid, reset to track, fast travel): forget the running skills and the velocity history, keep the chain.
    let jumped = sk.last_pos.replace(v.position).is_some_and(|p| p.distance(v.position) > 15.0);
    if jumped {
        sk.vel_hist.clear();
        sk.others.clear();
        sk.cancel_running();
        return;
    }
    // Crash: a big change of horizontal velocity within 0.15 s.
    sk.vel_hist.push((now, vel));
    sk.vel_hist.retain(|(t, _)| now - t <= 0.15);
    let crashed = sk.vel_hist.first().is_some_and(|(_, v0)| {
        let dv = Vec2::new(vel.x - v0.x, vel.z - v0.z).length();
        dv > CRASH_DV && Vec2::new(v0.x, v0.z).length() > CRASH_MIN_SPEED
    });
    if crashed {
        sk.vel_hist.clear();
        if sk.cancel_s <= 0.0 {
            if !sk.chain.awards.is_empty() {
                let lost = std::mem::take(&mut sk.chain);
                sk.outbox.push(SkillEvent::Lost { value: lost.value() });
                sk.toast = Some((format!("CRASH - chain lost ({})", fmt_num(lost.value() as i64)), 2.5));
                profile.data.skills.chains_lost += 1;
                sk.lost = Some((lost, now));
            }
            sk.cancel_running();
        }
        sk.cancel_s = CANCEL_DELAY_S;
    }
    if sk.cancel_s > 0.0 {
        sk.cancel_s -= dt;
        return;
    }

    let fwd = v.rotation * Vec3::NEG_Z;
    let fwd2 = Vec2::new(fwd.x, fwd.z).normalize_or_zero();
    let grounded = v.wheels.iter().filter(|w| w.grounded).count();
    let mut running = false;
    let mut live = None;

    // Drift.
    let slip = if speed > 1.0 { fwd2.angle_to(Vec2::new(vel.x, vel.z) / speed).abs().to_degrees() } else { 0.0 };
    if grounded >= 2 && speed >= 25.0 * MPH && (15.0..=110.0).contains(&slip) {
        sk.drift_m += speed * dt;
        sk.drift_off_s = 0.0;
    } else if sk.drift_m > 0.0 {
        sk.drift_off_s += dt;
        if sk.drift_off_s > 0.3 {
            let m = std::mem::take(&mut sk.drift_m);
            if let Some(f) = SkillKind::Drift.fame(m) {
                sk.award(SkillKind::Drift, f, format!("DRIFT {m:.0} m"), now);
            }
        }
    }
    if sk.drift_m > 0.0 {
        running = true;
        live = Some(format!("DRIFT  {:.0} m", sk.drift_m));
    }

    // Air.
    if grounded == 0 && speed > 5.0 {
        sk.air_s += dt;
    } else if sk.air_s > 0.0 {
        let s = std::mem::take(&mut sk.air_s);
        if let Some(f) = SkillKind::Air.fame(s) {
            sk.award(SkillKind::Air, f, format!("AIR {s:.1} s"), now);
        }
    }
    if sk.air_s > 0.15 {
        running = true;
        live = Some(format!("AIR  {:.1} s", sk.air_s));
    }

    // Clean speed: every 450 m above 100 mph.
    if speed >= 100.0 * MPH {
        sk.speed_m += speed * dt;
        sk.speed_t += dt;
        running = true;
        if sk.speed_m >= 450.0 {
            let mph = sk.speed_m / sk.speed_t.max(1e-3) / MPH;
            if let Some(f) = SkillKind::CleanSpeed.fame(mph) {
                sk.award(SkillKind::CleanSpeed, f, format!("SPEED {mph:.0} mph"), now);
            }
            sk.speed_m = 0.0;
            sk.speed_t = 0.0;
        }
    } else {
        sk.speed_m = 0.0;
        sk.speed_t = 0.0;
    }

    // Burnout: wheelspin while (nearly) stopped.
    let spin = v.wheels.iter().enumerate().filter(|(_, w)| w.grounded).map(|(i, w)| (w.omega * v.data.tyre_radius[i / 2]).abs() - speed).fold(0.0f32, f32::max);
    if speed < 5.0 * MPH && spin > 10.0 * MPH {
        sk.burnout_s += dt;
        running = true;
        live = Some(format!("BURNOUT  {:.1} s", sk.burnout_s));
    } else if sk.burnout_s > 0.0 {
        let s = std::mem::take(&mut sk.burnout_s);
        if let Some(f) = SkillKind::Burnout.fame(s) {
            if sk.retrigger_ok("BURNOUT", BURNOUT_RETRIGGER_S, now) {
                sk.award(SkillKind::Burnout, f, format!("BURNOUT {s:.1} s"), now);
            }
        }
    }

    // Other cars: near miss, pass, draft.
    let mut drafting = false;
    let mut seen = Vec::new();
    for (e, other) in &others {
        let o = &other.0;
        let d = o.position - v.position;
        if d.length_squared() > 60.0 * 60.0 {
            continue;
        }
        seen.push(e);
        let along = Vec2::new(d.x, d.z).dot(fwd2);
        let lateral = Vec2::new(d.x, d.z).perp_dot(fwd2).abs();
        let dist = d.length();
        let entry = *sk.others.get(&e).unwrap_or(&(along, f32::INFINITY, 0.0, 0.0));
        let (prev_along, mut closest, mut pass_cd, mut miss_cd) = entry;
        pass_cd = (pass_cd - dt).max(0.0);
        miss_cd = (miss_cd - dt).max(0.0);
        let rel = (vel - o.velocity).length();
        // Near miss: the closest approach of a pass-by (gap ~ centre distance - car width), judged when it opens again.
        if dist < 12.0 {
            closest = closest.min(dist);
        } else if closest.is_finite() {
            let gap = closest - 1.9;
            if gap < NEAR_MISS_M && miss_cd <= 0.0 && speed >= 50.0 * MPH && rel > 3.0 {
                if let Some(f) = SkillKind::NearMiss.fame(speed / MPH) {
                    sk.award(SkillKind::NearMiss, f, "NEAR MISS".into(), now);
                }
                miss_cd = 4.0;
            }
            closest = f32::INFINITY;
        }
        // Pass: from ahead to behind along the player's heading, alongside.
        if prev_along > 2.0 && along < -2.0 && lateral < 12.0 && pass_cd <= 0.0 {
            let diff = (speed - Vec2::new(o.velocity.x, o.velocity.z).dot(fwd2)) / MPH;
            if diff > 0.0 {
                if let Some(f) = SkillKind::Pass.fame(diff) {
                    sk.award(SkillKind::Pass, f, "PASS".into(), now);
                }
                pass_cd = 4.0;
            }
        }
        // Draft: tucked in behind a car going the same way.
        let same_way = o.velocity.normalize_or_zero().dot(vel.normalize_or_zero()) > 0.8;
        if speed >= 50.0 * MPH && (3.0..20.0).contains(&along) && lateral < 1.8 && same_way {
            drafting = true;
        }
        sk.others.insert(e, (along, closest, pass_cd, miss_cd));
    }
    sk.others.retain(|e, _| seen.contains(e));
    if drafting {
        sk.draft_s += dt;
        sk.draft_off_s = 0.0;
    } else if sk.draft_s > 0.0 {
        sk.draft_off_s += dt;
        if sk.draft_off_s > 0.5 {
            let s = std::mem::take(&mut sk.draft_s);
            if let Some(f) = SkillKind::Draft.fame(s) {
                sk.award(SkillKind::Draft, f, format!("DRAFTING {s:.1} s"), now);
            }
        }
    }
    if sk.draft_s > 0.5 {
        running = true;
        live = live.or(Some(format!("DRAFTING  {:.1} s", sk.draft_s)));
    }
    if let Some(l) = &live {
        let (label, value) = l.split_once("  ").unwrap_or((l.as_str(), ""));
        let ev = SkillEvent::Live { label: label.to_owned(), value_text: value.trim().to_owned() };
        sk.outbox.push(ev);
    }
    sk.live = live;

    sk.tick_display(dt);
    // Chain window; bank into popularity when it runs out.
    if !sk.chain.awards.is_empty() && !running {
        sk.chain.window -= dt;
        if sk.chain.window <= 0.0 {
            let chain = std::mem::take(&mut sk.chain);
            sk.outbox.push(SkillEvent::Banked { value: chain.value(), mult: chain.multiplier() });
            bank(&chain, &events, &mut profile, &mut banners);
            sk.toast = Some((format!("+{} POPULARITY", fmt_num(chain.value() as i64)), 2.5));
        }
    }
}

/// Fame.xml ranks that carry a dialogueEvent / radioFestivalUpdate (VERIFIED on the EU disc): 249, 240, 230, 175, 155,
/// 150, 125, 105, 100, 75, 55, 50, 25, 13, 6, 1. Only these rank-ups get a notification; every rank still levels the
/// popularity bar (P9, INFERRED rule until the xex is read: the game's notification for a plain rank-up is unconfirmed).
/// `FH1_RANKUP_NOTIFY=all` = every rank (before P9).
const MILESTONE_RANKS: [u32; 16] = [249, 240, 230, 175, 155, 150, 125, 105, 100, 75, 55, 50, 25, 13, 6, 1];

pub fn milestone_rank(rank: u32) -> bool {
    static ALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALL.get_or_init(|| std::env::var("FH1_RANKUP_NOTIFY").is_ok_and(|v| v == "all")) || MILESTONE_RANKS.contains(&rank)
}

fn bank(chain: &Chain, events: &Events, profile: &mut Profile, banners: &mut Banners) {
    let c = &events.career;
    let value = chain.value();
    let rank_before = c.rank(profile.data.fame);
    let p = &mut profile.data;
    p.fame += value;
    banners.1.push(super::CareerNotice::Fame { amount: value });
    p.skills.chains_banked += 1;
    p.skills.best_chain = p.skills.best_chain.max(value);
    for a in &chain.awards {
        let key = if a.kind == SkillKind::Combo { a.label.clone() } else { a.kind.label().to_owned() };
        *p.skills.counts.entry(key).or_default() += 1;
    }
    let rank = c.rank(p.fame);
    if rank < rank_before {
        let board = super::rival_board(c);
        // Several ranks in one bank: the notification goes to the best milestone crossed (if any).
        let milestone = (rank..rank_before).filter(|&r| milestone_rank(r)).min();
        if let Some(m) = milestone {
            let passed = board.get(m as usize).and_then(|id| c.driver_name(*id)).map(|n| format!("  passed {n}")).unwrap_or_default();
            banners.push(format!("POPULARITY #{m}{passed}"));
        }
        let passed = board.get(rank as usize).and_then(|id| c.driver_name(*id)).map(str::to_owned);
        banners.1.push(super::CareerNotice::RankUp { rank, passed, milestone: milestone.is_some() });
        // Popularity-gated events (showcases, exhibitions) that this bank opened.
        for def in events.races.iter().filter(|d| d.popularity_req > 0 && rank <= d.popularity_req && rank_before > d.popularity_req) {
            banners.push(format!("EVENT UNLOCKED  {}", def.name));
            banners.1.push(super::CareerNotice::EventUnlocked { name: def.name.clone() });
        }
        super::pay_rank_milestones(profile, rank, banners);
    }
    profile.commit();
}

/// Send the outbox as `SkillEvent` messages.
pub fn flush_skill_events(mut sk: ResMut<Skills>, mut out: MessageWriter<SkillEvent>) {
    if !sk.outbox.is_empty() {
        out.write_batch(std::mem::take(&mut sk.outbox));
    }
}

pub fn draw_skill_hud(
    external: Option<Res<ExternalSkillHud>>,
    sk: Res<Skills>,
    banners: Res<super::Banners>,
    profile: Res<Profile>,
    events: Res<Events>,
    mut texts: Query<(&mut Text, Option<&SkillHud>, Option<&BannerHud>, &mut Node), Or<(With<SkillHud>, With<BannerHud>)>>,
) {
    let mut s = String::new();
    if enabled() && external.is_none() {
        if let Some(l) = &sk.live {
            s += l;
            s += "\n";
        }
        let ch = &sk.chain;
        if !ch.awards.is_empty() {
            let recent: Vec<String> = ch.awards.iter().rev().take(3).map(|a| format!("{} +{}", a.label, a.fame)).collect();
            s += &recent.join("   ");
            let bar_n = (sk.chain_window_frac() * 10.0).round() as usize;
            s += &format!("\nx{}  CHAIN {}  {}{}", sk.shown_mult(), fmt_num((sk.shown_total() * sk.shown_mult() as u64) as i64), "|".repeat(bar_n), ".".repeat(10 - bar_n));
            if let Some(n) = ch.to_next_multiplier() {
                s += &format!("   ({n} to x{})", ch.multiplier() + 1);
            }
        } else if let Some((t, _)) = &sk.toast {
            s += t;
            s += &format!("\nPopularity #{}", events.career.rank(profile.data.fame));
        }
    }
    let b = if external.is_some() { String::new() } else { banners.0.first().map(|b| b.0.clone()).unwrap_or_default() };
    for (mut text, skill, banner, mut node) in &mut texts {
        let want = if skill.is_some() { &s } else if banner.is_some() { &b } else { continue };
        let display = if want.is_empty() && crate::ui::scene::fastpath() { Display::None } else { Display::Flex };
        if node.display != display {
            node.display = display;
        }
        if &text.0 != want {
            text.0 = want.clone();
        }
    }
}
