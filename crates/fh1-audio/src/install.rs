//! Disc → converted audio (the setup tool's `audio` group).
//!
//! Output:
//! - `cars/<CAR>.json`: resolved [`CarAudio`] for every car with engine tuning.
//! - `banks/<Bank>/<NNN>.wav` + `banks/<Bank>.json` ([`BankInfo`]): decoded FSB samples, for
//!   every engine bank a car references plus the shared tyre/wind/transmission/turbo banks.
//! - `tires.json`: physics surface name → tyre event group (`Tires.xml`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    let mut jobs: Vec<(Arc<Vec<u8>>, String, usize)> = Vec::new();
    for p in &banks {
        let buf = Arc::new(std::fs::read(p).with_context(|| p.display().to_string())?);
        let samples = fsb::parse(&buf).with_context(|| p.display().to_string())?;
        let stem = bank_stem(&p.file_name().unwrap().to_string_lossy()).to_owned();
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
    println!("  {} banks, {} samples, {} failed", banks.len(), jobs.len(), errors.len());
    for e in errors.iter().take(10) {
        eprintln!("  {e}");
    }
    if !errors.is_empty() {
        bail!("{} samples failed to decode", errors.len());
    }
    Ok(())
}
