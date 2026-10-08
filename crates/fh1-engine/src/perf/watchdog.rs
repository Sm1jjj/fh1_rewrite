//! Stall watchdog (user 2026-10-07: 28 s and 41 s freezes mid-drive at ~0 % CPU, inside a different phase each time).
//! The main / render phase stamps (hitch.rs) also beat atomics here; a thread checks every 50 ms and, for every main
//! frame that takes over [`STALL_MS`], writes to `<perf_logs>/<stamp>_stalls.txt`: when, how long, the main phase it was
//! stuck in, whether the render thread kept running meanwhile, whether the window had focus, the process's I/O, page
//! faults and CPU over the stall, and stack samples of every thread at [`SAMPLE_AT`] into it (stack.rs).
//! Main stuck + render running = something blocks the main thread; both stuck = the whole process (console, OS, driver).
//! The loop itself never symbolises: a report is named and written on its own thread, so a slow PDB load can't delay
//! the end of a stall being seen (it did on 2026-10-07: a 3.6 s hitch was reported as 9.9 s, 25 s late).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::PrimaryWindow;

const STALL_MS: u64 = 1000;

/// Systems currently inside a [`watch`] guard: (name, start ms). The watchdog lists those running when a stall starts.
static ACTIVE: std::sync::Mutex<Vec<(&'static str, u64)>> = std::sync::Mutex::new(Vec::new());

/// Marks `name` as running until the guard drops (for systems that touch other threads, files or locks).
pub(crate) struct Watch(&'static str);

pub(crate) fn watch(name: &'static str) -> Watch {
    if let Ok(mut a) = ACTIVE.lock() {
        a.push((name, now_ms()));
    }
    Watch(name)
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Ok(mut a) = ACTIVE.lock() {
            if let Some(i) = a.iter().rposition(|(n, _)| *n == self.0) {
                a.swap_remove(i);
            }
        }
    }
}

fn running_long(now: u64) -> String {
    let a = ACTIVE.lock().map(|a| a.clone()).unwrap_or_default();
    let mut v: Vec<String> = a.iter().filter(|(_, t)| now.saturating_sub(*t) > 500).map(|(n, t)| format!("{n} ({:.1} s)", (now - t) as f64 / 1000.0)).collect();
    v.extend(long_systems(now).into_iter().map(|s| format!("system {s}")));
    if v.is_empty() { "none of the watched systems".into() } else { v.join(", ") }
}

static START: OnceLock<Instant> = OnceLock::new();
static MAIN_BEAT: AtomicU64 = AtomicU64::new(0);
static MAIN_PHASE: AtomicU32 = AtomicU32::new(0);
static RENDER_BEAT: AtomicU64 = AtomicU64::new(0);
static RENDER_PHASE: AtomicU32 = AtomicU32::new(0);
static FOCUSED: AtomicBool = AtomicBool::new(true);

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Called by hitch.rs's main stamps (0 First, 1 PreUpdate, 2 Update, 3 PostUpdate, 4 Last).
pub(crate) fn main_beat(phase: u32) {
    MAIN_PHASE.store(phase, Ordering::Relaxed);
    MAIN_BEAT.store(now_ms(), Ordering::Relaxed);
}

/// Called by hitch.rs's render stamps (0 Extract .. 8 PostCleanup).
pub(crate) fn render_beat(phase: u32) {
    RENDER_PHASE.store(phase, Ordering::Relaxed);
    RENDER_BEAT.store(now_ms(), Ordering::Relaxed);
}

const MAIN_NAMES: [&str; 5] = ["First (waiting on render / present before it)", "PreUpdate", "Update", "PostUpdate", "Last"];
const RENDER_NAMES: [&str; 9] = ["Extract", "PrepareAssets", "Specialize", "Queue", "PhaseSort", "Prepare", "Render", "Cleanup", "PostCleanup"];

pub(crate) fn plugin(app: &mut App, file: PathBuf) {
    now_ms();
    super::stack::remember_main();
    super::stack::warm_up();
    app.add_systems(Update, track_focus);
    let _ = std::thread::Builder::new().name("fh1-stall-watchdog".into()).spawn(move || watch_loop(file));
}

fn track_focus(windows: Query<&Window, With<PrimaryWindow>>) {
    if let Ok(w) = windows.single() {
        FOCUSED.store(w.focused, Ordering::Relaxed);
    }
}

/// Stack samples this far into a stall (ms).
const SAMPLE_AT: [u64; 3] = [1500, 6000, 20000];

/// A stall in progress.
struct Stall {
    start: u64,
    phase: u32,
    render0: u64,
    focused0: bool,
    focused_all: bool,
    counters0: super::stack::Counters,
    /// (ms into the stall, systems running then, every thread's stack).
    samples: Vec<(u64, String, Vec<super::stack::ThreadStack>)>,
}

fn watch_loop(file: PathBuf) {
    let mut stall: Option<Stall> = None;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let (now, beat) = (now_ms(), MAIN_BEAT.load(Ordering::Relaxed));
        if beat == 0 {
            continue;
        }
        let focused = FOCUSED.load(Ordering::Relaxed);
        if let Some(s) = stall.take_if(|s| beat != s.start) {
            report(&file, s, beat);
            continue;
        }
        match &mut stall {
            None if now.saturating_sub(beat) > STALL_MS => {
                stall = Some(Stall {
                    start: beat,
                    phase: MAIN_PHASE.load(Ordering::Relaxed),
                    render0: RENDER_BEAT.load(Ordering::Relaxed),
                    focused0: focused,
                    focused_all: focused,
                    counters0: super::stack::counters(),
                    samples: Vec::new(),
                });
            }
            Some(s) => {
                s.focused_all &= focused;
                let into = now.saturating_sub(s.start);
                if SAMPLE_AT.get(s.samples.len()).is_some_and(|&at| into > at) {
                    // Raw return addresses only: naming them waits for the report thread.
                    s.samples.push((into, running_long(now), super::stack::sample()));
                }
            }
            None => {}
        }
    }
}

