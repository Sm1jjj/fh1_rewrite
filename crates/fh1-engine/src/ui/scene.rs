//! Draws FH1's own UI scenes (Anark Gameface, `fh1_ui::player`) with a dedicated orthographic UI
//! camera on its own render layer, so the 3D camera's post-processing never touches the UI.
//!
//! Each [`AnarkScene`] entity owns a [`Player`]. Every frame its draw list becomes a set of mesh
//! entities (one per model sub-mesh, two per text: inner + outer glyph meshes) whose world matrix
//! is written straight into `GlobalTransform` (scene matrices can shear, which `Transform` can't
//! hold). Draw order = the player's back-to-front order, carried in each material's depth bias
//! (Bevy retains transparent phase items and only re-sorts them when re-queued, so a moving
//! transform alone would keep a stale order).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bevy::camera::visibility::{NoFrustumCulling, RenderLayers};
use bevy::camera::{ClearColorConfig, ScalingMode};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::math::{Affine3A, Vec3A};
use bevy::prelude::*;
use bevy::render::view::Msaa;
use bevy::transform::TransformSystems;
use fh1_ui::anark::Scene;
use fh1_ui::player::{DrawKind, DrawText, Frame, Player};
use fh1_ui::strtable::StringTables;
use fh1_ui::vfont::{Font as VFont, FontMap};

use super::materials::{AnarkMaterial, AnarkUniform, VectorTextMaterial};

/// Render layer of everything the UI camera draws.
pub const UI_LAYER: usize = 7;

/// Anark text sizes look like points: one em on screen is `size × 96/72` UI pixels (INFERRED from the
/// Xenia free-roam frames: the horizon_e speedo digit measures 28 px against 21 px for size-as-pixels).
/// Applied to the driving HUD only (`AnarkScene::text_scale`); the menus keep size-as-pixels by the
/// user's choice (menus stay as they are; their SUPER_STACKER tapes don't fit bigger text yet).
pub const TEXT_PT_TO_PX: f32 = 96.0 / 72.0;
/// FH1 authors its UI at 1280×720 (bgf header).
pub const UI_SIZE: Vec2 = Vec2::new(1280.0, 720.0);

pub struct AnarkPlugin;

impl Plugin for AnarkPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_ui_camera)
            .add_systems(Update, (tick_scenes, sync_hdr))
            .add_systems(PostUpdate, draw_scenes.after(TransformSystems::Propagate));
    }
}

#[derive(Component)]
pub struct UiCamera;

/// The HUD camera is a `Camera2d` and the parts are `Mesh2d` (default; perf W4: a Camera3d overlay paid Bevy's 3D
/// per-view preprocessing / shadow / cluster setup, ~1.15 ms of render thread with nothing 3D to draw). Same
/// materials, shaders, blend states and draw order (2D sort key = z + depth bias). `FH1_HUD_3D=1` = the old Camera3d.
pub fn hud_2d() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_HUD_3D").as_deref() != Ok("1"))
}

/// Spawns a HUD part on the UI layer: Mesh2d / MeshMaterial2d with [`hud_2d`], else Mesh3d / MeshMaterial3d.
fn spawn_part<M: Material + bevy::sprite_render::Material2d>(commands: &mut Commands, mesh: Option<Handle<Mesh>>, material: Handle<M>) -> Entity {
    let mut e = commands.spawn((RenderLayers::layer(UI_LAYER), NoFrustumCulling, Transform::default()));
    match (hud_2d(), mesh) {
        (true, Some(m)) => e.insert((Mesh2d(m), bevy::sprite_render::MeshMaterial2d(material))),
        (true, None) => e.insert(bevy::sprite_render::MeshMaterial2d(material)),
        (false, Some(m)) => e.insert((Mesh3d(m), MeshMaterial3d(material))),
        (false, None) => e.insert(MeshMaterial3d(material)),
    };
    e.id()
}

