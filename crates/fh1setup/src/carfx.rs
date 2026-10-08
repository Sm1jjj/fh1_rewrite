//! Raw car streams for renderers that run the game's car shaders (fh1-render; the car vertex shader
//! decodes the packed vertices itself, so the pools are kept exactly as stored).
//!
//! Output, inside the `cars` group:
//! - `<CAR>/fx/<carbin name>.fxcar`: one per `.carbin` in the car's zip (body, `_lod0`, `_cockpit`,
//!   calipers, rotors; not the geometry-free `stripped_*` stubs).
//! - `<CAR>/fx/<name>.xml`: the car's `*ShaderSettings*.xml` (root of its zip).
//! - `wheels/<rim zip stem>/<carbin name>.fxcar`: every carbin in `media/wheels/*.zip` (type 3 rims).
//! - `shared/<path>`: every `.xml` in `media/cars/Shared.zip` (textures.xml, ShaderSettings/*.xml, ...).
//!
//! `.fxcar`, little-endian: `b"FH1FXCAR"`, u32 version 3, u32 carbin type id, u32 section count; per
//! section: u8 name length + name, f32x3 offset, f32x3 bounds_min, f32x3 bounds_max, then two pools (LOD1-5
//! pool, then LOD0 pool), each `u32 stride (28 / 32, 0 = empty), u32 vertex count, f32x3 pool_min,
//! f32x3 pool_max, u32 extra stride, u32 extra length, stride * count bytes AS STORED (big-endian), extra
//! bytes (also as stored; LOD pool: per-vertex stream from the section tail, 16 B on many rims; LOD0 pool:
//! the 4 B/vertex stream of `_lod0` / cockpit carbins; meanings not decoded)`; then u32
//! subsection count and per subsection `u8 name length + name, i32 lod (0 = LOD0 pool, else LOD pool),
//! f32x8 uv_transform (x_off, x_scale, y_off, y_scale for uv0 then uv1), u32 index count, u32 indices`
//! (triangle list); then (v3) f32x2 shCompressionFactors (offset, scale) from the carbin header.
//! See `fh1_formats::carbin` for the meaning of pool_min/max and the extra stream.

use std::path::Path;

use anyhow::Result;
use fh1_formats::carbin::{self, RawPool};
use fh1_formats::zip::Archive;

pub fn fxcar(c: &carbin::Carbin) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"FH1FXCAR");
    let u32s = |b: &mut Vec<u8>, v: &[u32]| v.iter().for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
    let f32s = |b: &mut Vec<u8>, v: &[f32]| v.iter().for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
    let name = |b: &mut Vec<u8>, s: &str| {
        let s = &s.as_bytes()[..s.len().min(255)];
        b.push(s.len() as u8);
        b.extend_from_slice(s);
    };
    u32s(&mut b, &[3, c.type_id, c.sections.len() as u32]);
    for s in &c.sections {
        name(&mut b, &s.name);
        f32s(&mut b, &s.offset);
        f32s(&mut b, &s.bounds_min);
        f32s(&mut b, &s.bounds_max);
        for p in [&s.lod_raw, &s.lod0_raw] {
            pool(&mut b, p);
        }
        u32s(&mut b, &[s.subsections.len() as u32]);
        for sub in &s.subsections {
            name(&mut b, &sub.name);
            b.extend_from_slice(&sub.lod.to_le_bytes());
            f32s(&mut b, &sub.uv_transform);
            u32s(&mut b, &[sub.indices.len() as u32]);
            u32s(&mut b, &sub.indices);
        }
    }
    f32s(&mut b, &c.sh_compression.unwrap_or(SH_COMPRESSION_DEFAULT));
    b
}

/// shCompressionFactors when the header pair isn't found (the typical value, e.g. ALF_8C_08_lod0).
const SH_COMPRESSION_DEFAULT: [f32; 2] = [-0.33793, 0.67674];

fn pool(b: &mut Vec<u8>, p: &RawPool) {
    let count = if p.stride == 0 { 0 } else { p.data.len() / p.stride };
    for v in [p.stride as u32, count as u32] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for v in p.pool_min.iter().chain(&p.pool_max) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&(p.extra_stride as u32).to_le_bytes());
    b.extend_from_slice(&(p.extra.len() as u32).to_le_bytes());
    b.extend_from_slice(&p.data);
    b.extend_from_slice(&p.extra);
}

/// Every carbin (as `.fxcar`) and, with `xml_filter`, matching XML files of a zip into `dir`.
/// Returns (carbins written, carbins that didn't parse).
fn export_zip(zip: &Path, dir: &Path, xml_filter: impl Fn(&str) -> bool) -> Result<(usize, usize)> {
    let mut ar = Archive::open(zip)?;
    let (mut ok, mut bad) = (0, 0);
    let mut seen = std::collections::HashSet::new();
    for e in ar.entries.clone() {
        let name = e.name.replace(char::from(92), "/");
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        // `stripped_*.carbin` are geometry-free stubs (1-200 KB) next to the real files.
        if lower.ends_with(".carbin") && !lower.rsplit('/').next().unwrap().starts_with("stripped_") {
            match carbin::parse(&ar.read(&e)?) {
                Ok(c) => {
                    std::fs::create_dir_all(dir)?;
                    let stem = name.rsplit('/').next().unwrap().trim_end_matches(".carbin").trim_end_matches(".CARBIN");
                    std::fs::write(dir.join(format!("{stem}.fxcar")), fxcar(&c))?;
                    ok += 1;
                }
                Err(_) => bad += 1,
            }
        } else if lower.ends_with(".xml") && xml_filter(&name) {
            let dst = dir.join(&name);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::write(dst, ar.read(&e)?)?;
        }
    }
    Ok((ok, bad))
}

/// One car: its zip's carbins and ShaderSettings XMLs into `<car dir>/fx`.
pub fn export_car(zip: &Path, car_dir: &Path) -> Result<(usize, usize)> {
    export_zip(zip, &car_dir.join("fx"), |n| !n.contains('/') && n.to_ascii_lowercase().contains("shadersettings"))
}

/// Wheels (`media/wheels/*.zip`) and `Shared.zip`'s XML files.
pub fn export_shared(disc: &Path, out: &Path) -> Result<()> {
    let (mut ok, mut bad, mut rims) = (0, 0, 0);
    let mut zips: Vec<_> = std::fs::read_dir(disc.join("media/wheels"))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    zips.sort();
    for z in zips.iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))) {
        let stem = z.file_stem().unwrap().to_string_lossy().into_owned();
        let (o, b) = export_zip(z, &out.join("wheels").join(&stem), |n| !n.contains('/') && n.to_ascii_lowercase().contains("shadersettings"))?;
        (ok, bad, rims) = (ok + o, bad + b, rims + 1);
    }
    export_zip(&disc.join("media/cars/Shared.zip"), &out.join("shared"), |_| true)?;
    println!("[cars] fx: {rims} wheel zips, {ok} wheel carbins ({bad} unparsed), Shared.zip XML");
    Ok(())
}
