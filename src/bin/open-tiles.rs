//! `open-tiles` CLI: build one terrain tile as a GLB, serve tiles over HTTP,
//! look up which tile covers a coordinate, generate missing per-tile
//! metadata, or clear negative-cache markers.
//!
//! Exit codes: 0 ok · 2 usage / invalid tile · 3 nothing upstream at any
//! zoom · 4 network, decode or I/O failure.

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use open_tiles::fetch::Fetcher;
use open_tiles::provider::Kind;
use open_tiles::server::ServeConfig;
use open_tiles::store::Store;
use open_tiles::{build_tile, Config, Error, TileId};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "open-tiles",
    version,
    about = "3D terrain tiles as GLB, at 1:1 world scale"
)]
/// Top-level parser: the global verbosity flag plus one subcommand.
struct Cli {
    /// Log fetches and timings (-v info, -vv debug).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// The operation to run.
    #[command(subcommand)]
    cmd: Cmd,
}

/// The five operations; each maps onto one `run_*` function below.
#[derive(Subcommand)]
enum Cmd {
    /// Build one tile and write it as a .glb file.
    Build(BuildArgs),
    /// Print the tile covering a lat/lon at a zoom, with its bounds and size.
    Lookup(LookupArgs),
    /// Serve tiles over HTTP: GET /{z}/{x}/{y}.glb, built on demand and cached.
    Serve(ServeArgs),
    /// Write the missing metadata JSONs next to the built tiles in the cache.
    Metadata(MetadataArgs),
    /// Forget cached 404s so the providers are asked again.
    #[command(name = "refresh-404")]
    Refresh404(RefreshArgs),
}

/// Arguments of `open-tiles build`.
#[derive(Args)]
struct BuildArgs {
    /// Zoom level (1..=22).
    zoom: u8,
    /// Tile column (west → east).
    x: u32,
    /// Tile row (north → south).
    y: u32,
    /// Output path (default: ./{zoom}-{x}-{y}.glb).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Input cache: a directory (shareable with raytiles / bevytiles) or
    /// s3://bucket[/prefix] (AWS_REGION, credentials and AWS_ENDPOINT_URL
    /// from the environment).
    #[arg(long, default_value = ".cache", env = "CACHE_DIR")]
    cache_dir: String,
    /// Vertices per edge for this zoom (2..=257); default comes from the
    /// per-zoom table and is capped by the height source's useful ceiling.
    #[arg(long)]
    resolution: Option<u32>,
    /// Shared provider flags.
    #[command(flatten)]
    provider: ProviderArgs,
}

/// Provider flags shared by `build` and `serve`: URL templates, zoom
/// hints, and the upstream read timeout. Unset flags keep the defaults
/// from [`Provider::default`](open_tiles::Provider).
#[derive(Args)]
struct ProviderArgs {
    /// Imagery URL template (:zoom:/:x:/:y: tokens).
    #[arg(long)]
    texture_url: Option<String>,
    /// Heightmap URL template (:zoom:/:x:/:y: tokens).
    #[arg(long)]
    heightmap_url: Option<String>,
    /// Normal-map URL template (:zoom:/:x:/:y: tokens).
    #[arg(long)]
    normals_url: Option<String>,
    /// Deepest zoom to ask the imagery provider for before deriving (default 19).
    #[arg(long)]
    texture_max_zoom: Option<u8>,
    /// Deepest zoom to ask the heightmap provider for before deriving (default 15).
    #[arg(long)]
    heightmap_max_zoom: Option<u8>,
    /// Deepest zoom to ask the normals provider for before deriving (default 15).
    #[arg(long)]
    normals_max_zoom: Option<u8>,
    /// Skip normals entirely: no provider fetch, no NORMAL attribute.
    #[arg(long)]
    no_normals: bool,
    /// HTTP read timeout in seconds for upstream fetches.
    #[arg(long, default_value_t = 10)]
    timeout: u64,
}

