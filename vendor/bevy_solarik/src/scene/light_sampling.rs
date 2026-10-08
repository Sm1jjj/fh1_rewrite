//! CPU alias distribution and geometry for light flux estimates.

use bevy_math::{Mat4, Vec3};
use bevy_mesh::{Mesh, VertexAttributeValues};
use core::f32::consts::PI;

/// One Walker alias bin; probability is the marginal PMF, not the threshold.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct AliasEntry {
    pub threshold: f32,
    pub alias: u32,
    pub probability: f32,
}

/// O(N) construction, O(1) sampling. Invalid power is treated as zero;
/// an entirely dark list falls back to uniform. Normalize by the maximum
/// first so finite weights cannot overflow their sum.
pub(super) fn build_alias_table(weights: &[f64]) -> Vec<AliasEntry> {
    let maximum = weights
        .iter()
        .copied()
        .filter(|w| w.is_finite() && *w > 0.0)
        .fold(0.0, f64::max);
    let normalized: Vec<_> = weights
        .iter()
        .map(|&w| {
            if maximum == 0.0 {
                1.0
            } else if w.is_finite() && w > 0.0 {
                (w / maximum).max(1e-30)
            } else {
                0.0
            }
        })
        .collect();
    let sum: f64 = normalized.iter().sum();
    let n = weights.len();
    let mut table: Vec<_> = normalized
        .iter()
        .enumerate()
        .map(|(i, &w)| AliasEntry {
            threshold: 1.0,
            alias: i as u32,
            probability: (w / sum) as f32,
        })
        .collect();
    let mut scaled: Vec<_> = normalized.iter().map(|w| w / sum * n as f64).collect();
    let (mut small, mut large): (Vec<_>, Vec<_>) = (0..n).partition(|&i| scaled[i] < 1.0);
    while !small.is_empty() && !large.is_empty() {
        let s = small.pop().expect("small is nonempty");
        let l = large.pop().expect("large is nonempty");
        table[s].threshold = scaled[s].clamp(0.0, 1.0) as f32;
        table[s].alias = l as u32;
        scaled[l] = scaled[l] + scaled[s] - 1.0;
        if scaled[l] < 1.0 {
            small.push(l);
        } else {
            large.push(l);
        }
    }
    table
}

/// Retain edges at mesh upload, when CPU positions are still available.
/// Translation is irrelevant to area; two transformed edges handle shear,
/// reflection and nonuniform scale without a determinant inverse.
pub(super) fn triangle_edges(mesh: &Mesh) -> Vec<[Vec3; 2]> {
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        return Vec::new();
    };
    let Some(indices) = mesh.indices() else {
        return Vec::new();
    };
    let indices: Vec<_> = indices.iter().collect();
    indices
        .chunks_exact(3)
        .filter_map(|triangle| {
            let a = Vec3::from_array(*positions.get(triangle[0])?);
            let b = Vec3::from_array(*positions.get(triangle[1])?);
            let c = Vec3::from_array(*positions.get(triangle[2])?);
            Some([b - a, c - a])
        })
        .collect()
}

pub(super) fn world_area(edges: &[[Vec3; 2]], transform: Mat4) -> f64 {
    edges
        .iter()
        .map(|edge| {
            let a = transform.transform_vector3(edge[0]);
            let b = transform.transform_vector3(edge[1]);
            f64::from(a.cross(b).length()) * 0.5
        })
        .sum()
}

pub(super) fn luminance(color: Vec3) -> f64 {
    f64::from(color.dot(Vec3::new(0.2126, 0.7152, 0.0722)).max(0.0))
}

