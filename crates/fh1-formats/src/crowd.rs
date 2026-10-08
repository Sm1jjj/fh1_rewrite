//! Spectator crowds: `.pgeo` objects of header type 3 (`CProceduralCharacters`) and
//! `media/Spectators.zip` (`spectators.xml`, skinned models, animations, sprite atlas).
//!
//! `.pgeo` layout (big-endian; loader 0x82E08200 = vtable 0x8200289C slot 0x78, VERIFIED on all 1,783
//! type-3 objects in Colorado: the walk ends exactly at the end of the file):
//! - common header: `OEGP`, version 0x2A, bbox min @0x10 / max @0x20 (collision space), type @0x30 = 3,
//!   name length (with NUL) @0x3C. Name @0x60, or @0x98 when 0x60 holds `0xFFFFFFFF` (the 1,615
//!   event-crowd objects: an extra 0x38-byte block precedes the name).
//! - 16-aligned after the name, a 0x40-byte block: bbox min @0, max @0x10 (the spectators' bounds),
//!   `u32 count` @0x20 (+ pointer slot), `u32 group count` @0x28 (+ pointer slot), u32 @0x30, byte @0x34.
//! - `count` spectators of 12 bytes: u16 x, y, z = fractions (/65535) of the block's bbox; u8 heading
//!   (/256 of a turn, uniform over 0..255); u8 crowd class index = `crowdclass id - 1` in
//!   `spectators.xml` (VERIFIED by meaning: classes 17-19 = GigCheer01-03 appear only in
//!   `festivalcrowd_stages`, classes 9/10 = SatShoulders stand/sit always come in equal numbers);
//!   4 more bytes (byte 8 is 0 or 1, rest 0; unknown).
//! - then (4-aligned) `group count` 12-byte entries `(pointer slot, runtime word, u32 crowd class)`: the
//!   walker paths ([`WalkerPath`], loader 0x82DFE050). Per entry, 16-aligned, in order: a 0x30-byte block
//!   (bbox min @0 / max @0x10, pointer slot @0x20, u32 knot count @0x24, f32 path length @0x28), then
//!   16-aligned knot records of 0x60 bytes: centre, in-handle, out-handle (vec4 each, w = a tag), a zero
//!   vec4, and 8 f32 of cumulative arc length along the path (span k sampled at t = 0, 1/8 .. 7/8).
//!   VERIFIED: the walk ends exactly at the end of all 5 objects that have paths (247 paths, 5,548
//!   knots, all class 3 = `Walkers`); the arc-length tables increase steadily and reach the stored length.
//!   Reading the knots as cubic Bézier spans (centre, out-handle, next in-handle, next centre) is INFERRED
//!   from that geometry.

use crate::Error;

pub mod anim;

#[derive(Debug, Clone, Copy)]
pub struct Spectator {
    /// Collision space (left-handed, +Z north): negate Z for the engine.
    pub position: [f32; 3],
    /// Fraction of a turn × 256 (direction convention UNVERIFIED).
    pub heading: u8,
    /// `crowdclass id - 1`.
    pub class: u8,
    pub extra: [u8; 4],
}

#[derive(Debug, Clone)]
pub struct Crowd {
    pub name: String,
    /// False for the `0xFFFFFFFF`-named (event) objects.
    pub named: bool,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub spectators: Vec<Spectator>,
    /// Walker paths (the second list).
    pub paths: Vec<WalkerPath>,
}

/// One walker path knot (collision space).
#[derive(Debug, Clone, Copy)]
pub struct Knot {
    pub centre: [f32; 3],
    pub in_handle: [f32; 3],
    pub out_handle: [f32; 3],
    /// Cumulative arc length at t = 0, 1/8 .. 7/8 of the span that starts here.
    pub distances: [f32; 8],
}

#[derive(Debug, Clone)]
pub struct WalkerPath {
    /// Crowd class index (`crowdclass id - 1`; 3 = Walkers on every Colorado path).
    pub class: u32,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub length: f32,
    pub knots: Vec<Knot>,
}

