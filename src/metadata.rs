//! Per-tile metadata: size, height range, and LOD geometric error
//! (`tasks.md` Milestone A).
//!
//! For every built tile `glb/{fingerprint}/z/x/y.glb` a JSON document with
//! the same name but a `.json` extension records:
//!
//! - `tile_size_m` — the tile edge in metres (§3.3 of `specs.md`);
//! - `min_height_m` / `max_height_m` — the height range of the tile's own
//!   heightmap texels (of the window onto the source tile when the
//!   heightmap was derived from an ancestor zoom);
//! - `geometric_error_m` — the maximum absolute difference between the
//!   tile's surface and the surface of its 4 children at `zoom + 1`,
//!   sampled at the children's texel centres with the tile's surface
//!   evaluated bilinearly. An LOD consumer splits a tile while this error
//!   projects to more screen pixels than its budget.
//!
//! A child whose heightmap resolves to the same (or a lower) source zoom as
//! the tile's own carries no extra detail and contributes 0 — so a tile at
//! the provider's deepest zoom gets `geometric_error_m: 0`, telling the
//! consumer there is nothing to refine into. Heightmaps are fetched like the
//! builder fetches them: cache first, network with write-through on a miss,
//! honouring `.404` markers.

use crate::fetch::Fetcher;
use crate::provider::Kind;
use crate::terrain::{decode_heightmap_png, HeightField, TILE_TEXELS};
use crate::tile::{TileId, MAX_ZOOM};
use crate::{Config, Error, Result};
use serde::Serialize;

/// The metadata document written next to a tile (see the module docs).
#[derive(Clone, Debug, Serialize)]
pub struct TileMetadata {
    /// Zoom level of the tile.
    pub zoom: u8,
    /// Tile column (west → east).
    pub x: u32,
    /// Tile row (north → south).
    pub y: u32,
    /// Edge length in metres at the tile's centre latitude.
    pub tile_size_m: f64,
    /// Lowest point of the tile's heightmap, metres above sea level.
    pub min_height_m: f32,
    /// Highest point of the tile's heightmap, metres above sea level.
    pub max_height_m: f32,
    /// Maximum distance in metres between this tile's surface and its 4
    /// children's; 0 when no finer data exists.
    pub geometric_error_m: f32,
}

impl TileMetadata {
    /// The document as it is stored: pretty-printed JSON bytes.
    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("plain struct serializes")
    }
}

/// What one [`generate_missing`] run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Metadata documents written.
    pub written: usize,
    /// Tiles that already had one.
    pub skipped: usize,
    /// Tiles whose metadata could not be computed (logged individually).
    pub failed: usize,
}

/// What happened to one tile of a scan (reported to
/// [`generate_missing_with`]'s progress callback).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A document was computed and stored.
    Written,
    /// The tile already had one.
    Skipped,
    /// Computing failed (logged); the scan moves on.
    Failed,
}

/// Compute the metadata for one tile (fetching heightmaps as needed).
pub fn compute(cfg: &Config, tile: TileId) -> Result<TileMetadata> {
    let fetcher = Fetcher::new(cfg.cache.clone(), cfg.connect_timeout, cfg.read_timeout);
    compute_with(&fetcher, cfg, tile)
}

/// [`compute`] over an existing fetcher (one HTTP pool for a whole run).
fn compute_with(fetcher: &Fetcher, cfg: &Config, tile: TileId) -> Result<TileMetadata> {
    let c = fetcher.fetch_closest(&cfg.provider, Kind::Heightmap, tile)?;
    let heights = decode_heightmap_png(&c.bytes, &format!("heightmap {}", c.source))?;
    let dz = tile.zoom - c.source.zoom;
    let (_, qx, qy) = tile.ancestor(c.source.zoom);
    let (min, max) = window_min_max(&heights, dz, qx, qy);
    // The surface the tile's mesh samples, over the tile's [0, 1]² domain.
    // Unpadded: the outermost half-texel band clamps instead of reading the
    // neighbours — a sub-texel effect on an extreme bound, not worth 8
    // fetches per tile here.
    let surface = HeightField::unpadded(&heights).windowed(dz, qx, qy);
    let error = geometric_error(fetcher, cfg, tile, c.source.zoom, &surface)?;
    Ok(TileMetadata {
        zoom: tile.zoom,
        x: tile.x,
        y: tile.y,
        tile_size_m: tile.size_m(),
        min_height_m: min,
        max_height_m: max,
        geometric_error_m: error,
    })
}

