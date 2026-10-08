//! carbin_check <zip> : parse every `.carbin` in a zip and report failures.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut ar = fh1_formats::zip::Archive::open(&a[1]).unwrap();
    for e in ar.entries.clone() {
        if !e.name.to_ascii_lowercase().ends_with(".carbin") { continue; }
        let d = ar.read(&e).unwrap();
        match fh1_formats::carbin::parse(&d) {
            Ok(c) => println!("ok   {} type {} sections {} lod0 extra {:?}", e.name, c.type_id, c.sections.len(), c.sections.iter().map(|s| s.lod0_raw.extra.len()).sum::<usize>()),
            Err(err) => println!("FAIL {} type {}: {err}", e.name, u32::from_be_bytes(d[..4].try_into().unwrap())),
        }
    }
}
