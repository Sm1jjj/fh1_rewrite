//! inst_lm_survey <track dir> : which `.pvsz` instances carry a per-instance lightmap (`.pvs` record u32 @12 !=
//! 0xFFFFFFFF; docs/LIGHTMAPS.md), split into world-space models and placed templates, and how many distinct
//! lightmaps each world-space model gets (one per model = the zone path can bind it per model).
use std::collections::{BTreeMap, HashMap, HashSet};

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("track dir (media/tracks/colorado)"));
    let ribbon = dir.join("Ribbon_00");
    let be = |b: &[u8], o: usize| u32::from_be_bytes(b[o..o + 4].try_into().unwrap());
    let pvs_bytes = std::fs::read(ribbon.join("Colorado_00.pvs")).unwrap();
    let pvs = fh1_formats::pvs::parse(&pvs_bytes).unwrap();
    let recs = fh1_formats::props::pvs_records(&pvs_bytes).unwrap();
    let fm = std::fs::read(ribbon.join("FilenameMap_00.dat")).unwrap();
    let names: Vec<String> = (0..be(&fm, 0) as usize / 4)
        .map(|i| {
            let o = be(&fm, i * 4) as usize;
            let end = fm[o..].iter().position(|&c| c == 0).map_or(fm.len(), |e| o + e);
            String::from_utf8_lossy(&fm[o..end]).to_ascii_lowercase()
        })
        .collect();
    let lookup = std::fs::read(ribbon.join("PVSZLookup_00.dat")).unwrap();
    let mut ar = fh1_formats::zip::Archive::open(dir.join("bin.zip")).unwrap();
    let by_name: HashMap<String, usize> = ar.entries.iter().enumerate().map(|(i, e)| (e.name.to_ascii_lowercase(), i)).collect();

    // World-space model -> lightmap textures; placed template model -> (placements, with lightmap).
    let mut world: BTreeMap<u16, HashSet<u32>> = BTreeMap::new();
    let mut world_without: HashSet<u16> = HashSet::new();
    let mut placed: BTreeMap<u16, (usize, usize)> = BTreeMap::new();
    let mut placed_lm: HashSet<u32> = HashSet::new();
    let mut seen_records = HashSet::new();
    // Template -> placements whose lightmap has a file on the disc.
    let mut with_file: BTreeMap<u16, usize> = BTreeMap::new();
    for pair in lookup.chunks_exact(8) {
        let name = be(pair, 0) as usize;
        let Some(&i) = names.get(name).and_then(|n| by_name.get(n)) else { continue };
        let e = ar.entries[i].clone();
        let d = ar.read(&e).unwrap();
        let Ok(zone) = fh1_formats::pvsz::parse(&d) else { continue };
        for inst in &zone.instances {
            let Some(r) = recs.get(inst.record as usize) else { continue };
            let m = u16::from_be_bytes([r[0], r[1]]);
            let lm = (be(r, 12) != u32::MAX).then(|| be(r, 8));
            if inst.is_placed() {
                let v = placed.entry(m).or_default();
                v.0 += 1;
                if let Some(t) = lm {
                    v.1 += 1;
                    placed_lm.insert(t);
                    if pvs.textures[t as usize].flags & 1 == 0 {
                        *with_file.entry(m).or_default() += 1;
                    }
                }
            } else if seen_records.insert(inst.record) {
                match lm {
                    Some(t) => {
                        world.entry(m).or_default().insert(t);
                    }
                    None => {
                        world_without.insert(m);
                    }
                }
            }
        }
    }
    let multi = world.values().filter(|s| s.len() > 1).count();
    let mixed = world.keys().filter(|m| world_without.contains(m)).count();
    println!(
        "world-space: {} models with a per-record lightmap ({multi} with more than one, {mixed} also drawn without), {} without",
        world.len(),
        world_without.len()
    );
    let (np, nl): (usize, usize) = placed.values().fold((0, 0), |a, v| (a.0 + v.0, a.1 + v.1));
    let templates_lm = placed.values().filter(|v| v.1 > 0).count();
    println!("placed: {np} instances in all zones, {nl} with a lightmap, {templates_lm} templates, {} distinct lightmaps", placed_lm.len());
    let mut flags: HashMap<u32, usize> = HashMap::new();
    for t in world.values().flatten().chain(&placed_lm) {
        *flags.entry(pvs.textures[*t as usize].flags).or_default() += 1;
    }
    println!("lightmap texture flags: {flags:?}");
    let mut top: Vec<_> = with_file.iter().map(|(m, n)| (*n, *m)).collect();
    top.sort_unstable_by(|a, b| b.cmp(a));
    println!("placements with a file-backed lightmap: {} over {} templates; top: {:?}", top.iter().map(|t| t.0).sum::<usize>(), top.len(), &top[..top.len().min(15)]);
    for (m, s) in world.iter().filter(|(_, s)| s.len() > 1).take(5) {
        println!("  world model {m}: {} lightmaps", s.len());
    }
}
