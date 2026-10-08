//! Car textures, faithful: every `.xds` / `.xpr` the car renderer can bind -> DDS holding the game's own
//! blocks for every level (untiled, never re-encoded; levels as the file stores them, see
//! `fh1_formats::bundle::decode_chain` and docs/LIGHTMAPS.md "Car textures"). (Owner: lightmap session d8.)
//!
//! Output, inside the `cars` group (each folder also gets a `tex.json`):
//! - `<CAR>/fx/tex/<path>.dds`: every `.xds` in `media/cars/<CAR>.zip` (path inside the zip, extension
//!   dropped: `nodamage_LOD0`, `DigitalGauge/ALF_8C_08_ICON`, ...); its `LiveryMasks/*.tga` copied as is.
//! - `shared/tex/<path>.dds`: `media/cars/Shared.zip` `.xds` and the `.xpr` cube maps (`staticCube`, ...).
//! - `driver/tex/<path>.dds`: `media/cars/Driver.zip` `.xds` (driver body, hands, gloves).
//! - `wheels/<rim zip stem>/<path>.dds`: `media/wheels/<rim>.zip` `.xds` (next to the rim's `.fxcar`).
//! - `carlights/<name>.dds`: `media/carlights/*.xds`.
//! - `cubemaps/<track>.dds`: `media/tracks/<track>/staticcarcubemap.xpr` (`tracks` = `media/tracks/` itself).
//! (`media/brakes` holds only `.carbin`, no textures.)
//!
//! `tex.json`: `{"<path>": {"file", "w0", "fmt", "width", "height", "mips", "cube"}}`. `w0` / `fmt` are the
//! game's fetch-constant dwords 0 and 1 as stored (sign / gamma bits are `(w0 >> 2) & 0xFF`, x/y/z/w two bits
//! each, 3 = gamma; format = `fmt & 0x3F`, endian `(fmt >> 6) & 3`). `width` / `height` are the real size;
//! block-compressed textures whose size isn't a multiple of 4 are written padded to the next multiple of 4
//! (the extra texels are the stored block padding) with the mips that still fit. DXGI formats: BC1/2/3/4/5
//! UNORM, ARGB8 as R8G8B8A8_UNORM (raw), DXT3A expanded to R8G8B8A8. Cube faces are +X -X +Y -Y +Z -Z as
//! stored (the game's left-handed cube space).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{bail, Result};
use fh1_formats::xds::{self, Format, Header, Image, HEADER_LEN};
use fh1_formats::zip::Archive;
use fh1_formats::{bundle, xpr};
use rayon::prelude::*;
use serde_json::{json, Value};

/// A decoded texture: `[level][layer]` (one layer, or six for cube maps) plus the game's fetch dwords.
struct Tex {
    levels: Vec<Vec<Image>>,
    w0: u32,
    fmt: u32,
    width: u32,
    height: u32,
}

/// Every stored level of a 2D `.xds`.
fn xds_tex(file: &[u8]) -> Result<Tex> {
    let h = Header::parse(file)?;
    if h.dimension != 1 {
        bail!("xds dimension {}", h.dimension);
    }
    let data = &file[HEADER_LEN..];
    let mips = h.max_mip + 1;
    let packed_base = h.packed_mips && h.width.next_power_of_two().min(h.height.next_power_of_two()) <= 16;
    let levels = if !h.tiled {
        // One untiled texture on the disc (Shared grille1_NRM_s_LOD0): base level only.
        vec![xds::decode_base(file)?.1]
    } else if packed_base {
        // The whole chain is one packed tail at the base address.
        bundle::decode_chain(data, h.format, h.endian, h.width, h.height, 0, mips)?
    } else {
        let mut v = vec![xds::decode_base(file)?.1];
        if mips > 1 {
            let at = ((h.fetch[5] >> 12) << 12) as usize;
            let chain = data.get(at..).ok_or_else(|| anyhow::anyhow!("mip address past the end"))?;
            v.extend(bundle::decode_chain(chain, h.format, h.endian, h.width, h.height, 1, mips)?);
        }
        v
    };
    Ok(Tex { levels: levels.into_iter().map(|l| vec![l]).collect(), w0: h.fetch[0], fmt: h.fetch[1], width: h.width, height: h.height })
}

