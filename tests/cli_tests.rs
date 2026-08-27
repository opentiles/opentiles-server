mod common;

use common::*;
use open_tiles::TileId;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_open-tiles"))
}

#[test]
fn build_writes_a_glb_from_a_seeded_cache() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    let out = dir.path().join("out.glb");
    let status = bin()
        .args(["build", "10", "500", "400", "--resolution", "17"])
        .arg("--cache-dir")
        .arg(dir.path())
        .arg("-o")
        .arg(&out)
        .args(["--texture-url", "http://127.0.0.1:9/t/:zoom:/:x:/:y:"])
        .args(["--heightmap-url", "http://127.0.0.1:9/h/:zoom:/:x:/:y:.png"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..4], b"glTF");
    gltf::import_slice(&bytes).expect("valid glb");
    let extras = &glb_json(&bytes)["extras"];
    assert_eq!(extras["resolution"], 17);
    assert_eq!(extras["x"], 500);
}

#[test]
fn invalid_tile_exits_2() {
    let out = bin().args(["build", "3", "9", "0"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid tile"));
    let out = bin().args(["build", "0", "0", "0"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn above_native_zoom_exits_2_with_message() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin()
        .args(["build", "16", "1", "1"])
        .arg("--cache-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("native terrain zoom"));
}

#[test]
fn bad_resolution_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin()
        .args(["build", "10", "1", "1", "--resolution", "1"])
        .arg("--cache-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn upstream_404_exits_3() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(vec![]);
    let out = bin()
        .args(["build", "10", "1", "1"])
        .arg("--cache-dir")
        .arg(dir.path())
        .args(["--texture-url", &format!("{}/t/:zoom:/:x:/:y:", srv.base)])
        .args([
            "--heightmap-url",
            &format!("{}/h/:zoom:/:x:/:y:.png", srv.base),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn lookup_prints_tile_and_size() {
    let out = bin()
        .args(["lookup", "36.1", "-112.1", "12"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("tile:      12 772 1607"), "{s}");
    assert!(s.contains("size_m:    7908.657"), "{s}");
}
