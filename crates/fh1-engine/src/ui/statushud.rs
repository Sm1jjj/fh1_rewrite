//! Free-roam status strip, top right: credits, popularity (fame) and the worn wristband, small and low-contrast.
//! `CR 123,456   POP 4,820   WB 3 BLUE` in the HUD font (Bahnschrift), wristband in its CareerWristbandLevels colour.
//! Sources: `Profile.data.credits` / `.fame` / `.xp`, tier = `Events.career.tier(xp)` (the career screen's own lookup).
//! A change counts up over [`COUNT_S`] with a brief brightening, then it fades to [`IDLE_ALPHA`].
//! Layout: absolute, `right` 12 px, `top` 5 px, one row of 13 px text (ends at ~21 px: above the circuit lap timer's
//! panel at top 24 px, ui/laptimer.rs). Hidden with the rest of the HUD (Settings.hud off, menu, photo mode, cutscene
//! HUD hide, loading, intro choice) and during any race (the race HUD owns the screen; the career screen has the numbers).
//! `FH1_STATUS_HUD=0` = off.

use bevy::prelude::*;

use super::{Menu, Settings, UiFont};
use crate::camera::CameraRig;

/// Seconds a changed value takes to count up.
const COUNT_S: f32 = 1.5;
/// Seconds the strip stays bright after a change before fading.
const HOLD_S: f32 = 2.5;
/// Resting opacity.
const IDLE_ALPHA: f32 = 0.38;
const FADE_PER_S: f32 = 0.6;
const FONT_PX: f32 = 13.0;

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_STATUS_HUD").map_or(true, |v| v != "0"))
}

pub struct StatusHudPlugin;

impl Plugin for StatusHudPlugin {
    fn build(&self, app: &mut App) {
        if enabled() {
            app.add_systems(Startup, spawn).add_systems(Update, update.after(crate::sync_visuals));
        }
    }
}

/// Thousands separators ("123,456", "-1,200").
pub fn group(n: i64) -> String {
    crate::progression::fmt_num(n)
}

pub fn fmt_credits(n: i64) -> String {
    format!("CR {}", group(n))
}

pub fn fmt_pop(n: u64) -> String {
    format!("POP {}", group(n.min(i64::MAX as u64) as i64))
}

/// "WB 3 BLUE" for tier index 2 (tier 0 = Yellow = level 1).
pub fn fmt_wristband(tier: usize) -> String {
    let t = tier.min(6);
    format!("WB {} {}", t + 1, crate::progression::TIER_NAMES[t].to_uppercase())
}

/// Eased value between `from` and `to` after `t` of `COUNT_S` seconds.
pub fn count_up(from: i64, to: i64, t: f32) -> i64 {
    let k = (t / COUNT_S).clamp(0.0, 1.0);
    let e = 1.0 - (1.0 - k).powi(3);
    from + ((to - from) as f64 * e as f64).round() as i64
}

#[derive(Component)]
struct StatusRoot;

#[derive(Component, Clone, Copy, PartialEq)]
enum Cell {
    Credits,
    Pop,
    Band,
}

/// One animated number: the value shown, where it started, where it is going and the time since it changed.
#[derive(Default, Clone, Copy)]
struct Anim {
    from: i64,
    to: i64,
    t: f32,
    init: bool,
}

impl Anim {
    fn set(&mut self, target: i64) -> bool {
        if !self.init {
            *self = Anim { from: target, to: target, t: COUNT_S, init: true };
            return false;
        }
        if target == self.to {
            return false;
        }
        self.from = self.shown();
        self.to = target;
        self.t = 0.0;
        true
    }

    fn shown(&self) -> i64 {
        count_up(self.from, self.to, self.t)
    }
}

#[derive(Default)]
struct State {
    credits: Anim,
    pop: Anim,
    tier: Option<usize>,
    alpha: f32,
    since_change: f32,
}

