//! Shared test helpers: synthetic Terrarium / imagery fixtures generated in
//! code (no binary fixture files), seeded caches, and a local HTTP server.

#![allow(dead_code)]

use image::{ImageEncoder, RgbImage};
use open_tiles::tile::TileId;
use std::io::Cursor;
use std::path::Path;

/// Encode heights (metres) as a 256×256 Terrarium PNG.
pub fn terrarium_png(heights: impl Fn(u32, u32) -> f64) -> Vec<u8> {
    let mut img = RgbImage::new(256, 256);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let fixed = ((heights(x, y) + 32768.0) * 256.0)
            .round()
            .clamp(0.0, 0xFF_FFFF as f64) as u32;
        *p = image::Rgb([
            (fixed >> 16) as u8,
            ((fixed >> 8) & 0xFF) as u8,
            (fixed & 0xFF) as u8,
        ]);
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(Cursor::new(&mut out))
        .write_image(img.as_raw(), 256, 256, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// A 256×256 JPEG with a colour gradient.
pub fn imagery_jpeg() -> Vec<u8> {
    let img = RgbImage::from_fn(256, 256, |x, y| image::Rgb([x as u8, y as u8, 128]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), 85)
        .encode_image(&img)
        .unwrap();
    out
}

/// A 256×256 PNG imagery tile (exercises the PNG → JPEG re-encode path).
pub fn imagery_png() -> Vec<u8> {
    let img = RgbImage::from_fn(256, 256, |x, y| image::Rgb([x as u8, y as u8, 200]));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(Cursor::new(&mut out))
        .write_image(img.as_raw(), 256, 256, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// Parse the JSON chunk straight out of a GLB container (the `gltf` crate
/// hides root `extras` behind a feature flag; reading the chunk ourselves
/// also checks the container layout independently of that crate).
pub fn glb_json(bytes: &[u8]) -> serde_json::Value {
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    assert_eq!(&bytes[..4], b"glTF");
    assert_eq!(u32_at(4), 2, "glb version");
    assert_eq!(u32_at(8) as usize, bytes.len(), "declared total length");
    let json_len = u32_at(12) as usize;
    assert_eq!(u32_at(16), 0x4E4F_534A, "first chunk must be JSON");
    assert_eq!(json_len % 4, 0, "json chunk padded to 4");
    let json = &bytes[20..20 + json_len];
    let bin_len = u32_at(20 + json_len) as usize;
    assert_eq!(
        u32_at(24 + json_len),
        0x004E_4942,
        "second chunk must be BIN"
    );
    assert_eq!(bin_len % 4, 0, "bin chunk padded to 4");
    assert_eq!(28 + json_len + bin_len, bytes.len());
    serde_json::from_slice(json).unwrap()
}

/// Write a cache entry the way the engines / fetcher lay it out.
pub fn seed(cache: &Path, kind: &str, tile: TileId, bytes: &[u8]) {
    let p = cache
        .join(kind)
        .join(tile.zoom.to_string())
        .join(tile.x.to_string())
        .join(format!("{}.png", tile.y));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// Seed a tile and its 8 neighbours with a single continuous ramp across all
/// of them (`height = global_x + 0.5 * global_y` in texels), plus imagery.
pub fn seed_block(cache: &Path, centre: TileId) {
    seed_block_into(&open_tiles::store::LocalStore::new(cache), centre);
}

/// [`seed_block`] into any store (the S3 tests seed a bucket).
pub fn seed_block_into(store: &dyn open_tiles::Store, centre: TileId) {
    for dy in -1..=1 {
        for dx in -1..=1 {
            let t = centre.offset(dx, dy).unwrap();
            let png = terrarium_png(move |x, y| {
                f64::from(dx + 1) * 256.0
                    + f64::from(x)
                    + 0.5 * (f64::from(dy + 1) * 256.0 + f64::from(y))
            });
            let key = |kind: &str| format!("{kind}/{}/{}/{}.png", t.zoom, t.x, t.y);
            store.put(&key("heightmap"), &png).unwrap();
            store.put(&key("texture"), &imagery_jpeg()).unwrap();
        }
    }
}

/// Minimal local HTTP server: serves `routes` (path → bytes), 404 otherwise,
/// and counts requests. Runs until dropped.
pub struct Server {
    pub base: String,
    pub hits: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    _thread: std::thread::JoinHandle<()>,
    server: std::sync::Arc<tiny_http::Server>,
}

impl Server {
    pub fn start(routes: Vec<(String, Vec<u8>)>) -> Self {
        Self::start_with_delay(routes, std::time::Duration::ZERO)
    }

    /// Like `start`, but every matched route sleeps `delay` before answering
    /// (lets concurrent requests pile up on one build).
    pub fn start_with_delay(routes: Vec<(String, Vec<u8>)>, delay: std::time::Duration) -> Self {
        let server = std::sync::Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr());
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (s, h) = (server.clone(), hits.clone());
        let thread = std::thread::spawn(move || {
            for req in s.incoming_requests() {
                let url = req.url().to_string();
                h.lock().unwrap().push(url.clone());
                match routes.iter().find(|(p, _)| *p == url) {
                    Some((_, body)) => {
                        std::thread::sleep(delay);
                        req.respond(tiny_http::Response::from_data(body.clone()))
                            .ok()
                    }
                    None => req.respond(tiny_http::Response::empty(404)).ok(),
                };
            }
        });
        Self {
            base,
            hits,
            _thread: thread,
            server,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.server.unblock();
    }
}
