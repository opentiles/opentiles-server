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
fn deep_zoom_builds_from_cached_ancestor_and_refresh_clears_markers() {
    let dir = tempfile::tempdir().unwrap();
    let z15 = TileId::new(15, 16_000, 12_800).unwrap();
    seed_block(dir.path(), z15);
    let srv = Server::start(vec![]);
    let out_path = dir.path().join("deep.glb");
    let out = bin()
        .args([
            "build",
            "20",
            &(16_000u32 << 5).to_string(),
            &(12_800u32 << 5).to_string(),
            "-v",
        ])
        .arg("--cache-dir")
        .arg(dir.path())
        .arg("-o")
        .arg(&out_path)
        .args(["--texture-url", &format!("{}/t/:zoom:/:x:/:y:", srv.base)])
        .args([
            "--heightmap-url",
            &format!("{}/h/:zoom:/:x:/:y:.png", srv.base),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let extras = &glb_json(&std::fs::read(&out_path).unwrap())["extras"];
    assert_eq!(extras["terrain_source_zoom"], 15);
    assert_eq!(extras["resolution"], 9);
    // imagery walked 19..16 (4 markers); refresh-404 removes them
    let out = bin()
        .args(["refresh-404", "--kind", "texture"])
        .arg("--cache-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("removed 4 markers"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
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
