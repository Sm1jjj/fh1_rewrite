//! Card menus, shop screens (2026-10-09; ui/cards.rs is the framework): the Garage hub, the Autoshow (makers -> cars ->
//! BUY CAR?), My cars (car -> Drive / Customize / Sell) and Change car (ownership off: makers -> cars of every game).
//!
//! - After FH1's 115_c_buy_mfrselect (maker panes), 108_c_buy_carselect (car panes + CAR_STATS: photo, class, maker,
//!   name, drivetrain) and 114_c_buy_buycar_color (price against the credits); carflow strings "BUY CAR?",
//!   "INSUFFICIENT CR!" (and run_shop's "CAR ADDED TO GARAGE").
//! - Pad: LB / RB class tabs (ALL + the classes on the screen; game tabs on Change car), X drive filter, Y sort (makers:
//!   name / car count; cars: name / price / PI / year), A select, R3 sell (My cars), B back.
//! - Prices, ownership and sell values come straight from progression::wallet (Profile + the career data) at each build,
//!   so the screens don't depend on ui/garage.rs's list rebuild (that one only runs for the old list page); `activate`
//!   recomputes them from `Act`'s profile / events / looks. Buying and selling stay in garage.rs `run_shop`
//!   (DialogAct::Buy / Sell -> ShopState::request_buy / request_sell).
//! - Car details: the stock car's FH rating bars (speed, handling, acceleration, launch, braking) from site-75's
//!   `GarageApi::stock` for the focused car (async: `pending` until it arrives), plus the catalog's spec lines.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bevy::prelude::*;

use super::super::browser::{class_rank, CarCatalog, CarEntry};
use super::super::garage::{shop_on, Mode};
use super::super::{Menu, ACCENT};
use super::{class_color, hint, initials, Act, Badge, Bar, Card, Ctx, Details, Dialog, DialogAct, Frame, HintAct, Item, Layout, Screen, View, BAD, GOOD, OWNED};
use crate::progression::{fmt_num, wallet};

/// A Garage hub tile.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) enum HubTile {
    MyCars,
    Autoshow,
    /// Every car, no prices (ownership off).
    ChangeCar,
    Customize,
}

const HUB: Layout = Layout::Hub { w: 360.0, h: 420.0 };
const MAKERS: Layout = Layout::Grid { cols: 5, rows: 3, w: 230.0, h: 170.0 };
const CARS: Layout = Layout::Grid { cols: 4, rows: 3, w: 250.0, h: 200.0 };

/// What the shop knows about the cars at this build (wallet + career data), keyed by garage index.
#[derive(Default, Clone)]
struct Wallet {
    /// Ownership on and the profile / career data loaded.
    live: bool,
    credits: Option<i64>,
    owned: HashSet<usize>,
    /// Autoshow price (for sale).
    price: HashMap<usize, i64>,
    /// Sell value of an owned car (`None` inside = can't be sold, a barn find).
    sell: HashMap<usize, Option<i64>>,
    /// "Maker Name" per garage index (dialogs).
    names: HashMap<usize, String>,
    /// Every owned car, also those missing from this install's garage (wallet::sell's last-car rule).
    owned_total: usize,
}

/// The wallet view of the catalog's cars (both `view` and `activate` build it: cheap, no file IO).
fn wallet_of(
    profile: Option<&crate::progression::Profile>,
    events: Option<&crate::race::Events>,
    garage: &crate::Garage,
    looks: &super::super::customize::CarLooks,
    cat: &CarCatalog,
) -> Wallet {
    let mut w = Wallet::default();
    for e in &cat.entries {
        w.names.insert(e.index, format!("{} {}", e.maker, e.name));
    }
    let (Some(p), Some(ev)) = (profile, events) else { return w };
    if !shop_on() {
        return w;
    }
    w.live = true;
    w.credits = Some(wallet::credits(p));
    w.owned_total = wallet::owned(p).len();
    let index: HashMap<&str, usize> = garage.cars.iter().enumerate().map(|(i, c)| (c.as_str(), i)).collect();
    for o in wallet::owned(p) {
        if let Some(&i) = index.get(o.car.as_str()) {
            w.owned.insert(i);
            w.sell.insert(i, wallet::sell_price(&ev.career, p, &o.car, looks.spent(&o.car)));
        }
    }
    for e in &cat.entries {
        if let Some(car) = garage.cars.get(e.index) {
            if let Some(c) = wallet::price(&ev.career, car) {
                w.price.insert(e.index, c);
            }
        }
    }
    w
}

