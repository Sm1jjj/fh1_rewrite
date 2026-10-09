//! In-world event marker states (site-73 round 2; `FH1_MARKER_STATES=0` = the markers as before).
//!
//! The columns of light over each event start already dimmed for Locked / Completed (visuals.rs `marker_look`, from the
//! [`crate::progression::EventCatalog`]). This module makes the three states easy to tell apart at a glance, from the same
//! per-profile state the world maps use ([`crate::ui::worldmap::state::event_state`]):
//! - Available: the normal wristband / street column (unchanged).
//! - Done (finished at least once): the same column desaturated and low (a "spent" marker), no ripples.
//! - Locked: a cold dark grey-blue stub; with the player standing in it, a dim red column (the HUD prompt then says
//!   "LOCKED: <reason>" and A flashes "Locked: <reason>", race.rs).
//!
//! [`MarkerStates`] is refreshed only when the profile (its `generation`) or the event list changes, never per frame.
//! The start rule itself is not here: race.rs refuses locked events with `progression::lock_reason`, which follows FH1's own
//! unlock data (wristband / XP / hub / popularity rank / Rewards_EventUnlock).
//!
//! Also: a one-line startability audit of the installed events at load (see [`audit`]).

use bevy::color::LinearRgba;
use bevy::prelude::*;

use super::Events;
use crate::missions::map::IconState;
use crate::progression::{profile::ProfileData, EventKind, Profile};

/// `FH1_MARKER_STATES=0`: the old marker looks (visuals.rs `marker_look` only).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_MARKER_STATES").map_or(true, |v| v != "0"))
}

/// `FH1_MARKER_STATE_FILTER=0`: locked and completed events / missions keep emitting their long-range marker (old
/// behaviour). Default: only unlocked, not-yet-completed ones emit (beam / ring / lights) and open the start prompt.
pub fn filter_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_MARKER_STATE_FILTER").map_or(true, |v| v != "0"))
}

/// Pure rule: a marker emits only while unlocked and not completed.
pub fn marker_visible(unlocked: bool, completed: bool) -> bool {
    unlocked && !completed
}

/// [`marker_visible`] for a catalog state (None = unknown, e.g. progression off: stays visible), honouring the flag.
pub fn state_visible(state: Option<crate::progression::EventState>) -> bool {
    use crate::progression::EventState;
    !filter_on() || state.is_none_or(|s| marker_visible(s != EventState::Locked, matches!(s, EventState::Completed { .. })))
}

/// [`marker_visible`] for a map-icon state, honouring the flag.
pub fn icon_visible(state: Option<IconState>) -> bool {
    !filter_on() || state.is_none_or(|s| marker_visible(s != IconState::Locked, s == IconState::Done))
}

/// A marker look: (key, colour, intensity, emphasis, ripple, beam height) — the tuple visuals.rs `marker_look` returns.
pub type Look = (String, LinearRgba, f32, f32, bool, f32);

/// State of every installed event for the current profile, by race index (`Events::races`).
#[derive(Resource, Default)]
pub struct MarkerStates {
    pub by_race: Vec<(IconState, Option<u8>)>,
    /// (profile generation, event count) the list was built for.
    seen: Option<(u32, usize)>,
    /// Bumps on every rebuild.
    pub generation: u32,
}

impl MarkerStates {
    pub fn get(&self, race: usize) -> Option<IconState> {
        self.by_race.get(race).map(|s| s.0)
    }
}

pub struct MarkerStatesPlugin;

impl Plugin for MarkerStatesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MarkerStates>().add_systems(Update, (refresh, audit));
    }
}

/// Rebuilds [`MarkerStates`] when the profile or the event list changed.
fn refresh(profile: Option<Res<Profile>>, catalog: Option<Res<crate::progression::EventCatalog>>, events: Res<Events>, mut st: ResMut<MarkerStates>) {
    if !enabled() {
        return;
    }
    let Some(profile) = profile else { return };
    // The catalog carries the lock rule (progression::lock_reason); without it only Done / Available are known.
    let key = (profile.generation.wrapping_add(catalog.as_ref().map_or(0, |c| c.generation.wrapping_mul(0x9E37))), events.races.len());
    if st.seen == Some(key) {
        return;
    }
    st.by_race = events
        .races
        .iter()
        .map(|r| match catalog.as_deref() {
            Some(c) => crate::ui::worldmap::state::catalog_state(c, &profile, &r.horizon_id),
            None => crate::ui::worldmap::state::event_state(&profile, &r.horizon_id),
        })
        .collect();
    st.seen = Some(key);
    st.generation = st.generation.wrapping_add(1);
}

fn luminance(c: LinearRgba) -> f32 {
    0.2126 * c.red + 0.7152 * c.green + 0.0722 * c.blue
}

