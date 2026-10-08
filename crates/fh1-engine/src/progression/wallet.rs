//! The credits ledger and the player's garage (P10 economy, 2026-10-08; docs/PROGRESSION.md "Economy"). One place for
//! every credit change: races, sponsors, nemesis bonuses (progression), buying / selling cars (this file), parts / rims /
//! kits (the garage menus, ui/*, and upgrades, which call [`spend`]). Each change is saved (profile.json via the
//! background writer) and sent as a [`CreditsChanged`] message for the HUD / notifications.
//!
//! VERIFIED on the EU disc: autoshow price = Data_Car.BaseCost; sell value = BaseCost x CarValueScales SellValueScale
//! (0.5); IsSelectable = 0 (traffic) and IsUnicorn cars are not in the autoshow; barn finds can't be sold (Barnfind
//! IDS_Trade_BarnFind_Message); paint is free (no price anywhere).
//! INFERRED: cars with BaseCost 0 are not for sale; bought parts add half their price to the sell value (the caller
//! passes it); prize / wristband reward cars sell for CarValueScales RewardCarsValue (100 CR); the starting garage is
//! the VW Corrado (the car the game's festival start uses, docs/RENDERDOC capture) with 10,000 CR (E3Demo/BaseCredits,
//! the only starting figure on the disc); no wristband / popularity gate on the autoshow.

use bevy::prelude::*;

use super::data::CareerData;
use super::profile::{CarSource, LedgerEntry, OwnedCar, Profile, ProfileData, LEDGER_LEN};

/// The starting garage (INFERRED, see the module doc).
pub const STARTER_CARS: [&str; 1] = ["VW_Corrado_95"];
/// Credits a new career starts with (INFERRED: GlobalRegistry E3Demo/BaseCredits).
pub const STARTING_CREDITS: i64 = 10_000;

/// A credits change, sent the frame it happens.
#[derive(Message, Clone, Debug)]
pub struct CreditsChanged {
    pub delta: i64,
    pub balance: i64,
    pub reason: String,
}

/// Car ownership gates "Change car" (the menus filter on it). `FH1_OWNERSHIP=0` = every car drivable (old).
pub fn ownership_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::enabled() && std::env::var("FH1_OWNERSHIP").map_or(true, |v| v != "0"))
}

/// Profile version 1 -> 2: starter garage, prize / wristband cars into the garage, starting credits for a new career.
pub(super) fn migrate(p: &mut ProfileData) {
    if p.version >= 2 {
        return;
    }
    let new_career = p.version == 0 && p.events.is_empty() && p.credits == 0 && p.xp == 0;
    for car in STARTER_CARS {
        add(p, car, CarSource::Starter, 0);
    }
    for car in p.cars_won.clone() {
        add(p, &car, CarSource::Prize, 0);
    }
    if new_career {
        p.credits = STARTING_CREDITS;
        push_ledger(p, STARTING_CREDITS, "New career");
    }
    p.version = 2;
}

fn add(p: &mut ProfileData, car: &str, source: CarSource, paid: i64) -> bool {
    if p.owned.iter().any(|o| o.car == car) {
        return false;
    }
    p.owned.push(OwnedCar { car: car.to_owned(), source, paid });
    true
}

fn push_ledger(p: &mut ProfileData, delta: i64, reason: &str) {
    p.ledger.push(LedgerEntry { delta, balance: p.credits, reason: reason.to_owned() });
    if p.ledger.len() > LEDGER_LEN {
        let cut = p.ledger.len() - LEDGER_LEN;
        p.ledger.drain(..cut);
    }
}

// ---- Credits ----

pub fn credits(p: &Profile) -> i64 {
    p.data.credits
}

pub fn can_afford(p: &Profile, n: i64) -> bool {
    n <= p.data.credits
}

/// Change the balance by `delta` without saving (callers that change more, e.g. race results, commit once).
pub(super) fn apply(p: &mut Profile, delta: i64, reason: &str) {
    if delta == 0 {
        return;
    }
    p.data.credits += delta;
    let balance = p.data.credits;
    push_ledger(&mut p.data, delta, reason);
    p.credit_events.push(CreditsChanged { delta, balance, reason: reason.to_owned() });
}

/// Pay `n` credits. False (and nothing changes) when the balance is short. Saves.
pub fn spend(p: &mut Profile, n: i64, reason: &str) -> bool {
    if n < 0 || !can_afford(p, n) {
        return false;
    }
    apply(p, -n, reason);
    p.commit();
    true
}

