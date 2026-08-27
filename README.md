# open-tiles

On-demand **3D terrain tiles as GLB**, at 1:1 world scale, addressed like a slippy map:
`zoom/x/y`. Built from the same inputs [raytiles](https://github.com/ziv/raytiles) and
[bevytiles](https://github.com/ziv/bevytiles) stream at runtime — Mapzen Terrarium heightmaps and
Esri imagery — but with the heights baked into real geometry, so any glTF loader can show the
terrain without a custom shader.

Status: **milestones 1–2** (builder library + CLI). The HTTP server and zoom > 15 synthesis
are next; see `outline.md` / `detailed.md`.

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
#   12-772-1607.glb (556021 bytes)
```

Options for `build`:

| flag | default | meaning |
|---|---|---|
| `-o, --output <path>` | `./{zoom}-{x}-{y}.glb` | where to write |
| `--cache-dir <dir>` | `.cache` | input cache, layout-compatible with raytiles/bevytiles |
| `--resolution <n>` | `129` | vertices per edge (2..=257; 257 is lossless w.r.t. the 256-texel source) |
| `--texture-url`, `--heightmap-url` | Esri / AWS Terrarium | provider templates with `:zoom:` `:x:` `:y:` tokens |
| `--timeout <s>` | `10` | HTTP read timeout |
| `-v` / `-vv` | | log fetches and timings |

Exit codes: `0` ok · `2` usage / invalid tile / above native zoom · `3` upstream 404 · `4` network,
decode or I/O failure.

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
  native_terrain, sources{imagery, elevation}`.

## Library

```rust
use open_tiles::{build_tile, Config, TileId};

let glb = build_tile(&Config::default(), TileId::new(12, 772, 1607)?)?;
```

`Config` mirrors the engines' `NetworkConfig` (cache dir, provider URL templates, timeouts) plus
`resolution`. `load_inputs` exposes the decoded, padded height field and imagery without
building the GLB.

## Tests

```sh
cargo test        # fully offline: synthetic Terrarium/imagery fixtures + a local HTTP server
```

## Data

Imagery © Esri; elevation from the Mapzen Terrain Tiles on AWS Open Data (Terrarium encoding).
Mind their terms of use — the tiles you build embed that data.

## License

MIT OR Apache-2.0
