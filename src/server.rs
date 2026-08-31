//! The HTTP server: `GET /{z}/{x}/{y}.glb`, built on first request, served
//! from the output cache afterwards, one build per tile at a time.
//!
//! Two-tier cache, both tiers in the one [`Store`](crate::store::Store) of
//! the [`Config`]: the builder's input cache (`{texture,heightmap}/…`) and
//! this module's output cache `glb/{fingerprint}/{z}/{x}/{y}.glb`.
//! The fingerprint hashes everything that changes the bytes — crate version,
//! resolution table, provider URLs and zoom hints, JPEG quality — so a config
//! change can never serve stale geometry.
//!
//! Dedup: the first request for a tile is the *leader* — it takes a build
//! permit, builds under `spawn_blocking`, publishes the tile to the cache,
//! removes the in-flight entry and publishes the outcome on a `watch`
//! channel; concurrent requests for the same tile wait on that channel.
//! Failures are published too, so waiters fail fast instead of each retrying
//! upstream. Requests arriving after publication hit the cache.

use crate::builder::{build_tile, Config};
use crate::tile::TileId;
use crate::Error;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Semaphore};

/// Server-only settings (the builder's live in [`Config`]).
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Address to listen on.
    pub bind: SocketAddr,
    /// Concurrent tile builds (each fans out ~10 upstream fetches).
    pub max_builds: usize,
    /// Send `Access-Control-Allow-Origin: *`.
    pub cors: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".parse().expect("static addr"),
            max_builds: std::thread::available_parallelism().map_or(4, |n| n.get()),
            cors: true,
        }
    }
}

/// Hash of everything that influences a tile's bytes; names the output-cache
/// directory and prefixes ETags.
pub fn fingerprint(cfg: &Config) -> String {
    let mut h = blake3::Hasher::new();
    h.update(env!("CARGO_PKG_VERSION").as_bytes());
    for r in cfg.resolution {
        h.update(&r.to_le_bytes());
    }
    let p = &cfg.provider;
    for s in [&p.texture_url, &p.heightmap_url] {
        h.update(s.as_bytes());
        h.update(b"\0");
    }
    h.update(&[p.texture_max_zoom, p.heightmap_max_zoom, cfg.jpeg_quality]);
    h.finalize().to_hex()[..16].to_string()
}

/// Output-cache key of a tile under `fingerprint`.
pub fn output_key(fingerprint: &str, tile: TileId) -> String {
    format!("glb/{fingerprint}/{}/{}/{}.glb", tile.zoom, tile.x, tile.y)
}

/// What one build produced — the GLB bytes or the error to report — shared
/// between the build's leader and every waiter (hence the `Arc`s).
type Outcome = Result<Arc<[u8]>, Arc<Error>>;

/// Shared state behind the handlers.
pub struct AppState {
    /// Builder configuration: providers, cache, resolution table.
    cfg: Arc<Config>,
    /// Server-only settings: bind address, CORS, build concurrency.
    serve: ServeConfig,
    /// Output-cache namespace and ETag prefix, computed once from `cfg`.
    fingerprint: String,
    /// One entry per tile currently being built. A request finding its tile
    /// here subscribes to the leader's channel instead of building again.
    inflight: Mutex<HashMap<TileId, watch::Receiver<Option<Outcome>>>>,
    /// Bounds how many *different* tiles build at once (`max_builds`).
    builds: Semaphore,
}

impl AppState {
    /// Build the state for [`router`].
    pub fn new(cfg: Config, serve: ServeConfig) -> Arc<Self> {
        let fingerprint = fingerprint(&cfg);
        Arc::new(Self {
            cfg: Arc::new(cfg),
            builds: Semaphore::new(serve.max_builds.max(1)),
            serve,
            fingerprint,
            inflight: Mutex::new(HashMap::new()),
        })
    }

