//! Card menus (2026-10-09; docs/UI.md "Card menus"): the pause menu's Garage pages (Garage hub, My cars, Autoshow,
//! Change car, Customize) as controller-first cards instead of lists, after FH1's own horizontal pane rows
//! (115_c_buy_mfrselect / 108_c_buy_carselect: slanted panes scrolled sideways, a CAR_STATS card, a button legend).
//!
//! - Screens stack: Garage hub -> Autoshow makers -> cars (-> BUY CAR? dialog) / My cars -> car actions / Customize hub
//!   -> Paint / Rims / Body kit / Upgrades. B (Backspace / Esc) pops one screen, Start resumes driving.
//! - Pad: D-pad / left stick move the focus (held = accelerating repeat, browser.rs `AxisRepeat`), A select, B back,
//!   LB / RB tabs (class / game / part category), LT / RT page, X filter, Y sort, R3 sell. Keyboard: arrows / WASD,
//!   Enter / Space, Backspace / Esc, Q / E, PgUp / PgDn (Z / C), F, Tab, Delete. Mouse: hover focuses a card, click
//!   selects, the wheel scrolls, tab chips and the legend's chips are buttons.
//! - Look: the HUD font (`UiFont`) and colours (`ACCENT` pink, `PANEL`); the focused card grows (UiTransform scale,
//!   eased) and takes the accent border + glow. Class badges use [`class_color`] (our choice: no colours in the data,
//!   the game draws `CLASS_<x>.TGA` textures that aren't converted yet).
//! - Shop logic stays where it was: buying / selling = ui/garage.rs `run_shop` (progression::wallet); paint / rims /
//!   kit / upgrades = ui/customize.rs `CustomizeMenu` (preview on the player car, commit, charge), whose part / kit /
//!   PI logic belongs to the upgrades worker (ui/customize_upgrades.rs, pi.rs). Screens here only show and request.
//!
//! ui.rs steps aside while [`owns`] is true (draw_menu hides its panel, menu_input / menu_mouse return).
//! `FH1_CARD_MENUS=0` = the old list pages.

use std::collections::HashMap;

use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::input::mouse::AccumulatedMouseScroll;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use super::browser::AxisRepeat;
use super::customize_upgrades::garage_api as api;
use super::{Menu, Page, Settings, SettingsPath, UiFont, ACCENT, PANEL};
use crate::Garage;

mod shop;
mod tune;

/// Card menus on (`FH1_CARD_MENUS=0` = the old list pages).
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_CARD_MENUS").map_or(true, |v| v != "0"))
}

/// The card menus own the open pause menu's Garage pages: ui.rs leaves their input and drawing to this module.
pub(super) fn owns(menu: &Menu) -> bool {
    on() && menu.open && matches!(menu.page, Page::Garage | Page::Cars | Page::Customize)
}

pub struct CardsPlugin;

impl Plugin for CardsPlugin {
    fn build(&self, app: &mut App) {
        if !on() {
            return;
        }
        app.init_resource::<Cards>().add_systems(
            Update,
            (
                // Input before the shop / customize request handlers, drawing after them (same frame results).
                cards_input.after(super::menu_input).after(super::menu_mouse).before(super::garage::run_shop).before(super::customize::apply_requests),
                (cards_draw, cards_focus).chain().after(super::garage::run_shop).after(super::customize::apply_requests).before(super::draw_menu),
            ),
        );
    }
}

// ------------------------------------------------------------------ model

/// A coloured chip (class badge, OWNED tag).
#[derive(Clone, Debug)]
pub(super) struct Badge {
    pub text: String,
    pub color: Color,
}

impl Badge {
    pub fn new(text: impl Into<String>, color: Color) -> Self {
        Self { text: text.into(), color }
    }
}

