#!/usr/bin/env bash
# Build the openframe-oss-frontend STATIC EXPORT and stage it as the Tauri web
# bundle (www/). This repo holds no UI code — the shell embeds that export.
#
# Source of the frontend, in order:
#   FRONTEND_DIR=/path/to/checkout   use an existing working copy as-is (no git
#                                    operations — this is the local dev loop)
#   otherwise                        fresh shallow clone of FRONTEND_REPO at
#                                    FRONTEND_REF into .frontend/ (git-ignored),
#                                    re-cloned on every build. FRONTEND_REF
#                                    is required — no `main` default, so a build
#                                    never silently ships the frontend's tip. A
#                                    release passes the frontend image tag prod
#                                    runs; the frontend release workflow tags
#                                    that commit with a GitHub release.
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
FRONTEND_REF="${FRONTEND_REF:-}"
CHECKOUT="$HERE/.frontend"

if [ -n "${FRONTEND_DIR:-}" ]; then
  if [ ! -d "$FRONTEND_DIR" ]; then
    echo "✗ FRONTEND_DIR not found: $FRONTEND_DIR" >&2
    exit 1
  fi
  echo "▸ Using local frontend checkout: $FRONTEND_DIR"
else
  if [ -z "$FRONTEND_REF" ]; then
    echo "✗ FRONTEND_REF is required: the frontend image tag to release against (e.g. 1.0.100), or a branch for a dev build" >&2
    exit 1
  fi
  # Shallow, single-ref: this is a build input, not something to develop in.
  # Re-cloned every time rather than refreshed in place: fetching a tag into an
  # existing shallow checkout leaves no local tag ref, so the bundle's
  # `git describe` (its X-OpenFrame-Client version) would report a bare sha
  # instead of the release. `npm ci` below reinstalls from scratch either way.
  echo "▸ Cloning $FRONTEND_REPO ($FRONTEND_REF) → .frontend/…"
  rm -rf "$CHECKOUT"
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$FRONTEND_REF" "$FRONTEND_REPO" "$CHECKOUT"
  FRONTEND_DIR="$CHECKOUT"
  echo "▸ Frontend $FRONTEND_REF at $(git -C "$CHECKOUT" rev-parse --short HEAD)"
fi

echo "▸ Installing frontend dependencies…"
if [ -f "$FRONTEND_DIR/package-lock.json" ]; then
  ( cd "$FRONTEND_DIR" && npm ci )
else
  ( cd "$FRONTEND_DIR" && npm install )
fi

echo "▸ Building static export…"
( cd "$FRONTEND_DIR" && OPENFRAME_BUILD_TARGET="export" npm run build )

if [ ! -d "$FRONTEND_DIR/dist" ]; then
  echo "✗ export produced no dist/ in $FRONTEND_DIR" >&2
  exit 1
fi

echo "▸ Staging export bundle → www/"
rm -rf "$HERE/www"
cp -R "$FRONTEND_DIR/dist" "$HERE/www"

echo "✓ web bundle staged. Next: npm run dev (or make build)"