/// The screen's candidate cars (entry indices) before tabs / filters: Autoshow = FH1 cars for sale, My cars = owned,
/// Change car = every car; of one maker when given.
fn candidates(cat: &CarCatalog, w: &Wallet, mode: Mode, maker: Option<&str>) -> Vec<usize> {
    let (game, maker) = match maker.map(split_key) {
        Some((g, m)) => (g, Some(m)),
        None => (None, None),
    };
    (0..cat.entries.len())
        .filter(|&i| {
            let e = &cat.entries[i];
            let keep = match mode {
                Mode::Shop => e.game == "FH1" && w.price.contains_key(&e.index),
                Mode::Owned => w.owned.contains(&e.index),
                Mode::All => true,
            };
            keep && maker.is_none_or(|m| e.maker == m) && game.is_none_or(|g| e.game == g)
        })
        .collect()
}

/// Change car's maker key carries its game tab ("game\u{1}maker"); other screens' keys are the maker alone.
fn maker_key(game: Option<&str>, maker: &str) -> String {
    match game {
        Some(g) => format!("{g}\u{1}{maker}"),
        None => maker.to_string(),
    }
}

/// (game, maker) of a maker key.
fn split_key(key: &str) -> (Option<&str>, &str) {
    match key.split_once('\u{1}') {
        Some((g, m)) => (Some(g), m),
        None => (None, key),
    }
}

/// "ALL" + the classes among `set`, lowest first.
fn class_tabs(cat: &CarCatalog, set: &[usize]) -> Vec<String> {
    let mut c: Vec<String> = Vec::new();
    for &i in set {
        if let Some(k) = &cat.entries[i].class {
            if !c.contains(k) {
                c.push(k.clone());
            }
        }
    }
    c.sort_by_key(|k| class_rank(k));
    std::iter::once("ALL".to_string()).chain(c).collect()
}

/// "ANY" + the drive types among `set`.
fn drive_options(cat: &CarCatalog, set: &[usize]) -> Vec<String> {
    let mut d: Vec<String> = Vec::new();
    for &i in set {
        let k = &cat.entries[i].drive;
        if !k.is_empty() && !d.contains(k) {
            d.push(k.clone());
        }
    }
    let rank = |s: &str| ["FWD", "RWD", "AWD"].iter().position(|k| *k == s).unwrap_or(3);
    d.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.cmp(b)));
    std::iter::once("ANY".to_string()).chain(d).collect()
}

fn pick<'a>(opts: &'a [String], k: usize) -> &'a str {
    opts.get(k % opts.len().max(1)).map_or("", String::as_str)
}

/// The catalog the screens read (the menu's shared one, or built now).
fn catalog(menu: &Menu, ctx: &Ctx) -> Arc<CarCatalog> {
    catalog_of(menu, &ctx.garage)
}

/// The menu's catalog, else `catalog_for` (the prefetched shared one; ui/garage.rs `display_name` uses the same).
fn catalog_of(menu: &Menu, garage: &crate::Garage) -> Arc<CarCatalog> {
    menu.catalog.clone().unwrap_or_else(|| super::super::browser::catalog_for(&garage.assets, &garage.cars))
}

fn status(w: &Wallet, rest: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = w.credits {
        parts.push(format!("Credits  {} CR", fmt_num(c)));
    }
    parts.extend(rest.iter().filter(|s| !s.is_empty()).cloned());
    parts.join("    ·    ")
}

pub(super) fn view(frame: &Frame, menu: &Menu, ctx: &mut Ctx) -> View {
    let cat = catalog(menu, ctx);
    let w = wallet_of(ctx.profile.as_deref(), ctx.events.as_deref(), &ctx.garage, &ctx.looks, &cat);
    match &frame.screen {
        Screen::Hub => hub(frame, &cat, &w, ctx),
        Screen::Makers(mode) => makers(frame, *mode, &cat, &w),
        Screen::Cars(mode, maker) => cars(frame, *mode, maker.as_deref(), &cat, &w, ctx),
        Screen::Tune(_) => View::default(),
    }
}

