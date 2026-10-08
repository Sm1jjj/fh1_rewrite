//! Career tables from the `events` group (events-2, fh1setup events.rs `progression`): wristbands, hubs, classes,
//! named drivers, car class / PI and the popularity ladder. Older installs (events-1) get the built-in copies below
//! (gamedb CareerWristbandLevels / WristbandScoring / CarClasses and Fame.xml, verified on the EU disc) and no
//! per-car class data.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Wristband {
    pub name: String,
    /// Cumulative XP to wear it.
    pub xp: u64,
    /// XP per finishing place (1st..8th), WristbandScoring.
    pub points: Vec<u32>,
    pub rival_mult: f32,
    /// Credits for beating this tier's nemesis.
    pub nemesis_bonus: u32,
}

#[derive(Clone, Debug)]
pub struct Hub {
    pub id: u32,
    pub name: String,
    pub unlock_xp: u64,
    /// Events.Id open as soon as the hub is.
    pub initial: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct CarClass {
    pub name: String,
    pub max_pi: u32,
}

#[derive(Clone, Debug)]
pub struct CarInfo {
    pub class: u32,
    pub pi: u32,
    pub drive: u32,
    pub year: u32,
    pub make: String,
    /// Data_Car.DisplayName ("Camaro SS Coupe"), events-4; empty before.
    pub name: String,
    pub selectable: bool,
    /// `cars/<media>` is installed (AI can drive it).
    pub installed: bool,
}

#[derive(Clone, Debug)]
pub struct Driver {
    pub id: u32,
    pub name: String,
    pub tier: u32,
    pub nemesis: bool,
}

#[derive(Clone, Debug)]
pub struct CareerData {
    /// events-2 data present (else built-ins, no car classes).
    pub installed: bool,
    pub wristbands: Vec<Wristband>,
    pub hubs: Vec<Hub>,
    pub classes: Vec<CarClass>,
    pub cars: HashMap<String, CarInfo>,
    pub drivers: HashMap<u32, Driver>,
    /// Popularity ladder (rank, cumulative fame), rank 249 -> 1 (Fame.xml). Above `fame[0]` = unranked (#250).
    pub fame: Vec<(u32, u64)>,
    /// Wristband reward cars (Rewards_Wristband: tier, car).
    pub wristband_cars: Vec<(u32, String)>,
}

/// gamedb CareerWristbandLevels (XP, name) x WristbandScoring (points per place).
const WRISTBANDS: [(&str, u64, [u32; 8], f32, u32); 7] = [
    ("Yellow", 0, [100, 90, 80, 70, 60, 50, 40, 30], 1.0, 1000),
    ("Green", 220, [110, 100, 90, 80, 70, 60, 50, 40], 1.5, 1500),
    ("Blue", 750, [120, 110, 100, 90, 80, 70, 60, 50], 2.0, 2000),
    ("Pink", 1450, [150, 120, 100, 90, 80, 70, 60, 50], 2.5, 2500),
    ("Orange", 2500, [180, 150, 120, 100, 90, 80, 70, 60], 3.0, 3000),
    ("Purple", 4100, [210, 160, 120, 100, 90, 80, 70, 60], 4.0, 4000),
    ("Gold", 6400, [250, 210, 160, 120, 100, 90, 80, 70], 5.0, 5000),
];

/// gamedb CarClasses (name, max display PI).
const CLASSES: [(&str, u32); 11] =
    [("F", 99), ("E", 200), ("D", 300), ("C", 400), ("B", 500), ("A", 600), ("S", 700), ("R3", 800), ("R2", 900), ("R1", 999), ("U", 999)];

/// Fame.xml samples (rank, cumulative fame); the built-in ladder interpolates between them.
const FAME: [(u32, u64); 15] = [
    (249, 15000),
    (248, 19500),
    (247, 24015),
    (240, 56040),
    (230, 103065),
    (220, 151590),
    (200, 254715),
    (150, 551902),
    (100, 934027),
    (50, 1428652),
    (10, 1913452),
    (5, 1984177),
    (3, 2013255),
    (2, 2027962),
    (1, 2042782),
];

impl Default for CareerData {
    fn default() -> Self {
        Self::builtin()
    }
}

impl CareerData {
    pub fn from_json(p: &Value, assets: &Path) -> Self {
        let u = |v: &Value| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0);
        let s = |v: &Value| v.as_str().unwrap_or("").to_owned();
        let mut d = Self::builtin();
        if !p.is_object() {
            return d;
        }
        d.installed = true;
        if let Some(w) = p["wristbands"].as_array().filter(|a| !a.is_empty()) {
            d.wristbands = w
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    let b = WRISTBANDS.get(i);
                    let points: Vec<u32> = w["points"].as_array().map(|a| a.iter().map(|x| u(x) as u32).collect()).unwrap_or_default();
                    Wristband {
                        name: w["name"].as_str().map(str::to_owned).or(b.map(|b| b.0.to_owned())).unwrap_or_else(|| format!("Tier {i}")),
                        xp: u(&w["xp"]),
                        points: if points.is_empty() { b.map(|b| b.2.to_vec()).unwrap_or_default() } else { points },
                        rival_mult: w["rival_mult"].as_f64().unwrap_or(1.0) as f32,
                        nemesis_bonus: u(&w["nemesis_bonus"]) as u32,
                    }
                })
                .collect();
        }
        d.hubs = p["hubs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|h| Hub {
                id: u(&h["id"]) as u32,
                name: s(&h["name"]),
                unlock_xp: u(&h["unlock_xp"]),
                initial: h["initial_events"].as_array().into_iter().flatten().map(|x| u(x) as u32).collect(),
            })
            .collect();
        if let Some(c) = p["classes"].as_array().filter(|a| !a.is_empty()) {
            d.classes = c.iter().map(|c| CarClass { name: s(&c["name"]), max_pi: u(&c["max_pi"]) as u32 }).collect();
        }
        let cars_dir = assets.join("cars");
        d.cars = p["cars"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, c)| {
                (
                    k.clone(),
                    CarInfo {
                        class: u(&c["class"]) as u32,
                        pi: u(&c["pi"]) as u32,
                        drive: u(&c["drive"]) as u32,
                        year: u(&c["year"]) as u32,
                        make: s(&c["make"]),
                        name: s(&c["name"]),
                        selectable: c["selectable"].as_bool().unwrap_or(false),
                        installed: cars_dir.join(k).is_dir(),
                    },
                )
            })
            .collect();
        d.drivers = p["drivers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                let id = u(&r["id"]) as u32;
                let name = r["full_name"].as_str().filter(|n| !n.is_empty()).or(r["name"].as_str()).unwrap_or("").to_owned();
                (id, Driver { id, name, tier: u(&r["tier"]) as u32, nemesis: r["nemesis"].as_bool().unwrap_or(false) })
            })
            .collect();
        let fame: Vec<(u32, u64)> = p["tunables"]["fame"].as_array().into_iter().flatten().map(|x| (u(&x[0]) as u32, u(&x[1]))).filter(|x| x.0 > 0).collect();
        if fame.len() > 100 {
            d.fame = fame;
        }
        d.wristband_cars = p["wristband_rewards"].as_array().into_iter().flatten().filter_map(|r| Some((u(&r["tier"]) as u32, r["car"].as_str()?.to_owned()))).collect();
        d
    }

    pub fn builtin() -> Self {
        let wristbands = WRISTBANDS
            .iter()
            .map(|w| Wristband { name: w.0.into(), xp: w.1, points: w.2.to_vec(), rival_mult: w.3, nemesis_bonus: w.4 })
            .collect();
        let classes = CLASSES.iter().map(|c| CarClass { name: c.0.into(), max_pi: c.1 }).collect();
        // Interpolated ladder, rank 249 -> 1.
        let mut fame = Vec::with_capacity(249);
        for rank in (1..=249u32).rev() {
            let k = FAME.iter().position(|f| f.0 <= rank).unwrap_or(FAME.len() - 1);
            let target = if FAME[k].0 == rank || k == 0 {
                FAME[k].1
            } else {
                let (hi, lo) = (FAME[k - 1], FAME[k]);
                let t = (hi.0 - rank) as f64 / (hi.0 - lo.0) as f64;
                (hi.1 as f64 + t * (lo.1 as f64 - hi.1 as f64)) as u64
            };
            fame.push((rank, target));
        }
        Self { installed: false, wristbands, hubs: Vec::new(), classes, cars: HashMap::new(), drivers: HashMap::new(), fame, wristband_cars: Vec::new() }
    }

    /// Wristband index worn at `xp`.
    pub fn tier(&self, xp: u64) -> usize {
        self.wristbands.iter().rposition(|w| xp >= w.xp).unwrap_or(0)
    }

    /// Popularity rank for `fame` (1 = top; 250 = not yet on the ladder).
    pub fn rank(&self, fame: u64) -> u32 {
        self.fame.iter().filter(|(_, t)| fame >= *t).map(|(r, _)| *r).min().unwrap_or(250)
    }

    /// Fame needed for `rank` (0 for 250).
    pub fn fame_for(&self, rank: u32) -> u64 {
        self.fame.iter().find(|(r, _)| *r == rank).map_or(0, |x| x.1)
    }

    /// "B 500" for a CarClasses id.
    pub fn class_label(&self, class: u32) -> Option<String> {
        self.classes.get(class as usize).map(|c| format!("{} {}", c.name, c.max_pi))
    }

    pub fn class_name(&self, class: u32) -> &str {
        self.classes.get(class as usize).map_or("?", |c| c.name.as_str())
    }

    pub fn driver_name(&self, id: u32) -> Option<&str> {
        self.drivers.get(&id).map(|d| d.name.as_str()).filter(|n| !n.is_empty())
    }
}
