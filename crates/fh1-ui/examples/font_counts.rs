//! font_counts [disc_root] — parses every `.dt` in `media/ui/Fonts.zip` and prints
//! `file chars verts indices winAscent winDescent em default_char hash_size space_width baseline
//! outer_tris inner_tris` (diffable against the reference reader).

use std::path::PathBuf;

use fh1_formats::zip::Archive;
use fh1_ui::vfont::Font;

fn main() {
    let root = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("disc"));
    let mut ar = Archive::open(root.join("media/ui/Fonts.zip")).expect("open Fonts.zip");
    let mut entries = ar.entries.clone();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries.iter().filter(|e| e.name.to_lowercase().ends_with(".dt")) {
        let f = Font::parse(&ar.read(e).expect("read")).unwrap_or_else(|err| panic!("{}: {err}", e.name));
        let m = &f.metrics;
        let outer: usize = f.glyphs.values().map(|g| g.outer.indices.len() / 3).sum();
        let inner: usize = f.glyphs.values().map(|g| g.inner.indices.len() / 3).sum();
        println!(
            "{} {} {} {} {} {} {} {} {} {:.5} {:.5} {outer} {inner}",
            e.name.rsplit('/').next().unwrap_or(&e.name),
            f.glyphs.len(), f.num_verts, f.num_indices, m.win_ascent, m.win_descent, m.em,
            f.default_char, f.hash_table.len(), m.space_width, m.baseline_offset,
        );
    }
}
