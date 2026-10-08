//! cockpit_survey <car zip> : sections + subsection materials (lod, tri count) of the main, _lod0 and
//! _cockpit carbins, to see which interior geometry each one carries (docs/CAR_INTERIOR.md).
use fh1_formats::{carbin, zip::Archive};
fn main() {
    let p = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let car = p.file_stem().unwrap().to_string_lossy().to_string();
    let mut ar = Archive::open(&p).unwrap();
    for suffix in ["", "_lod0", "_cockpit"] {
        let name = format!("{car}{suffix}.carbin");
        let Some(e) = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(&name)).cloned() else {
            println!("== {name}: missing");
            continue;
        };
        let c = match carbin::parse(&ar.read(&e).unwrap()) {
            Ok(c) => c,
            Err(err) => {
                println!("== {name}: {err:?}");
                continue;
            }
        };
        println!("== {name}: type {} {} sections", c.type_id, c.sections.len());
        for s in &c.sections {
            let subs: Vec<String> = s.subsections.iter().map(|u| format!("{}@{}:{}", u.name, u.lod, u.indices.len())).collect();
            if std::env::var("UV").is_ok_and(|w| s.name.to_lowercase().contains(&w)) {
                for u in &s.subsections {
                    let v = s.vertices_for(u);
                    let used: Vec<&carbin::Vertex> = u.indices.iter().filter_map(|&i| v.get(i as usize)).collect();
                    let range = |f: &dyn Fn(&carbin::Vertex) -> f32| used.iter().map(|x| f(x)).fold((f32::MAX, f32::MIN), |a, b| (a.0.min(b), a.1.max(b)));
                    let t = u.uv_transform;
                    println!(
                        "    {}@{} xf {:?} uv0 x{:?} y{:?} uv1 x{:?} y{:?}",
                        u.name, u.lod, t, range(&|x| x.uv0[0]), range(&|x| x.uv0[1]), range(&|x| x.uv1[0]), range(&|x| x.uv1[1])
                    );
                }
            }
            println!("  {:<22} v{}/{} bb {:?}..{:?}  {}", s.name, s.lod_vertices.len(), s.lod0_vertices.len(), s.bounds_min.map(|v| (v * 100.0).round() / 100.0), s.bounds_max.map(|v| (v * 100.0).round() / 100.0), subs.join(" "));
        }
    }
}
