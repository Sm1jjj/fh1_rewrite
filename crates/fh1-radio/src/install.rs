//! Disc → converted radio (the setup tool's `radio` group).
//!
//! The radio banks (`media/audio/radio/Radio_Music.fsb`, `Radio_VO_<LANG>.fsb`) are FSB4 like
//! the car banks, but their samples are **MP3** (mode 0x90000200 = MPEG | MPEG_LAYER3 |
//! SYNCPOINTS; LAME 3.98.4, CBR; 48 kHz MPEG-1, or 22050 Hz MPEG-2 for 58 VO clips), not XMA.
//! Each sample's data is 32-byte aligned and every frame is padded to 4 bytes; setup strips
//! the frame padding and writes plain `.mp3`s: no transcoding, no ffmpeg.
//!
//! Sample names in FSB headers are cut at 30 bytes; the full names come from the `.lst` next
//! to each bank (line *n* = sample *n*, which is also `SoundBankIndex` in
//! `RadioSoundbankInfo_*.xml`).
//!
//! Sync points live in the sample header's extra bytes (after the 0x50-byte base header):
//! `"SYNC"`, u32 count, then count × (u32 offset in frames, 256-byte name). Music uses
//! `SongStart` / `EventStart` / `IdentStart`, idents `StartNextTrack`; their times equal the
//! `Offset*` millisecond columns of `RadioSoundbankInfo_*.xml` (checked in `tests/disc.rs`).
//!
//! Output:
//! - `radio.json`: [`RadioData`] (the parsed `RadioSystem.xml` plus every clip's info).
//! - `music/<clip>.mp3`, `vo/<LANG>/<clip>.mp3`.
//! - `radio.json` also carries the radio's two mixer channels per snapshot ([`crate::snapshots`]).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, RadioSystem};

/// One playable clip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    /// Path relative to the `radio` folder.
    pub file: String,
    pub frames: u64,
    pub rate: u32,
    pub channels: u16,
    /// Sync point name → offset in frames.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sync: BTreeMap<String, u64>,
}

impl Clip {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.rate as f64
    }
    /// A sync point in seconds.
    pub fn sync_s(&self, name: &str) -> Option<f64> {
        self.sync.get(name).map(|&f| f as f64 / self.rate as f64)
    }
}

pub type Bank = BTreeMap<String, Clip>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioData {
    pub radio: RadioSystem,
    pub music: Bank,
    /// Language code (`EN`, `DE`, ...) → that language's VO bank (DJ lines and idents).
    pub vo: BTreeMap<String, Bank>,
    /// The radio's mixer channels per snapshot (`media/audio/AudioMixerSnapshots.xml`).
    #[serde(default)]
    pub snapshots: crate::snapshots::Snapshots,
    /// Stations added from the mods folder at load ([`crate::mods`]); never saved.
    #[serde(skip)]
    pub mods: Vec<crate::mods::ModStation>,
}

impl RadioData {
    pub fn load(radio_dir: &Path) -> Result<RadioData> {
        let path = radio_dir.join("radio.json");
        serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("read {}", path.display()))?)
            .with_context(|| format!("parse {}", path.display()))
    }
}

/// A sample as laid out in the bank, with what the radio needs on top of `fh1_audio::fsb`.
#[derive(Debug, Clone)]
pub struct RawSample {
    pub frames: u64,
    pub rate: u32,
    pub channels: u16,
    pub mode: u32,
    pub sync: BTreeMap<String, u64>,
    pub data: std::ops::Range<usize>,
}

pub const MODE_MPEG: u32 = 0x0000_0200;
pub const MODE_SYNCPOINTS: u32 = 0x8000_0000;
/// Alignment of each sample's data in the radio banks.
pub const DATA_ALIGN: usize = 32;
/// Sync points the radio uses.
pub const SYNC_NAMES: [&str; 4] = ["SongStart", "EventStart", "IdentStart", "StartNextTrack"];

