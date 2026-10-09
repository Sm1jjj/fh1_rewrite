//! Map icons for the world map (ui/worldmap.rs) and minimap (ui/minimap.rs) owners: one list rebuilt when the profile
//! or an activity changes (`generation` bumps). The map owners draw these with their own icon sheets; docs/MISSIONS.md
//! "Hooks" has the exact lines they need. Also `hidden_pois`: `map/pois.tsv` objects the maps should NOT draw (barn finds
//! not yet rumoured: pois.tsv lists every barn at its exact spot, which would give the hunt away).

use std::collections::HashSet;

use bevy::prelude::*;

use super::save::BarnState;
use super::Missions;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IconKind {
    /// Horizon Outpost (gas station); `done` = missions completed there.
    Outpost,
    SpeedCamera,
    AverageSpeed,
    /// A barn rumour: draw a circle of `radius` m around `pos` (the HintRegion), not the barn itself.
    BarnHint,
    /// Found barn (restoring / collected).
    BarnFound,
    /// The running activity's destination (speed trap, PR arena, photo location, encounter finish).
    #[default]
    Target,
    /// A rival waiting for a race encounter.
    Encounter,
}

/// Completion state of an icon (agreed with the map owner, site-73): Done = greyed + tick (or `medal`), Locked = dimmed +
/// padlock, Available = as is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IconState {
    #[default]
    Available,
    Done,
    Locked,
}

#[derive(Clone, Debug, Default)]
pub struct MapIcon {
    /// Stable key ("outpost:GasStation_01", "barnhint:barnfind_03").
    pub key: String,
    pub kind: IconKind,
    /// Engine (x, z).
    pub pos: Vec2,
    /// Hint circle radius (m), 0 = a plain icon.
    pub radius: f32,
    pub name: String,
    /// Card lines; `lines[0]` is the best score / progress text ("Best 152 mph", "Missions 2/3 · 6 stars", "Collected").
    pub lines: Vec<String>,
    pub state: IconState,
    /// Badge when Done: 1 gold, 2 silver, 3 bronze; None = a plain tick.
    pub medal: Option<u8>,
}

/// Medal from the stars of a set of missions (0..3 each): all 3 stars = gold, every mission >= 2 = silver, else bronze.
pub fn medal_of(stars: impl IntoIterator<Item = u8>) -> Option<u8> {
    let min = stars.into_iter().min()?;
    Some(match min {
        3.. => 1,
        2 => 2,
        _ => 3,
    })
}

#[derive(Resource, Default)]
pub struct MissionMapIcons {
    pub icons: Vec<MapIcon>,
    /// `map/pois.tsv` object names to skip (undiscovered barn finds).
    pub hidden_pois: HashSet<String>,
    /// The same barns' positions (engine x, z): ui/minimap / ui/worldmap's `load_pois` keep only (tag, position).
    pub hidden_barns: Vec<Vec2>,
    pub generation: u32,
    /// The running activity's target / encounter icons (set by the activity modules).
    pub dynamic: Vec<MapIcon>,
    built_for: Option<(u32, usize)>,
}

impl MissionMapIcons {
    /// A `map/pois.tsv` row (tag, engine position) the maps should not draw (an undiscovered barn find).
    pub fn hides(&self, tag: &str, pos: Vec3) -> bool {
        tag == "barnfind" && self.hidden_barns.iter().any(|b| b.distance(Vec2::new(pos.x, pos.z)) < 5.0)
    }

    /// Every icon to draw (the static list and the running activity's).
    pub fn all(&self) -> impl Iterator<Item = &MapIcon> {
        self.icons.iter().chain(self.dynamic.iter())
    }

    /// Replace the activity icons (bumps `generation` when they change).
    pub fn set_dynamic(&mut self, icons: Vec<MapIcon>) {
        let same = icons.len() == self.dynamic.len() && icons.iter().zip(&self.dynamic).all(|(a, b)| a.key == b.key && a.pos.distance(b.pos) < 5.0);
        if !same {
            self.dynamic = icons;
            self.generation = self.generation.wrapping_add(1);
        }
    }
}