impl WalkerPath {
    /// Position (collision space) and unit tangent `distance` metres along the path (wraps at `length`).
    /// Finds the span and t from the arc-length tables (linear between the 8 samples), then evaluates the
    /// cubic Bézier `centre[k], out[k], in[k+1], centre[k+1]`. When `length` runs past the last knot's start
    /// the path is a loop: its last span goes back to knot 0 (INFERRED from the arc lengths).
    pub fn point_at(&self, distance: f32) -> Option<([f32; 3], [f32; 3])> {
        let n = self.knots.len();
        if n < 2 {
            return None;
        }
        let total = self.length.max(1e-3);
        let s = distance.rem_euclid(total);
        // Span k covers [distances[0] of k, distances[0] of k + 1) (the last ends at `length`).
        let closed = total > self.knots[n - 1].distances[0] + 0.01;
        let spans = if closed { n } else { n - 1 };
        let span_end = |k: usize| if k + 1 < n { self.knots[k + 1].distances[0] } else { total };
        let k = (0..spans).find(|&k| s < span_end(k)).unwrap_or(spans - 1);
        let kn = &self.knots[k];
        let mut samples = [0f32; 9];
        samples[..8].copy_from_slice(&kn.distances);
        samples[8] = span_end(k);
        let j = (0..8).find(|&j| s < samples[j + 1]).unwrap_or(7);
        let seg = (samples[j + 1] - samples[j]).max(1e-6);
        let t = ((j as f32 + ((s - samples[j]) / seg).clamp(0.0, 1.0)) / 8.0).clamp(0.0, 1.0);
        let next = &self.knots[(k + 1) % n];
        let (p0, p1, p2, p3) = (kn.centre, kn.out_handle, next.in_handle, next.centre);
        let u = 1.0 - t;
        let pos = [0, 1, 2].map(|i| u * u * u * p0[i] + 3.0 * u * u * t * p1[i] + 3.0 * u * t * t * p2[i] + t * t * t * p3[i]);
        let d = [0, 1, 2].map(|i| 3.0 * u * u * (p1[i] - p0[i]) + 6.0 * u * t * (p2[i] - p1[i]) + 3.0 * t * t * (p3[i] - p2[i]));
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
        Some((pos, d.map(|x| x / l)))
    }
}

/// Header type (0x30) of a `.pgeo`: 3 = crowd.
pub fn pgeo_type(d: &[u8]) -> Option<u32> {
    d.get(0x30..0x34).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
}

fn be32(d: &[u8], o: usize) -> Result<u32, Error> {
    Ok(u32::from_be_bytes(d.get(o..o + 4).ok_or(Error::Truncated("crowd"))?.try_into().unwrap()))
}

fn f32x3(d: &[u8], o: usize) -> Result<[f32; 3], Error> {
    Ok([f32::from_bits(be32(d, o)?), f32::from_bits(be32(d, o + 4)?), f32::from_bits(be32(d, o + 8)?)])
}

