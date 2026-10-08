//! Parses every `.pgeo` in a folder (e.g. `fzip extract bin.zip <dir> .pgeo`) and reports per type
//! and free-roam flag how many parse and how many placements they hold; lists failures.
//!
//! `cargo run --release -p fh1-formats --example pgeo_check -- <dir>`

use std::collections::BTreeMap;

use fh1_formats::props::parse_pgeo;

fn main() {
    let dir = std::env::args().nth(1).expect("dir");
    let mut stats: BTreeMap<(u32, bool, bool), (usize, usize)> = BTreeMap::new();
    for e in std::fs::read_dir(&dir).expect("dir").flatten() {
        let d = std::fs::read(e.path()).unwrap();
        let t = u32::from_be_bytes(d[0x30..0x34].try_into().unwrap());
        let cond = d[0x60..0x64] == [0xFF; 4];
        match parse_pgeo(&d) {
            Ok(g) => {
                let s = stats.entry((t, cond, g.in_free_roam())).or_default();
                s.0 += 1;
                s.1 += g.placements.len();
            }
            Err(err) => println!("FAIL {} type {t} cond {cond}: {err}", e.file_name().to_string_lossy()),
        }
    }
    for ((t, cond, fr), (n, p)) in stats {
        println!("type {t} conditional {cond} free-roam {fr}: {n} files, {p} placements");
    }
}