pub fn update_icons(missions: Res<Missions>, profile: Res<crate::progression::Profile>, mut icons: ResMut<MissionMapIcons>) {
    let key = (profile.generation, missions.data.outposts.len());
    if icons.built_for == Some(key) {
        return;
    }
    icons.built_for = Some(key);
    let d = &missions.data;
    let s = &profile.data.missions;
    let mut out = Vec::new();
    for o in &d.outposts {
        let done = o.missions.iter().filter(|m| s.missions.get(*m).is_some_and(|r| r.completed)).count();
        let stars: u32 = o.missions.iter().map(|m| s.missions.get(m).map_or(0, |r| r.stars as u32)).sum();
        let found = s.outposts.contains(&o.name);
        let all_done = !o.missions.is_empty() && done == o.missions.len();
        out.push(MapIcon {
            key: format!("outpost:{}", o.name),
            kind: IconKind::Outpost,
            pos: Vec2::new(o.pos[0], o.pos[2]),
            radius: 0.0,
            name: if o.title.is_empty() { o.name.clone() } else { o.title.clone() },
            lines: vec![if found { format!("Missions {done}/{} · {stars} stars", o.missions.len()) } else { "Not discovered".into() }],
            state: if !found {
                IconState::Locked
            } else if all_done {
                IconState::Done
            } else {
                IconState::Available
            },
            medal: if all_done { medal_of(o.missions.iter().map(|m| s.missions.get(m).map_or(0, |r| r.stars))) } else { None },
        });
    }
    for c in &d.speed_cameras {
        let best = s.speed_cameras.get(&c.name);
        out.push(MapIcon {
            key: format!("camera:{}", c.name),
            kind: IconKind::SpeedCamera,
            pos: Vec2::new((c.left[0] + c.right[0]) * 0.5, (c.left[2] + c.right[2]) * 0.5),
            radius: 0.0,
            name: if c.label.is_empty() || c.label.starts_with("IDS_") { "Speed Camera".into() } else { c.label.clone() },
            lines: vec![best.map_or("No speed yet".into(), |b| format!("Best {b:.0} mph"))],
            state: if best.is_some() { IconState::Done } else { IconState::Available },
            medal: None,
        });
    }
    for z in &d.average_speed {
        let best = s.average_speed.get(&z.name);
        out.push(MapIcon {
            key: format!("average:{}", z.name),
            kind: IconKind::AverageSpeed,
            pos: Vec2::new((z.start[0][0] + z.start[1][0]) * 0.5, (z.start[0][2] + z.start[1][2]) * 0.5),
            radius: 0.0,
            name: if z.label.is_empty() || z.label.starts_with("IDS_") { "Average Speed Zone".into() } else { z.label.clone() },
            lines: vec![best.map_or("No average yet".into(), |b| format!("Best {b:.0} mph"))],
            state: if best.is_some() { IconState::Done } else { IconState::Available },
            medal: None,
        });
    }
    let mut hidden = HashSet::new();
    let mut hidden_pos = Vec::new();
    for (i, b) in d.barn_finds.iter().enumerate() {
        let st = s.barns.get(&b.name).map(|r| r.state).unwrap_or_default();
        match st {
            BarnState::Hidden => {
                hidden.insert(b.object.clone());
                hidden_pos.push(Vec2::new(b.pos[0], b.pos[2]));
            }
            BarnState::Rumoured => {
                hidden.insert(b.object.clone());
                hidden_pos.push(Vec2::new(b.pos[0], b.pos[2]));
                let (c, r) = super::barn::hint_circle(b);
                out.push(MapIcon { key: format!("barnhint:{}", b.name), kind: IconKind::BarnHint, pos: c, radius: r, name: "Barn Find Rumour".into(), lines: vec![format!("Rumour #{}", i + 1)], ..Default::default() });
            }
            BarnState::Restoring | BarnState::Collected => out.push(MapIcon {
                key: format!("barn:{}", b.name),
                kind: IconKind::BarnFound,
                pos: Vec2::new(b.pos[0], b.pos[2]),
                radius: 0.0,
                name: b.car.label(),
                lines: vec![if st == BarnState::Collected { "Collected".into() } else { "Being restored".into() }],
                state: IconState::Done,
                medal: None,
            }),
        }
    }
    icons.icons = out;
    icons.hidden_pois = hidden;
    icons.hidden_barns = hidden_pos;
    icons.generation = icons.generation.wrapping_add(1);
}
