//! The track the car is on: Colorado (the open world's collision mesh, drawn in the developers'
//! debug surface colours when its scenery isn't converted), an imported map (`imported/<map>/world`, e.g. an FM4
//! layout, docs/RENDERING.md) or the flat test plane.

use std::path::Path;
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use fh1_engine::vehicle::{FlatGround, Ground};
use fh1_engine::world::{WorldGround, MIRROR_Z};

#[derive(Resource, Clone)]
pub struct Track {
    /// Map id as given to `--track` / settings `map`: `colorado`, `flat`, or an imported map folder (`fm4/<layout>`).
    pub id: String,
    pub name: String,
    pub ground: Arc<dyn Ground + Send + Sync>,
    pub world: Option<Arc<WorldGround>>,
    /// (ground point, yaw) where the car can start.
    pub spawns: Vec<(Vec3, f32)>,
    /// Display name per spawn (fast-travel menu).
    pub spawn_names: Vec<String>,
    /// Spawn used when FH1_SPAWN isn't set.
    pub home: usize,
}

impl Track {
    pub fn flat() -> Self {
        Self { id: "flat".into(), name: "Test plane".into(), ground: Arc::new(FlatGround), world: None, spawns: vec![(Vec3::ZERO, 0.0)], spawn_names: vec!["Test plane".into()], home: 0 }
    }

    /// Loads `world/colorado` from the converted install.
    pub fn colorado(assets: &Path) -> anyhow::Result<Self> {
        let mut t = Self::open_world(&assets.join("world/colorado"), "colorado", "Colorado", true)?;
        // Home: the festival spot of the Xenia reference shots (tools/xenia/captures/l4/x_pos1.png). Appended so
        // FH1_SPAWN=<n> indices stay as they were.
        let (x, z, yaw) = FESTIVAL_HOME;
        let y = t.ground.ray(Vec3::new(x, 500.0, z), Vec3::NEG_Y, 2000.0).map_or(0.0, |h| h.point.y);
        t.spawns.push((Vec3::new(x, y, z), yaw));
        t.spawn_names.push("Horizon Festival".into());
        t.home = t.spawns.len() - 1;
        Ok(t)
    }

    /// Loads an imported map's world (`imported/<map>/world`, written by e.g. `fh1setup import-fm4`).
    pub fn imported(assets: &Path, map: &str, name: &str) -> anyhow::Result<Self> {
        Self::open_world(&assets.join("imported").join(map).join("world"), map, name, false)
    }

    /// Maps the menu can switch to: Colorado plus the converted worlds of the imported [`OPTIONAL_GAMES`] (other
    /// folders under `imported/`, e.g. a leftover `imported/gtasa`, are ignored). (id, display name).
    pub fn available(assets: &Path) -> Vec<(String, String)> {
        let mut out = vec![("colorado".to_string(), "Colorado".to_string())];
        if !IMPORTED_MAPS {
            return out;
        }
        for g in &OPTIONAL_GAMES {
            let dir = assets.join("imported").join(g.folder);
            // Another game's import (FH2, FM4): `imported/<game>/maps.json` = [{id, name}], id = folder under
            // `imported/` (docs/FH2_RECON.md).
            if let Some(maps) = game_maps(&dir) {
                out.extend(maps.into_iter().filter(|(mid, _)| assets.join("imported").join(mid).join("world/collision.bin").exists()));
                continue;
            }
            if dir.join("world/collision.bin").exists() {
                out.push((g.folder.to_owned(), imported_name(g.folder)));
            }
        }
        out
    }

    /// [`Track::available`] grouped by game for the menus: group = the id's first path segment (`colorado` -> FH1,
    /// `fh2/anthem` -> FH2, others upper-cased), groups in first-seen order.
    pub fn maps_by_game(assets: &Path) -> Vec<(String, Vec<(String, String)>)> {
        let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for (id, name) in Self::available(assets) {
            let g = game_label(&id);
            match out.iter_mut().find(|(k, _)| *k == g) {
                Some((_, v)) => v.push((id, name)),
                None => out.push((g, vec![(id, name)])),
            }
        }
        out
    }

