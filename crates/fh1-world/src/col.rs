//! Track collision files, Horizon version 4 (layout per Doliman100's community `col.bt`,
//! checked against the FH1 disc; see `docs/COLORADO_RECON.md`).
//!
//! `<Track>_track_00.col` is a grid of 100 m stream squares. Each square's collision mesh is a
//! separate `<array_id>.fiz` in the track's `bin.zip`, covering the square plus an 8 m cushion
//! on every side, so neighbouring squares share their border triangles.
//!
//! Everything is big-endian, except a few floats in the `.col` header that are stored
//! little-endian (cell sizes; unused here).

use anyhow::{bail, ensure, Context, Result};

const COL_MAGIC: u32 = 0xABCD_1234;
const COL_VERSION_HORIZON: u32 = 4;
/// Forza Motorsport 4: the same grid header, but each square's mesh is stored inside the `.col` itself (no `.fiz`
/// files), with 32-byte vertices (docs/FM4_RECON.md).
const COL_VERSION_FM4: u32 = 2;

/// The stream-square grid from a `.col` file.
#[derive(Debug, Clone)]
pub struct Grid {
    /// Cells along X and Z.
    pub cells: [u32; 2],
    /// World-space min / max corner as (x, z).
    pub min: [f32; 2],
    pub max: [f32; 2],
    /// Square edge length (100 m on Colorado).
    pub square_size: f32,
    /// Extra border each square's mesh covers (8 m on Colorado).
    pub cushion: f32,
    pub squares: Vec<Square>,
}

#[derive(Debug, Clone, Copy)]
pub struct Square {
    /// Index of the square's mesh: `<array_id>.fiz`.
    pub array_id: u32,
    /// Cell in the grid, `z * cells_x + x`.
    pub grid_id: u32,
    /// FM4: (offset, size) of the square's mesh inside the `.col` (read it with [`parse_fm4_square`]).
    pub embedded: Option<(u32, u32)>,
}

pub fn parse_col(b: &[u8]) -> Result<Grid> {
    ensure!(b.len() >= 0x54, "col: file too short ({} bytes)", b.len());
    let version = be_u32(b, 0x2C);
    let magic = be_u32(b, 0x30);
    ensure!(magic == COL_MAGIC, "col: bad magic {magic:08X}");
    ensure!(version == COL_VERSION_HORIZON || version == COL_VERSION_FM4, "col: version {version}, expected 4 (Horizon) or 2 (FM4)");
    let count = be_u32(b, 0x4C) as usize;
    let at = be_u32(b, 0x50) as usize;
    ensure!(at + count * 24 <= b.len(), "col: square table runs past the end");
    let squares = (0..count)
        .map(|i| {
            let e = at + i * 24;
            let embedded = (version == COL_VERSION_FM4).then(|| (be_u32(b, e + 12), be_u32(b, e + 16)));
            Square { array_id: be_u32(b, e + 4), grid_id: be_u32(b, e + 8), embedded }
        })
        .collect();
    Ok(Grid {
        cells: [be_u32(b, 0), be_u32(b, 4)],
        min: [be_f32(b, 8), be_f32(b, 12)],
        max: [be_f32(b, 16), be_f32(b, 20)],
        square_size: be_f32(b, 0x34),
        cushion: be_f32(b, 0x3C),
        squares,
    })
}

/// One collision triangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Poly {
    /// Unknown bit flags (0x0E on most road triangles).
    pub flags: u8,
    /// Index into the surface list of `surfaceTypes.xml` (0 = Asphalt, 1 = Dirt, 2 = Grass, ...).
    pub surface: u8,
    /// Route mask (0x8000 on most triangles; meaning unknown).
    pub routes: u16,
    /// Vertex indices into [`FizMesh::verts`].
    pub v: [u32; 3],
    /// Unit face normal as stored.
    pub normal: [f32; 3],
}

