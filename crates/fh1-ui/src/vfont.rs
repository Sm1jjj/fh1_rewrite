//! `*_vector_aa.dt` vector fonts (`media/ui/Fonts.zip`, Turn 10 FontCompiler, CAFF container with a
//! `vfont` asset) and `fontmap.xml`. VERIFIED field positions on all 16 fonts:
//!
//! - `vfont+0x18`: `u32 charset_off, u32 glyph_hdr_off` (relative to the `vfont` tag).
//! - charset: `u32[9] = 0, 0, 0, num_chars, (u16 default_char, u16 0), 1, num_chars, hash_off, hash_size`;
//!   `hash_off` → `{u16 unicode, u16 glyph_index}[hash_size]`, slot = codepoint % hash_size.
//! - glyph header: `u32[8]` (… `vb_bytes, num_verts, num_indices` at 5..8), `f32 space_width, size,
//!   baseline_offset`, `i32[10] = 8, winAscent, winDescent, typoAscent, typoDescent, typoLineGap,
//!   macAscent, macDescent, macLineGap, EM`, then 40-byte glyph records.
//! - `.gpu` at `file_end − (vb_bytes + 2·num_indices)`: vertices `4 × f16 BE (x, y, u, v)`, then u16 BE
//!   triangle-list indices relative to the glyph's first vertex.
//!
//! Each glyph is two pre-triangulated Loop-Blinn meshes sharing one index range. **Inner** triangles
//! (all x ≥ 0) fill where `u² − v ≤ 0`. **Outer** triangles (any x < 0: x stored negated) are the area
//! around the glyph; only its curve triangles (some u ≠ 0) contribute, filling where `u² − v > 0`
//! (concave curves). VERIFIED by rasterising every glyph of all 16 fonts. The sign of x is kept as stored.
//! Units: 1.0 = 1 em; draw a glyph's mesh at `pen + offset`.

use std::collections::HashMap;

use crate::reader::{be_u16, be_u32, f16_to_f32, latin1};
use crate::{need, Error, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Em units.
    pub space_width: f32,
    /// Compiler point size (14.0).
    pub size: f32,
    /// ≈ top of line box to baseline, em (GUESS).
    pub baseline_offset: f32,
    /// Always 8.
    pub unk8: i32,
    pub win_ascent: i32,
    pub win_descent: i32,
    pub typo_ascent: i32,
    pub typo_descent: i32,
    pub typo_line_gap: i32,
    pub mac_ascent: i32,
    pub mac_descent: i32,
    pub mac_line_gap: i32,
    /// Units per em of the source face (991 for A/B/D/E).
    pub em: i32,
}

/// Vertices `[x, y, u, v]` and triangle-list indices into them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub verts: Vec<[f32; 4]>,
    pub indices: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Glyph {
    pub codepoint: u32,
    /// Em units.
    pub advance: f32,
    /// 0xFFFF / 0 on every font (no kerning in FH1).
    pub kern_start: u16,
    pub kern_count: u16,
    /// `(offX, offY)`: draw the mesh at `pen + offset` (VERIFIED by side-bearing symmetry).
    pub offset: [f32; 2],
    /// Always 1.
    pub scale: f32,
    pub inner: Mesh,
    pub outer: Mesh,
    /// Raw record fields: vertex buffer byte offset, vertex count, first index, triangle count.
    pub vb_offset: u32,
    pub vertex_count: u32,
    pub first_index: u32,
    pub tri_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    /// CAFF version string (`21.11.05.0034`).
    pub caff_version: String,
    /// vfont version string (`12.07.06.0035`).
    pub vfont_version: String,
    pub metrics: Metrics,
    /// Fallback character (9633 = □).
    pub default_char: u16,
    pub glyphs: HashMap<char, Glyph>,
    /// Glyphs in file order.
    pub order: Vec<char>,
    /// The direct lookup table `{unicode, glyph_index}`, slot = codepoint % len.
    pub hash_table: Vec<(u16, u16)>,
    /// Totals from the glyph header.
    pub num_verts: u32,
    pub num_indices: u32,
}

impl Font {
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.get(..4) != Some(b"CAFF".as_slice()) {
            return Err(Error::BadMagic("CAFF"));
        }
        let t = |what| Error::Truncated { what, at: 0 };
        let caff_version = zstr(d.get(4..36).ok_or(t("caff header"))?);
        let v = find(d, b"vfont\0").ok_or(Error::Format("no vfont asset".into()))?;
        let vfont_version = zstr(d.get(v + 6..v + 0x18).ok_or(t("vfont header"))?);
        let cs = v + be_u32(d, v + 0x18).ok_or(t("vfont header"))? as usize;
        let gh = v + be_u32(d, v + 0x1C).ok_or(t("vfont header"))? as usize;

