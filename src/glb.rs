//! GLB (glTF 2.0 binary) writer for one terrain tile: a single buffer holding
//! positions, UVs, indices and the JPEG, one mesh, one textured material,
//! one node at identity, and tile metadata in the root `extras`.
//!
//! Written by hand with `serde_json` — a terrain tile is one mesh and one
//! material, which is less code than driving a glTF builder crate. The
//! output is validated in tests by parsing it with the `gltf` crate.

use crate::mesh::Grid;
use crate::tile::TileId;
use serde_json::json;

/// GLB container magic: the ASCII bytes "glTF" read as a little-endian u32.
const GLB_MAGIC: u32 = 0x4654_6C67;
/// Chunk-type tag of the (first, mandatory) JSON chunk: "JSON".
const CHUNK_JSON: u32 = 0x4E4F_534A;
/// Chunk-type tag of the binary chunk: "BIN\0".
const CHUNK_BIN: u32 = 0x004E_4942;

// glTF enums are raw OpenGL numbers; named here so the JSON below is readable.
/// `componentType`: 32-bit IEEE float (positions, UVs).
const FLOAT: u32 = 5126;
/// `componentType`: 16-bit index — used whenever the vertex count fits.
const UNSIGNED_SHORT: u32 = 5123;
/// `componentType`: 32-bit index — only the 257-vertex grids need it.
const UNSIGNED_INT: u32 = 5125;
/// bufferView `target` for vertex attributes.
const ARRAY_BUFFER: u32 = 34962;
/// bufferView `target` for triangle indices.
const ELEMENT_ARRAY_BUFFER: u32 = 34963;
/// Sampler magnification filter: bilinear.
const LINEAR: u32 = 9729;
/// Sampler minification filter: trilinear (interpolated mipmaps).
const LINEAR_MIPMAP_LINEAR: u32 = 9987;
/// Wrap mode: edge texels extend past 0/1, so a tile's border never bleeds
/// in the opposite edge of its own texture.
const CLAMP_TO_EDGE: u32 = 33071;

/// Where a tile's normals came from — recorded in the `extras` and deciding
/// whether the mesh carries a NORMAL attribute at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalsOrigin {
    /// Normals disabled: no NORMAL attribute, no `extras` entry.
    Omitted,
    /// Synthesized from the height field (`extras.normals_source_zoom` is
    /// `null`).
    Heights,
    /// The provider's normal tile at this zoom.
    Provider(u8),
}

/// Metadata recorded in the GLB root `extras` so a consumer can place and
/// attribute the tile without out-of-band knowledge.
#[derive(Clone, Debug)]
pub struct TileMeta {
    /// The tile.
    pub tile: TileId,
    /// Edge length in metres (at the tile's centre latitude).
    pub tile_size_m: f64,
    /// Vertices per edge actually used.
    pub resolution: u32,
    /// The configured resolution when the useful ceiling clamped it.
    pub resolution_requested: Option<u32>,
    /// Zoom the heightmap was taken from (`== tile.zoom` when served directly).
    pub terrain_source_zoom: u8,
    /// Zoom the imagery was taken from (`== tile.zoom` when served directly).
    pub imagery_source_zoom: u8,
    /// Where the normals came from (provider zoom, heights, or omitted).
    pub normals: NormalsOrigin,
    /// Imagery attribution.
    pub imagery_attribution: String,
    /// Elevation attribution.
    pub elevation_attribution: String,
}

