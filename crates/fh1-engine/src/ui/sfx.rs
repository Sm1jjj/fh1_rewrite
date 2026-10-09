//! UI sound effects (W8): menus, pause, world map, notifications, launch screen.
//!
//! Plans: data/extracted/plans/ui_audio.md (§0, §2.4 loops, §4.2 scene -> sound table, §5.2 one-shot / loop / stream,
//! §5.6 scene overrides) and architecture.md §2b / §6 / §9 (W8).
//!
//! How it works: any system writes a [`UiSfx`] message carrying a `ui4audio.xml` `play` name ([`keys`]). This plugin
//! resolves it through `fh1_audio::ui_events::UiEvents` (play name -> cue -> bank sample, with the scene overrides) and
//! plays it on the shared PCM stream's UI bus (`fh1_audio::pcm::shared().ui()`), at `Settings::ui_volume`. It runs every
//! frame, also while the game is paused or a menu is up, and is not gated by `loading::world_audio_allowed` or `ui::driving`.
//!
//! Besides the messages, a small system turns four UI states into sounds by itself (no hooks needed in their owners):
//! pause menu open / close (`Pause` / `UnPause`), world map page up / down (`MAP_AmbientBackgroundLoop` start / stop),
//! launch screen up / gone (`SplashScreen` loop start / stop) and a yes / no dialog opening (`PopupNormal`).
//!
//! INFERRED (ui_audio.md §0.2: no scene plays a sound, the game code raises the names, and the call sites are not
//! captured yet): every link from a screen or input to a play name comes from the §4.2 table, i.e. from the names.
//! VERIFIED: the play name -> cue table (`ui4audio.json`), the scene overrides (`Accept` is silent in the Showroom).
//! Loops end only through [`UiSfx::stop`] (the XML has no loop-end field); `Blank` cues are intentionally silent.
//!
//! Flags: `FH1_UI_SFX=0` (or `off`) turns all UI sound off. `FH1_UI_SFX_TEST=<play name>` plays that one key once at
//! start (T9, capture checks). `FH1_AUDIO=off` (and the agent-launch silence, `audio::audio_disabled`) also silence it.
//! Without the converted UI audio (`<assets>/audio/ui/ui4audio.json`, setup group audio-3) nothing is played.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bevy::prelude::*;
use fh1_audio::pcm::VoiceId;
use fh1_audio::ui_events::{Resolved, UiEvents};
use fh1_audio::ui_sfx::{UiClip, UiSfxPlayer};

use super::cards::Cards;
use super::loading::Loading;
use super::{Menu, Settings};
use crate::Garage;

/// ui4audio `play` names used by the hooks (typos are compile errors).
#[allow(dead_code)]
pub mod keys {
    pub const ACCEPT: &str = "Accept";
    pub const CANCEL: &str = "Cancel";
    pub const DENY_ACCESS: &str = "DenyAccess";
    pub const VSCROLL: &str = "Vscroll";
    pub const HSCROLL: &str = "Hscroll";
    pub const GRID_SCROLL: &str = "GridScroll";
    pub const SCROLL_END: &str = "ScrollEnd";
    pub const PAUSE: &str = "Pause";
    pub const UNPAUSE: &str = "UnPause";
    pub const POPUP_NORMAL: &str = "PopupNormal";
    pub const POPUP_ERROR: &str = "PopupError";
    pub const SPINNER: &str = "Spinner";
    pub const SPINNER_END: &str = "SpinnerEnd";
    pub const SLIDER_STEP_INCREMENT: &str = "SliderStep_Increment";
    pub const SLIDER_STEP_DECREMENT: &str = "SliderStep_Decrement";
    pub const SLIDER_SFX: &str = "Slider_SFX";
    pub const MAP_SNAP: &str = "Map_Snap";
    pub const MAP_ROUTE_ADD: &str = "Map_RouteAdd";
    pub const MAP_ROUTE_REMOVE: &str = "Map_RouteRemove";
    pub const MAP_AMBIENT_LOOP: &str = "MAP_AmbientBackgroundLoop";
    pub const MAP_EVENT_POPUP: &str = "Map_EventPopUp";
    pub const NOTIFICATION_APPEAR: &str = "Hud_GeneralNotificationAppear";
    pub const NOTIFICATION_DISAPPEAR: &str = "Hud_GeneralNotificationDisappear";
    pub const SPLASH_SCREEN: &str = "SplashScreen";
    pub const CAR_ADDED_TO_GARAGE: &str = "CarAddedToGarage";
}

/// Play (or stop) one UI sound. `key` is a `ui4audio.xml` `play` name (see [`keys`]).
#[derive(Message, Clone, Copy, Debug)]
pub struct UiSfx {
    pub key: &'static str,
    /// Stop the loop started by `key` instead of playing it.
    pub stop: bool,
}