        let csw = words::<9>(d, cs).ok_or(t("vfont charset"))?;
        let num_chars = csw[3] as usize;
        let default_char = (csw[4] >> 16) as u16;
        let (hash_off, hash_size) = (v + csw[7] as usize, csw[8] as usize);
        let hash_table = (0..hash_size)
            .map(|i| Some((be_u16(d, hash_off + 4 * i)?, be_u16(d, hash_off + 4 * i + 2)?)))
            .collect::<Option<Vec<_>>>()
            .ok_or(t("vfont hash table"))?;

        let ghw = words::<8>(d, gh).ok_or(t("vfont glyph header"))?;
        let (vb_bytes, num_verts, num_indices) = (ghw[5] as usize, ghw[6], ghw[7]);
        need(vb_bytes == 8 * num_verts as usize, || "vfont vertex buffer size".into())?;
        let f = |o: usize| be_u32(d, o).map(f32::from_bits).ok_or(t("vfont metrics"));
        let i = |k: usize| be_u32(d, gh + 0x2C + 4 * k).map(|x| x as i32).ok_or(t("vfont metrics"));
        let metrics = Metrics {
            space_width: f(gh + 0x20)?,
            size: f(gh + 0x24)?,
            baseline_offset: f(gh + 0x28)?,
            unk8: i(0)?,
            win_ascent: i(1)?,
            win_descent: i(2)?,
            typo_ascent: i(3)?,
            typo_descent: i(4)?,
            typo_line_gap: i(5)?,
            mac_ascent: i(6)?,
            mac_descent: i(7)?,
            mac_line_gap: i(8)?,
            em: i(9)?,
        };

        let gpu_len = vb_bytes + 2 * num_indices as usize;
        let gpu = d.len().checked_sub(gpu_len).ok_or(t("vfont gpu buffers"))?;
        need(gpu >= gh + 0x54 + 40 * num_chars, || "vfont glyph records overlap gpu data".into())?;
        let vb = &d[gpu..gpu + vb_bytes];
        let ib = &d[gpu + vb_bytes..];

        let mut glyphs = HashMap::new();
        let mut order = Vec::new();
        for k in 0..num_chars {
            let g = glyph(d, gh + 0x54 + 40 * k, vb, ib)?;
            let c = char::from_u32(g.codepoint)
                .ok_or_else(|| Error::Format(format!("vfont codepoint {:#x}", g.codepoint)))?;
            order.push(c);
            glyphs.insert(c, g);
        }
        Ok(Self {
            caff_version, vfont_version, metrics, default_char, glyphs, order, hash_table, num_verts, num_indices,
        })
    }

    /// Glyph for `c`, falling back to the default character.
    pub fn glyph(&self, c: char) -> Option<&Glyph> {
        self.glyphs.get(&c).or_else(|| self.glyphs.get(&char::from_u32(self.default_char as u32)?))
    }

    /// Line height in em: (winAscent + winDescent) / EM (GUESS).
    pub fn line_height(&self) -> f32 {
        let m = &self.metrics;
        (m.win_ascent + m.win_descent) as f32 / m.em.max(1) as f32
    }
}

fn glyph(d: &[u8], o: usize, vb: &[u8], ib: &[u8]) -> Result<Glyph> {
    let t = Error::Truncated { what: "vfont glyph record", at: o };
    let w = words::<10>(d, o).ok_or(t)?;
    let codepoint = w[0];
    let advance = f32::from_bits(w[1]);
    let (kern_start, kern_count) = ((w[2] >> 16) as u16, w[2] as u16);
    let (vb_offset, vertex_count, first_index, tri_count) = (w[3], w[4], w[5], w[6]);
    let offset = [f32::from_bits(w[7]), f32::from_bits(w[8])];
    let scale = f32::from_bits(w[9]);

    let v0 = vb_offset as usize;
    let vbytes = vertex_count as usize * 8;
    let gv = v0.checked_add(vbytes).and_then(|e| vb.get(v0..e));
    let gv = gv.ok_or(Error::Format(format!("vfont glyph {codepoint:#x}: vertices out of range")))?;
    let verts: Vec<[f32; 4]> = gv
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| {
            let h = |i: usize| f16_to_f32(u16::from_be_bytes([c[2 * i], c[2 * i + 1]]));
            [h(0), h(1), h(2), h(3)]
        })
        .collect();
    let i0 = first_index as usize * 2;
    let ibytes = tri_count as usize * 6;
    let gi = i0.checked_add(ibytes).and_then(|e| ib.get(i0..e));
    let gi = gi.ok_or(Error::Format(format!("vfont glyph {codepoint:#x}: indices out of range")))?;

    let (mut inner, mut outer) = (Split::default(), Split::default());
    for tri in gi.as_chunks::<6>().0 {
        let idx = [0, 1, 2].map(|k| u16::from_be_bytes([tri[2 * k], tri[2 * k + 1]]) as usize);
        let p = idx.map(|k| verts.get(k).copied());
        let [Some(a), Some(b), Some(c)] = p else {
            return Err(Error::Format(format!("vfont glyph {codepoint:#x}: index out of range")));
        };
        let dst = if [a, b, c].iter().any(|v| v[0] < 0.0) { &mut outer } else { &mut inner };
        for (k, v) in idx.into_iter().zip([a, b, c]) {
            dst.push(k, v);
        }
    }
    Ok(Glyph {
        codepoint, advance, kern_start, kern_count, offset, scale,
        inner: inner.mesh, outer: outer.mesh, vb_offset, vertex_count, first_index, tri_count,
    })
}