fn hub(frame: &Frame, cat: &CarCatalog, w: &Wallet, ctx: &mut Ctx) -> View {
    let current = cat.entry_of(ctx.garage.current).cloned();
    let current_name = current.as_ref().map_or_else(String::new, |e| format!("{} {}", e.maker, e.name));
    let current_photo = current.as_ref().and_then(|e| ctx.photo(e, true));
    let mut tiles: Vec<(HubTile, Card)> = Vec::new();
    if shop_on() {
        let for_sale = candidates(cat, w, Mode::Shop, None).len();
        tiles.push((
            HubTile::MyCars,
            Card { title: "MY CARS".into(), sub: format!("{} owned", w.owned.len()), image: current_photo.clone(), glyph: Some("MY".into()), ..default() },
        ));
        tiles.push((HubTile::Autoshow, Card { title: "AUTOSHOW".into(), sub: format!("{for_sale} cars for sale"), glyph: Some("AS".into()), ..default() }));
    } else {
        tiles.push((
            HubTile::ChangeCar,
            Card { title: "CHANGE CAR".into(), sub: format!("{} cars", cat.entries.len()), image: current_photo.clone(), glyph: Some("CAR".into()), ..default() },
        ));
    }
    if super::super::customize::enabled() {
        tiles.push((HubTile::Customize, Card { title: "CUSTOMIZE".into(), sub: current_name.clone(), image: current_photo, glyph: Some("CU".into()), ..default() }));
    }
    let focused = tiles.get(frame.cursor.min(tiles.len().saturating_sub(1))).map(|t| t.0.clone());
    let details = focused.map(|t| {
        let (title, sub) = match t {
            HubTile::MyCars => ("MY CARS", "Drive, customize or sell the cars you own."),
            HubTile::Autoshow => ("AUTOSHOW", "Buy new cars by manufacturer."),
            HubTile::ChangeCar => ("CHANGE CAR", "Pick any car to drive."),
            HubTile::Customize => ("CUSTOMIZE", "Paint, rims, body kits and upgrades for the car you're in."),
        };
        Details { title: title.into(), sub: sub.into(), lines: vec![("Current car".into(), current_name.clone())], ..default() }
    });
    let (cards, items): (Vec<Card>, Vec<Item>) = tiles.into_iter().map(|(t, c)| (c, Item::Hub(t))).unzip();
    View {
        title: "GARAGE".into(),
        crumbs: "Garage".into(),
        status: status(w, &[]),
        cards,
        items,
        layout: Some(HUB),
        details,
        hints: vec![hint("A", "Enter", "Select", HintAct::Confirm), hint("B", "Esc", "Back", HintAct::Back)],
        empty: "Nothing here".into(),
        backdrop: true,
        ..default()
    }
}

fn mode_title(mode: Mode) -> &'static str {
    match mode {
        Mode::Shop => "AUTOSHOW",
        Mode::Owned => "MY CARS",
        Mode::All => "CHANGE CAR",
    }
}