    /// A fh1-world folder (`collision.bin`, `surfaces.json`, `spawns.json`). Colorado's starts are sorted by open
    /// road ahead (some face an arena's rails); imported maps keep their file order (the menu's order).
    fn open_world(dir: &Path, id: &str, name: &str, sort_by_clear: bool) -> anyhow::Result<Self> {
        let world = Arc::new(WorldGround::load(&dir)?);
        let spawns_json: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("spawns.json"))?)?;
        let mz = if MIRROR_Z { -1.0 } else { 1.0 };
        let mut spawns = Vec::new();
        for s in spawns_json["spawns"].as_array().into_iter().flatten() {
            let n = |v: &serde_json::Value, i: usize| v[i].as_f64().unwrap_or(0.0) as f32;
            let p = &s["position"];
            let Some(ground_y) = s["ground_y"].as_f64() else { continue };
            let point = Vec3::new(n(p, 0), ground_y as f32, n(p, 2) * mz);
            let f = &s["facing"];
            let (fx, fz) = (n(f, 0), n(f, 2) * mz);
            let clear = s["clear_ahead"].as_f64().unwrap_or(0.0) as f32;
            let label = s["display_name"].as_str().map(str::to_owned).unwrap_or_else(|| spawn_label(s["name"].as_str().unwrap_or("start")));
            spawns.push((point, (-fx).atan2(-fz), clear, label));
        }
        if sort_by_clear {
            spawns.sort_by(|a, b| b.2.total_cmp(&a.2));
        }
        anyhow::ensure!(!spawns.is_empty(), "no spawn points in spawns.json");
        let spawn_names = spawns.iter().map(|s| s.3.clone()).collect();
        let spawns: Vec<(Vec3, f32)> = spawns.into_iter().map(|(p, y, _, _)| (p, y)).collect();
        Ok(Self { id: id.into(), name: name.into(), ground: world.clone(), world: Some(world), spawns, spawn_names, home: 0 })
    }
}

/// Colorado home spawn, engine space (x, z, yaw): the player car of the x_pos1 reference frame, from its body draw's
/// world matrix in RenderDoc capture fh1_frame4543 (game x -1028.39, z -153.034, yaw 133.888 deg; docs/GPU_CAPTURE.md).
const FESTIVAL_HOME: (f32, f32, f32) = (-1028.39, 153.034, -2.33679);

/// `start_location_07` -> `Start location 07`.
fn spawn_label(id: &str) -> String {
    let s = id.replace('_', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Menu group of a map id: `colorado` -> FH1, else its first path segment (`fh2` -> FH2).
pub fn game_label(id: &str) -> String {
    match id.split('/').next().unwrap_or(id) {
        "colorado" | "flat" => "FH1".into(),
        g => g.to_ascii_uppercase(),
    }
}

/// `imported/<game>/maps.json`: `[{id, name}]` (or `{"maps": [...]}`).
fn game_maps(game_dir: &Path) -> Option<Vec<(String, String)>> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(game_dir.join("maps.json")).ok()?).ok()?;
    let list = v.as_array().or_else(|| v["maps"].as_array())?;
    Some(list.iter().filter_map(|m| Some((m["id"].as_str()?.to_owned(), m["name"].as_str().unwrap_or(m["id"].as_str()?).to_owned()))).collect())
}

/// An imported map converted by fh1setup's own scenery pipeline (another Horizon-engine game, e.g. FH2's
/// `imported/fh2/anthem`, docs/FH2_RECON.md): its folder, drawn by [`crate::scenery::Scenery`] with the game shaders
/// instead of the glTF tiles of `imported.rs`.
pub fn native_track_dir(assets: &Path, id: &str) -> Option<std::path::PathBuf> {
    let dir = assets.join("imported").join(id);
    dir.join("scenery/materials.json").exists().then_some(dir)
}

/// The track's `TimeOfDayA.xml` (lighting curves): a native import's own (`imported/<id>/tracks`), else Colorado's.
pub fn time_of_day_path(assets: &Path, id: &str) -> std::path::PathBuf {
    native_track_dir(assets, id).map(|d| d.join("tracks/TimeOfDayA.xml")).filter(|p| p.exists()).unwrap_or_else(|| assets.join("tracks/colorado/TimeOfDayA.xml"))
}

