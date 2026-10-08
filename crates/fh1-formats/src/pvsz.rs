//! Colorado PVS zone files (`bin.zip` `__R00Znnnnn.pvsz`, one per road-network hex cell; spec in
//! docs/WORLD_LOD.md "`.pvsz` grammar").
//!
//! Layout (all big-endian):
//! - head: `u32 n` + n x u32 `.pvs` 18-byte draw-record indices (high bit = near/far set);
//! - `u32 m` + m x u16 model list;
//! - `u32 c` + c x 16-byte object records `{u8 sub, u8 0, u16 G, u16 0, u16, u32 kind, u32}`
//!   (kind 2 = `.pgeo` `__R00G%05d`, docs/PROPS.md);
//! - further lists, not decoded (`middle`);
//! - the **instance section**: `u32 n` (= head n) + n entries, entry i belonging to head record i.
//!
//! Instance entry (VERIFIED: all 1,434 Colorado zones parse to exactly the end of the file; the
//! start is unique in 1,433, and in zone 1163 an earlier byte list of the same count also frames, so
//! the last start that frames to EOF is taken):
//! ```text
//! +0   3 x f16   distances (NaN = none)
//! +6   3 x f32   position (collision space)
//! +18  9 x f16   axes rows (scale included)
//! +36  f32       (0 on world-space models; 0.5..4 on templates; meaning open)
//! +40  12 bytes  zero in every Colorado entry
//! +52  u8        block flag (0 / 1)
//! if 1:
//! +53  i32       object type (-1 = none; else 0..~190, INFERRED the physics object type:
//!                CollObjs smashables and GameObjs carry it)
//! +57  u8 k      + k u8 activity ids (activity 0 = free roam, as in `.pgeo` groups)
//!      char[32]  name, NUL padded ("" / "FR22_03" / "flyer_066" / "speed_camera_10_left")
//! ```
//! Entry size: 53 bytes, or 53 + 5 + k + 32.
//!
//! Forza Horizon 2 (360) zones store the activity ids as **u16** (entry 53 + 5 + 2k + 32; VERIFIED: all 2,798 Anthem
//! zones frame to EOF with a unique start, docs/FH2_RECON.md). [`parse`] tries the FH1 u8 grammar first.

use crate::Error;

/// One parsed zone file.
#[derive(Debug, Clone)]
pub struct Zone {
    /// `.pvs` 18-byte draw-record index per head entry, with the near/far bit (bit 31) kept.
    pub head: Vec<u32>,
    /// The u16 model list after the head.
    pub models: Vec<u16>,
    /// 16-byte object records (`.pgeo` and other kinds).
    pub objects: Vec<[u8; 16]>,
    /// Byte range of the undecoded lists between the object records and the instance section.
    pub middle: std::ops::Range<usize>,
    /// One instance per head entry.
    pub instances: Vec<Instance>,
}

/// The transform and conditions of one head record in this zone.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    /// `.pvs` record index (head entry with the high bit cleared).
    pub record: u32,
    /// Three f16 distances (NaN = none); read as LOD1 / LOD2 / cull (UNVERIFIED).
    pub distances: [f32; 3],
    /// Collision-space position.
    pub position: [f32; 3],
    /// Axes rows (scale included), collision space: the game's own draw matrix of the (right-handed) template,
    /// handedness flip included (det < 0). See [`Instance::placement_axes`].
    pub axes: [[f32; 3]; 3],
    /// The f32 at +36.
    pub radius: f32,
    /// Present when the block flag is 1.
    pub block: Option<Block>,
}

/// The optional instance block.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// -1 = none; otherwise an object type index (INFERRED).
    pub object_type: i32,
    /// Activity ids (0 = free roam). FH2 ids above 255 are saturated to 255 here (still "not free roam");
    /// the exact ids are in [`Block::activities_wide`].
    pub activities: Vec<u8>,
    /// FH2 zones: the u16 activity ids as stored. Empty for FH1 zones.
    pub activities_wide: Vec<u16>,
    /// Name (event node, GameObj id), empty for most.
    pub name: String,
}

