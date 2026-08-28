#!/bin/sh
# Entrypoint for the open-tiles container.
#
# Cloud platforms (Cloud Run, Fly.io, Heroku, Railway…) tell the app which port
# to listen on via $PORT; the tile cache location is configurable via $CACHE_DIR.
# Any arguments passed to the container are appended to `open-tiles serve`, so
# provider flags (--texture-url, --max-builds, --no-cors, -v …) keep working:
#
#   docker run -p 8080:8080 open-tiles
#   docker run -p 8080:8080 open-tiles --max-builds 2 -vv
set -eu

: "${PORT:=8080}"
: "${CACHE_DIR:=/data}"

exec open-tiles serve --bind "0.0.0.0:${PORT}" --cache-dir "${CACHE_DIR}" "$@"
