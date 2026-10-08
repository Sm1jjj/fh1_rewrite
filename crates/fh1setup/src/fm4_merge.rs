//! FM4 root from the two discs (Rust port of `tools/fm4/merge.py` + `stfs.py`, which it supersedes): a folder laid out
//! like an FH1 disc, which `fm4::run` reads.
//!
//! Steps (everything is written under `work`; idempotent, work already done is skipped):
//! 1. The Content Install Disc's STFS car packs (`content/0000000000000000/4d530910/00000002/*`) ->
//!    `work/disc2_content/<package>/` ([`Stfs`]).
//! 2. `work/merged/media`: junctions to the Play Disc's `Media/*`, except `cars/`, `wheels/` and `stringtables/`, which
//!    are real folders: the Play Disc's files (hard links, copies across volumes) + one stored zip per pack car / rim
//!    folder, read straight from each pack's `Media/DLCZips/000N_pri_65.zip` (LZX method 21; showroom `_SLOD` folders
//!    skipped; the Play Disc wins on duplicates, then the first pack) + the packs' `StringTables_pri_*.zip` in
//!    `stringtables/dlc/`.
//!
//! Without the Content Install Disc the merged root holds only the Play Disc's cars (no pack cars).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};

/// Play Disc `Media` folders rebuilt as real folders (the rest are junctions).
const REAL: &[&str] = &["cars", "wheels", "stringtables"];
/// Where the Content Install Disc keeps its STFS packages.
const PACKAGES: &[&str] = &["content", "0000000000000000", "4d530910", "00000002"];

