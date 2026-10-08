//! regsurvey <dirs>... — for each constant name: registers used per stage (consistency of the
//! global register allocation), and texture fetch dimensions per tf index.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fh1_shaders::container::{RegisterSet, Stage};
use fh1_shaders::ucode::{self, Fetch, Instr};

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
    let mut regs: BTreeMap<(String, String), BTreeMap<(String, u16, u16), usize>> = BTreeMap::new();
    let mut tfdim: BTreeMap<(String, u8), BTreeSet<(u8, String)>> = BTreeMap::new();
    for f in &list {
        let group = f.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
        let d = std::fs::read(f).unwrap();
        let Ok(fx) = fh1_shaders::effect::Effect::parse(&d) else { continue };
        for s in &fx.shaders {
            let st = if s.stage == Stage::Vertex { "vs" } else { "ps" };
            for c in &s.constants {
                *regs.entry((c.name.clone(), group.clone())).or_default().entry((format!("{st}:{:?}", c.set), c.register, c.count)).or_default() += 1;
            }
            for cf in ucode::control_flow(&s.microcode).iter().filter(|c| c.is_exec()) {
                for (_, ins) in ucode::exec_instructions(&s.microcode, cf) {
                    if let Instr::Fetch(Fetch::Texture(t)) = ins {
                        if t.opcode == 1 {
                            let name = s
                                .constants
                                .iter()
                                .find(|c| c.set == RegisterSet::Sampler && t.const_index as u16 >= c.register && (t.const_index as u16) < c.register + c.count)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            tfdim.entry((group.clone(), t.const_index)).or_default().insert((t.dimension, name));
                        }
                    }
                }
            }
        }
    }
    println!("== constants with more than one register assignment (per group)");
    for ((name, group), m) in &regs {
        let floats: BTreeSet<_> = m.keys().filter(|k| !k.0.contains("Sampler")).map(|k| (k.0.clone(), k.1)).collect();
        let stages: BTreeSet<_> = floats.iter().map(|k| k.0.clone()).collect();
        if floats.len() > stages.len() {
            println!("{group:12} {name:36} {m:?}");
        }
    }
    println!("== globals (all)");
    for ((name, group), m) in &regs {
        let total: usize = m.values().sum();
        println!("{group:12} {name:36} {total:5} {:?}", m.keys().collect::<Vec<_>>());
    }
    println!("== tf dims");
    for ((g, tf), s) in &tfdim {
        println!("{g:12} tf{tf:2} {s:?}");
    }
}
