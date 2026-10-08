//! Surface types from `media/physics.zip` → `surfaceTypes.xml`.
//!
//! `<SurfaceTypesList>` gives the names in index order (FM4's file has no list: block order instead); a collision triangle's `surface` byte
//! indexes it (verified on Colorado: GuardRail/WireFence/Invisible triangles are ~99% vertical,
//! Dirt/Grass/Gravel ~100% flat). `<SurfaceTypes>` then has one block per name with its
//! category and nested properties, e.g. `Friction/FrictionScale`.

use std::collections::BTreeMap;

use anyhow::Result;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub name: String,
    /// HardWorld, SoftWorld, GuardRail, TireWall, RumbleStrip, Car, ... (None if no block).
    pub category: Option<String>,
    /// Every numeric property of the block by path, e.g. `Friction/FrictionScale` = 0.95 or
    /// `SkidData/SmokeType@Dust` = 1. Values that aren't numbers (`NoiseType` = Simplex) are left out.
    pub params: BTreeMap<String, f32>,
}

impl Surface {
    pub fn param(&self, path: &str) -> Option<f32> {
        self.params.get(path).copied()
    }

    /// Tyre friction multiplier (1.0 on asphalt).
    pub fn friction(&self) -> f32 {
        self.param("Friction/FrictionScale").unwrap_or(1.0)
    }

    /// 0 on roads, 1 on dirt/grass/gravel.
    pub fn offroadness(&self) -> f32 {
        self.param("Friction/OffRoadness").unwrap_or(0.0)
    }

    /// The colour the developers used for this surface in their debug view.
    pub fn debug_color(&self) -> [u8; 3] {
        let c = |n: &str| self.param(&format!("DebugColor/Color{n}")).unwrap_or(255.0) as u8;
        [c("Red"), c("Green"), c("Blue")]
    }
}

pub fn parse_surface_types(xml: &str) -> Result<Vec<Surface>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut names = Vec::new();
    let mut blocks: BTreeMap<String, (Option<String>, BTreeMap<String, f32>)> = BTreeMap::new();
    // Block names in document order (FM4 has no `<SurfaceTypesList>`; its blocks are in index order).
    let mut block_order: Vec<String> = Vec::new();
    // Open element names below <Root>.
    let mut path: Vec<String> = Vec::new();
    loop {
        let (e, empty) = match reader.read_event()? {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(_) => {
                path.pop();
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let name = AsRef::<str>::as_ref(&e.name()).to_owned();
        match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [_, "SurfaceTypesList"] if name == "SurfaceType" => {
                if let Some(v) = attr(&e, "value")? {
                    names.push(v);
                }
            }
            [_, "SurfaceTypes"] => {
                block_order.push(name.clone());
                blocks.insert(name.clone(), (attr(&e, "Category")?, BTreeMap::new()));
            }
            [_, "SurfaceTypes", surface, rest @ ..] => {
                let key = rest.iter().copied().chain([name.as_str()]).collect::<Vec<_>>().join("/");
                let params = &mut blocks.get_mut(*surface).expect("block opened").1;
                for a in e.attributes() {
                    let a = a?;
                    let k = AsRef::<str>::as_ref(&a.key);
                    if let Ok(v) = a.normalized_value(XmlVersion::Implicit1_0)?.trim().parse::<f32>() {
                        let full = if k == "value" { key.clone() } else { format!("{key}@{k}") };
                        params.insert(full, v);
                    }
                }
            }
            _ => {}
        }
        if !empty {
            path.push(name);
        }
    }
    // Forza Motorsport 4 (docs/FM4_RECON.md): no list, the blocks' order is the index (INFERRED from the collision:
    // index 9 = Invisible on the walls, 6 = RumbleStrip, 2 = Grass, as in FH1's list).
    let names = if names.is_empty() { block_order } else { names };
    Ok(names
        .into_iter()
        .map(|name| {
            let (category, params) = blocks.remove(&name).unwrap_or_default();
            Surface { name, category, params }
        })
        .collect())
}

fn attr(e: &BytesStart, key: &str) -> Result<Option<String>> {
    for a in e.attributes() {
        let a = a?;
        if AsRef::<str>::as_ref(&a.key) == key {
            return Ok(Some(a.normalized_value(XmlVersion::Implicit1_0)?.into_owned()));
        }
    }
    Ok(None)
}
