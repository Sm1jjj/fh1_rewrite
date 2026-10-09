//! Car photos for cars the game has none for (imported FH2 / FM4 cars; FH1's own are `ui/textures/thumbnails/
//! thumbnail_<Data_Car.Id>.png`, ui group). Rendered on demand, once per car per session, the way FH1 made its own:
//! the car's glTF in a hidden "studio" (its own render layer [`THUMB_LAYER`] and camera into a 768x288 image) from
//! ThumbnailCamSettings.xml's default 608x288 camera (VERIFIED values: pos (-3.895, 0.778, 4.744), face (0.662,
//! 0.052, -0.748), fov 28.67 deg, roll -0.58 deg; game car space, Z negated here like every gamedb / XML position).
//! One car at a time: its scene loads, renders for [`SETTLE_FRAMES`] frames (textures arriving), then is despawned and
//! the camera parked; the image keeps the last frame.
//!
//! GUESSES: the fov is vertical; ambient light only (a second DirectionalLight would be taken for the sun by the
//! lighting code), so paint reads flatter than the game's photos. The camera has a negative order, so systems that
//! look for the main view (`order >= 0`) skip it. `FH1_THUMB_RENDER=0` = no rendered photos (text only, old).

use std::collections::{HashMap, VecDeque};

use bevy::camera::visibility::RenderLayers;
use bevy::camera::RenderTarget;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::gltf::GltfAssetLabel;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use bevy::world_serialization::WorldAssetRoot;

/// Render layer of the studio (free: see ui/graphics.rs BLIT_LAYER's list; blit 24).
pub const THUMB_LAYER: usize = 23;
/// The game's thumbnail size (ui/textures/thumbnails are 768x288).
const SIZE: UVec2 = UVec2::new(768, 288);
const POS: Vec3 = Vec3::new(-3.895325, 0.778423, -4.743706);
/// Where the studio stands: far below the map, so a car shown for a frame before its layer is set is never in view.
const STUDIO: Vec3 = Vec3::new(0.0, -5000.0, 0.0);
const FACE: Vec3 = Vec3::new(0.661972, 0.051638, 0.747748);
const FOV_DEG: f32 = 28.666201;
const ROLL_DEG: f32 = -0.580674;
/// Frames rendered after the car's scene appears (its textures load meanwhile).
const SETTLE_FRAMES: u32 = 45;
/// Give up on a car whose scene never appears (missing model) after this many frames.
const LOAD_TIMEOUT_FRAMES: u32 = 600;

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_THUMB_RENDER").map_or(true, |v| v != "0"))
}

/// The studio: rendered photos by car (Garage::cars entry), the cars waiting, the car being shot.
#[derive(Resource, Default)]
pub struct ThumbStudio {
    cache: HashMap<String, Handle<Image>>,
    queue: VecDeque<(String, Handle<Image>)>,
    job: Option<Job>,
    camera: Option<Entity>,
}

struct Job {
    root: Entity,
    /// Frames since the job started, and since the scene's meshes appeared.
    frames: u32,
    shown: Option<u32>,
}

impl ThumbStudio {
    /// The photo of `car` (a `Garage::cars` entry): its handle at once (the image fills in once rendered).
    pub fn photo(&mut self, car: &str, images: &Assets<Image>) -> Option<Handle<Image>> {
        if !enabled() {
            return None;
        }
        if let Some(h) = self.cache.get(car) {
            return Some(h.clone());
        }
        // Browsing fast queues many cars: keep the last few, the dropped ones queue again when selected again.
        while self.queue.len() >= 3 {
            if let Some((old, _)) = self.queue.pop_front() {
                self.cache.remove(&old);
            }
        }
        let h = images.reserve_handle();
        self.cache.insert(car.to_owned(), h.clone());
        self.queue.push_back((car.to_owned(), h.clone()));
        Some(h)
    }

