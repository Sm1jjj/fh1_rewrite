//! Character VO (audio_inventory C4): `VO_<LANG>_Stream.fsb` MP3 clips and `DialogueScript_<LANG>.xml` triggers.
//!
//! Bevy-free and decoder-free (builds with `default-features = false`): setup copies the MP3s out of the bank, the
//! engine decodes them (`fh1-engine/src/vo.rs`, via fh1-radio's symphonia decoder).
//!
//! Bank layout (VERIFIED on VO_EN / VO_ES): FSB4, full sample headers (0x50 bytes, no SYNC extra), mode
//! 0x10000200 = MPEG | Layer 3, **mono**, 48 kHz (702 clips in EN; some MPEG-2 22.05 kHz clips in ES), each
//! sample's data 32-byte aligned and every MP3 frame padded to 4 bytes (the radio banks' layout, see
//! `fh1-radio/src/install.rs`; that code is copied here because fh1-radio depends on fh1-audio, not the reverse).
//! The bank has no `.lst`: the script's `soundbankIndex` is the sample index (VERIFIED: 701 distinct indices, all
//! < 702). `soundbankIndex="-1"` = a trigger line with no recording (`Countdown`, `Finish`: 16 lines, VERIFIED).
//!
//! Output under `<out>/dialogue/<LANG>/`: `NNN.mp3` for every sample (the unreferenced EN index and the ES extras
//! are kept) and `triggers.json` = `[{trigger, file, index, speaker}]` (one entry per recorded line; `text` is
//! never set: the script carries no transcript). `VO_SEASON_*` is not on the disc (VERIFIED) and is not produced.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

/// Languages on the EU disc.
pub const LANGS: [&str; 6] = ["DE", "EN", "ES", "FR", "IT", "NL"];

const MODE_MPEG: u32 = 0x0000_0200;
/// Alignment of each sample's data in the bank.
const DATA_ALIGN: usize = 32;

/// One recorded line of a trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptClip {
    /// Clip name, e.g. `Alice_Tutorial_230PopularityReached_1` (first `_` segment = speaker).
    pub name: String,
    /// Sample index in the bank; negative = not recorded.
    pub index: i32,
}

/// One `<Trigger id="...">` of the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub id: String,
    pub clips: Vec<ScriptClip>,
}

/// One `triggers.json` row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriggerEntry {
    pub trigger: String,
    /// Relative to `dialogue/<LANG>/`.
    pub file: String,
    pub index: u32,
    pub speaker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The speaker of a clip name (`AliHoward_StreetRace_..` -> `AliHoward`).
pub fn speaker_of(clip: &str) -> &str {
    clip.split('_').next().unwrap_or(clip)
}

