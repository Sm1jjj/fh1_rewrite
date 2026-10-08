//! `.fxobj` compiled shaders (Xbox 360 / Xenos): container, microcode, WGSL translation.
//! See `docs/SHADERS.md`.

pub mod container;
pub mod disasm;
pub mod effect;
pub mod ucode;
pub mod wgsl;
pub mod xex;

pub use fh1_formats::Error;
