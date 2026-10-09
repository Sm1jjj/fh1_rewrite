//! Customize as cards (ui/cards.rs): hub -> Paint (factory / special / custom colour) | Rims | Body kit -> slot ->
//! options | Upgrades -> category -> slot (LB / RB = category) -> parts.
//!
//! The car stays visible above a strip of cards. The focused option previews on the driven car after a 0.15 s rest
//! (garage API `Preview`), A installs it (`Install`: pays for what isn't owned yet, saves garage.json, respawns the car),
//! B leaves the screen and drops the preview (`PreviewEnd`). Option cards and the details panel compare the SAVED build
//! with the saved build + that option (PI badge per card, rating bars, power / weight), so a running preview doesn't
//! zero the deltas. Data, prices and rules are the upgrades worker's (ui/customize_upgrades/garage_api.rs); the paint
//! lists come from ui/customize.rs `CustomizeMenu` (loaded by `open`).

use bevy::prelude::*;

use super::super::customize::{self, CarLook, Paint};
use super::super::customize_upgrades::garage_api as api;
use super::super::{Menu, Page, ACCENT};
use super::{class_color, hint, initials, Badge, Bar, Card, CardInput, Cards, Ctx, Details, Dialog, DialogAct, Frame, HintAct, Item as CardItem, Layout, View, BAD, GOOD, OWNED};
use crate::progression::fmt_num;

#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) enum Screen {
    Hub,
    /// Tabs: factory / special / custom.
    Paint,
    /// The custom colour editor (hue, saturation, brightness, finish).
    Custom,
    Rims,
    /// Body kit slots.
    Kits,
    /// Options of kit slot n (index into `api::kits`).
    KitOptions(usize),
    /// Upgrade categories.
    Categories,
    /// Slots of the category = the frame's tab (LB / RB switch category).
    Slots,
    /// Options of (category, slot).
    Parts(usize, usize),
}

#[derive(Clone, Debug)]
pub(in crate::ui) enum Item {
    Open(Screen),
    /// Upgrades > category n.
    OpenSlots(usize),
    /// Paint > the custom colour editor.
    Custom,
    Reset,
    /// An option: preview on focus, install on A.
    Change(api::Change),
}

/// State of the Customize screens.
#[derive(Default)]
pub(in crate::ui) struct State {
    hsv: [f32; 3],
    metallic: bool,
    slider: usize,
    /// A garage API preview is on the car (PreviewEnd owed).
    previewing: bool,
}

const RATINGS: [&str; 5] = ["Speed", "Handling", "Acceleration", "Launch", "Braking"];

/// Open the Customize hub on the driven car (Garage hub tile, My cars > Customize).
pub(in crate::ui) fn open(cards: &mut Cards, menu: &mut Menu) {
    // Loads the car's paint lists (ui/customize.rs; its other views stay unused here).
    menu.custom.open();
    cards.push(super::Screen::Tune(Screen::Hub));
    menu.page = Page::Customize;
    cards.page = Some(Page::Customize);
}

fn is_preview_screen(s: &Screen) -> bool {
    matches!(s, Screen::Paint | Screen::Custom | Screen::Rims | Screen::KitOptions(_) | Screen::Parts(..))
}

/// The preview is dropped (leaving the pages, resuming).
pub(in crate::ui) fn end_preview(cards: &mut Cards, w: &mut MessageWriter<api::GarageAction>, car: &str) {
    cards.preview_in = None;
    if std::mem::take(&mut cards.tune.previewing) {
        w.write(api::GarageAction::PreviewEnd { car: car.to_owned() });
    }
}

/// B on screen `s` (before the framework pops it).
pub(in crate::ui) fn leaving(cards: &mut Cards, s: &Screen, w: &mut MessageWriter<api::GarageAction>, car: &str) {
    if is_preview_screen(s) {
        end_preview(cards, w, car);
    }
}

fn custom_paint(st: &State) -> Paint {
    Paint::Custom { rgb: customize::hsv_rgb(st.hsv), metallic: st.metallic }
}

