//! Terrarium decoding and the padded height field the mesh samples.
//!
//! Terrarium: `h = r·256 + g + b/256 − 32768` metres. Reviewed against
//! bevytiles `synth.rs` / `height.rs`:
//! - the engines' `HeightGrid` quantises to whole metres (`u16`) to save
//!   runtime memory; a build-once server has no such pressure, so heights
//!   stay `f32` with the full 1/256 m fraction the source carries.
//! - the decode is done in `f64` and narrowed once. In `f32` the expression
//!   happens to be exact (24-bit mantissa covers 16 integer + 8 fraction
//!   bits), but that is a property of the operand order, not something a
//!   reader should have to prove.
//! - bilinear sampling keeps the engines' texel-centre convention
//!   (`(i + 0.5) / n`), extended over a 1-texel pad ring taken from the
//!   neighbouring tiles so a vertex on a shared edge sees the same two
//!   texels from either side.

use crate::{Error, Result};
use image::RgbImage;

/// Native heightmap edge length in texels.
pub const TILE_TEXELS: usize = 256;

/// Decode Terrarium RGB into metres, row-major.
pub fn decode_terrarium(img: &RgbImage) -> Vec<f32> {
    img.pixels()
        .map(|p| {
            (f64::from(p[0]) * 256.0 + f64::from(p[1]) + f64::from(p[2]) / 256.0 - 32768.0) as f32
        })
        .collect()
}

/// Decode a heightmap PNG's bytes to metres; rejects anything not 256×256.
pub fn decode_heightmap_png(bytes: &[u8], what: &str) -> Result<Vec<f32>> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| Error::Decode {
            what: what.into(),
            reason: e.to_string(),
        })?
        .into_rgb8();
    if img.width() as usize != TILE_TEXELS || img.height() as usize != TILE_TEXELS {
        return Err(Error::Decode {
            what: what.into(),
            reason: format!(
                "expected {TILE_TEXELS}×{TILE_TEXELS}, got {}×{}",
                img.width(),
                img.height()
            ),
        });
    }
    Ok(decode_terrarium(&img))
}

/// The 8 neighbours of a tile, row-major: `NW, N, NE, W, E, SW, S, SE`.
/// Indexes into [`HeightField::padded`]'s `neighbours` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Side {
    NW = 0,
    N = 1,
    NE = 2,
    W = 3,
    E = 4,
    SW = 5,
    S = 6,
    SE = 7,
}

/// A `(256 + 2)²` height field: the tile in the centre, one ring of texels
/// borrowed from the neighbours (or replicated from the tile's own edge
/// where a neighbour is missing).
#[derive(Clone, Debug)]
pub struct HeightField {
    /// Edge length including the pad (258).
    pub size: usize,
    /// Row-major heights in metres.
    pub data: Vec<f32>,
}

impl HeightField {
    /// Assemble the padded field. `tile` is the centre (256²); each
    /// `neighbours[i]` is the full 256² heightmap of that side (see
    /// [`Side`]) or `None`.
    pub fn padded(tile: &[f32], neighbours: &[Option<Vec<f32>>; 8]) -> Self {
        let n = TILE_TEXELS;
        let size = n + 2;
        debug_assert_eq!(tile.len(), n * n);
        let mut data = vec![0f32; size * size];

        // centre
        for row in 0..n {
            data[(row + 1) * size + 1..(row + 1) * size + 1 + n]
                .copy_from_slice(&tile[row * n..row * n + n]);
        }

        let at = |src: &[f32], x: usize, y: usize| src[y * n + x];
        let side = |i: Side| neighbours[i as usize].as_deref();

        // edges: neighbour's facing row/column, else replicate own edge
        for i in 0..n {
            // north (pad row 0) ← N's row 255
            data[i + 1] = side(Side::N).map_or(at(tile, i, 0), |s| at(s, i, n - 1));
            // south (pad row 257) ← S's row 0
            data[(size - 1) * size + i + 1] =
                side(Side::S).map_or(at(tile, i, n - 1), |s| at(s, i, 0));
            // west (pad col 0) ← W's col 255
            data[(i + 1) * size] = side(Side::W).map_or(at(tile, 0, i), |s| at(s, n - 1, i));
            // east (pad col 257) ← E's col 0
            data[(i + 1) * size + size - 1] =
                side(Side::E).map_or(at(tile, n - 1, i), |s| at(s, 0, i));
        }

        // corners: the diagonal neighbour's facing corner. When it is
        // missing, the fallback must be a texel *both* tiles sharing that
        // edge can see, or the boundary vertex (which touches the corner
        // pad) would differ between them: prefer the E/W neighbour's
        // corner texel, then the N/S one, then our own corner. With all
        // four tiles around a corner present this never triggers; with
        // two present it keeps their shared edge exact.
        let corner = |diag: Side,
                      ew: Side,
                      ns: Side,
                      d: (usize, usize),
                      e: (usize, usize),
                      n_: (usize, usize),
                      own: (usize, usize)| {
            side(diag)
                .map(|s| at(s, d.0, d.1))
                .or_else(|| side(ew).map(|s| at(s, e.0, e.1)))
                .or_else(|| side(ns).map(|s| at(s, n_.0, n_.1)))
                .unwrap_or_else(|| at(tile, own.0, own.1))
        };
        let last = n - 1;
        data[0] = corner(
            Side::NW,
            Side::W,
            Side::N,
            (last, last),
            (last, 0),
            (0, last),
            (0, 0),
        );
        data[size - 1] = corner(
            Side::NE,
            Side::E,
            Side::N,
            (0, last),
            (0, 0),
            (last, last),
            (last, 0),
        );
        data[(size - 1) * size] = corner(
            Side::SW,
            Side::W,
            Side::S,
            (last, 0),
            (last, last),
            (0, 0),
            (0, last),
        );
        data[size * size - 1] = corner(
            Side::SE,
            Side::E,
            Side::S,
            (0, 0),
            (0, last),
            (last, 0),
            (last, last),
        );

        Self { size, data }
    }

