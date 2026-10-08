//! install_ui <disc_root> <out_dir> — runs the `ui` setup group on its own (fh1setup calls the same
//! `fh1_ui::install::build`).

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let t = std::time::Instant::now();
    fh1_ui::install::build(a[1].as_ref(), a[2].as_ref()).expect("ui install");
    println!("done in {:.1}s", t.elapsed().as_secs_f32());
}
