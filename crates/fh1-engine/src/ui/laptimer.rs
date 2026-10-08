//! L1b: hot-lap timer for Motorsport circuits. Gates from `imported/<id>/track.json` `timing` (X2's FM4 contract):
//! `{finish: {centre: [x,y,z], forward: [x,z], width}, sectors: [gate, gate]}`, in collision space before MIRROR_Z.
//! Crossing the finish line forwards starts lap 1 (the run from the grid / pits is the out-lap), then each crossing
//! closes a lap: current, last and best lap and sector splits, top-right. Uses virtual time (pauses with the menu).

use bevy::prelude::*;
use fh1_engine::world::MIRROR_Z;

use super::{UiFont, ACCENT};
use crate::Car;

#[derive(Clone, Copy, Debug)]
struct Gate {
    centre: Vec3,
    forward: Vec2,
    half_width: f32,
}

impl Gate {
    fn parse(v: &serde_json::Value) -> Option<Self> {
        let c = v["centre"].as_array()?;
        let f = v["forward"].as_array()?;
        let mz = if MIRROR_Z { -1.0 } else { 1.0 };
        let n = |a: &Vec<serde_json::Value>, i: usize| a.get(i).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
        let forward = Vec2::new(n(f, 0), n(f, 1) * mz).normalize_or_zero();
        (forward != Vec2::ZERO).then_some(Self {
            centre: Vec3::new(n(c, 0), n(c, 1), n(c, 2) * mz),
            forward,
            half_width: v["width"].as_f64().unwrap_or(30.0) as f32 * 0.5 + 4.0,
        })
    }

    /// Crossed forwards between `a` and `b`.
    fn crossed(&self, a: Vec3, b: Vec3) -> bool {
        let rel = |p: Vec3| Vec2::new(p.x - self.centre.x, p.z - self.centre.z);
        let (da, db) = (rel(a).dot(self.forward), rel(b).dot(self.forward));
        if !(da < 0.0 && db >= 0.0) || (b.y - self.centre.y).abs() > 15.0 {
            return false;
        }
        let t = da / (da - db);
        let p = rel(a).lerp(rel(b), t);
        let side = Vec2::new(-self.forward.y, self.forward.x);
        p.dot(side).abs() <= self.half_width
    }
}

#[derive(Resource)]
pub struct LapTimer {
    finish: Gate,
    sectors: Vec<Gate>,
    /// Lap start (virtual s), lap number (0 = out-lap), next sector index.
    start: Option<f32>,
    lap: u32,
    next_sector: usize,
    splits: Vec<f32>,
    best_splits: Vec<Option<f32>>,
    last: Option<f32>,
    best: Option<f32>,
    prev: Option<Vec3>,
}

impl LapTimer {
    /// The track's timing gates, if its track.json has them.
    pub fn load(assets: &std::path::Path, id: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(assets.join("imported").join(id).join("track.json")).ok()?).ok()?;
        let t = &v["timing"];
        let finish = Gate::parse(&t["finish"])?;
        let sectors: Vec<Gate> = t["sectors"].as_array().into_iter().flatten().filter_map(Gate::parse).collect();
        let n = sectors.len();
        Some(Self { finish, sectors, start: None, lap: 0, next_sector: 0, splits: Vec::new(), best_splits: vec![None; n], last: None, best: None, prev: None })
    }
}

#[derive(Component)]
struct LapText;

#[derive(Component)]
struct LapRoot;

fn fmt(t: f32) -> String {
    let m = (t / 60.0).floor();
    format!("{}:{:06.3}", m as u32, t - m * 60.0)
}

fn delta(t: f32, best: Option<f32>) -> String {
    best.map_or_else(String::new, |b| format!("  ({}{:.3})", if t >= b { "+" } else { "-" }, (t - b).abs()))
}

fn update(timer: Option<ResMut<LapTimer>>, cars: Query<&Car>, time: Res<Time<Virtual>>, font: Res<UiFont>, mut commands: Commands, mut text: Query<&mut Text, With<LapText>>, root: Query<Entity, With<LapRoot>>) {
    let Some(mut lt) = timer else { return };
    let Some(car) = cars.iter().next() else { return };
    let now = time.elapsed_secs();
    let pos = car.0.position;
    if let Some(prev) = lt.prev.replace(pos) {
        // A teleport (reset, fast travel) isn't a crossing.
        if prev.distance(pos) < 30.0 {
            if lt.finish.crossed(prev, pos) {
                if let Some(s) = lt.start {
                    let lap = now - s;
                    // Only a full lap counts (all sectors passed, or no sectors).
                    if lt.next_sector >= lt.sectors.len() {
                        lt.last = Some(lap);
                        if lt.best.is_none_or(|b| lap < b) {
                            lt.best = Some(lap);
                            let splits = lt.splits.clone();
                            for (i, sp) in splits.into_iter().enumerate() {
                                if let Some(b) = lt.best_splits.get_mut(i) {
                                    *b = Some(sp);
                                }
                            }
                        }
                    }
                }
                lt.start = Some(now);
                lt.lap += 1;
                lt.next_sector = 0;
                lt.splits.clear();
            } else if let (Some(s), Some(g)) = (lt.start, lt.sectors.get(lt.next_sector).copied()) {
                if g.crossed(prev, pos) {
                    lt.splits.push(now - s);
                    lt.next_sector += 1;
                }
            }
        }
    }
    if root.is_empty() {
        commands
            .spawn((
                LapRoot,
                // The circuit's HUD: unloaded with the map (X1c).
                super::world_load::WorldEntity,
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(28.0),
                    top: Val::Px(24.0),
                    padding: UiRect::all(Val::Px(12.0)),
                    border: UiRect::left(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.04, 0.05, 0.07, 0.6)),
                BorderColor::all(ACCENT),
            ))
            .with_child((LapText, Text::new(""), font.text(20.0), TextColor(Color::WHITE)));
        return;
    }
    let mut s = match lt.start {
        Some(st) => format!("LAP {}   {}", lt.lap, fmt(now - st)),
        None => "OUT LAP".to_string(),
    };
    for (i, sp) in lt.splits.iter().enumerate() {
        s.push_str(&format!("\nS{}  {}{}", i + 1, fmt(*sp), delta(*sp, lt.best_splits.get(i).copied().flatten())));
    }
    if let Some(l) = lt.last {
        s.push_str(&format!("\nLAST  {}{}", fmt(l), delta(l, lt.best)));
    }
    if let Some(b) = lt.best {
        s.push_str(&format!("\nBEST  {}", fmt(b)));
    }
    for mut t in &mut text {
        if t.0 != s {
            t.0 = s.clone();
        }
    }
}

pub struct LapTimerPlugin;

impl Plugin for LapTimerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, update.after(crate::sync_visuals));
    }
}
