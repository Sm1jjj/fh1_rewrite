//! Read-through RAM cache for car files (fxcar streams, tex.json, DDS, ShaderSettings XML, the car shader library,
//! model/physics JSON). A car spawn read ~120 MB inside its systems, and the cockpit view read the same textures again:
//! the 2026-10-08 stall watchdog caught a 1.6 s frame in update_fx_cockpit -> CarTextures -> std::fs::read. With this,
//! [`prefetch_car`] runs on the car-load task and the systems find every file in RAM. The cache is LRU by bytes,
//! FH1_FILE_CACHE_MB (default 2048; RAM is plentiful). FH1_FILE_CACHE=0 = plain disk reads, as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

struct Cache {
    files: HashMap<PathBuf, (Arc<[u8]>, u64)>,
    bytes: usize,
    tick: u64,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !std::env::var("FH1_FILE_CACHE").is_ok_and(|v| v == "0"))
}

fn budget() -> usize {
    static MB: OnceLock<usize> = OnceLock::new();
    *MB.get_or_init(|| std::env::var("FH1_FILE_CACHE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(2048)) * 1024 * 1024
}

fn with<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(|| Cache { files: HashMap::new(), bytes: 0, tick: 0 }))
}

fn insert(path: &Path, data: Arc<[u8]>) {
    with(|c| {
        c.tick += 1;
        let tick = c.tick;
        if let Some((old, _)) = c.files.insert(path.to_owned(), (data.clone(), tick)) {
            c.bytes -= old.len();
        }
        c.bytes += data.len();
        // Evict least recently used until under budget (never the file just added).
        while c.bytes > budget() && c.files.len() > 1 {
            let Some(oldest) = c.files.iter().filter(|(p, _)| p.as_path() != path).min_by_key(|(_, (_, t))| *t).map(|(p, _)| p.clone()) else { break };
            if let Some((d, _)) = c.files.remove(&oldest) {
                c.bytes -= d.len();
            }
        }
    });
}

/// The file's bytes: from RAM when cached, else read (and cached).
pub fn read(path: &Path) -> Option<Arc<[u8]>> {
    if !enabled() {
        return std::fs::read(path).ok().map(Arc::from);
    }
    let hit = with(|c| {
        c.tick += 1;
        let tick = c.tick;
        c.files.get_mut(path).map(|(d, t)| {
            *t = tick;
            d.clone()
        })
    });
    if hit.is_some() {
        return hit;
    }
    let data: Arc<[u8]> = Arc::from(std::fs::read(path).ok()?);
    insert(path, data.clone());
    Some(data)
}

pub fn read_to_string(path: &Path) -> Option<String> {
    read(path).and_then(|b| String::from_utf8(b.to_vec()).ok())
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    read(path).and_then(|b| serde_json::from_slice(&b).ok())
}

/// Cached or present on disk.
pub fn exists(path: &Path) -> bool {
    (enabled() && with(|c| c.files.contains_key(path))) || path.exists()
}

/// Reads everything a car spawn will ask for into the cache. Blocking: call it on a task thread (the car-load task),
/// never in a system. `car` is the garage name (may be `../imported/<game>/cars/<media>`).
pub fn prefetch_car(assets: &Path, car: &str, track: &str) {
    if !enabled() {
        return;
    }
    let cars = assets.join("cars");
    let dir = cars.join(car);
    let mut files = vec![dir.join("model.json"), dir.join("physics.json"), assets.join("shaders/media/Cars/shaders_v16.fxobj")];
    let cube = cars.join("cubemaps").join(format!("{track}.dds"));
    files.push(if cube.exists() { cube } else { cars.join("cubemaps/colorado.dds") });
    let mut dirs = vec![dir.join("fx"), dir.join("fx/tex"), cars.join("shared"), cars.join("shared/tex")];
    // The stock rim (wheel.rs: `<car's game>/cars/wheels/<rim>`; an aftermarket rim loads on first use).
    if let Some(rim) = read_json(&dir.join("model.json")).and_then(|m| m["rim"].as_str().map(str::to_owned)) {
        dirs.push(dir.parent().unwrap_or(&cars).join("wheels").join(rim));
    }
    for sub in dirs {
        for e in std::fs::read_dir(&sub).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                files.push(e.path());
            }
        }
    }
    for f in files {
        let _ = read(&f);
    }
}
