//! strfind <disc_root> <LANG> <id or text substring> — every string-table entry whose id name or
//! text contains the pattern (case-insensitive), as `File:IDS_x = text`.

use fh1_ui::strtable::StringTables;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let t = StringTables::load_language(&a[1], &a[2]).expect("load");
    let pat = a[3].to_lowercase();
    let mut out = Vec::new();
    for (stem, table) in t.files.values() {
        for (h, text) in &table.texts {
            let id = table.names.get(h).map(|n| n.name.as_str()).unwrap_or("?");
            if id.to_lowercase().contains(&pat) || text.to_lowercase() == pat {
                out.push(format!("{stem}:{id} = {text:?}"));
            }
        }
    }
    out.sort();
    out.iter().for_each(|l| println!("{l}"));
}
