//! PERF P2 diagnostics (festival pop-in / FPS), driven from `scenery::stream` so it needs no main.rs lines.
//!
//! - `FH1_VSYNC=0`: presents without vsync (AutoNoVsync), so frame costs below 16.7 ms show.
//! - `FH1_P2_STATS=1`: every second logs frame mean / p99 / max, entities, mesh entities (total / visible last frame),
//!   prop and zone counts, FxMaterial assets and the streaming events of that second.
//! - `FH1_P2_AB=1`: in-process A/B (FH1_SHADOW_AB pattern): cycles the modes below every `AB_SECS`, logging per mode,
//!   so costs compare inside one run (other sessions load the machine; never compare across runs).
//! - `FH1_P2_FLY=<m/s>[,<yaw deg>[,slide[,<start m>]]]`: pop-in test. From 10 s on, the car is placed every frame along a straight line
//!   (held on the ground) at that speed, so streaming crosses many zone cells; FH1_ZONE_STATS=1 logs the switches.
//!   `slide`: instead, the car's own pose is shifted along its heading every frame (suspension and attitude stay
//!   live; flat ground only): moving-car checks such as shadow flicker. It first carries the car `start` m (default
//!   0) along `yaw` at 60 m/s; with speed 0, FH1_AUTODRIVE then drives it from there.
//! - `FH1_P2_BURST=<start s>,<frames>,<dir>`: saves that many consecutive frames as <dir>/f000.png... from <start> s,
//!   then exits. Unlike FH1_SHOT it leaves photo mode off, so FH1_AUTODRIVE drives (flicker checks on a moving car).

use bevy::prelude::*;
use bevy::window::{PresentMode, PrimaryWindow};

/// Seconds per A/B mode.
const AB_SECS: f64 = 5.0;
/// A/B modes: what is hidden.
pub const AB_MODES: [&str; 4] = ["all", "no_props", "no_zones", "no_props_no_zones"];

#[derive(Default)]
pub struct P2 {
    vsync_done: bool,
    pub stats: bool,
    ab: bool,
    window: f64,
    frames: Vec<f32>,
    /// Streaming events in the window.
    pub zone_loads: u32,
    pub prop_tiles: u32,
    pub spawned: u32,
    /// A/B: current mode, when it started, and per-mode frame times (cycles >= 1 only: cycle 0 compiles shaders).
    pub mode: usize,
    mode_start: f64,
    cycle: u32,
    ab_frames: Vec<Vec<f32>>,
}

impl P2 {
    pub fn new() -> Self {
        let on = |k: &str| std::env::var(k).is_ok_and(|v| v == "1");
        Self { stats: on("FH1_P2_STATS"), ab: on("FH1_P2_AB"), ab_frames: vec![Vec::new(); AB_MODES.len()], ..default() }
    }

    pub fn hide_props(&self) -> bool {
        self.ab && self.mode & 1 != 0
    }

    pub fn hide_zones(&self) -> bool {
        self.ab && self.mode & 2 != 0
    }
}

/// Other mesh kinds counted: caster proxy, grass, skinned.
type Kind = (Has<fh1_render::shadow::CasterProxy>, Has<MeshMaterial3d<crate::grass::GrassMaterial>>, Has<bevy::mesh::skinning::SkinnedMesh>, Has<MeshMaterial3d<crate::crowd::CrowdMaterial>>, Has<MeshMaterial3d<fh1_render::FxMaterial>>, Has<MeshMaterial3d<fh1_render::car_material::FxCarMaterial>>, Has<MeshMaterial3d<fh1_render::car_material::FxRawStandard>>, Has<MeshMaterial3d<StandardMaterial>>);

/// P2 tags for counting (zone-model batches / prop placement batches).
#[derive(Component)]
pub struct ZoneMesh;
#[derive(Component)]
pub struct PropMesh;

