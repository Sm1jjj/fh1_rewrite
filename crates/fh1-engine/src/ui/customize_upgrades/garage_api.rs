//! Garage API (P17, docs/CUSTOMIZE.md "Garage API"): what the garage / customize menus (ui/customize.rs, the card menus)
//! need from the upgrade, kit, rim, PI and wallet logic, without owning any of it.
//!
//! - Read: [`GarageApi`] (a read-only SystemParam): the part catalog per category, kit slots, rims, each option with its
//!   price (ownership applied), owned / installed flags, and the build's PI / class / ratings / specs ([`Eval`], computed
//!   off-thread and cached: `None` = still computing, ask again next frame).
//! - Act: send a [`GarageAction`] message; the answer is a [`GarageResult`]. Install / commit charge the wallet
//!   (progression::wallet, `FH1_OWNERSHIP=0` = free), record the bought rows as owned, save garage.json, and rebuild the
//!   driven car (physics in place, then the body) when its build changed. Previews are free and stack until
//!   `PreviewEnd` / an install / a commit.
//!
//! Rules (same as customize.rs did them): one boost system at a time; an engine swap drops the old engine's part
//! choices; a kit row also goes into `upgrades` (table -> row Id) for the physics; a rim style goes into `upgrades` as
//! [`RIM_TABLE`] (List_Wheels.ID) for its mass; bought rows stay owned (refit free, no refunds: INFERRED for FH1).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use serde_json::Value;

use super::{catalog, effective_doc, level_name, purchase_price, record_owned, OwnedParts, KIT_SLOTS, RIM_TABLE, SWAP_TABLE};
use crate::ui::customize::{CarLook, CarLooks, Paint};

// ---------------------------------------------------------------- types

/// One option of a part: (gamedb table, row Id). Row -1 = "None" (a car without the part, e.g. no turbo).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PartId {
    pub table: String,
    pub row: i64,
}

#[derive(Clone, Debug)]
pub struct PartCard {
    pub id: PartId,
    pub name: String,
    /// gamedb Level (-1 = None).
    pub level: i64,
    /// "Stock", "Street", "Sport", "Race" or "None".
    pub level_label: &'static str,
    /// Short effect against stock ("+8% torque · -12 kg").
    pub effect: String,
    /// gamedb Price (credits).
    pub list_price: u32,
    /// What installing it costs now: 0 for stock / None / owned.
    pub price: u32,
    pub owned: bool,
    pub installed: bool,
    pub stock: bool,
    /// Kit options: this renderer has a model for it (remaster: the setup `variants` export; the physics applies anyway).
    pub shows: bool,
}

#[derive(Clone, Debug)]
pub struct PartSlot {
    pub table: &'static str,
    pub name: &'static str,
    pub options: Vec<PartCard>,
}

#[derive(Clone, Debug)]
pub struct Category {
    /// "Engine", "Aspiration", "Drivetrain", "Platform", "Tyres".
    pub name: &'static str,
    pub slots: Vec<PartSlot>,
}

#[derive(Clone, Debug)]
pub struct KitSlot {
    /// garage.json `kit` key ("front_bumper", "rear_bumper", "side_skirts", "hood", "rear_wing").
    pub key: &'static str,
    pub table: &'static str,
    pub name: &'static str,
    pub options: Vec<PartCard>,
}

#[derive(Clone, Debug)]
pub struct RimCard {
    pub media: String,
    /// List_Wheels.ID.
    pub id: i64,
    pub name: String,
    pub maker: String,
    /// rims.json `type` ("street", "race", ...).
    pub kind: String,
    pub mass: f32,
    pub list_price: u32,
    pub price: u32,
    pub owned: bool,
    pub installed: bool,
}

/// The build's headline numbers (data.rs `CarData` of the patched physics).
#[derive(Clone, Copy, Debug, Default)]
pub struct Specs {
    /// Peak power (kW) at full boost, and its rpm.
    pub power_kw: f32,
    pub power_rpm: f32,
    pub torque_nm: f32,
    pub torque_rpm: f32,
    pub mass_kg: f32,
    /// Front weight share (0..1).
    pub front_weight: f32,
}

/// A build's PI (pi.rs on the patched physics; the game shows the same class / PI / ratings for an upgraded car) and
/// specs.
#[derive(Clone, Debug)]
pub struct Eval {
    pub class_letter: String,
    pub display_pi: u32,
    pub pi: f32,
    /// speed, handling, acceleration, launch, braking (3..10, the game's bars).
    pub ratings: [f32; 5],
    pub specs: Specs,
}

