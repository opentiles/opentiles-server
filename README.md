# opentiles-server

On-demand **3D terrain tiles as GLB**, at 1:1 world scale, addressed like a slippy map:
`zoom/x/y`. Tiles are built from public heightmap and imagery providers on first request,
cached, and served with immutable HTTP caching — ready to drop into three.js, Bevy, or any
glTF-capable engine.

- **One GLB per tile**: mesh, JPEG texture, per-vertex normals, PBR material — no extensions,
  no skirts.
- **Any zoom 1–22**: what the providers don't serve is derived from the closest lower zoom.
- **Per-tile metadata**: size, height range, and LOD geometric error as a JSON sidecar.
- **Stateless scaling**: cache on S3 and run any number of replicas.

The tile format — frame, mesh, heights, GLB layout, metadata, HTTP contract — is specified in
[`specs.md`](specs.md).

## Quick start

```sh
cargo run --release -- serve
open http://127.0.0.1:8080/example/
```

The bundled viewer loads a block of tiles around the Grand Canyon; parameters live in the URL.

## HTTP API

```sh
open-tiles serve --bind 0.0.0.0:8080 --cache-dir .cache -v
```

| endpoint             | description                                                              |
|----------------------|--------------------------------------------------------------------------|
| `GET /{z}/{x}/{y}.glb` | the tile — built on first request, then served from the cache          |
| `GET /metadata`      | generate missing tile metadata, streaming progress as SSE (see below)    |
| `GET /`              | service description: version, fingerprint, URL template, conventions     |
| `GET /healthz`       | liveness — answers `ok`                                                  |
| `GET /example/`      | the bundled three.js viewer                                              |

```sh
curl -I http://127.0.0.1:8080/12/772/1607.glb
#   HTTP/1.1 200 OK
#   content-type: model/gltf-binary
#   cache-control: public, max-age=31536000, immutable
#   etag: "<fingerprint>-548760"
#   access-control-allow-origin: *
```

- Tiles are cached at `glb/{fingerprint}/z/x/y.glb`. The *fingerprint* hashes everything that
  changes the bytes (version, resolution table, provider URLs and zoom hints, JPEG quality),
  so a configuration change never serves stale geometry; old fingerprint directories can
  simply be deleted.
- Concurrent requests for one tile share a single build; `--max-builds` (default: CPU count)
  bounds parallel builds. One process, in-memory dedup — no broker.
- Conditional requests: `If-None-Match` → `304`; `HEAD` is supported.
- Errors are JSON bodies: `400` invalid tile · `404` nothing upstream at any zoom
  (`max-age=3600`) · `502` upstream failure · `500` decode/I-O.
- CORS `*` by default (disable with `--no-cors`).
- Provider flags (`--texture-url`, `--heightmap-url`, `--normals-url`, the `--*-max-zoom`
  hints, `--no-normals`, `--timeout`) work as for `build`.

## Tile metadata

Every built tile gets a JSON document with the same name and a `.json` extension
(`glb/{fingerprint}/z/x/y.json`):

```json
{
  "zoom": 12,
  "x": 772,
  "y": 1607,
  "tile_size_m": 7908.657,
  "min_height_m": 731.9,
  "max_height_m": 2385.4,
  "geometric_error_m": 14.2
}
```

| field               | meaning                                                                  |
|---------------------|--------------------------------------------------------------------------|
| `tile_size_m`       | tile edge in metres at the tile's own centre latitude                    |
| `min_height_m` / `max_height_m` | height range of the tile's heightmap, metres above sea level |
| `geometric_error_m` | maximum distance between this tile's surface and its 4 children's at `zoom + 1`; `0` when no finer data exists |

The geometric error is measured, not estimated: the children's heightmaps are compared against
the tile's own surface (evaluated bilinearly at the children's texel centres) and the maximum
absolute difference is recorded. A `0` tells an LOD consumer there is nothing to refine into —
the tile is at (or beyond) the height provider's deepest coverage. A typical screen-space-error
refinement test is:

