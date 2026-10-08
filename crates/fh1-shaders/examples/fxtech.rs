//! fxtech <dir-or-file>... — techniques, passes and render states of every effect, plus a
//! histogram of render state values.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn files(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        for e in std::fs::read_dir(p).unwrap() {
            files(&e.unwrap().path(), out);
        }
    } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("fxobj")) {
        out.push(p.to_path_buf());
    }
}

fn main() {
    let mut list = Vec::new();
    for a in std::env::args().skip(1) {
        files(Path::new(&a), &mut list);
    }
    let mut hist: BTreeMap<(String, u32), BTreeMap<u32, usize>> = BTreeMap::new();
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let mut fails = 0;
    for f in &list {
        let d = std::fs::read(f).unwrap();
        match fh1_shaders::effect::Effect::parse(&d) {
            Ok(fx) => {
                println!("{}", f.file_name().unwrap().to_string_lossy());
                for t in &fx.techniques {
                    *names.entry(t.name.clone()).or_default() += 1;
                    for p in &t.passes {
                        let rs: Vec<String> = p.render_states.iter().map(|(k, v)| format!("{k:#x}={v:#x}")).collect();
                        println!("  {:20} {:10} vs={:?} ps={:?} {}", t.name, p.name, p.vs, p.ps, rs.join(" "));
                        for (k, v) in &p.render_states {
                            *hist.entry((t.name.clone(), *k)).or_default().entry(*v).or_default() += 1;
                        }
                    }
                }
            }
            Err(e) => {
                fails += 1;
                println!("FAIL {}: {e}", f.display());
            }
        }
    }
    println!("\n== techniques: {names:?}\n== {fails} failures");
    let mut seen = BTreeSet::new();
    for ((t, k), vals) in &hist {
        seen.insert(*k);
        println!("{t:20} {k:#06x}: {vals:?}");
    }
}
