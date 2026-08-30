//! The one entry point: fetch inputs (falling back to the closest provided
//! zoom) → decode → grid → GLB.

use crate::fetch::{Closest, Fetcher, Origin};
use crate::glb::{write_glb, TileMeta};
use crate::imagery;
use crate::mesh::{build_grid, check_resolution, useful_ceiling, ZOOM_LEVELS};
use crate::provider::{Kind, Provider};
use crate::store::{LocalStore, Store};
use crate::terrain::{decode_heightmap_png, HeightField};
use crate::tile::{TileId, MIN_ZOOM};
use crate::{Error, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Builder configuration. Every field defaulted; the defaults mirror the
/// engines' `NetworkConfig` so caches are interchangeable.
#[derive(Clone, Debug)]
pub struct Config {
    /// The cache: inputs under `{texture,heightmap}/z/x/y.png`, and — when
    /// serving — built tiles under `glb/{fingerprint}/z/x/y.glb`. A local
    /// directory by default; see [`store::open`](crate::store::open) for S3.
    pub cache: Arc<dyn Store>,
    /// Where inputs come from.
    pub provider: Provider,
    /// Vertices per edge of the output grid per zoom, indexed
    /// `zoom - MIN_ZOOM`; each entry in `2..=257`. See
    /// [`DEFAULT_RESOLUTIONS`](crate::mesh::DEFAULT_RESOLUTIONS). The value
    /// actually used is further capped by the useful ceiling when the
    /// heightmap came from a lower zoom.
    pub resolution: [u32; ZOOM_LEVELS],
    /// HTTP connect timeout.
    pub connect_timeout: Duration,
    /// HTTP read timeout.
    pub read_timeout: Duration,
    /// JPEG quality used when imagery has to be encoded (PNG input, or
    /// derived from an ancestor).
    pub jpeg_quality: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache: Arc::new(LocalStore::new(".cache")),
            provider: Provider::default(),
            resolution: crate::mesh::DEFAULT_RESOLUTIONS,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(10),
            jpeg_quality: 90,
        }
    }
}

impl Config {
    /// Defaults with the cache in a local directory.
    pub fn with_cache_dir(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            cache: Arc::new(LocalStore::new(dir)),
            ..Self::default()
        }
    }

    /// The configured resolution for `zoom`.
    pub fn resolution_for(&self, zoom: u8) -> u32 {
        self.resolution[(zoom.clamp(MIN_ZOOM, crate::tile::MAX_ZOOM) - MIN_ZOOM) as usize]
    }

    /// Override the resolution for one zoom.
    pub fn set_resolution(&mut self, zoom: u8, vertices_per_edge: u32) {
        self.resolution[(zoom.clamp(MIN_ZOOM, crate::tile::MAX_ZOOM) - MIN_ZOOM) as usize] =
            vertices_per_edge;
    }

    /// Same table as the defaults but with every entry replaced — handy for
    /// "everything at N".
    pub fn with_uniform_resolution(mut self, vertices_per_edge: u32) -> Self {
        self.resolution = [vertices_per_edge; ZOOM_LEVELS];
        self
    }
}

/// Decoded inputs for one tile.
pub struct TileInputs {
    /// The tile.
    pub tile: TileId,
    /// Height field (source tile + neighbour ring) windowed onto `tile`.
    pub height: HeightField,
    /// The tile the heightmap actually came from (`== tile` or an ancestor).
    pub terrain_source: TileId,
    /// Imagery as a JPEG stream for `tile` (pass-through, re-encoded, or
    /// derived from `imagery_source`).
    pub jpeg: Vec<u8>,
    /// The tile the imagery came from (`== tile` or an ancestor).
    pub imagery_source: TileId,
    /// How many of the source tile's 8 neighbours contributed a pad ring.
    pub neighbours_present: usize,
}

