//! Forza zip archives.
//!
//! Two layouts appear on the FH1 disc:
//! - Ordinary zips (`media/cars/*.zip`): local headers, then data.
//! - Headerless zips (`media/tracks/colorado/bin.zip`): entry data packed back to back from
//!   offset 0 with no local headers; only the central directory at the end describes them.
//!   They hold more than 65535 entries, so the 16-bit counts in the end record wrap and are
//!   ignored. The directory is walked until its signature stops.
//!
//! Methods: 0 = stored, 8 = deflate, 21 = XMemCompress LZX (see [`crate::xcompress`]).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::{xcompress, Error};

const EOCD_SIG: u32 = 0x0605_4b50;
const CDIR_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

pub const METHOD_STORED: u16 = 0;
pub const METHOD_DEFLATE: u16 = 8;
pub const METHOD_XMEM_LZX: u16 = 21;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub method: u16,
    pub crc32: u32,
    pub compressed_size: u32,
    pub size: u32,
    /// Offset of the local header, or of the data itself in headerless archives.
    pub offset: u32,
}

pub struct Archive<R> {
    reader: R,
    pub entries: Vec<Entry>,
}

impl Archive<File> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::new(File::open(path)?)
    }
}

impl<R: Read + Seek> Archive<R> {
    pub fn new(mut reader: R) -> Result<Self, Error> {
        let len = reader.seek(SeekFrom::End(0))?;
        // Forza archives carry no zip comment, so the end record is the last 22 bytes.
        // Fall back to scanning the tail in case one does.
        let tail_len = len.min(22 + 0xFFFF);
        reader.seek(SeekFrom::Start(len - tail_len))?;
        let mut tail = vec![0; tail_len as usize];
        reader.read_exact(&mut tail)?;
        let eocd = (0..=tail.len().saturating_sub(22))
            .rev()
            .find(|&i| u32_le(&tail[i..]) == EOCD_SIG)
            .ok_or(Error::BadMagic("zip end of central directory"))?;
        let e = &tail[eocd..];
        let cd_size = u32_le(&e[12..]) as usize;
        let cd_offset = u32_le(&e[16..]) as u64;

        reader.seek(SeekFrom::Start(cd_offset))?;
        let mut cd = vec![0; cd_size];
        reader.read_exact(&mut cd)?;

        let mut entries = Vec::new();
        let mut p = 0;
        while p + 46 <= cd.len() && u32_le(&cd[p..]) == CDIR_SIG {
            let h = &cd[p..];
            let name_len = u16_le(&h[28..]) as usize;
            let extra_len = u16_le(&h[30..]) as usize;
            let comment_len = u16_le(&h[32..]) as usize;
            let name = cd
                .get(p + 46..p + 46 + name_len)
                .ok_or(Error::Truncated("zip central directory"))?;
            entries.push(Entry {
                name: String::from_utf8_lossy(name).into_owned(),
                method: u16_le(&h[10..]),
                crc32: u32_le(&h[16..]),
                compressed_size: u32_le(&h[20..]),
                size: u32_le(&h[24..]),
                offset: u32_le(&h[42..]),
            });
            p += 46 + name_len + extra_len + comment_len;
        }
        Ok(Self { reader, entries })
    }

    /// Raw (still compressed) bytes of an entry.
    pub fn read_raw(&mut self, entry: &Entry) -> Result<Vec<u8>, Error> {
        self.reader.seek(SeekFrom::Start(entry.offset as u64))?;
        let mut head = [0u8; 30];
        let n = read_up_to(&mut self.reader, &mut head)?;
        let data_start = if n == 30 && u32_le(&head) == LOCAL_SIG {
            entry.offset as u64 + 30 + u16_le(&head[26..]) as u64 + u16_le(&head[28..]) as u64
        } else {
            entry.offset as u64
        };
        self.reader.seek(SeekFrom::Start(data_start))?;
        let mut buf = vec![0; entry.compressed_size as usize];
        self.reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Decompressed bytes of an entry, checked against its CRC32.
    pub fn read(&mut self, entry: &Entry) -> Result<Vec<u8>, Error> {
        let raw = self.read_raw(entry)?;
        let data = match entry.method {
            METHOD_STORED => raw,
            METHOD_DEFLATE => {
                let mut out = Vec::with_capacity(entry.size as usize);
                flate2::read::DeflateDecoder::new(&raw[..]).read_to_end(&mut out)?;
                out
            }
            METHOD_XMEM_LZX => xcompress::decompress(&raw, entry.size as usize)?,
            m => return Err(Error::UnsupportedMethod(m)),
        };
        let crc = crc32fast::hash(&data);
        if crc != entry.crc32 {
            return Err(Error::Crc {
                name: entry.name.clone(),
                expected: entry.crc32,
                got: crc,
            });
        }
        Ok(data)
    }
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
