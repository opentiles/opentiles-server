mod common;

use common::*;
use open_tiles::fetch::{Fetcher, Origin};
use open_tiles::provider::Kind;
use open_tiles::store::LocalStore;
use open_tiles::{Error, TileId};
use std::sync::Arc;
use std::time::Duration;

fn fetcher(dir: &std::path::Path) -> Fetcher {
    Fetcher::new(
        Arc::new(LocalStore::new(dir)),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
}

#[test]
fn miss_downloads_and_writes_through_then_hits() {
    let dir = tempfile::tempdir().unwrap();
    let body = terrarium_png(|_, _| 12.0);
    let srv = Server::start(vec![("/h/5/3/4.png".into(), body.clone())]);
    let f = fetcher(dir.path());
    let t = TileId::new(5, 3, 4).unwrap();
    let url = format!("{}/h/5/3/4.png", srv.base);

    let (b1, o1) = f.fetch(Kind::Heightmap, t, &url).unwrap();
    assert_eq!(o1, Origin::Network);
    assert_eq!(b1, body);
    assert_eq!(
        std::fs::read(dir.path().join("heightmap/5/3/4.png")).unwrap(),
        body
    );

    let (b2, o2) = f.fetch(Kind::Heightmap, t, &url).unwrap();
    assert_eq!(o2, Origin::Cache);
    assert_eq!(b2, body);
    assert_eq!(
        srv.hits.lock().unwrap().len(),
        1,
        "second call must not touch the network"
    );
    // no temp files left behind
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("heightmap/5/3"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec![std::ffi::OsString::from("4.png")]);
}

#[test]
fn connection_refused_is_fetch_error() {
    let dir = tempfile::tempdir().unwrap();
    let f = fetcher(dir.path());
    let t = TileId::new(5, 3, 4).unwrap();
    // port 9 (discard) is almost certainly closed; the error kind is what matters
    match f.fetch(Kind::Texture, t, "http://127.0.0.1:9/x.png") {
        Err(Error::Fetch { .. }) => {}
        other => panic!("expected Fetch, got {other:?}"),
    }
}

#[test]
fn empty_cache_entry_is_refetched() {
    let dir = tempfile::tempdir().unwrap();
    let body = imagery_jpeg();
    let srv = Server::start(vec![("/t.png".into(), body.clone())]);
    let f = fetcher(dir.path());
    let t = TileId::new(5, 3, 4).unwrap();
    seed(dir.path(), "texture", t, &[]);
    let (b, o) = f
        .fetch(Kind::Texture, t, &format!("{}/t.png", srv.base))
        .unwrap();
    assert_eq!(o, Origin::Network);
    assert_eq!(b, body);
}

#[test]
fn not_found_is_typed_and_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(vec![]);
    let f = fetcher(dir.path());
    let t = TileId::new(5, 3, 4).unwrap();
    let url = format!("{}/missing.png", srv.base);
    match f.fetch(Kind::Texture, t, &url) {
        Err(Error::NotFound { url: u }) => assert_eq!(u, url),
        other => panic!("expected NotFound, got {other:?}"),
    }
    assert!(
        !dir.path().join("texture/5/3/4.png").exists(),
        "404 body must not be cached"
    );
    assert!(
        dir.path().join("texture/5/3/4.png.404").exists(),
        "marker written"
    );
    assert!(matches!(
        f.fetch(Kind::Texture, t, &url),
        Err(Error::NotFound { .. })
    ));
    assert_eq!(
        srv.hits.lock().unwrap().len(),
        1,
        "marker must short-circuit"
    );
    // a real entry appearing later (e.g. written by an engine) wins over the marker
    seed(dir.path(), "texture", t, b"real");
    assert_eq!(f.fetch(Kind::Texture, t, &url).unwrap().1, Origin::Cache);
    assert_eq!(f.clear_missing_markers(None, None).unwrap(), 1);
    assert!(!dir.path().join("texture/5/3/4.png.404").exists());
}

#[test]
fn walk_down_finds_the_closest_zoom_and_stops_on_real_errors() {
    use open_tiles::Provider;
    let dir = tempfile::tempdir().unwrap();
    let body = terrarium_png(|_, _| 3.0);
    // only z13 exists upstream
    let srv = Server::start(vec![("/h/13/2/3.png".into(), body.clone())]);
    let f = fetcher(dir.path());
    let provider = Provider {
        heightmap_url: format!("{}/h/:zoom:/:x:/:y:.png", srv.base),
        heightmap_max_zoom: 15,
        ..Provider::default()
    };
    let t = TileId::new(18, 2 << 5, 3 << 5).unwrap();
    let c = f.fetch_closest(&provider, Kind::Heightmap, t).unwrap();
    assert_eq!(c.source, TileId::new(13, 2, 3).unwrap());
    assert_eq!(c.bytes, body);
    // (18, 64, 96) → started at min(18, 15) = 15: requests at 15, 14, 13 only
    assert_eq!(
        srv.hits.lock().unwrap().clone(),
        vec!["/h/15/8/12.png", "/h/14/4/6.png", "/h/13/2/3.png"]
    );

    // a dead provider is a Fetch error, not a silent walk to a lower zoom
    let dead = Provider {
        heightmap_url: "http://127.0.0.1:9/h/:zoom:/:x:/:y:.png".into(),
        ..Provider::default()
    };
    let dir2 = tempfile::tempdir().unwrap();
    let f2 = fetcher(dir2.path());
    assert!(matches!(
        f2.fetch_closest(&dead, Kind::Heightmap, t),
        Err(Error::Fetch { .. })
    ));
}
