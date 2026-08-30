# open-tiles

On-demand **3D terrain tiles as GLB**, at 1:1 world scale, addressed like a slippy map:
`zoom/x/y`.

The tile format itself — frame, mesh, heights, GLB layout,
metadata, HTTP contract — is specified in [`specs.md`](specs.md).

## Try it

```sh
cargo run --release -- serve
open http://127.0.0.1:8080/example/
```
## Server

```sh
target/release/open-tiles serve --bind 0.0.0.0:8080 --cache-dir .cache -v
curl -I http://127.0.0.1:8080/12/772/1607.glb
#   HTTP/1.1 200 OK
#   content-type: model/gltf-binary
#   cache-control: public, max-age=31536000, immutable
#   etag: "<fingerprint>-548760"
#   access-control-allow-origin: *
```

- `GET /{z}/{x}/{y}.glb` — built on first request, then served from
  `glb/{fingerprint}/z/x/y.glb` in the cache. The *fingerprint* hashes everything that changes the
  bytes (version, resolution table, provider URLs and zoom hints, JPEG quality), so a config
  change never serves stale geometry — old fingerprint directories can simply be deleted.
- `--cache-dir` (or `$CACHE_DIR`) is a local directory **or `s3://bucket[/prefix]`** — see
  "Cache on S3" below. Both tiers (provider inputs and built tiles) live there.
- Concurrent requests for one tile share a single build; `--max-builds` (default: CPU count)
  bounds parallel builds. No broker — one process, in-memory dedup.
- `If-None-Match` → `304`; `HEAD` supported; `400` invalid tile; `404` nothing upstream at any
  zoom (`max-age=3600`); `502` upstream failure; `500` decode/I/O. JSON error bodies.
- CORS `*` by default (`--no-cors`). `GET /` returns name, version, fingerprint, URL template,
  resolution table, conventions and attribution; `GET /healthz` → `ok`; `GET /example/` is the
  bundled viewer.
- Provider flags (`--texture-url`, `--heightmap-url`, `--texture-max-zoom`,
  `--heightmap-max-zoom`, `--timeout`) work as for `build`.

## Docker

```sh
docker run --rm -p 8080:8080 -e AWS_REGION=eu-north-1 \
  -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… ghcr.io/opentiles/opentiles-server:latest
# or build locally:
docker build -t open-tiles .
docker run --rm -p 8080:8080 -e AWS_REGION=eu-north-1 -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… open-tiles
curl -I http://127.0.0.1:8080/12/772/1607.glb
```
## Cache on S3

```sh
AWS_REGION=eu-central-1 open-tiles serve --cache-dir s3://my-tiles/open-tiles --bind 0.0.0.0:8080
```

Point `--cache-dir` (or `CACHE_DIR`) at `s3://bucket[/prefix]` and both cache tiers become
objects in that bucket, with the same key layout as the directory version
(`{prefix}/texture/z/x/y.png`, `{prefix}/glb/{fingerprint}/z/x/y.glb`, `.404` markers). That
makes the server stateless: any number of replicas — or ephemeral containers on Cloud Run /
Fargate — share one cache, and nothing is rebuilt after a restart.

