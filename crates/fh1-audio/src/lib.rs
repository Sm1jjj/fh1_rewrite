//! Forza Horizon audio.
//!
//! - [`fsb`]: FMOD Ex FSB4 banks (XMA samples) and an XMA2 RIFF wrapper.
//! - [`xma`]: XMA decoding (ffmpeg for now; setup-time only).
//! - [`tuning`]: per-car engine/harmonic/car-model tuning XML → [`tuning::CarAudio`].
//! - [`install`]: disc → converted `audio` group (WAV banks + JSON).
//! - [`synth`]: real-time car sound (engine loops + DSP, tyres, wind, transmission, turbo).
//! - `output` (feature): plays a [`synth::CarSound`] on the default device via cpal.
//! - [`ui_events`]: `ui4audio.xml` event table (setup) and play key → UI sample (runtime).
//! - [`soundscape`]: Colorado soundscape tiles → JSON (setup), soundbank CRC.
//! - `pcm` / `ambient` / `ui_sfx` (feature `output`): one shared stream for UI clicks, FMV audio,
//!   world ambience and stingers, beside the car `output` player.

pub mod fsb;
pub mod install;
pub mod tuning;
pub mod wav;
pub mod xma;
pub mod synth;
pub mod character;
pub mod soundscape;
pub mod ui_events;
pub mod fev;
pub mod dialogue;
#[cfg(feature = "output")]
pub mod output;
#[cfg(feature = "output")]
pub mod pcm;
#[cfg(feature = "output")]
pub mod ambient;
#[cfg(feature = "output")]
pub mod ui_sfx;