/// The texture resources of an `.xpr` (cube maps).
fn xpr_texs(file: &[u8]) -> Result<Vec<(String, Tex)>> {
    xpr::parse(file)?
        .into_iter()
        .map(|r| {
            let levels = xpr::decode(file, &r)?;
            let h = &r.header;
            Ok((r.name.clone(), Tex { levels, w0: h.fetch[0], fmt: h.fetch[1], width: h.width, height: h.height }))
        })
        .collect()
}

/// DDS (DX10 header) with the levels as given; DDS stores each layer's whole chain in turn.
fn dds(t: &Tex) -> Result<(Vec<u8>, Value)> {
    let format = t.levels[0][0].format;
    let (dxgi, block) = match format {
        Format::Dxt1 => (71, 4),
        Format::Dxt3 => (74, 4),
        Format::Dxt5 => (77, 4),
        Format::Dxt5A => (80, 4),
        Format::Dxn => (83, 4),
        Format::Argb8 | Format::Dxt3A => (28, 1),
        f => bail!("texture format {f:?}"),
    };
    // Block-compressed top levels must be whole blocks: pad, then keep the mips whose block counts still agree.
    let (w, h) = (t.width.next_multiple_of(block), t.height.next_multiple_of(block));
    let keep = (0..t.levels.len() as u32)
        .take_while(|&k| {
            let b = |v: u32| (v >> k).max(1).div_ceil(block);
            b(t.width) == b(w) && b(t.height) == b(h)
        })
        .count();
    let layers = t.levels[0].len();
    let mut body = Vec::new();
    for layer in 0..layers {
        for level in &t.levels[..keep] {
            let img = &level[layer];
            if format == Format::Dxt3A {
                body.extend(xds::to_rgba8(img)?);
            } else {
                body.extend_from_slice(&img.data);
            }
        }
    }
    let cube = layers == 6;
    let top = t.levels[0][0].data.len() as u32;
    let mut d = Vec::with_capacity(148 + body.len());
    let mut put = |v: u32| d.extend_from_slice(&v.to_le_bytes());
    put(0x2053_4444); // "DDS "
    put(124);
    put(0x1 | 0x2 | 0x4 | 0x1000 | 0x20000 | 0x80000); // caps, height, width, pixelformat, mipcount, linearsize
    put(h);
    put(w);
    put(if format == Format::Dxt3A { t.width * t.height * 4 } else { top });
    put(0);
    put(keep as u32);
    for _ in 0..11 {
        put(0);
    }
    put(32); // pixel format: size, DDPF_FOURCC, "DX10"
    put(0x4);
    put(0x3031_5844);
    for _ in 0..5 {
        put(0);
    }
    put(0x1000 | 0x400000 | 0x8); // texture, mipmap, complex
    put(if cube { 0xFE00 } else { 0 });
    for _ in 0..3 {
        put(0);
    }
    put(dxgi);
    put(3); // TEXTURE2D
    put(if cube { 0x4 } else { 0 });
    put(1);
    put(0);
    d.extend(body);
    let info = json!({
        "w0": t.w0, "fmt": t.fmt, "width": t.width, "height": t.height, "mips": keep, "cube": cube,
    });
    Ok((d, info))
}

/// Writes `<dir>/<key>.dds` and returns its `tex.json` entry.
fn write(dir: &Path, key: &str, t: &Tex) -> Result<Value> {
    let (bytes, mut info) = dds(t)?;
    let file = format!("{key}.dds");
    let path = dir.join(&file);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, bytes)?;
    info["file"] = json!(file);
    Ok(info)
}

/// Every `.xds` / `.xpr` (and `LiveryMasks` `.tga`) of a zip into `dir`, plus `dir/tex.json`.
/// Returns (textures written, failures).
pub(crate) fn convert_zip(zip: &Path, dir: &Path) -> Result<(usize, usize)> {
    let mut ar = Archive::open(zip)?;
    let mut seen = HashSet::new();
    let mut raw = Vec::new();
    for e in ar.entries.clone() {
        let name = e.name.replace(char::from(92), "/");
        let lower = name.to_ascii_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        if lower.ends_with(".xds") || lower.ends_with(".xpr") {
            raw.push((name, ar.read(&e)?));
        } else if lower.ends_with(".tga") && lower.contains("liverymasks/") {
            let dst = dir.join(&name);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::write(dst, ar.read(&e)?)?;
        }
    }
    convert(raw, dir, &zip.display().to_string())
}

