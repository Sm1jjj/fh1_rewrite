//! Race HUD (R1): plain Bevy UI text, not FH1's Anark HUD (UI parity not required, everything must be clear):
//! top left = position / lap / race time / lap times / split delta; centre = countdown, WRONG WAY, prompts and
//! flashes; centre panel = results; left panel = the event list (F6).

use bevy::prelude::*;

use super::{Events, RacePhase, RaceState};

#[derive(Component)]
pub enum HudText {
    Info,
    Centre,
    Prompt,
    Panel,
}

/// Glyph size of the centre text (countdown, GO, WRONG WAY); shown sizes come from the node scale.
const CENTRE_PX: f32 = 150.0;

fn fmt_time(s: f32) -> String {
    let s = s.max(0.0);
    format!("{}:{:02}.{:03}", (s / 60.0) as u32, (s % 60.0) as u32, ((s.fract()) * 1000.0) as u32)
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

pub fn spawn_race_hud(mut commands: Commands, font: Option<Res<crate::ui::UiFont>>) {
    let f = |px: f32| font.as_ref().map_or_else(|| TextFont { font_size: bevy::text::FontSize::Px(px), ..default() }, |f| f.text(px));
    let shadow = TextShadow { offset: Vec2::new(2.0, 2.0), color: Color::BLACK.with_alpha(0.8) };
    commands.spawn((
        HudText::Info,
        Text::new(""),
        f(26.0),
        TextColor(Color::WHITE),
        shadow,
        Node { position_type: PositionType::Absolute, left: Val::Px(24.0), top: Val::Px(110.0), ..default() },
    ));
    commands.spawn((
        HudText::Centre,
        Text::new(""),
        // One glyph size (CENTRE_PX); the countdown pop scales the node (UiTransform). Resizing the font every frame
        // filled the glyph atlas and blanked digits in the other HUD texts (2026-10-08).
        f(CENTRE_PX),
        TextColor(Color::WHITE),
        shadow,
        TextLayout::justify(Justify::Center),
        UiTransform { scale: Vec2::splat(96.0 / CENTRE_PX), ..UiTransform::IDENTITY },
        Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            // Above the gates (FH1_RACE_FX=0: the old 26 %).
            top: Val::Percent(if super::visuals::enabled() { 14.0 } else { 26.0 }),
            justify_content: JustifyContent::Center,
            ..default()
        },
    ));
    commands.spawn((
        HudText::Prompt,
        Text::new(""),
        f(28.0),
        TextColor(Color::WHITE),
        shadow,
        TextLayout::justify(Justify::Center),
        Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), bottom: Val::Percent(20.0), justify_content: JustifyContent::Center, ..default() },
    ));
    commands.spawn((
        HudText::Panel,
        Text::new(""),
        f(24.0),
        TextColor(Color::WHITE),
        BackgroundColor(Color::BLACK.with_alpha(0.0)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(25.0),
            top: Val::Percent(12.0),
            padding: UiRect::all(Val::Px(18.0)),
            ..default()
        },
    ));
}