fn makers(frame: &Frame, mode: Mode, cat: &CarCatalog, w: &Wallet) -> View {
    let all = candidates(cat, w, mode, None);
    // Tabs: classes (Autoshow / My cars), games (Change car: every game's cars).
    let tabs: Vec<String> = if mode == Mode::All {
        cat.games.iter().filter(|g| !cat.locked.contains_key(*g) && all.iter().any(|&i| cat.entries[i].game == **g)).cloned().collect()
    } else {
        class_tabs(cat, &all)
    };
    let tab = frame.tab % tabs.len().max(1);
    let drives = drive_options(cat, &all);
    let drive = pick(&drives, frame.filter);
    let sorts = ["name", "cars"];
    let sort = sorts[frame.sort % sorts.len()];
    let set: Vec<usize> = all
        .into_iter()
        .filter(|&i| {
            let e = &cat.entries[i];
            let tab_ok = match mode {
                Mode::All => tabs.get(tab).is_none_or(|g| e.game == *g),
                _ => tab == 0 || tabs.get(tab).is_some_and(|c| e.class.as_deref() == Some(c.as_str())),
            };
            tab_ok && (drive == "ANY" || e.drive == drive)
        })
        .collect();
    let mut makers: Vec<(String, Vec<usize>)> = Vec::new();
    for i in set {
        let m = &cat.entries[i].maker;
        match makers.iter_mut().find(|(k, _)| k == m) {
            Some((_, v)) => v.push(i),
            None => makers.push((m.clone(), vec![i])),
        }
    }
    match sort {
        "cars" => makers.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.to_lowercase().cmp(&b.0.to_lowercase()))),
        _ => makers.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase())),
    }
    let mut cards = Vec::new();
    let mut items = Vec::new();
    for (m, v) in &makers {
        let owned = v.iter().filter(|&&i| w.owned.contains(&cat.entries[i].index)).count();
        let tag = (mode == Mode::Shop && owned > 0).then(|| Badge::new(format!("{owned} OWNED"), OWNED));
        let cheapest = v.iter().filter_map(|&i| w.price.get(&cat.entries[i].index)).min();
        cards.push(Card {
            title: m.to_uppercase(),
            sub: if v.len() == 1 { "1 car".into() } else { format!("{} cars", v.len()) },
            glyph: Some(initials(m)),
            tag,
            foot: (mode == Mode::Shop).then(|| cheapest.map(|c| format!("from {} CR", fmt_num(*c)))).flatten(),
            ..default()
        });
        let game = (mode == Mode::All).then(|| tabs.get(tab).map(String::as_str)).flatten();
        items.push(Item::Maker(maker_key(game, m)));
    }
    let details = makers.get(frame.cursor).map(|(m, v)| {
        let classes = class_tabs(cat, v);
        let mut lines = vec![("Cars".to_string(), v.len().to_string())];
        if classes.len() > 1 {
            lines.push(("Classes".into(), classes[1..].join(" ")));
        }
        let years: Vec<i64> = v.iter().filter_map(|&i| cat.entries[i].year).collect();
        if let (Some(a), Some(b)) = (years.iter().min(), years.iter().max()) {
            lines.push(("Years".into(), if a == b { a.to_string() } else { format!("{a} - {b}") }));
        }
        if mode == Mode::Shop {
            let prices: Vec<i64> = v.iter().filter_map(|&i| w.price.get(&cat.entries[i].index).copied()).collect();
            if let (Some(a), Some(b)) = (prices.iter().min(), prices.iter().max()) {
                lines.push(("Prices".into(), if a == b { format!("{} CR", fmt_num(*a)) } else { format!("{} - {} CR", fmt_num(*a), fmt_num(*b)) }));
            }
        }
        Details { title: m.to_uppercase(), sub: mode_title(mode).into(), lines, action: Some("VIEW CARS".into()), ..default() }
    });
    let tab_label = if mode == Mode::All { "Game" } else { "Class" };
    View {
        title: mode_title(mode).into(),
        crumbs: format!("Garage  ›  {}", mode_title(mode).to_lowercase()),
        tabs,
        tab,
        status: status(w, &[format!("Drive: {}", drive.to_lowercase()), format!("Sort: {sort}")]),
        cards,
        items,
        layout: Some(MAKERS),
        details,
        hints: vec![
            hint("A", "Enter", "Select", HintAct::Confirm),
            hint("B", "Esc", "Back", HintAct::Back),
            hint("LB", "Q", format!("{tab_label} ‹"), HintAct::TabPrev),
            hint("RB", "E", format!("{tab_label} ›"), HintAct::TabNext),
            hint("X", "F", "Drive", HintAct::Filter),
            hint("Y", "Tab", "Sort", HintAct::Sort),
        ],
        empty: match mode {
            Mode::Shop if !w.live => "No prices loaded (progression off, or re-run fh1setup: the events group carries them)".into(),
            _ => "No cars match the filters".into(),
        },
        backdrop: true,
        ..default()
    }
}

/// Sort orders of a car screen (price only where prices mean something).
fn car_sorts(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::Shop | Mode::Owned => &["name", "price", "PI", "year"],
        Mode::All => &["name", "PI", "year"],
    }
}

