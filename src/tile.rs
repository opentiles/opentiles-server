//! Tile identity and web-mercator math: validation, metre size, lat/lon
//! bounds, lat/lon → tile lookup, neighbours.
//!
//! The forward projection matches bevytiles' `WorldConfig::from_lat_lon`
//! (and raytiles' geo constructor), so a tile built here sits exactly where
//! the engines would place it. Unlike the engines — which derive one size for
//! the whole world from the anchor latitude — every tile here is scaled by
//! its **own** centre latitude (see `detailed.md` §0.1).

use crate::{Error, Result};
use std::f64::consts::PI;

/// Lowest zoom the builder accepts. The engines start at 9 for LOD reasons;
/// a tile server has no such constraint — low zooms are just big tiles.
pub const MIN_ZOOM: u8 = 1;
/// Highest zoom the builder accepts (imagery providers stop around here).
pub const MAX_ZOOM: u8 = 22;

/// WGS84 equatorial circumference in metres — the same constant the engines
/// use, so tile sizes agree to the metre.
pub const EQUATOR_CIRCUMFERENCE_M: f64 = 40_075_016.686;

/// Highest latitude web-mercator covers (the square world's edge).
pub const MAX_LATITUDE_DEG: f64 = 85.051_128_779_806_59;

/// A slippy-map tile address: `zoom/x/y`, `y` increasing southward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId {
    /// Zoom level in `[MIN_ZOOM, MAX_ZOOM]`.
    pub zoom: u8,
    /// Column in `[0, 2^zoom)`, west → east.
    pub x: u32,
    /// Row in `[0, 2^zoom)`, north → south.
    pub y: u32,
}

/// Geographic bounds of a tile, degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    /// Northern edge latitude.
    pub north: f64,
    /// Southern edge latitude.
    pub south: f64,
    /// Western edge longitude.
    pub west: f64,
    /// Eastern edge longitude.
    pub east: f64,
}

impl TileId {
    /// Validated constructor.
    pub fn new(zoom: u8, x: u32, y: u32) -> Result<Self> {
        if !(MIN_ZOOM..=MAX_ZOOM).contains(&zoom) {
            return Err(Error::InvalidTile(format!(
                "zoom {zoom} outside {MIN_ZOOM}..={MAX_ZOOM}"
            )));
        }
        let n = 1u64 << zoom;
        if u64::from(x) >= n || u64::from(y) >= n {
            return Err(Error::InvalidTile(format!(
                "x/y ({x}, {y}) outside 0..{n} at zoom {zoom}"
            )));
        }
        Ok(Self { zoom, x, y })
    }

    /// Number of tiles per axis at this zoom.
    pub fn tiles_per_axis(&self) -> u64 {
        1u64 << self.zoom
    }

    /// The tile containing `(lat, lon)` at `zoom`. Latitude is clamped to the
    /// mercator range; longitude is wrapped to `[-180, 180)`.
    pub fn from_lat_lon(lat: f64, lon: f64, zoom: u8) -> Result<Self> {
        if !(MIN_ZOOM..=MAX_ZOOM).contains(&zoom) {
            return Err(Error::InvalidTile(format!(
                "zoom {zoom} outside {MIN_ZOOM}..={MAX_ZOOM}"
            )));
        }
        let n = (1u64 << zoom) as f64;
        let lat = lat.clamp(-MAX_LATITUDE_DEG, MAX_LATITUDE_DEG);
        let lon = ((lon + 180.0).rem_euclid(360.0)) - 180.0;
        let lat_rad = lat.to_radians();
        let x = (lon + 180.0) / 360.0 * n;
        let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0 * n;
        // the clamp guards the exact-edge case (lon = 180 wraps to -180 above;
        // lat = MAX_LATITUDE lands on y = n exactly)
        let max = (n - 1.0) as u32;
        Ok(Self {
            zoom,
            x: (x.floor() as u32).min(max),
            y: (y.floor() as u32).min(max),
        })
    }

