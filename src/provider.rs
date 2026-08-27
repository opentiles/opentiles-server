//! Tile providers: URL templates with `:zoom:` / `:x:` / `:y:` tokens, plus a
//! *most-provided zoom* hint per asset. Defaults are identical to raytiles /
//! bevytiles so an existing `.cache/` from either engine is usable as-is.
//!
//! There is no "native zoom" concept: a request at any zoom starts at
//! `min(zoom, hint)` and walks down on 404 to the closest zoom that exists
//! (see [`crate::fetch::Fetcher::fetch_closest`]). The hint only avoids
//! requests known to fail; a 404 below it keeps walking.

use crate::tile::TileId;

/// Where the raw inputs come from.
#[derive(Clone, Debug)]
pub struct Provider {
    /// Imagery URL template. Esri's default is `zoom/y/x` order — that swap
    /// is intentional, it is how Esri encodes its URLs.
    pub texture_url: String,
    /// Terrarium heightmap URL template (`zoom/x/y`).
    pub heightmap_url: String,
    /// Deepest zoom the imagery provider serves nearly everywhere (Esri: 19;
    /// deeper exists in cities). Start point of the fallback walk.
    pub texture_max_zoom: u8,
    /// Deepest zoom the heightmap provider serves (Mapzen Terrarium: 15).
    /// Start point of the fallback walk.
    pub heightmap_max_zoom: u8,
    /// Attribution strings recorded in the GLB `extras`.
    pub imagery_attribution: String,
    /// See `imagery_attribution`.
    pub elevation_attribution: String,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            texture_url:
                "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/:zoom:/:y:/:x:"
                    .into(),
            heightmap_url: "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/:zoom:/:x:/:y:.png".into(),
            texture_max_zoom: 19,
            heightmap_max_zoom: 15,
            imagery_attribution: "Esri World Imagery".into(),
            elevation_attribution: "Mapzen Terrain Tiles (Terrarium) on AWS Open Data".into(),
        }
    }
}

/// Which raw asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Satellite imagery (JPEG or PNG).
    Texture,
    /// Terrarium-encoded heightmap (PNG).
    Heightmap,
}

impl Kind {
    /// Cache sub-directory name — same names as the engines.
    pub fn dir(self) -> &'static str {
        match self {
            Kind::Texture => "texture",
            Kind::Heightmap => "heightmap",
        }
    }

    /// Human name for logs and errors.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Texture => "imagery",
            Kind::Heightmap => "heightmap",
        }
    }
}

impl Provider {
    /// Expanded URL for one asset of one tile.
    pub fn url(&self, kind: Kind, tile: TileId) -> String {
        let template = match kind {
            Kind::Texture => &self.texture_url,
            Kind::Heightmap => &self.heightmap_url,
        };
        expand_url(template, tile)
    }

    /// The most-provided-zoom hint for an asset.
    pub fn max_zoom(&self, kind: Kind) -> u8 {
        match kind {
            Kind::Texture => self.texture_max_zoom,
            Kind::Heightmap => self.heightmap_max_zoom,
        }
    }
}

/// Replace the first occurrence of each token. Reviewed against the engines'
/// implementation: identical semantics (`replacen(.., 1)` per token) so
/// provider strings written for raytiles/bevytiles behave the same here.
pub fn expand_url(template: &str, tile: TileId) -> String {
    template
        .replacen(":zoom:", &tile.zoom.to_string(), 1)
        .replacen(":x:", &tile.x.to_string(), 1)
        .replacen(":y:", &tile.y.to_string(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esri_swaps_y_and_x() {
        let p = Provider::default();
        let t = TileId::new(12, 772, 1607).unwrap();
        assert!(p.url(Kind::Texture, t).ends_with("/tile/12/1607/772"));
        assert!(p
            .url(Kind::Heightmap, t)
            .ends_with("/terrarium/12/772/1607.png"));
    }

    #[test]
    fn expand_replaces_once() {
        let t = TileId::new(3, 1, 2).unwrap();
        assert_eq!(expand_url("a/:zoom:/:x:/:y:/:x:", t), "a/3/1/2/:x:");
    }
}
