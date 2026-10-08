//! Career screen (F6, or `OpenCareer` from the world map): where the player stands (wristband, XP to the next one,
//! popularity rank and the rival above, credits, the car's class), the recommended next event, and every event by
//! wristband / street hub / showcase with its state. Up / Down pick, Left / Right (LB / RB) page, Enter / A start (to the
//! grid), Backspace / B / F6 close. Plain Bevy UI text like the race HUD (menus keep their own look).

use bevy::input::gamepad::{Gamepad, GamepadButton};
use bevy::prelude::*;

use super::{fmt_num, EventCatalog, EventInfo, EventState, OpenCareer, Profile, StartEvent, TIER_NAMES};
use crate::race::{Events, RacePhase, RaceState};
use crate::Car;

#[derive(Resource, Default)]
pub struct CareerUi {
    pub open: bool,
    /// Page: 0..6 wristbands, 7 street races, 8 showcases.
    pub page: usize,
    pub cursor: usize,
    /// The page was set to the player's wristband on this opening.
    placed: bool,
}

const PAGES: usize = 9;

fn page_name(page: usize) -> String {
    match page {
        0..=6 => format!("{} wristband", TIER_NAMES[page]),
        7 => "Street races".into(),
        _ => "Showcases".into(),
    }
}

fn page_of(e: &EventInfo, events: &Events) -> usize {
    let r = &events.races[e.race];
    if r.popularity_req > 0 || e.kind == super::EventKind::Showcase {
        8
    } else if e.hub > 0 {
        7
    } else {
        e.tier as usize
    }
}

fn page_events<'a>(cat: &'a EventCatalog, events: &Events, page: usize) -> Vec<&'a EventInfo> {
    let mut v: Vec<&EventInfo> = cat.events.iter().filter(|e| page_of(e, events) == page).collect();
    v.sort_by_key(|e| {
        let r = &events.races[e.race];
        (r.hub, r.unlock_xp, r.popularity_req.wrapping_neg(), r.event_order, e.id.clone())
    });
    v
}

#[derive(Component)]
pub struct CareerText;

pub fn spawn_career_ui(mut commands: Commands, font: Option<Res<crate::ui::UiFont>>) {
    let f = |px: f32| font.as_ref().map_or_else(|| TextFont { font_size: bevy::text::FontSize::Px(px), ..default() }, |f| f.text(px));
    commands.spawn((
        CareerText,
        Text::new(""),
        f(22.0),
        TextColor(Color::WHITE),
        BackgroundColor(Color::BLACK.with_alpha(0.0)),
        Visibility::Hidden,
        Node { position_type: PositionType::Absolute, left: Val::Percent(18.0), top: Val::Percent(8.0), padding: UiRect::all(Val::Px(22.0)), ..default() },
    ));
}