/// FH1's post chain inputs for a map: a native import's own TimeOfDay / TrackSettings / post zones / templates / LUTs
/// (`imported/<id>/{tracks,dynamicpost,scenery}`, FH2: `Tracks/Anthem`), else Colorado's.
pub fn post_config(assets: &Path, id: &str) -> fh1_render::postfx::FxPostConfig {
    let native = native_track_dir(assets, id).and_then(|d| {
        let tracks = std::fs::read_dir(d.join("dynamicpost/Tracks")).ok()?.flatten().find(|e| e.path().is_dir())?.file_name().into_string().ok()?;
        Some((d, tracks))
    });
    match native {
        Some((d, t)) => fh1_render::postfx::FxPostConfig::from_track(assets, &d.join("tracks"), &d.join("dynamicpost"), &t, &d.join("scenery")),
        // Colorado's lighting and post inputs; the glows only on Colorado (another map's folder has no props/glows.json).
        None if id == "colorado" => fh1_render::postfx::FxPostConfig::from_assets(assets),
        None => {
            let mut c = fh1_render::postfx::FxPostConfig::from_assets(assets);
            c.0.scenery_dir = assets.join("imported").join(id).join("scenery");
            c
        }
    }
}

/// Imported maps (docs/RENDERING.md) on/off: when false they don't load and the map menu lists only Colorado.
pub const IMPORTED_MAPS: bool = true;

/// A game the setup can import next to FH1 (FH1 is the only required disc; `fh1setup import-fh2` / `import-fm4` write
/// `imported/<folder>`). Only these folders are read as imports. A game that isn't imported stays in the menus, greyed
/// ("locked") with [`OptionalGame::requirement`] as its hint.
pub struct OptionalGame {
    /// Folder under `imported/`; upper-cased it is the menu group (`fh2` -> FH2).
    pub folder: &'static str,
    /// `Forza Horizon 2`.
    pub name: &'static str,
    /// Open worlds (HORIZON) listed locked while the game is missing: (map id, name).
    pub worlds: &'static [(&'static str, &'static str)],
    /// Has circuits (MOTORSPORT).
    pub circuits: bool,
    /// The setup can't import it yet.
    pub coming_soon: bool,
}

pub const OPTIONAL_GAMES: [OptionalGame; 3] = [
    // fh1setup fh2.rs MAP_NAME.
    OptionalGame { folder: "fh2", name: "Forza Horizon 2", worlds: &[("fh2/anthem", "Southern Europe (FH2)")], circuits: false, coming_soon: false },
    OptionalGame { folder: "fm4", name: "Forza Motorsport 4", worlds: &[], circuits: true, coming_soon: false },
    OptionalGame { folder: "fm3", name: "Forza Motorsport 3", worlds: &[], circuits: true, coming_soon: true },
];

impl OptionalGame {
    /// Menu group / car-list game: `FH2`.
    pub fn label(&self) -> String {
        self.folder.to_ascii_uppercase()
    }

    /// At least one of its maps is converted (the [`Track::available`] check).
    pub fn maps_imported(&self, assets: &Path) -> bool {
        let dir = assets.join("imported").join(self.folder);
        match game_maps(&dir) {
            Some(maps) => maps.iter().any(|(id, _)| assets.join("imported").join(id).join("world/collision.bin").exists()),
            None => dir.join("world/collision.bin").exists(),
        }
    }

    /// Its car list is converted (`imported/<folder>/cars/index.json`, ui/browser.rs `imported_car_folders`).
    pub fn cars_imported(&self, assets: &Path) -> bool {
        assets.join("imported").join(self.folder).join("cars/index.json").exists()
    }

    /// The greyed rows' hint.
    pub fn requirement(&self) -> String {
        if self.coming_soon { "Coming soon".into() } else { format!("Requires {} — run FH1 Rewrite setup to add it", self.name) }
    }
}

