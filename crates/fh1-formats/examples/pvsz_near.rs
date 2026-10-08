//! pvsz_near <bin.zip> <x> <z> <radius> : `.pvsz` instances and `.pgeo` placements within `radius` of game-space (x, z), with
//! their axes and determinant (game space, left-handed; compare with a RenderDoc capture's WorldMatrix c140,
//! docs/GPU_CAPTURE.md).
use fh1_formats::{pvsz, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (x, z, r): (f32, f32, f32) = (a[2].parse().unwrap(), a[3].parse().unwrap(), a[4].parse().unwrap());
    let mut ar = Archive::open(&a[1]).expect("open");
    let mut seen = std::collections::HashSet::new();
    let zones: Vec<_> = ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pvsz") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
    let geos: Vec<_> = ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pgeo")).cloned().collect();
    for e in &geos {
        let Ok(g) = fh1_formats::props::parse_pgeo(&ar.read(e).expect("read")) else { continue };
        for p in &g.placements {
            let q = p.position;
            if (q[0] - x).hypot(q[2] - z) > r {
                continue;
            }
            let m = [p.x_axis, p.y_axis, p.z_axis];
            println!("{} pgeo model {} pos ({:.2} {:.2} {:.2}) axes {:.3?} det {:.3}", e.name, p.model, q[0], q[1], q[2], m, det3(m));
        }
    }
    if let Ok(anim) = fh1_formats::props::track_anim(&mut ar) {
        let (mut pos, mut neg) = (0, 0);
        for i in anim.scenes.iter().flat_map(|s| &s.instances) {
            let p = &i.placement;
            if det3([p.x_axis, p.y_axis, p.z_axis]) < 0.0 { neg += 1 } else { pos += 1 }
        }
        println!("anim instances: det+ {pos} det- {neg}");
    }
    for e in &zones {
        let Ok(zone) = pvsz::parse(&ar.read(e).expect("read")) else { continue };
        for (i, inst) in zone.instances.iter().enumerate() {
            let p = inst.position;
            if (p[0] - x).hypot(p[2] - z) > r || !inst.is_placed() {
                continue;
            }
            let m = inst.axes;
            let det = det3(m);
            let name = inst.block.as_ref().map(|b| b.name.as_str()).unwrap_or("");
            println!("{} #{i} rec {} pos ({:.2} {:.2} {:.2}) axes {:.3?} det {det:.3} {name}", e.name, inst.record, p[0], p[1], p[2], m);
        }
    }
}

fn det3(m: [[f32; 3]; 3]) -> f32 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0]) + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}
