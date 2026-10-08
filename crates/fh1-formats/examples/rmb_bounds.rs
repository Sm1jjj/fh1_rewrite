//! Dumps every `.rmb.bin` in an archive with its directory index and header bounds (first copy of
//! each name): `rmb_bounds <bin.zip> > bounds.csv` (`index,name,minx,miny,minz,maxx,maxy,maxz`).
use std::collections::HashSet;

use fh1_formats::zip::Archive;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = Archive::open(&a[1]).expect("open archive");
    let entries = ar.entries.clone();
    let mut seen = HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        if !e.name.ends_with(".rmb.bin") || !seen.insert(e.name.to_ascii_lowercase()) {
            continue;
        }
        let Ok(d) = ar.read(e) else { continue };
        if d.len() < 36 {
            continue;
        }
        let f = |o: usize| f32::from_be_bytes(d[o..o + 4].try_into().unwrap());
        println!("{i},{},{},{},{},{},{},{}", e.name, f(4), f(8), f(12), f(20), f(24), f(28));
    }
}