/// Build (or reuse) `work/merged` from the extracted Play Disc and, optionally, the extracted Content Install Disc.
pub fn merge(disc1: &Path, disc2: Option<&Path>, work: &Path) -> Result<PathBuf> {
    let src = child_ci(disc1, "media");
    ensure!(
        src.join("db/gamedb.slt").exists(),
        "{}: no Media/db/gamedb.slt (not a Forza Motorsport 4 Play Disc)",
        disc1.display()
    );
    let root = work.join("merged");
    let m = root.join("media");
    // A merged view of other discs (moved or different folders) is rebuilt: its junctions point at the old source.
    let source = format!(
        "{}\n{}",
        std::path::absolute(disc1)?.display(),
        disc2.map(std::path::absolute).transpose()?.map(|p| p.display().to_string()).unwrap_or_default()
    );
    let marker = root.join(".source");
    if root.exists() && std::fs::read_to_string(&marker).ok().as_deref() != Some(source.as_str()) {
        println!("[fm4] rebuilding {} (different discs)", root.display());
        clear(&m)?;
    }
    std::fs::create_dir_all(&m)?;
    std::fs::write(&marker, &source)?;

    // 1. STFS packages.
    let mut packs = Vec::new();
    if let Some(disc2) = disc2 {
        let pkgdir = PACKAGES.iter().fold(disc2.to_path_buf(), |p, n| child_ci(&p, n));
        ensure!(
            pkgdir.is_dir(),
            "{}: no content/0000000000000000/4d530910/00000002 (not the Forza Motorsport 4 Content Install Disc)",
            disc2.display()
        );
        for pkg in sorted_names(&pkgdir)? {
            let out = work.join("disc2_content").join(&pkg);
            let done = out.join(".extracted");
            if !done.exists() {
                if out.exists() {
                    std::fs::remove_dir_all(&out)?;
                }
                let st = Stfs::open(&pkgdir.join(&pkg))?;
                let entries = st.entries()?;
                println!("[fm4] unpacking {pkg} ({}, {} files)", st.name, entries.iter().filter(|e| !e.dir).count());
                for e in entries.iter().filter(|e| !e.dir) {
                    let p = out.join(&e.path);
                    std::fs::create_dir_all(p.parent().unwrap())?;
                    let mut f = BufWriter::new(File::create(&p)?);
                    st.read(e, &mut f).with_context(|| format!("{pkg}: {}", e.path))?;
                    f.flush()?;
                }
                std::fs::write(&done, "")?;
            }
            let dlc = child_ci(&child_ci(&out, "media"), "DLCZips");
            if dlc.is_dir() {
                packs.push(dlc);
            }
        }
    }

    // 2. Play Disc media: junctions, then the real folders.
    for n in sorted_names(&src)? {
        let (s, d) = (src.join(&n), m.join(&n));
        if REAL.contains(&n.to_lowercase().as_str()) || d.symlink_metadata().is_ok() {
            continue;
        }
        if s.is_dir() {
            link_dir(&s, &d)?;
        } else {
            link_or_copy(&s, &d)?;
        }
    }
    std::fs::create_dir_all(m.join("carlights"))?;
    for n in sorted_names(&src)? {
        if !REAL.contains(&n.to_lowercase().as_str()) {
            continue;
        }
        let out = m.join(n.to_lowercase());
        std::fs::create_dir_all(&out)?;
        for f in sorted_names(&src.join(&n))? {
            let s = src.join(&n).join(&f);
            if s.is_file() && !out.join(&f).exists() {
                link_or_copy(&s, &out.join(&f))?;
            }
        }
    }

    // Pack cars / rims: one stored zip per folder.
    let mut stats: BTreeMap<&str, usize> = BTreeMap::new();
    for dlc in &packs {
        for z in sorted_names(dlc)? {
            if !z.ends_with("_pri_65.zip") {
                continue;
            }
            let path = dlc.join(&z);
            let mut ar = fh1_formats::zip::Archive::open(&path).with_context(|| path.display().to_string())?;
            for kind in ["cars", "wheels"] {
                // folder -> [(name inside the folder, entry)]
                let mut folders: BTreeMap<String, Vec<(String, fh1_formats::zip::Entry)>> = BTreeMap::new();
                for e in &ar.entries {
                    let name = e.name.replace('\\', "/");
                    let mut parts = name.splitn(4, '/');
                    let (Some(media), Some(k), Some(folder), Some(rest)) = (parts.next(), parts.next(), parts.next(), parts.next())
                    else {
                        continue;
                    };
                    if !media.eq_ignore_ascii_case("media") || !k.eq_ignore_ascii_case(kind) || rest.is_empty() || rest.ends_with('/') {
                        continue;
                    }
                    folders.entry(folder.to_owned()).or_default().push((rest.to_owned(), e.clone()));
                }
                for (folder, files) in folders {
                    let zpath = m.join(kind).join(format!("{folder}.zip"));
                    if folder.to_lowercase().contains("slod") || zpath.exists() {
                        continue;
                    }
                    let tmp = m.join(kind).join(format!("{folder}.zip.tmp"));
                    let mut zw = zip::ZipWriter::new(BufWriter::new(File::create(&tmp)?));
                    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
                    for (name, e) in &files {
                        let data = ar.read(e).with_context(|| format!("{}: {}", path.display(), e.name))?;
                        zw.start_file(name.as_str(), opts)?;
                        zw.write_all(&data)?;
                    }
                    zw.finish()?.flush()?;
                    std::fs::rename(&tmp, &zpath)?;
                    *stats.entry(kind).or_default() += 1;
                }
            }
        }
    }
    let dlc_out = m.join("stringtables/dlc");
    std::fs::create_dir_all(&dlc_out)?;
    for dlc in &packs {
        for z in sorted_names(dlc)? {
            if z.starts_with("StringTables") && !dlc_out.join(&z).exists() {
                link_or_copy(&dlc.join(&z), &dlc_out.join(&z))?;
            }
        }
    }
    println!(
        "[fm4] merged root {} ({} car packs; repacked {} cars, {} rims this run)",
        root.display(),
        packs.len(),
        stats.get("cars").unwrap_or(&0),
        stats.get("wheels").unwrap_or(&0)
    );
    Ok(root)
}

/// `dir/name`, matching an existing child case-insensitively (FM4's Play Disc has `Media`, the packs `Media/DLCZips`).
fn child_ci(dir: &Path, name: &str) -> PathBuf {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|e| e.path())
        .unwrap_or_else(|| dir.join(name))
}