/// Receive `n` credits. Saves.
pub fn earn(p: &mut Profile, n: i64, reason: &str) {
    if n <= 0 {
        return;
    }
    apply(p, n, reason);
    p.commit();
}

// ---- Cars ----

pub fn owned(p: &Profile) -> &[OwnedCar] {
    &p.data.owned
}

pub fn owns(p: &Profile, car: &str) -> bool {
    p.data.owned.iter().any(|o| o.car == car)
}

/// Adds a car without paying (prizes, wristband rewards, barn finds). False if already owned. Saves.
pub fn add_car(p: &mut Profile, car: &str, source: CarSource) -> bool {
    let added = add(&mut p.data, car, source, 0);
    if added {
        p.commit();
    }
    added
}

/// Autoshow price (None = not for sale: traffic, unicorns, BaseCost 0, or no data).
pub fn price(c: &CareerData, car: &str) -> Option<i64> {
    let info = c.cars.get(car)?;
    (info.selectable && !info.unicorn && info.price > 0).then_some(info.price)
}

/// Why `car` can't be bought now (None = it can).
pub fn buy_lock(c: &CareerData, p: &Profile, car: &str) -> Option<String> {
    if owns(p, car) {
        return Some("Already owned".into());
    }
    let Some(cost) = price(c, car) else { return Some("Not for sale".into()) };
    (!can_afford(p, cost)).then(|| format!("INSUFFICIENT CR! ({} needed)", super::fmt_num(cost)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuyError {
    AlreadyOwned,
    NotForSale,
    NotEnough { need: i64, have: i64 },
}

/// Buy `car` at its autoshow price. Returns the price paid. Saves.
pub fn buy(p: &mut Profile, c: &CareerData, car: &str) -> Result<i64, BuyError> {
    if owns(p, car) {
        return Err(BuyError::AlreadyOwned);
    }
    let cost = price(c, car).ok_or(BuyError::NotForSale)?;
    if !can_afford(p, cost) {
        return Err(BuyError::NotEnough { need: cost, have: p.data.credits });
    }
    apply(p, -cost, &format!("Bought {}", display(c, car)));
    add(&mut p.data, car, CarSource::Bought, cost);
    p.commit();
    Ok(cost)
}

/// What selling `car` pays: (BaseCost + `parts_value`) x SellValueScale; prize / wristband cars RewardCarsValue;
/// None = can't be sold (not owned, a barn find, or no value).
pub fn sell_price(c: &CareerData, p: &Profile, car: &str, parts_value: i64) -> Option<i64> {
    let o = p.data.owned.iter().find(|o| o.car == car)?;
    match o.source {
        CarSource::BarnFind => None,
        CarSource::Prize | CarSource::Wristband => Some(c.economy.reward_cars_value as i64),
        _ => {
            let base = c.cars.get(car).map_or(o.paid, |i| i.price.max(o.paid));
            let v = ((base + parts_value.max(0)) as f64 * c.economy.sell_value_scale).round() as i64;
            (v > 0).then_some(v)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SellError {
    NotOwned,
    /// The last car in the garage.
    LastCar,
    /// The car being driven (carflow IDS_Sell_Car_Error_Already_In_Car).
    Current,
    /// Barn finds (IDS_Trade_BarnFind_Message).
    CantSell,
}

/// Sell `car` (`current` = the car being driven). Returns the credits received. Saves.
pub fn sell(p: &mut Profile, c: &CareerData, car: &str, current: Option<&str>, parts_value: i64) -> Result<i64, SellError> {
    if !owns(p, car) {
        return Err(SellError::NotOwned);
    }
    if current == Some(car) {
        return Err(SellError::Current);
    }
    if p.data.owned.len() <= 1 {
        return Err(SellError::LastCar);
    }
    let value = sell_price(c, p, car, parts_value).ok_or(SellError::CantSell)?;
    p.data.owned.retain(|o| o.car != car);
    apply(p, value, &format!("Sold {}", display(c, car)));
    p.commit();
    Ok(value)
}

fn display(c: &CareerData, car: &str) -> String {
    c.cars.get(car).map(|i| format!("{} {}", i.make, i.name).trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| car.to_owned())
}

/// Send the pending credit changes (every frame).
pub(super) fn flush_credit_events(mut profile: ResMut<Profile>, mut out: MessageWriter<CreditsChanged>) {
    if !profile.credit_events.is_empty() {
        out.write_batch(std::mem::take(&mut profile.credit_events));
    }
}
