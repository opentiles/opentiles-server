//! Cache-or-HTTP raw bytes with atomic write-through, a negative cache for
//! 404s, and the "closest provided zoom" walk-down.
//!
//! Cache layout is `{cache_dir}/{texture|heightmap}/{zoom}/{x}/{y}.png` —
//! byte-compatible with raytiles' and bevytiles' on-disk caches (imagery is
//! stored under `.png` even when the provider sent JPEG; readers sniff the
//! format, exactly as the engines do). A provider 404 leaves a zero-byte
//! `{y}.png.404` marker next to the would-be entry so the walk-down never
//! repeats a known-missing request; the engines ignore unknown files.
//!
//! Ported from bevytiles `source/native.rs` after review; differences:
//! - HTTP 404 is a typed [`Error::NotFound`] (the server will map it to its
//!   own 404; the walk-down uses it to step to the next lower zoom).
//! - The atomic-write temp name includes the process id: the CLI may run
//!   next to an engine sharing the same cache, and a counter alone is only
//!   unique within one process.
//! - An empty cached file is treated as a miss and re-fetched (an interrupted
//!   writer can't produce one thanks to the rename, but a foreign tool can).
//! - Negative cache markers (the engines never fall back, so never needed one).

use crate::provider::{Kind, Provider};
use crate::tile::{TileId, MIN_ZOOM};
use crate::{Error, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Refuse to buffer more than this from one response (a tile is ~100 KB).
const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// Suffix of the negative-cache marker, appended to the entry's file name.
pub const MISSING_MARKER_SUFFIX: &str = ".404";

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

    /// Raw bytes for `url`, cached at the path for `(kind, tile)`. A real
    /// cache entry wins over a stale `.404` marker (an engine may have fetched
    /// it since); a marker short-circuits to [`Error::NotFound`].
    pub fn fetch(&self, kind: Kind, tile: TileId, url: &str) -> Result<(Vec<u8>, Origin)> {
        let path = self.cache_path(kind, tile);
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => return Ok((bytes, Origin::Cache)),
            Ok(_) => log::warn!("empty cache entry {}, refetching", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path, e)),
        }
        let marker = marker_path(&path);
        if marker.exists() {
            log::debug!("{} {tile}: known missing (marker)", kind.name());
            return Err(Error::NotFound { url: url.into() });
        }
        match self.download(url) {
            Ok(bytes) => {
                write_atomic(&path, &bytes)?;
                Ok((bytes, Origin::Network))
            }
            Err(e @ Error::NotFound { .. }) => {
                write_atomic(&marker, &[])?;
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
            let root = self.cache_dir.join(k.dir());
            let zoom_dirs: Vec<PathBuf> = match zoom {
                Some(z) => vec![root.join(z.to_string())],
                None => read_dir_sorted(&root)?,
            };
            for zdir in zoom_dirs {
                for xdir in read_dir_sorted(&zdir)? {
                    for f in read_dir_sorted(&xdir)? {
                        if f.to_string_lossy().ends_with(MISSING_MARKER_SUFFIX) {
                            std::fs::remove_file(&f).map_err(|e| io_err(&f, e))?;
                            removed += 1;
                        }
                    }
                }
            }
        }
        Ok(removed)
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

/// `{entry}.404` next to a cache entry.
pub fn marker_path(entry: &Path) -> PathBuf {
    let mut s = entry.as_os_str().to_owned();
    s.push(MISSING_MARKER_SUFFIX);
    PathBuf::from(s)
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            v.sort();
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io_err(dir, e)),
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
