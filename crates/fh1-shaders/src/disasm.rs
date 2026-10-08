//! Human-readable microcode listing (loosely in the style of the XNA disassembler).

use std::fmt::Write;

use crate::container::{usage, ShaderBlob, Stage};
use crate::ucode::{self, alu_swizzle, vfmt, CfOp, Fetch, Instr};

const XYZW: [char; 4] = ['x', 'y', 'z', 'w'];

fn mask(m: u8) -> String {
    (0..4).map(|i| if m & (1 << i) != 0 { XYZW[i] } else { '_' }).collect()
}

fn alu_src(a: &ucode::Alu, i: usize, comps: usize, stage: Stage) -> String {
    let reg = a.src_reg[i];
    let mut s = String::new();
    if a.src_neg[i] {
        s.push('-');
    }
    if a.src_is_temp[i] {
        let abs = reg & 0x80 != 0;
        let _ = write!(s, "r{}{}", reg & 0x3F, if reg & 0x40 != 0 { "[aL]" } else { "" });
        if abs {
            s = format!("|{s}|");
        }
    } else {
        let base = if stage == Stage::Pixel { "c" } else { "c" };
        let rel = if a.const_is_addressed(i) {
            if a.const_a0_relative {
                "[a0]"
            } else {
                "[aL]"
            }
        } else {
            ""
        };
        let _ = write!(s, "{base}{reg}{rel}");
        if a.abs_constants {
            s = format!("|{s}|");
        }
    }
    let sw = a.src_swiz[i];
    let comp: String = match comps {
        1 => XYZW[alu_swizzle(sw, 3) as usize].to_string(),
        2 => format!("{}{}", XYZW[alu_swizzle(sw, 3) as usize], XYZW[alu_swizzle(sw, 0) as usize]),
        _ => (0..4).map(|k| XYZW[alu_swizzle(sw, k) as usize]).collect(),
    };
    if comp != "xyzw" {
        let _ = write!(s, ".{comp}");
    }
    s
}

fn fetch_dst_swizzle(sw: u16) -> String {
    (0..4)
        .map(|i| match (sw >> (3 * i)) & 7 {
            0 => 'x',
            1 => 'y',
            2 => 'z',
            3 => 'w',
            4 => '0',
            5 => '1',
            7 => '_',
            _ => '?',
        })
        .collect()
}

