//! inst_lightmaps <bin.zip> <Colorado_00.pvs> <out.png> [skip] : the per-instance textures named by the `.pvs`
//! 18-byte draw records (u32 @8 = PVS texture index, u32 @12 = position in the model's texture list,
//! 0xFFFFFFFF = none). Prints counts by texture flags and writes a contact sheet (16x8 cells of 128 px) of the
//! ones that have a file on the disc (CAFF `.bin` or `.bix` + `_B.bix`).
use std::collections::{BTreeSet, HashMap};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pvs_bytes = std::fs::read(&a[2]).unwrap();
    let pvs = fh1_formats::pvs::parse(&pvs_bytes).unwrap();
    let recs = fh1_formats::props::pvs_records(&pvs_bytes).unwrap();
    let skip: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let be = |r: &[u8; 18], o: usize| u32::from_be_bytes(r[o..o + 4].try_into().unwrap());
    let mut inst = BTreeSet::new();
    let mut slots: HashMap<u32, usize> = HashMap::new();
    for r in &recs {
        if be(r, 12) != u32::MAX {
            inst.insert(be(r, 8));
            *slots.entry(be(r, 12)).or_default() += 1;
        }
    }
    let mut flags: HashMap<u32, usize> = HashMap::new();
    for &t in &inst {
        *flags.entry(pvs.textures[t as usize].flags).or_default() += 1;
    }
    println!("{} records, {} per-instance textures, flags {flags:?}, list positions {slots:?}", recs.len(), inst.len());

    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let by_name: HashMap<String, usize> = ar.entries.iter().enumerate().map(|(i, e)| (e.name.to_ascii_lowercase(), i)).collect();
    let mut read = |name: &str| -> Option<Vec<u8>> {
        let i = *by_name.get(&name.to_ascii_lowercase())?;
        let e = ar.entries[i].clone();
        ar.read(&e).ok()
    };
    let (cols, rows, cell) = (16usize, 8usize, 128usize);
    let mut sheet = vec![0u8; cols * rows * cell * cell * 4];
    let mut placed = 0usize;
    let mut n = 0usize;
    for &t in &inst {
        let tex = pvs.textures[t as usize];
        let Some(name) = tex.file_name() else { continue };
        let img = if name.ends_with(".bix") {
            let base = read(&name.replace(".bix", "_B.bix"));
            read(&name).zip(base).and_then(|(h, b)| fh1_formats::bix::decode_base(&h, &b).ok()).map(|(_, i)| i)
        } else {
            read(&name).and_then(|d| fh1_formats::caff::decode_base(&d).ok()).map(|(_, i)| i)
        };
        let Some(img) = img else {
            println!("{name}: no decode");
            continue;
        };
        let Ok(rgba) = fh1_formats::xds::to_rgba8(&img) else { continue };
        n += 1;
        let mean: [f64; 3] = [0, 1, 2].map(|c| rgba.chunks_exact(4).map(|p| p[c] as f64).sum::<f64>() / (rgba.len() / 4) as f64);
        if n <= 40 {
            println!("{t:6} {name} {}x{} mean rgb {:.0} {:.0} {:.0}", img.width, img.height, mean[0], mean[1], mean[2]);
        }
        if n > skip && placed < cols * rows {
            let (cx, cy) = (placed % cols * cell, placed / cols * cell);
            for y in 0..cell {
                for x in 0..cell {
                    let (sx, sy) = (x * img.width as usize / cell, y * img.height as usize / cell);
                    let s = (sy * img.width as usize + sx) * 4;
                    let d = ((cy + y) * cols * cell + cx + x) * 4;
                    sheet[d..d + 3].copy_from_slice(&rgba[s..s + 3]);
                    sheet[d + 3] = 255;
                }
            }
            placed += 1;
        }
    }
    println!("decoded {n} file-backed per-instance textures");
    let f = std::io::BufWriter::new(std::fs::File::create(&a[3]).unwrap());
    let mut enc = png::Encoder::new(f, (cols * cell) as u32, (rows * cell) as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&sheet).unwrap();
}