fn cars(frame: &Frame, mode: Mode, maker: Option<&str>, cat: &CarCatalog, w: &Wallet, ctx: &mut Ctx) -> View {
    let all = candidates(cat, w, mode, maker);
    let tabs = class_tabs(cat, &all);
    let tab = frame.tab % tabs.len().max(1);
    let drives = drive_options(cat, &all);
    let drive = pick(&drives, frame.filter);
    let sorts = car_sorts(mode);
    let sort = sorts[frame.sort % sorts.len()];
    let value = |i: usize| {
        let g = cat.entries[i].index;
        match mode {
            Mode::Owned => w.sell.get(&g).copied().flatten().unwrap_or(0),
            _ => w.price.get(&g).copied().unwrap_or(0),
        }
    };
    let mut set: Vec<usize> = all
        .into_iter()
        .filter(|&i| {
            let e = &cat.entries[i];
            (tab == 0 || tabs.get(tab).is_some_and(|c| e.class.as_deref() == Some(c.as_str()))) && (drive == "ANY" || e.drive == drive)
        })
        .collect();
    let name = |i: usize| (cat.entries[i].maker.to_lowercase(), cat.entries[i].name.to_lowercase());
    match sort {
        "price" => set.sort_by_key(|&i| (value(i), name(i))),
        "PI" => set.sort_by_key(|&i| (std::cmp::Reverse(cat.entries[i].pi.unwrap_or(0)), name(i))),
        "year" => set.sort_by_key(|&i| (cat.entries[i].year.unwrap_or(0), name(i))),
        _ => set.sort_by_key(|&i| name(i)),
    }
    let current = ctx.garage.current;
    let mut cards = Vec::new();
    let mut items = Vec::new();
    for (k, &i) in set.iter().enumerate() {
        let e = &cat.entries[i];
        let focused = k == frame.cursor;
        let owned = w.owned.contains(&e.index);
        let (tag, foot, foot_color) = match mode {
            Mode::Shop if owned => (Some(Badge::new("OWNED", OWNED)), None, None),
            Mode::Shop => {
                let p = w.price.get(&e.index).copied();
                let short = matches!((p, w.credits), (Some(p), Some(c)) if p > c);
                (None, p.map(|p| format!("{} CR", fmt_num(p))), Some(if short { BAD } else { GOOD }))
            }
            Mode::Owned => {
                let tag = (e.index == current).then(|| Badge::new("CURRENT", ACCENT));
                let foot = match w.sell.get(&e.index) {
                    Some(Some(v)) => Some(format!("Sell {} CR", fmt_num(*v))),
                    Some(None) => Some("Barn find".into()),
                    None => None,
                };
                (tag, foot, None)
            }
            Mode::All => ((e.index == current).then(|| Badge::new("CURRENT", ACCENT)), None, None),
        };
        cards.push(Card {
            title: e.name.clone(),
            sub: if maker.is_some() { year_drive(e) } else { e.maker.clone() },
            image: ctx.photo(e, focused),
            glyph: Some(initials(&e.maker)),
            badge: class_badge(e),
            tag,
            foot,
            foot_color,
            ..default()
        });
        items.push(Item::Car(e.index));
    }
    let mut pending = false;
    let details = set.get(frame.cursor).map(|&i| {
        let (d, wait) = car_details(cat, i, mode, w, current, ctx);
        pending = wait;
        d
    });
    let a_label = match (mode, set.get(frame.cursor).map(|&i| w.owned.contains(&cat.entries[i].index))) {
        (Mode::Shop, Some(false)) => "Buy",
        (Mode::Owned, _) => "Options",
        _ => "Drive",
    };
    let mut hints = vec![
        hint("A", "Enter", a_label, HintAct::Confirm),
        hint("B", "Esc", "Back", HintAct::Back),
        hint("LB", "Q", "Class ‹", HintAct::TabPrev),
        hint("RB", "E", "Class ›", HintAct::TabNext),
        hint("X", "F", "Drive", HintAct::Filter),
        hint("Y", "Tab", "Sort", HintAct::Sort),
    ];
    if mode == Mode::Owned {
        hints.push(hint("R3", "Del", "Sell", HintAct::Sell));
    }
    let (game, maker_name) = match maker.map(split_key) {
        Some((g, m)) => (g, Some(m)),
        None => (None, None),
    };
    let crumbs = match (game, maker_name) {
        (Some(g), Some(m)) => format!("Garage  ›  {}  ›  {g}  ›  {m}", mode_title(mode).to_lowercase()),
        (None, Some(m)) => format!("Garage  ›  {}  ›  {m}", mode_title(mode).to_lowercase()),
        _ => format!("Garage  ›  {}", mode_title(mode).to_lowercase()),
    };
    View {
        title: maker_name.map_or_else(|| mode_title(mode).to_string(), str::to_uppercase),
        crumbs,
        tabs,
        tab,
        status: status(w, &[format!("Drive: {}", drive.to_lowercase()), format!("Sort: {sort}")]),
        cards,
        items,
        layout: Some(CARS),
        details,
        hints,
        pending,
        empty: match mode {
            Mode::Owned => "No cars owned".into(),
            _ => "No cars match the filters".into(),
        },
        backdrop: true,
        ..default()
    }
}

