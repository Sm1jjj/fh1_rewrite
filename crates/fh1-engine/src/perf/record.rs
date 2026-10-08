//! Gameplay performance recorder (`FH1_PERF_REC=1`, set by remaster.bat): the user plays, this writes what the frame did.
//!
//! - `<data>/perf_logs/<UTC date-time>_<renderer>.csv`: one `sec` row per real second (map, car, renderer, position,
//!   speed, frame stats, render-thread and main-thread splits, CPU / RAM / GPU / VRAM from diag.rs's sampler, entity and
//!   visible-mesh counts, streaming gauges and counters), and one `hitch` row per frame over [`HITCH_MS`] with that
//!   frame's main split, the latest render-thread split and the streaming activity of the moment.
//! - `<same name>_summary.txt`, rewritten every [`SUMMARY_EVERY`] s and at exit: per (map, car, renderer) segment the
//!   time, fps, frame p50 / p99 / max, the share of slow frames, and the worst hitches.
//!
//! Cheap by design: per frame it only pushes a few numbers into memory; rows are buffered and appended to the file every
//! [`FLUSH_EVERY`] s. Read a log with `tools/perf_report.py <csv>`.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use bevy::prelude::*;

use super::hitch::{LastSplit, RenderStamps};
use crate::diag::Diag;
use crate::scenery::{Scenery, STREAMED};
use crate::track::Track;
use crate::Car;

/// A frame this long (ms) gets its own `hitch` row.
const HITCH_MS: f32 = 50.0;
const FLUSH_EVERY: f64 = 5.0;
const SUMMARY_EVERY: f64 = 30.0;

/// Log file writes, done on a writer thread: the main thread must never wait on the disk (per-frame / periodic writes
/// from systems froze the game for 10-26 s in the user's 2026-10-07 runs). At exit (`Drop`) they are written in place.
enum FileJob {
    Append(PathBuf, String),
    Write(PathBuf, String),
}

fn run_job(job: FileJob) {
    match job {
        FileJob::Append(p, s) => {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
                let _ = f.write_all(s.as_bytes());
            }
        }
        FileJob::Write(p, s) => {
            let _ = std::fs::write(&p, s);
        }
    }
}

static EXITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn file_job(job: FileJob) {
    static TX: std::sync::OnceLock<Option<std::sync::Mutex<std::sync::mpsc::Sender<FileJob>>>> = std::sync::OnceLock::new();
    if EXITING.load(std::sync::atomic::Ordering::Relaxed) {
        return run_job(job);
    }
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<FileJob>();
        std::thread::Builder::new().name("fh1-perf-log".into()).spawn(move || rx.into_iter().for_each(run_job)).ok()?;
        Some(std::sync::Mutex::new(tx))
    });
    match tx.as_ref().and_then(|t| t.lock().ok()) {
        Some(t) => {
            if let Err(e) = t.send(job) {
                run_job(e.0);
            }
        }
        None => run_job(job),
    }
}

pub fn on() -> bool {
    std::env::var("FH1_PERF_REC").is_ok_and(|v| v == "1")
}

pub fn plugin(app: &mut App) {
    if !on() {
        return;
    }
    let Some(rec) = Recorder::open() else { return };
    info!("perf recorder: writing {}", rec.csv.display());
    let stalls = rec.csv.with_file_name(rec.csv.file_stem().map_or_else(|| "stalls".into(), |s| format!("{}_stalls.txt", s.to_string_lossy())));
    super::watchdog::plugin(app, stalls);
    super::draws::plugin(app);
    super::watchdog::set_span_totals(true);
    app.insert_resource(rec).add_systems(Last, record);
}

