//! FMV playback on the UI camera (data/extracted/plans/architecture.md §3a/§5/§6, fmv.md §3): one movie at a time,
//! decoded by `fh1-video` (two ffmpeg children: RGBA frames + f32 stereo PCM), drawn either full screen (boot splashes,
//! story movies; [`Target::FullScreen`], letterboxed above every other overlay) or as the launch cover's backdrop
//! (PressStart behind the title; [`Target::Backdrop`], read by ui/loading.rs through [`backdrop`]).
//!
//! - **Texture:** one persistent 1280x720 `Rgba8UnormSrgb` image (render world only). A new decoded frame is copied
//!   into its GPU texture by a render-world system (`RenderQueue::write_texture`, once per new frame at the movie's
//!   30 Hz), never through `Assets<Image>::get_mut` (that would re-create the texture every frame).
//! - **Audio:** the movie's track (the `.def` language id, else the first audio stream) plays on fh1_audio's pcm stream
//!   (Bus Fmv) at `ui_volume`, bypassing `world_audio_allowed()`. Its consumption is the A/V clock (fh1-video); no audio
//!   = wall clock. A device change kills the stream: the movie stops (plan §6).
//! - **Missing:** no `fmv` group installed, ffmpeg absent or without the WMV decoders (`fh1_video::probe`): every play
//!   request is refused with one log line and the game carries on.
//!
//! Movies are the original WMVs copied by the setup group `fmv` (fh1setup fmv.rs) to `<assets>/fmv/`; [`EXT`] is the
//! one place that names the container. Flags: `FH1_FMV=0` = no movies at all (boot splashes, title loop, story movies);
//! `FH1_FMV=<name>` (e.g. `PressStart`, `FMV_01`, `FMV_04`) = play that movie full screen at startup (dev /
//! screenshot hook; ui/intro.rs skips the boot queue).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{Extent3d, Origin3d, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect, TextureDimension, TextureFormat};
use bevy::render::renderer::RenderQueue;
use bevy::render::texture::GpuImage;
use bevy::render::{Render, RenderApp, RenderSystems};
use bevy::window::PrimaryWindow;

use super::Settings;
use crate::Garage;

/// Movie container on disk (setup copies the disc's WMVs unchanged; an mp4 transcode would change only this).
pub const EXT: &str = "wmv";
const W: u32 = 1280;
const H: u32 = 720;
/// Above the loading / launch overlay (1001).
const Z: i32 = 1002;

/// `FH1_FMV`: off, a dev movie name, or unset (on).
fn flag() -> &'static Option<String> {
    static F: OnceLock<Option<String>> = OnceLock::new();
    F.get_or_init(|| std::env::var("FH1_FMV").ok())
}

/// Movies on this run (`FH1_FMV=0` = none).
pub fn enabled() -> bool {
    !matches!(flag().as_deref(), Some("0") | Some("off"))
}

/// `FH1_FMV=<name>`: the dev movie to play at startup.
pub fn dev_movie() -> Option<&'static str> {
    flag().as_deref().filter(|f| !matches!(*f, "0" | "off" | "1" | "on" | ""))
}

/// Where the movie draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// Full screen, letterboxed on black, above every overlay (boot splashes, story movies).
    FullScreen,
    /// The launch cover's backdrop (ui/loading.rs), under the title and menus.
    Backdrop,
}

/// A full-screen movie is up (input / world audio gate, ui/loading.rs).
static FULLSCREEN: AtomicBool = AtomicBool::new(false);
/// The backdrop movie has a frame on its texture.
static BACKDROP: AtomicBool = AtomicBool::new(false);
/// The movie texture (set at startup).
static IMAGE: OnceLock<Handle<Image>> = OnceLock::new();
/// Newest decoded frame waiting for the render world.
static UPLOAD: Mutex<Option<(AssetId<Image>, Arc<Vec<u8>>)>> = Mutex::new(None);

/// A full-screen movie is playing.
pub fn fullscreen_active() -> bool {
    FULLSCREEN.load(Ordering::Relaxed)
}

/// The movie texture while a backdrop movie shows (ui/loading.rs `draw_overlay` puts it in the Backdrop node).
pub fn backdrop() -> Option<Handle<Image>> {
    BACKDROP.load(Ordering::Relaxed).then(|| IMAGE.get().cloned()).flatten()
}

/// `fh1_audio::pcm` source over the movie's PCM (interleaved stereo f32 at the device rate).
struct Pcm(fh1_video::AudioSource);

impl fh1_audio::pcm::PcmSource for Pcm {
    fn read(&self, out: &mut [f32]) -> usize {
        self.0.read(out)
    }
}

struct Playing {
    name: String,
    target: Target,
    video: fh1_video::FfmpegVideo,
    stream: Option<fh1_audio::pcm::StreamId>,
    last_seq: Option<u64>,
    frames: u32,
}

