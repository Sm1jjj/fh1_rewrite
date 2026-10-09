//! World-audio handle over the shared [`PcmOutput`](crate::pcm::PcmOutput) (site-73 ambience, sfx).
//!
//! Ambience bus cap 48 voices, Sfx bus cap 16; clips are resampled linearly to the device rate and
//! gain / pan are ramped per block. There is no `world_audio_allowed` gate here: the caller gates
//! its own buses. `Bus::Ui` / `Bus::Fmv` passed to [`AmbientMixer::play`] are remapped to
//! `Bus::Sfx` (Ui belongs to the menus, Fmv to the video player).

use std::sync::Arc;

pub use crate::pcm::{Bus, Pcm, VoiceId, VoiceParams};
use crate::pcm::PcmOutput;

#[derive(Clone)]
pub struct AmbientMixer {
    out: &'static PcmOutput,
}

impl AmbientMixer {
    pub(crate) fn new(out: &'static PcmOutput) -> AmbientMixer {
        AmbientMixer { out }
    }

    /// `None` if the bus is at its cap.
    pub fn play(&self, pcm: Arc<Pcm>, mut p: VoiceParams) -> Option<VoiceId> {
        if matches!(p.bus, Bus::Ui | Bus::Fmv) {
            p.bus = Bus::Sfx;
        }
        self.out.play(pcm, p)
    }

    pub fn set(&self, id: VoiceId, gain: f32, pan: f32, pitch: f32) {
        self.out.set(id, gain, pan, pitch);
    }

    pub fn stop(&self, id: VoiceId, fade_s: f32) {
        self.out.stop(id, fade_s);
    }

    pub fn is_playing(&self, id: VoiceId) -> bool {
        self.out.is_playing(id)
    }

    pub fn set_bus_gain(&self, bus: Bus, gain: f32) {
        self.out.set_bus_gain(bus, gain);
    }
}