/// Parse a type-3 `.pgeo`.
pub fn parse(d: &[u8]) -> Result<Crowd, Error> {
    if d.get(..4) != Some(b"OEGP") {
        return Err(Error::BadMagic("pgeo: OEGP"));
    }
    if pgeo_type(d) != Some(3) {
        return Err(Error::Unsupported("pgeo: not a crowd (type 3)".into()));
    }
    let len = be32(d, 0x3C)? as usize;
    let named = d.get(0x60..0x64) != Some(&[0xFF; 4]);
    let at = if named { 0x60 } else { 0x98 };
    let raw = d.get(at..at + len).ok_or(Error::Truncated("crowd name"))?;
    let name = raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())].iter().map(|&b| b as char).collect();
    let b = (at + len + 15) & !15;
    let (min, max) = (f32x3(d, b)?, f32x3(d, b + 0x10)?);
    let count = be32(d, b + 0x20)? as usize;
    let groups = be32(d, b + 0x28)? as usize;
    let recs = d.get(b + 0x40..b + 0x40 + count * 12).ok_or(Error::Truncated("crowd spectators"))?;
    let spectators = recs
        .chunks_exact(12)
        .map(|r| {
            let q = |i: usize| u16::from_be_bytes([r[i], r[i + 1]]) as f32 / 65535.0;
            let pos = [0, 1, 2].map(|k| min[k] + q(2 * k) * (max[k] - min[k]));
            Spectator { position: pos, heading: r[6], class: r[7], extra: [r[8], r[9], r[10], r[11]] }
        })
        .collect();
    let mut at = (b + 0x40 + count * 12 + 3) & !3;
    let classes: Vec<u32> = (0..groups).map(|i| be32(d, at + i * 12 + 8)).collect::<Result<_, _>>()?;
    at += groups * 12;
    let mut paths = Vec::with_capacity(groups);
    for class in classes {
        at = (at + 15) & !15;
        let (min, max, n, length) = (f32x3(d, at)?, f32x3(d, at + 0x10)?, be32(d, at + 0x24)? as usize, f32::from_bits(be32(d, at + 0x28)?));
        at += 0x30;
        if n > 0 {
            at = (at + 15) & !15;
        }
        let knots = (0..n)
            .map(|k| {
                let r = at + k * 0x60;
                let mut distances = [0f32; 8];
                for (j, x) in distances.iter_mut().enumerate() {
                    *x = f32::from_bits(be32(d, r + 0x40 + j * 4)?);
                }
                Ok(Knot { centre: f32x3(d, r)?, in_handle: f32x3(d, r + 0x10)?, out_handle: f32x3(d, r + 0x20)?, distances })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        at += n * 0x60;
        paths.push(WalkerPath { class, bbox_min: min, bbox_max: max, length, knots });
    }
    if groups > 0 && at != d.len() {
        return Err(Error::Unsupported(format!("crowd: walker paths end at {at:#x} of {:#x}", d.len())));
    }
    Ok(Crowd { name, named, bbox_min: f32x3(d, 0x10)?, bbox_max: f32x3(d, 0x20)?, spectators, paths })
}

/// A DXT1 `.xds` (`sprites.xds`, the spectator textures) as RGBA8 with DXT1 punch-through alpha (the cut-out the sprite cards
/// need; [`crate::xds::to_rgba8`] decodes DXT1 opaque). Returns (width, height, texels).
pub fn decode_dxt1_alpha(file: &[u8]) -> Result<(u32, u32, Vec<u8>), Error> {
    let (_, img) = crate::xds::decode_base(file)?;
    if img.format != crate::xds::Format::Dxt1 {
        return Err(Error::Unsupported(format!("xds: {:?} is not DXT1", img.format)));
    }
    let (w, h) = (img.width as usize, img.height as usize);
    let mut px = vec![0u32; w * h];
    texture2ddecoder::decode_bc1a(&img.data, w, h, &mut px).map_err(|e| Error::Unsupported(e.to_string()))?;
    // texture2ddecoder packs BGRA into u32 (little-endian: B, G, R, A).
    let rgba = px
        .iter()
        .flat_map(|p| {
            let [b, g, r, a] = p.to_le_bytes();
            [r, g, b, a]
        })
        .collect();
    Ok((img.width, img.height, rgba))
}

// ------------------------------------------------------------------ .skinbin

/// A spectator skinned mesh vertex (model space: +Y up, metres, origin between the feet).
#[derive(Debug, Clone, Copy)]
pub struct SkinVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    /// Skeleton bone indices (the `.anim.bin` skeleton of the model's `_Sit` / `_Stand` set).
    pub bones: [u8; 2],
    /// Byte weights, summing to 255.
    pub weights: [u8; 2],
}

#[derive(Debug, Clone)]
pub struct SkinMesh {
    pub vertices: Vec<SkinVertex>,
    /// Triangle-list indices for all LODs back to back.
    pub indices: Vec<u16>,
    /// Per LOD (4, finest first): index range into `indices`.
    pub lods: Vec<std::ops::Range<usize>>,
}

