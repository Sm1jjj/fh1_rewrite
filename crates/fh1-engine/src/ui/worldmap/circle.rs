//! Cheap shared primitives for both maps: a ring + faint fill mesh (the barn rumour's hint circle) and a generated
//! badge texture (tick / padlock on a white disc, tinted by the material colour; glyph black).

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use super::state::Badge;

const SEGMENTS: usize = 72;

/// Which plane the circle lies in: `Xy` (world map) or `Xz` (minimap, y up).
#[derive(Clone, Copy)]
pub enum Plane {
    Xy,
    Xz,
}

/// A disc of `radius` (fill alpha `fill_a`) with a ring of `width` at its rim (alpha 0.9), colour as vertex colours
/// of `rgb` (the material colour should be white). Centred on the origin.
pub fn ring_mesh(radius: f32, width: f32, plane: Plane, rgb: [f32; 3], fill_a: f32) -> Mesh {
    let inner = (radius - width).max(radius * 0.5);
    let p = |r: f32, a: f32| match plane {
        Plane::Xy => [r * a.cos(), r * a.sin(), 0.0],
        Plane::Xz => [r * a.cos(), 0.0, r * a.sin()],
    };
    let n = match plane {
        Plane::Xy => [0.0, 0.0, 1.0],
        Plane::Xz => [0.0, 1.0, 0.0],
    };
    let mut pos = vec![p(0.0, 0.0)];
    let mut col = vec![[rgb[0], rgb[1], rgb[2], fill_a]];
    // Per segment: fill rim, ring inner, ring outer.
    for i in 0..SEGMENTS {
        let a = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        pos.push(p(inner, a));
        col.push([rgb[0], rgb[1], rgb[2], fill_a]);
        pos.push(p(inner, a));
        col.push([rgb[0], rgb[1], rgb[2], 0.9]);
        pos.push(p(radius, a));
        col.push([rgb[0], rgb[1], rgb[2], 0.9]);
    }
    let mut idx: Vec<u32> = Vec::new();
    for i in 0..SEGMENTS {
        let j = (i + 1) % SEGMENTS;
        let (a, b) = (1 + 3 * i as u32, 1 + 3 * j as u32);
        idx.extend([0, a, b]);
        idx.extend([a + 1, a + 2, b + 2, a + 1, b + 2, b + 1]);
    }
    // Both windings, so the winding / plane orientation never matters.
    let back: Vec<u32> = idx.chunks(3).flat_map(|t| [t[0], t[2], t[1]]).collect();
    idx.extend(back);
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos.clone())
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![n; pos.len()])
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, col)
        .with_inserted_indices(Indices::U32(idx))
}

/// A unit quad (side 1, centred, XY plane) with UV 0..1.
pub fn unit_quad() -> Mesh {
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[-0.5, 0.5, 0.0], [0.5, 0.5, 0.0], [0.5, -0.5, 0.0], [-0.5, -0.5, 0.0]])
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0, 0.0, 1.0]; 4])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
        .with_inserted_indices(Indices::U32(vec![0, 2, 1, 0, 3, 2]))
}

const PX: usize = 64;

/// Is (x, y) (0..1, y down) on the glyph? A tick for Tick / Medal, a padlock for Lock.
fn glyph(b: Badge, x: f32, y: f32) -> bool {
    let seg = |ax: f32, ay: f32, bx: f32, by: f32, w: f32| {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        let (px, py) = (ax + t * dx, ay + t * dy);
        ((x - px).powi(2) + (y - py).powi(2)).sqrt() < w
    };
    match b {
        Badge::Tick | Badge::Medal(_) => seg(0.27, 0.52, 0.43, 0.68, 0.06) || seg(0.43, 0.68, 0.74, 0.34, 0.06),
        Badge::Lock => {
            let body = (0.30..0.70).contains(&x) && (0.48..0.76).contains(&y);
            let d = ((x - 0.5).powi(2) + (y - 0.45).powi(2)).sqrt();
            let shackle = y < 0.48 && d > 0.12 && d < 0.19;
            body || shackle
        }
    }
}

/// RGBA image of a badge: a white disc with a dark rim and a black glyph (tint it with the material colour).
/// Non-sRGB so the minimap's gamma-space target gets the same bytes.
pub fn badge_image(b: Badge) -> Image {
    let mut data = Vec::with_capacity(PX * PX * 4);
    for j in 0..PX {
        for i in 0..PX {
            let (x, y) = ((i as f32 + 0.5) / PX as f32, (j as f32 + 0.5) / PX as f32);
            let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
            let a = ((0.5 - r) * PX as f32).clamp(0.0, 1.0);
            let v = if r > 0.44 || glyph(b, x, y) { 0u8 } else { 255 };
            data.extend([v, v, v, (a * 255.0) as u8]);
        }
    }
    Image::new(Extent3d { width: PX as u32, height: PX as u32, depth_or_array_layers: 1 }, TextureDimension::D2, data, TextureFormat::Rgba8Unorm, RenderAssetUsages::RENDER_WORLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_have_pixels() {
        let n = |b| (0..PX * PX).filter(|k| glyph(b, (k % PX) as f32 / PX as f32, (k / PX) as f32 / PX as f32)).count();
        assert!(n(Badge::Tick) > 50);
        assert!(n(Badge::Lock) > 200);
    }

    #[test]
    fn ring_indices_in_range() {
        let m = ring_mesh(100.0, 3.0, Plane::Xy, [1.0; 3], 0.1);
        let len = m.count_vertices() as u32;
        let Some(Indices::U32(i)) = m.indices() else { panic!() };
        assert!(i.iter().all(|&k| k < len));
    }
}