/// Walks an FSB4 bank's full sample headers (the radio banks never use "basic headers").
pub fn parse_bank(buf: &[u8]) -> Result<Vec<RawSample>> {
    ensure!(buf.len() >= 48 && &buf[..4] == b"FSB4", "not an FSB4 bank");
    let u32_at = |o: usize| -> Result<u32> { Ok(u32::from_le_bytes(buf.get(o..o + 4).context("truncated FSB")?.try_into()?)) };
    let u16_at = |o: usize| -> Result<u16> { Ok(u16::from_le_bytes(buf.get(o..o + 2).context("truncated FSB")?.try_into()?)) };
    let count = u32_at(4)? as usize;
    let headers_len = u32_at(8)? as usize;
    ensure!(u32_at(20)? & 0x02 == 0, "bank uses basic headers");
    let mut off = 48;
    let mut data = 48 + headers_len;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        // Each sample's data starts on a 32-byte boundary. Music clips happen to be multiples
        // of 32 bytes, VO clips mostly aren't (verified: with this, every sample in all seven
        // radio banks starts on an MP3 frame header and the walk ends at the end of the file).
        data = data.next_multiple_of(DATA_ALIGN);
        let size = u16_at(off)? as usize;
        ensure!(size >= 0x50, "sample header {i} too small ({size})");
        let csize = u32_at(off + 0x24)? as usize;
        let mut sync = BTreeMap::new();
        let extra = buf.get(off + 0x50..off + size).context("truncated sample header")?;
        if extra.len() >= 8 && &extra[..4] == b"SYNC" {
            let n = u32::from_le_bytes(extra[4..8].try_into()?) as usize;
            for k in 0..n {
                let e = extra.get(8 + k * 260..8 + (k + 1) * 260).context("truncated SYNC")?;
                let at = u32::from_le_bytes(e[..4].try_into()?) as u64;
                let name = &e[4..];
                let name = String::from_utf8_lossy(&name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())]).into_owned();
                // VO clips also carry a sync point named after the clip; keep the meaningful
                // ones, case-normalised (one track spells it `SongSTart`).
                if let Some(known) = SYNC_NAMES.iter().find(|k| k.eq_ignore_ascii_case(&name)) {
                    sync.insert((*known).to_owned(), at);
                }
            }
        }
        let s = RawSample {
            frames: u32_at(off + 0x20)? as u64,
            mode: u32_at(off + 0x30)?,
            rate: u32_at(off + 0x34)?,
            channels: u16_at(off + 0x3E)?,
            sync,
            data: data..data + csize,
        };
        if s.data.end > buf.len() {
            bail!("sample {i} data runs past end of bank");
        }
        data = s.data.end;
        off += size;
        out.push(s);
    }
    Ok(out)
}

/// Sample names from a `.lst` (file stems of the source WAVs, one per line).
pub fn parse_lst(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let path = l.split(',').next().unwrap_or(l).trim();
            let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
            file.strip_suffix(".wav").or_else(|| file.strip_suffix(".WAV")).unwrap_or(file).to_owned()
        })
        .collect()
}

/// Byte length of the MPEG audio frame whose 4-byte header starts `h` (Layer III only).
fn mp3_frame_len(h: &[u8]) -> Option<usize> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 || (h[1] >> 1) & 3 != 1 {
        return None;
    }
    // Version bits: 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5.
    let version = (h[1] >> 3) & 3;
    let mpeg1 = version == 3;
    const BR1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BR2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let bri = (h[2] >> 4) as usize;
    let sri = ((h[2] >> 2) & 3) as usize;
    if bri == 0 || bri == 15 || sri == 3 || version == 1 {
        return None;
    }
    let kbps = if mpeg1 { BR1[bri] } else { BR2[bri] };
    let rate = [44100, 48000, 32000][sri] >> match version {
        3 => 0,
        2 => 1,
        _ => 2,
    };
    let pad = ((h[2] >> 1) & 1) as u32;
    Some(((if mpeg1 { 144 } else { 72 }) * kbps * 1000 / rate + pad) as usize)
}

/// FMOD pads every MP3 frame in these banks to a multiple of 4 bytes (verified: 22050 Hz
/// MPEG-2 frames of 261/262 bytes sit 264 apart; the 48 kHz frames are 240/480 bytes, so
/// unaffected). Decoders choke on the padding, so it is stripped here.
pub fn unpad_mp3(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos < data.len() {
        let Some(len) = mp3_frame_len(&data[pos..]) else {
            // Trailing zero padding after the last frame.
            ensure!(data[pos..].iter().all(|&b| b == 0), "no MP3 frame at byte {pos} of {}", data.len());
            break;
        };
        let end = (pos + len).min(data.len());
        out.extend_from_slice(&data[pos..end]);
        pos = (pos + len).next_multiple_of(4);
    }
    Ok(out)
}

