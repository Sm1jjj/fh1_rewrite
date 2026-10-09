//! Done / locked state of map icons (world map + minimap + the in-world markers), `FH1_MAP_STATES=0` = old look.
//!
//! Rules reused, not invented: Done = a recorded finish (`EventRecord::best_place > 0`, the same notion as
//! `progression::event_state`'s Completed); Locked = `progression::lock_reason` is Some, which needs the `RaceDef` and
//! `CareerData` and is therefore already evaluated in `progression::EventCatalog` (`EventInfo::state`). A bare
//! `Profile` cannot know the lock, so [`event_state`] (the contract's signature) answers Done / Available only, and
//! [`catalog_state`] / [`info_state`] add Locked from the catalog.

pub use crate::missions::map::IconState;

use crate::progression::{EventCatalog, EventInfo, EventState, Profile};

/// `FH1_MAP_STATES=0` = the old look (no done / locked styling).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_MAP_STATES").map_or(true, |v| v != "0"))
}

/// Medal from a best place: 1 gold, 2 silver, 3 bronze.
pub fn medal_of_place(place: u8) -> Option<u8> {
    (1..=3).contains(&place).then_some(place)
}

/// Done (finished at least once) or Available for `event_id` (HorizonEventID) + the best place medal. Never Locked: see
/// the module doc; use [`catalog_state`] when the lock matters.
pub fn event_state(profile: &Profile, event_id: &str) -> (IconState, Option<u8>) {
    match profile.data.events.get(event_id).map(|r| r.best_place).filter(|&p| p > 0) {
        Some(p) => (IconState::Done, medal_of_place(p)),
        None => (IconState::Available, None),
    }
}

/// State from a catalog entry (Locked included).
pub fn info_state(e: &EventInfo) -> (IconState, Option<u8>) {
    match e.state {
        EventState::Locked => (IconState::Locked, None),
        EventState::Unlocked => (IconState::Available, None),
        EventState::Completed { place } => (IconState::Done, medal_of_place(place)),
    }
}

/// Full state of `event_id`: the catalog's (lock rules), else the profile's. Unknown id with no record = Available.
pub fn catalog_state(catalog: &EventCatalog, profile: &Profile, event_id: &str) -> (IconState, Option<u8>) {
    match catalog.events.iter().find(|e| e.id == event_id) {
        Some(e) => info_state(e),
        None => event_state(profile, event_id),
    }
}

/// Card line for a state ("Completed - best: 1st", "Locked: Blue wristband").
pub fn card_line(state: IconState, best: Option<u8>, reason: Option<&str>) -> Option<String> {
    match state {
        IconState::Available => None,
        IconState::Done => Some(match best {
            Some(p) => format!("Completed \u{2014} best: {}", ordinal(p)),
            None => "Completed".into(),
        }),
        IconState::Locked => Some(match reason {
            Some(r) => format!("Locked: {r}"),
            None => "Locked".into(),
        }),
    }
}

pub fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Badge on an icon.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Badge {
    Tick,
    /// 1 gold, 2 silver, 3 bronze.
    Medal(u8),
    Lock,
}

pub fn badge_of(state: IconState, medal: Option<u8>) -> Option<Badge> {
    match state {
        IconState::Available => None,
        IconState::Done => Some(medal.filter(|m| (1..=3).contains(m)).map_or(Badge::Tick, Badge::Medal)),
        IconState::Locked => Some(Badge::Lock),
    }
}

/// Badge disc colour (sRGB 0..1). The glyph (tick / padlock) is drawn black in `circle::badge_image`.
pub fn badge_rgb(b: Badge) -> [f32; 3] {
    match b {
        Badge::Tick => [0.45, 0.85, 0.5],
        Badge::Medal(1) => [1.0, 0.8, 0.2],
        Badge::Medal(2) => [0.82, 0.85, 0.9],
        Badge::Medal(_) => [0.82, 0.52, 0.28],
        Badge::Lock => [0.7, 0.7, 0.72],
    }
}

/// Styled RGBA of an icon layer: Done = grey mix at alpha 0.6, Locked = darker grey at alpha 0.45 (the profile's
/// "ghost"). Available = unchanged.
pub fn style_rgba(c: [f32; 4], state: IconState) -> [f32; 4] {
    let (mix, dim, alpha) = match state {
        IconState::Available => return c,
        IconState::Done => (0.7, 0.85, 0.6),
        IconState::Locked => (1.0, 0.6, 0.45),
    };
    let g = (c[0] + c[1] + c[2]) / 3.0;
    let f = |v: f32| (v + (g - v) * mix) * dim;
    [f(c[0]), f(c[1]), f(c[2]), c[3] * alpha]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progression::profile::EventRecord;

    fn profile(events: &[(&str, u8)]) -> Profile {
        let mut p = Profile::load(std::path::PathBuf::new(), false);
        for (id, place) in events {
            p.data.events.insert(id.to_string(), EventRecord { best_place: *place, ..Default::default() });
        }
        p
    }

    #[test]
    fn done_with_medal() {
        let p = profile(&[("FR02", 1), ("FR03", 3), ("FR04", 5), ("FR05", 0)]);
        assert_eq!(event_state(&p, "FR02"), (IconState::Done, Some(1)));
        assert_eq!(event_state(&p, "FR03"), (IconState::Done, Some(3)));
        assert_eq!(event_state(&p, "FR04"), (IconState::Done, None));
        assert_eq!(event_state(&p, "FR05"), (IconState::Available, None));
        assert_eq!(event_state(&p, "nope"), (IconState::Available, None));
    }

    #[test]
    fn badges_and_lines() {
        assert_eq!(badge_of(IconState::Available, None), None);
        assert_eq!(badge_of(IconState::Done, Some(2)), Some(Badge::Medal(2)));
        assert_eq!(badge_of(IconState::Done, None), Some(Badge::Tick));
        assert_eq!(badge_of(IconState::Locked, Some(1)), Some(Badge::Lock));
        assert_eq!(card_line(IconState::Done, Some(1), None).unwrap(), "Completed \u{2014} best: 1st");
        assert_eq!(card_line(IconState::Locked, None, Some("Blue wristband")).unwrap(), "Locked: Blue wristband");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(22), "22nd");
    }

    #[test]
    fn style() {
        let c = [1.0, 0.0, 0.0, 1.0];
        assert_eq!(style_rgba(c, IconState::Available), c);
        let d = style_rgba(c, IconState::Done);
        assert!((d[3] - 0.6).abs() < 1e-6 && d[0] < 1.0 && d[1] > 0.0);
        assert!(style_rgba(c, IconState::Locked)[3] < d[3]);
    }
}
