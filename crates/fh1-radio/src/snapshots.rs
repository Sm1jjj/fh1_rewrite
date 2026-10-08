//! The radio's two mixer channels from `media/audio/AudioMixerSnapshots.xml`.
//!
//! Verified against the EU disc: each snapshot belongs to a group (`Gameplay`, `Speed`, `Location`,
//! `VO`, `Radio`, `SatNav`, `Reverb`, ...) and gives every mixer channel a `destvolume` and
//! `fadetime`. One snapshot is active per group. The radio plays in channel `GameMusic` (music,
//! idents) and its DJ also in `DJModifier` (INFERRED from the radio code reading mixer fields
//! 0x5E10 / 0x5E44, which line up with these channels). The radio itself switches the `Radio`
//! group: `RadioDJSpeaking` (snapshot 34) for DJ / special / immediate lines,
//! `RadioFestivalUpdate` (35) for festival blocks, `RadioNormal` (36) after them.
//!
//! How groups combine (product of the groups' current values) and the fade shape (linear over
//! `fadetime`) are INFERRED: the snapshot manager itself was not traced.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One channel's setting in a snapshot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Level {
    pub volume: f32,
    /// Seconds to reach `volume` when the snapshot becomes active.
    pub fade: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub group: String,
    pub music: Level,
    pub dj: Level,
}

/// Snapshot name -> settings, in file order (the radio refers to snapshots 34..36 by index).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshots {
    pub list: Vec<Snapshot>,
}

pub const RADIO_DJ_SPEAKING: &str = "RadioDJSpeaking";
pub const RADIO_FESTIVAL_UPDATE: &str = "RadioFestivalUpdate";
pub const RADIO_NORMAL: &str = "RadioNormal";

impl Snapshots {
    pub fn parse(xml: &str) -> Result<Snapshots> {
        let root = crate::config::parse_tree(xml)?;
        let top = root.child("MixSnapshots").context("no <MixSnapshots>")?;
        let level = |el: &crate::config::El, ch: &str| -> Result<Level> {
            Ok(match el.child(ch) {
                Some(c) if c.attr("active") != Some("0") => Level { volume: c.f32("destvolume", 1.0)?, fade: c.f32("fadetime", 0.0)? },
                // Inactive or missing: the channel is left alone by this snapshot.
                _ => Level { volume: 1.0, fade: 0.0 },
            })
        };
        let mut list = Vec::new();
        for el in top.children.iter().filter(|c| c.attr("group").is_some()) {
            list.push(Snapshot {
                name: el.name.clone(),
                group: el.attr("group").unwrap().to_owned(),
                music: level(el, "GameMusic")?,
                dj: level(el, "DJModifier")?,
            });
        }
        Ok(Snapshots { list })
    }

    pub fn get(&self, name: &str) -> Option<&Snapshot> {
        self.list.iter().find(|s| s.name == name)
    }

    /// The snapshots active when nothing has been set (INFERRED defaults for free roam).
    pub fn defaults() -> &'static [&'static str] {
        &["FreeRoam", "DefaultLocation", "VONotPlaying", RADIO_NORMAL, "SatNavNotPlaying", "NotInTunnel"]
    }
}

/// Live state: one active snapshot per group, each group's channel values fading linearly.
#[derive(Debug, Clone)]
pub struct Mix {
    /// group -> (current music, current dj, target, rates per second)
    groups: BTreeMap<String, GroupState>,
}

#[derive(Debug, Clone)]
struct GroupState {
    active: String,
    music: f32,
    dj: f32,
    target: (f32, f32),
    rate: (f32, f32),
}

impl Mix {
    pub fn new(snaps: &Snapshots) -> Mix {
        let mut m = Mix { groups: BTreeMap::new() };
        for name in Snapshots::defaults() {
            if let Some(s) = snaps.get(name) {
                m.groups.insert(
                    s.group.clone(),
                    GroupState { active: s.name.clone(), music: s.music.volume, dj: s.dj.volume, target: (s.music.volume, s.dj.volume), rate: (0.0, 0.0) },
                );
            }
        }
        m
    }

    /// Makes `name` the active snapshot of its group. Unknown names are ignored.
    pub fn set(&mut self, snaps: &Snapshots, name: &str) {
        let Some(s) = snaps.get(name) else { return };
        let g = self.groups.entry(s.group.clone()).or_insert(GroupState {
            active: String::new(),
            music: 1.0,
            dj: 1.0,
            target: (1.0, 1.0),
            rate: (0.0, 0.0),
        });
        if g.active == s.name {
            return;
        }
        g.active = s.name.clone();
        g.target = (s.music.volume, s.dj.volume);
        let rate = |from: f32, to: f32, fade: f32| if fade <= 0.0 { f32::MAX } else { (to - from).abs() / fade };
        g.rate = (rate(g.music, s.music.volume, s.music.fade), rate(g.dj, s.dj.volume, s.dj.fade));
    }

    pub fn active(&self, group: &str) -> Option<&str> {
        self.groups.get(group).map(|g| g.active.as_str())
    }

    pub fn step(&mut self, dt: f32) {
        for g in self.groups.values_mut() {
            g.music += (g.target.0 - g.music).clamp(-g.rate.0 * dt, g.rate.0 * dt);
            g.dj += (g.target.1 - g.dj).clamp(-g.rate.1 * dt, g.rate.1 * dt);
        }
    }

    /// (GameMusic, DJModifier) levels: the product over groups.
    pub fn levels(&self) -> (f32, f32) {
        self.groups.values().fold((1.0, 1.0), |(m, d), g| (m * g.music, d * g.dj))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<MixSnapshots><DynamicsData><CrashData threshold="50"/></DynamicsData>
      <FreeRoam group="Gameplay"><GameMusic active="1" destvolume="1.0" fadetime="0.5"/><DJModifier active="1" destvolume="1.0" fadetime="0.5"/></FreeRoam>
      <Paused group="Gameplay"><GameMusic active="1" destvolume="0.25" fadetime="0.2"/><DJModifier active="1" destvolume="1.0" fadetime="0.2"/></Paused>
      <VOPlaying group="VO"><GameMusic active="1" destvolume="0.25" fadetime="0.3"/><DJModifier active="1" destvolume="0.0" fadetime="0.2"/></VOPlaying>
      <VONotPlaying group="VO"><GameMusic active="1" destvolume="1.0" fadetime="0.3"/><DJModifier active="1" destvolume="1.0" fadetime="0.2"/></VONotPlaying>
    </MixSnapshots>"#;

    #[test]
    fn groups_multiply_and_fade() {
        let s = Snapshots::parse(XML).unwrap();
        assert_eq!(s.list.len(), 4);
        let mut m = Mix::new(&s);
        assert_eq!(m.levels(), (1.0, 1.0));
        m.set(&s, "Paused");
        m.step(0.1);
        assert!((m.levels().0 - 0.625).abs() < 1e-4);
        m.step(0.2);
        assert!((m.levels().0 - 0.25).abs() < 1e-4);
        m.set(&s, "VOPlaying");
        m.step(1.0);
        let (mu, dj) = m.levels();
        assert!((mu - 0.0625).abs() < 1e-4 && dj == 0.0);
    }
}
