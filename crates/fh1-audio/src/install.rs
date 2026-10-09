//! Disc → converted audio (the setup tool's `audio` group).
//!
//! Output:
//! - `cars/<CAR>.json`: resolved [`CarAudio`] for every car with engine tuning.
//! - `banks/<Bank>/<NNN>.wav` + `banks/<Bank>.json` ([`BankInfo`]): decoded FSB samples, for
//!   every engine bank a car references plus the shared tyre/wind/transmission/turbo banks.
//! - `tires.json`: physics surface name → tyre event group (`Tires.xml`).
//! - `banks/<Bank>/…` for the non-car banks too (all XMA, asserted): UI (`UIInGame`, `UIInGame_Streams`),
//!   `General`, `Collisions`, `Collisions_HiRes`, `Glass`, `Horns`, and the 13 Colorado world banks
//!   (5 loose in `media/audio/tracks/colorado`, 8 inside `media/tracks/colorado/bin.zip`).
//! - `fev/<Name>.fev`: byte copies of the FMOD event files that pair those banks.
//! - `collisions/CollisionData.xml`: byte copy.
//! - `world_banks.json`: `[{name, fev, fsb, stem, samples, source[, pairing_note]}]`, one per world bank.
//! - `ui/sample_names.json` (FSB sample names per UI bank, header order) + the `ui4audio.xml` tables
//!   ([`crate::ui_events`]).
//! - `soundscape/`: Colorado soundscape tiles, `index.json`, `_templates/` ([`crate::soundscape`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use fh1_formats::zip::Archive;
use serde::{Deserialize, Serialize};

use crate::tuning::{CarAudio, El};
use crate::{fsb, wav, xma};

/// Banks every car shares, relative to `media/audio/cars/`.
pub const SHARED_BANKS: &[&str] = &[
    "Tires/Tires.fsb",
    "Wind/Wind.fsb",
    "Transmissions/Transmissions.fsb",
    "Turbos/Turbos.fsb",
    "SuperChargers/SuperChargers.fsb",
    "Suspension/Suspension.fsb",
    "ExhNoise/ExhNoise.fsb",
    "EngineLFE/EngineLFE.fsb",
    "Engines/Soundbanks/Burbles/EvoPops.fsb",
    "Engines/Soundbanks/Burbles/6N0_NSX_Pops.fsb",
    "Engines/Soundbanks/Burbles/6N3_NSX_Pops.fsb",
    "Engines/Soundbanks/Burbles/SequencePops_2.fsb",
    "Engines/Soundbanks/Burbles/SequencePops_3.fsb",
    "Engines/Soundbanks/Burbles/4T2_Tag_Espirit.fsb",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleInfo {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub frames: u32,
    pub loop_start: u32,
    pub loop_end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BankInfo {
    pub samples: Vec<SampleInfo>,
}

/// Bank folder/JSON stem: file name without `.fsb`.
pub fn bank_stem(bank: &str) -> &str {
    let name = bank.rsplit(['/', '\\']).next().unwrap_or(bank);
    name.strip_suffix(".fsb").or_else(|| name.strip_suffix(".FSB")).unwrap_or(name)
}

/// Every entry of a zip by lowercase file name.
fn read_zip(path: &Path) -> Result<HashMap<String, Vec<u8>>> {
    let mut ar = Archive::open(path).with_context(|| path.display().to_string())?;
    let entries = ar.entries.clone();
    let mut out = HashMap::new();
    for e in &entries {
        let name = e.name.rsplit(['/', '\\']).next().unwrap_or(&e.name).to_ascii_lowercase();
        out.insert(name, ar.read(e)?);
    }
    Ok(out)
}

/// Finds `name` in `dir` ignoring case.
fn find_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|e| e.path())
}

