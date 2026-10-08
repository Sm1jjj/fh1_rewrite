//! One background writer thread for every file the game writes while running (P9, 2026-10-08: log 140912 froze 31 s
//! with a game thread blocked in ZwWriteFile; a save or log append must never stall a frame). Jobs run in order on the
//! `fh1-file-writer` thread: append, or replace (written to `<path>.tmp`, then renamed: a crash never leaves half a
//! file). On `AppExit` the queue is drained (at most 3 s) before the process ends. `FH1_ASYNC_WRITES=0` = every write
//! in place on the calling thread (old).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use bevy::prelude::*;

enum Job {
    Append(PathBuf, Vec<u8>),
    Replace(PathBuf, Vec<u8>),
}

/// Jobs queued and not finished yet.
static PENDING: AtomicUsize = AtomicUsize::new(0);
/// After exit: write in place (the thread may never run again).
static EXITING: AtomicBool = AtomicBool::new(false);

fn async_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("FH1_ASYNC_WRITES").map_or(true, |v| v != "0"))
}

fn run(job: Job) {
    let res = match &job {
        Job::Append(p, b) => {
            use std::io::Write;
            std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| f.write_all(b))
        }
        Job::Replace(p, b) => {
            let mut tmp = p.clone().into_os_string();
            tmp.push(".tmp");
            let tmp = PathBuf::from(tmp);
            std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, p))
        }
    };
    if let Err(e) = res {
        let p = match &job {
            Job::Append(p, _) | Job::Replace(p, _) => p,
        };
        // eprintln, not warn!: logging from here must not wait on the log pipe either way.
        eprintln!("file writer: {}: {e}", p.display());
    }
}

fn submit(job: Job) {
    if !async_on() || EXITING.load(Ordering::Relaxed) {
        return run(job);
    }
    static TX: OnceLock<Option<Mutex<Sender<Job>>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("fh1-file-writer".into())
            .spawn(move || {
                for job in rx {
                    run(job);
                    PENDING.fetch_sub(1, Ordering::Relaxed);
                }
            })
            .ok()?;
        Some(Mutex::new(tx))
    });
    let Some(t) = tx.as_ref().and_then(|t| t.lock().ok()) else { return run(job) };
    PENDING.fetch_add(1, Ordering::Relaxed);
    if let Err(e) = t.send(job) {
        PENDING.fetch_sub(1, Ordering::Relaxed);
        run(e.0);
    }
}

/// Append `bytes` to `path` (created if missing), off the calling thread.
pub fn append(path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
    submit(Job::Append(path.into(), bytes.into()));
}

/// Replace `path` with `bytes` (via `<path>.tmp` + rename), off the calling thread.
pub fn replace(path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
    submit(Job::Replace(path.into(), bytes.into()));
}

/// Wait (at most `timeout`) until every queued write has finished; later writes run in place.
pub fn flush(timeout: Duration) {
    let end = Instant::now() + timeout;
    while PENDING.load(Ordering::Relaxed) > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Drain the queue and write everything after this in place (exit). At most 1 s (was 3 s: a stalled disk made quitting
/// hang); saves are a few KB.
pub fn finish() {
    if !EXITING.load(Ordering::Relaxed) {
        flush(Duration::from_secs(1));
        EXITING.store(true, Ordering::Relaxed);
    }
}

/// Quit without tearing the app down (2026-10-08, user: "quit via the menu froze"): after `App::run` returns, dropping the
/// world (~40k scenery entities, GPU resources, the render thread, task pools with streaming tasks in flight) took seconds.
/// The exit frame has already queued the saves and the perf summary; drain the writer (at most 1 s), give the log pipe's
/// writer thread 100 ms, then end the process. `FH1_FAST_EXIT=0` = return normally (old).
pub fn exit_now(code: AppExit) -> AppExit {
    if std::env::var("FH1_FAST_EXIT").is_ok_and(|v| v == "0") {
        return code;
    }
    finish();
    std::thread::sleep(Duration::from_millis(100));
    let status = match code {
        AppExit::Success => 0,
        AppExit::Error(n) => n.get() as i32,
    };
    std::process::exit(status)
}

/// Drain the queue when the app exits (registered by PerfPlugin, always on). Writes queued later in the exit frame run
/// in place.
fn flush_on_exit(mut exit: MessageReader<AppExit>) {
    if exit.read().next().is_some() {
        finish();
    }
}

pub(super) fn plugin(app: &mut App) {
    app.add_systems(Last, flush_on_exit);
}