/// A map row of a game that isn't imported (drawn greyed, can't be picked).
#[derive(Clone, Debug)]
pub struct LockedMap {
    /// Menu group (`FH2`).
    pub game: String,
    /// Never loaded: a world's real id (`fh2/anthem`), or `<folder>/*` for a game's circuits.
    pub id: String,
    pub name: String,
    pub reason: String,
    /// The game's circuits (MOTORSPORT) rather than an open world.
    pub circuit: bool,
    pub coming_soon: bool,
}

/// Locked rows of every [`OPTIONAL_GAMES`] game whose maps aren't imported: its open worlds, and one row for its circuits.
pub fn locked_maps(assets: &Path) -> Vec<LockedMap> {
    let mut out = Vec::new();
    if !IMPORTED_MAPS {
        return out;
    }
    for g in OPTIONAL_GAMES.iter().filter(|g| !g.maps_imported(assets)) {
        let row = |id: String, name: String, circuit: bool| LockedMap { game: g.label(), id, name, reason: g.requirement(), circuit, coming_soon: g.coming_soon };
        out.extend(g.worlds.iter().map(|(id, name)| row((*id).to_owned(), (*name).to_owned(), false)));
        if g.circuits {
            out.push(row(format!("{}/*", g.folder), format!("{} circuits", g.name), true));
        }
    }
    out
}

/// Display name of an imported map: `imported/<id>/track.json` "name", else [`imported_name`].
pub fn imported_name_at(assets: &Path, id: &str) -> String {
    std::fs::read(assets.join("imported").join(id).join("track.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|j| j["name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| imported_name(id))
}

/// Display name of an imported map folder.
pub fn imported_name(id: &str) -> String {
    spawn_label(id)
}

#[allow(clippy::too_many_arguments)]
pub fn setup(
    mut commands: Commands,
    track: Res<Track>,
    scenery: Option<Res<crate::scenery::Scenery>>,
    imported: Option<Res<crate::imported::ImportedScenery>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    match &track.world {
        // With real scenery the collision view is a hidden debug overlay: build it on the first V press instead of
        // behind the loading card (Colorado: 1.4 M triangles, ~110 ms on the main thread + ~170 MB of vertex buffers
        // uploaded for a mesh nobody sees). FH1_LAZY_COLLISION_VIEW=0 = build it up front as before.
        Some(_) if (scenery.is_some() || imported.is_some()) && lazy_collision_view() => {
            commands.insert_resource(CollisionViewLazy);
        }
        Some(world) => spawn_world_meshes(&mut commands, world, &mut meshes, &mut materials, scenery.is_none() && imported.is_none(), &track.name),
        None => spawn_test_plane(&mut commands, &mut meshes, &mut materials, &mut images),
    }
}

fn lazy_collision_view() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FH1_LAZY_COLLISION_VIEW").as_deref(), Ok("0") | Ok("off")))
}

/// The collision view of the loaded world isn't built yet (see `setup`).
#[derive(Resource)]
pub struct CollisionViewLazy;

/// First V press with the collision view not built: build it, shown (main.rs `toggle_collision_view` flips the
/// built tiles from then on).
pub fn collision_view_on_demand(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    lazy: Option<Res<CollisionViewLazy>>,
    track: Res<Track>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if lazy.is_none() || !keys.just_pressed(KeyCode::KeyV) {
        return;
    }
    commands.remove_resource::<CollisionViewLazy>();
    if let Some(world) = &track.world {
        spawn_world_meshes(&mut commands, world, &mut meshes, &mut materials, true, &track.name);
    }
}

/// The collision mesh in ~250 m tiles, flat-shaded, coloured by each surface's DebugColor.
/// The collision mesh drawn in DebugColors (toggle with V).
#[derive(Component)]
pub struct CollisionView;

