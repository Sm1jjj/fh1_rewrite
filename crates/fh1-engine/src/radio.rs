//! In-car radio: FH1's radio logic from `fh1-radio` (docs/RADIO.md) on its own output stream.
//! Needs the setup tool's `radio` group; without it the game runs without radio.
//!
//! Controls: D-pad right / left (as in FH1: "TO CHANGE RADIO STATIONS PRESS LEFT AND RIGHT ON
//! THE D-PAD"; Radio1 -> 2 -> 3 -> Off, wrapping, with the game's 1 s cooldown), or `.` / `,`.
//! The pause menu switches the mixer to the game's `Paused` snapshot (music at 0.25).
//! The now-playing card follows the game's HUD posts: a new song is announced 2 s after it
//! starts (after the DJ has finished), a station change at once with the station name.
//! `FH1_RADIO_LANG=DE|EN|ES|FR|IT|NL` picks the DJ language (default EN),
//! `FH1_RADIO=off` disables the radio, `FH1_RADIO_STATION=n` picks the start station (dial order),
//! `FH1_RADIO_VOLUME=0..1` scales it (on top of the pause menu's radio volume).
//! Mod stations (`mods/radio/stations/<Name>/*.mp3` + `logo.png`, fh1_radio::mods) sit between
//! Radio3 and Off; FH1's Anark widget has no slide for them, so they use this file's card plus
//! the folder's logo.
//! While the main menu or a loading card covers the world (`ui::loading::world_audio_allowed`) the radio is paused
//! (the game's SetRadioPaused: the clock, the song and the DJ stop and resume where they were); `FH1_MENU_AUDIO_GATE=0`
//! lets it play behind the covers as before.

use bevy::prelude::*;
use fh1_radio::config::station_display_name;
use fh1_radio::output::RadioPlayer;
use fh1_radio::system::HudPost;

use crate::ui::{Menu, Settings, UiFont};
use crate::Garage;

pub struct RadioPlugin;

impl Plugin for RadioPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, start_radio)
            .add_systems(Update, (radio_input.run_if(crate::ui::driving), radio_gate, radio_volume, radio_mix, radio_hud).chain());
    }
}

#[derive(Resource)]
struct Radio {
    player: RadioPlayer,
    /// `FH1_RADIO_VOLUME`, multiplied with the settings volume.
    env_volume: f32,
    /// HUD posts waiting for their delay (real seconds when due).
    pending: Vec<(HudPost, f32)>,
    /// The card on screen and when it appeared.
    shown: Option<(String, f32)>,
    snapshot: &'static str,
    /// Mod station index -> its logo (None = no logo.png).
    logos: Vec<(usize, Option<Handle<Image>>)>,
    silent: usize,
    /// Paused by the menu / loading-cover gate.
    gated: bool,
    /// Volume last handed to the mixer (the main thread only uses try_with, so a busy mixer retries next frame).
    applied_volume: f32,
}

#[derive(Component)]
struct RadioLogo;

#[derive(Component)]
struct RadioHud;

/// How long the now-playing card stays up (STOPGAP: the scene's own timeline decides this in
/// the game; the UI session's Anark player will replace this card).
const HUD_SHOW_S: f32 = 6.0;
const HUD_FADE_S: f32 = 0.6;

