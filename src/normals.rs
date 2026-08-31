//! Per-vertex normals: decoded from the provider's normal-map tiles, or
//! synthesized from the padded height field when the provider has none.
//!
//! The source mirrors raytiles / bevytiles, which fetch a third asset kind
//! next to imagery and heightmaps — a 256×256 normal-map tile (tilezen
//! `normal`, cached under `normals/z/x/y.png`) — and fall back to a flat
//! default when it is unavailable. A baked-geometry tile can do better than
//! flat: it has the padded height field in hand, so the fallback here
//! *derives* normals from the heights instead (central differences over the
//! same data the vertices sample, which keeps shared edges consistent).
//!
//! Map encoding (verified empirically against the height gradient of real
//! tiles): `n = rgb / 255 · 2 − 1` is a unit vector with **r = east,
//! g = north, b = up** (any alpha channel — tilezen stores quantized
//! elevation there — is ignored). In the tile frame (+X east, +Y up,
//! +Z south) that is `(r, b, −g)`.

use crate::terrain::{HeightField, Window, TILE_TEXELS};
use crate::{Error, Result};
use std::sync::Arc;

/// A decoded normal-map tile, texels already in the tile frame, viewed
/// through a [`Window`] exactly like a [`HeightField`].
#[derive(Clone, Debug)]
pub struct NormalMap {
    /// Edge length in texels (providers use 256; any square works).
    size: usize,
    /// Row-major unit vectors in the tile frame (+X east, +Y up, +Z south).
    vecs: Arc<[[f32; 3]]>,
    /// The part of the map this tile covers (`FULL` at the source's zoom).
    window: Window,
}

impl NormalMap {
    /// Decode a provider normal tile: any square RGB(A) image; each texel
    /// `rgb·2/255 − 1` mapped from (east, north, up) into the tile frame.
    pub fn decode(bytes: &[u8], what: &str) -> Result<Self> {
        let img = image::load_from_memory(bytes)
            .map_err(|e| Error::Decode {
                what: what.into(),
                reason: e.to_string(),
            })?
            .into_rgb8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        if w != h || w < 2 {
            return Err(Error::Decode {
                what: what.into(),
                reason: format!("expected a square normal map (≥2 px), got {w}×{h}"),
            });
        }
        let vecs: Vec<[f32; 3]> = img
            .pixels()
            .map(|p| {
                let c = |b: u8| f32::from(b) / 255.0 * 2.0 - 1.0;
                // (east, north, up) → (+X east, +Y up, +Z south)
                normalize([c(p[0]), c(p[2]), -c(p[1])])
            })
            .collect();
        Ok(Self {
            size: w,
            vecs: vecs.into(),
            window: Window::FULL,
        })
    }

    /// The view of this map covering the descendant tile at window offset
    /// `(qx, qy)` (in `[0, 2^dz)`), `dz` zoom levels deeper. Shares the
    /// texels; composes with an existing window. Same math as
    /// [`HeightField::windowed`].
    pub fn windowed(&self, dz: u8, qx: u32, qy: u32) -> Self {
        let n = f64::from(1u32 << dz);
        debug_assert!(f64::from(qx) < n && f64::from(qy) < n);
        let scale = self.window.scale / n;
        Self {
            size: self.size,
            vecs: self.vecs.clone(),
            window: Window {
                u0: self.window.u0 + f64::from(qx) * scale,
                v0: self.window.v0 + f64::from(qy) * scale,
                scale,
            },
        }
    }

    /// Bilinear normal at normalized tile coordinates `(u, v) ∈ [0, 1]`,
    /// through the window, renormalized after interpolation. Texel centres
    /// sit at `(i + 0.5) / size`; there is no pad ring (unlike heights), so
    /// the edges clamp — a boundary vertex may shade up to half a texel
    /// differently from its neighbour tile, which is the engines' behaviour
    /// too (their samplers clamp to edge).
    pub fn sample(&self, u: f64, v: f64) -> [f32; 3] {
        let w = &self.window;
        let n = self.size as f64;
        let fx = (w.u0 + u * w.scale) * n - 0.5;
        let fy = (w.v0 + v * w.scale) * n - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
        let max = (self.size - 1) as i64;
        let at = |x: i64, y: i64| -> [f32; 3] {
            let x = x.clamp(0, max) as usize;
            let y = y.clamp(0, max) as usize;
            self.vecs[y * self.size + x]
        };
        let (x0, y0) = (x0 as i64, y0 as i64);
        let mut out = [0f32; 3];
        for (a, o) in out.iter_mut().enumerate() {
            let top = at(x0, y0)[a] * (1.0 - tx) + at(x0 + 1, y0)[a] * tx;
            let bot = at(x0, y0 + 1)[a] * (1.0 - tx) + at(x0 + 1, y0 + 1)[a] * tx;
            *o = top * (1.0 - ty) + bot * ty;
        }
        normalize(out)
    }
}