    /// The output-cache fingerprint in use.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The tile bytes: output cache, or a (deduplicated) build.
    pub async fn tile(self: &Arc<Self>, tile: TileId) -> Outcome {
        let key = output_key(&self.fingerprint, tile);
        let (store, k) = (self.cfg.cache.clone(), key.clone());
        match tokio::task::spawn_blocking(move || store.get(&k)).await {
            Ok(Ok(Some(bytes))) if !bytes.is_empty() => {
                log::debug!("{tile}: output cache hit");
                return Ok(bytes.into());
            }
            Ok(Ok(_)) => {}
            // an unreadable cache is a reason to rebuild, not to fail the request
            Ok(Err(e)) => log::warn!("{tile}: output cache read failed, rebuilding: {e}"),
            Err(join) => log::warn!("{tile}: output cache read panicked, rebuilding: {join}"),
        }

        // join an in-flight build, or become its leader
        let (tx, mut rx) = {
            let mut map = self.inflight.lock().expect("inflight poisoned");
            if let Some(rx) = map.get(&tile) {
                (None, rx.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                map.insert(tile, rx.clone());
                (Some(tx), rx)
            }
        };

        if let Some(tx) = tx {
            let outcome = self.lead_build(tile, key).await;
            self.inflight
                .lock()
                .expect("inflight poisoned")
                .remove(&tile);
            let _ = tx.send(Some(outcome.clone()));
            return outcome;
        }

        log::debug!("{tile}: joining in-flight build");
        let waited = rx
            .wait_for(|v| v.is_some())
            .await
            .map(|v| (*v).clone().expect("checked is_some"));
        match waited {
            Ok(outcome) => outcome,
            // the leader vanished (panic); tell the client rather than hang
            Err(_) => Err(Arc::new(Error::Fetch {
                url: tile.to_string(),
                reason: "build task ended without a result".into(),
            })),
        }
    }

    /// Build `tile` as the sole leader and publish the bytes at `key` in
    /// the output cache. Takes a build permit first (the global concurrency
    /// bound) and runs the synchronous builder under `spawn_blocking` so
    /// the async workers stay free.
    async fn lead_build(&self, tile: TileId, key: String) -> Outcome {
        let _permit = self.builds.acquire().await.expect("semaphore closed");
        let cfg = self.cfg.clone();
        let started = std::time::Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            let glb = build_tile(&cfg, tile)?;
            cfg.cache.put(&key, &glb)?;
            Ok::<_, Error>(glb)
        })
        .await;
        match res {
            Ok(Ok(glb)) => {
                log::info!(
                    "{tile}: built {} bytes in {:?}",
                    glb.len(),
                    started.elapsed()
                );
                Ok(glb.into())
            }
            Ok(Err(e)) => {
                log::warn!("{tile}: build failed: {e}");
                Err(Arc::new(e))
            }
            Err(join) => Err(Arc::new(Error::Fetch {
                url: tile.to_string(),
                reason: format!("build task panicked: {join}"),
            })),
        }
    }
}

/// The application router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/example", get(example))
        .route("/example/", get(example))
        // the router can't split `{y}.glb` inside one segment; the handler does
        .route("/{z}/{x}/{y}", get(tile))
        .with_state(state)
}

/// Serve until Ctrl-C.
pub async fn run(cfg: Config, serve: ServeConfig) -> std::io::Result<()> {
    let bind = serve.bind;
    let state = AppState::new(cfg, serve);
    log::info!(
        "open-tiles {} listening on http://{bind}  (fingerprint {}, {} concurrent builds, cache {})",
        env!("CARGO_PKG_VERSION"),
        state.fingerprint,
        state.serve.max_builds,
        state.cfg.cache.location()
    );
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            log::info!("shutting down");
        })
        .await
}

// -- handlers -------------------------------------------------------------------

/// `GET /` — the service description: name, version, fingerprint, tile URL
/// template, zoom range, resolution table, frame conventions, attribution.
async fn index(State(state): State<Arc<AppState>>) -> Response {
    let cfg = &state.cfg;
    let body = json!({
        "name": "open-tiles",
        "version": env!("CARGO_PKG_VERSION"),
        "fingerprint": state.fingerprint,
        "tiles": "/{z}/{x}/{y}.glb",
        "example": "/example/",
        "zoom": { "min": crate::tile::MIN_ZOOM, "max": crate::tile::MAX_ZOOM },
        "resolution": cfg.resolution,
        "conventions": {
            "units": "metres", "up": "+Y", "origin": "north-west corner; +X east, +Z south",
            "y": "metres above sea level; place tiles with an X/Z translation only",
        },
        "sources": {
            "imagery": cfg.provider.imagery_attribution,
            "elevation": cfg.provider.elevation_attribution,
        },
    });
    finish(
        state.serve.cors,
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body.to_string(),
        )
            .into_response(),
    )
}

