//! Cache-or-HTTP raw bytes with write-through, a negative cache for 404s,
//! and the "closest provided zoom" walk-down.
//!
//! Cache entries are keyed `{texture|heightmap}/{zoom}/{x}/{y}.png` in a
//! [`Store`] — on a [`LocalStore`](crate::store::LocalStore) that is
//! byte-compatible with raytiles' and bevytiles' on-disk caches (imagery is
//! stored under `.png` even when the provider sent JPEG; readers sniff the
//! format, exactly as the engines do). A provider 404 leaves a zero-byte
//! `{y}.png.404` marker next to the would-be entry so the walk-down never
//! repeats a known-missing request; the engines ignore unknown files.
//!
//! Ported from bevytiles `source/native.rs` after review; differences:
//! - HTTP 404 is a typed [`Error::NotFound`] (the server will map it to its
//!   own 404; the walk-down uses it to step to the next lower zoom).
//! - Writes go through [`Store::put`], which is atomic on every backend.
//! - An empty cached entry is treated as a miss and re-fetched (an
//!   interrupted writer can't produce one, but a foreign tool can).
//! - Negative cache markers (the engines never fall back, so never needed one).

use crate::provider::{Kind, Provider};
use crate::store::Store;
use crate::tile::{TileId, MIN_ZOOM};
use crate::{Error, Result};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

pub use crate::store::write_atomic;

/// Refuse to buffer more than this from one response (a tile is ~100 KB).
const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// Suffix of the negative-cache marker, appended to the entry's key.
pub const MISSING_MARKER_SUFFIX: &str = ".404";

/// Blocking fetcher: one HTTP agent (keep-alive pool) + one cache store.
/// Cheap to clone; safe to share across threads.
#[derive(Clone)]
pub struct Fetcher {
    /// Shared HTTP agent — one keep-alive connection pool for all fetches.
    agent: ureq::Agent,
    /// Where fetched bytes are read from and written through to.
    store: Arc<dyn Store>,
}

/// Where the bytes came from — for logging and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Read from the cache.
    Cache,
    /// Downloaded (and written through to the cache).
    Network,
}

/// Result of a walk-down: the bytes and the tile they belong to (which may
/// be an ancestor of the one asked for).
pub struct Closest {
    /// Raw bytes of the asset.
    pub bytes: Vec<u8>,
    /// The tile the bytes are for (`source.zoom <= requested.zoom`).
    pub source: TileId,
    /// Cache or network.
    pub origin: Origin,
}

impl Fetcher {
    /// Build a fetcher over `store` with the given HTTP timeouts.
    pub fn new(store: Arc<dyn Store>, connect_timeout: Duration, read_timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(connect_timeout)
            .timeout_read(read_timeout)
            .user_agent(concat!("open-tiles/", env!("CARGO_PKG_VERSION")))
            .build();
        Self { agent, store }
    }

    /// The cache behind this fetcher.
    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    /// Cache key for one asset of one tile.
    pub fn cache_key(kind: Kind, tile: TileId) -> String {
        format!("{}/{}/{}/{}.png", kind.dir(), tile.zoom, tile.x, tile.y)
    }

    /// Raw bytes for `url`, cached under the key for `(kind, tile)`. A real
    /// cache entry wins over a stale `.404` marker (an engine may have fetched
    /// it since); a marker short-circuits to [`Error::NotFound`].
    pub fn fetch(&self, kind: Kind, tile: TileId, url: &str) -> Result<(Vec<u8>, Origin)> {
        let key = Self::cache_key(kind, tile);
        match self.store.get(&key)? {
            Some(bytes) if !bytes.is_empty() => return Ok((bytes, Origin::Cache)),
            Some(_) => log::warn!("empty cache entry {key}, refetching"),
            None => {}
        }
        let marker = marker_key(&key);
        if self.store.exists(&marker)? {
            log::debug!("{} {tile}: known missing (marker)", kind.name());
            return Err(Error::NotFound { url: url.into() });
        }
        match self.download(url) {
            Ok(bytes) => {
                self.store.put(&key, &bytes)?;
                Ok((bytes, Origin::Network))
            }
            Err(e @ Error::NotFound { .. }) => {
                self.store.put(&marker, &[])?;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// The asset for `tile`, or the closest lower zoom's: start at
    /// `min(tile.zoom, provider hint)` and walk down on 404. Any other error
    /// aborts the walk — a transient failure must never change which zoom a
    /// tile is derived from.
    pub fn fetch_closest(&self, provider: &Provider, kind: Kind, tile: TileId) -> Result<Closest> {
        let start = tile.zoom.min(provider.max_zoom(kind)).max(MIN_ZOOM);
        let mut last_url = String::new();
        for zoom in (MIN_ZOOM..=start).rev() {
            let (candidate, _, _) = tile.ancestor(zoom);
            let url = provider.url(kind, candidate);
            match self.fetch(kind, candidate, &url) {
                Ok((bytes, origin)) => {
                    return Ok(Closest {
                        bytes,
                        source: candidate,
                        origin,
                    })
                }
                Err(Error::NotFound { url }) => last_url = url,
                Err(e) => return Err(e),
            }
        }
        Err(Error::NotFound { url: last_url })
    }

    /// Delete `.404` markers for `kind` (or both kinds) at `zoom` (or every
    /// zoom). Returns how many were removed.
    pub fn clear_missing_markers(&self, kind: Option<Kind>, zoom: Option<u8>) -> Result<usize> {
        let kinds: Vec<Kind> = match kind {
            Some(k) => vec![k],
            None => vec![Kind::Texture, Kind::Heightmap],
        };
        let mut removed = 0;
        for k in kinds {
            let prefix = match zoom {
                Some(z) => format!("{}/{z}/", k.dir()),
                None => format!("{}/", k.dir()),
            };
            for key in self.store.list(&prefix)? {
                if key.ends_with(MISSING_MARKER_SUFFIX) {
                    self.store.delete(&key)?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// GET `url` and buffer the whole body (capped at [`MAX_RESPONSE_BYTES`]).
    /// HTTP 404 becomes the typed [`Error::NotFound`] the walk-down relies
    /// on; any other status, transport failure or empty body is
    /// [`Error::Fetch`].
    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let resp = match self.agent.get(url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(404, _)) => {
                return Err(Error::NotFound { url: url.into() });
            }
            Err(ureq::Error::Status(code, r)) => {
                return Err(Error::Fetch {
                    url: url.into(),
                    reason: format!("HTTP {code} {}", r.status_text()),
                });
            }
            Err(e) => {
                return Err(Error::Fetch {
                    url: url.into(),
                    reason: e.to_string(),
                });
            }
        };
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Fetch {
                url: url.into(),
                reason: format!("read body: {e}"),
            })?;
        if bytes.is_empty() {
            return Err(Error::Fetch {
                url: url.into(),
                reason: "empty body".into(),
            });
        }
        Ok(bytes)
    }
}

/// `{entry}.404` next to a cache entry.
pub fn marker_key(entry: &str) -> String {
    format!("{entry}{MISSING_MARKER_SUFFIX}")
}
