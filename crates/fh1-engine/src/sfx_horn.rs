//! Car horns (audio_inventory C8): `Horns.fev` events `HornA..F_Long/Short` (bank `Horns`, 18 samples). `FH1_HORN=0` = off.
//!
//! - Player: **H** or the **left stick click** (LeftThumb). Free: H is unused, LeftThumb is only read by the world map
//!   (`ui/worldmap.rs`, not while driving) and the look-back is RightThumb. The binding is INFERRED (FH1/Forza put the
//!   horn on the left stick click; Pinyon Q7). Tap = the Short variant; held past `HOLD_S` = the Long variant until
//!   released (stopped with a short fade). 3D at the player's car.
//! - Variant per car = stable hash of the car's media name over A..F ([`variant_for`]); the original's mapping is
//!   UNKNOWN (INFERRED placeholder, Q7).
//! - Traffic: when `TrafficCar.horn` turns true (blocked by the player, traffic/plugin.rs) that car plays its Long horn,
//!   3D at its position, once per rising edge with a per-car cooldown; at most `MAX_TRAFFIC` at once, within `RANGE_M`.
//!   Stopped when the flag drops.

use std::collections::HashMap;

use bevy::input::gamepad::{Gamepad, GamepadButton};
use bevy::prelude::*;
use fh1_audio::pcm::VoiceId;
use fh1_engine::ai::AiCar;
use fh1_engine::traffic::{TrafficCar, TrafficParked};

use crate::sfx_bank::{Listener, SfxBank};
use crate::sfx_race::{play_item, prefetch_item, sfx_allowed, Ctx, Item, SfxNames};
use crate::Car;

/// Held this long, a tap becomes the Long horn.
const HOLD_S: f32 = 0.18;
const MAX_TRAFFIC: usize = 2;
const RANGE_M: f32 = 120.0;
const COOLDOWN_S: f32 = 6.0;

/// `FH1_HORN=0` turns the horns off.
pub fn horn_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_HORN").map_or(true, |v| v != "0"))
}

pub struct HornPlugin;

impl Plugin for HornPlugin {
    fn build(&self, app: &mut App) {
        if !horn_on() {
            return;
        }
        app.init_resource::<SfxNames>()
            .init_resource::<PlayerHorn>()
            .add_systems(Update, (player_horn.run_if(crate::ui::driving), release_horn.run_if(not(crate::ui::driving)), traffic_horns).chain());
    }
}

/// The horn letter (A..F) for a car: FNV-1a of the lowercase media name, mod 6 (stable across runs and platforms).
pub fn variant_for(car: &str) -> char {
    let mut h: u32 = 0x811c_9dc5;
    for b in car.bytes() {
        h ^= u32::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0100_0193);
    }
    (b'A' + (h % 6) as u8) as char
}

/// The `Horns.fev` event for a car: `HornX_Long` / `HornX_Short`; the plain sample `HornX_<len>_01` as fallback.
fn horn_item(car: &str, long: bool) -> Item {
    let v = variant_for(car);
    let (event, wave): (String, &'static str) = match (v, long) {
        ('A', true) => ("HornA_Long".into(), "HornA_Long_01"),
        ('B', true) => ("HornB_Long".into(), "HornB_Long_01"),
        ('C', true) => ("HornC_Long".into(), "HornC_Long_01"),
        ('D', true) => ("HornD_Long".into(), "HornD_Long_01"),
        ('E', true) => ("HornE_Long".into(), "HornE_Long_01"),
        (_, true) => ("HornF_Long".into(), "HornF_Long_01"),
        ('A', false) => ("HornA_Short".into(), "HornA_Short_01"),
        ('B', false) => ("HornB_Short".into(), "HornB_Short_01"),
        ('C', false) => ("HornC_Short".into(), "HornC_Short_01"),
        ('D', false) => ("HornD_Short".into(), "HornD_Short_01"),
        ('E', false) => ("HornE_Short".into(), "HornE_Short_01"),
        (_, false) => ("HornF_Short".into(), "HornF_Short_01"),
    };
    Item { event, wave: Some(("Horns", wave)) }
}

#[derive(Resource, Default)]
struct PlayerHorn {
    down: bool,
    held_s: f32,
    short: Option<VoiceId>,
    long: Option<VoiceId>,
    /// Car whose horn samples were prefetched (so the first press isn't silent while the WAV loads).
    prefetched: String,
}

fn stop(sfx: &SfxBank, id: Option<VoiceId>, fade: f32) {
    if let (Some(id), Some(m)) = (id, sfx.mixer()) {
        m.stop(id, fade);
    }
}

