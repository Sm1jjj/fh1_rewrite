//! The starter car choice of the first-time career (ui/intro/story.rs, docs/PROGRESSION.md "First-time career"): three cars
//! "on the house", all in the VW Corrado's class and PI band, shown as a row of cards in the look of the Garage card menus
//! (ui/cards.rs: HUD font, `ACCENT` border + glow on the focused card, the game's own car thumbnails, class chips from
//! [`class_color`], the button legend, the BUY CAR? style dialog).
//!
//! The offer never contains the Corrado itself (every new profile already owns it; it is only the class / PI reference),
//! and the chosen car replaces it in the garage (story.rs `drop_default_starter`).
//!
//! A self-contained screen: the card framework (ui/cards.rs) is tied to the pause menu's Garage pages and its drawing
//! helpers are private, so this file keeps its own small copy of the look and registers its own systems
//! ([`input`], [`draw`], ordered between the pause menu's mouse input and `sync_pause`).
//!
//! - Left / Right (D-pad, left stick, arrows, A / D) move; A / Enter / Space / a click choose; then "Take the <car>?"
//!   with Yes / No (Left / Right / Up / Down, A / Enter; B / Backspace / Esc = back to the row, a click on No).
//! - There is no way to dismiss the choice itself: B / Esc on the row do nothing, and the pause menu is shut again in the
//!   frame it opens (Esc / Start would otherwise open it over the cards, [`input`]).
//! - Driving is blocked by story.rs (`Drive::Hold`: full brake). It cannot stop the other `ui::driving` systems (G =
//!   next start location, N / P = next owned car); [`choice_open`] is for ui.rs `driving` (see docs/PROGRESSION.md).
//! - The screen only reports the choice ([`Starter::take_choice`]); story.rs does the ownership, car swap and flags.
//!
//! Which cars: [`super::story::pick_starters`] (pure; INFERRED rule, documented there).

use std::sync::atomic::{AtomicBool, Ordering};

use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use super::super::browser::{catalog_for, AxisRepeat};
use super::super::cards::class_color;
use super::super::{Menu, UiFont, ACCENT};
use super::story::{pick_starters, STARTER_ANCHOR};
use crate::progression::data::CareerData;
use crate::Garage;

/// The choice is on screen (read by [`choice_open`]).
static OPEN: AtomicBool = AtomicBool::new(false);

/// The starter choice is up: driving-side input (G, N / P) should be ignored (ui.rs `driving`).
pub fn choice_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

/// A press this soon after the screen opened is ignored (the A / Enter that skipped FMV_01 must not choose).
const GUARD_S: f32 = 0.6;

const CARD_BG: Color = Color::srgba(0.07, 0.08, 0.10, 0.92);
const CARD_BG_ON: Color = Color::srgba(0.11, 0.12, 0.15, 0.96);
const ART_BG: Color = Color::srgb(0.13, 0.14, 0.17);
const DIM: Color = Color::srgba(1.0, 1.0, 1.0, 0.6);
const BACKDROP: Color = Color::srgba(0.02, 0.02, 0.03, 0.9);

/// One offered car: what the card shows.
#[derive(Clone, Debug)]
pub(super) struct Pick {
    /// `Garage::cars` index.
    pub index: usize,
    pub maker: String,
    pub name: String,
    pub year: Option<i64>,
    pub class: Option<String>,
    pub pi: Option<i64>,
    pub drive: String,
    /// gamedb Data_Car.Id: `ui/textures/thumbnails/thumbnail_<id>.png`.
    pub thumb: Option<i64>,
    /// Power, weight, 0-60, top speed (what the car's physics.json carries).
    pub stats: Vec<(String, String)>,
}

impl Pick {
    fn full_name(&self) -> String {
        format!("{} {}", self.maker, self.name).trim().to_owned()
    }
}

