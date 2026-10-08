//! audio_install <disc folder> <out dir>: runs the setup tool's `audio` group on its own.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(args.len() == 3, "usage: audio_install <disc folder> <out dir>");
    let t = std::time::Instant::now();
    fh1_audio::install::build(args[1].as_ref(), args[2].as_ref())?;
    println!("done in {:.1} s", t.elapsed().as_secs_f32());
    Ok(())
}
