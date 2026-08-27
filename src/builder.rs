//! The one entry point: fetch inputs → decode → grid → GLB.

use crate::fetch::{Fetcher, Origin};
use crate::glb::{write_glb, TileMeta};
use crate::mesh::{build_grid, check_resolution, DEFAULT_RESOLUTION};
use crate::provider::{Kind, Provider};
use crate::terrain::{decode_heightmap_png, HeightField};
use crate::tile::TileId;
use crate::{Error, Result};
use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Builder configuration. Every field defaulted; the defaults mirror the
/// engines' `NetworkConfig` so caches are interchangeable.
#[derive(Clone, Debug)]
pub struct Config {
    /// Root of the input cache (`{cache_dir}/{texture,heightmap}/z/x/y.png`).
    pub cache_dir: PathBuf,
    /// Where inputs come from.
    pub provider: Provider,
    /// Vertices per edge of the output grid, `2..=257`.
    pub resolution: u32,
    /// HTTP connect timeout.
    pub connect_timeout: Duration,
    /// HTTP read timeout.
    pub read_timeout: Duration,
    /// JPEG quality used only when the imagery provider returned PNG.
    pub jpeg_quality: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::from(".cache"),
            provider: Provider::default(),
            resolution: DEFAULT_RESOLUTION,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(10),
            jpeg_quality: 90,
        }
    }
}

/// Decoded inputs for one tile — what milestone 1 produces.
pub struct TileInputs {
    /// The tile.
    pub tile: TileId,
    /// Padded height field (tile + neighbour ring), metres.
    pub height: HeightField,
    /// Imagery as a JPEG stream (pass-through, or re-encoded from PNG).
    pub jpeg: Vec<u8>,
    /// How many of the 8 neighbours contributed a pad ring.
    pub neighbours_present: usize,
}

/// Fetch and decode everything the mesh needs. Neighbour heightmaps are
/// fetched concurrently; a neighbour that 404s is treated as the dataset
/// edge (its side clamps), any other neighbour failure fails the build —
/// a transient error must not silently produce a different mesh that then
/// gets cached as the tile.
pub fn load_inputs(cfg: &Config, tile: TileId) -> Result<TileInputs> {
    let native = cfg.provider.native_terrain_zoom;
    if tile.zoom > native {
        return Err(Error::AboveNativeZoom {
            zoom: tile.zoom,
            native,
        });
    }
    let fetcher = Fetcher::new(&cfg.cache_dir, cfg.connect_timeout, cfg.read_timeout);
    let fetcher = &fetcher;
    let neighbours = tile.neighbours();

    // centre heightmap + imagery + 8 neighbours, all in flight together
    let (centre, imagery, ring) = std::thread::scope(|s| {
        let centre = s.spawn(move || fetch_heightmap(fetcher, &cfg.provider, tile, "heightmap"));
        let imagery = s.spawn(move || fetch_imagery(fetcher, cfg, tile));
        let ring: Vec<_> = neighbours
            .iter()
            .map(|n| {
                s.spawn(move || match n {
                    None => Ok(None),
                    Some(t) => {
                        match fetch_heightmap(fetcher, &cfg.provider, *t, "neighbour heightmap") {
                            Ok(h) => Ok(Some(h)),
                            Err(Error::NotFound { url }) => {
                                log::info!("neighbour {t} not found ({url}); clamping that edge");
                                Ok(None)
                            }
                            Err(e) => Err(e),
                        }
                    }
                })
            })
            .collect();
        let ring: Vec<Result<Option<Vec<f32>>>> = ring
            .into_iter()
            .map(|h| h.join().expect("fetch thread panicked"))
            .collect();
        (
            centre.join().expect("fetch thread panicked"),
            imagery.join().expect("fetch thread panicked"),
            ring,
        )
    });

    let centre = centre?;
    let jpeg = imagery?;
    let mut sides: [Option<Vec<f32>>; 8] = Default::default();
    for (slot, r) in sides.iter_mut().zip(ring) {
        *slot = r?;
    }
    let neighbours_present = sides.iter().filter(|s| s.is_some()).count();
    let height = HeightField::padded(&centre, &sides);
    Ok(TileInputs {
        tile,
        height,
        jpeg,
        neighbours_present,
    })
}

/// Build the finished GLB for `tile`.
pub fn build_tile(cfg: &Config, tile: TileId) -> Result<Vec<u8>> {
    check_resolution(cfg.resolution)?;
    let t0 = Instant::now();
    let inputs = load_inputs(cfg, tile)?;
    let t1 = Instant::now();
    let grid = build_grid(&inputs.height, tile.size_m(), cfg.resolution)?;
    let meta = TileMeta {
        tile,
        tile_size_m: tile.size_m(),
        resolution: cfg.resolution,
        native_terrain: true,
        imagery_attribution: cfg.provider.imagery_attribution.clone(),
        elevation_attribution: cfg.provider.elevation_attribution.clone(),
    };
    let glb = write_glb(&grid, &inputs.jpeg, &meta);
    log::info!(
        "built {tile}: {} vertices, {} triangles, {} bytes (inputs {:?}, mesh+glb {:?}, {} neighbours)",
        grid.positions.len(),
        grid.triangles(),
        glb.len(),
        t1 - t0,
        t1.elapsed(),
        inputs.neighbours_present,
    );
    Ok(glb)
}

fn fetch_heightmap(
    fetcher: &Fetcher,
    provider: &Provider,
    tile: TileId,
    what: &str,
) -> Result<Vec<f32>> {
    let url = provider.url(Kind::Heightmap, tile);
    let (bytes, origin) = fetcher.fetch(Kind::Heightmap, tile, &url)?;
    log_fetch(what, tile, origin, bytes.len());
    decode_heightmap_png(&bytes, &format!("{what} {tile}"))
}

/// Imagery bytes as JPEG. Esri serves JPEG under a `.png`-named cache entry;
/// sniff, pass JPEG through untouched (keeps the build deterministic and
/// lossless), re-encode anything else.
fn fetch_imagery(fetcher: &Fetcher, cfg: &Config, tile: TileId) -> Result<Vec<u8>> {
    let url = cfg.provider.url(Kind::Texture, tile);
    let (bytes, origin) = fetcher.fetch(Kind::Texture, tile, &url)?;
    log_fetch("imagery", tile, origin, bytes.len());
    let format = image::guess_format(&bytes).map_err(|e| Error::Decode {
        what: format!("imagery {tile}"),
        reason: e.to_string(),
    })?;
    if format == image::ImageFormat::Jpeg {
        return Ok(bytes);
    }
    let img = image::load_from_memory_with_format(&bytes, format)
        .map_err(|e| Error::Decode {
            what: format!("imagery {tile}"),
            reason: e.to_string(),
        })?
        .into_rgb8();
    let mut out = Vec::new();
    let mut enc =
        image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), cfg.jpeg_quality);
    enc.encode_image(&img).map_err(|e| Error::Decode {
        what: format!("imagery {tile} (jpeg re-encode)"),
        reason: e.to_string(),
    })?;
    log::debug!(
        "imagery {tile}: re-encoded {format:?} → jpeg ({} bytes)",
        out.len()
    );
    Ok(out)
}

fn log_fetch(what: &str, tile: TileId, origin: Origin, len: usize) {
    match origin {
        Origin::Cache => log::debug!("{what} {tile}: cache hit ({len} bytes)"),
        Origin::Network => log::info!("{what} {tile}: downloaded ({len} bytes)"),
    }
}