/// Converts (path inside the source, file bytes) in parallel into `dir` and writes `dir/tex.json`.
fn convert(raw: Vec<(String, Vec<u8>)>, dir: &Path, source: &str) -> Result<(usize, usize)> {
    let results: Vec<Vec<(String, Result<Value>)>> = raw
        .par_iter()
        .map(|(name, bytes)| {
            let stem = &name[..name.len() - 4];
            if name.to_ascii_lowercase().ends_with(".xpr") {
                match xpr_texs(bytes) {
                    Ok(texs) => {
                        let one = texs.len() == 1;
                        texs.iter()
                            .enumerate()
                            .map(|(i, (_, t))| {
                                let key = if one { stem.to_owned() } else { format!("{stem}_{i}") };
                                let r = write(dir, &key, t);
                                (key, r)
                            })
                            .collect()
                    }
                    Err(e) => vec![(stem.to_owned(), Err(e))],
                }
            } else {
                vec![(stem.to_owned(), xds_tex(bytes).and_then(|t| write(dir, stem, &t)))]
            }
        })
        .collect();
    let mut index = BTreeMap::new();
    let mut bad = 0;
    for (key, r) in results.into_iter().flatten() {
        match r {
            Ok(info) => {
                index.insert(key, info);
            }
            Err(e) => {
                bad += 1;
                println!("[cars] tex {source}: {key}: {e:#}");
            }
        }
    }
    if !index.is_empty() {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("tex.json"), serde_json::to_vec_pretty(&index)?)?;
    }
    Ok((index.len(), bad))
}

/// One car: its zip's textures into `<car dir>/fx/tex`.
pub fn build_car(zip: &Path, car_dir: &Path) -> Result<()> {
    convert_zip(zip, &car_dir.join("fx").join("tex"))?;
    Ok(())
}

/// Shared.zip, wheels, car lights and the tracks' car cube maps.
pub fn build_shared(disc: &Path, out: &Path) -> Result<()> {
    let (mut ok, mut bad) = convert_zip(&disc.join("media/cars/Shared.zip"), &out.join("shared").join("tex"))?;
    let (o, b) = convert_zip(&disc.join("media/cars/Driver.zip"), &out.join("driver").join("tex"))?;
    (ok, bad) = (ok + o, bad + b);
    let mut zips: Vec<_> = std::fs::read_dir(disc.join("media/wheels"))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    zips.sort();
    for z in zips.iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))) {
        let stem = z.file_stem().unwrap().to_string_lossy().into_owned();
        let (o, b) = convert_zip(z, &out.join("wheels").join(&stem))?;
        (ok, bad) = (ok + o, bad + b);
    }
    let loose = |dir: &Path, ext: &str| -> Result<Vec<(String, Vec<u8>)>> {
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)) {
                    v.push((p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p)?));
                }
            }
        }
        v.sort();
        Ok(v)
    };
    let (o, b) = convert(loose(&disc.join("media/carlights"), "xds")?, &out.join("carlights"), "media/carlights")?;
    (ok, bad) = (ok + o, bad + b);
    // Car cube maps per track: media/tracks/<track>/staticcarcubemap.xpr, and media/tracks/ itself ("tracks").
    let tracks = disc.join("media/tracks");
    let mut cubes: Vec<(String, Vec<u8>)> = loose(&tracks, "xpr")?.into_iter().map(|(_, d)| ("tracks.xpr".to_owned(), d)).collect();
    let mut dirs: Vec<_> = std::fs::read_dir(&tracks)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for d in dirs {
        for (_, bytes) in loose(&d, "xpr")? {
            cubes.push((format!("{}.xpr", d.file_name().unwrap().to_string_lossy()), bytes));
        }
    }
    let (o, b) = convert(cubes, &out.join("cubemaps"), "media/tracks")?;
    (ok, bad) = (ok + o, bad + b);
    println!("[cars] textures: Shared, Driver, wheels, car lights, cube maps: {ok} written, {bad} failed");
    Ok(())
}
