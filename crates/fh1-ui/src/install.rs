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

use std::path::Path;

use fh1_formats::xds::{self, Format};
use fh1_formats::zip::Archive;

use crate::{Error, Result};

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
    println!("  ui: {scenes} scene files, {fonts} fonts, {langs} languages, {pois} map POIs, {ok} textures ({failed} undecodable)");
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
