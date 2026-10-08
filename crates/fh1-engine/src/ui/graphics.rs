//! Options > Graphics (P8-A, 2026-10-08; docs/PERF.md "P8"): quality preset, anti-aliasing, render scale + CAS.
//!
//! - **Anti-aliasing** Off / FXAA / SMAA (default) / MSAA 4x. Before this the main camera had no `Msaa` component, so
//!   Bevy's default 4x MSAA rendered every main pass into 4x HDR targets plus a resolve. FXAA / SMAA go on the main
//!   camera (after tonemapping and the remaster grade). Every window camera (HUD, world map, render-scale blit) gets the
//!   same `Msaa`: Bevy keys the shared window texture on it, and a mismatch gives the HUD its own never-cleared texture
//!   that ghosts (memory bevy-camera-texture-sharing). `FH1_AA=old` = the old behaviour (MSAA 4x, no post AA, nothing
//!   touched); `FH1_AA=off|fxaa|smaa|msaa` overrides the setting. Under RTX nothing changes (rtx.rs owns AA: DLSS/TAA).
//! - **Render scale** 50-100 % (default 100 % = the old path, nothing changes). Below 100 % the main camera renders into
//!   an offscreen image of scale x window, and a Camera2d (order 1, layer [`BLIT_LAYER`]) draws it over the window with
//!   linear filtering and Bevy's contrast-adaptive sharpening (`FH1_CAS=0` off, `FH1_CAS_STRENGTH`, default 0.5). The
//!   blit camera copies the main camera's Hdr / usages, so the HUD (which copies them too) shares its texture.
//!   `FH1_RENDER_SCALE=0.5..1` overrides the setting.
//! - **Quality** Low / Medium / High (default) / Ultra fills [`fh1_render::quality::GraphicsQuality`], which the owners
//!   of shadows, the car probe, draw distance, particles and crowds read.
//! - **Occlusion culling** (opt-in `FH1_OCCLUSION=1`): Bevy's experimental two-phase GPU occlusion culling
//!   (`OcclusionCulling` + `DepthPrepass`) on the main camera. Only meshes drawn in the depth prepass occlude, and the
//!   remaster scenery skips the prepass by default (material.rs), so test it together with the scenery prepass.

use bevy::anti_alias::contrast_adaptive_sharpening::ContrastAdaptiveSharpening;
use bevy::anti_alias::fxaa::Fxaa;
use bevy::anti_alias::smaa::Smaa;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{CameraMainTextureUsages, Hdr, ImageRenderTarget, RenderTarget};
use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::image::ImageSampler;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureFormat};
use bevy::render::view::Msaa;
use bevy::window::{PrimaryWindow, WindowRef};
use fh1_render::post::FxPostCamera;
use fh1_render::quality::{GraphicsQuality, QualityPreset};
use serde::{Deserialize, Serialize};

use super::Settings;

/// Render layer of the render-scale blit sprite (free: UI 7, minimap 8, world map 9, probe 25-26, cube 27/29, mirror 28,
/// casters 30, empty 31).
pub const BLIT_LAYER: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Medium,
    #[default]
    High,
    Ultra,
}

impl Quality {
    const ALL: [Quality; 4] = [Quality::Low, Quality::Medium, Quality::High, Quality::Ultra];

    pub fn name(self) -> &'static str {
        match self {
            Quality::Low => "Low",
            Quality::Medium => "Medium",
            Quality::High => "High",
            Quality::Ultra => "Ultra",
        }
    }

    pub fn next(self, back: bool) -> Self {
        cycle(&Self::ALL, self, back)
    }

    fn preset(self) -> QualityPreset {
        match self {
            Quality::Low => QualityPreset::Low,
            Quality::Medium => QualityPreset::Medium,
            Quality::High => QualityPreset::High,
            Quality::Ultra => QualityPreset::Ultra,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AntiAlias {
    Off,
    Fxaa,
    #[default]
    Smaa,
    Msaa4,
}

impl AntiAlias {
    const ALL: [AntiAlias; 4] = [AntiAlias::Off, AntiAlias::Fxaa, AntiAlias::Smaa, AntiAlias::Msaa4];

    pub fn name(self) -> &'static str {
        match self {
            AntiAlias::Off => "Off",
            AntiAlias::Fxaa => "FXAA",
            AntiAlias::Smaa => "SMAA",
            AntiAlias::Msaa4 => "MSAA 4x",
        }
    }

    pub fn next(self, back: bool) -> Self {
        cycle(&Self::ALL, self, back)
    }
}

fn cycle<T: Copy + PartialEq>(all: &[T], cur: T, back: bool) -> T {
    let n = all.len();
    let i = all.iter().position(|&x| x == cur).unwrap_or(0);
    all[if back { (i + n - 1) % n } else { (i + 1) % n }]
}

/// settings.json `graphics`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphicsSettings {
    pub quality: Quality,
    pub aa: AntiAlias,
    /// Main-camera resolution as a fraction of the window, 0.5..=1.
    pub render_scale: f32,
}

