//! Car and map browser of the pause menu (X1, 2026-10-05): Game (FH1 / FH2 / FM4 ...) -> Manufacturer -> car,
//! with year, class + PI and drive type per car, a class filter and sort orders; maps grouped by game.
//!
//! Sources: FH1's own cars = `cars/<media>/physics.json` `car` row (gamedb Data_Car; names through the installed
//! string tables `ui/strings/EN.zip`); another game's cars = `imported/<game>/cars/index.json` rows `{id, media_name,
//! name, maker, year, class, pi, drive, has_model}` (the import contract, docs/FH2_RECON.md).
//!
//! B1 (2026-10-05): controller-first navigation (input table below), game tabs (LB / RB) from any level, letter /
//! brand jumps (LT / RT), class + drive filters, held-repeat with acceleration, a details panel, remembered
//! selections, and one shared drawing ([`spawn_view`]) for the pause menu and the main menu.

use std::collections::HashMap;
use std::path::Path;

use bevy::prelude::*;
use serde_json::Value;

use super::loading::{Fade, FadeBg};
use super::{UiFont, ACCENT, DIM};

/// One car of the garage list.
#[derive(Debug, Clone)]
pub struct CarEntry {
    /// Index into `Garage::cars`.
    pub index: usize,
    pub game: String,
    pub maker: String,
    pub name: String,
    pub year: Option<i64>,
    pub class: Option<String>,
    pub pi: Option<i64>,
    pub drive: String,
    /// The car's `physics.json` (stats for the details panel, read on demand).
    pub physics: Option<std::path::PathBuf>,
    /// gamedb Data_Car.Id (FH1 cars; `ui/textures/thumbnails/thumbnail_<id>.png`, the game's own car photo).
    pub id: Option<i64>,
}

impl CarEntry {
    /// `1962  ·  B 657  ·  RWD`.
    pub fn detail(&self) -> String {
        let mut parts = Vec::new();
        if let Some(y) = self.year {
            parts.push(y.to_string());
        }
        match (&self.class, self.pi) {
            (Some(c), Some(p)) => parts.push(format!("{c} {p}")),
            (Some(c), None) => parts.push(c.clone()),
            (None, Some(p)) => parts.push(p.to_string()),
            _ => {}
        }
        if !self.drive.is_empty() {
            parts.push(self.drive.clone());
        }
        parts.join("  ·  ")
    }
}

/// FH1's CarClasses (gamedb): badge, MaxPerformanceIndex, MaxDisplayPerformanceIndex.
const FH1_CLASSES: [(&str, f64, f64); 11] = [
    ("F", 0.005, 99.0),
    ("E", 0.459, 200.0),
    ("D", 0.539, 300.0),
    ("C", 0.5975, 400.0),
    ("B", 0.6505, 500.0),
    ("A", 0.739, 600.0),
    ("S", 0.81, 700.0),
    ("R3", 0.8665, 800.0),
    ("R2", 0.8991, 900.0),
    ("R1", 0.999999999999, 999.0),
    ("U", 1.0, 999.0),
];

/// Normalised PI -> the number the game shows (fh1_engine::pi::display_pi, default.xex 82BE0378, VERIFIED by 47): class F
/// shows 99; class i >= 1 maps linearly onto [MaxDisplay(i-1) + 1, MaxDisplay(i)] with truncation.
fn fh1_display_pi(pi: f64) -> i64 {
    let p = pi.clamp(0.0, 1.0);
    let Some(i) = FH1_CLASSES.iter().position(|c| p <= c.1) else { return 999 };
    if i == 0 {
        return FH1_CLASSES[0].2 as i64;
    }
    let (lo_pi, lo_d) = (FH1_CLASSES[i - 1].1, FH1_CLASSES[i - 1].2 as i64 + 1);
    let (hi_pi, hi_d) = (FH1_CLASSES[i].1, FH1_CLASSES[i].2 as i64);
    let t = (p - lo_pi) / (hi_pi - lo_pi).max(1e-12);
    let v = (t * (hi_d + 1 - lo_d) as f64 + lo_d as f64) as i64;
    if v < lo_d { lo_d } else { v.min(hi_d) }
}

fn drive_name(id: Option<i64>) -> &'static str {
    match id {
        Some(1) => "FWD",
        Some(2) => "RWD",
        Some(3) => "AWD",
        _ => "",
    }
}

/// Class letters in rank order (lowest first), across every game's scheme.
const CLASS_ORDER: [&str; 16] = ["F", "E", "D", "C", "B", "A", "S", "S1", "S2", "R3", "R2", "R1", "X", "U", "P", "?"];

fn class_rank(c: &str) -> usize {
    CLASS_ORDER.iter().position(|k| *k == c).unwrap_or(CLASS_ORDER.len())
}

/// Sort orders of the car level.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Sort {
    #[default]
    Name,
    Pi,
    Year,
}

impl Sort {
    pub fn next(self) -> Self {
        match self {
            Sort::Name => Sort::Pi,
            Sort::Pi => Sort::Year,
            Sort::Year => Sort::Name,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Pi => "PI",
            Sort::Year => "year",
        }
    }
}

#[derive(Debug, Default)]
pub struct CarCatalog {
    pub entries: Vec<CarEntry>,
    /// Games in garage order.
    pub games: Vec<String>,
    /// Details-panel stats per entry index, read from `physics.json` the first time a car is selected.
    stats: std::sync::Mutex<HashMap<usize, Option<CarStats>>>,
    /// Games listed greyed because they aren't imported (crate::track::OPTIONAL_GAMES): label -> requirement. They
    /// are also in `games` (after the others) and have no entries.
    pub locked: HashMap<String, String>,
}

/// Spec figures of a car (gamedb Data_Car columns of its physics.json `car` row; imported cars carry the same columns).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CarStats {
    /// `SimPeakPower` x 100 W.
    pub power_w: Option<f64>,
    /// `CurbWeight` x 100 kg.
    pub mass_kg: Option<f64>,
    /// `Time:0-60-sec`.
    pub zero_60: Option<f64>,
    /// `TopSpeed-mph`.
    pub top_mph: Option<f64>,
}

impl CarStats {
    fn read(path: &Path) -> Option<Self> {
        let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let c = &v["car"];
        let pos = |k: &str| c[k].as_f64().filter(|x| *x > 0.0);
        Some(Self { power_w: pos("SimPeakPower").map(|p| p * 100.0), mass_kg: pos("CurbWeight").map(|m| m * 100.0), zero_60: pos("Time:0-60-sec"), top_mph: pos("TopSpeed-mph") })
    }
}

/// Class + drive filter of the car lists (`None` = any).
fn passes(e: &CarEntry, class: Option<&str>, drive: Option<&str>) -> bool {
    class.is_none_or(|c| e.class.as_deref() == Some(c)) && drive.is_none_or(|d| e.drive == d)
}

/// The catalog built ahead on a background thread at startup ([`prefetch_catalog`]): building it reads every car's
/// physics.json and the imported indexes, which ran inside the frame that opened car select (2026-10-08 freezes: a 3.6 s
/// hitch on the menu open). FH1_CATALOG_PREFETCH=0 = build on open, as before.
static PREFETCHED: std::sync::Mutex<Option<(Vec<String>, std::sync::Arc<CarCatalog>)>> = std::sync::Mutex::new(None);

/// Builds the catalog for `cars` (and every details-panel stat) on its own thread.
pub fn prefetch_catalog(assets: &Path, cars: &[String]) {
    if std::env::var("FH1_CATALOG_PREFETCH").is_ok_and(|v| v == "0") {
        return;
    }
    let (assets, cars) = (assets.to_owned(), cars.to_vec());
    let _ = std::thread::Builder::new().name("fh1-car-catalog".into()).spawn(move || {
        let t = std::time::Instant::now();
        let cat = CarCatalog::build(&assets, &cars);
        for i in 0..cat.entries.len() {
            cat.stats(i);
        }
        info!("car catalog: {} cars prefetched in {:.1} s", cat.entries.len(), t.elapsed().as_secs_f32());
        if let Ok(mut p) = PREFETCHED.lock() {
            *p = Some((cars, std::sync::Arc::new(cat)));
        }
    });
}

/// The prefetched catalog when it is ready and was built for these cars, else one built here (the old path).
pub fn catalog_for(assets: &Path, cars: &[String]) -> std::sync::Arc<CarCatalog> {
    let ready = PREFETCHED.lock().ok().and_then(|p| p.as_ref().filter(|(c, _)| c.as_slice() == cars).map(|(_, c)| c.clone()));
    ready.unwrap_or_else(|| std::sync::Arc::new(CarCatalog::build(assets, cars)))
}

impl CarCatalog {
    /// `cars` = `Garage::cars` (FH1 media names, or `../imported/<game>/cars/<media>`).
    pub fn build(assets: &Path, cars: &[String]) -> Self {
        let strings = fh1_ui::strtable::StringTables::load_zip(assets.join("ui/strings/EN.zip")).ok();
        let text = |r: &str| -> String {
            strings.as_ref().and_then(|s| s.resolve(r)).map(fh1_ui::strtable::strip_markup).unwrap_or_else(|| r.trim_start_matches("_&").to_owned())
        };
        let mut indexes: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut out = CarCatalog::default();
        for (index, car) in cars.iter().enumerate() {
            let entry = match car.strip_prefix("../imported/").and_then(|r| r.split_once("/cars/")) {
                Some((game, media)) => {
                    let rows = indexes.entry(game.to_owned()).or_insert_with(|| {
                        let v: Value = std::fs::read(assets.join("imported").join(game).join("cars/index.json"))
                            .ok()
                            .and_then(|b| serde_json::from_slice(&b).ok())
                            .unwrap_or(Value::Null);
                        v.as_array().into_iter().flatten().filter_map(|r| Some((r["media_name"].as_str()?.to_owned(), r.clone()))).collect()
                    });
                    imported_entry(index, game, media, rows.get(media))
                }
                None => fh1_entry(index, assets, car, &text),
            };
            let mut entry = entry;
            entry.physics = Some(assets.join("cars").join(car).join("physics.json"));
            if !out.games.contains(&entry.game) {
                out.games.push(entry.game.clone());
            }
            out.entries.push(entry);
        }
        // Optional games without imported cars: greyed tabs (FM3 isn't importable yet: not listed).
        for g in crate::track::OPTIONAL_GAMES.iter().filter(|g| !g.coming_soon) {
            let label = g.label();
            if !out.games.contains(&label) && !g.cars_imported(assets) {
                out.games.push(label.clone());
                out.locked.insert(label, g.requirement());
            }
        }
        out
    }