/// The bundled three.js viewer (`example/index.html`), embedded at compile
/// time so `open-tiles serve` is a complete demo on its own.
async fn example(State(state): State<Arc<AppState>>) -> Response {
    finish(
        state.serve.cors,
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            include_str!("../example/index.html"),
        )
            .into_response(),
    )
}

/// `GET`/`HEAD` `/{z}/{x}/{y}.glb` — parse the address, get the bytes from
/// [`AppState::tile`] (cache hit or deduplicated build), then handle the
/// HTTP niceties: ETag / `If-None-Match` → 304, bodyless HEAD, immutable
/// cache-control, and the error → status mapping.
async fn tile(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(String, String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let cors = state.serve.cors;
    let tile = match parse_tile_path(&z, &x, &y) {
        Ok(t) => t,
        Err(msg) => {
            return finish(
                cors,
                error_response(StatusCode::BAD_REQUEST, &msg, "no-store"),
            )
        }
    };
    match state.tile(tile).await {
        Ok(bytes) => {
            let etag = format!("\"{}-{}\"", state.fingerprint, bytes.len());
            let matches = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
            let mut resp = if matches {
                StatusCode::NOT_MODIFIED.into_response()
            } else if method == Method::HEAD {
                (StatusCode::OK, Body::empty()).into_response()
            } else {
                (StatusCode::OK, Body::from(bytes.to_vec())).into_response()
            };
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("model/gltf-binary"),
            );
            h.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            h.insert(
                header::ETAG,
                HeaderValue::from_str(&etag).expect("hex etag"),
            );
            if !matches {
                h.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
            }
            finish(cors, resp)
        }
        Err(e) => {
            let (status, cache) = match *e {
                Error::InvalidTile(_) | Error::InvalidResolution(_) => {
                    (StatusCode::BAD_REQUEST, "no-store")
                }
                Error::NotFound { .. } => (StatusCode::NOT_FOUND, "public, max-age=3600"),
                Error::Fetch { .. } => (StatusCode::BAD_GATEWAY, "no-store"),
                Error::Decode { .. } | Error::Io { .. } => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "no-store")
                }
            };
            finish(cors, error_response(status, &e.to_string(), cache))
        }
    }
}

/// `("12", "772", "1607.glb")` → the tile; anything else is a 400 message.
fn parse_tile_path(z: &str, x: &str, y: &str) -> Result<TileId, String> {
    let y = y
        .strip_suffix(".glb")
        .ok_or_else(|| "expected /{z}/{x}/{y}.glb".to_string())?;
    let num = |s: &str, what: &str| {
        s.parse::<u32>()
            .map_err(|_| format!("{what} must be a non-negative integer, got {s:?}"))
    };
    let z = num(z, "zoom")?;
    let (x, y) = (num(x, "x")?, num(y, "y")?);
    let z = u8::try_from(z).map_err(|_| format!("zoom {z} out of range"))?;
    TileId::new(z, x, y).map_err(|e| e.to_string())
}

/// A JSON error body `{"error", "status"}` with the given cache policy.
fn error_response(status: StatusCode, message: &str, cache: &'static str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, cache),
        ],
        json!({ "error": message, "status": status.as_u16() }).to_string(),
    )
        .into_response()
}

/// Last step of every handler: attach `Access-Control-Allow-Origin: *`
/// unless CORS was disabled.
fn finish(cors: bool, mut resp: Response) -> Response {
    if cors {
        resp.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tile_paths() {
        assert_eq!(
            parse_tile_path("12", "772", "1607.glb").unwrap(),
            TileId::new(12, 772, 1607).unwrap()
        );
        assert!(parse_tile_path("12", "772", "1607").is_err());
        assert!(parse_tile_path("12", "772", "1607.gltf").is_err());
        assert!(parse_tile_path("-1", "0", "0.glb").is_err());
        assert!(parse_tile_path("300", "0", "0.glb").is_err());
        assert!(parse_tile_path("3", "9", "0.glb").is_err());
    }
}