/// Compute and publish the metadata document at `json_key` unless one is
/// already there; returns whether a document was written. This is what the
/// server calls right after caching a freshly built tile (Milestone B) —
/// [`generate_missing`] fills whatever such calls failed to write.
pub fn write_missing(cfg: &Config, tile: TileId, json_key: &str) -> Result<bool> {
    if cfg.cache.exists(json_key)? {
        return Ok(false);
    }
    let meta = compute(cfg, tile)?;
    cfg.cache.put(json_key, &meta.to_json_bytes())?;
    Ok(true)
}

/// Scan every built tile under `glb/` and write the missing `.json`
/// documents next to them. Existing documents are left untouched; one tile
/// present under several fingerprints is computed once and written to each.
/// A tile that fails is logged, counted, and does not stop the run.
pub fn generate_missing(cfg: &Config) -> Result<Summary> {
    generate_missing_with(cfg, |_, _, _| true)
}

/// [`generate_missing`], reporting every tile to `progress` as it is
/// handled (`(json key, tile, outcome)`). Return `false` to stop the scan
/// early — the summary then covers only the part that ran. This is what
/// the server's SSE endpoint streams from, stopping when the client is
/// gone.
pub fn generate_missing_with(
    cfg: &Config,
    mut progress: impl FnMut(&str, TileId, Outcome) -> bool,
) -> Result<Summary> {
    let keys = cfg.cache.list("glb/")?;
    let existing: std::collections::HashSet<&str> = keys.iter().map(String::as_str).collect();
    let fetcher = Fetcher::new(cfg.cache.clone(), cfg.connect_timeout, cfg.read_timeout);
    let mut memo: std::collections::HashMap<TileId, Option<Vec<u8>>> = Default::default();
    let mut summary = Summary::default();
    for key in &keys {
        let Some(stem) = key.strip_suffix(".glb") else {
            continue;
        };
        let Some(tile) = parse_glb_key(key) else {
            continue;
        };
        let json_key = format!("{stem}.json");
        let outcome = if existing.contains(json_key.as_str()) {
            summary.skipped += 1;
            Outcome::Skipped
        } else {
            let bytes =
                memo.entry(tile)
                    .or_insert_with(|| match compute_with(&fetcher, cfg, tile) {
                        Ok(m) => Some(m.to_json_bytes()),
                        Err(e) => {
                            log::warn!("{tile}: computing metadata failed: {e}");
                            None
                        }
                    });
            match bytes {
                Some(b) => {
                    cfg.cache.put(&json_key, b)?;
                    log::info!("{tile}: wrote {json_key}");
                    summary.written += 1;
                    Outcome::Written
                }
                None => {
                    summary.failed += 1;
                    Outcome::Failed
                }
            }
        };
        if !progress(&json_key, tile, outcome) {
            break;
        }
    }
    Ok(summary)
}

/// `glb/{fingerprint}/{z}/{x}/{y}.glb` → the tile, or `None` for any key
/// that is not shaped like a built tile (foreign files are just skipped).
fn parse_glb_key(key: &str) -> Option<TileId> {
    let parts: Vec<&str> = key.split('/').collect();
    let ["glb", _fingerprint, z, x, y] = parts.as_slice() else {
        return None;
    };
    let y = y.strip_suffix(".glb")?;
    TileId::new(z.parse().ok()?, x.parse().ok()?, y.parse().ok()?).ok()
}