/// The stall ended at `beat`: one warning line now, the named stacks from a report thread.
fn report(file: &std::path::Path, s: Stall, beat: u64) {
    let render = RENDER_BEAT.load(Ordering::Relaxed);
    let secs = (beat - s.start) as f64 / 1000.0;
    let header = format!(
        "t {:.2} s  stall {:.2} s  main stuck after: {}  render thread: {} (now in {})  window focused: start {} / throughout {}  watched systems still running: {}\n    {}\n",
        s.start as f64 / 1000.0,
        secs,
        MAIN_NAMES.get(s.phase as usize).unwrap_or(&"?"),
        if render > s.render0 + 200 { "kept running" } else { "stuck too" },
        RENDER_NAMES.get(RENDER_PHASE.load(Ordering::Relaxed) as usize).unwrap_or(&"?"),
        s.focused0,
        s.focused_all,
        s.samples.first().map_or("(not sampled: shorter than 1.5 s)", |x| x.1.as_str()),
        // The counters were first read when the stall was noticed, STALL_MS in.
        super::stack::counters().delta(&s.counters0, secs - STALL_MS as f64 / 1000.0),
    );
    warn!("stall watchdog: {}", header.trim_end());
    let file = file.to_owned();
    let _ = std::thread::Builder::new().name("fh1-stall-report".into()).spawn(move || write_report(&file, header, s.samples));
}

