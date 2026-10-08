//! glyphbox <disc_root> <font stem> <chars> — each glyph's advance, offset, scale and the bounding
//! box of its inner mesh in em (as the engine lays it out: offset + |x|·scale).

use std::path::PathBuf;

use fh1_formats::zip::Archive;
use fh1_ui::vfont::Font;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = Archive::open(PathBuf::from(&a[1]).join("media/ui/Fonts.zip")).expect("open Fonts.zip");
    let want = format!("{}_vector_aa.dt", a[2].to_lowercase());
    let e = ar.entries.iter().find(|e| e.name.to_lowercase().ends_with(&want)).cloned().expect("font");
    let f = Font::parse(&ar.read(&e).expect("read")).expect("parse");
    println!("metrics {:?}", f.metrics);
    for c in a[3].chars() {
        let Some(g) = f.glyph(c) else { continue };
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for v in &g.inner.verts {
            let p = [g.offset[0] + v[0].abs() * g.scale, g.offset[1] + v[1] * g.scale];
            for k in 0..2 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        if std::env::var_os("GLYPH_DUMP").is_some() {
            for (name, m) in [("inner", &g.inner), ("outer", &g.outer)] {
                for t in m.indices.chunks(3) {
                    let v: Vec<String> = t.iter().map(|&i| format!("{:?}", m.verts[i as usize])).collect();
                    println!("tri {name} {}", v.join(" "));
                }
            }
        }
        println!("{c:?} advance {:.3} offset {:?} scale {:.3} box {lo:?}..{hi:?}", g.advance, g.offset, g.scale);
    }
}
