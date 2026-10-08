//! `sky` group: `media/realtimesky.zip` (the real-time sky's textures and data) → `sky/`.
//! (Owner: sky/TOD session, docs/SHADERS.md "Sky".)
//!
//! - every `.xds` → `sky/<name>.dds` + `sky/tex.json` (faithful blocks, same writer as the car textures:
//!   `crate::cartex::convert_zip`): SkyDither, Sun, Moon0-4, CloseCloud*, FarCloud*, FarCloudMask, Blank.
//! - `Stars.bin`, `CloudDefs.xml` copied as is (formats not decoded yet).

use std::path::Path;

use anyhow::Result;
use fh1_formats::zip::Archive;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let zip = disc.join("media/realtimesky.zip");
    let (ok, bad) = crate::cartex::convert_zip(&zip, out)?;
    let mut ar = Archive::open(&zip)?;
    for e in ar.entries.clone() {
        let name = e.name.replace(char::from(92), "/");
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".bin") || lower.ends_with(".xml") {
            std::fs::create_dir_all(out)?;
            std::fs::write(out.join(&name), ar.read(&e)?)?;
        }
    }
    println!("[sky] {ok} textures ({bad} failed)");
    Ok(())
}
