//! Source resolution: an ISO is extracted with extract-xiso; a folder or default.xex is used as is.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

const XISO_URL: &str = "https://github.com/XboxDev/extract-xiso/releases/download/build-202505152050/extract-xiso-Win64_Release.zip";
const XISO_SHA256: &str = "fec88d03c7efd6205ab09be4abba70c0afd0eb27a5709f0a6235b828ba5ac11e";

/// Files every supported disc must have.
const REQUIRED: &[&str] = &["default.xex", "media/db/gamedb.slt", "media/cars"];

pub fn resolve_disc(source: &Path, work: &Path) -> Result<PathBuf> {
    let root = if source.is_dir() {
        source.to_path_buf()
    } else if source
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("default.xex"))
    {
        source.parent().unwrap().to_path_buf()
    } else if source
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
    {
        extract_iso(source, work)?
    } else {
        bail!("{} is not an ISO, a disc folder or default.xex", source.display());
    };
    for r in REQUIRED {
        ensure!(
            root.join(r).exists(),
            "not a supported Forza Horizon disc: missing {r} in {}",
            root.display()
        );
    }
    Ok(root)
}

fn extract_iso(iso: &Path, work: &Path) -> Result<PathBuf> {
    let out = work.join("disc");
    let marker = out.join(".extracted");
    let iso_id = format!("{}:{}", iso.canonicalize()?.display(), iso.metadata()?.len());
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(iso_id.as_str()) {
        return Ok(out);
    }
    let tool = extract_xiso(&work.join("tools"))?;
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    std::fs::create_dir_all(&out)?;
    println!("extracting {} (several minutes)...", iso.display());
    // extract-xiso expects all options before the ISO path.
    let status = Command::new(&tool).arg("-x").arg("-d").arg(&out).arg(iso).status()?;
    ensure!(status.success(), "extract-xiso failed ({status})");
    std::fs::write(&marker, iso_id)?;
    Ok(out)
}

/// Download the pinned extract-xiso build (verified by SHA-256) unless it's already there.
fn extract_xiso(dir: &Path) -> Result<PathBuf> {
    let exe = dir.join("extract-xiso.exe");
    if exe.is_file() {
        return Ok(exe);
    }
    std::fs::create_dir_all(dir)?;
    let zip_path = dir.join("extract-xiso.zip");
    println!("downloading extract-xiso...");
    let status = Command::new("curl")
        .args(["-sSfL", "-o"])
        .arg(&zip_path)
        .arg(XISO_URL)
        .status()
        .context("running curl")?;
    ensure!(status.success(), "downloading extract-xiso failed");
    let bytes = std::fs::read(&zip_path)?;
    let got = hex(&Sha256::digest(&bytes));
    ensure!(got == XISO_SHA256, "extract-xiso download hash mismatch: {got}");

    let mut ar = fh1_formats::zip::Archive::new(std::io::Cursor::new(bytes))?;
    let entry = ar
        .entries
        .iter()
        .find(|e| e.name.replace('\\', "/").ends_with("extract-xiso.exe"))
        .cloned()
        .context("extract-xiso.exe not in release zip")?;
    std::fs::write(&exe, ar.read(&entry)?)?;
    std::fs::remove_file(&zip_path)?;
    Ok(exe)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
