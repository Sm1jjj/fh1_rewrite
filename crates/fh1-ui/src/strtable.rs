//! `.str` string tables (magic `LSB2`, big-endian), one zip per language in `media/stringtables/`
//! (113 files each; EN GB DE FR ES MX IT nl br cz PL HU RU DA FI NB SV JP KO CHT + pseudo LOC, DEV).
//! VERIFIED on all 2,481 files:
//!
//! - header: `"LSB2"`, 8 bytes `01 00 00 00 02 00 00 00`, `u32 0x24`, `u32 nsec (4)`, `u32 off[nsec]` (0 = absent).
//! - each section: `u32 size (incl. 8-byte header), u32 n`, then
//!   - sec0 texts: `(n+1) × {u16 hash, u32 char_offset}` sorted by hash, last = sentinel 0xFFFF;
//!     then UTF-16BE NUL-terminated strings (offsets in UTF-16 units from the blob start).
//!   - sec1 id names: `n × {u16 hash, u16 src_index, u32 byte_offset}` + ASCII NUL-terminated names.
//!   - sec2 translator notes: `n × {u8 0, u16 hash, u8 1, u32 byte_offset}` + ASCII notes.
//!   - sec3 authoring order: `n × u16 hash`.
//! - id hash = [`crate::hash::str_hash`] (matches every id name). `File:IDS_x` refs pick the file by stem
//!   (case-insensitive); gamedb `_&n` = `H(file) << 16 | H(id)` (all 4,878 refs resolve).
//!
//! Strings carry inline markup (`{0}`..`{5}` args, `{font='B'}`, `{colour='R G B A'}`, `{font_pop}`);
//! they are stored raw, see [`strip_markup`].

use std::collections::HashMap;
use std::path::Path;

