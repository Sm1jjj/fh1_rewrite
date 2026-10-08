//! Customize > Upgrades (docs/CUSTOMIZE.md "Upgrades"): the car's gamedb upgrade rows (setup group `upgrades`,
//! `upgrades/cars/<car>.json`) and the edit of `physics.json` they make before data.rs `CarData::load_with` reads it.
//!
//! The rules are INFERRED (the game's upgrade code is not traced; gamedb's Sim* stats are for stock cars only):
//! - A chosen row replaces the car's `stock_parts[table]` (camshaft, transmission, differential, clutch, flywheel, brakes,
//!   weight, boost tables are read from there by data.rs).
//! - Camshaft rows carry their own torque curve (`_torque_curve`, always a different curve from stock): it replaces
//!   `torque_curve`.
//! - `TorqueScale` rows (intake, exhaust, fuel, ignition, valves, displacement, pistons, manifold, oil cooling): the torque
//!   is multiplied by chosen / stock (stock rows are often already above 1, so a ratio, not a product).
//! - Intercooler `MaxScaleScale`: the boost table's MaxScale x chosen / stock.
//! - Boost: choosing a non-stock row of one boost table (single / twin turbo, CSC, DSC) drops the other boost tables, so
//!   data.rs uses that one.
//! - `MassDiff` (every part but the weight table): Weight.Mass + chosen - stock; `WeightDistDiff`: CMBackFront + chosen -
//!   stock; `DragScale`: car.BodyAeroLongitudinalDrag x chosen / stock. data.rs scales the suspension by the mass
//!   (HANDLING_PARITY 8.11, the game's rule for stock cars), so weight reduction also softens the springs.
//! - SpringDamper rows: `suspension.front/rear` = their List_SpringDamperPhysics rows; anti-roll bars:
//!   `suspension.anti_sway_front/rear`; tyre compound: the whole `tires` object (compound + both friction curves).
//! Not offered yet: tyre width / rim size (change the wheel visuals and hub heights), engine swaps, aero parts,
//! chassis stiffness (data.rs has no use for its friction scales yet), driveline.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// Offered parts: (area, gamedb table, label).
const PARTS: [(&str, &str, &str); 23] = [
    ("Engine", "List_UpgradeEngineIntake", "Intake"),
    ("Engine", "List_UpgradeEngineFuelSystem", "Fuel system"),
    ("Engine", "List_UpgradeEngineIgnition", "Ignition"),
    ("Engine", "List_UpgradeEngineExhaust", "Exhaust"),
    ("Engine", "List_UpgradeEngineCamshaft", "Camshaft"),
    ("Engine", "List_UpgradeEngineValves", "Valves"),
    ("Engine", "List_UpgradeEngineDisplacement", "Displacement"),
    ("Engine", "List_UpgradeEnginePistonsCompression", "Pistons / compression"),
    ("Engine", "List_UpgradeEngineManifold", "Manifold"),
    ("Engine", "List_UpgradeEngineOilCooling", "Oil cooling"),
    ("Engine", "List_UpgradeEngineFlywheel", "Flywheel"),
    ("Aspiration", "List_UpgradeEngineTurboSingle", "Single turbo"),
    ("Aspiration", "List_UpgradeEngineTurboTwin", "Twin turbo"),
    ("Aspiration", "List_UpgradeEngineCSC", "Centrifugal supercharger"),
    ("Aspiration", "List_UpgradeEngineDSC", "Positive-displacement supercharger"),
    ("Aspiration", "List_UpgradeEngineIntercooler", "Intercooler"),
    ("Drivetrain", "List_UpgradeDrivetrainClutch", "Clutch"),
    ("Drivetrain", "List_UpgradeDrivetrainTransmission", "Transmission"),
    ("Drivetrain", "List_UpgradeDrivetrainDifferential", "Differential"),
    ("Platform", "List_UpgradeSpringDamper", "Springs and dampers"),
    ("Platform", "List_UpgradeBrakes", "Brakes"),
    ("Platform", "List_UpgradeCarBodyWeight", "Weight reduction"),
    ("Tyres", "List_UpgradeTireCompound", "Tyre compound"),
];
/// Anti-roll bars are two tables shown as two rows.
const BARS: [(&str, &str, &str); 2] = [("Platform", "List_UpgradeAntiSwayFront", "Front anti-roll bar"), ("Platform", "List_UpgradeAntiSwayRear", "Rear anti-roll bar")];