fn spawn(mut commands: Commands, font: Res<UiFont>) {
    let dim = Color::srgba(1.0, 1.0, 1.0, IDLE_ALPHA);
    commands
        .spawn((
            StatusRoot,
            Node { position_type: PositionType::Absolute, right: Val::Px(12.0), top: Val::Px(5.0), column_gap: Val::Px(12.0), ..default() },
            Visibility::Hidden,
            GlobalZIndex(5),
        ))
        .with_children(|p| {
            for c in [Cell::Credits, Cell::Pop, Cell::Band] {
                p.spawn((c, Text::new(""), font.text(FONT_PX), TextColor(dim)));
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn update(
    time: Res<Time<Real>>,
    settings: Res<Settings>,
    menu: Res<Menu>,
    rig: Res<CameraRig>,
    profile: Option<Res<crate::progression::Profile>>,
    events: Option<Res<crate::race::Events>>,
    race: Option<Res<crate::race::RaceState>>,
    mut root: Query<&mut Visibility, With<StatusRoot>>,
    mut cells: Query<(&Cell, &mut Text, &mut TextColor)>,
    mut st: Local<State>,
) {
    let Ok(mut vis) = root.single_mut() else { return };
    let racing = race.is_some_and(|r| !matches!(r.phase, crate::race::RacePhase::Idle));
    let want = settings.hud
        && !menu.open
        && !rig.photo
        && !racing
        && !super::loading::blocking()
        && !crate::cutscene::active()
        && !super::intro::choice_open()
        && !crate::cutscene::hide_hud();
    let target = if want { Visibility::Inherited } else { Visibility::Hidden };
    if *vis != target {
        *vis = target;
    }
    let (Some(profile), Some(events)) = (profile, events) else { return };
    if !want {
        return;
    }
    let dt = time.delta_secs();
    let d = &profile.data;
    let mut changed = st.credits.set(d.credits);
    changed |= st.pop.set(d.fame.min(i64::MAX as u64) as i64);
    let tier = events.career.tier(d.xp).min(6);
    if st.tier.replace(tier).is_some_and(|t| t != tier) {
        changed = true;
    }
    st.credits.t += dt;
    st.pop.t += dt;
    if changed {
        st.since_change = 0.0;
        st.alpha = 0.9;
    } else {
        st.since_change += dt;
        if st.since_change > HOLD_S {
            st.alpha = (st.alpha - FADE_PER_S * dt).max(IDLE_ALPHA);
        }
    }
    let rising = |a: &Anim| a.t < COUNT_S;
    for (cell, mut text, mut color) in &mut cells {
        let (s, c) = match cell {
            Cell::Credits => (fmt_credits(st.credits.shown()), if rising(&st.credits) { Color::srgb(0.75, 1.0, 0.8) } else { Color::WHITE }),
            Cell::Pop => (fmt_pop(st.pop.shown().max(0) as u64), if rising(&st.pop) { Color::srgb(0.75, 1.0, 0.8) } else { Color::WHITE }),
            Cell::Band => (fmt_wristband(tier), crate::progression::tier_color(tier as u8)),
        };
        if text.0 != s {
            text.0 = s;
        }
        let want = c.with_alpha(st.alpha);
        if color.0 != want {
            color.0 = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_credits(123_456), "CR 123,456");
        assert_eq!(fmt_credits(0), "CR 0");
        assert_eq!(fmt_credits(-1_200), "CR -1,200");
        assert_eq!(fmt_pop(4_820), "POP 4,820");
        assert_eq!(fmt_pop(12_345_678), "POP 12,345,678");
        assert_eq!(fmt_wristband(2), "WB 3 BLUE");
        assert_eq!(fmt_wristband(99), "WB 7 GOLD");
    }

    #[test]
    fn counts_up() {
        assert_eq!(count_up(100, 1_100, 0.0), 100);
        assert_eq!(count_up(100, 1_100, COUNT_S), 1_100);
        assert_eq!(count_up(100, 1_100, 99.0), 1_100);
        let mid = count_up(0, 1_000, COUNT_S * 0.5);
        assert!(mid > 500 && mid < 1_000);
        assert_eq!(count_up(1_000, 0, COUNT_S), 0);
    }
}
