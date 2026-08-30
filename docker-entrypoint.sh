#!/bin/sh
# Entrypoint for the open-tiles container.
#
# Cloud platforms (Cloud Run, Fly.io, ECS, Railway…) tell the app which port to
# listen on via $PORT. The cache defaults to S3 so the container is stateless
# and every replica shares one cache: $CACHE_DIR is s3://bucket[/prefix]
# (credentials and region from AWS_REGION / AWS_ACCESS_KEY_ID /
# AWS_SECRET_ACCESS_KEY or the platform's IAM role). Set CACHE_DIR to a
# directory (e.g. /data, with a volume mounted there) to cache locally instead.
# Any arguments passed to the container are appended to `open-tiles serve`, so
# provider flags (--texture-url, --max-builds, --no-cors, -v …) keep working:
#
#   docker run -p 8080:8080 -e AWS_REGION=eu-north-1 -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… open-tiles
#   docker run -p 8080:8080 -e CACHE_DIR=s3://other-bucket/prefix -e AWS_REGION=… open-tiles
#   docker run -p 8080:8080 -e CACHE_DIR=/data -v open-tiles-cache:/data open-tiles --max-builds 2 -vv
set -eu

: "${PORT:=8080}"
: "${CACHE_DIR:=s3://opentiles-cache/cache}"

exec open-tiles serve --bind "0.0.0.0:${PORT}" --cache-dir "${CACHE_DIR}" "$@"