/// `models/*.skinbin` (big-endian; VERIFIED on all 40 files: the counts account for every byte).
/// Every value is preceded by a u32 1 (a serializer tag). Header: `1, 4` (LOD count), `1, 4`, `1, n0`
/// (LOD 0 index count, start 0), then per LOD 1..3 `1, (u16 start, u16 count)`, then `1, 1`, u32 vertex count.
/// Vertices, 36 bytes: `u32 1`, f32x3 position, f32x3 unit normal, u16x2 UV (/65535), u8x2 bone indices,
/// u8x2 weights (sum 255). Then `1, index count` and the u16 triangle-list indices (count = `start + count`
/// of the last LOD), then u32 vertex count, u32 index count.
pub fn parse_skinbin(d: &[u8]) -> Result<SkinMesh, Error> {
    let lods_n = be32(d, 4)? as usize;
    if lods_n == 0 || lods_n > 8 {
        return Err(Error::Unsupported(format!("skinbin: {lods_n} LODs")));
    }
    let mut lods = vec![0..be32(d, 0x14)? as usize];
    for k in 1..lods_n {
        let w = be32(d, 0x14 + k * 8)?;
        let (start, count) = ((w >> 16) as usize, (w & 0xFFFF) as usize);
        lods.push(start..start + count);
    }
    let at = 0x14 + lods_n * 8;
    let nv = be32(d, at + 4)? as usize;
    let vstart = at + 8;
    let vdata = d.get(vstart..vstart + nv * 36).ok_or(Error::Truncated("skinbin vertices"))?;
    let f = |b: &[u8], o: usize| f32::from_bits(u32::from_be_bytes(b[o..o + 4].try_into().unwrap()));
    let vertices = vdata
        .chunks_exact(36)
        .map(|v| SkinVertex {
            position: [f(v, 4), f(v, 8), f(v, 12)],
            normal: [f(v, 16), f(v, 20), f(v, 24)],
            uv: [u16::from_be_bytes([v[28], v[29]]) as f32 / 65535.0, u16::from_be_bytes([v[30], v[31]]) as f32 / 65535.0],
            bones: [v[32], v[33]],
            weights: [v[34], v[35]],
        })
        .collect();
    let ni = lods.iter().map(|r| r.end).max().unwrap_or(0);
    let istart = vstart + nv * 36 + 8;
    if be32(d, istart - 4)? as usize != ni {
        return Err(Error::Unsupported("skinbin: index count mismatch".into()));
    }
    let idata = d.get(istart..istart + ni * 2).ok_or(Error::Truncated("skinbin indices"))?;
    let indices: Vec<u16> = idata.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    if indices.iter().any(|&i| i as usize >= nv) {
        return Err(Error::Unsupported("skinbin: index out of range".into()));
    }
    Ok(SkinMesh { vertices, indices, lods })
}

// ------------------------------------------------------------------ spectators.xml

#[derive(Debug, Clone, Default)]
pub struct SpectatorModel {
    /// Skinned model stem: `models/<name><skeleton suffix>.skinbin`.
    pub name: String,
    /// `textures/<texture>.dds.xds`.
    pub texture: String,
}

#[derive(Debug, Clone, Default)]
pub struct Skeleton {
    pub name: String,
    /// `_Sit` / `_Stand`.
    pub suffix: String,
    pub modelset: String,
}

#[derive(Debug, Clone, Default)]
pub struct CrowdClass {
    pub id: u32,
    pub name: String,
    pub skeleton: String,
    pub modelset: String,
    pub idle: Vec<String>,
    pub cheer: Vec<String>,
    /// `BBoffsetY` (sprite / bounds offset, metres; UNVERIFIED use).
    pub bb_offset_y: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Spectators {
    /// In file order: also the order of the sprite atlas (`sprites.xds`, `sprites_per_model` cells each).
    pub models: Vec<SpectatorModel>,
    pub sprite_angles: u32,
    pub sprites_per_model: u32,
    pub modelsets: Vec<(String, Vec<String>)>,
    pub skeletons: Vec<Skeleton>,
    pub classes: Vec<CrowdClass>,
}

impl Spectators {
    /// The class of a spectator record (`class` = id - 1).
    pub fn class(&self, index: u8) -> Option<&CrowdClass> {
        self.classes.iter().find(|c| c.id == index as u32 + 1)
    }

