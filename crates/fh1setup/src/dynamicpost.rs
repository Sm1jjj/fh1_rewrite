//! `dynamicpost` group: `media/dynamicpost.zip` extracted as is (post templates per track and the colour
//! grading LUTs, 16³ A8R8G8B8 volume DDS), for fh1-render's FH1 post chain (`postfx::FxPostConfig`).
//! Also the track's default LUTs used outside every post zone (`media/tracks/colorado/ColorGradingLookup
//! {,_Night}.dds`, same format), copied to `Tracks/Colorado/`.

use std::path::Path;

use anyhow::Result;
use fh1_formats::zip::Archive;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let mut ar = Archive::open(disc.join("media/dynamicpost.zip"))?;
    let mut n = 0;
    for e in ar.entries.clone() {
        let name = e.name.replace(char::from(92), "/");
        if name.ends_with('/') || name.split('/').any(|p| p == "..") {
            continue;
        }
        let dst = out.join(&name);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::write(dst, ar.read(&e)?)?;
        n += 1;
    }
    for f in ["ColorGradingLookup.dds", "ColorGradingLookup_Night.dds"] {
        let src = disc.join("media/tracks/colorado").join(f);
        if src.exists() {
            let dst = out.join("Tracks/Colorado").join(f);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::copy(src, dst)?;
            n += 1;
        }
    }
    println!("[dynamicpost] {n} files");
    Ok(())
}