/// Sets a part's mesh (text parts get theirs after spawning).
fn set_part_mesh(commands: &mut Commands, e: Entity, mesh: Handle<Mesh>) {
    if hud_2d() {
        commands.entity(e).insert(Mesh2d(mesh));
    } else {
        commands.entity(e).insert(Mesh3d(mesh));
    }
}

/// Fonts, strings and file paths of the converted `ui` group.
#[derive(Resource)]
pub struct UiData {
    /// `<assets>/ui` on disk.
    pub dir: PathBuf,
    /// String-table language (EN, DE, …).
    pub lang: String,
    map: FontMap,
    fonts: HashMap<String, VFont>,
    pub strings: Option<StringTables>,
    /// Textures the 360 samples without gamma decoding (`textures/no_gamma.txt`).
    no_gamma: HashSet<String>,
}

impl UiData {
    pub fn load(assets: &std::path::Path, lang: &str) -> Option<Self> {
        let dir = assets.join("ui");
        let map = FontMap::parse(&std::fs::read_to_string(dir.join("fonts/fontmap.xml")).ok()?);
        let mut fonts = HashMap::new();
        for e in std::fs::read_dir(dir.join("fonts")).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if let Some(stem) = name.strip_suffix("_vector_aa.dt") {
                match std::fs::read(e.path()).map_err(|e| e.to_string()).and_then(|b| VFont::parse(&b).map_err(|e| e.to_string())) {
                    Ok(f) => {
                        fonts.insert(stem.to_owned(), f);
                    }
                    Err(err) => warn!("ui font {name}: {err}"),
                }
            }
        }
        let strings = StringTables::load_zip(dir.join(format!("strings/{lang}.zip"))).map_err(|e| warn!("ui strings {lang}: {e}")).ok();
        let no_gamma = std::fs::read_to_string(dir.join("textures/no_gamma.txt")).unwrap_or_default().lines().map(str::to_owned).collect();
        Some(Self { dir, lang: lang.to_owned(), map, fonts, strings, no_gamma })
    }

    /// The vector font for a scene font name (`horizon_e`, `e_helvetica…` → E).
    pub fn font(&self, name: &str) -> Option<&VFont> {
        let target = self.map.resolve(name)?;
        self.fonts.get(&target.to_lowercase())
    }

    /// Load a scene by name (`947_HUD`).
    pub fn scene(&self, name: &str) -> Option<Player> {
        let base = self.dir.join("scenes").join(name.to_lowercase());
        let read = |ext: &str| std::fs::read(base.with_extension(ext)).ok();
        let bgf = read("bgf")?;
        match Scene::load(&bgf, read("fbf").as_deref(), read("bsg").as_deref()) {
            Ok(s) => Some(Player::new(s)),
            Err(e) => {
                warn!("ui scene {name}: {e}");
                None
            }
        }
    }
}

/// One FH1 UI scene on screen.
#[derive(Component)]
pub struct AnarkScene {
    pub player: Player,
    pub visible: bool,
    /// Draw order between scenes (higher = in front).
    pub order: i32,
    /// Run the scene clock (UI animates in real time, also while the game is paused).
    pub playing: bool,
    /// Texture paths (as `DrawTexture::path`) the game supplies itself, e.g. the minimap render
    /// target for `horizon/placeholder.png`.
    pub texture_overrides: HashMap<String, Handle<Image>>,
    /// Text em in UI pixels per point of `size` (1.0 = size-as-pixels; the HUD uses `TEXT_PT_TO_PX`).
    pub text_scale: f32,
    cache: Cache,
    /// UI fast path: the inputs of the last draw (player revision, visible, overrides, text scale, first z) and the
    /// z range it used. An unchanged key skips evaluate + the part loop entirely.
    drawn: Option<(DrawKey, f32)>,
}

#[derive(Clone, Copy, PartialEq)]
struct DrawKey {
    revision: u64,
    visible: bool,
    overrides: usize,
    text_scale: u32,
    z0: u32,
}

