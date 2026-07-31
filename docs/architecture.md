# Shell architecture

How the desktop shell is put together: why it bundles the frontend, how the
bundle is staged and served, how windows behave, and what the security model is.
Auth and notifications have their own document —
[auth-and-notifications.md](./auth-and-notifications.md).

## Why a bundled export

The OpenFrame frontend is a Next.js SSR app: runtime env via `next-runtime-env`,
build-time `rewrites()`, and every backend URL resolved from
`runtimeEnv.tenantHostUrl() || window.location.origin`.

A webview wrapper can only load bundled static files or a remote URL, which for
an SSR frontend leaves three options:

| Shape | Frontend change | Verdict |
|---|---|---|
| Remote-URL shell | none | Works, but the app is a browser pointed at the tenant. Weakest posture for app-store distribution. |
| **Bundled static export** | de-SSR workstream | **Shipped.** A real native app; data goes over the API with token auth, which needs CORS for the shell origin. |
| Bundled SSR server | n/a | Rejected — a Node server cannot run on iOS, and the same shape everywhere is worth more than a desktop-only shortcut. |

The product is useless offline (auth, GraphQL, NATS and MeshCentral all live on
the gateway), so bundling buys nothing in availability. It buys app-store
viability and cold-start time.

## Bundle pipeline

`scripts/build-web.sh` (wrapped by `make web`, aliased `npm run build:web`)
clones or refreshes the frontend into git-ignored `.frontend/` — shallow, single
ref — installs, runs `OPENFRAME_BUILD_TARGET=export npm run build`, and copies
`dist/` to `www/`. `FRONTEND_DIR` points it at an existing checkout instead and
runs no git operations against it.

`www/` is a generated artifact. `tauri.conf.json` sets `frontendDist: "../www"`,
and `generate_context!` embeds the directory **at compile time** — so `www/` must
exist for `cargo check` to succeed, not just for a bundle. `npm run web:placeholder`
writes a stub for that, and the `predev` hook stages it when missing.

Unlike the mobile shell, there is no build-time env injection step: env is
injected at runtime (below), so one bundle works against any gateway.

## Runtime env injection

Every window hosting bundle content gets a Tauri initialization script
(`env_init_script`), which runs before any page script and supplies what the SSR
server would have:

- `window.__ENV` — `NEXT_PUBLIC_SHARED_HOST_URL`, `NEXT_PUBLIC_APP_MODE`, and the
  dev-ticket observer flag. This is what `next-runtime-env` reads; export builds
  omit `<PublicEnvScript />`. `NEXT_PUBLIC_TENANT_HOST_URL` is deliberately
  **absent**, so the frontend falls through to the tenant host learned at login.
- **`window.__OPENFRAME_SHELL__.nativeAuth`** — the auth bridge, backed by the
  `native_auth_*` Tauri commands. Its method names match openframe-mobile's
  `NativeAuthPlugin`, so the frontend keeps one interface with two
  implementations and desktop gets the same treatment as mobile — bearer auth,
  shell-held tokens — with no desktop-specific frontend code.

  This used to be injected as a **fake `window.Capacitor`** whose
  `isNativePlatform()` returned true, because the frontend had a single
  "is this native?" predicate that both shells had to satisfy. The frontend now
  distinguishes all three targets itself (`lib/platform.ts` — web / mobile /
  desktop, detecting us from Tauri's IPC globals), so nothing here impersonates
  Capacitor, and phone-only features (FCM push, biometrics, safe-area insets,
  Android back) can no longer switch on in a desktop window. Renaming or dropping
  this namespace kills desktop sign-in: keep it in sync with `nativeAuthPlugin()`
  in the frontend.

## Hosts

The shell knows exactly one configured URL, the **shared auth host**. There is no
tenant host to configure anywhere.

- Baked at build time from `OPENFRAME_SHARED_HOST_URL` (`option_env!`; `build.rs`
  declares `rerun-if-env-changed` so a changed host actually invalidates the
  cached build). Overridable per install by `shared_host` in `config.json`.
- `config.json` holds only `shared_host` and `learned_host` — the tenant origin
  the frontend pushes back after login, used by shell-side networking.
- Boot always goes straight to the main window. Unauthenticated, the bundle shows
  its own sign-in screen: email → tenant discovery on the shared host → provider
  → native login. The tenant origin is learned from the OAuth callback and
  persisted both in the webview and shell-side.
- Tray **Sign Out** clears tokens and the learned host and recreates the main
  window at the sign-in screen.

Consequence: self-hosted single-tenant instances, which have no shared auth host
to discover against, are out of scope for this shell.

## Windows

- **Routing.** Tauri's asset resolver falls back `path` → `path/index.html` →
  `index.html`, so the export's trailing-slash routes, cold deep links to
  query-param pages, and unknown paths (SPA fallback) all resolve.
- **New windows** (`window.open`, `target="_blank"`). Bundle-origin targets open
  as in-app child windows — cascaded, with the same init script injected.
  Matching is done against a live window's origin rather than a hardcoded one, so
  it is correct across platforms and in dev. External http(s) opens in the system
  browser; that now includes tenant-origin links, since app content no longer
  lives there. Other schemes are blocked.
- **No white flash.** Windows are created hidden with a dark background colour and
  revealed on `PageLoadEvent::Finished`, with a timer as a safety net.
  `background_color` alone is not enough on macOS, where it is not applied to the
  webview layer.
- **Close to tray**, single-instance, and macOS dock/activation-policy handling.
- **Replacement uses `destroy()`, not `close()`** — `close()` emits
  `CloseRequested`, which the close-to-tray handler turns into a hide. Because
  `destroy()` is processed by the event loop after the calling command returns,
  recreating a window under the same label is done from a spawned task; doing it
  inline would either collide on the label or deadlock the event loop.

## Security model

- **Capabilities.** Only `main.json` exists, scoped to `main` and `child-*`. The
  bundle is our own code, so it may invoke the IPC backing the NativeAuth bridge.
  The remote `native-auth` login window is in **no** capability, so the login page
  — which is third-party-influenced — cannot reach Tauri commands.
- **Gateway CORS is a prerequisite.** The gateway must allow `tauri://localhost`
  (macOS/Linux) and `http://tauri.localhost` (Windows), and expose the
  `Access-Token`/`Refresh-Token` headers. Without it the bundle renders and every
  data call fails.
- **Dev shares the production origin.** `npm run dev` passes `--no-dev-server`, so
  dev serves the bundle from disk over the same `tauri://localhost` protocol
  rather than the CLI's static server on `http://127.0.0.1`, whose origin would
  need its own CORS entry.

## Debugging

Release builds ship without devtools. `env_init_script` forwards the webview's
`console.error`, uncaught errors and unhandled rejections into the shell log, and
logs a boot marker with the injected `window.__ENV` and the page origin.

| OS | Log |
|---|---|
| macOS | `~/Library/Logs/com.openframe.desktop/openframe-desktop.log` |
| Windows | `%LOCALAPPDATA%\com.openframe.desktop\logs\openframe-desktop.log` |
| Linux | `~/.local/share/com.openframe.desktop/logs/openframe-desktop.log` |

## Known gaps

- Tokens are stored as plaintext in an owner-readable file. Keychain/Credential
  Manager storage is not implemented.
- No auto-updater.
- Linux has no OS-notification backend, and neither do unbundled macOS dev builds
  (the notification APIs require a bundle identifier).
- CI signing and notarization are out of scope for this repo's build tooling; they
  run against the produced artifact.
