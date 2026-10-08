//! ctabclass <file.fxobj> <shader index> — constant table entries with their class/type.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let fx = fh1_shaders::effect::Effect::parse(&std::fs::read(&a[1]).unwrap()).unwrap();
    let s = &fx.shaders[a[2].parse::<usize>().unwrap()];
    for c in &s.constants {
        println!("{:32} {:?} reg {} x{} class {:?} rows {} cols {}", c.name, c.set, c.register, c.count, c.class, c.rows, c.columns);
    }
}