#[allow(clippy::too_many_arguments)]
pub fn career_input(
    mut ui: ResMut<CareerUi>,
    mut open_req: MessageReader<OpenCareer>,
    mut start: MessageWriter<StartEvent>,
    cat: Res<EventCatalog>,
    events: Res<Events>,
    profile: Res<Profile>,
    rs: Option<Res<RaceState>>,
    keys: Res<ButtonInput<KeyCode>>,
    pads: Query<&Gamepad>,
) {
    if open_req.read().count() > 0 && rs.as_ref().is_none_or(|r| r.phase == RacePhase::Idle) {
        ui.open = true;
    }
    if !ui.open {
        ui.placed = false;
        return;
    }
    if rs.as_ref().is_some_and(|r| r.phase != RacePhase::Idle) {
        ui.open = false;
        return;
    }
    if !ui.placed {
        ui.placed = true;
        // Open on the recommended event's page, else the player's wristband.
        match cat.events.iter().find(|e| e.recommended) {
            Some(e) => {
                ui.page = page_of(e, &events);
                ui.cursor = page_events(&cat, &events, ui.page).iter().position(|x| x.id == e.id).unwrap_or(0);
            }
            None => {
                ui.page = events.career.tier(profile.data.xp).min(6);
                ui.cursor = 0;
            }
        }
    }
    let pad = |b: GamepadButton| pads.iter().any(|p| p.just_pressed(b));
    if keys.just_pressed(KeyCode::Backspace) || pad(GamepadButton::East) {
        ui.open = false;
        return;
    }
    let left = keys.just_pressed(KeyCode::ArrowLeft) || pad(GamepadButton::LeftTrigger) || pad(GamepadButton::DPadLeft);
    let right = keys.just_pressed(KeyCode::ArrowRight) || pad(GamepadButton::RightTrigger) || pad(GamepadButton::DPadRight);
    if left || right {
        ui.page = if right { (ui.page + 1) % PAGES } else { (ui.page + PAGES - 1) % PAGES };
        ui.cursor = 0;
    }
    let list = page_events(&cat, &events, ui.page);
    if list.is_empty() {
        ui.cursor = 0;
        return;
    }
    if keys.just_pressed(KeyCode::ArrowDown) || pad(GamepadButton::DPadDown) {
        ui.cursor = (ui.cursor + 1).min(list.len() - 1);
    }
    if keys.just_pressed(KeyCode::ArrowUp) || pad(GamepadButton::DPadUp) {
        ui.cursor = ui.cursor.saturating_sub(1);
    }
    ui.cursor = ui.cursor.min(list.len() - 1);
    if keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter) || pad(GamepadButton::South) {
        let e = list[ui.cursor];
        if e.state != EventState::Locked {
            start.write(StartEvent { id: e.id.clone() });
            ui.open = false;
        }
    }
}

