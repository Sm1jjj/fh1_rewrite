//! Mission HUD: our own text lines (title + live lines under the skill HUD, a prompt line low centre, a short flash
//! line), the HUD's objective line / satnav target hand-over while an activity runs (ui/notify.rs `Objective`,
//! ui/minimap.rs `SatNav`), and pop-ups through ui/notify.rs `HudNotify`.

use bevy::prelude::*;

/// What the mission HUD shows this frame. Activities set it; nothing persists between activities.
#[derive(Resource, Default)]
pub struct MissionHud {
    /// Big line (mission name / timer).
    pub title: Option<String>,
    /// Lines under it (score, target, speed).
    pub lines: Vec<String>,
    /// Prompt low centre ("Press A to ...").
    pub prompt: Option<String>,
    /// A short message (text, seconds left), e.g. a speed-camera result.
    pub flash: Option<(String, f32)>,
}

impl MissionHud {
    pub fn clear(&mut self) {
        self.title = None;
        self.lines.clear();
    }
    /// Everything off (an activity ended or was aborted).
    pub fn reset(&mut self) {
        self.title = None;
        self.lines.clear();
        self.prompt = None;
        self.flash = None;
    }
    pub fn flash(&mut self, s: impl Into<String>, secs: f32) {
        self.flash = Some((s.into(), secs));
    }
}

#[derive(Component)]
pub struct MissionHudMain;
#[derive(Component)]
pub struct MissionHudPrompt;
#[derive(Component)]
pub struct MissionHudFlash;

pub fn spawn_hud(mut commands: Commands, font: Option<Res<crate::ui::UiFont>>) {
    let f = |px: f32| font.as_ref().map_or_else(|| TextFont { font_size: bevy::text::FontSize::Px(px), ..default() }, |f| f.text(px));
    let shadow = TextShadow { offset: Vec2::new(2.0, 2.0), color: Color::BLACK.with_alpha(0.8) };
    let row = |top: f32| Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), top: Val::Percent(top), justify_content: JustifyContent::Center, ..default() };
    commands.spawn((MissionHudMain, Text::new(""), f(30.0), TextColor(Color::WHITE), shadow, TextLayout::justify(Justify::Center), row(14.0)));
    commands.spawn((MissionHudFlash, Text::new(""), f(36.0), TextColor(Color::srgb(1.0, 0.85, 0.1)), shadow, TextLayout::justify(Justify::Center), row(40.0)));
    commands.spawn((MissionHudPrompt, Text::new(""), f(26.0), TextColor(Color::srgb(0.85, 0.95, 1.0)), shadow, TextLayout::justify(Justify::Center), row(74.0)));
}

#[allow(clippy::type_complexity)]
pub fn draw_hud(
    time: Res<Time>,
    mut hud: ResMut<MissionHud>,
    menu: Option<Res<crate::ui::Menu>>,
    rig: Option<Res<crate::camera::CameraRig>>,
    mut main: Query<&mut Text, (With<MissionHudMain>, Without<MissionHudPrompt>, Without<MissionHudFlash>)>,
    mut prompt: Query<&mut Text, (With<MissionHudPrompt>, Without<MissionHudMain>, Without<MissionHudFlash>)>,
    mut flash: Query<&mut Text, (With<MissionHudFlash>, Without<MissionHudMain>, Without<MissionHudPrompt>)>,
) {
    if let Some((_, t)) = hud.flash.as_mut() {
        *t -= time.delta_secs();
        if *t <= 0.0 {
            hud.flash = None;
        }
    }
    // Hidden over menus; in photo mode only the main lines stay (the photo-shoot instruction).
    let menu_open = menu.is_some_and(|m| m.open);
    let photo = rig.is_some_and(|r| r.photo);
    let set = |t: &mut Text, s: String| {
        if t.0 != s {
            t.0 = s;
        }
    };
    let mut body = String::new();
    if !menu_open {
        if let Some(t) = &hud.title {
            body.push_str(t);
        }
        for l in &hud.lines {
            body.push('\n');
            body.push_str(l);
        }
    }
    for mut t in &mut main {
        set(&mut t, body.clone());
    }
    // The prompt is a driving prompt: not over menus or in photo mode (P18; it used to stay up in photo mode).
    let p = if menu_open || photo { String::new() } else { hud.prompt.clone().unwrap_or_default() };
    for mut t in &mut prompt {
        set(&mut t, p.clone());
    }
    let fl = if menu_open || photo { String::new() } else { hud.flash.as_ref().map(|f| f.0.clone()).unwrap_or_default() };
    for mut t in &mut flash {
        set(&mut t, fl.clone());
    }
}

/// Objective line + satnav target handed to an activity and given back afterwards.
#[derive(Resource, Default)]
pub struct NavGuard {
    saved: Option<(Option<String>, Option<Vec2>, Option<Vec2>)>,
}

impl NavGuard {
    /// Show `text` as the objective and route the satnav to `target` (saving what was there the first time).
    pub fn set(&mut self, obj: &mut crate::ui::notify::Objective, nav: Option<&mut crate::ui::minimap::SatNav>, text: Option<String>, target: Option<Vec2>) {
        let nav_target = nav.as_ref().and_then(|n| n.target);
        if self.saved.is_none() {
            self.saved = Some((obj.text.clone(), obj.target, nav_target));
        }
        obj.text = text;
        obj.target = None;
        if let Some(n) = nav {
            if n.target != target {
                n.target = target;
            }
        }
    }

    /// Drop what was saved without giving it back (the world changed: ui/world_load.rs already reset both).
    pub fn forget(&mut self) {
        self.saved = None;
    }

    /// Give the objective and satnav back.
    pub fn restore(&mut self, obj: &mut crate::ui::notify::Objective, nav: Option<&mut crate::ui::minimap::SatNav>) {
        if let Some((text, target, nav_target)) = self.saved.take() {
            obj.text = text;
            obj.target = target;
            if let Some(n) = nav {
                n.target = nav_target;
            }
        }
    }

    #[allow(dead_code)]
    pub fn active(&self) -> bool {
        self.saved.is_some()
    }
}

/// A HUD pop-up (ui/notify.rs CombinedNotification): title, sub line, third line.
pub fn notify(out: &mut MessageWriter<crate::ui::notify::HudNotify>, lines: &[&str]) {
    out.write(crate::ui::notify::HudNotify { lines: lines.iter().map(|s| (*s).to_owned()).collect() });
}

/// Speed in the player's units (Options metric) with its unit.
pub fn speed_text(mph: f32, metric: bool) -> String {
    if metric {
        format!("{:.0} KM/H", mph * 1.609_344)
    } else {
        format!("{mph:.0} MPH")
    }
}

/// m:ss.s
pub fn clock(s: f32) -> String {
    let s = s.max(0.0);
    format!("{}:{:04.1}", (s / 60.0) as u32, s % 60.0)
}