    /// Makers of a game with their car counts (under the class filter), alphabetical.
    pub fn makers(&self, game: &str, class: Option<&str>) -> Vec<(String, usize)> {
        self.makers_by(game, class, None)
    }

    /// Makers of a game with their car counts under the class and drive filters, alphabetical.
    pub fn makers_by(&self, game: &str, class: Option<&str>, drive: Option<&str>) -> Vec<(String, usize)> {
        let mut m: Vec<(String, usize)> = Vec::new();
        for e in self.entries.iter().filter(|e| e.game == game && passes(e, class, drive)) {
            match m.iter_mut().find(|(k, _)| *k == e.maker) {
                Some((_, n)) => *n += 1,
                None => m.push((e.maker.clone(), 1)),
            }
        }
        m.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        m
    }

    /// Cars of a game + maker (`None` = all makers) under the class filter, in `sort` order: entry indices.
    #[allow(dead_code)]
    pub fn cars(&self, game: &str, maker: Option<&str>, class: Option<&str>, sort: Sort) -> Vec<usize> {
        self.cars_by(game, maker, class, None, sort)
    }

    /// [`CarCatalog::cars`] with a drive filter (`FWD` / `RWD` / `AWD`) too.
    pub fn cars_by(&self, game: &str, maker: Option<&str>, class: Option<&str>, drive: Option<&str>, sort: Sort) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                e.game == game && maker.is_none_or(|m| e.maker == m) && passes(e, class, drive)
            })
            .collect();
        let name = |i: usize| (self.entries[i].maker.to_lowercase(), self.entries[i].name.to_lowercase());
        match sort {
            Sort::Name => v.sort_by_key(|&i| name(i)),
            Sort::Pi => v.sort_by_key(|&i| (std::cmp::Reverse(self.entries[i].pi.unwrap_or(0)), name(i))),
            Sort::Year => v.sort_by_key(|&i| (self.entries[i].year.unwrap_or(0), name(i))),
        }
        v
    }

    /// Classes present in a game, lowest first.
    pub fn classes(&self, game: &str) -> Vec<String> {
        let mut c: Vec<String> = Vec::new();
        for e in self.entries.iter().filter(|e| e.game == game) {
            if let Some(k) = &e.class {
                if !c.contains(k) {
                    c.push(k.clone());
                }
            }
        }
        c.sort_by_key(|k| class_rank(k));
        c
    }

    /// Drive types present in a game (FWD, RWD, AWD, then others).
    pub fn drives(&self, game: &str) -> Vec<String> {
        let mut d: Vec<String> = Vec::new();
        for e in self.entries.iter().filter(|e| e.game == game && !e.drive.is_empty()) {
            if !d.contains(&e.drive) {
                d.push(e.drive.clone());
            }
        }
        let rank = |s: &str| ["FWD", "RWD", "AWD"].iter().position(|k| *k == s).unwrap_or(3);
        d.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.cmp(b)));
        d
    }

    /// Stats of entry `i` (cached; `None` when its physics.json is missing).
    pub fn stats(&self, i: usize) -> Option<CarStats> {
        let mut cache = self.stats.lock().ok()?;
        *cache.entry(i).or_insert_with(|| self.entries.get(i).and_then(|e| e.physics.as_deref()).and_then(CarStats::read))
    }

    pub fn entry_of(&self, garage_index: usize) -> Option<&CarEntry> {
        self.entries.iter().find(|e| e.index == garage_index)
    }

    /// The entries `keep` accepts (garage indices unchanged), for the garage / autoshow lists (ui/garage.rs).
    pub fn subset(&self, keep: impl Fn(&CarEntry) -> bool) -> CarCatalog {
        let mut out = CarCatalog::default();
        for e in self.entries.iter().filter(|e| keep(e)) {
            if !out.games.contains(&e.game) {
                out.games.push(e.game.clone());
            }
            out.entries.push(e.clone());
        }
        out
    }
}

