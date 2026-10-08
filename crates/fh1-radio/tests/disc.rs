//! Checks against the user's disc (`disc/` at the workspace root or `FH1_DISC`); skips without it.

use std::path::PathBuf;

use fh1_radio::install::{parse_bank, parse_lst, unpad_mp3};

fn radio_dir() -> Option<PathBuf> {
    let disc = std::env::var_os("FH1_DISC").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("../../disc"));
    let dir = disc.join("media/audio/radio");
    dir.join("RadioSystem.xml").is_file().then_some(dir)
}

/// `<Bank Name=..><SoundBankInfo SoundBankIndex=.. Offset*=.. FileLength=../>` rows.
fn soundbank_info(xml: &str) -> Vec<(String, usize, [i64; 5])> {
    let attr = |s: &str, k: &str| -> String {
        let i = s.find(&format!("{k}=\"")).unwrap() + k.len() + 2;
        s[i..i + s[i..].find('"').unwrap()].to_owned()
    };
    xml.split("<Bank ")
        .skip(1)
        .map(|b| {
            let n = |k: &str| attr(b, k).parse::<i64>().unwrap();
            (
                attr(b, "Name"),
                n("SoundBankIndex") as usize,
                [n("OffsetSongStart"), n("OffsetEventStart"), n("OffsetIdentStart"), n("OffsetStartNextTrack"), n("FileLength")],
            )
        })
        .collect()
}

#[test]
fn config_parses() {
    let Some(dir) = radio_dir() else { return eprintln!("no disc, skipping") };
    let r = fh1_radio::config::parse(&std::fs::read_to_string(dir.join("RadioSystem.xml")).unwrap()).unwrap();
    let names: Vec<&str> = r.stations.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(&names[..4], ["Radio1", "Radio2", "Radio3", "Radio4_Silent"]);
    assert_eq!(r.stations.len(), 9);
    assert!(r.stations[3].is_off);
    assert_eq!(r.stations.iter().filter(|s| s.is_3d).count(), 5);
    assert!(r.stations[..3].iter().all(|s| s.playlist.items.len() >= 20 && !s.dj_regular.items.is_empty()));
}

/// The FSB sync points (frames) equal RadioSoundbankInfo's millisecond offsets, and the names
/// from the .lst line up with SoundBankIndex.
#[test]
fn sync_points_match_soundbank_info() {
    let Some(dir) = radio_dir() else { return eprintln!("no disc, skipping") };
    // Music: SongStart/EventStart/IdentStart on every track; VO: StartNextTrack on the idents.
    for (fsb, info, min) in [("Radio_Music", "RadioSoundbankInfo_Music.xml", 300), ("Radio_VO_EN", "RadioSoundbankInfo_VO_EN.xml", 40)] {
        let bank = std::fs::read(dir.join(format!("{fsb}.fsb"))).unwrap();
        let samples = parse_bank(&bank).unwrap();
        let names = parse_lst(&std::fs::read_to_string(dir.join(format!("{fsb}.lst"))).unwrap());
        assert_eq!(names.len(), samples.len());
        let rows = soundbank_info(&std::fs::read_to_string(dir.join(info)).unwrap());
        assert_eq!(rows.len(), samples.len(), "{info}");
        let mut checked = 0;
        for (name, idx, [song, event, ident, next, len]) in rows {
            assert!(names[idx].eq_ignore_ascii_case(&name), "{info}: index {idx} = {} not {name}", names[idx]);
            let s = &samples[idx];
            let ms = |frames: u64| (frames * 1000 / s.rate as u64) as i64;
            assert!((ms(s.frames) - len).abs() <= 1, "{name}: length {} vs {len} ms", ms(s.frames));
            for (key, want) in [("SongStart", song), ("EventStart", event), ("IdentStart", ident), ("StartNextTrack", next)] {
                if let Some(&f) = s.sync.get(key) {
                    assert!((ms(f) - want).abs() <= 1, "{name}.{key}: {} vs {want} ms", ms(f));
                    checked += 1;
                }
            }
        }
        assert!(checked >= min, "{fsb}: only {checked} sync points");
        eprintln!("{fsb}: {checked} sync points match");
    }
}

/// Every sample's data starts on an MP3 frame header, in every radio bank (catches a wrong
/// data alignment, which silently cuts VO clips).
#[test]
fn every_sample_starts_on_an_mp3_frame() {
    let Some(dir) = radio_dir() else { return eprintln!("no disc, skipping") };
    let mut banks = 0;
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let p = e.path();
        if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("fsb")) {
            continue;
        }
        let buf = std::fs::read(&p).unwrap();
        let samples = parse_bank(&buf).unwrap();
        for (i, s) in samples.iter().enumerate() {
            let h = &buf[s.data.start..s.data.start + 2];
            assert!(h[0] == 0xFF && h[1] & 0xE0 == 0xE0, "{}: sample {i} starts with {h:02x?}", p.display());
            unpad_mp3(&buf[s.data.clone()]).unwrap_or_else(|e| panic!("{}: sample {i}: {e:#}", p.display()));
        }
        let end = samples.last().unwrap().data.end;
        assert!(buf.len() - end < 32, "{}: {} bytes after the last sample", p.display(), buf.len() - end);
        banks += 1;
    }
    assert_eq!(banks, 7);
}

/// Every EN VO clip (48 kHz MPEG-1 and 22050 Hz MPEG-2) decodes to its header's length with the
/// runtime decoder, once unpadded.
#[test]
fn en_vo_decodes_to_full_length() {
    let Some(dir) = radio_dir() else { return eprintln!("no disc, skipping") };
    let buf = std::fs::read(dir.join("Radio_VO_EN.fsb")).unwrap();
    let names = parse_lst(&std::fs::read_to_string(dir.join("Radio_VO_EN.lst")).unwrap());
    let tmp = std::env::temp_dir().join(format!("fh1_radio_test_{}.mp3", std::process::id()));
    let mut low_rate = 0;
    for (name, s) in names.iter().zip(parse_bank(&buf).unwrap()) {
        std::fs::write(&tmp, unpad_mp3(&buf[s.data.clone()]).unwrap()).unwrap();
        let mut dec = fh1_radio::decode::Mp3Stream::open(&tmp, 0).unwrap();
        let mut pcm = Vec::new();
        while dec.decode_into(&mut pcm) {}
        let got = (pcm.len() / 2) as i64;
        // MP3 decoders add the encoder delay/padding (under ~2 frames of 1152).
        assert!((got - s.frames as i64).abs() < 2400, "{name}: decoded {got} frames, header says {}", s.frames);
        low_rate += (s.rate == 22050) as usize;
    }
    let _ = std::fs::remove_file(&tmp);
    assert!(low_rate > 50, "only {low_rate} 22050 Hz clips");
}
