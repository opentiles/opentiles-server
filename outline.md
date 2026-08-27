# OPEN-TILES — Project Outline

> Draft for approval. Scope and decisions only — no implementation details.

## 1. What it is

A tile server that turns the raw inputs raytiles/bevytiles already consume (Terrarium heightmap +
satellite imagery) into **ready-to-render 3D terrain tiles** in GLB, served over a slippy-map (XYZ)
URL, built on first request and cached forever after.

`GET /{zoom}/{x}/{y}.glb` → one self-contained terrain tile at 1:1 world scale.

## 2. Relationship to raytiles / bevytiles

Both engines run the same per-tile pipeline on background workers:
fetch assets → decode → (synthesize heightmap above z15) → build height grid → upload a *flat*
grid mesh + textures → the vertex shader lifts each vertex by the decoded height at runtime.

Open-tiles moves that pipeline **server-side** and does the lift once: every grid vertex's Y is
computed from the heightmap and stored in the mesh. The client gets a finished 3D surface and
needs only a GLB loader — no custom shader, no heightmap texture, no provider tokens.

Kept identical to the engines (so tiles line up with what raytiles/bevytiles render):

- Tile identity: web-mercator XYZ, zoom 9–22.
- 1:1 scale: tile edge = `earth circumference · cos(lat) / 2^zoom` metres (latitude-dependent).
- Coordinate frame: Y-up, metres, origin at the tile corner, **Y = 0 is sea level** (tile-local in
  X/Z; absolute in Y — the client places a tile with an X/Z translation only).
- Height source: Mapzen Terrarium (served to z15; deeper tiles derive from the closest lower zoom).
- Imagery source: Esri World Imagery by default, provider-agnostic via URL templates.
- Cache layout for raw inputs: `{texture,heightmap}/z/x/y` (the engines' layout minus normals).

## 3. Goals

1. Serve terrain tiles as GLB over `/{zoom}/{x}/{y}.glb`.
2. Build on demand; build once; serve from cache thereafter.
3. Geometry at true world scale, derived from Terrarium heights — at higher mesh resolution than
   the engines afford at runtime, since the server pays the cost once.
4. Imagery baked in as the tile's texture.
5. Provider-agnostic inputs (same URL-template model as the engines).
6. Usable by any glTF consumer: three.js, Bevy, Unity, Godot, Cesium, plain viewers.
7. A CLI that builds a single tile (`zoom x y` → `.glb`) — the builder exists and is usable
   before the server does.

## 4. Non-goals (v1)

- No client-side LOD/streaming logic — that stays in raytiles/bevytiles (or a future client lib).
- No 3D Tiles / tileset.json (possible later; see §8).
- No buildings, vegetation, roads — terrain surface only.
- No normal maps or vertex normals, no skirts, no `.gltf` (JSON) variant — GLB only.
- No authentication, rate limiting, or billing.
- No tile pre-generation / seeding tool (v1 is purely on demand; seeding is a later add-on).

## 5. Components

| Component | Responsibility |
|---|---|
| **Tile builder** | fetch inputs → decode → synthesize (z > 15) → mesh + texture → GLB (a library, used by both the CLI and the server) |
| **CLI** | `zoom x y` → `.glb` on disk; the first consumer of the builder |
| **HTTP API** | XYZ route, cache headers, health |
| **Input cache** | raw provider PNG/JPEG per asset (shared layout with the engines) |
| **Output cache** | finished `.glb` per tile; the only thing served after first build |
| **Providers** | URL templates + most-provided-zoom hint per asset (start of the fallback walk) |
| **Dedup / queue** | one in-flight build per key; concurrent requests wait on the same build |

## 6. Decisions (approved)

| Question | Decision | Why |
|---|---|---|
| **Rust or TypeScript?** | **Rust** | Terrarium decode, height synthesis, and tile math already exist and are tested in bevytiles (`synth`, `height`, `lod` math) — reuse instead of reimplement. CPU-heavy mesh/JPEG work fits Rust; single static binary is easy to deploy. |
| **Cache storage?** | **Filesystem, two tiers** (raw inputs + finished tiles), behind a small storage trait so object storage can be added later without touching the builder. | Matches the engines' cache layout; simplest to run locally and in a container; a CDN in front handles distribution. |
| **glTF or GLB?** | **GLB only.** No `.gltf` route. | One mesh + one texture per tile: a single binary file is one HTTP round-trip, no base64, universally loadable. |

Approved output shape:

- **Heights baked into vertex positions** — the server samples the heightmap per grid vertex and
  writes the height into Y; the client receives finished geometry, not a flat grid + heightmap.
- **Vertex Y is absolute: metres above sea level**, exactly as decoded from Terrarium; the tile's
  root node is identity. Y = 0 is the same sea-level plane in every tile, so placing tiles means
  setting X/Z translation only — no per-tile height offset to read or apply, and neighbouring
  edges meet in Y by construction. (Same convention as the engines: tile transforms carry y = 0
  and the shader adds the decoded height.)
- **Mesh resolution: higher than the engines' runtime grids**, chosen per zoom and configurable.
  The engines cap at 256×256 to fit a frame budget; the server builds once, so resolution is
  bounded by tile size on disk and load time, not by rendering cost. Exact per-zoom numbers to be
  decided in the detailed plan.
- **Inputs: height + imagery only.** No normal map fetched, no vertex normals emitted. (glTF
  loaders compute flat normals when none are present, so lit viewers still shade the terrain —
  faceted rather than smooth.)
- **No skirts.** Hiding LOD cracks is the client's problem.
- **Imagery embedded as JPEG** inside the GLB.
- **Tile metadata in glTF `extras`** — lat/lon bounds, tile size in metres, zoom/x/y, attribution.
- **URL path** `/{zoom}/{x}/{y}.glb` — public XYZ convention (`y`, not the engines' internal `z`).

## 7. Milestones

1. **Builder core** — fetch + input cache, Terrarium decode, tile-size math ported from bevytiles.
2. **CLI** — `open-tiles build <zoom> <x> <y>` writes a `.glb`; native-zoom tiles (z ≤ 15) with
   baked heights and embedded imagery. Verified in a stock glTF viewer before any server work.
3. **Any zoom (z 1–22)** — heightmap and imagery fall back to the closest lower zoom the
   provider serves (no "native zoom" concept); per-zoom resolution table (`detailed.md` §5).
4. **Server** — XYZ route over the same builder; output cache; dedup of concurrent builds;
   error/404 semantics; cache headers; attribution.
5. **Browser example** — a plain three.js page in `example/` (also served at `/example/`) that
   loads a predefined n × n block around a lat/lon from the server and lets you orbit it.

## 8. Future (not part of this plan)

- 3D Tiles `tileset.json` wrapper so Cesium/Google-style clients can stream open-tiles directly.
- Seeding/pre-warm CLI (equivalent of raytiles' `scripts/tiles-cache.mjs`).
- Object-storage backend and CDN cache invalidation.
- Draco / meshopt compression, KTX2 textures, smooth vertex normals or normal maps.
- Alternative height providers (e.g. Mapbox Terrain-RGB) with deeper coverage.
- A thin client crate/package that does what the engines' `store` + `lod` do, but against open-tiles.