const HEADER: &str = "kind,t_s,utc,map,car,renderer,state,x,y,z,speed_kmh,fps,frame_mean,frame_p50,frame_p99,frame_max,frames_over_33ms,\
rt_extract,rt_prepare_assets,rt_specialize,rt_queue,rt_prepare,rt_render,rt_total,main_wait,main_to_preupdate,main_to_update,\
main_to_postupdate,main_to_last,main_busy,cpu_process,cpu_system,ram_process_mb,ram_system_mb,gpu_util,vram_mb,vram_total_mb,gpu_temp,\
entities,meshes,meshes_visible,zones_loaded,zones_pending,prop_tiles,prop_pending,prop_placing,zone_loads,prop_tile_loads,\
scenery_spawned,views_3d,draws_opaque,draws_mask,draws_transparent,views_shadow,draws_shadow,draws_unbatched,draws_total,present_mode,render_exec,hitch_ms,mesh_props,mesh_props_visible,mesh_zones,mesh_zones_visible,mesh_crowd,mesh_grass,mesh_casters,mesh_other,mesh_other_visible,prop_levels_deferred,sched_gap_ms";

/// One (map, car, renderer) stretch of play.
#[derive(Default)]
struct Segment {
    seconds: f64,
    frames: Vec<f32>,
    /// (t, ms, attribution) of the hitches.
    hitches: Vec<(f64, f32, String)>,
}