fn start_radio(mut commands: Commands, garage: Res<Garage>, font: Res<UiFont>, mut images: ResMut<Assets<Image>>) {
    if crate::audio::audio_disabled() || std::env::var("FH1_RADIO").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return;
    }
    let dir = garage.assets.join("radio");
    if !dir.join("radio.json").is_file() {
        info!("no radio installed ({}); run fh1setup to convert it", dir.display());
        return;
    }
    let lang = std::env::var("FH1_RADIO_LANG").unwrap_or_else(|_| "EN".into()).to_ascii_uppercase();
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
    // Starts the free-roam flow (fade up over levelFadeUpTime).
    let player = match RadioPlayer::start(dir, &lang, seed) {
        Ok(p) => p,
        Err(e) => return warn!("radio: {e:#}"),
    };
    if let Some(s) = std::env::var("FH1_RADIO_STATION").ok().and_then(|s| s.parse().ok()) {
        player.with(|m| m.system().select_external(s, false, 0));
    }
    // Booting under the main menu / loading card: paused before the first update (the flow above has only just been
    // queued; the fade-up hasn't produced anything audible yet).
    let gated = !crate::ui::loading::world_audio_allowed();
    if gated {
        player.with(|m| m.system().set_paused(true));
        info!("radio: paused behind the main menu / loading screen");
    }
    info!("radio: {} Hz on \"{}\", {lang}", player.sample_rate, player.device);
    let env_volume = std::env::var("FH1_RADIO_VOLUME").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let (mods, silent) = player.with(|m| (m.data().mods.clone(), m.system().silent_station()));
    let logos = mods.iter().map(|s| (s.index, s.logo.as_deref().and_then(|p| load_png(p, &mut images)))).collect();
    for s in &mods {
        info!("radio: mod station \"{}\" ({} tracks)", s.display, player.with(|m| m.data().radio.stations[s.index].playlist.items.len()));
    }
    commands.insert_resource(Radio { player, env_volume, pending: Vec::new(), shown: None, snapshot: "FreeRoam", logos, silent, gated, applied_volume: -1.0 });
    commands.spawn((
        RadioLogo,
        ImageNode::default(),
        Visibility::Hidden,
        Node { position_type: PositionType::Absolute, right: Val::Px(20.0), top: Val::Px(16.0), width: Val::Px(240.0), height: Val::Px(120.0), ..default() },
    ));
    commands.spawn((
        RadioHud,
        Text::new(""),
        font.text(22.0),
        TextColor(Color::WHITE.with_alpha(0.0)),
        TextLayout::justify(Justify::Right),
        // Top right: the bottom right is the tachometer (STOPGAP placement, not FH1's).
        Node { position_type: PositionType::Absolute, right: Val::Px(20.0), top: Val::Px(16.0), ..default() },
    ));
}

fn load_png(path: &std::path::Path, images: &mut Assets<Image>) -> Option<Handle<Image>> {
    use bevy::asset::RenderAssetUsages;
    use bevy::image::{CompressedImageFormats, ImageSampler, ImageType};
    let bytes = std::fs::read(path).ok()?;
    match Image::from_buffer(&bytes, ImageType::Extension("png"), CompressedImageFormats::NONE, true, ImageSampler::linear(), RenderAssetUsages::RENDER_WORLD) {
        Ok(img) => Some(images.add(img)),
        Err(e) => {
            warn!("radio: logo {}: {e}", path.display());
            None
        }
    }
}

fn radio_input(radio: Option<Res<Radio>>, keys: Res<ButtonInput<KeyCode>>, pads: Query<&Gamepad>) {
    let _watch = crate::perf::watch("radio_input");
    let Some(radio) = radio else { return };
    let right = keys.just_pressed(KeyCode::Period) || pads.iter().any(|p| p.just_pressed(GamepadButton::DPadRight));
    let left = keys.just_pressed(KeyCode::Comma) || pads.iter().any(|p| p.just_pressed(GamepadButton::DPadLeft));
    if right != left {
        // The game's D-pad path: cooldown, radio volume > 0 and the switching lock are checked inside.
        radio.player.try_with(|m| m.system().dpad(left));
    }
}

/// Pauses the radio while a cover hides the world, resumes it with the picture (module doc).
fn radio_gate(radio: Option<ResMut<Radio>>) {
    let _watch = crate::perf::watch("radio_gate");
    let Some(mut radio) = radio else { return };
    let gated = !crate::ui::loading::world_audio_allowed();
    if gated != radio.gated && radio.player.try_with(|m| m.system().set_paused(gated)).is_some() {
        radio.gated = gated;
        info!("radio: {}", if gated { "paused (menu / loading screen)" } else { "resumed" });
    }
}

fn radio_volume(radio: Option<ResMut<Radio>>, settings: Res<Settings>) {
    let _watch = crate::perf::watch("radio_volume");
    let Some(mut radio) = radio else { return };
    let v = if crate::ui::intro::radio_off() { 0.0 } else { settings.radio_volume * radio.env_volume };
    if v != radio.applied_volume && radio.player.try_with(|m| m.volume = v).is_some() {
        radio.applied_volume = v;
    }
}

/// Gameplay mixer snapshot: `Paused` while the pause menu is open, else `FreeRoam`.
fn radio_mix(radio: Option<ResMut<Radio>>, menu: Res<Menu>, duck: Option<Res<crate::vo::VoDuck>>) {
    let _watch = crate::perf::watch("radio_mix");
    let Some(mut radio) = radio else { return };
    // VO ducks the radio (vo.rs VoDuck, the game's VOPlaying snapshot).
    let name = if menu.open { "Paused" } else if duck.is_some_and(|d| d.0) { "VOPlaying" } else { "FreeRoam" };
    if name != radio.snapshot && radio.player.try_with(|m| m.set_snapshot(name)).is_some() {
        radio.snapshot = name;
    }
}

