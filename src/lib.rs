//! # open-tiles
//!
//! Builds ready-to-render 3D terrain tiles (glTF binary, `.glb`) from
//! Mapzen Terrarium heightmaps and satellite imagery, at 1:1 world scale,
//! addressed by slippy-map `zoom/x/y`.
//!
//! The library is the builder plus the HTTP server ([`server`]); the
//! `open-tiles` binary wraps both (`build`, `serve`, `lookup`, `refresh-404`).
//!
//! ## Conventions
//!
//! - Right-handed, **Y-up, metres**. `+X` east, `+Z` south (slippy `y`
//!   increases southward). Origin at the tile's north-west corner; the tile
//!   spans `[0, size]` in X and Z, where `size` is the tile edge in metres at
//!   the tile's own centre latitude.
//! - Vertex **Y is metres above sea level**, unscaled. `Y = 0` is the same
//!   sea-level plane in every tile, so placing tile `(x, y)` on a uniform grid
//!   is `translation = (x·size, 0, y·size)` — X/Z only.
//! - Tile edges are watertight against same-zoom neighbours: boundary
//!   vertices are sampled over a height field padded with the neighbours'
//!   edge texels, so both sides evaluate to the same value.
//! - No normals, no skirts. Viewers compute flat normals; LOD seams are the
//!   client's problem.
//! - Any zoom 1–22. When a provider has nothing at the requested zoom, the
//!   asset comes from the closest lower zoom that exists: heights by sampling
//!   a window of the ancestor's field, imagery by crop-and-upscale.
//!
//! ## Quick start
//!
//! ```no_run
//! use open_tiles::{build_tile, Config, TileId};
//!
//! let cfg = Config::default();
//! let tile = TileId::new(12, 772, 1607).unwrap(); // Grand Canyon
//! let glb: Vec<u8> = build_tile(&cfg, tile).unwrap();
//! std::fs::write("12-772-1607.glb", glb).unwrap();
//! ```

#![warn(missing_docs)]

pub mod builder;
pub mod fetch;
pub mod glb;
pub mod imagery;
pub mod mesh;
pub mod provider;
pub mod server;
pub mod store;
pub mod terrain;
pub mod tile;

pub use builder::{build_tile, load_inputs, Config, TileInputs};
pub use provider::Provider;
pub use store::Store;
pub use tile::{TileId, MAX_ZOOM, MIN_ZOOM};

/// Everything that can go wrong while building a tile.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Zoom outside `[MIN_ZOOM, MAX_ZOOM]` or `x`/`y` outside `[0, 2^zoom)`.
    #[error("invalid tile: {0}")]
    InvalidTile(String),
    /// Invalid mesh resolution (vertices per edge).
    #[error("resolution must be in 2..=257 vertices per edge, got {0}")]
    InvalidResolution(u32),
    /// The provider answered 404 for a required asset.
    #[error("not found upstream: {url}")]
    NotFound {
        /// The URL that 404'd.
        url: String,
    },
    /// Any other HTTP failure (status, timeout, connection).
    #[error("fetch {url}: {reason}")]
    Fetch {
        /// The URL being fetched.
        url: String,
        /// Human-readable cause.
        reason: String,
    },
    /// Bytes that were not a decodable image.
    #[error("decode {what}: {reason}")]
    Decode {
        /// Which asset failed.
        what: String,
        /// Decoder message.
        reason: String,
    },
    /// Cache failure (filesystem or object store read/write).
    #[error("io {path}: {source}")]
    Io {
        /// The path or object URL involved.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
