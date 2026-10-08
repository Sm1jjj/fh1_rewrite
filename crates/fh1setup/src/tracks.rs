//! `tracks` group: per-track settings files the renderer reads at runtime.
//!
//! Output: `colorado/TimeOfDay{,A,B,Neutral}.xml` (lighting / fog / sky curves) and
//! `PostProcessingZones_Safe.xml` (post zones), all from `Ribbon_00`, and
//! `colorado/TrackSettings.xml`.

use std::path::Path;

use anyhow::Result;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    build_track(disc, "colorado", &out.join("colorado"))?;
    // Skid mark atlas (engine skidmarks.rs reads <assets>/tracks/treadmark.xds).
    std::fs::copy(disc.join("media/tracks/treadmark.xds"), out.join("treadmark.xds"))?;
    Ok(())
}

/// `media/tracks/<track>`'s settings files -> `dst` (FH2's `Anthem` has the same set, docs/FH2_RECON.md).
pub fn build_track(disc: &Path, track_name: &str, dst: &Path) -> Result<()> {
    let track = disc.join("media/tracks").join(track_name);
    std::fs::create_dir_all(dst)?;
    let mut n = 0;
    for name in ["TimeOfDay.xml", "TimeOfDayA.xml", "TimeOfDayB.xml", "TimeOfDayNeutral.xml", "PostProcessingZones_Safe.xml"] {
        std::fs::copy(track.join("Ribbon_00").join(name), dst.join(name))?;
        n += 1;
    }
    std::fs::copy(track.join("TrackSettings.xml"), dst.join("TrackSettings.xml"))?;
    println!("[tracks] {track_name}: {} files", n + 1);
    Ok(())
}