impl UiSfx {
    pub const fn play(key: &'static str) -> Self {
        Self { key, stop: false }
    }

    pub const fn stop(key: &'static str) -> Self {
        Self { key, stop: true }
    }
}

/// The scene whose overrides apply (ui4audio_scene_overrides.json: `924_POST_RACE`, `244_C_CAL_EVENTRESULTS`,
/// `075_SHOWROOM_HOMESPACE`). Nothing sets it yet; a screen that wants its overrides sets it while it is up.
#[derive(Resource, Default)]
pub struct UiSfxScene(pub Option<&'static str>);

/// Hook helper: the sound of a browser step (`ui/browser.rs` `Pick`): moved -> Vscroll, back -> Cancel, chosen -> Accept.
pub fn pick(p: &super::browser::Pick) -> Option<UiSfx> {
    use super::browser::Pick;
    match p {
        Pick::Browsing { changed: true } => Some(UiSfx::play(keys::VSCROLL)),
        Pick::Browsing { changed: false } => None,
        Pick::Leave => Some(UiSfx::play(keys::CANCEL)),
        Pick::Car(_) | Pick::Map(_) => Some(UiSfx::play(keys::ACCEPT)),
    }
}

/// Hook helper: the sound of changing an Options row left / right (`dir` -1 / 1; 0 = Enter): the volume sliders step,
/// every other row is a spinner.
pub(super) fn option_step(o: super::Opt, dir: i32) -> UiSfx {
    use super::Opt;
    match o {
        Opt::EngineVolume | Opt::RadioVolume | Opt::UiVolume | Opt::AmbientVolume => {
            UiSfx::play(if dir < 0 { keys::SLIDER_STEP_DECREMENT } else { keys::SLIDER_STEP_INCREMENT })
        }
        _ => UiSfx::play(keys::SPINNER),
    }
}

/// Same key within this many seconds is dropped (held-key repeat must not machine-gun).
const DEBOUNCE_S: f64 = 0.040;
/// Fade of a stopped loop.
const STOP_FADE_S: f32 = 0.15;
/// Long streams loaded ahead on a worker thread (their first `play` would otherwise decode on demand).
const PRELOAD_KEYS: [&str; 2] = [keys::MAP_AMBIENT_LOOP, keys::SPLASH_SCREEN];

#[derive(Resource)]
pub struct UiSfxState {
    player: Option<UiSfxPlayer>,
    events: Option<Arc<UiEvents>>,
    /// `Time<Real>` seconds of the last play per key (debounce).
    last: HashMap<&'static str, f64>,
    /// Running loops by the key that started them.
    loops: HashMap<&'static str, VoiceId>,
    /// Keys / cues already reported (Unresolved), so each is logged once.
    warned: HashSet<String>,
    /// Played before the first message (FH1_UI_SFX_TEST).
    pending: Vec<UiSfx>,
    /// Last `Settings::ui_volume` given to the player (NaN = never).
    volume: f32,
}

impl Default for UiSfxState {
    fn default() -> Self {
        Self { player: None, events: None, last: HashMap::new(), loops: HashMap::new(), warned: HashSet::new(), pending: Vec::new(), volume: f32::NAN }
    }
}

pub struct UiSfxPlugin;

impl Plugin for UiSfxPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiSfxState>()
            .init_resource::<UiSfxScene>()
            .add_message::<UiSfx>()
            .add_systems(Startup, start)
            .add_systems(Update, (state_edges, play).chain());
    }
}

fn flag_off() -> bool {
    std::env::var("FH1_UI_SFX").is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "0" | "off" | "false"))
}

fn start(mut st: ResMut<UiSfxState>, garage: Res<Garage>) {
    if crate::audio::audio_disabled() {
        return info!("ui sfx: disabled (agent launch; FH1_AUDIO=on to enable)");
    }
    if flag_off() {
        return info!("ui sfx: off (FH1_UI_SFX=0)");
    }
    let dir = garage.assets.join("audio");
    if !dir.join("ui").join("ui4audio.json").is_file() {
        return info!("ui sfx: no UI audio installed (setup group audio-3)");
    }
    let events = match UiEvents::load(&dir) {
        Ok(e) => Arc::new(e),
        Err(e) => return warn!("ui sfx: {e:#}"),
    };
    let Some(pcm) = fh1_audio::pcm::shared() else {
        return warn!("ui sfx: no audio output");
    };
    let player = pcm.ui();
    player.set_audio_dir(&dir);
    info!("ui sfx: ready ({})", dir.display());
    // The small bank and the two long streams, off the main thread.
    {
        let (p, ev) = (player.clone(), events.clone());
        let spawned = std::thread::Builder::new().name("ui-sfx-preload".into()).spawn(move || {
            match p.preload_bank("UIInGame") {
                Ok(n) => info!("ui sfx: {n} UIInGame samples loaded"),
                Err(e) => warn!("ui sfx: preload UIInGame: {e:#}"),
            }
            for key in PRELOAD_KEYS {
                if let Resolved::Sample { bank, index, .. } = ev.resolve(key, None) {
                    if let Err(e) = p.preload(&bank, index) {
                        warn!("ui sfx: preload {key}: {e:#}");
                    }
                }
            }
        });
        if let Err(e) = spawned {
            warn!("ui sfx: preload thread: {e}");
        }
    }
    if let Ok(v) = std::env::var("FH1_UI_SFX_TEST") {
        if !v.is_empty() {
            // One leaked string for the whole run: the key type is 'static.
            let key: &'static str = Box::leak(v.into_boxed_str());
            info!("ui sfx: test key {key}");
            st.pending.push(UiSfx::play(key));
        }
    }
    st.player = Some(player);
    st.events = Some(events);
}