/// `FH1_UI_FASTPATH=0`: evaluate and place every scene every frame (the old path).
pub fn fastpath() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_UI_FASTPATH").map_or(true, |v| v != "0"))
}

impl AnarkScene {
    pub fn new(player: Player, order: i32) -> Self {
        Self { player, visible: true, order, playing: true, texture_overrides: HashMap::new(), text_scale: 1.0, cache: Cache::default(), drawn: None }
    }
}

#[derive(Default)]
struct Cache {
    /// (fbf mesh, index group) → front-face mesh.
    meshes: HashMap<(usize, usize), Option<Handle<Mesh>>>,
    parts: HashMap<PartKey, Part>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum PartKey {
    Model(usize, usize),
    TextInner(usize),
    TextOuter(usize),
}

struct Part {
    entity: Entity,
    kind: PartMaterial,
    /// Text parts: the layout key (hash) the mesh was built for.
    text_key: Option<u64>,
}

enum PartMaterial {
    Anark(Handle<AnarkMaterial>),
    Text(Handle<VectorTextMaterial>),
}

/// `FH1_HUD_ADDITIVE_ALPHA=0`: additive UI parts add their raw colour (the old, solid-quad look).
fn additive_alpha() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_HUD_ADDITIVE_ALPHA").map_or(true, |v| v != "0"))
}

/// The window overlay (HUD) camera, as opposed to the minimap / world map cameras that are `UiCamera`s too.
#[derive(Component)]
pub struct HudCamera;

fn spawn_ui_camera(commands: Commands) {
    spawn_hud_camera(commands, None);
}

/// `look` = (Msaa, Hdr, main texture usages) copied from the main camera (a respawn, [`sync_hdr`]); None = defaults.
fn spawn_hud_camera(mut commands: Commands, look: Option<(Msaa, bool, bevy::camera::CameraMainTextureUsages)>) {
    if hud_2d() {
        let mut e = commands.spawn((
            UiCamera,
            HudCamera,
            // Bevy UI (placeholder menus, telemetry) draws through this window camera, never into
            // an offscreen one such as the minimap's.
            bevy::ui::IsDefaultUiCamera,
            Camera2d,
            Camera { order: 10, clear_color: ClearColorConfig::None, ..default() },
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::AutoMin { min_width: UI_SIZE.x, min_height: UI_SIZE.y },
                ..OrthographicProjection::default_2d()
            }),
            Tonemapping::None,
            // As the Camera3d it replaces (Camera3d requires DebandDither::Enabled).
            bevy::core_pipeline::tonemapping::DebandDither::Enabled,
            Transform::default(),
            RenderLayers::layer(UI_LAYER),
        ));
        if let Some((msaa, hdr, usages)) = look {
            e.insert((msaa, usages));
            if hdr {
                e.insert(bevy::camera::Hdr);
            }
        }
        return;
    }
    commands.spawn((
        UiCamera,
        HudCamera,
        // Bevy UI (placeholder menus, telemetry) draws through this window camera, never into
        // an offscreen one such as the minimap's.
        bevy::ui::IsDefaultUiCamera,
        Camera3d::default(),
        Camera { order: 10, clear_color: ClearColorConfig::None, ..default() },
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::AutoMin { min_width: UI_SIZE.x, min_height: UI_SIZE.y },
            near: 0.0,
            far: 4000.0,
            ..OrthographicProjection::default_3d()
        }),
        Tonemapping::None,
        Transform::from_xyz(0.0, 0.0, 2000.0).looking_at(Vec3::ZERO, Vec3::Y),
        RenderLayers::layer(UI_LAYER),
    ));
}

