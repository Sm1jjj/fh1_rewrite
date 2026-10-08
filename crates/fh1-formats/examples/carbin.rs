//! carbin <car.zip> <entry name> [preview.png] [lod]
//! Dumps sections; with a png path also renders side (top half) and top (bottom half) views.

use fh1_formats::{carbin, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = Archive::open(&a[1]).unwrap();
    let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(&a[2])).cloned().expect("entry");
    let data = ar.read(&e).unwrap();
    let c = carbin::parse(&data).unwrap();
    let want_lod: i32 = a.get(4).map(|s| s.parse().unwrap()).unwrap_or(0);
    println!("type {} sections {}", c.type_id, c.sections.len());
    let mut tris = Vec::new();
    for s in &c.sections {
        let subs: Vec<String> = s.subsections.iter().map(|x| format!("{}:L{}:{}", x.name, x.lod, x.indices.len() / 3)).collect();
        println!("  {:24} off={:?} min={:?} max={:?} lodV={} lod0V={} subs={}", s.name, s.offset.map(|v| (v * 100.0).round() / 100.0), s.bounds_min.map(|v| (v * 100.0).round() / 100.0), s.bounds_max.map(|v| (v * 100.0).round() / 100.0), s.lod_vertices.len(), s.lod0_vertices.len(), subs.join(" "));
        for sub in s.subsections.iter().filter(|x| x.lod == want_lod) {
            let v = s.vertices_for(sub);
            for t in sub.indices.chunks_exact(3) {
                let p = |i: u32| { let q = v[i as usize].position; [q[0] + s.offset[0], q[1] + s.offset[1], q[2] + s.offset[2]] };
                tris.push([p(t[0]), p(t[1]), p(t[2])]);
            }
        }
    }
    println!("{} triangles at LOD {want_lod}", tris.len());
    if let Some(path) = a.get(3) {
        render(&tris, path);
    }
}

fn render(tris: &[[[f32; 3]; 3]], path: &str) {
    let (w, h) = (1024usize, 1024usize);
    let mut img = vec![30u8; w * h * 3];
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for t in tris { for p in t { for k in 0..3 { lo[k] = lo[k].min(p[k]); hi[k] = hi[k].max(p[k]); } } }
    println!("bounds {lo:?} .. {hi:?}");
    // view: (horizontal axis, vertical axis, depth axis, y offset in image, flip depth)
    let views = [(2usize, 1usize, 0usize, 0usize), (2, 0, 1, h / 2)];
    let span = (0..3).map(|k| hi[k] - lo[k]).fold(0.0f32, f32::max);
    let scale = (w as f32 - 40.0) / span;
    for &(ax, ay, az, yoff) in &views {
        let mut zb = vec![f32::MIN; w * h / 2];
        for t in tris {
            let n = { let u = sub(t[1], t[0]); let v = sub(t[2], t[0]); norm(cross(u, v)) };
            let shade = (0.25 + 0.75 * n[az].abs()).min(1.0);
            let pr: Vec<(f32, f32, f32)> = t.iter().map(|p| (20.0 + (p[ax] - lo[ax]) * scale, (h / 2) as f32 - 20.0 - (p[ay] - lo[ay]) * scale, p[az])).collect();
            let minx = pr.iter().map(|p| p.0).fold(f32::MAX, f32::min).max(0.0) as usize;
            let maxx = (pr.iter().map(|p| p.0).fold(f32::MIN, f32::max) as usize).min(w - 1);
            let miny = pr.iter().map(|p| p.1).fold(f32::MAX, f32::min).max(0.0) as usize;
            let maxy = (pr.iter().map(|p| p.1).fold(f32::MIN, f32::max) as usize).min(h / 2 - 1);
            let area = edge(pr[0], pr[1], pr[2].0, pr[2].1);
            if area.abs() < 1e-9 { continue; }
            for y in miny..=maxy { for x in minx..=maxx {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let w0 = edge(pr[1], pr[2], fx, fy) / area; let w1 = edge(pr[2], pr[0], fx, fy) / area; let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 { continue; }
                let z = w0 * pr[0].2 + w1 * pr[1].2 + w2 * pr[2].2;
                let zi = y * w + x;
                if z > zb[zi] { zb[zi] = z; let c = (shade * 230.0) as u8; let o = ((y + yoff) * w + x) * 3; img[o] = c; img[o + 1] = c; img[o + 2] = (c as f32 * 0.9) as u8; }
            }}
        }
    }
    let f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut enc = png::Encoder::new(f, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.write_header().unwrap().write_image_data(&img).unwrap();
}
fn edge(a: (f32, f32, f32), b: (f32, f32, f32), x: f32, y: f32) -> f32 { (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0) }
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] { [a[0] - b[0], a[1] - b[1], a[2] - b[2]] }
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] { [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]] }
fn norm(a: [f32; 3]) -> [f32; 3] { let l = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt().max(1e-12); [a[0] / l, a[1] / l, a[2] / l] }