/// One card.
#[derive(Clone, Debug, Default)]
pub(super) struct Card {
    pub title: String,
    pub sub: String,
    /// Photo (car thumbnail).
    pub image: Option<Handle<Image>>,
    /// Paint cards: a colour block instead of a photo.
    pub swatch: Option<Color>,
    /// Big text in the art area when there is no photo (maker initials, hub tiles).
    pub glyph: Option<String>,
    /// Top-left chip (class / PI).
    pub badge: Option<Badge>,
    /// Top-right chip (OWNED, INSTALLED, CURRENT).
    pub tag: Option<Badge>,
    /// Bottom line (price).
    pub foot: Option<String>,
    pub foot_color: Option<Color>,
    /// Shown but can't be chosen (greyed).
    pub locked: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Layout {
    /// A few big tiles in one row (hubs).
    Hub { w: f32, h: f32 },
    /// `cols` x `rows` visible cards of `w` x `h`; scrolls by rows. The details panel (if any) is on the right.
    Grid { cols: usize, rows: usize, w: f32, h: f32 },
    /// One row along the bottom, `visible` cards; scrolls sideways. The car stays visible above (live preview).
    Strip { visible: usize, w: f32, h: f32 },
}

impl Layout {
    fn cols(&self, n: usize) -> usize {
        match *self {
            Layout::Grid { cols, .. } => cols.max(1),
            _ => n.max(1),
        }
    }
}

/// A rating bar (0..1) with the value it would have after the focused choice (upgrades, paint has none).
#[derive(Clone, Debug)]
pub(super) struct Bar {
    pub label: String,
    pub now: f32,
    pub after: Option<f32>,
    pub text: String,
}

/// The focused card's details (right-hand panel).
#[derive(Clone, Debug, Default)]
pub(super) struct Details {
    pub title: String,
    pub sub: String,
    pub image: Option<Handle<Image>>,
    pub badge: Option<Badge>,
    pub bars: Vec<Bar>,
    pub lines: Vec<(String, String)>,
    /// Price / value line in big type, and its colour (red when short of credits).
    pub price: Option<(String, Color)>,
    /// What A does here ("BUY", "DRIVE", "INSTALL").
    pub action: Option<String>,
}

/// A button-legend entry (FH1's BUTTON_LOGO + Text_BUTTON pairs).
#[derive(Clone, Debug)]
pub(super) struct Hint {
    pub pad: &'static str,
    pub key: &'static str,
    pub label: String,
    /// Clicking the chip does this.
    pub act: HintAct,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum HintAct {
    Confirm,
    Back,
    TabPrev,
    TabNext,
    Filter,
    Sort,
    Alt,
    Sell,
    None,
}

pub(super) fn hint(pad: &'static str, key: &'static str, label: impl Into<String>, act: HintAct) -> Hint {
    Hint { pad, key, label: label.into(), act }
}

/// What a card stands for (activation).
#[derive(Clone, Debug)]
pub(super) enum Item {
    Hub(shop::HubTile),
    Maker(String),
    /// A car: `Garage::cars` index.
    Car(usize),
    Tune(tune::Item),
}

/// A built screen: what's drawn and what each card does.
#[derive(Default)]
pub(super) struct View {
    pub title: String,
    pub crumbs: String,
    pub tabs: Vec<String>,
    pub tab: usize,
    /// Filter / sort state line under the tabs ("Drive: RWD · Sort: price").
    pub status: String,
    pub cards: Vec<Card>,
    pub items: Vec<Item>,
    pub layout: Option<Layout>,
    pub details: Option<Details>,
    pub hints: Vec<Hint>,
    /// Shown when there are no cards.
    pub empty: String,
    /// Dark full-screen backdrop (shop pages); Customize keeps the car visible.
    pub backdrop: bool,
    /// Extra panel above the strip (custom colour sliders): label, value text, 0..1, focused.
    pub sliders: Vec<(String, String, f32, bool)>,
    /// Some values are still being computed (garage API PI / ratings): redraw shortly.
    pub pending: bool,
}

/// A yes / no (or several-way) dialog over the cards.
pub(super) struct Dialog {
    pub title: String,
    pub message: String,
    pub options: Vec<(String, DialogAct)>,
    pub cursor: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DialogAct {
    Close,
    Buy(usize),
    Sell(usize),
    Drive(usize),
    /// Drive the car (if needed), then open Customize on it.
    Customize(usize),
    /// Customize > Reset to stock (garage API).
    ResetStock,
}

/// A screen on the stack and its own cursor / tab / filter / sort.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Screen {
    Hub,
    /// Manufacturers of the Autoshow (`Mode::Shop`) or of every car (`Mode::All`, ownership off).
    Makers(super::garage::Mode),
    /// Cars: Autoshow / every car of one maker (`Some`), or every listed car (`None`; My cars).
    Cars(super::garage::Mode, Option<String>),
    Tune(tune::Screen),
}

#[derive(Clone, Debug)]
pub(super) struct Frame {
    pub screen: Screen,
    pub cursor: usize,
    pub tab: usize,
    pub filter: usize,
    pub sort: usize,
}

impl Frame {
    pub fn new(screen: Screen) -> Self {
        Self { screen, cursor: 0, tab: 0, filter: 0, sort: 0 }
    }
}

/// The card menus' state.
#[derive(Resource, Default)]
pub(super) struct Cards {
    pub stack: Vec<Frame>,
    pub dialog: Option<Dialog>,
    /// The pause page seen last frame (entering a page resets the stack).
    page: Option<Page>,
    /// Rebuild everything / only the focus-dependent parts (details, legend) this frame.
    pub dirty: bool,
    pub focus_dirty: bool,
    repeat: (AxisRepeat, AxisRepeat),
    /// The last build: items (activation), layout, first visible card (scroll) and the card entities.
    items: Vec<Item>,
    layout: Option<Layout>,
    first: usize,
    hints: Vec<Hint>,
    /// Customize on this car once it is the driven car (My cars > Customize on another car).
    pub customize_after: Option<usize>,
    /// Notice line (shop results), and what it was built from.
    pub notice: Option<String>,
    /// customize.rs `CustomizeMenu::generation` the Tune screens were built from.
    tune_gen: u64,
    /// ui/garage.rs notice last shown (a new one = a buy / sell finished: rebuild).
    shop_notice: Option<String>,
    /// Debounced focus preview (Customize): time left before the focused card previews.
    pub preview_in: Option<f32>,
    /// Tabs of the last built view (LB / RB wrap).
    tab_count: usize,
    /// Customize screens' own state (custom colour editor, preview on).
    pub tune: tune::State,
    /// A view waits for computed values: time until the next redraw, and redraws spent waiting since the last real
    /// change (capped: a build whose evaluation failed reads as "computing" forever).
    wait_t: Option<f32>,
    wait_n: u32,
}

impl Cards {
    pub fn top(&self) -> Option<&Frame> {
        self.stack.last()
    }
    pub fn top_mut(&mut self) -> Option<&mut Frame> {
        self.stack.last_mut()
    }
    pub fn push(&mut self, screen: Screen) {
        self.stack.push(Frame::new(screen));
        self.notice = None;
        self.dirty = true;
    }
}

/// The pause page that matches the top screen (ui.rs back-cursor logic and `owns` follow it).
fn sync_page(cards: &Cards, menu: &mut Menu) {
    let page = match cards.top().map(|f| &f.screen) {
        Some(Screen::Hub) => Page::Garage,
        Some(Screen::Makers(_) | Screen::Cars(..)) => Page::Cars,
        Some(Screen::Tune(_)) => Page::Customize,
        None => return,
    };
    menu.page = page;
}

/// Class badge colours (FH-style; our choice, see module doc).
pub(super) fn class_color(class: &str) -> Color {
    match class {
        "D" => Color::srgb(0.27, 0.66, 0.95),
        "C" => Color::srgb(0.97, 0.78, 0.13),
        "B" => Color::srgb(0.98, 0.50, 0.12),
        "A" => Color::srgb(0.90, 0.20, 0.22),
        "S" | "S1" | "S2" => Color::srgb(0.62, 0.32, 0.92),
        "R3" | "R2" | "R1" | "X" => Color::srgb(0.18, 0.36, 0.86),
        "U" | "P" => Color::srgb(0.80, 0.66, 0.24),
        _ => Color::srgb(0.45, 0.47, 0.50),
    }
}

pub(super) const GOOD: Color = Color::srgb(0.30, 0.82, 0.38);
pub(super) const BAD: Color = Color::srgb(0.92, 0.26, 0.24);
pub(super) const OWNED: Color = Color::srgb(0.20, 0.62, 0.36);
const CARD_BG: Color = Color::srgba(0.07, 0.08, 0.10, 0.92);
const CARD_BG_ON: Color = Color::srgba(0.11, 0.12, 0.15, 0.96);
const ART_BG: Color = Color::srgb(0.13, 0.14, 0.17);
const DIM: Color = Color::srgba(1.0, 1.0, 1.0, 0.6);
const FAINT: Color = Color::srgba(1.0, 1.0, 1.0, 0.35);
const BACKDROP: Color = Color::srgba(0.02, 0.02, 0.03, 0.86);

// ------------------------------------------------------------------ input

/// One frame of card input (pad, keyboard; the mouse is read separately).
#[derive(Default, Clone, Copy, Debug)]
pub(super) struct CardInput {
    pub dx: i32,
    pub dy: i32,
    /// The direction keys are held (auto-repeat): stop at the ends instead of wrapping.
    pub held: bool,
    pub tab: i32,
    pub page: i32,
    pub filter: bool,
    pub sort: bool,
    pub alt: bool,
    pub sell: bool,
    pub confirm: bool,
    pub back: bool,
    pub resume: bool,
}

fn read_input(keys: &ButtonInput<KeyCode>, pads: &Query<&Gamepad>, repeat: &mut (AxisRepeat, AxisRepeat), dt: f32) -> CardInput {
    use GamepadButton as P;
    use KeyCode as K;
    let kp = |k: K| keys.just_pressed(k);
    let kh = |k: K| keys.pressed(k);
    let pp = |b: P| pads.iter().any(|p| p.just_pressed(b));
    let ph = |b: P| pads.iter().any(|p| p.pressed(b));
    let stick = |a: GamepadAxis| pads.iter().map(|p| p.get(a).unwrap_or(0.0)).fold(0.0f32, |m, v| if v.abs() > m.abs() { v } else { m });
    let dir = |neg: bool, pos: bool| if neg && !pos { -1 } else if pos && !neg { 1 } else { 0 };
    let sx = stick(GamepadAxis::LeftStickX);
    let sy = stick(GamepadAxis::LeftStickY);
    let h = dir(kh(K::ArrowLeft) || kh(K::KeyA) || ph(P::DPadLeft) || sx < -0.55, kh(K::ArrowRight) || kh(K::KeyD) || ph(P::DPadRight) || sx > 0.55);
    // Screen down = +1 (stick up is +Y).
    let v = dir(kh(K::ArrowUp) || kh(K::KeyW) || ph(P::DPadUp) || sy > 0.55, kh(K::ArrowDown) || kh(K::KeyS) || ph(P::DPadDown) || sy < -0.55);
    let (dx, hx) = repeat.0.update(h, dt);
    let (dy, hy) = repeat.1.update(v, dt);
    CardInput {
        dx,
        dy,
        held: hx || hy,
        tab: dir(pp(P::LeftTrigger) || kp(K::KeyQ), pp(P::RightTrigger) || kp(K::KeyE)),
        page: dir(pp(P::LeftTrigger2) || kp(K::PageUp) || kp(K::KeyZ), pp(P::RightTrigger2) || kp(K::PageDown) || kp(K::KeyC)),
        filter: pp(P::West) || kp(K::KeyF),
        sort: pp(P::North) || kp(K::Tab),
        alt: pp(P::Select) || kp(K::KeyG),
        sell: pp(P::RightThumb) || kp(K::Delete),
        confirm: pp(P::South) || kp(K::Enter) || kp(K::NumpadEnter) || kp(K::Space),
        back: pp(P::East) || kp(K::Backspace) || kp(K::Escape),
        resume: pp(P::Start),
    }
}

/// What a screen's `activate` may do: open screens / dialogs, drive a car, reach the menu and shop state.
pub(super) struct Act<'a, 'w> {
    pub cards: &'a mut Cards,
    pub menu: &'a mut Menu,
    pub settings: &'a mut Settings,
    pub path: &'a SettingsPath,
    pub garage: &'a Garage,
    pub actions: &'a mut MessageWriter<'w, super::GameAction>,
    /// Read-only career state (prices, ownership, sell values; progression::wallet).
    pub profile: Option<&'a crate::progression::Profile>,
    pub events: Option<&'a crate::race::Events>,
    /// Saved looks (parts value for sell prices: `CarLooks::spent`).
    pub looks: &'a super::customize::CarLooks,
}

impl Act<'_, '_> {
    /// Drive garage car `i` and resume.
    pub fn drive(&mut self, i: usize) {
        drive(i, self.menu, self.settings, self.path, self.garage, self.actions);
    }
    pub fn push(&mut self, screen: Screen) {
        self.cards.push(screen);
    }
    pub fn dialog(&mut self, d: Dialog) {
        self.cards.dialog = Some(d);
        self.cards.dirty = true;
    }
    /// Open Customize on the driven car (Garage hub tile).
    pub fn customize(&mut self) {
        tune::open(self.cards, self.menu);
    }
}

/// Card entity (mouse target): index into the view's cards.
#[derive(Component)]
struct CardNode(usize);

/// Tab chip (mouse): tab index.
#[derive(Component)]
struct TabChip(usize);

/// Legend chip (mouse).
#[derive(Component)]
struct HintChip(HintAct);

/// Dialog option (mouse).
#[derive(Component)]
struct DialogChip(usize);

/// The card layer's root (full screen, over the world, under notifications).
#[derive(Component)]
struct CardsRoot;

/// The details panel / legend: rebuilt when only the focus changed.
#[derive(Component)]
struct DetailsSlot;
#[derive(Component)]
struct LegendSlot;

/// Focus animation of a card: current scale.
#[derive(Component)]
struct CardFx {
    index: usize,
    scale: f32,
}

/// Everything the screens read to build a view (and act on a choice).
#[derive(bevy::ecs::system::SystemParam)]
pub(super) struct Ctx<'w> {
    pub garage: Res<'w, Garage>,
    pub profile: Option<Res<'w, crate::progression::Profile>>,
    pub events: Option<Res<'w, crate::race::Events>>,
    pub looks: Res<'w, super::customize::CarLooks>,
    pub asset_server: Res<'w, AssetServer>,
    pub studio: Option<ResMut<'w, super::thumbs::ThumbStudio>>,
    pub images: Res<'w, Assets<Image>>,
    /// The upgrades worker's read API (ui/customize_upgrades/garage_api.rs): parts, kits, rims, PI / ratings.
    pub api: api::GarageApi<'w>,
    /// Its evaluation cache (PI / ratings / specs of any build; async, None = computing).
    pub eval: Res<'w, api::GarageEval>,
}

impl Ctx<'_> {
    /// Photo of `car` (`Garage::cars` index): the game's own thumbnail for FH1 cars; else the thumbnail studio's
    /// render, queued only for the `focused` card (thumbs.rs keeps 3 in flight).
    pub fn photo(&mut self, e: &super::browser::CarEntry, focused: bool) -> Option<Handle<Image>> {
        if let Some(id) = e.id {
            return Some(self.asset_server.load(format!("ui/textures/thumbnails/thumbnail_{id}.png")));
        }
        let car = self.garage.cars.get(e.index)?.clone();
        let studio = self.studio.as_deref_mut()?;
        if focused { studio.photo(&car, &self.images) } else { studio.cached(&car) }
    }

