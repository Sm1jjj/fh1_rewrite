//! xds info|png <archive.zip> <out_dir> [name-filter]
//! Prints every .xds header in an archive; `png` also writes the base level as PNG.

use std::path::Path;

use fh1_formats::{xds, zip::Archive};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let png = a[1] == "png";
    let mut ar = Archive::open(&a[2]).unwrap();
    let out = Path::new(&a[3]);
    let filter = a.get(4).map(|s| s.to_lowercase());
    for e in ar.entries.clone() {
        let lname = e.name.to_lowercase();
        if !lname.ends_with(".xds") || filter.as_ref().is_some_and(|f| !lname.contains(f)) {
            continue;
        }
        let data = ar.read(&e).unwrap();
        match xds::Header::parse(&data) {
            Ok(h) => println!(
                "{:44} {:>9} {:?}({}) {}x{} pitch={} tiled={} endian={} dim={} mips={} packed={} fetch={:08x?}",
                e.name, data.len(), h.format, h.raw_format, h.width, h.height, h.pitch, h.tiled as u8,
                h.endian, h.dimension, h.max_mip, h.packed_mips as u8, h.fetch
            ),
            Err(err) => println!("{:44} {err}", e.name),
        }
        if png {
            match xds::decode_base(&data).and_then(|(_, img)| Ok((xds::to_rgba8(&img)?, img))) {
                Ok((rgba, img)) => {
                    let path = out.join(e.name.replace(['/', '\\'], "_") + ".png");
                    std::fs::create_dir_all(out).unwrap();
                    let f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
                    let mut enc = png::Encoder::new(f, img.width, img.height);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    enc.write_header().unwrap().write_image_data(&rgba).unwrap();
                }
                Err(err) => println!("   decode failed: {err}"),
            }
        }
    }
}
