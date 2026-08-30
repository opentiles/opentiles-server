mod common;

use common::*;
use open_tiles::server::{fingerprint, output_key, router, AppState, ServeConfig};
use open_tiles::{build_tile, Config, TileId};
use std::net::SocketAddr;
use std::sync::Arc;

/// Mount the router on an ephemeral port; returns the base URL.
async fn serve(cfg: Config, serve: ServeConfig) -> (String, Arc<AppState>) {
    let state = AppState::new(cfg, serve);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

fn cfg_for(cache: &std::path::Path, srv: &Server) -> Config {
    let mut cfg = Config::with_cache_dir(cache).with_uniform_resolution(17);
    cfg.provider.heightmap_url = format!("{}/h/:zoom:/:x:/:y:.png", srv.base);
    cfg.provider.texture_url = format!("{}/t/:zoom:/:x:/:y:", srv.base);
    cfg.connect_timeout = std::time::Duration::from_millis(500);
    cfg
}

/// Blocking GET on a worker thread (ureq is sync).
async fn get(url: String, etag: Option<String>) -> (u16, Vec<(String, String)>, Vec<u8>) {
    tokio::task::spawn_blocking(move || {
        let mut req = ureq::get(&url);
        if let Some(e) = etag {
            req = req.set("If-None-Match", &e);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        let status = resp.status();
        let headers: Vec<(String, String)> = resp
            .headers_names()
            .into_iter()
            .map(|n| (n.clone(), resp.header(&n).unwrap().to_string()))
            .collect();
        let mut body = Vec::new();
        use std::io::Read;
        resp.into_reader().read_to_end(&mut body).unwrap();
        (status, headers, body)
    })
    .await
    .unwrap()
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_tiles_with_cache_headers_and_output_cache() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    let srv = Server::start(vec![]);
    let cfg = cfg_for(dir.path(), &srv);
    let expected = build_tile(&cfg, centre).unwrap();
    let (base, state) = serve(cfg.clone(), ServeConfig::default()).await;

    let (status, h, body) = get(format!("{base}/10/500/400.glb"), None).await;
    assert_eq!(status, 200);
    assert_eq!(body, expected, "server bytes must equal the CLI's");
    assert_eq!(header(&h, "content-type"), Some("model/gltf-binary"));
    assert_eq!(
        header(&h, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(header(&h, "access-control-allow-origin"), Some("*"));
    let etag = header(&h, "etag").unwrap().to_string();
    assert!(etag.starts_with(&format!("\"{}-", state.fingerprint())));

    // output cache written under the fingerprint
    let cached = dir.path().join(output_key(state.fingerprint(), centre));
    assert_eq!(std::fs::read(&cached).unwrap(), expected);

    // 304 on a matching ETag
    let (status, _, body) = get(format!("{base}/10/500/400.glb"), Some(etag)).await;
    assert_eq!(status, 304);
    assert!(body.is_empty());

    // corrupt the output cache entry: the server must serve what is on disk
    // (it trusts tier 2), proving the second request did not rebuild
    std::fs::write(&cached, b"glTFcached").unwrap();
    let (status, _, body) = get(format!("{base}/10/500/400.glb"), None).await;
    assert_eq!(status, 200);
    assert_eq!(body, b"glTFcached");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_build_once() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    // seed only the neighbours + imagery; the centre heightmap comes from the
    // (slow) provider so the build takes long enough for requests to pile up
    seed_block(dir.path(), centre);
    let own = dir.path().join("heightmap/10/500/400.png");
    let body = std::fs::read(&own).unwrap();
    std::fs::remove_file(&own).unwrap();
    let srv = Server::start_with_delay(
        vec![("/h/10/500/400.png".into(), body)],
        std::time::Duration::from_millis(400),
    );
    let cfg = cfg_for(dir.path(), &srv);
    let (base, _) = serve(cfg, ServeConfig::default()).await;

    let handles: Vec<_> = (0..12)
        .map(|_| tokio::spawn(get(format!("{base}/10/500/400.glb"), None)))
        .collect();
    let mut bodies = Vec::new();
    for h in handles {
        let (status, _, body) = h.await.unwrap();
        assert_eq!(status, 200);
        bodies.push(body);
    }
    assert!(bodies.windows(2).all(|w| w[0] == w[1]));
    let hits = srv.hits.lock().unwrap().clone();
    assert_eq!(
        hits.iter()
            .filter(|h| h.as_str() == "/h/10/500/400.png")
            .count(),
        1,
        "upstream must be asked once: {hits:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn error_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(vec![]);
    let cfg = cfg_for(dir.path(), &srv);
    let (base, _) = serve(cfg, ServeConfig::default()).await;

    let (status, h, body) = get(format!("{base}/3/9/0.glb"), None).await;
    assert_eq!(status, 400);
    assert_eq!(header(&h, "content-type"), Some("application/json"));
    assert!(String::from_utf8_lossy(&body).contains("invalid tile"));

    let (status, h, _) = get(format!("{base}/5/1/1.glb"), None).await;
    assert_eq!(status, 404, "nothing upstream at any zoom");
    assert_eq!(header(&h, "cache-control"), Some("public, max-age=3600"));

    // dead provider → 502
    let dir2 = tempfile::tempdir().unwrap();
    let mut cfg = cfg_for(dir2.path(), &srv);
    cfg.provider.heightmap_url = "http://127.0.0.1:9/h/:zoom:/:x:/:y:.png".into();
    cfg.provider.texture_url = "http://127.0.0.1:9/t/:zoom:/:x:/:y:".into();
    let (base, _) = serve(cfg, ServeConfig::default()).await;
    let (status, h, _) = get(format!("{base}/5/1/1.glb"), None).await;
    assert_eq!(status, 502);
    assert_eq!(header(&h, "cache-control"), Some("no-store"));
}

#[tokio::test(flavor = "multi_thread")]
async fn index_health_and_no_cors() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(vec![]);
    let cfg = cfg_for(dir.path(), &srv);
    let serve_cfg = ServeConfig {
        cors: false,
        ..ServeConfig::default()
    };
    let (base, state) = serve(cfg, serve_cfg).await;
    let (status, h, body) = get(format!("{base}/"), None).await;
    assert_eq!(status, 200);
    assert!(header(&h, "access-control-allow-origin").is_none());
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["tiles"], "/{z}/{x}/{y}.glb");
    assert_eq!(v["fingerprint"], state.fingerprint());
    assert_eq!(v["resolution"][0], 17);
    let (status, _, body) = get(format!("{base}/healthz"), None).await;
    assert_eq!((status, body.as_slice()), (200, &b"ok"[..]));
    assert_eq!(v["example"], "/example/");
    for path in ["/example", "/example/"] {
        let (status, h, body) = get(format!("{base}{path}"), None).await;
        assert_eq!(status, 200);
        assert_eq!(header(&h, "content-type"), Some("text/html; charset=utf-8"));
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("GLTFLoader") && html.contains("/${zoom}/${x0 + dx}/${y0 + dy}.glb"));
    }
}

#[test]
fn fingerprint_tracks_the_config() {
    let a = Config::default();
    let b = Config::default().with_uniform_resolution(33);
    let mut c = Config::default();
    c.provider.texture_max_zoom = 18;
    let (fa, fb, fc) = (fingerprint(&a), fingerprint(&b), fingerprint(&c));
    assert_eq!(fa.len(), 16);
    assert_eq!(fa, fingerprint(&Config::default()));
    assert_ne!(fa, fb);
    assert_ne!(fa, fc);
    // cache location and timeouts don't change the bytes, so not the fingerprint
    let d = Config {
        read_timeout: std::time::Duration::from_secs(99),
        ..Config::with_cache_dir("/elsewhere")
    };
    assert_eq!(fa, fingerprint(&d));
}