/// The offer: [`pick_starters`] on the career catalog, looked up in the car browser's catalog for the card text.
pub(super) fn build(garage: &Garage, career: &CareerData) -> Vec<Pick> {
    let listed = |n: &str| garage.cars.iter().any(|c| c.eq_ignore_ascii_case(n));
    let names = pick_starters(&career.cars, STARTER_ANCHOR, &listed);
    if names.is_empty() {
        return Vec::new();
    }
    let cat = catalog_for(&garage.assets, &garage.cars);
    names
        .iter()
        .filter_map(|n| {
            let index = garage.cars.iter().position(|c| c.eq_ignore_ascii_case(n))?;
            let info = career.cars.get(n);
            let (at, e) = cat.entries.iter().enumerate().find(|(_, e)| e.index == index)?;
            let mut stats = Vec::new();
            if let Some(s) = cat.stats(at) {
                if let Some(p) = s.power_w {
                    stats.push(("Power".to_owned(), format!("{:.0} hp", p / 745.7)));
                }
                if let Some(m) = s.mass_kg {
                    stats.push(("Weight".to_owned(), format!("{} kg", crate::progression::fmt_num(m.round() as i64))));
                }
                if let Some(t) = s.zero_60 {
                    stats.push(("0-60 mph".to_owned(), format!("{t:.1} s")));
                }
                if let Some(v) = s.top_mph {
                    stats.push(("Top speed".to_owned(), format!("{v:.0} mph")));
                }
            }
            Some(Pick {
                index,
                maker: if e.maker.is_empty() { info.map(|i| i.make.clone()).unwrap_or_default() } else { e.maker.clone() },
                name: if e.name.is_empty() { info.map(|i| i.name.clone()).unwrap_or_default() } else { e.name.clone() },
                year: e.year,
                class: e.class.clone().or_else(|| info.map(|i| career.class_name(i.class).to_owned())),
                pi: e.pi.or_else(|| info.map(|i| i64::from(i.pi))),
                drive: e.drive.clone(),
                thumb: e.id,
                stats,
            })
        })
        .collect()
}

/// The starter choice screen's state (story.rs opens it, polls the choice and closes it).
#[derive(Resource, Default)]
pub(super) struct Starter {
    picks: Vec<Pick>,
    cursor: usize,
    /// The confirm dialog: 0 = Yes, 1 = No.
    dialog: Option<usize>,
    /// Garage index of the confirmed car, until story.rs takes it.
    chosen: Option<usize>,
    open: bool,
    age: f32,
    dirty: bool,
    repeat: (AxisRepeat, AxisRepeat),
}