impl Instance {
    /// Positioned away from the origin (world-space models sit at 0 with axes diag(1, 1, -1)).
    pub fn is_placed(&self) -> bool {
        self.position.iter().map(|v| v.abs()).sum::<f32>() >= 0.01
    }

    /// The axes in the convention of the other placement sources (`.pgeo`, CollObjs/GameObjs XML, anim scenes:
    /// det > 0, the template's handedness flip applied by the consumer as S·P·S in
    /// `props::Placement::engine_matrix`). `.pvsz` axes already hold the flip: they are exactly the game's
    /// `WorldMatrix` (c140, transposed) of every zone-placed draw (VERIFIED: 136 of 136 instance matrices in a
    /// RenderDoc capture of the festival, docs/GPU_CAPTURE.md; world-space models sit at diag(1, 1, -1) like the
    /// game's tile matrix). So the Z row is negated here, and S·P·S then gives S·P. Without this, every zone
    /// template drew mirrored along its local Z (the festival stage signs read backwards).
    pub fn placement_axes(&self) -> [[f32; 3]; 3] {
        let [x, y, z] = self.axes;
        [x, y, [-z[0], -z[1], -z[2]]]
    }

    /// Shown in free roam: no block, or a block listing activity 0 without a name (INFERRED: named
    /// entries are event nodes / GameObjs).
    pub fn in_free_roam(&self) -> bool {
        match &self.block {
            None => true,
            Some(b) => b.activities.contains(&0) && b.name.is_empty(),
        }
    }
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
}