fn sorted_names(dir: &Path) -> Result<Vec<String>> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| dir.display().to_string())?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<_>>()?;
    v.sort();
    Ok(v)
}

/// Remove a merged `media` folder without touching the discs: junctions / symlinks are removed as links, real folders
/// only hold hard links and our own zips.
fn clear(m: &Path) -> Result<()> {
    for e in std::fs::read_dir(m).into_iter().flatten().flatten() {
        let p = e.path();
        let meta = p.symlink_metadata()?;
        if meta.file_type().is_symlink() {
            std::fs::remove_dir(&p).or_else(|_| std::fs::remove_file(&p))?;
        } else if meta.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Hard link, or a copy when the link fails (another volume, FAT/exFAT).
fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if std::fs::hard_link(src, dst).is_err() {
        std::fs::copy(src, dst).with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
    }
    Ok(())
}

/// A directory junction (Windows) / symlink to `src`; where that fails, a tree of hard links (copies).
fn link_dir(src: &Path, dst: &Path) -> Result<()> {
    let src = std::path::absolute(src)?;
    #[cfg(windows)]
    let linked = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(dst)
        .arg(&src)
        .output()
        .is_ok_and(|o| o.status.success());
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&src, dst).is_ok();
    #[cfg(not(any(windows, unix)))]
    let linked = false;
    if !linked {
        link_tree(&src, dst)?;
    }
    Ok(())
}

fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for n in sorted_names(src)? {
        let (s, d) = (src.join(&n), dst.join(&n));
        if s.is_dir() {
            link_tree(&s, &d)?;
        } else if !d.exists() {
            link_or_copy(&s, &d)?;
        }
    }
    Ok(())
}

/// Xbox 360 STFS (CON/LIVE/PIRS) package reader: volume descriptor, block -> address with the hash-table interleave,
/// file table (port of `tools/fm4/stfs.py`).
pub struct Stfs {
    f: std::cell::RefCell<File>,
    /// 0 = female (one hash table per level), 1 = male (two).
    sex: u32,
    ft_blocks: u32,
    ft_start: u32,
    base: u64,
    step0: u64,
    /// Display name from the header.
    pub name: String,
}

pub struct StfsEntry {
    pub path: String,
    pub dir: bool,
    /// Blocks are consecutive (no hash-table chain walk needed).
    pub consecutive: bool,
    pub start: u32,
    pub size: u32,
}

const BLOCK: usize = 0x1000;

