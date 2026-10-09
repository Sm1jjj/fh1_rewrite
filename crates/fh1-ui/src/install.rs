//! `ui` setup group: everything the game's UI needs, converted from the user's disc.
//!
//! Output (under the group directory):
//! - `scenes/<name>.{bgf,fbf,bsg}`: Anark scenes from `media/UI.zip` `Scenes/ui4/` (names lower-cased),
//!   plus `ResPack.xml` and `EventNames.txt`.
//! - `fonts/<x>_vector_aa.dt` + `fonts/fontmap.xml`: vector fonts from `media/ui/Fonts.zip`.
//! - `map/colorado.nav`: the road network (see [`crate::nav`]).
//! - `strings/<LANG>.zip`: the string-table archives as on the disc (read with
//!   [`crate::strtable::StringTables::load_zip`]).
//! - `textures/horizon/<path>.png` (from `media/ui/textures/Horizon.zip`) and `textures/<path>.png`
//!   (from `media/ui/Textures.zip`), lower-cased; the layout [`crate::player::texture_path`] maps
//!   scene texture paths to. Single-channel textures (DXT5A/DXT3A) become white + alpha masks.
//!   `textures/no_gamma.txt` lists the textures the 360 samples without gamma decoding (fetch
//!   constant sign ≠ gamma; the rest are gamma-decoded on fetch).
//! - `fonts/cjk/{cht,jp,kor}.{png,json}`: the CJK bitmap fonts decoded from `media/ui/fonts/*.abc|sbm`
//!   (see `cjk`; schema `fh1.ui.cjk_font.v1`).
//! - `data/{credits,colorpicker,ResPack,cameras,SHLightSettings,LoadingDefs,MapProfileFullscreen,
//!   MapProfileMinimap}.{xml,json}` (XML byte copies + generic XML -> JSON, see `xmljson`),
//!   `data/EventNames.txt`, `data/EventNames_ui4.txt`, `data/behaviors/*.lua`, `map/MapGameReady.jpg`.

use std::path::Path;

use fh1_formats::xds::{self, Format};
use fh1_formats::zip::Archive;

use crate::{Error, Result};

mod cjk;
mod xmljson;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let media = disc.join("media");
    let scenes = scenes(&media.join("UI.zip"), &out.join("scenes"))?;
    let fonts = fonts(&media.join("ui/Fonts.zip"), &out.join("fonts"))?;
    let langs = strings(&media.join("stringtables"), &out.join("strings"))?;
    // The open-world road network the minimap draws.
    std::fs::create_dir_all(out.join("map"))?;
    std::fs::copy(media.join("tracks/colorado/colorado.nav"), out.join("map/colorado.nav"))?;
    let pois = crate::mappois::build(&media, out)?;
    let mut raw = Vec::new();
    let (mut ok, mut failed) = textures(&media.join("ui/textures/Horizon.zip"), &out.join("textures/horizon"), "horizon/", &mut raw)?;
    let (ok2, failed2) = textures(&media.join("ui/Textures.zip"), &out.join("textures"), "", &mut raw)?;
    ok += ok2;
    failed += failed2;
    raw.sort();
    std::fs::write(out.join("textures/no_gamma.txt"), raw.join("\n"))?;
    let cjk = cjk::build(&media.join("ui/fonts"), &out.join("fonts/cjk"))?;
    let data = ui_data(&media.join("UI.zip"), &media.join("ui"), out)?;
    println!("  ui: {scenes} scene files, {fonts} fonts, {langs} languages, {pois} map POIs, {ok} textures ({failed} undecodable), {cjk} CJK glyphs, {data} data files");
    Ok(())
}

fn norm(name: &str) -> String {
    name.replace('\\', "/").to_ascii_lowercase()
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, data)?;
    Ok(())
}

fn scenes(zip: &Path, out: &Path) -> Result<usize> {
    let mut ar = Archive::open(zip)?;
    let mut n = 0;
    for e in ar.entries.clone() {
        let name = norm(&e.name);
        let dst = if let Some(f) = name.strip_prefix("scenes/ui4/").filter(|f| f.ends_with(".bgf") || f.ends_with(".fbf") || f.ends_with(".bsg")) {
            out.join(f)
        } else if name == "respack.xml" {
            out.join("ResPack.xml")
        } else if name == "scenes/ui4/eventnames.txt" {
            out.join("EventNames.txt")
        } else {
            continue;
        };
        write(&dst, &ar.read(&e)?)?;
        n += 1;
    }
    Ok(n)
}