/// One change to a car's build.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// A performance part option ([`PartId`]); the stock row = back to stock.
    Part { table: String, row: i64 },
    /// A kit slot (`KitSlot::key`) option by row Id; None = stock.
    Kit { slot: String, row: Option<i64> },
    /// An aftermarket rim by media name; None = stock.
    Rim(Option<String>),
    /// Paint (free); None = the car's usual colour.
    Paint(Option<Paint>),
}

#[derive(Message, Clone, Debug)]
pub enum GarageAction {
    /// Show `change` on top of what the car shows now (free, not saved).
    Preview { car: String, change: Change },
    /// Back to the saved build.
    PreviewEnd { car: String },
    /// Apply `change` to the saved build: charge, save, show it (drops any preview).
    Install { car: String, change: Change },
    /// Buy and keep everything previewed.
    CommitPreview { car: String },
    /// Back to stock; bought parts stay owned and the spent total stays.
    ResetStock { car: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Done; `charged` credits for `bought` (names).
    Ok { charged: i64, bought: Vec<String> },
    NotEnoughCredits { need: i64, have: i64 },
    /// The change doesn't apply to this car (part / rim not offered, NoRimStyles, ...).
    Incompatible(String),
}

#[derive(Message, Clone, Debug)]
pub struct GarageResult {
    pub car: String,
    pub outcome: Outcome,
}

// ---------------------------------------------------------------- plugin

pub struct GarageApiPlugin;

impl Plugin for GarageApiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GarageEval>()
            .add_message::<GarageAction>()
            .add_message::<GarageResult>()
            .add_systems(Startup, migrate_rims)
            .add_systems(Update, (apply_actions, run_evals).chain());
    }
}

// ---------------------------------------------------------------- data (cached reads)

/// The car's upgrades doc ([`super::read_doc`], with the rim masses), read once per car.
fn doc(assets: &Path, car: &str) -> Arc<Value> {
    static DOCS: OnceLock<Mutex<HashMap<String, Arc<Value>>>> = OnceLock::new();
    let map = DOCS.get_or_init(Default::default);
    if let Some(d) = map.lock().ok().and_then(|m| m.get(car).cloned()) {
        return d;
    }
    let d = Arc::new(super::read_doc(assets, car));
    if let Ok(mut m) = map.lock() {
        m.insert(car.to_owned(), d.clone());
    }
    d
}

