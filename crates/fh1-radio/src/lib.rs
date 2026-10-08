//! Forza Horizon's in-game radio.
//!
//! - [`config`]: `RadioSystem.xml` -> stations, playlists, DJ pools, gameplay-event lines.
//! - [`install`]: disc -> converted `radio` group (MP3 clips copied out of the FSB banks + JSON).
//! - [`snapshots`]: the radio's mixer channels per mixer snapshot (`AudioMixerSnapshots.xml`).
//! - [`system`]: the game's radio logic (`CRadioSystem` in default.xex) as a state machine.
//! - [`mixer`]: drives [`system`] at the game's update rate, decodes and mixes its channels.
//! - [`mods`]: custom music stations from `mods/radio/stations/<Name>/*.mp3`.
//! - `output` (feature): plays the mixer on the default audio device via cpal.

pub mod config;
pub mod install;
pub mod snapshots;
pub mod system;
pub mod decode;
pub mod mixer;
pub mod mods;
#[cfg(feature = "output")]
pub mod output;