/// Lowercase → original spelling of car media names (`media/cars/<NAME>.zip`), which is
/// what the `cars` group's folders use.
fn car_names(disc: &Path) -> HashMap<String, String> {
    let Ok(dir) = std::fs::read_dir(disc.join("media/cars")) else { return HashMap::new() };
    dir.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let stem = n.strip_suffix(".zip")?.to_owned();
            Some((stem.to_ascii_lowercase(), stem))
        })
        .collect()
}

/// Non-car banks under `media/audio/`.
const EXTRA_BANKS: &[&str] = &[
    "UI/UIInGame/UIInGame.fsb",
    "UI/UIInGame/UIInGame_Streams.fsb",
    "gamemodes/General.fsb",
    "collisions/Collisions.fsb",
    "collisions/Collisions_HiRes.fsb",
    "collisions/Glass.fsb",
    "cars/Horns/Horns.fsb",
];
/// Event files that pair [`EXTRA_BANKS`], under `media/audio/`.
const EXTRA_FEVS: &[&str] = &[
    "UI/UIInGame/UIInGame.fev",
    "gamemodes/General.fev",
    "collisions/Collisions.fev",
    "collisions/Glass.fev",
    "cars/Horns/Horns.fev",
];
const LOOSE_WORLD_FSB: &[&str] =
    &["AMB_Default.fsb", "AMB_Festival_Ambience.fsb", "AMB_Quads_Stream.fsb", "Colorado_Festival.fsb", "Triggerable_Events.fsb"];
const LOOSE_WORLD_FEV: &[&str] =
    &["AMB_Default.fev", "AMB_Festival_Ambience.fev", "AMB_Quads.fev", "Colorado_Festival.fev", "Triggerable_Events.fev"];
/// World banks inside `media/tracks/colorado/bin.zip` (stems; `.fsb` + `.fev` each).
const ZIP_WORLD: &[&str] = &[
    "AMB_Foothills",
    "AMB_Main_Town",
    "AMB_Mountains",
    "AMB_Plains",
    "AMB_Redstone",
    "AMB_Red_Rock",
    "AMB_Reservoir",
    "AMB_Festival",
];

type Loaded = (String, Arc<Vec<u8>>);
type Files = Vec<(String, Vec<u8>)>;

fn read_disc(disc: &Path, rel: &str) -> Result<Vec<u8>> {
    let p = fh1_formats::path::resolve(&disc.join(rel));
    std::fs::read(&p).with_context(|| p.display().to_string())
}

/// Parses a non-car bank and requires every sample to be XMA (before anything is decoded).
fn check_xma(name: &str, buf: &[u8]) -> Result<Vec<fsb::Sample>> {
    let samples = fsb::parse(buf).with_context(|| name.to_owned())?;
    for s in &samples {
        if s.mode & fsb::MODE_XMA == 0 {
            bail!("{name}: sample {:?} has mode {:#x}, not XMA", s.name, s.mode);
        }
    }
    let rates: BTreeSet<u32> = samples.iter().map(|s| s.rate).collect();
    println!("  {name}: {} samples, rates {rates:?}", samples.len());
    Ok(samples)
}

fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Everything beyond the cars: UI/collision/horn/general/world banks, `.fev` copies, tables and the
/// soundscape. Returns the banks to decode (stem, bytes).
fn extras(disc: &Path, out: &Path) -> Result<Vec<Loaded>> {
    let mut loaded: Vec<Loaded> = Vec::new();
    let mut ui_names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    std::fs::create_dir_all(out.join("fev"))?;

    for rel in EXTRA_BANKS {
        let file = rel.rsplit('/').next().unwrap();
        let buf = Arc::new(read_disc(disc, &format!("media/audio/{rel}"))?);
        let samples = check_xma(file, &buf)?;
        if rel.starts_with("UI/") {
            ui_names.insert(file.to_owned(), samples.iter().map(|s| s.name.clone()).collect());
        }
        loaded.push((bank_stem(file).to_owned(), buf));
    }
    for rel in EXTRA_FEVS {
        let file = rel.rsplit('/').next().unwrap();
        std::fs::write(out.join("fev").join(file), read_disc(disc, &format!("media/audio/{rel}"))?)?;
    }
    std::fs::create_dir_all(out.join("collisions"))?;
    std::fs::write(out.join("collisions/CollisionData.xml"), read_disc(disc, "media/audio/collisions/CollisionData.xml")?)?;

    // --- world banks: 5 loose + 8 inside bin.zip (which also holds every .soundscape tile)
    let mut groups: Vec<(&str, Files, Files)> = Vec::new();
    let rd_all = |names: &[&str]| -> Result<Files> {
        names.iter().map(|n| Ok((n.to_string(), read_disc(disc, &format!("media/audio/tracks/colorado/{n}"))?))).collect()
    };
    groups.push(("media/audio/tracks/colorado", rd_all(LOOSE_WORLD_FEV)?, rd_all(LOOSE_WORLD_FSB)?));

    let zip_path = disc.join("media/tracks/colorado/bin.zip");
    let mut ar = Archive::open(&zip_path).with_context(|| zip_path.display().to_string())?;
    let wanted: HashSet<String> =
        ZIP_WORLD.iter().flat_map(|s| [format!("{s}.fsb").to_ascii_lowercase(), format!("{s}.fev").to_ascii_lowercase()]).collect();
    let mut found: HashMap<String, (u32, Vec<u8>)> = HashMap::new();
    let mut seen_tiles = HashSet::new();
    let mut tiles: Vec<(String, String)> = Vec::new();
    for i in 0..ar.entries.len() {
        let e = ar.entries[i].clone();
        let base = e.name.rsplit(['/', '\\']).next().unwrap_or(&e.name).to_owned();
        let lc = base.to_ascii_lowercase();
        if let Some(stem) = lc.strip_suffix(".soundscape") {
            // bin.zip repeats objects per streaming block: one copy each.
            if seen_tiles.insert(lc.clone()) {
                let stem = base[..stem.len()].to_owned();
                tiles.push((stem, String::from_utf8_lossy(&ar.read(&e)?).into_owned()));
            }
        } else if wanted.contains(&lc) {
            match found.get(&lc) {
                Some((size, _)) => {
                    if *size != e.size {
                        eprintln!("  warning: duplicate {base} in bin.zip differs in size ({size} vs {})", e.size);
                    }
                }
                None => {
                    let bytes = ar.read(&e)?;
                    found.insert(lc, (e.size, bytes));
                }
            }
        }
    }
    tiles.sort();
    let take = |names: Vec<String>| -> Result<Files> {
        names
            .into_iter()
            .map(|n| match found.get(&n.to_ascii_lowercase()) {
                Some((_, b)) => Ok((n, b.clone())),
                None => bail!("{n} not found in bin.zip"),
            })
            .collect()
    };
    let zip_fev = take(ZIP_WORLD.iter().map(|s| format!("{s}.fev")).collect())?;
    let zip_fsb = take(ZIP_WORLD.iter().map(|s| format!("{s}.fsb")).collect())?;
    groups.push(("media/tracks/colorado/bin.zip", zip_fev, zip_fsb));

    let mut world = Vec::new();
    for (source, fevs, fsbs) in groups {
        let mut used: HashSet<String> = HashSet::new();
        for (fev, fev_bytes) in &fevs {
            let stem = stem_of(fev);
            // Identical stem, else the fsb whose stem starts with the fev's (AMB_Quads.fev <-> AMB_Quads_Stream.fsb).
            let exact = fsbs.iter().position(|(n, _)| stem_of(n).eq_ignore_ascii_case(stem));
            let (idx, note) = match exact {
                Some(i) => (i, None),
                None => {
                    let lc = stem.to_ascii_lowercase();
                    let i = fsbs
                        .iter()
                        .position(|(n, _)| !used.contains(n) && n.to_ascii_lowercase().starts_with(&lc))
                        .with_context(|| format!("no fsb pairs {fev}"))?;
                    (i, Some("paired by name prefix, not identical stem"))
                }
            };
            let (fsb, fsb_bytes) = &fsbs[idx];
            used.insert(fsb.clone());
            std::fs::write(out.join("fev").join(fev), fev_bytes)?;
            let buf = Arc::new(fsb_bytes.clone());
            let samples = check_xma(fsb, &buf)?;
            let mut entry = serde_json::json!({
                "name": stem, "fev": fev, "fsb": fsb, "stem": bank_stem(fsb), "samples": samples.len(), "source": source,
            });
            if let Some(n) = note {
                entry["pairing_note"] = serde_json::json!(n);
            }
            world.push(entry);
            loaded.push((bank_stem(fsb).to_owned(), buf));
        }
    }
    std::fs::write(out.join("world_banks.json"), serde_json::to_vec_pretty(&world)?)?;

    // --- UI table
    let ui_xml = String::from_utf8(read_disc(disc, "media/audio/UI/ui4audio.xml")?).context("ui4audio.xml is not UTF-8")?;
    std::fs::create_dir_all(out.join("ui"))?;
    std::fs::write(out.join("ui/sample_names.json"), serde_json::to_vec_pretty(&ui_names)?)?;
    let stats = crate::ui_events::write(&out.join("ui"), &ui_xml, &ui_names)?;
    println!("  ui4audio: {stats}");

    // --- soundscape
    let text = |n: &str| -> Result<String> {
        Ok(String::from_utf8_lossy(&read_disc(disc, &format!("media/tracks/colorado/{n}"))?).into_owned())
    };
    let stats = crate::soundscape::build(
        &tiles,
        &text("colorado_ambience.xml")?,
        &text("colorado_reverb.xml")?,
        &text("colorado_soundbank_lookup.xml")?,
        &out.join("soundscape"),
    )?;
    println!("  soundscape: {stats}");
    Ok(loaded)
}

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let ffmpeg = xma::ffmpeg().context(
        "audio needs ffmpeg for XMA decoding (put it on PATH or set FH1_FFMPEG); \
         a built-in decoder is planned",
    )?;
    let cars_dir = disc.join("media/audio/cars");
    let text = |b: &Vec<u8>| String::from_utf8_lossy(b).into_owned();

    // --- tuning
    let ets = read_zip(&cars_dir.join("Engines/EngineTuning.zip"))?;
    let hts = read_zip(&cars_dir.join("Engines/HarmonicTuning.zip"))?;
    let cmts = read_zip(&cars_dir.join("CarModelTuning.zip"))?;
    std::fs::create_dir_all(out.join("cars"))?;
    let car_names = car_names(disc);
    let mut engine_banks = BTreeSet::new();
    let mut n_cars = 0;
    for (file, et) in &ets {
        let Some(car_lc) = file.strip_suffix("_et.xml") else { continue };
        // The tuning zips also cover DLC/unused cars; only convert cars on this disc.
        let Some(car) = car_names.get(car_lc) else { continue };
        let cmt = cmts.get(&format!("{car_lc}_cmt.xml")).map(text);
        let audio = CarAudio::resolve(car, Some(&text(et)), cmt.as_deref(), |ht| {
            hts.get(&ht.to_ascii_lowercase()).map(text)
        })?;
        engine_banks.extend(audio.banks().map(str::to_owned));
        std::fs::write(out.join("cars").join(format!("{car}.json")), serde_json::to_vec_pretty(&audio)?)?;
        n_cars += 1;
    }
    println!("  {n_cars} car tunings, {} engine banks", engine_banks.len());

    // --- tyre surface map
    let tires_xml = std::fs::read_to_string(cars_dir.join("Tires/Tires.xml"))?;
    let mut surfaces = BTreeMap::new();
    for m in El::parse(&tires_xml)?.kids.iter().filter(|k| k.name == "SurfaceMap") {
        if let (Some(s), Some(g)) = (m.attr("PhysicsSurface"), m.attr("Lod1EventGroup")) {
            surfaces.insert(s.to_owned(), g.rsplit('/').next().unwrap_or(g).to_owned());
        }
    }
    std::fs::write(out.join("tires.json"), serde_json::to_vec_pretty(&surfaces)?)?;

    // --- banks
    let mut banks: Vec<PathBuf> = SHARED_BANKS.iter().map(|b| cars_dir.join(b)).collect();
    let lod1 = cars_dir.join("Engines/Soundbanks/LOD1");
    for b in &engine_banks {
        match find_ci(&lod1, b) {
            Some(p) => banks.push(p),
            None => eprintln!("  warning: engine bank {b} not on disc"),
        }
    }
    let mut loaded: Vec<Loaded> = Vec::new();
    for p in &banks {
        let buf = Arc::new(std::fs::read(fh1_formats::path::resolve(p)).with_context(|| p.display().to_string())?);
        fsb::parse(&buf).with_context(|| p.display().to_string())?;
        loaded.push((bank_stem(&p.file_name().unwrap().to_string_lossy()).to_owned(), buf));
    }
    // Non-car banks are validated (all XMA) before anything is decoded.
    loaded.extend(extras(disc, out)?);

    let mut jobs: Vec<(Arc<Vec<u8>>, String, usize)> = Vec::new();
    for (stem, buf) in &loaded {
        let samples = fsb::parse(buf).with_context(|| stem.clone())?;
        let stem = stem.clone();
        let buf = buf.clone();
        std::fs::create_dir_all(out.join("banks").join(&stem))?;
        let info = BankInfo {
            samples: samples
                .iter()
                .map(|s| SampleInfo {
                    name: s.name.clone(),
                    rate: s.rate,
                    channels: s.channels,
                    frames: s.frames,
                    loop_start: s.loop_start,
                    loop_end: s.loop_end,
                })
                .collect(),
        };
        std::fs::write(out.join("banks").join(format!("{stem}.json")), serde_json::to_vec_pretty(&info)?)?;
        jobs.extend((0..samples.len()).map(|i| (buf.clone(), stem.clone(), i)));
    }

    // Decode in parallel: one ffmpeg process per sample.
    let next = AtomicUsize::new(0);
    let errors = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((buf, stem, idx)) = jobs.get(i) else { break };
                let r = (|| -> Result<()> {
                    let s = &fsb::parse(buf)?[*idx];
                    // Re-runs only decode what is missing (a bump that adds banks keeps the 3,858 decoded ones).
                    let wav_path = out.join("banks").join(stem).join(format!("{idx:03}.wav"));
                    if std::fs::metadata(&wav_path).is_ok_and(|m| m.len() > 44) {
                        return Ok(());
                    }
                    if s.mode & fsb::MODE_XMA == 0 {
                        bail!("mode {:#x} is not XMA", s.mode);
                    }
                    let pcm = xma::decode(&ffmpeg, fsb::xma2_riff(buf, s), s.channels, s.frames)?;
                    wav::write(&out.join("banks").join(stem).join(format!("{idx:03}.wav")), s.rate, s.channels, &pcm)
                })();
                if let Err(e) = r {
                    errors.lock().unwrap().push(format!("{stem}#{idx}: {e:#}"));
                }
                if i % 1000 == 0 {
                    println!("  decoded {i}/{}", jobs.len());
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    println!("  {} banks, {} samples, {} failed", loaded.len(), jobs.len(), errors.len());
    for e in errors.iter().take(10) {
        eprintln!("  {e}");
    }
    if !errors.is_empty() {
        bail!("{} samples failed to decode", errors.len());
    }
    // Character VO (site-73, docs/FEV.md): `dialogue/<LANG>/NNN.mp3` + `triggers.json`, languages from FH1_VO_LANGS.
    crate::dialogue::install_default(&disc.join("media/audio"), out)?;
    Ok(())
}
