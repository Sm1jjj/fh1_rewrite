//! CJK bitmap fonts: `media/ui/fonts/{CHTB,JPB,KORB}.{abc,sbm}` + `{cht,JP,kor}.ini` ->
//! `fonts/cjk/{cht,jp,kor}.{png,json}` (schema `fh1.ui.cjk_font.v1`).
//!
//! - `.abc` = 5-byte header, then one zlib stream -> 65,535 records x 8 B indexed by UTF-16 codepoint:
//!   `u32 BE` offset into the `.sbm`, then `00 a b c` metric bytes. Empty glyph = `00 00 00 00 00 01 13 01`.
//! - `.sbm` = independent zlib streams; the one at the record's offset inflates to 1152 B = 48x48 pixels,
//!   4 bpp, row-major, LOW nibble first, coverage 0..15 (x17 for 8 bit).
//! - Atlas: non-empty glyphs in ascending codepoint order, 64 columns of 48 px cells, row-major.
//!   Written as LA8 PNG (L = 255, A = coverage).

use std::io::Read;
use std::path::Path;

use super::xmljson::Json;
use crate::{Error, Result};

pub const CELL: usize = 48;
pub const COLS: usize = 64;
const EMPTY: [u8; 8] = [0, 0, 0, 0, 0, 1, 0x13, 1];
const RECORDS: usize = 65535;

/// (output name, source font, ini file name)
pub const FONTS: [(&str, &str, &str); 3] = [("cht", "CHTB", "cht.ini"), ("jp", "JPB", "JP.ini"), ("kor", "KORB", "kor.ini")];

const GLYPH_FIELDS: &str = "cp=UTF-16 codepoint; char=chr(cp) or null for surrogates; cell=index in atlas (row-major, 64 cols); rect=[x,y,w,h] px in atlas; sbm_offset=offset into <base>.sbm; m=[a,b,c] signed int8 abc metric bytes (meaning partly guessed, see WIRING.md)";

pub struct Glyph {
    pub cp: u32,
    pub cell: u32,
    pub sbm_offset: u32,
    /// abc bytes 5..8: `[s8, u8, s8]` exactly as the Python intermediate stored them.
    pub m: [i32; 3],
}

pub struct Decoded {
    /// Ascending codepoint order; `cell` == index.
    pub glyphs: Vec<Glyph>,
    pub rows: usize,
    /// `rows * 48` x `64 * 48` coverage values (0..255).
    pub coverage: Vec<u8>,
}

fn inflate(data: &[u8], limit: u64, what: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|e| Error::Format(format!("cjk: {what}: zlib: {e}")))?;
    Ok(out)
}

/// Inflate one glyph stream into 48x48 8-bit coverage (low nibble first, x17).
pub fn decode_glyph(raw: &[u8]) -> Result<[u8; CELL * CELL]> {
    if raw.len() != CELL * CELL / 2 {
        return Err(Error::Format(format!("cjk: glyph is {} bytes, expected 1152", raw.len())));
    }
    let mut px = [0u8; CELL * CELL];
    for (k, &b) in raw.iter().enumerate() {
        px[2 * k] = (b & 15) * 17;
        px[2 * k + 1] = (b >> 4) * 17;
    }
    Ok(px)
}

/// Decode every glyph of a font from the raw `.abc` file bytes and `.sbm` bytes.
pub fn decode_font(abc_file: &[u8], sbm: &[u8]) -> Result<Decoded> {
    let body = abc_file.get(5..).ok_or_else(|| Error::Format("cjk: abc too short".into()))?;
    let abc = inflate(body, 1 << 24, "abc")?;
    if abc.len() < RECORDS * 8 {
        return Err(Error::Format(format!("cjk: abc has {} bytes, expected {}", abc.len(), RECORDS * 8)));
    }
    let cps: Vec<usize> = (0..RECORDS).filter(|&cp| abc[cp * 8..cp * 8 + 8] != EMPTY).collect();
    let rows = cps.len().div_ceil(COLS);
    let w = COLS * CELL;
    let mut coverage = vec![0u8; rows * CELL * w];
    let mut glyphs = Vec::with_capacity(cps.len());
    for (idx, &cp) in cps.iter().enumerate() {
        let r = &abc[cp * 8..cp * 8 + 8];
        let off = u32::from_be_bytes([r[0], r[1], r[2], r[3]]);
        let start = off as usize;
        if start >= sbm.len() {
            return Err(Error::Format(format!("cjk: glyph U+{cp:04X} offset {off} past sbm end")));
        }
        let end = (start + 8000).min(sbm.len());
        let px = decode_glyph(&inflate(&sbm[start..end], 1153, &format!("glyph U+{cp:04X}"))?)?;
        let (cy, cx) = (idx / COLS, idx % COLS);
        for y in 0..CELL {
            let dst = (cy * CELL + y) * w + cx * CELL;
            coverage[dst..dst + CELL].copy_from_slice(&px[y * CELL..(y + 1) * CELL]);
        }
        glyphs.push(Glyph { cp: cp as u32, cell: idx as u32, sbm_offset: off, m: [r[5] as i8 as i32, r[6] as i32, r[7] as i8 as i32] });
    }
    Ok(Decoded { glyphs, rows, coverage })
}

