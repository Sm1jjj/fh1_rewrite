//! fxdump <file.fxobj> [shader index] — disassemble the shaders inside an effect.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let d = std::fs::read(&args[1]).expect("read");
    let fx = fh1_shaders::effect::Effect::parse(&d).expect("parse");
    println!("// hash {:08x}, {} techniques, {} shaders", fx.hash, fx.techniques.len(), fx.shaders.len());
    let only: Option<usize> = args.get(2).and_then(|s| s.parse().ok());
    for (i, s) in fx.shaders.iter().enumerate() {
        if only.is_some_and(|o| o != i) {
            continue;
        }
        println!("\n// ===== shader {i}");
        print!("{}", fh1_shaders::disasm::disassemble(s));
    }
}
