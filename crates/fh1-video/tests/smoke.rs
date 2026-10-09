//! T3 smoke tests against the real disc files; they skip (with a note) without ffmpeg or the disc.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use fh1_video::{AudioSel, FfmpegVideo, OpenOpts, StreamDef};

fn videos() -> Option<PathBuf> {
    if let Err(e) = fh1_video::probe() {
        eprintln!("skip: {e}");
        return None;
    }
    let root = std::env::var_os("FH1_DISC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../disc"));
    let v = root.join("media/ui/videos");
    if v.join("FMV_01.wmv").is_file() {
        Some(v)
    } else {
        eprintln!("skip: no disc at {}", v.display());
        None
    }
}

/// Pulls audio at real-time pace and polls frames for `secs`; returns (frames, rms, bad_frames).
fn run(mv: &FfmpegVideo, secs: f64, rate: u32) -> (usize, f64, usize) {
    let src = mv.audio_source();
    let t0 = Instant::now();
    let (mut got, mut sum, mut n, mut frames, mut bad) = (0usize, 0f64, 0usize, 0usize, 0usize);
    let mut buf = vec![0f32; 512];
    while t0.elapsed().as_secs_f64() < secs {
        let want = (t0.elapsed().as_secs_f64() * rate as f64) as usize * 2;
        while got < want {
            let k = src.read(&mut buf);
            if k == 0 {
                break;
            }
            got += k;
            sum += buf[..k].iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
            n += k;
        }
        if let Some(f) = mv.frame() {
            frames += 1;
            if f.data.len() != 1280 * 720 * 4 || f.data.iter().all(|b| *b == 0) {
                bad += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    (frames, if n > 0 { (sum / n as f64).sqrt() } else { 0.0 }, bad)
}

#[test]
fn fmv01_en_audio_and_frames() {
    let Some(dir) = videos() else { return };
    let def = StreamDef::load(&dir.join("FMV_01.def")).unwrap();
    let sel = AudioSel::choose(Some(&def), "EN");
    assert_eq!(sel, AudioSel::Stream(5));
    let mv = FfmpegVideo::open(&dir.join("FMV_01.wmv"), OpenOpts { audio: sel, sample_rate: 48000, ..Default::default() }).unwrap();
    assert!(mv.duration().unwrap() > 135.0);
    let (frames, rms, bad) = run(&mv, 2.0, 48000);
    eprintln!("frames={frames} rms={rms:.5} clock={:.2} err={:?}", mv.clock(), mv.error());
    assert!(frames >= 20, "too few frames: {frames}");
    assert_eq!(bad, 0, "wrong size or all-zero frames");
    assert!(rms > 1e-4, "audio silent");
    assert!(!mv.finished());
    let t = Instant::now();
    drop(mv);
    assert!(t.elapsed() < Duration::from_secs(2), "drop took {:?}", t.elapsed());
}

#[test]
fn press_start_loops_with_first_audio() {
    let Some(dir) = videos() else { return };
    let mv = FfmpegVideo::open(
        &dir.join("PressStart.wmv"),
        OpenOpts { audio: AudioSel::FirstAudio, looping: true, ..Default::default() },
    )
    .unwrap();
    let (frames, _rms, bad) = run(&mv, 1.0, 48000);
    eprintln!("press start frames={frames} err={:?}", mv.error());
    assert!(frames >= 8);
    assert_eq!(bad, 0);
    assert!(!mv.finished());
}

#[test]
fn silent_movie_uses_wall_clock() {
    let Some(dir) = videos() else { return };
    let mv = FfmpegVideo::open(&dir.join("splash_intros/Dolby_Corona_Intro.wmv"), OpenOpts::default()).unwrap();
    let t0 = Instant::now();
    let mut frames = 0;
    while t0.elapsed() < Duration::from_millis(1500) {
        frames += mv.frame().is_some() as usize;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(frames >= 10, "frames={frames}");
    assert!((mv.clock() - 1.5).abs() < 0.5);
}
