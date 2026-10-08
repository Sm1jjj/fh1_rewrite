//! FMOD Ex sample banks (`.fsb`, version 4) as shipped on the FH1 disc.
//!
//! Layout (little-endian, verified against the disc's banks):
//! - 48-byte header: `"FSB4"`, sample count, sample-header bytes, data bytes, version
//!   (0x00040000), bank mode, 8 zero bytes, 16-byte hash.
//! - Sample headers back to back: u16 size, 30-byte name, frame count, compressed bytes,
//!   loop start/end, mode, default rate, vol/pan/pri, channels, min/max distance,
//!   var freq/vol/pan (0x50 bytes) + codec extra (0x20 bytes for XMA).
//!   With bank mode bit 0x02 ("basic headers") every sample after the first is only
//!   frame count + compressed bytes and inherits the rest from the first.
//! - Sample data back to back, starting right after the headers (no padding on FH1 banks).
//!
//! Every car/road sample is mode 0x01000000 = XMA (36 kHz); radio banks use other codecs.

use anyhow::{bail, ensure, Context, Result};

pub const MODE_XMA: u32 = 0x0100_0000;
const BANK_BASIC_HEADERS: u32 = 0x02;

#[derive(Debug, Clone)]
pub struct Sample {
    pub name: String,
    pub frames: u32,
    pub loop_start: u32,
    pub loop_end: u32,
    pub mode: u32,
    pub rate: u32,
    pub channels: u16,
    /// Byte range of the compressed data inside the bank file.
    pub data: std::ops::Range<usize>,
}

pub fn parse(buf: &[u8]) -> Result<Vec<Sample>> {
    ensure!(buf.len() >= 48 && &buf[..4] == b"FSB4", "not an FSB4 bank");
    let u32_at = |o: usize| -> Result<u32> {
        Ok(u32::from_le_bytes(buf.get(o..o + 4).context("truncated FSB")?.try_into()?))
    };
    let u16_at = |o: usize| -> Result<u16> {
        Ok(u16::from_le_bytes(buf.get(o..o + 2).context("truncated FSB")?.try_into()?))
    };
    let count = u32_at(4)? as usize;
    let headers_len = u32_at(8)? as usize;
    let bank_mode = u32_at(20)?;
    let mut off = 48;
    let mut data = 48 + headers_len;
    let mut out: Vec<Sample> = Vec::with_capacity(count);
    for i in 0..count {
        let s = if i > 0 && bank_mode & BANK_BASIC_HEADERS != 0 {
            let first = &out[0];
            let frames = u32_at(off)?;
            let csize = u32_at(off + 4)? as usize;
            off += 8;
            Sample {
                name: format!("{}_{i}", first.name),
                frames,
                loop_start: 0,
                loop_end: frames.saturating_sub(1),
                data: data..data + csize,
                ..first.clone()
            }
        } else {
            let size = u16_at(off)? as usize;
            ensure!(size >= 0x50, "sample header {i} too small ({size})");
            let name_raw = &buf[off + 2..off + 32];
            let name_end = name_raw.iter().position(|&b| b == 0).unwrap_or(30);
            let csize = u32_at(off + 0x24)? as usize;
            let s = Sample {
                name: String::from_utf8_lossy(&name_raw[..name_end]).into_owned(),
                frames: u32_at(off + 0x20)?,
                loop_start: u32_at(off + 0x28)?,
                loop_end: u32_at(off + 0x2C)?,
                mode: u32_at(off + 0x30)?,
                rate: u32_at(off + 0x34)?,
                channels: u16_at(off + 0x3E)?,
                data: data..data + csize,
            };
            off += size;
            s
        };
        if s.data.end > buf.len() {
            bail!("sample {} data runs past end of bank", s.name);
        }
        data = s.data.end;
        out.push(s);
    }
    Ok(out)
}

/// Wraps one XMA sample in a RIFF/WAVE file with an XMA2WAVEFORMATEX header, the form
/// ffmpeg's xma2 decoder reads. FMOD stores XMA2 with 32 KiB blocks.
pub fn xma2_riff(bank: &[u8], s: &Sample) -> Vec<u8> {
    const BLOCK: u32 = 0x8000;
    let data = &bank[s.data.clone()];
    let ch = s.channels.max(1);
    let mut fmt = Vec::with_capacity(52);
    fmt.extend(0x166u16.to_le_bytes()); // WAVE_FORMAT_XMA2
    fmt.extend(ch.to_le_bytes());
    fmt.extend(s.rate.to_le_bytes());
    fmt.extend((s.rate * ch as u32 * 2).to_le_bytes()); // avg bytes/s (informational)
    fmt.extend(2048u16.to_le_bytes()); // block align (informational)
    fmt.extend(16u16.to_le_bytes());
    fmt.extend(34u16.to_le_bytes()); // cbSize
    fmt.extend(ch.div_ceil(2).to_le_bytes()); // streams
    fmt.extend((if ch == 2 { 3u32 } else { 4u32 }).to_le_bytes()); // channel mask
    fmt.extend(s.frames.to_le_bytes()); // samples encoded
    fmt.extend(BLOCK.to_le_bytes());
    fmt.extend(0u32.to_le_bytes()); // play begin
    fmt.extend(s.frames.to_le_bytes()); // play length
    fmt.extend(0u32.to_le_bytes()); // loop begin
    fmt.extend(0u32.to_le_bytes()); // loop length
    fmt.push(0); // loop count
    fmt.push(4); // encoder version
    fmt.extend(((data.len() as u32).div_ceil(BLOCK).max(1) as u16).to_le_bytes()); // block count

    let mut riff = Vec::with_capacity(data.len() + 80);
    riff.extend(b"RIFF");
    riff.extend(((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
    riff.extend(b"WAVEfmt ");
    riff.extend((fmt.len() as u32).to_le_bytes());
    riff.extend(&fmt);
    riff.extend(b"data");
    riff.extend((data.len() as u32).to_le_bytes());
    riff.extend(data);
    riff
}