/// Copies one bank's samples to `out/<sub>/<name>.mp3` and returns its clip table.
fn convert_bank(fsb: &Path, out: &Path, sub: &str) -> Result<Bank> {
    let buf = std::fs::read(fh1_formats::path::resolve(fsb)).with_context(|| format!("read {}", fsb.display()))?;
    let lst = fsb.with_extension("lst");
    let names = parse_lst(&std::fs::read_to_string(&lst).with_context(|| format!("read {}", lst.display()))?);
    let samples = parse_bank(&buf)?;
    ensure!(names.len() == samples.len(), "{}: {} names for {} samples", lst.display(), names.len(), samples.len());
    std::fs::create_dir_all(out.join(sub))?;
    let mut bank = Bank::new();
    for (name, s) in names.into_iter().zip(samples) {
        ensure!(s.mode & MODE_MPEG != 0, "{name}: not MPEG (mode {:08x})", s.mode);
        let file = format!("{sub}/{name}.mp3");
        let mp3 = unpad_mp3(&buf[s.data.clone()]).with_context(|| format!("{}: {name}", fsb.display()))?;
        std::fs::write(out.join(&file), mp3)?;
        bank.insert(name, Clip { file, frames: s.frames, rate: s.rate, channels: s.channels, sync: s.sync });
    }
    Ok(bank)
}

/// Finds `name` in `dir` ignoring case.
fn find_ci(dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// `disc` = extracted disc root (has `media/`).
pub fn build(disc: &Path, out: &Path) -> Result<()> {
    let dir = disc.join("media/audio/radio");
    let xml = find_ci(&dir, "RadioSystem.xml").with_context(|| format!("no RadioSystem.xml in {}", dir.display()))?;
    let radio = config::parse(&std::fs::read_to_string(&xml)?).context("RadioSystem.xml")?;

    let music = convert_bank(&find_ci(&dir, "Radio_Music.fsb").context("no Radio_Music.fsb")?, out, "music")?;
    println!("  radio: {} music clips", music.len());

    // Every Radio_VO_<LANG>.fsb on the disc (the EU disc has DE EN ES FR IT NL).
    let mut vo = BTreeMap::new();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let lower = name.to_ascii_lowercase();
        let Some(lang) = lower.strip_prefix("radio_vo_").and_then(|r| r.strip_suffix(".fsb")) else { continue };
        let lang = lang.to_ascii_uppercase();
        let bank = convert_bank(&p, out, &format!("vo/{lang}"))?;
        println!("  radio: VO {lang}: {} clips", bank.len());
        vo.insert(lang, bank);
    }
    ensure!(!vo.is_empty(), "no Radio_VO_*.fsb in {}", dir.display());

    // Report config references the banks can't satisfy (blank placeholders aside).
    let en = vo.get("EN").or_else(|| vo.values().next()).unwrap();
    let mut missing = Vec::new();
    for st in &radio.stations {
        missing.extend(st.playlist.items.iter().map(|t| &t.clip).filter(|c| !music.contains_key(*c)));
        let vo_refs = st.idents.items.iter()
            .chain(st.dj_regular.items.iter().map(|d| &d.clip))
            .chain(st.dj_special.iter().chain(&st.dj_immediate).map(|e| &e.clip))
            .chain(st.festival_lead_in.items.iter().chain(&st.festival_lead_out.items));
        missing.extend(vo_refs.filter(|c| !config::is_blank(c) && !en.contains_key(*c)));
    }
    missing.extend(radio.festival_updates.iter().map(|e| &e.clip).filter(|c| !en.contains_key(*c)));
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        println!("  radio: {} referenced clips not in the banks (skipped at runtime): {:?}", missing.len(), missing);
    }

    let snap_xml = disc.join("media/audio/AudioMixerSnapshots.xml");
    let snapshots = crate::snapshots::Snapshots::parse(&std::fs::read_to_string(&snap_xml).with_context(|| format!("read {}", snap_xml.display()))?)
        .context("AudioMixerSnapshots.xml")?;
    println!("  radio: {} mixer snapshots", snapshots.list.len());

    let data = RadioData { radio, music, vo, snapshots, mods: Vec::new() };
    std::fs::write(out.join("radio.json"), serde_json::to_vec_pretty(&data)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn lst_names() {
        let n = super::parse_lst("D:\\p4\\x\\music\\R1_Blue.wav, quality=0, fsound_loop_off\r\n\r\nD:/y/Z.wav, q\n");
        assert_eq!(n, ["R1_Blue", "Z"]);
    }
}
