//! One `.pvsz` file through `props::zone_entries`: count and the first entries.
//! `cargo run --release -p fh1-formats --example zone_one -- <file.pvsz>`
fn main() {
    let d = std::fs::read(std::env::args().nth(1).expect("file")).expect("read");
    let e = fh1_formats::props::zone_entries(&d).expect("parse");
    println!("{} entries", e.len());
    for x in e.iter().take(10) {
        println!("{} {:?} {:?} {:?}", x.record, x.distances, x.placement.position, x.activities);
    }
}
