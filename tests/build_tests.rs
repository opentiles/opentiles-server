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

    // a dead provider (connection refused) must fail the build instead —
    // drop the 404 marker the previous run left so the fetch really happens
    std::fs::remove_file(
        dir.path()
            .join(format!("heightmap/10/{}/{}.png.404", east.x, east.y)),
    )
    .unwrap();
    let cfg = offline_config(dir.path());
    match load_inputs(&cfg, centre) {
        Err(Error::Fetch { .. }) => {}
        other => panic!("expected Fetch error, got {:?}", other.map(|_| ())),
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
    let cfg = offline_config(dir.path()).with_uniform_resolution(33);
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
    let cfg = offline_config(dir.path()).with_uniform_resolution(257);
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

/// A config whose providers 404 everything not in the cache (so the
/// walk-down can run) and log every request.
fn fallback_config(cache: &std::path::Path, srv: &Server) -> Config {
    let mut cfg = offline_config(cache);
    cfg.provider.heightmap_url = format!("{}/h/:zoom:/:x:/:y:.png", srv.base);
    cfg.provider.texture_url = format!("{}/t/:zoom:/:x:/:y:", srv.base);
    cfg
}

#[test]
fn deeper_tile_is_derived_from_the_closest_cached_zoom() {
    let dir = tempfile::tempdir().unwrap();
    let z15 = TileId::new(15, 16_000, 12_800).unwrap();
    seed_block(dir.path(), z15);
    let srv = Server::start(vec![]);
    let cfg = fallback_config(dir.path(), &srv);

    // z18 tile inside z15's quadrant chain
    let t = TileId::new(18, 16_000 * 8 + 5, 12_800 * 8 + 6).unwrap();
    let inputs = load_inputs(&cfg, t).unwrap();
    assert_eq!(inputs.terrain_source, z15);
    assert_eq!(inputs.imagery_source, z15);
    assert_eq!(inputs.neighbours_present, 8);
    // the only network traffic is the imagery walk: heightmap starts at
    // min(18, 15) = 15 → cache hit, no request; imagery starts at
    // min(18, 19) = 18 → 404 at 18, 17, 16, then the cache hit at 15
    let hits = srv.hits.lock().unwrap().clone();
    assert!(hits.iter().all(|h| h.starts_with("/t/")), "{hits:?}");
    assert_eq!(hits.len(), 3, "{hits:?}");

    // geometry equals direct sub-window sampling of the z15 field
    let direct = load_inputs(&cfg, z15).unwrap().height;
    let (_, qx, qy) = t.ancestor(15);
    for (u, v) in [(0.0, 0.0), (0.5, 0.25), (1.0, 1.0)] {
        let expect = direct.sample((f64::from(qx) + u) / 8.0, (f64::from(qy) + v) / 8.0);
        assert_eq!(inputs.height.sample(u, v), expect);
    }

    // markers make the second build free
    let before = srv.hits.lock().unwrap().len();
    let glb = build_tile(&cfg, t).unwrap();
    assert_eq!(
        srv.hits.lock().unwrap().len(),
        before,
        "second build must not hit the network"
    );
    let extras = &glb_json(&glb)["extras"];
    assert_eq!(extras["terrain_source_zoom"], 15);
    assert_eq!(extras["imagery_source_zoom"], 15);
    assert_eq!(extras["resolution"], 33, "z18 default from the table");
    assert!(extras["resolution_requested"].is_null());
    // no derived files were written
    assert!(!dir.path().join("heightmap/18").exists());
    assert!(!dir
        .path()
        .join(format!("texture/18/{}/{}.png", t.x, t.y))
        .exists());
}

#[test]
fn resolution_is_capped_by_the_useful_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let z15 = TileId::new(15, 16_000, 12_800).unwrap();
    seed_block(dir.path(), z15);
    let srv = Server::start(vec![]);
    let mut cfg = fallback_config(dir.path(), &srv);
    cfg.set_resolution(20, 129); // far more than 8 source texels can give
    let t = TileId::new(20, 16_000 << 5, 12_800 << 5).unwrap();
    let glb = build_tile(&cfg, t).unwrap();
    let extras = &glb_json(&glb)["extras"];
    assert_eq!(extras["resolution"], 9);
    assert_eq!(extras["resolution_requested"], 129);
}

#[test]
fn imagery_falls_back_independently_of_heights() {
    let dir = tempfile::tempdir().unwrap();
    let centre = TileId::new(12, 500, 400).unwrap();
    seed_block(dir.path(), centre); // heightmaps + imagery at z12
                                    // remove the centre's own imagery; seed its z11 parent's imagery instead
    std::fs::remove_file(dir.path().join("texture/12/500/400.png")).unwrap();
    let (parent, _, _) = centre.ancestor(11);
    seed(dir.path(), "texture", parent, &imagery_png());
    let srv = Server::start(vec![]);
    let cfg = fallback_config(dir.path(), &srv);
    let inputs = load_inputs(&cfg, centre).unwrap();
    assert_eq!(inputs.terrain_source, centre);
    assert_eq!(inputs.imagery_source, parent);
    assert_eq!(
        image::guess_format(&inputs.jpeg).unwrap(),
        image::ImageFormat::Jpeg
    );
    assert!(
        dir.path().join("texture/12/500/400.png.404").exists(),
        "marker written"
    );
}

#[test]
fn watertight_across_a_source_boundary_when_derived() {
    // seed_block lays a continuous ramp over a 3×3 block of z15 sources;
    // build the two z17 tiles that meet on the boundary between two of them
    let dir = tempfile::tempdir().unwrap();
    let west = TileId::new(15, 16_000, 12_800).unwrap();
    seed_block(dir.path(), west);
    let srv = Server::start(vec![]);
    let cfg = fallback_config(dir.path(), &srv);
    let a = TileId::new(17, (16_000 << 2) + 3, (12_800 << 2) + 1).unwrap(); // west source, east-most column
    let b = TileId::new(17, 16_001 << 2, (12_800 << 2) + 1).unwrap(); // east source, west-most column
    let fa = load_inputs(&cfg, a).unwrap().height;
    let fb = load_inputs(&cfg, b).unwrap().height;
    for v in [0.0, 0.3, 1.0] {
        assert_eq!(fa.sample(1.0, v), fb.sample(0.0, v), "v={v}");
    }
}

#[test]
fn nothing_at_any_zoom_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(vec![]);
    let cfg = fallback_config(dir.path(), &srv);
    match build_tile(&cfg, TileId::new(6, 1, 1).unwrap()) {
        Err(Error::NotFound { .. }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
    // heightmap walked 6..=1 and imagery walked 6..=1: 12 requests, all now marked
    assert_eq!(srv.hits.lock().unwrap().len(), 12);
    let _ = build_tile(&cfg, TileId::new(6, 1, 1).unwrap());
    assert_eq!(
        srv.hits.lock().unwrap().len(),
        12,
        "markers must stop the retry"
    );
}
