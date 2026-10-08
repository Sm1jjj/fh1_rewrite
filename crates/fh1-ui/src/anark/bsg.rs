//! `.bsg` (scene graph): `u16 0x0104, u32 0, u16 0x18, "FGBkranA"`, eight header words
//! `2, 2, 0, 0, 8, n_nodes, n_resources, 96·n_nodes`, the 96-byte nodes, then `u32 5·n_resources`
//! and the 5-byte resources. VERIFIED on all 205 files (exact EOF).
//!
//! Baked values are not authoritative for animated / slide-dependent properties: prefer bgf S1 +
//! slides and fall back to these only where the bgf has no value (see `docs/UI.md`).

use crate::reader::Reader;
use crate::{need, Error, Result};

pub const MAGIC: &[u8; 8] = b"FGBkranA";

#[derive(Debug, Clone, PartialEq)]
pub struct Bsg {
    pub tag: u16,
    pub header_word: u32,
    /// `2, 2, 0, 0, 8, n_nodes, n_resources, node_bytes`.
    pub words: [u32; 8],
    pub nodes: Vec<Node>,
    pub resources: Vec<Resource>,
}

/// One scene node (all fbf records except materials and images).
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Node index, < own index; -1 = root.
    pub parent: i32,
    pub id: u32,
    /// 1 group/component, 2 model, 3 camera, 4 light, 5 text, 8 layer, 9 custom.
    pub kind: u8,
    pub pad: [u8; 3],
    pub position: [f32; 3],
    /// Radians.
    pub rotation: [f32; 3],
    pub scale: [f32; 3],
    /// Always 0.
    pub pivot: [f32; 3],
    /// Text/groups: min (1,1,1), max (-1,-1,-1) = empty.
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    /// 0..1.
    pub opacity: f32,
    /// Index of the enclosing layer, root = 0xFF (VERIFIED). Same as `tail[0]`.
    pub layer: u8,
    /// Bytes +88..+92: layer, 4, is_layer_or_root, 0.
    pub tail: [u8; 4],
    /// 1 for most models / all text, else 0 (GUESS: renderable).
    pub flag: u32,
}

pub const NODE_GROUP: u8 = 1;
pub const NODE_MODEL: u8 = 2;
pub const NODE_CAMERA: u8 = 3;
pub const NODE_LIGHT: u8 = 4;
pub const NODE_TEXT: u8 = 5;
pub const NODE_LAYER: u8 = 8;
pub const NODE_CUSTOM: u8 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resource {
    pub id: u32,
    /// 6 material, 7 image.
    pub kind: u8,
}

impl Bsg {
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.get(8..16) != Some(MAGIC.as_slice()) {
            return Err(Error::BadMagic("FGBkranA"));
        }
        let mut r = Reader::new(d, 0);
        let tag = r.u16("bsg header")?;
        let header_word = r.u32("bsg header")?;
        r.o = 0x10;
        let words: [u32; 8] = r.u32s("bsg header")?;
        need(words[..5] == [2, 2, 0, 0, 8], || format!("bsg header words {:?}", &words[..5]))?;
        let (n_nodes, n_res) = (words[5], words[6]);
        need(words[7] as u64 == 96 * n_nodes as u64, || "bsg node bytes".into())?;
        let mut nodes = Vec::new();
        for _ in 0..n_nodes {
            nodes.push(node(&mut r)?);
        }
        let rb = r.u32("bsg resources")?;
        need(rb as u64 == 5 * n_res as u64, || "bsg resource bytes".into())?;
        let mut resources = Vec::new();
        for _ in 0..n_res {
            resources.push(Resource { id: r.u32("bsg resource")?, kind: r.u8("bsg resource")? });
        }
        need(r.o == d.len(), || format!("bsg: {} trailing bytes", d.len() - r.o))?;
        Ok(Self { tag, header_word, words, nodes, resources })
    }
}

fn node(r: &mut Reader) -> Result<Node> {
    const W: &str = "bsg node";
    let parent = r.i32(W)?;
    let id = r.u32(W)?;
    let kind = r.u8(W)?;
    let pad = r.array(W)?;
    let position = r.f32s(W)?;
    let rotation = r.f32s(W)?;
    let scale = r.f32s(W)?;
    let pivot = r.f32s(W)?;
    let bbox_min = r.f32s(W)?;
    let bbox_max = r.f32s(W)?;
    let opacity = r.f32(W)?;
    let tail: [u8; 4] = r.array(W)?;
    let flag = r.u32(W)?;
    Ok(Node {
        parent, id, kind, pad, position, rotation, scale, pivot, bbox_min, bbox_max, opacity,
        layer: tail[0], tail, flag,
    })
}