/// The UI camera must share the main camera's main texture to composite over it: same `Hdr`
/// (fh1-render's post chain makes the main camera HDR), the same Msaa (both default) and the same
/// `CameraMainTextureUsages` (bevy_render 0.19 prepare_view_targets keys the shared texture on target, usages, format
/// and Msaa). Without a match Bevy gives the UI camera its own texture. With `ClearColorConfig::None` that texture is
/// never cleared, and the upscale pass alpha-blends it over the window every frame: under RTX (rtx.rs adds
/// STORAGE_BINDING to the main camera's usages for Solarik) moving text (telemetry, F3 overlay) ghosted over its old
/// frames. `FH1_HUD_SHARE_USAGES=0` = the old sync (Hdr only).
///
/// Msaa (Options > Anti-aliasing) is matched by RESPAWNING the 2D HUD camera (2026-10-09): changing a live Camera2d's
/// Msaa left a mesh2d pipeline specialized for the old sample count on HUD parts shown again after the pause menu
/// (wgpu "Incompatible sample count" validation error = crash). A new camera starts with fresh specializations and
/// phases. `FH1_HUD_RESPAWN=0` = mutate in place (old).
#[allow(clippy::type_complexity)]
fn sync_hdr(
    mut commands: Commands,
    main: Query<(Has<bevy::camera::Hdr>, Option<&bevy::camera::CameraMainTextureUsages>, &Msaa), With<fh1_render::post::FxPostCamera>>,
    mut ui: Query<(Entity, Has<bevy::camera::Hdr>, Option<&bevy::camera::CameraMainTextureUsages>, &RenderLayers, Option<&mut Msaa>, Has<HudCamera>), (With<UiCamera>, Without<fh1_render::post::FxPostCamera>)>,
) {
    let Ok((main_hdr, main_usages, main_msaa)) = main.single() else { return };
    let share_usages = std::env::var("FH1_HUD_SHARE_USAGES").map_or(true, |v| v != "0");
    let respawn = hud_2d() && std::env::var("FH1_HUD_RESPAWN").map_or(true, |v| v != "0");
    let main_usages_v = main_usages.map(|u| u.0).unwrap_or_else(|| bevy::camera::CameraMainTextureUsages::default().0);
    for (e, hdr, usages, layers, msaa, hud) in &mut ui {
        if hud && respawn {
            let have_usages = usages.map(|u| u.0).unwrap_or_else(|| bevy::camera::CameraMainTextureUsages::default().0);
            let msaa_ok = msaa.as_deref().is_some_and(|m| m == main_msaa);
            if !msaa_ok || hdr != main_hdr || (share_usages && have_usages != main_usages_v) {
                commands.entity(e).despawn();
                let usages = if share_usages { main_usages_v } else { have_usages };
                spawn_hud_camera(commands.reborrow(), Some((*main_msaa, main_hdr, bevy::camera::CameraMainTextureUsages(usages))));
                info!("ui: HUD camera respawned (Msaa {:?}, Hdr {main_hdr})", main_msaa);
                // One camera only: the next frame sees the new one.
                return;
            }
            continue;
        }
        // The Camera3d HUD (FH1_HUD_3D=1) and FH1_HUD_RESPAWN=0: match in place (graphics.rs sync_aa skips HudCamera).
        if let Some(mut m) = msaa.filter(|_| hud) {
            m.set_if_neq(*main_msaa);
        }
        // Only the window overlay camera; the minimap renders into its own image.
        if !layers.intersects(&RenderLayers::layer(UI_LAYER)) {
            continue;
        }
        if hdr != main_hdr {
            if main_hdr {
                commands.entity(e).insert(bevy::camera::Hdr);
            } else {
                commands.entity(e).remove::<bevy::camera::Hdr>();
            }
        }
        if share_usages {
            let want = main_usages.map(|u| u.0).unwrap_or_else(|| bevy::camera::CameraMainTextureUsages::default().0);
            let have = usages.map(|u| u.0).unwrap_or_else(|| bevy::camera::CameraMainTextureUsages::default().0);
            if want != have {
                commands.entity(e).insert(bevy::camera::CameraMainTextureUsages(want));
            }
        }
    }
}

