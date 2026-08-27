//! `open-tiles` CLI: build one terrain tile as a GLB, or look up which tile
//! covers a coordinate.
//!
//! Exit codes: 0 ok · 2 usage / invalid tile · 3 upstream 404 · 4 network,
//! decode or I/O failure.

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use open_tiles::{build_tile, Config, Error, TileId};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "open-tiles",
    version,
    about = "3D terrain tiles as GLB, at 1:1 world scale"
)]
struct Cli {
    /// Log fetches and timings (-v info, -vv debug).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build one tile and write it as a .glb file.
    Build(BuildArgs),
    /// Print the tile covering a lat/lon at a zoom, with its bounds and size.
    Lookup(LookupArgs),
}

#[derive(Args)]
struct BuildArgs {
    /// Zoom level (1..=22; heightmaps are native up to 15).
    zoom: u8,
    /// Tile column (west → east).
    x: u32,
    /// Tile row (north → south).
    y: u32,
    /// Output path (default: ./{zoom}-{x}-{y}.glb).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Input cache root, shareable with raytiles / bevytiles.
    #[arg(long, default_value = ".cache")]
    cache_dir: PathBuf,
    /// Vertices per edge of the mesh (2..=257).
    #[arg(long, default_value_t = open_tiles::mesh::DEFAULT_RESOLUTION)]
    resolution: u32,
    /// Imagery URL template (:zoom:/:x:/:y: tokens).
    #[arg(long)]
    texture_url: Option<String>,
    /// Heightmap URL template (:zoom:/:x:/:y: tokens).
    #[arg(long)]
    heightmap_url: Option<String>,
    /// HTTP read timeout in seconds.
    #[arg(long, default_value_t = 10)]
    timeout: u64,
}

#[derive(Args)]
struct LookupArgs {
    /// Latitude in degrees.
    #[arg(allow_negative_numbers = true)]
    lat: f64,
    /// Longitude in degrees.
    #[arg(allow_negative_numbers = true)]
    lon: f64,
    /// Zoom level (1..=22).
    zoom: u8,
}

fn main() {
    let cli = Cli::parse();
    init_logger(cli.verbose);
    let code = match cli.cmd {
        Cmd::Build(a) => run_build(a),
        Cmd::Lookup(a) => run_lookup(a),
    };
    std::process::exit(code);
}

fn run_build(a: BuildArgs) -> i32 {
    let tile = match TileId::new(a.zoom, a.x, a.y) {
        Ok(t) => t,
        Err(e) => return fail(2, &e.into()),
    };
    let mut cfg = Config {
        cache_dir: a.cache_dir,
        resolution: a.resolution,
        read_timeout: Duration::from_secs(a.timeout),
        ..Config::default()
    };
    if let Some(u) = a.texture_url {
        cfg.provider.texture_url = u;
    }
    if let Some(u) = a.heightmap_url {
        cfg.provider.heightmap_url = u;
    }
    let output = a
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{}-{}-{}.glb", tile.zoom, tile.x, tile.y)));

    let glb = match build_tile(&cfg, tile) {
        Ok(b) => b,
        Err(
            e @ (Error::InvalidTile(_)
            | Error::InvalidResolution(_)
            | Error::AboveNativeZoom { .. }),
        ) => return fail(2, &e.into()),
        Err(e @ Error::NotFound { .. }) => return fail(3, &e.into()),
        Err(e) => return fail(4, &e.into()),
    };
    if let Err(e) = open_tiles::fetch::write_atomic(&output, &glb)
        .with_context(|| format!("writing {}", output.display()))
    {
        return fail(4, &e);
    }
    println!("{} ({} bytes)", output.display(), glb.len());
    0
}

fn run_lookup(a: LookupArgs) -> i32 {
    let tile = match TileId::from_lat_lon(a.lat, a.lon, a.zoom) {
        Ok(t) => t,
        Err(e) => return fail(2, &e.into()),
    };
    let b = tile.bounds();
    println!("tile:      {} {} {}", tile.zoom, tile.x, tile.y);
    println!("size_m:    {:.3}", tile.size_m());
    println!("north:     {:.6}", b.north);
    println!("south:     {:.6}", b.south);
    println!("west:      {:.6}", b.west);
    println!("east:      {:.6}", b.east);
    println!(
        "build:     open-tiles build {} {} {}",
        tile.zoom, tile.x, tile.y
    );
    0
}

fn fail(code: i32, e: &anyhow::Error) -> i32 {
    eprintln!("error: {e:#}");
    code
}

// -- tiny stderr logger (keeps the binary free of a logging framework) ------

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("{:<5} {}", record.level(), record.args());
    }
    fn flush(&self) {}
}

fn init_logger(verbosity: u8) {
    let level = match verbosity {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        _ => log::LevelFilter::Debug,
    };
    let _ = log::set_logger(&StderrLogger);
    log::set_max_level(level);
}
