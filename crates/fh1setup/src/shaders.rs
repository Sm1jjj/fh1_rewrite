//! `shaders` group: the game's compiled shaders, so the renderer doesn't need the disc.
//!
//! Output: `media/<path>` = a copy of the disc's `media/shaders/**` (car, driver hands, track, v2), and
//! `track/<name>.fxobj` = the track shaders from Colorado's `bin.zip` (`shaders/track/*.fxobj`, names
//! lower-cased), which the scenery materials name; `xex/<addr:08x>.bin` = the 482 shader containers embedded
//! in `default.xex` (sky, clouds, post, shadow mask, particles, UI), decrypted by `fh1_shaders::xex`.

use std::path::Path;

use anyhow::Result;
use fh1_formats::zip::Archive;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let src = disc.join("media/shaders");
    let mut copied = 0;
    let mut stack = vec![src.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let dst = out.join("media").join(p.strip_prefix(&src)?);
                std::fs::create_dir_all(dst.parent().unwrap())?;
                std::fs::copy(&p, &dst)?;
                copied += 1;
            }
        }
    }
    let track = track_shaders(&disc.join("media/tracks/colorado/bin.zip"), &out.join("track"))?;
    let xex = fh1_shaders::xex::load(&std::fs::read(disc.join("default.xex"))?)?;
    std::fs::create_dir_all(out.join("xex"))?;
    let mut embedded = 0;
    for (addr, bytes) in xex.shader_containers() {
        std::fs::write(out.join("xex").join(format!("{addr:08x}.bin")), bytes)?;
        embedded += 1;
    }
    println!("[shaders] {copied} files from media/shaders, {track} track .fxobj, {embedded} from default.xex");
    Ok(())
}

/// A track `bin.zip`'s `shaders/track/*.fxobj` (names lower-cased) -> `dst`. Returns how many were written.
/// FH2's Anthem has its own builds of same-named shaders (docs/FH2_RECON.md), so each game keeps its own folder.
pub fn track_shaders(bin_zip: &Path, dst: &Path) -> Result<usize> {
    let mut ar = Archive::open(bin_zip)?;
    std::fs::create_dir_all(dst)?;
    let mut track = 0;
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase().replace(char::from(92), "/");
        if let Some(name) = n.strip_prefix("shaders/track/").filter(|n| n.ends_with(".fxobj")) {
            let dst = dst.join(name);
            if !dst.exists() {
                std::fs::write(dst, ar.read(&e)?)?;
                track += 1;
            }
        }
    }
    Ok(track)
}