fn tick_scenes(mut scenes: Query<&mut AnarkScene>, time: Res<Time<Real>>) {
    let dt = time.delta_secs() * 1000.0;
    for mut s in &mut scenes {
        // Hidden scenes don't animate (fast path): their next SHOW / slide change restarts what they play.
        if s.playing && (s.visible || !fastpath()) {
            s.player.update(dt);
        }
    }
}

/// Row-major Anark matrix → Bevy affine in UI-camera space (depth flattened to 0).
fn to_affine(m: &[[f32; 4]; 4], cam: [f32; 3]) -> Affine3A {
    Affine3A::from_cols(
        Vec3A::new(m[0][0], m[1][0], 0.0),
        Vec3A::new(m[0][1], m[1][1], 0.0),
        // Mesh z still moves x/y: some quads are modelled in the X–Z plane and turned to face
        // the camera with rotation.x (the tach redline). Depth itself is flattened (tiny scale
        // keeps the matrix invertible).
        Vec3A::new(m[0][2], m[1][2], 1e-4),
        // Every part sits at z = 0; draw order is the material's depth bias.
        Vec3A::new(m[0][3] - cam[0], m[1][3] - cam[1], 0.0),
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_scenes(
    mut commands: Commands,
    mut scenes: Query<&mut AnarkScene>,
    data: Option<Res<UiData>>,
    assets: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut anark_mats: ResMut<Assets<AnarkMaterial>>,
    mut text_mats: ResMut<Assets<VectorTextMaterial>>,
    mut parts_q: Query<(&mut GlobalTransform, &mut Transform, &mut Visibility)>,
    mut missing: Local<HashSet<String>>,
    time: Res<Time<Real>>,
    mut tex_cache: Local<HashMap<String, Vec<([u32; 2], Option<Handle<Image>>)>>>,
    mut seen: Local<HashSet<PartKey>>,
) {
    let Some(data) = data else { return };
    let mut list: Vec<_> = scenes.iter_mut().collect();
    list.sort_by_key(|s| s.order);
    let mut z = 0.0f32;
    // `FH1_UI_DEBUG=<s>`: log the placed parts once at that time.
    let dbg = std::env::var("FH1_UI_DEBUG").ok().and_then(|v| v.parse::<f32>().ok()).is_some_and(|at| {
        let t = time.elapsed_secs();
        t >= at && t - time.delta_secs() < at
    });
    for mut scene in list {
        // Fast path: nothing that feeds this scene's draw changed (and it starts at the same z): keep last frame's parts.
        let key = DrawKey {
            revision: scene.player.revision(),
            visible: scene.visible,
            overrides: scene.texture_overrides.len(),
            text_scale: scene.text_scale.to_bits(),
            z0: z.to_bits(),
        };
        if fastpath() && !dbg {
            if let Some((k, span)) = scene.drawn {
                if k == key {
                    z += span;
                    continue;
                }
            }
        }
        let z_start = z;
        let scene = &mut *scene;
        let frame = if scene.visible { scene.player.evaluate() } else { Frame::default() };
        seen.clear();
        for d in &frame.draws {
            let cam = frame.camera(d.layer).position;
            match &d.kind {
                DrawKind::Model { mesh, materials } => {
                    for m in materials {
                        let key = PartKey::Model(d.node, m.submesh);
                        let Some(handle) = scene.cache.meshes.entry((*mesh, m.submesh)).or_insert_with(|| model_mesh(&scene.player, *mesh, m.submesh).map(|me| meshes.add(me))).clone() else {
                            continue;
                        };
                        let alpha = d.opacity * m.opacity;
                        if alpha <= 0.001 {
                            continue;
                        }
                        let mut u = AnarkUniform { color: Vec4::new(m.diffuse[0], m.diffuse[1], m.diffuse[2], alpha), ..default() };
                        let mut tex: [Option<Handle<Image>>; 4] = default();
                        let mut n = 0;
                        for t in &m.textures {
                            if n == 4 {
                                break;
                            }
                            if let Some(h) = scene.texture_overrides.get(&t.path) {
                                // Game-supplied targets (minimap) hold gamma-space values.
                                u.flags.z |= 1 << n;
                                tex[n] = Some(h.clone());
                                u.uv_x[n] = Vec4::new(t.uv[0], t.uv[1], t.uv[2], 0.0);
                                u.uv_y[n] = Vec4::new(t.uv[3], t.uv[4], t.uv[5], 0.0);
                                n += 1;
                                continue;
                            }
                            // Cached per (path, tiling) (perf P2): the file check and the AssetServer path load ran per
                            // part and frame. The cache keeps the UI textures resident (they were held by materials anyway).
                            if !tex_cache.contains_key(t.path.as_str()) {
                                tex_cache.insert(t.path.clone(), Vec::new());
                            }
                            let slot = tex_cache.get_mut(t.path.as_str()).expect("inserted above");
                            let handle = match slot.iter().find(|(k, _)| *k == t.tiling) {
                                Some((_, h)) => h.clone(),
                                None => {
                                    let rel = format!("ui/textures/{}", t.path);
                                    let h = data.dir.join("textures").join(&t.path).is_file().then(|| load_texture(&assets, rel, t.tiling));
                                    slot.push((t.tiling, h.clone()));
                                    h
                                }
                            };
                            let Some(handle) = handle else {
                                if missing.insert(t.path.clone()) {
                                    debug!("ui texture missing on the disc: {}", t.path);
                                }
                                continue;
                            };
                            tex[n] = Some(handle);
                            if !data.no_gamma.contains(&t.path) {
                                u.flags.z |= 1 << n;
                            }
                            u.uv_x[n] = Vec4::new(t.uv[0], t.uv[1], t.uv[2], 0.0);
                            u.uv_y[n] = Vec4::new(t.uv[3], t.uv[4], t.uv[5], 0.0);
                            n += 1;
                        }
                        u.flags.x = n as u32;
                        // Additive parts weight their colour by alpha (Xenos SRCALPHA/ONE): flash textures are white RGB
                        // with the shape in alpha only, and unweighted they drew as solid quads (e4, 2026-10-08).
                        u.flags.w = (m.additive && additive_alpha()) as u32;
                        let [tex0, tex1, tex2, tex3] = tex;
                        let mat = AnarkMaterial { u, tex0, tex1, tex2, tex3, additive: m.additive, order: z };
                        let part = scene.cache.parts.entry(key).or_insert_with(|| {
                            let h = anark_mats.add(mat.clone());
                            let e = spawn_part(&mut commands, Some(handle.clone()), h.clone());
                            Part { entity: e, kind: PartMaterial::Anark(h), text_key: None }
                        });
                        if let PartMaterial::Anark(h) = &part.kind {
                            if let Some(cur) = anark_mats.get(h) {
                                if !same_anark(cur, &mat) {
                                    if let Some(mut cur) = anark_mats.get_mut(h) {
                                        *cur = mat;
                                    }
                                }
                            }
                        }
                        place(&mut parts_q, part.entity, to_affine(&d.world, cam));
                        if dbg {
                            info!("uidbg M {} sub{} z{z:.2} y{:.1} e{:?}", d.name, m.submesh, d.world[1][3], part.entity);
                        }
                        seen.insert(key);
                        z += 0.05;
                    }
                }
                DrawKind::Text(t) => {
                    let Some(font) = data.font(&t.font) else { continue };
                    let string = display_string(t, data.strings.as_ref());
                    if string.is_empty() {
                        continue;
                    }
                    let layout_key = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        (&string, &t.font, t.horzalign, t.vertalign, t.tracking.to_bits()).hash(&mut h);
                        h.finish()
                    };
                    let colour = Vec4::new(t.color[0], t.color[1], t.color[2], t.color[3] * d.opacity);
                    for (key, outer) in [(PartKey::TextInner(d.node), false), (PartKey::TextOuter(d.node), true)] {
                        let part = scene.cache.parts.entry(key).or_insert_with(|| {
                            let h = text_mats.add(VectorTextMaterial::new(colour, None, outer));
                            let e = spawn_part(&mut commands, None, h.clone());
                            Part { entity: e, kind: PartMaterial::Text(h), text_key: None }
                        });
                        if part.text_key != Some(layout_key) {
                            let mesh = text_mesh(font, &string, t, outer);
                            set_part_mesh(&mut commands, part.entity, meshes.add(mesh));
                            part.text_key = Some(layout_key);
                        }
                        if let PartMaterial::Text(h) = &part.kind {
                            if text_mats.get(h).map(|m| (m.u.text_colour, m.order)) != Some((colour, z)) {
                                if let Some(mut m) = text_mats.get_mut(h) {
                                    m.u.text_colour = colour;
                                    m.u.outline_colour = colour;
                                    m.order = z;
                                }
                            }
                        }
                        // Text meshes are in em: scale by the size in pixels (see TEXT_PT_TO_PX).
                        let px = t.size * scene.text_scale;
                        let affine = to_affine(&d.world, cam) * Affine3A::from_scale(Vec3::new(px, px, 1.0));
                        place(&mut parts_q, part.entity, affine);
                        if dbg {
                            info!("uidbg T {} '{}' z{z:.2} y{:.1} e{:?}", d.name, string, d.world[1][3], part.entity);
                        }
                        seen.insert(key);
                    }
                    z += 0.05;
                }
            }
        }
        for (key, part) in &scene.cache.parts {
            if !seen.contains(key) {
                if let Ok((_, _, mut v)) = parts_q.get_mut(part.entity) {
                    v.set_if_neq(Visibility::Hidden);
                }
            }
        }
        scene.drawn = Some((key, z - z_start));
    }
}

fn place(q: &mut Query<(&mut GlobalTransform, &mut Transform, &mut Visibility)>, e: Entity, a: Affine3A) {
    if let Ok((mut g, mut t, mut v)) = q.get_mut(e) {
        // Only on a real change (fast path): an unconditional write re-extracted every UI mesh every frame.
        if fastpath() {
            g.set_if_neq(GlobalTransform::from(a));
        } else {
            *g = GlobalTransform::from(a);
        }
        // Keep Transform's translation in step too: with it left at the origin, reused parts
        // were drawn in spawn order instead of by z (stale transparent sorting).
        let p = Vec3::from(a.translation);
        if t.translation != p {
            t.translation = p;
        }
        v.set_if_neq(Visibility::Visible);
    }
}

fn same_anark(a: &AnarkMaterial, b: &AnarkMaterial) -> bool {
    a.u.color == b.u.color && a.u.uv_x == b.u.uv_x && a.u.uv_y == b.u.uv_y && a.u.flags == b.u.flags && a.tex0 == b.tex0 && a.tex1 == b.tex1 && a.tex2 == b.tex2 && a.tex3 == b.tex3 && a.additive == b.additive && a.order == b.order
}

fn load_texture(assets: &AssetServer, path: String, tiling: [u32; 2]) -> Handle<Image> {
    // Anark tiling modes are the Xenos fetch-constant clamp values: 0 wrap, 1 mirror, 2 clamp to
    // the last texel (INFERRED). Mode 2 is what makes the offset two-slot masks work: the HUD pills
    // and pause tapes multiply a strip by a U-shifted copy of itself, and the strips' transparent
    // 3-texel border ends the bar at u = 1 − offset (the objective bar in the Xenia frames ends right
    // after its text; wrapping drew a second bar). The tach redline (mode 2, rotated UVs) needs the
    // clamp too. Other values (3..7: mirror-once / half-way / border) are clamped (GUESS).
    let mode = |t: u32| match t {
        0 => ImageAddressMode::Repeat,
        1 => ImageAddressMode::MirrorRepeat,
        _ => ImageAddressMode::ClampToEdge,
    };
    let (mu, mv) = (mode(tiling[0]), mode(tiling[1]));
    assets
        .load_builder()
        .with_settings(move |s: &mut ImageLoaderSettings| {
            // FH1 samples UI textures raw and gamma-encodes the result (materials.rs GAMMA2).
            s.is_srgb = !super::materials::GAMMA2;
            s.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor { address_mode_u: mu, address_mode_v: mv, ..ImageSamplerDescriptor::linear() });
        })
        .load(path)
}

