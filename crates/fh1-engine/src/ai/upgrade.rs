//! AI opponents upgraded to their event's class (docs/AI.md P17; `FH1_AI_UPGRADE=0` = stock cars as before).
//!
//! The game tunes AI cars to the event (VERIFIED data: Events.UpgradeAI = 1 on 116 of 119 events, EventParticipants.
//! TuningLevel = 1 on 103 of 742 entries, GameTunableSettings.ini `AIAutoUpgrades`; 335 of 742 stock AI cars sit below
//! their TargetClass floor). The mechanism is INFERRED from those names and values; the search below is ours:
//!
//! 1. [`needs_tune`]: a car above the class cap stays stock (the game doesn't detune, INFERRED); TuningLevel 0 keeps a car
//!    that is already in the band; TuningLevel 1 is tuned whenever it is under the target (just below the cap).
//! 2. Parts first, the player's way: an upgrade sequence over the car's real gamedb rows (`customize_upgrades::catalog`,
//!    the same `patch` the player's Customize uses, so the physics are exactly a player build with those parts), ordered by
//!    gamedb Level, then tyre compound / weight / engine / aspiration / drivetrain / chassis / tyre width (no engine swap,
//!    body kits or rims; INFERRED order). A binary search over the prefix length finds the largest build with PI <= target
//!    (about log2(steps) `pi_preview` evaluations, cached by (car, parts)).
//! 3. If the parts leave the car under the class floor: the `AIAutoUpgrades` "fake" scales on top, one factor t in [0,1]
//!    lerping mass 1 -> 0.9, engine torque 1 -> 1.4, tyre friction 1 -> 1.2, bisected for `Iterations` (10) steps to the
//!    target PI. They are physics.json edits (`torque_curve.torque_scale_nm`, `stock_parts.List_UpgradeCarBodyWeight.Mass`,
//!    `tires.compound.TireFricScale0/1`), so `pi::compute` and `CarData` see the same car. The mass scale multiplies the
//!    base curb mass only (the MassDiff sums of parts are not scaled; at most a few percent off, INFERRED).
//!    `FakedPIRange` (5) / `PIAdditionForFaking` (300) are not used: INFERRED to be the game's PI bonus when the target can't
//!    be reached; we tune the real physics instead and accept a car that stays under the floor.
//!
//! CarPIOverrides (40 CarIDs) has no PI column in gamedb (CarID only; docs/PI.md section 9: a recalculation list), so there is
//! no fixed PI to apply: the stock PI is Data_Car.PerformanceIndex.
//!
//! Included from main.rs as `#[path = "ai/upgrade.rs"] mod ai_upgrade;` (it needs the Customize upgrade code of the bin).
//! Cost per car: 0 evaluations when the car stays stock; about 6-7 for the parts; up to 11 more (each 3 analytic laps, a few
//! ms) when the fake scales are needed. Memoised per (car, band, target).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use bevy::prelude::*;
use fh1_engine::ai::AiTuneReq;
use fh1_engine::data::CarData;
use fh1_engine::pi::{self, PiConfig, PiResult};
use serde_json::{json, Value};

use crate::ui::customize_upgrades::{self as upg, Part};

/// GameTunableSettings.ini `AIAutoUpgrades` (VERIFIED on the EU disc): MinMassScale, MaxTorqueScale,
/// MaxFront/RearTireFrictionScale (both 1.2), Iterations.
const MIN_MASS_SCALE: f32 = 0.9;
const MAX_TORQUE_SCALE: f32 = 1.4;
const MAX_TIRE_FRICTION_SCALE: f32 = 1.2;
const ITERATIONS: usize = 10;

/// `FH1_AI_UPGRADE=0`: AI opponents race as stock cars.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_AI_UPGRADE").map_or(true, |v| v != "0"))
}