    /// The photo of `car` if it was already requested (rendered or still queued), without queueing it: card grids
    /// (ui/cards.rs) show these for every card and call [`ThumbStudio::photo`] only for the focused one, so a page of
    /// imported cars doesn't thrash the 3-deep queue.
    pub fn cached(&self, car: &str) -> Option<Handle<Image>> {
        if !enabled() {
            return None;
        }
        self.cache.get(car).cloned()
    }
}

pub struct ThumbPlugin;

impl Plugin for ThumbPlugin {
    fn build(&self, app: &mut App) {
        if enabled() {
            app.init_resource::<ThumbStudio>().add_systems(Update, run_studio);
        }
    }
}

#[allow(clippy::type_complexity)]
fn run_studio(
    mut commands: Commands,
    mut studio: ResMut<ThumbStudio>,
    mut images: ResMut<Assets<Image>>,
    asset_server: Res<AssetServer>,
    children: Query<&Children>,
    meshes: Query<(), (With<Mesh3d>, Without<RenderLayers>)>,
    has_mesh: Query<(), With<Mesh3d>>,
    mut cams: Query<(&mut Camera, &mut RenderTarget)>,
) {
    let studio = &mut *studio;
    // Start the next car.
    if studio.job.is_none() {
        let Some((car, handle)) = studio.queue.pop_front() else { return };
        let mut image = Image::new_target_texture(SIZE.x, SIZE.y, TextureFormat::Rgba8UnormSrgb, None);
        image.data = None;
        let _ = images.insert(&handle, image);
        let cam = *studio.camera.get_or_insert_with(|| {
            let up = Quat::from_axis_angle(FACE.normalize(), ROLL_DEG.to_radians()) * Vec3::Y;
            commands
                .spawn((
                    Camera3d::default(),
                    Camera { order: -50, is_active: false, clear_color: ClearColorConfig::Custom(Color::srgb(0.10, 0.11, 0.13)), ..default() },
                    RenderTarget::Image(handle.clone().into()),
                    Projection::Perspective(PerspectiveProjection { fov: FOV_DEG.to_radians(), aspect_ratio: SIZE.x as f32 / SIZE.y as f32, near: 0.1, far: 100.0, ..default() }),
                    Tonemapping::TonyMcMapface,
                    AmbientLight { color: Color::WHITE, brightness: 1500.0, ..default() },
                    Transform::from_translation(STUDIO + POS).looking_to(FACE, up),
                    RenderLayers::layer(THUMB_LAYER),
                    Msaa::Sample4,
                ))
                .id()
        });
        if let Ok((mut c, mut target)) = cams.get_mut(cam) {
            c.is_active = true;
            *target = RenderTarget::Image(handle.clone().into());
        }
        let scene = asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("cars/{car}/model.gltf")));
        let root = commands.spawn((WorldAssetRoot(scene), Transform::from_translation(STUDIO), Visibility::default(), RenderLayers::layer(THUMB_LAYER))).id();
        studio.job = Some(Job { root, frames: 0, shown: None });
        return;
    }
    let job = studio.job.as_mut().expect("checked above");
    job.frames += 1;
    // The scene's entities inherit no render layer: put every mesh on the studio's.
    let mut any = false;
    for e in children.iter_descendants(job.root) {
        if has_mesh.contains(e) {
            any = true;
            if meshes.contains(e) {
                commands.entity(e).insert(RenderLayers::layer(THUMB_LAYER));
            }
        }
    }
    if any && job.shown.is_none() {
        job.shown = Some(job.frames);
    }
    let done = match job.shown {
        Some(at) => job.frames - at >= SETTLE_FRAMES,
        None => job.frames >= LOAD_TIMEOUT_FRAMES,
    };
    if done {
        commands.entity(job.root).despawn();
        studio.job = None;
        if let Some((mut c, _)) = studio.camera.and_then(|e| cams.get_mut(e).ok()) {
            c.is_active = false;
        }
    }
}
