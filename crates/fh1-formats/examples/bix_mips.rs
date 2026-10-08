//! bix_mips <bin.zip> <track.pvs> : decodes the stored mip chain (levels 1..) of every `.bix` with
//! `bundle::decode_chain`, and compares the level the texture's `.bundle` preview covers with that preview.
use std::collections::{HashMap, HashSet};
use fh1_formats::{bix, bundle, xds};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let pvs = fh1_formats::pvs::parse(&std::fs::read(&a[2]).unwrap()).unwrap();
    let index: HashMap<u32, u32> = pvs.textures.iter().enumerate().map(|(i, t)| (t.file_id, i as u32)).collect();
    let mut seen = HashSet::new();
    let (mut bundles, mut bix_entries) = (Vec::new(), Vec::new());
    for e in ar.entries.clone() {
        let n = e.name.to_ascii_lowercase();
        if !seen.insert(n.clone()) {
            continue;
        }
        if n.ends_with(".bundle") {
            bundles.push(ar.read(&e).unwrap());
        } else if n.ends_with(".bix") && !n.ends_with("_b.bix") {
            bix_entries.push(e);
        }
    }
    let mut preview: HashMap<u32, (usize, bundle::Entry)> = HashMap::new();
    for (bi, d) in bundles.iter().enumerate() {
        for e in bundle::parse(d).unwrap() {
            preview.entry(e.texture_index).or_insert((bi, e));
        }
    }
    let (mut ok, mut fail, mut zero, mut exact, mut close, mut differ, mut nopreview) = (0, 0, 0, 0, 0, 0, 0);
    for e in &bix_entries {
        let d = ar.read(e).unwrap();
        let h = bix::parse_header(&d).unwrap();
        let mips = &d[bix::HEADER_LEN..];
        if mips.iter().all(|&b| b == 0) {
            zero += 1;
        }
        let chain = match bundle::decode_chain(mips, h.format, h.endian, h.width, h.height, 1, h.mip_count) {
            Ok(c) => c,
            Err(err) => {
                fail += 1;
                println!("{}: {err}", e.name);
                continue;
            }
        };
        ok += 1;
        let id = u32::from_str_radix(&e.name[3..11], 16).unwrap();
        let Some((bi, p)) = index.get(&id).and_then(|i| preview.get(i)) else {
            nopreview += 1;
            continue;
        };
        let Some(level) = chain.iter().find(|l| l.width == p.width && l.height == p.height) else {
            println!("{}: {}x{} {} mips, no level matches preview {}x{}", e.name, h.width, h.height, h.mip_count, p.width, p.height);
            differ += 1;
            continue;
        };
        let pv = match bundle::decode_level(&bundles[*bi], p, 0) {
            Ok(pv) => pv,
            Err(err) => {
                println!("{}: preview {}x{} {:08x} {} bytes: {err}", e.name, p.width, p.height, p.raw_format, p.len);
                differ += 1;
                continue;
            }
        };
        if pv.data == level.data {
            exact += 1;
        } else {
            let (x, y) = (xds::to_rgba8(&pv).unwrap(), xds::to_rgba8(level).unwrap());
            let mean = x.iter().zip(&y).map(|(a, b)| a.abs_diff(*b) as u64).sum::<u64>() as f64 / x.len() as f64;
            if mean < 4.0 {
                close += 1
            } else {
                differ += 1;
                println!("{}: {}x{} {:?}, preview {}x{}: mean texel difference {mean:.1}", e.name, h.width, h.height, h.format, p.width, p.height);
            }
        }
    }
    println!("bix {}: chains decoded {ok}, failed {fail}, all-zero chains {zero}", bix_entries.len());
    println!("vs bundle preview: identical blocks {exact}, texels within 4/255 {close}, different {differ}, no preview {nopreview}");
}
