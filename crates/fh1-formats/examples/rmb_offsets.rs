//! Prints each submodel's name, offset and position bounds for the given `.rmb.bin` files.
//!
//! `cargo run --release -p fh1-formats --example rmb_offsets -- <file.rmb.bin>...`

fn main() {
    for f in std::env::args().skip(1) {
        let m = fh1_formats::rmb::parse(&std::fs::read(&f).expect("read")).expect("parse");
        println!("{f}: bounds {:?}..{:?}", m.bounds_min, m.bounds_max);
        for s in &m.submodels {
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for p in &s.positions {
                for k in 0..3 {
                    lo[k] = lo[k].min(p[k]);
                    hi[k] = hi[k].max(p[k]);
                }
            }
            println!("  {} offset {:?} positions {:?}..{:?}", s.name, s.offset, lo, hi);
        }
    }
}
