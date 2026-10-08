//! Renders the radio offline to a WAV and logs every channel start, snapshot and HUD post:
//! `radio_render <radio dir> <out.wav> [seconds=600] [station 0..3=0] [dpad every N s=0] [lang=EN] [seed=1]`
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(a.len() >= 2, "usage: radio_render <radio dir> <out.wav> [seconds] [station] [dpad_every] [lang] [seed]");
    let dir = std::path::PathBuf::from(&a[0]);
    let secs: f64 = a.get(2).map_or(Ok(600.0), |s| s.parse())?;
    let station: usize = a.get(3).map_or(Ok(0), |s| s.parse())?;
    let dpad_every: f64 = a.get(4).map_or(Ok(0.0), |s| s.parse())?;
    let lang = a.get(5).map_or("EN", |s| s.as_str());
    let seed: u64 = a.get(6).map_or(Ok(1), |s| s.parse())?;
    let data = Arc::new(fh1_radio::install::RadioData::load(&dir)?);
    let rate = 48_000;
    let mut m = fh1_radio::mixer::Mixer::new(data, dir, lang, rate, seed);
    m.system().select_external(station, false, 0);
    m.system().start_free_roam(0.0, false);
    let mut pcm = Vec::with_capacity((secs * rate as f64) as usize * 2);
    // One radio update per block, so the log sees every change.
    let mut block = vec![0.0f32; 1440 * 2];
    let mut ids = [0u64; 4];
    let mut snap = "";
    let mut next_dpad = dpad_every;
    let t = std::time::Instant::now();
    // Wall time rendered (the radio clock stops while the radio is off or paused).
    let mut now = 0.0;
    while now < secs {
        if dpad_every > 0.0 && now >= next_dpad {
            println!("{now:8.2}s  D-pad right");
            m.step(1);
            next_dpad += dpad_every;
        }
        m.render(&mut block);
        pcm.extend(block.iter().map(|s| (s * 32767.0) as i16));
        now += 0.03;
        let clock = now;
        let sys = m.system();
        for (k, (name, p)) in [("music", &sys.music), ("dj", &sys.dj), ("lead", &sys.lead), ("ident", &sys.ident)].into_iter().enumerate() {
            if let Some(p) = p {
                if p.id != ids[k] {
                    ids[k] = p.id;
                    println!("{clock:8.2}s  {name:5} {} from {:.1} s", p.clip, p.pos_ms / 1000.0);
                }
            }
        }
        if sys.snapshot != snap {
            snap = sys.snapshot;
            println!("{clock:8.2}s  snapshot {snap}");
        }
        for h in sys.take_hud_posts() {
            println!("{clock:8.2}s  HUD post: station {} \"{}\" / \"{}\" delay {} logo {}", h.station, h.artist, h.title, h.delay, h.show_station);
        }
    }
    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
    println!("rendered {secs:.0} s in {:.1?} (peak {peak}, rms {rms:.0})", t.elapsed());
    fh1_audio::wav::write(a[1].as_ref(), rate, 2, &pcm)?;
    Ok(())
}
