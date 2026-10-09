//! The race module's link to R2's AI opponents (`fh1_engine::ai`, agreed with fh1-rewrite-d6): `SpawnRaceAi` per
//! grid slot, `AiRacer { slot }` + `AiCar(Vehicle)` on the spawned cars, `DespawnRaceAi`, and `AiRaceControl`
//! (hold until GO, finished slots, the active route). Everything is optional: without R2's plugin (no message
//! queue / control resource) races run solo. Also the collision's event barrier bits.

use std::collections::HashMap;

use bevy::ecs::message::Messages;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use fh1_engine::ai::{AiCar, AiRaceControl, AiRacer, DespawnRaceAi, SpawnRaceAi};

use super::field::FieldEntry;
use super::{crossed, gate, total_gates, RaceDef, RacePhase, Racer};

/// Barrier bits of the running race into the collision (`WorldGround::event_routes`, OR'd with free roam's).
/// `FH1_RACE_BARRIERS=0` keeps free roam's collision during races.
pub fn set_barriers(track: &crate::track::Track, bits: u16) {
    if let Some(w) = &track.world {
        w.event_routes.store(if barriers_on() { bits & 0x7FFF } else { 0 }, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `FH1_RACE_BARRIER_TRIS=0`: the old event walls (the setup's inferred route-mask bits `barrier_bits`, which switch on
/// every triangle carrying the bit on the whole map: invisible walls where this race has no barrier) instead of the
/// triangles under the race's own barrier objects (docs/RACES.md "Event walls").
pub fn barrier_tris_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_BARRIER_TRIS").map_or(true, |v| v != "0"))
}

/// The running race's event-wall triangles into the collision (`WorldGround::set_event_tris`; empty = none). Honours
/// `FH1_RACE_BARRIERS=0` like [`set_barriers`].
pub fn set_barrier_tris(track: &crate::track::Track, tris: std::collections::HashSet<u32>) {
    if let Some(w) = &track.world {
        w.set_event_tris(if barriers_on() { tris } else { Default::default() });
    }
}

fn barriers_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_BARRIERS").map_or(true, |v| v != "0"))
}

/// `FH1_RACE_AI=0`: no opponents (solo races) even when R2's AI is present.
fn ai_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_RACE_AI").map_or(true, |v| v != "0"))
}

#[derive(SystemParam)]
pub struct AiLink<'w, 's> {
    spawn: Option<ResMut<'w, Messages<SpawnRaceAi>>>,
    despawn: Option<ResMut<'w, Messages<DespawnRaceAi>>>,
    control: Option<ResMut<'w, AiRaceControl>>,
    /// AI difficulty (Options): which Events column the opponents use.
    settings: Option<Res<'w, crate::ui::Settings>>,
    cars: Query<'w, 's, (Entity, &'static AiRacer, &'static AiCar)>,
    /// Last position per slot (gate crossing).
    last: Local<'s, HashMap<u32, Vec3>>,
}

impl AiLink<'_, '_> {
    /// Whether R2's AI plugin is present.
    pub fn available(&self) -> bool {
        ai_on() && self.spawn.is_some() && self.control.is_some()
    }

    /// Options "AI difficulty" column (0 Easy .. 3 Pro; Medium without settings).
    pub fn difficulty(&self) -> usize {
        self.settings.as_ref().map_or(1, |s| s.ai_difficulty.index())
    }

    /// Ask for an opponent at grid slot `slot` (1..), as picked by `field::build`.
    pub fn spawn(&mut self, slot: u32, e: &FieldEntry, pose: (Vec3, f32), def: &RaceDef) {
        let Some(q) = self.spawn.as_mut() else { return };
        q.write(SpawnRaceAi {
            slot,
            car_id: e.car.clone(),
            pose,
            skill: e.skill,
            temperament: e.temperament,
            rubberband: e.rubberband,
            driver_id: e.driver,
            route_file: def.route_file.clone(),
            circuit: def.circuit,
            paint: e.color_seq,
            tune: e.tune,
            ..Default::default()
        });
        self.last.remove(&slot);
    }

    pub fn despawn_all(&mut self) {
        if let Some(q) = self.despawn.as_mut() {
            q.write(DespawnRaceAi);
        }
        self.last.clear();
        if let Some(c) = self.control.as_mut() {
            c.hold = false;
            c.finished.clear();
            c.route_file = None;
        }
    }

    /// Advance the AI racers' gates / laps / finish times (racer index = grid slot; 0 is the player).
    pub fn update_racers(&mut self, racers: &mut [Racer], def: &RaceDef, clock: f32) {
        let total = total_gates(def);
        let per_lap = def.gates.len() as u32;
        for (e, r, car) in &self.cars {
            let Some(racer) = racers.get_mut(r.slot as usize) else { continue };
            racer.entity = Some(e);
            let pos = car.0.position;
            if racer.finished_s.is_none() {
                if let Some(a) = self.last.insert(r.slot, pos) {
                    let g = *gate(def, racer.gates_done);
                    if crossed(&g, a, pos) {
                        racer.gates_done += 1;
                        if racer.gates_done >= total {
                            racer.finished_s = Some(clock);
                        } else {
                            racer.lap = racer.gates_done / per_lap.max(1) + 1;
                        }
                    }
                }
            }
            let next = gate(def, racer.gates_done.min(total.saturating_sub(1)));
            racer.to_next = Vec2::new(next.centre.x - pos.x, next.centre.z - pos.z).length();
        }
    }

    /// Hold the AI until GO; tell it which route is running and who has finished.
    pub fn set_control(&mut self, phase: RacePhase, route_file: Option<String>, racers: &[Racer]) {
        let Some(c) = self.control.as_mut() else { return };
        let hold = matches!(phase, RacePhase::Grid { .. } | RacePhase::Countdown { .. });
        let route = route_file.filter(|_| phase != RacePhase::Idle);
        let finished: Vec<u32> = racers.iter().enumerate().filter(|(_, r)| !r.is_player && r.finished_s.is_some()).map(|(i, _)| i as u32).collect();
        if c.hold != hold || c.route_file != route || c.finished != finished {
            c.hold = hold;
            c.route_file = route;
            c.finished = finished;
        }
    }
}
