//! Custom radio stations (mod): `mods/radio/stations/<Name>/*.mp3` (+ optional `logo.png`).
//!
//! Each folder becomes a music-only station inserted before the silent (Off) station, so the
//! D-pad goes Radio1 -> 2 -> 3 -> <mods...> -> Off. No DJ or idents: every track carries an
//! `IdentStart` sync at its last frame, and the system starts the next track when a station has
//! no ident to play. Title / artist come from the file name: `Artist - Title[ - Uploader]`,
//! or `Title - Artist` for two parts (the usual YouTube download names; MP3 tags are not read).
//! Read at radio start, never written into `radio.json`. `FH1_RADIO_MODS=off` skips them,
//! `FH1_RADIO_MODS=<dir>` points at another stations folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::config::{MusicTrack, Pool, Station};
use crate::install::{Clip, RadioData};

/// A station added from the mods folder.
#[derive(Debug, Clone)]
pub struct ModStation {
    /// Index in `RadioData::radio.stations`.
    pub index: usize,
    /// Folder name with `_` as spaces.
    pub display: String,
    pub logo: Option<PathBuf>,
}

/// `mods/radio/stations` in `from` or its nearest ancestor that has one (or `FH1_RADIO_MODS`).
pub fn find_dir(from: &Path) -> Option<PathBuf> {
    match std::env::var("FH1_RADIO_MODS") {
        Ok(v) if v.eq_ignore_ascii_case("off") => return None,
        Ok(v) if !v.is_empty() => return Some(PathBuf::from(v)),
        _ => {}
    }
    let cwd = std::env::current_dir().ok();
    let abs = std::path::absolute(from).ok();
    cwd.iter().chain(abs.iter()).flat_map(|p| p.ancestors()).map(|p| p.join("mods/radio/stations")).find(|p| p.is_dir())
}

/// Adds every station folder under `dir`. Returns how many were added.
pub fn add_stations(data: &mut RadioData, dir: &Path) -> Result<usize> {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    folders.sort();
    let stations = &data.radio.stations;
    let silent = stations.iter().position(|s| s.is_off).unwrap_or(stations.len());
    let Some(template) = stations.iter().find(|s| !s.is_off && !s.is_3d).cloned() else { return Ok(0) };
    let mut added = 0;
    for folder in folders {
        let Some(name) = folder.file_name().and_then(|n| n.to_str()).map(str::to_owned) else { continue };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&folder)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"))).collect();
        files.sort();
        let mut items = Vec::new();
        for f in files {
            let clip = match probe(&f) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("radio mod {name}: skipped {}: {e:#}", f.display());
                    continue;
                }
            };
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let key = format!("mod/{name}/{stem}");
            let (artist, title) = title_artist(stem);
            data.music.insert(key.clone(), clip);
            items.push(MusicTrack { title, artist, clip: key, likelihood: [1.0; 4] });
        }
        if items.is_empty() {
            continue;
        }
        let no_repeat = items.len() / 2;
        let station = Station {
            name: format!("Mod_{name}"),
            playlist: Pool { no_repeat, items },
            idents: Pool::default(),
            bookend_in: Pool::default(),
            bookend_out: Pool::default(),
            dj_regular: Pool::default(),
            dj_special: Vec::new(),
            dj_immediate: Vec::new(),
            festival_lead_in: Pool::default(),
            festival_lead_out: Pool::default(),
            ..template.clone()
        };
        let index = silent + added;
        data.radio.stations.insert(index, station);
        let logo = ["logo.png", "Logo.png", "logo.PNG"].iter().map(|l| folder.join(l)).find(|p| p.is_file());
        data.mods.push(ModStation { index, display: name.replace('_', " "), logo });
        added += 1;
    }
    Ok(added)
}

/// Frame count / rate / channels without decoding, plus an `IdentStart` at the end.
fn probe(path: &Path) -> Result<Clip> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut format = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())?.format;
    let track = format.default_track().context("no audio track")?;
    let (id, rate) = (track.id, track.codec_params.sample_rate.context("no sample rate")?);
    let channels = track.codec_params.channels.map_or(2, |c| c.count() as u16);
    let frames = match track.codec_params.n_frames {
        Some(n) => n,
        None => {
            let mut n = 0;
            while let Ok(p) = format.next_packet() {
                if p.track_id() == id {
                    n += p.dur;
                }
            }
            n
        }
    };
    anyhow::ensure!(frames > 0, "empty");
    let file = std::path::absolute(path)?.to_string_lossy().into_owned();
    Ok(Clip { file, frames, rate, channels, sync: BTreeMap::from([("IdentStart".to_owned(), frames)]) })
}

/// (artist, title) from a download-style file name.
fn title_artist(stem: &str) -> (String, String) {
    let parts: Vec<&str> = stem.split(" - ").map(str::trim).filter(|p| !p.is_empty()).collect();
    let (artist, title) = match parts.as_slice() {
        [] => (String::new(), stem.to_owned()),
        [one] => (String::new(), one.to_string()),
        [title, artist] => (artist.to_string(), title.to_string()),
        [artist, rest @ .., _uploader] => (artist.to_string(), rest.join(" - ")),
    };
    let (artist, mut title) = (clean(&artist), clean(&title));
    // "Yung Lean - Yung Lean Ginseng Strip 2002" -> "Ginseng Strip 2002".
    if !artist.is_empty() && title.len() > artist.len() + 1 && title.is_char_boundary(artist.len()) && title[..artist.len()].eq_ignore_ascii_case(&artist) {
        title = title[artist.len()..].trim_start_matches([' ', '-']).to_owned();
    }
    (artist, title)
}

/// Drops video-site noise: `(Official Video)`, `[Official Audio]`, a trailing `HD`, `♦`.
fn clean(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(['(', '[']) {
        let close = if rest[i..].starts_with('(') { ')' } else { ']' };
        let Some(j) = rest[i..].find(close).map(|j| i + j) else { break };
        let inner = rest[i + 1..j].to_ascii_lowercase();
        out.push_str(&rest[..i]);
        if !["official", "video", "audio", "lyric", "visualizer"].iter().any(|w| inner.contains(w)) {
            out.push_str(&rest[i..=j]);
        }
        rest = &rest[j + 1..];
    }
    out.push_str(rest);
    let out = out.replace('♦', " ");
    let mut out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.ends_with(" HD") {
        out.truncate(out.len() - 3);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::title_artist;

    #[test]
    fn names() {
        let t = |s| title_artist(s);
        assert_eq!(t("YE - KING - Kanye West"), ("YE".into(), "KING".into()));
        assert_eq!(t("Nine Years - Ticklah"), ("Ticklah".into(), "Nine Years".into()));
        assert_eq!(t("2Pac - Hit 'Em Up (Dirty) (Music Video) HD - Seven Hip-Hop"), ("2Pac".into(), "Hit 'Em Up (Dirty)".into()));
        assert_eq!(t("Shoreline Mafia - Touch Down [Official Audio] - Shoreline Mafia"), ("Shoreline Mafia".into(), "Touch Down".into()));
        assert_eq!(t("Yung Lean ♦ Ginseng Strip 2002 ♦ - Yung Lean"), ("Yung Lean".into(), "Ginseng Strip 2002".into()));
    }
}
