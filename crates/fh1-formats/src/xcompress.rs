//! XMemCompress "native" LZX streams (zip compression method 21 in Forza archives).
//!
//! The stream is a sequence of chunks, each one LZX frame of the same LZXD bitstream:
//! - `0xFF, u16be uncompressed_len, u16be compressed_len, data` for a short frame, or
//! - `u16be compressed_len, data` for a full 32 KiB frame.
//!
//! A zero `compressed_len` (or running out of input) ends the stream.

use lzxd::{Lzxd, WindowSize};

use crate::Error;

const FRAME: usize = 32 * 1024;

/// LZX window used by FH1's archives. XMemCompress defaults to 128 KiB.
pub const DEFAULT_WINDOW: WindowSize = WindowSize::KB128;

/// Decompress an XMemCompress native stream whose total output length is known.
pub fn decompress(input: &[u8], expected_len: usize) -> Result<Vec<u8>, Error> {
    decompress_with_window(input, expected_len, DEFAULT_WINDOW)
}

pub fn decompress_with_window(
    input: &[u8],
    expected_len: usize,
    window: WindowSize,
) -> Result<Vec<u8>, Error> {
    let mut lzx = Lzxd::new(window);
    let mut out = Vec::with_capacity(expected_len);
    let mut pos = 0;

    while out.len() < expected_len {
        let (frame_len, comp_len, header) = match input.get(pos..) {
            Some([0xFF, a, b, c, d, ..]) => (
                u16::from_be_bytes([*a, *b]) as usize,
                u16::from_be_bytes([*c, *d]) as usize,
                5,
            ),
            Some([a, b, ..]) => (FRAME, u16::from_be_bytes([*a, *b]) as usize, 2),
            _ => break,
        };
        if comp_len == 0 {
            break;
        }
        pos += header;
        let chunk = input
            .get(pos..pos + comp_len)
            .ok_or(Error::Truncated("xcompress chunk"))?;
        pos += comp_len;

        let frame_len = frame_len.min(expected_len - out.len());
        let data = lzx
            .decompress_next(chunk, frame_len)
            .map_err(|e| Error::Lzx(format!("{e:?}")))?;
        out.extend_from_slice(data);
    }

    if out.len() != expected_len {
        return Err(Error::SizeMismatch {
            expected: expected_len,
            got: out.len(),
        });
    }
    Ok(out)
}