/// The custom colour editor reads the stick itself: up / down pick a slider, left / right change it.
pub(in crate::ui) fn input(cards: &mut Cards, s: &Screen, input: &CardInput) -> bool {
    if *s != Screen::Custom {
        return false;
    }
    let st = &mut cards.tune;
    if input.dy != 0 {
        st.slider = (st.slider as i32 + input.dy.signum()).rem_euclid(4) as usize;
        cards.dirty = true;
    }
    if input.dx != 0 {
        let d = input.dx as f32;
        match st.slider {
            0 => st.hsv[0] = (st.hsv[0] + 5.0 * d).rem_euclid(360.0),
            1 => st.hsv[1] = (st.hsv[1] + 0.05 * d).clamp(0.0, 1.0),
            2 => st.hsv[2] = (st.hsv[2] + 0.05 * d).clamp(0.0, 1.0),
            _ => st.metallic = !st.metallic,
        }
        cards.dirty = true;
        cards.preview_in = Some(0.12);
    }
    true
}

/// Preview the focused option (or the custom colour) on the car.
pub(in crate::ui) fn preview_focused(cards: &mut Cards, w: &mut MessageWriter<api::GarageAction>, car: &str) {
    let Some(f) = cards.top() else { return };
    let change = match &f.screen {
        super::Screen::Tune(Screen::Custom) => Some(api::Change::Paint(Some(custom_paint(&cards.tune)))),
        super::Screen::Tune(s) if is_preview_screen(s) => match cards.items.get(f.cursor) {
            Some(CardItem::Tune(Item::Change(c))) => Some(c.clone()),
            _ => None,
        },
        _ => None,
    };
    if let Some(change) = change {
        w.write(api::GarageAction::Preview { car: car.to_owned(), change });
        cards.tune.previewing = true;
    }
}

/// A on a Customize card (or in the custom colour editor).
pub(in crate::ui) fn activate(cards: &mut Cards, menu: &mut Menu, s: &Screen, item: Option<&CardItem>, w: &mut MessageWriter<api::GarageAction>, api: &api::GarageApi) {
    let car = api.driven().to_owned();
    if *s == Screen::Custom {
        let change = api::Change::Paint(Some(custom_paint(&cards.tune)));
        install(cards, w, &car, change);
        return;
    }
    let Some(CardItem::Tune(item)) = item else { return };
    match item.clone() {
        Item::Open(next) => {
            let start = start_cursor(&next, menu, api, &car);
            cards.push(super::Screen::Tune(next));
            if let (Some(f), Some((tab, cursor))) = (cards.top_mut(), start) {
                f.tab = tab;
                f.cursor = cursor;
            }
        }
        Item::OpenSlots(k) => {
            cards.push(super::Screen::Tune(Screen::Slots));
            if let Some(f) = cards.top_mut() {
                f.tab = k;
            }
        }
        Item::Custom => {
            let look = api.look(&car);
            if let Some((rgb, metallic)) = menu.custom.shown_rgb(Some(&look)) {
                cards.tune.hsv = customize::rgb_hsv(rgb);
                cards.tune.metallic = metallic;
            }
            cards.tune.slider = 0;
            cards.push(super::Screen::Tune(Screen::Custom));
            cards.preview_in = Some(0.0);
        }
        Item::Reset => {
            cards.dialog = Some(Dialog {
                title: "Reset to stock?".into(),
                message: "Paint, rims, body kit and upgrades go back to the factory build. Parts you bought stay owned, so fitting them again is free.".into(),
                options: vec![("Reset".into(), DialogAct::ResetStock), ("Cancel".into(), DialogAct::Close)],
                cursor: 1,
            });
            cards.dirty = true;
        }
        Item::Change(change) => install(cards, w, &car, change),
    }
}

fn install(cards: &mut Cards, w: &mut MessageWriter<api::GarageAction>, car: &str, change: api::Change) {
    w.write(api::GarageAction::Install { car: car.to_owned(), change });
    // Install clears the preview itself (garage API).
    cards.tune.previewing = false;
    cards.preview_in = None;
    cards.notice = Some("Installing…".into());
    cards.dirty = true;
}

