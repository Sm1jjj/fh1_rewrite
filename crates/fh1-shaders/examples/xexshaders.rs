//! xexshaders <section.bin> <base_address_hex> [index] — list (or disassemble one of) the shader
//! containers embedded in a dumped default.xex section.

use fh1_shaders::container::{ShaderBlob, MAGIC_PS, MAGIC_VS};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let d = std::fs::read(&args[1]).unwrap();
    let base = u32::from_str_radix(args[2].trim_start_matches("0x"), 16).unwrap();
    let only: Option<usize> = args.get(3).and_then(|s| s.parse().ok());
    let mut n = 0;
    let mut o = 0;
    while o + 4 <= d.len() {
        let w = u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
        if w == MAGIC_VS || w == MAGIC_PS {
            if let Ok(b) = ShaderBlob::parse(&d[o..], o) {
                if only.is_none() {
                    let names: Vec<&str> = b.constants.iter().map(|c| c.name.as_str()).collect();
                    println!("{n:3} {:08x} {:?} ins={} {}", base + o as u32, b.stage, b.microcode.len() / 3, names.join(" "));
                } else if only == Some(n) {
                    print!("{}", fh1_shaders::disasm::disassemble(&b));
                }
                n += 1;
                o += ShaderBlob::container_len(&d[o..]).unwrap().max(4) & !3;
                continue;
            }
        }
        o += 4;
    }
    if only.is_none() {
        println!("{n} shaders");
    }
}
