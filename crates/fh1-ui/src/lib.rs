//! Forza Horizon 1 UI data, read from the user's own disc (nothing is shipped):
//!
//! - [`anark`]: Anark Gameface scenes (`media/UI.zip` → `Scenes/ui4/*.bgf/.fbf/.bsg`).
//! - [`strtable`]: `.str` string tables (`media/stringtables/<LANG>.zip`) and gamedb `_&n` refs.
//! - [`vfont`]: `*_vector_aa.dt` vector fonts (`media/ui/Fonts.zip`) and `fontmap.xml`.
//! - [`hash`]: the Anark name hash and the string-table id hash.
//!
//! All parsers are zero-panic: malformed input gives an [`Error`], never a panic.
//! Format notes: `docs/UI.md`. VERIFIED = checked against every file on the EU retail disc.

pub mod anark;
pub mod hash;
pub mod install;
pub mod mappois;
pub mod names;
pub mod nav;
pub mod player;
mod reader;
pub mod strtable;
pub mod vfont;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bad magic: {0}")]
    BadMagic(&'static str),
    #[error("truncated: {what} at 0x{at:x}")]
    Truncated { what: &'static str, at: usize },
    #[error("format: {0}")]
    Format(String),
    #[error("archive: {0}")]
    Archive(#[from] fh1_formats::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// `Err(Error::Format)` unless `cond` holds.
pub(crate) fn need(cond: bool, msg: impl FnOnce() -> String) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(Error::Format(msg()))
    }
}