    pub fn credits(&self) -> Option<i64> {
        let p = self.profile.as_deref()?;
        super::garage::shop_on().then(|| crate::progression::wallet::credits(p))
    }
}

// ------------------------------------------------------------------ logic

#[allow(clippy::too_many_arguments)]
fn cards_input(
    mut cards: ResMut<Cards>,
    mut menu: ResMut<Menu>,
    (mut settings, path): (ResMut<Settings>, Res<SettingsPath>),
    (garage, profile, events): (Res<Garage>, Option<Res<crate::progression::Profile>>, Option<Res<crate::race::Events>>),
    (keys, pads, time): (Res<ButtonInput<KeyCode>>, Query<&Gamepad>, Res<Time<Real>>),
    (scroll, card_hits, tab_hits, hint_hits, dialog_hits): (
        Res<AccumulatedMouseScroll>,
        Query<(&Interaction, &CardNode), Changed<Interaction>>,
        Query<(&Interaction, &TabChip), Changed<Interaction>>,
        Query<(&Interaction, &HintChip), Changed<Interaction>>,
        Query<(&Interaction, &DialogChip), Changed<Interaction>>,
    ),
    motion: Res<bevy::input::mouse::AccumulatedMouseMotion>,
    mut actions: MessageWriter<super::GameAction>,
    mut snd: MessageWriter<super::sfx::UiSfx>,
    (mut garage_actions, garage_api, looks): (MessageWriter<api::GarageAction>, api::GarageApi, Res<super::customize::CarLooks>),
) {
    let cards = &mut *cards;
    let menu = &mut *menu;
    let car = garage_api.driven().to_owned();
    if !owns(menu) {
        if cards.page.is_some() {
            // Left the pages any other way (menu closed, map switch): drop a running preview.
            tune::end_preview(cards, &mut garage_actions, &car);
            *cards = Cards::default();
        }
        return;
    }
    // Entering a Garage page from the pause menu starts its stack (moves between them keep it: sync_page).
    if cards.page != Some(menu.page) {
        let fresh = cards.page.is_none();
        cards.page = Some(menu.page);
        if fresh || cards.stack.is_empty() {
            cards.stack = vec![Frame::new(match menu.page {
                Page::Garage => Screen::Hub,
                Page::Customize => Screen::Tune(tune::Screen::Hub),
                _ => match menu.shop.mode {
                    super::garage::Mode::Owned => Screen::Cars(super::garage::Mode::Owned, None),
                    m => Screen::Makers(m),
                },
            })];
            if menu.page == Page::Customize {
                menu.custom.open();
            }
        }
        cards.dialog = None;
        cards.dirty = true;
        // The press that opened the page (ui.rs menu_input ran first this frame) must not also pick a card.
        return;
    }
    // A car just bought (garage.rs run_shop): drive it.
    if let Some(i) = menu.shop.drive.take() {
        drive(i, menu, &mut settings, &path, &garage, &mut actions);
        return;
    }
    // My cars > Customize on another car: open Customize once that car is the driven one.
    if let Some(i) = cards.customize_after {
        if garage.current == i {
            cards.customize_after = None;
            tune::open(cards, menu);
        }
    }
    let input = read_input(&keys, &pads, &mut cards.repeat, time.delta_secs());
    if input.resume {
        tune::end_preview(cards, &mut garage_actions, &car);
        super::close_menu(menu, &settings, &path);
        return;
    }

    // Mouse.
    let moved = motion.delta != Vec2::ZERO;
    let mut click_card = None;
    for (i, c) in &card_hits {
        match i {
            Interaction::Hovered if moved && cards.dialog.is_none() => set_cursor(cards, c.0),
            Interaction::Pressed => click_card = Some(c.0),
            _ => {}
        }
    }
    let hint_act = hint_hits.iter().find(|(i, _)| **i == Interaction::Pressed).map(|(_, h)| h.0);
    let tab_click = tab_hits.iter().find(|(i, _)| **i == Interaction::Pressed).map(|(_, t)| t.0);
    let mut dialog_click = None;
    for (i, d) in &dialog_hits {
        match i {
            Interaction::Hovered if moved => {
                if let Some(dl) = cards.dialog.as_mut() {
                    if dl.cursor != d.0 {
                        dl.cursor = d.0;
                        cards.dirty = true;
                    }
                }
            }
            Interaction::Pressed => dialog_click = Some(d.0),
            _ => {}
        }
    }

    // The dialog takes the input while it is up.
    if let Some(dl) = cards.dialog.as_mut() {
        let n = dl.options.len().max(1);
        let step = input.dx + input.dy;
        if step != 0 {
            snd.write(super::sfx::UiSfx::play(super::sfx::keys::VSCROLL));
            dl.cursor = (dl.cursor as i32 + step.signum()).rem_euclid(n as i32) as usize;
            cards.dirty = true;
        }
        let pick = if let Some(k) = dialog_click {
            Some(k)
        } else if input.confirm || hint_act == Some(HintAct::Confirm) {
            Some(dl.cursor)
        } else if input.back || hint_act == Some(HintAct::Back) {
            Some(usize::MAX)
        } else {
            None
        };
        if let Some(k) = pick {
            snd.write(super::sfx::UiSfx::play(if k == usize::MAX { super::sfx::keys::CANCEL } else { super::sfx::keys::ACCEPT }));
            let act = dl.options.get(k).map_or(DialogAct::Close, |o| o.1);
            cards.dialog = None;
            cards.dirty = true;
            match act {
                DialogAct::Close => {}
                DialogAct::Buy(i) => menu.shop.request_buy(i),
                DialogAct::Sell(i) => menu.shop.request_sell(i),
                DialogAct::Drive(i) => {
                    drive(i, menu, &mut settings, &path, &garage, &mut actions);
                    return;
                }
                DialogAct::Customize(i) => {
                    if i == garage.current {
                        tune::open(cards, menu);
                    } else {
                        actions.write(super::GameAction::SelectCar(i));
                        settings.car = garage.cars.get(i).cloned();
                        cards.customize_after = Some(i);
                    }
                }
                DialogAct::ResetStock => {
                    garage_actions.write(api::GarageAction::ResetStock { car: car.clone() });
                }
            }
        }
        return;
    }

    // Screens that read the stick themselves (the custom colour editor).
    let own = match cards.top().map(|f| f.screen.clone()) {
        Some(Screen::Tune(s)) => tune::input(cards, &s, &input),
        _ => false,
    };

    let n = cards.items.len();
    let layout = cards.layout;
    // Scroll wheel: a row of the grid, or a card of a strip / hub.
    let wheel = scroll.delta.y;
    let wheel_step = if wheel > 0.0 { -1 } else if wheel < 0.0 { 1 } else { 0 };
    if let (false, Some(l), true) = (own, layout, n > 0) {
        let cols = l.cols(n) as i32;
        let (mut dx, mut dy) = (input.dx, input.dy);
        if wheel_step != 0 {
            match l {
                Layout::Grid { .. } => dy += wheel_step,
                _ => dx += wheel_step,
            }
        }
        // Strip / hub: one row, so up / down step too.
        if !matches!(l, Layout::Grid { .. }) && dx == 0 {
            dx = dy;
            dy = 0;
        }
        let cursor = cards.top().map_or(0, |f| f.cursor);
        let mut c = cursor as i32;
        if dx != 0 {
            c = if input.held { (c + dx).clamp(0, n as i32 - 1) } else { (c + dx).rem_euclid(n as i32) };
        }
        if dy != 0 {
            let rows = (n as i32 + cols - 1) / cols;
            let (r, col) = (c / cols, c % cols);
            let r2 = if input.held { (r + dy).clamp(0, rows - 1) } else { (r + dy).rem_euclid(rows) };
            c = (r2 * cols + col).min(n as i32 - 1);
        }
        if input.page != 0 {
            let page = match l {
                Layout::Grid { cols, rows, .. } => (cols * rows) as i32,
                Layout::Strip { visible, .. } => visible as i32,
                Layout::Hub { .. } => 1,
            };
            c = (c + input.page * page).clamp(0, n as i32 - 1);
        }
        if c as usize != cursor {
            snd.write(super::sfx::UiSfx::play(if matches!(l, Layout::Grid { .. }) { super::sfx::keys::GRID_SCROLL } else { super::sfx::keys::HSCROLL }));
            set_cursor(cards, c as usize);
        }
    }
    // Tabs (LB / RB, a clicked chip), filter (X), sort (Y).
    let tab_step = input.tab
        + match hint_act {
            Some(HintAct::TabPrev) => -1,
            Some(HintAct::TabNext) => 1,
            _ => 0,
        };
    let filter = input.filter || hint_act == Some(HintAct::Filter);
    let sort = input.sort || hint_act == Some(HintAct::Sort);
    let tabs = cards.tab_count;
    let mut changed = false;
    if let Some(f) = cards.stack.last_mut() {
        if tabs > 1 && (tab_step != 0 || tab_click.is_some()) {
            let t = match tab_click {
                Some(t) => t.min(tabs - 1),
                None => (f.tab as i32 + tab_step).rem_euclid(tabs as i32) as usize,
            };
            if t != f.tab {
                snd.write(super::sfx::UiSfx::play(super::sfx::keys::HSCROLL));
                f.tab = t;
                f.cursor = 0;
                changed = true;
            }
        }
        if filter {
            f.filter = f.filter.wrapping_add(1);
            f.cursor = 0;
            changed = true;
        }
        if sort {
            f.sort = f.sort.wrapping_add(1);
            changed = true;
        }
    }
    if changed {
        cards.dirty = true;
        if matches!(cards.top().map(|f| &f.screen), Some(Screen::Tune(_))) {
            cards.preview_in = Some(0.15);
        }
    }
    let alt = input.alt || hint_act == Some(HintAct::Alt);
    let sell = input.sell || hint_act == Some(HintAct::Sell);
    let confirm = input.confirm || hint_act == Some(HintAct::Confirm) || click_card.is_some();
    if let Some(k) = click_card {
        set_cursor(cards, k);
    }
    if confirm {
        snd.write(super::sfx::UiSfx::play(super::sfx::keys::ACCEPT));
    }
    if confirm || alt || sell {
        let Some(f) = cards.top() else { return };
        let item = cards.items.get(f.cursor).cloned();
        match f.screen.clone() {
            Screen::Tune(s) => {
                if confirm {
                    tune::activate(cards, menu, &s, item.as_ref(), &mut garage_actions, &garage_api);
                }
            }
            _ => {
                if let Some(item) = item {
                    let mut act = Act {
                        cards: &mut *cards,
                        menu: &mut *menu,
                        settings: &mut settings,
                        path: &path,
                        garage: &garage,
                        actions: &mut actions,
                        profile: profile.as_deref(),
                        events: events.as_deref(),
                        looks: &looks,
                    };
                    if shop::activate(&mut act, &item, confirm, sell) {
                        return;
                    }
                }
            }
        }
    }
    if input.back || hint_act == Some(HintAct::Back) {
        snd.write(super::sfx::UiSfx::play(super::sfx::keys::CANCEL));
        if let Some(Screen::Tune(s)) = cards.top().map(|f| f.screen.clone()) {
            tune::leaving(cards, &s, &mut garage_actions, &car);
        }
        cards.stack.pop();
        cards.notice = None;
        cards.dirty = true;
        if cards.stack.is_empty() {
            // Back to the pause menu's Change car row.
            menu.page = Page::Main;
            menu.cursor = 3;
            menu.dirty = true;
            *cards = Cards::default();
            return;
        }
        sync_page(cards, menu);
        cards.page = Some(menu.page);
    }
    // Customize: the focused card previews on the car after a short rest (fast scrolling doesn't respawn the body
    // every step).
    if let Some(t) = cards.preview_in.as_mut() {
        *t -= time.delta_secs();
        if *t <= 0.0 {
            cards.preview_in = None;
            tune::preview_focused(cards, &mut garage_actions, &car);
        }
    }
}