#[derive(Resource)]
pub struct Recorder {
    csv: PathBuf,
    summary: PathBuf,
    buf: String,
    renderer: &'static str,
    t: f64,
    last_flush: f64,
    last_summary: f64,
    /// Current second's frame times (ms) and render / main split sums.
    frames: Vec<f32>,
    rt_sum: [f64; 8],
    main_sum: [f64; 5],
    sec_start: f64,
    /// "play", or the last non-play state seen this second.
    sec_state: &'static str,
    streamed_prev: [u32; 3],
    segments: Vec<((String, String, &'static str), Segment)>,
    /// Per-system time (watchdog span totals): `<stem>_systems.txt` gets each second's top list; the summary the
    /// session's (name -> summed ms, runs) over `span_frames` frames.
    systems: PathBuf,
    span_totals: HashMap<String, (f64, u32)>,
    span_frames: u64,
}

impl Recorder {
    fn open() -> Option<Self> {
        let mut args = std::env::args();
        let mut data = PathBuf::from("data");
        while let Some(a) = args.next() {
            if a == "--data" {
                data = args.next()?.into();
            }
        }
        let dir = data.join("perf_logs");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("perf recorder: can't create {}: {e}", dir.display());
            return None;
        }
        let renderer = if fh1_remaster::enabled() { "remaster" } else { "faithful" };
        let stem = format!("{}_{renderer}", utc_stamp().replace([':', '-'], "").replace('T', "_"));
        let csv = dir.join(format!("{stem}.csv"));
        let summary = dir.join(format!("{stem}_summary.txt"));
        let systems = dir.join(format!("{stem}_systems.txt"));
        std::fs::write(&csv, format!("{HEADER}\n")).ok()?;
        Some(Self {
            csv,
            summary,
            buf: String::new(),
            renderer,
            t: 0.0,
            last_flush: 0.0,
            last_summary: 0.0,
            frames: Vec::new(),
            rt_sum: [0.0; 8],
            main_sum: [0.0; 5],
            sec_start: 0.0,
            sec_state: "play",
            streamed_prev: [0; 3],
            segments: Vec::new(),
            systems,
            span_totals: HashMap::new(),
            span_frames: 0,
        })
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        file_job(FileJob::Append(self.csv.clone(), std::mem::take(&mut self.buf)));
    }

    fn segment(&mut self, key: (String, String, &'static str)) -> &mut Segment {
        // Consecutive stretches with the same key extend the last segment; returning to an earlier car/map starts a new one.
        if self.segments.last().is_none_or(|(k, _)| *k != key) {
            self.segments.push((key, Segment::default()));
        }
        &mut self.segments.last_mut().unwrap().1
    }

    fn write_summary(&self) {
        let mut out = format!("FH1 gameplay perf summary ({} renderer)\nlog: {}\n\n", self.renderer, self.csv.display());
        out += "segment (map / car / renderer)                      time s   fps   p50 ms  p99 ms  max ms  >33ms  >50ms  hitches\n";
        let mut all: Vec<(f64, f32, String, String)> = Vec::new();
        for ((map, car, r), s) in &self.segments {
            let mut v = s.frames.clone();
            if v.is_empty() {
                continue;
            }
            v.sort_by(f32::total_cmp);
            let pct = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
            let mean = v.iter().sum::<f32>() / v.len() as f32;
            let over = |ms: f32| v.iter().filter(|&&x| x > ms).count() as f32 * 100.0 / v.len() as f32;
            out += &format!(
                "{:<50} {:>8.0} {:>5.1} {:>7.1} {:>7.1} {:>7.1} {:>5.1}% {:>5.1}% {:>7}\n",
                format!("{map} / {car} / {r}"),
                s.seconds,
                1000.0 / mean.max(0.001),
                pct(0.5),
                pct(0.99),
                v[v.len() - 1],
                over(33.4),
                over(50.0),
                s.hitches.len()
            );
            all.extend(s.hitches.iter().map(|(t, ms, a)| (*t, *ms, format!("{map} / {car}"), a.clone())));
        }
        all.sort_by(|a, b| b.1.total_cmp(&a.1));
        out += "\nworst hitches (t s, ms, where, attribution):\n";
        for (t, ms, w, a) in all.iter().take(15) {
            out += &format!("  {t:7.1}  {ms:6.0}  {w}  {a}\n");
        }
        if self.span_frames > 0 {
            // Systems of both worlds run in parallel: these sum per name, they don't add up to the frame.
            let mut v: Vec<(&String, &(f64, u32))> = self.span_totals.iter().collect();
            v.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));
            out += &format!("\nslowest systems / render passes (ms per frame over {} frames; 'pass X' = a render pass, main_render_schedule = the render world's whole run):\n", self.span_frames);
            for (name, (ms, runs)) in v.into_iter().take(60) {
                out += &format!("  {:7.3}  {:>8} runs  {name}\n", ms / self.span_frames as f64, runs);
            }
        }
        file_job(FileJob::Write(self.summary.clone(), out));
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Exit: write in place (the writer thread may not get to run again).
        EXITING.store(true, std::sync::atomic::Ordering::Relaxed);
        self.flush();
        self.write_summary();
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    mut rec: ResMut<Recorder>,
    time: Res<Time<Real>>,
    split: Option<Res<LastSplit>>,
    stamps: Option<Res<RenderStamps>>,
    diag: Option<Res<Diag>>,
    track: Option<Res<Track>>,
    cars: Query<&Car>,
    scenery: Option<Res<Scenery>>,
    meshes: Query<(&ViewVisibility, MeshCat), With<Mesh3d>>,
    entities: Query<()>,
    menu: Option<Res<crate::ui::Menu>>,
    mut exit: MessageReader<AppExit>,
) {
    let r = &mut *rec;
    let dt = time.delta_secs();
    let ms = dt * 1000.0;
    r.t = time.elapsed_secs_f64();
    let main = split.map(|s| s.0).unwrap_or_default();
    let rt = stamps.and_then(|s| s.0.lock().ok().map(|f| f.last)).unwrap_or_default();
    // Loading covers / launch screen and the pause menu are logged (state column) but kept out of the segment stats
    // and the hitch list; so are the first frames (window creation).
    let state = if crate::ui::loading::blocking() || r.t <= 2.0 {
        "loading"
    } else if menu.as_ref().is_some_and(|m| m.open) {
        "menu"
    } else {
        "play"
    };
    if state != "play" {
        r.sec_state = state;
    }
    let counting = state == "play";
    {
        r.frames.push(ms);
        for k in 0..8 {
            r.rt_sum[k] += rt[k] as f64;
        }
        for k in 0..5 {
            r.main_sum[k] += main[k] as f64;
        }
    }
    let map = track.as_ref().map_or_else(|| "-".to_owned(), |t| t.id.clone());
    let car = cars.iter().next();
    let car_name = car.map_or_else(|| "-".to_owned(), |c| c.0.data.media_name.clone());
    let key = (map.clone(), car_name.clone(), r.renderer);
    let streamed: [u32; 3] = std::array::from_fn(|k| STREAMED[k].load(Ordering::Relaxed));
    let gauges = scenery.as_ref().map(|s| s.stream_gauges()).unwrap_or_default();
    if counting {
        let seg = r.segment(key.clone());
        seg.frames.push(ms);
        seg.seconds += dt as f64;
        if ms > HITCH_MS {
            let names = ["Extract", "PrepareAssets", "Specialize", "Queue", "PhaseSort", "Prepare", "Render", "Cleanup"];
            let mut parts: Vec<(f32, &str)> = rt.iter().copied().zip(names).collect();
            parts.extend(main.iter().copied().zip(["main wait", "main ->PreUpdate", "main ->Update", "main ->PostUpdate", "main ->Last"]));
            parts.sort_by(|a, b| b.0.total_cmp(&a.0));
            let ds: Vec<u32> = (0..3).map(|k| streamed[k] - r.streamed_prev[k]).collect();
            let (pos, kmh) = car.map_or((Vec3::ZERO, 0.0), |c| (c.0.position, c.0.velocity.length() * 3.6));
            let attr = format!(
                "top: {}; streaming this second: {} zone loads, {} prop tiles, {} spawned",
                parts.iter().take(3).map(|(v, n)| format!("{n} {v:.0}")).collect::<Vec<_>>().join(", "),
                ds[0],
                ds[1],
                ds[2]
            );
            let t = r.t;
            r.segment(key.clone()).hitches.push((t, ms, attr));
            let mut row = Row::new("hitch", r.t, &map, &car_name, r.renderer, state, pos, kmh);
            row.set("frame_max", ms, 1);
            for (k, c) in [(0, "rt_extract"), (1, "rt_prepare_assets"), (2, "rt_specialize"), (3, "rt_queue"), (5, "rt_prepare"), (6, "rt_render")] {
                row.set(c, rt[k], 2);
            }
            row.set("rt_total", rt[..8].iter().sum::<f32>(), 2);
            row.main(&main);
            row.gauges(&gauges, &ds);
            row.set("hitch_ms", ms, 1);
            r.buf += &row.line();
        }
    }
    let quitting = exit.read().next().is_some();
    if r.t - r.sec_start >= 1.0 || quitting {
        let n = r.frames.len();
        // A second with any loading / menu frame is labelled with that state.
        let sec_state = r.sec_state;
        if n > 0 {
            let mut v = r.frames.clone();
            v.sort_by(f32::total_cmp);
            let mean = v.iter().sum::<f32>() / n as f32;
            let pct = |p: f64| v[((n - 1) as f64 * p) as usize];
            let rtm: Vec<f32> = r.rt_sum.iter().map(|x| (x / n as f64) as f32).collect();
            let mm: Vec<f32> = r.main_sum.iter().map(|x| (x / n as f64) as f32).collect();
            let s = diag.as_ref().and_then(|d| d.shared.lock().ok().map(|s| s.clone())).unwrap_or_default();
            let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
            let (pos, kmh) = car.map_or((Vec3::ZERO, 0.0), |c| (c.0.position, c.0.velocity.length() * 3.6));
            // Per-category counts (P8): [props, props visible, zones, zones visible, crowd, grass, casters, other, other visible].
            let mut cat = [0usize; 9];
            let (mut total, mut visible) = (0usize, 0usize);
            for (vis, (prop, zone, crowd, walker, figure, skinned, grass, caster)) in &meshes {
                let v = vis.get() as usize;
                total += 1;
                visible += v;
                let (k, kv) = if prop {
                    (0, Some(1))
                } else if zone {
                    (2, Some(3))
                } else if crowd || walker || figure || skinned {
                    (4, None)
                } else if grass {
                    (5, None)
                } else if caster {
                    (6, None)
                } else {
                    (7, Some(8))
                };
                cat[k] += 1;
                if let Some(kv) = kv {
                    cat[kv] += v;
                }
            }
            let ds: Vec<u32> = (0..3).map(|k| streamed[k] - r.streamed_prev[k]).collect();
            let mut row = Row::new("sec", r.t, &map, &car_name, r.renderer, sec_state, pos, kmh);
            row.set("fps", 1000.0 / mean.max(0.001), 1);
            row.set("frame_mean", mean, 2);
            row.set("frame_p50", pct(0.5), 2);
            row.set("frame_p99", pct(0.99), 2);
            row.set("frame_max", v[n - 1], 2);
            row.text("frames_over_33ms", v.iter().filter(|&&x| x > 33.4).count().to_string());
            for (k, c) in [(0, "rt_extract"), (1, "rt_prepare_assets"), (2, "rt_specialize"), (3, "rt_queue"), (5, "rt_prepare"), (6, "rt_render")] {
                row.set(c, rtm[k], 2);
            }
            row.set("rt_total", rtm.iter().sum::<f32>(), 2);
            row.main(&mm);
            row.set("cpu_process", s.cpu_process, 1);
            row.set("cpu_system", s.cpu_total, 1);
            row.set("ram_process_mb", mb(s.ram_process) as f32, 0);
            row.set("ram_system_mb", mb(s.ram_used) as f32, 0);
            if let Some(g) = &s.gpu {
                row.text("gpu_util", g.util.to_string());
                row.set("vram_mb", mb(g.vram_used) as f32, 0);
                row.set("vram_total_mb", mb(g.vram_total) as f32, 0);
                row.text("gpu_temp", g.temp.to_string());
            }
            row.text("entities", entities.iter().count().to_string());
            row.text("meshes", total.to_string());
            row.text("meshes_visible", visible.to_string());
            for (c, n) in ["mesh_props", "mesh_props_visible", "mesh_zones", "mesh_zones_visible", "mesh_crowd", "mesh_grass", "mesh_casters", "mesh_other", "mesh_other_visible"].iter().zip(cat) {
                row.text(c, n.to_string());
            }
            row.text("sched_gap_ms", super::watchdog::take_sched_gap_ms().to_string());
            row.text("prop_levels_deferred", scenery.as_ref().map_or(0, |s| s.deferred_prop_levels()).to_string());
            // Last frame's render-phase draw counts (perf/draws.rs).
            for (name, c) in super::draws::NAMES.iter().zip(&super::draws::COUNTS) {
                row.text(name, c.load(std::sync::atomic::Ordering::Relaxed).to_string());
            }
            row.text("present_mode", super::present::current_name().to_owned());
            row.text("render_exec", super::exec::current_name().to_owned());
            row.gauges(&gauges, &ds);
            r.buf += &row.line();
            // Per-system time this second (watchdog span totals).
            let spans = super::watchdog::take_span_totals();
            let mut line = format!("t {:.1} s  {sec_state}  frame {mean:.1} ms:", r.t);
            for (name, ms, _) in spans.iter().take(12) {
                line += &format!("  {name} {:.2}", ms / n as f64);
            }
            for (name, ms, runs) in spans {
                let e = r.span_totals.entry(name).or_default();
                e.0 += ms;
                e.1 += runs;
            }
            r.span_frames += n as u64;
            file_job(FileJob::Append(r.systems.clone(), line + "\n"));
        }
        r.sec_state = "play";
        r.frames.clear();
        r.rt_sum = [0.0; 8];
        r.main_sum = [0.0; 5];
        r.sec_start = r.t;
        r.streamed_prev = streamed;
    }
    if r.t - r.last_flush >= FLUSH_EVERY || quitting {
        r.last_flush = r.t;
        r.flush();
    }
    if r.t - r.last_summary >= SUMMARY_EVERY || quitting {
        r.last_summary = r.t;
        r.write_summary();
    }
}

/// Mesh categories for the CSV (P8): scenery props / zone models (scenery.rs tags), crowd (cards, walkers, GPU figures,
/// skinned), grass, shadow caster proxies; the rest is "other" (tiles, cars, anim objects, effects, UI).
type MeshCat = (
    Has<crate::scenery::PropMesh>,
    Has<crate::scenery::ZoneMesh>,
    Has<MeshMaterial3d<crate::crowd::CrowdMaterial>>,
    Has<MeshMaterial3d<crate::crowd::WalkerMaterial>>,
    Has<crate::crowd::GpuFigure>,
    Has<bevy::mesh::skinning::SkinnedMesh>,
    Has<MeshMaterial3d<crate::grass::GrassMaterial>>,
    Has<fh1_render::shadow::CasterProxy>,
);

/// One CSV row, filled by column name (unknown names panic in debug: the header is the single source of the columns).
struct Row(Vec<String>);

impl Row {
    fn new(kind: &str, t: f64, map: &str, car: &str, renderer: &str, state: &str, pos: Vec3, kmh: f32) -> Self {
        let mut r = Row(vec![String::new(); HEADER.split(',').count()]);
        r.text("kind", kind.to_owned());
        r.text("t_s", format!("{t:.2}"));
        r.text("utc", utc_stamp());
        r.text("map", csv_field(map));
        r.text("car", csv_field(car));
        r.text("renderer", renderer.to_owned());
        r.text("state", state.to_owned());
        r.set("x", pos.x, 1);
        r.set("y", pos.y, 1);
        r.set("z", pos.z, 1);
        r.set("speed_kmh", kmh, 1);
        r
    }

