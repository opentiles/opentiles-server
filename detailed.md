# OPEN-TILES — Detailed Plan: Milestones 1 & 2

> Covers **Milestone 1 (builder core)** and **Milestone 2 (CLI)** from `outline.md`.
> Milestones 3–5 (greater zoom, server, consumer proof) are out of scope here.
> §0 records the decisions taken on this plan (2026-08-28).
> **Status (2026-08-28): milestones 1 and 2 implemented** — `cargo test` (34 tests, offline)
> green, clippy/fmt clean, 3×3 Grand Canyon block at z12 validated (official glTF validator:
> 0 errors/warnings on all 9 files) and inspected in a three.js viewer — watertight seams,
> imagery aligned with relief, Y range 707–2 489 m. Deviations from the plan are marked **[impl]**.

---

## 0. Decisions (approved)

These are the places where the engines' conventions don't transfer 1:1 to a self-contained tile.

### 0.1 Which latitude sets the tile's metre size?

The engines compute **one** tile size for the whole world from the *anchor's* latitude
(`WorldConfig::from_lat_lon`: `tile_size = 40 075 016.686 · cos(lat) / 2^9`) and halve it per zoom.
Every tile in a session shares that scale, so they stitch perfectly on a flat plane.

A server tile knows no anchor. Options:

| Option | Behaviour | Downside |
|---|---|---|
| **A. Tile's own centre latitude** (recommended) | Each GLB is exactly 1:1 for its own location. | Rows differ in size: adjacent rows mismatch by ~1.2 % of the edge at z9 (~800 m), ~0.03 % at z14 (~0.6 m), negligible at z16+. |
| B. Caller-supplied anchor latitude (`?lat=` query / CLI flag) | Engine-identical stitching. | Breaks "one URL = one cached file"; the same tile would need a cache entry per anchor. |

**Decision: A**, with `extras.tile_size_m` and the lat/lon bounds in the GLB so a client that
wants a uniform grid (e.g. a future bevytiles backend) can rescale X/Z by
`anchor_size / tile_size_m` — a per-node scale, no geometry rewrite. The row mismatch at low zoom
is inherent to laying Mercator tiles on a plane and is the client's LOD/placement problem, same as
cross-zoom cracks.

### 0.2 Same-zoom edge continuity without skirts

Terrarium texels are *areas*: the shared edge between tiles A and B lies between A's texel 255 and
B's texel 0. If each tile samples only its own heightmap, the two tiles' boundary vertices differ
by up to one texel's height difference → visible cracks between same-zoom neighbours (which the
engines hide with `skirt_overlap`; we removed skirts).

**Decision:** the builder fetches the **8 neighbouring heightmaps** (cheap: they go into
the same input cache and will be needed for those tiles anyway) and samples the vertex grid over a
**258×258 padded height field**. Boundary vertices then evaluate to the *same* value from either
side, so same-zoom neighbours are watertight by construction. Missing neighbours (404, dataset
edge) fall back to edge-clamped sampling for that side. Cross-zoom seams remain the client's job.

### 0.3 Mesh resolution

The height data is 256 texels per edge; a grid finer than that only interpolates, so the useful
ceiling is 257 vertices per edge. `--resolution` is expressed in **vertices per edge**:

| vertices/edge | quads | vertices | triangles | positions + UVs + indices (raw) |
|---|---|---|---|---|
| 65 | 64 | 4 225 | 8 192 | ≈ 0.13 MB (u16 indices) |
| **129** | 128 | 16 641 | 32 768 | ≈ 0.53 MB (u16 indices) |
| 257 | 256 | 66 049 | 131 072 | ≈ 2.9 MB (u32 indices — 66 049 > 65 535) |

Plus a ~15–40 KB JPEG. **Decision: default 129**, `--resolution <n>` (2..=257) to override. Index
type is chosen per file: u16 when the vertex count fits, u32 otherwise.

### 0.4 Material