fn set_cursor(cards: &mut Cards, c: usize) {
    let tune = matches!(cards.top().map(|f| &f.screen), Some(Screen::Tune(_)));
    if let Some(f) = cards.stack.last_mut() {
        if f.cursor != c {
            f.cursor = c;
            cards.focus_dirty = true;
            if tune {
                cards.preview_in = Some(0.15);
            }
        }
    }
}

/// Drive garage car `i` and resume (the pause menu's car pick, ui.rs `browser_pick`).
fn drive(i: usize, menu: &mut Menu, settings: &mut Settings, path: &SettingsPath, garage: &Garage, actions: &mut MessageWriter<super::GameAction>) {
    if i != garage.current {
        actions.write(super::GameAction::SelectCar(i));
    }
    settings.car = garage.cars.get(i).cloned();
    super::close_menu(menu, settings, path);
}

// ------------------------------------------------------------------ drawing

/// Size scale: the layouts are drawn for 1080p and scaled with the window height (0.6..1.6).
#[derive(Clone, Copy)]
struct Sz(f32);

impl Sz {
    fn px(&self, v: f32) -> Val {
        Val::Px(v * self.0)
    }
    fn f(&self, v: f32) -> f32 {
        v * self.0
    }
}

#[allow(clippy::too_many_arguments)]
fn cards_draw(
    mut commands: Commands,
    mut cards: ResMut<Cards>,
    menu: Res<Menu>,
    mut ctx: Ctx,
    font: Res<UiFont>,
    roots: Query<Entity, With<CardsRoot>>,
    details: Query<Entity, With<DetailsSlot>>,
    legend: Query<Entity, With<LegendSlot>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut last_height: Local<f32>,
    (mut results, time): (MessageReader<api::GarageResult>, Res<Time<Real>>),
) {
    let cards = &mut *cards;
    if !owns(&menu) {
        for r in &roots {
            commands.entity(r).despawn();
        }
        results.clear();
        return;
    }
    // Garage API results (install / reset): the notice line, and the cards show the new build.
    for r in results.read() {
        cards.notice = Some(tune::outcome_text(&r.outcome));
        cards.dirty = true;
    }
    // Values still computing (PI / ratings): redraw every 0.2 s until they are in.
    let mut from_wait = false;
    if let Some(t) = cards.wait_t.as_mut() {
        *t -= time.delta_secs();
        if *t <= 0.0 {
            cards.wait_t = None;
            from_wait = !cards.dirty && !cards.focus_dirty;
            cards.dirty = true;
        }
    }
    // Rebuild when the content behind a screen changed: a shop result, the Customize rows, the window size.
    if menu.shop.notice != cards.shop_notice {
        cards.shop_notice = menu.shop.notice.clone();
        cards.notice = menu.shop.notice.clone();
        cards.dirty = true;
    }
    if matches!(cards.top().map(|f| &f.screen), Some(Screen::Tune(_))) && menu.custom.generation != cards.tune_gen {
        cards.tune_gen = menu.custom.generation;
        cards.dirty = true;
    }
    let height = windows.single().map_or(1080.0, |w| w.height());
    if (height - *last_height).abs() > 1.0 {
        *last_height = height;
        cards.dirty = true;
    }
    if roots.is_empty() {
        cards.dirty = true;
    }
    if !cards.dirty && !cards.focus_dirty {
        return;
    }
    let sz = Sz((height / 1080.0).clamp(0.6, 1.6));
    if !from_wait {
        cards.wait_n = 0;
    }
    let mut view = View::default();
    // Built twice at most: when the list shrank under the cursor (a sold car, a filter), again with the clamped cursor so
    // the details match the focused card.
    for _ in 0..2 {
        let Some(frame) = cards.stack.last().cloned() else { return };
        view = match &frame.screen {
            Screen::Tune(s) => tune::view(s, &frame, &menu, &mut ctx, &cards.tune),
            _ => shop::view(&frame, &menu, &mut ctx),
        };
        let Some(f) = cards.stack.last_mut() else { return };
        let (cursor, tab) = (f.cursor, f.tab);
        f.cursor = f.cursor.min(view.cards.len().saturating_sub(1));
        if !view.tabs.is_empty() && f.tab >= view.tabs.len() {
            f.tab = 0;
        }
        if (f.cursor, f.tab) == (cursor, tab) {
            break;
        }
    }
    let cursor = cards.top().map_or(0, |f| f.cursor);
    cards.tab_count = view.tabs.len();
    if view.pending && cards.wait_t.is_none() && cards.wait_n < 50 {
        cards.wait_n += 1;
        cards.wait_t = Some(0.2);
    }
    if let Some(n) = cards.notice.clone() {
        view.status = if view.status.is_empty() { n } else { format!("{n}    ·    {}", view.status) };
    }
    // Scroll window: keep the focused card visible.
    let n = view.cards.len();
    let first = match view.layout {
        Some(Layout::Grid { cols, rows, .. }) => {
            let row = cursor / cols.max(1);
            let first_row = cards.first / cols.max(1);
            let first_row = if row < first_row { row } else if row >= first_row + rows { row + 1 - rows } else { first_row };
            (first_row * cols).min(n.saturating_sub(1) / cols.max(1) * cols)
        }
        Some(Layout::Strip { visible, .. }) => {
            let f = cards.first;
            if cursor < f { cursor } else if cursor >= f + visible { cursor + 1 - visible } else { f }
        }
        _ => 0,
    };
    let scrolled = first != cards.first || cards.layout != view.layout;
    cards.items = std::mem::take(&mut view.items);
    cards.layout = view.layout;
    cards.hints = view.hints.clone();
    let font = &*font;
    if cards.dirty || scrolled || roots.is_empty() {
        cards.first = first;
        for r in &roots {
            commands.entity(r).despawn();
        }
        spawn_root(&mut commands, font, sz, &view, cursor, first, cards.dialog.as_ref());
    } else {
        // Only the focus moved inside the visible window: details and legend follow, cards animate (cards_focus).
        for d in &details {
            commands.entity(d).despawn_children();
            commands.entity(d).with_children(|p| spawn_details(p, font, sz, view.details.as_ref()));
        }
        for l in &legend {
            commands.entity(l).despawn_children();
            commands.entity(l).with_children(|p| spawn_legend(p, font, sz, &view.hints));
        }
    }
    cards.dirty = false;
    cards.focus_dirty = false;
}

