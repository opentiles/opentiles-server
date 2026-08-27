# OPEN-TILES

- A server that create and serve tiles as glTF files.
- Tile should be created once (cached)
- Tile should be created on demand (when requested)
- Tile should be created from Terrauium data (heightmap) and images (satellite imagery)
- Tiles should be at the same as world scale (1:1) based on the zoom level and x/y coordinates.
- Tiles should be served in slippy map format (XYZ) with a URL structure like:
  `https://open-tiles.com/{zoom}/{x}/{z}.gltf`

## Open Questions

- Rust or TypeScript for the server implementation?
- What is the best way to store and serve the cached tiles?
- Should serve GLTF or GLB files? what are the pros and cons of each format?