/// `look` (the normal look of the event) restyled for its state. `prompt`: the player stands in the marker.
pub fn restyle(look: Look, state: Option<IconState>, prompt: bool) -> Look {
    if !enabled() {
        return look;
    }
    let (key, colour, intensity, emph, ripple, height) = look;
    match state {
        Some(IconState::Locked) if prompt => ("S-lockprompt".into(), LinearRgba::rgb(1.6, 0.35, 0.3), 0.7, 0.0, true, height),
        // Gold "you can start" prompt look stays (the state is Available / Done under the player).
        _ if prompt => (key, colour, intensity, emph, ripple, height),
        Some(IconState::Locked) => ("S-locked".into(), LinearRgba::rgb(0.3, 0.33, 0.46), 0.22, 0.0, false, height * 0.4),
        Some(IconState::Done) => {
            let l = luminance(colour);
            let mix = |c: f32| l + (c - l) * 0.3;
            (format!("S-done-{key}"), LinearRgba::rgb(mix(colour.red), mix(colour.green), mix(colour.blue)), intensity.min(0.9) * 0.45, 0.0, false, height * 0.55)
        }
        _ => (key, colour, intensity, emph, ripple, height),
    }
}

/// The HUD activation prompt's title: the event name, with the lock reason when locked.
pub fn prompt_title(name: &str, locked: Option<&str>) -> String {
    match locked.filter(|_| enabled()) {
        Some(why) => format!("{}  -  LOCKED: {}", name.to_uppercase(), why.to_uppercase()),
        None => name.to_uppercase(),
    }
}

/// Per-kind count of (installed, startable on a fresh profile) for the load-time log.
pub fn audit_counts(events: &Events, fresh: &ProfileData) -> Vec<(EventKind, usize, usize)> {
    let mut out: Vec<(EventKind, usize, usize)> = Vec::new();
    for r in &events.races {
        let k = EventKind::of(r);
        let open = crate::progression::lock_reason(r, fresh, &events.career).is_none();
        match out.iter_mut().find(|e| e.0 == k) {
            Some(e) => {
                e.1 += 1;
                e.2 += open as usize;
            }
            None => out.push((k, 1, open as usize)),
        }
    }
    out
}

/// Logs once, when the events are loaded: installed events per kind and how many are open on a fresh profile.
fn audit(events: Res<Events>, mut done: Local<bool>) {
    if *done || events.races.is_empty() {
        return;
    }
    *done = true;
    let line: Vec<String> =
        audit_counts(&events, &ProfileData::default()).into_iter().map(|(k, n, open)| format!("{} {n} ({open} open at start)", k.label())).collect();
    info!("race: {} events installed - {} (gamedb has 119; FREE_ROAM has no route, see docs/RACES.md)", events.races.len(), line.join(", "));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Look {
        ("open2false".into(), LinearRgba::rgb(0.2, 1.0, 0.3), 0.9, 0.0, true, 90.0)
    }

    #[test]
    fn available_is_unchanged() {
        let (k, c, i, _, r, h) = restyle(base(), Some(IconState::Available), false);
        assert_eq!((k.as_str(), i, r, h), ("open2false", 0.9, true, 90.0));
        assert_eq!(c.green, 1.0);
        let (k, ..) = restyle(base(), None, false);
        assert_eq!(k, "open2false");
    }

    #[test]
    fn done_is_dimmer_and_greyer() {
        let (k, c, i, _, ripple, h) = restyle(base(), Some(IconState::Done), false);
        assert!(k.starts_with("S-done-"));
        assert!(i < 0.5 && h < 90.0 * 0.6 && !ripple);
        // Saturation (max - min) shrinks.
        assert!(c.green - c.red < 1.0 - 0.2);
        // Luminance is kept.
        assert!((luminance(c) - luminance(LinearRgba::rgb(0.2, 1.0, 0.3))).abs() < 1e-4);
    }

    #[test]
    fn locked_looks_differ_by_prompt() {
        let (k1, c1, i1, ..) = restyle(base(), Some(IconState::Locked), false);
        let (k2, c2, ..) = restyle(base(), Some(IconState::Locked), true);
        assert_eq!(k1, "S-locked");
        assert_eq!(k2, "S-lockprompt");
        assert!(i1 < 0.3);
        assert!(c2.red > c2.blue && c1.blue >= c1.red);
        // The start prompt of an open or done event keeps the gold look it came with.
        let gold = ("prompt".to_owned(), LinearRgba::rgb(2.0, 1.7, 0.4), 1.2, 0.6, true, 90.0);
        assert_eq!(restyle(gold.clone(), Some(IconState::Done), true).0, "prompt");
    }

    #[test]
    fn marker_visible_rule() {
        assert!(marker_visible(true, false));
        assert!(!marker_visible(false, false));
        assert!(!marker_visible(true, true));
        assert!(!marker_visible(false, true));
        use crate::progression::EventState;
        assert!(state_visible(Some(EventState::Unlocked)) && state_visible(None));
        assert!(!state_visible(Some(EventState::Locked)) && !state_visible(Some(EventState::Completed { place: 1 })));
        assert!(icon_visible(Some(IconState::Available)) && !icon_visible(Some(IconState::Locked)) && !icon_visible(Some(IconState::Done)));
    }

    #[test]
    fn prompt_title_has_reason() {
        assert_eq!(prompt_title("Rally", None), "RALLY");
        assert!(prompt_title("Rally", Some("Blue wristband")).ends_with("LOCKED: BLUE WRISTBAND"));
    }
}