/// Eases the cards' scale and sets the focus border (every frame while the cards are up).
fn cards_focus(cards: Res<Cards>, time: Res<Time<Real>>, mut q: Query<(&mut CardFx, &mut UiTransform, &mut BorderColor, &mut BackgroundColor, &mut BoxShadow)>) {
    let cursor = cards.top().map_or(usize::MAX, |f| f.cursor);
    let dialog = cards.dialog.is_some();
    let k = 1.0 - (-time.delta_secs() * 16.0).exp();
    for (mut fx, mut tf, mut border, mut bg, mut shadow) in &mut q {
        let on = fx.index == cursor && !dialog;
        let want = if on { 1.07 } else { 1.0 };
        if (fx.scale - want).abs() > 1e-3 {
            fx.scale += (want - fx.scale) * k;
            tf.scale = Vec2::splat(fx.scale);
        }
        let b = if on { ACCENT } else { Color::NONE };
        if border.top != b {
            *border = BorderColor::all(b);
            bg.0 = if on { CARD_BG_ON } else { CARD_BG };
            shadow.0 = if on { glow() } else { Vec::new() };
        }
    }
}

fn glow() -> Vec<ShadowStyle> {
    vec![ShadowStyle { color: ACCENT.with_alpha(0.55), x_offset: Val::Px(0.0), y_offset: Val::Px(0.0), spread_radius: Val::Px(2.0), blur_radius: Val::Px(18.0) }]
}