/// (tab, cursor) a screen opens on: what the car wears.
fn start_cursor(s: &Screen, menu: &Menu, api: &api::GarageApi, car: &str) -> Option<(usize, usize)> {
    let look = api.look(car);
    match s {
        Screen::Paint => {
            let (factory, specials) = menu.custom.paint_options();
            match look.paint {
                Some(Paint::Custom { rgb, metallic }) => Some(match specials.iter().position(|p| p.rgb == rgb && p.metallic == metallic) {
                    Some(i) => (1, i),
                    None => (2, 0),
                }),
                Some(Paint::Factory { sequence }) => factory.iter().position(|p| p.paint == Paint::Factory { sequence }).map(|i| (0, i)),
                None => {
                    let shown = menu.custom.shown_rgb(Some(&look));
                    factory.iter().position(|p| Some((p.rgb, p.metallic)) == shown).map(|i| (0, i))
                }
            }
        }
        Screen::Rims => {
            let rims = api.rims(car);
            Some((0, rims.iter().position(|r| r.installed).map_or(0, |i| i + 1)))
        }
        Screen::KitOptions(k) => api.kits(car).get(*k).and_then(|slot| slot.options.iter().position(|o| o.installed)).map(|i| (0, i)),
        Screen::Parts(c, sl) => api.categories(car).get(*c).and_then(|cat| cat.slots.get(*sl)).and_then(|slot| slot.options.iter().position(|o| o.installed)).map(|i| (0, i)),
        _ => None,
    }
}

/// The notice line for a garage API result.
pub(in crate::ui) fn outcome_text(o: &api::Outcome) -> String {
    match o {
        api::Outcome::Ok { charged, bought } if *charged > 0 => format!("Bought {} for {} CR", bought.join(", "), fmt_num(*charged)),
        api::Outcome::Ok { .. } => "Installed".into(),
        api::Outcome::NotEnoughCredits { need, have } => format!("INSUFFICIENT CR!  It costs {} CR, you have {} CR", fmt_num(*need), fmt_num(*have)),
        api::Outcome::Incompatible(why) => why.clone(),
    }
}

// ------------------------------------------------------------------ views

/// What the views share: the car, its saved build and that build's evaluation.
struct Base {
    car: String,
    title: String,
    assets: std::path::PathBuf,
    saved: CarLook,
    now: Option<api::Eval>,
    credits: Option<i64>,
}

fn base(menu: &Menu, ctx: &Ctx) -> Base {
    let car = ctx.api.driven().to_owned();
    let saved = ctx.looks.saved(&car).cloned().unwrap_or_default();
    let title = menu.catalog.as_ref().and_then(|c| c.entry_of(ctx.garage.current)).map_or_else(|| car.clone(), |e| format!("{} {}", e.maker, e.name));
    let now = ctx.eval.get(&car, &saved.upgrades);
    Base { assets: ctx.garage.assets.clone(), title, now, credits: ctx.credits(), saved, car }
}

impl Base {
    /// The saved build + `change`, evaluated (Err = can't be fitted; Ok(None) = computing).
    fn after(&self, ctx: &Ctx, change: &api::Change) -> Result<Option<api::Eval>, String> {
        let look = api::with_change(&self.assets, &self.car, &self.saved, change)?;
        Ok(ctx.eval.get(&self.car, &look.upgrades))
    }

    fn crumbs(&self, path: &str) -> String {
        match self.credits {
            Some(c) => format!("Customize  ·  {}  ·  {path}  ·  {} CR", self.title, fmt_num(c)),
            None => format!("Customize  ·  {}  ·  {path}", self.title),
        }
    }
}

fn class_badge(e: &api::Eval) -> Badge {
    Badge::new(format!("{} {}", e.class_letter, e.display_pi), class_color(&e.class_letter))
}

/// PI change badge of an option card.
fn pi_badge(now: Option<&api::Eval>, after: &Result<Option<api::Eval>, String>, pending: &mut bool) -> Option<Badge> {
    match (now, after) {
        (_, Err(_)) => Some(Badge::new("N/A", Color::srgb(0.35, 0.36, 0.40))),
        (Some(n), Ok(Some(a))) => {
            let d = a.display_pi as i64 - n.display_pi as i64;
            let colour = if d > 0 { GOOD } else if d < 0 { BAD } else { Color::srgb(0.35, 0.36, 0.40) };
            Some(Badge::new(if d == 0 { format!("{} {}", a.class_letter, a.display_pi) } else { format!("PI {d:+}") }, colour))
        }
        _ => {
            *pending = true;
            Some(Badge::new("PI …", Color::srgb(0.35, 0.36, 0.40)))
        }
    }
}