const BOOST: [&str; 5] = ["List_UpgradeEngineTurboSingle", "List_UpgradeEngineTurboTwin", "List_UpgradeEngineTurboQuad", "List_UpgradeEngineCSC", "List_UpgradeEngineDSC"];

/// A car's bought parts: gamedb table -> row Ids (garage.json `owned_parts`; row Ids repeat across tables).
pub type OwnedParts = BTreeMap<String, Vec<i64>>;

/// `FH1_UPGRADES=0`: saved upgrades are not applied to the physics (the menu still shows them).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_UPGRADES").map_or(true, |v| v != "0"))
}

/// One offered part and its rows (stock first).
#[derive(Clone, Debug)]
pub struct Part {
    pub table: &'static str,
    pub label: String,
    /// (row Id, option name, short effect vs stock).
    pub options: Vec<(i64, String, String)>,
    /// Shop price of each option (gamedb `Price`, credits), index-aligned with `options`; 0 for stock / "None".
    pub prices: Vec<u32>,
}

impl Part {
    /// Option index of the row `chosen` (0 = stock).
    pub fn index(&self, chosen: Option<i64>) -> usize {
        chosen.and_then(|id| self.options.iter().position(|o| o.0 == id)).unwrap_or(0)
    }

    /// What installing option `i` costs now: 0 for stock, "None" or a part the car already owns (`owned` = the car's
    /// garage.json `owned_parts`: table -> bought row Ids; row Ids repeat across tables, so they are kept per table).
    /// FH1 / FM4 keep bought parts with the car; switching back is free and nothing is refunded (INFERRED for FH1).
    pub fn cost(&self, i: usize, owned: &OwnedParts) -> u32 {
        let Some(&(id, ..)) = self.options.get(i) else { return 0 };
        if id < 0 || owned.get(self.table).is_some_and(|v| v.contains(&id)) {
            return 0;
        }
        self.prices.get(i).copied().unwrap_or(0)
    }
}

/// Buying option `i` of `part`: the price to charge (None = free: stock, "None" or owned). Call the wallet's
/// `can_afford` / `spend` with it, then [`record_owned`] on success.
pub fn purchase_price(part: &Part, i: usize, owned: &OwnedParts) -> Option<u32> {
    Some(part.cost(i, owned)).filter(|&c| c > 0)
}

/// Credits paid for the car's bought parts (`owned` = garage.json `owned_parts`): the gamedb `Price` of each owned row
/// (stock rows cost nothing). For the sell flow (progression wallet `sell_price(.., parts_value)`).
pub fn parts_value(doc: &Value, owned: &OwnedParts) -> u32 {
    owned
        .iter()
        .flat_map(|(table, ids)| rows(doc, table).iter().filter(move |r| r["Id"].as_i64().is_some_and(|id| ids.contains(&id))))
        .filter(|r| r["IsStock"].as_i64() != Some(1))
        .map(|r| num(r, "Price").unwrap_or(0.0).max(0.0) as u32)
        .sum()
}

/// [`parts_value`] for `car` from the installed `upgrades/cars/<car>.json`.
pub fn parts_value_for(assets: &Path, car: &str, owned: &OwnedParts) -> u32 {
    if owned.is_empty() {
        return 0;
    }
    parts_value(&read_doc(assets, car), owned)
}

