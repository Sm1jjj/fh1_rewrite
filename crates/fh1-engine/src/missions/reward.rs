//! Mission rewards through the career's own paths: credits into the ledger (sent as `CreditsChanged`, so ui/notify.rs
//! toasts them), popularity into `fame` with the same rank-up notices and sponsor milestones as a banked skill chain
//! (progression/skill.rs `bank`), cars into the garage (`OwnedCar`), display-only achievements.
//!
//! The wallet's own `apply` / `add` are `pub(super)` inside progression; these mirror them field for field (ledger
//! length, message) so nothing in progression has to change. If the owner makes `wallet::apply` / `wallet::add`
//! public, the two helpers below can call them instead (docs/MISSIONS.md "Hooks").

use crate::progression::profile::{CarSource, LedgerEntry, OwnedCar, LEDGER_LEN};
use crate::progression::wallet::CreditsChanged;
use crate::progression::{Banners, CareerNotice, Profile};
use crate::race::Events;

/// `FH1_PROGRESSION=0` / `FH1_MISSION_REWARDS=0`: missions pay nothing (records still kept).
pub fn rewards_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::progression::enabled() && std::env::var("FH1_MISSION_REWARDS").map_or(true, |v| v != "0"))
}

/// Credits (ledger + `CreditsChanged`). No save: callers commit.
pub fn credits(profile: &mut Profile, delta: i64, reason: &str) {
    if delta == 0 || !rewards_on() {
        return;
    }
    let p = &mut profile.data;
    p.credits += delta;
    let balance = p.credits;
    p.ledger.push(LedgerEntry { delta, balance, reason: reason.to_owned() });
    if p.ledger.len() > LEDGER_LEN {
        let cut = p.ledger.len() - LEDGER_LEN;
        p.ledger.drain(..cut);
    }
    profile.credit_events.push(CreditsChanged { delta, balance, reason: reason.to_owned() });
}

/// Popularity (fame), with rank-up notices, popularity-gated event unlocks and sponsor milestones as skill.rs banks
/// them. No save: callers commit.
pub fn popularity(profile: &mut Profile, events: &Events, banners: &mut Banners, amount: u64) {
    if amount == 0 || !rewards_on() {
        return;
    }
    let c = &events.career;
    let rank_before = c.rank(profile.data.fame);
    profile.data.fame += amount;
    banners.1.push(CareerNotice::Fame { amount });
    let rank = c.rank(profile.data.fame);
    if rank < rank_before {
        let board = crate::progression::rival_board(c);
        let milestone = (rank..rank_before).filter(|&r| crate::progression::skill::milestone_rank(r)).min();
        if let Some(m) = milestone {
            let passed = board.get(m as usize).and_then(|id| c.driver_name(*id)).map(|n| format!("  passed {n}")).unwrap_or_default();
            banners.push(format!("POPULARITY #{m}{passed}"));
        }
        let passed = board.get(rank as usize).and_then(|id| c.driver_name(*id)).map(str::to_owned);
        banners.1.push(CareerNotice::RankUp { rank, passed, milestone: milestone.is_some() });
        for def in events.races.iter().filter(|d| d.popularity_req > 0 && rank <= d.popularity_req && rank_before > d.popularity_req) {
            banners.push(format!("EVENT UNLOCKED  {}", def.name));
            banners.1.push(CareerNotice::EventUnlocked { name: def.name.clone() });
        }
        crate::progression::pay_rank_milestones(profile, c, rank, banners);
    }
}

/// Into the garage unless owned (true = added). No save: callers commit.
pub fn grant_car(profile: &mut Profile, car: &str, source: CarSource) -> bool {
    let p = &mut profile.data;
    if p.owned.iter().any(|o| o.car == car) {
        return false;
    }
    p.owned.push(OwnedCar { car: car.to_owned(), source, paid: 0 });
    true
}

/// Display-only achievement, once per profile (true = new).
pub fn achievement(profile: &mut Profile, id: &str) -> bool {
    let a = &mut profile.data.missions.achievements;
    if a.iter().any(|x| x == id) {
        return false;
    }
    a.push(id.to_owned());
    true
}

/// The worn wristband tier (0 = Yellow).
pub fn tier(profile: &Profile, events: &Events) -> usize {
    events.career.tier(profile.data.xp)
}
