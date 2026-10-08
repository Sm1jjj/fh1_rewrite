//! Per-map shader globals for imported native maps: `imported/<id>/tracks/fx_globals.json`
//! = `{globals: {name: [x, y, z, w]}, suffix_defaults: {suffix: [x, y, z, w]}}` (written by fh1setup, e.g. `import-fm4`:
//! FM4's track shaders read globals FH1's lighting never sets, such as `V2LightmapColor1/2`, `RoadSpecularParams1/2` and
//! the per-texture `*_uvOS`; left at 0 they draw near-black roads, docs/FM4_RECON.md). Applied every frame because a
//! global only exists once a program that reads it has been built (scenery streams in). Maps without the file
//! (Colorado, FH2, ...) are untouched. `FH1_FX_OVERRIDES=0` = off.

use bevy::prelude::*;
use fh1_render::FxGlobals;
use serde_json::Value;

use crate::track::Track;

pub struct FxOverridesPlugin;

impl Plugin for FxOverridesPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var("FH1_FX_OVERRIDES").as_deref() == Ok("0") {
            return;
        }
        app.init_resource::<FxOverrides>().add_systems(PostUpdate, apply);
    }
}

#[derive(Resource, Default)]
struct FxOverrides {
    /// Map id the values were read for.
    map: Option<String>,
    globals: Vec<(String, Vec4)>,
    suffix: Vec<(String, Vec4)>,
}

fn vec4(v: &Value) -> Option<Vec4> {
    let a = v.as_array()?;
    let f = |i: usize| a.get(i).and_then(Value::as_f64).map(|x| x as f32);
    Some(Vec4::new(f(0)?, f(1)?, f(2)?, f(3)?))
}

fn apply(track: Option<Res<Track>>, garage: Option<Res<crate::Garage>>, mut state: ResMut<FxOverrides>, globals: Option<ResMut<FxGlobals>>) {
    let (Some(track), Some(garage), Some(mut g)) = (track, garage, globals) else { return };
    if state.map.as_deref() != Some(track.id.as_str()) {
        let path = garage.assets.join("imported").join(&track.id).join("tracks/fx_globals.json");
        let doc: Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let read = |key: &str| -> Vec<(String, Vec4)> {
            doc[key].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), vec4(v)?))).collect()).unwrap_or_default()
        };
        *state = FxOverrides { map: Some(track.id.clone()), globals: read("globals"), suffix: read("suffix_defaults") };
        if !state.globals.is_empty() || !state.suffix.is_empty() {
            info!("fx overrides for {}: {} globals, {} suffix defaults", track.id, state.globals.len(), state.suffix.len());
        }
    }
    if state.globals.is_empty() && state.suffix.is_empty() {
        return;
    }
    let by_suffix: Vec<(String, Vec4)> = g
        .names()
        .filter_map(|n| state.suffix.iter().find(|(s, _)| n.ends_with(s.as_str())).map(|(_, v)| (n.to_owned(), *v)))
        .collect();
    for (name, v) in by_suffix.iter().chain(&state.globals) {
        if g.get(name) != Some(*v) {
            g.set_vec(name, *v);
        }
    }
}
