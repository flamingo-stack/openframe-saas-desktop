#!/usr/bin/env bash
# Build the openframe-frontend STATIC EXPORT and stage it as the Tauri web
# bundle (www/). Mirrors openframe-mobile/scripts/build-web.sh, with one
# difference: the desktop shell injects window.__ENV at RUNTIME (Tauri
# initialization script built from the host picked in the connect window —
# see src-tauri/src/lib.rs env_init_script), so nothing is baked into the HTML
# here. Optional SaaS keys (shared host, app mode) live in the app's
# config.json, not in the bundle.
#
# Override the frontend checkout with FRONTEND_DIR=~/code/openframe-frontend.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_DIR="${FRONTEND_DIR:-$HOME/flamingo/openframe-frontend}"

if [ ! -d "$FRONTEND_DIR" ]; then
  echo "✗ FRONTEND_DIR not found: $FRONTEND_DIR" >&2
  echo "  Set FRONTEND_DIR to your openframe-frontend checkout." >&2
  exit 1
fi

echo "▸ Building openframe-frontend static export ($FRONTEND_DIR)…"
( cd "$FRONTEND_DIR" && OPENFRAME_BUILD_TARGET=export npm run build )

echo "▸ Staging export bundle → www/"
rm -rf "$HERE/www"
cp -R "$FRONTEND_DIR/dist" "$HERE/www"

echo "▸ Staging connect (host picker) page → www/connect.html"
cp "$HERE/connect/index.html" "$HERE/www/connect.html"

echo "✓ web bundle staged. Next: npm run dev (or npm run build)"
