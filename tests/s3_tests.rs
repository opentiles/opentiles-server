//! S3 backend, against a real bucket. Skipped unless `OPEN_TILES_TEST_S3`
//! names one (`s3://bucket[/prefix]`); the usual AWS environment supplies
//! region, credentials and — for MinIO / LocalStack — `AWS_ENDPOINT_URL`:
//!
//! ```sh
//! docker run -d -p 9000:9000 -e MINIO_ROOT_USER=test -e MINIO_ROOT_PASSWORD=testtest minio/minio server /data
//! AWS_ENDPOINT_URL=http://127.0.0.1:9000 AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=testtest \
//!   AWS_REGION=us-east-1 OPEN_TILES_TEST_S3=s3://open-tiles-test/ci cargo test --test s3_tests
//! ```
//!
//! The bucket is created if missing; every test works under its own unique
//! prefix and deletes what it wrote.

#![cfg(feature = "s3")]

mod common;

use common::*;
use open_tiles::fetch::{Fetcher, Origin};
use open_tiles::provider::Kind;
use open_tiles::server::{output_key, router, AppState, ServeConfig};
use open_tiles::store::{S3Store, Store};
use open_tiles::{build_tile, Config, Error, TileId};
use std::sync::Arc;
use std::time::Duration;

/// A store under a fresh prefix, or `None` (with a note) when the
/// environment has no bucket. Dropping it removes everything it holds.
struct Scratch {
    store: Arc<S3Store>,
}

impl Scratch {
    fn open(test: &str) -> Option<Self> {
        let base = match std::env::var("OPEN_TILES_TEST_S3") {
            Ok(v) if v.starts_with("s3://") => v,
            _ => {
                eprintln!("OPEN_TILES_TEST_S3 not set: skipping S3 test");
                return None;
            }
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let url = format!(
            "{}/{test}-{}-{nanos}",
            base.trim_end_matches('/'),
            std::process::id()
        );
        let store = S3Store::from_url(&url).unwrap();
        // on a plain thread: `block_on` is not allowed inside #[tokio::test]
        let bucket = store.bucket().to_string();
        std::thread::spawn(move || ensure_bucket(&bucket))
            .join()
            .unwrap();
        Some(Self {
            store: Arc::new(store),
        })
    }

    fn dyn_store(&self) -> Arc<dyn Store> {
        self.store.clone()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(keys) = self.store.list("") {
            for k in keys {
                let _ = self.store.delete(&k);
            }
        }
    }
}

/// Create the test bucket when it does not exist (MinIO / LocalStack start
/// empty). Uses the SDK directly: the store itself never creates buckets.
fn ensure_bucket(bucket: &str) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let sdk = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let cfg = aws_sdk_s3::config::Builder::from(&sdk)
            .force_path_style(true)
            .build();
        let client = aws_sdk_s3::Client::from_conf(cfg);
        if client.head_bucket().bucket(bucket).send().await.is_ok() {
            return;
        }
        // tests run in parallel: losing the race to create it is fine
        if let Err(e) = client.create_bucket().bucket(bucket).send().await {
            if client.head_bucket().bucket(bucket).send().await.is_err() {
                panic!(
                    "creating bucket {bucket}: {}",
                    aws_sdk_s3::error::DisplayErrorContext(&e)
                );
            }
        }
    });
}

#[test]
fn store_round_trip_under_a_prefix() {
    let Some(s) = Scratch::open("store") else {
        return;
    };
    let st = &*s.store;
    assert_eq!(st.get("a/b/c.png").unwrap(), None);
    assert!(!st.exists("a/b/c.png").unwrap());
    st.put("a/b/c.png", b"one").unwrap();
    st.put("a/b/c.png.404", b"").unwrap();
    st.put("a/d.glb", b"two").unwrap();
    assert_eq!(st.get("a/b/c.png").unwrap().as_deref(), Some(&b"one"[..]));
    assert!(st.exists("a/b/c.png.404").unwrap(), "empty objects exist");
    assert_eq!(
        st.get("a/b/c.png.404").unwrap().as_deref(),
        Some(&b""[..]),
        "an empty object reads as Some(empty), not as missing"
    );
    // keys come back relative to the store's prefix
    assert_eq!(
        st.list("a/").unwrap(),
        vec!["a/b/c.png", "a/b/c.png.404", "a/d.glb"]
    );
    assert_eq!(st.list("a/b/").unwrap(), vec!["a/b/c.png", "a/b/c.png.404"]);
    assert_eq!(st.list("nope/").unwrap(), Vec::<String>::new());
    st.delete("a/b/c.png.404").unwrap();
    st.delete("a/b/c.png.404").unwrap(); // idempotent
    assert_eq!(st.list("").unwrap(), vec!["a/b/c.png", "a/d.glb"]);
    // overwrite is a full replacement
    st.put("a/d.glb", b"three").unwrap();
    assert_eq!(st.get("a/d.glb").unwrap().as_deref(), Some(&b"three"[..]));
    assert!(st.location().starts_with("s3://"));
    assert!(st.object_key("x").ends_with("/x") && !st.object_key("/x").contains("//"));
}

