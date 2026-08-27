//! Cache-or-HTTP raw bytes with atomic write-through.
//!
//! Cache layout is `{cache_dir}/{texture|heightmap}/{zoom}/{x}/{y}.png` —
//! byte-compatible with raytiles' and bevytiles' on-disk caches (imagery is
//! stored under `.png` even when the provider sent JPEG; readers sniff the
//! format, exactly as the engines do).
//!
//! Ported from bevytiles `source/native.rs` after review; differences:
//! - HTTP 404 is a typed [`Error::NotFound`] (the server will map it to its
//!   own 404; neighbours treat it as "dataset edge") instead of a string.
//! - The atomic-write temp name includes the process id: the CLI may run
//!   next to an engine sharing the same cache, and a counter alone is only
//!   unique within one process.
//! - An empty cached file is treated as a miss and re-fetched (an interrupted
//!   writer can't produce one thanks to the rename, but a foreign tool can).

use crate::provider::Kind;
use crate::tile::TileId;
use crate::{Error, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Refuse to buffer more than this from one response (a tile is ~100 KB).
const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// Blocking fetcher: one HTTP agent (keep-alive pool) + one cache root.
/// Cheap to clone; safe to share across threads.
#[derive(Clone)]
pub struct Fetcher {
    agent: ureq::Agent,
    cache_dir: PathBuf,
}

/// Where the bytes came from — for logging and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Read from the on-disk cache.
    Cache,
    /// Downloaded (and written through to the cache).
    Network,
}

impl Fetcher {
    /// Build a fetcher rooted at `cache_dir` with the given HTTP timeouts.
    pub fn new(
        cache_dir: impl Into<PathBuf>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(connect_timeout)
            .timeout_read(read_timeout)
            .user_agent(concat!("open-tiles/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent,
            cache_dir: cache_dir.into(),
        }
    }

    /// Cache path for one asset of one tile.
    pub fn cache_path(&self, kind: Kind, tile: TileId) -> PathBuf {
        self.cache_dir
            .join(kind.dir())
            .join(tile.zoom.to_string())
            .join(tile.x.to_string())
            .join(format!("{}.png", tile.y))
    }

    /// Raw bytes for `url`, cached at the path for `(kind, tile)`.
    pub fn fetch(&self, kind: Kind, tile: TileId, url: &str) -> Result<(Vec<u8>, Origin)> {
        let path = self.cache_path(kind, tile);
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => return Ok((bytes, Origin::Cache)),
            Ok(_) => log::warn!("empty cache entry {}, refetching", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path, e)),
        }
        let bytes = self.download(url)?;
        write_atomic(&path, &bytes)?;
        Ok((bytes, Origin::Network))
    }

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

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Write `bytes` to `path` atomically: unique temp file in the same
/// directory, then rename. Concurrent writers of the same path race on the
/// rename only, which is benign (identical bytes).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, bytes).map_err(|e| io_err(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_err(path, e)
    })
}