#[derive(bevy::ecs::system::SystemParam)]
pub struct P2Params<'w, 's> {
    real: Res<'w, Time<Real>>,
    windows: Query<'w, 's, &'static mut Window, With<PrimaryWindow>>,
    entities: &'w bevy::ecs::entity::Entities,
    meshes: Query<'w, 's, (&'static ViewVisibility, Has<ZoneMesh>, Has<PropMesh>, Kind), With<Mesh3d>>,
}

fn stats(f: &mut [f32]) -> (f32, f32, f32, usize) {
    f.sort_by(f32::total_cmp);
    let n = f.len().max(1);
    let mean = f.iter().sum::<f32>() / n as f32;
    (mean, f.get((n * 99 / 100).min(n - 1)).copied().unwrap_or(0.0), f.last().copied().unwrap_or(0.0), f.len())
}

/// Per frame, before streaming. Returns true when the A/B mode changed this frame.
pub fn tick(p2: &mut P2, q: &mut P2Params, props: usize, zones_loaded: usize, fx_materials: usize, here: Vec2) -> bool {
    if !p2.vsync_done {
        p2.vsync_done = true;
        if std::env::var("FH1_VSYNC").as_deref() == Ok("0") {
            for mut w in &mut q.windows {
                w.present_mode = PresentMode::AutoNoVsync;
            }
            info!("p2: vsync off (FH1_VSYNC=0)");
        }
    }
    let now = q.real.elapsed_secs_f64();
    let ms = q.real.delta_secs() * 1000.0;
    let mut changed = false;
    if p2.ab {
        if p2.cycle >= 1 {
            p2.ab_frames[p2.mode].push(ms);
        }
        if now - p2.mode_start >= AB_SECS {
            p2.mode_start = now;
            p2.mode = (p2.mode + 1) % AB_MODES.len();
            changed = true;
            if p2.mode == 0 {
                p2.cycle += 1;
                if p2.cycle >= 2 {
                    let line: Vec<String> = p2
                        .ab_frames
                        .iter_mut()
                        .zip(AB_MODES)
                        .map(|(f, m)| {
                            let (mean, p99, _, n) = stats(&mut f.clone());
                            format!("{m} {mean:.2}/{p99:.2} ms ({n})")
                        })
                        .collect();
                    info!("p2 A/B after cycle {} (mean/p99): {}", p2.cycle - 1, line.join(", "));
                }
            }
        }
    }
    if p2.stats {
        p2.frames.push(ms);
        if now - p2.window >= 1.0 {
            let (mean, p99, max, n) = stats(&mut p2.frames);
            let (mut total, mut visible, mut zone, mut prop) = (0, 0, [0usize; 2], [0usize; 2]);
            let mut kinds = [[0usize; 2]; 8];
            for (v, z, p, (k0, k1, k2, k3, k4, k5, k6, k7)) in &q.meshes {
                // FxMaterial counted only when untagged (not zone / prop).
                let k4 = k4 && !z && !p;
                for (i, k) in [k0, k1, k2, k3, k4, k5, k6, k7].into_iter().enumerate() {
                    if k {
                        kinds[i][0] += 1;
                        kinds[i][1] += v.get() as usize;
                    }
                }
                total += 1;
                visible += v.get() as usize;
                if z {
                    zone[0] += 1;
                    zone[1] += v.get() as usize;
                }
                if p {
                    prop[0] += 1;
                    prop[1] += v.get() as usize;
                }
            }
            info!(
                "p2 t={now:.1}: frame {mean:.2}/{p99:.2}/{max:.2} ms ({n}) mode {}; entities {}, meshes {total} ({visible} visible; zone {}/{} vis, props {}/{} vis, proxies {}/{}, grass {}/{}, skinned {}/{}, crowd {}/{}, other fx {}/{}, car {}/{}, rawstd {}/{}, std {}/{}), prop tiles {props}, zone models {zones_loaded}, FxMat {}; this s: {} zone loads, {} prop tiles, {} spawned; at {:.0},{:.0}",
                if p2.ab { AB_MODES[p2.mode] } else { "-" },
                q.entities.len(),
                zone[0],
                zone[1],
                prop[0],
                prop[1],
                kinds[0][0],
                kinds[0][1],
                kinds[1][0],
                kinds[1][1],
                kinds[2][0],
                kinds[2][1],
                kinds[3][0],
                kinds[3][1],
                kinds[4][0],
                kinds[4][1],
                kinds[5][0],
                kinds[5][1],
                kinds[6][0],
                kinds[6][1],
                kinds[7][0],
                kinds[7][1],
                fx_materials,
                p2.zone_loads,
                p2.prop_tiles,
                p2.spawned,
                here.x,
                here.y
            );
            p2.frames.clear();
            p2.window = now;
            p2.zone_loads = 0;
            p2.prop_tiles = 0;
            p2.spawned = 0;
        }
    }
    changed
}

/// `FH1_P2_STATS=1` extras that need their own systems: main-world update time (First -> Last) and per-pass GPU / CPU
/// render timings (Bevy RenderDiagnosticsPlugin, timestamp queries), logged every 2 s.
pub struct P2Plugin;

impl Plugin for P2Plugin {
    fn build(&self, app: &mut App) {
        if std::env::var("FH1_P2_FLY").is_ok() {
            app.add_systems(Update, fly);
        }
        if std::env::var("FH1_P2_BURST").is_ok() {
            app.add_systems(Last, burst);
        }
        if !std::env::var("FH1_P2_STATS").is_ok_and(|v| v == "1") {
            return;
        }
        if !app.is_plugin_added::<bevy::render::diagnostic::RenderDiagnosticsPlugin>() {
            app.add_plugins(bevy::render::diagnostic::RenderDiagnosticsPlugin);
        }
        app.init_resource::<MainTime>()
            .add_systems(First, |mut m: ResMut<MainTime>| m.start = Some(std::time::Instant::now()))
            .add_systems(Last, main_time_report);
        render_phases(app);
        main_phases(app);
    }
}

/// Empty mark schedules between the main schedules (MainScheduleOrder), so each main schedule is timed exactly.
#[derive(bevy::ecs::schedule::ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
struct MainMark(usize);

const MAIN_PHASES: [&str; 6] = ["PreUpdate", "fixed(physics)", "Update", "SpawnScene+PostUpdate", "Last", "First"];

fn main_phases(app: &mut App) {
    use bevy::app::{MainScheduleOrder, RunFixedMainLoop, SpawnScene};
    app.init_resource::<MainMarks>();
    // Mark k runs after: First, PreUpdate, RunFixedMainLoop, Update, PostUpdate, Last.
    for k in 0..6 {
        let mut sched = Schedule::new(MainMark(k));
        sched.add_systems(move |mut m: ResMut<MainMarks>| m.mark(k));
        app.add_schedule(sched);
    }
    let _ = SpawnScene;
    let mut order = app.world_mut().resource_mut::<MainScheduleOrder>();
    order.insert_after(First, MainMark(0));
    order.insert_after(PreUpdate, MainMark(1));
    order.insert_after(RunFixedMainLoop, MainMark(2));
    order.insert_after(Update, MainMark(3));
    order.insert_after(PostUpdate, MainMark(4));
    order.insert_after(Last, MainMark(5));
}

#[derive(Resource, Default)]
struct MainMarks {
    last: Option<std::time::Instant>,
    sums: [f64; 6],
    frames: u32,
    since: Option<std::time::Instant>,
}

impl MainMarks {
    /// Mark k = end of the phase that started at mark k-1 (mark 0 ends "First", measured from the last mark of the
    /// previous frame, so it also holds the frame's wait / present outside the schedules).
    fn mark(&mut self, k: usize) {
        let now = std::time::Instant::now();
        if let Some(l) = self.last {
            // Phase index: time since the previous mark belongs to the phase ending here.
            let phase = if k == 0 { 5 } else { k - 1 };
            self.sums[phase] += now.duration_since(l).as_secs_f64() * 1000.0;
        }
        self.last = Some(now);
        if k == 5 {
            self.frames += 1;
            let since = *self.since.get_or_insert(now);
            if now.duration_since(since).as_secs_f32() >= 2.0 {
                let n = self.frames.max(1) as f64;
                let line: Vec<String> = MAIN_PHASES.iter().zip(self.sums).map(|(k, v)| format!("{k} {:.2}", v / n)).collect();
                info!("p2 main schedules ms/frame ('First' includes frame wait/present): {}", line.join(", "));
                self.sums = [0.0; 6];
                self.frames = 0;
                self.since = Some(now);
            }
        }
    }
}

/// Render-world phase boundaries timed: (label, set the mark runs after). The first mark runs before
/// ExtractCommands, i.e. right after extraction; "extract+wait" = previous Cleanup end -> this mark.
fn render_phases(app: &mut App) {
    use bevy::render::{Render, RenderApp, RenderSystems as R};
    let Some(ra) = app.get_sub_app_mut(RenderApp) else { return };
    ra.init_resource::<RenderMarks>();
    let sets = [R::ExtractCommands, R::PrepareAssets, R::PrepareMeshes, R::CreateViews, R::Specialize, R::PrepareViews, R::Queue, R::PhaseSort, R::Prepare, R::Render, R::Cleanup];
    ra.add_systems(Render, (|mut m: ResMut<RenderMarks>| m.mark(0)).before(R::ExtractCommands));
    for (i, set) in sets.iter().enumerate() {
        let mark = move |mut m: ResMut<RenderMarks>| m.mark(i + 1);
        let sys = IntoSystem::into_system(move |m: ResMut<RenderMarks>| mark(m));
        let c = sys.after(set.clone());
        match sets.get(i + 1) {
            Some(next) => ra.add_systems(Render, c.before(next.clone())),
            None => ra.add_systems(Render, c),
        };
    }
}

const PHASES: [&str; 12] = ["extract+wait", "extract_cmds", "prep_assets", "prep_meshes", "create_views", "specialize", "prep_views", "queue", "phase_sort", "prepare", "render", "cleanup"];

#[derive(Resource, Default)]
struct RenderMarks {
    last: Option<std::time::Instant>,
    sums: [f64; 12],
    frames: u32,
    since: Option<std::time::Instant>,
}

impl RenderMarks {
    fn mark(&mut self, i: usize) {
        let now = std::time::Instant::now();
        if let Some(l) = self.last {
            self.sums[i] += now.duration_since(l).as_secs_f64() * 1000.0;
        }
        self.last = Some(now);
        if i == 11 {
            self.frames += 1;
            let since = *self.since.get_or_insert(now);
            if now.duration_since(since).as_secs_f32() >= 2.0 {
                let n = self.frames.max(1) as f64;
                let total: f64 = self.sums[1..].iter().sum::<f64>() / n;
                let line: Vec<String> = PHASES.iter().zip(self.sums).map(|(k, v)| format!("{k} {:.2}", v / n)).collect();
                info!("p2 render world {total:.2} ms/frame (excl. extract+wait): {}", line.join(", "));
                self.sums = [0.0; 12];
                self.frames = 0;
                self.since = Some(now);
            }
        }
    }
}

#[derive(Resource, Default)]
struct MainTime {
    start: Option<std::time::Instant>,
    samples: Vec<f32>,
    last: Option<std::time::Instant>,
}

fn main_time_report(mut m: ResMut<MainTime>, store: Res<bevy::diagnostic::DiagnosticsStore>) {
    if let Some(s) = m.start.take() {
        let ms = s.elapsed().as_secs_f32() * 1000.0;
        m.samples.push(ms);
    }
    let now = std::time::Instant::now();
    if m.last.is_some_and(|l| now.duration_since(l).as_secs_f32() < 2.0) {
        return;
    }
    m.last = Some(now);
    let (mean, p99, _, n) = stats(&mut m.samples);
    m.samples.clear();
    // Per-pass timings: (path, smoothed ms). Paths look like render/<pass>/elapsed_gpu.
    let mut gpu: Vec<(String, f64)> = Vec::new();
    let mut cpu: Vec<(String, f64)> = Vec::new();
    for d in store.iter() {
        let path = d.path().as_str();
        let Some(v) = d.smoothed() else { continue };
        if let Some(p) = path.strip_prefix("render/") {
            if let Some(name) = p.strip_suffix("/elapsed_gpu") {
                gpu.push((name.to_owned(), v));
            } else if let Some(name) = p.strip_suffix("/elapsed_cpu") {
                cpu.push((name.to_owned(), v));
            }
        }
    }
    // Top-level passes only (no '/') sum to the frame; nested spans are listed too.
    let top = |v: &[(String, f64)]| v.iter().filter(|(k, _)| !k.contains('/')).map(|(_, x)| x).sum::<f64>();
    let (gsum, csum) = (top(&gpu), top(&cpu));
    gpu.sort_by(|a, b| b.1.total_cmp(&a.1));
    cpu.sort_by(|a, b| b.1.total_cmp(&a.1));
    let fmt = |v: &[(String, f64)]| v.iter().take(8).map(|(k, x)| format!("{k} {x:.2}")).collect::<Vec<_>>().join(", ");
    info!("p2 main world {mean:.2}/{p99:.2} ms ({n}); GPU passes {gsum:.2} ms: {}", fmt(&gpu));
    info!("p2 render CPU (pass encode) {csum:.2} ms: {}", fmt(&cpu));
}

/// `FH1_P2_FLY`: move the car along a straight line on the ground (see the module docs).
fn fly(time: Res<Time<Real>>, track: Res<crate::track::Track>, mut cars: Query<&mut crate::Car>, mut start: Local<Option<Vec3>>) {
    let t = time.elapsed_secs() - 10.0;
    if t < 0.0 {
        return;
    }
    let spec = std::env::var("FH1_P2_FLY").unwrap_or_default();
    let parts: Vec<&str> = spec.split(',').map(str::trim).collect();
    let num = |i: usize, d: f32| parts.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    let slide = parts.get(2) == Some(&"slide");
    let (speed, yaw) = (num(0, 30.0), num(1, 0.0).to_radians());
    let Some(mut car) = cars.iter_mut().next() else { return };
    if slide {
        // Travel to the start at 60 m/s (re-placed every frame: ground collision only exists near the car).
        let origin = *start.get_or_insert(car.0.position);
        let travel = num(3, 0.0);
        if t * 60.0 < travel {
            let p = origin + Vec3::new(-yaw.sin(), 0.0, -yaw.cos()) * t * 60.0;
            if let Some(h) = track.ground.ray(Vec3::new(p.x, p.y + 500.0, p.z), Vec3::NEG_Y, 3000.0) {
                car.0.place(h.point, yaw);
            }
            return;
        }
        let fwd = (car.0.rotation * Vec3::NEG_Z).reject_from(Vec3::Y).normalize_or(Vec3::NEG_Z);
        let step = fwd * speed * time.delta_secs();
        car.0.position += step;
        car.0.prev_position += step;
        return;
    }
    let origin = *start.get_or_insert(car.0.position);
    // Same yaw convention as Vehicle::place (forward = -Z at yaw 0).
    let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos());
    let p = origin + forward * speed * t;
    if let Some(h) = track.ground.ray(Vec3::new(p.x, p.y + 500.0, p.z), Vec3::NEG_Y, 3000.0) {
        car.0.place(h.point, yaw);
    }
}