    pub fn modelset(&self, name: &str) -> &[String] {
        self.modelsets.iter().find(|(n, _)| n == name).map(|(_, m)| m.as_slice()).unwrap_or(&[])
    }

    pub fn skeleton(&self, name: &str) -> Option<&Skeleton> {
        self.skeletons.iter().find(|s| s.name == name)
    }
}

/// One XML tag: name (with a leading `/` for end tags), attributes, self-closing.
struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
    empty: bool,
}

fn tags(xml: &str) -> Vec<Tag<'_>> {
    let mut out = Vec::new();
    let mut s = xml;
    while let Some(i) = s.find('<') {
        s = &s[i..];
        if s.starts_with("<!--") {
            s = s.find("-->").map_or("", |e| &s[e + 3..]);
            continue;
        }
        let Some(e) = s.find('>') else { break };
        let body = &s[1..e];
        s = &s[e + 1..];
        if body.starts_with('?') || body.starts_with('!') {
            continue;
        }
        let empty = body.ends_with('/');
        let body = body.trim_end_matches('/');
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let (name, mut rest) = body.split_at(name_end);
        let mut attrs = Vec::new();
        while let Some(eq) = rest.find('=') {
            let key = rest[..eq].trim();
            let after = rest[eq + 1..].trim_start();
            let Some(q) = after.chars().next() else { break };
            let Some(close) = after[1..].find(q) else { break };
            attrs.push((key, &after[1..1 + close]));
            rest = &after[close + 2..];
        }
        out.push(Tag { name, attrs, empty });
    }
    out
}