pub fn race_hud(
    events: Res<Events>,
    rs: Res<RaceState>,
    rewards: Option<Res<crate::progression::LastRewards>>,
    anark: Option<Res<super::anark_hud::AnarkRaceHud>>,
    mut last_px: Local<f32>,
    mut texts: Query<(&HudText, &mut Text, &mut TextColor, Option<&mut BackgroundColor>, Option<&mut UiTransform>, &mut Node)>,
) {
    if !super::races_on() || events.races.is_empty() {
        return;
    }
    let def = rs.race.and_then(|i| events.races.get(i));
    let player = rs.racers.first();
    // Countdown / GO pop (FH1_RACE_FX=0: fixed size): each number starts big and settles; GO grows then fades in size.
    let centre_px = match rs.phase {
        RacePhase::Countdown { left_s } if super::visuals::enabled() => 120.0 * (1.0 + 0.5 * left_s.fract().powi(4)),
        RacePhase::Racing if rs.clock_s < 1.0 && super::visuals::enabled() => 150.0 - 40.0 * rs.clock_s,
        _ => 96.0,
    };
    for (kind, mut text, mut colour, bg, tf, mut node) in &mut texts {
        if let (HudText::Centre, Some(mut tf)) = (kind, tf) {
            if (*last_px - centre_px).abs() > 0.5 {
                *last_px = centre_px;
                tf.scale = Vec2::splat(centre_px / CENTRE_PX);
            }
        }
        let s = match kind {
            // The game's own race widgets (race/anark_hud.rs) replace the panel, countdown and results.
            HudText::Info | HudText::Centre if anark.as_ref().is_some_and(|a| a.active) => String::new(),
            HudText::Info => match (def, player) {
                (Some(d), Some(p)) if !matches!(rs.phase, RacePhase::Idle | RacePhase::Results) => {
                    let mut s = format!("{}\n", d.name);
                    s += &format!("POS {} / {}\n", p.position, rs.racers.len());
                    if d.laps > 1 {
                        s += &format!("LAP {} / {}\n", p.lap.min(d.laps), d.laps);
                    }
                    s += &format!("TIME {}\n", fmt_time(p.finished_s.unwrap_or(rs.clock_s)));
                    if let Some(last) = rs.laps.last() {
                        let best = rs.laps.iter().copied().fold(f32::INFINITY, f32::min);
                        s += &format!("LAST {}   BEST {}\n", fmt_time(*last), fmt_time(best));
                    }
                    if let Some((split, best)) = rs.last_split {
                        match best {
                            Some(b) => s += &format!("SPLIT {}  ({}{:.2})\n", fmt_time(split), if split <= b { "-" } else { "+" }, (split - b).abs()),
                            None => s += &format!("SPLIT {}\n", fmt_time(split)),
                        }
                    }
                    let per_lap = d.gates.len() as u32;
                    let done = p.gates_done % per_lap.max(1);
                    s += &format!("CHECKPOINT {} / {}   {:.0} m", (done + 1).min(per_lap), per_lap, p.to_next);
                    s
                }
                _ => String::new(),
            },
            HudText::Centre => match rs.phase {
                RacePhase::Grid { .. } => {
                    colour.0 = Color::WHITE;
                    "READY".into()
                }
                RacePhase::Countdown { left_s } => {
                    // 3 red, 2 amber, 1 yellow (the start gantry's lights), then GO green.
                    colour.0 = match left_s.ceil() as u32 {
                        3.. if super::visuals::enabled() => Color::srgb(1.0, 0.22, 0.18),
                        2 if super::visuals::enabled() => Color::srgb(1.0, 0.55, 0.1),
                        _ => Color::srgb(1.0, 0.85, 0.1),
                    };
                    format!("{}", left_s.ceil().max(1.0) as u32)
                }
                RacePhase::Racing if rs.clock_s < 1.0 => {
                    colour.0 = Color::srgb(0.3, 1.0, 0.4);
                    "GO!".into()
                }
                RacePhase::Racing if rs.wrong_way_s > 2.0 => {
                    colour.0 = Color::srgb(1.0, 0.25, 0.2);
                    "WRONG WAY".into()
                }
                RacePhase::Finished { .. } => {
                    colour.0 = Color::WHITE;
                    player.map(|p| format!("FINISHED {}{}", p.position, ordinal(p.position))).unwrap_or_default()
                }
                _ => {
                    colour.0 = Color::WHITE;
                    String::new()
                }
            },
            HudText::Prompt => match (rs.phase, rs.prompt) {
                (RacePhase::Idle, Some(i)) if !rs.list_open => {
                    let r = &events.races[i];
                    let laps = if r.laps > 1 { format!(", {} laps", r.laps) } else { String::new() };
                    let class = r.target_class.and_then(|t| events.career.class_label(t)).map_or(String::new(), |c| format!(" - class {c}"));
                    let tier = crate::progression::TIER_NAMES[crate::progression::event_tier(r) as usize];
                    let head = format!("{}\n{tier} · {}{} - {:.1} km - {} CR{class}", r.name, r.kind, laps, r.length_m / 1000.0 * r.laps as f32, crate::progression::place_credits(r, &events.scoring, 1));
                    match (&rs.flash, &rs.prompt_locked) {
                        (Some(f), _) => format!("{head}\n{}", f.0),
                        (None, Some(why)) => format!("{head}\nLOCKED - needs {why}"),
                        (None, None) => format!("{head}\nStop and press A / Enter to start"),
                    }
                }
                (RacePhase::Idle, None) if !rs.list_open => rs.flash.as_ref().map(|f| f.0.clone()).unwrap_or_default(),
                // FH1 shows no key prompts while racing: only the flashes, and the reset hint when off track / stuck.
                (RacePhase::Grid { .. } | RacePhase::Countdown { .. }, _) if anark.as_ref().is_some_and(|a| a.active) => rs.flash.as_ref().map(|f| f.0.clone()).unwrap_or_default(),
                (RacePhase::Racing, _) if anark.as_ref().is_some_and(|a| a.active) => match &rs.flash {
                    Some(f) => f.0.clone(),
                    None if rs.lost_s() > 1.5 => "R / Y: reset to track".into(),
                    None => String::new(),
                },
                (RacePhase::Finished { .. }, _) if anark.as_ref().is_some_and(|a| a.active) => String::new(),
                (RacePhase::Grid { .. } | RacePhase::Countdown { .. }, _) => rs.flash.as_ref().map_or_else(|| "F7: quit race".into(), |f| format!("{}\nF7: quit race", f.0)),
                (RacePhase::Racing, _) => rs.flash.as_ref().map(|f| f.0.clone()).unwrap_or_else(|| "R / Y: reset to track    F7: quit race".into()),
                (RacePhase::Finished { .. }, _) => "A / Enter: results".into(),
                _ => String::new(),
            },
            HudText::Panel => {
                let mut s = String::new();
                if rs.phase == RacePhase::Results && !anark.as_ref().is_some_and(|a| a.active) {
                    if let Some(d) = def {
                        s += &format!("{}\n{}\n\n", d.name, d.kind);
                        let mut order: Vec<&super::Racer> = rs.racers.iter().collect();
                        order.sort_by_key(|r| r.position);
                        for r in order {
                            let t = r.finished_s.map_or("DNF".to_string(), fmt_time);
                            let credits = crate::progression::place_credits(d, &events.scoring, r.position);
                            let you = if r.is_player { "  <" } else { "" };
                            s += &format!("{:>2}{}  {:<24} {:>10}   {:>7} CR{you}\n", r.position, ordinal(r.position), r.name, t, credits);
                        }
                        if !rs.laps.is_empty() && d.laps > 1 {
                            let best = rs.laps.iter().copied().fold(f32::INFINITY, f32::min);
                            s += &format!("\nBest lap {}", fmt_time(best));
                        }
                        // Career rewards (progression.rs).
                        if let Some(rw) = rewards.as_ref().filter(|rw| rw.race == rs.race && !rw.lines.is_empty()) {
                            s += "\n";
                            for l in &rw.lines {
                                s += &format!("\n{l}");
                            }
                        }
                        s += "\n\nA / Enter: continue";
                    }
                } else if rs.list_open && rs.phase == RacePhase::Idle {
                    s += "EVENTS   (Up / Down, Enter: start, F6: close)\n\n";
                    let n = events.races.len();
                    let first = rs.list_cursor.saturating_sub(7).min(n.saturating_sub(15));
                    for (i, r) in events.races.iter().enumerate().skip(first).take(15) {
                        let mark = if i == rs.list_cursor { ">" } else { " " };
                        let laps = if r.laps > 1 { format!("{} laps", r.laps) } else { "1 lap".into() };
                        s += &format!("{mark} {:<38} {:<26} {:>7}  {:>5.1} km\n", r.name, r.kind, laps, r.length_m / 1000.0);
                    }
                }
                if let Some(mut bg) = bg {
                    bg.0 = Color::BLACK.with_alpha(if s.is_empty() { 0.0 } else { 0.7 });
                }
                s
            }
        };
        // Empty texts leave the layout (Display::None): bevy_ui's layout / text measuring skip them (UI cost, fast path).
        if crate::ui::scene::fastpath() {
            let display = if s.is_empty() { Display::None } else { Display::Flex };
            if node.display != display {
                node.display = display;
            }
        }
        if text.0 != s {
            text.0 = s;
        }
    }
}