pub fn draw_career(
    ui: Res<CareerUi>,
    cat: Res<EventCatalog>,
    events: Res<Events>,
    profile: Res<Profile>,
    cars: Query<&Car>,
    mut q: Query<(&mut Text, &mut Visibility, &mut BackgroundColor), With<CareerText>>,
) {
    let Ok((mut text, mut vis, mut bg)) = q.single_mut() else { return };
    let want = if ui.open { Visibility::Inherited } else { Visibility::Hidden };
    if *vis != want {
        *vis = want;
        bg.0 = Color::BLACK.with_alpha(if ui.open { 0.82 } else { 0.0 });
    }
    if !ui.open {
        return;
    }
    let c = &events.career;
    let p = &profile.data;
    let tier = c.tier(p.xp);
    let mut s = String::from("CAREER        Left / Right: page   Up / Down   Enter / A: go to the start   Backspace / B / F6: close\n\n");
    // Wristband and XP.
    let band = c.wristbands.get(tier).map_or("?", |w| w.name.as_str());
    match c.wristbands.get(tier + 1) {
        Some(next) => {
            let lo = c.wristbands[tier].xp;
            let t = (p.xp - lo) as f32 / (next.xp - lo).max(1) as f32;
            let n = (t.clamp(0.0, 1.0) * 20.0).round() as usize;
            s += &format!("{} WRISTBAND   {} XP   [{}{}]   {} XP to {}\n", band.to_uppercase(), fmt_num(p.xp as i64), "#".repeat(n), "-".repeat(20 - n), fmt_num((next.xp - p.xp) as i64), next.name);
        }
        None => s += &format!("{} WRISTBAND   {} XP   (top wristband)\n", band.to_uppercase(), fmt_num(p.xp as i64)),
    }
    // Popularity.
    let rank = c.rank(p.fame);
    let board = super::rival_board(c);
    let above = rank.saturating_sub(1).max(1);
    let rival = board.get(above as usize - 1).and_then(|id| c.driver_name(*id)).unwrap_or("next rival");
    if rank > 1 {
        let need = c.fame_for(rank.saturating_sub(1).max(1)).saturating_sub(p.fame);
        s += &format!("POPULARITY #{}{}   {} fame   next: #{} {} ({} to go)\n", rank, if rank >= 250 { " (unranked)" } else { "" }, fmt_num(p.fame as i64), above, rival, fmt_num(need as i64));
    } else {
        s += &format!("POPULARITY #1   {} fame - top of the Horizon\n", fmt_num(p.fame as i64));
    }
    s += &format!("CREDITS {} CR", fmt_num(p.credits));
    if !p.cars_won.is_empty() {
        s += &format!("   cars won: {}", p.cars_won.len());
    }
    if let Some(car) = cars.iter().next() {
        let m = &car.0.data.media_name;
        match super::player_class(c, m) {
            Some((class, pi)) => s += &format!("\nCAR {m}   class {} {pi}", c.class_name(class)),
            None => s += &format!("\nCAR {m}"),
        }
    }
    s += "\n\n";
    if let Some(e) = cat.events.iter().find(|e| e.recommended) {
        s += &format!("NEXT UP  {}  ({} · {}{})\n\n", e.name, TIER_NAMES[e.tier.min(6) as usize], e.kind.label(), e.class.as_ref().map_or(String::new(), |c| format!(" · {c}")));
    }
    // The page.
    let list = page_events(&cat, &events, ui.page);
    let done = list.iter().filter(|e| matches!(e.state, EventState::Completed { .. })).count();
    s += &format!("<  {}  >   {}/{} done\n", page_name(ui.page).to_uppercase(), done, list.len());
    if ui.page == 7 {
        for h in &c.hubs {
            let open = p.xp >= h.unlock_xp;
            s += &format!("  {}: {}\n", if h.name.is_empty() { format!("Hub {}", h.id) } else { h.name.clone() }, if open { "open".into() } else { format!("opens at {} XP", fmt_num(h.unlock_xp as i64)) });
        }
    }
    s += "\n";
    if list.is_empty() {
        s += "  (no events installed for this page)\n";
    }
    let first = ui.cursor.saturating_sub(8).min(list.len().saturating_sub(16));
    for (k, e) in list.iter().enumerate().skip(first).take(16) {
        let mark = if k == ui.cursor { ">" } else { " " };
        let state = match e.state {
            EventState::Completed { place } => format!("{place}{}", ordinal(place as u32)),
            EventState::Unlocked => "new".into(),
            EventState::Locked => "lock".into(),
        };
        let rec = if e.recommended { " *" } else { "" };
        let class = e.class.clone().unwrap_or_default();
        let reward = e.reward.map_or(String::new(), |r| format!("{} CR", fmt_num(r as i64)));
        let tail = match &e.lock_reason {
            Some(w) if e.state == EventState::Locked => format!("needs {w}"),
            _ => reward,
        };
        s += &format!("{mark} {state:<5} {:<34} {:<12} {:<14} {tail}{rec}\n", e.name, e.kind.label(), class);
    }
    if let Some(e) = list.get(ui.cursor) {
        let r = &events.races[e.race];
        s += &format!("\n{} - {:.1} km{}", e.name, r.length_m * r.laps as f32 / 1000.0, if r.laps > 1 { format!(", {} laps", r.laps) } else { String::new() });
        if let Some(rec) = p.events.get(&e.id) {
            s += &format!("   best {}{}{}", rec.best_place, ordinal(rec.best_place as u32), rec.best_time_s.map_or(String::new(), |t| format!(" in {:.2} s", t)));
        }
        if let Some(car) = &r.prize_car {
            s += &format!("\nPrize car for the win: {car}");
        }
        if !r.recommended.is_empty() {
            s += &format!("\nRecommended: {}", r.recommended.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
        }
    }
    if text.0 != s {
        text.0 = s;
    }
}

fn ordinal(n: u32) -> &'static str {
    match n % 100 {
        11..=13 => "th",
        _ => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}