**Decision: no normals in v1** — no normal map, no vertex normals, and no lighting extension or
flag. The material is a plain `pbrMetallicRoughness` with the imagery as `baseColorTexture`
(`metallicFactor 0`, `roughnessFactor 1`); lit viewers compute flat normals per the glTF spec.

---

## 1. Coordinate and data conventions (fixed)

- **Tile id**: web-mercator XYZ, `zoom ∈ [1, 22]`, `x, y ∈ [0, 2^zoom)`. Public `y` = engines'
  internal `z`. (The engines start at 9 for LOD reasons; a tile server has no reason to — low
  zooms are just big tiles.)
- **Frame**: right-handed, **Y-up, metres**. `+X` east, `+Z` south (slippy `y` increases
  southward — same as the engines' `z`). Origin at the tile's **north-west corner**; the tile spans
  `[0, size]` in X and Z. Placing tile `(x, y)` in a uniform grid is `translation = (x·size, 0,
  y·size)` — X/Z only.
- **Y = metres above sea level**, from Terrarium, unscaled. Root node identity.
- **Winding**: counter-clockwise seen from `+Y` (front faces point up).
- **UVs**: `u = X/size`, `v = Z/size`; glTF's UV origin is top-left, which matches image row 0 =
  north for both Esri imagery and Terrarium, so no flip.
- **Height sampling**: bilinear, texel centres at `(i + 0.5)/256`, over the padded field of §0.2;
  `f32` throughout (not the engines' `u16` whole-metre query grid — that quantisation exists for
  runtime memory, which we don't care about).
- **Terrarium decode**: `h = r·256 + g + b/256 − 32768` — ported from bevytiles `synth.rs`
  after review (evaluated in `f64`, narrowed once; no Bevy dependency).
- **Inputs per tile**: heightmap `{cache}/heightmap/{z}/{x}/{y}.png` (+ 8 neighbours), imagery
  `{cache}/texture/{z}/{x}/{y}.png` (Esri actually returns JPEG; the engines keep the `.png` name
  and sniff the format — we do the same so caches are interchangeable with raytiles/bevytiles).
- **Provider URL templates** with `:zoom:`, `:x:`, `:y:` tokens; Esri's default is `zoom/y/x`
  order on purpose. Defaults identical to the engines' `NetworkConfig`.
- **Native terrain zoom = 15.** In milestones 1–2 a request above 15 is rejected with a clear
  error; synthesis arrives in milestone 3.

---

## 2. Crate layout

Single Cargo package `open-tiles` = one library + one binary. The library is what the server
(milestone 4) will consume; the CLI is its first client.

```
open-tiles/
├── Cargo.toml
├── src/
│   ├── lib.rs          re-exports; `build_tile(&Config, TileId) -> Result<Vec<u8>>`
│   ├── tile.rs         TileId, validation, metre size, lat/lon bounds, lat/lon → tile lookup
│   ├── provider.rs     Provider { texture_url, heightmap_url, native_zoom }; expand_url
│   ├── fetch.rs        cache-or-HTTP raw bytes; atomic write-through; size cap; timeouts
│   ├── terrain.rs      Terrarium decode; HeightField (padded, bilinear sample)
│   ├── mesh.rs         grid → positions / uvs / indices
│   ├── glb.rs          GLB writer (JSON chunk + BIN chunk), extras, material
│   ├── builder.rs      orchestrates fetch → decode → mesh → glb; the one public entry point
│   └── bin/
│       └── open-tiles.rs   clap CLI: `build`, `lookup`
├── tests/
│   ├── common/mod.rs   synthetic Terrarium/imagery fixtures generated in code, seeded caches,
│   │                   local tiny_http server, GLB container parser  [impl: no fixture files]
│   ├── fetch_tests.rs  cache/HTTP behaviour against the local server
│   ├── build_tests.rs  load_inputs + build_tile + GLB validity/determinism (gltf crate)
│   └── cli_tests.rs    exit codes, lookup, end-to-end build from a seeded cache
│   (tile / mesh / terrain / provider have unit tests in their modules)
└── README.md
```

### Dependencies

| crate | why |
|---|---|
| `ureq 2` | blocking HTTP; same as bevytiles' native backend |
| `image 0.25` (`png`, `jpeg` only) | decode inputs, re-encode imagery to JPEG when the provider sent PNG |
| `serde` + `serde_json` | hand-written glTF JSON (a terrain tile is one mesh / one material — ~150 lines; avoids the `gltf-json` builder API) |
| `clap 4` (derive) | CLI |
| `thiserror` | library error type; `anyhow` in the binary only |
| `gltf 1.4` (dev-dep) | parse our own output in tests to prove it's valid glTF |
| `tempfile` (dev-dep) | scratch cache dirs |
| `tiny_http` (dev-dep) | local HTTP server for offline fetch tests |
| `log` | library logging facade; the CLI installs a 10-line stderr logger for `-v` **[impl]** |

No async runtime in milestones 1–2 (the CLI is sequential; neighbour fetches use a small
`std::thread::scope` fan-out). The server milestone chooses its runtime independently.

---

## 3. Milestone 1 — Builder core

Goal: a library that, given a `TileId` and config, produces the decoded inputs (padded height
field + imagery bytes). No GLB yet; fully tested offline.

### Step 1.1 — Scaffold
- `cargo init`, dependencies above, `#![warn(missing_docs)]`, `rustfmt`/`clippy` clean.
- `Config { cache_dir, provider, resolution, connect_timeout, read_timeout }` with defaults
  mirroring the engines (`.cache`, Esri/Terrarium URLs, 5 s / 3 s).
- Error enum: `InvalidTile`, `AboveNativeZoom`, `Fetch { url, source }`, `Decode`, `Io`.

### Step 1.2 — `tile.rs`
- `TileId { zoom: u8, x: u32, y: u32 }` with validation (`1 ≤ zoom ≤ 22`, `x, y < 2^zoom`).
- `bounds() -> (lat_n, lon_w, lat_s, lon_e)` via inverse Mercator.
- `size_m()` = `EQUATOR_CIRCUMFERENCE_M · cos(lat_centre) / 2^zoom` (§0.1 option A).
- `TileId::from_lat_lon(lat, lon, zoom)` — the engines' forward formula.
- `neighbours() -> [Option<TileId>; 8]` (None at the antimeridian/poles: no wrap in v1).
- Tests: known coordinates round-trip; `size_m` at the equator = `40 075 016.686 / 2^z`; matches
  bevytiles' `from_lat_lon` numbers for the Dolomites / Grand Canyon defaults.

### Step 1.3 — `provider.rs` + `fetch.rs`
- `expand_url` — token replacement identical to the engines' (`replacen`, one each).
- `fetch_bytes(kind, tile) -> Vec<u8>`: cache hit → read; miss → GET, cap 32 MiB, atomic
  write (`tmp{counter}` + rename, ported from bevytiles `write_atomic`), return bytes.
- Cache path `{cache}/{texture|heightmap}/{z}/{x}/{y}.png` — byte-compatible with the engines'
  caches (a pre-warmed raytiles `.cache/` works as-is).
- HTTP 404 is a typed error (the server milestone maps it to its own 404); other statuses and
  timeouts are `Fetch` errors.
- Tests: local `tiny_http`/`ureq`-served fixture (like the engines' `tile_source_tests`):
  cache miss → file written; second call → no HTTP; 404 → typed error; corrupt bytes → `Decode`.

### Step 1.4 — `terrain.rs`
- `decode_terrarium(&RgbImage) -> Vec<f32>` (verbatim from bevytiles).
- `HeightField { w, h, data }` built as **258×258**: centre = the tile, 1-texel ring = the
  adjacent row/column from each of the 8 neighbours (corner neighbours contribute one texel).
  Missing neighbour → replicate the tile's own edge texel (clamp).
- `sample(u, v) -> f32`: bilinear, texel-centre convention, over the padded field so `u = 0`
  and `u = 1` land exactly between the two tiles' edge texels.
- Tests: decode known pixels (0 m, 8848 m, −415 m, the g-carry case from bevytiles); a two-tile
  synthetic ramp yields identical values at the shared edge from either tile's field.

### Step 1.5 — `builder.rs` (inputs half)
- `load_inputs(cfg, tile) -> TileInputs { height: HeightField, imagery: Vec<u8>, imagery_mime }`.
- Fetches heightmap + 8 neighbours + imagery; all 10 fetches run together in a scoped thread
  fan-out **[impl: no concurrency cap — 10 requests per build is fine for the CLI; the server
  milestone owns global limits]**. Imagery is sniffed: JPEG passes through untouched; PNG is
  re-encoded to JPEG (quality 90) so the GLB always embeds `image/jpeg`.
- Neighbour policy **[impl]**: a neighbour that 404s is the dataset edge → that side clamps;
  any *other* neighbour failure fails the build, so a transient error can never yield a
  different mesh that then gets cached as the tile.
- Corner pad texels **[impl]**: when a diagonal neighbour is missing, fall back to the E/W
  neighbour's corner texel, then N/S, then own — the boundary vertex touches the corner pad, so
  the fallback must be a texel both tiles sharing the edge can see.
- Rejects `zoom > native_zoom` with `AboveNativeZoom` (milestone 3 removes this).

**Milestone 1 exit criteria:** `cargo test` green offline; `load_inputs` on a pre-warmed cache
returns a 258×258 field with plausible heights for a known tile (e.g. Grand Canyon z12).

---

## 4. Milestone 2 — CLI producing a GLB

Goal: `open-tiles build <zoom> <x> <y>` writes a valid, correctly scaled `.glb`; verified in a
stock viewer before any server work.

### Step 2.1 — `mesh.rs`
- `build_grid(field, size_m, resolution) -> Grid { positions: Vec<[f32;3]>, uvs: Vec<[f32;2]>,
  indices: Vec<u32> }` (`resolution` = vertices per edge, `n = resolution − 1` quads).
- Vertex `(i, j)`, `i, j ∈ [0, n]`: `X = i/n · size`, `Z = j/n · size`, `Y = field.sample(i/n,
  j/n)`, `uv = (i/n, j/n)`.
- Two CCW-from-above triangles per quad: `(a, c, b)` and `(b, c, d)` with `a=(i,j) b=(i+1,j)
  c=(i,j+1) d=(i+1,j+1)`.
- Track `min/max` per axis while generating (glTF requires them on POSITION accessors).
- Tests: vertex/index counts; corners at `(0,·,0)` and `(size,·,size)`; every triangle's
  normal has `+Y`; two adjacent tiles built from the synthetic ramp share bit-identical boundary
  vertex heights.

### Step 2.2 — `glb.rs`
Layout of the single binary buffer (all 4-byte aligned):

```
[ positions f32×3 ] [ uvs f32×2 ] [ indices u16|u32 ] [ jpeg bytes (padded) ]
```

JSON chunk:
- `asset { version: "2.0", generator: "open-tiles x.y.z" }`
- one `buffer`, four `bufferViews` (targets ARRAY / ARRAY / ELEMENT_ARRAY / none)
- accessors: POSITION (VEC3, min/max), TEXCOORD_0 (VEC2), indices (SCALAR, u16 if the vertex
  count ≤ 65 535 else u32)
- one `image { bufferView, mimeType: image/jpeg }`, one `sampler` (LINEAR / LINEAR_MIPMAP_LINEAR,
  CLAMP_TO_EDGE ×2), one `texture`
- one `material`: `pbrMetallicRoughness { baseColorTexture, metallicFactor 0, roughnessFactor 1 }`
  — nothing else (§0.4)
- one `mesh` / one `primitive` (TRIANGLES), one `node` (identity), one `scene`
- `extras` on the root: `{ zoom, x, y, tile_size_m, bounds: {north, south, west, east},
  resolution, native_terrain: true, sources: { imagery: "Esri World Imagery", elevation:
  "Mapzen Terrain Tiles (Terrarium)" }, generator_version }`
- GLB header + chunk headers per spec (JSON padded with spaces, BIN with zeros).

Tests: output parses with the `gltf` crate; accessor counts match the grid; POSITION min/max
equal the grid's; image bytes round-trip; file validates with `gltf-validator` in CI when the tool
is present (skipped otherwise).

### Step 2.3 — `builder.rs` (full)
- `build_tile(cfg, tile) -> Result<Vec<u8>>` = `load_inputs` → `build_grid` → `write_glb`.
- Deterministic: same inputs + config → byte-identical output (needed later for cache
  validation; enforced by a test).

### Step 2.4 — CLI (`src/bin/open-tiles.rs`)

```
open-tiles build <zoom> <x> <y> [options]
    -o, --output <path>        default ./{zoom}-{x}-{y}.glb
    --cache-dir <dir>          default .cache   (shareable with raytiles/bevytiles)
    --resolution <vertices>    vertices per edge, default 129, range 2..=257
    --texture-url <template>   --heightmap-url <template>
    --timeout <secs>
    -v                         log fetches (cache hit/miss, bytes, ms)

open-tiles lookup <lat> <lon> <zoom>
    prints x y, bounds, and tile_size_m — the helper for picking tiles to build
```

Exit codes: 0 ok · 2 usage / invalid tile · 3 upstream 404 · 4 network/decode failure.
Output is written atomically (tmp + rename).

### Step 2.5 — Tests & verification
- `cli_tests.rs`: run the binary against a seeded temp cache (fixtures) → file exists, parses,
  extras carry the requested zoom/x/y; invalid tile → exit 2; z16 → `AboveNativeZoom` message.
- **Manual acceptance (the milestone gate):**
  1. `open-tiles lookup 36.1 -112.1 12` (Grand Canyon) → build the 3×3 block around it.
  2. Open the nine files in a stock viewer (Blender import, or a three.js/donmccurdy viewer
     page), translate each by `(x·size, 0, y·size)`: edges line up, no gaps, relief looks right,
     imagery aligned with terrain (canyon rim matches the texture).
  3. `gltf-validator` reports zero errors.
  4. Dimensions check: reported `tile_size_m` ≈ 7.9 km at z12 for that latitude; a vertex's Y
     over the rim ≈ 2 100 m, river ≈ 700 m.

**Milestone 2 exit criteria:** the acceptance run above passes; `cargo test` green offline;
README documents the CLI and the conventions in §1.

**Acceptance run (2026-08-28):** `lookup 36.1 -112.1 12` → `12 772 1607`, `size_m 7908.657`.
Built the 3×3 block (x 771–773, y 1606–1608): ~545 KB each, 16 641 vertices, u16 indices,
~9 s first build (10 downloads), <1 ms mesh+GLB; Y ranges 707–2 489 m (river / rim).
`gltf-validator` 2.0.0-dev.3.10: 0 errors, 0 warnings, 0 infos on all nine. three.js viewer
with tiles translated by `(x·size, 0, y·size)`: continuous surface top-down, at grazing angles,
and in wireframe across the row boundary.

---

## 5. Explicitly deferred (not in these two milestones)

- Heightmap synthesis above z15 and lineage backfill (milestone 3).
- HTTP server, output cache, in-flight dedup (milestone 4).
- Antimeridian wrap for neighbour lookup.
- Texture upsampling from deeper-zoom imagery.
- Mesh compression / quantisation extensions; lighting extensions (`KHR_materials_unlit`).
