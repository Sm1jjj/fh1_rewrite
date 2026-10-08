//! Free-roam traffic (docs/TRAFFIC.md): FH1's road traffic and festival drivers on Colorado's road network.
//!
//! Headless parts live here (tests drive them without Bevy): `config` (AIOpenWorld.xml: car lists, car groups,
//! densities), `network` (lanes from colorado.nav), `driver` (lane following, car following, far-mode kinematics).
//! The engine plugin that spawns / steps / draws traffic is in the binary (src/traffic/plugin.rs).
//!
//! Traffic cars carry `ai::AiCar(Vehicle)` (like race AI, so car-vs-car contact, AI audio and wheel tagging see them)
//! plus [`TrafficCar`]; they never carry `AiBrain` / `AiRacer`, so the race AI systems skip them. Agreed with R3 (AI
//! audio): audio queries `(Entity, &AiCar, &GlobalTransform, Has<TrafficCar>)`; `Vehicle` rpm / gear / throttle /
//! velocity stay plausible in the far (kinematic) mode, and `Vehicle::data.media_name` names the car.

pub mod config;
pub mod driver;
pub mod network;

use std::collections::HashMap;
use std::path::Path;

use bevy::prelude::*;

/// Which density list spawned the car.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficKind {
    /// Everyday traffic (AIOpenWorld CarList: the IsSelectable=0 road cars, bus, truck).
    Road,
    /// Festival drivers (gamedb FreeRoamDrivers' cars) cruising between events.
    Festival,
}

/// A free-roam traffic car (with `ai::AiCar`).
#[derive(Component, Debug, Clone, Copy)]
pub struct TrafficCar {
    pub kind: TrafficKind,
    /// Running the full vehicle sim (near the player, or knocked about) rather than the far kinematic mode.
    pub simulated: bool,
    /// Sounding the horn (blocked by the player; OUR rule) for whoever plays car sounds.
    pub horn: bool,
}

/// A pooled traffic car waiting for reuse (hidden, parked at y = -10000, not simulated). Traffic cars are parked rather
/// than despawned: building a game-shaded body costs a frame hitch, reusing one costs nothing.
#[derive(Component, Debug, Clone, Copy)]
pub struct TrafficParked;

/// Pre-building the traffic pool behind the startup loading card (ui/loading.rs waits on it): `done` of `total` bodies
/// built. `total` = 0 = nothing to wait for (traffic off, not Colorado, no install).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct TrafficWarm {
    pub total: u32,
    pub done: u32,
    /// Real time (s) the pre-build started.
    pub started_at: Option<f32>,
}

impl TrafficWarm {
    /// Still building (the loading card should wait).
    pub fn pending(&self) -> bool {
        self.total > 0 && self.done < self.total
    }
}

/// FH1_TRAFFIC=0 turns traffic off (default on: the body pool is pre-built behind the loading card, see TrafficWarm).
pub fn enabled() -> bool {
    std::env::var("FH1_TRAFFIC").map_or(true, |v| v != "0")
}

/// FH1_TRAFFIC_DENSITY=<x>: scale on the game's densities (default 1).
pub fn density_scale() -> f32 {
    std::env::var("FH1_TRAFFIC_DENSITY").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0f32).max(0.0)
}

/// Everything the spawner needs from the install (`<assets>/traffic/`, fh1setup group `traffic`).
pub struct TrafficData {
    pub config: config::OpenWorldConfig,
    pub network: network::Network,
    /// Data_Car Id -> MediaName.
    pub cars: HashMap<u32, String>,
    /// FreeRoamDrivers CarIds (festival drivers), from ailines/ai_tables.json.
    pub festival_cars: Vec<u32>,
}

impl TrafficData {
    pub fn load(assets: &Path, mirror_z: bool) -> anyhow::Result<Self> {
        Self::load_on(assets, mirror_z, None)
    }

    /// As [`load`](Self::load), with lanes fitted to the paved road of `ground` (network.rs "Lane fit").
    pub fn load_on(assets: &Path, mirror_z: bool, ground: Option<&dyn crate::vehicle::Ground>) -> anyhow::Result<Self> {
        let dir = assets.join("traffic");
        let xml = std::fs::read_to_string(dir.join("AIOpenWorld.xml")).map_err(|e| anyhow::anyhow!("{}: {e} (run fh1setup --only traffic)", dir.join("AIOpenWorld.xml").display()))?;
        let config = config::OpenWorldConfig::parse(&xml);
        anyhow::ensure!(config.set("freeroam").is_some(), "AIOpenWorld.xml: no freeroam settings");
        let nav_bytes = std::fs::read(dir.join("colorado.nav")).or_else(|_| std::fs::read(assets.join("ui/map/colorado.nav")))?;
        let nav = fh1_ui::nav::Nav::parse(&nav_bytes).map_err(|e| anyhow::anyhow!("colorado.nav: {e:?}"))?;
        let network = network::Network::build_on(&nav, mirror_z, ground);
        let cars: HashMap<String, String> = serde_json::from_slice(&std::fs::read(dir.join("cars.json"))?)?;
        let cars = cars.into_iter().filter_map(|(k, v)| Some((k.parse().ok()?, v))).collect();
        let festival_cars = std::fs::read(assets.join("ailines/ai_tables.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["FreeRoamDrivers"].as_array().map(|a| a.iter().filter_map(|r| r["CarId"].as_u64().map(|c| c as u32)).collect()))
            .unwrap_or_default();
        Ok(Self { config, network, cars, festival_cars })
    }
}

#[cfg(test)]
mod tests;