/// Rating bars, PI and specs: the saved build against `after` (None = no comparison).
fn eval_details(title: String, sub: String, now: Option<&api::Eval>, after: Option<&api::Eval>, pending: &mut bool) -> Details {
    let Some(n) = now else {
        *pending = true;
        return Details { title, sub, lines: vec![("Performance".into(), "calculating…".into())], ..default() };
    };
    let shown = after.unwrap_or(n);
    let bars = RATINGS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let (v, a) = (n.ratings[i], after.map(|a| a.ratings[i]));
            let text = match a {
                Some(a) if (a - v).abs() >= 0.05 => format!("{v:.1}  →  {a:.1}"),
                _ => format!("{v:.1}"),
            };
            Bar { label: (*label).into(), now: v / 10.0, after: a.map(|a| a / 10.0), text }
        })
        .collect();
    let pair = |f: &dyn Fn(&api::Eval) -> String| match after {
        Some(a) if f(a) != f(n) => format!("{}  →  {}", f(n), f(a)),
        _ => f(n),
    };
    let lines = vec![
        ("PI".to_string(), pair(&|e| format!("{} {}", e.class_letter, e.display_pi))),
        ("Power".to_string(), pair(&|e| format!("{:.0} hp", e.specs.power_kw * 1.341))),
        ("Torque".to_string(), pair(&|e| format!("{:.0} N·m", e.specs.torque_nm))),
        ("Weight".to_string(), pair(&|e| format!("{:.0} kg", e.specs.mass_kg))),
        ("Front weight".to_string(), pair(&|e| format!("{:.0} %", e.specs.front_weight * 100.0))),
    ];
    Details { title, sub, badge: Some(class_badge(shown)), bars, lines, ..default() }
}

/// Price / ownership on an option card: (tag, foot, foot colour).
fn price_bits(price: u32, owned: bool, installed: bool, credits: Option<i64>) -> (Option<Badge>, Option<String>, Option<Color>) {
    if installed {
        return (Some(Badge::new("INSTALLED", OWNED)), None, None);
    }
    let tag = owned.then(|| Badge::new("OWNED", Color::srgb(0.25, 0.42, 0.32)));
    if price == 0 {
        return (tag, Some(if owned { "Owned".into() } else { "Free".into() }), Some(Color::srgba(1.0, 1.0, 1.0, 0.6)));
    }
    let short = credits.is_some_and(|c| c < price as i64);
    (tag, Some(format!("{} CR", fmt_num(price as i64))), Some(if short { BAD } else { Color::WHITE }))
}

/// The price block and A action of an option's details.
fn price_action(d: &mut Details, price: u32, owned: bool, installed: bool, credits: Option<i64>) {
    if installed {
        d.price = Some(("Installed".into(), OWNED));
        return;
    }
    if price == 0 {
        d.price = Some((if owned { "Owned" } else { "Free" }.into(), Color::WHITE));
        d.action = Some("Install".into());
        return;
    }
    let short = credits.is_some_and(|c| c < price as i64);
    d.price = Some((format!("{} CR", fmt_num(price as i64)), if short { BAD } else { Color::WHITE }));
    if let Some(c) = credits {
        d.lines.push(("Your credits".into(), format!("{} CR", fmt_num(c))));
    }
    d.action = Some(if short { "Insufficient CR".into() } else { "Buy & install".into() });
}

fn hints_select() -> Vec<super::Hint> {
    vec![hint("A", "Enter", "Select", HintAct::Confirm), hint("B", "Esc", "Back", HintAct::Back)]
}

fn hints_option(tabs: bool) -> Vec<super::Hint> {
    let mut h = vec![hint("A", "Enter", "Install", HintAct::Confirm), hint("B", "Esc", "Back", HintAct::Back)];
    if tabs {
        h.push(hint("LB", "Q", "Prev", HintAct::TabPrev));
        h.push(hint("RB", "E", "Next", HintAct::TabNext));
    }
    h
}

const STRIP: Layout = Layout::Strip { visible: 5, w: 250.0, h: 210.0 };

pub(in crate::ui) fn view(s: &Screen, frame: &Frame, menu: &Menu, ctx: &mut Ctx, st: &State) -> View {
    let b = base(menu, ctx);
    let mut pending = false;
    let mut v = match s {
        Screen::Hub => hub(&b, menu, ctx, &mut pending),
        Screen::Paint => paint(&b, frame, menu),
        Screen::Custom => custom(&b, st),
        Screen::Rims => rims(&b, frame, ctx, &mut pending),
        Screen::Kits => kits(&b, ctx),
        Screen::KitOptions(k) => kit_options(&b, frame, ctx, *k, &mut pending),
        Screen::Categories => categories(&b, ctx),
        Screen::Slots => slots(&b, frame, ctx),
        Screen::Parts(c, sl) => parts(&b, frame, ctx, *c, *sl, &mut pending),
    };
    v.pending |= pending;
    v.backdrop = false;
    v
}