fn fh1_entry(index: usize, assets: &Path, media: &str, text: &impl Fn(&str) -> String) -> CarEntry {
    let phys: Value = std::fs::read(assets.join("cars").join(media).join("physics.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let car = &phys["car"];
    let class = car["ClassID"].as_i64().and_then(|c| FH1_CLASSES.get(c as usize)).map(|c| c.0.to_owned());
    CarEntry {
        index,
        game: "FH1".into(),
        maker: car["MakeName"].as_str().map(text).filter(|s| !s.is_empty()).unwrap_or_else(|| media.split('_').next().unwrap_or(media).to_owned()),
        name: car["DisplayName"].as_str().map(text).filter(|s| !s.is_empty()).unwrap_or_else(|| media.to_owned()),
        year: car["Year"].as_i64(),
        class,
        pi: car["PerformanceIndex"].as_f64().map(fh1_display_pi),
        drive: drive_name(car["DriveTypeID"].as_i64()).into(),
        physics: None,
        id: phys["id"].as_i64(),
    }
}

fn imported_entry(index: usize, game: &str, media: &str, row: Option<&Value>) -> CarEntry {
    let game_label = game.to_ascii_uppercase();
    // The import contract (FH2, FM4).
    let Some((r, name)) = row.and_then(|r| Some((r, r["name"].as_str()?))) else {
        return CarEntry { index, game: game_label, maker: "Other".into(), name: media.into(), year: None, class: None, pi: None, drive: String::new(), physics: None, id: None };
    };
    CarEntry {
        index,
        game: game_label,
        maker: r["maker"].as_str().filter(|s| !s.is_empty()).unwrap_or("Other").to_owned(),
        name: name.to_owned(),
        year: r["year"].as_i64(),
        class: r["class"].as_str().map(str::to_owned),
        pi: r["pi"].as_i64(),
        drive: r["drive"].as_str().unwrap_or("").to_owned(),
        physics: None,
        id: None,
    }
}

/// Every imported game's selectable car folders, addressed relative to `cars/` (`../imported/<game>/cars/<media>`),
/// for `Garage::cars`: games with an `imported/<game>/cars/index.json` in the contract shape (rows with `name`),
/// rows with `has_model` and not `selectable: false`. Only the known optional games (crate::track::OPTIONAL_GAMES) are
/// read; other folders under `imported/` (leftovers of removed imports) are ignored.
pub fn imported_car_folders(assets: &Path) -> Vec<String> {
    let mut games: Vec<String> = crate::track::OPTIONAL_GAMES.iter().map(|g| g.folder.to_owned()).collect();
    games.sort();
    let mut out = Vec::new();
    for g in games {
        let Some(rows) = std::fs::read(assets.join("imported").join(&g).join("cars/index.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
            continue;
        };
        for r in rows.as_array().into_iter().flatten() {
            if r["name"].is_null() || r["has_model"].as_bool() != Some(true) || r["selectable"].as_bool() == Some(false) {
                continue;
            }
            if let Some(m) = r["media_name"].as_str() {
                out.push(format!("../imported/{g}/cars/{m}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Input (B1, 2026-10-05): controller-first, every action also on the keyboard.
//
// | Action                         | Pad              | Keyboard            |
// |--------------------------------|------------------|---------------------|
// | move (held = repeat, speeds up)| D-pad / L stick  | Up Down / W S       |
// | page jump (10 rows)            | D-pad left/right | Left Right / A D    |
// | previous / next game tab       | LB / RB          | Q / E               |
// | brand: jump letter; car: brand | LT / RT          | Z / C, PgUp / PgDn  |
// | class filter                   | X                | F                   |
// | drive filter                   | View (Back)      | G                   |
// | sort (cars)                    | Y                | Tab                 |
// | select                         | A                | Enter / Space       |
// | back a level (top: close)      | B                | Backspace           |
// ---------------------------------------------------------------------------------------------------------------

/// Rows a page jump moves.
pub const PAGE: usize = 10;

/// One frame of menu input for a browser.
#[derive(Clone, Copy, Debug, Default)]
pub struct BrowserInput {
    /// Rows to move: -1 up / +1 down (held auto-repeat may give more).
    pub vertical: i32,
    /// Page jump: -1 / +1 pages.
    pub horizontal: i32,
    /// Previous / next game tab (LB / RB).
    pub tab: i32,
    /// LT / RT: brand level = previous / next initial letter; car level = previous / next brand (map browser: track).
    pub jump: i32,
    /// Next class filter (X).
    pub filter: bool,
    /// Next drive filter (View).
    pub drive: bool,
    /// Next sort order (car browser, Y).
    pub sort: bool,
    pub confirm: bool,
    pub back: bool,
    /// `vertical` / `horizontal` come from a held auto-repeat: stop at the list ends instead of wrapping.
    pub held: bool,
}

/// Held-direction auto-repeat of one axis: a step on press, then after 0.35 s repeats that speed up from 11/s to
/// 50/s over 1.2 s.
#[derive(Clone, Copy, Debug, Default)]
pub struct AxisRepeat {
    dir: i32,
    held_for: f32,
    next: f32,
}

const REPEAT_DELAY: f32 = 0.35;

impl AxisRepeat {
    /// `held` = -1 / 0 / +1 this frame -> (steps, auto-repeat?).
    pub fn update(&mut self, held: i32, dt: f32) -> (i32, bool) {
        if held == 0 {
            *self = Self::default();
            return (0, false);
        }
        if held != self.dir {
            *self = Self { dir: held, held_for: 0.0, next: REPEAT_DELAY };
            return (held, false);
        }
        self.held_for += dt;
        self.next -= dt;
        let mut n = 0;
        while self.next <= 0.0 && n < 4 {
            n += 1;
            let k = ((self.held_for - REPEAT_DELAY) / 1.2).clamp(0.0, 1.0);
            self.next += 0.09 - 0.07 * k;
        }
        if self.next <= 0.0 {
            self.next = 0.02;
        }
        (held * n, n > 0)
    }
}

/// Auto-repeat state of the browser's two axes (keep one per menu).
#[derive(Clone, Copy, Debug, Default)]
pub struct Repeat {
    pub vertical: AxisRepeat,
    pub horizontal: AxisRepeat,
}

/// Reads one frame of browser input from the keyboard and every pad (mapping in the table above). Escape and Start
/// are left to the caller (the pause menu uses them to close).
pub fn read_input(keys: &ButtonInput<KeyCode>, pads: &Query<&bevy::input::gamepad::Gamepad>, repeat: &mut Repeat, dt: f32) -> BrowserInput {
    use bevy::input::gamepad::{GamepadAxis, GamepadButton as P};
    use KeyCode as K;
    let kp = |k: K| keys.just_pressed(k);
    let kh = |k: K| keys.pressed(k);
    let pp = |b: P| pads.iter().any(|p| p.just_pressed(b));
    let ph = |b: P| pads.iter().any(|p| p.pressed(b));
    let stick = |a: GamepadAxis| pads.iter().map(|p| p.get(a).unwrap_or(0.0)).fold(0.0f32, |m, v| if v.abs() > m.abs() { v } else { m });
    let dir = |neg: bool, pos: bool| if neg && !pos { -1 } else if pos && !neg { 1 } else { 0 };
    let sy = stick(GamepadAxis::LeftStickY);
    let sx = stick(GamepadAxis::LeftStickX);
    let v = dir(kh(K::ArrowUp) || kh(K::KeyW) || ph(P::DPadUp) || sy > 0.5, kh(K::ArrowDown) || kh(K::KeyS) || ph(P::DPadDown) || sy < -0.5);
    let h = dir(kh(K::ArrowLeft) || kh(K::KeyA) || ph(P::DPadLeft) || sx < -0.6, kh(K::ArrowRight) || kh(K::KeyD) || ph(P::DPadRight) || sx > 0.6);
    let (vertical, vh) = repeat.vertical.update(v, dt);
    let (horizontal, hh) = repeat.horizontal.update(h, dt);
    BrowserInput {
        vertical,
        // One page per step, however many repeats fell in this frame.
        horizontal: horizontal.signum(),
        tab: dir(kp(K::KeyQ) || pp(P::LeftTrigger), kp(K::KeyE) || pp(P::RightTrigger)),
        jump: dir(kp(K::KeyZ) || kp(K::PageUp) || pp(P::LeftTrigger2), kp(K::KeyC) || kp(K::PageDown) || pp(P::RightTrigger2)),
        filter: kp(K::KeyF) || pp(P::West),
        drive: kp(K::KeyG) || pp(P::Select),
        sort: kp(K::Tab) || pp(P::North),
        confirm: kp(K::Enter) || kp(K::NumpadEnter) || kp(K::Space) || pp(P::South),
        back: kp(K::Backspace) || pp(P::East),
        held: vh || hh,
    }
}

/// What a browser step chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Still browsing (redraw if `changed`).
    Browsing { changed: bool },
    /// Back out of the top level: the caller closes the browser.
    Leave,
    /// A car: index into `Garage::cars`.
    Car(usize),
    /// A map id (`colorado`, `fh2/anthem`, ...).
    Map(String),
}

/// Label colour of a greyed ([`BrowserRow::locked`]) row, and under the cursor (opaque: the main menu sets its own
/// fade alpha).
pub const LOCKED: Color = Color::srgb(0.42, 0.43, 0.46);
pub const LOCKED_ON: Color = Color::srgb(0.66, 0.67, 0.70);
/// Cursor background of a greyed row (instead of the accent).
pub const LOCKED_CURSOR: Color = Color::srgba(1.0, 1.0, 1.0, 0.10);

/// A row to draw.
#[derive(Clone, Debug)]
pub struct BrowserRow {
    pub label: String,
    pub value: Option<String>,
    /// Greyed and not pickable: content of an optional game that isn't imported (`value` = what it needs).
    pub locked: bool,
}

/// One game tab of the strip.
#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub label: String,
    pub count: usize,
    pub active: bool,
}

/// One button hint: pad glyphs (`A`, `LB`, ...), keyboard keys and the action.
#[derive(Clone, Debug, PartialEq)]
pub struct Hint {
    pub pads: &'static [&'static str],
    pub keys: &'static str,
    pub action: String,
}

fn hint(pads: &'static [&'static str], keys: &'static str, action: impl Into<String>) -> Hint {
    Hint { pads, keys, action: action.into() }
}

/// Everything a browser screen shows around its rows.
#[derive(Clone, Debug, Default)]
pub struct BrowserView {
    /// Game strip (LB / RB).
    // The browser's spawn_view / details panel is written but not wired yet (docs/UI.md): kept on purpose.
    #[allow(dead_code)]
    pub tabs: Vec<Tab>,
    /// Where you are: `FH2`, `Ferrari`.
    pub crumbs: Vec<String>,
    /// Filters, sort, position.
    pub status: String,
    /// Details panel of the selected row: title and (key, value) lines.
    // The browser's spawn_view / details panel is written but not wired yet (docs/UI.md): kept on purpose.
    #[allow(dead_code)]
    pub details: Option<(String, Vec<(String, String)>)>,
    pub hints: Vec<Hint>,
}

impl BrowserView {
    /// `FH2 › Ferrari  ·  Class: all ...  ·  3 / 24` (one line, for text-only callers).
    pub fn title(&self) -> String {
        let c = self.crumbs.join(" › ");
        if self.status.is_empty() { c } else { format!("{c}  ·  {}", self.status) }
    }

    /// `A Enter  select      B Bksp  back ...` (one line).
    pub fn hint_line(&self) -> String {
        self.hints.iter().map(|h| format!("{} {}  {}", h.pads.join("/"), h.keys, h.action)).collect::<Vec<_>>().join("      ")
    }
}

/// Move a cursor over `n` rows: single presses wrap, held repeats and pages stop at the ends.
fn move_cursor(cursor: usize, delta: i32, n: usize, wrap: bool) -> usize {
    if n == 0 {
        return 0;
    }
    let c = cursor as i64 + delta as i64;
    if wrap && delta.abs() == 1 { c.rem_euclid(n as i64) as usize } else { c.clamp(0, n as i64 - 1) as usize }
}

/// Start of the next (`dir` > 0) or previous group of equal keys around `cursor` (wrapping).
fn group_jump<K: PartialEq>(keys: &[K], cursor: usize, dir: i32) -> usize {
    let n = keys.len();
    if n == 0 {
        return 0;
    }
    let cursor = cursor.min(n - 1);
    let start_of = |mut i: usize| {
        while i > 0 && keys[i - 1] == keys[i] {
            i -= 1;
        }
        i
    };
    if dir > 0 {
        (cursor + 1..n).find(|&i| keys[i] != keys[cursor]).unwrap_or(0)
    } else {
        let s = start_of(cursor);
        if s == 0 { start_of(n - 1) } else { start_of(s - 1) }
    }
}

fn initial(s: &str) -> char {
    s.chars().find(|c| c.is_alphanumeric()).map_or(' ', |c| c.to_ascii_uppercase())
}

fn opt_label(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("all")
}

/// Next value of a filter cycling all -> each option -> all.
fn cycle(cur: &Option<String>, options: &[String]) -> Option<String> {
    match cur.as_ref().and_then(|c| options.iter().position(|o| o == c)) {
        None => options.first().cloned(),
        Some(i) => options.get(i + 1).cloned(),
    }
}

/// `D–S2` from a list of classes.
fn class_span(classes: impl Iterator<Item = String>) -> String {
    let mut v: Vec<String> = classes.collect();
    v.sort_by_key(|c| class_rank(c));
    v.dedup();
    match (v.first(), v.last()) {
        (Some(a), Some(b)) if a != b => format!("{a}–{b}"),
        (Some(a), _) => a.clone(),
        _ => String::new(),
    }
}

fn year_span(years: impl Iterator<Item = i64>) -> Option<String> {
    let v: Vec<i64> = years.collect();
    match (v.iter().min(), v.iter().max()) {
        (Some(a), Some(b)) if a != b => Some(format!("{a}–{b}")),
        (Some(a), _) => Some(a.to_string()),
        _ => None,
    }
}

/// The car browser's state: Game -> Manufacturer -> car. Reusable by any menu: build it with [`CarBrowser::new`], feed
/// [`CarBrowser::step`] / [`CarBrowser::click`] (input from [`read_input`]), draw [`CarBrowser::rows`] and
/// [`CarBrowser::view`] with [`spawn_view`].
#[derive(Clone, Debug)]
pub struct CarBrowser {
    pub catalog: std::sync::Arc<CarCatalog>,
    /// 0 = games, 1 = manufacturers, 2 = cars.
    pub level: u8,
    pub game: usize,
    /// Manufacturer of the car list; `None` = every manufacturer.
    pub maker: Option<String>,
    /// Class filter (`None` = every class).
    pub class: Option<String>,
    /// Drive filter (`FWD` / `RWD` / `AWD`; `None` = all).
    pub drive: Option<String>,
    pub sort: Sort,
    pub cursor: usize,
    /// Garage index of the car in use (marked "current").
    pub current: usize,
    /// Last manufacturer per game, and last car (garage index) per (game, manufacturer or "*").
    mem_maker: HashMap<String, Option<String>>,
    mem_car: HashMap<(String, String), usize>,
    /// Per garage index: a (label, value) shown after the row's value and in the details panel (garage / autoshow:
    /// price, owned, sell value; ui/garage.rs).
    pub tags: std::sync::Arc<HashMap<usize, (String, String)>>,
}

impl CarBrowser {
    /// Opens on the `current` car's car list, cursor on it (or the game list when it isn't catalogued).
    pub fn new(catalog: std::sync::Arc<CarCatalog>, current: usize) -> Self {
        let mut b = Self {
            catalog,
            level: 0,
            game: 0,
            maker: None,
            class: None,
            drive: None,
            sort: Sort::Name,
            cursor: 0,
            current,
            mem_maker: HashMap::new(),
            mem_car: HashMap::new(),
            tags: Default::default(),
        };
        let cat = b.catalog.clone();
        if let Some(e) = cat.entry_of(current) {
            b.game = cat.games.iter().position(|g| *g == e.game).unwrap_or(0);
            b.maker = Some(e.maker.clone());
            b.mem_maker.insert(e.game.clone(), b.maker.clone());
            b.enter_cars();
        } else if cat.games.len() == 1 {
            // A one-game list without the current car (the autoshow): open on its manufacturers.
            b.enter_makers();
        }
        b
    }

    /// The car tags (see [`CarBrowser::tags`]).
    pub fn with_tags(mut self, tags: HashMap<usize, (String, String)>) -> Self {
        self.tags = std::sync::Arc::new(tags);
        self
    }

    fn tag(&self, index: usize) -> String {
        self.tags.get(&index).map(|t| format!("  ·  {}", t.1)).unwrap_or_default()
    }

    fn game_name(&self) -> String {
        self.catalog.games.get(self.game).cloned().unwrap_or_default()
    }

    /// Requirement of the current game when it isn't imported (greyed tab).
    fn game_locked(&self) -> Option<String> {
        self.catalog.locked.get(&self.game_name()).cloned()
    }

    /// Row `row` of the current level is greyed (a game that isn't imported, or its message row).
    pub fn locked_row(&self, row: usize) -> bool {
        match self.level {
            0 => self.catalog.games.get(row).is_some_and(|g| self.catalog.locked.contains_key(g)),
            1 => self.game_locked().is_some(),
            _ => false,
        }
    }

    #[allow(dead_code)]
    pub fn class_filter(&self) -> Option<String> {
        self.class.clone()
    }

    fn makers(&self) -> Vec<(String, usize)> {
        self.catalog.makers_by(&self.game_name(), self.class.as_deref(), self.drive.as_deref())
    }

    fn car_list(&self) -> Vec<usize> {
        self.catalog.cars_by(&self.game_name(), self.maker.as_deref(), self.class.as_deref(), self.drive.as_deref(), self.sort)
    }

    fn maker_key(&self) -> (String, String) {
        (self.game_name(), self.maker.clone().unwrap_or_else(|| "*".into()))
    }

    pub fn len(&self) -> usize {
        match self.level {
            0 => self.catalog.games.len(),
            1 => self.makers().len() + 1,
            _ => self.car_list().len(),
        }
    }

    /// Catalog entry under the cursor (car level).
    pub fn selected(&self) -> Option<&CarEntry> {
        if self.level != 2 {
            return None;
        }
        self.car_list().get(self.cursor).map(|&i| &self.catalog.entries[i])
    }

    /// Manufacturer under the cursor on the brand level (`None` = "All manufacturers").
    fn cursor_maker(&self) -> Option<String> {
        self.cursor.checked_sub(1).and_then(|i| self.makers().get(i).map(|m| m.0.clone()))
    }

    /// Store the cursor's car / manufacturer so coming back lands on it.
    fn remember(&mut self) {
        match self.level {
            2 => {
                if let Some(idx) = self.selected().map(|e| e.index) {
                    let k = self.maker_key();
                    self.mem_car.insert(k, idx);
                }
                self.mem_maker.insert(self.game_name(), self.maker.clone());
            }
            1 => {
                let m = self.cursor_maker();
                self.mem_maker.insert(self.game_name(), m);
            }
            _ => {}
        }
    }

    /// Manufacturer list of the current game, cursor on the remembered manufacturer (else the current car's).
    fn enter_makers(&mut self) {
        self.level = 1;
        let game = self.game_name();
        let want = match self.mem_maker.get(&game) {
            Some(m) => m.clone(),
            None => self.catalog.entry_of(self.current).filter(|e| e.game == game).map(|e| e.maker.clone()),
        };
        self.cursor = want.and_then(|m| self.makers().iter().position(|x| x.0 == m)).map_or(0, |i| i + 1);
    }

    /// Car list of `maker`, cursor on the remembered car (else the current car).
    fn enter_cars(&mut self) {
        self.level = 2;
        let list = self.car_list();
        let want = self.mem_car.get(&self.maker_key()).copied().unwrap_or(self.current);
        self.cursor = list.iter().position(|&i| self.catalog.entries[i].index == want).unwrap_or(0);
    }

    /// Change filters / sort keeping the selected car (or manufacturer) under the cursor when it is still listed.
    fn refilter(&mut self, f: impl FnOnce(&mut Self)) {
        let keep_car = self.selected().map(|e| e.index);
        let keep_maker = if self.level == 1 { self.cursor_maker() } else { None };
        f(self);
        if self.level == 2 {
            if let Some(m) = self.maker.clone() {
                if !self.makers().iter().any(|x| x.0 == m) {
                    // That manufacturer has no car left under the filters: list the whole game.
                    self.maker = None;
                }
            }
            let list = self.car_list();
            self.cursor = keep_car.and_then(|c| list.iter().position(|&i| self.catalog.entries[i].index == c)).unwrap_or(0);
        } else if self.level == 1 {
            self.cursor = keep_maker.and_then(|m| self.makers().iter().position(|x| x.0 == m)).map_or(0, |i| i + 1);
        }
    }

    /// Switch to game `g` (filters kept when the game has those values): its manufacturer list.
    fn switch_game(&mut self, g: usize) {
        self.remember();
        self.game = g;
        let game = self.game_name();
        if self.class.as_ref().is_some_and(|c| !self.catalog.classes(&game).contains(c)) {
            self.class = None;
        }
        if self.drive.as_ref().is_some_and(|d| !self.catalog.drives(&game).contains(d)) {
            self.drive = None;
        }
        self.enter_makers();
    }

    /// Every row of the current level.
    pub fn rows(&self) -> Vec<BrowserRow> {
        let cat = &self.catalog;
        let current = cat.entry_of(self.current);
        let game = self.game_name();
        let here = |b: bool| if b { "  ·  current" } else { "" };
        match self.level {
            0 => cat
                .games
                .iter()
                .map(|g| {
                    if let Some(reason) = cat.locked.get(g) {
                        return BrowserRow { label: g.clone(), value: Some(reason.clone()), locked: true };
                    }
                    let n = cat.entries.iter().filter(|e| e.game == *g).count();
                    let brands = cat.makers(g, None).len();
                    BrowserRow { label: format!("{g}  ({n})"), value: Some(format!("{brands} brands{}", here(current.is_some_and(|c| c.game == *g)))), locked: false }
                })
                .collect(),
            // A game that isn't imported: its message instead of cars.
            1 if self.game_locked().is_some() => vec![BrowserRow { label: "Not imported".into(), value: self.game_locked(), locked: true }],
            1 => {
                let makers = self.makers();
                let total: usize = makers.iter().map(|m| m.1).sum();
                std::iter::once(BrowserRow { label: format!("All manufacturers  ({total})"), value: None, locked: false })
                    .chain(makers.iter().map(|(m, n)| {
                        let span = class_span(
                            cat.entries
                                .iter()
                                .filter(|e| e.game == game && e.maker == *m && passes(e, self.class.as_deref(), self.drive.as_deref()))
                                .filter_map(|e| e.class.clone()),
                        );
                        BrowserRow { label: format!("{m}  ({n})"), value: Some(format!("{span}{}", here(current.is_some_and(|c| c.game == game && c.maker == *m)))), locked: false }
                    }))
                    .collect()
            }
            _ => {
                let all = self.maker.is_none();
                self.car_list()
                    .into_iter()
                    .map(|i| {
                        let e = &cat.entries[i];
                        BrowserRow {
                            label: if all { format!("{} {}", e.maker, e.name) } else { e.name.clone() },
                            value: Some(format!("{}{}{}", e.detail(), self.tag(e.index), here(e.index == self.current))),
                            locked: false,
                        }
                    })
                    .collect()
            }
        }
    }

    /// One frame of input.
    pub fn step(&mut self, input: BrowserInput) -> Pick {
        let games = self.catalog.games.len();
        if input.back {
            match self.level {
                0 => return Pick::Leave,
                1 => {
                    self.remember();
                    (self.level, self.cursor) = (0, self.game);
                }
                _ => {
                    self.remember();
                    self.enter_makers();
                }
            }
            return Pick::Browsing { changed: true };
        }
        if input.tab != 0 && games > 0 {
            let base = if self.level == 0 { self.cursor } else { self.game };
            self.switch_game((base as i32 + input.tab).rem_euclid(games as i32) as usize);
            return Pick::Browsing { changed: true };
        }
        // A game that isn't imported shows only its message: no filters, sorts or jumps there.
        if self.level > 0 && self.game_locked().is_some() && (input.filter || input.drive || input.sort || input.jump != 0) {
            return Pick::Browsing { changed: false };
        }
        if input.filter && self.level > 0 {
            let classes = self.catalog.classes(&self.game_name());
            self.refilter(|b| b.class = cycle(&b.class, &classes));
            return Pick::Browsing { changed: true };
        }
        if input.drive && self.level > 0 {
            let drives = self.catalog.drives(&self.game_name());
            self.refilter(|b| b.drive = cycle(&b.drive, &drives));
            return Pick::Browsing { changed: true };
        }
        if input.sort && self.level == 2 {
            self.refilter(|b| b.sort = b.sort.next());
            return Pick::Browsing { changed: true };
        }
        if input.jump != 0 {
            match self.level {
                0 => self.cursor = move_cursor(self.cursor, input.jump, games, true),
                1 => {
                    // Row 0 ("All") is its own group; then one group per initial letter.
                    let keys: Vec<char> = std::iter::once('\0').chain(self.makers().iter().map(|m| initial(&m.0))).collect();
                    self.cursor = group_jump(&keys, self.cursor, input.jump);
                }
                _ => match self.maker.clone() {
                    Some(m) => {
                        // Previous / next manufacturer, staying on the car list.
                        let makers = self.makers();
                        if let Some(i) = makers.iter().position(|x| x.0 == m) {
                            self.remember();
                            self.maker = Some(makers[(i as i32 + input.jump).rem_euclid(makers.len() as i32) as usize].0.clone());
                            self.mem_maker.insert(self.game_name(), self.maker.clone());
                            self.enter_cars();
                        }
                    }
                    // All manufacturers by name: jump to the next / previous manufacturer's block.
                    None if self.sort == Sort::Name => {
                        let keys: Vec<String> = self.car_list().iter().map(|&i| self.catalog.entries[i].maker.clone()).collect();
                        self.cursor = group_jump(&keys, self.cursor, input.jump);
                    }
                    None => self.cursor = move_cursor(self.cursor, input.jump * PAGE as i32, self.len(), false),
                },
            }
            return Pick::Browsing { changed: true };
        }
        let mut changed = false;
        let n = self.len();
        if input.horizontal != 0 {
            self.cursor = move_cursor(self.cursor, input.horizontal * PAGE as i32, n, false);
            changed = true;
        }
        if input.vertical != 0 {
            self.cursor = move_cursor(self.cursor, input.vertical, n, !input.held);
            changed = true;
        }
        if input.confirm {
            return self.click(self.cursor);
        }
        Pick::Browsing { changed }
    }

    /// Activate row `row` (mouse click, or the cursor's row).
    pub fn click(&mut self, row: usize) -> Pick {
        match self.level {
            0 if row < self.catalog.games.len() => {
                self.cursor = row;
                self.switch_game(row);
                Pick::Browsing { changed: true }
            }
            // The message row of a game that isn't imported.
            1 if self.game_locked().is_some() => Pick::Browsing { changed: false },
            1 if row < self.len() => {
                self.maker = if row == 0 { None } else { self.makers().get(row - 1).map(|m| m.0.clone()) };
                self.mem_maker.insert(self.game_name(), self.maker.clone());
                self.enter_cars();
                Pick::Browsing { changed: true }
            }
            2 => match self.car_list().get(row) {
                Some(&i) => {
                    self.cursor = row;
                    self.remember();
                    Pick::Car(self.catalog.entries[i].index)
                }
                None => Pick::Browsing { changed: false },
            },
            _ => Pick::Browsing { changed: false },
        }
    }

    /// Tabs, breadcrumb, status, details and button hints of the current level.
    pub fn view(&self) -> BrowserView {
        let cat = &self.catalog;
        let n = self.len();
        let pos = format!("{} / {n}", (self.cursor + 1).min(n));
        let game = self.game_name();
        let tab_on = if self.level == 0 { self.cursor } else { self.game };
        let tabs = cat.games.iter().enumerate().map(|(i, g)| Tab { label: g.clone(), count: cat.entries.iter().filter(|e| e.game == *g).count(), active: i == tab_on }).collect();
        let filters = format!("Class: {}    Drive: {}", opt_label(&self.class), opt_label(&self.drive));
        let back = |top: bool| hint(&["B"], "Bksp", if top { "close" } else { "back" });
        let common = [hint(&["LB", "RB"], "Q E", "game"), hint(&["X"], "F", "class"), hint(&["View"], "G", "drive"), hint(&["D-pad ‹ ›"], "‹ ›", "page")];
        let (crumbs, status, details, hints) = match self.level {
            0 => {
                let g = cat.games.get(self.cursor).cloned().unwrap_or_default();
                let details = (!g.is_empty()).then(|| {
                    if let Some(reason) = cat.locked.get(&g) {
                        return (g.clone(), vec![(String::new(), reason.clone())]);
                    }
                    let cars: Vec<&CarEntry> = cat.entries.iter().filter(|e| e.game == g).collect();
                    let mut lines = vec![("Cars".to_string(), cars.len().to_string()), ("Brands".to_string(), cat.makers(&g, None).len().to_string())];
                    lines.push(("Classes".into(), class_span(cat.classes(&g).into_iter())));
                    if let Some(y) = year_span(cars.iter().filter_map(|e| e.year)) {
                        lines.push(("Years".into(), y));
                    }
                    (g.clone(), lines)
                });
                (vec!["Games".to_string()], pos, details, vec![hint(&["A"], "Enter", "open"), back(true), hint(&["LB", "RB"], "Q E", "game")])
            }
            1 => {
                let sel = self.cursor_maker();
                let cars: Vec<&CarEntry> = cat
                    .entries
                    .iter()
                    .filter(|e| e.game == game && sel.as_ref().is_none_or(|m| e.maker == *m) && passes(e, self.class.as_deref(), self.drive.as_deref()))
                    .collect();
                let mut lines = vec![("Cars".to_string(), cars.len().to_string()), ("Classes".to_string(), class_span(cars.iter().filter_map(|e| e.class.clone())))];
                if let Some(y) = year_span(cars.iter().filter_map(|e| e.year)) {
                    lines.push(("Years".into(), y));
                }
                let title = sel.unwrap_or_else(|| "All manufacturers".into());
                if let Some(reason) = self.game_locked() {
                    // A game that isn't imported: its message, and only the moves that leave it.
                    let h = vec![back(false), hint(&["LB", "RB"], "Q E", "game")];
                    return BrowserView { tabs, crumbs: vec![game.clone()], status: String::new(), details: Some((game.clone(), vec![(String::new(), reason)])), hints: h };
                }
                let mut h = vec![hint(&["A"], "Enter", "open"), back(false), hint(&["LT", "RT"], "Z C", "letter")];
                h.extend(common.iter().cloned());
                (vec![game.clone()], format!("{filters}    ·    {pos}"), Some((title, lines)), h)
            }
            _ => {
                let maker = self.maker.clone().unwrap_or_else(|| "All manufacturers".into());
                let details = self.selected().map(|e| {
                    let mut lines = vec![("Game".to_string(), e.game.clone())];
                    if let Some(y) = e.year {
                        lines.push(("Year".into(), y.to_string()));
                    }
                    match (&e.class, e.pi) {
                        (Some(c), Some(p)) => lines.push(("Class".into(), format!("{c}  {p}"))),
                        (Some(c), None) => lines.push(("Class".into(), c.clone())),
                        _ => {}
                    }
                    if !e.drive.is_empty() {
                        lines.push(("Drive".into(), e.drive.clone()));
                    }
                    let i = cat.entries.iter().position(|x| x.index == e.index).unwrap_or(usize::MAX);
                    if let Some(s) = cat.stats(i) {
                        if let Some(w) = s.power_w {
                            lines.push(("Power".into(), format!("{:.0} hp  ({:.0} kW)", w / 745.7, w / 1000.0)));
                        }
                        if let Some(m) = s.mass_kg {
                            lines.push(("Weight".into(), format!("{m:.0} kg")));
                        }
                        if let Some(t) = s.zero_60 {
                            lines.push(("0-60 mph".into(), format!("{t:.1} s")));
                        }
                        if let Some(v) = s.top_mph {
                            lines.push(("Top speed".into(), format!("{v:.0} mph")));
                        }
                    }
                    if let Some((k, v)) = self.tags.get(&e.index) {
                        lines.push((k.clone(), v.clone()));
                    }
                    if e.index == self.current {
                        lines.push((String::new(), "Current car".into()));
                    }
                    (format!("{} {}", e.maker, e.name), lines)
                });
                let jump = if self.maker.is_some() || self.sort == Sort::Name { "brand" } else { "page" };
                let mut h = vec![hint(&["A"], "Enter", "select"), back(false), hint(&["LT", "RT"], "Z C", jump), hint(&["Y"], "Tab", format!("sort: {}", self.sort.label()))];
                h.extend(common.iter().cloned());
                (vec![game.clone(), maker], format!("{filters}    Sort: {}    ·    {pos}", self.sort.label()), details, h)
            }
        };
        BrowserView { tabs, crumbs, status, details, hints }
    }

    /// `FH2 › Lamborghini  ·  Class: all    Drive: all    Sort: name    ·    3 / 12`.
    pub fn title(&self) -> String {
        self.view().title()
    }

    pub fn hint(&self) -> String {
        self.view().hint_line()
    }
}

/// The map browser: Game -> (track ->) map. `groups` = [`crate::track::Track::maps_by_game`], or groups labelled
/// `"<game>  ·  <track>"` (FM4 tracks with their layouts): a game with several groups gets a track level.
#[derive(Clone, Debug, Default)]
pub struct MapBrowser {
    pub groups: Vec<(String, Vec<(String, String)>)>,
    /// 0 = games, 1 = tracks of `game` (games with several groups only), 2 = maps of `group`.
    pub level: u8,
    /// Index into [`MapBrowser::games`].
    pub game: usize,
    /// Index into `groups`.
    pub group: usize,
    pub cursor: usize,
    /// Map in use (marked "current").
    pub current: String,
    /// Last group per game.
    mem_group: HashMap<String, usize>,
    /// Greyed maps (games that aren't imported, [`MapBrowser::with_locked`]): id -> requirement.
    locked: HashMap<String, String>,
}

/// Separator of `"<game>  ·  <track>"` group labels.
pub const GROUP_SEP: &str = "  ·  ";

fn group_game(label: &str) -> &str {
    label.split(GROUP_SEP).next().unwrap_or(label)
}

fn group_track(label: &str) -> &str {
    label.split_once(GROUP_SEP).map_or(label, |x| x.1)
}

impl MapBrowser {
    /// Opens on `current`'s map list (cursor on it).
    pub fn new(groups: Vec<(String, Vec<(String, String)>)>, current: &str) -> Self {
        let mut b = Self { groups, level: 2, current: current.to_owned(), ..Default::default() };
        if let Some((g, k)) = b.groups.iter().enumerate().find_map(|(g, (_, m))| m.iter().position(|x| x.0 == current).map(|k| (g, k))) {
            (b.group, b.cursor) = (g, k);
        }
        let gname = b.groups.get(b.group).map(|g| group_game(&g.0).to_owned()).unwrap_or_default();
        b.game = b.games().iter().position(|x| *x == gname).unwrap_or(0);
        b.mem_group.insert(b.game_name(), b.group);
        b
    }

    /// Adds greyed rows that can't be picked (crate::track::locked_maps): each joins its game's group, or a new group
    /// after the others.
    pub fn with_locked(mut self, locked: impl IntoIterator<Item = crate::track::LockedMap>) -> Self {
        for l in locked {
            match self.groups.iter_mut().find(|g| g.0 == l.game) {
                Some(g) => g.1.push((l.id.clone(), l.name)),
                None => self.groups.push((l.game, vec![(l.id.clone(), l.name)])),
            }
            self.locked.insert(l.id, l.reason);
        }
        self
    }

    /// Requirement of a greyed map.
    fn map_locked(&self, id: &str) -> Option<&String> {
        self.locked.get(id)
    }

    /// Requirement shared by every map of group `i` (the whole group is greyed), else `None`.
    fn group_locked(&self, i: usize) -> Option<&String> {
        let maps = &self.groups.get(i)?.1;
        if maps.iter().all(|m| self.locked.contains_key(&m.0)) { maps.first().and_then(|m| self.map_locked(&m.0)) } else { None }
    }

    /// Requirement of game `game` when all of its maps are greyed.
    fn game_locked(&self, game: &str) -> Option<&String> {
        let gs: Vec<usize> = (0..self.groups.len()).filter(|&i| group_game(&self.groups[i].0) == game).collect();
        if gs.iter().all(|&i| self.group_locked(i).is_some()) { gs.first().and_then(|&i| self.group_locked(i)) } else { None }
    }

    /// Row `row` of the current level is greyed.
    pub fn locked_row(&self, row: usize) -> bool {
        match self.level {
            0 => self.games().get(row).is_some_and(|g| self.game_locked(g).is_some()),
            1 => self.game_groups().get(row).is_some_and(|&i| self.group_locked(i).is_some()),
            _ => self.groups.get(self.group).and_then(|g| g.1.get(row)).is_some_and(|m| self.locked.contains_key(&m.0)),
        }
    }

    /// Show the game list instead (when there is more than one game), cursor on the current game.
    pub fn show_games(&mut self) {
        if self.games().len() > 1 {
            (self.level, self.cursor) = (0, self.game);
        }
    }

    /// Games in group order.
    pub fn games(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for (g, _) in &self.groups {
            let k = group_game(g);
            if !v.iter().any(|x| x == k) {
                v.push(k.to_owned());
            }
        }
        v
    }

    fn game_name(&self) -> String {
        self.games().get(self.game).cloned().unwrap_or_default()
    }

    /// Group indices of the current game.
    fn game_groups(&self) -> Vec<usize> {
        let g = self.game_name();
        (0..self.groups.len()).filter(|&i| group_game(&self.groups[i].0) == g).collect()
    }

    fn single_game(&self) -> bool {
        self.games().len() <= 1
    }

    pub fn len(&self) -> usize {
        match self.level {
            0 => self.games().len(),
            1 => self.game_groups().len(),
            _ => self.groups.get(self.group).map_or(0, |g| g.1.len()),
        }
    }

    /// Opens game `g`: its track list, or straight its maps when it has one group.
    fn enter_game(&mut self, g: usize) {
        if self.level == 2 {
            self.mem_group.insert(self.game_name(), self.group);
        }
        self.game = g;
        let groups = self.game_groups();
        let remembered = self.mem_group.get(&self.game_name()).copied().filter(|x| groups.contains(x));
        if groups.len() == 1 {
            self.enter_group(groups[0]);
        } else {
            self.level = 1;
            self.cursor = remembered.and_then(|r| groups.iter().position(|&x| x == r)).unwrap_or(0);
        }
    }

    fn enter_group(&mut self, group: usize) {
        self.group = group;
        self.level = 2;
        self.mem_group.insert(self.game_name(), group);
        self.cursor = self.groups.get(group).and_then(|g| g.1.iter().position(|m| m.0 == self.current)).unwrap_or(0);
    }

    pub fn rows(&self) -> Vec<BrowserRow> {
        let has_current = |i: usize| self.groups[i].1.iter().any(|x| x.0 == self.current);
        match self.level {
            0 => self
                .games()
                .iter()
                .map(|g| {
                    if let Some(reason) = self.game_locked(g) {
                        return BrowserRow { label: g.clone(), value: Some(reason.clone()), locked: true };
                    }
                    let gs: Vec<usize> = (0..self.groups.len()).filter(|&i| group_game(&self.groups[i].0) == g).collect();
                    let maps: usize = gs.iter().map(|&i| self.groups[i].1.len()).sum();
                    let mut value = Vec::new();
                    if gs.len() > 1 {
                        value.push(format!("{} tracks", gs.len()));
                    }
                    if gs.iter().any(|&i| has_current(i)) {
                        value.push("current".to_string());
                    }
                    BrowserRow { label: format!("{g}  ({maps})"), value: (!value.is_empty()).then(|| value.join("  ·  ")), locked: false }
                })
                .collect(),
            1 => self
                .game_groups()
                .iter()
                .map(|&i| match self.group_locked(i) {
                    Some(reason) => BrowserRow { label: group_track(&self.groups[i].0).to_owned(), value: Some(reason.clone()), locked: true },
                    None => BrowserRow { label: format!("{}  ({})", group_track(&self.groups[i].0), self.groups[i].1.len()), value: has_current(i).then(|| "current".into()), locked: false },
                })
                .collect(),
            _ => self.groups.get(self.group).map_or(Vec::new(), |g| {
                g.1.iter()
                    .map(|(id, name)| match self.map_locked(id) {
                        Some(reason) => BrowserRow { label: name.trim().to_owned(), value: Some(reason.clone()), locked: true },
                        None => BrowserRow { label: name.trim().to_owned(), value: (*id == self.current).then(|| "current".into()), locked: false },
                    })
                    .collect()
            }),
        }
    }

    pub fn step(&mut self, input: BrowserInput) -> Pick {
        let games = self.games().len();
        if input.back {
            let multi = self.game_groups().len() > 1;
            match self.level {
                2 if multi => {
                    let groups = self.game_groups();
                    self.level = 1;
                    self.cursor = groups.iter().position(|&x| x == self.group).unwrap_or(0);
                }
                0 => return Pick::Leave,
                _ if self.single_game() => return Pick::Leave,
                _ => {
                    if self.level == 2 {
                        self.mem_group.insert(self.game_name(), self.group);
                    }
                    (self.level, self.cursor) = (0, self.game);
                }
            }
            return Pick::Browsing { changed: true };
        }
        if input.tab != 0 && games > 1 {
            let base = if self.level == 0 { self.cursor } else { self.game };
            self.enter_game((base as i32 + input.tab).rem_euclid(games as i32) as usize);
            return Pick::Browsing { changed: true };
        }
        if input.jump != 0 {
            match self.level {
                0 => self.cursor = move_cursor(self.cursor, input.jump, games, true),
                1 => {
                    let keys: Vec<char> = self.game_groups().iter().map(|&i| initial(group_track(&self.groups[i].0))).collect();
                    self.cursor = group_jump(&keys, self.cursor, input.jump);
                }
                _ => {
                    // Previous / next track of the game, staying on the layout list.
                    let groups = self.game_groups();
                    if let Some(i) = groups.iter().position(|&x| x == self.group) {
                        self.enter_group(groups[(i as i32 + input.jump).rem_euclid(groups.len() as i32) as usize]);
                    }
                }
            }
            return Pick::Browsing { changed: true };
        }
        let mut changed = false;
        let n = self.len();
        if input.horizontal != 0 {
            self.cursor = move_cursor(self.cursor, input.horizontal * PAGE as i32, n, false);
            changed = true;
        }
        if input.vertical != 0 {
            self.cursor = move_cursor(self.cursor, input.vertical, n, !input.held);
            changed = true;
        }
        if input.confirm {
            return self.click(self.cursor);
        }
        Pick::Browsing { changed }
    }

    pub fn click(&mut self, row: usize) -> Pick {
        match self.level {
            0 if row < self.games().len() => {
                self.enter_game(row);
                Pick::Browsing { changed: true }
            }
            1 => match self.game_groups().get(row) {
                Some(&g) => {
                    self.enter_group(g);
                    Pick::Browsing { changed: true }
                }
                None => Pick::Browsing { changed: false },
            },
            // A greyed map (game not imported) can't be picked.
            2 if self.locked_row(row) => Pick::Browsing { changed: false },
            2 => self.groups.get(self.group).and_then(|g| g.1.get(row)).map_or(Pick::Browsing { changed: false }, |m| Pick::Map(m.0.clone())),
            _ => Pick::Browsing { changed: false },
        }
    }

    pub fn view(&self) -> BrowserView {
        let games = self.games();
        let n = self.len();
        let pos = format!("{} / {n}", (self.cursor + 1).min(n));
        let tab_on = if self.level == 0 { self.cursor } else { self.game };
        let tabs = if games.len() > 1 {
            games
                .iter()
                .enumerate()
                .map(|(i, g)| Tab { label: g.clone(), count: self.groups.iter().filter(|x| group_game(&x.0) == g).map(|x| x.1.len()).sum(), active: i == tab_on })
                .collect()
        } else {
            Vec::new()
        };
        let multi = self.game_groups().len() > 1;
        let top = self.level == 0 || (self.single_game() && (self.level == 1 || !multi));
        let mut h = vec![hint(&["A"], "Enter", if self.level == 2 { "load" } else { "open" }), hint(&["B"], "Bksp", if top { "close" } else { "back" })];
        if games.len() > 1 {
            h.push(hint(&["LB", "RB"], "Q E", "game"));
        }
        if self.level == 1 {
            h.push(hint(&["LT", "RT"], "Z C", "letter"));
        } else if self.level == 2 && multi {
            h.push(hint(&["LT", "RT"], "Z C", "track"));
        }
        if n > PAGE {
            h.push(hint(&["D-pad ‹ ›"], "‹ ›", "page"));
        }
        let mut crumbs = Vec::new();
        if self.level == 0 {
            crumbs.push("Games".to_string());
        } else {
            crumbs.push(self.game_name());
            if self.level == 2 && multi {
                crumbs.push(self.groups.get(self.group).map_or(String::new(), |g| group_track(&g.0).to_owned()));
            }
        }
        BrowserView { tabs, crumbs, status: pos, details: None, hints: h }
    }

    pub fn title(&self) -> String {
        self.view().title()
    }

    pub fn hint(&self) -> String {
        self.view().hint_line()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Drawing: the same layout for the pause menu (ui.rs) and the main menu (ui/launch.rs).
// ---------------------------------------------------------------------------------------------------------------

/// First row of a `visible`-row window around `cursor` (the pause menu's window formula).
#[allow(dead_code)]
pub fn window_first(cursor: usize, n: usize, visible: usize) -> usize {
    cursor.saturating_sub(visible / 2).min(n.saturating_sub(visible))
}

/// Xbox face-button colours for the glyph chips.
#[allow(dead_code)]
fn glyph_color(g: &str) -> Color {
    match g {
        "A" => Color::srgb(0.30, 0.66, 0.18),
        "B" => Color::srgb(0.82, 0.18, 0.16),
        "X" => Color::srgb(0.16, 0.42, 0.86),
        "Y" => Color::srgb(0.90, 0.68, 0.06),
        _ => Color::srgb(0.30, 0.32, 0.36),
    }
}

/// Spawns a browser screen under `parent`: game tabs (LB / RB), breadcrumb + status, the row list windowed around
/// `cursor` beside a details panel, and the button-hint bar. `row_hook(entity, shown_index)` decorates each row (the
/// pause menu adds its mouse components). `fade` = the main menu's overlay fade components.
// Written but not wired yet (docs/UI.md: the details panel / tabs view): kept on purpose.
#[allow(clippy::too_many_arguments, dead_code)]
pub fn spawn_view(
    commands: &mut Commands,
    parent: Entity,
    font: &UiFont,
    view: &BrowserView,
    rows: &[BrowserRow],
    cursor: usize,
    visible: usize,
    fade: bool,
    row_hook: &mut dyn FnMut(&mut EntityCommands, usize),
) {
    let text = |commands: &mut Commands, parent: Entity, s: String, px: f32, color: Color, node: Node| {
        let mut e = commands.spawn((ChildOf(parent), Text::new(s), font.text(px), TextColor(color), node));
        if fade {
            e.insert(Fade(color.alpha()));
        }
    };
    let boxed = |commands: &mut Commands, parent: Entity, node: Node, bg: Color| {
        let mut e = commands.spawn((ChildOf(parent), node, BackgroundColor(bg)));
        if fade {
            e.insert(FadeBg(bg.alpha()));
        }
        e.id()
    };
    let row_node = |gap: f32| Node { flex_direction: FlexDirection::Row, align_items: AlignItems::Center, column_gap: Val::Px(gap), flex_wrap: FlexWrap::Wrap, ..default() };
    let chip = |commands: &mut Commands, parent: Entity, label: &str, bg: Color| {
        let c = boxed(commands, parent, Node { padding: UiRect::axes(Val::Px(7.0), Val::Px(1.0)), min_width: Val::Px(24.0), justify_content: JustifyContent::Center, ..default() }, bg);
        text(commands, c, label.to_owned(), 14.0, Color::WHITE, Node::default());
    };

    // Game tabs.
    if !view.tabs.is_empty() {
        let strip = commands.spawn((ChildOf(parent), Node { margin: UiRect::bottom(Val::Px(10.0)), row_gap: Val::Px(6.0), ..row_node(6.0) })).id();
        chip(commands, strip, "LB", glyph_color("LB"));
        for t in &view.tabs {
            let bg = if t.active { ACCENT } else { Color::srgba(1.0, 1.0, 1.0, 0.08) };
            let c = boxed(commands, strip, Node { padding: UiRect::axes(Val::Px(14.0), Val::Px(5.0)), column_gap: Val::Px(8.0), align_items: AlignItems::Center, ..default() }, bg);
            text(commands, c, t.label.clone(), 20.0, Color::WHITE, Node::default());
            text(commands, c, t.count.to_string(), 14.0, if t.active { Color::srgba(1.0, 1.0, 1.0, 0.85) } else { DIM }, Node::default());
        }
        chip(commands, strip, "RB", glyph_color("RB"));
    }
    // Breadcrumb + status.
    text(commands, parent, view.crumbs.join("  ›  "), 26.0, Color::WHITE, Node::default());
    text(commands, parent, view.status.clone(), 15.0, DIM, Node { margin: UiRect::bottom(Val::Px(6.0)), ..default() });

    // Rows | details.
    let body = commands.spawn((ChildOf(parent), Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(18.0), align_items: AlignItems::FlexStart, ..default() })).id();
    let list = commands.spawn((ChildOf(body), Node { flex_direction: FlexDirection::Column, min_width: Val::Px(540.0), row_gap: Val::Px(2.0), ..default() })).id();
    let first = window_first(cursor, rows.len(), visible);
    let last = (first + visible).min(rows.len());
    let more = |n: usize, what: &str| if n > 0 { format!("{n} more {what}") } else { String::new() };
    let more_node = || Node { height: Val::Px(16.0), padding: UiRect::left(Val::Px(14.0)), ..default() };
    text(commands, list, more(first, "above"), 13.0, DIM, more_node());
    if rows.is_empty() {
        text(commands, list, "Nothing here with these filters".into(), 20.0, DIM, Node { padding: UiRect::all(Val::Px(14.0)), ..default() });
    }
    for (shown, k) in (first..last).enumerate() {
        let on = k == cursor;
        let r = boxed(
            commands,
            list,
            Node { height: Val::Px(38.0), padding: UiRect::horizontal(Val::Px(14.0)), column_gap: Val::Px(30.0), justify_content: JustifyContent::SpaceBetween, align_items: AlignItems::Center, ..default() },
            if on && rows[k].locked { LOCKED_CURSOR } else if on { ACCENT } else { Color::NONE },
        );
        let label_color = match (rows[k].locked, on) {
            (true, true) => LOCKED_ON,
            (true, false) => LOCKED,
            (false, true) => Color::WHITE,
            (false, false) => Color::srgba(1.0, 1.0, 1.0, 0.85),
        };
        text(commands, r, rows[k].label.clone(), 21.0, label_color, Node::default());
        if let Some(v) = &rows[k].value {
            text(commands, r, v.clone(), 17.0, if on { Color::WHITE } else { DIM }, Node::default());
        }
        row_hook(&mut commands.entity(r), shown);
    }
    text(commands, list, more(rows.len() - last, "below"), 13.0, DIM, more_node());
    if let Some((title, lines)) = &view.details {
        let d = boxed(
            commands,
            body,
            Node { flex_direction: FlexDirection::Column, width: Val::Px(300.0), padding: UiRect::all(Val::Px(14.0)), row_gap: Val::Px(6.0), margin: UiRect::top(Val::Px(18.0)), ..default() },
            Color::srgba(1.0, 1.0, 1.0, 0.06),
        );
        text(commands, d, title.clone(), 22.0, ACCENT, Node { margin: UiRect::bottom(Val::Px(6.0)), ..default() });
        for (k, v) in lines {
            let l = commands.spawn((ChildOf(d), Node { justify_content: JustifyContent::SpaceBetween, column_gap: Val::Px(12.0), ..default() })).id();
            text(commands, l, k.clone(), 16.0, DIM, Node::default());
            text(commands, l, v.clone(), 17.0, Color::WHITE, Node::default());
        }
    }

    // Button hints: [glyph] keys  action.
    let bar = commands.spawn((ChildOf(parent), Node { margin: UiRect::top(Val::Px(12.0)), row_gap: Val::Px(8.0), ..row_node(18.0) })).id();
    for h in &view.hints {
        let item = commands.spawn((ChildOf(bar), row_node(5.0))).id();
        for g in h.pads {
            chip(commands, item, g, glyph_color(g));
        }
        text(commands, item, h.keys.to_owned(), 13.0, DIM, Node::default());
        text(commands, item, h.action.clone(), 15.0, Color::WHITE, Node { margin: UiRect::left(Val::Px(3.0)), ..default() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_scale() {
        assert_eq!(fh1_display_pi(0.6505), 500);
        assert_eq!(fh1_display_pi(0.81), 700);
        // FER_250GTO_64: between C (0.5975 -> 400) and B (0.6505 -> 500).
        assert_eq!(fh1_display_pi(0.617993), 439);
        // Class F always 99; U (raw 1.0) 999.
        assert_eq!(fh1_display_pi(0.003), 99);
        assert_eq!(fh1_display_pi(1.0), 999);
    }

    fn car(index: usize, game: &str, maker: &str, name: &str, class: &str, pi: i64) -> CarEntry {
        CarEntry { index, game: game.into(), maker: maker.into(), name: name.into(), year: Some(2000), class: Some(class.into()), pi: Some(pi), drive: "RWD".into(), physics: None, id: None }
    }

    fn catalog() -> std::sync::Arc<CarCatalog> {
        let mut entries = vec![
            car(0, "FH1", "Alfa", "8C", "S", 700),
            car(1, "FH1", "BMW", "M3", "A", 600),
            car(2, "FH2", "Lamborghini", "Huracan", "S2", 925),
            car(3, "FH2", "Audi", "RS3", "A", 750),
            car(4, "FH2", "Audi", "R8", "S1", 840),
            car(5, "FH2", "Alfa", "4C", "A", 720),
            car(6, "FH2", "BMW", "M5", "S1", 800),
        ];
        entries[3].drive = "AWD".into();
        std::sync::Arc::new(CarCatalog { entries, games: vec!["FH1".into(), "FH2".into()], ..Default::default() })
    }

    fn key(f: impl FnOnce(&mut BrowserInput)) -> BrowserInput {
        let mut i = BrowserInput::default();
        f(&mut i);
        i
    }

    fn labels(b: &CarBrowser) -> Vec<String> {
        b.rows().into_iter().map(|r| r.label).collect()
    }

    /// Game -> maker -> car, class filter, back out, pick.
    #[test]
    fn car_browser_walk() {
        let mut b = CarBrowser::new(catalog(), 2);
        assert_eq!((b.level, b.game, b.cursor), (2, 1, 0));
        assert_eq!(b.view().crumbs, ["FH2", "Lamborghini"]);
        let back = key(|i| i.back = true);
        assert_eq!(b.step(back), Pick::Browsing { changed: true });
        // Makers: All, Alfa, Audi, BMW, Lamborghini; the cursor returns to Lamborghini.
        assert_eq!((b.level, b.cursor, b.len()), (1, 4, 5));
        assert_eq!(labels(&b)[2], "Audi  (2)");
        // Class filter (X) -> A: Alfa (4C) and Audi (RS3) left; Lamborghini is gone so the cursor falls back to "All".
        b.step(key(|i| i.filter = true));
        assert_eq!(b.class_filter().as_deref(), Some("A"));
        assert_eq!(labels(&b), ["All manufacturers  (2)", "Alfa  (1)", "Audi  (1)"]);
        assert_eq!(b.cursor, 0);
        assert_eq!(b.click(2), Pick::Browsing { changed: true });
        assert_eq!(b.step(key(|i| i.confirm = true)), Pick::Car(3));
        b.step(back);
        b.step(back);
        assert_eq!(b.level, 0);
        assert_eq!(b.step(back), Pick::Leave);
    }

    /// LB / RB switch games from any level and come back to the remembered brand; the car list remembers its car.
    #[test]
    fn tabs_and_memory() {
        let mut b = CarBrowser::new(catalog(), 2);
        b.step(key(|i| i.back = true));
        // FH2's brand list, from Lamborghini up to Audi; open it and pick the second car (R8, RS3 by name).
        b.step(key(|i| i.vertical = -1));
        b.step(key(|i| i.vertical = -1));
        assert_eq!(labels(&b)[b.cursor], "Audi  (2)");
        b.step(key(|i| i.confirm = true));
        b.step(key(|i| i.vertical = 1));
        assert_eq!(b.selected().map(|e| e.name.as_str()), Some("RS3"));
        // RB -> FH1 (wraps): brand list, nothing remembered there and the current car is FH2's: cursor on "All".
        b.step(key(|i| i.tab = 1));
        assert_eq!((b.level, b.game, b.cursor), (1, 0, 0));
        assert_eq!(b.view().tabs.iter().map(|t| t.active).collect::<Vec<_>>(), [true, false]);
        // LB -> back to FH2: cursor on Audi; open it: RS3 again.
        b.step(key(|i| i.tab = -1));
        assert_eq!((b.game, labels(&b)[b.cursor].as_str()), (1, "Audi  (2)"));
        b.step(key(|i| i.confirm = true));
        assert_eq!(b.selected().map(|e| e.name.as_str()), Some("RS3"));
        // Up and down again keeps it too.
        b.step(key(|i| i.back = true));
        b.step(key(|i| i.confirm = true));
        assert_eq!(b.selected().map(|e| e.name.as_str()), Some("RS3"));
    }

    /// LT / RT: next brand on the car list, letter groups on the brand list; sort keeps the selection; drive filter.
    #[test]
    fn jumps_sort_and_drive() {
        let mut b = CarBrowser::new(catalog(), 3); // FH2 Audi RS3
        b.step(key(|i| i.jump = 1));
        assert_eq!(b.view().crumbs, ["FH2", "BMW"]);
        b.step(key(|i| i.jump = -1));
        b.step(key(|i| i.jump = -1));
        assert_eq!(b.view().crumbs, ["FH2", "Alfa"]);
        // Brand list: rows All, Alfa, Audi, BMW, Lamborghini -> letter groups [All] [Alfa Audi] [BMW] [Lamborghini].
        b.step(key(|i| i.back = true));
        b.cursor = 0;
        b.step(key(|i| i.jump = 1));
        assert_eq!(b.cursor, 1);
        b.step(key(|i| i.jump = 1));
        assert_eq!(b.cursor, 3);
        b.step(key(|i| i.jump = -1));
        assert_eq!(b.cursor, 1);
        b.step(key(|i| i.jump = -1));
        assert_eq!(b.cursor, 0);
        // All manufacturers; sorting by PI keeps the selected car.
        b.click(0);
        let sel = |b: &CarBrowser| b.selected().map(|e| e.index);
        b.step(key(|i| i.vertical = 1));
        b.step(key(|i| i.vertical = 1));
        let before = sel(&b);
        b.step(key(|i| i.sort = true));
        assert_eq!(b.sort, Sort::Pi);
        assert_eq!(sel(&b), before);
        // Drive filter (View): drives present = [RWD, AWD].
        b.step(key(|i| i.drive = true));
        assert_eq!(b.drive.as_deref(), Some("RWD"));
        b.step(key(|i| i.drive = true));
        assert_eq!(b.drive.as_deref(), Some("AWD"));
        assert_eq!(b.len(), 1);
        b.step(key(|i| i.drive = true));
        assert_eq!(b.drive, None);
    }

    /// Held repeats stop at the ends, single presses wrap; pages clamp.
    #[test]
    fn cursor_moves() {
        let mut b = CarBrowser::new(catalog(), 2);
        b.step(key(|i| i.back = true));
        b.cursor = 0;
        b.step(key(|i| {
            i.vertical = -1;
            i.held = true;
        }));
        assert_eq!(b.cursor, 0);
        b.step(key(|i| i.vertical = -1));
        assert_eq!(b.cursor, 4);
        b.step(key(|i| i.horizontal = -1));
        assert_eq!(b.cursor, 0);
        b.step(key(|i| i.horizontal = 1));
        assert_eq!(b.cursor, 4);
    }

    #[test]
    fn repeat_accelerates() {
        let mut r = AxisRepeat::default();
        assert_eq!(r.update(1, 0.016), (1, false));
        let mut steps = 0;
        for _ in 0..20 {
            steps += r.update(1, 0.016).0; // 0.32 s: still in the delay
        }
        assert_eq!(steps, 0);
        let mut early = 0;
        for _ in 0..30 {
            early += r.update(1, 0.016).0;
        }
        for _ in 0..90 {
            r.update(1, 0.016);
        }
        let mut late = 0;
        for _ in 0..30 {
            late += r.update(1, 0.016).0;
        }
        assert!(late > early * 2, "{early} -> {late}");
        assert_eq!(r.update(0, 0.016), (0, false));
        assert_eq!(r.update(-1, 0.016), (-1, false));
    }

    #[test]
    fn map_browser_walk() {
        let groups = vec![("FH1".to_string(), vec![("colorado".to_string(), "Colorado".to_string())]), ("FH2".into(), vec![("fh2/anthem".into(), "Southern Europe (FH2)".into())])];
        let mut b = MapBrowser::new(groups, "colorado");
        assert_eq!((b.level, b.group), (2, 0));
        b.step(key(|i| i.back = true));
        assert_eq!(b.level, 0);
        b.step(key(|i| i.vertical = 1));
        assert_eq!(b.step(key(|i| i.confirm = true)), Pick::Browsing { changed: true });
        assert_eq!(b.step(key(|i| i.confirm = true)), Pick::Map("fh2/anthem".into()));
        // RB from FH2's maps wraps to FH1's maps; B there goes to the game list, B again leaves.
        b.step(key(|i| i.tab = 1));
        assert_eq!((b.level, b.group), (2, 0));
        b.step(key(|i| i.back = true));
        assert_eq!(b.step(key(|i| i.back = true)), Pick::Leave);
    }

    /// FM4-style groups: game -> track -> layout, LT / RT switch tracks on the layout list.
    #[test]
    fn map_browser_tracks() {
        let m = |id: &str| (id.to_string(), id.to_string());
        let groups = vec![
            ("FH1".to_string(), vec![m("colorado")]),
            ("FM4  ·  Bernese Alps".to_string(), vec![m("fm4/alps_a"), m("fm4/alps_b")]),
            ("FM4  ·  Catalunya".to_string(), vec![m("fm4/cat_gp"), m("fm4/cat_nat")]),
        ];
        let mut b = MapBrowser::new(groups, "fm4/cat_nat");
        assert_eq!((b.level, b.group, b.cursor), (2, 2, 1));
        assert_eq!(b.view().crumbs, ["FM4", "Catalunya"]);
        b.step(key(|i| i.jump = -1));
        assert_eq!((b.group, b.cursor), (1, 0));
        b.step(key(|i| i.back = true));
        assert_eq!((b.level, b.cursor), (1, 0));
        assert_eq!(b.rows()[1].label, "Catalunya  (2)");
        b.step(key(|i| i.back = true));
        assert_eq!((b.level, b.cursor), (0, 1));
        // LB from the game list opens FH1 (one group: straight to its maps).
        b.step(key(|i| i.tab = -1));
        assert_eq!((b.level, b.group), (2, 0));
        b.show_games();
        assert_eq!((b.level, b.cursor), (0, 0));
    }

    /// Games that aren't imported: greyed rows that can't be picked; LB / RB pass through their tabs.
    #[test]
    fn locked_games() {
        let groups = vec![("FH1".to_string(), vec![("colorado".to_string(), "Colorado".to_string())])];
        let lock = crate::track::LockedMap {
            game: "FH2".into(),
            id: "fh2/anthem".into(),
            name: "Southern Europe (FH2)".into(),
            reason: "Requires Forza Horizon 2".into(),
            circuit: false,
            coming_soon: false,
        };
        let mut b = MapBrowser::new(groups, "colorado").with_locked(vec![lock]);
        b.show_games();
        assert_eq!(b.rows().iter().map(|r| r.locked).collect::<Vec<_>>(), [false, true]);
        b.step(key(|i| i.vertical = 1));
        b.step(key(|i| i.confirm = true));
        assert_eq!((b.level, b.locked_row(0)), (2, true));
        assert_eq!(b.step(key(|i| i.confirm = true)), Pick::Browsing { changed: false });

        let mut cat = CarCatalog { entries: vec![car(0, "FH1", "Alfa", "8C", "S", 700)], games: vec!["FH1".into(), "FM4".into()], ..Default::default() };
        cat.locked.insert("FM4".into(), "Requires Forza Motorsport 4".into());
        let mut c = CarBrowser::new(std::sync::Arc::new(cat), 0);
        c.step(key(|i| i.tab = 1));
        assert_eq!((c.game, c.level, c.len()), (1, 1, 1));
        assert!(c.locked_row(0) && c.rows()[0].locked);
        assert_eq!(c.step(key(|i| i.confirm = true)), Pick::Browsing { changed: false });
        assert_eq!(c.step(key(|i| i.filter = true)), Pick::Browsing { changed: false });
        c.step(key(|i| i.tab = 1));
        assert_eq!((c.game, c.level), (0, 1));
        assert!(c.selected().is_none());
    }

    #[test]
    fn group_jumps() {
        let k = ['a', 'a', 'b', 'c', 'c'];
        assert_eq!(group_jump(&k, 0, 1), 2);
        assert_eq!(group_jump(&k, 4, 1), 0);
        assert_eq!(group_jump(&k, 4, -1), 2);
        assert_eq!(group_jump(&k, 1, -1), 3);
    }

    #[test]
    fn class_order() {
        assert!(class_rank("D") < class_rank("S1") && class_rank("S2") < class_rank("X") && class_rank("A") < class_rank("R3"));
    }
}