/// Serialize a tile to GLB bytes. `jpeg` must be a JPEG stream.
pub fn write_glb(grid: &Grid, jpeg: &[u8], meta: &TileMeta) -> Vec<u8> {
    // --- binary chunk -----------------------------------------------------
    let mut bin = Vec::new();
    let mut views = Vec::new();

    let pos_view = push_view(
        &mut bin,
        &mut views,
        &f32_bytes(grid.positions.iter().flatten()),
        Some(ARRAY_BUFFER),
        Some(12),
    );
    let uv_view = push_view(
        &mut bin,
        &mut views,
        &f32_bytes(grid.uvs.iter().flatten()),
        Some(ARRAY_BUFFER),
        Some(8),
    );

    let nrm_view = (!grid.normals.is_empty()).then(|| {
        push_view(
            &mut bin,
            &mut views,
            &f32_bytes(grid.normals.iter().flatten()),
            Some(ARRAY_BUFFER),
            Some(12),
        )
    });

    let (index_bytes, index_type) = if grid.fits_u16() {
        (
            grid.indices
                .iter()
                .flat_map(|&i| (i as u16).to_le_bytes())
                .collect::<Vec<u8>>(),
            UNSIGNED_SHORT,
        )
    } else {
        (
            grid.indices.iter().flat_map(|&i| i.to_le_bytes()).collect(),
            UNSIGNED_INT,
        )
    };
    let idx_view = push_view(
        &mut bin,
        &mut views,
        &index_bytes,
        Some(ELEMENT_ARRAY_BUFFER),
        None,
    );
    let img_view = push_view(&mut bin, &mut views, jpeg, None, None);
    pad_to_4(&mut bin, 0);

    // --- json chunk ---------------------------------------------------------
    // accessors: 0 POSITION, 1 TEXCOORD_0, [2 NORMAL when present], last SCALAR
    let mut accessors = vec![
        json!({
            "bufferView": pos_view, "componentType": FLOAT, "count": grid.positions.len(),
            "type": "VEC3", "min": grid.min, "max": grid.max,
        }),
        json!({
            "bufferView": uv_view, "componentType": FLOAT, "count": grid.uvs.len(),
            "type": "VEC2",
        }),
    ];
    let mut attributes = json!({ "POSITION": 0, "TEXCOORD_0": 1 });
    if let Some(view) = nrm_view {
        attributes["NORMAL"] = json!(accessors.len());
        accessors.push(json!({
            "bufferView": view, "componentType": FLOAT, "count": grid.normals.len(),
            "type": "VEC3",
        }));
    }
    let idx_accessor = accessors.len();
    accessors.push(json!({
        "bufferView": idx_view, "componentType": index_type, "count": grid.indices.len(),
        "type": "SCALAR",
    }));

    let b = meta.tile.bounds();
    let mut doc = json!({
        "asset": {
            "version": "2.0",
            "generator": concat!("open-tiles ", env!("CARGO_PKG_VERSION")),
        },
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": views,
        "accessors": accessors,
        "images": [{ "bufferView": img_view, "mimeType": "image/jpeg" }],
        "samplers": [{
            "magFilter": LINEAR, "minFilter": LINEAR_MIPMAP_LINEAR,
            "wrapS": CLAMP_TO_EDGE, "wrapT": CLAMP_TO_EDGE,
        }],
        "textures": [{ "sampler": 0, "source": 0 }],
        "materials": [{
            "name": "terrain",
            "pbrMetallicRoughness": {
                "baseColorTexture": { "index": 0 },
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0,
            },
        }],
        "meshes": [{
            "name": meta.tile.to_string(),
            "primitives": [{
                "attributes": attributes,
                "indices": idx_accessor,
                "material": 0,
                "mode": 4,
            }],
        }],
        "nodes": [{ "name": meta.tile.to_string(), "mesh": 0 }],
        "scenes": [{ "nodes": [0] }],
        "scene": 0,
        "extras": {
            "zoom": meta.tile.zoom,
            "x": meta.tile.x,
            "y": meta.tile.y,
            "tile_size_m": meta.tile_size_m,
            "bounds": { "north": b.north, "south": b.south, "west": b.west, "east": b.east },
            "resolution": meta.resolution,
            "resolution_requested": meta.resolution_requested,
            "terrain_source_zoom": meta.terrain_source_zoom,
            "imagery_source_zoom": meta.imagery_source_zoom,
            "units": "metres",
            "up": "+Y",
            "origin": "north-west corner; +X east, +Z south; Y is metres above sea level",
            "sources": {
                "imagery": meta.imagery_attribution,
                "elevation": meta.elevation_attribution,
            },
        },
    });
    match meta.normals {
        NormalsOrigin::Omitted => {}
        NormalsOrigin::Heights => doc["extras"]["normals_source_zoom"] = json!(null),
        NormalsOrigin::Provider(z) => doc["extras"]["normals_source_zoom"] = json!(z),
    }
    let mut json_bytes = serde_json::to_vec(&doc).expect("static json shape");
    pad_to_4(&mut json_bytes, b' ');

    // --- container -----------------------------------------------------------
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&GLB_MAGIC.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&CHUNK_JSON.to_le_bytes());
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&CHUNK_BIN.to_le_bytes());
    out.extend_from_slice(&bin);
    out
}

/// Flatten `f32`s into little-endian bytes (glTF buffers are always LE).
fn f32_bytes<'a>(it: impl Iterator<Item = &'a f32>) -> Vec<u8> {
    it.flat_map(|v| v.to_le_bytes()).collect()
}

/// Append `bytes` to the buffer at a 4-byte-aligned offset and record the
/// bufferView; returns its index.
fn push_view(
    bin: &mut Vec<u8>,
    views: &mut Vec<serde_json::Value>,
    bytes: &[u8],
    target: Option<u32>,
    stride: Option<u32>,
) -> usize {
    pad_to_4(bin, 0);
    let offset = bin.len();
    bin.extend_from_slice(bytes);
    let mut v = json!({ "buffer": 0, "byteOffset": offset, "byteLength": bytes.len() });
    if let Some(t) = target {
        v["target"] = json!(t);
    }
    if let Some(s) = stride {
        v["byteStride"] = json!(s);
    }
    views.push(v);
    views.len() - 1
}

/// Pad `buf` to a multiple of 4 bytes. GLB requires 4-byte alignment
/// everywhere: spaces inside the JSON chunk (still valid JSON), zeros in
/// binary data.
fn pad_to_4(buf: &mut Vec<u8>, fill: u8) {
    while !buf.len().is_multiple_of(4) {
        buf.push(fill);
    }
}
