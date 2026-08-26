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
- `config.json` holds `shared_host` and `learned_host` — the tenant origin the
  frontend pushes back after login, used by shell-side networking — alongside the
  updater and start-at-login keys. README's Configuration table is the
  authoritative list.
- Boot always goes straight to the main window. Unauthenticated, the bundle shows
  its own sign-in screen: email → tenant discovery on the shared host → provider
  → native login. The tenant origin is the one discovery resolved — the login
  callback lands on the app's custom scheme and carries no host — and it is
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

## Start at login

`src-tauri/src/autostart.rs`. Registration is per-user — a launchd agent at
`~/Library/LaunchAgents/com.openframe.console.plist` on macOS, an `HKCU`
`…\CurrentVersion\Run` value on Windows, a `~/.config/autostart` desktop entry on
Linux — and carries `--autostart`, which is the shell's only signal that a launch
came from the OS rather than the user. That flag suppresses two things: building
the main window, and — on macOS — the Regular activation policy, which would
otherwise leave a dock icon with nothing behind it. Everything a login start
exists for (tray, token refresh, NATS, notifications) is already wired
independently of any window; the tray's **Show** builds one on demand, and so
does a notification click, which would otherwise do nothing for the life of the
process.

`tauri`'s `restart()` re-spawns with the process's own argv, so an update taken
silently at login relaunches still headless. When the user *does* have a window
open, the updater records `autostart_show_window_next_start` in config.json
before installing, and the next launch consumes it and builds one. That is a
durable marker rather than a tweak to the relaunch argv because on Windows the
relaunch is not ours to shape: `download_and_install` hands this process's argv
to the installer (`/ARGS` for NSIS, `LAUNCHAPPARGS` for MSI) and then calls
`std::process::exit(0)`, so nothing after it runs. The marker is written when the
download finishes rather than before it starts, since a download can run for
minutes and the user may open a window during it.

Written by hand rather than with `tauri-plugin-autostart`: the plugin passes
`auto-launch` an unquoted exe path, and this product's install path contains
spaces, so Explorer's `CreateProcessW` prefix search would run a planted
`%LOCALAPPDATA%\OpenFrame.exe` ahead of ours. The plugin also never sets
`AssociatedBundleIdentifiers`, without which the macOS Login Items entry is an
unattributed background item. Both backends are a few lines each over `winreg`
and `write_atomic`, which the crate already has.

- **On by default, applied once.** The first launch registers and records
  `autostart_configured`; later launches never re-apply it, so a user who turns
  it off in the OS's own UI is not fought. What later launches do repair is a
  registration that no longer matches the current binary — the `.app` was moved,
  or reinstalled elsewhere.
- **Debug builds never register at startup.** `current_binary` is
  `target/debug/…`, which the OS never registered — no tray, no notifications, no
  bundle identity. The explicit toggle still works, so the mechanism stays
  testable in dev. (`current_binary`, not `current_exe`: on Linux it resolves the
  real AppImage path instead of the ephemeral `/tmp/.mount_*` mount, which would
  never match at the next launch and so be rewritten forever.)
- **A managed install can pin it** with `autostart_enforced`, which is converged
  on at every launch and greys out the tray item. It cannot beat the OS's own
  switch, and does not try to — see Known gaps.
- **The OS's own switch is read where it can be, and never overwritten by the
  reconcile.** Windows keeps it in `StartupApproved\Run`; GNOME/XFCE/KDE keep
  theirs as `X-GNOME-Autostart-enabled=false` or `Hidden=true` *inside the entry
  the shell wrote*. So on Linux the staleness comparison is an allow-list of the
  keys the shell owns — anything the desktop added, switch or otherwise, is
  ignored rather than read as a stale file — and a genuine repair copies those
  switch lines across. Only an explicit toggle clears either.
- **Surfaces**: the tray's *Start at Login* check item, and the
  `autostart_status` / `autostart_set` commands for the frontend. Both go through
  the same state, so either moves the same check mark. As app-defined commands
  they need no capability entry.

## Security model

- **Capabilities.** Only `main.json` exists, scoped to `main` and `child-*`. The
  bundle is our own code, so it may invoke the IPC backing the NativeAuth bridge.
  No window hosts a remote origin at all: the login page — which is
  third-party-influenced — renders in the user's own browser, not in a window of
  ours, and returns only a URL on the app's scheme.
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
| macOS | `~/Library/Logs/com.openframe.console/openframe-console.log` |
| Windows | `%LOCALAPPDATA%\com.openframe.console\logs\openframe-console.log` |
| Linux | `~/.local/share/com.openframe.console/logs/openframe-console.log` |

## Known gaps

- Tokens are stored as plaintext in an owner-readable file. Keychain/Credential
  Manager storage is not implemented.
- This document has no section on the updater (`src-tauri/src/updater.rs`), which
  runs a silent startup check, a 45-minute poll, and the `update_check` /
  `update_apply_now` commands behind the UI's update modal.
- Linux has no OS-notification backend, and neither do unbundled macOS dev builds
  (the notification APIs require a bundle identifier). Windows dev builds do,
  including the action buttons — the shell registers its own AUMID and toast
  activator under `HKCU` rather than depending on the installer.
- CI signing and notarization are out of scope for this repo's build tooling; they
  run against the produced artifact.
- **A user can overrule a pinned start-at-login policy, and the shell reports
  that rather than fighting it.** Windows keeps its own switch in
  `StartupApproved\Run` (Task Manager → Startup) and macOS keeps one in a
  background-item database with no public API, so `autostart_enforced: true` can
  coexist with the app not starting. `autostart_status` returns
  `enabled: false, enforced: true` for exactly that case, and it is logged at
  startup. Re-enabling ourselves in `StartupApproved` would enforce it on Windows
  only, and is textbook malware behaviour; it is deliberately not done.
- **On macOS the reported state means *registered*, not *will run*** — the
  background-item database is unreadable, so a disable made in System Settings is
  invisible to the shell. Windows and Linux have no such gap: their switches
  (`StartupApproved\Run`, and `Hidden` / `X-GNOME-Autostart-enabled` in the entry
  itself) are both readable and are read.
- **Neither uninstaller removes the registration.** Both the Run value and the
  plist are written at runtime. On Windows the leftover is a silent no-op; on
  macOS launchd logs a failure each login. Windows could be fixed with an NSIS
  uninstall hook; macOS has no uninstaller to hook.
