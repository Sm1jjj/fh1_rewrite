//! 16-bit PCM WAV, the converted-bank format (plain, so anything can play/inspect it).

use anyhow::{ensure, Context, Result};

pub fn write(path: &std::path::Path, rate: u32, channels: u16, pcm: &[i16]) -> Result<()> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend(b"RIFF");
    out.extend((36 + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(channels.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * channels as u32 * 2).to_le_bytes());
    out.extend((channels * 2).to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    for s in pcm {
        out.extend(s.to_le_bytes());
    }
    std::fs::write(path, out).with_context(|| path.display().to_string())
}

/// Reads a 16-bit PCM WAV into interleaved f32 in -1..1. Returns (rate, channels, samples).
pub fn read(path: &std::path::Path) -> Result<(u32, u16, Vec<f32>)> {
    let buf = std::fs::read(path).with_context(|| path.display().to_string())?;
    ensure!(buf.len() >= 12 && &buf[..4] == b"RIFF" && &buf[8..12] == b"WAVE", "{}: not WAV", path.display());
    let (mut rate, mut channels, mut pos) = (0u32, 0u16, 12usize);
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let len = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into()?) as usize;
        let body = buf.get(pos + 8..pos + 8 + len).context("truncated WAV chunk")?;
        if id == b"fmt " {
            ensure!(u16::from_le_bytes([body[0], body[1]]) == 1 && body[14] == 16, "only 16-bit PCM WAV");
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into()?);
        } else if id == b"data" {
            ensure!(channels > 0, "WAV data before fmt");
            let pcm = body.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
            return Ok((rate, channels, pcm));
        }
        pos += 8 + len + (len & 1);
    }
    anyhow::bail!("{}: no data chunk", path.display())
}
