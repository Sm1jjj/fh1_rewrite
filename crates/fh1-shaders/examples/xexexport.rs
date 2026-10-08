//! xexexport <section.bin> <base_hex> <out_dir> <addr_hex | addr_hex-addr_hex>... — write
//! `<addr>.asm` (disassembly) and `<addr>.wgsl` (translation, constants named after the
//! constant table) for the shaders embedded in a dumped default.xex section.
//! Output is derived from game code: keep it in a git-ignored directory (re/out/).

use std::fmt::Write;

use fh1_shaders::container::{RegisterSet, ShaderBlob, Stage, MAGIC_PS, MAGIC_VS};
use fh1_shaders::wgsl::{self, Resolver};

struct Named<'a> {
    blob: &'a ShaderBlob,
}

impl Named<'_> {
    fn name(&self, set: RegisterSet, reg: u32) -> Option<String> {
        self.blob.constants.iter().find(|c| c.set == set && reg >= c.register as u32 && reg < (c.register + c.count.max(1)) as u32).map(|c| {
            if c.count > 1 {
                format!("{}_{}", c.name, reg - c.register as u32)
            } else {
                c.name.clone()
            }
        })
    }
}

impl Resolver for Named<'_> {
    fn float_const(&mut self, _: Stage, reg: u32) -> String {
        format!("c[{reg}] /*{}*/", self.name(RegisterSet::Float4, reg).unwrap_or_default())
    }
    fn float_const_rel(&mut self, _: Stage, base: u32, index: &str) -> String {
        format!("c[clamp({base} + {index}, 0, 255)] /*{}[]*/", self.name(RegisterSet::Float4, base).unwrap_or_default())
    }
    fn bool_const(&mut self, st: Stage, addr: u32) -> String {
        let local = if st == Stage::Pixel { addr.wrapping_sub(128) } else { addr };
        format!("(b[{local}] != 0u) /*{}*/", self.name(RegisterSet::Bool, local).unwrap_or_default())
    }
    fn int_const(&mut self, _: Stage, id: u32) -> String {
        format!("i[{id}]")
    }
    fn texture(&mut self, _: Stage, tf: u32, dim: u8) -> Option<(String, String)> {
        let _ = dim;
        Some((format!("tex{tf} /*{}*/", self.name(RegisterSet::Sampler, tf).unwrap_or_default()), format!("samp{tf}")))
    }
    fn vertex_input(&mut self, usage: u8, idx: u8) -> String {
        format!("vin.{}{idx}", fh1_shaders::container::usage::name(usage))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let d = std::fs::read(&args[1]).unwrap();
    let base = u32::from_str_radix(args[2].trim_start_matches("0x"), 16).unwrap();
    let out = std::path::Path::new(&args[3]);
    std::fs::create_dir_all(out).unwrap();
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let ranges: Vec<(u32, u32)> = args[4..]
        .iter()
        .map(|a| match a.split_once('-') {
            Some((x, y)) => (hex(x), hex(y)),
            None => (hex(a), hex(a)),
        })
        .collect();
    let mut o = 0;
    while o + 4 <= d.len() {
        let w = u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
        if w == MAGIC_VS || w == MAGIC_PS {
            if let Ok(b) = ShaderBlob::parse(&d[o..], o) {
                let addr = base + o as u32;
                if ranges.iter().any(|&(x, y)| addr >= x && addr <= y) {
                    let asm = fh1_shaders::disasm::disassemble(&b);
                    std::fs::write(out.join(format!("{addr:08x}.asm")), &asm).unwrap();
                    let mut r = Named { blob: &b };
                    let t = wgsl::translate(&b, if b.stage == Stage::Vertex { "fx_vs" } else { "fx_ps" }, &mut r);
                    let mut s = String::new();
                    let _ = writeln!(s, "// {addr:08x} {:?} — translated from FH1 microcode by fh1-shaders. Bindings are placeholders:", b.stage);
                    let _ = writeln!(s, "// c[] = float constants (stage-local register), b[] = bools, texN/sampN = sampler register N.");
                    for c in &b.constants {
                        let _ = writeln!(s, "//   {:<32} {:?} {} x{}{}", c.name, c.set, c.register, c.count, c.default.as_ref().map(|v| format!(" default {:?}", v.iter().map(|w| f32::from_bits(*w)).collect::<Vec<_>>())).unwrap_or_default());
                    }
                    for e in &b.vertex_elements {
                        let _ = writeln!(s, "//   vertex input {}{} (fetch @{})", fh1_shaders::container::usage::name(e.usage), e.usage_index, e.address);
                    }
                    for i in &b.interpolators {
                        let _ = writeln!(s, "//   interpolator {}{} <-> r/o{}", fh1_shaders::container::usage::name(i.usage), i.usage_index, i.reg);
                    }
                    s += wgsl::PRELUDE;
                    s += &t.code;
                    std::fs::write(out.join(format!("{addr:08x}.wgsl")), s).unwrap();
                    println!("{addr:08x} {:?}", b.stage);
                }
                o += ShaderBlob::container_len(&d[o..]).unwrap().max(4) & !3;
                continue;
            }
        }
        o += 4;
    }
}
