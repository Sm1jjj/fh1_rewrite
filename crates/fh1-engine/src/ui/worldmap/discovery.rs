//! Discovered roads, as FH1's map shows them: every road segment the player has driven is drawn in its
//! road-type colour (MapProfileFullscreen.xml `ColourSecondary`), the rest in the grey `ColourPrimary` (120,120,120).
//! A segment counts as driven when the car passes within [`REACH_M`] of it. Saved beside settings.json as
//! `map_discovered.bin` (one byte per segment of `NavGraph::roads`, in order; a different length = a different road
//! network = start over). `FH1_MAP_DISCOVERY=0` draws every road as discovered.

use std::collections::HashMap;
use std::path::PathBuf;

use bevy::prelude::*;

use crate::ui::minimap::NavGraph;
use crate::Car;

/// A segment is driven when the car's centre comes this close (m).
const REACH_M: f32 = 22.0;
/// Grid cell for the segment lookup (m).
const CELL: f32 = 100.0;
/// Seconds between saves while something new was found.
const SAVE_EVERY: f32 = 20.0;

pub fn discovery_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_MAP_DISCOVERY").map_or(true, |v| v != "0"))
}

#[derive(Resource)]
pub struct Discovered {
    /// Per segment (road k's segment j = `first[k] + j`): driven.
    pub seen: Vec<u8>,
    pub first: Vec<usize>,
    /// Segment ends (engine x, z).
    segs: Vec<(Vec2, Vec2)>,
    grid: HashMap<(i32, i32), Vec<u32>>,
    path: PathBuf,
    /// Bumped when a segment is newly driven (the map rebuilds its road meshes).
    pub version: u32,
    unsaved: bool,
    since_save: f32,
    tick: f32,
}

impl Discovered {
    pub fn is_seen(&self, road: usize, seg: usize) -> bool {
        !discovery_on() || self.first.get(road).and_then(|f| self.seen.get(f + seg)).is_some_and(|&b| b != 0)
    }

    fn build(graph: &NavGraph, path: PathBuf) -> Self {
        let mut first = Vec::with_capacity(graph.roads.len());
        let mut segs = Vec::new();
        for (_, pts) in graph.roads.iter() {
            first.push(segs.len());
            for w in pts.windows(2) {
                segs.push((Vec2::from(w[0]), Vec2::from(w[1])));
            }
        }
        let mut grid: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for (i, (a, b)) in segs.iter().enumerate() {
            let (lo, hi) = (a.min(*b) - Vec2::splat(REACH_M), a.max(*b) + Vec2::splat(REACH_M));
            for cx in (lo.x / CELL).floor() as i32..=(hi.x / CELL).floor() as i32 {
                for cz in (lo.y / CELL).floor() as i32..=(hi.y / CELL).floor() as i32 {
                    grid.entry((cx, cz)).or_default().push(i as u32);
                }
            }
        }
        let seen = std::fs::read(&path).ok().filter(|b| b.len() == segs.len()).unwrap_or_else(|| vec![0; segs.len()]);
        info!("map: {} of {} road segments discovered", seen.iter().filter(|&&b| b != 0).count(), segs.len());
        Self { seen, first, segs, grid, path, version: 1, unsaved: false, since_save: 0.0, tick: 0.0 }
    }

    pub fn save(&mut self) {
        if !self.unsaved {
            return;
        }
        self.unsaved = false;
        self.since_save = 0.0;
        if let Err(e) = std::fs::write(&self.path, &self.seen) {
            warn!("map: can't save {}: {e}", self.path.display());
        }
    }
}

pub(super) fn build(app: &mut App) {
    app.add_systems(Update, (init, track).chain().run_if(crate::ui::minimap::on_colorado));
}

fn init(mut commands: Commands, graph: Option<Res<NavGraph>>, have: Option<Res<Discovered>>, path: Res<crate::ui::SettingsPath>) {
    if have.is_some() {
        return;
    }
    let Some(graph) = graph else { return };
    commands.insert_resource(Discovered::build(&graph, path.0.with_file_name("map_discovered.bin")));
}

fn closest_on_segment(p: Vec2, a: Vec2, b: Vec2) -> Vec2 {
    let ab = b - a;
    let t = if ab.length_squared() > 1e-6 { ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
    a + ab * t
}

fn track(real: Res<Time<Real>>, d: Option<ResMut<Discovered>>, cars: Query<&Car>) {
    let Some(mut d) = d else { return };
    let dt = real.delta_secs();
    d.since_save += dt;
    d.tick += dt;
    if d.unsaved && d.since_save > SAVE_EVERY {
        d.save();
    }
    if d.tick < 0.2 {
        return;
    }
    d.tick = 0.0;
    let Ok(car) = cars.single() else { return };
    let p = Vec2::new(car.0.position.x, car.0.position.z);
    let key = ((p.x / CELL).floor() as i32, (p.y / CELL).floor() as i32);
    let Some(list) = d.grid.get(&key).cloned() else { return };
    let mut new = false;
    for i in list {
        let i = i as usize;
        if d.seen[i] != 0 {
            continue;
        }
        let (a, b) = d.segs[i];
        if closest_on_segment(p, a, b).distance(p) < REACH_M {
            d.seen[i] = 1;
            new = true;
        }
    }
    if new {
        d.version += 1;
        d.unsaved = true;
    }
}