fn fonts(zip: &Path, out: &Path) -> Result<usize> {
    let mut ar = Archive::open(zip)?;
    let mut n = 0;
    for e in ar.entries.clone() {
        let name = norm(&e.name);
        // Top level only: `comparison/` holds the font compiler's before/after test builds.
        if name.contains('/') || !(name.ends_with(".dt") || name == "fontmap.xml") {
            continue;
        }
        write(&out.join(&name), &ar.read(&e)?)?;
        n += name.ends_with(".dt") as usize;
    }
    Ok(n)
}

fn strings(dir: &Path, out: &Path) -> Result<usize> {
    let mut n = 0;
    std::fs::create_dir_all(out)?;
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
            let stem = p.file_stem().and_then(|s| s.to_str()).ok_or_else(|| Error::Format(format!("{}", p.display())))?;
            std::fs::copy(&p, out.join(format!("{}.zip", stem.to_ascii_uppercase())))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Decode every `.xds` in a UI texture archive to PNG. Returns (written, undecodable). Textures
/// whose fetch constant does NOT mark R/G/B as gamma (sign = 3) are listed in `raw` (as their
/// `.png` path under `textures/`): the 360 samples those as-is, the rest gamma-decoded.
fn textures(zip: &Path, out: &Path, prefix: &str, raw: &mut Vec<String>) -> Result<(usize, usize)> {
    let mut ar = Archive::open(zip)?;
    let (mut ok, mut failed) = (0, 0);
    for e in ar.entries.clone() {
        let name = norm(&e.name);
        let Some(stem) = name.strip_suffix(".xds") else { continue };
        let data = ar.read(&e)?;
        let decoded = xds::decode_base(&data).and_then(|(h, img)| Ok((h, xds::to_rgba8(&img)?)));
        let Ok((h, mut rgba)) = decoded else {
            failed += 1;
            continue;
        };
        if matches!(h.format, Format::Dxt5A | Format::Dxt3A) {
            // One channel: a mask. White, alpha = the channel.
            for px in rgba.chunks_exact_mut(4) {
                let v = px[0];
                px.copy_from_slice(&[255, 255, 255, v]);
            }
        }
        // Fetch dword 0: bits 2..7 = sign x/y/z (3 = gamma).
        let gamma = (h.fetch[0] >> 2) & 0x3F == 0x3F;
        if !gamma {
            raw.push(format!("{prefix}{stem}.png"));
        }
        let path = out.join(format!("{stem}.png"));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        let mut enc = png::Encoder::new(file, h.width, h.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .and_then(|mut w| w.write_image_data(&rgba))
            .map_err(|err| Error::Format(format!("{}: {err}", path.display())))?;
        ok += 1;
    }
    Ok((ok, failed))
}

/// XML documents read from `media/UI.zip` (root level), as (zip name lower-cased, output stem).
const ZIP_XML: [(&str, &str); 5] = [
    ("credits.xml", "credits"),
    ("colorpicker.xml", "colorpicker"),
    ("respack.xml", "ResPack"),
    ("cameras.xml", "cameras"),
    ("shlightsettings.xml", "SHLightSettings"),
];

/// XML documents read from `media/ui/`.
const DISC_XML: [&str; 3] = ["LoadingDefs", "MapProfileFullscreen", "MapProfileMinimap"];

/// `data/*.{xml,json}`, `data/EventNames*.txt`, `data/behaviors/*.lua` and `map/MapGameReady.jpg`.
/// Returns the number of files written.
fn ui_data(zip: &Path, media_ui: &Path, out: &Path) -> Result<usize> {
    let mut ar = Archive::open(zip)?;
    let data = out.join("data");
    let mut xml: Vec<(String, Vec<u8>)> = Vec::new();
    let (mut n, mut lua) = (0, 0);
    for e in ar.entries.clone() {
        let name = norm(&e.name);
        if let Some((_, stem)) = ZIP_XML.iter().find(|(z, _)| *z == name) {
            xml.push((stem.to_string(), ar.read(&e)?));
        } else if name == "scenes/eventnames.txt" {
            write(&data.join("EventNames.txt"), &ar.read(&e)?)?;
            n += 1;
        } else if name == "scenes/ui4/eventnames.txt" {
            write(&data.join("EventNames_ui4.txt"), &ar.read(&e)?)?;
            n += 1;
        } else if name == "new map/mapgameready.jpg" {
            write(&out.join("map/MapGameReady.jpg"), &ar.read(&e)?)?;
            n += 1;
        } else if name.starts_with("behaviors/ui4/") && name.ends_with(".lua") {
            // Keep the disc spelling ("Fire Event.lua").
            let path = e.name.replace('\\', "/");
            let file = path.rsplit('/').next().unwrap_or(&path).to_string();
            write(&data.join("behaviors").join(file), &ar.read(&e)?)?;
            n += 1;
            lua += 1;
        }
    }
    if lua != 5 || xml.len() != ZIP_XML.len() {
        return Err(Error::Format(format!("{}: expected 5 behaviors and {} xml files, found {lua} and {}", zip.display(), ZIP_XML.len(), xml.len())));
    }
    for stem in DISC_XML {
        xml.push((stem.to_string(), std::fs::read(media_ui.join(format!("{stem}.xml")))?));
    }
    for (stem, bytes) in xml {
        let (json, _repaired) = xmljson::convert(&bytes).map_err(|e| Error::Format(format!("{stem}.xml: {e}")))?;
        write(&data.join(format!("{stem}.xml")), &bytes)?;
        write(&data.join(format!("{stem}.json")), json.as_bytes())?;
        n += 2;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn disc() -> Option<PathBuf> {
        let d = std::env::var_os("FH1_DISC").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
        d.join("media/UI.zip").exists().then_some(d)
    }

    fn reference() -> Option<PathBuf> {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/extracted/converted/ui");
        d.join("data").exists().then_some(d)
    }

    /// Python wrote its JSON with Windows newlines.
    fn lf(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap().replace("\r\n", "\n")
    }

    #[test]
    fn xml_json_matches_python_output() {
        let (Some(disc), Some(refd)) = (disc(), reference()) else {
            eprintln!("skipped: no disc or no converted/ui reference");
            return;
        };
        let mut ar = Archive::open(disc.join("media/UI.zip")).unwrap();
        let mut srcs: Vec<(String, Vec<u8>)> = Vec::new();
        for e in ar.entries.clone() {
            if let Some((_, stem)) = ZIP_XML.iter().find(|(z, _)| *z == norm(&e.name)) {
                srcs.push((stem.to_string(), ar.read(&e).unwrap()));
            }
        }
        for stem in DISC_XML {
            srcs.push((stem.to_string(), std::fs::read(disc.join(format!("media/ui/{stem}.xml"))).unwrap()));
        }
        assert_eq!(srcs.len(), 8);
        for (stem, bytes) in srcs {
            let (doc, _) = xmljson::parse_bytes(&bytes).unwrap();
            let want = lf(std::fs::read(refd.join(format!("data/{stem}.json"))).unwrap());
            assert_eq!(doc.to_json().to_pretty(), want, "{stem}");
        }
    }

    #[test]
    fn cjk_jp_matches_python_output() {
        let (Some(disc), Some(refd)) = (disc(), reference()) else {
            eprintln!("skipped: no disc or no converted/ui reference");
            return;
        };
        let raw = disc.join("media/ui/fonts");
        if !raw.join("JPB.abc").exists() || !refd.join("fonts/cjk/jp.json").exists() {
            eprintln!("skipped: no JPB.abc / reference jp.json");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("fh1_ui_cjk_test_{}", std::process::id()));
        let n = cjk::convert_font(&raw, &tmp, "jp", "JPB", "JP.ini").unwrap();
        assert_eq!(n, 7591);
        let got = std::fs::read_to_string(tmp.join("jp.json")).unwrap();
        let want = lf(std::fs::read(refd.join("fonts/cjk/jp.json")).unwrap());
        assert!(got == want, "jp.json differs from the Python output");
        assert!(got.contains("\"scale\": 0.66") && got.contains("\"baselineshift\": 0.074"));
        // Pixels: decoded PNG == reference PNG (LA8, 3072 x 5712).
        let load = |p: PathBuf| {
            let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(p).unwrap()));
            let mut r = dec.read_info().unwrap();
            let mut buf = vec![0; r.output_buffer_size().unwrap()];
            let info = r.next_frame(&mut buf).unwrap();
            assert_eq!(info.color_type, png::ColorType::GrayscaleAlpha);
            (info.width, info.height, buf[..info.buffer_size()].to_vec())
        };
        let a = load(tmp.join("jp.png"));
        let b = load(refd.join("fonts/cjk/jp.png"));
        assert_eq!((a.0, a.1), (3072, 5712));
        assert!(a == b, "jp.png differs from the Python output");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
