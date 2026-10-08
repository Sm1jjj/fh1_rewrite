//! audio_devices: lists output devices (default first) and plays a 2 s 440 Hz tone on the one
//! the game would use (`FH1_AUDIO_DEVICE` or the default).

use cpal::traits::{DeviceTrait, StreamTrait};

fn main() -> anyhow::Result<()> {
    for (i, n) in fh1_audio::output::device_names().iter().enumerate() {
        println!("{} {n}", if i == 0 { "default:" } else { "        " });
    }
    let d = fh1_audio::output::device()?;
    println!("using: {}", fh1_audio::output::device_name(&d));
    let cfg: cpal::StreamConfig = d.default_output_config()?.into();
    let (rate, ch) = (cfg.sample_rate as f32, cfg.channels as usize);
    let mut t = 0f32;
    let s = d.build_output_stream(
        &cfg,
        move |data: &mut [f32], _| {
            for f in data.chunks_mut(ch) {
                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
                f.iter_mut().for_each(|x| *x = v);
                t += 1.0 / rate;
            }
        },
        |e| eprintln!("stream error: {e}"),
        None,
    )?;
    s.play()?;
    println!("playing 440 Hz for 2 s");
    std::thread::sleep(std::time::Duration::from_secs(2));
    Ok(())
}
