//! Customize (docs/CUSTOMIZE.md): the pause menu's Garage > Customize page. Paint, rims, body kit and upgrades per car,
//! previewed live on the player's car and saved in `<data>/garage.json`.
//!
//! Paint: the car's Combo_Colors rows (`physics.json` `colors`) through `fh1_render::car::FxCarPaint`, FH1's special
//! colours (`upgrades/special_colors.json`, primary colour) and a custom colour (hue / saturation / brightness, gloss or
//! metallic) through `fh1_render::car::FxCarPaintRgb` (both renderers).
//! Rims: the aftermarket rims of the `upgrades` setup group (`upgrades/rims.json`) through `fh1_render::car::FxCarRim`.
//! Body kit: per slot, the car's gamedb kit rows (`upgrades/cars/<car>.json`, letter = `a` + Sequence) through
//! `fh1_render::car::FxCarKit`. Upgrades: gamedb part rows (ui/customize_upgrades.rs) edit physics.json through
//! data.rs `CarData::load_with` (main.rs spawn_car, and in place here when they change).
//!
//! Live preview: the player car's body child (main.rs `spawn_body`) is despawned and spawned again with the look; the
//! `Car` root (physics, pose) stays. Lists preview as the cursor moves (kit slots with Left / Right); Enter keeps,
//! Back reverts.
//!
//! Shop (P10, 2026-10-08): rims (List_Wheels.Price), body kit rows (List_UpgradeCarBody*.Price) and upgrades
//! (ui/customize_upgrades.rs `purchase_price`) are paid with credits (progression::wallet) when a look is kept (Enter);
//! previews are free, and parts bought once (or worn before the economy) stay owned per car, so switching back is free.
//! Paint is free: FH1 has no paint price anywhere (gamedb List_SpecialColors / Combo_Colors). Short of credits, the
//! preview is dropped and the title says so. `FH1_OWNERSHIP=0` (wallet::ownership_on) = everything free (old).
//!
//! `FH1_GARAGE=0`: garage.json is neither read nor applied (stock looks; the page still opens).
//! `FH1_CUSTOMIZE=0`: the pause menu's Change car opens the car list directly (no Garage page).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use super::customize_upgrades as upgrades;
use super::Menu;
use crate::track::Track;
use crate::{Car, CarModel, Garage};

/// The Garage page (Change car / Customize) is in the pause menu (`FH1_CUSTOMIZE=0` = old: straight to the car list).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_CUSTOMIZE").map_or(true, |v| v != "0"))
}

fn garage_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_GARAGE").map_or(true, |v| v != "0"))
}

pub struct CustomizePlugin {
    /// `<data>/garage.json`.
    pub path: PathBuf,
}

impl Plugin for CustomizePlugin {
    fn build(&self, app: &mut App) {
        let cars = if garage_on() { read_looks(&self.path) } else { BTreeMap::new() };
        let assets = app.world().get_resource::<Garage>().map(|g| g.assets.clone()).unwrap_or_default();
        app.insert_resource(CarLooks { path: self.path.clone(), assets, cars, preview: None })
            .add_systems(Update, apply_requests.after(super::menu_input).after(super::menu_mouse).before(super::draw_menu));
    }
}

fn read_looks(path: &Path) -> BTreeMap<String, CarLook> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            warn!("garage: {}: {e}", path.display());
            BTreeMap::new()
        }),
        Err(_) => BTreeMap::new(),
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// One car's customisation (garage.json value, keyed by the car's media name). Missing = stock.
#[derive(Clone, Default, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CarLook {
    pub paint: Option<Paint>,
    /// Aftermarket rim (`cars/wheels/<MediaName>`); None = stock.
    pub rim: Option<String>,
    /// Kit slot key ([`KIT_SLOTS`]) -> gamedb Sequence; missing = the stock row.
    pub kit: BTreeMap<String, u32>,
    /// Performance parts: gamedb table -> chosen row Id; missing = stock.
    pub upgrades: BTreeMap<String, i64>,
    /// Bought parts (performance upgrades and kit rows): gamedb table -> row Ids (Ids repeat across tables), kept when
    /// switching back (ui/customize_upgrades.rs `purchase_price` / `record_owned`; swapping to stock or an owned part is
    /// free). garage.json key `owned_upgrades` (a pre-release flat `owned_parts` list is ignored).
    #[serde(rename = "owned_upgrades")]
    pub owned_parts: upgrades::OwnedParts,
    /// Bought aftermarket rims (media names) for this car.
    pub owned_rims: Vec<String>,
    /// Credits spent on this car's parts (added to its sell value, progression::wallet::sell_price).
    pub spent: i64,
}

