//! bundle <bin.zip> <track.pvs> [out.png] : parses every `.bundle` (texture preview packs), checks the
//! record chains, matches them to the PVS texture table and reports what the runtime-provided textures
//! (PVS flags bit 0) hold. With `out.png`, writes a contact sheet of the first 256 runtime previews.
use std::collections::{BTreeMap, HashMap, HashSet};
use fh1_formats::{bundle, xds};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let pvs = fh1_formats::pvs::parse(&std::fs::read(&a[2]).unwrap()).unwrap();
    let mut seen = HashSet::new();
    let mut bundles = Vec::new();
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if n.ends_with(".bundle") && seen.insert(n) {
            bundles.push(ar.read(&e).unwrap());
        }
    }
    let mut first: HashMap<u32, (usize, bundle::Entry)> = HashMap::new();
    let (mut records, mut differing) = (0, 0);
    for (bi, d) in bundles.iter().enumerate() {
        for e in bundle::parse(d).unwrap() {
            records += 1;
            let (b0, e0) = first.entry(e.texture_index).or_insert((bi, e));
            if bundles[*b0][e0.offset..e0.offset + e0.len] != d[e.offset..e.offset + e.len] {
                differing += 1;
            }
        }
    }
    println!("{} bundles, {records} records, {} textures, {differing} records differ from the first copy", bundles.len(), first.len());

    // Runtime textures: classify level 0 by its texels.
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut sheet_items = Vec::new();
    let (mut runtime, mut without) = (0, 0);
    for (i, t) in pvs.textures.iter().enumerate() {
        if t.flags & 1 == 0 {
            continue;
        }
        runtime += 1;
        let Some((bi, e)) = first.get(&(i as u32)) else {
            without += 1;
            continue;
        };
        let img = bundle::decode_level(&bundles[*bi], e, 0).unwrap();
        let rgba = xds::to_rgba8(&img).unwrap();
        let px: HashSet<[u8; 4]> = rgba.chunks_exact(4).map(|p| p.try_into().unwrap()).collect();
        let kind = if px.len() == 1 {
            let p = px.iter().next().unwrap();
            format!("constant #{:02x}{:02x}{:02x}{:02x}", p[0], p[1], p[2], p[3])
        } else {
            "varying".into()
        };
        *kinds.entry(format!("{}x{} {kind}", e.width, e.height)).or_default() += 1;
        if sheet_items.len() < 256 {
            sheet_items.push((img.width as usize, img.height as usize, rgba));
        }
    }
    println!("runtime textures {runtime}: {without} without a preview");
    for (k, n) in &kinds {
        println!("  {n:6}  {k}");
    }

    if let Some(path) = a.get(3) {
        let (cols, cell) = (16usize, 32usize);
        let mut sheet = vec![0u8; cols * cols * cell * cell * 4];
        for (k, (w, h, rgba)) in sheet_items.iter().enumerate() {
            let (cx, cy) = (k % cols * cell, k / cols * cell);
            for y in 0..cell {
                for x in 0..cell {
                    let s = ((y * h / cell) * w + x * w / cell) * 4;
                    let d = ((cy + y) * cols * cell + cx + x) * 4;
                    sheet[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
                    sheet[d + 3] = 255;
                }
            }
        }
        let f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut enc = png::Encoder::new(f, (cols * cell) as u32, (cols * cell) as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&sheet).unwrap();
    }
}
