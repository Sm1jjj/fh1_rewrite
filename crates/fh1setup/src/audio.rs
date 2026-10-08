//! `audio` group: engine/tyre/wind/transmission/turbo banks + per-car tuning (see
//! `fh1_audio::install`). Needs ffmpeg for XMA until there's a native decoder.

use std::path::Path;

use anyhow::Result;

pub fn build(disc: &Path, out: &Path) -> Result<()> {
    fh1_audio::install::build(disc, out)
}
