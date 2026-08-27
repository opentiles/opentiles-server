mod common;

use common::*;
use open_tiles::{build_tile, load_inputs, Config, Error, TileId};

fn offline_config(cache: &std::path::Path) -> Config {
    let mut cfg = Config {
        cache_dir: cache.to_path_buf(),
        ..Config::default()
    };
    // any network access is a test failure: point the provider at a dead port
    cfg.provider.texture_url = "http://127.0.0.1:9/t/:zoom:/:x:/:y:".into();
    cfg.provider.heightmap_url = "http://127.0.0.1:9/h/:zoom:/:x:/:y:.png".into();
    cfg.connect_timeout = std::time::Duration::from_millis(200);
    cfg
}

#[test]
fn load_inputs_pads_from_neighbours() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    let cfg = offline_config(dir.path());
    let inputs = load_inputs(&cfg, centre).unwrap();
    assert_eq!(inputs.neighbours_present, 8);
    assert_eq!(inputs.height.size, 258);
    // the ramp is height = gx + 0.5·gy with the centre tile at gx,gy ∈ [256,512):
    // west edge (u = 0) sits between global x 255 and 256 → 255.5 (+ 0.5·gy)
    let v = 0.5; // gy = 256 + 127.5 → 383.5... sample sits between rows 127/128
    let expected_x = 255.5;
    let expected_y = 0.5 * (256.0 + 128.0 - 0.5); // texel centres straddle 127.5
    let got = inputs.height.sample(0.0, v);
    assert!(
        (got - (expected_x + expected_y) as f32).abs() < 1e-3,
        "{got}"
    );
    assert!(image::guess_format(&inputs.jpeg).unwrap() == image::ImageFormat::Jpeg);
}

#[test]
fn missing_neighbour_404_clamps_but_other_errors_fail() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    // remove the east neighbour from the cache; the provider 404s it
    let east = centre.offset(1, 0).unwrap();
    std::fs::remove_file(
        dir.path()
            .join(format!("heightmap/10/{}/{}.png", east.x, east.y)),
    )
    .unwrap();

    let srv = Server::start(vec![]); // everything 404s
    let mut cfg = offline_config(dir.path());
    cfg.provider.heightmap_url = format!("{}/h/:zoom:/:x:/:y:.png", srv.base);
    let inputs = load_inputs(&cfg, centre).unwrap();
    assert_eq!(inputs.neighbours_present, 7);

    // a dead provider (connection refused) must fail the build instead
    let cfg = offline_config(dir.path());
    match load_inputs(&cfg, centre) {
        Err(Error::Fetch { .. }) => {}
        other => panic!("expected Fetch error, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn centre_404_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(vec![]);
    let mut cfg = offline_config(dir.path());
    cfg.provider.heightmap_url = format!("{}/h/:zoom:/:x:/:y:.png", srv.base);
    cfg.provider.texture_url = format!("{}/t/:zoom:/:x:/:y:", srv.base);
    match build_tile(&cfg, TileId::new(10, 1, 1).unwrap()) {
        Err(Error::NotFound { .. }) => {}
        other => panic!("expected NotFound, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn above_native_zoom_is_rejected_before_any_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = offline_config(dir.path());
    match build_tile(&cfg, TileId::new(16, 1, 1).unwrap()) {
        Err(Error::AboveNativeZoom {
            zoom: 16,
            native: 15,
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[test]
fn png_imagery_is_reencoded_to_jpeg() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    seed(dir.path(), "texture", centre, &imagery_png());
    let cfg = offline_config(dir.path());
    let inputs = load_inputs(&cfg, centre).unwrap();
    assert_eq!(
        image::guess_format(&inputs.jpeg).unwrap(),
        image::ImageFormat::Jpeg
    );
}

#[test]
fn glb_is_valid_and_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    let cfg = Config {
        resolution: 33,
        ..offline_config(dir.path())
    };
    let a = build_tile(&cfg, centre).unwrap();
    let b = build_tile(&cfg, centre).unwrap();
    assert_eq!(a, b, "same inputs + config must give byte-identical output");

    let (doc, buffers, images) = gltf::import_slice(&a).expect("gltf crate must parse our output");
    assert_eq!(doc.meshes().count(), 1);
    let prim = doc.meshes().next().unwrap().primitives().next().unwrap();
    let reader = prim.reader(|b| Some(&buffers[b.index()]));
    let positions: Vec<[f32; 3]> = reader.read_positions().unwrap().collect();
    assert_eq!(positions.len(), 33 * 33);
    let indices: Vec<u32> = reader.read_indices().unwrap().into_u32().collect();
    assert_eq!(indices.len(), 32 * 32 * 6);
    assert!(indices.iter().all(|&i| (i as usize) < positions.len()));
    let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).unwrap().into_f32().collect();
    assert_eq!(uvs[0], [0.0, 0.0]);
    assert_eq!(uvs[33 * 33 - 1], [1.0, 1.0]);

    // POSITION accessor min/max match the data
    let acc = prim.get(&gltf::Semantic::Positions).unwrap();
    let min: Vec<f32> = acc
        .min()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap() as f32)
        .collect();
    let max: Vec<f32> = acc
        .max()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap() as f32)
        .collect();
    for a in 0..3 {
        let lo = positions.iter().map(|p| p[a]).fold(f32::INFINITY, f32::min);
        let hi = positions
            .iter()
            .map(|p| p[a])
            .fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(min[a], lo);
        assert_eq!(max[a], hi);
    }
    // geometry spans exactly one tile edge in X and Z, origin at the corner
    let size = centre.size_m() as f32;
    assert_eq!(min[0], 0.0);
    assert_eq!(min[2], 0.0);
    assert!((max[0] - size).abs() < 1e-2 && (max[2] - size).abs() < 1e-2);

    // one JPEG image, decodable, 256²
    assert_eq!(images.len(), 1);
    assert_eq!((images[0].width, images[0].height), (256, 256));
    assert!(doc
        .materials()
        .next()
        .unwrap()
        .pbr_metallic_roughness()
        .base_color_texture()
        .is_some());
    assert!(doc.materials().next().unwrap().normal_texture().is_none());

    // extras carry the tile identity and placement info
    let extras = &glb_json(&a)["extras"];
    assert_eq!(extras["zoom"], 10);
    assert_eq!(extras["x"], 500);
    assert_eq!(extras["y"], 400);
    assert_eq!(extras["resolution"], 33);
    assert!((extras["tile_size_m"].as_f64().unwrap() - centre.size_m()).abs() < 1e-9);
    assert!(
        extras["bounds"]["north"].as_f64().unwrap() > extras["bounds"]["south"].as_f64().unwrap()
    );
}

#[test]
fn large_resolution_uses_u32_indices() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(10, 500, 400).unwrap();
    seed_block(dir.path(), centre);
    let cfg = Config {
        resolution: 257,
        ..offline_config(dir.path())
    };
    let glb = build_tile(&cfg, centre).unwrap();
    let (doc, buffers, _) = gltf::import_slice(&glb).unwrap();
    let prim = doc.meshes().next().unwrap().primitives().next().unwrap();
    assert_eq!(
        prim.indices().unwrap().data_type(),
        gltf::accessor::DataType::U32
    );
    let n = prim
        .reader(|b| Some(&buffers[b.index()]))
        .read_positions()
        .unwrap()
        .count();
    assert_eq!(n, 257 * 257);
}
