# open-tiles: 3D Terrain Tile Specification

| | |
|---|---|
| **Status** | Draft, version 1 |
| **Date** | 2026-08-30 |
| **Applies to** | open-tiles 0.1.x (`asset.generator` = `open-tiles 0.1.0`) |
| **Editor** | Ziv Perry |

## Abstract

This document specifies the **open-tiles terrain tile**: a self-contained glTF 2.0 binary
(GLB) that carries the terrain of one Web Mercator slippy-map tile as real geometry, at 1:1
world scale, with satellite imagery as its texture. It defines how tiles are addressed, the
coordinate frame and scale a consumer can rely on, the mesh, height and imagery construction
rules that make neighbouring tiles fit together without cracks, the exact GLB layout and its
metadata, the behaviour when source data is missing at a zoom, and the HTTP interface that
serves the tiles.

The goal is that **any glTF loader can display a tile with no custom shader**, and that any
client can lay tiles out with an X/Z translation alone.

## 1. Conventions and terminology

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHOULD", "SHOULD NOT",
"RECOMMENDED", "MAY" and "OPTIONAL" in this document are to be interpreted as described in
[RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

- **Tile** — a square of the Web Mercator world at a zoom level, addressed `zoom/x/y`.
- **Builder** — the component that turns source data into a tile GLB (`open_tiles::build_tile`).
- **Server** — the HTTP service that builds tiles on demand and caches them.
- **Consumer** — anything loading a tile GLB (a game engine, three.js, a GIS viewer).
- **Provider** — an upstream HTTP source of raw inputs (heightmaps, imagery).
- **Source zoom** — the zoom the heightmap or imagery for a tile was actually taken from;
  equal to the tile's zoom unless the provider had nothing there (§6).
- **Texel** — one sample of a 256×256 source image.
- `S` — the tile's edge length in metres (§3.3). `R` — vertices per mesh edge (§4.2).
- `n = 2^zoom` — tiles per axis at a zoom.

Numeric values in this document are normative where stated; formulas are evaluated in IEEE 754
double precision unless a narrower type is named.

## 2. Tile addressing

### 2.1 Address space

A tile address is `(zoom, x, y)` with

- `zoom` ∈ **[1, 22]** (`MIN_ZOOM = 1`, `MAX_ZOOM = 22`);
- `x` ∈ [0, 2^zoom), increasing **eastward**;
- `y` ∈ [0, 2^zoom), increasing **southward** (XYZ / "slippy map" convention, not TMS).

An address outside these ranges is invalid. Producers MUST reject it (HTTP 400, CLI exit 2).

### 2.2 Projection

Tiles partition the Web Mercator square (EPSG:3857) between latitudes
±85.051 128 779 806 59°. The geographic bounds of a tile are, in degrees:

```
lon(x) = x / n · 360 − 180
lat(y) = atan( sinh( π · (1 − 2·y / n) ) )   (in radians, converted to degrees)

west  = lon(x)      east  = lon(x + 1)
north = lat(y)      south = lat(y + 1)
```

The tile containing `(lat, lon)` at a zoom is obtained by clamping `lat` to the Mercator range,
wrapping `lon` into [−180, 180), and taking

```
x = floor( (lon + 180) / 360 · n )
y = floor( (1 − ln( tan φ + sec φ ) / π) / 2 · n )        φ = lat in radians
```

each clamped to `n − 1`.

### 2.3 Ancestry

The **ancestor** of tile `(zoom, x, y)` at zoom `z' ≤ zoom` is `(z', x >> dz, y >> dz)` with
`dz = zoom − z'`. The tile's **window offset** inside that ancestor is
`(qx, qy) = (x − (ax << dz), y − (ay << dz))`, each in [0, 2^dz). These are used by §5.4 and
§7.2.

### 2.4 Neighbours

The eight same-zoom neighbours of a tile are the offsets `(dx, dy) ∈ {−1, 0, 1}²` excluding
`(0, 0)`, in the order **NW, N, NE, W, E, SW, S, SE**. An offset that leaves [0, n) on either
axis has no neighbour; there is **no antimeridian wrap** in this version.

## 3. Coordinate frame and scale

### 3.1 Frame

A tile's geometry is expressed in a **right-handed, Y-up, metric** frame:

- **+X** points east, **+Z** points south, **+Y** points up.
- The origin is the tile's **north-west corner** at sea level.
- The tile covers `[0, S]` in X and `[0, S]` in Z.

The glTF node carrying the mesh has an identity transform; the frame above *is* the glTF
scene frame of the file.

### 3.2 Vertical datum

**Y is metres above sea level**, unscaled, exactly as decoded from the source (§5.1). Every tile
shares the same `Y = 0` plane. Consequently placing tiles on a grid never involves a Y offset.

### 3.3 Tile size

The edge length of a tile in metres, `S`, is the Mercator ground resolution **at the tile's own
centre latitude**:

```
S = 40 075 016.686 · cos(lat_c) / 2^zoom
lat_c = atan( sinh( π · (1 − 2·(y + 0.5) / n) ) )
```

`40 075 016.686` m is the WGS84 equatorial circumference. A consumer MUST read `S` from
`extras.tile_size_m` (§8.6) rather than recomputing it, unless it reproduces the formula above
exactly.

### 3.4 Placement

Because each tile is scaled by its own centre latitude, adjacent *rows* of tiles differ slightly
in `S` (≈1 % at zoom 9, negligible from zoom 14). Two placement strategies are supported:

1. **Exact:** place tile `(x, y)` at translation `(x · S_xy, 0, y · S_xy)` using each tile's own
   `S`. Columns meet exactly; rows have a sub-percent gap or overlap at low zooms.
2. **Uniform grid (RECOMMENDED for blocks of tiles):** choose one edge length `G` (e.g. the
   centre tile's `S`), place tile `(x, y)` at `(dx · G, 0, dy · G)` relative to a reference
   tile, and scale the tile's node by `G / S_xy` in **X and Z only**. Y MUST NOT be scaled.
   Mercator rows then meet exactly.

The bundled viewer (`example/index.html`) implements strategy 2.

## 4. Mesh

### 4.1 Topology

A tile is one triangle-list mesh over a regular `R × R` vertex grid. Vertices are stored
**row-major, north row first, west vertex first**. Vertex `k = j · R + i` (row `j`, column `i`)
has normalized coordinates

```
u = i / (R − 1)        v = j / (R − 1)         i, j ∈ [0, R)
```

and position

```
X = u · S        Z = v · S        Y = height(u, v)   (§5.3)
```

All three components are stored as `f32` (X and Z are computed in `f64` and narrowed).

### 4.2 Resolution

`R` (vertices per edge) MUST satisfy `2 ≤ R ≤ 257`. Above 257 a grid would only interpolate the
256-texel source. The default `R` per zoom is:

| zoom | 1–7 | 8–15 | 16 | 17 | 18 | 19 | 20 | 21 | 22 |
|---|---|---|---|---|---|---|---|---|---|
| `R` | 257 | 129 | 129 | 65 | 33 | 17 | 9 | 5 | 3 |

The rationale: every tile carries 256 source texels while its metre size halves per zoom, so
continental tiles keep full fidelity, the streaming range uses two texels per vertex, and each
zoom beyond the heightmap provider's deepest level halves the vertex count.

The configured `R` is further capped by the **useful ceiling** when the heights came from a
lower zoom (§6):

```
ceiling(dz) = max( (256 >> dz) + 1, 2 )        dz = zoom − terrain_source_zoom
R_used      = min( R_configured, ceiling(dz) )
```

`R_used` is recorded in `extras.resolution`; when capping happened, `extras.resolution_requested`
carries the configured value.

### 4.3 Triangles and winding

Each grid cell with corners `a` (NW, index `j·R + i`), `b = a + 1` (NE), `c = a + R` (SW),
`d = c + 1` (SE) produces two triangles in this order:

```
(a, c, b)   (b, c, d)
```

Both wind **counter-clockwise when viewed from +Y** (looking down), so front faces point up
under the glTF default of CCW front faces. Cells are emitted row-major (north to south, west to
east). The mesh has `(R − 1)² · 2` triangles.

### 4.4 Texture coordinates

`TEXCOORD_0 = (u, v)` as defined in §4.1. glTF's UV origin is the top-left of the image, which
coincides with the north-west corner, so the imagery (row 0 = north) maps without a flip.

### 4.5 Index type

Indices are `UNSIGNED_SHORT` when the vertex count is ≤ 65 536 (`R ≤ 256`), otherwise
`UNSIGNED_INT`. With the defaults only `R = 257` (66 049 vertices, zooms 1–7) uses 32-bit
indices.

### 4.6 Normals

Tiles carry **no normals, no tangents, no skirts**. Consumers compute flat or smooth normals as
they see fit. Level-of-detail seams between tiles of different zooms are the consumer's
responsibility.

## 5. Heights

### 5.1 Source encoding

Heightmaps are **Terrarium**-encoded 256×256 RGB PNGs. A texel decodes to metres as

```
h = r · 256 + g + b / 256 − 32768
```

evaluated in `f64` and narrowed once to `f32`; the full 1/256 m fraction is preserved. Any
heightmap that is not 256×256 MUST be rejected (`Decode` error, HTTP 500).

### 5.2 Padded height field

Before sampling, the source tile's 256² texels are placed in the centre of a **258 × 258
padded field** `P`, where `P[j + 1][i + 1]` = source texel `(i, j)`. The one-texel ring around
it is filled from the eight same-zoom neighbours **of the source tile**:

| pad cells | taken from | fallback when that neighbour is missing |
|---|---|---|
| north row | N's row 255 | own row 0 |
| south row | S's row 0 | own row 255 |
| west column | W's column 255 | own column 0 |
| east column | E's column 0 | own column 255 |
| NW corner | NW's texel (255, 255) | W's (255, 0), then N's (0, 255), then own (0, 0) |
| NE corner | NE's (0, 255) | E's (0, 0), then N's (255, 255), then own (255, 0) |
| SW corner | SW's (255, 0) | W's (255, 255), then S's (0, 0), then own (0, 255) |
| SE corner | SE's (0, 0) | E's (0, 255), then S's (255, 0), then own (255, 255) |

The corner fallback order is normative: it is what keeps a shared edge identical between two
tiles when the diagonal neighbour is absent (both tiles can see the E/W or N/S neighbour's
corner texel, but not each other's own corner).

A neighbour that the provider reports as **404 at the source zoom** is treated as missing
(dataset edge). Any other neighbour failure MUST fail the build — a transient error must not
silently produce a different mesh that then gets cached.

### 5.3 Sampling

`height(u, v)` for `u, v ∈ [0, 1]` (`u` west→east, `v` north→south) is a bilinear
interpolation of `P` with **texel centres at `(i + 0.5) / 256`**:

```
fx = u · 256 + 0.5          fy = v · 256 + 0.5           (padded-field coordinates)
x0 = floor(fx)              y0 = floor(fy)
tx = fx − x0                ty = fy − y0                  (as f32)
P(x, y) with x, y clamped to [0, 257]

height = ( P(x0, y0)·(1−tx) + P(x0+1, y0)·tx ) · (1−ty)
       + ( P(x0, y0+1)·(1−tx) + P(x0+1, y0+1)·tx ) · ty
```

At `u = 0` the sample lies exactly between the west pad texel and texel 0, so a boundary vertex
depends only on the two texels either side of the boundary — the same two the neighbouring tile
uses.

### 5.4 Derived tiles (windowing)

When the heights come from an ancestor `dz` levels up (§6), the tile is a **window** into the
ancestor's padded field. No intermediate image is synthesized. With `(qx, qy)` from §2.3 and
`s = 2^−dz`:

```
height_tile(u, v) = sample_field( qx·s + u·s,  qy·s + v·s )
```

where `sample_field` is §5.3 applied to the ancestor's `P`. This is a single bilinear
interpolation from source texels to vertices, continuous across every child boundary inside the
ancestor and across the ancestor's own edges (through its pad ring).

### 5.5 Watertightness guarantees

- Two same-zoom tiles whose heightmaps resolve to the **same source zoom** produce bit-identical
  Y values (and identical X/Z when placed per §3.4) along their shared edge. Producers MUST
  preserve this; it follows from §5.2–§5.4 when both tiles see the same neighbour data.
- At a provider's **coverage boundary** — one tile at zoom `z`, its neighbour only available at
  `z − 1` — a crack of up to one `z − 1` texel's height gradient is possible. This is the
  dataset's edge and is not considered a defect.

## 6. Source fallback ("closest provided zoom")

Providers do not cover every zoom (Terrarium stops at 15; Esri imagery at ~19). For each asset
kind (heightmap, imagery) independently, the builder:

1. starts at `z₀ = max( min(zoom, hint), 1 )`, where `hint` is the provider's
   *most-provided-zoom* (`heightmap_max_zoom`, default 15; `texture_max_zoom`, default 19);
2. requests the tile's ancestor at `z₀`; on HTTP **404** steps to `z₀ − 1`, and so on down to
   zoom 1;
3. uses the first zoom that answers; records it as the asset's **source zoom**;
4. on **any error other than 404** (timeout, 5xx, connection refused) aborts the build with a
   `Fetch` error. A transient failure MUST NOT change which zoom a tile is derived from.

If nothing exists at any zoom the result is `NotFound` (HTTP 404, CLI exit 3).

The hint only avoids requests known to fail; a 404 *below* the hint keeps walking. Heights and
imagery may resolve to different source zooms (`extras.terrain_source_zoom`,
`extras.imagery_source_zoom`).

## 7. Imagery

### 7.1 Texture

The texture is a **256 × 256 JPEG**. A provider response that is already JPEG is embedded
**byte-for-byte** (deterministic); any other decodable format (PNG) is decoded and re-encoded as
JPEG at **quality 90** (`jpeg_quality`, part of the fingerprint §10.2).

### 7.2 Derived imagery

When the imagery source zoom is `dz > 0` levels above the tile, the tile's texture is the
ancestor image's sub-square

```
crop origin = (qx · (w / 2^dz),  qy · (h / 2^dz))      size = (w / 2^dz, h / 2^dz)
```

resized to 256 × 256 with a **bilinear (triangle) filter** and encoded as JPEG at quality 90.
Sharper filters are deliberately not used: the ancestor is the best data there is, and sharper
kernels only invent edges. An ancestor smaller than `2^dz` texels per axis is an error.

## 8. GLB container

A tile is a **glTF 2.0 binary** (`.glb`) with exactly one buffer, one mesh, one primitive, one
material, one texture, one image, one node and one scene. No extensions are used or required.
Consumers MAY rely on the layout below; producers MUST emit it.

### 8.1 Header and chunks

```
offset  size  value
0       4     magic  0x46546C67  ("glTF")
4       4     version 2
8       4     total length in bytes
12      4     JSON chunk length
16      4     chunk type 0x4E4F534A ("JSON")
20      …     JSON chunk, padded to a multiple of 4 with spaces (0x20)
…       4     BIN chunk length
…       4     chunk type 0x004E4942 ("BIN\0")
…       …     BIN chunk, padded to a multiple of 4 with zeros
```

All integers are little-endian. The JSON chunk MUST be first and the BIN chunk second.

### 8.2 Binary buffer layout

The single buffer holds four **bufferViews**, in this order, each starting at a 4-byte-aligned
offset (padding bytes are zero):

| view | content | `target` | `byteStride` |
|---|---|---|---|
| 0 | positions, `R²` × 3 × `f32` LE | 34962 (ARRAY_BUFFER) | 12 |
| 1 | UVs, `R²` × 2 × `f32` LE | 34962 | 8 |
| 2 | indices, `(R−1)²·6` × `u16` or `u32` LE | 34963 (ELEMENT_ARRAY_BUFFER) | — |
| 3 | the JPEG bytes (§7) | — | — |

### 8.3 Accessors

| index | bufferView | componentType | type | count | notes |
|---|---|---|---|---|---|
| 0 | 0 | 5126 FLOAT | VEC3 | `R²` | `min`/`max` present (per-axis bounds of positions) |
| 1 | 1 | 5126 FLOAT | VEC2 | `R²` | |
| 2 | 2 | 5123 UNSIGNED_SHORT or 5125 UNSIGNED_INT | SCALAR | `(R−1)²·6` | §4.5 |

Accessor 0's `min`/`max` give the tile's bounding box directly; `min[1]`/`max[1]` are the
lowest and highest vertex in metres above sea level.

### 8.4 Material, texture, sampler

```json
"images":    [{ "bufferView": 3, "mimeType": "image/jpeg" }],
"samplers":  [{ "magFilter": 9729, "minFilter": 9987, "wrapS": 33071, "wrapT": 33071 }],
"textures":  [{ "sampler": 0, "source": 0 }],
"materials": [{
  "name": "terrain",
  "pbrMetallicRoughness": {
    "baseColorTexture": { "index": 0 }, "metallicFactor": 0.0, "roughnessFactor": 1.0
  }
}]
```

Filters are LINEAR / LINEAR_MIPMAP_LINEAR; wrap is CLAMP_TO_EDGE on both axes so tile borders
never bleed the opposite edge. The material is plain PBR with no emissive, normal, occlusion or
alpha settings; a consumer MAY replace it (e.g. with an unlit material) without losing
information.

### 8.5 Mesh, node, scene

```json
"meshes": [{ "name": "<zoom>/<x>/<y>",
             "primitives": [{ "attributes": { "POSITION": 0, "TEXCOORD_0": 1 },
                              "indices": 2, "material": 0, "mode": 4 }] }],
"nodes":  [{ "name": "<zoom>/<x>/<y>", "mesh": 0 }],
"scenes": [{ "nodes": [0] }],
"scene":  0,
"asset":  { "version": "2.0", "generator": "open-tiles <crate version>" }
```

`mode` 4 is TRIANGLES. The node has no transform (identity).

### 8.6 Root `extras` (tile metadata)

The root-level `extras` object is REQUIRED and lets a consumer place and attribute a tile with
no out-of-band knowledge:

| field | type | meaning |
|---|---|---|
| `zoom`, `x`, `y` | integer | the tile address (§2) |
| `tile_size_m` | number | `S` (§3.3) |
| `bounds` | object `{north, south, west, east}` | degrees (§2.2) |
| `resolution` | integer | `R_used` (§4.2) |
| `resolution_requested` | integer or `null` | configured `R` when it was capped, else `null` |
| `terrain_source_zoom` | integer | zoom the heights came from (§6) |
| `imagery_source_zoom` | integer | zoom the imagery came from (§6) |
| `units` | string | always `"metres"` |
| `up` | string | always `"+Y"` |
| `origin` | string | human-readable frame statement (§3.1) |
| `sources` | object `{imagery, elevation}` | attribution strings (§12) |

Consumers SHOULD read `tile_size_m` and `zoom/x/y` from here for placement. New fields MAY be
added in later versions; consumers MUST ignore unknown fields.

### 8.7 Determinism

For a given builder version, configuration (§10.2) and identical source bytes, the GLB output
is **byte-for-byte reproducible**: JSON key order is fixed, JPEG input passes through untouched,
and all arithmetic is deterministic. This is what allows the ETag of §9.2 to be derived without
hashing the body.

## 9. HTTP interface

### 9.1 Endpoints

| method | path | response |
|---|---|---|
| `GET`, `HEAD` | `/{z}/{x}/{y}.glb` | the tile (§8) |
| `GET` | `/` | JSON service description (§9.4) |
| `GET` | `/healthz` | `ok` (text) |
| `GET` | `/example`, `/example/` | the bundled three.js viewer (HTML) |

The `.glb` suffix is REQUIRED; `/{z}/{x}/{y}` without it, or with another extension, is 400.

### 9.2 Successful tile response

```
HTTP/1.1 200 OK
Content-Type: model/gltf-binary
Content-Length: <bytes>
Cache-Control: public, max-age=31536000, immutable
ETag: "<fingerprint>-<bytes>"
Access-Control-Allow-Origin: *        (unless CORS is disabled)
```

- `fingerprint` is the 16-hex-character configuration hash of §10.2. Because output is
  deterministic (§8.7), `fingerprint + length` identifies the bytes; a config change produces a
  new fingerprint and therefore a new ETag for every tile.
- A request with `If-None-Match` containing a matching ETag (comma-separated list accepted)
  receives **304 Not Modified** with no body and no `Content-Length`.
- `HEAD` returns the same headers with an empty body.
- Tiles are declared `immutable` for one year: consumers and CDNs MAY cache them indefinitely
  under that URL as long as the server's fingerprint is unchanged.

### 9.3 Errors

Error responses carry `Content-Type: application/json` and a body
`{"error": "<message>", "status": <code>}`.

| status | when | `Cache-Control` |
|---|---|---|
| 400 | malformed path, zoom/x/y out of range, invalid resolution | `no-store` |
| 404 | nothing upstream at any zoom (§6) | `public, max-age=3600` |
| 502 | upstream provider failure other than 404 | `no-store` |
| 500 | undecodable source, or cache read/write failure | `no-store` |

### 9.4 Service description (`GET /`)

A JSON object (`Cache-Control: no-cache`) with at least:

```json
{
  "name": "open-tiles", "version": "<crate version>", "fingerprint": "<16 hex>",
  "tiles": "/{z}/{x}/{y}.glb", "example": "/example/",
  "zoom": { "min": 1, "max": 22 },
  "resolution": [ <R for zoom 1>, …, <R for zoom 22> ],
  "conventions": { "units": "metres", "up": "+Y",
                   "origin": "north-west corner; +X east, +Z south",
                   "y": "metres above sea level; place tiles with an X/Z translation only" },
  "sources": { "imagery": "<attribution>", "elevation": "<attribution>" }
}
```

### 9.5 Concurrency

The server MUST build a given tile at most once at a time: concurrent requests for the same tile
share one build (the first request leads; the rest wait for its outcome, including its failure).
The number of concurrent builds across different tiles is bounded (`--max-builds`, default: CPU
count). Nothing here is observable to a client beyond latency.

## 10. Caching

### 10.1 Layout

Both cache tiers share one key space, served by a local directory or an S3 bucket
(`--cache-dir <dir | s3://bucket[/prefix]>`):

```
texture/{zoom}/{x}/{y}.png          raw imagery as received (JPEG bytes may live under .png;
                                    readers sniff the format)
heightmap/{zoom}/{x}/{y}.png        raw Terrarium PNG as received
{kind}/{zoom}/{x}/{y}.png.404       zero-byte marker: the provider answered 404
glb/{fingerprint}/{zoom}/{x}/{y}.glb   built tile (server output cache)
```

The input layout is byte-compatible with the raytiles / bevytiles on-disk caches, so those can
be shared. A real entry always wins over a stale `.404` marker beside it. Markers are removed
with `open-tiles refresh-404`.

Writes are atomic on every backend (temp-file-and-rename locally; single PUT on S3); an empty
entry is treated as a miss and refetched.

### 10.2 Fingerprint

The fingerprint namespaces the output cache and prefixes ETags. It is the first 16 hex
characters of **BLAKE3** over, in order:

1. the crate version string (`CARGO_PKG_VERSION`), UTF-8;
2. the 22 resolution-table entries, each as little-endian `u32`;
3. the imagery URL template, UTF-8, followed by a `0x00` byte;
4. the heightmap URL template, UTF-8, followed by a `0x00` byte;
5. the three bytes `texture_max_zoom`, `heightmap_max_zoom`, `jpeg_quality`.

Everything that can change the bytes of a tile is included; cache location, timeouts and
concurrency are not. Old `glb/<fingerprint>/` trees can simply be deleted.

## 11. Providers

A provider is a URL template with the tokens `:zoom:`, `:x:`, `:y:`; the **first occurrence** of
each token is replaced (identical semantics to the engines). Defaults:

| asset | template | hint |
|---|---|---|
| imagery | `https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/:zoom:/:y:/:x:` | 19 |
| heightmap | `https://s3.amazonaws.com/elevation-tiles-prod/terrarium/:zoom:/:x:/:y:.png` | 15 |

Esri's `zoom/y/x` order is how that service encodes its URLs and is intentional. Responses are
capped at 32 MiB; an empty body is a fetch error.

## 12. Attribution and data terms

Tiles embed provider data. The `extras.sources` strings (defaults: `"Esri World Imagery"` and
`"Mapzen Terrain Tiles (Terrarium) on AWS Open Data"`) MUST be carried through and displayed
where the providers' terms require it. Operators substituting providers SHOULD set matching
attribution strings.

## 13. Command-line exit codes

| code | meaning |
|---|---|
| 0 | success |
| 2 | usage error, invalid tile address, or invalid resolution |
| 3 | nothing upstream at any zoom |
| 4 | network, decode, cache or I/O failure |

## 14. Versioning

This is **version 1** of the tile format, produced by open-tiles 0.1.x. Changes that alter
tile bytes for the same inputs (formulas, resolution table, JPEG quality, GLB layout) MUST bump
the crate version, which changes the fingerprint and thus every ETag and output-cache key.
Additive metadata (new `extras` fields, new service-description fields) is backward compatible.

## Appendix A. Worked example

Tile `12/772/1607` (Grand Canyon, default configuration):

- centre latitude ≈ 36.1° N → `S ≈ 7 908.657 m`;
- heights and imagery both at source zoom 12 (`dz = 0`), `R = 129`: 16 641 vertices,
  32 768 triangles, 16-bit indices;
- GLB ≈ 548 KB, of which the mesh is ≈ 0.5 MB and the JPEG the rest;
- `ETag: "e990d5eb222b8eb1-548816"` for open-tiles 0.1.0 with default settings.

Tile `20/790547/411413` (same defaults): heights derive from zoom 15 (`dz = 5` → ceiling 9),
imagery from zoom 19 (`dz = 1`); `R_used = 9` (configured 9, not capped); ≈ 7 KB.

## Appendix B. Consumer checklist

1. Load the GLB with any glTF 2.0 loader; no extensions needed.
2. Read `extras.zoom/x/y` and `extras.tile_size_m`.
3. Place the node at `(dx · G, 0, dy · G)` and scale it by `G / tile_size_m` in X and Z
   (§3.4, strategy 2) — never scale Y.
4. Compute normals if lighting is wanted; the material is plain PBR and can be swapped.
5. Cache by URL; honour `ETag` / `immutable`.
6. Display `extras.sources`.