/// `upgrades/rims.json`, read once.
fn rims_json(assets: &Path) -> &'static Value {
    static V: OnceLock<Value> = OnceLock::new();
    V.get_or_init(|| std::fs::read(assets.join("upgrades/rims.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
}

/// The car's kit.json sections (remaster variants export), lowercased; None = no export (stock kit kept), Some(None) =
/// an export without kit.json (everything requested is tried).
fn kit_sections(assets: &Path, car: &str) -> Option<Option<Vec<String>>> {
    let dir = assets.join("variants/cars").join(car);
    if !dir.join("model.gltf").is_file() {
        return None;
    }
    let k: Option<Value> = std::fs::read(dir.join("kit.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    Some(k.and_then(|k| k["sections"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_ascii_lowercase)).collect())))
}

/// gamedb maker names carry a two-letter prefix (`RO_JA Motorsports`).
fn maker(m: &str) -> String {
    let b = m.as_bytes();
    if b.len() > 3 && b[2] == b'_' && b[..2].iter().all(u8::is_ascii_uppercase) { m[3..].to_owned() } else { m.to_owned() }
}

// ---------------------------------------------------------------- pure catalog functions

/// Row Ids and rims `saved` owns: its owned lists plus what it wears (looks saved before the economy).
pub fn owned_of(assets: &Path, car: &str, saved: &CarLook) -> (OwnedParts, Vec<String>) {
    let mut ids = saved.owned_parts.clone();
    let mut rims = saved.owned_rims.clone();
    let mut add = |table: &str, id: i64| {
        let v = ids.entry(table.to_owned()).or_default();
        if id >= 0 && !v.contains(&id) {
            v.push(id);
        }
    };
    let d = doc(assets, car);
    for (key, table, ..) in KIT_SLOTS {
        if let Some(&seq) = saved.kit.get(key) {
            if let Some(id) = kit_rows(&d, table).iter().find(|o| o.sequence == seq).map(|o| o.id) {
                add(table, id);
            }
        }
    }
    for (table, &id) in &saved.upgrades {
        if table != RIM_TABLE {
            add(table, id);
        }
    }
    if let Some(r) = &saved.rim {
        if !rims.contains(r) {
            rims.push(r.clone());
        }
    }
    (ids, rims)
}

/// The performance parts of `car` as `look` stands (an engine swap brings the new engine's rows), by category.
pub fn categories(assets: &Path, car: &str, look: &CarLook, saved: &CarLook) -> Vec<Category> {
    let d = doc(assets, car);
    let (owned, _) = owned_of(assets, car, saved);
    let mut out: Vec<Category> = Vec::new();
    for part in catalog(&effective_doc(&d, &look.upgrades)) {
        let cur = part.index(look.upgrades.get(part.table).copied());
        let options = (0..part.options.len())
            .map(|i| {
                let (row, name, effect) = &part.options[i];
                let stock = part.stock.get(i).copied().unwrap_or(false);
                let owned_row = *row >= 0 && owned.get(part.table).is_some_and(|v| v.contains(row));
                PartCard {
                    id: PartId { table: part.table.to_owned(), row: *row },
                    name: name.clone(),
                    level: part.levels.get(i).copied().unwrap_or(0),
                    level_label: if *row < 0 { "None" } else if stock { "Stock" } else { level_name(part.levels.get(i).copied().unwrap_or(1)) },
                    effect: effect.clone(),
                    list_price: part.prices.get(i).copied().unwrap_or(0),
                    price: purchase_price(&part, i, &owned).unwrap_or(0),
                    owned: owned_row || stock,
                    installed: i == cur,
                    stock,
                    shows: true,
                }
            })
            .collect();
        let slot = PartSlot { table: part.table, name: part.name, options };
        match out.iter_mut().find(|c| c.name == part.area) {
            Some(c) => c.slots.push(slot),
            None => out.push(Category { name: part.area, slots: vec![slot] }),
        }
    }
    out
}

/// One kit row (stock first, one per Sequence).
#[derive(Clone, Copy, Debug)]
struct KitRow {
    id: i64,
    sequence: u32,
    level: i64,
    stock: bool,
    price: u32,
}

fn kit_rows(d: &Value, table: &str) -> Vec<KitRow> {
    let mut v: Vec<KitRow> = d["parts"][table]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    Some(KitRow {
                        id: r["Id"].as_i64().unwrap_or(-1),
                        sequence: r["Sequence"].as_u64()? as u32,
                        level: r["Level"].as_i64().unwrap_or(0),
                        stock: r["IsStock"].as_i64() == Some(1),
                        price: r["Price"].as_u64().unwrap_or(0) as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|o| (!o.stock, o.sequence));
    v.dedup_by_key(|o| o.sequence);
    v
}

/// The kit slots with more than one option.
pub fn kits(assets: &Path, car: &str, look: &CarLook, saved: &CarLook) -> Vec<KitSlot> {
    let d = doc(assets, car);
    let (owned, _) = owned_of(assets, car, saved);
    let remaster = fh1_remaster::enabled();
    let sections = if remaster { kit_sections(assets, car) } else { None };
    KIT_SLOTS
        .iter()
        .filter_map(|&(key, table, name, stems)| {
            let rows = kit_rows(&d, table);
            if rows.len() < 2 {
                return None;
            }
            let stock_seq = rows.iter().find(|o| o.stock).map_or(0, |o| o.sequence);
            let cur = look.kit.get(key).copied().unwrap_or(stock_seq);
            let options = rows
                .iter()
                .map(|o| {
                    let race = !o.stock && o.level >= 3;
                    let section = if race { format!("{}race", stems[0]) } else { format!("{}{}", stems[0], (b'a' + o.sequence.min(25) as u8) as char) };
                    let shows = o.stock
                        || !remaster
                        || match &sections {
                            None => false,
                            Some(None) => true,
                            Some(Some(have)) => have.iter().any(|n| n.strip_prefix(section.as_str()).is_some_and(|r| r.is_empty() || r.starts_with('_'))),
                        };
                    let owned_row = owned.get(table).is_some_and(|v| v.contains(&o.id));
                    PartCard {
                        id: PartId { table: table.to_owned(), row: o.id },
                        name: if o.stock { "Stock".into() } else if race { "Race".into() } else { format!("Option {}", (b'A' + o.sequence.min(25) as u8) as char) },
                        level: o.level,
                        level_label: if o.stock { "Stock" } else { level_name(o.level) },
                        effect: String::new(),
                        list_price: if o.stock { 0 } else { o.price },
                        price: if o.stock || owned_row || o.id < 0 { 0 } else { o.price },
                        owned: o.stock || owned_row,
                        installed: o.sequence == cur,
                        stock: o.stock,
                        shows,
                    }
                })
                .collect();
            Some(KitSlot { key, table, name, options })
        })
        .collect()
}

/// The aftermarket rims this renderer can draw (empty for NoRimStyles cars).
pub fn rims(assets: &Path, car: &str, look: &CarLook, saved: &CarLook) -> Vec<RimCard> {
    if doc(assets, car)["no_rim_styles"].as_bool().unwrap_or(false) {
        return Vec::new();
    }
    let (_, owned) = owned_of(assets, car, saved);
    let remaster = fh1_remaster::enabled();
    rims_json(assets)
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|r| !r["exception"].as_bool().unwrap_or(false))
                .filter_map(|r| {
                    let media = r["media_name"].as_str()?.to_owned();
                    let drawable = if remaster { assets.join("variants/wheels").join(&media).join("rim.json").is_file() } else { assets.join("cars/wheels").join(&media).is_dir() };
                    if !drawable {
                        return None;
                    }
                    let list_price = r["price"].as_u64().unwrap_or(0) as u32;
                    let is_owned = owned.contains(&media);
                    Some(RimCard {
                        id: r["id"].as_i64().unwrap_or(-1),
                        name: r["name"].as_str().unwrap_or(&media).to_owned(),
                        maker: maker(r["maker"].as_str().unwrap_or("")),
                        kind: r["type"].as_str().unwrap_or("").to_owned(),
                        mass: r["mass"].as_f64().unwrap_or(0.0) as f32,
                        list_price,
                        price: if is_owned { 0 } else { list_price },
                        owned: is_owned,
                        installed: look.rim.as_deref() == Some(media.as_str()),
                        media,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `look` with `change` applied (the menus' rules); Err = the change doesn't apply to this car.
pub fn with_change(assets: &Path, car: &str, look: &CarLook, change: &Change) -> Result<CarLook, String> {
    let mut look = look.clone();
    match change {
        Change::Part { table, row } => {
            let d = doc(assets, car);
            let parts = catalog(&effective_doc(&d, &look.upgrades));
            let part = parts.iter().find(|p| p.table == table.as_str()).ok_or_else(|| format!("{table}: not offered on {car}"))?;
            let i = part.options.iter().position(|o| o.0 == *row).ok_or_else(|| format!("{table}: no row {row} on {car}"))?;
            if i == 0 || part.stock.get(i).copied().unwrap_or(false) {
                look.upgrades.remove(part.table);
            } else {
                look.upgrades.insert(part.table.to_owned(), *row);
                // One boost system at a time: a turbo / supercharger choice drops the others.
                let boost = |t: &str| t.contains("Turbo") || t.ends_with("CSC") || t.ends_with("DSC");
                if boost(part.table) {
                    look.upgrades.retain(|t, _| t == part.table || !boost(t));
                }
            }
            // Engine swap: the old engine's part choices no longer apply.
            if part.table == SWAP_TABLE {
                look.upgrades.retain(|t, _| t == SWAP_TABLE || !t.starts_with("List_UpgradeEngine"));
            }
        }
        Change::Kit { slot, row } => {
            let &(key, table, ..) = KIT_SLOTS.iter().find(|s| s.0 == slot.as_str()).ok_or_else(|| format!("no kit slot {slot}"))?;
            let rows = kit_rows(&doc(assets, car), table);
            match row {
                None => {
                    look.kit.remove(key);
                    look.upgrades.remove(table);
                }
                Some(id) => {
                    let o = rows.iter().find(|o| o.id == *id).ok_or_else(|| format!("{slot}: no row {id} on {car}"))?;
                    if o.stock {
                        look.kit.remove(key);
                        look.upgrades.remove(table);
                    } else {
                        look.kit.insert(key.to_owned(), o.sequence);
                        if o.id >= 0 {
                            look.upgrades.insert(table.to_owned(), o.id);
                        }
                    }
                }
            }
        }
        Change::Rim(None) => {
            look.rim = None;
            look.upgrades.remove(RIM_TABLE);
        }
        Change::Rim(Some(media)) => {
            let saved = look.clone();
            let r = rims(assets, car, &look, &saved).into_iter().find(|r| &r.media == media).ok_or_else(|| format!("rim {media}: not available on {car}"))?;
            look.rim = Some(r.media);
            look.upgrades.insert(RIM_TABLE.to_owned(), r.id);
        }
        Change::Paint(p) => look.paint = *p,
    }
    Ok(look)
}

/// Credits `look` costs over `saved` (paint is free) and the bought items' names; records them as owned in `look`.
pub fn charge(assets: &Path, car: &str, saved: &CarLook, look: &mut CarLook) -> (i64, Vec<String>) {
    let (ids, rims_owned) = owned_of(assets, car, saved);
    look.owned_parts = ids;
    look.owned_rims = rims_owned;
    let mut cost = 0i64;
    let mut items = Vec::new();
    if let Some(r) = look.rim.clone() {
        if !look.owned_rims.contains(&r) {
            if let Some(rim) = rims_json(assets).as_array().and_then(|a| a.iter().find(|x| x["media_name"].as_str() == Some(r.as_str()))) {
                cost += rim["price"].as_i64().unwrap_or(0);
                items.push(rim["name"].as_str().unwrap_or(&r).to_owned());
            }
            look.owned_rims.push(r);
        }
    }
    let d = doc(assets, car);
    for (key, table, name, _) in KIT_SLOTS {
        let Some(&seq) = look.kit.get(key) else { continue };
        let Some(o) = kit_rows(&d, table).into_iter().find(|o| o.sequence == seq) else { continue };
        if !o.stock && o.id >= 0 && !look.owned_parts.get(table).is_some_and(|v| v.contains(&o.id)) {
            cost += o.price as i64;
            items.push(name.to_owned());
            look.owned_parts.entry(table.to_owned()).or_default().push(o.id);
        }
    }
    let parts = catalog(&effective_doc(&d, &look.upgrades));
    let chosen: Vec<(String, i64)> = look.upgrades.iter().map(|(t, &id)| (t.clone(), id)).collect();
    for (table, id) in chosen {
        let Some(part) = parts.iter().find(|p| p.table == table) else { continue };
        let i = part.index(Some(id));
        if let Some(c) = purchase_price(part, i, &look.owned_parts) {
            cost += c as i64;
            items.push(part.name.to_owned());
        }
        record_owned(part, i, &mut look.owned_parts);
    }
    (cost, items)
}

// ---------------------------------------------------------------- evaluation (PI + specs, off-thread)

type EvalKey = (String, BTreeMap<String, i64>);

/// Cached build evaluations; requests come from [`GarageApi`] (read-only), tasks run on the async pool.
#[derive(Resource, Default)]
pub struct GarageEval {
    done: Mutex<HashMap<EvalKey, Option<Eval>>>,
    wanted: Mutex<Vec<EvalKey>>,
    tasks: Vec<(EvalKey, Task<Option<Eval>>)>,
}

/// Evaluations in flight at once.
const MAX_TASKS: usize = 4;

impl GarageEval {
    /// The evaluation of `car` with `upgrades`; None = computing (queued) or failed.
    pub fn get(&self, car: &str, upgrades: &BTreeMap<String, i64>) -> Option<Eval> {
        let key = (car.to_owned(), upgrades.clone());
        if let Some(e) = self.done.lock().ok()?.get(&key) {
            return e.clone();
        }
        if let Ok(mut w) = self.wanted.lock() {
            if !w.contains(&key) {
                w.push(key);
            }
        }
        None
    }
}

fn evaluate(assets: PathBuf, car: String, chosen: BTreeMap<String, i64>) -> Option<Eval> {
    let dir = assets.join("cars").join(&car);
    let pi = super::pi_preview(&assets, &dir, &car, &chosen)?;
    let data = crate::data::CarData::load_with(&dir, |p| {
        if super::enabled() && !chosen.is_empty() {
            super::patch(p, &super::read_doc(&assets, &car), &chosen);
        }
    })
    .map_err(|e| warn!("{car}: garage eval: {e:#}"))
    .ok()?;
    let top = data.rev_limit_rpm.max(data.redline_rpm).max(1500.0);
    let mut s = Specs { mass_kg: data.mass, front_weight: data.front_weight, ..default() };
    for i in 0..=240 {
        let rpm = 800.0 + (top - 800.0) * i as f32 / 240.0;
        let t = data.boosted_torque_at(rpm);
        let kw = t * rpm * std::f32::consts::TAU / 60.0 / 1000.0;
        if t > s.torque_nm {
            (s.torque_nm, s.torque_rpm) = (t, rpm);
        }
        if kw > s.power_kw {
            (s.power_kw, s.power_rpm) = (kw, rpm);
        }
    }
    Some(Eval { class_letter: pi.class_letter, display_pi: pi.display_pi, pi: pi.pi, ratings: pi.ratings, specs: s })
}

fn run_evals(mut eval: ResMut<GarageEval>, garage: Res<crate::Garage>) {
    let eval = &mut *eval;
    eval.tasks.retain_mut(|(key, task)| match bevy::tasks::block_on(bevy::tasks::futures_lite::future::poll_once(task)) {
        Some(r) => {
            if let Ok(mut d) = eval.done.lock() {
                d.insert(key.clone(), r);
            }
            false
        }
        None => true,
    });
    let Ok(mut wanted) = eval.wanted.lock() else { return };
    while eval.tasks.len() < MAX_TASKS && !wanted.is_empty() {
        // Newest first: the card the player is looking at now.
        let key = wanted.pop().unwrap();
        if eval.done.lock().is_ok_and(|d| d.contains_key(&key)) || eval.tasks.iter().any(|(k, _)| *k == key) {
            continue;
        }
        let (assets, car, chosen) = (garage.assets.clone(), key.0.clone(), key.1.clone());
        eval.tasks.push((key, AsyncComputeTaskPool::get().spawn(async move { evaluate(assets, car, chosen) })));
    }
}

// ---------------------------------------------------------------- read API

/// Read-only garage view for the menus.
#[derive(SystemParam)]
pub struct GarageApi<'w> {
    looks: Res<'w, CarLooks>,
    eval: Res<'w, GarageEval>,
    garage: Res<'w, crate::Garage>,
    profile: Option<Res<'w, crate::progression::Profile>>,
}

impl GarageApi<'_> {
    /// The driven car's media name (the car the API respawns).
    pub fn driven(&self) -> &str {
        &self.garage.cars[self.garage.current]
    }

    fn assets(&self) -> &Path {
        &self.garage.assets
    }

    /// What `car` shows: the preview, else the saved look.
    pub fn look(&self, car: &str) -> CarLook {
        self.looks.get(car).cloned().unwrap_or_default()
    }

    fn saved(&self, car: &str) -> CarLook {
        self.looks.saved(car).cloned().unwrap_or_default()
    }

    pub fn categories(&self, car: &str) -> Vec<Category> {
        categories(self.assets(), car, &self.look(car), &self.saved(car))
    }

    pub fn kits(&self, car: &str) -> Vec<KitSlot> {
        kits(self.assets(), car, &self.look(car), &self.saved(car))
    }

    pub fn rims(&self, car: &str) -> Vec<RimCard> {
        rims(self.assets(), car, &self.look(car), &self.saved(car))
    }

    pub fn with_change(&self, car: &str, change: &Change) -> Result<CarLook, String> {
        with_change(self.assets(), car, &self.look(car), change)
    }

    /// The shown build's evaluation (None = computing).
    pub fn current(&self, car: &str) -> Option<Eval> {
        self.eval.get(car, &self.look(car).upgrades)
    }

    /// The stock car's evaluation (Autoshow cards).
    pub fn stock(&self, car: &str) -> Option<Eval> {
        self.eval.get(car, &BTreeMap::new())
    }

    /// The evaluation with `change` on top of the shown build (None = computing or not applicable).
    pub fn after(&self, car: &str, change: &Change) -> Option<Eval> {
        let look = self.with_change(car, change).ok()?;
        self.eval.get(car, &look.upgrades)
    }

    pub fn credits(&self) -> Option<i64> {
        self.profile.as_deref().map(crate::progression::wallet::credits)
    }
}

// ---------------------------------------------------------------- actions

/// Old saves: a rim without its List_Wheels.ID in `upgrades` (mass not in the physics) gets it once.
fn migrate_rims(mut looks: ResMut<CarLooks>) {
    let assets = looks.assets().to_path_buf();
    let fix: Vec<(String, CarLook)> = looks
        .saved_cars()
        .filter(|(_, l)| l.rim.is_some() && !l.upgrades.contains_key(RIM_TABLE))
        .filter_map(|(car, l)| {
            let media = l.rim.as_deref()?;
            let id = rims_json(&assets).as_array()?.iter().find(|r| r["media_name"].as_str() == Some(media))?["id"].as_i64()?;
            let mut l = l.clone();
            l.upgrades.insert(RIM_TABLE.to_owned(), id);
            Some((car.clone(), l))
        })
        .collect();
    for (car, l) in fix {
        looks.commit(&car, l);
    }
}

/// What the body shows (paint, rim, kit) differs.
fn visual_differs(a: &CarLook, b: &CarLook) -> bool {
    a.paint != b.paint || a.rim != b.rim || a.kit != b.kit
}

#[allow(clippy::too_many_arguments)]
fn apply_actions(
    mut actions: MessageReader<GarageAction>,
    mut results: MessageWriter<GarageResult>,
    mut commands: Commands,
    mut looks: ResMut<CarLooks>,
    garage: Res<crate::Garage>,
    track: Res<crate::track::Track>,
    asset_server: Res<AssetServer>,
    mut cars: Query<(Entity, &mut crate::Car), With<fh1_engine::ai::PlayerCar>>,
    bodies: Query<(Entity, &ChildOf), With<crate::CarModel>>,
    mut profile: Option<ResMut<crate::progression::Profile>>,
) {
    if actions.is_empty() {
        return;
    }
    let driven = garage.cars[garage.current].clone();
    let before = looks.get(&driven).cloned().unwrap_or_default();
    let assets = garage.assets.clone();
    for action in actions.read() {
        let (car, outcome) = match action.clone() {
            GarageAction::Preview { car, change } => {
                let current = looks.get(&car).cloned().unwrap_or_default();
                let o = match with_change(&assets, &car, &current, &change) {
                    Ok(look) => {
                        looks.set_preview(&car, Some(look));
                        Outcome::Ok { charged: 0, bought: Vec::new() }
                    }
                    Err(e) => Outcome::Incompatible(e),
                };
                (car, o)
            }
            GarageAction::PreviewEnd { car } => {
                if looks.preview_car() == Some(car.as_str()) {
                    looks.set_preview(&car, None);
                }
                (car, Outcome::Ok { charged: 0, bought: Vec::new() })
            }
            GarageAction::Install { car, change } => {
                let saved = looks.saved(&car).cloned().unwrap_or_default();
                let o = match with_change(&assets, &car, &saved, &change) {
                    Ok(look) => buy_and_keep(&assets, &car, &saved, look, &mut looks, profile.as_deref_mut()),
                    Err(e) => Outcome::Incompatible(e),
                };
                (car, o)
            }
            GarageAction::CommitPreview { car } => {
                let saved = looks.saved(&car).cloned().unwrap_or_default();
                let o = match looks.preview_car() == Some(car.as_str()) {
                    true => {
                        let look = looks.get(&car).cloned().unwrap_or_default();
                        buy_and_keep(&assets, &car, &saved, look, &mut looks, profile.as_deref_mut())
                    }
                    false => Outcome::Ok { charged: 0, bought: Vec::new() },
                };
                (car, o)
            }
            GarageAction::ResetStock { car } => {
                let old = looks.saved(&car).cloned().unwrap_or_default();
                if looks.preview_car() == Some(car.as_str()) {
                    looks.set_preview(&car, None);
                }
                // Bought parts stay owned (switching back to them is free).
                let (owned_parts, owned_rims) = owned_of(&assets, &car, &old);
                looks.commit(&car, CarLook { owned_parts, owned_rims, spent: old.spent, ..default() });
                (car, Outcome::Ok { charged: 0, bought: Vec::new() })
            }
        };
        results.write(GarageResult { car, outcome });
    }
    // The driven car: rebuild what changed (physics in place, same spot and heading; then the body).
    let after = looks.get(&driven).cloned().unwrap_or_default();
    let mut respawn = visual_differs(&before, &after);
    if after.upgrades != before.upgrades && super::enabled() {
        let dir = assets.join("cars").join(&driven);
        for (_, mut car) in &mut cars {
            match crate::data::CarData::load_with(&dir, |p| looks.patch_physics(&driven, p)) {
                Ok(data) => {
                    let ground = car.0.position - Vec3::Y * car.0.data.cg_height.max(0.2);
                    let yaw = car.0.yaw();
                    info!("{driven}: build {:?}: {:.0} kg, {:.0} N·m", after.upgrades, data.mass, data.torque_scale);
                    car.0.data = data;
                    car.0.place(ground, yaw);
                    respawn = true;
                }
                Err(e) => warn!("{driven}: upgrades: {e:#}"),
            }
        }
    }
    if !respawn {
        return;
    }
    for (root, car) in &cars {
        for (body, parent) in &bodies {
            if parent.parent() == root {
                commands.entity(body).despawn();
            }
        }
        let cg = car.0.cg_model;
        commands.entity(root).with_children(|p| crate::spawn_body(p, &garage, &track.id, &driven, cg, &asset_server, &looks));
    }
}

/// Charge `look` over `saved`, then keep it (save) and drop the preview; short of credits nothing changes.
fn buy_and_keep(assets: &Path, car: &str, saved: &CarLook, mut look: CarLook, looks: &mut CarLooks, profile: Option<&mut crate::progression::Profile>) -> Outcome {
    let (cost, items) = charge(assets, car, saved, &mut look);
    if cost > 0 && crate::progression::wallet::ownership_on() {
        if let Some(p) = profile {
            let have = crate::progression::wallet::credits(p);
            if !crate::progression::wallet::spend(p, cost, &format!("{car}: {}", items.join(", "))) {
                return Outcome::NotEnoughCredits { need: cost, have };
            }
            look.spent += cost;
        }
    }
    if looks.preview_car() == Some(car) {
        looks.set_preview(car, None);
    }
    looks.commit(car, look);
    Outcome::Ok { charged: cost, bought: items }
}

// ---------------------------------------------------------------- looks on the wire (multiplayer, net.rs)

/// The race-section letter (fh1-remaster car.rs `RACE`, fh1-render car.rs `RACE_LETTER`).
const RACE: char = fh1_remaster::car::RACE;

/// What other players need to draw `look` on `car`: (rim media name or "", kit Sequence per [`KIT_SLOTS`] slot or
/// `fh1_net::KIT_STOCK`, roll cage fitted).
pub fn wire_look(assets: &Path, car: &str, look: &CarLook) -> (String, [u8; fh1_net::KIT_SLOTS], bool) {
    let mut kit = [fh1_net::KIT_STOCK; fh1_net::KIT_SLOTS];
    for (i, (key, ..)) in KIT_SLOTS.iter().enumerate().take(fh1_net::KIT_SLOTS) {
        if let Some(&seq) = look.kit.get(*key) {
            kit[i] = seq.min(0xFE) as u8;
        }
    }
    (look.rim.clone().unwrap_or_default(), kit, cage_fitted(assets, car, look))
}

/// The race weight reduction (List_UpgradeCarBodyWeight Level 3) brings the roll cage (customize.rs, INFERRED).
fn cage_fitted(assets: &Path, car: &str, look: &CarLook) -> bool {
    const WEIGHT: &str = "List_UpgradeCarBodyWeight";
    look.upgrades.get(WEIGHT).is_some_and(|&id| {
        doc(assets, car)["parts"][WEIGHT].as_array().is_some_and(|rows| rows.iter().any(|r| r["Id"].as_i64() == Some(id) && r["Level"].as_i64().unwrap_or(0) >= 3))
    })
}

/// A peer's looks as a [`CarLook`] for `car`, keeping only what this install has: a rim listed in its `rims.json`, kit
/// Sequences that are rows of the car's kit tables (a peer's strings never become arbitrary asset paths). Returns the
/// look and whether the cage is fitted.
pub fn look_from_wire(assets: &Path, car: &str, paint: Option<Paint>, rim: &str, kit: &[u8; fh1_net::KIT_SLOTS], flags: u8) -> (CarLook, bool) {
    let mut look = CarLook { paint, ..default() };
    if !rim.is_empty() && rims_json(assets).as_array().is_some_and(|a| a.iter().any(|r| r["media_name"].as_str() == Some(rim) && !r["exception"].as_bool().unwrap_or(false))) {
        look.rim = Some(rim.to_owned());
    }
    let d = doc(assets, car);
    for (i, &(key, table, ..)) in KIT_SLOTS.iter().enumerate().take(fh1_net::KIT_SLOTS) {
        let seq = kit[i];
        if seq != fh1_net::KIT_STOCK && kit_rows(&d, table).iter().any(|o| o.sequence == seq as u32 && !o.stock) {
            look.kit.insert(key.to_owned(), seq as u32);
        }
    }
    (look, flags & fh1_net::LOOK_CAGE != 0)
}

/// Put `look` on a car body entity (paint, rim, kit, cage) the way customize.rs `CarLooks::apply` does for the player:
/// every kit stem gets a letter (the chosen row's, else the stock row's), race rows the race section, a race front
/// bumper also its rear half when the rear has no row chosen.
pub fn apply_look(assets: &Path, car: &str, look: &CarLook, cage: bool, body: &mut EntityCommands) {
    match look.paint {
        Some(Paint::Factory { sequence }) => {
            body.insert(fh1_render::car::FxCarPaint { sequence });
        }
        Some(Paint::Custom { rgb, metallic }) => {
            body.insert(fh1_render::car::FxCarPaintRgb { rgb, metallic });
        }
        None => {}
    }
    if let Some(rim) = &look.rim {
        body.insert(fh1_render::car::FxCarRim(rim.clone()));
    }
    if look.kit.is_empty() && !cage {
        return;
    }
    let d = doc(assets, car);
    let mut letters = Vec::new();
    let mut race_front = false;
    for &(key, table, _, stems) in KIT_SLOTS.iter() {
        let rows = kit_rows(&d, table);
        let stock = rows.iter().find(|o| o.stock).map_or(0, |o| o.sequence);
        let seq = look.kit.get(key).copied().unwrap_or(stock);
        let race = rows.iter().any(|o| o.sequence == seq && !o.stock && o.level >= 3);
        race_front |= race && key == "front_bumper";
        let letter = if race { RACE } else { (b'a' + seq.min(25) as u8) as char };
        letters.extend(stems.iter().map(|s| (s.to_string(), letter)));
    }
    if race_front && !look.kit.contains_key("rear_bumper") {
        letters.retain(|(s, _)| s != "bumperr");
        letters.push(("bumperr".into(), RACE));
    }
    if cage {
        letters.push(("cage".into(), RACE));
    }
    body.insert(fh1_render::car::FxCarKit(fh1_render::car::StockKit(letters)));
}
