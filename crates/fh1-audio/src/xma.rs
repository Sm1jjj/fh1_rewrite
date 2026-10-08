//! XMA → PCM. There is no Rust XMA decoder yet, so setup shells out to ffmpeg (its `xma2`
//! decoder handles FMOD's XMA2 streams; sample counts match the FSB headers exactly).
//! STOPGAP: users would need ffmpeg; a native decoder (or a bundled LGPL ffmpeg) replaces this.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// `FH1_FFMPEG`, else `ffmpeg` on PATH.
pub fn ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FH1_FFMPEG") {
        return Some(p.into());
    }
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| "ffmpeg".into())
}

/// Decodes an XMA2 RIFF (see [`crate::fsb::xma2_riff`]) to interleaved i16, trimmed or
/// zero-padded to exactly `frames` frames.
pub fn decode(ffmpeg: &std::path::Path, riff: Vec<u8>, channels: u16, frames: u32) -> Result<Vec<i16>> {
    let mut child = Command::new(ffmpeg)
        .args(["-v", "error", "-f", "wav", "-i", "pipe:0", "-f", "s16le", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn ffmpeg")?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&riff));
    let mut raw = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut raw)?;
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err)?;
    let status = child.wait()?;
    // A broken pipe here just means ffmpeg stopped reading early; its exit status decides.
    let _ = writer.join();
    if !status.success() || raw.is_empty() {
        bail!("ffmpeg failed: {}", err.trim());
    }
    let mut pcm: Vec<i16> = raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    pcm.resize(frames as usize * channels as usize, 0);
    Ok(pcm)
}
