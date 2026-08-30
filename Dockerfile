# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# open-tiles — on-demand 3D terrain tiles (GLB) over HTTP.
#
# Two-stage build: compile with the official Rust toolchain (pinned to the
# crate's `rust-version`), then ship only the binary on a slim Debian base.
# The binary has no native dependencies (rustls + bundled root certs, pure-Rust
# image codecs), so the runtime image needs nothing beyond glibc.
#
#   docker build -t open-tiles .
#   docker run --rm -p 8080:8080 -e AWS_REGION=eu-north-1 \
#     -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… open-tiles
#   curl -I http://127.0.0.1:8080/12/772/1607.glb
# ---------------------------------------------------------------------------

########## build stage ######################################################
FROM rust:1.95-slim-bookworm AS builder

WORKDIR /build

# Everything the compiler needs: manifests, sources, and example/index.html,
# which src/server.rs embeds with include_str! at compile time.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY example ./example

# BuildKit cache mounts keep the crate registry and the incremental target dir
# between builds, so a source-only change doesn't recompile every dependency.
# The binary is copied out because cache mounts aren't part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin open-tiles \
    && cp target/release/open-tiles /usr/local/bin/open-tiles

########## runtime stage ####################################################
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="open-tiles" \
      org.opencontainers.image.description="On-demand 3D terrain tiles (GLB) from Terrarium heightmaps and satellite imagery" \
      org.opencontainers.image.source="https://github.com/ziv/open-tiles" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# Run unprivileged. The cache — upstream inputs ({texture,heightmap}/…) and
# built tiles (glb/<fingerprint>/…) — lives in S3 by default (CACHE_DIR below,
# plus AWS_REGION and credentials / an IAM role at runtime). /data exists for
# opting into a local cache instead: CACHE_DIR=/data with a volume mounted there.
RUN groupadd --system --gid 10001 opentiles \
    && useradd --system --uid 10001 --gid opentiles --home-dir /data --shell /usr/sbin/nologin opentiles \
    && mkdir -p /data \
    && chown opentiles:opentiles /data

COPY --from=builder /usr/local/bin/open-tiles /usr/local/bin/open-tiles
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

USER opentiles
WORKDIR /data

# PORT and CACHE_DIR are read by the entrypoint (PORT is what cloud platforms
# inject). CACHE_DIR: s3://bucket[/prefix] or a directory such as /data.
# Set RUST_BACKTRACE for readable panics in logs.
ENV PORT=8080 \
    CACHE_DIR=s3://opentiles-cache/cache \
    RUST_BACKTRACE=1

EXPOSE 8080

# The server shuts down gracefully on SIGINT (tokio::signal::ctrl_c); Docker's
# default stop signal is SIGTERM, which would kill it mid-build instead.
STOPSIGNAL SIGINT

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
# Default extra args: info-level logs to stderr (what container log collectors read).
# Override at `docker run … open-tiles <args>` — e.g. `--max-builds 2 -vv`.
CMD ["-v"]