#[derive(Default)]
struct Ini {
    top: Vec<(String, String)>,
    secs: Vec<(u32, Vec<(String, String)>)>,
}

fn parse_ini(text: &str) -> Ini {
    let mut ini = Ini::default();
    let mut cur: Option<usize> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(n) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')).and_then(|n| n.parse::<u32>().ok()) {
            cur = Some(match ini.secs.iter().position(|(k, _)| *k == n) {
                Some(i) => i,
                None => {
                    ini.secs.push((n, Vec::new()));
                    ini.secs.len() - 1
                }
            });
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let kv = (k.trim().to_string(), v.trim().to_string());
        let list = match cur {
            Some(i) => &mut ini.secs[i].1,
            None => &mut ini.top,
        };
        match list.iter_mut().find(|(k, _)| *k == kv.0) {
            Some(slot) => slot.1 = kv.1,
            None => list.push(kv),
        }
    }
    ini
}

fn get<'a>(list: &'a [(String, String)], key: &str) -> Option<&'a str> {
    list.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn sorted(list: &[(String, String)]) -> Vec<(String, String)> {
    let mut v = list.to_vec();
    v.sort();
    v
}

fn write_png(path: &Path, w: u32, h: u32, coverage: &[u8]) -> Result<()> {
    let mut la = Vec::with_capacity(coverage.len() * 2);
    for &c in coverage {
        la.push(255);
        la.push(c);
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::GrayscaleAlpha);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut wr| wr.write_image_data(&la))
        .map_err(|err| Error::Format(format!("{}: {err}", path.display())))
}

/// Convert one font (`name` = cht/jp/kor) from `raw` (= `media/ui/fonts`) into `out/{name}.png|json`.
/// Returns the glyph count.
pub fn convert_font(raw: &Path, out: &Path, name: &str, base: &str, ini_name: &str) -> Result<usize> {
    let ini = parse_ini(&String::from_utf8_lossy(&std::fs::read(raw.join(ini_name))?));
    let s0 = ini.secs.iter().find(|(k, _)| *k == 0).map(|(_, v)| v.as_slice()).ok_or_else(|| Error::Format(format!("cjk: {ini_name} has no [0]")))?;
    let identical = ini.secs.iter().find(|(k, _)| *k == 1).is_some_and(|(_, s1)| sorted(s1) == sorted(s0));
    let ea = get(s0, "eaabc").ok_or_else(|| Error::Format(format!("cjk: {ini_name}: no eaabc")))?;
    let scale = ea
        .split_once('{')
        .and_then(|(_, r)| r.split_once('}'))
        .and_then(|(n, _)| n.parse::<f64>().ok())
        .ok_or_else(|| Error::Format(format!("cjk: {ini_name}: no {{scale}} in eaabc")))?;
    let num = |key: &str| get(s0, key).ok_or_else(|| Error::Format(format!("cjk: {ini_name}: no {key}")));
    let baselineshift: f64 = num("baselineshift")?.parse().map_err(|_| Error::Format(format!("cjk: {ini_name}: bad baselineshift")))?;
    let offset: i64 = num("offset")?.parse().map_err(|_| Error::Format(format!("cjk: {ini_name}: bad offset")))?;

    let d = decode_font(&std::fs::read(raw.join(format!("{base}.abc")))?, &std::fs::read(raw.join(format!("{base}.sbm")))?)?;
    std::fs::create_dir_all(out)?;
    let (w, h) = (COLS * CELL, d.rows * CELL);
    write_png(&out.join(format!("{name}.png")), w as u32, h as u32, &d.coverage)?;

    let glyphs: Vec<Json> = d
        .glyphs
        .iter()
        .map(|g| {
            let (cy, cx) = ((g.cell as usize) / COLS, (g.cell as usize) % COLS);
            Json::Obj(vec![
                ("cp".into(), Json::int(g.cp as i64)),
                ("char".into(), char::from_u32(g.cp).map_or(Json::Null, |c| Json::Str(c.to_string()))),
                ("cell".into(), Json::int(g.cell as i64)),
                ("rect".into(), Json::Arr([cx * CELL, cy * CELL, CELL, CELL].iter().map(|&v| Json::int(v as i64)).collect())),
                ("sbm_offset".into(), Json::int(g.sbm_offset as i64)),
                ("m".into(), Json::Arr(g.m.iter().map(|&v| Json::int(v as i64)).collect())),
            ])
        })
        .collect();
    let n = glyphs.len();
    let doc = Json::Obj(vec![
        ("schema".into(), Json::str("fh1.ui.cjk_font.v1")),
        ("font".into(), Json::str(name)),
        ("source_font".into(), Json::str(base)),
        ("atlas".into(), Json::Str(format!("fonts/cjk/{name}.png"))),
        ("atlas_format".into(), Json::str("LA8 PNG: L=255 constant, A=coverage 0..255 (4-bit nibble * 17); no gamma")),
        ("atlas_width".into(), Json::int(w as i64)),
        ("atlas_height".into(), Json::int(h as i64)),
        ("cell_px".into(), Json::int(CELL as i64)),
        ("cols".into(), Json::int(COLS as i64)),
        ("rows".into(), Json::int(d.rows as i64)),
        ("glyph_count".into(), Json::int(n as i64)),
        ("scale".into(), Json::float(scale)),
        ("baselineshift".into(), Json::float(baselineshift)),
        ("offset".into(), Json::int(offset)),
        ("precachetexture".into(), get(&ini.top, "precachetexture").map_or(Json::Null, Json::str)),
        ("ini_sections_identical".into(), Json::Bool(identical)),
        ("glyph_fields".into(), Json::str(GLYPH_FIELDS)),
        ("glyphs".into(), Json::Arr(glyphs)),
    ]);
    std::fs::write(out.join(format!("{name}.json")), doc.to_pretty())?;
    Ok(n)
}

