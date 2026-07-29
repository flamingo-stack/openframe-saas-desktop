# Build entry points for the OpenFrame desktop shell (Tauri).
#
# Mirrors clients/openframe-chat/Makefile in openframe-oss-tenant so the same CI
# shape drives both:
#   make lint
#   make build TARGET=<rust-target> OPENFRAME_VERSION=<v> [BUNDLES=app]
#
# Two differences, both forced by this shell bundling the openframe-frontend
# static export rather than building its own UI:
#   - `build` stages www/ first (chat uses tauri's beforeBuildCommand); the
#     bundle is embedded by generate_context! at compile time, so every Rust
#     target needs www/ to exist — hence the placeholder dependency on lint/test.
#   - the shared auth host is baked in at compile time (see below).

CARGO ?= cargo
NPM ?= npm

# Shared auth host baked into the binary (option_env! in src-tauri/src/lib.rs).
# Required for a shippable build: discovery, /oauth/login, /oauth/dev-exchange
# and /oauth/refresh all live there. The tenant is never configured — it is
# discovered from the user's email at login.
OPENFRAME_SHARED_HOST_URL ?=
export OPENFRAME_SHARED_HOST_URL

# Checked at parse time so a shippable build fails before the minutes-long
# frontend export, not after. Same guard as openframe-mobile's inject-env.mjs:
# without it the bundle renders and then 401s every call.
ifneq (,$(filter build,$(MAKECMDGOALS)))
ifeq ($(OPENFRAME_SHARED_HOST_URL),)
$(error OPENFRAME_SHARED_HOST_URL is required, e.g. \
  make build OPENFRAME_SHARED_HOST_URL=https://openframe.ai)
endif
endif

# App version. Unlike chat's Makefile — which accepts this and drops it — the
# value is applied, so a CI-stamped build reports it instead of tauri.conf.json's.
OPENFRAME_VERSION ?=
ifneq ($(OPENFRAME_VERSION),)
  VERSION_FLAG := --config '{"version":"$(OPENFRAME_VERSION)"}'
endif

# Optional target for cross-compilation
TARGET ?=
ifneq ($(TARGET),)
  TARGET_FLAG := --target $(TARGET)
endif

# Optional bundle subset, e.g. BUNDLES=app for an unsigned .app
BUNDLES ?=
ifneq ($(BUNDLES),)
  BUNDLES_FLAG := --bundles $(BUNDLES)
endif

.PHONY: deps web web-placeholder fmt fmt-check clippy lint build test clean

deps:
	$(NPM) ci

# Stage the openframe-oss-frontend static export into www/. Clones/refreshes
# .frontend/ by default; FRONTEND_DIR=<path> uses an existing checkout instead,
# FRONTEND_REF=<branch|tag> pins the ref.
web:
	$(NPM) run build:web

# Stub bundle so generate_context! has something to embed. Enough to compile,
# lint and test; never enough to ship. No-op when a real bundle is staged.
web-placeholder:
	$(NPM) run web:placeholder -- --if-missing

fmt:
	cd src-tauri && $(CARGO) fmt --all

fmt-check:
	cd src-tauri && $(CARGO) fmt --all -- --check

clippy: web-placeholder
	cd src-tauri && $(CARGO) clippy --all-targets -- -D warnings

lint: fmt-check clippy

build: deps web
	npx tauri build $(TARGET_FLAG) $(BUNDLES_FLAG) $(VERSION_FLAG)

test: web-placeholder
	cd src-tauri && $(CARGO) test

clean:
	cd src-tauri && $(CARGO) clean
	rm -rf www node_modules
