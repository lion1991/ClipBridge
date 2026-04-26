#!/usr/bin/env bash
# Pull the latest ClipBridge relay image and restart the container.
# Run from the deploy/ directory: `./update.sh`
#
# Idempotent: safe to run repeatedly (no-op when image is unchanged).

set -euo pipefail

cd "$(dirname "$0")"

# Pick the right compose file — falls back to docker-compose.yml.
COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.yml}"

if [[ ! -f "$COMPOSE_FILE" ]]; then
  echo "no $COMPOSE_FILE in $(pwd)" >&2
  exit 1
fi

echo "==> pulling latest image"
docker compose -f "$COMPOSE_FILE" pull

echo "==> recreating container if image changed"
docker compose -f "$COMPOSE_FILE" up -d

echo "==> tail logs (Ctrl+C to detach)"
docker compose -f "$COMPOSE_FILE" logs -f --tail 20