/// Value of `name="..."` in a tag's text (attribute names are matched after a space).
fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let end = tag[at..].find('"')?;
    Some(unescape(&tag[at..at + end]))
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Parses `DialogueScript_<LANG>.xml` (`<Trigger id>` containing `<Filename id soundbankIndex/>`; a trigger may be
/// self-closing or hold only `-1` lines). A leading BOM is fine. Order is file order.
pub fn parse_script(xml: &str) -> Vec<Trigger> {
    let mut out: Vec<Trigger> = Vec::new();
    let mut rest = xml.trim_start_matches('\u{feff}');
    while let Some(lt) = rest.find('<') {
        rest = &rest[lt + 1..];
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[..gt];
        rest = &rest[gt + 1..];
        if tag.starts_with("Trigger ") {
            if let Some(id) = attr(tag, "id") {
                out.push(Trigger { id, clips: Vec::new() });
            }
        } else if tag.starts_with("Filename ") {
            let (Some(name), Some(t)) = (attr(tag, "id"), out.last_mut()) else { continue };
            let index = attr(tag, "soundbankIndex").and_then(|v| v.trim().parse().ok()).unwrap_or(-1);
            t.clips.push(ScriptClip { name, index });
        }
    }
    out
}

/// `triggers.json` rows for a script and a bank of `samples` clips (lines without a recording are dropped).
pub fn entries(script: &[Trigger], samples: usize) -> Result<Vec<TriggerEntry>> {
    let mut out = Vec::new();
    for t in script {
        for c in &t.clips {
            if c.index < 0 {
                continue;
            }
            ensure!((c.index as usize) < samples, "{}: {} has index {} but the bank has {samples} clips", t.id, c.name, c.index);
            out.push(TriggerEntry {
                trigger: t.id.clone(),
                file: format!("{:03}.mp3", c.index),
                index: c.index as u32,
                speaker: speaker_of(&c.name).to_owned(),
                text: None,
            });
        }
    }
    Ok(out)
}

/// A sample in the bank.
#[derive(Debug, Clone)]
pub struct RawClip {
    pub frames: u32,
    pub rate: u32,
    pub channels: u16,
    pub mode: u32,
    pub data: std::ops::Range<usize>,
}

/// Walks an FSB4 bank with full sample headers and 32-byte aligned data (module doc).
pub fn parse_bank(buf: &[u8]) -> Result<Vec<RawClip>> {
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
        data = data.next_multiple_of(DATA_ALIGN);
        let size = u16_at(off)? as usize;
        ensure!(size >= 0x50, "sample header {i} too small ({size})");
        let csize = u32_at(off + 0x24)? as usize;
        let c = RawClip { frames: u32_at(off + 0x20)?, mode: u32_at(off + 0x30)?, rate: u32_at(off + 0x34)?, channels: u16_at(off + 0x3E)?, data: data..data + csize };
        if c.data.end > buf.len() {
            bail!("sample {i} data runs past end of bank");
        }
        data = c.data.end;
        off += size;
        out.push(c);
    }
    Ok(out)
}

/// Byte length of the MPEG audio frame whose 4-byte header starts `h` (Layer III only).
fn mp3_frame_len(h: &[u8]) -> Option<usize> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 || (h[1] >> 1) & 3 != 1 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5
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

/// Strips FMOD's 4-byte frame padding (same rule as `fh1_radio::install::unpad_mp3`).
pub fn unpad_mp3(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos < data.len() {
        let Some(len) = mp3_frame_len(&data[pos..]) else {
            ensure!(data[pos..].iter().all(|&b| b == 0), "no MP3 frame at byte {pos} of {}", data.len());
            break;
        };
        let end = (pos + len).min(data.len());
        out.extend_from_slice(&data[pos..end]);
        pos = (pos + len).next_multiple_of(4);
    }
    Ok(out)
}

fn find_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(fh1_formats::path::resolve(dir)).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// Installs one language: `disc_audio` = `<disc>/media/audio`, `out` = `<assets>/audio`.
/// Writes `out/dialogue/<LANG>/NNN.mp3` (existing non-empty files are kept) and `triggers.json`.
pub fn install(disc_audio: &Path, out: &Path, lang: &str) -> Result<()> {
    let lang = lang.to_ascii_uppercase();
    let vo = disc_audio.join("vo");
    let fsb = find_ci(&vo, &format!("VO_{lang}_Stream.fsb")).with_context(|| format!("no VO_{lang}_Stream.fsb in {}", vo.display()))?;
    let xml = find_ci(&vo, &format!("DialogueScript_{lang}.xml")).with_context(|| format!("no DialogueScript_{lang}.xml in {}", vo.display()))?;
    let buf = std::fs::read(fh1_formats::path::resolve(&fsb)).with_context(|| format!("read {}", fsb.display()))?;
    let script = parse_script(&String::from_utf8_lossy(&std::fs::read(&xml).with_context(|| format!("read {}", xml.display()))?));
    let clips = parse_bank(&buf).with_context(|| fsb.display().to_string())?;
    let dir = out.join("dialogue").join(&lang);
    std::fs::create_dir_all(&dir)?;
    for (i, c) in clips.iter().enumerate() {
        ensure!(c.mode & MODE_MPEG != 0, "{lang} clip {i}: not MPEG (mode {:08x})", c.mode);
        let path = dir.join(format!("{i:03}.mp3"));
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            continue;
        }
        let mp3 = unpad_mp3(&buf[c.data.clone()]).with_context(|| format!("{lang} clip {i}"))?;
        std::fs::write(&path, mp3)?;
    }
    let rows = entries(&script, clips.len())?;
    std::fs::write(dir.join("triggers.json"), serde_json::to_vec_pretty(&rows)?)?;
    println!("  dialogue {lang}: {} clips, {} triggers, {} recorded lines", clips.len(), script.len(), rows.len());
    Ok(())
}