impl Starter {
    /// Show `picks` (the focus starts on the first card, the nearest PI).
    pub fn open(&mut self, picks: Vec<Pick>) {
        *self = Self { picks, open: true, dirty: true, ..default() };
        OPEN.store(true, Ordering::Relaxed);
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The confirmed car (`Garage::cars` index), once.
    pub fn take_choice(&mut self) -> Option<usize> {
        self.chosen.take()
    }

    pub fn close(&mut self) {
        self.open = false;
        self.chosen = None;
        self.dialog = None;
        OPEN.store(false, Ordering::Relaxed);
    }
}

#[derive(Component)]
pub(super) struct StarterRoot;

/// Card entity (mouse): index into the picks.
#[derive(Component)]
pub(super) struct PickCard(usize);

/// Dialog option (mouse): 0 = Yes, 1 = No.
#[derive(Component)]
pub(super) struct DialogBtn(usize);

// ---------------------------------------------------------------- input

#[allow(clippy::too_many_arguments)]
pub(super) fn input(
    mut st: ResMut<Starter>,
    menu: Option<ResMut<Menu>>,
    (keys, pads, motion, time): (Res<ButtonInput<KeyCode>>, Query<&Gamepad>, Res<AccumulatedMouseMotion>, Res<Time<Real>>),
    (cards, buttons): (Query<(&Interaction, &PickCard), Changed<Interaction>>, Query<(&Interaction, &DialogBtn), Changed<Interaction>>),
) {
    if !st.open {
        return;
    }
    // Esc / Start opened the pause menu this frame (menu_input ran before): shut it again, the choice can't be left.
    if let Some(mut m) = menu {
        if m.open {
            m.open = false;
            m.dirty = true;
        }
    }
    let dt = time.delta_secs();
    st.age += dt;
    let kp = |k: KeyCode| keys.just_pressed(k);
    let kh = |k: KeyCode| keys.pressed(k);
    let pp = |b: GamepadButton| pads.iter().any(|p| p.just_pressed(b));
    let ph = |b: GamepadButton| pads.iter().any(|p| p.pressed(b));
    let stick = |a: GamepadAxis| pads.iter().map(|p| p.get(a).unwrap_or(0.0)).fold(0.0f32, |m, v| if v.abs() > m.abs() { v } else { m });
    let dir = |neg: bool, pos: bool| if neg && !pos { -1 } else if pos && !neg { 1 } else { 0 };
    let (sx, sy) = (stick(GamepadAxis::LeftStickX), stick(GamepadAxis::LeftStickY));
    let h = dir(
        kh(KeyCode::ArrowLeft) || kh(KeyCode::KeyA) || ph(GamepadButton::DPadLeft) || sx < -0.55,
        kh(KeyCode::ArrowRight) || kh(KeyCode::KeyD) || ph(GamepadButton::DPadRight) || sx > 0.55,
    );
    // Screen down = +1 (stick up is +Y).
    let v = dir(
        kh(KeyCode::ArrowUp) || kh(KeyCode::KeyW) || ph(GamepadButton::DPadUp) || sy > 0.55,
        kh(KeyCode::ArrowDown) || kh(KeyCode::KeyS) || ph(GamepadButton::DPadDown) || sy < -0.55,
    );
    let (dx, _) = st.repeat.0.update(h, dt);
    let (dy, _) = st.repeat.1.update(v, dt);
    let confirm = pp(GamepadButton::South) || kp(KeyCode::Enter) || kp(KeyCode::NumpadEnter) || kp(KeyCode::Space);
    let back = pp(GamepadButton::East) || kp(KeyCode::Backspace) || kp(KeyCode::Escape);
    if st.chosen.is_some() || st.picks.is_empty() {
        return;
    }
    let ready = st.age >= GUARD_S;
    // The mouse only moves the focus while it moves (a card appearing under a resting pointer must not steal it).
    let moved = motion.delta != Vec2::ZERO;
    let n = st.picks.len();
    match st.dialog {
        Some(c) => {
            let mut c = c;
            if dx != 0 || dy != 0 {
                c = 1 - c;
                st.dialog = Some(c);
                st.dirty = true;
            }
            if moved {
                for (i, b) in &buttons {
                    if matches!(i, Interaction::Hovered | Interaction::Pressed) && b.0 != c {
                        c = b.0;
                        st.dialog = Some(c);
                        st.dirty = true;
                    }
                }
            }
            let clicked = buttons.iter().find(|(i, _)| matches!(i, Interaction::Pressed)).map(|(_, b)| b.0);
            let act = if ready && (confirm || clicked.is_some()) { Some(clicked.unwrap_or(c)) } else { None };
            if ready && back {
                st.dialog = None;
                st.dirty = true;
            } else if let Some(choice) = act {
                if choice == 0 {
                    st.chosen = Some(st.picks[st.cursor.min(n - 1)].index);
                } else {
                    st.dialog = None;
                }
                st.dirty = true;
            }
        }
        None => {
            let mut cur = st.cursor.min(n - 1);
            if dx != 0 {
                cur = (cur as i32 + dx).clamp(0, n as i32 - 1) as usize;
            }
            if moved {
                for (i, c) in &cards {
                    if matches!(i, Interaction::Hovered | Interaction::Pressed) {
                        cur = c.0.min(n - 1);
                    }
                }
            }
            if cur != st.cursor {
                st.cursor = cur;
                st.dirty = true;
            }
            let clicked = ready && cards.iter().any(|(i, c)| matches!(i, Interaction::Pressed) && c.0 == cur);
            if ready && (confirm || clicked) {
                st.dialog = Some(0);
                st.dirty = true;
            }
        }
    }
}

// ---------------------------------------------------------------- drawing

fn txt(p: &mut ChildSpawnerCommands, font: &UiFont, px: f32, s: impl Into<String>, color: Color) {
    p.spawn((Text::new(s), font.text(px), TextColor(color)));
}

fn chip(p: &mut ChildSpawnerCommands, font: &UiFont, sz: f32, text: String, color: Color, px: f32) {
    p.spawn((Node { padding: UiRect::axes(Val::Px(8.0 * sz), Val::Px(2.0 * sz)), border_radius: BorderRadius::all(Val::Px(4.0 * sz)), ..default() }, BackgroundColor(color)))
        .with_children(|c| txt(c, font, px * sz, text, Color::WHITE));
}

fn glow() -> Vec<ShadowStyle> {
    vec![ShadowStyle { color: ACCENT.with_alpha(0.55), x_offset: Val::Px(0.0), y_offset: Val::Px(0.0), spread_radius: Val::Px(2.0), blur_radius: Val::Px(18.0) }]
}

/// A face-button glyph chip (the card menus' colours).
fn glyph(p: &mut ChildSpawnerCommands, font: &UiFont, sz: f32, pad: &str) {
    let colour = match pad {
        "A" => Color::srgb(0.30, 0.66, 0.18),
        "B" => Color::srgb(0.82, 0.18, 0.16),
        _ => Color::srgb(0.30, 0.32, 0.36),
    };
    p.spawn((
        Node { min_width: Val::Px(26.0 * sz), height: Val::Px(26.0 * sz), padding: UiRect::horizontal(Val::Px(6.0 * sz)), justify_content: JustifyContent::Center, align_items: AlignItems::Center, border_radius: BorderRadius::all(Val::Px(13.0 * sz)), ..default() },
        BackgroundColor(colour),
    ))
    .with_children(|c| txt(c, font, 14.0 * sz, pad.to_owned(), Color::WHITE));
}

fn legend(p: &mut ChildSpawnerCommands, font: &UiFont, sz: f32, hints: &[(&str, &str)]) {
    for (pad, label) in hints {
        p.spawn(Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(8.0 * sz), align_items: AlignItems::Center, ..default() }).with_children(|r| {
            glyph(r, font, sz, pad);
            txt(r, font, 16.0 * sz, label.to_uppercase(), DIM);
        });
    }
}