/// Builds one sub-mesh, remapping the glyph's vertex indices to its own vertex list.
#[derive(Default)]
struct Split {
    mesh: Mesh,
    map: HashMap<usize, u16>,
}

impl Split {
    fn push(&mut self, k: usize, v: [f32; 4]) {
        let verts = &mut self.mesh.verts;
        let i = *self.map.entry(k).or_insert_with(|| {
            verts.push(v);
            (verts.len() - 1) as u16
        });
        self.mesh.indices.push(i);
    }
}

fn words<const N: usize>(d: &[u8], o: usize) -> Option<[u32; N]> {
    let mut a = [0; N];
    for (k, w) in a.iter_mut().enumerate() {
        *w = be_u32(d, o + 4 * k)?;
    }
    Some(a)
}

fn find(d: &[u8], pat: &[u8]) -> Option<usize> {
    d.windows(pat.len()).position(|w| w == pat)
}

fn zstr(b: &[u8]) -> String {
    latin1(b.split(|&c| c == 0).next().unwrap_or_default())
}

/// `fontmap.xml`: scene / authoring font name → font target (`A`, `B`, `D`, `E`, `SYM`, `DG1`..`DG6`).
#[derive(Debug, Clone, Default)]
pub struct FontMap {
    /// Lower-case font name → target as written.
    pub mappings: HashMap<String, String>,
}

impl FontMap {
    /// Reads the `<mapping fontname="…" target="…"/>` lines.
    pub fn parse(xml: &str) -> Self {
        let mut mappings = HashMap::new();
        for tag in xml.split('<').filter(|t| t.starts_with("mapping")) {
            if let (Some(n), Some(t)) = (attr(tag, "fontname"), attr(tag, "target")) {
                mappings.insert(n.to_lowercase(), t.to_string());
            }
        }
        Self { mappings }
    }

    /// Target for a font name as scenes store it (`horizon_e` → `E`; case-insensitive). Names not in
    /// the map fall back to the first-letter rule for `x_…` authoring faces (`e_helvetica67…` → `E`),
    /// which is a GUESS (docs/UI.md).
    pub fn resolve(&self, name: &str) -> Option<&str> {
        let key = name.trim().to_lowercase();
        if let Some(t) = self.mappings.get(&key) {
            return Some(t);
        }
        let (prefix, _) = key.split_once('_')?;
        self.mappings.get(prefix).map(String::as_str)
    }

    /// The `.dt` file of a target in `Fonts.zip` (`E` → `e_vector_aa.dt`). DG6 has no file on the disc.
    pub fn file_name(target: &str) -> String {
        format!("{}_vector_aa.dt", target.to_lowercase())
    }
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let i = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let rest = &tag[i..];
    Some(&rest[..rest.find('"')?])
}

#[cfg(test)]
mod tests {
    use super::FontMap;

    #[test]
    fn fontmap() {
        let m = FontMap::parse(
            r#"<fontmap><mapping fontname="Horizon_E" target="E" />
               <mapping fontname="E" target="E"/><mapping fontname="Horizon_C"  target="A" />
               <mapping fontname="lcd" target="DG2" /></fontmap>"#,
        );
        assert_eq!(m.resolve("horizon_e"), Some("E"));
        assert_eq!(m.resolve("Horizon_C"), Some("A"));
        assert_eq!(m.resolve("e_helvetica67-condensedmedium medium"), Some("E"));
        assert_eq!(m.resolve("LCD"), Some("DG2"));
        assert_eq!(m.resolve("helvetica53-e"), None);
        assert_eq!(FontMap::file_name("DG2"), "dg2_vector_aa.dt");
    }
}