```
split while  geometric_error_m · screenHeightPx / (2 · distance · tan(fovY / 2))  >  budgetPx
```

Metadata is produced three ways, all writing the identical document:

1. **During every build** — the server writes the document right after caching a freshly built
   tile; `open-tiles build` writes it next to the output file (`tile.glb` → `tile.json`). A
   metadata failure never fails the build; it is logged and left for a later pass.
2. **`open-tiles metadata`** — scans the cache and backfills every built tile that lacks a
   document (existing ones are skipped, never rewritten):

   ```sh
   open-tiles metadata --cache-dir .cache
   #   metadata: 118 written, 3024 skipped, 0 failed
   ```
3. **`GET /metadata`** — the same scan, triggered over HTTP. Progress streams as server-sent
   events, one JSON event per tile and a final summary:

   ```sh
   curl -N http://127.0.0.1:8080/metadata
   #   data: {"tile":"12/772/1607","key":"glb/…/12/772/1607.json","status":"written"}
   #   data: {"tile":"12/772/1608","key":"glb/…/12/772/1608.json","status":"skipped"}
   #   data: {"done":true,"written":1,"skipped":1,"failed":0}
   ```

   Closing the connection stops the scan; only one scan runs at a time (a concurrent request
   answers `409`).

Computing the error needs the 4 child heightmaps, fetched exactly like build inputs: from the
cache when present, from the provider (with write-through and `.404` markers) when not.

## CLI

```sh
cargo build --release

# which tile covers a coordinate?
open-tiles lookup 36.1 -112.1 12
#   tile:      12 772 1607
#   size_m:    7908.657
#   …

# build it (downloads into .cache/, reuses on the next run)
open-tiles build 12 772 1607 -v
#   12-772-1607.glb (548760 bytes)
#   12-772-1607.json

# any zoom works; missing provider zooms derive from the closest lower one
open-tiles build 20 790547 411413 -v
#   INFO  heightmap 20/…: derived from zoom 15 (15/24704/12856)
#   INFO  imagery 20/…: derived from zoom 19 (19/395273/205706)
#   20-790547-411413.glb (7036 bytes)
```

| command       | description                                                        |
|---------------|--------------------------------------------------------------------|
| `build z x y` | build one tile; writes the GLB and its metadata JSON               |
| `serve`       | the HTTP server                                                    |
| `lookup lat lon z` | which tile covers a coordinate, with bounds and size          |
| `metadata`    | backfill missing metadata for every built tile in the cache        |
| `refresh-404` | forget remembered provider 404s (`--zoom N`, `--kind …`)           |

Options for `build`:

| flag                                         | default                 | meaning                                                                              |
|----------------------------------------------|-------------------------|--------------------------------------------------------------------------------------|
| `-o, --output <path>`                        | `./{zoom}-{x}-{y}.glb`  | where to write                                                                       |
| `--cache-dir <dir\|s3://…>`                  | `.cache` (`$CACHE_DIR`) | input cache: a directory (layout-compatible with raytiles/bevytiles) or an S3 bucket |
| `--resolution <n>`                           | per-zoom table (below)  | vertices per edge for this zoom (2..=257)                                            |
| `--texture-url`, `--heightmap-url`, `--normals-url` | Esri / AWS Terrarium / AWS normals | provider templates with `:zoom:` `:x:` `:y:` tokens |
| `--texture-max-zoom`, `--heightmap-max-zoom`, `--normals-max-zoom` | `19` / `15` / `15` | deepest zoom to *ask* the provider for; deeper tiles derive from there |
| `--no-normals`                               | off                     | skip the normals fetch and the NORMAL attribute                                      |
| `--timeout <s>`                              | `10`                    | HTTP read timeout                                                                    |
| `-v` / `-vv`                                 |                         | log fetches, fallbacks and timings                                                   |