/// The text to show: the scene's string, or the localised one for its `loc_key`.
fn display_string(t: &DrawText, strings: Option<&StringTables>) -> String {
    if t.string.is_empty() && !t.loc_key.is_empty() {
        if let Some(s) = strings.and_then(|st| st.resolve(&t.loc_key)) {
            return fh1_ui::strtable::strip_markup(s);
        }
    }
    fh1_ui::strtable::strip_markup(&t.string)
}

/// Front faces of one index group of a scene mesh (the meshes are double sided: each surface
/// is stored twice, drawn once).
fn model_mesh(player: &Player, mesh: usize, group: usize) -> Option<Mesh> {
    let m = player.scene.fbf.as_ref()?.meshes.get(mesh)?;
    let idx = m.groups.get(group)?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tri in idx.chunks_exact(3) {
        let mut key: Vec<[i32; 3]> = tri
            .iter()
            .filter_map(|&i| m.vertices.get(i as usize))
            .map(|v| [(v.pos[0] * 1000.0).round() as i32, (v.pos[1] * 1000.0).round() as i32, (v.pos[2] * 1000.0).round() as i32])
            .collect();
        if key.len() < 3 {
            continue;
        }
        key.sort();
        if seen.insert(key) {
            out.extend_from_slice(tri);
        }
    }
    if out.is_empty() {
        return None;
    }
    let pos: Vec<[f32; 3]> = m.vertices.iter().map(|v| v.pos).collect();
    let nrm: Vec<[f32; 3]> = m.vertices.iter().map(|v| v.normal).collect();
    let uv: Vec<[f32; 2]> = m.vertices.iter().map(|v| v.uv).collect();
    Some(
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, nrm)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uv)
            .with_inserted_indices(Indices::U16(out)),
    )
}