impl ProviderArgs {
    /// Copy every flag that was actually given onto `cfg`; absent flags
    /// leave the defaults untouched.
    fn apply(self, cfg: &mut Config) {
        cfg.read_timeout = Duration::from_secs(self.timeout);
        if let Some(u) = self.texture_url {
            cfg.provider.texture_url = u;
        }
        if let Some(u) = self.heightmap_url {
            cfg.provider.heightmap_url = u;
        }
        if let Some(u) = self.normals_url {
            cfg.provider.normals_url = u;
        }
        if let Some(z) = self.texture_max_zoom {
            cfg.provider.texture_max_zoom = z;
        }
        if let Some(z) = self.heightmap_max_zoom {
            cfg.provider.heightmap_max_zoom = z;
        }
        if let Some(z) = self.normals_max_zoom {
            cfg.provider.normals_max_zoom = z;
        }
        cfg.include_normals = !self.no_normals;
    }
}

/// Arguments of `open-tiles serve`.
#[derive(Args)]
struct ServeArgs {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: std::net::SocketAddr,
    /// Input + output cache: a directory or s3://bucket[/prefix].
    #[arg(long, default_value = ".cache", env = "CACHE_DIR")]
    cache_dir: String,
    /// Concurrent tile builds (default: CPU count).
    #[arg(long)]
    max_builds: Option<usize>,
    /// Do not send Access-Control-Allow-Origin: *.
    #[arg(long)]
    no_cors: bool,
    /// Shared provider flags.
    #[command(flatten)]
    provider: ProviderArgs,
}

/// Arguments of `open-tiles lookup`.
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

/// Arguments of `open-tiles metadata`.
#[derive(Args)]
struct MetadataArgs {
    /// Cache holding the built tiles (glb/{fingerprint}/z/x/y.glb) and the
    /// heightmaps: a directory or s3://bucket[/prefix].
    #[arg(long, default_value = ".cache", env = "CACHE_DIR")]
    cache_dir: String,
    /// Shared provider flags (heightmaps missing from the cache are fetched).
    #[command(flatten)]
    provider: ProviderArgs,
}

/// `--kind` values for `refresh-404`, mirroring [`Kind`].
#[derive(Clone, Copy, ValueEnum)]
enum AssetKind {
    /// Imagery markers only.
    Texture,
    /// Heightmap markers only.
    Heightmap,
    /// Normal-map markers only.
    Normals,
}

/// Arguments of `open-tiles refresh-404`.
#[derive(Args)]
struct RefreshArgs {
    /// Cache holding the markers: a directory or s3://bucket[/prefix].
    #[arg(long, default_value = ".cache", env = "CACHE_DIR")]
    cache_dir: String,
    /// Only this zoom (default: all).
    #[arg(long)]
    zoom: Option<u8>,
    /// Only this asset kind (default: both).
    #[arg(long, value_enum)]
    kind: Option<AssetKind>,
}

fn main() {
    let cli = Cli::parse();
    init_logger(cli.verbose);
    let code = match cli.cmd {
        Cmd::Build(a) => run_build(a),
        Cmd::Lookup(a) => run_lookup(a),
        Cmd::Serve(a) => run_serve(a),
        Cmd::Metadata(a) => run_metadata(a),
        Cmd::Refresh404(a) => run_refresh(a),
    };
    std::process::exit(code);
}

/// `build`: build one tile, write the GLB to `--output` (default
/// `./{zoom}-{x}-{y}.glb`) with its metadata JSON next to it, and map
/// error kinds onto the documented exit codes (2 usage · 3 not found ·
/// 4 network/decode/IO).
fn run_build(a: BuildArgs) -> i32 {
    let tile = match TileId::new(a.zoom, a.x, a.y) {
        Ok(t) => t,
        Err(e) => return fail(2, &e.into()),
    };
    let cache = match open_cache(&a.cache_dir) {
        Ok(c) => c,
        Err(e) => return fail(4, &e),
    };
    let mut cfg = Config {
        cache,
        ..Config::default()
    };
    a.provider.apply(&mut cfg);
    if let Some(r) = a.resolution {
        cfg.set_resolution(tile.zoom, r);
    }
    let output = a
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{}-{}-{}.glb", tile.zoom, tile.x, tile.y)));

    let glb = match build_tile(&cfg, tile) {
        Ok(b) => b,
        Err(e @ (Error::InvalidTile(_) | Error::InvalidResolution(_))) => {
            return fail(2, &e.into())
        }
        Err(e @ Error::NotFound { .. }) => return fail(3, &e.into()),
        Err(e) => return fail(4, &e.into()),
    };
    if let Err(e) = open_tiles::fetch::write_atomic(&output, &glb)
        .with_context(|| format!("writing {}", output.display()))
    {
        return fail(4, &e);
    }
    println!("{} ({} bytes)", output.display(), glb.len());
    // the metadata document rides along with every build; a failure leaves
    // only the .glb (the build itself succeeded)
    let json = output.with_extension("json");
    match open_tiles::metadata::compute(&cfg, tile) {
        Ok(meta) => {
            if let Err(e) = open_tiles::fetch::write_atomic(&json, &meta.to_json_bytes())
                .with_context(|| format!("writing {}", json.display()))
            {
                return fail(4, &e);
            }
            println!("{}", json.display());
        }
        Err(e) => log::warn!("{tile}: metadata failed: {e}"),
    }
    0
}