fn text(p: &mut ChildSpawnerCommands, font: &UiFont, px: f32, s: impl Into<String>, color: Color) {
    p.spawn((Text::new(s), font.text(px), TextColor(color)));
}

fn chip(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, b: &Badge, px: f32) {
    p.spawn((Node { padding: UiRect::axes(sz.px(8.0), sz.px(2.0)), border_radius: BorderRadius::all(sz.px(4.0)), ..default() }, BackgroundColor(b.color)))
        .with_children(|c| text(c, font, sz.f(px), b.text.clone(), Color::WHITE));
}

fn spawn_root(commands: &mut Commands, font: &UiFont, sz: Sz, view: &View, cursor: usize, first: usize, dialog: Option<&Dialog>) {
    let root = commands
        .spawn((
            CardsRoot,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::axes(sz.px(64.0), sz.px(40.0)),
                ..default()
            },
            BackgroundColor(if view.backdrop { BACKDROP } else { Color::NONE }),
            GlobalZIndex(101),
        ))
        .id();
    commands.entity(root).with_children(|p| {
        // Header: crumbs + title on the left, credits on the right.
        p.spawn(Node { flex_direction: FlexDirection::Row, justify_content: JustifyContent::SpaceBetween, align_items: AlignItems::FlexEnd, ..default() }).with_children(|h| {
            h.spawn(Node { flex_direction: FlexDirection::Column, ..default() }).with_children(|t| {
                if !view.crumbs.is_empty() {
                    text(t, font, sz.f(18.0), view.crumbs.to_uppercase(), DIM);
                }
                text(t, font, sz.f(46.0), view.title.to_uppercase(), Color::WHITE);
            });
        });
        // Tabs (LB / RB).
        if !view.tabs.is_empty() {
            p.spawn(Node { flex_direction: FlexDirection::Row, column_gap: sz.px(6.0), align_items: AlignItems::Center, margin: UiRect::top(sz.px(14.0)), ..default() }).with_children(|t| {
                glyph(t, font, sz, "LB", HintAct::TabPrev);
                for (i, label) in view.tabs.iter().enumerate() {
                    let on = i == view.tab;
                    t.spawn((
                        TabChip(i),
                        Button,
                        Node { padding: UiRect::axes(sz.px(14.0), sz.px(5.0)), border_radius: BorderRadius::all(sz.px(4.0)), ..default() },
                        BackgroundColor(if on { ACCENT } else { Color::srgba(1.0, 1.0, 1.0, 0.08) }),
                    ))
                    .with_children(|c| text(c, font, sz.f(17.0), label.to_uppercase(), if on { Color::WHITE } else { DIM }));
                }
                glyph(t, font, sz, "RB", HintAct::TabNext);
            });
        }
        if !view.status.is_empty() {
            p.spawn((Text::new(view.status.clone()), font.text(sz.f(17.0)), TextColor(ACCENT), Node { margin: UiRect::top(sz.px(8.0)), ..default() }));
        }
        // Body.
        let strip = matches!(view.layout, Some(Layout::Strip { .. }));
        p.spawn(Node {
            flex_grow: 1.0,
            flex_direction: if strip { FlexDirection::Column } else { FlexDirection::Row },
            justify_content: if strip { JustifyContent::FlexEnd } else { JustifyContent::SpaceBetween },
            align_items: if strip { AlignItems::Stretch } else { AlignItems::FlexStart },
            margin: UiRect::top(sz.px(22.0)),
            column_gap: sz.px(32.0),
            row_gap: sz.px(18.0),
            ..default()
        })
        .with_children(|b| {
            if strip {
                // Details (and the custom colour sliders) above the strip, on the right; the car shows on the left.
                b.spawn(Node { flex_direction: FlexDirection::Row, justify_content: JustifyContent::FlexEnd, align_items: AlignItems::FlexEnd, column_gap: sz.px(24.0), ..default() }).with_children(|r| {
                    if !view.sliders.is_empty() {
                        spawn_sliders(r, font, sz, &view.sliders);
                    }
                    r.spawn((DetailsSlot, Node { width: sz.px(440.0), flex_direction: FlexDirection::Column, ..default() })).with_children(|d| spawn_details(d, font, sz, view.details.as_ref()));
                });
                spawn_cards(b, font, sz, view, cursor, first);
            } else {
                spawn_cards(b, font, sz, view, cursor, first);
                if view.details.is_some() {
                    b.spawn((DetailsSlot, Node { width: sz.px(460.0), flex_direction: FlexDirection::Column, ..default() })).with_children(|d| spawn_details(d, font, sz, view.details.as_ref()));
                }
            }
        });
        // Legend.
        p.spawn((LegendSlot, Node { flex_direction: FlexDirection::Row, column_gap: sz.px(26.0), margin: UiRect::top(sz.px(18.0)), ..default() }))
            .with_children(|l| spawn_legend(l, font, sz, &view.hints));
    });
    if let Some(d) = dialog {
        commands.entity(root).with_children(|p| spawn_dialog(p, font, sz, d));
    }
}