fn hub(b: &Base, menu: &Menu, ctx: &Ctx, pending: &mut bool) -> View {
    let saved = &b.saved;
    let rims = api::rims(&b.assets, &b.car, saved, saved);
    let kits = api::kits(&b.assets, &b.car, saved, saved);
    let cats = api::categories(&b.assets, &b.car, saved, saved);
    let rim = rims.iter().find(|r| r.installed);
    let kit_slots = kits.iter().filter(|k| k.options.len() > 1).count();
    let kit_changed = kits.iter().filter(|k| k.options.iter().any(|o| o.installed && !o.stock)).count();
    let parts_changed = cats.iter().flat_map(|c| &c.slots).filter(|sl| sl.options.iter().any(|o| o.installed && !o.stock)).count();
    let swatch = menu.custom.shown_rgb(Some(saved)).map(|(rgb, _)| customize::srgb(rgb));
    let cards = vec![
        Card { title: "Paint".into(), sub: menu.custom.paint_name(Some(saved)), swatch, ..default() },
        Card {
            title: "Rims".into(),
            sub: if rims.is_empty() { "Not available".into() } else { rim.map_or_else(|| "Stock".into(), |r| r.name.clone()) },
            glyph: Some(rim.map_or_else(|| "STOCK".into(), |r| initials(&r.maker))),
            locked: rims.is_empty(),
            ..default()
        },
        Card {
            title: "Body kit".into(),
            sub: match (kit_slots, kit_changed) {
                (0, _) => "No options".into(),
                (_, 0) => "Stock".into(),
                (n, k) => format!("{k} of {n} changed"),
            },
            glyph: Some("KIT".into()),
            locked: kit_slots == 0,
            ..default()
        },
        Card {
            title: "Upgrades".into(),
            sub: match parts_changed {
                0 => "Stock".into(),
                n => format!("{n} part{} fitted", if n == 1 { "" } else { "s" }),
            },
            glyph: Some(b.now.as_ref().map_or_else(|| "PI".into(), |e| e.display_pi.to_string())),
            badge: b.now.as_ref().map(class_badge),
            locked: cats.is_empty(),
            ..default()
        },
        Card { title: "Reset to stock".into(), sub: "Factory build".into(), glyph: Some("STOCK".into()), ..default() },
    ];
    let items = [Item::Open(Screen::Paint), Item::Open(Screen::Rims), Item::Open(Screen::Kits), Item::Open(Screen::Categories), Item::Reset].into_iter().map(CardItem::Tune).collect();
    View {
        title: "Customize".into(),
        crumbs: b.crumbs("Garage"),
        cards,
        items,
        layout: Some(STRIP),
        details: Some(eval_details(b.title.clone(), "Current build".into(), b.now.as_ref(), None, pending)),
        hints: hints_select(),
        ..default()
    }
}