pub fn disassemble(blob: &ShaderBlob) -> String {
    let mut o = String::new();
    let st = blob.stage;
    let _ = writeln!(o, "// {} ({:?}) @0x{:X}", blob.target, st, blob.file_offset);
    for c in &blob.constants {
        let _ = writeln!(o, "//   {:<34} {:?} {} x{}", c.name, c.set, c.register, c.count);
    }
    for (r, v) in &blob.float_defs {
        let _ = writeln!(o, "    def c{r}, {}, {}, {}, {}", v[0], v[1], v[2], v[3]);
    }
    for (r, v) in &blob.int_defs {
        let _ = writeln!(o, "    defi i{r}, {}, {}, {}, {}", v[0], v[1], v[2], v[3]);
    }
    for e in &blob.vertex_elements {
        let _ = writeln!(o, "    dcl_{}{} @{}", usage::name(e.usage), e.usage_index, e.address);
    }
    for i in &blob.interpolators {
        let _ = writeln!(o, "    {} {}{} r{}", if st == Stage::Pixel { "dcl_in" } else { "dcl_out" }, usage::name(i.usage), i.usage_index, i.reg);
    }
    if let Some(r) = blob.vpos_register {
        let _ = writeln!(o, "    dcl_vpos r{r}");
    }
    let code = &blob.microcode;
    for (n, cf) in ucode::control_flow(code).iter().enumerate() {
        let _ = write!(o, "{n:3}: ");
        match cf.op {
            CfOp::Nop => {
                let _ = writeln!(o, "nop");
                continue;
            }
            CfOp::Exec { end } => {
                let _ = writeln!(o, "exec{} @{} x{}", if end { "e" } else { "" }, cf.address, cf.count);
            }
            CfOp::CondExec { end, bool_addr, condition } => {
                let _ =
                    writeln!(o, "cexec{} {}b{} @{} x{}", if end { "e" } else { "" }, if condition { "" } else { "!" }, bool_addr, cf.address, cf.count);
            }
            CfOp::CondExecPred { end, condition } => {
                let _ = writeln!(o, "({}p0) exec{} @{} x{}", if condition { "" } else { "!" }, if end { "e" } else { "" }, cf.address, cf.count);
            }
            CfOp::LoopStart { loop_id, skip_to, repeat } => {
                let _ = writeln!(o, "loop{} i{loop_id}, skip {skip_to}", if repeat { "_repeat" } else { "" });
            }
            CfOp::LoopEnd { loop_id, body, predicated_break, condition } => {
                let _ = writeln!(o, "endloop i{loop_id}, body {body}{}", if predicated_break { format!(" break_if {}p0", if condition { "" } else { "!" }) } else { String::new() });
            }
            CfOp::CondCall { target, unconditional, predicated, bool_addr, condition } => {
                let c = if unconditional { String::new() } else if predicated { format!(" {}p0", if condition { "" } else { "!" }) } else { format!(" {}b{bool_addr}", if condition { "" } else { "!" }) };
                let _ = writeln!(o, "call {target}{c}");
            }
            CfOp::Return => {
                let _ = writeln!(o, "ret");
            }
            CfOp::CondJmp { target, unconditional, predicated, bool_addr, condition } => {
                let c = if unconditional { String::new() } else if predicated { format!(" {}p0", if condition { "" } else { "!" }) } else { format!(" {}b{bool_addr}", if condition { "" } else { "!" }) };
                let _ = writeln!(o, "jmp {target}{c}");
            }
            CfOp::Alloc { kind, size } => {
                let _ = writeln!(o, "alloc {} {size}", ["none", "position", "interpolators/colors", "memory"][kind as usize]);
            }
            CfOp::MarkVsFetchDone => {
                let _ = writeln!(o, "mark_vs_fetch_done");
            }
        }
        if !cf.is_exec() {
            continue;
        }
        for (addr, ins) in ucode::exec_instructions(code, cf) {
            let _ = write!(o, "     {addr:3}  ");
            match ins {
                Instr::Fetch(Fetch::Vertex(v)) => {
                    let _ = writeln!(
                        o,
                        "{}vfetch{} r{}.{}, r{}.{}, vf{} fmt={} off={} stride={}{}{}",
                        pred(v.predicated, v.pred_condition),
                        if v.mini { "_mini" } else { "_full" },
                        v.dst_reg,
                        fetch_dst_swizzle(v.dst_swiz),
                        v.src_reg,
                        XYZW[v.src_swiz as usize],
                        v.const_index,
                        vfmt::name(v.format),
                        v.offset,
                        v.stride,
                        if v.signed { " signed" } else { "" },
                        if v.normalized { "" } else { " int" },
                    );
                }
                Instr::Fetch(Fetch::Texture(t)) => {
                    let name = match t.opcode {
                        1 => ["tfetch1D", "tfetch2D", "tfetch3D", "tfetchCube"][t.dimension as usize],
                        16 => "getBCF",
                        17 => "getCompTexLOD",
                        18 => "getGradients",
                        19 => "getWeights",
                        24 => "setTexLOD",
                        25 => "setGradientH",
                        26 => "setGradientV",
                        _ => "tfetch?",
                    };
                    let src: String = (0..3).map(|i| XYZW[((t.src_swiz >> (2 * i)) & 3) as usize]).collect();
                    let _ = writeln!(
                        o,
                        "{}{name} r{}.{}, r{}.{src}, tf{}{}{}{}",
                        pred(t.predicated, t.pred_condition),
                        t.dst_reg,
                        fetch_dst_swizzle(t.dst_swiz),
                        t.src_reg,
                        t.const_index,
                        if t.lod_bias != 0.0 { format!(" LODBias={}", t.lod_bias) } else { String::new() },
                        if t.use_register_lod { " UseRegisterLOD" } else { "" },
                        if t.offset != [0.0; 3] { format!(" Offset={:?}", t.offset) } else { String::new() },
                    );
                }
                Instr::Alu(a) => {
                    let p = pred(a.predicated, a.pred_condition);
                    let dst = |r: u8, rel: bool| {
                        if a.export {
                            match st {
                                Stage::Vertex => match r {
                                    62 => "oPos".to_string(),
                                    63 => "oPts".to_string(),
                                    0..=15 => format!("o{r}"),
                                    _ => format!("export{r}"),
                                },
                                Stage::Pixel => match r {
                                    0..=3 => format!("oC{r}"),
                                    61 => "oDepth".to_string(),
                                    _ => format!("export{r}"),
                                },
                            }
                        } else {
                            format!("r{r}{}", if rel { "[aL]" } else { "" })
                        }
                    };
                    let vm = a.vector_result_mask() | a.const0_mask() | a.const1_mask();
                    let vop = a.vector_op as usize;
                    let nargs = ucode::VECTOR_OP_ARGS[vop] as usize;
                    let args: Vec<String> = (0..nargs).map(|i| alu_src(&a, i, 4, st)).collect();
                    let _ = writeln!(
                        o,
                        "{p}{}{} {}.{}, {}",
                        ucode::VECTOR_OP_NAMES[vop],
                        if a.vector_clamp { "_sat" } else { "" },
                        dst(a.vector_dest, a.vector_dest_rel),
                        mask(vm),
                        args.join(", ")
                    );
                    let sop = a.scalar_op;
                    let sargs = match ucode::scalar_op_shape(sop) {
                        0 => String::new(),
                        1 => alu_src(&a, 2, 1, st),
                        2 => alu_src(&a, 2, 2, st),
                        _ => format!(
                            "{}c{}.{}, r{}.{}",
                            if a.src_neg[2] { "-" } else { "" },
                            a.src_reg[2],
                            XYZW[alu_swizzle(a.src_swiz[2], 3) as usize],
                            a.scalar_const_temp_reg(),
                            XYZW[alu_swizzle(a.src_swiz[2], 0) as usize]
                        ),
                    };
                    if !(sop == 50 && a.scalar_result_mask() == 0) {
                        let sd = if a.export { dst(a.vector_dest, false) } else { dst(a.scalar_dest, a.scalar_dest_rel) };
                        let _ = writeln!(
                            o,
                            "          + {}{} {}.{}, {}",
                            ucode::SCALAR_OP_NAMES[sop as usize],
                            if a.scalar_clamp { "_sat" } else { "" },
                            sd,
                            mask(a.scalar_result_mask()),
                            sargs
                        );
                    }
                }
            }
        }
    }
    o
}

fn pred(p: bool, c: bool) -> &'static str {
    match (p, c) {
        (false, _) => "",
        (true, true) => "(p0) ",
        (true, false) => "(!p0) ",
    }
}
