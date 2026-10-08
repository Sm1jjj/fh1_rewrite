//! Imported maps' scenery (docs/RENDERING.md): streams the glTF tiles written by the import tools
//! (`imported/<map>/scenery/{hd,lod}/<x>_<z>.gltf`, `index.json`, `water.gltf`)
//! around the entity marked [`SceneryFocus`]. HD tiles within [`ImportedScenery::hd_radius`], LOD tiles beyond
//! it (only where no HD tile is shown) up to [`ImportedScenery::lod_radius`]; a few spawns per frame, nearest
//! first (queuing hundreds of glTF loads at once overflowed the stack of Bevy's IO task pool).
//! Only bevy + serde_json.

use std::collections::HashMap;
use std::path::Path;

use bevy::gltf::GltfAssetLabel;
use bevy::prelude::*;
use bevy::world_serialization::WorldAssetRoot;

/// Marks the entity the scenery streams around (the car).
#[derive(Component)]
pub struct SceneryFocus;

/// Marks spawned tile roots.
#[derive(Component)]
pub struct ImportedTile;

#[derive(Resource)]
pub struct ImportedScenery {
    /// Asset path of the scenery folder, e.g. `imported/fm4/<layout>/scenery`.
    pub root: String,
    pub tile: f32,
    pub hd: HashMap<(i32, i32), String>,
    pub lod: HashMap<(i32, i32), String>,
    pub water: Option<String>,
    pub hd_radius: f32,
    pub lod_radius: f32,
    /// Tile spawns per frame.
    pub per_frame: usize,
    spawned: HashMap<(bool, i32, i32), Entity>,
    water_spawned: bool,
}

impl ImportedScenery {
    /// `assets` = the install's `assets/private` (the asset root); `map` = folder under `imported/`.
    pub fn load(assets: &Path, map: &str) -> Option<Self> {
        let root = format!("imported/{map}/scenery");
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(assets.join(&root).join("index.json")).ok()?).ok()?;
        let tiles = |k: &str| -> HashMap<(i32, i32), String> {
            index[k]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| Some(((t[0].as_i64()? as i32, t[1].as_i64()? as i32), t[2].as_str()?.to_owned())))
                .collect()
        };
        Some(Self {
            tile: index["tile"].as_f64().unwrap_or(250.0) as f32,
            hd: tiles("hd"),
            lod: tiles("lod"),
            water: index["water"]["file"].as_str().map(str::to_owned),
            root,
            hd_radius: index["hd_radius"].as_f64().unwrap_or(600.0) as f32,
            lod_radius: 3000.0,
            per_frame: 4,
            spawned: HashMap::new(),
            water_spawned: false,
        })
    }

    fn centre(&self, (x, z): (i32, i32)) -> Vec2 {
        Vec2::new((x as f32 + 0.5) * self.tile, (z as f32 + 0.5) * self.tile)
    }
}

pub struct ImportedSceneryPlugin;

impl Plugin for ImportedSceneryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, stream.run_if(resource_exists::<ImportedScenery>));
    }
}

fn stream(mut commands: Commands, mut sc: ResMut<ImportedScenery>, focus: Query<&GlobalTransform, With<SceneryFocus>>, assets: Res<AssetServer>) {
    let Some(f) = focus.iter().next() else { return };
    let here = Vec2::new(f.translation().x, f.translation().z);
    if !sc.water_spawned {
        if let Some(w) = sc.water.clone() {
            commands.spawn((WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(format!("{}/{w}", sc.root)))), ImportedTile));
        }
        sc.water_spawned = true;
    }
    // Wanted: (is_lod, key, distance).
    let mut wanted: Vec<(bool, (i32, i32), f32)> = Vec::new();
    for &k in sc.hd.keys() {
        let d = sc.centre(k).distance(here);
        if d < sc.hd_radius {
            wanted.push((false, k, d));
        }
    }
    let hd_shown: std::collections::HashSet<(i32, i32)> = wanted.iter().map(|w| w.1).collect();
    for &k in sc.lod.keys() {
        let d = sc.centre(k).distance(here);
        if d < sc.lod_radius && !hd_shown.contains(&k) {
            wanted.push((true, k, d));
        }
    }
    let keep: std::collections::HashSet<(bool, i32, i32)> = wanted.iter().map(|&(l, (x, z), _)| (l, x, z)).collect();
    // Despawn what's no longer wanted (with a little slack so tiles don't flicker on a boundary).
    let slack = sc.tile * 0.5;
    let drop: Vec<(bool, i32, i32)> = sc
        .spawned
        .keys()
        .filter(|k| !keep.contains(k))
        .filter(|&&(lod, x, z)| {
            let d = sc.centre((x, z)).distance(here);
            if lod { d > sc.lod_radius + slack || hd_shown.contains(&(x, z)) } else { d > sc.hd_radius + slack }
        })
        .copied()
        .collect();
    for k in drop {
        if let Some(e) = sc.spawned.remove(&k) {
            commands.entity(e).despawn();
        }
    }
    wanted.retain(|&(l, (x, z), _)| !sc.spawned.contains_key(&(l, x, z)));
    wanted.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.total_cmp(&b.2)));
    for (lod, (x, z), _) in wanted.into_iter().take(sc.per_frame) {
        let file = if lod { &sc.lod[&(x, z)] } else { &sc.hd[&(x, z)] };
        let path = format!("{}/{}/{file}", sc.root, if lod { "lod" } else { "hd" });
        let e = commands.spawn((WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(path))), ImportedTile)).id();
        sc.spawned.insert((lod, x, z), e);
    }
}
