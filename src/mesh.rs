//! Grid mesh generation: a regular `resolution × resolution` vertex grid over
//! the tile, heights baked into Y from the padded [`HeightField`].
//!
//! Frame: origin at the tile's north-west corner, `+X` east, `+Z` south,
//! `Y` metres above sea level. Triangles wind counter-clockwise seen from
//! `+Y`. UV `(u, v) = (X/size, Z/size)` — glTF's UV origin is top-left, which
//! matches image row 0 = north for both the imagery and the heightmap, so no
//! flip is needed.

use crate::terrain::HeightField;
use crate::{Error, Result};

/// Default vertices per edge (128 quads; 16 641 vertices, u16-indexable).
pub const DEFAULT_RESOLUTION: u32 = 129;
/// Smallest useful grid: one quad.
pub const MIN_RESOLUTION: u32 = 2;
/// Beyond 257 the grid only interpolates the 256-texel source.
pub const MAX_RESOLUTION: u32 = 257;

/// Flat vertex/index arrays ready for a GLB.
#[derive(Clone, Debug)]
pub struct Grid {
    /// Vertices per edge.
    pub resolution: u32,
    /// `[x, y, z]` metres, row-major (north row first, west first).
    pub positions: Vec<[f32; 3]>,
    /// `[u, v]` in `[0, 1]`.
    pub uvs: Vec<[f32; 2]>,
    /// Triangle list, CCW from above.
    pub indices: Vec<u32>,
    /// Per-axis minimum of `positions` (glTF requires it on POSITION).
    pub min: [f32; 3],
    /// Per-axis maximum of `positions`.
    pub max: [f32; 3],
}

impl Grid {
    /// Triangle count.
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }

    /// True when every index fits a `u16` — decides the GLB index type.
    pub fn fits_u16(&self) -> bool {
        self.positions.len() <= usize::from(u16::MAX) + 1
    }
}

/// Validate a resolution (vertices per edge).
pub fn check_resolution(resolution: u32) -> Result<()> {
    if (MIN_RESOLUTION..=MAX_RESOLUTION).contains(&resolution) {
        Ok(())
    } else {
        Err(Error::InvalidResolution(resolution))
    }
}

/// Build the grid for a tile of `size_m` metres per edge.
pub fn build_grid(field: &HeightField, size_m: f64, resolution: u32) -> Result<Grid> {
    check_resolution(resolution)?;
    let r = resolution as usize;
    let n = (r - 1) as f64; // quads per edge
    let mut positions = Vec::with_capacity(r * r);
    let mut uvs = Vec::with_capacity(r * r);
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];

    for j in 0..r {
        let v = j as f64 / n;
        let z = (v * size_m) as f32;
        for i in 0..r {
            let u = i as f64 / n;
            let x = (u * size_m) as f32;
            let y = field.sample(u, v);
            let p = [x, y, z];
            for a in 0..3 {
                min[a] = min[a].min(p[a]);
                max[a] = max[a].max(p[a]);
            }
            positions.push(p);
            uvs.push([u as f32, v as f32]);
        }
    }

    let mut indices = Vec::with_capacity((r - 1) * (r - 1) * 6);
    for j in 0..r - 1 {
        for i in 0..r - 1 {
            // a = NW, b = NE, c = SW, d = SE. CCW seen from +Y (with +Z
            // pointing south, "down" on the map): (a, c, b) and (b, c, d)
            // both have (edge1 × edge2)·Y > 0
            let a = (j * r + i) as u32;
            let b = a + 1;
            let c = a + r as u32;
            let d = c + 1;
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }

    Ok(Grid {
        resolution,
        positions,
        uvs,
        indices,
        min,
        max,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::{Side, TILE_TEXELS};

    fn flat(h: f32) -> HeightField {
        HeightField::unpadded(&vec![h; TILE_TEXELS * TILE_TEXELS])
    }

    #[test]
    fn counts_corners_and_bounds() {
        let g = build_grid(&flat(100.0), 1000.0, 5).unwrap();
        assert_eq!(g.positions.len(), 25);
        assert_eq!(g.triangles(), 32);
        assert_eq!(g.positions[0], [0.0, 100.0, 0.0]);
        assert_eq!(g.positions[24], [1000.0, 100.0, 1000.0]);
        assert_eq!(g.uvs[4], [1.0, 0.0]);
        assert_eq!(g.min, [0.0, 100.0, 0.0]);
        assert_eq!(g.max, [1000.0, 100.0, 1000.0]);
        assert!(g.fits_u16());
        assert!(!build_grid(&flat(0.0), 1.0, 257).unwrap().fits_u16());
    }

    #[test]
    fn rejects_bad_resolution() {
        assert!(build_grid(&flat(0.0), 1.0, 1).is_err());
        assert!(build_grid(&flat(0.0), 1.0, 258).is_err());
    }

    #[test]
    fn all_triangles_face_up() {
        let n = TILE_TEXELS;
        // bumpy field so the test isn't trivially flat
        let tile: Vec<f32> = (0..n * n)
            .map(|i| ((i % n) as f32 * 0.3).sin() * 50.0 + ((i / n) as f32 * 0.2).cos() * 30.0)
            .collect();
        let g = build_grid(&HeightField::unpadded(&tile), 500.0, 33).unwrap();
        for t in g.indices.chunks(3) {
            let p = |k: u32| g.positions[k as usize];
            let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
            let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let ny = e1[2] * e2[0] - e1[0] * e2[2];
            assert!(ny > 0.0, "triangle {t:?} faces down (ny={ny})");
        }
    }

    #[test]
    fn neighbours_share_boundary_vertices_exactly() {
        let n = TILE_TEXELS;
        let west: Vec<f32> = (0..n * n)
            .map(|i| (i % n) as f32 + (i / n) as f32 * 0.5)
            .collect();
        let east: Vec<f32> = (0..n * n)
            .map(|i| (n + i % n) as f32 + (i / n) as f32 * 0.5)
            .collect();
        let mut nw: [Option<Vec<f32>>; 8] = Default::default();
        nw[Side::E as usize] = Some(east.clone());
        let mut ne: [Option<Vec<f32>>; 8] = Default::default();
        ne[Side::W as usize] = Some(west.clone());
        let r = 65u32;
        let gw = build_grid(&HeightField::padded(&west, &nw), 1000.0, r).unwrap();
        let ge = build_grid(&HeightField::padded(&east, &ne), 1000.0, r).unwrap();
        for j in 0..r as usize {
            let a = gw.positions[j * r as usize + (r as usize - 1)];
            let b = ge.positions[j * r as usize];
            assert_eq!(a[1], b[1], "row {j}: {a:?} vs {b:?}");
            assert_eq!(a[2], b[2]);
        }
    }
}