    /// A field with no neighbours (edges replicated) — handy in tests.
    pub fn unpadded(tile: &[f32]) -> Self {
        Self::padded(tile, &Default::default())
    }

    /// Bilinear height at normalized tile coordinates `(u, v) ∈ [0, 1]`,
    /// `u` west→east, `v` north→south. Texel centres sit at `(i + 0.5)/256`
    /// of the *tile*, i.e. at `i + 1.5` in padded coordinates; `u = 0` lands
    /// exactly between the west pad texel and texel 0.
    pub fn sample(&self, u: f64, v: f64) -> f32 {
        let n = (self.size - 2) as f64;
        let fx = u * n + 0.5; // padded-space coordinate, texel centres at k + 0.5
        let fy = v * n + 0.5;
        let x0 = fx.floor();
        let y0 = fy.floor();
        let tx = (fx - x0) as f32;
        let ty = (fy - y0) as f32;
        let max = (self.size - 1) as i64;
        let s = |x: i64, y: i64| -> f32 {
            let x = x.clamp(0, max) as usize;
            let y = y.clamp(0, max) as usize;
            self.data[y * self.size + x]
        };
        let (x0, y0) = (x0 as i64, y0 as i64);
        let top = s(x0, y0) * (1.0 - tx) + s(x0 + 1, y0) * tx;
        let bot = s(x0, y0 + 1) * (1.0 - tx) + s(x0 + 1, y0 + 1) * tx;
        top * (1.0 - ty) + bot * ty
    }

    /// Minimum and maximum height in the field (pad included).
    pub fn min_max(&self) -> (f32, f32) {
        self.data
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &h| {
                (lo.min(h), hi.max(h))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(h: f64) -> image::Rgb<u8> {
        let fixed = ((h + 32768.0) * 256.0).round() as u32;
        image::Rgb([
            (fixed >> 16) as u8,
            ((fixed >> 8) & 0xFF) as u8,
            (fixed & 0xFF) as u8,
        ])
    }

    #[test]
    fn decodes_known_heights() {
        let heights = [0.0, 8848.0, -415.0, 100.5, 255.99609375, 256.0];
        let mut img = RgbImage::new(heights.len() as u32, 1);
        for (i, h) in heights.iter().enumerate() {
            img.put_pixel(i as u32, 0, px(*h));
        }
        for (a, b) in heights.iter().zip(decode_terrarium(&img)) {
            assert!((*a as f32 - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    /// Two tiles side by side with a single ramp across both: the boundary
    /// vertex must evaluate identically from either tile's padded field.
    #[test]
    fn shared_edge_is_identical_from_both_sides() {
        let n = TILE_TEXELS;
        let ramp = |x: usize| -> f32 { x as f32 * 2.0 };
        let west: Vec<f32> = (0..n * n).map(|i| ramp(i % n)).collect();
        let east: Vec<f32> = (0..n * n).map(|i| ramp(n + i % n)).collect();
        let mut nb_w: [Option<Vec<f32>>; 8] = Default::default();
        nb_w[Side::E as usize] = Some(east.clone());
        let mut nb_e: [Option<Vec<f32>>; 8] = Default::default();
        nb_e[Side::W as usize] = Some(west.clone());
        let fw = HeightField::padded(&west, &nb_w);
        let fe = HeightField::padded(&east, &nb_e);
        for v in [0.0, 0.25, 0.5, 1.0] {
            let a = fw.sample(1.0, v);
            let b = fe.sample(0.0, v);
            assert_eq!(a, b, "v={v}: {a} vs {b}");
            // and it's the true ramp value at the boundary (texel 255.5 → 511)
            assert!((a - 511.0).abs() < 1e-3, "{a}");
        }
        // interior samples land on texel centres exactly
        assert!((fw.sample(0.5 / n as f64, 0.5) - 0.0).abs() < 1e-4);
        assert!((fw.sample(1.5 / n as f64, 0.5) - 2.0).abs() < 1e-4);
    }

    #[test]
    fn missing_neighbour_clamps_to_own_edge() {
        let n = TILE_TEXELS;
        let tile: Vec<f32> = (0..n * n).map(|i| (i % n) as f32).collect();
        let f = HeightField::unpadded(&tile);
        assert_eq!(f.sample(0.0, 0.5), 0.0);
        assert_eq!(f.sample(1.0, 0.5), (n - 1) as f32);
        assert_eq!(f.min_max(), (0.0, (n - 1) as f32));
    }
}