fn paint(b: &Base, frame: &Frame, menu: &Menu) -> View {
    let (factory, specials) = menu.custom.paint_options();
    let shown = menu.custom.shown_rgb(Some(&b.saved));
    let finish = |m: bool| if m { "Metallic" } else { "Gloss" };
    let (mut cards, mut items) = (Vec::new(), Vec::new());
    match frame.tab {
        0 | 1 => {
            let list = if frame.tab == 0 { &factory } else { &specials };
            for p in list {
                let current = shown == Some((p.rgb, p.metallic));
                cards.push(Card {
                    title: p.name.clone(),
                    sub: format!("{}{}", finish(p.metallic), if p.stock { "  ·  stock" } else { "" }),
                    swatch: Some(customize::srgb(p.rgb)),
                    tag: current.then(|| Badge::new("CURRENT", ACCENT)),
                    ..default()
                });
                items.push(CardItem::Tune(Item::Change(api::Change::Paint(Some(p.paint)))));
            }
        }
        _ => {
            let custom = matches!(b.saved.paint, Some(Paint::Custom { .. })) && !specials.iter().any(|p| shown == Some((p.rgb, p.metallic)));
            cards.push(Card {
                title: "Custom colour".into(),
                sub: "Hue, saturation, brightness, finish".into(),
                swatch: shown.filter(|_| custom).map(|(rgb, _)| customize::srgb(rgb)),
                glyph: Some("HSV".into()),
                tag: custom.then(|| Badge::new("CURRENT", ACCENT)),
                ..default()
            });
            items.push(CardItem::Tune(Item::Custom));
        }
    }
    let details = match items.get(frame.cursor) {
        Some(CardItem::Tune(Item::Change(api::Change::Paint(Some(p))))) => {
            let (name, rgb, metallic) = factory.iter().chain(&specials).find(|o| o.paint == *p).map_or((String::new(), 0, false), |o| (o.name.clone(), o.rgb, o.metallic));
            Some(Details {
                title: name,
                sub: format!("#{rgb:06X}  ·  {}", finish(metallic)),
                price: Some(("Free".into(), Color::WHITE)),
                action: Some("Apply".into()),
                ..default()
            })
        }
        _ => Some(Details { title: "Custom colour".into(), sub: "Mix your own paint".into(), action: Some("Open".into()), ..default() }),
    };
    View {
        title: "Paint".into(),
        crumbs: b.crumbs("Paint"),
        tabs: vec!["Factory".into(), "Special".into(), "Custom".into()],
        tab: frame.tab,
        cards,
        items,
        layout: Some(Layout::Strip { visible: 7, w: 180.0, h: 170.0 }),
        details,
        hints: {
            let mut h = vec![hint("A", "Enter", "Apply", HintAct::Confirm), hint("B", "Esc", "Back", HintAct::Back)];
            h.push(hint("LB", "Q", "Factory / Special / Custom", HintAct::TabPrev));
            h
        },
        empty: "Loading colours…".into(),
        ..default()
    }
}

fn custom(b: &Base, st: &State) -> View {
    let rgb = customize::hsv_rgb(st.hsv);
    let sliders = vec![
        ("Hue".into(), format!("{:.0}°", st.hsv[0]), st.hsv[0] / 360.0, st.slider == 0),
        ("Saturation".into(), format!("{:.0} %", st.hsv[1] * 100.0), st.hsv[1], st.slider == 1),
        ("Brightness".into(), format!("{:.0} %", st.hsv[2] * 100.0), st.hsv[2], st.slider == 2),
        ("Finish".into(), if st.metallic { "Metallic".into() } else { "Gloss".into() }, if st.metallic { 1.0 } else { 0.0 }, st.slider == 3),
    ];
    View {
        title: "Custom colour".into(),
        crumbs: b.crumbs("Paint"),
        layout: Some(Layout::Strip { visible: 1, w: 180.0, h: 170.0 }),
        details: Some(Details {
            title: format!("#{rgb:06X}"),
            sub: if st.metallic { "Metallic".into() } else { "Gloss".into() },
            price: Some(("Free".into(), Color::WHITE)),
            action: Some("Apply".into()),
            ..default()
        }),
        sliders,
        hints: vec![
            hint("A", "Enter", "Apply", HintAct::Confirm),
            hint("B", "Esc", "Cancel", HintAct::Back),
            hint("LS", "Up / Down", "Choose", HintAct::None),
            hint("LS", "Left / Right", "Adjust", HintAct::None),
        ],
        ..default()
    }
}

fn rims(b: &Base, frame: &Frame, ctx: &Ctx, pending: &mut bool) -> View {
    let list = api::rims(&b.assets, &b.car, &b.saved, &b.saved);
    let (mut cards, mut items) = (Vec::new(), Vec::new());
    if !list.is_empty() {
        let stock = b.saved.rim.is_none();
        cards.push(Card { title: "Stock".into(), sub: "Factory wheels".into(), glyph: Some("STOCK".into()), tag: stock.then(|| Badge::new("INSTALLED", OWNED)), ..default() });
        items.push(CardItem::Tune(Item::Change(api::Change::Rim(None))));
    }
    for r in &list {
        let (tag, foot, foot_color) = price_bits(r.price, r.owned, r.installed, b.credits);
        cards.push(Card { title: r.name.clone(), sub: format!("{}  ·  {}", r.maker, r.kind), glyph: Some(initials(&r.maker)), tag, foot, foot_color, ..default() });
        items.push(CardItem::Tune(Item::Change(api::Change::Rim(Some(r.media.clone())))));
    }
    // Details of the focused rim: its mass changes the physics (unsprung), so the ratings can move.
    let details = items.get(frame.cursor).and_then(|it| match it {
        CardItem::Tune(Item::Change(change)) => {
            let after = b.after(ctx, change).ok().flatten();
            let r = frame.cursor.checked_sub(1).and_then(|i| list.get(i));
            let (title, sub) = r.map_or(("Stock".into(), "Factory wheels".into()), |r| (r.name.clone(), format!("{}  ·  {}  ·  {:.1} kg", r.maker, r.kind, r.mass)));
            let mut d = eval_details(title, sub, b.now.as_ref(), after.as_ref(), pending);
            match r {
                Some(r) => price_action(&mut d, r.price, r.owned, r.installed, b.credits),
                None => price_action(&mut d, 0, true, b.saved.rim.is_none(), b.credits),
            }
            Some(d)
        }
        _ => None,
    });
    View {
        title: "Rims".into(),
        crumbs: b.crumbs("Rims"),
        cards,
        items,
        layout: Some(STRIP),
        details,
        hints: hints_option(false),
        empty: "No rim options for this car".into(),
        ..default()
    }
}

