//! fxcheck <dir-or-file>... — translate every shader in every .fxobj to WGSL and validate it
//! with naga. Writes failing modules to `fxcheck_fail/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fh1_shaders::container::Stage;
use fh1_shaders::wgsl::{self, Resolver};

#[derive(Default)]
struct Test {
    textures: BTreeMap<(u32, u8), ()>,
}

impl Resolver for Test {
    fn float_const(&mut self, stage: Stage, reg: u32) -> String {
        format!("{}[{reg}]", if stage == Stage::Vertex { "fx.vc" } else { "fx.pc" })
    }
    fn float_const_rel(&mut self, stage: Stage, base: u32, index: &str) -> String {
        format!("{}[clamp({base} + {index}, 0, 255)]", if stage == Stage::Vertex { "fx.vc" } else { "fx.pc" })
    }
    fn bool_const(&mut self, _: Stage, addr: u32) -> String {
        format!("(fx.b[{}][{}] != 0u)", addr / 4, addr % 4)
    }
    fn int_const(&mut self, _: Stage, id: u32) -> String {
        format!("fx.i[{id}]")
    }
    fn texture(&mut self, _: Stage, tf: u32, dim: u8) -> Option<(String, String)> {
        self.textures.insert((tf, dim), ());
        Some((format!("t{tf}_{dim}"), "samp".into()))
    }
    fn vertex_input(&mut self, usage: u8, idx: u8) -> String {
        format!("vec4<f32>(f32({usage}), f32({idx}), 0.0, 1.0)")
    }
}

fn files(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        for e in std::fs::read_dir(p).unwrap() {
            files(&e.unwrap().path(), out);
        }
    } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("fxobj") || e.eq_ignore_ascii_case("bin")) {
        out.push(p.to_path_buf());
    }
}

fn raw_shaders(d: &[u8]) -> Vec<fh1_shaders::container::ShaderBlob> {
    use fh1_shaders::container::{ShaderBlob, MAGIC_PS, MAGIC_VS};
    let mut out = Vec::new();
    let mut o = 0;
    while o + 4 <= d.len() {
        let w = u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
        if w == MAGIC_VS || w == MAGIC_PS {
            if let Ok(b) = ShaderBlob::parse(&d[o..], o) {
                out.push(b);
                o += ShaderBlob::container_len(&d[o..]).unwrap().max(4) & !3;
                continue;
            }
        }
        o += 4;
    }
    out
}

fn main() {
    let mut list = Vec::new();
    for a in std::env::args().skip(1) {
        files(Path::new(&a), &mut list);
    }
    let (mut ok, mut bad, mut sm) = (0, 0, 0);
    std::fs::create_dir_all("fxcheck_fail").ok();
    for f in &list {
        let d = std::fs::read(f).unwrap();
        // Raw memory dumps (.bin): every embedded container. Effects: their shader list.
        let shaders = if f.extension().is_some_and(|e| e.eq_ignore_ascii_case("bin")) {
            raw_shaders(&d)
        } else {
            match fh1_shaders::effect::Effect::parse(&d) {
                Ok(fx) => fx.shaders,
                Err(e) => {
                    eprintln!("{}: {e}", f.display());
                    continue;
                }
            }
        };
        for (i, s) in shaders.iter().enumerate() {
            let mut r = Test::default();
            let t = wgsl::translate(s, "fx_main", &mut r);
            if t.state_machine {
                sm += 1;
            }
            let mut m = String::from("diagnostic(off, derivative_uniformity);\n");
            m += wgsl::PRELUDE;
            m += "struct Fx { vc: array<vec4<f32>, 256>, pc: array<vec4<f32>, 256>, b: array<vec4<u32>, 64>, i: array<vec4<i32>, 32> }\n";
            m += "@group(0) @binding(0) var<uniform> fx: Fx;\n@group(0) @binding(1) var samp: sampler;\n";
            for (n, &(tf, dim)) in r.textures.keys().enumerate() {
                let ty = ["texture_2d<f32>", "texture_2d<f32>", "texture_3d<f32>", "texture_cube<f32>"][dim as usize];
                m += &format!("@group(1) @binding({n}) var t{tf}_{dim}: {ty};\n");
            }
            m += &t.code;
            match s.stage {
                Stage::Vertex => m += "@vertex fn main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> { return fx_main(vi).pos; }\n",
                Stage::Pixel => m += "@fragment fn main(@builtin(position) p: vec4<f32>, @builtin(front_facing) ff: bool) -> @location(0) vec4<f32> { var i: FxPsIn; i.vpos = p; i.front_facing = ff; return fx_main(i).c0; }\n",
            }
            let res = naga::front::wgsl::parse_str(&m).map_err(|e| e.emit_to_string(&m)).and_then(|module| {
                naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
                    .validate(&module)
                    .map(|_| ())
                    .map_err(|e| format!("{e:?}"))
            });
            match res {
                Ok(()) => ok += 1,
                Err(e) => {
                    bad += 1;
                    let name = format!("fxcheck_fail/{}_{i}.wgsl", f.file_stem().unwrap().to_string_lossy());
                    std::fs::write(&name, &m).ok();
                    if bad <= 5 {
                        eprintln!("FAIL {} shader {i}: {}", f.display(), e.lines().take(12).collect::<Vec<_>>().join("\n"));
                    }
                }
            }
        }
    }
    println!("{ok} ok, {bad} failed, {sm} via state machine, {} files", list.len());
}