/// Glyph meshes for a string, in em (1.0 = the text's point size). `outer` picks the mirrored
/// concave/fringe mesh (drawn with sign −1). Alignment: horzalign 0/1/2 = left/centre/right of
/// the node origin; vertalign 1 = caps centred on it (GUESS), 0 = top, 2 = bottom.
fn text_mesh(font: &VFont, s: &str, t: &DrawText, outer: bool) -> Mesh {
    let mut pos = Vec::new();
    let mut uv = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    let track = t.tracking / t.size.max(1.0);
    let mut pen = 0.0f32;
    for c in s.chars() {
        if c == ' ' {
            pen += font.metrics.space_width + track;
            continue;
        }
        let Some(g) = font.glyph(c) else { continue };
        let mesh = if outer { &g.outer } else { &g.inner };
        let base = pos.len() as u32;
        for v in &mesh.verts {
            pos.push([pen + g.offset[0] + v[0].abs() * g.scale, g.offset[1] + v[1] * g.scale, 0.0]);
            uv.push([v[2], v[3]]);
        }
        idx.extend(mesh.indices.iter().map(|&i| base + i as u32));
        pen += g.advance + track;
    }
    let width = pen;
    let dx = match t.horzalign {
        1 => -width / 2.0,
        2 => -width,
        _ => 0.0,
    };
    // Cap height ≈ 0.75 em (A).
    let dy = match t.vertalign {
        0 => -0.75,
        2 => 0.25,
        _ => -0.375,
    };
    for p in &mut pos {
        p[0] += dx;
        p[1] += dy;
    }
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uv)
        .with_inserted_indices(Indices::U32(idx))
}
