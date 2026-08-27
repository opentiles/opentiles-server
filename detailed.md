# OPEN-TILES — Detailed Plan: Milestones 1 & 2

> Covers **Milestone 1 (builder core)**, **Milestone 2 (CLI)** and **Milestone 3 (greater
> zoom)** from `outline.md`. Milestones 4–5 (server, consumer proof) are out of scope here.
> §0 records the decisions taken on this plan (2026-08-28).
> **Status (2026-08-28): milestones 1, 2 and 3 implemented** (§5 acceptance recorded at its end) — `cargo test` (34 tests, offline)
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
- **No native-zoom concept.** Milestones 1–2 reject heightmap requests above 15; milestone 3
  (§5) replaces that with "closest provided zoom" fallback for every asset at every zoom.

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

## 5. Milestone 3 — Any zoom: fall back to the closest provided zoom

Goal: `open-tiles build <z> <x> <y>` works for every `z ∈ [1, 22]` regardless of what the
providers actually serve there. Decided 2026-08-28: **there is no "native zoom" concept.** Each
provider has a *most-provided zoom* hint; a request at any zoom starts at
`min(zoom, hint)` and, on 404, walks down to the closest lower zoom that exists. The tile is then
derived from that ancestor — heights by windowed sampling (§5.0.1 A, approved), imagery by
crop-and-upscale. This applies uniformly to heightmaps and imagery, and to the tile's neighbours.

### 5.0 Decisions

#### 5.0.1 Heights: sample the ancestor's field directly — **approved (A)**

Build the ancestor's padded 258×258 field (the milestone-1 structure) and give the derived tile a
*window* into it: `u_src = (qx + u) / 2^dz`. One bilinear interpolation from source to vertex,
no intermediate image, no clamp at ancestor edges, nothing written to disk. Derived heightmap PNGs
an engine may have left in a shared `.cache/` are ignored. The engines' `upsample_quadrant` /
`encode_terrarium` are not ported (a reference copy lives in a test to pin interior equivalence).

#### 5.0.2 Per-zoom mesh resolution table — **approved as proposed** (implementation started on "start impl")

Principle: **vertex spacing tracks the data**, and the data per tile is fixed at 256 texels
while the tile's metre size halves per zoom. So resolution should be *high at low zoom* (huge
tiles, few of them, full source fidelity is cheap in aggregate) and *decrease at high zoom*,
where a tile covers only a fraction of a source texel grid. Proposed defaults, vertices per edge:

| zoom | default | vertices | indices | raw mesh | rationale |
|---|---|---|---|---|---|
| 1–7 | **257** | 66 049 | u32 | ≈ 2.9 MB | continental tiles (≥ 300 km edge): ≤ 16 k tiles exist at z7; use every source texel |
| 8–15 | **129** | 16 641 | u16 | ≈ 0.5 MB | the streaming range (150 km → 1 km edge): 2 source texels per vertex — the size/quality point already chosen in §0.3 |
| 16 | 129 | 16 641 | u16 | ≈ 0.5 MB | ceiling: a z16 tile covers 128 source texels (source at z15) |
| 17 | 65 | 4 225 | u16 | ≈ 0.13 MB | covers 64 texels |
| 18 | 33 | 1 089 | u16 | ≈ 33 KB | 32 texels |
| 19 | 17 | 289 | u16 | ≈ 9 KB | 16 texels |
| 20 | 9 | 81 | u16 | ≈ 2.5 KB | 8 texels |
| 21 | 5 | 25 | u16 | < 1 KB | 4 texels |
| 22 | 3 | 9 | u16 | < 1 KB | 2 texels — a bilinear patch |

Rules on top of the table:

- **Ceiling by actual source**: whichever zoom the heightmap really came from (`z_src`, after
  fallback), the effective resolution is `min(table[z], (256 >> (z − z_src)) + 1)`. The table
  above already equals that ceiling for z ≥ 16 when Terrarium serves z15; if Terrarium 404s and
  the source is z14, a z16 tile automatically drops to 65.
- `--resolution <n>` overrides the table for the requested zoom (still subject to the ceiling);
  `Config.resolution: [u32; 22]` holds the table for library users.
- `extras.resolution` records the value used; `extras.resolution_requested` appears when the
  ceiling clamped it.

If you'd rather keep 257 deeper (say through z11 — 20 km tiles, 3 MB each) or 129 shallower,
change the two boundaries; everything else follows from the ceiling rule.

