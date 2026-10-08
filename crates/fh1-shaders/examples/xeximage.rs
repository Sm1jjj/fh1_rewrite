//! xeximage <default.xex> [out.bin] — decrypt/decompress the PE image and count embedded shaders.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let d = std::fs::read(&args[1]).unwrap();
    let x = fh1_shaders::xex::load(&d).unwrap();
    println!("load {:#x}, image {} bytes, starts {:02x?}", x.load_address, x.image.len(), &x.image[..4]);
    let n = x.image.windows(4).filter(|w| *w == [0x10, 0x2A, 0x11, 0x00] || *w == [0x10, 0x2A, 0x11, 0x01]).count();
    println!("{n} shader container magics");
    for (va, bytes) in x.embedded_effects() {
        let (fx, _) = fh1_shaders::effect::Effect::parse_embedded(&bytes).unwrap();
        println!("embedded effect @{va:08x}: {} bytes, {} techniques, {} shaders", bytes.len(), fx.techniques.len(), fx.shaders.len());
        for t in fx.techniques.iter().filter(|t| t.name.starts_with("FinalCombine_BloomTonemap_Vignette") || t.name == "Add_HotExtract") {
            let p = &t.passes[0];
            println!("  {} vs {:?} ps {:?} states {:x?}", t.name, p.vs.map(|i| fx.shaders[i].file_offset), p.ps.map(|i| fx.shaders[i].file_offset), p.render_states);
        }
    }
    if let Some(out) = args.get(2) {
        std::fs::write(out, &x.image).unwrap();
    }
}