fn be24(b: &[u8]) -> u32 {
    (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32
}

fn le24(b: &[u8]) -> u32 {
    b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16
}

impl Stfs {
    pub fn open(path: &Path) -> Result<Self> {
        let mut f = File::open(path).with_context(|| path.display().to_string())?;
        let mut h = vec![0u8; 0xA000];
        f.read_exact(&mut h).with_context(|| format!("{}: STFS header", path.display()))?;
        let magic = &h[0..4];
        ensure!(magic == b"CON " || magic == b"LIVE" || magic == b"PIRS", "{}: not an STFS package", path.display());
        let hdr = u32::from_be_bytes(h[0x340..0x344].try_into().unwrap()) as u64;
        let vd = &h[0x379..0x379 + 0x24];
        let sex = (!vd[2] & 1) as u32;
        let ft_blocks = u16::from_le_bytes([vd[3], vd[4]]) as u32;
        let ft_start = le24(&vd[5..8]);
        let units: Vec<u16> = h[0x411..0x411 + 0x80].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        let name = String::from_utf16_lossy(&units);
        let name = name.split('\0').next().unwrap_or_default().to_owned();
        Ok(Self {
            f: std::cell::RefCell::new(f),
            sex,
            ft_blocks,
            ft_start,
            base: (hdr + 0xFFF) & 0xFFFF_F000,
            step0: if sex == 0 { 0xAB } else { 0xAC },
            name,
        })
    }

    /// Data block number -> backing block (skipping the interleaved hash tables).
    fn back(&self, b: u32) -> u64 {
        let (b, sex) = (b as u64, self.sex);
        let r = (((b + 0xAA) / 0xAA) << sex) + b;
        if b < 0xAA {
            r
        } else if b < 0x70E4 {
            r + (((b + 0x70E4) / 0x70E4) << sex)
        } else {
            (1 << sex) + r + (((b + 0x70E4) / 0x70E4) << sex)
        }
    }

    fn addr(&self, b: u32) -> u64 {
        (self.back(b) << 12) + self.base
    }

    /// Offset of block `b`'s level-0 hash entry.
    fn h0(&self, b: u32) -> u64 {
        let (b, sex) = (b as u64, self.sex);
        let mut n = 0;
        if b >= 0xAA {
            n = (b / 0xAA) * self.step0 + (((b / 0x70E4) + 1) << sex);
            if b / 0x70E4 != 0 {
                n += 1 << sex;
            }
        }
        (n << 12) + self.base + (b % 0xAA) * 0x18
    }

    /// Next block in a chain (from the hash entry).
    fn next(&self, b: u32) -> Result<u32> {
        let mut f = self.f.borrow_mut();
        f.seek(SeekFrom::Start(self.h0(b) + 0x15))?;
        let mut x = [0u8; 3];
        f.read_exact(&mut x)?;
        Ok(be24(&x))
    }

    /// One block (shorter at the end of the file).
    fn read_block(&self, b: u32) -> Result<Vec<u8>> {
        let mut f = self.f.borrow_mut();
        f.seek(SeekFrom::Start(self.addr(b)))?;
        let mut d = Vec::with_capacity(BLOCK);
        (&mut *f).take(BLOCK as u64).read_to_end(&mut d)?;
        Ok(d)
    }

    /// The file table, paths joined with `/`.
    pub fn entries(&self) -> Result<Vec<StfsEntry>> {
        // (name, parent slot, entry); `slots` maps each table slot (64 per block) to its entry.
        let mut raw: Vec<(String, i16, StfsEntry)> = Vec::new();
        let mut slots: Vec<Option<usize>> = Vec::new();
        let mut b = self.ft_start;
        for i in 0..self.ft_blocks {
            if i > 0 {
                b = self.next(b)?;
            }
            let d = self.read_block(b)?;
            ensure!(d.len() == BLOCK, "STFS file table block {b} truncated");
            for e in d.chunks_exact(64) {
                if e[0] == 0 {
                    slots.push(None);
                    continue;
                }
                let nl = (e[0x28] & 0x3F) as usize;
                let name: String = e[..nl].iter().map(|&c| c as char).collect();
                slots.push(Some(raw.len()));
                raw.push((
                    name,
                    i16::from_be_bytes([e[0x32], e[0x33]]),
                    StfsEntry {
                        path: String::new(),
                        dir: e[0x28] & 0x80 != 0,
                        consecutive: e[0x28] & 0x40 != 0,
                        start: le24(&e[0x2F..0x32]),
                        size: u32::from_be_bytes(e[0x34..0x38].try_into().unwrap()),
                    },
                ));
            }
        }
        let mut out = Vec::with_capacity(raw.len());
        for (name, parent, e) in &raw {
            let mut parts = vec![name.as_str()];
            let mut p = *parent;
            while p != -1 {
                let Some(&Some(i)) = slots.get(p as usize) else { bail!("STFS {name}: bad parent {p}") };
                parts.push(raw[i].0.as_str());
                p = raw[i].1;
                ensure!(parts.len() < 64, "STFS {name}: parent loop");
            }
            parts.reverse();
            out.push(StfsEntry { path: parts.join("/"), dir: e.dir, consecutive: e.consecutive, start: e.start, size: e.size });
        }
        Ok(out)
    }

    /// Write a file's bytes to `w`.
    pub fn read(&self, e: &StfsEntry, w: &mut impl Write) -> Result<()> {
        let (mut b, mut left) = (e.start, e.size as usize);
        while left > 0 {
            let d = self.read_block(b)?;
            let n = left.min(BLOCK);
            ensure!(d.len() >= n, "STFS block {b} truncated");
            w.write_all(&d[..n])?;
            left -= n;
            if left > 0 {
                b = if e.consecutive { b + 1 } else { self.next(b)? };
            }
        }
        Ok(())
    }
}
