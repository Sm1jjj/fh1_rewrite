//! Forza Horizon audio.
//!
//! - [`fsb`]: FMOD Ex FSB4 banks (XMA samples) and an XMA2 RIFF wrapper.
//! - [`xma`]: XMA decoding (ffmpeg for now; setup-time only).
//! - [`tuning`]: per-car engine/harmonic/car-model tuning XML → [`tuning::CarAudio`].
//! - [`install`]: disc → converted `audio` group (WAV banks + JSON).
//! - [`synth`]: real-time car sound (engine loops + DSP, tyres, wind, transmission, turbo).
//! - `output` (feature): plays a [`synth::CarSound`] on the default device via cpal.

pub mod fsb;
pub mod install;
pub mod tuning;
pub mod wav;
pub mod xma;
pub mod synth;
pub mod character;
#[cfg(feature = "output")]
pub mod output;