#### 5.0.3 Imagery: derive from the closest lower zoom — **approved**

On 404 at the requested zoom, walk down; take the ancestor image, crop the `256 / 2^dz` texel
sub-window the tile covers, upscale to 256×256 (bilinear — ancestor imagery is already the best
data there is; sharper filters only invent edges), encode JPEG. `extras.imagery_source_zoom`
records the zoom used. Imagery has no seam problem (it's a texture, not geometry) so no neighbour
handling is needed.

#### 5.0.4 Negative cache — needed to make fallback affordable

Without it, building a z22 tile means 7 heightmap 404s (z22→z15) every time, and every
neighbour repeats them. Decision: on 404, write a zero-byte **`{path}.404`** marker next to the
would-be cache entry; `fetch` treats a marker as an instant 404. Markers have no TTL in v1
(providers gain coverage rarely); a `--refresh-404` flag deletes markers under a zoom prefix.
Markers are open-tiles-only files; the engines ignore unknown files in the cache tree.

#### 5.0.5 Provider hints (was "native zoom")

`Provider` gets `heightmap_max_zoom: u8` (default **15**, Terrarium) and `texture_max_zoom: u8`
(default **19**, Esri — it goes deeper in cities, but 19 is the everywhere-safe start point;
tiles above it are derived unless a `--texture-max-zoom` override says to try). They are **start
points for the walk-down, not limits**: a 404 below the hint keeps walking; the hint only avoids
requests that are known to fail. `native_terrain_zoom` is removed.

### 5.1 Conventions added

- Ancestor of `(z, x, y)` at zoom `s < z`: `dz = z − s`, `(s, x >> dz, y >> dz)`; window offset
  inside it `qx = x − (ax << dz)`, `qy = y − (ay << dz)` in `[0, 2^dz)`; window scale `1/2^dz`.
- Height sampling for a derived tile: `(u, v) ↦ ((qx + u)/2^dz, (qy + v)/2^dz)` in the ancestor's
  padded field. The pad ring comes from the ancestor's 8 neighbours *at the ancestor's zoom*;
  a neighbour that 404s there → that side clamps (v1; see the watertightness statement).
- **Watertightness guarantee** (honest statement): two adjacent tiles at the same zoom whose
  heightmaps resolve to the **same source zoom** share an edge exactly. At a provider's coverage
  boundary (tile A resolves at z, its neighbour B only at z−1) the edge can crack by up to one
  z−1 texel's gradient — the dataset edge itself. Not fixed in v1; documented in README.
- `extras`: `terrain_source_zoom`, `imagery_source_zoom`, `resolution`, optional
  `resolution_requested`; `native_terrain` is replaced by `terrain_source_zoom == zoom`.

### 5.2 Steps

#### Step 3.1 — `fetch.rs`: negative cache + walk-down
- `fetch` honours `{path}.404` (instant `NotFound`) and writes it on HTTP 404.
- `fetch_closest(kind, tile, start_zoom) -> (bytes, source: TileId)`: for `z` from
  `min(tile.zoom, start_zoom)` down to 1, try the ancestor at `z`; return the first hit. A network
  error (non-404) at any level aborts (transient errors never silently change which zoom a tile
  is derived from — same rule as neighbours today).
- Tests (local server): 404 at z16 + hit at z15 → returns z15 bytes and `12/…` source; marker
  written; second call makes no request at z16; a 500 at z16 → `Fetch` error, no marker.

#### Step 3.2 — `terrain.rs`: windowed field
- `HeightField { data: Arc<[f32]>, size, window: Window { u0, v0, scale } }`; `sample` maps
  through the window; `HeightField::windowed(&self, dz, qx, qy)` derives a child view.
- Tests: NW z16 child at `(1, 1)` = ancestor at `(0.5, 0.5)`; two z17 tiles across a z15
  boundary agree on the shared edge (two-ancestor ramp fixture); interior texel equals a
  test-local port of the engines' `upsample_quadrant`.

#### Step 3.3 — `imagery.rs` (new): crop-and-upscale
- `derive_imagery(ancestor_bytes, dz, qx, qy) -> jpeg`: decode → crop `256>>dz` square at
  `(qx, qy)·(256>>dz)` → resize to 256² (bilinear / `image::imageops::resize` with `Triangle`)
  → JPEG at `Config.jpeg_quality`.
- Tests: dz = 1 crop of a 2×2 checker gives a solid colour; dz = 0 is a byte pass-through;
  output is 256² JPEG.

#### Step 3.4 — `builder.rs`: routing + resolution table
- `Config.resolution` becomes `[u32; 22]` (index `zoom − 1`) with the §5.0.2 defaults;
  `Config::resolution_for(zoom)`; `check_resolution` applies to every entry.
- `load_inputs`: heightmap via `fetch_closest` with `heightmap_max_zoom`; the 8 neighbours are
  the *source* tile's neighbours; imagery via `fetch_closest` with `texture_max_zoom` then
  `derive_imagery` when `dz > 0`. `Error::AboveNativeZoom` is deleted.
- `build_tile`: effective resolution = ceiling rule; `TileMeta` gains the new extras.
- Tests: z16 from a seeded z15 cache — no z16 heightmap request, `terrain_source_zoom = 15`,
  geometry equals direct sub-window sampling; z18 mesh is 33×33; a z12 tile whose imagery
  404s at z12 but exists at z11 embeds an upscaled crop and reports `imagery_source_zoom = 11`;
  a tile with nothing at any zoom → exit 3 / `NotFound`.

#### Step 3.5 — CLI & docs
- `--resolution` (override for this zoom), `--heightmap-max-zoom`, `--texture-max-zoom`,
  `--refresh-404 <zoom-prefix>`; README: fallback rules, resolution table, the watertightness
  statement; `detailed.md` status.

#### Step 3.6 — Acceptance
- Grand Canyon: the 16 z16 tiles under z14 `3090/6428` (spans several z15 ancestors), one z18,
  one z20, one z22; validator clean on all; viewer wireframe at grazing angles across z15
  ancestor boundaries — no cracks; z18 = 33×33; z20/z22 imagery derived (blurry but present,
  `imagery_source_zoom` ≤ 19) rather than exit 3.
- Low zoom: z3, z5, z7 tiles build at 257 (u32 indices, ≈ 3 MB) and validate.
- Negative cache: second build of the z22 tile makes zero HTTP requests (`-vv` shows only
  cache hits and markers).

**Milestone 3 exit criteria:** acceptance passes; `cargo test` green offline; no derived
heightmap or imagery files are written under `.cache/` (only native entries and `.404` markers).

**Implementation notes [impl]:** `refresh-404` is a subcommand (`open-tiles refresh-404
[--zoom] [--kind]`), not a `build` flag. `Config.resolution` is `[u32; 22]` with
`resolution_for` / `set_resolution` / `with_uniform_resolution`. `HeightField` shares its data
via `Arc` and composes windows (`windowed(dz, qx, qy)`). `native_terrain` in `extras` is
replaced by `terrain_source_zoom` + `imagery_source_zoom`. 47 tests, offline.

**Acceptance run (2026-08-28):** 16 z16 tiles under z14 `3090/6429` (Grand Canyon, x 12360–12363,
y 25716–25719; spans four z15 ancestors), z18 `49445/102870` (heights z15, imagery native z18,
33×33), z20 (heights z15, imagery z19, 9×9), z22 (imagery z19, 3×3), z3/z5/z7 (257², ≈2.9 MB
each): `gltf-validator` 0 errors/warnings on every file; second build of the z22 tile made zero
downloads; no files under `heightmap/16+`; three.js viewer of the z16 block: continuous textured
surface and an unbroken wireframe across the z15 ancestor boundaries. (A first run of this block
was accidentally built at tile numbers shifted to 91°E — Tibet, ~5 000 m — which also validated
clean and derived correctly; discarded for the record.) Sea-level check (Ziv's suggestion): a
3×3 z14 block on the Tel Aviv shoreline — the all-water tile `14/9773/6648` has Y −0.0…0.1 m
and the coast sits exactly on a Y = 0 reference grid in the viewer; land tiles rise to 50–150 m.

---

## 6. Explicitly deferred (not in these milestones)

- HTTP server, output cache, in-flight dedup (milestone 4).
- Antimeridian wrap for neighbour lookup.
- Fixing cracks at a provider's coverage boundary (§5.1 watertightness statement).
- TTL / automatic expiry for `.404` markers.
- Mesh compression / quantisation extensions; lighting extensions (`KHR_materials_unlit`).