/// Normal synthesized from the height field at tile coordinates `(u, v)`:
/// central differences half a source texel wide over the *padded* field, so
/// a boundary vertex reads the same texels from both sides of an edge and
/// synthesized normals stay watertight wherever the heights are (§5.5 of
/// specs.md). `source_size_m` is the metre size of the tile the field came
/// from ([`TileId::size_m`](crate::TileId::size_m) of the terrain source) —
/// it converts the height deltas into slopes.
pub fn from_heights(field: &HeightField, u: f64, v: f64, source_size_m: f64) -> [f32; 3] {
    /// Half a source texel, in field space.
    const STEP: f64 = 0.5 / TILE_TEXELS as f64;
    let w = field.window;
    let (fu, fv) = (w.u0 + u * w.scale, w.v0 + v * w.scale);
    let span = (2.0 * STEP * source_size_m) as f32;
    let gx = (field.sample_raw(fu + STEP, fv) - field.sample_raw(fu - STEP, fv)) / span;
    let gz = (field.sample_raw(fu, fv + STEP) - field.sample_raw(fu, fv - STEP)) / span;
    normalize([-gx, 1.0, -gz])
}

/// Unit-length `v`; straight up when `v` is degenerate (a zero vector can
/// only come from corrupt map bytes, and up is the only safe answer).
pub fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-6 {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode one world-frame normal back into map RGB, the test-side
    /// inverse of [`NormalMap::decode`].
    fn rgb_of(n: [f32; 3]) -> [u8; 3] {
        let b = |v: f32| ((v + 1.0) / 2.0 * 255.0).round() as u8;
        [b(n[0]), b(-n[2]), b(n[1])]
    }

    fn map_of(px: [u8; 3], size: u32) -> NormalMap {
        let img = image::RgbImage::from_pixel(size, size, image::Rgb(px));
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        NormalMap::decode(&bytes, "test").unwrap()
    }

    #[test]
    fn decodes_flat_and_tilted() {
        // the engines' flat default: RGB(128, 128, 255) ≈ straight up
        let up = map_of([128, 128, 255], 4).sample(0.5, 0.5);
        assert!(up[1] > 0.999, "{up:?}");
        // g > 128 means the surface faces north = −Z here
        let north = map_of(rgb_of(normalize([0.0, 1.0, -0.5])), 4).sample(0.5, 0.5);
        assert!(north[2] < -0.3 && north[1] > 0.8, "{north:?}");
        // r > 128 faces east = +X
        let east = map_of(rgb_of(normalize([0.5, 1.0, 0.0])), 4).sample(0.5, 0.5);
        assert!(east[0] > 0.3, "{east:?}");
    }

    #[test]
    fn rejects_non_square() {
        let img = image::RgbImage::new(4, 8);
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        assert!(matches!(
            NormalMap::decode(&bytes, "t"),
            Err(Error::Decode { .. })
        ));
    }

    #[test]
    fn windowing_composes_like_heights() {
        let m = map_of([128, 128, 255], 4);
        let w = m.windowed(2, 3, 1);
        assert!((w.window.scale - 0.25).abs() < 1e-12);
        assert!((w.window.u0 - 0.75).abs() < 1e-12);
        // sampling is still a unit up-normal wherever the window lands
        assert!(w.sample(0.0, 1.0)[1] > 0.999);
    }

    #[test]
    fn heights_normal_matches_a_known_ramp() {
        // h = x_texel metres: rises 1 m per texel eastward
        let n = TILE_TEXELS;
        let tile: Vec<f32> = (0..n * n).map(|i| (i % n) as f32).collect();
        let field = HeightField::unpadded(&tile);
        let size_m = 256.0; // 1 m per texel → slope 1 eastward
        let got = from_heights(&field, 0.5, 0.5, size_m);
        let want = normalize([-1.0, 1.0, 0.0]);
        for a in 0..3 {
            assert!((got[a] - want[a]).abs() < 1e-3, "{got:?} vs {want:?}");
        }
    }
}