fn spawn_world_meshes(commands: &mut Commands, world: &WorldGround, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, show_collision: bool, name: &str) {
    const TILE: f32 = 250.0;
    let w = &world.world;
    let colors: Vec<[f32; 4]> = w
        .surfaces
        .iter()
        .map(|s| {
            let [r, g, b] = s.debug_color();
            let lin = |c: u8| (c as f32 / 255.0).powf(2.2);
            [lin(r), lin(g), lin(b), 1.0]
        })
        .collect();
    let mz = if MIRROR_Z { -1.0 } else { 1.0 };
    let mut tiles: std::collections::HashMap<(i32, i32), (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 4]>)> = Default::default();
    for t in &w.tris {
        if Some(t.surface) == world.invisible || t.routes & world.routes == 0 {
            continue;
        }
        let p = t.v.map(|i| {
            let v = w.verts[i as usize];
            Vec3::new(v[0], v[1], v[2] * mz)
        });
        // Mirroring flips handedness, so swap two corners to keep the winding consistent.
        let p = if MIRROR_Z { [p[0], p[2], p[1]] } else { p };
        let n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or(Vec3::Y);
        let n = if n.y < -0.2 { -n } else { n };
        // The road network is fenced by very tall collision walls (guard rails and asphalt walls
        // reaching tens of metres up) that the game never draws. Keep them for physics, but
        // only draw walls up to a believable height.
        let height = p.iter().map(|v| v.y).fold(f32::MIN, f32::max) - p.iter().map(|v| v.y).fold(f32::MAX, f32::min);
        if n.y.abs() < 0.3 && height > 4.0 {
            continue;
        }
        let c = (p[0] + p[1] + p[2]) / 3.0;
        let key = ((c.x / TILE).floor() as i32, (c.z / TILE).floor() as i32);
        let tile = tiles.entry(key).or_default();
        let color = colors.get(t.surface as usize).copied().unwrap_or([1.0, 0.0, 1.0, 1.0]);
        for v in p {
            tile.0.push(v.to_array());
            tile.1.push(n.to_array());
            tile.2.push(color);
        }
    }
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        perceptual_roughness: 0.9,
        double_sided: true,
        cull_mode: None,
        ..default()
    });
    let count = tiles.len();
    // With the real scenery available the collision mesh is a debug overlay (V toggles it).
    let visibility = if show_collision { Visibility::Inherited } else { Visibility::Hidden };
    for (_, (pos, nrm, col)) in tiles {
        let n = pos.len() as u32;
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nrm);
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, col);
        mesh.insert_indices(Indices::U32((0..n).collect()));
        commands.spawn((Mesh3d(meshes.add(mesh)), MeshMaterial3d(material.clone()), CollisionView, visibility, crate::ui::world_load::WorldEntity));
    }
    info!("{name}: {} triangles in {count} tiles", w.tris.len());
}

fn spawn_test_plane(commands: &mut Commands, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, images: &mut Assets<Image>) {
    // Checker ground so speed is readable.
    let size = 64u32;
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let c = if (x / 32 + y / 32) % 2 == 0 { 92 } else { 76 };
            let line = if x % 32 == 0 || y % 32 == 0 { 30 } else { 0 };
            let v = (c + line) as u8;
            px.extend_from_slice(&[v, (v as f32 * 1.05) as u8, v, 255]);
        }
    }
    let mut img = Image::new(
        Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        TextureDimension::D2,
        px,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        ..ImageSamplerDescriptor::linear()
    });
    let extent = 4000.0;
    let mut ground = Mesh::from(Plane3d::default().mesh().size(extent, extent));
    if let Some(bevy::mesh::VertexAttributeValues::Float32x2(uvs)) = ground.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        // One checker square (half the texture) = 10 m.
        for uv in uvs.iter_mut() {
            uv[0] *= extent / 20.0;
            uv[1] *= extent / 20.0;
        }
    }
    commands.spawn((
        crate::ui::world_load::WorldEntity,
        Mesh3d(meshes.add(ground)),
        MeshMaterial3d(materials.add(StandardMaterial { base_color_texture: Some(images.add(img)), perceptual_roughness: 0.95, ..default() })),
    ));

    // Cones every 25 m along a straight, for scale and speed reference.
    let cone = meshes.add(Cone { radius: 0.18, height: 0.5 });
    let orange = materials.add(StandardMaterial { base_color: Color::srgb(1.0, 0.35, 0.05), ..default() });
    for k in 1..80 {
        for x in [-6.0, 6.0] {
            commands.spawn((Mesh3d(cone.clone()), MeshMaterial3d(orange.clone()), Transform::from_xyz(x, 0.25, -25.0 * k as f32), crate::ui::world_load::WorldEntity));
        }
    }
}