/// `lookup`: print which tile covers a lat/lon at a zoom — address,
/// metre size, bounds, and a ready-to-paste `build` command.
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

/// `serve`: open the cache (directory or S3), assemble the configs, and
/// run the HTTP server on a fresh tokio runtime until Ctrl-C.
fn run_serve(a: ServeArgs) -> i32 {
    let cache = match open_cache(&a.cache_dir) {
        Ok(c) => c,
        Err(e) => return fail(4, &e),
    };
    let mut cfg = Config {
        cache,
        ..Config::default()
    };
    a.provider.apply(&mut cfg);
    let mut serve = ServeConfig {
        bind: a.bind,
        cors: !a.no_cors,
        ..ServeConfig::default()
    };
    if let Some(n) = a.max_builds {
        serve.max_builds = n.max(1);
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return fail(4, &anyhow::Error::from(e).context("starting the runtime")),
    };
    match rt.block_on(open_tiles::server::run(cfg, serve)) {
        Ok(()) => 0,
        Err(e) => fail(4, &anyhow::Error::from(e).context("serving")),
    }
}

/// `metadata`: write the missing per-tile metadata JSONs next to every
/// built tile in the cache (exit 4 when any tile failed — the rest are
/// still written).
fn run_metadata(a: MetadataArgs) -> i32 {
    let cache = match open_cache(&a.cache_dir) {
        Ok(c) => c,
        Err(e) => return fail(4, &e),
    };
    let mut cfg = Config {
        cache,
        ..Config::default()
    };
    a.provider.apply(&mut cfg);
    match open_tiles::metadata::generate_missing(&cfg) {
        Ok(s) => {
            println!(
                "metadata: {} written, {} skipped, {} failed",
                s.written, s.skipped, s.failed
            );
            if s.failed > 0 {
                4
            } else {
                0
            }
        }
        Err(e) => fail(4, &e.into()),
    }
}

/// `refresh-404`: delete negative-cache markers so the providers are
/// asked again (e.g. after their coverage improved).
fn run_refresh(a: RefreshArgs) -> i32 {
    let cache = match open_cache(&a.cache_dir) {
        Ok(c) => c,
        Err(e) => return fail(4, &e),
    };
    let f = Fetcher::new(cache, Duration::from_secs(1), Duration::from_secs(1));
    let kind = a.kind.map(|k| match k {
        AssetKind::Texture => Kind::Texture,
        AssetKind::Heightmap => Kind::Heightmap,
        AssetKind::Normals => Kind::Normals,
    });
    match f.clear_missing_markers(kind, a.zoom) {
        Ok(n) => {
            println!("removed {n} markers");
            0
        }
        Err(e) => fail(4, &e.into()),
    }
}

/// `--cache-dir`: a directory or an `s3://` URL.
fn open_cache(spec: &str) -> anyhow::Result<Arc<dyn Store>> {
    open_tiles::store::open(spec).with_context(|| format!("opening cache {spec}"))
}

/// The single error path of every subcommand: print `error: …` with the
/// cause chain to stderr, return the exit code for `main` to pass on.
fn fail(code: i32, e: &anyhow::Error) -> i32 {
    eprintln!("error: {e:#}");
    code
}

// -- tiny stderr logger (keeps the binary free of a logging framework) ------

/// Minimal `log` sink: one `LEVEL message` line per record on stderr.
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

/// Map `-v` count to a level (0 warn · 1 info · 2+ debug) and install
/// [`StderrLogger`].
fn init_logger(verbosity: u8) {
    let level = match verbosity {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        _ => log::LevelFilter::Debug,
    };
    let _ = log::set_logger(&StderrLogger);
    log::set_max_level(level);
}