fn year_drive(e: &CarEntry) -> String {
    let mut parts = Vec::new();
    if let Some(y) = e.year {
        parts.push(y.to_string());
    }
    if !e.drive.is_empty() {
        parts.push(e.drive.clone());
    }
    if e.game != "FH1" {
        parts.push(e.game.clone());
    }
    parts.join("  ·  ")
}

fn class_badge(e: &CarEntry) -> Option<Badge> {
    match (&e.class, e.pi) {
        (Some(c), Some(p)) => Some(Badge::new(format!("{c} {p}"), class_color(c))),
        (Some(c), None) => Some(Badge::new(c.clone(), class_color(c))),
        (None, Some(p)) => Some(Badge::new(p.to_string(), class_color(""))),
        _ => None,
    }
}

/// The focused car's CAR_STATS panel: photo, class, maker, name, drivetrain, rating bars, specs, price against the
/// credits. Also whether the ratings are still being computed (`View::pending`).
fn car_details(cat: &CarCatalog, i: usize, mode: Mode, w: &Wallet, current: usize, ctx: &mut Ctx) -> (Details, bool) {
    let e = &cat.entries[i];
    // FH rating bars of the stock car (the game's 3..10 scale), from site-75's evaluation (async, cached).
    let mut bars = Vec::new();
    let mut pending = false;
    if let Some(car) = ctx.garage.cars.get(e.index).filter(|_| e.game == "FH1").cloned() {
        match ctx.api.stock(&car) {
            Some(ev) => {
                for (label, r) in ["Speed", "Handling", "Acceleration", "Launch", "Braking"].iter().zip(ev.ratings) {
                    bars.push(Bar { label: (*label).into(), now: (r / 10.0).clamp(0.0, 1.0), after: None, text: format!("{r:.1}") });
                }
            }
            None => pending = true,
        }
    }
    let mut lines = Vec::new();
    if let Some(s) = cat.stats(i) {
        if let Some(p) = s.power_w {
            lines.push(("Power".to_string(), format!("{:.0} hp", p / 745.7)));
        }
        if let Some(t) = s.zero_60 {
            lines.push(("0-60 mph".into(), format!("{t:.1} s")));
        }
        if let Some(v) = s.top_mph {
            lines.push(("Top speed".into(), format!("{v:.0} mph")));
        }
        if let Some(m) = s.mass_kg {
            lines.push(("Weight".into(), format!("{} kg", fmt_num(m.round() as i64))));
        }
    }
    if let Some(y) = e.year {
        lines.push(("Year".to_string(), y.to_string()));
    }
    if !e.drive.is_empty() {
        lines.push(("Drivetrain".into(), e.drive.clone()));
    }
    if e.game != "FH1" {
        lines.push(("Game".into(), e.game.clone()));
    }
    let owned = w.owned.contains(&e.index);
    let (price, action) = match mode {
        Mode::Shop if owned => (Some(("OWNED".to_string(), OWNED)), "DRIVE"),
        Mode::Shop => match (w.price.get(&e.index), w.credits) {
            (Some(&p), Some(c)) => {
                lines.push(("Credits".into(), format!("{} CR", fmt_num(c))));
                (Some((format!("{} CR", fmt_num(p)), if p > c { BAD } else { GOOD })), if p > c { "INSUFFICIENT CR!" } else { "BUY" })
            }
            (Some(&p), None) => (Some((format!("{} CR", fmt_num(p)), GOOD)), "BUY"),
            _ => (None, "NOT FOR SALE"),
        },
        Mode::Owned => (
            match w.sell.get(&e.index) {
                Some(Some(v)) => Some((format!("Sell value {} CR", fmt_num(*v)), Color::WHITE)),
                Some(None) => Some(("Barn find: can't be sold".to_string(), Color::WHITE)),
                None => None,
            },
            if e.index == current { "CURRENT CAR" } else { "DRIVE / CUSTOMIZE / SELL" },
        ),
        Mode::All => (None, if e.index == current { "CURRENT CAR" } else { "DRIVE" }),
    };
    let d = Details {
        title: e.name.clone(),
        sub: e.maker.clone(),
        image: ctx.photo(e, true),
        badge: class_badge(e),
        bars,
        lines,
        price,
        action: Some(action.into()),
    };
    (d, pending)
}

