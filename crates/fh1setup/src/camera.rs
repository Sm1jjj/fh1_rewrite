//! `camera` group: the gameplay camera tuning from `media/camera.zip`, copied as is for the engine's cameras
//! (fh1-engine camera.rs `CameraData`, parsed with `fh1_formats::camera`; docs/CAMERA.md).
//!
//! Output: `CameraSettings.ini`, `CameraPhysics.xml`, `CameraPhysicsSansEffects.xml`, `CameraGroups.xml`,
//! `CarRelativeCams.xml`.

use std::path::Path;

use anyhow::{Context, Result};
use fh1_formats::zip::Archive;

const FILES: &[&str] = &["CameraSettings.ini", "CameraPhysics.xml", "CameraPhysicsSansEffects.xml", "CameraGroups.xml", "CarRelativeCams.xml"];

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let mut ar = Archive::open(disc.join("media/camera.zip"))?;
    std::fs::create_dir_all(out)?;
    for name in FILES {
        let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned().with_context(|| format!("camera.zip: {name}"))?;
        std::fs::write(out.join(name), ar.read(&e)?)?;
    }
    println!("[camera] {} files", FILES.len());
    Ok(())
}