impl CarLook {
    /// Same paint, rim and kit (what the body shows).
    fn same_visual(&self, o: &CarLook) -> bool {
        self.paint == o.paint && self.rim == o.rim && self.kit == o.kit
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Paint {
    /// The car's Combo_Colors row with this Sequence.
    Factory { sequence: u32 },
    /// Any colour (0xRRGGBB, gamma-encoded like Combo_Colors): a special colour or the custom picker.
    Custom { rgb: u32, metallic: bool },
}

/// Body kit slots: (garage.json key, gamedb table, label, carbin section stems).
const KIT_SLOTS: [(&str, &str, &str, &[&str]); 5] = [
    ("front_bumper", "List_UpgradeCarBodyFrontBumper", "Front bumper", &["bumperf"]),
    ("rear_bumper", "List_UpgradeCarBodyRearBumper", "Rear bumper", &["bumperr"]),
    ("side_skirts", "List_UpgradeCarBodySideSkirt", "Side skirts", &["skirtl", "skirtr"]),
    ("hood", "List_UpgradeCarBodyHood", "Hood", &["hood"]),
    ("rear_wing", "List_UpgradeRearWing", "Rear wing", &["wing"]),
];

/// One kit option: gamedb row Id, Sequence, Level, stock flag and Price (CR).
#[derive(Clone, Copy, Debug)]
struct KitOption {
    id: i64,
    sequence: u32,
    level: i64,
    stock: bool,
    price: u32,
}

impl KitOption {
    fn name(&self) -> String {
        if self.stock {
            return "Stock".into();
        }
        let letter = (b'A' + self.sequence.min(25) as u8) as char;
        if self.level >= 3 { format!("Option {letter} (race)") } else { format!("Option {letter}") }
    }
}

/// The kit rows of `car` per slot (index = [`KIT_SLOTS`]), stock first.
fn kit_options(assets: &Path, car: &str) -> [Vec<KitOption>; 5] {
    let doc = read_json(&assets.join("upgrades/cars").join(format!("{car}.json")));
    std::array::from_fn(|i| {
        let mut v: Vec<KitOption> = doc["parts"][KIT_SLOTS[i].1]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        Some(KitOption {
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
    })
}

/// Saved looks, plus the one being previewed in the menu.
#[derive(Resource)]
pub struct CarLooks {
    path: PathBuf,
    /// Installed assets (`upgrades/` for the kit rows).
    assets: PathBuf,
    cars: BTreeMap<String, CarLook>,
    preview: Option<(String, CarLook)>,
}

impl CarLooks {
    /// The look to draw `car` with: the menu's preview, else the saved one.
    pub fn get(&self, car: &str) -> Option<&CarLook> {
        let preview = self.preview.as_ref().filter(|p| p.0 == car).map(|p| &p.1);
        if !garage_on() {
            return preview;
        }
        preview.or_else(|| self.cars.get(car))
    }

    /// Put `car`'s look on its body entity (main.rs `spawn_body`, after FxCarBody and the default paint).
    pub fn apply(&self, body: &mut EntityCommands, car: &str) {
        let Some(look) = self.get(car) else { return };
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
        if !look.kit.is_empty() {
            // Every kit stem gets a letter: the chosen row's, else the stock row's (as fh1setup cars.rs stock_kit).
            let options = kit_options(&self.assets, car);
            let mut letters = Vec::new();
            for (i, (key, _, _, stems)) in KIT_SLOTS.iter().enumerate() {
                let stock = options[i].iter().find(|o| o.stock).map_or(0, |o| o.sequence);
                let seq = look.kit.get(*key).copied().unwrap_or(stock);
                let letter = (b'a' + seq.min(25) as u8) as char;
                letters.extend(stems.iter().map(|s| (s.to_string(), letter)));
            }
            body.insert(fh1_render::car::FxCarKit(fh1_render::car::StockKit(letters)));
        }
    }

    /// Credits spent on `car`'s parts (its sell value, ui/garage.rs).
    pub fn spent(&self, car: &str) -> i64 {
        self.cars.get(car).map_or(0, |l| l.spent)
    }

    /// Edit `car`'s physics.json with its upgrades (main.rs spawn_car's `CarData::load_with`).
    pub fn patch_physics(&self, car: &str, p: &mut serde_json::Value) {
        if !upgrades::enabled() {
            return;
        }
        let Some(look) = self.get(car).filter(|l| !l.upgrades.is_empty()) else { return };
        upgrades::patch(p, &upgrades::read_doc(&self.assets, car), &look.upgrades);
    }

    /// [`Self::patch_physics`] as an owned closure, so the car's physics load (and the upgrades doc read) can run on a
    /// task thread instead of in a system (main.rs car switch).
    pub fn physics_patcher(&self, car: &str) -> impl FnOnce(&mut serde_json::Value) + Send + 'static {
        let job = upgrades::enabled()
            .then(|| self.get(car).filter(|l| !l.upgrades.is_empty()).map(|l| (self.assets.clone(), car.to_owned(), l.upgrades.clone())))
            .flatten();
        move |p| {
            if let Some((assets, car, chosen)) = job {
                upgrades::patch(p, &upgrades::read_doc(&assets, &car), &chosen);
            }
        }
    }

    fn save(&self) {
        if !garage_on() {
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_vec_pretty(&self.cars) {
            // Written on the background writer (perf/writer.rs): a save never stalls a frame.
            Ok(b) => crate::perf::writer::replace(self.path.clone(), b),
            Err(e) => warn!("garage: {e}"),
        }
    }
}

/// A menu row (ui.rs maps it to its `Item`).
#[derive(Clone)]
pub struct Row {
    pub label: String,
    pub value: Option<String>,
    /// Shown as the value's text colour (paint swatches).
    pub swatch: Option<Color>,
    pub selectable: bool,
}

fn row(label: impl Into<String>, value: Option<String>) -> Row {
    Row { label: label.into(), value, swatch: None, selectable: true }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum View {
    #[default]
    Root,
    Paint,
    Rims,
    Kit,
    /// The custom colour editor (hue, saturation, brightness, finish).
    Custom,
    /// Performance parts (Left / Right per part).
    Upgrades,
}

/// Root rows, in order.
const ROOT_PAINT: usize = 0;
const ROOT_RIMS: usize = 1;
const ROOT_KIT: usize = 2;
const ROOT_UPGRADES: usize = 3;
const ROOT_RESET: usize = 4;

#[derive(Clone, Copy, Debug)]
enum Req {
    /// The page or a sub-view opened: read the car's data and build the rows.
    Open,
    /// Preview the paint / rim under the cursor.
    PreviewRow,
    /// Step the kit slot under the cursor by ±1 option and preview.
    KitStep(i32),
    /// Preview the custom colour editor's colour.
    PreviewCustom,
    /// Step the upgrade part under the cursor by ±1 option.
    UpgradeStep(i32),
    /// Keep the preview (Enter).
    Commit,
    /// Drop the preview (Back).
    Revert,
    /// Remove the car's look (back to stock).
    Reset,
}

/// A special colour (upgrades/special_colors.json): name, primary RGB, metallic.
#[derive(Clone, Debug)]
struct Special {
    name: String,
    rgb: u32,
    metallic: bool,
}

/// An aftermarket rim (upgrades/rims.json).
#[derive(Clone, Debug)]
struct Rim {
    media: String,
    label: String,
    /// List_Wheels.Price (CR).
    price: u32,
}

/// The page's state, held by ui.rs `Menu`.
#[derive(Default)]
pub struct CustomizeMenu {
    view: View,
    pub cursor: usize,
    root_cursor: usize,
    /// Media name of the car being customised.
    car: String,
    /// (Sequence, RGB, metallic) from physics.json `colors`.
    colours: Vec<(u32, u32, bool)>,
    /// The paint the car shows when it has no saved look (model.json stock, or main.rs's default-car colour).
    base_seq: u32,
    /// Aftermarket rims (loaded once); empty when the upgrades group isn't installed.
    rims: Vec<Rim>,
    /// gamedb CarUpgradeExceptions NoRimStyles.
    no_rims: bool,
    kit: [Vec<KitOption>; 5],
    /// Kit view row -> slot index (slots with a choice only).
    kit_rows: Vec<usize>,
    /// Special colours (loaded once).
    specials: Vec<Special>,
    /// The car's upgradable parts (ui/customize_upgrades.rs).
    parts: Vec<upgrades::Part>,
    /// Custom colour editor: hue (deg), saturation, value (0..1), metallic.
    hsv: [f32; 3],
    metallic: bool,
    pending: Vec<Req>,
    /// Built by [`apply_requests`]; ui.rs `items()` shows them.
    pub rows: Vec<Row>,
    /// Shop message for the title (bought / not enough credits); cleared when a view opens.
    notice: Option<String>,
}

/// Menu navigation (ui.rs `Nav`).
#[derive(Clone, Copy, Default)]
pub struct Nav {
    pub vertical: i32,
    pub horizontal: i32,
    pub confirm: bool,
    pub back: bool,
}

/// What ui.rs does after a step.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Stay { changed: bool },
    /// Leave the page (back to the Garage page).
    Leave,
}

impl CustomizeMenu {
    pub fn open(&mut self) {
        self.view = View::Root;
        self.cursor = 0;
        self.root_cursor = 0;
        self.rows.clear();
        self.pending.push(Req::Open);
    }

    pub fn title(&self) -> String {
        let t = self.view_title();
        match &self.notice {
            Some(n) => format!("{t}    ·    {n}"),
            None => t,
        }
    }

    fn view_title(&self) -> String {
        match self.view {
            View::Root => self.car.clone(),
            View::Paint => format!("{}  ·  paint {} / {}", self.car, self.cursor + 1, self.rows.len()),
            View::Custom => format!("{}  ·  custom colour  #{:06X}", self.car, hsv_rgb(self.hsv)),
            View::Rims => format!("{}  ·  rims {} / {}", self.car, self.cursor + 1, self.rims.len() + 1),
            View::Kit => format!("{}  ·  body kit", self.car),
            View::Upgrades => format!("{}  ·  upgrades", self.car),
        }
    }

    pub fn hint(&self) -> String {
        match self.view {
            View::Root => "Enter / A  select      Esc / B  back".into(),
            View::Paint | View::Rims => "Up / Down  preview      Enter / A  keep      Esc / B  cancel".into(),
            View::Kit | View::Custom | View::Upgrades => "Left / Right  change      Enter / A  keep      Esc / B  cancel".into(),
        }
    }

    pub fn step(&mut self, nav: Nav) -> Step {
        let n = self.rows.len().max(1);
        let mut changed = false;
        if nav.vertical != 0 {
            self.cursor = (self.cursor as i32 + nav.vertical).rem_euclid(n as i32) as usize;
            if matches!(self.view, View::Paint | View::Rims) {
                self.pending.push(Req::PreviewRow);
            }
            changed = true;
        }
        if nav.horizontal != 0 && self.view == View::Kit {
            self.pending.push(Req::KitStep(nav.horizontal.signum()));
            changed = true;
        }
        if nav.horizontal != 0 && self.view == View::Upgrades {
            self.pending.push(Req::UpgradeStep(nav.horizontal.signum()));
            changed = true;
        }
        if nav.horizontal != 0 && self.view == View::Custom {
            self.adjust_custom(nav.horizontal.signum());
            changed = true;
        }
        if nav.back {
            if self.view == View::Root {
                return Step::Leave;
            }
            self.pending.push(Req::Revert);
            self.view = View::Root;
            self.cursor = self.root_cursor;
            return Step::Stay { changed: true };
        }
        if nav.confirm {
            return self.activate(self.cursor);
        }
        Step::Stay { changed }
    }

    /// The mouse moved onto row `k` (paint and rim rows preview).
    pub fn hover(&mut self, k: usize) {
        if k < self.rows.len() && k != self.cursor {
            self.cursor = k;
            if matches!(self.view, View::Paint | View::Rims) {
                self.pending.push(Req::PreviewRow);
            }
        }
    }

    /// Enter / click on row `k`.
    pub fn activate(&mut self, k: usize) -> Step {
        if !self.rows.get(k).is_some_and(|r| r.selectable) {
            return Step::Stay { changed: false };
        }
        match self.view {
            View::Root => {
                let view = match k {
                    ROOT_PAINT => Some(View::Paint),
                    ROOT_RIMS => Some(View::Rims),
                    ROOT_KIT => Some(View::Kit),
                    ROOT_UPGRADES => Some(View::Upgrades),
                    ROOT_RESET => {
                        self.pending.push(Req::Reset);
                        None
                    }
                    _ => None,
                };
                if let Some(v) = view {
                    self.notice = None;
                    self.root_cursor = k;
                    self.view = v;
                    // The cursor starts on what the car wears; set by build_rows.
                    self.cursor = usize::MAX;
                    self.pending.push(Req::Open);
                }
            }
            // The paint list's last row opens the custom colour editor on the colour the car shows.
            View::Paint if k + 1 == self.rows.len() => {
                self.view = View::Custom;
                self.cursor = 0;
                self.pending.push(Req::Open);
                self.pending.push(Req::PreviewCustom);
            }
            View::Custom if k == CUSTOM_FINISH => {
                self.adjust_custom(1);
            }
            View::Paint | View::Rims | View::Kit | View::Custom | View::Upgrades => {
                if k != self.cursor && matches!(self.view, View::Paint | View::Rims) {
                    self.cursor = k;
                    self.pending.push(Req::PreviewRow);
                }
                self.pending.push(Req::Commit);
                self.view = View::Root;
                self.cursor = self.root_cursor;
            }
        }
        Step::Stay { changed: true }
    }

    /// Left / Right on the custom colour editor's row under the cursor.
    fn adjust_custom(&mut self, dir: i32) {
        let d = dir as f32;
        match self.cursor {
            0 => self.hsv[0] = (self.hsv[0] + 5.0 * d).rem_euclid(360.0),
            1 => self.hsv[1] = (self.hsv[1] + 0.05 * d).clamp(0.0, 1.0),
            2 => self.hsv[2] = (self.hsv[2] + 0.05 * d).clamp(0.0, 1.0),
            _ => self.metallic = !self.metallic,
        }
        self.pending.push(Req::PreviewCustom);
    }

    fn paint_name(&self, look: Option<&CarLook>) -> String {
        match look.and_then(|l| l.paint) {
            Some(Paint::Custom { rgb, metallic }) => match self.specials.iter().find(|s| s.rgb == rgb && s.metallic == metallic) {
                Some(s) => s.name.clone(),
                None => format!("Custom  #{rgb:06X}"),
            },
            _ => {
                let seq = self.shown_seq(look);
                let i = self.colours.iter().position(|c| c.0 == seq).map_or(0, |i| i + 1);
                format!("Factory {i} / {}", self.colours.len())
            }
        }
    }

    /// (RGB, metallic) the car shows with `look`.
    fn shown_rgb(&self, look: Option<&CarLook>) -> Option<(u32, bool)> {
        match look.and_then(|l| l.paint) {
            Some(Paint::Custom { rgb, metallic }) => Some((rgb, metallic)),
            _ => {
                let seq = self.shown_seq(look);
                self.colours.iter().find(|c| c.0 == seq).map(|c| (c.1, c.2))
            }
        }
    }

    /// The paint `look` shows on this car.
    fn shown_seq(&self, look: Option<&CarLook>) -> u32 {
        match look.and_then(|l| l.paint) {
            Some(Paint::Factory { sequence }) => sequence,
            _ => self.base_seq,
        }
    }

    fn rim_name(&self, look: Option<&CarLook>) -> String {
        match look.and_then(|l| l.rim.as_deref()) {
            Some(m) => self.rims.iter().find(|r| r.media == m).map_or_else(|| m.to_owned(), |r| r.label.clone()),
            None => "Stock".into(),
        }
    }

    /// Option index of kit slot `i` in `look` (0 = stock).
    fn kit_index(&self, look: Option<&CarLook>, i: usize) -> usize {
        let seq = look.and_then(|l| l.kit.get(KIT_SLOTS[i].0).copied());
        seq.and_then(|s| self.kit[i].iter().position(|o| o.sequence == s)).unwrap_or(0)
    }

    fn build_rows(&mut self, looks: &CarLooks) {
        let look = looks.get(&self.car);
        // Prices are shown when the economy is on; what the saved look owns (or wears) shows as owned.
        let shop = crate::progression::wallet::ownership_on();
        let owned = self.owned_of(&looks.cars.get(&self.car).cloned().unwrap_or_default());
        self.kit_rows = (0..KIT_SLOTS.len()).filter(|&i| self.kit[i].len() > 1).collect();
        self.rows = match self.view {
            View::Root => {
                let swatch = self.shown_rgb(look).map(|c| srgb(c.0));
                let custom_kit = look.is_some_and(|l| !l.kit.is_empty());
                vec![
                    Row { swatch, ..row("Paint", Some(self.paint_name(look))) },
                    Row {
                        selectable: !self.rims.is_empty() && !self.no_rims,
                        ..row("Rims", Some(if self.no_rims { "Not available".into() } else { self.rim_name(look) }))
                    },
                    Row {
                        selectable: !self.kit_rows.is_empty(),
                        ..row("Body kit", Some(if self.kit_rows.is_empty() { "No options".into() } else if custom_kit { "Custom".into() } else { "Stock".into() }))
                    },
                    Row {
                        selectable: !self.parts.is_empty(),
                        ..row(
                            "Upgrades",
                            Some(match look.map_or(0, |l| l.upgrades.len()) {
                                _ if self.parts.is_empty() => "No parts".into(),
                                0 => "Stock".into(),
                                n => format!("{n} part{}", if n == 1 { "" } else { "s" }),
                            }),
                        )
                    },
                    row("Reset to stock", None),
                ]
            }
            View::Paint => {
                let stock = self.colours.first().map(|c| c.0);
                let finish = |m: bool| if m { "Metallic" } else { "Gloss" };
                let factory = self.colours.iter().enumerate().map(|(i, &(seq, rgb, metallic))| Row {
                    label: format!("Colour {}{}", i + 1, if Some(seq) == stock { "  (stock)" } else { "" }),
                    value: Some(format!("{}  #{rgb:06X}", finish(metallic))),
                    swatch: Some(srgb(rgb)),
                    selectable: true,
                });
                let specials = self.specials.iter().map(|s| Row {
                    label: s.name.clone(),
                    value: Some(format!("{}  #{:06X}", finish(s.metallic), s.rgb)),
                    swatch: Some(srgb(s.rgb)),
                    selectable: true,
                });
                factory.chain(specials).chain(std::iter::once(row("Custom colour…", None))).collect()
            }
            View::Upgrades => self
                .parts
                .iter()
                .map(|part| {
                    let i = part.index(look.and_then(|l| l.upgrades.get(part.table).copied()));
                    let (id, name, effect) = &part.options[i];
                    let effect = if effect.is_empty() { String::new() } else { format!("   {effect}") };
                    let tag = if !shop {
                        String::new()
                    } else if let Some(c) = upgrades::purchase_price(part, i, &owned.0) {
                        format!("   ·  {} CR", crate::progression::fmt_num(c as i64))
                    } else if i > 0 && owned.0.get(part.table).is_some_and(|v| v.contains(id)) {
                        "   ·  owned".into()
                    } else {
                        String::new()
                    };
                    row(part.label.clone(), Some(format!("‹  {name}  ›{effect}{tag}")))
                })
                .collect(),
            View::Custom => {
                let rgb = hsv_rgb(self.hsv);
                vec![
                    Row { swatch: Some(srgb(rgb)), ..row("Hue", Some(format!("‹  {:.0}°  ›", self.hsv[0]))) },
                    row("Saturation", Some(format!("‹  {:.0}%  ›", self.hsv[1] * 100.0))),
                    row("Brightness", Some(format!("‹  {:.0}%  ›", self.hsv[2] * 100.0))),
                    row("Finish", Some(format!("‹  {}  ›", if self.metallic { "Metallic" } else { "Gloss" }))),
                ]
            }
            View::Rims => std::iter::once(row("Stock", None))
                .chain(self.rims.iter().map(|r| {
                    let tag = (shop && r.price > 0).then(|| if owned.1.contains(&r.media) { "Owned".to_string() } else { format!("{} CR", crate::progression::fmt_num(r.price as i64)) });
                    row(r.label.clone(), tag)
                }))
                .collect(),
            View::Kit => self
                .kit_rows
                .iter()
                .map(|&i| {
                    let o = &self.kit[i][self.kit_index(look, i)];
                    let tag = if shop && !o.stock && o.price > 0 {
                        if owned.0.get(KIT_SLOTS[i].1).is_some_and(|v| v.contains(&o.id)) { "   ·  owned".to_string() } else { format!("   ·  {} CR", crate::progression::fmt_num(o.price as i64)) }
                    } else {
                        String::new()
                    };
                    row(KIT_SLOTS[i].2, Some(format!("‹  {}  ›{tag}", o.name())))
                })
                .collect(),
        };
        if self.cursor == usize::MAX {
            self.cursor = match self.view {
                View::Paint => match look.and_then(|l| l.paint) {
                    Some(Paint::Custom { rgb, metallic }) => self
                        .specials
                        .iter()
                        .position(|s| s.rgb == rgb && s.metallic == metallic)
                        .map_or(self.colours.len() + self.specials.len(), |i| self.colours.len() + i),
                    _ => {
                        let seq = self.shown_seq(look);
                        self.colours.iter().position(|c| c.0 == seq).unwrap_or(0)
                    }
                },
                View::Rims => look.and_then(|l| l.rim.as_deref()).and_then(|m| self.rims.iter().position(|r| r.media == m)).map_or(0, |i| i + 1),
                _ => 0,
            };
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// Part row Ids and rims `saved` owns: its owned lists plus what it wears (looks saved before the economy).
    fn owned_of(&self, saved: &CarLook) -> (upgrades::OwnedParts, Vec<String>) {
        let mut ids = saved.owned_parts.clone();
        let mut rims = saved.owned_rims.clone();
        let add = |ids: &mut upgrades::OwnedParts, table: &str, id: i64| {
            let v = ids.entry(table.to_owned()).or_default();
            if id >= 0 && !v.contains(&id) {
                v.push(id);
            }
        };
        if let Some(r) = &saved.rim {
            if !rims.contains(r) {
                rims.push(r.clone());
            }
        }
        for (i, slot) in KIT_SLOTS.iter().enumerate() {
            if let Some(o) = saved.kit.get(slot.0).and_then(|&seq| self.kit[i].iter().find(|o| o.sequence == seq)) {
                add(&mut ids, slot.1, o.id);
            }
        }
        for (table, &id) in &saved.upgrades {
            add(&mut ids, table, id);
        }
        (ids, rims)
    }

    /// Credits `look` costs over `saved` (paint is free), with the bought items' names; records them as owned in `look`.
    fn charge(&self, saved: &CarLook, look: &mut CarLook) -> (i64, Vec<String>) {
        let (ids, rims) = self.owned_of(saved);
        look.owned_parts = ids;
        look.owned_rims = rims;
        let mut cost = 0i64;
        let mut items = Vec::new();
        if let Some(r) = look.rim.clone() {
            if !look.owned_rims.contains(&r) {
                if let Some(rim) = self.rims.iter().find(|x| x.media == r) {
                    cost += rim.price as i64;
                    items.push(rim.label.clone());
                }
                look.owned_rims.push(r);
            }
        }
        for (i, slot) in KIT_SLOTS.iter().enumerate() {
            let Some(o) = look.kit.get(slot.0).and_then(|&seq| self.kit[i].iter().find(|o| o.sequence == seq)).copied() else { continue };
            if !o.stock && o.id >= 0 && !look.owned_parts.get(slot.1).is_some_and(|v| v.contains(&o.id)) {
                cost += o.price as i64;
                items.push(format!("{} {}", slot.2, o.name()));
                look.owned_parts.entry(slot.1.to_owned()).or_default().push(o.id);
            }
        }
        let chosen: Vec<(String, i64)> = look.upgrades.iter().map(|(t, &id)| (t.clone(), id)).collect();
        for (table, id) in chosen {
            let Some(part) = self.parts.iter().find(|p| p.table == table) else { continue };
            let i = part.index(Some(id));
            if let Some(c) = upgrades::purchase_price(part, i, &look.owned_parts) {
                cost += c as i64;
                items.push(part.label.clone());
            }
            upgrades::record_owned(part, i, &mut look.owned_parts);
        }
        (cost, items)
    }

    /// Read the car's colours, kit rows and the rim list.
    fn load(&mut self, garage: &Garage) {
        let name = garage.cars[garage.current].clone();
        if self.rims.is_empty() {
            let maker = |m: &str| {
                // gamedb maker names carry a two-letter prefix (`RO_JA Motorsports`).
                let b = m.as_bytes();
                if b.len() > 3 && b[2] == b'_' && b[..2].iter().all(u8::is_ascii_uppercase) { m[3..].to_owned() } else { m.to_owned() }
            };
            self.rims = read_json(&garage.assets.join("upgrades/rims.json"))
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|r| !r["exception"].as_bool().unwrap_or(false))
                        .filter_map(|r| {
                            let media = r["media_name"].as_str()?.to_owned();
                            garage.assets.join("cars/wheels").join(&media).is_dir().then(|| Rim {
                                label: format!("{}  {}", maker(r["maker"].as_str().unwrap_or("")), r["name"].as_str().unwrap_or(&media)),
                                price: r["price"].as_u64().unwrap_or(0) as u32,
                                media,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        if self.specials.is_empty() {
            // Primary colour of FH1's special colours; carbon fibre (finish 2) and colourless rows are skipped.
            self.specials = read_json(&garage.assets.join("upgrades/special_colors.json"))
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|c| c["finish"].as_u64().unwrap_or(0) < 2)
                        .filter_map(|c| Some(Special { name: c["name"].as_str()?.to_owned(), rgb: c["primary"].as_u64()? as u32 & 0xFF_FFFF, metallic: c["finish"].as_u64() == Some(1) }))
                        .collect()
                })
                .unwrap_or_default();
        }
        if self.car == name && !self.colours.is_empty() {
            return;
        }
        let dir = garage.assets.join("cars").join(&name);
        let physics = read_json(&dir.join("physics.json"));
        self.colours = physics["colors"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| Some((r["Sequence"].as_u64()? as u32, r["RGB"].as_u64()? as u32, r["Metallic"].as_u64().unwrap_or(0) != 0))).collect())
            .unwrap_or_default();
        let stock = read_json(&dir.join("model.json"))["paint"]["sequence"].as_u64().map(|v| v as u32);
        // main.rs DEFAULT_CAR wears its own colour when no look is saved.
        self.base_seq = if name == crate::DEFAULT_CAR.0 { crate::DEFAULT_CAR.1 } else { stock.or(self.colours.first().map(|c| c.0)).unwrap_or(1) };
        self.no_rims = read_json(&garage.assets.join("upgrades/cars").join(format!("{name}.json")))["no_rim_styles"].as_bool().unwrap_or(false);
        self.kit = kit_options(&garage.assets, &name);
        self.parts = upgrades::catalog(&upgrades::read_doc(&garage.assets, &name));
        self.car = name;
    }
}

/// Preview `look` on `car`; true when the body must be respawned (paint, rim or kit differ from what it shows).
fn set_preview(looks: &mut CarLooks, car: &str, look: CarLook) -> bool {
    let shown = looks.get(car).cloned().unwrap_or_default();
    if shown == look {
        return false;
    }
    let visual = !shown.same_visual(&look);
    looks.preview = Some((car.to_owned(), look));
    visual
}

/// Custom editor rows.
const CUSTOM_FINISH: usize = 3;

/// 0xRRGGBB of hue (deg), saturation, value.
fn hsv_rgb([h, s, v]: [f32; 3]) -> u32 {
    let c = Color::from(Hsva::new(h, s, v, 1.0)).to_srgba();
    let b = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u32;
    (b(c.red) << 16) | (b(c.green) << 8) | b(c.blue)
}

/// Hue (deg), saturation, value of 0xRRGGBB.
fn rgb_hsv(rgb: u32) -> [f32; 3] {
    let h = Hsva::from(Color::from(srgb(rgb).to_srgba()));
    [h.hue, h.saturation, h.value]
}

fn srgb(rgb: u32) -> Color {
    Color::srgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Carry out the page's requests: (re)build its rows, preview, keep or drop a look, save garage.json and respawn the
/// player car's body.
#[allow(clippy::too_many_arguments)]
fn apply_requests(
    mut commands: Commands,
    mut menu: ResMut<Menu>,
    mut looks: ResMut<CarLooks>,
    garage: Res<Garage>,
    track: Res<Track>,
    asset_server: Res<AssetServer>,
    mut cars: Query<(Entity, &mut Car), With<fh1_engine::ai::PlayerCar>>,
    bodies: Query<(Entity, &ChildOf), With<CarModel>>,
    mut profile: Option<ResMut<crate::progression::Profile>>,
) {
    let m = &mut menu.custom;
    if m.pending.is_empty() {
        return;
    }
    let mut respawn = false;
    // The upgrades the car drives with before these requests (its physics is rebuilt when they change).
    let upgrades_before = looks.get(&m.car).map(|l| l.upgrades.clone()).unwrap_or_default();
    for req in std::mem::take(&mut m.pending) {
        // The look being edited: the preview, else the saved one.
        let current = looks.get(&m.car).cloned().unwrap_or_default();
        let saved = looks.cars.get(&m.car).cloned().unwrap_or_default();
        match req {
            Req::Open => {
                m.load(&garage);
                if m.view == View::Custom {
                    // The editor starts on the colour the car shows.
                    if let Some((rgb, metallic)) = m.shown_rgb(Some(&current)) {
                        m.hsv = rgb_hsv(rgb);
                        m.metallic = metallic;
                    }
                }
            }
            Req::UpgradeStep(dir) => {
                let Some(part) = m.parts.get(m.cursor) else { continue };
                let n = part.options.len() as i32;
                let i = (part.index(current.upgrades.get(part.table).copied()) as i32 + dir).rem_euclid(n.max(1)) as usize;
                let mut look = current;
                if i == 0 {
                    look.upgrades.remove(part.table);
                } else {
                    look.upgrades.insert(part.table.to_owned(), part.options[i].0);
                    // One boost system at a time: a turbo / supercharger choice drops the others.
                    if part.table.contains("Turbo") || part.table.ends_with("CSC") || part.table.ends_with("DSC") {
                        look.upgrades.retain(|t, _| t == part.table || !(t.contains("Turbo") || t.ends_with("CSC") || t.ends_with("DSC")));
                    }
                }
                respawn |= set_preview(&mut looks, &m.car, look);
            }
            Req::PreviewCustom => {
                let mut look = current;
                look.paint = Some(Paint::Custom { rgb: hsv_rgb(m.hsv), metallic: m.metallic });
                respawn |= set_preview(&mut looks, &m.car, look);
            }
            Req::PreviewRow => {
                let mut look = saved;
                match m.view {
                    View::Paint => {
                        let special = m.cursor.checked_sub(m.colours.len()).and_then(|i| m.specials.get(i));
                        look.paint = match (m.colours.get(m.cursor), special) {
                            (Some(&(sequence, ..)), _) => Some(Paint::Factory { sequence }),
                            (None, Some(s)) => Some(Paint::Custom { rgb: s.rgb, metallic: s.metallic }),
                            // The "Custom colour" row: keep what the car shows.
                            _ => continue,
                        };
                    }
                    View::Rims => look.rim = m.cursor.checked_sub(1).and_then(|i| m.rims.get(i)).map(|r| r.media.clone()),
                    _ => continue,
                }
                respawn |= set_preview(&mut looks, &m.car, look);
            }
            Req::KitStep(dir) => {
                let Some(&slot) = m.kit_rows.get(m.cursor) else { continue };
                let n = m.kit[slot].len() as i32;
                let i = (m.kit_index(Some(&current), slot) as i32 + dir).rem_euclid(n.max(1)) as usize;
                let mut look = current;
                match m.kit[slot].get(i) {
                    Some(o) if !o.stock => {
                        look.kit.insert(KIT_SLOTS[slot].0.to_owned(), o.sequence);
                    }
                    _ => {
                        look.kit.remove(KIT_SLOTS[slot].0);
                    }
                }
                respawn |= set_preview(&mut looks, &m.car, look);
            }
            Req::Commit => {
                if let Some((car, mut look)) = looks.preview.take() {
                    // Pay for the parts not owned yet (module doc); short of credits the preview is dropped.
                    let (cost, items) = m.charge(&saved, &mut look);
                    if cost > 0 && crate::progression::wallet::ownership_on() {
                        if let Some(p) = profile.as_deref_mut() {
                            let label = crate::progression::fmt_num(cost);
                            if crate::progression::wallet::spend(p, cost, &format!("{car}: {}", items.join(", "))) {
                                look.spent += cost;
                                m.notice = Some(format!("Bought for {label} CR"));
                            } else {
                                m.notice = Some(format!("INSUFFICIENT CREDITS: {label} CR needed"));
                                respawn = true;
                                continue;
                            }
                        }
                    }
                    if look == CarLook::default() {
                        looks.cars.remove(&car);
                    } else {
                        looks.cars.insert(car, look);
                    }
                    looks.save();
                }
            }
            Req::Revert => {
                if looks.preview.take().is_some() {
                    respawn = true;
                }
            }
            Req::Reset => {
                looks.preview = None;
                if let Some(old) = looks.cars.remove(&m.car) {
                    // Bought parts stay owned (switching back to them is free).
                    let keep = CarLook { owned_parts: old.owned_parts, owned_rims: old.owned_rims, spent: old.spent, ..default() };
                    if keep != CarLook::default() {
                        looks.cars.insert(m.car.clone(), keep);
                    }
                    looks.save();
                    respawn = true;
                }
            }
        }
    }
    m.build_rows(&looks);
    menu.dirty = true;
    let name = &garage.cars[garage.current];
    // Upgrades changed: rebuild the player car's physics in place (same spot and heading; the body follows, as its
    // offset is the centre of mass).
    let upgrades_now = looks.get(name).map(|l| l.upgrades.clone()).unwrap_or_default();
    if upgrades_now != upgrades_before && upgrades::enabled() {
        let dir = garage.assets.join("cars").join(name);
        for (_, mut car) in &mut cars {
            match crate::data::CarData::load_with(&dir, |p| looks.patch_physics(name, p)) {
                Ok(data) => {
                    let ground = car.0.position - Vec3::Y * car.0.data.cg_height.max(0.2);
                    let yaw = car.0.yaw();
                    info!("{name}: upgrades {upgrades_now:?}: {:.0} kg, {:.0} N·m", data.mass, data.torque_scale);
                    car.0.data = data;
                    car.0.place(ground, yaw);
                    respawn = true;
                }
                Err(e) => warn!("{name}: upgrades: {e:#}"),
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
        commands.entity(root).with_children(|p| crate::spawn_body(p, &garage, &track.id, name, cg, &asset_server, &looks));
    }
}