fn kits(b: &Base, ctx: &Ctx) -> View {
    let _ = ctx;
    let list = api::kits(&b.assets, &b.car, &b.saved, &b.saved);
    let (mut cards, mut items) = (Vec::new(), Vec::new());
    for (i, slot) in list.iter().enumerate() {
        if slot.options.len() <= 1 {
            continue;
        }
        let on = slot.options.iter().find(|o| o.installed);
        cards.push(Card {
            title: slot.name.into(),
            sub: on.map_or_else(|| "Stock".into(), |o| if o.stock { "Stock".into() } else { format!("{}  ·  {}", o.name, o.level_label) }),
            glyph: Some(initials(slot.name)),
            tag: on.filter(|o| !o.stock).map(|_| Badge::new("CHANGED", ACCENT)),
            foot: Some(format!("{} options", slot.options.len() - 1)),
            foot_color: Some(Color::srgba(1.0, 1.0, 1.0, 0.6)),
            ..default()
        });
        items.push(CardItem::Tune(Item::Open(Screen::KitOptions(i))));
    }
    View { title: "Body kit".into(), crumbs: b.crumbs("Body kit"), cards, items, layout: Some(STRIP), hints: hints_select(), empty: "No body kit options for this car".into(), ..default() }
}

/// Option cards of one slot (kit or upgrade), with the PI change of each against the saved build.
fn option_cards(b: &Base, ctx: &Ctx, options: &[api::PartCard], change: impl Fn(&api::PartCard) -> api::Change, pending: &mut bool) -> (Vec<Card>, Vec<CardItem>) {
    let (mut cards, mut items) = (Vec::new(), Vec::new());
    for o in options {
        let c = change(o);
        let (tag, foot, foot_color) = price_bits(o.price, o.owned, o.installed, b.credits);
        let badge = pi_badge(b.now.as_ref(), &b.after(ctx, &c), pending);
        let mut sub = o.level_label.to_string();
        if !o.effect.is_empty() {
            sub = format!("{sub}  ·  {}", o.effect);
        }
        if !o.shows {
            sub = format!("{sub}  ·  no visual");
        }
        cards.push(Card { title: o.name.clone(), sub, glyph: Some(o.level_label.to_uppercase()), badge, tag, foot, foot_color, ..default() });
        items.push(CardItem::Tune(Item::Change(c)));
    }
    (cards, items)
}

fn option_details(b: &Base, ctx: &Ctx, o: &api::PartCard, change: &api::Change, pending: &mut bool) -> Details {
    let after = b.after(ctx, change);
    let sub = if o.effect.is_empty() { o.level_label.to_string() } else { format!("{}  ·  {}", o.level_label, o.effect) };
    let mut d = match &after {
        Err(why) => Details { title: o.name.clone(), sub, lines: vec![("Can't be fitted".into(), why.clone())], ..default() },
        Ok(a) => eval_details(o.name.clone(), sub, b.now.as_ref(), a.as_ref(), pending),
    };
    if after.is_ok() {
        price_action(&mut d, o.price, o.owned, o.installed, b.credits);
    }
    d
}

