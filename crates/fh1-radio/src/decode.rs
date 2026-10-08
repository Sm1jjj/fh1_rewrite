//! Streaming MP3 → interleaved stereo f32 (symphonia, pure Rust).

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub struct Mp3Stream {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
    pub rate: u32,
    /// Frames still to drop after a seek (seeks land on a packet boundary).
    skip: u64,
    scratch: Option<SampleBuffer<f32>>,
    done: bool,
}

impl Mp3Stream {
    /// Opens `path` positioned at frame `start`.
    pub fn open(path: &Path, start: u64) -> Result<Mp3Stream> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("mp3");
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .with_context(|| format!("probe {}", path.display()))?;
        let format = probed.format;
        let track = format.default_track().context("no audio track")?;
        let rate = track.codec_params.sample_rate.unwrap_or(48_000);
        let decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;
        let mut s = Mp3Stream { track: track.id, format, decoder, rate, skip: 0, scratch: None, done: false };
        if start > 0 {
            s.seek(start)?;
        }
        Ok(s)
    }

    pub fn seek(&mut self, frame: u64) -> Result<()> {
        match self.format.seek(SeekMode::Accurate, SeekTo::TimeStamp { ts: frame, track_id: self.track }) {
            Ok(to) => {
                self.decoder.reset();
                self.skip = to.required_ts.saturating_sub(to.actual_ts);
                self.done = false;
            }
            // Past the end.
            Err(_) => self.done = true,
        }
        Ok(())
    }

    /// Appends the next decoded packet to `out` as stereo frames. False at the end.
    pub fn decode_into(&mut self, out: &mut Vec<f32>) -> bool {
        while !self.done {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(_) => {
                    self.done = true;
                    break;
                }
            };
            if packet.track_id() != self.track {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(Error::DecodeError(_)) => continue,
                Err(_) => {
                    self.done = true;
                    break;
                }
            };
            let spec = *decoded.spec();
            let frames = decoded.frames();
            if frames == 0 {
                continue;
            }
            let scratch = match &mut self.scratch {
                Some(b) if b.capacity() >= frames * spec.channels.count() => b,
                _ => self.scratch.insert(SampleBuffer::new(decoded.capacity() as u64, spec)),
            };
            scratch.copy_interleaved_ref(decoded);
            let ch = spec.channels.count().max(1);
            let samples = scratch.samples();
            let drop = (self.skip as usize).min(frames);
            self.skip -= drop as u64;
            for f in drop..frames {
                let l = samples[f * ch];
                let r = if ch > 1 { samples[f * ch + 1] } else { l };
                out.push(l);
                out.push(r);
            }
            if drop < frames {
                return true;
            }
        }
        false
    }
}
