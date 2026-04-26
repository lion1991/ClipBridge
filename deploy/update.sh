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

# Prefer the new V2 plugin (`docker compose`), fall back to the legacy
# standalone V1 binary (`docker-compose`).
if docker compose version >/dev/null 2>&1; then
  COMPOSE=(docker compose)
elif command -v docker-compose >/dev/null 2>&1; then
  COMPOSE=(docker-compose)
else
  cat >&2 <<'EOF'
neither `docker compose` nor `docker-compose` is available.

Install the plugin (recommended):
  Ubuntu/Debian: sudo apt-get install -y docker-compose-plugin
  RHEL/Alma/Rocky: sudo dnf install -y docker-compose-plugin
  Or reinstall Docker: curl -fsSL https://get.docker.com | sudo sh
EOF
  exit 1
fi

echo "==> using: ${COMPOSE[*]}"

echo "==> pulling latest image"
"${COMPOSE[@]}" -f "$COMPOSE_FILE" pull

echo "==> recreating container if image changed"
"${COMPOSE[@]}" -f "$COMPOSE_FILE" up -d

echo "==> tail logs (Ctrl+C to detach)"
"${COMPOSE[@]}" -f "$COMPOSE_FILE" logs -f --tail 20