fn spawn_card(p: &mut ChildSpawnerCommands, font: &UiFont, asset_server: &AssetServer, sz: f32, c: &Pick, index: usize, on: bool) {
    let (w, h) = (400.0, 560.0);
    let art_h = h * 0.46;
    let scale = if on { 1.07 } else { 1.0 };
    p.spawn((
        PickCard(index),
        Button,
        Node {
            width: Val::Px(w * sz),
            height: Val::Px(h * sz),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(Val::Px(3.0 * sz)),
            border_radius: BorderRadius::all(Val::Px(8.0 * sz)),
            overflow: Overflow::clip(),
            ..default()
        },
        UiTransform { scale: Vec2::splat(scale), ..UiTransform::IDENTITY },
        BackgroundColor(if on { CARD_BG_ON } else { CARD_BG }),
        BorderColor::all(if on { ACCENT } else { Color::NONE }),
        BoxShadow(if on { glow() } else { Vec::new() }),
    ))
    .with_children(|card| {
        card.spawn((Node { width: Val::Percent(100.0), height: Val::Px(art_h * sz), justify_content: JustifyContent::Center, align_items: AlignItems::Center, ..default() }, BackgroundColor(ART_BG)))
            .with_children(|art| {
                match c.thumb {
                    Some(id) => {
                        let image: Handle<Image> = asset_server.load(format!("ui/textures/thumbnails/thumbnail_{id}.png"));
                        art.spawn((ImageNode { image, image_mode: NodeImageMode::Stretch, ..default() }, Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() }));
                    }
                    None => txt(art, font, 56.0 * sz, c.maker.chars().take(3).collect::<String>().to_uppercase(), Color::srgba(1.0, 1.0, 1.0, 0.85)),
                }
                let badge = match (&c.class, c.pi) {
                    (Some(k), Some(pi)) => Some((format!("{k} {pi}"), class_color(k))),
                    (Some(k), None) => Some((k.clone(), class_color(k))),
                    (None, Some(pi)) => Some((pi.to_string(), class_color(""))),
                    _ => None,
                };
                if let Some((text, color)) = badge {
                    art.spawn(Node { position_type: PositionType::Absolute, left: Val::Px(8.0 * sz), top: Val::Px(8.0 * sz), ..default() }).with_children(|x| chip(x, font, sz, text, color, 15.0));
                }
                if !c.drive.is_empty() {
                    art.spawn(Node { position_type: PositionType::Absolute, right: Val::Px(8.0 * sz), top: Val::Px(8.0 * sz), ..default() })
                        .with_children(|x| chip(x, font, sz, c.drive.clone(), Color::srgba(0.0, 0.0, 0.0, 0.6), 13.0));
                }
            });
        card.spawn(Node { flex_direction: FlexDirection::Column, padding: UiRect::axes(Val::Px(14.0 * sz), Val::Px(10.0 * sz)), row_gap: Val::Px(3.0 * sz), flex_grow: 1.0, ..default() }).with_children(|t| {
            txt(t, font, 16.0 * sz, c.maker.to_uppercase(), DIM);
            txt(t, font, 28.0 * sz, c.name.clone(), Color::WHITE);
            if let Some(y) = c.year {
                txt(t, font, 15.0 * sz, y.to_string(), DIM);
            }
            t.spawn(Node { flex_grow: 1.0, ..default() });
            for (k, v) in &c.stats {
                t.spawn(Node { flex_direction: FlexDirection::Row, justify_content: JustifyContent::SpaceBetween, ..default() }).with_children(|r| {
                    txt(r, font, 17.0 * sz, k.clone(), DIM);
                    txt(r, font, 17.0 * sz, v.clone(), Color::WHITE);
                });
            }
        });
    });
}