/// All three fonts. Returns the total glyph count.
pub fn build(raw: &Path, out: &Path) -> Result<usize> {
    let mut total = 0;
    for (name, base, ini) in FONTS {
        total += convert_font(raw, out, name, base, ini)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn nibbles_low_first() {
        let mut raw = vec![0u8; 1152];
        raw[0] = 0x21; // px0 = 1, px1 = 2
        raw[24] = 0xF0; // row 1, px0 = 0, px1 = 15
        raw[1151] = 0xA9;
        let px = decode_glyph(&raw).unwrap();
        assert_eq!((px[0], px[1]), (17, 34));
        assert_eq!((px[48], px[49]), (0, 255));
        assert_eq!((px[48 * 48 - 2], px[48 * 48 - 1]), (9 * 17, 10 * 17));
        assert!(decode_glyph(&raw[..1151]).is_err());
    }

    #[test]
    fn synthetic_font_roundtrip() {
        // Two glyphs: U+0041 and U+4E00 (cells 0 and 1), second glyph's stream follows the first.
        let g1 = {
            let mut r = vec![0u8; 1152];
            r[0] = 0x21;
            zlib(&r)
        };
        let g2 = {
            let mut r = vec![0u8; 1152];
            r[23] = 0xF1;
            zlib(&r)
        };
        let mut sbm = g1.clone();
        let off2 = sbm.len() as u32;
        sbm.extend_from_slice(&g2);
        let mut abc = Vec::new();
        for _ in 0..RECORDS {
            abc.extend_from_slice(&EMPTY);
        }
        abc[0x41 * 8..0x41 * 8 + 8].copy_from_slice(&[0, 0, 0, 0, 0, 0xFE, 20, 3]);
        abc[0x4E00 * 8..0x4E00 * 8 + 4].copy_from_slice(&off2.to_be_bytes());
        abc[0x4E00 * 8 + 4..0x4E00 * 8 + 8].copy_from_slice(&[0, 2, 44, 0x81]);
        let mut file = vec![1, 1, 0x30, 4, 0x2a];
        file.extend_from_slice(&zlib(&abc));
        let d = decode_font(&file, &sbm).unwrap();
        assert_eq!(d.glyphs.len(), 2);
        assert_eq!(d.rows, 1);
        assert_eq!((d.glyphs[0].cp, d.glyphs[0].cell, d.glyphs[0].m), (0x41, 0, [-2, 20, 3]));
        assert_eq!((d.glyphs[1].cp, d.glyphs[1].sbm_offset, d.glyphs[1].m), (0x4E00, off2, [2, 44, -127]));
        let w = COLS * CELL;
        assert_eq!((d.coverage[0], d.coverage[1]), (17, 34));
        assert_eq!(d.coverage.len(), CELL * w);
        // glyph 2: byte 23 = last pair of row 0 -> x = 46, 47 of cell 1
        assert_eq!((d.coverage[CELL + 46], d.coverage[CELL + 47]), (17, 255));
    }

    #[test]
    fn ini_parse() {
        let ini = parse_ini("precachetexture=d:\\x.tex\n[0]\neaabc=a.abc{0.66}\noffset=0\n[1]\neaabc=a.abc{0.66}\noffset=0\n");
        assert_eq!(get(&ini.top, "precachetexture"), Some("d:\\x.tex"));
        assert_eq!(ini.secs.len(), 2);
        assert_eq!(sorted(&ini.secs[0].1), sorted(&ini.secs[1].1));
    }
}