/// Card text for a post: the station line only when the station changed (SHOW_STATION), the
/// song line when there is one (SHOW_TRACK_NAME / SHOW_ARTIST_NAME), "RADIO OFF" for Off.
fn card(post: &HudPost, station_names: &[String]) -> String {
    let station = station_names.get(post.station).cloned().unwrap_or_default();
    let song = match (post.artist.is_empty(), post.title.is_empty()) {
        (true, true) => String::new(),
        (false, false) => format!("{} - {}", post.artist, post.title),
        (false, true) => post.artist.clone(),
        (true, false) => post.title.clone(),
    };
    match (post.show_station, song.is_empty()) {
        (true, true) => station,
        (true, false) => format!("{station}\n{song}"),
        (false, _) => song,
    }
}

fn radio_hud(
    radio: Option<ResMut<Radio>>,
    time: Res<Time>,
    mut hud: Query<(&mut Text, &mut TextColor, &mut Node), (With<RadioHud>, Without<RadioLogo>)>,
    mut logo: Query<(&mut ImageNode, &mut Visibility, &Node), (With<RadioLogo>, Without<RadioHud>)>,
    mut fh1_posts: MessageWriter<crate::ui::hud::RadioHudPost>,
    fh1: Option<Res<crate::ui::hud::Fh1Hud>>,
) {
    let _watch = crate::perf::watch("radio_hud");
    let Some(mut radio) = radio else { return };
    let Ok((mut text, mut color, mut text_node)) = hud.single_mut() else { return };
    let now = time.elapsed_secs();
    // Never wait on the mixer here (every frame): a busy mix thread just delays the posts by a frame.
    let Some((posts, names)) = radio.player.try_with(|m| {
        let data = m.data();
        let names: Vec<String> = data
            .radio
            .stations
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if let Some(ms) = data.mods.iter().find(|ms| ms.index == i) {
                    return ms.display.to_uppercase();
                }
                station_display_name(&s.name).map(|(d, _)| d.to_owned()).unwrap_or_else(|| if s.is_off { "RADIO OFF".into() } else { s.name.clone() })
            })
            .collect();
        (m.take_hud_posts(), names)
    }) else {
        return;
    };
    for p in posts {
        let due = now + p.delay;
        radio.pending.push((p, due));
    }
    // The newest due post wins (the game's receiver keeps only the latest).
    if let Some(i) = radio.pending.iter().rposition(|(_, due)| *due <= now) {
        let (mut post, _) = radio.pending.remove(i);
        radio.pending.retain(|(_, due)| *due > now);
        let mod_logo = radio.logos.iter().find(|(i, _)| *i == post.station).map(|(_, l)| l.clone());
        // FH1's own radio widget (947_HUD) shows it when the `ui` group is installed; its logo
        // slides are Radio1..3 + OFF (dial positions 0..3), so mod stations use the card here.
        let fh1_shows = fh1.is_some() && mod_logo.is_none();
        let card_text = if fh1_shows { String::new() } else { card(&post, &names) };
        if mod_logo.is_none() {
            if post.station == radio.silent {
                post.station = 3;
            }
            fh1_posts.write(crate::ui::hud::RadioHudPost(post));
        }
        if let Ok((mut img, mut vis, _)) = logo.single_mut() {
            match mod_logo.flatten() {
                Some(h) => {
                    img.image = h;
                    *vis = Visibility::Inherited;
                }
                None => *vis = Visibility::Hidden,
            }
        }
        if !card_text.is_empty() {
            radio.shown = Some((card_text, now));
        }
    }
    let logo_shown = logo.single().is_ok_and(|(_, v, _)| *v != Visibility::Hidden);
    let logo_h = logo.single().map_or(0.0, |(_, _, n)| if let Val::Px(h) = n.height { h } else { 0.0 });
    let top = if logo_shown { 16.0 + logo_h + 6.0 } else { 16.0 };
    if text_node.top != Val::Px(top) {
        text_node.top = Val::Px(top);
    }
    let Some((t, since)) = &radio.shown else {
        color.0 = Color::WHITE.with_alpha(0.0);
        return;
    };
    let age = now - since;
    let alpha = if age < HUD_SHOW_S { 1.0 } else { (1.0 - (age - HUD_SHOW_S) / HUD_FADE_S).max(0.0) };
    if text.0 != *t {
        text.0 = t.clone();
    }
    color.0 = Color::WHITE.with_alpha(alpha);
    if let Ok((mut img, mut vis, _)) = logo.single_mut() {
        img.color = Color::WHITE.with_alpha(alpha);
        if alpha <= 0.0 {
            *vis = Visibility::Hidden;
        }
    }
}
