//! carbin_regress <disc> : one line per .carbin on the disc (cars, wheels, brakes): parse result,
//! section count and each section's name + bounds. Diff two runs to check a parser change.
use fh1_formats::{carbin, zip::Archive};
fn line(name: &str, d: &[u8]) -> String {
    match carbin::parse(d) {
        Ok(c) => {
            let secs: Vec<String> = c.sections.iter().map(|s| format!("{}[{:.3?}..{:.3?}]v{}/{}", s.name, s.bounds_min, s.bounds_max, s.lod_vertices.len(), s.lod0_vertices.len())).collect();
            format!("{name} ok {} {}", c.sections.len(), secs.join(" "))
        }
        Err(e) => format!("{name} ERR {e}"),
    }
}
fn main() {
    let disc = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    for dir in ["media/cars", "media/wheels"] {
        let mut zips: Vec<_> = std::fs::read_dir(disc.join(dir)).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "zip")).collect();
        zips.sort();
        for z in zips {
            let Ok(mut ar) = Archive::open(&z) else { continue };
            let entries: Vec<_> = ar.entries.iter().filter(|e| e.name.to_lowercase().ends_with(".carbin")).cloned().collect();
            for e in entries {
                let d = ar.read(&e).unwrap();
                println!("{}", line(&format!("{}:{}", z.file_name().unwrap().to_string_lossy(), e.name), &d));
            }
        }
    }
    let mut files: Vec<_> = std::fs::read_dir(disc.join("media/brakes")).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    for f in files {
        if f.to_string_lossy().ends_with(".carbin") {
            println!("{}", line(&f.file_name().unwrap().to_string_lossy(), &std::fs::read(&f).unwrap()));
        }
    }
}
