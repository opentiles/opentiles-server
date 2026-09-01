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

#[test]
fn metadata_writes_missing_jsons_and_skips_existing() {
    let dir = tempfile::tempdir().unwrap();
    let tile = TileId::new(10, 500, 400).unwrap();
    // the tile's own heightmap: seed_block's ramp is, in local texels,
    // h(px, py) = 384 + px + 0.5·py over the centre tile
    seed_block(dir.path(), tile);
    // its 4 children: the same (linear) surface evaluated at the child's
    // texel centres, plus a constant 10 m of "new detail"
    for (cx, cy) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
        let child = TileId::new(11, 1000 + cx, 800 + cy).unwrap();
        let png = terrarium_png(move |i, j| {
            let px = (f64::from(cx * 256 + i) + 0.5) / 2.0 - 0.5;
            let py = (f64::from(cy * 256 + j) + 0.5) / 2.0 - 0.5;
            384.0 + px + 0.5 * py + 10.0
        });
        seed(dir.path(), "heightmap", child, &png);
    }
    // two fingerprints hold the built tile; one already has its metadata
    let glb = |fp: &str, ext: &str| dir.path().join(format!("glb/{fp}/10/500/400.{ext}"));
    for fp in ["fpa", "fpb"] {
        std::fs::create_dir_all(glb(fp, "glb").parent().unwrap()).unwrap();
        std::fs::write(glb(fp, "glb"), b"glTF fake").unwrap();
    }
    std::fs::write(glb("fpb", "json"), b"{}").unwrap();

    let run = || {
        bin()
            .arg("metadata")
            .arg("--cache-dir")
            .arg(dir.path())
            // bogus providers: everything must come from the seeded cache
            .args(["--texture-url", "http://127.0.0.1:9/t/:zoom:/:x:/:y:"])
            .args(["--heightmap-url", "http://127.0.0.1:9/h/:zoom:/:x:/:y:.png"])
            .output()
            .unwrap()
    };
    let out = run();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 written, 1 skipped, 0 failed"),
        "{stdout}"
    );

    // the pre-existing document is untouched, the new one is complete
    assert_eq!(std::fs::read(glb("fpb", "json")).unwrap(), b"{}");
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(glb("fpa", "json")).unwrap()).unwrap();
    assert_eq!(meta["zoom"], 10);
    assert_eq!(meta["x"], 500);
    assert_eq!(meta["y"], 400);
    assert!((meta["tile_size_m"].as_f64().unwrap() - tile.size_m()).abs() < 1e-6);
    assert!(
        (meta["min_height_m"].as_f64().unwrap() - 384.0).abs() < 0.01,
        "{meta}"
    );
    assert!(
        (meta["max_height_m"].as_f64().unwrap() - 766.5).abs() < 0.01,
        "{meta}"
    );
    // the children differ from the tile's surface by exactly the 10 m bump
    // (small slack: quantisation plus the clamped half-texel edge band)
    let err = meta["geometric_error_m"].as_f64().unwrap();
    assert!((err - 10.0).abs() < 0.5, "geometric_error_m {err}");

    // a second run finds both documents and writes nothing
    let out = run();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("0 written, 2 skipped, 0 failed"),
        "{stdout}"
    );
}

#[test]
fn build_writes_metadata_next_to_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    // the mock provider has no children at z11: geometric error must be 0
    let srv = Server::start(vec![]);
    let out_path = dir.path().join("tile.glb");
    let out = bin()
        .args(["build", "10", "500", "400", "--resolution", "17"])
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
    let json_path = dir.path().join("tile.json");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("tile.json"),
        "stdout must mention the metadata file"
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
    assert_eq!(meta["zoom"], 10);
    assert_eq!(meta["x"], 500);
    assert_eq!(meta["y"], 400);
    assert!((meta["tile_size_m"].as_f64().unwrap() - centre.size_m()).abs() < 1e-6);
    assert_eq!(meta["geometric_error_m"], 0.0, "{meta}");
}