/// Fetch and decode everything the mesh needs.
///
/// Heightmap and imagery each resolve to the closest provided zoom
/// ([`Fetcher::fetch_closest`]). The pad ring is built from the *source*
/// tile's 8 neighbours at the source zoom, fetched concurrently: a neighbour
/// that 404s there is treated as the dataset edge (its side clamps); any
/// other neighbour failure fails the build — a transient error must not
/// silently produce a different mesh that then gets cached as the tile.
pub fn load_inputs(cfg: &Config, tile: TileId) -> Result<TileInputs> {
    let fetcher = Fetcher::new(cfg.cache.clone(), cfg.connect_timeout, cfg.read_timeout);
    let fetcher = &fetcher;

    // resolve the height source first: the neighbour set depends on it
    let (imagery, height_src) = std::thread::scope(|s| {
        let imagery = s.spawn(move || fetch_imagery(fetcher, cfg, tile));
        let height = fetch_closest_logged(fetcher, &cfg.provider, Kind::Heightmap, tile);
        (imagery.join().expect("fetch thread panicked"), height)
    });
    let height_src = height_src?;
    let (jpeg, imagery_source) = imagery?;
    let source = height_src.source;
    let centre = decode_heightmap_png(&height_src.bytes, &format!("heightmap {source}"))?;

    let neighbours = source.neighbours();
    let ring: Vec<Result<Option<Vec<f32>>>> = std::thread::scope(|s| {
        let handles: Vec<_> = neighbours
            .iter()
            .map(|n| {
                s.spawn(move || match n {
                    None => Ok(None),
                    Some(t) => {
                        let url = cfg.provider.url(Kind::Heightmap, *t);
                        match fetcher.fetch(Kind::Heightmap, *t, &url) {
                            Ok((bytes, origin)) => {
                                log_fetch("neighbour heightmap", *t, origin, bytes.len());
                                decode_heightmap_png(&bytes, &format!("neighbour heightmap {t}"))
                                    .map(Some)
                            }
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
        handles
            .into_iter()
            .map(|h| h.join().expect("fetch thread panicked"))
            .collect()
    });

    let mut sides: [Option<Vec<f32>>; 8] = Default::default();
    for (slot, r) in sides.iter_mut().zip(ring) {
        *slot = r?;
    }
    let neighbours_present = sides.iter().filter(|s| s.is_some()).count();
    let field = HeightField::padded(&centre, &sides);
    let (_, qx, qy) = tile.ancestor(source.zoom);
    let height = field.windowed(tile.zoom - source.zoom, qx, qy);
    Ok(TileInputs {
        tile,
        height,
        terrain_source: source,
        jpeg,
        imagery_source,
        neighbours_present,
    })
}

/// Build the finished GLB for `tile`.
pub fn build_tile(cfg: &Config, tile: TileId) -> Result<Vec<u8>> {
    for r in cfg.resolution {
        check_resolution(r)?;
    }
    let t0 = Instant::now();
    let inputs = load_inputs(cfg, tile)?;
    let t1 = Instant::now();

    let requested = cfg.resolution_for(tile.zoom);
    let ceiling = useful_ceiling(tile.zoom - inputs.terrain_source.zoom);
    let resolution = requested.min(ceiling);
    if resolution < requested {
        log::info!(
            "{tile}: resolution {requested} clamped to {resolution} (heights come from zoom {}, {} source texels per edge)",
            inputs.terrain_source.zoom,
            ceiling - 1
        );
    }

    let grid = build_grid(&inputs.height, tile.size_m(), resolution)?;
    let meta = TileMeta {
        tile,
        tile_size_m: tile.size_m(),
        resolution,
        resolution_requested: (resolution < requested).then_some(requested),
        terrain_source_zoom: inputs.terrain_source.zoom,
        imagery_source_zoom: inputs.imagery_source.zoom,
        imagery_attribution: cfg.provider.imagery_attribution.clone(),
        elevation_attribution: cfg.provider.elevation_attribution.clone(),
    };
    let glb = write_glb(&grid, &inputs.jpeg, &meta);
    log::info!(
        "built {tile}: {} vertices, {} triangles, {} bytes (heights z{}, imagery z{}; inputs {:?}, mesh+glb {:?}, {} neighbours)",
        grid.positions.len(),
        grid.triangles(),
        glb.len(),
        inputs.terrain_source.zoom,
        inputs.imagery_source.zoom,
        t1 - t0,
        t1.elapsed(),
        inputs.neighbours_present,
    );
    Ok(glb)
}

fn fetch_closest_logged(
    fetcher: &Fetcher,
    provider: &Provider,
    kind: Kind,
    tile: TileId,
) -> Result<Closest> {
    let c = fetcher.fetch_closest(provider, kind, tile)?;
    log_fetch(kind.name(), c.source, c.origin, c.bytes.len());
    if c.source != tile {
        log::info!(
            "{} {tile}: derived from zoom {} ({})",
            kind.name(),
            c.source.zoom,
            c.source
        );
    }
    Ok(c)
}

/// Imagery bytes as JPEG for `tile`, from the closest provided zoom.
fn fetch_imagery(fetcher: &Fetcher, cfg: &Config, tile: TileId) -> Result<(Vec<u8>, TileId)> {
    let c = fetch_closest_logged(fetcher, &cfg.provider, Kind::Texture, tile)?;
    let (_, qx, qy) = tile.ancestor(c.source.zoom);
    let what = format!("imagery {tile}");
    let jpeg = imagery::derive_from_ancestor(
        &c.bytes,
        tile.zoom - c.source.zoom,
        qx,
        qy,
        cfg.jpeg_quality,
        &what,
    )?;
    Ok((jpeg, c.source))
}

fn log_fetch(what: &str, tile: TileId, origin: Origin, len: usize) {
    match origin {
        Origin::Cache => log::debug!("{what} {tile}: cache hit ({len} bytes)"),
        Origin::Network => log::info!("{what} {tile}: downloaded ({len} bytes)"),
    }
}