/// Bevy extracts both point and spot intensities in candela. Integrating
/// its squared ramp over cos(theta) gives ramp width / 3 plus the inner cap.
pub(super) fn local_flux(color: Vec3, intensity: f32, spot: Option<(f32, f32)>) -> f64 {
    let solid_angle = match spot {
        Some((inner, outer)) => {
            let inner_cos = inner.cos();
            let outer_cos = outer.cos();
            2.0 * PI * (1.0 - inner_cos + (inner_cos - outer_cos) / 3.0)
        }
        None => 4.0 * PI,
    };
    luminance(color) * f64::from(intensity.max(0.0)) * f64::from(solid_angle.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_mesh::Indices;
    use bevy_render::render_resource::PrimitiveTopology;

    fn assert_distribution(weights: &[f64], expected: &[f64]) {
        let table = build_alias_table(weights);
        assert_eq!(table.len(), expected.len());
        let mut actual = vec![0.0; table.len()];
        for (i, entry) in table.iter().enumerate() {
            assert!((0.0..=1.0).contains(&entry.threshold));
            assert!((entry.alias as usize) < table.len());
            actual[i] += f64::from(entry.threshold) / table.len() as f64;
            actual[entry.alias as usize] += (1.0 - f64::from(entry.threshold)) / table.len() as f64;
        }
        for ((actual, expected), entry) in actual.iter().zip(expected).zip(&table) {
            assert!((actual - expected).abs() < 1e-7, "{actual} != {expected}");
            assert!((f64::from(entry.probability) - expected).abs() < 1e-7);
        }
    }

    #[test]
    fn alias_table_reconstructs_flux_proportions() {
        assert_distribution(&[1.0, 3.0], &[0.25, 0.75]);
        assert_distribution(&[1.0, 2.0, 3.0, 4.0], &[0.1, 0.2, 0.3, 0.4]);
        assert_distribution(&[0.0, 9.0, 1.0], &[0.0, 0.9, 0.1]);
        assert_distribution(&[7.0], &[1.0]);
    }

    #[test]
    fn empty_and_dark_distributions_are_safe() {
        assert!(build_alias_table(&[]).is_empty());
        assert_distribution(&[0.0, 0.0], &[0.5, 0.5]);
        assert_distribution(&[f64::NAN, -2.0, f64::INFINITY], &[1.0 / 3.0; 3]);
        assert_distribution(&[f64::NAN, 2.0], &[0.0, 1.0]);
    }

    #[test]
    fn extreme_weights_do_not_overflow_normalization() {
        assert_distribution(&[f64::MAX, f64::MAX], &[0.5, 0.5]);
        assert_distribution(&[1e-200, 3e-200], &[0.25, 0.75]);
    }

    #[test]
    fn two_light_estimator_agrees_with_uniform() {
        let values = [2.0f64, 11.0];
        for weights in [[1.0, 1.0], [1.0, 9.0]] {
            let table = build_alias_table(&weights);
            let mut estimate = 0.0;
            for (i, entry) in table.iter().enumerate() {
                let alias = entry.alias as usize;
                estimate += (f64::from(entry.threshold) * values[i]
                    / f64::from(table[i].probability)
                    + (1.0 - f64::from(entry.threshold)) * values[alias]
                        / f64::from(table[alias].probability))
                    / table.len() as f64;
            }
            assert!((estimate - values.iter().sum::<f64>()).abs() < 1e-5);
        }
    }

    #[test]
    fn area_handles_nonuniform_scale_and_reflection() {
        let edges = [[Vec3::X, Vec3::Y], [Vec3::Y, Vec3::Z]];
        assert!((world_area(&edges, Mat4::IDENTITY) - 1.0).abs() < 1e-6);
        assert!(
            (world_area(&edges, Mat4::from_scale(Vec3::new(-2.0, 3.0, 4.0))) - 9.0).abs() < 1e-6
        );
        assert_eq!(world_area(&edges, Mat4::from_scale(Vec3::ZERO)), 0.0);
    }

    #[test]
    fn mesh_edges_follow_indexed_triangles() {
        let mesh = Mesh::new(PrimitiveTopology::TriangleList, Default::default())
            .with_inserted_attribute(
                Mesh::ATTRIBUTE_POSITION,
                vec![[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 3.0, 0.0]],
            )
            .with_inserted_indices(Indices::U32(vec![0, 1, 2]));
        let edges = triangle_edges(&mesh);
        assert_eq!(edges.len(), 1);
        assert!((world_area(&edges, Mat4::IDENTITY) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn point_and_spot_flux_integrate_raster_cone() {
        assert!((local_flux(Vec3::ONE, 800.0 / (4.0 * PI), None) - 800.0).abs() < 1e-3);
        let inner = 0.3f32;
        let outer = 0.5f32;
        let solid_angle = 2.0 * PI * (1.0 - inner.cos() + (inner.cos() - outer.cos()) / 3.0);
        assert!(
            (local_flux(Vec3::ONE, 1.0, Some((inner, outer))) - f64::from(solid_angle)).abs()
                < 1e-6
        );
        assert_eq!(local_flux(Vec3::ZERO, 100.0, None), 0.0);
    }
}
