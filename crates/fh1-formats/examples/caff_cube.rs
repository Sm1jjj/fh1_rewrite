//! caff_cube <bin.zip> <entry name> <out.png> : the six faces of a CAFF cube map side by side.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(&a[2])).unwrap().clone();
    let (t, faces) = fh1_formats::caff::decode_faces(&ar.read(&e).unwrap()).unwrap();
    let (w, h) = (t.width as usize, t.height as usize);
    let mut strip = vec![0u8; w * faces.len() * h * 4];
    for (f, img) in faces.iter().enumerate() {
        let rgba = fh1_formats::xds::to_rgba8(img).unwrap();
        for y in 0..h {
            strip[(y * w * faces.len() + f * w) * 4..][..w * 4].copy_from_slice(&rgba[y * w * 4..][..w * 4]);
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(&a[3]).unwrap());
    let mut enc = png::Encoder::new(file, (w * faces.len()) as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&strip).unwrap();
}
