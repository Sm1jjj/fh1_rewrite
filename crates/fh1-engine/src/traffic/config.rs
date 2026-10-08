//! `AIOpenWorld.xml` (media/gametunablesettings.zip, VERIFIED layout): per settings set (`freeroam`, `race`,
//! `firsttimecareer`) a car list, car groups and two density lists (`traffic`, `festival`) keyed by the nav ways'
//! `traffic_density` id. The default spawn tunables (AI/Spawning/*) come from default.xex (docs/TRAFFIC.md).

use std::collections::HashMap;

/// AI/Spawning/SpawnInDistance default (m, 0x831B2AE0, VERIFIED).
pub const SPAWN_IN: f32 = 380.0;
/// AI/Spawning/SpawnOutDistance default (m, 0x831B2B70, VERIFIED).
pub const SPAWN_OUT: f32 = 400.0;
/// AI/Spawning/SpawnAwayChanceMin / Max (0x831B2C90 / 0x831B2C00, VERIFIED); what lerps between them is not traced.
pub const AWAY_CHANCE_MIN: f32 = 0.3;
pub const AWAY_CHANCE_MAX: f32 = 0.7;

#[derive(Debug, Clone, Default)]
pub struct CarEntry {
    /// Data_Car Id.
    pub model: u32,
    /// At most this many on the road at once (bus, truck: 1).
    pub max_active: Option<u32>,
    pub stream_only: bool,
    pub large: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Density {
    pub id: u32,
    pub min: f32,
    pub max: f32,
    /// Cruise speed (mph, INFERRED unit: 30 on B-roads / towns, 60 on freeways).
    pub speed_mph: Option<f32>,
    pub spawn_in: Option<f32>,
    pub spawn_out: Option<f32>,
    pub away_chance: Option<f32>,
    /// Car groups banned on these roads / the only groups allowed (dirt roads: 4x4).
    pub banned: Vec<String>,
    pub allowed: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SettingsSet {
    pub name: String,
    pub cars: Vec<CarEntry>,
    pub initial_traffic: u32,
    pub initial_festival: u32,
    pub max_loaded_models: u32,
    pub groups: HashMap<String, Vec<u32>>,
    pub traffic: HashMap<u32, Density>,
    pub festival: HashMap<u32, Density>,
}

impl SettingsSet {
    /// May `model` drive on roads of density `d`?
    pub fn allowed(&self, d: &Density, model: u32) -> bool {
        let in_group = |g: &String| self.groups.get(g).is_some_and(|m| m.contains(&model));
        if !d.allowed.is_empty() && !d.allowed.iter().any(in_group) {
            return false;
        }
        !d.banned.iter().any(in_group)
    }
}

#[derive(Debug, Clone, Default)]
pub struct OpenWorldConfig {
    pub sets: Vec<SettingsSet>,
}

impl OpenWorldConfig {
    pub fn set(&self, name: &str) -> Option<&SettingsSet> {
        self.sets.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn parse(xml: &str) -> Self {
        let mut sets: Vec<SettingsSet> = Vec::new();
        // Context while walking: current density list ("traffic"/"festival"), density, group, and whether we're in
        // a BannedVehicles / AllowedVehicles block.
        let mut list = String::new();
        let mut density: Option<Density> = None;
        let mut group: Option<String> = None;
        let mut rule = 0u8; // 1 banned, 2 allowed
        for tag in tags(xml) {
            let set = sets.last_mut();
            match (tag.name, tag.close) {
                ("Settings", false) => sets.push(SettingsSet { name: tag.attr("name").unwrap_or_default().to_owned(), ..Default::default() }),
                ("CarList", false) => {
                    if let Some(s) = set {
                        s.initial_traffic = tag.num("numInitialTrafficCars").unwrap_or(0.0) as u32;
                        s.initial_festival = tag.num("numInitialFestivalCars").unwrap_or(0.0) as u32;
                        s.max_loaded_models = tag.num("maxLoadedTrafficModels").unwrap_or(4.0) as u32;
                    }
                }
                ("Group", false) => {
                    let id = tag.attr("id").unwrap_or_default().to_owned();
                    if rule == 0 {
                        group = (!tag.selfclose).then_some(id);
                    } else if let Some(d) = density.as_mut() {
                        if rule == 1 { d.banned.push(id) } else { d.allowed.push(id) }
                    }
                }
                ("Group", true) => group = None,
                ("Car", false) => {
                    let Some(s) = set else { continue };
                    let Some(model) = tag.num("model").map(|m| m as u32) else { continue };
                    if let Some(g) = &group {
                        s.groups.entry(g.clone()).or_default().push(model);
                    } else if density.is_none() {
                        s.cars.push(CarEntry {
                            model,
                            max_active: tag.num("maxactive").map(|v| v as u32),
                            stream_only: tag.attr("streamonly") == Some("true"),
                            large: tag.attr("largevehicle") == Some("true"),
                        });
                    }
                }
                ("DensityList", false) => list = tag.attr("cartype").unwrap_or_default().to_owned(),
                ("Density", false) => {
                    let d = Density {
                        id: tag.num("id").unwrap_or(0.0) as u32,
                        min: tag.num("min").unwrap_or(0.0),
                        max: tag.num("max").unwrap_or(0.0),
                        speed_mph: tag.num("speed"),
                        spawn_in: tag.num("spawnInDistance"),
                        spawn_out: tag.num("spawnOutDistance"),
                        away_chance: tag.num("spawnAwayChance"),
                        ..Default::default()
                    };
                    if tag.selfclose {
                        store(set, &list, d);
                    } else {
                        density = Some(d);
                    }
                }
                ("Density", true) => {
                    if let Some(d) = density.take() {
                        store(set, &list, d);
                    }
                }
                ("BannedVehicles", false) => rule = 1,
                ("AllowedVehicles", false) => rule = 2,
                ("BannedVehicles" | "AllowedVehicles", true) => rule = 0,
                _ => {}
            }
        }
        Self { sets }
    }
}

fn store(set: Option<&mut SettingsSet>, list: &str, d: Density) {
    let Some(s) = set else { return };
    let map = if list.eq_ignore_ascii_case("festival") { &mut s.festival } else { &mut s.traffic };
    map.insert(d.id, d);
}

/// One XML tag: name, attributes, `</..>` (close) or `<../>` (selfclose).
struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
    close: bool,
    selfclose: bool,
}

impl<'a> Tag<'a> {
    fn attr(&self, k: &str) -> Option<&'a str> {
        self.attrs.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| *v)
    }

