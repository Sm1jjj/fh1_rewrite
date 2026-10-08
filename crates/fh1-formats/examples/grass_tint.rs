//! grass_tint <install>/assets/private — the grass ground tints and the in-game blade shade they give
//! (PROC_VEGETATION instance normal.w: docs/PROPS.md "Grass").

fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).expect("install assets dir"));
    let blob = std::fs::read(root.join("grass/colorado/grass.bin")).unwrap();
    let index: Vec<(usize, usize)> = {
        let s = std::fs::read_to_string(root.join("grass/colorado/index.json")).unwrap();
        // Minimal scan of "offset"/"len" pairs (no serde_json dependency here).
        let num = |k: &str, from: usize| -> (usize, usize) {
            let p = s[from..].find(k).unwrap() + from + k.len();
            let q = p + s[p..].find(|c: char| c.is_ascii_digit()).unwrap();
            let e = q + s[q..].find(|c: char| !c.is_ascii_digit()).unwrap();
            (s[q..e].parse().unwrap(), e)
        };
        let mut v = Vec::new();
        let mut at = 0;
        while let Some(p) = s[at..].find("\"offset\"") {
            let (o, e) = num("\"offset\"", at + p);
            let (l, e2) = num("\"len\"", e);
            v.push((o, l));
            at = e2;
        }
        v
    };
    let (mut verts, mut grey_v, mut low5_hist) = (0u64, 0u64, [0u64; 32]);
    let mut shade = [0u64; 3];
    for (o, l) in &index {
        let g = fh1_formats::grass::parse(&blob[*o..o + l]).unwrap();
        for v in &g.vertices {
            verts += 1;
            let (r, gg, b) = (v.colour >> 11, (v.colour >> 5) & 63, v.colour & 31);
            if r == b && gg >> 1 == r {
                grey_v += 1;
            }
            low5_hist[b as usize] += 1;
        }
        for sub in 0..3 {
            for bl in g.scatter(sub) {
                shade[(bl.shade() * 2.0).round() as usize] += 1;
            }
        }
    }
    println!("{} objects, {verts} vertices, {grey_v} grey (r = g/2 = b)", index.len());
    println!("low 5 bits (game's shade channel) histogram: {low5_hist:?}");
    println!("blade shade 0 / 0.5 / 1: {shade:?}");
}