/// Installs the languages named by `FH1_VO_LANGS` (comma list, `ALL` = every language on the disc; default `EN`,
/// the engine's default; `FH1_VO_LANG` / `FH1_RADIO_LANG` pick the one played). A language missing on the disc is skipped.
pub fn install_default(disc_audio: &Path, out: &Path) -> Result<()> {
    let want = std::env::var("FH1_VO_LANGS").unwrap_or_else(|_| "EN".into());
    let langs: Vec<String> = if want.trim().eq_ignore_ascii_case("all") {
        LANGS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        want.split(',').map(|s| s.trim().to_ascii_uppercase()).filter(|s| !s.is_empty()).collect()
    };
    for l in &langs {
        if let Err(e) = install(disc_audio, out, l) {
            eprintln!("  dialogue {l}: {e:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_triggers_and_self_closing() {
        let xml = "\u{feff}<?xml version=\"1.0\"?>\n<DialogueScript>\n  <Trigger id=\"A\">\n    <Filename id=\"Alice_X_A_1\" soundbankIndex=\"5\" />\n    <Filename id=\"Alice_X_A_2\" soundbankIndex=\"-1\" />\n  </Trigger>\n  <Trigger id=\"B\" />\n</DialogueScript>";
        let t = parse_script(xml);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].clips, vec![ScriptClip { name: "Alice_X_A_1".into(), index: 5 }, ScriptClip { name: "Alice_X_A_2".into(), index: -1 }]);
        assert!(t[1].clips.is_empty());
        let e = entries(&t, 6).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].file.as_str(), e[0].speaker.as_str()), ("005.mp3", "Alice"));
        assert!(entries(&t, 5).is_err());
    }

    #[test]
    fn strips_frame_padding() {
        // Two MPEG-2 Layer III 22.05 kHz 32 kbps frames: 72*32000/22050 = 104 (+1 pad bit set in the first = 105),
        // so the padded layout is 105 -> 108 and 104 -> 104.
        let hdr = |pad: u8| [0xFF, 0xF3, 0x40 | (pad << 1), 0xC4];
        let len0 = mp3_frame_len(&hdr(1)).unwrap();
        let len1 = mp3_frame_len(&hdr(0)).unwrap();
        assert_eq!((len0, len1), (105, 104));
        let mut data = Vec::new();
        data.extend_from_slice(&hdr(1));
        data.resize(108, 0xAA);
        data.extend_from_slice(&hdr(0));
        data.resize(108 + 104, 0xBB);
        let out = unpad_mp3(&data).unwrap();
        assert_eq!(out.len(), 105 + 104);
        assert_eq!(&out[105..109], &hdr(0));
    }

    /// The disc's EN script: 435 triggers, 701 recorded clips (VERIFIED, audio_inventory C4).
    #[test]
    fn disc_script_en() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../disc/media/audio/vo/DialogueScript_EN.xml");
        let Ok(bytes) = std::fs::read(&p) else { return };
        let t = parse_script(&String::from_utf8_lossy(&bytes));
        assert_eq!(t.len(), 435);
        let mut idx: Vec<i32> = t.iter().flat_map(|t| t.clips.iter().map(|c| c.index)).filter(|&i| i >= 0).collect();
        assert_eq!(idx.len(), 701);
        idx.sort_unstable();
        idx.dedup();
        assert_eq!(idx.len(), 701);
        assert_eq!(entries(&t, 702).unwrap().len(), 701);
        // Empty triggers (no recording): Countdown and Finish.
        assert!(t.iter().find(|t| t.id == "Countdown").is_some_and(|t| t.clips.iter().all(|c| c.index < 0)));
    }
}