fn kit_options(b: &Base, frame: &Frame, ctx: &Ctx, k: usize, pending: &mut bool) -> View {
    let list = api::kits(&b.assets, &b.car, &b.saved, &b.saved);
    let Some(slot) = list.get(k) else { return View { title: "Body kit".into(), layout: Some(STRIP), empty: "Gone".into(), ..default() } };
    let key = slot.key.to_owned();
    let change = |o: &api::PartCard| api::Change::Kit { slot: key.clone(), row: (!o.stock).then_some(o.id.row) };
    let (cards, items) = option_cards(b, ctx, &slot.options, &change, pending);
    let details = slot.options.get(frame.cursor).map(|o| option_details(b, ctx, o, &change(o), pending));
    View { title: slot.name.into(), crumbs: b.crumbs("Body kit"), cards, items, layout: Some(STRIP), details, hints: hints_option(false), ..default() }
}

fn categories(b: &Base, ctx: &Ctx) -> View {
    let _ = ctx;
    let cats = api::categories(&b.assets, &b.car, &b.saved, &b.saved);
    let (mut cards, mut items) = (Vec::new(), Vec::new());
    for (i, c) in cats.iter().enumerate() {
        let fitted = c.slots.iter().filter(|s| s.options.iter().any(|o| o.installed && !o.stock)).count();
        cards.push(Card {
            title: c.name.into(),
            sub: format!("{} part{}", c.slots.len(), if c.slots.len() == 1 { "" } else { "s" }),
            glyph: Some(initials(c.name)),
            tag: (fitted > 0).then(|| Badge::new(format!("{fitted} FITTED"), ACCENT)),
            ..default()
        });
        items.push(CardItem::Tune(Item::OpenSlots(i)));
    }
    let mut pending = false;
    let details = Some(eval_details(b.title.clone(), "Current build".into(), b.now.as_ref(), None, &mut pending));
    View { title: "Upgrades".into(), crumbs: b.crumbs("Upgrades"), cards, items, layout: Some(STRIP), details, hints: hints_select(), empty: "No upgrades for this car".into(), pending, ..default() }
}

fn slots(b: &Base, frame: &Frame, ctx: &Ctx) -> View {
    let _ = ctx;
    let cats = api::categories(&b.assets, &b.car, &b.saved, &b.saved);
    let tab = frame.tab.min(cats.len().saturating_sub(1));
    let (mut cards, mut items) = (Vec::new(), Vec::new());
    if let Some(cat) = cats.get(tab) {
        for (j, slot) in cat.slots.iter().enumerate() {
            let on = slot.options.iter().find(|o| o.installed);
            cards.push(Card {
                title: slot.name.into(),
                sub: on.map_or_else(|| "None".into(), |o| format!("{}  ·  {}", o.level_label, o.name)),
                glyph: Some(on.map_or_else(|| "—".into(), |o| o.level_label.to_uppercase())),
                tag: on.filter(|o| !o.stock).map(|_| Badge::new("UPGRADED", ACCENT)),
                foot: Some(format!("{} options", slot.options.len())),
                foot_color: Some(Color::srgba(1.0, 1.0, 1.0, 0.6)),
                ..default()
            });
            items.push(CardItem::Tune(Item::Open(Screen::Parts(tab, j))));
        }
    }
    View {
        title: cats.get(tab).map_or("Upgrades", |c| c.name).into(),
        crumbs: b.crumbs("Upgrades"),
        tabs: cats.iter().map(|c| c.name.to_string()).collect(),
        tab,
        cards,
        items,
        layout: Some(STRIP),
        hints: {
            let mut h = hints_select();
            h.push(hint("LB", "Q", "Category", HintAct::TabPrev));
            h.push(hint("RB", "E", "Category", HintAct::TabNext));
            h
        },
        empty: "No parts in this category".into(),
        ..default()
    }
}

fn parts(b: &Base, frame: &Frame, ctx: &Ctx, c: usize, sl: usize, pending: &mut bool) -> View {
    let cats = api::categories(&b.assets, &b.car, &b.saved, &b.saved);
    let Some(slot) = cats.get(c).and_then(|cat| cat.slots.get(sl)) else { return View { title: "Upgrades".into(), layout: Some(STRIP), empty: "Gone".into(), ..default() } };
    let change = |o: &api::PartCard| api::Change::Part { table: o.id.table.clone(), row: o.id.row };
    let (cards, items) = option_cards(b, ctx, &slot.options, &change, pending);
    let details = slot.options.get(frame.cursor).map(|o| option_details(b, ctx, o, &change(o), pending));
    View {
        title: slot.name.into(),
        crumbs: b.crumbs(&format!("Upgrades  ·  {}", cats[c].name)),
        cards,
        items,
        layout: Some(STRIP),
        details,
        hints: hints_option(false),
        ..default()
    }
}