fn spawn_cards(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, view: &View, cursor: usize, first: usize) {
    let Some(layout) = view.layout else { return };
    if view.cards.is_empty() {
        p.spawn((Text::new(view.empty.clone()), font.text(sz.f(24.0)), TextColor(DIM)));
        return;
    }
    let n = view.cards.len();
    let (w, h, shown, wrap) = match layout {
        Layout::Hub { w, h } => (w, h, n, false),
        Layout::Grid { cols, rows, w, h } => (w, h, cols * rows, true),
        Layout::Strip { visible, w, h } => (w, h, visible, false),
    };
    let last = (first + shown).min(n);
    let cols = layout.cols(n);
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: sz.px(8.0),
        flex_grow: if matches!(layout, Layout::Grid { .. }) { 1.0 } else { 0.0 },
        align_items: if matches!(layout, Layout::Hub { .. }) { AlignItems::Center } else { AlignItems::FlexStart },
        ..default()
    })
    .with_children(|col| {
        // "More above / before" markers.
        if first > 0 {
            text(col, font, sz.f(15.0), if wrap { format!("▲  {} more", first) } else { format!("◀  {} more", first) }, FAINT);
        }
        col.spawn(Node {
            flex_direction: FlexDirection::Row,
            flex_wrap: if wrap { FlexWrap::Wrap } else { FlexWrap::NoWrap },
            column_gap: sz.px(16.0),
            row_gap: sz.px(16.0),
            width: if wrap { sz.px((w + 16.0) * cols as f32) } else { Val::Auto },
            justify_content: if matches!(layout, Layout::Hub { .. }) { JustifyContent::Center } else { JustifyContent::FlexStart },
            padding: UiRect::all(sz.px(8.0)),
            ..default()
        })
        .with_children(|row| {
            for i in first..last {
                spawn_card(row, font, sz, &view.cards[i], i, i == cursor, w, h);
            }
        });
        if last < n {
            text(col, font, sz.f(15.0), if wrap { format!("▼  {} more", n - last) } else { format!("{} more  ▶", n - last) }, FAINT);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_card(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, c: &Card, index: usize, on: bool, w: f32, h: f32) {
    let art_h = h * 0.62;
    let scale = if on { 1.07 } else { 1.0 };
    p.spawn((
        CardNode(index),
        CardFx { index, scale },
        Button,
        Node {
            width: sz.px(w),
            height: sz.px(h),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(sz.px(3.0)),
            border_radius: BorderRadius::all(sz.px(8.0)),
            overflow: Overflow::clip(),
            ..default()
        },
        UiTransform { scale: Vec2::splat(scale), ..UiTransform::IDENTITY },
        BackgroundColor(if on { CARD_BG_ON } else { CARD_BG }),
        BorderColor::all(if on { ACCENT } else { Color::NONE }),
        BoxShadow(if on { glow() } else { Vec::new() }),
    ))
    .with_children(|card| {
        // Art: photo / swatch / glyph, with the badge and tag over it.
        card.spawn((Node { width: Val::Percent(100.0), height: sz.px(art_h), justify_content: JustifyContent::Center, align_items: AlignItems::Center, ..default() }, BackgroundColor(c.swatch.unwrap_or(ART_BG))))
            .with_children(|art| {
                if let Some(img) = &c.image {
                    art.spawn((ImageNode { image: img.clone(), image_mode: NodeImageMode::Stretch, ..default() }, Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() }));
                } else if let Some(g) = &c.glyph {
                    text(art, font, sz.f((art_h * 0.32).clamp(18.0, 64.0)), g.clone(), if c.locked { FAINT } else { Color::srgba(1.0, 1.0, 1.0, 0.85) });
                }
                if let Some(b) = &c.badge {
                    art.spawn(Node { position_type: PositionType::Absolute, left: sz.px(8.0), top: sz.px(8.0), ..default() }).with_children(|x| chip(x, font, sz, b, 15.0));
                }
                if let Some(t) = &c.tag {
                    art.spawn(Node { position_type: PositionType::Absolute, right: sz.px(8.0), top: sz.px(8.0), ..default() }).with_children(|x| chip(x, font, sz, t, 13.0));
                }
            });
        // Text block.
        card.spawn(Node { flex_direction: FlexDirection::Column, padding: UiRect::axes(sz.px(12.0), sz.px(8.0)), row_gap: sz.px(2.0), flex_grow: 1.0, ..default() }).with_children(|t| {
            text(t, font, sz.f(19.0), c.title.clone(), if c.locked { FAINT } else { Color::WHITE });
            if !c.sub.is_empty() {
                text(t, font, sz.f(14.0), c.sub.clone(), DIM);
            }
            if let Some(f) = &c.foot {
                t.spawn(Node { flex_grow: 1.0, ..default() });
                text(t, font, sz.f(17.0), f.clone(), c.foot_color.unwrap_or(Color::WHITE));
            }
        });
    });
}

fn spawn_details(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, d: Option<&Details>) {
    let Some(d) = d else { return };
    p.spawn((
        Node { flex_direction: FlexDirection::Column, padding: UiRect::all(sz.px(18.0)), row_gap: sz.px(6.0), border: UiRect::left(sz.px(5.0)), border_radius: BorderRadius::all(sz.px(6.0)), ..default() },
        BackgroundColor(PANEL),
        BorderColor::all(ACCENT),
    ))
    .with_children(|c| {
        if let Some(img) = &d.image {
            c.spawn((ImageNode { image: img.clone(), image_mode: NodeImageMode::Stretch, ..default() }, Node { width: Val::Percent(100.0), height: sz.px(150.0), margin: UiRect::bottom(sz.px(6.0)), ..default() }));
        }
        c.spawn(Node { flex_direction: FlexDirection::Row, column_gap: sz.px(10.0), align_items: AlignItems::Center, ..default() }).with_children(|r| {
            if let Some(b) = &d.badge {
                chip(r, font, sz, b, 17.0);
            }
            text(r, font, sz.f(26.0), d.title.clone(), Color::WHITE);
        });
        if !d.sub.is_empty() {
            text(c, font, sz.f(16.0), d.sub.clone(), DIM);
        }
        for bar in &d.bars {
            spawn_bar(c, font, sz, bar);
        }
        for (k, v) in &d.lines {
            c.spawn(Node { flex_direction: FlexDirection::Row, justify_content: JustifyContent::SpaceBetween, ..default() }).with_children(|r| {
                text(r, font, sz.f(16.0), k.clone(), DIM);
                text(r, font, sz.f(16.0), v.clone(), Color::WHITE);
            });
        }
        if let Some((price, color)) = &d.price {
            c.spawn((Text::new(price.clone()), font.text(sz.f(30.0)), TextColor(*color), Node { margin: UiRect::top(sz.px(6.0)), ..default() }));
        }
        if let Some(a) = &d.action {
            c.spawn(Node { flex_direction: FlexDirection::Row, column_gap: sz.px(8.0), align_items: AlignItems::Center, margin: UiRect::top(sz.px(6.0)), ..default() }).with_children(|r| {
                glyph(r, font, sz, "A", HintAct::Confirm);
                text(r, font, sz.f(20.0), a.to_uppercase(), Color::WHITE);
            });
        }
    });
}

fn spawn_bar(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, bar: &Bar) {
    p.spawn(Node { flex_direction: FlexDirection::Column, row_gap: sz.px(3.0), margin: UiRect::top(sz.px(4.0)), ..default() }).with_children(|c| {
        c.spawn(Node { flex_direction: FlexDirection::Row, justify_content: JustifyContent::SpaceBetween, ..default() }).with_children(|r| {
            text(r, font, sz.f(15.0), bar.label.to_uppercase(), DIM);
            let colour = match bar.after {
                Some(a) if a > bar.now + 1e-3 => GOOD,
                Some(a) if a < bar.now - 1e-3 => BAD,
                _ => Color::WHITE,
            };
            text(r, font, sz.f(15.0), bar.text.clone(), colour);
        });
        c.spawn((Node { width: Val::Percent(100.0), height: sz.px(8.0), border_radius: BorderRadius::all(sz.px(3.0)), ..default() }, BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.1))))
            .with_children(|track| {
                let now = bar.now.clamp(0.0, 1.0);
                let after = bar.after.map(|a| a.clamp(0.0, 1.0));
                // Gain: the extra in green after the white bar; loss: the lost part in red at its end.
                let base = after.map_or(now, |a| a.min(now));
                track.spawn((Node { position_type: PositionType::Absolute, height: Val::Percent(100.0), width: Val::Percent(base * 100.0), border_radius: BorderRadius::all(sz.px(3.0)), ..default() }, BackgroundColor(Color::WHITE)));
                if let Some(a) = after {
                    if (a - now).abs() > 1e-3 {
                        let (from, to, colour) = if a > now { (now, a, GOOD) } else { (a, now, BAD) };
                        track.spawn((Node { position_type: PositionType::Absolute, left: Val::Percent(from * 100.0), height: Val::Percent(100.0), width: Val::Percent((to - from) * 100.0), ..default() }, BackgroundColor(colour)));
                    }
                }
            });
    });
}