/// Previous values of the UI states that make sounds by themselves.
#[derive(Default)]
struct Edges {
    menu: bool,
    map: bool,
    launch: bool,
    popup: bool,
}

/// Pause menu, world map page, launch screen and dialogs: a sound on each change (INFERRED links, ui_audio.md §4.2
/// rows 1, 5, 6, 11, 34).
fn state_edges(
    menu: Option<Res<Menu>>,
    loading: Option<Res<Loading>>,
    cards: Option<Res<Cards>>,
    mut prev: Local<Edges>,
    mut out: MessageWriter<UiSfx>,
) {
    if let Some(menu) = menu.as_deref() {
        if menu.open != prev.menu {
            prev.menu = menu.open;
            out.write(UiSfx::play(if menu.open { keys::PAUSE } else { keys::UNPAUSE }));
        }
        let map = menu.map_open();
        if map != prev.map {
            prev.map = map;
            out.write(if map { UiSfx::play(keys::MAP_AMBIENT_LOOP) } else { UiSfx::stop(keys::MAP_AMBIENT_LOOP) });
        }
        let popup = menu.open && (menu.shop.confirm.is_some() || cards.as_deref().is_some_and(|c| c.dialog.is_some()));
        if popup && !prev.popup {
            out.write(UiSfx::play(keys::POPUP_NORMAL));
        }
        prev.popup = popup;
    }
    if let Some(l) = loading.as_deref() {
        let launch = l.on_launch();
        if launch != prev.launch {
            prev.launch = launch;
            out.write(if launch { UiSfx::play(keys::SPLASH_SCREEN) } else { UiSfx::stop(keys::SPLASH_SCREEN) });
        }
    }
}

fn play(
    mut st: ResMut<UiSfxState>,
    mut msgs: MessageReader<UiSfx>,
    scene: Res<UiSfxScene>,
    settings: Option<Res<Settings>>,
    time: Res<Time<Real>>,
) {
    let st = &mut *st;
    let Some(player) = st.player.clone() else {
        msgs.clear();
        return;
    };
    let Some(events) = st.events.clone() else {
        msgs.clear();
        return;
    };
    if let Some(s) = settings.as_deref() {
        let v = s.ui_volume.clamp(0.0, 1.0);
        if st.volume != v {
            st.volume = v;
            player.set_volume(v);
        }
    }
    let now = time.elapsed_secs_f64();
    let queued: Vec<UiSfx> = std::mem::take(&mut st.pending).into_iter().chain(msgs.read().copied()).collect();
    for m in queued {
        if m.stop {
            if let Some(id) = st.loops.remove(m.key) {
                player.stop(id, STOP_FADE_S);
            }
            continue;
        }
        if st.last.get(m.key).is_some_and(|&t| now - t < DEBOUNCE_S) {
            continue;
        }
        st.last.insert(m.key, now);
        match events.resolve(m.key, scene.0) {
            Resolved::Silent => {}
            Resolved::Sample { bank, index, looping, gain, pitch } => {
                if looping {
                    if let Some(&id) = st.loops.get(m.key) {
                        if player.is_playing(id) {
                            continue;
                        }
                    }
                }
                let clip = UiClip { bank, index };
                match player.play_with(&clip, gain, pitch, looping) {
                    Some(id) if looping => {
                        st.loops.insert(m.key, id);
                    }
                    Some(_) => {}
                    None => {
                        if st.warned.insert(format!("play:{}", m.key)) {
                            warn!("ui sfx: {} ({}:{}) did not start", m.key, clip.bank, clip.index);
                        }
                    }
                }
            }
            Resolved::Unresolved { cue } => {
                if st.warned.insert(format!("cue:{cue}")) {
                    warn!("ui sfx: {} -> cue \"{cue}\" has no sample (silent)", m.key);
                }
            }
        }
    }
}