/// A tuned AI car: the parts (gamedb table -> row Id, as the player's `chosen`) and the fake scales on top.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct AiTune {
    pub chosen: BTreeMap<String, i64>,
    /// Fake scales (1 = none): base mass, engine torque, tyre friction.
    pub mass: f32,
    pub torque: f32,
    pub tyre: f32,
    /// PI after tuning (pi.rs) and its CarClasses index.
    pub pi: f32,
    pub class: usize,
    /// Parts fitted.
    pub parts: usize,
}

impl AiTune {
    /// Edit `physics.json`: the parts exactly as the player's Customize does, then the fake scales.
    pub fn apply(&self, p: &mut Value, doc: &Value) {
        if upg::enabled() {
            upg::patch(p, doc, &self.chosen);
        }
        apply_fake(p, self.mass, self.torque, self.tyre);
    }
}

/// The AI car's data: stock, or with the tune applied (`CarData::load_with`, as the player's car loads its upgrades).
pub fn load_car(assets: &Path, dir: &Path, car: &str, tune: Option<&AiTune>) -> Result<CarData> {
    let Some(t) = tune else { return CarData::load(dir) };
    let doc = if upg::enabled() && !t.chosen.is_empty() { upg::read_doc(assets, car) } else { Value::Null };
    CarData::load_with(dir, |p| t.apply(p, &doc))
}

/// The game's fake scales at factor `t` in [0,1]: (mass, torque, tyre friction).
fn fake_scales(t: f32) -> (f32, f32, f32) {
    (1.0 + t * (MIN_MASS_SCALE - 1.0), 1.0 + t * (MAX_TORQUE_SCALE - 1.0), 1.0 + t * (MAX_TIRE_FRICTION_SCALE - 1.0))
}

/// The fake scales as a physics.json edit (after the parts patch: a tyre compound row replaces `tires`).
fn apply_fake(p: &mut Value, mass: f32, torque: f32, tyre: f32) {
    if (mass - 1.0).abs() > 1e-6 {
        if let Some(m) = p["stock_parts"]["List_UpgradeCarBodyWeight"]["Mass"].as_f64() {
            p["stock_parts"]["List_UpgradeCarBodyWeight"]["Mass"] = json!(m * mass as f64);
        }
    }
    if (torque - 1.0).abs() > 1e-6 {
        if let Some(t) = p["torque_curve"]["torque_scale_nm"].as_f64() {
            p["torque_curve"]["torque_scale_nm"] = json!(t * torque as f64);
        }
    }
    if (tyre - 1.0).abs() > 1e-6 {
        // TireFricScale(width) is baked into both friction tables (data.rs TyreColumns / pi.rs): it scales force and peak.
        for k in ["TireFricScale0", "TireFricScale1"] {
            let v = p["tires"]["compound"][k].as_f64().unwrap_or(1.0);
            p["tires"]["compound"][k] = json!(v * tyre as f64);
        }
    }
}

/// Whether a car with stock PI `stock` gets tuned for `req`.
pub(crate) fn needs_tune(stock: f32, req: &AiTuneReq) -> bool {
    if stock > req.hi {
        false
    } else if req.tuning_level >= 1 {
        stock < req.target
    } else {
        stock < req.lo
    }
}