#[test]
fn fetcher_writes_through_and_remembers_404s() {
    let Some(s) = Scratch::open("fetch") else {
        return;
    };
    let body = terrarium_png(|_, _| 12.0);
    let srv = Server::start(vec![("/h/5/3/4.png".into(), body.clone())]);
    let f = Fetcher::new(
        s.dyn_store(),
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    let t = TileId::new(5, 3, 4).unwrap();
    let url = format!("{}/h/5/3/4.png", srv.base);

    let (b1, o1) = f.fetch(Kind::Heightmap, t, &url).unwrap();
    assert_eq!((o1, &b1), (Origin::Network, &body));
    assert_eq!(
        s.store.get("heightmap/5/3/4.png").unwrap(),
        Some(body.clone())
    );
    let (b2, o2) = f.fetch(Kind::Heightmap, t, &url).unwrap();
    assert_eq!((o2, &b2), (Origin::Cache, &body));
    assert_eq!(
        srv.hits.lock().unwrap().len(),
        1,
        "second call is a cache hit"
    );

    // 404 → marker object; the marker short-circuits the next attempt
    let missing = TileId::new(5, 3, 5).unwrap();
    let murl = format!("{}/h/5/3/5.png", srv.base);
    assert!(matches!(
        f.fetch(Kind::Heightmap, missing, &murl),
        Err(Error::NotFound { .. })
    ));
    assert!(s.store.exists("heightmap/5/3/5.png.404").unwrap());
    assert!(!s.store.exists("heightmap/5/3/5.png").unwrap());
    assert!(matches!(
        f.fetch(Kind::Heightmap, missing, &murl),
        Err(Error::NotFound { .. })
    ));
    assert_eq!(srv.hits.lock().unwrap().len(), 2);
    assert_eq!(
        f.clear_missing_markers(Some(Kind::Heightmap), Some(5))
            .unwrap(),
        1
    );
    assert!(!s.store.exists("heightmap/5/3/5.png.404").unwrap());
    assert_eq!(f.clear_missing_markers(None, None).unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn server_builds_from_and_publishes_to_s3() {
    let Some(s) = Scratch::open("serve") else {
        return;
    };
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block_into(&*s.store, centre);
    // no provider: every input must come from the bucket
    let mut cfg = Config {
        cache: s.dyn_store(),
        ..Config::default()
    }
    .with_uniform_resolution(17);
    cfg.provider.heightmap_url = "http://127.0.0.1:9/h/:zoom:/:x:/:y:.png".into();
    cfg.provider.texture_url = "http://127.0.0.1:9/t/:zoom:/:x:/:y:".into();
    cfg.connect_timeout = Duration::from_millis(200);
    let expected = build_tile(&cfg, centre).unwrap();

    let state = AppState::new(cfg, ServeConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let get = |path: String| {
        let url = format!("{base}{path}");
        tokio::task::spawn_blocking(move || {
            let resp = ureq::get(&url).call().unwrap();
            let status = resp.status();
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).unwrap();
            (status, body)
        })
    };
    let (status, body) = get("/10/500/400.glb".to_string()).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, expected);
    let key = output_key(state.fingerprint(), centre);
    assert_eq!(
        s.store.get(&key).unwrap(),
        Some(expected.clone()),
        "published to the bucket"
    );

    // a second server sharing the bucket serves the cached object without building:
    // prove it by replacing the object and reading it back through the server
    s.store.put(&key, b"glTFfromS3").unwrap();
    let (status, body) = get("/10/500/400.glb".to_string()).await.unwrap();
    assert_eq!((status, body.as_slice()), (200, &b"glTFfromS3"[..]));
}