fn spawn_dialog(p: &mut ChildSpawnerCommands, font: &UiFont, sz: f32, name: &str, cursor: usize) {
    p.spawn((
        Node { position_type: PositionType::Absolute, left: Val::Px(0.0), top: Val::Px(0.0), width: Val::Percent(100.0), height: Val::Percent(100.0), justify_content: JustifyContent::Center, align_items: AlignItems::Center, ..default() },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
        GlobalZIndex(102),
    ))
    .with_children(|o| {
        o.spawn((
            Node { flex_direction: FlexDirection::Column, width: Val::Px(720.0 * sz), padding: UiRect::all(Val::Px(28.0 * sz)), row_gap: Val::Px(14.0 * sz), border: UiRect::left(Val::Px(6.0 * sz)), border_radius: BorderRadius::all(Val::Px(8.0 * sz)), ..default() },
            BackgroundColor(Color::srgba(0.05, 0.06, 0.08, 0.97)),
            BorderColor::all(ACCENT),
        ))
        .with_children(|c| {
            txt(c, font, 34.0 * sz, format!("Take the {name}?").to_uppercase(), Color::WHITE);
            txt(c, font, 20.0 * sz, "It is a gift: yours to keep, free. The Autoshow has the rest.", DIM);
            c.spawn(Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(12.0 * sz), margin: UiRect::top(Val::Px(10.0 * sz)), ..default() }).with_children(|r| {
                for (i, label) in ["Yes", "No"].iter().enumerate() {
                    let on = i == cursor;
                    r.spawn((
                        DialogBtn(i),
                        Button,
                        Node { padding: UiRect::axes(Val::Px(22.0 * sz), Val::Px(10.0 * sz)), border: UiRect::all(Val::Px(2.0 * sz)), border_radius: BorderRadius::all(Val::Px(6.0 * sz)), ..default() },
                        BackgroundColor(if on { ACCENT } else { Color::srgba(1.0, 1.0, 1.0, 0.06) }),
                        BorderColor::all(if on { ACCENT } else { Color::srgba(1.0, 1.0, 1.0, 0.2) }),
                    ))
                    .with_children(|b| txt(b, font, 20.0 * sz, label.to_uppercase(), Color::WHITE));
                }
            });
            c.spawn(Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(22.0 * sz), margin: UiRect::top(Val::Px(6.0 * sz)), ..default() })
                .with_children(|l| legend(l, font, sz, &[("A", "Select  (Enter)"), ("B", "Back  (Esc)")]));
        });
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    mut commands: Commands,
    mut st: ResMut<Starter>,
    font: Res<UiFont>,
    asset_server: Res<AssetServer>,
    roots: Query<Entity, With<StarterRoot>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut last_height: Local<f32>,
) {
    if !st.open {
        for r in &roots {
            commands.entity(r).despawn();
        }
        return;
    }
    let height = windows.single().map_or(1080.0, |w| w.height());
    if (height - *last_height).abs() > 1.0 {
        *last_height = height;
        st.dirty = true;
    }
    if !st.dirty && !roots.is_empty() {
        return;
    }
    st.dirty = false;
    for r in &roots {
        commands.entity(r).despawn();
    }
    let sz = (height / 1080.0).clamp(0.6, 1.6);
    let font = &*font;
    let cursor = st.cursor.min(st.picks.len().saturating_sub(1));
    let root = commands
        .spawn((
            StarterRoot,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::axes(Val::Px(64.0 * sz), Val::Px(40.0 * sz)),
                ..default()
            },
            BackgroundColor(BACKDROP),
            GlobalZIndex(101),
        ))
        .id();
    let picks = st.picks.clone();
    let dialog = st.dialog;
    commands.entity(root).with_children(|p| {
        p.spawn(Node { flex_direction: FlexDirection::Column, ..default() }).with_children(|t| {
            txt(t, font, 18.0 * sz, "ON THE HOUSE", DIM);
            txt(t, font, 46.0 * sz, "CHOOSE YOUR CAR", Color::WHITE);
            txt(t, font, 20.0 * sz, "Three cars of the same class and performance. The one you take is yours.", DIM);
        });
        p.spawn(Node { flex_grow: 1.0, flex_direction: FlexDirection::Row, justify_content: JustifyContent::Center, align_items: AlignItems::Center, column_gap: Val::Px(36.0 * sz), ..default() })
            .with_children(|row| {
                for (i, c) in picks.iter().enumerate() {
                    spawn_card(row, font, &asset_server, sz, c, i, i == cursor);
                }
            });
        p.spawn(Node { flex_direction: FlexDirection::Row, column_gap: Val::Px(26.0 * sz), ..default() })
            .with_children(|l| legend(l, font, sz, &[("A", "Choose  (Enter)"), ("<>", "Move  (arrows)")]));
    });
    if let (Some(d), Some(c)) = (dialog, picks.get(cursor)) {
        commands.entity(root).with_children(|p| spawn_dialog(p, font, sz, &c.full_name(), d));
    }
}