    fn num(&self, k: &str) -> Option<f32> {
        self.attr(k)?.trim().parse().ok()
    }
}

/// The tags of a plain XML document (no CDATA / entities in this file); comments and `<?..?>` skipped.
fn tags(xml: &str) -> impl Iterator<Item = Tag<'_>> {
    let mut rest = xml;
    std::iter::from_fn(move || loop {
        let start = rest.find('<')?;
        rest = &rest[start..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.find("-->").map_or("", |e| &r[e + 3..]);
            continue;
        }
        let end = rest.find('>')?;
        let body = &rest[1..end];
        rest = &rest[end + 1..];
        if body.starts_with('?') || body.starts_with('!') {
            continue;
        }
        let close = body.starts_with('/');
        let selfclose = body.ends_with('/');
        let body = body.trim_start_matches('/').trim_end_matches('/');
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let name = &body[..name_end];
        let mut attrs = Vec::new();
        let mut a = &body[name_end..];
        while let Some(eq) = a.find('=') {
            let key = a[..eq].trim();
            let after = a[eq + 1..].trim_start();
            let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
            let Some(close_q) = after[1..].find(q) else { break };
            attrs.push((key, &after[1..1 + close_q]));
            a = &after[close_q + 2..];
        }
        return Some(Tag { name, attrs, close, selfclose });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample() {
        let xml = r#"<?xml version="1.0"?><AIOpenWorld><Settings name="freeroam">
            <CarList numInitialTrafficCars="4" maxLoadedTrafficModels="4"><Car model="282"/> <!-- x -->
            <Car model="1529" maxactive="1" streamonly="true" largevehicle="true"/></CarList>
            <Densities><CarGroups><Group id="bus"><Car model="1529"/></Group></CarGroups>
            <DensityList cartype="traffic"><Density id="2" min="0.04" max="0.4" speed="30" spawnAwayChance="0.6">
            <BannedVehicles><Group id="bus"/></BannedVehicles></Density>
            <Density id="4" min="0.15" max="1.5" speed="60"/></DensityList>
            <DensityList cartype="festival"><Density id="8" min="1" max="1"/></DensityList></Densities></Settings></AIOpenWorld>"#;
        let c = OpenWorldConfig::parse(xml);
        let s = c.set("freeroam").unwrap();
        assert_eq!(s.cars.len(), 2);
        assert_eq!(s.cars[1].max_active, Some(1));
        assert!(s.cars[1].large);
        assert_eq!(s.initial_traffic, 4);
        assert_eq!(s.groups["bus"], vec![1529]);
        let d2 = &s.traffic[&2];
        assert_eq!(d2.banned, vec!["bus".to_string()]);
        assert_eq!(d2.speed_mph, Some(30.0));
        assert!(!s.allowed(d2, 1529) && s.allowed(d2, 282));
        assert!(s.allowed(&s.traffic[&4], 1529));
        assert_eq!(s.festival[&8].max, 1.0);
    }
}