/// How a movie ended (ui/intro.rs reacts: next in the queue, restart the loop, ...).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ended {
    Finished,
    Stopped,
    Failed(String),
}

#[derive(Resource)]
pub struct Fmv {
    dir: PathBuf,
    current: Option<Playing>,
    /// Last movie that ended and how (taken by [`Fmv::take_ended`]).
    ended: Option<(String, Ended)>,
    /// Game language for the audio track (`FH1_UI_LANG`, as the UI strings).
    lang: String,
    /// `Some(reason)` = movies can't play this run.
    unavailable: Option<String>,
}

impl Fmv {
    /// Movies can be played (group installed, ffmpeg usable, not switched off).
    pub fn available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// `<assets>/fmv/<name>.wmv`.
    fn path(&self, name: &str) -> Option<PathBuf> {
        Some(self.dir.join(format!("{name}.{EXT}"))).filter(|p| p.is_file())
    }

    /// Start `name` (stops the current movie). False (and logged) when it can't play.
    pub fn play(&mut self, name: &str, target: Target, looping: bool, volume: f32) -> bool {
        self.stop();
        if let Some(why) = &self.unavailable {
            info!("fmv: {name} skipped ({why})");
            return false;
        }
        let Some(path) = self.path(name) else {
            info!("fmv: missing {name}, skipped");
            self.ended = Some((name.to_owned(), Ended::Failed("missing".into())));
            return false;
        };
        // Audio: the .def's track for the game language (FMV_0N), else the file's first audio stream (PressStart's
        // .def lists only its video but the file carries a stereo track; the splashes have no .def).
        let pcm = if crate::audio::audio_disabled() { None } else { fh1_audio::pcm::shared() };
        let audio = match pcm {
            None => fh1_video::AudioSel::None,
            Some(_) => fh1_video::AudioSel::choose(fh1_video::StreamDef::load(&path.with_extension("def")).ok().as_ref(), &self.lang),
        };
        let sample_rate = pcm.map_or(48_000, |p| p.device_rate());
        let opts = fh1_video::OpenOpts { audio, sample_rate, width: W, height: H, looping };
        let video = match fh1_video::FfmpegVideo::open(&path, opts) {
            Ok(v) => v,
            Err(e) => {
                warn!("fmv: {name}: {e}");
                self.ended = Some((name.to_owned(), Ended::Failed(e.to_string())));
                return false;
            }
        };
        // The movie must be pulled (its clock is the audio consumed), so a stream exists whenever audio was asked for.
        let stream = pcm.filter(|_| audio != fh1_video::AudioSel::None).map(|p| p.play_stream(Arc::new(Pcm(video.audio_source())), volume));
        info!("fmv: play {name} ({target:?}{}, audio {audio:?} at {sample_rate} Hz)", if looping { ", looped" } else { "" });
        self.current = Some(Playing { name: name.to_owned(), target, video, stream, last_seq: None, frames: 0 });
        FULLSCREEN.store(target == Target::FullScreen, Ordering::Relaxed);
        true
    }

    /// Stop the current movie (skip). Kills its ffmpeg children.
    pub fn stop(&mut self) {
        self.end(Ended::Stopped);
    }

    fn end(&mut self, how: Ended) {
        let Some(p) = self.current.take() else { return };
        if let (Some(id), Some(pcm)) = (p.stream, fh1_audio::pcm::shared()) {
            pcm.stop_stream(id);
        }
        match &how {
            Ended::Finished => info!("fmv: {} finished ({} frames)", p.name, p.frames),
            Ended::Stopped => info!("fmv: {} stopped at {:.1} s", p.name, p.video.clock()),
            Ended::Failed(e) => warn!("fmv: {} failed: {e}", p.name),
        }
        FULLSCREEN.store(false, Ordering::Relaxed);
        BACKDROP.store(false, Ordering::Relaxed);
        self.ended = Some((p.name, how));
        // Dropping `p.video` here kills both children.
    }

    /// The playing movie and its target.
    pub fn playing(&self) -> Option<(&str, Target)> {
        self.current.as_ref().map(|p| (p.name.as_str(), p.target))
    }

    /// How the last movie ended (once).
    pub fn take_ended(&mut self) -> Option<(String, Ended)> {
        self.ended.take()
    }
}

pub struct FmvPlugin;

impl Plugin for FmvPlugin {
    fn build(&self, app: &mut App) {
        let assets = app.world().get_resource::<Garage>().map(|g| g.assets.clone()).unwrap_or_default();
        let dir = assets.join("fmv");
        let unavailable = if !enabled() {
            Some("FH1_FMV=0".to_string())
        } else if !dir.is_dir() {
            Some(format!("{} not installed (setup group fmv)", dir.display()))
        } else {
            fh1_video::probe().err()
        };
        match &unavailable {
            Some(why) => info!("fmv: movies off ({why})"),
            None => info!("fmv: movies on ({})", dir.display()),
        }
        let lang = std::env::var("FH1_UI_LANG").unwrap_or_else(|_| "EN".into()).to_ascii_uppercase();
        let off = unavailable.is_some();
        app.insert_resource(Fmv { dir, current: None, ended: None, lang, unavailable });
        if off {
            return;
        }
        app.add_systems(Startup, spawn).add_systems(Update, update);
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.add_systems(Render, upload.in_set(RenderSystems::Prepare));
        }
    }
}