    fn text(&mut self, col: &str, v: String) {
        match HEADER.split(',').position(|c| c == col) {
            Some(i) => self.0[i] = v,
            None => debug_assert!(false, "perf recorder: no column {col}"),
        }
    }

    fn set(&mut self, col: &str, v: f32, decimals: usize) {
        self.text(col, format!("{v:.decimals$}"));
    }

    /// The main-thread split (5 intervals) and their busy sum (First .. Last).
    fn main(&mut self, m: &[f32]) {
        for (k, c) in ["main_wait", "main_to_preupdate", "main_to_update", "main_to_postupdate", "main_to_last"].iter().enumerate() {
            self.set(c, m[k], 2);
        }
        self.set("main_busy", m[1..5].iter().sum::<f32>(), 2);
    }

    fn gauges(&mut self, g: &[usize; 5], ds: &[u32]) {
        for (k, c) in ["zones_loaded", "zones_pending", "prop_tiles", "prop_pending", "prop_placing"].iter().enumerate() {
            self.text(c, g[k].to_string());
        }
        for (k, c) in ["zone_loads", "prop_tile_loads", "scenery_spawned"].iter().enumerate() {
            self.text(c, ds[k].to_string());
        }
    }

    fn line(&self) -> String {
        let mut s = self.0.join(",");
        s.push('\n');
        s
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SS` (civil-from-days, no date crate needed).
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_fill_known_columns() {
        let mut r = Row::new("sec", 1.0, "colorado", "VW_Corrado_95", "faithful", "play", Vec3::ZERO, 0.0);
        r.main(&[1.0; 5]);
        r.gauges(&[1; 5], &[1; 3]);
        for c in ["fps", "frame_mean", "frame_p50", "frame_p99", "frame_max", "rt_extract", "rt_prepare_assets", "rt_specialize", "rt_queue", "rt_prepare", "rt_render", "rt_total", "cpu_process", "cpu_system", "ram_process_mb", "ram_system_mb", "vram_mb", "vram_total_mb", "hitch_ms"] {
            r.set(c, 1.0, 1);
        }
        for c in ["frames_over_33ms", "gpu_util", "gpu_temp", "entities", "meshes", "meshes_visible"] {
            r.text(c, "1".into());
        }
        // Every column is written by some path.
        assert!(r.0.iter().all(|v| !v.is_empty()), "{:?}", HEADER.split(',').zip(&r.0).filter(|(_, v)| v.is_empty()).collect::<Vec<_>>());
        assert!(r.line().ends_with('\n'));
    }

    #[test]
    fn utc_format() {
        assert_eq!(utc_stamp().len(), 19);
    }
}