/// Names the sampled frames and appends the report. One report at a time, so two stalls never interleave.
fn write_report(file: &std::path::Path, header: String, samples: Vec<(u64, String, Vec<super::stack::ThreadStack>)>) {
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = header;
    let mut cpu_before: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    for (at, running, threads) in &samples {
        out += &format!("    sample {:.1} s into the stall  (running: {running})\n", *at as f64 / 1000.0);
        // Identical stacks (idle pool workers) are listed once.
        let mut seen: Vec<(&[u64], u32)> = Vec::new();
        for t in threads {
            let cpu = match cpu_before.insert(t.tid, t.cpu_100ns) {
                Some(b) => format!("cpu +{:.2} s since the last sample", t.cpu_100ns.saturating_sub(b) as f64 / 1e7),
                None => format!("cpu {:.1} s total", t.cpu_100ns as f64 / 1e7),
            };
            let name = if t.main { "MAIN" } else if t.name.is_empty() { "(unnamed)" } else { t.name.as_str() };
            if let Some((_, first)) = seen.iter().find(|(pcs, _)| !t.main && !pcs.is_empty() && *pcs == t.pcs.as_slice()) {
                out += &format!("      thread {} {name}  {cpu}: same stack as thread {first}\n", t.tid);
                continue;
            }
            seen.push((&t.pcs, t.tid));
            out += &format!("      thread {} {name}  {cpu}:\n", t.tid);
            for &pc in &t.pcs {
                out += &format!("        {}\n", super::stack::symbol(pc));
            }
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = f.write_all(out.as_bytes());
    }
}

/// Every Bevy system run is a tracing span named "system" with its `name` (bevy `trace` feature). This layer keeps, per
/// span name, whether it is running and since when (a stall names the system that blocks) and, with the gameplay
/// recorder on, its summed time ([`take_span_totals`]): systems of both worlds (Bevy 0.19's render passes are systems too,
/// `RenderContextState::apply` = "pass <system>"), `main_render_schedule` and `present_frames`.
/// Hot path without global locks (2026-10-08: the first version took three global mutexes, cloned a String and hashed a
/// name on every enter/exit of every system on ~12 threads; that contention showed as a ~0.12 ms floor on near-empty
/// systems): each span carries its name's shared accumulator (atomics) in its registry extensions, and its own start
/// instant. The global map is locked only when a span is created.
mod spans {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use bevy::log::tracing::field::{Field, Visit};
    use bevy::log::tracing::{span, Subscriber};
    use bevy::log::tracing_subscriber::layer::{Context, Layer};
    use bevy::log::tracing_subscriber::registry::LookupSpan;

    /// One per span name, shared by every span with that name.
    pub(super) struct Acc {
        pub(super) name: String,
        pub(super) ns: AtomicU64,
        pub(super) runs: AtomicU32,
        /// Watchdog ms (`super::now_ms`) when entered; 0 = not inside.
        pub(super) entered_ms: AtomicU64,
    }

    static ACCS: Mutex<Option<HashMap<String, Arc<Acc>>>> = Mutex::new(None);
    pub(super) static ACCUM: AtomicBool = AtomicBool::new(false);

    struct Tracked(Arc<Acc>);
    struct Start(Instant);

    fn acc_for(name: String) -> Arc<Acc> {
        let mut g = ACCS.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(HashMap::new)
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Acc { name, ns: AtomicU64::new(0), runs: AtomicU32::new(0), entered_ms: AtomicU64::new(0) }))
            .clone()
    }

    pub(super) fn all() -> Vec<Arc<Acc>> {
        ACCS.lock().ok().and_then(|g| g.as_ref().map(|m| m.values().cloned().collect())).unwrap_or_default()
    }

    pub(super) struct SystemSpans;

    struct NameField(Option<String>, &'static str);

    impl Visit for NameField {
        fn record_str(&mut self, f: &Field, v: &str) {
            if f.name() == self.1 {
                self.0 = Some(v.to_owned());
            }
        }
        fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
            if f.name() == self.1 && self.0.is_none() {
                self.0 = Some(format!("{v:?}"));
            }
        }
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for SystemSpans {
        fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
            let name = match attrs.metadata().name() {
                "system" => {
                    let mut v = NameField(None, "name");
                    attrs.record(&mut v);
                    v.0
                }
                // Per-frame spans: tracked only for the totals.
                "RenderContextState::apply" if ACCUM.load(Ordering::Relaxed) => {
                    let mut v = NameField(None, "system");
                    attrs.record(&mut v);
                    v.0.map(|s| format!("pass {s}"))
                }
                n @ ("present_frames" | "main_render_schedule") if ACCUM.load(Ordering::Relaxed) => Some(n.to_owned()),
                _ => None,
            };
            if let (Some(n), Some(span)) = (name, ctx.span(id)) {
                span.extensions_mut().insert(Tracked(acc_for(n)));
            }
        }
        fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
            let Some(span) = ctx.span(id) else { return };
            let acc = span.extensions().get::<Tracked>().map(|t| t.0.clone());
            if let Some(acc) = acc {
                acc.entered_ms.store(super::now_ms().max(1), Ordering::Relaxed);
                if ACCUM.load(Ordering::Relaxed) {
                    span.extensions_mut().replace(Start(Instant::now()));
                }
            }
        }
        fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
            let Some(span) = ctx.span(id) else { return };
            let ext = span.extensions();
            let Some(t) = ext.get::<Tracked>() else { return };
            t.0.entered_ms.store(0, Ordering::Relaxed);
            if ACCUM.load(Ordering::Relaxed) {
                if let Some(s) = ext.get::<Start>() {
                    t.0.ns.fetch_add(s.0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    t.0.runs.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// The recorder turns the per-span totals on.
pub(crate) fn set_span_totals(on: bool) {
    spans::ACCUM.store(on, Ordering::Relaxed);
}

/// (name, summed ms, runs) per tracked span name since the last call, slowest first.
pub(crate) fn take_span_totals() -> Vec<(String, f64, u32)> {
    let mut v: Vec<(String, f64, u32)> = spans::all()
        .iter()
        .filter_map(|a| {
            let runs = a.runs.swap(0, Ordering::Relaxed);
            let ns = a.ns.swap(0, Ordering::Relaxed);
            (runs > 0).then(|| (a.name.clone(), ns as f64 / 1e6, runs))
        })
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
}

/// LogPlugin `custom_layer`: the system-span tracker above.
pub fn system_span_layer(_app: &mut App) -> Option<bevy::log::BoxedLayer> {
    Some(Box::new(spans::SystemSpans))
}

/// Systems inside their span for over 0.5 s at `now` (empty without the bevy `trace` feature).
fn long_systems(now: u64) -> Vec<String> {
    spans::all()
        .iter()
        .filter_map(|a| {
            let t = a.entered_ms.load(Ordering::Relaxed);
            (t > 0 && now.saturating_sub(t) > 500).then(|| format!("{} ({:.1} s)", a.name, (now - t) as f64 / 1000.0))
        })
        .collect()
}