The default provider URLs are examples — any XYZ tile source can take their place; see
[Data providers](#data-providers).

Exit codes: `0` ok · `2` usage / invalid tile / bad resolution · `3` nothing upstream at any
zoom · `4` network, decode or I/O failure.

## Fallback: any zoom, from whatever the providers have

Terrarium heightmaps stop at z15; Esri imagery at z19 nearly everywhere. A request at any zoom
starts at `min(zoom, hint)` and, on 404, walks down to the closest lower zoom that exists:

- **heights** — the tile becomes a *window* into its ancestor's padded height field: one
  bilinear interpolation from source texels to vertices, no intermediate image, no cracks at
  ancestor boundaries.
- **imagery** — the ancestor's image is cropped to the tile's sub-square and upscaled to 256².
- **normals** — sampled from the ancestor's normal map through the same window; when *no* zoom
  has one, normals are synthesized from the height field instead of failing the tile.
- A provider 404 leaves a zero-byte `{y}.png.404` marker in the cache so the walk never repeats
  a known-missing request; `refresh-404` deletes the markers.
- `extras.terrain_source_zoom` / `extras.imagery_source_zoom` record what was used.

Watertightness holds between neighbours whose heightmaps resolve to the same source zoom. At a
provider's coverage boundary (one tile at z, its neighbour only at z−1) a crack of up to one
z−1 texel's gradient is possible — that is the dataset's edge.

## Resolution per zoom

Vertex spacing tracks the data: every tile carries 256 source texels while its metre size halves
per zoom. Defaults (vertices per edge), overridable with `--resolution`:

| zoom          | 1–7    | 8–15   | 16     | 17      | 18    | 19    | 20     | 21      | 22    |
|---------------|--------|--------|--------|---------|-------|-------|--------|---------|-------|
| vertices/edge | 257    | 129    | 129    | 65      | 33    | 17    | 9      | 5       | 3     |
| raw mesh      | 3.7 MB | 0.7 MB | 0.7 MB | 0.18 MB | 46 KB | 12 KB | 3.4 KB | <1.5 KB | <1 KB |

Whatever zoom the heights really came from, the value is capped at `(256 >> dz) + 1` — beyond
that a grid only interpolates. `extras.resolution` records the value used
(`resolution_requested` when capped).

## Tile format

- One mesh, one JPEG texture, one plain PBR material, per-vertex normals (no skirts, no
  extensions). Normals come from the providers' normal tiles (tilezen `normal`) or, where
  those end, are derived from the heights; `--no-normals` drops them (~35 % smaller tiles).
- **Frame:** right-handed, Y-up, metres. `+X` east, `+Z` south. Origin at the tile's north-west
  corner; the tile spans `[0, size_m]` in X and Z.
- **Y is metres above sea level.** Every tile shares the same `Y = 0`, so laying tiles out is an
  X/Z translation only: `(x · size_m, 0, y · size_m)`.
- `size_m` is the tile's edge at its *own* centre latitude (`40 075 016.686 · cos(lat) / 2^zoom`).
  Adjacent rows therefore differ slightly (≈1 % at z9, negligible from z14); rescale by
  `extras.tile_size_m` for a strictly uniform grid.
- Same-zoom neighbours are watertight: boundary vertices are sampled over a height field padded
  with the neighbours' edge texels, so both tiles compute the identical value.
- Root `extras`: `zoom, x, y, tile_size_m, bounds{north,south,west,east}, resolution,
  resolution_requested?, terrain_source_zoom, imagery_source_zoom, sources{imagery, elevation}`.

## Caching, and caching on S3

`--cache-dir` (or `$CACHE_DIR`) holds both tiers — provider inputs
(`{texture,heightmap,normal}/z/x/y.png`) and built tiles plus metadata
(`glb/{fingerprint}/z/x/y.{glb,json}`). It is a local directory or **`s3://bucket[/prefix]`**:

```sh
AWS_REGION=eu-central-1 open-tiles serve --cache-dir s3://my-tiles/open-tiles --bind 0.0.0.0:8080
```

With S3 the server is stateless: any number of replicas — or ephemeral containers on Cloud Run /
Fargate — share one cache, and nothing is rebuilt after a restart.

- **Credentials and region** come from the standard AWS environment (`AWS_REGION`,
  `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, `AWS_PROFILE`, or an IAM role — instance
  profile, ECS task role, EKS IRSA). No open-tiles-specific settings.
- **IAM:** `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject` on `bucket/prefix/*` and
  `s3:ListBucket` on the bucket. Without `ListBucket`, S3 answers `403` instead of `404` for
  missing keys; open-tiles treats such a `403` as a miss (warning once) so it still works, but
  granting `ListBucket` keeps real permission problems visible and lets `refresh-404` and the
  metadata scan list keys.
- **TLS:** the S3 client trusts Mozilla's root CAs (bundled) *plus* the platform's — it works
  in a slim container without `ca-certificates` and behind a corporate proxy whose CA is
  installed or named by `SSL_CERT_FILE`.
- **S3-compatible stores** (MinIO, LocalStack, Ceph, R2, …): set `AWS_ENDPOINT_URL` (or
  `AWS_ENDPOINT_URL_S3`); path-style addressing switches on automatically.
- `build`, `metadata` and `refresh-404` accept `--cache-dir s3://…` the same way.
- Each cache operation is one S3 request (a build touches ~10 inputs, in parallel). Concurrent
  writers of one key are harmless: puts are atomic and the bytes identical.
- The S3 backend is the `s3` cargo feature (on by default);
  `cargo build --no-default-features` drops the AWS SDK.

## Docker

```sh
docker run --rm -p 8080:8080 -e AWS_REGION=eu-north-1 \
  -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… ghcr.io/opentiles/opentiles-server:latest
```

The container caches on S3 by default (`$CACHE_DIR`, credentials from the AWS environment or
the platform's IAM role) so replicas stay stateless; `$PORT` is honoured for Cloud Run-style
platforms, and container arguments are appended to `open-tiles serve`. For a local cache
instead, mount a volume:

```sh
docker build -t open-tiles .
docker run --rm -p 8080:8080 -e CACHE_DIR=/data -v open-tiles-cache:/data open-tiles
```

## Library

```rust
use open_tiles::{build_tile, Config, TileId};

let cfg = Config::default();
let tile = TileId::new(12, 772, 1607)?;
let glb = build_tile(&cfg, tile)?;
let meta = open_tiles::metadata::compute(&cfg, tile)?; // TileMetadata
```

`Config` mirrors the engines' `NetworkConfig` (cache, provider URL templates + zoom hints,
timeouts) plus the per-zoom `resolution` table. `Config::with_cache_dir(path)` caches on disk;
`Config { cache: open_tiles::store::open("s3://bucket/prefix")?, ..Default::default() }` caches
in S3 — or implement the `Store` trait for anything else. `load_inputs` exposes the windowed
height field, imagery and source tiles without building the GLB;
`metadata::generate_missing[_with]` is the scan behind the CLI and the SSE endpoint.

## Tests

```sh
cargo test        # fully offline: synthetic Terrarium/imagery fixtures + a local HTTP server
```

`tests/s3_tests.rs` additionally runs against a real bucket when
`OPEN_TILES_TEST_S3=s3://bucket/prefix` is set (any S3-compatible endpoint; the file's header
shows a one-line MinIO setup) and is skipped otherwise.

## Data providers

The built-in provider URLs are **defaults, not a dependency**: imagery © Esri World Imagery,
elevation and normal maps from the Mapzen Terrain Tiles on AWS Open Data (Terrarium /
`normal` encodings). Any slippy-map (XYZ) tile provider works in their place — point
`--texture-url`, `--heightmap-url` and `--normals-url` at a template with `:zoom:`, `:x:`,
`:y:` tokens (heightmaps must be Terrarium-encoded PNGs, normals tilezen-encoded), and set the
matching `--*-max-zoom` hints to the provider's deepest coverage.

Whichever providers you use, **their licenses and terms of use are yours to comply with** —
the tiles you build and serve embed that data (the imagery is baked into every GLB). The
default Esri and Mapzen sources have their own terms. Each tile records attribution strings in
`extras.sources`; they default to the Esri/Mapzen credits, so when you swap providers via the
library, set `Provider::imagery_attribution` / `elevation_attribution` to match.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