/// Min/max over the source texels covering the window `(qx, qy)` at `dz`
/// levels below the source — the whole image when `dz == 0`. The block is
/// rounded outward to whole texels, so for derived tiles the range is the
/// tight bound on the bilinear surface, not an undershoot.
fn window_min_max(heights: &[f32], dz: u8, qx: u32, qy: u32) -> (f32, f32) {
    let n = TILE_TEXELS;
    let scale = n as f64 / f64::from(1u32 << dz);
    let x0 = (f64::from(qx) * scale).floor() as usize;
    let x1 = ((f64::from(qx + 1) * scale).ceil() as usize).clamp(x0 + 1, n);
    let y0 = (f64::from(qy) * scale).floor() as usize;
    let y1 = ((f64::from(qy + 1) * scale).ceil() as usize).clamp(y0 + 1, n);
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for row in heights[..n * n].chunks_exact(n).take(y1).skip(y0) {
        for &h in &row[x0..x1] {
            lo = lo.min(h);
            hi = hi.max(h);
        }
    }
    (lo, hi)
}

/// The maximum distance between `tile`'s surface and its 4 children's,
/// children fetched concurrently. 0 at [`MAX_ZOOM`] (no children exist) and
/// for children without finer data (404, or resolved to `source_zoom` or
/// above it).
fn geometric_error(
    fetcher: &Fetcher,
    cfg: &Config,
    tile: TileId,
    source_zoom: u8,
    surface: &HeightField,
) -> Result<f32> {
    if tile.zoom >= MAX_ZOOM {
        return Ok(0.0);
    }
    let results: Vec<Result<f32>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4u32)
            .map(|q| {
                let (dx, dy) = (q & 1, q >> 1);
                s.spawn(move || {
                    let child = TileId::new(tile.zoom + 1, tile.x * 2 + dx, tile.y * 2 + dy)
                        .expect("children of a valid non-max tile are valid");
                    child_error(
                        fetcher,
                        cfg,
                        child,
                        source_zoom,
                        &surface.windowed(1, dx, dy),
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("fetch thread panicked"))
            .collect()
    });
    let mut max = 0f32;
    for r in results {
        max = max.max(r?);
    }
    Ok(max)
}

/// One child's contribution: the max |child − parent| over the child's
/// domain, `parent` being the tile's surface already windowed onto that
/// quadrant. Like the builder, only a 404 is "no data" — a transient error
/// fails the computation rather than silently recording a smaller error.
fn child_error(
    fetcher: &Fetcher,
    cfg: &Config,
    child: TileId,
    source_zoom: u8,
    parent: &HeightField,
) -> Result<f32> {
    let c = match fetcher.fetch_closest(&cfg.provider, Kind::Heightmap, child) {
        Ok(c) => c,
        Err(Error::NotFound { .. }) => return Ok(0.0),
        Err(e) => return Err(e),
    };
    if c.source.zoom <= source_zoom {
        return Ok(0.0); // same data the tile was built from: no extra detail
    }
    let heights = decode_heightmap_png(&c.bytes, &format!("heightmap {}", c.source))?;
    let (_, qx, qy) = child.ancestor(c.source.zoom);
    let fine = HeightField::unpadded(&heights).windowed(child.zoom - c.source.zoom, qx, qy);
    Ok(max_abs_diff(parent, &fine))
}

/// Max |a − b| over both surfaces sampled at the 256² texel centres of
/// their shared [0, 1]² domain.
fn max_abs_diff(a: &HeightField, b: &HeightField) -> f32 {
    let n = TILE_TEXELS;
    let mut max = 0f32;
    for j in 0..n {
        let v = (j as f64 + 0.5) / n as f64;
        for i in 0..n {
            let u = (i as f64 + 0.5) / n as f64;
            max = max.max((a.sample(u, v) - b.sample(u, v)).abs());
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `height = x + 0.5·y` in texel units, offset by `(ox, oy)` texels.
    fn ramp(ox: f64, oy: f64) -> Vec<f32> {
        let n = TILE_TEXELS;
        (0..n * n)
            .map(|i| ((i % n) as f64 + ox + 0.5 * ((i / n) as f64 + oy)) as f32)
            .collect()
    }

    #[test]
    fn parses_only_built_tile_keys() {
        let t = parse_glb_key("glb/abcd1234/12/772/1607.glb").unwrap();
        assert_eq!((t.zoom, t.x, t.y), (12, 772, 1607));
        for bad in [
            "glb/abcd1234/12/772/1607.json",
            "glb/12/772/1607.glb",
            "glb/f/deep/12/772/1607.glb",
            "glb/f/99/0/0.glb",
            "glb/f/12/x/0.glb",
            "texture/12/772/1607.png",
        ] {
            assert!(parse_glb_key(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn window_min_max_covers_whole_and_quadrants() {
        let h = ramp(0.0, 0.0);
        let n = (TILE_TEXELS - 1) as f32;
        assert_eq!(window_min_max(&h, 0, 0, 0), (0.0, n * 1.5));
        // NW quadrant: rows/cols 0..128
        let (lo, hi) = window_min_max(&h, 1, 0, 0);
        assert_eq!(lo, 0.0);
        assert_eq!(hi, 127.0 * 1.5);
        // SE quadrant: rows/cols 128..256
        let (lo, hi) = window_min_max(&h, 1, 1, 1);
        assert_eq!(lo, 128.0 * 1.5);
        assert_eq!(hi, n * 1.5);
        // deep window: never empty, stays within the image
        let (lo, hi) = window_min_max(&h, 10, 1023, 0);
        assert!(lo <= hi && hi <= n * 1.5);
    }

    /// A child that is the parent's surface plus a constant must measure
    /// exactly that constant (bilinear interpolation of a linear ramp is
    /// exact away from the clamped edge band).
    #[test]
    fn constant_offset_child_measures_the_offset() {
        let parent = HeightField::unpadded(&ramp(0.0, 0.0));
        // NW child at zoom+1: its texel (i, j) sits at parent texel
        // coordinates ((i + 0.5) / 2 − 0.5, …) — the same ramp evaluated
        // there, plus 10.
        let child_heights: Vec<f32> = {
            let n = TILE_TEXELS;
            (0..n * n)
                .map(|k| {
                    let (i, j) = ((k % n) as f64, (k / n) as f64);
                    let (px, py) = ((i + 0.5) / 2.0 - 0.5, (j + 0.5) / 2.0 - 0.5);
                    (px + 0.5 * py + 10.0) as f32
                })
                .collect()
        };
        let child = HeightField::unpadded(&child_heights);
        let err = max_abs_diff(&parent.windowed(1, 0, 0), &child);
        assert!((err - 10.0).abs() < 1e-3, "err {err}");
    }

    /// A progress callback returning `false` stops the scan after the
    /// current tile: nothing later is touched or reported.
    #[test]
    fn scan_stops_when_progress_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::Config::with_cache_dir(dir.path());
        // two built tiles, both already documented (no fetches involved)
        for y in [0, 1] {
            cfg.cache
                .put(&format!("glb/f/10/0/{y}.glb"), b"glTF")
                .unwrap();
            cfg.cache
                .put(&format!("glb/f/10/0/{y}.json"), b"{}")
                .unwrap();
        }
        let mut seen = Vec::new();
        let summary = generate_missing_with(&cfg, |key, tile, outcome| {
            seen.push((key.to_string(), tile, outcome));
            false
        })
        .unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, "glb/f/10/0/0.json");
        assert_eq!(seen[0].2, Outcome::Skipped);
        assert_eq!(
            summary,
            Summary {
                written: 0,
                skipped: 1,
                failed: 0
            }
        );
    }

    #[test]
    fn identical_surfaces_measure_zero() {
        let parent = HeightField::unpadded(&ramp(0.0, 0.0));
        let same = parent.windowed(1, 1, 0);
        assert_eq!(max_abs_diff(&parent.windowed(1, 1, 0), &same), 0.0);
    }
}