use crate::hash::str_hash;
use crate::reader::{be_u16, be_u32, latin1};
use crate::{need, Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdName {
    pub name: String,
    pub src_index: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrTable {
    /// Header bytes 4..12 (always `01 00 00 00 02 00 00 00`).
    pub flags: [u8; 8],
    /// Text by id hash.
    pub texts: HashMap<u16, String>,
    /// Id name by hash (shipped on retail).
    pub names: HashMap<u16, IdName>,
    /// Translator notes by hash (as stored, may be empty).
    pub notes: HashMap<u16, String>,
    /// Hashes in authoring order.
    pub order: Vec<u16>,
}

impl StrTable {
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.get(..4) != Some(b"LSB2".as_slice()) {
            return Err(Error::BadMagic("LSB2"));
        }
        let trunc = |what| Error::Truncated { what, at: 0 };
        let mut flags = [0; 8];
        flags.copy_from_slice(d.get(4..12).ok_or(trunc("str header"))?);
        let nsec = be_u32(d, 0x10).ok_or(trunc("str header"))? as usize;
        need(nsec <= 16, || format!("str nsec {nsec}"))?;
        let secs = (0..nsec)
            .map(|i| be_u32(d, 0x14 + 4 * i).map(|o| o as usize).ok_or(trunc("str section table")))
            .collect::<Result<Vec<_>>>()?;
        let sec = |i: usize| secs.get(i).copied().filter(|&o| o != 0);
        let mut t = Self { flags, ..Self::default() };

        let o = sec(0).ok_or(Error::Format("str has no text section".into()))?;
        let n = be_u32(d, o + 4).ok_or(trunc("str sec0"))? as usize;
        let blob = o + 8 + 6 * (n + 1);
        for i in 0..n {
            let e = o + 8 + 6 * i;
            let h = be_u16(d, e).ok_or(trunc("str sec0 entry"))?;
            let off = be_u32(d, e + 2).ok_or(trunc("str sec0 entry"))? as usize;
            t.texts.insert(h, utf16z(d, blob + 2 * off)?);
        }
        if let Some(o) = sec(1) {
            let n = be_u32(d, o + 4).ok_or(trunc("str sec1"))? as usize;
            let blob = o + 8 + 8 * n;
            for i in 0..n {
                let e = o + 8 + 8 * i;
                let h = be_u16(d, e).ok_or(trunc("str sec1 entry"))?;
                let src_index = be_u16(d, e + 2).ok_or(trunc("str sec1 entry"))?;
                let off = be_u32(d, e + 4).ok_or(trunc("str sec1 entry"))? as usize;
                t.names.insert(h, IdName { name: cstr(d, blob + off)?, src_index });
            }
        }
        if let Some(o) = sec(2) {
            let n = be_u32(d, o + 4).ok_or(trunc("str sec2"))? as usize;
            let blob = o + 8 + 8 * n;
            for i in 0..n {
                let e = o + 8 + 8 * i;
                let h = be_u16(d, e + 1).ok_or(trunc("str sec2 entry"))?;
                let off = be_u32(d, e + 4).ok_or(trunc("str sec2 entry"))? as usize;
                t.notes.insert(h, cstr(d, blob + off)?);
            }
        }
        if let Some(o) = sec(3) {
            let n = be_u32(d, o + 4).ok_or(trunc("str sec3"))? as usize;
            t.order = (0..n)
                .map(|i| be_u16(d, o + 8 + 2 * i).ok_or(trunc("str sec3 entry")))
                .collect::<Result<_>>()?;
        }
        Ok(t)
    }

    /// Text of id `IDS_x`.
    pub fn get(&self, id: &str) -> Option<&str> {
        self.texts.get(&str_hash(id)).map(String::as_str)
    }
}

fn utf16z(d: &[u8], start: usize) -> Result<String> {
    let mut units = Vec::new();
    let mut o = start;
    loop {
        let u = be_u16(d, o).ok_or(Error::Truncated { what: "str text", at: o })?;
        if u == 0 {
            break;
        }
        units.push(u);
        o += 2;
    }
    String::from_utf16(&units).map_err(|_| Error::Format(format!("str: bad UTF-16 at 0x{start:x}")))
}

fn cstr(d: &[u8], start: usize) -> Result<String> {
    let rest = d.get(start..).ok_or(Error::Truncated { what: "str name", at: start })?;
    let end = rest.iter().position(|&c| c == 0).ok_or(Error::Truncated { what: "str name", at: start })?;
    Ok(latin1(&rest[..end]))
}

/// All string tables of one language.
#[derive(Debug, Clone, Default)]
pub struct StringTables {
    /// Lower-case stem → (stem as stored, table).
    pub files: HashMap<String, (String, StrTable)>,
    /// `H(stem)` → lower-case stem (no collisions among the 113 stems, VERIFIED).
    by_hash: HashMap<u16, String>,
}

impl StringTables {
    /// Loads every `.str` from a language zip (`media/stringtables/EN.zip`).
    pub fn load_zip(path: impl AsRef<Path>) -> Result<Self> {
        let mut ar = fh1_formats::zip::Archive::open(path)?;
        let mut out = Self::default();
        for e in ar.entries.clone() {
            let base = e.name.rsplit(['/', '\\']).next().unwrap_or(&e.name);
            let Some(stem) = base.strip_suffix(".str").or_else(|| base.strip_suffix(".STR")) else {
                continue;
            };
            let table = StrTable::parse(&ar.read(&e)?)?;
            out.insert(stem, table);
        }
        Ok(out)
    }

    /// `media/stringtables/<lang>.zip` under the extracted disc root.
    pub fn load_language(disc: impl AsRef<Path>, lang: &str) -> Result<Self> {
        Self::load_zip(disc.as_ref().join("media/stringtables").join(format!("{lang}.zip")))
    }

    pub fn insert(&mut self, stem: &str, table: StrTable) {
        let key = stem.to_lowercase();
        self.by_hash.insert(str_hash(stem), key.clone());
        self.files.insert(key, (stem.to_string(), table));
    }

    pub fn file(&self, stem: &str) -> Option<&StrTable> {
        self.files.get(&stem.to_lowercase()).map(|(_, t)| t)
    }

    /// `File:IDS_x` (file stem case-insensitive).
    pub fn get(&self, file: &str, id: &str) -> Option<&str> {
        self.file(file)?.get(id)
    }

    /// gamedb `_&n` value.
    pub fn gamedb(&self, n: u32) -> Option<&str> {
        let stem = self.by_hash.get(&((n >> 16) as u16))?;
        let (_, t) = self.files.get(stem)?;
        t.texts.get(&(n as u16)).map(String::as_str)
    }

    /// Resolves `File:IDS_x` or a gamedb `_&n` string.
    pub fn resolve(&self, r: &str) -> Option<&str> {
        if let Some(n) = r.strip_prefix("_&") {
            return self.gamedb(n.parse().ok()?);
        }
        let (file, id) = r.split_once(':')?;
        self.get(file, id)
    }
}

/// Removes `{font…}` / `{colour…}` / `{color…}` / `{font_pop}` tags, including the doubled
/// `{{font='SYM'}}` form. Argument slots like `{0}` and any other text are kept.
pub fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let open = tail.bytes().take_while(|&b| b == b'{').count();
        let body = &tail[open..];
        let is_tag = ["font", "colour", "color"].iter().any(|t| body.starts_with(t));
        match body.find('}') {
            Some(j) if is_tag => {
                let close = body[j..].bytes().take(open).take_while(|&b| b == b'}').count();
                rest = &body[j + close..];
            }
            _ => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::strip_markup;

    #[test]
    fn markup() {
        assert_eq!(strip_markup("{font='SYM'}*{font_pop} NEW"), "* NEW");
        assert_eq!(strip_markup("{colour='255 0 0 255'}{0}{font_pop} km/h"), "{0} km/h");
        assert_eq!(strip_markup("PRICE {{font='SYM'}}${{font_pop}}"), "PRICE $");
        assert_eq!(strip_markup("{1}:{2} {"), "{1}:{2} {");
    }
}