fn f16(d: &[u8], o: usize) -> f32 {
    let h = u16::from_be_bytes([d[o], d[o + 1]]);
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1F) as i32;
    let m = (h & 0x3FF) as f32;
    match e {
        0 => s * m * 2f32.powi(-24),
        31 if m != 0.0 => f32::NAN,
        31 => s * f32::INFINITY,
        _ => s * (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Walks n entries from `p`; `Some` only if they end exactly at the file end.
/// `wide` = FH2's u16 activity ids.
fn instances_at(d: &[u8], head: &[u32], mut p: usize, wide: bool) -> Option<Vec<Instance>> {
    let mut out = Vec::with_capacity(head.len());
    for &h in head {
        let flag = *d.get(p + 52)?;
        let (block, next) = match flag {
            0 => (None, p + 53),
            1 => {
                let object_type = u32_at(d, p + 53)? as i32;
                let k = *d.get(p + 57)? as usize;
                let ids = if wide { 2 * k } else { k };
                let raw_ids = d.get(p + 58..p + 58 + ids)?;
                let (activities, activities_wide) = if wide {
                    let w: Vec<u16> = raw_ids.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                    (w.iter().map(|&a| a.min(255) as u8).collect(), w)
                } else {
                    (raw_ids.to_vec(), Vec::new())
                };
                let raw = d.get(p + 58 + ids..p + 90 + ids)?;
                let name = String::from_utf8_lossy(&raw[..raw.iter().position(|&c| c == 0).unwrap_or(32)]).into_owned();
                (Some(Block { object_type, activities, activities_wide, name }), p + 90 + ids)
            }
            _ => return None,
        };
        let pos = |k: usize| f32::from_bits(u32_at(d, p + 6 + 4 * k).unwrap());
        let a = |k: usize| f16(d, p + 18 + 2 * k);
        out.push(Instance {
            record: h & 0x7FFF_FFFF,
            distances: [f16(d, p), f16(d, p + 2), f16(d, p + 4)],
            position: [pos(0), pos(1), pos(2)],
            axes: [[a(0), a(1), a(2)], [a(3), a(4), a(5)], [a(6), a(7), a(8)]],
            radius: f32::from_bits(u32_at(d, p + 36)?),
            block,
        });
        p = next;
    }
    (p == d.len()).then_some(out)
}

/// Parses a `.pvsz` file.
pub fn parse(d: &[u8]) -> Result<Zone, Error> {
    let n =u32_at(d, 0).ok_or(Error::Truncated("pvsz"))? as usize;
    let mut p = 4;
    let head: Vec<u32> = (0..n).map(|i| u32_at(d, p + 4 * i)).collect::<Option<_>>().ok_or(Error::Truncated("pvsz head"))?;
    p += 4 * n;
    let m = u32_at(d, p).ok_or(Error::Truncated("pvsz models"))? as usize;
    let models: Vec<u16> = d.get(p + 4..p + 4 + 2 * m).ok_or(Error::Truncated("pvsz models"))?.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    p += 4 + 2 * m;
    let c = u32_at(d, p).ok_or(Error::Truncated("pvsz objects"))? as usize;
    let objects = d.get(p + 4..p + 4 + 16 * c).ok_or(Error::Truncated("pvsz objects"))?.as_chunks::<16>().0.to_vec();
    p += 4 + 16 * c;
    // The instance section: the last `u32 n` whose n entries end exactly at the file end.
    let key = (n as u32).to_be_bytes();
    let find = |wide: bool| (p..d.len().saturating_sub(4)).rev().filter(|&o| d[o..o + 4] == key).find_map(|o| instances_at(d, &head, o + 4, wide).map(|inst| (o, inst)));
    // FH1 grammar first (all Colorado zones), then FH2's u16 activity ids.
    let found = find(false).or_else(|| find(true));
    let (start, instances) = found.ok_or(Error::Truncated("pvsz instance section"))?;
    Ok(Zone { head, models, objects, middle: p..start, instances })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the user's disc: every Colorado zone parses; the festival zone maps the round marquee.
    #[test]
    fn colorado_zones() {
        let disc = std::env::var_os("FH1_DISC").map(std::path::PathBuf::from).unwrap_or_else(|| "../../disc".into());
        let Ok(mut ar) = crate::zip::Archive::open(disc.join("media/tracks/colorado/bin.zip")) else {
            return eprintln!("no disc, skipping");
        };
        let mut seen = std::collections::HashSet::new();
        let zones: Vec<_> = ar.entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".pvsz") && seen.insert(e.name.to_ascii_lowercase())).cloned().collect();
        assert_eq!(zones.len(), 1434);
        let (mut blocks, mut mirrored) = (0, 0);
        for z in &zones {
            let zone = parse(&ar.read(z).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", z.name));
            assert_eq!(zone.instances.len(), zone.head.len());
            blocks += zone.instances.iter().filter(|i| i.block.is_some()).count();
            // The game draws templates with a det < 0 matrix (the handedness flip), so in the other sources'
            // convention all but 7 of 611,192 zone placements are unmirrored (VERIFIED 2026-10-04).
            for i in zone.instances.iter().filter(|i| i.is_placed()) {
                let [x, y, w] = i.placement_axes();
                let det = x[0] * (y[1] * w[2] - y[2] * w[1]) - x[1] * (y[0] * w[2] - y[2] * w[0]) + x[2] * (y[0] * w[1] - y[1] * w[0]);
                mirrored += (det < 0.0) as usize;
            }
        }
        assert!(blocks > 300_000, "{blocks}");
        assert!(mirrored < 50, "{mirrored} zone placements mirrored");
        let z = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case("__R00Z00655.pvsz")).cloned().unwrap();
        let zone = parse(&ar.read(&z).unwrap()).unwrap();
        assert_eq!(zone.instances.len(), 1372);
        let (a, b) = (&zone.instances[7], &zone.instances[8]);
        assert_eq!(a.position, b.position);
        assert!((a.position[0] + 882.09).abs() < 0.1 && a.block.is_none());
    }

    #[test]
    fn halves() {
        assert_eq!(f16(&[0x3C, 0x00], 0), 1.0);
        assert_eq!(f16(&[0x56, 0x40], 0), 100.0);
        assert!(f16(&[0x7F, 0xFF], 0).is_nan());
    }
}
