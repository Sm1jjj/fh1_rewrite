//! Plays the radio live on the default device: `radio_play <radio dir> [seconds=60] [volume=1] [lang=EN]`.
//! Steps to the next station every 20 s and prints what's on.
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(!a.is_empty(), "usage: radio_play <radio dir> [seconds] [volume] [lang]");
    let secs: f32 = a.get(1).map_or(Ok(60.0), |s| s.parse())?;
    let volume: f32 = a.get(2).map_or(Ok(1.0), |s| s.parse())?;
    let lang = a.get(3).map_or("EN", |s| s.as_str());
    let player = fh1_radio::output::RadioPlayer::start(a[0].clone().into(), lang, 42)?;
    player.with(|m| m.volume = volume);
    println!("{} Hz", player.sample_rate);
    let start = std::time::Instant::now();
    let mut last = 0;
    while start.elapsed().as_secs_f32() < secs {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let t = start.elapsed().as_secs_f32();
        if t as u64 / 20 != last {
            last = t as u64 / 20;
            player.with(|m| m.step(1));
        }
        let np = player.now_playing();
        print!("\r{t:5.1}s  {:20} {} - {}            ", np.display, np.artist.unwrap_or_default(), np.title.unwrap_or_default());
        use std::io::Write;
        std::io::stdout().flush()?;
    }
    println!();
    Ok(())
}