impl Default for GraphicsSettings {
    fn default() -> Self {
        Self { quality: Quality::High, aa: AntiAlias::Smaa, render_scale: 1.0 }
    }
}

/// The renderer is Remaster + RTX (rtx.rs sets Msaa::Off everywhere and runs DLSS / TAA).
fn rtx() -> bool {
    fh1_remaster::enabled() && std::env::var("FH1_RTX").is_ok_and(|v| v == "1")
}

/// The AA in effect: None = `FH1_AA=old` (leave the cameras alone) or RTX.
fn aa_choice(g: &GraphicsSettings) -> Option<AntiAlias> {
    if rtx() {
        return None;
    }
    match std::env::var("FH1_AA").unwrap_or_default().to_ascii_lowercase().as_str() {
        "old" => None,
        "off" | "0" => Some(AntiAlias::Off),
        "fxaa" => Some(AntiAlias::Fxaa),
        "smaa" => Some(AntiAlias::Smaa),
        "msaa" | "msaa4" => Some(AntiAlias::Msaa4),
        _ => Some(g.aa),
    }
}

/// The render scale in effect (1 under RTX: DLSS owns the resolution there).
fn render_scale(g: &GraphicsSettings) -> f32 {
    if rtx() {
        return 1.0;
    }
    let s = std::env::var("FH1_RENDER_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(g.render_scale);
    if s.is_finite() { s.clamp(0.5, 1.0) } else { 1.0 }
}

/// Options value: the setting, plus what actually runs when an override or RTX differs.
pub fn aa_value(g: &GraphicsSettings) -> String {
    match aa_choice(g) {
        Some(a) if a == g.aa => a.name().into(),
        Some(a) => format!("{} ({} by FH1_AA)", g.aa.name(), a.name()),
        None if rtx() => format!("{} (RTX: DLSS)", g.aa.name()),
        None => format!("{} (FH1_AA=old)", g.aa.name()),
    }
}

pub fn scale_value(g: &GraphicsSettings) -> String {
    let set = format!("{:.0}%", g.render_scale * 100.0);
    let run = render_scale(g);
    if (run - g.render_scale).abs() < 1e-3 { set } else { format!("{set} (runs {:.0}%)", run * 100.0) }
}

/// Left / right: 5 % steps in 50..100 %; Enter wraps from 100 % to 50 %.
pub fn step_scale(g: &mut GraphicsSettings, dir: i32) {
    let steps = (g.render_scale * 20.0).round() as i32;
    let steps = match dir {
        0 if steps >= 20 => 10,
        0 => steps + 1,
        d if d < 0 => steps - 1,
        _ => steps + 1,
    };
    g.render_scale = steps.clamp(10, 20) as f32 / 20.0;
}

pub struct GraphicsPlugin;

impl Plugin for GraphicsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GraphicsQuality>()
            .init_resource::<Scaled>()
            .add_systems(Update, (sync_quality, sync_aa, sync_render_scale).chain());
        if std::env::var("FH1_OCCLUSION").is_ok_and(|v| v == "1") && !rtx() {
            app.add_systems(Update, occlusion_culling);
        }
    }
}

fn sync_quality(settings: Res<Settings>, mut quality: ResMut<GraphicsQuality>) {
    let preset = settings.graphics.quality.preset();
    if quality.preset != preset {
        *quality = GraphicsQuality::from_preset(preset);
        info!("graphics: quality {:?}", preset);
    }
}

/// Same Msaa on the main camera and every window camera; FXAA / SMAA on the main camera.
#[allow(clippy::type_complexity)]
fn sync_aa(mut commands: Commands, settings: Res<Settings>, mut cams: Query<(Entity, &RenderTarget, &mut Msaa, Has<FxPostCamera>, Has<BlitCamera>, Has<Fxaa>, Has<Smaa>)>) {
    let Some(aa) = aa_choice(&settings.graphics) else { return };
    let msaa = if aa == AntiAlias::Msaa4 { Msaa::Sample4 } else { Msaa::Off };
    for (e, target, mut m, main, blit, fxaa, smaa) in &mut cams {
        if !(main || blit || matches!(target, RenderTarget::Window(_))) {
            continue;
        }
        m.set_if_neq(msaa);
        if !main {
            continue;
        }
        match (aa == AntiAlias::Fxaa, fxaa) {
            (true, false) => {
                commands.entity(e).insert(Fxaa::default());
            }
            (false, true) => {
                commands.entity(e).remove::<Fxaa>();
            }
            _ => {}
        }
        match (aa == AntiAlias::Smaa, smaa) {
            (true, false) => {
                commands.entity(e).insert(Smaa::default());
            }
            (false, true) => {
                commands.entity(e).remove::<Smaa>();
            }
            _ => {}
        }
    }
}

/// The render-scale blit camera (window target, draws the main camera's offscreen image).
#[derive(Component)]
pub struct BlitCamera;

#[derive(Component)]
struct BlitSprite;