/// Parse `spectators.xml` (commented-out entries are skipped, as the game's parser 0x8314BED8 never sees
/// them).
pub fn parse_spectators_xml(xml: &str) -> Spectators {
    let mut sp = Spectators::default();
    let get = |t: &Tag, k: &str| t.attrs.iter().find(|(a, _)| a.eq_ignore_ascii_case(k)).map(|(_, v)| v.to_string());
    // Context: which element the nested `<model>` / `<modelset>` / `<animation>` tags belong to.
    #[derive(PartialEq)]
    enum In {
        None,
        Models,
        Model,
        Modelset,
        Skeleton,
        Class,
    }
    let mut ctx = In::None;
    for t in tags(xml) {
        match (t.name, &ctx) {
            ("models", _) => {
                sp.sprite_angles = get(&t, "numSpriteAngles").and_then(|v| v.parse().ok()).unwrap_or(0);
                ctx = In::Models;
            }
            ("/models", _) => ctx = In::None,
            ("model", In::Models) => {
                sp.sprites_per_model = get(&t, "spritesPerTex").and_then(|v| v.parse().ok()).unwrap_or(sp.sprites_per_model);
                sp.models.push(SpectatorModel { name: get(&t, "name").unwrap_or_default(), texture: String::new() });
                if !t.empty {
                    ctx = In::Model;
                }
            }
            ("texture", In::Model) => {
                if let Some(m) = sp.models.last_mut() {
                    m.texture = get(&t, "name").unwrap_or_default();
                }
            }
            ("/model", In::Model) => ctx = In::Models,
            ("modelset", In::None) => {
                sp.modelsets.push((get(&t, "name").unwrap_or_default(), Vec::new()));
                ctx = In::Modelset;
            }
            ("model", In::Modelset) => {
                if let Some(s) = sp.modelsets.last_mut() {
                    s.1.push(get(&t, "name").unwrap_or_default());
                }
            }
            ("/modelset", In::Modelset) => ctx = In::None,
            ("skeleton", In::None) => {
                sp.skeletons.push(Skeleton {
                    name: get(&t, "name").unwrap_or_default(),
                    suffix: get(&t, "suffix").unwrap_or_default(),
                    modelset: String::new(),
                });
                ctx = In::Skeleton;
            }
            ("modelset", In::Skeleton) => {
                if let Some(s) = sp.skeletons.last_mut() {
                    s.modelset = get(&t, "name").unwrap_or_default();
                }
            }
            ("/skeleton", In::Skeleton) => ctx = In::None,
            ("crowdclass", In::None) => {
                sp.classes.push(CrowdClass {
                    id: get(&t, "id").and_then(|v| v.parse().ok()).unwrap_or(0),
                    name: get(&t, "name").unwrap_or_default(),
                    bb_offset_y: get(&t, "BBoffsetY").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                    ..Default::default()
                });
                ctx = In::Class;
            }
            (n @ ("skeleton" | "modelset" | "idle" | "cheer"), In::Class) => {
                let (Some(c), Some(v)) = (sp.classes.last_mut(), get(&t, "name")) else { continue };
                match n {
                    "skeleton" => c.skeleton = v,
                    "modelset" => c.modelset = v,
                    "idle" => c.idle.push(v),
                    _ => c.cheer.push(v),
                }
            }
            ("/crowdclass", In::Class) => ctx = In::None,
            _ => {}
        }
    }
    sp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_comments_and_classes() {
        let x = r#"<spectators><models Version="2" numSpriteAngles="10">
            <model name="A" spritesPerTex="9"><texture name="TA" /></model>
            <!-- <model name="X"><texture name="TX" /></model> -->
            <model name="B" spritesPerTex="9"><texture name="TB" /></model></models>
            <modelset name="Default"><model name="A" /><!-- <model name="B" /> --></modelset>
            <skeleton Version="2" name="Sitting" suffix="_Sit" subdir="sit"><animation name="S" filename="S" /><modelset name="Default" /></skeleton>
            <crowdclass id="021" name="ThreeSat" BBoffsetY="-1.0"><skeleton name="Sitting" /><modelset name="Default" /><idle name="S" weight="1.0"/></crowdclass>
            </spectators>"#;
        let s = parse_spectators_xml(x);
        assert_eq!(s.models.iter().map(|m| (m.name.as_str(), m.texture.as_str())).collect::<Vec<_>>(), [("A", "TA"), ("B", "TB")]);
        assert_eq!(s.modelset("Default"), ["A".to_string()]);
        assert_eq!(s.skeleton("Sitting").unwrap().suffix, "_Sit");
        let c = s.class(20).unwrap();
        assert_eq!((c.name.as_str(), c.skeleton.as_str(), c.bb_offset_y, c.idle.len()), ("ThreeSat", "Sitting", -1.0, 1));
        assert_eq!((s.sprite_angles, s.sprites_per_model), (10, 9));
    }

    /// Against the user's disc (skips without `disc/` or `FH1_DISC`): every spectator model parses, bone
    /// weights sum to 255 and bone indices stay inside the 19-bone skeletons.
    #[test]
    fn disc_skinbins() {
        let disc = std::env::var("FH1_DISC").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../disc").into());
        let Ok(mut ar) = crate::zip::Archive::open(std::path::Path::new(&disc).join("media/Spectators.zip")) else { return };
        let mut n = 0;
        for e in ar.entries.clone().iter().filter(|e| e.name.ends_with(".skinbin")) {
            let m = parse_skinbin(&ar.read(e).unwrap()).unwrap_or_else(|err| panic!("{}: {err}", e.name));
            assert_eq!(m.lods.len(), 4, "{}", e.name);
            assert!(m.lods.iter().all(|r| r.len() % 3 == 0), "{}", e.name);
            for v in &m.vertices {
                assert_eq!(v.weights[0] as u32 + v.weights[1] as u32, 255, "{}", e.name);
                assert!(v.bones.iter().all(|&b| b < 19), "{}: bone {:?}", e.name, v.bones);
            }
            n += 1;
        }
        assert_eq!(n, 40);
    }

    /// Against the user's disc: every type-3 object parses (walker paths included, the walk ending at the
    /// file end), arc-length tables increase and stay within the stored length, and stepping along a path
    /// with [`WalkerPath::point_at`] moves about as far as the arc length says.
    #[test]
    fn disc_crowds() {
        let disc = std::env::var("FH1_DISC").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../disc").into());
        let Ok(mut ar) = crate::zip::Archive::open(std::path::Path::new(&disc).join("media/tracks/colorado/bin.zip")) else { return };
        let (mut objects, mut spectators, mut paths, mut knots) = (0, 0, 0, 0);
        let mut seen = std::collections::HashSet::new();
        for e in ar.entries.clone() {
            let n = e.name.to_ascii_lowercase();
            if !n.ends_with(".pgeo") || !seen.insert(n.clone()) {
                continue;
            }
            let d = ar.read(&e).unwrap();
            if pgeo_type(&d) != Some(3) {
                continue;
            }
            let c = parse(&d).unwrap_or_else(|err| panic!("{n}: {err}"));
            objects += 1;
            spectators += c.spectators.len();
            for p in &c.paths {
                paths += 1;
                knots += p.knots.len();
                let all: Vec<f32> = p.knots.iter().flat_map(|k| k.distances).collect();
                assert!(all.windows(2).all(|w| w[1] >= w[0]), "{n}: arc lengths not increasing");
                assert!(all.last().is_some_and(|&l| l <= p.length + 1e-3), "{n}: arc length past the path length");
                // Chord sum over 0.5 m steps ~ arc length (within 5%).
                let (mut prev, mut walked) = (p.point_at(0.0).unwrap().0, 0.0f32);
                let steps = (p.length / 0.5) as usize;
                for i in 1..steps {
                    let q = p.point_at(i as f32 * 0.5).unwrap().0;
                    walked += ((q[0] - prev[0]).powi(2) + (q[1] - prev[1]).powi(2) + (q[2] - prev[2]).powi(2)).sqrt();
                    prev = q;
                }
                let expect = (steps - 1) as f32 * 0.5;
                assert!((walked - expect).abs() <= expect * 0.05 + 1.0, "{n}: walked {walked} m over {expect} m of arc length");
            }
        }
        assert_eq!((objects, spectators, paths, knots), (1783, 149_126, 247, 5_548));
    }

    #[test]
    fn crowd_record() {
        // Named object, name "crowd_x" (8 bytes with NUL) at 0x60 -> block at 0x70.
        let mut d = vec![0u8; 0x70];
        d[..4].copy_from_slice(b"OEGP");
        d[0x30..0x34].copy_from_slice(&3u32.to_be_bytes());
        d[0x3C..0x40].copy_from_slice(&8u32.to_be_bytes());
        d[0x60..0x68].copy_from_slice(b"crowd_x\0");
        let mut blk = vec![0u8; 0x40];
        for (i, v) in [0.0f32, 0.0, 0.0, 0.0, 10.0, 2.0, 20.0, 0.0].iter().enumerate() {
            blk[i * 4..i * 4 + 4].copy_from_slice(&v.to_bits().to_be_bytes());
        }
        blk[0x20..0x24].copy_from_slice(&1u32.to_be_bytes());
        d.extend_from_slice(&blk);
        d.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0x80, 0x00, 64, 1, 0, 0, 0, 0]);
        let c = parse(&d).unwrap();
        assert_eq!((c.name.as_str(), c.named, c.spectators.len()), ("crowd_x", true, 1));
        let s = c.spectators[0];
        assert_eq!((s.heading, s.class), (64, 1));
        assert!((s.position[0] - 10.0).abs() < 1e-4 && s.position[1].abs() < 1e-4 && (s.position[2] - 10.0).abs() < 1e-3);
    }
}
