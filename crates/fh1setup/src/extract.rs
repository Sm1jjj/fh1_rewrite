//! Source resolution: an ISO is extracted with extract-xiso; a folder or default.xex is used as is; a .zip holding
//! an ISO or an extracted disc is unpacked first; a folder that only *contains* one of those is searched.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

const XISO_URL: &str = "https://github.com/XboxDev/extract-xiso/releases/download/build-202505152050/extract-xiso-Win64_Release.zip";
const XISO_SHA256: &str = "fec88d03c7efd6205ab09be4abba70c0afd0eb27a5709f0a6235b828ba5ac11e";

/// Files every supported disc must have.
const REQUIRED: &[&str] = &["default.xex", "media/db/gamedb.slt", "media/cars"];

/// An FH1 disc: [`resolve_any`] plus a check for the files FH1's pipelines read.
pub fn resolve_disc(source: &Path, work: &Path) -> Result<PathBuf> {
    let root = resolve_any(source, work)?;
    for r in REQUIRED {
        ensure!(
            root.join(r).exists(),
            "not a supported Forza Horizon disc: missing {r} in {}",
            root.display()
        );
    }
    Ok(root)
}

/// Any Xbox 360 disc (FH1, FH2, FM4 play or content disc) as an extracted folder under `work` (or in place):
/// an ISO, a `.zip` holding an ISO or a disc folder, an extracted disc folder, `default.xex`, or a folder that holds
/// exactly one ISO / zip / disc folder (what users get from "point at the folder with your ISO").
pub fn resolve_any(source: &Path, work: &Path) -> Result<PathBuf> {
    ensure!(source.exists(), "{} does not exist", source.display());
    if source.is_dir() {
        if is_disc_root(source) {
            return Ok(source.to_path_buf());
        }
        let inner = find_inner(source)?;
        return resolve_any(&inner, work);
    }
    let ext = source.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if source.file_name().is_some_and(|n| n.eq_ignore_ascii_case("default.xex")) {
        Ok(source.parent().unwrap().to_path_buf())
    } else if ext.as_deref() == Some("iso") {
        extract_iso(source, work)
    } else if ext.as_deref() == Some("zip") {
        extract_zip(source, work)
    } else {
        bail!("{} is not an ISO, a .zip, a disc folder or default.xex", source.display());
    }
}

/// An extracted disc: `default.xex` (play discs) or a `content` / `Media` folder (content install discs).
fn is_disc_root(dir: &Path) -> bool {
    ["default.xex", "media", "content"].iter().any(|n| child_ci(dir, n).is_some())
}

/// `dir/name`, matching the name case-insensitively (discs mix `Media` and `media`).
fn child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// The one ISO, zip or disc folder inside `dir` (searched two levels deep). ISOs win over zips over folders, so a
/// folder holding both the ISO and a previous extraction still picks the ISO.
fn find_inner(dir: &Path) -> Result<PathBuf> {
    let (mut isos, mut zips, mut discs) = (Vec::new(), Vec::new(), Vec::new());
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if is_disc_root(&p) {
                    discs.push(p);
                } else if depth < 2 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
                Some("iso") => isos.push(p),
                Some("zip") => zips.push(p),
                _ => {}
            }
        }
    }
    for list in [isos, zips, discs] {
        match list.len() {
            0 => continue,
            1 => return Ok(list.into_iter().next().unwrap()),
            _ => bail!(
                "{} holds several disc images ({}); pick the one you want directly",
                dir.display(),
                list.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    bail!("no Xbox 360 disc (ISO, .zip or extracted folder) found in {}", dir.display())
}

/// Unpack a `.zip`: one ISO inside is streamed out and extracted (then deleted to save space); otherwise the zip
/// must hold an extracted disc (a `default.xex`, `Media/` or `Content/` at some prefix), which is unpacked as is.
fn extract_zip(zip_path: &Path, work: &Path) -> Result<PathBuf> {
    let file = File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut ar = zip::ZipArchive::new(std::io::BufReader::new(file)).with_context(|| format!("{} is not a readable zip", zip_path.display()))?;
    let names: Vec<String> = ar.file_names().map(str::to_owned).collect();
    let isos: Vec<&String> = names.iter().filter(|n| n.to_ascii_lowercase().ends_with(".iso")).collect();
    if isos.len() == 1 {
        let name = isos[0].clone();
        let dir = work.join("from_zip");
        std::fs::create_dir_all(&dir)?;
        let iso = dir.join(Path::new(&name).file_name().context("ISO name")?);
        let mut entry = ar.by_name(&name)?;
        let want = entry.size();
        if iso.metadata().map(|m| m.len()).ok() != Some(want) {
            println!("unpacking {name} from {} (several minutes)...", zip_path.display());
            let mut out = BufWriter::new(File::create(&iso)?);
            std::io::copy(&mut entry, &mut out).with_context(|| format!("unpacking {name}"))?;
        }
        drop(entry);
        let disc = extract_iso(&iso, work)?;
        let _ = std::fs::remove_file(&iso);
        return Ok(disc);
    }
    ensure!(isos.is_empty(), "{} holds several ISOs; unzip it and pick one", zip_path.display());

    // The disc root inside the zip: the folder holding default.xex, else Media/ or Content/.
    let prefix = |marker: &str| {
        names.iter().find_map(|n| {
            let l = n.replace('\\', "/");
            let lower = l.to_ascii_lowercase();
            let pos = if marker.ends_with('/') { lower.find(marker) } else { lower.ends_with(marker).then(|| lower.len() - marker.len()) }?;
            (pos == 0 || lower.as_bytes()[pos - 1] == b'/').then(|| l[..pos].to_owned())
        })
    };
    let root = prefix("default.xex").or_else(|| prefix("media/")).or_else(|| prefix("content/"));
    let root = root.with_context(|| format!("{} holds neither an ISO nor an extracted Xbox 360 disc", zip_path.display()))?;
    let out = work.join("disc_zip");
    let marker = out.join(".extracted");
    let id = format!("{}:{}", zip_path.canonicalize()?.display(), zip_path.metadata()?.len());
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(id.as_str()) {
        return Ok(out);
    }
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    println!("unpacking {} (several minutes)...", zip_path.display());
    for i in 0..ar.len() {
        let mut e = ar.by_index(i)?;
        let name = e.name().replace('\\', "/");
        let Some(rel) = name.strip_prefix(&root) else { continue };
        // enclosed_name rejects absolute paths and `..` (zip slip).
        if e.is_dir() || rel.is_empty() || e.enclosed_name().is_none() {
            continue;
        }
        let dst = out.join(rel);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::io::copy(&mut e, &mut BufWriter::new(File::create(&dst)?)).with_context(|| format!("unpacking {name}"))?;
    }
    std::fs::write(&marker, id)?;
    Ok(out)
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

/// extract-xiso: `FH1_EXTRACT_XISO`, else next to this exe (release builds ship it there), else the pinned build
/// downloaded into `dir` (verified by SHA-256).
fn extract_xiso(dir: &Path) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FH1_EXTRACT_XISO") {
        return Ok(p.into());
    }
    if let Some(p) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("extract-xiso.exe"))).filter(|p| p.is_file()) {
        return Ok(p);
    }
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
