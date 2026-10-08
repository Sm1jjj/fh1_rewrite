//! Parsers for Forza Horizon (Xbox 360) data files.

pub mod bix;
pub mod bundle;
pub mod caff;
pub mod camera;
pub mod carbin;
pub mod crowd;
pub mod fxobj;
pub mod granny;
pub mod grass;
pub mod props;
pub mod path;
pub mod pvs;
pub mod pvsz;
pub mod rmb;
pub mod xcompress;
pub mod xds;
pub mod xpr;
pub mod zip;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad magic: {0}")]
    BadMagic(&'static str),
    #[error("truncated: {0}")]
    Truncated(&'static str),
    #[error("lzx: {0}")]
    Lzx(String),
    #[error("size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: usize, got: usize },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("unsupported zip method {0}")]
    UnsupportedMethod(u16),
    #[error("crc mismatch in {name}: expected {expected:08x}, got {got:08x}")]
    Crc { name: String, expected: u32, got: u32 },
}
