//! Customize > Upgrades (docs/CUSTOMIZE.md "Upgrades"): the car's gamedb upgrade rows (setup group `upgrades`,
//! `upgrades/cars/<car>.json`) and the edit of `physics.json` they make before data.rs `CarData::load_with` reads it.
//!
//! Rules (docs/CUSTOMIZE.md "Upgrade rules (VERIFIED from default.xex)"): a chosen row is FITTED, i.e. it replaces the
//! car's `stock_parts[table]` with its absolute values; data.rs (`upgrade_rules_on`) then does what the game does with
//! the fitted set: MassDiff / WeightDistDiff summed, DragScale multiplied, TorqueScale additive (S = 1 + sum(x - 1) + the
//! boost system's RobScale - 1), intercooler added to the boost maximum, one aspiration (turbo > CSC > DSC > manifold),
//! tyre width / rim size keeping the stock outer diameter, ChassisStiffness friction scales. Here only what replaces whole
//! blocks: the camshaft's torque curve, spring / damper and anti-roll bar rows, the tyre compound.
//! `FH1_UPG_RULES=0` = the old INFERRED reading (chosen / stock ratios applied here).
//! Not offered yet: engine swaps (need the new engine's part set exported), front bumper / rear wing aero elements (the
//! sim has no aero elements yet; body-kit slots are 15's customize.rs), sell-back.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// Offered parts: (area, gamedb table, label).
const PARTS: [(&str, &str, &str); 30] = [
    ("Engine", "List_UpgradeEngine", "Engine swap"),
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
    ("Drivetrain", "List_UpgradeDrivetrainDriveline", "Driveline"),
    ("Platform", "List_UpgradeSpringDamper", "Springs and dampers"),
    ("Platform", "List_UpgradeBrakes", "Brakes"),
    ("Platform", "List_UpgradeCarBodyWeight", "Weight reduction"),
    ("Platform", "List_UpgradeCarBodyChassisStiffness", "Chassis reinforcement"),
    ("Tyres", "List_UpgradeTireCompound", "Tyre compound"),
    ("Tyres", "List_UpgradeCarBodyTireWidthFront", "Front tyre width"),
    ("Tyres", "List_UpgradeCarBodyTireWidthRear", "Rear tyre width"),
    ("Tyres", "List_UpgradeRimSizeFront", "Front rim size"),
    ("Tyres", "List_UpgradeRimSizeRear", "Rear rim size"),
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

/// The upgrades doc as the car stands with `chosen`: after an engine swap, the engine-part tables are the swapped
/// engine's (`_engine_parts`, setup upgrades-3); rows of the old engine no longer match and are not fitted (the game's
/// "Ordinal doesn't match"), so its upgrades drop out.
pub fn effective_doc(doc: &Value, chosen: &BTreeMap<String, i64>) -> Value {
    let Some(&id) = chosen.get(SWAP_TABLE).filter(|&&id| id >= 0) else { return doc.clone() };
    let Some(row) = rows(doc, SWAP_TABLE).iter().find(|r| r["Id"].as_i64() == Some(id)) else { return doc.clone() };
    let Some(set) = row["_engine_parts"].as_object() else { return doc.clone() };
    let mut d = doc.clone();
    if let Some(parts) = d["parts"].as_object_mut() {
        parts.retain(|k, _| !is_engine_part(k));
        for (t, l) in set {
            parts.insert(t.clone(), l.clone());
        }
    }
    d
}

const SWAP_TABLE: &str = "List_UpgradeEngine";

/// Engine-keyed part tables (EngineID; everything `List_UpgradeEngine*` but the swap table itself).
fn is_engine_part(table: &str) -> bool {
    table.starts_with("List_UpgradeEngine") && table != SWAP_TABLE
}

/// The parts this car can change (more than one row), in menu order. Pass [`effective_doc`] when the car may carry an
/// engine swap.
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
                let name = if is_stock {
                    "Stock".to_owned()
                } else if let Some(e) = r["swap_engine"]["EngineName"].as_str() {
                    // "I4T    -    Scirocco R" -> "I4T - Scirocco R"
                    e.split_whitespace().collect::<Vec<_>>().join(" ")
                } else {
                    level_name(r["Level"].as_i64().unwrap_or(1)).to_owned()
                };
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
    for k in ["FrontTireWidth", "RearTireWidth"] {
        if let (Some(a), Some(b)) = (num(r, k), s(k)) {
            if (a - b).abs() >= 1.0 {
                out.push(format!("{a:.0} mm"));
            }
        }
    }
    for k in ["FrontWheelDiameter", "RearWheelDiameter"] {
        if let (Some(a), Some(b)) = (num(r, k), s(k)) {
            if (a - b).abs() >= 0.5 {
                out.push(format!("{a:.0} in"));
            }
        }
    }
    if let (Some(a), Some(b)) = (num(r, "FrontLatFrictionScale"), s("FrontLatFrictionScale")) {
        let pct = (a / b.max(1e-6) - 1.0) * 100.0;
        if pct.abs() >= 0.5 {
            out.push(format!("{pct:+.0}% grip"));
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

/// The verified path: fit the chosen rows (absolute values; data.rs does the sums, S, boost and tyre sizes).
fn fit(p: &mut Value, doc: &Value, chosen: &BTreeMap<String, i64>) {
    // Engine swap first (82BEEF98): Data_Engine and the whole engine-part set become the new engine's stock rows (its
    // camshaft's torque curve with them); the chosen engine-part rows below then come from the swapped set.
    let doc = &effective_doc(doc, chosen);
    if let Some(row) = chosen.get(SWAP_TABLE).filter(|&&id| id >= 0).and_then(|&id| rows(doc, SWAP_TABLE).iter().find(|r| r["Id"].as_i64() == Some(id))) {
        if row["IsStock"].as_i64() != Some(1) && row["_engine_parts"].is_object() {
            if let Some(parts) = p["stock_parts"].as_object_mut() {
                parts.retain(|k, _| !is_engine_part(k));
            }
            if let Some(set) = doc["parts"].as_object() {
                for (t, l) in set.iter().filter(|(t, _)| is_engine_part(t)) {
                    let Some(stock) = l.as_array().and_then(|l| l.iter().find(|r| r["IsStock"].as_i64() == Some(1))) else { continue };
                    let mut clean = stock.clone();
                    if let Some(o) = clean.as_object_mut() {
                        o.retain(|k, _| !k.starts_with('_'));
                    }
                    p["stock_parts"][t.as_str()] = clean;
                    if t == "List_UpgradeEngineCamshaft" && !stock["_torque_curve"].is_null() {
                        p["torque_curve"] = stock["_torque_curve"].clone();
                    }
                }
            }
            if !row["swap_engine"].is_null() {
                p["engine"] = row["swap_engine"].clone();
            }
        }
    }
    for (table, &id) in chosen {
        if table == SWAP_TABLE && id >= 0 {
            // The swap row itself (MassDiff / WeightDistDiff / DragScale join the sums).
            if let Some(row) = rows(doc, table).iter().find(|r| r["Id"].as_i64() == Some(id)) {
                let mut clean = row.clone();
                if let Some(o) = clean.as_object_mut() {
                    o.retain(|k, _| !k.starts_with('_') && k != "swap_engine");
                }
                p["stock_parts"][table.as_str()] = clean;
            }
            continue;
        }
        if id < 0 {
            if let Some(parts) = p["stock_parts"].as_object_mut() {
                parts.remove(table);
            }
            continue;
        }
        let Some(row) = rows(doc, table).iter().find(|r| r["Id"].as_i64() == Some(id)) else { continue };
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
            "List_UpgradeCarBodyFrontBumper" if !row["_aero"].is_null() => p["aero"]["front_bumper"] = row["_aero"].clone(),
            "List_UpgradeRearWing" if !row["_aero"].is_null() => p["aero"]["rear_wing"] = row["_aero"].clone(),
            _ => {}
        }
    }
    // A chosen boost system beats the others and the manifold (82BF4FB0's priority order); data.rs applies the same order.
    if let Some(keep) = crate::data::ASPIRATION.iter().copied().find(|t| !t.ends_with("Manifold") && chosen.get(*t).is_some_and(|&id| id >= 0)) {
        if let Some(parts) = p["stock_parts"].as_object_mut() {
            parts.retain(|k, _| k == keep || !crate::data::ASPIRATION.contains(&k.as_str()));
        }
    }
}

/// Edit `physics.json` (`p`) with the chosen rows (`chosen`: table -> row Id; -1 = remove the part).
pub fn patch(p: &mut Value, doc: &Value, chosen: &BTreeMap<String, i64>) {
    if chosen.is_empty() || doc.is_null() {
        return;
    }
    if crate::data::upgrade_rules_on() {
        fit(p, doc, chosen);
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
