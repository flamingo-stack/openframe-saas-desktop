#!/usr/bin/env bash
# Build the openframe-oss-frontend STATIC EXPORT and stage it as the Tauri web
# bundle (www/). This repo holds no UI code — the shell embeds that export.
#
# Source of the frontend, in order:
#   FRONTEND_DIR=/path/to/checkout   use an existing working copy as-is (no git
#                                    operations — this is the local dev loop)
#   otherwise                        clone/refresh FRONTEND_REPO at FRONTEND_REF
#                                    into .frontend/ (git-ignored)
#
# Mirrors openframe-mobile/scripts/build-web.sh, with one difference: no
# inject-env.mjs step — the desktop shell injects window.__ENV at RUNTIME (see
# src-tauri/src/lib.rs env_init_script), so nothing is baked into the HTML here.
#
# The shell's one configured URL is the shared auth host, and it comes from the
# Rust build rather than this script:
#   make build OPENFRAME_SHARED_HOST_URL=https://auth.openframe.example
# (per-install override: "shared_host" in the app's config.json). The tenant is
# never configured — the bundle's /auth pages discover it from the user's email,
# and login learns the tenant origin from the OAuth callback.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_REPO="${FRONTEND_REPO:-https://github.com/flamingo-stack/openframe-oss-frontend}"
FRONTEND_REF="${FRONTEND_REF:-main}"
CHECKOUT="$HERE/.frontend"

if [ -n "${FRONTEND_DIR:-}" ]; then
  if [ ! -d "$FRONTEND_DIR" ]; then
    echo "✗ FRONTEND_DIR not found: $FRONTEND_DIR" >&2
    exit 1
  fi
  echo "▸ Using local frontend checkout: $FRONTEND_DIR"
else
  # Shallow, single-ref: this is a build input, not something to develop in.
  if [ -d "$CHECKOUT/.git" ]; then
    echo "▸ Refreshing $CHECKOUT ($FRONTEND_REF)…"
    git -C "$CHECKOUT" remote set-url origin "$FRONTEND_REPO"
    git -C "$CHECKOUT" fetch --depth 1 origin "$FRONTEND_REF"
  else
    echo "▸ Cloning $FRONTEND_REPO ($FRONTEND_REF) → .frontend/…"
    rm -rf "$CHECKOUT"
    git clone --depth 1 --branch "$FRONTEND_REF" "$FRONTEND_REPO" "$CHECKOUT"
    git -C "$CHECKOUT" fetch --depth 1 origin "$FRONTEND_REF"
  fi
  # Hard reset, not merge: local edits here are never intentional.
  git -C "$CHECKOUT" checkout --quiet --detach FETCH_HEAD
  git -C "$CHECKOUT" clean -qfd
  FRONTEND_DIR="$CHECKOUT"
  echo "▸ Frontend at $(git -C "$CHECKOUT" rev-parse --short HEAD)"
fi

echo "▸ Installing frontend dependencies…"
if [ -f "$FRONTEND_DIR/package-lock.json" ]; then
  ( cd "$FRONTEND_DIR" && npm ci )
else
  ( cd "$FRONTEND_DIR" && npm install )
fi

echo "▸ Building static export…"
( cd "$FRONTEND_DIR" && OPENFRAME_BUILD_TARGET=export npm run build )

if [ ! -d "$FRONTEND_DIR/dist" ]; then
  echo "✗ export produced no dist/ in $FRONTEND_DIR" >&2
  exit 1
fi

echo "▸ Staging export bundle → www/"
rm -rf "$HERE/www"
cp -R "$FRONTEND_DIR/dist" "$HERE/www"

echo "✓ web bundle staged. Next: npm run dev (or make build)"