#[allow(clippy::too_many_arguments)]
fn player_horn(
    keys: Res<ButtonInput<KeyCode>>,
    pads: Query<&Gamepad>,
    cars: Query<&Car>,
    sfx: Option<Res<SfxBank>>,
    listener: Option<Res<Listener>>,
    mut names: ResMut<SfxNames>,
    garage: Res<crate::Garage>,
    virt: Res<Time<Virtual>>,
    real: Res<Time<Real>>,
    mut st: ResMut<PlayerHorn>,
) {
    let _watch = crate::perf::watch("player_horn");
    let (Some(sfx), Some(listener)) = (sfx, listener) else { return };
    let Ok(Car(v)) = cars.single() else { return };
    if st.prefetched != v.data.media_name {
        let mut ctx = Ctx { sfx: &sfx, listener: &listener, names: &mut names, assets: &garage.assets };
        prefetch_item(&mut ctx, "Horns", &horn_item(&v.data.media_name, false));
        prefetch_item(&mut ctx, "Horns", &horn_item(&v.data.media_name, true));
        st.prefetched = v.data.media_name.clone();
    }
    let want = (keys.pressed(KeyCode::KeyH) || pads.iter().any(|p| p.pressed(GamepadButton::LeftThumb))) && sfx_allowed(&virt);
    if !want {
        if st.down {
            stop(&sfx, st.long.take(), 0.08);
            st.short = None;
            st.down = false;
        }
        return;
    }
    if !st.down {
        st.down = true;
        st.held_s = 0.0;
    } else {
        st.held_s += real.delta_secs();
    }
    let mut ctx = Ctx { sfx: &sfx, listener: &listener, names: &mut names, assets: &garage.assets };
    if st.held_s >= HOLD_S {
        if st.long.is_none() {
            st.long = play_item(&mut ctx, "Horns", &horn_item(&v.data.media_name, true), Some(v.position), 1.0);
            if st.long.is_some() {
                stop(&sfx, st.short.take(), 0.05);
            }
        }
    } else if st.short.is_none() {
        st.short = play_item(&mut ctx, "Horns", &horn_item(&v.data.media_name, false), Some(v.position), 1.0);
    }
}

/// Menu / photo mode opened while the horn was down: let go.
fn release_horn(sfx: Option<Res<SfxBank>>, mut st: ResMut<PlayerHorn>) {
    if st.down {
        if let Some(sfx) = sfx {
            stop(&sfx, st.long.take(), 0.08);
        }
        *st = PlayerHorn::default();
    }
}

#[derive(Default)]
struct TrafficHorns {
    /// Car -> (voice, real time it may sound again, flag last frame).
    cars: HashMap<Entity, (Option<VoiceId>, f32, bool)>,
}

#[allow(clippy::too_many_arguments)]
fn traffic_horns(
    traffic: Query<(Entity, &TrafficCar, &AiCar), Without<TrafficParked>>,
    sfx: Option<Res<SfxBank>>,
    listener: Option<Res<Listener>>,
    mut names: ResMut<SfxNames>,
    garage: Res<crate::Garage>,
    virt: Res<Time<Virtual>>,
    real: Res<Time<Real>>,
    mut st: Local<TrafficHorns>,
) {
    let _watch = crate::perf::watch("traffic_horns");
    let (Some(sfx), Some(listener)) = (sfx, listener) else { return };
    let now = real.elapsed_secs();
    let allowed = sfx_allowed(&virt);
    // Cars that are gone or stopped honking.
    let gone: Vec<Entity> = st.cars.keys().copied().filter(|e| traffic.get(*e).map_or(true, |(_, t, _)| !t.horn)).collect();
    for e in gone {
        if let Some((id, until, _)) = st.cars.remove(&e) {
            stop(&sfx, id, 0.1);
            // Keep the cooldown for cars that are still around.
            if traffic.get(e).is_ok() {
                st.cars.insert(e, (None, until, false));
            }
        }
    }
    if !allowed {
        return;
    }
    let mut active = st.cars.values().filter(|(id, _, _)| id.is_some_and(|i| sfx.mixer().is_some_and(|m| m.is_playing(i)))).count();
    let mut ctx = Ctx { sfx: &sfx, listener: &listener, names: &mut names, assets: &garage.assets };
    for (e, tc, ai) in &traffic {
        if !tc.horn || active >= MAX_TRAFFIC {
            continue;
        }
        let entry = st.cars.entry(e).or_insert((None, 0.0, false));
        if entry.0.is_some() || now < entry.1 || ai.0.position.distance(listener.pos) > RANGE_M {
            continue;
        }
        let id = play_item(&mut ctx, "Horns", &horn_item(&ai.0.data.media_name, true), Some(ai.0.position), 1.0);
        if id.is_some() {
            *entry = (id, now + COOLDOWN_S, true);
            active += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_hash_is_stable() {
        // Fixed values: the hash must never change between builds (a car keeps its horn).
        assert_eq!(variant_for("ALF_8C_08"), variant_for("alf_8c_08"));
        let v: Vec<char> = ["ALF_8C_08", "FER_FXX_05", "NIS_Leaf_11", ""].iter().map(|c| variant_for(c)).collect();
        assert_eq!(v[3], variant_for(""));
        assert!(v.iter().all(|c| ('A'..='F').contains(c)));
        // FNV-1a("") = 0x811c9dc5 = 2166136261, mod 6 = 1 -> B.
        assert_eq!(variant_for(""), 'B');
        // FNV-1a("a") = 0xe40c292c = 3826002220, mod 6 = 4 -> E.
        assert_eq!(variant_for("a"), 'E');
    }

    #[test]
    fn items_name_the_fev_events() {
        let i = horn_item("a", true);
        assert_eq!((i.event.as_str(), i.wave), ("HornE_Long", Some(("Horns", "HornE_Long_01"))));
        assert_eq!(horn_item("a", false).event, "HornE_Short");
    }
}