/// Remember that the car owns option `i` of `part` (after a successful spend).
pub fn record_owned(part: &Part, i: usize, owned: &mut OwnedParts) {
    if let Some(&(id, ..)) = part.options.get(i) {
        let ids = owned.entry(part.table.to_owned()).or_default();
        if id >= 0 && !ids.contains(&id) {
            ids.push(id);
        }
    }
}

pub fn read_doc(assets: &Path, car: &str) -> Value {
    std::fs::read(assets.join("upgrades/cars").join(format!("{car}.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn rows<'a>(doc: &'a Value, table: &str) -> &'a [Value] {
    doc["parts"][table].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn stock_row<'a>(doc: &'a Value, table: &str) -> Option<&'a Value> {
    rows(doc, table).iter().find(|r| r["IsStock"].as_i64() == Some(1))
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

fn level_name(level: i64) -> &'static str {
    match level {
        ..=1 => "Street",
        2 => "Sport",
        _ => "Race",
    }
}

/// The parts this car can change (more than one row), in menu order.
pub fn catalog(doc: &Value) -> Vec<Part> {
    PARTS
        .iter()
        .chain(BARS.iter())
        .filter_map(|&(area, table, label)| {
            let rows = rows(doc, table);
            let stock = stock_row(doc, table);
            let mut options: Vec<(i64, String, String)> = Vec::new();
            let mut prices: Vec<u32> = Vec::new();
            // A car without the part (e.g. no turbo): an implicit "None" option first.
            if stock.is_none() {
                options.push((-1, "None".into(), String::new()));
                prices.push(0);
            }
            for r in rows {
                let Some(id) = r["Id"].as_i64() else { continue };
                let is_stock = r["IsStock"].as_i64() == Some(1);
                let name = if is_stock { "Stock".to_owned() } else { level_name(r["Level"].as_i64().unwrap_or(1)).to_owned() };
                // Same level twice (a few tables): keep the names apart.
                let name = if options.iter().any(|o| o.1 == name) { format!("{name} {}", options.len()) } else { name };
                options.push((id, name, effect(r, stock)));
                prices.push(if is_stock { 0 } else { num(r, "Price").unwrap_or(0.0).max(0.0) as u32 });
            }
            (options.len() > 1).then(|| Part { table, label: format!("{area} · {label}"), options, prices })
        })
        .collect()
}

/// A short description of a row against the stock row: torque %, mass, redline.
fn effect(r: &Value, stock: Option<&Value>) -> String {
    let mut out = Vec::new();
    let s = |k: &str| stock.and_then(|s| num(s, k));
    if let (Some(a), b) = (num(r, "TorqueScale"), s("TorqueScale").unwrap_or(1.0)) {
        let pct = (a / b.max(1e-6) - 1.0) * 100.0;
        if pct.abs() >= 0.5 {
            out.push(format!("{pct:+.0}% torque"));
        }
    }
    if let (Some(a), Some(b)) = (num(r, "RedlineRPM"), s("RedlineRPM")) {
        if (a - b).abs() >= 1.0 {
            out.push(format!("{:+.0} rpm", a - b));
        }
    }
    if num(r, "MaxScale").is_some() && stock.is_none() {
        out.push("adds boost".into());
    }
    let mass = match (num(r, "Mass"), s("Mass")) {
        (Some(a), Some(b)) => a - b,
        _ => num(r, "MassDiff").unwrap_or(0.0) - s("MassDiff").unwrap_or(0.0),
    };
    if mass.abs() >= 0.5 {
        out.push(format!("{mass:+.0} kg"));
    }
    out.join(" · ")
}

/// Edit `physics.json` (`p`) with the chosen rows (`chosen`: table -> row Id; -1 = remove the part).
pub fn patch(p: &mut Value, doc: &Value, chosen: &BTreeMap<String, i64>) {
    if chosen.is_empty() || doc.is_null() {
        return;
    }
    let mut torque = 1.0f64;
    let (mut mass, mut dist, mut drag, mut boost_k) = (0.0f64, 0.0f64, 1.0f64, 1.0f64);
    let mut boost_choice: Option<&str> = None;
    for (table, &id) in chosen {
        let stock = stock_row(doc, table);
        if id < 0 {
            if let Some(parts) = p["stock_parts"].as_object_mut() {
                parts.remove(table);
            }
            continue;
        }
        let Some(row) = rows(doc, table).iter().find(|r| r["Id"].as_i64() == Some(id)) else { continue };
        let s = |k: &str| stock.and_then(|s| num(s, k));
        if let Some(a) = num(row, "TorqueScale") {
            torque *= a / s("TorqueScale").unwrap_or(1.0).max(1e-6);
        }
        if let Some(a) = num(row, "MaxScaleScale") {
            boost_k *= a / s("MaxScaleScale").unwrap_or(1.0).max(1e-6);
        }
        if table != "List_UpgradeCarBodyWeight" {
            mass += num(row, "MassDiff").unwrap_or(0.0) - s("MassDiff").unwrap_or(0.0);
        }
        dist += num(row, "WeightDistDiff").unwrap_or(0.0) - s("WeightDistDiff").unwrap_or(0.0);
        if let Some(a) = num(row, "DragScale") {
            drag *= a / s("DragScale").unwrap_or(1.0).max(1e-6);
        }
        if BOOST.contains(&table.as_str()) && row["IsStock"].as_i64() != Some(1) {
            boost_choice = Some(table);
        }
        // The row itself, without the inlined physics.
        let mut clean = row.clone();
        if let Some(o) = clean.as_object_mut() {
            o.retain(|k, _| !k.starts_with('_'));
        }
        p["stock_parts"][table.as_str()] = clean;
        match table.as_str() {
            "List_UpgradeEngineCamshaft" if !row["_torque_curve"].is_null() => p["torque_curve"] = row["_torque_curve"].clone(),
            "List_UpgradeSpringDamper" => {
                if !row["_front"].is_null() {
                    p["suspension"]["front"] = row["_front"].clone();
                }
                if !row["_rear"].is_null() {
                    p["suspension"]["rear"] = row["_rear"].clone();
                }
            }
            "List_UpgradeAntiSwayFront" if !row["_physics"].is_null() => p["suspension"]["anti_sway_front"] = row["_physics"].clone(),
            "List_UpgradeAntiSwayRear" if !row["_physics"].is_null() => p["suspension"]["anti_sway_rear"] = row["_physics"].clone(),
            "List_UpgradeTireCompound" if !row["_tires"].is_null() => p["tires"] = row["_tires"].clone(),
            _ => {}
        }
    }
    if let Some(keep) = boost_choice {
        if let Some(parts) = p["stock_parts"].as_object_mut() {
            parts.retain(|k, _| k == keep || !BOOST.contains(&k.as_str()));
        }
    }
    let scale = |v: &mut Value, k: f64| {
        if let Some(x) = v.as_f64() {
            *v = serde_json::json!(x * k);
        }
    };
    scale(&mut p["torque_curve"]["torque_scale_nm"], torque);
    scale(&mut p["car"]["BodyAeroLongitudinalDrag"], drag);
    if boost_k != 1.0 {
        if let Some(t) = BOOST.iter().find(|t| p["stock_parts"][**t].get("MaxScale").is_some()) {
            scale(&mut p["stock_parts"][*t]["MaxScale"], boost_k);
        }
    }
    let w = &mut p["stock_parts"]["List_UpgradeCarBodyWeight"];
    if let Some(m) = w["Mass"].as_f64() {
        w["Mass"] = serde_json::json!((m + mass).max(100.0));
    }
    if let Some(d) = w["CMBackFront"].as_f64() {
        w["CMBackFront"] = serde_json::json!((d + dist).clamp(0.2, 0.8));
    }
}
