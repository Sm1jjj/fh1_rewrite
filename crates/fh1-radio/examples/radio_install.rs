//! Converts the disc's radio into a folder: `radio_install <disc root> <out dir>`.
//! (`fh1setup` does the same as its `radio` group.)
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 2, "usage: radio_install <disc root> <out dir>");
    let t = std::time::Instant::now();
    fh1_radio::install::build(args[0].as_ref(), args[1].as_ref())?;
    println!("done in {:.1?}", t.elapsed());
    Ok(())
}