/// The screen on top: its mode (makers / cars), for activation.
fn top_mode(act: &Act) -> Option<Mode> {
    match act.cards.top().map(|f| &f.screen) {
        Some(Screen::Makers(m)) | Some(Screen::Cars(m, _)) => Some(*m),
        _ => None,
    }
}

pub(super) fn activate(act: &mut Act, item: &Item, confirm: bool, sell: bool) -> bool {
    let cat = catalog_of(act.menu, act.garage);
    let w = wallet_of(act.profile, act.events, act.garage, act.looks, &cat);
    match item {
        Item::Hub(t) if confirm => {
            match t {
                HubTile::MyCars => act.push(Screen::Cars(Mode::Owned, None)),
                HubTile::Autoshow => act.push(Screen::Makers(Mode::Shop)),
                HubTile::ChangeCar => act.push(Screen::Makers(Mode::All)),
                HubTile::Customize => act.customize(),
            }
            false
        }
        Item::Maker(m) if confirm => {
            if let Some(mode) = top_mode(act) {
                act.push(Screen::Cars(mode, Some(m.clone())));
            }
            false
        }
        Item::Car(i) => {
            let i = *i;
            let name = w.names.get(&i).cloned().unwrap_or_else(|| act.garage.cars.get(i).cloned().unwrap_or_default());
            let current = act.garage.current;
            match top_mode(act) {
                Some(Mode::Owned) if sell => {
                    act.dialog(sell_dialog(&w, i, &name, current));
                    false
                }
                Some(Mode::Owned) if confirm => {
                    let mut options = vec![
                        (if i == current { "KEEP DRIVING".to_string() } else { "DRIVE".to_string() }, DialogAct::Drive(i)),
                        ("CUSTOMIZE".to_string(), DialogAct::Customize(i)),
                    ];
                    if let Some(Some(v)) = w.sell.get(&i) {
                        if i != current {
                            options.push((format!("SELL  {} CR", fmt_num(*v)), DialogAct::Sell(i)));
                        }
                    }
                    options.push(("CANCEL".to_string(), DialogAct::Close));
                    act.dialog(Dialog { title: name.to_uppercase(), message: String::new(), options, cursor: 0 });
                    false
                }
                Some(Mode::Shop) if confirm => {
                    if w.owned.contains(&i) {
                        act.drive(i);
                        return true;
                    }
                    match (w.price.get(&i), w.credits) {
                        (Some(&p), Some(c)) if p > c => act.dialog(Dialog {
                            title: "INSUFFICIENT CR!".into(),
                            message: format!("The {name} costs {} CR. You have {} CR.", fmt_num(p), fmt_num(c)),
                            options: vec![("OK".into(), DialogAct::Close)],
                            cursor: 0,
                        }),
                        (Some(&p), _) => act.dialog(Dialog {
                            title: "BUY CAR?".into(),
                            message: format!("Do you want to buy the {name} for {} CR?", fmt_num(p)),
                            options: vec![("BUY".into(), DialogAct::Buy(i)), ("CANCEL".into(), DialogAct::Close)],
                            cursor: 0,
                        }),
                        (None, _) => act.dialog(Dialog {
                            title: "NOT FOR SALE".into(),
                            message: format!("The {name} is not for sale."),
                            options: vec![("OK".into(), DialogAct::Close)],
                            cursor: 0,
                        }),
                    }
                    false
                }
                Some(Mode::All) if confirm => {
                    act.drive(i);
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// R3 on an owned car: the sell confirm (carflow IDS_Sell_Car_Confirm), or why it can't be sold (run_shop's rules).
fn sell_dialog(w: &Wallet, i: usize, name: &str, current: usize) -> Dialog {
    let refuse = |m: String| Dialog { title: "SELL CAR".into(), message: m, options: vec![("OK".into(), DialogAct::Close)], cursor: 0 };
    if i == current {
        return refuse("You can't do this to the car you are currently in. Get in another car first.".into());
    }
    if w.owned_total <= 1 {
        return refuse("You can't sell your last car".into());
    }
    match w.sell.get(&i) {
        Some(Some(v)) => Dialog {
            title: "SELL CAR".into(),
            message: format!("Are you sure you want to sell your {name} for {} CR?", fmt_num(*v)),
            options: vec![("SELL".into(), DialogAct::Sell(i)), ("CANCEL".into(), DialogAct::Close)],
            cursor: 1,
        },
        _ => refuse("You cannot sell Barn finds".into()),
    }
}