/// The main camera's offscreen image while the render scale is below 100 %.
#[derive(Resource, Default)]
struct Scaled(Option<Handle<Image>>);

fn cas() -> Option<ContrastAdaptiveSharpening> {
    if std::env::var("FH1_CAS").is_ok_and(|v| v == "0") {
        return None;
    }
    let strength = std::env::var("FH1_CAS_STRENGTH").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5f32);
    Some(ContrastAdaptiveSharpening { enabled: true, sharpening_strength: strength.clamp(0.0, 1.0), denoise: false })
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn sync_render_scale(
    mut commands: Commands,
    settings: Res<Settings>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut images: ResMut<Assets<Image>>,
    mut state: ResMut<Scaled>,
    mut main: Query<(&mut RenderTarget, Has<Hdr>, Option<&CameraMainTextureUsages>), (With<FxPostCamera>, Without<BlitCamera>)>,
    blit: Query<(Entity, Has<Hdr>, Option<&CameraMainTextureUsages>), With<BlitCamera>>,
    mut sprite: Query<(Entity, &mut Sprite), With<BlitSprite>>,
) {
    let Ok(window) = windows.single() else { return };
    let Ok((mut target, main_hdr, main_usages)) = main.single_mut() else { return };
    let scale = render_scale(&settings.graphics);
    if scale >= 0.999 {
        if state.0.take().is_some() {
            *target = RenderTarget::Window(WindowRef::Primary);
            for (e, ..) in &blit {
                commands.entity(e).despawn();
            }
            for (e, _) in &sprite {
                commands.entity(e).despawn();
            }
            info!("graphics: render scale 100% (main camera -> window)");
        }
        return;
    }
    let phys = window.physical_size();
    let size = UVec2::new(((phys.x as f32 * scale).round() as u32).max(1), ((phys.y as f32 * scale).round() as u32).max(1));
    let handle = match &state.0 {
        Some(h) => h.clone(),
        None => {
            let mut image = Image::new_target_texture(size.x, size.y, TextureFormat::Rgba8UnormSrgb, None);
            // Never uploaded: the main camera writes it every frame.
            image.data = None;
            image.sampler = ImageSampler::linear();
            let h = images.add(image);
            let mut cam = commands.spawn((
                BlitCamera,
                Camera2d,
                // After the main camera (0), before the HUD (10) and the world map (11).
                Camera { order: 1, ..default() },
                // The image is already graded and tonemapped.
                Tonemapping::None,
                DebandDither::Disabled,
                RenderLayers::layer(BLIT_LAYER),
            ));
            if let Some(c) = cas() {
                cam.insert(c);
            }
            commands.spawn((BlitSprite, Sprite { image: h.clone(), custom_size: Some(window.size()), ..default() }, Transform::default(), RenderLayers::layer(BLIT_LAYER)));
            state.0 = Some(h.clone());
            info!("graphics: render scale {:.0}% ({}x{})", scale * 100.0, size.x, size.y);
            h
        }
    };
    if images.get(&handle).is_some_and(|i| i.size() != size) {
        if let Some(mut i) = images.get_mut(&handle) {
            i.resize(Extent3d { width: size.x, height: size.y, depth_or_array_layers: 1 });
            i.data = None;
            i.copy_on_resize = false;
        }
    }
    // Logical size = the window's, so anything sizing UI from the main camera keeps its numbers.
    let scale_factor = window.scale_factor() * scale;
    let ok = matches!(&*target, RenderTarget::Image(t) if t.handle == handle && (t.scale_factor - scale_factor).abs() < 1e-4);
    if !ok {
        *target = RenderTarget::Image(ImageRenderTarget { handle: handle.clone(), scale_factor });
    }
    for (_, mut s) in &mut sprite {
        let want = Some(window.size());
        if s.custom_size != want {
            s.custom_size = want;
        }
    }
    // Same Hdr / usages as the main camera: the HUD copies the main camera's, and must share the blit camera's texture.
    let want_usages = main_usages.map(|u| u.0).unwrap_or_else(|| CameraMainTextureUsages::default().0);
    for (e, hdr, usages) in &blit {
        if hdr != main_hdr {
            if main_hdr {
                commands.entity(e).insert(Hdr);
            } else {
                commands.entity(e).remove::<Hdr>();
            }
        }
        if usages.map(|u| u.0).unwrap_or_else(|| CameraMainTextureUsages::default().0) != want_usages {
            commands.entity(e).insert(CameraMainTextureUsages(want_usages));
        }
    }
}

/// `FH1_OCCLUSION=1`: GPU occlusion culling on the main camera (needs its depth prepass).
fn occlusion_culling(mut commands: Commands, cams: Query<Entity, (With<FxPostCamera>, Without<bevy::render::occlusion_culling::OcclusionCulling>)>) {
    for e in &cams {
        commands.entity(e).insert((bevy::core_pipeline::prepass::DepthPrepass, bevy::render::occlusion_culling::OcclusionCulling));
        info!("graphics: GPU occlusion culling on the main camera");
    }
}
