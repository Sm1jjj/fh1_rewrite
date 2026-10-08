//! Anark Gameface UI scenes (Xbox 360, big-endian, byte-packed). A scene is three files sharing a
//! stem in `media/UI.zip` → `Scenes/ui4/`:
//!
//! - [`bgf`] `X.bgf`: object property table, slides (states), keyframe tracks, event handlers, strings.
//! - [`fbf`] `X.fbf`: per-object payloads (text, material, image, model, camera, light) + mesh blob.
//! - [`bsg`] `X.bsg`: node hierarchy with baked TRS / bbox / opacity, and the resource list.
//!
//! VERIFIED on the EU disc: all 230 `.bgf` and the 205 `.fbf`/`.bsg` pairs parse to EOF with the
//! header counts matching (48,710,522 bytes; the other 25 scenes ship only a `.bgf`).
//! Semantics (transforms, visibility, slide evaluation) are in `docs/UI.md`.

pub mod bgf;
pub mod bsg;
pub mod fbf;

pub use bgf::Bgf;
pub use bsg::Bsg;
pub use fbf::Fbf;

use crate::Result;

/// A typed property value. The type comes from the key's low 4 type bits (`key >> 27 & 15`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropValue {
    Int(u32),
    Float(f32),
    Bool(bool),
    /// Index into [`Bgf::strings`].
    Str(u32),
    /// A type code outside 1/3/4/5 (none on the disc); raw value.
    Other(u32),
}

/// One `(key, value)` pair as stored. `key` keeps its type bits (`key >> 27`: type + 16 = dynamic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prop {
    pub key: u32,
    pub raw: u32,
}

impl Prop {
    /// Type code including the dynamic flag (0..=31).
    pub fn type_bits(&self) -> u32 {
        self.key >> 27
    }

    /// Base type (1 int, 3 float, 4 bool, 5 string).
    pub fn base_type(&self) -> u32 {
        (self.key >> 27) & 15
    }

    /// True for dynamic/custom attributes (type + 16). Meaning GUESS.
    pub fn is_dynamic(&self) -> bool {
        self.key >> 27 & 16 != 0
    }

    /// 27-bit name hash (compare with [`crate::names`]).
    pub fn name_hash(&self) -> u32 {
        self.key & crate::hash::MASK27
    }

    pub fn name(&self) -> Option<&'static str> {
        crate::names::prop_name(self.key)
    }

    pub fn value(&self) -> PropValue {
        typed(self.key, self.raw)
    }
}

/// Interpret `raw` by the type bits of `key`.
pub fn typed(key: u32, raw: u32) -> PropValue {
    match (key >> 27) & 15 {
        1 => PropValue::Int(raw),
        3 => PropValue::Float(f32::from_bits(raw)),
        4 => PropValue::Bool(raw != 0),
        5 => PropValue::Str(raw),
        _ => PropValue::Other(raw),
    }
}

/// One parsed scene. `fbf`/`bsg` are absent for the 25 bgf-only scenes.
#[derive(Debug, Clone)]
pub struct Scene {
    pub bgf: Bgf,
    pub fbf: Option<Fbf>,
    pub bsg: Option<Bsg>,
}

impl Scene {
    pub fn load(bgf: &[u8], fbf: Option<&[u8]>, bsg: Option<&[u8]>) -> Result<Self> {
        Ok(Self {
            bgf: Bgf::parse(bgf)?,
            fbf: fbf.map(Fbf::parse).transpose()?,
            bsg: bsg.map(Bsg::parse).transpose()?,
        })
    }

    /// Bgf string-table entry (for [`PropValue::Str`]).
    pub fn string(&self, index: u32) -> Option<&str> {
        self.bgf.strings.get(index as usize).map(String::as_str)
    }
}
