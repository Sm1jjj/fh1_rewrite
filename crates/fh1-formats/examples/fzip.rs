//! fzip list|verify|extract <archive.zip> [out_dir] [name-filter]
//!
//! `verify` decompresses every entry and checks its CRC32 without writing anything.

use std::path::Path;

use fh1_formats::zip::Archive;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: fzip list|verify|extract <archive.zip> [out_dir] [name-filter]");
        std::process::exit(2);
    }
    let mut ar = Archive::open(&args[2]).expect("open archive");
    let entries = ar.entries.clone();
    match args[1].as_str() {
        "list" => {
            for e in &entries {
                println!("{:3} {:10} {:10} {:08x} {}", e.method, e.size, e.compressed_size, e.crc32, e.name);
            }
            println!("{} entries", entries.len());
        }
        "verify" => {
            let (mut ok, mut bad) = (0usize, 0usize);
            for e in &entries {
                match ar.read(e) {
                    Ok(_) => ok += 1,
                    Err(err) => {
                        bad += 1;
                        if bad <= 20 {
                            eprintln!("FAIL {}: {err}", e.name);
                        }
                    }
                }
            }
            println!("{ok} ok, {bad} failed of {}", entries.len());
            if bad > 0 {
                std::process::exit(1);
            }
        }
        "extract" => {
            let out = Path::new(args.get(3).expect("out_dir"));
            let filter = args.get(4).map(|s| s.to_lowercase());
            let mut n = 0;
            for e in &entries {
                if filter.as_ref().is_some_and(|f| !e.name.to_lowercase().contains(f)) {
                    continue;
                }
                let data = ar.read(e).unwrap_or_else(|err| panic!("{}: {err}", e.name));
                let path = out.join(e.name.replace('\\', "/"));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, data).unwrap();
                n += 1;
            }
            println!("extracted {n} entries to {}", out.display());
        }
        other => panic!("unknown command {other}"),
    }
}