/// `FH1_P2_BURST`: consecutive screenshots (see the module docs).
fn burst(mut commands: Commands, time: Res<Time<Real>>, mut n: Local<u32>, mut exit: MessageWriter<AppExit>, mut done_at: Local<Option<f32>>, lamp_ab: Option<Res<fh1_render::car::LampSortAb>>) {
    let spec = std::env::var("FH1_P2_BURST").unwrap_or_default();
    let mut it = spec.splitn(3, ',');
    let start: f32 = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(30.0);
    let frames: u32 = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(20);
    let dir = std::path::PathBuf::from(it.next().unwrap_or("burst").trim());
    let t = time.elapsed_secs();
    if let Some(at) = *done_at {
        // Give the last screenshots time to reach the disk.
        if t - at > 3.0 {
            exit.write(AppExit::Success);
        }
        return;
    }
    if t < start {
        return;
    }
    if *n == 0 {
        let _ = std::fs::create_dir_all(&dir);
    }
    // With FH1_CARFX_LENS_SORT=ab the lamp-sort state is in the name (f012_on.png / f012_off.png).
    let tag = lamp_ab.filter(|a| a.ab).map_or(String::new(), |a| if a.on { "_on".into() } else { "_off".into() });
    let path = dir.join(format!("f{:03}{tag}.png", *n));
    commands.spawn(bevy::render::view::screenshot::Screenshot::primary_window()).observe(bevy::render::view::screenshot::save_to_disk(path));
    *n += 1;
    if *n >= frames {
        *done_at = Some(t);
        info!("p2 burst: {frames} frames saved to {}", dir.display());
    }
}