- **Credentials and region** come from the standard AWS environment: `AWS_REGION`,
  `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, `AWS_PROFILE`, or an IAM role (EC2 instance
  profile, ECS task role, EKS IRSA). No open-tiles-specific settings.
- **IAM:** `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject` on `bucket/prefix/*` and
  `s3:ListBucket` on the bucket. Without `ListBucket`, S3 answers `403` instead of `404` for
  keys that don't exist; open-tiles treats such a `403` as a miss (and warns once at startup)
  so it still works, but grant `ListBucket` so real read-permission problems stay visible and
  `refresh-404` can list markers.
- **TLS:** the S3 client trusts Mozilla's root CAs (bundled in the binary) *plus* the
  platform's — so it works in a slim container without `ca-certificates`, and behind a
  corporate proxy whose CA is installed or named by `SSL_CERT_FILE` (which, on its own, would
  replace the platform roots and break the connection).
- **S3-compatible stores** (MinIO, LocalStack, Ceph, R2…): set `AWS_ENDPOINT_URL`
  (or `AWS_ENDPOINT_URL_S3`); path-style addressing is switched on automatically.
- `open-tiles build --cache-dir s3://…` and `refresh-404 --cache-dir s3://…` work the same way.
- Each cache operation is one S3 request (a build touches ~10 inputs, in parallel). Concurrent
  writers of the same key are harmless: puts are atomic and the bytes are identical.
- The S3 backend is the `s3` cargo feature (on by default);
  `cargo build --no-default-features` drops the AWS SDK.

Tests: `tests/s3_tests.rs` runs against a real bucket when `OPEN_TILES_TEST_S3=s3://bucket/prefix`
is set (any S3-compatible endpoint; the file's header shows a one-line MinIO setup) and is
skipped otherwise.

## CLI

```sh
cargo build --release

# which tile covers a coordinate?
target/release/open-tiles lookup 36.1 -112.1 12
#   tile:      12 772 1607
#   size_m:    7908.657
#   ...

# build it (downloads into .cache/, reuses on the next run)
target/release/open-tiles build 12 772 1607 -v
#   12-772-1607.glb (548760 bytes)

# any zoom works; what the providers don't serve is derived from the closest lower zoom
target/release/open-tiles build 20 790547 411413 -v
#   INFO  heightmap 20/…: derived from zoom 15 (15/24704/12856)
#   INFO  imagery 20/…: derived from zoom 19 (19/395273/205706)
#   20-790547-411413.glb (7036 bytes)
```

Options for `build`:

| flag                                         | default                 | meaning                                                                              |
|----------------------------------------------|-------------------------|--------------------------------------------------------------------------------------|
| `-o, --output <path>`                        | `./{zoom}-{x}-{y}.glb`  | where to write                                                                       |
| `--cache-dir <dir\|s3://…>`                  | `.cache` (`$CACHE_DIR`) | input cache: a directory (layout-compatible with raytiles/bevytiles) or an S3 bucket |
| `--resolution <n>`                           | per-zoom table (below)  | vertices per edge for this zoom (2..=257)                                            |
| `--texture-url`, `--heightmap-url`           | Esri / AWS Terrarium    | provider templates with `:zoom:` `:x:` `:y:` tokens                                  |
| `--texture-max-zoom`, `--heightmap-max-zoom` | `19` / `15`             | deepest zoom to *ask* the provider for; deeper tiles derive from there               |
| `--timeout <s>`                              | `10`                    | HTTP read timeout                                                                    |
| `-v` / `-vv`                                 |                         | log fetches, fallbacks and timings                                                   |

Exit codes: `0` ok · `2` usage / invalid tile / bad resolution · `3` nothing upstream at any
zoom · `4` network, decode or I/O failure.

`open-tiles refresh-404 [--zoom N] [--kind texture|heightmap]` forgets remembered 404s (see
"Fallback" below).

## Fallback: any zoom, from whatever the providers have

Terrarium heightmaps stop at z15; Esri imagery at z19 nearly everywhere. A request at any zoom
starts at `min(zoom, hint)` and, on 404, walks down to the closest lower zoom that exists:

- **heights** — the tile becomes a *window* into its ancestor's padded height field: one
  bilinear interpolation from source texels to vertices, no intermediate image, no cracks at
  ancestor boundaries. Nothing is written to disk.
- **imagery** — the ancestor's image is cropped to the tile's sub-square and upscaled to 256².
- A provider 404 leaves a zero-byte `{y}.png.404` marker in the cache so the walk never repeats
  a known-missing request; `refresh-404` deletes markers.
- `extras.terrain_source_zoom` / `extras.imagery_source_zoom` record what was used.

Watertightness holds between neighbours whose heightmaps resolve to the same source zoom. At a
provider's coverage boundary (one tile at z, its neighbour only at z−1) a crack of up to one
z−1 texel's gradient is possible — that is the dataset's edge.

## Resolution per zoom

Vertex spacing tracks the data: every tile carries 256 source texels while its metre size halves
per zoom. Defaults (vertices per edge), overridable with `--resolution` for a zoom:

| zoom          | 1–7    | 8–15   | 16     | 17      | 18    | 19   | 20     | 21    | 22    |
|---------------|--------|--------|--------|---------|-------|------|--------|-------|-------|
| vertices/edge | 257    | 129    | 129    | 65      | 33    | 17   | 9      | 5     | 3     |
| raw mesh      | 2.9 MB | 0.5 MB | 0.5 MB | 0.13 MB | 33 KB | 9 KB | 2.5 KB | <1 KB | <1 KB |

Whatever zoom the heights really came from, the value is capped at `(256 >> dz) + 1` — beyond
that a grid only interpolates. `extras.resolution` records the value used
(`resolution_requested` when capped).

## What's in a tile

- One mesh, one JPEG texture, one plain PBR material (no normals, no skirts, no extensions).
- **Frame:** right-handed, Y-up, metres. `+X` east, `+Z` south. Origin at the tile's north-west
  corner; the tile spans `[0, size_m]` in X and Z.
- **Y is metres above sea level.** Every tile shares the same `Y = 0`, so laying tiles out is an
  X/Z translation only: `(x · size_m, 0, y · size_m)`.
- `size_m` is the tile's edge at its *own* centre latitude (`40 075 016.686 · cos(lat) / 2^zoom`).
  Adjacent rows therefore differ slightly (≈1 % at z9, negligible from z14); rescale by
  `extras.tile_size_m` if you want a strictly uniform grid.
- Same-zoom neighbours are watertight: boundary vertices are sampled over a height field padded
  with the neighbours' edge texels, so both tiles compute the identical value.
- Root `extras`: `zoom, x, y, tile_size_m, bounds{north,south,west,east}, resolution,
  resolution_requested?, terrain_source_zoom, imagery_source_zoom, sources{imagery, elevation}`.

## Library

```rust
use open_tiles::{build_tile, Config, TileId};

let glb = build_tile( & Config::default (), TileId::new(12, 772, 1607) ? ) ?;
```

`Config` mirrors the engines' `NetworkConfig` (cache, provider URL templates + zoom hints,
timeouts) plus the per-zoom `resolution` table. `Config::with_cache_dir(path)` caches on disk;
`Config { cache: open_tiles::store::open("s3://bucket/prefix")?, ..Default::default() }` caches in
S3 — or implement the `Store` trait for anything else. `load_inputs` exposes the windowed height field,
imagery and the source tiles without building the GLB.

## Tests

```sh
cargo test        # fully offline: synthetic Terrarium/imagery fixtures + a local HTTP server
```

## Data

Imagery © Esri; elevation from the Mapzen Terrain Tiles on AWS Open Data (Terrarium encoding).
Mind their terms of use — the tiles you build embed that data.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