fn spawn_sliders(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, sliders: &[(String, String, f32, bool)]) {
    p.spawn((
        Node { flex_direction: FlexDirection::Column, width: sz.px(380.0), padding: UiRect::all(sz.px(16.0)), row_gap: sz.px(8.0), border_radius: BorderRadius::all(sz.px(6.0)), ..default() },
        BackgroundColor(PANEL),
    ))
    .with_children(|c| {
        for (label, value, v, on) in sliders {
            c.spawn((Node { flex_direction: FlexDirection::Column, padding: UiRect::all(sz.px(6.0)), border: UiRect::left(sz.px(4.0)), row_gap: sz.px(4.0), ..default() }, BorderColor::all(if *on { ACCENT } else { Color::NONE })))
                .with_children(|s| {
                    s.spawn(Node { flex_direction: FlexDirection::Row, justify_content: JustifyContent::SpaceBetween, ..default() }).with_children(|r| {
                        text(r, font, sz.f(16.0), label.to_uppercase(), if *on { Color::WHITE } else { DIM });
                        text(r, font, sz.f(16.0), format!("‹  {value}  ›"), Color::WHITE);
                    });
                    s.spawn((Node { width: Val::Percent(100.0), height: sz.px(6.0), ..default() }, BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.1)))).with_children(|t| {
                        t.spawn((Node { width: Val::Percent(v.clamp(0.0, 1.0) * 100.0), height: Val::Percent(100.0), ..default() }, BackgroundColor(if *on { ACCENT } else { Color::WHITE })));
                    });
                });
        }
    });
}

/// A face-button glyph chip (browser.rs colours), clickable.
fn glyph(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, pad: &str, act: HintAct) {
    let colour = match pad {
        "A" => Color::srgb(0.30, 0.66, 0.18),
        "B" => Color::srgb(0.82, 0.18, 0.16),
        "X" => Color::srgb(0.16, 0.42, 0.86),
        "Y" => Color::srgb(0.90, 0.68, 0.06),
        _ => Color::srgb(0.30, 0.32, 0.36),
    };
    p.spawn((
        HintChip(act),
        Button,
        Node { min_width: sz.px(26.0), height: sz.px(26.0), padding: UiRect::horizontal(sz.px(6.0)), justify_content: JustifyContent::Center, align_items: AlignItems::Center, border_radius: BorderRadius::all(sz.px(13.0)), ..default() },
        BackgroundColor(colour),
    ))
    .with_children(|c| text(c, font, sz.f(14.0), pad.to_owned(), Color::WHITE));
}

fn spawn_legend(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, hints: &[Hint]) {
    for h in hints {
        p.spawn(Node { flex_direction: FlexDirection::Row, column_gap: sz.px(8.0), align_items: AlignItems::Center, ..default() }).with_children(|r| {
            glyph(r, font, sz, h.pad, h.act);
            text(r, font, sz.f(16.0), format!("{}  ({})", h.label.to_uppercase(), h.key), DIM);
        });
    }
}

fn spawn_dialog(p: &mut ChildSpawnerCommands, font: &UiFont, sz: Sz, d: &Dialog) {
    p.spawn((
        Node { position_type: PositionType::Absolute, left: Val::Px(0.0), top: Val::Px(0.0), width: Val::Percent(100.0), height: Val::Percent(100.0), justify_content: JustifyContent::Center, align_items: AlignItems::Center, ..default() },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
        GlobalZIndex(102),
    ))
    .with_children(|o| {
        o.spawn((
            Node { flex_direction: FlexDirection::Column, width: sz.px(720.0), padding: UiRect::all(sz.px(28.0)), row_gap: sz.px(14.0), border: UiRect::left(sz.px(6.0)), border_radius: BorderRadius::all(sz.px(8.0)), ..default() },
            BackgroundColor(Color::srgba(0.05, 0.06, 0.08, 0.97)),
            BorderColor::all(ACCENT),
        ))
        .with_children(|c| {
            text(c, font, sz.f(34.0), d.title.to_uppercase(), Color::WHITE);
            text(c, font, sz.f(20.0), d.message.clone(), DIM);
            c.spawn(Node { flex_direction: FlexDirection::Row, column_gap: sz.px(12.0), margin: UiRect::top(sz.px(10.0)), flex_wrap: FlexWrap::Wrap, row_gap: sz.px(10.0), ..default() }).with_children(|r| {
                for (i, (label, _)) in d.options.iter().enumerate() {
                    let on = i == d.cursor;
                    r.spawn((
                        DialogChip(i),
                        Button,
                        Node { padding: UiRect::axes(sz.px(22.0), sz.px(10.0)), border: UiRect::all(sz.px(2.0)), border_radius: BorderRadius::all(sz.px(6.0)), ..default() },
                        BackgroundColor(if on { ACCENT } else { Color::srgba(1.0, 1.0, 1.0, 0.06) }),
                        BorderColor::all(if on { ACCENT } else { Color::srgba(1.0, 1.0, 1.0, 0.2) }),
                    ))
                    .with_children(|b| text(b, font, sz.f(20.0), label.to_uppercase(), Color::WHITE));
                }
            });
            c.spawn(Node { flex_direction: FlexDirection::Row, column_gap: sz.px(22.0), margin: UiRect::top(sz.px(6.0)), ..default() }).with_children(|l| {
                spawn_legend(l, font, sz, &[hint("A", "Enter", "Select", HintAct::Confirm), hint("B", "Esc", "Cancel", HintAct::Back)]);
            });
        });
    });
}

/// Initials of a maker for its card when no logo art exists ("Aston Martin" -> "AM", "Ferrari" -> "FER").
pub(super) fn initials(maker: &str) -> String {
    let words: Vec<&str> = maker.split_whitespace().collect();
    if words.len() >= 2 {
        words.iter().take(3).filter_map(|w| w.chars().next()).collect::<String>().to_uppercase()
    } else {
        maker.chars().take(3).collect::<String>().to_uppercase()
    }
}

/// Counts per key, keeping first-seen order.
pub(super) fn count_by<K: std::hash::Hash + Eq + Clone>(keys: impl Iterator<Item = K>) -> Vec<(K, usize)> {
    let mut order = Vec::new();
    let mut n: HashMap<K, usize> = HashMap::new();
    for k in keys {
        if !n.contains_key(&k) {
            order.push(k.clone());
        }
        *n.entry(k).or_default() += 1;
    }
    order.into_iter().map(|k| {
        let c = n[&k];
        (k, c)
    }).collect()
}