/// The collision mesh of one stream square.
#[derive(Debug, Clone, Default)]
pub struct FizMesh {
    pub verts: Vec<[f32; 3]>,
    pub polys: Vec<Poly>,
}

/// Parses a `.fiz`: a 32-byte header (`fiz `, version, data sizes, AI block index), then the
/// mesh. Offsets inside the mesh are relative to its start (byte 32). Vertices are 16 bytes
/// (x, y, z, packed normal); polygons are 32 bytes (flags, surface, routes, 3 vertex byte
/// offsets, face normal, 0x00FFFFFF). The trailing collision grid and AI block are skipped.
pub fn parse_fiz(b: &[u8]) -> Result<FizMesh> {
    ensure!(b.len() >= 48 && &b[..4] == b"fiz ", "fiz: bad magic");
    parse_mesh(b, 32, 16)
}

/// One FM4 square: the `.fiz` mesh body without the 32-byte `fiz ` header, with 32-byte vertices (xyz first, the
/// rest unread). Offsets are relative to the square's start. Polygons are FH1's 32-byte records, except the last
/// word isn't 0x00FFFFFF. Their route masks are 0 (each layout has its own `.col`), so every triangle is given the
/// always-present bit 0x8000 that FH1's free roam keeps.
pub fn parse_fm4_square(b: &[u8]) -> Result<FizMesh> {
    ensure!(b.len() >= 16, "col (FM4): square too short");
    let mut mesh = parse_mesh(b, 0, 32)?;
    for p in &mut mesh.polys {
        p.routes |= 0x8000;
    }
    Ok(mesh)
}

/// The mesh at `b[base..]`: `u32 vertex count, polygon count, vertex offset, polygon offset` (offsets from `base`),
/// vertices `vstride` bytes apart (xyz first), 32-byte polygons.
fn parse_mesh(b: &[u8], base: usize, vstride: usize) -> Result<FizMesh> {
    let nv = be_u32(b, base) as usize;
    let np = be_u32(b, base + 4) as usize;
    let voff = be_u32(b, base + 8) as usize;
    let poff = be_u32(b, base + 12) as usize;
    ensure!(base + voff + nv * vstride <= b.len(), "fiz: vertices run past the end");
    ensure!(base + poff + np * 32 <= b.len(), "fiz: polygons run past the end");

    let verts = (0..nv)
        .map(|i| {
            let o = base + voff + i * vstride;
            [be_f32(b, o), be_f32(b, o + 4), be_f32(b, o + 8)]
        })
        .collect();
    let mut polys = Vec::with_capacity(np);
    for i in 0..np {
        let o = base + poff + i * 32;
        let w0 = be_u32(b, o);
        let mut v = [0u32; 3];
        for (k, slot) in v.iter_mut().enumerate() {
            let r = be_u32(b, o + 4 + k * 4) as usize;
            let idx = r.checked_sub(voff).map(|d| (d / vstride, d % vstride));
            match idx {
                Some((idx, 0)) if idx < nv => *slot = idx as u32,
                _ => bail!("fiz: polygon {i} has bad vertex reference {r:#X}"),
            }
        }
        polys.push(Poly {
            flags: (w0 >> 24) as u8,
            surface: (w0 >> 16) as u8,
            routes: w0 as u16,
            v,
            normal: [be_f32(b, o + 16), be_f32(b, o + 20), be_f32(b, o + 24)],
        });
    }
    Ok(FizMesh { verts, polys })
}

fn be_u32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn be_f32(b: &[u8], o: usize) -> f32 {
    f32::from_bits(be_u32(b, o))
}

/// Finds `<track>/Ribbon_00/*_track_00.col` (the name's case varies by track).
pub fn find_col(track_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let ribbon = track_dir.join("Ribbon_00");
    std::fs::read_dir(&ribbon)
        .with_context(|| format!("reading {}", ribbon.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().ends_with("_track_00.col")))
        .with_context(|| format!("no *_track_00.col in {}", ribbon.display()))
}