/// Largest prefix length k in 0..=n with `pi_of(k) <= target`, by bisection (k = 0, the stock car, is assumed to fit).
/// A NaN (failed evaluation) counts as too high.
pub(crate) fn longest_prefix(n: usize, mut pi_of: impl FnMut(usize) -> f32, target: f32) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if pi_of(mid) <= target {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// The fake factor: t = 1 when full scales still fit under `target`, else the largest t found in `iters` bisections
/// (0 = nothing fits).
pub(crate) fn fake_search(mut pi_of: impl FnMut(f32) -> f32, target: f32, iters: usize) -> f32 {
    if pi_of(1.0) <= target {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..iters {
        let mid = 0.5 * (lo + hi);
        if pi_of(mid) <= target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Pseudo table in [`ORDER`]: the one aspiration table the AI uses.
const ASPIRATION: &str = "@aspiration";

/// Part tables in the order the AI fits them within one gamedb Level (INFERRED): grip and weight first, then the engine,
/// aspiration, drivetrain and chassis, tyre width last. Not offered: engine swap, body kit rows, rim style / size, oil cooling.
const ORDER: [&str; 24] = [
    "List_UpgradeTireCompound",
    "List_UpgradeCarBodyWeight",
    "List_UpgradeEngineIntake",
    "List_UpgradeEngineFuelSystem",
    "List_UpgradeEngineIgnition",
    "List_UpgradeEngineExhaust",
    "List_UpgradeEngineCamshaft",
    "List_UpgradeEngineValves",
    "List_UpgradeEngineDisplacement",
    "List_UpgradeEnginePistonsCompression",
    "List_UpgradeEngineManifold",
    "List_UpgradeEngineFlywheel",
    ASPIRATION,
    "List_UpgradeEngineIntercooler",
    "List_UpgradeDrivetrainTransmission",
    "List_UpgradeDrivetrainDifferential",
    "List_UpgradeDrivetrainClutch",
    "List_UpgradeDrivetrainDriveline",
    "List_UpgradeBrakes",
    "List_UpgradeSpringDamper",
    "List_UpgradeAntiSwayFront",
    "List_UpgradeAntiSwayRear",
    "List_UpgradeCarBodyChassisStiffness",
    "List_UpgradeCarBodyTireWidthFront",
];
/// Rear tyre width follows the front one.
const LAST: &str = "List_UpgradeCarBodyTireWidthRear";

/// The aspiration table to upgrade: the car's own (a boost table with a real stock row), else the first offered of
/// single turbo, centrifugal, positive-displacement, twin turbo.
fn aspiration_table(parts: &[Part]) -> Option<&'static str> {
    const BOOST: [&str; 4] = ["List_UpgradeEngineTurboSingle", "List_UpgradeEngineCSC", "List_UpgradeEngineDSC", "List_UpgradeEngineTurboTwin"];
    let has_stock = |t: &str| parts.iter().any(|p| p.table == t && p.options.iter().zip(&p.stock).any(|(o, &s)| s && o.0 >= 0));
    BOOST.iter().copied().find(|t| has_stock(*t)).or_else(|| BOOST.iter().copied().find(|t| parts.iter().any(|p| p.table == *t)))
}

/// The upgrade sequence: for each gamedb Level 1.., each table of [`ORDER`] that has a non-stock row of that Level
/// (the first one) is one step (a later Level's row replaces the table's earlier step).
pub(crate) fn build_steps(parts: &[Part]) -> Vec<(&'static str, i64)> {
    let asp = aspiration_table(parts);
    let top = parts.iter().flat_map(|p| p.levels.iter().copied()).max().unwrap_or(0);
    let mut steps = Vec::new();
    for level in 1..=top {
        for name in ORDER.iter().copied().chain(std::iter::once(LAST)) {
            let table = if name == ASPIRATION {
                match asp {
                    Some(t) => t,
                    None => continue,
                }
            } else {
                name
            };
            let Some(part) = parts.iter().find(|p| p.table == table) else { continue };
            let pick = (0..part.options.len()).find(|&i| !part.stock[i] && part.options[i].0 >= 0 && part.levels.get(i) == Some(&level));
            if let Some(i) = pick {
                steps.push((part.table, part.options[i].0));
            }
        }
    }
    steps
}

/// The parts chosen by the first `k` steps.
fn chosen_of(steps: &[(&'static str, i64)], k: usize) -> BTreeMap<String, i64> {
    steps.iter().take(k).map(|&(t, id)| (t.to_owned(), id)).collect()
}

fn pi_config(assets: &Path) -> Option<&'static PiConfig> {
    static CFG: OnceLock<Option<PiConfig>> = OnceLock::new();
    CFG.get_or_init(|| PiConfig::load(assets)).as_ref()
}

type Key = (String, u32, u32, u32, u32);

/// Tune `car` for `req`; None = stay stock (not needed, no PI data, or nothing helps). Memoised per (car, request).
pub fn plan(assets: &Path, car: &str, req: &AiTuneReq) -> Option<AiTune> {
    static MEMO: OnceLock<Mutex<HashMap<Key, Option<AiTune>>>> = OnceLock::new();
    if !enabled() {
        return None;
    }
    let key: Key = (car.to_owned(), req.lo.to_bits(), req.hi.to_bits(), req.target.to_bits(), req.tuning_level);
    let memo = MEMO.get_or_init(Default::default);
    if let Some(r) = memo.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return r;
    }
    let r = plan_uncached(assets, car, req);
    if let Ok(mut m) = memo.lock() {
        m.insert(key, r.clone());
    }
    r
}

fn plan_uncached(assets: &Path, car: &str, req: &AiTuneReq) -> Option<AiTune> {
    let dir = assets.join("cars").join(car);
    let base: Value = serde_json::from_slice(&std::fs::read(dir.join("physics.json")).ok()?).ok()?;
    let stock = base["car"]["PerformanceIndex"].as_f64()? as f32;
    if !needs_tune(stock, req) {
        return None;
    }
    let cfg = pi_config(assets)?;
    let doc = upg::read_doc(assets, car);
    let steps = if upg::enabled() { build_steps(&upg::catalog(&doc)) } else { Vec::new() };
    let mut seen: HashMap<usize, PiResult> = HashMap::new();
    let k = longest_prefix(
        steps.len(),
        |k| match upg::pi_preview(assets, &dir, car, &chosen_of(&steps, k)) {
            Some(r) => {
                let pi = r.pi;
                seen.insert(k, r);
                pi
            }
            None => f32::NAN,
        },
        req.target,
    );
    let chosen = chosen_of(&steps, k);
    let mut cur = seen.remove(&k).map_or(stock, |r| r.pi);
    let (mut mass, mut torque, mut tyre) = (1.0, 1.0, 1.0);
    if cur < req.lo {
        // Parts can't reach the floor: the game's fake scales on top of the best build.
        let mut pp = base.clone();
        if upg::enabled() {
            upg::patch(&mut pp, &doc, &chosen);
        }
        let mut tried: Vec<(f32, f32)> = Vec::new();
        let t = fake_search(
            |t| {
                let (m, tq, ty) = fake_scales(t);
                let mut p = pp.clone();
                apply_fake(&mut p, m, tq, ty);
                let v = pi::compute(&p, cfg).map_or(f32::NAN, |r| r.pi);
                tried.push((t, v));
                v
            },
            req.target,
            ITERATIONS,
        );
        if let Some(&(_, v)) = tried.iter().find(|(x, _)| *x == t) {
            if t > 0.0 && v > cur {
                (mass, torque, tyre) = fake_scales(t);
                cur = v;
            }
        }
    }
    if k == 0 && mass == 1.0 && torque == 1.0 && tyre == 1.0 {
        info!("AI tune {car}: PI {stock:.3} stays (band {:.3}..{:.3}, target {:.3}): no part or scale helps", req.lo, req.hi, req.target);
        return None;
    }
    let class = pi::class_index(cur as f64, cfg);
    info!(
        "AI tune {car}: PI {stock:.3} -> {cur:.3} (band {:.3}..{:.3}, target {:.3}), {k} parts, mass x{mass:.3} torque x{torque:.3} tyre x{tyre:.3}",
        req.lo, req.hi, req.target
    );
    Some(AiTune { chosen, mass, torque, tyre, pi: cur, class, parts: k })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(lo: f32, hi: f32, target: f32, tuning_level: u32) -> AiTuneReq {
        AiTuneReq { lo, hi, target, tuning_level }
    }

    #[test]
    fn who_gets_tuned() {
        let r = req(0.5975, 0.6505, 0.64, 0);
        assert!(needs_tune(0.495, &r), "below the floor");
        assert!(!needs_tune(0.62, &r), "in the band, TuningLevel 0: stock");
        assert!(!needs_tune(0.70, &r), "above the cap: stock");
        let t = req(0.5975, 0.6505, 0.6465, 1);
        assert!(needs_tune(0.62, &t), "TuningLevel 1 goes up to the target");
        assert!(!needs_tune(0.648, &t));
        assert!(!needs_tune(0.66, &t));
    }

    #[test]
    fn prefix_bisection() {
        // PI rises 0.01 per step from 0.50.
        let mut evals = 0;
        let k = longest_prefix(40, |k| {
            evals += 1;
            0.5 + 0.01 * k as f32
        }, 0.565);
        assert_eq!(k, 6);
        assert!(evals <= 7, "{evals} evaluations");
        assert_eq!(longest_prefix(0, |_| unreachable!(), 0.6), 0);
        assert_eq!(longest_prefix(10, |_| 0.9, 0.6), 0, "nothing fits");
        assert_eq!(longest_prefix(10, |_| 0.5, 0.6), 10, "everything fits");
        assert_eq!(longest_prefix(10, |_| f32::NAN, 0.6), 0, "failed evaluations");
    }

    #[test]
    fn fake_bisection() {
        let mut n = 0;
        let t = fake_search(|t| {
            n += 1;
            0.5 + 0.2 * t
        }, 0.6, ITERATIONS);
        assert!((t - 0.5).abs() < 0.002, "{t}");
        assert_eq!(n, 1 + ITERATIONS);
        assert_eq!(fake_search(|_| 0.5, 0.6, ITERATIONS), 1.0, "full scales fit");
        assert_eq!(fake_search(|_| 0.9, 0.6, ITERATIONS), 0.0, "nothing fits");
        let (m, tq, ty) = fake_scales(1.0);
        assert!((m - 0.9).abs() < 1e-6 && (tq - 1.4).abs() < 1e-6 && (ty - 1.2).abs() < 1e-6);
        assert_eq!(fake_scales(0.0), (1.0, 1.0, 1.0));
    }

    fn part(table: &'static str, levels: &[i64], real_stock: bool) -> Part {
        // Option 0 = stock (or "None" for a part the car lacks), then one row per level.
        let mut options = vec![(if real_stock { 100 } else { -1 }, "Stock".to_owned(), String::new())];
        let mut lv = vec![if real_stock { 0 } else { -1 }];
        let mut stock = vec![true];
        for (i, &l) in levels.iter().enumerate() {
            options.push((table.len() as i64 * 10 + i as i64, format!("L{l}"), String::new()));
            lv.push(l);
            stock.push(false);
        }
        let prices = vec![0; options.len()];
        Part { table, label: table.to_owned(), area: "Engine", name: "x", options, prices, levels: lv, stock }
    }

    #[test]
    fn step_order() {
        let parts = vec![
            part("List_UpgradeEngineIntake", &[1, 2, 3], true),
            part("List_UpgradeTireCompound", &[1, 2], true),
            part("List_UpgradeEngineTurboSingle", &[1, 2, 3], false),
            part("List_UpgradeEngineCSC", &[1, 2, 3], false),
            part("List_UpgradeEngine", &[1], true),
            part("List_UpgradeCarBodyWeight", &[3], true),
        ];
        let steps = build_steps(&parts);
        let names: Vec<&str> = steps.iter().map(|s| s.0).collect();
        // Level 1: compound, intake, turbo (single beats CSC); level 2: the same; level 3: weight, intake, turbo. No swap.
        assert_eq!(
            names,
            [
                "List_UpgradeTireCompound",
                "List_UpgradeEngineIntake",
                "List_UpgradeEngineTurboSingle",
                "List_UpgradeTireCompound",
                "List_UpgradeEngineIntake",
                "List_UpgradeEngineTurboSingle",
                "List_UpgradeCarBodyWeight",
                "List_UpgradeEngineIntake",
                "List_UpgradeEngineTurboSingle",
            ]
        );
        let c = chosen_of(&steps, 6);
        assert_eq!(c.len(), 3, "later levels replace earlier rows of the same table");
        // A car with a stock supercharger upgrades that table.
        let own = vec![part("List_UpgradeEngineCSC", &[1], true), part("List_UpgradeEngineTurboSingle", &[1], false)];
        assert_eq!(aspiration_table(&own), Some("List_UpgradeEngineCSC"));
        assert!(build_steps(&[]).is_empty());
    }

    #[test]
    fn fake_scales_edit_physics() {
        let mut p = json!({
            "stock_parts": {"List_UpgradeCarBodyWeight": {"Mass": 1000.0}},
            "torque_curve": {"torque_scale_nm": 300.0},
            "tires": {"compound": {"TireFricScale0": 1.0, "TireFricScale1": 0.9}}
        });
        apply_fake(&mut p, 0.9, 1.4, 1.2);
        assert!((p["stock_parts"]["List_UpgradeCarBodyWeight"]["Mass"].as_f64().unwrap() - 900.0).abs() < 1e-3);
        assert!((p["torque_curve"]["torque_scale_nm"].as_f64().unwrap() - 420.0).abs() < 1e-3);
        assert!((p["tires"]["compound"]["TireFricScale0"].as_f64().unwrap() - 1.2).abs() < 1e-6);
        assert!((p["tires"]["compound"]["TireFricScale1"].as_f64().unwrap() - 1.08).abs() < 1e-6);
        let before = p.clone();
        apply_fake(&mut p, 1.0, 1.0, 1.0);
        assert_eq!(p, before);
    }

    /// Every installed race: the AI field's PI after tuning is at least the class floor for >= 95% of the entries. Needs
    /// the converted install (cars, upgrades, events groups); skips silently otherwise.
    /// `cargo test --release -p fh1-engine --bin fh1-engine ai_upgrade::tests::ai_field_pi -- --nocapture`.
    #[test]
    fn ai_field_pi() {
        use crate::race::field::{self, FieldCtx};
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let Ok(assets) = fh1_engine::data::private_assets(&data) else { return };
        if !assets.join("upgrades/pi.json").exists() || !assets.join("events/colorado/events.json").exists() || !assets.join("cars").exists() {
            return;
        }
        if !enabled() {
            return;
        }
        let events = crate::race::Events::load(&assets);
        let (mut total, mut ok, mut tuned, mut below_stock) = (0usize, 0usize, 0usize, 0usize);
        for def in &events.races {
            let Some((lo, _)) = def.target_class.and_then(field::class_band) else { continue };
            let ctx = FieldCtx { career: &events.career, player_car: "", tier: 0, rank: 250, difficulty: 1, assets: Some(&assets) };
            let (entries, _) = field::build(def, &ctx, def.field.len());
            for e in entries {
                let Some(stock) = std::fs::read(assets.join("cars").join(&e.car).join("physics.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|p| p["car"]["PerformanceIndex"].as_f64())
                else {
                    continue;
                };
                total += 1;
                below_stock += (stock < lo as f64) as usize;
                let pi = match e.tune.and_then(|r| plan(&assets, &e.car, &r)) {
                    Some(t) => {
                        tuned += 1;
                        t.pi as f64
                    }
                    None => stock,
                };
                ok += (pi >= lo as f64 - 1e-4) as usize;
            }
        }
        if total == 0 {
            return;
        }
        let share = ok as f64 / total as f64;
        eprintln!("ai_field_pi: {total} AI entries, {below_stock} stock below their class floor, {tuned} tuned, {ok} at or above the floor after tuning ({:.1}%)", share * 100.0);
        assert!(share >= 0.95, "only {:.1}% of the AI field reaches its class floor", share * 100.0);
    }
}
