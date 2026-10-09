//! UI one-shots and loops from the installed UI banks (`<audio dir>/banks/<Stem>.json` +
//! `banks/<Stem>/NNN.wav`, the same layout as the car banks), played 2D on [`Bus::Ui`].
//!
//! `UIInGame` (207 small samples) is meant to be preloaded at startup on a worker thread
//! ([`UiSfxPlayer::preload_bank`]); `UIInGame_Streams` (192 long stereo samples, ~356 MB of PCM) and
//! `General` load on demand on first use, `_Streams` banks keeping at most [`MAX_STREAM_CACHE`]
//! clips (oldest dropped first). A clip that fails to load is logged once.
//!
//! The cache is process-wide: every [`UiSfxPlayer`] from `PcmOutput::ui()` shares it.
//!
//! UNVERIFIED: clips play at the WAV rate (the FSB header rate, as recorded); whether FMOD plays
//! these at the header rate (some are 3515 Hz) is plan Q9.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use anyhow::{anyhow, Context, Result};

use crate::install::{bank_stem, BankInfo};
use crate::pcm::{Bus, Pcm, PcmOutput, VoiceId, VoiceParams};

/// Clips kept per `*_Streams` bank cache (all banks together).
const MAX_STREAM_CACHE: usize = 8;

/// One sample of a bank: `bank` is the stem (`UIInGame`), `index` the sample number.
pub struct UiClip {
    pub bank: String,
    pub index: usize,
}

#[derive(Default)]
struct State {
    dir: Option<PathBuf>,
    /// Bank JSON by stem (`None` = missing / unreadable, logged once).
    banks: HashMap<String, Option<Arc<BankInfo>>>,
    cache: HashMap<(String, usize), Arc<Pcm>>,
    /// Cached `_Streams` clips, oldest first.
    order: VecDeque<(String, usize)>,
    failed: HashSet<(String, usize)>,
}

static STATE: OnceLock<Arc<Mutex<State>>> = OnceLock::new();

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub struct UiSfxPlayer {
    out: &'static PcmOutput,
    state: Arc<Mutex<State>>,
}

impl UiSfxPlayer {
    pub(crate) fn new(out: &'static PcmOutput) -> UiSfxPlayer {
        UiSfxPlayer { out, state: STATE.get_or_init(|| Arc::new(Mutex::new(State::default()))).clone() }
    }

    /// `<assets>/audio`; call once before [`Self::play`]. Bank JSON is (re)read lazily.
    pub fn set_audio_dir(&self, dir: &Path) {
        let mut s = lock(&self.state);
        s.dir = Some(dir.to_path_buf());
        s.banks.clear();
        s.failed.clear();
    }

    fn bank_info(&self, bank: &str) -> Result<Arc<BankInfo>> {
        let stem = bank_stem(bank).to_owned();
        let dir = {
            let s = lock(&self.state);
            match s.banks.get(&stem) {
                Some(Some(b)) => return Ok(b.clone()),
                Some(None) => return Err(anyhow!("bank {stem} unavailable")),
                None => {}
            }
            s.dir.clone().ok_or_else(|| anyhow!("ui_sfx: set_audio_dir not called"))?
        };
        let path = dir.join("banks").join(format!("{stem}.json"));
        let r = std::fs::read(&path)
            .with_context(|| path.display().to_string())
            .and_then(|b| serde_json::from_slice::<BankInfo>(&b).with_context(|| path.display().to_string()));
        let mut s = lock(&self.state);
        match r {
            Ok(info) => {
                let info = Arc::new(info);
                s.banks.insert(stem, Some(info.clone()));
                Ok(info)
            }
            Err(e) => {
                eprintln!("fh1-audio ui: {e:#}");
                s.banks.insert(stem, None);
                Err(e)
            }
        }
    }

    /// The decoded clip, from the cache or loaded now (on the caller's thread, outside the lock).
    fn load(&self, bank: &str, index: usize) -> Result<Arc<Pcm>> {
        let stem = bank_stem(bank).to_owned();
        let key = (stem.clone(), index);
        let dir = {
            let s = lock(&self.state);
            if let Some(p) = s.cache.get(&key) {
                return Ok(p.clone());
            }
            if s.failed.contains(&key) {
                return Err(anyhow!("clip {stem}#{index} failed to load earlier"));
            }
            s.dir.clone().ok_or_else(|| anyhow!("ui_sfx: set_audio_dir not called"))?
        };
        let path = dir.join("banks").join(&stem).join(format!("{index:03}.wav"));
        let pcm = match Pcm::load_wav(&path) {
            Ok(p) => p,
            Err(e) => {
                let mut s = lock(&self.state);
                if s.failed.insert(key) {
                    eprintln!("fh1-audio ui: {e:#}");
                }
                return Err(e);
            }
        };
        let mut s = lock(&self.state);
        s.cache.insert(key.clone(), pcm.clone());
        if stem.ends_with("_Streams") {
            s.order.retain(|k| *k != key);
            s.order.push_back(key);
            while s.order.len() > MAX_STREAM_CACHE {
                if let Some(old) = s.order.pop_front() {
                    s.cache.remove(&old);
                }
            }
        }
        Ok(pcm)
    }

    /// Decodes the clip into the cache (for a worker thread).
    pub fn preload(&self, bank: &str, index: usize) -> Result<()> {
        self.load(bank, index).map(|_| ())
    }

    /// Decodes a whole bank into the cache; returns the clips loaded. Clips that fail are skipped
    /// (logged once each).
    pub fn preload_bank(&self, bank: &str) -> Result<usize> {
        let info = self.bank_info(bank)?;
        Ok((0..info.samples.len()).filter(|&i| self.load(bank, i).is_ok()).count())
    }

    /// Plays on [`Bus::Ui`], centred. Loads on demand. `looped` uses the sample's loop points from
    /// the bank JSON when `loop_end > loop_start`, else the whole clip.
    pub fn play(&self, clip: &UiClip, gain: f32, looped: bool) -> Option<VoiceId> {
        self.play_with(clip, gain, 1.0, looped)
    }

    /// [`Self::play`] at a playback-rate multiplier (the FEV event x wave pitch).
    pub fn play_with(&self, clip: &UiClip, gain: f32, pitch: f32, looped: bool) -> Option<VoiceId> {
        let pcm = self.load(&clip.bank, clip.index).ok()?;
        let mut p = VoiceParams { looped, gain, pitch, bus: Bus::Ui, ..Default::default() };
        if looped {
            let s = self.bank_info(&clip.bank).ok().and_then(|b| b.samples.get(clip.index).cloned());
            if let Some(s) = s.filter(|s| s.loop_end > s.loop_start) {
                p.loop_start = s.loop_start as usize;
                // FSB loop end is inclusive; the voice's is exclusive.
                p.loop_end = Some(s.loop_end as usize + 1);
            }
        }
        self.out.play(pcm, p)
    }

    pub fn stop(&self, id: VoiceId, fade_s: f32) {
        self.out.stop(id, fade_s);
    }

    pub fn is_playing(&self, id: VoiceId) -> bool {
        self.out.is_playing(id)
    }

    /// [`Bus::Ui`] gain (settings.ui_volume).
    pub fn set_volume(&self, gain: f32) {
        self.out.set_bus_gain(Bus::Ui, gain);
    }

    /// Sample names of `banks/<stem>.json`, in index order (empty if the bank is missing).
    pub fn sample_names(&self, bank: &str) -> Vec<String> {
        self.bank_info(bank).map(|b| b.samples.iter().map(|s| s.name.clone()).collect()).unwrap_or_default()
    }
}