/// Full-screen movie layer: black root + the movie picture, letterboxed in `update`.
#[derive(Component)]
struct FmvRoot;

#[derive(Component)]
struct FmvPicture;

fn spawn(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let size = Extent3d { width: W, height: H, depth_or_array_layers: 1 };
    let image = images.add(Image::new_fill(size, TextureDimension::D2, &[0, 0, 0, 255], TextureFormat::Rgba8UnormSrgb, RenderAssetUsages::RENDER_WORLD));
    let _ = IMAGE.set(image.clone());
    // Open the audio stream now (behind the launch cover), not on the first play.
    if !crate::audio::audio_disabled() {
        info!("fmv: audio device rate {:?}", fh1_audio::pcm::device_rate());
    }
    let root = commands
        .spawn((
            FmvRoot,
            GlobalZIndex(Z),
            Visibility::Hidden,
            Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), overflow: Overflow::clip(), ..default() },
            BackgroundColor(Color::BLACK),
        ))
        .id();
    commands.spawn((
        FmvPicture,
        ChildOf(root),
        Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() },
        ImageNode { image, image_mode: NodeImageMode::Stretch, color: Color::NONE, ..default() },
    ));
}

#[allow(clippy::type_complexity)]
fn update(
    mut fmv: ResMut<Fmv>,
    settings: Res<Settings>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut root: Query<&mut Visibility, With<FmvRoot>>,
    mut picture: Query<(&mut ImageNode, &mut Node), With<FmvPicture>>,
) {
    let volume = settings.ui_volume.clamp(0.0, 1.0);
    let mut end = None;
    let mut shown = false;
    if let Some(p) = fmv.current.as_mut() {
        if let (Some(id), Some(pcm)) = (p.stream, fh1_audio::pcm::shared()) {
            if pcm.stream_alive(id) {
                pcm.set_stream_gain(id, volume);
            } else {
                end = Some(Ended::Failed("audio device changed".into()));
            }
        }
        if let Some(f) = p.video.frame() {
            if p.last_seq != Some(f.seq) {
                p.last_seq = Some(f.seq);
                p.frames += 1;
                if let (Some(img), Ok(mut slot)) = (IMAGE.get(), UPLOAD.lock()) {
                    *slot = Some((img.id(), f.data.clone()));
                }
            }
        }
        shown = p.frames > 0;
        if p.target == Target::Backdrop && shown {
            BACKDROP.store(true, Ordering::Relaxed);
        }
        if end.is_none() {
            if let Some(e) = p.video.error() {
                end = Some(Ended::Failed(e));
            } else if p.video.finished() {
                end = Some(Ended::Finished);
            }
        }
    }
    if let Some(how) = end {
        fmv.end(how);
        shown = false;
    }
    let full = fmv.current.as_ref().is_some_and(|p| p.target == Target::FullScreen);
    for mut v in &mut root {
        v.set_if_neq(if full { Visibility::Inherited } else { Visibility::Hidden });
    }
    if !full {
        return;
    }
    // Letterbox 16:9 into the window.
    let Ok(w) = windows.single() else { return };
    let (ww, wh) = (w.width().max(1.0), w.height().max(1.0));
    let s = (ww / W as f32).min(wh / H as f32);
    let (pw, ph) = (W as f32 * s / ww * 100.0, H as f32 * s / wh * 100.0);
    for (mut img, mut node) in &mut picture {
        let c = if shown { Color::WHITE } else { Color::NONE };
        if img.color != c {
            img.color = c;
        }
        node.width = Val::Percent(pw);
        node.height = Val::Percent(ph);
        node.left = Val::Percent((100.0 - pw) * 0.5);
        node.top = Val::Percent((100.0 - ph) * 0.5);
    }
}

/// Render world: copy the newest decoded frame into the movie texture.
fn upload(images: Res<RenderAssets<GpuImage>>, queue: Res<RenderQueue>) {
    let Some((id, data)) = UPLOAD.lock().ok().and_then(|mut s| s.take()) else { return };
    let Some(gpu) = images.get(id) else {
        // Not prepared yet: keep it for the next frame (unless a newer one arrived meanwhile).
        if let Ok(mut s) = UPLOAD.lock() {
            s.get_or_insert((id, data));
        }
        return;
    };
    if data.len() != (W * H * 4) as usize {
        return;
    }
    queue.write_texture(
        TexelCopyTextureInfo { texture: &gpu.texture, mip_level: 0, origin: Origin3d::ZERO, aspect: TextureAspect::All },
        &data,
        TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) },
        Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
}
