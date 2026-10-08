//! caff <bin.zip> <out.png> [skip] : decodes every CAFF `_0x*.bin` texture (counts failures) and writes a
//! contact sheet of 16x8 thumbnails (128 px) starting at `skip`.
use std::collections::HashSet;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let skip: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let (cols, rows, cell) = (16usize, 8usize, 128usize);
    let mut sheet = vec![0u8; cols * rows * cell * cell * 4];
    let (mut ok, mut fail, mut cubes, mut placed) = (0, 0, 0, 0usize);
    let mut seen = HashSet::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !(n.starts_with("_0x") && n.ends_with(".bin")) || !seen.insert(n) { continue; }
        let d = ar.read(&e).unwrap();
        if fh1_formats::caff::parse(&d).is_ok_and(|t| t.cube) { cubes += 1; continue; }
        match fh1_formats::caff::decode_base(&d).and_then(|(_, img)| Ok((fh1_formats::xds::to_rgba8(&img)?, img))) {
            Ok((rgba, img)) => {
                ok += 1;
                if ok > skip && placed < cols * rows {
                    let (cx, cy) = (placed % cols * cell, placed / cols * cell);
                    for y in 0..cell {
                        for x in 0..cell {
                            let (sx, sy) = (x * img.width as usize / cell, y * img.height as usize / cell);
                            let s = (sy * img.width as usize + sx) * 4;
                            let t = ((cy + y) * cols * cell + cx + x) * 4;
                            sheet[t..t + 4].copy_from_slice(&rgba[s..s + 4]);
                            sheet[t + 3] = 255;
                        }
                    }
                    placed += 1;
                }
            }
            Err(err) => { fail += 1; println!("{}: {err}", e.name); }
        }
    }
    println!("decoded {ok}, failed {fail}, cube maps {cubes}");
    let f = std::io::BufWriter::new(std::fs::File::create(&a[2]).unwrap());
    let mut enc = png::Encoder::new(f, (cols * cell) as u32, (rows * cell) as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&sheet).unwrap();
}
