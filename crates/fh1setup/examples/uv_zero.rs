//! uv_zero <disc> [car] : body subsections whose UV0 transform has a zero scale (model.rs drew them untextured),
//! with the transform and the raw uv0 range of the vertices they use.
use fh1_formats::{carbin, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let disc = std::path::Path::new(&a[1]);
    let mut zips: Vec<_> = std::fs::read_dir(disc.join("media/cars")).unwrap().map(|e| e.unwrap().path()).collect();
    zips.sort();
    for z in zips {
        let stem = z.file_stem().unwrap().to_string_lossy().into_owned();
        if a.get(2).is_some_and(|c| !c.eq_ignore_ascii_case(&stem)) {
            continue;
        }
        let Ok(mut ar) = Archive::open(&z) else { continue };
        for file in [format!("{stem}.carbin"), format!("{stem}_lod0.carbin")] {
            let Some(e) = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(&file)).cloned() else { continue };
            let Ok(c) = carbin::parse(&ar.read(&e).unwrap()) else { continue };
            for s in &c.sections {
                let min = s.subsections.iter().map(|x| x.lod).min();
                for sub in s.subsections.iter().filter(|x| Some(x.lod) == min || x.lod == 0) {
                    let t = sub.uv_transform;
                    if t[1].abs() > 1e-5 && t[3].abs() > 1e-5 {
                        continue;
                    }
                    let pool = s.vertices_for(sub);
                    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
                    for &i in &sub.indices {
                        let uv = pool[i as usize].uv0;
                        for k in 0..2 {
                            lo[k] = lo[k].min(uv[k]);
                            hi[k] = hi[k].max(uv[k]);
                        }
                    }
                    println!("{file} {} {} L{} tris {} t0 {:.4?} t1 {:.4?} uv0 {:.3?}..{:.3?}", s.name, sub.name, sub.lod, sub.indices.len() / 3, &t[..4], &t[4..], lo, hi);
                }
            }
        }
    }
}