    /// Geographic bounds (degrees).
    pub fn bounds(&self) -> Bounds {
        let n = self.tiles_per_axis() as f64;
        let lon = |x: f64| x / n * 360.0 - 180.0;
        let lat = |y: f64| (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
        Bounds {
            north: lat(f64::from(self.y)),
            south: lat(f64::from(self.y) + 1.0),
            west: lon(f64::from(self.x)),
            east: lon(f64::from(self.x) + 1.0),
        }
    }

    /// Latitude of the tile's centre (degrees) — what scales the tile.
    pub fn center_lat(&self) -> f64 {
        let n = self.tiles_per_axis() as f64;
        let y = f64::from(self.y) + 0.5;
        (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees()
    }

    /// Edge length in metres at the tile's centre latitude:
    /// `circumference · cos(lat) / 2^zoom`. Same formula as the engines, but
    /// evaluated per tile instead of once per world.
    pub fn size_m(&self) -> f64 {
        EQUATOR_CIRCUMFERENCE_M * self.center_lat().to_radians().cos()
            / self.tiles_per_axis() as f64
    }

    /// The tile offset by `(dx, dy)` at the same zoom, or `None` if that
    /// leaves the grid. No antimeridian wrap in v1.
    pub fn offset(&self, dx: i32, dy: i32) -> Option<Self> {
        let n = self.tiles_per_axis() as i64;
        let x = i64::from(self.x) + i64::from(dx);
        let y = i64::from(self.y) + i64::from(dy);
        if x < 0 || y < 0 || x >= n || y >= n {
            return None;
        }
        Some(Self {
            zoom: self.zoom,
            x: x as u32,
            y: y as u32,
        })
    }

    /// The 8 neighbours in row-major order around the tile:
    /// `NW, N, NE, W, E, SW, S, SE`. `None` where the grid ends.
    pub fn neighbours(&self) -> [Option<Self>; 8] {
        [
            self.offset(-1, -1),
            self.offset(0, -1),
            self.offset(1, -1),
            self.offset(-1, 0),
            self.offset(1, 0),
            self.offset(-1, 1),
            self.offset(0, 1),
            self.offset(1, 1),
        ]
    }
}

impl std::fmt::Display for TileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.zoom, self.x, self.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_range() {
        assert!(TileId::new(0, 0, 0).is_err());
        assert!(TileId::new(23, 0, 0).is_err());
        assert!(TileId::new(1, 2, 0).is_err());
        assert!(TileId::new(1, 0, 2).is_err());
        assert!(TileId::new(1, 1, 1).is_ok());
        assert!(TileId::new(22, (1 << 22) - 1, 0).is_ok());
    }

    #[test]
    fn matches_engine_anchor_math() {
        // bevytiles README coordinate (the Dolomites), zoom 9
        let t = TileId::from_lat_lon(46.206889, 9.497194, 9).unwrap();
        assert_eq!((t.x, t.y), (269, 181));
        // Grand Canyon, zoom 12
        let g = TileId::from_lat_lon(36.1, -112.1, 12).unwrap();
        assert_eq!((g.x, g.y), (772, 1607));
        assert!((g.size_m() - 7908.657).abs() < 0.01, "{}", g.size_m());
    }

    #[test]
    fn bounds_round_trip() {
        let t = TileId::new(12, 772, 1607).unwrap();
        let b = t.bounds();
        assert!(b.north > b.south && b.east > b.west);
        // the centre must map back to the same tile
        let c =
            TileId::from_lat_lon((b.north + b.south) / 2.0, (b.west + b.east) / 2.0, 12).unwrap();
        assert_eq!(c, t);
        // and the centre latitude helper agrees with the bounds' midpoint in y
        assert!((t.center_lat() - (b.north + b.south) / 2.0).abs() < 0.01);
    }

    #[test]
    fn size_at_equator_is_circumference_over_n() {
        // zoom 1, row 1 spans 0..-85°; use zoom 4 rows straddling the equator
        let t = TileId::new(4, 8, 8).unwrap(); // just south of the equator
        let b = t.bounds();
        assert!(b.north.abs() < 1e-9);
        let expected = EQUATOR_CIRCUMFERENCE_M * t.center_lat().to_radians().cos() / 16.0;
        assert!((t.size_m() - expected).abs() < 1e-6);
        // and a tile's size shrinks with latitude
        let polar = TileId::new(4, 8, 1).unwrap();
        assert!(polar.size_m() < t.size_m());
    }

    #[test]
    fn size_agrees_with_engine_default_within_half_row() {
        // bevytiles: from_lat_lon(46.206889, 9.497194) → tile_size ≈ 54 175 m at
        // the *anchor* latitude; our per-tile size uses the centre latitude, so
        // it differs slightly but must stay within one row's cos drift (~1.2 %)
        let t = TileId::new(9, 269, 181).unwrap();
        let engine = EQUATOR_CIRCUMFERENCE_M * 46.206889f64.to_radians().cos() / 512.0;
        let rel = (t.size_m() - engine).abs() / engine;
        assert!(rel < 0.012, "rel diff {rel}");
    }

    #[test]
    fn neighbours_and_edges() {
        let t = TileId::new(2, 0, 0).unwrap();
        let n = t.neighbours();
        assert!(n[0].is_none() && n[1].is_none() && n[3].is_none());
        assert_eq!(n[4], Some(TileId::new(2, 1, 0).unwrap()));
        assert_eq!(n[7], Some(TileId::new(2, 1, 1).unwrap()));
        let m = TileId::new(2, 1, 1).unwrap();
        assert!(m.neighbours().iter().all(Option::is_some));
    }

    #[test]
    fn lookup_clamps_and_wraps() {
        let t = TileId::from_lat_lon(89.0, 180.0, 3).unwrap();
        assert_eq!((t.x, t.y), (0, 0)); // lon 180 wraps to -180, lat clamps to the top row
        let s = TileId::from_lat_lon(-89.0, 179.999, 3).unwrap();
        assert_eq!((s.x, s.y), (7, 7));
    }
}
